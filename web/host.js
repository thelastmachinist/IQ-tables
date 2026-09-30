// IQ Tables — browser bridge.
// Browsers can only run WebAssembly through JavaScript, so this file gives the
// Rust app the few things it can't do itself: the DOM, fetch, localStorage,
// the clock, secure randomness and files. No app logic and no wallet extension:
// keys are dropped in by the user, held in memory, and all signing happens in Rust.
(async () => {
  "use strict";
  const root = document.getElementById("app");
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  let w = null; // wasm exports
  let staged = null;
  // Files chosen for crowdfunded uploads stay here (they can be gigabytes);
  // the app reads them a piece at a time. Downloads are assembled from
  // pieces the same way.
  const kept = new Map();
  let keptId = 0;
  const blobs = new Map();
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
      tz: () => -new Date().getTimezoneOffset(),
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
      file_read: (id, fid, start, len) => {
        const f = kept.get(fid);
        if (!f) return done(id, false, 0, "that file is no longer open in this tab");
        f.slice(start, start + len).arrayBuffer()
          .then((ab) => done(id, true, 200, new Uint8Array(ab)))
          .catch((e) => done(id, false, 0, errText(e)));
      },
      fetch_bytes: (id, up, ul, start, len) => {
        const init = len > 0 ? { headers: { range: `bytes=${start}-${start + len - 1}` } } : {};
        fetch(str(up, ul), init)
          .then(async (r) => done(id, true, r.status, new Uint8Array(await r.arrayBuffer())))
          .catch((e) => done(id, false, 0, errText(e)));
      },
      blob_part: (bid, p, l) => {
        if (!blobs.has(bid)) blobs.set(bid, []);
        blobs.get(bid).push(new Blob([bytes(p, l)]));
      },
      blob_save: (bid, np, nl, mp, ml) => {
        const a = document.createElement("a");
        a.href = URL.createObjectURL(new Blob(blobs.get(bid) || [], { type: str(mp, ml) }));
        a.download = str(np, nl);
        blobs.delete(bid);
        document.body.appendChild(a);
        a.click();
        setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 60000);
      },
      blob_drop: (bid) => { blobs.delete(bid); },
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
    if (t.dataset.filekeep && t.files && t.files[0]) {
      const f = t.files[0];
      const fid = ++keptId;
      kept.set(fid, f);
      ev("file", t.dataset.filekeep, t.dataset.arg, JSON.stringify({ fid, name: f.name, type: f.type || "application/octet-stream", size: f.size }));
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
    if (e.target.closest && e.target.closest("[data-fileb64],[data-filekeep]")) return;
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
  // where this page is served from (IQ's browser: the repository it was deployed from)
  ev("page", "", "", location.origin + location.pathname);
  ev("route", "", "", location.hash);
})().catch((e) => {
  document.getElementById("app").innerHTML =
    '<div class="card bad" style="margin:2rem">IQ Tables failed to start: ' +
    String(e && e.message || e).replace(/[<&]/g, "") + "</div>";
});
