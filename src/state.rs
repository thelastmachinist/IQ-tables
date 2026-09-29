//! Persistent state: settings and the ghost workspace (drafts that have not
//! been inscribed yet). Stored as JSON in the browser; exportable as a file.

use crate::json::{self, Json};
use crate::schema::{self, ColMeta, Doc, TableKeys};

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
    /// IQ SDK speed profile for sending the parts of big uploads.
    pub upload_speed: String,
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
            upload_speed: crate::upload::DEFAULT_PROFILE.into(),
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
            ("upload_speed", json::s(&self.upload_speed)),
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
            upload_speed: crate::upload::profile(&v.get("upload_speed").str_or(crate::upload::DEFAULT_PROFILE)).name.into(),
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

#[derive(Clone, Debug, Default)]
pub struct DraftTable {
    /// On-chain name (the table's seed). Never changes once saved.
    pub name: String,
    /// The table's name as people and SQL see it (RENAME TABLE changes it).
    pub title: String,
    pub columns: Vec<String>,
    /// Per-column type and constraints, parallel to `columns`.
    pub meta: Vec<ColMeta>,
    pub id_col: usize,
    pub keys: TableKeys,
    pub open: bool,
    /// Extra wallets allowed to add rows (GRANT), besides the database wallet.
    pub writers: Vec<String>,
    pub compress: bool,
    /// Signature of create_table, or "existing" for tables already on chain.
    pub created: Option<String>,
    pub rows: Vec<GhostRow>,
    /// The structure record as last read from / written to the chain.
    pub chain_doc: Option<String>,
    /// TRUNCATE is waiting to be saved: rows saved before are discarded.
    pub clear: bool,
    /// DROP TABLE is waiting to be saved.
    pub dropped: bool,
    /// A checkpoint (OPTIMIZE TABLE) is waiting to be saved: the whole table
    /// is rewritten so readers can skip its history.
    pub checkpoint: bool,
    /// A crowdfunded file's manifest (crowd.rs), written with the structure.
    pub crowd: Option<Json>,
    /// Display name and writer list as last seen on chain (to spot renames
    /// and privilege changes waiting to be saved).
    pub chain_title: Option<String>,
    pub chain_writers: Option<Vec<String>>,
}

impl DraftTable {
    /// An untyped table (the pre-types layout).
    pub fn plain(name: &str, columns: Vec<String>, id_col: usize) -> DraftTable {
        let meta = columns.iter().map(|c| ColMeta::plain(c)).collect();
        DraftTable { name: name.into(), title: name.into(), columns, meta, id_col, compress: true, ..Default::default() }
    }
    pub fn typed(name: &str, cols: Vec<(String, ColMeta)>, id_col: usize) -> DraftTable {
        let (columns, meta) = cols.into_iter().unzip();
        DraftTable { name: name.into(), title: name.into(), columns, meta, id_col, compress: true, ..Default::default() }
    }
    /// A new table the way most people want one: an automatic number as
    /// the ID, then the given columns (untyped unless changed later).
    pub fn starter(name: &str, cols: &[&str]) -> DraftTable {
        use crate::schema::{IntKind, Ty};
        let has_id = cols.iter().any(|c| c.eq_ignore_ascii_case("id"));
        let mut v: Vec<(String, ColMeta)> = vec![];
        if !has_id {
            v.push(("id".into(), ColMeta { not_null: true, auto_inc: true, ..ColMeta::typed("id", Ty::Int(IntKind::Int, false)) }));
        }
        for c in cols {
            v.push((c.to_string(), ColMeta::plain(c)));
        }
        let id = v.iter().position(|(n, _)| n.eq_ignore_ascii_case("id")).unwrap_or(0);
        DraftTable::typed(name, v, id)
    }
    pub fn ghosts(&self) -> usize {
        self.rows.iter().filter(|r| r.sig.is_none()).count()
    }
    /// Keep `meta` in step with `columns` (older saved drafts have none).
    pub fn fix_meta(&mut self) {
        while self.meta.len() < self.columns.len() {
            let c = self.columns[self.meta.len()].clone();
            let used: Vec<String> = self.meta.iter().map(|m| m.key.clone()).collect();
            self.meta.push(ColMeta::plain(&schema::fresh_key(&c, &used, &self.keys.retired)));
        }
        self.meta.truncate(self.columns.len());
        if self.id_col >= self.columns.len() {
            self.id_col = 0;
        }
    }
    pub fn col_keys(&self) -> Vec<String> {
        self.meta.iter().map(|m| m.key.clone()).collect()
    }
    pub fn pk_key(&self) -> String {
        self.meta.get(self.id_col).map(|m| m.key.clone()).unwrap_or_default()
    }
    /// Who may write, as the chain will store it: nobody listed = anyone.
    pub fn desired_writers(&self, db_wallet: Option<&str>) -> Vec<String> {
        if self.open {
            return vec![];
        }
        let mut v: Vec<String> = db_wallet.map(|w| vec![w.to_string()]).unwrap_or_default();
        for w in &self.writers {
            if !v.contains(w) {
                v.push(w.clone());
            }
        }
        v
    }
    /// A rename or a change of writers waits to be saved.
    pub fn meta_changed(&self, db_wallet: Option<&str>) -> bool {
        self.created.is_some()
            && (self.chain_title.as_ref().map(|t| *t != self.title).unwrap_or(false) || self.chain_writers.as_ref().map(|w| *w != self.desired_writers(db_wallet)).unwrap_or(false))
    }
    /// Before changing the name or writers of a saved table, remember what the chain has.
    pub fn remember_chain_meta(&mut self, db_wallet: Option<&str>) {
        if self.created.is_some() {
            if self.chain_title.is_none() {
                self.chain_title = Some(self.title.clone());
            }
            if self.chain_writers.is_none() {
                self.chain_writers = Some(self.desired_writers(db_wallet));
            }
        }
    }
    /// Tables IQ Tables keeps for itself (views and such).
    pub fn is_system(&self) -> bool {
        self.name.starts_with(SYSTEM_TABLE)
    }
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.eq_ignore_ascii_case(name))
    }
    pub fn doc(&self) -> Doc {
        Doc {
            cols: self.columns.iter().cloned().zip(self.meta.iter().cloned()).collect(),
            pk: self.pk_key(),
            keys: self.keys.clone(),
            clear: self.clear,
            dropped: self.dropped,
            snap: vec![],
            crowd: self.crowd.clone(),
        }
    }
    /// The structure without the one-off events, as text (for comparing).
    pub fn doc_text(&self) -> String {
        let mut d = self.doc();
        d.clear = false;
        d.dropped = false;
        d.to_json().to_string()
    }
    /// Does saving need to write a structure record?
    pub fn schema_changed(&self) -> bool {
        if self.clear || self.dropped || self.checkpoint {
            return true;
        }
        match &self.chain_doc {
            Some(c) => *c != self.doc_text(),
            None => !self.doc().is_trivial(),
        }
    }
    /// Adopt a structure (read from the chain); pending rows follow their
    /// columns by storage key.
    pub fn apply_doc(&mut self, d: &Doc) {
        let old_keys = self.col_keys();
        let new_keys: Vec<String> = d.cols.iter().map(|(_, m)| m.key.clone()).collect();
        if old_keys != new_keys {
            for r in self.rows.iter_mut() {
                let vals: Vec<Json> = new_keys.iter().map(|k| old_keys.iter().position(|o| o == k).and_then(|p| r.vals.get(p).cloned()).unwrap_or(Json::Null)).collect();
                r.vals = vals;
            }
        }
        self.columns = d.cols.iter().map(|(n, _)| n.clone()).collect();
        self.meta = d.cols.iter().map(|(_, m)| m.clone()).collect();
        self.id_col = d.pos(&d.pk).unwrap_or(0);
        self.keys = d.keys.clone();
        if d.crowd.is_some() {
            self.crowd = d.crowd.clone();
        }
    }
}

/// Name prefix of IQ Tables' own table in a database (views live there).
pub const SYSTEM_TABLE: &str = "_iqt";

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
    /// Saved queries (kept in this browser): (name, SQL).
    pub bookmarks: Vec<(String, String)>,
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
            bookmarks: vec![],
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
                                    let mut o = json::obj(vec![
                                        ("name", json::s(&t.name)),
                                        ("title", json::s(&t.title)),
                                        ("columns", Json::Arr(t.columns.iter().map(|c| json::s(c)).collect())),
                                        ("idCol", json::n(t.id_col)),
                                        ("schema", t.doc().to_json()),
                                        ("open", Json::Bool(t.open)),
                                        ("compress", Json::Bool(t.compress)),
                                        ("created", opt(&t.created)),
                                        ("rows", Json::Arr(t.rows.iter().map(row_to_json).collect())),
                                    ]);
                                    if !t.writers.is_empty() {
                                        o.set("writers", Json::Arr(t.writers.iter().map(|w| json::s(w)).collect()));
                                    }
                                    if let Some(c) = &t.chain_doc {
                                        o.set("chainDoc", json::s(c));
                                    }
                                    if let Some(c) = &t.chain_title {
                                        o.set("chainTitle", json::s(c));
                                    }
                                    if let Some(w) = &t.chain_writers {
                                        o.set("chainWriters", Json::Arr(w.iter().map(|x| json::s(x)).collect()));
                                    }
                                    if t.checkpoint {
                                        o.set("checkpoint", Json::Bool(true));
                                    }
                                    if let Some(c) = &t.crowd {
                                        o.set("crowd", c.clone());
                                    }
                                    o
                                })
                                .collect(),
                        ),
                    ),
                    ("bookmarks", Json::Arr(d.bookmarks.iter().map(|(n, q)| Json::Arr(vec![json::s(n), json::s(q)])).collect())),
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
                .map(|t| {
                    let mut tb = DraftTable {
                        name: t.get("name").str_or(""),
                        title: t.get("title").str_or(""),
                        columns: t.get("columns").arr().iter().map(|c| c.str_or("")).collect(),
                        id_col: t.get("idCol").u64().unwrap_or(0) as usize,
                        open: t.get("open").bool().unwrap_or(false),
                        writers: t.get("writers").arr().iter().filter_map(|w| w.str().map(String::from)).collect(),
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
                        chain_doc: ostr(t.get("chainDoc")),
                        chain_title: ostr(t.get("chainTitle")),
                        chain_writers: match t.get("chainWriters") {
                            Json::Arr(v) => Some(v.iter().filter_map(|w| w.str().map(String::from)).collect()),
                            _ => None,
                        },
                        checkpoint: t.get("checkpoint").bool().unwrap_or(false),
                        crowd: Some(t.get("crowd").clone()).filter(|c| !c.is_null()),
                        ..Default::default()
                    };
                    if tb.title.is_empty() {
                        tb.title = tb.name.clone();
                    }
                    match Doc::from_json(t.get("schema")) {
                        Some(d) => {
                            tb.columns = d.cols.iter().map(|(n, _)| n.clone()).collect();
                            tb.meta = d.cols.iter().map(|(_, m)| m.clone()).collect();
                            tb.id_col = d.pos(&d.pk).unwrap_or(0);
                            tb.keys = d.keys.clone();
                            tb.clear = d.clear;
                            tb.dropped = d.dropped;
                        }
                        None => tb.fix_meta(),
                    }
                    tb
                })
                .collect(),
            sel: 0,
            page: 0,
            bookmarks: d.get("bookmarks").arr().iter().map(|b| (b.idx(0).str_or(""), b.idx(1).str_or(""))).collect(),
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
