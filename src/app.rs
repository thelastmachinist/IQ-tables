//! Application state, event dispatch and the asynchronous flows (explorer
//! loads, wallet, funding). The inscription pipeline lives in `inscribe.rs`
//! and rendering in `views.rs`.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::crypto::{base58, base64_encode};
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
    Table { root: Option<String>, pda: String },
    Search(String),
    Workspace,
    Draft(String),
    Settings,
    About,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Who {
    Official,
    Unofficial,
    All,
}

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
    Connect,
    Sign { draft: String, first: Option<Vec<u8>> },
    NameCheck { draft: String, name: String },
    Balance(String),
    FundHash { draft: String, lamports: u64 },
    FundSent { draft: String },
    WithdrawHash { draft: String },
    WithdrawSent { draft: String },
    Confirm { what: String, sig: String, addr: String, since: f64 },
    ConfirmTick { what: String, sig: String, addr: String, since: f64 },
    Run(RunOp),
    Ignore,
}

pub struct PlanCache {
    pub rev: u64,
    pub cap: usize,
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
    pub wallets: Vec<(String, String)>,
    pub owner: Option<(String, String)>,
    pub keys: HashMap<String, Keypair>,
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
    pub connect_menu: bool,
    /// Set when an action navigates and its message should survive the route change.
    pub keep_toast: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = RefCell::new(None);
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
        let mut app = App::new();
        app.refresh_wallets();
        *a.borrow_mut() = Some(app);
    });
}

#[no_mangle]
pub unsafe extern "C" fn on_event(kp: *mut u8, kl: usize, ap: *mut u8, al: usize, gp: *mut u8, gl: usize, vp: *mut u8, vl: usize) {
    let kind = owned_str(kp, kl);
    let action = owned_str(ap, al);
    let arg = owned_str(gp, gl);
    let val = owned_str(vp, vl);
    with_app(|app| app.event(&kind, &action, &arg, &val));
}

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

impl App {
    pub fn new() -> Self {
        let settings = host::storage_get(K_SETTINGS)
            .and_then(|s| json::parse(&s).ok())
            .map(|v| Settings::from_json(&v))
            .unwrap_or_default();
        let drafts = host::storage_get(K_DRAFTS)
            .and_then(|s| json::parse(&s).ok())
            .map(|v| state::drafts_from_json(&v))
            .unwrap_or_default();
        App {
            route: Route::Databases,
            settings,
            drafts,
            dbroots: Load::None,
            db_filter: String::new(),
            search_q: String::new(),
            search: Load::None,
            table: None,
            wallets: vec![],
            owner: None,
            keys: HashMap::new(),
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
            connect_menu: false,
            keep_toast: false,
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
        let url = format!("{}{}", self.settings.gateway.trim_end_matches('/'), path);
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

    pub fn refresh_wallets(&mut self) {
        let v = json::parse(&host::wallets()).unwrap_or(Json::Arr(vec![]));
        self.wallets = v.arr().iter().map(|w| (w.get("name").str_or("?"), w.get("icon").str_or(""))).collect();
    }

    pub fn render(&mut self) {
        let html = crate::views::render(self);
        host::render(&html);
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
        self.route = match parts.as_slice() {
            ["db", pda] => Route::Db(pda.to_string()),
            ["t", root, pda] => Route::Table { root: Some(root.to_string()), pda: pda.to_string() },
            ["t", pda] => Route::Table { root: None, pda: pda.to_string() },
            ["search", q] => Route::Search(pct_decode(q)),
            ["ws"] => Route::Workspace,
            ["ws", key] => Route::Draft(key.to_string()),
            ["settings"] => Route::Settings,
            ["about"] => Route::About,
            _ => Route::Databases,
        };
        match self.route.clone() {
            Route::Databases | Route::Db(_) => self.ensure_dbroots(),
            Route::Table { root, pda } => {
                self.ensure_dbroots();
                self.open_table(root, pda);
            }
            Route::Search(q) => {
                self.search_q = q.clone();
                self.search = Load::Loading;
                let path = format!("/search?q={}&limit=40", pct_encode(&q));
                self.get(&path, P::Search(q));
            }
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
            }
            _ => {}
        }
    }

    fn ensure_dbroots(&mut self) {
        if matches!(self.dbroots, Load::None | Load::Err(_)) {
            self.dbroots = Load::Loading;
            self.get("/dbroots", P::DbRoots);
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
        self.get(&format!("/table/{}/meta", pda), P::Meta(pda.clone(), gen));
        let q = if fresh { "&fresh=1" } else { "" };
        self.get(&format!("/table/{}/rows?limit=100{}", pda, q), P::Rows(pda.clone(), gen));
        let needs_root = self.table.as_ref().map(|t| t.creator.is_none()).unwrap_or(false);
        if needs_root {
            if let Some(r) = root {
                // not in the (30-minute cached) gateway list yet: ask the chain
                self.rpc("getAccountInfo", json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\"}}]", r)).unwrap(), P::RootInfo(pda, gen));
            }
        }
    }

    /// Fill creator / database name for the open table from the gateway list.
    fn attach_root_info(&mut self) {
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
            t.db_id = id;
            if !label.is_empty() {
                t.label = Some(label);
            }
        }
    }

    fn more_rows(&mut self) {
        let Some(t) = self.table.as_mut() else { return };
        if t.loading || t.done {
            return;
        }
        t.loading = true;
        let path = match &t.cursor {
            Some(c) => format!("/table/{}/rows?limit=100&before={}", t.pda, c),
            None => format!("/table/{}/rows?limit=100", t.pda),
        };
        let p = P::Rows(t.pda.clone(), t.gen);
        self.get(&path, p);
    }

    // ------------------------------------------------------------- events

    pub fn event(&mut self, kind: &str, action: &str, arg: &str, val: &str) -> bool {
        match (kind, action) {
            ("route", _) => self.route(val),
            ("wallets", _) => self.refresh_wallets(),
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
            // wallet
            (_, "connect-menu") => {
                self.refresh_wallets();
                self.connect_menu = !self.connect_menu;
            }
            (_, "connect") => {
                self.connect_menu = false;
                let id = self.nid();
                self.pending.insert(id, P::Connect);
                host::wallet_connect(id, arg);
            }
            (_, "disconnect") => {
                host::wallet_disconnect();
                self.owner = None;
                self.keys.clear();
                self.connect_menu = false;
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
            (_, "unlock") => self.unlock(arg),
            (_, "import-key") => self.import_key(arg),
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
            (_, "fund") => self.fund(arg),
            (_, "withdraw") => self.withdraw(arg),
            (_, "add-table") => self.add_table(arg),
            (_, "sel-table") => {
                let mut it = arg.splitn(2, ':');
                let (k, t) = (it.next().unwrap_or(""), it.next().unwrap_or("0").parse().unwrap_or(0));
                if let Some(i) = self.draft_idx(k) {
                    self.drafts[i].sel = t;
                    self.drafts[i].page = 0;
                }
            }
            (_, "del-table") => self.edit_table(arg, |d, t| {
                if d.tables[t].created.is_none() {
                    d.tables.remove(t);
                    d.sel = 0;
                }
            }),
            (_, "table-open") => {
                let v = val == "true";
                self.edit_table(arg, move |d, t| d.tables[t].open = v)
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
            (_, "inscribe") => self.start_run(arg),
            (_, "run-stop") => {
                if let Some(r) = self.run.as_mut() {
                    r.stop = true;
                }
            }
            (_, "run-resume") => self.resume_run(),
            (_, "run-close") => {
                if self.run.as_ref().map(|r| !r.busy()).unwrap_or(true) {
                    self.run = None;
                }
            }
            // settings
            (_, "set") => self.set_setting(arg, val),
            (_, "export-ws") => {
                let data = state::drafts_to_json(&self.drafts).to_string();
                host::download("iq-tables-workspace.json", "application/json", data.as_bytes());
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
                Err(e) => self.err(format!("Not a workspace file: {}", e)),
            },
            _ => return false,
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
                if val == "devnet" && self.settings.rpc.contains("mainnet") {
                    self.settings.rpc = "https://api.devnet.solana.com".into();
                } else if val == "mainnet" && self.settings.rpc.contains("devnet") {
                    self.settings.rpc = "https://api.mainnet-beta.solana.com".into();
                }
                self.balances.clear();
                self.name_checks.clear();
            }
            "tx" => {
                self.settings.tx_format = match val {
                    "v1" => state::TxFormat::V1,
                    "legacy" => state::TxFormat::Legacy,
                    _ => state::TxFormat::Auto,
                };
                self.plans.clear();
            }
            "simulate" => self.settings.simulate = val == "true",
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
                if let Some(t) = self.table.as_mut().filter(|t| t.pda == pda && t.gen == gen) {
                    t.meta = if http_ok {
                        json::parse(&text()).map(Load::Ready).unwrap_or_else(Load::Err)
                    } else {
                        Load::Err(fetch_err(ok, status, &text()))
                    };
                }
            }
            P::RootInfo(pda, gen) => {
                let creator = if http_ok {
                    net::rpc_result(&text())
                        .ok()
                        .and_then(|r| net::account_data(r.get("value")))
                        .and_then(|d| iq::decode_db_root(&d))
                } else {
                    None
                };
                if let (Some(t), Some(root)) = (self.table.as_mut().filter(|t| t.pda == pda && t.gen == gen), creator) {
                    t.creator = Some(b58(&root.creator));
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
                if let Some(t) = self.table.as_mut().filter(|t| t.pda == pda && t.gen == gen) {
                    t.loading = false;
                    if http_ok {
                        match json::parse(&text()) {
                            Ok(v) => {
                                let rows = v.get("rows").arr().to_vec();
                                for r in &rows {
                                    t.decoded.push(decode_row(r));
                                }
                                let n = rows.len();
                                t.rows.extend(rows);
                                t.cursor = v.get("nextCursor").str().map(String::from);
                                if n == 0 || t.cursor.is_none() {
                                    t.done = true;
                                    t.load_all = false;
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
                if again {
                    self.more_rows();
                }
            }
            P::Connect => {
                if ok {
                    let v = json::parse(&text()).unwrap_or(Json::Null);
                    let addr = v.get("address").str_or("");
                    if parse_pk(&addr).is_some() {
                        self.owner = Some((v.get("name").str_or("Wallet"), addr.clone()));
                        self.fetch_balance(&addr);
                        self.ok(format!("Connected {}", solana::short(&addr)));
                    } else {
                        self.err("Wallet returned no Solana account");
                    }
                } else {
                    self.err(format!("Wallet connection failed: {}", text()));
                }
            }
            P::Sign { draft, first } => self.on_signature(&draft, first, ok, data),
            P::NameCheck { draft, name } => {
                let cur = self.draft_idx(&draft).map(|i| self.drafts[i].name.clone());
                if cur.as_deref() == Some(&name) {
                    let v = if http_ok {
                        match net::rpc_result(&text()) {
                            Ok(r) => Load::Ready(
                                net::account_data(r.get("value")).and_then(|d| iq::decode_db_root(&d)).map(|d| b58(&d.creator)),
                            ),
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
            P::FundHash { draft, lamports } => {
                let hash = if http_ok { net::rpc_result(&text()).ok() } else { None };
                let bh = hash.as_ref().and_then(|r| r.get("value").get("blockhash").str().and_then(base58::decode32));
                match (bh, self.owner.clone(), self.draft_idx(&draft).and_then(|i| self.drafts[i].wallet.clone())) {
                    (Some(bh), Some((_, owner)), Some(dest)) => {
                        let (Some(from), Some(to)) = (parse_pk(&owner), parse_pk(&dest)) else { return true };
                        let msg = solana::compile(&from, &[solana::system_transfer(&from, &to, lamports)], bh);
                        let tx = solana::legacy_unsigned(&msg);
                        let id = self.nid();
                        self.pending.insert(id, P::FundSent { draft });
                        let chain = self.settings.chain();
                        host::wallet_sign_and_send(id, &tx, chain);
                        self.ok("Approve the transfer in your wallet…");
                    }
                    _ => self.err(format!("Could not prepare the transfer: {}", fetch_err(ok, status, &text()))),
                }
            }
            P::FundSent { draft } => {
                if ok {
                    let sig = base58::encode(&data);
                    let addr = self.draft_idx(&draft).and_then(|i| self.drafts[i].wallet.clone()).unwrap_or_default();
                    self.ok(format!("Transfer sent: {}", solana::short(&sig)));
                    let now = host::now_ms();
                    self.timer(1500, P::ConfirmTick { what: "Funding".into(), sig, addr, since: now });
                } else {
                    self.err(format!("Transfer not sent: {}", text()));
                }
            }
            P::WithdrawHash { draft } => self.withdraw_send(&draft, ok, status, &text()),
            P::WithdrawSent { draft } => {
                let addr = self.draft_idx(&draft).and_then(|i| self.drafts[i].wallet.clone()).unwrap_or_default();
                match if http_ok { net::rpc_result(&text()) } else { Err(fetch_err(ok, status, &text())) } {
                    Ok(r) => {
                        let sig = r.str_or("");
                        self.ok(format!("Withdrawal sent: {}", solana::short(&sig)));
                        let now = host::now_ms();
                        self.timer(1500, P::ConfirmTick { what: "Withdrawal".into(), sig, addr, since: now });
                    }
                    Err(e) => self.err(format!("Withdrawal failed: {}", e)),
                }
            }
            P::ConfirmTick { what, sig, addr, since } => {
                let params = json::parse(&format!("[[\"{}\"]]", sig)).unwrap();
                self.rpc("getSignatureStatuses", params, P::Confirm { what, sig, addr, since });
                return false;
            }
            P::Confirm { what, sig, addr, since } => {
                let st = if http_ok { net::rpc_result(&text()).ok() } else { None };
                let v = st.as_ref().map(|r| r.get("value").idx(0).clone()).unwrap_or(Json::Null);
                let conf = v.get("confirmationStatus").str_or("");
                if !v.get("err").is_null() {
                    self.err(format!("{} failed on chain: {}", what, v.get("err")));
                } else if conf == "confirmed" || conf == "finalized" {
                    self.ok(format!("{} confirmed", what));
                    self.fetch_balance(&addr);
                    if let Some((_, o)) = self.owner.clone() {
                        self.fetch_balance(&o);
                    }
                } else if host::now_ms() - since < 90_000.0 {
                    self.timer(2000, P::ConfirmTick { what, sig, addr, since });
                    return false;
                } else {
                    self.err(format!("{} not confirmed after 90s — check {}", what, solana::short(&sig)));
                }
            }
            P::Run(op) => return self.run_async(op, ok, status, data),
        }
        true
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

    fn unlock(&mut self, key: &str) {
        let Some(i) = self.draft_idx(key) else { return };
        if self.owner.is_none() {
            self.err("Connect your wallet first — it is your account and the key to your database wallets.");
            return;
        }
        let msg = state::derivation_message(&self.drafts[i].name);
        let id = self.nid();
        self.pending.insert(id, P::Sign { draft: key.to_string(), first: None });
        host::wallet_sign_message(id, msg.as_bytes());
        self.ok("Sign the unlock message in your wallet (it does not move funds)…");
    }

    fn on_signature(&mut self, key: &str, first: Option<Vec<u8>>, ok: bool, sig: Vec<u8>) {
        let Some(i) = self.draft_idx(key) else { return };
        if !ok || sig.len() != 64 {
            self.err(format!("Signature not received: {}", String::from_utf8_lossy(&sig)));
            return;
        }
        let owner = self.owner.clone().map(|o| o.1).unwrap_or_default();
        let known = self.drafts[i].wallet.clone();
        // First unlock of a new database: sign twice to prove the wallet's
        // signatures are deterministic, otherwise the key could never be
        // recovered.
        if known.is_none() && first.is_none() {
            let msg = state::derivation_message(&self.drafts[i].name);
            let id = self.nid();
            self.pending.insert(id, P::Sign { draft: key.to_string(), first: Some(sig) });
            host::wallet_sign_message(id, msg.as_bytes());
            self.ok("Sign once more — this checks your wallet always produces the same key.");
            return;
        }
        if let Some(f) = first {
            if f != sig {
                self.err("Your wallet produced two different signatures for the same message, so a database wallet can't be reliably derived from it. Import a key instead.");
                return;
            }
        }
        let kp = Keypair::from_seed(state::derive_seed(&sig));
        let addr = b58(&kp.pubkey);
        if let Some(k) = known {
            if k != addr && self.drafts[i].wallet_kind == "derived" {
                self.err(format!(
                    "This wallet derives {} but the draft belongs to {}. Connect the wallet that created it ({}).",
                    solana::short(&addr),
                    solana::short(&k),
                    self.drafts[i].owner.as_deref().map(solana::short).unwrap_or_default()
                ));
                return;
            }
        }
        let d = &mut self.drafts[i];
        d.wallet = Some(addr.clone());
        d.owner = Some(owner);
        d.wallet_kind = "derived".into();
        self.keys.insert(key.to_string(), kp);
        self.save_drafts();
        self.fetch_balance(&addr);
        self.ok(format!("Database wallet unlocked: {}", solana::short(&addr)));
    }

    fn import_key(&mut self, key: &str) {
        let Some(i) = self.draft_idx(key) else { return };
        let secret = self.form.remove(&format!("key:{}", key)).unwrap_or_default();
        match Keypair::from_secret_b58(&secret) {
            Some(kp) => {
                let addr = b58(&kp.pubkey);
                if let Some(w) = &self.drafts[i].wallet {
                    if w != &addr {
                        self.err(format!("That key is for {}, not this database's wallet {}", solana::short(&addr), solana::short(w)));
                        return;
                    }
                }
                self.drafts[i].wallet = Some(addr.clone());
                if self.drafts[i].owner.is_none() {
                    self.drafts[i].wallet_kind = "imported".into();
                }
                self.keys.insert(key.to_string(), kp);
                self.save_drafts();
                self.fetch_balance(&addr);
                self.ok("Key imported for this session (it is never stored).");
            }
            None => self.err("Not a valid base58 secret key (64-byte Phantom export or 32-byte seed)."),
        }
    }

    fn fund(&mut self, key: &str) {
        let amount = self.form.get(&format!("fund:{}", key)).cloned().unwrap_or_default();
        let Some(lamports) = ui::parse_sol(&amount).filter(|&l| l > 0) else {
            self.err("Enter an amount in SOL, e.g. 0.1");
            return;
        };
        if self.owner.is_none() {
            self.err("Connect your wallet first");
            return;
        }
        let params = json::parse("[{\"commitment\":\"confirmed\"}]").unwrap();
        self.rpc("getLatestBlockhash", params, P::FundHash { draft: key.to_string(), lamports });
    }

    fn withdraw(&mut self, key: &str) {
        if !self.keys.contains_key(key) {
            self.err("Unlock the database wallet first");
            return;
        }
        let params = json::parse("[{\"commitment\":\"confirmed\"}]").unwrap();
        self.rpc("getLatestBlockhash", params, P::WithdrawHash { draft: key.to_string() });
    }

    fn withdraw_send(&mut self, key: &str, ok: bool, status: u32, text: &str) {
        let Some(kp) = self.keys.get(key).cloned() else { return };
        let dest = self.form.get(&format!("wd:{}", key)).cloned().filter(|s| !s.trim().is_empty()).or(self.owner.clone().map(|o| o.1));
        let Some(dest) = dest.and_then(|d| parse_pk(&d)) else {
            self.err("Enter a destination address (or connect your wallet)");
            return;
        };
        let bal = self.balances.get(&b58(&kp.pubkey)).and_then(|b| b.ready().copied()).unwrap_or(0);
        if bal <= iq::TX_FEE {
            self.err("Nothing to withdraw");
            return;
        }
        let bh = if ok && (200..300).contains(&status) {
            net::rpc_result(text).ok().and_then(|r| r.get("value").get("blockhash").str().and_then(base58::decode32))
        } else {
            None
        };
        let Some(bh) = bh else {
            self.err(format!("Could not get a blockhash: {}", fetch_err(ok, status, text)));
            return;
        };
        let msg = solana::compile(&kp.pubkey, &[solana::system_transfer(&kp.pubkey, &dest, bal - iq::TX_FEE)], bh);
        let (raw, _) = solana::legacy_signed(&msg, &kp.seed);
        let params = json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\"}}]", base64_encode(&raw))).unwrap();
        self.rpc("sendTransaction", params, P::WithdrawSent { draft: key.to_string() });
    }

    // ---------------------------------------------------------- workspace

    pub fn new_key(&self) -> String {
        let mut r = [0u8; 6];
        host::random(&mut r);
        base58::encode(&r)
    }

    fn new_draft(&mut self) {
        let name = self.form.get("new-db").cloned().unwrap_or_default().trim().to_string();
        if name.is_empty() {
            self.err("Give the database a name");
            return;
        }
        if name.len() > iq::MAX_DB_ID_BYTES {
            self.err(format!("Database names are at most {} bytes (this one is {})", iq::MAX_DB_ID_BYTES, name.len()));
            return;
        }
        let key = self.new_key();
        self.drafts.push(Draft::new(key.clone(), name));
        self.form.remove("new-db");
        self.save_drafts();
        host::set_hash(&format!("#/ws/{}", key));
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
            columns = vec!["id".into()];
        }
        let d = &mut self.drafts[i];
        d.tables.push(DraftTable {
            name: name.clone(),
            title: name,
            columns,
            id_col: 0,
            open,
            compress: true,
            created: None,
            rows: vec![],
        });
        d.sel = d.tables.len() - 1;
        d.page = 0;
        for k in ["tname", "tcols", "topen"] {
            self.form.remove(&format!("{}:{}", k, key));
        }
        self.save_drafts();
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

    fn import_text(&mut self, arg: &str, text: &str) {
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
        // A fresh table adopts the file's columns.
        if tb.created.is_none() && tb.rows.is_empty() {
            let mut cols: Vec<String> = header.iter().filter(|h| !h.is_empty()).cloned().collect();
            cols.dedup();
            if !cols.is_empty() {
                tb.columns = cols;
                tb.id_col = 0;
            }
        }
        let map: Vec<Option<usize>> = header.iter().map(|h| tb.columns.iter().position(|c| c == h)).collect();
        let unknown: Vec<&String> = header.iter().zip(&map).filter(|(_, m)| m.is_none()).map(|(h, _)| h).collect();
        let n = rows.len();
        for r in rows {
            let mut vals = vec![Json::Null; tb.columns.len()];
            for (j, v) in r.into_iter().enumerate() {
                if let Some(Some(c)) = map.get(j) {
                    vals[*c] = v;
                }
            }
            tb.rows.push(GhostRow { vals, deleted: false, sig: None });
        }
        let msg = if unknown.is_empty() {
            format!("Imported {} ghost rows", n)
        } else {
            format!("Imported {} ghost rows (ignored columns not in the table: {})", n, unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
        };
        self.form.remove(&format!("csv:{}", arg));
        self.bump(&k, t);
        self.save_drafts();
        self.ok(msg);
    }

    /// Open an on-chain IQT table in the workspace to draft changes to it.
    fn draft_from_table(&mut self, edit_key: &str) {
        let Some(tv) = self.table.as_ref() else { return };
        let Some(db) = tv.db_id.clone() else {
            self.err("This database's name isn't readable (it was created from a hash), so it can't be opened in the workspace.");
            return;
        };
        let label = tv.label.clone().unwrap_or_default();
        if label.is_empty() || label.starts_with('#') {
            self.err("This table's name isn't readable, so it can't be opened in the workspace.");
            return;
        }
        let merged = crate::views::merged_records(tv, Who::Official);
        let Some(schema) = tv.decoded.iter().rev().filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok())).map(|p| p.schema.clone()).last() else {
            self.err("No IQ Tables packs found in this table yet.");
            return;
        };
        let edit_row: Option<Vec<Json>> = if edit_key.is_empty() {
            None
        } else {
            merged.iter().find(|m| m.key == edit_key).map(|m| {
                schema.cols.iter().map(|c| m.vals.iter().find(|(k, _)| k == c).map(|(_, v)| v.clone()).unwrap_or(Json::Null)).collect()
            })
        };
        let key = match self.drafts.iter().position(|d| d.name == db) {
            Some(i) => self.drafts[i].key.clone(),
            None => {
                let key = self.new_key();
                let mut d = Draft::new(key.clone(), db.clone());
                d.root_sig = Some("existing".into());
                d.lock_creators = false;
                self.drafts.push(d);
                key
            }
        };
        let i = self.draft_idx(&key).unwrap();
        let t = match self.drafts[i].tables.iter().position(|t| t.name == label) {
            Some(t) => t,
            None => {
                self.drafts[i].tables.push(DraftTable {
                    name: label.clone(),
                    title: label.clone(),
                    columns: schema.cols.clone(),
                    id_col: schema.id,
                    open: true,
                    compress: true,
                    created: Some("existing".into()),
                    rows: vec![],
                });
                self.drafts[i].tables.len() - 1
            }
        };
        if let Some(vals) = edit_row {
            let tb = &mut self.drafts[i].tables[t];
            let mut v2 = vec![Json::Null; tb.columns.len()];
            for (ci, c) in schema.cols.iter().enumerate() {
                if let Some(pos) = tb.columns.iter().position(|x| x == c) {
                    v2[pos] = vals[ci].clone();
                }
            }
            tb.rows.push(GhostRow { vals: v2, deleted: false, sig: None });
        }
        self.drafts[i].sel = t;
        self.drafts[i].page = 0;
        self.bump(&key, t);
        self.save_drafts();
        self.ok(if edit_key.is_empty() { "Opened in your workspace" } else { "Record copied into a ghost row — edit it, then inscribe" });
        self.keep_toast = true;
        host::set_hash(&format!("#/ws/{}", key));
    }

    fn export_view(&mut self, fmt: &str) {
        let Some(tv) = self.table.as_ref() else { return };
        let (cols, rows) = crate::views::view_rows(tv);
        let name = tv.label.clone().unwrap_or_else(|| tv.pda.clone());
        if fmt == "json" {
            let arr: Vec<Json> = rows
                .iter()
                .map(|r| Json::Obj(cols.iter().cloned().zip(r.vals.iter().cloned()).collect()))
                .collect();
            host::download(&format!("{}.json", safe_name(&name)), "application/json", Json::Arr(arr).to_string().as_bytes());
        } else {
            let mut out = cols.iter().map(|c| ui::csv_cell(c)).collect::<Vec<_>>().join(",");
            out.push('\n');
            for r in &rows {
                out.push_str(&r.vals.iter().map(|v| ui::csv_cell(&v.cell_text())).collect::<Vec<_>>().join(","));
                out.push('\n');
            }
            host::download(&format!("{}.csv", safe_name(&name)), "text/csv", out.as_bytes());
        }
    }

    /// Pack plan for a draft table (cached by edit revision).
    pub fn plan_for(&mut self, key: &str, t: usize, cap: usize) -> &Result<Vec<pack::PlannedPack>, String> {
        let rev = *self.revs.get(&(key.to_string(), t)).unwrap_or(&0);
        let ck = (key.to_string(), t);
        let stale = self.plans.get(&ck).map(|p| p.rev != rev || p.cap != cap).unwrap_or(true);
        if stale {
            let result = match self.draft_idx(key).and_then(|i| self.drafts[i].tables.get(t)) {
                Some(tb) => {
                    let schema = pack::Schema { cols: tb.columns.clone(), id: tb.id_col };
                    let recs: Vec<pack::Record> = tb
                        .rows
                        .iter()
                        .filter(|r| r.sig.is_none())
                        .map(|r| {
                            let mut vals = r.vals.clone();
                            vals.resize(tb.columns.len(), Json::Null);
                            pack::Record { vals, deleted: r.deleted }
                        })
                        .collect();
                    if recs.is_empty() {
                        Ok(vec![])
                    } else if let Some(bad) = recs.iter().position(|r| r.key(&schema).is_empty()) {
                        Err(format!("Row {} has no value in the id column \"{}\"", bad + 1, schema.id_col()))
                    } else {
                        pack::plan(&schema, &recs, cap, tb.compress)
                    }
                }
                None => Ok(vec![]),
            };
            self.plans.insert(ck.clone(), PlanCache { rev, cap, result });
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

pub fn decode_row(r: &Json) -> Option<Result<pack::SourcePack, String>> {
    let p = r.get("p").str()?;
    if !p.starts_with(pack::MAGIC) {
        return None;
    }
    Some(pack::decode_payload(p).map(|(schema, recs)| pack::SourcePack {
        tx: r.get("__txSignature").str_or(""),
        signer: r.get("__signer").str_or(""),
        time: r.get("__blockTime").f64().map(|f| f as i64),
        schema,
        recs,
    }))
}

pub fn fetch_err(ok: bool, status: u32, body: &str) -> String {
    if !ok {
        return format!("network error: {}", body);
    }
    if status == 429 {
        return "rate limited (429) — the public RPC is heavily throttled; set your own RPC URL in Settings".into();
    }
    if status == 403 {
        return format!("forbidden (403) — this endpoint refuses browser requests; set a different RPC URL in Settings. {}", body.chars().take(120).collect::<String>());
    }
    net::gateway_error(status, body)
}

fn safe_name(s: &str) -> String {
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
