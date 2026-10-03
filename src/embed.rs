//! Using a table outside IQ Tables.
//!
//! * Snapshot: the data itself, as CSV, JSON or an HTML table, made here
//!   from what the explorer read. Nothing to run; it never changes.
//! * Live: a developer's server (or web page) reads the table each time with
//!   the IQ Tables decoder (`decoder/`, deployed as `iqt-decoder.wasm` in this
//!   site's own IQ git repository). They keep one small file,
//!   `iqt-loader.mjs` (`embed/`), which finds the newest decoder there (or a
//!   pinned one) and runs it sealed off: no imports, and requests only to the
//!   gateway and RPC they configured. `iqt-formats.json` says which decoder
//!   reads each storage format, so data in an old format stays readable
//!   after newer decoders drop it.

use crate::app::{fetch_err, App, Load, Mode, Route, TableView, P};
use crate::crypto::{base58, base64_decode};
use crate::git;
use crate::host;
use crate::records::{self, Who};
use crate::solana;
use crate::ui::{self, esc};

pub const LOADER: &str = include_str!("../embed/iqt-loader.mjs");
pub const LOADER_FILE: &str = "iqt-loader.mjs";
pub const DECODER_FILE: &str = "iqt-decoder.wasm";
/// On-chain rows the explorer reads at most (the same cap as "load all").
const SNAPSHOT_MAX: usize = 20_000;
/// Characters of a snapshot shown in the dialog.
const PREVIEW: usize = 12_000;
/// Rows per page, and the shortest time between reads, in an iframe.
const FRAME_PAGE: usize = 50;
const MIN_EVERY: u32 = 60;

/// `#/embed/[<database>/]<table>?rows=…&every=…`
pub fn route(root: Option<&str>, pda: &str, query: &str) -> Route {
    let mut who = Who::Official;
    let mut every = 0;
    for kv in query.split('&') {
        match kv.split_once('=') {
            Some(("rows", v)) => who = Who::parse(v).unwrap_or(Who::Official),
            Some(("every", v)) => every = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    Route::Embed { root: root.map(String::from), pda: pda.to_string(), who, every }
}

/// The address an iframe opens: this site, at the embed view of the table.
pub fn frame_url(app: &App, tv: &TableView, who: Who, every: u32) -> String {
    let site = if app.page_url.is_empty() { "https://browser.iqlabs.dev/".to_string() } else { app.page_url.clone() };
    let at = match &tv.root {
        Some(r) => format!("{}/{}", r, tv.pda),
        None => tv.pda.clone(),
    };
    let mut url = format!("{}#/embed/{}?rows={}", site, at, who.as_str());
    if every > 0 {
        url.push_str(&format!("&every={}", every));
    }
    url
}

fn iframe_code(app: &App, tv: &TableView, who: Who) -> String {
    let every = app.embed.as_ref().map(|e| e.every).unwrap_or(0);
    format!(
        "<iframe src=\"{}\" title=\"{}\" width=\"100%\" height=\"520\" style=\"border:0\" loading=\"lazy\"></iframe>",
        esc(&frame_url(app, tv, who, every)),
        esc(&name_of(tv))
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Snapshot,
    Live,
    Frame,
}

#[derive(Default)]
pub struct Embed {
    pub tab: Tab,
    /// Whose rows (None: official when the official wallet is known).
    pub who: Option<Who>,
    /// Snapshot format: csv, json or html.
    pub format: String,
    /// Live output: csv or json.
    pub live: String,
    pub pin: bool,
    /// An IQ browser link to the repository holding the decoder, when this
    /// page can't tell (it isn't running from IQ's browser).
    pub repo: String,
    /// Iframe: seconds between reads (0: once).
    pub every: u32,
}

fn is_pk(s: &str) -> bool {
    base58::decode(s).map(|b| b.len()) == Some(32)
}

/// The address in `https://browser.iqlabs.dev/<address>[/…]`.
pub fn browser_address(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("https://").or_else(|| url.trim().strip_prefix("http://"))?;
    let path = rest.strip_prefix("browser.iqlabs.dev/")?;
    let first = path.split(['/', '?', '#']).next()?;
    is_pk(first).then(|| first.to_string())
}

/// The `.sol` name this page is served under: `<name>.sol.site`, or
/// `browser.iqlabs.dev/<name>.sol`.
pub fn sol_name(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("https://").or_else(|| url.trim().strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    let name = match host.strip_suffix(".sol.site") {
        Some(n) => format!("{}.sol", n),
        None if host == "browser.iqlabs.dev" => path.split(['/', '?', '#']).next()?.to_ascii_lowercase(),
        None => return None,
    };
    let ok = name.len() > 4 && name.ends_with(".sol") && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.' || c == b'_');
    ok.then_some(name)
}

/// The repository a page served under a `.sol` name comes from: the name's
/// SOL record (looked up once through IQ's gateway). None when there's no
/// name, or it doesn't point at an address; Err while it's being looked up.
fn sol_repo(app: &App) -> Option<Result<String, &'static str>> {
    if app.embed.as_ref().map(|e| !e.repo.trim().is_empty()).unwrap_or(false) {
        return None;
    }
    let name = sol_name(&app.page_url)?;
    match app.git.sns.get(&name) {
        Some(Load::Ready(Some(pda))) => Some(Ok(pda.clone())),
        Some(Load::Ready(None)) | Some(Load::Err(_)) => None,
        _ => {
            app.git.wanted.borrow_mut().push(format!("sns:{}", name));
            Some(Err("Looking up this site's .sol name…"))
        }
    }
}

/// The IQ git repository holding the decoder: a link pasted in the dialog,
/// or the repository this page is served from by IQ's browser.
pub fn decoder_repo(app: &App) -> Option<String> {
    let typed = app.embed.as_ref().map(|e| e.repo.trim().to_string()).filter(|s| !s.is_empty());
    match typed {
        Some(t) => browser_address(&t).or_else(|| is_pk(&t).then_some(t)),
        None => browser_address(&app.page_url),
    }
}

fn who_of(app: &App) -> Who {
    let known = app.table.as_ref().map(|t| t.creator.is_some()).unwrap_or(false);
    match app.embed.as_ref().and_then(|e| e.who) {
        Some(w) if known || w == Who::All => w,
        _ if known => Who::Official,
        _ => Who::All,
    }
}

/// Everything a snapshot of `who`'s rows needs has been read.
pub fn ready(tv: &TableView, who: Who) -> bool {
    if tv.loading || tv.chunk_waits > 0 || matches!(tv.meta, Load::Loading) {
        return false;
    }
    // the owner's checkpoint rewrote every official record, so official rows
    // are complete there; other writers' rows may be older than it
    if who == Who::Official {
        tv.done || tv.cut
    } else {
        tv.done && (tv.full || !tv.cut)
    }
}

fn name_of(tv: &TableView) -> String {
    tv.meta
        .ready()
        .and_then(|m| m.get("name").str().map(String::from))
        .filter(|n| !n.is_empty())
        .or_else(|| tv.label.clone())
        .unwrap_or_else(|| solana::short(&tv.pda))
}

/// The snapshot text and its row count.
pub fn snapshot(tv: &TableView, who: Who, fmt: &str) -> (String, usize) {
    let src = crate::views::source(tv);
    let (cols, rows) = src.table(who, true);
    let text = match fmt {
        "json" => records::json_rows(&cols, &rows).to_string(),
        "html" => {
            let when = src.as_of().and_then(|(_, t)| t).map(|t| format!(" as of {}", ui::time(t))).unwrap_or_default();
            records::html(&cols, &rows, &format!("IQ table {} · {} rows{} · a snapshot from IQ Tables", tv.pda, who.as_str(), when))
        }
        _ => records::csv(&cols, &rows),
    };
    (text, rows.len())
}

/// Where the live decoder stands.
enum Found {
    /// No repository known yet.
    NoRepo,
    Reading(&'static str),
    Bad(String),
    Ready {
        repo: String,
        name: String,
        owner: String,
        ago: String,
        decoder: Option<String>,
    },
}

fn resolve(app: &App) -> Found {
    if app.use_rpc() {
        return Found::Bad("Live embeds find the decoder through IQ's gateway: set Settings → Read tables from → IQ gateway.".into());
    }
    let pda = match decoder_repo(app).map(Ok).or_else(|| sol_repo(app)) {
        Some(Ok(pda)) => pda,
        Some(Err(reading)) => return Found::Reading(reading),
        None => return Found::NoRepo,
    };
    app.git.wanted.borrow_mut().push(pda.clone());
    let repo = match app.git.repos.get(&pda) {
        Some(Load::Ready(Some(r))) => r,
        Some(Load::Ready(None)) => return Found::Bad("That address isn't an IQ git repository.".into()),
        Some(Load::Err(e)) => return Found::Bad(e.clone()),
        _ => return Found::Reading("Looking up the decoder's repository…"),
    };
    let c = match &repo.commits {
        Load::Ready(cs) => match cs.first() {
            Some(c) => c,
            None => return Found::Bad(format!("{} has no commits yet.", repo.name)),
        },
        Load::Err(e) => return Found::Bad(e.clone()),
        _ => return Found::Reading("Reading the repository's commits…"),
    };
    app.git.wanted.borrow_mut().push(format!("tree:{}", c.tree));
    match app.git.trees.get(&c.tree) {
        Some(Load::Ready(files)) => Found::Ready {
            repo: pda,
            name: repo.name.clone(),
            owner: repo.owner.clone(),
            ago: git::ago(host::now_ms(), c.time_ms),
            decoder: files.iter().find(|(p, _)| p == DECODER_FILE).map(|(_, t)| t.clone()),
        },
        Some(Load::Err(e)) => Found::Bad(e.clone()),
        _ => Found::Reading("Reading the newest commit…"),
    }
}

struct Snip<'a> {
    table: &'a str,
    /// Not IQ's main gateway (devnet, or one set in Settings).
    gateway: Option<String>,
    official: Option<&'a str>,
    who: Who,
    repo: &'a str,
    owner: &'a str,
    pin: Option<&'a str>,
}

impl Snip<'_> {
    fn options(&self, pad: &str) -> String {
        let mut o = format!("{p}table: \"{}\",\n", self.table, p = pad);
        if let Some(w) = self.official {
            o.push_str(&format!("{p}official: \"{}\", // only this wallet's rows count\n", w, p = pad));
        }
        o.push_str(&format!("{p}rows: \"{}\",\n", self.who.as_str(), p = pad));
        if let Some(g) = &self.gateway {
            o.push_str(&format!("{p}gateway: \"{}\",\n", g, p = pad));
        }
        o
    }
    fn decoder(&self, pad: &str) -> String {
        match self.pin {
            Some(p) => {
                format!("{pad}decoder: {{ repo: \"{}\", owner: \"{}\", pin: \"{}\" }}, // this decoder version, forever\n", self.repo, self.owner, p, pad = pad)
            }
            None => format!("{pad}decoder: {{ repo: \"{}\", owner: \"{}\" }}, // the newest decoder on IQ git\n", self.repo, self.owner, pad = pad),
        }
    }
    fn server(&self, fmt: &str) -> String {
        format!(
            "// Node 18+, Deno or Bun: no packages to install. {file} is the file from the button above.\nimport {{ readTable }} from \"./{file}\";\n\nconst table = await readTable({{\n{opts}  format: \"{fmt}\",\n{dec}}});\n// table.data is the {upper}; table.rows / table.cols hold the values.\n// Serve it at a URL of yours (e.g. /table.{fmt}) for your site, Excel or Google Sheets.\n",
            file = LOADER_FILE,
            opts = self.options("  "),
            fmt = fmt,
            dec = self.decoder("  "),
            upper = fmt.to_uppercase()
        )
    }
    fn web(&self) -> String {
        format!(
            "<div id=\"iq-table\"></div>\n<script type=\"module\">\n  // {file} sits next to this page on your site\n  import {{ renderTable }} from \"./{file}\";\n  renderTable(document.getElementById(\"iq-table\"), {{\n{opts}{dec}    every: 300000, // read it again every 5 minutes\n  }});\n</script>\n",
            file = LOADER_FILE,
            opts = self.options("    "),
            dec = self.decoder("    ")
        )
    }
}

fn snip_for<'a>(app: &'a App, tv: &'a TableView, f: &'a Found) -> Option<Snip<'a>> {
    let Found::Ready { repo, owner, decoder, .. } = f else { return None };
    // a live read has to know whose rows are the owner's
    tv.creator.as_ref()?;
    let pin = app.embed.as_ref().map(|e| e.pin).unwrap_or(false);
    let gw = app.gateway_url();
    Some(Snip {
        table: &tv.pda,
        gateway: (gw != crate::app::MAIN_GATEWAY).then_some(gw),
        official: tv.creator.as_deref(),
        who: who_of(app),
        repo,
        owner,
        pin: if pin { decoder.as_deref() } else { None },
    })
}

impl App {
    /// Read what a snapshot needs: every row, or past the owner's
    /// checkpoint when other writers' rows are wanted.
    fn embed_load(&mut self) {
        if self.embed.as_ref().map(|e| e.tab != Tab::Snapshot).unwrap_or(true) {
            return;
        }
        let who = who_of(self);
        let Some(t) = self.table.as_mut() else { return };
        if ready(t, who) {
            return;
        }
        if who != Who::Official && t.cut && !t.full {
            t.full = true;
            t.cut = false;
            if t.cursor.is_some() {
                t.done = false;
            }
        }
        if t.done {
            return;
        }
        t.load_all = true;
        if !t.loading {
            self.more_rows();
        }
    }

    pub fn embed_event(&mut self, action: &str, arg: &str, val: &str) -> bool {
        match action {
            "embed-open" => {
                self.embed = Some(Embed { format: "csv".into(), live: "csv".into(), ..Default::default() });
                self.embed_load();
            }
            "embed-close" => self.embed = None,
            "embed-tab" => {
                if let Some(e) = self.embed.as_mut() {
                    e.tab = match arg {
                        "live" => Tab::Live,
                        "frame" => Tab::Frame,
                        _ => Tab::Snapshot,
                    };
                }
                self.embed_load();
            }
            "embed-set" => {
                if let Some(e) = self.embed.as_mut() {
                    match arg {
                        "who" => e.who = Who::parse(val),
                        "format" if ["csv", "json", "html"].contains(&val) => e.format = val.to_string(),
                        "live" if ["csv", "json"].contains(&val) => e.live = val.to_string(),
                        "repo" => e.repo = val.trim().to_string(),
                        "every" => e.every = val.parse().unwrap_or(0),
                        _ => {}
                    }
                }
                self.embed_load();
            }
            "embed-pin" => {
                if let Some(e) = self.embed.as_mut() {
                    e.pin = arg == "1";
                }
            }
            "embed-copy" | "embed-dl" => self.embed_out(action == "embed-dl", arg),
            _ => return false,
        }
        true
    }

    fn embed_out(&mut self, download: bool, what: &str) {
        let Some(tv) = self.table.as_ref() else { return };
        let who = who_of(self);
        let fmt = self.embed.as_ref().map(|e| e.format.clone()).unwrap_or_else(|| "csv".into());
        let base = crate::app::safe_name(&name_of(tv));
        match what {
            "snapshot" => {
                if !ready(tv, who) {
                    return;
                }
                let (text, n) = snapshot(tv, who, &fmt);
                if download {
                    let mime = match fmt.as_str() {
                        "json" => "application/json",
                        "html" => "text/html",
                        _ => "text/csv",
                    };
                    host::download(&format!("{}.{}", base, fmt), mime, text.as_bytes());
                } else {
                    host::copy(&text);
                    self.ok(format!("Copied {} rows", n));
                }
            }
            "loader" => host::download(LOADER_FILE, "text/javascript", LOADER.as_bytes()),
            "decoder" => {
                if let Found::Ready { decoder: Some(sig), .. } = resolve(self) {
                    self.get(&format!("/data/{}", sig), P::EmbedWasm(sig));
                }
            }
            "iframe" => {
                let code = iframe_code(self, tv, who);
                host::copy(&code);
                self.ok("Copied");
            }
            "server" | "web" => {
                let found = resolve(self);
                let text = snip_for(self, tv, &found).map(|s| {
                    let live = self.embed.as_ref().map(|e| e.live.clone()).unwrap_or_else(|| "csv".into());
                    if what == "web" {
                        s.web()
                    } else {
                        s.server(&live)
                    }
                });
                if let Some(t) = text {
                    host::copy(&t);
                    self.ok("Copied");
                }
            }
            _ => {}
        }
    }

    /// Open the table an iframe shows (`#/embed/…`): every row, and the rows asked for.
    pub fn embed_open_view(&mut self) {
        let Route::Embed { root, pda, who, every } = self.route.clone() else { return };
        self.ensure_dbroots();
        self.open_table(root, pda);
        let gen = self.table.as_mut().map(|t| {
            t.who = who;
            t.mode = Mode::Records;
            t.load_all = true;
            t.full = who != Who::Official;
            t.gen
        });
        if let (Some(gen), true) = (gen, every > 0) {
            self.timer(every.max(MIN_EVERY) * 1000, P::EmbedTick(gen));
        }
    }

    pub fn embed_async(&mut self, p: P, ok: bool, status: u32, data: Vec<u8>) -> bool {
        if let P::EmbedTick(gen) = p {
            // still showing that table: read it again (and set the next read)
            if matches!(self.route, Route::Embed { .. }) && self.table.as_ref().map(|t| t.gen == gen).unwrap_or(false) {
                self.embed_open_view();
                return true;
            }
            return false;
        }
        let P::EmbedWasm(sig) = p else { return false };
        let text = String::from_utf8_lossy(&data).into_owned();
        let bytes = if ok && (200..300).contains(&status) {
            crate::json::parse(&text).ok().and_then(|v| v.get("data").str().map(String::from)).and_then(|d| base64_decode(d.trim()))
        } else {
            None
        };
        match bytes {
            Some(b) if b.starts_with(b"\0asm") => {
                let short: String = sig.chars().take(8).collect();
                host::download(&format!("iqt-decoder-{}.wasm", short), "application/wasm", &b);
            }
            Some(_) => self.err("That inscription isn't a WebAssembly decoder."),
            None => self.err(format!(
                "Couldn't read the decoder from IQ's gateway: {}",
                if ok && (200..300).contains(&status) { "no data".into() } else { fetch_err(ok, status, &text) }
            )),
        }
        true
    }
}

// ------------------------------------------------------------------ view

fn select(arg: &str, cur: &str, opts: &[(&str, &str)]) -> String {
    let mut h = format!("<select data-in=\"embed-set\" data-arg=\"{}\">", arg);
    for (v, label) in opts {
        h.push_str(&format!("<option value=\"{}\"{}>{}</option>", v, if *v == cur { " selected" } else { "" }, label));
    }
    h.push_str("</select>");
    h
}

pub fn panel(app: &App, h: &mut String) {
    let (Some(e), Some(tv)) = (app.embed.as_ref(), app.table.as_ref()) else { return };
    let who = who_of(app);
    let name = name_of(tv);
    h.push_str("<div class=\"modal\" role=\"dialog\" aria-modal=\"true\" aria-label=\"Embed this table\"><div class=\"card sheet embedpanel\">");
    h.push_str(&format!(
        "<div class=\"tablehead\"><h3 class=\"grow\">Use “{}” elsewhere</h3><button class=\"x\" data-a=\"embed-close\" aria-label=\"Close\">×</button></div>",
        esc(&name)
    ));
    let who_opts: Vec<(&str, &str)> = if tv.creator.is_some() {
        vec![("official", "Official rows only"), ("all", "Everyone's rows"), ("unofficial", "Only other people's rows")]
    } else {
        vec![("all", "Every row (the official wallet isn't known yet)")]
    };
    h.push_str(&format!(
        "<div class=\"row\"><div class=\"seg\" role=\"group\" aria-label=\"Kind\"><button class=\"{}\" data-a=\"embed-tab\" data-arg=\"snapshot\">Snapshot</button><button class=\"{}\" data-a=\"embed-tab\" data-arg=\"frame\">Iframe</button><button class=\"{}\" data-a=\"embed-tab\" data-arg=\"live\">Live</button></div><label class=\"inline\">Rows {}</label></div>",
        if e.tab == Tab::Snapshot { "on" } else { "" },
        if e.tab == Tab::Frame { "on" } else { "" },
        if e.tab == Tab::Live { "on" } else { "" },
        select("who", who.as_str(), &who_opts)
    ));
    match e.tab {
        Tab::Snapshot => snapshot_tab(app, e, tv, who, h),
        Tab::Frame => frame_tab(app, e, tv, who, h),
        Tab::Live => live_tab(app, e, tv, who, h),
    }
    if who == Who::Official {
        if let Some(c) = &tv.creator {
            h.push_str(&format!(
                "<p class=\"small muted\">Only rows written by the official wallet ({}) are included, so rows other people add to this table won't show up where you use it.</p>",
                crate::views_account::who(app, c)
            ));
        }
    }
    h.push_str("</div></div>");
}

fn snapshot_tab(_app: &App, e: &Embed, tv: &TableView, who: Who, h: &mut String) {
    h.push_str("<p class=\"small muted\">The data as it is right now, to paste into a page or open in Excel or Google Sheets. It won't change when the table does.</p>");
    h.push_str(&format!(
        "<div class=\"row\"><label class=\"inline\">Format {}</label></div>",
        select("format", &e.format, &[("csv", "CSV (Excel, Google Sheets)"), ("json", "JSON"), ("html", "HTML table")])
    ));
    if !ready(tv, who) {
        if !tv.loading && tv.rows.len() >= SNAPSHOT_MAX {
            h.push_str(&format!(
                "<div class=\"card warnbox small\">This table has more than {} on-chain rows — too many for a snapshot here. Use <b>Live</b> on a server instead.</div>",
                SNAPSHOT_MAX
            ));
        } else if let Some(err) = &tv.err {
            h.push_str(&format!("<div class=\"card bad\">{}</div>", esc(err)));
        } else {
            h.push_str(&format!("<div class=\"loading\">Reading every row first… {} read so far</div>", tv.rows.len()));
        }
        return;
    }
    let (text, n) = snapshot(tv, who, &e.format);
    let src = crate::views::source(tv);
    let when =
        src.as_of().map(|(sig, t)| format!(" · newest write {} ({})", t.map(ui::time).unwrap_or_default(), esc(&solana::short(&sig)))).unwrap_or_default();
    h.push_str(&format!("<p class=\"small\">{} row{}{}</p>", n, if n == 1 { "" } else { "s" }, when));
    let clipped = text.chars().count() > PREVIEW;
    let preview: String = text.chars().take(PREVIEW).collect();
    h.push_str(&format!("<textarea readonly rows=\"12\" class=\"snapshot\" aria-label=\"Snapshot\" spellcheck=\"false\">{}</textarea>", esc(&preview)));
    if clipped {
        h.push_str("<p class=\"small muted\">The preview is cut short; Download and Copy include everything.</p>");
    }
    h.push_str("<div class=\"row\"><button class=\"btn primary\" data-a=\"embed-dl\" data-arg=\"snapshot\">Download</button><button class=\"btn\" data-a=\"embed-copy\" data-arg=\"snapshot\">Copy</button></div>");
}

fn live_tab(app: &App, e: &Embed, tv: &TableView, who: Who, h: &mut String) {
    let _ = who;
    h.push_str(&format!(
        "<p class=\"small muted\">Your server (or your web page) reads the table each time with the IQ Tables decoder, kept on IQ git. You keep one small file, <code>{}</code>: no packages, and nothing hosted by IQ Tables. The decoder runs sealed off — it can't reach anything on your machine, and only talks to IQ's gateway (or the Solana RPC you set).</p>",
        LOADER_FILE
    ));
    let found = resolve(app);
    let repo_field = |h: &mut String, note: &str| {
        h.push_str(&format!(
            "<label class=\"stack\">IQ git repository with the decoder <input data-in=\"embed-set\" data-arg=\"repo\" value=\"{}\" placeholder=\"https://browser.iqlabs.dev/…\" spellcheck=\"false\"></label><p class=\"small muted\">{}</p>",
            esc(&e.repo),
            note
        ));
    };
    match &found {
        Found::NoRepo => repo_field(
            h,
            "This page isn't running from IQ's browser, so it can't tell where its decoder lives. Paste the IQ browser link of the IQ Tables repository (the one this site is deployed from).",
        ),
        Found::Reading(what) => h.push_str(&format!("<div class=\"loading\">{}</div>", what)),
        Found::Bad(msg) => {
            h.push_str(&format!("<div class=\"card bad small\">{}</div>", esc(msg)));
            repo_field(h, "The IQ browser link of the IQ Tables repository.");
        }
        Found::Ready { repo, name, owner, ago, decoder } => {
            h.push_str(&format!(
                "<p class=\"small\">Decoder: <b>{}</b> in <a href=\"{}{}\" target=\"_blank\" rel=\"noopener\">{}</a> by {} · newest commit {}</p>",
                DECODER_FILE,
                git::BROWSER,
                esc(repo),
                esc(name),
                crate::views_account::who(app, owner),
                esc(ago)
            ));
            match decoder {
                None => h.push_str(&format!(
                    "<div class=\"card warnbox small\">The newest commit of {} has no <code>{}</code>. Build and deploy the site with it (see the README).</div>",
                    esc(name),
                    DECODER_FILE
                )),
                Some(sig) => h.push_str(&format!(
                    "<div class=\"row\"><div class=\"seg\" role=\"group\" aria-label=\"Decoder version\"><button class=\"{}\" data-a=\"embed-pin\" data-arg=\"0\">Always the newest</button><button class=\"{}\" data-a=\"embed-pin\" data-arg=\"1\">Pin this version</button></div><span class=\"small muted mono\">{}</span></div><p class=\"small muted\">{}</p>",
                    if e.pin { "" } else { "on" },
                    if e.pin { "on" } else { "" },
                    esc(&solana::short(sig)),
                    if e.pin {
                        "Pinned: nothing changes until you copy a new snippet. Tables saved in a newer storage format still read, through the decoder the repository lists for that format."
                    } else {
                        "Newest: fixes reach you without doing anything. Only the repository owner's commits count, and a decoder can't do anything but return rows."
                    }
                )),
            }
        }
    }
    h.push_str(&format!(
        "<div class=\"row\"><button class=\"btn\" data-a=\"embed-dl\" data-arg=\"loader\">Download {}</button><button class=\"btn\" data-a=\"embed-dl\" data-arg=\"decoder\"{} title=\"A fixed copy of the decoder, for hosts that can't run code they download (Cloudflare Workers, Vercel Edge): pass it as decoder.wasm\">Download the decoder file</button></div>",
        LOADER_FILE,
        if matches!(found, Found::Ready { decoder: Some(_), .. }) { "" } else { " disabled" }
    ));
    if tv.creator.is_none() {
        h.push_str("<div class=\"card warnbox small\">This table's official wallet (its database's creator) isn't known yet, and a live read needs it to tell the owner's rows from anyone else's. Try again once the database shows up in the list.</div>");
        return;
    }
    let Some(s) = snip_for(app, tv, &found) else { return };
    h.push_str(&format!(
        "<div class=\"tablehead\"><h4 class=\"grow\">On your server</h4><label class=\"inline small\">Output {}</label></div>",
        select("live", &e.live, &[("csv", "CSV"), ("json", "JSON")])
    ));
    h.push_str(&format!("<pre class=\"code\" data-snippet=\"server\">{}</pre>", esc(&s.server(&e.live))));
    h.push_str("<div class=\"row\"><button class=\"btn\" data-a=\"embed-copy\" data-arg=\"server\">Copy</button></div>");
    h.push_str("<p class=\"small muted\">For a live spreadsheet, serve that text at a URL and point Excel (Data → From Web) or Google Sheets (<code>=IMPORTDATA(\"…\")</code>) at it. Results are reused for a minute (<code>cacheMs</code>), so busy sites don't read IQ's gateway on every request.</p>");
    h.push_str("<div class=\"tablehead\"><h4 class=\"grow\">In a web page</h4></div>");
    h.push_str(&format!("<pre class=\"code\" data-snippet=\"web\">{}</pre>", esc(&s.web())));
    h.push_str(&format!(
        "<div class=\"row\"><button class=\"btn\" data-a=\"embed-copy\" data-arg=\"web\">Copy</button></div><p class=\"small muted\">Put <code>{}</code> on your site next to the page. Values are shown as text, never as HTML.</p>",
        LOADER_FILE
    ));
}

fn frame_tab(app: &App, e: &Embed, tv: &TableView, who: Who, h: &mut String) {
    h.push_str("<p class=\"small muted\">A read-only view of the table for any page that takes an iframe — Notion (<code>/embed</code>), a Squarespace or Wix code block, WordPress's Custom HTML. It reads the table each time it's shown and links back here; nothing else on the page can be reached from it.</p>");
    h.push_str(&format!(
        "<div class=\"row\"><label class=\"inline\">Read again {}</label></div>",
        select("every", &e.every.to_string(), &[("0", "only when the page loads"), ("300", "every 5 minutes"), ("1800", "every 30 minutes")])
    ));
    let url = frame_url(app, tv, who, e.every);
    h.push_str(&format!("<pre class=\"code\" data-snippet=\"iframe\">{}</pre>", esc(&iframe_code(app, tv, who))));
    h.push_str(&format!(
        "<div class=\"row\"><button class=\"btn primary\" data-a=\"embed-copy\" data-arg=\"iframe\">Copy</button><a class=\"btn\" href=\"{}\" target=\"_blank\" rel=\"noopener\">Preview</a></div>",
        esc(&url)
    ));
    h.push_str("<p class=\"small muted\">It runs IQ Tables itself (about 2 MB, which the visitor's browser keeps after the first load). If the frame stays empty, the host of this site doesn't allow framing — use the web-page code under <b>Live</b> instead.</p>");
}

/// The page inside an iframe: the table, a pager, and a link back.
pub fn view(app: &App, h: &mut String) {
    let Route::Embed { pda, .. } = &app.route else { return };
    h.push_str("<main class=\"frameview\">");
    let Some(tv) = app.table.as_ref().filter(|t| &t.pda == pda) else {
        h.push_str("<p class=\"muted\">Loading…</p></main>");
        return;
    };
    let (cols, rows) = crate::views::view_rows(tv);
    if rows.is_empty() {
        let msg = match &tv.err {
            Some(e) => format!("Couldn't read this table: {}", esc(e)),
            None if tv.loading || !tv.done => "Loading…".into(),
            None => "No rows yet.".into(),
        };
        h.push_str(&format!("<p class=\"muted\">{}</p>", msg));
    } else {
        let pages = rows.len().div_ceil(FRAME_PAGE);
        let page = tv.page.min(pages - 1);
        h.push_str("<div class=\"scroll\"><table class=\"grid data\"><thead><tr>");
        for c in &cols {
            h.push_str(&format!("<th>{}</th>", esc(c)));
        }
        h.push_str("</tr></thead><tbody>");
        for r in rows.iter().skip(page * FRAME_PAGE).take(FRAME_PAGE) {
            h.push_str("<tr>");
            for v in &r.vals {
                let t = v.cell_text();
                let shown: String = t.chars().take(200).collect();
                let web = (t.starts_with("https://") || t.starts_with("http://")) && !t.contains(char::is_whitespace);
                let num = matches!(v, crate::json::Json::Num(_));
                if web {
                    h.push_str(&format!("<td><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a></td>", esc(&t), esc(&shown)));
                } else {
                    h.push_str(&format!(
                        "<td{} title=\"{}\">{}</td>",
                        if num { " class=\"num\"" } else { "" },
                        esc(&t.chars().take(400).collect::<String>()),
                        esc(&shown)
                    ));
                }
            }
            h.push_str("</tr>");
        }
        h.push_str("</tbody></table></div><div class=\"framefoot\">");
        h.push_str(&format!(
            "<span class=\"small muted\">{} row{}{}</span>",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
            if tv.loading { " · reading…" } else { "" }
        ));
        if pages > 1 {
            if page > 0 {
                h.push_str(&format!("<button class=\"btn\" data-a=\"tv-page\" data-arg=\"{}\">‹ Prev</button>", page - 1));
            }
            h.push_str(&format!("<span class=\"small\">{} / {}</span>", page + 1, pages));
            if page + 1 < pages {
                h.push_str(&format!("<button class=\"btn\" data-a=\"tv-page\" data-arg=\"{}\">Next ›</button>", page + 1));
            }
        }
        h.push_str("</div>");
    }
    let at = match &tv.root {
        Some(r) => format!("{}/{}", r, tv.pda),
        None => tv.pda.clone(),
    };
    h.push_str(&format!(
        "<p class=\"framecredit small\"><a href=\"{}#/t/{}\" target=\"_blank\" rel=\"noopener\">{} · on-chain table · IQ Tables</a></p></main>",
        esc(&app.page_url),
        esc(&at),
        esc(&name_of(tv))
    ));
}
