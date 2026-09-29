// End-to-end test: the built single-file app in headless Chromium against
//  * a mock Solana chain that decodes every IQ instruction with the official
//    IDL coder, checks accounts/data against the SDK's own instruction builder,
//    verifies Ed25519 signatures (legacy and v1 wire formats) and applies the
//    effects (DbRoot, tables, writer locks, rows, fees);
//  * a mock IQ gateway serving rows, files and file listings from that chain.
// Logs in the way users do: an account file (created, saved, dropped on the
// page, unlocked with its passphrase) plus imported Solana CLI / base58 keys.
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
const TABLE_FEE = 930_000; // measured against the deployed program on devnet

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
  txs: new Map(), // sig -> { raw (base64), blockTime }
  addrSigs: new Map(), // address -> [sig] oldest first
  files: new Map(), // sig -> { metadata, signer, blockTime }
  assets: new Map(), // wallet -> [asset] oldest first
  batches: 0,
};
const FEATURE = "Feature111111111111111111111111111111111111";
const V1_GATE = "txv1aq4pp281K9um3tnPgkfX8UqtFT6wcVW3hNezGLL";
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
    return { sig: bs58.encode(raw.subarray(msgEnd, msgEnd + 64)), feePayer: keys[0], signers: keys.slice(0, nsig), instructions, nsig, blockhash, allKeys: keys.map((k) => k.toBase58()) };
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
    allKeys: msg.accountKeys.map((k) => k.toBase58()),
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
      const invLen = st.accounts.get(iq.contract.getUserInventoryPda(signer, PID).toBase58()).data.length;
      if (Buffer.byteLength(a.metadata) > 700 && invLen < 4213) throw new Error("custom program error: AccountDataTooSmall (pre-upgrade user_inventory, needs realloc)");
      const md = JSON.parse(a.metadata);
      if (md.total_chunks !== 1 || md.method !== 0) throw new Error("bad metadata envelope");
      const row = JSON.parse(md.data);
      const cols = new Set([...meta.column_names.map((c) => Buffer.from(c).toString()), Buffer.from(meta.id_col).toString()]);
      for (const k of Object.keys(row)) if (!cols.has(k)) throw new Error("custom program error: 0x1787 SchemaMismatch " + k);
      debit(st, signer.toBase58(), 1_000_000, "write fee");
      credit(st, FEE_RECEIVER, 1_000_000);
      st.pendingRows = st.pendingRows || [];
      st.pendingRows.push({ table: tpda.toBase58(), row, signer: signer.toBase58() });
    } else if (dec.name === "user_inventory_code_in") {
      const inv = iq.contract.getUserInventoryPda(signer, PID);
      sameIx(ix, iq.contract.userInventoryCodeInInstruction(builder, {
        user: signer, user_inventory: inv, system_program: new PublicKey(SYS), receiver: new PublicKey(FEE_RECEIVER), session: undefined, iq_ata: undefined,
      }, a), dec.name);
      const invAcc = st.accounts.get(inv.toBase58());
      if (!invAcc) throw new Error("custom program error: user not initialized");
      if (a.on_chain_path !== "" || a.session) throw new Error("expected an inline file");
      const bytes = Buffer.byteLength(a.metadata);
      if (bytes > 3400) throw new Error("metadata over the inline cap");
      if (bytes > 700 && invAcc.data.length < 4213) throw new Error("custom program error: AccountDataTooSmall");
      const md = JSON.parse(a.metadata);
      if (md.total_chunks !== 1 || md.method !== 0 || typeof md.data !== "string" || !md.filename || !md.filetype) throw new Error("bad file metadata envelope");
      debit(st, signer.toBase58(), 1_000_000, "write fee");
      credit(st, FEE_RECEIVER, 1_000_000);
      st.pendingFiles = st.pendingFiles || [];
      st.pendingFiles.push({ signer: signer.toBase58(), metadata: a.metadata });
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
    const bt = ++blockTime;
    for (const p of st.pendingRows || []) {
      const list = chain.rows.get(p.table) || [];
      list.push({ ...p.row, __txSignature: tx.sig, __signer: p.signer, __blockTime: bt });
      chain.rows.set(p.table, list);
    }
    for (const f of st.pendingFiles || []) {
      chain.files.set(tx.sig, { ...f, blockTime: bt });
      const list = chain.assets.get(f.signer) || [];
      list.push({ signature: tx.sig, slot: 1000, err: null, memo: null, blockTime: bt, confirmationStatus: "finalized", onChainPath: "", metadata: f.metadata });
      chain.assets.set(f.signer, list);
    }
    chain.txs.set(tx.sig, { raw: Buffer.from(raw).toString("base64"), blockTime: bt });
    for (const k of new Set(tx.allKeys)) {
      const l = chain.addrSigs.get(k) || [];
      l.push(tx.sig);
      chain.addrSigs.set(k, l);
    }
  }
  chain.sigs.set(tx.sig, { err });
  return { tx, err, logs };
}

function rpc(body) {
  const parsed = JSON.parse(body);
  if (Array.isArray(parsed)) {
    chain.batches++;
    return parsed.map((p) => rpcOne(p));
  }
  return rpcOne(parsed);
}

function rpcOne({ method, params, id }) {
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
    case "getProgramAccounts": {
      if (params[0] !== PID.toBase58()) return ok([]);
      const filters = (params[1] && params[1].filters) || [];
      const out = [];
      for (const [k, a] of chain.accounts) {
        if (a.owner !== PID.toBase58()) continue;
        const pass = filters.every((f) => {
          if (!f.memcmp) return true;
          const want = Buffer.from(bs58.decode(f.memcmp.bytes));
          return a.data.subarray(f.memcmp.offset, f.memcmp.offset + want.length).equals(want);
        });
        if (pass) out.push({ pubkey: k, account: info(k) });
      }
      return ok(out);
    }
    case "getSignaturesForAddress": {
      const all = [...(chain.addrSigs.get(params[0]) || [])].reverse();
      const cfg = params[1] || {};
      let start = 0;
      if (cfg.before) start = all.indexOf(cfg.before) + 1;
      return ok(all.slice(start, start + (cfg.limit || 1000)).map((s) => ({ signature: s, slot: 1000, err: chain.sigs.get(s) && chain.sigs.get(s).err ? {} : null, memo: null, blockTime: (chain.txs.get(s) || {}).blockTime, confirmationStatus: "confirmed" })));
    }
    case "getTransaction": {
      const t = chain.txs.get(params[0]);
      if (!t) return ok(null);
      const raw = Buffer.from(t.raw, "base64");
      return ok({ slot: 1000, blockTime: t.blockTime, version: raw[0] === 129 ? 1 : "legacy", meta: { err: null, fee: 5000 }, transaction: [t.raw, "base64"] });
    }
    case "requestAirdrop": {
      credit(chain, params[0], params[1]);
      const sig = bs58.encode(Buffer.from(Array.from({ length: 64 }, () => Math.random() * 256 | 0)));
      chain.sigs.set(sig, { err: null });
      chain.airdrops = (chain.airdrops || 0) + 1;
      return ok(sig);
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
  m = p.match(/^\/data\/([^/]+)$/);
  if (m) {
    const f = chain.files.get(m[1]);
    if (!f) return { error: "not found" };
    const md = JSON.parse(f.metadata);
    const data = md.data;
    delete md.data;
    return { data, metadata: JSON.stringify(md), signature: m[1], signer: f.signer, blockTime: f.blockTime, slot: 1000 };
  }
  m = p.match(/^\/user\/([^/]+)\/assets$/);
  if (m) return [...(chain.assets.get(m[1]) || [])].reverse();
  m = p.match(/^\/table\/([^/]+)\/notify$/);
  if (m && method === "POST") { chain.notifies.push({ table: m[1], body: JSON.parse(body) }); return { ok: true }; }
  return { error: "mock: no route " + p };
}

// -------------------------------------------------------------------- keys
const { sha256 } = require("@noble/hashes/sha2");
// "funder": a Solana CLI keypair file the user imports; "other": a base58 key a
// second user pastes. Both start with 10 SOL. "other" also has IQ accounts from
// the pre-upgrade program (900 bytes), so its first v1 write must grow them.
const funderKp = Keypair.fromSeed(Buffer.alloc(32, 7));
const otherKp = Keypair.fromSeed(Buffer.alloc(32, 9));
for (const kp of [funderKp, otherKp]) setAcct(kp.publicKey.toBase58(), { lamports: 10 * LAMPORTS, data: Buffer.alloc(0), owner: SYS });
{
  const o = otherKp.publicKey;
  setAcct(iq.contract.getUserInventoryPda(o, PID).toBase58(), { lamports: rent(900), data: Buffer.alloc(900), owner: PID.toBase58() });
  setAcct(iq.contract.getCodeAccountPda(o, PID).toBase58(), { lamports: rent(900), data: Buffer.alloc(900), owner: PID.toBase58() });
  const us = accCoder.encode("UserState", { owner: o, trail_anchor: Buffer.alloc(0), metadata: Buffer.alloc(0), total_session_files: new anchor.BN(3) });
  setAcct(iq.contract.getUserPda(o, PID).toBase58(), { lamports: rent(us.length), data: us, owner: PID.toBase58() });
}
// v1 transactions are live on the mock cluster (the feature gate the SDK checks)
setAcct(V1_GATE, { lamports: 1, data: Buffer.from([1, 0x40, 0x42, 0x0f, 0, 0, 0, 0, 0]), owner: FEATURE });

const PASS = "correct horse battery staple";
const u32le = (i) => { const b = Buffer.alloc(4); b.writeUInt32LE(i); return b; };

// -------------------------------------------------------------------- run
(async () => {
  const html = fs.readFileSync(path.join(ROOT, "site", "index.html"), "utf8");
  // CHROME_PATH: any Chromium/Chrome binary; otherwise Playwright's installed browser.
  const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH || undefined, args: ["--no-sandbox"] });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, acceptDownloads: true });
  const page = await ctx.newPage();
  const consoleErrors = [];
  page.on("console", (m) => { if (m.type() === "error") consoleErrors.push(m.text()); });
  page.on("pageerror", (e) => consoleErrors.push("pageerror: " + e.message));
  const rpcRoute = async (route, req) => {
    if (req.method() === "OPTIONS") return route.fulfill({ status: 204, headers: { "access-control-allow-origin": "*", "access-control-allow-headers": "content-type" } });
    let res;
    try { res = rpc(req.postData()); } catch (e) { res = { jsonrpc: "2.0", id: 1, error: { code: -1, message: "mock crashed: " + e.message } }; }
    return route.fulfill({ status: 200, contentType: "application/json", headers: { "access-control-allow-origin": "*" }, body: JSON.stringify(res) });
  };
  await page.route("**/*", async (route) => {
    const req = route.request();
    const url = req.url();
    if (url.startsWith("https://iq.test/")) return route.fulfill({ status: 200, contentType: "text/html", body: html });
    if (url.startsWith("https://gateway.iqlabs.dev/")) {
      const res = gateway(url, req.method(), req.postData());
      return route.fulfill({ status: res.error ? 404 : 200, contentType: "application/json", headers: { "access-control-allow-origin": "*" }, body: JSON.stringify(res) });
    }
    if (url.startsWith("https://api.mainnet-beta.solana.com") || url.startsWith("https://api.devnet.solana.com")) return rpcRoute(route, req);
    return route.fulfill({ status: 404, body: "blocked in test: " + url });
  });
  const openDetails = (label) => page.evaluate((l) => {
    for (const s of document.querySelectorAll("details > summary")) if (s.textContent.includes(l)) s.parentElement.open = true;
  }, label);
  const shot = (n) => page.screenshot({ path: path.join(OUT, n + ".png"), fullPage: true });
  const text = () => page.locator("#app").innerText();
  const toast = () => page.locator(".toast").innerText().catch(() => "");
  const waitText = async (s, ms = 20000) => {
    try {
      await page.waitForFunction((t) => document.getElementById("app").innerText.includes(t), s, { timeout: ms });
    } catch (e) {
      await shot("fail");
      const run = await page.locator(".run").allInnerTexts().catch(() => []);
      console.log("---- waiting for:", s, "\n---- run panel:\n" + run.join("\n") + "\n---- toast: " + (await toast()));
      console.log("---- console errors:", consoleErrors.slice(0, 5));
      throw e;
    }
  };
  const download = async (fn) => {
    const [d] = await Promise.all([page.waitForEvent("download"), fn()]);
    return { name: d.suggestedFilename(), text: fs.readFileSync(await d.path(), "utf8") };
  };
  const dropFile = async (name, content) => {
    const dt = await page.evaluateHandle(([n, c]) => {
      const dt = new DataTransfer();
      dt.items.add(new File([c], n, { type: "application/json" }));
      return dt;
    }, [name, content]);
    await page.dispatchEvent("main", "dragenter", { dataTransfer: dt });
    const overlay = await page.evaluate(() => document.body.classList.contains("dropping"));
    await page.dispatchEvent("main", "drop", { dataTransfer: dt });
    return overlay;
  };
  const menu = async (item) => {
    await page.click("header .wallet button");
    await page.click(`.menu :text('${item}')`);
  };
  const addrOf = (label) => page.locator(`tr:has(input[data-in='wallet-label'][value='${label}']) .addr`).first().getAttribute("title");

  console.log("Explorer");
  await page.goto("https://iq.test/#/");
  await waitText("iq-locker");
  check((await text()).includes("iq-snake-game"), "database list renders from IQ's gateway");
  await shot("01-databases");
  await page.click("text=iq-locker");
  await waitText("Official wallet");
  await page.click("a:has-text('notes')");
  await waitText("first note");
  const t1 = await text();
  check(t1.includes("second <b>note</b>"), "row text is shown escaped (no HTML injection)");
  check(!t1.includes("a visitor wrote this"), "unofficial row hidden under the Official filter");
  await page.click("button:has-text('Unofficial')");
  await waitText("a visitor wrote this");
  check(true, "Unofficial filter shows the visitor's row");
  await page.fill("#q", "notes");
  await page.press("#q", "Enter");
  await waitText("Search: notes");
  check((await text()).includes("notes"), "search results render");

  console.log("Account: create, import a key file, save");
  await page.goto("https://iq.test/#/mine");
  await waitText("Log in to see");
  await page.click("header a:has-text('Log in')");
  await waitText("New account");
  await page.fill("#acct-name", "e2e");
  await page.fill("#pass1", PASS);
  await page.fill("#pass2", PASS);
  const f1 = await download(() => page.click("button:has-text('Create account')"));
  check(f1.name === "e2e.iqaccount.json", "account file downloaded: " + f1.name);
  const file1 = JSON.parse(f1.text);
  check(file1.format === "iq-tables-account" && file1.ciphertext && !f1.text.includes("master"), "account file is encrypted (no secrets in the clear)");
  const plain = JSON.parse(Buffer.from(await iq.crypto.passwordDecrypt(PASS, file1.salt, file1.iv, file1.ciphertext)).toString());
  check(typeof plain.master === "string" && plain.master.length === 64, "the IQ SDK's own passwordDecrypt opens the account file");
  const master = Buffer.from(plain.master, "hex");
  const derive = (i) => Keypair.fromSeed(sha256(Buffer.concat([Buffer.from("iq-tables/account/v1/wallet"), master, u32le(i)])));
  await waitText("Databases you own");
  check((await page.evaluate(() => location.hash)) === "#/mine", "logged in and taken to My tables");
  await page.goto("https://iq.test/#/account");
  await waitText("Wallets");
  check((await addrOf("Main")) === derive(0).publicKey.toBase58(), "Main wallet = SHA-256(domain ‖ master ‖ 0), matches independent derivation");
  await page.setInputFiles("input[data-file='import-keys']", { name: "funder.json", mimeType: "application/json", buffer: Buffer.from(JSON.stringify(Array.from(funderKp.secretKey))) });
  await waitText("Imported 1 new wallet");
  check((await addrOf("funder")) === funderKp.publicKey.toBase58(), "Solana CLI keypair file imported as wallet \"funder\"");
  await waitText("10.0000 SOL");
  check(true, "balances read for every wallet");
  await menu("Log out");
  check((await toast()).includes("aren't saved"), "logout refused while imported keys are unsaved");
  const f2 = await download(() => page.click(".warnbox button:has-text('Save account file')"));
  const plain2 = JSON.parse(Buffer.from(await iq.crypto.passwordDecrypt(PASS, JSON.parse(f2.text).salt, JSON.parse(f2.text).iv, JSON.parse(f2.text).ciphertext)).toString());
  check(plain2.wallets.some((w) => w.label === "funder" && w.secret === bs58.encode(funderKp.secretKey)), "saved file carries the imported key");
  await menu("Log out");
  await waitText("Logged out");

  console.log("Account: log in by dropping the file");
  const overlay = await dropFile("e2e.iqaccount.json", f2.text);
  check(overlay, "drop overlay shows while dragging a file over the page");
  await waitText("enter its passphrase");
  await page.fill("#unlock-pass", "wrong passphrase!");
  await page.press("#unlock-pass", "Enter");
  await waitText("Wrong passphrase");
  check(true, "wrong passphrase rejected");
  await page.fill("#unlock-pass", PASS);
  await page.press("#unlock-pass", "Enter");
  await waitText("Databases you own");
  check((await page.locator("header .wallet button").innerText()).includes("e2e"), "logged in to every wallet in the file");

  console.log("Account: a wallet made after the last save is recovered");
  await page.goto("https://iq.test/#/account");
  await waitText("Wallets");
  await page.fill("#wlabel", "spare");
  await page.press("#wlabel", "Enter");
  await waitText("New wallet \"spare\"");
  const spare = derive(1).publicKey.toBase58();
  check((await addrOf("spare")) === spare, "new wallet derived at index 1");
  const fa = funderKp.publicKey.toBase58();
  await openDetails("Send");
  await page.selectOption(`select[data-arg='to:${fa}']`, spare);
  await page.fill(`#amt-${fa}`, "0.25");
  await page.click(`button[data-a='transfer'][data-arg='${fa}']`);
  await waitText("Transfer confirmed");
  check(lam(spare) === 0.25 * LAMPORTS, "moved 0.25 SOL between two wallets of the account");
  await menu("Log out");
  await waitText("Logged out");
  await dropFile("e2e.iqaccount.json", f2.text); // the older file, without "spare"
  await waitText("enter its passphrase");
  await page.fill("#unlock-pass", PASS);
  await page.press("#unlock-pass", "Enter");
  await waitText("Recovered 1 wallet");
  check(true, "login rescan recovered the wallet created after the file was saved");
  await page.goto("https://iq.test/#/account");
  await waitText("Wallets");
  check((await addrOf("Recovered #1")) === spare, "recovered wallet has the same address");
  await page.check("input[data-in='remember']");
  await page.reload();
  await page.goto("https://iq.test/#/account");
  await waitText("Welcome back");
  await page.fill("#unlock-pass", PASS);
  await page.press("#unlock-pass", "Enter");
  await waitText("Databases you own");
  check(true, "remembered account unlocks with just the passphrase after a reload");
  await shot("02-account");

  console.log("Workspace: new database with its own wallet");
  await page.goto("https://iq.test/#/ws");
  await waitText("New database");
  await page.fill("#newdb", "e2e-parts");
  await page.press("#newdb", "Enter");
  await waitText("name available");
  await page.click("button:has-text('Create a dedicated wallet')");
  await waitText("Created dedicated wallet");
  const dbw = (await page.locator(".addrbox .mono").first().innerText()).trim();
  check(dbw === derive(2).publicKey.toBase58(), "dedicated database wallet derived from the account (index 2)");
  check((await page.locator("svg.qr").count()) === 1, "donation QR code rendered");
  const dkey = (await page.evaluate(() => location.hash)).split("/")[2];
  await page.selectOption(`select[data-arg='ffrom:${dkey}']`, fa);
  await page.fill(`#famt-${dkey}`, "0.5");
  await page.click("button:has-text('Move SOL')");
  await waitText("Transfer confirmed");
  check(lam(dbw) === 0.5 * LAMPORTS, "funded the database wallet from another account wallet: 0.5 SOL");

  console.log("Workspace: tables, ghost rows, links and a file");
  const rootPda = iq.contract.getDbRootPda(Buffer.from("e2e-parts"), PID).toBase58();
  const fastPda = iq.contract.getTablePda(new PublicKey(rootPda), iq.utils.toSeedBytes("fasteners"), PID).toBase58();
  const supPda = iq.contract.getTablePda(new PublicKey(rootPda), iq.utils.toSeedBytes("suppliers"), PID).toBase58();
  await page.fill("input[data-arg^='tname:']", "fasteners");
  await page.selectOption("select[data-arg^='topen:']", "open");
  await page.click("button:has-text('Add table')");
  await waitText("No rows yet");
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
  await page.fill("#" + (await page.locator("input[id^='c-']").first().getAttribute("id")), "FST-EDITED");
  await page.keyboard.press("Tab");
  await openDetails("New table");
  await page.fill("input[data-arg^='tname:']", "suppliers");
  await page.fill("input[data-arg^='tcols:']", "name, city, state, website, catalog, spec_file");
  await page.click("button:has-text('Add table')");
  await page.click("button:has-text('+ Row')");
  const cells = page.locator("table.edit input[data-in='cell']");
  const vals = ["Brazos Bolt & Nut", "Waco", "TX", "https://brazosbolt.example/catalog", `iq://table/${fastPda}/FST-001001`];
  for (let i = 0; i < vals.length; i++) { await cells.nth(i).fill(vals[i]); await cells.nth(i).press("Tab"); }
  check((await page.locator("select[data-in='attach-col']").inputValue()) === "spec_file", "file column picked automatically (spec_file)");
  const spec = "Torque spec, dry threads\nM6 grade 8.8: 10 N·m\nM8 grade 8.8: 25 N·m\nM10 grade 8.8: 49 N·m\n";
  await page.setInputFiles("input[data-fileb64='attach']", { name: "torque-spec.txt", mimeType: "text/plain", buffer: Buffer.from(spec) });
  await waitText("is inscribed and linked", 30000);
  check(!(await text()).includes("one-time IQ account setup"), "cost estimate drops the wallet setup once an attachment has done it");
  check(chain.ixSeen.user_inventory_code_in === 1 && chain.ixSeen.user_initialize === 1, "file inscribed with user_inventory_code_in (after the wallet's one-time setup), checked against the SDK builder");
  const fileSig = [...chain.files.keys()][0];
  check((await cells.nth(5).inputValue()) === `iq://tx/${fileSig}#torque-spec.txt`, "cell holds the file's iq://tx link");
  check(JSON.parse(chain.files.get(fileSig).metadata).data === spec, "file stored as text, exactly like the SDK's codeIn");
  await shot("03-draft");

  console.log("Inscribe");
  await page.click("button:has-text('Inscribe')");
  await waitText("Done. Spent", 60000);
  const t3 = await text();
  check(!t3.includes("Simulation rejected") && !t3.includes("failed"), "inscription finished without errors");
  const root = accCoder.decode("DbRoot", acct(rootPda).data);
  check(root.creator.toBase58() === dbw, "DbRoot created with the database wallet as creator (official signer)");
  check(root.table_creators.length === 1 && root.table_creators[0].toBase58() === dbw, "table creation locked to the database wallet");
  const fastMeta = accCoder.decode("Table", acct(fastPda).data);
  const supMeta = accCoder.decode("Table", acct(supPda).data);
  check(fastMeta.writers.length === 0, "open table: no writer restriction");
  check(supMeta.writers.length === 1 && supMeta.writers[0].toBase58() === dbw, "locked table: writers = [database wallet]");
  check((chain.rows.get(fastPda) || []).length === nPacks, `${nPacks} pack rows written to "fasteners"`);
  check(chain.txCount.v1 > 0, `v1 transactions used once the feature gate is on (${chain.txCount.v1} v1, ${chain.txCount.legacy} legacy)`);
  check((chain.reallocs || 0) >= 1, "DbRoot realloc path exercised (" + (chain.reallocs || 0) + ")");
  check(chain.notifies.length === nPacks + 1, "IQ gateway notified for every pack (" + chain.notifies.length + ")");
  console.log("   instruction mix:", JSON.stringify(chain.ixSeen));
  await shot("04-inscribed");

  console.log("Explorer: links and files");
  await page.click("a:has-text('view on chain')");
  await page.waitForSelector("table.data td:has-text('Brazos Bolt & Nut')");
  const web = page.locator("table.data a[href='https://brazosbolt.example/catalog']");
  check((await web.count()) === 1 && (await web.getAttribute("target")) === "_blank", "web link in a cell opens in a new tab");
  await waitText("e2e-parts › fasteners › FST-001001");
  check(true, "iq://table link shows the database › table › record it points to");
  await page.click("button[data-a='open-tx']:has-text('torque-spec.txt')");
  await waitText("M8 grade 8.8: 25 N·m");
  check((await text()).includes("read from IQ gateway"), "file opens in the viewer, read through IQ's gateway /data");
  await shot("05b-viewer");
  const dl = await download(() => page.click("button:has-text('Download')"));
  check(dl.name === "torque-spec.txt" && dl.text === spec, "file downloads byte-for-byte");
  await page.click("button[data-a='viewer-close']");
  await page.click("a:has-text('e2e-parts › fasteners › FST-001001')");
  await waitText("Copy record link");
  check(/showing 1 record/.test(await text()), "record link opens that one record");
  await page.click("button:has-text('Copy record link')");
  check((await toast()).includes(`iq://table/${fastPda}/FST-001001`), "record link copied: iq://table/<table>/<id>");

  console.log("Explorer reads the rest back");
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
  await page.click("td:has-text('FST-001123')");
  await page.click("button:has-text('Edit in workspace')");
  await waitText("Record copied into a ghost row");
  const ghostInputs = page.locator("tr.ghost input[data-in='cell']");
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

  console.log("My tables");
  await page.goto("https://iq.test/#/mine");
  await waitText("torque-spec.txt");
  const tm = await text();
  check(/Databases you own[\s\S]*e2e-parts/.test(tm), "My tables lists the database the account's wallet owns");
  check(tm.includes("fasteners") && tm.includes("suppliers"), "…with its tables");
  check(/Files[\s\S]*torque-spec\.txt/.test(tm), "…and the files its wallets inscribed (IQ gateway /user/<wallet>/assets)");
  check(/Drafts in this browser[\s\S]*e2e-parts/.test(tm), "…and the drafts in this browser");
  await shot("06-mine");

  console.log("Unofficial contribution from a second account");
  await menu("Log out");
  await waitText("Logged out");
  await page.goto("https://iq.test/#/account");
  await waitText("Welcome back");
  await page.click("button:has-text('Forget it on this device')");
  await waitText("New account");
  await page.fill("#acct-name", "visitor");
  await page.fill("#pass1", PASS + "!");
  await page.fill("#pass2", PASS + "!");
  await download(() => page.click("button:has-text('Create account')"));
  await waitText("Databases you own");
  await page.goto("https://iq.test/#/account");
  await waitText("Wallets");
  await openDetails("Import existing keys");
  await page.fill("#keys-text", `other: ${bs58.encode(otherKp.secretKey)}`);
  await page.click("button:has-text('Import pasted keys')");
  await waitText("Imported 1 new wallet");
  const oa = otherKp.publicKey.toBase58();
  check((await addrOf("other")) === oa, "pasted \"label: base58-key\" imported");
  await page.goto("https://iq.test/#/ws");
  await page.fill("#newdb", "e2e-parts");
  await page.press("#newdb", "Enter");
  await waitText("Taken");
  check(true, "name check reports the database as taken by its owner");
  await page.selectOption("select[data-in='draft-wallet']", oa);
  await waitText("signs everything for");
  await page.fill("input[data-arg^='tname:']", "fasteners");
  await page.fill("input[data-arg^='tcols:']", "part_no, name, note");
  await page.click("button:has-text('Add table')");
  await page.click("button:has-text('+ Row')");
  const c2 = page.locator("table.edit input[data-in='cell']");
  await c2.nth(0).fill("FST-900001"); await c2.nth(0).press("Tab");
  await c2.nth(1).fill("Community-submitted washer"); await c2.nth(1).press("Tab");
  const reallocsBefore = chain.reallocs || 0;
  await page.click("button:has-text('Inscribe')");
  await waitText("Done. Spent", 60000);
  check((await text()).includes("unofficial contributions"), "contributor mode explained in the run log");
  check((chain.reallocs || 0) >= reallocsBefore + 2 && acct(iq.contract.getUserInventoryPda(otherKp.publicKey, PID).toBase58()).data.length === 4213, "pre-upgrade IQ accounts grown before the first v1 write (like the SDK)");
  await page.fill("input[data-arg^='tname:']", "suppliers");
  await page.fill("input[data-arg^='tcols:']", "name, city, state");
  await page.click("button:has-text('Add table')");
  await page.click("button:has-text('+ Row')");
  const c3 = page.locator("table.edit input[data-in='cell']");
  await c3.nth(0).fill("Spam Co"); await c3.nth(0).press("Tab");
  const supBefore = (chain.rows.get(supPda) || []).length;
  await page.click("button:has-text('Inscribe')");
  await waitText("Simulation rejected", 30000);
  check((chain.rows.get(supPda) || []).length === supBefore, "write to a locked table is rejected at simulation; nothing sent");
  await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
  await waitText("Unofficial");
  await page.click("button:has-text('Unofficial')");
  await waitText("Community-submitted washer");
  const t6 = (await text()).replace(/\s+/g, " ");
  check(/Unofficial 1/.test(t6), "explorer counts 1 unofficial record");
  check(/Official 600/.test(t6), "official records unaffected (600)");
  await shot("07-unofficial");

  console.log("Reading straight from Solana (no gateway)");
  await page.goto("https://iq.test/#/settings");
  await page.selectOption("select[data-arg='source']", "rpc");
  await page.goto("https://iq.test/#/");
  await waitText("e2e-parts");
  check((await text()).includes("Source: Solana RPC"), "database list read with getProgramAccounts");
  await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
  await page.waitForFunction(() => /Official\s*600/.test(document.getElementById("app").innerText), null, { timeout: 20000 });
  await page.click("button[data-a='tv-who'][data-arg='official']"); // the view remembers the last filter used on this table
  await waitText("FST-EDITED");
  const t7 = (await text()).replace(/\s+/g, " ");
  check(/Unofficial 1/.test(t7) && t7.includes("FST-EDITED"), "rows rebuilt from the table's transactions: 600 official + 1 unofficial");
  check(chain.batches > 0, "transactions fetched with batched JSON-RPC (" + chain.batches + " batch)");
  await page.fill("#tvq", "FST-001123");
  await page.waitForFunction(() => /showing 1 record/.test(document.getElementById("app").innerText), null, { timeout: 5000 });
  await page.click("td:has-text('FST-001123')");
  check((await text()).includes("2 versions"), "latest-wins merge works on chain-read rows too");
  await page.goto("https://iq.test/#/settings");
  await page.selectOption("select[data-arg='source']", "gateway");

  console.log("Devnet");
  await page.selectOption("select[data-arg='cluster']", "devnet");
  await page.goto("https://iq.test/#/account");
  await waitText("Wallets");
  check((await page.locator(".devnet").count()) === 1, "devnet banner shown");
  const before = lam(oa);
  await openDetails("Send");
  await page.click(`button[data-a='airdrop'][data-arg='${oa}']`);
  await waitText("Airdrop confirmed");
  check(lam(oa) === before + LAMPORTS, "devnet airdrop of 1 SOL confirmed");
  await shot("08b-account");
  await page.goto("https://iq.test/#/settings");
  await page.selectOption("select[data-arg='cluster']", "mainnet");

  console.log("Mobile layout");
  await page.setViewportSize({ width: 390, height: 844 });
  for (const [h, t] of [["#/", "iq-locker"], ["#/account", "Wallets"], ["#/mine", "Drafts in this browser"]]) {
    await page.goto("https://iq.test/" + h);
    await waitText(t);
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    check(overflow <= 1, `no horizontal page scroll at 390px on ${h} (${overflow})`);
  }
  await shot("08-mobile");

  check(consoleErrors.length === 0, "no console errors" + (consoleErrors.length ? ": " + consoleErrors.slice(0, 3).join(" | ") : ""));
  await browser.close();
  const failed = results.filter((r) => !r[0]).length;
  console.log(`\n${results.length - failed}/${results.length} checks passed`);
})().catch((e) => { console.error("E2E crashed:", e); process.exit(1); });
