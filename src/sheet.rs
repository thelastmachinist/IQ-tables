//! The spreadsheet model behind the editor and the SQL console: a table's
//! saved records (read from the chain, or known locally) with pending edits
//! ("ghost rows") laid over them. Editing a saved record creates a pending
//! copy; deleting one creates a pending tombstone; saving inscribes only the
//! pending rows as packs.

use std::collections::{HashMap, HashSet};

use crate::json::Json;
use crate::state::{DraftTable, GhostRow};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RowState {
    Saved,
    New,
    Changed,
    Deleted,
}

/// A saved record: (id, storage key → value pairs, who wrote it).
#[derive(Clone, Debug)]
pub struct BaseRec {
    pub key: String,
    pub vals: Vec<(String, Json)>,
    pub signer: String,
}

#[derive(Clone, Debug)]
pub struct SRow {
    /// Current values, aligned to the table's columns.
    pub vals: Vec<Json>,
    /// Saved values (for saved, changed and deleted rows).
    pub base: Option<Vec<Json>>,
    /// Index of the pending edit in `DraftTable.rows`.
    pub ghost: Option<usize>,
    pub state: RowState,
    pub signer: String,
}

impl SRow {
    pub fn changed(&self, c: usize) -> bool {
        self.state == RowState::Changed && self.base.as_ref().and_then(|b| b.get(c)) != self.vals.get(c)
    }
}

fn padded(v: &[Json], n: usize) -> Vec<Json> {
    let mut v = v.to_vec();
    v.resize(n, Json::Null);
    v
}

/// Saved records known locally: rows this browser inscribed, latest wins.
pub fn local_base(tb: &DraftTable) -> Vec<BaseRec> {
    let mut order: Vec<String> = vec![];
    let mut map: HashMap<String, BaseRec> = HashMap::new();
    for r in tb.rows.iter().filter(|r| r.sig.is_some()) {
        let key = r.vals.get(tb.id_col).map(|v| v.cell_text()).unwrap_or_default();
        if r.deleted {
            map.remove(&key);
            continue;
        }
        if !map.contains_key(&key) && !order.contains(&key) {
            order.push(key.clone());
        }
        map.insert(key.clone(), BaseRec { key, vals: tb.col_keys().into_iter().zip(padded(&r.vals, tb.columns.len())).collect(), signer: String::new() });
    }
    order.into_iter().filter_map(|k| map.remove(&k)).collect()
}

/// Apply pending edits over the saved records.
pub fn rows(tb: &DraftTable, base: &[BaseRec]) -> Vec<SRow> {
    let n = tb.columns.len();
    let id = tb.id_col;
    let mut pend: HashMap<String, usize> = HashMap::new();
    for (i, g) in tb.rows.iter().enumerate() {
        if g.sig.is_none() {
            let k = g.vals.get(id).map(|v| v.cell_text()).unwrap_or_default();
            if !k.is_empty() {
                pend.insert(k, i);
            }
        }
    }
    let mut used: HashSet<usize> = HashSet::new();
    let mut out = vec![];
    for b in base {
        let bv = crate::schema::align(&tb.meta, &b.vals);
        // keyed by the current ID column (it may differ from when it was saved)
        let bkey = match bv.get(id).map(|v| v.cell_text()) {
            Some(k) if !k.is_empty() => k,
            _ => b.key.clone(),
        };
        match pend.get(&bkey) {
            Some(&g) => {
                used.insert(g);
                let gr = &tb.rows[g];
                if gr.deleted {
                    out.push(SRow { vals: bv.clone(), base: Some(bv), ghost: Some(g), state: RowState::Deleted, signer: b.signer.clone() });
                } else {
                    out.push(SRow { vals: padded(&gr.vals, n), base: Some(bv), ghost: Some(g), state: RowState::Changed, signer: b.signer.clone() });
                }
            }
            None => out.push(SRow { vals: bv.clone(), base: Some(bv), ghost: None, state: RowState::Saved, signer: b.signer.clone() }),
        }
    }
    for (i, g) in tb.rows.iter().enumerate() {
        if g.sig.is_some() || used.contains(&i) {
            continue;
        }
        let state = if g.deleted { RowState::Deleted } else { RowState::New };
        out.push(SRow { vals: padded(&g.vals, n), base: None, ghost: Some(i), state, signer: String::new() });
    }
    out
}

/// Counts of pending changes: (new, changed, deleted).
pub fn pending(rows: &[SRow]) -> (usize, usize, usize) {
    let mut c = (0, 0, 0);
    for r in rows {
        match r.state {
            RowState::New => c.0 += 1,
            RowState::Changed => c.1 += 1,
            RowState::Deleted => c.2 += 1,
            RowState::Saved => {}
        }
    }
    c
}

// ---------------------------------------------------------------- mutations

pub fn set_cell(tb: &mut DraftTable, row: &SRow, c: usize, val: Json) -> Result<bool, String> {
    let n = tb.columns.len();
    if c >= n {
        return Err("No such column".into());
    }
    if c == tb.id_col && row.base.is_some() && row.state != RowState::New {
        let cur = row.vals.get(c).cloned().unwrap_or(Json::Null);
        if cur == val {
            return Ok(false);
        }
        return Err(format!("\"{}\" is this row's ID and it's already saved, so it can't change. Delete the row and add a new one instead.", tb.columns[c]));
    }
    match (row.state, row.ghost) {
        (RowState::Deleted, _) => Err("This row is marked for deletion. Undo the delete first.".into()),
        (_, Some(g)) => {
            let r = &mut tb.rows[g];
            r.vals.resize(n, Json::Null);
            if r.vals[c] == val {
                return Ok(false);
            }
            r.vals[c] = val;
            // edited back to exactly what's saved: nothing pending any more
            if let Some(b) = &row.base {
                if padded(&tb.rows[g].vals, n) == *b {
                    tb.rows.remove(g);
                }
            }
            Ok(true)
        }
        (_, None) => {
            let mut vals = row.base.clone().unwrap_or_else(|| vec![Json::Null; n]);
            vals.resize(n, Json::Null);
            if vals[c] == val {
                return Ok(false);
            }
            vals[c] = val;
            tb.rows.push(GhostRow { vals, deleted: false, sig: None });
            Ok(true)
        }
    }
}

/// Replace whole rows at once (paste, SQL UPDATE). Rows keep their ghost
/// indices valid by appending first and removing reverted ghosts last.
pub fn apply_rows(tb: &mut DraftTable, updates: Vec<(SRow, Vec<Json>)>) -> Vec<String> {
    let n = tb.columns.len();
    let id = tb.id_col;
    let mut errs = vec![];
    let mut remove = vec![];
    for (row, mut vals) in updates {
        vals.resize(n, Json::Null);
        if vals == row.vals {
            continue;
        }
        if row.state == RowState::Deleted {
            errs.push("A row marked for deletion can't be edited — undo the delete first.".into());
            continue;
        }
        if row.state != RowState::New && row.base.is_some() && vals.get(id) != row.vals.get(id) {
            errs.push(format!("\"{}\" is a saved row's ID, so it can't change.", tb.columns.get(id).cloned().unwrap_or_default()));
            continue;
        }
        match row.ghost {
            Some(g) => {
                if row.base.as_ref() == Some(&vals) {
                    remove.push(g);
                } else {
                    tb.rows[g].vals = vals;
                }
            }
            None => {
                if row.base.as_ref() != Some(&vals) {
                    tb.rows.push(GhostRow { vals, deleted: false, sig: None });
                }
            }
        }
    }
    remove.sort_unstable();
    remove.dedup();
    for g in remove.into_iter().rev() {
        tb.rows.remove(g);
    }
    errs
}

pub fn insert_row(tb: &mut DraftTable) {
    tb.fix_meta();
    tb.rows.push(GhostRow { vals: vec![Json::Null; tb.columns.len()], deleted: false, sig: None });
}

/// Delete (or, for rows already marked, restore) several rows at once.
pub fn delete_rows(tb: &mut DraftTable, rows: &[&SRow]) {
    let n = tb.columns.len();
    let id = tb.id_col;
    let mut remove: Vec<usize> = vec![];
    let mut push: Vec<GhostRow> = vec![];
    for r in rows {
        let tomb = || {
            let mut vals = vec![Json::Null; n];
            vals[id] = r.vals.get(id).cloned().unwrap_or(Json::Null);
            GhostRow { vals, deleted: true, sig: None }
        };
        match (r.state, r.ghost) {
            (RowState::New, Some(g)) | (RowState::Deleted, Some(g)) => remove.push(g),
            (RowState::Changed, Some(g)) => {
                let t = tomb();
                tb.rows[g] = t;
            }
            (RowState::Saved, _) => push.push(tomb()),
            _ => {}
        }
    }
    tb.rows.extend(push);
    remove.sort_unstable();
    remove.dedup();
    for g in remove.into_iter().rev() {
        tb.rows.remove(g);
    }
}

/// Throw away every pending edit.
pub fn discard(tb: &mut DraftTable) {
    tb.rows.retain(|r| r.sig.is_some());
}

fn has_saved_data(tb: &DraftTable, base: &[BaseRec]) -> bool {
    tb.created.is_some() || !base.is_empty() || tb.rows.iter().any(|r| r.sig.is_some())
}

pub fn add_column(tb: &mut DraftTable, name: &str, at: Option<usize>) -> Result<usize, String> {
    add_column_def(tb, name, crate::schema::ColMeta::plain(""), at)
}

/// Add a column with a type and constraints; `meta.key` is filled in.
pub fn add_column_def(tb: &mut DraftTable, name: &str, mut meta: crate::schema::ColMeta, at: Option<usize>) -> Result<usize, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the column a name".into());
    }
    if name.chars().count() > 64 {
        return Err("Column names are at most 64 characters".into());
    }
    if tb.columns.iter().any(|c| c.eq_ignore_ascii_case(name)) {
        return Err(format!("There's already a column called \"{}\"", name));
    }
    tb.fix_meta();
    meta.key = crate::schema::fresh_key(name, &tb.col_keys(), &tb.keys.retired);
    let at = at.unwrap_or(tb.columns.len()).min(tb.columns.len());
    tb.columns.insert(at, name.to_string());
    tb.meta.insert(at, meta);
    for r in tb.rows.iter_mut() {
        if r.vals.len() >= at {
            r.vals.insert(at, Json::Null);
        }
    }
    if at <= tb.id_col && tb.columns.len() > 1 {
        tb.id_col += 1;
    }
    Ok(at)
}

/// A fresh column name like "column_4".
pub fn next_column_name(tb: &DraftTable) -> String {
    let mut i = tb.columns.len() + 1;
    loop {
        let n = format!("column_{}", i);
        if !tb.columns.iter().any(|c| c == &n) {
            return n;
        }
        i += 1;
    }
}

pub fn delete_column(tb: &mut DraftTable, c: usize) -> Result<(), String> {
    if c >= tb.columns.len() {
        return Err("No such column".into());
    }
    if c == tb.id_col {
        return Err("That's the ID column; every row needs one.".into());
    }
    tb.fix_meta();
    let key = tb.meta[c].key.clone();
    if let Some(f) = tb.keys.fks.iter().find(|f| f.cols.contains(&key)) {
        return Err(format!("\"{}\" is used by the link {} — remove the link first.", tb.columns[c], f.name));
    }
    for ix in tb.keys.indexes.iter_mut() {
        ix.cols.retain(|k| *k != key);
    }
    tb.keys.indexes.retain(|ix| !ix.cols.is_empty());
    if (tb.created.is_some() || tb.rows.iter().any(|r| r.sig.is_some())) && !tb.keys.retired.contains(&key) {
        tb.keys.retired.push(key);
    }
    tb.columns.remove(c);
    tb.meta.remove(c);
    for r in tb.rows.iter_mut() {
        if c < r.vals.len() {
            r.vals.remove(c);
        }
    }
    if c < tb.id_col {
        tb.id_col -= 1;
    }
    Ok(())
}

pub fn rename_column(tb: &mut DraftTable, base: &[BaseRec], c: usize, name: &str) -> Result<(), String> {
    let name = name.trim();
    if c >= tb.columns.len() || name.is_empty() {
        return Err("Give the column a name".into());
    }
    let _ = base;
    if tb.columns[c] == name {
        return Ok(());
    }
    if name.chars().count() > 64 {
        return Err("Column names are at most 64 characters".into());
    }
    if tb.columns.iter().enumerate().any(|(i, x)| i != c && x.eq_ignore_ascii_case(name)) {
        return Err(format!("There's already a column called \"{}\"", name));
    }
    tb.columns[c] = name.to_string();
    Ok(())
}

/// Make column `c` the primary key. Every live row needs a value in it and
/// no two rows may share one; saved rows are re-keyed when this is saved.
pub fn set_id_column(tb: &mut DraftTable, base: &[BaseRec], c: usize) -> Result<(), String> {
    if c >= tb.columns.len() {
        return Err("No such column".into());
    }
    if c == tb.id_col {
        return Ok(());
    }
    let rows = rows(tb, base);
    let mut seen = HashSet::new();
    for r in rows.iter().filter(|r| r.state != RowState::Deleted) {
        let k = r.vals.get(c).map(|v| v.cell_text()).unwrap_or_default();
        if k.is_empty() {
            return Err(format!("\"{}\" can't be the ID: some rows have no value in it.", tb.columns[c]));
        }
        if !seen.insert(k.to_lowercase()) {
            return Err(format!("\"{}\" can't be the ID: the value “{}” appears more than once.", tb.columns[c], k));
        }
    }
    let _ = has_saved_data(tb, base);
    // pending rows were matched to saved ones by the old ID: re-point them
    let old = tb.id_col;
    let saved: HashMap<String, Json> = rows.iter().filter_map(|r| r.base.as_ref().map(|b| (b.get(old).map(|v| v.cell_text()).unwrap_or_default(), b.get(c).cloned().unwrap_or(Json::Null)))).collect();
    for g in tb.rows.iter_mut().filter(|g| g.sig.is_none() && g.deleted) {
        let k = g.vals.get(old).map(|v| v.cell_text()).unwrap_or_default();
        if let Some(v) = saved.get(&k) {
            g.vals.resize(c.max(old) + 1, Json::Null);
            g.vals[c] = v.clone();
        }
    }
    if let Some(m) = tb.meta.get_mut(c) {
        m.not_null = true;
    }
    tb.id_col = c;
    Ok(())
}

pub fn move_column(tb: &mut DraftTable, c: usize, to: usize) {
    let n = tb.columns.len();
    if c >= n || to >= n || c == to {
        return;
    }
    tb.fix_meta();
    let col = tb.columns.remove(c);
    tb.columns.insert(to, col);
    let m = tb.meta.remove(c);
    tb.meta.insert(to, m);
    for r in tb.rows.iter_mut() {
        r.vals.resize(n, Json::Null);
        let v = r.vals.remove(c);
        r.vals.insert(to, v);
    }
    let id = tb.id_col;
    tb.id_col = if id == c {
        to
    } else if c < id && to >= id {
        id - 1
    } else if c > id && to <= id {
        id + 1
    } else {
        id
    };
}

/// Make sure every column seen in saved data exists in the table.
pub fn adopt_columns(tb: &mut DraftTable, cols: &[String], id_name: Option<&str>) -> bool {
    let mut changed = false;
    if tb.columns.is_empty() {
        tb.columns = cols.to_vec();
        tb.meta = cols.iter().map(|c| crate::schema::ColMeta::plain(c)).collect();
        tb.id_col = id_name.and_then(|n| cols.iter().position(|c| c == n)).unwrap_or(0);
        for r in tb.rows.iter_mut() {
            r.vals.resize(cols.len(), Json::Null);
        }
        return !cols.is_empty();
    }
    tb.fix_meta();
    for c in cols {
        if !tb.meta.iter().any(|m| &m.key == c) && !tb.keys.retired.contains(c) {
            let at = tb.columns.len();
            tb.columns.push(c.clone());
            tb.meta.push(crate::schema::ColMeta::plain(c));
            for r in tb.rows.iter_mut() {
                if r.vals.len() >= at {
                    r.vals.push(Json::Null);
                }
            }
            changed = true;
        }
    }
    changed
}

/// Tab-separated text (what Excel and Google Sheets put on the clipboard).
pub fn parse_tsv(text: &str) -> Vec<Vec<String>> {
    let t = text.strip_suffix("\r\n").or_else(|| text.strip_suffix('\n')).unwrap_or(text);
    if !t.contains('\t') && !t.contains('\n') {
        return vec![vec![t.to_string()]];
    }
    // Excel quotes cells that contain tabs/newlines; reuse the CSV reader.
    let mut rows = vec![];
    let mut row = vec![];
    let mut cell = String::new();
    let mut q = false;
    let mut it = t.chars().peekable();
    while let Some(c) = it.next() {
        if q {
            if c == '"' {
                if it.peek() == Some(&'"') {
                    cell.push('"');
                    it.next();
                } else {
                    q = false;
                }
            } else {
                cell.push(c);
            }
        } else if c == '"' && cell.is_empty() {
            q = true;
        } else if c == '\t' {
            row.push(std::mem::take(&mut cell));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && it.peek() == Some(&'\n') {
                it.next();
            }
            row.push(std::mem::take(&mut cell));
            rows.push(std::mem::take(&mut row));
        } else {
            cell.push(c);
        }
    }
    row.push(cell);
    rows.push(row);
    rows
}

pub fn to_tsv(cells: &[Vec<String>]) -> String {
    cells
        .iter()
        .map(|r| {
            r.iter()
                .map(|c| if c.contains('\t') || c.contains('\n') || c.contains('"') { format!("\"{}\"", c.replace('"', "\"\"")) } else { c.clone() })
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// "A", "B", …, "Z", "AA", … like a spreadsheet.
pub fn col_letter(mut c: usize) -> String {
    let mut s = vec![];
    loop {
        s.push((b'A' + (c % 26) as u8) as char);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.iter().rev().collect()
}
