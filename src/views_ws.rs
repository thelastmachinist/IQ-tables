//! The editor: a phpMyAdmin-style layout (databases and tables in a sidebar,
//! tabs on the right) around a spreadsheet, a SQL console and the save panel.

use crate::app::{App, Load};
use crate::editor::{BaseState, SHEET_PAGE};
use crate::inscribe::RunState;
use crate::iq;
use crate::json::Json;
use crate::sheet::{self, RowState};
use crate::solana::{self, b58};
use crate::sql_exec::Out;
use crate::ui::{self, esc};
use crate::views::{cell_html, pretty};
use crate::views_account as va;

fn form<'a>(app: &'a App, k: &str) -> &'a str {
    app.form.get(k).map(|s| s.as_str()).unwrap_or("")
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
            h.push_str(&format!(
                "<a class=\"tile\" href=\"#/ws/{}\"><b>{}</b><span class=\"muted small\">{} table(s)</span>{}{}</a>",
                esc(&d.key),
                esc(&d.name),
                d.tables.len(),
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
            for (t, tb) in app.drafts[di].tables.iter().enumerate() {
                h.push_str(&format!(
                    "<li><a class=\"tb{}\" href=\"#/ws/{}/{}\">▦ {}</a>{}</li>",
                    if cur.and_then(|c| c.1) == Some(t) { " on" } else { "" },
                    esc(&key),
                    t,
                    esc(&tb.name),
                    if tb.ghosts() > 0 { "<span class=\"dotp\"></span>" } else { "" }
                ));
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

// --------------------------------------------------------------- database

pub fn page(app: &mut App, key: &str, h: &mut String) {
    let Some(di) = app.draft_idx(key) else {
        h.push_str("<div class=\"card\">That database isn't in this browser. <a href=\"#/ws\">Back to the editor</a></div>");
        return;
    };
    let ntables = app.drafts[di].tables.len();
    let t = app.drafts[di].sel.min(ntables.saturating_sub(1));
    let has_table = ntables > 0;
    h.push_str("<div class=\"ed\">");
    sidebar(app, Some((key, if has_table { Some(t) } else { None })), h);
    h.push_str("<section class=\"edmain\">");
    let d = &app.drafts[di];
    let dname = d.name.clone();
    let on_chain = d.root_sig.is_some();
    h.push_str(&format!(
        "<div class=\"edhead\"><h1>{}{}</h1>{}</div>",
        esc(&dname),
        if has_table { format!(" <span class=\"muted\">›</span> {}", esc(&d.tables[t].name)) } else { String::new() },
        if on_chain { "<span class=\"pill off\">on the blockchain</span>" } else { "<span class=\"pill ghost\">not saved yet</span>" }
    ));
    // someone else already owns this name
    if let Some(Load::Ready(Some(c))) = app.name_checks.get(key) {
        let ours = app.drafts[di].wallet.as_deref() == Some(c.as_str()) || va::wallet_label(app, c).is_some();
        if !ours {
            h.push_str(&format!(
                "<p class=\"warn small\">“{}” already belongs to someone else ({}). You can add rows to its open tables — they'll show as unofficial — but you can't create tables in it.</p>",
                esc(&dname),
                esc(&solana::short(c))
            ));
        }
    }
    // tabs
    let tab = if app.ed.tab.is_empty() { "browse".to_string() } else { app.ed.tab.clone() };
    let tab = if !has_table && tab != "sql" && tab != "save" { "structure".to_string() } else { tab };
    // pending across the database
    let mut pend = (0, 0, 0);
    for tt in 0..ntables {
        let rows = app.sheet_rows(key, tt);
        let p = sheet::pending(&rows);
        pend = (pend.0 + p.0, pend.1 + p.1, pend.2 + p.2);
    }
    let pend_n = pend.0 + pend.1 + pend.2;
    let new_tables = app.drafts[di].tables.iter().filter(|t| t.created.is_none()).count();
    let (cost, _) = app.save_estimate(key);
    h.push_str("<nav class=\"tabs2\" role=\"tablist\">");
    for (id, label) in [("browse", "Browse"), ("structure", "Structure"), ("sql", "SQL"), ("import", "Import & export"), ("save", "Save")] {
        if !has_table && (id == "browse" || id == "import") {
            continue;
        }
        let badge = if id == "save" && (pend_n > 0 || new_tables > 0 || !on_chain) { " <span class=\"dotp\"></span>" } else { "" };
        h.push_str(&format!("<button role=\"tab\" class=\"{}\" data-a=\"ed-tab\" data-arg=\"{}\">{}{}</button>", if tab == id { "on" } else { "" }, id, label, badge));
    }
    h.push_str("</nav>");
    let running = app.run.as_ref().map(|r| r.busy() && r.draft == key).unwrap_or(false);
    if tab != "save" && (pend_n > 0 || running) {
        h.push_str(&format!(
            "<div class=\"pendbar\"><span>{}</span><span class=\"grow\"></span>{}</div>",
            if running {
                "Saving to the blockchain…".to_string()
            } else {
                format!("● {} unsaved change(s){}", pend_n, detail(pend))
            },
            if running {
                "<button class=\"btn\" data-a=\"ed-tab\" data-arg=\"save\">See progress</button>".to_string()
            } else {
                format!(
                    "<button class=\"btn primary\" data-a=\"inscribe\" data-arg=\"{}\">Save to blockchain · ≈{}</button><button class=\"link\" data-a=\"discard\">discard</button>",
                    esc(key),
                    ui::sol(cost)
                )
            }
        ));
    }
    match tab.as_str() {
        "browse" => browse(app, key, t, h),
        "structure" => structure(app, key, if has_table { Some(t) } else { None }, h),
        "sql" => sql_tab(app, key, h),
        "import" => import_tab(app, key, t, h),
        _ => save_tab(app, key, h),
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

// ------------------------------------------------------------------ sheet

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
    let cur_val = order.get(sel_r).and_then(|&i| rows[i].vals.get(sel_c)).map(|v| v.cell_text()).unwrap_or_default();
    h.push_str(&format!(
        "<div class=\"fbar\"><span class=\"ref\">{}{}</span><input id=\"fbar\" value=\"{}\" data-in=\"sheet-fbar\" aria-label=\"Cell value\" spellcheck=\"false\"></div>",
        sheet::col_letter(sel_c),
        sel_r + 1,
        esc(&cur_val)
    ));
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
            h.push_str(&format!(
                "<div class=\"ch\"><span class=\"letter\">{}</span><span class=\"cname\" title=\"{}\">{}</span>{}{}<button class=\"cm\" data-a=\"sheet-menu\" data-arg=\"{}\" aria-label=\"Column options\">▾</button></div>",
                sheet::col_letter(c),
                esc(name),
                esc(name),
                if c == tb.id_col { "<span class=\"key\" title=\"ID column: identifies each row\">🔑</span>" } else { "" },
                sort,
                c
            ));
        }
        if app.ed.menu == Some(c) {
            h.push_str("<div class=\"colmenu\">");
            h.push_str(&format!("<button data-a=\"sheet-sort\" data-arg=\"{c}:asc\">Sort A → Z</button><button data-a=\"sheet-sort\" data-arg=\"{c}:desc\">Sort Z → A</button><hr>", c = c));
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
            let mut cls = String::new();
            if is_sel {
                cls.push_str(" sel");
            } else if in_rng {
                cls.push_str(" rng");
            }
            if chg {
                cls.push_str(" chg");
            }
            if matches!(v, Json::Num(_)) {
                cls.push_str(" num");
            }
            if is_sel && editing.is_some() {
                h.push_str(&format!(
                    "<td class=\"{} editing\"><input id=\"cell-editor\" data-editor=\"1\" data-keys=\"editor\" data-arg=\"{}:{}\" data-focus=\"force\" value=\"{}\" spellcheck=\"false\" aria-label=\"Edit cell\"></td>",
                    cls.trim(),
                    r,
                    c,
                    esc(editing.as_deref().unwrap_or(""))
                ));
            } else {
                let title = match (chg, row.and_then(|x| x.base.as_ref()).and_then(|b| b.get(c))) {
                    (true, Some(old)) => format!(" title=\"was: {}\"", esc(&old.cell_text())),
                    _ => String::new(),
                };
                h.push_str(&format!("<td class=\"{}\" data-drag=\"scell\" data-arg=\"{}:{}\"{}>{}</td>", cls.trim(), r, c, title, cell_html(app, &v, 80)));
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
                        if let Some(f) = crate::sql::num(v) {
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
        h.push_str(&format!("<span>Sum {}</span><span>Average {}</span>", crate::sql::fmt_num(sum).cell_text(), crate::sql::fmt_num(sum / nums.len() as f64).cell_text()));
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
    h.push_str("<span class=\"grow\"></span><span class=\"legend\"><i class=\"lg new\"></i>new <i class=\"lg chg\"></i>edited <i class=\"lg del\"></i>deleted</span></div>");
    h.push_str("<p class=\"muted small tip\">Click a cell and type. Enter/Tab move, arrows navigate, Shift extends the selection, Ctrl+C / Ctrl+V copy and paste from Excel or Google Sheets, Delete clears, Ctrl+Z undoes. A column's ▾ menu sorts and changes columns.</p>");
}

// -------------------------------------------------------------- structure

fn structure(app: &mut App, key: &str, t: Option<usize>, h: &mut String) {
    let di = app.draft_idx(key).unwrap();
    let d = app.drafts[di].clone();
    // tables overview
    h.push_str("<section class=\"card\"><h3>Tables</h3>");
    if d.tables.is_empty() {
        h.push_str("<p class=\"muted\">No tables yet.</p>");
    } else {
        h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th>Table</th><th class=\"num\">Rows</th><th>Columns</th><th>Who can add rows</th><th>Status</th></tr></thead><tbody>");
        for (i, tb) in d.tables.iter().enumerate() {
            let rows = app.sheet_rows(key, i);
            let live = rows.iter().filter(|r| r.state != RowState::Deleted).count();
            h.push_str(&format!(
                "<tr><td><a href=\"#/ws/{}/{}\">▦ {}</a></td><td class=\"num\">{}</td><td class=\"small\">{}</td><td>{}</td><td>{}</td></tr>",
                esc(key),
                i,
                esc(&tb.name),
                live,
                esc(&tb.columns.join(", ")),
                if tb.open { "anyone (shown as unofficial)" } else { "only you" },
                if tb.created.is_some() { "<span class=\"pill off\">on the blockchain</span>" } else { "<span class=\"pill ghost\">not saved yet</span>" }
            ));
        }
        h.push_str("</tbody></table></div>");
    }
    h.push_str(&format!(
        "<details {}><summary>New table</summary><div class=\"formgrid\"><label>Name<input id=\"tname-{k}\" placeholder=\"e.g. suppliers\" value=\"{}\" data-in=\"form\" data-arg=\"tname:{k}\" maxlength=\"64\"></label><label>Columns (comma separated; the first identifies each row)<input id=\"tcols-{k}\" placeholder=\"part_no, name, qty\" value=\"{}\" data-in=\"form\" data-arg=\"tcols:{k}\"></label><label>Who can add rows<select data-in=\"form\" data-arg=\"topen:{k}\"><option value=\"locked\">Only you</option><option value=\"open\" {}>Anyone (their rows show as unofficial)</option></select></label></div><div class=\"row\"><button class=\"btn primary\" data-a=\"add-table\" data-arg=\"{k}\">Create table</button></div></details>",
        if d.tables.is_empty() { "open" } else { "" },
        esc(form(app, &format!("tname:{}", key))),
        esc(form(app, &format!("tcols:{}", key))),
        if form(app, &format!("topen:{}", key)) == "open" { "selected" } else { "" },
        k = esc(key)
    ));
    h.push_str("</section>");
    let Some(t) = t else { return };
    let tb = d.tables[t].clone();
    let (base, _) = app.sheet_base(key, t);
    let saved = tb.created.is_some() || !base.is_empty();
    let arg = format!("{}:{}", key, t);
    h.push_str(&format!("<section class=\"card\"><h3>Columns of {}</h3>", esc(&tb.name)));
    h.push_str("<div class=\"scroll\"><table class=\"grid\"><thead><tr><th></th><th>Column</th><th>Identifies rows</th><th></th></tr></thead><tbody>");
    for (c, name) in tb.columns.iter().enumerate() {
        h.push_str(&format!("<tr><td class=\"muted\">{}</td><td>", sheet::col_letter(c)));
        if saved {
            h.push_str(&esc(name));
        } else {
            h.push_str(&format!("<input id=\"cn-{}\" class=\"inline\" value=\"{}\" data-in=\"col-rename\" data-arg=\"{}\" aria-label=\"Column name\">", c, esc(name), c));
        }
        h.push_str("</td><td>");
        if c == tb.id_col {
            h.push_str("🔑 ID");
        } else if !saved {
            h.push_str(&format!("<button class=\"link\" data-a=\"col-id\" data-arg=\"{}\">use as ID</button>", c));
        }
        h.push_str("</td><td class=\"nowrap\">");
        if c > 0 {
            h.push_str(&format!("<button class=\"link\" data-a=\"col-move\" data-arg=\"{}:{}\" aria-label=\"Move up\">↑</button> ", c, c - 1));
        }
        if c + 1 < tb.columns.len() {
            h.push_str(&format!("<button class=\"link\" data-a=\"col-move\" data-arg=\"{}:{}\" aria-label=\"Move down\">↓</button> ", c, c + 1));
        }
        if c != tb.id_col {
            h.push_str(&format!("<button class=\"link danger\" data-a=\"col-delete\" data-arg=\"{}\">delete</button>", c));
        }
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table></div>");
    h.push_str(&format!(
        "<div class=\"row\"><input id=\"newcol\" placeholder=\"new column name\" value=\"{}\" data-in=\"form\" data-arg=\"newcol:{}\" data-enter=\"col-add\"><button class=\"btn\" data-a=\"col-add\">Add column</button></div>",
        esc(form(app, &format!("newcol:{}", arg))),
        esc(&arg)
    ));
    if saved {
        h.push_str("<p class=\"muted small\">This table is on the blockchain: you can add columns any time. Renaming or changing the ID column would disconnect the rows already saved, so it isn't offered.</p>");
    }
    h.push_str("</section>");
    h.push_str(&format!("<section class=\"card\"><h3>Settings</h3>"));
    if tb.created.is_none() {
        h.push_str(&format!(
            "<label class=\"check\"><input type=\"checkbox\" data-in=\"table-open\" data-arg=\"{}\" {}> Let anyone add rows (their rows show as unofficial)</label>",
            esc(&arg),
            if tb.open { "checked" } else { "" }
        ));
    } else {
        h.push_str(&format!("<p>{}</p>", if tb.open { "Anyone can add rows (shown as unofficial)." } else { "Only this database's wallet can add rows." }));
    }
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"table-compress\" data-arg=\"{}\" {}> Compress saved rows (cheaper; off makes them readable by other tools)</label>",
        esc(&arg),
        if tb.compress { "checked" } else { "" }
    ));
    let (_, tpda) = app.table_pda_of(key, t).unwrap_or_default();
    h.push_str(&format!(
        "<p class=\"small muted\">Table address <span class=\"mono\">{}</span> · <button class=\"link\" data-a=\"copy\" data-arg=\"iq://table/{}\">copy link</button>{}</p>",
        esc(&solana::short(&tpda)),
        esc(&tpda),
        if tb.created.is_some() { format!(" · <a href=\"#/t/{}/{}\">view in Explore</a>", esc(&b58(&iq::db_root_pda(d.name.as_bytes()))), esc(&tpda)) } else { String::new() }
    ));
    if tb.created.is_none() {
        h.push_str(&format!("<p><button class=\"link danger\" data-a=\"del-table\" data-arg=\"{}\">Delete this table</button></p>", esc(&arg)));
    }
    h.push_str("</section>");
    h.push_str(&format!("<p><button class=\"link danger\" data-a=\"del-draft\" data-arg=\"{}\">Remove this database from the editor</button> <span class=\"muted small\">(anything already on the blockchain stays there)</span></p>", esc(key)));
}

// -------------------------------------------------------------------- SQL

const EXAMPLES: &[(&str, &str)] = &[
    ("SHOW TABLES", "SHOW TABLES;"),
    ("Browse", "SELECT * FROM `{t}` LIMIT 50;"),
    ("Filter + sort", "SELECT * FROM `{t}` WHERE `{c}` LIKE '%a%' ORDER BY `{c}` LIMIT 50;"),
    ("Count", "SELECT COUNT(*) AS total FROM `{t}`;"),
    ("Structure", "DESCRIBE `{t}`;"),
    ("Unsaved changes", "SHOW CHANGES;"),
];

fn sql_tab(app: &mut App, key: &str, h: &mut String) {
    let di = app.draft_idx(key).unwrap();
    let t = app.drafts[di].sel;
    let tname = app.drafts[di].tables.get(t).map(|x| x.name.clone()).unwrap_or_else(|| "table".into());
    let cname = app.drafts[di].tables.get(t).and_then(|x| x.columns.get(1).or(x.columns.first()).cloned()).unwrap_or_else(|| "name".into());
    let text = app.ed.sql_text.get(key).cloned().unwrap_or_else(|| format!("SELECT * FROM `{}` LIMIT 50;", tname));
    h.push_str(&format!(
        "<div class=\"sqlbox\"><textarea id=\"sql-{k}\" rows=\"6\" data-in=\"form\" data-arg=\"sql:{k}\" data-keys=\"sql\" spellcheck=\"false\" aria-label=\"SQL\">{}</textarea>",
        esc(&text),
        k = esc(key)
    ));
    h.push_str("<div class=\"row\"><button class=\"btn primary\" data-a=\"sql-run\">Run <span class=\"muted small\">Ctrl+Enter</span></button>");
    for (label, q) in EXAMPLES {
        let q = q.replace("{t}", &tname).replace("{c}", &cname);
        h.push_str(&format!("<button class=\"btn small\" data-a=\"sql-example\" data-arg=\"{}\">{}</button>", esc(&q), label));
    }
    if !app.ed.sql_hist.is_empty() {
        h.push_str("<details class=\"hist\"><summary>History</summary><ol>");
        for (i, q) in app.ed.sql_hist.iter().enumerate().rev().take(15) {
            h.push_str(&format!("<li><button class=\"link mono small\" data-a=\"sql-hist\" data-arg=\"{}\">{}</button></li>", i, esc(&q.chars().take(90).collect::<String>())));
        }
        h.push_str("</ol></details>");
    }
    h.push_str("</div></div><div class=\"sqlout\">");
    for o in &app.ed.sql_out.clone() {
        match o {
            Out::Msg(ok, m) => h.push_str(&format!("<div class=\"sqlmsg {}\">{}</div>", if *ok { "ok" } else { "bad" }, esc(m))),
            Out::Rows { cols, rows, note, .. } => {
                h.push_str("<div class=\"scroll sqlres\"><table class=\"grid data\"><thead><tr>");
                for c in cols {
                    h.push_str(&format!("<th>{}</th>", esc(c)));
                }
                h.push_str("</tr></thead><tbody>");
                for r in rows {
                    h.push_str("<tr>");
                    for v in r {
                        h.push_str(&format!("<td class=\"{}\" title=\"{}\">{}</td>", if matches!(v, Json::Num(_)) { "num" } else { "" }, esc(&v.cell_text().chars().take(300).collect::<String>()), cell_html(app, v, 80)));
                    }
                    h.push_str("</tr>");
                }
                if rows.is_empty() {
                    h.push_str(&format!("<tr><td colspan=\"{}\" class=\"muted center\">No rows.</td></tr>", cols.len().max(1)));
                }
                h.push_str(&format!("</tbody></table></div><p class=\"muted small\">{}</p>", esc(note)));
            }
        }
    }
    h.push_str("</div>");
    h.push_str(r#"<details class="ref"><summary>What you can type</summary><div class="small"><pre>SELECT * | col, … | COUNT(*), SUM(col), AVG(col), MIN(col), MAX(col)
  FROM table [WHERE …] [GROUP BY col] [ORDER BY col [DESC]] [LIMIT n [OFFSET m]]
  WHERE: = != &lt; &gt; &lt;= &gt;=, AND, OR, NOT, LIKE '%x%', IN (…), BETWEEN a AND b, IS [NOT] NULL
INSERT INTO table (col, …) VALUES (…), (…)
UPDATE table SET col = value, … [WHERE …]
DELETE FROM table [WHERE …]
CREATE TABLE name (id PRIMARY KEY, col, …) [OPEN]
ALTER TABLE name ADD COLUMN col | DROP COLUMN col | RENAME COLUMN a TO b
DROP TABLE name · SHOW TABLES · SHOW CHANGES · DESCRIBE name
COMMIT   (save to the blockchain) · ROLLBACK (discard unsaved changes)</pre>
<p>Reads are free and instant. Changes wait until you COMMIT — that's the only step that costs anything. Data on the blockchain is permanent: deleting hides rows and dropping a column hides it, but earlier versions stay in the chain's history.</p></div></details>"#);
}

// ---------------------------------------------------------- import/export

fn import_tab(app: &mut App, key: &str, t: usize, h: &mut String) {
    let arg = format!("{}:{}", key, t);
    h.push_str("<section class=\"card\"><h3>Import</h3><p class=\"small muted\">Paste rows copied from Excel or Google Sheets straight into the sheet (select a cell, Ctrl+V), or import a CSV / JSON file here. Rows whose ID already exists update that row.</p>");
    h.push_str(&format!(
        "<textarea id=\"csv-{a}\" rows=\"6\" placeholder=\"CSV with a header row, or a JSON array of objects\" data-in=\"form\" data-arg=\"csv:{a}\" aria-label=\"Import data\">{}</textarea><div class=\"row\"><button class=\"btn primary\" data-a=\"import-csv\" data-arg=\"{a}\">Import</button><label class=\"btn\">Choose file…<input type=\"file\" accept=\".csv,.tsv,.json,text/csv,application/json\" data-file=\"import-file\" data-arg=\"{a}\" hidden></label></div></section>",
        esc(form(app, &format!("csv:{}", arg))),
        a = esc(&arg)
    ));
    h.push_str("<section class=\"card\"><h3>Export</h3><p class=\"small muted\">Everything in the sheet, including unsaved changes.</p><div class=\"row\"><button class=\"btn\" data-a=\"sheet-export\" data-arg=\"csv\">Download CSV</button><button class=\"btn\" data-a=\"sheet-export\" data-arg=\"json\">Download JSON</button></div></section>");
}

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
    let mut lines: Vec<String> = vec![];
    if d.root_sig.is_none() {
        lines.push(format!("Create the database “{}” · {}", esc(&d.name), ui::sol(iq::DB_ROOT_COST_ESTIMATE)));
    }
    let new_tables: Vec<&str> = d.tables.iter().filter(|t| t.created.is_none()).map(|t| t.name.as_str()).collect();
    if !new_tables.is_empty() {
        lines.push(format!("Create table(s) {} · {} each", esc(&new_tables.join(", ")), ui::sol(iq::TABLE_COST_ESTIMATE)));
    }
    if d.user_init_sig.is_none() {
        lines.push(format!("One-time setup for this database's wallet · {}", ui::sol(iq::USER_INIT_RENT_ESTIMATE)));
    }
    for (t, tb) in d.tables.iter().enumerate() {
        match &plans[t] {
            Ok(p) if !p.is_empty() => {
                let recs: usize = p.iter().map(|x| x.count).sum();
                lines.push(format!(
                    "<b>{}</b>: {} changed row(s) in {} write(s) · {}",
                    esc(&tb.name),
                    recs,
                    p.len(),
                    ui::sol(p.len() as u64 * (iq::FEE_DIRECT_WRITE + iq::TX_FEE))
                ));
            }
            Err(e) => lines.push(format!("<span class=\"warn\"><b>{}</b>: {}</span>", esc(&tb.name), esc(e))),
            _ => {}
        }
    }
    let nothing = lines.is_empty();
    let running = app.run.as_ref().map(|r| r.busy()).unwrap_or(false);
    h.push_str("<section class=\"card savecard\">");
    if nothing {
        h.push_str("<h3>Everything is saved</h3><p class=\"muted\">Edit the sheet and your changes will be waiting here.</p>");
    } else {
        h.push_str(&format!("<h3>Save to the blockchain</h3><p class=\"big\">≈ {}</p><ul class=\"costs\">", ui::sol(cost)));
        for l in &lines {
            h.push_str(&format!("<li>{}</li>", l));
        }
        h.push_str("</ul>");
        h.push_str("<p class=\"small muted\">Saved data is public and permanent. Most of the cost is a deposit the blockchain keeps for the new accounts; the rest are small fees (0.001 SOL per write to IQ Labs).</p>");
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
                h.push_str(&format!(
                    "<div class=\"row\"><button class=\"btn primary big\" data-a=\"inscribe\" data-arg=\"{}\" {}>{}</button>{}</div>",
                    esc(key),
                    if running || app.busy_note.is_some() { "disabled" } else { "" },
                    if packs > 0 { format!("Save {} change(s) · ≈{}", plans.iter().filter_map(|p| p.as_ref().ok()).map(|p| p.iter().map(|x| x.count).sum::<usize>()).sum::<usize>(), ui::sol(cost)) } else { format!("Save · ≈{}", ui::sol(cost)) },
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
    // advanced
    h.push_str("<details class=\"adv\"><summary>Advanced: this database's wallet and options</summary>");
    h.push_str(&format!(
        "<label class=\"check\"><input type=\"checkbox\" data-in=\"lock-creators\" data-arg=\"{}\" {} {}> Only this database's wallet may create tables in it</label>",
        esc(key),
        if d.lock_creators { "checked" } else { "" },
        if d.root_sig.is_some() { "disabled" } else { "" }
    ));
    va::draft_wallet(app, key, h);
    h.push_str("</details>");
    let _ = pretty;
    let _ = Load::<()>::None;
}
