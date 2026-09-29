//! Files in table cells, and opening them again.
//!
//! A file is inscribed on its own with IQ's `codeIn`, like the SDK does it,
//! from the database's wallet: a small one in one `user_inventory_code_in`
//! (text stored as text, binary as base64); a bigger one in parts — a linked
//! list of `send_code` transactions below 10 parts, a session of
//! `post_chunk`s from 10 — finalized by a `user_inventory_code_in` that
//! points at them. The cell then holds an
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
    /// Part `i` of a file sent in parts.
    Chunk(usize),
    /// The rest of a session's parts, several at a time (upload.rs).
    Batch,
    /// The code-in that points at the parts.
    Final,
    /// A crowdfunded piece: the row in the project's table saying where it is.
    Register,
}

/// A piece of a crowdfunded file (crowd.rs) being uploaded as an IQ file.
#[derive(Clone, Debug)]
pub struct CrowdPiece {
    pub pda: String,
    pub piece: usize,
    pub sha256: String,
    pub db_id: String,
    /// The table's name in its database (its seed).
    pub table: String,
    /// Signature of the IQ file holding the piece, once written.
    pub piece_sig: Option<String>,
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
    /// Already topped up from the main balance once.
    pub funded: bool,
    /// The file's content as stored (text, or base64 for binary files).
    pub data: String,
    pub filetype: String,
    /// Parts, when the file is too big for one transaction.
    pub chunks: Vec<String>,
    /// Session sequence number (None = linked list).
    pub seq: Option<u64>,
    pub session_exists: Option<bool>,
    /// Signature of the last part sent.
    pub last: Option<String>,
    /// Set for a piece of a crowdfunded file (not a cell).
    pub crowd: Option<Box<CrowdPiece>>,
}

impl Job {
    pub fn cell(&self) -> String {
        match &self.crowd {
            Some(c) => format!("crowd:{}:{}", c.pda, c.piece),
            None => format!("{}:{}:{}", self.key, self.t, self.r),
        }
    }
}

/// Biggest file that goes into a cell (after base64 for binary files).
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

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
    !t.is_empty() && t.len().is_multiple_of(4) && t.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'=')
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
    // a file of an IQ git commit: base64 bytes stored as octet-stream
    if let Some(path) = filename.strip_prefix("iqgit-blob:") {
        v.filename = path.to_string();
        v.filetype = crate::git::filetype_of(path).to_string();
        if let Some(b) = base64_decode(data.trim()) {
            let img = inline_image(&v.filetype) && v.filetype != "image/svg+xml";
            match String::from_utf8(b) {
                Ok(t) if !img && !t.contains('\0') => v.text = Some(t),
                Ok(t) => v.bytes = Some(t.into_bytes()),
                Err(e) => {
                    if v.filetype.starts_with("text/") {
                        v.filetype = "application/octet-stream".into();
                    }
                    v.bytes = Some(e.into_bytes());
                }
            }
            return v;
        }
    }
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
        if self.attach_status.is_some() || self.crowd.work.as_ref().map(|w| !w.finished).unwrap_or(false) {
            self.err("Another file is still being inscribed — wait for it to finish.");
            return;
        }
        let ghost = self.drafts[i].tables.get(t).and_then(|tb| tb.rows.get(r)).map(|x| x.sig.is_none() && !x.deleted).unwrap_or(false);
        if !ghost {
            self.err("Files can only go into rows that aren't saved yet.");
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
            self.err("Sign in with the wallet that owns this database.");
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
        if data.len() > MAX_FILE_BYTES {
            return self.err(format!("{} is {} once encoded; files up to {} can go into a cell here.", name, ui::bytes(data.len()), ui::bytes(MAX_FILE_BYTES)));
        }
        let legacy_only = self.settings.tx_format == TxFormat::Legacy;
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
            funded: false,
            data,
            filetype: ftype,
            chunks: vec![],
            seq: None,
            session_exists: None,
            last: None,
            crowd: None,
        };
        if let Some(m) = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address()) {
            self.fetch_balance(&m);
        }
        self.attach_check(job);
    }

    /// Upload a piece of a crowdfunded file from `wallet`, then record it in
    /// the project's table.
    pub fn attach_crowd_piece(&mut self, wallet: String, filename: String, bytes: &[u8], piece: CrowdPiece) {
        if self.attach_status.is_some() {
            return self.crowd_piece_failed("Another file is being inscribed from this tab — try again when it's done.");
        }
        if self.keypair(&wallet).is_none() {
            return self.crowd_piece_failed("Sign in with your wallet to upload pieces.");
        }
        let data = base64_encode(bytes);
        let ftype = "application/octet-stream".to_string();
        let metadata = iq::file_metadata(&ftype, &filename, &data);
        let legacy_only = self.settings.tx_format == TxFormat::Legacy;
        let job = Job {
            key: String::new(),
            t: 0,
            r: 0,
            c: 0,
            wallet,
            filename,
            legacy: legacy_only || metadata.len() <= iq::INLINE_CAP_LEGACY,
            metadata,
            iq_ata: None,
            stage: Stage::Write,
            funded: false,
            data,
            filetype: ftype,
            chunks: vec![],
            seq: None,
            session_exists: None,
            last: None,
            crowd: Some(Box::new(piece)),
        };
        self.attach_check(job);
    }

    fn attach_status_set(&mut self, job: &Job, msg: &str) {
        self.attach_status = Some((job.cell(), msg.to_string()));
    }

    pub fn attach_fail(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        let crowd = self.attach_status.as_ref().map(|(c, _)| c.starts_with("crowd:")).unwrap_or(false);
        self.attach_status = None;
        if crowd {
            self.crowd_piece_failed(&msg);
        } else {
            self.err(msg);
        }
    }

    fn attach_check(&mut self, job: Job) {
        let Some(user) = parse_pk(&job.wallet) else {
            // (signed out mid-way)
            self.attach_status = Some((job.cell(), String::new()));
            return self.attach_fail("Sign in with the wallet that owns this database.");
        };
        let mint = solana::pk(iq::IQ_MINT_STR);
        let addrs = [
            iq::user_inventory_pda(&user),
            iq::code_account_pda(&user),
            iq::ata(&user, &mint, &solana::pk(iq::TOKEN_PROGRAM_STR)),
            iq::ata(&user, &mint, &solana::pk(iq::TOKEN_2022_STR)),
            solana::pk(iq::TX_V1_FEATURE_GATE_STR),
            user,
            iq::user_state_pda(&user),
        ];
        let params = Json::Arr(vec![
            Json::Arr(addrs.iter().map(|a| json::s(&b58(a))).collect()),
            json::obj(vec![("encoding", json::s("base64")), ("commitment", json::s("confirmed"))]),
        ]);
        self.attach_status_set(&job, &format!("Preparing {}…", job.filename));
        self.rpc("getMultipleAccounts", params, P::AttachCheck(job));
    }

    fn attach_send(&mut self, job: Job) {
        if job.stage == Stage::Batch {
            return self.attach_batch_start(job);
        }
        let msg = match &job.stage {
            Stage::Init => "Setting up the wallet's IQ account (one time)…".to_string(),
            Stage::Grow(_) => "Enlarging the wallet's IQ accounts for 4 KB writes (one time)…".to_string(),
            Stage::Write => format!("Inscribing {}…", job.filename),
            Stage::Chunk(i) => format!("Inscribing {} — part {} of {}…", job.filename, i + 1, job.chunks.len()),
            Stage::Batch => format!("Inscribing {}…", job.filename),
            Stage::Final => format!("Finishing {}…", job.filename),
            Stage::Register => "Recording the piece in the table…".to_string(),
        };
        self.attach_status_set(&job, &msg);
        let params = json::parse("[{\"commitment\":\"confirmed\"}]").unwrap();
        self.rpc("getLatestBlockhash", params, P::AttachHash(job));
    }

    pub fn attach_retry(&mut self, job: Job) {
        self.attach_check(job);
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
                // the next step may need its own top-up
                job.funded = false;
                self.attach_check(job) // re-read sizes (pre-upgrade accounts need growing)
            }
            Stage::Grow(_) => {
                job.stage = if job.chunks.is_empty() { Stage::Write } else { Stage::Chunk(0) };
                self.attach_send(job);
            }
            Stage::Write | Stage::Final if job.crowd.is_some() => {
                // the piece is on chain; now say where it is
                if let Some(c) = job.crowd.as_mut() {
                    c.piece_sig = Some(sig.to_string());
                }
                job.stage = Stage::Register;
                self.attach_send(job);
            }
            Stage::Register => {
                self.attach_status = None;
                let Some(c) = job.crowd else { return };
                let tx = c.piece_sig.clone().unwrap_or_default();
                if self.settings.notify_gateway {
                    let row = crate::pack::row_json(&crate::crowd::registration_payload(c.piece, &c.sha256, &tx));
                    let body = format!("{{\"txSignature\":\"{}\",\"signer\":\"{}\",\"row\":{}}}", sig, job.wallet, row);
                    let id = self.nid();
                    self.pending.insert(id, P::Ignore);
                    let url = format!("{}/table/{}/notify", self.gateway_url(), c.pda);
                    host::fetch(id, "POST", &url, &body, "application/json");
                }
                self.crowd_piece_done(c.piece, c.sha256, tx, job.wallet.clone());
            }
            Stage::Write | Stage::Final => self.attach_done(job, sig),
            Stage::Chunk(i) => {
                job.last = Some(sig.to_string());
                job.session_exists = Some(true);
                job.stage = match (i + 1 < job.chunks.len(), job.seq.is_some()) {
                    // a session's other parts go out together
                    (true, true) => Stage::Batch,
                    (true, false) => Stage::Chunk(i + 1),
                    (false, _) => Stage::Final,
                };
                self.attach_send(job);
            }
            Stage::Batch => {}
        }
    }

    fn attach_batch_start(&mut self, job: Job) {
        let (Some(seq), Some(kp)) = (job.seq, self.keypair(&job.wallet)) else {
            return self.attach_fail("Sign in with the wallet that owns this database.");
        };
        let same = self.uploads.get("attach").map(|b| b.kp.pubkey == kp.pubkey && b.same_upload(seq, &job.chunks)).unwrap_or(false);
        if same {
            // funded again after running short: continue where it stopped
            self.up_resume("attach");
            return;
        }
        let (legacy, chunks) = (job.legacy, job.chunks.clone());
        let speed = self.settings.upload_speed.clone();
        let b = crate::upload::Batch::new(crate::upload::Owner::Attach(Box::new(job)), kp, legacy, seq, chunks, 1, &speed);
        self.up_begin("attach", b);
    }

    pub fn attach_batch_done(&mut self, mut job: Job) {
        job.stage = Stage::Final;
        self.attach_send(job);
    }

    pub fn attach_batch_failed(&mut self, msg: String) {
        self.attach_fail(msg);
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
            self.ok(format!("{} is saved on the blockchain and linked in the cell. Save the table to keep the link.", job.filename));
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
        let params =
            json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"maxSupportedTransactionVersion\":1,\"commitment\":\"confirmed\"}}]", sig)).unwrap();
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
                // v1 (4 KB) transactions when the cluster has them, like the SDK
                let v1 = self.settings.tx_format != TxFormat::Legacy && vals.get(4).map(iq::v1_active).unwrap_or(false);
                let cap = if v1 { iq::INLINE_CAP_V1 } else { iq::INLINE_CAP_LEGACY };
                job.legacy = !v1 || job.metadata.len() <= iq::INLINE_CAP_LEGACY;
                if job.metadata.len() > cap {
                    // too big for one transaction: IQ's chunked upload, as the SDK splits it
                    if job.chunks.is_empty() {
                        job.chunks = iq::to_chunks(&job.data, if v1 { iq::CHUNK_SIZE_V1 } else { iq::CHUNK_SIZE_LEGACY });
                    }
                    if job.chunks.len() >= iq::LINKED_LIST_THRESHOLD && job.seq.is_none() {
                        job.seq = Some(vals.get(6).and_then(net::account_data).and_then(|d| iq::decode_user_state_seq(&d)).unwrap_or(0));
                    }
                } else {
                    job.chunks.clear();
                }
                let write = match job.chunks.len() {
                    0 => iq::FEE_DIRECT_WRITE + iq::TX_FEE,
                    n => crate::pack::write_cost(n),
                } + if job.crowd.is_some() { iq::FEE_DIRECT_WRITE + iq::TX_FEE } else { 0 };
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
                    let first = match (&job.stage, job.chunks.is_empty()) {
                        (Stage::Register, _) => Stage::Register,
                        (Stage::Chunk(i), false) => Stage::Chunk(*i),
                        (Stage::Batch, false) => Stage::Batch,
                        (Stage::Final, false) => Stage::Final,
                        (_, false) => Stage::Chunk(0),
                        _ => Stage::Write,
                    };
                    if grow.is_empty() {
                        (first, write)
                    } else {
                        (Stage::Grow(grow), 50_000_000 + write)
                    }
                };
                if bal < need + iq::RENT_FLOOR {
                    // pay from the main balance automatically, like saving does
                    let main = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address()).filter(|m| m != &job.wallet);
                    let main_bal = main.as_ref().and_then(|m| self.balances.get(m)).and_then(|b| b.ready().copied()).unwrap_or(0);
                    let short = need + iq::RENT_FLOOR + need / 10 - bal;
                    if let (Some(m), false) = (main, job.funded) {
                        if main_bal >= short + iq::TX_FEE + iq::RENT_FLOOR {
                            job.stage = stage;
                            job.funded = true;
                            self.attach_status_set(&job, "Moving SOL from your balance…");
                            let w = job.wallet.clone();
                            self.transfer(&m, &w, short, After::AttachFunded(job));
                            return true;
                        }
                    }
                    self.attach_fail(format!(
                        "{} has {} but this needs about {}{}. Fund it first.",
                        if job.crowd.is_some() { "Your balance" } else { "The database wallet" },
                        ui::sol(bal),
                        ui::sol(need + iq::RENT_FLOOR),
                        if stage == Stage::Init { " (its first IQ write includes a one-time ~0.05 SOL account setup)" } else { "" }
                    ));
                    return true;
                }
                job.stage = stage;
                // an interrupted session is continued rather than re-created
                if let (Some(seq), None, Stage::Chunk(0)) = (job.seq, job.session_exists, &job.stage) {
                    let sess = iq::session_pda(&user, seq);
                    let params = json::parse(&format!("[\"{}\",{{\"encoding\":\"base64\",\"commitment\":\"confirmed\"}}]", b58(&sess))).unwrap();
                    self.rpc("getAccountInfo", params, P::AttachSession(job));
                    return true;
                }
                self.attach_send(job);
            }
            P::AttachSession(mut job) => {
                job.session_exists = Some(res().map(|v| !v.get("value").is_null()).unwrap_or(false));
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
                    Stage::Batch => return false,
                    Stage::Register => {
                        let Some(c) = job.crowd.as_ref() else { return false };
                        let payload = crate::crowd::registration_payload(c.piece, &c.sha256, c.piece_sig.as_deref().unwrap_or(""));
                        let md = iq::inline_metadata(0, &crate::pack::row_json(&payload));
                        vec![iq::db_code_in_inline(&kp.pubkey, c.db_id.as_bytes(), &iq::seed_bytes(&c.table), &md, job.iq_ata)]
                    }
                    Stage::Chunk(i) => match job.seq {
                        None => vec![iq::send_code(&kp.pubkey, &job.chunks[*i], job.last.as_deref().filter(|_| *i > 0).unwrap_or("Genesis"))],
                        Some(seq) => {
                            let mut v = vec![];
                            if *i == 0 && job.session_exists != Some(true) {
                                v.push(iq::create_session(&kp.pubkey, seq));
                            }
                            v.push(iq::post_chunk(&kp.pubkey, seq, *i as u32, &job.chunks[*i]));
                            v
                        }
                    },
                    Stage::Final => {
                        let path = match job.seq {
                            Some(seq) => iq::ChunkPath::Session { seq, total: job.chunks.len() as u32 },
                            None => iq::ChunkPath::Linked(job.last.clone().unwrap_or_default()),
                        };
                        vec![iq::user_inventory_code_in(&kp.pubkey, &path, &iq::chunked_metadata(&job.filetype, &job.filename, job.chunks.len()), None)]
                    }
                };
                let msg = solana::compile(&kp.pubkey, &ixs, bh);
                let v1 = matches!(job.stage, Stage::Write | Stage::Chunk(_) | Stage::Final) && !job.legacy;
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
                        Stage::Write | Stage::Final => format!("File {}", job.filename),
                        Stage::Register => format!("Record of {}", job.filename),
                        Stage::Chunk(i) => format!("Part {} of {}", i + 1, job.filename),
                        Stage::Batch => format!("Parts of {}", job.filename),
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
                                        Some(Asset {
                                            sig: a.get("signature").str_or(""),
                                            filename,
                                            filetype,
                                            time: a.get("blockTime").u64().map(|t| t as i64),
                                            chunks,
                                        })
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
