//! Minimal Solana primitives: addresses, program-derived addresses, instruction
//! compilation and the two wire formats we send (legacy and v1 / SIMD-0296).

use crate::crypto::{base58, ed25519, sha2};

pub type Pubkey = [u8; 32];

pub const SYSTEM_PROGRAM: Pubkey = [0u8; 32];

/// Decode a base58 address. Used for compile-time-known constants.
pub fn pk(s: &str) -> Pubkey {
    base58::decode32(s).expect("valid base58 address")
}

pub fn parse_pk(s: &str) -> Option<Pubkey> {
    base58::decode32(s.trim())
}

pub fn b58(p: &Pubkey) -> String {
    base58::encode(p)
}

pub fn short(p: &str) -> String {
    if p.len() <= 10 {
        return p.to_string();
    }
    format!("{}…{}", &p[..4], &p[p.len() - 4..])
}

/// `PublicKey.findProgramAddressSync` equivalent.
pub fn find_program_address(seeds: &[&[u8]], program: &Pubkey) -> (Pubkey, u8) {
    for bump in (0u8..=255).rev() {
        let mut h = sha2::Sha256::new();
        for s in seeds {
            h.update(s);
        }
        h.update(&[bump]);
        h.update(program);
        h.update(b"ProgramDerivedAddress");
        let out = h.finish();
        if !ed25519::is_on_curve(&out) {
            return (out, bump);
        }
    }
    panic!("no viable bump")
}

#[derive(Clone, Debug)]
pub struct AccountMeta {
    pub pubkey: Pubkey,
    pub is_signer: bool,
    pub is_writable: bool,
}

impl AccountMeta {
    pub fn w(pubkey: Pubkey) -> Self {
        AccountMeta { pubkey, is_signer: false, is_writable: true }
    }
    pub fn r(pubkey: Pubkey) -> Self {
        AccountMeta { pubkey, is_signer: false, is_writable: false }
    }
    pub fn ws(pubkey: Pubkey) -> Self {
        AccountMeta { pubkey, is_signer: true, is_writable: true }
    }
}

#[derive(Clone, Debug)]
pub struct Instruction {
    pub program_id: Pubkey,
    pub accounts: Vec<AccountMeta>,
    pub data: Vec<u8>,
}

pub fn system_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM,
        accounts: vec![AccountMeta::ws(*from), AccountMeta::w(*to)],
        data,
    }
}

pub struct Message {
    pub num_required_signatures: u8,
    pub num_readonly_signed: u8,
    pub num_readonly_unsigned: u8,
    pub keys: Vec<Pubkey>,
    pub blockhash: [u8; 32],
    /// (program index, account indices, data)
    pub ixs: Vec<(u8, Vec<u8>, Vec<u8>)>,
}

/// Compile instructions into a message with `payer` as the fee payer.
/// Ordering follows the runtime rules: writable signers, readonly signers,
/// writable non-signers, readonly non-signers (payer always first).
pub fn compile(payer: &Pubkey, ixs: &[Instruction], blockhash: [u8; 32]) -> Message {
    let mut metas: Vec<AccountMeta> = vec![AccountMeta::ws(*payer)];
    let push = |m: &AccountMeta, metas: &mut Vec<AccountMeta>| {
        if let Some(e) = metas.iter_mut().find(|e| e.pubkey == m.pubkey) {
            e.is_signer |= m.is_signer;
            e.is_writable |= m.is_writable;
        } else {
            metas.push(m.clone());
        }
    };
    for ix in ixs {
        for a in &ix.accounts {
            push(a, &mut metas);
        }
    }
    for ix in ixs {
        push(&AccountMeta::r(ix.program_id), &mut metas);
    }
    let payer_meta = metas.remove(0);
    // Same order web3.js produces (signer, then writable, then base58 string
    // under ICU "en" collation with lowercase first), so our bytes match the SDK.
    let names: Vec<String> = metas.iter().map(|m| b58(&m.pubkey)).collect();
    let mut idxs: Vec<usize> = (0..metas.len()).collect();
    idxs.sort_by(|&a, &b| {
        let (x, y) = (&metas[a], &metas[b]);
        y.is_signer
            .cmp(&x.is_signer)
            .then(y.is_writable.cmp(&x.is_writable))
            .then_with(|| collate_en(&names[a], &names[b]))
    });
    let mut ordered = vec![payer_meta];
    ordered.extend(idxs.into_iter().map(|i| metas[i].clone()));
    let num_required_signatures = ordered.iter().filter(|m| m.is_signer).count() as u8;
    let num_readonly_signed = ordered.iter().filter(|m| m.is_signer && !m.is_writable).count() as u8;
    let num_readonly_unsigned =
        ordered.iter().filter(|m| !m.is_signer && !m.is_writable).count() as u8;
    let keys: Vec<Pubkey> = ordered.iter().map(|m| m.pubkey).collect();
    let idx = |p: &Pubkey| keys.iter().position(|k| k == p).unwrap() as u8;
    let cixs = ixs
        .iter()
        .map(|ix| (idx(&ix.program_id), ix.accounts.iter().map(|a| idx(&a.pubkey)).collect(), ix.data.clone()))
        .collect();
    Message {
        num_required_signatures,
        num_readonly_signed,
        num_readonly_unsigned,
        keys,
        blockhash,
        ixs: cixs,
    }
}

/// ICU-style comparison for alphanumeric strings: case-insensitive first,
/// then lowercase before uppercase at the first case difference.
fn collate_en(a: &str, b: &str) -> std::cmp::Ordering {
    let la = a.to_ascii_lowercase();
    let lb = b.to_ascii_lowercase();
    la.cmp(&lb).then_with(|| {
        for (ca, cb) in a.bytes().zip(b.bytes()) {
            if ca != cb {
                return cb.is_ascii_lowercase().cmp(&ca.is_ascii_lowercase());
            }
        }
        std::cmp::Ordering::Equal
    })
}

fn compact_u16(out: &mut Vec<u8>, mut v: usize) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

pub fn legacy_message_bytes(m: &Message) -> Vec<u8> {
    let mut out = vec![m.num_required_signatures, m.num_readonly_signed, m.num_readonly_unsigned];
    compact_u16(&mut out, m.keys.len());
    for k in &m.keys {
        out.extend_from_slice(k);
    }
    out.extend_from_slice(&m.blockhash);
    compact_u16(&mut out, m.ixs.len());
    for (p, accs, data) in &m.ixs {
        out.push(*p);
        compact_u16(&mut out, accs.len());
        out.extend_from_slice(accs);
        compact_u16(&mut out, data.len());
        out.extend_from_slice(data);
    }
    out
}

/// Legacy transaction with empty signature slots, for a browser wallet to sign.
pub fn legacy_unsigned(m: &Message) -> Vec<u8> {
    let mut out = vec![];
    compact_u16(&mut out, m.num_required_signatures as usize);
    for _ in 0..m.num_required_signatures {
        out.extend_from_slice(&[0u8; 64]);
    }
    out.extend(legacy_message_bytes(m));
    out
}

/// Legacy transaction signed by a single keypair (the fee payer).
pub fn legacy_signed(m: &Message, seed: &[u8; 32]) -> (Vec<u8>, [u8; 64]) {
    let msg = legacy_message_bytes(m);
    let sig = ed25519::sign(seed, &msg);
    let mut out = vec![1u8];
    out.extend_from_slice(&sig);
    out.extend(msg);
    (out, sig)
}

pub const V1_MAX_TX_BYTES: usize = 4096;
pub const LEGACY_MAX_TX_BYTES: usize = 1232;

/// v1 transaction (SIMD-0296 / SIMD-0385), byte-for-byte the layout used by the
/// IQ SDK's `buildV1Transaction`: signatures at the end, compute limits in a
/// config mask, no address lookup tables. Single keypair signer only.
pub fn v1_signed(m: &Message, seed: &[u8; 32]) -> (Vec<u8>, [u8; 64]) {
    assert_eq!(m.num_required_signatures, 1, "v1 path supports one signer");
    let mut msg = vec![129u8, m.num_required_signatures, m.num_readonly_signed, m.num_readonly_unsigned];
    let config_mask: u32 = (1 << 2) | (1 << 3);
    msg.extend_from_slice(&config_mask.to_le_bytes());
    msg.extend_from_slice(&m.blockhash);
    msg.push(m.ixs.len() as u8);
    msg.push(m.keys.len() as u8);
    for k in &m.keys {
        msg.extend_from_slice(k);
    }
    msg.extend_from_slice(&200_000u32.to_le_bytes());
    msg.extend_from_slice(&(32u32 * 1024 * 1024).to_le_bytes());
    for (p, accs, data) in &m.ixs {
        msg.push(*p);
        msg.push(accs.len() as u8);
        msg.extend_from_slice(&(data.len() as u16).to_le_bytes());
    }
    for (_, accs, data) in &m.ixs {
        msg.extend_from_slice(accs);
        msg.extend_from_slice(data);
    }
    let sig = ed25519::sign(seed, &msg);
    msg.extend_from_slice(&sig);
    (msg, sig)
}

/// A keypair held only in memory (database wallets are re-derived on demand).
#[derive(Clone)]
pub struct Keypair {
    pub seed: [u8; 32],
    pub pubkey: Pubkey,
}

impl Keypair {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Keypair { pubkey: ed25519::public_key(&seed), seed }
    }
    /// 64-byte secret key (seed || pubkey), base58 — the format Phantom imports.
    pub fn export_b58(&self) -> String {
        let mut sk = self.seed.to_vec();
        sk.extend_from_slice(&self.pubkey);
        base58::encode(&sk)
    }
    pub fn from_secret_b58(s: &str) -> Option<Self> {
        let v = base58::decode(s.trim())?;
        if v.len() != 64 && v.len() != 32 {
            return None;
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&v[..32]);
        let kp = Keypair::from_seed(seed);
        if v.len() == 64 && v[32..] != kp.pubkey {
            return None;
        }
        Some(kp)
    }
}
