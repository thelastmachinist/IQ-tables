//! Rendering. Every view is plain HTML built from state; `host.js` swaps it
//! into the page and routes `data-*` events back to `App::event`.

use crate::app::{App, Load, Mode, Route, TableView, Who, ROWS_PER_PAGE};
use crate::inscribe::RunState;
use crate::iq;
use crate::json::Json;
use crate::net;
use crate::pack;
use crate::solana::{self, b58};
use crate::ui::{self, addr, esc};

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
        Route::Workspace => workspace(app, &mut h),
        Route::Draft(k) => draft(app, &k, &mut h),
        Route::Settings => settings(app, &mut h),
        Route::About => about(&mut h),
    }
    h.push_str("</main><footer>IQ Tables · a community portal for <a href=\"https://iqlabs.dev\" target=\"_blank\" rel=\"noopener\">IQ Labs</a> on-chain tables · written in Rust, running as WebAssembly · <a href=\"#/about\">how it works</a></footer>");
    h
}

fn header(app: &App, h: &mut String) {
    let tab = |r: &Route| match r {
        Route::Databases | Route::Db(_) | Route::Table { .. } | Route::Search(_) => 0,
        Route::Workspace | Route::Draft(_) => 1,
        Route::Settings => 2,
        Route::About => 3,
    };
    let cur = tab(&app.route);
    let ghosts: usize = app.drafts.iter().map(|d| d.ghosts()).sum();
    let link = |i: usize, href: &str, label: String| {
        format!("<a href=\"{}\" class=\"{}\">{}</a>", href, if i == cur { "on" } else { "" }, label)
    };
    h.push_str("<header class=\"top\"><a class=\"brand\" href=\"#/\"><span class=\"logo\" aria-hidden=\"true\"></span>IQ&nbsp;Tables</a><nav>");
    h.push_str(&link(0, "#/", "Explore".into()));
    h.push_str(&link(
        1,
        "#/ws",
        if ghosts > 0 { format!("Workspace <span class=\"pill ghost\">{}</span>", ghosts) } else { "Workspace".into() },
    ));
    h.push_str(&link(2, "#/settings", "Settings".into()));
    h.push_str(&link(3, "#/about", "About".into()));
    h.push_str("</nav>");
    h.push_str(&format!(
        "<input class=\"search\" type=\"search\" id=\"q\" placeholder=\"Search every IQ table…\" value=\"{}\" data-enter=\"search\" aria-label=\"Search\">",
        esc(&app.search_q)
    ));
    h.push_str("<div class=\"wallet\">");
    match &app.owner {
        Some((name, a)) => {
            let bal = app.balances.get(a).and_then(|b| b.ready().copied()).map(ui::sol).unwrap_or_default();
            h.push_str(&format!(
                "<button class=\"btn ghostbtn\" data-a=\"connect-menu\" title=\"{}\"><span class=\"dot on\"></span>{} · {}</button>",
                esc(a),
                esc(&solana::short(a)),
                esc(if bal.is_empty() { name } else { &bal })
            ));
            if app.connect_menu {
                h.push_str(&format!(
                    "<div class=\"menu\"><div class=\"muted small\">Connected with {}</div><div class=\"mono small\">{}</div><button class=\"btn\" data-a=\"copy\" data-arg=\"{}\">Copy address</button><button class=\"btn\" data-a=\"disconnect\">Disconnect</button></div>",
                    esc(name),
                    esc(a),
                    esc(a)
                ));
            }
        }
        None => {
            h.push_str("<button class=\"btn primary\" data-a=\"connect-menu\">Connect wallet</button>");
            if app.connect_menu {
                h.push_str("<div class=\"menu\">");
                if app.wallets.is_empty() {
                    h.push_str("<p class=\"small\">No Solana wallet found in this browser. Install <a href=\"https://phantom.com\" target=\"_blank\" rel=\"noopener\">Phantom</a>, Solflare or Backpack, then reload.</p>");
                }
                for (name, icon) in &app.wallets {
                    let img = if icon.starts_with("data:image/") {
                        format!("<img src=\"{}\" alt=\"\" width=\"18\" height=\"18\">", esc(icon))
                    } else {
                        String::new()
                    };
                    h.push_str(&format!("<button class=\"btn wide\" data-a=\"connect\" data-arg=\"{}\">{}{}</button>", esc(name), img, esc(name)));
                }
                h.push_str("<p class=\"muted small\">Your wallet is your account. It never signs a transaction without showing you first.</p></div>");
            }
        }
    }
    h.push_str("</div></header>");
}

// ----------------------------------------------------------------- explore

fn databases(app: &App, h: &mut String) {
    h.push_str("<section class=\"hero\"><h1>Every database on IQ</h1><p>Browse the tables people have inscribed on Solana through IQ Labs, or build your own in the <a href=\"#/ws\">workspace</a> and inscribe it when it's ready.</p></section>");
    match &app.dbroots {
        Load::Ready(roots) => {
            let f = app.db_filter.to_lowercase();
            h.push_str(&format!(
                "<div class=\"toolbar\"><input type=\"search\" id=\"dbf\" placeholder=\"Filter {} databases…\" value=\"{}\" data-live=\"db-filter\" aria-label=\"Filter databases\"><span class=\"muted small\">Source: IQ gateway (cached up to 30 min)</span></div>",
                roots.len(),
                esc(&app.db_filter)
            ));
            h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Database</th><th>Tables</th><th>Official wallet (creator)</th><th>Table creation</th></tr></thead><tbody>");
            for r in roots {
                let name = r.name();
                if !f.is_empty() && !name.to_lowercase().contains(&f) && !r.tables.iter().any(|t| net::label_of(t).to_lowercase().contains(&f)) {
                    continue;
                }
                let cls = if r.id.is_some() { "" } else { " class=\"muted\"" };
                h.push_str(&format!(
                    "<tr><td><a href=\"#/db/{}\"{}>{}</a></td><td class=\"num\">{}</td><td>{}</td><td>{}</td></tr>",
                    esc(&r.pda),
                    cls,
                    esc(&name),
                    r.tables.len(),
                    addr(&r.creator),
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
        } else {
            h.push_str(&format!("<div class=\"card\">Database {} isn't in the gateway's list yet (it caches for up to 30 minutes).</div>", esc(pda)));
        }
        return;
    };
    h.push_str(&format!("<p class=\"crumbs\"><a href=\"#/\">Databases</a> › {}</p>", esc(&r.name())));
    h.push_str(&format!("<h1>{}</h1>", esc(&r.name())));
    h.push_str("<div class=\"kv\">");
    h.push_str(&format!("<div><span>DbRoot</span><span class=\"mono\">{}</span></div>", esc(&r.pda)));
    h.push_str(&format!(
        "<div><span>Official wallet</span><span>{} <button class=\"link\" data-a=\"copy\" data-arg=\"{}\">copy</button> · <a href=\"solana:{}?label={}\" title=\"Open in your wallet to send a donation\">donate</a> · <a href=\"{}\" target=\"_blank\" rel=\"noopener\">explorer</a></span></div>",
        addr(&r.creator),
        esc(&r.creator),
        esc(&r.creator),
        ui::esc(&crate::app::pct_encode(&format!("{} on IQ", r.name()))),
        esc(&ui::solscan_account(&r.creator, &app.settings.cluster))
    ));
    h.push_str(&format!(
        "<div><span>Table creation</span><span>{}</span></div>",
        if r.table_creators.is_empty() { "open to anyone".into() } else { format!("restricted to {}", r.table_creators.iter().map(|c| addr(c)).collect::<Vec<_>>().join(", ")) }
    ));
    h.push_str("</div>");
    h.push_str("<h2>Tables</h2><div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Table</th><th>Listing</th><th>Address</th></tr></thead><tbody>");
    for t in &r.tables {
        h.push_str(&format!(
            "<tr><td><a href=\"#/t/{}/{}\">{}</a></td><td>{}</td><td class=\"mono small\">{}</td></tr>",
            esc(&r.pda),
            esc(&t.pda),
            esc(&net::label_of(t)),
            if t.public { "public" } else { "<span class=\"muted\">unlisted</span>" },
            esc(&solana::short(&t.pda))
        ));
    }
    h.push_str("</tbody></table></div>");
}

fn search(app: &App, h: &mut String) {
    h.push_str(&format!("<h1>Search: {}</h1>", esc(&app.search_q)));
    match &app.search {
        Load::Ready(hits) if hits.is_empty() => h.push_str("<p class=\"muted\">No matches in the gateway's catalog. New tables appear there as they're read.</p>"),
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
                        "<a href=\"{}\" target=\"_blank\" rel=\"noopener\">{}</a> <span class=\"muted small\">{}</span>",
                        esc(&ui::solscan_tx(&hit.id, &app.settings.cluster)),
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

fn source_packs(tv: &TableView, official: Option<bool>) -> Vec<pack::SourcePack> {
    // gateway returns newest first; merge wants oldest first
    tv.decoded
        .iter()
        .rev()
        .filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok()))
        .filter(|p| match (official, &tv.creator) {
            (None, _) | (_, None) => true,
            (Some(o), Some(c)) => (&p.signer == c) == o,
        })
        .cloned()
        .collect()
}

pub fn merged_records(tv: &TableView, who: Who) -> Vec<pack::Merged> {
    match who {
        Who::Official => pack::merge(&source_packs(tv, Some(true))),
        Who::Unofficial => pack::merge(&source_packs(tv, Some(false))),
        Who::All => {
            let mut v = pack::merge(&source_packs(tv, Some(true)));
            v.extend(pack::merge(&source_packs(tv, Some(false))));
            v
        }
    }
}

/// Columns and rows for the current table view (filtered and sorted).
pub fn view_rows(tv: &TableView) -> (Vec<String>, Vec<VRow>) {
    let mut cols: Vec<String> = vec![];
    let mut rows: Vec<VRow> = vec![];
    let official_of = |signer: &str| tv.creator.as_ref().map(|c| c == signer);
    if tv.mode == Mode::Records && is_packed_table(tv) {
        let add = |recs: Vec<pack::Merged>, official: Option<bool>, rows: &mut Vec<VRow>, cols: &mut Vec<String>| {
            for m in recs {
                for (c, _) in &m.vals {
                    if !cols.contains(c) {
                        cols.push(c.clone());
                    }
                }
                rows.push(VRow {
                    key: m.key.clone(),
                    vals: vec![],
                    signer: m.signer.clone(),
                    tx: m.tx.clone(),
                    time: m.time,
                    official: official.or_else(|| official_of(&m.signer)),
                    versions: m.versions,
                    packed: true,
                });
                let last = rows.last_mut().unwrap();
                last.vals = m.vals.iter().map(|(_, v)| v.clone()).collect();
                // stash names for re-alignment below
                last.key = format!("{}\u{0}{}", m.key, m.vals.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>().join("\u{1}"));
            }
        };
        if tv.creator.is_none() {
            add(pack::merge(&source_packs(tv, None)), None, &mut rows, &mut cols);
        } else {
            if tv.who != Who::Unofficial {
                add(pack::merge(&source_packs(tv, Some(true))), Some(true), &mut rows, &mut cols);
            }
            if tv.who != Who::Official {
                add(pack::merge(&source_packs(tv, Some(false))), Some(false), &mut rows, &mut cols);
            }
        }
        // align values to the union of columns
        for r in rows.iter_mut() {
            let (key, names) = r.key.split_once('\u{0}').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or_default();
            let names: Vec<&str> = if names.is_empty() { vec![] } else { names.split('\u{1}').collect() };
            let mut vals = vec![Json::Null; cols.len()];
            for (i, n) in names.iter().enumerate() {
                if let Some(p) = cols.iter().position(|c| c == n) {
                    vals[p] = r.vals.get(i).cloned().unwrap_or(Json::Null);
                }
            }
            r.vals = vals;
            r.key = key;
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

fn table(app: &App, h: &mut String) {
    let Some(tv) = app.table.as_ref() else { return };
    let dbname = tv
        .db_id
        .clone()
        .or_else(|| tv.root.as_ref().and_then(|r| app.dbroots.ready()?.iter().find(|d| &d.pda == r).map(|d| d.name())))
        .unwrap_or_else(|| "database".into());
    let title = tv
        .label
        .clone()
        .or_else(|| tv.meta.ready().and_then(|m| m.get("name").str().map(String::from)))
        .unwrap_or_else(|| solana::short(&tv.pda));
    match &tv.root {
        Some(r) => h.push_str(&format!("<p class=\"crumbs\"><a href=\"#/\">Databases</a> › <a href=\"#/db/{}\">{}</a> › {}</p>", esc(r), esc(&dbname), esc(&title))),
        None => h.push_str(&format!("<p class=\"crumbs\"><a href=\"#/\">Databases</a> › {}</p>", esc(&title))),
    }
    let packed = is_packed_table(tv);
    h.push_str(&format!("<h1>{}{}</h1>", esc(&title), if packed { " <span class=\"pill iqt\" title=\"Records are packed and compressed by IQ Tables\">IQT packed</span>" } else { "" }));
    h.push_str("<div class=\"kv\">");
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
    }
    h.push_str(&format!("<div><span>Table address</span><span class=\"mono small\">{}</span></div>", esc(&tv.pda)));
    match &tv.creator {
        Some(c) => h.push_str(&format!("<div><span>Official wallet</span><span>{}</span></div>", addr(c))),
        None => h.push_str("<div><span>Official wallet</span><span class=\"muted\">unknown (database not in the gateway list yet)</span></div>"),
    }
    h.push_str("</div>");

    let (cols, rows) = view_rows(tv);
    // counts for the filter chips
    let (mut n_off, mut n_un) = (0usize, 0usize);
    if tv.creator.is_some() {
        if packed && tv.mode == Mode::Records {
            n_off = pack::merge(&source_packs(tv, Some(true))).len();
            n_un = pack::merge(&source_packs(tv, Some(false))).len();
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
    h.push_str(&format!(
        "<input type=\"search\" id=\"tvq\" placeholder=\"Filter rows…\" value=\"{}\" data-live=\"tv-text\" aria-label=\"Filter rows\">",
        esc(&tv.text)
    ));
    h.push_str("<span class=\"grow\"></span>");
    h.push_str("<button class=\"btn\" data-a=\"tv-refresh\" title=\"Reload from the gateway, bypassing its cache\">Refresh</button>");
    h.push_str("<button class=\"btn\" data-a=\"tv-export\" data-arg=\"csv\">CSV</button><button class=\"btn\" data-a=\"tv-export\" data-arg=\"json\">JSON</button>");
    if packed && tv.db_id.is_some() {
        h.push_str("<button class=\"btn primary\" data-a=\"tv-draft\" data-arg=\"\" title=\"Draft additions or edits in your workspace\">Draft changes</button>");
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
        "<p class=\"muted small\">{} on-chain row(s) read{} · showing {} {} · {}</p>",
        tv.rows.len(),
        if packed { format!(" ({} packs{})", packs, if bad > 0 { format!(", {} unreadable", bad) } else { String::new() }) } else { String::new() },
        rows.len(),
        if packed && tv.mode == Mode::Records { "record(s)" } else { "row(s)" },
        status
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
            let shown: String = t.chars().take(120).collect();
            let numeric = matches!(v, Json::Num(_));
            h.push_str(&format!(
                "<td class=\"{}\" title=\"{}\">{}{}</td>",
                if numeric { "num" } else { "" },
                esc(&t.chars().take(400).collect::<String>()),
                esc(&shown),
                if t.chars().count() > 120 { "…" } else { "" }
            ));
        }
        let badge = match r.official {
            Some(true) => "<span class=\"pill off\">official</span> ",
            Some(false) => "<span class=\"pill un\">unofficial</span> ",
            None => "",
        };
        h.push_str(&format!(
            "<td>{}{}</td><td class=\"small\">{}</td></tr>",
            badge,
            addr(&r.signer),
            r.time.map(ui::time).unwrap_or_default()
        ));
        if sel {
            h.push_str(&format!("<tr class=\"detail\"><td colspan=\"{}\">", cols.len() + 2));
            let obj = Json::Obj(cols.iter().cloned().zip(r.vals.iter().cloned()).filter(|(_, v)| !v.is_null()).collect());
            h.push_str(&format!("<pre>{}</pre>", esc(&pretty(&obj, 0))));
            h.push_str(&format!(
                "<p class=\"small\">Written by {} in tx <a href=\"{}\" target=\"_blank\" rel=\"noopener\">{}</a>{}</p>",
                addr(&r.signer),
                esc(&ui::solscan_tx(&r.tx, &app.settings.cluster)),
                esc(&solana::short(&r.tx)),
                if r.versions > 1 { format!(" · {} versions (latest shown)", r.versions) } else { String::new() }
            ));
            if r.packed && tv.mode == Mode::Records && tv.db_id.is_some() {
                h.push_str(&format!("<button class=\"btn\" data-a=\"tv-draft\" data-arg=\"{}\">Edit in workspace</button>", esc(&r.key)));
            }
            h.push_str("</td></tr>");
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
        h.push_str(&format!(
            "<button class=\"btn\" data-a=\"tv-more\" {}>Load 100 more</button><button class=\"btn\" data-a=\"tv-all\" {}>Load everything</button>",
            if tv.loading { "disabled" } else { "" },
            if tv.loading { "disabled" } else { "" }
        ));
    }
    h.push_str("</div>");
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

// --------------------------------------------------------------- workspace

fn workspace(app: &App, h: &mut String) {
    h.push_str("<section class=\"hero\"><h1>Workspace</h1><p>Build databases as <em>ghost data</em> first: tables and rows that live only in this browser until you inscribe them. Nothing costs anything until you press Inscribe.</p></section>");
    h.push_str("<div class=\"card\"><h3>New database</h3><div class=\"row\">");
    h.push_str(&format!(
        "<input id=\"newdb\" placeholder=\"database name (max 32 bytes), e.g. texas-fasteners\" value=\"{}\" data-in=\"form\" data-arg=\"new-db\" data-enter=\"new-db\" maxlength=\"32\" aria-label=\"Database name\">",
        esc(app.form.get("new-db").map(|s| s.as_str()).unwrap_or(""))
    ));
    h.push_str("<button class=\"btn primary\" data-a=\"new-db\">Create draft</button></div><p class=\"muted small\">The name becomes the database's permanent on-chain id. First come, first served.</p></div>");
    if app.drafts.is_empty() {
        h.push_str("<p class=\"muted\">No drafts yet.</p>");
    } else {
        h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Database</th><th>Tables</th><th>Ghost rows</th><th>Inscribed rows</th><th>Database wallet</th></tr></thead><tbody>");
        for d in &app.drafts {
            let inscribed: usize = d.tables.iter().map(|t| t.rows.len() - t.ghosts()).sum();
            h.push_str(&format!(
                "<tr><td><a href=\"#/ws/{}\">{}</a>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td></tr>",
                esc(&d.key),
                esc(&d.name),
                if d.root_sig.is_some() { " <span class=\"pill off\">on chain</span>" } else { " <span class=\"pill ghost\">ghost</span>" },
                d.tables.len(),
                d.ghosts(),
                inscribed,
                d.wallet.as_ref().map(|w| addr(w)).unwrap_or_else(|| "<span class=\"muted\">not created</span>".into())
            ));
        }
        h.push_str("</tbody></table></div>");
    }
    h.push_str("<div class=\"row\"><button class=\"btn\" data-a=\"export-ws\">Export workspace file</button><label class=\"btn\">Import workspace file<input type=\"file\" accept=\".json,application/json\" data-file=\"import-ws\" hidden></label></div>");
    h.push_str("<p class=\"muted small\">Drafts are stored in this browser only. Export a file to move them or keep a backup.</p>");
}

fn draft(app: &mut App, key: &str, h: &mut String) {
    let Some(di) = app.draft_idx(key) else {
        h.push_str("<div class=\"card\">That draft doesn't exist in this browser. <a href=\"#/ws\">Back to workspace</a></div>");
        return;
    };
    // pack plans first (needs &mut for the cache)
    let cap = app.inline_cap();
    let ntables = app.drafts[di].tables.len();
    let mut plans: Vec<Result<Vec<pack::PlannedPack>, String>> = vec![];
    for t in 0..ntables {
        plans.push(app.plan_for(key, t, cap).clone());
    }
    let d = &app.drafts[di];
    let unlocked = app.keys.get(key).cloned();
    let wallet = d.wallet.clone();
    let root_pda = b58(&iq::db_root_pda(d.name.as_bytes()));
    h.push_str(&format!("<p class=\"crumbs\"><a href=\"#/ws\">Workspace</a> › {}</p>", esc(&d.name)));
    h.push_str(&format!(
        "<h1>{} {}</h1>",
        esc(&d.name),
        if d.root_sig.is_some() { "<span class=\"pill off\">on chain</span>" } else { "<span class=\"pill ghost\">ghost</span>" }
    ));
    // name availability
    let taken = match app.name_checks.get(key) {
        Some(Load::Ready(None)) => "<span class=\"good\">✓ name available</span>".to_string(),
        Some(Load::Ready(Some(c))) => {
            if wallet.as_deref() == Some(c.as_str()) {
                "<span class=\"good\">✓ this database is yours</span>".to_string()
            } else {
                format!("<span class=\"warn\">Taken — owned by {}. You can still add <em>unofficial</em> rows to its open tables.</span>", addr(c))
            }
        }
        Some(Load::Err(e)) => format!("<span class=\"warn\">Couldn't check the name: {}</span>", esc(e)),
        _ => "<span class=\"muted\">checking name…</span>".into(),
    };
    h.push_str(&format!("<p class=\"small\">DbRoot <span class=\"mono\">{}</span> · {}</p>", esc(&solana::short(&root_pda)), taken));

    h.push_str("<div class=\"cols2\">");
    // ---- wallet card
    h.push_str("<section class=\"card\"><h3>Database wallet</h3>");
    match (&app.owner, &unlocked) {
        (_, Some(kp)) => {
            let a = b58(&kp.pubkey);
            let bal = match app.balances.get(&a) {
                Some(Load::Ready(b)) => ui::sol(*b),
                Some(Load::Loading) => "…".into(),
                Some(Load::Err(e)) => format!("error: {}", e),
                _ => "—".into(),
            };
            h.push_str(&format!(
                "<p>This wallet signs everything for <b>{}</b> and is its <b>official</b> identity. Anyone can fund it by sending SOL to its address.</p>",
                esc(&d.name)
            ));
            h.push_str(&format!(
                "<div class=\"addrbox\"><span class=\"mono\">{}</span><button class=\"link\" data-a=\"copy\" data-arg=\"{}\">copy</button><a href=\"solana:{}?label={}\">donation link</a><a href=\"{}\" target=\"_blank\" rel=\"noopener\">explorer</a></div>",
                esc(&a),
                esc(&a),
                esc(&a),
                esc(&crate::app::pct_encode(&format!("{} on IQ", d.name))),
                esc(&ui::solscan_account(&a, &app.settings.cluster))
            ));
            h.push_str(&format!("<p class=\"big\">{} <button class=\"link\" data-a=\"balance\" data-arg=\"{}\">refresh</button></p>", esc(&bal), esc(&a)));
            h.push_str(&crate::qr::svg(&format!("solana:{}", a)).unwrap_or_default());
            if app.owner.is_some() {
                h.push_str(&format!(
                    "<div class=\"row\"><input id=\"fund-{k}\" placeholder=\"0.1\" inputmode=\"decimal\" value=\"{v}\" data-in=\"form\" data-arg=\"fund:{k}\" aria-label=\"Amount in SOL\"><button class=\"btn primary\" data-a=\"fund\" data-arg=\"{k}\">Fund from my wallet</button></div>",
                    k = esc(key),
                    v = esc(app.form.get(&format!("fund:{}", key)).map(|s| s.as_str()).unwrap_or(""))
                ));
            }
            h.push_str(&format!(
                "<details><summary>Withdraw · export key</summary><div class=\"row\"><input placeholder=\"destination (default: your wallet)\" value=\"{v}\" data-in=\"form\" data-arg=\"wd:{k}\" aria-label=\"Withdraw destination\"><button class=\"btn\" data-a=\"withdraw\" data-arg=\"{k}\">Withdraw all</button></div>",
                k = esc(key),
                v = esc(app.form.get(&format!("wd:{}", key)).map(|s| s.as_str()).unwrap_or(""))
            ));
            if app.reveal_key.as_deref() == Some(key) {
                h.push_str(&format!(
                    "<p class=\"warn small\">Anyone with this key controls the database wallet and its funds. It is never stored by this page.</p><pre class=\"secret\">{}</pre><button class=\"btn\" data-a=\"reveal-key\" data-arg=\"{}\">Hide</button>",
                    esc(&kp.export_b58()),
                    esc(key)
                ));
            } else {
                h.push_str(&format!("<button class=\"btn\" data-a=\"reveal-key\" data-arg=\"{}\">Reveal secret key (import into Phantom)</button>", esc(key)));
            }
            h.push_str("</details>");
        }
        (None, None) => {
            h.push_str("<p>Connect your wallet. It's your account: each database gets its own wallet, derived from a signature by yours, so it can be recovered on any device.</p><button class=\"btn primary\" data-a=\"connect-menu\">Connect wallet</button>");
        }
        (Some(_), None) => {
            if let Some(w) = &wallet {
                h.push_str(&format!("<p>Database wallet {} is locked. Unlocking asks your wallet to sign a message (no funds move).</p>", addr(w)));
                h.push_str(&format!("<button class=\"btn primary\" data-a=\"unlock\" data-arg=\"{}\">Unlock</button>", esc(key)));
            } else {
                h.push_str("<p>Create this database's own wallet. It becomes the database's creator on chain (its <em>official</em> signer) and its public donation address. It's derived from a signature by your wallet — the same wallet always gets the same database wallet back.</p>");
                h.push_str(&format!("<button class=\"btn primary\" data-a=\"unlock\" data-arg=\"{}\">Create database wallet</button>", esc(key)));
            }
        }
    }
    h.push_str(&format!(
        "<details><summary>Use a key instead</summary><div class=\"row\"><input type=\"password\" placeholder=\"base58 secret key\" data-in=\"form\" data-arg=\"key:{k}\" aria-label=\"Secret key\" autocomplete=\"off\"><button class=\"btn\" data-a=\"import-key\" data-arg=\"{k}\">Use key</button></div><p class=\"muted small\">For wallets that can't sign messages. Held in memory for this session only.</p></details>",
        k = esc(key)
    ));
    h.push_str("</section>");

    // ---- cost + inscribe card
    h.push_str("<section class=\"card\"><h3>Inscribe</h3>");
    let mut total: u64 = 0;
    let mut lines: Vec<String> = vec![];
    if d.root_sig.is_none() {
        total += 3_000_000;
        lines.push(format!("Create the database{} · ~0.003 SOL rent", if d.lock_creators { " and lock table creation to its wallet" } else { "" }));
    }
    let new_tables = d.tables.iter().filter(|t| t.created.is_none()).count();
    if new_tables > 0 {
        total += new_tables as u64 * 5_000_000;
        lines.push(format!("{} new table(s) · ~0.005 SOL rent each + IQ's table-creation fee (exact amount shown when simulated)", new_tables));
    }
    if d.user_init_sig.is_none() {
        lines.push("First write from a new wallet: one-time IQ account setup · ~0.062 SOL rent".into());
        total += iq::USER_INIT_RENT_ESTIMATE;
    }
    let mut packs_total = 0usize;
    for (t, tb) in d.tables.iter().enumerate() {
        match &plans[t] {
            Ok(p) if !p.is_empty() => {
                let recs: usize = p.iter().map(|x| x.count).sum();
                let raw: usize = p.iter().map(|x| x.raw_bytes).sum();
                let onchain: usize = p.iter().map(|x| x.size).sum();
                packs_total += p.len();
                lines.push(format!(
                    "<b>{}</b>: {} ghost row(s) → {} pack(s), ~{:.0} per pack · {} → {} on chain · {}",
                    esc(&tb.name),
                    recs,
                    p.len(),
                    recs as f64 / p.len() as f64,
                    ui::bytes(raw),
                    ui::bytes(onchain),
                    ui::sol(p.len() as u64 * (iq::FEE_DIRECT_WRITE + iq::TX_FEE))
                ));
            }
            Err(e) => lines.push(format!("<span class=\"warn\"><b>{}</b>: {}</span>", esc(&tb.name), esc(e))),
            _ => {}
        }
    }
    total += packs_total as u64 * (iq::FEE_DIRECT_WRITE + iq::TX_FEE);
    if lines.is_empty() {
        h.push_str("<p class=\"muted\">Nothing to inscribe yet.</p>");
    } else {
        h.push_str("<ul class=\"costs\">");
        for l in &lines {
            h.push_str(&format!("<li>{}</li>", l));
        }
        h.push_str("</ul>");
        h.push_str(&format!("<p>Estimated total: <b>{}</b> <span class=\"muted small\">+ table fees · each write is 0.001 SOL to IQ Labs + 0.000005 network fee</span></p>", ui::sol(total)));
    }
    let running = app.run.as_ref().map(|r| r.busy()).unwrap_or(false);
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"lock-creators\" data-arg=\"{}\" {} {}> Only this database's wallet may create tables in it</label>",
        esc(key),
        if d.lock_creators { "checked" } else { "" },
        if d.root_sig.is_some() { "disabled" } else { "" }
    ));
    let nothing = lines.is_empty();
    h.push_str(&format!(
        "<div class=\"row\"><button class=\"btn primary big\" data-a=\"inscribe\" data-arg=\"{}\" {}>{}</button></div>",
        esc(key),
        if unlocked.is_none() || running || nothing { "disabled" } else { "" },
        if nothing {
            "Up to date".to_string()
        } else if packs_total > 0 {
            format!("Inscribe {} pack(s)", packs_total)
        } else {
            "Inscribe setup".into()
        }
    ));
    if unlocked.is_none() {
        h.push_str("<p class=\"muted small\">Unlock the database wallet to inscribe.</p>");
    }
    if let Some(r) = app.run.as_ref().filter(|r| r.draft == key) {
        let (cls, st) = match &r.state {
            RunState::Preparing => ("", "Preparing…".to_string()),
            RunState::Working(s) => ("", s.clone()),
            RunState::Paused(s) => ("warn", s.clone()),
            RunState::Done => ("good", "Done".into()),
            RunState::Failed(s) => ("bad", s.clone()),
        };
        h.push_str(&format!("<div class=\"run\"><p class=\"{}\"><b>{}</b></p>", cls, esc(&st).replace('\n', "<br>")));
        let done = r.steps.iter().filter(|s| s.sig.is_some()).count();
        if !r.steps.is_empty() {
            h.push_str(&format!(
                "<div class=\"bar\"><span style=\"width:{}%\"></span></div><p class=\"small\">{}/{} steps{}{}</p>",
                done * 100 / r.steps.len(),
                done,
                r.steps.len(),
                r.spent().map(|s| format!(" · spent {}", ui::sol(s))).unwrap_or_default(),
                if r.legacy { " · legacy tx format" } else { " · v1 tx format" }
            ));
        }
        h.push_str("<div class=\"row\">");
        if r.busy() {
            h.push_str("<button class=\"btn\" data-a=\"run-stop\">Stop after this step</button>");
        } else {
            if !matches!(r.state, RunState::Done) {
                h.push_str("<button class=\"btn primary\" data-a=\"run-resume\">Resume</button>");
            }
            h.push_str("<button class=\"btn\" data-a=\"run-close\">Close</button>");
        }
        h.push_str("</div><ol class=\"log\">");
        for (ok, m) in r.log.iter().rev().take(30) {
            h.push_str(&format!("<li class=\"{}\">{}</li>", if *ok { "" } else { "warn" }, esc(m).replace('\n', "<br>")));
        }
        h.push_str("</ol><details><summary>Steps</summary><ol class=\"steps\">");
        for (i, s) in r.steps.iter().enumerate() {
            let mark = if s.sig.is_some() { "✓" } else if i == r.i && r.busy() { "▶" } else { "·" };
            let link = s
                .sig
                .as_ref()
                .map(|sig| format!(" <a href=\"{}\" target=\"_blank\" rel=\"noopener\">{}</a>", esc(&ui::solscan_tx(sig, &app.settings.cluster)), esc(&solana::short(sig))))
                .unwrap_or_default();
            let cost = s.cost.map(|c| format!(" · {}", ui::sol(c))).unwrap_or_default();
            h.push_str(&format!("<li>{} {}{}{}</li>", mark, esc(&r.describe(s, &d.tables)), cost, link));
        }
        h.push_str("</ol></details></div>");
    }
    h.push_str("</section></div>");

    // ---- tables
    h.push_str("<section class=\"card\"><h3>Tables</h3><div class=\"tabs\" role=\"tablist\">");
    for (t, tb) in d.tables.iter().enumerate() {
        h.push_str(&format!(
            "<button role=\"tab\" class=\"{}\" data-a=\"sel-table\" data-arg=\"{}:{}\">{}{}</button>",
            if t == d.sel { "on" } else { "" },
            esc(key),
            t,
            esc(&tb.name),
            if tb.ghosts() > 0 { format!(" <span class=\"pill ghost\">{}</span>", tb.ghosts()) } else { String::new() }
        ));
    }
    h.push_str("</div>");
    h.push_str(&format!(
        "<details {}><summary>New table</summary><div class=\"formgrid\"><label>Name<input placeholder=\"e.g. fasteners\" value=\"{}\" data-in=\"form\" data-arg=\"tname:{k}\" maxlength=\"64\"></label><label>Columns (comma separated, first is the id)<input placeholder=\"part_no, name, material, qty\" value=\"{}\" data-in=\"form\" data-arg=\"tcols:{k}\"></label><label>Who can write<select data-in=\"form\" data-arg=\"topen:{k}\"><option value=\"locked\">Only this database's wallet (locked)</option><option value=\"open\" {}>Anyone — contributions show as unofficial (open)</option></select></label></div><div class=\"row\"><button class=\"btn primary\" data-a=\"add-table\" data-arg=\"{k}\">Add table</button><span class=\"muted small\">Or add a table and import a CSV — its header becomes the columns.</span></div></details>",
        if d.tables.is_empty() { "open" } else { "" },
        esc(app.form.get(&format!("tname:{}", key)).map(|s| s.as_str()).unwrap_or("")),
        esc(app.form.get(&format!("tcols:{}", key)).map(|s| s.as_str()).unwrap_or("")),
        if app.form.get(&format!("topen:{}", key)).map(|v| v == "open").unwrap_or(false) { "selected" } else { "" },
        k = esc(key)
    ));
    if let Some(tb) = d.tables.get(d.sel) {
        let t = d.sel;
        let arg = format!("{}:{}", key, t);
        h.push_str("<div class=\"tablehead\">");
        h.push_str(&format!("<h3>{}</h3>", esc(&tb.name)));
        match &tb.created {
            Some(s) if s == "existing" => h.push_str("<span class=\"pill off\">on chain</span>"),
            Some(s) => h.push_str(&format!(
                "<span class=\"pill off\">on chain</span> <a class=\"small\" href=\"{}\" target=\"_blank\" rel=\"noopener\">{}</a>",
                esc(&ui::solscan_tx(s, &app.settings.cluster)),
                esc(&solana::short(s))
            )),
            None => h.push_str("<span class=\"pill ghost\">ghost</span>"),
        }
        if tb.created.is_some() {
            let tpda = b58(&iq::table_pda(&iq::db_root_pda(d.name.as_bytes()), &iq::seed_bytes(&tb.name)));
            h.push_str(&format!(" <a class=\"small\" href=\"#/t/{}/{}\">view on chain →</a>", esc(&root_pda), esc(&tpda)));
        }
        h.push_str("</div><div class=\"row small\">");
        if tb.created.is_none() {
            h.push_str(&format!(
                "<label class=\"check\"><input type=\"checkbox\" data-in=\"table-open\" data-arg=\"{}\" {}> Open to contributions (unofficial rows)</label>",
                esc(&arg),
                if tb.open { "checked" } else { "" }
            ));
        } else {
            h.push_str(if tb.open { "<span>Open to contributions</span>" } else { "<span>Locked to the database wallet</span>" });
        }
        h.push_str(&format!(
            "<label class=\"check\"><input type=\"checkbox\" data-in=\"table-compress\" data-arg=\"{}\" {}> Compress packs (off = readable/searchable, ~5× more writes)</label>",
            esc(&arg),
            if tb.compress { "checked" } else { "" }
        ));
        if tb.created.is_none() {
            h.push_str(&format!("<button class=\"link danger\" data-a=\"del-table\" data-arg=\"{}\">delete table</button>", esc(&arg)));
        }
        h.push_str("</div>");
        h.push_str(&format!(
            "<p class=\"small muted\">Columns: <span class=\"mono\">{}</span> · id column <b>{}</b>. Every pack stores its own column list, so you can add columns later without breaking older data.</p>",
            esc(&tb.columns.join(", ")),
            esc(tb.columns.get(tb.id_col).map(|s| s.as_str()).unwrap_or("?"))
        ));
        // rows editor
        let total_rows = tb.rows.len();
        let pages = (total_rows + ROWS_PER_PAGE - 1).max(1) / ROWS_PER_PAGE.max(1);
        let pages = pages.max(1);
        let page = d.page.min(pages - 1);
        h.push_str("<div class=\"scroll\"><table class=\"grid edit\"><thead><tr><th>#</th>");
        for (ci, c) in tb.columns.iter().enumerate() {
            h.push_str(&format!("<th>{}{}</th>", esc(c), if ci == tb.id_col { " <span class=\"pill\">id</span>" } else { "" }));
        }
        h.push_str("<th></th></tr></thead><tbody>");
        // pending changes first, then what's already inscribed
        let order: Vec<usize> = (0..tb.rows.len())
            .filter(|&i| tb.rows[i].sig.is_none())
            .chain((0..tb.rows.len()).filter(|&i| tb.rows[i].sig.is_some()))
            .collect();
        for &ri in order.iter().skip(page * ROWS_PER_PAGE).take(ROWS_PER_PAGE) {
            let r = &tb.rows[ri];
            let cls = if r.sig.is_some() { "inscribed" } else if r.deleted { "ghost tomb" } else { "ghost" };
            h.push_str(&format!("<tr class=\"{}\"><td class=\"muted small\">{}</td>", cls, ri + 1));
            for ci in 0..tb.columns.len() {
                let v = r.vals.get(ci).cloned().unwrap_or(Json::Null);
                if r.sig.is_some() || (r.deleted && ci != tb.id_col) {
                    h.push_str(&format!("<td>{}</td>", esc(&v.cell_text())));
                } else {
                    h.push_str(&format!(
                        "<td><input id=\"c-{k}-{t}-{r}-{c}\" value=\"{}\" data-in=\"cell\" data-arg=\"{k}:{t}:{r}:{c}\" aria-label=\"{}\"></td>",
                        esc(&v.cell_text()),
                        esc(&tb.columns[ci]),
                        k = esc(key),
                        t = t,
                        r = ri,
                        c = ci
                    ));
                }
            }
            let action = if r.sig.is_some() {
                format!(
                    "<span class=\"pill off\" title=\"{}\">✓ inscribed</span> <button class=\"link\" data-a=\"tomb-row\" data-arg=\"{}:{}\" title=\"Draft a deletion\">delete</button>",
                    esc(r.sig.as_deref().unwrap_or("")),
                    esc(&arg),
                    ri
                )
            } else if r.deleted {
                format!("<span class=\"pill un\">deletion</span> <button class=\"link\" data-a=\"del-row\" data-arg=\"{}:{}\">undo</button>", esc(&arg), ri)
            } else {
                format!("<button class=\"link danger\" data-a=\"del-row\" data-arg=\"{}:{}\" aria-label=\"Remove ghost row\">remove</button>", esc(&arg), ri)
            };
            h.push_str(&format!("<td>{}</td></tr>", action));
        }
        if tb.rows.is_empty() {
            h.push_str(&format!("<tr><td colspan=\"{}\" class=\"muted center\">No rows yet — add one or import a CSV.</td></tr>", tb.columns.len() + 2));
        }
        h.push_str("</tbody></table></div>");
        h.push_str("<div class=\"row\">");
        h.push_str(&format!("<button class=\"btn\" data-a=\"add-row\" data-arg=\"{}\">+ Row</button>", esc(&arg)));
        if pages > 1 {
            if page > 0 {
                h.push_str(&format!("<button class=\"btn\" data-a=\"ws-page\" data-arg=\"{}:{}\">‹</button>", esc(key), page - 1));
            }
            h.push_str(&format!("<span class=\"small\">page {}/{}</span>", page + 1, pages));
            if page + 1 < pages {
                h.push_str(&format!("<button class=\"btn\" data-a=\"ws-page\" data-arg=\"{}:{}\">›</button>", esc(key), page + 1));
            }
        }
        h.push_str("<span class=\"grow\"></span>");
        if tb.ghosts() > 0 {
            h.push_str(&format!("<button class=\"link danger\" data-a=\"clear-ghosts\" data-arg=\"{}\">discard all ghost rows</button>", esc(&arg)));
        }
        h.push_str("</div>");
        h.push_str(&format!(
            "<details><summary>Import CSV / JSON</summary><textarea rows=\"6\" placeholder=\"Paste CSV with a header row, or a JSON array of objects\" data-in=\"form\" data-arg=\"csv:{a}\" aria-label=\"Import data\">{}</textarea><div class=\"row\"><button class=\"btn primary\" data-a=\"import-csv\" data-arg=\"{a}\">Import as ghost rows</button><label class=\"btn\">Choose file…<input type=\"file\" accept=\".csv,.tsv,.json,text/csv,application/json\" data-file=\"import-file\" data-arg=\"{a}\" hidden></label></div></details>",
            esc(app.form.get(&format!("csv:{}", arg)).map(|s| s.as_str()).unwrap_or("")),
            a = esc(&arg)
        ));
    }
    h.push_str("</section>");
    h.push_str(&format!("<p><button class=\"link danger\" data-a=\"del-draft\" data-arg=\"{}\">Delete this draft from the browser</button> <span class=\"muted small\">(on-chain data is permanent and unaffected)</span></p>", esc(key)));
}

// ---------------------------------------------------------------- settings

fn settings(app: &App, h: &mut String) {
    let s = &app.settings;
    h.push_str("<h1>Settings</h1><div class=\"card formgrid\">");
    h.push_str(&format!(
        "<label>Solana RPC (writes and balances)<input value=\"{}\" data-in=\"set\" data-arg=\"rpc\" spellcheck=\"false\"></label><p class=\"muted small\">The public endpoint is heavily rate-limited and may refuse browser requests. A free key from Helius, QuickNode or Triton works much better.</p>",
        esc(&s.rpc)
    ));
    h.push_str(&format!(
        "<label>IQ gateway (reads)<input value=\"{}\" data-in=\"set\" data-arg=\"gateway\" spellcheck=\"false\"></label>",
        esc(&s.gateway)
    ));
    h.push_str(&format!(
        "<label>Cluster<select data-in=\"set\" data-arg=\"cluster\"><option value=\"mainnet\" {}>mainnet-beta</option><option value=\"devnet\" {}>devnet (testing — the gateway only serves mainnet)</option></select></label>",
        if s.cluster == "mainnet" { "selected" } else { "" },
        if s.cluster == "devnet" { "selected" } else { "" }
    ));
    h.push_str(&format!(
        "<label>Transaction format<select data-in=\"set\" data-arg=\"tx\"><option value=\"auto\" {}>Auto — v1 (4 KB, ~5× bigger packs), fall back to legacy</option><option value=\"v1\" {}>v1 only</option><option value=\"legacy\" {}>Legacy only (1.2 KB)</option></select></label>",
        if s.tx_format == crate::state::TxFormat::Auto { "selected" } else { "" },
        if s.tx_format == crate::state::TxFormat::V1 { "selected" } else { "" },
        if s.tx_format == crate::state::TxFormat::Legacy { "selected" } else { "" }
    ));
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"set\" data-arg=\"simulate\" {}> Simulate every transaction before sending (shows exact costs, catches errors for free)</label>",
        if s.simulate { "checked" } else { "" }
    ));
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"set\" data-arg=\"notify\" {}> Tell the gateway about new rows right away</label>",
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
<p>Each on-chain row holds a <em>pack</em> of many records: <code>{"id": pack-id, "p": "IQT1z…"}</code>. Records are laid out column by column, compressed with a small context-mixing compressor (order 1–5 contexts, a match model and logistic mixing — tighter than gzip or brotli on small tables), then written with a 92-character alphabet that never needs escaping inside JSON. A pack fills one v1 transaction (up to 3,400 bytes of metadata), so a single 0.001 SOL write can carry hundreds of records. Packs carry their own column list, so tables can gain columns later. Newer records replace older ones with the same id; deletions are tombstone records. Uncompressed packs (<code>IQT1j</code>) are available when you want rows searchable by the gateway.</p>
<h3>Wallets</h3>
<p>Your wallet is your account. Each database gets its own wallet: SHA-256 of <code>"iq-tables/db-wallet/v1"</code> followed by your wallet's Ed25519 signature of a fixed message naming the database. Ed25519 signatures are deterministic, so the same wallet always gets the same database wallet back — nothing is stored anywhere. The database wallet creates the database on chain, so its address is the <em>official</em> signer and a public donation address. Rows written by anyone else are shown as <em>unofficial</em>.</p>
<h3>Written in Rust</h3>
<p>Everything — SHA-2, Keccak, Ed25519, Base58, Solana transaction encoding (legacy and v1), the IQ program's instructions, JSON, compression, the UI — is dependency-free Rust compiled to WebAssembly. A small JavaScript file only connects it to the page, the network and your wallet.</p>
</div>"#);
}
