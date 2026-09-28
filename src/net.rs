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
                    tables.push(TableRef {
                        label: t.get("label").str().map(String::from),
                        hex: t.get("hex").str_or(""),
                        pda,
                        public,
                    });
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
    out.sort_by(|a, b| (a.id.is_none(), a.name().to_lowercase()).cmp(&(b.id.is_none(), b.name().to_lowercase())));
    out
}

pub fn rpc_body(method: &str, params: Json) -> String {
    json::obj(vec![
        ("jsonrpc", json::s("2.0")),
        ("id", json::n(1)),
        ("method", json::s(method)),
        ("params", params),
    ])
    .to_string()
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
            msg.push_str("\n");
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
