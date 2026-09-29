//! Syntax tree for the SQL dialect.

use crate::json::Json;
use crate::schema::{RefAction, Ty};

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Lit(Json),
    /// [table.]column
    Col(Option<String>, String),
    /// @variable
    Var(String),
    /// DEFAULT (in INSERT values / UPDATE SET)
    Default,
    /// VALUES(col) inside ON DUPLICATE KEY UPDATE
    Values(String),
    Unary(&'static str, Box<Expr>),
    Bin(&'static str, Box<Expr>, Box<Expr>),
    Like { e: Box<Expr>, pat: Box<Expr>, esc: Option<Box<Expr>>, not: bool },
    Regexp { e: Box<Expr>, pat: Box<Expr>, not: bool },
    In { e: Box<Expr>, list: Vec<Expr>, not: bool },
    InQuery { e: Box<Expr>, q: Box<Query>, not: bool },
    /// e op ANY/ALL (subquery)
    Quantified { e: Box<Expr>, op: &'static str, all: bool, q: Box<Query> },
    Exists(Box<Query>, bool),
    Scalar(Box<Query>),
    IsNull(Box<Expr>, bool),
    /// IS [NOT] TRUE / FALSE
    IsBool(Box<Expr>, bool, bool),
    Between { e: Box<Expr>, lo: Box<Expr>, hi: Box<Expr>, not: bool },
    Case { operand: Option<Box<Expr>>, whens: Vec<(Expr, Expr)>, other: Option<Box<Expr>> },
    Cast(Box<Expr>, CastTo),
    /// INTERVAL n unit
    Interval(Box<Expr>, String),
    /// Function or aggregate call (name upper case).
    Func(Box<Call>),
    /// Window function: call OVER (…)
    Window(Box<Call>, Box<Over>),
    /// (a, b) row constructor
    Row(Vec<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub name: String,
    pub args: Vec<Expr>,
    pub star: bool,
    pub distinct: bool,
    /// GROUP_CONCAT(… ORDER BY …)
    pub order: Vec<Order>,
    /// GROUP_CONCAT(… SEPARATOR 'x')
    pub sep: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Over {
    pub partition: Vec<Expr>,
    pub order: Vec<Order>,
    /// ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING etc.:
    /// Some(true) = whole partition, Some(false) = running, None = default.
    pub whole: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CastTo {
    Signed,
    Unsigned,
    Decimal(u32, u32),
    Double,
    Char,
    Date,
    DateTime,
    Time,
    Json,
    Binary,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Order {
    pub e: Expr,
    pub desc: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SelItem {
    /// * or t.*
    Star(Option<String>),
    /// expression, alias, label (the source text)
    Expr(Expr, Option<String>, String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum From {
    Table { name: String, alias: Option<String> },
    Sub { q: Box<Query>, alias: String },
    Join { left: Box<From>, right: Box<From>, kind: JoinKind, on: Option<Expr>, using: Vec<String>, natural: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Cross,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<SelItem>,
    pub from: Option<From>,
    pub filter: Option<Expr>,
    pub group: Vec<Expr>,
    pub rollup: bool,
    pub having: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SetOp {
    Union,
    UnionAll,
    Intersect,
    Except,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Select(Box<Select>),
    Set(SetOp, Box<Body>, Box<Body>),
    /// A parenthesised query used as an operand of a set operation.
    Paren(Box<Query>),
    /// VALUES ROW(…), ROW(…) / VALUES (…), (…)
    Values(Vec<Vec<Expr>>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cte {
    pub name: String,
    pub cols: Vec<String>,
    pub q: Query,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub with: Vec<Cte>,
    pub recursive: bool,
    pub body: Body,
    pub order: Vec<Order>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

// -------------------------------------------------------------- statements

#[derive(Clone, Debug, PartialEq)]
pub enum DefaultSpec {
    Lit(Json),
    /// Expression text, e.g. "CURRENT_TIMESTAMP", "(UUID())"
    Expr(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSpec {
    pub name: String,
    pub ty: Ty,
    /// Some(true) = NOT NULL, Some(false) = NULL, None = not said.
    pub not_null: Option<bool>,
    pub default: Option<Option<DefaultSpec>>,
    pub on_update_now: bool,
    pub auto_inc: bool,
    pub unique: bool,
    pub primary: bool,
    pub comment: Option<String>,
    pub references: Option<RefSpec>,
    pub check: Option<(Option<String>, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RefSpec {
    pub table: String,
    pub cols: Vec<String>,
    pub on_delete: RefAction,
    pub on_update: RefAction,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Constraint {
    Primary(Vec<String>),
    Unique(Option<String>, Vec<String>),
    Index(Option<String>, Vec<String>),
    Foreign(Option<String>, Vec<String>, RefSpec),
    /// name, expression text
    Check(Option<String>, String),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct TableOpts {
    pub comment: Option<String>,
    pub auto_increment: Option<u64>,
    /// OPEN (anyone may add rows) / LOCKED
    pub open: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    First,
    After(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum AlterOp {
    AddColumn(ColumnSpec, Option<Place>),
    AddConstraint(Constraint),
    DropColumn(String),
    DropIndex(String),
    DropPrimary,
    DropForeign(String),
    DropCheck(String),
    /// Modify(old name, new definition, place) — CHANGE and MODIFY
    Modify(String, ColumnSpec, Option<Place>),
    RenameColumn(String, String),
    RenameIndex(String, String),
    RenameTable(String),
    SetDefault(String, Option<DefaultSpec>),
    Options(TableOpts),
    /// ENGINE=…, CONVERT TO CHARACTER SET … — nothing to do
    Ignored(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum InsertSrc {
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
    Set(Vec<(String, Expr)>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Show {
    Tables { full: bool, like: Option<String> },
    Databases,
    Columns { table: String, full: bool },
    CreateTable(String),
    CreateView(String),
    Index(String),
    TableStatus,
    Changes,
    Grants(Option<String>),
    Variables,
    Warnings,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Query(Query),
    Insert { table: String, cols: Option<Vec<String>>, src: InsertSrc, ignore: bool, replace: bool, on_dup: Vec<(String, Expr)> },
    Update { from: From, sets: Vec<(Option<String>, String, Expr)>, filter: Option<Expr>, order: Vec<Order>, limit: Option<Expr> },
    Delete { targets: Vec<String>, from: From, filter: Option<Expr>, order: Vec<Order>, limit: Option<Expr> },
    CreateTable { name: String, if_not_exists: bool, cols: Vec<ColumnSpec>, cons: Vec<Constraint>, opts: TableOpts, like: Option<String>, query: Option<Box<Query>> },
    Alter { table: String, ops: Vec<AlterOp> },
    DropTable { names: Vec<String>, if_exists: bool },
    RenameTable(Vec<(String, String)>),
    Truncate(String),
    /// OPTIMIZE TABLE a, b: write a checkpoint for each on the next save.
    Optimize(Vec<String>),
    CreateView { name: String, or_replace: bool, cols: Vec<String>, sql: String, q: Box<Query> },
    DropView { names: Vec<String>, if_exists: bool },
    CreateIndex { name: String, table: String, cols: Vec<String>, unique: bool },
    DropIndex { name: String, table: String },
    CreateDatabase { name: String, if_not_exists: bool },
    Use(String),
    Show(Show),
    Describe(String),
    Explain(Box<Stmt>),
    /// GRANT/REVOKE writing rights: table, grantee wallets ("PUBLIC" = anyone)
    Grant { table: String, to: Vec<String>, revoke: bool },
    /// SET @x = …, SET FOREIGN_KEY_CHECKS = 0, …
    Set(Vec<(String, Expr)>),
    Begin,
    Commit,
    Rollback,
    /// Maintenance commands that have nothing to do here (OPTIMIZE, LOCK TABLES, …)
    Noop(String),
    /// Server-side features that don't exist on IQ (triggers, procedures, users…)
    Unsupported(String),
}
