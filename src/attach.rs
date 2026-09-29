//! Files in table cells, and opening them again.
//!
//! A small file is inscribed on its own with `user_inventory_code_in` — the
//! IQ SDK's `codeIn` direct path, same metadata, text stored as text and
//! binary as base64 — from the database's wallet. The cell then holds an
//! `iq://tx/<signature>#<filename>` link. Opening a link asks IQ's gateway
//! (`/data/<sig>`) first and falls back to reading the transaction from
//! Solana, so it works for files inscribed by any IQ tool.

use crate::app::{fetch_err, pct_decode, pct_encode, After, App, Load, P};
use crate::crypto::{base58, base64_decode, base64_encode};
use crate::host;
use crate::iq;
use crate::json::{self, Json};
use crate::net;
use crate::solana::{self, b58, parse_pk, Pubkey};
use crate::state::TxFormat;
use crate::ui;

#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// First IQ write from this wallet: create its IQ accounts.
    Init,
    /// Grow pre-upgrade accounts so a v1 (4 KB) write fits.
    Grow(Vec<(Pubkey, u64)>),
    Write,
}

#[derive(Clone)]
pub struct Job {
    pub key: String,
    pub t: usize,
    pub r: usize,
    pub c: usize,
    pub wallet: String,
    pub filename: String,
    pub metadata: String,
    pub legacy: bool,
    pub iq_ata: Option<Pubkey>,
    pub stage: Stage,
}

impl Job {
    pub fn cell(&self) -> String {
        format!("{}:{}:{}", self.key, self.t, self.r)
    }
}

/// An opened `iq://tx/…` link.
pub struct Viewed {
    pub filetype: String,
    pub filename: String,
    pub text: Option<String>,
    pub bytes: Option<Vec<u8>>,
    pub signer: String,
    pub time: Option<i64>,
    /// Set when the content can't be shown here (e.g. a chunked upload).
    pub note: Option<String>,
    pub source: &'static str,
}

pub struct Viewer {
    pub sig: String,
    pub label: String,
    pub state: Load<Viewed>,
}

/// A file a wallet inscribed (from IQ's `/user/<wallet>/assets`).
pub struct Asset {
    pub sig: String,
    pub filename: String,
    pub filetype: String,
    pub time: Option<i64>,
    pub chunks: u64,
}

pub fn is_text(ft: &str) -> bool {
    let ft = ft.to_ascii_lowercase();
    ft.starts_with("text/")
        || ft.ends_with("+json")
        || ft.ends_with("+xml")
        || ["application/json", "application/xml", "image/svg+xml", "application/javascript", "application/x-ndjson", "application/csv"].contains(&ft.as_str())
}

/// Image types we'll show inline (as a data: URL inside <img>, never as a document).
pub fn inline_image(ft: &str) -> bool {
    ["image/png", "image/jpeg", "image/gif", "image/webp", "image/avif", "image/bmp", "image/svg+xml"].contains(&ft.to_ascii_lowercase().as_str())
}

fn looks_b64(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && t.len() % 4 == 0 && t.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'=')
}

/// Build the viewer contents from a file's metadata and data string.
pub fn viewed(filetype: &str, filename: &str, data: String, signer: String, time: Option<i64>, source: &'static str) -> Viewed {
    let mut v = Viewed {
        filetype: if filetype.is_empty() { "application/octet-stream".into() } else { filetype.to_string() },
        filename: if filename.is_empty() { "file".into() } else { filename.to_string() },
        text: None,
        bytes: None,
        signer,
        time,
        note: None,
        source,
    };
    if !is_text(&v.filetype) && looks_b64(&data) {
        if let Some(b) = base64_decode(data.trim()) {
            v.bytes = Some(b);
            return v;
        }
    }
    v.text = Some(data);
    v
}

/// (filetype, filename, total_chunks) from a metadata JSON string.
fn meta_fields(md: &Json) -> (String, String, u64) {
    (md.get("filetype").str_or(""), md.get("filename").str_or(""), md.get("total_chunks").u64().unwrap_or(1))
}

fn parse_meta(v: &Json) -> Json {
    match v {
        Json::Str(s) => json::parse(s).unwrap_or(Json::Null),
        other => other.clone(),
    }
}

/// The file (or row) carried by a transaction read from Solana.
fn viewed_from_tx(result: &Json, sig: &str) -> Result<Viewed, String> {
    if result.is_null() {
        return Err("transaction not found on this cluster".into());
    }
    let raw = result.get("transaction").idx(0).str().and_then(base64_decode).ok_or("unreadable transaction")?;
    let tx = solana::parse_tx(&raw).ok_or("unreadable transaction")?;
    let pid = iq::program_id();
    let time = result.get("blockTime").u64().map(|t| t as i64);
    for (p, accs, data) in &tx.ixs {
        if tx.keys.get(*p) != Some(&pid) {
            continue;
        }
        let found = iq::decode_inventory_code_in(data).or_else(|| iq::decode_db_code_in(data).map(|d| (d.on_chain_path, d.metadata)));
        let Some((path, metadata)) = found else { continue };
        let signer = accs.first().and_then(|&i| tx.keys.get(i)).map(b58).unwrap_or_default();
        let md = json::parse(&metadata).unwrap_or(Json::Null);
        let (ft, name, chunks) = meta_fields(&md);
        if !path.is_empty() {
            let mut v = viewed(&ft, &name, String::new(), signer, time, "Solana");
            v.text = None;
            v.note = Some(format!("This file was uploaded in {} chunks. IQ's gateway reassembles those; open it there.", chunks));
            return Ok(v);
        }
        let d = match md.get("data") {
            Json::Str(s) => s.clone(),
            Json::Null => String::new(),
            other => other.to_string(),
        };
        return Ok(viewed(&ft, &name, d, signer, time, "Solana"));
    }
    Err(format!("{} isn't an IQ inscription", solana::short(sig)))
}

impl App {
    /// Column that receives file links for a draft table.
    pub fn attach_column(&self, key: &str, t: usize) -> Option<usize> {
        let tb = self.drafts.get(self.draft_idx(key)?)?.tables.get(t)?;
        if let Some(name) = self.form.get(&format!("attachcol:{}:{}", key, t)) {
            return tb.columns.iter().position(|c| c == name).filter(|&i| i != tb.id_col);
        }
        let hints = ["file", "attach", "link", "url", "image", "img", "doc", "media", "photo", "pdf"];
        tb.columns.iter().enumerate().find(|(i, c)| *i != tb.id_col && hints.iter().any(|h| c.to_lowercase().contains(h))).map(|(i, _)| i)
    }

    pub fn attach_busy(&self, key: &str) -> bool {
        self.attach_status.as_ref().map(|(cell, _)| cell.split(':').next() == Some(key)).unwrap_or(false)
    }

    pub fn attach_file(&mut self, arg: &str, val: &str) {
        let p: Vec<&str> = arg.split(':').collect();
        if p.len() != 3 {
            return;
        }
        let (Some(i), Ok(t), Ok(r)) = (self.draft_idx(p[0]), p[1].parse::<usize>(), p[2].parse::<usize>()) else { return };
        let key = p[0].to_string();
        if self.attach_status.is_some() {
            self.err("Another file is still being inscribed — wait for it to finish.");
            return;
        }
        let ghost = self.drafts[i].tables.get(t).and_then(|tb| tb.rows.get(r)).map(|x| x.sig.is_none() && !x.deleted).unwrap_or(false);
        if !ghost {
            self.err("Files can only be attached to ghost rows.");
            return;
        }
        let Some(c) = self.attach_column(&key, t) else {
            self.err("Choose which column holds files (above the rows), or add one like \"file\".");
            return;
        };
        let Some(wallet) = self.drafts[i].wallet.clone() else {
            self.err("Pick this database's wallet first — it inscribes and pays for the file.");
            return;
        };
        if self.keypair(&wallet).is_none() {
            self.err("Log in with the account that holds this database's wallet.");
            return;
        }
        let v = match json::parse(val) {
            Ok(v) => v,
            Err(e) => return self.err(format!("Couldn't read that file: {}", e)),
        };
        let name = v.get("name").str_or("file");
        let mut ftype = v.get("type").str_or("");
        if ftype.is_empty() {
            ftype = "application/octet-stream".into();
        }
        let Some(bytes) = v.get("b64").str().and_then(base64_decode) else {
            return self.err("Couldn't read that file");
        };
        let data = match String::from_utf8(bytes.clone()) {
            Ok(s) if is_text(&ftype) => s,
            _ => base64_encode(&bytes),
        };
        let metadata = iq::file_metadata(&ftype, &name, &data);
        let size = metadata.len();
        let legacy_only = self.settings.tx_format == TxFormat::Legacy;
        let cap = if legacy_only { iq::INLINE_CAP_LEGACY } else { iq::INLINE_CAP_V1 };
        if size > cap {
            let room = cap.saturating_sub(iq::file_metadata(&ftype, &name, "").len());
            return self.err(format!(
                "{} is {} once encoded; a one-transaction file can carry {} (about {} of binary data or {} of text). Bigger files need IQ's chunked upload, which the portal doesn't do yet.",
                name,
                ui::bytes(size),
                ui::bytes(cap),
                ui::bytes(room * 3 / 4),
                ui::bytes(room)
            ));
        }
        let job = Job {
            key,
            t,
            r,
            c,
            wallet,
            filename: name,
            metadata,
            legacy: legacy_only || size <= iq::INLINE_CAP_LEGACY,
            iq_ata: None,
            stage: Stage::Write,
        };
        self.attach_check(job);
    }

    fn attach_status_set(&mut self, job: &Job, msg: &str) {
        self.attach_status = Some((job.cell(), msg.to_string()));
    }

    fn attach_fail(&mut self, msg: impl Into<String>) {
        self.attach_status = None;
        self.err(msg);
    }

    fn attach_check(&mut self, job: Job) {
        let user = parse_pk(&job.wallet).unwrap();
        let mint = solana::pk(iq::IQ_MINT_STR);
        let addrs = [
            iq::user_inventory_pda(&user),
            iq::code_account_pda(&user),
            iq::ata(&user, &mint, &solana::pk(iq::TOKEN_PROGRAM_STR)),
            iq::ata(&user, &mint, &solana::pk(iq::TOKEN_2022_STR)),
            solana::pk(iq::TX_V1_FEATURE_GATE_STR),
            user,
        ];
        let params = Json::Arr(vec![
            Json::Arr(addrs.iter().map(|a| json::s(&b58(a))).collect()),
            json::obj(vec![("encoding", json::s("base64")), ("commitment", json::s("confirmed"))]),
        ]);
        self.attach_status_set(&job, &format!("Preparing {}…", job.filename));
        self.rpc("getMultipleAccounts", params, P::AttachCheck(job));
    }

    fn attach_send(&mut self, job: Job) {
        let msg = match &job.stage {
            Stage::Init => "Setting up the wallet's IQ account (one time)…".to_string(),
            Stage::Grow(_) => "Enlarging the wallet's IQ accounts for 4 KB writes (one time)…".to_string(),
            Stage::Write => format!("Inscribing {}…", job.filename),
        };
        self.attach_status_set(&job, &msg);
        let params = json::parse("[{\"commitment\":\"confirmed\"}]").unwrap();
        self.rpc("getLatestBlockhash", params, P::AttachHash(job));
    }

    /// A transaction of the attachment flow confirmed.
    pub fn attach_confirmed(&mut self, mut job: Job, sig: &str) {
        match job.stage {
            Stage::Init => {
                // the wallet's one-time IQ setup is done; drafts using it no longer need it
                for d in self.drafts.iter_mut().filter(|d| d.wallet.as_deref() == Some(job.wallet.as_str())) {
                    d.user_init_sig.get_or_insert_with(|| sig.to_string());
                }
                self.save_drafts();
                self.attach_check(job) // re-read sizes (pre-upgrade accounts need growing)
            }
            Stage::Grow(_) => {
                job.stage = Stage::Write;
                self.attach_send(job);
            }
            Stage::Write => self.attach_done(job, sig),
        }
    }

    fn attach_done(&mut self, job: Job, sig: &str) {
        self.attach_status = None;
        let link = format!("iq://tx/{}#{}", sig, pct_encode(&job.filename));
        let mut placed = false;
        if let Some(i) = self.draft_idx(&job.key) {
            if let Some(row) = self.drafts[i].tables.get_mut(job.t).and_then(|tb| tb.rows.get_mut(job.r)) {
                if row.sig.is_none() {
                    if row.vals.len() <= job.c {
                        row.vals.resize(job.c + 1, Json::Null);
                    }
                    row.vals[job.c] = Json::Str(link.clone());
                    placed = true;
                }
            }
        }
        if placed {
            self.bump(&job.key, job.t);
            self.save_drafts();
            self.ok(format!("{} is inscribed and linked in the row. Inscribe the row to publish the link.", job.filename));
        } else {
            host::copy(&link);
            self.ok(format!("{} is inscribed ({}); its row changed meanwhile, so the link was copied to your clipboard.", job.filename, link));
        }
        let w = job.wallet.clone();
        self.fetch_balance(&w);
    }

    /// Open an `iq://tx/…` link: IQ's gateway first, Solana as the fallback.
    pub fn open_tx(&mut self, sig: &str, label: &str) {
        if base58::decode(sig).map(|b| b.len()) != Some(64) {
            return self.err("That isn't a transaction signature");
        }
        self.viewer = Some(Viewer { sig: sig.to_string(), label: label.to_string(), state: Load::Loading });
        if self.use_rpc() {
            self.tx_from_chain(sig);
        } else {
            self.get(&format!("/data/{}", sig), P::TxView(sig.to_string(), false));
        }
    }

    fn tx_from_chain(&mut self, sig: &str) {
        let params = json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"maxSupportedTransactionVersion\":1,\"commitment\":\"confirmed\"}}]", sig)).unwrap();
        self.rpc("getTransaction", params, P::TxView(sig.to_string(), true));
    }

    /// Files the account's wallets inscribed, from IQ's gateway.
    pub fn load_files(&mut self) {
        if self.use_rpc() {
            return;
        }
        let Some(a) = self.account.as_ref() else { return };
        for w in a.addresses() {
            if matches!(self.files.get(&w), Some(Load::Loading)) {
                continue;
            }
            self.files.insert(w.clone(), Load::Loading);
            self.get(&format!("/user/{}/assets?limit=100", w), P::Files(w));
        }
    }

    pub fn viewer_download(&mut self) {
        let Some(Load::Ready(v)) = self.viewer.as_ref().map(|v| &v.state) else { return };
        let data: Vec<u8> = match (&v.bytes, &v.text) {
            (Some(b), _) => b.clone(),
            (_, Some(t)) => t.as_bytes().to_vec(),
            _ => return,
        };
        host::download(&v.filename, &v.filetype, &data);
    }

    pub fn attach_async(&mut self, p: P, ok: bool, status: u32, data: Vec<u8>) -> bool {
        let text = String::from_utf8_lossy(&data).into_owned();
        let http_ok = ok && (200..300).contains(&status);
        let res = || if http_ok { net::rpc_result(&text) } else { Err(fetch_err(ok, status, &text)) };
        match p {
            P::AttachCheck(mut job) => {
                let v = match res() {
                    Ok(v) => v,
                    Err(e) => {
                        self.attach_fail(format!("Couldn't read the wallet's state: {}", e));
                        return true;
                    }
                };
                let vals = v.get("value").arr().to_vec();
                let len = |i: usize| vals.get(i).and_then(net::account_data).map(|d| d.len() as u64);
                let exists = |i: usize| vals.get(i).map(|x| !x.is_null()).unwrap_or(false);
                let user = parse_pk(&job.wallet).unwrap();
                let mint = solana::pk(iq::IQ_MINT_STR);
                job.iq_ata = if exists(2) {
                    Some(iq::ata(&user, &mint, &solana::pk(iq::TOKEN_PROGRAM_STR)))
                } else if exists(3) {
                    Some(iq::ata(&user, &mint, &solana::pk(iq::TOKEN_2022_STR)))
                } else {
                    None
                };
                let bal = vals.get(5).and_then(|x| x.get("lamports").u64()).unwrap_or(0);
                if !job.legacy && !vals.get(4).map(iq::v1_active).unwrap_or(false) {
                    self.attach_fail(format!(
                        "{} needs a v1 transaction (it's over {} encoded), and v1 transactions aren't active on {} yet. Files up to about 500 bytes work now.",
                        job.filename,
                        ui::bytes(iq::INLINE_CAP_LEGACY),
                        self.settings.cluster
                    ));
                    return true;
                }
                let write = iq::FEE_DIRECT_WRITE + iq::TX_FEE;
                let (stage, need) = if !exists(0) {
                    (Stage::Init, iq::USER_INIT_RENT_ESTIMATE + write + iq::TX_FEE)
                } else {
                    let mut grow = vec![];
                    if !job.legacy {
                        if len(0).unwrap_or(0) < iq::USER_INVENTORY_SPACE {
                            grow.push((iq::user_inventory_pda(&user), iq::USER_INVENTORY_SPACE));
                        }
                        if exists(1) && len(1).unwrap_or(0) < iq::CODE_ACCOUNT_SPACE {
                            grow.push((iq::code_account_pda(&user), iq::CODE_ACCOUNT_SPACE));
                        }
                    }
                    if grow.is_empty() {
                        (Stage::Write, write)
                    } else {
                        (Stage::Grow(grow), 50_000_000 + write)
                    }
                };
                if bal < need + iq::RENT_FLOOR {
                    self.attach_fail(format!(
                        "The database wallet has {} but this needs about {}{}. Fund it first.",
                        ui::sol(bal),
                        ui::sol(need + iq::RENT_FLOOR),
                        if stage == Stage::Init { " (its first IQ write includes a one-time ~0.062 SOL account setup)" } else { "" }
                    ));
                    return true;
                }
                job.stage = stage;
                self.attach_send(job);
            }
            P::AttachHash(job) => {
                let bh = res().ok().and_then(|r| r.get("value").get("blockhash").str().and_then(base58::decode32));
                let (Some(bh), Some(kp)) = (bh, self.keypair(&job.wallet)) else {
                    self.attach_fail("Couldn't get a blockhash from the RPC");
                    return true;
                };
                let ixs = match &job.stage {
                    Stage::Init => vec![iq::user_initialize(&kp.pubkey)],
                    Stage::Grow(list) => list.iter().map(|(t, n)| iq::realloc_account(&kp.pubkey, t, *n)).collect(),
                    Stage::Write => vec![iq::user_inventory_code_in_inline(&kp.pubkey, &job.metadata, job.iq_ata)],
                };
                let msg = solana::compile(&kp.pubkey, &ixs, bh);
                let v1 = job.stage == Stage::Write && !job.legacy;
                let (raw, _) = if v1 { solana::v1_signed(&msg, &kp.seed) } else { solana::legacy_signed(&msg, &kp.seed) };
                let limit = if v1 { solana::V1_MAX_TX_BYTES } else { solana::LEGACY_MAX_TX_BYTES };
                if raw.len() > limit {
                    self.attach_fail(format!("The transaction is {} bytes, over the {}-byte limit", raw.len(), limit));
                    return true;
                }
                let params = json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"preflightCommitment\":\"confirmed\"}}]", base64_encode(&raw))).unwrap();
                self.rpc("sendTransaction", params, P::AttachSent(job));
                return false;
            }
            P::AttachSent(job) => match res() {
                Ok(r) => {
                    let sig = r.str_or("");
                    let what = match job.stage {
                        Stage::Init => "Wallet setup".to_string(),
                        Stage::Grow(_) => "Account resize".to_string(),
                        Stage::Write => format!("File {}", job.filename),
                    };
                    let now = host::now_ms();
                    self.timer(1200, P::ConfirmTick { what, sig, since: now, after: After::Attach(job) });
                    return false;
                }
                Err(e) => self.attach_fail(format!("The network rejected the transaction (nothing was spent): {}", e)),
            },
            P::TxView(sig, from_chain) => {
                if self.viewer.as_ref().map(|v| v.sig != sig).unwrap_or(true) {
                    return false;
                }
                let state = if from_chain {
                    match res().and_then(|r| viewed_from_tx(&r, &sig)) {
                        Ok(v) => Load::Ready(v),
                        Err(e) => Load::Err(e),
                    }
                } else {
                    match json::parse(&text).ok().filter(|_| http_ok) {
                        Some(v) if !v.get("data").is_null() || !v.get("metadata").is_null() => {
                            let md = parse_meta(v.get("metadata"));
                            let (ft, name, chunks) = meta_fields(&md);
                            let d = match v.get("data") {
                                Json::Str(s) => s.clone(),
                                Json::Null => String::new(),
                                other => other.to_string(),
                            };
                            let time = v.get("blockTime").u64().map(|t| t as i64);
                            let mut out = viewed(&ft, &name, d, v.get("signer").str_or(""), time, "IQ gateway");
                            if out.text.as_deref() == Some("") && out.bytes.is_none() && chunks > 1 {
                                out.note = Some(format!("{} chunks — the gateway didn't return the data; try opening it in IQ's viewer.", chunks));
                            }
                            Load::Ready(out)
                        }
                        _ => {
                            // not in IQ's index (yet): read it from Solana
                            self.tx_from_chain(&sig);
                            return false;
                        }
                    }
                };
                if let Some(v) = self.viewer.as_mut() {
                    v.state = state;
                }
            }
            P::Files(w) => {
                let v = if http_ok {
                    match json::parse(&text) {
                        Ok(v) => {
                            let list = if matches!(v, Json::Arr(_)) { v.arr().to_vec() } else { v.get("assets").arr().to_vec() };
                            Load::Ready(
                                list.iter()
                                    .filter(|a| a.get("err").is_null())
                                    .filter_map(|a| {
                                        let md = parse_meta(a.get("metadata"));
                                        let (filetype, filename, chunks) = meta_fields(&md);
                                        // "<n>.bin" octet-streams with no name are table writes, listed with the databases
                                        let default_name = filename.ends_with(".bin") && filename[..filename.len() - 4].bytes().all(|c| c.is_ascii_digit());
                                        if default_name && filetype == "application/octet-stream" {
                                            return None;
                                        }
                                        Some(Asset { sig: a.get("signature").str_or(""), filename, filetype, time: a.get("blockTime").u64().map(|t| t as i64), chunks })
                                    })
                                    .collect(),
                            )
                        }
                        Err(e) => Load::Err(e),
                    }
                } else {
                    Load::Err(fetch_err(ok, status, &text))
                };
                self.files.insert(w, v);
            }
            _ => return false,
        }
        true
    }
}

/// `iq://tx/<sig>#<name>` → (sig, name)
pub fn parse_tx_link(s: &str) -> Option<(String, String)> {
    let rest = s.strip_prefix("iq://tx/")?;
    let (sig, name) = match rest.split_once('#') {
        Some((a, b)) => (a, pct_decode(b)),
        None => (rest, String::new()),
    };
    (base58::decode(sig).map(|b| b.len()) == Some(64)).then(|| (sig.to_string(), name))
}
