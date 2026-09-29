//! Persistent state: settings and the ghost workspace (drafts that have not
//! been inscribed yet). Stored as JSON in the browser; exportable as a file.

use crate::json::{self, Json};

#[derive(Clone, Debug, PartialEq)]
pub enum TxFormat {
    Auto,
    V1,
    Legacy,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub rpc: String,
    pub gateway: String,
    pub cluster: String, // "mainnet" | "devnet"
    pub tx_format: TxFormat,
    pub simulate: bool,
    pub notify_gateway: bool,
    /// "gateway" (IQ's cached HTTP API) or "rpc" (straight from Solana).
    pub source: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            rpc: "https://api.mainnet-beta.solana.com".into(),
            gateway: "https://gateway.iqlabs.dev".into(),
            cluster: "mainnet".into(),
            tx_format: TxFormat::Auto,
            simulate: true,
            notify_gateway: true,
            source: "gateway".into(),
        }
    }
}

impl Settings {
    pub fn to_json(&self) -> Json {
        json::obj(vec![
            ("rpc", json::s(&self.rpc)),
            ("gateway", json::s(&self.gateway)),
            ("cluster", json::s(&self.cluster)),
            (
                "tx",
                json::s(match self.tx_format {
                    TxFormat::Auto => "auto",
                    TxFormat::V1 => "v1",
                    TxFormat::Legacy => "legacy",
                }),
            ),
            ("simulate", Json::Bool(self.simulate)),
            ("notify", Json::Bool(self.notify_gateway)),
            ("source", json::s(&self.source)),
        ])
    }
    pub fn from_json(v: &Json) -> Self {
        let d = Settings::default();
        Settings {
            rpc: v.get("rpc").str().filter(|s| !s.is_empty()).map(String::from).unwrap_or(d.rpc),
            gateway: v.get("gateway").str().filter(|s| !s.is_empty()).map(String::from).unwrap_or(d.gateway),
            cluster: v.get("cluster").str().map(String::from).unwrap_or(d.cluster),
            tx_format: match v.get("tx").str() {
                Some("v1") => TxFormat::V1,
                Some("legacy") => TxFormat::Legacy,
                _ => TxFormat::Auto,
            },
            simulate: v.get("simulate").bool().unwrap_or(true),
            notify_gateway: v.get("notify").bool().unwrap_or(true),
            source: if v.get("source").str() == Some("rpc") { "rpc".into() } else { "gateway".into() },
        }
    }
    pub fn chain(&self) -> &'static str {
        if self.cluster == "devnet" {
            "solana:devnet"
        } else {
            "solana:mainnet"
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GhostRow {
    pub vals: Vec<Json>,
    pub deleted: bool,
    /// Signature of the pack that inscribed this row (None = still a ghost).
    pub sig: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DraftTable {
    pub name: String,
    pub title: String,
    pub columns: Vec<String>,
    pub id_col: usize,
    pub open: bool,
    pub compress: bool,
    /// Signature of create_table, or "existing" for tables already on chain.
    pub created: Option<String>,
    pub rows: Vec<GhostRow>,
}

impl DraftTable {
    pub fn ghosts(&self) -> usize {
        self.rows.iter().filter(|r| r.sig.is_none()).count()
    }
}

#[derive(Clone, Debug)]
pub struct Draft {
    pub key: String,
    pub name: String,
    /// Main wallet the database wallet is derived from.
    pub owner: Option<String>,
    /// Database wallet address (the on-chain creator / official signer).
    pub wallet: Option<String>,
    /// "derived" (from a signature of the owner wallet) or "imported".
    pub wallet_kind: String,
    pub lock_creators: bool,
    pub root_sig: Option<String>,
    pub user_init_sig: Option<String>,
    pub tables: Vec<DraftTable>,
    pub sel: usize,
    pub page: usize,
}

impl Draft {
    pub fn new(key: String, name: String) -> Self {
        Draft {
            key,
            name,
            owner: None,
            wallet: None,
            wallet_kind: "derived".into(),
            lock_creators: true,
            root_sig: None,
            user_init_sig: None,
            tables: vec![],
            sel: 0,
            page: 0,
        }
    }
    pub fn ghosts(&self) -> usize {
        self.tables.iter().map(|t| t.ghosts()).sum()
    }
}

fn row_to_json(r: &GhostRow) -> Json {
    let mut o = json::obj(vec![("v", Json::Arr(r.vals.clone()))]);
    if r.deleted {
        o.set("d", Json::Bool(true));
    }
    if let Some(s) = &r.sig {
        o.set("s", json::s(s));
    }
    o
}

fn opt(v: &Option<String>) -> Json {
    v.as_ref().map(|s| json::s(s)).unwrap_or(Json::Null)
}

fn ostr(v: &Json) -> Option<String> {
    v.str().map(String::from)
}

pub fn drafts_to_json(ds: &[Draft]) -> Json {
    Json::Arr(
        ds.iter()
            .map(|d| {
                json::obj(vec![
                    ("key", json::s(&d.key)),
                    ("name", json::s(&d.name)),
                    ("owner", opt(&d.owner)),
                    ("wallet", opt(&d.wallet)),
                    ("walletKind", json::s(&d.wallet_kind)),
                    ("lockCreators", Json::Bool(d.lock_creators)),
                    ("rootSig", opt(&d.root_sig)),
                    ("userInitSig", opt(&d.user_init_sig)),
                    (
                        "tables",
                        Json::Arr(
                            d.tables
                                .iter()
                                .map(|t| {
                                    json::obj(vec![
                                        ("name", json::s(&t.name)),
                                        ("title", json::s(&t.title)),
                                        ("columns", Json::Arr(t.columns.iter().map(|c| json::s(c)).collect())),
                                        ("idCol", json::n(t.id_col)),
                                        ("open", Json::Bool(t.open)),
                                        ("compress", Json::Bool(t.compress)),
                                        ("created", opt(&t.created)),
                                        ("rows", Json::Arr(t.rows.iter().map(row_to_json).collect())),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect(),
    )
}

pub fn drafts_from_json(v: &Json) -> Vec<Draft> {
    v.arr()
        .iter()
        .map(|d| Draft {
            key: d.get("key").str_or("x"),
            name: d.get("name").str_or(""),
            owner: ostr(d.get("owner")),
            wallet: ostr(d.get("wallet")),
            wallet_kind: d.get("walletKind").str_or("derived"),
            lock_creators: d.get("lockCreators").bool().unwrap_or(true),
            root_sig: ostr(d.get("rootSig")),
            user_init_sig: ostr(d.get("userInitSig")),
            tables: d
                .get("tables")
                .arr()
                .iter()
                .map(|t| DraftTable {
                    name: t.get("name").str_or(""),
                    title: t.get("title").str_or(""),
                    columns: t.get("columns").arr().iter().map(|c| c.str_or("")).collect(),
                    id_col: t.get("idCol").u64().unwrap_or(0) as usize,
                    open: t.get("open").bool().unwrap_or(false),
                    compress: t.get("compress").bool().unwrap_or(true),
                    created: ostr(t.get("created")),
                    rows: t
                        .get("rows")
                        .arr()
                        .iter()
                        .map(|r| GhostRow {
                            vals: r.get("v").arr().to_vec(),
                            deleted: r.get("d").bool().unwrap_or(false),
                            sig: ostr(r.get("s")),
                        })
                        .collect(),
                })
                .collect(),
            sel: 0,
            page: 0,
        })
        .collect()
}

/// The exact text a main wallet signs to unlock a database wallet. Frozen:
/// changing a single byte would change every database wallet's address.
pub fn derivation_message(db_name: &str) -> String {
    format!(
        "IQ Tables — unlock database wallet\n\nDatabase: {}\nKey version: 1\n\nSigning this gives this page the key to the database wallet for \"{}\". Only sign it on the IQ Tables portal. It does not move any funds.",
        db_name, db_name
    )
}

/// Seed = SHA-256("iq-tables/db-wallet/v1" || signature).
pub fn derive_seed(signature: &[u8]) -> [u8; 32] {
    crate::crypto::sha2::sha256_parts(&[b"iq-tables/db-wallet/v1", signature])
}
