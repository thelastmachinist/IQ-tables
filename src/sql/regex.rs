//! A small backtracking regular-expression engine for REGEXP / RLIKE and
//! the REGEXP_* functions: . [] [^] ^ $ | () (?:) * + ? {n,m} (and lazy
//! forms), \d \w \s \b and POSIX classes like [[:alpha:]]. Case-insensitive
//! by default, like MySQL's default collation.

#[derive(Clone, Debug)]
enum Item {
    Ch(char),
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Clone, Debug)]
enum Node {
    Ch(char),
    Any,
    Class(Vec<Item>, bool),
    Start,
    End,
    WordB(bool),
    Group(Vec<Vec<Node>>),
    Rep(Box<Node>, usize, usize, bool),
}

pub struct Regex {
    alts: Vec<Vec<Node>>,
    ci: bool,
}

struct P<'a> {
    c: &'a [char],
    i: usize,
}

impl<'a> P<'a> {
    fn alts(&mut self) -> Result<Vec<Vec<Node>>, String> {
        let mut alts = vec![self.seq()?];
        while self.i < self.c.len() && self.c[self.i] == '|' {
            self.i += 1;
            alts.push(self.seq()?);
        }
        Ok(alts)
    }
    fn seq(&mut self) -> Result<Vec<Node>, String> {
        let mut v = vec![];
        while self.i < self.c.len() && self.c[self.i] != '|' && self.c[self.i] != ')' {
            let atom = self.atom()?;
            let atom = self.quant(atom)?;
            v.push(atom);
        }
        Ok(v)
    }
    fn quant(&mut self, a: Node) -> Result<Node, String> {
        if self.i >= self.c.len() {
            return Ok(a);
        }
        let (min, max) = match self.c[self.i] {
            '*' => {
                self.i += 1;
                (0, usize::MAX)
            }
            '+' => {
                self.i += 1;
                (1, usize::MAX)
            }
            '?' => {
                self.i += 1;
                (0, 1)
            }
            '{' => {
                let st = self.i;
                self.i += 1;
                let num = |p: &mut P| {
                    let s = p.i;
                    while p.i < p.c.len() && p.c[p.i].is_ascii_digit() {
                        p.i += 1;
                    }
                    p.c[s..p.i].iter().collect::<String>().parse::<usize>().ok()
                };
                let Some(a) = num(self) else {
                    self.i = st;
                    return Ok(a_lit(a));
                };
                let b = if self.i < self.c.len() && self.c[self.i] == ',' {
                    self.i += 1;
                    num(self).unwrap_or(usize::MAX)
                } else {
                    a
                };
                if self.i >= self.c.len() || self.c[self.i] != '}' {
                    return Err("Bad {n,m} in the pattern".into());
                }
                self.i += 1;
                (a, b)
            }
            _ => return Ok(a),
        };
        let lazy = self.i < self.c.len() && self.c[self.i] == '?';
        if lazy {
            self.i += 1;
        }
        Ok(Node::Rep(Box::new(a), min, max, !lazy))
    }
    fn esc(&mut self) -> Result<Item, String> {
        let c = *self.c.get(self.i).ok_or("Pattern ends with \\")?;
        self.i += 1;
        Ok(match c {
            'd' => Item::Digit(true),
            'D' => Item::Digit(false),
            'w' => Item::Word(true),
            'W' => Item::Word(false),
            's' => Item::Space(true),
            'S' => Item::Space(false),
            'n' => Item::Ch('\n'),
            't' => Item::Ch('\t'),
            'r' => Item::Ch('\r'),
            x => Item::Ch(x),
        })
    }
    fn atom(&mut self) -> Result<Node, String> {
        let c = self.c[self.i];
        self.i += 1;
        Ok(match c {
            '.' => Node::Any,
            '^' => Node::Start,
            '$' => Node::End,
            '(' => {
                if self.c.get(self.i) == Some(&'?') && self.c.get(self.i + 1) == Some(&':') {
                    self.i += 2;
                }
                let a = self.alts()?;
                if self.c.get(self.i) != Some(&')') {
                    return Err("Unclosed ( in the pattern".into());
                }
                self.i += 1;
                Node::Group(a)
            }
            '[' => self.class()?,
            '\\' => {
                let c = *self.c.get(self.i).ok_or("Pattern ends with \\")?;
                if c == 'b' || c == 'B' {
                    self.i += 1;
                    Node::WordB(c == 'b')
                } else {
                    match self.esc()? {
                        Item::Ch(x) => Node::Ch(x),
                        it => Node::Class(vec![it], false),
                    }
                }
            }
            x => Node::Ch(x),
        })
    }
    fn class(&mut self) -> Result<Node, String> {
        let neg = self.c.get(self.i) == Some(&'^');
        if neg {
            self.i += 1;
        }
        let mut items = vec![];
        let mut first = true;
        loop {
            let Some(&c) = self.c.get(self.i) else { return Err("Unclosed [ in the pattern".into()) };
            if c == ']' && !first {
                self.i += 1;
                break;
            }
            first = false;
            if c == '[' && self.c.get(self.i + 1) == Some(&':') {
                let rest: String = self.c[self.i..].iter().collect();
                if let Some(end) = rest.find(":]") {
                    let name = &rest[2..end];
                    self.i += end + 2;
                    match name {
                        "alpha" => items.extend([Item::Range('a', 'z'), Item::Range('A', 'Z')]),
                        "digit" => items.push(Item::Digit(true)),
                        "alnum" => items.extend([Item::Range('a', 'z'), Item::Range('A', 'Z'), Item::Digit(true)]),
                        "space" => items.push(Item::Space(true)),
                        "upper" => items.push(Item::Range('A', 'Z')),
                        "lower" => items.push(Item::Range('a', 'z')),
                        "punct" => items.extend("!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~".chars().map(Item::Ch)),
                        "xdigit" => items.extend([Item::Digit(true), Item::Range('a', 'f'), Item::Range('A', 'F')]),
                        "word" => items.push(Item::Word(true)),
                        _ => return Err(format!("Unknown class [:{}:]", name)),
                    }
                    continue;
                }
            }
            self.i += 1;
            let it = if c == '\\' { self.esc()? } else { Item::Ch(c) };
            if let Item::Ch(lo) = it {
                if self.c.get(self.i) == Some(&'-') && self.c.get(self.i + 1).map(|x| *x != ']').unwrap_or(false) {
                    self.i += 1;
                    let hi = self.c[self.i];
                    self.i += 1;
                    items.push(Item::Range(lo, hi));
                    continue;
                }
            }
            items.push(it);
        }
        Ok(Node::Class(items, neg))
    }
}

fn a_lit(n: Node) -> Node {
    n
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Regex {
    pub fn new(pat: &str, ci: bool) -> Result<Regex, String> {
        let c: Vec<char> = pat.chars().collect();
        let mut p = P { c: &c, i: 0 };
        let alts = p.alts()?;
        if p.i < c.len() {
            return Err("Unmatched ) in the pattern".into());
        }
        Ok(Regex { alts, ci })
    }

    fn eq(&self, a: char, b: char) -> bool {
        a == b || (self.ci && a.to_lowercase().eq(b.to_lowercase()))
    }

    fn item(&self, it: &Item, c: char) -> bool {
        match it {
            Item::Ch(x) => self.eq(*x, c),
            Item::Range(a, b) => {
                (*a <= c && c <= *b)
                    || (self.ci && {
                        let l = c.to_lowercase().next().unwrap_or(c);
                        let u = c.to_uppercase().next().unwrap_or(c);
                        (*a <= l && l <= *b) || (*a <= u && u <= *b)
                    })
            }
            Item::Digit(p) => c.is_ascii_digit() == *p,
            Item::Word(p) => is_word(c) == *p,
            Item::Space(p) => c.is_whitespace() == *p,
        }
    }

    fn one(&self, n: &Node, t: &[char], i: usize) -> bool {
        match n {
            Node::Ch(c) => i < t.len() && self.eq(*c, t[i]),
            Node::Any => i < t.len() && t[i] != '\n',
            Node::Class(items, neg) => i < t.len() && items.iter().any(|it| self.item(it, t[i])) != *neg,
            _ => false,
        }
    }

    /// Match `seq[k..]` at position `i`, then call `next` with the end position.
    fn m(&self, seq: &[Node], k: usize, t: &[char], i: usize, next: &mut dyn FnMut(usize) -> bool, depth: usize) -> bool {
        if depth > 5000 {
            return false;
        }
        if k == seq.len() {
            return next(i);
        }
        match &seq[k] {
            Node::Start => i == 0 && self.m(seq, k + 1, t, i, next, depth + 1),
            Node::End => i == t.len() && self.m(seq, k + 1, t, i, next, depth + 1),
            Node::WordB(b) => {
                let before = i > 0 && is_word(t[i - 1]);
                let after = i < t.len() && is_word(t[i]);
                ((before != after) == *b) && self.m(seq, k + 1, t, i, next, depth + 1)
            }
            Node::Group(alts) => {
                for a in alts {
                    if self.m(a, 0, t, i, &mut |j| self.m(seq, k + 1, t, j, next, depth + 1), depth + 1) {
                        return true;
                    }
                }
                false
            }
            Node::Rep(node, min, max, greedy) => self.rep(node, *min, *max, *greedy, 0, seq, k, t, i, next, depth + 1),
            simple => self.one(simple, t, i) && self.m(seq, k + 1, t, i + 1, next, depth + 1),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rep(
        &self,
        node: &Node,
        min: usize,
        max: usize,
        greedy: bool,
        count: usize,
        seq: &[Node],
        k: usize,
        t: &[char],
        i: usize,
        next: &mut dyn FnMut(usize) -> bool,
        depth: usize,
    ) -> bool {
        if depth > 5000 {
            return false;
        }
        let try_more = |next: &mut dyn FnMut(usize) -> bool| -> bool {
            if count >= max {
                return false;
            }
            let single = std::slice::from_ref(node);
            self.m(single, 0, t, i, &mut |j| j != i && self.rep(node, min, max, greedy, count + 1, seq, k, t, j, next, depth + 1), depth + 1)
        };
        if greedy {
            if try_more(next) {
                return true;
            }
            count >= min && self.m(seq, k + 1, t, i, next, depth + 1)
        } else {
            if count >= min && self.m(seq, k + 1, t, i, next, depth + 1) {
                return true;
            }
            try_more(next)
        }
    }

    /// First match at or after `from`: (start, end) in chars.
    pub fn find_at(&self, t: &[char], from: usize) -> Option<(usize, usize)> {
        for st in from..=t.len() {
            for a in &self.alts {
                let mut end = None;
                if self.m(
                    a,
                    0,
                    t,
                    st,
                    &mut |j| {
                        end = Some(j);
                        true
                    },
                    0,
                ) {
                    return end.map(|e| (st, e));
                }
            }
        }
        None
    }

    pub fn is_match(&self, s: &str) -> bool {
        let t: Vec<char> = s.chars().collect();
        self.find_at(&t, 0).is_some()
    }

    /// Replace matches (all when occurrence = 0, else only the n-th).
    pub fn replace(&self, s: &str, rep: &str, pos: usize, occurrence: usize) -> String {
        let t: Vec<char> = s.chars().collect();
        let mut out: String = t[..pos.min(t.len())].iter().collect();
        let mut i = pos.min(t.len());
        let mut n = 0;
        while i <= t.len() {
            let Some((a, b)) = self.find_at(&t, i) else { break };
            n += 1;
            out.extend(&t[i..a]);
            if occurrence == 0 || n == occurrence {
                out.push_str(rep);
            } else {
                out.extend(&t[a..b]);
            }
            if b == a {
                if a < t.len() {
                    out.push(t[a]);
                }
                i = a + 1;
            } else {
                i = b;
            }
            if occurrence != 0 && n >= occurrence {
                break;
            }
        }
        if i < t.len() {
            out.extend(&t[i..]);
        }
        out
    }
}
