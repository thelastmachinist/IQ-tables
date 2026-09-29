// IQ Tables — browser bridge.
// Browsers can only run WebAssembly through JavaScript, so this file gives the
// Rust app the few things it can't do itself: the DOM, fetch, localStorage,
// the clock, secure randomness and files. No app logic and no wallet extension:
// keys live in the user's account file and all signing happens in Rust.
(async () => {
  "use strict";
  const root = document.getElementById("app");
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  let w = null; // wasm exports
  let staged = null;
  let rendering = false;
  let scrollSel = false;
  // Calls into the wasm never nest: an event raised while the app is busy
  // (a render swapping out a focused field, a result arriving synchronously)
  // waits until the current call returns instead of being lost.
  let depth = 0;
  const pending = [];
  const call = (f) => {
    if (depth > 0) { pending.push(f); return undefined; }
    depth++;
    try { return f(); } finally {
      while (pending.length) { try { pending.shift()(); } catch (e) { console.error(e); } }
      depth--;
    }
  };

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
    call(() => { const p = put(u8); w.on_async(id, ok ? 1 : 0, status, p, u8.length); });
  };

  const errText = (e) => String((e && (e.message || e.name)) || e || "error");
  const b64 = (u8) => {
    let s = "";
    for (let i = 0; i < u8.length; i += 0x8000) s += String.fromCharCode.apply(null, u8.subarray(i, i + 0x8000));
    return btoa(s);
  };

  // ---- rendering with focus/selection preserved across innerHTML swaps
  function render(html) {
    rendering = true;
    const a = document.activeElement;
    const id = a && a.id;
    let s0 = null, s1 = null;
    try { s0 = a.selectionStart; s1 = a.selectionEnd; } catch (_) {}
    const openDetails = [...root.querySelectorAll("details[open] > summary")].map((s) => s.textContent);
    // text typed but not yet committed (no "change" yet) survives a render
    // triggered by something else, e.g. a balance arriving mid-typing
    const fieldKey = (n) => (n.id || n.dataset.in || n.dataset.keys) ? `${n.id}|${n.dataset.in || ""}|${n.dataset.arg || ""}` : null;
    const typed = new Map();
    for (const n of root.querySelectorAll("input:not([type=checkbox]):not([type=radio]):not([type=file]), textarea")) {
      // (a value already sent to the app is the app's to show or clear)
      if (n.value !== n.defaultValue && n.dataset.sent !== n.value) { const k = fieldKey(n); if (k) typed.set(k, [n.defaultValue, n.value]); }
    }
    root.innerHTML = html;
    if (typed.size) {
      for (const n of root.querySelectorAll("input, textarea")) {
        const k = fieldKey(n), t = k && typed.get(k);
        if (t && n.value === t[0]) n.value = t[1];
      }
    }
    for (const s of root.querySelectorAll("details > summary")) if (openDetails.includes(s.textContent)) s.parentElement.open = true;
    if (id) {
      const n = document.getElementById(id);
      if (n) {
        n.focus({ preventScroll: true });
        try { if (s0 !== null) n.setSelectionRange(s0, s1); } catch (_) {}
      }
    }
    // data-focus="force" (a cell being edited) always takes focus, caret at
    // the end; "soft" (the sheet) only when nothing else has it
    const act = document.activeElement;
    const f = root.querySelector('[data-focus="force"]') || ((!act || act === document.body) ? root.querySelector('[data-focus="soft"]') : null);
    if (f && f !== act) {
      f.focus({ preventScroll: true });
      if (f.dataset.focus === "force") { try { const n = f.value.length; f.setSelectionRange(n, n); } catch (_) {} }
    }
    if (scrollSel) {
      scrollSel = false;
      const c = root.querySelector(".sheet td.sel, .sheet td.editing");
      if (c) c.scrollIntoView({ block: "nearest", inline: "nearest" });
    }
    rendering = false;
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
      download: (np, nl, mp, ml, dp, dl) => {
        const a = document.createElement("a");
        a.href = URL.createObjectURL(new Blob([bytes(dp, dl)], { type: str(mp, ml) }));
        a.download = str(np, nl);
        document.body.appendChild(a);
        a.click();
        setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 5000);
      },
      copy: (p, l) => {
        const s = str(p, l);
        const fallback = () => {
          const t = document.createElement("textarea");
          t.value = s; t.setAttribute("readonly", ""); t.style.cssText = "position:fixed;opacity:0";
          document.body.appendChild(t); t.select();
          try { document.execCommand("copy"); } catch (_) {}
          t.remove();
        };
        try { navigator.clipboard.writeText(s).catch(fallback); } catch (_) { fallback(); }
      },
      timer: (id, ms) => setTimeout(() => done(id, true, 0, ""), ms),
      passkey: (id, p, l) => {
        const req = JSON.parse(str(p, l));
        const out = (o) => done(id, true, 200, JSON.stringify(o));
        const rnd = (n) => crypto.getRandomValues(new Uint8Array(n));
        const b64u = (buf) => b64(new Uint8Array(buf)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
        const prfOf = (c) => { const r = c.getClientExtensionResults(); return r && r.prf && r.prf.results && r.prf.results.first ? b64(new Uint8Array(r.prf.results.first)) : null; };
        (async () => {
          if (!window.PublicKeyCredential || !navigator.credentials) return out({ ok: false, unsupported: true });
          try {
            if (PublicKeyCredential.getClientCapabilities) {
              const caps = await PublicKeyCredential.getClientCapabilities();
              if (caps && caps["extension:prf"] === false) return out({ ok: false, unsupported: true });
            }
          } catch (_) {}
          const salt = Uint8Array.from(atob(req.salt), (c) => c.charCodeAt(0));
          const prf = { eval: { first: salt } };
          try {
            let cred, key = null;
            if (req.mode === "create") {
              cred = await navigator.credentials.create({ publicKey: {
                rp: { name: req.name }, user: { id: rnd(16), name: req.name + " account", displayName: req.name + " account" },
                challenge: rnd(32), pubKeyCredParams: [{ type: "public-key", alg: -7 }, { type: "public-key", alg: -257 }],
                authenticatorSelection: { residentKey: "required", requireResidentKey: true, userVerification: "required" },
                extensions: { prf } } });
              key = prfOf(cred);
              const ext = cred.getClientExtensionResults();
              if (!key && !(ext.prf && ext.prf.enabled)) return out({ ok: false, unsupported: true });
              if (!key) {
                // some authenticators only evaluate the PRF when signing in
                const got = await navigator.credentials.get({ publicKey: { challenge: rnd(32), allowCredentials: [{ type: "public-key", id: cred.rawId }], userVerification: "required", extensions: { prf } } });
                key = prfOf(got);
              }
            } else {
              cred = await navigator.credentials.get({ publicKey: { challenge: rnd(32), userVerification: "required", extensions: { prf } } });
              key = prfOf(cred);
            }
            if (!key) return out({ ok: false, unsupported: true });
            out({ ok: true, cred: b64u(cred.rawId), prf: key });
          } catch (e) { out({ ok: false, error: (e && e.name) || "Error", message: errText(e) }); }
        })();
      },
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
    call(() => {
      const parts = [kind, action, arg, value].map((s) => enc.encode(String(s == null ? "" : s)));
      const ps = parts.map(put);
      w.on_event(ps[0], parts[0].length, ps[1], parts[1].length, ps[2], parts[2].length, ps[3], parts[3].length);
    });
  }

  // Typing in a field and then clicking a button fires "change" on
  // mousedown; re-rendering at that moment would swap the button out from
  // under the pointer and swallow the click. Hold changes until the click.
  let pointerDown = false;
  const held = [];
  const flush = () => { while (held.length) ev(...held.shift()); };
  const commitEditor = (except) => {
    const ed = root.querySelector("[data-editor]");
    if (ed && ed !== except && !ed.dataset.done) { ed.dataset.done = "1"; ev("edit", "commit", ed.dataset.arg, ed.value); }
  };
  document.addEventListener("pointerdown", (e) => { pointerDown = true; commitEditor(e.target); }, true);
  document.addEventListener("pointerup", () => { pointerDown = false; setTimeout(flush, 0); }, true);
  document.addEventListener("click", (e) => {
    // a link to the page already showing still re-opens it (e.g. back to Browse)
    const l = e.target.closest && e.target.closest("a[href^='#/']");
    if (l && !e.ctrlKey && !e.metaKey && l.getAttribute("href") === location.hash) {
      e.preventDefault();
      flush();
      ev("route", "", "", location.hash);
      return;
    }
    const t = e.target.closest("[data-a]");
    flush();
    if (!t || t.disabled) return;
    if (t.tagName === "TR" && e.target.closest("a,button,input,select,textarea,label")) return;
    e.preventDefault();
    ev("click", t.dataset.a, t.dataset.arg, t.dataset.val);
  });
  // Text fields report their value when they lose focus and differ from
  // what the app rendered (the browser's own "change" misses a field that
  // was re-rendered while being typed in).
  const textField = (t) => t.tagName === "TEXTAREA" || (t.tagName === "INPUT" && !/^(checkbox|radio|file|button|submit)$/.test(t.type));
  document.addEventListener("focusout", (e) => {
    const t = e.target;
    if (!t.dataset || !t.dataset.in || !textField(t)) return;
    if (t.value === (t.dataset.sent != null ? t.dataset.sent : t.defaultValue)) return;
    t.dataset.sent = t.value;
    const args = ["change", t.dataset.in, t.dataset.arg, t.value];
    if (pointerDown) held.push(args); else ev(...args);
  }, true);
  document.addEventListener("change", (e) => {
    const t = e.target;
    if (!t.dataset) return;
    if (t.dataset.in && !textField(t)) ev("change", t.dataset.in, t.dataset.arg, t.type === "checkbox" ? String(t.checked) : t.value);
    if (t.dataset.file && t.files && t.files.length) {
      for (const f of t.files) f.text().then((txt) => ev("file", t.dataset.file, t.dataset.arg || f.name, txt));
      t.value = "";
    }
    if (t.dataset.fileb64 && t.files && t.files[0]) {
      const f = t.files[0];
      f.arrayBuffer().then((ab) => ev("file", t.dataset.fileb64, t.dataset.arg,
        JSON.stringify({ name: f.name, type: f.type || "application/octet-stream", size: f.size, b64: b64(new Uint8Array(ab)) })));
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
      if (t.dataset.in) { t.dataset.sent = t.value; ev("change", t.dataset.in, t.dataset.arg, t.value); }
      ev("enter", t.dataset.enter, t.dataset.arg, t.value);
    } else if (t.dataset.in && t.tagName === "INPUT") {
      e.preventDefault();
      t.blur();
    }
  });
  document.addEventListener("click", (e) => {
    // close the account menu when clicking elsewhere
    const m = document.querySelector(".menu");
    if (m && !e.target.closest(".wallet")) ev("click", "account-menu", "", "");
  }, true);
  // Drop account files / key files anywhere on the page to log in.
  let dragDepth = 0;
  const hasFiles = (e) => e.dataTransfer && [...e.dataTransfer.types].includes("Files");
  window.addEventListener("dragenter", (e) => { if (!hasFiles(e)) return; dragDepth++; document.body.classList.add("dropping"); });
  window.addEventListener("dragleave", () => { if (--dragDepth <= 0) { dragDepth = 0; document.body.classList.remove("dropping"); } });
  window.addEventListener("dragover", (e) => { if (hasFiles(e)) e.preventDefault(); });
  window.addEventListener("drop", (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    dragDepth = 0;
    document.body.classList.remove("dropping");
    if (e.target.closest && e.target.closest("[data-fileb64]")) return;
    for (const f of e.dataTransfer.files) {
      if (f.size > 2 * 1024 * 1024) { ev("file", "drop-too-big", f.name, ""); continue; }
      f.text().then((txt) => ev("file", "drop-file", f.name, txt));
    }
  });
  window.addEventListener("hashchange", () => ev("route", "", "", location.hash));

  // ---- spreadsheet: keys, paste, drag-select
  const keyName = (e) => {
    const k = e.key;
    const ctrl = e.ctrlKey || e.metaKey;
    if (k.length === 1 && !ctrl && !e.altKey) return k;
    return (ctrl ? "Ctrl+" : "") + (e.altKey ? "Alt+" : "") + (e.shiftKey ? "Shift+" : "") + (k.length === 1 ? k.toLowerCase() : k);
  };
  document.addEventListener("keydown", (e) => {
    if (e.isComposing || depth > 0) return;
    const t = e.target.closest && e.target.closest("[data-keys]");
    if (!t) return;
    // fields inside the sheet (e.g. renaming a column) keep their own keys
    if (t !== e.target && /^(INPUT|TEXTAREA|SELECT|BUTTON|A)$/.test(e.target.tagName)) return;
    flush();
    const parts = [t.dataset.keys, t.dataset.arg, keyName(e), e.target.value != null ? e.target.value : ""].map((s) => enc.encode(String(s == null ? "" : s)));
    const ps = parts.map(put);
    scrollSel = true;
    const handled = call(() => w.on_key(ps[0], parts[0].length, ps[1], parts[1].length, ps[2], parts[2].length, ps[3], parts[3].length));
    if (handled) { e.preventDefault(); e.stopPropagation(); } else scrollSel = false;
  }, true);
  document.addEventListener("paste", (e) => {
    const t = e.target.closest && e.target.closest("[data-paste]");
    if (!t || (t !== e.target && /^(INPUT|TEXTAREA)$/.test(e.target.tagName))) return;
    const text = e.clipboardData && e.clipboardData.getData("text/plain");
    if (text == null) return;
    e.preventDefault();
    ev("paste", t.dataset.paste, t.dataset.arg, text);
  });
  let dragging = null, lastArg = null;
  document.addEventListener("mousedown", (e) => {
    const t = e.target.closest && e.target.closest("[data-drag]");
    if (!t || e.button !== 0 || e.target.closest("a,button,input,select,textarea,label")) return;
    e.preventDefault(); // no text selection; the sheet keeps the keyboard
    dragging = t.dataset.drag;
    lastArg = t.dataset.arg;
    scrollSel = true;
    ev("down", t.dataset.drag, t.dataset.arg, e.shiftKey ? "shift" : "");
    const k = root.querySelector('[data-keys="sheet"]');
    if (k && !root.querySelector("[data-editor]")) k.focus({ preventScroll: true });
  });
  document.addEventListener("mouseover", (e) => {
    if (!dragging || !(e.buttons & 1)) { dragging = null; return; }
    const t = e.target.closest && e.target.closest("[data-drag]");
    if (!t || t.dataset.drag !== dragging || t.dataset.arg === lastArg) return;
    lastArg = t.dataset.arg;
    ev("drag", dragging, t.dataset.arg, "");
  });
  document.addEventListener("mouseup", () => { dragging = null; });

  call(() => w.start());
  ev("route", "", "", location.hash);
})().catch((e) => {
  document.getElementById("app").innerHTML =
    '<div class="card bad" style="margin:2rem">IQ Tables failed to start: ' +
    String(e && e.message || e).replace(/[<&]/g, "") + "</div>";
});
