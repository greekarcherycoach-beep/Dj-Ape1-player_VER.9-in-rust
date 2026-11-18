/* Dj Ape1 – native bridge (injected before the page scripts).
   Emulates the `window.desktop` API the UI expects. __TOKEN__ is replaced by Rust at start-up. */
(() => {
  if (window.top !== window.self || window.desktop) return;
  const TOKEN = "__TOKEN__";
  const post = (m) => window.ipc.postMessage(JSON.stringify(m));

  /* ---- request/response over wry IPC (window-level operations) ---- */
  let seq = 0;
  const pending = new Map();
  const call = (m, a) =>
    new Promise((res, rej) => {
      const id = ++seq;
      pending.set(id, { res, rej });
      post({ id, m, a: a === undefined ? null : a });
    });

  /* ---- request/response over the local HTTP server (data operations) ---- */
  const rpc = async (m, a) => {
    const r = await fetch(`/rpc/${TOKEN}/${m}`, {
      method: "POST",
      body: JSON.stringify(a === undefined ? null : a),
    });
    if (!r.ok) throw new Error((await r.text()) || "rpc " + r.status);
    const t = await r.text();
    return t ? JSON.parse(t) : null;
  };
  const b64ToU8 = (s) => {
    const b = atob(s), u = new Uint8Array(b.length);
    for (let i = 0; i < b.length; i++) u[i] = b.charCodeAt(i);
    return u;
  };

  /* ---- events pushed from Rust ---- */
  const subs = { fullscreen: [], cmd: [], open: [], flush: [], changed: [] };
  const on = (k) => (cb) => { subs[k].push(cb); };
  let startPaths = null;
  window.__dj = {
    res(id, ok, v) {
      const p = pending.get(id);
      if (!p) return;
      pending.delete(id);
      ok ? p.res(v) : p.rej(new Error(String(v)));
    },
    emit(k, v) {
      if (k === "flush" && !subs.flush.length) { post({ id: 0, m: "flushDone", a: null }); return; }
      for (const f of subs[k] || []) { try { f(v); } catch (e) { console.error(e); } }
    },
    /* files/folders dropped on the window (Rust has the real paths) */
    async drop(paths, x, y) {
      try {
        const ev = { clientX: x, clientY: y, target: document.elementFromPoint(x, y) };
        const toPlayer = typeof isPlayerDrop === "function" ? !!isPlayerDrop(ev) : false;
        const dz = document.getElementById("dz");
        if (dz) dz.classList.add("hide");
        await openPaths(paths, toPlayer);
      } catch (e) { console.error(e); }
    },
  };

  /* ---- <input type=file> → native dialogs (a webview gives no real paths) ---- */
  const origClick = HTMLInputElement.prototype.click;
  HTMLInputElement.prototype.click = function () {
    if (this.type !== "file") return origClick.call(this);
    const inp = this, accept = inp.accept || "";
    (async () => {
      const list = await call("pickInput", {
        dir: inp.hasAttribute("webkitdirectory"),
        multi: !!inp.multiple,
        json: /json/i.test(accept),
      });
      if (!list || !list.length) return;
      const files = [];
      for (const e of list) {
        let f;
        if (/json/i.test(accept)) {
          const b = await (await fetch(`/f/${TOKEN}/${encodeURIComponent(e.path)}`)).blob();
          f = new File([b], e.name);
        } else {
          f = new File([], e.name);
          Object.defineProperty(f, "size", { value: e.size });
        }
        Object.defineProperty(f, "lastModified", { value: e.mtime });
        Object.defineProperty(f, "__path", { value: e.path });
        if (e.rel) Object.defineProperty(f, "webkitRelativePath", { value: e.rel });
        files.push(f);
      }
      Object.defineProperty(inp, "files", { value: files, configurable: true });
      inp.dispatchEvent(new Event("change", { bubbles: true }));
    })().catch((e) => console.error(e));
  };

  window.desktop = {
    native: {
      pickFolder: () => call("pickFolder"),
      pathFor: (f) => (f && f.__path) || "",
      stat: (p) => rpc("stat", p),
      readdir: (p) => rpc("readdir", p),
      readTags: async (p) => {
        const t = await rpc("readTags", p);
        if (t && t.picture && t.picture.b64) {
          t.picture.data = b64ToU8(t.picture.b64);
          delete t.picture.b64;
        }
        return t;
      },
      libInfo: () => rpc("libInfo"),
      libLoad: async () => {
        const r = await fetch(`/lib/${TOKEN}`);
        if (r.status === 204) return null;
        if (!r.ok) throw new Error(await r.text());
        return { json: await r.text(), from: r.headers.get("X-From") || "" };
      },
      libSave: async (json) => {
        const r = await fetch(`/lib/${TOKEN}`, { method: "PUT", body: json });
        if (!r.ok) throw new Error((await r.text()) || "save " + r.status);
      },
      libBackups: () => rpc("libBackups"),
      libBackup: async (name) => {
        const r = await fetch(`/libbk/${TOKEN}/${encodeURIComponent(name)}`);
        if (!r.ok) throw new Error(await r.text());
        return r.text();
      },
      libBackupNow: () => rpc("libBackupNow"),
      openLibDir: () => rpc("openLibDir"),
      showItem: (p) => rpc("showItem", p),
      watch: (paths) => rpc("watch", paths),
      onChanged: on("changed"),
      fileOpen: (dir, name) => rpc("fileOpen", { dir, name }),
      fileWrite: async (id, data, pos) => {
        const q = pos === null || pos === undefined ? "" : `?pos=${pos}`;
        const r = await fetch(`/w/${TOKEN}/${id}${q}`, { method: "PUT", body: data });
        if (!r.ok) throw new Error((await r.text()) || "write " + r.status);
      },
      fileClose: (id) => rpc("fileClose", id),
    },
    prefs: { get: () => rpc("prefsGet"), set: (p) => rpc("prefsSet", p) },
    toggleFullscreen: () => post({ id: 0, m: "toggleFullscreen", a: null }),
    openLog: () => rpc("openLog"),
    getCrash: () => rpc("getCrash"),
    diag: (name, data) => { rpc("diag", { name, data }).catch(() => {}); },
    onFullscreen: on("fullscreen"),
    onCmd: on("cmd"),
    onOpen: on("open"),
    onFlush: on("flush"),
    flushDone: () => post({ id: 0, m: "flushDone", a: null }),
    state: (s) => post({ id: 0, m: "state", a: s }),
    getOpen: async () => { const p = await rpc("getOpen"); return p || []; },
  };
})();
