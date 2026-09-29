//! Minimal QR code encoder (byte mode, error correction level M, versions
//! 1–10) for donation addresses. Follows the structure of Project Nayuki's
//! reference implementation (MIT).

const ECC_PER_BLOCK_M: [usize; 11] = [0, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26];
const BLOCKS_M: [usize; 11] = [0, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5];

fn raw_modules(ver: usize) -> usize {
    let mut r = (16 * ver + 128) * ver + 64;
    if ver >= 2 {
        let na = ver / 7 + 2;
        r -= (25 * na - 10) * na - 55;
        if ver >= 7 {
            r -= 36;
        }
    }
    r
}

fn data_capacity(ver: usize) -> usize {
    raw_modules(ver) / 8 - ECC_PER_BLOCK_M[ver] * BLOCKS_M[ver]
}

fn gf_mul(x: u8, y: u8) -> u8 {
    let mut z: u32 = 0;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= ((y as u32 >> i) & 1) * x as u32;
    }
    z as u8
}

fn rs_divisor(deg: usize) -> Vec<u8> {
    let mut r = vec![0u8; deg];
    r[deg - 1] = 1;
    let mut root: u8 = 1;
    for _ in 0..deg {
        for j in 0..deg {
            r[j] = gf_mul(r[j], root);
            if j + 1 < deg {
                r[j] ^= r[j + 1];
            }
        }
        root = gf_mul(root, 2);
    }
    r
}

fn rs_remainder(data: &[u8], div: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; div.len()];
    for &b in data {
        let f = b ^ r.remove(0);
        r.push(0);
        for i in 0..r.len() {
            r[i] ^= gf_mul(div[i], f);
        }
    }
    r
}

struct Qr {
    size: usize,
    m: Vec<Vec<bool>>,
    f: Vec<Vec<bool>>,
}

impl Qr {
    fn set(&mut self, x: usize, y: usize, dark: bool) {
        self.m[y][x] = dark;
        self.f[y][x] = true;
    }

    fn align_positions(&self, ver: usize) -> Vec<usize> {
        if ver == 1 {
            return vec![];
        }
        let na = ver / 7 + 2;
        let step = (ver * 4 + 4).div_ceil(na * 2 - 2) * 2;
        let mut res = vec![6];
        let mut pos = self.size - 7;
        let mut rest = vec![];
        while rest.len() + 1 < na {
            rest.push(pos);
            pos -= step;
        }
        rest.reverse();
        res.extend(rest);
        res
    }

    fn function_patterns(&mut self, ver: usize) {
        let s = self.size;
        for i in 0..s {
            self.set(6, i, i % 2 == 0);
            self.set(i, 6, i % 2 == 0);
        }
        for (cx, cy) in [(3, 3), (s - 4, 3), (3, s - 4)] {
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                    if x >= 0 && y >= 0 && (x as usize) < s && (y as usize) < s {
                        let d = dx.abs().max(dy.abs());
                        self.set(x as usize, y as usize, d != 2 && d != 4);
                    }
                }
            }
        }
        let pos = self.align_positions(ver);
        let n = pos.len();
        for i in 0..n {
            for j in 0..n {
                // the three corners hold finder patterns, not alignment patterns
                if (i == 0 && (j == 0 || j == n - 1)) || (i == n - 1 && j == 0) {
                    continue;
                }
                for dy in -2i32..=2 {
                    for dx in -2i32..=2 {
                        self.set((pos[i] as i32 + dx) as usize, (pos[j] as i32 + dy) as usize, dx.abs().max(dy.abs()) != 1);
                    }
                }
            }
        }
        self.format_bits(0);
        if ver >= 7 {
            let mut rem = ver as u32;
            for _ in 0..12 {
                rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
            }
            let bits = (ver as u32) << 12 | rem;
            for i in 0..18 {
                let bit = (bits >> i) & 1 != 0;
                let a = s - 11 + i % 3;
                let b = i / 3;
                self.set(a, b, bit);
                self.set(b, a, bit);
            }
        }
    }

    fn format_bits(&mut self, mask: u32) {
        let data = mask; // error-correction level M is 0b00, so only the mask bits are set
        let mut rem = data;
        for _ in 0..10 {
            rem = (rem << 1) ^ ((rem >> 9) * 0x537);
        }
        let bits = (data << 10 | rem) ^ 0x5412;
        let bit = |i: u32| (bits >> i) & 1 != 0;
        let s = self.size;
        for i in 0..=5 {
            self.set(8, i, bit(i as u32));
        }
        self.set(8, 7, bit(6));
        self.set(8, 8, bit(7));
        self.set(7, 8, bit(8));
        for i in 9..15 {
            self.set(14 - i, 8, bit(i as u32));
        }
        for i in 0..8 {
            self.set(s - 1 - i, 8, bit(i as u32));
        }
        for i in 8..15 {
            self.set(8, s - 15 + i, bit(i as u32));
        }
        self.set(8, s - 8, true);
    }

    fn codewords(&mut self, data: &[u8]) {
        let s = self.size;
        let mut i = 0usize;
        let mut right = s as i32 - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vert in 0..s {
                for j in 0..2 {
                    let x = (right - j) as usize;
                    let upward = ((right + 1) & 2) == 0;
                    let y = if upward { s - 1 - vert } else { vert };
                    if !self.f[y][x] && i < data.len() * 8 {
                        self.m[y][x] = (data[i >> 3] >> (7 - (i & 7))) & 1 != 0;
                        i += 1;
                    }
                }
            }
            right -= 2;
        }
    }

    fn apply_mask(&mut self, mask: u32) {
        for y in 0..self.size {
            for x in 0..self.size {
                let inv = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => x * y % 2 + x * y % 3 == 0,
                    6 => (x * y % 2 + x * y % 3) % 2 == 0,
                    _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                };
                if inv && !self.f[y][x] {
                    self.m[y][x] = !self.m[y][x];
                }
            }
        }
    }

    fn penalty(&self) -> i32 {
        let s = self.size as i32;
        let mut result = 0;
        let add_hist = |len: i32, h: &mut [i32; 7]| {
            let mut l = len;
            if h[0] == 0 {
                l += s;
            }
            h.rotate_right(1);
            h[0] = l;
        };
        let count = |h: &[i32; 7]| -> i32 {
            let n = h[1];
            let core = n > 0 && h[2] == n && h[3] == n * 3 && h[4] == n && h[5] == n;
            (core && h[0] >= n * 4 && h[6] >= n) as i32 + (core && h[6] >= n * 4 && h[0] >= n) as i32
        };
        for pass in 0..2 {
            for a in 0..self.size {
                let mut color = false;
                let mut run = 0;
                let mut hist = [0i32; 7];
                for b in 0..self.size {
                    let v = if pass == 0 { self.m[a][b] } else { self.m[b][a] };
                    if v == color {
                        run += 1;
                        if run == 5 {
                            result += 3;
                        } else if run > 5 {
                            result += 1;
                        }
                    } else {
                        add_hist(run, &mut hist);
                        if !color {
                            result += count(&hist) * 40;
                        }
                        color = v;
                        run = 1;
                    }
                }
                if color {
                    add_hist(run, &mut hist);
                    run = 0;
                }
                run += s;
                add_hist(run, &mut hist);
                result += count(&hist) * 40;
            }
        }
        for y in 0..self.size - 1 {
            for x in 0..self.size - 1 {
                let c = self.m[y][x];
                if c == self.m[y][x + 1] && c == self.m[y + 1][x] && c == self.m[y + 1][x + 1] {
                    result += 3;
                }
            }
        }
        let dark: i32 = self.m.iter().map(|r| r.iter().filter(|&&d| d).count() as i32).sum();
        let total = s * s;
        let k = ((dark * 20 - total * 10).abs() + total - 1) / total - 1;
        result + k * 10
    }
}

pub fn encode(text: &str) -> Option<Vec<Vec<bool>>> {
    let bytes = text.as_bytes();
    let ver = (1..=10).find(|&v| {
        let cc_bits = if v <= 9 { 8 } else { 16 };
        4 + cc_bits + bytes.len() * 8 <= data_capacity(v) * 8
    })?;
    let cap = data_capacity(ver);
    let mut bits: Vec<bool> = vec![];
    let push = |v: u32, n: usize, bits: &mut Vec<bool>| {
        for i in (0..n).rev() {
            bits.push((v >> i) & 1 != 0);
        }
    };
    push(4, 4, &mut bits);
    push(bytes.len() as u32, if ver <= 9 { 8 } else { 16 }, &mut bits);
    for &b in bytes {
        push(b as u32, 8, &mut bits);
    }
    let term = (cap * 8 - bits.len()).min(4);
    push(0, term, &mut bits);
    while !bits.len().is_multiple_of(8) {
        bits.push(false);
    }
    let mut data: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b as u8)).collect();
    let mut pad = 0xECu8;
    while data.len() < cap {
        data.push(pad);
        pad ^= 0xEC ^ 0x11;
    }
    // error correction + interleave
    let nb = BLOCKS_M[ver];
    let ecl = ECC_PER_BLOCK_M[ver];
    let raw = raw_modules(ver) / 8;
    let n_short = nb - raw % nb;
    let short_len = raw / nb;
    let div = rs_divisor(ecl);
    let mut blocks: Vec<Vec<u8>> = vec![];
    let mut k = 0;
    for i in 0..nb {
        let len = short_len - ecl + if i < n_short { 0 } else { 1 };
        let mut dat = data[k..k + len].to_vec();
        k += len;
        let ecc = rs_remainder(&dat, &div);
        if i < n_short {
            dat.push(0);
        }
        dat.extend(ecc);
        blocks.push(dat);
    }
    let mut all = vec![];
    for i in 0..blocks[0].len() {
        for (j, b) in blocks.iter().enumerate() {
            if i != short_len - ecl || j >= n_short {
                all.push(b[i]);
            }
        }
    }
    let size = ver * 4 + 17;
    let mut q = Qr { size, m: vec![vec![false; size]; size], f: vec![vec![false; size]; size] };
    q.function_patterns(ver);
    q.codewords(&all);
    let mut best = (i32::MAX, 0);
    for mask in 0..8 {
        q.apply_mask(mask);
        q.format_bits(mask);
        let p = q.penalty();
        if p < best.0 {
            best = (p, mask);
        }
        q.apply_mask(mask);
    }
    q.apply_mask(best.1);
    q.format_bits(best.1);
    Some(q.m)
}

/// Dark-on-light SVG (always light background so phones can scan it in dark mode).
pub fn svg(text: &str) -> Option<String> {
    let m = encode(text)?;
    let n = m.len() + 8;
    let mut path = String::new();
    for (y, row) in m.iter().enumerate() {
        for (x, &d) in row.iter().enumerate() {
            if d {
                path.push_str(&format!("M{},{}h1v1h-1z", x + 4, y + 4));
            }
        }
    }
    Some(format!(
        "<svg class=\"qr\" viewBox=\"0 0 {n} {n}\" width=\"148\" height=\"148\" role=\"img\" aria-label=\"QR code for the donation address\" shape-rendering=\"crispEdges\"><rect width=\"{n}\" height=\"{n}\" fill=\"#fff\"/><path d=\"{}\" fill=\"#000\"/></svg>",
        path,
        n = n
    ))
}
