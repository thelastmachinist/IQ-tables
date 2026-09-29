//! The IQ Labs on-chain program (`9KLL…iQLabs`): PDAs, instruction encoders and
//! account decoders. Everything here mirrors `@iqlabs-official/solana-sdk`
//! 0.2.0 and is checked byte-for-byte against it in the test suite.

use crate::crypto::{keccak::keccak256, unhex};
use crate::json;
use crate::solana::{find_program_address, pk, AccountMeta, Instruction, Pubkey, SYSTEM_PROGRAM};

pub const PROGRAM_ID_STR: &str = "9KLLchQVJpGkw4jPuUmnvqESdR7mtNCYr3qS4iQLabs";
pub const FEE_RECEIVER_STR: &str = "EWNSTD8tikwqHMcRNuuNbZrnYJUiJdKq9UXLXSEU4wZ1";
pub const IQ_MINT_STR: &str = "3uXACfojUrya7VH51jVC1DCHq3uzK4A7g469Q954LABS";
pub const TOKEN_PROGRAM_STR: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_STR: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const ATA_PROGRAM_STR: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

pub fn program_id() -> Pubkey {
    pk(PROGRAM_ID_STR)
}
pub fn fee_receiver() -> Pubkey {
    pk(FEE_RECEIVER_STR)
}

const SEED_DB_ROOT: &[u8] = b"rootmY}AGBJiqLabs";
const SEED_TABLE: &[u8] = b"tablemY}AGBJiqLabs";
const SEED_INSTRUCTION: &[u8] = b"instructionmY}AGBJiqLabs";
const SEED_USER: &[u8] = b"usermY}AGBJiqLabs";
const SEED_CODE_ACCOUNT: &[u8] = b"codemY}AGBJiqLabs";
const SEED_USER_INVENTORY: &[u8] = b"inventorymY}AGBJiqLabs";

// Anchor discriminators from idl/code_in.json.
const IX_INITIALIZE_DB_ROOT: [u8; 8] = [189, 253, 79, 50, 58, 187, 182, 196];
const IX_MANAGE_TABLE_CREATORS: [u8; 8] = [211, 53, 161, 236, 144, 91, 109, 110];
const IX_CREATE_TABLE: [u8; 8] = [214, 142, 131, 250, 242, 83, 135, 185];
const IX_USER_INITIALIZE: [u8; 8] = [223, 157, 253, 44, 62, 158, 83, 137];
const IX_DB_CODE_IN: [u8; 8] = [38, 100, 165, 242, 99, 137, 206, 108];
const IX_REALLOC_ACCOUNT: [u8; 8] = [51, 237, 126, 233, 52, 244, 186, 244];
const IX_USER_INVENTORY_CODE_IN: [u8; 8] = [81, 177, 5, 122, 213, 125, 21, 238];
const ACC_DB_ROOT: [u8; 8] = [245, 92, 214, 180, 144, 59, 3, 240];
const ACC_TABLE: [u8; 8] = [34, 100, 138, 97, 236, 129, 230, 112];
const ACC_USER_STATE: [u8; 8] = [72, 177, 85, 249, 76, 167, 186, 126];

/// Max length of a DbRoot id (program error 6021 `InvalidDbRootId`).
pub const MAX_DB_ID_BYTES: usize = 32;
/// Inline payload budget: the whole metadata JSON must fit this many bytes
/// for a write to go out as a single "direct" transaction.
pub const INLINE_CAP_V1: usize = 3400;
pub const INLINE_CAP_LEGACY: usize = 700;

/// `toSeedBytes`: 64-hex strings are taken literally, anything else is keccak'd.
pub fn seed_bytes(v: &str) -> Vec<u8> {
    if v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit()) {
        return unhex(v).unwrap();
    }
    keccak256(v.as_bytes()).to_vec()
}

pub fn db_root_pda(db_id: &[u8]) -> Pubkey {
    let p = program_id();
    find_program_address(&[SEED_DB_ROOT, &p, db_id], &p).0
}
pub fn table_pda(db_root: &Pubkey, table_seed: &[u8]) -> Pubkey {
    let p = program_id();
    find_program_address(&[SEED_TABLE, &p, db_root, table_seed], &p).0
}
pub fn instruction_table_pda(db_root: &Pubkey, table_seed: &[u8]) -> Pubkey {
    let p = program_id();
    find_program_address(&[SEED_TABLE, &p, db_root, table_seed, SEED_INSTRUCTION], &p).0
}
pub fn user_state_pda(user: &Pubkey) -> Pubkey {
    let p = program_id();
    find_program_address(&[SEED_USER, &p, user], &p).0
}
pub fn code_account_pda(user: &Pubkey) -> Pubkey {
    find_program_address(&[SEED_CODE_ACCOUNT, user], &program_id()).0
}
pub fn user_inventory_pda(user: &Pubkey) -> Pubkey {
    find_program_address(&[SEED_USER_INVENTORY, user], &program_id()).0
}
pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    find_program_address(&[owner, token_program, mint], &pk(ATA_PROGRAM_STR)).0
}

/// Borsh writer.
pub struct Borsh(pub Vec<u8>);
impl Borsh {
    pub fn new(disc: &[u8; 8]) -> Self {
        Borsh(disc.to_vec())
    }
    pub fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bytes(self, v: &[u8]) -> Self {
        let mut s = self.u32(v.len() as u32);
        s.0.extend_from_slice(v);
        s
    }
    pub fn string(self, v: &str) -> Self {
        self.bytes(v.as_bytes())
    }
    pub fn vec_bytes(self, v: &[Vec<u8>]) -> Self {
        let mut s = self.u32(v.len() as u32);
        for x in v {
            s = s.bytes(x);
        }
        s
    }
    pub fn vec_pk(self, v: &[Pubkey]) -> Self {
        let mut s = self.u32(v.len() as u32);
        for x in v {
            s.0.extend_from_slice(x);
        }
        s
    }
}

fn ix(accounts: Vec<AccountMeta>, data: Vec<u8>) -> Instruction {
    Instruction { program_id: program_id(), accounts, data }
}

pub fn initialize_db_root(signer: &Pubkey, db_id: &[u8]) -> Instruction {
    ix(
        vec![AccountMeta::w(db_root_pda(db_id)), AccountMeta::ws(*signer), AccountMeta::r(SYSTEM_PROGRAM)],
        Borsh::new(&IX_INITIALIZE_DB_ROOT).bytes(db_id).0,
    )
}

pub fn manage_table_creators(signer: &Pubkey, db_id: &[u8], creators: &[Pubkey], ext: &[Pubkey]) -> Instruction {
    ix(
        vec![AccountMeta::ws(*signer), AccountMeta::w(db_root_pda(db_id)), AccountMeta::r(SYSTEM_PROGRAM)],
        Borsh::new(&IX_MANAGE_TABLE_CREATORS).bytes(db_id).vec_pk(creators).vec_pk(ext).0,
    )
}

pub struct TableSpec<'a> {
    pub db_id: &'a [u8],
    pub table_seed: &'a [u8],
    pub hint: &'a str,
    pub name: &'a str,
    pub columns: &'a [String],
    pub id_col: &'a str,
    pub ext_keys: &'a [String],
    pub writers: Option<&'a [Pubkey]>,
}

pub fn create_table(signer: &Pubkey, db_root_creator: &Pubkey, t: &TableSpec) -> Instruction {
    let root = db_root_pda(t.db_id);
    let cols: Vec<Vec<u8>> = t.columns.iter().map(|c| c.as_bytes().to_vec()).collect();
    let ext: Vec<Vec<u8>> = t.ext_keys.iter().map(|c| c.as_bytes().to_vec()).collect();
    let mut b = Borsh::new(&IX_CREATE_TABLE)
        .bytes(t.db_id)
        .bytes(t.table_seed)
        .bytes(t.hint.as_bytes())
        .bytes(t.name.as_bytes())
        .vec_bytes(&cols)
        .bytes(t.id_col.as_bytes())
        .vec_bytes(&ext)
        .u8(0); // gate_opt: None
    b = match t.writers {
        Some(w) => b.u8(1).vec_pk(w),
        None => b.u8(0),
    };
    ix(
        vec![
            AccountMeta::w(root),
            AccountMeta::w(fee_receiver()),
            AccountMeta::w(*db_root_creator),
            AccountMeta::ws(*signer),
            AccountMeta::w(table_pda(&root, t.table_seed)),
            AccountMeta::w(instruction_table_pda(&root, t.table_seed)),
            AccountMeta::r(SYSTEM_PROGRAM),
        ],
        b.0,
    )
}

pub fn user_initialize(user: &Pubkey) -> Instruction {
    ix(
        vec![
            AccountMeta::ws(*user),
            AccountMeta::w(code_account_pda(user)),
            AccountMeta::w(user_state_pda(user)),
            AccountMeta::w(user_inventory_pda(user)),
            AccountMeta::r(SYSTEM_PROGRAM),
        ],
        Borsh::new(&IX_USER_INITIALIZE).0,
    )
}

pub fn realloc_account(payer: &Pubkey, target: &Pubkey, new_size: u64) -> Instruction {
    ix(
        vec![AccountMeta::ws(*payer), AccountMeta::w(*target), AccountMeta::r(SYSTEM_PROGRAM)],
        Borsh::new(&IX_REALLOC_ACCOUNT).u64(new_size).0,
    )
}

/// `db_code_in` on the direct (inline) path: the whole row travels inside the
/// metadata string, no session or linked-list chunks. Optional accounts that
/// are absent are passed as the program id, like the SDK does.
pub fn db_code_in_inline(user: &Pubkey, db_id: &[u8], table_seed: &[u8], metadata: &str, iq_ata: Option<Pubkey>) -> Instruction {
    let p = program_id();
    let root = db_root_pda(db_id);
    ix(
        vec![
            AccountMeta::ws(*user),
            AccountMeta::w(user_inventory_pda(user)),
            AccountMeta::r(SYSTEM_PROGRAM),
            AccountMeta::w(fee_receiver()),
            AccountMeta::r(p),                        // session: None
            AccountMeta::r(iq_ata.unwrap_or(p)),      // iq_ata
            AccountMeta::r(root),
            AccountMeta::w(table_pda(&root, table_seed)),
            AccountMeta::r(p),                        // signer_ata: None (ungated table)
            AccountMeta::r(p),                        // metadata_account: None
        ],
        Borsh::new(&IX_DB_CODE_IN)
            .bytes(db_id)
            .bytes(table_seed)
            .string("")
            .string(metadata)
            .u8(0)
            .0,
    )
}

/// `user_inventory_code_in` on the direct path: a small file stored inline in
/// the metadata (what the SDK's `codeIn` does for data under the inline cap).
pub fn user_inventory_code_in_inline(user: &Pubkey, metadata: &str, iq_ata: Option<Pubkey>) -> Instruction {
    let p = program_id();
    ix(
        vec![
            AccountMeta::ws(*user),
            AccountMeta::w(user_inventory_pda(user)),
            AccountMeta::r(SYSTEM_PROGRAM),
            AccountMeta::w(fee_receiver()),
            AccountMeta::r(p),                   // session: None
            AccountMeta::r(iq_ata.unwrap_or(p)), // iq_ata
        ],
        Borsh::new(&IX_USER_INVENTORY_CODE_IN).string("").string(metadata).u8(0).0,
    )
}

/// Metadata for an inline file inscription, as the SDK's `codeIn` builds it.
pub fn file_metadata(filetype: &str, filename: &str, data: &str) -> String {
    json::obj(vec![
        ("filetype", json::s(filetype)),
        ("method", json::n(0)),
        ("filename", json::s(filename)),
        ("total_chunks", json::n(1)),
        ("data", json::s(data)),
    ])
    .to_string()
}

/// Decoded `db_code_in` arguments (from a transaction read back from chain).
pub struct DbCodeIn {
    pub db_id: Vec<u8>,
    pub table_seed: Vec<u8>,
    pub on_chain_path: String,
    pub metadata: String,
}

pub fn decode_db_code_in(data: &[u8]) -> Option<DbCodeIn> {
    if data.len() < 8 || data[..8] != IX_DB_CODE_IN {
        return None;
    }
    let mut r = Rd { b: data, i: 8 };
    let db_id = r.bytes()?;
    let table_seed = r.bytes()?;
    let on_chain_path = String::from_utf8(r.bytes()?).ok()?;
    let metadata = String::from_utf8(r.bytes()?).ok()?;
    Some(DbCodeIn { db_id, table_seed, on_chain_path, metadata })
}

/// (on_chain_path, metadata) of a `user_inventory_code_in` (a file inscription).
pub fn decode_inventory_code_in(data: &[u8]) -> Option<(String, String)> {
    if data.len() < 8 || data[..8] != IX_USER_INVENTORY_CODE_IN {
        return None;
    }
    let mut r = Rd { b: data, i: 8 };
    let path = String::from_utf8(r.bytes()?).ok()?;
    let metadata = String::from_utf8(r.bytes()?).ok()?;
    Some((path, metadata))
}

/// Row object from an inline `db_code_in` (None for chunked uploads).
pub fn row_from_metadata(metadata: &str) -> Option<json::Json> {
    let md = json::parse(metadata).ok()?;
    let data = md.get("data").str()?;
    let row = json::parse(data).ok()?;
    matches!(row, json::Json::Obj(_)).then_some(row)
}

/// Gateway-shaped table metadata from a decoded Table account.
pub fn meta_json(t: &TableMeta) -> json::Json {
    json::obj(vec![
        ("name", json::s(&t.name)),
        ("columns", json::Json::Arr(t.columns.iter().map(|c| json::s(c)).collect())),
        ("idCol", json::s(&t.id_col)),
        ("lastTimestamp", json::n(t.last_timestamp)),
        ("gate", json::Json::Null),
        ("writers", json::Json::Arr(t.writers.iter().map(|w| json::s(&crate::solana::b58(w))).collect())),
    ])
}

pub const ACC_DB_ROOT_DISC: [u8; 8] = ACC_DB_ROOT;

/// The metadata string the SDK writes for an inline row: exactly
/// `JSON.stringify({filetype, method, filename, total_chunks, data})`.
pub fn inline_metadata(seq: u64, row_json: &str) -> String {
    json::obj(vec![
        ("filetype", json::s("application/octet-stream")),
        ("method", json::n(0)),
        ("filename", json::s(&format!("{}.bin", seq))),
        ("total_chunks", json::n(1)),
        ("data", json::s(row_json)),
    ])
    .to_string()
}

// ---------------------------------------------------------------- decoding

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.i + n > self.b.len() {
            return None;
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.take(4)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Option<u64> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Some(u64::from_le_bytes(a))
    }
    fn pk(&mut self) -> Option<Pubkey> {
        let mut a = [0u8; 32];
        a.copy_from_slice(self.take(32)?);
        Some(a)
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        Some(self.take(n)?.to_vec())
    }
    fn vec_bytes(&mut self) -> Option<Vec<Vec<u8>>> {
        let n = self.u32()? as usize;
        (0..n).map(|_| self.bytes()).collect()
    }
    fn vec_pk(&mut self) -> Option<Vec<Pubkey>> {
        let n = self.u32()? as usize;
        (0..n).map(|_| self.pk()).collect()
    }
}

#[derive(Clone, Debug)]
pub struct DbRoot {
    pub creator: Pubkey,
    pub table_seeds: Vec<Vec<u8>>,
    pub global_table_seeds: Vec<Vec<u8>>,
    pub id: Vec<u8>,
    pub table_creators: Vec<Pubkey>,
    pub ext_creators: Vec<Pubkey>,
    /// Bytes the serialized struct occupies (for realloc decisions).
    pub used: usize,
    pub account_len: usize,
}

pub fn decode_db_root(data: &[u8]) -> Option<DbRoot> {
    if data.len() < 8 || data[..8] != ACC_DB_ROOT {
        return None;
    }
    let mut r = Rd { b: data, i: 8 };
    let creator = r.pk()?;
    let table_seeds = r.vec_bytes()?;
    let global_table_seeds = r.vec_bytes()?;
    let id = r.bytes()?;
    let table_creators = r.vec_pk().unwrap_or_default();
    let ext_creators = r.vec_pk().unwrap_or_default();
    let _ = r.u64();
    let _ = r.u8();
    Some(DbRoot {
        creator,
        table_seeds,
        global_table_seeds,
        id,
        table_creators,
        ext_creators,
        used: r.i,
        account_len: data.len(),
    })
}

/// If a new table hint of `hint_len` bytes won't fit in the DbRoot account
/// (it is stored twice), return the size to grow it to. Mirrors the SDK's
/// `buildReallocIxIfNeeded`, but counts every field.
pub fn db_root_realloc_size(root: &DbRoot, hint_len: usize) -> Option<u64> {
    let need = ((4 + hint_len) * 2).max(128);
    let free = root.account_len.saturating_sub(root.used);
    if free >= need {
        return None;
    }
    let grow = 2048usize.max(need - free + 2048);
    Some((root.account_len + grow) as u64)
}

#[derive(Clone, Debug)]
pub struct TableMeta {
    pub columns: Vec<String>,
    pub id_col: String,
    pub ext_keys: Vec<String>,
    pub name: String,
    pub last_timestamp: i64,
    pub gate_mint: Pubkey,
    pub writers: Vec<Pubkey>,
}

pub fn decode_table(data: &[u8]) -> Option<TableMeta> {
    if data.len() < 8 || data[..8] != ACC_TABLE {
        return None;
    }
    let mut r = Rd { b: data, i: 8 };
    let st = |v: Vec<u8>| String::from_utf8_lossy(&v).into_owned();
    let columns = r.vec_bytes()?.into_iter().map(st).collect();
    let id_col = st(r.bytes()?);
    let ext_keys = r.vec_bytes()?.into_iter().map(st).collect();
    let name = st(r.bytes()?);
    let last_timestamp = r.u64()? as i64;
    let gate_mint = r.pk()?;
    let _amount = r.u64()?;
    let _gt = r.u8()?;
    let writers = r.vec_pk().unwrap_or_default();
    Some(TableMeta { columns, id_col, ext_keys, name, last_timestamp, gate_mint, writers })
}

/// `total_session_files` from a UserState account (used for the SDK's
/// default filename `<seq>.bin`).
pub fn decode_user_state_seq(data: &[u8]) -> Option<u64> {
    if data.len() < 8 || data[..8] != ACC_USER_STATE {
        return None;
    }
    let mut r = Rd { b: data, i: 8 };
    r.pk()?;
    r.bytes()?;
    r.bytes()?;
    r.u64()
}

// ------------------------------------------------------------------- costs
// Program write fees per upload method, from IQ Labs' own iq6900 cost model
// (verified there against mainnet transfers to the fee receiver).
pub const FEE_DIRECT_WRITE: u64 = 1_000_000; // 0.001 SOL
pub const TX_FEE: u64 = 5_000; // per signature
/// One-time rent for a wallet's first IQ write (user_inventory + code_account
/// + user_state). Measured on devnet: ~0.051 SOL; kept a little above.
pub const USER_INIT_RENT_ESTIMATE: u64 = 55_000_000;
/// Measured against the deployed program (devnet dry-runs): a new DbRoot is
/// 2,133 bytes (~0.0115 SOL rent); a table is ~0.015 SOL rent for its two
/// accounts plus IQ's 0.00093 SOL table-creation fee.
pub const DB_ROOT_COST_ESTIMATE: u64 = 12_000_000;
pub const TABLE_COST_ESTIMATE: u64 = 17_000_000;
/// Rent-exempt minimum for an empty system account; the payer must keep this.
pub const RENT_FLOOR: u64 = 890_880;

/// Post-upgrade sizes of the per-wallet accounts; smaller ones were made by
/// the pre-upgrade program and must be grown before a v1-sized write.
pub const CODE_ACCOUNT_SPACE: u64 = 4215;
pub const USER_INVENTORY_SPACE: u64 = 4213;
/// Feature gate for v1 transactions (the SDK checks the same account).
pub const TX_V1_FEATURE_GATE_STR: &str = "txv1aq4pp281K9um3tnPgkfX8UqtFT6wcVW3hNezGLL";
pub const FEATURE_PROGRAM_STR: &str = "Feature111111111111111111111111111111111111";

/// Whether a `getMultipleAccounts` entry for the feature gate says v1 is live
/// (owned by the Feature program, `Option<u64>` activation slot is `Some`).
pub fn v1_active(acc: &json::Json) -> bool {
    acc.get("owner").str() == Some(FEATURE_PROGRAM_STR)
        && crate::net::account_data(acc).map(|d| d.first() == Some(&1)).unwrap_or(false)
}

pub fn rent_exempt(bytes: usize) -> u64 {
    ((bytes as u64) + 128) * 6960
}
