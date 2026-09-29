//! HMAC-SHA256, PBKDF2-HMAC-SHA256 and AES-256-GCM — the exact scheme the IQ
//! SDK's `passwordEncrypt` uses (PBKDF2-SHA256, 250,000 iterations, 16-byte
//! salt, AES-256-GCM, 12-byte IV, tag appended to the ciphertext), so account
//! files can be opened by `passwordDecrypt` too.

use super::sha2::{compress256, sha256, Sha256};

pub const PBKDF2_ITERATIONS: u32 = 250_000;

fn words_to_bytes(w: &[u32; 8]) -> [u8; 32] {
    let mut o = [0u8; 32];
    for i in 0..8 {
        o[4 * i..4 * i + 4].copy_from_slice(&w[i].to_be_bytes());
    }
    o
}

struct HmacKey {
    istate: [u32; 8],
    ostate: [u32; 8],
}

impl HmacKey {
    fn new(key: &[u8]) -> Self {
        let k: Vec<u8> = if key.len() > 64 { sha256(key).to_vec() } else { key.to_vec() };
        let mut ipad = [0x36u8; 64];
        let mut opad = [0x5cu8; 64];
        for (i, b) in k.iter().enumerate() {
            ipad[i] ^= b;
            opad[i] ^= b;
        }
        HmacKey { istate: Sha256::state_after_block(&ipad), ostate: Sha256::state_after_block(&opad) }
    }
    fn mac(&self, parts: &[&[u8]]) -> [u8; 32] {
        let mut inner = Sha256::resume(self.istate, 64);
        for p in parts {
            inner.update(p);
        }
        let ih = inner.finish();
        let mut outer = Sha256::resume(self.ostate, 64);
        outer.update(&ih);
        outer.finish()
    }
    /// HMAC of a 32-byte message: exactly one compression each side.
    fn mac32(&self, msg: &[u8; 32]) -> [u8; 32] {
        let mut blk = [0u8; 64];
        blk[..32].copy_from_slice(msg);
        blk[32] = 0x80;
        blk[56..64].copy_from_slice(&((64u64 + 32) * 8).to_be_bytes());
        let mut st = self.istate;
        compress256(&mut st, &blk);
        blk[..32].copy_from_slice(&words_to_bytes(&st));
        let mut st2 = self.ostate;
        compress256(&mut st2, &blk);
        words_to_bytes(&st2)
    }
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    HmacKey::new(key).mac(&[msg])
}

pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32, out_len: usize) -> Vec<u8> {
    let k = HmacKey::new(password);
    let mut out = Vec::with_capacity(out_len);
    let mut block: u32 = 1;
    while out.len() < out_len {
        let mut u = k.mac(&[salt, &block.to_be_bytes()]);
        let mut t = u;
        for _ in 1..iterations {
            u = k.mac32(&u);
            for i in 0..32 {
                t[i] ^= u[i];
            }
        }
        let take = (out_len - out.len()).min(32);
        out.extend_from_slice(&t[..take]);
        block += 1;
    }
    out
}

// ----------------------------------------------------------------- AES-256

const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

struct Aes256 {
    rk: [[u8; 16]; 15],
}

fn xtime(x: u8) -> u8 {
    (x << 1) ^ (((x >> 7) & 1) * 0x1b)
}

impl Aes256 {
    fn new(key: &[u8; 32]) -> Self {
        let mut w = [[0u8; 4]; 60];
        for i in 0..8 {
            w[i].copy_from_slice(&key[4 * i..4 * i + 4]);
        }
        let mut rcon: u8 = 1;
        for i in 8..60 {
            let mut t = w[i - 1];
            if i % 8 == 0 {
                t = [SBOX[t[1] as usize] ^ rcon, SBOX[t[2] as usize], SBOX[t[3] as usize], SBOX[t[0] as usize]];
                rcon = xtime(rcon);
            } else if i % 8 == 4 {
                t = [SBOX[t[0] as usize], SBOX[t[1] as usize], SBOX[t[2] as usize], SBOX[t[3] as usize]];
            }
            for j in 0..4 {
                w[i][j] = w[i - 8][j] ^ t[j];
            }
        }
        let mut rk = [[0u8; 16]; 15];
        for r in 0..15 {
            for c in 0..4 {
                rk[r][4 * c..4 * c + 4].copy_from_slice(&w[4 * r + c]);
            }
        }
        Aes256 { rk }
    }

    fn encrypt(&self, input: &[u8; 16]) -> [u8; 16] {
        let mut s = *input;
        for i in 0..16 {
            s[i] ^= self.rk[0][i];
        }
        for round in 1..15 {
            for b in s.iter_mut() {
                *b = SBOX[*b as usize];
            }
            // shift rows (state is column-major: s[r + 4c])
            let t = s;
            for r in 1..4 {
                for c in 0..4 {
                    s[r + 4 * c] = t[r + 4 * ((c + r) % 4)];
                }
            }
            if round != 14 {
                for c in 0..4 {
                    let a = [s[4 * c], s[4 * c + 1], s[4 * c + 2], s[4 * c + 3]];
                    let all = a[0] ^ a[1] ^ a[2] ^ a[3];
                    for r in 0..4 {
                        s[4 * c + r] = a[r] ^ all ^ xtime(a[r] ^ a[(r + 1) % 4]);
                    }
                }
            }
            for i in 0..16 {
                s[i] ^= self.rk[round][i];
            }
        }
        s
    }
}

// --------------------------------------------------------------------- GCM

fn gmul(x: u128, y: u128) -> u128 {
    const R: u128 = 0xE1 << 120;
    let mut z: u128 = 0;
    let mut v = y;
    for i in 0..128 {
        let bit = (x >> (127 - i)) & 1;
        z ^= v & 0u128.wrapping_sub(bit);
        let lsb = v & 1;
        v = (v >> 1) ^ (R & 0u128.wrapping_sub(lsb));
    }
    z
}

fn ghash(h: u128, data: &[u8]) -> u128 {
    let mut x: u128 = 0;
    for chunk in data.chunks(16) {
        let mut b = [0u8; 16];
        b[..chunk.len()].copy_from_slice(chunk);
        x = gmul(x ^ u128::from_be_bytes(b), h);
    }
    let mut len = [0u8; 16];
    len[8..].copy_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    gmul(x ^ u128::from_be_bytes(len), h)
}

fn ctr(aes: &Aes256, j0: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut cb = *j0;
    for chunk in data.chunks(16) {
        let c = u32::from_be_bytes([cb[12], cb[13], cb[14], cb[15]]).wrapping_add(1);
        cb[12..].copy_from_slice(&c.to_be_bytes());
        let ks = aes.encrypt(&cb);
        for (i, b) in chunk.iter().enumerate() {
            out.push(b ^ ks[i]);
        }
    }
    out
}

fn j0(iv: &[u8; 12]) -> [u8; 16] {
    let mut j = [0u8; 16];
    j[..12].copy_from_slice(iv);
    j[15] = 1;
    j
}

/// AES-256-GCM encrypt (no AAD). Returns ciphertext || 16-byte tag, like WebCrypto.
pub fn gcm_encrypt(key: &[u8; 32], iv: &[u8; 12], plain: &[u8]) -> Vec<u8> {
    let aes = Aes256::new(key);
    let h = u128::from_be_bytes(aes.encrypt(&[0u8; 16]));
    let j = j0(iv);
    let mut ct = ctr(&aes, &j, plain);
    let s = ghash(h, &ct) ^ u128::from_be_bytes(aes.encrypt(&j));
    ct.extend_from_slice(&s.to_be_bytes());
    ct
}

/// AES-256-GCM decrypt; `None` if the tag doesn't verify (wrong passphrase).
pub fn gcm_decrypt(key: &[u8; 32], iv: &[u8; 12], ct_tag: &[u8]) -> Option<Vec<u8>> {
    if ct_tag.len() < 16 {
        return None;
    }
    let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
    let aes = Aes256::new(key);
    let h = u128::from_be_bytes(aes.encrypt(&[0u8; 16]));
    let j = j0(iv);
    let expect = (ghash(h, ct) ^ u128::from_be_bytes(aes.encrypt(&j))).to_be_bytes();
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= expect[i] ^ tag[i];
    }
    if diff != 0 {
        return None;
    }
    Some(ctr(&aes, &j, ct))
}

/// `passwordEncrypt` from the IQ SDK, with caller-supplied randomness.
pub struct Sealed {
    pub salt: [u8; 16],
    pub iv: [u8; 12],
    pub ciphertext: Vec<u8>,
}

pub fn password_key(password: &str, salt: &[u8]) -> [u8; 32] {
    let k = pbkdf2_sha256(password.as_bytes(), salt, PBKDF2_ITERATIONS, 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&k);
    out
}

pub fn password_encrypt(password: &str, plain: &[u8], salt: [u8; 16], iv: [u8; 12]) -> Sealed {
    let key = password_key(password, &salt);
    Sealed { salt, iv, ciphertext: gcm_encrypt(&key, &iv, plain) }
}

pub fn password_decrypt(password: &str, salt: &[u8], iv: &[u8; 12], ct: &[u8]) -> Option<Vec<u8>> {
    let key = password_key(password, salt);
    gcm_decrypt(&key, iv, ct)
}
