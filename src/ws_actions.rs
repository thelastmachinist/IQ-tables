//! What the editor's phpMyAdmin-style screens do. Structure, search,
//! insert and operations all build a SQL statement and run it through the
//! same executor as the SQL console, so the rules are the same everywhere
//! and the statement is shown afterwards for anyone who wants to learn it.

use crate::app::App;
use crate::host;
use crate::json::Json;
use crate::schema::{DefVal, Ty};
use crate::sql_exec::{sql_ident as q, sql_str, Out};
use crate::ui;

/// Plain-language type choices in the column form: (id, label, SQL).
pub const TYPES: &[(&str, &str)] = &[
    ("text", "Text"),
    ("longtext", "Long text"),
    ("int", "Whole number"),
    ("decimal", "Number with decimals"),
    ("float", "Number (any size)"),
    ("bool", "Yes / No"),
    ("date", "Date"),
    ("datetime", "Date & time"),
    ("time", "Time"),
    ("enum", "Choice list"),
    ("json", "JSON"),
    ("any", "Anything"),
    ("custom", "Other SQL type…"),
];

/// Preset + parameter for an existing type (for the form).
pub fn preset_of(t: &Ty) -> (&'static str, String) {
    match t {
        Ty::Varchar(n) | Ty::Char(n) => ("text", n.to_string()),
        Ty::Text(_) => ("longtext", String::new()),
        Ty::Int(..) => ("int", String::new()),
        Ty::Decimal(_, s, _) => ("decimal", s.to_string()),
        Ty::Float(..) => ("float", String::new()),
        Ty::Bool => ("bool", String::new()),
        Ty::Date => ("date", String::new()),
        Ty::DateTime(_) | Ty::Timestamp(_) => ("datetime", String::new()),
        Ty::Time(_) => ("time", String::new()),
        Ty::Enum(v) => ("enum", v.join(", ")),
        Ty::Json => ("json", String::new()),
        Ty::Any => ("any", String::new()),
        other => ("custom", other.sql()),
    }
}

/// SQL type text from the form's preset + parameter.
pub fn type_from_form(preset: &str, param: &str) -> Result<String, String> {
    let p = param.trim();
    Ok(match preset {
        "text" => format!("VARCHAR({})", p.parse::<u32>().ok().filter(|n| *n > 0 && *n <= 16383).unwrap_or(255)),
        "longtext" => "TEXT".into(),
        "int" => "INT".into(),
        "decimal" => {
            let s = p.parse::<u32>().ok().filter(|n| *n <= 10).unwrap_or(2);
            format!("DECIMAL({},{})", 13 + s, s)
        }
        "float" => "DOUBLE".into(),
        "bool" => "BOOLEAN".into(),
        "date" => "DATE".into(),
        "datetime" => "DATETIME".into(),
        "time" => "TIME".into(),
        "enum" => {
            let v: Vec<String> = p.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()).map(sql_str).collect();
            if v.is_empty() {
                return Err("List the choices, separated by commas (e.g. small, medium, large)".into());
            }
            format!("ENUM({})", v.join(","))
        }
        "json" => "JSON".into(),
        "any" => String::new(),
        _ => {
            if p.is_empty() {
                return Err("Type the SQL type, e.g. SMALLINT UNSIGNED".into());
            }
            Ty::parse(p)?;
            p.to_string()
        }
    })
}

impl App {
    fn f(&self, k: &str) -> String {
        self.form.get(k).cloned().unwrap_or_default()
    }

    fn cur_tb(&self) -> Option<(String, usize, crate::state::DraftTable)> {
        let key = self.cur_db()?;
        let i = self.draft_idx(&key)?;
        let t = self.drafts[i].sel;
        let tb = self.drafts[i].tables.get(t)?.clone();
        Some((key, t, tb))
    }

    /// Two-click confirmation for destructive buttons (no browser dialogs).
    fn confirmed(&mut self, action: &str, arg: &str) -> bool {
        let id = format!("{}:{}", action, arg);
        if self.ed.confirm.as_deref() == Some(id.as_str()) {
            self.ed.confirm = None;
            true
        } else {
            self.ed.confirm = Some(id);
            false
        }
    }

    /// Load a column into the column form.
    fn col_form_load(&mut self, tb: &crate::state::DraftTable, c: Option<usize>) {
        for k in ["name", "type", "param", "null", "default", "ai", "comment", "place"] {
            self.form.remove(&format!("ce:{}", k));
        }
        match c {
            Some(c) => {
                let m = &tb.meta[c];
                let (p, param) = preset_of(&m.ty);
                self.form.insert("ce:name".into(), tb.columns[c].clone());
                self.form.insert("ce:type".into(), p.into());
                self.form.insert("ce:param".into(), param);
                self.form.insert("ce:null".into(), if m.not_null || c == tb.id_col { "false" } else { "true" }.into());
                self.form.insert(
                    "ce:default".into(),
                    match &m.default {
                        None => String::new(),
                        Some(DefVal::Lit(v)) => v.cell_text(),
                        Some(DefVal::Expr(e)) => {
                            if e.to_ascii_uppercase().contains("CURRENT") || e.to_ascii_uppercase().contains("NOW") {
                                "now".into()
                            } else {
                                format!("={}", e)
                            }
                        }
                    },
                );
                self.form.insert("ce:ai".into(), m.auto_inc.to_string());
                self.form.insert("ce:comment".into(), m.comment.clone());
            }
            None => {
                self.form.insert("ce:type".into(), "text".into());
                self.form.insert("ce:null".into(), "true".into());
            }
        }
    }

    /// The column definition the form describes.
    fn col_form_sql(&self) -> Result<String, String> {
        let name = self.f("ce:name").trim().to_string();
        if name.is_empty() {
            return Err("Give the column a name".into());
        }
        let preset = { let p = self.f("ce:type"); if p.is_empty() { "text".to_string() } else { p } };
        let ty = type_from_form(&preset, &self.f("ce:param"))?;
        let mut s = format!("{}{}{}", q(&name), if ty.is_empty() { "" } else { " " }, ty);
        let nullable = self.f("ce:null") != "false";
        if !nullable {
            s.push_str(" NOT NULL");
        }
        let d = self.f("ce:default").trim().to_string();
        if !d.is_empty() {
            if d.eq_ignore_ascii_case("now") || d.eq_ignore_ascii_case("today") {
                s.push_str(if preset == "date" { " DEFAULT (CURDATE())" } else { " DEFAULT CURRENT_TIMESTAMP" });
            } else if let Some(e) = d.strip_prefix('=') {
                s.push_str(&format!(" DEFAULT ({})", e));
            } else if d.eq_ignore_ascii_case("null") {
                s.push_str(" DEFAULT NULL");
            } else if matches!(preset.as_str(), "int" | "decimal" | "float") && crate::schema::numeric_text(&Json::Str(d.clone())).is_some() {
                s.push_str(&format!(" DEFAULT {}", crate::schema::numeric_text(&Json::Str(d.clone())).unwrap()));
            } else {
                s.push_str(&format!(" DEFAULT {}", sql_str(&d)));
            }
        }
        if self.f("ce:ai") == "true" {
            s.push_str(" AUTO_INCREMENT");
        }
        let c = self.f("ce:comment");
        if !c.trim().is_empty() {
            s.push_str(&format!(" COMMENT {}", sql_str(c.trim())));
        }
        Ok(s)
    }

    fn goto_table(&mut self, key: &str, t: usize, tab: &str) {
        if let Some(i) = self.draft_idx(key) {
            self.drafts[i].sel = t;
        }
        self.ed.tab = tab.into();
        self.ed.scope_db = false;
        host::set_hash(&format!("#/ws/{}/{}", key, t));
    }

    /// Editor screens' actions. None = not ours.
    pub fn ws_event(&mut self, kind: &str, action: &str, arg: &str, val: &str) -> Option<bool> {
        let _ = kind;
        let key = self.cur_db()?;
        let di = self.draft_idx(&key)?;
        let destructive = matches!(action, "op-truncate" | "op-drop" | "col-drop" | "idx-drop" | "fk-drop" | "chk-drop" | "view-drop" | "bookmark-del" | "del-draft-confirm");
        if !destructive && action != "form" {
            self.ed.confirm = None;
        }
        match action {
            // ------------------------------------------------ structure
            "col-edit" => {
                let (_, _, tb) = self.cur_tb()?;
                let c = arg.parse::<usize>().ok();
                self.col_form_load(&tb, c);
                self.ed.col_edit = Some(c.unwrap_or(usize::MAX));
            }
            "col-edit-cancel" => self.ed.col_edit = None,
            "col-edit-go" => {
                // from the sheet's column menu to the column form
                let (_, _, tb) = self.cur_tb()?;
                let c = arg.parse::<usize>().ok();
                self.col_form_load(&tb, c);
                self.ed.col_edit = Some(c.unwrap_or(usize::MAX));
                self.ed.menu = None;
                self.ed.tab = "structure".into();
            }
            "del-draft-confirm" => {
                if self.confirmed(action, arg) {
                    self.drafts.remove(di);
                    self.save_drafts();
                    host::set_hash("#/ws");
                }
            }
            "col-save" => {
                let (key, _, tb) = self.cur_tb()?;
                let def = match self.col_form_sql() {
                    Ok(d) => d,
                    Err(e) => {
                        self.err(e);
                        return Some(true);
                    }
                };
                let stmt = match self.ed.col_edit {
                    Some(c) if c < tb.columns.len() => format!("ALTER TABLE {} CHANGE {} {}", q(&tb.title), q(&tb.columns[c]), def),
                    _ => {
                        let place = match self.f("ce:place").as_str() {
                            "" | "end" => String::new(),
                            "first" => " FIRST".into(),
                            other => format!(" AFTER {}", q(other)),
                        };
                        format!("ALTER TABLE {} ADD COLUMN {}{}", q(&tb.title), def, place)
                    }
                };
                if self.run_ui_sql(&key, &stmt) {
                    self.ed.col_edit = None;
                }
            }
            "col-drop" => {
                let (key, _, tb) = self.cur_tb()?;
                let c: usize = arg.parse().ok()?;
                if self.confirmed(action, arg) {
                    self.run_ui_sql(&key, &format!("ALTER TABLE {} DROP COLUMN {}", q(&tb.title), q(tb.columns.get(c)?)));
                }
            }
            "col-pk" => {
                let (key, _, tb) = self.cur_tb()?;
                let c: usize = arg.parse().ok()?;
                self.run_ui_sql(&key, &format!("ALTER TABLE {} DROP PRIMARY KEY, ADD PRIMARY KEY ({})", q(&tb.title), q(tb.columns.get(c)?)));
            }
            "col-unique" | "col-index" => {
                let (key, _, tb) = self.cur_tb()?;
                let c: usize = arg.parse().ok()?;
                self.run_ui_sql(&key, &format!("ALTER TABLE {} ADD {} ({})", q(&tb.title), if action == "col-unique" { "UNIQUE" } else { "INDEX" }, q(tb.columns.get(c)?)));
            }
            "idx-add" => {
                let (key, _, tb) = self.cur_tb()?;
                let cols: Vec<String> = tb.columns.iter().filter(|c| self.f(&format!("ix:c:{}", c)) == "true").map(|c| q(c)).collect();
                if cols.is_empty() {
                    self.err("Tick the column(s) for the key");
                    return Some(true);
                }
                let kind = if self.f("ix:type") == "index" { "INDEX" } else { "UNIQUE" };
                if self.run_ui_sql(&key, &format!("ALTER TABLE {} ADD {} ({})", q(&tb.title), kind, cols.join(", "))) {
                    for c in &tb.columns {
                        self.form.remove(&format!("ix:c:{}", c));
                    }
                }
            }
            "idx-drop" => {
                let (key, _, tb) = self.cur_tb()?;
                if self.confirmed(action, arg) {
                    self.run_ui_sql(&key, &format!("ALTER TABLE {} DROP INDEX {}", q(&tb.title), q(arg)));
                }
            }
            "fk-add" => {
                let (key, _, tb) = self.cur_tb()?;
                let col = self.f("fk:col");
                // "table" (its ID column) or "table<TAB>column"
                let target = self.f("fk:target");
                let mut it = target.splitn(2, '\t');
                let rt = it.next().unwrap_or("").to_string();
                let rc = it.next().unwrap_or("").to_string();
                if col.is_empty() || rt.is_empty() {
                    self.err("Pick the column and the table it points to");
                    return Some(true);
                }
                let del = match self.f("fk:del").as_str() {
                    "cascade" => " ON DELETE CASCADE",
                    "setnull" => " ON DELETE SET NULL",
                    _ => "",
                };
                let stmt = format!("ALTER TABLE {} ADD FOREIGN KEY ({}) REFERENCES {}{}{}", q(&tb.title), q(&col), q(&rt), if rc.is_empty() { String::new() } else { format!(" ({})", q(&rc)) }, del);
                if self.run_ui_sql(&key, &stmt) {
                    for k in ["fk:col", "fk:target", "fk:del"] {
                        self.form.remove(k);
                    }
                }
            }
            "fk-drop" => {
                let (key, _, tb) = self.cur_tb()?;
                if self.confirmed(action, arg) {
                    self.run_ui_sql(&key, &format!("ALTER TABLE {} DROP FOREIGN KEY {}", q(&tb.title), q(arg)));
                }
            }
            "chk-add" => {
                let (key, _, tb) = self.cur_tb()?;
                let e = self.f("ck:expr");
                if e.trim().is_empty() {
                    self.err("Write the rule, e.g. qty >= 0");
                    return Some(true);
                }
                if self.run_ui_sql(&key, &format!("ALTER TABLE {} ADD CHECK ({})", q(&tb.title), e.trim())) {
                    self.form.remove("ck:expr");
                }
            }
            "chk-drop" => {
                let (key, _, tb) = self.cur_tb()?;
                if self.confirmed(action, arg) {
                    self.run_ui_sql(&key, &format!("ALTER TABLE {} DROP CHECK {}", q(&tb.title), q(arg)));
                }
            }
            // --------------------------------------------------- search
            "search-run" => {
                let (key, _, tb) = self.cur_tb()?;
                let mut conds = vec![];
                for c in &tb.columns {
                    let op = self.f(&format!("sq:op:{}", c));
                    let v = self.f(&format!("sq:v:{}", c));
                    let col = q(c);
                    let lit = |x: &str| {
                        let t = x.trim();
                        if crate::schema::numeric_text(&Json::Str(t.to_string())).is_some() && !t.is_empty() && !t.starts_with('0') || t == "0" {
                            t.to_string()
                        } else {
                            sql_str(t)
                        }
                    };
                    let cond = match op.as_str() {
                        "" => {
                            if v.trim().is_empty() {
                                continue;
                            }
                            format!("{} LIKE {}", col, sql_str(&format!("%{}%", v.trim())))
                        }
                        "like" => format!("{} LIKE {}", col, sql_str(&format!("%{}%", v.trim()))),
                        "notlike" => format!("{} NOT LIKE {}", col, sql_str(&format!("%{}%", v.trim()))),
                        "eq" => format!("{} = {}", col, lit(&v)),
                        "ne" => format!("{} != {}", col, lit(&v)),
                        "lt" => format!("{} < {}", col, lit(&v)),
                        "gt" => format!("{} > {}", col, lit(&v)),
                        "le" => format!("{} <= {}", col, lit(&v)),
                        "ge" => format!("{} >= {}", col, lit(&v)),
                        "in" => format!("{} IN ({})", col, v.split(',').map(|x| lit(x)).collect::<Vec<_>>().join(", ")),
                        "between" => {
                            let mut it = v.splitn(2, |ch| ch == ',' || ch == '-' && false);
                            let a = it.next().unwrap_or("");
                            let b = it.next().unwrap_or("");
                            format!("{} BETWEEN {} AND {}", col, lit(a), lit(b))
                        }
                        "null" => format!("{} IS NULL", col),
                        "notnull" => format!("{} IS NOT NULL", col),
                        "regexp" => format!("{} REGEXP {}", col, sql_str(v.trim())),
                        "starts" => format!("{} LIKE {}", col, sql_str(&format!("{}%", v.trim()))),
                        _ => continue,
                    };
                    conds.push(cond);
                }
                let joiner = if self.f("sq:any") == "true" { " OR " } else { " AND " };
                let stmt = format!("SELECT * FROM {}{} LIMIT 1000", q(&tb.title), if conds.is_empty() { String::new() } else { format!(" WHERE {}", conds.join(joiner)) });
                self.ed.search_out = self.run_sql(&key, &stmt);
                self.ed.last_sql = Some(stmt);
            }
            "search-clear" => {
                let keys: Vec<String> = self.form.keys().filter(|k| k.starts_with("sq:")).cloned().collect();
                for k in keys {
                    self.form.remove(&k);
                }
                self.ed.search_out.clear();
            }
            "replace-run" => {
                let (key, _, tb) = self.cur_tb()?;
                let col = self.f("rp:col");
                let find = self.f("rp:find");
                if col.is_empty() || find.is_empty() {
                    self.err("Pick a column and the text to find");
                    return Some(true);
                }
                let stmt = format!("UPDATE {t} SET {c} = REPLACE({c}, {f}, {w}) WHERE {c} LIKE {l}", t = q(&tb.title), c = q(&col), f = sql_str(&find), w = sql_str(&self.f("rp:with")), l = sql_str(&format!("%{}%", find.replace('%', "\\%").replace('_', "\\_"))));
                self.run_ui_sql(&key, &stmt);
            }
            "dbsearch-run" => {
                let text = self.f("dbq").trim().to_string();
                if text.is_empty() {
                    return Some(true);
                }
                let tables: Vec<(usize, String, Vec<String>)> = self.drafts[di].tables.iter().enumerate().filter(|(_, t)| !t.dropped && !t.is_system()).map(|(i, t)| (i, t.title.clone(), t.columns.clone())).collect();
                let pat = sql_str(&format!("%{}%", text));
                let mut rows = vec![];
                for (i, title, cols) in tables {
                    let cond = cols.iter().map(|c| format!("{} LIKE {}", q(c), pat)).collect::<Vec<_>>().join(" OR ");
                    let out = self.run_sql(&key, &format!("SELECT COUNT(*) FROM {} WHERE {}", q(&title), cond));
                    let n = out.iter().find_map(|o| if let Out::Rows { rows, .. } = o { rows.first().and_then(|r| r.first()).map(|v| v.cell_text()) } else { None }).unwrap_or_else(|| "0".into());
                    rows.push(vec![Json::Str(title), Json::Str(n), Json::Str(format!("{}", i))]);
                }
                self.ed.search_out = vec![Out::Rows { title: "__dbsearch".into(), cols: vec!["Table".into(), "Matching rows".into(), "".into()], rows, note: format!("Rows containing “{}” anywhere", text) }];
            }
            "dbsearch-browse" => {
                let t: usize = arg.parse().ok()?;
                let text = self.f("dbq").trim().to_string();
                self.pending_filter = Some(text);
                self.goto_table(&key, t, "browse");
            }
            // --------------------------------------------------- insert
            "insert-row" => {
                let (key, _, tb) = self.cur_tb()?;
                let mut cols = vec![];
                let mut vals = vec![];
                for (c, name) in tb.columns.iter().enumerate() {
                    let v = self.f(&format!("in:{}", c));
                    let m = &tb.meta[c];
                    if m.ty == Ty::Bool {
                        if v.is_empty() {
                            continue;
                        }
                        cols.push(q(name));
                        vals.push(if v == "true" { "TRUE".to_string() } else { "FALSE".to_string() });
                        continue;
                    }
                    if v.trim().is_empty() {
                        continue;
                    }
                    cols.push(q(name));
                    vals.push(if m.ty.is_numeric() && crate::schema::numeric_text(&Json::Str(v.clone())).is_some() { crate::schema::numeric_text(&Json::Str(v.clone())).unwrap() } else { sql_str(&v.replace('T', if matches!(m.ty, Ty::DateTime(_) | Ty::Timestamp(_)) { " " } else { "T" })) });
                }
                let stmt = if cols.is_empty() { format!("INSERT INTO {} () VALUES ()", q(&tb.title)) } else { format!("INSERT INTO {} ({}) VALUES ({})", q(&tb.title), cols.join(", "), vals.join(", ")) };
                if self.run_ui_sql(&key, &stmt) {
                    for c in 0..tb.columns.len() {
                        self.form.remove(&format!("in:{}", c));
                    }
                }
            }
            // --------------------------------------------------- export
            "export" => {
                // everything exported must have been read from the chain first
                let need: Vec<usize> = match arg {
                    "db-sql" | "db-sql-structure" => (0..self.drafts[di].tables.len()).filter(|&t| !self.drafts[di].tables[t].dropped).collect(),
                    _ => vec![self.drafts[di].sel],
                };
                let mut waiting = vec![];
                for t in need {
                    self.ensure_base(&key, t);
                    if matches!(self.sheet_base(&key, t).1, crate::editor::BaseState::Loading) {
                        waiting.push(self.drafts[di].tables[t].title.clone());
                    }
                }
                if !waiting.is_empty() {
                    self.err(format!("Still reading {} from the blockchain — try again in a moment", waiting.join(", ")));
                    return Some(true);
                }
                let safe = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect::<String>();
                let dname = self.drafts[di].name.clone();
                match arg {
                    "csv" | "json" => {
                        let (key, t, _) = self.cur_tb()?;
                        self.export_rows(&key, t, arg);
                    }
                    "sql" | "sql-structure" | "sql-data" => {
                        let (key, t, tb) = self.cur_tb()?;
                        let s = self.dump_sql(&key, Some(t), arg != "sql-data", arg != "sql-structure");
                        host::download(&format!("{}-{}.sql", safe(&dname), safe(&tb.title)), "application/sql", s.as_bytes());
                    }
                    "db-sql" | "db-sql-structure" => {
                        let s = self.dump_sql(&key, None, true, arg == "db-sql");
                        host::download(&format!("{}.sql", safe(&dname)), "application/sql", s.as_bytes());
                    }
                    _ => {}
                }
            }
            // --------------------------------------------------- import
            "import-sql" => {
                let text = self.f("sqlimp");
                if text.trim().is_empty() {
                    self.err("Paste SQL or choose a .sql file");
                    return Some(true);
                }
                self.import_sql(&key, &text);
            }
            // ----------------------------------------------- operations
            "op-rename" => {
                let (key, _, tb) = self.cur_tb()?;
                let n = self.f("op:name");
                if !n.trim().is_empty() && self.run_ui_sql(&key, &format!("RENAME TABLE {} TO {}", q(&tb.title), q(n.trim()))) {
                    self.form.remove("op:name");
                }
            }
            "op-comment" => {
                let (key, _, tb) = self.cur_tb()?;
                let c = self.f("op:comment");
                self.run_ui_sql(&key, &format!("ALTER TABLE {} COMMENT = {}", q(&tb.title), sql_str(c.trim())));
            }
            "op-copy" => {
                let (key, _, tb) = self.cur_tb()?;
                let n = self.f("op:copy").trim().to_string();
                if n.is_empty() {
                    self.err("Name the copy");
                    return Some(true);
                }
                let mut stmt = format!("CREATE TABLE {} LIKE {}", q(&n), q(&tb.title));
                if self.f("op:copydata") != "false" {
                    stmt.push_str(&format!("; INSERT INTO {} SELECT * FROM {}", q(&n), q(&tb.title)));
                }
                if self.run_ui_sql(&key, &stmt) {
                    self.form.remove("op:copy");
                    if let Ok(t) = self.tbl(&key, &n) {
                        self.goto_table(&key, t, "browse");
                    }
                }
            }
            "op-truncate" => {
                if let Ok(t) = arg.parse::<usize>() {
                    self.drafts[di].sel = t;
                }
                let (key, _, tb) = self.cur_tb()?;
                if self.confirmed(action, arg) {
                    self.run_ui_sql(&key, &format!("TRUNCATE TABLE {}", q(&tb.title)));
                }
            }
            "op-drop" => {
                if let Ok(t) = arg.parse::<usize>() {
                    self.drafts[di].sel = t;
                }
                let (key, _, tb) = self.cur_tb()?;
                if self.confirmed(action, arg) && self.run_ui_sql(&key, &format!("DROP TABLE {}", q(&tb.title))) {
                    self.ed.tab = "structure".into();
                    self.ed.scope_db = true;
                    host::set_hash(&format!("#/ws/{}", key));
                }
            }
            "op-access" => {
                let (key, _, tb) = self.cur_tb()?;
                let stmt = if arg == "open" { format!("GRANT INSERT ON {} TO PUBLIC", q(&tb.title)) } else { format!("REVOKE INSERT ON {} FROM PUBLIC", q(&tb.title)) };
                self.run_ui_sql(&key, &stmt);
            }
            "op-writer-add" => {
                let (key, _, tb) = self.cur_tb()?;
                let w = self.f("op:writer").trim().to_string();
                if !w.is_empty() && self.run_ui_sql(&key, &format!("GRANT INSERT ON {} TO {}", q(&tb.title), sql_str(&w))) {
                    self.form.remove("op:writer");
                }
            }
            "op-writer-del" => {
                let (key, _, tb) = self.cur_tb()?;
                self.run_ui_sql(&key, &format!("REVOKE INSERT ON {} FROM {}", q(&tb.title), sql_str(arg)));
            }
            "view-drop" => {
                if self.confirmed(action, arg) {
                    self.run_ui_sql(&key, &format!("DROP VIEW {}", q(arg)));
                }
            }
            "view-create" => {
                let name = self.f("vw:name").trim().to_string();
                let body = self.f("vw:sql").trim().trim_end_matches(';').to_string();
                if name.is_empty() || body.is_empty() {
                    self.err("Name the view and write its SELECT");
                    return Some(true);
                }
                if self.run_ui_sql(&key, &format!("CREATE VIEW {} AS {}", q(&name), body)) {
                    self.form.remove("vw:name");
                    self.form.remove("vw:sql");
                }
            }
            "view-from-sql" => {
                // make a view from the query in the SQL box
                let text = self.form.get(&format!("sql:{}", key)).cloned().or_else(|| self.ed.sql_text.get(&key).cloned()).unwrap_or_default();
                self.form.insert("vw:sql".into(), text.trim().trim_end_matches(';').to_string());
                self.ed.scope_db = true;
                self.ed.tab = "structure".into();
                host::set_hash(&format!("#/ws/{}", key));
            }
            "sql-open" => {
                self.ed.sql_text.insert(key.clone(), arg.to_string());
                self.form.remove(&format!("sql:{}", key));
                self.ed.tab = "sql".into();
            }
            "import-sql-file" => self.import_sql(&key, val),
            "import-csv-new" => {
                let stem = arg.rsplit_once('.').map(|(a, _)| a).unwrap_or(arg);
                let mut base: String = stem.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>().trim_matches('_').chars().take(48).collect();
                if base.is_empty() {
                    base = "imported".into();
                }
                let taken = |n: &str, d: &crate::state::Draft| d.tables.iter().any(|t| t.name.eq_ignore_ascii_case(n) || t.title.eq_ignore_ascii_case(n));
                let mut name = base.clone();
                let mut i = 2;
                while taken(&name, &self.drafts[di]) {
                    name = format!("{}_{}", base, i);
                    i += 1;
                }
                self.drafts[di].tables.push(crate::state::DraftTable::starter(&name, &[]));
                let t = self.drafts[di].tables.len() - 1;
                self.import_text(&format!("{}:{}", key, t), val);
                if self.drafts[di].tables[t].rows.is_empty() {
                    self.drafts[di].tables.pop();
                    self.table_removed(&key, t);
                    self.save_drafts();
                } else {
                    self.goto_table(&key, t, "browse");
                }
            }
            "view-open" => {
                let stmt = format!("SELECT * FROM {} LIMIT 100;", q(arg));
                self.ed.sql_text.insert(key.clone(), stmt.clone());
                self.ed.sql_out = self.run_sql(&key, &stmt);
                self.ed.tab = "sql".into();
            }
            "db-create-table" => {
                let name = self.f("ct:name").trim().to_string();
                if name.is_empty() {
                    self.err("Name the table");
                    return Some(true);
                }
                let cols: Vec<String> = self.f("ct:cols").split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("id")).collect();
                let mut defs = vec!["`id` INT AUTO_INCREMENT PRIMARY KEY".to_string()];
                defs.extend(cols.iter().map(|c| q(c)));
                if cols.is_empty() {
                    defs.push("`name` VARCHAR(255)".into());
                }
                let stmt = format!("CREATE TABLE {} ({}){}", q(&name), defs.join(", "), if self.f("ct:open") == "open" { " OPEN" } else { "" });
                if self.run_ui_sql(&key, &stmt) {
                    for k in ["ct:name", "ct:cols", "ct:open"] {
                        self.form.remove(k);
                    }
                    if let Ok(t) = self.tbl(&key, &name) {
                        self.goto_table(&key, t, "structure");
                    }
                }
            }
            "goto-tab" => {
                let mut it = arg.splitn(2, ':');
                let t: usize = it.next()?.parse().ok()?;
                let tab = it.next().unwrap_or("browse").to_string();
                self.goto_table(&key, t, &tab);
            }
            // ------------------------------------------------------ SQL
            "bookmark-save" => {
                let name = self.f("bm:name").trim().to_string();
                let text = self.form.get(&format!("sql:{}", key)).cloned().or_else(|| self.ed.sql_text.get(&key).cloned()).unwrap_or_default();
                if name.is_empty() || text.trim().is_empty() {
                    self.err("Name the query (and write one) to save it");
                    return Some(true);
                }
                self.drafts[di].bookmarks.retain(|(n, _)| *n != name);
                self.drafts[di].bookmarks.push((name.clone(), text));
                self.form.remove("bm:name");
                self.save_drafts();
                self.ok(format!("Saved “{}” in this browser", name));
            }
            "bookmark-load" => {
                let i: usize = arg.parse().ok()?;
                let text = self.drafts[di].bookmarks.get(i)?.1.clone();
                self.ed.sql_text.insert(key.clone(), text);
                self.form.remove(&format!("sql:{}", key));
            }
            "bookmark-del" => {
                let i: usize = arg.parse().ok()?;
                if self.confirmed(action, arg) && i < self.drafts[di].bookmarks.len() {
                    self.drafts[di].bookmarks.remove(i);
                    self.save_drafts();
                }
            }
            "sql-csv" => {
                let Some(Out::Rows { cols, rows, title, .. }) = self.ed.sql_out.iter().rev().find(|o| matches!(o, Out::Rows { .. })).cloned() else { return Some(true) };
                let mut out = String::from("\u{feff}");
                out.push_str(&cols.iter().map(|c| ui::csv_cell(c)).collect::<Vec<_>>().join(","));
                out.push('\n');
                for r in rows {
                    out.push_str(&r.iter().map(|v| ui::csv_cell(&v.cell_text())).collect::<Vec<_>>().join(","));
                    out.push('\n');
                }
                host::download(&format!("{}.csv", if title.is_empty() { "query".to_string() } else { title }), "text/csv", out.as_bytes());
            }
            "sql-edit" => {
                let mut it = arg.splitn(2, ':');
                let t: usize = it.next()?.parse().ok()?;
                let id = it.next().unwrap_or("").to_string();
                self.pending_filter = Some(id);
                self.ed.table = (String::new(), usize::MAX);
                self.goto_table(&key, t, "browse");
            }
            "ws-scope" => {
                self.ed.scope_db = arg == "db";
                self.ed.tab = if arg == "db" { "structure".into() } else { "browse".into() };
                host::set_hash(&if arg == "db" { format!("#/ws/{}", key) } else { format!("#/ws/{}/{}", key, self.drafts[di].sel) });
            }
            _ => return None,
        }
        let _ = val;
        Some(true)
    }

    /// Run a SQL file / pasted script into the database (a dump, a schema).
    pub fn import_sql(&mut self, key: &str, text: &str) {
        let out = self.run_sql_opts(key, text, true);
        let bad = out.iter().filter(|o| matches!(o, Out::Msg(false, _))).count();
        self.ed.import_out = out;
        if bad == 0 {
            self.form.remove("sqlimp");
            self.ok("SQL imported — nothing is saved on the blockchain until you press Save");
        } else {
            self.err("The import stopped at an error — see below. Everything before it was applied (Undo in each table, or ROLLBACK, takes it back).");
        }
    }

    /// CSV (Excel-friendly) or JSON of a table, unsaved changes included.
    pub fn export_rows(&mut self, key: &str, t: usize, fmt: &str) {
        let rows = self.sheet_rows(key, t);
        let Some(tb) = self.draft_idx(key).and_then(|i| self.drafts[i].tables.get(t)).cloned() else { return };
        let live: Vec<&crate::sheet::SRow> = rows.iter().filter(|r| r.state != crate::sheet::RowState::Deleted).collect();
        let name: String = tb.title.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        if fmt == "json" {
            let arr: Vec<Json> = live.iter().map(|r| Json::Obj(tb.columns.iter().cloned().zip(r.vals.iter().cloned()).collect())).collect();
            host::download(&format!("{}.json", name), "application/json", Json::Arr(arr).to_string().as_bytes());
        } else {
            let mut out = String::from("\u{feff}");
            out.push_str(&tb.columns.iter().map(|c| ui::csv_cell(c)).collect::<Vec<_>>().join(","));
            out.push('\n');
            for r in &live {
                out.push_str(&r.vals.iter().enumerate().map(|(c, v)| ui::csv_cell(&tb.meta.get(c).map(|m| if m.ty == Ty::Bool { v.cell_text() } else { m.ty.show(v) }).unwrap_or_else(|| v.cell_text()))).collect::<Vec<_>>().join(","));
                out.push('\n');
            }
            host::download(&format!("{}.csv", name), "text/csv", out.as_bytes());
        }
    }
}
