//! The IQ Tables decoder: reads a table written by IQ Tables from IQ Labs'
//! gateway (or straight from Solana) and returns its records as CSV, JSON or
//! HTML. It is built from the pure half of IQ Tables only and has no
//! imports, so it can't reach the network, files, clock or anything else on
//! the machine running it: `iqt-loader.js` does the fetching it asks for,
//! and only from the hosts the developer configured.
//!
//! # Interface (ABI 1 — frozen)
//!
//! Exports: `memory`, `iqt_abi() -> 1`, `iqt_alloc(len) -> ptr`,
//! `iqt_call(ptr, len) -> ptr`, `iqt_len() -> len`.
//!
//! `iqt_call` takes one JSON message (UTF-8, in memory from `iqt_alloc`;
//! the call takes that memory back) and returns a JSON reply, valid until the
//! next call. Messages:
//!
//! * `{"op":"info"}` → `{"abi":1,"version":"…","formats":["IQT1"]}`
//! * `{"op":"decode","payload":"IQT1…"}` → `{"ok":{"schema":…,"records":…,"meta":…}}` or `{"error":"…"}`
//! * `{"op":"encode","csv":…|"objects":…|"cols"+"rows","id":…,"mode":…}` → `{"ok":{"row":{"id","p"},…}}`
//!   (added in decoder 1.1; older decoders answer "unknown op")
//! * `{"op":"unpack","payload":…,"format":"csv"|"json"}` → `{"ok":{"text":…,"records":n,…}}` (1.1)
//! * `{"op":"read","config":{…}}` → a step
//! * `{"op":"resume","results":[…]}` → the next step
//!
//! Steps: `{"fetch":[{"url","method","body","type"}]}` (answer with one
//! `{"ok","status","body"}` per request), `{"decode":[{"format","payload"}]}`
//! (answer with another decoder's `decode` reply per item), `{"wait":ms}`
//! (answer with `[]`), `{"done":{…}}` or `{"error":"…"}`.
//!
//! Requests may only be GETs under `config.gateway`, or JSON-RPC POSTs of
//! read-only methods (getAccountInfo, getMultipleAccounts,
//! getSignaturesForAddress, getTransaction, getSlot, getBlockTime) to exactly
//! `config.rpc` — which loaders may set to a stand-in address
//! (`https://rpc.invalid/`) so the decoder never sees the developer's RPC URL
//! or API key. Loaders refuse anything else, and refuse redirects.
//!
//! `decode` is pure: a payload always gives the same records, forever. Only
//! `read` knows how IQ Labs stores data, and loaders always run the newest
//! decoder's `read`, handing formats it no longer knows to the older decoder
//! that the format registry (`iqt-formats.json`) names.

pub mod reader;
pub mod tools;

use iq_tables::json::{self, Json};
use std::cell::RefCell;

pub const ABI: u32 = 1;
/// Storage formats this decoder reads.
pub const FORMATS: &[&str] = &["IQT1"];
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

thread_local! {
    static READER: RefCell<Option<reader::Reader>> = const { RefCell::new(None) };
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn err(msg: &str) -> Json {
    json::obj(vec![("error", json::s(msg))])
}

/// The pure decode of one payload (the `decode` op).
pub fn decode(payload: &str) -> Json {
    let tag = iq_tables::records::format_tag(&json::obj(vec![("p", json::s(payload))]));
    if !tag.map(|t| FORMATS.contains(&t.as_str())).unwrap_or(false) {
        return err("unsupported format");
    }
    match iq_tables::pack::decode_any(payload) {
        Ok((schema, recs, meta)) => json::obj(vec![("ok", iq_tables::records::pack_json(&schema, &recs, &meta))]),
        Err(e) => err(&e),
    }
}

/// One message in, one reply out (what `iqt_call` does, minus the memory).
pub fn handle(msg: &str) -> String {
    let m = match json::parse(msg) {
        Ok(m) => m,
        Err(e) => return err(&format!("bad message: {}", e)).to_string(),
    };
    let reply = match m.get("op").str() {
        Some("info") => {
            json::obj(vec![("abi", json::n(ABI)), ("version", json::s(VERSION)), ("formats", Json::Arr(FORMATS.iter().map(|f| json::s(f)).collect()))])
        }
        Some("decode") => decode(&m.get("payload").str_or("")),
        Some("encode") => tools::encode(&m),
        Some("unpack") => tools::unpack(&m),
        Some("read") => match reader::Config::from_json(m.get("config")) {
            Ok(cfg) => {
                let mut r = reader::Reader::new(cfg);
                let step = r.start();
                READER.with(|x| *x.borrow_mut() = Some(r));
                step.to_json()
            }
            Err(e) => err(&e),
        },
        Some("resume") => READER.with(|x| match x.borrow_mut().as_mut() {
            Some(r) => r.resume(m.get("results").arr()).to_json(),
            None => err("nothing to resume: send a read first"),
        }),
        _ => err("unknown op"),
    };
    reply.to_string()
}

#[no_mangle]
pub extern "C" fn iqt_abi() -> u32 {
    ABI
}

#[no_mangle]
pub extern "C" fn iqt_alloc(len: usize) -> *mut u8 {
    let mut v: Vec<u8> = Vec::with_capacity(len.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `p` must come from `iqt_alloc(len)`; this call takes it back.
#[no_mangle]
pub unsafe extern "C" fn iqt_call(p: *mut u8, len: usize) -> *const u8 {
    let input = Vec::from_raw_parts(p, len, len.max(1));
    let msg = String::from_utf8_lossy(&input).into_owned();
    drop(input);
    let out = handle(&msg);
    OUT.with(|o| {
        let mut o = o.borrow_mut();
        *o = out.into_bytes();
        o.as_ptr()
    })
}

#[no_mangle]
pub extern "C" fn iqt_len() -> usize {
    OUT.with(|o| o.borrow().len())
}

#[cfg(test)]
mod tests;
