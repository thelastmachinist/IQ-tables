//! Crowdfunded uploads: a big file put on the blockchain by many people.
//!
//! The organizer publishes the file's *manifest* — its name, size, SHA-256,
//! and the SHA-256 of each piece — in the structure record of an IQ table
//! that anyone may add rows to. Anyone can then upload a piece as an ordinary
//! IQ file (IQ's chunked upload, from their own wallet) and add a row to the
//! table saying which piece it is and where it is. Readers use the first
//! manifest the organizer published and accept a piece only if its bytes hash
//! to the manifest's value — so it doesn't matter who uploaded what, only
//! that the data matches what the organizer first described.

use std::collections::VecDeque;

use crate::app::{fetch_err, App, P};
use crate::crypto::{base64_decode, hex, sha2::Sha256};
use crate::host;
use crate::iq;
use crate::json::{self, Json};
use crate::pack::{self, Record, Schema, SourcePack};
use crate::ui::{self, esc};

pub const SMALL_PIECE: u64 = 1 << 20;
pub const BIG_PIECE: u64 = 4 << 20;
pub const MAX_SIZE: u64 = 8 << 30;
/// Piece sizes a manifest may use (IQ Tables makes 1 MB and 4 MB ones).
const MIN_PIECE: u64 = 64 << 10;
const MAX_PIECE: u64 = 8 << 20;

/// Pieces of 1 MB for files up to 16 MB (so even small files have several
/// pieces to share out), 4 MB above that (fewer uploads, so fewer IQ fees).
pub fn piece_size(total: u64) -> u64 {
    if total <= 16 << 20 {
        SMALL_PIECE
    } else {
        BIG_PIECE
    }
}

/// Storage keys of a registration record: which piece, its hash, and the
/// signature of the IQ file holding it.
pub const REG_COLS: [&str; 4] = ["id", "piece", "sha256", "tx"];

#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub size: u64,
    pub ftype: String,
    pub sha256: String,
    pub piece: u64,
    pub hashes: Vec<String>,
    /// Where contributors' browsers can read the file (optional).
    pub source: String,
    pub note: String,
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

impl Manifest {
    pub fn to_json(&self) -> Json {
        let mut o = json::obj(vec![
            ("v", json::n(1)),
            ("name", json::s(&self.name)),
            ("size", json::n(self.size)),
            ("type", json::s(&self.ftype)),
            ("sha256", json::s(&self.sha256)),
            ("piece", json::n(self.piece)),
            ("hashes", Json::Arr(self.hashes.iter().map(|h| json::s(h)).collect())),
        ]);
        if !self.source.is_empty() {
            o.set("source", json::s(&self.source));
        }
        if !self.note.is_empty() {
            o.set("note", json::s(&self.note));
        }
        o
    }

    pub fn from_json(v: &Json) -> Option<Manifest> {
        let m = Manifest {
            name: v.get("name").str_or("file"),
            size: v.get("size").u64()?,
            ftype: v.get("type").str().filter(|t| !t.is_empty()).unwrap_or("application/octet-stream").to_string(),
            sha256: v.get("sha256").str()?.to_ascii_lowercase(),
            piece: v.get("piece").u64().filter(|p| *p > 0)?,
            hashes: v.get("hashes").arr().iter().filter_map(|h| h.str().map(|s| s.to_ascii_lowercase())).collect(),
            // only a web address is ever used as a source
            source: v.get("source").str().filter(|u| u.starts_with("https://") || u.starts_with("http://")).unwrap_or("").to_string(),
            note: v.get("note").str_or(""),
        };
        // pieces a browser can hold (a manifest is anyone's JSON)
        let sane = (MIN_PIECE..=MAX_PIECE).contains(&m.piece) && m.size <= MAX_SIZE;
        let ok = sane && m.size > 0 && is_hash(&m.sha256) && m.hashes.len() as u64 == m.size.div_ceil(m.piece) && m.hashes.iter().all(|h| is_hash(h));
        ok.then_some(m)
    }

    pub fn count(&self) -> usize {
        self.hashes.len()
    }

    /// (start, length) of piece `i`.
    pub fn range(&self, i: usize) -> (u64, u64) {
        let start = i as u64 * self.piece;
        (start, self.piece.min(self.size - start))
    }
}

#[derive(Clone, Debug)]
pub struct Reg {
    pub piece: usize,
    pub sha256: String,
    pub tx: String,
    pub signer: String,
    pub time: Option<i64>,
}

pub struct Scan {
    pub manifest: Manifest,
    pub organizer: String,
    /// A later record from the organizer describes a different file (ignored).
    pub changed: bool,
    pub regs: Vec<Reg>,
}

impl Scan {
    /// Registrations that claim the right hash, per piece, oldest first
    /// (leaving out copies already found not to match).
    pub fn candidates(&self, extra: &[Reg], bad: &[String]) -> Vec<Vec<Reg>> {
        let mut out: Vec<Vec<Reg>> = vec![vec![]; self.manifest.count()];
        for r in self.regs.iter().chain(extra.iter()) {
            if r.piece < out.len() && r.sha256 == self.manifest.hashes[r.piece] && !bad.contains(&r.tx) && !out[r.piece].iter().any(|x| x.tx == r.tx) {
                out[r.piece].push(r.clone());
            }
        }
        out
    }
}

/// Read a crowdfunded table from its packs (oldest first).
pub fn scan(packs: &[SourcePack], official: &dyn Fn(&str) -> bool) -> Option<Scan> {
    let mut first: Option<(Manifest, String)> = None;
    let mut changed = false;
    let mut regs = vec![];
    for p in packs {
        if let Some(meta) = &p.meta {
            if !official(&p.signer) {
                continue;
            }
            if let Some(m) = Manifest::from_json(meta.get("crowd")) {
                match &first {
                    None => first = Some((m, p.signer.clone())),
                    Some((f, _)) if *f != m => changed = true,
                    _ => {}
                }
            }
            continue;
        }
        let pos = |k: &str| p.schema.cols.iter().position(|c| c == k);
        let (Some(pi), Some(ph), Some(pt)) = (pos("piece"), pos("sha256"), pos("tx")) else { continue };
        for r in p.recs.iter().filter(|r| !r.deleted) {
            let (Some(piece), Some(sha), Some(tx)) =
                (r.vals.get(pi).and_then(|v| v.u64()), r.vals.get(ph).and_then(|v| v.str()), r.vals.get(pt).and_then(|v| v.str()))
            else {
                continue;
            };
            regs.push(Reg { piece: piece as usize, sha256: sha.to_ascii_lowercase(), tx: tx.to_string(), signer: p.signer.clone(), time: p.time });
        }
    }
    let (manifest, organizer) = first?;
    Some(Scan { manifest, organizer, changed, regs })
}

/// The pack a contributor writes to say "piece `i` is in transaction `tx`".
pub fn registration_payload(piece: usize, sha256: &str, tx: &str) -> String {
    let schema = Schema { cols: REG_COLS.iter().map(|c| c.to_string()).collect(), id: 0 };
    let id = format!("{}.{}", piece, tx.chars().take(10).collect::<String>());
    let rec = Record { vals: vec![json::s(&id), json::n(piece), json::s(sha256), json::s(tx)], deleted: false };
    pack::encode_payload(&schema, &[rec], false)
}

/// Estimated cost of uploading a piece of `len` bytes and recording it.
pub fn piece_cost(len: u64, legacy: bool) -> u64 {
    let b64 = len.div_ceil(3) * 4;
    let cap = if legacy { iq::INLINE_CAP_LEGACY } else { iq::INLINE_CAP_V1 } as u64;
    let write = if b64 + 120 <= cap {
        iq::FEE_DIRECT_WRITE + iq::TX_FEE
    } else {
        let chunk = if legacy { iq::CHUNK_SIZE_LEGACY } else { iq::CHUNK_SIZE_V1 } as u64;
        pack::write_cost(b64.div_ceil(chunk) as usize)
    };
    write + iq::FEE_DIRECT_WRITE + iq::TX_FEE
}

/// Cost of uploading all of `m`'s pieces.
pub fn total_cost(m: &Manifest, legacy: bool) -> u64 {
    (0..m.count()).map(|i| piece_cost(m.range(i).1, legacy)).sum()
}

/// Does this explorer table look like a crowdfunded one (read all of it)?
pub fn looks_crowd(packs: &[&SourcePack]) -> bool {
    packs.iter().any(|p| match &p.meta {
        Some(m) => !m.get("crowd").is_null(),
        None => ["piece", "sha256", "tx"].iter().all(|k| p.schema.cols.iter().any(|c| c == k)),
    })
}

fn hex32(h: [u8; 32]) -> String {
    hex(&h)
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex32(crate::crypto::sha2::sha256(b))
}

/// A file chosen in this tab (kept by the page, read a piece at a time).
#[derive(Clone, Debug)]
pub struct LocalFile {
    pub fid: u32,
    pub name: String,
    pub size: u64,
    pub ftype: String,
}

/// The organizer's file being fingerprinted.
pub struct Prep {
    pub key: String,
    pub file: LocalFile,
    pub piece: u64,
    pub hashes: Vec<String>,
    whole: Option<Sha256>,
    pub sha256: String,
    pub err: Option<String>,
}

impl Prep {
    pub fn count(&self) -> usize {
        self.file.size.div_ceil(self.piece) as usize
    }
    pub fn done(&self) -> bool {
        !self.sha256.is_empty()
    }
}

pub enum Src {
    File(LocalFile),
    Url(String),
}

/// Pieces this browser is uploading.
pub struct Work {
    pub pda: String,
    pub manifest: Manifest,
    pub db_id: String,
    pub table: String,
    pub src: Src,
    pub queue: VecDeque<usize>,
    pub total: usize,
    pub done: usize,
    pub current: Option<usize>,
    pub err: Option<String>,
    pub stop: bool,
    pub finished: bool,
}

/// A download being put together and checked.
pub struct Dl {
    pub pda: String,
    pub manifest: Manifest,
    cands: Vec<Vec<Reg>>,
    pub i: usize,
    k: usize,
    bid: u32,
    whole: Option<Sha256>,
    pub skipped: usize,
    /// Attempts at reading the current copy (network hiccups are retried).
    tries: u32,
    /// A copy of the current piece couldn't be read at all.
    unreadable: Option<String>,
    pub err: Option<String>,
    pub done: bool,
}

#[derive(Default)]
pub struct Crowd {
    pub files: Vec<LocalFile>,
    pub prep: Option<Prep>,
    pub work: Option<Work>,
    pub dl: Option<Dl>,
    /// Pieces recorded from this tab (shown before IQ's gateway lists them).
    pub just: Vec<(String, Reg)>,
    /// Recorded copies whose bytes didn't match (found while downloading):
    /// their pieces count as missing again, so someone can upload them.
    pub bad: Vec<String>,
}

fn safe_table_name(file: &str) -> String {
    let stem = file.rsplit_once('.').map(|(a, _)| a).filter(|a| !a.is_empty()).unwrap_or(file);
    let mut s: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s = s.trim_matches('_').chars().take(28).collect::<String>();
    if s.is_empty() || s.as_bytes()[0].is_ascii_digit() {
        format!("file_{}", s)
    } else {
        s
    }
}

impl App {
    /// The crowdfunded view of the open explorer table, if it is one — only
    /// once its whole history is read, since the first manifest is what counts.
    pub fn crowd_scan(&self) -> Option<Scan> {
        let tv = self.table.as_ref()?;
        if !tv.done || tv.cut {
            return None;
        }
        let creator = tv.creator.clone()?;
        let packs: Vec<SourcePack> = tv.decoded.iter().rev().filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok())).cloned().collect();
        scan(&packs, &|s| s == creator)
    }

    fn crowd_extra(&self, pda: &str) -> Vec<Reg> {
        self.crowd.just.iter().filter(|(p, _)| p == pda).map(|(_, r)| r.clone()).collect()
    }

    pub fn crowd_event(&mut self, kind: &str, action: &str, arg: &str, val: &str) -> bool {
        match (kind, action) {
            ("file", "crowd-new") => self.crowd_new(arg, val),
            ("file", "crowd-have") => {
                if let Some(f) = self.crowd_file(val) {
                    let want = self.crowd_scan().map(|s| s.manifest.size);
                    if want.is_some() && want != Some(f.size) {
                        self.err(format!(
                            "{} is {}, but the file being uploaded is {} — it isn't the same file.",
                            f.name,
                            ui::bytes(f.size),
                            ui::bytes(want.unwrap_or(0))
                        ));
                    } else {
                        self.ok(format!("Using {} — each piece is checked against its fingerprint before it's uploaded.", f.name));
                    }
                }
            }
            ("click", "crowd-create") => self.crowd_create(arg),
            ("click", "crowd-cancel") => self.crowd.prep = None,
            ("click", "crowd-go") => self.crowd_go(arg),
            ("click", "crowd-stop") => {
                if let Some(w) = self.crowd.work.as_mut() {
                    w.stop = true;
                }
            }
            ("click", "crowd-dismiss") => {
                if self.crowd.work.as_ref().map(|w| w.finished).unwrap_or(false) {
                    self.crowd.work = None;
                }
                if self.crowd.dl.as_ref().map(|d| d.done || d.err.is_some()).unwrap_or(false) {
                    self.crowd.dl = None;
                }
            }
            ("click", "crowd-dl") => self.crowd_download(),
            _ => return false,
        }
        true
    }

    fn crowd_file(&mut self, val: &str) -> Option<LocalFile> {
        let v = json::parse(val).ok()?;
        let f = LocalFile {
            fid: v.get("fid").u64()? as u32,
            name: v.get("name").str_or("file"),
            size: v.get("size").u64()?,
            ftype: v.get("type").str_or("application/octet-stream"),
        };
        self.crowd.files.retain(|x| !(x.size == f.size && x.name == f.name));
        self.crowd.files.push(f.clone());
        Some(f)
    }

    // ------------------------------------------------ organizer: new project

    fn crowd_new(&mut self, key: &str, val: &str) {
        let Some(f) = self.crowd_file(val) else { return self.err("Couldn't open that file") };
        if f.size == 0 || f.size > MAX_SIZE {
            return self.err(format!("Files from 1 byte to {} can be crowdfunded here.", ui::bytes(MAX_SIZE)));
        }
        if self.crowd.prep.as_ref().map(|p| !p.done() && p.err.is_none()).unwrap_or(false) {
            return self.err("Still fingerprinting the other file — wait for it or cancel it first.");
        }
        let piece = piece_size(f.size);
        let name = safe_table_name(&f.name);
        self.form.insert("crowd:name".into(), name);
        self.crowd.prep = Some(Prep { key: key.to_string(), file: f, piece, hashes: vec![], whole: Some(Sha256::new()), sha256: String::new(), err: None });
        self.crowd_prep_next();
    }

    fn crowd_prep_next(&mut self) {
        let Some(p) = self.crowd.prep.as_ref() else { return };
        let i = p.hashes.len() as u64;
        let (fid, start, len) = (p.file.fid, i * p.piece, p.piece.min(p.file.size - i * p.piece));
        let id = self.nid();
        self.pending.insert(id, P::CrowdHash(fid, i as usize));
        host::file_read(id, fid, start, len);
    }

    fn crowd_create(&mut self, key: &str) {
        let Some(p) = self.crowd.prep.as_ref().filter(|p| p.done() && p.key == key) else { return };
        let Some(di) = self.draft_idx(key) else { return };
        let name = self.form.get("crowd:name").cloned().unwrap_or_default().trim().to_string();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.len() > 32 {
            return self.err("Name the table with letters, numbers and _ (up to 32).");
        }
        if self.drafts[di].tables.iter().any(|t| t.name.eq_ignore_ascii_case(&name) || t.title.eq_ignore_ascii_case(&name)) {
            return self.err(format!("This database already has a table called {}.", name));
        }
        let source = self.form.get("crowd:source").cloned().unwrap_or_default().trim().to_string();
        if !source.is_empty() && !source.starts_with("https://") && !source.starts_with("http://") {
            return self.err("The source should be a web address (https://…), or left empty.");
        }
        let m = Manifest {
            name: p.file.name.clone(),
            size: p.file.size,
            ftype: p.file.ftype.clone(),
            sha256: p.sha256.clone(),
            piece: p.piece,
            hashes: p.hashes.clone(),
            source,
            note: self.form.get("crowd:note").cloned().unwrap_or_default().trim().to_string(),
        };
        use crate::schema::{ColMeta, IntKind, Ty};
        let cols = vec![
            ("id".to_string(), ColMeta { not_null: true, ..ColMeta::typed("id", Ty::Varchar(40)) }),
            ("piece".to_string(), ColMeta::typed("piece", Ty::Int(IntKind::Int, true))),
            ("sha256".to_string(), ColMeta::typed("sha256", Ty::Varchar(64))),
            ("tx".to_string(), ColMeta::typed("tx", Ty::Varchar(100))),
        ];
        let mut tb = crate::state::DraftTable::typed(&name, cols, 0);
        tb.open = true;
        tb.compress = false;
        tb.keys.comment = format!("Crowdfunded upload of {} ({})", m.name, ui::bytes(m.size));
        tb.crowd = Some(m.to_json());
        self.drafts[di].tables.push(tb);
        let t = self.drafts[di].tables.len() - 1;
        self.bump(key, t);
        self.save_drafts();
        self.crowd.prep = None;
        for k in ["crowd:name", "crowd:source", "crowd:note"] {
            self.form.remove(k);
        }
        self.ok(format!("Table {} is ready. Press Save to publish its fingerprints; then anyone can upload pieces from the table's page.", name));
    }

    // ------------------------------------------------ contributors

    fn crowd_go(&mut self, how: &str) {
        let Some(s) = self.crowd_scan() else { return };
        let Some(tv) = self.table.as_ref() else { return };
        if self.crowd.work.as_ref().map(|w| !w.finished).unwrap_or(false) || self.attach_status.is_some() {
            return self.err("Pieces are already being uploaded from this tab.");
        }
        if self.account.as_ref().and_then(|a| a.main()).is_none() {
            return self.err("Sign in first — pieces are paid for from your balance.");
        }
        let (Some(db_id), Some(table)) = (tv.db_id.clone(), tv.label.clone()) else {
            return self.err("Still reading this table's database — try again in a moment.");
        };
        let pda = tv.pda.clone();
        let m = s.manifest.clone();
        let src = match how {
            "url" if !m.source.is_empty() => Src::Url(m.source.clone()),
            _ => match self.crowd.files.iter().rev().find(|f| f.size == m.size) {
                Some(f) => Src::File(f.clone()),
                None => return self.err("Choose your copy of the file first (it's checked piece by piece)."),
            },
        };
        let extra = self.crowd_extra(&pda);
        let cands = s.candidates(&extra, &self.crowd.bad);
        let mut missing: Vec<usize> = (0..m.count()).filter(|&i| cands[i].is_empty()).collect();
        if missing.is_empty() {
            return self.ok("Every piece is already on the blockchain.");
        }
        // different browsers pick different pieces
        let mut rnd = vec![0u8; missing.len() * 4];
        host::random(&mut rnd);
        for i in (1..missing.len()).rev() {
            let r = u32::from_le_bytes([rnd[i * 4], rnd[i * 4 + 1], rnd[i * 4 + 2], rnd[i * 4 + 3]]) as usize % (i + 1);
            missing.swap(i, r);
        }
        let (_, want) = pick_count(self.form.get("crowd:count").map(|s| s.as_str()), missing.len());
        missing.truncate(want);
        let total = missing.len();
        self.crowd.work = Some(Work {
            pda,
            manifest: m,
            db_id,
            table,
            src,
            queue: missing.into_iter().collect(),
            total,
            done: 0,
            current: None,
            err: None,
            stop: false,
            finished: false,
        });
        if let Some(main) = self.account.as_ref().and_then(|a| a.main()).map(|w| w.address()) {
            self.fetch_balance(&main);
        }
        self.crowd_next();
    }

    fn crowd_next(&mut self) {
        let Some(w) = self.crowd.work.as_mut() else { return };
        if w.stop || w.queue.is_empty() || w.err.is_some() {
            w.finished = true;
            w.current = None;
            let (done, total, err) = (w.done, w.total, w.err.clone());
            if err.is_none() {
                self.ok(format!("{} piece{} uploaded and recorded{}.", done, if done == 1 { "" } else { "s" }, if done < total { " (stopped)" } else { "" }));
            }
            self.crowd_refresh();
            return;
        }
        let i = w.queue.pop_front().unwrap();
        w.current = Some(i);
        let (start, len) = w.manifest.range(i);
        let id = self.nid();
        self.pending.insert(id, P::CrowdPiece(i));
        match &self.crowd.work.as_ref().unwrap().src {
            Src::File(f) => host::file_read(id, f.fid, start, len),
            Src::Url(u) => host::fetch_bytes(id, u, start, len),
        }
    }

    fn crowd_refresh(&mut self) {
        self.event("click", "tv-refresh", "", "");
    }

    fn crowd_fail(&mut self, msg: String) {
        if let Some(w) = self.crowd.work.as_mut() {
            w.err = Some(msg);
        }
        self.crowd_next();
    }

    /// The upload of the current piece failed (attach.rs).
    pub fn crowd_piece_failed(&mut self, msg: &str) {
        if self.crowd.work.as_ref().map(|w| w.current.is_some()).unwrap_or(false) {
            self.crowd_fail(msg.to_string());
        }
    }

    /// A piece is on chain and recorded in the table (attach.rs).
    pub fn crowd_piece_done(&mut self, piece: usize, sha256: String, tx: String, signer: String) {
        let Some(w) = self.crowd.work.as_mut() else { return };
        w.done += 1;
        w.current = None;
        let pda = w.pda.clone();
        self.crowd.just.push((pda, Reg { piece, sha256, tx, signer, time: Some((host::now_ms() / 1000.0) as i64) }));
        self.crowd_next();
    }

    // ------------------------------------------------ download

    fn crowd_download(&mut self) {
        let Some(s) = self.crowd_scan() else { return };
        if self.use_rpc() {
            return self.err("Downloading reads the pieces through IQ's gateway — switch Settings → Read tables from to the gateway.");
        }
        let pda = self.table.as_ref().map(|t| t.pda.clone()).unwrap_or_default();
        let cands = s.candidates(&self.crowd_extra(&pda), &self.crowd.bad);
        if let Some(i) = cands.iter().position(|c| c.is_empty()) {
            return self.err(format!("Piece {} isn't on the blockchain yet.", i + 1));
        }
        if let Some(old) = self.crowd.dl.take() {
            host::blob_drop(old.bid);
        }
        let bid = self.nid();
        self.crowd.dl = Some(Dl {
            pda,
            manifest: s.manifest,
            cands,
            i: 0,
            k: 0,
            bid,
            whole: Some(Sha256::new()),
            skipped: 0,
            tries: 0,
            unreadable: None,
            err: None,
            done: false,
        });
        self.crowd_dl_fetch();
    }

    fn crowd_dl_fetch(&mut self) {
        let Some(d) = self.crowd.dl.as_ref() else { return };
        let (i, k) = (d.i, d.k);
        let tx = d.cands[i][k].tx.clone();
        self.get(&format!("/data/{}", tx), P::CrowdDl(i, k));
    }

    fn crowd_dl_fail(&mut self, msg: String) {
        if let Some(d) = self.crowd.dl.as_mut() {
            host::blob_drop(d.bid);
            d.err = Some(msg.clone());
        }
        self.err(msg);
    }

    pub fn crowd_async(&mut self, p: P, ok: bool, status: u32, data: Vec<u8>) -> bool {
        match p {
            P::CrowdHash(fid, i) => {
                let Some(pr) = self.crowd.prep.as_mut().filter(|x| x.file.fid == fid && x.hashes.len() == i) else { return false };
                if !ok {
                    pr.err = Some(format!("Couldn't read the file: {}", String::from_utf8_lossy(&data)));
                    return true;
                }
                pr.hashes.push(sha256_hex(&data));
                if let Some(h) = pr.whole.as_mut() {
                    h.update(&data);
                }
                if pr.hashes.len() == pr.count() {
                    pr.sha256 = hex32(pr.whole.take().unwrap().finish());
                } else {
                    self.crowd_prep_next();
                }
            }
            P::CrowdPiece(i) => {
                let Some(w) = self.crowd.work.as_ref().filter(|w| w.current == Some(i)) else { return false };
                let (start, len) = w.manifest.range(i);
                let from_url = matches!(w.src, Src::Url(_));
                let mut bytes = data;
                if !ok || !(200..300).contains(&status) {
                    let e = if from_url {
                        format!(
                            "Couldn't read piece {} from {}: {}. The server has to let browsers read it (CORS); otherwise choose the file from your computer.",
                            i + 1,
                            w.manifest.source,
                            fetch_err(ok, status, &String::from_utf8_lossy(&bytes))
                        )
                    } else {
                        format!("Couldn't read piece {} of your file: {}", i + 1, String::from_utf8_lossy(&bytes))
                    };
                    self.crowd_fail(e);
                    return true;
                }
                // a server that ignores the range sends the whole file
                if from_url && status == 200 && bytes.len() as u64 == w.manifest.size && len != w.manifest.size {
                    bytes = bytes[start as usize..(start + len) as usize].to_vec();
                }
                let hash = sha256_hex(&bytes);
                if bytes.len() as u64 != len || hash != w.manifest.hashes[i] {
                    let e = if from_url {
                        format!(
                            "The file at {} doesn't match piece {}'s fingerprint — it's a different version. Nothing was uploaded.",
                            w.manifest.source,
                            i + 1
                        )
                    } else {
                        format!(
                            "Your file doesn't match piece {}'s fingerprint — it's a different version of {}. Nothing was uploaded.",
                            i + 1,
                            w.manifest.name
                        )
                    };
                    self.crowd_fail(e);
                    return true;
                }
                let (pda, db_id, table, name, count) = (w.pda.clone(), w.db_id.clone(), w.table.clone(), w.manifest.name.clone(), w.manifest.count());
                let wallet = self.account.as_ref().and_then(|a| a.main()).map(|x| x.address()).unwrap_or_default();
                let piece = crate::attach::CrowdPiece { pda, piece: i, sha256: hash, db_id, table, piece_sig: None };
                let filename = format!("{} (piece {} of {})", name, i + 1, count);
                self.attach_crowd_piece(wallet, filename, &bytes, piece);
            }
            P::CrowdDlNext => self.crowd_dl_fetch(),
            P::CrowdDl(i, k) => {
                let Some(d) = self.crowd.dl.as_ref().filter(|d| d.i == i && d.k == k && d.err.is_none()) else { return false };
                let text = String::from_utf8_lossy(&data).into_owned();
                let want = d.manifest.hashes[i].clone();
                let read = ok && (200..300).contains(&status);
                let got = if read {
                    json::parse(&text).ok().map(|v| match v.get("data") {
                        Json::Str(s) => base64_decode(s.trim()).unwrap_or_else(|| s.as_bytes().to_vec()),
                        _ => vec![],
                    })
                } else {
                    None
                };
                let good = got.filter(|b| sha256_hex(b) == want);
                let d = self.crowd.dl.as_mut().unwrap();
                if !read {
                    // the gateway hiccuped: try the same copy again, then the next one
                    d.tries += 1;
                    if d.tries < 4 {
                        let wait = 1000 * d.tries;
                        self.timer(wait, P::CrowdDlNext);
                        return false;
                    }
                    d.unreadable = Some(fetch_err(ok, status, &text));
                    d.tries = 0;
                    d.k += 1;
                    if d.k >= d.cands[i].len() {
                        let e = d.unreadable.clone().unwrap_or_default();
                        self.crowd_dl_fail(format!(
                            "Couldn't read piece {} from IQ's gateway ({}). Nothing was saved — try Download again in a moment.",
                            i + 1,
                            e
                        ));
                        return true;
                    }
                    self.crowd_dl_fetch();
                    return true;
                }
                d.tries = 0;
                match good {
                    Some(b) => {
                        host::blob_part(d.bid, &b);
                        if let Some(h) = d.whole.as_mut() {
                            h.update(&b);
                        }
                        d.i += 1;
                        d.k = 0;
                        d.unreadable = None;
                        if d.i == d.cands.len() {
                            let whole = hex32(d.whole.take().unwrap().finish());
                            if whole != d.manifest.sha256 {
                                self.crowd_dl_fail("Every piece matched, but the whole file didn't — the organizer's fingerprints disagree with each other. Nothing was saved.".into());
                                return true;
                            }
                            d.done = true;
                            host::blob_save(d.bid, &d.manifest.name, &d.manifest.ftype);
                            let (n, skipped) = (d.manifest.name.clone(), d.skipped);
                            self.ok(format!(
                                "Downloaded {} — every piece and the whole file matched their fingerprints{}.",
                                n,
                                if skipped > 0 { format!(" ({} copy that didn't match was skipped)", skipped) } else { String::new() }
                            ));
                            return true;
                        }
                    }
                    None => {
                        let bad_tx = d.cands[i][d.k].tx.clone();
                        d.skipped += 1;
                        d.k += 1;
                        let exhausted = d.k >= d.cands[i].len();
                        if ok && (200..300).contains(&status) && !self.crowd.bad.contains(&bad_tx) {
                            self.crowd.bad.push(bad_tx);
                        }
                        if exhausted {
                            let msg = match d.unreadable.clone() {
                                Some(e) => format!("Piece {}: one copy couldn't be read ({}) and the others don't match its fingerprint. Nothing was saved — try Download again in a moment.", i + 1, e),
                                None => format!("No copy of piece {} on the blockchain matches its fingerprint, so it's shown as missing again — it can be uploaded from the file.", i + 1),
                            };
                            self.crowd_dl_fail(msg);
                            return true;
                        }
                    }
                }
                self.crowd_dl_fetch();
            }
            _ => return false,
        }
        true
    }
}

// ---------------------------------------------------------------- views

/// The crowdfunding card on a table's page in the explorer.
pub fn card(app: &App, h: &mut String) {
    let Some(tv) = app.table.as_ref() else { return };
    let Some(s) = app.crowd_scan() else {
        let packs: Vec<&SourcePack> = tv.decoded.iter().filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok())).collect();
        if !tv.done && looks_crowd(&packs) {
            let note = if tv.rows.len() >= 20_000 && !tv.loading {
                "This table has too many rows to read here, so its file description can't be checked."
            } else {
                "Reading the table's whole history (the file's description is its oldest record)…"
            };
            h.push_str(&format!("<section class=\"card crowd\"><h3>📦 Crowdfunded upload</h3><p class=\"small muted\">{}</p></section>", note));
        }
        return;
    };
    let m = &s.manifest;
    let extra = app.crowd_extra(&tv.pda);
    let cands = s.candidates(&extra, &app.crowd.bad);
    let have = cands.iter().filter(|c| !c.is_empty()).count();
    let n = m.count();
    let legacy = app.settings.tx_format == crate::state::TxFormat::Legacy;
    let work = app.crowd.work.as_ref().filter(|w| w.pda == tv.pda);
    let uploading = work.and_then(|w| w.current);
    h.push_str("<section class=\"card crowd\">");
    h.push_str(&format!("<h3>📦 {} <span class=\"pill\">crowdfunded upload</span></h3>", esc(&m.name)));
    if !m.note.is_empty() {
        h.push_str(&format!("<p>{}</p>", esc(&m.note)));
    }
    h.push_str(&format!(
        "<div class=\"kv\"><div><span>Size</span><span>{} in {} piece{} of {}</span></div><div><span>SHA-256</span><span class=\"mono small hash\">{}</span></div><div><span>Organizer</span><span>{}</span></div>{}</div>",
        ui::bytes(m.size),
        n,
        if n == 1 { "" } else { "s" },
        ui::bytes(m.piece),
        esc(&m.sha256),
        crate::views_account::who(app, &s.organizer),
        if m.source.is_empty() { String::new() } else { format!("<div><span>Source</span><span><a href=\"{0}\" target=\"_blank\" rel=\"noopener noreferrer\">{0}</a></span></div>", esc(&m.source)) }
    ));
    if s.changed {
        h.push_str("<p class=\"warn small\">The organizer later wrote a different description of the file. IQ Tables keeps the first one, so the file can't be swapped.</p>");
    }
    h.push_str(&format!("<p><b>{} of {} pieces</b> are on the blockchain{}.</p>", have, n, if have == n { " — the whole file" } else { "" }));
    h.push_str("<div class=\"pieces\" aria-hidden=\"true\">");
    for (i, c) in cands.iter().enumerate() {
        let cls = if uploading == Some(i) {
            "up"
        } else if !c.is_empty() {
            "on"
        } else {
            ""
        };
        h.push_str(&format!("<i class=\"{}\" title=\"Piece {}{}\"></i>", cls, i + 1, if c.is_empty() { " — missing" } else { "" }));
    }
    h.push_str("</div>");
    // download
    if let Some(d) = app.crowd.dl.as_ref().filter(|d| d.pda == tv.pda) {
        if let Some(e) = &d.err {
            h.push_str(&format!("<p class=\"bad small\">{} <button class=\"link\" data-a=\"crowd-dismiss\">OK</button></p>", esc(e)));
        } else if d.done {
            h.push_str("<p class=\"ok small\">Downloaded and checked. <button class=\"link\" data-a=\"crowd-dismiss\">OK</button></p>");
        } else {
            h.push_str(&format!("<p class=\"small\">Downloading… {} of {} pieces read and checked.</p>", d.i, d.cands.len()));
        }
    }
    if have == n {
        h.push_str("<div class=\"row\"><button class=\"btn primary\" data-a=\"crowd-dl\">Download</button><span class=\"small muted\">Every piece is checked against its fingerprint, and the whole file against the file's.</span></div>");
    }
    // contribute
    if let Some(w) = work {
        match (&w.err, w.finished) {
            (Some(e), _) => h.push_str(&format!("<p class=\"bad small\">{} <button class=\"link\" data-a=\"crowd-dismiss\">OK</button></p>", esc(e))),
            (None, true) => h.push_str(&format!(
                "<p class=\"ok small\">You uploaded {} piece{}. Thank you! <button class=\"link\" data-a=\"crowd-dismiss\">OK</button></p>",
                w.done,
                if w.done == 1 { "" } else { "s" }
            )),
            (None, false) => {
                let st =
                    app.attach_status.as_ref().filter(|(c, _)| c.starts_with("crowd:")).map(|(_, m)| m.clone()).unwrap_or_else(|| "Reading the piece…".into());
                h.push_str(&format!(
                    "<p class=\"small\">Uploading piece {} ({} of {} done) — {}</p><div class=\"row\"><button class=\"btn\" data-a=\"crowd-stop\"{}>Stop after this piece</button></div>",
                    w.current.map(|c| (c + 1).to_string()).unwrap_or_default(),
                    w.done,
                    w.total,
                    esc(&st),
                    if w.stop { " disabled" } else { "" }
                ));
            }
        }
    }
    let busy = work.map(|w| !w.finished).unwrap_or(false);
    if have < n && !busy {
        let missing = n - have;
        let per = piece_cost(m.piece.min(m.size), legacy);
        let all: u64 = (0..n).filter(|&i| cands[i].is_empty()).map(|i| piece_cost(m.range(i).1, legacy)).sum();
        let opts = count_options(missing);
        let (count, chosen) = pick_count(app.form.get("crowd:count").map(|s| s.as_str()), missing);
        let file = app.crowd.files.iter().rev().find(|f| f.size == m.size);
        h.push_str("<h4>Help put it on the blockchain</h4>");
        h.push_str(&format!(
            "<p class=\"small muted\">Each piece costs about {} (IQ's fee for a big upload, the network fee for each part, and a small row saying where the piece is). All {} missing piece{}: about {}. Pieces are paid from your balance; it doesn't matter who uploads which — each one is checked against the organizer's fingerprint.</p>",
            ui::sol(per),
            missing,
            if missing == 1 { "" } else { "s" },
            ui::sol(all)
        ));
        h.push_str("<div class=\"row\"><label>Upload<select data-in=\"form\" data-arg=\"crowd:count\">");
        for (v, l) in &opts {
            h.push_str(&format!("<option value=\"{}\" {}>{}</option>", v, if *v == count.as_str() { "selected" } else { "" }, esc(l)));
        }
        h.push_str("</select></label></div><div class=\"row\">");
        match file {
            Some(f) => h.push_str(&format!(
                "<button class=\"btn primary\" data-a=\"crowd-go\" data-arg=\"file\">Upload {} from {} · ≈{}</button><label class=\"link small\">use a different copy<input type=\"file\" data-filekeep=\"crowd-have\" hidden></label>",
                if chosen == 1 { "1 piece".to_string() } else { format!("{} pieces", chosen) },
                esc(&f.name),
                ui::sol(per * chosen as u64)
            )),
            None => h.push_str("<label class=\"btn primary\">I have the file — choose it…<input type=\"file\" data-filekeep=\"crowd-have\" hidden></label>"),
        }
        if !m.source.is_empty() {
            let host_name = m.source.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(&m.source);
            h.push_str(&format!(
                "<button class=\"btn\" data-a=\"crowd-go\" data-arg=\"url\">Upload {} from {} · ≈{}</button>",
                if chosen == 1 { "1 piece".to_string() } else { format!("{} pieces", chosen) },
                esc(host_name),
                ui::sol(per * chosen as u64)
            ));
        }
        h.push_str("</div>");
        if app.account.is_none() {
            h.push_str("<p class=\"small muted\">Sign in to upload pieces.</p>");
        }
    }
    h.push_str("</section>");
}

/// How many missing pieces to upload: (option value, label) choices.
fn count_options(missing: usize) -> Vec<(String, String)> {
    let mut o = vec![];
    if missing > 1 {
        o.push(("1".to_string(), "1 piece".to_string()));
    }
    for k in [5usize, 10, 50] {
        if k < missing {
            o.push((k.to_string(), format!("{} pieces", k)));
        }
    }
    o.push(("all".to_string(), if missing == 1 { "the missing piece".to_string() } else { format!("all {} missing", missing) }));
    o
}

/// The chosen option (falling back to the first when the saved choice isn't
/// offered any more) and how many pieces it means.
pub fn pick_count(saved: Option<&str>, missing: usize) -> (String, usize) {
    let opts = count_options(missing);
    let v = saved.filter(|v| opts.iter().any(|(o, _)| o == v)).map(String::from).unwrap_or_else(|| opts[0].0.clone());
    let n = if v == "all" { missing } else { v.parse::<usize>().unwrap_or(1).min(missing) };
    (v, n)
}

/// The organizer's card in the Editor (a database's Structure tab).
pub fn editor_card(app: &App, key: &str, h: &mut String) {
    h.push_str("<section class=\"card crowd\"><h3>Crowdfund a big file</h3><p class=\"small muted\">Put a big file on the blockchain together: you publish its fingerprints (one per piece), anyone can pay to upload pieces from their own balance, and everyone who downloads it gets exactly this file back — any piece that doesn't match is ignored. Only share files you have the right to share: nothing on the blockchain can be taken down.</p>");
    let Some(p) = app.crowd.prep.as_ref().filter(|p| p.key == key) else {
        h.push_str(&format!("<div class=\"row\"><label class=\"btn\">Choose the file…<input type=\"file\" data-filekeep=\"crowd-new\" data-arg=\"{}\" hidden></label></div></section>", esc(key)));
        return;
    };
    let n = p.count();
    if let Some(e) = &p.err {
        h.push_str(&format!(
            "<p class=\"bad small\">{}</p><div class=\"row\"><button class=\"btn\" data-a=\"crowd-cancel\">OK</button></div></section>",
            esc(e)
        ));
        return;
    }
    if !p.done() {
        h.push_str(&format!(
            "<p class=\"small\">Fingerprinting {} ({})… {} of {} pieces.</p><div class=\"row\"><button class=\"btn\" data-a=\"crowd-cancel\">Cancel</button></div></section>",
            esc(&p.file.name),
            ui::bytes(p.file.size),
            p.hashes.len(),
            n
        ));
        return;
    }
    let legacy = app.settings.tx_format == crate::state::TxFormat::Legacy;
    let m = Manifest {
        name: p.file.name.clone(),
        size: p.file.size,
        ftype: p.file.ftype.clone(),
        sha256: p.sha256.clone(),
        piece: p.piece,
        hashes: p.hashes.clone(),
        source: String::new(),
        note: String::new(),
    };
    let get = |k: &str| app.form.get(k).cloned().unwrap_or_default();
    h.push_str(&format!(
        "<p class=\"small\">Fingerprint ready: <b>{}</b>, {} in {} piece{} of {}. SHA-256 <span class=\"mono small hash\">{}</span></p><p class=\"small muted\">Uploading every piece costs about {} in all, paid by whoever uploads them. Publishing the fingerprints costs one write (more for very big files) plus the new table.</p>",
        esc(&p.file.name),
        ui::bytes(p.file.size),
        n,
        if n == 1 { "" } else { "s" },
        ui::bytes(p.piece),
        esc(&p.sha256),
        ui::sol(total_cost(&m, legacy))
    ));
    h.push_str(&format!(
        "<div class=\"formgrid\"><label>Table name<input id=\"crowd-name\" data-in=\"form\" data-arg=\"crowd:name\" value=\"{}\" maxlength=\"32\"></label><label>Where people can get the file (optional)<input id=\"crowd-source\" data-in=\"form\" data-arg=\"crowd:source\" value=\"{}\" placeholder=\"https://… — contributors' browsers read pieces from here\"></label><label>Description (optional)<input id=\"crowd-note\" data-in=\"form\" data-arg=\"crowd:note\" value=\"{}\" placeholder=\"What it is, and why it may be shared (e.g. public domain)\"></label></div><div class=\"row\"><button class=\"btn primary\" data-a=\"crowd-create\" data-arg=\"{}\">Create the table</button><button class=\"btn\" data-a=\"crowd-cancel\">Cancel</button></div></section>",
        esc(&get("crowd:name")),
        esc(&get("crowd:source")),
        esc(&get("crowd:note")),
        esc(key)
    ));
}
