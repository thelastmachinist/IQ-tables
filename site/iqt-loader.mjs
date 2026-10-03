// iqt-loader.mjs — reads IQ Tables tables anywhere JavaScript runs.
// Node 18+, Deno, Bun or a web page. No dependencies. MIT license.
// Also a command-line tool and an MCP server for AI assistants:
// `node iqt-loader.mjs help` (and connect() further down, for SQL and writes in code).
//
//   import { readTable, renderTable } from "./iqt-loader.mjs";
//
//   const { data } = await readTable({
//     table: "<table address>",
//     official: "<wallet whose rows count>",   // usually the database's creator
//     format: "csv",                            // "csv" | "json" | "html" | "rows"
//     decoder: { repo: "<IQ git repository>", owner: "<its owner>" },
//   });
//
//   renderTable(document.getElementById("parts"), { ...same options, every: 300000 });
//
// How it works: the IQ Tables decoder is a small WebAssembly module kept on
// IQ git. This file finds the newest version (or the one you pin) and runs it
// sealed off:
//   - it gets no imports, so it can't touch files, secrets, the clock or the
//     network on this machine;
//   - the only requests it can have made are GETs to your gateway, and
//     read-only JSON-RPC calls to your RPC (whose address, and any API key in
//     it, it never sees); no redirects;
//   - its memory is capped at 1 GiB, and one that asks for more isn't run.
// A bad decoder could return wrong rows, or keep this thread busy; it can't
// read or send anything. This file never needs updating: fixes arrive with
// the decoder. It speaks the decoder's frozen interface, ABI 1.
//
// Options for readTable / renderTable:
//   table      (required) the table's address
//   official   (required) the wallet whose rows count as official: the database's creator.
//              Anyone can write to a table; rows from other wallets are only shown when
//              `rows` asks for them, and are held to a small size.
//   rows       "official" (default) | "unofficial" | "all"
//   format     "rows" (default: cols + rows) | "csv" | "json" | "html" (text in .data)
//   gateway    IQ's gateway (default https://gateway.iqlabs.dev)
//   rpc        a Solana RPC with full history, used when the gateway can't answer
//   source     "auto" (default) | "gateway" | "solana"
//   fresh      skip the gateway's cache for the first page
//   maxRows    stop after this many on-chain rows (default 100000)
//   cacheMs    reuse a result this long (default 60000; 0 = always read)
//   decoder    { repo, owner }  the IQ git repository holding iqt-decoder.wasm and its owner:
//                               the newest decoder there is used (checked every refreshMs,
//                               default 10 minutes)
//              { pin }          one decoder version (its inscription signature), forever
//              { wasm }         a local copy (a WebAssembly.Module or its bytes) — for hosts
//                               that can't compile downloaded code, like Cloudflare Workers
//              pin and wasm can be combined with repo/owner: the repository is then only used
//              to find decoders for storage formats the pinned one doesn't know
//   store      { get(key) -> string|null, set(key, string) } (may be async): keeps decoders
//              between restarts so a gateway outage doesn't stop you. Browsers use
//              localStorage by default (at most two decoders kept).
//   fetch      a fetch function to use instead of the global one

export const ABI = 1;
export const GATEWAY = "https://gateway.iqlabs.dev";
const DECODER_FILE = "iqt-decoder.wasm";
const REGISTRY_FILE = "iqt-formats.json";
// what the decoder is told the RPC is; requests to it go to yours
const RPC_STANDIN = "https://rpc.invalid/";
const RPC_READS = new Set(["getAccountInfo", "getMultipleAccounts", "getSignaturesForAddress", "getTransaction", "getSlot", "getBlockTime"]);
const MAX_STEPS = 200000;
const MAX_PAGES = 1 << 14; // 64 KiB pages: 1 GiB
const PARALLEL = 8;
const COMMIT_PAGES = 20;
const SIG = /^[1-9A-HJ-NP-Za-km-z]{80,90}$/;

const modules = new Map(); // decoder inscription -> WebAssembly.Module
const repos = new Map(); // repo:owner -> { at, owner, tree }
const registries = new Map(); // registry inscription -> { formats }
const results = new Map(); // config -> { at, value }
const local = new WeakMap(); // a local copy (decoder.wasm) -> { id, module }
let localIds = 0;

export function clearCache() {
  modules.clear();
  repos.clear();
  registries.clear();
  results.clear();
}

// ------------------------------------------------------------------ bytes

function fromB64(s) {
  const bin = atob(String(s).trim());
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function defaultStore() {
  try {
    const ls = globalThis.localStorage;
    if (!ls) return null;
    ls.setItem("iqt-probe", "1");
    ls.removeItem("iqt-probe");
    const LIST = "iqt-decoders";
    return {
      get: (k) => ls.getItem(k),
      set: (k, v) => {
        try {
          if (k.startsWith("iqt-decoder-")) {
            // keep the two most recent decoders only
            let list = [];
            try { list = JSON.parse(ls.getItem(LIST) || "[]").filter((x) => typeof x === "string" && x !== k); } catch (_) { /* start over */ }
            list.push(k);
            while (list.length > 2) ls.removeItem(list.shift());
            ls.setItem(LIST, JSON.stringify(list));
          }
          ls.setItem(k, v);
        } catch (_) { /* full: skip */ }
      },
    };
  } catch (_) {
    return null;
  }
}

async function storeGet(store, k) {
  if (!store) return null;
  try { return (await store.get(k)) ?? null; } catch (_) { return null; }
}

async function storeSet(store, k, v) {
  if (!store) return;
  try { await store.set(k, v); } catch (_) { /* best effort */ }
}

// ---------------------------------------------------------------- network

// Redirects aren't followed ("manual" works everywhere, including Cloudflare
// Workers, which rejects "error"); a redirect answer counts as a failure.
const redirected = (r) => r.type === "opaqueredirect" || (r.status >= 300 && r.status < 400);

async function getJson(fx, url) {
  const r = await fx(url, { redirect: "manual" });
  if (redirected(r)) throw new Error(`${url}: redirected (not followed)`);
  if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`);
  return r.json();
}

// An inscription's data as IQ's gateway serves it (IQ git files are base64).
async function inscription(fx, gw, sig) {
  if (!SIG.test(sig)) throw new Error(`not an inscription signature: ${sig}`);
  const v = await getJson(fx, `${gw}/data/${sig}`);
  if (v == null || v.data == null || v.data === "") throw new Error(`IQ's gateway returned no data for ${sig}`);
  return v.data;
}

// Is this JSON-RPC body (one call or a batch) read-only?
function readOnly(body) {
  let v;
  try { v = JSON.parse(body); } catch (_) { return false; }
  const calls = Array.isArray(v) ? v : [v];
  return calls.length > 0 && calls.length <= 100 && calls.every((c) => c && typeof c === "object" && RPC_READS.has(c.method));
}

// A request the decoder asked for: GETs under the gateway, read-only
// JSON-RPC to the RPC. Anything else is refused.
async function http(ctx, req) {
  const url = String(req.url);
  let target;
  let init;
  if (ctx.rpc && url === RPC_STANDIN) {
    if (req.method !== "POST" || !readOnly(req.body)) throw new Error("IQ Tables: refused an RPC call that isn't a read");
    target = ctx.rpc;
    init = { method: "POST", body: String(req.body), headers: { "content-type": "application/json" } };
  } else if ((req.method || "GET") === "GET" && url.startsWith(ctx.gw + "/")) {
    target = url;
    init = { method: "GET" };
  } else {
    let where = url;
    try { where = new URL(url).origin; } catch (_) { /* as is */ }
    throw new Error(`IQ Tables: refused a request to ${where} — only GETs to your gateway and read-only calls to your RPC are allowed`);
  }
  try {
    const r = await ctx.fx(target, { ...init, redirect: "manual" });
    if (redirected(r)) return { ok: false, status: 0, body: "redirected (not followed)" };
    return { ok: true, status: r.status, body: await r.text() };
  } catch (e) {
    return { ok: false, status: 0, body: String((e && e.message) || e) };
  }
}

async function pool(items, f) {
  const out = new Array(items.length);
  let next = 0;
  const worker = async () => {
    while (next < items.length) {
      const i = next++;
      out[i] = await f(items[i]);
    }
  };
  await Promise.all(Array.from({ length: Math.min(PARALLEL, items.length) }, worker));
  return out;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// --------------------------------------------------------------- decoders

// Look at a module before running anything: no imports, no start function,
// and exactly one memory, with a declared maximum of 1 GiB or less.
function inspect(b) {
  let i = 8;
  const leb = () => {
    let v = 0;
    let shift = 0;
    for (;;) {
      if (i >= b.length) throw new Error("truncated");
      const c = b[i++];
      v += (c & 127) * 2 ** shift;
      if (c < 128) return v;
      shift += 7;
    }
  };
  if (b.length < 8 || b[0] !== 0 || b[1] !== 0x61 || b[2] !== 0x73 || b[3] !== 0x6d) throw new Error("IQ Tables: not a WebAssembly module");
  let memory = false;
  while (i < b.length) {
    const id = b[i++];
    const size = leb();
    const end = i + size;
    if (id === 2 && leb() > 0) throw new Error("IQ Tables: refusing a decoder that asks for access to anything outside itself");
    if (id === 8) throw new Error("IQ Tables: refusing a decoder that runs code as it loads");
    if (id === 5) {
      const n = leb();
      if (n !== 1) throw new Error("IQ Tables: refusing a decoder that doesn't have exactly one memory");
      for (let k = 0; k < n; k++) {
        const flags = b[i++];
        leb();
        if (!(flags & 1) || flags & ~1 || leb() > MAX_PAGES) throw new Error("IQ Tables: refusing a decoder whose memory isn't capped at 1 GiB");
      }
      memory = n > 0;
    }
    i = end;
  }
  if (!memory) throw new Error("IQ Tables: not a decoder (no memory)");
}

async function compile(code) {
  let m;
  if (code instanceof WebAssembly.Module) m = code;
  else {
    const bytes = code instanceof Uint8Array ? code : new Uint8Array(code);
    inspect(bytes);
    m = await WebAssembly.compile(bytes);
  }
  if (WebAssembly.Module.imports(m).length) throw new Error("IQ Tables: refusing a decoder that asks for access to anything outside itself");
  const names = WebAssembly.Module.exports(m).map((e) => e.name);
  for (const n of ["memory", "iqt_abi", "iqt_alloc", "iqt_call", "iqt_len"]) {
    if (!names.includes(n)) throw new Error(`IQ Tables: not a decoder (it has no ${n})`);
  }
  const abi = new WebAssembly.Instance(m, {}).exports.iqt_abi();
  if (abi !== ABI) throw new Error(`IQ Tables: this decoder speaks ABI ${abi} and this loader ABI ${ABI}; download the loader again`);
  return m;
}

// One conversation with a decoder instance.
function open(m) {
  const e = new WebAssembly.Instance(m, {}).exports;
  return (msg) => {
    const input = new TextEncoder().encode(JSON.stringify(msg));
    const p = e.iqt_alloc(input.length) >>> 0;
    new Uint8Array(e.memory.buffer, p, input.length).set(input);
    const o = e.iqt_call(p, input.length) >>> 0;
    const n = e.iqt_len() >>> 0;
    return JSON.parse(new TextDecoder().decode(new Uint8Array(e.memory.buffer, o, n)));
  };
}

async function bySig(ctx, sig) {
  if (modules.has(sig)) return modules.get(sig);
  const key = `iqt-decoder-${sig}`;
  let m = null;
  const saved = await storeGet(ctx.store, key);
  if (saved) {
    try { m = await compile(fromB64(saved)); } catch (_) { m = null; } // damaged: download it again
  }
  if (!m) {
    const b64 = await inscription(ctx.fx, ctx.gw, sig);
    m = await compile(fromB64(b64));
    await storeSet(ctx.store, key, String(b64));
  }
  modules.set(sig, m);
  return m;
}

// The owner's newest commit in the decoder repository. Anyone may add rows to
// a commit table, so pages are read until one of the owner's commits shows up.
async function newestCommit(ctx) {
  const d = ctx.decoder;
  const time = (r) => Number(r.timestamp) || (Number(r.__blockTime) || 0) * 1000;
  let before = null;
  for (let page = 0; page < COMMIT_PAGES; page++) {
    const v = await getJson(ctx.fx, `${ctx.gw}/table/${d.repo}/rows?limit=100${before ? `&before=${before}` : ""}`);
    const mine = (v.rows || []).filter((r) => r.__signer === d.owner && SIG.test(r.treeTxId || "")).sort((a, b) => time(b) - time(a));
    if (mine.length) return mine[0];
    before = v.nextCursor;
    if (!before || !(v.rows || []).length) break;
  }
  throw new Error(`no commits by ${d.owner} in ${d.repo}`);
}

// The newest commit's files in the decoder repository.
async function newestTree(ctx) {
  const d = ctx.decoder;
  const key = `${d.repo}:${d.owner}`;
  const refresh = d.refreshMs ?? 600000;
  const have = repos.get(key);
  if (have && Date.now() - have.at < refresh) return have.tree;
  let saved = null;
  try {
    const s = JSON.parse((await storeGet(ctx.store, `iqt-repo-${d.repo}`)) || "null");
    if (s && s.owner === d.owner && s.tree && typeof s.tree === "object") saved = s;
  } catch (_) { /* ignore */ }
  if (!have && saved && Date.now() - saved.at < refresh) {
    repos.set(key, saved);
    return saved.tree;
  }
  try {
    const c = await newestCommit(ctx);
    const data = await inscription(ctx.fx, ctx.gw, c.treeTxId);
    const tree = typeof data === "string" ? JSON.parse(data) : data;
    if (!tree || typeof tree !== "object") throw new Error("unreadable commit tree");
    const s = { at: Date.now(), owner: d.owner, commit: c.id, tree };
    repos.set(key, s);
    await storeSet(ctx.store, `iqt-repo-${d.repo}`, JSON.stringify(s));
    return tree;
  } catch (e) {
    // keep using the last decoder that worked; look again in a minute
    const last = have || saved;
    if (last) {
      last.at = Date.now() - refresh + 60000;
      repos.set(key, last);
      return last.tree;
    }
    throw new Error(`IQ Tables: couldn't find the decoder in ${d.repo}: ${e.message}`);
  }
}

function fileSig(tree, name) {
  const f = tree && tree[name];
  return f && typeof f.txId === "string" ? f.txId : null;
}

async function newestDecoder(ctx) {
  const tree = await newestTree(ctx);
  const sig = fileSig(tree, DECODER_FILE);
  if (!sig) throw new Error(`IQ Tables: the newest commit of ${ctx.decoder.repo} has no ${DECODER_FILE}`);
  return { module: await bySig(ctx, sig), ref: sig };
}

async function mainDecoder(ctx) {
  const d = ctx.decoder;
  if (d.wasm) {
    if (typeof d.wasm !== "object") throw new Error("IQ Tables: decoder.wasm must be a WebAssembly.Module or its bytes");
    if (!local.has(d.wasm)) local.set(d.wasm, { id: ++localIds, module: await compile(d.wasm) });
    return { module: local.get(d.wasm).module, ref: "local" };
  }
  if (d.pin) return { module: await bySig(ctx, d.pin), ref: d.pin };
  if (!d.repo || !d.owner) throw new Error("IQ Tables: set decoder to { repo, owner } (where iqt-decoder.wasm lives on IQ git), { pin } or { wasm }");
  return newestDecoder(ctx);
}

// A pack in a format the running decoder doesn't read: ask the decoder the
// format registry names for it.
async function delegate(ctx, item) {
  try {
    const d = ctx.decoder;
    if (!d.repo || !d.owner) return { error: `format ${item.format} needs another decoder, and no decoder repository is set` };
    const tree = await newestTree(ctx);
    const rsig = fileSig(tree, REGISTRY_FILE);
    if (!rsig) return { error: `the decoder repository has no ${REGISTRY_FILE}` };
    let reg = registries.get(rsig);
    if (!reg) {
      reg = JSON.parse(new TextDecoder().decode(fromB64(await inscription(ctx.fx, ctx.gw, rsig))));
      registries.set(rsig, reg);
    }
    const ref = reg && reg.formats && Object.prototype.hasOwnProperty.call(reg.formats, item.format) ? reg.formats[item.format] : null;
    if (!ref) return { error: `no decoder is listed for format ${item.format}` };
    let m;
    if (ref === "current") {
      const newest = await newestDecoder(ctx);
      if (newest.ref === ctx.running) return { error: `the newest decoder doesn't read format ${item.format}` };
      m = newest.module;
    } else m = await bySig(ctx, ref);
    if (!ctx.sessions.has(m)) ctx.sessions.set(m, open(m));
    try {
      return ctx.sessions.get(m)({ op: "decode", payload: item.payload });
    } catch (e) {
      // a trapped instance isn't reused: the next pack gets a fresh one
      ctx.sessions.delete(m);
      throw e;
    }
  } catch (e) {
    return { error: String((e && e.message) || e) };
  }
}

function context(opts) {
  const fx = opts.fetch || globalThis.fetch.bind(globalThis);
  const gw = String(opts.gateway || GATEWAY).replace(/\/+$/, "");
  new URL(gw);
  if (opts.rpc) new URL(opts.rpc);
  const store = opts.store === undefined ? defaultStore() : opts.store;
  return { fx, gw, rpc: opts.rpc || null, store, decoder: opts.decoder || {}, sessions: new Map() };
}

// ----------------------------------------------------------------- public

// Options this file handles itself; everything else goes to the decoder as
// is, so options that newer decoders add work without a new loader.
const LOADER_OPTS = new Set(["fetch", "store", "decoder", "cacheMs", "every", "className"]);

// What the decoder is told: the options it reads, normalized.
function readConfig(opts) {
  const config = {};
  for (const [k, v] of Object.entries(opts)) {
    if (!LOADER_OPTS.has(k) && v !== undefined && typeof v !== "function") config[k] = v;
  }
  return Object.assign(config, {
    rows: opts.rows || "official",
    format: opts.format || "rows",
    gateway: String(opts.gateway || GATEWAY).replace(/\/+$/, ""),
    rpc: opts.rpc ? RPC_STANDIN : undefined,
    source: opts.source || "auto",
    fresh: !!opts.fresh,
  });
}

async function run(opts, config) {
  const ctx = context(opts);
  const dec = await mainDecoder(ctx);
  ctx.running = dec.ref;
  const call = open(dec.module);
  let step = call({ op: "read", config });
  for (let i = 0; i < MAX_STEPS; i++) {
    if (step.done) return { ...step.done, decoderRef: dec.ref };
    if (step.error) throw new Error(`IQ Tables: ${step.error}`);
    let res;
    if (step.fetch) res = await pool(step.fetch, (r) => http(ctx, r));
    else if (step.decode) res = await pool(step.decode, (d) => delegate(ctx, d));
    else if (step.wait != null) {
      await sleep(Math.min(Number(step.wait) || 0, 60000));
      res = [];
    } else throw new Error("IQ Tables: the decoder sent a step this loader doesn't know");
    step = call({ op: "resume", results: res });
  }
  throw new Error("IQ Tables: the decoder took too many steps");
}

// Read a table. Resolves to { cols, rows, count, data (csv/json/html text),
// asOf: { tx, time }, source: "gateway"|"solana", notes, decoderRef, … }.
export async function readTable(opts) {
  if (!opts || !opts.table) throw new Error("IQ Tables: readTable needs a table address");
  const d = opts.decoder || {};
  if (d.wasm && typeof d.wasm === "object" && !local.has(d.wasm)) local.set(d.wasm, { id: ++localIds, module: await compile(d.wasm) });
  const config = readConfig(opts);
  const key = JSON.stringify([config, opts.rpc, d.repo, d.owner, d.pin, d.wasm ? local.get(d.wasm).id : 0]);
  const cacheMs = opts.cacheMs ?? 60000;
  const now = Date.now();
  if (results.size > 64) for (const [k, v] of results) if (now - v.at >= v.ms) results.delete(k);
  const hit = results.get(key);
  if (hit && cacheMs > 0 && now - hit.at < cacheMs) return hit.value;
  const value = run(opts, config);
  if (cacheMs > 0) results.set(key, { at: now, ms: cacheMs, value });
  try {
    return await value;
  } catch (e) {
    if (results.get(key)?.value === value) results.delete(key);
    throw e;
  }
}

// One call to the decoder that isn't a read (encode, unpack, info).
async function tool(opts, msg) {
  const dec = await mainDecoder(context(opts));
  const r = open(dec.module)(msg);
  if (r.error) throw new Error(`IQ Tables: ${r.error === "unknown op" ? `this decoder (${dec.ref}) is too old for ${msg.op}` : r.error}`);
  return r.ok;
}

// Pack rows into one IQ Tables row, as densely as the format allows.
// `data`: CSV text, an array of objects, or { cols, rows }. Options: `id`
// (the id column's name or number; default the first), `mode` ("dense",
// "compressed" or "plain"), plus the decoder options.
// Resolves to { row: { id, p }, records, columns, idColumn, bytes, raw, compressed, duplicateIds }.
export async function encodeRows(data, opts = {}) {
  const msg = { op: "encode", id: opts.id, mode: opts.mode || "dense" };
  if (typeof data === "string") msg.csv = data;
  else if (Array.isArray(data)) msg.objects = data;
  else if (data && Array.isArray(data.cols)) Object.assign(msg, { cols: data.cols, rows: data.rows || [] });
  else throw new Error("IQ Tables: encodeRows takes CSV text, an array of objects, or { cols, rows }");
  return tool(opts, msg);
}

// Unpack one IQ Tables pack (its payload, or the whole { id, p } row) into
// text: `format` "json" (default) or "csv". Resolves to { text, records, deleted, structure }.
export async function decodePack(pack, opts = {}) {
  return tool(opts, { op: "unpack", payload: typeof pack === "string" ? pack : pack, format: opts.format || "json" });
}

// Which decoder a configuration would use: { ref, abi, version, formats }.
export async function decoderInfo(opts = {}) {
  const ctx = context(opts);
  const dec = await mainDecoder(ctx);
  return { ref: dec.ref, ...open(dec.module)({ op: "info" }) };
}

const text = (v) => (v == null ? "" : typeof v === "object" ? JSON.stringify(v) : String(v));

// Show a table inside `el` (a web page). Values are set as text, never as
// HTML. `every` (ms) re-reads it on a timer.
export async function renderTable(el, opts = {}) {
  const draw = async () => {
    const r = await readTable({ ...opts, format: "rows" });
    const doc = el.ownerDocument;
    const table = doc.createElement("table");
    table.className = opts.className || "iq-table";
    const head = table.createTHead().insertRow();
    for (const c of r.cols) {
      const th = doc.createElement("th");
      th.textContent = c;
      head.appendChild(th);
    }
    const body = table.createTBody();
    for (const row of r.rows) {
      const tr = body.insertRow();
      for (const v of row) tr.insertCell().textContent = text(v);
    }
    el.replaceChildren(table);
    return r;
  };
  let first;
  try {
    first = await draw();
  } catch (e) {
    el.textContent = `Couldn't load this table: ${(e && e.message) || e}`;
    throw e;
  }
  if (opts.every > 0) setInterval(() => draw().catch((e) => console.warn(e)), Math.max(opts.every, 10000));
  return first;
}

// ------------------------------------------------------- the app, headless

// `sql`, `write` and `mcp` run IQ Tables itself — the WebAssembly inside this
// site's index.html — without a page: the same SQL engine, table rules and
// saving (every transaction simulated before it's sent) as the website.
// Unlike the decoder it's given your key, to sign, so it's only ever loaded
// from a file on this machine, and it can only reach your RPC and gateway.

const isWasm = (b) => b.length >= 4 && b[0] === 0 && b[1] === 0x61 && b[2] === 0x73 && b[3] === 0x6d;

async function appModule(app, fs) {
  if (app instanceof WebAssembly.Module) return app;
  let bytes;
  if (app instanceof Uint8Array || app instanceof ArrayBuffer) bytes = new Uint8Array(app);
  else {
    try {
      bytes = new Uint8Array(fs.readFileSync(app || new URL("./index.html", import.meta.url)));
    } catch (_) {
      throw new Error(
        "IQ Tables: sql, write and mcp run the IQ Tables app itself: keep this file next to the site's index.html " +
          "(`iqgit clone` puts them together), or pass --app <index.html or .wasm>",
      );
    }
  }
  if (!isWasm(bytes)) {
    const m = /<script[^>]*id="wasm-b64"[^>]*>([^<]+)<\/script>/.exec(new TextDecoder().decode(bytes));
    if (!m) throw new Error("IQ Tables: that file doesn't hold the IQ Tables app");
    bytes = fromB64(m[1]);
  }
  const mod = await WebAssembly.compile(bytes);
  if (!WebAssembly.Module.exports(mod).some((e) => e.name === "cli")) {
    throw new Error("IQ Tables: this copy of the app is older than the command line; get the site again");
  }
  return mod;
}

function keyText(key) {
  if (key == null || key === "") return null;
  if (typeof key === "string") return key;
  if (key instanceof Uint8Array || Array.isArray(key)) return JSON.stringify(Array.from(key));
  throw new Error("IQ Tables: key must be a key file's text or its 64 bytes");
}

function importText(data) {
  if (typeof data === "string") return data;
  if (Array.isArray(data)) return JSON.stringify(data);
  if (data && Array.isArray(data.cols)) {
    return JSON.stringify((data.rows || []).map((r) => Object.fromEntries(data.cols.map((c, i) => [c, r[i] ?? null]))));
  }
  throw new Error("IQ Tables: rows must be CSV text, an array of objects, or { cols, rows }");
}

// Start the app. Options: key (a Solana key file's text, or its 64 bytes; only
// needed to save), rpc, gateway, devnet, source ("gateway" | "solana"), app
// (index.html's path, or the app's bytes or WebAssembly.Module), fetch, fs.
// Resolves to { wallet, open, sql, write, pending, save, close }:
//   sql(db, statements)        → { results: [{ title, cols, cells, csv, json, count, note }],
//                                  messages: [{ ok, message }], pending }
//   write(db, table, rows, { create, open }) → { messages, pending }
//   pending(db)                → { changes, createDb, tables, lamports, writes }
//   save(db, { maxLamports, onNote }) → { spent, notes, messages }; a step that would take
//                                the save past maxLamports isn't sent. Throws if it didn't
//                                finish, with what was spent anyway as error.spent.
// One call runs at a time (later ones wait their turn).
// Changes stay staged in memory until save(); close() drops them.
export async function connect(opts = {}) {
  const fsName = "node:fs";
  const local = opts.app instanceof WebAssembly.Module || opts.app instanceof Uint8Array || opts.app instanceof ArrayBuffer;
  const fs = opts.fs || (local ? null : await import(fsName));
  const module = await appModule(opts.app, fs);
  const fx = opts.fetch || globalThis.fetch.bind(globalThis);
  let rnd = globalThis.crypto;
  if (!rnd || !rnd.getRandomValues) {
    const cryptoName = "node:crypto";
    rnd = (await import(cryptoName)).webcrypto;
  }
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const storage = new Map();
  const timers = new Set();
  let ex = null;
  let staged = null;
  let inflight = 0;
  let closed = false;
  let broken = null; // the app trapped: nothing more can run
  let wake = null;
  let reach = { rpc: null, gateway: null };
  const kick = () => {
    const w = wake;
    wake = null;
    if (w) w();
  };
  const mem = () => new Uint8Array(ex.memory.buffer);
  const str = (p, l) => dec.decode(mem().subarray(p, p + l));
  const put = (u8) => {
    const p = ex.alloc(u8.length) >>> 0;
    mem().set(u8, p);
    return p;
  };
  const done = (id, ok, status, data) => {
    if (closed) return;
    const u8 = typeof data === "string" ? enc.encode(data) : data;
    try {
      ex.on_async(id, ok ? 1 : 0, status, put(u8), u8.length);
    } catch (e) {
      broken = e;
      closed = true;
      for (const t of timers) clearTimeout(t);
      timers.clear();
    }
  };
  const allowed = (url) => (reach.rpc && url === reach.rpc) || (reach.gateway && url.startsWith(reach.gateway + "/"));
  const host = {
    log: () => {},
    render: () => {},
    fetch: (id, mp, ml, up, ul, bp, bl, cp, cl) => {
      const url = str(up, ul);
      const init = { method: str(mp, ml), redirect: "manual" };
      if (typeof AbortSignal !== "undefined" && AbortSignal.timeout) init.signal = AbortSignal.timeout(120000);
      if (bl) init.body = str(bp, bl);
      if (cl) init.headers = { "content-type": str(cp, cl) };
      inflight++;
      (async () => {
        let r;
        try {
          if (!allowed(url)) throw new Error(`not sent: the app only reaches your RPC and IQ's gateway (${url.split("/").slice(0, 3).join("/")})`);
          const res = await fx(url, init);
          r = redirected(res) ? [false, 0, "redirected (not followed)"] : [true, res.status, await res.text()];
        } catch (e) {
          r = [false, 0, String((e && e.message) || e)];
        }
        inflight--;
        try {
          done(id, ...r);
        } finally {
          kick();
        }
      })();
    },
    storage_get: (kp, kl) => {
      const v = storage.get(str(kp, kl));
      if (v === undefined) return -1;
      staged = enc.encode(v);
      return staged.length;
    },
    take: (p) => {
      mem().set(staged, p);
      staged = null;
    },
    storage_set: (kp, kl, vp, vl) => {
      storage.set(str(kp, kl), str(vp, vl));
    },
    now: () => Date.now(),
    tz: () => -new Date().getTimezoneOffset(),
    random: (p, l) => {
      rnd.getRandomValues(mem().subarray(p, p + l));
    },
    download: () => {},
    copy: () => {},
    set_hash: () => {},
    timer: (id, ms) => {
      if (closed) return;
      const t = setTimeout(() => {
        timers.delete(t);
        try {
          done(id, true, 0, "");
        } finally {
          kick();
        }
      }, ms);
      timers.add(t);
    },
  };
  ex = (await WebAssembly.instantiate(module, { host })).exports;
  ex.start();
  const stopped = () => new Error(broken ? `IQ Tables: the app stopped: ${(broken && broken.message) || broken}` : "IQ Tables: this connection is closed");
  const call = (msg) => {
    if (closed) throw stopped();
    const u8 = enc.encode(JSON.stringify(msg));
    const o = ex.cli(put(u8), u8.length) >>> 0;
    return JSON.parse(dec.decode(mem().subarray(o, o + (ex.cli_len() >>> 0))));
  };
  const fail = (r) => {
    if (r && r.error) throw new Error(`IQ Tables: ${r.error}`);
    return r;
  };
  const close = () => {
    closed = true;
    for (const t of timers) clearTimeout(t);
    timers.clear();
    kick();
  };
  // Wait until the app has nothing left to do; collect what it said.
  const settle = async (onNote, limitMs = 3600000) => {
    const acc = { log: [], out: [], notes: [], run: null, commit: false };
    const t0 = Date.now();
    for (;;) {
      const st = call({ op: "status" });
      acc.log.push(...st.log);
      acc.out.push(...st.out);
      acc.commit = acc.commit || st.commit;
      if (st.run) {
        acc.run = st.run;
        for (const n of st.run.notes) {
          acc.notes.push(n);
          if (onNote) onNote(n);
        }
      }
      if (!st.busy && inflight === 0) return acc;
      if (inflight === 0 && timers.size === 0) throw new Error("IQ Tables: the app stopped without finishing");
      if (Date.now() - t0 > limitMs) throw new Error("IQ Tables: gave up waiting for the app");
      let tick;
      await new Promise((r) => {
        wake = r;
        tick = setTimeout(r, 15000); // look at the clock now and then, even if nothing answers
      });
      clearTimeout(tick);
      if (closed) throw stopped();
    }
  };
  try {
    const setup = { op: "setup", rpc: opts.rpc, gateway: opts.gateway, devnet: !!opts.devnet, solana: opts.source === "solana" };
    let r = fail(call(setup));
    reach = { rpc: r.rpc, gateway: String(r.gateway).replace(/\/+$/, "") };
    const key = keyText(opts.key);
    if (key) r = fail(call({ ...setup, key, keyName: opts.keyName || "key file" }));
    await settle();
    const open = async (db) => {
      for (let i = 0; i < 200; i++) {
        const o = fail(call({ op: "open", db: String(db || "") }));
        if (o.ready) return o;
        await settle();
      }
      throw new Error(`IQ Tables: couldn't open ${db}`);
    };
    // one thing at a time: the app has one open database and one save
    let queue = Promise.resolve();
    const serial = (f) => (...args) => {
      const p = queue.then(() => f(...args));
      queue = p.catch(() => {});
      return p;
    };
    return {
      wallet: r.wallet || null,
      rpc: reach.rpc,
      gateway: reach.gateway,
      open: serial(open),
      pending: serial(async (db) => {
        await open(db);
        return fail(call({ op: "pending" }));
      }),
      sql: serial(async (db, statements) => {
        const database = await open(db);
        fail(call({ op: "sql", text: String(statements) }));
        const s = await settle();
        return {
          database,
          results: s.out.filter((o) => Array.isArray(o.cols)),
          messages: s.out.filter((o) => !Array.isArray(o.cols)).concat(s.log),
          commit: s.commit,
          pending: fail(call({ op: "pending" })),
        };
      }),
      write: serial(async (db, table, rows, o = {}) => {
        const database = await open(db);
        const text = importText(rows);
        let imported = false;
        for (let i = 0; i < 200 && !imported; i++) {
          imported = !!fail(call({ op: "import", table: String(table || ""), text, create: !!o.create, open: !!o.open })).ok;
          if (!imported) await settle();
        }
        if (!imported) throw new Error(`IQ Tables: couldn't read ${table}`);
        const s = await settle();
        return { database, messages: s.log, pending: fail(call({ op: "pending" })) };
      }),
      save: serial(async (db, o = {}) => {
        await open(db);
        const max = o.maxLamports == null ? undefined : Math.max(0, Math.floor(Number(o.maxLamports)));
        fail(call({ op: "save", max }));
        let s;
        try {
          s = await settle(o.onNote);
        } catch (e) {
          // waiting gave out: the save stops before its next step
          try {
            call({ op: "stop" });
          } catch (_) { /* closed */ }
          throw e;
        }
        const run = s.run;
        if (!run || run.state !== "done") {
          const why = (run && run.message) || s.log.filter((m) => !m.ok).map((m) => m.message).pop() || "the save didn't start";
          const e = new Error(`IQ Tables: not saved: ${why}`);
          e.spent = (run && run.spent) || 0;
          throw e;
        }
        return { spent: run.spent, notes: s.notes, messages: s.log };
      }),
      close,
    };
  } catch (e) {
    close();
    throw e;
  }
}

// ------------------------------------------------------------ command line

const USAGE = `IQ Tables from the command line (Node 18+, Deno or Bun):

  node iqt-loader.mjs read <table> --official <wallet> [--rows official|all|unofficial]
                           [--format csv|json|html|rows] [--rpc <url>] [--fresh] [--out <file>]
      Read a table: its records as CSV (default), JSON or an HTML table.

  node iqt-loader.mjs sql "<statements>" --db <database> [--format csv|json|table] [--key <file> --yes]
      Run SQL on a database, with the IQ Tables app itself: SELECT, JOIN, GROUP BY, SHOW TABLES,
      DESCRIBE…, and CREATE / ALTER / INSERT / UPDATE / DELETE under the tables' rules.
      Changes are saved only with --key and --yes (each transaction is simulated first);
      without --yes you see what saving would write and roughly cost. A save stops before
      any step that would take it past --max <SOL> (default: half again the estimate, plus
      0.01 SOL). "-" or --file <file> reads the statements from standard input or a file.

  node iqt-loader.mjs write <database> <table> <file.csv | file.json | -> [--create] [--open] [--key <file> --yes]
      Add rows to a table (a row whose id is already there updates it), under the table's
      rules. --create makes the table, and the database, if they don't exist; --open lets
      anyone add rows to a table it makes. Saved like sql.

  node iqt-loader.mjs mcp [--key <file>] [--budget <SOL>]
      Serve all of this to an AI assistant as tools (Model Context Protocol, over stdin and
      stdout): read_table, sql, write_rows, pending_changes, save_changes, encode_rows,
      decode_pack. Changes are saved only if the server has a key, and only up to --budget
      (default 0.1 SOL) per session: a step that would pass it isn't sent.

  node iqt-loader.mjs encode <file.csv | file.json | -> [--id <column>] [--mode dense|compressed|plain] [--out <file>]
      Pack rows into one IQ Tables row ({"id","p"}), as densely as the format allows.
      JSON input: an array of objects, or {"cols": [...], "rows": [[...]]}. The id column
      defaults to the first. (write does this for you; this is for IQ's SDK and other tools.)

  node iqt-loader.mjs decode <payload | row JSON | file | -> [--format json|csv] [--out <file>]
      Unpack one IQ Tables row into its records.

  node iqt-loader.mjs info
      Which decoder is used.

read, encode and decode use the sealed decoder: iqt-decoder.wasm next to this file, or
--decoder <file.wasm>, --pin <inscription>, or --repo <address> --owner <wallet>.
sql, write and mcp run the IQ Tables app from the site's index.html next to this file
(--app <file> for another copy). --devnet, --rpc <url>, --gateway <url> and --solana
(read straight from Solana) choose where they go.
--key <file> takes a Solana key file (the JSON array the Solana CLI and iqgit write) or a
file holding a base58 secret key; it stays in this process's memory and only signs.
"-" reads from standard input.

Other languages: only in WebAssembly, get good.
`;

function parseArgs(argv) {
  const pos = [];
  const flags = {};
  const bare = new Set(["help", "fresh", "yes", "create", "open", "devnet", "solana"]);
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "-h") flags.help = true;
    else if (a === "-y") flags.yes = true;
    else if (a.startsWith("--")) {
      const eq = a.indexOf("=");
      if (eq > 0) {
        const k = a.slice(2, eq);
        const v = a.slice(eq + 1);
        flags[k] = bare.has(k) ? !/^(false|0|no|off|)$/i.test(v) : v;
      } else if (bare.has(a.slice(2))) flags[a.slice(2)] = true;
      else flags[a.slice(2)] = argv[++i];
    } else pos.push(a);
  }
  return { pos, flags };
}

async function readStdin() {
  const chunks = [];
  for await (const c of process.stdin) chunks.push(typeof c === "string" ? new TextEncoder().encode(c) : c);
  const all = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let at = 0;
  for (const c of chunks) {
    all.set(c, at);
    at += c.length;
  }
  return new TextDecoder().decode(all);
}

export function sol(lamports) {
  if (lamports == null) return "an unknown amount of SOL";
  return `${(Number(lamports) / 1e9).toFixed(6).replace(/0+$/, "").replace(/\.$/, "")} SOL`;
}

// "To save in shop: parts: 3 rows added …" — what saving would do.
export function describePending(p) {
  const parts = (p.tables || []).map((t) => {
    const bits = [];
    if (t.create) bits.push("new table");
    if (t.drop) bits.push("dropped");
    if (t.structure) bits.push("structure changed");
    if (t.inserted) bits.push(`${t.inserted} row(s) added`);
    if (t.updated) bits.push(`${t.updated} changed`);
    if (t.deleted) bits.push(`${t.deleted} deleted`);
    return `${t.name}: ${bits.join(", ") || "renamed, or who can add rows changed"}`;
  });
  return `To save in ${p.db}${p.createDb ? " (a new database)" : ""}: ${parts.join("; ")}. About ${sol(p.lamports)} (${p.writes} write(s)).`;
}

function tableText(res) {
  const flat = (v) => String(v ?? "").replace(/\s+/g, " ");
  const rows = [res.cols, ...res.cells].map((r) => r.map(flat));
  const w = res.cols.map((_, i) => Math.min(48, Math.max(1, ...rows.map((r) => r[i].length))));
  const cut = (s, n) => (s.length > n ? s.slice(0, n - 1) + "…" : s);
  const line = (r) => r.map((c, i) => cut(c, w[i]).padEnd(w[i])).join("  ").trimEnd();
  return [line(rows[0]), w.map((n) => "-".repeat(n)).join("  "), ...rows.slice(1).map(line)].join("\n") + "\n";
}

// Run a command (`argv` without "node" and the file). `io` can replace
// fetch, fs, stdout, stderr and stdin lines (for tests). Resolves to an exit code.
export async function cli(argv, io = {}) {
  const fsName = "node:fs";
  const fs = io.fs || (await import(fsName));
  const out = io.stdout || ((t) => process.stdout.write(t));
  const err = io.stderr || ((t) => process.stderr.write(t));
  const { pos, flags } = parseArgs(argv);
  const cmd = pos[0];
  if (!cmd || flags.help || cmd === "help") {
    out(USAGE);
    return cmd || flags.help ? 0 : 2;
  }
  const decoder = {};
  if (flags.decoder) decoder.wasm = fs.readFileSync(flags.decoder);
  if (flags.pin) decoder.pin = flags.pin;
  if (flags.repo) decoder.repo = flags.repo;
  if (flags.owner) decoder.owner = flags.owner;
  if (!decoder.wasm && !decoder.pin && !decoder.repo) {
    // the decoder that `iqgit clone` leaves next to this file
    try {
      decoder.wasm = fs.readFileSync(new URL("./iqt-decoder.wasm", import.meta.url));
    } catch (_) { /* none here */ }
  }
  const base = { decoder, gateway: flags.gateway, store: null, ...(io.fetch ? { fetch: io.fetch } : {}) };
  const keyFile = () => {
    if (!flags.key) return null;
    try {
      return fs.readFileSync(flags.key, "utf8");
    } catch (_) {
      // (never echo what was given: it might be a secret key typed in by mistake)
      throw new Error("couldn't read the --key file: give the path of a key file, not the key itself");
    }
  };
  const appOpts = () => ({
    rpc: flags.rpc,
    gateway: flags.gateway,
    devnet: !!flags.devnet,
    source: flags.solana ? "solana" : "gateway",
    app: flags.app,
    key: keyFile(),
    keyName: "the --key file",
    fs,
    ...(io.fetch ? { fetch: io.fetch } : {}),
  });
  const source = async (arg) => {
    if (arg === undefined || arg === "-") return io.stdin !== undefined ? io.stdin : readStdin();
    return fs.existsSync(arg) ? fs.readFileSync(arg, "utf8") : null;
  };
  const write = (t) => {
    if (flags.out) fs.writeFileSync(flags.out, t);
    else out(t.endsWith("\n") ? t : t + "\n");
  };
  const say = (m) => err(`${m.ok === false ? "✗" : "✓"} ${m.message}\n`);
  // Changes left after sql or write: saved with --key and --yes, else shown.
  const finish = async (app, db, pending, failed) => {
    if (!pending.changes) return failed ? 1 : 0;
    err(describePending(pending) + "\n");
    if (failed) {
      err("Not saved: a statement failed.\n");
      return 1;
    }
    if (!flags.yes) {
      err(`Not saved (a dry run). Add --yes${flags.key ? "" : " and --key <wallet key file>"} to save.\n`);
      return 0;
    }
    if (!flags.key) {
      err("Saving signs transactions: add --key <wallet key file>.\n");
      return 1;
    }
    const max = flags.max != null ? Math.round(Number(flags.max) * 1e9) : Math.round(pending.lamports * 1.5) + 10_000_000;
    if (!(max >= 0)) throw new Error("--max is an amount of SOL, e.g. 0.2");
    err(`Saving (at most ${sol(max)})…\n`);
    try {
      const s = await app.save(db, { maxLamports: max, onNote: (n) => err(`  ${n.ok ? "" : "! "}${n.message}\n`) });
      err(`Saved. Spent ${sol(s.spent)}.\n`);
      return 0;
    } catch (e) {
      if (e.spent) err(`Spent ${sol(e.spent)} on the steps that went through.\n`);
      throw e;
    }
  };
  switch (cmd) {
    case "read": {
      if (!pos[1]) throw new Error("read needs a table address");
      const r = await readTable({
        ...base,
        table: pos[1],
        official: flags.official,
        rows: flags.rows,
        format: flags.format || "csv",
        rpc: flags.rpc,
        source: flags.source,
        fresh: !!flags.fresh,
        maxRows: flags["max-rows"] ? Number(flags["max-rows"]) : undefined,
        cacheMs: 0,
      });
      write(r.data ?? JSON.stringify({ cols: r.cols, rows: r.rows }));
      err(`${r.count} row(s) from ${r.source === "gateway" ? "IQ's gateway" : "Solana"}${r.notes.length ? "\n" + r.notes.join("\n") : ""}\n`);
      return 0;
    }
    case "sql": {
      if (!flags.db) throw new Error('sql needs --db <database name>, e.g. sql "SHOW TABLES" --db shop');
      const text = flags.file ? fs.readFileSync(flags.file, "utf8") : pos.length > 1 && pos[1] !== "-" ? pos.slice(1).join(" ") : await source("-");
      const fmt = flags.format || "csv";
      if (!["csv", "json", "table"].includes(fmt)) throw new Error("--format is csv, json or table");
      const app = await connect(appOpts());
      try {
        const r = await app.sql(flags.db, text);
        if (!r.database.exists) err(`("${flags.db}" isn't on chain yet: saving changes would create it)\n`);
        const shown = r.results.map((res) => (fmt === "json" ? res.json + "\n" : fmt === "table" ? tableText(res) : res.csv));
        if (shown.length) write(shown.join(fmt === "json" ? "" : "\n"));
        for (const res of r.results) err(`${res.count} row(s)${res.note && !/^\d+ rows?\b/.test(res.note) ? ` · ${res.note}` : ""}\n`);
        for (const m of r.messages) say(m);
        return await finish(app, flags.db, r.pending, r.messages.some((m) => m.ok === false));
      } finally {
        app.close();
      }
    }
    case "write": {
      const [, db, table, file] = pos;
      if (!db || !table) throw new Error("write needs a database, a table and a file: write shop parts parts.csv");
      const text = await source(file);
      if (text == null) throw new Error(`no such file: ${file}`);
      const rows = text.trim().startsWith("{") ? JSON.parse(text) : text; // {"cols","rows"}
      const app = await connect(appOpts());
      try {
        const r = await app.write(db, table, rows, { create: !!flags.create, open: !!flags.open });
        for (const m of r.messages) say(m);
        return await finish(app, db, r.pending, r.messages.some((m) => m.ok === false));
      } finally {
        app.close();
      }
    }
    case "mcp":
      await mcpServer({ ...base, app: appOpts(), budget: flags.budget }, io);
      return 0;
    case "encode": {
      const text = await source(pos[1]);
      if (text == null) throw new Error(`no such file: ${pos[1]}`);
      const t = text.trim();
      const data = t.startsWith("[") || t.startsWith("{") ? JSON.parse(t) : text;
      const r = await encodeRows(data, { ...base, id: flags.id, mode: flags.mode });
      write(JSON.stringify(r.row));
      err(
        `${r.records} record(s), ${r.columns} column(s), id column "${r.idColumn}": ${r.bytes} bytes ` +
          `(${r.compressed ? "compressed" : "plain"}; ${r.raw} bytes unpacked)` +
          (r.duplicateIds ? `\n${r.duplicateIds} row(s) repeat an id: the later one wins when read` : "") +
          "\n",
      );
      return 0;
    }
    case "decode": {
      const arg = pos[1];
      const inline = arg && arg !== "-" && !fs.existsSync(arg);
      const text = inline ? arg : await source(arg);
      const r = await decodePack(text.trim(), { ...base, format: flags.format || "json" });
      write(r.text);
      err(r.structure ? "a table-structure record\n" : `${r.records} record(s)${r.deleted && r.deleted.length ? `, ${r.deleted.length} deleted` : ""}\n`);
      return 0;
    }
    case "info":
      out(JSON.stringify(await decoderInfo(base)) + "\n");
      return 0;
    default:
      err(`unknown command: ${cmd}\n\n${USAGE}`);
      return 2;
  }
}

// ------------------------------------------------------------------- MCP

// The Model Context Protocol over stdin/stdout (one JSON-RPC message per
// line), so AI assistants can use IQ Tables as tools. Staged changes last for
// the session; saving needs the server's key and stays within its budget.
// Options: the decoder options for read/encode/decode, app (connect's
// options, key included), budget (SOL). `io`: lines (async iterable),
// send(message), stderr — for tests; stdin and stdout otherwise.
const MCP_VERSIONS = ["2025-06-18", "2025-03-26", "2024-11-05"];

async function* stdinLines() {
  let rest = "";
  const dec = new TextDecoder();
  for await (const c of process.stdin) {
    rest += typeof c === "string" ? c : dec.decode(c, { stream: true });
    let i;
    while ((i = rest.indexOf("\n")) >= 0) {
      yield rest.slice(0, i);
      rest = rest.slice(i + 1);
    }
  }
  if (rest.trim()) yield rest;
}

export async function mcpServer(opts = {}, io = {}) {
  const send = io.send || ((m) => process.stdout.write(JSON.stringify(m) + "\n"));
  const lines = io.lines || stdinLines();
  const appOpts = opts.app || {};
  const canSave = !!keyText(appOpts.key);
  const budget = Math.round(Number(opts.budget ?? 0.1) * 1e9);
  if (!(budget >= 0)) throw new Error("--budget is an amount of SOL, e.g. 0.05");
  let spent = 0;
  let app = null;
  const getApp = () => (app = app || connect(appOpts));
  const dbArg = { type: "string", description: "the database's name (as created in IQ Tables)" };
  const tools = [
    {
      name: "read_table",
      description:
        "Read an IQ Labs table on Solana by its address, with the sealed IQ Tables decoder. Only rows written by the official wallet (the database's creator) unless rows asks for others.",
      inputSchema: {
        type: "object",
        properties: {
          table: { type: "string", description: "the table's address" },
          official: { type: "string", description: "the wallet whose rows are official: the database's creator" },
          rows: { type: "string", enum: ["official", "all", "unofficial"] },
          format: { type: "string", enum: ["json", "csv"] },
        },
        required: ["table", "official"],
      },
    },
    {
      name: "sql",
      description:
        "Run SQL (MySQL dialect) on an IQ Tables database, by name. SELECT, JOIN, GROUP BY, SHOW TABLES, DESCRIBE read it; CREATE, ALTER, INSERT, UPDATE and DELETE stage changes, checked against the tables' types, keys and rules. Staged changes last for this session" +
        (canSave ? " and are written to Solana by save_changes." : "; this server has no key, so they can't be saved."),
      inputSchema: { type: "object", properties: { database: dbArg, query: { type: "string", description: "one or more SQL statements" } }, required: ["database", "query"] },
    },
    {
      name: "write_rows",
      description:
        "Stage rows for a table: added, or replacing the row with the same id. Give rows (objects keyed by column) or csv. create makes the table (and database) if missing; open lets anyone add rows to a table it makes.",
      inputSchema: {
        type: "object",
        properties: {
          database: dbArg,
          table: { type: "string" },
          rows: { type: "array", items: { type: "object" } },
          csv: { type: "string", description: "CSV text with a header row (instead of rows)" },
          create: { type: "boolean" },
          open: { type: "boolean" },
        },
        required: ["database", "table"],
      },
    },
    {
      name: "pending_changes",
      description: "What saving a database's staged changes would write, and roughly what it would cost.",
      inputSchema: { type: "object", properties: { database: dbArg }, required: ["database"] },
    },
    ...(canSave
      ? [
          {
            name: "save_changes",
            description: `Write a database's staged changes to Solana, signed by this server's wallet. Costs SOL: at most ${sol(budget)} in this session, less what's already been spent.`,
            inputSchema: { type: "object", properties: { database: dbArg }, required: ["database"] },
          },
        ]
      : []),
    {
      name: "encode_rows",
      description: 'Pack rows into one IQ Tables row ({"id","p"}) as densely as possible, for writing with IQ\'s SDK.',
      inputSchema: {
        type: "object",
        properties: {
          rows: { type: "array", items: { type: "object" } },
          csv: { type: "string" },
          id: { type: "string", description: "the id column (default: the first)" },
          mode: { type: "string", enum: ["dense", "compressed", "plain"] },
        },
      },
    },
    {
      name: "decode_pack",
      description: "Unpack one IQ Tables row (or its payload, starting IQT1) into its records.",
      inputSchema: { type: "object", properties: { pack: { type: "string" }, format: { type: "string", enum: ["json", "csv"] } }, required: ["pack"] },
    },
  ];
  // Results as JSON text; rows keep their exact values (no float round trip).
  const sqlText = (r) =>
    `{"results":[${r.results.map((x) => `{"title":${JSON.stringify(x.title)},"count":${x.count},"rows":${x.json}}`).join(",")}],` +
    `"messages":${JSON.stringify(r.messages)},"pending":${JSON.stringify(r.pending.changes ? describePending(r.pending) : "nothing to save")}}`;
  const run = async (name, a) => {
    switch (name) {
      case "read_table": {
        const r = await readTable({ ...opts, app: undefined, budget: undefined, table: a.table, official: a.official, rows: a.rows, format: a.format === "csv" ? "csv" : "json", cacheMs: 0 });
        return r.data;
      }
      case "sql":
        return sqlText(await (await getApp()).sql(a.database, a.query));
      case "write_rows": {
        const rows = a.csv != null ? String(a.csv) : a.rows;
        const r = await (await getApp()).write(a.database, a.table, rows, { create: !!a.create, open: !!a.open });
        return JSON.stringify({ messages: r.messages, pending: r.pending.changes ? describePending(r.pending) : "nothing to save" });
      }
      case "pending_changes": {
        const p = await (await getApp()).pending(a.database);
        return JSON.stringify({ ...p, summary: p.changes ? describePending(p) : "nothing to save" });
      }
      case "save_changes": {
        if (!canSave) throw new Error("this server was started without a key, so it can't save");
        const a2 = await getApp();
        const p = await a2.pending(a.database);
        if (!p.changes) return "Nothing to save.";
        const left = budget - spent;
        if (p.lamports > left) {
          throw new Error(`saving would cost about ${sol(p.lamports)}, more than the ${sol(Math.max(0, left))} left of this session's budget (start the server with a bigger --budget)`);
        }
        let s;
        try {
          s = await a2.save(a.database, { maxLamports: left });
        } catch (e) {
          spent += e.spent || 0; // steps that went through still count
          throw e;
        }
        spent += s.spent ?? p.lamports;
        return JSON.stringify({ saved: true, spent: sol(s.spent), budgetLeft: sol(Math.max(0, budget - spent)), notes: s.notes.map((n) => n.message) });
      }
      case "encode_rows": {
        const data = a.csv != null ? String(a.csv) : a.rows;
        const r = await encodeRows(data, { ...opts, id: a.id, mode: a.mode });
        return JSON.stringify(r);
      }
      case "decode_pack":
        return (await decodePack(String(a.pack).trim(), { ...opts, format: a.format === "csv" ? "csv" : "json" })).text;
      default:
        throw new Error(`no tool called ${name}`);
    }
  };
  const handle = async (m) => {
    const reply = (result) => ({ jsonrpc: "2.0", id: m.id, result });
    switch (m.method) {
      case "initialize": {
        const asked = m.params && m.params.protocolVersion;
        return reply({
          protocolVersion: MCP_VERSIONS.includes(asked) ? asked : MCP_VERSIONS[0],
          capabilities: { tools: {} },
          serverInfo: { name: "iq-tables", version: "1.0.0" },
          instructions:
            "IQ Tables: databases on Solana (IQ Labs). Use sql to read and change a database by name; changes are staged until save_changes (if available) writes them, which costs SOL.",
        });
      }
      case "ping":
        return reply({});
      case "tools/list":
        return reply({ tools });
      case "tools/call": {
        const name = m.params && m.params.name;
        try {
          const text = await run(name, (m.params && m.params.arguments) || {});
          return reply({ content: [{ type: "text", text: String(text) }] });
        } catch (e) {
          return reply({ content: [{ type: "text", text: String((e && e.message) || e).replace(/^IQ Tables: /, "") }], isError: true });
        }
      }
      default:
        return m.id === undefined ? null : { jsonrpc: "2.0", id: m.id, error: { code: -32601, message: `unknown method ${m.method}` } };
    }
  };
  try {
    for await (const line of lines) {
      if (!String(line).trim()) continue;
      let m;
      try {
        m = JSON.parse(line);
      } catch (_) {
        send({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "not JSON" } });
        continue;
      }
      const r = await handle(m);
      if (r && m.id !== undefined) send(r);
    }
  } finally {
    if (app) (await app.catch(() => null))?.close();
  }
}

// Run as a program: `node iqt-loader.mjs …` (not when imported).
if (typeof process !== "undefined" && Array.isArray(process.argv) && process.argv[1] && typeof document === "undefined") {
  let main = import.meta.main === true;
  if (!main) {
    try {
      const urlName = "node:url";
      const fsName = "node:fs";
      const { pathToFileURL } = await import(urlName);
      const { realpathSync } = await import(fsName);
      main = pathToFileURL(realpathSync(process.argv[1])).href === import.meta.url;
    } catch (_) { /* not Node-like */ }
  }
  if (main) {
    process.exitCode = await cli(process.argv.slice(2)).catch((e) => {
      process.stderr.write(`${(e && e.message) || e}\n`);
      return 1;
    });
  }
}
