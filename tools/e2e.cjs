// End-to-end test: the built single-file app in headless Chromium against
//  * a mock Solana chain that decodes every IQ instruction with the official
//    IDL coder, checks accounts/data against the SDK's own instruction builder,
//    verifies Ed25519 signatures (legacy and v1 wire formats) and applies the
//    effects (DbRoot, tables, writer locks, rows, fees);
//  * a mock IQ gateway serving rows straight from that chain;
//  * two fake Wallet Standard wallets.
// Dev-only. Run from tools/: npm install && npm run e2e
const fs = require("fs");
const path = require("path");
const { chromium } = require("playwright-core");
const iqmod = require("@iqlabs-official/solana-sdk");
const iq = iqmod.default || iqmod;
const SDK_DIST = process.env.SDK_DIST || require("path").dirname(require.resolve("@iqlabs-official/solana-sdk"));
const anchor = require("@coral-xyz/anchor");
const { PublicKey, Transaction, Keypair } = require("@solana/web3.js");
const { ed25519 } = require("@noble/curves/ed25519.js");
const bs58 = require("bs58").default || require("bs58");
const IDL = require(path.join(SDK_DIST, "..", "idl", "code_in.json"));

const ROOT = path.join(__dirname, "..");
const OUT = path.join(ROOT, "target", "e2e");
fs.mkdirSync(OUT, { recursive: true });
const ixCoder = new anchor.BorshInstructionCoder(IDL);
const accCoder0 = new anchor.BorshAccountsCoder(IDL);
// anchor 0.32's BorshAccountsCoder.encode is async; encode synchronously with its layouts
const accCoder = {
  decode: (n, d) => accCoder0.decode(n, d),
  encode: (n, obj) => {
    const { discriminator, layout } = accCoder0.accountLayouts.get(n);
    const buf = Buffer.alloc(64 * 1024);
    const len = layout.encode(obj, buf);
    return Buffer.concat([Buffer.from(discriminator), buf.subarray(0, len)]);
  },
};
const PID = iq.contract.PROGRAM_ID;
const SYS = "11111111111111111111111111111111";
const FEE_RECEIVER = iq.constants.DEFAULT_WRITE_FEE_RECEIVER;
const builder = iq.contract.createInstructionBuilder();
const LAMPORTS = 1_000_000_000;
const TABLE_FEE = 10_000_000; // assumed for the mock; the real amount isn't published

const results = [];
const check = (ok, what) => {
  results.push([ok, what]);
  console.log(ok ? "  ✓" : "  ✗", what);
  if (!ok) process.exitCode = 1;
};

// ------------------------------------------------------------------ chain
const chain = {
  accounts: new Map(), // b58 -> { lamports, data: Buffer, owner }
  rows: new Map(), // table pda -> [row objects], oldest first
  sigs: new Map(), // sig -> { err }
  notifies: [],
  txCount: { v1: 0, legacy: 0 },
  ixSeen: {},
};
const acct = (k) => chain.accounts.get(k);
const lam = (k) => (acct(k) ? acct(k).lamports : 0);
const setAcct = (k, a) => chain.accounts.set(k, a);
const rent = (n) => (n + 128) * 6960;

function credit(st, k, n) {
  const a = st.accounts.get(k) || { lamports: 0, data: Buffer.alloc(0), owner: SYS };
  a.lamports += n;
  st.accounts.set(k, a);
}
function debit(st, k, n, why) {
  const a = st.accounts.get(k);
  if (!a || a.lamports < n) throw new Error(`insufficient lamports for ${why}: need ${n}, have ${a ? a.lamports : 0}`);
  a.lamports -= n;
}

function parseTx(raw) {
  if (raw[0] === 129) {
    chain.txCount.v1++;
    let o = 0;
    const [, nsig, roSigned, roUnsigned] = raw.subarray(0, 4);
    o = 4;
    const mask = raw.readUInt32LE(o); o += 4;
    const blockhash = raw.subarray(o, o + 32); o += 32;
    const nIx = raw[o++], nKeys = raw[o++];
    const keys = [];
    for (let i = 0; i < nKeys; i++) { keys.push(new PublicKey(raw.subarray(o, o + 32))); o += 32; }
    if (mask & 3) o += 8;
    if (mask & 4) o += 4;
    if (mask & 8) o += 4;
    if (mask & 16) o += 4;
    const heads = [];
    for (let i = 0; i < nIx; i++) { heads.push([raw[o], raw[o + 1], raw.readUInt16LE(o + 2)]); o += 4; }
    const ixs = [];
    for (const [p, na, dl] of heads) {
      const idx = [...raw.subarray(o, o + na)]; o += na;
      const data = Buffer.from(raw.subarray(o, o + dl)); o += dl;
      ixs.push({ p, idx, data });
    }
    const msgEnd = raw.length - 64 * nsig;
    if (o !== msgEnd) throw new Error(`v1 layout mismatch: parsed ${o}, expected ${msgEnd}`);
    const msg = raw.subarray(0, msgEnd);
    for (let i = 0; i < nsig; i++) {
      const sig = raw.subarray(msgEnd + 64 * i, msgEnd + 64 * (i + 1));
      if (!ed25519.verify(sig, msg, keys[i].toBytes())) throw new Error("v1 signature invalid");
    }
    const isW = (i) => (i < nsig ? i < nsig - roSigned : i < nKeys - roUnsigned);
    const instructions = ixs.map((x) => ({
      programId: keys[x.p],
      keys: x.idx.map((i) => ({ pubkey: keys[i], isSigner: i < nsig, isWritable: isW(i) })),
      data: x.data,
    }));
    return { sig: bs58.encode(raw.subarray(msgEnd, msgEnd + 64)), feePayer: keys[0], signers: keys.slice(0, nsig), instructions, nsig, blockhash };
  }
  chain.txCount.legacy++;
  const tx = Transaction.from(raw);
  if (!tx.verifySignatures()) throw new Error("legacy signature invalid");
  const msg = tx.compileMessage();
  const nsig = msg.header.numRequiredSignatures;
  return {
    sig: bs58.encode(tx.signatures[0].signature),
    feePayer: tx.feePayer,
    signers: msg.accountKeys.slice(0, nsig),
    instructions: tx.instructions,
    nsig,
  };
}

function sameIx(ours, sdk, what) {
  // Flags in a compiled transaction are per-key for the whole message, so a
  // key that is a signer anywhere shows as a signer everywhere. Compare keys
  // exactly and flags "at least as strong as the SDK's, and explained by the
  // same key being signer/writable elsewhere in the instruction".
  const a = ours.keys.map((k) => k.pubkey.toBase58());
  const b = sdk.keys.map((k) => k.pubkey.toBase58());
  const bad = () => { throw new Error(`${what}: accounts differ from SDK\n ours ${JSON.stringify(ours.keys.map((k) => [k.pubkey.toBase58(), k.isSigner, k.isWritable]))}\n sdk  ${JSON.stringify(sdk.keys.map((k) => [k.pubkey.toBase58(), k.isSigner, k.isWritable]))}`); };
  if (JSON.stringify(a) !== JSON.stringify(b)) bad();
  sdk.keys.forEach((k, i) => {
    const o = ours.keys[i];
    const sameKey = sdk.keys.filter((x) => x.pubkey.equals(k.pubkey));
    const expS = sameKey.some((x) => x.isSigner), expW = sameKey.some((x) => x.isWritable);
    if (o.isSigner !== expS || o.isWritable !== expW) bad();
  });
  if (!Buffer.from(ours.data).equals(Buffer.from(sdk.data))) throw new Error(`${what}: data differs from SDK`);
}

function decodeRoot(st, k) {
  const a = st.accounts.get(k);
  return a ? accCoder.decode("DbRoot", a.data) : null;
}
function encodeRoot(st, k, root, size) {
  const enc = accCoder.encode("DbRoot", root);
  return enc; // caller handles sizing
}

function execute(st, tx) {
  const logs = [];
  debit(st, tx.feePayer.toBase58(), 5000 * tx.nsig, "tx fee");
  for (const ix of tx.instructions) {
    const prog = ix.programId.toBase58();
    if (prog === SYS) {
      const kind = ix.data.readUInt32LE(0);
      if (kind !== 2) throw new Error("unexpected system instruction");
      const n = Number(ix.data.readBigUInt64LE(4));
      debit(st, ix.keys[0].pubkey.toBase58(), n, "transfer");
      credit(st, ix.keys[1].pubkey.toBase58(), n);
      logs.push(`transfer ${n}`);
      continue;
    }
    if (prog !== PID.toBase58()) throw new Error("unknown program " + prog);
    const dec = ixCoder.decode(Buffer.from(ix.data));
    if (!dec) throw new Error("IDL could not decode instruction data");
    const a = dec.data;
    chain.ixSeen[dec.name] = (chain.ixSeen[dec.name] || 0) + 1;
    logs.push("Program log: Instruction: " + dec.name);
    const K = (i) => ix.keys[i].pubkey;
    const signer = tx.signers[0];
    if (dec.name === "initialize_db_root") {
      const root = iq.contract.getDbRootPda(Buffer.from(a.db_root_id), PID);
      sameIx(ix, iq.contract.initializeDbRootInstruction(builder, { db_root: root, signer, system_program: new PublicKey(SYS) }, a), dec.name);
      if (st.accounts.get(root.toBase58())) throw new Error("custom program error: DbRoot already in use");
      if (a.db_root_id.length > 32) throw new Error("custom program error: 0x1775 InvalidDbRootId");
      const data = accCoder.encode("DbRoot", {
        creator: signer, table_seeds: [], global_table_seeds: [], id: Buffer.from(a.db_root_id),
        table_creators: [], ext_creators: [], table_creation_fee_override: new anchor.BN(0), table_creation_fee_is_set: false,
      });
      const sized = Buffer.concat([data, Buffer.alloc(96)]); // small headroom so realloc gets exercised
      debit(st, signer.toBase58(), rent(sized.length), "DbRoot rent");
      st.accounts.set(root.toBase58(), { lamports: rent(sized.length), data: sized, owner: PID.toBase58() });
    } else if (dec.name === "manage_table_creators") {
      const root = iq.contract.getDbRootPda(Buffer.from(a.db_root_id), PID);
      sameIx(ix, iq.contract.manageTableCreatorsInstruction(builder, { signer, db_root: root, system_program: new PublicKey(SYS) }, a), dec.name);
      const r = decodeRoot(st, root.toBase58());
      if (!r.creator.equals(signer)) throw new Error("custom program error: 0x1770 NotAuthorized");
      r.table_creators = a.table_creators;
      r.ext_creators = a.ext_creators;
      const acc = st.accounts.get(root.toBase58());
      const enc = accCoder.encode("DbRoot", r);
      const size = Math.max(acc.data.length, enc.length);
      acc.data = Buffer.concat([enc, Buffer.alloc(size - enc.length)]);
    } else if (dec.name === "realloc_account") {
      const target = K(1).toBase58();
      const acc = st.accounts.get(target);
      const ns = Number(a.new_size);
      if (ns <= acc.data.length) throw new Error("realloc must grow");
      debit(st, signer.toBase58(), rent(ns) - rent(acc.data.length), "realloc rent");
      acc.lamports += rent(ns) - rent(acc.data.length);
      acc.data = Buffer.concat([acc.data, Buffer.alloc(ns - acc.data.length)]);
      chain.reallocs = (chain.reallocs || 0) + 1;
    } else if (dec.name === "create_table") {
      const root = iq.contract.getDbRootPda(Buffer.from(a.db_root_id), PID);
      const r = decodeRoot(st, root.toBase58());
      if (!r) throw new Error("db_root not found");
      const seed = Buffer.from(a.table_seed);
      const expected = iq.contract.createTableInstruction(builder, {
        db_root: root, receiver: new PublicKey(FEE_RECEIVER), db_root_creator: r.creator, signer,
        table: iq.contract.getTablePda(root, seed, PID), instruction_table: iq.contract.getInstructionTablePda(root, seed, PID),
        system_program: new PublicKey(SYS),
      }, a);
      sameIx(ix, expected, dec.name);
      if (r.table_creators.length && !r.creator.equals(signer) && !r.table_creators.some((c) => c.equals(signer))) throw new Error("custom program error: 0x1770 NotAuthorized");
      const tpda = iq.contract.getTablePda(root, seed, PID).toBase58();
      if (st.accounts.get(tpda)) throw new Error("custom program error: 0x1783 TableExists");
      const cols = a.column_names.map((c) => Buffer.from(c).toString());
      if (!cols.includes(Buffer.from(a.id_col).toString())) throw new Error("custom program error: 0x1782 IdColNotInColumns");
      // DbRoot must have room for the hint twice (the SDK reallocs first when it doesn't)
      r.table_seeds.push(Buffer.from(a.table_hint));
      r.global_table_seeds.push(Buffer.from(a.table_hint));
      const enc = accCoder.encode("DbRoot", r);
      const racc = st.accounts.get(root.toBase58());
      if (enc.length > racc.data.length) throw new Error("custom program error: 0x1775 NeedsRealloc (DbRoot full)");
      racc.data = Buffer.concat([enc, Buffer.alloc(racc.data.length - enc.length)]);
      const tdata = accCoder.encode("Table", {
        column_names: a.column_names, id_col: a.id_col, ext_keys: a.ext_keys, name: a.table_name,
        last_timestamp: new anchor.BN(0), gate: { mint: new PublicKey(SYS), amount: new anchor.BN(0), gate_type: 0 },
        writers: a.writers_opt || [],
      });
      debit(st, signer.toBase58(), rent(tdata.length) + rent(9) + TABLE_FEE, "create_table");
      credit(st, FEE_RECEIVER, TABLE_FEE / 2);
      credit(st, r.creator.toBase58(), TABLE_FEE / 2);
      st.accounts.set(tpda, { lamports: rent(tdata.length), data: tdata, owner: PID.toBase58() });
      st.accounts.set(iq.contract.getInstructionTablePda(root, seed, PID).toBase58(), { lamports: rent(9), data: Buffer.alloc(9), owner: PID.toBase58() });
    } else if (dec.name === "user_initialize") {
      sameIx(ix, iq.contract.userInitializeInstruction(builder, {
        user: signer, code_account: iq.contract.getCodeAccountPda(signer, PID), user_state: iq.contract.getUserPda(signer, PID),
        user_inventory: iq.contract.getUserInventoryPda(signer, PID), system_program: new PublicKey(SYS),
      }), dec.name);
      const inv = iq.contract.getUserInventoryPda(signer, PID).toBase58();
      if (st.accounts.get(inv)) throw new Error("already initialized");
      const us = accCoder.encode("UserState", { owner: signer, trail_anchor: Buffer.alloc(0), metadata: Buffer.alloc(0), total_session_files: new anchor.BN(0) });
      debit(st, signer.toBase58(), rent(4215) + rent(4213) + rent(us.length), "user init rent");
      st.accounts.set(inv, { lamports: rent(4213), data: Buffer.alloc(4213), owner: PID.toBase58() });
      st.accounts.set(iq.contract.getCodeAccountPda(signer, PID).toBase58(), { lamports: rent(4215), data: Buffer.alloc(4215), owner: PID.toBase58() });
      st.accounts.set(iq.contract.getUserPda(signer, PID).toBase58(), { lamports: rent(us.length), data: us, owner: PID.toBase58() });
    } else if (dec.name === "db_code_in") {
      const root = iq.contract.getDbRootPda(Buffer.from(a.db_root_id), PID);
      const seed = Buffer.from(a.table_seed);
      const tpda = iq.contract.getTablePda(root, seed, PID);
      const expected = iq.contract.dbCodeInInstruction(builder, {
        user: signer, signer, user_inventory: iq.contract.getUserInventoryPda(signer, PID), db_root: root, table: tpda,
        system_program: new PublicKey(SYS), receiver: new PublicKey(FEE_RECEIVER),
      }, a);
      sameIx(ix, expected, dec.name);
      if (!st.accounts.get(iq.contract.getUserInventoryPda(signer, PID).toBase58())) throw new Error("custom program error: user not initialized");
      const tacc = st.accounts.get(tpda.toBase58());
      if (!tacc) throw new Error("table not found");
      const meta = accCoder.decode("Table", tacc.data);
      if (meta.writers.length && !meta.writers.some((w) => w.equals(signer))) throw new Error("custom program error: 0x1770 NotAuthorized (signer not in writers)");
      if (a.on_chain_path !== "" || a.session) throw new Error("expected an inline write");
      if (Buffer.byteLength(a.metadata) > 3400) throw new Error("metadata over the inline cap: " + Buffer.byteLength(a.metadata));
      const md = JSON.parse(a.metadata);
      if (md.total_chunks !== 1 || md.method !== 0) throw new Error("bad metadata envelope");
      const row = JSON.parse(md.data);
      const cols = new Set([...meta.column_names.map((c) => Buffer.from(c).toString()), Buffer.from(meta.id_col).toString()]);
      for (const k of Object.keys(row)) if (!cols.has(k)) throw new Error("custom program error: 0x1787 SchemaMismatch " + k);
      debit(st, signer.toBase58(), 1_000_000, "write fee");
      credit(st, FEE_RECEIVER, 1_000_000);
      st.pendingRows = st.pendingRows || [];
      st.pendingRows.push({ table: tpda.toBase58(), row, signer: signer.toBase58() });
    } else {
      throw new Error("unexpected instruction " + dec.name);
    }
  }
  return logs;
}

function cloneState() {
  const accounts = new Map();
  for (const [k, v] of chain.accounts) accounts.set(k, { lamports: v.lamports, data: Buffer.from(v.data), owner: v.owner });
  return { accounts };
}

let blockTime = 1790600000;
function submit(raw, simulate) {
  const tx = parseTx(Buffer.from(raw));
  const st = cloneState();
  let err = null, logs = [];
  try { logs = execute(st, tx); } catch (e) { err = e.message; }
  if (simulate) return { tx, st, err, logs };
  if (!err) {
    chain.accounts = st.accounts;
    for (const p of st.pendingRows || []) {
      const list = chain.rows.get(p.table) || [];
      list.push({ ...p.row, __txSignature: tx.sig, __signer: p.signer, __blockTime: ++blockTime });
      chain.rows.set(p.table, list);
    }
  }
  chain.sigs.set(tx.sig, { err });
  return { tx, err, logs };
}

function rpc(body) {
  const { method, params, id } = JSON.parse(body);
  const ok = (result) => ({ jsonrpc: "2.0", id, result });
  const ctx = { context: { slot: 1000 } };
  const info = (k) => {
    const a = acct(k);
    return a ? { lamports: a.lamports, owner: a.owner, executable: false, rentEpoch: 0, data: [a.data.toString("base64"), "base64"] } : null;
  };
  switch (method) {
    case "getBalance": return ok({ ...ctx, value: lam(params[0]) });
    case "getAccountInfo": return ok({ ...ctx, value: info(params[0]) });
    case "getMultipleAccounts": return ok({ ...ctx, value: params[0].map(info) });
    case "getLatestBlockhash": return ok({ ...ctx, value: { blockhash: bs58.encode(Buffer.from(Array.from({ length: 32 }, () => Math.random() * 256 | 0))), lastValidBlockHeight: 99999 } });
    case "simulateTransaction": {
      const raw = Buffer.from(params[0], "base64");
      const r = submit(raw, true);
      const addrs = (params[1] && params[1].accounts && params[1].accounts.addresses) || [];
      return ok({ ...ctx, value: {
        err: r.err ? { InstructionError: [0, { Custom: 1 }] } : null,
        logs: r.err ? [...r.logs, "Program log: Error: " + r.err] : r.logs,
        accounts: addrs.map((k) => { const a = r.st.accounts.get(k); return a ? { lamports: a.lamports, data: ["", "base64"], owner: a.owner } : null; }),
        unitsConsumed: 12345,
      } });
    }
    case "sendTransaction": {
      const raw = Buffer.from(params[0], "base64");
      const r = submit(raw, false);
      if (r.err && !(params[1] && params[1].skipPreflight)) return { jsonrpc: "2.0", id, error: { code: -32002, message: "Transaction simulation failed: " + r.err } };
      return ok(r.tx.sig);
    }
    case "getSignatureStatuses": return ok({ ...ctx, value: params[0].map((s) => {
      const st = chain.sigs.get(s);
      return st ? { slot: 1000, confirmations: null, err: st.err ? { InstructionError: [0, "Custom"] } : null, confirmationStatus: "confirmed" } : null;
    }) });
    default: return { jsonrpc: "2.0", id, error: { code: -32601, message: "mock: unsupported " + method } };
  }
}

// ---------------------------------------------------------------- gateway
const CREATOR_LOCKER = "B8d355pft6DfrQNetCqXNumRk8WoEs21waqeuPP3HUJC";
const fixtureRoots = [
  { pda: "HrWK65t1rXTebdeKPV8Pa2WMuv7qah5mB4hvzv6TA7YM", id: "iq-locker", idHex: "69712d6c6f636b6572", creator: CREATOR_LOCKER, tableCreators: [], extCreators: [],
    tableSeeds: [{ label: "notes", hex: "6e6f746573", tablePda: "3n7hcAoXkNhTc6CCGvVafkHfWmq3Rf72VXapMyzE6ZvP" }], globalTableSeeds: [] },
  { pda: "CAWKtbVu66RWyj5JNi8rBjfAQY6dMBWkSSugpvs99DLY", id: "iq-snake-game", idHex: "69712d736e616b652d67616d65", creator: "8QWrZjNNFzngKWCLCrFkAy7ydnagrBSVYdJFyEvw9agh", tableCreators: [], extCreators: [],
    tableSeeds: [{ label: "scores", hex: "73636f726573", tablePda: "4Rt1HfeqLy8SUptjqizWDeGioitfVPGZXTYc1obb2szU" }], globalTableSeeds: [] },
  { pda: "2PELnigDpNrE8wZaJ2n2va3tnBCsjtPQ8o2v2KEUc3Pb", id: null, idHex: "3258e463ec592b498d145c5cd3b93cbf3340e4f60393cde8c1ac700b8024b09e", creator: "69FKfGvVQyKrnbgh7yC5hXwPpfbiTtTbjDUiMGonibLM", tableCreators: [], extCreators: [],
    tableSeeds: [{ label: null, hex: "82f3777f4e7bb9d69059781ff58f92870a42251ea639ed37e6b049fe500ec341", tablePda: "Gt6eGCL7urcPSZqVURWx658cNFVa5GtpnV5ChSusW7u8" }], globalTableSeeds: [] },
];
chain.rows.set("3n7hcAoXkNhTc6CCGvVafkHfWmq3Rf72VXapMyzE6ZvP", [
  { id: "n1", text: "first note", __txSignature: "5sigA", __signer: CREATOR_LOCKER, __blockTime: 1777000000 },
  { id: "n2", text: "second <b>note</b>", __txSignature: "5sigB", __signer: CREATOR_LOCKER, __blockTime: 1777000100 },
  { id: "n3", text: "a visitor wrote this", __txSignature: "5sigC", __signer: "8QWrZjNNFzngKWCLCrFkAy7ydnagrBSVYdJFyEvw9agh", __blockTime: 1777000200 },
]);

function liveRoots() {
  const out = [...fixtureRoots];
  for (const [k, a] of chain.accounts) {
    if (a.owner !== PID.toBase58() || a.data.length < 8) continue;
    let r;
    try { r = accCoder.decode("DbRoot", a.data); } catch (_) { continue; }
    const id = Buffer.from(r.id).toString();
    const seeds = r.table_seeds.map((h) => ({ label: Buffer.from(h).toString(), hex: Buffer.from(h).toString("hex"), tablePda: iq.contract.getTablePda(new PublicKey(k), iq.utils.toSeedBytes(Buffer.from(h).toString()), PID).toBase58() }));
    out.push({ pda: k, id, idHex: Buffer.from(r.id).toString("hex"), creator: r.creator.toBase58(), tableCreators: r.table_creators.map((x) => x.toBase58()), extCreators: [], tableSeeds: seeds, globalTableSeeds: seeds });
  }
  return { dbroots: out, count: out.length };
}

function gateway(url, method, body) {
  const u = new URL(url);
  const p = u.pathname;
  if (p === "/dbroots") return liveRoots();
  if (p === "/search") return { q: u.searchParams.get("q"), hits: [
    { kind: "dbroot", id: "HrWK65t1rXTebdeKPV8Pa2WMuv7qah5mB4hvzv6TA7YM", network: "solana", dbroot: "iq-locker", label: "iq-locker", snippet: "DbRoot iq-locker" },
    { kind: "table", id: "3n7hcAoXkNhTc6CCGvVafkHfWmq3Rf72VXapMyzE6ZvP", network: "solana", dbroot: "iq-locker", label: "notes", snippet: "iq-locker / notes" },
    { kind: "row", id: "5sigA", network: "solana", dbroot: "", label: "notes — first note", snippet: "first note" },
  ], count: 3 };
  let m = p.match(/^\/table\/([^/]+)\/meta$/);
  if (m) {
    const a = acct(m[1]);
    if (a) {
      const t = accCoder.decode("Table", a.data);
      return { name: Buffer.from(t.name).toString(), columns: t.column_names.map((c) => Buffer.from(c).toString()), idCol: Buffer.from(t.id_col).toString(), lastTimestamp: 0, gate: null };
    }
    if (m[1] === "3n7hcAoXkNhTc6CCGvVafkHfWmq3Rf72VXapMyzE6ZvP") return { name: "notes", columns: ["id", "text"], idCol: "id", lastTimestamp: 1777000200, gate: null };
    return { error: "not found" };
  }
  m = p.match(/^\/table\/([^/]+)\/rows$/);
  if (m) {
    const all = [...(chain.rows.get(m[1]) || [])].reverse(); // newest first
    const limit = Math.min(Number(u.searchParams.get("limit") || 50), 100);
    const before = u.searchParams.get("before");
    let start = 0;
    if (before) start = all.findIndex((r) => r.__txSignature === before) + 1;
    const rows = all.slice(start, start + limit);
    return { tablePda: m[1], rows, count: rows.length, nextCursor: start + limit < all.length ? rows[rows.length - 1].__txSignature : null, cached: false };
  }
  m = p.match(/^\/table\/([^/]+)\/notify$/);
  if (m && method === "POST") { chain.notifies.push({ table: m[1], body: JSON.parse(body) }); return { ok: true }; }
  return { error: "mock: no route " + p };
}

// ----------------------------------------------------------------- wallets
const walletKeys = {
  "Test Wallet": Keypair.fromSeed(Buffer.alloc(32, 7)),
  "Other Wallet": Keypair.fromSeed(Buffer.alloc(32, 9)),
};
for (const kp of Object.values(walletKeys)) setAcct(kp.publicKey.toBase58(), { lamports: 10 * LAMPORTS, data: Buffer.alloc(0), owner: SYS });

const walletInit = (names) => `
(() => {
  const mk = (name) => {
    const w = { version: "1.0.0", name, icon: "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAxIDEiPjxyZWN0IHdpZHRoPSIxIiBoZWlnaHQ9IjEiIGZpbGw9IiM4ODgiLz48L3N2Zz4=", chains: ["solana:mainnet", "solana:devnet"], accounts: [],
      features: {
        "standard:connect": { version: "1.0.0", connect: async () => { const address = await window.__walletAddress(name); const account = { address, publicKey: new Uint8Array(32), chains: w.chains, features: [] }; w.accounts = [account]; return { accounts: [account] }; } },
        "standard:disconnect": { version: "1.0.0", disconnect: async () => { w.accounts = []; } },
        "solana:signMessage": { version: "1.0.0", signMessage: async ({ message }) => { const s = await window.__walletSign(name, Array.from(message)); return [{ signedMessage: message, signature: new Uint8Array(s) }]; } },
        "solana:signAndSendTransaction": { version: "1.0.0", supportedTransactionVersions: ["legacy"], signAndSendTransaction: async ({ transaction, chain }) => { const s = await window.__walletSend(name, Array.from(transaction), chain); if (typeof s === "string") throw new Error(s); return [{ signature: new Uint8Array(s) }]; } },
      } };
    return w;
  };
  const ws = ${JSON.stringify(names)}.map(mk);
  window.addEventListener("wallet-standard:app-ready", (e) => e.detail.register(...ws));
})();`;

// -------------------------------------------------------------------- run
(async () => {
  const html = fs.readFileSync(path.join(ROOT, "site", "index.html"), "utf8");
  // CHROME_PATH: any Chromium/Chrome binary; otherwise Playwright's installed browser.
  const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH || undefined, args: ["--no-sandbox"] });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 } });
  const page = await ctx.newPage();
  const consoleErrors = [];
  page.on("console", (m) => { if (m.type() === "error") consoleErrors.push(m.text()); });
  page.on("pageerror", (e) => consoleErrors.push("pageerror: " + e.message));
  await page.exposeFunction("__walletAddress", (name) => walletKeys[name].publicKey.toBase58());
  await page.exposeFunction("__walletSign", (name, msg) => Array.from(ed25519.sign(Uint8Array.from(msg), walletKeys[name].secretKey.slice(0, 32))));
  await page.exposeFunction("__walletSend", (name, txArr) => {
    const tx = Transaction.from(Buffer.from(txArr));
    tx.partialSign(walletKeys[name]);
    const r = submit(tx.serialize(), false);
    if (r.err) return "rejected: " + r.err;
    return Array.from(bs58.decode(r.tx.sig));
  });
  await page.addInitScript(walletInit(Object.keys(walletKeys)));
  await page.route("**/*", async (route) => {
    const req = route.request();
    const url = req.url();
    if (url.startsWith("https://iq.test/")) return route.fulfill({ status: 200, contentType: "text/html", body: html });
    if (url.startsWith("https://gateway.iqlabs.dev/")) {
      const res = gateway(url, req.method(), req.postData());
      return route.fulfill({ status: res.error ? 404 : 200, contentType: "application/json", headers: { "access-control-allow-origin": "*" }, body: JSON.stringify(res) });
    }
    if (url.startsWith("https://api.mainnet-beta.solana.com")) {
      if (req.method() === "OPTIONS") return route.fulfill({ status: 204, headers: { "access-control-allow-origin": "*", "access-control-allow-headers": "content-type" } });
      let res;
      try { res = rpc(req.postData()); } catch (e) { res = { jsonrpc: "2.0", id: 1, error: { code: -1, message: "mock crashed: " + e.message } }; }
      return route.fulfill({ status: 200, contentType: "application/json", headers: { "access-control-allow-origin": "*" }, body: JSON.stringify(res) });
    }
    return route.fulfill({ status: 404, body: "blocked in test: " + url });
  });
  const openDetails = (label) => page.evaluate((l) => {
    for (const s of document.querySelectorAll("details > summary")) if (s.textContent.includes(l)) s.parentElement.open = true;
  }, label);
  const shot = (n) => page.screenshot({ path: path.join(OUT, n + ".png"), fullPage: true });
  const text = () => page.locator("#app").innerText();
  const waitText = async (s, ms = 20000) => {
    try {
      await page.waitForFunction((t) => document.getElementById("app").innerText.includes(t), s, { timeout: ms });
    } catch (e) {
      await shot("fail");
      const run = await page.locator(".run").allInnerTexts().catch(() => []);
      const toast = await page.locator(".toast").allInnerTexts().catch(() => []);
      console.log("---- waiting for:", s, "\n---- run panel:\n" + run.join("\n") + "\n---- toast: " + toast.join(" | "));
      console.log("---- console errors:", consoleErrors.slice(0, 5));
      throw e;
    }
  };

  console.log("Explorer");
  await page.goto("https://iq.test/#/");
  await waitText("iq-locker");
  check((await text()).includes("iq-snake-game"), "database list renders from the gateway");
  await shot("01-databases");
  await page.click("text=iq-locker");
  await waitText("Official wallet");
  await page.click("text=notes");
  await waitText("first note");
  const t1 = await text();
  check(t1.includes("second <b>note</b>"), "row text is shown escaped (no HTML injection)");
  check(!t1.includes("a visitor wrote this"), "unofficial row hidden under the Official filter");
  await page.click("button:has-text('Unofficial')");
  await waitText("a visitor wrote this");
  check(true, "Unofficial filter shows the visitor's row");
  await shot("02-table-raw");
  await page.fill("#q", "notes");
  await page.press("#q", "Enter");
  await waitText("Search: notes");
  check((await text()).includes("iq-locker / notes") || (await text()).includes("notes"), "search results render");

  console.log("Workspace: new database");
  await page.goto("https://iq.test/#/ws");
  await waitText("New database");
  await page.fill("#newdb", "e2e-parts");
  await page.press("#newdb", "Enter");
  await waitText("Database wallet");
  await waitText("name available");
  check(true, "draft created; name availability checked on chain");
  await page.click("header .wallet button:has-text('Connect wallet')");
  await page.click(".menu button:has-text('Test Wallet')");
  await waitText("Connected");
  await page.click("button:has-text('Create database wallet')");
  await waitText("Database wallet unlocked");
  const dbw = await page.locator(".addrbox .mono").first().innerText();
  // the derived key must match an independent computation
  const { sha256 } = require("@noble/hashes/sha2");
  const msg = Buffer.from(`IQ Tables — unlock database wallet\n\nDatabase: e2e-parts\nKey version: 1\n\nSigning this gives this page the key to the database wallet for "e2e-parts". Only sign it on the IQ Tables portal. It does not move any funds.`);
  const sig = ed25519.sign(msg, walletKeys["Test Wallet"].secretKey.slice(0, 32));
  const seed = sha256(Buffer.concat([Buffer.from("iq-tables/db-wallet/v1"), Buffer.from(sig)]));
  const expectedDbw = Keypair.fromSeed(seed).publicKey.toBase58();
  check(dbw.trim() === expectedDbw, "database wallet = SHA-256(domain ‖ signature), matches independent derivation");
  check((await page.locator("svg.qr").count()) === 1, "donation QR code rendered");
  await page.fill("input[data-arg^='fund:']", "0.5");
  await page.click("button:has-text('Fund from my wallet')");
  await waitText("Funding confirmed");
  check(lam(expectedDbw) === 0.5 * LAMPORTS, "funding transfer (legacy tx signed by the wallet) landed: 0.5 SOL");

  console.log("Workspace: tables and ghost rows");
  await page.fill("input[data-arg^='tname:']", "fasteners");
  await page.selectOption("select[data-arg^='topen:']", "open");
  await page.click("button:has-text('Add table')");
  await waitText("No rows yet");
  // 600-part CSV
  const kinds = ["Hex bolt", "Socket head cap screw", "Flat washer", "Nylon lock nut", "Carriage bolt", "Set screw"];
  const mats = ["18-8 stainless", "316 stainless", "Grade 5 steel", "Grade 8 steel", "Brass"];
  const thr = ["M6x1.0", "M8x1.25", "M10x1.5", "1/4-20 UNC", "3/8-16 UNC"];
  let x = 2463534242;
  const rnd = (n) => { x ^= x << 13; x >>>= 0; x ^= x >>> 17; x ^= x << 5; x >>>= 0; return x % n; };
  const sups = ["Brazos Bolt & Nut, LLC", "Lone Star Fastener Co.", "Gulf Coast Supply", "Permian Industrial"];
  let csv = "part_no,name,material,thread,length_mm,qty,unit_price,supplier,updated\n";
  for (let i = 0; i < 600; i++) {
    const k = kinds[rnd(6)], m = mats[rnd(5)], t = thr[rnd(5)], len = [10, 16, 20, 25, 30, 40, 50][rnd(7)];
    csv += `FST-${String(1000 + i).padStart(6, "0")},"${k} ${t} x ${len}mm",${m},${t},${len},${rnd(5000)},${rnd(40)}.${String(rnd(100)).padStart(2, "0")},"${sups[rnd(4)]}",${1790000000000 + rnd(900000000) * 100}\n`;
  }
  await openDetails("Import CSV / JSON");
  await page.fill("textarea[data-arg^='csv:']", csv);
  await page.click("button:has-text('Import as ghost rows')");
  await waitText("Imported 600 ghost rows");
  const t2 = await text();
  const packMatch = t2.match(/600 ghost row\(s\) → (\d+) pack\(s\)/);
  check(!!packMatch, "pack plan shown: " + (packMatch ? packMatch[0] : "missing"));
  const nPacks = packMatch ? Number(packMatch[1]) : 0;
  check(nPacks > 1 && nPacks <= 12, `600 records fit in ${nPacks} inscriptions`);
  // edit a ghost cell, and add a second (locked) table
  await page.fill("#c-" + (await page.locator("input[id^='c-']").first().getAttribute("id")).slice(2), "FST-EDITED");
  await page.keyboard.press("Tab");
  await openDetails("New table");
  await page.fill("input[data-arg^='tname:']", "suppliers");
  await page.fill("input[data-arg^='tcols:']", "name, city, state");
  await page.click("button:has-text('Add table')");
  await page.click("button:has-text('+ Row')");
  const cells = page.locator("table.edit input");
  await cells.nth(0).fill("Brazos Bolt & Nut"); await cells.nth(0).press("Tab");
  await cells.nth(1).fill("Waco"); await cells.nth(1).press("Tab");
  await cells.nth(2).fill("TX"); await cells.nth(2).press("Tab");
  await shot("03-draft");

  console.log("Inscribe");
  await page.click("button:has-text('Inscribe')");
  await waitText("Done. Spent", 60000);
  const t3 = await text();
  check(!t3.includes("Simulation rejected") && !t3.includes("failed"), "inscription finished without errors");
  const rootPda = iq.contract.getDbRootPda(Buffer.from("e2e-parts"), PID).toBase58();
  const root = accCoder.decode("DbRoot", acct(rootPda).data);
  check(root.creator.toBase58() === expectedDbw, "DbRoot created with the database wallet as creator (official signer)");
  check(root.table_creators.length === 1 && root.table_creators[0].toBase58() === expectedDbw, "table creation locked to the database wallet");
  const fastPda = iq.contract.getTablePda(new PublicKey(rootPda), iq.utils.toSeedBytes("fasteners"), PID).toBase58();
  const supPda = iq.contract.getTablePda(new PublicKey(rootPda), iq.utils.toSeedBytes("suppliers"), PID).toBase58();
  const fastMeta = accCoder.decode("Table", acct(fastPda).data);
  const supMeta = accCoder.decode("Table", acct(supPda).data);
  check(fastMeta.writers.length === 0, "open table: no writer restriction");
  check(supMeta.writers.length === 1 && supMeta.writers[0].toBase58() === expectedDbw, "locked table: writers = [database wallet]");
  check((chain.rows.get(fastPda) || []).length === nPacks, `${nPacks} pack rows written to "fasteners"`);
  check(chain.txCount.v1 > 0, `v1 transactions used (${chain.txCount.v1} v1, ${chain.txCount.legacy} legacy)`);
  check((chain.reallocs || 0) >= 1, "DbRoot realloc path exercised (" + (chain.reallocs || 0) + ")");
  check(chain.notifies.length === nPacks + 1, "gateway notified for every pack (" + chain.notifies.length + ")");
  console.log("   instruction mix:", JSON.stringify(chain.ixSeen));
  console.log("   db wallet balance after:", lam(expectedDbw) / LAMPORTS, "SOL");
  await shot("04-inscribed");

  console.log("Explorer reads it back");
  await page.click("a:has-text('view on chain')");
  await waitText("record(s)");
  check((await text()).includes("Brazos Bolt & Nut") && (await text()).includes("Waco"), "locked table's record reads back");
  await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
  try {
    await page.waitForFunction(() => /showing 600 record/.test(document.getElementById("app").innerText), null, { timeout: 20000 });
  } catch (e) { await shot("fail"); console.log((await text()).slice(0, 1500)); throw e; }
  const t4 = await text();
  check(t4.includes("FST-EDITED"), "edited ghost value came back from the chain");
  check(t4.includes("IQT packed"), "table recognised as IQT-packed");
  await shot("05-records");
  await page.fill("#tvq", "FST-001123");
  await page.waitForFunction(() => /showing 1 record/.test(document.getElementById("app").innerText), null, { timeout: 5000 });
  check(true, "row filter narrows to one record");
  await page.click("td:has-text('FST-001123')");
  await page.click("button:has-text('Edit in workspace')");
  await waitText("Record copied into a ghost row");
  const ghostInputs = page.locator("tr.ghost input");
  await ghostInputs.nth(5).fill("9999");
  await ghostInputs.nth(5).press("Tab");
  await page.click("button:has-text('Inscribe')");
  await waitText("Done. Spent", 60000);
  await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
  await page.fill("#tvq", "FST-001123");
  await page.waitForFunction(() => /showing 1 record/.test(document.getElementById("app").innerText), null, { timeout: 20000 });
  await page.click("td:has-text('FST-001123')");
  const t5 = await text();
  check(t5.includes("9999") && t5.includes("2 versions"), "update inscribed as a new pack; latest version wins (2 versions)");

  console.log("Unofficial contribution from another wallet");
  await page.click("header .wallet button");
  await page.click(".menu button:has-text('Disconnect')");
  await page.goto("https://iq.test/#/ws");
  await page.fill("#newdb", "e2e-parts");
  await page.press("#newdb", "Enter");
  await waitText("Taken");
  check(true, "name check reports the database as taken by its owner");
  await page.click("header .wallet button:has-text('Connect wallet')");
  await page.click(".menu button:has-text('Other Wallet')");
  await waitText("Connected");
  await page.click("button:has-text('Create database wallet')");
  await waitText("Database wallet unlocked");
  await page.fill("input[data-arg^='fund:']", "0.2");
  await page.click("button:has-text('Fund from my wallet')");
  await waitText("Funding confirmed");
  await page.fill("input[data-arg^='tname:']", "fasteners");
  await page.fill("input[data-arg^='tcols:']", "part_no, name, note");
  await page.click("button:has-text('Add table')");
  await page.click("button:has-text('+ Row')");
  const c2 = page.locator("table.edit input");
  await c2.nth(0).fill("FST-900001"); await c2.nth(0).press("Tab");
  await c2.nth(1).fill("Community-submitted washer"); await c2.nth(1).press("Tab");
  await page.click("button:has-text('Inscribe')");
  await waitText("Done. Spent", 60000);
  check((await text()).includes("unofficial contributions"), "contributor mode explained in the run log");
  // try the locked table too: must be refused by the (mock) program before anything is sent
  await page.fill("input[data-arg^='tname:']", "suppliers");
  await page.fill("input[data-arg^='tcols:']", "name, city, state");
  await page.click("button:has-text('Add table')");
  await page.click("button:has-text('+ Row')");
  const c3 = page.locator("table.edit input");
  await c3.nth(0).fill("Spam Co"); await c3.nth(0).press("Tab");
  const supBefore = (chain.rows.get(supPda) || []).length;
  await page.click("button:has-text('Inscribe')");
  await waitText("Simulation rejected", 30000);
  check((chain.rows.get(supPda) || []).length === supBefore, "write to a locked table is rejected at simulation; nothing sent");
  await shot("06-contributor");
  await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
  await waitText("Unofficial");
  await page.click("button:has-text('Unofficial')");
  await waitText("Community-submitted washer");
  const t6 = await text();
  check(/Unofficial 1/.test(t6.replace(/\s+/g, " ")), "explorer counts 1 unofficial record");
  check(/Official 600/.test(t6.replace(/\s+/g, " ")), "official records unaffected (600)");
  await shot("07-unofficial");

  console.log("Mobile layout");
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("https://iq.test/#/");
  await waitText("iq-locker");
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  check(overflow <= 1, "no horizontal page scroll at 390px (" + overflow + ")");
  await shot("08-mobile");

  check(consoleErrors.length === 0, "no console errors" + (consoleErrors.length ? ": " + consoleErrors.slice(0, 3).join(" | ") : ""));
  await browser.close();
  const failed = results.filter((r) => !r[0]).length;
  console.log(`\n${results.length - failed}/${results.length} checks passed`);
})().catch((e) => { console.error("E2E crashed:", e); process.exit(1); });
