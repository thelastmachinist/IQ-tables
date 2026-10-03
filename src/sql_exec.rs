//! Running SQL against a draft database: reads see the saved rows plus
//! unsaved edits; writes become unsaved edits (checked against the table's
//! rules); COMMIT saves to the blockchain.

use std::collections::HashMap;
use std::rc::Rc;

use crate::app::App;
use crate::constraints::{Change, Opts};
use crate::json::{self, Json};
use crate::sheet::{self, RowState, SRow};
use crate::sql::{self, ast::*, engine, eval, Col, Engine, Rel};

#[derive(Clone, Debug)]
pub enum Out {
    Rows { title: String, cols: Vec<String>, rows: Vec<Vec<Json>>, note: String },
    Msg(bool, String),
}

pub const SHOW_MAX: usize = 1000;

/// A snapshot of a database for one statement.
pub struct Snap {
    pub tables: HashMap<String, Rc<Rel>>,
    pub views: HashMap<String, String>,
}

impl sql::Catalog for Snap {
    fn table(&self, name: &str) -> Option<Rc<Rel>> {
        self.tables.get(&name.to_lowercase()).cloned()
    }
    fn view(&self, name: &str) -> Option<String> {
        self.views.get(&name.to_lowercase()).cloned()
    }
}

pub struct RunCtx {
    /// Running an imported file: COMMIT doesn't save.
    pub import: bool,
}

impl App {
    // ----------------------------------------------------------- lookup

    /// Table index by its SQL name (title) or on-chain name.
    pub fn tbl(&self, key: &str, name: &str) -> Result<usize, String> {
        let d = &self.drafts[self.draft_idx(key).ok_or("No such database")?];
        let vis = |t: &crate::state::DraftTable| !t.dropped && !t.is_system();
        d.tables
            .iter()
            .position(|t| vis(t) && t.title.eq_ignore_ascii_case(name))
            .or_else(|| d.tables.iter().position(|t| vis(t) && t.name.eq_ignore_ascii_case(name)))
            .ok_or_else(|| {
                let names: Vec<&str> = d.tables.iter().filter(|t| vis(t)).map(|t| t.title.as_str()).collect();
                format!("No table `{}` in {}. Tables: {}", name, d.name, if names.is_empty() { "none yet".to_string() } else { names.join(", ") })
            })
    }

    pub fn system_table(&self, key: &str) -> Option<usize> {
        let d = &self.drafts[self.draft_idx(key)?];
        d.tables.iter().position(|t| t.is_system())
    }

    /// Views: (name, SELECT text), from IQ Tables' own table in the database.
    pub fn views(&mut self, key: &str) -> Vec<(String, String)> {
        let Some(st) = self.system_table(key) else { return vec![] };
        self.ensure_base(key, st);
        let tb = self.drafts[self.draft_idx(key).unwrap()].tables[st].clone();
        let (Some(n), Some(k), Some(q)) = (tb.col("name"), tb.col("kind"), tb.col("body")) else { return vec![] };
        self.sheet_rows(key, st)
            .into_iter()
            .filter(|r| r.state != RowState::Deleted && r.vals.get(k).map(|v| v.cell_text() == "view").unwrap_or(false))
            .map(|r| (r.vals.get(n).map(|v| v.cell_text()).unwrap_or_default(), r.vals.get(q).map(|v| v.cell_text()).unwrap_or_default()))
            .collect()
    }

    /// Everything a statement may read, as of now.
    pub fn snapshot(&mut self, key: &str) -> Snap {
        let mut tables = HashMap::new();
        let Some(di) = self.draft_idx(key) else { return Snap { tables, views: HashMap::new() } };
        let n = self.drafts[di].tables.len();
        for t in 0..n {
            let tb = self.drafts[di].tables[t].clone();
            if tb.dropped || tb.is_system() {
                continue;
            }
            let rows: Vec<Vec<Json>> = self
                .sheet_rows(key, t)
                .into_iter()
                .filter(|r| r.state != RowState::Deleted)
                .map(|mut r| {
                    r.vals.resize(tb.columns.len(), Json::Null);
                    r.vals
                })
                .collect();
            let rel = Rc::new(Rel { cols: tb.columns.iter().map(|c| Col::new(None, c)).collect(), rows });
            tables.insert(tb.title.to_lowercase(), rel.clone());
            if tb.name != tb.title {
                tables.entry(tb.name.to_lowercase()).or_insert(rel);
            }
        }
        let views = self.views(key).into_iter().map(|(n, q)| (n.to_lowercase(), q)).collect();
        Snap { tables, views }
    }

    pub fn engine_for<'c>(&self, key: &str, snap: &'c Snap) -> Engine<'c> {
        let name = self.draft_idx(key).map(|i| self.drafts[i].name.clone()).unwrap_or_default();
        let user = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address()).unwrap_or_else(|| "anonymous".into());
        let e = Engine::new(snap, &name, &user);
        *e.vars.borrow_mut() = self.ed.sql_vars.clone();
        e.last_insert_id.set(self.ed.last_insert_id);
        e
    }

    pub fn fk_checks(&self) -> bool {
        self.ed.sql_vars.get("foreign_key_checks").map(|v| eval::truthy(v) != Some(false)).unwrap_or(true)
    }

    // -------------------------------------------------------------- run

    /// Parse and run a script. Tables still being read from the blockchain
    /// are waited for; the script then runs by itself.
    pub fn run_sql(&mut self, key: &str, src: &str) -> Vec<Out> {
        self.run_sql_opts(key, src, false)
    }

    pub fn run_sql_opts(&mut self, key: &str, src: &str, import: bool) -> Vec<Out> {
        let stmts = match sql::parse(src) {
            Ok(s) => s,
            Err(e) => return vec![Out::Msg(false, e)],
        };
        let stmts = if import { crate::ddl::hoist_keys(stmts) } else { stmts };
        // every saved table (and the views table) is read before running
        let Some(di) = self.draft_idx(key) else { return vec![Out::Msg(false, "No such database".into())] };
        let mut loading = vec![];
        for t in 0..self.drafts[di].tables.len() {
            if self.drafts[di].tables[t].created.is_none() {
                continue;
            }
            self.ensure_base(key, t);
            if matches!(self.sheet_base(key, t).1, crate::editor::BaseState::Loading) {
                loading.push(self.drafts[di].tables[t].title.clone());
            }
        }
        if !loading.is_empty() {
            self.ed.sql_wait = Some((key.to_string(), src.to_string()));
            return vec![Out::Msg(true, format!("Reading {} from the blockchain… the query runs as soon as it's loaded.", loading.join(", ")))];
        }
        let mut ctx = RunCtx { import };
        let mut out = vec![];
        let total = stmts.len();
        for (k, (s, text)) in stmts.into_iter().enumerate() {
            match self.exec(key, s, &text, &mut ctx) {
                Ok(o) => out.extend(o),
                Err(e) => {
                    let e = if total > 1 { format!("Statement {} of {}: {}", k + 1, total, e) } else { e };
                    out.push(Out::Msg(false, e));
                    if k + 1 < total {
                        out.push(Out::Msg(false, format!("Stopped here; the {} statement(s) after it didn't run.", total - k - 1)));
                    }
                    break;
                }
            }
        }
        if import && out.len() > 12 {
            // keep an import's report short
            let oks = out.iter().filter(|o| matches!(o, Out::Msg(true, _))).count();
            let bad: Vec<Out> = out.iter().filter(|o| matches!(o, Out::Msg(false, _))).cloned().collect();
            let mut v = vec![Out::Msg(true, format!("{} statement(s) ran.", oks))];
            v.extend(bad);
            return v;
        }
        out
    }

    fn pending_note(&mut self, key: &str) -> String {
        let n = self.draft_idx(key).map(|i| self.drafts[i].tables.len()).unwrap_or(0);
        let mut p = 0;
        for t in 0..n {
            let (a, b, c) = sheet::pending(&self.sheet_rows(key, t));
            p += a + b + c;
        }
        if p > 0 {
            " · includes unsaved changes".into()
        } else {
            String::new()
        }
    }

    pub fn exec(&mut self, key: &str, s: Stmt, text: &str, ctx: &mut RunCtx) -> Result<Vec<Out>, String> {
        let one = |o: Out| Ok(vec![o]);
        match s {
            Stmt::Query(q) => {
                let snap = self.snapshot(key);
                let eng = self.engine_for(key, &snap);
                let t0 = crate::host::now_ms();
                let r = eng.query(&q, None)?;
                let ms = crate::host::now_ms() - t0;
                self.ed.sql_vars = eng.vars.borrow().clone();
                let title = single_table(&q).unwrap_or_default();
                let cols = r.names();
                let rows = r.visible_rows();
                let total = rows.len();
                // the command line gets every row; the page shows the first ones
                let show = if self.cli.is_some() { usize::MAX } else { SHOW_MAX };
                let note = format!(
                    "{} row{}{}{}{}",
                    total,
                    if total == 1 { "" } else { "s" },
                    if ms >= 1.0 { format!(" · {:.0} ms", ms) } else { String::new() },
                    self.pending_note(key),
                    if total > show { format!(" · first {} shown", SHOW_MAX) } else { String::new() }
                );
                one(Out::Rows { title, cols, rows: rows.into_iter().take(show).collect(), note })
            }
            Stmt::Insert { table, cols, src, ignore, replace, on_dup } => {
                self.insert(key, &table, cols, src, ignore, replace, on_dup).map(|m| vec![Out::Msg(true, m)])
            }
            Stmt::Update { from, sets, filter, order, limit } => self.update(key, from, sets, filter, order, limit).map(|m| vec![Out::Msg(true, m)]),
            Stmt::Delete { targets, from, filter, order, limit } => self.delete(key, targets, from, filter, order, limit).map(|m| vec![Out::Msg(true, m)]),
            Stmt::Explain(inner) => self.explain(key, *inner),
            Stmt::Set(v) => {
                let snap = self.snapshot(key);
                let eng = self.engine_for(key, &snap);
                let mut notes = vec![];
                for (name, e) in v {
                    let val = eng.eval_const(&e)?;
                    if name == "foreign_key_checks" {
                        notes.push(format!("Foreign key checks {}.", if eval::truthy(&val) == Some(false) { "off" } else { "on" }));
                    }
                    eng.vars.borrow_mut().insert(name, val);
                }
                self.ed.sql_vars = eng.vars.borrow().clone();
                if notes.is_empty() {
                    Ok(vec![])
                } else {
                    one(Out::Msg(true, notes.join(" ")))
                }
            }
            Stmt::Begin => one(Out::Msg(true, "Changes are always held until COMMIT — no need to start a transaction.".into())),
            Stmt::Commit => {
                if let Some(c) = self.cli.as_mut() {
                    c.commit = true;
                    return one(Out::Msg(true, "COMMIT noted: nothing is saved until you save (the command line's --yes, or save_changes).".into()));
                }
                if ctx.import {
                    return one(Out::Msg(true, "COMMIT in the file ignored — press Save when you're ready.".into()));
                }
                self.ed.tab = "save".into();
                self.save(key);
                one(Out::Msg(true, "Saving to the blockchain — progress is on the Save tab.".into()))
            }
            Stmt::Rollback => {
                let n = self.draft_idx(key).map(|i| self.drafts[i].tables.len()).unwrap_or(0);
                for t in 0..n {
                    self.edit_tb(key, t, |tb, _| sheet::discard(tb));
                }
                one(Out::Msg(
                    true,
                    "Unsaved row changes discarded (Undo in the sheet brings them back). Structure changes stay — undo them with ALTER TABLE.".into(),
                ))
            }
            Stmt::Noop(m) => one(Out::Msg(true, m)),
            Stmt::Unsupported(m) => Err(m),
            other => self.exec_ddl(key, other, text, ctx),
        }
    }

    // ------------------------------------------------------------ INSERT

    #[allow(clippy::too_many_arguments)]
    fn insert(
        &mut self,
        key: &str,
        table: &str,
        cols: Option<Vec<String>>,
        src: InsertSrc,
        ignore: bool,
        replace: bool,
        on_dup: Vec<(String, Expr)>,
    ) -> Result<String, String> {
        let t = self.tbl_writable(key, table)?;
        let tb = self.drafts[self.draft_idx(key).unwrap()].tables[t].clone();
        let n = tb.columns.len();
        let map: Vec<usize> = match (&cols, &src) {
            (_, InsertSrc::Set(pairs)) => {
                pairs.iter().map(|(c, _)| tb.col(c).ok_or_else(|| format!("Unknown column `{}` in {}", c, tb.title))).collect::<Result<_, _>>()?
            }
            (Some(cs), _) => cs.iter().map(|c| tb.col(c).ok_or_else(|| format!("Unknown column `{}` in {}", c, tb.title))).collect::<Result<_, _>>()?,
            (None, _) => (0..n).collect(),
        };
        let snap = self.snapshot(key);
        let eng = self.engine_for(key, &snap);
        // the new rows: Some(value) or None (DEFAULT)
        let mut tuples: Vec<Vec<Option<Json>>> = vec![];
        match &src {
            InsertSrc::Values(rows) => {
                for r in rows {
                    if r.is_empty() {
                        tuples.push(vec![None; map.len()]);
                        continue;
                    }
                    if r.len() != map.len() {
                        return Err(format!(
                            "{} value(s) for {} column(s){}",
                            r.len(),
                            map.len(),
                            if cols.is_none() { format!(" ({} has: {})", tb.title, tb.columns.join(", ")) } else { String::new() }
                        ));
                    }
                    let mut v = vec![];
                    for e in r {
                        v.push(if matches!(e, Expr::Default) { None } else { Some(eng.eval_const(e)?) });
                    }
                    tuples.push(v);
                }
            }
            InsertSrc::Set(pairs) => {
                let mut v = vec![];
                for (_, e) in pairs {
                    v.push(if matches!(e, Expr::Default) { None } else { Some(eng.eval_const(e)?) });
                }
                tuples.push(v);
            }
            InsertSrc::Query(q) => {
                let r = eng.query(q, None)?;
                if r.names().len() != map.len() {
                    return Err(format!("The SELECT returns {} column(s) but {} are being filled", r.names().len(), map.len()));
                }
                for row in r.visible_rows() {
                    tuples.push(row.into_iter().map(Some).collect());
                }
            }
        }
        let rows = self.sheet_rows(key, t);
        let by_id: HashMap<String, SRow> = rows
            .iter()
            .filter(|r| r.state != RowState::Deleted)
            .map(|r| (r.vals.get(tb.id_col).map(|v| v.cell_text()).unwrap_or_default(), r.clone()))
            .collect();
        let mut changes = vec![];
        let (mut skipped, mut dup_updates, mut replaced) = (0, 0, 0);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for tup in tuples {
            let mut vals = vec![Json::Null; n];
            let mut given = vec![false; n];
            for (k, &c) in map.iter().enumerate() {
                if let Some(v) = tup.get(k).cloned().flatten() {
                    vals[c] = v;
                    given[c] = true;
                }
            }
            let id = tb.meta.get(tb.id_col).map(|m| m.ty.coerce(&vals[tb.id_col]).unwrap_or(Json::Null)).unwrap_or(Json::Null).cell_text();
            let existing = if id.is_empty() || seen.contains(&id) { None } else { by_id.get(&id).cloned() };
            if let Some(ex) = existing {
                if !on_dup.is_empty() {
                    *eng.values_row.borrow_mut() = Some((tb.columns.clone(), vals.clone()));
                    let cols: Vec<Col> = tb.columns.iter().map(|c| Col::new(Some(&tb.title), c)).collect();
                    let mut nv = ex.vals.clone();
                    nv.resize(n, Json::Null);
                    let mut set = vec![false; n];
                    for (c, e) in &on_dup {
                        let ci = tb.col(c).ok_or_else(|| format!("Unknown column `{}`", c))?;
                        let v = if matches!(e, Expr::Default) {
                            crate::constraints::default_value(&tb.meta[ci].ty, &tb.meta[ci].default)?
                        } else {
                            eng.eval_row(&cols, &nv, e)?
                        };
                        nv[ci] = v;
                        set[ci] = true;
                    }
                    *eng.values_row.borrow_mut() = None;
                    changes.push(Change::Update { row: ex, vals: nv, set });
                    dup_updates += 1;
                    seen.insert(id);
                    continue;
                }
                if replace {
                    // REPLACE: the old row goes, the new one takes its place
                    let mut full = vals.clone();
                    for c in 0..n {
                        if !given[c] {
                            full[c] = crate::constraints::default_value(&tb.meta[c].ty, &tb.meta[c].default)?;
                        }
                    }
                    changes.push(Change::Update { row: ex, vals: full, set: vec![true; n] });
                    replaced += 1;
                    seen.insert(id);
                    continue;
                }
                if ignore {
                    skipped += 1;
                    continue;
                }
                return Err(format!(
                    "A row with {} = '{}' already exists — use UPDATE, REPLACE or INSERT … ON DUPLICATE KEY UPDATE",
                    tb.columns[tb.id_col], id
                ));
            }
            if !id.is_empty() && !seen.insert(id.clone()) {
                if ignore {
                    skipped += 1;
                    continue;
                }
                return Err(format!("The value '{}' for {} appears twice in this INSERT", id, tb.columns[tb.id_col]));
            }
            changes.push(Change::Insert { vals, given });
        }
        drop(eng);
        let fk = self.fk_checks();
        let a = self.apply_changes(key, t, changes, &Opts { strict: true, fk_checks: fk })?;
        if let Some(id) = a.first_auto {
            self.ed.last_insert_id = id;
        }
        let mut m = format!("{} row(s) inserted into {}", a.inserted, tb.title);
        if dup_updates > 0 {
            m.push_str(&format!(", {} existing row(s) updated", dup_updates));
        }
        if replaced > 0 {
            m.push_str(&format!(", {} replaced", replaced));
        }
        if skipped > 0 {
            m.push_str(&format!(", {} duplicate(s) skipped", skipped));
        }
        if let Some(id) = a.first_auto {
            if a.inserted == 1 {
                m.push_str(&format!(" · new id {}", id));
            } else {
                m.push_str(&format!(" · new ids from {}", id));
            }
        }
        for c in &a.cascaded {
            m.push_str(&format!(" · {}", c));
        }
        m.push_str(" (not saved yet — COMMIT to save)");
        Ok(m)
    }

    /// A table that can take row changes (not a view, not IQ Tables' own).
    fn tbl_writable(&self, key: &str, name: &str) -> Result<usize, String> {
        match self.tbl(key, name) {
            Ok(t) => Ok(t),
            Err(e) => {
                let d = &self.drafts[self.draft_idx(key).unwrap()];
                if d.tables.iter().any(|t| t.is_system() && name.eq_ignore_ascii_case(&t.name)) {
                    return Err("That table belongs to IQ Tables itself (it keeps the database's views).".into());
                }
                Err(e)
            }
        }
    }

    // ------------------------------------------------------------ UPDATE

    #[allow(clippy::type_complexity)]
    fn target_rows(
        &mut self,
        key: &str,
        targets: &[String],
        from: &From,
        filter: Option<&Expr>,
        order: &[Order],
        limit: Option<&Expr>,
    ) -> Result<(Rel, Vec<usize>, HashMap<String, (usize, usize)>), String> {
        let mut aliases: Vec<(String, String)> = vec![];
        collect_aliases(from, &mut aliases);
        let snap = self.snapshot(key);
        let eng = self.engine_for(key, &snap);
        let mut tmap: HashMap<String, (usize, usize)> = HashMap::new();
        for tname in targets {
            let (alias, real) = aliases
                .iter()
                .find(|(a, r)| a.eq_ignore_ascii_case(tname) || r.eq_ignore_ascii_case(tname))
                .cloned()
                .ok_or_else(|| format!("`{}` isn't in the FROM list", tname))?;
            if snap.views.contains_key(&real.to_lowercase()) {
                return Err(format!("{} is a view — change the table it reads from instead", real));
            }
            let t = self.tbl_writable(key, &real)?;
            eng.rid_tables.borrow_mut().push(alias.clone());
            tmap.insert(alias.to_lowercase(), (t, 0));
        }
        let rel = eng.from(from, None)?;
        for (alias, v) in tmap.iter_mut() {
            v.1 = rel
                .cols
                .iter()
                .position(|c| c.name == engine::RID && c.table.as_deref().map(|x| x.eq_ignore_ascii_case(alias)).unwrap_or(false))
                .ok_or("internal: row id missing")?;
        }
        let mut hit: Vec<usize> = vec![];
        for (i, row) in rel.rows.iter().enumerate() {
            let sc = sql::Scope::row(&rel.cols, row, None);
            let keep = match filter {
                Some(f) => eng.truthy(f, &sc)?,
                None => true,
            };
            if keep {
                hit.push(i);
            }
        }
        if !order.is_empty() {
            let mut keys = vec![];
            for &i in &hit {
                let sc = sql::Scope::row(&rel.cols, &rel.rows[i], None);
                let mut k = vec![];
                for o in order {
                    k.push(eng.eval(&o.e, &sc)?);
                }
                keys.push((k, i));
            }
            keys.sort_by(|a, b| engine::order_cmp(&a.0, &b.0, order));
            hit = keys.into_iter().map(|x| x.1).collect();
        }
        if let Some(l) = limit {
            let v = eng.eval_const(l)?;
            hit.truncate(eval::num(&v).unwrap_or(0.0).max(0.0) as usize);
        }
        Ok((rel, hit, tmap))
    }

    fn update(
        &mut self,
        key: &str,
        from: From,
        sets: Vec<(Option<String>, String, Expr)>,
        filter: Option<Expr>,
        order: Vec<Order>,
        limit: Option<Expr>,
    ) -> Result<String, String> {
        let mut aliases: Vec<(String, String)> = vec![];
        collect_aliases(&from, &mut aliases);
        for (_, real) in &aliases {
            if self.tbl(key, real).is_err() && self.views(key).iter().any(|(v, _)| v.eq_ignore_ascii_case(real)) {
                return Err(format!("{} is a view — change the table it reads from instead", real));
            }
        }
        // which table each SET column belongs to
        let mut targets: Vec<String> = vec![];
        let mut plan: Vec<(String, usize, Expr)> = vec![]; // (alias, column index, expr)
        for (q, c, e) in &sets {
            let alias = match q {
                Some(q) => aliases
                    .iter()
                    .find(|(a, _)| a.eq_ignore_ascii_case(q))
                    .map(|x| x.0.clone())
                    .ok_or_else(|| format!("`{}` isn't in the UPDATE's table list", q))?,
                None => {
                    let mut owners = vec![];
                    for (a, real) in &aliases {
                        if let Ok(t) = self.tbl(key, real) {
                            if self.drafts[self.draft_idx(key).unwrap()].tables[t].col(c).is_some() {
                                owners.push(a.clone());
                            }
                        }
                    }
                    match owners.len() {
                        0 => return Err(format!("Unknown column `{}`", c)),
                        1 => owners.remove(0),
                        _ => return Err(format!("Column `{}` is in more than one table — write table.{}", c, c)),
                    }
                }
            };
            let real = aliases.iter().find(|(a, _)| *a == alias).unwrap().1.clone();
            let t = self.tbl_writable(key, &real)?;
            let ci = self.drafts[self.draft_idx(key).unwrap()].tables[t].col(c).ok_or_else(|| format!("Unknown column `{}` in {}", c, real))?;
            if !targets.contains(&alias) {
                targets.push(alias.clone());
            }
            plan.push((alias, ci, e.clone()));
        }
        let (rel, hit, tmap) = self.target_rows(key, &targets, &from, filter.as_ref(), &order, limit.as_ref())?;
        let single = targets.len() == 1 && aliases.len() == 1;
        let mut total = (0, 0);
        let mut cascaded = vec![];
        for alias in &targets {
            let (t, rid_col) = tmap[&alias.to_lowercase()];
            let tb = self.drafts[self.draft_idx(key).unwrap()].tables[t].clone();
            let live: Vec<SRow> = self.sheet_rows(key, t).into_iter().filter(|r| r.state != RowState::Deleted).collect();
            let snap = self.snapshot(key);
            let eng = self.engine_for(key, &snap);
            let mut done: std::collections::HashSet<usize> = std::collections::HashSet::new();
            let mut changes = vec![];
            let mut matched = 0;
            for &i in &hit {
                let row = &rel.rows[i];
                let Some(rid) = eval::num(&row[rid_col]).map(|f| f as usize) else { continue };
                if !done.insert(rid) {
                    continue;
                }
                matched += 1;
                let Some(sr) = live.get(rid) else { continue };
                let mut vals = sr.vals.clone();
                vals.resize(tb.columns.len(), Json::Null);
                let mut set = vec![false; tb.columns.len()];
                // a single-table UPDATE sees its own earlier assignments (MySQL)
                let mut scope_row = row.clone();
                for (a, ci, e) in plan.iter().filter(|p| &p.0 == alias) {
                    let sc = sql::Scope::row(&rel.cols, &scope_row, None);
                    let v = if matches!(e, Expr::Default) {
                        crate::constraints::default_value(&tb.meta[*ci].ty, &tb.meta[*ci].default)?
                    } else {
                        eng.eval(e, &sc)?
                    };
                    vals[*ci] = v.clone();
                    set[*ci] = true;
                    if single {
                        if let Some(p) = rel.cols.iter().position(|c| {
                            c.table.as_deref().map(|x| x.eq_ignore_ascii_case(a)).unwrap_or(false) && c.name.eq_ignore_ascii_case(&tb.columns[*ci])
                        }) {
                            scope_row[p] = v;
                        }
                    }
                }
                changes.push(Change::Update { row: sr.clone(), vals, set });
            }
            drop(eng);
            let fk = self.fk_checks();
            let a = self.apply_changes(key, t, changes, &Opts { strict: true, fk_checks: fk })?;
            total.0 += matched;
            total.1 += a.updated;
            cascaded.extend(a.cascaded);
        }
        let mut m = format!("{} row(s) changed ({} matched)", total.1, total.0);
        for c in cascaded {
            m.push_str(&format!(" · {}", c));
        }
        if total.1 > 0 {
            m.push_str(" (not saved yet — COMMIT to save)");
        }
        Ok(m)
    }

    // ------------------------------------------------------------ DELETE

    fn delete(&mut self, key: &str, targets: Vec<String>, from: From, filter: Option<Expr>, order: Vec<Order>, limit: Option<Expr>) -> Result<String, String> {
        let (rel, hit, tmap) = self.target_rows(key, &targets, &from, filter.as_ref(), &order, limit.as_ref())?;
        let mut total = 0;
        let mut saved = 0;
        let mut cascaded = vec![];
        let mut names = vec![];
        for alias in &targets {
            let (t, rid_col) = tmap[&alias.to_lowercase()];
            let live: Vec<SRow> = self.sheet_rows(key, t).into_iter().filter(|r| r.state != RowState::Deleted).collect();
            let mut picked: std::collections::HashSet<usize> = std::collections::HashSet::new();
            let mut changes = vec![];
            for &i in &hit {
                let Some(rid) = eval::num(&rel.rows[i][rid_col]).map(|f| f as usize) else { continue };
                if picked.insert(rid) {
                    if let Some(r) = live.get(rid) {
                        if r.state != RowState::New {
                            saved += 1;
                        }
                        changes.push(Change::Delete { row: r.clone() });
                    }
                }
            }
            let fk = self.fk_checks();
            let a = self.apply_changes(key, t, changes, &Opts { strict: true, fk_checks: fk })?;
            total += a.deleted;
            cascaded.extend(a.cascaded);
            names.push(self.drafts[self.draft_idx(key).unwrap()].tables[t].title.clone());
        }
        let mut m = format!("{} row(s) deleted from {}", total, names.join(", "));
        for c in cascaded {
            m.push_str(&format!(" · {}", c));
        }
        if saved > 0 {
            m.push_str(" (not saved yet — COMMIT to save; deleted rows are hidden, their old versions stay in the blockchain's history)");
        }
        Ok(m)
    }

    // ----------------------------------------------------------- EXPLAIN

    fn explain(&mut self, key: &str, s: Stmt) -> Result<Vec<Out>, String> {
        let mut tables = vec![];
        match &s {
            Stmt::Query(q) => tables = engine::tables_in_query(q),
            Stmt::Update { from, .. } | Stmt::Delete { from, .. } => engine::tables_in_from(from, &mut tables),
            Stmt::Insert { table, .. } => tables.push(table.clone()),
            _ => {}
        }
        let snap = self.snapshot(key);
        let mut rows = vec![];
        for (i, t) in tables.iter().enumerate() {
            let n = sql::Catalog::table(&snap, t).map(|r| r.rows.len()).unwrap_or(0);
            let kind = if snap.views.contains_key(&t.to_lowercase()) { "view" } else { "table" };
            rows.push(vec![
                json::n(i + 1),
                json::s(t),
                json::s(kind),
                json::n(n),
                json::s(if i == 0 { "reads every row (they're all in memory)" } else { "joined with a hash lookup on = conditions, else row by row" }),
            ]);
        }
        Ok(vec![Out::Rows {
            title: "plan".into(),
            cols: ["id", "table", "type", "rows", "how"].iter().map(|s| s.to_string()).collect(),
            rows,
            note: "Everything runs in your browser over rows read from the blockchain; there are no indexes to use or miss.".into(),
        }])
    }

    /// Run SQL made by the UI (Structure, Operations, Search…) and report
    /// it the way phpMyAdmin does.
    pub fn run_ui_sql(&mut self, key: &str, text: &str) -> bool {
        let out = self.run_sql(key, text);
        let bad = out.iter().find_map(|o| if let Out::Msg(false, m) = o { Some(m.clone()) } else { None });
        self.ed.last_sql = Some(text.to_string());
        match bad {
            Some(e) => {
                self.err(e);
                false
            }
            None => {
                // buttons, not SQL, save here
                let msg = out
                    .iter()
                    .filter_map(|o| if let Out::Msg(true, m) = o { Some(m.clone()) } else { None })
                    .collect::<Vec<_>>()
                    .join(" ")
                    .replace("COMMIT to save", "press Save to keep it")
                    .replace("with the next COMMIT", "when you press Save");
                self.ok(if msg.is_empty() { "Done".to_string() } else { msg });
                true
            }
        }
    }
}

fn collect_aliases(f: &From, out: &mut Vec<(String, String)>) {
    match f {
        From::Table { name, alias } => out.push((alias.clone().unwrap_or_else(|| name.clone()), name.clone())),
        From::Sub { .. } => {}
        From::Join { left, right, .. } => {
            collect_aliases(left, out);
            collect_aliases(right, out);
        }
    }
}

fn single_table(q: &Query) -> Option<String> {
    if let Body::Select(s) = &q.body {
        if let Some(From::Table { name, .. }) = &s.from {
            return Some(name.clone());
        }
    }
    None
}

pub fn sql_ident(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

pub fn sql_value(v: &Json) -> String {
    match v {
        Json::Null => "NULL".into(),
        Json::Num(n) => n.clone(),
        Json::Bool(b) => (if *b { "1" } else { "0" }).into(),
        Json::Str(s) => sql_str(s),
        other => sql_str(&other.to_string()),
    }
}

pub fn sql_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('\'');
    for c in s.chars() {
        match c {
            '\'' => o.push_str("\\'"),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\0' => o.push_str("\\0"),
            '\u{1a}' => o.push_str("\\Z"),
            c => o.push(c),
        }
    }
    o.push('\'');
    o
}
