//! Applying row changes with the table's rules: types, NOT NULL, defaults,
//! AUTO_INCREMENT, UNIQUE, CHECK and foreign keys (with ON DELETE / ON
//! UPDATE actions). Both the SQL console and the spreadsheet go through
//! here, so a rule can't be broken from either side. Rows others add to an
//! open table with other tools aren't bound by it — the chain only stores
//! what it's given — so the rules hold for everything written from here.

use std::collections::{HashMap, HashSet};

use crate::app::App;
use crate::json::Json;
use crate::schema::{DefVal, RefAction, Ty};
use crate::sheet::{self, RowState, SRow};
use crate::sql::{self, eval, Col, Engine, Rel};
use crate::state::{DraftTable, GhostRow};

#[derive(Clone, Debug)]
pub enum Change {
    /// A new row; `given[c]` = the value for column c was supplied (else its default applies).
    Insert { vals: Vec<Json>, given: Vec<bool> },
    /// New values for an existing row; `set[c]` = column c was assigned explicitly.
    Update { row: SRow, vals: Vec<Json>, set: Vec<bool> },
    Delete { row: SRow },
}

pub struct Opts {
    /// Enforce NOT NULL and a present primary key now (SQL). The sheet lets
    /// rows be filled in over several edits and checks them at save time.
    pub strict: bool,
    pub fk_checks: bool,
}

#[derive(Default, Debug)]
pub struct Applied {
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
    pub first_auto: Option<u64>,
    pub cascaded: Vec<String>,
}

struct Empty;
impl sql::Catalog for Empty {
    fn table(&self, _: &str) -> Option<std::rc::Rc<Rel>> {
        None
    }
    fn view(&self, _: &str) -> Option<String> {
        None
    }
}

/// A column default, evaluated now (CURRENT_TIMESTAMP, (UUID()), literals).
pub fn default_value(ty: &Ty, d: &Option<DefVal>) -> Result<Json, String> {
    match d {
        None => Ok(Json::Null),
        Some(DefVal::Lit(v)) => ty.coerce(v),
        Some(DefVal::Expr(e)) => {
            let ex = sql::parse_expr(e.trim())?;
            let cat = Empty;
            let eng = Engine::new(&cat, "", "");
            let v = eng.eval_const(&ex)?;
            ty.coerce(&v)
        }
    }
}

/// MySQL's value for a NOT NULL column with no default when a column is
/// added to rows that already exist.
pub fn implicit_default(ty: &Ty) -> Json {
    match ty {
        Ty::Int(..) | Ty::Decimal(..) | Ty::Float(..) => Json::Num("0".into()),
        Ty::Bool => Json::Bool(false),
        Ty::Char(_) | Ty::Varchar(_) | Ty::Text(_) | Ty::Set(_) => Json::Str(String::new()),
        Ty::Enum(v) => Json::Str(v.first().cloned().unwrap_or_default()),
        Ty::Date => Json::Str("0000-00-00".into()),
        Ty::DateTime(_) | Ty::Timestamp(_) => Json::Str("0000-00-00 00:00:00".into()),
        Ty::Time(_) => Json::Str("00:00:00".into()),
        Ty::Year => Json::Num("0".into()),
        Ty::Json => Json::Null,
        Ty::Any => Json::Null,
    }
}

fn key_of(vals: &[Json], cols: &[usize]) -> Option<String> {
    let mut k = String::new();
    for &c in cols {
        let v = vals.get(c)?;
        if v.is_null() || (matches!(v, Json::Str(s) if s.is_empty()) && cols.len() == 1) {
            return None;
        }
        k.push_str(&eval::key(v));
        k.push('\u{1}');
    }
    Some(k)
}

fn show_row_id(tb: &DraftTable, vals: &[Json]) -> String {
    let id = vals.get(tb.id_col).map(|v| v.cell_text()).unwrap_or_default();
    if id.is_empty() {
        "a new row".into()
    } else {
        format!("the row {} = {}", tb.columns.get(tb.id_col).cloned().unwrap_or_default(), id)
    }
}

impl App {
    fn table_by_seed(&self, key: &str, seed: &str) -> Option<usize> {
        let i = self.draft_idx(key)?;
        self.drafts[i].tables.iter().position(|t| t.name == seed && !t.dropped)
    }

    /// Live rows of another table as (values) for foreign-key lookups.
    fn live_vals(&mut self, key: &str, t: usize) -> Vec<Vec<Json>> {
        self.sheet_rows(key, t).into_iter().filter(|r| r.state != RowState::Deleted).map(|r| r.vals).collect()
    }

    /// Check a CHECK expression against a row.
    fn check_ok(&self, tb: &DraftTable, expr: &str, vals: &[Json]) -> Result<bool, String> {
        let e = sql::parse_expr(expr)?;
        let cat = Empty;
        let eng = Engine::new(&cat, "", "");
        let cols: Vec<Col> = tb.columns.iter().map(|c| Col::new(None, c)).collect();
        let v = eng.eval_row(&cols, vals, &e)?;
        // NULL passes a CHECK, as in SQL
        Ok(eval::truthy(&v) != Some(false))
    }

    /// Validate and apply changes to table `t` (and cascade to tables that
    /// reference it). Nothing is applied if any change breaks a rule.
    pub fn apply_changes(&mut self, key: &str, t: usize, changes: Vec<Change>, o: &Opts) -> Result<Applied, String> {
        let mut applied = Applied::default();
        let di = self.draft_idx(key).ok_or("No such database")?;
        let mut tb = self.drafts[di].tables.get(t).cloned().ok_or("No such table")?;
        tb.fix_meta();
        let n = tb.columns.len();
        let rows = self.sheet_rows(key, t);
        let now = crate::dates::now_local();
        // current live rows: index → values; unique indexes over them
        let mut live: HashMap<usize, Vec<Json>> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            if r.state != RowState::Deleted {
                live.insert(i, r.vals.clone());
            }
        }
        let mut uniq: Vec<(String, Vec<usize>, HashMap<String, usize>)> = vec![];
        uniq.push(("PRIMARY".into(), vec![tb.id_col], HashMap::new()));
        for ix in tb.keys.indexes.iter().filter(|i| i.unique) {
            let cols: Vec<usize> = ix.cols.iter().filter_map(|k| tb.meta.iter().position(|m| &m.key == k)).collect();
            if cols.len() == ix.cols.len() {
                uniq.push((ix.name.clone(), cols, HashMap::new()));
            }
        }
        for (i, v) in &live {
            for u in uniq.iter_mut() {
                if let Some(k) = key_of(v, &u.1) {
                    u.2.insert(k, *i);
                }
            }
        }
        // referenced tables for foreign keys
        let mut fk_sets: Vec<(usize, Vec<usize>, HashSet<String>, String)> = vec![]; // (fk idx, local cols, allowed keys, label)
        if o.fk_checks {
            for (fi, fk) in tb.keys.fks.iter().enumerate() {
                let local: Vec<usize> = fk.cols.iter().filter_map(|k| tb.meta.iter().position(|m| &m.key == k)).collect();
                if local.len() != fk.cols.len() {
                    continue;
                }
                let (allowed, label) = if fk.table == tb.name {
                    (HashSet::new(), tb.title.clone())
                } else {
                    let Some(rt) = self.table_by_seed(key, &fk.table) else { continue };
                    let rtb = self.drafts[di].tables[rt].clone();
                    let rc: Vec<usize> = fk.ref_cols.iter().filter_map(|k| rtb.meta.iter().position(|m| &m.key == k)).collect();
                    let set: HashSet<String> = self.live_vals(key, rt).iter().filter_map(|v| key_of(v, &rc)).collect();
                    let names: Vec<String> = rc.iter().map(|&c| rtb.columns[c].clone()).collect();
                    (set, format!("{}.{}", rtb.title, names.join(",")))
                };
                fk_sets.push((fi, local, allowed, label));
            }
        }
        // auto-increment
        let ai_col = tb.meta.iter().position(|m| m.auto_inc);
        let mut next_ai: u64 = 0;
        if let Some(c) = ai_col {
            let mut mx = 0u64;
            for r in &rows {
                if let Some(v) = r.vals.get(c).and_then(|v| eval::num(v)) {
                    if v > 0.0 {
                        mx = mx.max(v as u64);
                    }
                }
                if let Some(v) = r.base.as_ref().and_then(|b| b.get(c)).and_then(|v| eval::num(v)) {
                    if v > 0.0 {
                        mx = mx.max(v as u64);
                    }
                }
            }
            next_ai = (mx + 1).max(tb.keys.ai_next);
        }
        let mut new_rows: Vec<Vec<Json>> = vec![];
        let mut updates: Vec<(SRow, Vec<Json>)> = vec![];
        let mut deletes: Vec<SRow> = vec![];
        let mut fresh_id = rows.len();
        let mut deleted_vals: Vec<Vec<Json>> = vec![];
        let mut changed_refs: Vec<(Vec<Json>, Vec<Json>)> = vec![]; // (old, new) of updated rows, for ON UPDATE
        for ch in changes {
            match ch {
                Change::Delete { row } => {
                    let idx = rows.iter().position(|r| r.ghost == row.ghost && r.base == row.base && r.vals == row.vals);
                    if let Some(i) = idx {
                        if let Some(v) = live.remove(&i) {
                            for u in uniq.iter_mut() {
                                if let Some(k) = key_of(&v, &u.1) {
                                    u.2.remove(&k);
                                }
                            }
                            deleted_vals.push(v);
                        }
                    }
                    if row.state != RowState::Deleted {
                        deletes.push(row);
                        applied.deleted += 1;
                    }
                }
                Change::Insert { mut vals, given } => {
                    vals.resize(n, Json::Null);
                    for c in 0..n {
                        let m = &tb.meta[c];
                        if !given.get(c).copied().unwrap_or(false) && vals[c].is_null() && !m.auto_inc {
                            vals[c] = default_value(&m.ty, &m.default).map_err(|e| format!("Default of {}: {}", tb.columns[c], e))?;
                        }
                        vals[c] = m.ty.coerce(&vals[c]).map_err(|e| format!("{}: {}", tb.columns[c], e))?;
                    }
                    if let Some(c) = ai_col {
                        let v = &vals[c];
                        if v.is_null() || eval::num(v) == Some(0.0) {
                            vals[c] = Json::Num(next_ai.to_string());
                            if applied.first_auto.is_none() {
                                applied.first_auto = Some(next_ai);
                            }
                            next_ai += 1;
                        } else if let Some(x) = eval::num(v) {
                            next_ai = next_ai.max(x as u64 + 1);
                        }
                    }
                    self.row_rules(&tb, &vals, o)?;
                    let id = fresh_id;
                    fresh_id += 1;
                    for u in uniq.iter_mut() {
                        if let Some(k) = key_of(&vals, &u.1) {
                            if u.2.contains_key(&k) {
                                return Err(dup_msg(&tb, &u.0, &u.1, &vals));
                            }
                            u.2.insert(k, id);
                        }
                    }
                    self.fk_rules(&tb, &vals, &fk_sets, &live, &new_rows)?;
                    live.insert(id, vals.clone());
                    new_rows.push(vals);
                    applied.inserted += 1;
                }
                Change::Update { row, mut vals, set } => {
                    if row.state == RowState::Deleted {
                        return Err("That row is marked for deletion — undo the delete first.".into());
                    }
                    vals.resize(n, Json::Null);
                    for c in 0..n {
                        vals[c] = tb.meta[c].ty.coerce(&vals[c]).map_err(|e| format!("{}: {}", tb.columns[c], e))?;
                    }
                    let mut old = row.vals.clone();
                    old.resize(n, Json::Null);
                    if vals == old {
                        continue;
                    }
                    for c in 0..n {
                        if tb.meta[c].on_update_now && !set.get(c).copied().unwrap_or(false) {
                            vals[c] = tb.meta[c].ty.coerce(&Json::Str(now.datetime_str())).unwrap_or(Json::Null);
                        }
                    }
                    if vals[tb.id_col].is_null() && row.base.is_some() && row.state != RowState::New {
                        return Err(format!("A saved row needs its ID ({}) — delete the row instead of clearing it", tb.columns[tb.id_col]));
                    }
                    self.row_rules(&tb, &vals, o)?;
                    let idx = rows.iter().position(|r| r.ghost == row.ghost && r.base == row.base && r.vals == row.vals).unwrap_or(usize::MAX);
                    for u in uniq.iter_mut() {
                        let before = key_of(&old, &u.1);
                        let after = key_of(&vals, &u.1);
                        if before == after {
                            continue;
                        }
                        if let Some(k) = &after {
                            if let Some(&other) = u.2.get(k) {
                                if other != idx {
                                    return Err(dup_msg(&tb, &u.0, &u.1, &vals));
                                }
                            }
                        }
                        if let Some(k) = before {
                            u.2.remove(&k);
                        }
                        if let Some(k) = after {
                            u.2.insert(k, idx);
                        }
                    }
                    self.fk_rules(&tb, &vals, &fk_sets, &live, &new_rows)?;
                    live.insert(idx, vals.clone());
                    changed_refs.push((old.clone(), vals.clone()));
                    // a saved row whose ID changes becomes delete + insert
                    let id_changed = old.get(tb.id_col) != vals.get(tb.id_col);
                    if id_changed && row.state != RowState::New && row.base.is_some() {
                        deletes.push(row);
                        new_rows.push(vals);
                    } else {
                        updates.push((row, vals));
                    }
                    applied.updated += 1;
                }
            }
        }
        // rows in other tables that point here
        if o.fk_checks && (!deleted_vals.is_empty() || !changed_refs.is_empty()) {
            self.referencing_actions(key, &tb, &deleted_vals, &changed_refs, o, &mut applied)?;
        }
        // self-referencing keys (parent rows in the same table)
        if o.fk_checks {
            for (fi, local, _, _) in &fk_sets {
                let fk = &tb.keys.fks[*fi];
                if fk.table != tb.name {
                    continue;
                }
                let rc: Vec<usize> = fk.ref_cols.iter().filter_map(|k| tb.meta.iter().position(|m| &m.key == k)).collect();
                let have: HashSet<String> = live.values().filter_map(|v| key_of(v, &rc)).collect();
                for v in live.values() {
                    if let Some(k) = key_of(v, local) {
                        if !have.contains(&k) {
                            return Err(format!("{} refers to a {} that doesn't exist (link {})", show_row_id(&tb, v), local.iter().map(|&c| tb.columns[c].clone()).collect::<Vec<_>>().join(","), fk.name));
                        }
                    }
                }
            }
        }
        // apply
        let refs: Vec<SRow> = deletes;
        let n_cols = n;
        self.edit_tb(key, t, move |tb, _| {
            tb.fix_meta();
            let errs = sheet::apply_rows(tb, updates);
            let r2: Vec<&SRow> = refs.iter().collect();
            sheet::delete_rows(tb, &r2);
            for mut v in new_rows {
                v.resize(n_cols, Json::Null);
                tb.rows.push(GhostRow { vals: v, deleted: false, sig: None });
            }
            errs
        })
        .map(|errs| if let Some(e) = errs.first() { Err(e.clone()) } else { Ok(()) })
        .unwrap_or(Ok(()))?;
        if let (Some(c), true) = (ai_col, next_ai > 0) {
            let _ = c;
        }
        Ok(applied)
    }

    /// NOT NULL, primary key present, CHECK constraints.
    fn row_rules(&self, tb: &DraftTable, vals: &[Json], o: &Opts) -> Result<(), String> {
        if o.strict {
            for (c, m) in tb.meta.iter().enumerate() {
                let empty = vals[c].is_null();
                if empty && (m.not_null || c == tb.id_col) {
                    return Err(if c == tb.id_col {
                        format!("Every row needs a value in the ID column `{}`", tb.columns[c])
                    } else {
                        format!("Column `{}` can't be empty (it's NOT NULL and has no default)", tb.columns[c])
                    });
                }
            }
        }
        for ck in &tb.keys.checks {
            if !self.check_ok(tb, &ck.expr, vals)? {
                return Err(format!("{} breaks the rule {} ({})", show_row_id(tb, vals), ck.name, ck.expr));
            }
        }
        Ok(())
    }

    fn fk_rules(&self, tb: &DraftTable, vals: &[Json], fk_sets: &[(usize, Vec<usize>, HashSet<String>, String)], _live: &HashMap<usize, Vec<Json>>, _new: &[Vec<Json>]) -> Result<(), String> {
        for (fi, local, allowed, label) in fk_sets {
            if tb.keys.fks[*fi].table == tb.name {
                continue; // checked after the batch
            }
            if let Some(k) = key_of(vals, local) {
                if !allowed.contains(&k) {
                    let v: Vec<String> = local.iter().map(|&c| vals[c].cell_text()).collect();
                    return Err(format!("“{}” isn't in {} — {} must match an existing row there (link {})", v.join(", "), label, local.iter().map(|&c| tb.columns[c].clone()).collect::<Vec<_>>().join(","), tb.keys.fks[*fi].name));
                }
            }
        }
        Ok(())
    }

    /// ON DELETE / ON UPDATE for other tables' foreign keys that point at `tb`.
    fn referencing_actions(&mut self, key: &str, tb: &DraftTable, deleted: &[Vec<Json>], changed: &[(Vec<Json>, Vec<Json>)], o: &Opts, applied: &mut Applied) -> Result<(), String> {
        let di = self.draft_idx(key).ok_or("No such database")?;
        let nt = self.drafts[di].tables.len();
        for ct in 0..nt {
            let child = self.drafts[di].tables[ct].clone();
            if child.dropped || child.name == tb.name {
                continue;
            }
            for fk in child.keys.fks.iter().filter(|f| f.table == tb.name) {
                let rc: Vec<usize> = fk.ref_cols.iter().filter_map(|k| tb.meta.iter().position(|m| &m.key == k)).collect();
                let cc: Vec<usize> = fk.cols.iter().filter_map(|k| child.meta.iter().position(|m| &m.key == k)).collect();
                if rc.len() != fk.ref_cols.len() || cc.len() != fk.cols.len() {
                    continue;
                }
                let gone: HashSet<String> = deleted.iter().filter_map(|v| key_of(v, &rc)).collect();
                let moved: HashMap<String, Vec<Json>> = changed
                    .iter()
                    .filter_map(|(a, b)| {
                        let ka = key_of(a, &rc)?;
                        if key_of(b, &rc).as_ref() == Some(&ka) {
                            return None;
                        }
                        Some((ka, rc.iter().map(|&c| b[c].clone()).collect()))
                    })
                    .collect();
                if gone.is_empty() && moved.is_empty() {
                    continue;
                }
                let crow = self.sheet_rows(key, ct);
                let mut ch_changes = vec![];
                let mut blocked = None;
                for r in crow.iter().filter(|r| r.state != RowState::Deleted) {
                    let Some(k) = key_of(&r.vals, &cc) else { continue };
                    if gone.contains(&k) {
                        match fk.on_delete {
                            RefAction::Cascade => ch_changes.push(Change::Delete { row: r.clone() }),
                            RefAction::SetNull => {
                                let mut v = r.vals.clone();
                                for &c in &cc {
                                    v[c] = Json::Null;
                                }
                                ch_changes.push(Change::Update { row: r.clone(), vals: v, set: vec![true; child.columns.len()] });
                            }
                            _ => blocked = Some((r.vals.clone(), "deleted")),
                        }
                    } else if let Some(newv) = moved.get(&k) {
                        match fk.on_update {
                            RefAction::Cascade | RefAction::SetNull => {
                                let mut v = r.vals.clone();
                                for (j, &c) in cc.iter().enumerate() {
                                    v[c] = if fk.on_update == RefAction::Cascade { newv[j].clone() } else { Json::Null };
                                }
                                ch_changes.push(Change::Update { row: r.clone(), vals: v, set: vec![true; child.columns.len()] });
                            }
                            _ => blocked = Some((r.vals.clone(), "changed")),
                        }
                    }
                    if blocked.is_some() {
                        break;
                    }
                }
                if let Some((v, what)) = blocked {
                    return Err(format!(
                        "Can't be {}: {} in {} still points to it (link {} is {}). Delete or change those rows first, or make the link ON DELETE CASCADE.",
                        what,
                        show_row_id(&child, &v),
                        child.title,
                        fk.name,
                        if what == "deleted" { fk.on_delete.sql() } else { fk.on_update.sql() }
                    ));
                }
                if !ch_changes.is_empty() {
                    let n = ch_changes.len();
                    let sub = self.apply_changes(key, ct, ch_changes, o)?;
                    applied.cascaded.push(format!("{} row(s) in {} {}", n, child.title, if sub.deleted > 0 { "deleted too" } else { "updated too" }));
                }
            }
        }
        Ok(())
    }

    /// Problems that block saving a table: missing IDs, empty required
    /// columns, broken rules (rows typed into the sheet bit by bit).
    pub fn row_problems(&mut self, key: &str, t: usize) -> Vec<String> {
        let Some(di) = self.draft_idx(key) else { return vec![] };
        let Some(tb) = self.drafts[di].tables.get(t).cloned() else { return vec![] };
        let rows = self.sheet_rows(key, t);
        let mut out = vec![];
        for (i, r) in rows.iter().enumerate() {
            if r.ghost.is_none() || r.state == RowState::Deleted {
                continue;
            }
            let label = format!("{} row {}", tb.title, i + 1);
            for (c, m) in tb.meta.iter().enumerate() {
                let empty = r.vals.get(c).map(|v| v.is_null()).unwrap_or(true);
                if empty && c == tb.id_col {
                    out.push(format!("{}: needs a value in {}", label, tb.columns[c]));
                } else if empty && m.not_null && !m.auto_inc {
                    out.push(format!("{}: {} can't be empty", label, tb.columns[c]));
                }
            }
            for ck in &tb.keys.checks {
                if let Ok(false) = self.check_ok(&tb, &ck.expr, &r.vals) {
                    out.push(format!("{}: breaks the rule {}", label, ck.name));
                }
            }
            if out.len() > 20 {
                break;
            }
        }
        out
    }
}

fn dup_msg(tb: &DraftTable, name: &str, cols: &[usize], vals: &[Json]) -> String {
    let v: Vec<String> = cols.iter().map(|&c| vals[c].cell_text()).collect();
    let cn: Vec<String> = cols.iter().map(|&c| tb.columns[c].clone()).collect();
    if name == "PRIMARY" {
        format!("A row with {} = '{}' already exists (duplicate ID)", cn.join(", "), v.join(", "))
    } else {
        format!("'{}' is already used in {} — it must be unique (key {})", v.join(", "), cn.join(", "), name)
    }
}
