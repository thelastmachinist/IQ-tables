//! Sending the parts of an upload session in parallel, the way IQ's SDK does
//! (`uploadSession` with its speed profiles): every `post_chunk` carries its
//! own index, so parts may land in any order and only the code-in that
//! finalizes the upload has to wait for all of them.
//!
//! Parts are signed locally and sent without waiting for each other, up to
//! the profile's number in flight and requests per second. Landed parts are
//! found with `getSignatureStatuses` (256 at a time). A part that hasn't
//! landed after a while is sent again with a fresh blockhash — if both copies
//! land, readers take either (they index parts by number), so a duplicate
//! only costs its network fee. When the RPC says "too many requests", fewer
//! parts go out at once, then the number climbs back.

use std::collections::VecDeque;

use crate::app::{fetch_err, App, P};
use crate::crypto::{base58, base64_encode};
use crate::host;
use crate::iq;
use crate::json::{self, Json};
use crate::net;
use crate::solana::{self, Keypair};

/// IQ SDK speed profiles (`sdk/utils/session_speed.js`): requests per second
/// and parts in flight.
pub struct Profile {
    pub name: &'static str,
    pub label: &'static str,
    pub max_rps: u32,
    pub parallel: usize,
}

pub const PROFILES: [Profile; 4] = [
    Profile { name: "light", label: "Light — one part at a time, 2 per second (the SDK's default)", max_rps: 2, parallel: 1 },
    Profile { name: "medium", label: "Medium — 5 parts at a time, up to 50 per second", max_rps: 50, parallel: 5 },
    Profile { name: "heavy", label: "Heavy — 50 parts at a time, up to 100 per second", max_rps: 100, parallel: 50 },
    Profile { name: "extreme", label: "Extreme — 100 parts at a time, up to 250 per second", max_rps: 250, parallel: 100 },
];

pub const DEFAULT_PROFILE: &str = "heavy";

pub fn profile(name: &str) -> &'static Profile {
    PROFILES.iter().find(|p| p.name == name).unwrap_or(&PROFILES[2])
}

/// A part not seen on chain this long after sending is sent again.
const RESEND_AFTER_MS: f64 = 20_000.0;
/// Blockhashes are good for ~60 s; refresh well before.
const BLOCKHASH_MAX_AGE_MS: f64 = 30_000.0;
const MAX_TRIES: u8 = 8;

/// Who started the batch (told when it finishes).
pub enum Owner {
    /// The save pipeline (inscribe.rs).
    Run,
    /// A file going into a cell (attach.rs).
    Attach(Box<crate::attach::Job>),
}

#[derive(Clone, Debug)]
enum Part {
    Todo(u8),
    Sending(u8),
    Sent { sig: String, at: f64, tries: u8 },
    Landed,
}

pub struct Batch {
    pub owner: Owner,
    pub kp: Keypair,
    pub legacy: bool,
    pub seq: u64,
    chunks: Vec<String>,
    /// Indexes below this were sent before the batch (with `create_session`).
    from: usize,
    parts: Vec<Part>,
    todo: VecDeque<usize>,
    pub cap: usize,
    max: usize,
    rps: u32,
    in_flight: usize,
    bh: Option<[u8; 32]>,
    bh_at: f64,
    bh_wait: bool,
    bh_errors: u32,
    sent_at: VecDeque<f64>,
    polling: bool,
    timer: bool,
    hold_until: f64,
    streak: usize,
    pub landed: usize,
    pub stop: bool,
    /// The owner was told the batch stopped.
    halted: bool,
    pub throttled: u32,
    pub resent: u32,
    pub started: f64,
    /// Signature of one landed part (reported as the step's signature).
    pub last_sig: Option<String>,
}

#[derive(Clone, Debug)]
pub enum UpOp {
    Hash,
    Sent(usize),
    Status(Vec<usize>),
    Tick,
}

impl Batch {
    pub fn new(owner: Owner, kp: Keypair, legacy: bool, seq: u64, chunks: Vec<String>, from: usize, speed: &str) -> Batch {
        let p = profile(speed);
        let n = chunks.len();
        Batch {
            owner,
            kp,
            legacy,
            seq,
            from,
            parts: (0..n).map(|i| if i < from { Part::Landed } else { Part::Todo(0) }).collect(),
            todo: (from..n).collect(),
            chunks,
            cap: p.parallel,
            max: p.parallel,
            rps: p.max_rps,
            in_flight: 0,
            bh: None,
            bh_at: 0.0,
            bh_wait: false,
            bh_errors: 0,
            sent_at: VecDeque::new(),
            polling: false,
            timer: false,
            hold_until: 0.0,
            streak: 0,
            landed: 0,
            stop: false,
            halted: false,
            throttled: 0,
            resent: 0,
            started: host::now_ms(),
            last_sig: None,
        }
    }

    /// Parts this batch sends.
    pub fn total(&self) -> usize {
        self.chunks.len() - self.from
    }

    pub fn same_upload(&self, seq: u64, chunks: &[String]) -> bool {
        self.seq == seq && self.chunks == chunks
    }

    pub fn done(&self) -> bool {
        self.landed >= self.total()
    }

    /// "812 of 1,553 parts on chain · 50 at a time"
    pub fn progress(&self) -> String {
        let secs = ((host::now_ms() - self.started) / 1000.0).max(0.0) as u64;
        let mut s = format!("{} of {} parts on chain · {} at a time · {}s", self.landed, self.total(), self.cap, secs);
        if self.throttled > 0 {
            s.push_str(" · slowed down for the RPC's rate limit");
        }
        s
    }

    fn requeue(&mut self, k: usize, tries: u8) {
        self.parts[k] = Part::Todo(tries);
        self.todo.push_back(k);
    }
}

/// Does an RPC error mean "too many requests"?
fn rate_limited(status: u32, text: &str) -> bool {
    let l = text.to_ascii_lowercase();
    status == 429 || l.contains("429") || l.contains("too many requests") || l.contains("rate limit")
}

impl App {
    pub fn up_begin(&mut self, key: &str, b: Batch) {
        self.uploads.insert(key.to_string(), b);
        self.up_pump(key);
    }

    /// Continue a stopped batch.
    pub fn up_resume(&mut self, key: &str) {
        if let Some(b) = self.uploads.get_mut(key) {
            b.stop = false;
            b.halted = false;
            b.hold_until = 0.0;
        }
        self.up_pump(key);
    }

    fn owner_stopping(&self, key: &str) -> bool {
        match self.uploads.get(key).map(|b| &b.owner) {
            Some(Owner::Run) => self.run.as_ref().map(|r| r.stop).unwrap_or(true),
            _ => false,
        }
    }

    fn up_pump(&mut self, key: &str) {
        let stopping = self.owner_stopping(key);
        let now = host::now_ms();
        let mut sends: Vec<(usize, String, String)> = vec![];
        let mut want_hash = false;
        let mut want_poll = false;
        let mut want_timer: Option<u32> = None;
        {
            let Some(b) = self.uploads.get_mut(key) else { return };
            if stopping {
                b.stop = true;
            }
            if b.done() {
                return self.up_finish(key);
            }
            if b.stop {
                if b.in_flight == 0 && !b.bh_wait && !b.halted {
                    b.halted = true;
                    return self.up_stopped(key);
                }
                return;
            }
            if (b.bh.is_none() || now - b.bh_at > BLOCKHASH_MAX_AGE_MS) && !b.bh_wait {
                b.bh_wait = true;
                want_hash = true;
            }
            if let Some(bh) = b.bh.filter(|_| now - b.bh_at <= BLOCKHASH_MAX_AGE_MS * 1.5) {
                while b.in_flight < b.cap && !b.todo.is_empty() {
                    if now < b.hold_until {
                        want_timer = Some((b.hold_until - now).max(50.0) as u32);
                        break;
                    }
                    while b.sent_at.front().map(|t| now - t >= 1000.0).unwrap_or(false) {
                        b.sent_at.pop_front();
                    }
                    if b.sent_at.len() as u32 >= b.rps {
                        let wait = b.sent_at.front().map(|t| 1000.0 - (now - t)).unwrap_or(100.0);
                        want_timer = Some(wait.max(20.0) as u32);
                        break;
                    }
                    let k = b.todo.pop_front().unwrap();
                    let tries = match b.parts[k] {
                        Part::Todo(t) => t,
                        _ => continue,
                    };
                    let ix = iq::post_chunk(&b.kp.pubkey, b.seq, k as u32, &b.chunks[k]);
                    let msg = solana::compile(&b.kp.pubkey, &[ix], bh);
                    let (raw, sig) = if b.legacy { solana::legacy_signed(&msg, &b.kp.seed) } else { solana::v1_signed(&msg, &b.kp.seed) };
                    b.parts[k] = Part::Sending(tries);
                    b.in_flight += 1;
                    b.sent_at.push_back(now);
                    sends.push((k, base58::encode(&sig), base64_encode(&raw)));
                }
            }
            if !b.polling && b.parts.iter().any(|p| matches!(p, Part::Sent { .. })) {
                b.polling = true;
                want_poll = true;
            }
            if want_timer.is_some() {
                if b.timer {
                    want_timer = None;
                } else {
                    b.timer = true;
                }
            }
        }
        if want_hash {
            let params = json::parse("[{\"commitment\":\"confirmed\"}]").unwrap();
            self.rpc("getLatestBlockhash", params, P::Up(key.to_string(), UpOp::Hash));
        }
        for (k, sig, raw) in sends {
            let params = json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"skipPreflight\":true,\"preflightCommitment\":\"confirmed\",\"maxRetries\":3}}]", raw)).unwrap();
            let _ = sig;
            self.rpc("sendTransaction", params, P::Up(key.to_string(), UpOp::Sent(k)));
        }
        if want_poll {
            self.timer(700, P::Up(key.to_string(), UpOp::Tick));
        }
        if let Some(ms) = want_timer {
            self.timer(ms, P::Up(format!("{}#pump", key), UpOp::Tick));
        }
        self.up_report(key);
    }

    /// Show progress wherever the owner shows it.
    fn up_report(&mut self, key: &str) {
        let Some(b) = self.uploads.get(key) else { return };
        let line = b.progress();
        match &b.owner {
            Owner::Run => {
                if let Some(r) = self.run.as_mut() {
                    if r.busy() {
                        r.state = crate::inscribe::RunState::Working(format!("Step {}/{} · {}", r.i + 1, r.steps.len(), line));
                    }
                }
            }
            Owner::Attach(job) => {
                let cell = job.cell();
                let name = job.filename.clone();
                self.attach_status = Some((cell, format!("Inscribing {} — {}", name, line)));
            }
        }
    }

    fn up_fail(&mut self, key: &str, msg: String) {
        let Some(b) = self.uploads.remove(key) else { return };
        match b.owner {
            Owner::Run => self.run_batch_failed(msg),
            Owner::Attach(_) => self.attach_batch_failed(msg),
        }
    }

    fn up_finish(&mut self, key: &str) {
        let Some(b) = self.uploads.remove(key) else { return };
        let sig = b.last_sig.clone().unwrap_or_default();
        let cost = b.total() as u64 * iq::TX_FEE + b.resent as u64 * iq::TX_FEE;
        match b.owner {
            Owner::Run => self.run_batch_done(sig, cost),
            Owner::Attach(job) => self.attach_batch_done(*job),
        }
    }

    fn up_stopped(&mut self, key: &str) {
        let Some(b) = self.uploads.get(key) else { return };
        if matches!(b.owner, Owner::Run) {
            self.run_batch_stopped();
        }
    }

    pub fn up_async(&mut self, key: String, op: UpOp, ok: bool, status: u32, data: Vec<u8>) -> bool {
        if let Some(k) = key.strip_suffix("#pump") {
            if let Some(b) = self.uploads.get_mut(k) {
                b.timer = false;
            }
            let k = k.to_string();
            self.up_pump(&k);
            return true;
        }
        if !self.uploads.contains_key(&key) {
            return false;
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        let http_ok = ok && (200..300).contains(&status);
        let res = if http_ok { net::rpc_result(&text) } else { Err(fetch_err(ok, status, &text)) };
        let now = host::now_ms();
        match op {
            UpOp::Hash => {
                let bh = res.as_ref().ok().and_then(|r| r.get("value").get("blockhash").str().and_then(base58::decode32));
                let b = self.uploads.get_mut(&key).unwrap();
                b.bh_wait = false;
                match bh {
                    Some(h) => {
                        b.bh = Some(h);
                        b.bh_at = now;
                        b.bh_errors = 0;
                    }
                    None => {
                        b.bh_errors += 1;
                        if b.bh_errors > 5 {
                            let e = res.err().unwrap_or_else(|| "no blockhash".into());
                            self.up_fail(&key, format!("Couldn't get a blockhash from the RPC: {}", e));
                            return true;
                        }
                        b.hold_until = now + 1000.0 * b.bh_errors as f64;
                        let wait = 1000 * b.bh_errors;
                        if !b.timer {
                            b.timer = true;
                            self.timer(wait, P::Up(format!("{}#pump", key), UpOp::Tick));
                        }
                        return true;
                    }
                }
            }
            UpOp::Sent(k) => {
                let b = self.uploads.get_mut(&key).unwrap();
                b.in_flight = b.in_flight.saturating_sub(1);
                let tries = match b.parts[k] {
                    Part::Sending(t) => t,
                    _ => 0,
                };
                match res {
                    Ok(v) if v.str().is_some() => {
                        b.parts[k] = Part::Sent { sig: v.str_or(""), at: now, tries };
                        b.streak += 1;
                        if b.streak >= b.cap && b.cap < b.max {
                            b.cap += 1;
                            b.streak = 0;
                        }
                    }
                    Ok(_) => b.requeue(k, tries + 1),
                    Err(e) => {
                        b.streak = 0;
                        let l = e.to_ascii_lowercase();
                        if rate_limited(status, &text) {
                            b.throttled += 1;
                            b.cap = (b.cap / 2).max(1);
                            b.hold_until = now + 1000.0;
                            b.requeue(k, tries);
                        } else if l.contains("blockhash") {
                            b.bh = None;
                            b.requeue(k, tries);
                        } else if l.contains("insufficient") {
                            self.up_fail(&key, format!("Not enough SOL left to send the rest of the parts: {}", e));
                            return true;
                        } else if tries + 1 >= MAX_TRIES {
                            self.up_fail(&key, format!("Part {} couldn't be sent after {} tries: {}", k + 1, MAX_TRIES, e));
                            return true;
                        } else {
                            b.hold_until = now + 500.0;
                            b.requeue(k, tries + 1);
                        }
                    }
                }
            }
            UpOp::Tick => {
                let b = self.uploads.get_mut(&key).unwrap();
                let mut idx = vec![];
                let mut sigs = vec![];
                for (k, p) in b.parts.iter().enumerate() {
                    if let Part::Sent { sig, .. } = p {
                        idx.push(k);
                        sigs.push(json::s(sig));
                        if idx.len() == 256 {
                            break;
                        }
                    }
                }
                if idx.is_empty() {
                    b.polling = false;
                } else {
                    let params = Json::Arr(vec![Json::Arr(sigs), json::obj(vec![("searchTransactionHistory", Json::Bool(false))])]);
                    self.rpc("getSignatureStatuses", params, P::Up(key.clone(), UpOp::Status(idx)));
                    return false;
                }
            }
            UpOp::Status(idx) => {
                let b = self.uploads.get_mut(&key).unwrap();
                b.polling = false;
                match res {
                    Ok(v) => {
                        let vals = v.get("value").arr().to_vec();
                        for (j, k) in idx.into_iter().enumerate() {
                            let Part::Sent { sig, at, tries } = b.parts[k].clone() else { continue };
                            let st = vals.get(j).cloned().unwrap_or(Json::Null);
                            if !st.is_null() && !st.get("err").is_null() {
                                let e = format!("Part {} failed on chain: {}", k + 1, st.get("err"));
                                self.up_fail(&key, e);
                                return true;
                            }
                            if matches!(st.get("confirmationStatus").str(), Some("confirmed") | Some("finalized")) {
                                b.parts[k] = Part::Landed;
                                b.landed += 1;
                                b.last_sig = Some(sig);
                            } else if st.is_null() && now - at > RESEND_AFTER_MS {
                                if tries + 1 >= MAX_TRIES {
                                    self.up_fail(&key, format!("Part {} never landed after {} tries — the RPC may be dropping transactions. Try a different RPC in Settings.", k + 1, MAX_TRIES));
                                    return true;
                                }
                                b.resent += 1;
                                b.requeue(k, tries + 1);
                                // a stale blockhash is the usual reason
                                b.bh = None;
                            }
                        }
                    }
                    Err(_) => {
                        if rate_limited(status, &text) {
                            b.throttled += 1;
                            b.cap = (b.cap / 2).max(1);
                        }
                    }
                }
                if !b.done() && b.parts.iter().any(|p| matches!(p, Part::Sent { .. })) {
                    b.polling = true;
                    self.timer(1000, P::Up(key.clone(), UpOp::Tick));
                }
            }
        }
        self.up_pump(&key);
        true
    }
}
