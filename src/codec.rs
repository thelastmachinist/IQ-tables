//! Compression for packed rows: a small context-mixing compressor in the
//! spirit of lpaq (orders 1–5 + match model, logistic mixing, SSE), followed
//! by a text-safe encoding that survives being nested inside two layers of
//! JSON strings without any escaping.
//!
//! Stream layout: varint(raw_len) ++ arithmetic-coded bits.

// ------------------------------------------------------------ squash/stretch

/// 1/(1+e^-d) with d scaled by 256, output 12-bit. Integer-only (lpaq's
/// interpolation table) so every build encodes and decodes identically.
fn squash(d: i32) -> i32 {
    const T: [i32; 33] = [
        1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488, 747, 1101, 1546, 2047, 2549, 2994, 3348, 3607, 3785, 3901, 3975, 4024, 4050, 4068, 4079, 4085,
        4089, 4092, 4093, 4094,
    ];
    if d > 2047 {
        return 4095;
    }
    if d < -2047 {
        return 1;
    }
    let w = d & 127;
    let i = ((d >> 7) + 16) as usize;
    ((T[i] * (128 - w) + T[i + 1] * w + 64) >> 7).clamp(1, 4095)
}

struct Tables {
    stretch: Vec<i16>,
}

impl Tables {
    fn new() -> Self {
        // stretch(p) = ln(p/(1-p)), inverse of squash
        let mut stretch = vec![0i16; 4096];
        let mut pi = 0;
        for x in -2047..=2047 {
            let v = squash(x);
            for j in pi..=v as usize {
                stretch[j] = x as i16;
            }
            pi = v as usize + 1;
        }
        for j in pi..4096 {
            stretch[j] = 2047;
        }
        Tables { stretch }
    }
}

// ------------------------------------------------------ adaptive probability

/// Probability (upper 22 bits) + hit count (lower 10 bits), adaptive rate.
#[derive(Clone, Copy)]
struct Counter(u32);

impl Counter {
    const INIT: Counter = Counter(1 << 31);
    #[inline]
    fn p(self) -> i32 {
        (self.0 >> 20) as i32
    }
    #[inline]
    fn update(&mut self, bit: i32, limit: u32) {
        let n = self.0 & 1023;
        let p = (self.0 >> 10) as i64; // 22-bit probability
        let target = if bit != 0 { (1i64 << 22) - 1 } else { 0 };
        let np = p + (target - p) * 2 / (2 * n as i64 + 3);
        let nn = if n < limit { n + 1 } else { n };
        self.0 = ((np as u32) << 10) | nn;
    }
}

// ------------------------------------------------------------------- mixer

const LR: i32 = 2;

struct Mixer {
    n: usize,
    w: Vec<i32>,
    x: Vec<i32>,
    nx: usize,
    ctx: usize,
    pr: i32,
}

impl Mixer {
    fn new(n: usize, contexts: usize) -> Self {
        Mixer { n, w: vec![(1 << 16) / n as i32; n * contexts], x: vec![0; n], nx: 0, ctx: 0, pr: 2048 }
    }
    fn add(&mut self, st: i32) {
        self.x[self.nx] = st;
        self.nx += 1;
    }
    fn mix(&mut self, t: &Tables, ctx: usize) -> i32 {
        self.ctx = ctx;
        let w = &self.w[ctx * self.n..ctx * self.n + self.n];
        let mut dot: i64 = 0;
        for i in 0..self.nx {
            dot += self.x[i] as i64 * w[i] as i64;
        }
        let _ = t;
        self.pr = squash((dot >> 16) as i32);
        self.pr
    }
    fn update(&mut self, bit: i32) {
        let err = ((bit << 12) - self.pr) * LR;
        let base = self.ctx * self.n;
        for i in 0..self.nx {
            let dw = (self.x[i] * err) >> 10;
            self.w[base + i] = self.w[base + i].saturating_add(dw);
        }
        self.nx = 0;
    }
}

// --------------------------------------------------------------------- SSE

struct Apm {
    t: Vec<i32>,
    idx: usize,
}

impl Apm {
    fn new(n: usize, tb: &Tables) -> Self {
        let mut t = vec![0i32; n * 33];
        for i in 0..n {
            for j in 0..33 {
                t[i * 33 + j] = squash((j as i32 - 16) * 128) * 16;
            }
        }
        let _ = tb;
        Apm { t, idx: 0 }
    }
    fn pp(&mut self, tb: &Tables, pr: i32, cx: usize) -> i32 {
        let s = tb.stretch[pr as usize] as i32 + 2048;
        let lo = (s & 127) as u32;
        let j = (s >> 7) as usize;
        self.idx = j + cx * 33;
        let a = self.t[self.idx] as u32;
        let b = self.t[self.idx + 1] as u32;
        (((a * (128 - lo) + b * lo) >> 11) as i32).clamp(1, 4095)
    }
    fn update(&mut self, bit: i32) {
        let g = (bit << 16) + (bit << 7) - bit - bit;
        for k in 0..2 {
            let v = self.t[self.idx + k];
            self.t[self.idx + k] = (v + ((g - v) >> 7)).clamp(0, 65535);
        }
    }
}

// --------------------------------------------------------------- predictor

const ORDERS: usize = 6; // orders 0..=5 (order 0 is direct)
const HBITS: u32 = 17;

struct Predictor {
    tb: Tables,
    t: Vec<Vec<Counter>>,
    hashes: [u32; ORDERS],
    slots: [usize; ORDERS],
    c0: u32, // partial byte with leading 1
    c4: u32, // last 4 bytes
    bpos: u32,
    hist: Vec<u8>,
    // match model
    mm_table: Vec<u32>,
    mm_ptr: usize,
    mm_len: usize,
    mm_sm: Vec<Counter>,
    mm_slot: usize,
    mixer: Mixer,
    apm1: Apm,
    apm2: Apm,
    pr: i32,
    pr_mix: i32,
}

const MM_BITS: u32 = 16;
const APM2_BITS: u32 = 14;
const MINLEN: usize = 4;

impl Predictor {
    fn new() -> Self {
        let tb = Tables::new();
        let mut t = vec![vec![Counter::INIT; 256]];
        for _ in 1..ORDERS {
            t.push(vec![Counter::INIT; 1 << HBITS]);
        }
        let apm1 = Apm::new(256, &tb);
        let apm2 = Apm::new(1 << APM2_BITS, &tb);
        Predictor {
            tb,
            t,
            hashes: [0; ORDERS],
            slots: [0; ORDERS],
            c0: 1,
            c4: 0,
            bpos: 0,
            hist: Vec::with_capacity(1 << 14),
            mm_table: vec![0; 1 << MM_BITS],
            mm_ptr: 0,
            mm_len: 0,
            mm_sm: vec![Counter::INIT; 32],
            mm_slot: 0,
            mixer: Mixer::new(ORDERS + 2, 256),
            apm1,
            apm2,
            pr: 2048,
            pr_mix: 2048,
        }
        .primed()
    }

    fn primed(mut self) -> Self {
        self.predict();
        self
    }

    fn predict(&mut self) {
        let c0 = self.c0 as usize;
        // order 0
        self.slots[0] = c0;
        let p0 = self.t[0][c0].p();
        self.mixer.add(self.tb.stretch[p0 as usize] as i32);
        for o in 1..ORDERS {
            let idx = ((self.hashes[o].wrapping_add((c0 as u32).wrapping_mul(0x9E37_79B1))) >> (32 - HBITS)) as usize;
            self.slots[o] = idx;
            let p = self.t[o][idx].p();
            self.mixer.add(self.tb.stretch[p as usize] as i32);
        }
        // match model
        if self.mm_len > 0 && self.mm_ptr < self.hist.len() {
            let expected = self.hist[self.mm_ptr] as u32 | 0x100;
            let expected_bit = ((expected >> (7 - self.bpos)) & 1) as usize;
            // still consistent with the bits of this byte so far?
            if (expected >> (8 - self.bpos)) == self.c0 {
                let l = self.mm_len.min(15);
                self.mm_slot = l * 2 + expected_bit;
                let p = self.mm_sm[self.mm_slot].p();
                let st = self.tb.stretch[p as usize] as i32;
                self.mixer.add(st);
                self.mixer.add(if expected_bit == 1 { (l as i32) * 32 } else { -(l as i32) * 32 });
            } else {
                self.mm_len = 0;
                self.mm_slot = usize::MAX;
                self.mixer.add(0);
                self.mixer.add(0);
            }
        } else {
            self.mm_slot = usize::MAX;
            self.mixer.add(0);
            self.mixer.add(0);
        }
        let pm = self.mixer.mix(&self.tb, c0);
        self.pr_mix = pm;
        let a1 = self.apm1.pp(&self.tb, pm, c0);
        let c1 = (self.c4 & 0xff) as usize;
        let h2 = ((c0 as u32 | ((c1 as u32) << 8)).wrapping_mul(0x9E37_79B1) >> (32 - APM2_BITS)) as usize;
        let a2 = self.apm2.pp(&self.tb, pm, h2);
        self.pr = ((pm + a1 * 2 + a2 + 2) >> 2).clamp(1, 4095);
    }

    fn update(&mut self, bit: i32) {
        self.mixer.update(bit);
        self.apm1.update(bit);
        self.apm2.update(bit);
        self.t[0][self.slots[0]].update(bit, 60);
        for o in 1..ORDERS {
            self.t[o][self.slots[o]].update(bit, 255);
        }
        if self.mm_slot != usize::MAX {
            self.mm_sm[self.mm_slot].update(bit, 255);
        }
        self.c0 = (self.c0 << 1) | bit as u32;
        self.bpos += 1;
        if self.bpos == 8 {
            let byte = (self.c0 & 0xff) as u8;
            self.hist.push(byte);
            self.c4 = (self.c4 << 8) | byte as u32;
            self.c0 = 1;
            self.bpos = 0;
            // context hashes for orders 1..5
            let n = self.hist.len();
            for o in 1..ORDERS {
                let mut h: u32 = (o as u32).wrapping_mul(0x2F0B_3A49);
                for k in 0..o {
                    let b = if n > k { self.hist[n - 1 - k] } else { 0 } as u32;
                    h = (h ^ b).wrapping_mul(0x0100_0193).rotate_left(5);
                }
                self.hashes[o] = h.wrapping_mul(0x9E37_79B1);
            }
            // match model: extend or look up
            if self.mm_len > 0 && self.mm_ptr < n - 1 && self.hist[self.mm_ptr] == byte {
                self.mm_len = (self.mm_len + 1).min(65535);
                self.mm_ptr += 1;
            } else {
                self.mm_len = 0;
            }
            if n >= MINLEN {
                let mut h: u32 = 0;
                for k in 0..MINLEN {
                    h = (h ^ self.hist[n - 1 - k] as u32).wrapping_mul(0x0100_0193);
                }
                let slot = (h >> (32 - MM_BITS)) as usize;
                if self.mm_len == 0 {
                    let cand = self.mm_table[slot] as usize;
                    if cand > 0 && cand < n {
                        let mut l = 0;
                        while l < 32 && cand > l && self.hist[cand - 1 - l] == self.hist[n - 1 - l] {
                            l += 1;
                        }
                        if l >= MINLEN {
                            self.mm_len = l;
                            self.mm_ptr = cand;
                        }
                    }
                }
                self.mm_table[slot] = n as u32;
            }
        }
        self.predict();
    }
}

// ------------------------------------------------------- arithmetic coding

struct Encoder {
    x1: u32,
    x2: u32,
    out: Vec<u8>,
}

impl Encoder {
    fn bit(&mut self, p12: i32, bit: i32) {
        let range = self.x2 - self.x1;
        let xmid = self.x1 + (range >> 12) * p12 as u32 + (((range & 0xfff) * p12 as u32) >> 12);
        if bit != 0 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.out.push((self.x2 >> 24) as u8);
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        // One byte pins the final interval; the decoder pads with 0xFF.
        self.out.push((self.x1 >> 24) as u8);
        self.out
    }
}

struct Decoder<'a> {
    x1: u32,
    x2: u32,
    x: u32,
    src: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(src: &'a [u8]) -> Self {
        let mut d = Decoder { x1: 0, x2: 0xffff_ffff, x: 0, src, pos: 0 };
        for _ in 0..4 {
            d.x = (d.x << 8) | d.next() as u32;
        }
        d
    }
    fn next(&mut self) -> u8 {
        let b = *self.src.get(self.pos).unwrap_or(&255);
        self.pos += 1;
        b
    }
    fn bit(&mut self, p12: i32) -> i32 {
        let range = self.x2 - self.x1;
        let xmid = self.x1 + (range >> 12) * p12 as u32 + (((range & 0xfff) * p12 as u32) >> 12);
        let bit = if self.x <= xmid {
            self.x2 = xmid;
            1
        } else {
            self.x1 = xmid + 1;
            0
        };
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
            self.x = (self.x << 8) | self.next() as u32;
        }
        bit
    }
}

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub fn get_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v: u64 = 0;
    let mut shift = 0;
    loop {
        let c = *b.get(*i)?;
        *i += 1;
        v |= ((c & 0x7f) as u64) << shift;
        if c & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

pub fn compress(data: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    put_varint(&mut out, data.len() as u64);
    let mut p = Predictor::new();
    let mut e = Encoder { x1: 0, x2: 0xffff_ffff, out: vec![] };
    for &byte in data {
        for i in (0..8).rev() {
            let bit = ((byte >> i) & 1) as i32;
            e.bit(p.pr, bit);
            p.update(bit);
        }
    }
    out.extend(e.finish());
    out
}

pub const MAX_RAW: u64 = 1 << 24;

pub fn decompress(src: &[u8]) -> Option<Vec<u8>> {
    let mut i = 0;
    let n = get_varint(src, &mut i)?;
    if n > MAX_RAW {
        return None;
    }
    let mut p = Predictor::new();
    let mut d = Decoder::new(&src[i..]);
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let mut c = 0u32;
        for _ in 0..8 {
            let bit = d.bit(p.pr);
            p.update(bit);
            c = (c << 1) | bit as u32;
        }
        out.push(c as u8);
    }
    Some(out)
}

// ------------------------------------------------------ text-safe encoding

/// 92 printable ASCII characters: everything from `!` to `~` except `"` and
/// `\`, so the text needs no escaping inside JSON strings (at any depth).
fn alphabet() -> [u8; 92] {
    let mut a = [0u8; 92];
    let mut k = 0;
    for c in 0x21u8..=0x7e {
        if c != b'"' && c != b'\\' {
            a[k] = c;
            k += 1;
        }
    }
    a
}

/// 13 bits -> 2 characters (92² = 8464 ≥ 8192): ~23% overhead vs 33% for base64.
pub fn to_text(data: &[u8]) -> String {
    let a = alphabet();
    let mut out = String::with_capacity(data.len() * 16 / 13 + 2);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &b in data {
        acc |= (b as u32) << bits;
        bits += 8;
        if bits >= 13 {
            let v = acc & 0x1fff;
            acc >>= 13;
            bits -= 13;
            out.push(a[(v % 92) as usize] as char);
            out.push(a[(v / 92) as usize] as char);
        }
    }
    if bits > 0 {
        let v = acc & 0x1fff;
        out.push(a[(v % 92) as usize] as char);
        if bits > 6 {
            out.push(a[(v / 92) as usize] as char);
        }
    }
    out
}

pub fn from_text(s: &str) -> Option<Vec<u8>> {
    let a = alphabet();
    let mut rev = [255u8; 128];
    for (i, &c) in a.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 13 / 16 + 1);
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut i = 0;
    while i < bytes.len() {
        let lo = *rev.get(bytes[i] as usize)? as u32;
        if lo == 255 {
            return None;
        }
        let (v, nb) = if i + 1 < bytes.len() {
            let hi = *rev.get(bytes[i + 1] as usize)? as u32;
            if hi == 255 {
                return None;
            }
            (lo + hi * 92, 13)
        } else {
            (lo, 6)
        };
        i += 2;
        acc |= v << bits;
        bits += nb;
        while bits >= 8 {
            out.push((acc & 0xff) as u8);
            acc >>= 8;
            bits -= 8;
        }
    }
    Some(out)
}
