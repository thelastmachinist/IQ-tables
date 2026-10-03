//! The app without a page, for the command line and AI tools
//! (`node iqt-loader.mjs sql | write | mcp`). The loader gives this same
//! WebAssembly the host functions the web page gives it (fetch, timers,
//! storage kept in memory; no rendering) and drives it with JSON messages
//! through the `cli` export, so SQL, table rules and saving work exactly as
//! they do on the website.
//!
//! * `{"op":"setup","rpc","gateway","devnet","solana","key","keyName"}` → `{"ok":true,"wallet"}`
//! * `{"op":"open","db"}` → `{"wait":true}` until the database has been looked
//!   up on chain, then `{"ready":true,"db","exists","owner","wallet","tables"}`
//!   (a database that doesn't exist yet is made when changes are saved)
//! * `{"op":"sql","text"}` → `{}`; the results arrive with `status`
//! * `{"op":"import","table","text","create","open"}` → `{"wait":true}` while
//!   saved tables' records load, then `{"ok":true}`: rows added, or updated
//!   when their id exists, under the table's rules (not saved yet)
//! * `{"op":"pending"}` → what saving would write, and roughly what it costs
//! * `{"op":"save","max"}` → `{"ok":true}`; progress and the outcome come with
//!   `status`. `max` (lamports) caps what the save may spend: a step that would
//!   pass it isn't sent, and the save stops there
//! * `{"op":"stop"}` → the save stops before its next step
//! * `{"op":"status"}` → `{"busy","log","out","run"}` (log and out are new
//!   since the last status)
//!
//! Errors are `{"error":"…"}`.

use std::cell::RefCell;

use crate::app::{fetch_err, After, App, Load, Route, P};
use crate::inscribe::RunState;
use crate::json::{self, Json};
use crate::sql_exec::Out;
use crate::state::{DraftTable, TxFormat, RPC_DEVNET, RPC_MAINNET};
use crate::{iq, net, sheet};

/// What the command line is doing with this app.
#[derive(Default)]
pub struct Cli {
    /// Messages the app showed (successes and errors), oldest first.
    pub log: Vec<(bool, String)>,
    /// The open database (its draft key).
    pub db: Option<String>,
    /// A database being looked up on chain: (name, outcome once known).
    pub lookup: Option<(String, Option<Result<(), String>>)>,
    /// Wallets whose IQ accounts are being checked.
    pub users: usize,
    /// Run notes already reported.
    pub seen: usize,
    /// A COMMIT statement ran.
    pub commit: bool,
    /// What the current save may spend in all (lamports).
    pub cap: Option<u64>,
    /// A table the current import made: (database key, name).
    pub made: Option<(String, String)>,
    /// The signed-in wallet already has its IQ accounts (no one-time setup).
    pub user_ready: bool,
    /// Rent for growing that wallet's pre-upgrade IQ accounts before a v1 write.
    pub grow: u64,
    /// The cluster takes v1 transactions (else packs are legacy-sized); None
    /// until checked.
    pub v1: Option<bool>,
}

thread_local! {
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// # Safety
/// `p` must come from `alloc(len)`; this call takes it back.
#[no_mangle]
pub unsafe extern "C" fn cli(p: *mut u8, len: usize) -> *const u8 {
    let input = Vec::from_raw_parts(p, len, len.max(1));
    let msg = String::from_utf8_lossy(&input).into_owned();
    drop(input);
    let reply = crate::app::app_call(|app| app.cli_call(&msg)).unwrap_or_else(|| err("the app isn't running (call start first)"));
    OUT.with(|o| {
        let mut o = o.borrow_mut();
        *o = reply.to_string().into_bytes();
        o.as_ptr()
    })
}

#[no_mangle]
pub extern "C" fn cli_len() -> usize {
    OUT.with(|o| o.borrow().len())
}

fn err(m: &str) -> Json {
    json::obj(vec![("error", json::s(m))])
}

fn wait() -> Json {
    json::obj(vec![("wait", Json::Bool(true))])
}

fn ok() -> Json {
    json::obj(vec![("ok", Json::Bool(true))])
}

/// A result set as CSV (like every other export), JSON objects, and the
/// cells' text (numbers stay exact: no float parsing on the way out).
fn out_json(o: &Out) -> Json {
    use crate::records::csv_value;
    match o {
        Out::Rows { title, cols, rows, note } => {
            let mut csv = cols.iter().map(|c| csv_value(&Json::Str(c.clone()))).collect::<Vec<_>>().join(",");
            csv.push('\n');
            for r in rows {
                csv.push_str(&r.iter().map(csv_value).collect::<Vec<_>>().join(","));
                csv.push('\n');
            }
            // keys stay unique (a join's two `id`s become id and id_2)
            let mut keys: Vec<String> = vec![];
            for c in cols {
                let mut k = c.clone();
                let mut n = 2;
                while keys.contains(&k) {
                    k = format!("{}_{}", c, n);
                    n += 1;
                }
                keys.push(k);
            }
            let objects = Json::Arr(rows.iter().map(|r| Json::Obj(keys.iter().cloned().zip(r.iter().cloned()).collect())).collect());
            json::obj(vec![
                ("title", json::s(title)),
                ("cols", Json::Arr(cols.iter().map(|c| json::s(c)).collect())),
                ("cells", Json::Arr(rows.iter().map(|r| Json::Arr(r.iter().map(|v| json::s(&v.cell_text())).collect())).collect())),
                ("csv", json::s(&csv)),
                ("json", json::s(&objects.to_string())),
                ("count", json::n(rows.len())),
                ("note", json::s(note)),
            ])
        }
        Out::Msg(good, m) => json::obj(vec![("ok", Json::Bool(*good)), ("message", json::s(m))]),
    }
}

impl App {
    pub fn cli_call(&mut self, msg: &str) -> Json {
        let m = match json::parse(msg) {
            Ok(m) => m,
            Err(e) => return err(&format!("bad message: {}", e)),
        };
        if self.cli.is_none() && m.get("op").str() != Some("setup") {
            return err("send setup first");
        }
        match m.get("op").str() {
            Some("setup") => self.cli_setup(&m),
            Some("open") => self.cli_open(m.get("db").str().unwrap_or("").trim()),
            Some("sql") => self.cli_sql(m.get("text").str().unwrap_or("")),
            Some("import") => self.cli_import(&m),
            Some("pending") => self.cli_pending(),
            Some("save") => self.cli_save(m.get("max").u64()),
            Some("stop") => {
                if let Some(r) = self.run.as_mut().filter(|r| r.busy()) {
                    r.stop = true;
                }
                ok()
            }
            Some("status") => self.cli_status(),
            _ => err("unknown op"),
        }
    }

    fn cli_setup(&mut self, m: &Json) -> Json {
        self.cli = Some(Cli::default());
        // nothing is drawn; the cheapest page keeps renders cheap
        self.route = Route::About;
        let devnet = m.get("devnet").bool().unwrap_or(false);
        let s = &mut self.settings;
        s.cluster = if devnet { "devnet" } else { "mainnet" }.into();
        s.rpc = m.get("rpc").str().filter(|r| !r.is_empty()).unwrap_or(if devnet { RPC_DEVNET } else { RPC_MAINNET }).to_string();
        if let Some(g) = m.get("gateway").str().filter(|g| !g.is_empty()) {
            s.gateway = g.trim_end_matches('/').to_string();
        }
        s.source = if m.get("solana").bool().unwrap_or(false) { "rpc" } else { "gateway" }.into();
        s.tx_format = TxFormat::Auto;
        s.simulate = true;
        s.notify_gateway = true;
        if let Some(k) = m.get("key").str() {
            let name = m.get("keyName").str().unwrap_or("key file").to_string();
            self.account_event("file", "drop-file", &name, k);
            if self.account.is_none() {
                let why = self.cli.as_ref().and_then(|c| c.log.iter().rev().find(|(good, _)| !good)).map(|(_, m)| m.clone());
                return err(&why.unwrap_or_else(|| format!("{} isn't a key this can sign with (use a Solana key file)", name)));
            }
        }
        let wallet = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address());
        if let Some(w) = &wallet {
            self.cli_check_user(w);
        }
        json::obj(vec![
            ("ok", Json::Bool(true)),
            ("wallet", wallet.map(|w| json::s(&w)).unwrap_or(Json::Null)),
            ("rpc", json::s(&self.settings.rpc)),
            ("gateway", json::s(&self.gateway_url())),
        ])
    }

    fn cli_state(&mut self) -> &mut Cli {
        self.cli.get_or_insert_with(Cli::default)
    }

    fn cli_key(&self) -> Option<String> {
        self.cli.as_ref().and_then(|c| c.db.clone()).filter(|k| self.draft_idx(k).is_some())
    }

    fn cli_open(&mut self, db: &str) -> Json {
        if db.is_empty() {
            return err("name a database (--db <name>)");
        }
        if db.len() > iq::MAX_DB_ID_BYTES {
            return err(&format!("database names are at most {} bytes", iq::MAX_DB_ID_BYTES));
        }
        if let Some(i) = self.drafts.iter().position(|d| d.name == db) {
            let key = self.drafts[i].key.clone();
            self.cli_state().db = Some(key.clone());
            if self.cli_busy() {
                return wait();
            }
            return self.cli_db_info(&key);
        }
        let lookup = self.cli_state().lookup.clone();
        match lookup {
            Some((name, Some(Err(e)))) if name == db => {
                self.cli_state().lookup = None;
                err(&e)
            }
            Some((name, _)) if name == db => wait(),
            _ => {
                self.cli_state().lookup = Some((db.to_string(), None));
                let pda = crate::solana::b58(&iq::db_root_pda(db.as_bytes()));
                let params = json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"commitment\":\"confirmed\"}}]", pda)).unwrap();
                self.rpc("getAccountInfo", params, P::CliRoot(db.to_string()));
                wait()
            }
        }
    }

    fn cli_db_info(&mut self, key: &str) -> Json {
        let Some(i) = self.draft_idx(key) else { return err("no such database") };
        let d = &self.drafts[i];
        let tables: Vec<Json> = d
            .tables
            .iter()
            .filter(|t| !t.dropped && !t.is_system())
            .map(|t| json::obj(vec![("name", json::s(&t.title)), ("saved", Json::Bool(t.created.is_some())), ("open", Json::Bool(t.open))]))
            .collect();
        let owner = self.creator_of(key);
        json::obj(vec![
            ("ready", Json::Bool(true)),
            ("db", json::s(&d.name)),
            ("exists", Json::Bool(d.root_sig.is_some())),
            ("owner", owner.map(|o| json::s(&o)).unwrap_or(Json::Null)),
            ("wallet", d.wallet.clone().map(|w| json::s(&w)).unwrap_or(Json::Null)),
            ("tables", Json::Arr(tables)),
        ])
    }

    /// The database's lookup on chain came back.
    pub fn cli_async(&mut self, p: P, ok: bool, status: u32, data: Vec<u8>) -> bool {
        let text = String::from_utf8_lossy(&data).into_owned();
        let http_ok = ok && (200..300).contains(&status);
        match p {
            P::CliRoot(db) => {
                let result = if http_ok { net::rpc_result(&text) } else { Err(fetch_err(ok, status, &text)) };
                let outcome = match result {
                    Err(e) => Err(format!("Couldn't look up \"{}\" on chain: {}", db, e)),
                    Ok(r) => {
                        let pda = crate::solana::b58(&iq::db_root_pda(db.as_bytes()));
                        match net::account_data(r.get("value")).and_then(|d| iq::decode_db_root(&d)) {
                            Some(root) => {
                                let info = net::dbroot_info(&pda, &root);
                                match &mut self.dbroots {
                                    Load::Ready(list) => {
                                        list.retain(|x| x.pda != pda);
                                        list.push(info);
                                    }
                                    other => *other = Load::Ready(vec![info]),
                                }
                                self.open_in_editor(Some(db.clone()), Some(pda), None, None);
                                match self.drafts.iter().position(|d| d.name == db) {
                                    Some(i) => {
                                        let key = self.drafts[i].key.clone();
                                        self.check_name(&key);
                                        Ok(key)
                                    }
                                    None => Err(format!("Couldn't open \"{}\"", db)),
                                }
                            }
                            // not on chain: a new database, made when it's saved
                            None => self.create_draft(&db, false),
                        }
                    }
                };
                match outcome {
                    Ok(key) => {
                        let c = self.cli_state();
                        c.db = Some(key);
                        c.lookup = Some((db, Some(Ok(()))));
                    }
                    Err(e) => self.cli_state().lookup = Some((db, Some(Err(e)))),
                }
            }
            P::CliUser(_) => {
                let v = if http_ok { net::rpc_result(&text).ok() } else { None };
                let acc = |i: usize| v.as_ref().map(|v| v.get("value").idx(i).clone()).unwrap_or(Json::Null);
                let len = |i: usize| net::account_data(&acc(i)).map(|d| d.len() as u64);
                let v1 = iq::v1_active(&acc(2));
                // pre-upgrade accounts are grown (rent) before the first v1 write
                let mut grow = 0;
                if v1 {
                    if let Some(n) = len(0).filter(|n| *n < iq::USER_INVENTORY_SPACE) {
                        grow += iq::rent_exempt(iq::USER_INVENTORY_SPACE as usize).saturating_sub(iq::rent_exempt(n as usize)) + iq::TX_FEE;
                    }
                    if let Some(n) = len(1).filter(|n| *n < iq::CODE_ACCOUNT_SPACE) {
                        grow += iq::rent_exempt(iq::CODE_ACCOUNT_SPACE as usize).saturating_sub(iq::rent_exempt(n as usize));
                    }
                }
                let c = self.cli_state();
                c.users = c.users.saturating_sub(1);
                c.user_ready = !acc(0).is_null();
                c.v1 = Some(v1);
                c.grow = grow;
            }
            _ => {}
        }
        false
    }

    /// What saving with this wallet costs beyond the writes: its one-time IQ
    /// account setup, or growing pre-upgrade accounts; and whether the
    /// cluster takes v1 transactions (bigger packs).
    fn cli_check_user(&mut self, wallet: &str) {
        let Some(pk) = crate::solana::parse_pk(wallet) else { return };
        let addrs = [iq::user_inventory_pda(&pk), iq::code_account_pda(&pk), crate::solana::pk(iq::TX_V1_FEATURE_GATE_STR)];
        let list: Vec<Json> = addrs.iter().map(|a| json::s(&crate::solana::b58(a))).collect();
        let params = Json::Arr(vec![Json::Arr(list), json::obj(vec![("encoding", json::s("base64")), ("commitment", json::s("confirmed"))])]);
        self.cli_state().users += 1;
        self.rpc("getMultipleAccounts", params, P::CliUser(wallet.to_string()));
    }

    /// A save step about to be sent, costing `cost`: refused when it would
    /// take the save past its cap.
    pub fn cli_over_cap(&self, cost: u64) -> Option<String> {
        let cap = self.cli.as_ref()?.cap?;
        let r = self.run.as_ref()?;
        let spent = r.spent().unwrap_or(0);
        (spent + cost > cap).then(|| {
            format!(
                "Stopped before step {} of {}: it would cost {}, and this save may spend {} in all ({} so far). Nothing more was sent.",
                r.i + 1,
                r.steps.len(),
                crate::ui::sol(cost),
                crate::ui::sol(cap),
                crate::ui::sol(spent)
            )
        })
    }

    fn cli_sql(&mut self, text: &str) -> Json {
        let Some(key) = self.cli_key() else { return err("open a database first") };
        if text.trim().is_empty() {
            return err("no SQL to run");
        }
        self.cli_state().commit = false;
        self.ed.sql_out = self.run_sql(&key, text);
        ok()
    }

    fn cli_import(&mut self, m: &Json) -> Json {
        let Some(key) = self.cli_key() else { return err("open a database first") };
        let name = m.get("table").str().unwrap_or("").trim().to_string();
        let text = m.get("text").str().unwrap_or("");
        if text.trim().is_empty() {
            return err("no rows to write");
        }
        let di = self.draft_idx(&key).unwrap();
        let made = self.cli.as_ref().and_then(|c| c.made.clone()) == Some((key.clone(), name.clone()));
        let t = match self.tbl(&key, &name) {
            Ok(t) => t,
            Err(e) => {
                if !m.get("create").bool().unwrap_or(false) {
                    return err(&format!("{} (add --create to make it)", e));
                }
                if let Err(e) = crate::ddl::valid_name("table", &name) {
                    return err(&e);
                }
                let mut tb = DraftTable::starter(&name, &[]);
                tb.open = m.get("open").bool().unwrap_or(false);
                self.drafts[di].tables.push(tb);
                let t = self.drafts[di].tables.len() - 1;
                self.bump(&key, t);
                self.cli_state().made = Some((key.clone(), name.clone()));
                t
            }
        };
        // only a table made here takes the file's columns
        let made = made || self.cli.as_ref().and_then(|c| c.made.clone()) == Some((key.clone(), name.clone()));
        // saved tables' records come first: rows with an existing id update
        // theirs, and keys and relations are checked against what's saved
        let mut loading = false;
        for j in 0..self.drafts[di].tables.len() {
            if self.drafts[di].tables[j].created.is_none() || self.drafts[di].tables[j].dropped {
                continue;
            }
            self.ensure_base(&key, j);
            match self.sheet_base(&key, j).1 {
                crate::editor::BaseState::Loading => loading = true,
                crate::editor::BaseState::Err(e) => return err(&format!("Couldn't read {}: {}", self.drafts[di].tables[j].title, e)),
                _ => {}
            }
        }
        if loading {
            return wait();
        }
        let before = self.cli_state().log.len();
        // a file's rows are complete: required columns are checked now
        self.import_rows(&format!("{}:{}", key, t), text, made, true);
        self.cli_state().made = None;
        let failed = self.cli.as_ref().map(|c| c.log[before..].iter().any(|(good, _)| !good)).unwrap_or(false);
        if failed {
            // a table made for this import, that nothing went into, isn't kept
            if made && self.drafts[di].tables.get(t).map(|tb| tb.created.is_none() && tb.rows.is_empty()).unwrap_or(false) {
                self.drafts[di].tables.remove(t);
                self.table_removed(&key, t);
            }
            let why = self.cli.as_ref().and_then(|c| c.log[before..].iter().rev().find(|(good, _)| !good)).map(|(_, m)| m.clone());
            return err(&why.unwrap_or_else(|| "Nothing was imported".into()));
        }
        ok()
    }

    fn cli_pending(&mut self) -> Json {
        let Some(key) = self.cli_key() else { return err("open a database first") };
        let di = self.draft_idx(&key).unwrap();
        let n = self.drafts[di].tables.len();
        let wallet = self.drafts[di].wallet.clone();
        let mut tables = vec![];
        let mut changes = 0usize;
        for t in 0..n {
            let tb = self.drafts[di].tables[t].clone();
            let (new, changed, deleted) = if tb.dropped { (0, 0, 0) } else { sheet::pending(&self.sheet_rows(&key, t)) };
            let create = tb.created.is_none() && !tb.dropped;
            let structure = tb.schema_changed() && !create;
            let other = tb.meta_changed(wallet.as_deref()) || tb.dropped || tb.checkpoint;
            let k = new + changed + deleted + structure as usize + other as usize + create as usize;
            if k == 0 {
                continue;
            }
            changes += k;
            tables.push(json::obj(vec![
                ("name", json::s(&tb.title)),
                ("create", Json::Bool(create)),
                ("drop", Json::Bool(tb.dropped)),
                ("structure", Json::Bool(structure)),
                ("inserted", json::n(new)),
                ("updated", json::n(changed)),
                ("deleted", json::n(deleted)),
            ]));
        }
        let create_db = self.drafts[di].root_sig.is_none() && changes > 0;
        let main = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address());
        let (ready, grow) = self.cli.as_ref().map(|c| (c.user_ready, c.grow)).unwrap_or((false, 0));
        let mine = main.is_some() && (self.drafts[di].wallet.is_none() || self.drafts[di].wallet == main);
        if ready && mine {
            self.drafts[di].user_init_sig.get_or_insert_with(|| "existing".into());
        }
        let (lamports, writes) = if changes > 0 { self.save_estimate(&key) } else { (0, 0) };
        let lamports = lamports + if changes > 0 && ready && mine { grow } else { 0 };
        json::obj(vec![
            ("db", json::s(&self.drafts[di].name)),
            ("changes", json::n(changes)),
            ("createDb", Json::Bool(create_db)),
            ("tables", Json::Arr(tables)),
            ("lamports", json::n(lamports)),
            ("writes", json::n(writes)),
        ])
    }

    fn cli_save(&mut self, max: Option<u64>) -> Json {
        let Some(key) = self.cli_key() else { return err("open a database first") };
        if self.account.is_none() {
            return err("saving signs transactions: give a key (--key <file>)");
        }
        let before = self.cli_state().log.len();
        self.cli_state().cap = max;
        self.ed.tab = "save".into();
        // the balance is checked first; the run (and its notes) starts after
        self.save(&key);
        self.cli_state().seen = 0;
        match self.cli.as_ref().and_then(|c| c.log[before..].iter().rev().find(|(good, _)| !good)).map(|(_, m)| m.clone()) {
            Some(why) => err(&why),
            None => ok(),
        }
    }

    fn cli_busy(&self) -> bool {
        let c = self.cli.as_ref();
        self.ed.sql_wait.is_some()
            || self.busy_note.is_some()
            || self.run.as_ref().map(|r| r.busy()).unwrap_or(false)
            || self.uploads.values().any(|b| !b.idle())
            || self.bases.values().any(|t| t.loading)
            || self.name_checks.values().any(|l| matches!(l, Load::Loading))
            // a save waiting on its balance check, or on SOL moving to the database's wallet
            || self.pending.values().any(|p| {
                matches!(
                    p,
                    P::SaveCheck { .. }
                        | P::TransferHash { after: After::StartRun(_), .. }
                        | P::TransferSent(After::StartRun(_))
                        | P::ConfirmTick { after: After::StartRun(_), .. }
                        | P::Confirm { after: After::StartRun(_), .. }
                )
            })
            || c.map(|c| c.users > 0 || matches!(c.lookup, Some((_, None)))).unwrap_or(false)
    }

    fn cli_status(&mut self) -> Json {
        let busy = self.cli_busy();
        let out: Vec<Json> = if self.ed.sql_wait.is_none() { std::mem::take(&mut self.ed.sql_out).iter().map(out_json).collect() } else { vec![] };
        let commit = self.cli.as_ref().map(|c| c.commit).unwrap_or(false);
        let log: Vec<Json> = self
            .cli
            .as_mut()
            .map(|c| std::mem::take(&mut c.log))
            .unwrap_or_default()
            .into_iter()
            .map(|(good, m)| json::obj(vec![("ok", Json::Bool(good)), ("message", json::s(&m))]))
            .collect();
        let run = match self.run.as_ref() {
            Some(r) => {
                let seen = self.cli.as_ref().map(|c| c.seen).unwrap_or(0);
                let fresh = r.noted.saturating_sub(seen).min(r.log.len());
                let notes: Vec<Json> =
                    r.log[r.log.len() - fresh..].iter().map(|(good, m)| json::obj(vec![("ok", Json::Bool(*good)), ("message", json::s(m))])).collect();
                let (state, msg) = match &r.state {
                    RunState::Preparing => ("working", String::new()),
                    RunState::Working(m) => ("working", m.clone()),
                    RunState::Paused(m) => ("paused", m.clone()),
                    RunState::Done => ("done", String::new()),
                    RunState::Failed(m) => ("failed", m.clone()),
                };
                let spent = r.spent();
                let noted = r.noted;
                self.cli_state().seen = noted;
                json::obj(vec![
                    ("state", json::s(state)),
                    ("message", json::s(&msg)),
                    ("notes", Json::Arr(notes)),
                    ("spent", spent.map(json::n).unwrap_or(Json::Null)),
                ])
            }
            None => Json::Null,
        };
        json::obj(vec![("busy", Json::Bool(busy)), ("log", Json::Arr(log)), ("out", Json::Arr(out)), ("commit", Json::Bool(commit)), ("run", run)])
    }
}
