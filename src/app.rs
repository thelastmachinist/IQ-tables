//! Application state, event dispatch and the asynchronous flows. Accounts and
//! wallets live in `accounts_flow.rs`, direct chain reads in `chain.rs`, file
//! attachments in `attach.rs`, the inscription pipeline in `inscribe.rs` and
//! rendering in `views.rs`.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::account::Account;
use crate::crypto::base58;
use crate::host;
use crate::inscribe::{Run, RunOp};
use crate::iq;
use crate::json::{self, Json};
use crate::net::{self, DbRootInfo};
use crate::pack;
use crate::solana::{self, b58, parse_pk, Keypair};

use crate::state::{self, Draft, DraftTable, GhostRow, Settings};
use crate::ui;

pub enum Load<T> {
    None,
    Loading,
    Ready(T),
    Err(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(t) => Some(t),
            _ => None,
        }
    }
    pub fn is_loading(&self) -> bool {
        matches!(self, Load::Loading)
    }
}

#[derive(Clone, PartialEq)]
pub enum Route {
    Databases,
    Db(String),
    Table { root: Option<String>, pda: String, record: Option<String> },
    Search(String),
    Mine,
    Account,
    Workspace,
    Draft(String),
    Settings,
    About,
}

pub use crate::records::Who;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Records,
    Raw,
}

pub struct TableView {
    pub pda: String,
    pub root: Option<String>,
    pub creator: Option<String>,
    pub db_id: Option<String>,
    pub label: Option<String>,
    pub meta: Load<Json>,
    pub rows: Vec<Json>,
    pub decoded: Vec<Option<Result<pack::SourcePack, String>>>,
    pub cursor: Option<String>,
    pub loading: bool,
    pub done: bool,
    pub err: Option<String>,
    pub load_all: bool,
    /// Reading stopped at a checkpoint: older history wasn't needed.
    pub cut: bool,
    /// Chunked rows still being reassembled (direct Solana reads).
    pub chunk_waits: usize,
    /// Read past the owner's checkpoint (other writers' older rows are wanted).
    pub full: bool,
    /// Unpacked bytes of other writers' packs so far (records::decode_row_for).
    pub others_raw: u64,
    /// Rows were decoded before the official wallet was known (with everyone
    /// held to the small limits); decode them again once it is.
    pub blind: bool,
    pub mode: Mode,
    pub who: Who,
    pub text: String,
    pub sort: Option<(String, bool)>,
    pub page: usize,
    pub selected: Option<String>,
    pub gen: u32,
}

pub struct Hit {
    pub kind: String,
    pub id: String,
    pub dbroot: String,
    pub label: String,
    pub snippet: String,
}

pub enum P {
    DbRoots,
    Search(String),
    Meta(String, u32),
    Rows(String, u32),
    RootInfo(String, u32),
    NameCheck {
        draft: String,
        name: String,
    },
    /// The Table account behind an editor table (its name and writers).
    BaseAcct(String),
    Balance(String),
    Confirm {
        what: String,
        sig: String,
        since: f64,
        after: After,
    },
    ConfirmTick {
        what: String,
        sig: String,
        since: f64,
        after: After,
    },
    Run(RunOp),
    // accounts (accounts_flow.rs)
    Unlock,
    SetPass,
    Rescan(Vec<(u32, String)>),
    Balances(Vec<String>),
    TransferHash {
        from: String,
        to: String,
        lamports: u64,
        after: After,
    },
    TransferSent(After),
    Airdrop(String),
    Sns {
        name: String,
        lamports: u64,
    },
    SaveCheck {
        key: String,
        need: u64,
        main: Option<String>,
    },
    // direct chain reads (chain.rs)
    RpcRoots,
    RpcMeta(String, u32),
    RpcSigs(String, u32),
    RpcTxs(crate::chain::Page),
    /// Reading a row that was sent in chunks.
    RpcChunk(crate::chain::ChunkRead),
    // files and links (attach.rs)
    AttachCheck(crate::attach::Job),
    /// Does an interrupted upload session exist?
    AttachSession(crate::attach::Job),
    AttachHash(crate::attach::Job),
    AttachSent(crate::attach::Job),
    /// (signature, read from Solana rather than IQ's gateway)
    TxView(String, bool),
    Files(String),
    /// Parallel upload parts (upload.rs), by batch key.
    Up(String, crate::upload::UpOp),
    // crowdfunded uploads (crowd.rs)
    /// Piece `i` of the organizer's file `fid`, to fingerprint.
    CrowdHash(u32, usize),
    /// A piece to upload, read from the contributor's file or the source.
    CrowdPiece(usize),
    /// A piece of a download: (piece, candidate).
    CrowdDl(usize, usize),
    /// Read the current piece of the download again (after a pause).
    CrowdDlNext,
    /// A decoder inscription to download (embed.rs).
    EmbedWasm(String),
    // IQ git links (git.rs)
    GitMeta(String),
    GitRows(String),
    GitTree(String),
    Ignore,
}

/// What to do once a confirmed transaction lands.
#[derive(Clone)]
pub enum After {
    Balances,
    Attach(crate::attach::Job),
    /// Funds arrived in a database wallet: continue saving.
    StartRun(String),
    /// Funds arrived for a file attachment: try it again.
    AttachFunded(crate::attach::Job),
}

/// A payment waiting for the user's confirmation.
pub struct SendReview {
    pub to: String,
    pub label: String,
    pub lamports: u64,
}

pub struct PlanCache {
    pub rev: u64,
    pub cap: usize,
    /// Saved records the plan was made from (checkpoints rewrite them all).
    pub base: usize,
    pub result: Result<Vec<pack::PlannedPack>, String>,
}

pub struct App {
    pub route: Route,
    pub settings: Settings,
    pub drafts: Vec<Draft>,
    pub dbroots: Load<Vec<DbRootInfo>>,
    pub db_filter: String,
    pub search_q: String,
    pub search: Load<Vec<Hit>>,
    pub table: Option<TableView>,
    /// The logged-in account (all its keys are in memory only while logged in).
    pub account: Option<Account>,
    /// An encrypted account file waiting for its passphrase: (name, file).
    pub locked: Option<(String, Json)>,
    /// Blocking work in progress (e.g. "Unlocking…" while PBKDF2 runs).
    pub busy: Option<String>,
    pub account_menu: bool,
    /// Files each account wallet inscribed (IQ gateway `/user/<wallet>/assets`).
    pub files: HashMap<String, Load<Vec<crate::attach::Asset>>>,
    /// A file being inscribed into a cell: ("key:table:row", status).
    pub attach_status: Option<(String, String)>,
    /// An opened `iq://tx/…` link.
    pub viewer: Option<crate::attach::Viewer>,
    pub balances: HashMap<String, Load<u64>>,
    pub name_checks: HashMap<String, Load<Option<String>>>,
    pub pending: HashMap<u32, P>,
    pub next_id: u32,
    pub toast: Option<(bool, String)>,
    pub run: Option<Run>,
    pub plans: HashMap<(String, usize), PlanCache>,
    pub revs: HashMap<(String, usize), u64>,
    pub form: HashMap<String, String>,
    pub reveal_key: Option<String>,
    /// Set when an action navigates and its message should survive the route change.
    pub keep_toast: bool,
    /// Saved records of draft tables read from the chain (the editor's base).
    pub bases: HashMap<String, TableView>,
    pub base_cache: HashMap<String, (String, std::rc::Rc<Vec<crate::sheet::BaseRec>>)>,
    pub ed: crate::editor::Ed,
    pub undo: HashMap<(String, usize), crate::editor::Undo>,
    /// Small non-blocking status ("Checking your balance…").
    pub busy_note: Option<String>,
    pub send_review: Option<SendReview>,
    /// Filter to apply once the editor opens (editing one record).
    pub pending_filter: Option<String>,
    /// Which account panel is open on the account page ("add", "send", "").
    pub panel: String,
    /// Live links to IQ git repositories.
    pub git: crate::git::Git,
    /// Upload sessions whose parts are being sent in parallel ("run", "attach").
    pub uploads: HashMap<String, crate::upload::Batch>,
    /// Crowdfunded uploads (crowd.rs).
    pub crowd: crate::crowd::Crowd,
    /// The open "use this table elsewhere" dialog (embed.rs).
    pub embed: Option<crate::embed::Embed>,
    /// This page's address (origin and path), from the browser.
    pub page_url: String,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

const K_SETTINGS: &str = "iqtables:v1:settings";
const K_DRAFTS: &str = "iqtables:v1:drafts";

// ------------------------------------------------------------------ exports

#[no_mangle]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

///
/// # Safety
/// Called only by `web/host.js` with memory from `alloc` (the page gives ownership back).
#[no_mangle]
pub unsafe extern "C" fn dealloc(p: *mut u8, len: usize) {
    drop(Vec::from_raw_parts(p, 0, len.max(1)));
}

unsafe fn owned(p: *mut u8, len: usize) -> Vec<u8> {
    Vec::from_raw_parts(p, len, len.max(1))
}

unsafe fn owned_str(p: *mut u8, len: usize) -> String {
    String::from_utf8_lossy(&owned(p, len)).into_owned()
}

#[no_mangle]
pub extern "C" fn start() {
    APP.with(|a| {
        // nothing about accounts is kept in the browser any more
        for k in crate::accounts_flow::LEGACY_FLAGS {
            if host::storage_get(k).map(|v| !v.is_empty()).unwrap_or(false) {
                host::storage_set(k, "");
            }
        }
        *a.borrow_mut() = Some(App::new());
    });
}

/// Key presses from elements marked data-keys. Returns 1 when handled (the
/// page then prevents the browser's default action).
///
/// # Safety
/// Called only by `web/host.js` with memory from `alloc` (the page gives ownership back).
#[no_mangle]
pub unsafe extern "C" fn on_key(ap: *mut u8, al: usize, gp: *mut u8, gl: usize, kp: *mut u8, kl: usize, vp: *mut u8, vl: usize) -> u32 {
    let action = owned_str(ap, al);
    let arg = owned_str(gp, gl);
    let key = owned_str(kp, kl);
    let val = owned_str(vp, vl);
    let mut handled = 0;
    APP.with(|a| {
        if let Ok(mut g) = a.try_borrow_mut() {
            if let Some(app) = g.as_mut() {
                let (h, r) = app.key(&action, &arg, &key, &val);
                handled = h as u32;
                if r {
                    app.render();
                }
            }
        }
    });
    handled
}

///
/// # Safety
/// Called only by `web/host.js` with memory from `alloc` (the page gives ownership back).
#[no_mangle]
pub unsafe extern "C" fn on_event(kp: *mut u8, kl: usize, ap: *mut u8, al: usize, gp: *mut u8, gl: usize, vp: *mut u8, vl: usize) {
    let kind = owned_str(kp, kl);
    let action = owned_str(ap, al);
    let arg = owned_str(gp, gl);
    let val = owned_str(vp, vl);
    with_app(|app| app.event(&kind, &action, &arg, &val));
}

///
/// # Safety
/// Called only by `web/host.js` with memory from `alloc` (the page gives ownership back).
#[no_mangle]
pub unsafe extern "C" fn on_async(id: u32, ok: u32, status: u32, p: *mut u8, l: usize) {
    let data = owned(p, l);
    with_app(|app| app.async_done(id, ok != 0, status, data));
}

fn with_app(f: impl FnOnce(&mut App) -> bool) {
    APP.with(|a| {
        if let Ok(mut g) = a.try_borrow_mut() {
            if let Some(app) = g.as_mut() {
                if f(app) {
                    app.render();
                }
            }
        }
    });
}

// ---------------------------------------------------------------------- app

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let settings = host::storage_get(K_SETTINGS).and_then(|s| json::parse(&s).ok()).map(|v| Settings::from_json(&v)).unwrap_or_default();
        let drafts = host::storage_get(K_DRAFTS).and_then(|s| json::parse(&s).ok()).map(|v| state::drafts_from_json(&v)).unwrap_or_default();
        App {
            route: Route::Databases,
            settings,
            drafts,
            dbroots: Load::None,
            db_filter: String::new(),
            search_q: String::new(),
            search: Load::None,
            table: None,
            account: None,
            locked: None,
            busy: None,
            account_menu: false,
            files: HashMap::new(),
            attach_status: None,
            viewer: None,
            balances: HashMap::new(),
            name_checks: HashMap::new(),
            pending: HashMap::new(),
            next_id: 1,
            toast: None,
            run: None,
            plans: HashMap::new(),
            revs: HashMap::new(),
            form: HashMap::new(),
            reveal_key: None,
            keep_toast: false,
            bases: HashMap::new(),
            base_cache: HashMap::new(),
            ed: crate::editor::Ed { tab: "browse".into(), ..Default::default() },
            undo: HashMap::new(),
            busy_note: None,
            send_review: None,
            pending_filter: None,
            panel: String::new(),
            git: Default::default(),
            uploads: HashMap::new(),
            crowd: Default::default(),
            embed: None,
            page_url: String::new(),
        }
    }

    pub fn nid(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    pub fn save_drafts(&self) {
        host::storage_set(K_DRAFTS, &state::drafts_to_json(&self.drafts).to_string());
    }

    pub fn save_settings(&self) {
        host::storage_set(K_SETTINGS, &self.settings.to_json().to_string());
    }

    pub fn ok(&mut self, m: impl Into<String>) {
        self.toast = Some((true, m.into()));
    }
    pub fn err(&mut self, m: impl Into<String>) {
        self.toast = Some((false, m.into()));
    }

    pub fn get(&mut self, path: &str, p: P) {
        let id = self.nid();
        self.pending.insert(id, p);
        let url = format!("{}{}", self.gateway_url(), path);
        host::fetch(id, "GET", &url, "", "");
    }

    pub fn rpc(&mut self, method: &str, params: Json, p: P) {
        let id = self.nid();
        self.pending.insert(id, p);
        host::fetch(id, "POST", &self.settings.rpc.clone(), &net::rpc_body(method, params), "application/json");
    }

    pub fn timer(&mut self, ms: u32, p: P) {
        let id = self.nid();
        self.pending.insert(id, p);
        host::timer(id, ms);
    }

    pub fn draft_idx(&self, key: &str) -> Option<usize> {
        self.drafts.iter().position(|d| d.key == key)
    }

    pub fn bump(&mut self, key: &str, t: usize) {
        *self.revs.entry((key.to_string(), t)).or_insert(0) += 1;
    }

    /// Table `t` of `key` was removed: state kept per table index moves with
    /// the tables that shift down, and nothing computed for the old index
    /// survives.
    pub fn table_removed(&mut self, key: &str, t: usize) {
        self.plans.retain(|(k, _), _| k != key);
        let n = self.draft_idx(key).map(|i| self.drafts[i].tables.len()).unwrap_or(0);
        for j in 0..=n {
            self.bump(key, j);
        }
        let keys: Vec<(String, usize)> = self.undo.keys().filter(|(k, j)| k == key && *j >= t).cloned().collect();
        let mut moved: Vec<(usize, crate::editor::Undo)> = keys.into_iter().filter_map(|ck| self.undo.remove(&ck).map(|u| (ck.1, u))).collect();
        moved.sort_by_key(|m| m.0);
        for (j, u) in moved {
            if j > t {
                self.undo.insert((key.to_string(), j - 1), u);
            }
        }
        self.ed.table = (String::new(), usize::MAX);
    }

    /// Key for an address, if it belongs to the logged-in account.
    pub fn keypair(&self, address: &str) -> Option<Keypair> {
        self.account.as_ref()?.find(address).map(|w| w.kp.clone())
    }

    /// The database wallet's key for a draft, if it's in the account.
    pub fn draft_keypair(&self, key: &str) -> Option<Keypair> {
        let w = self.drafts.get(self.draft_idx(key)?)?.wallet.clone()?;
        self.keypair(&w)
    }

    /// Read tables straight from Solana instead of the IQ gateway.
    pub fn use_rpc(&self) -> bool {
        self.settings.source == "rpc"
    }

    /// IQ's gateway for the cluster in use: its devnet twin on devnet
    /// (unless a custom gateway is set).
    pub fn gateway_url(&self) -> String {
        let g = self.settings.gateway.trim_end_matches('/').to_string();
        if self.settings.cluster == "devnet" && g == "https://gateway.iqlabs.dev" {
            return DEV_GATEWAY.into();
        }
        g
    }

    pub fn render(&mut self) {
        self.sync_sheet();
        let html = crate::views::render(self);
        host::render(&html);
        self.git_fetch_wanted();
    }

    // ------------------------------------------------------------ routing

    fn route(&mut self, hash: &str) {
        let h = hash.trim_start_matches('#');
        let parts: Vec<&str> = h.split('/').filter(|s| !s.is_empty()).collect();
        if self.keep_toast {
            self.keep_toast = false;
        } else {
            self.toast = None;
        }
        self.account_menu = false;
        self.embed = None;
        self.route = match parts.as_slice() {
            ["db", pda] => Route::Db(pda.to_string()),
            ["t", root, pda, "r", rec] => Route::Table { root: Some(root.to_string()), pda: pda.to_string(), record: Some(pct_decode(rec)) },
            ["t", pda, "r", rec] => Route::Table { root: None, pda: pda.to_string(), record: Some(pct_decode(rec)) },
            ["t", root, pda] => Route::Table { root: Some(root.to_string()), pda: pda.to_string(), record: None },
            ["t", pda] => Route::Table { root: None, pda: pda.to_string(), record: None },
            ["search", q] => Route::Search(pct_decode(q)),
            ["mine"] => Route::Mine,
            ["account"] => Route::Account,
            ["ws"] => Route::Workspace,
            ["ws", key] => {
                // opening a database shows its tables, as in phpMyAdmin
                self.ed.scope_db = true;
                self.ed.tab = "structure".into();
                self.ed.col_edit = None;
                Route::Draft(key.to_string())
            }
            ["ws", key, t] => {
                // from the database's screens, a table opens on Browse
                if self.ed.scope_db && self.ed.tab != "structure" {
                    self.ed.tab = "browse".into();
                }
                self.ed.scope_db = false;
                self.ed.col_edit = None;
                if let (Some(i), Ok(t)) = (self.draft_idx(key), t.parse::<usize>()) {
                    if t < self.drafts[i].tables.len() {
                        self.drafts[i].sel = t;
                    }
                }
                // picking a table shows it, as in phpMyAdmin's tree
                if self.ed.tab == "sql" || self.ed.tab == "save" {
                    self.ed.tab = "browse".into();
                }
                Route::Draft(key.to_string())
            }
            ["settings"] => Route::Settings,
            ["about"] => Route::About,
            _ => Route::Databases,
        };
        match self.route.clone() {
            Route::Databases | Route::Db(_) => self.ensure_dbroots(),
            Route::Table { root, pda, record } => {
                self.ensure_dbroots();
                self.open_table(root, pda);
                if let (Some(rec), Some(t)) = (record, self.table.as_mut()) {
                    t.text = rec.clone();
                    t.selected = Some(rec);
                }
            }
            Route::Search(q) => {
                self.search_q = q.clone();
                if self.use_rpc() {
                    self.ensure_dbroots();
                    self.search = Load::None; // searched locally over the database list
                } else {
                    self.search = Load::Loading;
                    let path = format!("/search?q={}&limit=40", pct_encode(&q));
                    self.get(&path, P::Search(q));
                }
            }
            Route::Mine => {
                self.load_mine();
                self.fetch_all_balances();
            }
            Route::Account => self.fetch_all_balances(),
            Route::Draft(key) => {
                if let Some(i) = self.draft_idx(&key) {
                    if let Some(w) = self.drafts[i].wallet.clone() {
                        if !self.balances.contains_key(&w) {
                            self.fetch_balance(&w);
                        }
                    }
                    if !self.name_checks.contains_key(&key) {
                        self.check_name(&key);
                    }
                }
                let main = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address());
                if let Some(m) = main {
                    if !self.balances.contains_key(&m) {
                        self.fetch_all_balances();
                    }
                }
            }
            _ => {}
        }
    }

    pub fn ensure_dbroots(&mut self) {
        if matches!(self.dbroots, Load::None | Load::Err(_)) {
            self.dbroots = Load::Loading;
            if self.use_rpc() {
                self.rpc_roots();
            } else {
                self.get("/dbroots", P::DbRoots);
            }
        }
    }

    pub fn find_table(&self, pda: &str) -> Option<(&DbRootInfo, &net::TableRef)> {
        let roots = self.dbroots.ready()?;
        for r in roots {
            if let Some(t) = r.tables.iter().find(|t| t.pda == pda) {
                return Some((r, t));
            }
        }
        None
    }

    fn open_table(&mut self, root: Option<String>, pda: String) {
        self.open_table_with(root, pda, false);
    }

    /// (Re)load a table. Revisiting the same table keeps the view settings
    /// (filters, sort, mode) but always fetches fresh rows.
    fn open_table_with(&mut self, root: Option<String>, pda: String, fresh: bool) {
        let prev = self.table.take().filter(|t| t.pda == pda);
        let gen = self.nid();
        self.table = Some(TableView {
            pda: pda.clone(),
            root: root.clone(),
            creator: None,
            db_id: None,
            label: None,
            meta: Load::Loading,
            rows: vec![],
            decoded: vec![],
            cursor: None,
            loading: true,
            done: false,
            err: None,
            load_all: false,
            cut: false,
            chunk_waits: 0,
            full: false,
            others_raw: 0,
            blind: false,
            mode: Mode::Records,
            who: Who::Official,
            text: String::new(),
            sort: None,
            page: 0,
            selected: None,
            gen,
        });
        if let (Some(p), Some(t)) = (prev, self.table.as_mut()) {
            t.mode = p.mode;
            t.who = p.who;
            t.sort = p.sort;
            if fresh {
                t.text = p.text;
            }
            t.label = p.label;
            t.creator = p.creator;
            t.db_id = p.db_id;
            if t.root.is_none() {
                t.root = p.root;
            }
        }
        self.attach_root_info();
        if self.use_rpc() {
            self.rpc_meta(&pda, gen);
            self.rpc_rows(&pda, gen, None);
        } else {
            self.get(&format!("/table/{}/meta", pda), P::Meta(pda.clone(), gen));
            let q = if fresh { "&fresh=1" } else { "" };
            self.get(&format!("/table/{}/rows?limit=100{}", pda, q), P::Rows(pda.clone(), gen));
        }
        let needs_root = self.table.as_ref().map(|t| t.creator.is_none()).unwrap_or(false);
        if needs_root {
            if let Some(r) = root {
                // not in the (30-minute cached) gateway list yet: ask the chain
                self.rpc("getAccountInfo", json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\"}}]", r)).unwrap(), P::RootInfo(pda, gen));
            }
        }
    }

    /// Fill creator / database name for the open table from the gateway list.
    pub fn attach_root_info(&mut self) {
        let Some(t) = self.table.as_ref() else { return };
        let pda = t.pda.clone();
        let root = t.root.clone();
        let info = self.find_table(&pda).map(|(r, tr)| (r.pda.clone(), r.creator.clone(), r.id.clone(), net::label_of(tr)));
        let info = info.or_else(|| {
            let r = root.as_ref()?;
            let d = self.dbroots.ready()?.iter().find(|d| &d.pda == r)?;
            Some((d.pda.clone(), d.creator.clone(), d.id.clone(), String::new()))
        });
        if let (Some((rp, creator, id, label)), Some(t)) = (info, self.table.as_mut()) {
            t.root = Some(rp);
            t.creator = Some(creator);
            redecode(t);
            t.db_id = id;
            if !label.is_empty() {
                t.label = Some(label);
            }
        }
    }

    pub fn more_rows(&mut self) {
        if let Some(pda) = self.table.as_ref().map(|t| t.pda.clone()) {
            self.more_rows_for(&pda);
        }
    }

    /// The table view (explorer) or editor base loading `pda` at generation `gen`.
    pub fn tv_mut(&mut self, pda: &str, gen: u32) -> Option<&mut TableView> {
        if self.table.as_ref().map(|t| t.pda == pda && t.gen == gen).unwrap_or(false) {
            return self.table.as_mut();
        }
        self.bases.get_mut(pda).filter(|t| t.gen == gen)
    }

    /// The wallet whose records count as official for this table: the
    /// database's creator (explorer), or the draft's owner (editor).
    pub fn official_for(&self, pda: &str) -> Option<String> {
        if let Some(c) = self.table.as_ref().filter(|t| t.pda == pda).and_then(|t| t.creator.clone()) {
            return Some(c);
        }
        for d in &self.drafts {
            for t in 0..d.tables.len() {
                if self.table_pda_of(&d.key, t).map(|(_, p)| p == pda).unwrap_or(false) {
                    // the database's creator: from the name check, the database list, or our own wallet
                    let root = b58(&iq::db_root_pda(d.name.as_bytes()));
                    let listed = self.dbroots.ready().and_then(|rs| rs.iter().find(|r| r.pda == root)).map(|r| r.creator.clone());
                    return self.creator_of(&d.key).or(listed).or_else(|| d.wallet.clone());
                }
            }
        }
        None
    }

    /// Reading a table newest-first for everything (an editor base): stop at
    /// the owner's latest checkpoint once its packs are in. Returns true if
    /// reading can stop.
    pub fn stop_at_checkpoint(&mut self, pda: &str, gen: u32) -> bool {
        let Some(owner) = self.official_for(pda) else { return false };
        let Some(t) = self.tv_mut(pda, gen) else { return false };
        if t.full {
            return false;
        }
        let packs: Vec<pack::SourcePack> = t.decoded.iter().filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok())).cloned().collect();
        // a crowdfunded table is always read to its start: its first manifest is what counts
        if crate::crowd::looks_crowd(&packs.iter().collect::<Vec<_>>()) {
            return false;
        }
        if !pack::checkpoint_covers(&packs, &|s| s == owner) {
            return false;
        }
        t.cut = true;
        if !t.load_all || t.done {
            return false;
        }
        t.done = true;
        t.load_all = false;
        true
    }

    /// Fetch the next page of rows for the explorer table or an editor base.
    pub fn more_rows_for(&mut self, pda: &str) {
        let rpc = self.use_rpc();
        let t = if self.table.as_ref().map(|t| t.pda == pda).unwrap_or(false) { self.table.as_mut() } else { self.bases.get_mut(pda) };
        let Some(t) = t else { return };
        if t.loading || t.done {
            return;
        }
        t.loading = true;
        t.err = None;
        let (pda, gen, cur) = (t.pda.clone(), t.gen, t.cursor.clone());
        if rpc {
            self.rpc_rows(&pda, gen, cur);
            return;
        }
        let path = match &cur {
            Some(c) => format!("/table/{}/rows?limit=100&before={}", pda, c),
            None => format!("/table/{}/rows?limit=100", pda),
        };
        self.get(&path, P::Rows(pda, gen));
    }

    // ------------------------------------------------------------- events

    pub fn event(&mut self, kind: &str, action: &str, arg: &str, val: &str) -> bool {
        match (kind, action) {
            ("route", _) => self.route(val),
            ("page", _) => self.page_url = val.to_string(),
            (_, "go") => host::set_hash(arg),
            (_, "toast-close") => self.toast = None,
            (_, "db-filter") => self.db_filter = val.to_string(),
            (_, "search") => {
                let q = val.trim();
                if !q.is_empty() {
                    host::set_hash(&format!("#/search/{}", pct_encode(q)));
                }
            }
            (_, "copy") => {
                host::copy(arg);
                self.ok("Copied to clipboard");
            }
            // table view
            (_, "tv-more") => self.more_rows(),
            (_, "tv-refresh") => {
                if let Some(t) = self.table.as_ref() {
                    let (root, pda) = (t.root.clone(), t.pda.clone());
                    self.open_table_with(root, pda, true);
                }
            }
            (_, "tv-all") => {
                if let Some(t) = self.table.as_mut() {
                    t.load_all = true;
                }
                self.more_rows();
            }
            (_, "tv-who") => {
                if let Some(t) = self.table.as_mut() {
                    t.who = match arg {
                        "unofficial" => Who::Unofficial,
                        "all" => Who::All,
                        _ => Who::Official,
                    };
                    t.page = 0;
                }
            }
            (_, "tv-mode") => {
                if let Some(t) = self.table.as_mut() {
                    t.mode = if arg == "raw" { Mode::Raw } else { Mode::Records };
                    t.page = 0;
                    t.sort = None;
                }
            }
            (_, "tv-text") => {
                if let Some(t) = self.table.as_mut() {
                    t.text = val.to_string();
                    t.page = 0;
                }
            }
            (_, "tv-sort") => {
                if let Some(t) = self.table.as_mut() {
                    t.sort = match &t.sort {
                        Some((c, false)) if c == arg => Some((arg.to_string(), true)),
                        Some((c, true)) if c == arg => None,
                        _ => Some((arg.to_string(), false)),
                    };
                }
            }
            (_, "tv-page") => {
                if let Some(t) = self.table.as_mut() {
                    t.page = arg.parse().unwrap_or(0);
                }
            }
            (_, "tv-select") => {
                if let Some(t) = self.table.as_mut() {
                    t.selected = if t.selected.as_deref() == Some(arg) { None } else { Some(arg.to_string()) };
                }
            }
            (_, "tv-export") => self.export_view(arg),
            (_, "tv-draft") => self.draft_from_table(arg),
            (_, "db-edit") => {
                let db = self.dbroots.ready().and_then(|rs| rs.iter().find(|d| d.pda == arg)).and_then(|d| d.id.clone());
                self.open_in_editor(db, Some(arg.to_string()), None, None);
            }
            (_, "copy-record") => {
                if let Some(t) = self.table.as_ref() {
                    let link = format!("iq://table/{}/{}", t.pda, pct_encode(arg));
                    host::copy(&link);
                    self.ok(format!("Copied {}", link));
                }
            }
            // workspace
            (_, "form") => {
                // form fields keep their own text; no re-render needed
                self.form.insert(arg.to_string(), val.to_string());
                return false;
            }
            (_, "new-db") => self.new_draft(),
            (_, "del-draft") => {
                if let Some(i) = self.draft_idx(arg) {
                    self.drafts.remove(i);
                    self.save_drafts();
                    host::set_hash("#/ws");
                }
            }
            (_, "reveal-key") => {
                self.reveal_key = if self.reveal_key.as_deref() == Some(arg) { None } else { Some(arg.to_string()) };
            }
            (_, "lock-creators") => {
                if let Some(i) = self.draft_idx(arg) {
                    self.drafts[i].lock_creators = val == "true";
                    self.save_drafts();
                }
            }
            (_, "balance") => self.fetch_balance(arg),
            (_, "add-table") => self.add_table(arg.strip_prefix("tname:").unwrap_or(arg)),
            (_, "sel-table") => {
                let mut it = arg.splitn(2, ':');
                let (k, t) = (it.next().unwrap_or(""), it.next().unwrap_or("0").parse().unwrap_or(0));
                if let Some(i) = self.draft_idx(k) {
                    self.drafts[i].sel = t;
                    self.drafts[i].page = 0;
                }
            }
            (_, "del-table") | (_, "del-row") | (_, "clear-ghosts") | (_, "inscribe") if self.attach_busy(arg.split(':').next().unwrap_or("")) => {
                self.err("Wait for the file being attached to finish.");
            }
            (_, "del-table") => {
                let mut removed = None;
                self.edit_table(arg, |d, t| {
                    if d.tables[t].created.is_none() {
                        d.tables.remove(t);
                        d.sel = 0;
                        removed = Some(t);
                    }
                });
                if let Some(t) = removed {
                    self.table_removed(arg.split(':').next().unwrap_or(""), t);
                }
            }
            (_, "table-open") => {
                let v = val == "true";
                self.edit_table(arg, move |d, t| {
                    let w = d.wallet.clone();
                    d.tables[t].remember_chain_meta(w.as_deref());
                    d.tables[t].open = v;
                })
            }
            (_, "table-compress") => {
                let v = val == "true";
                self.edit_table(arg, move |d, t| d.tables[t].compress = v)
            }
            (_, "add-row") => self.edit_table(arg, |d, t| {
                let n = d.tables[t].columns.len();
                d.tables[t].rows.push(GhostRow { vals: vec![Json::Null; n], deleted: false, sig: None });
                // ghost rows are listed first, so the new one is at the end of those
                let ghosts = d.tables[t].ghosts();
                d.page = (ghosts.saturating_sub(1)) / ROWS_PER_PAGE;
            }),
            (_, "cell") => self.edit_cell(arg, val),
            (_, "del-row") => self.row_op(arg, "del"),
            (_, "tomb-row") => self.row_op(arg, "tomb"),
            (_, "clear-ghosts") => self.edit_table(arg, |d, t| d.tables[t].rows.retain(|r| r.sig.is_some())),
            (_, "ws-page") => {
                let mut it = arg.splitn(2, ':');
                let (k, p) = (it.next().unwrap_or(""), it.next().unwrap_or("0").parse().unwrap_or(0));
                if let Some(i) = self.draft_idx(k) {
                    self.drafts[i].page = p;
                }
            }
            (_, "import-csv") => {
                let text = self.form.get(&format!("csv:{}", arg)).cloned().unwrap_or_default();
                self.import_text(arg, &text);
            }
            ("file", "import-file") => self.import_text(arg, val),
            ("file", "attach") => self.attach_file(arg, val),
            (_, "open-tx") => self.open_tx(arg, val),
            (_, "viewer-close") => self.viewer = None,
            (_, a) if a.starts_with("git-") => self.git_action(a, arg),
            (_, a) if a.starts_with("embed-") => {
                self.embed_event(a, arg, val);
            }
            (k, a) if a.starts_with("crowd-") && self.crowd_event(k, a, arg, val) => {}
            (_, "viewer-download") => self.viewer_download(),
            (_, "attach-col") => {
                let mut it = arg.splitn(2, ':');
                let (k, t) = (it.next().unwrap_or("").to_string(), it.next().unwrap_or("0").parse().unwrap_or(0));
                self.form.insert(format!("attachcol:{}:{}", k, t), val.to_string());
            }
            (_, "inscribe") => self.save(arg),
            (_, "run-stop") => {
                if let Some(r) = self.run.as_mut() {
                    r.stop = true;
                }
            }
            (_, "run-resume") => {
                let paused = self.run.as_ref().map(|r| matches!(r.state, crate::inscribe::RunState::Paused(_))).unwrap_or(false);
                match (paused, self.run.as_ref().map(|r| r.draft.clone())) {
                    (true, Some(k)) => self.save(&k),
                    _ => self.resume_run(),
                }
            }
            (_, "run-close") => {
                if self.run.as_ref().map(|r| !r.busy()).unwrap_or(true) {
                    self.run = None;
                    self.uploads.remove("run");
                }
            }
            // settings
            (_, "set") => self.set_setting(arg, val),
            (_, "export-ws") => {
                let data = state::drafts_to_json(&self.drafts).to_string();
                host::download("iq-tables-drafts.json", "application/json", data.as_bytes());
            }
            ("file", "import-ws") => match json::parse(val) {
                Ok(v) => {
                    let mut ds = state::drafts_from_json(&v);
                    for d in ds.iter_mut() {
                        if self.draft_idx(&d.key).is_some() {
                            d.key = self.new_key();
                        }
                    }
                    let n = ds.len();
                    self.drafts.extend(ds);
                    self.save_drafts();
                    self.ok(format!("Imported {} draft database(s)", n));
                }
                Err(e) => self.err(format!("Not a drafts file: {}", e)),
            },
            _ => {
                if let Some(r) = self.editor_event(kind, action, arg, val) {
                    return r;
                }
                return self.account_event(kind, action, arg, val);
            }
        }
        true
    }

    fn set_setting(&mut self, key: &str, val: &str) {
        match key {
            "rpc" => self.settings.rpc = val.trim().to_string(),
            "gateway" => {
                self.settings.gateway = val.trim().trim_end_matches('/').to_string();
                self.dbroots = Load::None;
            }
            "cluster" => {
                self.settings.cluster = val.to_string();
                // the default RPC follows the cluster; a custom one is left alone
                if val == "devnet" && (self.settings.rpc == state::RPC_MAINNET || self.settings.rpc.contains("mainnet")) {
                    self.settings.rpc = state::RPC_DEVNET.into();
                } else if val == "mainnet" && (self.settings.rpc == state::RPC_DEVNET || self.settings.rpc.contains("devnet")) {
                    self.settings.rpc = state::RPC_MAINNET.into();
                }
                self.balances.clear();
                self.name_checks.clear();
                self.dbroots = Load::None;
                self.files.clear();
                // the same addresses hold different data on the other cluster
                self.bases.clear();
                self.base_cache.clear();
                self.table = None;
            }
            "tx" => {
                self.settings.tx_format = match val {
                    "v1" => state::TxFormat::V1,
                    "legacy" => state::TxFormat::Legacy,
                    _ => state::TxFormat::Auto,
                };
                self.plans.clear();
            }
            "source" => {
                self.settings.source = if val == "rpc" { "rpc".into() } else { "gateway".into() };
                self.dbroots = Load::None;
                self.files.clear();
            }
            "simulate" => self.settings.simulate = val == "true",
            "upload_speed" => self.settings.upload_speed = crate::upload::profile(val).name.to_string(),
            "notify" => self.settings.notify_gateway = val == "true",
            _ => {}
        }
        self.save_settings();
    }

    // ------------------------------------------------------------- async

    pub fn async_done(&mut self, id: u32, ok: bool, status: u32, data: Vec<u8>) -> bool {
        let Some(p) = self.pending.remove(&id) else { return false };
        let text = || String::from_utf8_lossy(&data).into_owned();
        let http_ok = ok && (200..300).contains(&status);
        match p {
            P::Ignore => return false,
            P::DbRoots => {
                self.dbroots = if http_ok {
                    match json::parse(&text()) {
                        Ok(v) => Load::Ready(net::parse_dbroots(&v)),
                        Err(e) => Load::Err(e),
                    }
                } else {
                    Load::Err(fetch_err(ok, status, &text()))
                };
                self.attach_root_info();
            }
            P::Search(q) => {
                if q != self.search_q {
                    return false;
                }
                self.search = if http_ok {
                    match json::parse(&text()) {
                        Ok(v) => Load::Ready(
                            v.get("hits")
                                .arr()
                                .iter()
                                .map(|h| Hit {
                                    kind: h.get("kind").str_or(""),
                                    id: h.get("id").str_or(""),
                                    dbroot: h.get("dbroot").str_or(""),
                                    label: h.get("label").str_or(""),
                                    snippet: h.get("snippet").str_or(""),
                                })
                                .collect(),
                        ),
                        Err(e) => Load::Err(e),
                    }
                } else {
                    Load::Err(fetch_err(ok, status, &text()))
                };
            }
            P::Meta(pda, gen) => {
                if let Some(t) = self.tv_mut(&pda, gen) {
                    t.meta = if http_ok { json::parse(&text()).map(Load::Ready).unwrap_or_else(Load::Err) } else { Load::Err(fetch_err(ok, status, &text())) };
                }
            }
            P::RootInfo(pda, gen) => {
                let creator = if http_ok {
                    net::rpc_result(&text()).ok().and_then(|r| net::account_data(r.get("value"))).and_then(|d| iq::decode_db_root(&d))
                } else {
                    None
                };
                if let (Some(t), Some(root)) = (self.table.as_mut().filter(|t| t.pda == pda && t.gen == gen), creator) {
                    t.creator = Some(b58(&root.creator));
                    redecode(t);
                    if t.db_id.is_none() {
                        t.db_id = String::from_utf8(root.id.clone()).ok();
                    }
                    // Recover the table's name from the DbRoot's hints: the one
                    // whose derived PDA is this table.
                    if t.label.is_none() {
                        if let Some(rp) = t.root.as_ref().and_then(|r| parse_pk(r)) {
                            for h in root.table_seeds.iter().chain(root.global_table_seeds.iter()) {
                                if let Ok(name) = std::str::from_utf8(h) {
                                    if b58(&iq::table_pda(&rp, &iq::seed_bytes(name))) == t.pda {
                                        t.label = Some(name.to_string());
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            P::Rows(pda, gen) => {
                let mut again = false;
                let mut finished = false;
                let official = self.official_for(&pda);
                if let Some(t) = self.tv_mut(&pda, gen) {
                    t.loading = false;
                    if http_ok {
                        match json::parse(&text()) {
                            Ok(v) => {
                                let rows = v.get("rows").arr().to_vec();
                                for r in &rows {
                                    let d = decode_for(r, official.as_deref(), t);
                                    t.decoded.push(d);
                                }
                                let n = rows.len();
                                t.rows.extend(rows);
                                t.cursor = v.get("nextCursor").str().map(String::from);
                                // a crowdfunded table is read whole (its manifest is its oldest record)
                                if !t.load_all
                                    && crate::crowd::looks_crowd(&t.decoded.iter().filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok())).collect::<Vec<_>>())
                                {
                                    t.load_all = true;
                                    // its rows come from everyone who uploads a piece
                                    t.who = Who::All;
                                }
                                if n == 0 || t.cursor.is_none() {
                                    t.done = true;
                                    t.load_all = false;
                                    finished = true;
                                }
                                again = t.load_all && !t.done && t.rows.len() < 20_000;
                            }
                            Err(e) => t.err = Some(e),
                        }
                    } else {
                        t.err = Some(fetch_err(ok, status, &text()));
                        t.load_all = false;
                    }
                }
                if (again || finished) && self.stop_at_checkpoint(&pda, gen) {
                    again = false;
                    finished = true;
                }
                if again {
                    self.more_rows_for(&pda);
                } else if finished {
                    self.base_loaded(&pda);
                }
            }
            P::BaseAcct(pda) => {
                if http_ok {
                    if let Some(m) = net::rpc_result(&text()).ok().and_then(|r| net::account_data(r.get("value"))).and_then(|d| iq::decode_table(&d)) {
                        self.adopt_table_meta(&pda, &m);
                    }
                }
            }
            P::NameCheck { draft, name } => {
                let cur = self.draft_idx(&draft).map(|i| self.drafts[i].name.clone());
                if cur.as_deref() == Some(&name) {
                    let v = if http_ok {
                        match net::rpc_result(&text()) {
                            Ok(r) => Load::Ready(net::account_data(r.get("value")).and_then(|d| iq::decode_db_root(&d)).map(|d| b58(&d.creator))),
                            Err(e) => Load::Err(e),
                        }
                    } else {
                        Load::Err(fetch_err(ok, status, &text()))
                    };
                    self.name_checks.insert(draft, v);
                }
            }
            P::Balance(addr) => {
                let v = if http_ok {
                    match net::rpc_result(&text()) {
                        Ok(r) => r.get("value").u64().map(Load::Ready).unwrap_or(Load::Err("no balance".into())),
                        Err(e) => Load::Err(e),
                    }
                } else {
                    Load::Err(fetch_err(ok, status, &text()))
                };
                self.balances.insert(addr, v);
            }
            P::ConfirmTick { what, sig, since, after } => {
                let params = json::parse(&format!("[[\"{}\"]]", sig)).unwrap();
                self.rpc("getSignatureStatuses", params, P::Confirm { what, sig, since, after });
                return false;
            }
            P::Confirm { what, sig, since, after } => {
                let st = if http_ok { net::rpc_result(&text()).ok() } else { None };
                let v = st.as_ref().map(|r| r.get("value").idx(0).clone()).unwrap_or(Json::Null);
                let conf = v.get("confirmationStatus").str_or("");
                if !v.get("err").is_null() {
                    self.after_failed(&after, format!("{} failed on chain: {}", what, v.get("err")));
                } else if conf == "confirmed" || conf == "finalized" {
                    // crowdfunded pieces report progress on their own card
                    if !matches!(&after, After::Attach(j) if j.crowd.is_some()) {
                        self.ok(format!("{} confirmed", what));
                    }
                    self.fetch_all_balances();
                    self.after_confirm(after, &sig);
                } else if host::now_ms() - since < 90_000.0 {
                    self.timer(2000, P::ConfirmTick { what, sig, since, after });
                    return false;
                } else {
                    self.after_failed(&after, format!("{} not confirmed after 90s — check {}", what, solana::short(&sig)));
                }
            }
            P::Run(op) => return self.run_async(op, ok, status, data),
            other => return self.more_async(other, ok, status, data),
        }
        true
    }

    fn more_async(&mut self, p: P, ok: bool, status: u32, data: Vec<u8>) -> bool {
        match p {
            P::RpcRoots | P::RpcMeta(..) | P::RpcSigs(..) | P::RpcTxs(..) | P::RpcChunk(..) => self.chain_async(p, ok, status, data),
            P::AttachCheck(_) | P::AttachSession(_) | P::AttachHash(_) | P::AttachSent(_) | P::TxView(..) | P::Files(_) => {
                self.attach_async(p, ok, status, data)
            }
            P::SaveCheck { .. } => self.save_async(p, ok, status, data),
            P::GitMeta(_) | P::GitRows(_) | P::GitTree(_) => self.git_async(p, ok, status, data),
            P::EmbedWasm(_) => self.embed_async(p, ok, status, data),
            P::Up(key, op) => self.up_async(key, op, ok, status, data),
            P::CrowdHash(..) | P::CrowdPiece(_) | P::CrowdDl(..) | P::CrowdDlNext => self.crowd_async(p, ok, status, data),
            other => self.account_async(other, ok, status, data),
        }
    }

    /// A transaction that something was waiting for failed or never landed.
    pub fn after_failed(&mut self, after: &After, msg: String) {
        match after {
            After::Attach(_) | After::AttachFunded(_) => self.attach_fail(msg),
            _ => self.err(msg),
        }
    }

    fn after_confirm(&mut self, after: After, sig: &str) {
        match after {
            After::Balances => {}
            After::Attach(job) => self.attach_confirmed(job, sig),
            After::StartRun(key) => self.start_run(&key),
            After::AttachFunded(job) => self.attach_retry(job),
        }
    }

    // ------------------------------------------------------------- wallet

    pub fn fetch_balance(&mut self, addr: &str) {
        if parse_pk(addr).is_none() {
            return;
        }
        self.balances.insert(addr.to_string(), Load::Loading);
        let params = json::parse(&format!("[\"{}\",{{\"commitment\":\"confirmed\"}}]", addr)).unwrap();
        self.rpc("getBalance", params, P::Balance(addr.to_string()));
    }

    // ---------------------------------------------------------- workspace

    pub fn new_key(&self) -> String {
        let mut r = [0u8; 6];
        host::random(&mut r);
        base58::encode(&r)
    }

    fn new_draft(&mut self) {
        let name = self.form.get("new-db").cloned().unwrap_or_default().trim().to_string();
        match self.create_draft(&name, true) {
            Ok(key) => {
                self.form.remove("new-db");
                self.ed.tab = "browse".into();
                host::set_hash(&format!("#/ws/{}/0", key));
            }
            Err(e) => self.err(e),
        }
    }

    /// A new database in the editor (with a first sheet if `starter`).
    pub fn create_draft(&mut self, name: &str, starter: bool) -> Result<String, String> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("Give the database a name".into());
        }
        if name.len() > iq::MAX_DB_ID_BYTES {
            return Err(format!("Database names are at most {} bytes (this one is {})", iq::MAX_DB_ID_BYTES, name.len()));
        }
        if self.drafts.iter().any(|d| d.name == name) {
            return Err(format!("You already have a database called \"{}\" here", name));
        }
        let key = self.new_key();
        let mut d = Draft::new(key.clone(), name.clone());
        if starter {
            // start with a sheet to type into, like a new spreadsheet
            d.tables.push(DraftTable::starter("sheet1", &["name", "notes"]));
        }
        // saved with the wallet you're signed in with
        d.wallet = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address());
        self.drafts.push(d);
        self.save_drafts();
        self.check_name(&key);
        Ok(key)
    }

    pub fn check_name(&mut self, key: &str) {
        let Some(i) = self.draft_idx(key) else { return };
        let name = self.drafts[i].name.clone();
        let pda = b58(&iq::db_root_pda(name.as_bytes()));
        self.name_checks.insert(key.to_string(), Load::Loading);
        let params = json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"commitment\":\"confirmed\"}}]", pda)).unwrap();
        self.rpc("getAccountInfo", params, P::NameCheck { draft: key.to_string(), name });
    }

    fn add_table(&mut self, key: &str) {
        let Some(i) = self.draft_idx(key) else { return };
        let name = self.form.get(&format!("tname:{}", key)).cloned().unwrap_or_default().trim().to_string();
        let cols_raw = self.form.get(&format!("tcols:{}", key)).cloned().unwrap_or_default();
        let open = self.form.get(&format!("topen:{}", key)).map(|v| v == "open").unwrap_or(false);
        if name.is_empty() || name.len() > 64 {
            self.err("Table names must be 1–64 bytes");
            return;
        }
        if self.drafts[i].tables.iter().any(|t| t.name == name) {
            self.err("There is already a table with that name");
            return;
        }
        let mut columns: Vec<String> = cols_raw.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
        columns.dedup();
        if columns.is_empty() {
            columns = vec!["name".into()];
        }
        let names: Vec<&str> = columns.iter().map(|c| c.as_str()).collect();
        let d = &mut self.drafts[i];
        let mut tb = DraftTable::starter(&name, &names);
        tb.open = open;
        d.tables.push(tb);
        d.sel = d.tables.len() - 1;
        d.page = 0;
        for k in ["tname", "tcols", "topen"] {
            self.form.remove(&format!("{}:{}", k, key));
        }
        self.save_drafts();
        self.ed.tab = "browse".into();
        let t = self.drafts[i].sel;
        host::set_hash(&format!("#/ws/{}/{}", key, t));
    }

    fn edit_table(&mut self, arg: &str, f: impl FnOnce(&mut Draft, usize)) {
        let mut it = arg.splitn(2, ':');
        let (k, t) = (it.next().unwrap_or(""), it.next().unwrap_or("0").parse::<usize>().unwrap_or(0));
        let Some(i) = self.draft_idx(k) else { return };
        if t >= self.drafts[i].tables.len() {
            return;
        }
        f(&mut self.drafts[i], t);
        let k = k.to_string();
        self.bump(&k, t);
        self.save_drafts();
    }

    fn edit_cell(&mut self, arg: &str, val: &str) {
        // key:table:row:col
        let p: Vec<&str> = arg.split(':').collect();
        if p.len() != 4 {
            return;
        }
        let (Some(i), Ok(t), Ok(r), Ok(c)) = (self.draft_idx(p[0]), p[1].parse::<usize>(), p[2].parse::<usize>(), p[3].parse::<usize>()) else { return };
        if p[3] == "id" {
            return;
        }
        {
            let Some(tb) = self.drafts[i].tables.get_mut(t) else { return };
            if c == usize::MAX {
                return;
            }
            let ncols = tb.columns.len();
            let Some(row) = tb.rows.get_mut(r) else { return };
            if row.sig.is_some() || c >= ncols {
                return;
            }
            row.vals.resize(ncols, Json::Null);
            row.vals[c] = ui::typed(val);
        }
        let k = p[0].to_string();
        self.bump(&k, t);
        self.save_drafts();
    }

    fn row_op(&mut self, arg: &str, op: &str) {
        let p: Vec<&str> = arg.split(':').collect();
        if p.len() != 3 {
            return;
        }
        let (Some(i), Ok(t), Ok(r)) = (self.draft_idx(p[0]), p[1].parse::<usize>(), p[2].parse::<usize>()) else { return };
        {
            let Some(tb) = self.drafts[i].tables.get_mut(t) else { return };
            if r >= tb.rows.len() {
                return;
            }
            match op {
                "del" if tb.rows[r].sig.is_none() => {
                    tb.rows.remove(r);
                }
                "tomb" => {
                    let mut vals = vec![Json::Null; tb.columns.len()];
                    vals[tb.id_col] = tb.rows[r].vals.get(tb.id_col).cloned().unwrap_or(Json::Null);
                    tb.rows.push(GhostRow { vals, deleted: true, sig: None });
                }
                _ => {}
            }
        }
        let k = p[0].to_string();
        self.bump(&k, t);
        self.save_drafts();
    }

    pub fn import_text(&mut self, arg: &str, text: &str) {
        let mut it = arg.splitn(2, ':');
        let (k, t) = (it.next().unwrap_or("").to_string(), it.next().unwrap_or("0").parse::<usize>().unwrap_or(0));
        let Some(i) = self.draft_idx(&k) else { return };
        let Some(tb) = self.drafts[i].tables.get_mut(t) else { return };
        let text = text.trim_start_matches('\u{feff}');
        let (header, rows): (Vec<String>, Vec<Vec<Json>>) = if text.trim_start().starts_with('[') {
            match json::parse(text) {
                Ok(v) => {
                    let mut header: Vec<String> = vec![];
                    for o in v.arr() {
                        for (key, _) in o.obj() {
                            if !header.contains(key) {
                                header.push(key.clone());
                            }
                        }
                    }
                    let rows = v.arr().iter().map(|o| header.iter().map(|h| o.get(h).clone()).collect()).collect();
                    (header, rows)
                }
                Err(e) => {
                    self.err(format!("JSON import failed: {}", e));
                    return;
                }
            }
        } else {
            let mut csv = ui::parse_csv(text);
            if csv.is_empty() {
                self.err("Nothing to import");
                return;
            }
            let header: Vec<String> = csv.remove(0).into_iter().map(|h| h.trim().to_string()).collect();
            let rows = csv.into_iter().map(|r| r.iter().map(|c| ui::typed(c)).collect()).collect();
            (header, rows)
        };
        // A fresh table adopts the file's columns; the first column is the
        // ID when its values are all there and all different.
        if tb.created.is_none() && tb.rows.is_empty() {
            let mut cols: Vec<String> = header.iter().filter(|h| !h.is_empty()).cloned().collect();
            cols.dedup();
            if !cols.is_empty() && cols != tb.columns {
                let first_ok = {
                    let mut seen = std::collections::HashSet::new();
                    !rows.is_empty() && rows.iter().all(|r| r.first().map(|v| !v.is_null() && seen.insert(v.cell_text())).unwrap_or(false))
                };
                let name = tb.name.clone();
                let (open, title) = (tb.open, tb.title.clone());
                *tb = if first_ok {
                    DraftTable::plain(&name, cols, 0)
                } else {
                    let names: Vec<&str> = cols.iter().map(|c| c.as_str()).collect();
                    DraftTable::starter(&name, &names)
                };
                tb.open = open;
                tb.title = title;
            }
        }
        let tb = tb.clone();
        let map: Vec<Option<usize>> = header.iter().map(|h| tb.col(h)).collect();
        let unknown: Vec<&String> = header.iter().zip(&map).filter(|(_, m)| m.is_none()).map(|(h, _)| h).collect();
        let n = rows.len();
        let cur = self.sheet_rows(&k, t);
        let by_id: std::collections::HashMap<String, crate::sheet::SRow> = cur
            .iter()
            .filter(|r| r.state != crate::sheet::RowState::Deleted)
            .map(|r| (r.vals.get(tb.id_col).map(|v| v.cell_text()).unwrap_or_default(), r.clone()))
            .collect();
        let nc = tb.columns.len();
        let mut changes = vec![];
        let mut updated = 0;
        for r in rows {
            let mut vals = vec![Json::Null; nc];
            let mut given = vec![false; nc];
            for (j, v) in r.into_iter().enumerate() {
                if let Some(Some(c)) = map.get(j) {
                    vals[*c] = v;
                    given[*c] = true;
                }
            }
            let id = vals[tb.id_col].cell_text();
            match by_id.get(&id).filter(|_| !id.is_empty()) {
                Some(row) => {
                    let mut nv = row.vals.clone();
                    nv.resize(nc, Json::Null);
                    for c in 0..nc {
                        if given[c] {
                            nv[c] = vals[c].clone();
                        }
                    }
                    changes.push(crate::constraints::Change::Update { row: row.clone(), vals: nv, set: given });
                    updated += 1;
                }
                None => changes.push(crate::constraints::Change::Insert { vals, given }),
            }
        }
        let fk = self.fk_checks();
        if let Err(e) = self.apply_changes(&k, t, changes, &crate::constraints::Opts { strict: false, fk_checks: fk }) {
            self.err(format!("Nothing was imported: {}", e));
            return;
        }
        let mut msg = format!("Imported {} rows (not saved yet)", n);
        if updated > 0 {
            msg = format!("Imported {} rows — {} updated existing rows with the same ID (not saved yet)", n, updated);
        }
        if !unknown.is_empty() {
            msg.push_str(&format!(". Ignored columns not in the table: {}", unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
        }
        self.form.remove(&format!("csv:{}", arg));
        self.save_drafts();
        self.ok(msg);
    }

    /// Open an on-chain IQT table in the editor (its records load from the chain).
    fn draft_from_table(&mut self, edit_key: &str) {
        let Some(tv) = self.table.as_ref() else { return };
        let (root, pda) = (tv.root.clone(), tv.pda.clone());
        let label = tv.label.clone();
        let db = tv.db_id.clone();
        self.open_in_editor(db, root, Some((pda, label)), if edit_key.is_empty() { None } else { Some(edit_key.to_string()) });
    }

    /// Open a database (and optionally one of its tables) in the editor.
    pub fn open_in_editor(&mut self, db: Option<String>, root: Option<String>, table: Option<(String, Option<String>)>, record: Option<String>) {
        let info = root.as_ref().and_then(|r| self.dbroots.ready().and_then(|rs| rs.iter().find(|d| &d.pda == r)).cloned());
        let Some(db) = db.or_else(|| info.as_ref().and_then(|i| i.id.clone())) else {
            self.err("This database's name isn't readable (it was created from a hash), so it can't be opened in the editor.");
            return;
        };
        let key = match self.drafts.iter().position(|d| d.name == db) {
            Some(i) => self.drafts[i].key.clone(),
            None => {
                let key = self.new_key();
                let mut d = Draft::new(key.clone(), db.clone());
                d.root_sig = Some("existing".into());
                d.lock_creators = false;
                // edit with the owner's wallet if it's in this account
                if let (Some(i), Some(a)) = (&info, &self.account) {
                    if a.find(&i.creator).is_some() {
                        d.wallet = Some(i.creator.clone());
                    }
                }
                self.drafts.push(d);
                key
            }
        };
        let i = self.draft_idx(&key).unwrap();
        let mut names: Vec<String> = info.as_ref().map(|r| r.tables.iter().filter_map(|t| t.label.clone()).collect()).unwrap_or_default();
        let wanted = table.as_ref().and_then(|(pda, label)| {
            label
                .clone()
                .filter(|l| !l.starts_with('#'))
                .or_else(|| info.as_ref().and_then(|r| r.tables.iter().find(|t| &t.pda == pda).and_then(|t| t.label.clone())))
        });
        if table.is_some() && wanted.is_none() {
            self.err("This table's name isn't readable, so it can't be opened in the editor.");
            return;
        }
        if let Some(w) = &wanted {
            if !names.contains(w) {
                names.push(w.clone());
            }
        }
        for n in names {
            if !self.drafts[i].tables.iter().any(|t| t.name == n) {
                self.drafts[i].tables.push(DraftTable { open: true, created: Some("existing".into()), ..DraftTable::plain(&n, vec![], 0) });
            }
        }
        let t = wanted.and_then(|w| self.drafts[i].tables.iter().position(|t| t.name == w)).unwrap_or(0);
        self.drafts[i].sel = t;
        self.save_drafts();
        self.ed.tab = "browse".into();
        self.ed.table = (String::new(), 0);
        if let Some(r) = record {
            self.pending_filter = Some(r);
        }
        self.keep_toast = true;
        host::set_hash(&format!("#/ws/{}/{}", key, t));
    }

    fn export_view(&mut self, fmt: &str) {
        let Some(tv) = self.table.as_ref() else { return };
        let (cols, rows) = crate::views::view_rows(tv);
        let name = tv.label.clone().unwrap_or_else(|| tv.pda.clone());
        if fmt == "json" {
            host::download(&format!("{}.json", safe_name(&name)), "application/json", crate::records::json_rows(&cols, &rows).to_string().as_bytes());
        } else {
            host::download(&format!("{}.csv", safe_name(&name)), "text/csv", crate::records::csv(&cols, &rows).as_bytes());
        }
    }

    /// Pack plan for a draft table (cached by edit revision).
    pub fn plan_for(&mut self, key: &str, t: usize, cap: usize) -> &Result<Vec<pack::PlannedPack>, String> {
        let rev = *self.revs.get(&(key.to_string(), t)).unwrap_or(&0);
        let ck = (key.to_string(), t);
        let checkpoint = self.draft_idx(key).and_then(|i| self.drafts[i].tables.get(t)).map(|tb| tb.checkpoint && !tb.dropped).unwrap_or(false);
        let base = if checkpoint { self.sheet_base(key, t).0.len() } else { 0 };
        let stale = self.plans.get(&ck).map(|p| p.rev != rev || p.cap != cap || p.base != base).unwrap_or(true);
        if stale {
            let chunk = if cap <= iq::INLINE_CAP_LEGACY { iq::CHUNK_SIZE_LEGACY } else { iq::CHUNK_SIZE_V1 };
            let live = if checkpoint { self.sheet_rows(key, t) } else { vec![] };
            let result = match self.draft_idx(key).and_then(|i| self.drafts[i].tables.get(t)).filter(|tb| !tb.dropped) {
                Some(tb) => {
                    let mut tb = tb.clone();
                    tb.fix_meta();
                    let schema = pack::Schema { cols: tb.col_keys(), id: tb.id_col };
                    let nc = tb.columns.len();
                    // a checkpoint rewrites every live row; otherwise only the unsaved ones go
                    let (recs, ghosts): (Vec<pack::Record>, Vec<usize>) = if checkpoint {
                        (
                            live.iter()
                                .filter(|r| r.state != crate::sheet::RowState::Deleted)
                                .map(|r| {
                                    let mut vals = r.vals.clone();
                                    vals.resize(nc, Json::Null);
                                    pack::Record { vals, deleted: false }
                                })
                                .collect(),
                            vec![],
                        )
                    } else {
                        tb.rows
                            .iter()
                            .enumerate()
                            .filter(|(_, r)| r.sig.is_none())
                            .map(|(i, r)| {
                                let mut vals = r.vals.clone();
                                vals.resize(nc, Json::Null);
                                (pack::Record { vals, deleted: r.deleted }, i)
                            })
                            .unzip()
                    };
                    if recs.is_empty() {
                        Ok(vec![])
                    } else if let Some(bad) = recs.iter().position(|r| r.key(&schema).is_empty()) {
                        Err(format!("Row {} has no value in the ID column \"{}\"", bad + 1, tb.columns.get(tb.id_col).cloned().unwrap_or_default()))
                    } else {
                        pack::plan_best(&schema, &recs, cap, chunk, tb.compress).map(|mut ps| {
                            for p in ps.iter_mut() {
                                p.ghosts = ghosts.get(p.first..p.first + p.count).map(|g| g.to_vec()).unwrap_or_default();
                            }
                            ps
                        })
                    }
                }
                None => Ok(vec![]),
            };
            self.plans.insert(ck.clone(), PlanCache { rev, cap, base, result });
        }
        &self.plans.get(&ck).unwrap().result
    }

    pub fn inline_cap(&self) -> usize {
        match self.settings.tx_format {
            state::TxFormat::Legacy => iq::INLINE_CAP_LEGACY,
            _ => {
                if self.run.as_ref().map(|r| r.legacy).unwrap_or(false) {
                    iq::INLINE_CAP_LEGACY
                } else {
                    iq::INLINE_CAP_V1
                }
            }
        }
    }
}

pub const ROWS_PER_PAGE: usize = 50;
/// IQ's gateway for devnet (the one the IQ SDKs switch to on devnet).
pub const DEV_GATEWAY: &str = "https://dev-gateway.iqlabs.dev";
pub const MAIN_GATEWAY: &str = "https://gateway.iqlabs.dev";

pub use crate::records::decode_row;

/// Decode a row read for table view `t`. Until the official wallet is known,
/// everyone is held to the limits for other writers (anyone can write to a
/// table, and a shared link opens the table before its database is known);
/// `App::redecode` reads those rows again once it is.
pub fn decode_for(r: &Json, official: Option<&str>, t: &mut TableView) -> Option<Result<pack::SourcePack, String>> {
    if official.is_none() {
        t.blind = true;
    }
    crate::records::decode_row_for(r, Some(official.unwrap_or("")), &mut t.others_raw)
}

pub fn fetch_err(ok: bool, status: u32, body: &str) -> String {
    if !ok {
        return format!("network error: {}", body);
    }
    if status == 429 {
        return "rate limited (429) — the RPC is throttling requests; wait a moment, or set your own RPC URL in Settings".into();
    }
    if status == 403 {
        return format!(
            "forbidden (403) — this endpoint refuses browser requests; set a different RPC URL in Settings. {}",
            body.chars().take(120).collect::<String>()
        );
    }
    net::gateway_error(status, body)
}

pub fn safe_name(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

pub fn pct_encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{:02X}", b));
        }
    }
    o
}

pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The official wallet is now known: decode again what was decoded without it.
pub fn redecode(t: &mut TableView) {
    if !t.blind {
        return;
    }
    let Some(c) = t.creator.clone() else { return };
    t.blind = false;
    t.others_raw = 0;
    let mut used = 0;
    t.decoded = t.rows.iter().map(|r| crate::records::decode_row_for(r, Some(&c), &mut used)).collect();
    t.others_raw = used;
}
