//! Running SQL against a draft database: reads over the sheet (saved +
//! unsaved), writes staged as unsaved edits, COMMIT = save to the blockchain.

use std::collections::HashMap;

use crate::app::App;
use crate::json::{self, Json};
use crate::sheet::{self, RowState, SRow};
use crate::sql::{self, Expr, Item, Stmt};
use crate::state::{DraftTable, GhostRow};
use crate::ui;

#[derive(Clone, Debug)]
pub enum Out {
    Rows { title: String, cols: Vec<String>, rows: Vec<Vec<Json>>, note: String },
    Msg(bool, String),
}

type Getter<'a> = Box<dyn Fn(&str) -> Result<Json, String> + 'a>;

fn getter<'a>(cols: &'a [String], vals: &'a [Json]) -> Getter<'a> {
    Box::new(move |name: &str| {
        cols.iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .map(|p| vals.get(p).cloned().unwrap_or(Json::Null))
            .ok_or_else(|| format!("Unknown column `{}` (columns: {})", name, cols.join(", ")))
    })
}

fn no_cols(name: &str) -> Result<Json, String> {
    Err(format!("`{}` isn't allowed here (no row to read it from)", name))
}

fn truthy(v: &Json) -> bool {
    match v {
        Json::Bool(b) => *b,
        Json::Null => false,
        Json::Num(n) => n.parse::<f64>().map(|f| f != 0.0).unwrap_or(false),
        Json::Str(s) => !s.is_empty(),
        _ => true,
    }
}

const SHOW_MAX: usize = 1000;

impl App {
    fn table_idx(&self, key: &str, name: &str) -> Result<usize, String> {
        let d = &self.drafts[self.draft_idx(key).ok_or("No such database")?];
        d.tables.iter().position(|t| t.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
            format!(
                "No table `{}` in {}. Tables: {}",
                name,
                d.name,
                if d.tables.is_empty() { "none yet".to_string() } else { d.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ") }
            )
        })
    }

    /// Parse and run a script. Tables that live on the blockchain are read
    /// first; the script then runs once their records arrive.
    pub fn run_sql(&mut self, key: &str, src: &str) -> Vec<Out> {
        let stmts = match sql::parse(src) {
            Ok(s) => s,
            Err(e) => return vec![Out::Msg(false, e)],
        };
        let mut loading = vec![];
        for s in &stmts {
            if !s.reads_rows() {
                continue;
            }
            if let Some(t) = s.table().and_then(|n| self.table_idx(key, n).ok()) {
                self.ensure_base(key, t);
                if matches!(self.sheet_base(key, t).1, crate::editor::BaseState::Loading) {
                    loading.push(self.drafts[self.draft_idx(key).unwrap()].tables[t].name.clone());
                }
            }
        }
        if !loading.is_empty() {
            self.ed.sql_wait = Some((key.to_string(), src.to_string()));
            return vec![Out::Msg(true, format!("Reading {} from the blockchain… the query runs as soon as it's loaded.", loading.join(", ")))];
        }
        let mut out = vec![];
        for s in stmts {
            match self.exec(key, s) {
                Ok(o) => out.push(o),
                Err(e) => {
                    out.push(Out::Msg(false, e));
                    break;
                }
            }
        }
        out
    }

    fn pending_note(rows: &[SRow]) -> String {
        let (n, c, d) = sheet::pending(rows);
        if n + c + d == 0 {
            String::new()
        } else {
            " · includes unsaved changes".into()
        }
    }

    fn exec(&mut self, key: &str, s: Stmt) -> Result<Out, String> {
        match s {
            Stmt::Select { distinct, items, table, filter, group, order, limit, offset } => {
                let t = self.table_idx(key, &table)?;
                let rows = self.sheet_rows(key, t);
                let tb = self.drafts[self.draft_idx(key).unwrap()].tables[t].clone();
                let cols = tb.columns.clone();
                let live: Vec<&SRow> = rows.iter().filter(|r| r.state != RowState::Deleted).collect();
                // WHERE
                let mut picked: Vec<&SRow> = vec![];
                for r in &live {
                    let g = getter(&cols, &r.vals);
                    let keep = match &filter {
                        Some(f) => truthy(&sql::eval(f, &*g, None)?),
                        None => true,
                    };
                    if keep {
                        picked.push(r);
                    }
                }
                let agg = !group.is_empty() || items.iter().any(|i| matches!(i, Item::Expr(e, _) if sql::is_aggregate(e)));
                // output columns
                let mut heads: Vec<String> = vec![];
                for it in &items {
                    match it {
                        Item::Star => heads.extend(cols.iter().cloned()),
                        Item::Expr(e, a) => heads.push(a.clone().unwrap_or_else(|| sql::label(e))),
                    }
                }
                let mut result: Vec<(Vec<Json>, Vec<Json>)> = vec![]; // (row, sort keys)
                let alias_of = |e: &Expr, out: &[Json]| -> Option<Json> {
                    if let Expr::Col(c) = e {
                        heads.iter().position(|h| h.eq_ignore_ascii_case(c)).map(|p| out[p].clone())
                    } else {
                        None
                    }
                };
                if agg {
                    let mut groups: Vec<(String, Vec<&SRow>)> = vec![];
                    let mut idx: HashMap<String, usize> = HashMap::new();
                    for r in &picked {
                        let g = getter(&cols, &r.vals);
                        let mut k = String::new();
                        for e in &group {
                            k.push_str(&sql::eval(e, &*g, None)?.cell_text().to_lowercase());
                            k.push('\u{1}');
                        }
                        match idx.get(&k) {
                            Some(&p) => groups[p].1.push(r),
                            None => {
                                idx.insert(k.clone(), groups.len());
                                groups.push((k, vec![r]));
                            }
                        }
                    }
                    if groups.is_empty() && group.is_empty() {
                        groups.push((String::new(), vec![]));
                    }
                    for (_, members) in &groups {
                        let getters: Vec<Getter> = members.iter().map(|r| getter(&cols, &r.vals)).collect();
                        let refs: Vec<&dyn Fn(&str) -> Result<Json, String>> = getters.iter().map(|g| &**g as &dyn Fn(&str) -> Result<Json, String>).collect();
                        let empty = vec![Json::Null; cols.len()];
                        let first = getter(&cols, members.first().map(|r| r.vals.as_slice()).unwrap_or(&empty));
                        let mut out = vec![];
                        for it in &items {
                            match it {
                                Item::Star => {
                                    for c in &cols {
                                        out.push(first(c)?);
                                    }
                                }
                                Item::Expr(e, _) => out.push(sql::eval(e, &*first, Some(&refs))?),
                            }
                        }
                        let mut keys = vec![];
                        for (e, _) in &order {
                            keys.push(match alias_of(e, &out) {
                                Some(v) => v,
                                None => sql::eval(e, &*first, Some(&refs))?,
                            });
                        }
                        result.push((out, keys));
                    }
                } else {
                    for r in &picked {
                        let g = getter(&cols, &r.vals);
                        let mut out = vec![];
                        for it in &items {
                            match it {
                                Item::Star => out.extend(r.vals.iter().cloned()),
                                Item::Expr(e, _) => out.push(sql::eval(e, &*g, None)?),
                            }
                        }
                        let mut keys = vec![];
                        for (e, _) in &order {
                            keys.push(match alias_of(e, &out) {
                                Some(v) => v,
                                None => sql::eval(e, &*g, None)?,
                            });
                        }
                        result.push((out, keys));
                    }
                }
                if !order.is_empty() {
                    result.sort_by(|a, b| {
                        for (i, (_, desc)) in order.iter().enumerate() {
                            let o = match (a.1[i].is_null(), b.1[i].is_null()) {
                                (true, true) => std::cmp::Ordering::Equal,
                                (true, false) => std::cmp::Ordering::Less,
                                (false, true) => std::cmp::Ordering::Greater,
                                _ => sql::compare(&a.1[i], &b.1[i]).unwrap_or(std::cmp::Ordering::Equal),
                            };
                            let o = if *desc { o.reverse() } else { o };
                            if o != std::cmp::Ordering::Equal {
                                return o;
                            }
                        }
                        std::cmp::Ordering::Equal
                    });
                }
                let mut out: Vec<Vec<Json>> = result.into_iter().map(|r| r.0).collect();
                if distinct {
                    let mut seen = std::collections::HashSet::new();
                    out.retain(|r| seen.insert(r.iter().map(|v| v.cell_text().to_lowercase()).collect::<Vec<_>>().join("\u{1}")));
                }
                let total = out.len();
                let out: Vec<Vec<Json>> = out.into_iter().skip(offset).take(limit.unwrap_or(usize::MAX)).collect();
                let shown = out.len().min(SHOW_MAX);
                let note = format!(
                    "{} row(s){}{}",
                    out.len(),
                    if total != out.len() { format!(" of {}", total) } else { String::new() },
                    Self::pending_note(&rows)
                ) + if out.len() > SHOW_MAX { " · first 1,000 shown" } else { "" };
                Ok(Out::Rows { title: table, cols: heads, rows: out.into_iter().take(shown).collect(), note })
            }
            Stmt::Insert { table, cols: names, rows: tuples } => {
                let t = self.table_idx(key, &table)?;
                let tb = self.drafts[self.draft_idx(key).unwrap()].tables[t].clone();
                let map: Vec<usize> = match &names {
                    Some(ns) => ns
                        .iter()
                        .map(|n| tb.columns.iter().position(|c| c.eq_ignore_ascii_case(n)).ok_or_else(|| format!("Unknown column `{}` in {}", n, tb.name)))
                        .collect::<Result<_, _>>()?,
                    None => (0..tb.columns.len()).collect(),
                };
                let existing: std::collections::HashSet<String> = self
                    .sheet_rows(key, t)
                    .iter()
                    .filter(|r| r.state != RowState::Deleted)
                    .map(|r| r.vals.get(tb.id_col).map(|v| v.cell_text()).unwrap_or_default())
                    .collect();
                let mut new_rows = vec![];
                let mut ids = std::collections::HashSet::new();
                for tup in &tuples {
                    if tup.len() != map.len() {
                        return Err(format!("{} value(s) for {} column(s)", tup.len(), map.len()));
                    }
                    let mut vals = vec![Json::Null; tb.columns.len()];
                    for (e, &c) in tup.iter().zip(&map) {
                        vals[c] = sql::eval(e, &no_cols, None)?;
                    }
                    let id = vals[tb.id_col].cell_text();
                    if id.is_empty() {
                        return Err(format!("Every row needs a value in the ID column `{}`", tb.columns[tb.id_col]));
                    }
                    if existing.contains(&id) || !ids.insert(id.clone()) {
                        return Err(format!("A row with {} = '{}' already exists — use UPDATE to change it", tb.columns[tb.id_col], id));
                    }
                    new_rows.push(vals);
                }
                let n = new_rows.len();
                self.edit_tb(key, t, |tb, _| {
                    for vals in new_rows {
                        tb.rows.push(GhostRow { vals, deleted: false, sig: None });
                    }
                });
                Ok(Out::Msg(true, format!("{} row(s) added to {} (not saved yet — COMMIT to save)", n, tb.name)))
            }
            Stmt::Update { table, sets, filter } => {
                let t = self.table_idx(key, &table)?;
                let rows = self.sheet_rows(key, t);
                let tb = self.drafts[self.draft_idx(key).unwrap()].tables[t].clone();
                let cols = tb.columns.clone();
                let targets: Vec<(usize, &Expr)> = sets
                    .iter()
                    .map(|(c, e)| cols.iter().position(|x| x.eq_ignore_ascii_case(c)).map(|p| (p, e)).ok_or_else(|| format!("Unknown column `{}` in {}", c, tb.name)))
                    .collect::<Result<_, _>>()?;
                let mut updates = vec![];
                for r in rows.iter().filter(|r| r.state != RowState::Deleted) {
                    let g = getter(&cols, &r.vals);
                    if let Some(f) = &filter {
                        if !truthy(&sql::eval(f, &*g, None)?) {
                            continue;
                        }
                    }
                    let mut vals = r.vals.clone();
                    for (c, e) in &targets {
                        vals[*c] = sql::eval(e, &*g, None)?;
                    }
                    if vals != r.vals {
                        updates.push((r.clone(), vals));
                    }
                }
                let n = updates.len();
                let errs = self.edit_tb(key, t, |tb, _| sheet::apply_rows(tb, updates)).unwrap_or_default();
                if let Some(e) = errs.first() {
                    return Err(e.clone());
                }
                Ok(Out::Msg(true, format!("{} row(s) changed in {} (not saved yet — COMMIT to save)", n, tb.name)))
            }
            Stmt::Delete { table, filter } => {
                let t = self.table_idx(key, &table)?;
                let rows = self.sheet_rows(key, t);
                let cols = self.drafts[self.draft_idx(key).unwrap()].tables[t].columns.clone();
                let mut hit = vec![];
                for r in rows.iter().filter(|r| r.state != RowState::Deleted) {
                    let g = getter(&cols, &r.vals);
                    let keep = match &filter {
                        Some(f) => truthy(&sql::eval(f, &*g, None)?),
                        None => true,
                    };
                    if keep {
                        hit.push(r);
                    }
                }
                let n = hit.len();
                let saved = hit.iter().filter(|r| r.state != RowState::New).count();
                self.edit_tb(key, t, |tb, _| sheet::delete_rows(tb, &hit));
                Ok(Out::Msg(
                    true,
                    format!(
                        "{} row(s) deleted from {}{}",
                        n,
                        table,
                        if saved > 0 { " (not saved yet — COMMIT to save; deleted rows are hidden, their old versions stay in the blockchain's history)" } else { "" }
                    ),
                ))
            }
            Stmt::Create { table, cols, id, open } => {
                let i = self.draft_idx(key).ok_or("No such database")?;
                if self.drafts[i].tables.iter().any(|t| t.name.eq_ignore_ascii_case(&table)) {
                    return Err(format!("Table `{}` already exists", table));
                }
                if table.len() > 64 {
                    return Err("Table names are at most 64 characters".into());
                }
                let mut uniq: Vec<String> = vec![];
                for c in cols {
                    if uniq.iter().any(|u| u.eq_ignore_ascii_case(&c)) {
                        return Err(format!("Column `{}` appears twice", c));
                    }
                    uniq.push(c);
                }
                let id_col = match &id {
                    Some(n) => uniq.iter().position(|c| c.eq_ignore_ascii_case(n)).ok_or_else(|| format!("PRIMARY KEY `{}` isn't one of the columns", n))?,
                    None => 0,
                };
                self.drafts[i].tables.push(DraftTable { name: table.clone(), title: table.clone(), columns: uniq, id_col, open, compress: true, created: None, rows: vec![] });
                self.drafts[i].sel = self.drafts[i].tables.len() - 1;
                self.save_drafts();
                Ok(Out::Msg(true, format!("Table {} created ({} for other people). It goes on the blockchain with the next COMMIT.", table, if open { "open" } else { "locked" })))
            }
            Stmt::AddColumn { table, col } => {
                let t = self.table_idx(key, &table)?;
                match self.edit_tb(key, t, |tb, _| sheet::add_column(tb, &col, None)) {
                    Some(Ok(_)) => Ok(Out::Msg(true, format!("Column {} added to {}", col, table))),
                    Some(Err(e)) => Err(e),
                    None => Err("No such table".into()),
                }
            }
            Stmt::DropColumn { table, col } => {
                let t = self.table_idx(key, &table)?;
                let i = self.draft_idx(key).unwrap();
                let c = self.drafts[i].tables[t].columns.iter().position(|x| x.eq_ignore_ascii_case(&col)).ok_or_else(|| format!("Unknown column `{}`", col))?;
                let on_chain = self.drafts[i].tables[t].created.is_some();
                match self.edit_tb(key, t, |tb, _| sheet::delete_column(tb, c)) {
                    Some(Ok(())) => Ok(Out::Msg(
                        true,
                        if on_chain { format!("Column {} removed from {}. Values already on the blockchain stay there; new saves leave it out.", col, table) } else { format!("Column {} removed from {}", col, table) },
                    )),
                    Some(Err(e)) => Err(e),
                    None => Err("No such table".into()),
                }
            }
            Stmt::RenameColumn { table, from, to } => {
                let t = self.table_idx(key, &table)?;
                let i = self.draft_idx(key).unwrap();
                let c = self.drafts[i].tables[t].columns.iter().position(|x| x.eq_ignore_ascii_case(&from)).ok_or_else(|| format!("Unknown column `{}`", from))?;
                match self.edit_tb(key, t, |tb, base| sheet::rename_column(tb, base, c, &to)) {
                    Some(Ok(())) => Ok(Out::Msg(true, format!("Renamed {} to {}", from, to))),
                    Some(Err(e)) => Err(e),
                    None => Err("No such table".into()),
                }
            }
            Stmt::DropTable { table } => {
                let t = self.table_idx(key, &table)?;
                let i = self.draft_idx(key).unwrap();
                if self.drafts[i].tables[t].created.is_some() {
                    return Err(format!("{} is on the blockchain, and data there is permanent, so it can't be dropped. DELETE FROM {} hides every row instead.", table, table));
                }
                self.drafts[i].tables.remove(t);
                self.drafts[i].sel = 0;
                self.table_removed(key, t);
                self.save_drafts();
                Ok(Out::Msg(true, format!("Table {} dropped (it was never saved)", table)))
            }
            Stmt::ShowTables => {
                let i = self.draft_idx(key).ok_or("No such database")?;
                let n = self.drafts[i].tables.len();
                let mut rows = vec![];
                for t in 0..n {
                    let r = self.sheet_rows(key, t);
                    let (a, c, d) = sheet::pending(&r);
                    let tb = &self.drafts[i].tables[t];
                    rows.push(vec![
                        json::s(&tb.name),
                        json::n(r.iter().filter(|x| x.state != RowState::Deleted).count()),
                        json::s(&tb.columns.join(", ")),
                        json::s(if tb.created.is_some() { "on the blockchain" } else { "not saved yet" }),
                        json::s(if tb.open { "open" } else { "locked" }),
                        json::s(&if a + c + d == 0 { "—".to_string() } else { format!("{} new, {} changed, {} deleted", a, c, d) }),
                    ]);
                }
                Ok(Out::Rows {
                    title: "tables".into(),
                    cols: ["table", "rows", "columns", "status", "writers", "unsaved"].iter().map(|s| s.to_string()).collect(),
                    rows,
                    note: String::new(),
                })
            }
            Stmt::ShowChanges => {
                let i = self.draft_idx(key).ok_or("No such database")?;
                let n = self.drafts[i].tables.len();
                let mut rows = vec![];
                for t in 0..n {
                    let r = self.sheet_rows(key, t);
                    let (a, c, d) = sheet::pending(&r);
                    rows.push(vec![json::s(&self.drafts[i].tables[t].name), json::n(a), json::n(c), json::n(d)]);
                }
                let (cost, packs) = self.save_estimate(key);
                Ok(Out::Rows {
                    title: "unsaved changes".into(),
                    cols: ["table", "new", "changed", "deleted"].iter().map(|s| s.to_string()).collect(),
                    rows,
                    note: format!("COMMIT would write {} pack(s) · about {}", packs, ui::sol(cost)),
                })
            }
            Stmt::Describe { table } => {
                let t = self.table_idx(key, &table)?;
                let tb = &self.drafts[self.draft_idx(key).unwrap()].tables[t];
                let rows = tb
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(c, name)| vec![json::s(&sheet::col_letter(c)), json::s(name), json::s(if c == tb.id_col { "PRI" } else { "" })])
                    .collect();
                Ok(Out::Rows {
                    title: table.clone(),
                    cols: ["", "column", "key"].iter().map(|s| s.to_string()).collect(),
                    rows,
                    note: format!("{} · {}", if tb.open { "open to everyone" } else { "locked to the owner" }, if tb.created.is_some() { "on the blockchain" } else { "not saved yet" }),
                })
            }
            Stmt::Commit => {
                self.ed.tab = "save".into();
                self.save(key);
                Ok(Out::Msg(true, "Saving to the blockchain — progress is on the Save tab.".into()))
            }
            Stmt::Rollback => {
                let n = self.draft_idx(key).map(|i| self.drafts[i].tables.len()).unwrap_or(0);
                for t in 0..n {
                    self.edit_tb(key, t, |tb, _| sheet::discard(tb));
                }
                Ok(Out::Msg(true, "All unsaved changes discarded (Undo in the sheet brings them back).".into()))
            }
            Stmt::Begin => Ok(Out::Msg(true, "Changes are always held until COMMIT — no need to start a transaction.".into())),
        }
    }
}
