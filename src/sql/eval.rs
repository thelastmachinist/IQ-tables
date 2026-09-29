//! Values, comparisons and the built-in scalar functions (MySQL semantics,
//! with a couple of friendlier choices noted inline).

use std::cmp::Ordering;

use super::regex::Regex;
use crate::dates::{self, Dt};
use crate::json::{self, Json};
use crate::schema;

// ------------------------------------------------------------------ values

pub fn t() -> Json {
    Json::Num("1".into())
}
pub fn f() -> Json {
    Json::Num("0".into())
}
pub fn b(x: bool) -> Json {
    if x {
        t()
    } else {
        f()
    }
}

/// Strict: the value is a number (or text that is exactly a number).
pub fn num(v: &Json) -> Option<f64> {
    match v {
        Json::Num(n) => n.parse().ok(),
        Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Json::Str(s) => {
            let t = s.trim();
            if t.is_empty() || t.contains(|c: char| c.is_alphabetic() && c != 'e' && c != 'E') {
                None
            } else {
                t.parse().ok()
            }
        }
        _ => None,
    }
}

/// MySQL's arithmetic reading: the leading number in text, else 0.
pub fn num_lenient(v: &Json) -> Option<f64> {
    match v {
        Json::Null => None,
        Json::Str(s) => {
            let t = s.trim();
            let b = t.as_bytes();
            let mut i = 0;
            if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
                i += 1;
            }
            let st = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') && i > st {
                let save = i;
                i += 1;
                if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
                    i += 1;
                }
                let ds = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i == ds {
                    i = save;
                }
            }
            Some(t[..i].parse().unwrap_or(0.0))
        }
        other => num(other).or(Some(0.0)),
    }
}

fn int_text(v: &Json) -> Option<i128> {
    match v {
        Json::Num(n) | Json::Str(n) => {
            let t = n.trim();
            if !t.is_empty() && t.bytes().enumerate().all(|(i, c)| c.is_ascii_digit() || (i == 0 && (c == b'-' || c == b'+'))) && t.len() < 36 {
                t.parse().ok()
            } else {
                None
            }
        }
        Json::Bool(b) => Some(*b as i128),
        _ => None,
    }
}

/// Decimal places in a number's text ("1.25" → 2).
fn scale_of(v: &Json) -> usize {
    match v {
        Json::Num(n) | Json::Str(n) if !n.contains(['e', 'E']) => n.split_once('.').map(|(_, f)| f.len()).unwrap_or(0),
        _ => 0,
    }
}

pub fn fmt_num(f: f64) -> Json {
    if !f.is_finite() {
        return Json::Null;
    }
    if f.fract() == 0.0 && f.abs() < 1e15 {
        Json::Num(format!("{}", f as i64))
    } else {
        let r = (f * 1e10).round() / 1e10;
        let s = format!("{}", r);
        if s.contains('e') {
            Json::Num(schema::fmt_f64(f))
        } else {
            Json::Num(s)
        }
    }
}

fn fmt_scaled(f: f64, scale: usize) -> Json {
    if !f.is_finite() {
        return Json::Null;
    }
    let s = format!("{:.*}", scale.min(30), f);
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    Json::Num(if s == "-0" { "0".into() } else { s })
}

/// None = NULL.
pub fn truthy(v: &Json) -> Option<bool> {
    match v {
        Json::Null => None,
        Json::Bool(b) => Some(*b),
        Json::Num(n) => Some(n.parse::<f64>().map(|f| f != 0.0).unwrap_or(false)),
        Json::Str(s) => Some(num_lenient(&Json::Str(s.clone())).map(|f| f != 0.0).unwrap_or(false)),
        _ => Some(true),
    }
}

pub fn text(v: &Json) -> String {
    match v {
        Json::Bool(b) => (if *b { "1" } else { "0" }).into(),
        other => other.cell_text(),
    }
}

/// Numbers compare as numbers; text case-insensitively (like MySQL's
/// default collation). Text that is exactly a number compares numerically.
pub fn compare(a: &Json, b: &Json) -> Option<Ordering> {
    if a.is_null() || b.is_null() {
        return None;
    }
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => Some(text(a).to_lowercase().trim_end().cmp(text(b).to_lowercase().trim_end())),
    }
}

/// Grouping / DISTINCT / join key: equal values give equal keys.
pub fn key(v: &Json) -> String {
    match v {
        Json::Null => "\u{0}N".into(),
        other => match num(other) {
            Some(f) => format!("\u{0}#{}", f),
            None => text(other).to_lowercase().trim_end().to_string(),
        },
    }
}

pub fn like(text: &str, pat: &str, esc: char) -> bool {
    let t: Vec<char> = text.to_lowercase().chars().collect();
    // tokens: Some(c) literal, None = '_', and '%' as a separate marker
    let mut p: Vec<(char, bool)> = vec![]; // (char, is_literal)
    let pc: Vec<char> = pat.to_lowercase().chars().collect();
    let mut i = 0;
    while i < pc.len() {
        if pc[i] == esc && i + 1 < pc.len() {
            p.push((pc[i + 1], true));
            i += 2;
        } else {
            p.push((pc[i], false));
            i += 1;
        }
    }
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && ((!p[pi].1 && p[pi].0 == '_') || (p[pi].0 == t[ti] && (p[pi].1 || p[pi].0 != '%'))) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && !p[pi].1 && p[pi].0 == '%' {
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
    while pi < p.len() && !p[pi].1 && p[pi].0 == '%' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------- arithmetic

pub fn arith(op: &str, a: &Json, b: &Json) -> Result<Json, String> {
    if a.is_null() || b.is_null() {
        return Ok(Json::Null);
    }
    if let (Some(x), Some(y)) = (int_text(a), int_text(b)) {
        let r = match op {
            "+" => x.checked_add(y),
            "-" => x.checked_sub(y),
            "*" => x.checked_mul(y),
            "DIV" => {
                if y == 0 {
                    return Ok(Json::Null);
                }
                Some(x / y)
            }
            "%" => {
                if y == 0 {
                    return Ok(Json::Null);
                }
                Some(x % y)
            }
            "&" => Some(x & y),
            "|" => Some(x | y),
            "^" => Some(x ^ y),
            "<<" => Some(x.checked_shl(y as u32).unwrap_or(0)),
            ">>" => Some(x.checked_shr(y as u32).unwrap_or(0)),
            _ => None,
        };
        if let Some(r) = r {
            if r.abs() < (1i128 << 100) {
                return Ok(Json::Num(r.to_string()));
            }
        }
    }
    let (Some(x), Some(y)) = (num_lenient(a), num_lenient(b)) else { return Ok(Json::Null) };
    let scale = scale_of(a).max(scale_of(b));
    Ok(match op {
        "+" => fmt_scaled(x + y, scale.max(if (x + y).fract() != 0.0 { 10 } else { 0 })),
        "-" => fmt_scaled(x - y, scale.max(if (x - y).fract() != 0.0 { 10 } else { 0 })),
        "*" => fmt_num(x * y),
        "/" => {
            if y == 0.0 {
                Json::Null
            } else {
                // MySQL keeps 4 more decimal places than the dividend
                fmt_scaled(x / y, scale_of(a) + 4)
            }
        }
        "DIV" => {
            if y == 0.0 {
                Json::Null
            } else {
                fmt_num((x / y).trunc())
            }
        }
        "%" => {
            if y == 0.0 {
                Json::Null
            } else {
                fmt_num(x % y)
            }
        }
        _ => fmt_num(((x as i64) & (y as i64)) as f64),
    })
}

// ----------------------------------------------------------------- dates

pub fn to_dt(v: &Json) -> Option<Dt> {
    match v {
        Json::Null => None,
        Json::Num(n) => {
            // 20260928 or 20260928143000
            let t = n.split('.').next().unwrap_or("");
            if t.len() == 8 {
                dates::parse_date(t).map(|(y, m, d)| Dt { days: dates::days_from_civil(y, m, d), secs: 0, micros: 0, has_time: false })
            } else if t.len() == 14 {
                let d = dates::parse_date(&t[..8])?;
                let (h, mi, s) = (t[8..10].parse::<i64>().ok()?, t[10..12].parse::<i64>().ok()?, t[12..14].parse::<i64>().ok()?);
                Some(Dt { days: dates::days_from_civil(d.0, d.1, d.2), secs: h * 3600 + mi * 60 + s, micros: 0, has_time: true })
            } else {
                None
            }
        }
        other => dates::parse_datetime(&text(other)),
    }
}

fn to_secs_time(v: &Json) -> Option<(bool, i64, u32)> {
    let s = text(v);
    if let Some(t) = dates::parse_time(&s) {
        return Some(t);
    }
    to_dt(v).map(|d| (false, d.secs, d.micros))
}

fn dt_out(d: &Dt) -> Json {
    if d.has_time {
        Json::Str(d.datetime_str())
    } else {
        Json::Str(d.date_str())
    }
}

pub fn interval_of(n: &Json, unit: &str) -> Option<i64> {
    let _ = unit;
    num_lenient(n).map(|f| f.round() as i64)
}

pub fn date_add(d: &Json, n: &Json, unit: &str, sub: bool) -> Json {
    let (Some(dt), Some(k)) = (to_dt(d), interval_of(n, unit)) else { return Json::Null };
    match dates::add(dt, if sub { -k } else { k }, unit) {
        Some(mut r) => {
            if matches!(unit, "HOUR" | "MINUTE" | "SECOND" | "MICROSECOND") {
                r.has_time = true;
            }
            dt_out(&r)
        }
        None => Json::Null,
    }
}

fn unit_diff(unit: &str, a: &Dt, b: &Dt) -> i64 {
    // TIMESTAMPDIFF(unit, a, b) = b - a
    let secs = (b.unix() - a.unix()) as i128 * 1_000_000 + b.micros as i128 - a.micros as i128;
    match unit {
        "MICROSECOND" => secs as i64,
        "SECOND" => (secs / 1_000_000) as i64,
        "MINUTE" => (secs / 60_000_000) as i64,
        "HOUR" => (secs / 3_600_000_000) as i64,
        "DAY" => (secs / 86_400_000_000) as i64,
        "WEEK" => (secs / 604_800_000_000) as i64,
        _ => {
            let (ya, ma, da) = a.ymd();
            let (yb, mb, db) = b.ymd();
            let mut months = (yb - ya) * 12 + (mb as i64 - ma as i64);
            // not a full month yet?
            let later_day = (db, b.secs) < (da, a.secs);
            let earlier_day = (db, b.secs) > (da, a.secs);
            if months > 0 && later_day {
                months -= 1;
            } else if months < 0 && earlier_day {
                months += 1;
            }
            match unit {
                "MONTH" => months,
                "QUARTER" => months / 3,
                _ => months / 12,
            }
        }
    }
}

fn iso_week(d: &Dt) -> (i64, u32) {
    // ISO 8601 week: Monday first, week 1 contains the first Thursday
    let wd = d.weekday() as i64; // Mon=0
    let thursday = d.days - wd + 3;
    let (y, _, _) = dates::civil_from_days(thursday);
    let jan1 = dates::days_from_civil(y, 1, 1);
    (y, ((thursday - jan1) / 7 + 1) as u32)
}

// ------------------------------------------------------------------ JSON

fn json_path(p: &str) -> Option<Vec<Result<String, usize>>> {
    let p = p.trim();
    let mut rest = p.strip_prefix('$')?;
    let mut out = vec![];
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            if let Some(r2) = r.strip_prefix('"') {
                let end = r2.find('"')?;
                out.push(Ok(r2[..end].to_string()));
                rest = &r2[end + 1..];
            } else {
                let end = r.find(['.', '[']).unwrap_or(r.len());
                out.push(Ok(r[..end].to_string()));
                rest = &r[end..];
            }
        } else if let Some(r) = rest.strip_prefix('[') {
            let end = r.find(']')?;
            out.push(Err(r[..end].trim().parse().ok()?));
            rest = &r[end + 1..];
        } else {
            return None;
        }
    }
    Some(out)
}

fn json_doc(v: &Json) -> Option<Json> {
    match v {
        Json::Str(s) => json::parse(s).ok(),
        Json::Null => None,
        other => Some(other.clone()),
    }
}

pub fn json_extract(doc: &Json, path: &str) -> Option<Json> {
    let mut cur = json_doc(doc)?;
    for step in json_path(path)? {
        cur = match (step, cur) {
            (Ok(k), Json::Obj(v)) => v.into_iter().find(|(x, _)| *x == k).map(|(_, v)| v)?,
            (Err(i), Json::Arr(v)) => v.into_iter().nth(i)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn json_of(v: &Json) -> Json {
    // strings that hold JSON documents stay text: JSON_ARRAY('a') = ["a"]
    v.clone()
}

// -------------------------------------------------------------- functions

pub struct FnEnv<'a> {
    pub now: Dt,
    pub database: &'a str,
    pub user: &'a str,
    pub last_insert_id: u64,
    pub row_count: i64,
    pub found_rows: u64,
    pub rand: &'a std::cell::Cell<u64>,
}

fn arg<'a>(a: &'a [Json], i: usize) -> &'a Json {
    static NULL: Json = Json::Null;
    a.get(i).unwrap_or(&NULL)
}

fn need(name: &str, a: &[Json], lo: usize, hi: usize) -> Result<(), String> {
    if a.len() < lo || a.len() > hi {
        if lo == hi {
            return Err(format!("{}() takes {} argument{}", name, lo, if lo == 1 { "" } else { "s" }));
        }
        return Err(format!("{}() takes {} to {} arguments", name, lo, hi));
    }
    Ok(())
}

fn sint(v: &Json) -> Option<i64> {
    num_lenient(v).map(|f| f.round() as i64)
}

fn soundex(s: &str) -> String {
    let code = |c: char| match c.to_ascii_lowercase() {
        'b' | 'f' | 'p' | 'v' => '1',
        'c' | 'g' | 'j' | 'k' | 'q' | 's' | 'x' | 'z' => '2',
        'd' | 't' => '3',
        'l' => '4',
        'm' | 'n' => '5',
        'r' => '6',
        _ => '0',
    };
    let letters: Vec<char> = s.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    let Some(&first) = letters.first() else { return String::new() };
    let mut out = String::from(first.to_ascii_uppercase());
    let mut last = code(first);
    for &c in &letters[1..] {
        let d = code(c);
        if d != '0' && d != last {
            out.push(d);
        }
        if !matches!(c.to_ascii_lowercase(), 'h' | 'w') {
            last = d;
        }
    }
    while out.len() < 4 {
        out.push('0');
    }
    out
}

fn regex(pat: &Json, mt: &Json) -> Result<Regex, String> {
    let m = text(mt);
    let ci = !m.contains('c');
    Regex::new(&text(pat), ci || m.contains('i'))
}

fn format_number(x: f64, d: usize) -> String {
    let s = format!("{:.*}", d.min(30), x.abs());
    let (int, frac) = match s.split_once('.') {
        Some((a, b)) => (a.to_string(), Some(b.to_string())),
        None => (s, None),
    };
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let mut out = if x < 0.0 && x.abs() >= 0.5 * 10f64.powi(-(d as i32)) { format!("-{}", grouped) } else { grouped };
    if let Some(f) = frac {
        out.push('.');
        out.push_str(&f);
    }
    out
}

pub fn round_num(v: &Json, d: i64, trunc: bool) -> Json {
    let Some(x) = num_lenient(v) else { return Json::Null };
    if d >= 0 && !trunc {
        if let Json::Num(n) | Json::Str(n) = v {
            if let Some(r) = schema::round_dec(n, d as u32) {
                return Json::Num(if r.contains('.') && scale_of(v) < d as usize { r.trim_end_matches('0').trim_end_matches('.').to_string() } else { r });
            }
        }
    }
    let p = 10f64.powi(d as i32);
    let r = if trunc { (x * p).trunc() / p } else { (x * p).abs().round().copysign(x) / p };
    if d > 0 {
        fmt_scaled(r, d as usize)
    } else {
        fmt_num(r)
    }
}

/// Call a scalar function. `a` are the evaluated arguments.
pub fn scalar(name: &str, a: &[Json], env: &FnEnv) -> Result<Json, String> {
    let s0 = || text(arg(a, 0));
    let any_null = || a.iter().any(|v| v.is_null());
    let n = |i: usize| num_lenient(arg(a, i));
    Ok(match name {
        // ---------------------------------------------------------- text
        "CONCAT" => {
            if any_null() {
                Json::Null
            } else {
                Json::Str(a.iter().map(text).collect())
            }
        }
        "CONCAT_WS" => {
            need(name, a, 1, usize::MAX)?;
            if a[0].is_null() {
                Json::Null
            } else {
                Json::Str(a[1..].iter().filter(|v| !v.is_null()).map(text).collect::<Vec<_>>().join(&text(&a[0])))
            }
        }
        "LENGTH" | "OCTET_LENGTH" | "BIT_LENGTH" => {
            need(name, a, 1, 1)?;
            if a[0].is_null() {
                Json::Null
            } else {
                json::n(s0().len() * if name == "BIT_LENGTH" { 8 } else { 1 })
            }
        }
        "CHAR_LENGTH" | "CHARACTER_LENGTH" => {
            need(name, a, 1, 1)?;
            if a[0].is_null() {
                Json::Null
            } else {
                json::n(s0().chars().count())
            }
        }
        "LOWER" | "LCASE" | "UPPER" | "UCASE" => {
            need(name, a, 1, 1)?;
            if a[0].is_null() {
                Json::Null
            } else if name.starts_with('L') {
                Json::Str(s0().to_lowercase())
            } else {
                Json::Str(s0().to_uppercase())
            }
        }
        "TRIM_BOTH" | "TRIM_LEADING" | "TRIM_TRAILING" | "LTRIM" | "RTRIM" | "TRIM" => {
            if a[0].is_null() {
                return Ok(Json::Null);
            }
            let s = s0();
            let rem = if a.len() > 1 { text(&a[1]) } else { " ".into() };
            if rem.is_empty() {
                return Ok(Json::Str(s));
            }
            let mut r = s.as_str();
            if matches!(name, "TRIM_BOTH" | "TRIM_LEADING" | "LTRIM" | "TRIM") {
                while let Some(x) = r.strip_prefix(rem.as_str()) {
                    r = x;
                }
            }
            if matches!(name, "TRIM_BOTH" | "TRIM_TRAILING" | "RTRIM" | "TRIM") {
                while let Some(x) = r.strip_suffix(rem.as_str()) {
                    r = x;
                }
            }
            Json::Str(r.to_string())
        }
        "SUBSTRING" => {
            need(name, a, 2, 3)?;
            if any_null() {
                return Ok(Json::Null);
            }
            let c: Vec<char> = s0().chars().collect();
            let pos = sint(&a[1]).unwrap_or(0);
            let len = if a.len() > 2 { sint(&a[2]).unwrap_or(0) } else { i64::MAX };
            if pos == 0 || len <= 0 {
                return Ok(Json::Str(String::new()));
            }
            let start = if pos > 0 { pos - 1 } else { c.len() as i64 + pos };
            if start < 0 || start as usize >= c.len() {
                return Ok(Json::Str(String::new()));
            }
            let end = (start as i128 + len as i128).min(c.len() as i128) as usize;
            Json::Str(c[start as usize..end].iter().collect())
        }
        "LEFT" | "RIGHT" => {
            need(name, a, 2, 2)?;
            if any_null() {
                return Ok(Json::Null);
            }
            let c: Vec<char> = s0().chars().collect();
            let k = sint(&a[1]).unwrap_or(0).max(0) as usize;
            let k = k.min(c.len());
            Json::Str(if name == "LEFT" { c[..k].iter().collect() } else { c[c.len() - k..].iter().collect() })
        }
        "REPLACE" => {
            need(name, a, 3, 3)?;
            if any_null() {
                return Ok(Json::Null);
            }
            let from = text(&a[1]);
            if from.is_empty() {
                Json::Str(s0())
            } else {
                Json::Str(s0().replace(&from, &text(&a[2])))
            }
        }
        "INSTR" | "LOCATE" | "POSITION" => {
            need(name, a, 2, 3)?;
            if any_null() {
                return Ok(Json::Null);
            }
            let (hay, sub) = if name == "INSTR" { (text(&a[0]), text(&a[1])) } else { (text(&a[1]), text(&a[0])) };
            let from = if name != "INSTR" && a.len() > 2 { sint(&a[2]).unwrap_or(1).max(1) as usize - 1 } else { 0 };
            let h: Vec<char> = hay.to_lowercase().chars().collect();
            let s: Vec<char> = sub.to_lowercase().chars().collect();
            if s.is_empty() {
                return Ok(json::n(from + 1));
            }
            let mut found = 0;
            if h.len() >= s.len() {
                for i in from..=h.len() - s.len() {
                    if h[i..i + s.len()] == s[..] {
                        found = i + 1;
                        break;
                    }
                }
            }
            json::n(found)
        }
        "LPAD" | "RPAD" => {
            need(name, a, 3, 3)?;
            if any_null() {
                return Ok(Json::Null);
            }
            let c: Vec<char> = s0().chars().collect();
            let k = sint(&a[1]).unwrap_or(0).max(0) as usize;
            let pad: Vec<char> = text(&a[2]).chars().collect();
            if c.len() >= k {
                return Ok(Json::Str(c[..k].iter().collect()));
            }
            if pad.is_empty() {
                return Ok(Json::Null);
            }
            let fill: String = pad.iter().cycle().take(k - c.len()).collect();
            let body: String = c.iter().collect();
            Json::Str(if name == "LPAD" { fill + &body } else { body + &fill })
        }
        "REVERSE" => {
            if a[0].is_null() {
                Json::Null
            } else {
                Json::Str(s0().chars().rev().collect())
            }
        }
        "REPEAT" => {
            need(name, a, 2, 2)?;
            if any_null() {
                return Ok(Json::Null);
            }
            let k = sint(&a[1]).unwrap_or(0).clamp(0, 100_000) as usize;
            Json::Str(s0().repeat(k))
        }
        "SPACE" => Json::Str(" ".repeat(sint(arg(a, 0)).unwrap_or(0).clamp(0, 100_000) as usize)),
        "FORMAT" => {
            need(name, a, 2, 3)?;
            match (n(0), sint(&a[1])) {
                (Some(x), Some(d)) => Json::Str(format_number(x, d.max(0) as usize)),
                _ => Json::Null,
            }
        }
        "STRCMP" => {
            need(name, a, 2, 2)?;
            match compare(&Json::Str(text(&a[0])), &Json::Str(text(&a[1]))) {
                None => Json::Null,
                Some(o) => json::n(o as i32),
            }
        }
        "ASCII" | "ORD" => {
            if a[0].is_null() {
                Json::Null
            } else {
                json::n(s0().chars().next().map(|c| c as u32).unwrap_or(0))
            }
        }
        "CHAR" => Json::Str(a.iter().filter_map(|v| sint(v)).filter_map(|c| char::from_u32(c as u32)).collect()),
        "HEX" => match arg(a, 0) {
            Json::Null => Json::Null,
            v @ Json::Num(_) => Json::Str(format!("{:X}", sint(v).unwrap_or(0))),
            v => Json::Str(crate::crypto::hex(text(v).as_bytes()).to_uppercase()),
        },
        "UNHEX" => match crate::crypto::unhex(&s0()) {
            Some(b) => Json::Str(String::from_utf8_lossy(&b).into_owned()),
            None => Json::Null,
        },
        "TO_BASE64" => {
            if a[0].is_null() {
                Json::Null
            } else {
                Json::Str(crate::crypto::base64_encode(s0().as_bytes()))
            }
        }
        "FROM_BASE64" => match crate::crypto::base64_decode(&s0()) {
            Some(b) => Json::Str(String::from_utf8_lossy(&b).into_owned()),
            None => Json::Null,
        },
        "SHA2" => {
            let bits = sint(arg(a, 1)).unwrap_or(256);
            if a[0].is_null() {
                Json::Null
            } else if bits == 256 || bits == 0 {
                Json::Str(crate::crypto::hex(&crate::crypto::sha2::sha256(s0().as_bytes())))
            } else {
                return Err("SHA2 supports 256 bits here".into());
            }
        }
        "MD5" | "SHA" | "SHA1" => return Err(format!("{}() isn't available; SHA2(text, 256) is.", name)),
        "UUID" => {
            let mut r = [0u8; 16];
            crate::host::random(&mut r);
            r[6] = (r[6] & 0x0f) | 0x40;
            r[8] = (r[8] & 0x3f) | 0x80;
            let h = crate::crypto::hex(&r);
            Json::Str(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
        }
        "FIELD" => {
            if a.is_empty() || a[0].is_null() {
                return Ok(json::n(0));
            }
            json::n(a[1..].iter().position(|v| compare(&a[0], v) == Some(Ordering::Equal)).map(|p| p + 1).unwrap_or(0))
        }
        "FIND_IN_SET" => {
            need(name, a, 2, 2)?;
            if any_null() {
                return Ok(Json::Null);
            }
            let x = s0().to_lowercase();
            json::n(text(&a[1]).split(',').position(|p| p.to_lowercase() == x).map(|p| p + 1).unwrap_or(0))
        }
        "ELT" => {
            let k = sint(arg(a, 0)).unwrap_or(0);
            if k >= 1 && (k as usize) < a.len() {
                a[k as usize].clone()
            } else {
                Json::Null
            }
        }
        "SUBSTRING_INDEX" => {
            need(name, a, 3, 3)?;
            if any_null() {
                return Ok(Json::Null);
            }
            let s = s0();
            let d = text(&a[1]);
            let k = sint(&a[2]).unwrap_or(0);
            if d.is_empty() || k == 0 {
                return Ok(Json::Str(String::new()));
            }
            let parts: Vec<&str> = s.split(d.as_str()).collect();
            let r = if k > 0 { parts[..(k as usize).min(parts.len())].join(&d) } else { parts[parts.len().saturating_sub((-k) as usize)..].join(&d) };
            Json::Str(r)
        }
        "INSERT" => {
            need(name, a, 4, 4)?;
            if any_null() {
                return Ok(Json::Null);
            }
            let c: Vec<char> = s0().chars().collect();
            let pos = sint(&a[1]).unwrap_or(0);
            let len = sint(&a[2]).unwrap_or(0).max(0) as usize;
            if pos < 1 || pos as usize > c.len() {
                return Ok(Json::Str(c.iter().collect()));
            }
            let st = pos as usize - 1;
            let end = (st + len).min(c.len());
            Json::Str(c[..st].iter().collect::<String>() + &text(&a[3]) + &c[end..].iter().collect::<String>())
        }
        "QUOTE" => match arg(a, 0) {
            Json::Null => Json::Str("NULL".into()),
            v => Json::Str(format!("'{}'", text(v).replace('\\', "\\\\").replace('\'', "\\'"))),
        },
        "SOUNDEX" => {
            if a[0].is_null() {
                Json::Null
            } else {
                Json::Str(soundex(&s0()))
            }
        }
        "REGEXP_LIKE" => {
            need(name, a, 2, 3)?;
            if a[0].is_null() || a[1].is_null() {
                return Ok(Json::Null);
            }
            b(regex(&a[1], arg(a, 2))?.is_match(&s0()))
        }
        "REGEXP_REPLACE" => {
            need(name, a, 3, 6)?;
            if a[..3].iter().any(|v| v.is_null()) {
                return Ok(Json::Null);
            }
            let r = regex(&a[1], arg(a, 5))?;
            let pos = sint(arg(a, 3)).unwrap_or(1).max(1) as usize - 1;
            let occ = sint(arg(a, 4)).unwrap_or(0).max(0) as usize;
            Json::Str(r.replace(&s0(), &text(&a[2]), pos, occ))
        }
        "REGEXP_SUBSTR" | "REGEXP_INSTR" => {
            need(name, a, 2, 6)?;
            if a[0].is_null() || a[1].is_null() {
                return Ok(Json::Null);
            }
            let mt = if name == "REGEXP_SUBSTR" { arg(a, 4) } else { arg(a, 5) };
            let r = regex(&a[1], mt)?;
            let c: Vec<char> = s0().chars().collect();
            let mut pos = sint(arg(a, 2)).unwrap_or(1).max(1) as usize - 1;
            let occ = sint(arg(a, 3)).unwrap_or(1).max(1);
            let mut hit = None;
            for _ in 0..occ {
                match r.find_at(&c, pos) {
                    Some((s, e)) => {
                        hit = Some((s, e));
                        pos = if e > s { e } else { s + 1 };
                    }
                    None => {
                        hit = None;
                        break;
                    }
                }
            }
            match (name, hit) {
                ("REGEXP_SUBSTR", Some((s, e))) => Json::Str(c[s..e].iter().collect()),
                ("REGEXP_SUBSTR", None) => Json::Null,
                (_, Some((s, e))) => json::n(if sint(arg(a, 4)).unwrap_or(0) == 1 { e + 1 } else { s + 1 }),
                (_, None) => json::n(0),
            }
        }
        // ------------------------------------------------------- numbers
        "ABS" => n(0).map(|x| if let Json::Num(s) = &a[0] { Json::Num(s.trim_start_matches('-').to_string()) } else { fmt_num(x.abs()) }).unwrap_or(Json::Null),
        "CEIL" | "CEILING" => n(0).map(|x| fmt_num(x.ceil())).unwrap_or(Json::Null),
        "FLOOR" => n(0).map(|x| fmt_num(x.floor())).unwrap_or(Json::Null),
        "ROUND" => {
            need(name, a, 1, 2)?;
            if any_null() {
                return Ok(Json::Null);
            }
            round_num(&a[0], sint(arg(a, 1)).unwrap_or(0), false)
        }
        "TRUNCATE" => {
            need(name, a, 2, 2)?;
            if any_null() {
                return Ok(Json::Null);
            }
            round_num(&a[0], sint(&a[1]).unwrap_or(0), true)
        }
        "MOD" => {
            need(name, a, 2, 2)?;
            arith("%", &a[0], &a[1])?
        }
        "POW" | "POWER" => match (n(0), n(1)) {
            (Some(x), Some(y)) => fmt_num(x.powf(y)),
            _ => Json::Null,
        },
        "SQRT" => n(0).filter(|x| *x >= 0.0).map(|x| fmt_num(x.sqrt())).unwrap_or(Json::Null),
        "EXP" => n(0).map(|x| fmt_num(x.exp())).unwrap_or(Json::Null),
        "LN" => n(0).filter(|x| *x > 0.0).map(|x| fmt_num(x.ln())).unwrap_or(Json::Null),
        "LOG" => {
            if a.len() == 2 {
                match (n(0), n(1)) {
                    (Some(bs), Some(x)) if bs > 0.0 && bs != 1.0 && x > 0.0 => fmt_num(x.ln() / bs.ln()),
                    _ => Json::Null,
                }
            } else {
                n(0).filter(|x| *x > 0.0).map(|x| fmt_num(x.ln())).unwrap_or(Json::Null)
            }
        }
        "LOG10" => n(0).filter(|x| *x > 0.0).map(|x| fmt_num(x.log10())).unwrap_or(Json::Null),
        "LOG2" => n(0).filter(|x| *x > 0.0).map(|x| fmt_num(x.log2())).unwrap_or(Json::Null),
        "SIGN" => n(0).map(|x| json::n(if x > 0.0 { 1 } else if x < 0.0 { -1 } else { 0 })).unwrap_or(Json::Null),
        "PI" => Json::Num("3.141593".into()),
        "SIN" | "COS" | "TAN" | "ASIN" | "ACOS" | "ATAN" | "COT" | "DEGREES" | "RADIANS" => match n(0) {
            None => Json::Null,
            Some(x) => {
                if name == "ATAN" && a.len() == 2 {
                    return Ok(n(1).map(|y| fmt_num(x.atan2(y))).unwrap_or(Json::Null));
                }
                fmt_num(match name {
                    "SIN" => x.sin(),
                    "COS" => x.cos(),
                    "TAN" => x.tan(),
                    "ASIN" => x.asin(),
                    "ACOS" => x.acos(),
                    "ATAN" => x.atan(),
                    "COT" => 1.0 / x.tan(),
                    "DEGREES" => x.to_degrees(),
                    _ => x.to_radians(),
                })
            }
        },
        "ATAN2" => match (n(0), n(1)) {
            (Some(y), Some(x)) => fmt_num(y.atan2(x)),
            _ => Json::Null,
        },
        "RAND" => {
            let mut x = env.rand.get();
            if let Some(seed) = n(0) {
                x = seed as u64 ^ 0x9E3779B97F4A7C15;
            }
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            env.rand.set(x);
            fmt_num((x >> 11) as f64 / (1u64 << 53) as f64)
        }
        "GREATEST" | "LEAST" => {
            if a.is_empty() || any_null() {
                return Ok(Json::Null);
            }
            let mut best = a[0].clone();
            for v in &a[1..] {
                let o = compare(v, &best).unwrap_or(Ordering::Equal);
                if (name == "GREATEST" && o == Ordering::Greater) || (name == "LEAST" && o == Ordering::Less) {
                    best = v.clone();
                }
            }
            best
        }
        "CONV" => {
            need(name, a, 3, 3)?;
            let from = sint(&a[1]).unwrap_or(10) as u32;
            let to = sint(&a[2]).unwrap_or(10) as u32;
            if !(2..=36).contains(&from) || !(2..=36).contains(&to) || a[0].is_null() {
                return Ok(Json::Null);
            }
            let Ok(v) = u128::from_str_radix(&s0().to_ascii_lowercase(), from) else { return Ok(json::n(0)) };
            let digits = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".as_bytes();
            let mut v = v;
            let mut out = vec![];
            loop {
                out.push(digits[(v % to as u128) as usize] as char);
                v /= to as u128;
                if v == 0 {
                    break;
                }
            }
            Json::Str(out.iter().rev().collect())
        }
        // --------------------------------------------------------- dates
        "NOW" | "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" | "SYSDATE" => {
            let mut d = env.now;
            d.micros = 0;
            Json::Str(d.datetime_str())
        }
        "CURDATE" | "CURRENT_DATE" => Json::Str(env.now.date_str()),
        "CURTIME" | "CURRENT_TIME" => {
            let (h, m, s) = env.now.hms();
            Json::Str(format!("{:02}:{:02}:{:02}", h, m, s))
        }
        "UTC_TIMESTAMP" | "UTC_DATE" | "UTC_TIME" => {
            let d = dates::now_utc();
            match name {
                "UTC_DATE" => Json::Str(d.date_str()),
                "UTC_TIME" => Json::Str(d.datetime_str()[11..].to_string()),
                _ => Json::Str(d.datetime_str()),
            }
        }
        "DATE" => to_dt(arg(a, 0)).map(|d| Json::Str(d.date_str())).unwrap_or(Json::Null),
        "TIME" => match to_secs_time(arg(a, 0)) {
            Some((neg, s, us)) => Json::Str(dates::time_str(neg, s, us)),
            None => Json::Null,
        },
        "TIMESTAMP" => {
            let Some(mut d) = to_dt(arg(a, 0)) else { return Ok(Json::Null) };
            if let Some((neg, s, _)) = a.get(1).and_then(to_secs_time) {
                d = dates::add(d, if neg { -s } else { s }, "SECOND").unwrap_or(d);
            }
            d.has_time = true;
            Json::Str(d.datetime_str())
        }
        "YEAR" | "MONTH" | "DAY" | "DAYOFMONTH" | "DAYOFWEEK" | "WEEKDAY" | "DAYOFYEAR" | "QUARTER" | "MONTHNAME" | "DAYNAME" | "LAST_DAY" | "WEEKOFYEAR" | "TO_DAYS" | "YEARWEEK" => {
            let Some(d) = to_dt(arg(a, 0)) else { return Ok(Json::Null) };
            let (y, m, dd) = d.ymd();
            match name {
                "YEAR" => json::n(y),
                "MONTH" => json::n(m),
                "DAY" | "DAYOFMONTH" => json::n(dd),
                "DAYOFWEEK" => json::n((d.weekday() + 1) % 7 + 1),
                "WEEKDAY" => json::n(d.weekday()),
                "DAYOFYEAR" => json::n(d.day_of_year()),
                "QUARTER" => json::n((m - 1) / 3 + 1),
                "MONTHNAME" => Json::Str(dates::MONTHS[m as usize - 1].into()),
                "DAYNAME" => Json::Str(dates::DAYS[d.weekday() as usize].into()),
                "LAST_DAY" => Json::Str(format!("{:04}-{:02}-{:02}", y, m, dates::days_in_month(y, m))),
                "WEEKOFYEAR" => json::n(iso_week(&d).1),
                "YEARWEEK" => {
                    let w = dates::week0(&d);
                    json::n(y * 100 + w as i64)
                }
                _ => json::n(d.days + 719528),
            }
        }
        "WEEK" => {
            let Some(d) = to_dt(arg(a, 0)) else { return Ok(Json::Null) };
            match sint(arg(a, 1)).unwrap_or(0) {
                3 => json::n(iso_week(&d).1),
                _ => json::n(dates::week0(&d)),
            }
        }
        "HOUR" | "MINUTE" | "SECOND" | "MICROSECOND" => {
            let Some((_, s, us)) = to_secs_time(arg(a, 0)) else { return Ok(Json::Null) };
            json::n(match name {
                "HOUR" => s / 3600,
                "MINUTE" => (s / 60) % 60,
                "SECOND" => s % 60,
                _ => us as i64,
            })
        }
        "EXTRACT" => {
            let unit = text(arg(a, 0));
            let v = arg(a, 1);
            match unit.as_str() {
                "HOUR" | "MINUTE" | "SECOND" | "MICROSECOND" => scalar(&unit, &[v.clone()], env)?,
                "WEEK" => scalar("WEEK", &[v.clone()], env)?,
                u => scalar(u, &[v.clone()], env)?,
            }
        }
        "DATE_FORMAT" | "TIME_FORMAT" => {
            need(name, a, 2, 2)?;
            if any_null() {
                return Ok(Json::Null);
            }
            let d = if name == "TIME_FORMAT" { to_secs_time(&a[0]).map(|(_, s, us)| Dt { days: 0, secs: s % 86400, micros: us, has_time: true }) } else { to_dt(&a[0]) };
            match d {
                Some(d) => Json::Str(dates::format(&d, &text(&a[1]))),
                None => Json::Null,
            }
        }
        "STR_TO_DATE" => {
            need(name, a, 2, 2)?;
            if any_null() {
                return Ok(Json::Null);
            }
            match dates::parse_with(&text(&a[0]), &text(&a[1])) {
                Some(d) => {
                    let f = text(&a[1]);
                    let has_date = ["%Y", "%y", "%m", "%c", "%d", "%e", "%M", "%b"].iter().any(|k| f.contains(k));
                    if !has_date {
                        Json::Str(dates::time_str(false, d.secs, 0))
                    } else {
                        dt_out(&d)
                    }
                }
                None => Json::Null,
            }
        }
        "DATEDIFF" => {
            need(name, a, 2, 2)?;
            match (to_dt(&a[0]), to_dt(&a[1])) {
                (Some(x), Some(y)) => json::n(x.days - y.days),
                _ => Json::Null,
            }
        }
        "TIMEDIFF" => {
            need(name, a, 2, 2)?;
            let secs = |v: &Json| -> Option<i64> {
                let s = text(v);
                if let Some((neg, t, _)) = dates::parse_time(&s) {
                    return Some(if neg { -t } else { t });
                }
                to_dt(v).map(|d| d.unix())
            };
            match (secs(&a[0]), secs(&a[1])) {
                (Some(x), Some(y)) => {
                    let d = x - y;
                    Json::Str(dates::time_str(d < 0, d.abs(), 0))
                }
                _ => Json::Null,
            }
        }
        "TIMESTAMPDIFF" => {
            need(name, a, 3, 3)?;
            match (to_dt(&a[1]), to_dt(&a[2])) {
                (Some(x), Some(y)) => json::n(unit_diff(&text(&a[0]), &x, &y)),
                _ => Json::Null,
            }
        }
        "TIMESTAMPADD" => {
            need(name, a, 3, 3)?;
            date_add(&a[2], &a[1], &text(&a[0]), false)
        }
        "DATE_ADD" | "ADDDATE" | "DATE_SUB" | "SUBDATE" => {
            need(name, a, 2, 2)?;
            // the INTERVAL argument arrives as [n, unit]
            let sub = name == "DATE_SUB" || name == "SUBDATE";
            match &a[1] {
                Json::Arr(v) if v.len() == 2 => date_add(&a[0], &v[0], &text(&v[1]), sub),
                n => date_add(&a[0], n, "DAY", sub),
            }
        }
        "ADDTIME" | "SUBTIME" => {
            need(name, a, 2, 2)?;
            let Some((neg, s, _)) = to_secs_time(&a[1]) else { return Ok(Json::Null) };
            let s = if neg ^ (name == "SUBTIME") { -s } else { s };
            if let Some(d) = to_dt(&a[0]).filter(|d| d.has_time || text(&a[0]).contains('-')) {
                let mut r = dates::add(d, s, "SECOND").unwrap_or(d);
                r.has_time = true;
                Json::Str(r.datetime_str())
            } else if let Some((n0, t0, _)) = to_secs_time(&a[0]) {
                let tot = if n0 { -t0 } else { t0 } + s;
                Json::Str(dates::time_str(tot < 0, tot.abs(), 0))
            } else {
                Json::Null
            }
        }
        "UNIX_TIMESTAMP" => {
            if a.is_empty() {
                json::n((crate::host::now_ms() / 1000.0) as i64)
            } else {
                match to_dt(&a[0]) {
                    Some(d) => json::n(d.unix() - crate::host::tz_offset_min() as i64 * 60),
                    None => Json::Null,
                }
            }
        }
        "FROM_UNIXTIME" => {
            let Some(ts) = n(0) else { return Ok(Json::Null) };
            let d = Dt::from_unix(ts as i64 + crate::host::tz_offset_min() as i64 * 60);
            if a.len() > 1 {
                Json::Str(dates::format(&d, &text(&a[1])))
            } else {
                Json::Str(d.datetime_str())
            }
        }
        "FROM_DAYS" => sint(arg(a, 0)).map(|k| Json::Str(Dt { days: k - 719528, secs: 0, micros: 0, has_time: false }.date_str())).unwrap_or(Json::Null),
        "MAKEDATE" => match (sint(arg(a, 0)), sint(arg(a, 1))) {
            (Some(y), Some(doy)) if doy > 0 => Json::Str(Dt { days: dates::days_from_civil(y, 1, 1) + doy - 1, secs: 0, micros: 0, has_time: false }.date_str()),
            _ => Json::Null,
        },
        "MAKETIME" => match (sint(arg(a, 0)), sint(arg(a, 1)), sint(arg(a, 2))) {
            (Some(h), Some(m), Some(s)) if (0..60).contains(&m) && (0..60).contains(&s) => Json::Str(dates::time_str(h < 0, h.abs() * 3600 + m * 60 + s, 0)),
            _ => Json::Null,
        },
        "SEC_TO_TIME" => sint(arg(a, 0)).map(|s| Json::Str(dates::time_str(s < 0, s.abs(), 0))).unwrap_or(Json::Null),
        "TIME_TO_SEC" => to_secs_time(arg(a, 0)).map(|(neg, s, _)| json::n(if neg { -s } else { s })).unwrap_or(Json::Null),
        "CONVERT_TZ" => arg(a, 0).clone(),
        // ----------------------------------------------- flow and nulls
        "IF" => {
            need(name, a, 3, 3)?;
            if truthy(&a[0]).unwrap_or(false) {
                a[1].clone()
            } else {
                a[2].clone()
            }
        }
        "IFNULL" | "NVL" => {
            need(name, a, 2, 2)?;
            if a[0].is_null() {
                a[1].clone()
            } else {
                a[0].clone()
            }
        }
        "NULLIF" => {
            need(name, a, 2, 2)?;
            if compare(&a[0], &a[1]) == Some(Ordering::Equal) {
                Json::Null
            } else {
                a[0].clone()
            }
        }
        "COALESCE" => a.iter().find(|v| !v.is_null()).cloned().unwrap_or(Json::Null),
        "ISNULL" => b(arg(a, 0).is_null()),
        // ---------------------------------------------------------- JSON
        "JSON_EXTRACT" => {
            need(name, a, 2, usize::MAX)?;
            if a[0].is_null() {
                return Ok(Json::Null);
            }
            if a.len() == 2 {
                json_extract(&a[0], &text(&a[1])).unwrap_or(Json::Null)
            } else {
                let v: Vec<Json> = a[1..].iter().filter_map(|p| json_extract(&a[0], &text(p))).collect();
                if v.is_empty() {
                    Json::Null
                } else {
                    Json::Arr(v)
                }
            }
        }
        "JSON_UNQUOTE" => match arg(a, 0) {
            Json::Str(s) => match json::parse(s) {
                Ok(Json::Str(x)) if s.trim_start().starts_with('"') => Json::Str(x),
                _ => Json::Str(s.clone()),
            },
            Json::Null => Json::Null,
            v @ (Json::Arr(_) | Json::Obj(_)) => Json::Str(v.to_string()),
            v => v.clone(),
        },
        "JSON_OBJECT" => {
            if a.len() % 2 != 0 {
                return Err("JSON_OBJECT() takes key, value pairs".into());
            }
            Json::Obj(a.chunks(2).map(|kv| (text(&kv[0]), json_of(&kv[1]))).collect())
        }
        "JSON_ARRAY" => Json::Arr(a.iter().map(json_of).collect()),
        "JSON_LENGTH" => {
            let doc = if a.len() > 1 { json_extract(&a[0], &text(&a[1])) } else { json_doc(arg(a, 0)) };
            match doc {
                Some(Json::Arr(v)) => json::n(v.len()),
                Some(Json::Obj(v)) => json::n(v.len()),
                Some(_) => json::n(1),
                None => Json::Null,
            }
        }
        "JSON_VALID" => match arg(a, 0) {
            Json::Null => Json::Null,
            Json::Str(s) => b(json::parse(s).is_ok()),
            _ => t(),
        },
        "JSON_KEYS" => match json_doc(arg(a, 0)) {
            Some(Json::Obj(v)) => Json::Arr(v.into_iter().map(|(k, _)| Json::Str(k)).collect()),
            _ => Json::Null,
        },
        "JSON_TYPE" => match json_doc(arg(a, 0)) {
            Some(Json::Obj(_)) => Json::Str("OBJECT".into()),
            Some(Json::Arr(_)) => Json::Str("ARRAY".into()),
            Some(Json::Str(_)) => Json::Str("STRING".into()),
            Some(Json::Num(n)) => Json::Str(if n.contains('.') { "DOUBLE" } else { "INTEGER" }.into()),
            Some(Json::Bool(_)) => Json::Str("BOOLEAN".into()),
            Some(Json::Null) => Json::Str("NULL".into()),
            None => Json::Null,
        },
        "JSON_QUOTE" => Json::Str(json::quote(&s0())),
        "JSON_CONTAINS" => {
            let (Some(doc), Some(cand)) = (json_doc(arg(a, 0)), json_doc(arg(a, 1))) else { return Ok(Json::Null) };
            b(match &doc {
                Json::Arr(v) => v.contains(&cand) || matches!(&cand, Json::Arr(c) if c.iter().all(|x| v.contains(x))),
                other => *other == cand,
            })
        }
        // --------------------------------------------------------- info
        "DATABASE" | "SCHEMA" => Json::Str(env.database.to_string()),
        "USER" | "CURRENT_USER" | "SESSION_USER" | "SYSTEM_USER" => Json::Str(env.user.to_string()),
        "VERSION" => Json::Str("8.0.0-iq-tables".into()),
        "CONNECTION_ID" => json::n(1),
        "LAST_INSERT_ID" => json::n(env.last_insert_id),
        "ROW_COUNT" => json::n(env.row_count),
        "FOUND_ROWS" => json::n(env.found_rows),
        "SLEEP" | "BENCHMARK" => json::n(0),
        other => return Err(format!("Unknown function {}()", other)),
    })
}

/// Aggregate names (also usable as window functions).
pub fn is_aggregate_name(n: &str) -> bool {
    matches!(
        n,
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "GROUP_CONCAT" | "STD" | "STDDEV" | "STDDEV_POP" | "STDDEV_SAMP" | "VARIANCE" | "VAR_POP" | "VAR_SAMP" | "ANY_VALUE" | "JSON_ARRAYAGG" | "JSON_OBJECTAGG" | "BIT_AND" | "BIT_OR" | "BIT_XOR"
    )
}

/// Combine the (already evaluated) values of an aggregate.
/// For COUNT(*) pass one `1` per row; NULLs are skipped here.
pub fn aggregate(name: &str, vals: Vec<Vec<Json>>, distinct: bool, sep: &str) -> Json {
    let mut vs: Vec<Vec<Json>> = vals.into_iter().filter(|r| r.iter().all(|v| !v.is_null())).collect();
    if distinct {
        let mut seen = std::collections::HashSet::new();
        vs.retain(|r| seen.insert(r.iter().map(key).collect::<Vec<_>>().join("\u{1}")));
    }
    let first: Vec<Json> = vs.iter().map(|r| r[0].clone()).collect();
    let nums: Vec<f64> = first.iter().filter_map(num_lenient).collect();
    let var = |samp: bool| -> Json {
        let n = nums.len() as f64;
        if nums.is_empty() || (samp && nums.len() < 2) {
            return Json::Null;
        }
        let mean = nums.iter().sum::<f64>() / n;
        let ss: f64 = nums.iter().map(|x| (x - mean).powi(2)).sum();
        fmt_num(ss / if samp { n - 1.0 } else { n })
    };
    match name {
        "COUNT" => json::n(vs.len()),
        "SUM" => {
            if first.is_empty() {
                Json::Null
            } else if let Some(total) = first.iter().try_fold(0i128, |acc, v| int_text(v).and_then(|x| acc.checked_add(x))) {
                Json::Num(total.to_string())
            } else {
                let scale = first.iter().map(scale_of).max().unwrap_or(0);
                fmt_scaled(nums.iter().sum(), scale.max(if nums.iter().sum::<f64>().fract() != 0.0 { 2 } else { 0 }))
            }
        }
        "AVG" => {
            if nums.is_empty() {
                Json::Null
            } else {
                let scale = first.iter().map(scale_of).max().unwrap_or(0) + 4;
                fmt_scaled(nums.iter().sum::<f64>() / nums.len() as f64, scale)
            }
        }
        "MIN" => first.into_iter().min_by(|a, b| compare(a, b).unwrap_or(Ordering::Equal)).unwrap_or(Json::Null),
        "MAX" => first.into_iter().max_by(|a, b| compare(a, b).unwrap_or(Ordering::Equal)).unwrap_or(Json::Null),
        "ANY_VALUE" => first.into_iter().next().unwrap_or(Json::Null),
        "GROUP_CONCAT" => {
            if vs.is_empty() {
                Json::Null
            } else {
                Json::Str(vs.iter().map(|r| r.iter().map(text).collect::<String>()).collect::<Vec<_>>().join(sep))
            }
        }
        "STD" | "STDDEV" | "STDDEV_POP" => match var(false) {
            Json::Num(n) => fmt_num(n.parse::<f64>().unwrap_or(0.0).sqrt()),
            other => other,
        },
        "STDDEV_SAMP" => match var(true) {
            Json::Num(n) => fmt_num(n.parse::<f64>().unwrap_or(0.0).sqrt()),
            other => other,
        },
        "VARIANCE" | "VAR_POP" => var(false),
        "VAR_SAMP" => var(true),
        "JSON_ARRAYAGG" => Json::Arr(first),
        "JSON_OBJECTAGG" => Json::Obj(vs.iter().filter(|r| r.len() >= 2).map(|r| (text(&r[0]), r[1].clone())).collect()),
        "BIT_AND" => json::n(first.iter().filter_map(int_text).fold(u64::MAX as i128, |a, x| a & x)),
        "BIT_OR" => json::n(first.iter().filter_map(int_text).fold(0i128, |a, x| a | x)),
        "BIT_XOR" => json::n(first.iter().filter_map(int_text).fold(0i128, |a, x| a ^ x)),
        _ => Json::Null,
    }
}

/// CAST(v AS …)
pub fn cast(v: &Json, to: &super::ast::CastTo) -> Json {
    use super::ast::CastTo as C;
    if v.is_null() {
        return Json::Null;
    }
    match to {
        C::Signed | C::Unsigned => match num_lenient(v) {
            Some(x) => {
                let i = x.round() as i64;
                if matches!(to, C::Unsigned) && i < 0 {
                    Json::Num(((i as i128) + (1i128 << 64)).to_string())
                } else {
                    json::n(i)
                }
            }
            None => Json::Null,
        },
        C::Decimal(_, s) => schema::round_dec(&num_lenient(v).map(|x| x.to_string()).unwrap_or_default(), *s).map(Json::Num).unwrap_or(Json::Null),
        C::Double => num_lenient(v).map(fmt_num).unwrap_or(Json::Null),
        C::Char | C::Binary => Json::Str(text(v)),
        C::Date => to_dt(v).map(|d| Json::Str(d.date_str())).unwrap_or(Json::Null),
        C::DateTime => to_dt(v)
            .map(|mut d| {
                d.has_time = true;
                Json::Str(d.datetime_str())
            })
            .unwrap_or(Json::Null),
        C::Time => to_secs_time(v).map(|(n, s, us)| Json::Str(dates::time_str(n, s, us))).unwrap_or(Json::Null),
        C::Json => match v {
            Json::Str(s) => json::parse(s).unwrap_or(Json::Null),
            other => other.clone(),
        },
    }
}
