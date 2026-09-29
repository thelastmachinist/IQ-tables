//! Recursive-descent parser for the MySQL dialect.

use super::ast::*;
use super::lex::{show, tokenize, Tok, Token};
use crate::json::Json;
use crate::schema::{RefAction, Ty, TypeArg};

pub struct Parser<'s> {
    src: &'s str,
    t: Vec<Token>,
    i: usize,
}

const RESERVED: &[&str] = &[
    "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "ORDER", "BY", "GROUP", "LIMIT", "OFFSET", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "AS", "IS", "NULL", "LIKE", "IN", "BETWEEN", "DISTINCT", "TRUE", "FALSE", "CREATE", "TABLE", "ALTER", "DROP", "HAVING", "JOIN",
    "INNER", "LEFT", "RIGHT", "CROSS", "OUTER", "ON", "USING", "UNION", "INTERSECT", "EXCEPT", "CASE", "WHEN", "THEN", "ELSE", "END", "EXISTS", "WITH", "NATURAL", "WINDOW", "INTERVAL", "REGEXP", "RLIKE", "DIV", "XOR", "ALL", "STRAIGHT_JOIN", "FOR", "ASC", "DESC", "ESCAPE",
    "LOCK", "OVER", "PARTITION", "ROLLUP", "DUAL",
];

pub fn reserved(w: &str) -> bool {
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(w))
}

const UNITS: &[&str] = &["MICROSECOND", "SECOND", "MINUTE", "HOUR", "DAY", "WEEK", "MONTH", "QUARTER", "YEAR"];

fn unit_of(w: &str) -> Option<String> {
    let u = w.to_ascii_uppercase();
    let u = u.strip_suffix('S').filter(|x| UNITS.contains(x)).map(String::from).unwrap_or(u);
    UNITS.contains(&u.as_str()).then_some(u)
}

type R<T> = Result<T, String>;

impl<'s> Parser<'s> {
    pub fn new(src: &'s str) -> R<Parser<'s>> {
        Ok(Parser { src, t: tokenize(src)?, i: 0 })
    }
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i).map(|t| &t.t)
    }
    fn peek_at(&self, k: usize) -> Option<&Tok> {
        self.t.get(self.i + k).map(|t| &t.t)
    }
    fn is_kw(t: Option<&Tok>, k: &str) -> bool {
        matches!(t, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(k))
    }
    fn peek_kw(&self, k: &str) -> bool {
        Self::is_kw(self.peek(), k)
    }
    fn peek_kw_at(&self, n: usize, k: &str) -> bool {
        Self::is_kw(self.peek_at(n), k)
    }
    fn peek_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Sym(x)) if *x == s)
    }
    fn eat_kw(&mut self, k: &str) -> bool {
        if self.peek_kw(k) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn eat_kws(&mut self, ks: &[&str]) -> bool {
        for (n, k) in ks.iter().enumerate() {
            if !self.peek_kw_at(n, k) {
                return false;
            }
        }
        self.i += ks.len();
        true
    }
    fn eat_sym(&mut self, s: &str) -> bool {
        if self.peek_sym(s) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn want_kw(&mut self, k: &str) -> R<()> {
        if self.eat_kw(k) {
            Ok(())
        } else {
            Err(format!("Expected {} {}", k, self.near()))
        }
    }
    fn want_sym(&mut self, s: &str) -> R<()> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            Err(format!("Expected '{}' {}", s, self.near()))
        }
    }
    fn near(&self) -> String {
        match self.peek() {
            None => "at the end".into(),
            Some(t) => format!("near {}", show(t)),
        }
    }
    fn pos(&self) -> usize {
        self.t.get(self.i).map(|t| t.pos).unwrap_or(self.src.len())
    }
    fn prev_end(&self) -> usize {
        if self.i == 0 {
            0
        } else {
            self.t[self.i - 1].end
        }
    }
    fn text(&self, from: usize) -> String {
        self.src[from..self.prev_end().max(from)].trim().to_string()
    }
    fn eat_eq(&mut self) {
        self.eat_sym("=");
    }

    /// A name: bare word (not reserved), `quoted`, or 'string' where a name is expected.
    pub fn ident(&mut self) -> R<String> {
        match self.peek().cloned() {
            Some(Tok::Word(w)) if !reserved(&w) => {
                self.i += 1;
                Ok(w)
            }
            Some(Tok::Ident(w)) | Some(Tok::Str(w)) => {
                self.i += 1;
                Ok(w)
            }
            Some(Tok::Word(w)) => Err(format!("{} is a reserved word — put it in `backticks` to use it as a name", w)),
            _ => Err(format!("Expected a name {}", self.near())),
        }
    }
    /// Any word, reserved or not (after a dot, in index names, …).
    fn any_ident(&mut self) -> R<String> {
        match self.peek().cloned() {
            Some(Tok::Word(w)) | Some(Tok::Ident(w)) | Some(Tok::Str(w)) => {
                self.i += 1;
                Ok(w)
            }
            _ => Err(format!("Expected a name {}", self.near())),
        }
    }
    /// table or db.table (the database part is ignored).
    fn table_name(&mut self) -> R<String> {
        let mut n = self.ident()?;
        while self.eat_sym(".") {
            n = self.any_ident()?;
        }
        Ok(n)
    }
    fn paren_idents(&mut self) -> R<Vec<String>> {
        self.want_sym("(")?;
        let mut v = vec![];
        loop {
            v.push(self.any_ident()?);
            // key part length and order: col(10) DESC
            if self.eat_sym("(") {
                self.number()?;
                self.want_sym(")")?;
            }
            if !self.eat_kw("ASC") {
                self.eat_kw("DESC");
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        self.want_sym(")")?;
        Ok(v)
    }
    fn number(&mut self) -> R<u64> {
        match self.peek().cloned() {
            Some(Tok::Num(n)) => {
                self.i += 1;
                n.parse::<f64>().map(|f| f as u64).map_err(|_| format!("Expected a whole number, got {}", n))
            }
            _ => Err(format!("Expected a number {}", self.near())),
        }
    }
    fn string(&mut self) -> R<String> {
        match self.peek().cloned() {
            Some(Tok::Str(s)) => {
                self.i += 1;
                Ok(s)
            }
            _ => Err(format!("Expected a 'quoted text' {}", self.near())),
        }
    }
    /// Skip tokens up to (not including) the end of the statement.
    fn skip_rest(&mut self) {
        while self.peek().is_some() && !self.peek_sym(";") {
            self.i += 1;
        }
    }
    fn done(&self) -> bool {
        self.peek().is_none() || self.peek_sym(";")
    }

    // ------------------------------------------------------------ script

    pub fn script(&mut self) -> R<Vec<(Stmt, String)>> {
        let mut out = vec![];
        loop {
            while self.eat_sym(";") {}
            if self.peek().is_none() {
                break;
            }
            let st = self.pos();
            let s = self.stmt()?;
            let text = self.text(st);
            if !self.eat_sym(";") && self.peek().is_some() {
                return Err(format!("Unexpected {} (missing ';' between statements?)", self.near().trim_start_matches("near ")));
            }
            out.push((s, text));
        }
        Ok(out)
    }

    fn stmt(&mut self) -> R<Stmt> {
        let Some(first) = self.peek().cloned() else { return Err("Empty statement".into()) };
        if self.peek_sym("(") {
            return Ok(Stmt::Query(self.query()?));
        }
        let Tok::Word(w) = first else { return Err(format!("A statement can't start with {}", show(&first))) };
        let up = w.to_ascii_uppercase();
        match up.as_str() {
            "SELECT" | "WITH" | "VALUES" | "TABLE" => Ok(Stmt::Query(self.query()?)),
            "INSERT" | "REPLACE" => {
                self.i += 1;
                self.insert(up == "REPLACE")
            }
            "UPDATE" => {
                self.i += 1;
                self.update()
            }
            "DELETE" => {
                self.i += 1;
                self.delete()
            }
            "CREATE" => {
                self.i += 1;
                self.create()
            }
            "ALTER" => {
                self.i += 1;
                if self.eat_kw("TABLE") || self.eat_kws(&["ONLINE", "TABLE"]) {
                    self.alter()
                } else if self.eat_kw("DATABASE") || self.eat_kw("SCHEMA") {
                    self.skip_rest();
                    Ok(Stmt::Noop("Database options (character sets, collations) don't apply here — everything is stored as UTF-8.".into()))
                } else if self.eat_kw("VIEW") {
                    let name = self.table_name()?;
                    let cols = if self.peek_sym("(") { self.paren_idents()? } else { vec![] };
                    self.want_kw("AS")?;
                    let st = self.pos();
                    let q = self.query()?;
                    let sql = self.text(st);
                    Ok(Stmt::CreateView { name, or_replace: true, cols, sql, q: Box::new(q) })
                } else {
                    Err(format!("ALTER {} isn't supported", self.near().trim_start_matches("near ")))
                }
            }
            "DROP" => {
                self.i += 1;
                self.drop()
            }
            "RENAME" => {
                self.i += 1;
                if !self.eat_kw("TABLE") {
                    self.want_kw("TABLES")?;
                }
                let mut v = vec![];
                loop {
                    let a = self.table_name()?;
                    self.want_kw("TO")?;
                    v.push((a, self.table_name()?));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                Ok(Stmt::RenameTable(v))
            }
            "TRUNCATE" => {
                self.i += 1;
                self.eat_kw("TABLE");
                Ok(Stmt::Truncate(self.table_name()?))
            }
            "SHOW" => {
                self.i += 1;
                self.show()
            }
            "DESCRIBE" | "DESC" | "EXPLAIN" => {
                self.i += 1;
                self.eat_kw("EXTENDED");
                self.eat_kw("ANALYZE");
                if self.peek_kw("SELECT") || self.peek_kw("WITH") || self.peek_kw("UPDATE") || self.peek_kw("DELETE") || self.peek_kw("INSERT") || self.peek_sym("(") {
                    return Ok(Stmt::Explain(Box::new(self.stmt()?)));
                }
                let t = self.table_name()?;
                if !self.done() {
                    self.skip_rest();
                }
                Ok(Stmt::Describe(t))
            }
            "USE" => {
                self.i += 1;
                Ok(Stmt::Use(self.ident()?))
            }
            "GRANT" | "REVOKE" => {
                self.i += 1;
                self.grant(up == "REVOKE")
            }
            "SET" => {
                self.i += 1;
                self.set()
            }
            "BEGIN" | "START" => {
                self.i += 1;
                self.eat_kw("TRANSACTION");
                self.eat_kw("WORK");
                self.skip_rest();
                Ok(Stmt::Begin)
            }
            "COMMIT" | "SAVE" => {
                self.i += 1;
                self.skip_rest();
                Ok(Stmt::Commit)
            }
            "ROLLBACK" => {
                self.i += 1;
                self.skip_rest();
                Ok(Stmt::Rollback)
            }
            "OPTIMIZE" | "CHECKPOINT" => {
                // OPTIMIZE [NO_WRITE_TO_BINLOG | LOCAL] TABLE a, b  ·  CHECKPOINT [TABLE] a, b
                self.i += 1;
                while self.eat_kw("NO_WRITE_TO_BINLOG") || self.eat_kw("LOCAL") || self.eat_kw("TABLE") || self.eat_kw("TABLES") {}
                let mut names = vec![self.table_name()?];
                while self.eat_sym(",") {
                    names.push(self.table_name()?);
                }
                Ok(Stmt::Optimize(names))
            }
            "LOCK" | "UNLOCK" | "ANALYZE" | "CHECK" | "REPAIR" | "FLUSH" | "CHECKSUM" | "SAVEPOINT" | "RELEASE" | "KILL" | "RESET" | "PURGE" | "HANDLER" | "DO" | "CHANGE" | "CACHE" | "LOAD" | "INSTALL" | "UNINSTALL" => {
                self.i += 1;
                self.skip_rest();
                Ok(Stmt::Noop(match up.as_str() {
                    "LOCK" | "UNLOCK" => "Tables don't need locking: your changes wait in the editor until you save them.".into(),
                    "ANALYZE" | "CHECK" | "REPAIR" | "CHECKSUM" => format!("{} TABLE: nothing to do — there are no index files to rebuild; rows are read straight from the blockchain.", up),
                    "LOAD" => "LOAD DATA isn't available in a browser — use Import (CSV, JSON or SQL) instead.".into(),
                    _ => format!("{} has no effect here.", up),
                }))
            }
            "DELIMITER" => {
                self.skip_rest();
                Ok(Stmt::Noop("DELIMITER is a command-line client setting; ignored.".into()))
            }
            "CALL" => {
                self.skip_rest();
                Ok(Stmt::Unsupported("Stored procedures don't exist on IQ, so there's nothing to CALL.".into()))
            }
            other => Err(format!("Unknown command {}. Try SELECT, INSERT, UPDATE, DELETE, CREATE TABLE, ALTER TABLE, SHOW TABLES, DESCRIBE or COMMIT.", other)),
        }
    }

    // ------------------------------------------------------------ queries

    pub fn query(&mut self) -> R<Query> {
        let mut with = vec![];
        let mut recursive = false;
        if self.eat_kw("WITH") {
            recursive = self.eat_kw("RECURSIVE");
            loop {
                let name = self.ident()?;
                let cols = if self.peek_sym("(") { self.paren_idents()? } else { vec![] };
                self.want_kw("AS")?;
                self.want_sym("(")?;
                let q = self.query()?;
                self.want_sym(")")?;
                with.push(Cte { name, cols, q });
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let body = self.set_expr()?;
        let (order, limit, offset) = self.order_limit()?;
        // FOR UPDATE / LOCK IN SHARE MODE: nothing to lock
        if self.eat_kw("FOR") {
            self.eat_kw("UPDATE");
            self.eat_kw("SHARE");
        }
        if self.eat_kw("LOCK") {
            self.eat_kws(&["IN", "SHARE", "MODE"]);
        }
        // a lone parenthesised query keeps its own ORDER BY / LIMIT
        if with.is_empty() && order.is_empty() && limit.is_none() {
            if let Body::Paren(q) = body {
                return Ok(*q);
            }
        }
        Ok(Query { with, recursive, body, order, limit, offset })
    }

    fn order_limit(&mut self) -> R<(Vec<Order>, Option<Expr>, Option<Expr>)> {
        let mut order = vec![];
        if self.eat_kw("ORDER") {
            self.want_kw("BY")?;
            order = self.order_list()?;
        }
        let (mut limit, mut offset) = (None, None);
        if self.eat_kw("LIMIT") {
            let a = self.expr()?;
            if self.eat_sym(",") {
                offset = Some(a);
                limit = Some(self.expr()?);
            } else {
                limit = Some(a);
                if self.eat_kw("OFFSET") {
                    offset = Some(self.expr()?);
                }
            }
        }
        Ok((order, limit, offset))
    }

    fn order_list(&mut self) -> R<Vec<Order>> {
        let mut v = vec![];
        loop {
            let e = self.expr()?;
            let desc = if self.eat_kw("DESC") {
                true
            } else {
                self.eat_kw("ASC");
                false
            };
            v.push(Order { e, desc });
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(v)
    }

    fn set_expr(&mut self) -> R<Body> {
        let mut l = self.set_term()?;
        loop {
            let op = if self.eat_kw("UNION") {
                if self.eat_kw("ALL") {
                    SetOp::UnionAll
                } else {
                    self.eat_kw("DISTINCT");
                    SetOp::Union
                }
            } else if self.eat_kw("EXCEPT") || self.eat_kw("MINUS") {
                self.eat_kw("DISTINCT");
                SetOp::Except
            } else {
                break;
            };
            let r = self.set_term()?;
            l = Body::Set(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn set_term(&mut self) -> R<Body> {
        let mut l = self.set_primary()?;
        while self.eat_kw("INTERSECT") {
            self.eat_kw("DISTINCT");
            let r = self.set_primary()?;
            l = Body::Set(SetOp::Intersect, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn set_primary(&mut self) -> R<Body> {
        if self.eat_sym("(") {
            let q = self.query()?;
            self.want_sym(")")?;
            return Ok(Body::Paren(Box::new(q)));
        }
        if self.eat_kw("VALUES") {
            let mut rows = vec![];
            loop {
                self.eat_kw("ROW");
                self.want_sym("(")?;
                let mut r = vec![self.expr()?];
                while self.eat_sym(",") {
                    r.push(self.expr()?);
                }
                self.want_sym(")")?;
                rows.push(r);
                if !self.eat_sym(",") {
                    break;
                }
            }
            return Ok(Body::Values(rows));
        }
        if self.eat_kw("TABLE") {
            let name = self.table_name()?;
            return Ok(Body::Select(Box::new(Select {
                distinct: false,
                items: vec![SelItem::Star(None)],
                from: Some(From::Table { name, alias: None }),
                filter: None,
                group: vec![],
                rollup: false,
                having: None,
            })));
        }
        self.want_kw("SELECT")?;
        Ok(Body::Select(Box::new(self.select_core()?)))
    }

    fn select_core(&mut self) -> R<Select> {
        let mut distinct = false;
        loop {
            if self.eat_kw("DISTINCT") || self.eat_kw("DISTINCTROW") {
                distinct = true;
            } else if self.eat_kw("ALL") {
            } else if ["SQL_CALC_FOUND_ROWS", "SQL_NO_CACHE", "SQL_CACHE", "HIGH_PRIORITY", "STRAIGHT_JOIN", "SQL_SMALL_RESULT", "SQL_BIG_RESULT", "SQL_BUFFER_RESULT"].iter().any(|k| self.peek_kw(k)) {
                self.i += 1;
            } else {
                break;
            }
        }
        let mut items = vec![];
        loop {
            if self.eat_sym("*") {
                items.push(SelItem::Star(None));
            } else if matches!(self.peek(), Some(Tok::Word(_)) | Some(Tok::Ident(_))) && matches!(self.peek_at(1), Some(Tok::Sym("."))) && matches!(self.peek_at(2), Some(Tok::Sym("*"))) {
                let t = self.any_ident()?;
                self.i += 2;
                items.push(SelItem::Star(Some(t)));
            } else {
                let st = self.pos();
                let e = self.expr()?;
                let label = self.text(st);
                let alias = if self.eat_kw("AS") {
                    Some(self.any_ident()?)
                } else if matches!(self.peek(), Some(Tok::Word(w)) if !reserved(w)) || matches!(self.peek(), Some(Tok::Ident(_)) | Some(Tok::Str(_))) {
                    Some(self.any_ident()?)
                } else {
                    None
                };
                items.push(SelItem::Expr(e, alias, label));
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        if self.peek_kw("INTO") {
            return Err("SELECT … INTO isn't supported; use INSERT INTO … SELECT or CREATE TABLE … AS SELECT.".into());
        }
        let from = if self.eat_kw("FROM") { Some(self.from_list()?) } else { None };
        let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        let mut group = vec![];
        let mut rollup = false;
        if self.eat_kw("GROUP") {
            self.want_kw("BY")?;
            loop {
                group.push(self.expr()?);
                if !self.eat_kw("ASC") {
                    self.eat_kw("DESC");
                }
                if !self.eat_sym(",") {
                    break;
                }
            }
            if self.eat_kw("WITH") {
                self.want_kw("ROLLUP")?;
                rollup = true;
            }
        }
        let having = if self.eat_kw("HAVING") { Some(self.expr()?) } else { None };
        if self.peek_kw("WINDOW") {
            return Err("Named windows (WINDOW w AS …) aren't supported; write OVER (…) directly.".into());
        }
        Ok(Select { distinct, items, from, filter, group, rollup, having })
    }

    fn from_list(&mut self) -> R<From> {
        let mut l = self.join_chain()?;
        while self.eat_sym(",") {
            let r = self.join_chain()?;
            l = From::Join { left: Box::new(l), right: Box::new(r), kind: JoinKind::Cross, on: None, using: vec![], natural: false };
        }
        Ok(l)
    }

    fn join_chain(&mut self) -> R<From> {
        let mut l = self.table_factor()?;
        loop {
            let natural = self.eat_kw("NATURAL");
            let kind = if self.eat_kw("INNER") {
                JoinKind::Inner
            } else if self.eat_kw("CROSS") {
                JoinKind::Cross
            } else if self.eat_kw("LEFT") {
                self.eat_kw("OUTER");
                JoinKind::Left
            } else if self.eat_kw("RIGHT") {
                self.eat_kw("OUTER");
                JoinKind::Right
            } else if self.peek_kw("JOIN") || self.peek_kw("STRAIGHT_JOIN") {
                JoinKind::Inner
            } else if natural {
                return Err(format!("Expected JOIN {}", self.near()));
            } else {
                break;
            };
            if !self.eat_kw("JOIN") && !self.eat_kw("STRAIGHT_JOIN") {
                return Err(format!("Expected JOIN {}", self.near()));
            }
            let r = self.table_factor()?;
            let (mut on, mut using) = (None, vec![]);
            if !natural {
                if self.eat_kw("ON") {
                    on = Some(self.expr()?);
                } else if self.eat_kw("USING") {
                    using = self.paren_idents()?;
                }
            }
            let kind = if kind == JoinKind::Cross && on.is_some() { JoinKind::Inner } else { kind };
            l = From::Join { left: Box::new(l), right: Box::new(r), kind, on, using, natural };
        }
        Ok(l)
    }

    fn table_alias(&mut self) -> R<Option<String>> {
        if self.eat_kw("AS") {
            return Ok(Some(self.any_ident()?));
        }
        match self.peek() {
            Some(Tok::Word(w)) if !reserved(w) && !["USE", "IGNORE", "FORCE"].iter().any(|k| w.eq_ignore_ascii_case(k)) => Ok(Some(self.any_ident()?)),
            Some(Tok::Ident(_)) => Ok(Some(self.any_ident()?)),
            _ => Ok(None),
        }
    }

    fn table_factor(&mut self) -> R<From> {
        if self.eat_sym("(") {
            if self.peek_kw("SELECT") || self.peek_kw("WITH") || self.peek_kw("VALUES") || self.peek_sym("(") && self.peek_kw_at(1, "SELECT") {
                let q = self.query()?;
                self.want_sym(")")?;
                let alias = self.table_alias()?.ok_or("A subquery in FROM needs a name: (SELECT …) AS t")?;
                return Ok(From::Sub { q: Box::new(q), alias });
            }
            let f = self.from_list()?;
            self.want_sym(")")?;
            return Ok(f);
        }
        if self.eat_kw("DUAL") {
            return Ok(From::Sub { q: Box::new(dual()), alias: "dual".into() });
        }
        let name = self.table_name()?;
        let alias = self.table_alias()?;
        // index hints
        while self.peek_kw("USE") || self.peek_kw("IGNORE") || self.peek_kw("FORCE") {
            self.i += 1;
            if !self.eat_kw("INDEX") {
                self.eat_kw("KEY");
            }
            if self.eat_kw("FOR") {
                self.i += 1;
                self.eat_kw("BY");
            }
            let mut depth = 0;
            loop {
                if self.eat_sym("(") {
                    depth += 1;
                } else if self.eat_sym(")") {
                    depth -= 1;
                    if depth <= 0 {
                        break;
                    }
                } else if self.peek().is_none() {
                    break;
                } else {
                    self.i += 1;
                }
            }
        }
        Ok(From::Table { name, alias })
    }

    // -------------------------------------------------------- expressions

    pub fn expr(&mut self) -> R<Expr> {
        self.or()
    }
    fn or(&mut self) -> R<Expr> {
        let mut l = self.xor()?;
        while self.eat_kw("OR") || self.eat_sym("||") {
            l = Expr::Bin("OR", Box::new(l), Box::new(self.xor()?));
        }
        Ok(l)
    }
    fn xor(&mut self) -> R<Expr> {
        let mut l = self.and()?;
        while self.eat_kw("XOR") {
            l = Expr::Bin("XOR", Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }
    fn and(&mut self) -> R<Expr> {
        let mut l = self.not()?;
        while self.eat_kw("AND") || self.eat_sym("&&") {
            l = Expr::Bin("AND", Box::new(l), Box::new(self.not()?));
        }
        Ok(l)
    }
    fn not(&mut self) -> R<Expr> {
        if self.peek_kw("NOT") && !self.peek_kw_at(1, "EXISTS") {
            self.i += 1;
            return Ok(Expr::Unary("NOT", Box::new(self.not()?)));
        }
        self.pred()
    }
    fn subquery_follows(&self) -> bool {
        self.peek_sym("(") && (self.peek_kw_at(1, "SELECT") || self.peek_kw_at(1, "WITH") || (matches!(self.peek_at(1), Some(Tok::Sym("("))) && self.peek_kw_at(2, "SELECT")))
    }
    fn pred(&mut self) -> R<Expr> {
        let mut l = self.bitor()?;
        loop {
            let ops: [(&str, &'static str); 7] = [("=", "="), ("<=>", "<=>"), ("!=", "!="), ("<=", "<="), (">=", ">="), ("<", "<"), (">", ">")];
            let mut matched = false;
            for (s, op) in ops {
                if self.eat_sym(s) {
                    matched = true;
                    let all = if self.eat_kw("ALL") {
                        Some(true)
                    } else if self.eat_kw("ANY") || self.eat_kw("SOME") {
                        Some(false)
                    } else {
                        None
                    };
                    if let Some(all) = all {
                        self.want_sym("(")?;
                        let q = self.query()?;
                        self.want_sym(")")?;
                        l = Expr::Quantified { e: Box::new(l), op, all, q: Box::new(q) };
                    } else {
                        let r = self.bitor()?;
                        l = Expr::Bin(op, Box::new(l), Box::new(r));
                    }
                    break;
                }
            }
            if matched {
                continue;
            }
            if self.eat_kw("IS") {
                let not = self.eat_kw("NOT");
                if self.eat_kw("NULL") || self.eat_kw("UNKNOWN") {
                    l = Expr::IsNull(Box::new(l), not);
                } else if self.eat_kw("TRUE") {
                    l = Expr::IsBool(Box::new(l), true, not);
                } else if self.eat_kw("FALSE") {
                    l = Expr::IsBool(Box::new(l), false, not);
                } else {
                    return Err(format!("Expected NULL, TRUE or FALSE after IS {}", self.near()));
                }
                continue;
            }
            let save = self.i;
            let not = self.eat_kw("NOT");
            if self.eat_kw("LIKE") {
                let pat = self.bitor()?;
                let esc = if self.eat_kw("ESCAPE") { Some(Box::new(self.primary()?)) } else { None };
                l = Expr::Like { e: Box::new(l), pat: Box::new(pat), esc, not };
                continue;
            }
            if self.eat_kw("REGEXP") || self.eat_kw("RLIKE") {
                let pat = self.bitor()?;
                l = Expr::Regexp { e: Box::new(l), pat: Box::new(pat), not };
                continue;
            }
            if self.eat_kw("IN") {
                if self.subquery_follows() {
                    self.want_sym("(")?;
                    let q = self.query()?;
                    self.want_sym(")")?;
                    l = Expr::InQuery { e: Box::new(l), q: Box::new(q), not };
                } else {
                    self.want_sym("(")?;
                    let mut v = vec![];
                    if !self.peek_sym(")") {
                        v.push(self.expr()?);
                        while self.eat_sym(",") {
                            v.push(self.expr()?);
                        }
                    }
                    self.want_sym(")")?;
                    l = Expr::In { e: Box::new(l), list: v, not };
                }
                continue;
            }
            if self.eat_kw("BETWEEN") {
                let lo = self.bitor()?;
                self.want_kw("AND")?;
                let hi = self.bitor()?;
                l = Expr::Between { e: Box::new(l), lo: Box::new(lo), hi: Box::new(hi), not };
                continue;
            }
            if self.eat_kws(&["SOUNDS", "LIKE"]) {
                let r = self.bitor()?;
                l = Expr::Bin("=", Box::new(func("SOUNDEX", vec![l])), Box::new(func("SOUNDEX", vec![r])));
                continue;
            }
            if not {
                self.i = save;
            }
            break;
        }
        Ok(l)
    }
    fn bitor(&mut self) -> R<Expr> {
        let mut l = self.bitand()?;
        while self.eat_sym("|") {
            l = Expr::Bin("|", Box::new(l), Box::new(self.bitand()?));
        }
        Ok(l)
    }
    fn bitand(&mut self) -> R<Expr> {
        let mut l = self.shift()?;
        while self.eat_sym("&") {
            l = Expr::Bin("&", Box::new(l), Box::new(self.shift()?));
        }
        Ok(l)
    }
    fn shift(&mut self) -> R<Expr> {
        let mut l = self.add()?;
        loop {
            let op = if self.eat_sym("<<") {
                "<<"
            } else if self.eat_sym(">>") {
                ">>"
            } else {
                break;
            };
            l = Expr::Bin(op, Box::new(l), Box::new(self.add()?));
        }
        Ok(l)
    }
    fn add(&mut self) -> R<Expr> {
        let mut l = self.mul()?;
        loop {
            let op = if self.eat_sym("+") {
                "+"
            } else if self.eat_sym("-") {
                "-"
            } else {
                break;
            };
            l = Expr::Bin(op, Box::new(l), Box::new(self.mul()?));
        }
        Ok(l)
    }
    fn mul(&mut self) -> R<Expr> {
        let mut l = self.bitxor()?;
        loop {
            let op = if self.eat_sym("*") {
                "*"
            } else if self.eat_sym("/") {
                "/"
            } else if self.eat_sym("%") || self.eat_kw("MOD") {
                "%"
            } else if self.eat_kw("DIV") {
                "DIV"
            } else {
                break;
            };
            l = Expr::Bin(op, Box::new(l), Box::new(self.bitxor()?));
        }
        Ok(l)
    }
    fn bitxor(&mut self) -> R<Expr> {
        let mut l = self.unary()?;
        while self.eat_sym("^") {
            l = Expr::Bin("^", Box::new(l), Box::new(self.unary()?));
        }
        Ok(l)
    }
    fn unary(&mut self) -> R<Expr> {
        if self.eat_sym("-") {
            let e = self.unary()?;
            return Ok(match e {
                Expr::Lit(Json::Num(n)) if !n.starts_with('-') => Expr::Lit(Json::Num(format!("-{}", n))),
                e => Expr::Unary("-", Box::new(e)),
            });
        }
        if self.eat_sym("+") {
            return self.unary();
        }
        if self.eat_sym("~") {
            return Ok(Expr::Unary("~", Box::new(self.unary()?)));
        }
        if self.eat_sym("!") {
            return Ok(Expr::Unary("NOT", Box::new(self.unary()?)));
        }
        if self.eat_kw("BINARY") {
            return self.unary();
        }
        let mut e = self.primary()?;
        loop {
            if self.eat_kw("COLLATE") {
                self.any_ident()?;
            } else if self.eat_sym("->") {
                let p = self.primary()?;
                e = func("JSON_EXTRACT", vec![e, p]);
            } else if self.eat_sym("->>") {
                let p = self.primary()?;
                e = func("JSON_UNQUOTE", vec![func("JSON_EXTRACT", vec![e, p])]);
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn primary(&mut self) -> R<Expr> {
        let Some(t) = self.peek().cloned() else { return Err("Unexpected end of the statement".into()) };
        match t {
            Tok::Num(n) => {
                self.i += 1;
                Ok(Expr::Lit(Json::Num(n)))
            }
            Tok::Str(s) => {
                self.i += 1;
                Ok(Expr::Lit(Json::Str(s)))
            }
            Tok::Var(v) => {
                self.i += 1;
                Ok(Expr::Var(v))
            }
            Tok::Ident(_) => self.column(),
            Tok::Sym("(") => {
                if self.subquery_follows() {
                    self.i += 1;
                    let q = self.query()?;
                    self.want_sym(")")?;
                    return Ok(Expr::Scalar(Box::new(q)));
                }
                self.i += 1;
                let mut v = vec![self.expr()?];
                while self.eat_sym(",") {
                    v.push(self.expr()?);
                }
                self.want_sym(")")?;
                Ok(if v.len() == 1 { v.pop().unwrap() } else { Expr::Row(v) })
            }
            Tok::Sym("?") => Err("Placeholders (?) aren't supported — write the values into the statement.".into()),
            Tok::Sym("{") => Err("ODBC escapes ({d '…'}) aren't supported — write DATE '…' instead.".into()),
            Tok::Word(w) => {
                let up = w.to_ascii_uppercase();
                let next_paren = matches!(self.peek_at(1), Some(Tok::Sym("(")));
                match up.as_str() {
                    "NULL" | "UNKNOWN" => {
                        self.i += 1;
                        return Ok(Expr::Lit(Json::Null));
                    }
                    "TRUE" => {
                        self.i += 1;
                        return Ok(Expr::Lit(Json::Num("1".into())));
                    }
                    "FALSE" => {
                        self.i += 1;
                        return Ok(Expr::Lit(Json::Num("0".into())));
                    }
                    "DEFAULT" if !next_paren => {
                        self.i += 1;
                        return Ok(Expr::Default);
                    }
                    "CASE" => {
                        self.i += 1;
                        return self.case();
                    }
                    "EXISTS" => {
                        self.i += 1;
                        self.want_sym("(")?;
                        let q = self.query()?;
                        self.want_sym(")")?;
                        return Ok(Expr::Exists(Box::new(q), false));
                    }
                    "NOT" if self.peek_kw_at(1, "EXISTS") => {
                        self.i += 2;
                        self.want_sym("(")?;
                        let q = self.query()?;
                        self.want_sym(")")?;
                        return Ok(Expr::Exists(Box::new(q), true));
                    }
                    "INTERVAL" => {
                        self.i += 1;
                        let n = self.expr()?;
                        let u = self.any_ident()?;
                        let unit = unit_of(&u).ok_or_else(|| format!("Unknown interval unit {}", u))?;
                        return Ok(Expr::Interval(Box::new(n), unit));
                    }
                    "DATE" | "TIME" | "TIMESTAMP" | "DATETIME" if matches!(self.peek_at(1), Some(Tok::Str(_))) => {
                        self.i += 1;
                        let s = self.string()?;
                        let to = match up.as_str() {
                            "DATE" => CastTo::Date,
                            "TIME" => CastTo::Time,
                            _ => CastTo::DateTime,
                        };
                        return Ok(Expr::Cast(Box::new(Expr::Lit(Json::Str(s))), to));
                    }
                    "CURRENT_DATE" | "CURRENT_TIME" | "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" | "UTC_DATE" | "UTC_TIME" | "UTC_TIMESTAMP" | "CURRENT_USER" if !next_paren => {
                        self.i += 1;
                        return Ok(func(&up, vec![]));
                    }
                    "VALUES" if next_paren => {
                        self.i += 2;
                        let c = self.any_ident()?;
                        self.want_sym(")")?;
                        return Ok(Expr::Values(c));
                    }
                    "MATCH" if next_paren => return Err("MATCH … AGAINST (full-text search) isn't supported — use LIKE '%word%' or REGEXP.".into()),
                    _ => {}
                }
                if next_paren {
                    self.i += 2;
                    return self.call(&up);
                }
                if reserved(&w) {
                    return Err(format!("Unexpected {} — if it's a column name, put it in `backticks`", w));
                }
                self.column()
            }
            other => Err(format!("Unexpected {}", show(&other))),
        }
    }

    fn column(&mut self) -> R<Expr> {
        let a = self.any_ident()?;
        if self.eat_sym(".") {
            let b = self.any_ident()?;
            if self.eat_sym(".") {
                // db.table.column
                let c = self.any_ident()?;
                return Ok(Expr::Col(Some(b), c));
            }
            return Ok(Expr::Col(Some(a), b));
        }
        Ok(Expr::Col(None, a))
    }

    fn case(&mut self) -> R<Expr> {
        let operand = if self.peek_kw("WHEN") { None } else { Some(Box::new(self.expr()?)) };
        let mut whens = vec![];
        while self.eat_kw("WHEN") {
            let c = self.expr()?;
            self.want_kw("THEN")?;
            whens.push((c, self.expr()?));
        }
        if whens.is_empty() {
            return Err("CASE needs at least one WHEN … THEN …".into());
        }
        let other = if self.eat_kw("ELSE") { Some(Box::new(self.expr()?)) } else { None };
        self.want_kw("END")?;
        Ok(Expr::Case { operand, whens, other })
    }

    pub fn cast_type(&mut self) -> R<CastTo> {
        let w = self.any_ident()?.to_ascii_uppercase();
        let t = match w.as_str() {
            "SIGNED" => {
                self.eat_kw("INTEGER");
                self.eat_kw("INT");
                CastTo::Signed
            }
            "UNSIGNED" => {
                self.eat_kw("INTEGER");
                self.eat_kw("INT");
                CastTo::Unsigned
            }
            "INT" | "INTEGER" | "BIGINT" => CastTo::Signed,
            "DECIMAL" | "NUMERIC" => {
                let (mut p, mut s) = (10, 0);
                if self.eat_sym("(") {
                    p = self.number()? as u32;
                    if self.eat_sym(",") {
                        s = self.number()? as u32;
                    }
                    self.want_sym(")")?;
                }
                return Ok(CastTo::Decimal(p, s));
            }
            "DOUBLE" | "FLOAT" | "REAL" => CastTo::Double,
            "CHAR" | "VARCHAR" | "NCHAR" | "TEXT" => CastTo::Char,
            "BINARY" => CastTo::Binary,
            "DATE" => CastTo::Date,
            "DATETIME" | "TIMESTAMP" => CastTo::DateTime,
            "TIME" => CastTo::Time,
            "JSON" => CastTo::Json,
            other => return Err(format!("Can't CAST to {}", other)),
        };
        if self.eat_sym("(") {
            self.number()?;
            self.want_sym(")")?;
        }
        while self.eat_kw("CHARACTER") || self.eat_kw("CHARSET") || self.eat_kw("COLLATE") {
            self.eat_kw("SET");
            self.any_ident()?;
        }
        Ok(t)
    }

    fn call(&mut self, name: &str) -> R<Expr> {
        let mut c = Call { name: name.to_string(), args: vec![], star: false, distinct: false, order: vec![], sep: None };
        match name {
            "CAST" => {
                let e = self.expr()?;
                self.want_kw("AS")?;
                let t = self.cast_type()?;
                self.want_sym(")")?;
                return Ok(Expr::Cast(Box::new(e), t));
            }
            "CONVERT" => {
                let e = self.expr()?;
                if self.eat_kw("USING") {
                    self.any_ident()?;
                    self.want_sym(")")?;
                    return Ok(e);
                }
                self.want_sym(",")?;
                let t = self.cast_type()?;
                self.want_sym(")")?;
                return Ok(Expr::Cast(Box::new(e), t));
            }
            "TRIM" => {
                let mut mode = "BOTH";
                for m in ["LEADING", "TRAILING", "BOTH"] {
                    if self.eat_kw(m) {
                        mode = m;
                    }
                }
                if self.eat_kw("FROM") {
                    let s = self.expr()?;
                    self.want_sym(")")?;
                    return Ok(func(&format!("TRIM_{}", mode), vec![s, Expr::Lit(Json::Str(" ".into()))]));
                }
                let a = self.expr()?;
                if self.eat_kw("FROM") {
                    let s = self.expr()?;
                    self.want_sym(")")?;
                    return Ok(func(&format!("TRIM_{}", mode), vec![s, a]));
                }
                self.want_sym(")")?;
                return Ok(func(&format!("TRIM_{}", mode), vec![a, Expr::Lit(Json::Str(" ".into()))]));
            }
            "SUBSTRING" | "SUBSTR" | "MID" => {
                let s = self.expr()?;
                if self.eat_kw("FROM") {
                    let p = self.expr()?;
                    let mut args = vec![s, p];
                    if self.eat_kw("FOR") {
                        args.push(self.expr()?);
                    }
                    self.want_sym(")")?;
                    return Ok(func("SUBSTRING", args));
                }
                c.args.push(s);
                while self.eat_sym(",") {
                    c.args.push(self.expr()?);
                }
                self.want_sym(")")?;
                c.name = "SUBSTRING".into();
                return Ok(Expr::Func(Box::new(c)));
            }
            "POSITION" => {
                let sub = self.bitor()?;
                self.want_kw("IN")?;
                let s = self.expr()?;
                self.want_sym(")")?;
                return Ok(func("LOCATE", vec![sub, s]));
            }
            "EXTRACT" => {
                let u = self.any_ident()?;
                let unit = unit_of(&u).ok_or_else(|| format!("Unknown unit {}", u))?;
                self.want_kw("FROM")?;
                let e = self.expr()?;
                self.want_sym(")")?;
                return Ok(func("EXTRACT", vec![Expr::Lit(Json::Str(unit)), e]));
            }
            "TIMESTAMPDIFF" | "TIMESTAMPADD" => {
                let u = self.any_ident()?;
                let unit = unit_of(&u).ok_or_else(|| format!("Unknown unit {}", u))?;
                c.args.push(Expr::Lit(Json::Str(unit)));
                while self.eat_sym(",") {
                    c.args.push(self.expr()?);
                }
                self.want_sym(")")?;
                return Ok(Expr::Func(Box::new(c)));
            }
            "CHAR" => {
                c.args.push(self.expr()?);
                while self.eat_sym(",") {
                    c.args.push(self.expr()?);
                }
                if self.eat_kw("USING") {
                    self.any_ident()?;
                }
                self.want_sym(")")?;
                return Ok(Expr::Func(Box::new(c)));
            }
            _ => {}
        }
        if self.eat_sym("*") {
            c.star = true;
        } else if !self.peek_sym(")") {
            if self.eat_kw("DISTINCT") {
                c.distinct = true;
            } else {
                self.eat_kw("ALL");
            }
            c.args.push(self.expr()?);
            while self.eat_sym(",") {
                c.args.push(self.expr()?);
            }
            if self.eat_kw("ORDER") {
                self.want_kw("BY")?;
                c.order = self.order_list()?;
            }
            if self.eat_kw("SEPARATOR") {
                c.sep = Some(self.string()?);
            }
        }
        self.want_sym(")")?;
        // window functions
        if self.eat_kw("OVER") {
            self.want_sym("(")?;
            let mut over = Over { partition: vec![], order: vec![], whole: None };
            if self.eat_kw("PARTITION") {
                self.want_kw("BY")?;
                loop {
                    over.partition.push(self.expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
            }
            if self.eat_kw("ORDER") {
                self.want_kw("BY")?;
                over.order = self.order_list()?;
            }
            if self.eat_kw("ROWS") || self.eat_kw("RANGE") {
                // only the two common frames
                let st = self.i;
                let mut words = vec![];
                while !self.peek_sym(")") && self.peek().is_some() {
                    if let Some(Tok::Word(w)) = self.peek() {
                        words.push(w.to_ascii_uppercase());
                    }
                    self.i += 1;
                }
                let w = words.join(" ");
                over.whole = Some(if w.contains("UNBOUNDED FOLLOWING") {
                    true
                } else if w.contains("UNBOUNDED PRECEDING") && w.contains("CURRENT ROW") || w == "UNBOUNDED PRECEDING" {
                    false
                } else {
                    self.i = st;
                    return Err("Only ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW / UNBOUNDED FOLLOWING frames are supported.".into());
                });
            }
            self.want_sym(")")?;
            return Ok(Expr::Window(Box::new(c), Box::new(over)));
        }
        Ok(Expr::Func(Box::new(c)))
    }

    // ------------------------------------------------------------ writes

    fn insert(&mut self, replace: bool) -> R<Stmt> {
        let mut ignore = false;
        loop {
            if self.eat_kw("IGNORE") {
                ignore = true;
            } else if self.eat_kw("LOW_PRIORITY") || self.eat_kw("DELAYED") || self.eat_kw("HIGH_PRIORITY") {
            } else {
                break;
            }
        }
        self.eat_kw("INTO");
        let table = self.table_name()?;
        let mut cols = None;
        if self.peek_sym("(") && !self.subquery_follows() {
            self.i += 1;
            if self.peek_sym(")") {
                cols = Some(vec![]);
            } else {
                cols = Some(self.ident_list_any()?);
            }
            self.want_sym(")")?;
        }
        let src = if self.eat_kw("VALUES") || self.eat_kw("VALUE") {
            let mut rows = vec![];
            loop {
                self.eat_kw("ROW");
                self.want_sym("(")?;
                let mut r = vec![];
                if !self.peek_sym(")") {
                    r.push(self.expr()?);
                    while self.eat_sym(",") {
                        r.push(self.expr()?);
                    }
                }
                self.want_sym(")")?;
                rows.push(r);
                if !self.eat_sym(",") {
                    break;
                }
            }
            InsertSrc::Values(rows)
        } else if self.eat_kw("SET") {
            let mut v = vec![];
            loop {
                let c = self.any_ident()?;
                self.want_sym("=")?;
                v.push((c, self.expr()?));
                if !self.eat_sym(",") {
                    break;
                }
            }
            InsertSrc::Set(v)
        } else if self.peek_kw("SELECT") || self.peek_kw("WITH") || self.peek_sym("(") || self.peek_kw("TABLE") {
            InsertSrc::Query(Box::new(self.query()?))
        } else {
            return Err(format!("Expected VALUES, SET or SELECT {}", self.near()));
        };
        // INSERT … VALUES (…) AS new ON DUPLICATE KEY UPDATE a = new.a
        if self.eat_kw("AS") {
            self.any_ident()?;
        }
        let mut on_dup = vec![];
        if self.eat_kws(&["ON", "DUPLICATE", "KEY", "UPDATE"]) {
            loop {
                let mut c = self.any_ident()?;
                if self.eat_sym(".") {
                    c = self.any_ident()?;
                }
                self.want_sym("=")?;
                on_dup.push((c, self.expr()?));
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        Ok(Stmt::Insert { table, cols, src, ignore, replace, on_dup })
    }

    fn ident_list_any(&mut self) -> R<Vec<String>> {
        let mut v = vec![self.any_ident()?];
        while self.eat_sym(",") {
            v.push(self.any_ident()?);
        }
        Ok(v)
    }

    fn update(&mut self) -> R<Stmt> {
        while self.eat_kw("LOW_PRIORITY") || self.eat_kw("IGNORE") {}
        let from = self.from_list()?;
        self.want_kw("SET")?;
        let mut sets = vec![];
        loop {
            let a = self.any_ident()?;
            let (q, c) = if self.eat_sym(".") { (Some(a), self.any_ident()?) } else { (None, a) };
            self.want_sym("=")?;
            sets.push((q, c, self.expr()?));
            if !self.eat_sym(",") {
                break;
            }
        }
        let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        let (order, limit, _) = self.order_limit()?;
        Ok(Stmt::Update { from, sets, filter, order, limit })
    }

    fn delete(&mut self) -> R<Stmt> {
        while self.eat_kw("LOW_PRIORITY") || self.eat_kw("QUICK") || self.eat_kw("IGNORE") {}
        let mut targets = vec![];
        let from;
        if self.eat_kw("FROM") {
            let t = self.table_name()?;
            if self.eat_kw("USING") {
                // DELETE FROM t1 USING t1 JOIN t2 …
                targets.push(t);
                from = self.from_list()?;
            } else {
                let alias = self.table_alias()?;
                targets.push(alias.clone().unwrap_or_else(|| t.clone()));
                if self.peek_sym(",") || self.peek_kw("JOIN") {
                    return Err("Multi-table DELETE is written DELETE t1 FROM t1 JOIN t2 ON …".into());
                }
                from = From::Table { name: t, alias };
            }
        } else {
            loop {
                let mut t = self.table_name()?;
                if self.eat_sym(".") {
                    self.want_sym("*")?;
                    t = t.to_string();
                }
                targets.push(t);
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.want_kw("FROM")?;
            from = self.from_list()?;
        }
        let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        let (order, limit, _) = self.order_limit()?;
        Ok(Stmt::Delete { targets, from, filter, order, limit })
    }

    // --------------------------------------------------------- structure

    /// A column type, e.g. INT UNSIGNED, VARCHAR(255), DECIMAL(10,2), ENUM('a','b').
    fn col_type(&mut self) -> R<Option<Ty>> {
        let Some(Tok::Word(w)) = self.peek().cloned() else { return Ok(None) };
        let up = w.to_ascii_uppercase();
        let mut name = up.clone();
        if Ty::from_parts(&up, &[TypeArg::Num(10), TypeArg::Num(2)], false).is_err() && !matches!(up.as_str(), "ENUM" | "SET" | "VARCHAR" | "NATIONAL" | "LONG") {
            return Ok(None);
        }
        if up == "SET" && !matches!(self.peek_at(1), Some(Tok::Sym("("))) {
            return Ok(None);
        }
        self.i += 1;
        if up == "NATIONAL" {
            name = self.any_ident()?.to_ascii_uppercase();
        }
        if name == "DOUBLE" {
            self.eat_kw("PRECISION");
        }
        if name == "CHARACTER" && self.eat_kw("VARYING") {
            name = "VARCHAR".into();
        }
        if name == "LONG" {
            if self.eat_kw("VARCHAR") || self.eat_kw("VARBINARY") {
                name = "MEDIUMTEXT".into();
            }
        }
        let mut args = vec![];
        if self.eat_sym("(") {
            loop {
                match self.peek().cloned() {
                    Some(Tok::Num(n)) => {
                        self.i += 1;
                        args.push(TypeArg::Num(n.parse::<f64>().unwrap_or(0.0) as u32));
                    }
                    Some(Tok::Str(s)) => {
                        self.i += 1;
                        args.push(TypeArg::Str(s));
                    }
                    _ => return Err(format!("Expected a number or 'text' in the type {}", self.near())),
                }
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.want_sym(")")?;
        }
        let mut unsigned = false;
        loop {
            if self.eat_kw("UNSIGNED") {
                unsigned = true;
            } else if self.eat_kw("SIGNED") || self.eat_kw("ZEROFILL") || self.eat_kw("BINARY") {
            } else if self.eat_kw("CHARACTER") || self.eat_kw("CHARSET") {
                self.eat_kw("SET");
                self.any_ident()?;
            } else if self.eat_kw("COLLATE") {
                self.any_ident()?;
            } else {
                break;
            }
        }
        Ty::from_parts(&name, &args, unsigned).map(Some)
    }

    fn default_value(&mut self) -> R<DefaultSpec> {
        let st = self.pos();
        if self.peek_sym("(") {
            self.expr()?;
            return Ok(DefaultSpec::Expr(self.text(st)));
        }
        if let Some(Tok::Word(w)) = self.peek().cloned() {
            let up = w.to_ascii_uppercase();
            if matches!(up.as_str(), "CURRENT_TIMESTAMP" | "NOW" | "LOCALTIME" | "LOCALTIMESTAMP" | "CURRENT_DATE" | "CURDATE" | "CURRENT_TIME" | "CURTIME" | "UTC_TIMESTAMP") {
                self.i += 1;
                if self.eat_sym("(") {
                    if !self.peek_sym(")") {
                        self.number()?;
                    }
                    self.want_sym(")")?;
                }
                return Ok(DefaultSpec::Expr(match up.as_str() {
                    "CURRENT_DATE" | "CURDATE" => "CURRENT_DATE".into(),
                    "CURRENT_TIME" | "CURTIME" => "CURRENT_TIME".into(),
                    _ => "CURRENT_TIMESTAMP".into(),
                }));
            }
        }
        let e = self.unary()?;
        match e {
            Expr::Lit(v) => Ok(DefaultSpec::Lit(v)),
            Expr::Cast(inner, _) => match *inner {
                Expr::Lit(v) => Ok(DefaultSpec::Lit(v)),
                _ => Ok(DefaultSpec::Expr(self.text(st))),
            },
            _ => Ok(DefaultSpec::Expr(self.text(st))),
        }
    }

    fn ref_spec(&mut self) -> R<RefSpec> {
        let table = self.table_name()?;
        let cols = if self.peek_sym("(") { self.paren_idents()? } else { vec![] };
        let mut r = RefSpec { table, cols, on_delete: RefAction::Restrict, on_update: RefAction::Restrict };
        loop {
            if self.eat_kw("MATCH") {
                self.any_ident()?;
            } else if self.eat_kw("ON") {
                let del = if self.eat_kw("DELETE") {
                    true
                } else {
                    self.want_kw("UPDATE")?;
                    false
                };
                let a = if self.eat_kw("CASCADE") {
                    RefAction::Cascade
                } else if self.eat_kw("RESTRICT") {
                    RefAction::Restrict
                } else if self.eat_kws(&["SET", "NULL"]) {
                    RefAction::SetNull
                } else if self.eat_kws(&["NO", "ACTION"]) {
                    RefAction::NoAction
                } else if self.eat_kws(&["SET", "DEFAULT"]) {
                    return Err("ON DELETE SET DEFAULT isn't supported; use SET NULL, CASCADE or RESTRICT.".into());
                } else {
                    return Err(format!("Expected CASCADE, RESTRICT, SET NULL or NO ACTION {}", self.near()));
                };
                if del {
                    r.on_delete = a;
                } else {
                    r.on_update = a;
                }
            } else {
                break;
            }
        }
        Ok(r)
    }

    fn check_expr(&mut self) -> R<String> {
        self.want_sym("(")?;
        let st = self.pos();
        self.expr()?;
        let t = self.text(st);
        self.want_sym(")")?;
        if self.eat_kw("NOT") {
            self.eat_kw("ENFORCED");
        } else {
            self.eat_kw("ENFORCED");
        }
        Ok(t)
    }

    pub fn column_spec(&mut self) -> R<ColumnSpec> {
        let name = self.any_ident()?;
        let ty = self.col_type()?.unwrap_or(Ty::Any);
        let serial = matches!(ty, Ty::Int(crate::schema::IntKind::Big, true)) && self.t[self.i.saturating_sub(1)].t == Tok::Word("SERIAL".into());
        let mut c = ColumnSpec { name, ty, not_null: None, default: None, on_update_now: false, auto_inc: false, unique: false, primary: false, comment: None, references: None, check: None };
        if serial {
            c.not_null = Some(true);
            c.auto_inc = true;
            c.unique = true;
        }
        loop {
            if self.eat_kws(&["NOT", "NULL"]) {
                c.not_null = Some(true);
            } else if self.eat_kw("NULL") {
                c.not_null = Some(false);
            } else if self.eat_kw("DEFAULT") {
                c.default = Some(if self.eat_kw("NULL") { None } else { Some(self.default_value()?) });
            } else if self.eat_kws(&["ON", "UPDATE"]) {
                let st = self.pos();
                let _ = self.default_value()?;
                let _ = st;
                c.on_update_now = true;
            } else if self.eat_kw("AUTO_INCREMENT") || self.eat_kw("AUTOINCREMENT") {
                c.auto_inc = true;
            } else if self.eat_kw("UNIQUE") {
                self.eat_kw("KEY");
                c.unique = true;
            } else if self.eat_kw("PRIMARY") {
                self.want_kw("KEY")?;
                c.primary = true;
            } else if self.eat_kw("KEY") {
                c.primary = true;
            } else if self.eat_kw("COMMENT") {
                c.comment = Some(self.string()?);
            } else if self.eat_kw("COLLATE") {
                self.any_ident()?;
            } else if self.eat_kw("CHARACTER") || self.eat_kw("CHARSET") {
                self.eat_kw("SET");
                self.any_ident()?;
            } else if self.eat_kw("REFERENCES") {
                c.references = Some(self.ref_spec()?);
            } else if self.eat_kw("CONSTRAINT") {
                let n = if self.peek_kw("CHECK") { None } else { Some(self.any_ident()?) };
                self.want_kw("CHECK")?;
                c.check = Some((n, self.check_expr()?));
            } else if self.eat_kw("CHECK") {
                c.check = Some((None, self.check_expr()?));
            } else if self.eat_kw("VISIBLE") || self.eat_kw("INVISIBLE") || self.eat_kws(&["COLUMN_FORMAT"]) || self.eat_kw("STORAGE") {
                if matches!(self.peek(), Some(Tok::Word(w)) if ["FIXED", "DYNAMIC", "DEFAULT", "DISK", "MEMORY"].iter().any(|k| w.eq_ignore_ascii_case(k))) {
                    self.i += 1;
                }
            } else if self.eat_kw("GENERATED") || self.peek_kw("AS") && matches!(self.peek_at(1), Some(Tok::Sym("("))) {
                return Err("Generated columns (… AS (expr)) aren't supported; use a view or compute it in SELECT.".into());
            } else if self.eat_kws(&["SERIAL", "DEFAULT", "VALUE"]) {
                c.not_null = Some(true);
                c.auto_inc = true;
                c.unique = true;
            } else {
                break;
            }
        }
        Ok(c)
    }

    fn constraint(&mut self) -> R<Option<Constraint>> {
        let save = self.i;
        let mut cname = None;
        if self.eat_kw("CONSTRAINT") {
            if !(self.peek_kw("PRIMARY") || self.peek_kw("UNIQUE") || self.peek_kw("FOREIGN") || self.peek_kw("CHECK")) {
                cname = Some(self.any_ident()?);
            }
        }
        if self.eat_kw("PRIMARY") {
            self.want_kw("KEY")?;
            if self.eat_kw("USING") {
                self.any_ident()?;
            }
            let cols = self.paren_idents()?;
            self.index_opts();
            return Ok(Some(Constraint::Primary(cols)));
        }
        if self.eat_kw("UNIQUE") {
            if !self.eat_kw("KEY") {
                self.eat_kw("INDEX");
            }
            let n = if self.peek_sym("(") { None } else { Some(self.any_ident()?) };
            if self.eat_kw("USING") {
                self.any_ident()?;
            }
            let cols = self.paren_idents()?;
            self.index_opts();
            return Ok(Some(Constraint::Unique(n.or(cname), cols)));
        }
        if self.eat_kw("FOREIGN") {
            self.want_kw("KEY")?;
            let n = if self.peek_sym("(") { None } else { Some(self.any_ident()?) };
            let cols = self.paren_idents()?;
            self.want_kw("REFERENCES")?;
            let r = self.ref_spec()?;
            return Ok(Some(Constraint::Foreign(cname.or(n), cols, r)));
        }
        if self.eat_kw("CHECK") {
            return Ok(Some(Constraint::Check(cname, self.check_expr()?)));
        }
        if cname.is_none() && (self.peek_kw("KEY") || self.peek_kw("INDEX") || self.peek_kw("FULLTEXT") || self.peek_kw("SPATIAL")) {
            self.i += 1;
            if self.peek_kw("KEY") || self.peek_kw("INDEX") {
                self.i += 1;
            }
            let n = if self.peek_sym("(") { None } else { Some(self.any_ident()?) };
            if self.eat_kw("USING") {
                self.any_ident()?;
            }
            let cols = self.paren_idents()?;
            self.index_opts();
            return Ok(Some(Constraint::Index(n, cols)));
        }
        self.i = save;
        Ok(None)
    }

    fn index_opts(&mut self) {
        loop {
            if self.eat_kw("USING") || self.eat_kw("COMMENT") || self.eat_kw("KEY_BLOCK_SIZE") {
                self.eat_eq();
                self.i += 1;
            } else if self.eat_kw("VISIBLE") || self.eat_kw("INVISIBLE") {
            } else {
                break;
            }
        }
    }

    fn table_opts(&mut self, o: &mut TableOpts) -> R<()> {
        loop {
            self.eat_sym(",");
            if self.eat_kw("ENGINE") || self.eat_kw("TYPE") || self.eat_kw("ROW_FORMAT") || self.eat_kw("COLLATE") || self.eat_kw("STATS_PERSISTENT") || self.eat_kw("PACK_KEYS") || self.eat_kw("CHECKSUM") || self.eat_kw("KEY_BLOCK_SIZE") || self.eat_kw("AVG_ROW_LENGTH") || self.eat_kw("MAX_ROWS") || self.eat_kw("MIN_ROWS") {
                self.eat_eq();
                self.i += 1;
            } else if self.eat_kw("DEFAULT") {
            } else if self.eat_kw("CHARSET") || self.eat_kws(&["CHARACTER", "SET"]) {
                self.eat_eq();
                self.i += 1;
            } else if self.eat_kw("AUTO_INCREMENT") {
                self.eat_eq();
                o.auto_increment = Some(self.number()?);
            } else if self.eat_kw("COMMENT") {
                self.eat_eq();
                o.comment = Some(self.string()?);
            } else if self.eat_kw("OPEN") {
                o.open = Some(true);
            } else if self.eat_kw("LOCKED") {
                o.open = Some(false);
            } else {
                break;
            }
        }
        Ok(())
    }

    fn create(&mut self) -> R<Stmt> {
        let or_replace = self.eat_kws(&["OR", "REPLACE"]);
        self.eat_kw("TEMPORARY");
        // CREATE [ALGORITHM=…] [DEFINER=…] [SQL SECURITY …] VIEW
        loop {
            if self.eat_kw("ALGORITHM") {
                self.eat_eq();
                self.i += 1;
            } else if self.eat_kw("DEFINER") {
                self.eat_eq();
                self.i += 1;
                if matches!(self.peek(), Some(Tok::Var(_))) {
                    self.i += 1;
                }
            } else if self.eat_kws(&["SQL", "SECURITY"]) {
                self.i += 1;
            } else {
                break;
            }
        }
        if self.eat_kw("VIEW") {
            let name = self.table_name()?;
            let cols = if self.peek_sym("(") { self.paren_idents()? } else { vec![] };
            self.want_kw("AS")?;
            let st = self.pos();
            let q = self.query()?;
            let sql = self.text(st);
            if self.eat_kw("WITH") {
                self.skip_rest();
            }
            return Ok(Stmt::CreateView { name, or_replace, cols, sql, q: Box::new(q) });
        }
        if self.eat_kw("DATABASE") || self.eat_kw("SCHEMA") {
            let if_not_exists = self.eat_kws(&["IF", "NOT", "EXISTS"]);
            let name = self.ident()?;
            self.skip_rest();
            return Ok(Stmt::CreateDatabase { name, if_not_exists });
        }
        let unique = self.eat_kw("UNIQUE");
        if self.eat_kw("FULLTEXT") || self.eat_kw("SPATIAL") {}
        if self.eat_kw("INDEX") || self.eat_kw("KEY") {
            let name = self.any_ident()?;
            if self.eat_kw("USING") {
                self.any_ident()?;
            }
            self.want_kw("ON")?;
            let table = self.table_name()?;
            let cols = self.paren_idents()?;
            self.index_opts();
            return Ok(Stmt::CreateIndex { name, table, cols, unique });
        }
        for (kw, what) in [("TRIGGER", "Triggers"), ("PROCEDURE", "Stored procedures"), ("FUNCTION", "Stored functions"), ("EVENT", "Scheduled events"), ("USER", "Database users"), ("ROLE", "Roles")] {
            if self.eat_kw(kw) {
                self.skip_rest();
                return Ok(Stmt::Unsupported(format!(
                    "{} need a server that runs code, and IQ is storage on Solana. {}",
                    what,
                    if kw == "USER" || kw == "ROLE" { "Who may write is set per table: GRANT INSERT ON t TO 'wallet address'." } else { "Do it in a query instead." }
                )));
            }
        }
        self.want_kw("TABLE")?;
        let if_not_exists = self.eat_kws(&["IF", "NOT", "EXISTS"]);
        let name = self.table_name()?;
        if self.eat_kw("LIKE") {
            let like = self.table_name()?;
            return Ok(Stmt::CreateTable { name, if_not_exists, cols: vec![], cons: vec![], opts: TableOpts::default(), like: Some(like), query: None });
        }
        let mut cols = vec![];
        let mut cons = vec![];
        let mut opts = TableOpts::default();
        if self.peek_sym("(") && !self.subquery_follows() {
            self.i += 1;
            if self.eat_kw("LIKE") {
                let like = self.table_name()?;
                self.want_sym(")")?;
                return Ok(Stmt::CreateTable { name, if_not_exists, cols: vec![], cons: vec![], opts, like: Some(like), query: None });
            }
            loop {
                match self.constraint()? {
                    Some(c) => cons.push(c),
                    None => cols.push(self.column_spec()?),
                }
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.want_sym(")")?;
        }
        self.table_opts(&mut opts)?;
        // PARTITION BY …: not meaningful here
        if self.eat_kw("PARTITION") {
            return Err("PARTITION BY isn't supported (and isn't needed: tables are read whole).".into());
        }
        self.eat_kw("IGNORE");
        self.eat_kw("REPLACE");
        let query = if self.eat_kw("AS") || self.peek_kw("SELECT") || self.peek_kw("WITH") || self.peek_sym("(") {
            Some(Box::new(self.query()?))
        } else {
            None
        };
        if cols.is_empty() && query.is_none() {
            return Err("A new table needs columns: CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100))".into());
        }
        Ok(Stmt::CreateTable { name, if_not_exists, cols, cons, opts, like: None, query })
    }

    fn place(&mut self) -> R<Option<Place>> {
        if self.eat_kw("FIRST") {
            return Ok(Some(Place::First));
        }
        if self.eat_kw("AFTER") {
            return Ok(Some(Place::After(self.any_ident()?)));
        }
        Ok(None)
    }

    fn alter(&mut self) -> R<Stmt> {
        self.eat_kw("IGNORE");
        let table = self.table_name()?;
        let mut ops = vec![];
        loop {
            if self.done() {
                break;
            }
            if self.eat_kw("ADD") {
                if let Some(c) = self.constraint()? {
                    ops.push(AlterOp::AddConstraint(c));
                } else {
                    self.eat_kw("COLUMN");
                    if self.eat_sym("(") {
                        loop {
                            match self.constraint()? {
                                Some(c) => ops.push(AlterOp::AddConstraint(c)),
                                None => ops.push(AlterOp::AddColumn(self.column_spec()?, None)),
                            }
                            if !self.eat_sym(",") {
                                break;
                            }
                        }
                        self.want_sym(")")?;
                    } else {
                        let c = self.column_spec()?;
                        let p = self.place()?;
                        ops.push(AlterOp::AddColumn(c, p));
                    }
                }
            } else if self.eat_kw("DROP") {
                if self.eat_kws(&["PRIMARY", "KEY"]) {
                    ops.push(AlterOp::DropPrimary);
                } else if self.eat_kw("INDEX") || self.eat_kw("KEY") {
                    ops.push(AlterOp::DropIndex(self.any_ident()?));
                } else if self.eat_kws(&["FOREIGN", "KEY"]) {
                    ops.push(AlterOp::DropForeign(self.any_ident()?));
                } else if self.eat_kw("CHECK") || self.eat_kw("CONSTRAINT") {
                    ops.push(AlterOp::DropCheck(self.any_ident()?));
                } else {
                    self.eat_kw("COLUMN");
                    ops.push(AlterOp::DropColumn(self.any_ident()?));
                }
            } else if self.eat_kw("MODIFY") {
                self.eat_kw("COLUMN");
                let c = self.column_spec()?;
                let p = self.place()?;
                ops.push(AlterOp::Modify(c.name.clone(), c, p));
            } else if self.eat_kw("CHANGE") {
                self.eat_kw("COLUMN");
                let old = self.any_ident()?;
                let c = self.column_spec()?;
                let p = self.place()?;
                ops.push(AlterOp::Modify(old, c, p));
            } else if self.eat_kw("RENAME") {
                if self.eat_kw("COLUMN") {
                    let a = self.any_ident()?;
                    self.want_kw("TO")?;
                    ops.push(AlterOp::RenameColumn(a, self.any_ident()?));
                } else if self.eat_kw("INDEX") || self.eat_kw("KEY") {
                    let a = self.any_ident()?;
                    self.want_kw("TO")?;
                    ops.push(AlterOp::RenameIndex(a, self.any_ident()?));
                } else {
                    if !self.eat_kw("TO") {
                        self.eat_kw("AS");
                    }
                    ops.push(AlterOp::RenameTable(self.table_name()?));
                }
            } else if self.eat_kw("ALTER") {
                if self.eat_kw("INDEX") {
                    self.any_ident()?;
                    self.i += 1;
                    ops.push(AlterOp::Ignored("index visibility".into()));
                } else if self.eat_kw("CHECK") || self.eat_kw("CONSTRAINT") {
                    self.any_ident()?;
                    self.eat_kw("NOT");
                    self.eat_kw("ENFORCED");
                    ops.push(AlterOp::Ignored("check enforcement".into()));
                } else {
                    self.eat_kw("COLUMN");
                    let c = self.any_ident()?;
                    if self.eat_kws(&["SET", "DEFAULT"]) {
                        let d = if self.eat_kw("NULL") { None } else { Some(self.default_value()?) };
                        ops.push(AlterOp::SetDefault(c, d));
                    } else if self.eat_kws(&["DROP", "DEFAULT"]) {
                        ops.push(AlterOp::SetDefault(c, None));
                    } else if self.eat_kw("SET") {
                        self.i += 1;
                        ops.push(AlterOp::Ignored("column visibility".into()));
                    } else {
                        return Err(format!("Expected SET DEFAULT or DROP DEFAULT {}", self.near()));
                    }
                }
            } else if self.eat_kw("CONVERT") {
                self.skip_to_comma();
                ops.push(AlterOp::Ignored("character set".into()));
            } else if self.eat_kw("ORDER") {
                self.want_kw("BY")?;
                self.skip_to_comma();
                ops.push(AlterOp::Ignored("ORDER BY".into()));
            } else if self.eat_kw("DISABLE") || self.eat_kw("ENABLE") {
                self.eat_kw("KEYS");
                ops.push(AlterOp::Ignored("keys".into()));
            } else if self.eat_kw("FORCE") || self.eat_kw("ALGORITHM") || self.eat_kw("LOCK") {
                if self.eat_sym("=") {
                    self.i += 1;
                }
                ops.push(AlterOp::Ignored("algorithm".into()));
            } else {
                let mut o = TableOpts::default();
                let before = self.i;
                self.table_opts(&mut o)?;
                if self.i == before {
                    return Err(format!("Unknown ALTER TABLE change {}", self.near()));
                }
                ops.push(AlterOp::Options(o));
                continue;
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        if ops.is_empty() {
            return Err("ALTER TABLE needs a change, e.g. ADD COLUMN, MODIFY, RENAME COLUMN, DROP COLUMN".into());
        }
        Ok(Stmt::Alter { table, ops })
    }

    fn skip_to_comma(&mut self) {
        let mut depth = 0;
        while let Some(t) = self.peek() {
            match t {
                Tok::Sym("(") => depth += 1,
                Tok::Sym(")") => depth -= 1,
                Tok::Sym(",") | Tok::Sym(";") if depth == 0 => break,
                _ => {}
            }
            self.i += 1;
        }
    }

    fn drop(&mut self) -> R<Stmt> {
        self.eat_kw("TEMPORARY");
        if self.eat_kw("TABLE") || self.eat_kw("TABLES") {
            let if_exists = self.eat_kws(&["IF", "EXISTS"]);
            let mut names = vec![self.table_name()?];
            while self.eat_sym(",") {
                names.push(self.table_name()?);
            }
            self.eat_kw("CASCADE");
            self.eat_kw("RESTRICT");
            return Ok(Stmt::DropTable { names, if_exists });
        }
        if self.eat_kw("VIEW") {
            let if_exists = self.eat_kws(&["IF", "EXISTS"]);
            let mut names = vec![self.table_name()?];
            while self.eat_sym(",") {
                names.push(self.table_name()?);
            }
            return Ok(Stmt::DropView { names, if_exists });
        }
        if self.eat_kw("INDEX") || self.eat_kw("KEY") {
            let name = self.any_ident()?;
            self.want_kw("ON")?;
            let table = self.table_name()?;
            return Ok(Stmt::DropIndex { name, table });
        }
        if self.eat_kw("DATABASE") || self.eat_kw("SCHEMA") {
            self.skip_rest();
            return Ok(Stmt::Unsupported("A database on IQ can't be deleted: its name and everything saved in it stay on the blockchain. Drop its tables instead, or remove it from the editor.".into()));
        }
        for kw in ["TRIGGER", "PROCEDURE", "FUNCTION", "EVENT", "USER", "ROLE"] {
            if self.eat_kw(kw) {
                self.skip_rest();
                return Ok(Stmt::Noop(format!("There are no {}s here, so there's nothing to drop.", kw.to_lowercase())));
            }
        }
        Err(format!("Expected TABLE, VIEW or INDEX after DROP {}", self.near()))
    }

    fn show(&mut self) -> R<Stmt> {
        let full = self.eat_kw("FULL");
        if self.eat_kw("TABLES") {
            if self.eat_kw("FROM") || self.eat_kw("IN") {
                self.ident()?;
            }
            let like = if self.eat_kw("LIKE") { Some(self.string()?) } else { None };
            if self.eat_kw("WHERE") {
                self.skip_rest();
            }
            return Ok(Stmt::Show(Show::Tables { full, like }));
        }
        if self.eat_kw("DATABASES") || self.eat_kw("SCHEMAS") {
            self.skip_rest();
            return Ok(Stmt::Show(Show::Databases));
        }
        if self.eat_kw("COLUMNS") || self.eat_kw("FIELDS") {
            if !self.eat_kw("FROM") {
                self.want_kw("IN")?;
            }
            let table = self.table_name()?;
            if self.eat_kw("FROM") || self.eat_kw("IN") {
                self.ident()?;
            }
            self.skip_rest();
            return Ok(Stmt::Show(Show::Columns { table, full }));
        }
        if self.eat_kw("CREATE") {
            if self.eat_kw("VIEW") {
                return Ok(Stmt::Show(Show::CreateView(self.table_name()?)));
            }
            if self.eat_kw("DATABASE") || self.eat_kw("SCHEMA") {
                self.skip_rest();
                return Ok(Stmt::Noop("Databases here have no options to show; SHOW CREATE TABLE shows a table.".into()));
            }
            self.want_kw("TABLE")?;
            return Ok(Stmt::Show(Show::CreateTable(self.table_name()?)));
        }
        if self.eat_kw("INDEX") || self.eat_kw("INDEXES") || self.eat_kw("KEYS") {
            if !self.eat_kw("FROM") {
                self.want_kw("IN")?;
            }
            let t = self.table_name()?;
            self.skip_rest();
            return Ok(Stmt::Show(Show::Index(t)));
        }
        if self.eat_kws(&["TABLE", "STATUS"]) {
            self.skip_rest();
            return Ok(Stmt::Show(Show::TableStatus));
        }
        if self.eat_kw("CHANGES") {
            return Ok(Stmt::Show(Show::Changes));
        }
        if self.eat_kw("GRANTS") || self.eat_kw("PRIVILEGES") {
            let t = if self.eat_kw("FOR") || self.eat_kw("ON") { Some(self.table_name()?) } else { None };
            self.skip_rest();
            return Ok(Stmt::Show(Show::Grants(t)));
        }
        if self.eat_kw("VARIABLES") || self.eat_kw("STATUS") {
            self.skip_rest();
            return Ok(Stmt::Show(Show::Variables));
        }
        if self.eat_kw("WARNINGS") || self.eat_kw("ERRORS") {
            return Ok(Stmt::Show(Show::Warnings));
        }
        if self.eat_kw("TRIGGERS") || self.eat_kw("EVENTS") || self.eat_kws(&["PROCEDURE", "STATUS"]) || self.eat_kws(&["FUNCTION", "STATUS"]) || self.eat_kw("PROCESSLIST") || self.eat_kw("ENGINES") || self.eat_kw("PLUGINS") {
            self.skip_rest();
            return Ok(Stmt::Show(Show::Warnings));
        }
        Err("Try SHOW TABLES, SHOW COLUMNS FROM t, SHOW CREATE TABLE t, SHOW INDEX FROM t, SHOW TABLE STATUS or SHOW CHANGES".into())
    }

    fn grant(&mut self, revoke: bool) -> R<Stmt> {
        // privilege list up to ON
        while !self.peek_kw("ON") {
            if self.peek().is_none() {
                return Err("Expected ON".into());
            }
            self.i += 1;
        }
        self.want_kw("ON")?;
        self.eat_kw("TABLE");
        let table = if self.eat_sym("*") {
            if self.eat_sym(".") {
                self.want_sym("*")?;
            }
            "*".to_string()
        } else {
            let a = self.ident()?;
            if self.eat_sym(".") {
                if self.eat_sym("*") {
                    "*".to_string()
                } else {
                    self.any_ident()?
                }
            } else {
                a
            }
        };
        if revoke {
            self.want_kw("FROM")?;
        } else {
            self.want_kw("TO")?;
        }
        let mut to = vec![];
        loop {
            let g = match self.peek().cloned() {
                Some(Tok::Word(w)) if w.eq_ignore_ascii_case("PUBLIC") => {
                    self.i += 1;
                    "PUBLIC".to_string()
                }
                _ => self.any_ident()?,
            };
            // 'user'@'host'
            if let Some(Tok::Var(_)) = self.peek() {
                self.i += 1;
            }
            to.push(g);
            if !self.eat_sym(",") {
                break;
            }
        }
        self.skip_rest();
        Ok(Stmt::Grant { table, to, revoke })
    }

    fn set(&mut self) -> R<Stmt> {
        if self.eat_kw("NAMES") || self.eat_kws(&["CHARACTER", "SET"]) || self.eat_kw("CHARSET") {
            self.skip_rest();
            return Ok(Stmt::Noop("Text is always UTF-8 here.".into()));
        }
        if self.eat_kw("TRANSACTION") {
            self.skip_rest();
            return Ok(Stmt::Noop("Transactions: your changes are held until COMMIT anyway.".into()));
        }
        let mut v = vec![];
        loop {
            self.eat_kw("SESSION");
            self.eat_kw("GLOBAL");
            self.eat_kw("LOCAL");
            let name = match self.peek().cloned() {
                Some(Tok::Var(n)) => {
                    self.i += 1;
                    n
                }
                _ => {
                    let mut n = self.any_ident()?;
                    while self.eat_sym(".") {
                        n = self.any_ident()?;
                    }
                    n.to_lowercase()
                }
            };
            if !self.eat_sym("=") {
                self.want_sym(":=")?;
            }
            v.push((name, self.expr()?));
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(Stmt::Set(v))
    }
}

pub fn func(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Func(Box::new(Call { name: name.to_string(), args, star: false, distinct: false, order: vec![], sep: None }))
}

/// SELECT with no table: one empty row.
pub fn dual() -> Query {
    Query { with: vec![], recursive: false, body: Body::Values(vec![vec![]]), order: vec![], limit: None, offset: None }
}

/// Parse a script: statements (with their source text) separated by semicolons.
pub fn parse(src: &str) -> Result<Vec<(Stmt, String)>, String> {
    Parser::new(src)?.script()
}

pub fn parse_expr(src: &str) -> Result<Expr, String> {
    let mut p = Parser::new(src)?;
    let e = p.expr()?;
    if p.peek().is_some() {
        return Err(format!("Unexpected {}", p.near()));
    }
    Ok(e)
}

pub fn parse_query(src: &str) -> Result<Query, String> {
    let mut p = Parser::new(src)?;
    let q = p.query()?;
    while p.eat_sym(";") {}
    if p.peek().is_some() {
        return Err(format!("Unexpected {}", p.near()));
    }
    Ok(q)
}
