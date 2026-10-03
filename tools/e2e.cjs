// End-to-end test: the built single-file app in headless Chromium against
//  * a mock Solana chain that decodes every IQ instruction with the official
//    IDL coder, checks accounts/data against the SDK's own instruction builder,
//    verifies Ed25519 signatures (legacy and v1 wire formats) and applies the
//    effects (DbRoot, tables, writer locks, rows, fees);
//  * a mock IQ gateway serving rows, files, file listings and .sol lookups;
//  * a virtual passkey authenticator (with the PRF extension) in Chromium.
// Signs in the ways users do (passkey, backup file dropped on the page, this
// browser) and edits tables through the spreadsheet and the SQL console.
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
  codes: new Map(), // send_code tx sig -> { code, before }
  sessions: new Map(), // session pda -> Map(index -> chunk)
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
    // the fee payer is writable in every transaction, whatever the instruction says
    const expS = sameKey.some((x) => x.isSigner), expW = sameKey.some((x) => x.isWritable) || k.pubkey.toBase58() === chain.curPayer;
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

// Reassemble data sent in chunks, the way IQ's readers do.
function assemble(st, path, total) {
  if (path.length >= 80) {
    const parts = [];
    let cur = path, guard = 0;
    while (cur && cur !== "Genesis") {
      const c = (st.pendingCodes || []).find((x) => x.sig === cur) || chain.codes.get(cur);
      if (!c || ++guard > 1000) throw new Error("linked list broken at " + cur);
      parts.unshift(c.code);
      cur = c.before;
    }
    return parts.join("");
  }
  const m = new Map(chain.sessions.get(path) || []);
  for (const p of st.pendingPosts || []) if (p.session === path) m.set(p.index, p.chunk);
  const out = [];
  for (let i = 0; i < total; i++) {
    if (!m.has(i)) throw new Error(`session ${path} is missing chunk ${i} of ${total}`);
    out.push(m.get(i));
  }
  return out.join("");
}
function userSeq(st, user) {
  const a = st.accounts.get(iq.contract.getUserPda(user, PID).toBase58());
  return a ? Number(accCoder.decode("UserState", a.data).total_session_files.toString()) : 0;
}
// A chunked finalize: fee by method (as measured on devnet), the session is
// closed and the user's sequence number advances.
function finalizeChunks(st, signer, a, ix, sessionIdx) {
  const md = JSON.parse(a.metadata);
  if (md.data !== undefined) throw new Error("chunked metadata must not carry data");
  if (md.total_chunks === undefined) throw new Error("chunked metadata needs total_chunks");
  let data;
  if (a.session) {
    const seq = Number(a.session.seq.toString());
    const sess = iq.contract.getSessionPda(signer, seq, PID).toBase58();
    if (a.on_chain_path !== sess) throw new Error("session path mismatch");
    if (ix.keys[sessionIdx].pubkey.toBase58() !== sess) throw new Error("session account mismatch");
    if (seq !== userSeq(st, signer)) throw new Error(`session seq ${seq} != user_state ${userSeq(st, signer)}`);
    const acc = st.accounts.get(sess);
    if (!acc) throw new Error("session not found");
    if (acc.data[13] === 1) throw new Error("custom program error: 0x177e SessionFinalized");
    acc.data.writeUInt32LE(a.session.total_chunks, 9);
    acc.data[13] = 1;
    data = assemble(st, sess, a.session.total_chunks);
    const us = st.accounts.get(iq.contract.getUserPda(signer, PID).toBase58());
    const u = accCoder.decode("UserState", us.data);
    u.total_session_files = new anchor.BN(seq + 1);
    const enc = accCoder.encode("UserState", u);
    us.data = Buffer.concat([enc, Buffer.alloc(Math.max(0, us.data.length - enc.length))]);
    debit(st, signer.toBase58(), 5_000_000, "session write fee");
    credit(st, FEE_RECEIVER, 5_000_000);
    st.sessionWrites = (st.sessionWrites || 0) + 1;
  } else {
    if (a.on_chain_path.length < 80) throw new Error("linked-list path must be a signature");
    data = assemble(st, a.on_chain_path);
    debit(st, signer.toBase58(), 3_000_000, "linked-list write fee");
    credit(st, FEE_RECEIVER, 3_000_000);
    st.linkedWrites = (st.linkedWrites || 0) + 1;
  }
  return data;
}

function execute(st, tx) {
  const logs = [];
  debit(st, tx.feePayer.toBase58(), 5000 * tx.nsig, "tx fee");
  chain.curPayer = tx.feePayer.toBase58();
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
      if (a.on_chain_path !== "" || a.session) {
        const sessAcc = a.session ? iq.contract.getSessionPda(signer, Number(a.session.seq.toString()), PID) : undefined;
        sameIx(ix, iq.contract.dbCodeInInstruction(builder, {
          user: signer, signer, user_inventory: iq.contract.getUserInventoryPda(signer, PID), db_root: root, table: tpda,
          system_program: new PublicKey(SYS), receiver: new PublicKey(FEE_RECEIVER), session: sessAcc,
        }, a), dec.name + " (chunked)");
      } else {
        sameIx(ix, expected, dec.name);
      }
      if (!st.accounts.get(iq.contract.getUserInventoryPda(signer, PID).toBase58())) throw new Error("custom program error: user not initialized");
      const tacc = st.accounts.get(tpda.toBase58());
      if (!tacc) throw new Error("table not found");
      const meta = accCoder.decode("Table", tacc.data);
      if (meta.writers.length && !meta.writers.some((w) => w.equals(signer))) throw new Error("custom program error: 0x1770 NotAuthorized (signer not in writers)");
      if (Buffer.byteLength(a.metadata) > 3400) throw new Error("metadata over the inline cap: " + Buffer.byteLength(a.metadata));
      const invLen = st.accounts.get(iq.contract.getUserInventoryPda(signer, PID).toBase58()).data.length;
      if (Buffer.byteLength(a.metadata) > 700 && invLen < 4213) throw new Error("custom program error: AccountDataTooSmall (pre-upgrade user_inventory, needs realloc)");
      const md = JSON.parse(a.metadata);
      let row, chunkedPath = "";
      if (a.on_chain_path !== "" || a.session) {
        row = JSON.parse(finalizeChunks(st, signer, a, ix, 4));
        chunkedPath = a.on_chain_path;
      } else {
        if (md.total_chunks !== 1 || md.method !== 0) throw new Error("bad metadata envelope");
        row = JSON.parse(md.data);
      }
      const cols = new Set([...meta.column_names.map((c) => Buffer.from(c).toString()), Buffer.from(meta.id_col).toString()]);
      for (const k of Object.keys(row)) if (!cols.has(k)) throw new Error("custom program error: 0x1787 SchemaMismatch " + k);
      if (!chunkedPath) {
        debit(st, signer.toBase58(), 1_000_000, "write fee");
        credit(st, FEE_RECEIVER, 1_000_000);
      }
      st.pendingRows = st.pendingRows || [];
      st.pendingRows.push({ table: tpda.toBase58(), row, signer: signer.toBase58(), path: chunkedPath });
    } else if (dec.name === "user_inventory_code_in") {
      const inv = iq.contract.getUserInventoryPda(signer, PID);
      const chunked = a.on_chain_path !== "" || !!a.session;
      const sessAcc = a.session ? iq.contract.getSessionPda(signer, Number(a.session.seq.toString()), PID) : undefined;
      sameIx(ix, iq.contract.userInventoryCodeInInstruction(builder, {
        user: signer, user_inventory: inv, system_program: new PublicKey(SYS), receiver: new PublicKey(FEE_RECEIVER), session: sessAcc, iq_ata: undefined,
      }, a), dec.name + (chunked ? " (chunked)" : ""));
      const invAcc = st.accounts.get(inv.toBase58());
      if (!invAcc) throw new Error("custom program error: user not initialized");
      const bytes = Buffer.byteLength(a.metadata);
      if (bytes > 3400) throw new Error("metadata over the inline cap");
      if (bytes > 700 && invAcc.data.length < 4213) throw new Error("custom program error: AccountDataTooSmall");
      const md = JSON.parse(a.metadata);
      let stored = a.metadata;
      if (chunked) {
        if (md.method !== 0 || !md.filename || !md.filetype || md.total_chunks < 1) throw new Error("bad chunked file metadata");
        md.data = finalizeChunks(st, signer, a, ix, 4);
        stored = JSON.stringify(md);
        chain.chunkedFiles = (chain.chunkedFiles || 0) + 1;
      } else {
        if (md.total_chunks !== 1 || md.method !== 0 || typeof md.data !== "string" || !md.filename || !md.filetype) throw new Error("bad file metadata envelope");
        debit(st, signer.toBase58(), 1_000_000, "write fee");
        credit(st, FEE_RECEIVER, 1_000_000);
      }
      st.pendingFiles = st.pendingFiles || [];
      st.pendingFiles.push({ signer: signer.toBase58(), metadata: stored, listed: a.metadata });
    } else if (dec.name === "send_code") {
      sameIx(ix, iq.contract.sendCodeInstruction(builder, { user: signer, code_account: iq.contract.getCodeAccountPda(signer, PID), system_program: new PublicKey(SYS) }, a), dec.name);
      const ca = st.accounts.get(iq.contract.getCodeAccountPda(signer, PID).toBase58());
      if (!ca) throw new Error("custom program error: code account missing (user not initialized)");
      if (Buffer.byteLength(a.code) + 200 > ca.data.length) throw new Error("custom program error: AccountDataTooSmall (code account)");
      st.pendingCodes = st.pendingCodes || [];
      st.pendingCodes.push({ sig: tx.sig, code: a.code, before: a.before_tx });
    } else if (dec.name === "create_session") {
      const seq = Number(a.seq.toString());
      const sess = iq.contract.getSessionPda(signer, seq, PID);
      sameIx(ix, iq.contract.createSessionInstruction(builder, { user: signer, user_state: iq.contract.getUserPda(signer, PID), session: sess, system_program: new PublicKey(SYS) }, a), dec.name);
      if (seq !== userSeq(st, signer)) throw new Error(`create_session seq ${seq} != user_state ${userSeq(st, signer)}`);
      if (st.accounts.get(sess.toBase58())) throw new Error("custom program error: session already in use");
      const data = Buffer.concat([Buffer.from([74, 34, 65, 133, 96, 163, 80, 69]), Buffer.alloc(6)]);
      debit(st, signer.toBase58(), 721_360, "session rent");
      st.accounts.set(sess.toBase58(), { lamports: 721_360, data, owner: PID.toBase58() });
    } else if (dec.name === "post_chunk") {
      const sess = K(1).toBase58();
      sameIx(ix, iq.contract.postChunkInstruction(builder, { user: signer, session: K(1) }, a), dec.name);
      const acc = st.accounts.get(sess);
      if (!acc) throw new Error("session not found");
      if (acc.data[13] === 1) throw new Error("custom program error: 0x177e SessionFinalized");
      st.pendingPosts = st.pendingPosts || [];
      st.pendingPosts.push({ session: sess, index: a.index, chunk: a.chunk });
    } else if (dec.name === "update_table") {
      const root = iq.contract.getDbRootPda(Buffer.from(a.db_root_id), PID);
      const seed = Buffer.from(a.table_seed);
      const tpda = iq.contract.getTablePda(root, seed, PID);
      sameIx(ix, iq.contract.updateTableInstruction(builder, { db_root: root, table: tpda, signer }, a), dec.name);
      const r = decodeRoot(st, root.toBase58());
      if (!r) throw new Error("db_root not found");
      // as on devnet: only the database's creator may change a table (else NotAuthorized, 6000)
      if (!r.creator.equals(signer)) throw new Error("custom program error: 0x1770 NotAuthorized");
      const tacc = st.accounts.get(tpda.toBase58());
      if (!tacc) throw new Error("table not found");
      const cur = accCoder.decode("Table", tacc.data);
      const cols = a.column_names.map((c) => Buffer.from(c).toString());
      if (!cols.includes(Buffer.from(a.id_col).toString())) throw new Error("custom program error: 0x1782 IdColNotInColumns");
      const next = { ...cur, name: a.table_name, column_names: a.column_names, id_col: a.id_col, ext_keys: a.ext_keys, writers: a.writers_opt === null ? cur.writers : a.writers_opt };
      const enc = accCoder.encode("Table", next);
      if (enc.length > tacc.data.length) throw new Error("custom program error: AccountDidNotSerialize (table account too small, realloc first)");
      tacc.data = Buffer.concat([enc, Buffer.alloc(tacc.data.length - enc.length)]);
      st.tableUpdates = (st.tableUpdates || 0) + 1;
    } else if (dec.name === "update_db_root_table_list") {
      const root = iq.contract.getDbRootPda(Buffer.from(a.db_root_id), PID);
      sameIx(ix, iq.contract.updateDbRootTableListInstruction(builder, { db_root: root, signer }, a), dec.name);
      const r = decodeRoot(st, root.toBase58());
      if (!r) throw new Error("db_root not found");
      if (!r.creator.equals(signer)) throw new Error("custom program error: 0x1770 NotAuthorized");
      r.table_seeds = a.new_table_seeds.map((x) => Buffer.from(x));
      const enc = accCoder.encode("DbRoot", r);
      const racc = st.accounts.get(root.toBase58());
      if (enc.length > racc.data.length) throw new Error("custom program error: AccountDidNotSerialize (DbRoot too small, realloc first)");
      racc.data = Buffer.concat([enc, Buffer.alloc(racc.data.length - enc.length)]);
      st.listUpdates = (st.listUpdates || 0) + 1;
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
    chain.tableUpdates = (chain.tableUpdates || 0) + (st.tableUpdates || 0);
    chain.listUpdates = (chain.listUpdates || 0) + (st.listUpdates || 0);
    const bt = ++blockTime;
    for (const c of st.pendingCodes || []) chain.codes.set(c.sig, { code: c.code, before: c.before });
    for (const p of st.pendingPosts || []) {
      const m = chain.sessions.get(p.session) || new Map();
      m.set(p.index, p.chunk);
      chain.sessions.set(p.session, m);
    }
    chain.sessionWrites = (chain.sessionWrites || 0) + (st.sessionWrites || 0);
    chain.linkedWrites = (chain.linkedWrites || 0) + (st.linkedWrites || 0);
    for (const p of st.pendingRows || []) {
      const list = chain.rows.get(p.table) || [];
      list.push({ ...p.row, __txSignature: tx.sig, __signer: p.signer, __blockTime: bt, __onChainPath: p.path || "" });
      chain.rows.set(p.table, list);
    }
    for (const f of st.pendingFiles || []) {
      chain.files.set(tx.sig, { ...f, blockTime: bt });
      const list = chain.assets.get(f.signer) || [];
      list.push({ signature: tx.sig, slot: 1000, err: null, memo: null, blockTime: bt, confirmationStatus: "finalized", onChainPath: "", metadata: f.listed || f.metadata });
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

// An IQ git repository, stored the way IQ Labs' git stores it: commits are
// rows of table git_commits:<owner>:<repo> in the database iq-git-v1, a
// commit's tree is a JSON inscription {path: {txId, hash}} named iqgit-tree,
// and each file an inscription iqgit-blob:<path> with base64 bytes.
const REPOS = new Map(); // commit table -> repository
function gitRepo(seed, repo) {
  const owner = new PublicKey(Buffer.alloc(32, seed)).toBase58();
  const root = iq.contract.getDbRootPda(iq.utils.toSeedBytes("iq-git-v1"), PID);
  const pda = iq.contract.getTablePda(root, iq.utils.toSeedBytes(`git_commits:${owner}:${repo}`), PID).toBase58();
  const R = { owner, repo, pda, trees: [] };
  REPOS.set(pda, R);
  return R;
}
const GIT = gitRepo(11, "hello-iq");
const fakeSig = () => bs58.encode(require("crypto").randomBytes(64));
// a file inscribed the way IQ git stores blobs (base64 bytes)
function gitBlob(p, content, signer) {
  const sig = fakeSig();
  chain.files.set(sig, { metadata: JSON.stringify({ filetype: "application/octet-stream", method: 0, filename: "iqgit-blob:" + p, total_chunks: 1, data: Buffer.from(content).toString("base64") }), signer, blockTime: Math.floor(Date.now() / 1000) });
  return sig;
}
function gitCommit(message, files, signer = GIT.owner) {
  return repoCommit(GIT, message, files, signer);
}
function repoCommit(R, message, files, signer = R.owner) {
  const bt = Math.floor(Date.now() / 1000);
  const tree = {};
  for (const [p, content] of Object.entries(files)) {
    const sig = gitBlob(p, content, signer);
    tree[p] = { txId: sig, hash: require("crypto").createHash("sha256").update(content).digest("hex") };
  }
  const treeSig = fakeSig();
  chain.files.set(treeSig, { metadata: JSON.stringify({ filetype: "application/json", method: 0, filename: "iqgit-tree", total_chunks: 1, data: JSON.stringify(tree) }), signer, blockTime: bt });
  const list = chain.rows.get(R.pda) || [];
  const parent = list.length ? list[list.length - 1].id : undefined;
  const id = require("crypto").randomUUID();
  list.push({ id, message, treeTxId: treeSig, ...(parent ? { parentCommitId: parent } : {}), timestamp: Date.now() + list.length, author: signer, __txSignature: fakeSig(), __signer: signer, __blockTime: bt });
  chain.rows.set(R.pda, list);
  R.trees.push(treeSig);
  return { id, tree: treeSig, files: tree };
}
gitCommit("first version", { "index.html": "<h1>v1</h1>", "README.md": "# hello-iq\n" });

// A hand-made WebAssembly module speaking the decoder interface (ABI `abi`)
// that answers every message with `reply`; with `imports`, it also asks for
// a function from outside (which a loader must refuse).
function fakeDecoder(reply, { abi = 1, imports = false, capped = true, memories = 1 } = {}) {
  const leb = (n) => { const o = []; do { let b = n & 127; n >>>= 7; if (n) b |= 128; o.push(b); } while (n); return o; };
  const sleb = (n) => { const o = []; for (;;) { const b = n & 127; n >>= 7; if ((n === 0 && !(b & 64)) || (n === -1 && (b & 64))) { o.push(b); return o; } o.push(b | 128); } };
  const str = (s) => [...leb(Buffer.byteLength(s)), ...Buffer.from(s)];
  const sec = (id, body) => [id, ...leb(body.length), ...body];
  const vec = (items) => [...leb(items.length), ...items.flat()];
  const data = [...Buffer.from(JSON.stringify(reply))];
  const I32 = 0x7f;
  const types = vec([[0x60, 0, 1, I32], [0x60, 1, I32, 1, I32], [0x60, 2, I32, I32, 1, I32]]);
  const base = imports ? 1 : 0;
  const body = (instrs) => { const b = [0, ...instrs, 0x0b]; return [...leb(b.length), ...b]; };
  const konst = (n) => [0x41, ...sleb(n)];
  const bytes = [0, 0x61, 0x73, 0x6d, 1, 0, 0, 0,
    ...sec(1, types),
    ...(imports ? sec(2, vec([[...str("env"), ...str("peek"), 0, 0]])) : []),
    ...sec(3, vec([[0], [1], [2], [0]])),
    ...sec(5, vec(Array.from({ length: memories }, () => (capped ? [1, 2, 2] : [0, 2])))),
    ...sec(7, vec([[...str("memory"), 2, 0], [...str("iqt_abi"), 0, base], [...str("iqt_alloc"), 0, base + 1], [...str("iqt_call"), 0, base + 2], [...str("iqt_len"), 0, base + 3]])),
    ...sec(10, vec([body(konst(abi)), body(konst(65536)), body(konst(0)), body(konst(data.length))])),
    ...sec(11, vec([[0, ...konst(0), 0x0b, ...leb(data.length), ...data]])),
  ];
  return Buffer.from(bytes);
}

// The IQ Tables repository, as deployed: the page plus the decoder, the
// loader and the format registry. IQT9 is a (made-up) retired format that
// only an older decoder reads.
const DEC = gitRepo(13, "iq-tables");
DEC.wasm = fs.readFileSync(path.join(ROOT, "site", "iqt-decoder.wasm"));
DEC.iqt9 = gitBlob("iqt-decoder.wasm", fakeDecoder({ ok: { schema: { cols: ["part_no", "name", "note"], id: 0 }, records: [{ vals: ["FST-IQT9", "From a retired format", null], deleted: false }], meta: null } }), DEC.owner);
DEC.files = () => ({
  "index.html": "<!doctype html><title>IQ Tables</title>",
  "iqt-decoder.wasm": DEC.wasm,
  "iqt-loader.mjs": fs.readFileSync(path.join(ROOT, "embed", "iqt-loader.mjs")),
  "iqt-formats.json": JSON.stringify({ abi: 1, formats: { IQT1: "current", IQT9: DEC.iqt9 } }),
});
DEC.first = repoCommit(DEC, "Deploy IQ Tables", DEC.files());

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
    const R = REPOS.get(m[1]);
    if (R) return { name: `git_commits:${R.owner}:${R.repo}`, columns: ["id", "message", "treeTxId", "parentCommitId", "timestamp", "author"], idCol: "id", lastTimestamp: 0, gate: null };
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
  m = p.match(/^\/sns\/([^/]+)$/);
  if (m) return chain.sns && chain.sns[decodeURIComponent(m[1])] ? { domain: m[1], owner: chain.sns[decodeURIComponent(m[1])], record: null } : { domain: m[1], owner: null, record: null };
  m = p.match(/^\/user\/([^/]+)\/assets$/);
  if (m) return [...(chain.assets.get(m[1]) || [])].reverse();
  m = p.match(/^\/table\/([^/]+)\/notify$/);
  if (m && method === "POST") { chain.notifies.push({ table: m[1], body: JSON.parse(body) }); return { ok: true }; }
  return { error: "mock: no route " + p };
}

// -------------------------------------------------------------------- keys
const { sha256 } = require("@noble/hashes/sha2");
// "funder": a Solana CLI keypair file imported into a file account; "other": a
// base58 key a second user pastes. Both start with 10 SOL. "other" also has IQ
// accounts from the pre-upgrade program (900 bytes), so its first v1 write
// must grow them.
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
chain.sns = { "alice.sol": funderKp.publicKey.toBase58() };

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
  // (the browser logs each 429 from the rate-limited mock RPC; those are expected)
  page.on("console", (m) => { if (m.type() === "error" && !/status of 429/.test(m.text())) consoleErrors.push(m.text()); });
  page.on("pageerror", (e) => consoleErrors.push("pageerror: " + e.message));
  const POST_CHUNK = Buffer.from([209, 8, 101, 123, 165, 205, 108, 54]);
  const CREATE_SESSION = Buffer.from([242, 193, 143, 179, 150, 25, 122, 227]);
  const rpcRoute = async (route, req) => {
    if (req.method() === "OPTIONS") return route.fulfill({ status: 204, headers: { "access-control-allow-origin": "*", "access-control-allow-headers": "content-type" } });
    let one;
    try { one = JSON.parse(req.postData()); } catch (_) {}
    if (one && one.method === "sendTransaction") {
      // an RPC with a rate limit, some latency, and (on demand) dropped transactions
      const now = Date.now();
      chain.sendTimes = (chain.sendTimes || []).filter((t) => now - t < 1000);
      if (chain.sendLimit && chain.sendTimes.length >= chain.sendLimit) {
        chain.throttled = (chain.throttled || 0) + 1;
        return route.fulfill({ status: 429, contentType: "text/plain", headers: { "access-control-allow-origin": "*" }, body: "Too many requests for a specific RPC call" });
      }
      chain.sendTimes.push(now);
      chain.sendInflight = (chain.sendInflight || 0) + 1;
      chain.maxSendInflight = Math.max(chain.maxSendInflight || 0, chain.sendInflight);
      await new Promise((r) => setTimeout(r, 40));
      chain.sendInflight--;
      const raw = Buffer.from(one.params[0], "base64");
      if (chain.dropPosts > 0 && raw.indexOf(POST_CHUNK) >= 0 && raw.indexOf(CREATE_SESSION) < 0) {
        chain.dropPosts--;
        chain.dropped = (chain.dropped || 0) + 1;
        const counts = { ...chain.txCount };
        const sig = parseTx(raw).sig;
        chain.txCount = counts;
        return route.fulfill({ status: 200, contentType: "application/json", headers: { "access-control-allow-origin": "*" }, body: JSON.stringify({ jsonrpc: "2.0", id: one.id, result: sig }) });
      }
    }
    let res;
    try { res = rpc(req.postData()); } catch (e) { res = { jsonrpc: "2.0", id: 1, error: { code: -1, message: "mock crashed: " + e.message } }; }
    return route.fulfill({ status: 200, contentType: "application/json", headers: { "access-control-allow-origin": "*" }, body: JSON.stringify(res) });
  };
  const handler = async (route) => {
    const req = route.request();
    const url = req.url();
    if (url.startsWith("https://iq.test/")) return route.fulfill({ status: 200, contentType: "text/html", body: html });
    // someone else's site with an iframe of a table
    if (url.startsWith("https://host.test/")) return route.fulfill({ status: 200, contentType: "text/html", body: chain.hostPage || "<p>empty</p>" });
    // the site as IQ's browser serves it: at the address of the repository it was deployed from
    if (url.startsWith(`https://browser.iqlabs.dev/${DEC.pda}`)) return route.fulfill({ status: 200, contentType: "text/html", body: html });
    if (url.startsWith("https://gateway.iqlabs.dev/") || url.startsWith("https://dev-gateway.iqlabs.dev/")) {
      const res = gateway(url, req.method(), req.postData());
      return route.fulfill({ status: res.error ? 404 : 200, contentType: "application/json", headers: { "access-control-allow-origin": "*" }, body: JSON.stringify(res) });
    }
    if (url.startsWith("https://solana-rpc.publicnode.com") || url.startsWith("https://api.devnet.solana.com")) return rpcRoute(route, req);
    return route.fulfill({ status: 404, body: "blocked in test: " + url });
  };
  await page.route("**/*", handler);
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
      const run = await page.locator(".run").allTextContents().catch(() => []);
      console.log("---- waiting for:", s, "\n---- run panel:\n" + run.join("\n") + "\n---- toast: " + (await toast()));
      console.log("---- page:", (await text()).slice(0, 1200));
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
  const sql = async (q) => {
    await page.click("button[data-a='ed-tab'][data-arg='sql']");
    await page.fill("textarea[data-keys='sql']", q);
    await page.click("button[data-a='sql-run']");
    await page.waitForTimeout(150);
    // COMMIT switches to the Save tab, where the progress is
    return (await page.locator(".sqlout").count()) ? await page.locator(".sqlout").innerText() : await text();
  };
  const cell = (r, c) => page.locator(`table.sheet td[data-arg='${r}:${c}']`);
  const sheetText = () => page.locator("table.sheet tbody").innerText();

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

  console.log("Sign in with a wallet key");
  await page.goto("https://iq.test/#/mine");
  await waitText("Sign in to see");
  await page.click("header a:has-text('Sign in')");
  await waitText("Sign in with your wallet");
  check(!(await text()).includes("passkey") && (await page.locator("[data-a='passkey-create'],[data-a='browser-account']").count()) === 0, "the only way in is a wallet key (no passkeys, no browser-saved accounts)");
  await shot("02a-signin");
  // no wallet yet: make one here
  const nw = await download(() => page.click("button[data-a='make-wallet']"));
  const nwKey = Buffer.from(JSON.parse(nw.text));
  const nwKp = Keypair.fromSecretKey(Uint8Array.from(nwKey));
  const mainAddr = nwKp.publicKey.toBase58();
  const nwB58 = bs58.encode(nwKey);
  check(/^wallet-\w{8}\.json$/.test(nw.name) && nwKey.length === 64, `a new wallet's key downloads as a Solana key file (${nw.name}), the format the Solana CLI and iqgit use`);
  await waitText("the only way into this wallet");
  await page.goto("https://iq.test/#/account");
  await page.click("button[data-a='panel'][data-arg='add']");
  await waitText("Copy address");
  check((await page.locator(".panel .addrbox .mono").innerText()).trim() === mainAddr && (await page.locator(".panel svg.qr").count()) === 1, "signed in as that wallet; Add funds shows its address and a QR code");
  await page.click("button[data-a='panel'][data-arg='add']");
  const stored = await page.evaluate(() => JSON.stringify(localStorage));
  check(!stored.includes(mainAddr) && !stored.includes(nwB58) && !stored.includes(Array.from(nwKey.subarray(0, 12)).join(",")), "nothing about the wallet is stored in the browser");
  await menu("Sign out");
  await waitText("Signed out");
  // next visit: drop the key file anywhere on the page
  const overlay = await dropFile(nw.name, nw.text);
  check(overlay, "drop overlay shows while dragging a file over the page");
  await waitText(`Signed in as ${solana_short(mainAddr)}`);
  check(true, "dropping the key file signs in again");
  // the user buys SOL on an exchange and sends it to their address
  setAcct(mainAddr, { lamports: 5 * LAMPORTS, data: Buffer.alloc(0), owner: SYS });
  await page.goto("https://iq.test/#/account");
  await page.click("button[data-a='refresh-balances']");
  await waitText("5.0000 SOL");
  check(true, "balance shows funds sent to the wallet");
  await shot("02-account");
  // an extra wallet is made from the key, so it can't be lost
  await openDetails("Advanced");
  await page.fill("#wlabel", "spare");
  await page.press("#wlabel", "Enter");
  await waitText("New wallet \"spare\"");
  const master = sha256(Buffer.concat([Buffer.from("iq-tables/key-master/v1"), nwKey.subarray(0, 32)]));
  const derive = (i) => Keypair.fromSeed(sha256(Buffer.concat([Buffer.from("iq-tables/account/v1/wallet"), master, u32le(i)])));
  const spare = derive(0).publicKey.toBase58();
  check((await addrOf("spare")) === spare, "a new wallet = SHA-256(domain ‖ SHA-256(domain ‖ key) ‖ 0), matching an independent derivation");
  await openDetails("Send · key");
  await page.selectOption(`select[data-arg='to:${mainAddr}']`, spare);
  await page.fill(`#amt-${mainAddr}`, "0.25");
  await page.click(`button[data-a='transfer'][data-arg='${mainAddr}']`);
  await waitText("Transfer confirmed");
  check(lam(spare) === 0.25 * LAMPORTS, "moved 0.25 SOL between two wallets");
  await menu("Sign out");
  await waitText("Signed out");
  // pasting the secret key works too, and brings the extra wallet back
  await page.goto("https://iq.test/#/account");
  await page.fill("#keys-text", nwB58);
  await page.press("#keys-text", "Enter");
  await waitText("Recovered 1 wallet");
  check(true, "pasting the secret key signs in, and the wallet made from it is found again");
  // optional: a passphrase-protected copy (the IQ SDK's passwordEncrypt scheme)
  await page.goto("https://iq.test/#/account");
  await openDetails("Advanced");
  await page.fill("#pass1", PASS);
  await page.fill("#pass2", PASS);
  const f2 = await download(() => page.click("button[data-a='set-passphrase']"));
  const p2 = JSON.parse(f2.text);
  check(p2.format === "iq-tables-account" && p2.ciphertext && !f2.text.includes(nwB58), "the protected copy is encrypted (no secrets in the clear)");
  const plain2 = JSON.parse(Buffer.from(await iq.crypto.passwordDecrypt(PASS, p2.salt, p2.iv, p2.ciphertext)).toString());
  check(plain2.wallets.some((w) => w.secret === nwB58), "the IQ SDK's own passwordDecrypt opens it");
  await menu("Sign out");
  await waitText("Signed out");
  await dropFile(f2.name, f2.text);
  await waitText("enter its passphrase");
  await page.fill("#unlock-pass", "wrong passphrase!");
  await page.press("#unlock-pass", "Enter");
  await waitText("Wrong passphrase");
  check(true, "wrong passphrase rejected");
  await page.fill("#unlock-pass", PASS);
  await page.press("#unlock-pass", "Enter");
  await waitText("Databases you own");
  await page.goto("https://iq.test/#/account");
  await page.click("button[data-a='panel'][data-arg='add']");
  check((await page.locator(".panel .addrbox .mono").innerText()).trim() === mainAddr, "…and signs in as the same wallet");
  await page.click("button[data-a='panel'][data-arg='add']");
  const fa = funderKp.publicKey.toBase58();
  await page.click("button[data-a='panel'][data-arg='send']");
  await page.fill("#send-to", "alice.sol");
  await page.fill("#send-amt", "0.5");
  await page.click("button[data-a='send-review']");
  await waitText("Send 0.5000 SOL to alice.sol");
  check((await text()).includes(solana_short(fa)), ".sol name resolved through IQ's gateway before sending");
  const funderBefore = lam(fa);
  await page.click("button[data-a='send-confirm']");
  await waitText("Transfer confirmed");
  check(lam(fa) === funderBefore + 0.5 * LAMPORTS, "sent 0.5 SOL to alice.sol");

  console.log("Editor: new database");
  await page.goto("https://iq.test/#/ws");
  await waitText("New database");
  await page.fill("#newdb", "e2e-parts");
  await page.press("#newdb", "Enter");
  await page.waitForSelector("table.sheet");
  check((await text()).includes("sheet1"), "a new database opens on an empty sheet");
  const dkey = (await page.evaluate(() => location.hash)).split("/")[2];
  const rootPda = iq.contract.getDbRootPda(Buffer.from("e2e-parts"), PID).toBase58();
  const fastPda = iq.contract.getTablePda(new PublicKey(rootPda), iq.utils.toSeedBytes("fasteners"), PID).toBase58();
  const supPda = iq.contract.getTablePda(new PublicKey(rootPda), iq.utils.toSeedBytes("suppliers"), PID).toBase58();
  // typing like a spreadsheet; the id column numbers rows by itself
  check((await page.locator("table.sheet thead th").count()) === 4 && (await page.locator("table.sheet thead").innerText()).includes("id"), "a new sheet starts with an automatic id column");
  await cell(0, 0).click();
  await page.keyboard.type("A-1");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(100);
  check((await toast()).includes("whole number"), "the id column only takes numbers: " + (await toast()).slice(0, 80));
  await cell(0, 1).click();
  await page.keyboard.type("Hex bolt");
  await page.keyboard.press("Tab");
  await page.keyboard.type("zinc");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(100);
  check((await sheetText()).includes("1\t1\tHex bolt\tzinc"), "click a cell, type, Tab and Enter — like Excel; the row got id 1");
  await cell(1, 1).click();
  await page.evaluate(() => { const dt = new DataTransfer(); dt.setData("text/plain", "Nut\tsteel\tM8\nWasher\tbrass\tM6\n"); document.getElementById("sheet").dispatchEvent(new ClipboardEvent("paste", { clipboardData: dt, bubbles: true })); });
  await page.waitForTimeout(150);
  const afterPaste = await sheetText();
  check(afterPaste.includes("2\tNut\tsteel\tM8") && afterPaste.includes("3\tWasher\tbrass\tM6"), "paste from Excel fills cells, numbers the rows and adds a column when needed");
  await page.keyboard.press("Control+z");
  await page.waitForTimeout(100);
  check(!(await sheetText()).includes("Nut") && (await page.locator("table.sheet thead th").count()) === 4, "Ctrl+Z undoes the whole paste in one step");
  await page.keyboard.press("Control+y");
  await page.waitForTimeout(100);
  check((await sheetText()).includes("Washer"), "Ctrl+Y redoes it");
  await shot("03-sheet");

  console.log("Editor: tables via the sidebar, import and SQL");
  await page.fill("#quick-t", "fasteners");
  await page.press("#quick-t", "Enter");
  await page.waitForFunction(() => location.hash.endsWith("/1"));
  const kinds = ["Hex bolt", "Socket head cap screw", "Flat washer", "Nylon lock nut", "Carriage bolt", "Set screw"];
  const mats = ["18-8 stainless", "316 stainless", "Grade 5 steel", "Grade 8 steel", "Brass"];
  const thr = ["M6x1.0", "M8x1.25", "M10x1.5", "1/4-20 UNC", "3/8-16 UNC"];
  let x = 2463534242;
  const rnd = (n) => { x ^= x << 13; x >>>= 0; x ^= x >>> 17; x ^= x << 5; x >>>= 0; return x % n; };
  const sups = ["Brazos Bolt & Nut, LLC", "Lone Star Fastener Co.", "Gulf Coast Supply", "Permian Industrial"];
  let csv = "part_no,name,material,thread,length_mm,qty,unit_price,supplier,updated\n";
  const fastRows = [];
  for (let i = 0; i < 600; i++) {
    const k = kinds[rnd(6)], m = mats[rnd(5)], t = thr[rnd(5)], len = [10, 16, 20, 25, 30, 40, 50][rnd(7)];
    const qty = rnd(5000), p1 = rnd(40), p2 = rnd(100), sup = sups[rnd(4)], up = rnd(900000000);
    const part = `FST-${String(1000 + i).padStart(6, "0")}`;
    fastRows.push({ part, material: m, qty, supplier: sup });
    csv += `${part},"${k} ${t} x ${len}mm",${m},${t},${len},${qty},${p1}.${String(p2).padStart(2, "0")},"${sup}",${1790000000000 + up * 100}\n`;
  }
  await page.click("button[data-a='ed-tab'][data-arg='import']");
  await page.fill("textarea[data-arg^='csv:']", csv);
  await page.click("button[data-a='import-csv']");
  await waitText("Imported 600 rows");
  await page.click("button[data-a='ed-tab'][data-arg='operations']");
  await page.click("button[data-a='op-access'][data-arg='open']");
  await waitText("Anyone can add rows");
  check((await page.locator(".lastsql").innerText()).includes("GRANT INSERT ON `fasteners` TO PUBLIC"), "Operations → Anyone runs GRANT … TO PUBLIC and shows it");
  await page.click("button[data-a='ed-tab'][data-arg='browse']");
  await cell(0, 0).click();
  await page.keyboard.type("FST-EDITED");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(100);
  check((await sheetText()).startsWith("1\tFST-EDITED"), "edited the first imported row in place");
  let out = await sql(`DROP TABLE sheet1; CREATE TABLE suppliers (name PRIMARY KEY, city, state, website, catalog, spec_file); INSERT INTO suppliers VALUES ('Brazos Bolt & Nut', 'Waco', 'TX', 'https://brazosbolt.example/catalog', 'iq://table/${fastPda}/FST-001001', NULL), ('Lone Star Fastener Co.', 'Austin', 'TX', NULL, NULL, NULL)`);
  check(out.includes("Table sheet1 dropped") && out.includes("Table suppliers created") && out.includes("2 row(s) inserted"), "SQL: DROP TABLE, CREATE TABLE and INSERT");
  out = await sql("SELECT name, city FROM suppliers WHERE state = 'tx' ORDER BY city; SELECT COUNT(*) AS n, material FROM fasteners WHERE material LIKE '%stainless' GROUP BY material ORDER BY n DESC LIMIT 1");
  check(/Lone Star Fastener Co\.\s+Austin\s+(✎ edit\s+)?Brazos Bolt & Nut\s+Waco/.test(out), "SQL: WHERE (case-insensitive) and ORDER BY over unsaved rows");
  check(/\n\d+\s+(18-8|316) stainless/.test(out), "SQL: GROUP BY with COUNT(*)");
  out = await sql("DELETE FROM suppliers WHERE city = 'Austin'; SHOW CHANGES");
  check(out.includes("1 row(s) deleted") && /COMMIT would write \d+ pack/.test(out), "SQL: DELETE and SHOW CHANGES with a cost estimate");
  await shot("04-sql");
  // a file in the selected cell (paid from the main balance automatically)
  await page.click(`a[href='#/ws/${dkey}/1']`);
  await page.waitForSelector("table.sheet");
  await cell(0, 5).click();
  const spec = "Torque spec, dry threads\nM6 grade 8.8: 10 N·m\nM8 grade 8.8: 25 N·m\nM10 grade 8.8: 49 N·m\n";
  await page.setInputFiles("input[data-fileb64='attach-sel']", { name: "torque-spec.txt", mimeType: "text/plain", buffer: Buffer.from(spec) });
  await waitText("linked in the cell", 30000);
  check(chain.ixSeen.user_inventory_code_in === 1 && chain.ixSeen.user_initialize === 1, "file inscribed with user_inventory_code_in (after the wallet's one-time setup), checked against the SDK builder");
  const fileSig = [...chain.files.keys()].pop();
  check((await cell(0, 5).innerText()).includes("torque-spec.txt"), "the cell links to the file");
  check(JSON.parse(chain.files.get(fileSig).metadata).data === spec, "file stored as text, exactly like the SDK's codeIn");

  console.log("Save to blockchain (one button)");
  await page.click("button[data-a='ed-tab'][data-arg='save']");
  const t2 = await text();
  const packMatch = t2.match(/fasteners: 600 changed row\(s\) in one write sent in (\d+) parts/);
  check(!!packMatch, "save plan: 600 rows as ONE write in parts (IQ's chunked upload is cheaper than 4 direct writes): " + (packMatch ? packMatch[0] : t2.match(/fasteners:[^\n]*/)));
  const nParts = packMatch ? Number(packMatch[1]) : 0;
  const nPacks = 1;
  check(nParts > 1 && nParts < 10, `600 records fit in ${nParts} parts of a linked list`);
  const mainBefore = lam(mainAddr);
  await page.click(`button[data-a='inscribe'][data-arg='${dkey}']`);
  await waitText("Saved ✓", 60000);
  const t3 = await page.locator(".run").textContent();
  check(!t3.includes("Simulation rejected") && !t3.includes("failed"), "saved without errors");
  const root = accCoder.decode("DbRoot", acct(rootPda).data);
  const dbw = root.creator.toBase58();
  check(dbw === mainAddr && lam(mainAddr) < mainBefore, "the database belongs to the wallet you signed in with, which paid for the save");
  check(root.table_creators.length === 1 && root.table_creators[0].toBase58() === dbw, "table creation locked to the database wallet");
  const fastMeta = accCoder.decode("Table", acct(fastPda).data);
  const supMeta = accCoder.decode("Table", acct(supPda).data);
  check(fastMeta.writers.length === 0, "open table: no writer restriction");
  check(supMeta.writers.length === 1 && supMeta.writers[0].toBase58() === dbw, "locked table: writers = [database wallet]");
  check((chain.rows.get(fastPda) || []).length === 1 && chain.linkedWrites === 1 && chain.ixSeen.send_code >= nParts, `one pack row in "fasteners", finalized after ${nParts} send_code parts (checked against the SDK builder)`);
  check(!acct(iq.contract.getTablePda(new PublicKey(rootPda), iq.utils.toSeedBytes("sheet1"), PID).toBase58()), "the dropped table was never created");
  check(chain.txCount.v1 > 0, `v1 transactions used once the feature gate is on (${chain.txCount.v1} v1, ${chain.txCount.legacy} legacy)`);
  check((chain.reallocs || 0) >= 1, "DbRoot realloc path exercised (" + (chain.reallocs || 0) + ")");
  check(chain.notifies.length === [...(chain.rows.get(fastPda) || []), ...(chain.rows.get(supPda) || [])].filter((r) => !r.__onChainPath).length, "IQ gateway notified for every direct write (" + chain.notifies.length + ")");
  console.log("   instruction mix:", JSON.stringify(chain.ixSeen));
  await page.click("button[data-a='ed-tab'][data-arg='browse']");
  await page.waitForFunction(() => !document.querySelector(".pendbar"), null, { timeout: 20000 });
  check(true, "after saving, nothing is pending");
  await shot("05-saved");

  console.log("Explorer: links and files");
  await page.goto(`https://iq.test/#/t/${rootPda}/${supPda}`);
  await page.waitForSelector("table.data td:has-text('Brazos Bolt & Nut')");
  const web = page.locator("table.data a[href='https://brazosbolt.example/catalog']");
  check((await web.count()) === 1 && (await web.getAttribute("target")) === "_blank", "web link in a cell opens in a new tab");
  check(!(await text()).includes("Lone Star Fastener"), "the row deleted before saving never reached the chain");
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

  console.log("Explorer reads it back; edit a saved record in the editor");
  await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
  await page.waitForFunction(() => /showing 600 record/.test(document.getElementById("app").innerText), null, { timeout: 20000 });
  const t4 = await text();
  check(t4.includes("FST-EDITED"), "edited value came back from the chain");
  check(t4.includes("IQT packed"), "table recognised as IQT-packed");
  await page.fill("#tvq", "FST-001123");
  await page.waitForFunction(() => /showing 1 record/.test(document.getElementById("app").innerText), null, { timeout: 5000 });
  await page.click("td:has-text('FST-001123')");
  await page.click("button:has-text('Edit in the Editor')");
  await page.waitForFunction(() => location.hash.startsWith("#/ws/"));
  await page.waitForFunction(() => { const t = document.querySelector("table.sheet tbody"); return t && t.innerText.includes("FST-001123"); }, null, { timeout: 20000 });
  check((await page.locator("table.sheet tbody tr:not(.blank)").count()) === 1, "the editor opens on that record, read from the chain");
  await cell(0, 5).click();
  await page.keyboard.type("9999");
  await page.keyboard.press("Enter");
  await waitText("1 row change (1 edited)");
  check((await page.locator("table.sheet td.chg").count()) === 1, "the edited cell is highlighted until saved");
  await page.click("button[data-a='inscribe']");
  await waitText("Saved ✓", 60000);
  await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
  await page.fill("#tvq", "FST-001123");
  await page.waitForFunction(() => /showing 1 record/.test(document.getElementById("app").innerText), null, { timeout: 20000 });
  await page.click("td:has-text('FST-001123')");
  const t5 = await text();
  check(t5.includes("9999") && t5.includes("2 versions"), "only the change was saved, as a new version (latest wins)");

  console.log("My tables");
  await page.goto("https://iq.test/#/mine");
  await waitText("torque-spec.txt");
  const tm = await text();
  check(/Databases you own[\s\S]*e2e-parts/.test(tm), "My tables lists the database the account owns");
  check(tm.includes("fasteners") && tm.includes("suppliers"), "…with its tables");
  check(/Files[\s\S]*torque-spec\.txt/.test(tm), "…and the files its wallets inscribed (IQ gateway /user/<wallet>/assets)");
  await shot("06-mine");

  console.log("phpMyAdmin-style administration of saved tables");
  const sqlBad = () => page.locator(".sqlout .sqlmsg.bad").allInnerTexts();
  const lastSql = () => page.locator(".lastsql code").innerText().catch(() => "");
  const effQty = (r) => (r.part === "FST-001123" ? 9999 : r.qty);
  // suppliers (table 1): change a column with the form
  await page.goto(`https://iq.test/#/ws/${dkey}/1`);
  await page.waitForSelector("table.sheet");
  await page.click("button[data-a='ed-tab'][data-arg='structure']");
  await page.waitForSelector("table.struct");
  await page.click("button[data-a='col-edit'][data-arg='2']");
  await page.waitForSelector(".colform");
  check((await page.inputValue("#ce-name")) === "state", "Change opens the column form filled in");
  await page.selectOption("#ce-type", "enum");
  await page.fill("#ce-param", "TX, OK, NM");
  await page.fill("#ce-default", "TX");
  await shot("06a-column-form");
  await page.click("button[data-a='col-save']");
  await page.waitForFunction(() => !document.querySelector(".colform"));
  check((await lastSql()).includes("ALTER TABLE `suppliers` CHANGE `state` `state` ENUM('TX','OK','NM') DEFAULT 'TX'"), "column form → ALTER TABLE … CHANGE … ENUM … DEFAULT (shown like phpMyAdmin)");
  await page.click("button[data-a='col-edit'][data-arg='new']");
  await page.waitForSelector(".colform");
  await page.fill("#ce-name", "rating");
  await page.selectOption("#ce-type", "int");
  await page.fill("#ce-default", "3");
  await page.selectOption("select[data-arg='ce:place']", "name");
  await page.click("button[data-a='col-save']");
  await page.waitForFunction(() => !document.querySelector(".colform"));
  check((await lastSql()).includes("ADD COLUMN `rating` INT DEFAULT 3 AFTER `name`"), "Add column form → ADD COLUMN … AFTER");
  let out2 = await sql("ALTER TABLE suppliers RENAME COLUMN city TO town, ADD UNIQUE KEY web (website), ADD CONSTRAINT rating_range CHECK (rating BETWEEN 1 AND 5)");
  check((await sqlBad()).length === 0, "SQL: ALTER TABLE with RENAME COLUMN, ADD UNIQUE and ADD CHECK in one statement" + ((await sqlBad()).join(" ")));
  out2 = await sql("INSERT INTO suppliers (name, town, rating) VALUES ('Permian Industrial', 'Odessa', 9)");
  check(/rating_range/.test((await sqlBad()).join(" ")), "CHECK constraint enforced on INSERT");
  out2 = await sql("INSERT INTO suppliers (name, website) VALUES ('Copycat', 'https://brazosbolt.example/catalog')");
  check(/unique|already/i.test((await sqlBad()).join(" ")), "UNIQUE key enforced");
  // Insert tab: typed form
  await page.click("button[data-a='ed-tab'][data-arg='insert']");
  await page.fill("[data-arg='in:0']", "Permian Industrial");
  await page.fill("[data-arg='in:1']", "4");
  await page.fill("[data-arg='in:2']", "Odessa");
  await page.click("button[data-a='insert-row']");
  await waitText("1 row change (1 new)");
  check((await lastSql()).includes("INSERT INTO `suppliers` (`name`, `rating`, `town`) VALUES ('Permian Industrial', 4, 'Odessa')"), "Insert tab → INSERT with typed values");
  await page.click("button[data-a='ed-tab'][data-arg='browse']");
  await page.waitForSelector("table.sheet");
  const supSheet = await sheetText();
  check(/Permian Industrial\t4\tOdessa\tTX/.test(supSheet) && /Brazos Bolt & Nut\t3\tWaco\tTX/.test(supSheet), "defaults fill the new row (TX) and the new column (3 for rows already saved)");
  check((await page.locator("table.sheet thead").innerText()).includes("town"), "renamed column shows in the sheet");
  // a file bigger than one transaction goes into a cell in parts (IQ's session upload)
  const bin = Buffer.alloc(40000);
  { let z = 99991; for (let i = 0; i < bin.length; i++) { z = (z * 1103515245 + 12345) & 0x7fffffff; bin[i] = (z >> 16) & 255; } }
  const vRows = await page.locator("table.sheet tbody tr").allInnerTexts();
  const permian = vRows.findIndex((t) => t.includes("Permian Industrial"));
  await cell(permian, 6).click();
  const sessBefore = chain.sessionWrites || 0;
  chain.sendLimit = 4; // a strict RPC: 4 transactions a second
  chain.maxSendInflight = 0;
  chain.throttled = 0;
  await page.setInputFiles("input[data-fileb64='attach-sel']", { name: "drawing.bin", mimeType: "application/octet-stream", buffer: bin });
  await waitText("linked in the cell", 180000);
  check(chain.maxSendInflight > 1 && chain.throttled > 0, `the parts went out several at a time (up to ${chain.maxSendInflight} in flight) and slowed down when the RPC said "too many requests" (${chain.throttled} times)`);
  chain.sendLimit = 0;
  check((chain.sessionWrites || 0) === sessBefore + 1 && chain.chunkedFiles === 1 && chain.ixSeen.post_chunk >= 15, `a 40 KB file went into the cell in parts: create_session + ${chain.ixSeen.post_chunk / 2} post_chunk, then user_inventory_code_in (each checked against the SDK builder)`);
  const bigSig = [...chain.files.keys()].pop();
  check(JSON.parse(chain.files.get(bigSig).metadata).data === bin.toString("base64"), "the parts reassemble to the file, byte for byte");
  check((await cell(permian, 6).innerText()).includes("drawing.bin"), "the cell links to the big file");

  // fasteners (table 0): types, rules, search, find & replace
  await page.goto(`https://iq.test/#/ws/${dkey}/0`);
  await page.waitForSelector("table.sheet");
  out2 = await sql("ALTER TABLE fasteners MODIFY qty INT NOT NULL, MODIFY unit_price DECIMAL(8,2), ADD CONSTRAINT qty_ok CHECK (qty >= 0)");
  check((await sqlBad()).length === 0, "SQL: MODIFY column types over 600 saved rows" + ((await sqlBad()).join(" ")));
  out2 = await sql("UPDATE fasteners SET qty = -1 WHERE part_no = 'FST-001002'");
  check(/qty_ok/.test((await sqlBad()).join(" ")), "CHECK enforced on UPDATE");
  out2 = await sql("UPDATE fasteners SET qty = 'lots' WHERE part_no = 'FST-001001'");
  check(/whole number/.test((await sqlBad()).join(" ")), "INT column rejects text");
  await page.click("button[data-a='ed-tab'][data-arg='search']");
  await page.selectOption("select[data-arg='sq:op:qty']", "gt");
  await page.fill("#sq-qty", "4900");
  await page.selectOption("select[data-arg='sq:op:material']", "eq");
  await page.fill("#sq-material", "Brass");
  await page.click("button[data-a='search-run']");
  await page.waitForSelector("table.res");
  const expBig = fastRows.filter((r) => effQty(r) > 4900 && r.material === "Brass").length;
  const noteTxt = await page.locator(".card:has(table.res) p.muted").first().innerText();
  check(noteTxt.startsWith(`${expBig} row`), `Search tab (qty > 4900 AND material = Brass): ${expBig} rows, like the data says (${noteTxt})`);
  check((await page.locator("button[data-a='sql-edit']").count()) === expBig, "each result row has an Edit link");
  await shot("06c-search");
  const firstPart = (await page.locator("table.res tbody tr").first().locator("td").nth(1).innerText()).trim();
  await page.locator("button[data-a='sql-edit']").first().click();
  await page.waitForFunction(() => { const t = document.querySelector("table.sheet tbody"); return t && t.querySelectorAll("tr:not(.blank)").length === 1; }, null, { timeout: 5000 });
  check((await sheetText()).includes(firstPart), "Edit opens that row in the sheet");
  await page.click("button[data-a='ed-tab'][data-arg='search']");
  await page.click("button[data-a='search-clear']");
  await openDetails("Find and replace");
  await page.selectOption("select[data-arg='rp:col']", "material");
  await page.fill("#rp-find", "Brass");
  await page.fill("#rp-with", "Bronze");
  await page.click("button[data-a='replace-run']");
  const nBrass = fastRows.filter((r) => r.material === "Brass").length;
  await waitText(`${nBrass} row(s) changed`);
  check((await lastSql()).startsWith("UPDATE `fasteners` SET `material` = REPLACE(`material`, 'Brass', 'Bronze')"), `Find and replace → UPDATE … REPLACE (${nBrass} rows)`);
  // Browse shows types in the headers
  await page.click("button[data-a='ed-tab'][data-arg='browse']");
  await page.click("button[data-a='ed-tab'][data-arg='structure']");
  await shot("06g-structure");
  await page.click("button[data-a='ed-tab'][data-arg='insert']");
  await shot("06h-insert");
  await page.click("button[data-a='ed-tab'][data-arg='browse']");
  check((await page.locator("table.sheet thead .ty").allInnerTexts()).includes("123"), "sheet headers show column types");

  // Operations: rename, writers; database: a view and a scratch table
  await page.goto(`https://iq.test/#/ws/${dkey}/1`);
  await page.click("button[data-a='ed-tab'][data-arg='operations']");
  await page.fill("#op-name", "vendors");
  await page.click("button[data-a='op-rename']");
  await waitText("▦ vendors");
  check((await lastSql()) === "RENAME TABLE `suppliers` TO `vendors`", "Operations → RENAME TABLE");
  const extraW = Keypair.fromSeed(Buffer.alloc(32, 11)).publicKey.toBase58();
  await page.fill("#op-writer", extraW);
  await page.click("button[data-a='op-writer-add']");
  await page.waitForFunction(() => document.querySelectorAll(".writers li").length === 2);
  check((await lastSql()) === `GRANT INSERT ON \`vendors\` TO '${extraW}'`, "Operations → allow a wallet (GRANT INSERT)");
  await shot("06d-operations");
  await page.goto(`https://iq.test/#/ws/${dkey}`);
  await page.waitForSelector("button[data-a='ed-tab'][data-arg='operations']");
  check((await page.locator("nav.tabs2 button").allInnerTexts()).map((x) => x.trim()).join(",").startsWith("Structure,SQL,Search,Export,Import,Operations,Save"), "database tabs like phpMyAdmin");
  await openDetails("Create a view");
  await page.fill("#vw-name", "stainless_stock");
  await page.fill("#vw-sql", "SELECT material, SUM(qty) AS total FROM fasteners WHERE material LIKE '%stainless' GROUP BY material");
  await page.click("button[data-a='view-create']");
  await waitText("👁 stainless_stock");
  await shot("06e-database");
  out2 = await sql("SELECT * FROM stainless_stock ORDER BY material");
  const sums = {};
  for (const r of fastRows) if (r.material.endsWith("stainless")) sums[r.material] = (sums[r.material] || 0) + effQty(r);
  check(out2.includes(`18-8 stainless\t${sums["18-8 stainless"]}`) && out2.includes(`316 stainless\t${sums["316 stainless"]}`), "a view is queried like a table (SUM over 600 rows)");
  out2 = await sql("SELECT v.name, COUNT(f.part_no) AS parts FROM vendors v LEFT JOIN fasteners f ON f.supplier LIKE CONCAT(v.name, '%') GROUP BY v.name ORDER BY v.name");
  const bz = fastRows.filter((r) => r.supplier.startsWith("Brazos Bolt & Nut")).length, pm = fastRows.filter((r) => r.supplier.startsWith("Permian Industrial")).length;
  check(out2.includes(`Brazos Bolt & Nut\t${bz}`) && out2.includes(`Permian Industrial\t${pm}`), `JOIN + GROUP BY across saved and unsaved rows (${bz}, ${pm})`);
  out2 = await sql("CREATE TABLE scratch (id INT AUTO_INCREMENT PRIMARY KEY, note VARCHAR(20) NOT NULL); INSERT INTO scratch (note) VALUES ('a'), ('b'), ('c')");
  check((await sqlBad()).length === 0, "CREATE TABLE with types + INSERT");
  let progSrc = "// generated by the end-to-end test\n";
  { let y = 7; while (progSrc.length < 70000) { let line = "// "; for (let j = 0; j < 12; j++) { y = (y * 1103515245 + 12345) & 0x7fffffff; line += (y >> 4).toString(36).slice(-5); } progSrc += line + "\n"; } }
  out2 = await sql(`CREATE TABLE programs (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100) NOT NULL, source LONGTEXT); INSERT INTO programs (name, source) VALUES ('generator', '${progSrc}')`);
  check((await sqlBad()).length === 0, `a ${Math.round(progSrc.length / 1000)} KB program goes into one cell`);
  const progPda = iq.contract.getTablePda(new PublicKey(rootPda), iq.utils.toSeedBytes("programs"), PID).toBase58();
  await page.click("button[data-a='ed-tab'][data-arg='sql']");
  await shot("06f-sql");
  const dump = await download(async () => { await page.click("button[data-a='ed-tab'][data-arg='export']"); await page.click("button[data-a='export'][data-arg='db-sql']"); });
  check(dump.name === "e2e-parts.sql" && dump.text.includes("CREATE TABLE `vendors`") && dump.text.includes("`state` ENUM('TX','OK','NM') DEFAULT 'TX'") && dump.text.includes("CONSTRAINT `rating_range` CHECK") && dump.text.includes("CREATE OR REPLACE VIEW `stainless_stock`"), "Export → a MySQL-style .sql dump with types, keys, rules and views");
  await page.click("button[data-a='ed-tab'][data-arg='save']");
  const plan2 = (await text()).replace(/\s+/g, " ");
  check(plan2.includes("vendors: new structure") && plan2.includes("rename to “vendors”") && plan2.includes("change who can add rows") && plan2.includes("fasteners: new structure"), "Save lists structure, rename and writer changes");
  check(/programs: 1 changed row\(s\) in one write sent in \d+ parts \(IQ's chunked upload, session\)/.test(plan2), "the big row is planned as one write in parts (session): " + (plan2.match(/programs: [^·]*/) || [""])[0]);
  const sessBeforeSave = chain.sessionWrites || 0;
  const tuBefore = chain.tableUpdates || 0;
  chain.dropPosts = 2; // two parts vanish on the way (never land)
  chain.dropped = 0;
  chain.maxSendInflight = 0;
  await page.click(`button[data-a='inscribe'][data-arg='${dkey}']`);
  await waitText("Saved ✓", 90000);
  check(chain.dropped === 2 && chain.maxSendInflight >= 10, `the program's parts went out ${chain.maxSendInflight} at a time; 2 dropped parts were noticed and sent again before finishing`);
  check(!(await page.locator(".run").textContent()).includes("failed"), "saved without errors");
  const supMeta2 = accCoder.decode("Table", acct(supPda).data);
  check(Buffer.from(supMeta2.name).toString() === "vendors" && supMeta2.writers.map((w) => w.toBase58()).includes(extraW) && supMeta2.writers.length === 2, "update_table: the table's name and writers changed on chain (checked against the SDK builder)");
  check(supMeta2.column_names.map((c) => Buffer.from(c).toString()).join(",") === "id,p", "…keeping its on-chain columns");
  check((chain.tableUpdates || 0) === tuBefore + 1, "one update_table transaction");
  const isSchema = (r) => typeof r.p === "string" && /^IQT1[sS]/.test(r.p);
  check((chain.rows.get(supPda) || []).some(isSchema) && (chain.rows.get(fastPda) || []).some(isSchema), "structure records written to both tables (one small write each)");
  const iqtPda = iq.contract.getTablePda(new PublicKey(rootPda), iq.utils.toSeedBytes("_iqt"), PID).toBase58();
  const scratchPda = iq.contract.getTablePda(new PublicKey(rootPda), iq.utils.toSeedBytes("scratch"), PID).toBase58();
  check(!!acct(iqtPda) && !!acct(scratchPda), "views table and scratch table created");
  const progRows = chain.rows.get(progPda) || [];
  const progRow = progRows.find((r) => !/^IQT1[sS]/.test(r.p));
  check(!!progRow && progRow.__onChainPath && progRow.__onChainPath.length < 80 && (chain.sessionWrites || 0) === sessBeforeSave + 1, "the program was written as one chunked row through a session (the file's session had used the previous sequence number)");
  // TRUNCATE and DROP a saved table
  await page.click("button[data-a='ed-tab'][data-arg='structure']");
  await page.waitForSelector("tr:has-text('scratch') button[data-a='op-truncate']");
  await page.click("tr:has-text('scratch') button[data-a='op-truncate']");
  check((await page.locator("tr:has-text('scratch') button[data-a='op-truncate']").innerText()).includes("Click again"), "Empty asks for a second click");
  await page.click("tr:has-text('scratch') button[data-a='op-truncate']");
  await waitText("will be emptied");
  await page.click("button[data-a='ed-tab'][data-arg='save']");
  await page.click(`button[data-a='inscribe'][data-arg='${dkey}']`);
  await waitText("Saved ✓", 60000);
  await page.goto(`https://iq.test/#/t/${rootPda}/${scratchPda}`);
  await page.waitForFunction(() => /showing 0 record/.test(document.getElementById("app").innerText), null, { timeout: 20000 });
  check(true, "after TRUNCATE is saved, readers see the table empty");
  await page.goto(`https://iq.test/#/ws/${dkey}`);
  await page.waitForSelector("tr:has-text('scratch') button[data-a='op-drop']");
  await page.click("tr:has-text('scratch') button[data-a='op-drop']");
  await page.click("tr:has-text('scratch') button[data-a='op-drop']");
  await page.waitForFunction(() => ![...document.querySelectorAll(".side a.tb")].some((a) => a.textContent.includes("scratch")));
  await page.click("button[data-a='ed-tab'][data-arg='save']");
  check((await text()).includes("scratch: delete the table"), "Save lists the dropped table");
  await page.click(`button[data-a='inscribe'][data-arg='${dkey}']`);
  await waitText("Saved ✓", 60000);
  const root3 = accCoder.decode("DbRoot", acct(rootPda).data);
  const seeds3 = root3.table_seeds.map((x) => Buffer.from(x).toString());
  check(!seeds3.includes("scratch") && seeds3.includes("fasteners") && seeds3.includes("suppliers") && chain.listUpdates === 1, "DROP TABLE: update_db_root_table_list took it off the database's list (" + seeds3.join(",") + ")");
  await page.goto(`https://iq.test/#/db/${rootPda}`);
  await waitText("fasteners");
  check(!(await text()).includes("scratch"), "Explore no longer lists the dropped table");

  // checkpoint: fasteners has a history of edits by now
  const histBefore = (chain.rows.get(fastPda) || []).length;
  await page.goto(`https://iq.test/#/ws/${dkey}/0`);
  await page.waitForSelector("table.sheet");
  await page.click("button[data-a='ed-tab'][data-arg='operations']");
  await waitText("Checkpoint on next save");
  const opsText = (await text()).replace(/\s+/g, " ");
  check(/replays its whole history: \d+ write\(s\) on the blockchain for 600 row\(s\)/.test(opsText), `Operations shows the table's history: ` + (opsText.match(/Opening this[^.]*\./) || [""])[0]);
  await page.click("button[data-a='op-checkpoint'][data-arg='on']");
  await waitText("checkpoint on next save");
  check((await lastSql()) === "OPTIMIZE TABLE `fasteners`", "Checkpoint → OPTIMIZE TABLE");
  await page.click("button[data-a='ed-tab'][data-arg='save']");
  const plan3 = (await text()).replace(/\s+/g, " ");
  check(plan3.includes("fasteners: checkpoint") && /fasteners: 600 row\(s\) rewritten/.test(plan3), "Save lists the checkpoint: " + (plan3.match(/fasteners: 600[^·]*/) || [""])[0]);
  await page.click(`button[data-a='inscribe'][data-arg='${dkey}']`);
  await waitText("Saved ✓", 90000);
  const hist = chain.rows.get(fastPda) || [];
  check(hist.length === histBefore + 2 && /^IQT1[sS]/.test(hist[hist.length - 1].p) && !!hist[hist.length - 2].__onChainPath, "checkpoint on chain: the whole table as one chunked pack, then the checkpoint record");

  // a clean browser reads the structure back from the chain
  {
    const ctx2 = await browser.newContext({ viewport: { width: 1280, height: 900 }, acceptDownloads: true });
    const p2 = await ctx2.newPage();
    p2.on("pageerror", (e) => consoleErrors.push("pageerror(2): " + e.message));
    await p2.route("**/*", handler);
    await p2.goto(`https://iq.test/#/t/${rootPda}/${supPda}`);
    await p2.waitForSelector("table.data td:has-text('Permian Industrial')", { timeout: 20000 });
    const h2 = await p2.locator("table.data thead").innerText();
    const r2 = (await p2.locator("table.data tbody tr:has-text('Permian Industrial')").innerText()).replace(/\s+/g, " ");
    const b2 = (await p2.locator("table.data tbody tr:has-text('Brazos')").innerText()).replace(/\s+/g, " ");
    check(/town/i.test(h2) && /rating/i.test(h2) && !/city/i.test(h2), "Explore shows the renamed and added columns from the structure record");
    check(r2.includes("Odessa") && r2.includes("TX") && / 4 /.test(" " + r2 + " ") && / 3 /.test(" " + b2 + " "), "…with defaults applied to rows saved before the change");
    check((await p2.locator("#app").innerText()).includes("vendors"), "…and the table's new name");
    await p2.click("td:has-text('Permian Industrial')");
    await p2.click("button:has-text('Edit in the Editor')");
    await p2.waitForFunction(() => location.hash.startsWith("#/ws/"));
    await p2.waitForSelector("table.sheet");
    await p2.click("button[data-a='ed-tab'][data-arg='structure']");
    await p2.waitForSelector("table.struct");
    const st2 = (await p2.locator("#app").innerText()).replace(/\s+/g, " ");
    check(st2.includes("Choice: TX, OK, NM") && st2.includes("rating_range") && st2.includes("web") && st2.includes("Whole number"), "the editor rebuilds types, keys and rules from the chain");
    check(await p2.waitForSelector(".side button[data-a='view-open']:has-text('stainless_stock')", { timeout: 10000 }).then(() => true).catch(() => false), "…and the database's views");
    const sql2 = async (q) => {
      await p2.click("button[data-a='ed-tab'][data-arg='sql']");
      await p2.fill("textarea[data-keys='sql']", q);
      await p2.click("button[data-a='sql-run']");
      await p2.waitForFunction(() => { const o = document.querySelector(".sqlout"); return o && /row|error|No /i.test(o.innerText); }, null, { timeout: 30000 });
      await p2.waitForTimeout(300);
      return p2.locator(".sqlout").innerText();
    };
    await p2.click(".side a.tb:has-text('fasteners')");
    await p2.click("button[data-a='ed-tab'][data-arg='browse']");
    await p2.waitForSelector("table.sheet", { timeout: 30000 });
    const f2 = await sql2("SELECT COUNT(*) AS n, SUM(qty = 9999) AS edited, SUM(material = 'Bronze') AS bronze, SUM(part_no = 'FST-EDITED') AS renamed FROM fasteners");
    check(f2.includes(`600\t1\t${nBrass}\t1`), `from the checkpoint: 600 rows with every earlier edit (${f2.split("\n").slice(1, 2).join(" ")})`);
    await p2.click("button[data-a='ed-tab'][data-arg='operations']");
    const ck2 = (await p2.locator("#app").innerText()).replace(/\s+/g, " ");
    check(/opens from its last checkpoint: \d+ write\(s\) read for 600 row\(s\)/.test(ck2), "the editor found the checkpoint and reads fasteners from it: " + (ck2.match(/(This table opens|Opening this table)[^.]*\./) || [""])[0]);
    await p2.click(".side a.tb:has-text('programs')");
    await p2.click("button[data-a='ed-tab'][data-arg='browse']");
    await p2.waitForSelector("table.sheet", { timeout: 30000 });
    const pr2 = await sql2("SELECT name, LENGTH(source) AS len FROM programs");
    check(pr2.includes(`generator\t${progSrc.length}`), `the ${progSrc.length}-character program reads back from its session chunks`);
    await ctx2.close();
  }
  await shot("06b-structure");

  console.log("Unofficial contribution from a second person");
  await menu("Sign out");
  await page.goto("https://iq.test/#/account");
  await page.fill("#keys-text", `other: ${bs58.encode(otherKp.secretKey)}`);
  await page.click("button[data-a='import-keys-text']");
  const oa = otherKp.publicKey.toBase58();
  await waitText(`Signed in as ${solana_short(oa)}`);
  await page.goto("https://iq.test/#/account");
  await openDetails("Advanced");
  check((await addrOf("other")) === oa, "pasted \"label: base58-key\" signs in as that wallet");
  await page.goto(`https://iq.test/#/ws/${dkey}`);
  await page.click("button[data-a='ed-tab'][data-arg='operations']");
  await page.click("button[data-a='del-draft-confirm']");
  await page.click("button[data-a='del-draft-confirm']");
  await page.waitForFunction(() => location.hash === "#/ws");
  await page.goto("https://iq.test/#/ws");
  await page.fill("#newdb", "e2e-parts");
  await page.press("#newdb", "Enter");
  await waitText("already belongs to someone else");
  check(true, "the editor says the name belongs to someone else");
  await page.click("button[data-a='ed-tab'][data-arg='save']");
  await openDetails("Advanced: this database");
  await openDetails("Use a different wallet");
  await page.selectOption("select[data-in='draft-wallet']", oa);
  out = await sql("DROP TABLE sheet1; CREATE TABLE fasteners (part_no PRIMARY KEY, name, note); INSERT INTO fasteners VALUES ('FST-900001', 'Community-submitted washer', NULL); COMMIT");
  check(/Save to the blockchain|Saving|Checking your balance/.test(out) && (await page.locator("button[data-a='ed-tab'][data-arg='save'].on").count()) === 1, "SQL: COMMIT saves (and shows the progress)");
  await waitText("Saved ✓", 60000);
  check((await page.locator(".run").textContent()).includes("unofficial contributions"), "contributor mode explained in the run log");
  check((chain.reallocs || 0) >= 3 && acct(iq.contract.getUserInventoryPda(otherKp.publicKey, PID).toBase58()).data.length === 4213, "pre-upgrade IQ accounts grown before the first v1 write (like the SDK)");
  const supBefore = (chain.rows.get(supPda) || []).length;
  out = await sql("CREATE TABLE suppliers (name PRIMARY KEY, city, state); INSERT INTO suppliers VALUES ('Spam Co', 'x', 'y'); COMMIT");
  await waitText("Simulation rejected", 30000);
  check((chain.rows.get(supPda) || []).length === supBefore, "a write to a locked table is rejected at simulation; nothing sent");
  await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
  await waitText("Unofficial");
  await page.click("button:has-text('Unofficial')");
  await waitText("Community-submitted washer");
  const t6 = (await text()).replace(/\s+/g, " ");
  check(/Unofficial 1/.test(t6), "explorer counts 1 unofficial record");
  check(/Official 600/.test(t6), "official records unaffected (600)");
  await shot("07-unofficial");

  console.log("Live links to IQ git repositories");
  {
    const k3 = (await page.evaluate(() => JSON.parse(localStorage.getItem("iqtables:v1:drafts"))))[0].key;
    await page.goto(`https://iq.test/#/ws/${k3}`);
    await page.waitForSelector("button[data-a='ed-tab'][data-arg='sql']");
    const link = `https://browser.iqlabs.dev/${GIT.pda}`;
    out = await sql(`CREATE TABLE software (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(80) NOT NULL, repo VARCHAR(200)); INSERT INTO software (name, repo) VALUES ('Hello IQ', '${link}')`);
    check((await sqlBad()).length === 0, "a table of software with IQ's browser link to a repository");
    await sql("SELECT name, repo FROM software");
    const gitBtn = page.locator(".sqlout button[data-a='git-open']");
    await gitBtn.waitFor({ timeout: 10000 });
    await page.waitForFunction(() => /first version/.test((document.querySelector(".sqlout button[data-a='git-open']") || {}).title || ""), null, { timeout: 10000 });
    check((await gitBtn.innerText()).includes("hello-iq"), "the cell shows the repository, and its newest commit on hover: " + (await gitBtn.innerText()).replace(/\s+/g, " "));
    // the project moves on; the table doesn't change
    const c2 = gitCommit("second version", { "index.html": "<h1>v2</h1>", "README.md": "# hello-iq\nnow with v2\n", "app.js": "console.log('v2')\n" });
    gitCommit("totally the latest", { "index.html": "<h1>spoofed</h1>" }, new PublicKey(Buffer.alloc(32, 12)).toBase58());
    await gitBtn.click();
    await page.waitForSelector(".gitpanel");
    await page.click(".gitpanel button[data-a='git-refresh']");
    await page.waitForFunction(() => /second version/.test(document.querySelector(".gitpanel .commits").innerText), null, { timeout: 10000 });
    const commits = await page.locator(".gitpanel .commits").innerText();
    check(/second version\s*latest/.test(commits) && !commits.includes("totally the latest"), "a new commit shows up as latest; a commit inscribed by someone else is ignored");
    await page.waitForSelector(".gitpanel button[data-a='open-tx'][data-val='app.js']", { timeout: 10000 });
    check((await page.locator(".gitpanel .files li").count()) === 3, "the newest commit's files are listed (from its tree inscription)");
    await shot("07b-git-panel");
    await page.click(".gitpanel button[data-a='open-tx'][data-val='index.html']");
    await page.waitForFunction(() => /<h1>v2<\/h1>/.test((document.querySelector(".modal pre") || {}).innerText || ""), null, { timeout: 10000 });
    check((await page.locator(".modal h3").last().innerText()).includes("index.html"), "a file of the commit opens as text (IQ git's base64 blob decoded)");
    await page.click("button[data-a='viewer-close']");
    await page.click(".gitpanel button[data-a='git-commit']:has-text('first version')");
    await page.waitForSelector(".gitpanel .files li:nth-child(2)", { timeout: 10000 });
    const pin = (await page.locator(".gitpanel code").innerText()).trim();
    check(pin.startsWith(`iq://tx/${GIT.trees[0]}#hello-iq@`) && (await page.locator(".gitpanel .files li").count()) === 2, "an older commit shows its own files and a link that pins that version: " + pin);
    await page.click("button[data-a='git-close']");
    await page.waitForFunction(() => /second version/.test((document.querySelector(".sqlout button[data-a='git-open']") || {}).title || ""), null, { timeout: 10000 });
    const cellNow = await sql("SELECT repo FROM software");
    const tipNow = (await page.locator(".sqlout button[data-a='git-open']").first().getAttribute("title")) || "";
    check(tipNow.includes("second version") && cellNow.includes("hello-iq") && (await sql("SELECT COUNT(*) AS n FROM software WHERE repo = '" + link + "'")).includes("1"), "the cell now shows the new commit; its value is still the same link");
    out = await sql(`INSERT INTO software (name, repo) VALUES ('Hello IQ v1', '${pin}'); SELECT name, repo FROM software WHERE name = 'Hello IQ v1'`);
    await page.click(".sqlout button[data-a='open-tx']");
    await page.waitForSelector(".modal .files li", { timeout: 10000 });
    check((await page.locator(".modal .files").innerText()).includes("README.md"), "the pinned link opens that commit's file list");
    await page.click("button[data-a='viewer-close']");
    await sql("DROP TABLE software");
    void c2;
  }

  console.log("Embeds: snapshots, and live reads with the decoder");
  {
    // the official wallet: the database's creator
    const fa = accCoder.decode("DbRoot", acct(rootPda).data).creator.toBase58();
    const decSig = DEC.first.files["iqt-decoder.wasm"].txId;
    await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
    await page.waitForFunction(() => /Official\s*600/.test(document.getElementById("app").innerText), null, { timeout: 20000 });
    await page.click("button[data-a='embed-open']");
    await page.waitForSelector(".embedpanel textarea.snapshot", { timeout: 30000 });
    const panel = () => page.locator(".embedpanel").innerText();
    const snap = await download(() => page.click(".embedpanel button[data-a='embed-dl'][data-arg='snapshot']"));
    await page.click(".embedpanel button[data-a='embed-close']");
    // (an earlier section left this table's filter on unofficial rows; revisiting keeps it)
    await page.click("button[data-a='tv-who'][data-arg='official']");
    const tvCsv = await download(() => page.click("button[data-a='tv-export'][data-arg='csv']"));
    await page.click("button[data-a='embed-open']");
    await page.waitForSelector(".embedpanel textarea.snapshot", { timeout: 30000 });
    const lines = snap.text.trimEnd().split("\n");
    check(snap.name === "fasteners.csv" && lines.length === 601 && (await panel()).includes("600 rows"), `Snapshot: a CSV of the 600 official records (${lines.length - 1} rows)`);
    check(snap.text === tvCsv.text, "the snapshot is exactly the explorer's CSV export of the official rows");
    check((await page.locator(".embedpanel textarea.snapshot").inputValue()).startsWith(lines[0] + "\n"), "the dialog previews it");
    await page.selectOption(".embedpanel select[data-arg='who']", "all");
    await page.waitForFunction(() => /601 rows/.test(document.querySelector(".embedpanel").innerText), null, { timeout: 30000 });
    await page.selectOption(".embedpanel select[data-arg='format']", "json");
    const jd = await download(() => page.click(".embedpanel button[data-a='embed-dl'][data-arg='snapshot']"));
    const arr = JSON.parse(jd.text);
    check(jd.name === "fasteners.json" && arr.length === 601 && jd.text.includes("Community-submitted washer"), "Everyone's rows as JSON: 601, the community row included");
    await page.selectOption(".embedpanel select[data-arg='format']", "html");
    const hd = await download(() => page.click(".embedpanel button[data-a='embed-dl'][data-arg='snapshot']"));
    check(hd.name === "fasteners.html" && hd.text.includes('<table class="iq-table">') && (hd.text.match(/<tr>/g) || []).length === 602, "an HTML table to paste into a page (601 rows + header)");
    await page.selectOption(".embedpanel select[data-arg='who']", "official");
    await page.click(".embedpanel button[data-a='embed-tab'][data-arg='live']");
    await page.waitForSelector(".embedpanel input[data-arg='repo']");
    check((await panel()).includes("isn't running from IQ's browser"), "Live, off IQ's browser: the dialog asks which repository holds the decoder");
    await page.fill(".embedpanel input[data-arg='repo']", `https://browser.iqlabs.dev/${DEC.pda}`);
    await page.press(".embedpanel input[data-arg='repo']", "Enter");
    await page.waitForSelector(".embedpanel pre[data-snippet='server']", { timeout: 20000 });
    const server = await page.locator(".embedpanel pre[data-snippet='server']").innerText();
    check(server.includes(`table: "${fastPda}"`) && server.includes(`official: "${fa}"`) && server.includes(`rows: "official"`) && server.includes(`decoder: { repo: "${DEC.pda}", owner: "${DEC.owner}" }`) && !server.includes("pin:"), "the server snippet: the table, its official wallet, and the newest decoder in the repository");
    check((await panel()).includes("iqt-decoder.wasm in iq-tables"), "the dialog shows which decoder it found");
    await page.click(".embedpanel button[data-a='embed-pin'][data-arg='1']");
    check((await page.locator(".embedpanel pre[data-snippet='server']").innerText()).includes(`pin: "${decSig}"`), "Pin this version: the snippet pins the decoder's inscription");
    const web = await page.locator(".embedpanel pre[data-snippet='web']").innerText();
    check(web.includes('<script type="module">') && web.includes("renderTable(") && web.includes(`pin: "${decSig}"`), "a web-page snippet too");
    const ld = await download(() => page.click(".embedpanel button[data-a='embed-dl'][data-arg='loader']"));
    check(ld.name === "iqt-loader.mjs" && ld.text === fs.readFileSync(path.join(ROOT, "embed", "iqt-loader.mjs"), "utf8"), "Download iqt-loader.mjs: the one file developers keep");
    const [wd] = await Promise.all([page.waitForEvent("download"), page.click(".embedpanel button[data-a='embed-dl'][data-arg='decoder']")]);
    check(wd.suggestedFilename() === `iqt-decoder-${decSig.slice(0, 8)}.wasm` && fs.readFileSync(await wd.path()).equals(DEC.wasm), "Download the decoder file: the deployed decoder, byte for byte");
    await shot("09c-embed-live");
    // the iframe: code to paste, and the bare view it shows
    await page.click(".embedpanel button[data-a='embed-tab'][data-arg='frame']");
    const frameCode = () => page.locator(".embedpanel pre[data-snippet='iframe']").innerText();
    check((await frameCode()).includes(`src="https://iq.test/#/embed/${rootPda}/${fastPda}?rows=official"`), "Iframe: code pointing at this site's bare view of the table");
    await page.selectOption(".embedpanel select[data-arg='every']", "300");
    check((await frameCode()).includes("?rows=official&amp;every=300") || (await frameCode()).includes("?rows=official&every=300"), "…optionally read again every 5 minutes");
    await page.click(".embedpanel button[data-a='embed-close']");
    await page.goto(`https://iq.test/#/embed/${rootPda}/${fastPda}?rows=official`);
    await page.waitForFunction(() => /600 rows/.test(document.getElementById("app").innerText), null, { timeout: 30000 });
    const fv = await page.evaluate(() => ({ header: document.querySelectorAll("header").length, rows: document.querySelectorAll(".frameview tbody tr").length, pager: document.querySelector(".framefoot").innerText, credit: document.querySelector(".framecredit a").getAttribute("href") }));
    check(fv.header === 0 && fv.rows === 50 && /1 \/ 12/.test(fv.pager) && fv.credit === `https://iq.test/#/t/${rootPda}/${fastPda}`, "the bare view: no header, 50 rows a page of 600, a link back to the table");
    await page.click(".framefoot button[data-arg='1']");
    check(/2 \/ 12/.test(await page.locator(".framefoot").innerText()), "…with paging");
    await shot("09d-embed-view");
    await page.goto(`https://iq.test/#/embed/${rootPda}/${fastPda}?rows=all`);
    await page.waitForFunction(() => /601 rows/.test(document.getElementById("app").innerText), null, { timeout: 30000 });
    check(true, "rows=all shows everyone's rows (601)");
    {
      const p4 = await ctx.newPage();
      await p4.route("**/*", handler);
      chain.hostPage = `<!doctype html><h1>My parts site</h1><iframe id="f" src="https://iq.test/#/embed/${rootPda}/${fastPda}?rows=official" width="800" height="520" style="border:0"></iframe>`;
      await p4.goto("https://host.test/");
      const frame = p4.frameLocator("#f");
      await frame.locator(".framefoot").waitFor({ timeout: 30000 });
      const inFrame = await frame.locator(".frameview tbody tr").count();
      check(inFrame === 50 && (await frame.locator(".framefoot").innerText()).includes("600 rows"), "…and it works inside another site's iframe");
      await p4.close();
    }
    await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
    await page.waitForFunction(() => /Official\s*600/.test(document.getElementById("app").innerText), null, { timeout: 20000 });
    // served by IQ's browser, the page knows its own repository (another tab: this one stays signed in)
    {
      const p3 = await ctx.newPage();
      await p3.route("**/*", handler);
      await p3.goto(`https://browser.iqlabs.dev/${DEC.pda}#/t/${rootPda}/${fastPda}`);
      await p3.waitForSelector("button[data-a='embed-open']", { timeout: 20000 });
      await p3.click("button[data-a='embed-open']");
      await p3.click(".embedpanel button[data-a='embed-tab'][data-arg='live']");
      await p3.waitForSelector(".embedpanel pre[data-snippet='server']", { timeout: 20000 });
      check((await p3.locator(".embedpanel input[data-arg='repo']").count()) === 0 && (await p3.locator(".embedpanel pre[data-snippet='server']").innerText()).includes(`repo: "${DEC.pda}"`), "served from IQ's browser, the dialog finds the decoder in the repository the site was deployed from");
      await p3.close();
    }

    // the loader and decoder, outside the app (Node)
    const L = await import(require("url").pathToFileURL(path.join(ROOT, "embed", "iqt-loader.mjs")).href);
    const seen = [];
    const down = { gateway: false, repo: false, data: false };
    const mf = async (u, init = {}) => {
      seen.push(u);
      if (u.startsWith("https://gateway.iqlabs.dev/")) {
        if (down.gateway) throw new TypeError("fetch failed");
        if (down.repo && u.includes(`/table/${DEC.pda}/`)) return new Response("unavailable", { status: 503 });
        if (down.data && u.includes("/data/")) return new Response("unavailable", { status: 503 });
        const res = gateway(u, init.method || "GET", init.body);
        return new Response(JSON.stringify(res), { status: res.error ? 404 : 200 });
      }
      if (u.startsWith("https://rpc.test/")) return new Response(JSON.stringify(rpc(init.body)), { status: 200 });
      throw new TypeError("no route to " + u);
    };
    const base = { table: fastPda, official: fa, fetch: mf, store: null, decoder: { repo: DEC.pda, owner: DEC.owner } };
    const fail = async (o) => { try { await L.readTable({ ...base, cacheMs: 0, ...o }); return "no error"; } catch (e) { return e.message; } };
    const r1 = await L.readTable({ ...base, format: "csv" });
    check(r1.data === snap.text && r1.count === 600 && r1.source === "gateway" && r1.decoderRef === decSig, `Node: readTable gives the snapshot's CSV exactly (600 rows), with the decoder ${decSig.slice(0, 8)}… from IQ git`);
    const n1 = seen.length;
    await L.readTable({ ...base, format: "csv" });
    check(seen.length === n1, "a second read within cacheMs makes no requests");
    const all = await L.readTable({ ...base, rows: "all", format: "json", cacheMs: 0 });
    check(JSON.parse(all.data).length === 601 && all.data.includes("Community-submitted washer"), "rows \"all\" as JSON: 601, the community row included");
    L.clearCache();
    seen.length = 0;
    const r2 = await L.readTable({ ...base, decoder: { pin: decSig }, format: "csv" });
    check(r2.data === r1.data && !seen.some((u) => u.includes(`/table/${DEC.pda}/`)), "a pinned decoder loads by its inscription, without looking at the repository");
    L.clearCache();
    seen.length = 0;
    const r3 = await L.readTable({ ...base, decoder: { wasm: fs.readFileSync(path.join(ROOT, "site", "iqt-decoder.wasm")) }, format: "csv" });
    check(r3.data === r1.data && !seen.some((u) => u.includes("/data/")), "a local copy of the decoder (hosts that can't compile downloaded code) needs no download");
    down.gateway = true;
    const r4 = await L.readTable({ ...base, decoder: { wasm: DEC.wasm }, rpc: "https://rpc.test/", format: "csv", cacheMs: 0 });
    down.gateway = false;
    const sorted = (t) => t.trimEnd().split("\n").sort().join("\n");
    check(r4.source === "solana" && sorted(r4.data) === sorted(r1.data) && r4.notes.some((n) => n.includes("gateway")), "IQ's gateway down: the rows are rebuilt from Solana (a chunked linked-list write included), same CSV");
    const r5 = await L.readTable({ table: progPda, official: fa, fetch: mf, store: null, decoder: { wasm: DEC.wasm }, rpc: "https://rpc.test/", source: "solana", cacheMs: 0 });
    check(r5.rows.some((row) => row.includes("generator") && row.includes(progSrc)), `source "solana": a ${Math.round(progSrc.length / 1000)} KB row sent through a session is rebuilt from its parts`);
    check((await fail({ decoder: { wasm: fakeDecoder({ done: {} }, { imports: true }) } })).includes("refusing a decoder"), "a decoder that asks for anything outside itself is refused");
    check((await fail({ decoder: { wasm: fakeDecoder({ done: {} }, { abi: 2 }) } })).includes("ABI 2"), "a decoder with another interface version is refused");
    check((await fail({ decoder: { wasm: fakeDecoder({ done: {} }, { capped: false }) } })).includes("capped at 1 GiB"), "a decoder whose memory isn't capped is refused before it runs");
    check((await fail({ decoder: { wasm: fakeDecoder({ done: {} }, { memories: 4 }) } })).includes("exactly one memory"), "a decoder with several memories (a way around the cap) is refused");
    const write = await fail({ rpc: "https://rpc.test/", decoder: { wasm: fakeDecoder({ fetch: [{ url: "https://rpc.invalid/", method: "POST", body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "sendTransaction", params: ["AAAA"] }) }] }) } });
    check(write.includes("isn't a read"), "a decoder can't send transactions (or any call but reads) through your RPC");
    const post = await fail({ decoder: { wasm: fakeDecoder({ fetch: [{ url: `https://gateway.iqlabs.dev/table/${fastPda}/notify`, method: "POST", body: "{}" }] }) } });
    check(post.includes("refused a request"), "a decoder can only GET from the gateway");
    seen.length = 0;
    const snoop = await fail({ decoder: { wasm: fakeDecoder({ fetch: [{ url: "http://169.254.169.254/latest/meta-data/", method: "GET" }] }) } });
    check(snoop.includes("refused a request to http://169.254.169.254") && !seen.some((u) => u.includes("169.254")), "a decoder asking for any host but the gateway or RPC gets nothing");
    const futPda = new PublicKey(Buffer.alloc(32, 21)).toBase58();
    chain.rows.set(futPda, [{ id: "old1", p: "IQT9 written long ago", __txSignature: fakeSig(), __signer: fa, __blockTime: 1700000000 }]);
    const r6 = await L.readTable({ ...base, table: futPda, cacheMs: 0 });
    check(r6.rows.length === 1 && r6.rows[0][0] === "FST-IQT9", "a pack in a retired format goes to the older decoder that iqt-formats.json lists for it");
    const mem = new Map();
    const store = { get: (k) => mem.get(k) ?? null, set: (k, v) => { mem.set(k, v); } };
    L.clearCache();
    await L.readTable({ ...base, store, cacheMs: 0 });
    L.clearCache();
    down.data = true;
    down.repo = true;
    const r7 = await L.readTable({ ...base, store, cacheMs: 0 });
    down.data = false;
    down.repo = false;
    check(r7.count === 600 && [...mem.keys()].some((k) => k.startsWith("iqt-decoder-")), "with a store, a restarted server runs its saved decoder while IQ's gateway can't serve it");
    const loaderSrc = fs.readFileSync(path.join(ROOT, "embed", "iqt-loader.mjs"), "utf8");
    const rendered = await page.evaluate(async ([src, opts]) => {
      const m = await import("data:text/javascript;base64," + btoa(unescape(encodeURIComponent(src))));
      const el = document.createElement("div");
      document.body.appendChild(el);
      await m.renderTable(el, opts);
      const out = { rows: el.querySelectorAll("tbody tr").length, cols: el.querySelectorAll("thead th").length, stored: Object.keys(localStorage).filter((k) => k.startsWith("iqt-")).length };
      el.remove();
      return out;
    }, [loaderSrc, { table: fastPda, official: fa, decoder: { repo: DEC.pda, owner: DEC.owner } }]);
    check(rendered.rows === 600 && rendered.cols > 1 && rendered.stored >= 2, `In a web page: renderTable draws the 600 rows; the decoder is kept in localStorage (${rendered.stored} entries)`);
    // the command line: in-process for reads (through the mock), as a program for encode/decode
    {
      const outs = [];
      const errs = [];
      const io = { fetch: mf, stdout: (t) => outs.push(t), stderr: (t) => errs.push(t) };
      const code = await L.cli(["read", fastPda, "--official", fa, "--decoder", path.join(ROOT, "site", "iqt-decoder.wasm")], io);
      check(code === 0 && outs.join("") === snap.text && errs.join("").includes("600 row(s)"), "Command line: `read` prints the same CSV as the snapshot");
      const tmp = fs.mkdtempSync(path.join(require("os").tmpdir(), "iqt-cli-"));
      fs.copyFileSync(path.join(ROOT, "site", "iqt-loader.mjs"), path.join(tmp, "iqt-loader.mjs"));
      fs.copyFileSync(path.join(ROOT, "site", "iqt-decoder.wasm"), path.join(tmp, "iqt-decoder.wasm"));
      const run = (args, input) => require("child_process").spawnSync(process.execPath, [path.join(tmp, "iqt-loader.mjs"), ...args], { cwd: tmp, input, encoding: "utf8" });
      fs.writeFileSync(path.join(tmp, "parts.csv"), snap.text);
      const enc = run(["encode", "parts.csv", "--out", "row.json"]);
      const row = JSON.parse(fs.readFileSync(path.join(tmp, "row.json"), "utf8"));
      check(enc.status === 0 && row.p.startsWith("IQT1z") && row.p.length * 3 < snap.text.length && /600 record\(s\)/.test(enc.stderr), `\`encode\` packs the 600-row CSV into one row (${snap.text.length} → ${row.p.length} bytes), using the decoder next to it`);
      const dec = run(["decode", "row.json", "--format", "csv"]);
      check(dec.status === 0 && dec.stdout === snap.text, "`decode` gives the same CSV back");
      const piped = run(["encode", "-", "--mode", "plain"], snap.text);
      check(piped.status === 0 && JSON.parse(piped.stdout).p.startsWith("IQT1j"), "…from standard input, and plain (readable) when asked");
      const onChain = (chain.rows.get(fastPda) || []).find((r) => r.__signer === fa && typeof r.p === "string" && r.p.startsWith("IQT1"));
      const fromChain = run(["decode", JSON.stringify({ id: onChain.id, p: onChain.p })]);
      check(fromChain.status === 0 && JSON.parse(fromChain.stdout).length > 0, `\`decode\` reads a pack straight from the table (${JSON.parse(fromChain.stdout).length} records)`);
      const bad = run(["decode", "nonsense"]);
      check(bad.status === 1 && bad.stderr.includes("isn't an IQ Tables pack"), "…and says so when given something else");
      const help = run([]);
      check(help.status === 2 && help.stdout.includes("encode") && help.stdout.includes("decode"), "running it bare prints how to use it");
      const msgs = [
        { jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "t", version: "1" } } },
        { jsonrpc: "2.0", method: "notifications/initialized" },
        { jsonrpc: "2.0", id: 2, method: "tools/list" },
        { jsonrpc: "2.0", id: 3, method: "tools/call", params: { name: "encode_rows", arguments: { csv: "id,name\n1,bolt\n2,nut\n" } } },
      ];
      const stdio = run(["mcp"], msgs.map((m) => JSON.stringify(m)).join("\n") + "\n");
      const replies = stdio.stdout.trim().split("\n").map((l) => JSON.parse(l));
      check(stdio.status === 0 && replies.length === 3 && replies[0].result.protocolVersion === "2024-11-05" && replies[1].result.tools.some((x) => x.name === "sql") && JSON.parse(replies[2].result.content[0].text).records === 2,
        "`mcp` speaks the Model Context Protocol over stdin and stdout, one message per line");
      fs.rmSync(tmp, { recursive: true, force: true });
    }
    // ---- the app from the command line: sql, write, MCP (Node, the mock chain)
    {
      // the site's copy of the loader: the app (index.html) is next to it
      const L = await import(require("url").pathToFileURL(path.join(ROOT, "site", "iqt-loader.mjs")).href);
      const run = async (args, stdin) => {
        const outs = [];
        const errs = [];
        // as `node iqt-loader.mjs` runs it: an error is printed, exit code 1
        const code = await L.cli(args, { fetch: mf, stdout: (t) => outs.push(t), stderr: (t) => errs.push(t), ...(stdin !== undefined ? { stdin } : {}) }).catch((e) => {
          errs.push(`${(e && e.message) || e}\n`);
          return 1;
        });
        return { code, out: outs.join(""), err: errs.join("") };
      };
      const net = ["--rpc", "https://rpc.test/"];
      const sends = () => [...chain.txs.keys()].length;
      if (typeof fastPda === "string" && acct(rootPda)) {
        const recs = JSON.parse((await L.readTable({ table: fastPda, official: fa, fetch: mf, store: null, decoder: { wasm: fs.readFileSync(path.join(ROOT, "site", "iqt-decoder.wasm")) }, format: "json", cacheMs: 0 })).data);
        const c1 = await run(["sql", "SELECT COUNT(*) AS n FROM fasteners", "--db", "e2e-parts", ...net]);
        check(c1.code === 0 && c1.out === `n\n${recs.length}\n`, `sql: COUNT(*) over the saved table, read by the app itself (${c1.out.trim().split("\n").pop()} rows)`);
        const byMat = {};
        for (const r of recs) byMat[r.material] = (byMat[r.material] || 0) + 1;
        const want = Object.keys(byMat).sort().map((m) => ({ material: m, n: byMat[m] }));
        const c2 = await run(["sql", "SELECT material, COUNT(*) AS n FROM fasteners GROUP BY material ORDER BY material", "--db", "e2e-parts", "--format", "json", ...net]);
        const got = c2.code === 0 ? JSON.parse(c2.out) : null;
        check(!!got && JSON.stringify(got.map((g) => ({ material: g.material, n: Number(g.n) }))) === JSON.stringify(want), "sql: GROUP BY gives the same counts as the decoder's records, as JSON");
        const c3 = await run(["sql", "SELECT part_no, qty FROM fasteners ORDER BY qty DESC LIMIT 2", "--db", "e2e-parts", "--format", "table", ...net]);
        check(c3.code === 0 && /^part_no\s+qty\n-+\s+-+\n/.test(c3.out) && c3.out.trim().split("\n").length === 4, "sql --format table: aligned columns for people");
        down.gateway = true;
        const outage = await run(["sql", "SELECT COUNT(*) FROM fasteners", "--db", "e2e-parts", ...net]);
        down.gateway = false;
        check(outage.code === 1 && /Couldn't read a table/.test(outage.err), "a table that can't be read fails the statement with the reason (no hang)");
      }
      // a wallet of the command line's own: a Solana key file
      const kp = Keypair.fromSeed(Buffer.alloc(32, 21));
      const me = kp.publicKey.toBase58();
      credit(chain, me, 2 * LAMPORTS);
      const tmp = fs.mkdtempSync(path.join(require("os").tmpdir(), "iqt-app-"));
      const keyFile = path.join(tmp, "wallet.json");
      fs.writeFileSync(keyFile, JSON.stringify(Array.from(kp.secretKey)));
      const parts = "id,name,qty\n1,Hex bolt,40\n2,Nut,15\n3,Washer,100\n";
      fs.writeFileSync(path.join(tmp, "parts.csv"), parts);
      const before = sends();
      const dry = await run(["write", "cli-shop", "parts", path.join(tmp, "parts.csv"), "--create", ...net]);
      check(dry.code === 0 && /Imported 3 rows/.test(dry.err) && /To save in cli-shop \(a new database\): parts: new table, 3 row\(s\) added/.test(dry.err) && /Not saved \(a dry run\)/.test(dry.err) && sends() === before,
        "write without --yes: what saving would do and cost, and nothing sent");
      const w = await run(["write", "cli-shop", "parts", path.join(tmp, "parts.csv"), "--create", "--key", keyFile, "--yes", ...net]);
      const shopRoot = iq.contract.getDbRootPda(Buffer.from("cli-shop"), PID).toBase58();
      const partsPda = iq.contract.getTablePda(new PublicKey(shopRoot), iq.utils.toSeedBytes("parts"), PID).toBase58();
      const root = acct(shopRoot) && accCoder.decode("DbRoot", acct(shopRoot).data);
      check(w.code === 0 && /Saved\. Spent/.test(w.err) && !!root && root.creator.toBase58() === me && !!acct(partsPda),
        `write --create --key --yes: the database and table made and the rows inscribed, signed with the key file's wallet (${sends() - before} transactions)`);
      if (w.code !== 0) console.log(w.err);
      const dec = path.join(ROOT, "site", "iqt-decoder.wasm");
      const back = await run(["read", partsPda, "--official", me, "--decoder", dec, ...net]);
      check(back.code === 0 && back.out === parts, "read gives the written rows back exactly");
      const u = await run(["sql", "UPDATE parts SET qty = qty + 1 WHERE id = 2; DELETE FROM parts WHERE id = 3; COMMIT", "--db", "cli-shop", "--key", keyFile, "--yes", ...net]);
      const back2 = await run(["read", partsPda, "--official", me, "--decoder", dec, ...net]);
      check(u.code === 0 && /parts: 1 changed, 1 deleted/.test(u.err) && back2.out === "id,name,qty\n1,Hex bolt,40\n2,Nut,16\n", "sql UPDATE and DELETE with --yes: only the changes are written, and read back");
      if (u.code !== 0) console.log(u.err);
      // the tables' rules hold on the command line too
      const t = await run(["sql", "CREATE TABLE bins (code VARCHAR(4) PRIMARY KEY, qty INT NOT NULL CHECK (qty >= 0))", "--db", "cli-shop", "--key", keyFile, "--yes", ...net]);
      check(t.code === 0 && /Saved/.test(t.err), "sql CREATE TABLE with types and a CHECK rule, saved");
      const n0 = sends();
      fs.writeFileSync(path.join(tmp, "bad.csv"), "code,qty\nA1,5\nB2,-3\n");
      const bad = await run(["write", "cli-shop", "bins", path.join(tmp, "bad.csv"), "--key", keyFile, "--yes", ...net]);
      check(bad.code === 1 && /qty/.test(bad.err) && sends() === n0, `a row breaking the table's CHECK rule is refused before anything is sent (${(bad.err.split("\n").find((l) => /✗|Nothing/.test(l)) || "").slice(0, 70)}…)`);
      const piped = await run(["sql", "-", "--db", "cli-shop", ...net], "SELECT name FROM parts WHERE qty > 20;");
      check(piped.code === 0 && piped.out === "name\nHex bolt\n", "sql reads its statements from standard input");
      const nokey = await run(["sql", "INSERT INTO parts VALUES (9, 'Pin', 1)", "--db", "cli-shop", "--yes", ...net]);
      check(nokey.code === 1 && /--key/.test(nokey.err), "--yes without a key: refused, nothing signed");
      const n2 = sends();
      const capped = await run(["sql", "INSERT INTO parts VALUES (9, 'Pin', 1)", "--db", "cli-shop", "--key", keyFile, "--yes", "--max", "0.0001", ...net]);
      check(capped.code === 1 && /Stopped before step 1 of 1: it would cost/.test(capped.err) && sends() === n2, "--max: a step that would pass the cap isn't sent");
      const no = await run(["sql", "INSERT INTO parts VALUES (9, 'Pin', 1)", "--db", "cli-shop", "--key", keyFile, "--yes=false", ...net]);
      check(no.code === 0 && /a dry run/.test(no.err) && sends() === n2, "--yes=false is no");
      const secret = "5" + "K".repeat(60);
      const leak = await run(["sql", "SELECT 1", "--db", "cli-shop", "--key", secret, ...net]);
      check(leak.code === 1 && /--key file/.test(leak.err) && !leak.err.includes(secret), "a key typed where its file should go is never echoed");
      fs.writeFileSync(path.join(tmp, "more.json"), JSON.stringify({ cols: ["id", "name", "qty"], rows: [[5, "Cotter pin", 12]] }));
      const wj = await run(["write", "cli-shop", "parts", path.join(tmp, "more.json"), ...net]);
      check(wj.code === 0 && /parts: 1 row\(s\) added/.test(wj.err), "write takes a JSON {cols, rows} file too");
      // MCP: an AI assistant's view of the same
      const mcp = async (opts, msgs) => {
        const sent = [];
        await L.mcpServer({ decoder: { wasm: fs.readFileSync(dec) }, store: null, fetch: mf, app: { rpc: "https://rpc.test/", fetch: mf, ...opts.app }, budget: opts.budget }, {
          lines: msgs.map((m) => JSON.stringify(m)),
          send: (m) => sent.push(m),
        });
        return sent;
      };
      const call = (id, name, args) => ({ jsonrpc: "2.0", id, method: "tools/call", params: { name, arguments: args } });
      const keyText = fs.readFileSync(keyFile, "utf8");
      const s1 = await mcp({ app: { key: keyText }, budget: 0.05 }, [
        { jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "test", version: "1" } } },
        { jsonrpc: "2.0", method: "notifications/initialized" },
        { jsonrpc: "2.0", id: 2, method: "tools/list" },
        call(3, "sql", { database: "cli-shop", query: "SELECT id, name, qty FROM parts ORDER BY id" }),
        call(4, "write_rows", { database: "cli-shop", table: "bins", rows: [{ code: "C3", qty: 7 }] }),
        call(5, "pending_changes", { database: "cli-shop" }),
        call(6, "save_changes", { database: "cli-shop" }),
        call(7, "read_table", { table: partsPda, official: me }),
        call(8, "decode_pack", { pack: "nonsense" }),
        { jsonrpc: "2.0", id: 9, method: "nope" },
      ]);
      const res = (id) => s1.find((m) => m.id === id);
      const names = (res(2).result.tools || []).map((x) => x.name);
      check(res(1).result.protocolVersion === "2025-06-18" && res(1).result.serverInfo.name === "iq-tables" && ["read_table", "sql", "write_rows", "pending_changes", "save_changes", "encode_rows", "decode_pack"].every((n) => names.includes(n)),
        "MCP: initialize and tools/list (with save_changes, since the server has a key)");
      const q = JSON.parse(res(3).result.content[0].text);
      check(JSON.stringify(q.results[0].rows) === JSON.stringify([{ id: 1, name: "Hex bolt", qty: 40 }, { id: 2, name: "Nut", qty: 16 }]), "MCP sql: rows as JSON objects");
      const pend = JSON.parse(res(5).result.content[0].text);
      check(!res(4).result.isError && pend.changes === 1 && /bins: 1 row\(s\) added/.test(pend.summary), "MCP write_rows stages a row; pending_changes shows it and its cost");
      const saved = !res(6).result.isError && JSON.parse(res(6).result.content[0].text);
      check(!!saved && saved.saved === true && /SOL/.test(saved.budgetLeft), `MCP save_changes writes it, within the session's budget (${saved && saved.budgetLeft} left)`);
      check(JSON.parse(res(7).result.content[0].text).length === 2 && res(8).result.isError === true && res(9).error.code === -32601, "MCP read_table and decode_pack (errors come back as tool errors), unknown methods refused");
      const binsPda = iq.contract.getTablePda(new PublicKey(shopRoot), iq.utils.toSeedBytes("bins"), PID).toBase58();
      const bins = await run(["read", binsPda, "--official", me, "--decoder", dec, ...net]);
      check(bins.out === "code,qty\nC3,7\n", "…and the row is on chain");
      const s4 = await mcp({ app: {} }, [
        call(1, "sql", { database: "cli-shop", query: "CREATE TABLE lots (id INT PRIMARY KEY, label VARCHAR(10) NOT NULL, qty INT DEFAULT 0)" }),
        call(2, "write_rows", { database: "cli-shop", table: "lots", rows: [{ id: 1, label: "first" }] }),
        call(3, "write_rows", { database: "cli-shop", table: "lots", rows: [{ id: 2 }] }),
        call(4, "sql", { database: "cli-shop", query: "SELECT id, label, qty FROM lots" }),
      ]);
      const lots = JSON.parse(s4[3].result.content[0].text);
      check(!s4[1].result.isError && s4[2].result.isError === true && JSON.stringify(lots.results[0].rows) === JSON.stringify([{ id: 1, label: "first", qty: 0 }]),
        "MCP: rows written into a table staged with CREATE TABLE keep its types, defaults and NOT NULL; a refused row leaves the table there");
      const n1 = sends();
      const s2 = await mcp({ app: { key: keyText }, budget: 0.0001 }, [
        call(1, "write_rows", { database: "cli-shop", table: "bins", csv: "code,qty\nD4,1\n" }),
        call(2, "save_changes", { database: "cli-shop" }),
      ]);
      check(s2[1].result.isError === true && /budget/.test(s2[1].result.content[0].text) && sends() === n1, "MCP: a save over the session's budget is refused, nothing sent");
      const s3 = await mcp({ app: {} }, [{ jsonrpc: "2.0", id: 1, method: "tools/list" }, call(2, "sql", { database: "cli-shop", query: "SELECT COUNT(*) AS n FROM bins" })]);
      check(!s3[0].result.tools.some((x) => x.name === "save_changes") && JSON.parse(s3[1].result.content[0].text).results[0].rows[0].n === 1, "MCP without a key: no save_changes tool, reading works");
      fs.rmSync(tmp, { recursive: true, force: true });
    }
    // following the newest decoder
    const other = new PublicKey(Buffer.alloc(32, 22)).toBase58();
    repoCommit(DEC, "not the owner", { ...DEC.files(), "iqt-decoder.wasm": fakeDecoder({ error: "impostor" }) }, other);
    const fresh = { decoder: { ...base.decoder, refreshMs: 0 } };
    L.clearCache();
    check((await L.readTable({ ...base, ...fresh, cacheMs: 0 })).decoderRef === decSig, "a commit by anyone but the repository's owner is ignored");
    repoCommit(DEC, "decoder 1.0.1", { ...DEC.files(), "iqt-decoder.wasm": fakeDecoder({ error: "hello from the newest decoder" }) });
    check((await fail(fresh)).includes("hello from the newest decoder"), "the owner commits a new decoder: the next read uses it (no loader update)");
    check((await fail({ decoder: { pin: decSig } })) === "no error", "…while a pinned read keeps the version it pinned");
    repoCommit(DEC, "decoder 1.0.2", DEC.files());
  }

  console.log("Reading straight from Solana (no gateway)");
  await page.goto("https://iq.test/#/settings");
  await page.selectOption("select[data-arg='source']", "rpc");
  await page.goto("https://iq.test/#/");
  await waitText("e2e-parts");
  check((await text()).includes("Source: Solana RPC"), "database list read with getProgramAccounts");
  await page.goto(`https://iq.test/#/t/${rootPda}/${fastPda}`);
  await page.waitForFunction(() => /Official\s*600/.test(document.getElementById("app").innerText), null, { timeout: 20000 });
  await page.click("button[data-a='tv-who'][data-arg='official']");
  await waitText("FST-EDITED");
  const t7 = (await text()).replace(/\s+/g, " ");
  check(/Unofficial 1/.test(t7) && t7.includes("FST-EDITED"), "rows rebuilt from the table's transactions: 600 official + 1 unofficial");
  check(chain.batches > 0, "transactions fetched with batched JSON-RPC (" + chain.batches + " batch)");
  await page.goto(`https://iq.test/#/t/${rootPda}/${progPda}`);
  await page.waitForSelector("table.data td:has-text('generator')", { timeout: 30000 });
  check(true, "a row sent through a session is rebuilt from its post_chunk transactions (getSignaturesForAddress on the session + getTransaction)");
  await page.goto("https://iq.test/#/settings");
  await page.selectOption("select[data-arg='source']", "gateway");

  console.log("Devnet");
  await page.selectOption("select[data-arg='cluster']", "devnet");
  await page.goto("https://iq.test/#/account");
  await waitText("Balance");
  check((await page.locator(".devnet").count()) === 1, "devnet banner shown");
  await page.click("button[data-a='panel'][data-arg='add']");
  const devMain = (await page.locator(".panel .addrbox .mono").innerText()).trim();
  const before = lam(devMain);
  await page.click("button:has-text('Get 1 free test SOL')");
  await waitText("Airdrop confirmed");
  check(lam(devMain) === before + LAMPORTS, "devnet airdrop of 1 SOL confirmed");
  await page.goto("https://iq.test/#/settings");
  await page.selectOption("select[data-arg='cluster']", "mainnet");

  console.log("Mobile layout");
  await page.setViewportSize({ width: 390, height: 844 });
  const k2 = (await page.evaluate(() => JSON.parse(localStorage.getItem("iqtables:v1:drafts"))))[0].key;
  for (const [h, t] of [["#/", "iq-locker"], ["#/account", "Balance"], ["#/mine", "Drafts in this browser"], [`#/ws/${k2}/0`, "Browse"]]) {
    await page.goto("https://iq.test/" + h);
    await waitText(t);
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    check(overflow <= 1, `no horizontal page scroll at 390px on ${h.split("/").slice(0, 2).join("/")} (${overflow})`);
  }
  await shot("08-mobile");

  check(consoleErrors.length === 0, "no console errors" + (consoleErrors.length ? ": " + consoleErrors.slice(0, 3).join(" | ") : ""));
  await browser.close();
  const failed = results.filter((r) => !r[0]).length;
  console.log(`\n${results.length - failed}/${results.length} checks passed`);
})().catch((e) => { console.error("E2E crashed:", e); process.exit(1); });

function solana_short(a) { return a.slice(0, 4) + "…" + a.slice(-4); }
