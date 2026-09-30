//! Calendar arithmetic for DATE / DATETIME / TIME values, which are stored
//! as MySQL-style text ("2026-09-28", "2026-09-28 14:05:00", "14:05:00").

pub const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
pub const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

/// Days since 1970-01-01 (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if leap(y) {
                29
            } else {
                28
            }
        }
    }
}

/// A point in time: days since the epoch plus seconds into the day.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dt {
    pub days: i64,
    pub secs: i64,
    pub micros: u32,
    /// Parsed from a value that had a time part.
    pub has_time: bool,
}

impl Dt {
    pub fn ymd(&self) -> (i64, u32, u32) {
        civil_from_days(self.days)
    }
    pub fn hms(&self) -> (i64, i64, i64) {
        (self.secs / 3600, (self.secs / 60) % 60, self.secs % 60)
    }
    pub fn date_str(&self) -> String {
        let (y, m, d) = self.ymd();
        format!("{:04}-{:02}-{:02}", y, m, d)
    }
    pub fn datetime_str(&self) -> String {
        let (h, mi, s) = self.hms();
        let base = format!("{} {:02}:{:02}:{:02}", self.date_str(), h, mi, s);
        if self.micros > 0 {
            format!("{}.{:06}", base, self.micros)
        } else {
            base
        }
    }
    pub fn unix(&self) -> i64 {
        self.days * 86400 + self.secs
    }
    pub fn from_unix(t: i64) -> Dt {
        Dt { days: t.div_euclid(86400), secs: t.rem_euclid(86400), micros: 0, has_time: true }
    }
    /// Monday = 0 … Sunday = 6.
    pub fn weekday(&self) -> u32 {
        ((self.days + 3).rem_euclid(7)) as u32
    }
    pub fn day_of_year(&self) -> u32 {
        let (y, _, _) = self.ymd();
        (self.days - days_from_civil(y, 1, 1) + 1) as u32
    }
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn valid(y: i64, m: u32, d: u32) -> bool {
    (1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m) && (0..=9999).contains(&y)
}

fn month_from_name(s: &str) -> Option<u32> {
    let l = s.to_ascii_lowercase();
    if l.len() < 3 {
        return None;
    }
    let head = l.get(..3)?;
    MONTHS.iter().position(|m| m.to_ascii_lowercase().starts_with(head) && m.to_ascii_lowercase().starts_with(&l)).map(|i| i as u32 + 1)
}

fn two_digit_year(y: i64) -> i64 {
    if y < 70 {
        2000 + y
    } else if y < 100 {
        1900 + y
    } else {
        y
    }
}

/// Dates the way people type them: 2026-09-28, 2026/9/28, 9/28/2026 (US),
/// 9/28/26, 20260928, Sep 28 2026, September 28, 2026, 28 Sep 2026.
pub fn parse_date(s: &str) -> Option<(i64, u32, u32)> {
    let t = s.trim();
    if t.len() == 8 && t.bytes().all(|b| b.is_ascii_digit()) {
        let (y, m, d) = (num(&t[..4])?, num(&t[4..6])? as u32, num(&t[6..])? as u32);
        return valid(y, m, d).then_some((y, m, d));
    }
    let parts: Vec<&str> = t.split(['-', '/', '.', ' ', ',']).filter(|p| !p.is_empty()).collect();
    if parts.len() != 3 {
        return None;
    }
    let (y, m, d) = if let (Some(a), Some(b), Some(c)) = (num(parts[0]), num(parts[1]), num(parts[2])) {
        if parts[0].len() == 4 {
            (a, b as u32, c as u32)
        } else if t.contains('.') {
            // 28.09.2026 (day first)
            (two_digit_year(c), b as u32, a as u32)
        } else {
            // 9/28/2026 (month first, US)
            (two_digit_year(c), a as u32, b as u32)
        }
    } else if let (Some(m), Some(d), Some(y)) = (month_from_name(parts[0]), num(parts[1]), num(parts[2])) {
        (two_digit_year(y), m, d as u32)
    } else if let (Some(d), Some(m), Some(y)) = (num(parts[0]), month_from_name(parts[1]), num(parts[2])) {
        (two_digit_year(y), m, d as u32)
    } else {
        return None;
    };
    valid(y, m, d).then_some((y, m, d))
}

/// Time of day (or a duration): 14:05, 14:05:30, 2:05 PM, 14:05:30.25.
/// Returns (negative, seconds, micros).
pub fn parse_time(s: &str) -> Option<(bool, i64, u32)> {
    let t = s.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r.trim()),
        None => (false, t),
    };
    let lower = t.to_ascii_lowercase();
    let (body, ampm) = if let Some(b) = lower.strip_suffix("am").or_else(|| lower.strip_suffix("a.m.")) {
        (b.trim().to_string(), Some(false))
    } else if let Some(b) = lower.strip_suffix("pm").or_else(|| lower.strip_suffix("p.m.")) {
        (b.trim().to_string(), Some(true))
    } else {
        (lower.clone(), None)
    };
    let (main, frac) = match body.split_once('.') {
        Some((a, b)) => (a.to_string(), Some(b.to_string())),
        None => (body.clone(), None),
    };
    let p: Vec<&str> = main.split(':').collect();
    let (mut h, mi, sec) = match p.len() {
        1 if ampm.is_some() => (num(p[0])?, 0, 0),
        2 => (num(p[0])?, num(p[1])?, 0),
        3 => (num(p[0])?, num(p[1])?, num(p[2])?),
        _ => return None,
    };
    if mi > 59 || sec > 59 {
        return None;
    }
    if let Some(pm) = ampm {
        if !(1..=12).contains(&h) {
            return None;
        }
        h = match (pm, h) {
            (false, 12) => 0,
            (true, 12) => 12,
            (true, x) => x + 12,
            (false, x) => x,
        };
    }
    if h > 838 {
        return None;
    }
    let micros = match frac {
        Some(f) if !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()) => {
            let mut f = f;
            f.truncate(6);
            while f.len() < 6 {
                f.push('0');
            }
            f.parse().ok()?
        }
        Some(_) => return None,
        None => 0,
    };
    Some((neg, h * 3600 + mi * 60 + sec, micros))
}

/// Date and time: "2026-09-28 14:05:00", "2026-09-28T14:05:00Z", "9/28/2026 2:05 PM",
/// or a date alone (midnight).
pub fn parse_datetime(s: &str) -> Option<Dt> {
    let t = s.trim().trim_end_matches('Z').trim_end_matches('z');
    if let Some((y, m, d)) = parse_date(t) {
        return Some(Dt { days: days_from_civil(y, m, d), secs: 0, micros: 0, has_time: false });
    }
    // split at 'T' or the first space that is followed by a digit
    let idx = t.find('T').or_else(|| {
        let b = t.as_bytes();
        (0..b.len()).filter(|&i| b[i] == b' ').find(|&i| {
            let rest = &t[i + 1..];
            parse_time(rest).is_some() && parse_date(&t[..i]).is_some()
        })
    })?;
    let (dp, tp) = (&t[..idx], &t[idx + 1..]);
    let (y, m, d) = parse_date(dp)?;
    // strip a trailing +hh:mm offset (kept as given; no conversion)
    let tp = match tp.rfind('+') {
        Some(p) if p > 0 => &tp[..p],
        _ => tp,
    };
    let (neg, secs, micros) = parse_time(tp)?;
    if neg || secs >= 86400 {
        return None;
    }
    Some(Dt { days: days_from_civil(y, m, d), secs, micros, has_time: true })
}

pub fn time_str(neg: bool, secs: i64, micros: u32) -> String {
    let base = format!("{}{:02}:{:02}:{:02}", if neg { "-" } else { "" }, secs / 3600, (secs / 60) % 60, secs % 60);
    if micros > 0 {
        format!("{}.{:06}", base, micros)
    } else {
        base
    }
}

/// The browser's local time right now.
#[cfg(feature = "app")]
pub fn now_local() -> Dt {
    let ms = crate::host::now_ms() as i64 + crate::host::tz_offset_min() as i64 * 60_000;
    let mut d = Dt::from_unix(ms.div_euclid(1000));
    d.micros = (ms.rem_euclid(1000) * 1000) as u32;
    d
}

#[cfg(feature = "app")]
pub fn now_utc() -> Dt {
    let ms = crate::host::now_ms() as i64;
    Dt::from_unix(ms.div_euclid(1000))
}

/// Add an interval. Months and years clamp the day (Jan 31 + 1 month = Feb 28).
pub fn add(dt: Dt, n: i64, unit: &str) -> Option<Dt> {
    let mut d = dt;
    match unit {
        "MICROSECOND" => {
            let total = d.secs as i128 * 1_000_000 + d.micros as i128 + n as i128;
            let day_us = 86_400_000_000i128;
            d.days += total.div_euclid(day_us) as i64;
            let r = total.rem_euclid(day_us);
            d.secs = (r / 1_000_000) as i64;
            d.micros = (r % 1_000_000) as u32;
            d.has_time = true;
        }
        "SECOND" | "MINUTE" | "HOUR" => {
            let k = match unit {
                "SECOND" => 1,
                "MINUTE" => 60,
                _ => 3600,
            };
            let total = d.secs + n * k;
            d.days += total.div_euclid(86400);
            d.secs = total.rem_euclid(86400);
            d.has_time = true;
        }
        "DAY" => d.days += n,
        "WEEK" => d.days += 7 * n,
        "MONTH" | "QUARTER" | "YEAR" => {
            let months = match unit {
                "MONTH" => n,
                "QUARTER" => 3 * n,
                _ => 12 * n,
            };
            let (y, m, day) = d.ymd();
            let total = y * 12 + (m as i64 - 1) + months;
            let (ny, nm) = (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32);
            if !(0..=9999).contains(&ny) {
                return None;
            }
            d.days = days_from_civil(ny, nm, day.min(days_in_month(ny, nm)));
        }
        _ => return None,
    }
    Some(d)
}

fn ordinal(n: u32) -> String {
    let suf = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{}{}", n, suf)
}

/// MySQL week number, mode 0 (weeks start on Sunday; days before the first
/// Sunday are week 0).
pub fn week0(dt: &Dt) -> u32 {
    let (y, _, _) = dt.ymd();
    let jan1 = Dt { days: days_from_civil(y, 1, 1), secs: 0, micros: 0, has_time: false };
    let jan1_sun = (jan1.weekday() + 1) % 7; // Sunday = 0
    let first_sunday = if jan1_sun == 0 { 0 } else { 7 - jan1_sun };
    let doy = dt.day_of_year() as i64 - 1;
    if doy < first_sunday as i64 {
        0
    } else {
        ((doy - first_sunday as i64) / 7 + 1) as u32
    }
}

/// DATE_FORMAT with MySQL's specifiers.
pub fn format(dt: &Dt, fmt: &str) -> String {
    let (y, m, d) = dt.ymd();
    let (h, mi, s) = dt.hms();
    let h12 = if h % 12 == 0 { 12 } else { h % 12 };
    let mut out = String::new();
    let mut it = fmt.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(f) = it.next() else { break };
        match f {
            'Y' => out.push_str(&format!("{:04}", y)),
            'y' => out.push_str(&format!("{:02}", y % 100)),
            'm' => out.push_str(&format!("{:02}", m)),
            'c' => out.push_str(&m.to_string()),
            'M' => out.push_str(MONTHS[m as usize - 1]),
            'b' => out.push_str(&MONTHS[m as usize - 1][..3]),
            'd' => out.push_str(&format!("{:02}", d)),
            'e' => out.push_str(&d.to_string()),
            'D' => out.push_str(&ordinal(d)),
            'H' => out.push_str(&format!("{:02}", h)),
            'k' => out.push_str(&h.to_string()),
            'h' | 'I' => out.push_str(&format!("{:02}", h12)),
            'l' => out.push_str(&h12.to_string()),
            'i' => out.push_str(&format!("{:02}", mi)),
            's' | 'S' => out.push_str(&format!("{:02}", s)),
            'f' => out.push_str(&format!("{:06}", dt.micros)),
            'p' => out.push_str(if h < 12 { "AM" } else { "PM" }),
            'r' => out.push_str(&format!("{:02}:{:02}:{:02} {}", h12, mi, s, if h < 12 { "AM" } else { "PM" })),
            'T' => out.push_str(&format!("{:02}:{:02}:{:02}", h, mi, s)),
            'W' => out.push_str(DAYS[dt.weekday() as usize]),
            'a' => out.push_str(&DAYS[dt.weekday() as usize][..3]),
            'w' => out.push_str(&((dt.weekday() + 1) % 7).to_string()),
            'j' => out.push_str(&format!("{:03}", dt.day_of_year())),
            'U' => out.push_str(&format!("{:02}", week0(dt))),
            '%' => out.push('%'),
            other => out.push(other),
        }
    }
    out
}

/// STR_TO_DATE for the common specifiers.
pub fn parse_with(text: &str, fmt: &str) -> Option<Dt> {
    let t: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    let (mut y, mut m, mut d) = (2000i64, 1u32, 1u32);
    let (mut h, mut mi, mut s) = (0i64, 0i64, 0i64);
    let mut pm: Option<bool> = None;
    let mut has_date = false;
    let mut has_time = false;
    let digits = |i: &mut usize, max: usize| -> Option<i64> {
        let st = *i;
        while *i < t.len() && *i - st < max && t[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == st {
            return None;
        }
        t[st..*i].iter().collect::<String>().parse().ok()
    };
    let mut it = fmt.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            if i < t.len() && (t[i] == c || (c == ' ' && t[i].is_whitespace())) {
                i += 1;
                continue;
            }
            return None;
        }
        match it.next()? {
            'Y' => {
                y = digits(&mut i, 4)?;
                has_date = true;
            }
            'y' => {
                y = two_digit_year(digits(&mut i, 2)?);
                has_date = true;
            }
            'm' | 'c' => {
                m = digits(&mut i, 2)? as u32;
                has_date = true;
            }
            'd' | 'e' => {
                d = digits(&mut i, 2)? as u32;
                has_date = true;
            }
            'M' | 'b' => {
                let st = i;
                while i < t.len() && t[i].is_alphabetic() {
                    i += 1;
                }
                m = month_from_name(&t[st..i].iter().collect::<String>())?;
                has_date = true;
            }
            'H' | 'k' | 'h' | 'I' | 'l' => {
                h = digits(&mut i, 2)?;
                has_time = true;
            }
            'i' => {
                mi = digits(&mut i, 2)?;
                has_time = true;
            }
            's' | 'S' => {
                s = digits(&mut i, 2)?;
                has_time = true;
            }
            'p' => {
                let w: String = t[i..(i + 2).min(t.len())].iter().collect::<String>().to_ascii_uppercase();
                pm = Some(w == "PM");
                i += 2;
            }
            'T' => {
                h = digits(&mut i, 2)?;
                if t.get(i) != Some(&':') {
                    return None;
                }
                i += 1;
                mi = digits(&mut i, 2)?;
                if t.get(i) != Some(&':') {
                    return None;
                }
                i += 1;
                s = digits(&mut i, 2)?;
                has_time = true;
            }
            '%' => {
                if t.get(i) != Some(&'%') {
                    return None;
                }
                i += 1;
            }
            _ => return None,
        }
    }
    if let Some(p) = pm {
        if h == 12 {
            h = 0;
        }
        if p {
            h += 12;
        }
    }
    if has_date && !valid(y, m, d) {
        return None;
    }
    if mi > 59 || s > 59 || h > 23 {
        return None;
    }
    Some(Dt { days: days_from_civil(y, m, d), secs: h * 3600 + mi * 60 + s, micros: 0, has_time })
}
