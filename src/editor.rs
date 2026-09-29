//! The editor: spreadsheet state and events, saved-record bases loaded from
//! the chain, undo/redo, and "Save to blockchain" with automatic top-up.

use std::collections::HashMap;
use std::rc::Rc;

use crate::app::{fetch_err, After, App, Load, Mode, TableView, Who, P};
use crate::host;
use crate::iq;
use crate::json::Json;
use crate::net;
use crate::pack;
use crate::sheet::{self, BaseRec, RowState, SRow};
use crate::solana::b58;
use crate::state::{DraftTable, GhostRow};
use crate::ui;

pub const SHEET_PAGE: usize = 100;

/// Where a table's saved records came from.
#[derive(Clone, Debug, PartialEq)]
pub enum BaseState {
    /// Not on the blockchain yet (or only known from this browser).
    Local,
    Loading,
    Chain,
    Err(String),
}

#[derive(Default)]
pub struct Ed {
    pub tab: String,
    /// (display row, column) of the active cell and the other corner of the range.
    pub sel: (usize, usize),
    pub anchor: (usize, usize),
    /// Some(initial text) while a cell is being edited.
    pub editing: Option<String>,
    pub sort: Option<(usize, bool)>,
    pub filter: String,
    pub menu: Option<usize>,
    pub renaming: Option<usize>,
    pub last_down: (f64, usize, usize),
    pub table: (String, usize),
    pub sql_text: HashMap<String, String>,
    pub sql_out: Vec<crate::sql_exec::Out>,
    pub sql_hist: Vec<String>,
    pub sql_wait: Option<(String, String)>,
    pub scroll: bool,
    /// Session state of the SQL console: @variables, LAST_INSERT_ID, FOREIGN_KEY_CHECKS.
    pub sql_vars: HashMap<String, Json>,
    pub last_insert_id: u64,
    /// The last statement the UI ran for you (shown like phpMyAdmin does).
    pub last_sql: Option<String>,
    /// Showing the database (its tables) rather than one table.
    pub scope_db: bool,
    /// Column form open on the Structure tab: Some(column) or Some(usize::MAX) for a new one.
    pub col_edit: Option<usize>,
    /// A destructive button waiting for its second click ("action:arg").
    pub confirm: Option<String>,
    pub search_out: Vec<crate::sql_exec::Out>,
    pub import_out: Vec<crate::sql_exec::Out>,
}

#[derive(Default)]
pub struct Undo {
    pub undo: Vec<DraftTable>,
    pub redo: Vec<DraftTable>,
}

impl App {
    // ------------------------------------------------------------ bases

    pub fn table_pda_of(&self, key: &str, t: usize) -> Option<(String, String)> {
        let d = self.drafts.get(self.draft_idx(key)?)?;
        let tb = d.tables.get(t)?;
        let root = iq::db_root_pda(d.name.as_bytes());
        Some((b58(&root), b58(&iq::table_pda(&root, &iq::seed_bytes(&tb.name)))))
    }

    /// Start reading a table's saved records from the chain (once).
    pub fn ensure_base(&mut self, key: &str, t: usize) {
        let Some(i) = self.draft_idx(key) else { return };
        if self.drafts[i].tables.get(t).map(|tb| tb.created.is_none()).unwrap_or(true) {
            return;
        }
        let Some((root, pda)) = self.table_pda_of(key, t) else { return };
        if let Some(tv) = self.bases.get(&pda) {
            if tv.err.is_none() || tv.loading {
                return;
            }
        }
        self.load_base(root, pda, false);
    }

    fn load_base(&mut self, root: String, pda: String, fresh: bool) {
        let gen = self.nid();
        self.bases.insert(
            pda.clone(),
            TableView {
                pda: pda.clone(),
                root: Some(root),
                creator: None,
                db_id: None,
                label: None,
                meta: Load::None,
                rows: vec![],
                decoded: vec![],
                cursor: None,
                loading: true,
                done: false,
                err: None,
                load_all: true,
                cut: false,
                chunk_waits: 0,
                mode: Mode::Records,
                who: Who::All,
                text: String::new(),
                sort: None,
                page: 0,
                selected: None,
                gen,
            },
        );
        // the table's name and writers as the chain has them
        let params = crate::json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"commitment\":\"confirmed\"}}]", pda)).unwrap();
        self.rpc("getAccountInfo", params, crate::app::P::BaseAcct(pda.clone()));
        if self.use_rpc() {
            self.rpc_rows(&pda, gen, None);
        } else {
            let q = if fresh { "&fresh=1" } else { "" };
            self.get(&format!("/table/{}/rows?limit=100{}", pda, q), P::Rows(pda, gen));
        }
    }

    /// Re-read a table's records (after a save).
    pub fn reload_base(&mut self, key: &str, t: usize) {
        if let Some((root, pda)) = self.table_pda_of(key, t) {
            if self.bases.contains_key(&pda) {
                self.load_base(root, pda, true);
            }
        }
    }

    /// A base finished loading: adopt the table's structure (the owner's
    /// latest structure record, or the columns seen in its packs), then run
    /// a waiting SQL query.
    pub fn base_loaded(&mut self, pda: &str) {
        let Some(tv) = self.bases.get(pda) else { return };
        let packs: Vec<pack::SourcePack> = tv.decoded.iter().rev().filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok())).cloned().collect();
        let mut cols: Vec<String> = vec![];
        let mut id_name: Option<String> = None;
        for p in packs.iter().rev().filter(|p| p.meta.is_none()) {
            for c in &p.schema.cols {
                if !cols.contains(c) {
                    cols.push(c.clone());
                }
            }
            id_name = Some(p.schema.id_col().to_string());
        }
        let mut touched = vec![];
        let mut gone = vec![];
        for di in 0..self.drafts.len() {
            for t in 0..self.drafts[di].tables.len() {
                let key = self.drafts[di].key.clone();
                if !self.table_pda_of(&key, t).map(|(_, p)| p == pda).unwrap_or(false) {
                    continue;
                }
                let mut owners: Vec<String> = self.drafts[di].wallet.iter().cloned().collect();
                if let Some(c) = self.creator_of(&key) {
                    owners = vec![c];
                }
                let official = |s: &str| owners.is_empty() || owners.iter().any(|o| o == s);
                let (_, doc) = pack::merge_events(&packs, &official, &|_| false);
                let tb = &mut self.drafts[di].tables[t];
                match doc {
                    Some(d) => {
                        let mut plain = d.clone();
                        plain.clear = false;
                        plain.dropped = false;
                        let text = plain.to_json().to_string();
                        if tb.chain_doc.as_deref() == Some(text.as_str()) {
                            continue;
                        }
                        if d.dropped && !tb.dropped && tb.ghosts() == 0 {
                            gone.push((key.clone(), t));
                            continue;
                        }
                        let local_edit = match &tb.chain_doc {
                            Some(c) => tb.doc_text() != *c,
                            None => !tb.columns.is_empty() && !tb.doc().is_trivial(),
                        };
                        if !local_edit {
                            tb.apply_doc(&plain);
                        }
                        tb.chain_doc = Some(text);
                        touched.push((key, t));
                    }
                    None => {
                        if sheet::adopt_columns(tb, &cols, id_name.as_deref()) {
                            touched.push((key, t));
                        }
                    }
                }
            }
        }
        for (k, t) in touched {
            self.bump(&k, t);
        }
        for (k, t) in gone.into_iter().rev() {
            if let Some(i) = self.draft_idx(&k) {
                self.drafts[i].tables.remove(t);
                self.drafts[i].sel = 0;
                self.table_removed(&k, t);
            }
        }
        self.save_drafts();
        if let Some((key, text)) = self.ed.sql_wait.take() {
            self.ed.sql_out = self.run_sql(&key, &text);
        }
    }

    /// A table's on-chain name and writers, for editor tables that match it
    /// and have no rename or writer change of their own waiting.
    pub fn adopt_table_meta(&mut self, pda: &str, m: &iq::TableMeta) {
        for di in 0..self.drafts.len() {
            let key = self.drafts[di].key.clone();
            let owner = self.creator_of(&key).or_else(|| self.drafts[di].wallet.clone());
            for t in 0..self.drafts[di].tables.len() {
                if !self.table_pda_of(&key, t).map(|(_, p)| p == pda).unwrap_or(false) {
                    continue;
                }
                let taken = |d: &crate::state::Draft, n: &str| {
                    d.tables.iter().enumerate().any(|(j, x)| j != t && !x.dropped && (x.title.eq_ignore_ascii_case(n) || x.name.eq_ignore_ascii_case(n)))
                };
                let name_ok = !m.name.is_empty() && m.name.len() <= 64 && !taken(&self.drafts[di], &m.name);
                let tb = &mut self.drafts[di].tables[t];
                if tb.chain_title.is_none() && name_ok {
                    tb.title = m.name.clone();
                }
                if tb.chain_writers.is_none() {
                    let ws: Vec<String> = m.writers.iter().map(b58).collect();
                    tb.open = ws.is_empty();
                    tb.writers = ws.into_iter().filter(|w| Some(w) != owner.as_ref()).collect();
                }
                self.bump(&key, t);
            }
        }
        self.save_drafts();
    }

    pub fn creator_of(&self, key: &str) -> Option<String> {
        match self.name_checks.get(key) {
            Some(Load::Ready(Some(c))) => Some(c.clone()),
            _ => None,
        }
    }

    /// Saved records of a draft table: from the chain when it's there (the
    /// owner's and this draft's wallet's records), with rows this browser
    /// inscribed laid on top until the chain read catches up.
    pub fn sheet_base(&mut self, key: &str, t: usize) -> (Rc<Vec<BaseRec>>, BaseState) {
        let Some(i) = self.draft_idx(key) else { return (Rc::new(vec![]), BaseState::Local) };
        let Some(tb) = self.drafts[i].tables.get(t) else { return (Rc::new(vec![]), BaseState::Local) };
        if tb.created.is_none() {
            return (Rc::new(sheet::local_base(tb)), BaseState::Local);
        }
        if tb.clear {
            // TRUNCATE / DROP waiting to be saved: nothing saved counts
            return (Rc::new(vec![]), BaseState::Chain);
        }
        let Some((_, pda)) = self.table_pda_of(key, t) else { return (Rc::new(vec![]), BaseState::Local) };
        let Some(tv) = self.bases.get(&pda) else {
            return (Rc::new(sheet::local_base(tb)), BaseState::Loading);
        };
        if tv.rows.is_empty() && !tv.done {
            let st = match &tv.err {
                Some(e) => BaseState::Err(e.clone()),
                None => BaseState::Loading,
            };
            return (Rc::new(sheet::local_base(tb)), st);
        }
        let state = match (&tv.err, tv.done) {
            (Some(e), _) => BaseState::Err(e.clone()),
            (None, true) => BaseState::Chain,
            (None, false) => BaseState::Loading,
        };
        let stamp = format!("{}:{}:{}:{}", tv.gen, tv.rows.len(), tb.rows.iter().filter(|r| r.sig.is_some()).count(), tb.col_keys().join(","));
        if let Some((s, b)) = self.base_cache.get(&pda) {
            if *s == stamp {
                return (b.clone(), state);
            }
        }
        let mut allowed: Vec<String> = vec![];
        if let Some(w) = &self.drafts[i].wallet {
            allowed.push(w.clone());
        }
        let creator = self.creator_of(key);
        if let Some(c) = &creator {
            allowed.push(c.clone());
        }
        let packs: Vec<pack::SourcePack> = tv.decoded.iter().rev().filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok())).cloned().collect();
        let owner: Vec<String> = match &creator {
            Some(c) => vec![c.clone()],
            None => self.drafts[i].wallet.iter().cloned().collect(),
        };
        let official = |s: &str| owner.is_empty() || owner.iter().any(|o| o == s);
        let take = |p: &pack::SourcePack| allowed.is_empty() || allowed.contains(&p.signer);
        let seen: std::collections::HashSet<String> = tv.rows.iter().map(|r| r.get("__txSignature").str_or("")).collect();
        let mut out: Vec<BaseRec> =
            pack::merge_events(&packs, &official, &take).0.into_iter().map(|m| BaseRec { key: m.key, vals: m.vals, signer: m.signer }).collect();
        // our own recent writes the chain read doesn't include yet
        for r in tb.rows.iter().filter(|r| r.sig.as_ref().map(|s| !seen.contains(s)).unwrap_or(false)) {
            let k = r.vals.get(tb.id_col).map(|v| v.cell_text()).unwrap_or_default();
            let pos = out.iter().position(|b| b.key == k);
            if r.deleted {
                if let Some(p) = pos {
                    out.remove(p);
                }
                continue;
            }
            let rec = BaseRec {
                key: k,
                vals: tb.col_keys().into_iter().zip(r.vals.iter().cloned()).collect(),
                signer: self.drafts[i].wallet.clone().unwrap_or_default(),
            };
            match pos {
                Some(p) => out[p] = rec,
                None => out.push(rec),
            }
        }
        let rc = Rc::new(out);
        self.base_cache.insert(pda, (stamp, rc.clone()));
        (rc, state)
    }

    pub fn sheet_rows(&mut self, key: &str, t: usize) -> Vec<SRow> {
        let (base, _) = self.sheet_base(key, t);
        match self.draft_idx(key).and_then(|i| self.drafts[i].tables.get(t)) {
            Some(tb) => sheet::rows(tb, &base),
            None => vec![],
        }
    }

    /// Display order after the sheet's sort and filter: indices into rows.
    pub fn sheet_view(&mut self, key: &str, t: usize) -> (Vec<SRow>, Vec<usize>) {
        let rows = self.sheet_rows(key, t);
        let q = self.ed.filter.to_lowercase();
        let mut order: Vec<usize> =
            (0..rows.len()).filter(|&i| q.is_empty() || rows[i].vals.iter().any(|v| v.cell_text().to_lowercase().contains(&q))).collect();
        if let Some((c, desc)) = self.ed.sort {
            order.sort_by(|&a, &b| {
                let x = rows[a].vals.get(c).map(|v| v.cell_text()).unwrap_or_default();
                let y = rows[b].vals.get(c).map(|v| v.cell_text()).unwrap_or_default();
                let o = ui::cmp_cells(&x, &y);
                if desc {
                    o.reverse()
                } else {
                    o
                }
            });
        }
        (rows, order)
    }

    // --------------------------------------------------------- mutations

    /// Run `f` on a draft table with an undo snapshot; saves and re-plans.
    pub fn edit_tb<R>(&mut self, key: &str, t: usize, f: impl FnOnce(&mut DraftTable, &[BaseRec]) -> R) -> Option<R> {
        let (base, _) = self.sheet_base(key, t);
        let i = self.draft_idx(key)?;
        let snap = self.drafts[i].tables.get(t)?.clone();
        let r = f(&mut self.drafts[i].tables[t], &base);
        let after = &self.drafts[i].tables[t];
        if after.rows != snap.rows
            || after.columns != snap.columns
            || after.id_col != snap.id_col
            || after.open != snap.open
            || after.name != snap.name
            || after.title != snap.title
            || after.writers != snap.writers
            || after.clear != snap.clear
            || after.dropped != snap.dropped
            || after.meta != snap.meta
            || after.keys != snap.keys
        {
            let u = self.undo.entry((key.to_string(), t)).or_default();
            u.undo.push(snap);
            if u.undo.len() > 100 {
                u.undo.remove(0);
            }
            u.redo.clear();
            self.bump(key, t);
            self.save_drafts();
        }
        Some(r)
    }

    pub fn undo(&mut self, key: &str, t: usize, redo: bool) {
        let Some(i) = self.draft_idx(key) else { return };
        let Some(cur) = self.drafts[i].tables.get(t).cloned() else { return };
        let u = self.undo.entry((key.to_string(), t)).or_default();
        let prev = if redo { u.redo.pop() } else { u.undo.pop() };
        let Some(mut prev) = prev else {
            self.err(if redo { "Nothing to redo" } else { "Nothing to undo" });
            return;
        };
        // never undo past something that was saved meanwhile
        for (r, c) in prev.rows.iter_mut().zip(cur.rows.iter()) {
            if c.sig.is_some() && r.vals == c.vals && r.deleted == c.deleted {
                r.sig = c.sig.clone();
            }
        }
        if redo {
            u.undo.push(cur);
        } else {
            u.redo.push(cur);
        }
        self.drafts[i].tables[t] = prev;
        self.bump(key, t);
        self.save_drafts();
    }

    fn cur_table(&self) -> Option<(String, usize)> {
        match &self.route {
            crate::app::Route::Draft(k) => {
                let i = self.draft_idx(k)?;
                let t = self.drafts[i].sel;
                (t < self.drafts[i].tables.len()).then(|| (k.clone(), t))
            }
            _ => None,
        }
    }

    /// Make sure the selection belongs to the table on screen.
    pub fn sync_sheet(&mut self) {
        let Some(cur) = self.cur_table() else { return };
        if self.ed.table != cur {
            self.ed.table = cur.clone();
            self.ed.sel = (0, 0);
            self.ed.anchor = (0, 0);
            self.ed.editing = None;
            self.ed.sort = None;
            self.ed.filter.clear();
            self.ed.menu = None;
            self.ed.renaming = None;
            if let Some(f) = self.pending_filter.take() {
                self.ed.filter = f;
            }
            self.ensure_base(&cur.0, cur.1);
        }
    }

    fn range(&self) -> ((usize, usize), (usize, usize)) {
        let (a, b) = (self.ed.sel, self.ed.anchor);
        ((a.0.min(b.0), a.1.min(b.1)), (a.0.max(b.0), a.1.max(b.1)))
    }

    fn ncols(&self, key: &str, t: usize) -> usize {
        self.draft_idx(key).and_then(|i| self.drafts[i].tables.get(t)).map(|tb| tb.columns.len()).unwrap_or(0)
    }

    /// Put `val` into display cell (r, c); the blank line below the last row
    /// creates a new row. The table's rules apply (types, keys, links); a
    /// new row's other required cells can be filled in afterwards.
    fn set_display_cell(&mut self, key: &str, t: usize, r: usize, c: usize, val: Json) -> Result<bool, String> {
        let (rows, order) = self.sheet_view(key, t);
        let n = self.ncols(key, t);
        if c >= n {
            return Ok(false);
        }
        let change = match order.get(r).map(|&i| rows[i].clone()) {
            Some(row) => {
                if row.state == RowState::Deleted {
                    return Err("This row is marked for deletion. Undo the delete first.".into());
                }
                let mut vals = row.vals.clone();
                vals.resize(n, Json::Null);
                if vals[c] == val {
                    return Ok(false);
                }
                vals[c] = val;
                let mut set = vec![false; n];
                set[c] = true;
                crate::constraints::Change::Update { row, vals, set }
            }
            None => {
                if val.is_null() {
                    return Ok(false);
                }
                let mut vals = vec![Json::Null; n];
                let mut given = vec![false; n];
                vals[c] = val;
                given[c] = true;
                crate::constraints::Change::Insert { vals, given }
            }
        };
        let fk = self.fk_checks();
        self.apply_changes(key, t, vec![change], &crate::constraints::Opts { strict: false, fk_checks: fk }).map(|a| a.inserted + a.updated + a.deleted > 0)
    }

    pub fn commit_edit(&mut self, text: &str) {
        let Some((key, t)) = self.cur_table() else { return };
        if self.ed.editing.take().is_none() {
            return;
        }
        let (r, c) = self.ed.sel;
        if let Err(e) = self.set_display_cell(&key, t, r, c, ui::typed(text)) {
            self.err(e);
        }
    }

    fn move_sel(&mut self, dr: i64, dc: i64, extend: bool, rows: usize, cols: usize) {
        let r = (self.ed.sel.0 as i64 + dr).clamp(0, rows as i64) as usize; // rows = blank line index
        let c = (self.ed.sel.1 as i64 + dc).clamp(0, cols.saturating_sub(1) as i64) as usize;
        self.ed.sel = (r, c);
        if !extend {
            self.ed.anchor = self.ed.sel;
        }
        self.ed.scroll = true;
    }

    fn selected_text(&mut self, key: &str, t: usize) -> Vec<Vec<String>> {
        let ((r0, c0), (r1, c1)) = self.range();
        let (rows, order) = self.sheet_view(key, t);
        (r0..=r1)
            .filter_map(|r| order.get(r).map(|&i| &rows[i]))
            .map(|row| (c0..=c1).map(|c| row.vals.get(c).map(|v| v.cell_text()).unwrap_or_default()).collect())
            .collect()
    }

    fn clear_range(&mut self, key: &str, t: usize) {
        let ((r0, c0), (r1, c1)) = self.range();
        let (rows, order) = self.sheet_view(key, t);
        let n = self.ncols(key, t);
        let mut changes = vec![];
        for r in r0..=r1 {
            let Some(&i) = order.get(r) else { continue };
            let row = rows[i].clone();
            if row.state == RowState::Deleted {
                continue;
            }
            let mut vals = row.vals.clone();
            vals.resize(n, Json::Null);
            let mut set = vec![false; n];
            let mut any = false;
            for c in c0..=c1.min(n.saturating_sub(1)) {
                if !vals[c].is_null() {
                    vals[c] = Json::Null;
                    set[c] = true;
                    any = true;
                }
            }
            if any {
                changes.push(crate::constraints::Change::Update { row, vals, set });
            }
        }
        if changes.is_empty() {
            return;
        }
        let fk = self.fk_checks();
        if let Err(e) = self.apply_changes(key, t, changes, &crate::constraints::Opts { strict: false, fk_checks: fk }) {
            self.err(e);
        }
    }

    /// Fold the last two undo steps into one (a paste that also added columns).
    fn merge_last_undo(&mut self, key: &str, t: usize) {
        if let Some(u) = self.undo.get_mut(&(key.to_string(), t)) {
            if u.undo.len() >= 2 {
                u.undo.pop();
            }
        }
    }

    /// Paste tab-separated text (from Excel/Sheets) at the active cell.
    pub fn paste(&mut self, text: &str) {
        let Some((key, t)) = self.cur_table() else { return };
        let grid = sheet::parse_tsv(text);
        if grid.is_empty() {
            return;
        }
        let (r0, c0) = self.ed.sel;
        let width = grid.iter().map(|r| r.len()).max().unwrap_or(0);
        // grow the table to fit, like a spreadsheet does
        let grew = self.ncols(&key, t) < c0 + width;
        if grew {
            self.edit_tb(&key, t, |tb, _| {
                while tb.columns.len() < c0 + width {
                    let n = sheet::next_column_name(tb);
                    if sheet::add_column(tb, &n, None).is_err() {
                        break;
                    }
                }
            });
        }
        let (rows, order) = self.sheet_view(&key, t);
        let n = self.ncols(&key, t);
        let mut changes = vec![];
        for (i, line) in grid.iter().enumerate() {
            match order.get(r0 + i).map(|&x| rows[x].clone()) {
                Some(row) => {
                    if row.state == RowState::Deleted {
                        continue;
                    }
                    let mut vals = row.vals.clone();
                    vals.resize(n, Json::Null);
                    let mut set = vec![false; n];
                    for (j, cell) in line.iter().enumerate() {
                        if c0 + j < n {
                            vals[c0 + j] = ui::typed(cell);
                            set[c0 + j] = true;
                        }
                    }
                    changes.push(crate::constraints::Change::Update { row, vals, set });
                }
                None => {
                    let mut vals = vec![Json::Null; n];
                    let mut given = vec![false; n];
                    for (j, cell) in line.iter().enumerate() {
                        if c0 + j < n {
                            vals[c0 + j] = ui::typed(cell);
                            given[c0 + j] = !vals[c0 + j].is_null();
                        }
                    }
                    if vals.iter().any(|v| !v.is_null()) {
                        changes.push(crate::constraints::Change::Insert { vals, given });
                    }
                }
            }
        }
        let fk = self.fk_checks();
        match self.apply_changes(&key, t, changes, &crate::constraints::Opts { strict: false, fk_checks: fk }) {
            Ok(_) => {
                if grew {
                    self.merge_last_undo(&key, t);
                }
                self.ed.anchor = (r0, c0);
                self.ed.sel = (r0 + grid.len() - 1, c0 + width - 1);
                self.ok(format!("Pasted {} row(s) × {} column(s)", grid.len(), width));
            }
            Err(e) => {
                if grew {
                    self.undo(&key, t, false);
                    if let Some(u) = self.undo.get_mut(&(key.clone(), t)) {
                        u.redo.pop();
                    }
                }
                self.err(format!("Nothing was pasted: {}", e));
            }
        }
    }

    // ------------------------------------------------------------ events

    /// Keyboard events from the sheet, the cell editor and the SQL box.
    /// Returns (handled → preventDefault, re-render).
    pub fn key(&mut self, action: &str, _arg: &str, key: &str, val: &str) -> (bool, bool) {
        match action {
            "editor" => match key {
                "Enter" | "Tab" | "Shift+Tab" | "Shift+Enter" | "ArrowUp" | "ArrowDown" => {
                    self.commit_edit(val);
                    let Some((k, t)) = self.cur_table() else { return (true, true) };
                    let (rows, order) = self.sheet_view(&k, t);
                    let _ = rows;
                    let (nr, nc) = (order.len(), self.ncols(&k, t));
                    match key {
                        "Enter" | "ArrowDown" => self.move_sel(1, 0, false, nr, nc),
                        "Shift+Enter" | "ArrowUp" => self.move_sel(-1, 0, false, nr, nc),
                        "Tab" => self.move_sel(0, 1, false, nr, nc),
                        _ => self.move_sel(0, -1, false, nr, nc),
                    }
                    (true, true)
                }
                "Escape" => {
                    self.ed.editing = None;
                    (true, true)
                }
                _ => (false, false),
            },
            "sql" => {
                if key == "Ctrl+Enter" {
                    if let Some((k, _)) = self.cur_table().or_else(|| self.cur_db().map(|k| (k, 0))) {
                        self.ed.sql_text.insert(k.clone(), val.to_string());
                        self.sql_run(&k);
                    }
                    return (true, true);
                }
                (false, false)
            }
            "sheet" => self.sheet_key(key),
            _ => (false, false),
        }
    }

    pub fn cur_db(&self) -> Option<String> {
        match &self.route {
            crate::app::Route::Draft(k) => Some(k.clone()),
            _ => None,
        }
    }

    fn sheet_key(&mut self, key: &str) -> (bool, bool) {
        let Some((k, t)) = self.cur_table() else { return (false, false) };
        if self.ed.editing.is_some() {
            return (false, false);
        }
        let (_, order) = self.sheet_view(&k, t);
        let (nr, nc) = (order.len(), self.ncols(&k, t));
        if nc == 0 {
            return (false, false);
        }
        self.ed.menu = None;
        let shift = key.starts_with("Shift+");
        let base = key.trim_start_matches("Shift+");
        match base {
            "ArrowDown" => self.move_sel(1, 0, shift, nr, nc),
            "ArrowUp" => self.move_sel(-1, 0, shift, nr, nc),
            "ArrowLeft" => self.move_sel(0, -1, shift, nr, nc),
            "ArrowRight" => self.move_sel(0, 1, shift, nr, nc),
            "PageDown" => self.move_sel(20, 0, shift, nr, nc),
            "PageUp" => self.move_sel(-20, 0, shift, nr, nc),
            "Home" => self.move_sel(0, -(nc as i64), shift, nr, nc),
            "End" => self.move_sel(0, nc as i64, shift, nr, nc),
            "Ctrl+Home" => {
                self.ed.sel = (0, 0);
                self.ed.anchor = (0, 0);
                self.ed.scroll = true;
            }
            "Ctrl+End" => {
                self.ed.sel = (nr.saturating_sub(1), nc - 1);
                self.ed.anchor = self.ed.sel;
                self.ed.scroll = true;
            }
            "Enter" => self.move_sel(if shift { -1 } else { 1 }, 0, false, nr, nc),
            "Tab" => self.move_sel(0, if shift { -1 } else { 1 }, false, nr, nc),
            "F2" => {
                let text = self.cell_text(&k, t, self.ed.sel);
                self.ed.editing = Some(text);
            }
            "Delete" | "Backspace" => self.clear_range(&k, t),
            "Escape" => self.ed.anchor = self.ed.sel,
            "Ctrl+a" => {
                self.ed.anchor = (0, 0);
                self.ed.sel = (nr.saturating_sub(1), nc - 1);
            }
            "Ctrl+c" | "Ctrl+x" => {
                let cells = self.selected_text(&k, t);
                host::copy(&sheet::to_tsv(&cells));
                if base == "Ctrl+x" {
                    self.clear_range(&k, t);
                }
            }
            "Ctrl+z" => self.undo(&k, t, false),
            "Ctrl+y" | "Ctrl+Shift+z" => self.undo(&k, t, true),
            other => {
                // typing starts editing the cell, like Excel
                if other.chars().count() == 1 && !key.contains("Ctrl+") && !key.contains("Alt+") {
                    self.ed.editing = Some(other.to_string());
                    return (true, true);
                }
                if key == "Shift+ " || key == " " {
                    self.ed.editing = Some(" ".into());
                    return (true, true);
                }
                return (false, false);
            }
        }
        (true, true)
    }

    fn cell_text(&mut self, key: &str, t: usize, (r, c): (usize, usize)) -> String {
        let (rows, order) = self.sheet_view(key, t);
        order.get(r).and_then(|&i| rows[i].vals.get(c)).map(|v| v.cell_text()).unwrap_or_default()
    }

    /// Pointer events from the sheet (cells, row and column headers).
    pub fn sheet_pointer(&mut self, kind: &str, what: &str, arg: &str, val: &str) {
        let Some((k, t)) = self.cur_table() else { return };
        let (_, order) = self.sheet_view(&k, t);
        let (nr, nc) = (order.len(), self.ncols(&k, t));
        let shift = val == "shift";
        let mut it = arg.split(':');
        let a: usize = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let b: usize = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        self.ed.menu = None;
        match (kind, what) {
            ("down", "scell") => {
                let now = host::now_ms();
                let dbl = self.ed.last_down.1 == a && self.ed.last_down.2 == b && now - self.ed.last_down.0 < 450.0;
                self.ed.last_down = (now, a, b);
                self.ed.sel = (a.min(nr), b.min(nc.saturating_sub(1)));
                if !shift {
                    self.ed.anchor = self.ed.sel;
                }
                if dbl {
                    let text = self.cell_text(&k, t, self.ed.sel);
                    self.ed.editing = Some(text);
                }
            }
            ("drag", "scell") => self.ed.sel = (a.min(nr), b.min(nc.saturating_sub(1))),
            ("down", "colh") => {
                self.ed.sel = (nr.saturating_sub(1), a);
                if !shift {
                    self.ed.anchor = (0, a);
                }
            }
            ("drag", "colh") => self.ed.sel.1 = a.min(nc.saturating_sub(1)),
            ("down", "rowh") => {
                self.ed.sel = (a.min(nr), nc.saturating_sub(1));
                if !shift {
                    self.ed.anchor = (a.min(nr), 0);
                }
            }
            ("drag", "rowh") => self.ed.sel.0 = a.min(nr),
            _ => {}
        }
    }

    /// Clicks and form changes inside the editor. Returns false if not ours.
    pub fn editor_event(&mut self, kind: &str, action: &str, arg: &str, val: &str) -> Option<bool> {
        let cur = self.cur_table();
        match (kind, action) {
            ("edit", "commit") => {
                self.commit_edit(val);
                return Some(false); // rendered by the pointer event that follows
            }
            (_, "ed-tab") => {
                self.ed.tab = arg.to_string();
                self.ed.menu = None;
                self.ed.last_sql = None;
                self.ed.col_edit = None;
                self.ed.confirm = None;
            }
            (_, "sheet-filter") => {
                self.ed.filter = val.to_string();
                self.ed.sel = (0, self.ed.sel.1);
                self.ed.anchor = self.ed.sel;
            }
            (_, "sheet-fbar") => {
                let (k, t) = cur?;
                let (r, c) = self.ed.sel;
                if let Err(e) = self.set_display_cell(&k, t, r, c, ui::typed(val)) {
                    self.err(e);
                }
            }
            (_, "sheet-menu") => {
                let c: usize = arg.parse().ok()?;
                self.ed.menu = if self.ed.menu == Some(c) { None } else { Some(c) };
            }
            (_, "sheet-sort") => {
                let mut it = arg.split(':');
                let c: usize = it.next()?.parse().ok()?;
                let desc = it.next() == Some("desc");
                self.ed.sort = if self.ed.sort == Some((c, desc)) { None } else { Some((c, desc)) };
                self.ed.menu = None;
            }
            (_, "col-insert") => {
                let (k, t) = cur?;
                let mut it = arg.split(':');
                let c: usize = it.next()?.parse().ok()?;
                let right = it.next() == Some("right");
                let r = self.edit_tb(&k, t, |tb, _| {
                    let n = sheet::next_column_name(tb);
                    sheet::add_column(tb, &n, Some(if right { c + 1 } else { c }))
                })?;
                match r {
                    Ok(at) => {
                        self.ed.renaming = Some(at);
                        self.ed.sel.1 = at;
                        self.ed.anchor = self.ed.sel;
                    }
                    Err(e) => self.err(e),
                }
                self.ed.menu = None;
            }
            (_, "col-add") => {
                let (k, t) = cur?;
                let name = self.form.remove(&format!("newcol:{}:{}", k, t)).unwrap_or_default();
                let r = self.edit_tb(&k, t, |tb, _| {
                    let n = if name.trim().is_empty() { sheet::next_column_name(tb) } else { name.clone() };
                    sheet::add_column(tb, &n, None)
                })?;
                if let Err(e) = r {
                    self.err(e);
                }
            }
            (_, "col-rename-start") => {
                self.ed.renaming = arg.parse().ok();
                self.ed.menu = None;
            }
            (_, "col-rename") => {
                let (k, t) = cur?;
                let c: usize = arg.parse().ok()?;
                self.ed.renaming = None;
                if let Some(Err(e)) = self.edit_tb(&k, t, |tb, base| sheet::rename_column(tb, base, c, val)) {
                    self.err(e);
                }
            }
            (_, "col-delete") => {
                let (k, t) = cur?;
                let c: usize = arg.parse().ok()?;
                let on_chain = self.drafts[self.draft_idx(&k)?].tables[t].created.is_some();
                match self.edit_tb(&k, t, |tb, _| sheet::delete_column(tb, c))? {
                    Ok(()) => {
                        if on_chain {
                            self.ok("Column removed from the table. Values already saved in it stay on the blockchain, but new saves won't include it.");
                        }
                        self.ed.sel.1 = self.ed.sel.1.min(self.ncols(&k, t).saturating_sub(1));
                        self.ed.anchor = self.ed.sel;
                    }
                    Err(e) => self.err(e),
                }
                self.ed.menu = None;
            }
            (_, "col-id") => {
                let (k, t) = cur?;
                let c: usize = arg.parse().ok()?;
                if let Some(Err(e)) = self.edit_tb(&k, t, |tb, base| sheet::set_id_column(tb, base, c)) {
                    self.err(e);
                }
                self.ed.menu = None;
            }
            (_, "col-move") => {
                let (k, t) = cur?;
                let mut it = arg.split(':');
                let c: usize = it.next()?.parse().ok()?;
                let to: usize = it.next()?.parse().ok()?;
                self.edit_tb(&k, t, |tb, _| sheet::move_column(tb, c, to));
            }
            (_, "row-add") => {
                let (k, t) = cur?;
                self.edit_tb(&k, t, |tb, _| sheet::insert_row(tb));
                let (_, order) = self.sheet_view(&k, t);
                self.ed.sort = None;
                self.ed.filter.clear();
                self.ed.sel = (order.len().saturating_sub(1), 0);
                self.ed.anchor = self.ed.sel;
                self.ed.scroll = true;
            }
            (_, "row-delete") => {
                let (k, t) = cur?;
                let ((r0, _), (r1, _)) = self.range();
                let (rows, order) = self.sheet_view(&k, t);
                let picked: Vec<SRow> = (r0..=r1).filter_map(|r| order.get(r).map(|&i| rows[i].clone())).collect();
                // rows already marked come back; the others go (links are checked)
                let restore: Vec<&SRow> = picked.iter().filter(|r| r.state == RowState::Deleted).collect();
                if !restore.is_empty() {
                    self.edit_tb(&k, t, |tb, _| sheet::delete_rows(tb, &restore));
                }
                let del: Vec<crate::constraints::Change> =
                    picked.iter().filter(|r| r.state != RowState::Deleted).map(|r| crate::constraints::Change::Delete { row: r.clone() }).collect();
                if !del.is_empty() {
                    let fk = self.fk_checks();
                    match self.apply_changes(&k, t, del, &crate::constraints::Opts { strict: false, fk_checks: fk }) {
                        Ok(a) => {
                            if !a.cascaded.is_empty() {
                                self.ok(format!("Deleted. {}", a.cascaded.join("; ")));
                            }
                        }
                        Err(e) => self.err(e),
                    }
                }
            }
            (_, "sheet-undo") => {
                let (k, t) = cur?;
                self.undo(&k, t, arg == "redo");
            }
            (_, "discard") => {
                let (k, t) = cur?;
                self.edit_tb(&k, t, |tb, _| sheet::discard(tb));
                self.ok("Unsaved changes discarded (Undo brings them back)");
            }
            (_, "base-reload") => {
                let (k, t) = cur?;
                self.reload_base(&k, t);
            }
            (_, "sql-run") => {
                let k = self.cur_db()?;
                if let Some(v) = self.form.remove(&format!("sql:{}", k)) {
                    self.ed.sql_text.insert(k.clone(), v);
                }
                self.sql_run(&k);
            }
            (_, "sql-example") => {
                let k = self.cur_db()?;
                self.ed.sql_text.insert(k, arg.to_string());
            }
            (_, "sql-hist") => {
                let k = self.cur_db()?;
                let i: usize = arg.parse().ok()?;
                if let Some(q) = self.ed.sql_hist.get(i).cloned() {
                    self.ed.sql_text.insert(k, q);
                }
            }
            (_, "sheet-goto") => {
                let r: usize = arg.parse().ok()?;
                self.ed.sel = (r, self.ed.sel.1);
                self.ed.anchor = self.ed.sel;
            }
            (_, "sheet-export") => {
                let (k, t) = cur?;
                self.export_sheet(&k, t, arg);
            }
            ("file", "attach-sel") => {
                let (k, t) = cur?;
                self.attach_to_selection(&k, t, val);
            }
            ("paste", "sheet") => self.paste(val),
            ("down", w) | ("drag", w) if matches!(w, "scell" | "colh" | "rowh") => self.sheet_pointer(kind, w, arg, val),
            _ => return self.ws_event(kind, action, arg, val),
        }
        Some(true)
    }

    fn export_sheet(&mut self, key: &str, t: usize, fmt: &str) {
        let rows = self.sheet_rows(key, t);
        let Some(tb) = self.draft_idx(key).and_then(|i| self.drafts[i].tables.get(t)).cloned() else { return };
        let live: Vec<&SRow> = rows.iter().filter(|r| r.state != RowState::Deleted).collect();
        let name: String = tb.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        if fmt == "json" {
            let arr: Vec<Json> = live.iter().map(|r| Json::Obj(tb.columns.iter().cloned().zip(r.vals.iter().cloned()).collect())).collect();
            host::download(&format!("{}.json", name), "application/json", Json::Arr(arr).to_string().as_bytes());
        } else {
            let mut out = tb.columns.iter().map(|c| ui::csv_cell(c)).collect::<Vec<_>>().join(",");
            out.push('\n');
            for r in &live {
                out.push_str(&r.vals.iter().map(|v| ui::csv_cell(&v.cell_text())).collect::<Vec<_>>().join(","));
                out.push('\n');
            }
            host::download(&format!("{}.csv", name), "text/csv", out.as_bytes());
        }
    }

    /// Put a small file into the selected cell (inscribed on its own).
    fn attach_to_selection(&mut self, key: &str, t: usize, val: &str) {
        let (r, c) = self.ed.sel;
        let (rows, order) = self.sheet_view(key, t);
        let Some(tb) = self.draft_idx(key).and_then(|i| self.drafts[i].tables.get(t)).cloned() else { return };
        if c == tb.id_col {
            return self.err("Pick a cell outside the ID column for the file.");
        }
        let row = order.get(r).map(|&i| rows[i].clone());
        // the file lands in a pending row: find it, or make one
        let ghost = match row {
            Some(ref x) if x.state == RowState::Deleted => return self.err("That row is marked for deletion."),
            Some(ref x) if x.ghost.is_some() => x.ghost,
            Some(x) => self.edit_tb(key, t, |tb, _| {
                tb.rows.push(GhostRow { vals: x.vals.clone(), deleted: false, sig: None });
                tb.rows.len() - 1
            }),
            None => self.edit_tb(key, t, |tb, _| {
                let n = sheet::next_column_name(tb);
                let _ = n;
                tb.rows.push(GhostRow { vals: vec![Json::Null; tb.columns.len()], deleted: false, sig: None });
                tb.rows.len() - 1
            }),
        };
        let Some(g) = ghost else { return };
        self.form.insert(format!("attachcol:{}:{}", key, t), tb.columns[c].clone());
        self.attach_file(&format!("{}:{}:{}", key, t, g), val);
    }

    pub fn sql_run(&mut self, key: &str) {
        let text = self.ed.sql_text.get(key).cloned().unwrap_or_default();
        if text.trim().is_empty() {
            return;
        }
        if self.ed.sql_hist.last() != Some(&text) {
            self.ed.sql_hist.push(text.clone());
            if self.ed.sql_hist.len() > 30 {
                self.ed.sql_hist.remove(0);
            }
        }
        self.ed.sql_out = self.run_sql(key, &text);
    }

    // -------------------------------------------------------------- save

    /// Estimated cost of saving a draft's pending changes (lamports).
    pub fn save_estimate(&mut self, key: &str) -> (u64, usize) {
        let Some(i) = self.draft_idx(key) else { return (0, 0) };
        let cap = self.inline_cap();
        let d = &self.drafts[i];
        let mut total = 0u64;
        if d.root_sig.is_none() {
            total += iq::DB_ROOT_COST_ESTIMATE;
        }
        total += d.tables.iter().filter(|t| t.created.is_none() && !t.dropped).count() as u64 * iq::TABLE_COST_ESTIMATE;
        if d.user_init_sig.is_none() {
            total += iq::USER_INIT_RENT_ESTIMATE;
        }
        let wallet = d.wallet.clone();
        let structure = d.tables.iter().filter(|t| t.schema_changed()).count();
        let meta = d.tables.iter().filter(|t| t.meta_changed(wallet.as_deref())).count() + d.tables.iter().filter(|t| t.dropped).count().min(1);
        let n = d.tables.len();
        let mut packs = structure;
        total += structure as u64 * pack::write_cost(0);
        for t in 0..n {
            if let Ok(p) = self.plan_for(key, t, cap) {
                packs += p.len();
                total += p.iter().map(|x| x.cost()).sum::<u64>();
            }
        }
        total += meta as u64 * iq::TX_FEE;
        (total, packs)
    }

    /// "Save to blockchain": make sure there's a database wallet with enough
    /// SOL (moving it from the main wallet if needed), then inscribe.
    pub fn save(&mut self, key: &str) {
        if self.run.as_ref().map(|r| r.busy()).unwrap_or(false) {
            self.err("Already saving — wait for it to finish.");
            return;
        }
        let Some(i) = self.draft_idx(key) else { return };
        let Some(a) = self.account.as_ref() else {
            self.err("Sign in with your wallet's key to save to the blockchain.");
            self.keep_toast = true;
            host::set_hash("#/account");
            return;
        };
        let main = a.main().map(|w| w.address());
        if self.drafts[i].wallet.is_none() {
            self.drafts[i].wallet = main.clone();
            self.save_drafts();
        }
        let Some(wallet) = self.drafts[i].wallet.clone() else {
            self.err("Sign in with your wallet's key to save to the blockchain.");
            return;
        };
        if self.keypair(&wallet).is_none() {
            self.err("This database belongs to a wallet you're not signed in with. Drop in that wallet's key, or pick another wallet under Save → Advanced.");
            return;
        }
        // rows typed in bit by bit must be complete before they're saved
        let mut problems = vec![];
        for t in 0..self.drafts[i].tables.len() {
            if !self.drafts[i].tables[t].dropped {
                problems.extend(self.row_problems(key, t));
            }
        }
        if !problems.is_empty() {
            self.ed.tab = "save".into();
            self.err(format!(
                "Fill these in first: {}{}",
                problems.iter().take(3).cloned().collect::<Vec<_>>().join("; "),
                if problems.len() > 3 { format!(" (+{} more)", problems.len() - 3) } else { String::new() }
            ));
            return;
        }
        self.ed.tab = "save".into();
        // the last save's report would read as this one's
        self.run = None;
        let (est, _) = self.save_estimate(key);
        let mut addrs = vec![Json::Str(wallet.clone())];
        if let Some(m) = &main {
            if m != &wallet {
                addrs.push(Json::Str(m.clone()));
            }
        }
        let params = Json::Arr(vec![
            Json::Arr(addrs),
            crate::json::obj(vec![
                ("encoding", crate::json::s("base64")),
                ("dataSlice", crate::json::obj(vec![("offset", crate::json::n(0)), ("length", crate::json::n(0))])),
                ("commitment", crate::json::s("confirmed")),
            ]),
        ]);
        self.busy_note = Some("Checking your balance…".into());
        self.rpc("getMultipleAccounts", params, P::SaveCheck { key: key.to_string(), need: est, main });
    }

    pub fn save_async(&mut self, p: P, ok: bool, status: u32, data: Vec<u8>) -> bool {
        let text = String::from_utf8_lossy(&data).into_owned();
        let http_ok = ok && (200..300).contains(&status);
        let P::SaveCheck { key, need, main } = p else { return false };
        self.busy_note = None;
        let v = match if http_ok { net::rpc_result(&text) } else { Err(fetch_err(ok, status, &text)) } {
            Ok(v) => v,
            Err(e) => {
                self.err(format!("Couldn't check your balance: {}", e));
                return true;
            }
        };
        let bal = |i: usize| v.get("value").idx(i).get("lamports").u64().unwrap_or(0);
        let Some(i) = self.draft_idx(&key) else { return true };
        let wallet = self.drafts[i].wallet.clone().unwrap_or_default();
        let have = bal(0);
        self.balances.insert(wallet.clone(), Load::Ready(have));
        // keep a little headroom: the simulation reveals the exact amount
        let want = need + need / 10 + iq::RENT_FLOOR;
        if have >= want || need == 0 {
            self.start_run(&key);
            return true;
        }
        let short = want - have;
        match main.filter(|m| m != &wallet) {
            Some(m) => {
                let main_bal = bal(1);
                self.balances.insert(m.clone(), Load::Ready(main_bal));
                if main_bal >= short + iq::TX_FEE + iq::RENT_FLOOR {
                    self.ok(format!("Moving {} from your balance to this database's wallet…", ui::sol(short)));
                    self.transfer(&m, &wallet, short, After::StartRun(key));
                } else {
                    self.err(format!("Saving needs about {} and your balance is {}. Add funds, then save again.", ui::sol(want), ui::sol(main_bal + have)));
                    self.ed.tab = "save".into();
                }
            }
            None => {
                self.err(format!("Saving needs about {} more SOL in this database's wallet. Add funds, then save again.", ui::sol(short)));
                self.ed.tab = "save".into();
            }
        }
        true
    }

    /// Everything a run just saved should show as saved: re-read bases.
    pub fn after_run(&mut self, key: &str) {
        let n = self.draft_idx(key).map(|i| self.drafts[i].tables.len()).unwrap_or(0);
        for t in 0..n {
            self.reload_base(key, t);
        }
    }

    pub fn row_state_class(s: RowState) -> &'static str {
        match s {
            RowState::Saved => "",
            RowState::New => "new",
            RowState::Changed => "chg",
            RowState::Deleted => "del",
        }
    }
}
