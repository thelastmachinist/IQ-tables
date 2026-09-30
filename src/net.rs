//! Gateway (read) and Solana JSON-RPC (write) request/response helpers.

use crate::crypto::base64_decode;
use crate::json::{self, Json};

#[derive(Clone, Debug)]
pub struct TableRef {
    pub label: Option<String>,
    pub hex: String,
    pub pda: String,
    pub public: bool,
}

#[derive(Clone, Debug)]
pub struct DbRootInfo {
    pub pda: String,
    pub id: Option<String>,
    pub id_hex: String,
    pub creator: String,
    pub table_creators: Vec<String>,
    pub tables: Vec<TableRef>,
}

impl DbRootInfo {
    pub fn name(&self) -> String {
        match &self.id {
            Some(s) if !s.is_empty() => s.clone(),
            _ => format!("#{}", &self.id_hex[..self.id_hex.len().min(10)]),
        }
    }
}

pub fn label_of(t: &TableRef) -> String {
    t.label.clone().unwrap_or_else(|| format!("#{}", &t.hex[..t.hex.len().min(10)]))
}

pub fn parse_dbroots(v: &Json) -> Vec<DbRootInfo> {
    let mut out: Vec<DbRootInfo> = v
        .get("dbroots")
        .arr()
        .iter()
        .map(|d| {
            let mut tables: Vec<TableRef> = vec![];
            for (list, public) in [("tableSeeds", true), ("globalTableSeeds", false)] {
                for t in d.get(list).arr() {
                    let pda = t.get("tablePda").str_or("");
                    if pda.is_empty() || tables.iter().any(|x| x.pda == pda) {
                        continue;
                    }
                    tables.push(TableRef { label: t.get("label").str().map(String::from), hex: t.get("hex").str_or(""), pda, public });
                }
            }
            DbRootInfo {
                pda: d.get("pda").str_or(""),
                id: d.get("id").str().map(String::from),
                id_hex: d.get("idHex").str_or(""),
                creator: d.get("creator").str_or(""),
                table_creators: d.get("tableCreators").arr().iter().map(|x| x.str_or("")).collect(),
                tables,
            }
        })
        .collect();
    sort_roots(&mut out);
    out
}

pub fn sort_roots(v: &mut [DbRootInfo]) {
    v.sort_by_key(|a| (a.id.is_none(), a.name().to_lowercase()));
}

/// A table hint stored in a DbRoot → (readable name, table seed). Names are
/// hashed exactly like the SDK's `toSeedBytes`; raw 32-byte hints are seeds.
pub fn hint_seed(h: &[u8]) -> (Option<String>, Vec<u8>) {
    match std::str::from_utf8(h) {
        Ok(s) if !s.is_empty() && !s.chars().any(|c| c.is_control()) => (Some(s.to_string()), crate::iq::seed_bytes(s)),
        _ if h.len() == 32 => (None, h.to_vec()),
        _ => (None, crate::crypto::keccak::keccak256(h).to_vec()),
    }
}

/// Gateway-shaped database info from a DbRoot account read off the chain.
pub fn dbroot_info(pda: &str, r: &crate::iq::DbRoot) -> DbRootInfo {
    use crate::solana::{b58, parse_pk};
    let root = parse_pk(pda);
    let mut tables: Vec<TableRef> = vec![];
    for (list, public) in [(&r.table_seeds, true), (&r.global_table_seeds, false)] {
        for h in list.iter() {
            let (label, seed) = hint_seed(h);
            let Some(rp) = root else { continue };
            let tp = b58(&crate::iq::table_pda(&rp, &seed));
            if tables.iter().any(|t| t.pda == tp) {
                continue;
            }
            tables.push(TableRef { label, hex: crate::crypto::hex(&seed), pda: tp, public });
        }
    }
    DbRootInfo {
        pda: pda.to_string(),
        id: String::from_utf8(r.id.clone()).ok().filter(|s| !s.is_empty() && !s.chars().any(|c| c.is_control())),
        id_hex: crate::crypto::hex(&r.id),
        creator: b58(&r.creator),
        table_creators: r.table_creators.iter().map(b58).collect(),
        tables,
    }
}

pub fn rpc_body(method: &str, params: Json) -> String {
    json::obj(vec![("jsonrpc", json::s("2.0")), ("id", json::n(1)), ("method", json::s(method)), ("params", params)]).to_string()
}

/// Returns the `result` field, or a readable error (including program logs
/// when the RPC attaches them).
pub fn rpc_result(text: &str) -> Result<Json, String> {
    let v = json::parse(text).map_err(|e| format!("bad RPC response ({}): {}", e, text.chars().take(160).collect::<String>()))?;
    let err = v.get("error");
    if !err.is_null() {
        let mut msg = err.get("message").str_or("RPC error");
        let logs: Vec<String> = err.get("data").get("logs").arr().iter().filter_map(|l| l.str().map(String::from)).collect();
        if !logs.is_empty() {
            msg.push('\n');
            msg.push_str(&logs.iter().rev().take(8).rev().cloned().collect::<Vec<_>>().join("\n"));
        }
        return Err(msg);
    }
    Ok(v.get("result").clone())
}

/// Account data from a base64 `getAccountInfo`/`getMultipleAccounts` value.
pub fn account_data(v: &Json) -> Option<Vec<u8>> {
    if v.is_null() {
        return None;
    }
    base64_decode(v.get("data").idx(0).str()?)
}

pub fn gateway_error(status: u32, body: &str) -> String {
    let v = json::parse(body).ok();
    let m = v.as_ref().and_then(|v| v.get("error").str().map(String::from)).unwrap_or_else(|| body.chars().take(160).collect());
    format!("gateway {}: {}", status, m)
}

// ------------------------------------------------ rows read from Solana

/// A `getTransaction` JSON-RPC request.
pub fn tx_request(sig: &str, id: usize) -> Json {
    json::obj(vec![
        ("jsonrpc", json::s("2.0")),
        ("id", json::n(id)),
        ("method", json::s("getTransaction")),
        (
            "params",
            Json::Arr(vec![
                json::s(sig),
                json::obj(vec![("encoding", json::s("base64")), ("maxSupportedTransactionVersion", json::n(1)), ("commitment", json::s("confirmed"))]),
            ]),
        ),
    ])
}

/// A `getSignaturesForAddress` JSON-RPC request.
pub fn sigs_request(address: &str, limit: usize, before: Option<&str>, id: usize) -> Json {
    let mut cfg = json::obj(vec![("limit", json::n(limit)), ("commitment", json::s("confirmed"))]);
    if let Some(b) = before {
        cfg.set("before", json::s(b));
    }
    json::obj(vec![
        ("jsonrpc", json::s("2.0")),
        ("id", json::n(id)),
        ("method", json::s("getSignaturesForAddress")),
        ("params", Json::Arr(vec![json::s(address), cfg])),
    ])
}

/// The transaction in a `getTransaction` result.
pub fn parsed_tx(result: &Json) -> Option<crate::solana::ParsedTx> {
    let raw = result.get("transaction").idx(0).str().and_then(base64_decode)?;
    crate::solana::parse_tx(&raw)
}

/// Rows (gateway-shaped) from one `getTransaction` result, for table `pda`.
/// A row sent in chunks comes back as a placeholder (`__pending`, with
/// `__onChainPath` and `__chunks`) until its parts are read.
pub fn rows_from_tx(result: &Json, pda: &str) -> Vec<Json> {
    use crate::iq;
    use crate::solana::b58;
    let mut out = vec![];
    if result.is_null() || !result.get("meta").get("err").is_null() {
        return out;
    }
    let Some(tx) = parsed_tx(result) else { return out };
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
        let mut row = if !d.on_chain_path.is_empty() {
            // a chunked upload: a placeholder, filled in once its parts are read
            let total = json::parse(&d.metadata).ok().and_then(|m| m.get("total_chunks").u64()).unwrap_or(0);
            json::obj(vec![("__onChainPath", json::s(&d.on_chain_path)), ("__chunks", json::n(total)), ("__pending", Json::Bool(true))])
        } else {
            match iq::row_from_metadata(&d.metadata) {
                // `__…` fields are the reader's: a row can't set them itself
                Some(Json::Obj(o)) => Json::Obj(o.into_iter().filter(|(k, _)| !k.starts_with("__")).collect()),
                Some(r) => r,
                None => continue,
            }
        };
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

/// A chunked row's on-chain path is a session address (short) or the tail
/// of IQ's older linked list of transactions (a signature, long).
pub fn is_linked_path(path: &str) -> bool {
    path.len() >= 80
}

/// The parts of a session upload carried by one transaction: (index, text).
pub fn session_parts(result: &Json) -> Vec<(u32, String)> {
    let pid = crate::iq::program_id();
    let Some(tx) = parsed_tx(result) else { return vec![] };
    tx.ixs.iter().filter(|(p, _, _)| tx.keys.get(*p) == Some(&pid)).filter_map(|(_, _, data)| crate::iq::decode_post_chunk(data)).collect()
}

/// One link of IQ's older chunk list: (text, previous signature or "Genesis").
pub fn linked_part(result: &Json) -> Option<(String, String)> {
    let pid = crate::iq::program_id();
    let tx = parsed_tx(result)?;
    tx.ixs.iter().find_map(|(p, _, data)| (tx.keys.get(*p) == Some(&pid)).then(|| crate::iq::decode_send_code(data)).flatten())
}

/// A session's text once every part up to `total` is in (None if any is missing).
pub fn join_parts(parts: &std::collections::BTreeMap<u32, String>, total: usize) -> Option<String> {
    let n = if total > 0 { total } else { parts.len() };
    (0..n as u32).all(|i| parts.contains_key(&i)).then(|| (0..n as u32).map(|i| parts[&i].as_str()).collect::<String>())
}

/// The row a chunked upload holds, with the placeholder's signature, signer
/// and time; an `__unreadable` row if its text isn't a JSON object.
pub fn chunked_row(text: Option<&str>, placeholder: &Json) -> Json {
    let mut row = match text.and_then(|t| json::parse(t).ok()) {
        // `__…` fields are the reader's: the row's own data can't set them
        Some(Json::Obj(o)) => Json::Obj(o.into_iter().filter(|(k, _)| !k.starts_with("__")).collect()),
        _ => json::obj(vec![("__unreadable", Json::Bool(true))]),
    };
    for k in ["__txSignature", "__signer", "__blockTime", "__onChainPath"] {
        if !placeholder.get(k).is_null() {
            row.set(k, placeholder.get(k).clone());
        }
    }
    row
}
