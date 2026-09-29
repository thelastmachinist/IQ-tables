//! HTML helpers and formatting.

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

pub fn sol(lamports: u64) -> String {
    let whole = lamports / 1_000_000_000;
    let frac = lamports % 1_000_000_000;
    let mut f = format!("{:09}", frac);
    while f.len() > 4 && f.ends_with('0') {
        f.pop();
    }
    format!("{}.{} SOL", whole, f)
}

pub fn parse_sol(s: &str) -> Option<u64> {
    let s = s.trim().trim_end_matches("SOL").trim();
    let (w, f) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    if f.len() > 9 || (w.is_empty() && f.is_empty()) {
        return None;
    }
    let w: u64 = if w.is_empty() { 0 } else { w.parse().ok()? };
    let mut fp = f.to_string();
    while fp.len() < 9 {
        fp.push('0');
    }
    let f: u64 = fp.parse().ok()?;
    w.checked_mul(1_000_000_000)?.checked_add(f)
}

/// A size in bytes for people: "512 B", "3.4 KB", "2.19 MB", "1.50 GB".
/// (u64: the wasm build's usize is 32 bits, too small for big files.)
pub fn bytes(n: impl TryInto<u64>) -> String {
    let n: u64 = n.try_into().unwrap_or(u64::MAX);
    let f = n as f64;
    if n < 1 << 10 {
        format!("{} B", n)
    } else if n < 1 << 20 {
        format!("{:.1} KB", f / 1024.0)
    } else if n < 1 << 30 {
        format!("{:.2} MB", f / (1u64 << 20) as f64)
    } else {
        format!("{:.2} GB", f / (1u64 << 30) as f64)
    }
}

/// Unix seconds -> "2026-09-28 17:40 UTC".
pub fn time(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Howard Hinnant's civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02} {:02}:{:02} UTC", y, m, d, rem / 3600, (rem % 3600) / 60)
}

pub fn addr(a: &str) -> String {
    format!("<span class=\"addr\" title=\"{}\">{}</span>", esc(a), esc(&crate::solana::short(a)))
}

pub fn solscan_tx(sig: &str, cluster: &str) -> String {
    let q = if cluster == "devnet" { "?cluster=devnet" } else { "" };
    format!("https://solscan.io/tx/{}{}", sig, q)
}

pub fn solscan_account(a: &str, cluster: &str) -> String {
    let q = if cluster == "devnet" { "?cluster=devnet" } else { "" };
    format!("https://solscan.io/account/{}{}", a, q)
}

/// Compare two cell values: numbers numerically, otherwise case-insensitive.
pub fn cmp_cells(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        _ => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

/// RFC 4180 CSV (also accepts tab-separated input).
pub fn parse_csv(src: &str) -> Vec<Vec<String>> {
    let delim = {
        let first = src.lines().next().unwrap_or("");
        if first.matches('\t').count() > first.matches(',').count() {
            '\t'
        } else {
            ','
        }
    };
    let mut rows = vec![];
    let mut row: Vec<String> = vec![];
    let mut cell = String::new();
    let mut q = false;
    let mut it = src.chars().peekable();
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
        } else if c == delim {
            row.push(std::mem::take(&mut cell));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && it.peek() == Some(&'\n') {
                it.next();
            }
            row.push(std::mem::take(&mut cell));
            if !(row.len() == 1 && row[0].is_empty()) {
                rows.push(std::mem::take(&mut row));
            } else {
                row.clear();
            }
        } else {
            cell.push(c);
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

pub fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Interpret imported text: plain decimal numbers become numbers, empty
/// becomes null, everything else stays a string (no guessing at dates etc.).
pub fn typed(s: &str) -> crate::json::Json {
    use crate::json::Json;
    let t = s.trim();
    if t.is_empty() {
        return Json::Null;
    }
    let b = t.as_bytes();
    let digits = |x: &[u8]| !x.is_empty() && x.iter().all(|c| c.is_ascii_digit());
    let (neg, body) = if b[0] == b'-' { (true, &b[1..]) } else { (false, b) };
    let _ = neg;
    let ok = match body.iter().position(|&c| c == b'.') {
        None => digits(body) && (body.len() == 1 || body[0] != b'0') && body.len() <= 15,
        Some(p) => digits(&body[..p]) && digits(&body[p + 1..]) && (p == 1 || body[0] != b'0') && body.len() <= 17,
    };
    if ok {
        Json::Num(t.to_string())
    } else {
        Json::Str(s.to_string())
    }
}
