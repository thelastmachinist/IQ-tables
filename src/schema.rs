//! Column types, constraints and the table structure record.
//!
//! Every column has a display name and a *storage key*, the name its values
//! are written under in packs. The key never changes, so renaming a column,
//! reordering columns or changing a type is only a new structure record on
//! chain (one small write) — the rows already saved are read through it.
//! Dropped keys are retired and never reused, so a new column with an old
//! name starts empty, as in MySQL.
//!
//! The structure record ("IQT1s…" pack) also carries two events that apply
//! to rows saved before it: `clr` (TRUNCATE / DROP: earlier rows are gone)
//! and a change of the primary-key column (earlier rows are re-keyed by it).

use crate::dates;
use crate::json::{self, Json};

// ------------------------------------------------------------------- types

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IntKind {
    Tiny,
    Small,
    Medium,
    Int,
    Big,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextKind {
    Tiny,
    Text,
    Medium,
    Long,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    /// No declared type (tables made before types existed, or on purpose).
    Any,
    Int(IntKind, bool),
    Bool,
    /// precision, scale, unsigned
    Decimal(u32, u32, bool),
    /// double, unsigned
    Float(bool, bool),
    Char(u32),
    Varchar(u32),
    Text(TextKind),
    Date,
    DateTime(u32),
    Timestamp(u32),
    Time(u32),
    Year,
    Json,
    Enum(Vec<String>),
    Set(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeArg {
    Num(u32),
    Str(String),
}

impl Ty {
    /// Build a type from its name, arguments and UNSIGNED flag.
    pub fn from_parts(name: &str, args: &[TypeArg], unsigned: bool) -> Result<Ty, String> {
        let n = |i: usize| match args.get(i) {
            Some(TypeArg::Num(v)) => Some(*v),
            _ => None,
        };
        let strs = || -> Result<Vec<String>, String> {
            let mut v = vec![];
            for a in args {
                match a {
                    TypeArg::Str(s) => {
                        if !v.contains(s) {
                            v.push(s.clone())
                        }
                    }
                    TypeArg::Num(x) => v.push(x.to_string()),
                }
            }
            if v.is_empty() {
                return Err(format!("{} needs a list of values, e.g. {}('small','large')", name, name));
            }
            Ok(v)
        };
        let up = name.to_ascii_uppercase();
        Ok(match up.as_str() {
            "TINYINT" if n(0) == Some(1) && !unsigned => Ty::Bool,
            "TINYINT" | "INT1" => Ty::Int(IntKind::Tiny, unsigned),
            "SMALLINT" | "INT2" => Ty::Int(IntKind::Small, unsigned),
            "MEDIUMINT" | "INT3" | "MIDDLEINT" => Ty::Int(IntKind::Medium, unsigned),
            "INT" | "INTEGER" | "INT4" => Ty::Int(IntKind::Int, unsigned),
            "BIGINT" | "INT8" | "SERIAL" => Ty::Int(IntKind::Big, unsigned || up == "SERIAL"),
            "BOOL" | "BOOLEAN" | "BIT" => Ty::Bool,
            "DECIMAL" | "NUMERIC" | "DEC" | "FIXED" => {
                let p = n(0).unwrap_or(10);
                let s = n(1).unwrap_or(0);
                if p == 0 || p > 65 || s > 30 || s > p {
                    return Err(format!("DECIMAL({},{}) isn't valid: up to 65 digits, up to 30 after the point", p, s));
                }
                Ty::Decimal(p, s, unsigned)
            }
            "FLOAT" | "FLOAT4" => Ty::Float(n(0).map(|p| p > 24).unwrap_or(false), unsigned),
            "DOUBLE" | "REAL" | "FLOAT8" => Ty::Float(true, unsigned),
            "CHAR" | "CHARACTER" | "NCHAR" | "BINARY" => Ty::Char(n(0).unwrap_or(1).clamp(1, 255)),
            "VARCHAR" | "NVARCHAR" | "VARBINARY" | "CHARACTER VARYING" => Ty::Varchar(n(0).ok_or("VARCHAR needs a length, e.g. VARCHAR(255)")?.clamp(1, 65535)),
            "TINYTEXT" | "TINYBLOB" => Ty::Text(TextKind::Tiny),
            "TEXT" | "BLOB" => Ty::Text(TextKind::Text),
            "MEDIUMTEXT" | "MEDIUMBLOB" | "LONG" => Ty::Text(TextKind::Medium),
            "LONGTEXT" | "LONGBLOB" => Ty::Text(TextKind::Long),
            "DATE" => Ty::Date,
            "DATETIME" => Ty::DateTime(n(0).unwrap_or(0).min(6)),
            "TIMESTAMP" => Ty::Timestamp(n(0).unwrap_or(0).min(6)),
            "TIME" => Ty::Time(n(0).unwrap_or(0).min(6)),
            "YEAR" => Ty::Year,
            "JSON" => Ty::Json,
            "ENUM" => Ty::Enum(strs()?),
            "SET" => Ty::Set(strs()?),
            "ANY" => Ty::Any,
            other => return Err(format!("Unknown column type {}", other)),
        })
    }

    /// Parse a type written as text, e.g. "VARCHAR(255)", "ENUM('a','b')".
    pub fn parse(s: &str) -> Result<Ty, String> {
        let s = s.trim();
        let (head, rest) = match s.find('(') {
            Some(p) => (&s[..p], &s[p..]),
            None => (s, ""),
        };
        let mut words: Vec<String> = head.split_whitespace().map(|w| w.to_ascii_uppercase()).collect();
        let mut unsigned = false;
        words.retain(|w| {
            if w == "UNSIGNED" {
                unsigned = true;
                false
            } else {
                w != "ZEROFILL" && w != "SIGNED"
            }
        });
        let name =
            if words.len() >= 2 && words[0] == "DOUBLE" && words[1] == "PRECISION" { "DOUBLE".to_string() } else { words.first().cloned().unwrap_or_default() };
        let mut args = vec![];
        if !rest.is_empty() {
            let b: Vec<char> = rest.chars().collect();
            let mut i = 1;
            let mut depth_end = None;
            while i < b.len() {
                let c = b[i];
                if c == ')' {
                    depth_end = Some(i);
                    break;
                } else if c == '\'' || c == '"' {
                    let q = c;
                    i += 1;
                    let mut v = String::new();
                    while i < b.len() {
                        if b[i] == q {
                            if b.get(i + 1) == Some(&q) {
                                v.push(q);
                                i += 2;
                                continue;
                            }
                            break;
                        }
                        if b[i] == '\\' && i + 1 < b.len() {
                            i += 1;
                        }
                        v.push(b[i]);
                        i += 1;
                    }
                    args.push(TypeArg::Str(v));
                    i += 1;
                } else if c.is_ascii_digit() {
                    let st = i;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    args.push(TypeArg::Num(b[st..i].iter().collect::<String>().parse().unwrap_or(0)));
                } else {
                    i += 1;
                }
            }
            if let Some(e) = depth_end {
                if b[e + 1..].iter().collect::<String>().to_ascii_uppercase().contains("UNSIGNED") {
                    unsigned = true;
                }
            }
        }
        if name.is_empty() {
            return Err("Missing type".into());
        }
        Ty::from_parts(&name, &args, unsigned)
    }

    /// The type as SQL, e.g. "VARCHAR(255)".
    pub fn sql(&self) -> String {
        let u = |b: bool| if b { " UNSIGNED" } else { "" };
        let list = |v: &[String]| v.iter().map(|s| format!("'{}'", s.replace('\'', "''"))).collect::<Vec<_>>().join(",");
        match self {
            Ty::Any => "ANY".into(),
            Ty::Int(k, un) => format!(
                "{}{}",
                match k {
                    IntKind::Tiny => "TINYINT",
                    IntKind::Small => "SMALLINT",
                    IntKind::Medium => "MEDIUMINT",
                    IntKind::Int => "INT",
                    IntKind::Big => "BIGINT",
                },
                u(*un)
            ),
            Ty::Bool => "BOOLEAN".into(),
            Ty::Decimal(p, s, un) => format!("DECIMAL({},{}){}", p, s, u(*un)),
            Ty::Float(d, un) => format!("{}{}", if *d { "DOUBLE" } else { "FLOAT" }, u(*un)),
            Ty::Char(n) => format!("CHAR({})", n),
            Ty::Varchar(n) => format!("VARCHAR({})", n),
            Ty::Text(k) => match k {
                TextKind::Tiny => "TINYTEXT",
                TextKind::Text => "TEXT",
                TextKind::Medium => "MEDIUMTEXT",
                TextKind::Long => "LONGTEXT",
            }
            .into(),
            Ty::Date => "DATE".into(),
            Ty::DateTime(f) => {
                if *f > 0 {
                    format!("DATETIME({})", f)
                } else {
                    "DATETIME".into()
                }
            }
            Ty::Timestamp(f) => {
                if *f > 0 {
                    format!("TIMESTAMP({})", f)
                } else {
                    "TIMESTAMP".into()
                }
            }
            Ty::Time(f) => {
                if *f > 0 {
                    format!("TIME({})", f)
                } else {
                    "TIME".into()
                }
            }
            Ty::Year => "YEAR".into(),
            Ty::Json => "JSON".into(),
            Ty::Enum(v) => format!("ENUM({})", list(v)),
            Ty::Set(v) => format!("SET({})", list(v)),
        }
    }

    /// Plain-language name for people who don't speak SQL.
    pub fn friendly(&self) -> String {
        match self {
            Ty::Any => "Anything".into(),
            Ty::Int(..) => "Whole number".into(),
            Ty::Bool => "Yes / No".into(),
            Ty::Decimal(_, s, _) => {
                if *s == 0 {
                    "Whole number".into()
                } else {
                    format!("Number ({} decimal{})", s, if *s == 1 { "" } else { "s" })
                }
            }
            Ty::Float(..) => "Number".into(),
            Ty::Char(n) | Ty::Varchar(n) => format!("Text (up to {})", n),
            Ty::Text(_) => "Long text".into(),
            Ty::Date => "Date".into(),
            Ty::DateTime(_) | Ty::Timestamp(_) => "Date & time".into(),
            Ty::Time(_) => "Time".into(),
            Ty::Year => "Year".into(),
            Ty::Json => "JSON".into(),
            Ty::Enum(v) => format!("Choice: {}", v.join(", ")),
            Ty::Set(v) => format!("Choices: {}", v.join(", ")),
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int(..) | Ty::Decimal(..) | Ty::Float(..) | Ty::Year)
    }
    pub fn is_int(&self) -> bool {
        matches!(self, Ty::Int(..))
    }
    pub fn is_text(&self) -> bool {
        matches!(self, Ty::Char(_) | Ty::Varchar(_) | Ty::Text(_) | Ty::Enum(_) | Ty::Set(_))
    }
    pub fn is_temporal(&self) -> bool {
        matches!(self, Ty::Date | Ty::DateTime(_) | Ty::Timestamp(_) | Ty::Time(_) | Ty::Year)
    }

    fn int_range(&self) -> (i128, i128) {
        match self {
            Ty::Int(k, un) => {
                let bits = match k {
                    IntKind::Tiny => 8,
                    IntKind::Small => 16,
                    IntKind::Medium => 24,
                    IntKind::Int => 32,
                    IntKind::Big => 64,
                };
                if *un {
                    (0, (1i128 << bits) - 1)
                } else {
                    (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
                }
            }
            _ => (i128::MIN, i128::MAX),
        }
    }

    /// Turn a value into this type's stored form, or say why it doesn't fit.
    /// NULL stays NULL (NOT NULL is checked separately).
    pub fn coerce(&self, v: &Json) -> Result<Json, String> {
        if v.is_null() {
            return Ok(Json::Null);
        }
        let shown = || {
            let t = v.cell_text();
            if t.chars().count() > 40 {
                format!("“{}…”", t.chars().take(40).collect::<String>())
            } else {
                format!("“{}”", t)
            }
        };
        match self {
            Ty::Any => Ok(v.clone()),
            Ty::Int(..) => {
                let t = numeric_text(v).ok_or_else(|| format!("{} isn't a whole number", shown()))?;
                let r = round_dec(&t, 0).ok_or_else(|| format!("{} isn't a whole number", shown()))?;
                let i: i128 = r.parse().map_err(|_| format!("{} is too big", shown()))?;
                let (lo, hi) = self.int_range();
                if i < lo || i > hi {
                    return Err(format!("{} is out of range ({} to {})", shown(), lo, hi));
                }
                Ok(Json::Num(i.to_string()))
            }
            Ty::Decimal(p, s, un) => {
                let t = numeric_text(v).ok_or_else(|| format!("{} isn't a number", shown()))?;
                let r = round_dec(&t, *s).ok_or_else(|| format!("{} isn't a number", shown()))?;
                let int_digits = r.trim_start_matches('-').split('.').next().unwrap_or("").trim_start_matches('0').len() as u32;
                if int_digits > p - s {
                    return Err(format!("{} is too big for DECIMAL({},{})", shown(), p, s));
                }
                if *un && r.starts_with('-') {
                    return Err(format!("{} can't be negative", shown()));
                }
                Ok(Json::Num(r))
            }
            Ty::Float(_, un) => {
                let t = numeric_text(v).ok_or_else(|| format!("{} isn't a number", shown()))?;
                let f: f64 = t.parse().map_err(|_| format!("{} isn't a number", shown()))?;
                if !f.is_finite() {
                    return Err(format!("{} isn't a number", shown()));
                }
                if *un && f < 0.0 {
                    return Err(format!("{} can't be negative", shown()));
                }
                Ok(Json::Num(fmt_f64(f)))
            }
            Ty::Bool => match v {
                Json::Bool(b) => Ok(Json::Bool(*b)),
                Json::Num(n) => Ok(Json::Bool(n.parse::<f64>().map(|f| f != 0.0).unwrap_or(true))),
                Json::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" | "y" | "on" | "x" | "✓" | "✔" | "t" => Ok(Json::Bool(true)),
                    "0" | "false" | "no" | "n" | "off" | "" | "f" | "✗" => Ok(Json::Bool(false)),
                    _ => Err(format!("{} isn't yes or no", shown())),
                },
                _ => Err(format!("{} isn't yes or no", shown())),
            },
            Ty::Char(n) | Ty::Varchar(n) => {
                let t = scalar_text(v);
                if t.chars().count() > *n as usize {
                    return Err(format!("{} is too long (at most {} characters)", shown(), n));
                }
                Ok(Json::Str(t))
            }
            Ty::Text(k) => {
                let t = scalar_text(v);
                let max = match k {
                    TextKind::Tiny => 255,
                    TextKind::Text => 65_535,
                    TextKind::Medium => 16_777_215,
                    TextKind::Long => usize::MAX,
                };
                if t.len() > max {
                    return Err(format!("{} is too long for this column", shown()));
                }
                Ok(Json::Str(t))
            }
            Ty::Date => {
                let t = scalar_text(v);
                match dates::parse_datetime(&t) {
                    Some(d) => Ok(Json::Str(d.date_str())),
                    None => Err(format!("{} isn't a date (try 2026-09-28 or 9/28/2026)", shown())),
                }
            }
            Ty::DateTime(f) | Ty::Timestamp(f) => {
                let t = scalar_text(v);
                match dates::parse_datetime(&t) {
                    Some(mut d) => {
                        if *f == 0 {
                            d.micros = 0;
                        } else {
                            let k = 10u32.pow(6 - f);
                            d.micros = d.micros / k * k;
                        }
                        Ok(Json::Str(d.datetime_str()))
                    }
                    None => Err(format!("{} isn't a date and time (try 2026-09-28 14:30)", shown())),
                }
            }
            Ty::Time(_) => {
                let t = scalar_text(v);
                match dates::parse_time(&t) {
                    Some((neg, s, us)) => Ok(Json::Str(dates::time_str(neg, s, us))),
                    None => Err(format!("{} isn't a time (try 14:30 or 2:30 PM)", shown())),
                }
            }
            Ty::Year => {
                let t = numeric_text(v).ok_or_else(|| format!("{} isn't a year", shown()))?;
                let y: i64 = round_dec(&t, 0).and_then(|r| r.parse().ok()).ok_or_else(|| format!("{} isn't a year", shown()))?;
                let y = match y {
                    0 => 0,
                    1..=69 => 2000 + y,
                    70..=99 => 1900 + y,
                    1901..=2155 => y,
                    _ => return Err(format!("{} isn't a year between 1901 and 2155", shown())),
                };
                Ok(Json::Num(y.to_string()))
            }
            Ty::Json => match v {
                Json::Str(s) => json::parse(s).map_err(|_| format!("{} isn't valid JSON", shown())),
                other => Ok(other.clone()),
            },
            Ty::Enum(list) => {
                let t = scalar_text(v);
                list.iter()
                    .find(|x| x.eq_ignore_ascii_case(t.trim()))
                    .map(|x| Json::Str(x.clone()))
                    .ok_or_else(|| format!("{} isn't one of the choices: {}", shown(), list.join(", ")))
            }
            Ty::Set(list) => {
                let t = scalar_text(v);
                let mut picked = vec![false; list.len()];
                for part in t.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
                    match list.iter().position(|x| x.eq_ignore_ascii_case(part)) {
                        Some(i) => picked[i] = true,
                        None => return Err(format!("“{}” isn't one of the choices: {}", part, list.join(", "))),
                    }
                }
                Ok(Json::Str(list.iter().zip(picked).filter(|(_, p)| *p).map(|(x, _)| x.as_str()).collect::<Vec<_>>().join(",")))
            }
        }
    }

    /// Read side: show what the value would be under this type, leaving
    /// values that don't convert as they were.
    pub fn read(&self, v: &Json) -> Json {
        if matches!(self, Ty::Any) || v.is_null() {
            return v.clone();
        }
        self.coerce(v).unwrap_or_else(|_| v.clone())
    }

    /// Text for a spreadsheet cell.
    pub fn show(&self, v: &Json) -> String {
        match (self, v) {
            (_, Json::Null) => String::new(),
            (Ty::Bool, Json::Bool(b)) => (if *b { "Yes" } else { "No" }).into(),
            _ => v.cell_text(),
        }
    }
}

fn scalar_text(v: &Json) -> String {
    match v {
        Json::Bool(b) => (if *b { "1" } else { "0" }).into(),
        other => other.cell_text(),
    }
}

/// Number-looking text: accepts "1,234.50", "$12", " -3 ", "1e3", booleans.
pub fn numeric_text(v: &Json) -> Option<String> {
    let raw = match v {
        Json::Num(n) => return Some(n.clone()),
        Json::Bool(b) => return Some((if *b { "1" } else { "0" }).into()),
        Json::Str(s) => s.trim().to_string(),
        _ => return None,
    };
    let (neg, body) = match raw.strip_prefix('-') {
        Some(r) => (true, r.trim_start()),
        None => (false, raw.strip_prefix('+').unwrap_or(&raw)),
    };
    let body = body.trim_start_matches(['$', '€', '£', '¥']).trim();
    // thousands separators: 1,234,567.89
    let clean: String = if body.contains(',') {
        let (int, frac) = match body.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (body, None),
        };
        let groups: Vec<&str> = int.split(',').collect();
        let ok = groups.len() > 1
            && !groups[0].is_empty()
            && groups[0].len() <= 3
            && groups[1..].iter().all(|g| g.len() == 3)
            && groups.iter().all(|g| g.bytes().all(|c| c.is_ascii_digit()));
        if !ok {
            return None;
        }
        let mut s = groups.concat();
        if let Some(f) = frac {
            s.push('.');
            s.push_str(f);
        }
        s
    } else {
        body.to_string()
    };
    if clean.is_empty() || clean.parse::<f64>().is_err() || clean.contains(|c: char| c.is_alphabetic() && c != 'e' && c != 'E') {
        return None;
    }
    Some(if neg { format!("-{}", clean) } else { clean })
}

/// Round decimal text to `scale` places (half away from zero), exactly.
pub fn round_dec(t: &str, scale: u32) -> Option<String> {
    let t = t.trim();
    if t.contains(['e', 'E']) {
        let f: f64 = t.parse().ok()?;
        if !f.is_finite() || f.abs() > 1e30 {
            return None;
        }
        return round_dec(&format!("{:.*}", (scale as usize + 2).min(40), f), scale);
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int, frac) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    if (int.is_empty() && frac.is_empty()) || !int.bytes().all(|c| c.is_ascii_digit()) || !frac.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut digits: Vec<u8> = int.bytes().map(|c| c - b'0').collect();
    let s = scale as usize;
    let mut f: Vec<u8> = frac.bytes().map(|c| c - b'0').collect();
    let round_up = f.get(s).map(|&d| d >= 5).unwrap_or(false);
    f.resize(s, 0);
    digits.extend(f);
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, 1);
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0;
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let (ip, fp) = digits.split_at(digits.len() - s);
    let mut ip: String = ip.iter().map(|d| (d + b'0') as char).collect::<String>().trim_start_matches('0').to_string();
    if ip.is_empty() {
        ip = "0".into();
    }
    let fp: String = fp.iter().map(|d| (d + b'0') as char).collect();
    let zero = ip == "0" && fp.bytes().all(|c| c == b'0');
    let mut out = String::new();
    if neg && !zero {
        out.push('-');
    }
    out.push_str(&ip);
    if s > 0 {
        out.push('.');
        out.push_str(&fp);
    }
    Some(out)
}

/// Shortest text for a float that reads back the same.
pub fn fmt_f64(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        let s = format!("{}", f);
        if s.len() > 20 {
            format!("{:e}", f)
        } else {
            s
        }
    }
}

// ------------------------------------------------------------- structure

#[derive(Clone, Debug, PartialEq)]
pub enum DefVal {
    Lit(Json),
    /// An expression evaluated when a row is added, e.g. CURRENT_TIMESTAMP, (UUID()).
    Expr(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColMeta {
    /// Storage key in packs (stable across renames).
    pub key: String,
    pub ty: Ty,
    pub not_null: bool,
    pub default: Option<DefVal>,
    pub on_update_now: bool,
    pub auto_inc: bool,
    pub comment: String,
    /// Value for rows saved before this column was added to a saved table.
    pub fill: Json,
    /// Primary-key column made automatically because none was declared.
    pub auto_added: bool,
}

impl ColMeta {
    pub fn plain(key: &str) -> ColMeta {
        ColMeta {
            key: key.to_string(),
            ty: Ty::Any,
            not_null: false,
            default: None,
            on_update_now: false,
            auto_inc: false,
            comment: String::new(),
            fill: Json::Null,
            auto_added: false,
        }
    }
    pub fn typed(key: &str, ty: Ty) -> ColMeta {
        ColMeta { ty, ..ColMeta::plain(key) }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Index {
    pub name: String,
    /// Storage keys of the indexed columns.
    pub cols: Vec<String>,
    pub unique: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RefAction {
    Restrict,
    Cascade,
    SetNull,
    NoAction,
}

impl RefAction {
    pub fn sql(&self) -> &'static str {
        match self {
            RefAction::Restrict => "RESTRICT",
            RefAction::Cascade => "CASCADE",
            RefAction::SetNull => "SET NULL",
            RefAction::NoAction => "NO ACTION",
        }
    }
    pub fn parse(s: &str) -> RefAction {
        match s.to_ascii_uppercase().as_str() {
            "CASCADE" => RefAction::Cascade,
            "SET NULL" | "SETNULL" => RefAction::SetNull,
            "NO ACTION" | "NOACTION" => RefAction::NoAction,
            _ => RefAction::Restrict,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fk {
    pub name: String,
    /// Storage keys in this table.
    pub cols: Vec<String>,
    /// Referenced table (its on-chain name).
    pub table: String,
    /// Storage keys in the referenced table.
    pub ref_cols: Vec<String>,
    pub on_delete: RefAction,
    pub on_update: RefAction,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    /// SQL, in terms of column names.
    pub expr: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct TableKeys {
    pub indexes: Vec<Index>,
    pub fks: Vec<Fk>,
    pub checks: Vec<Check>,
    pub comment: String,
    /// Storage keys of dropped columns (never reused).
    pub retired: Vec<String>,
    /// AUTO_INCREMENT=n table option: the next id is at least this.
    pub ai_next: u64,
}

// ------------------------------------------------------------ JSON form

fn col_json(name: &str, m: &ColMeta) -> Json {
    let mut o = json::obj(vec![("n", json::s(name))]);
    if m.key != name {
        o.set("k", json::s(&m.key));
    }
    if m.ty != Ty::Any {
        o.set("t", json::s(&m.ty.sql()));
    }
    if m.not_null {
        o.set("nn", json::n(1));
    }
    match &m.default {
        Some(DefVal::Lit(v)) => o.set("d", v.clone()),
        Some(DefVal::Expr(e)) => o.set("dx", json::s(e)),
        None => {}
    }
    if m.on_update_now {
        o.set("ou", json::n(1));
    }
    if m.auto_inc {
        o.set("ai", json::n(1));
    }
    if !m.comment.is_empty() {
        o.set("cm", json::s(&m.comment));
    }
    if !m.fill.is_null() {
        o.set("f", m.fill.clone());
    }
    if m.auto_added {
        o.set("aa", json::n(1));
    }
    o
}

fn col_from_json(v: &Json) -> Option<(String, ColMeta)> {
    let name = v.get("n").str()?.to_string();
    let key = v.get("k").str().map(String::from).unwrap_or_else(|| name.clone());
    let ty = match v.get("t").str() {
        Some(t) => Ty::parse(t).unwrap_or(Ty::Any),
        None => Ty::Any,
    };
    let default = if let Some(e) = v.get("dx").str() {
        Some(DefVal::Expr(e.to_string()))
    } else {
        match v.get("d") {
            Json::Null => None,
            x => Some(DefVal::Lit(x.clone())),
        }
    };
    let flag = |k: &str| v.get(k).u64().map(|x| x != 0).unwrap_or(false);
    Some((
        name,
        ColMeta {
            key,
            ty,
            not_null: flag("nn"),
            default,
            on_update_now: flag("ou"),
            auto_inc: flag("ai"),
            comment: v.get("cm").str_or(""),
            fill: v.get("f").clone(),
            auto_added: flag("aa"),
        },
    ))
}

/// A table's structure as stored on chain (and in this browser).
#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    pub cols: Vec<(String, ColMeta)>,
    /// Storage key of the primary-key column.
    pub pk: String,
    pub keys: TableKeys,
    /// Rows saved before this record are gone (TRUNCATE / DROP).
    pub clear: bool,
    pub dropped: bool,
    /// A checkpoint: these packs (written just before this record) hold the
    /// whole table; history before them can be skipped.
    pub snap: Vec<String>,
}

impl Doc {
    pub fn to_json(&self) -> Json {
        let mut o = json::obj(vec![("v", json::n(1)), ("c", Json::Arr(self.cols.iter().map(|(n, m)| col_json(n, m)).collect())), ("pk", json::s(&self.pk))]);
        let k = &self.keys;
        if !k.indexes.is_empty() {
            o.set(
                "ix",
                Json::Arr(
                    k.indexes
                        .iter()
                        .map(|i| {
                            let mut x = json::obj(vec![("n", json::s(&i.name)), ("c", Json::Arr(i.cols.iter().map(|c| json::s(c)).collect()))]);
                            if i.unique {
                                x.set("u", json::n(1));
                            }
                            x
                        })
                        .collect(),
                ),
            );
        }
        if !k.fks.is_empty() {
            o.set(
                "fk",
                Json::Arr(
                    k.fks
                        .iter()
                        .map(|f| {
                            json::obj(vec![
                                ("n", json::s(&f.name)),
                                ("c", Json::Arr(f.cols.iter().map(|c| json::s(c)).collect())),
                                ("t", json::s(&f.table)),
                                ("r", Json::Arr(f.ref_cols.iter().map(|c| json::s(c)).collect())),
                                ("d", json::s(f.on_delete.sql())),
                                ("u", json::s(f.on_update.sql())),
                            ])
                        })
                        .collect(),
                ),
            );
        }
        if !k.checks.is_empty() {
            o.set("ck", Json::Arr(k.checks.iter().map(|c| json::obj(vec![("n", json::s(&c.name)), ("e", json::s(&c.expr))])).collect()));
        }
        if !k.comment.is_empty() {
            o.set("cm", json::s(&k.comment));
        }
        if !k.retired.is_empty() {
            o.set("ret", Json::Arr(k.retired.iter().map(|c| json::s(c)).collect()));
        }
        if k.ai_next > 0 {
            o.set("ai", json::n(k.ai_next));
        }
        if self.clear {
            o.set("clr", json::n(1));
        }
        if self.dropped {
            o.set("drop", json::n(1));
        }
        if !self.snap.is_empty() {
            o.set("snap", Json::Arr(self.snap.iter().map(|s| json::s(s)).collect()));
        }
        o
    }

    pub fn from_json(v: &Json) -> Option<Doc> {
        let cols: Vec<(String, ColMeta)> = v.get("c").arr().iter().filter_map(col_from_json).collect();
        if cols.is_empty() && v.get("drop").u64().unwrap_or(0) == 0 {
            return None;
        }
        let strs = |x: &Json| -> Vec<String> { x.arr().iter().filter_map(|s| s.str().map(String::from)).collect() };
        let keys = TableKeys {
            indexes: v
                .get("ix")
                .arr()
                .iter()
                .map(|i| Index { name: i.get("n").str_or(""), cols: strs(i.get("c")), unique: i.get("u").u64().unwrap_or(0) != 0 })
                .collect(),
            fks: v
                .get("fk")
                .arr()
                .iter()
                .map(|f| Fk {
                    name: f.get("n").str_or(""),
                    cols: strs(f.get("c")),
                    table: f.get("t").str_or(""),
                    ref_cols: strs(f.get("r")),
                    on_delete: RefAction::parse(&f.get("d").str_or("RESTRICT")),
                    on_update: RefAction::parse(&f.get("u").str_or("RESTRICT")),
                })
                .collect(),
            checks: v.get("ck").arr().iter().map(|c| Check { name: c.get("n").str_or(""), expr: c.get("e").str_or("") }).collect(),
            comment: v.get("cm").str_or(""),
            retired: strs(v.get("ret")),
            ai_next: v.get("ai").u64().unwrap_or(0),
        };
        let pk = v.get("pk").str().map(String::from).or_else(|| cols.first().map(|c| c.1.key.clone())).unwrap_or_default();
        Some(Doc { cols, pk, keys, clear: v.get("clr").u64().unwrap_or(0) != 0, dropped: v.get("drop").u64().unwrap_or(0) != 0, snap: strs(v.get("snap")) })
    }

    /// Nothing beyond plain, untyped columns: no record needed on chain.
    pub fn is_trivial(&self) -> bool {
        !self.clear
            && !self.dropped
            && self.snap.is_empty()
            && self.keys == TableKeys::default()
            && self.cols.iter().all(|(n, m)| *m == ColMeta::plain(n))
            && self.cols.first().map(|c| c.1.key == self.pk).unwrap_or(true)
    }

    /// Column position of a storage key.
    pub fn pos(&self, key: &str) -> Option<usize> {
        self.cols.iter().position(|(_, m)| m.key == key)
    }
}

// ------------------------------------------------------------ helpers

/// A fresh storage key for a new column called `name`.
pub fn fresh_key(name: &str, used: &[String], retired: &[String]) -> String {
    let taken = |k: &str| used.iter().any(|u| u == k) || retired.iter().any(|u| u == k);
    if !taken(name) {
        return name.to_string();
    }
    let mut i = 2;
    loop {
        let k = format!("{}~{}", name, i);
        if !taken(&k) {
            return k;
        }
        i += 1;
    }
}

/// Values from a record's (key, value) pairs, in column order. Keys a
/// record doesn't have take the column's fill value; types are applied on read.
pub fn align(cols: &[ColMeta], pairs: &[(String, Json)]) -> Vec<Json> {
    cols.iter()
        .map(|m| match pairs.iter().find(|(k, _)| k == &m.key) {
            Some((_, v)) => m.ty.read(v),
            None => m.fill.clone(),
        })
        .collect()
}
