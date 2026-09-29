//! A small SQL dialect for the editor's console, in the spirit of
//! phpMyAdmin: reads run instantly over the table (saved records plus
//! unsaved edits); writes become unsaved edits, and only COMMIT puts them on
//! the blockchain.
//!
//! SELECT [DISTINCT] cols|*|aggregates FROM t [WHERE …] [GROUP BY …]
//!        [ORDER BY … [ASC|DESC]] [LIMIT n [OFFSET m]]
//! INSERT INTO t [(cols)] VALUES (…), (…)
//! UPDATE t SET col = expr, … [WHERE …]
//! DELETE FROM t [WHERE …]
//! CREATE TABLE t (col [type] [PRIMARY KEY], …) [LOCKED|OPEN]
//! ALTER TABLE t ADD [COLUMN] c | DROP [COLUMN] c | RENAME [COLUMN] a TO b
//! DROP TABLE t · SHOW TABLES · SHOW CHANGES · DESCRIBE t · COMMIT · ROLLBACK

use crate::json::{self, Json};

// ------------------------------------------------------------------ tokens

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Word(String),  // keyword or bare identifier (original case)
    Ident(String), // `quoted identifier`
    Str(String),
    Num(String),
    Sym(&'static str),
}

pub fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = vec![];
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && b.get(i + 1) == Some(&'-') || c == '#' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == '\'' || c == '"' || c == '`' {
            let q = c;
            i += 1;
            let mut s = String::new();
            loop {
                if i >= b.len() {
                    return Err(format!("Unclosed {} quote", q));
                }
                if b[i] == q {
                    if b.get(i + 1) == Some(&q) {
                        s.push(q);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if b[i] == '\\' && q != '`' && i + 1 < b.len() {
                    i += 1;
                    s.push(match b[i] {
                        'n' => '\n',
                        't' => '\t',
                        x => x,
                    });
                    i += 1;
                    continue;
                }
                s.push(b[i]);
                i += 1;
            }
            out.push(if q == '`' { Tok::Ident(s) } else { Tok::Str(s) });
        } else if c.is_ascii_digit() || (c == '.' && b.get(i + 1).map(|d| d.is_ascii_digit()).unwrap_or(false)) {
            let st = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == '.') {
                i += 1;
            }
            if i < b.len() && (b[i] == 'e' || b[i] == 'E') && b.get(i + 1).map(|d| d.is_ascii_digit() || *d == '-' || *d == '+').unwrap_or(false) {
                i += 2;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            out.push(Tok::Num(b[st..i].iter().collect()));
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let st = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$' || (b[i] == '-' && b.get(i + 1).map(|d| d.is_alphanumeric()).unwrap_or(false) && i > st)) {
                i += 1;
            }
            out.push(Tok::Word(b[st..i].iter().collect()));
        } else {
            let two: String = b[i..(i + 2).min(b.len())].iter().collect();
            let sym = match two.as_str() {
                "<=" => Some("<="),
                ">=" => Some(">="),
                "<>" => Some("!="),
                "!=" => Some("!="),
                "==" => Some("="),
                "||" => Some("||"),
                _ => None,
            };
            if let Some(s) = sym {
                out.push(Tok::Sym(s));
                i += 2;
                continue;
            }
            let one = match c {
                '=' => "=",
                '<' => "<",
                '>' => ">",
                '(' => "(",
                ')' => ")",
                ',' => ",",
                ';' => ";",
                '*' => "*",
                '+' => "+",
                '-' => "-",
                '/' => "/",
                '%' => "%",
                '.' => ".",
                _ => return Err(format!("Unexpected character '{}'", c)),
            };
            out.push(Tok::Sym(one));
            i += 1;
        }
    }
    Ok(out)
}

// --------------------------------------------------------------------- AST

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Lit(Json),
    Col(String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(&'static str, Box<Expr>, Box<Expr>),
    Like(Box<Expr>, Box<Expr>, bool),
    In(Box<Expr>, Vec<Expr>, bool),
    IsNull(Box<Expr>, bool),
    Between(Box<Expr>, Box<Expr>, Box<Expr>, bool),
    /// name (upper case), args, COUNT(*)
    Func(String, Vec<Expr>, bool),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Star,
    Expr(Expr, Option<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Select { distinct: bool, items: Vec<Item>, table: String, filter: Option<Expr>, group: Vec<Expr>, order: Vec<(Expr, bool)>, limit: Option<usize>, offset: usize },
    Insert { table: String, cols: Option<Vec<String>>, rows: Vec<Vec<Expr>> },
    Update { table: String, sets: Vec<(String, Expr)>, filter: Option<Expr> },
    Delete { table: String, filter: Option<Expr> },
    Create { table: String, cols: Vec<String>, id: Option<String>, open: bool },
    AddColumn { table: String, col: String },
    DropColumn { table: String, col: String },
    RenameColumn { table: String, from: String, to: String },
    DropTable { table: String },
    ShowTables,
    ShowChanges,
    Describe { table: String },
    Commit,
    Rollback,
    Begin,
}

impl Stmt {
    pub fn table(&self) -> Option<&str> {
        match self {
            Stmt::Select { table, .. } | Stmt::Insert { table, .. } | Stmt::Update { table, .. } | Stmt::Delete { table, .. } | Stmt::Describe { table } => Some(table),
            Stmt::AddColumn { table, .. } | Stmt::DropColumn { table, .. } | Stmt::RenameColumn { table, .. } | Stmt::DropTable { table } => Some(table),
            _ => None,
        }
    }
    /// Needs the table's saved records loaded before it can run.
    pub fn reads_rows(&self) -> bool {
        matches!(self, Stmt::Select { .. } | Stmt::Update { .. } | Stmt::Delete { .. } | Stmt::Insert { .. })
    }
}

// ------------------------------------------------------------------ parser

struct P {
    t: Vec<Tok>,
    i: usize,
}

fn kw(t: &Tok, k: &str) -> bool {
    matches!(t, Tok::Word(w) if w.eq_ignore_ascii_case(k))
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn peek_kw(&self, k: &str) -> bool {
        self.peek().map(|t| kw(t, k)).unwrap_or(false)
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
    fn eat_sym(&mut self, s: &str) -> bool {
        if self.peek_sym(s) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn want_kw(&mut self, k: &str) -> Result<(), String> {
        if self.eat_kw(k) {
            Ok(())
        } else {
            Err(format!("Expected {} {}", k, self.near()))
        }
    }
    fn want_sym(&mut self, s: &str) -> Result<(), String> {
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
    fn ident(&mut self) -> Result<String, String> {
        match self.peek().cloned() {
            Some(Tok::Word(w)) if !reserved(&w) => {
                self.i += 1;
                Ok(w)
            }
            Some(Tok::Ident(w)) => {
                self.i += 1;
                Ok(w)
            }
            Some(Tok::Str(w)) => {
                // tolerate "quoted" names where a name is expected
                self.i += 1;
                Ok(w)
            }
            _ => Err(format!("Expected a name {}", self.near())),
        }
    }
    fn ident_list(&mut self) -> Result<Vec<String>, String> {
        let mut v = vec![self.ident()?];
        while self.eat_sym(",") {
            v.push(self.ident()?);
        }
        Ok(v)
    }

    fn stmt(&mut self) -> Result<Stmt, String> {
        let Some(first) = self.peek().cloned() else { return Err("Empty statement".into()) };
        let Tok::Word(w) = first else { return Err(format!("A statement can't start with {}", show(&first))) };
        self.i += 1;
        match w.to_ascii_uppercase().as_str() {
            "SELECT" => self.select(),
            "INSERT" => {
                self.want_kw("INTO")?;
                let table = self.ident()?;
                let cols = if self.eat_sym("(") {
                    let c = self.ident_list()?;
                    self.want_sym(")")?;
                    Some(c)
                } else {
                    None
                };
                if !self.eat_kw("VALUES") {
                    self.want_kw("VALUE")?;
                }
                let mut rows = vec![];
                loop {
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
                Ok(Stmt::Insert { table, cols, rows })
            }
            "UPDATE" => {
                let table = self.ident()?;
                self.want_kw("SET")?;
                let mut sets = vec![];
                loop {
                    let c = self.ident()?;
                    self.want_sym("=")?;
                    sets.push((c, self.expr()?));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
                Ok(Stmt::Update { table, sets, filter })
            }
            "DELETE" => {
                self.want_kw("FROM")?;
                let table = self.ident()?;
                let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
                Ok(Stmt::Delete { table, filter })
            }
            "CREATE" => {
                self.want_kw("TABLE")?;
                if self.eat_kw("IF") {
                    self.want_kw("NOT")?;
                    self.want_kw("EXISTS")?;
                }
                let table = self.ident()?;
                self.want_sym("(")?;
                let mut cols = vec![];
                let mut id = None;
                loop {
                    if self.peek_kw("PRIMARY") {
                        // table-level PRIMARY KEY (col)
                        self.i += 1;
                        self.want_kw("KEY")?;
                        self.want_sym("(")?;
                        id = Some(self.ident()?);
                        self.want_sym(")")?;
                    } else {
                        let c = self.ident()?;
                        // skip a type and constraints until , or )
                        let mut depth = 0;
                        while let Some(t) = self.peek() {
                            if depth == 0 && (matches!(t, Tok::Sym(",")) || matches!(t, Tok::Sym(")"))) {
                                break;
                            }
                            if kw(t, "PRIMARY") {
                                id = Some(c.clone());
                            }
                            if matches!(t, Tok::Sym("(")) {
                                depth += 1;
                            }
                            if matches!(t, Tok::Sym(")")) {
                                depth -= 1;
                            }
                            self.i += 1;
                        }
                        cols.push(c);
                    }
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.want_sym(")")?;
                let mut open = false;
                while let Some(t) = self.peek().cloned() {
                    if matches!(t, Tok::Sym(";")) {
                        break;
                    }
                    if kw(&t, "OPEN") {
                        open = true;
                    } else if kw(&t, "LOCKED") {
                        open = false;
                    }
                    self.i += 1; // ignore ENGINE=… and friends
                }
                Ok(Stmt::Create { table, cols, id, open })
            }
            "ALTER" => {
                self.want_kw("TABLE")?;
                let table = self.ident()?;
                if self.eat_kw("ADD") {
                    self.eat_kw("COLUMN");
                    let col = self.ident()?;
                    while self.peek().map(|t| !matches!(t, Tok::Sym(";"))).unwrap_or(false) {
                        self.i += 1; // type
                    }
                    Ok(Stmt::AddColumn { table, col })
                } else if self.eat_kw("DROP") {
                    self.eat_kw("COLUMN");
                    Ok(Stmt::DropColumn { table, col: self.ident()? })
                } else if self.eat_kw("RENAME") {
                    self.eat_kw("COLUMN");
                    let from = self.ident()?;
                    self.want_kw("TO")?;
                    Ok(Stmt::RenameColumn { table, from, to: self.ident()? })
                } else {
                    Err("ALTER TABLE supports ADD COLUMN, DROP COLUMN and RENAME COLUMN".into())
                }
            }
            "DROP" => {
                self.want_kw("TABLE")?;
                if self.eat_kw("IF") {
                    self.want_kw("EXISTS")?;
                }
                Ok(Stmt::DropTable { table: self.ident()? })
            }
            "SHOW" => {
                if self.eat_kw("TABLES") {
                    Ok(Stmt::ShowTables)
                } else if self.eat_kw("CHANGES") {
                    Ok(Stmt::ShowChanges)
                } else if self.eat_kw("COLUMNS") || self.eat_kw("FIELDS") {
                    if !self.eat_kw("FROM") {
                        self.want_kw("IN")?;
                    }
                    Ok(Stmt::Describe { table: self.ident()? })
                } else {
                    Err("Try SHOW TABLES, SHOW CHANGES or SHOW COLUMNS FROM t".into())
                }
            }
            "DESCRIBE" | "DESC" | "EXPLAIN" => Ok(Stmt::Describe { table: self.ident()? }),
            "COMMIT" | "SAVE" => Ok(Stmt::Commit),
            "ROLLBACK" => Ok(Stmt::Rollback),
            "BEGIN" | "START" => {
                self.eat_kw("TRANSACTION");
                Ok(Stmt::Begin)
            }
            "USE" => Err("Each database opens in its own editor — pick it in the sidebar.".into()),
            other => Err(format!("Unknown command {}. Try SELECT, INSERT, UPDATE, DELETE, CREATE TABLE, ALTER TABLE, SHOW TABLES, DESCRIBE or COMMIT.", other)),
        }
    }

    fn select(&mut self) -> Result<Stmt, String> {
        let distinct = self.eat_kw("DISTINCT");
        let mut items = vec![];
        loop {
            if self.eat_sym("*") {
                items.push(Item::Star);
            } else {
                let e = self.expr()?;
                let alias = if self.eat_kw("AS") {
                    Some(self.ident()?)
                } else if matches!(self.peek(), Some(Tok::Word(w)) if !reserved(w)) || matches!(self.peek(), Some(Tok::Ident(_))) {
                    Some(self.ident()?)
                } else {
                    None
                };
                items.push(Item::Expr(e, alias));
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        self.want_kw("FROM")?;
        let table = self.ident()?;
        let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        let mut group = vec![];
        if self.eat_kw("GROUP") {
            self.want_kw("BY")?;
            loop {
                group.push(self.expr()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let mut order = vec![];
        if self.eat_kw("ORDER") {
            self.want_kw("BY")?;
            loop {
                let e = self.expr()?;
                let desc = if self.eat_kw("DESC") {
                    true
                } else {
                    self.eat_kw("ASC");
                    false
                };
                order.push((e, desc));
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let (mut limit, mut offset) = (None, 0);
        if self.eat_kw("LIMIT") {
            let a = self.number()?;
            if self.eat_sym(",") {
                offset = a;
                limit = Some(self.number()?);
            } else {
                limit = Some(a);
                if self.eat_kw("OFFSET") {
                    offset = self.number()?;
                }
            }
        }
        Ok(Stmt::Select { distinct, items, table, filter, group, order, limit, offset })
    }

    fn number(&mut self) -> Result<usize, String> {
        match self.peek().cloned() {
            Some(Tok::Num(n)) => {
                self.i += 1;
                n.parse().map_err(|_| format!("Expected a whole number, got {}", n))
            }
            _ => Err(format!("Expected a number {}", self.near())),
        }
    }

    fn expr(&mut self) -> Result<Expr, String> {
        self.or()
    }
    fn or(&mut self) -> Result<Expr, String> {
        let mut l = self.and()?;
        while self.eat_kw("OR") {
            l = Expr::Bin("OR", Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }
    fn and(&mut self) -> Result<Expr, String> {
        let mut l = self.not()?;
        while self.eat_kw("AND") {
            l = Expr::Bin("AND", Box::new(l), Box::new(self.not()?));
        }
        Ok(l)
    }
    fn not(&mut self) -> Result<Expr, String> {
        if self.eat_kw("NOT") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.cmp()
    }
    fn cmp(&mut self) -> Result<Expr, String> {
        let l = self.add()?;
        for op in ["=", "!=", "<=", ">=", "<", ">"] {
            if self.eat_sym(op) {
                let r = self.add()?;
                return Ok(Expr::Bin(op, Box::new(l), Box::new(r)));
            }
        }
        if self.eat_kw("IS") {
            let not = self.eat_kw("NOT");
            self.want_kw("NULL")?;
            return Ok(Expr::IsNull(Box::new(l), not));
        }
        let not = self.eat_kw("NOT");
        if self.eat_kw("LIKE") {
            return Ok(Expr::Like(Box::new(l), Box::new(self.add()?), not));
        }
        if self.eat_kw("IN") {
            self.want_sym("(")?;
            let mut v = vec![self.expr()?];
            while self.eat_sym(",") {
                v.push(self.expr()?);
            }
            self.want_sym(")")?;
            return Ok(Expr::In(Box::new(l), v, not));
        }
        if self.eat_kw("BETWEEN") {
            let lo = self.add()?;
            self.want_kw("AND")?;
            let hi = self.add()?;
            return Ok(Expr::Between(Box::new(l), Box::new(lo), Box::new(hi), not));
        }
        if not {
            return Err(format!("Expected LIKE, IN or BETWEEN after NOT {}", self.near()));
        }
        Ok(l)
    }
    fn add(&mut self) -> Result<Expr, String> {
        let mut l = self.mul()?;
        loop {
            let op = if self.eat_sym("+") {
                "+"
            } else if self.eat_sym("-") {
                "-"
            } else if self.eat_sym("||") {
                "||"
            } else {
                break;
            };
            l = Expr::Bin(op, Box::new(l), Box::new(self.mul()?));
        }
        Ok(l)
    }
    fn mul(&mut self) -> Result<Expr, String> {
        let mut l = self.unary()?;
        loop {
            let op = if self.eat_sym("*") {
                "*"
            } else if self.eat_sym("/") {
                "/"
            } else if self.eat_sym("%") {
                "%"
            } else {
                break;
            };
            l = Expr::Bin(op, Box::new(l), Box::new(self.unary()?));
        }
        Ok(l)
    }
    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat_sym("-") {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        if self.eat_sym("+") {
            return self.unary();
        }
        self.primary()
    }
    fn primary(&mut self) -> Result<Expr, String> {
        let Some(t) = self.peek().cloned() else { return Err("Unexpected end of the statement".into()) };
        self.i += 1;
        match t {
            Tok::Num(n) => Ok(Expr::Lit(Json::Num(n))),
            Tok::Str(s) => Ok(Expr::Lit(Json::Str(s))),
            Tok::Ident(s) => Ok(Expr::Col(s)),
            Tok::Sym("(") => {
                let e = self.expr()?;
                self.want_sym(")")?;
                Ok(e)
            }
            Tok::Word(w) => {
                let up = w.to_ascii_uppercase();
                match up.as_str() {
                    "NULL" => return Ok(Expr::Lit(Json::Null)),
                    "TRUE" => return Ok(Expr::Lit(Json::Bool(true))),
                    "FALSE" => return Ok(Expr::Lit(Json::Bool(false))),
                    _ => {}
                }
                if self.eat_sym("(") {
                    if self.eat_sym("*") {
                        self.want_sym(")")?;
                        return Ok(Expr::Func(up, vec![], true));
                    }
                    self.eat_kw("DISTINCT");
                    let mut args = vec![];
                    if !self.peek_sym(")") {
                        args.push(self.expr()?);
                        while self.eat_sym(",") {
                            args.push(self.expr()?);
                        }
                    }
                    self.want_sym(")")?;
                    return Ok(Expr::Func(up, args, false));
                }
                if reserved(&w) {
                    return Err(format!("Unexpected {} — if it's a column name, put it in `backticks`", w));
                }
                // table.column → column
                if self.eat_sym(".") {
                    return Ok(Expr::Col(self.ident()?));
                }
                Ok(Expr::Col(w))
            }
            other => Err(format!("Unexpected {}", show(&other))),
        }
    }
}

fn reserved(w: &str) -> bool {
    const R: &[&str] = &[
        "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "ORDER", "BY", "GROUP", "LIMIT", "OFFSET", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "AS", "IS", "NULL", "LIKE", "IN", "BETWEEN", "ASC", "DESC", "DISTINCT", "TRUE", "FALSE", "CREATE", "TABLE", "ALTER", "DROP", "HAVING",
    ];
    R.iter().any(|r| r.eq_ignore_ascii_case(w))
}

fn show(t: &Tok) -> String {
    match t {
        Tok::Word(w) => format!("\"{}\"", w),
        Tok::Ident(w) => format!("`{}`", w),
        Tok::Str(s) => format!("'{}'", s),
        Tok::Num(n) => n.clone(),
        Tok::Sym(s) => format!("'{}'", s),
    }
}

/// Parse a script: statements separated by semicolons.
pub fn parse(src: &str) -> Result<Vec<Stmt>, String> {
    let toks = tokenize(src)?;
    let mut out = vec![];
    let mut p = P { t: toks, i: 0 };
    loop {
        while p.eat_sym(";") {}
        if p.peek().is_none() {
            break;
        }
        let s = p.stmt()?;
        if !p.eat_sym(";") && p.peek().is_some() {
            return Err(format!("Unexpected {} (missing ';' between statements?)", p.near().trim_start_matches("near ")));
        }
        out.push(s);
    }
    Ok(out)
}

// -------------------------------------------------------------- evaluation

pub fn num(v: &Json) -> Option<f64> {
    match v {
        Json::Num(n) => n.parse().ok(),
        Json::Str(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse().ok()
            }
        }
        Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

pub fn fmt_num(f: f64) -> Json {
    if !f.is_finite() {
        return Json::Null;
    }
    if f.fract() == 0.0 && f.abs() < 1e15 {
        Json::Num(format!("{}", f as i64))
    } else {
        let s = format!("{}", (f * 1e10).round() / 1e10);
        Json::Num(s)
    }
}

fn truthy(v: &Json) -> bool {
    match v {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Num(n) => n.parse::<f64>().map(|f| f != 0.0).unwrap_or(false),
        Json::Str(s) => !s.is_empty() && s != "0",
        _ => true,
    }
}

/// Spreadsheet-style comparison: numbers as numbers, text case-insensitive.
pub fn compare(a: &Json, b: &Json) -> Option<std::cmp::Ordering> {
    if a.is_null() || b.is_null() {
        return None;
    }
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => Some(a.cell_text().to_lowercase().cmp(&b.cell_text().to_lowercase())),
    }
}

fn like(text: &str, pat: &str) -> bool {
    // % = any run, _ = one character; case-insensitive
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let p: Vec<char> = pat.to_lowercase().chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

pub fn is_aggregate(e: &Expr) -> bool {
    match e {
        Expr::Func(n, args, _) => matches!(n.as_str(), "COUNT" | "SUM" | "AVG" | "MIN" | "MAX") || args.iter().any(is_aggregate),
        Expr::Neg(a) | Expr::Not(a) | Expr::IsNull(a, _) => is_aggregate(a),
        Expr::Bin(_, a, b) | Expr::Like(a, b, _) => is_aggregate(a) || is_aggregate(b),
        Expr::In(a, l, _) => is_aggregate(a) || l.iter().any(is_aggregate),
        Expr::Between(a, b, c, _) => is_aggregate(a) || is_aggregate(b) || is_aggregate(c),
        _ => false,
    }
}

/// Evaluate an expression. `col` looks a column up in the current row;
/// `group` holds the rows of the current group for aggregates.
pub fn eval(e: &Expr, col: &dyn Fn(&str) -> Result<Json, String>, group: Option<&[&dyn Fn(&str) -> Result<Json, String>]>) -> Result<Json, String> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Col(c) => col(c)?,
        Expr::Neg(a) => match num(&eval(a, col, group)?) {
            Some(f) => fmt_num(-f),
            None => Json::Null,
        },
        Expr::Not(a) => {
            let v = eval(a, col, group)?;
            if v.is_null() {
                Json::Null
            } else {
                Json::Bool(!truthy(&v))
            }
        }
        Expr::Bin(op, a, b) => {
            let x = eval(a, col, group)?;
            match *op {
                "AND" => {
                    if !x.is_null() && !truthy(&x) {
                        return Ok(Json::Bool(false));
                    }
                    let y = eval(b, col, group)?;
                    if x.is_null() || y.is_null() {
                        if !y.is_null() && !truthy(&y) {
                            Json::Bool(false)
                        } else {
                            Json::Null
                        }
                    } else {
                        Json::Bool(truthy(&y))
                    }
                }
                "OR" => {
                    if truthy(&x) {
                        return Ok(Json::Bool(true));
                    }
                    let y = eval(b, col, group)?;
                    if truthy(&y) {
                        Json::Bool(true)
                    } else if x.is_null() || y.is_null() {
                        Json::Null
                    } else {
                        Json::Bool(false)
                    }
                }
                _ => {
                    let y = eval(b, col, group)?;
                    match *op {
                        "=" | "!=" | "<" | ">" | "<=" | ">=" => match compare(&x, &y) {
                            None => Json::Null,
                            Some(o) => Json::Bool(match *op {
                                "=" => o.is_eq(),
                                "!=" => o.is_ne(),
                                "<" => o.is_lt(),
                                ">" => o.is_gt(),
                                "<=" => o.is_le(),
                                _ => o.is_ge(),
                            }),
                        },
                        "||" => {
                            if x.is_null() || y.is_null() {
                                Json::Null
                            } else {
                                Json::Str(x.cell_text() + &y.cell_text())
                            }
                        }
                        _ => match (num(&x), num(&y)) {
                            (Some(p), Some(q)) => match *op {
                                "+" => fmt_num(p + q),
                                "-" => fmt_num(p - q),
                                "*" => fmt_num(p * q),
                                "/" => {
                                    if q == 0.0 {
                                        Json::Null
                                    } else {
                                        fmt_num(p / q)
                                    }
                                }
                                _ => {
                                    if q == 0.0 {
                                        Json::Null
                                    } else {
                                        fmt_num(p % q)
                                    }
                                }
                            },
                            _ => Json::Null,
                        },
                    }
                }
            }
        }
        Expr::Like(a, p, not) => {
            let x = eval(a, col, group)?;
            let y = eval(p, col, group)?;
            if x.is_null() || y.is_null() {
                Json::Null
            } else {
                Json::Bool(like(&x.cell_text(), &y.cell_text()) != *not)
            }
        }
        Expr::In(a, list, not) => {
            let x = eval(a, col, group)?;
            if x.is_null() {
                return Ok(Json::Null);
            }
            let mut hit = false;
            for l in list {
                if compare(&x, &eval(l, col, group)?).map(|o| o.is_eq()).unwrap_or(false) {
                    hit = true;
                    break;
                }
            }
            Json::Bool(hit != *not)
        }
        Expr::IsNull(a, not) => {
            let x = eval(a, col, group)?;
            let null = x.is_null() || x.cell_text().is_empty();
            Json::Bool(null != *not)
        }
        Expr::Between(a, lo, hi, not) => {
            let x = eval(a, col, group)?;
            let l = eval(lo, col, group)?;
            let h = eval(hi, col, group)?;
            match (compare(&x, &l), compare(&x, &h)) {
                (Some(p), Some(q)) => Json::Bool((p.is_ge() && q.is_le()) != *not),
                _ => Json::Null,
            }
        }
        Expr::Func(name, args, star) => match name.as_str() {
            "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" => {
                let Some(rows) = group else { return Err(format!("{}() needs rows to work on", name)) };
                let mut vals = vec![];
                for r in rows {
                    if *star {
                        vals.push(Json::Bool(true));
                    } else if let Some(a) = args.first() {
                        let v = eval(a, *r, None)?;
                        if !v.is_null() && !(matches!(v, Json::Str(ref s) if s.is_empty())) {
                            vals.push(v);
                        }
                    }
                }
                match name.as_str() {
                    "COUNT" => fmt_num(vals.len() as f64),
                    "SUM" => fmt_num(vals.iter().filter_map(num).sum()),
                    "AVG" => {
                        let n: Vec<f64> = vals.iter().filter_map(num).collect();
                        if n.is_empty() {
                            Json::Null
                        } else {
                            fmt_num(n.iter().sum::<f64>() / n.len() as f64)
                        }
                    }
                    "MIN" => vals.into_iter().min_by(|a, b| compare(a, b).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or(Json::Null),
                    _ => vals.into_iter().max_by(|a, b| compare(a, b).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or(Json::Null),
                }
            }
            "LOWER" | "UPPER" | "LENGTH" | "TRIM" | "ROUND" | "ABS" | "COALESCE" | "IFNULL" | "CONCAT" => {
                let vs: Vec<Json> = args.iter().map(|a| eval(a, col, group)).collect::<Result<_, _>>()?;
                let first = vs.first().cloned().unwrap_or(Json::Null);
                match name.as_str() {
                    "LOWER" => json::s(&first.cell_text().to_lowercase()),
                    "UPPER" => json::s(&first.cell_text().to_uppercase()),
                    "TRIM" => json::s(first.cell_text().trim()),
                    "LENGTH" => {
                        if first.is_null() {
                            Json::Null
                        } else {
                            fmt_num(first.cell_text().chars().count() as f64)
                        }
                    }
                    "ABS" => num(&first).map(|f| fmt_num(f.abs())).unwrap_or(Json::Null),
                    "ROUND" => {
                        let d = vs.get(1).and_then(num).unwrap_or(0.0) as i32;
                        num(&first).map(|f| fmt_num((f * 10f64.powi(d)).round() / 10f64.powi(d))).unwrap_or(Json::Null)
                    }
                    "COALESCE" | "IFNULL" => vs.into_iter().find(|v| !v.is_null()).unwrap_or(Json::Null),
                    _ => json::s(&vs.iter().map(|v| v.cell_text()).collect::<String>()),
                }
            }
            other => return Err(format!("Unknown function {}()", other)),
        },
    })
}

/// The column a plain `SELECT expr` shows as its header.
pub fn label(e: &Expr) -> String {
    match e {
        Expr::Col(c) => c.clone(),
        Expr::Lit(v) => v.cell_text(),
        Expr::Func(n, a, star) => format!("{}({})", n, if *star { "*".to_string() } else { a.iter().map(label).collect::<Vec<_>>().join(", ") }),
        Expr::Bin(op, a, b) => format!("{} {} {}", label(a), op, label(b)),
        Expr::Neg(a) => format!("-{}", label(a)),
        _ => "expr".into(),
    }
}

