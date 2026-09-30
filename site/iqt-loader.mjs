// iqt-loader.mjs — reads IQ Tables tables anywhere JavaScript runs.
// Node 18+, Deno, Bun or a web page. No dependencies. MIT license.
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

async function run(opts) {
  const ctx = context(opts);
  const dec = await mainDecoder(ctx);
  ctx.running = dec.ref;
  const call = open(dec.module);
  const config = {
    table: opts.table,
    official: opts.official,
    rows: opts.rows || "official",
    format: opts.format || "rows",
    gateway: ctx.gw,
    rpc: ctx.rpc ? RPC_STANDIN : undefined,
    source: opts.source || "auto",
    fresh: !!opts.fresh,
    maxRows: opts.maxRows,
  };
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
  const key = JSON.stringify([opts.table, opts.official, opts.rows, opts.format, opts.gateway, opts.rpc, opts.source, opts.fresh, opts.maxRows, d.repo, d.owner, d.pin, d.wasm ? local.get(d.wasm).id : 0]);
  const cacheMs = opts.cacheMs ?? 60000;
  const now = Date.now();
  if (results.size > 64) for (const [k, v] of results) if (now - v.at >= v.ms) results.delete(k);
  const hit = results.get(key);
  if (hit && cacheMs > 0 && now - hit.at < cacheMs) return hit.value;
  const value = run(opts);
  if (cacheMs > 0) results.set(key, { at: now, ms: cacheMs, value });
  try {
    return await value;
  } catch (e) {
    if (results.get(key)?.value === value) results.delete(key);
    throw e;
  }
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
