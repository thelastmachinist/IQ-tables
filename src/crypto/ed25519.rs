//! Ed25519 signing and the "is this a curve point" check Solana uses for
//! program-derived addresses. Ported from TweetNaCl (public domain): small and
//! constant-time for secret-dependent operations, which is what we need for
//! signing with database wallets inside the browser.

use super::sha2::sha512_parts;

type Gf = [i64; 16];

const GF0: Gf = [0; 16];
const GF1: Gf = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const D: Gf = [
    0x78a3, 0x1359, 0x4dca, 0x75eb, 0xd8ab, 0x4141, 0x0a4d, 0x0070, 0xe898, 0x7779, 0x4079, 0x8cc7,
    0xfe73, 0x2b6f, 0x6cee, 0x5203,
];
const D2: Gf = [
    0xf159, 0x26b2, 0x9b94, 0xebd6, 0xb156, 0x8283, 0x149a, 0x00e0, 0xd130, 0xeef3, 0x80f2, 0x198e,
    0xfce7, 0x56df, 0xd9dc, 0x2406,
];
const X: Gf = [
    0xd51a, 0x8f25, 0x2d60, 0xc956, 0xa7b2, 0x9525, 0xc760, 0x692c, 0xdc5c, 0xfdd6, 0xe231, 0xc0a4,
    0x53fe, 0xcd6e, 0x36d3, 0x2169,
];
const Y: Gf = [
    0x6658, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666,
    0x6666, 0x6666, 0x6666, 0x6666,
];
const I: Gf = [
    0xa0b0, 0x4a0e, 0x1b27, 0xc4ee, 0xe478, 0xad2f, 0x1806, 0x2f43, 0xd7a7, 0x3dfb, 0x0099, 0x2b4d,
    0xdf0b, 0x4fc1, 0x2480, 0x2b83,
];
const L: [i64; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];

fn car(o: &mut Gf) {
    let mut c: i64 = 1;
    for i in 0..16 {
        let v = o[i] + c + 65535;
        c = v >> 16;
        o[i] = v - (c << 16);
    }
    o[0] += c - 1 + 37 * (c - 1);
}

fn sel(p: &mut Gf, q: &mut Gf, b: i64) {
    let c = !(b - 1);
    for i in 0..16 {
        let t = c & (p[i] ^ q[i]);
        p[i] ^= t;
        q[i] ^= t;
    }
}

fn pack25519(n: &Gf) -> [u8; 32] {
    let mut t = *n;
    let mut m: Gf = GF0;
    car(&mut t);
    car(&mut t);
    car(&mut t);
    for _ in 0..2 {
        m[0] = t[0] - 0xffed;
        for i in 1..15 {
            m[i] = t[i] - 0xffff - ((m[i - 1] >> 16) & 1);
            m[i - 1] &= 0xffff;
        }
        m[15] = t[15] - 0x7fff - ((m[14] >> 16) & 1);
        let b = (m[15] >> 16) & 1;
        m[14] &= 0xffff;
        sel(&mut t, &mut m, 1 - b);
    }
    let mut o = [0u8; 32];
    for i in 0..16 {
        o[2 * i] = (t[i] & 0xff) as u8;
        o[2 * i + 1] = ((t[i] >> 8) & 0xff) as u8;
    }
    o
}

fn neq(a: &Gf, b: &Gf) -> bool {
    pack25519(a) != pack25519(b)
}

fn par(a: &Gf) -> u8 {
    pack25519(a)[0] & 1
}

fn unpack25519(n: &[u8; 32]) -> Gf {
    let mut o = GF0;
    for i in 0..16 {
        o[i] = n[2 * i] as i64 + ((n[2 * i + 1] as i64) << 8);
    }
    o[15] &= 0x7fff;
    o
}

fn add_f(a: &Gf, b: &Gf) -> Gf {
    let mut o = GF0;
    for i in 0..16 {
        o[i] = a[i] + b[i];
    }
    o
}

fn sub_f(a: &Gf, b: &Gf) -> Gf {
    let mut o = GF0;
    for i in 0..16 {
        o[i] = a[i] - b[i];
    }
    o
}

fn mul(a: &Gf, b: &Gf) -> Gf {
    let mut t = [0i64; 31];
    for i in 0..16 {
        for j in 0..16 {
            t[i + j] += a[i] * b[j];
        }
    }
    for i in 0..15 {
        t[i] += 38 * t[i + 16];
    }
    let mut o = GF0;
    o.copy_from_slice(&t[..16]);
    car(&mut o);
    car(&mut o);
    o
}

fn sq(a: &Gf) -> Gf {
    mul(a, a)
}

fn inv(i: &Gf) -> Gf {
    let mut c = *i;
    for a in (0..=253).rev() {
        c = sq(&c);
        if a != 2 && a != 4 {
            c = mul(&c, i);
        }
    }
    c
}

fn pow2523(i: &Gf) -> Gf {
    let mut c = *i;
    for a in (0..=250).rev() {
        c = sq(&c);
        if a != 1 {
            c = mul(&c, i);
        }
    }
    c
}

type Point = [Gf; 4];

fn padd(p: &mut Point, q: &Point) {
    let a = mul(&sub_f(&p[1], &p[0]), &sub_f(&q[1], &q[0]));
    let b = mul(&add_f(&p[0], &p[1]), &add_f(&q[0], &q[1]));
    let c = mul(&mul(&p[3], &q[3]), &D2);
    let d0 = mul(&p[2], &q[2]);
    let d = add_f(&d0, &d0);
    let e = sub_f(&b, &a);
    let f = sub_f(&d, &c);
    let g = add_f(&d, &c);
    let h = add_f(&b, &a);
    p[0] = mul(&e, &f);
    p[1] = mul(&h, &g);
    p[2] = mul(&g, &f);
    p[3] = mul(&e, &h);
}

fn cswap(p: &mut Point, q: &mut Point, b: i64) {
    for i in 0..4 {
        sel(&mut p[i], &mut q[i], b);
    }
}

fn pack_point(p: &Point) -> [u8; 32] {
    let zi = inv(&p[2]);
    let tx = mul(&p[0], &zi);
    let ty = mul(&p[1], &zi);
    let mut r = pack25519(&ty);
    r[31] ^= par(&tx) << 7;
    r
}

fn scalarmult(q: &mut Point, s: &[u8; 32]) -> Point {
    let mut p: Point = [GF0, GF1, GF1, GF0];
    for i in (0..256).rev() {
        let b = ((s[i / 8] >> (i & 7)) & 1) as i64;
        cswap(&mut p, q, b);
        let pc = p;
        padd(q, &pc);
        let pc2 = p;
        padd(&mut p, &pc2);
        cswap(&mut p, q, b);
    }
    p
}

fn scalarbase(s: &[u8; 32]) -> Point {
    let mut q: Point = [X, Y, GF1, mul(&X, &Y)];
    scalarmult(&mut q, s)
}

fn mod_l(x: &mut [i64; 64]) -> [u8; 32] {
    for i in (32..64).rev() {
        let mut carry: i64 = 0;
        let mut j = i - 32;
        let k = i - 12;
        while j < k {
            x[j] += carry - 16 * x[i] * L[j - (i - 32)];
            carry = (x[j] + 128) >> 8;
            x[j] -= carry << 8;
            j += 1;
        }
        x[j] += carry;
        x[i] = 0;
    }
    let mut carry: i64 = 0;
    for j in 0..32 {
        x[j] += carry - (x[31] >> 4) * L[j];
        carry = x[j] >> 8;
        x[j] &= 255;
    }
    for j in 0..32 {
        x[j] -= carry * L[j];
    }
    let mut r = [0u8; 32];
    for i in 0..32 {
        x[i + 1] += x[i] >> 8;
        r[i] = (x[i] & 255) as u8;
    }
    r
}

fn reduce(h: &[u8; 64]) -> [u8; 32] {
    let mut x = [0i64; 64];
    for i in 0..64 {
        x[i] = h[i] as i64;
    }
    mod_l(&mut x)
}

fn expand(seed: &[u8; 32]) -> [u8; 64] {
    let mut d = sha512_parts(&[seed]);
    d[0] &= 248;
    d[31] &= 127;
    d[31] |= 64;
    d
}

/// Public key for a 32-byte Ed25519 seed (the first half of a Solana secret key).
pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    let d = expand(seed);
    let mut a = [0u8; 32];
    a.copy_from_slice(&d[..32]);
    pack_point(&scalarbase(&a))
}

/// Detached Ed25519 signature (RFC 8032, deterministic).
pub fn sign(seed: &[u8; 32], msg: &[u8]) -> [u8; 64] {
    let d = expand(seed);
    let mut a = [0u8; 32];
    a.copy_from_slice(&d[..32]);
    let pk = pack_point(&scalarbase(&a));
    let r = reduce(&sha512_parts(&[&d[32..64], msg]));
    let big_r = pack_point(&scalarbase(&r));
    let h = reduce(&sha512_parts(&[&big_r, &pk, msg]));
    let mut x = [0i64; 64];
    for i in 0..32 {
        x[i] = r[i] as i64;
    }
    for i in 0..32 {
        for j in 0..32 {
            x[i + j] += (h[i] as i64) * (d[j] as i64);
        }
    }
    let s = mod_l(&mut x);
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&big_r);
    sig[32..].copy_from_slice(&s);
    sig
}

/// True when the 32 bytes decompress to a point on the Ed25519 curve
/// (same rule as curve25519-dalek's `CompressedEdwardsY::decompress`).
/// Program-derived addresses must be *off* the curve.
pub fn is_on_curve(p: &[u8; 32]) -> bool {
    let r1 = unpack25519(p);
    let r2 = GF1;
    let num0 = sq(&r1);
    let den = add_f(&r2, &mul(&num0, &D));
    let num = sub_f(&num0, &r2);
    let den2 = sq(&den);
    let den4 = sq(&den2);
    let den6 = mul(&den4, &den2);
    let mut t = mul(&mul(&den6, &num), &den);
    t = pow2523(&t);
    t = mul(&mul(&mul(&t, &num), &den), &den);
    let mut r0 = mul(&t, &den);
    let chk = mul(&sq(&r0), &den);
    if neq(&chk, &num) {
        r0 = mul(&r0, &I);
    }
    let chk = mul(&sq(&r0), &den);
    !neq(&chk, &num)
}
