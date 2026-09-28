//! Base58 (Bitcoin alphabet), used for Solana addresses and signatures.

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

pub fn encode(input: &[u8]) -> String {
    let zeros = input.iter().take_while(|&&b| b == 0).count();
    // big-endian base conversion 256 -> 58
    let mut digits: Vec<u8> = Vec::with_capacity(input.len() * 138 / 100 + 1);
    for &byte in &input[zeros..] {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = String::with_capacity(zeros + digits.len());
    for _ in 0..zeros {
        out.push('1');
    }
    for &d in digits.iter().rev() {
        out.push(ALPHABET[d as usize] as char);
    }
    out
}

pub fn decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let zeros = bytes.iter().take_while(|&&b| b == b'1').count();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    for &c in &bytes[zeros..] {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        let mut carry = v;
        for o in out.iter_mut() {
            carry += (*o as u32) * 58;
            *o = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            out.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let mut res = vec![0u8; zeros];
    res.extend(out.iter().rev());
    Some(res)
}

pub fn decode32(input: &str) -> Option<[u8; 32]> {
    let v = decode(input)?;
    if v.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Some(out)
}
