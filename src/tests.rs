//! Byte-for-byte checks against reference output produced by the official
//! IQ Labs SDK (`tools/fixtures.cjs` -> `tools/fixtures.json`).

use crate::crypto::{base58, ed25519, hex, keccak::keccak256, sha2, unhex};
use crate::iq;
use crate::json::{self, Json};
use crate::solana::{self, b58, pk, Instruction};

fn fixtures() -> Json {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tools/fixtures.json");
    json::parse(&std::fs::read_to_string(path).expect("run tools/fixtures.cjs first")).unwrap()
}

fn h(v: &Json) -> Vec<u8> {
    unhex(v.str().unwrap()).unwrap()
}

#[test]
fn hashes_match() {
    let f = fixtures();
    for c in f.get("hashes").arr() {
        let input = h(c.get("input"));
        assert_eq!(hex(&sha2::sha256(&input)), c.get("sha256").str().unwrap(), "sha256 len {}", input.len());
        assert_eq!(hex(&sha2::sha512_parts(&[&input])), c.get("sha512").str().unwrap(), "sha512 len {}", input.len());
        assert_eq!(hex(&keccak256(&input)), c.get("keccak256").str().unwrap(), "keccak len {}", input.len());
    }
}

#[test]
fn base58_matches() {
    for c in fixtures().get("base58").arr() {
        let bytes = h(c.get("hex"));
        let enc = c.get("b58").str().unwrap();
        assert_eq!(base58::encode(&bytes), enc);
        assert_eq!(base58::decode(enc).unwrap(), bytes);
    }
}

#[test]
fn ed25519_matches_noble() {
    for c in fixtures().get("ed25519").arr() {
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&h(c.get("seed")));
        assert_eq!(hex(&ed25519::public_key(&seed)), c.get("pub").str().unwrap());
        let msg = h(c.get("msg"));
        assert_eq!(hex(&ed25519::sign(&seed, &msg)), c.get("sig").str().unwrap(), "msg len {}", msg.len());
    }
}

#[test]
fn pdas_match_sdk() {
    let f = fixtures();
    for c in f.get("pdas").arr() {
        let db_id = h(c.get("dbId"));
        let seed = iq::seed_bytes(c.get("tableName").str().unwrap());
        assert_eq!(hex(&seed), c.get("tableSeed").str().unwrap());
        let root = iq::db_root_pda(&db_id);
        assert_eq!(b58(&root), c.get("dbRoot").str().unwrap());
        assert_eq!(b58(&iq::table_pda(&root, &seed)), c.get("table").str().unwrap());
        assert_eq!(b58(&iq::instruction_table_pda(&root, &seed)), c.get("instructionTable").str().unwrap());
        let user = pk(c.get("user").str().unwrap());
        assert_eq!(b58(&iq::user_state_pda(&user)), c.get("userState").str().unwrap());
        assert_eq!(b58(&iq::code_account_pda(&user)), c.get("codeAccount").str().unwrap());
        assert_eq!(b58(&iq::user_inventory_pda(&user)), c.get("userInventory").str().unwrap());
    }
    let k = f.get("knownPdas");
    let root = iq::db_root_pda(b"iq-locker");
    assert_eq!(b58(&root), "HrWK65t1rXTebdeKPV8Pa2WMuv7qah5mB4hvzv6TA7YM");
    assert_eq!(b58(&root), k.get("iqLockerRoot").str().unwrap());
    assert_eq!(
        b58(&iq::table_pda(&root, &iq::seed_bytes("notes"))),
        "3n7hcAoXkNhTc6CCGvVafkHfWmq3Rf72VXapMyzE6ZvP"
    );
    let a = f.get("ata");
    let owner = pk(a.get("owner").str().unwrap());
    let mint = pk(iq::IQ_MINT_STR);
    assert_eq!(b58(&iq::ata(&owner, &mint, &pk(iq::TOKEN_PROGRAM_STR))), a.get("legacy").str().unwrap());
    assert_eq!(b58(&iq::ata(&owner, &mint, &pk(iq::TOKEN_2022_STR))), a.get("t22").str().unwrap());
}

fn same_ix(ours: &Instruction, theirs: &Json, what: &str) {
    assert_eq!(b58(&ours.program_id), theirs.get("program").str().unwrap(), "{} program", what);
    assert_eq!(hex(&ours.data), theirs.get("data").str().unwrap(), "{} data", what);
    let keys = theirs.get("keys").arr();
    assert_eq!(ours.accounts.len(), keys.len(), "{} key count", what);
    for (i, (a, k)) in ours.accounts.iter().zip(keys).enumerate() {
        assert_eq!(b58(&a.pubkey), k.idx(0).str().unwrap(), "{} key {} pubkey", what, i);
        assert_eq!(a.is_signer, k.idx(1).bool().unwrap(), "{} key {} signer", what, i);
        assert_eq!(a.is_writable, k.idx(2).bool().unwrap(), "{} key {} writable", what, i);
    }
}

#[test]
fn instructions_match_sdk() {
    let f = fixtures();
    let x = f.get("ix");
    let signer = pk(x.get("signer").str().unwrap());
    let creator = pk(x.get("creator").str().unwrap());
    let db_id = h(x.get("dbId"));
    let seed = h(x.get("tableSeed"));
    assert_eq!(seed, iq::seed_bytes("fasteners"));
    same_ix(&iq::initialize_db_root(&signer, &db_id), x.get("initializeDbRoot"), "initialize_db_root");
    same_ix(
        &iq::manage_table_creators(&signer, &db_id, &[signer], &[signer, creator]),
        x.get("manageTableCreators"),
        "manage_table_creators",
    );
    let cols = vec!["id".to_string(), "p".to_string()];
    let ext = vec!["iqt:1".to_string()];
    let spec = |writers| iq::TableSpec {
        db_id: &db_id,
        table_seed: &seed,
        hint: "fasteners",
        name: "Fasteners & hardware",
        columns: &cols,
        id_col: "id",
        ext_keys: &ext,
        writers,
    };
    same_ix(&iq::create_table(&signer, &creator, &spec(None)), x.get("createTableOpen"), "create_table open");
    let w = [signer];
    same_ix(&iq::create_table(&signer, &creator, &spec(Some(&w))), x.get("createTableLocked"), "create_table locked");
    same_ix(&iq::user_initialize(&signer), x.get("userInitialize"), "user_initialize");
    let row = x.get("rowJson").str().unwrap();
    let md = iq::inline_metadata(7, row);
    assert_eq!(md, x.get("metadata").str().unwrap());
    same_ix(&iq::db_code_in_inline(&signer, &db_id, &seed, &md, None), x.get("dbCodeIn"), "db_code_in");
    let ata = pk(x.get("iqAta").str().unwrap());
    same_ix(&iq::db_code_in_inline(&signer, &db_id, &seed, &md, Some(ata)), x.get("dbCodeInAta"), "db_code_in ata");
    same_ix(&iq::realloc_account(&signer, &iq::db_root_pda(&db_id), 4321), x.get("realloc"), "realloc");
    same_ix(&iq::user_inventory_code_in_inline(&signer, &md, None), x.get("userInventoryCodeIn"), "user_inventory_code_in");
    let none: Vec<String> = vec![];
    let uspec = |writers| iq::TableSpec { db_id: &db_id, table_seed: &seed, hint: "", name: "Parts we stock", columns: &cols, id_col: "id", ext_keys: &none, writers };
    let w2 = [signer, creator];
    same_ix(&iq::update_table(&signer, &uspec(Some(&w2))), x.get("updateTableLocked"), "update_table locked");
    same_ix(&iq::update_table(&signer, &uspec(Some(&[]))), x.get("updateTableOpen"), "update_table open");
    same_ix(&iq::update_db_root_table_list(&signer, &db_id, &[b"fasteners".to_vec(), b"suppliers".to_vec()]), x.get("updateTableList"), "update_db_root_table_list");
    // data bigger than one transaction
    let c = f.get("chunks");
    assert_eq!(crate::solana::b58(&iq::session_pda(&signer, 3)), c.get("session").str().unwrap(), "session pda");
    let text = c.get("text").str().unwrap();
    for (size, key) in [(iq::CHUNK_SIZE_V1, "v1"), (iq::CHUNK_SIZE_LEGACY, "legacy")] {
        let want: Vec<String> = c.get(key).arr().iter().map(|v| v.str_or("")).collect();
        assert_eq!(iq::to_chunks(text, size), want, "chunking at {}", size);
    }
    same_ix(&iq::send_code(&signer, "chunk é", "Genesis"), x.get("sendCode"), "send_code");
    same_ix(&iq::create_session(&signer, 3), x.get("createSession"), "create_session");
    same_ix(&iq::post_chunk(&signer, 3, 5, "part five"), x.get("postChunk"), "post_chunk");
    let md = c.get("metadata").str().unwrap();
    assert_eq!(iq::chunked_metadata("application/octet-stream", "3.bin", 12), md);
    let tail = iq::ChunkPath::Linked(c.get("tail").str().unwrap().to_string());
    let sess = iq::ChunkPath::Session { seq: 3, total: 12 };
    same_ix(&iq::db_code_in(&signer, &db_id, &seed, &tail, md, None), x.get("dbCodeInLinked"), "db_code_in linked list");
    same_ix(&iq::db_code_in(&signer, &db_id, &seed, &sess, md, None), x.get("dbCodeInSession"), "db_code_in session");
    same_ix(&iq::user_inventory_code_in(&signer, &tail, md, None), x.get("inventoryLinked"), "user_inventory_code_in linked list");
    same_ix(&iq::user_inventory_code_in(&signer, &sess, md, None), x.get("inventorySession"), "user_inventory_code_in session");
    let inline = iq::ChunkPath::Inline;
    let row_md = iq::inline_metadata(7, row);
    same_ix(&iq::db_code_in(&signer, &db_id, &seed, &inline, &row_md, Some(ata)), x.get("dbCodeInAta"), "db_code_in (general) inline");
}

#[test]
fn json_escaping_matches_js() {
    for c in fixtures().get("jsonStrings").arr() {
        let s = c.get("s").str().unwrap();
        assert_eq!(json::quote(s), c.get("json").str().unwrap(), "escaping {:?}", s);
        assert_eq!(json::parse(c.get("json").str().unwrap()).unwrap(), Json::Str(s.to_string()));
    }
}

#[test]
fn v1_transactions_match_sdk() {
    let f = fixtures();
    let v = f.get("v1");
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&h(v.get("seed")));
    let kp = solana::Keypair::from_seed(seed);
    let bh = base58::decode32(v.get("blockhash").str().unwrap()).unwrap();
    let db_id = h(f.get("ix").get("dbId"));
    let msg = solana::compile(&kp.pubkey, &[iq::initialize_db_root(&kp.pubkey, &db_id)], bh);
    let (raw, sig) = solana::v1_signed(&msg, &seed);
    assert_eq!(hex(&raw), v.get("raw").str().unwrap());
    assert_eq!(base58::encode(&sig), v.get("signature").str().unwrap());

    let seedt = iq::seed_bytes("fasteners");
    let md = f.get("ix").get("metadata").str().unwrap();
    let msg = solana::compile(&kp.pubkey, &[iq::db_code_in_inline(&kp.pubkey, &db_id, &seedt, md, None)], bh);
    let (raw, _) = solana::v1_signed(&msg, &seed);
    assert_eq!(hex(&raw), f.get("v1CodeIn").get("raw").str().unwrap());
}

#[test]
fn json_roundtrip() {
    let src = r#"{"a":[1,2.5e3,-7],"b":{"c":"xé🙂","d":null,"e":true},"big":12345678901234567890}"#;
    let v = json::parse(src).unwrap();
    assert_eq!(v.get("b").get("c").str().unwrap(), "xé🙂");
    assert_eq!(v.get("big"), &Json::Num("12345678901234567890".into()));
    let again = json::parse(&v.to_string()).unwrap();
    assert_eq!(v, again);
}

// ------------------------------------------------------------ codec + packs

use crate::{codec, pack};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, v: &[&'a str]) -> &'a str {
        v[(self.next() % v.len() as u64) as usize]
    }
}

pub fn sample_parts(n: usize, seed: u64) -> (pack::Schema, Vec<pack::Record>) {
    let mut r = Rng(seed);
    let cols = ["part_no", "name", "material", "finish", "thread", "length_mm", "qty", "unit_price", "supplier", "origin", "updated"];
    let kinds = ["Hex bolt", "Socket head cap screw", "Flat washer", "Nylon lock nut", "Carriage bolt", "Set screw", "Dowel pin", "Shoulder bolt"];
    let mats = ["18-8 stainless", "316 stainless", "Grade 5 steel", "Grade 8 steel", "Brass", "Aluminum 6061", "Titanium Gr5"];
    let fin = ["Plain", "Zinc plated", "Black oxide", "Hot-dip galvanized", "Passivated"];
    let thr = ["M6x1.0", "M8x1.25", "M10x1.5", "1/4-20 UNC", "5/16-18 UNC", "3/8-16 UNC", "#10-32 UNF"];
    let sup = ["Lone Star Fastener Co.", "Gulf Coast Supply", "Permian Industrial", "Hill Country Hardware", "Brazos Bolt & Nut"];
    let recs = (0..n)
        .map(|i| {
            let k = r.pick(&kinds);
            let t = r.pick(&thr);
            let len = [10, 12, 16, 20, 25, 30, 35, 40, 50, 60, 75, 100][(r.next() % 12) as usize];
            pack::Record {
                vals: vec![
                    json::s(&format!("FST-{:06}", 1000 + i * 3)),
                    json::s(&format!("{} {} x {}mm", k, t, len)),
                    json::s(r.pick(&mats)),
                    json::s(r.pick(&fin)),
                    json::s(t),
                    json::n(len),
                    json::n(r.next() % 5000),
                    json::n(format!("{}.{:02}", r.next() % 40, r.next() % 100)),
                    json::s(r.pick(&sup)),
                    json::s(if r.next() % 5 == 0 { "MX" } else { "US" }),
                    json::n(1_790_000_000_000u64 + r.next() % 90_000_000_000),
                ],
                deleted: false,
            }
        })
        .collect();
    (pack::Schema { cols: cols.iter().map(|s| s.to_string()).collect(), id: 0 }, recs)
}

#[test]
fn codec_roundtrips() {
    let mut r = Rng(99);
    for len in [0usize, 1, 2, 3, 7, 13, 100, 1000, 20000] {
        // mixed: random bytes and repetitive text
        let mut d: Vec<u8> = (0..len).map(|_| (r.next() & 0xff) as u8).collect();
        if len > 50 {
            for i in 0..len / 2 {
                d[i] = b"the quick brown fox "[i % 20];
            }
        }
        let c = codec::compress(&d);
        assert_eq!(codec::decompress(&c).unwrap(), d, "len {}", len);
        let t = codec::to_text(&c);
        assert!(!t.contains('"') && !t.contains('\\'));
        assert!(t.bytes().all(|b| (0x21..=0x7e).contains(&b)));
        assert_eq!(codec::from_text(&t).unwrap(), c, "text len {}", len);
    }
    for n in 0..300 {
        let d: Vec<u8> = (0..n).map(|_| (r.next() & 0xff) as u8).collect();
        assert_eq!(codec::from_text(&codec::to_text(&d)).unwrap(), d, "b92 n={}", n);
    }
}

#[test]
fn packs_roundtrip_and_fit() {
    let (schema, mut recs) = sample_parts(600, 7);
    recs[5].deleted = true;
    recs[9].vals[3] = Json::Arr(vec![json::n(1), json::s("x\"y")]);
    recs[11].vals[2] = Json::Null;
    for compress in [true, false] {
        let plan = pack::plan(&schema, &recs, iq::INLINE_CAP_V1, compress).unwrap();
        let mut total = 0;
        for p in &plan {
            assert!(p.size <= iq::INLINE_CAP_V1);
            let (s2, r2) = pack::decode_payload(&p.payload).unwrap();
            assert_eq!(s2, schema);
            let want: Vec<pack::Record> = recs[p.first..p.first + p.count].iter().map(|r| r.normalized(&schema)).collect();
            assert_eq!(r2, want);
            total += p.count;
            // the whole v1 transaction must fit 4096 bytes
            let kp = solana::Keypair::from_seed([7; 32]);
            let md = iq::inline_metadata(9_999_999_999, &pack::row_json(&p.payload));
            let ix = iq::db_code_in_inline(&kp.pubkey, b"a-32-byte-long-database-name-xyz", &iq::seed_bytes("t"), &md, Some([3; 32]));
            let (raw, _) = solana::v1_signed(&solana::compile(&kp.pubkey, &[ix], [1; 32]), &kp.seed);
            assert!(raw.len() <= solana::V1_MAX_TX_BYTES, "tx {} bytes", raw.len());
        }
        assert_eq!(total, recs.len());
        let raw_json: usize = recs.iter().map(|r| Json::Arr(r.vals.clone()).to_string().len()).sum();
        eprintln!(
            "compress={} packs={} records/pack={:.1} raw_json={}B -> on-chain {}B",
            compress,
            plan.len(),
            recs.len() as f64 / plan.len() as f64,
            raw_json,
            plan.iter().map(|p| p.size).sum::<usize>()
        );
    }
}

#[test]
fn merge_latest_wins_and_tombstones() {
    let schema = pack::Schema { cols: vec!["id".into(), "v".into()], id: 0 };
    let rec = |id: &str, v: i32, del: bool| pack::Record { vals: vec![json::s(id), json::n(v)], deleted: del };
    let sp = |tx: &str, recs| pack::SourcePack { id: tx.into(), tx: tx.into(), signer: "S".into(), time: None, schema: schema.clone(), recs, meta: None };
    let merged = pack::merge(&[
        sp("t1", vec![rec("a", 1, false), rec("b", 2, false), rec("c", 3, false)]),
        sp("t2", vec![rec("a", 10, false), rec("b", 0, true)]),
    ]);
    let got: Vec<(String, String, usize)> =
        merged.iter().map(|m| (m.key.clone(), m.vals[1].1.to_string(), m.versions)).collect();
    assert_eq!(got, vec![("a".into(), "10".into(), 2), ("c".into(), "3".into(), 1)]);
}

#[test]
fn compression_ratio_report() {
    let (schema, recs) = sample_parts(120, 11);
    let raw = pack::layout(&schema, &recs);
    let c = codec::compress(&raw);
    eprintln!("RATIO layout={} compressed={} ratio={:.2}", raw.len(), c.len(), raw.len() as f64 / c.len() as f64);
    let _ = std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "/target/sample_layout.bin"), &raw);
}

#[test]
fn qr_matrix_for_scanner_check() {
    // written out so tools/qrcheck.cjs can decode it with an independent scanner
    let text = "solana:B3oPWRqx2RPJfUAeqX4e2M4H9WiVa2211PtHpjmoCvYq?label=e2e-parts%20on%20IQ";
    let m = crate::qr::encode(text).unwrap();
    let rows: Vec<String> = m.iter().map(|r| r.iter().map(|&d| if d { '1' } else { '0' }).collect()).collect();
    let _ = std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/target"));
    std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "/target/qr.txt"), format!("{}\n{}", text, rows.join("\n"))).unwrap();
    assert!(m.len() >= 21 && (m.len() - 17) % 4 == 0);
}

// ------------------------------------------------------ password encryption

use crate::crypto::aead;

#[test]
fn pbkdf2_matches_node() {
    for c in fixtures().get("pbkdf2").arr() {
        let key = aead::pbkdf2_sha256(c.get("pw").str().unwrap().as_bytes(), &h(c.get("salt")), c.get("it").u64().unwrap() as u32, 32);
        assert_eq!(hex(&key), c.get("key").str().unwrap(), "pbkdf2 it={}", c.get("it"));
    }
}

#[test]
fn aes_gcm_matches_node() {
    for c in fixtures().get("gcm").arr() {
        let mut key = [0u8; 32];
        key.copy_from_slice(&h(c.get("key")));
        let mut iv = [0u8; 12];
        iv.copy_from_slice(&h(c.get("iv")));
        let pt = h(c.get("pt"));
        let ct = aead::gcm_encrypt(&key, &iv, &pt);
        assert_eq!(hex(&ct), c.get("ct").str().unwrap(), "gcm len {}", pt.len());
        assert_eq!(aead::gcm_decrypt(&key, &iv, &ct).unwrap(), pt);
        let mut bad = ct.clone();
        bad[0] ^= 1;
        assert!(aead::gcm_decrypt(&key, &iv, &bad).is_none(), "tampering detected");
    }
}

#[test]
fn opens_sdk_password_encrypt_output() {
    let f = fixtures();
    let s = f.get("sdkPassword");
    let mut iv = [0u8; 12];
    iv.copy_from_slice(&h(s.get("iv")));
    let pw = s.get("password").str().unwrap();
    let pt = aead::password_decrypt(pw, &h(s.get("salt")), &iv, &h(s.get("ciphertext"))).expect("decrypts SDK output");
    assert_eq!(hex(&pt), s.get("plaintext").str().unwrap());
    assert!(aead::password_decrypt("wrong", &h(s.get("salt")), &iv, &h(s.get("ciphertext"))).is_none());
    // and the SDK format round-trips the other way: same salt/iv -> same bytes
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&h(s.get("salt")));
    let again = aead::password_encrypt(pw, &pt, salt, iv);
    assert_eq!(hex(&again.ciphertext), s.get("ciphertext").str().unwrap());
}

#[test]
fn parses_our_transactions_back() {
    let kp = solana::Keypair::from_seed([5; 32]);
    let seed = iq::seed_bytes("t");
    let md = iq::inline_metadata(3, &pack::row_json("IQT1zABC"));
    let ix = iq::db_code_in_inline(&kp.pubkey, b"db", &seed, &md, None);
    let msg = solana::compile(&kp.pubkey, &[ix.clone()], [9; 32]);
    for (raw, sig) in [solana::v1_signed(&msg, &kp.seed), solana::legacy_signed(&msg, &kp.seed)] {
        let t = solana::parse_tx(&raw).expect("parses");
        assert_eq!(t.signature, base58::encode(&sig));
        let (p, accs, data) = &t.ixs[0];
        assert_eq!(t.keys[*p], iq::program_id());
        assert_eq!(t.keys[accs[0]], kp.pubkey);
        let d = iq::decode_db_code_in(data).unwrap();
        assert_eq!(d.db_id, b"db");
        assert_eq!(d.table_seed, seed);
        let row = iq::row_from_metadata(&d.metadata).unwrap();
        assert_eq!(row.get("p").str(), Some("IQT1zABC"));
    }
    // the SDK's own v1 transaction parses too
    let f = fixtures();
    let raw = h(f.get("v1CodeIn").get("raw"));
    let t = solana::parse_tx(&raw).unwrap();
    assert_eq!(t.signature, f.get("v1CodeIn").get("signature").str().unwrap());
    assert!(iq::decode_db_code_in(&t.ixs[0].2).is_some());
}

#[test]
fn account_file_roundtrip_and_imports() {
    use crate::account::{self, Parsed};
    let mut a = account::Account::new("Test", [3; 32]);
    let w2 = a.new_wallet("db: parts", "database parts");
    let cli = solana::Keypair::from_seed([8; 32]);
    assert!(a.import(cli.clone(), "cli"));
    assert!(!a.import(cli.clone(), "again"), "duplicate import ignored");
    a.set_passphrase("pass phrase", [1; 16]);
    let file = a.to_file([2; 12]);
    assert!(!file.contains(&b58(&w2.kp.pubkey)), "addresses are inside the ciphertext");
    let Parsed::Locked(name, v) = account::parse_file("acct.json", &file).unwrap() else { panic!() };
    assert_eq!(name, "Test");
    assert!(account::unlock(&v, "nope").is_err());
    let b = account::unlock(&v, "pass phrase").unwrap();
    assert_eq!(b.addresses(), a.addresses());
    assert_eq!(b.next_index, 2);
    // the encrypted payload is plain SDK passwordEncrypt output
    let salt = unhex(v.get("salt").str().unwrap()).unwrap();
    let iv: [u8; 12] = unhex(v.get("iv").str().unwrap()).unwrap().try_into().unwrap();
    assert!(crate::crypto::aead::password_decrypt("pass phrase", &salt, &iv, &unhex(v.get("ciphertext").str().unwrap()).unwrap()).is_some());
    // rescan finds a wallet created after the save
    let lost = account::derive(&[3; 32], 5);
    assert!(b.rescan_candidates().iter().any(|(i, k)| *i == 5 && k.pubkey == lost.pubkey));
    // Solana CLI keypair file
    let mut arr: Vec<u8> = cli.seed.to_vec();
    arr.extend_from_slice(&cli.pubkey);
    let cli_json = format!("[{}]", arr.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","));
    let Parsed::Keys(k) = account::parse_file("id.json", &cli_json).unwrap() else { panic!() };
    assert_eq!(k[0].1.pubkey, cli.pubkey);
    assert_eq!(k[0].0, "id");
    // text list with labels
    let txt = format!("# my keys\ntreasury: {}\n{}\n", cli.export_b58(), solana::Keypair::from_seed([9; 32]).export_b58());
    let Parsed::Keys(k) = account::parse_file("keys.txt", &txt).unwrap() else { panic!() };
    assert_eq!(k.len(), 2);
    assert_eq!(k[0].0, "treasury");
    assert!(account::parse_file("x.txt", "not a key").is_err());
    // unencrypted export round-trips
    let mut c = account::Account::new("Plain", [4; 32]);
    c.seal = None;
    let Parsed::Account(c2) = account::parse_file("p.json", &c.to_file([0; 12])).unwrap() else { panic!() };
    assert_eq!(c2.addresses(), c.addresses());
}

// ------------------------------------------------------ spreadsheet + SQL

fn cells(v: &[&str]) -> Vec<Json> {
    v.iter().map(|s| crate::ui::typed(s)).collect()
}

#[test]
fn sheet_overlays_pending_edits_on_saved_rows() {
    use crate::sheet::{self, RowState};
    use crate::state::{DraftTable, GhostRow};
    let mut tb = DraftTable { created: Some("x".into()), ..DraftTable::plain("t", vec!["id".into(), "name".into(), "qty".into()], 0) };
    // two rows saved earlier from this browser
    tb.rows.push(GhostRow { vals: cells(&["a", "bolt", "5"]), deleted: false, sig: Some("s1".into()) });
    tb.rows.push(GhostRow { vals: cells(&["b", "nut", "7"]), deleted: false, sig: Some("s1".into()) });
    let base = sheet::local_base(&tb);
    let rows = sheet::rows(&tb, &base);
    assert_eq!(rows.iter().map(|r| r.state).collect::<Vec<_>>(), vec![RowState::Saved, RowState::Saved]);
    // edit a saved row → pending copy; edit it back → nothing pending
    assert!(sheet::set_cell(&mut tb, &rows[0], 2, crate::ui::typed("6")).unwrap());
    let rows = sheet::rows(&tb, &base);
    assert_eq!(rows[0].state, RowState::Changed);
    assert!(rows[0].changed(2) && !rows[0].changed(1));
    sheet::set_cell(&mut tb, &rows[0], 2, crate::ui::typed("5")).unwrap();
    assert_eq!(sheet::pending(&sheet::rows(&tb, &base)), (0, 0, 0));
    // saved IDs can't change
    let rows = sheet::rows(&tb, &base);
    assert!(sheet::set_cell(&mut tb, &rows[1], 0, crate::ui::typed("zz")).is_err());
    // delete a saved row, then undo it by deleting again
    sheet::delete_rows(&mut tb, &[&rows[1]]);
    let rows = sheet::rows(&tb, &base);
    assert_eq!(rows[1].state, RowState::Deleted);
    sheet::delete_rows(&mut tb, &[&rows[1]]);
    assert_eq!(sheet::pending(&sheet::rows(&tb, &base)), (0, 0, 0));
    // new rows, and a batch update that reverts one edit and makes another
    sheet::insert_row(&mut tb);
    let rows = sheet::rows(&tb, &base);
    assert_eq!(rows[2].state, RowState::New);
    let errs = sheet::apply_rows(&mut tb, vec![(rows[2].clone(), cells(&["c", "washer", "1"])), (rows[0].clone(), cells(&["a", "bolt", "9"]))]);
    assert!(errs.is_empty());
    assert_eq!(sheet::pending(&sheet::rows(&tb, &base)), (1, 1, 0));
    // columns: add, move, delete keep values aligned
    sheet::add_column(&mut tb, "color", Some(1)).unwrap();
    let rows = sheet::rows(&tb, &sheet::local_base(&tb));
    assert_eq!(rows[0].vals[2].cell_text(), "bolt");
    sheet::delete_column(&mut tb, 1).unwrap();
    assert!(sheet::delete_column(&mut tb, 0).is_err());
    assert!(sheet::rename_column(&mut tb, &base, 1, "title").is_ok(), "renaming keeps the storage key");
    assert_eq!(tb.meta[1].key, "name");
    // clipboard text from Excel
    assert_eq!(sheet::parse_tsv("a\tb\r\nc\t\"d\te\"\n"), vec![vec!["a".to_string(), "b".into()], vec!["c".into(), "d\te".into()]]);
    assert_eq!(sheet::col_letter(0), "A");
    assert_eq!(sheet::col_letter(27), "AB");
}

#[test]
fn sql_parses_and_evaluates() {
    use crate::sql::{self, Stmt};
    let s = sql::parse("SELECT name, COUNT(*) AS n FROM `parts list` WHERE qty >= 10 AND name LIKE '%bolt%' GROUP BY name ORDER BY n DESC LIMIT 5 OFFSET 1; -- c\nSHOW TABLES").unwrap();
    assert_eq!(s.len(), 2);
    let Stmt::Query(q) = &s[0].0 else { panic!("{:?}", s[0]) };
    assert_eq!(q.order.len(), 1);
    assert!(q.limit.is_some() && q.offset.is_some());
    assert!(sql::parse("SELEC * FROM t").is_err());
    assert!(sql::parse("SELECT * FROM t WHERE").is_err());
    assert!(sql::parse("SELECT 'unterminated FROM t").is_err());
    let e = |src: &str| {
        let ex = sql::parse_expr(src).unwrap();
        let cat = crate::sql_exec::Snap { tables: Default::default(), views: Default::default() };
        let eng = sql::Engine::new(&cat, "", "");
        let cols = vec![sql::Col::new(None, "qty"), sql::Col::new(None, "name")];
        eng.eval_row(&cols, &[Json::Num("12".into()), Json::Str("Hex Bolt".into())], &ex).unwrap()
    };
    let t = Json::Num("1".into());
    assert_eq!(e("qty * 2 + 1"), Json::Num("25".into()));
    assert_eq!(e("name LIKE '%bolt'"), t, "LIKE is case-insensitive");
    assert_eq!(e("qty BETWEEN 10 AND 12"), t);
    assert_eq!(e("qty IN (1, 2, '12')"), t);
    assert_eq!(e("name = 'hex bolt'"), t);
    assert_eq!(e("NULL = NULL"), Json::Null);
    assert_eq!(e("CONCAT(UPPER(name), '!')"), Json::Str("HEX BOLT!".into()));
    assert_eq!(e("7 / 2"), Json::Num("3.5".into()));
    assert_eq!(e("1 / 0"), Json::Null);
}

#[test]
fn sql_runs_against_a_draft() {
    use crate::sql_exec::Out;
    let mut app = crate::app::App::new();
    app.drafts.push(crate::state::Draft::new("k".into(), "shop".into()));
    let run = |app: &mut crate::app::App, q: &str| app.run_sql("k", q);
    let out = run(&mut app, "CREATE TABLE parts (sku PRIMARY KEY, name, qty); INSERT INTO parts VALUES ('A1','Hex bolt',10), ('A2','Nut',3), ('A3','Washer',30)");
    assert!(out.iter().all(|o| matches!(o, Out::Msg(true, _))), "{:?}", out);
    assert!(matches!(&run(&mut app, "INSERT INTO parts (sku) VALUES ('A1')")[0], Out::Msg(false, m) if m.contains("already exists")));
    let out = run(&mut app, "UPDATE parts SET qty = qty + 1 WHERE qty < 20; DELETE FROM parts WHERE sku = 'A2'");
    assert!(matches!(&out[0], Out::Msg(true, m) if m.starts_with("2 row(s) changed")), "{:?}", out);
    let out = run(&mut app, "SELECT sku, qty FROM parts ORDER BY qty DESC");
    let Out::Rows { rows, .. } = &out[0] else { panic!("{:?}", out) };
    assert_eq!(rows.iter().map(|r| format!("{}={}", r[0].cell_text(), r[1].cell_text())).collect::<Vec<_>>(), vec!["A3=30", "A1=11"]);
    let out = run(&mut app, "SELECT COUNT(*) AS n, SUM(qty) AS total, AVG(qty) FROM parts");
    let Out::Rows { rows, cols, .. } = &out[0] else { panic!() };
    assert_eq!(cols, &vec!["n".to_string(), "total".into(), "AVG(qty)".into()]);
    assert_eq!(rows[0].iter().map(|v| v.cell_text()).collect::<Vec<_>>(), vec!["2", "41", "20.5"]);
    let out = run(&mut app, "ALTER TABLE parts ADD COLUMN color; UPDATE parts SET color = 'red' WHERE sku = 'A3'; SELECT sku FROM parts WHERE color IS NOT NULL");
    let Out::Rows { rows, .. } = &out[2] else { panic!("{:?}", out) };
    assert_eq!(rows.len(), 1);
    assert!(matches!(&run(&mut app, "SELECT nope FROM parts")[0], Out::Msg(false, m) if m.contains("Unknown column")));
    assert!(matches!(&run(&mut app, "DROP TABLE parts")[0], Out::Msg(true, _)), "unsaved tables can be dropped");
    assert!(matches!(&run(&mut app, "SELECT * FROM parts")[0], Out::Msg(false, m) if m.contains("No table")));
}

#[test]
fn dropping_a_table_forgets_what_was_computed_for_its_position() {
    let mut app = crate::app::App::new();
    app.drafts.push(crate::state::Draft::new("k".into(), "shop".into()));
    let out = app.run_sql("k", "CREATE TABLE a (id PRIMARY KEY); INSERT INTO a VALUES ('x'); CREATE TABLE b (id PRIMARY KEY); INSERT INTO b VALUES ('1'), ('2'), ('3')");
    assert!(out.iter().all(|o| matches!(o, crate::sql_exec::Out::Msg(true, _))), "{:?}", out);
    let cap = app.inline_cap();
    let count = |app: &mut crate::app::App, t| app.plan_for("k", t, cap).as_ref().unwrap().iter().map(|p| p.count).sum::<usize>();
    assert_eq!((count(&mut app, 0), count(&mut app, 1)), (1, 3));
    app.run_sql("k", "DROP TABLE a");
    assert_eq!(count(&mut app, 0), 3, "table b moved to position 0; its own plan is used");
}

#[test]
fn iq_git_links() {
    use crate::git;
    // addresses as they exist on mainnet (IQ git's own derivation)
    let owner = "BniQFboKCynd5Xfwb3nWudAaDq4pa3yWq4zjNHVespG4";
    let pda = "FLYAenCNTwiR7FaxDuVuxkqWtSXs6oNXjzZrwWimwAPt";
    assert_eq!(b58(&iq::db_root_pda(&iq::seed_bytes(git::GIT_DB))), "AVJUfjuGiJcsGVZT13gD3vAbJqXPcrXXMcTZJuTgCs5x");
    assert_eq!(git::commits_pda(owner, "blockchain-internet"), pda);
    assert_eq!(git::browser_pda(&format!("https://browser.iqlabs.dev/{}", pda)), Some(pda));
    assert_eq!(git::browser_pda(&format!("browser.iqlabs.dev/{}/", pda)), Some(pda));
    assert_eq!(git::browser_pda("https://browser.iqlabs.dev/alice.sol"), None);
    let name = format!("git_commits:{}:blockchain-internet", owner);
    assert_eq!(git::repo_of(pda, &name), Some((owner.to_string(), "blockchain-internet".to_string())));
    // the same name on a table somewhere else isn't the repository
    assert_eq!(git::repo_of("GFLaoPfndXaxHZE3GpZ3NysTLwNuXA4xp2R1CmUFuFY2", &name), None);
    assert_eq!(git::repo_of(pda, "notes"), None);
    // gateway rows (shape as served by gateway.iqlabs.dev), plus a row someone else inscribed
    let tree_a = "5ZpBaYn9HsCV8wpbzj1yoiqTTn6UkNoq3RuhkCKBJdiroxqxPjXgdt24eoJ9kGep52FTX88n26A9Vkw3bDy94p2z";
    let tree_b = "2uRwrfNobjpMiSUmVfMcxa5fc1i2JJhDNJRPkxLX1TD8X55r3S9Y9ftYEBEMb46hRAxzQVmQg2ws1v6USzsdjnDj";
    let rows = json::parse(&format!(
        r#"[{{"id":"7f5f5851","message":"first","treeTxId":"{a}","timestamp":1781197549125,"author":"{o}","__signer":"{o}"}},
            {{"id":"a8ccd41d","message":"edit\nmore","treeTxId":"{b}","parentCommitId":"7f5f5851","timestamp":1781198047455,"author":"{o}","__signer":"{o}"}},
            {{"id":"spoof","message":"totally the latest","treeTxId":"{b}","timestamp":1799999999999,"author":"{o}","__signer":"GFLaoPfndXaxHZE3GpZ3NysTLwNuXA4xp2R1CmUFuFY2"}}]"#,
        a = tree_a,
        b = tree_b,
        o = owner
    ))
    .unwrap();
    let c = git::parse_commits(rows.arr(), owner);
    assert_eq!(c.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["a8ccd41d", "7f5f5851"], "newest first, owner's commits only");
    assert_eq!(c[0].parent, "7f5f5851");
    assert!(git::pinned_link("blockchain-internet", &c[0]).starts_with(&format!("iq://tx/{}#blockchain-internet%40a8ccd41d", tree_b)));
    let (sig, label) = crate::attach::parse_tx_link(&git::pinned_link("blockchain-internet", &c[0])).unwrap();
    assert_eq!((sig.as_str(), label.as_str()), (tree_b, "blockchain-internet@a8ccd41d"));
    // a tree inscription and a file of it
    let tree = git::parse_tree(r#"{"iqpages.json":{"txId":"21Y6pXb84UfKxQDsJjWuw3hEv1yrgjzvwdMvA54zcc78TbSxhRzBh8Th8k9w7SdyPDgpSSjj63GX4PpHEECnGLez","hash":"31"},"index.html":{"txId":"2sWxhbizTf5EhJ96pWzf43QJnGshbGDD35ZrzcsM4v6dhL2qufiqAoc1jWeuRruiXif5sXsywAoRVWDrkymen2RM","hash":"c9"}}"#).unwrap();
    assert_eq!(tree.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(), vec!["index.html", "iqpages.json"]);
    assert_eq!(git::parse_tree(r#"{"a":{"nope":1}}"#), None);
    let v = crate::attach::viewed("application/octet-stream", "iqgit-blob:index.html", crate::crypto::base64_encode(b"<!doctype html>\n<p>hi</p>"), owner.into(), None, "IQ gateway");
    assert_eq!((v.filename.as_str(), v.filetype.as_str(), v.text.as_deref()), ("index.html", "text/html", Some("<!doctype html>\n<p>hi</p>")));
    let v = crate::attach::viewed("application/octet-stream", "iqgit-blob:logo.png", crate::crypto::base64_encode(&[137, 80, 78, 71, 0, 1]), owner.into(), None, "IQ gateway");
    assert_eq!((v.filetype.as_str(), v.bytes.as_ref().map(|b| b.len())), ("image/png", Some(6)));
    assert_eq!(git::ago(10_000_000.0, 10_000_000 - 3 * 86_400_000), "3 days ago");
    assert_eq!(git::ago(10_000_000.0, 10_000_000 - 3_600_000), "1 hour ago");
}
