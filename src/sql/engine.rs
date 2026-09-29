//! Query execution over in-memory relations: FROM with joins, WHERE,
//! GROUP BY / HAVING / ROLLUP, window functions, DISTINCT, ORDER BY,
//! LIMIT, set operations, CTEs (incl. recursive), subqueries (correlated
//! ones re-run per row; uncorrelated ones run once).

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

use super::ast::*;
use super::eval::{self, FnEnv};
use crate::dates::Dt;
use crate::json::{self, Json};

#[derive(Clone, Debug, PartialEq)]
pub struct Col {
    /// Table name or alias the column belongs to (for t.col).
    pub table: Option<String>,
    pub name: String,
    /// Left out of SELECT * (USING duplicates, row ids).
    pub hidden: bool,
}

impl Col {
    pub fn new(table: Option<&str>, name: &str) -> Col {
        Col { table: table.map(String::from), name: name.to_string(), hidden: false }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Rel {
    pub cols: Vec<Col>,
    pub rows: Vec<Vec<Json>>,
}

impl Rel {
    pub fn names(&self) -> Vec<String> {
        self.cols.iter().filter(|c| !c.hidden).map(|c| c.name.clone()).collect()
    }
    /// Rows without hidden columns.
    pub fn visible_rows(&self) -> Vec<Vec<Json>> {
        let keep: Vec<usize> = self.cols.iter().enumerate().filter(|(_, c)| !c.hidden).map(|(i, _)| i).collect();
        if keep.len() == self.cols.len() {
            return self.rows.clone();
        }
        self.rows.iter().map(|r| keep.iter().map(|&i| r[i].clone()).collect()).collect()
    }
}

pub trait Catalog {
    /// A base table's columns (unqualified) and live rows.
    fn table(&self, name: &str) -> Option<Rc<Rel>>;
    /// A view's SELECT text.
    fn view(&self, name: &str) -> Option<String>;
}

pub struct Group<'a> {
    pub rows: &'a [Vec<Json>],
    pub idx: &'a [usize],
}

#[derive(Clone, Copy)]
pub struct Scope<'a> {
    pub cols: &'a [Col],
    pub row: &'a [Json],
    pub group: Option<&'a Group<'a>>,
    /// SELECT-list names and values (ORDER BY / HAVING may use them).
    pub aliases: Option<(&'a [String], &'a [Json])>,
    /// Window results: (by expression address, candidate index).
    pub win: Option<(&'a HashMap<usize, Vec<Json>>, usize)>,
    pub outer: Option<&'a Scope<'a>>,
}

impl<'a> Scope<'a> {
    pub fn row(cols: &'a [Col], row: &'a [Json], outer: Option<&'a Scope<'a>>) -> Scope<'a> {
        Scope { cols, row, group: None, aliases: None, win: None, outer }
    }
}

static EMPTY_COLS: [Col; 0] = [];
static EMPTY_ROW: [Json; 0] = [];

fn empty_scope<'a>() -> Scope<'a> {
    Scope { cols: &EMPTY_COLS, row: &EMPTY_ROW, group: None, aliases: None, win: None, outer: None }
}

type R<T> = Result<T, String>;

pub struct Engine<'c> {
    pub cat: &'c dyn Catalog,
    pub vars: RefCell<HashMap<String, Json>>,
    pub now: Dt,
    pub database: String,
    pub user: String,
    pub last_insert_id: Cell<u64>,
    pub row_count: Cell<i64>,
    pub found_rows: Cell<u64>,
    rand: Cell<u64>,
    ctes: RefCell<Vec<HashMap<String, Rc<Rel>>>>,
    sub_cache: RefCell<HashMap<usize, Rc<Rel>>>,
    touched_outer: Cell<bool>,
    depth: Cell<u32>,
    /// VALUES(col) inside ON DUPLICATE KEY UPDATE: (columns, the row that would have been inserted)
    pub values_row: RefCell<Option<(Vec<String>, Vec<Json>)>>,
    /// Tables that get a hidden row-number column (UPDATE/DELETE targets).
    pub rid_tables: RefCell<Vec<String>>,
}

pub const RID: &str = "__rid";

impl<'c> Engine<'c> {
    pub fn new(cat: &'c dyn Catalog, database: &str, user: &str) -> Engine<'c> {
        let mut seed = [0u8; 8];
        crate::host::random(&mut seed);
        Engine {
            cat,
            vars: RefCell::new(HashMap::new()),
            now: crate::dates::now_local(),
            database: database.to_string(),
            user: user.to_string(),
            last_insert_id: Cell::new(0),
            row_count: Cell::new(-1),
            found_rows: Cell::new(0),
            rand: Cell::new(u64::from_le_bytes(seed) | 1),
            ctes: RefCell::new(vec![]),
            sub_cache: RefCell::new(HashMap::new()),
            touched_outer: Cell::new(false),
            depth: Cell::new(0),
            values_row: RefCell::new(None),
            rid_tables: RefCell::new(vec![]),
        }
    }

    // --------------------------------------------------------- columns

    fn find(&self, cols: &[Col], t: Option<&str>, name: &str) -> R<Option<usize>> {
        let mut hit: Option<usize> = None;
        for (i, c) in cols.iter().enumerate() {
            if !c.name.eq_ignore_ascii_case(name) {
                continue;
            }
            match t {
                Some(t) => {
                    if c.table.as_deref().map(|x| x.eq_ignore_ascii_case(t)).unwrap_or(false) {
                        return Ok(Some(i));
                    }
                }
                None => {
                    if c.hidden && c.name != RID {
                        continue;
                    }
                    if let Some(h) = hit {
                        if cols[h].table != c.table {
                            return Err(format!("Column `{}` is ambiguous — write table.{}", name, name));
                        }
                    } else {
                        hit = Some(i);
                    }
                }
            }
        }
        if hit.is_none() && t.is_none() {
            // a hidden USING column is still reachable by name
            hit = cols.iter().position(|c| c.name.eq_ignore_ascii_case(name));
        }
        Ok(hit)
    }

    fn column(&self, sc: &Scope, t: Option<&str>, name: &str) -> R<Json> {
        if t.is_none() {
            if let Some((names, vals)) = sc.aliases {
                if let Some(p) = names.iter().position(|n| n.eq_ignore_ascii_case(name)) {
                    // prefer a real column over an alias of the same name
                    if self.find(sc.cols, None, name)?.is_none() || sc.group.is_some() {
                        return Ok(vals[p].clone());
                    }
                }
            }
        }
        if let Some(i) = self.find(sc.cols, t, name)? {
            return Ok(sc.row.get(i).cloned().unwrap_or(Json::Null));
        }
        let mut o = sc.outer;
        while let Some(s) = o {
            if let Some(i) = self.find(s.cols, t, name)? {
                self.touched_outer.set(true);
                return Ok(s.row.get(i).cloned().unwrap_or(Json::Null));
            }
            o = s.outer;
        }
        let known: Vec<String> = sc.cols.iter().filter(|c| !c.hidden).map(|c| c.name.clone()).collect();
        Err(match t {
            Some(t) => format!("Unknown column `{}.{}`", t, name),
            None => {
                if known.is_empty() {
                    format!("Unknown column `{}`", name)
                } else {
                    format!("Unknown column `{}` (columns: {})", name, dedup(known).join(", "))
                }
            }
        })
    }

    fn fn_env(&self) -> FnEnv<'_> {
        FnEnv {
            now: self.now,
            database: &self.database,
            user: &self.user,
            last_insert_id: self.last_insert_id.get(),
            row_count: self.row_count.get(),
            found_rows: self.found_rows.get(),
            rand: &self.rand,
        }
    }

    // ----------------------------------------------------- expressions

    pub fn eval_const(&self, e: &Expr) -> R<Json> {
        self.eval(e, &empty_scope())
    }

    pub fn eval_row(&self, cols: &[Col], row: &[Json], e: &Expr) -> R<Json> {
        self.eval(e, &Scope::row(cols, row, None))
    }

    pub fn truthy(&self, e: &Expr, sc: &Scope) -> R<bool> {
        Ok(eval::truthy(&self.eval(e, sc)?).unwrap_or(false))
    }

    pub fn eval(&self, e: &Expr, sc: &Scope) -> R<Json> {
        Ok(match e {
            Expr::Lit(v) => v.clone(),
            Expr::Col(t, n) => self.column(sc, t.as_deref(), n)?,
            Expr::Var(v) => self.var(v),
            Expr::Default => return Err("DEFAULT can only be used as a value in INSERT or UPDATE".into()),
            Expr::Values(c) => match &*self.values_row.borrow() {
                Some((cols, vals)) => cols.iter().position(|x| x.eq_ignore_ascii_case(c)).map(|p| vals[p].clone()).unwrap_or(Json::Null),
                None => Json::Null,
            },
            Expr::Unary(op, a) => {
                let v = self.eval(a, sc)?;
                match *op {
                    "NOT" => match eval::truthy(&v) {
                        None => Json::Null,
                        Some(b) => eval::b(!b),
                    },
                    "-" => match &v {
                        Json::Null => Json::Null,
                        Json::Num(n) => Json::Num(if let Some(x) = n.strip_prefix('-') { x.to_string() } else { format!("-{}", n) }),
                        other => eval::num_lenient(other).map(|f| eval::fmt_num(-f)).unwrap_or(Json::Null),
                    },
                    _ => eval::num_lenient(&v).map(|f| json::n(!(f as i64))).unwrap_or(Json::Null),
                }
            }
            Expr::Bin(op, a, b) => self.bin(op, a, b, sc)?,
            Expr::Like { e, pat, esc, not } => {
                let x = self.eval(e, sc)?;
                let p = self.eval(pat, sc)?;
                if x.is_null() || p.is_null() {
                    return Ok(Json::Null);
                }
                let esc = match esc {
                    Some(e) => eval::text(&self.eval(e, sc)?).chars().next().unwrap_or('\\'),
                    None => '\\',
                };
                eval::b(eval::like(&eval::text(&x), &eval::text(&p), esc) != *not)
            }
            Expr::Regexp { e, pat, not } => {
                let x = self.eval(e, sc)?;
                let p = self.eval(pat, sc)?;
                if x.is_null() || p.is_null() {
                    return Ok(Json::Null);
                }
                let r = super::regex::Regex::new(&eval::text(&p), true)?;
                eval::b(r.is_match(&eval::text(&x)) != *not)
            }
            Expr::In { e, list, not } => {
                let x = self.eval(e, sc)?;
                let mut vals = vec![];
                for l in list {
                    vals.push(self.eval(l, sc)?);
                }
                in_list(&x, &vals, *not)
            }
            Expr::InQuery { e, q, not } => {
                let x = self.eval(e, sc)?;
                let r = self.subquery(q, sc)?;
                let want = match &x {
                    Json::Arr(v) => v.len(),
                    _ => 1,
                };
                if r.names().len() != want {
                    return Err(format!("The subquery after IN must return {} column{}", want, if want == 1 { "" } else { "s" }));
                }
                let vals: Vec<Json> = r.visible_rows().into_iter().map(|mut row| if want == 1 { row.swap_remove(0) } else { Json::Arr(row) }).collect();
                in_list(&x, &vals, *not)
            }
            Expr::Quantified { e, op, all, q } => {
                let x = self.eval(e, sc)?;
                let r = self.subquery(q, sc)?;
                if r.names().len() != 1 {
                    return Err("The subquery after ANY/ALL must return one column".into());
                }
                let mut saw_null = x.is_null();
                let mut result = *all;
                for row in r.visible_rows() {
                    match eval::compare(&x, &row[0]) {
                        None => saw_null = true,
                        Some(o) => {
                            let hit = cmp_op(op, o);
                            if *all && !hit {
                                result = false;
                                break;
                            }
                            if !*all && hit {
                                result = true;
                                break;
                            }
                        }
                    }
                }
                if saw_null && result == *all {
                    Json::Null
                } else {
                    eval::b(result)
                }
            }
            Expr::Exists(q, not) => {
                let r = self.subquery(q, sc)?;
                eval::b(r.rows.is_empty() == *not)
            }
            Expr::Scalar(q) => {
                let r = self.subquery(q, sc)?;
                if r.names().len() != 1 {
                    return Err("A subquery used as a value must return one column".into());
                }
                match r.rows.len() {
                    0 => Json::Null,
                    1 => r.visible_rows().swap_remove(0).swap_remove(0),
                    _ => return Err("A subquery used as a value returned more than one row — add LIMIT 1 or a tighter WHERE".into()),
                }
            }
            Expr::IsNull(a, not) => {
                let v = self.eval(a, sc)?;
                eval::b(v.is_null() != *not)
            }
            Expr::IsBool(a, want, not) => {
                let v = self.eval(a, sc)?;
                let is = eval::truthy(&v) == Some(*want);
                eval::b(is != *not)
            }
            Expr::Between { e, lo, hi, not } => {
                let x = self.eval(e, sc)?;
                let l = self.eval(lo, sc)?;
                let h = self.eval(hi, sc)?;
                match (eval::compare(&x, &l), eval::compare(&x, &h)) {
                    (Some(p), Some(q)) => eval::b((p.is_ge() && q.is_le()) != *not),
                    _ => Json::Null,
                }
            }
            Expr::Case { operand, whens, other } => {
                let op = match operand {
                    Some(o) => Some(self.eval(o, sc)?),
                    None => None,
                };
                for (c, v) in whens {
                    let hit = match &op {
                        Some(x) => eval::compare(x, &self.eval(c, sc)?) == Some(Ordering::Equal),
                        None => self.truthy(c, sc)?,
                    };
                    if hit {
                        return self.eval(v, sc);
                    }
                }
                match other {
                    Some(o) => self.eval(o, sc)?,
                    None => Json::Null,
                }
            }
            Expr::Cast(a, to) => eval::cast(&self.eval(a, sc)?, to),
            Expr::Interval(n, unit) => Json::Arr(vec![self.eval(n, sc)?, Json::Str(unit.clone())]),
            Expr::Row(v) => {
                let mut out = vec![];
                for x in v {
                    out.push(self.eval(x, sc)?);
                }
                Json::Arr(out)
            }
            Expr::Window(..) => match sc.win {
                Some((m, i)) => m.get(&(e as *const Expr as usize)).and_then(|v| v.get(i)).cloned().unwrap_or(Json::Null),
                None => return Err("Window functions (… OVER (…)) can only be used in the SELECT list or ORDER BY".into()),
            },
            Expr::Func(c) => {
                if eval::is_aggregate_name(&c.name) {
                    return self.aggregate(c, sc);
                }
                if matches!(
                    c.name.as_str(),
                    "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "NTILE" | "LAG" | "LEAD" | "FIRST_VALUE" | "LAST_VALUE" | "NTH_VALUE" | "PERCENT_RANK" | "CUME_DIST"
                ) {
                    return Err(format!("{}() needs OVER (…)", c.name));
                }
                let mut args = vec![];
                for a in &c.args {
                    args.push(self.eval(a, sc)?);
                }
                eval::scalar(&c.name, &args, &self.fn_env())?
            }
        })
    }

    fn var(&self, v: &str) -> Json {
        if let Some(x) = self.vars.borrow().get(v) {
            return x.clone();
        }
        match v.trim_start_matches("session.").trim_start_matches("global.") {
            "foreign_key_checks" | "unique_checks" | "autocommit" => json::n(1),
            "version" => json::s("8.0.0-iq-tables"),
            "sql_mode" => json::s("STRICT_TRANS_TABLES"),
            "time_zone" => json::s("SYSTEM"),
            "character_set_client" | "character_set_results" | "character_set_connection" => json::s("utf8mb4"),
            "collation_connection" => json::s("utf8mb4_general_ci"),
            _ => Json::Null,
        }
    }

    fn bin(&self, op: &str, a: &Expr, b: &Expr, sc: &Scope) -> R<Json> {
        match op {
            "AND" => {
                let x = eval::truthy(&self.eval(a, sc)?);
                if x == Some(false) {
                    return Ok(eval::f());
                }
                let y = eval::truthy(&self.eval(b, sc)?);
                Ok(match (x, y) {
                    (_, Some(false)) => eval::f(),
                    (Some(true), Some(true)) => eval::t(),
                    _ => Json::Null,
                })
            }
            "OR" => {
                let x = eval::truthy(&self.eval(a, sc)?);
                if x == Some(true) {
                    return Ok(eval::t());
                }
                let y = eval::truthy(&self.eval(b, sc)?);
                Ok(match (x, y) {
                    (_, Some(true)) => eval::t(),
                    (Some(false), Some(false)) => eval::f(),
                    _ => Json::Null,
                })
            }
            "XOR" => {
                let x = eval::truthy(&self.eval(a, sc)?);
                let y = eval::truthy(&self.eval(b, sc)?);
                Ok(match (x, y) {
                    (Some(p), Some(q)) => eval::b(p != q),
                    _ => Json::Null,
                })
            }
            _ => {
                let x = self.eval(a, sc)?;
                let y = self.eval(b, sc)?;
                match op {
                    "=" | "!=" | "<" | ">" | "<=" | ">=" => Ok(match (&x, &y) {
                        (Json::Arr(p), Json::Arr(q)) if matches!(a, Expr::Row(_)) || matches!(b, Expr::Row(_)) => {
                            if p.len() != q.len() {
                                return Err("Row comparisons need the same number of values on both sides".into());
                            }
                            let mut ord = Some(Ordering::Equal);
                            for (u, v) in p.iter().zip(q) {
                                match eval::compare(u, v) {
                                    None => {
                                        ord = None;
                                        break;
                                    }
                                    Some(Ordering::Equal) => {}
                                    Some(o) => {
                                        ord = Some(o);
                                        break;
                                    }
                                }
                            }
                            match ord {
                                None => Json::Null,
                                Some(o) => eval::b(cmp_op(op, o)),
                            }
                        }
                        _ => match eval::compare(&x, &y) {
                            None => Json::Null,
                            Some(o) => eval::b(cmp_op(op, o)),
                        },
                    }),
                    "<=>" => Ok(eval::b(match (x.is_null(), y.is_null()) {
                        (true, true) => true,
                        (false, false) => eval::compare(&x, &y) == Some(Ordering::Equal),
                        _ => false,
                    })),
                    "+" | "-" => {
                        // date arithmetic: d + INTERVAL n unit
                        if let Expr::Interval(..) = b {
                            if let Json::Arr(v) = &y {
                                return Ok(eval::date_add(&x, &v[0], &eval::text(&v[1]), op == "-"));
                            }
                        }
                        if let (Expr::Interval(..), "+") = (a, op) {
                            if let Json::Arr(v) = &x {
                                return Ok(eval::date_add(&y, &v[0], &eval::text(&v[1]), false));
                            }
                        }
                        eval::arith(op, &x, &y)
                    }
                    _ => eval::arith(op, &x, &y),
                }
            }
        }
    }

    fn aggregate(&self, c: &Call, sc: &Scope) -> R<Json> {
        let Some(g) = sc.group else {
            return Err(format!("{}() adds up rows, so it can't be used in WHERE — use HAVING, or a subquery", c.name));
        };
        let mut vals: Vec<Vec<Json>> = vec![];
        let mut keys: Vec<Vec<Json>> = vec![];
        for &i in g.idx {
            let row = &g.rows[i];
            let inner = Scope { cols: sc.cols, row, group: None, aliases: None, win: None, outer: sc.outer };
            if c.star {
                vals.push(vec![eval::t()]);
                continue;
            }
            let mut v = vec![];
            for a in &c.args {
                v.push(self.eval(a, &inner)?);
            }
            if !c.order.is_empty() {
                let mut k = vec![];
                for o in &c.order {
                    k.push(self.eval(&o.e, &inner)?);
                }
                keys.push(k);
            }
            vals.push(v);
        }
        if !c.order.is_empty() {
            let mut idx: Vec<usize> = (0..vals.len()).collect();
            idx.sort_by(|&x, &y| order_cmp(&keys[x], &keys[y], &c.order));
            vals = idx.into_iter().map(|i| vals[i].clone()).collect();
        }
        if c.name == "COUNT" && !c.star && c.args.len() > 1 && !c.distinct {
            return Err("COUNT(a, b) needs DISTINCT: COUNT(DISTINCT a, b)".into());
        }
        Ok(eval::aggregate(&c.name, vals, c.distinct, c.sep.as_deref().unwrap_or(",")))
    }

    fn subquery(&self, q: &Query, sc: &Scope) -> R<Rc<Rel>> {
        let k = q as *const Query as usize;
        if let Some(r) = self.sub_cache.borrow().get(&k) {
            return Ok(r.clone());
        }
        let before = self.touched_outer.replace(false);
        let r = Rc::new(self.query(q, Some(sc))?);
        let touched = self.touched_outer.get();
        self.touched_outer.set(before || touched);
        if !touched {
            self.sub_cache.borrow_mut().insert(k, r.clone());
        }
        Ok(r)
    }

    // --------------------------------------------------------- queries

    pub fn query(&self, q: &Query, outer: Option<&Scope>) -> R<Rel> {
        let d = self.depth.get();
        if d > 40 {
            return Err("Queries are nested too deeply (a view that uses itself?)".into());
        }
        self.depth.set(d + 1);
        let r = self.query_inner(q, outer);
        self.depth.set(d);
        r
    }

    fn query_inner(&self, q: &Query, outer: Option<&Scope>) -> R<Rel> {
        let pushed = !q.with.is_empty();
        if pushed {
            self.ctes.borrow_mut().push(HashMap::new());
            for c in &q.with {
                let rel = match self.cte(c, q.recursive) {
                    Ok(r) => r,
                    Err(e) => {
                        self.ctes.borrow_mut().pop();
                        return Err(e);
                    }
                };
                self.ctes.borrow_mut().last_mut().unwrap().insert(c.name.to_lowercase(), Rc::new(rel));
            }
        }
        let r = self.query_body(q, outer);
        if pushed {
            self.ctes.borrow_mut().pop();
        }
        r
    }

    fn cte(&self, c: &Cte, recursive: bool) -> R<Rel> {
        let rename = |mut r: Rel| -> R<Rel> {
            if !c.cols.is_empty() {
                if c.cols.len() != r.names().len() {
                    return Err(format!("{} lists {} columns but its query returns {}", c.name, c.cols.len(), r.names().len()));
                }
                let mut k = 0;
                for col in r.cols.iter_mut().filter(|x| !x.hidden) {
                    col.name = c.cols[k].clone();
                    k += 1;
                }
            }
            for col in r.cols.iter_mut() {
                col.table = None;
            }
            Ok(r)
        };
        let self_ref = recursive && refers_to(&c.q, &c.name);
        if !self_ref {
            return rename(self.query(&c.q, None)?);
        }
        let Body::Set(op @ (SetOp::Union | SetOp::UnionAll), anchor, rec) = &c.q.body else {
            return Err(format!("A recursive CTE is written anchor UNION [ALL] recursive part ({})", c.name));
        };
        let base = rename(self.body(anchor, None)?)?;
        let cols = base.cols.clone();
        let mut all = base.rows.clone();
        let mut seen: std::collections::HashSet<String> = all.iter().map(|r| row_key(r)).collect();
        let mut work = base.rows;
        for round in 0.. {
            if round > 1000 || all.len() > 200_000 {
                return Err(format!("{} kept growing (over 1,000 rounds or 200,000 rows) — check its stop condition", c.name));
            }
            self.ctes.borrow_mut().last_mut().unwrap().insert(c.name.to_lowercase(), Rc::new(Rel { cols: cols.clone(), rows: work.clone() }));
            self.sub_cache.borrow_mut().clear();
            let next = self.body(rec, None)?;
            if next.names().len() != cols.len() {
                return Err(format!("Both halves of {} must return the same number of columns", c.name));
            }
            let mut fresh = vec![];
            for r in next.visible_rows() {
                if *op == SetOp::Union && !seen.insert(row_key(&r)) {
                    continue;
                }
                fresh.push(r);
            }
            if fresh.is_empty() {
                break;
            }
            all.extend(fresh.iter().cloned());
            work = fresh;
        }
        self.sub_cache.borrow_mut().clear();
        Ok(Rel { cols, rows: all })
    }

    fn query_body(&self, q: &Query, outer: Option<&Scope>) -> R<Rel> {
        if let Body::Select(s) = &q.body {
            return self.select(s, &q.order, q.limit.as_ref(), q.offset.as_ref(), outer);
        }
        let mut r = self.body(&q.body, outer)?;
        if !q.order.is_empty() {
            let names = r.names();
            let mut keyed: Vec<(Vec<Json>, Vec<Json>)> = vec![];
            for row in r.rows.drain(..) {
                let sc = Scope::row(&r.cols, &row, outer);
                let mut k = vec![];
                for o in &q.order {
                    k.push(match &o.e {
                        Expr::Lit(Json::Num(n)) => {
                            let p: usize = n.parse().unwrap_or(0);
                            if p == 0 || p > names.len() {
                                return Err(format!("ORDER BY {} is out of range", n));
                            }
                            row[p - 1].clone()
                        }
                        e => self.eval(e, &sc)?,
                    });
                }
                keyed.push((k, row));
            }
            keyed.sort_by(|a, b| order_cmp(&a.0, &b.0, &q.order));
            r.rows = keyed.into_iter().map(|x| x.1).collect();
        }
        self.limit(&mut r.rows, q.limit.as_ref(), q.offset.as_ref())?;
        Ok(r)
    }

    fn limit(&self, rows: &mut Vec<Vec<Json>>, limit: Option<&Expr>, offset: Option<&Expr>) -> R<()> {
        let n = |e: Option<&Expr>| -> R<Option<usize>> {
            match e {
                None => Ok(None),
                Some(e) => {
                    let v = self.eval_const(e)?;
                    match eval::num(&v) {
                        Some(f) if f >= 0.0 => Ok(Some(f as usize)),
                        _ => Err("LIMIT and OFFSET take whole numbers".into()),
                    }
                }
            }
        };
        let off = n(offset)?.unwrap_or(0);
        self.found_rows.set(rows.len() as u64);
        if off > 0 {
            if off >= rows.len() {
                rows.clear();
            } else {
                rows.drain(..off);
            }
        }
        if let Some(l) = n(limit)? {
            rows.truncate(l);
        }
        Ok(())
    }

    fn body(&self, b: &Body, outer: Option<&Scope>) -> R<Rel> {
        match b {
            Body::Select(s) => self.select(s, &[], None, None, outer),
            Body::Paren(q) => self.query(q, outer),
            Body::Values(rows) => {
                let width = rows.first().map(|r| r.len()).unwrap_or(0);
                let mut out = vec![];
                for r in rows {
                    if r.len() != width {
                        return Err("Every VALUES row needs the same number of values".into());
                    }
                    let mut v = vec![];
                    for e in r {
                        v.push(self.eval(e, &outer.copied().unwrap_or_else(empty_scope))?);
                    }
                    out.push(v);
                }
                Ok(Rel { cols: (0..width).map(|i| Col::new(None, &format!("column_{}", i))).collect(), rows: out })
            }
            Body::Set(op, l, r) => {
                let a = self.body(l, outer)?;
                let b = self.body(r, outer)?;
                let (na, nb) = (a.names(), b.names());
                if na.len() != nb.len() {
                    return Err(format!("The two sides of {} return different numbers of columns ({} and {})", setop_name(*op), na.len(), nb.len()));
                }
                let cols: Vec<Col> = na.iter().map(|n| Col::new(None, n)).collect();
                let (ra, rb) = (a.visible_rows(), b.visible_rows());
                let rows = match op {
                    SetOp::UnionAll => ra.into_iter().chain(rb).collect(),
                    SetOp::Union => {
                        let mut seen = std::collections::HashSet::new();
                        ra.into_iter().chain(rb).filter(|r| seen.insert(row_key(r))).collect()
                    }
                    SetOp::Intersect => {
                        let right: std::collections::HashSet<String> = rb.iter().map(|r| row_key(r)).collect();
                        let mut seen = std::collections::HashSet::new();
                        ra.into_iter().filter(|r| right.contains(&row_key(r)) && seen.insert(row_key(r))).collect()
                    }
                    SetOp::Except => {
                        let right: std::collections::HashSet<String> = rb.iter().map(|r| row_key(r)).collect();
                        let mut seen = std::collections::HashSet::new();
                        ra.into_iter().filter(|r| !right.contains(&row_key(r)) && seen.insert(row_key(r))).collect()
                    }
                };
                Ok(Rel { cols, rows })
            }
        }
    }

    // ------------------------------------------------------------ FROM

    fn table_rel(&self, name: &str, alias: Option<&str>) -> R<Rel> {
        let q = alias.unwrap_or(name);
        let mut rel = None;
        for frame in self.ctes.borrow().iter().rev() {
            if let Some(r) = frame.get(&name.to_lowercase()) {
                rel = Some((**r).clone());
                break;
            }
        }
        if rel.is_none() {
            if let Some(sql) = self.cat.view(name) {
                let vq = super::parse::parse_query(&sql).map_err(|e| format!("View {} is broken: {}", name, e))?;
                rel = Some(self.query(&vq, None)?);
            }
        }
        let mut rel = match rel {
            Some(r) => r,
            None => match self.cat.table(name) {
                Some(r) => (*r).clone(),
                None => return Err(format!("No table `{}`", name)),
            },
        };
        for c in rel.cols.iter_mut() {
            c.table = Some(q.to_string());
        }
        if self.rid_tables.borrow().iter().any(|t| t.eq_ignore_ascii_case(q)) {
            rel.cols.push(Col { table: Some(q.to_string()), name: RID.into(), hidden: true });
            for (i, r) in rel.rows.iter_mut().enumerate() {
                r.push(json::n(i));
            }
        }
        Ok(rel)
    }

    pub fn from(&self, f: &From, outer: Option<&Scope>) -> R<Rel> {
        match f {
            From::Table { name, alias } => self.table_rel(name, alias.as_deref()),
            From::Sub { q, alias } => {
                let _ = outer;
                let mut r = self.query(q, None)?;
                for c in r.cols.iter_mut() {
                    c.table = Some(alias.clone());
                }
                Ok(r)
            }
            From::Join { left, right, kind, on, using, natural } => {
                let l = self.from(left, outer)?;
                let r = self.from(right, outer)?;
                let mut using = using.clone();
                if *natural {
                    let ln: Vec<String> = l.cols.iter().filter(|c| !c.hidden).map(|c| c.name.to_lowercase()).collect();
                    using = r.cols.iter().filter(|c| !c.hidden && ln.contains(&c.name.to_lowercase())).map(|c| c.name.clone()).collect();
                }
                self.join(l, r, *kind, on.as_ref(), &using, outer)
            }
        }
    }

    fn join(&self, l: Rel, r: Rel, kind: JoinKind, on: Option<&Expr>, using: &[String], outer: Option<&Scope>) -> R<Rel> {
        let mut cols = l.cols.clone();
        let mut rc = r.cols.clone();
        let mut using_pairs: Vec<(usize, usize)> = vec![];
        for u in using {
            let li = l
                .cols
                .iter()
                .position(|c| !c.hidden && c.name.eq_ignore_ascii_case(u))
                .ok_or_else(|| format!("USING ({}): the left side has no such column", u))?;
            let ri = r
                .cols
                .iter()
                .position(|c| !c.hidden && c.name.eq_ignore_ascii_case(u))
                .ok_or_else(|| format!("USING ({}): the right side has no such column", u))?;
            rc[ri].hidden = true;
            using_pairs.push((li, ri));
        }
        cols.extend(rc);
        let lw = l.cols.len();
        let rw = r.cols.len();
        // equi-join keys from ON a.x = b.y (plus USING)
        let mut keys: Vec<(usize, usize)> = using_pairs.clone();
        let mut rest: Vec<&Expr> = vec![];
        if let Some(on) = on {
            let mut conj = vec![];
            conjuncts(on, &mut conj);
            for c in conj {
                if let Expr::Bin("=", a, b) = c {
                    if let (Expr::Col(ta, na), Expr::Col(tb, nb)) = (&**a, &**b) {
                        let la = self.find(&l.cols, ta.as_deref(), na).ok().flatten();
                        let rb = self.find(&r.cols, tb.as_deref(), nb).ok().flatten();
                        let lb = self.find(&l.cols, tb.as_deref(), nb).ok().flatten();
                        let ra = self.find(&r.cols, ta.as_deref(), na).ok().flatten();
                        let ambiguous = |x: Option<usize>, y: Option<usize>| x.is_some() && y.is_some();
                        if !ambiguous(la, ra) && !ambiguous(lb, rb) {
                            if let (Some(x), Some(y)) = (la, rb) {
                                keys.push((x, y));
                                continue;
                            }
                            if let (Some(x), Some(y)) = (lb, ra) {
                                keys.push((x, y));
                                continue;
                            }
                        }
                    }
                }
                rest.push(c);
            }
        }
        let check = |row: &[Json]| -> R<bool> {
            for c in &rest {
                let sc = Scope::row(&cols, row, outer);
                if !self.truthy(c, &sc)? {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        let nulls_l = vec![Json::Null; lw];
        let nulls_r = vec![Json::Null; rw];
        let mut out = vec![];
        let right_first = kind == JoinKind::Right;
        let (outer_rows, inner_rows) = if right_first { (&r.rows, &l.rows) } else { (&l.rows, &r.rows) };
        let key_of = |row: &[Json], side_left: bool| -> Option<String> {
            let mut k = String::new();
            for (a, b) in &keys {
                let v = &row[if side_left { *a } else { *b }];
                if v.is_null() {
                    return None;
                }
                k.push_str(&eval::key(v));
                k.push('\u{1}');
            }
            Some(k)
        };
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();
        if !keys.is_empty() {
            for (i, row) in inner_rows.iter().enumerate() {
                if let Some(k) = key_of(row, right_first) {
                    index.entry(k).or_default().push(i);
                }
            }
        }
        let all: Vec<usize> = (0..inner_rows.len()).collect();
        for orow in outer_rows {
            let cands: &[usize] = if keys.is_empty() {
                &all
            } else {
                match key_of(orow, !right_first) {
                    Some(k) => index.get(&k).map(|v| v.as_slice()).unwrap_or(&[]),
                    None => &[],
                }
            };
            let mut matched = false;
            for &i in cands {
                let irow = &inner_rows[i];
                let mut row = Vec::with_capacity(lw + rw);
                if right_first {
                    row.extend_from_slice(irow);
                    row.extend_from_slice(orow);
                } else {
                    row.extend_from_slice(orow);
                    row.extend_from_slice(irow);
                }
                if check(&row)? {
                    matched = true;
                    out.push(row);
                }
            }
            if !matched && (kind == JoinKind::Left || kind == JoinKind::Right) {
                let mut row = Vec::with_capacity(lw + rw);
                if right_first {
                    row.extend_from_slice(&nulls_l);
                    row.extend_from_slice(orow);
                } else {
                    row.extend_from_slice(orow);
                    row.extend_from_slice(&nulls_r);
                }
                out.push(row);
            }
            if out.len() > 2_000_000 {
                return Err("That join produces over 2 million rows — add an ON condition".into());
            }
        }
        // USING columns show the non-null side (matters for RIGHT/LEFT joins)
        if kind == JoinKind::Right {
            for (li, ri) in &using_pairs {
                for row in out.iter_mut() {
                    if row[*li].is_null() {
                        row[*li] = row[lw + ri].clone();
                    }
                }
            }
        }
        Ok(Rel { cols, rows: out })
    }

    // ---------------------------------------------------------- SELECT

    fn select(&self, s: &Select, order: &[Order], limit: Option<&Expr>, offset: Option<&Expr>, outer: Option<&Scope>) -> R<Rel> {
        let src = match &s.from {
            Some(f) => self.from(f, outer)?,
            None => Rel { cols: vec![], rows: vec![vec![]] },
        };
        // WHERE
        let mut rows: Vec<Vec<Json>> = Vec::with_capacity(src.rows.len());
        match &s.filter {
            Some(w) => {
                for row in &src.rows {
                    let sc = Scope::row(&src.cols, row, outer);
                    if self.truthy(w, &sc)? {
                        rows.push(row.clone());
                    }
                }
            }
            None => rows = src.rows.clone(),
        }
        let cols = &src.cols;
        // output columns
        let mut labels: Vec<String> = vec![];
        let mut plan: Vec<Result<usize, &Expr>> = vec![]; // Ok(source column) for stars
        for it in &s.items {
            match it {
                SelItem::Star(t) => {
                    let mut any = false;
                    for (i, c) in cols.iter().enumerate() {
                        if c.hidden {
                            continue;
                        }
                        if let Some(t) = t {
                            if !c.table.as_deref().map(|x| x.eq_ignore_ascii_case(t)).unwrap_or(false) {
                                continue;
                            }
                        }
                        any = true;
                        labels.push(c.name.clone());
                        plan.push(Ok(i));
                    }
                    if let (Some(t), false) = (t, any) {
                        return Err(format!("Unknown table `{}` in {}.*", t, t));
                    }
                }
                SelItem::Expr(e, alias, label) => {
                    labels.push(alias.clone().unwrap_or_else(|| label.clone()));
                    plan.push(Err(e));
                }
            }
        }
        let exprs: Vec<&Expr> = plan.iter().filter_map(|p| p.as_ref().err().copied()).collect();
        let grouped =
            !s.group.is_empty() || exprs.iter().any(|e| has_agg(e)) || s.having.as_ref().map(has_agg).unwrap_or(false) || order.iter().any(|o| has_agg(&o.e));
        // candidates: (representative row, member rows)
        let mut cands: Vec<(Vec<Json>, Vec<usize>)> = vec![];
        if grouped {
            let group_exprs: Vec<Expr> = s.group.iter().map(|g| resolve_group_expr(g, &s.items)).collect::<R<_>>()?;
            let mut index: HashMap<String, usize> = HashMap::new();
            let mut gkeys: Vec<Vec<Json>> = vec![];
            for (i, row) in rows.iter().enumerate() {
                let sc = Scope::row(cols, row, outer);
                let mut kv = vec![];
                let mut k = String::new();
                for g in &group_exprs {
                    let v = self.eval(g, &sc)?;
                    k.push_str(&eval::key(&v));
                    k.push('\u{1}');
                    kv.push(v);
                }
                match index.get(&k) {
                    Some(&p) => cands[p].1.push(i),
                    None => {
                        index.insert(k, cands.len());
                        cands.push((row.clone(), vec![i]));
                        gkeys.push(kv);
                    }
                }
            }
            if cands.is_empty() && s.group.is_empty() {
                cands.push((vec![Json::Null; cols.len()], vec![]));
                gkeys.push(vec![]);
            }
            // like MySQL 5.x: groups come out sorted by the GROUP BY values
            if !s.group.is_empty() {
                let mut idx: Vec<usize> = (0..cands.len()).collect();
                let asc: Vec<Order> = group_exprs.iter().map(|e| Order { e: e.clone(), desc: false }).collect();
                idx.sort_by(|&a, &b| order_cmp(&gkeys[a], &gkeys[b], &asc));
                cands = idx.iter().map(|&i| cands[i].clone()).collect();
                gkeys = idx.iter().map(|&i| gkeys[i].clone()).collect();
            }
            if s.rollup && !s.group.is_empty() {
                cands = self.rollup(cands, &gkeys, &group_exprs, cols)?;
            }
        } else {
            for (i, row) in rows.iter().enumerate() {
                cands.push((row.clone(), vec![i]));
            }
        }
        // first pass: values without window functions (for HAVING)
        let has_win = exprs.iter().any(|e| has_window(e)) || order.iter().any(|o| has_window(&o.e));
        let project = |cand: &(Vec<Json>, Vec<usize>), win: Option<(&HashMap<usize, Vec<Json>>, usize)>| -> R<Vec<Json>> {
            let g = Group { rows: &rows, idx: &cand.1 };
            let sc = Scope { cols, row: &cand.0, group: if grouped { Some(&g) } else { None }, aliases: None, win, outer };
            let mut out = vec![];
            for p in &plan {
                out.push(match p {
                    Ok(i) => cand.0.get(*i).cloned().unwrap_or(Json::Null),
                    Err(e) => {
                        if win.is_none() && has_window(e) {
                            Json::Null
                        } else {
                            self.eval(e, &sc)?
                        }
                    }
                });
            }
            Ok(out)
        };
        let mut projected: Vec<Vec<Json>> = vec![];
        let mut kept: Vec<(Vec<Json>, Vec<usize>)> = vec![];
        for cand in cands {
            let vals = project(&cand, None)?;
            if let Some(h) = &s.having {
                let g = Group { rows: &rows, idx: &cand.1 };
                let sc = Scope { cols, row: &cand.0, group: if grouped { Some(&g) } else { None }, aliases: Some((&labels, &vals)), win: None, outer };
                if !self.truthy(h, &sc)? {
                    continue;
                }
            }
            projected.push(vals);
            kept.push(cand);
        }
        // window functions over what's left
        let mut win: HashMap<usize, Vec<Json>> = HashMap::new();
        if has_win {
            let mut wexprs: Vec<&Expr> = vec![];
            for e in &exprs {
                collect_windows(e, &mut wexprs);
            }
            for o in order {
                collect_windows(&o.e, &mut wexprs);
            }
            for w in wexprs {
                let vals = self.window(w, &kept, &rows, cols, grouped, outer)?;
                win.insert(w as *const Expr as usize, vals);
            }
            for (i, cand) in kept.iter().enumerate() {
                projected[i] = project(cand, Some((&win, i)))?;
            }
        }
        // ORDER BY
        let mut idx: Vec<usize> = (0..kept.len()).collect();
        if !order.is_empty() {
            let mut keys: Vec<Vec<Json>> = vec![];
            for (i, cand) in kept.iter().enumerate() {
                let g = Group { rows: &rows, idx: &cand.1 };
                let sc = Scope {
                    cols,
                    row: &cand.0,
                    group: if grouped { Some(&g) } else { None },
                    aliases: Some((&labels, &projected[i])),
                    win: Some((&win, i)),
                    outer,
                };
                let mut k = vec![];
                for o in order {
                    k.push(match &o.e {
                        Expr::Lit(Json::Num(n)) => {
                            let p: usize = n.parse().unwrap_or(0);
                            if p == 0 || p > labels.len() {
                                return Err(format!("ORDER BY {} is out of range (there are {} columns)", n, labels.len()));
                            }
                            projected[i][p - 1].clone()
                        }
                        e => self.eval(e, &sc)?,
                    });
                }
                keys.push(k);
            }
            idx.sort_by(|&a, &b| order_cmp(&keys[a], &keys[b], order));
        }
        let mut out: Vec<Vec<Json>> = idx.into_iter().map(|i| std::mem::take(&mut projected[i])).collect();
        if s.distinct {
            let mut seen = std::collections::HashSet::new();
            out.retain(|r| seen.insert(row_key(r)));
        }
        self.limit(&mut out, limit, offset)?;
        Ok(Rel { cols: labels.iter().map(|l| Col::new(None, l)).collect(), rows: out })
    }

    fn rollup(&self, cands: Vec<(Vec<Json>, Vec<usize>)>, gkeys: &[Vec<Json>], group_exprs: &[Expr], cols: &[Col]) -> R<Vec<(Vec<Json>, Vec<usize>)>> {
        let n = group_exprs.len();
        let simple: Vec<Option<usize>> =
            group_exprs.iter().map(|g| if let Expr::Col(t, c) = g { self.find(cols, t.as_deref(), c).ok().flatten() } else { None }).collect();
        let mut out = vec![];
        let blank = |rep: &[Json], level: usize| -> Vec<Json> {
            let mut r = rep.to_vec();
            for s in simple.iter().skip(level).flatten() {
                r[*s] = Json::Null;
            }
            r
        };
        // accumulators per prefix length
        let mut acc: Vec<Vec<usize>> = vec![vec![]; n];
        let mut reps: Vec<Vec<Json>> = vec![vec![]; n];
        for (i, c) in cands.iter().enumerate() {
            out.push(c.clone());
            for level in 0..n {
                if acc[level].is_empty() {
                    reps[level] = c.0.clone();
                }
                acc[level].extend(c.1.iter().copied());
            }
            // close the levels whose prefix changes next
            let next = gkeys.get(i + 1);
            for level in (0..n).rev() {
                let changes = match next {
                    None => true,
                    Some(nk) => (0..level).any(|k| eval::key(&gkeys[i][k]) != eval::key(&nk[k])),
                };
                if changes && level < n {
                    out.push((blank(&reps[level], level), std::mem::take(&mut acc[level])));
                }
            }
        }
        Ok(out)
    }

    fn window(&self, w: &Expr, cands: &[(Vec<Json>, Vec<usize>)], rows: &[Vec<Json>], cols: &[Col], grouped: bool, outer: Option<&Scope>) -> R<Vec<Json>> {
        let Expr::Window(call, over) = w else { return Ok(vec![]) };
        let n = cands.len();
        let mut part_key = vec![String::new(); n];
        let mut okeys: Vec<Vec<Json>> = vec![vec![]; n];
        for i in 0..n {
            let g = Group { rows, idx: &cands[i].1 };
            let sc = Scope { cols, row: &cands[i].0, group: if grouped { Some(&g) } else { None }, aliases: None, win: None, outer };
            for p in &over.partition {
                part_key[i].push_str(&eval::key(&self.eval(p, &sc)?));
                part_key[i].push('\u{1}');
            }
            for o in &over.order {
                okeys[i].push(self.eval(&o.e, &sc)?);
            }
        }
        let mut parts: Vec<Vec<usize>> = vec![];
        let mut pidx: HashMap<String, usize> = HashMap::new();
        for i in 0..n {
            match pidx.get(&part_key[i]) {
                Some(&p) => parts[p].push(i),
                None => {
                    pidx.insert(part_key[i].clone(), parts.len());
                    parts.push(vec![i]);
                }
            }
        }
        let mut out = vec![Json::Null; n];
        let arg = |i: usize, k: usize| -> R<Json> {
            let g = Group { rows, idx: &cands[i].1 };
            let sc = Scope { cols, row: &cands[i].0, group: if grouped { Some(&g) } else { None }, aliases: None, win: None, outer };
            match call.args.get(k) {
                Some(e) => self.eval(e, &sc),
                None => Ok(Json::Null),
            }
        };
        for mut p in parts {
            if !over.order.is_empty() {
                p.sort_by(|&a, &b| order_cmp(&okeys[a], &okeys[b], &over.order));
            }
            let m = p.len();
            let peer = |a: usize, b: usize| over.order.is_empty() || order_cmp(&okeys[p[a]], &okeys[p[b]], &over.order) == Ordering::Equal;
            // last index of each row's peer group
            let mut last_peer = vec![0; m];
            let mut j = m;
            while j > 0 {
                j -= 1;
                last_peer[j] = if j + 1 < m && peer(j, j + 1) { last_peer[j + 1] } else { j };
            }
            let running = !over.order.is_empty() && over.whole != Some(true);
            match call.name.as_str() {
                "ROW_NUMBER" => {
                    for (k, &i) in p.iter().enumerate() {
                        out[i] = json::n(k + 1);
                    }
                }
                "RANK" | "DENSE_RANK" | "PERCENT_RANK" | "CUME_DIST" => {
                    let mut rank = 1;
                    let mut dense = 1;
                    for k in 0..m {
                        if k > 0 && !peer(k - 1, k) {
                            rank = k + 1;
                            dense += 1;
                        }
                        out[p[k]] = match call.name.as_str() {
                            "RANK" => json::n(rank),
                            "DENSE_RANK" => json::n(dense),
                            "PERCENT_RANK" => eval::fmt_num(if m > 1 { (rank - 1) as f64 / (m - 1) as f64 } else { 0.0 }),
                            _ => eval::fmt_num((last_peer[k] + 1) as f64 / m as f64),
                        };
                    }
                }
                "NTILE" => {
                    let b = eval::num(&arg(p[0], 0)?).unwrap_or(1.0).max(1.0) as usize;
                    let (q, r) = (m / b, m % b);
                    let mut k = 0;
                    for bucket in 0..b {
                        let size = q + if bucket < r { 1 } else { 0 };
                        for _ in 0..size {
                            if k < m {
                                out[p[k]] = json::n(bucket + 1);
                                k += 1;
                            }
                        }
                    }
                }
                "LAG" | "LEAD" => {
                    for k in 0..m {
                        let off = match call.args.get(1) {
                            Some(_) => eval::num(&arg(p[k], 1)?).unwrap_or(1.0) as i64,
                            None => 1,
                        };
                        let t = if call.name == "LAG" { k as i64 - off } else { k as i64 + off };
                        out[p[k]] = if t >= 0 && (t as usize) < m { arg(p[t as usize], 0)? } else { arg(p[k], 2)? };
                    }
                }
                "FIRST_VALUE" | "LAST_VALUE" | "NTH_VALUE" => {
                    for k in 0..m {
                        let end = if running { last_peer[k] } else { m - 1 };
                        out[p[k]] = match call.name.as_str() {
                            "FIRST_VALUE" => arg(p[0], 0)?,
                            "LAST_VALUE" => arg(p[end], 0)?,
                            _ => {
                                let nth = eval::num(&arg(p[k], 1)?).unwrap_or(1.0) as usize;
                                if nth >= 1 && nth - 1 <= end {
                                    arg(p[nth - 1], 0)?
                                } else {
                                    Json::Null
                                }
                            }
                        };
                    }
                }
                name if eval::is_aggregate_name(name) => {
                    let mut vals: Vec<Vec<Json>> = vec![];
                    for &i in &p {
                        if call.star {
                            vals.push(vec![eval::t()]);
                        } else {
                            let mut v = vec![];
                            for k in 0..call.args.len() {
                                v.push(arg(i, k)?);
                            }
                            vals.push(v);
                        }
                    }
                    if running {
                        let mut k = 0;
                        while k < m {
                            let end = last_peer[k];
                            let v = eval::aggregate(name, vals[..=end].to_vec(), call.distinct, call.sep.as_deref().unwrap_or(","));
                            for x in k..=end {
                                out[p[x]] = v.clone();
                            }
                            k = end + 1;
                        }
                    } else {
                        let v = eval::aggregate(name, vals, call.distinct, call.sep.as_deref().unwrap_or(","));
                        for &i in &p {
                            out[i] = v.clone();
                        }
                    }
                }
                other => return Err(format!("{}() can't be used as a window function", other)),
            }
        }
        Ok(out)
    }
}

// ------------------------------------------------------------- helpers

fn dedup(v: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for x in v {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}

fn cmp_op(op: &str, o: Ordering) -> bool {
    match op {
        "=" => o.is_eq(),
        "!=" => o.is_ne(),
        "<" => o.is_lt(),
        ">" => o.is_gt(),
        "<=" => o.is_le(),
        _ => o.is_ge(),
    }
}

fn in_list(x: &Json, vals: &[Json], not: bool) -> Json {
    if x.is_null() {
        return Json::Null;
    }
    let mut saw_null = false;
    for v in vals {
        let eq = match (x, v) {
            (Json::Arr(a), Json::Arr(b)) => {
                if a.len() != b.len() {
                    Some(false)
                } else {
                    let mut r = Some(true);
                    for (p, q) in a.iter().zip(b) {
                        match eval::compare(p, q) {
                            None => r = None,
                            Some(o) if !o.is_eq() => {
                                r = Some(false);
                                break;
                            }
                            _ => {}
                        }
                    }
                    r
                }
            }
            _ => eval::compare(x, v).map(|o| o.is_eq()),
        };
        match eq {
            Some(true) => return eval::b(!not),
            None => saw_null = true,
            _ => {}
        }
    }
    if saw_null {
        Json::Null
    } else {
        eval::b(not)
    }
}

pub fn row_key(r: &[Json]) -> String {
    r.iter().map(eval::key).collect::<Vec<_>>().join("\u{1}")
}

/// ORDER BY comparison: NULLs first ascending (MySQL), last descending.
pub fn order_cmp(a: &[Json], b: &[Json], order: &[Order]) -> Ordering {
    for (i, o) in order.iter().enumerate() {
        let (x, y) = (&a[i], &b[i]);
        let c = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => eval::compare(x, y).unwrap_or(Ordering::Equal),
        };
        let c = if o.desc { c.reverse() } else { c };
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::Bin("AND", a, b) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        other => out.push(other),
    }
}

fn setop_name(op: SetOp) -> &'static str {
    match op {
        SetOp::Union | SetOp::UnionAll => "UNION",
        SetOp::Intersect => "INTERSECT",
        SetOp::Except => "EXCEPT",
    }
}

/// GROUP BY 2 / GROUP BY alias → the SELECT item's expression.
fn resolve_group_expr(g: &Expr, items: &[SelItem]) -> R<Expr> {
    if let Expr::Lit(Json::Num(n)) = g {
        let p: usize = n.parse().unwrap_or(0);
        return match items.get(p.wrapping_sub(1)) {
            Some(SelItem::Expr(e, _, _)) => Ok(e.clone()),
            _ => Err(format!("GROUP BY {} doesn't match a SELECT column", n)),
        };
    }
    if let Expr::Col(None, name) = g {
        for it in items {
            if let SelItem::Expr(e, Some(a), _) = it {
                if a.eq_ignore_ascii_case(name) && !matches!(e, Expr::Col(_, c) if c.eq_ignore_ascii_case(name)) {
                    return Ok(e.clone());
                }
            }
        }
    }
    Ok(g.clone())
}

/// Visit an expression tree (not descending into subqueries).
pub fn walk<'e>(e: &'e Expr, f: &mut dyn FnMut(&'e Expr) -> bool) {
    if !f(e) {
        return;
    }
    match e {
        Expr::Unary(_, a) | Expr::IsNull(a, _) | Expr::IsBool(a, _, _) | Expr::Cast(a, _) | Expr::Interval(a, _) => walk(a, f),
        Expr::Bin(_, a, b) => {
            walk(a, f);
            walk(b, f);
        }
        Expr::Like { e, pat, esc, .. } => {
            walk(e, f);
            walk(pat, f);
            if let Some(x) = esc {
                walk(x, f);
            }
        }
        Expr::Regexp { e, pat, .. } => {
            walk(e, f);
            walk(pat, f);
        }
        Expr::In { e, list, .. } => {
            walk(e, f);
            for x in list {
                walk(x, f);
            }
        }
        Expr::InQuery { e, .. } | Expr::Quantified { e, .. } => walk(e, f),
        Expr::Between { e, lo, hi, .. } => {
            walk(e, f);
            walk(lo, f);
            walk(hi, f);
        }
        Expr::Case { operand, whens, other } => {
            if let Some(o) = operand {
                walk(o, f);
            }
            for (a, b) in whens {
                walk(a, f);
                walk(b, f);
            }
            if let Some(o) = other {
                walk(o, f);
            }
        }
        Expr::Func(c) => {
            for a in &c.args {
                walk(a, f);
            }
        }
        Expr::Window(c, o) => {
            for a in &c.args {
                walk(a, f);
            }
            for p in &o.partition {
                walk(p, f);
            }
            for x in &o.order {
                walk(&x.e, f);
            }
        }
        Expr::Row(v) => {
            for x in v {
                walk(x, f);
            }
        }
        _ => {}
    }
}

pub fn has_agg(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| {
        match x {
            Expr::Func(c) if eval::is_aggregate_name(&c.name) => {
                found = true;
                false
            }
            // aggregates inside a window's own call belong to the window
            Expr::Window(c, o) => {
                for a in &c.args {
                    if has_agg(a) {
                        found = true;
                    }
                }
                for p in &o.partition {
                    if has_agg(p) {
                        found = true;
                    }
                }
                for q in &o.order {
                    if has_agg(&q.e) {
                        found = true;
                    }
                }
                false
            }
            _ => true,
        }
    });
    found
}

fn has_window(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| {
        if matches!(x, Expr::Window(..)) {
            found = true;
        }
        !found
    });
    found
}

fn collect_windows<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    walk(e, &mut |x| {
        if matches!(x, Expr::Window(..)) {
            out.push(x);
            false
        } else {
            true
        }
    });
}

/// Does the query mention table `name` anywhere (for recursive CTEs)?
fn refers_to(q: &Query, name: &str) -> bool {
    tables_in_query(q).iter().any(|t| t.eq_ignore_ascii_case(name))
}

/// Every table name a query reads (tables, views, CTE names), including subqueries.
pub fn tables_in_query(q: &Query) -> Vec<String> {
    let mut out = vec![];
    for c in &q.with {
        out.extend(tables_in_query(&c.q));
    }
    tables_in_body(&q.body, &mut out);
    for o in &q.order {
        tables_in_expr(&o.e, &mut out);
    }
    out
}

fn tables_in_body(b: &Body, out: &mut Vec<String>) {
    match b {
        Body::Select(s) => {
            if let Some(f) = &s.from {
                tables_in_from(f, out);
            }
            for it in &s.items {
                if let SelItem::Expr(e, _, _) = it {
                    tables_in_expr(e, out);
                }
            }
            for e in s.filter.iter().chain(s.having.iter()).chain(s.group.iter()) {
                tables_in_expr(e, out);
            }
        }
        Body::Set(_, a, b) => {
            tables_in_body(a, out);
            tables_in_body(b, out);
        }
        Body::Paren(q) => out.extend(tables_in_query(q)),
        Body::Values(rows) => {
            for r in rows {
                for e in r {
                    tables_in_expr(e, out);
                }
            }
        }
    }
}

pub fn tables_in_from(f: &From, out: &mut Vec<String>) {
    match f {
        From::Table { name, .. } => out.push(name.clone()),
        From::Sub { q, .. } => out.extend(tables_in_query(q)),
        From::Join { left, right, on, .. } => {
            tables_in_from(left, out);
            tables_in_from(right, out);
            if let Some(e) = on {
                tables_in_expr(e, out);
            }
        }
    }
}

pub fn tables_in_expr(e: &Expr, out: &mut Vec<String>) {
    walk(e, &mut |x| {
        match x {
            Expr::InQuery { q, .. } | Expr::Quantified { q, .. } | Expr::Exists(q, _) | Expr::Scalar(q) => out.extend(tables_in_query(q)),
            _ => {}
        }
        true
    });
}
