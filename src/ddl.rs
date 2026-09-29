//! Structure statements: CREATE / ALTER / DROP / RENAME / TRUNCATE TABLE,
//! views, indexes, GRANT, SHOW …, and SQL dumps (export and import).
//!
//! On a table that's already saved, a structure change is one small
//! record on chain: rows keep their storage keys and are read through the
//! new structure (see schema.rs). Only the database's owner can make them.

use std::collections::HashSet;

use crate::app::App;
use crate::constraints::{self, Change, Opts};
use crate::json::{self, Json};
use crate::schema::{self, Check, ColMeta, DefVal, Fk, Index, IntKind, RefAction, TextKind, Ty};
use crate::sheet::{self, BaseRec, RowState, SRow};
use crate::sql::{self, ast::*, eval};
use crate::sql_exec::{sql_ident, sql_str, sql_value, Out, RunCtx};
use crate::state::{DraftTable, SYSTEM_TABLE};

type R<T> = Result<T, String>;

fn msg(s: impl Into<String>) -> R<Vec<Out>> {
    Ok(vec![Out::Msg(true, s.into())])
}

fn rows_out(title: &str, cols: &[&str], rows: Vec<Vec<Json>>, note: impl Into<String>) -> R<Vec<Out>> {
    Ok(vec![Out::Rows { title: title.into(), cols: cols.iter().map(|s| s.to_string()).collect(), rows, note: note.into() }])
}

fn valid_name(what: &str, n: &str) -> R<()> {
    if n.trim().is_empty() {
        return Err(format!("A {} needs a name", what));
    }
    if n.len() > 64 {
        return Err(format!("{} names are at most 64 bytes", what));
    }
    if n.to_ascii_lowercase().starts_with(SYSTEM_TABLE) {
        return Err(format!("Names starting with {} are kept for IQ Tables itself", SYSTEM_TABLE));
    }
    Ok(())
}

fn unique_name(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t.eq_ignore_ascii_case(base)) {
        return base.to_string();
    }
    let mut i = 2;
    loop {
        let n = format!("{}_{}", base, i);
        if !taken.iter().any(|t| t.eq_ignore_ascii_case(&n)) {
            return n;
        }
        i += 1;
    }
}

/// Replace a column name inside a CHECK expression.
fn rename_in_expr(expr: &str, from: &str, to: &str) -> String {
    let Ok(toks) = sql::lex::tokenize(expr) else { return expr.to_string() };
    let mut out = String::new();
    let mut last = 0;
    for (i, t) in toks.iter().enumerate() {
        let is_name = match &t.t {
            sql::lex::Tok::Word(w) | sql::lex::Tok::Ident(w) => w.eq_ignore_ascii_case(from),
            _ => false,
        };
        let is_call = matches!(toks.get(i + 1).map(|x| &x.t), Some(sql::lex::Tok::Sym("(")));
        if is_name && !is_call {
            out.push_str(&expr[last..t.pos]);
            out.push_str(&sql_ident(to));
            last = t.end;
        }
    }
    out.push_str(&expr[last..]);
    out
}

fn mentions(expr: &str, col: &str) -> bool {
    rename_in_expr(expr, col, "\u{1}") != expr
}

/// A type that fits every value (CREATE TABLE … AS SELECT).
fn infer_type(vals: &[&Json]) -> Ty {
    let present: Vec<&&Json> = vals.iter().filter(|v| !v.is_null()).collect();
    if present.is_empty() {
        return Ty::Any;
    }
    if present.iter().all(|v| matches!(v, Json::Bool(_))) {
        return Ty::Bool;
    }
    let texts: Vec<String> = present.iter().map(|v| eval::text(v)).collect();
    if texts.iter().all(|t| !t.is_empty() && t.bytes().enumerate().all(|(i, c)| c.is_ascii_digit() || (i == 0 && c == b'-')) && t.len() < 19) {
        return Ty::Int(IntKind::Big, false);
    }
    if present.iter().all(|v| matches!(v, Json::Num(_))) {
        let scale = texts.iter().map(|t| t.split_once('.').map(|x| x.1.len()).unwrap_or(0)).max().unwrap_or(0);
        let int = texts.iter().map(|t| t.trim_start_matches('-').split('.').next().unwrap_or("").len()).max().unwrap_or(1);
        if !texts.iter().any(|t| t.contains(['e', 'E'])) && scale <= 10 && int + scale <= 30 {
            return Ty::Decimal((int + scale).max(1) as u32 + 2, scale as u32, false);
        }
        return Ty::Float(true, false);
    }
    if texts.iter().all(|t| t.len() == 10 && crate::dates::parse_date(t).is_some()) {
        return Ty::Date;
    }
    if texts.iter().all(|t| t.len() == 19 && crate::dates::parse_datetime(t).is_some()) {
        return Ty::DateTime(0);
    }
    if present.iter().any(|v| matches!(v, Json::Arr(_) | Json::Obj(_))) {
        return Ty::Json;
    }
    let max = texts.iter().map(|t| t.chars().count()).max().unwrap_or(1);
    if max <= 255 {
        Ty::Varchar(255)
    } else {
        Ty::Text(TextKind::Text)
    }
}

/// Move PRIMARY KEY definitions that a dump adds after its INSERTs
/// (phpMyAdmin writes `ALTER TABLE t ADD PRIMARY KEY (id)` at the end)
/// into the CREATE TABLE, so rows are keyed right from the start.
pub fn hoist_keys(mut stmts: Vec<(Stmt, String)>) -> Vec<(Stmt, String)> {
    let n = stmts.len();
    for i in 0..n {
        let (name, has_pk) = match &stmts[i].0 {
            Stmt::CreateTable { name, cols, cons, .. } => {
                (name.clone(), cols.iter().any(|c| c.primary) || cons.iter().any(|c| matches!(c, Constraint::Primary(_))))
            }
            _ => continue,
        };
        if has_pk {
            continue;
        }
        for j in i + 1..n {
            let mut found: Option<Vec<String>> = None;
            if let Stmt::Alter { table, ops } = &mut stmts[j].0 {
                if table.eq_ignore_ascii_case(&name) {
                    if let Some(p) = ops.iter().position(|o| matches!(o, AlterOp::AddConstraint(Constraint::Primary(_)))) {
                        if let AlterOp::AddConstraint(Constraint::Primary(c)) = ops.remove(p) {
                            found = Some(c);
                        }
                        if ops.is_empty() {
                            ops.push(AlterOp::Ignored("primary key (moved into CREATE TABLE)".into()));
                        }
                    }
                }
            }
            if let Some(c) = found {
                if let Stmt::CreateTable { cons, .. } = &mut stmts[i].0 {
                    cons.push(Constraint::Primary(c));
                }
                break;
            }
        }
    }
    stmts
}

struct Built {
    columns: Vec<String>,
    meta: Vec<ColMeta>,
    pk: Option<usize>,
    notes: Vec<String>,
}

impl App {
    /// Is the signed-in account the database's owner (or is it not on chain yet)?
    pub fn owns(&self, key: &str) -> bool {
        let Some(i) = self.draft_idx(key) else { return false };
        match self.creator_of(key) {
            Some(c) => self.drafts[i].wallet.as_deref() == Some(c.as_str()),
            None => true,
        }
    }

    fn require_owner(&self, key: &str, t: usize) -> R<()> {
        let i = self.draft_idx(key).ok_or("No such database")?;
        if self.drafts[i].tables[t].created.is_some() && !self.owns(key) {
            return Err("Only the database's owner can change the structure of its saved tables.".into());
        }
        Ok(())
    }

    fn table_names(&self, key: &str) -> Vec<String> {
        let d = &self.drafts[self.draft_idx(key).unwrap()];
        d.tables.iter().filter(|t| !t.dropped).flat_map(|t| [t.title.clone(), t.name.clone()]).collect()
    }

    fn spec_meta(&self, s: &ColumnSpec, key: &str, primary: bool) -> R<ColMeta> {
        let mut ty = s.ty.clone();
        if s.auto_inc {
            match ty {
                Ty::Any => ty = Ty::Int(IntKind::Int, false),
                Ty::Int(..) => {}
                Ty::Decimal(_, 0, _) => {}
                _ => return Err(format!("`{}`: AUTO_INCREMENT needs a whole-number column (INT)", s.name)),
            }
        }
        let not_null = s.not_null == Some(true) || primary || s.primary || s.auto_inc;
        if s.not_null == Some(false) && (primary || s.primary) {
            return Err(format!("`{}` is the primary key, so it can't allow NULL", s.name));
        }
        let default = match &s.default {
            None | Some(None) => {
                if matches!(s.default, Some(None)) && not_null {
                    return Err(format!("`{}` is NOT NULL, so its default can't be NULL", s.name));
                }
                None
            }
            Some(Some(DefaultSpec::Lit(v))) => Some(DefVal::Lit(ty.coerce(v).map_err(|e| format!("Default for `{}`: {}", s.name, e))?)),
            Some(Some(DefaultSpec::Expr(e))) => {
                let d = Some(DefVal::Expr(e.clone()));
                constraints::default_value(&ty, &d).map_err(|x| format!("Default for `{}`: {}", s.name, x))?;
                d
            }
        };
        if s.on_update_now && !matches!(ty, Ty::DateTime(_) | Ty::Timestamp(_)) {
            return Err(format!("`{}`: ON UPDATE CURRENT_TIMESTAMP needs a DATETIME or TIMESTAMP column", s.name));
        }
        Ok(ColMeta {
            key: key.to_string(),
            ty,
            not_null,
            default,
            on_update_now: s.on_update_now,
            auto_inc: s.auto_inc,
            comment: s.comment.clone().unwrap_or_default(),
            fill: Json::Null,
            auto_added: false,
        })
    }

    /// Resolve a REFERENCES clause against the database.
    fn fk_from(&self, key: &str, tb: &DraftTable, name: Option<String>, cols: &[String], r: &RefSpec) -> R<Fk> {
        let local: Vec<String> = cols
            .iter()
            .map(|c| tb.col(c).map(|i| tb.meta[i].key.clone()).ok_or_else(|| format!("FOREIGN KEY: no column `{}` in {}", c, tb.title)))
            .collect::<R<_>>()?;
        let same = r.table.eq_ignore_ascii_case(&tb.title) || r.table.eq_ignore_ascii_case(&tb.name);
        let (rt_name, ref_cols) = if same {
            let rc: Vec<String> = if r.cols.is_empty() {
                vec![tb.pk_key()]
            } else {
                r.cols
                    .iter()
                    .map(|c| tb.col(c).map(|i| tb.meta[i].key.clone()).ok_or_else(|| format!("REFERENCES: no column `{}` in {}", c, tb.title)))
                    .collect::<R<_>>()?
            };
            (tb.name.clone(), rc)
        } else {
            let t = self.tbl(key, &r.table).map_err(|_| format!("REFERENCES {}: no such table", r.table))?;
            let rtb = &self.drafts[self.draft_idx(key).unwrap()].tables[t];
            let rc: Vec<String> = if r.cols.is_empty() {
                vec![rtb.pk_key()]
            } else {
                r.cols
                    .iter()
                    .map(|c| rtb.col(c).map(|i| rtb.meta[i].key.clone()).ok_or_else(|| format!("REFERENCES: no column `{}` in {}", c, rtb.title)))
                    .collect::<R<_>>()?
            };
            (rtb.name.clone(), rc)
        };
        if ref_cols.len() != local.len() {
            return Err("FOREIGN KEY and REFERENCES list different numbers of columns".into());
        }
        let taken: Vec<String> = tb.keys.fks.iter().map(|f| f.name.clone()).collect();
        let name = name.unwrap_or_else(|| unique_name(&format!("{}_ibfk_{}", tb.title, tb.keys.fks.len() + 1), &taken));
        Ok(Fk { name, cols: local, table: rt_name, ref_cols, on_delete: r.on_delete, on_update: r.on_update })
    }

    // ---------------------------------------------------------- dispatch

    pub fn exec_ddl(&mut self, key: &str, s: Stmt, text: &str, ctx: &mut RunCtx) -> R<Vec<Out>> {
        let _ = (text, &ctx);
        match s {
            Stmt::CreateTable { name, if_not_exists, cols, cons, opts, like, query } => {
                self.create_table(key, name, if_not_exists, cols, cons, opts, like, query)
            }
            Stmt::Alter { table, ops } => self.alter(key, &table, ops),
            Stmt::DropTable { names, if_exists } => {
                let mut out = vec![];
                for n in names {
                    match self.tbl(key, &n) {
                        Ok(t) => out.push(self.drop_table(key, t)?),
                        Err(e) => {
                            if !if_exists {
                                return Err(e);
                            }
                            out.push(format!("No table {} (nothing to drop)", n));
                        }
                    }
                }
                msg(out.join(" "))
            }
            Stmt::RenameTable(pairs) => {
                let mut out = vec![];
                for (a, b) in pairs {
                    let t = self.tbl(key, &a)?;
                    out.push(self.rename_table(key, t, &b)?);
                }
                msg(out.join(" "))
            }
            Stmt::Truncate(n) => {
                let t = self.tbl(key, &n)?;
                msg(self.truncate(key, t)?)
            }
            Stmt::Optimize(names) => {
                let mut out = vec![];
                for n in names {
                    let t = self.tbl(key, &n)?;
                    out.push(self.checkpoint(key, t, true)?);
                }
                msg(out.join(" "))
            }
            Stmt::CreateView { name, or_replace, cols, sql, q } => self.create_view(key, &name, or_replace, cols, &sql, &q),
            Stmt::DropView { names, if_exists } => {
                let mut out = vec![];
                for n in names {
                    out.push(self.drop_view(key, &n, if_exists)?);
                }
                msg(out.join(" "))
            }
            Stmt::CreateIndex { name, table, cols, unique } => self.alter(
                key,
                &table,
                vec![AlterOp::AddConstraint(if unique { Constraint::Unique(Some(name), cols) } else { Constraint::Index(Some(name), cols) })],
            ),
            Stmt::DropIndex { name, table } => self.alter(key, &table, vec![AlterOp::DropIndex(name)]),
            Stmt::CreateDatabase { name, if_not_exists } => {
                if self.drafts.iter().any(|d| d.name == name) {
                    if if_not_exists {
                        return msg(format!("Database {} already exists", name));
                    }
                    return Err(format!("Database {} already exists", name));
                }
                self.create_draft(&name, false)?;
                msg(format!("Database {} created in the editor — USE {} to work in it. It goes on the blockchain with its first save.", name, name))
            }
            Stmt::Use(name) => match self.drafts.iter().find(|d| d.name.eq_ignore_ascii_case(&name)).map(|d| d.key.clone()) {
                Some(k) => {
                    crate::host::set_hash(&format!("#/ws/{}", k));
                    self.ed.scope_db = true;
                    msg(format!("Now working in {}", name))
                }
                None => Err(format!(
                    "No database {} in the editor. CREATE DATABASE {} makes one; to open one from the blockchain, find it in Explore and choose Edit.",
                    name, name
                )),
            },
            Stmt::Show(s) => self.show(key, s),
            Stmt::Describe(t) => self.show(key, Show::Columns { table: t, full: false }),
            Stmt::Grant { table, to, revoke } => self.grant(key, &table, to, revoke),
            other => Err(format!("{:?} isn't supported here", other).chars().take(120).collect()),
        }
    }

    // ------------------------------------------------------ CREATE TABLE

    fn build_columns(&self, cols: &[ColumnSpec], cons: &[Constraint], name: &str) -> R<Built> {
        let mut columns = vec![];
        let mut meta = vec![];
        let mut pk: Option<usize> = None;
        let mut notes = vec![];
        let mut pk_decls = 0;
        for c in cols {
            if columns.iter().any(|x: &String| x.eq_ignore_ascii_case(&c.name)) {
                return Err(format!("Column `{}` appears twice", c.name));
            }
            if c.name.len() > 64 {
                return Err(format!("Column name `{}` is longer than 64 bytes", c.name));
            }
            let m = self.spec_meta(c, &c.name, false)?;
            if c.primary {
                pk = Some(columns.len());
                pk_decls += 1;
            }
            columns.push(c.name.clone());
            meta.push(m);
        }
        for c in cons {
            if let Constraint::Primary(pc) = c {
                pk_decls += 1;
                if pc.len() == 1 {
                    let p = columns.iter().position(|x| x.eq_ignore_ascii_case(&pc[0])).ok_or_else(|| format!("PRIMARY KEY ({}): no such column", pc[0]))?;
                    pk = Some(p);
                } else {
                    notes.push(format!(
                        "{}: a primary key made of several columns ({}) isn't possible here, so an automatic id is the key and ({}) is UNIQUE instead.",
                        name,
                        pc.join(", "),
                        pc.join(", ")
                    ));
                }
            }
        }
        if pk_decls > 1 {
            return Err("A table can have only one PRIMARY KEY".into());
        }
        if let Some(p) = pk {
            meta[p].not_null = true;
        }
        Ok(Built { columns, meta, pk, notes })
    }

    #[allow(clippy::too_many_arguments)]
    fn create_table(
        &mut self,
        key: &str,
        name: String,
        if_not_exists: bool,
        cols: Vec<ColumnSpec>,
        cons: Vec<Constraint>,
        opts: TableOpts,
        like: Option<String>,
        query: Option<Box<Query>>,
    ) -> R<Vec<Out>> {
        valid_name("Table", &name)?;
        if self.tbl(key, &name).is_ok() {
            if if_not_exists {
                return msg(format!("Table {} already exists (left as it is)", name));
            }
            return Err(format!("Table `{}` already exists", name));
        }
        if self.views(key).iter().any(|(v, _)| v.eq_ignore_ascii_case(&name)) {
            return Err(format!("There's already a view called {}", name));
        }
        let di = self.draft_idx(key).ok_or("No such database")?;
        let mut notes: Vec<String> = vec![];
        let mut tb = DraftTable { name: name.clone(), title: name.clone(), compress: true, ..Default::default() };
        let mut select_rows: Vec<Vec<Json>> = vec![];
        let mut select_cols: Vec<String> = vec![];
        if let Some(src) = like {
            let st = self.tbl(key, &src)?;
            let s = self.drafts[di].tables[st].clone();
            tb.columns = s.columns.clone();
            // same names, fresh storage keys (= names)
            let remap: Vec<(String, String)> = s.meta.iter().zip(&s.columns).map(|(m, n)| (m.key.clone(), n.clone())).collect();
            let k = |old: &str| remap.iter().find(|(a, _)| a == old).map(|x| x.1.clone()).unwrap_or_default();
            tb.meta = s.meta.iter().zip(&s.columns).map(|(m, n)| ColMeta { key: n.clone(), fill: Json::Null, ..m.clone() }).collect();
            tb.id_col = s.id_col;
            tb.keys.indexes =
                s.keys.indexes.iter().map(|i| Index { name: i.name.clone(), cols: i.cols.iter().map(|c| k(c)).collect(), unique: i.unique }).collect();
            tb.keys.checks = s.keys.checks.clone();
            tb.keys.comment = s.keys.comment.clone();
            tb.open = s.open;
            // foreign keys are not copied by CREATE TABLE … LIKE (as in MySQL)
        } else {
            let b = self.build_columns(&cols, &cons, &name)?;
            notes.extend(b.notes);
            tb.columns = b.columns;
            tb.meta = b.meta;
            let mut pk = b.pk;
            if let Some(q) = &query {
                let snap = self.snapshot(key);
                let eng = self.engine_for(key, &snap);
                let r = eng.query(q, None)?;
                select_cols = r.names();
                select_rows = r.visible_rows();
                for (i, c) in select_cols.iter().enumerate() {
                    if tb.col(c).is_none() {
                        let vals: Vec<&Json> = select_rows.iter().map(|r| &r[i]).collect();
                        tb.columns.push(c.clone());
                        tb.meta.push(ColMeta::typed(c, infer_type(&vals)));
                    }
                }
            }
            if tb.columns.is_empty() {
                return Err("A table needs at least one column".into());
            }
            if pk.is_none() {
                // every row here is keyed: add an automatic id
                let idn = if tb.col("id").is_some() { unique_name("row_id", &tb.columns) } else { "id".to_string() };
                tb.columns.insert(0, idn.clone());
                tb.meta.insert(0, ColMeta { not_null: true, auto_inc: true, auto_added: true, ..ColMeta::typed(&idn, Ty::Int(IntKind::Int, false)) });
                pk = Some(0);
                if query.is_none() && !cols.is_empty() {
                    notes.push(format!("No primary key was given, so {} got an automatic `{}` column to tell rows apart.", name, idn));
                }
            }
            tb.id_col = pk.unwrap();
            // column-level UNIQUE / REFERENCES / CHECK, and table constraints
            let mut taken: Vec<String> = vec!["PRIMARY".into()];
            for c in &cols {
                let ci = tb.col(&c.name).unwrap();
                if c.unique && ci != tb.id_col {
                    let n = unique_name(&c.name, &taken);
                    taken.push(n.clone());
                    tb.keys.indexes.push(Index { name: n, cols: vec![tb.meta[ci].key.clone()], unique: true });
                }
                if let Some(r) = &c.references {
                    let fk = self.fk_from(key, &tb, None, std::slice::from_ref(&c.name), r)?;
                    tb.keys.fks.push(fk);
                }
                if let Some((n, e)) = &c.check {
                    sql::parse_expr(e)?;
                    let taken_c: Vec<String> = tb.keys.checks.iter().map(|x| x.name.clone()).collect();
                    tb.keys.checks.push(Check {
                        name: n.clone().unwrap_or_else(|| unique_name(&format!("{}_chk_{}", name, tb.keys.checks.len() + 1), &taken_c)),
                        expr: e.clone(),
                    });
                }
            }
            for c in &cons {
                match c {
                    Constraint::Primary(pc) if pc.len() > 1 => {
                        let keys: Vec<String> = pc
                            .iter()
                            .map(|x| tb.col(x).map(|i| tb.meta[i].key.clone()).ok_or_else(|| format!("PRIMARY KEY: no column `{}`", x)))
                            .collect::<R<_>>()?;
                        taken.push("PRIMARY_cols".into());
                        tb.keys.indexes.push(Index { name: unique_name(&pc[0], &taken), cols: keys, unique: true });
                    }
                    Constraint::Primary(_) => {}
                    Constraint::Unique(n, uc) | Constraint::Index(n, uc) => {
                        let keys: Vec<String> =
                            uc.iter().map(|x| tb.col(x).map(|i| tb.meta[i].key.clone()).ok_or_else(|| format!("KEY: no column `{}`", x))).collect::<R<_>>()?;
                        let nm = unique_name(n.as_deref().unwrap_or(&uc[0]), &taken);
                        taken.push(nm.clone());
                        tb.keys.indexes.push(Index { name: nm, cols: keys, unique: matches!(c, Constraint::Unique(..)) });
                    }
                    Constraint::Foreign(n, fc, r) => {
                        let fk = self.fk_from(key, &tb, n.clone(), fc, r)?;
                        tb.keys.fks.push(fk);
                    }
                    Constraint::Check(n, e) => {
                        sql::parse_expr(e)?;
                        let taken_c: Vec<String> = tb.keys.checks.iter().map(|x| x.name.clone()).collect();
                        tb.keys.checks.push(Check {
                            name: n.clone().unwrap_or_else(|| unique_name(&format!("{}_chk_{}", name, tb.keys.checks.len() + 1), &taken_c)),
                            expr: e.clone(),
                        });
                    }
                }
            }
        }
        if let Some(c) = opts.comment {
            tb.keys.comment = c;
        }
        if let Some(a) = opts.auto_increment {
            tb.keys.ai_next = a;
        }
        tb.open = opts.open.unwrap_or(tb.open);
        // a dropped (saved) table with this name comes back to life
        let revived = self.drafts[di].tables.iter().position(|t| t.dropped && (t.name.eq_ignore_ascii_case(&name) || t.title.eq_ignore_ascii_case(&name)));
        let t = match revived {
            Some(t) => {
                let old = self.drafts[di].tables[t].clone();
                tb.name = old.name.clone();
                tb.created = old.created.clone();
                tb.chain_doc = old.chain_doc.clone();
                tb.clear = true;
                tb.keys.retired = old.keys.retired.clone();
                tb.keys.retired.extend(old.meta.iter().map(|m| m.key.clone()));
                // storage keys must not reuse the old ones
                let mut used = vec![];
                for m in tb.meta.iter_mut() {
                    let k = schema::fresh_key(&m.key, &used, &tb.keys.retired);
                    for ix in tb.keys.indexes.iter_mut() {
                        for c in ix.cols.iter_mut() {
                            if *c == m.key {
                                *c = k.clone();
                            }
                        }
                    }
                    for f in tb.keys.fks.iter_mut() {
                        for c in f.cols.iter_mut() {
                            if *c == m.key {
                                *c = k.clone();
                            }
                        }
                    }
                    m.key = k.clone();
                    used.push(k);
                }
                self.drafts[di].tables[t] = tb.clone();
                self.bump(key, t);
                t
            }
            None => {
                self.drafts[di].tables.push(tb.clone());
                self.drafts[di].tables.len() - 1
            }
        };
        self.drafts[di].sel = t;
        self.save_drafts();
        let mut m = format!("Table {} created{}.", name, if tb.open { " (anyone may add rows)" } else { "" });
        if !select_rows.is_empty() {
            let n = tb.columns.len();
            let map: Vec<usize> = select_cols.iter().map(|c| tb.col(c).unwrap()).collect();
            let changes: Vec<Change> = select_rows
                .into_iter()
                .map(|r| {
                    let mut vals = vec![Json::Null; n];
                    let mut given = vec![false; n];
                    for (k, &c) in map.iter().enumerate() {
                        vals[c] = r[k].clone();
                        given[c] = true;
                    }
                    Change::Insert { vals, given }
                })
                .collect();
            let a = self.apply_changes(key, t, changes, &Opts { strict: true, fk_checks: self.fk_checks() })?;
            m.push_str(&format!(" {} row(s) copied in.", a.inserted));
        }
        m.push_str(" It goes on the blockchain with the next COMMIT.");
        for n in notes {
            m.push(' ');
            m.push_str(&n);
        }
        msg(m)
    }

    // ------------------------------------------------------------ ALTER

    fn live(&self, tb: &DraftTable, base: &[BaseRec]) -> Vec<SRow> {
        sheet::rows(tb, base).into_iter().filter(|r| r.state != RowState::Deleted).collect()
    }

    fn check_unique(&self, tb: &DraftTable, base: &[BaseRec], cols: &[String], label: &str) -> R<()> {
        let idx: Vec<usize> = cols.iter().map(|k| tb.meta.iter().position(|m| &m.key == k).ok_or_else(|| "internal: key".to_string())).collect::<R<_>>()?;
        let mut seen = HashSet::new();
        for r in self.live(tb, base) {
            let vals: Vec<&Json> = idx.iter().map(|&c| &r.vals[c]).collect();
            if vals.iter().any(|v| v.is_null()) {
                continue;
            }
            let k: String = vals.iter().map(|v| eval::key(v)).collect::<Vec<_>>().join("\u{1}");
            if !seen.insert(k) {
                return Err(format!(
                    "Can't add {}: the value '{}' appears more than once",
                    label,
                    vals.iter().map(|v| v.cell_text()).collect::<Vec<_>>().join(", ")
                ));
            }
        }
        Ok(())
    }

    fn check_fk_data(&mut self, key: &str, tb: &DraftTable, base: &[BaseRec], fk: &Fk) -> R<()> {
        if !self.fk_checks() {
            return Ok(());
        }
        let local: Vec<usize> = fk.cols.iter().filter_map(|k| tb.meta.iter().position(|m| &m.key == k)).collect();
        let (allowed, label): (HashSet<String>, String) = if fk.table == tb.name {
            let rc: Vec<usize> = fk.ref_cols.iter().filter_map(|k| tb.meta.iter().position(|m| &m.key == k)).collect();
            (self.live(tb, base).iter().filter_map(|r| keyv(&r.vals, &rc)).collect(), tb.title.clone())
        } else {
            let di = self.draft_idx(key).unwrap();
            let rt = self.drafts[di].tables.iter().position(|t| t.name == fk.table && !t.dropped).ok_or("The referenced table is gone")?;
            let rtb = self.drafts[di].tables[rt].clone();
            let rc: Vec<usize> = fk.ref_cols.iter().filter_map(|k| rtb.meta.iter().position(|m| &m.key == k)).collect();
            (self.sheet_rows(key, rt).iter().filter(|r| r.state != RowState::Deleted).filter_map(|r| keyv(&r.vals, &rc)).collect(), rtb.title.clone())
        };
        for r in self.live(tb, base) {
            if let Some(k) = keyv(&r.vals, &local) {
                if !allowed.contains(&k) {
                    let v: Vec<String> = local.iter().map(|&c| r.vals[c].cell_text()).collect();
                    return Err(format!("Can't add the link {}: '{}' in {} has no match in {}", fk.name, v.join(", "), tb.title, label));
                }
            }
        }
        Ok(())
    }

    fn set_pk(&self, tb: &mut DraftTable, base: &[BaseRec], c: usize, notes: &mut Vec<String>) -> R<()> {
        if tb.meta[c].ty == Ty::Any {
            // any value works as a key; fine
        }
        let old = tb.id_col;
        sheet::set_id_column(tb, base, c)?;
        tb.meta[tb.id_col].not_null = true;
        // the automatic id is no longer needed
        if old != tb.id_col && tb.meta.get(old).map(|m| m.auto_added).unwrap_or(false) {
            let name = tb.columns[old].clone();
            let used_by_fk = tb.keys.fks.iter().any(|f| f.cols.contains(&tb.meta[old].key));
            if !used_by_fk {
                sheet::delete_column(tb, old)?;
                notes.push(format!("The automatic `{}` column was removed.", name));
            }
        }
        Ok(())
    }

    fn alter(&mut self, key: &str, table: &str, ops: Vec<AlterOp>) -> R<Vec<Out>> {
        let t = self.tbl(key, table)?;
        self.require_owner(key, t)?;
        let di = self.draft_idx(key).unwrap();
        let (base, _) = self.sheet_base(key, t);
        let mut tb = self.drafts[di].tables[t].clone();
        tb.fix_meta();
        let saved = tb.created.is_some() || tb.rows.iter().any(|r| r.sig.is_some()) || !base.is_empty();
        let mut notes: Vec<String> = vec![];
        let mut pk_dropped = false;
        let snap = self.snapshot(key);
        let now_eng = self.engine_for(key, &snap);
        let _ = &now_eng;
        let mut new_name: Option<String> = None;
        let mut privileges: Option<bool> = None;
        for op in ops {
            match op {
                AlterOp::AddColumn(spec, place) => {
                    if tb.col(&spec.name).is_some() {
                        return Err(format!("Column `{}` already exists in {}", spec.name, tb.title));
                    }
                    let mut m = self.spec_meta(&spec, "", false)?;
                    if m.auto_inc && saved {
                        return Err("Adding an AUTO_INCREMENT column to a table with saved rows isn't supported. Add it as INT, fill it with UPDATE, then make it the key.".into());
                    }
                    // what rows that already exist get
                    let fill = match &m.default {
                        Some(d) => constraints::default_value(&m.ty, &Some(d.clone()))?,
                        None if m.not_null => constraints::implicit_default(&m.ty),
                        None => Json::Null,
                    };
                    if saved {
                        m.fill = fill.clone();
                    }
                    let at = match &place {
                        None => None,
                        Some(Place::First) => Some(0),
                        Some(Place::After(c)) => Some(tb.col(c).ok_or_else(|| format!("AFTER {}: no such column", c))? + 1),
                    };
                    let ci = sheet::add_column_def(&mut tb, &spec.name, m, at)?;
                    // rows not saved yet get the default too
                    let mut next = 1u64;
                    for r in tb.rows.iter_mut() {
                        r.vals.resize(tb.columns.len(), Json::Null);
                        if !r.deleted {
                            if tb.meta[ci].auto_inc {
                                r.vals[ci] = json::n(next);
                                next += 1;
                            } else {
                                r.vals[ci] = fill.clone();
                            }
                        }
                    }
                    let k = tb.meta[ci].key.clone();
                    if spec.primary {
                        pk_dropped = false;
                        self.set_pk(&mut tb, &base, ci, &mut notes)?;
                    }
                    if spec.unique {
                        self.check_unique(&tb, &base, std::slice::from_ref(&k), &format!("UNIQUE on {}", spec.name))?;
                        let taken: Vec<String> = tb.keys.indexes.iter().map(|i| i.name.clone()).collect();
                        tb.keys.indexes.push(Index { name: unique_name(&spec.name, &taken), cols: vec![k.clone()], unique: true });
                    }
                    if let Some(r) = &spec.references {
                        let fk = self.fk_from(key, &tb, None, std::slice::from_ref(&spec.name), r)?;
                        self.check_fk_data(key, &tb, &base, &fk)?;
                        tb.keys.fks.push(fk);
                    }
                    if let Some((n, e)) = &spec.check {
                        self.add_check(&mut tb, &base, n.clone(), e)?;
                    }
                }
                AlterOp::AddConstraint(c) => match c.clone() {
                    Constraint::Primary(cols) => {
                        if cols.len() != 1 {
                            return Err("A primary key here is one column. For a combination, use a UNIQUE key: ALTER TABLE t ADD UNIQUE (a, b)".into());
                        }
                        let ci = tb.col(&cols[0]).ok_or_else(|| format!("PRIMARY KEY ({}): no such column", cols[0]))?;
                        let cur_auto = tb.meta.get(tb.id_col).map(|m| m.auto_added).unwrap_or(false);
                        if !pk_dropped && !cur_auto && ci != tb.id_col {
                            return Err(format!(
                                "{} already has a primary key ({}). Change it with: ALTER TABLE {} DROP PRIMARY KEY, ADD PRIMARY KEY ({})",
                                tb.title,
                                tb.columns[tb.id_col],
                                sql_ident(&tb.title),
                                sql_ident(&cols[0])
                            ));
                        }
                        self.set_pk(&mut tb, &base, ci, &mut notes)?;
                        pk_dropped = false;
                        if saved {
                            notes.push("Saved rows will be keyed by the new primary key once you save.".into());
                        }
                    }
                    Constraint::Unique(n, cols) | Constraint::Index(n, cols) => {
                        let keys: Vec<String> =
                            cols.iter().map(|x| tb.col(x).map(|i| tb.meta[i].key.clone()).ok_or_else(|| format!("No column `{}`", x))).collect::<R<_>>()?;
                        let unique = matches!(c, Constraint::Unique(..));
                        let taken: Vec<String> = tb.keys.indexes.iter().map(|i| i.name.clone()).collect();
                        let name = n.clone().unwrap_or_else(|| unique_name(&cols[0], &taken));
                        if taken.iter().any(|x| x.eq_ignore_ascii_case(&name)) || name.eq_ignore_ascii_case("PRIMARY") {
                            return Err(format!("There's already a key called {}", name));
                        }
                        if unique {
                            self.check_unique(&tb, &base, &keys, &format!("UNIQUE key {}", name))?;
                        }
                        tb.keys.indexes.push(Index { name, cols: keys, unique });
                    }
                    Constraint::Foreign(n, cols, r) => {
                        let fk = self.fk_from(key, &tb, n.clone(), &cols, &r)?;
                        if tb.keys.fks.iter().any(|f| f.name.eq_ignore_ascii_case(&fk.name)) {
                            return Err(format!("There's already a foreign key called {}", fk.name));
                        }
                        self.check_fk_data(key, &tb, &base, &fk)?;
                        tb.keys.fks.push(fk);
                    }
                    Constraint::Check(n, e) => self.add_check(&mut tb, &base, n, &e)?,
                },
                AlterOp::DropColumn(c) => {
                    let ci = tb.col(&c).ok_or_else(|| format!("No column `{}` in {}", c, tb.title))?;
                    if ci == tb.id_col {
                        return Err(format!(
                            "`{}` is the primary key. Make another column the key first (ALTER TABLE … DROP PRIMARY KEY, ADD PRIMARY KEY (other)).",
                            c
                        ));
                    }
                    if let Some(ck) = tb.keys.checks.iter().find(|k| mentions(&k.expr, &c)) {
                        return Err(format!("The rule {} uses `{}` — drop the rule first (ALTER TABLE … DROP CHECK {})", ck.name, c, ck.name));
                    }
                    let refd = self.referenced_by(key, &tb, &tb.meta[ci].key.clone());
                    if let Some(r) = refd {
                        return Err(format!("`{}` is referenced by {} — drop that link first", c, r));
                    }
                    if tb.columns.len() == 1 {
                        return Err("A table needs at least one column".into());
                    }
                    sheet::delete_column(&mut tb, ci)?;
                }
                AlterOp::DropIndex(n) => {
                    if n.eq_ignore_ascii_case("PRIMARY") {
                        pk_dropped = true;
                        continue;
                    }
                    let before = tb.keys.indexes.len();
                    tb.keys.indexes.retain(|i| !i.name.eq_ignore_ascii_case(&n));
                    if tb.keys.indexes.len() == before {
                        return Err(format!("No key called {} in {}", n, tb.title));
                    }
                }
                AlterOp::DropPrimary => pk_dropped = true,
                AlterOp::DropForeign(n) => {
                    let before = tb.keys.fks.len();
                    tb.keys.fks.retain(|f| !f.name.eq_ignore_ascii_case(&n));
                    if tb.keys.fks.len() == before {
                        return Err(format!("No foreign key called {} in {}", n, tb.title));
                    }
                }
                AlterOp::DropCheck(n) => {
                    let (a, b, c) = (tb.keys.checks.len(), tb.keys.fks.len(), tb.keys.indexes.len());
                    tb.keys.checks.retain(|x| !x.name.eq_ignore_ascii_case(&n));
                    tb.keys.fks.retain(|x| !x.name.eq_ignore_ascii_case(&n));
                    tb.keys.indexes.retain(|x| !(x.unique && x.name.eq_ignore_ascii_case(&n)));
                    if (a, b, c) == (tb.keys.checks.len(), tb.keys.fks.len(), tb.keys.indexes.len()) {
                        return Err(format!("No constraint called {} in {}", n, tb.title));
                    }
                }
                AlterOp::Modify(old, spec, place) => {
                    let ci = tb.col(&old).ok_or_else(|| format!("No column `{}` in {}", old, tb.title))?;
                    if !spec.name.eq_ignore_ascii_case(&old) && tb.col(&spec.name).is_some() {
                        return Err(format!("Column `{}` already exists", spec.name));
                    }
                    let is_pk = ci == tb.id_col;
                    let mut m = self.spec_meta(&spec, &tb.meta[ci].key, is_pk)?;
                    m.fill = if tb.meta[ci].fill.is_null() { Json::Null } else { m.ty.coerce(&tb.meta[ci].fill).unwrap_or(Json::Null) };
                    m.auto_added = tb.meta[ci].auto_added && m.auto_inc;
                    // every current value must fit the new definition
                    let label = spec.name.clone();
                    for r in self.live(&tb, &base) {
                        let v = &r.vals[ci];
                        let cv = m.ty.coerce(v).map_err(|e| format!("Can't change `{}` to {}: in {}, {}", old, m.ty.sql(), row_label(&tb, &r.vals), e))?;
                        if cv.is_null() && m.not_null {
                            return Err(format!(
                                "Can't make `{}` NOT NULL: {} has no value in it. Fill it in first (UPDATE {} SET {} = … WHERE {} IS NULL).",
                                label,
                                row_label(&tb, &r.vals),
                                sql_ident(&tb.title),
                                sql_ident(&old),
                                sql_ident(&old)
                            ));
                        }
                    }
                    if m.auto_inc && !is_pk && !tb.keys.indexes.iter().any(|i| i.cols == vec![m.key.clone()]) && !spec.unique && !spec.primary {
                        return Err(format!("`{}`: AUTO_INCREMENT goes on the primary key (or a UNIQUE column)", spec.name));
                    }
                    if m.auto_inc && tb.meta.iter().enumerate().any(|(i, x)| i != ci && x.auto_inc) {
                        return Err("A table can have only one AUTO_INCREMENT column".into());
                    }
                    for r in tb.rows.iter_mut() {
                        if let Some(v) = r.vals.get_mut(ci) {
                            if !v.is_null() {
                                *v = m.ty.coerce(v).unwrap_or_else(|_| v.clone());
                            }
                        }
                    }
                    tb.meta[ci] = m;
                    if !spec.name.eq(&old) {
                        let from = tb.columns[ci].clone();
                        tb.columns[ci] = spec.name.clone();
                        for ck in tb.keys.checks.iter_mut() {
                            ck.expr = rename_in_expr(&ck.expr, &from, &spec.name);
                        }
                    }
                    if let Some(p) = place {
                        let to = match p {
                            Place::First => 0,
                            Place::After(c) => {
                                let a = tb.col(&c).ok_or_else(|| format!("AFTER {}: no such column", c))?;
                                if a < ci {
                                    a + 1
                                } else {
                                    a
                                }
                            }
                        };
                        sheet::move_column(&mut tb, ci, to);
                    }
                    let ci = tb.col(&spec.name).unwrap();
                    if spec.primary && ci != tb.id_col {
                        let cur_auto = tb.meta.get(tb.id_col).map(|m| m.auto_added).unwrap_or(false);
                        if !pk_dropped && !cur_auto {
                            return Err(format!("{} already has a primary key ({})", tb.title, tb.columns[tb.id_col]));
                        }
                        self.set_pk(&mut tb, &base, ci, &mut notes)?;
                        pk_dropped = false;
                    }
                    if spec.unique {
                        let k = tb.meta[ci].key.clone();
                        self.check_unique(&tb, &base, std::slice::from_ref(&k), &format!("UNIQUE on {}", spec.name))?;
                        let taken: Vec<String> = tb.keys.indexes.iter().map(|i| i.name.clone()).collect();
                        if !tb.keys.indexes.iter().any(|i| i.unique && i.cols == vec![k.clone()]) {
                            tb.keys.indexes.push(Index { name: unique_name(&spec.name, &taken), cols: vec![k], unique: true });
                        }
                    }
                    if let Some(r) = &spec.references {
                        let fk = self.fk_from(key, &tb, None, std::slice::from_ref(&spec.name), r)?;
                        self.check_fk_data(key, &tb, &base, &fk)?;
                        tb.keys.fks.push(fk);
                    }
                    if let Some((n, e)) = &spec.check {
                        self.add_check(&mut tb, &base, n.clone(), e)?;
                    }
                }
                AlterOp::RenameColumn(a, b) => {
                    let ci = tb.col(&a).ok_or_else(|| format!("No column `{}` in {}", a, tb.title))?;
                    sheet::rename_column(&mut tb, &base, ci, &b)?;
                    for ck in tb.keys.checks.iter_mut() {
                        ck.expr = rename_in_expr(&ck.expr, &a, &b);
                    }
                }
                AlterOp::RenameIndex(a, b) => {
                    let ix = tb.keys.indexes.iter_mut().find(|i| i.name.eq_ignore_ascii_case(&a)).ok_or_else(|| format!("No key called {}", a))?;
                    ix.name = b;
                }
                AlterOp::RenameTable(n) => new_name = Some(n),
                AlterOp::SetDefault(c, d) => {
                    let ci = tb.col(&c).ok_or_else(|| format!("No column `{}` in {}", c, tb.title))?;
                    tb.meta[ci].default = match d {
                        None => None,
                        Some(DefaultSpec::Lit(v)) => Some(DefVal::Lit(tb.meta[ci].ty.coerce(&v).map_err(|e| format!("Default for `{}`: {}", c, e))?)),
                        Some(DefaultSpec::Expr(e)) => {
                            let d = Some(DefVal::Expr(e));
                            constraints::default_value(&tb.meta[ci].ty, &d)?;
                            d
                        }
                    };
                }
                AlterOp::Options(o) => {
                    if let Some(c) = o.comment {
                        tb.keys.comment = c;
                    }
                    if let Some(a) = o.auto_increment {
                        tb.keys.ai_next = a;
                    }
                    if let Some(open) = o.open {
                        privileges = Some(open);
                    }
                }
                AlterOp::Ignored(what) => notes.push(format!("({} doesn't apply here)", what)),
            }
        }
        if pk_dropped {
            return Err("Every table here needs a primary key. Drop it and add the new one in the same statement: ALTER TABLE t DROP PRIMARY KEY, ADD PRIMARY KEY (column)".into());
        }
        if let Some(open) = privileges {
            let w = self.drafts[di].wallet.clone();
            tb.remember_chain_meta(w.as_deref());
            tb.open = open;
        }
        drop(now_eng);
        let title = tb.title.clone();
        self.edit_tb(key, t, move |x, _| *x = tb);
        if let Some(n) = new_name {
            notes.insert(0, self.rename_table(key, t, &n)?);
        }
        let cost = if saved { " Saving it is one small write (0.001 SOL); rows already saved are read through the new structure." } else { "" };
        msg(format!("Table {} changed.{}{}", title, cost, if notes.is_empty() { String::new() } else { format!(" {}", notes.join(" ")) }))
    }

    fn add_check(&self, tb: &mut DraftTable, base: &[BaseRec], name: Option<String>, e: &str) -> R<()> {
        let ex = sql::parse_expr(e)?;
        let taken: Vec<String> = tb.keys.checks.iter().map(|x| x.name.clone()).collect();
        let name = name.unwrap_or_else(|| unique_name(&format!("{}_chk_{}", tb.title, tb.keys.checks.len() + 1), &taken));
        let cols: Vec<sql::Col> = tb.columns.iter().map(|c| sql::Col::new(None, c)).collect();
        let snap = crate::sql_exec::Snap { tables: Default::default(), views: Default::default() };
        let eng = sql::Engine::new(&snap, "", "");
        for r in self.live(tb, base) {
            let v = eng.eval_row(&cols, &r.vals, &ex)?;
            if eval::truthy(&v) == Some(false) {
                return Err(format!("Can't add the rule {}: {} doesn't meet it", name, row_label(tb, &r.vals)));
            }
        }
        tb.keys.checks.push(Check { name, expr: e.to_string() });
        Ok(())
    }

    /// A foreign key in another table that points at this column, if any.
    fn referenced_by(&self, key: &str, tb: &DraftTable, col_key: &str) -> Option<String> {
        let d = &self.drafts[self.draft_idx(key)?];
        for o in d.tables.iter().filter(|o| !o.dropped && o.name != tb.name) {
            for f in &o.keys.fks {
                if f.table == tb.name && f.ref_cols.iter().any(|c| c == col_key) {
                    return Some(format!("{}.{}", o.title, f.name));
                }
            }
        }
        None
    }

    fn references_to(&self, key: &str, tb: &DraftTable) -> Option<String> {
        let d = &self.drafts[self.draft_idx(key)?];
        d.tables
            .iter()
            .filter(|o| !o.dropped && o.name != tb.name)
            .find_map(|o| o.keys.fks.iter().find(|f| f.table == tb.name).map(|f| format!("{} (link {})", o.title, f.name)))
    }

    // --------------------------------------------- DROP / TRUNCATE / RENAME

    pub fn drop_table(&mut self, key: &str, t: usize) -> R<String> {
        self.require_owner(key, t)?;
        let di = self.draft_idx(key).unwrap();
        let tb = self.drafts[di].tables[t].clone();
        if self.fk_checks() {
            if let Some(r) = self.references_to(key, &tb) {
                return Err(format!("Can't drop {}: {} points to it. Drop that link (or table) first, or SET FOREIGN_KEY_CHECKS = 0.", tb.title, r));
            }
        }
        if tb.created.is_none() {
            self.drafts[di].tables.remove(t);
            self.drafts[di].sel = 0;
            self.table_removed(key, t);
            self.save_drafts();
            return Ok(format!("Table {} dropped (it was never saved).", tb.title));
        }
        self.edit_tb(key, t, |x, _| {
            x.dropped = true;
            x.clear = true;
            x.rows.retain(|r| r.sig.is_some());
        });
        self.drafts[di].sel = 0;
        Ok(format!(
            "Table {} will be deleted when you save: its rows disappear from IQ Tables and it comes off the database's table list. What was saved stays in the blockchain's history, and the name stays taken (CREATE TABLE {} brings it back empty).",
            tb.title, tb.title
        ))
    }

    pub fn truncate(&mut self, key: &str, t: usize) -> R<String> {
        self.require_owner(key, t)?;
        let tb = self.drafts[self.draft_idx(key).unwrap()].tables[t].clone();
        if self.fk_checks() {
            if let Some(r) = self.references_to(key, &tb) {
                return Err(format!("Can't empty {}: {} points to it. Delete those rows first, or SET FOREIGN_KEY_CHECKS = 0.", tb.title, r));
            }
        }
        let saved = tb.created.is_some();
        self.edit_tb(key, t, |x, _| {
            x.rows.retain(|r| r.sig.is_some());
            if saved {
                x.clear = true;
            } else {
                x.rows.clear();
            }
            x.keys.ai_next = 0;
        });
        Ok(if saved { format!("{} will be emptied when you save (one small write, whatever its size).", tb.title) } else { format!("{} emptied.", tb.title) })
    }

    /// Mark a table for a checkpoint (or un-mark it): the next save rewrites
    /// its rows in as few writes as possible and records that readers can
    /// start there instead of replaying its whole history.
    pub fn checkpoint(&mut self, key: &str, t: usize, on: bool) -> R<String> {
        self.require_owner(key, t)?;
        let di = self.draft_idx(key).unwrap();
        let tb = self.drafts[di].tables[t].clone();
        if tb.created.is_none() {
            return Ok(format!("{} isn't saved yet, so it has no history to skip.", tb.title));
        }
        self.drafts[di].tables[t].checkpoint = on;
        self.bump(key, t);
        self.save_drafts();
        Ok(if on {
            format!(
                "{} will get a checkpoint when you save: its rows are rewritten together, and from then on it opens without replaying older history.",
                tb.title
            )
        } else {
            format!("Checkpoint for {} cancelled.", tb.title)
        })
    }

    pub fn rename_table(&mut self, key: &str, t: usize, new: &str) -> R<String> {
        valid_name("Table", new)?;
        let di = self.draft_idx(key).unwrap();
        let old = self.drafts[di].tables[t].title.clone();
        if !new.eq_ignore_ascii_case(&old)
            && (self.table_names(key).iter().any(|n| n.eq_ignore_ascii_case(new)) || self.views(key).iter().any(|(v, _)| v.eq_ignore_ascii_case(new)))
        {
            return Err(format!("There's already a table or view called {}", new));
        }
        self.require_owner(key, t)?;
        let saved = self.drafts[di].tables[t].created.is_some();
        let new_s = new.to_string();
        let w = self.drafts[di].wallet.clone();
        self.edit_tb(key, t, move |x, _| {
            x.remember_chain_meta(w.as_deref());
            x.title = new_s.clone();
            if !saved {
                x.name = new_s;
            }
        });
        Ok(if saved {
            format!("Table {} renamed to {} (the change is saved on chain with the next COMMIT; its address stays the same).", old, new)
        } else {
            format!("Table {} renamed to {}.", old, new)
        })
    }

    // ------------------------------------------------------------ VIEWS

    fn ensure_system_table(&mut self, key: &str) -> usize {
        if let Some(t) = self.system_table(key) {
            let di = self.draft_idx(key).unwrap();
            let tb = &mut self.drafts[di].tables[t];
            // an older copy read from the chain has untyped columns: fine
            tb.fix_meta();
            return t;
        }
        let di = self.draft_idx(key).unwrap();
        let tb = DraftTable::typed(
            SYSTEM_TABLE,
            vec![
                ("name".into(), ColMeta { not_null: true, ..ColMeta::typed("name", Ty::Varchar(64)) }),
                ("kind".into(), ColMeta::typed("kind", Ty::Varchar(16))),
                ("body".into(), ColMeta::typed("body", Ty::Text(TextKind::Medium))),
            ],
            0,
        );
        self.drafts[di].tables.push(tb);
        self.save_drafts();
        self.drafts[di].tables.len() - 1
    }

    fn create_view(&mut self, key: &str, name: &str, or_replace: bool, cols: Vec<String>, text: &str, q: &Query) -> R<Vec<Out>> {
        valid_name("View", name)?;
        if !self.owns(key) {
            return Err("Only the database's owner can add views to it.".into());
        }
        if self.tbl(key, name).is_ok() {
            return Err(format!("There's already a table called {}", name));
        }
        let exists = self.views(key).iter().any(|(v, _)| v.eq_ignore_ascii_case(name));
        if exists && !or_replace {
            return Err(format!("View {} already exists (use CREATE OR REPLACE VIEW)", name));
        }
        // it has to run
        let snap = self.snapshot(key);
        let eng = self.engine_for(key, &snap);
        let r = eng.query(q, None).map_err(|e| format!("The view's query doesn't run: {}", e))?;
        drop(eng);
        let body = if cols.is_empty() {
            text.to_string()
        } else {
            if cols.len() != r.names().len() {
                return Err(format!("{} names for {} columns", cols.len(), r.names().len()));
            }
            // keep the column names by wrapping the query
            format!(
                "SELECT {} FROM ({}) AS v",
                r.names().iter().zip(&cols).map(|(a, b)| format!("{} AS {}", sql_ident(a), sql_ident(b))).collect::<Vec<_>>().join(", "),
                text
            )
        };
        let st = self.ensure_system_table(key);
        let rows = self.sheet_rows(key, st);
        let existing =
            rows.iter().find(|r| r.state != RowState::Deleted && r.vals.first().map(|v| v.cell_text().eq_ignore_ascii_case(name)).unwrap_or(false)).cloned();
        let vals = vec![json::s(name), json::s("view"), json::s(&body)];
        let ch = match existing {
            Some(row) => Change::Update { row, vals, set: vec![true; 3] },
            None => Change::Insert { vals, given: vec![true; 3] },
        };
        self.apply_changes(key, st, vec![ch], &Opts { strict: true, fk_checks: false })?;
        msg(format!(
            "View {} {}. It's stored with the database{}.",
            name,
            if exists { "replaced" } else { "created" },
            if self.drafts[self.draft_idx(key).unwrap()].tables[st].created.is_none() {
                " (the first view adds a small settings table, about 0.017 SOL, when you save)"
            } else {
                ""
            }
        ))
    }

    fn drop_view(&mut self, key: &str, name: &str, if_exists: bool) -> R<String> {
        let Some(st) = self.system_table(key) else {
            return if if_exists { Ok(format!("No view {}.", name)) } else { Err(format!("No view called {}", name)) };
        };
        let rows = self.sheet_rows(key, st);
        let Some(row) =
            rows.into_iter().find(|r| r.state != RowState::Deleted && r.vals.first().map(|v| v.cell_text().eq_ignore_ascii_case(name)).unwrap_or(false))
        else {
            return if if_exists { Ok(format!("No view {}.", name)) } else { Err(format!("No view called {}", name)) };
        };
        self.apply_changes(key, st, vec![Change::Delete { row }], &Opts { strict: false, fk_checks: false })?;
        Ok(format!("View {} dropped.", name))
    }

    // ------------------------------------------------------------ GRANT

    fn grant(&mut self, key: &str, table: &str, to: Vec<String>, revoke: bool) -> R<Vec<Out>> {
        let ts: Vec<usize> = if table == "*" {
            let d = &self.drafts[self.draft_idx(key).unwrap()];
            (0..d.tables.len()).filter(|&t| !d.tables[t].dropped && !d.tables[t].is_system()).collect()
        } else {
            vec![self.tbl(key, table)?]
        };
        if !self.owns(key) {
            return Err("Only the database's owner decides who may write to its tables.".into());
        }
        let mut notes = vec![];
        for t in ts {
            let title = self.drafts[self.draft_idx(key).unwrap()].tables[t].title.clone();
            for g in &to {
                let dw = self.drafts[self.draft_idx(key).unwrap()].wallet.clone();
                if g.eq_ignore_ascii_case("PUBLIC") || g == "%" || g == "*" {
                    let open = !revoke;
                    self.edit_tb(key, t, move |x, _| {
                        x.remember_chain_meta(dw.as_deref());
                        x.open = open;
                    });
                    notes.push(format!(
                        "{}: {}",
                        title,
                        if open { "anyone may now add rows (they show as unofficial)" } else { "only the database wallet and listed wallets may add rows" }
                    ));
                    continue;
                }
                let w = g.split('@').next().unwrap_or("").trim_matches(|c| c == '\'' || c == '"' || c == '`').to_string();
                if crate::crypto::base58::decode32(&w).is_none() {
                    return Err(format!("“{}” isn't a Solana wallet address. GRANT gives write access to wallets: GRANT INSERT ON {} TO 'address'", w, title));
                }
                let wc = w.clone();
                self.edit_tb(key, t, move |x, _| {
                    x.remember_chain_meta(dw.as_deref());
                    if revoke {
                        x.writers.retain(|a| *a != wc);
                    } else if !x.writers.contains(&wc) {
                        x.writers.push(wc);
                    }
                });
                let open = self.drafts[self.draft_idx(key).unwrap()].tables[t].open;
                notes.push(format!(
                    "{}: {} {}{}",
                    title,
                    crate::solana::short(&w),
                    if revoke { "can no longer add rows" } else { "may add rows" },
                    if open && !revoke { " (the table is open to everyone anyway)" } else { "" }
                ));
            }
        }
        msg(format!(
            "{}. Saved on chain with the next COMMIT. (Writing is all-or-nothing on IQ: a wallet that may insert may also update and delete its own rows.)",
            notes.join("; ")
        ))
    }

    // ------------------------------------------------------------- SHOW

    fn show(&mut self, key: &str, s: Show) -> R<Vec<Out>> {
        let di = self.draft_idx(key).ok_or("No such database")?;
        let dname = self.drafts[di].name.clone();
        match s {
            Show::Tables { full, like } => {
                let mut rows = vec![];
                let pat = like.map(|l| l.to_lowercase());
                let keep = |n: &str| pat.as_ref().map(|p| eval::like(n, p, '\\')).unwrap_or(true);
                for tb in self.drafts[di].tables.iter().filter(|t| !t.dropped && !t.is_system()) {
                    if keep(&tb.title) {
                        rows.push(if full { vec![json::s(&tb.title), json::s("BASE TABLE")] } else { vec![json::s(&tb.title)] });
                    }
                }
                for (v, _) in self.views(key) {
                    if keep(&v) {
                        rows.push(if full { vec![json::s(&v), json::s("VIEW")] } else { vec![json::s(&v)] });
                    }
                }
                let col = format!("Tables_in_{}", dname);
                let cols: Vec<&str> = if full { vec![&col, "Table_type"] } else { vec![&col] };
                rows_out("tables", &cols, rows, "")
            }
            Show::Databases => {
                let rows = self
                    .drafts
                    .iter()
                    .map(|d| vec![json::s(&d.name), json::s(if d.root_sig.is_some() { "on the blockchain" } else { "not saved yet" })])
                    .collect();
                rows_out("databases", &["Database", "Status"], rows, "Databases open in this editor")
            }
            Show::Columns { table, full } => {
                let t = self.tbl(key, &table);
                let t = match t {
                    Ok(t) => t,
                    Err(e) => {
                        // DESCRIBE a view: its result columns
                        if let Some((_, q)) = self.views(key).into_iter().find(|(v, _)| v.eq_ignore_ascii_case(&table)) {
                            let snap = self.snapshot(key);
                            let eng = self.engine_for(key, &snap);
                            let r = eng.query(&sql::parse_query(&q)?, None)?;
                            return rows_out(
                                &table,
                                &["Field", "Type"],
                                r.names().iter().map(|n| vec![json::s(n), json::s("(view column)")]).collect(),
                                "a view",
                            );
                        }
                        return Err(e);
                    }
                };
                let tb = self.drafts[di].tables[t].clone();
                let mut rows = vec![];
                for (c, m) in tb.meta.iter().enumerate() {
                    let k = &m.key;
                    let keyk = if c == tb.id_col {
                        "PRI"
                    } else if tb.keys.indexes.iter().any(|i| i.unique && i.cols.first() == Some(k)) {
                        "UNI"
                    } else if tb.keys.indexes.iter().any(|i| i.cols.first() == Some(k)) || tb.keys.fks.iter().any(|f| f.cols.first() == Some(k)) {
                        "MUL"
                    } else {
                        ""
                    };
                    let def = match &m.default {
                        None => Json::Null,
                        Some(DefVal::Lit(v)) => json::s(&v.cell_text()),
                        Some(DefVal::Expr(e)) => json::s(e),
                    };
                    let mut extra = vec![];
                    if m.auto_inc {
                        extra.push("auto_increment".to_string());
                    }
                    if m.on_update_now {
                        extra.push("on update CURRENT_TIMESTAMP".into());
                    }
                    let mut row = vec![
                        json::s(&tb.columns[c]),
                        json::s(&type_sql(m, c == tb.id_col)),
                        json::s(if m.not_null || c == tb.id_col { "NO" } else { "YES" }),
                        json::s(keyk),
                        def,
                        json::s(&extra.join(" ")),
                    ];
                    if full {
                        row.push(json::s(&m.comment));
                    }
                    rows.push(row);
                }
                let mut cols = vec!["Field", "Type", "Null", "Key", "Default", "Extra"];
                if full {
                    cols.push("Comment");
                }
                rows_out(
                    &tb.title,
                    &cols,
                    rows,
                    format!(
                        "{} · {}",
                        if tb.open { "open to everyone" } else { "locked to the owner" },
                        if tb.created.is_some() { "on the blockchain" } else { "not saved yet" }
                    ),
                )
            }
            Show::CreateTable(table) => {
                let t = self.tbl(key, &table)?;
                let tb = self.drafts[di].tables[t].clone();
                let sql = self.create_sql(key, &tb);
                rows_out(&tb.title, &["Table", "Create Table"], vec![vec![json::s(&tb.title), json::s(&sql)]], "")
            }
            Show::CreateView(v) => {
                let (n, q) = self.views(key).into_iter().find(|(x, _)| x.eq_ignore_ascii_case(&v)).ok_or_else(|| format!("No view called {}", v))?;
                rows_out(&n, &["View", "Create View"], vec![vec![json::s(&n), json::s(&format!("CREATE VIEW {} AS {}", sql_ident(&n), q))]], "")
            }
            Show::Index(table) => {
                let t = self.tbl(key, &table)?;
                let tb = self.drafts[di].tables[t].clone();
                let name_of = |k: &str| tb.meta.iter().position(|m| m.key == k).map(|i| tb.columns[i].clone()).unwrap_or_default();
                let mut rows = vec![vec![json::s(&tb.title), json::n(0), json::s("PRIMARY"), json::n(1), json::s(&tb.columns[tb.id_col])]];
                for ix in &tb.keys.indexes {
                    for (i, c) in ix.cols.iter().enumerate() {
                        rows.push(vec![json::s(&tb.title), json::n(if ix.unique { 0 } else { 1 }), json::s(&ix.name), json::n(i + 1), json::s(&name_of(c))]);
                    }
                }
                for f in &tb.keys.fks {
                    for (i, c) in f.cols.iter().enumerate() {
                        rows.push(vec![json::s(&tb.title), json::n(1), json::s(&f.name), json::n(i + 1), json::s(&name_of(c))]);
                    }
                }
                rows_out(
                    &tb.title,
                    &["Table", "Non_unique", "Key_name", "Seq_in_index", "Column_name"],
                    rows,
                    "Keys here are rules (unique, links) — lookups don't need them",
                )
            }
            Show::TableStatus => {
                let mut rows = vec![];
                let n = self.drafts[di].tables.len();
                for t in 0..n {
                    let tb = self.drafts[di].tables[t].clone();
                    if tb.dropped || tb.is_system() {
                        continue;
                    }
                    let r = self.sheet_rows(key, t);
                    let (a, c, d) = sheet::pending(&r);
                    let live = r.iter().filter(|x| x.state != RowState::Deleted).count();
                    let ai = tb
                        .meta
                        .iter()
                        .position(|m| m.auto_inc)
                        .map(|c| r.iter().filter_map(|x| x.vals.get(c).and_then(eval::num)).fold(0.0f64, f64::max) as u64 + 1);
                    rows.push(vec![
                        json::s(&tb.title),
                        json::n(live),
                        json::s(if tb.created.is_some() { "on the blockchain" } else { "not saved yet" }),
                        json::s(&writers_text(&tb)),
                        ai.map(|x| json::n(x.max(tb.keys.ai_next))).unwrap_or(Json::Null),
                        json::s(&tb.keys.comment),
                        json::s(&if a + c + d == 0 { "—".to_string() } else { format!("{} new, {} changed, {} deleted", a, c, d) }),
                    ]);
                }
                rows_out("table status", &["Name", "Rows", "Status", "Who can add rows", "Auto_increment", "Comment", "Unsaved"], rows, "")
            }
            Show::Changes => {
                let n = self.drafts[di].tables.len();
                let mut rows = vec![];
                for t in 0..n {
                    let tb = self.drafts[di].tables[t].clone();
                    let r = self.sheet_rows(key, t);
                    let (a, c, d) = sheet::pending(&r);
                    let structure = if tb.dropped {
                        "delete table"
                    } else if tb.clear {
                        "empty table"
                    } else if tb.schema_changed() && tb.created.is_some() {
                        "structure"
                    } else if tb.created.is_none() {
                        "new table"
                    } else {
                        "—"
                    };
                    if a + c + d == 0 && structure == "—" {
                        continue;
                    }
                    rows.push(vec![json::s(if tb.is_system() { "(views)" } else { &tb.title }), json::n(a), json::n(c), json::n(d), json::s(structure)]);
                }
                let (cost, packs) = self.save_estimate(key);
                rows_out(
                    "unsaved changes",
                    &["table", "new", "changed", "deleted", "other"],
                    rows,
                    format!("COMMIT would write {} pack(s) · about {}", packs, crate::ui::sol(cost)),
                )
            }
            Show::Grants(table) => {
                let mut rows = vec![];
                for tb in self.drafts[di].tables.iter().filter(|t| !t.dropped && !t.is_system()) {
                    if let Some(t) = &table {
                        if !tb.title.eq_ignore_ascii_case(t) {
                            continue;
                        }
                    }
                    rows.push(vec![json::s(&tb.title), json::s(&writers_text(tb))]);
                }
                rows_out(
                    "grants",
                    &["table", "who can add rows"],
                    rows,
                    "The database wallet can always write. GRANT INSERT ON t TO 'wallet' / TO PUBLIC changes this.",
                )
            }
            Show::Variables => {
                let mut rows: Vec<Vec<Json>> = self.ed.sql_vars.iter().map(|(k, v)| vec![json::s(k), v.clone()]).collect();
                rows.sort_by(|a, b| a[0].cell_text().cmp(&b[0].cell_text()));
                rows.push(vec![json::s("foreign_key_checks"), json::n(if self.fk_checks() { 1 } else { 0 })]);
                rows_out("variables", &["Variable_name", "Value"], rows, "")
            }
            Show::Warnings => rows_out("warnings", &["Level", "Code", "Message"], vec![], "None"),
        }
    }

    // ------------------------------------------------------ SQL output

    /// CREATE TABLE text for a table (valid MySQL).
    pub fn create_sql(&self, key: &str, tb: &DraftTable) -> String {
        if tb.columns.is_empty() {
            return format!("-- {}: its structure hasn't been read from the blockchain yet", sql_ident(&tb.title));
        }
        let d = &self.drafts[self.draft_idx(key).unwrap()];
        let name_of = |t: &DraftTable, k: &str| t.meta.iter().position(|m| m.key == k).map(|i| t.columns[i].clone()).unwrap_or_else(|| k.to_string());
        let mut lines = vec![];
        for (c, m) in tb.meta.iter().enumerate() {
            let mut l = format!("  {} {}", sql_ident(&tb.columns[c]), type_sql(m, c == tb.id_col || tb.keys.indexes.iter().any(|i| i.cols.contains(&m.key))));
            if m.not_null || c == tb.id_col {
                l.push_str(" NOT NULL");
            }
            match &m.default {
                Some(DefVal::Lit(v)) => l.push_str(&format!(" DEFAULT {}", sql_value(v))),
                Some(DefVal::Expr(e)) => l.push_str(&format!(" DEFAULT {}", e)),
                None if !m.not_null && c != tb.id_col && !m.auto_inc => l.push_str(" DEFAULT NULL"),
                None => {}
            }
            if m.on_update_now {
                l.push_str(" ON UPDATE CURRENT_TIMESTAMP");
            }
            if m.auto_inc {
                l.push_str(" AUTO_INCREMENT");
            }
            if !m.comment.is_empty() {
                l.push_str(&format!(" COMMENT {}", sql_str(&m.comment)));
            }
            lines.push(l);
        }
        lines.push(format!("  PRIMARY KEY ({})", sql_ident(&tb.columns[tb.id_col])));
        for ix in &tb.keys.indexes {
            lines.push(format!(
                "  {}KEY {} ({})",
                if ix.unique { "UNIQUE " } else { "" },
                sql_ident(&ix.name),
                ix.cols.iter().map(|k| sql_ident(&name_of(tb, k))).collect::<Vec<_>>().join(", ")
            ));
        }
        for f in &tb.keys.fks {
            let rt = d.tables.iter().find(|t| t.name == f.table);
            let rname = rt.map(|t| t.title.clone()).unwrap_or_else(|| f.table.clone());
            let rcols: Vec<String> = f.ref_cols.iter().map(|k| rt.map(|t| name_of(t, k)).unwrap_or_else(|| k.clone())).collect();
            let mut l = format!(
                "  CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
                sql_ident(&f.name),
                f.cols.iter().map(|k| sql_ident(&name_of(tb, k))).collect::<Vec<_>>().join(", "),
                sql_ident(&rname),
                rcols.iter().map(|c| sql_ident(c)).collect::<Vec<_>>().join(", ")
            );
            if f.on_delete != RefAction::Restrict {
                l.push_str(&format!(" ON DELETE {}", f.on_delete.sql()));
            }
            if f.on_update != RefAction::Restrict {
                l.push_str(&format!(" ON UPDATE {}", f.on_update.sql()));
            }
            lines.push(l);
        }
        for ck in &tb.keys.checks {
            lines.push(format!("  CONSTRAINT {} CHECK ({})", sql_ident(&ck.name), ck.expr));
        }
        let mut s = format!("CREATE TABLE {} (\n{}\n)", sql_ident(&tb.title), lines.join(",\n"));
        if !tb.keys.comment.is_empty() {
            s.push_str(&format!(" COMMENT={}", sql_str(&tb.keys.comment)));
        }
        s
    }

    /// A SQL file of tables (structure, rows) and views — phpMyAdmin style.
    pub fn dump_sql(&mut self, key: &str, only: Option<usize>, structure: bool, data: bool) -> String {
        let di = self.draft_idx(key).unwrap();
        let d = self.drafts[di].clone();
        let mut out = format!(
            "-- IQ Tables SQL dump\n-- Database: {}\n-- Written: {}\n--\n-- Load it back here (Import → SQL) or into MySQL / MariaDB.\n\nSET FOREIGN_KEY_CHECKS = 0;\n\n",
            d.name,
            crate::dates::now_local().datetime_str()
        );
        let n = d.tables.len();
        // referenced tables first
        let mut order: Vec<usize> = (0..n).filter(|&t| !d.tables[t].dropped && !d.tables[t].is_system() && only.map(|o| o == t).unwrap_or(true)).collect();
        order.sort_by_key(|&t| d.tables[t].keys.fks.iter().filter(|f| f.table != d.tables[t].name).count());
        for t in order {
            let tb = d.tables[t].clone();
            if structure {
                out.push_str(&format!(
                    "-- --------------------------------------------------------\n-- Table {}\n\n{};\n\n",
                    sql_ident(&tb.title),
                    self.create_sql(key, &tb)
                ));
            }
            if data {
                let rows: Vec<Vec<Json>> = self.sheet_rows(key, t).into_iter().filter(|r| r.state != RowState::Deleted).map(|r| r.vals).collect();
                if rows.is_empty() {
                    continue;
                }
                let cols = tb.columns.iter().map(|c| sql_ident(c)).collect::<Vec<_>>().join(", ");
                for chunk in rows.chunks(100) {
                    out.push_str(&format!("INSERT INTO {} ({}) VALUES\n", sql_ident(&tb.title), cols));
                    let lines: Vec<String> = chunk
                        .iter()
                        .map(|r| {
                            let mut r = r.clone();
                            r.resize(tb.columns.len(), Json::Null);
                            format!("({})", r.iter().map(sql_value).collect::<Vec<_>>().join(", "))
                        })
                        .collect();
                    out.push_str(&lines.join(",\n"));
                    out.push_str(";\n");
                }
                out.push('\n');
            }
        }
        if only.is_none() && structure {
            for (v, q) in self.views(key) {
                out.push_str(&format!("CREATE OR REPLACE VIEW {} AS {};\n", sql_ident(&v), q));
            }
        }
        out.push_str("\nSET FOREIGN_KEY_CHECKS = 1;\n");
        out
    }
}

fn keyv(vals: &[Json], cols: &[usize]) -> Option<String> {
    let mut k = String::new();
    for &c in cols {
        let v = vals.get(c)?;
        if v.is_null() {
            return None;
        }
        k.push_str(&eval::key(v));
        k.push('\u{1}');
    }
    Some(k)
}

fn row_label(tb: &DraftTable, vals: &[Json]) -> String {
    format!("the row with {} = {}", tb.columns[tb.id_col], vals.get(tb.id_col).map(|v| v.cell_text()).unwrap_or_default())
}

/// A column's type for SQL output: untyped columns become TEXT (or
/// VARCHAR(255) where a key needs a length).
pub fn type_sql(m: &ColMeta, keyed: bool) -> String {
    match &m.ty {
        Ty::Any => {
            if keyed {
                "VARCHAR(255)".into()
            } else {
                "TEXT".into()
            }
        }
        t => t.sql(),
    }
}

pub fn writers_text(tb: &DraftTable) -> String {
    if tb.open {
        "anyone (unofficial rows)".into()
    } else if tb.writers.is_empty() {
        "the database wallet".into()
    } else {
        format!("the database wallet + {}", tb.writers.iter().map(|w| crate::solana::short(w)).collect::<Vec<_>>().join(", "))
    }
}
