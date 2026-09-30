//! Small JSON parser/serializer. Numbers keep their original text so values
//! read from the chain round-trip exactly; string escaping matches the
//! browser's `JSON.stringify`, which matters for sizing inscriptions.

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> &Json {
        static NULL: Json = Json::Null;
        match self {
            Json::Obj(v) => v.iter().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or(&NULL),
            _ => &NULL,
        }
    }
    pub fn idx(&self, i: usize) -> &Json {
        static NULL: Json = Json::Null;
        match self {
            Json::Arr(v) => v.get(i).unwrap_or(&NULL),
            _ => &NULL,
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn str_or(&self, d: &str) -> String {
        self.str().unwrap_or(d).to_string()
    }
    pub fn f64(&self) -> Option<f64> {
        match self {
            Json::Num(s) => s.parse().ok(),
            Json::Str(s) => s.parse().ok(),
            _ => None,
        }
    }
    pub fn u64(&self) -> Option<u64> {
        match self {
            Json::Num(s) | Json::Str(s) => s.parse::<u64>().ok().or_else(|| s.parse::<f64>().ok().map(|f| f as u64)),
            _ => None,
        }
    }
    pub fn bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(v) => v,
            _ => &[],
        }
    }
    pub fn obj(&self) -> &[(String, Json)] {
        match self {
            Json::Obj(v) => v,
            _ => &[],
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }
    pub fn set(&mut self, key: &str, val: Json) {
        if let Json::Obj(v) = self {
            if let Some(e) = v.iter_mut().find(|(k, _)| k == key) {
                e.1 = val;
            } else {
                v.push((key.to_string(), val));
            }
        }
    }

    /// Display text for a cell: strings unquoted, everything else as JSON.
    pub fn cell_text(&self) -> String {
        match self {
            Json::Str(s) | Json::Num(s) => s.clone(),
            Json::Null => String::new(),
            other => other.to_string(),
        }
    }
}

pub fn s(v: &str) -> Json {
    Json::Str(v.to_string())
}
pub fn n<T: std::fmt::Display>(v: T) -> Json {
    Json::Num(v.to_string())
}
pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// JSON's number grammar: -?(0|[1-9][0-9]*)(.[0-9]+)?([eE][+-]?[0-9]+)?
pub fn is_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > start
    };
    if b.first() == Some(&b'-') {
        i += 1;
    }
    match b.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            digits(&mut i);
        }
        _ => return false,
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == b.len()
}

pub fn escape_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    escape_into(&mut o, s);
    o
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        write_json(&mut out, self);
        f.write_str(&out)
    }
}

fn write_json(out: &mut String, v: &Json) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        // a number that isn't one (it came from somewhere untrusted) goes out as text
        Json::Num(s) if is_number(s) => out.push_str(s),
        Json::Num(s) => escape_into(out, s),
        Json::Str(s) => escape_into(out, s),
        Json::Arr(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, x);
            }
            out.push(']');
        }
        Json::Obj(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                escape_into(out, k);
                out.push(':');
                write_json(out, x);
            }
            out.push('}');
        }
    }
}

pub fn parse(src: &str) -> Result<Json, String> {
    let mut p = Parser { b: src.as_bytes(), i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("trailing data at {}", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\n' | b'\r' | b'\t') {
            self.i += 1;
        }
    }
    fn err<T>(&self, m: &str) -> Result<T, String> {
        Err(format!("{} at {}", m, self.i))
    }
    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > 200 {
            return self.err("too deep");
        }
        match self.b.get(self.i) {
            None => self.err("unexpected end"),
            Some(b'{') => {
                self.i += 1;
                let mut v = vec![];
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Obj(v));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return self.err("expected :");
                    }
                    self.i += 1;
                    self.ws();
                    let x = self.value(depth + 1)?;
                    v.push((k, x));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Obj(v));
                        }
                        _ => return self.err("expected , or }"),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut v = vec![];
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Arr(v));
                }
                loop {
                    self.ws();
                    v.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr(v));
                        }
                        _ => return self.err("expected , or ]"),
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') if self.b[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ok(Json::Bool(true))
            }
            Some(b'f') if self.b[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ok(Json::Bool(false))
            }
            Some(b'n') if self.b[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ok(Json::Null)
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let st = self.i;
                self.i += 1;
                while self.i < self.b.len() && matches!(self.b[self.i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                    self.i += 1;
                }
                Ok(Json::Num(String::from_utf8_lossy(&self.b[st..self.i]).into_owned()))
            }
            _ => self.err("unexpected character"),
        }
    }
    fn hex4(&mut self) -> Result<u32, String> {
        if self.i + 4 > self.b.len() {
            return self.err("bad escape");
        }
        let s = std::str::from_utf8(&self.b[self.i..self.i + 4]).map_err(|_| "bad escape".to_string())?;
        let v = u32::from_str_radix(s, 16).map_err(|_| "bad escape".to_string())?;
        self.i += 4;
        Ok(v)
    }
    fn string(&mut self) -> Result<String, String> {
        if self.b.get(self.i) != Some(&b'"') {
            return self.err("expected string");
        }
        self.i += 1;
        let mut out: Vec<u8> = vec![];
        loop {
            let c = match self.b.get(self.i) {
                None => return self.err("unterminated string"),
                Some(c) => *c,
            };
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("bad escape")?;
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) && self.b.get(self.i) == Some(&b'\\') && self.b.get(self.i + 1) == Some(&b'u') {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    self.i = save;
                                }
                            }
                            let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return self.err("bad escape"),
                    }
                }
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| "invalid utf8".to_string())
    }
}
