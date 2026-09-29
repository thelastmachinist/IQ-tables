//! The inscription pipeline: turn a draft database into on-chain state with
//! the database wallet as signer. Each step is simulated before it is sent
//! (so program errors and the real cost show up before any SOL moves), sent,
//! and polled until confirmed. Progress is saved after every step, so a
//! closed tab resumes where it left off; packs are content-addressed, so a
//! retried pack can never duplicate records.

use crate::app::{fetch_err, App, P};
use crate::crypto::{base58, base64_encode};
use crate::host;
use crate::iq;
use crate::json::{self, Json};
use crate::net;
use crate::pack;
use crate::solana::{self, b58, Keypair, Pubkey};
use crate::state::TxFormat;
use crate::ui;

#[derive(Clone, Debug)]
pub enum StepKind {
    Root,
    Table(usize),
    UserInit,
    /// Grow accounts made by the pre-upgrade program so v1 writes fit.
    Grow(Vec<(Pubkey, u64)>),
    Pack { t: usize, rows: Vec<usize>, payload: String, pack_id: String, count: usize },
    /// A table-structure record (types, keys, renames; TRUNCATE / DROP).
    Schema { t: usize, payload: String, doc: String },
    /// Rename a table or change who may write to it (grow the account first if needed).
    /// Rename / change writers; the table's own columns, ID column and
    /// ext keys are kept as the chain has them.
    UpdateTable { t: usize, realloc: Option<u64>, cols: Vec<String>, id_col: String, ext: Vec<String> },
    /// Rewrite the database's table list (a dropped table comes off it).
    TableList { seeds: Vec<Vec<u8>>, realloc: Option<u64> },
}

#[derive(Clone, Debug)]
pub struct Step {
    pub kind: StepKind,
    pub sig: Option<String>,
    pub cost: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RunState {
    Preparing,
    Working(String),
    Paused(String),
    Done,
    Failed(String),
}

pub enum RunOp {
    Prep,
    Balance,
    RootData,
    Blockhash,
    Simulate,
    Send,
    Status,
    Tick,
}

pub struct Run {
    pub draft: String,
    pub kp: Keypair,
    pub legacy: bool,
    pub auto: bool,
    pub steps: Vec<Step>,
    pub i: usize,
    pub state: RunState,
    pub log: Vec<(bool, String)>,
    pub start_balance: Option<u64>,
    pub balance: Option<u64>,
    pub seq: u64,
    pub iq_ata: Option<Pubkey>,
    pub root_creator: Option<Pubkey>,
    pub root_data: Option<Vec<u8>>,
    pub attempt: u32,
    pub sig: Option<String>,
    pub raw: Vec<u8>,
    pub sent_at: f64,
    pub stop: bool,
    pub contributor: bool,
    /// Dropped tables whose record is written: leave the editor when done.
    pub dropped_done: Vec<String>,
}

impl Run {
    pub fn busy(&self) -> bool {
        matches!(self.state, RunState::Preparing | RunState::Working(_))
    }
    fn note(&mut self, ok: bool, m: impl Into<String>) {
        self.log.push((ok, m.into()));
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }
    pub fn spent(&self) -> Option<u64> {
        Some(self.start_balance?.saturating_sub(self.balance?))
    }
    pub fn describe(&self, s: &Step, tables: &[crate::state::DraftTable]) -> String {
        match &s.kind {
            StepKind::Root => "Create database (DbRoot) and lock table creation".into(),
            StepKind::Table(t) => format!("Create table \"{}\"", tables.get(*t).map(|x| x.name.as_str()).unwrap_or("?")),
            StepKind::UserInit => "One-time IQ account setup for the database wallet".into(),
            StepKind::Grow(_) => "Enlarge the wallet's IQ accounts for 4 KB (v1) writes".into(),
            StepKind::Pack { t, count, pack_id, .. } => format!(
                "Save {} rows to \"{}\" (pack {})",
                count,
                tables.get(*t).map(|x| x.title.as_str()).unwrap_or("?"),
                pack_id
            ),
            StepKind::Schema { t, .. } => {
                let tb = tables.get(*t);
                let what = match tb {
                    Some(x) if x.dropped => "Delete",
                    Some(x) if x.clear => "Empty and update the structure of",
                    _ => "Save the structure of",
                };
                format!("{} \"{}\"", what, tb.map(|x| x.title.as_str()).unwrap_or("?"))
            }
            StepKind::UpdateTable { t, .. } => format!("Update the name / writers of \"{}\"", tables.get(*t).map(|x| x.title.as_str()).unwrap_or("?")),
            StepKind::TableList { .. } => "Update the database's list of tables".into(),
        }
    }
}

/// Minimum balance to attempt a step (simulation catches the exact amount).
fn guard(k: &StepKind) -> u64 {
    match k {
        StepKind::Root => iq::DB_ROOT_COST_ESTIMATE,
        StepKind::Table(_) => iq::TABLE_COST_ESTIMATE,
        StepKind::UserInit => iq::USER_INIT_RENT_ESTIMATE,
        StepKind::Grow(_) => 50_000_000,
        StepKind::Pack { .. } | StepKind::Schema { .. } => iq::FEE_DIRECT_WRITE + iq::TX_FEE,
        StepKind::UpdateTable { realloc, .. } | StepKind::TableList { realloc, .. } => iq::TX_FEE + realloc.map(|r| iq::rent_exempt(r as usize) / 4).unwrap_or(0),
    }
}

impl App {
    pub fn start_run(&mut self, key: &str) {
        if self.run.as_ref().map(|r| r.busy()).unwrap_or(false) {
            self.err("An inscription is already running");
            return;
        }
        let Some(i) = self.draft_idx(key) else { return };
        let Some(kp) = self.draft_keypair(key) else {
            self.err(if self.account.is_none() {
                "Log in first (drop your account file anywhere on the page)"
            } else {
                "Pick a wallet from your account for this database first"
            });
            return;
        };
        if self.drafts[i].tables.is_empty() {
            self.err("Add a table first");
            return;
        }
        let auto = self.settings.tx_format == TxFormat::Auto;
        let legacy = self.settings.tx_format == TxFormat::Legacy;
        self.run = Some(Run {
            draft: key.to_string(),
            kp: kp.clone(),
            legacy,
            auto,
            steps: vec![],
            i: 0,
            state: RunState::Preparing,
            log: vec![],
            start_balance: None,
            balance: None,
            seq: 0,
            iq_ata: None,
            root_creator: None,
            root_data: None,
            attempt: 0,
            sig: None,
            raw: vec![],
            sent_at: 0.0,
            stop: false,
            contributor: false,
            dropped_done: vec![],
        });
        self.prep();
    }

    fn prep(&mut self) {
        let Some(r) = self.run.as_ref() else { return };
        let Some(i) = self.draft_idx(&r.draft) else { return };
        let d = &self.drafts[i];
        let user = r.kp.pubkey;
        let root = iq::db_root_pda(d.name.as_bytes());
        let mint = solana::pk(iq::IQ_MINT_STR);
        let mut addrs: Vec<Pubkey> = vec![
            root,
            iq::user_inventory_pda(&user),
            iq::user_state_pda(&user),
            iq::ata(&user, &mint, &solana::pk(iq::TOKEN_PROGRAM_STR)),
            iq::ata(&user, &mint, &solana::pk(iq::TOKEN_2022_STR)),
            user,
            iq::code_account_pda(&user),
            solana::pk(iq::TX_V1_FEATURE_GATE_STR),
        ];
        for t in &d.tables {
            addrs.push(iq::table_pda(&root, &iq::seed_bytes(&t.name)));
        }
        let list = addrs.iter().map(|a| json::s(&b58(a))).collect();
        let params = Json::Arr(vec![
            Json::Arr(list),
            json::obj(vec![("encoding", json::s("base64")), ("commitment", json::s("confirmed"))]),
        ]);
        if let Some(r) = self.run.as_mut() {
            r.state = RunState::Preparing;
            r.note(true, "Reading on-chain state…");
        }
        self.rpc("getMultipleAccounts", params, P::Run(RunOp::Prep));
    }

    pub fn resume_run(&mut self) {
        let Some(r) = self.run.as_mut() else { return };
        if r.busy() {
            return;
        }
        r.stop = false;
        r.attempt = 0;
        if matches!(r.state, RunState::Failed(_)) || r.steps.is_empty() {
            // rebuild the plan from what is actually on chain now
            r.steps.clear();
            r.i = 0;
            self.prep();
        } else {
            r.state = RunState::Working("Resuming".into());
            self.next_step();
        }
    }

    fn fail(&mut self, m: impl Into<String>) {
        let m = m.into();
        if let Some(r) = self.run.as_mut() {
            r.note(false, m.clone());
            r.state = RunState::Failed(m);
        }
    }

    fn pause(&mut self, m: impl Into<String>) {
        let m = m.into();
        if let Some(r) = self.run.as_mut() {
            r.note(false, m.clone());
            r.state = RunState::Paused(m);
        }
    }

    pub fn run_async(&mut self, op: RunOp, ok: bool, status: u32, data: Vec<u8>) -> bool {
        if self.run.is_none() {
            return false;
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        let res = if matches!(op, RunOp::Tick) {
            Ok(Json::Null)
        } else if ok && (200..300).contains(&status) {
            net::rpc_result(&text)
        } else {
            Err(fetch_err(ok, status, &text))
        };
        match op {
            RunOp::Prep => match res {
                Ok(v) => self.after_prep(&v),
                Err(e) => self.fail(format!("Could not read chain state: {}", e)),
            },
            RunOp::Balance => match res {
                Ok(v) => {
                    let bal = v.get("value").u64().unwrap_or(0);
                    let wallet = b58(&self.run.as_ref().unwrap().kp.pubkey);
                    self.balances.insert(wallet, crate::app::Load::Ready(bal));
                    let r = self.run.as_mut().unwrap();
                    if r.start_balance.is_none() {
                        r.start_balance = Some(bal);
                    }
                    r.balance = Some(bal);
                    let need = guard(&r.steps[r.i].kind) + iq::RENT_FLOOR;
                    if bal < need {
                        let msg = format!(
                            "Paused: the database wallet has {} but this step needs at least {}. Fund it (or receive a donation) and resume.",
                            ui::sol(bal),
                            ui::sol(need)
                        );
                        self.pause(msg);
                    } else if matches!(r.steps[r.i].kind, StepKind::Table(_)) {
                        let root = b58(&iq::db_root_pda(self.draft_name().as_bytes()));
                        let params = json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"commitment\":\"confirmed\"}}]", root)).unwrap();
                        self.rpc("getAccountInfo", params, P::Run(RunOp::RootData));
                    } else {
                        self.request_blockhash();
                    }
                }
                Err(e) => self.fail(format!("Balance check failed: {}", e)),
            },
            RunOp::RootData => match res.map(|v| net::account_data(v.get("value"))) {
                Ok(Some(d)) => {
                    let r = self.run.as_mut().unwrap();
                    r.root_creator = iq::decode_db_root(&d).map(|x| x.creator);
                    r.root_data = Some(d);
                    self.request_blockhash();
                }
                Ok(None) => self.fail("The database (DbRoot) doesn't exist yet, so tables can't be created."),
                Err(e) => self.fail(format!("Could not read the DbRoot: {}", e)),
            },
            RunOp::Blockhash => match res.map(|v| v.get("value").get("blockhash").str().and_then(base58::decode32)) {
                Ok(Some(bh)) => self.build_and_go(bh),
                Ok(None) => self.fail("RPC returned no blockhash"),
                Err(e) => self.fail(format!("Could not get a blockhash: {}", e)),
            },
            RunOp::Simulate => match res {
                Ok(v) => {
                    let val = v.get("value");
                    if !val.get("err").is_null() {
                        let logs: Vec<String> = val.get("logs").arr().iter().filter_map(|l| l.str().map(String::from)).collect();
                        let tail = logs.iter().rev().take(6).rev().cloned().collect::<Vec<_>>().join("\n");
                        let e = format!("{}", val.get("err"));
                        if tail.contains("insufficient lamports") || e.contains("InsufficientFunds") || tail.contains("insufficient funds") {
                            self.pause(format!("Paused: not enough SOL in the database wallet for this step.\n{}", tail));
                        } else {
                            self.fail(format!("Simulation rejected this step (nothing was sent): {}\n{}", e, tail));
                        }
                    } else {
                        let post = val.get("accounts").idx(0).get("lamports").u64();
                        let r = self.run.as_mut().unwrap();
                        if let (Some(post), Some(bal)) = (post, r.balance) {
                            let cost = bal.saturating_sub(post);
                            let i = r.i;
                            r.steps[i].cost = Some(cost);
                        }
                        self.send_current();
                    }
                }
                Err(e) => {
                    if self.maybe_fallback(&e) {
                        return true;
                    }
                    self.fail(format!("Simulation failed: {}", e));
                }
            },
            RunOp::Send => match res {
                Ok(v) => {
                    let sig = v.str_or("");
                    let r = self.run.as_mut().unwrap();
                    r.sig = Some(sig.clone());
                    r.sent_at = host::now_ms();
                    r.state = RunState::Working(format!("Confirming {}", solana::short(&sig)));
                    self.timer(1200, P::Run(RunOp::Tick));
                }
                Err(e) => {
                    if self.maybe_fallback(&e) {
                        return true;
                    }
                    self.fail(format!("Send failed: {}", e));
                }
            },
            RunOp::Tick => {
                let Some(sig) = self.run.as_ref().and_then(|r| r.sig.clone()) else { return true };
                let params = json::parse(&format!("[[\"{}\"],{{\"searchTransactionHistory\":false}}]", sig)).unwrap();
                self.rpc("getSignatureStatuses", params, P::Run(RunOp::Status));
                return false;
            }
            RunOp::Status => {
                let v = res.as_ref().map(|v| v.get("value").idx(0).clone()).unwrap_or(Json::Null);
                let r = self.run.as_mut().unwrap();
                if !v.is_null() && !v.get("err").is_null() {
                    let e = format!("Transaction failed on chain: {}", v.get("err"));
                    self.fail(e);
                } else if matches!(v.get("confirmationStatus").str(), Some("confirmed") | Some("finalized")) {
                    self.complete_step();
                } else if host::now_ms() - r.sent_at > 75_000.0 {
                    r.attempt += 1;
                    if r.attempt > 3 {
                        self.fail("Transaction was not confirmed after 3 attempts — the RPC may be dropping transactions. Try a different RPC in Settings, then Resume.");
                    } else {
                        r.note(false, "Not confirmed in 75s (likely dropped); retrying with a fresh blockhash");
                        self.request_blockhash();
                    }
                } else {
                    self.timer(1500, P::Run(RunOp::Tick));
                    return false;
                }
            }
        }
        true
    }

    fn draft_name(&self) -> String {
        self.run.as_ref().and_then(|r| self.draft_idx(&r.draft)).map(|i| self.drafts[i].name.clone()).unwrap_or_default()
    }

    fn after_prep(&mut self, v: &Json) {
        let vals = v.get("value").arr().to_vec();
        let key = self.run.as_ref().unwrap().draft.clone();
        let Some(di) = self.draft_idx(&key) else { return };
        let kp = self.run.as_ref().unwrap().kp.clone();
        let exists = |k: usize| vals.get(k).map(|x| !x.is_null()).unwrap_or(false);
        let root = vals.first().and_then(net::account_data).and_then(|d| iq::decode_db_root(&d));
        let has_inventory = exists(1);
        let seq = vals.get(2).and_then(net::account_data).and_then(|d| iq::decode_user_state_seq(&d)).unwrap_or(0);
        let mint = solana::pk(iq::IQ_MINT_STR);
        let iq_ata = if exists(3) {
            Some(iq::ata(&kp.pubkey, &mint, &solana::pk(iq::TOKEN_PROGRAM_STR)))
        } else if exists(4) {
            Some(iq::ata(&kp.pubkey, &mint, &solana::pk(iq::TOKEN_2022_STR)))
        } else {
            None
        };
        let bal = vals.get(5).and_then(|x| x.get("lamports").u64()).unwrap_or(0);
        let len = |k: usize| vals.get(k).and_then(net::account_data).map(|d| d.len() as u64).unwrap_or(0);
        let mut steps = vec![];
        let mut notes: Vec<(bool, String)> = vec![];
        // Like the SDK: only use v1 transactions once the cluster's feature gate is on.
        {
            let r = self.run.as_mut().unwrap();
            if r.auto && !r.legacy && !vals.get(7).map(iq::v1_active).unwrap_or(false) {
                r.legacy = true;
                notes.push((true, "v1 transactions aren't active on this cluster yet, so packs use legacy transactions (up to 700 bytes each).".into()));
            }
        }
        let contributor;
        match &root {
            None => {
                steps.push(Step { kind: StepKind::Root, sig: None, cost: None });
                contributor = false;
            }
            Some(r) => {
                contributor = r.creator != kp.pubkey;
                if contributor {
                    notes.push((
                        true,
                        format!(
                            "\"{}\" already exists and is owned by {}. Your rows will be inscribed as unofficial contributions from {}.",
                            self.drafts[di].name,
                            solana::short(&b58(&r.creator)),
                            solana::short(&b58(&kp.pubkey))
                        ),
                    ));
                } else if self.drafts[di].root_sig.is_none() {
                    self.drafts[di].root_sig = Some("existing".into());
                }
            }
        }
        let tables = self.drafts[di].tables.clone();
        let wallet_s = b58(&kp.pubkey);
        for (t, tb) in tables.iter().enumerate() {
            let on_chain = exists(8 + t);
            if on_chain {
                if tb.created.is_none() {
                    self.drafts[di].tables[t].created = Some("existing".into());
                }
                // what the chain has now, for renames and writer changes; a
                // table with no change of its own takes the chain's name and
                // writers (so saving never changes what nobody asked to change)
                if let Some(meta) = vals.get(8 + t).and_then(net::account_data).and_then(|d| iq::decode_table(&d)) {
                    let owner = root.as_ref().map(|r| b58(&r.creator));
                    let x = &mut self.drafts[di].tables[t];
                    let chain_w: Vec<String> = meta.writers.iter().map(b58).collect();
                    if x.chain_title.is_none() && !meta.name.is_empty() {
                        x.title = meta.name.clone();
                    }
                    if x.chain_writers.is_none() {
                        x.open = chain_w.is_empty();
                        x.writers = chain_w.iter().filter(|w| Some(*w) != owner.as_ref()).cloned().collect();
                    }
                    x.chain_title = Some(meta.name.clone());
                    x.chain_writers = Some(chain_w);
                }
                continue;
            }
            if tb.dropped {
                continue;
            }
            if let Some(r) = &root {
                if contributor && !r.table_creators.is_empty() && !r.table_creators.contains(&kp.pubkey) {
                    self.fail(format!("Table \"{}\" doesn't exist and only the database owner can create tables here.", tb.name));
                    return;
                }
            }
            steps.push(Step { kind: StepKind::Table(t), sig: None, cost: None });
        }
        let legacy = self.run.as_ref().unwrap().legacy;
        if !has_inventory {
            steps.push(Step { kind: StepKind::UserInit, sig: None, cost: None });
        } else if !legacy {
            let mut grow = vec![];
            if len(1) < iq::USER_INVENTORY_SPACE {
                grow.push((iq::user_inventory_pda(&kp.pubkey), iq::USER_INVENTORY_SPACE));
            }
            if exists(6) && len(6) < iq::CODE_ACCOUNT_SPACE {
                grow.push((iq::code_account_pda(&kp.pubkey), iq::CODE_ACCOUNT_SPACE));
            }
            if !grow.is_empty() {
                steps.push(Step { kind: StepKind::Grow(grow), sig: None, cost: None });
            }
        }
        let cap = if legacy { iq::INLINE_CAP_LEGACY } else { iq::INLINE_CAP_V1 };
        let tables = self.drafts[di].tables.clone();
        let mut list_change = false;
        for (t, tb) in tables.iter().enumerate() {
            if !contributor && exists(8 + t) {
                let desired = tb.desired_writers(Some(&wallet_s));
                let chain_w = tb.chain_writers.clone().unwrap_or_default();
                let title_differs = tb.chain_title.as_deref().map(|x| x != tb.title).unwrap_or(false);
                let writers_differ = { let mut a = desired.clone(); let mut b = chain_w.clone(); a.sort(); b.sort(); a != b };
                if !tb.dropped && (title_differs || writers_differ) {
                    let meta = vals.get(8 + t).and_then(net::account_data).and_then(|d| iq::decode_table(&d));
                    let (cols, id_col, ext) = match meta {
                        Some(m) => (m.columns, m.id_col, m.ext_keys),
                        None => (vec!["id".to_string(), "p".to_string()], "id".to_string(), vec![]),
                    };
                    let wpk: Vec<Pubkey> = desired.iter().filter_map(|w| solana::parse_pk(w)).collect();
                    let seed = iq::seed_bytes(&tb.name);
                    let spec = iq::TableSpec { db_id: &[], table_seed: &seed, hint: "", name: &tb.title, columns: &cols, id_col: &id_col, ext_keys: &ext, writers: Some(&wpk) };
                    let need = iq::table_account_size(&spec);
                    let have = len(8 + t);
                    let realloc = (need > have).then(|| need + 64);
                    steps.push(Step { kind: StepKind::UpdateTable { t, realloc, cols, id_col, ext }, sig: None, cost: None });
                }
            }
            if tb.schema_changed() && !contributor {
                let payload = pack::encode_schema(&tb.doc().to_json(), cap);
                if pack::inscribed_size(&payload) > cap {
                    self.fail(format!("The structure of \"{}\" is too large for one {} transaction ({} bytes).", tb.title, if legacy { "legacy" } else { "v1" }, pack::inscribed_size(&payload)));
                    return;
                }
                steps.push(Step { kind: StepKind::Schema { t, payload, doc: tb.doc_text() }, sig: None, cost: None });
            }
            if tb.dropped {
                list_change = true;
                continue;
            }
            let ghost_idx: Vec<usize> = tb.rows.iter().enumerate().filter(|(_, r)| r.sig.is_none()).map(|(i, _)| i).collect();
            if ghost_idx.is_empty() {
                continue;
            }
            let plan = self.plan_for(&key, t, cap).clone();
            match plan {
                Ok(packs) => {
                    for p in packs {
                        let rows = ghost_idx[p.first..p.first + p.count].to_vec();
                        steps.push(Step {
                            kind: StepKind::Pack { t, rows, payload: p.payload, pack_id: p.pack_id, count: p.count },
                            sig: None,
                            cost: None,
                        });
                    }
                }
                Err(e) => {
                    self.fail(format!("Table \"{}\": {}", tb.name, e));
                    return;
                }
            }
        }
        // the database's table list: dropped tables come off, revived ones go back
        if !contributor {
            if let Some(rt) = &root {
                let rp = iq::db_root_pda(self.drafts[di].name.as_bytes());
                let pda_of = |h: &[u8]| -> Pubkey {
                    let seed = match std::str::from_utf8(h) {
                        Ok(s) if !s.is_empty() => iq::seed_bytes(s),
                        _ => h.to_vec(),
                    };
                    iq::table_pda(&rp, &seed)
                };
                let dropped: Vec<Pubkey> = tables.iter().filter(|x| x.dropped && x.created.is_some()).map(|x| iq::table_pda(&rp, &iq::seed_bytes(&x.name))).collect();
                let mut seeds: Vec<Vec<u8>> = rt.table_seeds.iter().filter(|h| !dropped.contains(&pda_of(h))).cloned().collect();
                for x in tables.iter().filter(|x| !x.dropped && x.clear && x.created.is_some()) {
                    let p = iq::table_pda(&rp, &iq::seed_bytes(&x.name));
                    if !seeds.iter().any(|h| pda_of(h) == p) {
                        seeds.push(x.name.as_bytes().to_vec());
                    }
                }
                if seeds != rt.table_seeds || list_change && seeds.len() != rt.table_seeds.len() {
                    let used = rt.used;
                    let old: usize = rt.table_seeds.iter().map(|h| 4 + h.len()).sum();
                    let new: usize = seeds.iter().map(|h| 4 + h.len()).sum();
                    let realloc = (used + new > old + rt.account_len).then(|| (rt.account_len + 2048) as u64);
                    steps.push(Step { kind: StepKind::TableList { seeds, realloc }, sig: None, cost: None });
                }
            }
        }
        self.save_drafts();
        let r = self.run.as_mut().unwrap();
        for n in notes {
            r.note(n.0, n.1);
        }
        r.seq = seq;
        r.iq_ata = iq_ata;
        r.contributor = contributor;
        r.root_creator = root.map(|x| x.creator);
        if r.start_balance.is_none() {
            r.start_balance = Some(bal);
        }
        r.balance = Some(bal);
        r.steps = steps;
        r.i = 0;
        let n = r.steps.len();
        if n == 0 {
            r.state = RunState::Done;
            r.note(true, "Nothing to save — everything is already on the blockchain.");
            return;
        }
        r.note(true, format!("{} step(s) planned; database wallet balance {}", n, ui::sol(bal)));
        r.state = RunState::Working("Starting".into());
        self.next_step();
    }

    fn next_step(&mut self) {
        let Some(r) = self.run.as_mut() else { return };
        if r.i >= r.steps.len() {
            r.state = RunState::Done;
            let spent = r.spent().map(ui::sol).unwrap_or_default();
            r.note(true, format!("Done. Spent {}.", spent));
            let gone = std::mem::take(&mut r.dropped_done);
            let key = r.draft.clone();
            for name in gone {
                if let Some(i) = self.draft_idx(&key) {
                    if let Some(t) = self.drafts[i].tables.iter().position(|x| x.name == name && x.dropped) {
                        self.drafts[i].tables.remove(t);
                        self.drafts[i].sel = 0;
                        self.table_removed(&key, t);
                    }
                }
            }
            self.save_drafts();
            let r = self.run.as_mut().unwrap();
            let w = b58(&r.kp.pubkey);
            self.fetch_balance(&w);
            self.ok("Inscription complete");
            // pick up new databases/tables next time the list is shown
            self.dbroots = crate::app::Load::None;
            return;
        }
        if r.stop {
            r.stop = false;
            let m = "Stopped. Progress is saved — Resume continues from here.".to_string();
            r.note(false, m.clone());
            r.state = RunState::Paused(m);
            return;
        }
        r.attempt = 0;
        r.state = RunState::Working(format!("Step {}/{}", r.i + 1, r.steps.len()));
        let params = json::parse(&format!("[\"{}\",{{\"commitment\":\"confirmed\"}}]", b58(&r.kp.pubkey))).unwrap();
        self.rpc("getBalance", params, P::Run(RunOp::Balance));
    }

    fn request_blockhash(&mut self) {
        let params = json::parse("[{\"commitment\":\"confirmed\"}]").unwrap();
        self.rpc("getLatestBlockhash", params, P::Run(RunOp::Blockhash));
    }

    fn build_and_go(&mut self, bh: [u8; 32]) {
        let r = self.run.as_ref().unwrap();
        let Some(di) = self.draft_idx(&r.draft) else { return };
        let d = &self.drafts[di];
        let kp = r.kp.clone();
        let db_id = d.name.as_bytes().to_vec();
        let step = r.steps[r.i].clone();
        let mut ixs = vec![];
        match &step.kind {
            StepKind::Root => {
                ixs.push(iq::initialize_db_root(&kp.pubkey, &db_id));
                if d.lock_creators {
                    ixs.push(iq::manage_table_creators(&kp.pubkey, &db_id, &[kp.pubkey], &[kp.pubkey]));
                }
            }
            StepKind::Table(t) => {
                let tb = &d.tables[*t];
                let seed = iq::seed_bytes(&tb.name);
                if let Some(root) = r.root_data.as_ref().and_then(|x| iq::decode_db_root(x)) {
                    if let Some(size) = iq::db_root_realloc_size(&root, tb.name.len()) {
                        ixs.push(iq::realloc_account(&kp.pubkey, &iq::db_root_pda(&db_id), size));
                    }
                }
                let cols = vec!["id".to_string(), "p".to_string()];
                let desired = tb.desired_writers(Some(&b58(&kp.pubkey)));
                let writers: Vec<Pubkey> = desired.iter().filter_map(|w| solana::parse_pk(w)).collect();
                let creator = r.root_creator.unwrap_or(kp.pubkey);
                ixs.push(iq::create_table(
                    &kp.pubkey,
                    &creator,
                    &iq::TableSpec {
                        db_id: &db_id,
                        table_seed: &seed,
                        hint: &tb.name,
                        name: &tb.title,
                        columns: &cols,
                        id_col: "id",
                        ext_keys: &[],
                        writers: if tb.open { None } else { Some(&writers) },
                    },
                ));
            }
            StepKind::UserInit => ixs.push(iq::user_initialize(&kp.pubkey)),
            StepKind::Grow(list) => {
                for (target, size) in list {
                    ixs.push(iq::realloc_account(&kp.pubkey, target, *size));
                }
            }
            StepKind::Pack { t, payload, .. } | StepKind::Schema { t, payload, .. } => {
                let tb = &d.tables[*t];
                let md = iq::inline_metadata(r.seq, &pack::row_json(payload));
                ixs.push(iq::db_code_in_inline(&kp.pubkey, &db_id, &iq::seed_bytes(&tb.name), &md, r.iq_ata));
            }
            StepKind::UpdateTable { t, realloc, cols, id_col, ext } => {
                let tb = &d.tables[*t];
                let seed = iq::seed_bytes(&tb.name);
                let root = iq::db_root_pda(&db_id);
                if let Some(size) = realloc {
                    ixs.push(iq::realloc_account(&kp.pubkey, &iq::table_pda(&root, &seed), *size));
                }
                let desired = tb.desired_writers(Some(&b58(&kp.pubkey)));
                let writers: Vec<Pubkey> = desired.iter().filter_map(|w| solana::parse_pk(w)).collect();
                ixs.push(iq::update_table(
                    &kp.pubkey,
                    &iq::TableSpec { db_id: &db_id, table_seed: &seed, hint: "", name: &tb.title, columns: cols, id_col, ext_keys: ext, writers: Some(&writers) },
                ));
            }
            StepKind::TableList { seeds, realloc } => {
                if let Some(size) = realloc {
                    ixs.push(iq::realloc_account(&kp.pubkey, &iq::db_root_pda(&db_id), *size));
                }
                ixs.push(iq::update_db_root_table_list(&kp.pubkey, &db_id, seeds));
            }
        }
        let msg = solana::compile(&kp.pubkey, &ixs, bh);
        let (raw, sig) = if r.legacy { solana::legacy_signed(&msg, &kp.seed) } else { solana::v1_signed(&msg, &kp.seed) };
        let limit = if r.legacy { solana::LEGACY_MAX_TX_BYTES } else { solana::V1_MAX_TX_BYTES };
        if raw.len() > limit {
            self.fail(format!("Transaction is {} bytes, over the {}-byte limit", raw.len(), limit));
            return;
        }
        let simulate = self.settings.simulate;
        let r = self.run.as_mut().unwrap();
        r.raw = raw;
        r.sig = Some(base58::encode(&sig));
        if simulate {
            r.state = RunState::Working(format!("Simulating step {}/{}", r.i + 1, r.steps.len()));
            let params = json::parse(&format!(
                "[\"{}\",{{\"encoding\":\"base64\",\"sigVerify\":false,\"replaceRecentBlockhash\":true,\"commitment\":\"confirmed\",\"accounts\":{{\"encoding\":\"base64\",\"addresses\":[\"{}\"]}}}}]",
                base64_encode(&r.raw),
                b58(&kp.pubkey)
            ))
            .unwrap();
            self.rpc("simulateTransaction", params, P::Run(RunOp::Simulate));
        } else {
            self.send_current();
        }
    }

    fn send_current(&mut self) {
        let r = self.run.as_mut().unwrap();
        r.state = RunState::Working(format!("Sending step {}/{}", r.i + 1, r.steps.len()));
        let preflight = !self.settings.simulate;
        let params = json::parse(&format!(
            "[\"{}\",{{\"encoding\":\"base64\",\"skipPreflight\":{},\"preflightCommitment\":\"confirmed\",\"maxRetries\":5}}]",
            base64_encode(&r.raw),
            !preflight
        ))
        .unwrap();
        self.rpc("sendTransaction", params, P::Run(RunOp::Send));
    }

    /// If the cluster/RPC can't take v1 transactions, drop to legacy (smaller
    /// packs) and re-plan. Only in Auto mode.
    fn maybe_fallback(&mut self, e: &str) -> bool {
        let Some(r) = self.run.as_mut() else { return false };
        let looks_v1 = {
            let l = e.to_lowercase();
            l.contains("deserial") || l.contains("unsupported") || l.contains("version") || l.contains("invalid transaction") || l.contains("failed to decode")
        };
        if !r.auto || r.legacy || !looks_v1 {
            return false;
        }
        r.legacy = true;
        r.note(false, format!("The RPC rejected the v1 transaction format ({}). Falling back to legacy transactions and re-planning packs (700-byte cap).", e.lines().next().unwrap_or("")));
        r.steps.clear();
        r.i = 0;
        self.prep();
        true
    }

    fn complete_step(&mut self) {
        let r = self.run.as_mut().unwrap();
        let sig = r.sig.clone().unwrap_or_default();
        let i = r.i;
        r.steps[i].sig = Some(sig.clone());
        let kind = r.steps[i].kind.clone();
        let cost = r.steps[i].cost;
        let key = r.draft.clone();
        let signer = b58(&r.kp.pubkey);
        let Some(di) = self.draft_idx(&key) else { return };
        let mut notify: Option<(String, String)> = None;
        let desc = {
            let r = self.run.as_ref().unwrap();
            r.describe(&r.steps[i], &self.drafts[di].tables)
        };
        match &kind {
            StepKind::Root => {
                self.drafts[di].root_sig = Some(sig.clone());
                self.run.as_mut().unwrap().root_creator = Some(self.run.as_ref().unwrap().kp.pubkey);
            }
            StepKind::Table(t) => {
                let signer_s = signer.clone();
                let x = &mut self.drafts[di].tables[*t];
                x.created = Some(sig.clone());
                x.chain_title = Some(x.title.clone());
                x.chain_writers = Some(x.desired_writers(Some(&signer_s)));
            }
            StepKind::UpdateTable { t, .. } => {
                let signer_s = signer.clone();
                let x = &mut self.drafts[di].tables[*t];
                x.chain_title = Some(x.title.clone());
                x.chain_writers = Some(x.desired_writers(Some(&signer_s)));
            }
            StepKind::TableList { .. } => {}
            StepKind::Schema { t, payload, doc } => {
                let x = &mut self.drafts[di].tables[*t];
                x.chain_doc = Some(doc.clone());
                x.clear = false;
                let dropped = x.dropped.then(|| x.name.clone());
                let root = iq::db_root_pda(self.drafts[di].name.as_bytes());
                let tpda = b58(&iq::table_pda(&root, &iq::seed_bytes(&self.drafts[di].tables[*t].name)));
                notify = Some((tpda, pack::row_json(payload)));
                if let Some(n) = dropped {
                    self.run.as_mut().unwrap().dropped_done.push(n);
                }
                let t = *t;
                self.bump(&key, t);
            }
            StepKind::UserInit => self.drafts[di].user_init_sig = Some(sig.clone()),
            StepKind::Grow(_) => {}
            StepKind::Pack { t, rows, payload, .. } => {
                let tb = &mut self.drafts[di].tables[*t];
                for &ri in rows {
                    if let Some(row) = tb.rows.get_mut(ri) {
                        row.sig = Some(sig.clone());
                    }
                }
                let root = iq::db_root_pda(self.drafts[di].name.as_bytes());
                let tpda = b58(&iq::table_pda(&root, &iq::seed_bytes(&self.drafts[di].tables[*t].name)));
                notify = Some((tpda, pack::row_json(payload)));
                let t = *t;
                self.bump(&key, t);
            }
        }
        self.save_drafts();
        let cost_s = cost.map(|c| format!(" · cost {}", ui::sol(c))).unwrap_or_default();
        let r = self.run.as_mut().unwrap();
        r.note(true, format!("✓ {}{} · {}", desc, cost_s, solana::short(&sig)));
        if let Some(c) = cost {
            r.balance = r.balance.map(|b| b.saturating_sub(c));
        }
        r.i += 1;
        let replan = matches!(kind, StepKind::UserInit) && !r.legacy;
        if let (Some((tpda, row)), true) = (notify, self.settings.notify_gateway && self.settings.cluster != "devnet") {
            // Warm the gateway cache so the explorer shows the rows right away.
            let body = format!("{{\"txSignature\":\"{}\",\"signer\":\"{}\",\"row\":{}}}", sig, signer, row);
            let id = self.nid();
            self.pending.insert(id, P::Ignore);
            let url = format!("{}/table/{}/notify", self.settings.gateway.trim_end_matches('/'), tpda);
            host::fetch(id, "POST", &url, &body, "application/json");
        }
        if replan {
            // A pre-upgrade program makes small accounts; re-read them so a
            // resize step is added before any 4 KB write.
            let r = self.run.as_mut().unwrap();
            r.steps.clear();
            r.i = 0;
            self.prep();
            return;
        }
        self.next_step();
    }
}
