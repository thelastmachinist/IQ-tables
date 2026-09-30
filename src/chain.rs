//! Reading straight from Solana when the IQ gateway can't help (devnet, or
//! when chosen in Settings): the database list via `getProgramAccounts`, table
//! metadata via `getAccountInfo`, and rows by walking the table's transaction
//! history and decoding each inline `db_code_in` — the same thing the gateway
//! does, minus its cache.

use crate::app::{fetch_err, App, Load, P};
use crate::host;
use crate::iq;
use crate::json::{self, Json};
use crate::net::{self, rows_from_tx, tx_request, DbRootInfo};

/// Signatures per page of table history.
pub const SIG_PAGE: usize = 25;

/// Reassembling a row sent in chunks (IQ's linked list or session).
#[derive(Clone)]
pub struct ChunkRead {
    pub pda: String,
    pub gen: u32,
    /// The finalizing transaction (the placeholder row's signature).
    pub sig: String,
    pub kind: ChunkKind,
}

#[derive(Clone)]
pub enum ChunkKind {
    /// Walking back from the tail: the next signature to read, parts so far (in order).
    Linked { next: String, parts: Vec<String> },
    /// A session: its address, signatures still to read, parts by index.
    Session { session: String, sigs: Vec<String>, parts: std::collections::BTreeMap<u32, String>, total: usize, listed: bool },
}

/// One page of a table's history being turned into rows.
#[derive(Clone)]
pub struct Page {
    pub pda: String,
    pub gen: u32,
    /// Signatures still to read (all of them in batch mode).
    pub sigs: Vec<String>,
    /// Cursor for the next page (the oldest signature seen).
    pub next: Option<String>,
    /// This was the last page of history.
    pub last: bool,
    /// Try one JSON-RPC batch request; fall back to one call per transaction.
    pub batch: bool,
}

fn roots_params() -> Json {
    Json::Arr(vec![
        json::s(iq::PROGRAM_ID_STR),
        json::obj(vec![
            ("encoding", json::s("base64")),
            ("commitment", json::s("confirmed")),
            (
                "filters",
                Json::Arr(vec![json::obj(vec![(
                    "memcmp",
                    json::obj(vec![("offset", json::n(0)), ("bytes", json::s(&crate::crypto::base58::encode(&iq::ACC_DB_ROOT_DISC)))]),
                )])]),
            ),
        ]),
    ])
}

/// Every DbRoot in a `getProgramAccounts` result.
pub fn roots_from(v: &Json) -> Vec<DbRootInfo> {
    let mut out: Vec<DbRootInfo> = v
        .arr()
        .iter()
        .filter_map(|a| {
            let pda = a.get("pubkey").str()?;
            let data = net::account_data(a.get("account"))?;
            let root = iq::decode_db_root(&data)?;
            Some(net::dbroot_info(pda, &root))
        })
        .collect();
    net::sort_roots(&mut out);
    out
}

impl App {
    fn post_rpc(&mut self, body: String, p: P) {
        let id = self.nid();
        self.pending.insert(id, p);
        host::fetch(id, "POST", &self.settings.rpc.clone(), &body, "application/json");
    }

    pub fn rpc_roots(&mut self) {
        self.rpc("getProgramAccounts", roots_params(), P::RpcRoots);
    }

    pub fn rpc_meta(&mut self, pda: &str, gen: u32) {
        let params = Json::Arr(vec![json::s(pda), json::obj(vec![("encoding", json::s("base64")), ("commitment", json::s("confirmed"))])]);
        self.rpc("getAccountInfo", params, P::RpcMeta(pda.to_string(), gen));
    }

    pub fn rpc_rows(&mut self, pda: &str, gen: u32, before: Option<String>) {
        let mut cfg = json::obj(vec![("limit", json::n(SIG_PAGE)), ("commitment", json::s("confirmed"))]);
        if let Some(b) = before {
            cfg.set("before", json::s(&b));
        }
        self.rpc("getSignaturesForAddress", Json::Arr(vec![json::s(pda), cfg]), P::RpcSigs(pda.to_string(), gen));
    }

    fn fetch_txs(&mut self, page: Page) {
        if page.sigs.is_empty() {
            self.finish_page(page);
            return;
        }
        let body = if page.batch {
            Json::Arr(page.sigs.iter().enumerate().map(|(i, s)| tx_request(s, i)).collect()).to_string()
        } else {
            tx_request(&page.sigs[0], 0).to_string()
        };
        self.post_rpc(body, P::RpcTxs(page));
    }

    fn push_rows(&mut self, pda: &str, gen: u32, rows: Vec<Json>) {
        let mut reads = vec![];
        let official = self.official_for(pda);
        if let Some(t) = self.tv_mut(pda, gen) {
            for r in rows {
                // a retried page must not add the same row twice
                let sig = r.get("__txSignature").str_or("");
                if t.rows.iter().any(|x| x.get("__txSignature").str() == Some(sig.as_str()) && *x == r) {
                    continue;
                }
                if r.get("__pending").bool() == Some(true) {
                    t.chunk_waits += 1;
                    let path = r.get("__onChainPath").str_or("");
                    let total = r.get("__chunks").u64().unwrap_or(0) as usize;
                    reads.push((sig.clone(), path, total));
                }
                let d = crate::app::decode_for(&r, official.as_deref(), t);
                t.decoded.push(d);
                t.rows.push(r);
            }
        }
        for (sig, path, total) in reads {
            let kind = if net::is_linked_path(&path) {
                ChunkKind::Linked { next: path, parts: vec![] }
            } else {
                ChunkKind::Session { session: path, sigs: vec![], parts: Default::default(), total, listed: false }
            };
            self.chunk_step(ChunkRead { pda: pda.to_string(), gen, sig, kind });
        }
    }

    /// Next request for a chunked row.
    fn chunk_step(&mut self, c: ChunkRead) {
        match &c.kind {
            ChunkKind::Linked { next, .. } => {
                let body = tx_request(next, 0).get("params").clone();
                self.rpc("getTransaction", body, P::RpcChunk(c));
            }
            ChunkKind::Session { session, listed: false, .. } => {
                let params = Json::Arr(vec![json::s(session), json::obj(vec![("limit", json::n(1000)), ("commitment", json::s("confirmed"))])]);
                self.rpc("getSignaturesForAddress", params, P::RpcChunk(c));
            }
            ChunkKind::Session { sigs, .. } => {
                let Some(s) = sigs.first().cloned() else {
                    self.chunk_done(c);
                    return;
                };
                let body = tx_request(&s, 0).get("params").clone();
                self.rpc("getTransaction", body, P::RpcChunk(c));
            }
        }
    }

    /// All parts are in (or reading failed): replace the placeholder row.
    fn chunk_done(&mut self, c: ChunkRead) {
        let text = match &c.kind {
            ChunkKind::Linked { parts, .. } => Some(parts.concat()),
            ChunkKind::Session { parts, total, .. } => net::join_parts(parts, *total),
        };
        let official = self.official_for(&c.pda);
        let mut finished = false;
        if let Some(t) = self.tv_mut(&c.pda, c.gen) {
            if let Some(i) = t.rows.iter().position(|r| r.get("__txSignature").str() == Some(c.sig.as_str()) && r.get("__pending").bool() == Some(true)) {
                let row = net::chunked_row(text.as_deref(), &t.rows[i]);
                t.decoded[i] = crate::app::decode_for(&row, official.as_deref(), t);
                t.rows[i] = row;
            }
            t.chunk_waits = t.chunk_waits.saturating_sub(1);
            finished = t.chunk_waits == 0 && t.done;
        }
        if finished {
            self.base_loaded(&c.pda);
        }
    }

    fn finish_page(&mut self, page: Page) {
        let mut again = false;
        let mut finished = false;
        if let Some(t) = self.tv_mut(&page.pda, page.gen) {
            t.loading = false;
            t.cursor = page.next;
            if !t.load_all && crate::crowd::looks_crowd(&t.decoded.iter().filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok())).collect::<Vec<_>>()) {
                t.load_all = true;
                t.who = crate::app::Who::All;
            }
            if page.last || t.cursor.is_none() {
                t.done = true;
                t.load_all = false;
                finished = true;
            }
            again = t.load_all && !t.done && t.rows.len() < 20_000;
        }
        if (again || finished) && self.stop_at_checkpoint(&page.pda, page.gen) {
            again = false;
            finished = true;
        }
        let waiting = self.tv_mut(&page.pda, page.gen).map(|t| t.chunk_waits > 0).unwrap_or(false);
        if again {
            self.more_rows_for(&page.pda);
        } else if finished && !waiting {
            self.base_loaded(&page.pda);
        }
    }

    fn table_err(&mut self, pda: &str, gen: u32, e: String) {
        if let Some(t) = self.tv_mut(pda, gen) {
            t.loading = false;
            t.load_all = false;
            t.err = Some(e);
        }
    }

    /// Everything the logged-in account created, from the database list
    /// (IQ's gateway, or the chain in RPC mode). Brand-new databases also
    /// show up from the local drafts.
    pub fn load_mine(&mut self) {
        if self.use_rpc() && !self.dbroots.is_loading() {
            // RPC mode reads the list live, so refresh it
            self.dbroots = Load::Loading;
            self.rpc_roots();
        } else {
            self.ensure_dbroots();
        }
        self.load_files();
    }

    pub fn chain_async(&mut self, p: P, ok: bool, status: u32, data: Vec<u8>) -> bool {
        let text = String::from_utf8_lossy(&data).into_owned();
        let http_ok = ok && (200..300).contains(&status);
        let res = || if http_ok { net::rpc_result(&text) } else { Err(fetch_err(ok, status, &text)) };
        match p {
            P::RpcRoots => {
                self.dbroots = match res() {
                    Ok(v) => Load::Ready(roots_from(&v)),
                    Err(e) => Load::Err(format!("{} (reading databases straight from Solana needs an RPC that allows getProgramAccounts)", e)),
                };
                self.attach_root_info();
            }
            P::RpcMeta(pda, gen) => {
                let r = res();
                let cluster = self.settings.cluster.clone();
                if let Some(t) = self.tv_mut(&pda, gen) {
                    t.meta = match r {
                        Ok(v) => match net::account_data(v.get("value")) {
                            Some(d) => match iq::decode_table(&d) {
                                Some(m) => Load::Ready(iq::meta_json(&m)),
                                None => Load::Err("that account isn't an IQ table".into()),
                            },
                            None => Load::Err(format!("no table at this address on {}", cluster)),
                        },
                        Err(e) => Load::Err(e),
                    };
                }
            }
            P::RpcSigs(pda, gen) => {
                if self.tv_mut(&pda, gen).is_none() {
                    return false;
                }
                match res() {
                    Ok(v) => {
                        let list = v.arr();
                        let next = list.last().and_then(|x| x.get("signature").str()).map(String::from);
                        let last = list.len() < SIG_PAGE;
                        let sigs = list.iter().filter(|x| x.get("err").is_null()).filter_map(|x| x.get("signature").str().map(String::from)).collect();
                        self.fetch_txs(Page { pda, gen, sigs, next, last, batch: true });
                        return false;
                    }
                    Err(e) => self.table_err(&pda, gen, e),
                }
            }
            P::RpcTxs(mut page) => {
                if self.tv_mut(&page.pda, page.gen).is_none() {
                    return false;
                }
                if page.batch {
                    let parsed = if http_ok { json::parse(&text).ok() } else { None };
                    match parsed {
                        Some(Json::Arr(items)) => {
                            let mut results: Vec<Option<Json>> = vec![None; page.sigs.len()];
                            for it in &items {
                                if let Some(i) = it.get("id").u64().map(|i| i as usize).filter(|&i| i < results.len()) {
                                    if it.get("error").is_null() {
                                        results[i] = Some(it.get("result").clone());
                                    }
                                }
                            }
                            let failed: Vec<String> = page.sigs.iter().zip(&results).filter(|(_, r)| r.is_none()).map(|(s, _)| s.clone()).collect();
                            let pda = page.pda.clone();
                            let rows: Vec<Json> = results.iter().flatten().flat_map(|r| rows_from_tx(r, &pda)).collect();
                            self.push_rows(&pda, page.gen, rows);
                            page.sigs = failed;
                            // anything the batch couldn't read is retried one by one
                            page.batch = false;
                            self.fetch_txs(page);
                        }
                        _ if status == 429 => {
                            let (pda, gen) = (page.pda.clone(), page.gen);
                            self.table_err(&pda, gen, fetch_err(ok, status, &text));
                        }
                        _ => {
                            // this RPC doesn't take batches: one request per transaction
                            page.batch = false;
                            self.fetch_txs(page);
                            return false;
                        }
                    }
                } else {
                    match res() {
                        Ok(r) => {
                            let rows = rows_from_tx(&r, &page.pda);
                            let (pda, gen) = (page.pda.clone(), page.gen);
                            self.push_rows(&pda, gen, rows);
                            page.sigs.remove(0);
                            self.fetch_txs(page);
                        }
                        Err(e) => {
                            let (pda, gen) = (page.pda.clone(), page.gen);
                            self.table_err(&pda, gen, e);
                        }
                    }
                }
            }
            P::RpcChunk(mut c) => {
                if self.tv_mut(&c.pda, c.gen).is_none() {
                    return false;
                }
                let r = res();
                match (&mut c.kind, r) {
                    (ChunkKind::Linked { next, parts }, Ok(v)) => match net::linked_part(&v) {
                        Some((code, before)) if parts.len() < 1000 => {
                            parts.insert(0, code);
                            if before == "Genesis" || before.is_empty() {
                                self.chunk_done(c);
                            } else {
                                *next = before;
                                self.chunk_step(c);
                            }
                        }
                        _ => {
                            parts.clear();
                            parts.push(String::new());
                            self.chunk_done(c);
                        }
                    },
                    (ChunkKind::Session { sigs, listed, .. }, Ok(v)) if !*listed => {
                        *listed = true;
                        *sigs = v.arr().iter().filter(|x| x.get("err").is_null()).filter_map(|x| x.get("signature").str().map(String::from)).collect();
                        // oldest first, so a re-sent part (newer) wins
                        sigs.reverse();
                        self.chunk_step(c);
                    }
                    (ChunkKind::Session { sigs, parts, .. }, Ok(v)) => {
                        for (i, chunk) in net::session_parts(&v) {
                            parts.insert(i, chunk);
                        }
                        if !sigs.is_empty() {
                            sigs.remove(0);
                        }
                        self.chunk_step(c);
                    }
                    (_, Err(_)) => self.chunk_done(c),
                }
            }
            _ => return false,
        }
        true
    }
}
