//! Rendering. Every view is plain HTML built from state; `host.js` swaps it
//! into the page and routes `data-*` events back to `App::event`.

use crate::app::{pct_decode, App, Load, Mode, Route, TableView, Who};
use crate::attach;
use crate::crypto::{base58, base64_encode};
use crate::iq;
use crate::json::Json;
use crate::net;
use crate::pack;
use crate::solana::{self, b58, parse_pk};
use crate::ui::{self, addr, esc};
use crate::views_account as va;

const PAGE: usize = 100;

pub fn render(app: &mut App) -> String {
    let mut h = String::with_capacity(64 * 1024);
    header(app, &mut h);
    if let Some((ok, m)) = &app.toast {
        h.push_str(&format!(
            "<div class=\"toast {}\" role=\"status\"><span>{}</span><button class=\"x\" data-a=\"toast-close\" aria-label=\"Dismiss\">×</button></div>",
            if *ok { "ok" } else { "bad" },
            esc(m).replace('\n', "<br>")
        ));
    }
    h.push_str("<main>");
    match app.route.clone() {
        Route::Databases => databases(app, &mut h),
        Route::Db(pda) => database(app, &pda, &mut h),
        Route::Table { .. } => table(app, &mut h),
        Route::Search(_) => search(app, &mut h),
        Route::Mine => va::mine(app, &mut h),
        Route::Account => va::account(app, &mut h),
        Route::Workspace => crate::views_ws::home(app, &mut h),
        Route::Draft(k) => crate::views_ws::page(app, &k, &mut h),
        Route::Settings => settings(app, &mut h),
        Route::About => about(&mut h),
    }
    h.push_str("</main><footer>IQ Tables · a community portal for <a href=\"https://iqlabs.dev\" target=\"_blank\" rel=\"noopener\">IQ Labs</a> on-chain tables · written in Rust, running as WebAssembly · <a href=\"#/about\">how it works</a></footer>");
    crate::git::panel(app, &mut h);
    viewer(app, &mut h);
    if let Some(m) = &app.busy {
        h.push_str(&format!("<div class=\"modal\" role=\"alert\" aria-busy=\"true\"><div class=\"card sheet center\"><p class=\"big\">{}</p><p class=\"muted small\">Deriving the key from your passphrase takes a moment.</p></div></div>", esc(m)));
    }
    h
}

fn header(app: &App, h: &mut String) {
    let tab = |r: &Route| match r {
        Route::Databases | Route::Db(_) | Route::Table { .. } | Route::Search(_) => 0,
        Route::Mine | Route::Account => 1,
        Route::Workspace | Route::Draft(_) => 2,
        Route::Settings => 3,
        Route::About => 4,
    };
    let cur = tab(&app.route);
    let ghosts: usize = app.drafts.iter().map(|d| d.ghosts()).sum();
    let link = |i: usize, href: &str, label: String| format!("<a href=\"{}\" class=\"{}\">{}</a>", href, if i == cur { "on" } else { "" }, label);
    h.push_str("<header class=\"top\"><a class=\"brand\" href=\"#/\"><span class=\"logo\" aria-hidden=\"true\"></span>IQ&nbsp;Tables</a><nav>");
    h.push_str(&link(0, "#/", "Explore".into()));
    h.push_str(&link(1, "#/mine", "My tables".into()));
    h.push_str(&link(2, "#/ws", if ghosts > 0 { format!("Editor <span class=\"pill un\">{}</span>", ghosts) } else { "Editor".into() }));
    h.push_str(&link(3, "#/settings", "Settings".into()));
    h.push_str(&link(4, "#/about", "About".into()));
    h.push_str("</nav>");
    h.push_str(&format!(
        "<input class=\"search\" type=\"search\" id=\"q\" placeholder=\"Search every IQ table…\" value=\"{}\" data-enter=\"search\" aria-label=\"Search\">",
        esc(&app.search_q)
    ));
    va::account_button(app, h);
    h.push_str("</header>");
    if app.settings.cluster == "devnet" {
        h.push_str(if app.use_rpc() { "<div class=\"devnet\">Devnet — test SOL only · reading straight from Solana</div>" } else { "<div class=\"devnet\">Devnet — test SOL only</div>" });
    }
}

// -------------------------------------------------------------------- links

/// A cell value rendered as a link when it points somewhere: IQ links
/// (`iq://table/…`, `iq://db/…`, `iq://tx/…`), web links, `.sol` names
/// (opened in IQ's browser), transaction signatures and known addresses.
pub fn link_html(app: &App, raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 400 || s.contains(char::is_whitespace) || s.contains('<') || s.contains('"') {
        return None;
    }
    let ext = |href: &str, label: &str| Some(format!("<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\" title=\"{}\">↗ {}</a>", esc(href), esc(s), esc(label)));
    let tx = |sig: &str, label: &str| Some(format!("<button class=\"link file\" data-a=\"open-tx\" data-arg=\"{}\" data-val=\"{}\" title=\"{}\">📎 {}</button>", esc(sig), esc(label), esc(s), esc(label)));
    if let Some((sig, name)) = attach::parse_tx_link(s) {
        let label = if name.is_empty() { format!("tx {}", solana::short(&sig)) } else { name };
        return tx(&sig, &label);
    }
    if let Some(rest) = s.strip_prefix("iq://table/") {
        let mut it = rest.splitn(2, '/');
        let pda = it.next()?;
        parse_pk(pda)?;
        let key = it.next().filter(|k| !k.is_empty());
        let known = app.find_table(pda).map(|(r, t)| (r.pda.clone(), format!("{} › {}", r.name(), net::label_of(t)))).or_else(|| draft_table(app, pda));
        let root = known.as_ref().map(|(r, _)| r.clone());
        let mut label = known.map(|(_, l)| l).unwrap_or_else(|| format!("table {}", solana::short(pda)));
        if let Some(k) = key {
            label = format!("{} › {}", label, pct_decode(k));
        }
        let href = match (root, key) {
            (Some(r), Some(k)) => format!("#/t/{}/{}/r/{}", r, pda, k),
            (None, Some(k)) => format!("#/t/{}/r/{}", pda, k),
            (Some(r), None) => format!("#/t/{}/{}", r, pda),
            (None, None) => format!("#/t/{}", pda),
        };
        return Some(format!("<a href=\"{}\" title=\"{}\">↗ {}</a>", esc(&href), esc(s), esc(&label)));
    }
    if let Some(pda) = s.strip_prefix("iq://db/") {
        parse_pk(pda)?;
        let label = app.dbroots.ready().and_then(|rs| rs.iter().find(|r| r.pda == pda)).map(|r| r.name()).unwrap_or_else(|| format!("database {}", solana::short(pda)));
        return Some(format!("<a href=\"#/db/{}\" title=\"{}\">↗ {}</a>", esc(pda), esc(s), esc(&label)));
    }
    if let Some(pda) = crate::git::browser_pda(s) {
        if let Some(c) = crate::git::cell(app, pda, s) {
            return Some(c);
        }
    }
    if s.starts_with("https://") || s.starts_with("http://") {
        let shown = s.split_once("://").map(|x| x.1).unwrap_or(s);
        let shown: String = shown.chars().take(60).collect();
        return ext(s, &shown);
    }
    let lower = s.to_ascii_lowercase();
    if lower.ends_with(".sol") && lower.len() > 4 && lower[..lower.len() - 4].split('.').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')) {
        return ext(&format!("https://browser.iqlabs.dev/{}", lower), &lower);
    }
    match base58::decode(s).map(|b| b.len()) {
        Some(64) => tx(s, &format!("tx {}", solana::short(s))),
        Some(32) => {
            if let Some((r, t)) = app.find_table(s) {
                return Some(format!("<a href=\"#/t/{}/{}\" title=\"{}\">↗ {} › {}</a>", esc(&r.pda), esc(s), esc(s), esc(&r.name()), esc(&net::label_of(t))));
            }
            if let Some(r) = app.dbroots.ready().and_then(|rs| rs.iter().find(|r| r.pda == s)) {
                return Some(format!("<a href=\"#/db/{}\" title=\"{}\">↗ {}</a>", esc(s), esc(s), esc(&r.name())));
            }
            ext(&format!("https://browser.iqlabs.dev/{}", s), &solana::short(s))
        }
        _ => None,
    }
}

thread_local! {
    static TABLE_PDAS: std::cell::RefCell<std::collections::HashMap<(String, String), (String, String)>> = Default::default();
}

/// (root, "db › table") for a table address that belongs to one of the local
/// drafts — so links to brand-new tables read well before IQ's database list
/// (cached ~30 min) catches up. PDAs are memoized; deriving them isn't free.
fn draft_table(app: &App, pda: &str) -> Option<(String, String)> {
    TABLE_PDAS.with(|c| {
        let mut c = c.borrow_mut();
        for d in &app.drafts {
            for t in &d.tables {
                let k = (d.name.clone(), t.name.clone());
                let (root, tp) = c
                    .entry(k)
                    .or_insert_with(|| {
                        let root = iq::db_root_pda(d.name.as_bytes());
                        (b58(&root), b58(&iq::table_pda(&root, &iq::seed_bytes(&t.name))))
                    })
                    .clone();
                if tp == pda {
                    return Some((root, format!("{} › {}", d.name, t.name)));
                }
            }
        }
        None
    })
}

/// Cell contents: a link if the value is one, else (truncated) text.
pub fn cell_html(app: &App, v: &Json, max: usize) -> String {
    if let Json::Str(s) = v {
        if let Some(l) = link_html(app, s) {
            return l;
        }
    }
    let t = v.cell_text();
    let shown: String = t.chars().take(max).collect();
    format!("{}{}", esc(&shown), if t.chars().count() > max { "…" } else { "" })
}

fn viewer(app: &App, h: &mut String) {
    let Some(v) = app.viewer.as_ref() else { return };
    let gw_url = app.gateway_url();
    let gw = gw_url.as_str();
    h.push_str("<div class=\"modal\" role=\"dialog\" aria-modal=\"true\" aria-label=\"Inscription\"><div class=\"card sheet\">");
    let title = match &v.state {
        Load::Ready(x) if x.filename != "iqgit-tree" => x.filename.clone(),
        _ => v.label.clone(),
    };
    h.push_str(&format!("<div class=\"tablehead\"><h3 class=\"grow\">📎 {}</h3><button class=\"x\" data-a=\"viewer-close\" aria-label=\"Close\">×</button></div>", esc(&title)));
    match &v.state {
        Load::Ready(x) => {
            let size = x.bytes.as_ref().map(|b| b.len()).or_else(|| x.text.as_ref().map(|t| t.len())).unwrap_or(0);
            h.push_str(&format!(
                "<p class=\"small muted\">{} · {} · by {}{} · read from {} · <a href=\"{}\" target=\"_blank\" rel=\"noopener\">transaction</a> · <a href=\"{}/{}/{}\" target=\"_blank\" rel=\"noopener\">open in IQ</a></p>",
                esc(&x.filetype),
                ui::bytes(size),
                va::who(app, &x.signer),
                x.time.map(|t| format!(" · {}", ui::time(t))).unwrap_or_default(),
                x.source,
                esc(&ui::solscan_tx(&v.sig, &app.settings.cluster)),
                esc(gw),
                if attach::inline_image(&x.filetype) { "img" } else if x.text.is_some() { "view" } else { "data" },
                esc(&v.sig)
            ));
            if let Some(n) = &x.note {
                h.push_str(&format!("<p class=\"warn\">{}</p>", esc(n)));
            }
            let img = attach::inline_image(&x.filetype);
            match (&x.bytes, &x.text) {
                (Some(b), _) if img => h.push_str(&format!("<img class=\"preview\" alt=\"{}\" src=\"data:{};base64,{}\">", esc(&x.filename), esc(&x.filetype.to_ascii_lowercase()), base64_encode(b))),
                (_, Some(t)) if img => h.push_str(&format!("<img class=\"preview\" alt=\"{}\" src=\"data:image/svg+xml;base64,{}\">", esc(&x.filename), base64_encode(t.as_bytes()))),
                (_, Some(t)) if x.filename == "iqgit-tree" && crate::git::parse_tree(t).is_some() => {
                    crate::git::tree_html(&crate::git::parse_tree(t).unwrap_or_default(), h);
                }
                (_, Some(t)) => {
                    // an IQ Tables pack (or any row) is shown decoded
                    let parsed = crate::json::parse(t).ok();
                    match parsed.as_ref().and_then(crate::app::decode_row) {
                        Some(Ok(p)) => {
                            h.push_str(&format!("<p class=\"small\"><span class=\"pill iqt\">IQT pack</span> {} record(s), columns {}</p>", p.recs.len(), esc(&p.schema.cols.join(", "))));
                            let recs: Vec<Json> = p
                                .recs
                                .iter()
                                .map(|r| Json::Obj(p.schema.cols.iter().cloned().zip(r.vals.iter().cloned()).filter(|(_, v)| !v.is_null()).collect()))
                                .collect();
                            h.push_str(&format!("<pre>{}</pre>", esc(&pretty(&Json::Arr(recs), 0))));
                        }
                        _ => match parsed {
                            Some(j @ (Json::Obj(_) | Json::Arr(_))) => h.push_str(&format!("<pre>{}</pre>", esc(&pretty(&j, 0)))),
                            _ => h.push_str(&format!("<pre class=\"wrap\">{}</pre>", esc(t))),
                        },
                    }
                }
                (Some(b), _) => h.push_str(&format!("<p class=\"muted\">Binary file, {}.</p>", ui::bytes(b.len()))),
                _ => {}
            }
            if x.bytes.is_some() || x.text.as_ref().map(|t| !t.is_empty()).unwrap_or(false) {
                h.push_str("<div class=\"row\"><button class=\"btn primary\" data-a=\"viewer-download\">Download</button>");
            } else {
                h.push_str("<div class=\"row\">");
            }
            h.push_str(&format!("<button class=\"btn\" data-a=\"copy\" data-arg=\"iq://tx/{}#{}\">Copy link</button></div>", esc(&v.sig), esc(&crate::app::pct_encode(&x.filename))));
        }
        Load::Err(e) => h.push_str(&format!("<div class=\"card bad\">{}</div>", esc(e))),
        _ => h.push_str("<div class=\"loading\">Reading the inscription…</div>"),
    }
    h.push_str("</div></div>");
}

// ----------------------------------------------------------------- explore

fn source_note(app: &App) -> &'static str {
    if app.use_rpc() {
        "Source: Solana RPC (live)"
    } else {
        "Source: IQ gateway (cached up to 30 min)"
    }
}

fn databases(app: &App, h: &mut String) {
    h.push_str("<section class=\"hero\"><h1>Every database on IQ</h1><p>Browse the tables people have saved on the Solana blockchain through IQ Labs, or make your own in the <a href=\"#/ws\">Editor</a>.</p></section>");
    match &app.dbroots {
        Load::Ready(roots) => {
            let f = app.db_filter.to_lowercase();
            h.push_str(&format!(
                "<div class=\"toolbar\"><input type=\"search\" id=\"dbf\" placeholder=\"Filter {} databases…\" value=\"{}\" data-live=\"db-filter\" aria-label=\"Filter databases\"><span class=\"muted small\">{}</span></div>",
                roots.len(),
                esc(&app.db_filter),
                source_note(app)
            ));
            let mine: Vec<String> = app.account.as_ref().map(|a| a.addresses()).unwrap_or_default();
            h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Database</th><th class=\"num\">Tables</th><th>Official wallet (creator)</th><th>Table creation</th></tr></thead><tbody>");
            for r in roots {
                let name = r.name();
                if !f.is_empty() && !name.to_lowercase().contains(&f) && !r.tables.iter().any(|t| net::label_of(t).to_lowercase().contains(&f)) {
                    continue;
                }
                let cls = if r.id.is_some() { "" } else { " class=\"muted\"" };
                h.push_str(&format!(
                    "<tr><td><a href=\"#/db/{}\"{}>{}</a>{}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td></tr>",
                    esc(&r.pda),
                    cls,
                    esc(&name),
                    if mine.contains(&r.creator) { " <span class=\"pill off\">yours</span>" } else { "" },
                    r.tables.len(),
                    va::who(app, &r.creator),
                    if r.table_creators.is_empty() { "<span class=\"muted\">anyone</span>".to_string() } else { format!("{} wallet(s)", r.table_creators.len()) }
                ));
            }
            h.push_str("</tbody></table></div>");
        }
        Load::Err(e) => h.push_str(&format!("<div class=\"card bad\">Could not load databases: {}</div>", esc(e))),
        _ => h.push_str("<div class=\"loading\">Loading databases…</div>"),
    }
}

fn database(app: &App, pda: &str, h: &mut String) {
    let Some(r) = app.dbroots.ready().and_then(|rs| rs.iter().find(|r| r.pda == pda)) else {
        if app.dbroots.is_loading() {
            h.push_str("<div class=\"loading\">Loading…</div>");
        } else if app.use_rpc() {
            h.push_str(&format!("<div class=\"card\">No database at {} on {}.</div>", esc(pda), esc(&app.settings.cluster)));
        } else {
            h.push_str(&format!("<div class=\"card\">Database {} isn't in IQ's list yet (it refreshes every ~30 minutes).</div>", esc(pda)));
        }
        return;
    };
    h.push_str(&format!("<p class=\"crumbs\"><a href=\"#/\">Databases</a> › {}</p>", esc(&r.name())));
    h.push_str(&format!("<h1>{}</h1>", esc(&r.name())));
    h.push_str("<div class=\"kv\">");
    h.push_str(&format!("<div><span>DbRoot</span><span><span class=\"mono small\">{}</span> <button class=\"link\" data-a=\"copy\" data-arg=\"iq://db/{}\">copy link</button></span></div>", esc(&r.pda), esc(&r.pda)));
    h.push_str(&format!(
        "<div><span>Official wallet</span><span>{} <button class=\"link\" data-a=\"copy\" data-arg=\"{}\">copy</button> · <a href=\"solana:{}?label={}\" title=\"Open in your wallet to send a donation\">donate</a> · <a href=\"https://browser.iqlabs.dev/{}\" target=\"_blank\" rel=\"noopener\">IQ browser</a> · <a href=\"{}\" target=\"_blank\" rel=\"noopener\">Solscan</a></span></div>",
        va::who(app, &r.creator),
        esc(&r.creator),
        esc(&r.creator),
        esc(&crate::app::pct_encode(&format!("{} on IQ", r.name()))),
        esc(&r.creator),
        esc(&ui::solscan_account(&r.creator, &app.settings.cluster))
    ));
    h.push_str(&format!(
        "<div><span>Table creation</span><span>{}</span></div>",
        if r.table_creators.is_empty() { "open to anyone".into() } else { format!("restricted to {}", r.table_creators.iter().map(|c| addr(c)).collect::<Vec<_>>().join(", ")) }
    ));
    h.push_str("</div>");
    h.push_str("<h2>Tables</h2><div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Table</th><th>Listing</th><th>Address</th><th></th></tr></thead><tbody>");
    for t in &r.tables {
        h.push_str(&format!(
            "<tr><td><a href=\"#/t/{}/{}\">{}</a></td><td>{}</td><td class=\"mono small\">{}</td><td><button class=\"link\" data-a=\"copy\" data-arg=\"iq://table/{}\">copy link</button></td></tr>",
            esc(&r.pda),
            esc(&t.pda),
            esc(&net::label_of(t)),
            if t.public { "public" } else { "<span class=\"muted\">unlisted</span>" },
            esc(&solana::short(&t.pda)),
            esc(&t.pda)
        ));
    }
    h.push_str("</tbody></table></div>");
}

fn search(app: &App, h: &mut String) {
    h.push_str(&format!("<h1>Search: {}</h1>", esc(&app.search_q)));
    let q = app.search_q.trim();
    // a pasted signature or address goes straight to it
    if let Some(l) = link_html(app, q) {
        h.push_str(&format!("<p>Open: {}</p>", l));
    }
    if app.use_rpc() {
        let Some(roots) = app.dbroots.ready() else {
            h.push_str("<div class=\"loading\">Loading databases…</div>");
            return;
        };
        let ql = q.to_lowercase();
        h.push_str("<p class=\"muted small\">Searching database and table names read from Solana (row search needs IQ's gateway).</p><div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Kind</th><th>Match</th><th>Database</th></tr></thead><tbody>");
        let mut n = 0;
        for r in roots {
            if r.name().to_lowercase().contains(&ql) {
                n += 1;
                h.push_str(&format!("<tr><td><span class=\"pill\">dbroot</span></td><td><a href=\"#/db/{}\">{}</a></td><td></td></tr>", esc(&r.pda), esc(&r.name())));
            }
            for t in &r.tables {
                if net::label_of(t).to_lowercase().contains(&ql) {
                    n += 1;
                    h.push_str(&format!(
                        "<tr><td><span class=\"pill\">table</span></td><td><a href=\"#/t/{}/{}\">{}</a></td><td>{}</td></tr>",
                        esc(&r.pda),
                        esc(&t.pda),
                        esc(&net::label_of(t)),
                        esc(&r.name())
                    ));
                }
            }
        }
        if n == 0 {
            h.push_str("<tr><td colspan=\"3\" class=\"muted center\">No matches.</td></tr>");
        }
        h.push_str("</tbody></table></div>");
        return;
    }
    match &app.search {
        Load::Ready(hits) if hits.is_empty() => h.push_str("<p class=\"muted\">No matches in IQ's catalog. New tables appear there as they're read.</p>"),
        Load::Ready(hits) => {
            h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Kind</th><th>Match</th><th>Database</th></tr></thead><tbody>");
            for hit in hits {
                let target = match hit.kind.as_str() {
                    "dbroot" => format!("<a href=\"#/db/{}\">{}</a>", esc(&hit.id), esc(&hit.label)),
                    "table" => {
                        let root = app.find_table(&hit.id).map(|(r, _)| r.pda.clone());
                        match root {
                            Some(r) => format!("<a href=\"#/t/{}/{}\">{}</a>", esc(&r), esc(&hit.id), esc(&hit.label)),
                            None => format!("<a href=\"#/t/{}\">{}</a>", esc(&hit.id), esc(&hit.label)),
                        }
                    }
                    _ => format!(
                        "<button class=\"link\" data-a=\"open-tx\" data-arg=\"{}\" data-val=\"{}\">{}</button> <span class=\"muted small\">{}</span>",
                        esc(&hit.id),
                        esc(&hit.label),
                        esc(&hit.snippet),
                        esc(&hit.label)
                    ),
                };
                h.push_str(&format!("<tr><td><span class=\"pill\">{}</span></td><td>{}</td><td>{}</td></tr>", esc(&hit.kind), target, esc(&hit.dbroot)));
            }
            h.push_str("</tbody></table></div>");
        }
        Load::Err(e) => h.push_str(&format!("<div class=\"card bad\">Search failed: {}</div>", esc(e))),
        _ => h.push_str("<div class=\"loading\">Searching…</div>"),
    }
}

// ------------------------------------------------------------------ tables

pub struct VRow {
    pub key: String,
    pub vals: Vec<Json>,
    pub signer: String,
    pub tx: String,
    pub time: Option<i64>,
    pub official: Option<bool>,
    pub versions: usize,
    pub packed: bool,
}

fn is_packed_table(tv: &TableView) -> bool {
    tv.decoded.iter().any(|d| d.is_some())
}

/// Every decoded pack (data and structure records), oldest first.
fn all_packs(tv: &TableView) -> Vec<pack::SourcePack> {
    // gateway returns newest first; merge wants oldest first
    tv.decoded.iter().rev().filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok())).cloned().collect()
}

/// Records of one group of writers (None = all), with the owner's
/// structure records applied, and the owner's latest structure.
pub fn merged(tv: &TableView, official: Option<bool>) -> (Vec<pack::Merged>, Option<crate::schema::Doc>) {
    let packs = all_packs(tv);
    let creator = tv.creator.clone();
    let is_owner = |s: &str| creator.as_deref().map(|c| c == s).unwrap_or(true);
    let take = |p: &pack::SourcePack| match (official, &creator) {
        (None, _) | (_, None) => true,
        (Some(o), Some(c)) => (&p.signer == c) == o,
    };
    pack::merge_events(&packs, &is_owner, &take)
}

pub fn merged_records(tv: &TableView, who: Who) -> Vec<pack::Merged> {
    match who {
        Who::Official => merged(tv, Some(true)).0,
        Who::Unofficial => merged(tv, Some(false)).0,
        Who::All => {
            let mut v = merged(tv, Some(true)).0;
            v.extend(merged(tv, Some(false)).0);
            v
        }
    }
}

/// The owner's latest structure record for a packed table, if any.
pub fn table_doc(tv: &TableView) -> Option<crate::schema::Doc> {
    merged(tv, Some(true)).1
}

/// Columns and rows for the current table view (filtered and sorted).
pub fn view_rows(tv: &TableView) -> (Vec<String>, Vec<VRow>) {
    let mut cols: Vec<String> = vec![];
    let mut rows: Vec<VRow> = vec![];
    let official_of = |signer: &str| tv.creator.as_ref().map(|c| c == signer);
    if tv.mode == Mode::Records && is_packed_table(tv) {
        // columns: the owner's structure (names, order, types), then any
        // other keys found in rows (from other writers or older layouts)
        let doc = table_doc(tv);
        let mut keys: Vec<(String, Option<crate::schema::ColMeta>)> = vec![];
        if let Some(d) = &doc {
            for (n, m) in &d.cols {
                cols.push(n.clone());
                keys.push((m.key.clone(), Some(m.clone())));
            }
        }
        let retired: Vec<String> = doc.as_ref().map(|d| d.keys.retired.clone()).unwrap_or_default();
        let mut add = |recs: Vec<pack::Merged>, official: Option<bool>, rows: &mut Vec<VRow>, cols: &mut Vec<String>| {
            for m in recs {
                for (c, _) in &m.vals {
                    if !keys.iter().any(|(k, _)| k == c) && !retired.contains(c) {
                        keys.push((c.clone(), None));
                        cols.push(c.clone());
                    }
                }
                rows.push(VRow {
                    key: m.key.clone(),
                    vals: m.vals.iter().map(|(k, v)| Json::Arr(vec![Json::Str(k.clone()), v.clone()])).collect(),
                    signer: m.signer.clone(),
                    tx: m.tx.clone(),
                    time: m.time,
                    official: official.or_else(|| official_of(&m.signer)),
                    versions: m.versions,
                    packed: true,
                });
            }
        };
        if tv.creator.is_none() {
            add(merged(tv, None).0, None, &mut rows, &mut cols);
        } else {
            if tv.who != Who::Unofficial {
                add(merged(tv, Some(true)).0, Some(true), &mut rows, &mut cols);
            }
            if tv.who != Who::Official {
                add(merged(tv, Some(false)).0, Some(false), &mut rows, &mut cols);
            }
        }
        // align values to the columns, reading them through the types
        for r in rows.iter_mut() {
            let pairs: Vec<(String, Json)> = r.vals.iter().map(|p| (p.idx(0).str_or(""), p.idx(1).clone())).collect();
            r.vals = keys
                .iter()
                .map(|(k, m)| match (pairs.iter().find(|(x, _)| x == k), m) {
                    (Some((_, v)), Some(m)) => m.ty.read(v),
                    (Some((_, v)), None) => v.clone(),
                    (None, Some(m)) => m.fill.clone(),
                    (None, None) => Json::Null,
                })
                .collect();
        }
    } else {
        if let Load::Ready(m) = &tv.meta {
            for c in m.get("columns").arr() {
                if let Some(c) = c.str() {
                    cols.push(c.to_string());
                }
            }
        }
        for r in &tv.rows {
            for (k, _) in r.obj() {
                if !k.starts_with("__") && !cols.contains(k) {
                    cols.push(k.clone());
                }
            }
        }
        for r in &tv.rows {
            let signer = r.get("__signer").str_or("");
            let official = official_of(&signer);
            let keep = match (tv.who, official) {
                (_, None) | (Who::All, _) => true,
                (Who::Official, Some(o)) => o,
                (Who::Unofficial, Some(o)) => !o,
            };
            if !keep {
                continue;
            }
            rows.push(VRow {
                key: r.get("__txSignature").str_or(""),
                vals: cols.iter().map(|c| r.get(c).clone()).collect(),
                signer,
                tx: r.get("__txSignature").str_or(""),
                time: r.get("__blockTime").f64().map(|f| f as i64),
                official,
                versions: 1,
                packed: pack::is_packed(r),
            });
        }
    }
    let q = tv.text.to_lowercase();
    if !q.is_empty() {
        rows.retain(|r| r.vals.iter().any(|v| v.cell_text().to_lowercase().contains(&q)) || r.signer.to_lowercase().contains(&q));
    }
    if let Some((c, desc)) = &tv.sort {
        if let Some(i) = cols.iter().position(|x| x == c) {
            rows.sort_by(|a, b| ui::cmp_cells(&a.vals[i].cell_text(), &b.vals[i].cell_text()));
        } else if c == "__time" {
            rows.sort_by_key(|r| r.time.unwrap_or(0));
        } else if c == "__signer" {
            rows.sort_by(|a, b| a.signer.cmp(&b.signer));
        }
        if *desc {
            rows.reverse();
        }
    }
    (cols, rows)
}


pub fn pretty(v: &Json, ind: usize) -> String {
    let pad = "  ".repeat(ind + 1);
    let end = "  ".repeat(ind);
    match v {
        Json::Arr(a) if !a.is_empty() => format!("[\n{}\n{}]", a.iter().map(|x| format!("{}{}", pad, pretty(x, ind + 1))).collect::<Vec<_>>().join(",\n"), end),
        Json::Obj(o) if !o.is_empty() => format!(
            "{{\n{}\n{}}}",
            o.iter().map(|(k, x)| format!("{}{}: {}", pad, crate::json::quote(k), pretty(x, ind + 1))).collect::<Vec<_>>().join(",\n"),
            end
        ),
        other => other.to_string(),
    }
}

fn table(app: &App, h: &mut String) {
    let Some(tv) = app.table.as_ref() else { return };
    let dbname = tv
        .db_id
        .clone()
        .or_else(|| tv.root.as_ref().and_then(|r| app.dbroots.ready()?.iter().find(|d| &d.pda == r).map(|d| d.name())))
        .unwrap_or_else(|| "database".into());
    // the table's own name (RENAME TABLE changes it) before the database's hint
    let meta_name = tv.meta.ready().and_then(|m| m.get("name").str().map(String::from)).filter(|n| !n.is_empty());
    let title = meta_name.clone().or_else(|| tv.label.clone()).unwrap_or_else(|| solana::short(&tv.pda));
    let stored_as = tv.label.clone().filter(|l| Some(l) != meta_name.as_ref() && meta_name.is_some());
    match &tv.root {
        Some(r) => h.push_str(&format!("<p class=\"crumbs\"><a href=\"#/\">Databases</a> › <a href=\"#/db/{}\">{}</a> › {}</p>", esc(r), esc(&dbname), esc(&title))),
        None => h.push_str(&format!("<p class=\"crumbs\"><a href=\"#/\">Databases</a> › {}</p>", esc(&title))),
    }
    let packed = is_packed_table(tv);
    h.push_str(&format!("<h1>{}{}</h1>", esc(&title), if packed { " <span class=\"pill iqt\" title=\"Records are packed and compressed by IQ Tables\">IQT packed</span>" } else { "" }));
    h.push_str("<div class=\"kv\">");
    if let Some(l) = &stored_as {
        h.push_str(&format!("<div><span>Listed as</span><span class=\"mono small\">{}</span></div>", esc(l)));
    }
    if let Load::Ready(m) = &tv.meta {
        let cols: Vec<String> = m.get("columns").arr().iter().map(|c| c.str_or("")).collect();
        h.push_str(&format!("<div><span>On-chain columns</span><span class=\"mono small\">{}</span></div>", esc(&cols.join(", "))));
        h.push_str(&format!("<div><span>ID column</span><span class=\"mono small\">{}</span></div>", esc(&m.get("idCol").str_or(""))));
        if let Some(ts) = m.get("lastTimestamp").f64() {
            if ts > 0.0 {
                h.push_str(&format!("<div><span>Last write</span><span>{}</span></div>", ui::time(ts as i64)));
            }
        }
        if !m.get("gate").is_null() {
            h.push_str(&format!("<div><span>Gate</span><span class=\"mono small\">{}</span></div>", esc(&m.get("gate").to_string())));
        }
    } else if let Load::Err(e) = &tv.meta {
        h.push_str(&format!("<div><span>Table</span><span class=\"warn small\">{}</span></div>", esc(e)));
    }
    h.push_str(&format!(
        "<div><span>Table address</span><span><span class=\"mono small\">{}</span> <button class=\"link\" data-a=\"copy\" data-arg=\"iq://table/{}\">copy link</button></span></div>",
        esc(&tv.pda),
        esc(&tv.pda)
    ));
    match &tv.creator {
        Some(c) => h.push_str(&format!("<div><span>Official wallet</span><span>{}</span></div>", va::who(app, c))),
        None => h.push_str("<div><span>Official wallet</span><span class=\"muted\">unknown (database not in the list yet)</span></div>"),
    }
    h.push_str("</div>");

    let (cols, rows) = view_rows(tv);
    // counts for the filter chips
    let (mut n_off, mut n_un) = (0usize, 0usize);
    if tv.creator.is_some() {
        if packed && tv.mode == Mode::Records {
            n_off = merged(tv, Some(true)).0.len();
            n_un = merged(tv, Some(false)).0.len();
        } else {
            for r in &tv.rows {
                if tv.creator.as_deref() == r.get("__signer").str() {
                    n_off += 1;
                } else {
                    n_un += 1;
                }
            }
        }
    }
    h.push_str("<div class=\"toolbar\">");
    if packed {
        h.push_str(&format!(
            "<div class=\"seg\" role=\"group\" aria-label=\"View\"><button class=\"{}\" data-a=\"tv-mode\" data-arg=\"records\">Records</button><button class=\"{}\" data-a=\"tv-mode\" data-arg=\"raw\">On-chain rows</button></div>",
            if tv.mode == Mode::Records { "on" } else { "" },
            if tv.mode == Mode::Raw { "on" } else { "" }
        ));
    }
    if tv.creator.is_some() {
        h.push_str(&format!(
            "<div class=\"seg\" role=\"group\" aria-label=\"Who wrote it\"><button class=\"{}\" data-a=\"tv-who\" data-arg=\"official\">Official <span class=\"n\">{}</span></button><button class=\"{}\" data-a=\"tv-who\" data-arg=\"unofficial\">Unofficial <span class=\"n\">{}</span></button><button class=\"{}\" data-a=\"tv-who\" data-arg=\"all\">All</button></div>",
            if tv.who == Who::Official { "on" } else { "" },
            n_off,
            if tv.who == Who::Unofficial { "on" } else { "" },
            n_un,
            if tv.who == Who::All { "on" } else { "" }
        ));
    }
    h.push_str(&format!("<input type=\"search\" id=\"tvq\" placeholder=\"Filter rows…\" value=\"{}\" data-live=\"tv-text\" aria-label=\"Filter rows\">", esc(&tv.text)));
    h.push_str("<span class=\"grow\"></span>");
    h.push_str(&format!(
        "<button class=\"btn\" data-a=\"tv-refresh\" title=\"{}\">Refresh</button>",
        if app.use_rpc() { "Read again from Solana" } else { "Reload from IQ's gateway, bypassing its cache" }
    ));
    h.push_str("<button class=\"btn\" data-a=\"tv-export\" data-arg=\"csv\">CSV</button><button class=\"btn\" data-a=\"tv-export\" data-arg=\"json\">JSON</button>");
    if packed && tv.db_id.is_some() {
        h.push_str("<button class=\"btn primary\" data-a=\"tv-draft\" data-arg=\"\" title=\"Open this table in the Editor\">Edit</button>");
    }
    h.push_str("</div>");

    let status = if tv.loading {
        "loading…".to_string()
    } else if tv.done {
        "all rows loaded".into()
    } else {
        "more on chain".into()
    };
    let packs = tv.decoded.iter().filter(|d| d.is_some()).count();
    let bad = tv.decoded.iter().filter(|d| matches!(d, Some(Err(_)))).count();
    h.push_str(&format!(
        "<p class=\"muted small\">{} on-chain row(s) read{} · showing {} {} · {} · {}</p>",
        tv.rows.len(),
        if packed { format!(" ({} packs{})", packs, if bad > 0 { format!(", {} unreadable", bad) } else { String::new() }) } else { String::new() },
        rows.len(),
        if packed && tv.mode == Mode::Records { "record(s)" } else { "row(s)" },
        status,
        source_note(app)
    ));
    if let Some(e) = &tv.err {
        h.push_str(&format!("<div class=\"card bad\">{}</div>", esc(e)));
    }
    // grid
    let pages = (rows.len() + PAGE - 1) / PAGE;
    let page = tv.page.min(pages.saturating_sub(1));
    h.push_str("<div class=\"scroll\"><table class=\"grid data\"><thead><tr>");
    let arrow = |c: &str| match &tv.sort {
        Some((s, false)) if s == c => " ▲",
        Some((s, true)) if s == c => " ▼",
        _ => "",
    };
    for c in &cols {
        h.push_str(&format!("<th><button class=\"sort\" data-a=\"tv-sort\" data-arg=\"{}\">{}{}</button></th>", esc(c), esc(c), arrow(c)));
    }
    h.push_str(&format!(
        "<th><button class=\"sort\" data-a=\"tv-sort\" data-arg=\"__signer\">Signer{}</button></th><th><button class=\"sort\" data-a=\"tv-sort\" data-arg=\"__time\">Time{}</button></th></tr></thead><tbody>",
        arrow("__signer"),
        arrow("__time")
    ));
    for r in rows.iter().skip(page * PAGE).take(PAGE) {
        let sel = tv.selected.as_deref() == Some(&r.key);
        let cls = match r.official {
            Some(false) => "unofficial",
            _ => "",
        };
        h.push_str(&format!("<tr class=\"{}{}\" data-a=\"tv-select\" data-arg=\"{}\">", cls, if sel { " sel" } else { "" }, esc(&r.key)));
        for v in &r.vals {
            let t = v.cell_text();
            let numeric = matches!(v, Json::Num(_));
            h.push_str(&format!(
                "<td class=\"{}\" title=\"{}\">{}</td>",
                if numeric { "num" } else { "" },
                esc(&t.chars().take(400).collect::<String>()),
                cell_html(app, v, 120)
            ));
        }
        let badge = match r.official {
            Some(true) => "<span class=\"pill off\">official</span> ",
            Some(false) => "<span class=\"pill un\">unofficial</span> ",
            None => "",
        };
        h.push_str(&format!("<td>{}{}</td><td class=\"small\">{}</td></tr>", badge, va::who(app, &r.signer), r.time.map(ui::time).unwrap_or_default()));
        if sel {
            h.push_str(&format!("<tr class=\"detail\"><td colspan=\"{}\">", cols.len() + 2));
            let obj = Json::Obj(cols.iter().cloned().zip(r.vals.iter().cloned()).filter(|(_, v)| !v.is_null()).collect());
            h.push_str(&format!("<pre>{}</pre>", esc(&pretty(&obj, 0))));
            let links: Vec<String> = cols
                .iter()
                .zip(&r.vals)
                .filter_map(|(c, v)| v.str().and_then(|s| link_html(app, s)).map(|l| format!("<b>{}</b>: {}", esc(c), l)))
                .collect();
            if !links.is_empty() {
                h.push_str(&format!("<p class=\"small\">Links — {}</p>", links.join(" · ")));
            }
            h.push_str(&format!(
                "<p class=\"small\">Written by {} in <button class=\"link\" data-a=\"open-tx\" data-arg=\"{}\" data-val=\"{}\">{}</button> (<a href=\"{}\" target=\"_blank\" rel=\"noopener\">Solscan</a>){}</p>",
                va::who(app, &r.signer),
                esc(&r.tx),
                esc(&solana::short(&r.tx)),
                esc(&solana::short(&r.tx)),
                esc(&ui::solscan_tx(&r.tx, &app.settings.cluster)),
                if r.versions > 1 { format!(" · {} versions (latest shown)", r.versions) } else { String::new() }
            ));
            h.push_str("<div class=\"row\">");
            if r.packed && tv.mode == Mode::Records {
                h.push_str(&format!("<button class=\"btn\" data-a=\"copy-record\" data-arg=\"{}\" title=\"An iq://table/… link you can paste into another table's cell\">Copy record link</button>", esc(&r.key)));
                if tv.db_id.is_some() {
                    h.push_str(&format!("<button class=\"btn\" data-a=\"tv-draft\" data-arg=\"{}\">Edit in the Editor</button>", esc(&r.key)));
                }
            }
            h.push_str("</div></td></tr>");
        }
    }
    if rows.is_empty() && !tv.loading {
        h.push_str(&format!("<tr><td colspan=\"{}\" class=\"muted center\">No rows match.</td></tr>", cols.len() + 2));
    }
    h.push_str("</tbody></table></div>");
    h.push_str("<div class=\"toolbar\">");
    if pages > 1 {
        h.push_str(&format!("<span class=\"small\">Page {} of {}</span>", page + 1, pages));
        if page > 0 {
            h.push_str(&format!("<button class=\"btn\" data-a=\"tv-page\" data-arg=\"{}\">‹ Prev</button>", page - 1));
        }
        if page + 1 < pages {
            h.push_str(&format!("<button class=\"btn\" data-a=\"tv-page\" data-arg=\"{}\">Next ›</button>", page + 1));
        }
    }
    h.push_str("<span class=\"grow\"></span>");
    if !tv.done {
        let dis = if tv.loading { "disabled" } else { "" };
        h.push_str(&format!(
            "<button class=\"btn\" data-a=\"tv-more\" {}>Load more</button><button class=\"btn\" data-a=\"tv-all\" {}>Load everything</button>",
            dis, dis
        ));
    }
    h.push_str("</div>");
}

// ---------------------------------------------------------------- settings

fn settings(app: &App, h: &mut String) {
    let s = &app.settings;
    h.push_str("<h1>Settings</h1><div class=\"card formgrid\">");
    h.push_str(&format!(
        "<label>Read tables from<select data-in=\"set\" data-arg=\"source\"><option value=\"gateway\" {}>IQ gateway — fast, cached, with search and files (mainnet)</option><option value=\"rpc\" {}>Solana directly — live, no cache (needs an RPC that allows getProgramAccounts)</option></select></label>",
        if s.source != "rpc" { "selected" } else { "" },
        if s.source == "rpc" { "selected" } else { "" }
    ));
    h.push_str(&format!(
        "<label>IQ gateway<input value=\"{}\" data-in=\"set\" data-arg=\"gateway\" spellcheck=\"false\"></label>",
        esc(&s.gateway)
    ));
    h.push_str(&format!(
        "<label>Solana RPC (writes, balances, direct reads)<input value=\"{}\" data-in=\"set\" data-arg=\"rpc\" spellcheck=\"false\"></label><p class=\"muted small\">The public endpoint is heavily rate-limited and may refuse browser requests. A free key from Helius, QuickNode or Triton works much better.</p>",
        esc(&s.rpc)
    ));
    h.push_str(&format!(
        "<label>Cluster<select data-in=\"set\" data-arg=\"cluster\"><option value=\"mainnet\" {}>mainnet-beta</option><option value=\"devnet\" {}>devnet (free test SOL; reads through IQ's devnet gateway)</option></select></label>",
        if s.cluster == "mainnet" { "selected" } else { "" },
        if s.cluster == "devnet" { "selected" } else { "" }
    ));
    h.push_str(&format!(
        "<label>Transaction format<select data-in=\"set\" data-arg=\"tx\"><option value=\"auto\" {}>Auto — v1 (4 KB, ~5× bigger packs) once the cluster has it, else legacy</option><option value=\"v1\" {}>v1 only</option><option value=\"legacy\" {}>Legacy only (1.2 KB)</option></select></label>",
        if s.tx_format == crate::state::TxFormat::Auto { "selected" } else { "" },
        if s.tx_format == crate::state::TxFormat::V1 { "selected" } else { "" },
        if s.tx_format == crate::state::TxFormat::Legacy { "selected" } else { "" }
    ));
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"set\" data-arg=\"simulate\" {}> Simulate every transaction before sending (shows exact costs, catches errors for free)</label>",
        if s.simulate { "checked" } else { "" }
    ));
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"set\" data-arg=\"notify\" {}> Tell IQ's gateway about new rows right away</label>",
        if s.notify_gateway { "checked" } else { "" }
    ));
    h.push_str("</div>");
}

fn about(h: &mut String) {
    h.push_str(r#"<h1>How IQ Tables works</h1>
<div class="card prose">
<h3>Ghost data, then inscription</h3>
<p>Drafts live in your browser. Inscribing turns them into IQ Labs table rows on Solana with <code>db_code_in</code>, the same instruction the official SDK uses. Before each step the transaction is simulated, so errors and the exact cost show up before any SOL moves.</p>
<h3>Packing and compression</h3>
<p>Each on-chain row holds a <em>pack</em> of many records: <code>{"id": pack-id, "p": "IQT1z…"}</code>. Records are laid out column by column, compressed with a small context-mixing compressor (order 1–5 contexts, a match model and logistic mixing — tighter than gzip or brotli on small tables), then written with a 92-character alphabet that never needs escaping inside JSON. A pack fills one transaction (up to 3,400 bytes of metadata with v1 transactions), so a single 0.001 SOL write can carry hundreds of records. Packs carry their own column list, so tables can gain columns later. Newer records replace older ones with the same id; deletions are tombstone records. Uncompressed packs (<code>IQT1j</code>) are available when you want rows searchable by the gateway.</p>
<h3>Accounts and wallets</h3>
<p>Your account is one file holding all your wallets. Drop it anywhere on the page and every wallet in it is unlocked — no browser extension. New wallets are derived from the account's 32-byte master secret (<code>SHA-256("iq-tables/account/v1/wallet" ‖ master ‖ index)</code>), so a wallet made after your last save is found again on login. Keys from elsewhere (Solana CLI keypairs, base58 secret keys) can be imported and live in the file. The file is encrypted with the IQ SDK's <code>passwordEncrypt</code> scheme (PBKDF2-SHA256 × 250,000 → AES-256-GCM), so the SDK's <code>passwordDecrypt</code> can open it too. Each database can have its own wallet: it creates the database, so its address is the <em>official</em> signer and a public donation address. Rows written by anyone else show as <em>unofficial</em>.</p>
<h3>Links and files</h3>
<p>Cells can link anywhere: <code>iq://table/&lt;table&gt;/&lt;record&gt;</code> and <code>iq://db/&lt;database&gt;</code> open in the explorer, web links and <code>.sol</code> names open in a new tab (<code>.sol</code> through IQ's browser), and <code>iq://tx/&lt;signature&gt;</code> opens an inscription. Attaching a file inscribes it with <code>user_inventory_code_in</code>, exactly like the SDK's <code>codeIn</code> (text as text, binary as base64), and IQ's gateway serves it back at <code>/data</code>, <code>/img</code> and <code>/view</code>.</p>
<h3>Written in Rust</h3>
<p>Everything — SHA-2, Keccak, Ed25519, PBKDF2, AES-GCM, Base58, Solana transaction encoding (legacy and v1), the IQ program's instructions, JSON, compression, QR codes, the UI — is dependency-free Rust compiled to WebAssembly. A small JavaScript file only connects it to the page, the network and your files.</p>
</div>"#);
}
