//! The editor, laid out like phpMyAdmin: databases and tables in a tree on
//! the left; for a database the tabs are Structure, SQL, Search, Export,
//! Import, Operations and Save; for a table Browse (a spreadsheet),
//! Structure, SQL, Search, Insert, Export, Import, Operations and Save.
//! Every form builds a SQL statement (see ws_actions.rs) and shows it after
//! running, so the screens and the SQL console follow the same rules.

use crate::app::{App, Load};
use crate::editor::{BaseState, SHEET_PAGE};
use crate::inscribe::RunState;
use crate::iq;
use crate::json::Json;
use crate::schema::{DefVal, Ty};
use crate::sheet::{self, RowState};
use crate::solana::{self, b58};
use crate::sql_exec::Out;
use crate::state::DraftTable;
use crate::ui::{self, esc};
use crate::views::cell_html;
use crate::views_account as va;
use crate::ws_actions::{preset_of, TYPES};

pub const DB_TABS: &[(&str, &str)] = &[
    ("structure", "Structure"),
    ("sql", "SQL"),
    ("search", "Search"),
    ("export", "Export"),
    ("import", "Import"),
    ("operations", "Operations"),
    ("save", "Save"),
];

pub const TABLE_TABS: &[(&str, &str)] = &[
    ("browse", "Browse"),
    ("structure", "Structure"),
    ("sql", "SQL"),
    ("search", "Search"),
    ("insert", "Insert"),
    ("export", "Export"),
    ("import", "Import"),
    ("operations", "Operations"),
    ("save", "Save"),
];

fn form<'a>(app: &'a App, k: &str) -> &'a str {
    app.form.get(k).map(|s| s.as_str()).unwrap_or("")
}

/// Tables people see (not dropped, not IQ Tables' own).
fn visible(app: &App, di: usize) -> Vec<usize> {
    app.drafts[di].tables.iter().enumerate().filter(|(_, t)| !t.dropped && !t.is_system()).map(|(i, _)| i).collect()
}

/// A destructive button: the first click arms it, the second does it.
fn danger(app: &App, action: &str, arg: &str, label: &str, link: bool) -> String {
    let armed = app.ed.confirm.as_deref() == Some(format!("{}:{}", action, arg).as_str());
    let cls = match (link, armed) {
        (true, false) => "link danger",
        (true, true) => "link danger armed",
        (false, false) => "btn danger",
        (false, true) => "btn confirm",
    };
    format!(
        "<button class=\"{}\" data-a=\"{}\" data-arg=\"{}\">{}</button>",
        cls,
        action,
        esc(arg),
        if armed { "Click again to confirm".to_string() } else { esc(label) }
    )
}

fn options(opts: &[(String, String)], cur: &str) -> String {
    opts.iter().map(|(v, l)| format!("<option value=\"{}\"{}>{}</option>", esc(v), if v == cur { " selected" } else { "" }, esc(l))).collect()
}

fn o(v: &str, l: &str) -> (String, String) {
    (v.to_string(), l.to_string())
}

/// A short tag for a column's type in the sheet header.
fn ty_tag(t: &Ty) -> &'static str {
    match t {
        Ty::Any => "",
        Ty::Int(..) => "123",
        Ty::Decimal(_, 0, _) => "123",
        Ty::Decimal(..) => "1.00",
        Ty::Float(..) => "1.5",
        Ty::Bool => "yes/no",
        Ty::Char(_) | Ty::Varchar(_) => "abc",
        Ty::Text(_) => "text",
        Ty::Date => "date",
        Ty::DateTime(_) | Ty::Timestamp(_) => "date+time",
        Ty::Time(_) => "time",
        Ty::Year => "year",
        Ty::Json => "json",
        Ty::Enum(_) => "choice",
        Ty::Set(_) => "choices",
    }
}

fn default_text(d: &Option<DefVal>) -> String {
    match d {
        None => String::new(),
        Some(DefVal::Lit(Json::Null)) => "NULL".into(),
        Some(DefVal::Lit(v)) => v.cell_text(),
        Some(DefVal::Expr(e)) => e.clone(),
    }
}

fn name_of_key(tb: &DraftTable, k: &str) -> String {
    tb.meta.iter().position(|m| m.key == k).map(|i| tb.columns[i].clone()).unwrap_or_else(|| k.to_string())
}

fn id_name(tb: &DraftTable) -> String {
    tb.columns.get(tb.id_col).cloned().unwrap_or_default()
}

/// A cell whose row can't be saved until it's filled.
fn needs_value(tb: &DraftTable, c: usize) -> bool {
    let m = &tb.meta[c];
    !m.auto_inc && (c == tb.id_col || (m.not_null && m.default.is_none()))
}

// ------------------------------------------------------------------- home

pub fn home(app: &mut App, h: &mut String) {
    h.push_str("<div class=\"ed\">");
    sidebar(app, None, h);
    h.push_str("<section class=\"edmain\"><h1>Editor</h1><p class=\"muted\">Make a database, fill its tables like a spreadsheet, and save to the blockchain when you're ready. Nothing costs anything until you save.</p>");
    h.push_str("<div class=\"card\"><h3>New database</h3><div class=\"row\">");
    h.push_str(&format!(
        "<input id=\"newdb\" placeholder=\"name it, e.g. texas-fasteners\" value=\"{}\" data-in=\"form\" data-arg=\"new-db\" data-enter=\"new-db\" maxlength=\"32\" aria-label=\"Database name\">",
        esc(form(app, "new-db"))
    ));
    h.push_str("<button class=\"btn primary\" data-a=\"new-db\">Create</button></div><p class=\"muted small\">The name is permanent once saved, and first come, first served.</p></div>");
    if !app.drafts.is_empty() {
        h.push_str("<h2>Your databases</h2><div class=\"tiles\">");
        for d in &app.drafts {
            let pend = d.ghosts();
            let n = d.tables.iter().filter(|t| !t.dropped && !t.is_system()).count();
            h.push_str(&format!(
                "<a class=\"tile\" href=\"#/ws/{}\"><b>{}</b><span class=\"muted small\">{} table{}</span>{}{}</a>",
                esc(&d.key),
                esc(&d.name),
                n,
                if n == 1 { "" } else { "s" },
                if d.root_sig.is_some() { "<span class=\"pill off\">on the blockchain</span>" } else { "<span class=\"pill ghost\">not saved yet</span>" },
                if pend > 0 { format!(" <span class=\"pill un\">{} unsaved</span>", pend) } else { String::new() }
            ));
        }
        h.push_str("</div>");
    }
    h.push_str("<details><summary>Backup of this browser's drafts</summary><div class=\"row\"><button class=\"btn\" data-a=\"export-ws\">Download drafts file</button><label class=\"btn\">Load drafts file<input type=\"file\" accept=\".json,application/json\" data-file=\"import-ws\" hidden></label></div></details>");
    h.push_str("</section></div>");
}

fn sidebar(app: &mut App, cur: Option<(&str, Option<usize>)>, h: &mut String) {
    h.push_str("<aside class=\"side\" aria-label=\"Databases\"><div class=\"side-h\"><a href=\"#/ws\">Databases</a></div><ul class=\"tree\">");
    let n = app.drafts.len();
    for di in 0..n {
        let key = app.drafts[di].key.clone();
        let on = cur.map(|(k, _)| k == key).unwrap_or(false);
        let pend = app.drafts[di].ghosts();
        h.push_str(&format!(
            "<li><a class=\"db{}\" href=\"#/ws/{}\">🗄 {}</a>{}",
            if on && cur.map(|c| c.1.is_none()).unwrap_or(false) { " on" } else { "" },
            esc(&key),
            esc(&app.drafts[di].name),
            if pend > 0 { "<span class=\"dotp\" title=\"unsaved changes\"></span>" } else { "" }
        ));
        if on {
            h.push_str("<ul>");
            for t in visible(app, di) {
                let tb = &app.drafts[di].tables[t];
                let dirty = tb.ghosts() > 0 || tb.schema_changed() || tb.created.is_none() || tb.meta_changed(app.drafts[di].wallet.as_deref());
                h.push_str(&format!(
                    "<li><a class=\"tb{}\" href=\"#/ws/{}/{}\" title=\"{}\">▦ {}</a>{}</li>",
                    if cur.and_then(|c| c.1) == Some(t) { " on" } else { "" },
                    esc(&key),
                    t,
                    esc(&tb.title),
                    esc(&tb.title),
                    if dirty { "<span class=\"dotp\"></span>" } else { "" }
                ));
            }
            let views = app.views(&key);
            if !views.is_empty() {
                h.push_str("<li class=\"grp\">Views</li>");
                for (v, _) in views {
                    h.push_str(&format!("<li><button class=\"link vw\" data-a=\"view-open\" data-arg=\"{}\">👁 {}</button></li>", esc(&v), esc(&v)));
                }
            }
            h.push_str(&format!(
                "<li class=\"newt\"><input id=\"quick-t\" placeholder=\"+ new table\" value=\"{}\" data-in=\"form\" data-arg=\"tname:{k}\" data-enter=\"add-table\" aria-label=\"New table name\"><button class=\"link\" data-a=\"add-table\" data-arg=\"{k}\">add</button></li>",
                esc(form(app, &format!("tname:{}", key))),
                k = esc(&key)
            ));
            h.push_str("</ul>");
        }
        h.push_str("</li>");
    }
    h.push_str("</ul>");
    h.push_str(&format!(
        "<div class=\"side-new\"><input id=\"side-newdb\" placeholder=\"+ new database\" value=\"{}\" data-in=\"form\" data-arg=\"new-db\" data-enter=\"new-db\" maxlength=\"32\" aria-label=\"New database name\"></div>",
        esc(form(app, "new-db"))
    ));
    h.push_str("</aside>");
}

// ------------------------------------------------------------------ page

pub fn page(app: &mut App, key: &str, h: &mut String) {
    let Some(di) = app.draft_idx(key) else {
        h.push_str("<div class=\"card\">That database isn't in this browser. <a href=\"#/ws\">Back to the editor</a></div>");
        return;
    };
    let vis = visible(app, di);
    let mut t = app.drafts[di].sel;
    if !vis.contains(&t) {
        if let Some(&f) = vis.first() {
            t = f;
            app.drafts[di].sel = f;
        }
    }
    let scope_db = app.ed.scope_db || vis.is_empty();
    let tabs = if scope_db { DB_TABS } else { TABLE_TABS };
    if !tabs.iter().any(|(id, _)| *id == app.ed.tab) {
        app.ed.tab = tabs[0].0.to_string();
    }
    let tab = app.ed.tab.clone();
    h.push_str("<div class=\"ed\">");
    sidebar(app, Some((key, if scope_db { None } else { Some(t) })), h);
    h.push_str("<section class=\"edmain\">");
    let d = app.drafts[di].clone();
    let on_chain = d.root_sig.is_some();
    // breadcrumbs + title
    if scope_db {
        h.push_str(&format!(
            "<p class=\"crumbs\">Database</p><div class=\"edhead\"><h1>🗄 {}</h1>{}</div>",
            esc(&d.name),
            if on_chain { "<span class=\"pill off\">on the blockchain</span>" } else { "<span class=\"pill ghost\">not saved yet</span>" }
        ));
    } else {
        let tb = &d.tables[t];
        h.push_str(&format!(
            "<p class=\"crumbs\">🗄 <a href=\"#/ws/{}\">{}</a> › Table</p><div class=\"edhead\"><h1>▦ {}</h1>{}{}</div>",
            esc(key),
            esc(&d.name),
            esc(&tb.title),
            if tb.created.is_some() { "<span class=\"pill off\">on the blockchain</span>" } else { "<span class=\"pill ghost\">not saved yet</span>" },
            if tb.keys.comment.is_empty() { String::new() } else { format!("<span class=\"muted small\">{}</span>", esc(&tb.keys.comment)) }
        ));
    }
    // someone else already owns this name
    if let Some(Load::Ready(Some(c))) = app.name_checks.get(key) {
        let ours = d.wallet.as_deref() == Some(c.as_str()) || va::wallet_label(app, c).is_some();
        if !ours {
            h.push_str(&format!(
                "<p class=\"warn small\">“{}” already belongs to someone else ({}). You can add rows to its open tables — they'll show as unofficial — but you can't create tables in it or change their structure.</p>",
                esc(&d.name),
                esc(&solana::short(c))
            ));
        }
    }
    // what's waiting to be saved
    let ntables = d.tables.len();
    let mut pend = (0, 0, 0);
    for tt in 0..ntables {
        if d.tables[tt].dropped {
            continue;
        }
        let rows = app.sheet_rows(key, tt);
        let p = sheet::pending(&rows);
        pend = (pend.0 + p.0, pend.1 + p.1, pend.2 + p.2);
    }
    let pend_n = pend.0 + pend.1 + pend.2;
    let wallet = d.wallet.clone();
    let structural = d.tables.iter().filter(|x| (x.created.is_none() && !x.dropped) || (x.created.is_some() && (x.schema_changed() || x.meta_changed(wallet.as_deref())))).count();
    let (cost, _) = app.save_estimate(key);
    h.push_str("<nav class=\"tabs2\" role=\"tablist\">");
    for (id, label) in tabs {
        let badge = if *id == "save" && (pend_n > 0 || structural > 0 || !on_chain) { " <span class=\"dotp\"></span>" } else { "" };
        h.push_str(&format!("<button role=\"tab\" class=\"{}\" data-a=\"ed-tab\" data-arg=\"{}\">{}{}</button>", if tab == *id { "on" } else { "" }, id, label, badge));
    }
    h.push_str("</nav>");
    let running = app.run.as_ref().map(|r| r.busy() && r.draft == key).unwrap_or(false);
    if tab != "save" && (pend_n > 0 || structural > 0 || running) {
        let mut what = vec![];
        if pend_n > 0 {
            what.push(format!("{} row change{}{}", pend_n, if pend_n == 1 { "" } else { "s" }, detail(pend)));
        }
        if structural > 0 {
            what.push(format!("{} table{} with structure changes", structural, if structural == 1 { "" } else { "s" }));
        }
        h.push_str(&format!(
            "<div class=\"pendbar\"><span>{}</span><span class=\"grow\"></span>{}</div>",
            if running { "Saving to the blockchain…".to_string() } else { format!("● Not saved yet: {}", what.join(" · ")) },
            if running {
                "<button class=\"btn\" data-a=\"ed-tab\" data-arg=\"save\">See progress</button>".to_string()
            } else {
                format!(
                    "<button class=\"btn primary\" data-a=\"inscribe\" data-arg=\"{}\">Save to blockchain · ≈{}</button>{}",
                    esc(key),
                    ui::sol(cost),
                    if !scope_db && d.tables[t].ghosts() > 0 { "<button class=\"link\" data-a=\"discard\">discard this table's row changes</button>" } else { "" }
                )
            }
        ));
    }
    // the statement the last button ran, like phpMyAdmin
    if let Some(s) = app.ed.last_sql.clone() {
        h.push_str(&format!(
            "<div class=\"lastsql\"><span class=\"muted\">SQL</span><code>{}</code><button class=\"link small\" data-a=\"sql-open\" data-arg=\"{}\">edit</button></div>",
            esc(&s),
            esc(&s)
        ));
    }
    if !scope_db && d.tables[t].columns.is_empty() && !matches!(tab.as_str(), "browse" | "sql" | "save") {
        let (_, st) = app.sheet_base(key, t);
        h.push_str(match st {
            BaseState::Err(_) => "<div class=\"card bad\">Couldn't read this table from the blockchain.</div>",
            _ => "<div class=\"loading\">Reading this table from the blockchain…</div>",
        });
        h.push_str("</section></div>");
        return;
    }
    match (scope_db, tab.as_str()) {
        (true, "structure") => db_structure(app, key, h),
        (true, "search") => db_search(app, key, h),
        (true, "export") => db_export(app, key, h),
        (true, "import") => import_tab(app, key, None, h),
        (true, "operations") => db_operations(app, key, h),
        (_, "sql") => sql_tab(app, key, if scope_db { None } else { Some(t) }, h),
        (_, "save") => save_tab(app, key, h),
        (false, "browse") => browse(app, key, t, h),
        (false, "structure") => tbl_structure(app, key, t, h),
        (false, "search") => tbl_search(app, key, t, h),
        (false, "insert") => tbl_insert(app, key, t, h),
        (false, "export") => tbl_export(app, key, t, h),
        (false, "import") => import_tab(app, key, Some(t), h),
        (false, "operations") => tbl_operations(app, key, t, h),
        _ => {}
    }
    h.push_str("</section></div>");
}

fn detail((a, c, d): (usize, usize, usize)) -> String {
    let mut v = vec![];
    if a > 0 {
        v.push(format!("{} new", a));
    }
    if c > 0 {
        v.push(format!("{} edited", c));
    }
    if d > 0 {
        v.push(format!("{} deleted", d));
    }
    if v.is_empty() {
        String::new()
    } else {
        format!(" ({})", v.join(", "))
    }
}

/// Query results / messages. `key` lets rows from one table get an Edit link.
fn outputs(app: &mut App, key: &str, outs: &[Out], h: &mut String) {
    for o in outs {
        match o {
            Out::Msg(ok, m) => h.push_str(&format!("<div class=\"sqlmsg {}\">{}</div>", if *ok { "ok" } else { "bad" }, esc(m).replace('\n', "<br>"))),
            Out::Rows { cols, rows, note, title } => {
                // rows of one table, with its ID column: offer Edit
                let edit = if title.is_empty() {
                    None
                } else {
                    app.tbl(key, title).ok().and_then(|t| {
                        let tb = &app.drafts[app.draft_idx(key)?].tables[t];
                        let idn = tb.columns.get(tb.id_col)?;
                        cols.iter().position(|c| c.eq_ignore_ascii_case(idn)).map(|p| (t, p))
                    })
                };
                h.push_str("<div class=\"scroll sqlres\"><table class=\"grid data res\"><thead><tr>");
                if edit.is_some() {
                    h.push_str("<th></th>");
                }
                for c in cols {
                    h.push_str(&format!("<th>{}</th>", esc(c)));
                }
                h.push_str("</tr></thead><tbody>");
                for r in rows {
                    h.push_str("<tr>");
                    if let Some((t, p)) = edit {
                        let id = r.get(p).map(|v| v.cell_text()).unwrap_or_default();
                        h.push_str(&format!("<td class=\"acts\"><button class=\"link small\" data-a=\"sql-edit\" data-arg=\"{}:{}\" title=\"Open this row in the sheet\">✎ edit</button></td>", t, esc(&id)));
                    }
                    for v in r {
                        h.push_str(&format!(
                            "<td class=\"{}\" title=\"{}\">{}</td>",
                            if matches!(v, Json::Num(_)) { "num" } else { "" },
                            esc(&v.cell_text().chars().take(300).collect::<String>()),
                            if v.is_null() { "<i class=\"muted\">NULL</i>".to_string() } else { cell_html(app, v, 80) }
                        ));
                    }
                    h.push_str("</tr>");
                }
                if rows.is_empty() {
                    h.push_str(&format!("<tr><td colspan=\"{}\" class=\"muted center\">No rows.</td></tr>", cols.len().max(1) + edit.map(|_| 1).unwrap_or(0)));
                }
                h.push_str(&format!("</tbody></table></div><p class=\"muted small\">{}</p>", esc(note)));
            }
        }
    }
}

// ================================================================ database

fn db_structure(app: &mut App, key: &str, h: &mut String) {
    let di = app.draft_idx(key).unwrap();
    let d = app.drafts[di].clone();
    let vis = visible(app, di);
    h.push_str("<section class=\"card\"><h3>Tables</h3>");
    if vis.is_empty() {
        h.push_str("<p class=\"muted\">No tables yet — create one below.</p>");
    } else {
        h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Table</th><th>Action</th><th class=\"num\">Rows</th><th class=\"num\">Columns</th><th>Who can add rows</th><th>Status</th></tr></thead><tbody>");
        let mut total = 0usize;
        for &t in &vis {
            let tb = d.tables[t].clone();
            app.ensure_base(key, t);
            let (_, st) = app.sheet_base(key, t);
            let rows = app.sheet_rows(key, t);
            let live = rows.iter().filter(|r| r.state != RowState::Deleted).count();
            total += live;
            let rows_txt = match st {
                BaseState::Loading => "…".to_string(),
                BaseState::Err(_) => "?".to_string(),
                _ => live.to_string(),
            };
            let status = if tb.created.is_none() {
                "<span class=\"pill ghost\">not saved yet</span>".to_string()
            } else if tb.clear {
                "<span class=\"pill un\">will be emptied</span>".to_string()
            } else if tb.checkpoint {
                "<span class=\"pill un\">checkpoint on save</span>".to_string()
            } else if tb.schema_changed() || tb.meta_changed(d.wallet.as_deref()) || tb.ghosts() > 0 {
                "<span class=\"pill un\">unsaved changes</span>".to_string()
            } else {
                "<span class=\"pill off\">saved</span>".to_string()
            };
            h.push_str(&format!(
                "<tr><td><a href=\"#/ws/{k}/{t}\" title=\"{c}\">▦ {n}</a>{cm}</td><td class=\"acts\"><button class=\"link\" data-a=\"goto-tab\" data-arg=\"{t}:browse\">Browse</button> <button class=\"link\" data-a=\"goto-tab\" data-arg=\"{t}:structure\">Structure</button> <button class=\"link\" data-a=\"goto-tab\" data-arg=\"{t}:search\">Search</button> <button class=\"link\" data-a=\"goto-tab\" data-arg=\"{t}:insert\">Insert</button> {e} {x}</td><td class=\"num\">{r}</td><td class=\"num\">{nc}</td><td class=\"small\">{w}</td><td>{s}</td></tr>",
                k = esc(key),
                t = t,
                n = esc(&tb.title),
                c = esc(&tb.columns.join(", ")),
                cm = if tb.keys.comment.is_empty() { String::new() } else { format!("<div class=\"muted small\">{}</div>", esc(&tb.keys.comment)) },
                e = danger(app, "op-truncate", &t.to_string(), "Empty", true),
                x = danger(app, "op-drop", &t.to_string(), "Drop", true),
                r = rows_txt,
                nc = tb.columns.len(),
                w = esc(&crate::ddl::writers_text(&tb)),
                s = status
            ));
        }
        h.push_str(&format!(
            "</tbody><tfoot><tr><th>{} table{}</th><th></th><th class=\"num\">{}</th><th></th><th></th><th></th></tr></tfoot></table></div>",
            vis.len(),
            if vis.len() == 1 { "" } else { "s" },
            total
        ));
    }
    h.push_str("</section>");
    // create a table
    h.push_str(&format!(
        "<section class=\"card\"><h3>Create a table</h3><div class=\"formgrid cols3\"><label>Name<input id=\"ct-name\" placeholder=\"e.g. suppliers\" value=\"{}\" data-in=\"form\" data-arg=\"ct:name\" data-enter=\"db-create-table\" maxlength=\"64\"></label><label>Columns (comma separated)<input id=\"ct-cols\" placeholder=\"name, city, phone\" value=\"{}\" data-in=\"form\" data-arg=\"ct:cols\" data-enter=\"db-create-table\"></label><label>Who can add rows<select data-in=\"form\" data-arg=\"ct:open\">{}</select></label></div><div class=\"row\"><button class=\"btn primary\" data-a=\"db-create-table\">Create table</button><span class=\"muted small\">Every table gets an automatic <b>id</b> number. Set types, rules and keys on its Structure tab — or write <code>CREATE TABLE</code> in SQL.</span></div>",
        esc(form(app, "ct:name")),
        esc(form(app, "ct:cols")),
        options(&[o("locked", "Only you"), o("open", "Anyone (their rows show as unofficial)")], form(app, "ct:open"))
    ));
    h.push_str("<p class=\"small\">Have a spreadsheet? <label class=\"link\">Import a CSV / JSON file as a new table<input type=\"file\" accept=\".csv,.tsv,.txt,.json,text/csv,application/json\" data-file=\"import-csv-new\" hidden></label></p></section>");
    // views
    let views = app.views(key);
    h.push_str("<section class=\"card\"><h3>Views</h3><p class=\"small muted\">A view is a saved SELECT you can query like a table — handy for joins and reports.</p>");
    if !views.is_empty() {
        h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>View</th><th>Query</th><th></th></tr></thead><tbody>");
        for (v, q) in &views {
            h.push_str(&format!(
                "<tr><td><button class=\"link\" data-a=\"view-open\" data-arg=\"{a}\">👁 {n}</button></td><td class=\"small mono\">{q}</td><td class=\"acts\">{x}</td></tr>",
                a = esc(v),
                n = esc(v),
                q = esc(&q.chars().take(160).collect::<String>()),
                x = danger(app, "view-drop", v, "Drop", true)
            ));
        }
        h.push_str("</tbody></table></div>");
    }
    h.push_str(&format!(
        "<details {}><summary>Create a view</summary><div class=\"formgrid\"><label>Name<input id=\"vw-name\" value=\"{}\" data-in=\"form\" data-arg=\"vw:name\" placeholder=\"e.g. big_orders\"></label><label>SELECT<textarea id=\"vw-sql\" rows=\"3\" data-in=\"form\" data-arg=\"vw:sql\" spellcheck=\"false\" placeholder=\"SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id\">{}</textarea></label></div><div class=\"row\"><button class=\"btn\" data-a=\"view-create\">Create view</button></div></details></section>",
        if form(app, "vw:sql").is_empty() { "" } else { "open" },
        esc(form(app, "vw:name")),
        esc(form(app, "vw:sql"))
    ));
}

fn db_search(app: &mut App, key: &str, h: &mut String) {
    h.push_str(&format!(
        "<section class=\"card\"><h3>Search the whole database</h3><div class=\"row\"><input id=\"dbq\" type=\"search\" placeholder=\"words or numbers to find in any table\" value=\"{}\" data-in=\"form\" data-arg=\"dbq\" data-enter=\"dbsearch-run\" aria-label=\"Search text\"><button class=\"btn primary\" data-a=\"dbsearch-run\">Search</button></div>",
        esc(form(app, "dbq"))
    ));
    let res = app.ed.search_out.iter().find(|o| matches!(o, Out::Rows { title, .. } if title == "__dbsearch")).cloned();
    if let Some(Out::Rows { rows, note, .. }) = res {
        h.push_str(&format!("<p class=\"small muted\">{}</p><div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Table</th><th class=\"num\">Matching rows</th><th></th></tr></thead><tbody>", esc(&note)));
        for r in &rows {
            let n = r.get(1).map(|v| v.cell_text()).unwrap_or_default();
            h.push_str(&format!(
                "<tr><td>▦ {}</td><td class=\"num\">{}</td><td>{}</td></tr>",
                esc(&r.first().map(|v| v.cell_text()).unwrap_or_default()),
                esc(&n),
                if n != "0" { format!("<button class=\"link\" data-a=\"dbsearch-browse\" data-arg=\"{}\">Show them</button>", esc(&r.get(2).map(|v| v.cell_text()).unwrap_or_default())) } else { String::new() }
            ));
        }
        h.push_str("</tbody></table></div>");
    }
    h.push_str("</section>");
    let _ = key;
}

fn db_export(app: &mut App, key: &str, h: &mut String) {
    let _ = key;
    h.push_str("<section class=\"card\"><h3>Export the database</h3><p class=\"small muted\">A .sql file that loads into IQ Tables (Import) or into MySQL / MariaDB. Unsaved changes are included.</p><div class=\"row\"><button class=\"btn primary\" data-a=\"export\" data-arg=\"db-sql\">SQL: structure + rows</button><button class=\"btn\" data-a=\"export\" data-arg=\"db-sql-structure\">SQL: structure only</button></div><p class=\"small muted\">For one table as CSV (Excel) or JSON, open the table and use its Export tab.</p></section>");
    h.push_str("<details class=\"adv\"><summary>Backup of every draft in this browser</summary><div class=\"row\"><button class=\"btn\" data-a=\"export-ws\">Download drafts file</button></div></details>");
    let _ = app;
}

fn db_operations(app: &mut App, key: &str, h: &mut String) {
    let di = app.draft_idx(key).unwrap();
    let d = app.drafts[di].clone();
    h.push_str("<section class=\"card\"><h3>Who can create tables</h3>");
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"lock-creators\" data-arg=\"{}\" {} {}> Only this database's wallet may create tables in it</label>",
        esc(key),
        if d.lock_creators { "checked" } else { "" },
        if d.root_sig.is_some() { "disabled" } else { "" }
    ));
    h.push_str(&format!(
        "<p class=\"small muted\">{}</p></section>",
        if d.root_sig.is_some() { "Set when the database was first saved." } else { "Decided when the database is first saved." }
    ));
    h.push_str("<section class=\"card\"><h3>This database's wallet</h3>");
    va::draft_wallet(app, key, h);
    h.push_str(&format!(
        "<p class=\"small muted\">Database address <span class=\"mono\">{}</span></p></section>",
        esc(&b58(&iq::db_root_pda(d.name.as_bytes())))
    ));
    h.push_str(&format!(
        "<section class=\"card\"><h3>Remove from this browser</h3><p class=\"small muted\">Removes the draft from the editor. Anything already on the blockchain stays there and can be opened again from Explore.</p>{}</section>",
        danger(app, "del-draft-confirm", key, "Remove this database from the editor", false)
    ));
}

// ================================================================== table

fn browse(app: &mut App, key: &str, t: usize, h: &mut String) {
    let (_, state) = app.sheet_base(key, t);
    let (rows, order) = app.sheet_view(key, t);
    let di = app.draft_idx(key).unwrap();
    let tb = app.drafts[di].tables[t].clone();
    let nc = tb.columns.len();
    if nc == 0 {
        h.push_str(match state {
            BaseState::Err(_) => "<div class=\"card bad\">Couldn't read this table from the blockchain.</div>",
            _ => "<div class=\"loading\">Reading this table from the blockchain…</div>",
        });
        return;
    }
    let nr = order.len();
    let (sel_r, sel_c) = (app.ed.sel.0.min(nr), app.ed.sel.1.min(nc - 1));
    let (a_r, a_c) = (app.ed.anchor.0.min(nr), app.ed.anchor.1.min(nc - 1));
    let (r0, r1) = (sel_r.min(a_r), sel_r.max(a_r));
    let (c0, c1) = (sel_c.min(a_c), sel_c.max(a_c));
    let arg = format!("{}:{}", key, t);
    // toolbar
    h.push_str("<div class=\"sheet-tools\">");
    h.push_str(&format!("<input type=\"search\" id=\"sheetq\" placeholder=\"Filter rows…\" value=\"{}\" data-live=\"sheet-filter\" aria-label=\"Filter rows\">", esc(&app.ed.filter)));
    h.push_str("<button class=\"btn\" data-a=\"row-add\" title=\"Add a row\">+ Row</button><button class=\"btn\" data-a=\"col-add\" title=\"Add a column\">+ Column</button><button class=\"btn\" data-a=\"row-delete\" title=\"Delete the selected rows (Delete key clears cells)\">Delete row</button>");
    h.push_str(&format!(
        "<label class=\"btn\" title=\"Put a small file (up to ~2 KB) into the selected cell\">📎 File<input type=\"file\" data-fileb64=\"attach-sel\" data-arg=\"{}\" hidden></label>",
        esc(&arg)
    ));
    h.push_str("<button class=\"btn\" data-a=\"sheet-undo\" title=\"Undo (Ctrl+Z)\">↶</button><button class=\"btn\" data-a=\"sheet-undo\" data-arg=\"redo\" title=\"Redo (Ctrl+Y)\">↷</button>");
    h.push_str("<span class=\"grow\"></span>");
    h.push_str(&match &state {
        BaseState::Loading => "<span class=\"muted small\">Reading saved rows from the blockchain…</span>".to_string(),
        BaseState::Err(e) => format!("<span class=\"warn small\" title=\"{}\">Couldn't read saved rows</span> <button class=\"link\" data-a=\"base-reload\">retry</button>", esc(e)),
        BaseState::Chain => "<button class=\"link small\" data-a=\"base-reload\" title=\"Read the saved rows again\">↻ refresh</button>".to_string(),
        BaseState::Local => String::new(),
    });
    h.push_str("</div>");
    // formula bar
    let cur_val = order.get(sel_r).and_then(|&i| rows[i].vals.get(sel_c)).map(|v| show(&tb, sel_c, v)).unwrap_or_default();
    h.push_str(&format!(
        "<div class=\"fbar\"><span class=\"ref\">{}{}</span><input id=\"fbar\" value=\"{}\" data-in=\"sheet-fbar\" aria-label=\"Cell value\" spellcheck=\"false\"{}></div>",
        sheet::col_letter(sel_c),
        sel_r + 1,
        esc(&cur_val),
        if choices(&tb.meta[sel_c].ty).is_some() { " list=\"dl-cell\"" } else { "" }
    ));
    if let Some(ch) = choices(&tb.meta[sel_c].ty) {
        h.push_str("<datalist id=\"dl-cell\">");
        for c in ch {
            h.push_str(&format!("<option value=\"{}\">", esc(&c)));
        }
        h.push_str("</datalist>");
    }
    // grid
    let page = sel_r / SHEET_PAGE;
    let from = page * SHEET_PAGE;
    let to = (from + SHEET_PAGE).min(nr);
    h.push_str(&format!(
        "<div class=\"sheetwrap\" id=\"sheet\" tabindex=\"0\" data-keys=\"sheet\" data-arg=\"{}\" data-paste=\"sheet\" data-focus=\"{}\"><table class=\"sheet\"><thead><tr><th class=\"corner\"></th>",
        esc(&arg),
        if app.ed.editing.is_some() || app.ed.renaming.is_some() { "none" } else { "soft" }
    ));
    for c in 0..nc {
        let name = &tb.columns[c];
        let m = &tb.meta[c];
        let colsel = c >= c0 && c <= c1;
        h.push_str(&format!("<th class=\"colh{}\" data-drag=\"colh\" data-arg=\"{}\">", if colsel { " hi" } else { "" }, c));
        if app.ed.renaming == Some(c) {
            h.push_str(&format!(
                "<input id=\"col-rename\" class=\"inline\" value=\"{}\" data-in=\"col-rename\" data-arg=\"{}\" data-focus=\"force\" aria-label=\"Column name\">",
                esc(name),
                c
            ));
        } else {
            let sort = match app.ed.sort {
                Some((sc, false)) if sc == c => " ▲",
                Some((sc, true)) if sc == c => " ▼",
                _ => "",
            };
            let fk = tb.keys.fks.iter().find(|f| f.cols.len() == 1 && f.cols[0] == m.key);
            let mut tip = format!("{} · {}", name, if m.ty == Ty::Any { "any value".to_string() } else { m.ty.sql() });
            if needs_value(&tb, c) {
                tip.push_str(" · required");
            }
            if m.auto_inc {
                tip.push_str(" · numbered automatically");
            }
            if let Some(f) = fk {
                let rt = app.drafts[di].tables.iter().find(|x| x.name == f.table).map(|x| x.title.clone()).unwrap_or_else(|| f.table.clone());
                tip.push_str(&format!(" · links to {}", rt));
            }
            if !m.comment.is_empty() {
                tip.push_str(&format!(" · {}", m.comment));
            }
            let tag = ty_tag(&m.ty);
            h.push_str(&format!(
                "<div class=\"ch\" title=\"{}\"><span class=\"letter\">{}</span><span class=\"cname\">{}</span>{}{}{}{}<button class=\"cm\" data-a=\"sheet-menu\" data-arg=\"{}\" aria-label=\"Column options\">▾</button></div>",
                esc(&tip),
                sheet::col_letter(c),
                esc(name),
                if c == tb.id_col { "<span class=\"key\" title=\"ID column: identifies each row\">🔑</span>" } else { "" },
                if fk.is_some() { "<span class=\"key\">🔗</span>" } else { "" },
                if tag.is_empty() { String::new() } else { format!("<span class=\"ty\">{}</span>", tag) },
                sort,
                c
            ));
        }
        if app.ed.menu == Some(c) {
            h.push_str("<div class=\"colmenu\">");
            h.push_str(&format!("<button data-a=\"sheet-sort\" data-arg=\"{c}:asc\">Sort A → Z</button><button data-a=\"sheet-sort\" data-arg=\"{c}:desc\">Sort Z → A</button><hr>", c = c));
            h.push_str(&format!("<button data-a=\"col-edit-go\" data-arg=\"{}\">Type, default & rules…</button>", c));
            h.push_str(&format!("<button data-a=\"col-insert\" data-arg=\"{c}:left\">Insert column left</button><button data-a=\"col-insert\" data-arg=\"{c}:right\">Insert column right</button>", c = c));
            h.push_str(&format!("<button data-a=\"col-rename-start\" data-arg=\"{}\">Rename</button>", c));
            if c > 0 {
                h.push_str(&format!("<button data-a=\"col-move\" data-arg=\"{}:{}\">Move left</button>", c, c - 1));
            }
            if c + 1 < nc {
                h.push_str(&format!("<button data-a=\"col-move\" data-arg=\"{}:{}\">Move right</button>", c, c + 1));
            }
            if c != tb.id_col {
                h.push_str(&format!("<button data-a=\"col-id\" data-arg=\"{}\">Use as ID column</button>", c));
                h.push_str(&format!("<hr><button class=\"danger\" data-a=\"col-delete\" data-arg=\"{}\">Delete column</button>", c));
            }
            h.push_str("</div>");
        }
        h.push_str("</th>");
    }
    h.push_str("</tr></thead><tbody>");
    let editing = app.ed.editing.clone();
    for r in from..=to {
        let row = order.get(r).map(|&i| &rows[i]);
        let blank = row.is_none();
        if blank && r != nr {
            continue;
        }
        let cls = row.map(|x| App::row_state_class(x.state)).unwrap_or("blank");
        let rowsel = r >= r0 && r <= r1;
        let pending = row.map(|x| matches!(x.state, RowState::New | RowState::Changed)).unwrap_or(false);
        h.push_str(&format!(
            "<tr class=\"{}\"><th class=\"rowh{}\" data-drag=\"rowh\" data-arg=\"{}\" title=\"{}\">{}</th>",
            cls,
            if rowsel { " hi" } else { "" },
            r,
            match row.map(|x| x.state) {
                Some(RowState::New) => "New row — not saved yet",
                Some(RowState::Changed) => "Edited — not saved yet",
                Some(RowState::Deleted) => "Will be deleted when you save",
                Some(RowState::Saved) => "Saved",
                None => "Type here to add a row",
            },
            if blank { "+".to_string() } else { (r + 1).to_string() }
        ));
        for c in 0..nc {
            let v = row.and_then(|x| x.vals.get(c)).cloned().unwrap_or(Json::Null);
            let is_sel = r == sel_r && c == sel_c;
            let in_rng = rowsel && c >= c0 && c <= c1;
            let chg = row.map(|x| x.changed(c)).unwrap_or(false);
            let req = pending && v.is_null() && needs_value(&tb, c);
            let mut cls = String::new();
            if is_sel {
                cls.push_str(" sel");
            } else if in_rng {
                cls.push_str(" rng");
            }
            if chg {
                cls.push_str(" chg");
            }
            if req {
                cls.push_str(" req");
            }
            if matches!(v, Json::Num(_)) {
                cls.push_str(" num");
            }
            if is_sel && editing.is_some() {
                h.push_str(&format!(
                    "<td class=\"{} editing\"><input id=\"cell-editor\" data-editor=\"1\" data-keys=\"editor\" data-arg=\"{}:{}\" data-focus=\"force\" value=\"{}\" spellcheck=\"false\" aria-label=\"Edit cell\"{}></td>",
                    cls.trim(),
                    r,
                    c,
                    esc(editing.as_deref().unwrap_or("")),
                    if choices(&tb.meta[c].ty).is_some() { " list=\"dl-cell\"" } else { "" }
                ));
            } else {
                let title = match (chg, row.and_then(|x| x.base.as_ref()).and_then(|b| b.get(c))) {
                    (true, Some(old)) => format!(" title=\"was: {}\"", esc(&old.cell_text())),
                    _ if req => " title=\"Needs a value before this row can be saved\"".to_string(),
                    _ => String::new(),
                };
                let body = if tb.meta[c].ty == Ty::Bool { esc(&tb.meta[c].ty.show(&v)) } else { cell_html(app, &v, 80) };
                h.push_str(&format!("<td class=\"{}\" data-drag=\"scell\" data-arg=\"{}:{}\"{}>{}</td>", cls.trim(), r, c, title, body));
            }
        }
        h.push_str("</tr>");
    }
    h.push_str("</tbody></table></div>");
    // status bar
    let mut nums = vec![];
    let mut count = 0;
    for r in r0..=r1.min(nr.saturating_sub(1)) {
        if let Some(&i) = order.get(r) {
            for c in c0..=c1 {
                if let Some(v) = rows[i].vals.get(c) {
                    if !v.is_null() && !v.cell_text().is_empty() {
                        count += 1;
                        if let Some(f) = crate::sql::eval::num(v) {
                            nums.push(f);
                        }
                    }
                }
            }
        }
    }
    let pages = (nr + 1 + SHEET_PAGE - 1) / SHEET_PAGE;
    h.push_str("<div class=\"statusbar\">");
    h.push_str(&format!("<span>{} row(s){}</span>", nr, if app.ed.filter.is_empty() { String::new() } else { " match".into() }));
    if count > 1 {
        h.push_str(&format!("<span>Count {}</span>", count));
    }
    if nums.len() > 1 {
        let sum: f64 = nums.iter().sum();
        h.push_str(&format!("<span>Sum {}</span><span>Average {}</span>", crate::sql::eval::fmt_num(sum).cell_text(), crate::sql::eval::fmt_num(sum / nums.len() as f64).cell_text()));
    }
    if pages > 1 {
        h.push_str(&format!("<span>Page {} of {}", page + 1, pages));
        if page > 0 {
            h.push_str(&format!(" <button class=\"link\" data-a=\"sheet-goto\" data-arg=\"{}\">‹ prev</button>", (page - 1) * SHEET_PAGE));
        }
        if page + 1 < pages {
            h.push_str(&format!(" <button class=\"link\" data-a=\"sheet-goto\" data-arg=\"{}\">next ›</button>", (page + 1) * SHEET_PAGE));
        }
        h.push_str("</span>");
    }
    if let Some((cell, msg)) = &app.attach_status {
        if cell.starts_with(&format!("{}:", arg)) {
            h.push_str(&format!("<span class=\"ghosttext\">📎 {}</span>", esc(msg)));
        }
    }
    h.push_str("<span class=\"grow\"></span><span class=\"legend\"><i class=\"lg new\"></i>new <i class=\"lg chg\"></i>edited <i class=\"lg del\"></i>deleted <i class=\"lg req\"></i>needs a value</span></div>");
    h.push_str("<p class=\"muted small tip\">Click a cell and type. Enter/Tab move, arrows navigate, Shift extends the selection, Ctrl+C / Ctrl+V copy and paste from Excel or Google Sheets, Delete clears, Ctrl+Z undoes. A column's ▾ menu sorts, sets its type and changes columns.</p>");
}

/// What a cell shows / starts editing with.
fn show(tb: &DraftTable, c: usize, v: &Json) -> String {
    tb.meta.get(c).map(|m| m.ty.show(v)).unwrap_or_else(|| v.cell_text())
}

/// Suggestions for a column's cells.
fn choices(t: &Ty) -> Option<Vec<String>> {
    match t {
        Ty::Enum(v) | Ty::Set(v) => Some(v.clone()),
        Ty::Bool => Some(vec!["Yes".into(), "No".into()]),
        _ => None,
    }
}

// -------------------------------------------------------------- structure

fn tbl_structure(app: &mut App, key: &str, t: usize, h: &mut String) {
    let di = app.draft_idx(key).unwrap();
    let d = app.drafts[di].clone();
    let tb = d.tables[t].clone();
    let saved = tb.created.is_some();
    if let Some(c) = app.ed.col_edit {
        col_form(app, &tb, if c < tb.columns.len() { Some(c) } else { None }, h);
    }
    // columns
    h.push_str("<section class=\"card\"><div class=\"tablehead\"><h3 class=\"grow\">Columns</h3><button class=\"btn small\" data-a=\"col-edit\" data-arg=\"new\">+ Add column</button></div>");
    h.push_str("<div class=\"scroll\"><table class=\"grid struct\"><thead><tr><th>#</th><th>Name</th><th>Type</th><th>Empty allowed</th><th>Default</th><th>Extra</th><th>Action</th></tr></thead><tbody>");
    for (c, name) in tb.columns.iter().enumerate() {
        let m = &tb.meta[c];
        let mut marks = String::new();
        if c == tb.id_col {
            marks.push_str(" <span class=\"pill iqt\" title=\"Primary key: identifies each row\">🔑 primary</span>");
        }
        for ix in tb.keys.indexes.iter().filter(|ix| ix.cols.first() == Some(&m.key)) {
            marks.push_str(&format!(" <span class=\"pill\" title=\"{}\">{}</span>", esc(&ix.name), if ix.unique { "unique" } else { "index" }));
        }
        for f in tb.keys.fks.iter().filter(|f| f.cols.contains(&m.key)) {
            let rt = d.tables.iter().find(|x| x.name == f.table).map(|x| x.title.clone()).unwrap_or_else(|| f.table.clone());
            marks.push_str(&format!(" <span class=\"pill\" title=\"{}\">🔗 {}</span>", esc(&f.name), esc(&rt)));
        }
        let mut extra = vec![];
        if m.auto_inc {
            extra.push("AUTO_INCREMENT".to_string());
        }
        if m.on_update_now {
            extra.push("ON UPDATE CURRENT_TIMESTAMP".to_string());
        }
        h.push_str(&format!(
            "<tr><td class=\"muted\">{}</td><td><b>{}</b>{}{}</td><td><span title=\"{}\">{}</span><div class=\"muted small mono\">{}</div></td><td>{}</td><td class=\"small\">{}</td><td class=\"small\">{}</td><td class=\"acts\">",
            c + 1,
            esc(name),
            marks,
            if m.comment.is_empty() { String::new() } else { format!("<div class=\"muted small\">{}</div>", esc(&m.comment)) },
            esc(&m.ty.sql()),
            esc(&m.ty.friendly()),
            esc(&crate::ddl::type_sql(m, false)),
            if m.not_null || c == tb.id_col { "No" } else { "Yes" },
            esc(&default_text(&m.default)),
            esc(&extra.join(" "))
        ));
        h.push_str(&format!("<button class=\"link\" data-a=\"col-edit\" data-arg=\"{c}\">Change</button> ", c = c));
        if c != tb.id_col {
            h.push_str(&danger(app, "col-drop", &c.to_string(), "Drop", true));
            h.push_str(&format!(" <button class=\"link\" data-a=\"col-pk\" data-arg=\"{c}\" title=\"Make this the column that identifies rows\">Primary</button>", c = c));
        }
        h.push_str(&format!(" <button class=\"link\" data-a=\"col-unique\" data-arg=\"{c}\" title=\"No two rows may share a value\">Unique</button> <button class=\"link\" data-a=\"col-index\" data-arg=\"{c}\">Index</button>", c = c));
        if c > 0 {
            h.push_str(&format!(" <button class=\"link\" data-a=\"col-move\" data-arg=\"{}:{}\" aria-label=\"Move up\">↑</button>", c, c - 1));
        }
        if c + 1 < tb.columns.len() {
            h.push_str(&format!(" <button class=\"link\" data-a=\"col-move\" data-arg=\"{}:{}\" aria-label=\"Move down\">↓</button>", c, c + 1));
        }
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table></div>");
    if saved {
        h.push_str("<p class=\"muted small\">Structure changes (types, names, keys, rules) are saved as one small write when you press Save. Rows already on the blockchain aren't rewritten — they're read through the new structure.</p>");
    }
    h.push_str("</section>");
    // indexes
    h.push_str("<section class=\"card\"><h3>Keys</h3><div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Name</th><th>Kind</th><th>Columns</th><th></th></tr></thead><tbody>");
    h.push_str(&format!("<tr><td>PRIMARY</td><td>Primary</td><td>{}</td><td class=\"muted small\">identifies rows</td></tr>", esc(&id_name(&tb))));
    for ix in &tb.keys.indexes {
        h.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"acts\">{}</td></tr>",
            esc(&ix.name),
            if ix.unique { "Unique" } else { "Index" },
            esc(&ix.cols.iter().map(|k| name_of_key(&tb, k)).collect::<Vec<_>>().join(", ")),
            danger(app, "idx-drop", &ix.name, "Drop", true)
        ));
    }
    h.push_str("</tbody></table></div>");
    h.push_str(&format!("<details><summary>Add a key on one or more columns</summary><div class=\"row\"><select data-in=\"form\" data-arg=\"ix:type\" aria-label=\"Key kind\">{}</select></div><div class=\"checks\">", options(&[o("unique", "Unique — no duplicates allowed"), o("index", "Index — just for lookups")], form(app, "ix:type"))));
    for c in &tb.columns {
        h.push_str(&format!(
            "<label class=\"check\"><input type=\"checkbox\" data-in=\"form\" data-arg=\"ix:c:{}\" {}> {}</label>",
            esc(c),
            if form(app, &format!("ix:c:{}", c)) == "true" { "checked" } else { "" },
            esc(c)
        ));
    }
    h.push_str("</div><div class=\"row\"><button class=\"btn\" data-a=\"idx-add\">Add key</button></div></details></section>");
    // relations
    let vis = visible(app, di);
    h.push_str("<section class=\"card\"><h3>Relations</h3><p class=\"small muted\">A relation (foreign key) makes a column point at rows of another table: values must exist there, and deleting those rows can be blocked, cascade or clear the link.</p>");
    if !tb.keys.fks.is_empty() {
        h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Name</th><th>Column</th><th>Points to</th><th>On delete</th><th>On update</th><th></th></tr></thead><tbody>");
        for f in &tb.keys.fks {
            let rt = d.tables.iter().find(|x| x.name == f.table);
            h.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}.{}</td><td>{}</td><td>{}</td><td class=\"acts\">{}</td></tr>",
                esc(&f.name),
                esc(&f.cols.iter().map(|k| name_of_key(&tb, k)).collect::<Vec<_>>().join(", ")),
                esc(&rt.map(|x| x.title.clone()).unwrap_or_else(|| f.table.clone())),
                esc(&f.ref_cols.iter().map(|k| rt.map(|x| name_of_key(x, k)).unwrap_or_else(|| k.clone())).collect::<Vec<_>>().join(", ")),
                f.on_delete.sql(),
                f.on_update.sql(),
                danger(app, "fk-drop", &f.name, "Drop", true)
            ));
        }
        h.push_str("</tbody></table></div>");
    }
    let mut cols = vec![o("", "— column —")];
    cols.extend(tb.columns.iter().map(|c| o(c, c)));
    let mut targets = vec![o("", "— points to —")];
    for &x in &vis {
        let xt = &d.tables[x];
        if xt.columns.is_empty() {
            continue;
        }
        targets.push((xt.title.clone(), format!("{} (its {})", xt.title, id_name(xt))));
        for ix in xt.keys.indexes.iter().filter(|ix| ix.unique && ix.cols.len() == 1) {
            let cn = name_of_key(xt, &ix.cols[0]);
            targets.push((format!("{}\t{}", xt.title, cn), format!("{}.{}", xt.title, cn)));
        }
    }
    h.push_str(&format!(
        "<div class=\"row\"><select data-in=\"form\" data-arg=\"fk:col\" aria-label=\"Column\">{}</select><span>→</span><select data-in=\"form\" data-arg=\"fk:target\" aria-label=\"Points to\">{}</select><select data-in=\"form\" data-arg=\"fk:del\" aria-label=\"When the row pointed to is deleted\">{}</select><button class=\"btn\" data-a=\"fk-add\">Add relation</button></div></section>",
        options(&cols, form(app, "fk:col")),
        options(&targets, form(app, "fk:target")),
        options(&[o("", "deleting it is blocked"), o("cascade", "deleting it deletes these rows"), o("setnull", "deleting it clears the link")], form(app, "fk:del"))
    ));
    // checks
    h.push_str("<section class=\"card\"><h3>Rules</h3><p class=\"small muted\">Conditions every row must meet, written like a WHERE clause — e.g. <code>qty &gt;= 0</code> or <code>end_date &gt; start_date</code>.</p>");
    if !tb.keys.checks.is_empty() {
        h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Name</th><th>Rule</th><th></th></tr></thead><tbody>");
        for ck in &tb.keys.checks {
            h.push_str(&format!("<tr><td>{}</td><td class=\"mono small\">{}</td><td class=\"acts\">{}</td></tr>", esc(&ck.name), esc(&ck.expr), danger(app, "chk-drop", &ck.name, "Drop", true)));
        }
        h.push_str("</tbody></table></div>");
    }
    h.push_str(&format!(
        "<div class=\"row\"><input id=\"ck-expr\" placeholder=\"price > 0\" value=\"{}\" data-in=\"form\" data-arg=\"ck:expr\" data-enter=\"chk-add\" aria-label=\"Rule\" spellcheck=\"false\"><button class=\"btn\" data-a=\"chk-add\">Add rule</button></div></section>",
        esc(form(app, "ck:expr"))
    ));
    let create = app.create_sql(key, &tb);
    h.push_str(&format!("<details class=\"adv\"><summary>SHOW CREATE TABLE</summary><pre class=\"wrap\">{};</pre></details>", esc(&create)));
}

fn col_form(app: &mut App, tb: &DraftTable, c: Option<usize>, h: &mut String) {
    let f = |k: &str| app.form.get(&format!("ce:{}", k)).cloned().unwrap_or_default();
    let ty = { let t = f("type"); if t.is_empty() { "text".to_string() } else { t } };
    h.push_str(&format!(
        "<section class=\"card colform\"><h3>{}</h3><div class=\"formgrid cols3\">",
        match c {
            Some(c) => format!("Change column <code>{}</code>", esc(&tb.columns[c])),
            None => "Add a column".into(),
        }
    ));
    h.push_str(&format!("<label>Name<input id=\"ce-name\" value=\"{}\" data-in=\"form\" data-arg=\"ce:name\" data-enter=\"col-save\" maxlength=\"64\" data-focus=\"{}\"></label>", esc(&f("name")), if c.is_none() && f("name").is_empty() { "force" } else { "soft" }));
    let mut types: Vec<(String, String)> = TYPES.iter().map(|(v, l)| o(v, l)).collect();
    if let Some(c) = c {
        if preset_of(&tb.meta[c].ty).0 == "custom" {
            types.last_mut().unwrap().1 = format!("Other: {}", tb.meta[c].ty.sql());
        }
    }
    h.push_str(&format!("<label>Type<select id=\"ce-type\" data-in=\"form\" data-arg=\"ce:type\">{}</select></label>", options(&types, &ty)));
    h.push_str(&format!(
        "<label>Length / choices<input id=\"ce-param\" value=\"{}\" data-in=\"form\" data-arg=\"ce:param\" data-enter=\"col-save\" placeholder=\"see below\"></label>",
        esc(&f("param"))
    ));
    h.push_str(&format!(
        "<label>Default<input id=\"ce-default\" value=\"{}\" data-in=\"form\" data-arg=\"ce:default\" data-enter=\"col-save\" placeholder=\"none\"></label>",
        esc(&f("default"))
    ));
    h.push_str(&format!("<label>Comment<input id=\"ce-comment\" value=\"{}\" data-in=\"form\" data-arg=\"ce:comment\" data-enter=\"col-save\"></label>", esc(&f("comment"))));
    if c.is_none() {
        let mut places = vec![o("end", "At the end"), o("first", "At the start")];
        places.extend(tb.columns.iter().map(|x| (x.clone(), format!("After {}", x))));
        h.push_str(&format!("<label>Position<select data-in=\"form\" data-arg=\"ce:place\">{}</select></label>", options(&places, &f("place"))));
    }
    h.push_str("</div><div class=\"row\">");
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"form\" data-arg=\"ce:null\" {}> Can be left empty (NULL)</label>",
        if f("null") != "false" { "checked" } else { "" }
    ));
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"form\" data-arg=\"ce:ai\" {}> Number automatically (AUTO_INCREMENT)</label>",
        if f("ai") == "true" { "checked" } else { "" }
    ));
    h.push_str("</div>");
    h.push_str("<p class=\"small muted\"><b>Length / choices:</b> Text → longest allowed (255) · Number with decimals → digits after the point (2) · Choice list → the choices, comma separated · Other SQL type → e.g. <code>SMALLINT UNSIGNED</code>, <code>DECIMAL(10,4)</code>, <code>SET('a','b')</code>. <b>Default:</b> a value, <code>now</code> for the current time, or <code>=expression</code> such as <code>=UUID()</code>.</p>");
    if let Some(c) = c {
        if tb.created.is_some() && tb.meta[c].ty != Ty::Any {
            h.push_str("<p class=\"small muted\">Changing the type checks every row first; values that don't fit stop the change and are listed.</p>");
        }
    }
    h.push_str("<div class=\"row\"><button class=\"btn primary\" data-a=\"col-save\">Save column</button><button class=\"btn\" data-a=\"col-edit-cancel\">Cancel</button></div></section>");
}

// ----------------------------------------------------------------- search

const OPS: &[(&str, &str)] = &[
    ("", "contains"),
    ("eq", "="),
    ("ne", "≠"),
    ("lt", "<"),
    ("le", "≤"),
    ("gt", ">"),
    ("ge", "≥"),
    ("starts", "starts with"),
    ("notlike", "doesn't contain"),
    ("in", "is one of (a, b, …)"),
    ("between", "between (a, b)"),
    ("regexp", "matches pattern (REGEXP)"),
    ("null", "is empty (NULL)"),
    ("notnull", "is not empty"),
];

fn tbl_search(app: &mut App, key: &str, t: usize, h: &mut String) {
    let di = app.draft_idx(key).unwrap();
    let tb = app.drafts[di].tables[t].clone();
    let ops: Vec<(String, String)> = OPS.iter().map(|(v, l)| o(v, l)).collect();
    h.push_str("<section class=\"card\"><h3>Find rows</h3><p class=\"small muted\">Fill in what you know; empty lines are ignored.</p><div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Column</th><th>Type</th><th>Condition</th><th>Value</th></tr></thead><tbody>");
    for (ci, c) in tb.columns.iter().enumerate() {
        let m = &tb.meta[ci];
        h.push_str(&format!(
            "<tr><td><b>{n}</b></td><td class=\"small muted\">{ty}</td><td><select data-in=\"form\" data-arg=\"sq:op:{a}\" aria-label=\"Condition for {n}\">{ops}</select></td><td><input id=\"sq-{a}\" value=\"{v}\" data-in=\"form\" data-arg=\"sq:v:{a}\" data-enter=\"search-run\" aria-label=\"Value for {n}\"{list}></td></tr>",
            n = esc(c),
            a = esc(c),
            ty = esc(&m.ty.friendly()),
            ops = options(&ops, form(app, &format!("sq:op:{}", c))),
            v = esc(form(app, &format!("sq:v:{}", c))),
            list = if let Ty::Enum(_) = m.ty { format!(" list=\"dl-sq-{}\"", esc(c)) } else { String::new() }
        ));
        if let Ty::Enum(v) = &m.ty {
            h.push_str(&format!("<datalist id=\"dl-sq-{}\">{}</datalist>", esc(c), v.iter().map(|x| format!("<option value=\"{}\">", esc(x))).collect::<String>()));
        }
    }
    h.push_str("</tbody></table></div>");
    h.push_str(&format!(
        "<div class=\"row\"><label class=\"check\"><input type=\"checkbox\" data-in=\"form\" data-arg=\"sq:any\" {}> Match any line (OR) instead of all (AND)</label><span class=\"grow\"></span><button class=\"btn\" data-a=\"search-clear\">Clear</button><button class=\"btn primary\" data-a=\"search-run\">Search</button></div></section>",
        if form(app, "sq:any") == "true" { "checked" } else { "" }
    ));
    let res: Vec<Out> = app.ed.search_out.iter().filter(|o| !matches!(o, Out::Rows { title, .. } if title == "__dbsearch")).cloned().collect();
    if !res.is_empty() {
        h.push_str("<section class=\"card\"><h3>Results</h3>");
        outputs(app, key, &res, h);
        h.push_str("</section>");
    }
    // find and replace
    let mut cols = vec![o("", "— column —")];
    cols.extend(tb.columns.iter().zip(&tb.meta).filter(|(_, m)| m.ty.is_text() || m.ty == Ty::Any).map(|(c, _)| o(c, c)));
    h.push_str(&format!(
        "<details class=\"card\"><summary>Find and replace</summary><div class=\"row\"><select data-in=\"form\" data-arg=\"rp:col\" aria-label=\"Column\">{}</select><input id=\"rp-find\" placeholder=\"find\" value=\"{}\" data-in=\"form\" data-arg=\"rp:find\" aria-label=\"Find\"><input id=\"rp-with\" placeholder=\"replace with\" value=\"{}\" data-in=\"form\" data-arg=\"rp:with\" aria-label=\"Replace with\"><button class=\"btn\" data-a=\"replace-run\">Replace all</button></div><p class=\"small muted\">Changes every matching row (case-sensitive). Undo in the sheet takes it back; nothing is saved until you press Save.</p></details>",
        options(&cols, form(app, "rp:col")),
        esc(form(app, "rp:find")),
        esc(form(app, "rp:with"))
    ));
}

// ----------------------------------------------------------------- insert

fn tbl_insert(app: &mut App, key: &str, t: usize, h: &mut String) {
    let di = app.draft_idx(key).unwrap();
    let tb = app.drafts[di].tables[t].clone();
    h.push_str("<section class=\"card\"><h3>Add a row</h3><div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Column</th><th>Type</th><th>Value</th></tr></thead><tbody>");
    for (c, name) in tb.columns.iter().enumerate() {
        let m = &tb.meta[c];
        let k = format!("in:{}", c);
        let v = form(app, &k).to_string();
        let ph = if m.auto_inc {
            "automatic".to_string()
        } else if let Some(d) = &m.default {
            format!("default: {}", match d { DefVal::Lit(v) => v.cell_text(), DefVal::Expr(e) => e.clone() })
        } else if needs_value(&tb, c) {
            "required".to_string()
        } else {
            String::new()
        };
        let input = match &m.ty {
            Ty::Bool => format!("<select data-in=\"form\" data-arg=\"{}\" aria-label=\"{}\">{}</select>", k, esc(name), options(&[o("", if ph.is_empty() { "—" } else { &ph }), o("true", "Yes"), o("false", "No")], &v)),
            Ty::Enum(opts) => {
                let mut ov = vec![o("", if ph.is_empty() { "—" } else { &ph })];
                ov.extend(opts.iter().map(|x| o(x, x)));
                format!("<select data-in=\"form\" data-arg=\"{}\" aria-label=\"{}\">{}</select>", k, esc(name), options(&ov, &v))
            }
            Ty::Text(_) | Ty::Json => format!("<textarea rows=\"2\" data-in=\"form\" data-arg=\"{}\" placeholder=\"{}\" aria-label=\"{}\">{}</textarea>", k, esc(&ph), esc(name), esc(&v)),
            ty => {
                let (typ, extra) = match ty {
                    Ty::Int(..) | Ty::Decimal(..) | Ty::Float(..) | Ty::Year => ("number", " step=\"any\""),
                    Ty::Date => ("date", ""),
                    Ty::DateTime(_) | Ty::Timestamp(_) => ("datetime-local", " step=\"1\""),
                    Ty::Time(_) => ("time", " step=\"1\""),
                    _ => ("text", ""),
                };
                format!(
                    "<input id=\"in-{c}\" type=\"{typ}\"{extra} value=\"{v}\" data-in=\"form\" data-arg=\"{k}\" data-enter=\"insert-row\" placeholder=\"{ph}\" aria-label=\"{n}\"{ml}>",
                    c = c,
                    typ = typ,
                    extra = extra,
                    v = esc(&v),
                    k = k,
                    ph = esc(&ph),
                    n = esc(name),
                    ml = match ty { Ty::Varchar(n) | Ty::Char(n) => format!(" maxlength=\"{}\"", n), _ => String::new() }
                )
            }
        };
        h.push_str(&format!(
            "<tr><td><b>{}</b>{}</td><td class=\"small muted\">{}</td><td class=\"inval\">{}</td></tr>",
            esc(name),
            if c == tb.id_col { " 🔑" } else { "" },
            esc(&m.ty.friendly()),
            input
        ));
    }
    h.push_str("</tbody></table></div><div class=\"row\"><button class=\"btn primary\" data-a=\"insert-row\">Insert</button><span class=\"muted small\">The row goes into the sheet as a new, unsaved row. Save when you're ready.</span></div></section>");
}

// ---------------------------------------------------------- import/export

fn tbl_export(app: &mut App, key: &str, t: usize, h: &mut String) {
    let _ = (app, key, t);
    h.push_str("<section class=\"card\"><h3>Export this table</h3><p class=\"small muted\">Everything in the sheet, including unsaved changes.</p><div class=\"row\"><button class=\"btn primary\" data-a=\"export\" data-arg=\"csv\">CSV (Excel, Google Sheets)</button><button class=\"btn\" data-a=\"export\" data-arg=\"json\">JSON</button></div><div class=\"row\"><button class=\"btn\" data-a=\"export\" data-arg=\"sql\">SQL: structure + rows</button><button class=\"btn\" data-a=\"export\" data-arg=\"sql-structure\">SQL: structure only</button><button class=\"btn\" data-a=\"export\" data-arg=\"sql-data\">SQL: rows only (INSERTs)</button></div></section>");
}

fn import_tab(app: &mut App, key: &str, t: Option<usize>, h: &mut String) {
    if let Some(t) = t {
        let arg = format!("{}:{}", key, t);
        h.push_str("<section class=\"card\"><h3>Import rows (CSV / JSON)</h3><p class=\"small muted\">Paste rows copied from Excel or Google Sheets straight into the sheet (select a cell, Ctrl+V), or import a CSV / JSON file here. The first line names the columns. Rows whose ID already exists update that row.</p>");
        h.push_str(&format!(
            "<textarea id=\"csv-{a}\" rows=\"6\" placeholder=\"CSV with a header row, or a JSON array of objects\" data-in=\"form\" data-arg=\"csv:{a}\" aria-label=\"Import data\">{}</textarea><div class=\"row\"><button class=\"btn primary\" data-a=\"import-csv\" data-arg=\"{a}\">Import</button><label class=\"btn\">Choose file…<input type=\"file\" accept=\".csv,.tsv,.txt,.json,text/csv,application/json\" data-file=\"import-file\" data-arg=\"{a}\" hidden></label></div></section>",
            esc(form(app, &format!("csv:{}", arg))),
            a = esc(&arg)
        ));
    } else {
        h.push_str("<section class=\"card\"><h3>Import a spreadsheet as a new table</h3><p class=\"small muted\">A CSV or JSON file becomes a new table named after the file; its first line names the columns.</p><div class=\"row\"><label class=\"btn primary\">Choose file…<input type=\"file\" accept=\".csv,.tsv,.txt,.json,text/csv,application/json\" data-file=\"import-csv-new\" hidden></label></div></section>");
    }
    h.push_str("<section class=\"card\"><h3>Import SQL</h3><p class=\"small muted\">A .sql file or pasted statements — a phpMyAdmin or mysqldump export works. Tables, rows, keys and views are created here as unsaved changes; nothing is written to the blockchain until you press Save.</p>");
    h.push_str(&format!(
        "<textarea id=\"sqlimp\" rows=\"6\" placeholder=\"CREATE TABLE …; INSERT INTO …;\" data-in=\"form\" data-arg=\"sqlimp\" spellcheck=\"false\" aria-label=\"SQL to import\">{}</textarea><div class=\"row\"><button class=\"btn primary\" data-a=\"import-sql\">Run import</button><label class=\"btn\">Choose .sql file…<input type=\"file\" accept=\".sql,.txt,application/sql,text/plain\" data-file=\"import-sql-file\" hidden></label></div>",
        esc(form(app, "sqlimp"))
    ));
    let outs = app.ed.import_out.clone();
    if !outs.is_empty() {
        let ok = outs.iter().filter(|o| matches!(o, Out::Msg(true, _))).count();
        let bad: Vec<Out> = outs.iter().filter(|o| matches!(o, Out::Msg(false, _))).cloned().collect();
        h.push_str(&format!("<p class=\"small\">{} statement{} ran.</p>", ok, if ok == 1 { "" } else { "s" }));
        if !bad.is_empty() {
            outputs(app, key, &bad, h);
        }
        let shown: Vec<Out> = outs.iter().filter(|o| matches!(o, Out::Msg(true, _))).take(12).cloned().collect();
        h.push_str("<details><summary>Details</summary>");
        outputs(app, key, &shown, h);
        h.push_str("</details>");
    }
    h.push_str("</section>");
}

// ------------------------------------------------------------- operations

fn tbl_operations(app: &mut App, key: &str, t: usize, h: &mut String) {
    let di = app.draft_idx(key).unwrap();
    let d = app.drafts[di].clone();
    let tb = d.tables[t].clone();
    let arg = format!("{}:{}", key, t);
    h.push_str("<div class=\"cols2\">");
    // rename + comment
    h.push_str(&format!(
        "<section class=\"card\"><h3>Rename</h3><div class=\"row\"><input id=\"op-name\" value=\"{}\" placeholder=\"{}\" data-in=\"form\" data-arg=\"op:name\" data-enter=\"op-rename\" maxlength=\"64\" aria-label=\"New name\"><button class=\"btn\" data-a=\"op-rename\">Rename</button></div><p class=\"small muted\">{}</p>",
        esc(form(app, "op:name")),
        esc(&tb.title),
        if tb.created.is_some() { "The table keeps its address on the blockchain; the new name is saved with one small update." } else { "" }
    ));
    h.push_str(&format!(
        "<h3>Comment</h3><div class=\"row\"><input id=\"op-comment\" value=\"{}\" placeholder=\"what this table holds\" data-in=\"form\" data-arg=\"op:comment\" data-enter=\"op-comment\" aria-label=\"Table comment\"><button class=\"btn\" data-a=\"op-comment\">Set</button></div></section>",
        esc(app.form.get("op:comment").map(|s| s.as_str()).unwrap_or(&tb.keys.comment))
    ));
    // access
    h.push_str("<section class=\"card\"><h3>Who can add rows</h3>");
    h.push_str(&format!(
        "<div class=\"seg\"><button class=\"{}\" data-a=\"op-access\" data-arg=\"locked\">Only chosen wallets</button><button class=\"{}\" data-a=\"op-access\" data-arg=\"open\">Anyone</button></div>",
        if !tb.open { "on" } else { "" },
        if tb.open { "on" } else { "" }
    ));
    if tb.open {
        h.push_str("<p class=\"small muted\">Anyone can add rows. Rows from wallets other than this database's show as unofficial, and readers can hide them.</p>");
    } else {
        h.push_str("<ul class=\"small writers\"><li>This database's wallet</li>");
        for w in &tb.writers {
            h.push_str(&format!("<li>{} <button class=\"link danger\" data-a=\"op-writer-del\" data-arg=\"{}\">remove</button></li>", va::who(app, w), esc(w)));
        }
        h.push_str(&format!(
            "</ul><div class=\"row\"><input id=\"op-writer\" placeholder=\"wallet address to allow\" value=\"{}\" data-in=\"form\" data-arg=\"op:writer\" data-enter=\"op-writer-add\" aria-label=\"Wallet address\"><button class=\"btn\" data-a=\"op-writer-add\">Allow</button></div>",
            esc(form(app, "op:writer"))
        ));
    }
    h.push_str("<p class=\"small muted\">Same as <code>GRANT INSERT</code> / <code>REVOKE INSERT</code> in SQL. Only IQ Tables enforces column types and rules; the blockchain enforces who may write.</p></section>");
    // copy
    h.push_str(&format!(
        "<section class=\"card\"><h3>Copy table</h3><div class=\"row\"><input id=\"op-copy\" placeholder=\"{}_copy\" value=\"{}\" data-in=\"form\" data-arg=\"op:copy\" data-enter=\"op-copy\" maxlength=\"64\" aria-label=\"Name of the copy\"><button class=\"btn\" data-a=\"op-copy\">Copy</button></div><label class=\"check\"><input type=\"checkbox\" data-in=\"form\" data-arg=\"op:copydata\" {}> with its rows</label></section>",
        esc(&tb.title),
        esc(form(app, "op:copy")),
        if form(app, "op:copydata") != "false" { "checked" } else { "" }
    ));
    // storage
    let (_, tpda) = app.table_pda_of(key, t).unwrap_or_default();
    h.push_str(&format!(
        "<section class=\"card\"><h3>Storage</h3><label class=\"check\"><input type=\"checkbox\" data-in=\"table-compress\" data-arg=\"{}\" {}> Compress saved rows (cheaper; off keeps them readable by other tools)</label><p class=\"small muted\">On-chain name <span class=\"mono\">{}</span> · address <span class=\"mono\">{}</span> · <button class=\"link\" data-a=\"copy\" data-arg=\"iq://table/{}\">copy link</button>{}</p></section>",
        esc(&arg),
        if tb.compress { "checked" } else { "" },
        esc(&tb.name),
        esc(&solana::short(&tpda)),
        esc(&tpda),
        if tb.created.is_some() { format!(" · <a href=\"#/t/{}/{}\">view in Explore</a>", esc(&b58(&iq::db_root_pda(d.name.as_bytes()))), esc(&tpda)) } else { String::new() }
    ));
    h.push_str("</div>");
    // checkpoint
    if tb.created.is_some() {
        let (_, pda) = app.table_pda_of(key, t).unwrap_or_default();
        // writes read from the chain, plus ones this browser made since
        let (mut writes, cut, seen) = app
            .bases
            .get(&pda)
            .map(|b| (b.rows.len(), b.cut, b.rows.iter().filter_map(|r| r.get("__txSignature").str().map(String::from)).collect::<Vec<_>>()))
            .unwrap_or((0, false, vec![]));
        let mut mine: Vec<&String> = tb.rows.iter().filter_map(|r| r.sig.as_ref()).filter(|s| !seen.contains(s)).collect();
        mine.sort();
        mine.dedup();
        writes += mine.len();
        let live = app.sheet_rows(key, t).iter().filter(|r| r.state != RowState::Deleted).count();
        h.push_str("<section class=\"card\"><h3>Checkpoint</h3>");
        h.push_str(&format!(
            "<p class=\"small\">{}</p>",
            if cut {
                format!("This table opens from its last checkpoint: {} write(s) read for {} row(s).", writes, live)
            } else {
                format!("Opening this table replays its whole history: {} write(s) on the blockchain for {} row(s) today.", writes, live)
            }
        ));
        h.push_str("<p class=\"small muted\">A checkpoint rewrites the current rows together (as one chunked write when that's cheaper) and records that readers can start there, so a table with a long history of edits opens quickly again. Nothing old is erased. Same as <code>OPTIMIZE TABLE</code>.</p>");
        h.push_str(&if tb.checkpoint {
            "<div class=\"row\"><span class=\"pill un\">checkpoint on next save</span><button class=\"btn\" data-a=\"op-checkpoint\" data-arg=\"off\">Cancel</button></div>".to_string()
        } else {
            "<div class=\"row\"><button class=\"btn\" data-a=\"op-checkpoint\" data-arg=\"on\">Checkpoint on next save</button></div>".to_string()
        });
        h.push_str("</section>");
    }
    // danger zone
    h.push_str("<section class=\"card dz\"><h3>Delete data</h3><div class=\"row\">");
    h.push_str(&danger(app, "op-truncate", &t.to_string(), "Empty the table (TRUNCATE)", false));
    h.push_str(&danger(app, "op-drop", &t.to_string(), "Delete the table (DROP)", false));
    h.push_str("</div><p class=\"small muted\">Takes effect when you save. The blockchain keeps history, so earlier saves stay readable in the chain's records — IQ Tables just stops showing them.</p></section>");
}

// -------------------------------------------------------------------- SQL

fn sql_tab(app: &mut App, key: &str, t: Option<usize>, h: &mut String) {
    let di = app.draft_idx(key).unwrap();
    let d = app.drafts[di].clone();
    let q = crate::sql_exec::sql_ident;
    let examples: Vec<(String, String)> = match t {
        Some(t) => {
            let tb = &d.tables[t];
            let tn = q(&tb.title);
            let cols: Vec<String> = tb.columns.iter().map(|c| q(c)).collect();
            let non_id: Vec<String> = tb.columns.iter().enumerate().filter(|(c, _)| !tb.meta[*c].auto_inc).map(|(_, n)| q(n)).collect();
            let idc = q(&id_name(tb));
            vec![
                ("SELECT *".into(), format!("SELECT * FROM {} WHERE 1 LIMIT 50;", tn)),
                ("SELECT".into(), format!("SELECT {} FROM {} WHERE 1;", cols.join(", "), tn)),
                ("INSERT".into(), format!("INSERT INTO {} ({}) VALUES ({});", tn, non_id.join(", "), non_id.iter().map(|_| "''").collect::<Vec<_>>().join(", "))),
                ("UPDATE".into(), format!("UPDATE {} SET {} WHERE {} = 1;", tn, non_id.iter().map(|c| format!("{} = ''", c)).collect::<Vec<_>>().join(", "), idc)),
                ("DELETE".into(), format!("DELETE FROM {} WHERE {} = 1;", tn, idc)),
                ("Count".into(), format!("SELECT COUNT(*) AS total FROM {};", tn)),
                ("Describe".into(), format!("DESCRIBE {};", tn)),
            ]
        }
        None => {
            let names: Vec<String> = d.tables.iter().filter(|x| !x.dropped && !x.is_system()).map(|x| q(&x.title)).collect();
            let mut v = vec![("SHOW TABLES".into(), "SHOW TABLES;".into()), ("Unsaved changes".into(), "SHOW CHANGES;".into())];
            if names.len() >= 2 {
                v.push(("Join".into(), format!("SELECT a.*, b.*\nFROM {} a\nJOIN {} b ON b.id = a.id\nLIMIT 50;", names[0], names[1])));
            }
            v.push(("New table".into(), "CREATE TABLE `products` (\n  `id` INT AUTO_INCREMENT PRIMARY KEY,\n  `name` VARCHAR(100) NOT NULL,\n  `price` DECIMAL(10,2) DEFAULT 0 CHECK (`price` >= 0),\n  `added` DATETIME DEFAULT CURRENT_TIMESTAMP\n);".into()));
            v
        }
    };
    let default_text = match t {
        Some(t) => format!("SELECT * FROM {} WHERE 1 LIMIT 50;", q(&d.tables[t].title)),
        None => "SHOW TABLES;".into(),
    };
    let text = app.ed.sql_text.get(key).cloned().unwrap_or(default_text);
    h.push_str(&format!(
        "<div class=\"sqlbox\"><p class=\"small muted\">Run SQL on {}{}:</p><textarea id=\"sql-{k}\" rows=\"7\" data-in=\"form\" data-arg=\"sql:{k}\" data-keys=\"sql\" spellcheck=\"false\" aria-label=\"SQL\">{}</textarea>",
        if t.is_some() { "table " } else { "database " },
        esc(&t.map(|t| d.tables[t].title.clone()).unwrap_or_else(|| d.name.clone())),
        esc(&text),
        k = esc(key)
    ));
    h.push_str("<div class=\"row\"><button class=\"btn primary\" data-a=\"sql-run\">Go <span class=\"muted small\">Ctrl+Enter</span></button>");
    for (label, q) in &examples {
        h.push_str(&format!("<button class=\"btn small\" data-a=\"sql-example\" data-arg=\"{}\">{}</button>", esc(q), esc(label)));
    }
    h.push_str("</div><div class=\"row\">");
    h.push_str(&format!(
        "<input id=\"bm-name\" class=\"bm\" placeholder=\"name this query to keep it\" value=\"{}\" data-in=\"form\" data-arg=\"bm:name\" data-enter=\"bookmark-save\" aria-label=\"Bookmark name\"><button class=\"btn small\" data-a=\"bookmark-save\">☆ Save query</button><button class=\"btn small\" data-a=\"view-from-sql\" title=\"Save this SELECT as a view\">Make a view</button>",
        esc(form(app, "bm:name"))
    ));
    h.push_str("</div>");
    if !d.bookmarks.is_empty() {
        h.push_str("<div class=\"row bms\"><span class=\"muted small\">Saved queries:</span>");
        for (i, (n, _)) in d.bookmarks.iter().enumerate() {
            h.push_str(&format!("<span class=\"bmk\"><button class=\"link\" data-a=\"bookmark-load\" data-arg=\"{}\">☆ {}</button>{}</span>", i, esc(n), danger(app, "bookmark-del", &i.to_string(), "×", true)));
        }
        h.push_str("</div>");
    }
    if !app.ed.sql_hist.is_empty() {
        h.push_str("<details class=\"hist\"><summary>History</summary><ol>");
        for (i, q) in app.ed.sql_hist.iter().enumerate().rev().take(20) {
            h.push_str(&format!("<li><button class=\"link mono small\" data-a=\"sql-hist\" data-arg=\"{}\">{}</button></li>", i, esc(&q.chars().take(110).collect::<String>())));
        }
        h.push_str("</ol></details>");
    }
    h.push_str("</div><div class=\"sqlout\">");
    let outs = app.ed.sql_out.clone();
    if outs.iter().any(|o| matches!(o, Out::Rows { .. })) {
        h.push_str("<div class=\"row\"><span class=\"grow\"></span><button class=\"btn small\" data-a=\"sql-csv\">Download result as CSV</button></div>");
    }
    outputs(app, key, &outs, h);
    h.push_str("</div>");
    h.push_str(SQL_HELP);
}

const SQL_HELP: &str = r#"<details class="ref"><summary>What you can type</summary><div class="small"><p>MySQL-style SQL, run in your browser over the rows read from the blockchain plus your unsaved changes.</p><pre>SELECT [DISTINCT] … FROM t [JOIN u ON … | LEFT/RIGHT/CROSS JOIN | USING (…)]
  [WHERE …] [GROUP BY … [WITH ROLLUP]] [HAVING …] [ORDER BY … [DESC]] [LIMIT n [OFFSET m]]
  subqueries (IN, EXISTS, ANY/ALL, scalar, FROM (SELECT …)), UNION [ALL] / INTERSECT / EXCEPT,
  WITH [RECURSIVE] cte AS (…), CASE, window functions (ROW_NUMBER, RANK, LAG, SUM() OVER …),
  ~100 functions: CONCAT, SUBSTRING, REPLACE, REGEXP_REPLACE, ROUND, DATE_FORMAT, DATE_ADD,
  DATEDIFF, JSON_EXTRACT, GROUP_CONCAT, IFNULL, COALESCE, …
INSERT [IGNORE] INTO t (…) VALUES (…), (…) | SELECT … [ON DUPLICATE KEY UPDATE …] · REPLACE INTO …
UPDATE t [JOIN …] SET … [WHERE …] · DELETE FROM t [WHERE …]
CREATE TABLE t (col TYPE [NOT NULL] [DEFAULT …] [AUTO_INCREMENT] [UNIQUE] [CHECK (…)],
  PRIMARY KEY (…), UNIQUE (…), INDEX (…), FOREIGN KEY (…) REFERENCES u (…) [ON DELETE CASCADE]) [OPEN]
CREATE TABLE t LIKE u · CREATE TABLE t AS SELECT …
ALTER TABLE t ADD [COLUMN] … [FIRST | AFTER c] | DROP COLUMN c | MODIFY c TYPE … | CHANGE a b TYPE …
  | RENAME COLUMN a TO b | ALTER c SET DEFAULT … | ADD PRIMARY KEY/UNIQUE/INDEX/FOREIGN KEY/CHECK
  | DROP PRIMARY KEY/INDEX k/FOREIGN KEY k/CHECK k | RENAME TO u | COMMENT = '…' | AUTO_INCREMENT = n
RENAME TABLE a TO b · TRUNCATE t · DROP TABLE [IF EXISTS] t
CREATE [OR REPLACE] VIEW v AS SELECT … · DROP VIEW v
GRANT INSERT ON t TO 'wallet' | PUBLIC · REVOKE INSERT ON t FROM 'wallet' | PUBLIC
SHOW TABLES | COLUMNS FROM t | CREATE TABLE t | INDEX FROM t | CHANGES · DESCRIBE t · EXPLAIN SELECT …
SET @x = …, FOREIGN_KEY_CHECKS = 0 · COMMIT (save) · ROLLBACK (discard unsaved changes)</pre>
<p>Reads are free and instant. Changes wait until you press Save (or COMMIT) — that's the only step that costs anything. Data on the blockchain is permanent: deleting hides rows and dropping hides columns or tables, but earlier saves stay in the chain's history. Types, keys and rules are enforced by IQ Tables; other programs writing to an open table aren't bound by them.</p></div></details>"#;

// ------------------------------------------------------------------- save

fn save_tab(app: &mut App, key: &str, h: &mut String) {
    let di = app.draft_idx(key).unwrap();
    let ntables = app.drafts[di].tables.len();
    let cap = app.inline_cap();
    let mut plans = vec![];
    for t in 0..ntables {
        plans.push(app.plan_for(key, t, cap).clone());
    }
    let (cost, packs) = app.save_estimate(key);
    let d = app.drafts[di].clone();
    let wallet = d.wallet.clone();
    let write = iq::FEE_DIRECT_WRITE + iq::TX_FEE;
    let mut lines: Vec<String> = vec![];
    if d.root_sig.is_none() {
        lines.push(format!("Create the database “{}” · {}", esc(&d.name), ui::sol(iq::DB_ROOT_COST_ESTIMATE)));
    }
    let new_tables: Vec<String> = d.tables.iter().filter(|t| t.created.is_none() && !t.dropped).map(|t| if t.is_system() { "(views)".to_string() } else { t.title.clone() }).collect();
    if !new_tables.is_empty() {
        lines.push(format!("Create table{} {} · {} each", if new_tables.len() == 1 { "" } else { "s" }, esc(&new_tables.join(", ")), ui::sol(iq::TABLE_COST_ESTIMATE)));
    }
    if d.user_init_sig.is_none() {
        lines.push(format!("One-time setup for this database's wallet · {}", ui::sol(iq::USER_INIT_RENT_ESTIMATE)));
    }
    for (t, tb) in d.tables.iter().enumerate() {
        let label = if tb.is_system() { "Views".to_string() } else { tb.title.clone() };
        if tb.dropped {
            if tb.created.is_some() {
                lines.push(format!("<b>{}</b>: delete the table · {}", esc(&label), ui::sol(write + iq::TX_FEE)));
            }
            continue;
        }
        if tb.created.is_some() {
            if tb.checkpoint {
                lines.push(format!("<b>{}</b>: checkpoint — the table's rows are rewritten together, then readers skip its older history · {} for the record", esc(&label), ui::sol(write)));
            } else if tb.clear {
                lines.push(format!("<b>{}</b>: empty it (earlier rows stop showing) · {}", esc(&label), ui::sol(write)));
            } else if tb.schema_changed() {
                lines.push(format!("<b>{}</b>: new structure (columns, types, keys or rules) · {}", esc(&label), ui::sol(write)));
            }
            if tb.meta_changed(wallet.as_deref()) {
                let mut what = vec![];
                if tb.chain_title.as_ref().map(|x| *x != tb.title).unwrap_or(false) {
                    what.push(format!("rename to “{}”", tb.title));
                }
                if tb.chain_writers.as_ref().map(|w| *w != tb.desired_writers(wallet.as_deref())).unwrap_or(false) {
                    what.push("change who can add rows".to_string());
                }
                lines.push(format!("<b>{}</b>: {} · {}", esc(&label), esc(&what.join(", ")), ui::sol(iq::TX_FEE)));
            }
        } else if tb.schema_changed() && !tb.is_system() {
            lines.push(format!("<b>{}</b>: its column types and rules · {}", esc(&label), ui::sol(write)));
        }
        match &plans[t] {
            Ok(p) if !p.is_empty() => {
                let recs: usize = p.iter().map(|x| x.count).sum();
                let parts: usize = p.iter().map(|x| x.chunks).sum();
                let how = if parts > 0 {
                    format!("in one write sent in {} parts (IQ's chunked upload{})", parts, if parts >= iq::LINKED_LIST_THRESHOLD { ", session" } else { "" })
                } else {
                    format!("in {} write{}", p.len(), if p.len() == 1 { "" } else { "s" })
                };
                lines.push(format!(
                    "<b>{}</b>: {} {} {} {} · {}",
                    esc(&label),
                    recs,
                    if tb.checkpoint { "row(s) rewritten" } else { "changed" },
                    if tb.checkpoint { "" } else if tb.is_system() { "view(s)" } else { "row(s)" },
                    how,
                    ui::sol(p.iter().map(|x| x.cost()).sum::<u64>())
                ));
            }
            Err(e) => lines.push(format!("<span class=\"warn\"><b>{}</b>: {}</span>", esc(&label), esc(e))),
            _ => {}
        }
    }
    // rows that aren't complete yet
    let mut problems = vec![];
    for t in 0..ntables {
        if !d.tables[t].dropped {
            problems.extend(app.row_problems(key, t));
        }
    }
    let nothing = lines.is_empty();
    let running = app.run.as_ref().map(|r| r.busy()).unwrap_or(false);
    h.push_str("<section class=\"card savecard\">");
    if nothing {
        h.push_str("<h3>Everything is saved</h3><p class=\"muted\">Edit the sheet, the structure or run SQL and your changes will be waiting here.</p>");
    } else {
        h.push_str(&format!("<h3>Save to the blockchain</h3><p class=\"big\">≈ {}</p><ul class=\"costs\">", ui::sol(cost)));
        for l in &lines {
            h.push_str(&format!("<li>{}</li>", l));
        }
        h.push_str("</ul>");
        h.push_str("<p class=\"small muted\">Saved data is public and permanent. Most of the cost is a deposit the blockchain keeps for new accounts; the rest are small fees (0.001 SOL per write to IQ Labs).</p>");
    }
    if !problems.is_empty() {
        h.push_str("<div class=\"warnbox card\"><b>Fill these in before saving:</b><ul class=\"small\">");
        for p in problems.iter().take(12) {
            h.push_str(&format!("<li>{}</li>", esc(p)));
        }
        if problems.len() > 12 {
            h.push_str(&format!("<li>… and {} more</li>", problems.len() - 12));
        }
        h.push_str("</ul></div>");
    }
    match &app.account {
        None => {
            h.push_str("<div class=\"row\"><a class=\"btn primary big\" href=\"#/account\">Create a free account to save</a></div>");
        }
        Some(a) => {
            let main = a.main().map(|w| w.address()).unwrap_or_default();
            let main_bal = app.balances.get(&main).and_then(|b| b.ready().copied());
            let dbw = d.wallet.clone().unwrap_or_default();
            let db_bal = app.balances.get(&dbw).and_then(|b| b.ready().copied());
            if !nothing {
                h.push_str(&format!(
                    "<p class=\"small\">Paid from your balance{}{}.</p>",
                    main_bal.map(|b| format!(" ({})", ui::sol(b))).unwrap_or_default(),
                    if d.wallet.is_some() && dbw != main { format!(" — moved automatically to this database's own wallet{}", db_bal.map(|b| format!(", which has {}", ui::sol(b))).unwrap_or_default()) } else { String::new() }
                ));
                let rows: usize = plans.iter().filter_map(|p| p.as_ref().ok()).map(|p| p.iter().map(|x| x.count).sum::<usize>()).sum();
                h.push_str(&format!(
                    "<div class=\"row\"><button class=\"btn primary big\" data-a=\"inscribe\" data-arg=\"{}\" {}>{}</button>{}</div>",
                    esc(key),
                    if running || app.busy_note.is_some() { "disabled" } else { "" },
                    if packs > 0 && rows > 0 { format!("Save {} change(s) · ≈{}", rows, ui::sol(cost)) } else { format!("Save · ≈{}", ui::sol(cost)) },
                    app.busy_note.as_ref().map(|n| format!("<span class=\"muted small\">{}</span>", esc(n))).unwrap_or_default()
                ));
                if main_bal.map(|b| b < cost).unwrap_or(false) && db_bal.map(|b| b < cost).unwrap_or(true) {
                    h.push_str("<p class=\"warn small\">Your balance is lower than this. <button class=\"link\" data-a=\"add-funds\">Add funds</button> first.</p>");
                }
            }
        }
    }
    if let Some(r) = app.run.as_ref().filter(|r| r.draft == key) {
        let (cls, st) = match &r.state {
            RunState::Preparing => ("", "Getting ready…".to_string()),
            RunState::Working(s) => ("", s.clone()),
            RunState::Paused(s) => ("warn", s.clone()),
            RunState::Done => ("good", "Saved ✓".into()),
            RunState::Failed(s) => ("bad", s.clone()),
        };
        h.push_str(&format!("<div class=\"run\"><p class=\"{}\"><b>{}</b></p>", cls, esc(&st).replace('\n', "<br>")));
        let done = r.steps.iter().filter(|s| s.sig.is_some()).count();
        if !r.steps.is_empty() {
            h.push_str(&format!(
                "<div class=\"bar\"><span style=\"width:{}%\"></span></div><p class=\"small\">{}/{} steps{}</p>",
                done * 100 / r.steps.len(),
                done,
                r.steps.len(),
                r.spent().map(|s| format!(" · spent {}", ui::sol(s))).unwrap_or_default()
            ));
        }
        h.push_str("<div class=\"row\">");
        if r.busy() {
            h.push_str("<button class=\"btn\" data-a=\"run-stop\">Pause after this step</button>");
        } else {
            if !matches!(r.state, RunState::Done) {
                h.push_str("<button class=\"btn primary\" data-a=\"run-resume\">Continue</button>");
            }
            h.push_str("<button class=\"btn\" data-a=\"run-close\">Close</button>");
        }
        h.push_str("</div><details><summary>Details</summary><ol class=\"log\">");
        for (ok, m) in r.log.iter().rev().take(30) {
            h.push_str(&format!("<li class=\"{}\">{}</li>", if *ok { "" } else { "warn" }, esc(m).replace('\n', "<br>")));
        }
        h.push_str("</ol><ol class=\"steps\">");
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
    h.push_str("</section>");
    h.push_str("<details class=\"adv\"><summary>Advanced: this database's wallet and options</summary>");
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"lock-creators\" data-arg=\"{}\" {} {}> Only this database's wallet may create tables in it</label>",
        esc(key),
        if d.lock_creators { "checked" } else { "" },
        if d.root_sig.is_some() { "disabled" } else { "" }
    ));
    va::draft_wallet(app, key, h);
    h.push_str("</details>");
}
