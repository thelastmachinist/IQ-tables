//! SQL tokens (MySQL flavour): `backtick` names, 'single' and "double"
//! quoted strings, @variables, and the usual comments.

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    /// Keyword or bare name (original case).
    Word(String),
    /// `quoted name`
    Ident(String),
    Str(String),
    Num(String),
    /// @name (user variable) or @@name (system variable)
    Var(String),
    Sym(&'static str),
}

#[derive(Clone, Debug)]
pub struct Token {
    pub t: Tok,
    /// Byte offsets into the source.
    pub pos: usize,
    pub end: usize,
}

const SYMS3: [&str; 3] = ["<=>", "->>", "..."];
const SYMS2: [&str; 13] = ["<=", ">=", "<>", "!=", "==", "||", "&&", "<<", ">>", "->", ":=", "::", "%%"];
const SYMS1: &str = "=<>(),;*+-/%.!~&|^?{}[]:";

fn sym(s: &str) -> &'static str {
    match s {
        "<=>" => "<=>",
        "->>" => "->>",
        "<=" => "<=",
        ">=" => ">=",
        "<>" | "!=" => "!=",
        "==" => "=",
        "||" => "||",
        "&&" => "&&",
        "<<" => "<<",
        ">>" => ">>",
        "->" => "->",
        ":=" => ":=",
        "=" => "=",
        "<" => "<",
        ">" => ">",
        "(" => "(",
        ")" => ")",
        "," => ",",
        ";" => ";",
        "*" => "*",
        "+" => "+",
        "-" => "-",
        "/" => "/",
        "%" => "%",
        "." => ".",
        "!" => "!",
        "~" => "~",
        "&" => "&",
        "|" => "|",
        "^" => "^",
        "?" => "?",
        "{" => "{",
        "}" => "}",
        "[" => "[",
        "]" => "]",
        ":" => ":",
        _ => "?",
    }
}

pub fn tokenize(src: &str) -> Result<Vec<Token>, String> {
    let b: Vec<(usize, char)> = src.char_indices().collect();
    let n = b.len();
    let at = |i: usize| if i < n { b[i].1 } else { '\0' };
    let off = |i: usize| if i < n { b[i].0 } else { src.len() };
    let mut i = 0;
    let mut out: Vec<Token> = vec![];
    while i < n {
        let c = b[i].1;
        let st = i;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if (c == '-' && at(i + 1) == '-') || c == '#' {
            while i < n && b[i].1 != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && at(i + 1) == '*' {
            i += 2;
            while i < n && !(b[i].1 == '*' && at(i + 1) == '/') {
                i += 1;
            }
            if i >= n {
                return Err("Unclosed /* comment".into());
            }
            i += 2;
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            let q = c;
            i += 1;
            let mut s = String::new();
            loop {
                if i >= n {
                    return Err(format!("Unclosed {} quote", q));
                }
                let ch = b[i].1;
                if ch == q {
                    if at(i + 1) == q {
                        s.push(q);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if ch == '\\' && q != '`' && i + 1 < n {
                    i += 1;
                    let e = b[i].1;
                    match e {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        'r' => s.push('\r'),
                        '0' => s.push('\0'),
                        'b' => s.push('\u{8}'),
                        'Z' => s.push('\u{1a}'),
                        // LIKE wildcards keep their backslash
                        '%' | '_' => {
                            s.push('\\');
                            s.push(e);
                        }
                        x => s.push(x),
                    }
                    i += 1;
                    continue;
                }
                s.push(ch);
                i += 1;
            }
            let t = if q == '`' { Tok::Ident(s) } else { Tok::Str(s) };
            // adjacent string literals join: 'a' 'b' = 'ab'
            if let (Tok::Str(s2), Some(Token { t: Tok::Str(prev), end, .. })) = (&t, out.last_mut()) {
                if src[*end..off(st)].trim().is_empty() && !src[*end..off(st)].is_empty() {
                    prev.push_str(s2);
                    *end = off(i);
                    continue;
                }
            }
            out.push(Token { t, pos: off(st), end: off(i) });
            continue;
        }
        // X'4142' hex strings
        if (c == 'x' || c == 'X') && at(i + 1) == '\'' {
            i += 2;
            let mut hex = String::new();
            while i < n && b[i].1 != '\'' {
                hex.push(b[i].1);
                i += 1;
            }
            i += 1;
            let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|k| u8::from_str_radix(&hex[2 * k..2 * k + 2], 16).ok()).collect();
            out.push(Token { t: Tok::Str(String::from_utf8_lossy(&bytes).into_owned()), pos: off(st), end: off(i) });
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && at(i + 1).is_ascii_digit() && !matches!(out.last(), Some(Token { t: Tok::Word(_) | Tok::Ident(_), .. }))) {
            if c == '0' && (at(i + 1) == 'x' || at(i + 1) == 'X') && at(i + 2).is_ascii_hexdigit() {
                i += 2;
                let hs = i;
                while i < n && b[i].1.is_ascii_hexdigit() {
                    i += 1;
                }
                let h: String = b[hs..i].iter().map(|x| x.1).collect();
                let v = u128::from_str_radix(&h, 16).map_err(|_| "Hex number too large")?;
                out.push(Token { t: Tok::Num(v.to_string()), pos: off(st), end: off(i) });
                continue;
            }
            while i < n && (b[i].1.is_ascii_digit() || b[i].1 == '.') {
                i += 1;
            }
            if (at(i) == 'e' || at(i) == 'E') && (at(i + 1).is_ascii_digit() || ((at(i + 1) == '-' || at(i + 1) == '+') && at(i + 2).is_ascii_digit())) {
                i += 2;
                while i < n && b[i].1.is_ascii_digit() {
                    i += 1;
                }
            }
            // 1col is a valid MySQL name
            if at(i).is_alphabetic() || at(i) == '_' {
                while i < n && (b[i].1.is_alphanumeric() || b[i].1 == '_' || b[i].1 == '$') {
                    i += 1;
                }
                out.push(Token { t: Tok::Word(src[off(st)..off(i)].to_string()), pos: off(st), end: off(i) });
                continue;
            }
            out.push(Token { t: Tok::Num(src[off(st)..off(i)].to_string()), pos: off(st), end: off(i) });
            continue;
        }
        if c.is_alphabetic() || c == '_' || c == '$' || (c as u32) > 127 {
            while i < n && (b[i].1.is_alphanumeric() || b[i].1 == '_' || b[i].1 == '$' || (b[i].1 as u32) > 127 && !b[i].1.is_whitespace()) {
                i += 1;
            }
            out.push(Token { t: Tok::Word(src[off(st)..off(i)].to_string()), pos: off(st), end: off(i) });
            continue;
        }
        if c == '@' {
            i += 1;
            if at(i) == '@' {
                i += 1;
            }
            let vs = i;
            if at(i) == '`' || at(i) == '\'' || at(i) == '"' {
                let q = at(i);
                i += 1;
                while i < n && b[i].1 != q {
                    i += 1;
                }
                let name: String = b[vs + 1..i].iter().map(|x| x.1).collect();
                i += 1;
                out.push(Token { t: Tok::Var(name.to_lowercase()), pos: off(st), end: off(i) });
                continue;
            }
            while i < n && (b[i].1.is_alphanumeric() || b[i].1 == '_' || b[i].1 == '.' || b[i].1 == '$') {
                i += 1;
            }
            let name: String = b[vs..i].iter().map(|x| x.1).collect();
            out.push(Token { t: Tok::Var(name.to_lowercase()), pos: off(st), end: off(i) });
            continue;
        }
        let three: String = b[i..(i + 3).min(n)].iter().map(|x| x.1).collect();
        if SYMS3.contains(&three.as_str()) {
            i += 3;
            out.push(Token { t: Tok::Sym(sym(&three)), pos: off(st), end: off(i) });
            continue;
        }
        let two: String = b[i..(i + 2).min(n)].iter().map(|x| x.1).collect();
        if SYMS2.contains(&two.as_str()) {
            i += 2;
            out.push(Token { t: Tok::Sym(sym(&two)), pos: off(st), end: off(i) });
            continue;
        }
        if SYMS1.contains(c) {
            i += 1;
            out.push(Token { t: Tok::Sym(sym(&c.to_string())), pos: off(st), end: off(i) });
            continue;
        }
        return Err(format!("Unexpected character '{}'", c));
    }
    Ok(out)
}

pub fn show(t: &Tok) -> String {
    match t {
        Tok::Word(w) => format!("\"{}\"", w),
        Tok::Ident(w) => format!("`{}`", w),
        Tok::Str(s) => format!("'{}'", s.chars().take(30).collect::<String>()),
        Tok::Num(n) => n.clone(),
        Tok::Var(v) => format!("@{}", v),
        Tok::Sym(s) => format!("'{}'", s),
    }
}
