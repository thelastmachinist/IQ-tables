//! A table's records from its on-chain rows: decoding IQ Tables packs,
//! merging versions, keeping the official writer's rows (or everyone's) and
//! laying the result out as columns and rows. Shared by the explorer, the
//! snapshot embeds and the embeddable decoder, so all three show a table the
//! same way.

use crate::json::{self, Json};
use crate::pack::{self, Merged, SourcePack};
use crate::schema::Doc;
use crate::ui;

/// Whose rows: the table owner's (official), everyone else's, or both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Who {
    Official,
    Unofficial,
    All,
}

impl Who {
    pub fn parse(s: &str) -> Option<Who> {
        match s {
            "official" => Some(Who::Official),
            "unofficial" => Some(Who::Unofficial),
            "all" => Some(Who::All),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Who::Official => "official",
            Who::Unofficial => "unofficial",
            Who::All => "all",
        }
    }
}

pub struct VRow {
    pub key: String,
    pub vals: Vec<Json>,
    pub signer: String,
    pub tx: String,
    pub time: Option<i64>,
    pub official: Option<bool>,
    pub versions: usize,
    pub packed: bool,
}

/// The pack in a gateway-shaped row, or None when the row isn't an IQ
/// Tables pack this code reads (a plain row, or a format it doesn't know).
pub fn decode_row(r: &Json) -> Option<Result<SourcePack, String>> {
    decode_row_for(r, None, &mut 0)
}

/// Unpacked size of one pack from someone other than the table's owner, and
/// of all of them in one read. Anyone can write to a table and unpacking is
/// slow, so without these a few forged rows could stall everyone reading it.
/// (Community rows, crowdfunded registrations included, are small.)
pub const OTHERS_PACK_MAX: u64 = 64 << 10;
pub const OTHERS_READ_MAX: u64 = 4 << 20;

/// `decode_row`, knowing the official wallet: rows from anyone else are held
/// to `OTHERS_PACK_MAX` each and `OTHERS_READ_MAX` in all (`used` keeps count).
pub fn decode_row_for(r: &Json, official: Option<&str>, used: &mut u64) -> Option<Result<SourcePack, String>> {
    // exactly IQT1 ("IQT10…" is a different format)
    if format_tag(r).as_deref() != Some(pack::MAGIC) {
        return None;
    }
    let p = r.get("p").str()?;
    let theirs = official.map(|o| r.get("__signer").str() != Some(o)).unwrap_or(false);
    let max = if theirs {
        let size = pack::unpacked_size(p).unwrap_or(u64::MAX);
        if size > OTHERS_PACK_MAX || *used + size > OTHERS_READ_MAX {
            return Some(Err("too big for a row from someone other than the table's owner".into()));
        }
        *used += size;
        OTHERS_PACK_MAX
    } else {
        crate::codec::MAX_RAW
    };
    Some(pack::decode_any_max(p, max).map(|(schema, recs, meta)| pack_of(r, schema, recs, meta)))
}

/// Shaped like an IQ Tables pack row: an `id` and a `p`, nothing else
/// (besides the reader's `__…` fields).
pub fn pack_shaped(r: &Json) -> bool {
    r.obj().iter().all(|(k, _)| k == "id" || k == "p" || k.starts_with("__")) && r.get("p").str().is_some()
}

/// A decoded pack with the row's signature, signer and time.
pub fn pack_of(r: &Json, schema: pack::Schema, recs: Vec<pack::Record>, meta: Option<Json>) -> SourcePack {
    SourcePack {
        id: r.get("id").str_or(""),
        tx: r.get("__txSignature").str_or(""),
        signer: r.get("__signer").str_or(""),
        time: r.get("__blockTime").f64().map(|f| f as i64),
        schema,
        recs,
        meta,
    }
}

/// The storage format of a row written by IQ Tables ("IQT1", …), known to
/// this code or not. Every pack starts with its format, so a reader can
/// always tell which decoder it needs.
pub fn format_tag(r: &Json) -> Option<String> {
    let rest = r.get("p").str()?.strip_prefix("IQT")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!digits.is_empty()).then(|| format!("IQT{}", digits))
}

/// A pack as JSON — the `decode` result of the decoder's frozen interface
/// (ABI 1): `{"schema":{"cols":[…],"id":n},"records":[{"vals":[…],"deleted":b}],"meta":…}`.
pub fn pack_json(schema: &pack::Schema, recs: &[pack::Record], meta: &Option<Json>) -> Json {
    json::obj(vec![
        ("schema", json::obj(vec![("cols", Json::Arr(schema.cols.iter().map(|c| json::s(c)).collect())), ("id", json::n(schema.id))])),
        ("records", Json::Arr(recs.iter().map(|r| json::obj(vec![("vals", Json::Arr(r.vals.clone())), ("deleted", Json::Bool(r.deleted))])).collect())),
        ("meta", meta.clone().unwrap_or(Json::Null)),
    ])
}

/// The inverse of `pack_json`, for packs another decoder read.
pub fn pack_from_json(v: &Json) -> Option<(pack::Schema, Vec<pack::Record>, Option<Json>)> {
    let cols: Vec<String> = v.get("schema").get("cols").arr().iter().map(|c| c.str().map(String::from)).collect::<Option<_>>()?;
    let id = v.get("schema").get("id").u64()? as usize;
    if id >= cols.len() {
        return None;
    }
    let recs =
        v.get("records").arr().iter().map(|r| pack::Record { vals: r.get("vals").arr().to_vec(), deleted: r.get("deleted").bool().unwrap_or(false) }).collect();
    let meta = Some(v.get("meta").clone()).filter(|m| !m.is_null());
    Some((pack::Schema { cols, id }, recs, meta))
}

/// Everything read of one table.
pub struct Source<'a> {
    /// Gateway-shaped rows, newest first.
    pub rows: &'a [Json],
    /// `decode_row` of each row (same order).
    pub decoded: &'a [Option<Result<SourcePack, String>>],
    /// Gateway-shaped table metadata (on-chain columns), if read.
    pub meta: Option<&'a Json>,
    /// The official wallet (the database's creator), if known.
    pub creator: Option<&'a str>,
}

impl Source<'_> {
    /// Written by IQ Tables: the official wallet wrote a readable pack, so a
    /// pack-like row from someone else doesn't turn a plain table into a
    /// packed one. (While the official wallet isn't known — briefly, in the
    /// explorer — any readable pack counts.)
    pub fn packed(&self) -> bool {
        self.decoded.iter().any(|d| match d {
            Some(Ok(p)) => self.creator.map(|c| c == p.signer).unwrap_or(true),
            _ => false,
        })
    }

    /// Every decoded pack (data and structure records), oldest first.
    pub fn packs(&self) -> Vec<SourcePack> {
        self.decoded.iter().rev().filter_map(|d| d.as_ref().and_then(|r| r.as_ref().ok())).cloned().collect()
    }

    /// Records of one group of writers (None = all), with the owner's
    /// structure records applied, and the owner's latest structure.
    pub fn merged(&self, official: Option<bool>) -> (Vec<Merged>, Option<Doc>) {
        let packs = self.packs();
        let creator = self.creator;
        let is_owner = |s: &str| creator.map(|c| c == s).unwrap_or(true);
        let take = |p: &SourcePack| match (official, creator) {
            (None, _) | (_, None) => true,
            (Some(o), Some(c)) => (p.signer == c) == o,
        };
        pack::merge_events(&packs, &is_owner, &take)
    }

    /// The newest row read: (signature, block time).
    pub fn as_of(&self) -> Option<(String, Option<i64>)> {
        self.rows
            .iter()
            .max_by_key(|r| r.get("__blockTime").f64().map(|t| t as i64).unwrap_or(0))
            .map(|r| (r.get("__txSignature").str_or(""), r.get("__blockTime").f64().map(|t| t as i64)))
    }

    /// Columns and rows: a packed table's records (when `records`), or the
    /// on-chain rows as stored. Not filtered by text, not sorted.
    pub fn table(&self, who: Who, records: bool) -> (Vec<String>, Vec<VRow>) {
        let mut cols: Vec<String> = vec![];
        let mut rows: Vec<VRow> = vec![];
        let official_of = |signer: &str| self.creator.map(|c| c == signer);
        if records && self.packed() {
            // columns: the owner's structure (names, order, types), then any
            // other keys found in rows (from other writers or older layouts)
            let doc = self.merged(Some(true)).1;
            let mut keys: Vec<(String, Option<crate::schema::ColMeta>)> = vec![];
            if let Some(d) = &doc {
                for (n, m) in &d.cols {
                    cols.push(n.clone());
                    keys.push((m.key.clone(), Some(m.clone())));
                }
            }
            let retired: Vec<String> = doc.as_ref().map(|d| d.keys.retired.clone()).unwrap_or_default();
            let mut add = |recs: Vec<Merged>, official: Option<bool>, rows: &mut Vec<VRow>, cols: &mut Vec<String>| {
                for m in recs {
                    for (c, _) in &m.vals {
                        if !keys.iter().any(|(k, _)| k == c) && !retired.contains(c) {
                            keys.push((c.clone(), None));
                            cols.push(c.clone());
                        }
                    }
                    rows.push(VRow {
                        key: m.key.clone(),
                        vals: m.vals.iter().map(|(k, v)| Json::Arr(vec![Json::Str(k.clone()), v.clone()])).collect(),
                        signer: m.signer.clone(),
                        tx: m.tx.clone(),
                        time: m.time,
                        official: official.or_else(|| official_of(&m.signer)),
                        versions: m.versions,
                        packed: true,
                    });
                }
            };
            if self.creator.is_none() {
                add(self.merged(None).0, None, &mut rows, &mut cols);
            } else {
                if who != Who::Unofficial {
                    add(self.merged(Some(true)).0, Some(true), &mut rows, &mut cols);
                }
                if who != Who::Official {
                    add(self.merged(Some(false)).0, Some(false), &mut rows, &mut cols);
                }
            }
            // align values to the columns, reading them through the types
            for r in rows.iter_mut() {
                let pairs: Vec<(String, Json)> = r.vals.iter().map(|p| (p.idx(0).str_or(""), p.idx(1).clone())).collect();
                r.vals = keys
                    .iter()
                    .map(|(k, m)| match (pairs.iter().find(|(x, _)| x == k), m) {
                        (Some((_, v)), Some(m)) => m.ty.read(v),
                        (Some((_, v)), None) => v.clone(),
                        (None, Some(m)) => m.fill.clone(),
                        (None, None) => Json::Null,
                    })
                    .collect();
            }
        } else {
            if let Some(m) = self.meta {
                for c in m.get("columns").arr() {
                    if let Some(c) = c.str() {
                        cols.push(c.to_string());
                    }
                }
            }
            let keep = |r: &Json| match (who, official_of(&r.get("__signer").str_or(""))) {
                (_, None) | (Who::All, _) => true,
                (Who::Official, Some(o)) => o,
                (Who::Unofficial, Some(o)) => !o,
            };
            // columns only from the rows shown: others' rows add none
            for r in self.rows.iter().filter(|r| keep(r)) {
                for (k, _) in r.obj() {
                    if !k.starts_with("__") && !cols.contains(k) {
                        cols.push(k.clone());
                    }
                }
            }
            for r in self.rows.iter().filter(|r| keep(r)) {
                let signer = r.get("__signer").str_or("");
                let official = official_of(&signer);
                rows.push(VRow {
                    key: r.get("__txSignature").str_or(""),
                    vals: cols.iter().map(|c| r.get(c).clone()).collect(),
                    signer,
                    tx: r.get("__txSignature").str_or(""),
                    time: r.get("__blockTime").f64().map(|f| f as i64),
                    official,
                    versions: 1,
                    packed: pack::is_packed(r),
                });
            }
        }
        (cols, rows)
    }
}

/// A cell for CSV. Text that a spreadsheet would run as a formula (`=…`,
/// `@…`, or `+`/`-` followed by something other than a number) gets a
/// leading `'`, since anyone can write to a table and the file is meant to
/// be opened in Excel or Sheets. Numbers are never touched.
pub fn csv_value(v: &Json) -> String {
    let t = v.cell_text();
    let text = match v {
        Json::Num(n) => !json::is_number(n),
        Json::Str(_) => true,
        _ => false,
    };
    let risky = text
        && match t.chars().next() {
            Some('=' | '@' | '\t' | '\r') => true,
            Some('+' | '-') => {
                let rest = t[1..].trim();
                !rest.is_empty() && rest.parse::<f64>().is_err()
            }
            _ => false,
        };
    ui::csv_cell(&if risky { format!("'{}", t) } else { t })
}

pub fn csv(cols: &[String], rows: &[VRow]) -> String {
    let mut out = cols.iter().map(|c| csv_value(&Json::Str(c.clone()))).collect::<Vec<_>>().join(",");
    out.push('\n');
    for r in rows {
        out.push_str(&r.vals.iter().map(csv_value).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    out
}

/// An array of objects, one per row, keyed by column name.
pub fn json_rows(cols: &[String], rows: &[VRow]) -> Json {
    Json::Arr(rows.iter().map(|r| Json::Obj(cols.iter().cloned().zip(r.vals.iter().cloned()).collect())).collect())
}

/// A plain HTML table (every value escaped), for pasting into a page.
pub fn html(cols: &[String], rows: &[VRow], note: &str) -> String {
    let mut h = String::new();
    if !note.is_empty() {
        h.push_str(&format!("<!-- {} -->\n", note.replace("--", "—")));
    }
    h.push_str("<table class=\"iq-table\">\n<thead><tr>");
    for c in cols {
        h.push_str(&format!("<th>{}</th>", ui::esc(c)));
    }
    h.push_str("</tr></thead>\n<tbody>\n");
    for r in rows {
        h.push_str("<tr>");
        for v in &r.vals {
            h.push_str(&format!("<td>{}</td>", ui::esc(&v.cell_text())));
        }
        h.push_str("</tr>\n");
    }
    h.push_str("</tbody>\n</table>\n");
    h
}
