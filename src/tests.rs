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
    let sp = |tx: &str, recs| pack::SourcePack { tx: tx.into(), signer: "S".into(), time: None, schema: schema.clone(), recs };
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
