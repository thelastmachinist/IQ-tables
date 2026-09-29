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
