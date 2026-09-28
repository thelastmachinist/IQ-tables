// IQ Tables — browser bridge.
// Browsers can only run WebAssembly through JavaScript, so this file gives the
// Rust app the few things it can't do itself: the DOM, fetch, localStorage,
// the clock/RNG, and the Solana wallet (via the Wallet Standard). No app logic.
(async () => {
  "use strict";
  const root = document.getElementById("app");
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  let w = null; // wasm exports
  let staged = null;
  let rendering = false;
  const queue = [];

  const mem = () => new Uint8Array(w.memory.buffer);
  const str = (p, l) => dec.decode(mem().subarray(p, p + l));
  const bytes = (p, l) => mem().slice(p, p + l);
  const put = (u8) => {
    const p = w.alloc(u8.length);
    mem().set(u8, p);
    return p;
  };
  const stage = (u8) => { staged = u8; return u8.length; };
  const done = (id, ok, status, data) => {
    const u8 = typeof data === "string" ? enc.encode(data) : data || new Uint8Array(0);
    const p = put(u8);
    w.on_async(id, ok ? 1 : 0, status, p, u8.length);
  };

  // ---- Wallet Standard discovery (Phantom, Solflare, Backpack, …)
  const wallets = [];
  let current = null; // { wallet, account }
  const api = {
    register(...ws) {
      for (const x of ws) if (!wallets.includes(x)) wallets.push(x);
      if (w) ev("wallets", "", "", "");
      return () => {};
    },
  };
  window.addEventListener("wallet-standard:register-wallet", (e) => { try { e.detail(api); } catch (_) {} });
  try { window.dispatchEvent(new CustomEvent("wallet-standard:app-ready", { detail: api })); } catch (_) {}
  const solana = () => wallets.filter((x) => (x.chains || []).some((c) => String(c).startsWith("solana:")) && x.features && x.features["standard:connect"]);
  const errText = (e) => String((e && (e.message || e.name)) || e || "error");

  // ---- rendering with focus/selection preserved across innerHTML swaps
  function render(html) {
    rendering = true;
    const a = document.activeElement;
    const id = a && a.id;
    let s0 = null, s1 = null;
    try { s0 = a.selectionStart; s1 = a.selectionEnd; } catch (_) {}
    const openDetails = [...root.querySelectorAll("details[open] > summary")].map((s) => s.textContent);
    root.innerHTML = html;
    for (const s of root.querySelectorAll("details > summary")) if (openDetails.includes(s.textContent)) s.parentElement.open = true;
    if (id) {
      const n = document.getElementById(id);
      if (n) {
        n.focus({ preventScroll: true });
        try { if (s0 !== null) n.setSelectionRange(s0, s1); } catch (_) {}
      }
    }
    rendering = false;
    while (queue.length) ev(...queue.shift());
  }

  const imports = {
    host: {
      log: (p, l) => console.log("[iq-tables]", str(p, l)),
      render: (p, l) => render(str(p, l)),
      fetch: (id, mp, ml, up, ul, bp, bl, cp, cl) => {
        const method = str(mp, ml), url = str(up, ul);
        const init = { method };
        if (bl) init.body = str(bp, bl);
        if (cl) init.headers = { "content-type": str(cp, cl) };
        fetch(url, init)
          .then(async (r) => done(id, true, r.status, await r.text()))
          .catch((e) => done(id, false, 0, errText(e)));
      },
      storage_get: (kp, kl) => {
        let v = null;
        try { v = localStorage.getItem(str(kp, kl)); } catch (_) {}
        return v === null ? -1 : stage(enc.encode(v));
      },
      take: (p) => { mem().set(staged, p); staged = null; },
      storage_set: (kp, kl, vp, vl) => { try { localStorage.setItem(str(kp, kl), str(vp, vl)); } catch (_) {} },
      now: () => Date.now(),
      random: (p, l) => crypto.getRandomValues(mem().subarray(p, p + l)),
      wallets: () => stage(enc.encode(JSON.stringify(solana().map((x) => ({ name: x.name, icon: x.icon || "" }))))),
      wallet_connect: (id, np, nl) => {
        const name = str(np, nl);
        const x = solana().find((y) => y.name === name);
        if (!x) return done(id, false, 0, "wallet not found");
        x.features["standard:connect"].connect()
          .then((r) => {
            const acct = (r && r.accounts && r.accounts[0]) || x.accounts[0];
            if (!acct) throw new Error("no account");
            current = { wallet: x, account: acct };
            done(id, true, 0, JSON.stringify({ name: x.name, address: acct.address }));
          })
          .catch((e) => done(id, false, 0, errText(e)));
      },
      wallet_disconnect: () => {
        try { current && current.wallet.features["standard:disconnect"] && current.wallet.features["standard:disconnect"].disconnect(); } catch (_) {}
        current = null;
      },
      wallet_sign_message: (id, mp, ml) => {
        const message = bytes(mp, ml);
        const f = current && current.wallet.features["solana:signMessage"];
        if (!f) return done(id, false, 0, "this wallet can't sign messages");
        f.signMessage({ account: current.account, message })
          .then((out) => done(id, true, 0, new Uint8Array(out[0].signature)))
          .catch((e) => done(id, false, 0, errText(e)));
      },
      wallet_sign_and_send: (id, tp, tl, cp, cl) => {
        const transaction = bytes(tp, tl), chain = str(cp, cl);
        const f = current && current.wallet.features["solana:signAndSendTransaction"];
        if (!f) return done(id, false, 0, "this wallet can't send transactions");
        f.signAndSendTransaction({ account: current.account, chain, transaction })
          .then((out) => done(id, true, 0, new Uint8Array(out[0].signature)))
          .catch((e) => done(id, false, 0, errText(e)));
      },
      download: (np, nl, mp, ml, dp, dl) => {
        const a = document.createElement("a");
        a.href = URL.createObjectURL(new Blob([bytes(dp, dl)], { type: str(mp, ml) }));
        a.download = str(np, nl);
        document.body.appendChild(a);
        a.click();
        setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 5000);
      },
      copy: (p, l) => { try { navigator.clipboard.writeText(str(p, l)); } catch (_) {} },
      timer: (id, ms) => setTimeout(() => done(id, true, 0, ""), ms),
      set_hash: (p, l) => {
        const h = str(p, l);
        if (location.hash !== h) location.hash = h;
        else ev("route", "", "", h);
      },
    },
  };

  // ---- load the wasm (embedded for single-file builds, else fetched)
  let module;
  const embedded = document.getElementById("wasm-b64");
  if (embedded) {
    const bin = Uint8Array.from(atob(embedded.textContent.trim()), (c) => c.charCodeAt(0));
    module = await WebAssembly.instantiate(bin, imports);
  } else {
    module = await WebAssembly.instantiateStreaming(fetch("iq_tables.wasm"), imports);
  }
  w = module.instance.exports;

  function ev(kind, action, arg, value) {
    if (rendering) { queue.push([kind, action, arg, value]); return; }
    const parts = [kind, action, arg, value].map((s) => enc.encode(String(s == null ? "" : s)));
    const ps = parts.map(put);
    w.on_event(ps[0], parts[0].length, ps[1], parts[1].length, ps[2], parts[2].length, ps[3], parts[3].length);
  }

  // Typing in a field and then clicking a button fires "change" on
  // mousedown; re-rendering at that moment would swap the button out from
  // under the pointer and swallow the click. Hold changes until the click.
  let pointerDown = false;
  const held = [];
  const flush = () => { while (held.length) ev(...held.shift()); };
  document.addEventListener("pointerdown", () => { pointerDown = true; }, true);
  document.addEventListener("pointerup", () => { pointerDown = false; setTimeout(flush, 0); }, true);
  document.addEventListener("click", (e) => {
    const t = e.target.closest("[data-a]");
    flush();
    if (!t || t.disabled) return;
    if (t.tagName === "TR" && e.target.closest("a,button,input,select,textarea,label")) return;
    e.preventDefault();
    ev("click", t.dataset.a, t.dataset.arg, t.dataset.val);
  });
  document.addEventListener("change", (e) => {
    const t = e.target;
    if (!t.dataset) return;
    if (t.dataset.in) {
      const args = ["change", t.dataset.in, t.dataset.arg, t.type === "checkbox" ? String(t.checked) : t.value];
      if (pointerDown && t.type !== "checkbox" && t.tagName !== "SELECT") held.push(args); else ev(...args);
    }
    if (t.dataset.file && t.files && t.files[0]) {
      const f = t.files[0];
      f.text().then((txt) => ev("file", t.dataset.file, t.dataset.arg, txt));
      t.value = "";
    }
  });
  let liveTimer = null;
  document.addEventListener("input", (e) => {
    const t = e.target;
    if (!t.dataset || !t.dataset.live) return;
    clearTimeout(liveTimer);
    liveTimer = setTimeout(() => ev("input", t.dataset.live, t.dataset.arg, t.value), 140);
  });
  document.addEventListener("keydown", (e) => {
    const t = e.target;
    if (e.key !== "Enter" || !t.dataset) return;
    if (t.dataset.enter) {
      e.preventDefault();
      if (t.dataset.in) ev("change", t.dataset.in, t.dataset.arg, t.value);
      ev("enter", t.dataset.enter, t.dataset.arg, t.value);
    } else if (t.dataset.in && t.tagName === "INPUT") {
      e.preventDefault();
      t.blur();
    }
  });
  document.addEventListener("click", (e) => {
    // close the wallet menu when clicking elsewhere
    const m = document.querySelector(".menu");
    if (m && !e.target.closest(".wallet")) ev("click", "connect-menu", "", "");
  }, true);
  window.addEventListener("hashchange", () => ev("route", "", "", location.hash));

  w.start();
  ev("route", "", "", location.hash);
  // wallets that register a moment after load
  setTimeout(() => ev("wallets", "", "", ""), 600);
})().catch((e) => {
  document.getElementById("app").innerHTML =
    '<div class="card bad" style="margin:2rem">IQ Tables failed to start: ' +
    String(e && e.message || e).replace(/[<&]/g, "") + "</div>";
});
