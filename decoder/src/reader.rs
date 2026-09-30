//! `read`: getting a table the way IQ Labs stores it, one step at a time.
//!
//! The reader never touches the network itself. Each step asks the loader
//! for some HTTP requests (to IQ's gateway, or to the developer's Solana RPC)
//! and continues with the answers. The gateway is tried first; with an RPC
//! configured, the table's transactions are read straight from Solana when
//! the gateway can't answer (or always, with `source: "solana"`).

use std::collections::BTreeMap;

use iq_tables::crypto::base58;
use iq_tables::json::{self, Json};
use iq_tables::net;
use iq_tables::pack::{self, SourcePack};
use iq_tables::records::{self, Source, Who};

/// Gateway rows per request (its maximum).
const PAGE: usize = 100;
/// Signatures per page of a table's history on Solana.
const SIG_PAGE: usize = 100;
/// JSON-RPC calls per batch request.
const BATCH: usize = 50;
const MAX_RETRIES: u32 = 5;
/// Links followed in IQ's older chunk lists.
const MAX_HOPS: usize = 1000;
/// Distinct chunked rows put back together in one read.
const MAX_CHUNKED: usize = 10_000;
/// Transactions read to put chunked rows back together, per round and per read.
const CHUNK_ROUND: usize = 500;
const MAX_CHUNK_CALLS: usize = 50_000;
/// The same for rows from anyone but the table's owner.
const OTHERS_CHUNKED: usize = 200;
const OTHERS_CHUNK_CALLS: usize = 5_000;
pub const DEFAULT_GATEWAY: &str = "https://gateway.iqlabs.dev";
const DEFAULT_MAX_ROWS: usize = 100_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Csv,
    Json,
    Html,
    Rows,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prefer {
    Auto,
    Gateway,
    Solana,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub table: String,
    pub official: Option<String>,
    pub who: Who,
    pub format: Format,
    pub gateway: String,
    pub rpc: Option<String>,
    pub prefer: Prefer,
    pub fresh: bool,
    pub max_rows: usize,
}

fn is_pk(s: &str) -> bool {
    base58::decode(s).map(|b| b.len()) == Some(32)
}

fn is_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://")
}

impl Config {
    pub fn from_json(v: &Json) -> Result<Config, String> {
        let table = v.get("table").str_or("");
        if !is_pk(&table) {
            return Err("config.table must be a table address".into());
        }
        let official = v.get("official").str().map(String::from).filter(|s| !s.is_empty());
        if official.as_deref().map(|o| !is_pk(o)).unwrap_or(false) {
            return Err("config.official must be a wallet address".into());
        }
        let who = match v.get("rows").str() {
            None => Who::Official,
            Some(s) => Who::parse(s).ok_or("config.rows must be \"official\", \"unofficial\" or \"all\"")?,
        };
        // anyone can write to a table: knowing whose rows are the owner's is
        // what keeps other writers' rows from costing more than a little
        if official.is_none() {
            return Err("config.official (the wallet whose rows count as official: the database's creator) is required".into());
        }
        let format = match v.get("format").str().unwrap_or("rows") {
            "csv" => Format::Csv,
            "json" => Format::Json,
            "html" => Format::Html,
            "rows" => Format::Rows,
            _ => return Err("config.format must be \"csv\", \"json\", \"html\" or \"rows\"".into()),
        };
        let gateway = v.get("gateway").str().unwrap_or(DEFAULT_GATEWAY).trim_end_matches('/').to_string();
        if !is_url(&gateway) {
            return Err("config.gateway must be an http(s) URL".into());
        }
        let rpc = v.get("rpc").str().map(String::from).filter(|s| !s.is_empty());
        if rpc.as_deref().map(|r| !is_url(r)).unwrap_or(false) {
            return Err("config.rpc must be an http(s) URL".into());
        }
        let prefer = match v.get("source").str().unwrap_or("auto") {
            "auto" => Prefer::Auto,
            "gateway" => Prefer::Gateway,
            "solana" | "rpc" => Prefer::Solana,
            _ => return Err("config.source must be \"auto\", \"gateway\" or \"solana\"".into()),
        };
        if prefer == Prefer::Solana && rpc.is_none() {
            return Err("config.source \"solana\" needs config.rpc".into());
        }
        Ok(Config {
            table,
            official,
            who,
            format,
            gateway,
            rpc,
            prefer,
            fresh: v.get("fresh").bool().unwrap_or(false),
            max_rows: v.get("maxRows").u64().map(|n| n as usize).filter(|&n| n > 0).unwrap_or(DEFAULT_MAX_ROWS),
        })
    }
}

/// An HTTP request for the loader.
#[derive(Clone, Debug)]
pub struct Req {
    pub url: String,
    pub method: &'static str,
    pub body: String,
}

impl Req {
    fn get(url: String) -> Req {
        Req { url, method: "GET", body: String::new() }
    }
    fn post(url: &str, body: String) -> Req {
        Req { url: url.to_string(), method: "POST", body }
    }
}

#[derive(Debug)]
pub enum Step {
    Fetch(Vec<Req>),
    /// (format, payload) for other decoders.
    Decode(Vec<(String, String)>),
    Wait(u32),
    Done(Json),
    Error(String),
}

impl Step {
    pub fn to_json(&self) -> Json {
        match self {
            Step::Fetch(reqs) => json::obj(vec![(
                "fetch",
                Json::Arr(
                    reqs.iter()
                        .map(|r| {
                            let mut o = json::obj(vec![("url", json::s(&r.url)), ("method", json::s(r.method))]);
                            if !r.body.is_empty() {
                                o.set("body", json::s(&r.body));
                                o.set("type", json::s("application/json"));
                            }
                            o
                        })
                        .collect(),
                ),
            )]),
            Step::Decode(items) => {
                json::obj(vec![("decode", Json::Arr(items.iter().map(|(f, p)| json::obj(vec![("format", json::s(f)), ("payload", json::s(p))])).collect()))])
            }
            Step::Wait(ms) => json::obj(vec![("wait", json::n(ms))]),
            Step::Done(v) => json::obj(vec![("done", v.clone())]),
            Step::Error(e) => json::obj(vec![("error", json::s(e))]),
        }
    }
}

/// One answer from the loader.
struct Answer {
    ok: bool,
    status: u32,
    body: String,
}

impl Answer {
    fn of(v: &Json) -> Answer {
        Answer { ok: v.get("ok").bool().unwrap_or(false), status: v.get("status").u64().unwrap_or(0) as u32, body: v.get("body").str_or("") }
    }
    fn good(&self) -> bool {
        self.ok && (200..300).contains(&self.status)
    }
    fn limited(&self) -> bool {
        self.ok && self.status == 429
    }
    fn why(&self) -> String {
        if !self.ok {
            format!("network error: {}", self.body.chars().take(160).collect::<String>())
        } else {
            net::gateway_error(self.status, &self.body)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Via {
    Gateway,
    Solana,
}

/// A chunked row being put back together from Solana.
#[derive(Debug)]
enum Chunk {
    Session { row: usize, official: bool, session: String, listed: bool, sigs: Vec<String>, parts: BTreeMap<u32, String>, total: usize, done: bool },
    Linked { row: usize, official: bool, start: String, next: String, parts: Vec<String>, done: bool },
}

#[derive(Clone, Debug)]
enum RpcWhat {
    /// getAccountInfo (metadata) + the first page of signatures.
    First,
    Sigs,
    /// getTransaction for these signatures (index in the page).
    Txs(Vec<usize>),
    /// Per call: (chunk, is the signature listing).
    Chunks(Vec<(usize, bool)>),
}

#[derive(Clone, Debug)]
enum Kind {
    GwFirst,
    GwPage,
    Rpc { calls: Vec<Json>, what: RpcWhat, batched: bool },
    Delegate(Vec<usize>),
}

#[derive(Clone, Debug)]
struct Out {
    reqs: Vec<Req>,
    kind: Kind,
}

pub struct Reader {
    cfg: Config,
    /// Rows as read (gateway-shaped), newest first.
    rows: Vec<Json>,
    decoded: Vec<Option<Result<SourcePack, String>>>,
    meta: Option<Json>,
    via: Via,
    cursor: Option<String>,
    more: bool,
    truncated: bool,
    /// Solana: signatures of the page being read, and rows per signature.
    page_sigs: Vec<String>,
    page_rows: BTreeMap<usize, Vec<Json>>,
    chunks: Vec<Chunk>,
    /// Rows whose chunks are the same as another row's: (row, chunk).
    aliases: Vec<(usize, usize)>,
    /// Where the page being read starts in `rows`.
    page_start: usize,
    /// Chunked rows started, and transactions read for them, in this read:
    /// the owner's, and everyone else's (who get a much smaller share).
    chunked: usize,
    chunk_calls: usize,
    others_chunked: usize,
    others_calls: usize,
    /// Unpacked bytes of other writers' packs (records::decode_row_for).
    others_raw: u64,
    batch_ok: bool,
    retries: u32,
    /// The owner's newest checkpoint (index of its structure record, the
    /// packs it lists), the official packs seen after it, and how far the
    /// rows have been looked at.
    ckpt: Option<(usize, Vec<String>)>,
    ckpt_seen: std::collections::HashSet<String>,
    ckpt_scan: usize,
    crowd: bool,
    /// What the last step is waiting for.
    pending: Option<Out>,
    /// A step to send again after a wait (rate limits).
    again: Option<Out>,
    notes: Vec<String>,
}

/// How many transactions a session with `total` parts may list: its parts,
/// re-sent ones, and the ones that open and close it.
fn session_limit(total: usize) -> usize {
    if total == 0 {
        1000
    } else {
        (total * 4 + 16).min(1000)
    }
}

fn rpc_call(id: usize, method: &str, params: Json) -> Json {
    json::obj(vec![("jsonrpc", json::s("2.0")), ("id", json::n(id)), ("method", json::s(method)), ("params", params)])
}

fn with_id(mut call: Json, id: usize) -> Json {
    call.set("id", json::n(id));
    call
}

enum RpcFail {
    Limited,
    NoBatch,
    Down(String),
}

impl Reader {
    pub fn new(cfg: Config) -> Reader {
        let via = if cfg.prefer == Prefer::Solana { Via::Solana } else { Via::Gateway };
        Reader {
            cfg,
            rows: vec![],
            decoded: vec![],
            meta: None,
            via,
            cursor: None,
            more: true,
            truncated: false,
            page_sigs: vec![],
            page_rows: BTreeMap::new(),
            chunks: vec![],
            aliases: vec![],
            page_start: 0,
            chunked: 0,
            chunk_calls: 0,
            others_chunked: 0,
            others_calls: 0,
            others_raw: 0,
            batch_ok: true,
            retries: 0,
            ckpt: None,
            ckpt_seen: Default::default(),
            ckpt_scan: 0,
            crowd: false,
            pending: None,
            again: None,
            notes: vec![],
        }
    }

    pub fn start(&mut self) -> Step {
        match self.via {
            Via::Gateway => {
                let meta = Req::get(format!("{}/table/{}/meta", self.cfg.gateway, self.cfg.table));
                let rows = self.gw_rows_req();
                self.send(Out { reqs: vec![meta, rows], kind: Kind::GwFirst })
            }
            Via::Solana => self.rpc_first(),
        }
    }

    /// Rows this read needs decoded: reading official rows, nobody else's
    /// (they'd only cost time, and anyone can write them).
    fn wanted(&self, r: &Json) -> bool {
        match (&self.cfg.official, self.cfg.who) {
            (Some(o), Who::Official) => r.get("__signer").str() == Some(o.as_str()),
            _ => true,
        }
    }

    fn decode(&mut self, r: &Json) -> Option<Result<SourcePack, String>> {
        if !self.wanted(r) {
            return None;
        }
        records::decode_row_for(r, self.cfg.official.as_deref(), &mut self.others_raw)
    }

    fn is_official(&self, r: &Json) -> bool {
        r.get("__signer").str() == self.cfg.official.as_deref()
    }

    fn send(&mut self, out: Out) -> Step {
        let step = Step::Fetch(out.reqs.clone());
        self.pending = Some(out);
        step
    }

    fn gw_rows_req(&self) -> Req {
        let mut url = format!("{}/table/{}/rows?limit={}", self.cfg.gateway, self.cfg.table, PAGE);
        if let Some(c) = &self.cursor {
            url.push_str(&format!("&before={}", c));
        } else if self.cfg.fresh {
            url.push_str("&fresh=1");
        }
        Req::get(url)
    }

    /// Wait and send `out` again, or give up with `why` after a few tries.
    fn backoff(&mut self, out: Out, why: String) -> Step {
        self.retries += 1;
        if self.retries > MAX_RETRIES {
            return Step::Error(why);
        }
        self.again = Some(out);
        Step::Wait(500 * (1 << self.retries.min(6)))
    }

    pub fn resume(&mut self, results: &[Json]) -> Step {
        if let Some(out) = self.again.take() {
            return self.send(out);
        }
        let Some(out) = self.pending.take() else { return Step::Error("nothing to resume".into()) };
        let answers: Vec<Answer> = results.iter().map(Answer::of).collect();
        match out.kind.clone() {
            Kind::GwFirst | Kind::GwPage => {
                if answers.len() != out.reqs.len() {
                    return Step::Error("the loader returned the wrong number of answers".into());
                }
                let rows = answers.last().unwrap();
                if matches!(out.kind, Kind::GwFirst) {
                    let meta = &answers[0];
                    if meta.good() {
                        self.meta = json::parse(&meta.body).ok().filter(|m| m.get("error").is_null());
                    }
                }
                if rows.limited() {
                    let why = format!("IQ's gateway ({}) kept answering \"too many requests\" (429)", self.cfg.gateway);
                    return self.backoff(out, why);
                }
                if !rows.good() {
                    return self.gateway_failed(rows.why());
                }
                let v = match json::parse(&rows.body) {
                    Ok(v) => v,
                    Err(e) => return self.gateway_failed(format!("unreadable answer from the gateway: {}", e)),
                };
                self.retries = 0;
                let page = v.get("rows").arr().to_vec();
                let n = page.len();
                for r in page {
                    let d = self.decode(&r);
                    self.decoded.push(d);
                    self.rows.push(r);
                }
                self.cursor = v.get("nextCursor").str().map(String::from);
                self.more = n > 0 && self.cursor.is_some();
                self.next_page()
            }
            Kind::Rpc { calls, what, batched } => match self.rpc_answers(&answers, &calls, batched) {
                Ok(res) => self.rpc_done(what, res),
                Err(RpcFail::Limited) => self.backoff(out, "The Solana RPC kept answering \"too many requests\" (429)".into()),
                Err(RpcFail::NoBatch) => {
                    // this RPC doesn't take batches: one request per call
                    self.batch_ok = false;
                    let o = self.rpc_out(calls, what);
                    self.send(o)
                }
                Err(RpcFail::Down(e)) => Step::Error(format!("The Solana RPC failed: {}", e)),
            },
            Kind::Delegate(idx) => {
                if results.len() != idx.len() {
                    return Step::Error("the loader returned the wrong number of decoded packs".into());
                }
                for (i, r) in idx.iter().zip(results) {
                    let row = &self.rows[*i];
                    self.decoded[*i] = Some(match records::pack_from_json(r.get("ok")) {
                        Some((schema, recs, meta)) => Ok(records::pack_of(row, schema, recs, meta)),
                        None => Err(r.get("error").str().map(String::from).unwrap_or_else(|| "another decoder couldn't read this pack".into())),
                    });
                }
                self.finish()
            }
        }
    }

    /// The gateway couldn't answer: read Solana instead if we may.
    fn gateway_failed(&mut self, why: String) -> Step {
        if self.cfg.prefer == Prefer::Auto && self.cfg.rpc.is_some() {
            self.notes.push(format!("IQ's gateway couldn't answer ({}), so the table was read straight from Solana.", why));
            self.via = Via::Solana;
            self.rows.clear();
            self.decoded.clear();
            self.cursor = None;
            self.more = true;
            self.retries = 0;
            self.ckpt = None;
            self.ckpt_seen.clear();
            self.ckpt_scan = 0;
            self.crowd = false;
            return self.rpc_first();
        }
        Step::Error(format!("IQ's gateway couldn't answer: {}", why))
    }

    /// After a page: read the next one, or finish.
    fn next_page(&mut self) -> Step {
        if self.rows.len() >= self.cfg.max_rows && self.more {
            self.truncated = true;
            self.more = false;
        }
        if self.more && self.checkpoint_reached() {
            self.more = false;
        }
        if !self.more {
            return self.finish();
        }
        match self.via {
            Via::Gateway => {
                let r = self.gw_rows_req();
                self.send(Out { reqs: vec![r], kind: Kind::GwPage })
            }
            Via::Solana => {
                let call = with_id(net::sigs_request(&self.cfg.table, SIG_PAGE, self.cursor.as_deref(), 0), 0);
                let o = self.rpc_out(vec![call], RpcWhat::Sigs);
                self.send(o)
            }
        }
    }

    /// Reading official rows newest first can stop at the owner's latest
    /// checkpoint once the packs it lists are in: it rewrote every live
    /// record, so older packs add nothing. (What `pack::checkpoint_covers`
    /// decides, kept up as rows arrive; only the owner's packs count.)
    fn checkpoint_reached(&mut self) -> bool {
        let Some(owner) = self.cfg.official.clone() else { return false };
        if self.cfg.who != Who::Official {
            return false;
        }
        for i in self.ckpt_scan..self.decoded.len() {
            let Some(Ok(p)) = &self.decoded[i] else { continue };
            if p.signer != owner {
                continue;
            }
            // a crowdfunded upload is read whole
            if pack::looks_crowd(&[p]) {
                self.crowd = true;
            }
            match (&p.meta, &self.ckpt) {
                (Some(m), None) => {
                    if let Some(d) = iq_tables::schema::Doc::from_json(m).filter(|d| !d.snap.is_empty()) {
                        self.ckpt = Some((i, d.snap.clone()));
                    }
                }
                (None, Some(_)) => {
                    self.ckpt_seen.insert(p.id.clone());
                }
                _ => {}
            }
        }
        self.ckpt_scan = self.decoded.len();
        !self.crowd && self.ckpt.as_ref().map(|(_, snap)| snap.iter().all(|id| self.ckpt_seen.contains(id))).unwrap_or(false)
    }

    // ------------------------------------------------------------ Solana

    fn rpc_url(&self) -> String {
        self.cfg.rpc.clone().unwrap_or_default()
    }

    fn rpc_out(&self, calls: Vec<Json>, what: RpcWhat) -> Out {
        let url = self.rpc_url();
        let reqs = if self.batch_ok {
            calls.chunks(BATCH).map(|c| Req::post(&url, Json::Arr(c.to_vec()).to_string())).collect()
        } else {
            calls.iter().map(|c| Req::post(&url, c.to_string())).collect()
        };
        Out { reqs, kind: Kind::Rpc { calls, what, batched: self.batch_ok } }
    }

    fn rpc_first(&mut self) -> Step {
        let info = rpc_call(
            0,
            "getAccountInfo",
            Json::Arr(vec![json::s(&self.cfg.table), json::obj(vec![("encoding", json::s("base64")), ("commitment", json::s("confirmed"))])]),
        );
        let sigs = with_id(net::sigs_request(&self.cfg.table, SIG_PAGE, None, 1), 1);
        let o = self.rpc_out(vec![info, sigs], RpcWhat::First);
        self.send(o)
    }

    /// Each call's result, in call order.
    fn rpc_answers(&self, answers: &[Answer], calls: &[Json], batched: bool) -> Result<Vec<Result<Json, String>>, RpcFail> {
        if answers.iter().any(|a| a.limited()) {
            return Err(RpcFail::Limited);
        }
        if batched {
            let mut by_id: BTreeMap<u64, Result<Json, String>> = BTreeMap::new();
            for a in answers {
                match json::parse(&a.body) {
                    Ok(Json::Arr(items)) if a.good() => {
                        for it in items {
                            if let Some(id) = it.get("id").u64() {
                                let r = if it.get("error").is_null() {
                                    Ok(it.get("result").clone())
                                } else {
                                    Err(it.get("error").get("message").str_or("RPC error"))
                                };
                                by_id.insert(id, r);
                            }
                        }
                    }
                    // an answer that isn't a list: no batches here (or a failure we'll see unbatched)
                    _ if a.ok => return Err(RpcFail::NoBatch),
                    _ => return Err(RpcFail::Down(a.why())),
                }
            }
            Ok(calls.iter().map(|c| by_id.remove(&c.get("id").u64().unwrap_or(u64::MAX)).unwrap_or_else(|| Err("no answer".into()))).collect())
        } else {
            if answers.len() != calls.len() {
                return Err(RpcFail::Down("the loader returned the wrong number of answers".into()));
            }
            answers
                .iter()
                .map(|a| {
                    if !a.ok {
                        return Err(RpcFail::Down(a.why()));
                    }
                    Ok(net::rpc_result(&a.body).map_err(|e| if a.good() { e } else { a.why() }))
                })
                .collect()
        }
    }

    fn rpc_done(&mut self, what: RpcWhat, res: Vec<Result<Json, String>>) -> Step {
        match what {
            RpcWhat::First | RpcWhat::Sigs => {
                let sigs = if matches!(what, RpcWhat::First) {
                    if let Ok(v) = &res[0] {
                        if let Some(m) = net::account_data(v.get("value")).and_then(|d| iq_tables::iq::decode_table(&d)) {
                            self.meta = Some(iq_tables::iq::meta_json(&m));
                        } else if v.get("value").is_null() {
                            return Step::Error(format!("No IQ table at {} on this RPC's cluster.", self.cfg.table));
                        }
                    }
                    &res[1]
                } else {
                    &res[0]
                };
                let list = match sigs {
                    Ok(v) => v.arr().to_vec(),
                    Err(e) => return Step::Error(format!("The Solana RPC couldn't list the table's transactions: {}", e)),
                };
                self.retries = 0;
                self.cursor = list.last().and_then(|x| x.get("signature").str()).map(String::from);
                self.more = list.len() >= SIG_PAGE;
                self.page_sigs = list.iter().filter(|x| x.get("err").is_null()).filter_map(|x| x.get("signature").str().map(String::from)).collect();
                self.page_rows.clear();
                let idx: Vec<usize> = (0..self.page_sigs.len()).collect();
                self.read_txs(idx)
            }
            RpcWhat::Txs(idx) => {
                let mut failed = vec![];
                let mut failed_errs = vec![];
                for (k, r) in idx.iter().zip(res) {
                    match r {
                        Ok(v) => {
                            self.page_rows.insert(*k, net::rows_from_tx(&v, &self.cfg.table));
                        }
                        Err(e) => {
                            failed.push(*k);
                            failed_errs.push(e);
                        }
                    }
                }
                if !failed.is_empty() {
                    let why = format!(
                        "The Solana RPC couldn't return {} of the table's transactions ({}). Reading a table from Solana needs an RPC that keeps full history.",
                        failed_errs.len(),
                        failed_errs.first().cloned().unwrap_or_default()
                    );
                    let calls: Vec<Json> = failed.iter().enumerate().map(|(n, &k)| net::tx_request(&self.page_sigs[k], n)).collect();
                    let o = self.rpc_out(calls, RpcWhat::Txs(failed));
                    return self.backoff(o, why);
                }
                self.retries = 0;
                // the page's rows in history order (newest first)
                let start = self.rows.len();
                self.page_start = start;
                for (_, rows) in std::mem::take(&mut self.page_rows) {
                    for r in rows {
                        let d = self.decode(&r);
                        self.decoded.push(d);
                        self.rows.push(r);
                    }
                }
                for i in start..self.rows.len() {
                    let r = &self.rows[i];
                    if r.get("__pending").bool() != Some(true) || !self.wanted(r) {
                        continue;
                    }
                    let path = r.get("__onChainPath").str_or("");
                    let total = r.get("__chunks").u64().unwrap_or(0) as usize;
                    // several rows can point at the same parts: read them once — but only
                    // share with a row of the same standing, so someone else's placeholder
                    // (with its own part count) can't stand in for the owner's upload
                    let official = self.is_official(r);
                    if let Some(c) = self.chunks.iter().position(|c| match c {
                        Chunk::Session { session, official: o, .. } => *session == path && *o == official,
                        Chunk::Linked { start, official: o, .. } => *start == path && *o == official,
                    }) {
                        self.aliases.push((i, c));
                        continue;
                    }
                    if official {
                        if self.chunked >= MAX_CHUNKED {
                            continue;
                        }
                        self.chunked += 1;
                    } else {
                        if self.others_chunked >= OTHERS_CHUNKED {
                            continue;
                        }
                        self.others_chunked += 1;
                    }
                    self.chunks.push(if net::is_linked_path(&path) {
                        Chunk::Linked { row: i, official, start: path.clone(), next: path, parts: vec![], done: false }
                    } else {
                        Chunk::Session { row: i, official, session: path, listed: false, sigs: vec![], parts: BTreeMap::new(), total, done: false }
                    });
                }
                self.chunk_round()
            }
            RpcWhat::Chunks(map) => {
                for ((c, listing), r) in map.iter().zip(res) {
                    let chunk = &mut self.chunks[*c];
                    match (chunk, r) {
                        (Chunk::Session { listed, sigs, total, done, parts, .. }, Ok(v)) if *listing => {
                            *listed = true;
                            // oldest first, so a part sent again (newer) wins
                            *sigs =
                                v.arr().iter().filter(|x| x.get("err").is_null()).filter_map(|x| x.get("signature").str().map(String::from)).rev().collect();
                            // a session holds its parts plus a few extra transactions; an
                            // address with far more is something else (and isn't read)
                            if sigs.len() > session_limit(*total) {
                                sigs.clear();
                                parts.clear();
                                *done = true;
                            }
                        }
                        (Chunk::Session { parts, .. }, Ok(v)) => {
                            for (i, text) in net::session_parts(&v) {
                                parts.insert(i, text);
                            }
                        }
                        (Chunk::Linked { next, parts, done, .. }, Ok(v)) => match net::linked_part(&v) {
                            Some((text, before)) if parts.len() < MAX_HOPS => {
                                parts.insert(0, text);
                                if before == "Genesis" || before.is_empty() {
                                    *done = true;
                                } else {
                                    *next = before;
                                }
                            }
                            _ => {
                                parts.clear();
                                *done = true;
                            }
                        },
                        (Chunk::Session { done, parts, .. }, Err(_)) => {
                            parts.clear();
                            *done = true;
                        }
                        (Chunk::Linked { done, parts, .. }, Err(_)) => {
                            parts.clear();
                            *done = true;
                        }
                    }
                }
                // a session whose transactions were all read is complete
                for (c, listing) in &map {
                    if let Chunk::Session { done, sigs, .. } = &mut self.chunks[*c] {
                        if !*listing && sigs.is_empty() {
                            *done = true;
                        }
                    }
                }
                self.chunk_round()
            }
        }
    }

    fn read_txs(&mut self, idx: Vec<usize>) -> Step {
        if idx.is_empty() {
            return self.rpc_done(RpcWhat::Txs(vec![]), vec![]);
        }
        let calls: Vec<Json> = idx.iter().enumerate().map(|(n, &k)| net::tx_request(&self.page_sigs[k], n)).collect();
        let o = self.rpc_out(calls, RpcWhat::Txs(idx));
        self.send(o)
    }

    /// Next requests for chunked rows; when all are in, fill them in and go on.
    fn chunk_round(&mut self) -> Step {
        // a session with nothing listed has nothing more to read
        for chunk in self.chunks.iter_mut() {
            if let Chunk::Session { sigs, listed: true, done, .. } = chunk {
                if sigs.is_empty() {
                    *done = true;
                }
            }
        }
        // a spent budget (the owner's, or everyone else's) leaves the rest unreadable
        let owner_spent = self.chunk_calls >= MAX_CHUNK_CALLS;
        let others_spent = self.others_calls >= OTHERS_CHUNK_CALLS;
        let mut left = 0;
        for chunk in self.chunks.iter_mut() {
            let (official, done) = match chunk {
                Chunk::Session { official, done, .. } | Chunk::Linked { official, done, .. } => (*official, done),
            };
            if !*done && (if official { owner_spent } else { others_spent }) {
                *done = true;
                left += 1;
                match chunk {
                    Chunk::Session { parts, .. } => parts.clear(),
                    Chunk::Linked { parts, .. } => parts.clear(),
                }
            }
        }
        if left > 0 {
            self.notes.push(format!("{} row(s) sent in parts weren't read: this read hit its limit of transactions for them.", left));
        }
        let mut calls = vec![];
        let mut map = vec![];
        let (mut owner_calls, mut other_calls) = (0usize, 0usize);
        // the owner's rows first, so other writers can't use up the round
        for pass in [true, false] {
            for (c, chunk) in self.chunks.iter_mut().enumerate() {
                if calls.len() >= CHUNK_ROUND {
                    break;
                }
                let official = match chunk {
                    Chunk::Session { official, .. } | Chunk::Linked { official, .. } => *official,
                };
                if official != pass {
                    continue;
                }
                let before = calls.len();
                match chunk {
                    Chunk::Session { session, total, listed: false, done: false, .. } => {
                        calls.push(with_id(net::sigs_request(session, session_limit(*total) + 1, None, 0), calls.len()));
                        map.push((c, true));
                    }
                    Chunk::Session { sigs, listed: true, done: false, .. } => {
                        let take = sigs.len().min(CHUNK_ROUND - calls.len());
                        for s in sigs.drain(..take) {
                            calls.push(net::tx_request(&s, calls.len()));
                            map.push((c, false));
                        }
                    }
                    Chunk::Linked { next, done: false, .. } => {
                        calls.push(net::tx_request(next, calls.len()));
                        map.push((c, false));
                    }
                    _ => {}
                }
                if official {
                    owner_calls += calls.len() - before;
                } else {
                    other_calls += calls.len() - before;
                }
            }
        }
        if calls.is_empty() {
            return self.chunks_finished();
        }
        self.chunk_calls += owner_calls;
        self.others_calls += other_calls;
        self.chunk_send(calls, map)
    }

    fn chunk_send(&mut self, calls: Vec<Json>, map: Vec<(usize, bool)>) -> Step {
        let o = self.rpc_out(calls, RpcWhat::Chunks(map));
        self.send(o)
    }

    fn chunks_finished(&mut self) -> Step {
        let texts: Vec<(usize, Option<String>)> = std::mem::take(&mut self.chunks)
            .into_iter()
            .map(|chunk| match chunk {
                Chunk::Session { row, parts, total, .. } => (row, net::join_parts(&parts, total)),
                Chunk::Linked { row, parts, .. } => (row, (!parts.is_empty()).then(|| parts.concat())),
            })
            .collect();
        let mut fill: Vec<(usize, Option<&str>)> = texts.iter().map(|(r, t)| (*r, t.as_deref())).collect();
        for (row, c) in std::mem::take(&mut self.aliases) {
            fill.push((row, texts[c].1.as_deref()));
        }
        let mut skipped = 0;
        let filled: std::collections::HashSet<usize> = fill.iter().map(|(r, _)| *r).collect();
        for i in self.page_start..self.rows.len() {
            if self.rows[i].get("__pending").bool() == Some(true) && !filled.contains(&i) {
                skipped += 1;
                fill.push((i, None));
            }
        }
        if skipped > 0 && self.chunked >= MAX_CHUNKED {
            self.notes.push(format!("{} row(s) sent in parts weren't read (more than {} in one read).", skipped, MAX_CHUNKED));
        }
        for (row, text) in fill {
            let filled = net::chunked_row(text, &self.rows[row]);
            let d = self.decode(&filled);
            self.decoded[row] = d;
            self.rows[row] = filled;
        }
        self.retries = 0;
        self.next_page()
    }

    // ------------------------------------------------------------ finish

    fn finish(&mut self) -> Step {
        // packs in a format this decoder doesn't read go to the one that does
        let foreign: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.decoded[i].is_none() && self.wanted(&self.rows[i]) && records::pack_shaped(&self.rows[i]))
            .filter(|&i| records::format_tag(&self.rows[i]).map(|f| !crate::FORMATS.contains(&f.as_str())).unwrap_or(false))
            .collect();
        if !foreign.is_empty() {
            let items = foreign.iter().map(|&i| (records::format_tag(&self.rows[i]).unwrap_or_default(), self.rows[i].get("p").str_or(""))).collect();
            self.pending = Some(Out { reqs: vec![], kind: Kind::Delegate(foreign) });
            return Step::Decode(items);
        }
        self.done()
    }

    fn done(&self) -> Step {
        let src = Source { rows: &self.rows, decoded: &self.decoded, meta: self.meta.as_ref(), creator: self.cfg.official.as_deref() };
        let (cols, rows) = src.table(self.cfg.who, true);
        let mut notes = self.notes.clone();
        let bad = self.decoded.iter().filter(|d| matches!(d, Some(Err(_)))).count();
        if bad > 0 {
            notes.push(format!("{} pack(s) couldn't be read and were left out.", bad));
        }
        if self.truncated {
            notes.push(format!("Stopped after {} on-chain rows (maxRows); older rows weren't read.", self.rows.len()));
        }
        let as_of = src.as_of();
        let mut out = json::obj(vec![
            ("table", json::s(&self.cfg.table)),
            ("name", self.meta.as_ref().map(|m| m.get("name").clone()).unwrap_or(Json::Null)),
            ("rowsFrom", json::s(self.cfg.who.as_str())),
            ("cols", Json::Arr(cols.iter().map(|c| json::s(c)).collect())),
            ("rows", Json::Arr(rows.iter().map(|r| Json::Arr(r.vals.clone())).collect())),
            ("count", json::n(rows.len())),
            ("source", json::s(if self.via == Via::Gateway { "gateway" } else { "solana" })),
            (
                "asOf",
                match as_of {
                    Some((tx, time)) => json::obj(vec![("tx", json::s(&tx)), ("time", time.map(json::n).unwrap_or(Json::Null))]),
                    None => Json::Null,
                },
            ),
            ("truncated", Json::Bool(self.truncated)),
            ("notes", Json::Arr(notes.iter().map(|n| json::s(n)).collect())),
            ("decoder", json::s(crate::VERSION)),
        ]);
        let note = format!(
            "IQ table {} · {} rows · read {}",
            self.cfg.table,
            self.cfg.who.as_str(),
            if self.via == Via::Gateway { "through IQ's gateway" } else { "from Solana" }
        );
        match self.cfg.format {
            Format::Csv => out.set("data", json::s(&records::csv(&cols, &rows))),
            Format::Json => out.set("data", json::s(&records::json_rows(&cols, &rows).to_string())),
            Format::Html => out.set("data", json::s(&records::html(&cols, &rows, &note))),
            Format::Rows => {}
        }
        Step::Done(out)
    }
}
