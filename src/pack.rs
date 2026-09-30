//! Packed rows ("IQT1"): many logical records inside one on-chain row.
//!
//! On chain, a packed table has two columns, `id` and `p`:
//!   {"id":"<pack id>","p":"IQT1z<compressed, text-safe>"}      (compressed)
//!   {"id":"<pack id>","p":"IQT1j<json>"}                        (readable)
//!
//! Every pack carries its own schema, so packs stay decodable forever even if
//! the table's columns change later. Records are keyed by the table's id
//! column: newer packs replace older records with the same id, and a record
//! can be a tombstone (deleted). Pack ids are content hashes, so an accidental
//! double-inscription of the same pack is harmless.

use crate::codec;
use crate::crypto::{base58, keccak::keccak256};
use crate::iq;
use crate::json::{self, Json};

pub const MAGIC: &str = "IQT1";

#[derive(Clone, Debug, PartialEq)]
pub struct Schema {
    pub cols: Vec<String>,
    pub id: usize,
}

impl Schema {
    pub fn id_col(&self) -> &str {
        &self.cols[self.id]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub vals: Vec<Json>,
    pub deleted: bool,
}

impl Record {
    /// Tombstones only keep their id; everything else is dropped on encode.
    pub fn normalized(&self, s: &Schema) -> Record {
        if !self.deleted {
            return self.clone();
        }
        let mut vals = vec![Json::Null; s.cols.len()];
        vals[s.id] = self.vals.get(s.id).cloned().unwrap_or(Json::Null);
        Record { vals, deleted: true }
    }
    pub fn key(&self, s: &Schema) -> String {
        self.vals.get(s.id).map(|v| v.cell_text()).unwrap_or_default()
    }
}

// ------------------------------------------------------------ binary layout

fn put_str(out: &mut Vec<u8>, s: &str) {
    codec::put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn get_str(b: &[u8], i: &mut usize) -> Option<String> {
    let n = codec::get_varint(b, i)? as usize;
    if *i + n > b.len() {
        return None;
    }
    let s = String::from_utf8(b[*i..*i + n].to_vec()).ok()?;
    *i += n;
    Some(s)
}

/// Columnar layout: all values of column 0, then column 1, ... Similar values
/// sit next to each other, which is what the compressor feeds on.
pub fn layout(schema: &Schema, recs: &[Record]) -> Vec<u8> {
    let mut out = vec![b'Q', 1];
    codec::put_varint(&mut out, schema.cols.len() as u64);
    for c in &schema.cols {
        put_str(&mut out, c);
    }
    codec::put_varint(&mut out, schema.id as u64);
    codec::put_varint(&mut out, recs.len() as u64);
    let dels: Vec<usize> = recs.iter().enumerate().filter(|(_, r)| r.deleted).map(|(i, _)| i).collect();
    codec::put_varint(&mut out, dels.len() as u64);
    for d in dels {
        codec::put_varint(&mut out, d as u64);
    }
    for c in 0..schema.cols.len() {
        for r in recs {
            if r.deleted && c != schema.id {
                out.push(0);
                continue;
            }
            match r.vals.get(c).unwrap_or(&Json::Null) {
                Json::Null => out.push(0),
                Json::Str(s) => {
                    out.push(1);
                    put_str(&mut out, s);
                }
                Json::Num(s) => {
                    out.push(2);
                    put_str(&mut out, s);
                }
                Json::Bool(true) => out.push(3),
                Json::Bool(false) => out.push(4),
                other => {
                    out.push(5);
                    put_str(&mut out, &other.to_string());
                }
            }
        }
    }
    out
}

pub fn unlayout(b: &[u8]) -> Option<(Schema, Vec<Record>)> {
    if b.len() < 2 || b[0] != b'Q' || b[1] != 1 {
        return None;
    }
    let mut i = 2;
    let ncols = codec::get_varint(b, &mut i)? as usize;
    if ncols == 0 || ncols > 4096 {
        return None;
    }
    let mut cols = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        cols.push(get_str(b, &mut i)?);
    }
    let id = codec::get_varint(b, &mut i)? as usize;
    if id >= ncols {
        return None;
    }
    let n = codec::get_varint(b, &mut i)? as usize;
    if n > 1_000_000 {
        return None;
    }
    let nd = codec::get_varint(b, &mut i)? as usize;
    // every value takes at least one byte, so a pack can't claim more values
    // than it has bytes left (this keeps a forged header from allocating gigabytes)
    if n.checked_mul(ncols)? > b.len() - i || nd > n {
        return None;
    }
    let mut recs: Vec<Record> = (0..n).map(|_| Record { vals: vec![Json::Null; ncols], deleted: false }).collect();
    for _ in 0..nd {
        let d = codec::get_varint(b, &mut i)? as usize;
        recs.get_mut(d)?.deleted = true;
    }
    for c in 0..ncols {
        for r in recs.iter_mut() {
            let tag = *b.get(i)?;
            i += 1;
            r.vals[c] = match tag {
                0 => Json::Null,
                1 => Json::Str(get_str(b, &mut i)?),
                2 => {
                    // only real numbers stay numbers (the text is written into JSON as is)
                    let t = get_str(b, &mut i)?;
                    if json::is_number(&t) {
                        Json::Num(t)
                    } else {
                        Json::Str(t)
                    }
                }
                3 => Json::Bool(true),
                4 => Json::Bool(false),
                5 => json::parse(&get_str(b, &mut i)?).ok()?,
                _ => return None,
            };
        }
    }
    Some((Schema { cols, id }, recs))
}

// ---------------------------------------------------------------- payloads

pub fn encode_payload(schema: &Schema, recs: &[Record], compress: bool) -> String {
    if compress {
        let raw = layout(schema, recs);
        format!("{}z{}", MAGIC, codec::to_text(&codec::compress(&raw)))
    } else {
        let rows: Vec<Json> = recs.iter().map(|r| Json::Arr(r.normalized(schema).vals)).collect();
        let dels: Vec<Json> = recs.iter().enumerate().filter(|(_, r)| r.deleted).map(|(i, _)| json::n(i)).collect();
        let mut o = json::obj(vec![("c", Json::Arr(schema.cols.iter().map(|c| json::s(c)).collect())), ("i", json::n(schema.id)), ("r", Json::Arr(rows))]);
        if !dels.is_empty() {
            o.set("d", Json::Arr(dels));
        }
        format!("{}j{}", MAGIC, o)
    }
}

/// A table-structure record ("IQT1s<json>", or "IQT1S<compressed json>").
pub fn encode_schema(doc: &Json, cap: usize) -> String {
    let plain = format!("{}s{}", MAGIC, doc);
    if inscribed_size(&plain) <= cap {
        return plain;
    }
    format!("{}S{}", MAGIC, codec::to_text(&codec::compress(doc.to_string().as_bytes())))
}

/// Decode any pack: data (schema + records) or a structure record.
pub fn decode_any(p: &str) -> Result<(Schema, Vec<Record>, Option<Json>), String> {
    decode_any_max(p, codec::MAX_RAW)
}

/// The size a pack says it unpacks to, read from its first few characters
/// (before doing any of the work).
pub fn unpacked_size(p: &str) -> Option<u64> {
    let body = p.strip_prefix(MAGIC)?;
    let mode = body.chars().next()?;
    let rest = &body[mode.len_utf8()..];
    match mode {
        'z' | 'S' => {
            // 13 bits per character pair: 16 characters hold any varint
            let head: String = rest.chars().take(16).collect();
            let b = codec::from_text(&head)?;
            let mut i = 0;
            codec::get_varint(&b, &mut i)
        }
        _ => Some(rest.len() as u64),
    }
}

/// `decode_any`, refusing packs that unpack past `max` bytes.
pub fn decode_any_max(p: &str, max: u64) -> Result<(Schema, Vec<Record>, Option<Json>), String> {
    let body = p.strip_prefix(MAGIC).ok_or("not an IQT1 pack")?;
    let none = Schema { cols: vec!["id".into()], id: 0 };
    match body.chars().next() {
        Some('s') => Ok((none, vec![], Some(json::parse(&body[1..])?))),
        Some('S') => {
            let bytes = codec::from_text(&body[1..]).ok_or("bad text encoding")?;
            let raw = codec::decompress_max(&bytes, max).ok_or("bad compressed stream (or too big)")?;
            let text = String::from_utf8(raw).map_err(|_| "bad structure record")?;
            Ok((none, vec![], Some(json::parse(&text)?)))
        }
        _ => decode_payload_max(p, max).map(|(s, r)| (s, r, None)),
    }
}

pub fn is_packed(v: &Json) -> bool {
    v.get("p").str().map(|p| p.starts_with(MAGIC)).unwrap_or(false)
}

pub fn decode_payload(p: &str) -> Result<(Schema, Vec<Record>), String> {
    decode_payload_max(p, codec::MAX_RAW)
}

fn decode_payload_max(p: &str, max: u64) -> Result<(Schema, Vec<Record>), String> {
    let body = p.strip_prefix(MAGIC).ok_or("not an IQT1 pack")?;
    if p.len() as u64 > max.saturating_mul(2) + 4096 {
        return Err("pack too big".into());
    }
    let mode = body.chars().next().ok_or("empty pack")?;
    let rest = &body[mode.len_utf8()..];
    match mode {
        'z' => {
            let bytes = codec::from_text(rest).ok_or("bad text encoding")?;
            let raw = codec::decompress_max(&bytes, max).ok_or("bad compressed stream (or too big)")?;
            unlayout(&raw).ok_or_else(|| "bad pack layout".to_string())
        }
        'j' => {
            let v = json::parse(rest)?;
            let cols: Vec<String> = v.get("c").arr().iter().map(|c| c.str_or("")).collect();
            let id = v.get("i").u64().unwrap_or(0) as usize;
            if cols.is_empty() || cols.len() > 4096 || id >= cols.len() {
                return Err("bad schema".into());
            }
            // rows are padded to the columns: keep that proportional to the pack's size
            if v.get("r").arr().len().checked_mul(cols.len()).map(|n| n > rest.len() + 4096).unwrap_or(true) {
                return Err("bad pack size".into());
            }
            let mut recs: Vec<Record> = v
                .get("r")
                .arr()
                .iter()
                .map(|r| {
                    let mut vals = r.arr().to_vec();
                    vals.resize(cols.len(), Json::Null);
                    Record { vals, deleted: false }
                })
                .collect();
            for d in v.get("d").arr() {
                if let Some(r) = d.u64().and_then(|d| recs.get_mut(d as usize)) {
                    r.deleted = true;
                }
            }
            Ok((Schema { cols, id }, recs))
        }
        _ => Err(format!("unknown pack mode {}", mode)),
    }
}

pub fn pack_id(payload: &str) -> String {
    base58::encode(&keccak256(payload.as_bytes())[..10])
}

pub fn row_json(payload: &str) -> String {
    json::obj(vec![("id", json::s(&pack_id(payload))), ("p", json::s(payload))]).to_string()
}

/// Worst-case metadata size for this payload (10-digit sequence number).
pub fn inscribed_size(payload: &str) -> usize {
    iq::inline_metadata(9_999_999_999, &row_json(payload)).len()
}

// ------------------------------------------------------------------ planner

#[derive(Clone, Debug)]
pub struct PlannedPack {
    pub first: usize,
    pub count: usize,
    pub payload: String,
    pub pack_id: String,
    pub raw_bytes: usize,
    pub size: usize,
    /// Sent in several transactions with IQ's chunked upload (0 = one
    /// direct write).
    pub chunks: usize,
    /// Unsaved rows (indexes in the draft table) this pack saves.
    pub ghosts: Vec<usize>,
}

impl PlannedPack {
    /// What writing this pack costs (program fee + network fees, and the
    /// session account's deposit).
    pub fn cost(&self) -> u64 {
        write_cost(self.chunks)
    }
}

/// Cost of one write: direct (chunks = 0), a linked list (< 10 chunks) or a
/// session (10+). Measured against IQ's program on devnet.
pub fn write_cost(chunks: usize) -> u64 {
    let tx = iq::TX_FEE;
    match chunks {
        0 => iq::FEE_DIRECT_WRITE + tx,
        n if n < iq::LINKED_LIST_THRESHOLD => iq::FEE_LINKED_WRITE + (n as u64 + 1) * tx,
        n => iq::FEE_SESSION_WRITE + iq::SESSION_RENT_ESTIMATE + (n as u64 + 1) * tx,
    }
}

/// Chunks the SDK would split this row into (`to_chunks` over the row JSON).
pub fn chunk_count(payload: &str, chunk_size: usize) -> usize {
    let row = row_json(payload);
    if row.len() <= chunk_size {
        return 1;
    }
    iq::to_chunks(&row, chunk_size).len()
}

/// The cheapest way to write these records: one direct write per pack (the
/// old way, each ≤ `cap`), or everything as one pack sent in chunks (IQ's
/// linked-list or session upload — cheaper from about four packs on, has no
/// size limit, and appears all at once when its last transaction lands).
pub fn plan_best(schema: &Schema, recs: &[Record], cap: usize, chunk_size: usize, compress: bool) -> Result<Vec<PlannedPack>, String> {
    if recs.is_empty() {
        return Ok(vec![]);
    }
    let direct = plan(schema, recs, cap, compress);
    let direct_cost = direct.as_ref().map(|p| p.iter().map(|x| x.cost()).sum::<u64>()).ok();
    if let Ok(p) = &direct {
        if p.len() <= 1 {
            return direct;
        }
    }
    let payload = encode_payload(schema, recs, compress);
    let n = chunk_count(&payload, chunk_size);
    let one = PlannedPack {
        first: 0,
        count: recs.len(),
        pack_id: pack_id(&payload),
        raw_bytes: layout(schema, recs).len(),
        size: payload.len(),
        chunks: n.max(2),
        payload,
        ghosts: vec![],
    };
    match direct_cost {
        Some(c) if c <= one.cost() => direct,
        _ => Ok(vec![one]),
    }
}

/// Split records into as few packs as possible, each small enough to be
/// inscribed as a single direct write (metadata ≤ `cap` bytes).
pub fn plan(schema: &Schema, recs: &[Record], cap: usize, compress: bool) -> Result<Vec<PlannedPack>, String> {
    let mut out = vec![];
    let mut i = 0;
    let mut guess = 8usize;
    let try_k = |i: usize, k: usize| -> (bool, String) {
        let p = encode_payload(schema, &recs[i..i + k], compress);
        (inscribed_size(&p) <= cap, p)
    };
    while i < recs.len() {
        let remaining = recs.len() - i;
        let (ok1, p1) = try_k(i, 1);
        if !ok1 {
            return Err(format!(
                "record {} (id \"{}\") is too large to fit in one inscription ({} bytes > {})",
                i + 1,
                recs[i].key(schema),
                inscribed_size(&p1),
                cap
            ));
        }
        // exponential probe from the last pack's size, then binary search
        let mut good = 1;
        let mut good_p = p1;
        let mut bad = remaining + 1;
        let mut k = guess.min(remaining).max(1);
        loop {
            if k <= good {
                k = good + 1;
            }
            if k >= bad {
                break;
            }
            let (ok, p) = try_k(i, k);
            if ok {
                good = k;
                good_p = p;
                if k == remaining {
                    break;
                }
                k = (k * 2).min(remaining);
            } else {
                bad = k;
                break;
            }
        }
        while bad - good > 1 {
            let mid = (good + bad) / 2;
            let (ok, p) = try_k(i, mid);
            if ok {
                good = mid;
                good_p = p;
            } else {
                bad = mid;
            }
        }
        guess = good;
        let size = inscribed_size(&good_p);
        out.push(PlannedPack {
            first: i,
            count: good,
            pack_id: pack_id(&good_p),
            raw_bytes: layout(schema, &recs[i..i + good]).len(),
            payload: good_p,
            size,
            chunks: 0,
            ghosts: vec![],
        });
        i += good;
    }
    Ok(out)
}

// -------------------------------------------------------------------- merge

#[derive(Clone, Debug)]
pub struct SourcePack {
    /// The on-chain row's id (the pack id).
    pub id: String,
    pub tx: String,
    pub signer: String,
    pub time: Option<i64>,
    pub schema: Schema,
    pub recs: Vec<Record>,
    /// Set for a table-structure record.
    pub meta: Option<Json>,
}

#[derive(Clone, Debug)]
pub struct Merged {
    pub key: String,
    pub vals: Vec<(String, Json)>,
    pub tx: String,
    pub signer: String,
    pub time: Option<i64>,
    pub versions: usize,
}

/// Apply packs oldest-first: upserts replace by id, tombstones remove.
pub fn merge(packs: &[SourcePack]) -> Vec<Merged> {
    merge_events(packs, &|_| false, &|_| true).0
}

/// Merge with structure records applied in order: the official ones (from
/// the table's owner) can clear everything saved before them and re-key the
/// saved records by a new primary-key column. Returns the records and the
/// latest official structure.
pub fn merge_events(packs: &[SourcePack], official: &dyn Fn(&str) -> bool, take: &dyn Fn(&SourcePack) -> bool) -> (Vec<Merged>, Option<crate::schema::Doc>) {
    let mut order: Vec<String> = vec![];
    let mut map: std::collections::HashMap<String, Merged> = std::collections::HashMap::new();
    let mut doc: Option<crate::schema::Doc> = None;
    // The newest official checkpoint whose packs are all here: start from its
    // packs (the whole table as the owner saved it) and replay only what came
    // after them.
    let mut start = 0;
    let mut skip: Vec<usize> = vec![];
    if let Some((ci, d)) = latest_checkpoint(packs, official) {
        let idx: Vec<usize> = d.snap.iter().filter_map(|id| packs[..ci].iter().rposition(|p| p.meta.is_none() && p.id == *id && official(&p.signer))).collect();
        if idx.len() == d.snap.len() {
            let first = *idx.iter().min().unwrap();
            for &i in &idx {
                if take(&packs[i]) {
                    apply_records(&packs[i], &mut map, &mut order);
                }
            }
            let mut plain = d.clone();
            plain.snap.clear();
            doc = Some(plain);
            skip = idx;
            skip.push(ci);
            // structure records between the snapshot and the checkpoint are
            // already part of it
            for (i, p) in packs.iter().enumerate().take(ci).skip(first) {
                if p.meta.is_some() {
                    skip.push(i);
                }
            }
            start = first;
        }
    }
    for (pi, p) in packs.iter().enumerate().skip(start) {
        if skip.contains(&pi) {
            continue;
        }
        if let Some(m) = &p.meta {
            if !official(&p.signer) {
                continue;
            }
            let Some(d) = crate::schema::Doc::from_json(m) else { continue };
            if d.clear {
                map.clear();
                order.clear();
            }
            if doc.as_ref().map(|x| x.pk != d.pk).unwrap_or(true) && !map.is_empty() {
                // re-key what's saved so far by the (new) primary-key column
                let old: Vec<Merged> = order.drain(..).filter_map(|k| map.remove(&k)).collect();
                for mut m in old {
                    if let Some((_, v)) = m.vals.iter().find(|(k, _)| *k == d.pk) {
                        let nk = v.cell_text();
                        if !nk.is_empty() {
                            m.key = nk;
                        }
                    }
                    if !map.contains_key(&m.key) {
                        order.push(m.key.clone());
                    }
                    map.insert(m.key.clone(), m);
                }
            }
            doc = Some(d);
            continue;
        }
        if !take(p) {
            continue;
        }
        apply_records(p, &mut map, &mut order);
    }
    (order.into_iter().filter_map(|k| map.remove(&k)).collect(), doc)
}

fn apply_records(p: &SourcePack, map: &mut std::collections::HashMap<String, Merged>, order: &mut Vec<String>) {
    for r in &p.recs {
        let key = r.key(&p.schema);
        if r.deleted {
            map.remove(&key);
            continue;
        }
        let vals: Vec<(String, Json)> = p.schema.cols.iter().cloned().zip(r.vals.iter().cloned()).collect();
        let versions = map.get(&key).map(|m| m.versions + 1).unwrap_or(1);
        if !map.contains_key(&key) {
            order.push(key.clone());
        }
        map.insert(key.clone(), Merged { key, vals, tx: p.tx.clone(), signer: p.signer.clone(), time: p.time, versions });
    }
}

/// The newest checkpoint written by the owner: (position, its record).
pub fn latest_checkpoint(packs: &[SourcePack], official: &dyn Fn(&str) -> bool) -> Option<(usize, crate::schema::Doc)> {
    packs.iter().enumerate().rev().find_map(|(i, p)| {
        let m = p.meta.as_ref()?;
        if !official(&p.signer) {
            return None;
        }
        let d = crate::schema::Doc::from_json(m)?;
        (!d.snap.is_empty()).then_some((i, d))
    })
}

/// Reading newest first: has a complete official checkpoint been seen, so
/// older history isn't needed?
pub fn checkpoint_covers(newest_first: &[SourcePack], official: &dyn Fn(&str) -> bool) -> bool {
    for (i, p) in newest_first.iter().enumerate() {
        let Some(m) = &p.meta else { continue };
        if !official(&p.signer) {
            continue;
        }
        let Some(d) = crate::schema::Doc::from_json(m) else { continue };
        if d.snap.is_empty() {
            continue;
        }
        // its snapshot packs come after it in newest-first order
        return d.snap.iter().all(|id| newest_first[i..].iter().any(|q| q.meta.is_none() && q.id == *id && official(&q.signer)));
    }
    false
}

/// A crowdfunded upload's table (its manifest is a structure record with
/// `crowd`; registrations have columns piece, sha256, tx). Such a table is
/// read whole: its oldest record is what counts, and anyone may add rows.
pub fn looks_crowd(packs: &[&SourcePack]) -> bool {
    packs.iter().any(|p| match &p.meta {
        Some(m) => !m.get("crowd").is_null(),
        None => ["piece", "sha256", "tx"].iter().all(|k| p.schema.cols.iter().any(|c| c == k)),
    })
}
