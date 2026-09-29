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
      if (t.dataset.in) ev("change", t.dataset.in, t.dataset.arg, t.value);
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

  w.start();
  ev("route", "", "", location.hash);
})().catch((e) => {
  document.getElementById("app").innerHTML =
    '<div class="card bad" style="margin:2rem">IQ Tables failed to start: ' +
    String(e && e.message || e).replace(/[<&]/g, "") + "</div>";
});
