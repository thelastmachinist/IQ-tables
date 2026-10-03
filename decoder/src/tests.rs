use crate::handle;
use iq_tables::json::{self, Json};
use iq_tables::pack::{self, Record, Schema};

const TABLE: &str = "3n7hcAoXkNhTc6CCGvVafkHfWmq3Rf72VXapMyzE6ZvP";
const OWNER: &str = "B8d355pft6DfrQNetCqXNumRk8WoEs21waqeuPP3HUJC";
const OTHER: &str = "8QWrZjNNFzngKWCLCrFkAy7ydnagrBSVYdJFyEvw9agh";

fn call(msg: Json) -> Json {
    json::parse(&handle(&msg.to_string())).unwrap()
}

fn read(cfg: &str) -> Json {
    call(json::obj(vec![("op", json::s("read")), ("config", json::parse(cfg).unwrap())]))
}

fn resume(results: Vec<Json>) -> Json {
    call(json::obj(vec![("op", json::s("resume")), ("results", Json::Arr(results))]))
}

fn answer(status: u32, body: &str) -> Json {
    json::obj(vec![("ok", Json::Bool(true)), ("status", json::n(status)), ("body", json::s(body))])
}

fn down() -> Json {
    json::obj(vec![("ok", Json::Bool(false)), ("status", json::n(0)), ("body", json::s("connection refused"))])
}

/// A gateway row holding one pack of (part, qty) records.
fn pack_row(sig: &str, signer: &str, time: u64, recs: &[(&str, i64)], compress: bool) -> Json {
    let schema = Schema { cols: vec!["part".into(), "qty".into()], id: 0 };
    let recs: Vec<Record> = recs.iter().map(|(p, q)| Record { vals: vec![json::s(p), json::n(q)], deleted: false }).collect();
    let payload = pack::encode_payload(&schema, &recs, compress);
    json::obj(vec![
        ("id", json::s(&pack::pack_id(&payload))),
        ("p", json::s(&payload)),
        ("__txSignature", json::s(sig)),
        ("__signer", json::s(signer)),
        ("__blockTime", json::n(time)),
    ])
}

fn page(rows: Vec<Json>, next: Option<&str>) -> String {
    json::obj(vec![("rows", Json::Arr(rows)), ("nextCursor", next.map(json::s).unwrap_or(Json::Null))]).to_string()
}

fn urls(step: &Json) -> Vec<String> {
    step.get("fetch").arr().iter().map(|r| r.get("url").str_or("")).collect()
}

#[test]
fn info_and_pure_decode() {
    let info = call(json::parse(r#"{"op":"info"}"#).unwrap());
    assert_eq!(info.get("abi").u64(), Some(1));
    assert_eq!(info.get("formats").idx(0).str(), Some("IQT1"));
    let row = pack_row("s1", OWNER, 1, &[("bolt", 5)], true);
    let d = call(json::obj(vec![("op", json::s("decode")), ("payload", row.get("p").clone())]));
    let (schema, recs, meta) = iq_tables::records::pack_from_json(d.get("ok")).unwrap();
    assert_eq!(schema.cols, vec!["part", "qty"]);
    assert_eq!(recs[0].vals, vec![json::s("bolt"), json::n(5)]);
    assert!(meta.is_none());
    assert_eq!(call(json::parse(r#"{"op":"decode","payload":"IQT9zzz"}"#).unwrap()).get("error").str(), Some("unsupported format"));
    assert!(call(json::parse(r#"{"op":"nope"}"#).unwrap()).get("error").str().is_some());
    assert!(handle("not json").contains("error"));
}

#[test]
fn config_is_checked() {
    assert!(read(r#"{"table":"nope"}"#).get("error").str().unwrap().contains("table"));
    assert!(read(&format!(r#"{{"table":"{}"}}"#, TABLE)).get("error").str().unwrap().contains("official"), "the official wallet is required");
    assert!(read(&format!(r#"{{"table":"{}","rows":"all"}}"#, TABLE)).get("error").str().unwrap().contains("official"), "even for everyone's rows");
    assert!(read(&format!(r#"{{"table":"{}","official":"{}","rows":"all","format":"xml"}}"#, TABLE, OWNER)).get("error").str().unwrap().contains("format"));
    assert!(read(&format!(r#"{{"table":"{}","official":"{}","rows":"all","gateway":"ftp://x"}}"#, TABLE, OWNER))
        .get("error")
        .str()
        .unwrap()
        .contains("gateway"));
    assert!(read(&format!(r#"{{"table":"{}","official":"{}","rows":"all","source":"solana"}}"#, TABLE, OWNER)).get("error").str().unwrap().contains("rpc"));
    assert!(resume(vec![]).get("error").str().unwrap().contains("read first"), "resume needs a read");
}

#[test]
fn reads_pages_through_the_gateway() {
    let cfg = format!(r#"{{"table":"{}","official":"{}","format":"csv","gateway":"https://gw.test/"}}"#, TABLE, OWNER);
    let s = read(&cfg);
    assert_eq!(urls(&s), vec![format!("https://gw.test/table/{}/meta", TABLE), format!("https://gw.test/table/{}/rows?limit=100", TABLE)]);
    // newest first: a visitor's row, then the owner's two packs over two pages
    let p1 = page(vec![pack_row("s3", OTHER, 30, &[("spam", 1)], true), pack_row("s2", OWNER, 20, &[("bolt", 7), ("=HYPERLINK(1)", 2)], false)], Some("s2"));
    let s = resume(vec![answer(200, r#"{"name":"parts","columns":["id","p"]}"#), answer(200, &p1)]);
    assert_eq!(urls(&s), vec![format!("https://gw.test/table/{}/rows?limit=100&before=s2", TABLE)]);
    let p2 = page(vec![pack_row("s1", OWNER, 10, &[("bolt", 5), ("nut", 3)], true)], None);
    let s = resume(vec![answer(200, &p2)]);
    let d = s.get("done");
    assert_eq!(d.get("cols").arr().len(), 2);
    assert_eq!(d.get("count").u64(), Some(3), "bolt (newest version), nut, and the formula-looking part");
    assert_eq!(d.get("name").str(), Some("parts"));
    assert_eq!(d.get("source").str(), Some("gateway"));
    assert_eq!(d.get("asOf").get("tx").str(), Some("s3"));
    let csv = d.get("data").str_or("");
    assert!(csv.starts_with("part,qty\n"), "{}", csv);
    assert!(csv.contains("bolt,7\n") && !csv.contains("bolt,5") && csv.contains("nut,3\n"), "{}", csv);
    assert!(!csv.contains("spam"), "someone else's rows stay out of official rows");
    assert!(csv.contains("'=HYPERLINK(1)"), "text a spreadsheet would run is defused: {}", csv);

    // everyone's rows, as JSON objects
    let s = read(&format!(r#"{{"table":"{}","official":"{}","rows":"all","format":"json"}}"#, TABLE, OWNER));
    assert!(urls(&s)[1].starts_with("https://gateway.iqlabs.dev/"), "IQ's gateway is the default");
    let s = resume(vec![
        answer(404, r#"{"error":"not found"}"#),
        answer(200, &page(vec![pack_row("s3", OTHER, 30, &[("spam", 1)], true), pack_row("s1", OWNER, 10, &[("nut", 3)], true)], None)),
    ]);
    let rows = json::parse(&s.get("done").get("data").str_or("")).unwrap();
    assert_eq!(rows.arr().len(), 2);
    assert!(rows.arr().iter().any(|r| r.get("part").str() == Some("spam")));
}

#[test]
fn html_output_is_escaped_and_fresh_reads_skip_the_cache() {
    let s = read(&format!(r#"{{"table":"{}","official":"{}","format":"html","fresh":true}}"#, TABLE, OWNER));
    assert!(urls(&s)[1].ends_with("&fresh=1"));
    let s = resume(vec![answer(200, "{}"), answer(200, &page(vec![pack_row("s1", OWNER, 10, &[("<script>x</script>", 1)], true)], None))]);
    let h = s.get("done").get("data").str_or("");
    assert!(h.contains("&lt;script&gt;") && !h.contains("<script>"), "{}", h);
    assert!(h.contains("<table class=\"iq-table\">"));
}

#[test]
fn rate_limits_wait_and_retry() {
    let s = read(&format!(r#"{{"table":"{}","official":"{}","rows":"all"}}"#, TABLE, OWNER));
    let first = urls(&s);
    let w = resume(vec![answer(200, "{}"), answer(429, "slow down")]);
    assert!(w.get("wait").u64().unwrap() >= 500);
    let s = resume(vec![]);
    assert_eq!(urls(&s), first, "the same requests go out again after the wait");
    for _ in 0..4 {
        assert!(resume(vec![answer(200, "{}"), answer(429, "")]).get("wait").u64().is_some());
        resume(vec![]);
    }
    let e = resume(vec![answer(200, "{}"), answer(429, "")]);
    assert!(e.get("error").str().unwrap().contains("429"));
}

#[test]
fn gateway_down_falls_back_to_solana_when_an_rpc_is_set() {
    let s = read(&format!(r#"{{"table":"{}","official":"{}","rows":"all"}}"#, TABLE, OWNER));
    let e = resume(vec![down(), down()]);
    assert!(e.get("error").str().unwrap().contains("gateway"), "no RPC: the error says what failed");

    let _ = s;
    read(&format!(r#"{{"table":"{}","official":"{}","rows":"all","rpc":"https://rpc.test"}}"#, TABLE, OWNER));
    let s = resume(vec![answer(200, "{}"), answer(503, "maintenance")]);
    let reqs = s.get("fetch").arr();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].get("url").str(), Some("https://rpc.test"));
    let body = json::parse(&reqs[0].get("body").str_or("")).unwrap();
    let methods: Vec<String> = body.arr().iter().map(|c| c.get("method").str_or("")).collect();
    assert_eq!(methods, vec!["getAccountInfo", "getSignaturesForAddress"], "one batch: the table account and its history");
    // this RPC refuses batches: the same calls go out one by one
    let s = resume(vec![answer(200, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"batch requests are not allowed"}}"#)]);
    let reqs = s.get("fetch").arr();
    assert_eq!(reqs.len(), 2);
    assert_eq!(json::parse(&reqs[1].get("body").str_or("")).unwrap().get("method").str(), Some("getSignaturesForAddress"));
    // an empty history: done, read from Solana, with a note saying why
    let s = resume(vec![
        answer(200, r#"{"jsonrpc":"2.0","id":0,"result":{"context":{"slot":1},"value":{"lamports":1,"owner":"x","data":["","base64"]}}}"#),
        answer(200, r#"{"jsonrpc":"2.0","id":1,"result":[]}"#),
    ]);
    let d = s.get("done");
    assert_eq!(d.get("source").str(), Some("solana"));
    assert_eq!(d.get("count").u64(), Some(0));
    assert!(d.get("notes").idx(0).str().unwrap().contains("gateway"));
}

#[test]
fn unknown_formats_go_to_the_decoder_that_reads_them() {
    let s = read(&format!(r#"{{"table":"{}","official":"{}","format":"rows"}}"#, TABLE, OWNER));
    assert!(s.get("fetch").arr().len() == 2);
    let future = json::obj(vec![
        ("id", json::s("f1")),
        ("p", json::s("IQT9whatever")),
        ("__txSignature", json::s("s9")),
        ("__signer", json::s(OWNER)),
        ("__blockTime", json::n(90)),
    ]);
    let s = resume(vec![answer(200, "{}"), answer(200, &page(vec![future, pack_row("s1", OWNER, 10, &[("nut", 3)], true)], None))]);
    let items = s.get("decode").arr();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].get("format").str(), Some("IQT9"));
    assert_eq!(items[0].get("payload").str(), Some("IQT9whatever"));
    // the older decoder's answer (the frozen `decode` reply)
    let other = json::parse(r#"{"ok":{"schema":{"cols":["part","qty"],"id":0},"records":[{"vals":["washer",40],"deleted":false},{"vals":["nut",null],"deleted":true}],"meta":null}}"#).unwrap();
    let s = resume(vec![other]);
    let d = s.get("done");
    let rows: Vec<String> = d.get("rows").arr().iter().map(|r| r.idx(0).str_or("")).collect();
    assert_eq!(rows, vec!["washer"], "the newer pack deleted nut and added washer");
    assert!(d.get("data").is_null(), "rows format has no text");

    // a decoder that can't read it: the pack is left out, and the notes say so
    let s = read(&format!(r#"{{"table":"{}","official":"{}"}}"#, TABLE, OWNER));
    let _ = s;
    let future = json::obj(vec![
        ("id", json::s("f1")),
        ("p", json::s("IQT9x")),
        ("__txSignature", json::s("s9")),
        ("__signer", json::s(OWNER)),
        ("__blockTime", json::n(90)),
    ]);
    resume(vec![answer(200, "{}"), answer(200, &page(vec![future], None))]);
    let s = resume(vec![json::parse(r#"{"error":"unsupported format"}"#).unwrap()]);
    assert!(s.get("done").get("notes").idx(0).str().unwrap().contains("couldn't be read"));
}

#[test]
fn max_rows_stops_early_and_says_so() {
    let s = read(&format!(r#"{{"table":"{}","official":"{}","rows":"all","maxRows":1}}"#, TABLE, OWNER));
    let _ = s;
    let s = resume(vec![answer(200, "{}"), answer(200, &page(vec![pack_row("s2", OWNER, 20, &[("a", 1)], true)], Some("s2")))]);
    let d = s.get("done");
    assert_eq!(d.get("truncated").bool(), Some(true));
    assert!(d.get("notes").arr().iter().any(|n| n.str_or("").contains("maxRows")));
}

#[test]
fn a_transaction_the_rpc_keeps_refusing_ends_in_an_error() {
    read(&format!(r#"{{"table":"{}","official":"{}","rows":"all","rpc":"https://rpc.test","source":"solana"}}"#, TABLE, OWNER));
    // account + one signature
    let s = resume(vec![answer(
        200,
        r#"[{"jsonrpc":"2.0","id":0,"result":{"context":{"slot":1},"value":null}},{"jsonrpc":"2.0","id":1,"result":[{"signature":"sig1","err":null}]}]"#,
    )]);
    assert!(s.get("error").str().unwrap().contains("No IQ table"), "a missing account is reported: {}", s);
    read(&format!(r#"{{"table":"{}","official":"{}","rows":"all","rpc":"https://rpc.test","source":"solana"}}"#, TABLE, OWNER));
    let s = resume(vec![answer(
        200,
        r#"[{"jsonrpc":"2.0","id":0,"result":{"context":{"slot":1},"value":{"lamports":1,"owner":"x","data":["","base64"]}}},{"jsonrpc":"2.0","id":1,"result":[{"signature":"sig1","err":null}]}]"#,
    )]);
    assert!(s.get("fetch").arr()[0].get("body").str_or("").contains("getTransaction"));
    let refuse = r#"[{"jsonrpc":"2.0","id":0,"error":{"code":-32011,"message":"Transaction history is not available from this node"}}]"#;
    let mut last = resume(vec![answer(200, refuse)]);
    let mut rounds = 0;
    while last.get("wait").u64().is_some() {
        rounds += 1;
        assert!(rounds < 10, "it must give up");
        let again = resume(vec![]);
        assert!(again.get("fetch").arr()[0].get("body").str_or("").contains("sig1"));
        last = resume(vec![answer(200, refuse)]);
    }
    let e = last.get("error").str().unwrap();
    assert!(e.contains("full history") && e.contains("not available"), "{}", e);
}

#[test]
fn hostile_rows_cant_crash_or_rewrite_the_output() {
    // IQT10 is not IQT1: it goes to another decoder; IQT1 with a multibyte mode doesn't crash
    let row = |sig: &str, signer: &str, p: &str| {
        json::obj(vec![("id", json::s(sig)), ("p", json::s(p)), ("__txSignature", json::s(sig)), ("__signer", json::s(signer)), ("__blockTime", json::n(1))])
    };
    read(&format!(r#"{{"table":"{}","official":"{}","rows":"all"}}"#, TABLE, OWNER));
    let s =
        resume(vec![answer(200, "{}"), answer(200, &page(vec![row("a", OWNER, "IQT10zzz"), row("b", OTHER, "IQT1é"), row("c", OTHER, "IQT1z!!!!")], None))]);
    let items = s.get("decode").arr();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].get("format").str(), Some("IQT10"));
    let d = resume(vec![json::parse(r#"{"error":"nope"}"#).unwrap()]);
    assert!(matches!(d.get("done"), Json::Obj(_)), "{}", d);

    // a "number" that isn't one can't break out of the JSON reply
    let schema = Schema { cols: vec!["part".into(), "qty".into()], id: 0 };
    let recs = vec![Record { vals: vec![json::s("x"), Json::Num("1]],\"table\":\"FAKE\",\"rows\":[[\"y\"".into())], deleted: false }];
    let payload = pack::encode_payload(&schema, &recs, true);
    read(&format!(r#"{{"table":"{}","official":"{}","rows":"all","format":"csv"}}"#, TABLE, OWNER));
    let r = json::obj(vec![
        ("id", json::s("p1")),
        ("p", json::s(&payload)),
        ("__txSignature", json::s("s1")),
        ("__signer", json::s(OTHER)),
        ("__blockTime", json::n(1)),
    ]);
    let owners = pack_row("s0", OWNER, 0, &[("bolt", 5)], true);
    let out = handle(
        &json::obj(vec![("op", json::s("resume")), ("results", Json::Arr(vec![answer(200, "{}"), answer(200, &page(vec![r, owners], None))]))]).to_string(),
    );
    let d = json::parse(&out).unwrap();
    assert_eq!(d.get("done").get("table").str(), Some(TABLE), "the reply keeps its own fields: {}", out);
    let fake = d.get("done").get("rows").arr().iter().find(|r| r.idx(0).str() == Some("x")).map(|r| r.idx(1).clone());
    assert_eq!(fake.and_then(|v| v.str().map(|s| s.starts_with("1]]"))), Some(true), "the fake number is text now: {}", out);

    // a plain table stays plain when someone else writes a pack into it, and
    // columns come only from the rows shown (a formula-like header is defused)
    read(&format!(r#"{{"table":"{}","official":"{}","format":"csv"}}"#, TABLE, OWNER));
    let plain = json::obj(vec![("sku", json::s("A1")), ("__txSignature", json::s("s1")), ("__signer", json::s(OWNER)), ("__blockTime", json::n(1))]);
    let junk =
        json::obj(vec![("=cmd|' /C calc'!A0", json::s("x")), ("__txSignature", json::s("s2")), ("__signer", json::s(OTHER)), ("__blockTime", json::n(2))]);
    let s = resume(vec![answer(200, "{}"), answer(200, &page(vec![junk, pack_row("s3", OTHER, 3, &[("spam", 1)], true), plain], None))]);
    let d = s.get("done");
    assert_eq!(d.get("data").str(), Some("sku\nA1\n"), "{}", d);
    let rows = vec![json::obj(vec![("=HYPERLINK(1)", json::s("x")), ("__signer", json::s(OWNER))])];
    let decoded = vec![None];
    let (cols, vr) =
        iq_tables::records::Source { rows: &rows, decoded: &decoded, meta: None, creator: Some(OWNER) }.table(iq_tables::records::Who::Official, true);
    assert!(iq_tables::records::csv(&cols, &vr).starts_with("'=HYPERLINK(1)\n"));
}

#[test]
fn official_reads_ignore_other_writers_rows() {
    // a broken (or deliberately huge) pack from someone else isn't even decoded
    read(&format!(r#"{{"table":"{}","official":"{}"}}"#, TABLE, OWNER));
    let junk = json::obj(vec![
        ("id", json::s("j")),
        ("p", json::s("IQT1z!!!!!!!!")),
        ("__txSignature", json::s("s9")),
        ("__signer", json::s(OTHER)),
        ("__blockTime", json::n(9)),
    ]);
    let future = json::obj(vec![
        ("id", json::s("f")),
        ("p", json::s("IQT9x")),
        ("__txSignature", json::s("s8")),
        ("__signer", json::s(OTHER)),
        ("__blockTime", json::n(8)),
    ]);
    let s = resume(vec![answer(200, "{}"), answer(200, &page(vec![junk, future, pack_row("s1", OWNER, 1, &[("nut", 3)], true)], None))]);
    let d = s.get("done");
    assert!(matches!(d, Json::Obj(_)), "no decode step for someone else's future-format row: {}", s);
    assert_eq!(d.get("count").u64(), Some(1));
    assert!(d.get("notes").arr().is_empty(), "nothing unreadable to report: {}", d);
}

#[test]
fn other_writers_get_a_small_share() {
    // someone else's big pack is left out (and said so); the owner's rows are read
    let big: Vec<(String, i64)> = (0..6000usize).map(|i| (format!("row-{:05}-{}", i, "x".repeat(i % 17)), i as i64)).collect();
    let big_ref: Vec<(&str, i64)> = big.iter().map(|(a, b)| (a.as_str(), *b)).collect();
    let theirs = pack_row("s2", OTHER, 2, &big_ref, true);
    assert!(iq_tables::pack::unpacked_size(theirs.get("p").str().unwrap()).unwrap() > iq_tables::records::OTHERS_PACK_MAX);
    let mine = pack_row("s1", OWNER, 1, &big_ref, true);
    read(&format!(r#"{{"table":"{}","official":"{}","rows":"all"}}"#, TABLE, OWNER));
    let s = resume(vec![answer(200, "{}"), answer(200, &page(vec![theirs, mine], None))]);
    let d = s.get("done");
    assert_eq!(d.get("count").u64(), Some(6000), "the owner's big pack is read in full: {}", d.get("notes"));
    assert!(d.get("notes").idx(0).str().unwrap().contains("couldn't be read"));
}

#[test]
fn encode_and_unpack_round_trip() {
    let csv = "part_no,name,qty,price\nFST-1,\"Hex bolt, M8\",1200,0.18\nFST-2,Flat washer,,0.04\nFST-3,=cmd,5,1\n";
    let e = call(json::obj(vec![("op", json::s("encode")), ("csv", json::s(csv)), ("id", json::s("part_no"))]));
    let ok = e.get("ok");
    assert_eq!(ok.get("records").u64(), Some(3), "{}", e);
    assert_eq!(ok.get("idColumn").str(), Some("part_no"));
    let row = ok.get("row");
    let p = row.get("p").str_or("");
    assert!(p.starts_with("IQT1"));
    assert_eq!(row.get("id").str().map(String::from), Some(iq_tables::pack::pack_id(&p)), "the row id is the pack's content hash");
    // the decoder (and so IQ Tables) reads it back, typed
    let u = call(json::obj(vec![("op", json::s("unpack")), ("payload", json::s(&row.to_string())), ("format", json::s("json"))]));
    let back = json::parse(&u.get("ok").get("text").str_or("")).unwrap();
    assert_eq!(back.arr().len(), 3);
    assert_eq!(back.arr()[0].get("name").str(), Some("Hex bolt, M8"));
    assert_eq!(back.arr()[0].get("qty"), &json::n(1200), "numbers stay numbers");
    assert!(back.arr()[1].get("qty").is_null(), "empty stays empty");
    let c = call(json::obj(vec![("op", json::s("unpack")), ("payload", json::s(&p)), ("format", json::s("csv"))]));
    let text = c.get("ok").get("text").str_or("");
    assert!(text.starts_with("part_no,name,qty,price\n") && text.contains("\"Hex bolt, M8\"") && text.contains("'=cmd"), "{}", text);
    // dense picks the shorter form: tiny data stays plain JSON, bulk data compresses
    let tiny = call(json::parse(r#"{"op":"encode","cols":["a"],"rows":[["x"]]}"#).unwrap());
    let both = |mode: &str| {
        call(json::parse(&format!(r#"{{"op":"encode","cols":["a"],"rows":[["x"]],"mode":"{}"}}"#, mode)).unwrap()).get("ok").get("bytes").u64().unwrap()
    };
    assert_eq!(tiny.get("ok").get("bytes").u64().unwrap(), both("plain").min(both("compressed")));
    let many: String = (0..2000).map(|i| format!("ROW-{:05},Socket head cap screw M6x1.0,{},316 stainless\n", i, i % 50)).collect();
    let big = call(json::obj(vec![("op", json::s("encode")), ("csv", json::s(&format!("sku,desc,qty,mat\n{}", many)))]));
    let ok = big.get("ok");
    assert_eq!(ok.get("compressed").bool(), Some(true));
    assert!(ok.get("bytes").u64().unwrap() * 8 < many.len() as u64, "at least 8x smaller than the CSV: {} vs {}", ok.get("bytes"), many.len());
    // objects in, with the id chosen by name
    let o = call(json::parse(r#"{"op":"encode","objects":[{"k":"a","v":1},{"k":"b","w":true}],"id":"k"}"#).unwrap());
    assert_eq!(o.get("ok").get("columns").u64(), Some(3));
    // errors say what's wrong
    let bad = |m: &str| call(json::parse(m).unwrap()).get("error").str().map(String::from).unwrap_or_default();
    assert!(bad(r#"{"op":"encode","cols":["a"],"rows":[[null]]}"#).contains("no value in the id column"));
    assert!(bad(r#"{"op":"encode","cols":["a"],"rows":[]}"#).contains("no rows"));
    assert!(bad(r#"{"op":"encode","cols":["a"],"rows":[["x"]],"id":"zz"}"#).contains("no column"));
    assert!(bad(r#"{"op":"unpack","payload":"hello"}"#).contains("isn't an IQ Tables pack"));
}
