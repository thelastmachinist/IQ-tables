//! Reading straight from Solana when the IQ gateway can't help (devnet, or
//! when chosen in Settings): the database list via `getProgramAccounts`, table
//! metadata via `getAccountInfo`, and rows by walking the table's transaction
//! history and decoding each inline `db_code_in` — the same thing the gateway
//! does, minus its cache.

use crate::app::{decode_row, fetch_err, App, Load, P};
use crate::crypto::base64_decode;
use crate::host;
use crate::iq;
use crate::json::{self, Json};
use crate::net::{self, DbRootInfo};
use crate::solana::{self, b58};

/// Signatures per page of table history.
pub const SIG_PAGE: usize = 25;

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

fn tx_request(sig: &str, id: usize) -> Json {
    json::obj(vec![
        ("jsonrpc", json::s("2.0")),
        ("id", json::n(id)),
        ("method", json::s("getTransaction")),
        (
            "params",
            Json::Arr(vec![
                json::s(sig),
                json::obj(vec![
                    ("encoding", json::s("base64")),
                    ("maxSupportedTransactionVersion", json::n(1)),
                    ("commitment", json::s("confirmed")),
                ]),
            ]),
        ),
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

/// Rows (gateway-shaped) from one `getTransaction` result, for table `pda`.
pub fn rows_from_tx(result: &Json, pda: &str) -> Vec<Json> {
    let mut out = vec![];
    if result.is_null() || !result.get("meta").get("err").is_null() {
        return out;
    }
    let Some(raw) = result.get("transaction").idx(0).str().and_then(base64_decode) else { return out };
    let Some(tx) = solana::parse_tx(&raw) else { return out };
    let pid = iq::program_id();
    let time = result.get("blockTime").u64();
    for (p, accs, data) in &tx.ixs {
        if tx.keys.get(*p) != Some(&pid) {
            continue;
        }
        let Some(d) = iq::decode_db_code_in(data) else { continue };
        // account 7 of db_code_in is the table
        if accs.get(7).and_then(|&i| tx.keys.get(i)).map(b58).as_deref() != Some(pda) {
            continue;
        }
        if !d.on_chain_path.is_empty() {
            continue; // chunked upload: only the gateway reassembles those
        }
        let Some(mut row) = iq::row_from_metadata(&d.metadata) else { continue };
        row.set("__txSignature", json::s(&tx.signature));
        if let Some(signer) = accs.first().and_then(|&i| tx.keys.get(i)) {
            row.set("__signer", json::s(&b58(signer)));
        }
        if let Some(t) = time {
            row.set("__blockTime", json::n(t));
        }
        out.push(row);
    }
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

    fn push_rows(&mut self, rows: Vec<Json>) {
        if let Some(t) = self.table.as_mut() {
            for r in rows {
                // a retried page must not add the same row twice
                let sig = r.get("__txSignature").str_or("");
                if t.rows.iter().any(|x| x.get("__txSignature").str() == Some(sig.as_str()) && *x == r) {
                    continue;
                }
                t.decoded.push(decode_row(&r));
                t.rows.push(r);
            }
        }
    }

    fn finish_page(&mut self, page: Page) {
        let mut again = false;
        if let Some(t) = self.table.as_mut().filter(|t| t.pda == page.pda && t.gen == page.gen) {
            t.loading = false;
            t.cursor = page.next;
            if page.last || t.cursor.is_none() {
                t.done = true;
                t.load_all = false;
            }
            again = t.load_all && !t.done && t.rows.len() < 20_000;
        }
        if again {
            self.more_rows();
        }
    }

    fn table_err(&mut self, pda: &str, gen: u32, e: String) {
        if let Some(t) = self.table.as_mut().filter(|t| t.pda == pda && t.gen == gen) {
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
                if let Some(t) = self.table.as_mut().filter(|t| t.pda == pda && t.gen == gen) {
                    t.meta = match r {
                        Ok(v) => match net::account_data(v.get("value")) {
                            Some(d) => match iq::decode_table(&d) {
                                Some(m) => Load::Ready(iq::meta_json(&m)),
                                None => Load::Err("that account isn't an IQ table".into()),
                            },
                            None => Load::Err(format!("no table at this address on {}", self.settings.cluster)),
                        },
                        Err(e) => Load::Err(e),
                    };
                }
            }
            P::RpcSigs(pda, gen) => {
                if !self.table.as_ref().map(|t| t.pda == pda && t.gen == gen).unwrap_or(false) {
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
                if !self.table.as_ref().map(|t| t.pda == page.pda && t.gen == page.gen).unwrap_or(false) {
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
                            self.push_rows(rows);
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
                            self.push_rows(rows);
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
            _ => return false,
        }
        true
    }
}
