//! Offline tools: packing data into IQ Tables' storage format (`encode`) and
//! unpacking one pack into readable text (`unpack`). Pure, like `decode`.

use iq_tables::json::{self, Json};
use iq_tables::pack::{self, Record, Schema};
use iq_tables::records::{self, VRow};
use iq_tables::ui;

fn err(msg: &str) -> Json {
    json::obj(vec![("error", json::s(msg))])
}

/// Columns and rows from `{"csv": text}`, `{"objects": [{…}]}` or
/// `{"cols": […], "rows": [[…]]}`.
fn table_of(m: &Json) -> Result<(Vec<String>, Vec<Vec<Json>>), String> {
    if let Some(text) = m.get("csv").str() {
        let typed = m.get("typed").bool().unwrap_or(true);
        let mut lines = ui::parse_csv(text).into_iter();
        let cols: Vec<String> = lines.next().ok_or("the CSV is empty")?.into_iter().map(|c| c.trim().to_string()).collect();
        let rows = lines.map(|r| r.iter().map(|v| if typed { ui::typed(v) } else { json::s(v) }).collect()).collect();
        return Ok((cols, rows));
    }
    if let Json::Arr(objs) = m.get("objects") {
        let mut cols: Vec<String> = vec![];
        for o in objs {
            let Json::Obj(pairs) = o else { return Err("objects must be an array of objects".into()) };
            for (k, _) in pairs {
                if !cols.contains(k) {
                    cols.push(k.clone());
                }
            }
        }
        let rows = objs.iter().map(|o| cols.iter().map(|c| o.get(c).clone()).collect()).collect();
        return Ok((cols, rows));
    }
    let cols: Vec<String> = m.get("cols").arr().iter().map(|c| c.str().map(String::from).ok_or("cols must be names")).collect::<Result<_, _>>()?;
    let rows = m.get("rows").arr().iter().map(|r| r.arr().to_vec()).collect();
    Ok((cols, rows))
}

/// Pack rows into one IQ Tables row (`{"id","p"}`), as densely as the format
/// allows: compressed (`IQT1z`) or plain JSON (`IQT1j`), whichever is shorter,
/// unless `"mode"` asks for one.
pub fn encode(m: &Json) -> Json {
    let (cols, rows) = match table_of(m) {
        Ok(t) => t,
        Err(e) => return err(&e),
    };
    if cols.is_empty() || cols.len() > 4096 {
        return err("there must be 1 to 4096 columns");
    }
    if cols.iter().any(|c| c.is_empty()) {
        return err("every column needs a name");
    }
    if rows.is_empty() {
        return err("there are no rows");
    }
    let id = match m.get("id") {
        Json::Str(name) => match cols.iter().position(|c| c == name) {
            Some(i) => i,
            None => return err(&format!("there's no column \"{}\" for the id", name)),
        },
        Json::Num(_) => match m.get("id").u64().map(|i| i as usize).filter(|&i| i < cols.len()) {
            Some(i) => i,
            None => return err("the id column number is out of range"),
        },
        _ => 0,
    };
    let schema = Schema { cols: cols.clone(), id };
    let recs: Vec<Record> = rows
        .into_iter()
        .map(|mut vals| {
            vals.resize(cols.len(), Json::Null);
            Record { vals, deleted: false }
        })
        .collect();
    if let Some(i) = recs.iter().position(|r| r.key(&schema).is_empty()) {
        return err(&format!("row {} has no value in the id column \"{}\"", i + 1, cols[id]));
    }
    let raw = pack::layout(&schema, &recs).len();
    if raw as u64 > iq_tables::codec::MAX_RAW {
        return err("that's more than one pack can hold (16 MB unpacked): split the rows");
    }
    let mode = m.get("mode").str().unwrap_or("dense");
    let payload = match mode {
        "compressed" => pack::encode_payload(&schema, &recs, true),
        "plain" => pack::encode_payload(&schema, &recs, false),
        _ => {
            let z = pack::encode_payload(&schema, &recs, true);
            let j = pack::encode_payload(&schema, &recs, false);
            if j.len() < z.len() {
                j
            } else {
                z
            }
        }
    };
    let dups = {
        let mut keys: Vec<String> = recs.iter().map(|r| r.key(&schema)).collect();
        let n = keys.len();
        keys.sort();
        keys.dedup();
        n - keys.len()
    };
    json::obj(vec![(
        "ok",
        json::obj(vec![
            ("row", json::parse(&pack::row_json(&payload)).unwrap_or(Json::Null)),
            ("records", json::n(recs.len())),
            ("columns", json::n(cols.len())),
            ("idColumn", json::s(&cols[id])),
            ("bytes", json::n(payload.len())),
            ("raw", json::n(raw)),
            ("compressed", Json::Bool(payload.starts_with("IQT1z"))),
            ("duplicateIds", json::n(dups)),
        ]),
    )])
}

/// One pack as text: its records as CSV or JSON (a structure record as its
/// JSON). Accepts the payload or the whole row (`{"id","p"}`).
pub fn unpack(m: &Json) -> Json {
    let payload = match m.get("payload") {
        Json::Str(s) => match json::parse(s) {
            Ok(row @ Json::Obj(_)) => row.get("p").str_or(""),
            _ => s.trim().to_string(),
        },
        row @ Json::Obj(_) => row.get("p").str_or(""),
        _ => String::new(),
    };
    let d = crate::decode(&payload);
    let Some((schema, recs, meta)) = records::pack_from_json(d.get("ok")) else {
        return match d.get("error").str() {
            Some("unsupported format") | None => err("that isn't an IQ Tables pack this decoder reads (it should start with IQT1)"),
            Some(e) => err(e),
        };
    };
    if let Some(meta) = meta {
        return json::obj(vec![("ok", json::obj(vec![("structure", Json::Bool(true)), ("text", json::s(&meta.to_string())), ("records", json::n(0))]))]);
    }
    let deleted: Vec<Json> = recs.iter().filter(|r| r.deleted).map(|r| json::s(&r.key(&schema))).collect();
    let rows: Vec<VRow> = recs
        .iter()
        .filter(|r| !r.deleted)
        .map(|r| VRow {
            key: r.key(&schema),
            vals: r.vals.clone(),
            signer: String::new(),
            tx: String::new(),
            time: None,
            official: None,
            versions: 1,
            packed: true,
        })
        .collect();
    let text = match m.get("format").str().unwrap_or("json") {
        "csv" => records::csv(&schema.cols, &rows),
        _ => records::json_rows(&schema.cols, &rows).to_string(),
    };
    json::obj(vec![(
        "ok",
        json::obj(vec![("structure", Json::Bool(false)), ("text", json::s(&text)), ("records", json::n(rows.len())), ("deleted", Json::Arr(deleted))]),
    )])
}
