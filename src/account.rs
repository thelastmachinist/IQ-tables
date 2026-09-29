//! Accounts: the wallets a person signed in with, held in memory only.
//!
//! * Signing in is dropping in a wallet's key (a Solana CLI keypair file, or
//!   base58 secret keys). The first key is the main wallet. Nothing is saved:
//!   closing the tab signs out.
//! * Extra wallets are derived from a master secret that comes from the main
//!   key: seed(i) = SHA-256("iq-tables/account/v1/wallet" ‖ master ‖ u32le(i)),
//!   master = SHA-256("iq-tables/key-master/v1" ‖ main key's seed). Dropping
//!   the same key in again brings them back (signing in rescans for any that
//!   were used on chain).
//! * Optionally the keys can be downloaded as a passphrase-protected file,
//!   encrypted with the IQ SDK's `passwordEncrypt` scheme (PBKDF2-SHA256 ×
//!   250,000 → AES-256-GCM), so `passwordDecrypt` opens it too.

use crate::crypto::{aead, base58, hex, sha2, unhex};
use crate::json::{self, Json};
use crate::solana::{b58, Keypair};

pub const FORMAT: &str = "iq-tables-account";
pub const RESCAN_WINDOW: u32 = 10;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Derived(u32),
    Imported,
}

#[derive(Clone)]
pub struct Wallet {
    pub label: String,
    pub note: String,
    pub kind: Kind,
    pub kp: Keypair,
}

impl Wallet {
    pub fn address(&self) -> String {
        b58(&self.kp.pubkey)
    }
}

/// How the person signed in.
#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
    /// Dropped-in keys.
    Keys,
    /// A passphrase-protected file made here earlier.
    File,
}

#[derive(Clone)]
pub struct Account {
    pub name: String,
    pub master: [u8; 32],
    pub next_index: u32,
    pub wallets: Vec<Wallet>,
    /// (salt, AES key) from the passphrase, kept so saving doesn't re-run PBKDF2.
    pub seal: Option<([u8; 16], [u8; 32])>,
    pub origin: Origin,
}

pub fn derive(master: &[u8; 32], index: u32) -> Keypair {
    Keypair::from_seed(sha2::sha256_parts(&[b"iq-tables/account/v1/wallet", master, &index.to_le_bytes()]))
}

impl Account {
    pub fn new(name: &str, master: [u8; 32]) -> Self {
        let mut a = Account { name: name.to_string(), master, next_index: 0, wallets: vec![], seal: None, origin: Origin::File };
        a.new_wallet("Main", "");
        a
    }

    /// Signed in with keys: the first one is the main wallet, and derived
    /// wallets come from it, so they're the same every time it's used.
    pub fn from_keys(keys: Vec<(String, Keypair)>) -> Option<Self> {
        let first = keys.first()?.1.clone();
        let master = sha2::sha256_parts(&[b"iq-tables/key-master/v1", &first.seed]);
        let mut a = Account { name: crate::solana::short(&b58(&first.pubkey)), master, next_index: 0, wallets: vec![], seal: None, origin: Origin::Keys };
        for (i, (label, kp)) in keys.into_iter().enumerate() {
            let label = match (label.trim().is_empty(), i) {
                (false, _) => label,
                (true, 0) => "Main".to_string(),
                (true, i) => format!("Key {}", i + 1),
            };
            a.import(kp, &label);
        }
        Some(a)
    }

    pub fn new_wallet(&mut self, label: &str, note: &str) -> Wallet {
        let i = self.next_index;
        self.next_index += 1;
        let w = Wallet { label: label.to_string(), note: note.to_string(), kind: Kind::Derived(i), kp: derive(&self.master, i) };
        self.wallets.push(w.clone());
        w
    }

    pub fn find(&self, address: &str) -> Option<&Wallet> {
        self.wallets.iter().find(|w| w.address() == address)
    }

    pub fn find_mut(&mut self, address: &str) -> Option<&mut Wallet> {
        self.wallets.iter_mut().find(|w| w.address() == address)
    }

    pub fn addresses(&self) -> Vec<String> {
        self.wallets.iter().map(|w| w.address()).collect()
    }

    /// Returns false if the key was already present.
    pub fn import(&mut self, kp: Keypair, label: &str) -> bool {
        if self.wallets.iter().any(|w| w.kp.pubkey == kp.pubkey) {
            return false;
        }
        self.wallets.push(Wallet { label: label.to_string(), note: String::new(), kind: Kind::Imported, kp });
        true
    }

    pub fn recover(&mut self, index: u32) {
        if self.wallets.iter().any(|w| w.kind == Kind::Derived(index)) {
            return;
        }
        let kp = derive(&self.master, index);
        self.wallets.push(Wallet { label: format!("Recovered #{}", index), note: String::new(), kind: Kind::Derived(index), kp });
        self.next_index = self.next_index.max(index + 1);
    }

    /// The main wallet: the key signed in with (or, in a file made by an
    /// older version, its first wallet).
    pub fn main(&self) -> Option<&Wallet> {
        self.wallets.first()
    }

    /// Candidate derived wallets from `start` (gap-limit rescans).
    pub fn scan_from(&self, start: u32) -> Vec<(u32, Keypair)> {
        (start..start + RESCAN_WINDOW).map(|i| (i, derive(&self.master, i))).collect()
    }

    fn payload(&self) -> Json {
        json::obj(vec![
            ("master", json::s(&hex(&self.master))),
            ("next", json::n(self.next_index)),
            (
                "wallets",
                Json::Arr(
                    self.wallets
                        .iter()
                        .map(|w| {
                            let mut o = json::obj(vec![("label", json::s(&w.label)), ("address", json::s(&w.address()))]);
                            if !w.note.is_empty() {
                                o.set("note", json::s(&w.note));
                            }
                            match w.kind {
                                Kind::Derived(i) => o.set("index", json::n(i)),
                                Kind::Imported => o.set("secret", json::s(&w.kp.export_b58())),
                            }
                            o
                        })
                        .collect(),
                ),
            ),
        ])
    }

    pub fn from_payload(name: &str, p: &Json) -> Result<Self, String> {
        let m = unhex(p.get("master").str().ok_or("missing master secret")?).ok_or("bad master secret")?;
        if m.len() != 32 {
            return Err("bad master secret".into());
        }
        let mut master = [0u8; 32];
        master.copy_from_slice(&m);
        let mut a =
            Account { name: name.to_string(), master, next_index: p.get("next").u64().unwrap_or(0) as u32, wallets: vec![], seal: None, origin: Origin::File };
        for w in p.get("wallets").arr() {
            let label = w.get("label").str_or("Wallet");
            let note = w.get("note").str_or("");
            if let Some(i) = w.get("index").u64() {
                let i = i as u32;
                a.wallets.push(Wallet { label, note, kind: Kind::Derived(i), kp: derive(&master, i) });
                a.next_index = a.next_index.max(i + 1);
            } else if let Some(kp) = w.get("secret").str().and_then(Keypair::from_secret_b58) {
                a.wallets.push(Wallet { label, note, kind: Kind::Imported, kp });
            }
        }
        Ok(a)
    }

    /// Serialize to the account file. `seal` = (salt, key); `iv` must be fresh.
    pub fn to_file(&self, iv: [u8; 12]) -> String {
        match self.seal {
            Some((salt, key)) => {
                let ct = aead::gcm_encrypt(&key, &iv, self.payload().to_string().as_bytes());
                json::obj(vec![
                    ("format", json::s(FORMAT)),
                    ("version", json::n(1)),
                    ("name", json::s(&self.name)),
                    ("encryption", json::s("iq-sdk-password-v1 (PBKDF2-SHA256 250000 + AES-256-GCM)")),
                    ("salt", json::s(&hex(&salt))),
                    ("iv", json::s(&hex(&iv))),
                    ("ciphertext", json::s(&hex(&ct))),
                ])
                .to_string()
            }
            None => json::obj(vec![
                ("format", json::s(FORMAT)),
                ("version", json::n(1)),
                ("name", json::s(&self.name)),
                ("encryption", json::s("none")),
                ("warning", json::s("UNENCRYPTED: anyone with this file controls every wallet in it.")),
                ("payload", self.payload()),
            ])
            .to_string(),
        }
    }

    pub fn set_passphrase(&mut self, passphrase: &str, salt: [u8; 16]) {
        self.seal = Some((salt, aead::password_key(passphrase, &salt)));
    }
}

pub enum Parsed {
    /// An account file that needs a passphrase: (name, file JSON).
    Locked(String, Json),
    Account(Account),
    /// Loose keys: (label, keypair).
    Keys(Vec<(String, Keypair)>),
}

/// Open an encrypted account file with its passphrase.
pub fn unlock(file: &Json, passphrase: &str) -> Result<Account, String> {
    let salt = unhex(file.get("salt").str().unwrap_or("")).ok_or("bad salt")?;
    let ivv = unhex(file.get("iv").str().unwrap_or("")).ok_or("bad iv")?;
    let ct = unhex(file.get("ciphertext").str().unwrap_or("")).ok_or("bad ciphertext")?;
    if salt.len() != 16 || ivv.len() != 12 {
        return Err("unsupported encryption parameters".into());
    }
    let mut iv = [0u8; 12];
    iv.copy_from_slice(&ivv);
    let key = aead::password_key(passphrase, &salt);
    let plain = aead::gcm_decrypt(&key, &iv, &ct).ok_or("Wrong passphrase (or the file was modified).")?;
    let payload = json::parse(&String::from_utf8(plain).map_err(|_| "corrupt payload")?)?;
    let mut a = Account::from_payload(&file.get("name").str_or("Account"), &payload)?;
    let mut s = [0u8; 16];
    s.copy_from_slice(&salt);
    a.seal = Some((s, key));
    Ok(a)
}

fn key_from_bytes(v: &[u8]) -> Option<Keypair> {
    match v.len() {
        64 => {
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&v[..32]);
            let kp = Keypair::from_seed(seed);
            (kp.pubkey[..] == v[32..]).then_some(kp)
        }
        32 => {
            let mut seed = [0u8; 32];
            seed.copy_from_slice(v);
            Some(Keypair::from_seed(seed))
        }
        _ => None,
    }
}

fn json_byte_array(v: &Json) -> Option<Vec<u8>> {
    let a = v.arr();
    if a.is_empty() {
        return None;
    }
    a.iter().map(|x| x.u64().filter(|&n| n < 256).map(|n| n as u8)).collect()
}

/// Recognise whatever was dropped: an IQ Tables account file, a Solana CLI
/// keypair (`[12,34,…]`), a list of those, or text with one base58 secret key
/// per line (optionally `label: key` or `label,key`).
pub fn parse_file(text: &str) -> Result<Parsed, String> {
    let t = text.trim().trim_start_matches('\u{feff}');
    // keys without a label of their own get one when they're added (see `label_keys`)
    if t.starts_with('{') {
        let v = json::parse(t)?;
        if v.get("format").str() != Some(FORMAT) {
            return Err("Not an IQ Tables account file".into());
        }
        let nm = v.get("name").str_or("Account");
        if v.get("encryption").str() == Some("none") {
            return Ok(Parsed::Account(Account::from_payload(&nm, v.get("payload"))?));
        }
        return Ok(Parsed::Locked(nm, v));
    }
    if t.starts_with('[') {
        let v = json::parse(t)?;
        if let Some(bytes) = json_byte_array(&v) {
            return key_from_bytes(&bytes).map(|kp| Parsed::Keys(vec![(String::new(), kp)])).ok_or_else(|| "That array isn't a valid 64-byte keypair".into());
        }
        let mut keys = vec![];
        for (i, item) in v.arr().iter().enumerate() {
            let kp = json_byte_array(item)
                .and_then(|b| key_from_bytes(&b))
                .or_else(|| item.str().and_then(Keypair::from_secret_b58))
                .or_else(|| item.get("secret").str().and_then(Keypair::from_secret_b58));
            match kp {
                Some(kp) => keys.push((item.get("label").str_or(""), kp)),
                None => return Err(format!("Entry {} isn't a key", i + 1)),
            }
        }
        return Ok(Parsed::Keys(keys));
    }
    let mut keys = vec![];
    for (n, line) in t.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (label, key) = match line.rsplit_once([':', ',', '\t', ' ']) {
            Some((l, k)) if !k.trim().is_empty() => (l.trim().trim_end_matches([':', ',']).trim().to_string(), k.trim()),
            _ => (String::new(), line),
        };
        let kp = match (Keypair::from_secret_b58(key), base58::decode(key).map(|b| b.len())) {
            (Some(kp), _) => kp,
            (None, Some(32)) => {
                return Err("That's a wallet address, not its secret key. The secret key is 64 bytes (about 88 characters) — in Phantom or Solflare it's under Export private key.".into())
            }
            _ => return Err(format!("Line {} isn't a secret key", n + 1)),
        };
        keys.push((label, kp));
    }
    if keys.is_empty() {
        return Err("No keys found in that file".into());
    }
    Ok(Parsed::Keys(keys))
}
