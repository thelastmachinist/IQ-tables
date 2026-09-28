// Generates reference outputs from the official IQ Labs SDK (and the libraries
// it ships with) so the Rust implementation can be checked byte-for-byte.
// Dev-only. Run from tools/: npm install && npm run fixtures
const fs = require("fs");
const crypto = require("crypto");
const iqmod = require("@iqlabs-official/solana-sdk");
const iq = iqmod.default || iqmod;
const SDK_DIST = process.env.SDK_DIST || require("path").dirname(require.resolve("@iqlabs-official/solana-sdk"));
const { PublicKey, Keypair } = require("@solana/web3.js");
const { ed25519 } = require("@noble/curves/ed25519.js");
const { keccak_256 } = require("@noble/hashes/sha3");
const bs58 = require("bs58").default || require("bs58");

let seed = 12345;
const rnd = (n) => {
  const b = Buffer.alloc(n);
  for (let i = 0; i < n; i++) {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    b[i] = (seed >> 16) & 0xff;
  }
  return b;
};
const hex = (b) => Buffer.from(b).toString("hex");
const pid = iq.contract.PROGRAM_ID;
const out = {};

// hashes
out.hashes = [];
for (const len of [0, 1, 3, 55, 56, 63, 64, 111, 112, 127, 128, 135, 136, 137, 200, 1000, 5000]) {
  const d = rnd(len);
  out.hashes.push({
    input: hex(d),
    sha256: crypto.createHash("sha256").update(d).digest("hex"),
    sha512: crypto.createHash("sha512").update(d).digest("hex"),
    keccak256: hex(keccak_256(d)),
  });
}

// base58
out.base58 = [];
for (const len of [0, 1, 5, 32, 64]) {
  for (const zeros of [0, 1, 3]) {
    const d = Buffer.concat([Buffer.alloc(Math.min(zeros, len)), rnd(Math.max(0, len - zeros))]);
    out.base58.push({ hex: hex(d), b58: bs58.encode(d) });
  }
}

// ed25519
out.ed25519 = [];
for (let i = 0; i < 24; i++) {
  const sk = rnd(32);
  const msg = rnd(i * 37);
  out.ed25519.push({
    seed: hex(sk),
    pub: hex(ed25519.getPublicKey(sk)),
    msg: hex(msg),
    sig: hex(ed25519.sign(msg, sk)),
  });
}

// PDAs
out.pdas = [];
for (let i = 0; i < 150; i++) {
  const idLen = 1 + (i % 32);
  const dbId = rnd(idLen);
  const tableName = "t" + i + ":" + hex(rnd(i % 7));
  const tableSeed = iq.utils.toSeedBytes(tableName);
  const root = iq.contract.getDbRootPda(dbId, pid);
  const user = new PublicKey(rnd(32));
  out.pdas.push({
    dbId: hex(dbId),
    tableName,
    tableSeed: hex(tableSeed),
    dbRoot: root.toBase58(),
    table: iq.contract.getTablePda(root, tableSeed, pid).toBase58(),
    instructionTable: iq.contract.getInstructionTablePda(root, tableSeed, pid).toBase58(),
    user: user.toBase58(),
    userState: iq.contract.getUserPda(user, pid).toBase58(),
    codeAccount: iq.contract.getCodeAccountPda(user, pid).toBase58(),
    userInventory: iq.contract.getUserInventoryPda(user, pid).toBase58(),
  });
}
out.knownPdas = {
  iqLockerRoot: iq.contract.getDbRootPda(Buffer.from("iq-locker"), pid).toBase58(),
  iqLockerNotes: iq.contract
    .getTablePda(iq.contract.getDbRootPda(Buffer.from("iq-locker"), pid), iq.utils.toSeedBytes("notes"), pid)
    .toBase58(),
};

// instructions via the SDK's own builder
const b = iq.contract.createInstructionBuilder();
const ser = (ix) => ({
  program: ix.programId.toBase58(),
  keys: ix.keys.map((k) => [k.pubkey.toBase58(), k.isSigner, k.isWritable]),
  data: hex(ix.data),
});
const signer = new PublicKey(rnd(32));
const creator = new PublicKey(rnd(32));
const dbId = Buffer.from("my-parts-db");
const root = iq.contract.getDbRootPda(dbId, pid);
const tseed = iq.utils.toSeedBytes("fasteners");
const SYS = new PublicKey("11111111111111111111111111111111");
out.ix = { signer: signer.toBase58(), creator: creator.toBase58(), dbId: hex(dbId), tableSeed: hex(tseed) };
out.ix.initializeDbRoot = ser(
  iq.contract.initializeDbRootInstruction(b, { db_root: root, signer, system_program: SYS }, { db_root_id: dbId })
);
out.ix.manageTableCreators = ser(
  iq.contract.manageTableCreatorsInstruction(
    b,
    { signer, db_root: root, system_program: SYS },
    { db_root_id: dbId, table_creators: [signer], ext_creators: [signer, creator] }
  )
);
const ctAccounts = {
  db_root: root,
  receiver: new PublicKey(iq.constants.DEFAULT_WRITE_FEE_RECEIVER),
  db_root_creator: creator,
  signer,
  table: iq.contract.getTablePda(root, tseed, pid),
  instruction_table: iq.contract.getInstructionTablePda(root, tseed, pid),
  system_program: SYS,
};
const ctArgs = (writers) => ({
  db_root_id: dbId,
  table_seed: tseed,
  table_hint: Buffer.from("fasteners"),
  table_name: Buffer.from("Fasteners & hardware"),
  column_names: ["id", "p"].map((s) => Buffer.from(s)),
  id_col: Buffer.from("id"),
  ext_keys: ["iqt:1"].map((s) => Buffer.from(s)),
  gate_opt: null,
  writers_opt: writers,
});
out.ix.createTableOpen = ser(iq.contract.createTableInstruction(b, ctAccounts, ctArgs(null)));
out.ix.createTableLocked = ser(iq.contract.createTableInstruction(b, ctAccounts, ctArgs([signer])));
out.ix.userInitialize = ser(
  iq.contract.userInitializeInstruction(b, {
    user: signer,
    code_account: iq.contract.getCodeAccountPda(signer, pid),
    user_state: iq.contract.getUserPda(signer, pid),
    user_inventory: iq.contract.getUserInventoryPda(signer, pid),
    system_program: SYS,
  })
);
const rowJson = JSON.stringify({ id: "pk1", p: 'IQT1z<>&%$#!"quote"\\back' });
const metadata = JSON.stringify({
  filetype: "application/octet-stream",
  method: 0,
  filename: "7.bin",
  total_chunks: 1,
  data: rowJson,
});
out.ix.rowJson = rowJson;
out.ix.metadata = metadata;
const iqAta = new PublicKey(rnd(32));
out.ix.iqAta = iqAta.toBase58();
for (const [name, ata] of [["dbCodeIn", undefined], ["dbCodeInAta", iqAta]]) {
  out.ix[name] = ser(
    iq.contract.dbCodeInInstruction(
      b,
      {
        user: signer,
        signer,
        user_inventory: iq.contract.getUserInventoryPda(signer, pid),
        db_root: root,
        table: iq.contract.getTablePda(root, tseed, pid),
        signer_ata: undefined,
        metadata_account: undefined,
        system_program: SYS,
        receiver: new PublicKey(iq.constants.DEFAULT_WRITE_FEE_RECEIVER),
        session: undefined,
        iq_ata: ata,
      },
      { db_root_id: dbId, table_seed: tseed, on_chain_path: "", metadata, session: null }
    )
  );
}
out.ix.realloc = ser(
  iq.contract.reallocAccountInstruction(b, { payer: signer, target: root, system_program: SYS }, { new_size: new (require("@coral-xyz/anchor").BN)(4321) })
);

// ATA derivation
const owner = new PublicKey(rnd(32));
const mint = new PublicKey(iq.constants.DEFAULT_IQ_MINT);
const { findAssociatedTokenAddress } = require(SDK_DIST + "/sdk/utils/ata.js");
out.ata = {
  owner: owner.toBase58(),
  legacy: findAssociatedTokenAddress(owner, mint).toBase58(),
  t22: findAssociatedTokenAddress(owner, mint, new PublicKey("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb")).toBase58(),
};

// JSON.stringify string escaping
out.jsonStrings = ["plain", 'q"uote', "back\\slash", "nl\nret\rtab\t", "\u0001\u001f\u007f", "é漢字🙂", "\b\f", "</script>"].map(
  (s) => ({ s, json: JSON.stringify(s) })
);

// v1 transaction reference from the SDK itself (fixed blockhash)
const { buildV1Transaction } = require(SDK_DIST + "/sdk/writer/v1_tx.js");
const kp = Keypair.fromSeed(rnd(32));
const bh = bs58.encode(rnd(32));
const ixs = [
  iq.contract.initializeDbRootInstruction(b, { db_root: root, signer: kp.publicKey, system_program: SYS }, { db_root_id: dbId }),
];
const v1 = buildV1Transaction(kp, ixs, bh);
out.v1 = { seed: hex(kp.secretKey.slice(0, 32)), blockhash: bh, raw: hex(v1.raw), signature: v1.signature };
const v1b = buildV1Transaction(
  kp,
  [
    iq.contract.dbCodeInInstruction(
      b,
      {
        user: kp.publicKey,
        signer: kp.publicKey,
        user_inventory: iq.contract.getUserInventoryPda(kp.publicKey, pid),
        db_root: root,
        table: iq.contract.getTablePda(root, tseed, pid),
        system_program: SYS,
        receiver: new PublicKey(iq.constants.DEFAULT_WRITE_FEE_RECEIVER),
      },
      { db_root_id: dbId, table_seed: tseed, on_chain_path: "", metadata, session: null }
    ),
  ],
  bh
);
out.v1CodeIn = { raw: hex(v1b.raw), signature: v1b.signature };

fs.writeFileSync(process.argv[2] || "fixtures.json", JSON.stringify(out, null, 1));
console.log("wrote fixtures:", Object.keys(out).join(", "));
