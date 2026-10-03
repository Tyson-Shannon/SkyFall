"use strict";

// ---- formatting helpers ---------------------------------------------------
function fmtBytes(n) {
  if (n == null) return "N/A";
  if (n < 1024) return n + " B";
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < units.length - 1);
  return n.toFixed(n >= 100 ? 0 : 1) + " " + units[i];
}
function fmtBps(n) {
  if (n == null) return "N/A";
  if (n < 1000) return n + " B/s";
  if (n < 1e6) return (n / 1e3).toFixed(1) + " KB/s";
  if (n < 1e9) return (n / 1e6).toFixed(1) + " MB/s";
  return (n / 1e9).toFixed(2) + " GB/s";
}
function pct(v) {
  return v == null ? "N/A" : Math.round(v) + "%";
}
function fmtTime(ms) {
  const d = new Date(ms);
  const p = (x) => String(x).padStart(2, "0");
  return p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds());
}
const foot = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};

// ---- canvas line chart ----------------------------------------------------
class Chart {
  constructor(canvas, opts = {}) {
    this.cv = canvas;
    this.ctx = canvas.getContext("2d");
    this.color = opts.color || "#5e17eb";
    this.window = opts.window || 120;
    this.floor = opts.floor != null ? opts.floor : 0; // y-axis lower bound
    this.data = []; // [ [ts, value], ... ]
    this.hover = -1;
    this.singleVal = opts.singleVal != null ? opts.singleVal : false;
    this._bind();
    this._resize();
    if (opts.resObs !== false) {
      this.ro = new ResizeObserver(() => this._resize());
      this.ro.observe(canvas);
    }
  }
  push(ts, v) {
    if (v == null || !isFinite(v)) return;
    this.data.push([ts, v]);
    if (this.data.length > this.window) this.data.shift();
    this.render();
  }
  reset() { this.data = []; this.render(); }
  _bind() {
    this.cv.addEventListener("mousemove", (e) => {
      const r = this.cv.getBoundingClientRect();
      const x = e.clientX - r.left;
      const i = Math.round((x / r.width) * (this.data.length - 1));
      this.hover = isFinite(i) ? i : -1;
      this.render();
    });
    this.cv.addEventListener("mouseleave", () => { this.hover = -1; this.render(); });
  }
  _resize() {
    const dpr = window.devicePixelRatio || 1;
    const r = this.cv.getBoundingClientRect();
    // Remember the CSS size: the context is scaled by dpr, so every coordinate
    // below is in CSS pixels, not backing-store pixels.
    this.cssW = Math.max(1, r.width);
    this.cssH = Math.max(1, r.height);
    this.cv.width = Math.max(1, Math.floor(this.cssW * dpr));
    this.cv.height = Math.max(1, Math.floor(this.cssH * dpr));
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    this.render();
  }
  _xy(i, w, h, min, max) {
    if (this.data.length <= 1) return null;
    const x = (i / (this.data.length - 1)) * w;
    const y = h - ((this.data[i][1] - min) / (max - min)) * (h - 6) - 3;
    return [x, y];
  }
  render() {
    const ctx = this.ctx, cv = this.cv;
    const w = this.cssW || cv.width, h = this.cssH || cv.height;
    ctx.clearRect(0, 0, w, h);
    if (this.data.length < 2) {
      if (this.data.length === 1) {
        ctx.fillStyle = this.color;
        ctx.beginPath();
        ctx.arc(w / 2, h / 2, 2.5, 0, Math.PI * 2);
        ctx.fill();
      }
      return;
    }
    let min = Infinity, max = -Infinity;
    for (const [, v] of this.data) { if (v < min) min = v; if (v > max) max = v; }
    min = Math.min(min, this.floor);
    if (max - min < 1e-6) max = min + 1;
    const pad = (max - min) * 0.08;
    max += pad;

    // grid + baseline
    ctx.strokeStyle = "rgba(139,147,167,0.14)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, h - 0.5);
    ctx.lineTo(w, h - 0.5);
    ctx.stroke();

    // area + line
    const pts = this.data.map((_, i) => this._xy(i, w, h, min, max));
    ctx.beginPath();
    ctx.moveTo(pts[0][0], h);
    for (const p of pts) ctx.lineTo(p[0], p[1]);
    ctx.lineTo(pts[pts.length - 1][0], h);
    ctx.closePath();
    ctx.fillStyle = this.color + "2e";
    ctx.fill();

    ctx.beginPath();
    ctx.moveTo(pts[0][0], pts[0][1]);
    for (const p of pts) ctx.lineTo(p[0], p[1]);
    ctx.strokeStyle = this.color;
    ctx.lineWidth = 1.6;
    ctx.stroke();

    // last value dot
    const lastP = pts[pts.length - 1];
    ctx.beginPath();
    ctx.arc(lastP[0], lastP[1], 2.4, 0, Math.PI * 2);
    ctx.fillStyle = this.color;
    ctx.fill();

    // hover crosshair + value
    if (this.hover >= 0 && this.hover < pts.length) {
      const p = pts[this.hover];
      ctx.setLineDash([3, 3]);
      ctx.beginPath();
      ctx.moveTo(p[0], 0);
      ctx.lineTo(p[0], h);
      ctx.strokeStyle = "rgba(255,255,255,0.35)";
      ctx.stroke();
      ctx.setLineDash([]);
      const [, v] = this.data[this.hover];
      const label = (this.singleVal ? "" : fmtTime(this.data[this.hover][0]) + "  ") +
        (v >= 1000 ? Math.round(v) : +v.toFixed(1));
      ctx.font = "10px " + getComputedStyle(document.body).fontFamily;
      const tw = ctx.measureText(label).width + 10;
      let bx = p[0] + 6, by = 4;
      if (bx + tw > w) bx = p[0] - tw - 6;
      ctx.fillStyle = "rgba(13,17,23,0.92)";
      ctx.strokeStyle = "rgba(255,255,255,0.18)";
      ctx.beginPath();
      ctx.roundRect(bx, by, tw, 16, 4);
      ctx.fill();
      ctx.stroke();
      ctx.fillStyle = "#e6e9f0";
      ctx.fillText(label, bx + 5, by + 11);
    }
  }
}

// ---- state ----------------------------------------------------------------
const state = {
  latest: null,
  sort: { key: "cpu", dir: "desc" },
  cpu: new Chart(foot("cpuChart"), { color: "#5e17eb", window: 180 }),
  ram: new Chart(foot("ramChart"), { color: "#37d3a0", window: 180 }),
  net: new Chart(foot("netChart"), { color: "#22d3ee", window: 180 }),
  gpus: new Map(), // id -> {card, chart, valueEl, footEl, memVal}
};

// ---- GPU cards ------------------------------------------------------------
function gpuCard(g) {
  const card = el("div", "card gpu", null);
  card.innerHTML =
    '<div class="card-head">' +
    '<span class="card-title">GPU</span>' +
    '<span class="card-value" data-v>&ndash;</span></div>' +
    '<canvas class="chart" data-c></canvas>' +
    '<div class="gpu-name" data-m></div>' +
    '<div class="gpu-stats" data-f></div>';
  const chart = new Chart(card.querySelector("[data-c]"), { color: "#ff9f43", window: 180 });
  const entry = {
    card, chart,
    v: card.querySelector("[data-v]"),
    m: card.querySelector("[data-m]"),
    f: card.querySelector("[data-f]"),
    memBar: null,
  };
  document.getElementById("gpu-cards").appendChild(card);
  return entry;
}
function syncGpus(gpus) {
  for (const g of gpus) {
    let e = state.gpus.get(g.id);
    if (!e) { e = gpuCard(g); state.gpus.set(g.id, e); }
    e.m.textContent = g.model;
    e.v.textContent = pct(g.utilization);
    const bits = [];
    if (g.temperature != null) bits.push("temp <b>" + Math.round(g.temperature) + "&deg;C</b>");
    if (g.clock_mhz != null) bits.push("clock <b>" + Math.round(g.clock_mhz) + " MHz</b>");
    if (g.memory_used != null) {
      bits.push("mem <b>" + fmtBytes(g.memory_used) + "</b>" +
        (g.memory_total != null ? " / " + fmtBytes(g.memory_total) : ""));
    }
    e.f.innerHTML = bits.join(" &middot; ") || "&mdash;";
    if (g.utilization != null) e.chart.push(g.ts_ms, g.utilization);
  }
  for (const k of state.gpus.keys()) {
    if (!gpus.some((g) => g.id === k)) {
      state.gpus.get(k).card.remove();
      state.gpus.delete(k);
    }
  }
}

// ---- process table --------------------------------------------------------
function renderProcs() {
  const snap = state.latest;
  const tbody = document.querySelector("#procTable tbody");
  tbody.innerHTML = "";
  if (!snap) return;
  const { key, dir } = state.sort;
  const procs = snap.processes.slice().sort((a, b) => {
    const va = a[key], vb = b[key];
    let cmp = typeof va === "string" ? va.localeCompare(vb) :
      (va == null ? -1 : vb == null ? 1 : va - vb);
    return dir === "desc" ? -cmp : cmp;
  });
  foot("procCount").textContent = snap.processes.length + " tracked";
  for (const p of procs) {
    const tr = el("tr");
    tr.appendChild(el("td", "num", String(p.pid)));
    tr.appendChild(el("td", "name", p.name));
    tr.appendChild(el("td", "num", p.cpu_percent.toFixed(1)));
    tr.appendChild(el("td", "num", fmtBytes(p.ram_kb * 1024)));
    tr.appendChild(el("td", "num", fmtBps(p.disk_read_bps)));
    tr.appendChild(el("td", "num", fmtBps(p.disk_write_bps)));
    tbody.appendChild(tr);
  }
}
function wireSort() {
  document.querySelectorAll("th.sortable").forEach((th) => {
    th.addEventListener("click", () => {
      const key = th.dataset.key;
      if (state.sort.key === key) {
        state.sort.dir = state.sort.dir === "desc" ? "asc" : "desc";
      } else {
        state.sort = { key, dir: "desc" };
      }
      document.querySelectorAll("th.sortable").forEach((t) =>
        t.classList.remove("sort-desc", "sort-asc"));
      th.classList.add("sort-" + state.sort.dir);
      th.innerHTML = th.innerHTML.replace(/<span.*/, "") +
        '<span class="arrow">' + (state.sort.dir === "desc" ? " \u25be" : " \u25b4") + "</span>";
      renderProcs();
    });
  });
  document.querySelectorAll("th.sortable").forEach((th) => {
    if (th.dataset.key === state.sort.key) th.innerHTML =
      th.innerHTML.replace(/<span.*/, "") +
      '<span class="arrow">&#9662;</span>';
  });
}

// ---- snapshot rendering ---------------------------------------------------
const fmtPctShort = (v) => v == null ? "N/A" : (Math.round(v) + "%");

function applySnapshot(snap) {
  state.latest = snap;
  const s = snap.system;
  foot("cpuValue").textContent = fmtPctShort(s.cpu_total);
  foot("cpuFoot").innerHTML =
    "load " + s.load1.toFixed(2) + " / " + s.load5.toFixed(2) + " / " + s.load15.toFixed(2) +
    " &middot; " + s.cpu_count + " cores";
  foot("ramValue").textContent = fmtPctShort(s.ram_percent);
  foot("ramFoot").textContent = fmtBytes(s.ram_used) + " / " + fmtBytes(s.ram_total);
  const net = s.net_rx_bps + s.net_tx_bps;
  foot("netValue").textContent = fmtBps(net);
  foot("netFoot").innerHTML =
    "rx " + fmtBps(s.net_rx_bps) + " &middot; tx " + fmtBps(s.net_tx_bps);

  state.cpu.push(snap.ts_ms, s.cpu_total);
  state.ram.push(snap.ts_ms, s.ram_percent);
  state.net.push(snap.ts_ms, net);
  syncGpus(snap.gpus);

  // disks
  const dl = foot("diskList");
  dl.innerHTML = "";
  for (const d of snap.disks) {
    const row = el("div", "barrow");
    const bar = el("div", "bar d-bar");
    bar.appendChild(el("div"));
    bar.firstChild.style.width = Math.min(100, d.percent) + "%";
    row.appendChild(el("div", "barlabel", null));
    const lb = row.querySelector(".barlabel");
    lb.appendChild(el("span", null, d.mount_point + (d.name ? " (" + d.name + ")" : "")));
    lb.appendChild(el("span", null, pct(d.percent) + " &middot; " + fmtBytes(d.used)));
    row.appendChild(bar);
    dl.appendChild(row);
  }

  // temps
  const tl = foot("tempList");
  tl.innerHTML = "";
  for (const t of snap.temps) {
    if (t.temperature <= 0) continue;
    const cls = t.temperature >= 95 ? "chip hotx" : t.temperature >= 85 ? "chip hot" : "chip";
    tl.appendChild(el("span", cls, null));
    const chip = tl.lastChild;
    chip.append(t.label + " ");
    chip.appendChild(el("b", null, Math.round(t.temperature) + "\u00b0C"));
  }

  renderProcs();
}

// ---- history backfill -----------------------------------------------------
async function backfill() {
  try {
    // Seed from SQLite. Runs once, before the live listener is attached, so the
    // series are replaced rather than appended to and we never plot an instant twice.
    state.cpu.reset();
    state.ram.reset();
    state.net.reset();
    for (const [, e] of state.gpus) e.chart.reset();
    const h = await tauriInvoke("history", { hours: 1 });
    for (const p of h.system) {
      state.cpu.push(p.ts, p.cpu);
      state.ram.push(p.ts, p.ram);
      state.net.push(p.ts, p.rx_bps + p.tx_bps);
    }
    for (const g of h.gpus) {
      let e = state.gpus.get(g.id);
      if (!e) { e = gpuCard({ id: g.id, model: g.model }); state.gpus.set(g.id, e); }
      for (const p of g.points) if (p.util != null) e.chart.push(p.ts, p.util);
      state.gpus.get(g.id).m.textContent = g.model;
      state.gpus.get(g.id).v.textContent = "&ndash;";
    }
  } catch (_) { /* history is best-effort */ }
}

// ---- alert feed -----------------------------------------------------------
// `shown`   ids currently in the DOM, so the feed never re-renders an item you
//           are reading (the old code rebuilt the list and collapsed it).
// `pending` alerts withheld while paused; the collector keeps recording them.
const feed = { shown: new Set(), pending: [], paused: false, timer: null };

function sevClass(sev) {
  return { low: "sev-low", medium: "sev-medium", high: "sev-high", critical: "sev-critical" }[sev] || "sev-medium";
}

function feedItem(a) {
  const item = el("div", "feed-item " + sevClass(a.severity));
  const head = el("div", "feed-head");
  head.appendChild(el("span", "feed-time", fmtTime(a.ts)));
  head.appendChild(el("span", "feed-kind", a.kind));
  if (a.headline) {
    head.appendChild(el("span", "feed-headline", a.headline));
    if (a.confidence != null) {
      head.appendChild(el("span", "feed-conf", (a.confidence * 100).toFixed(0) + "%"));
    }
  } else {
    head.appendChild(el("span", "feed-message", a.message));
  }
  item.appendChild(head);
  if (a.headline) item.appendChild(el("div", "feed-message", a.message));
  const detail = el("div", "feed-detail");
  if (a.root_cause) {
    detail.appendChild(el("p", "feed-cause", "Likely cause: " + a.root_cause));
  }
  if (a.explanation) detail.appendChild(el("p", null, a.explanation));
  if (a.recommended_action) {
    detail.appendChild(el("p", "feed-action", "Do: " + a.recommended_action));
  }
  if (a.dossier) detail.appendChild(el("p", "feed-dossier", a.dossier));
  item.appendChild(detail);
  item.addEventListener("click", () => item.classList.toggle("open"));
  return item;
}

// `fresh` arrives newest-first; walk it backwards and prepend so the newest
// alert ends up at the top without disturbing the items already on screen.
function addFeedItems(fresh) {
  const list = foot("feedList");
  for (const a of fresh.slice().reverse()) {
    list.insertBefore(feedItem(a), list.firstChild);
    feed.shown.add(a.id);
  }
  syncFeedBar();
}

function renderFeed(alerts) {
  const list = foot("feedList");
  list.innerHTML = "";
  feed.shown = new Set();
  addFeedItems(alerts);
}

function syncFeedBar() {
  const btn = foot("feedPause");
  const hint = foot("feedHint");
  const held = feed.pending.length;
  btn.classList.toggle("on", feed.paused);
  btn.setAttribute("aria-pressed", feed.paused ? "true" : "false");
  btn.textContent = feed.paused
    ? (held ? "Resume (" + held + ")" : "Resume")
    : "Pause";
  hint.textContent = feed.paused
    ? (held ? held + " withheld · still recorded" : "paused · still recorded")
    : "new alerts are still recorded";
  const shown = foot("feedList").childElementCount;
  foot("feedCount").textContent = feed.paused && held
    ? shown + " shown · " + held + " new"
    : shown + (shown === 1 ? " alert" : " alerts");
}

function setPaused(paused) {
  feed.paused = paused;
  if (!paused && feed.pending.length > 0) {
    const batch = feed.pending;
    feed.pending = [];
    addFeedItems(batch);
  }
  syncFeedBar();
  tauriInvoke("set_alert_pause", { paused }).catch(() => {});
}

async function pollAlerts() {
  try {
    const alerts = await tauriInvoke("alerts", { limit: 30 });
    const held = new Set(feed.pending.map((a) => a.id));
    const fresh = alerts.filter((a) => !feed.shown.has(a.id) && !held.has(a.id));
    if (foot("feedList").childElementCount === 0) {
      renderFeed(alerts);
      return;
    }
    if (fresh.length === 0) return;
    // While paused, hold them back: they stay on disk and in the count, but the
    // list the user is reading does not move under them.
    if (feed.paused) {
      feed.pending = fresh.concat(feed.pending);
      syncFeedBar();
    } else {
      addFeedItems(fresh);
    }
  } catch (_) { /* best-effort */ }
}

// ---- live stream ------------------------------------------------------------
// The core pushes every sample here; there is no local HTTP server to poll.
const EVENT_SNAPSHOT = "skyfall://snapshot";

async function connect() {
  try {
    // Backfill first, then attach. Seeding while the listener is live raced the
    // two writers and left the CPU/RAM/net series frozen after the first sample.
    await backfill();
    await tauriListen(EVENT_SNAPSHOT, (event) => {
      const snap = event.payload;
      if (snap) applySnapshot(snap);
    });
    foot("statusText").textContent = "live";
    foot("status").querySelector(".dot").className = "dot on";
  } catch (e) {
    foot("statusText").textContent = "disconnected";
    foot("status").querySelector(".dot").className = "dot off";
  }
}

// Tauri 2 splits the global API: commands live under __TAURI__.core, while the
// event namespace is a sibling on __TAURI__ itself. Older builds hung both off
// the root, so accept either shape for each namespace independently.
function tauriRoot() {
  return (window.__TAURI__ && typeof window.__TAURI__ === "object") ? window.__TAURI__ : null;
}

function tauriInvoke(cmd, args) {
  const root = tauriRoot();
  const core = root && (root.core || root);
  if (!core || typeof core.invoke !== "function") {
    return Promise.reject(new Error("Tauri IPC unavailable"));
  }
  return core.invoke(cmd, args);
}

function tauriListen(event, handler) {
  const root = tauriRoot();
  const events = root && (root.event || (root.core && root.core.event));
  if (!events || typeof events.listen !== "function") {
    return Promise.reject(new Error("Tauri event IPC unavailable"));
  }
  return events.listen(event, handler);
}

// ---- settings --------------------------------------------------------------
const settings = {
  backdrop: foot("settingsBackdrop"),
  form: foot("settingsForm"),
  msg: foot("settingsMsg"),
  autostart: foot("autostartWrap"),
  autostartChk: foot("autostartChk"),
  open: false,
  bundle: null,     // last get_config, so put_config can preserve untouched sections
  status: null,     // last ai_status
  pulling: false,
  cancelled: false,
};

// Every control in the modal is addressed by its `name`, never by id: the markup
// has no ids on the inputs, so getElementById would return null and break the form.
function ctl(name) { return foot("settingsForm").elements[name] || null; }
function num(name) { const c = ctl(name); return c ? c.value : ""; }
function chk(name) { const c = ctl(name); return c ? c.checked : false; }
function setVal(name, v) { const c = ctl(name); if (c) c.value = v == null ? "" : String(v); }
function setChk(name, v) { const c = ctl(name); if (c) c.checked = !!v; }

function settingsMsg(text, ok) {
  settings.msg.textContent = text || "";
  settings.msg.className = ok === false ? "modal-msg err" : (ok ? "modal-msg ok" : "modal-msg");
}

function openSettings() {
  settings.backdrop.hidden = false;
  settings.open = true;
  document.body.style.overflow = "hidden";
  settingsMsg("");
  loadSettings()
    .catch((e) => settingsMsg("Failed to load settings: " + e, false));
}
function closeSettings() {
  settings.backdrop.hidden = true;
  settings.open = false;
  document.body.style.overflow = "";
}

async function loadSettings() {
  const b = await tauriInvoke("get_config");
  // Keep the whole bundle: the modal only edits a handful of fields, and saving
  // replaces the config wholesale. Re-sending a partial object would silently
  // reset storage_path/ai and every non-narrator sidecar section.
  settings.bundle = b;
  const c = b.core || {};
  setVal("sampling_interval_ms", c.sampling_interval_ms);
  setVal("idle_interval_ms", c.idle_interval_ms);
  setVal("idle_cpu_threshold", c.idle_cpu_threshold);
  setVal("storage_interval_ms", c.storage_interval_ms);
  setVal("top_processes", c.top_processes);
  setChk("per_core_cpu", c.per_core_cpu);
  setChk("per_process_io", c.per_process_io);
  setChk("echo_stdout", c.echo_stdout);
  setChk("desktop_notifications", c.desktop_notifications);
  const n = (b.sidecar && b.sidecar.narrator) || {};
  const sel = ctl("ai_enabled");
  // The narrator is on unless it was explicitly turned off; `undefined` means
  // the bundle had nothing to say, which is not the same as "off".
  if (sel) sel.value = n.enabled === false ? "false" : "true";
  setVal("ai_base_url", n.base_url);
  setVal("ai_model", n.model);
  setVal("ai_api_key", n.api_key);
  settings.autostart.hidden = false;
  try {
    const s = await tauriInvoke("plugin:autostart|is_enabled");
    settings.autostartChk.checked = !!s;
  } catch (_) { settings.autostartChk.checked = false; }
  refreshModelStatus();
}

function collectBundle() {
  const base = settings.bundle || { core: {}, sidecar: null };
  const bundle = JSON.parse(JSON.stringify(base));
  bundle.core = Object.assign({}, bundle.core, {
    sampling_interval_ms: parseInt(num("sampling_interval_ms"), 10),
    idle_interval_ms: parseInt(num("idle_interval_ms"), 10),
    idle_cpu_threshold: parseFloat(num("idle_cpu_threshold")),
    storage_interval_ms: parseInt(num("storage_interval_ms"), 10),
    top_processes: parseInt(num("top_processes"), 10),
    per_core_cpu: chk("per_core_cpu"),
    per_process_io: chk("per_process_io"),
    echo_stdout: chk("echo_stdout"),
    desktop_notifications: chk("desktop_notifications"),
  });
  const sel = ctl("ai_enabled");
  const sidecar = bundle.sidecar && typeof bundle.sidecar === "object" ? bundle.sidecar : {};
  sidecar.narrator = Object.assign({}, sidecar.narrator, {
    enabled: sel ? sel.value === "true" : true,
    base_url: (ctl("ai_base_url") ? ctl("ai_base_url").value : "").trim(),
    model: (ctl("ai_model") ? ctl("ai_model").value : "").trim(),
    api_key: (ctl("ai_api_key") ? ctl("ai_api_key").value : "").trim() || null,
  });
  bundle.sidecar = sidecar;
  return bundle;
}

/* ---- model install (Ollama) ------------------------------------------- */

function fmtBytes(n) {
  if (!n) return "";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i += 1; }
  return (v < 10 && i > 1 ? v.toFixed(1) : Math.round(v)) + " " + u[i];
}

function modelState(text, cls) {
  const el = foot("aiModelState");
  if (!el) return;
  el.textContent = text;
  el.className = "model-state" + (cls ? " " + cls : "");
}

function setProgress(visible, pct, text) {
  const wrap = foot("aiProgressWrap");
  const fill = foot("aiProgressFill");
  const label = foot("aiProgressText");
  if (!wrap) return;
  wrap.hidden = !visible;
  if (fill) {
    // Null percent = we do not know total yet (manifest/verify phases).
    fill.classList.toggle("indeterminate", pct == null);
    fill.style.width = pct == null ? "" : Math.max(0, Math.min(100, pct)) + "%";
  }
  if (label) label.textContent = text || "";
}

async function refreshModelStatus() {
  if (settings.pulling) return;
  modelState("Checking the AI service\u2026");
  let st;
  try {
    st = await tauriInvoke("ai_status");
  } catch (e) {
    modelState("Could not check the AI service: " + e, "bad");
    return;
  }
  settings.status = st;
  const btn = foot("aiInstallBtn");
  if (!st.reachable) {
    modelState(st.detail || "AI service not reachable.", "bad");
    if (btn) btn.hidden = true;
    return;
  }
  if (!st.is_ollama) {
    modelState(st.detail || "Using a remote AI provider.", "ok");
    if (btn) btn.hidden = true;
    return;
  }
  if (st.model_installed) {
    modelState("Ready \u2014 " + st.model + " is installed.", "ok");
    if (btn) btn.hidden = true;
    return;
  }
  const size = st.size_hint ? " (" + st.size_hint + " download)" : "";
  modelState(st.detail + size, "warn");
  if (btn) { btn.hidden = false; btn.textContent = "Install"; }
}

// Progress arrives as `skyfall://ai-pull` events from the core, one per upstream
// NDJSON line; `ai_pull` itself resolves when the download ends.
const EVENT_PULL = "skyfall://ai-pull";

async function installModel() {
  if (settings.pulling) return;
  const st = settings.status || {};
  const model = (ctl("ai_model") ? ctl("ai_model").value : "").trim() || st.model;
  settings.pulling = true;
  const btn = foot("aiInstallBtn");
  if (btn) btn.hidden = true;
  modelState("Downloading " + model + "\u2026");
  setProgress(true, null, "Starting\u2026");

  let done = 0;
  let total = 0;
  let best = 0;          // high-water mark, see the clamp below
  const seen = new Map();
  let failure = null;
  let unlisten = null;

  const onProgress = (event) => {
    const msg = event.payload;
    if (!msg || failure) return;
    if (msg.error) { failure = msg.error; return; }
    if (msg.status === "cancelled") return;
    // Ollama streams one blob per layer; sum them for a truthful overall bar.
    if (msg.digest && typeof msg.completed === "number") {
      seen.set(msg.digest, { c: msg.completed, t: msg.total || 0 });
      done = 0; total = 0;
      seen.forEach((v) => { done += v.c; total += v.t; });
    }
    if (total > 0) {
      // Ollama announces one blob at a time, so a running total can read 100%
      // the moment the first layer completes and then drop when the next layer
      // appears. Clamp the bar to its high-water mark so it never looks like it
      // is running backwards; the byte counters stay live and honest.
      best = Math.max(best, (done / total) * 100);
      setProgress(true, best, fmtBytes(done) + " / " + fmtBytes(total) +
        "  (" + Math.round(best) + "%)");
    } else {
      setProgress(true, null, msg.status || "Working\u2026");
    }
  };

  try {
    unlisten = await tauriListen(EVENT_PULL, onProgress);
    await tauriInvoke("ai_pull", { model: model });
    if (failure) throw new Error(failure);
    if (settings.cancelled) throw new Error("cancelled");
    setProgress(true, 100, "Installed");
    modelState("Ready \u2014 " + model + " is installed.", "ok");
    await delay(700);
    setProgress(false, 0, "");
    settingsMsg(model + " is ready. Save to apply.", true);
  } catch (e) {
    setProgress(false, 0, "");
    modelState("Install failed: " + e, "bad");
    settingsMsg("Could not install " + model + ": " + e, false);
  } finally {
    if (unlisten) unlisten();
    settings.pulling = false;
    settings.cancelled = false;
    refreshModelStatus();
  }
}

function cancelInstall() {
  // Stop reporting progress and let the core drop the download; the sidecar/LLM
  // path is unaffected, and the next install resumes where this one stopped.
  settings.pulling = false;
  settings.cancelled = true;
  setProgress(false, 0, "");
  modelState("Install stopped. You can resume it from here.", "warn");
  const btn = foot("aiInstallBtn");
  if (btn) btn.hidden = false;
  tauriInvoke("cancel_ai_pull").catch(() => {});
}

function delay(ms) { return new Promise((r) => setTimeout(r, ms)); }

async function saveSettings(e) {
  e.preventDefault();
  settingsMsg("");
  settings.msg.textContent = "Saving\u2026";
  try {
    const bundle = collectBundle();
    const saved = await tauriInvoke("put_config", { bundle: bundle });
    settings.bundle = saved;
    settingsMsg("Saved. Configuration reloaded.", true);
    refreshAiBadge();
    try {
      await tauriInvoke(
        settings.autostartChk.checked ? "plugin:autostart|enable" : "plugin:autostart|disable");
    } catch (_) { /* autostart unavailable */ }
    setTimeout(closeSettings, 450);
  } catch (err) {
    settingsMsg("Save failed: " + err, false);
  }
}

// Highlight whichever preset matches what is in the model box (nothing is
// highlighted for a custom model, which is the honest answer).
function markModelPreset() {
  const modelBox = ctl("ai_model");
  if (!modelBox) return;
  settings.form.querySelectorAll(".chip[data-model]").forEach((chip) => {
    const on = chip.dataset.model === modelBox.value.trim();
    chip.setAttribute("aria-pressed", on ? "true" : "false");
  });
}

function wireSettings() {
  foot("gearBtn").addEventListener("click", openSettings);
  foot("aiInstallBtn").addEventListener("click", installModel);
  foot("aiRefreshBtn").addEventListener("click", refreshModelStatus);
  foot("aiCancelBtn").addEventListener("click", cancelInstall);
  // Editing the model name invalidates the status we are showing.
  const modelBox = ctl("ai_model");
  if (modelBox) {
    modelBox.addEventListener("change", refreshModelStatus);
    modelBox.addEventListener("input", markModelPreset);
    markModelPreset();
    // A preset is just the model field filled in, so it goes through the same
    // save path as typing it - no second source of truth to drift.
    settings.form.querySelectorAll(".chip[data-model]").forEach((chip) => {
      chip.addEventListener("click", () => {
        modelBox.value = chip.dataset.model;
        markModelPreset();
        refreshModelStatus();
      });
    });
  }
  foot("settingsClose").addEventListener("click", closeSettings);
  foot("settingsCancel").addEventListener("click", closeSettings);
  settings.backdrop.addEventListener("mousedown", (e) => {
    if (e.target === settings.backdrop) closeSettings();
  });
  settings.form.addEventListener("submit", saveSettings);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && settings.open) closeSettings();
  });
}

// ---- boot ---------------------------------------------------------------
// ---- AI state badge (header) ------------------------------------------------
const ai = { badgeTimer: null };
// The settings dialog explains AI in detail; the header just answers "is it on
// and will it actually work?" without opening anything.
async function refreshAiBadge() {
  const badge = foot("aiBadge");
  if (!badge) return;
  const text = foot("aiBadgeText");
  const dot = foot("aiBadgeDot");
  const set = (cls, label, title) => {
    badge.className = "ai-badge" + (cls ? " " + cls : "");
    if (text) text.textContent = label;
    badge.title = title || label;
    if (dot) dot.className = "dot " + (cls === "bad" ? "off" : cls === "off" ? "" : "on");
  };

  let enabled = false;
  try {
    const bundle = await tauriInvoke("get_config");
    const narrator = bundle && bundle.sidecar && bundle.sidecar.narrator;
    enabled = !!(narrator && narrator.enabled);
  } catch (_) {
    set("", "AI ?", "Could not read the AI setting.");
    return;
  }
  if (!enabled) {
    set("off", "AI off", "AI explanations are off. Click to turn them on.");
    return;
  }
  let st = null;
  try {
    st = await tauriInvoke("ai_status");
  } catch (_) { /* fall through to unknown */ }
  if (!st || !st.reachable) {
    set("bad", "AI offline", "AI is on but the service did not answer. Click for settings.");
    return;
  }
  if (!st.is_ollama) {
    set("on", "AI on", "AI is on and using a remote provider.");
    return;
  }
  if (st.model_installed) {
    set("on", "AI on", "AI is on and " + st.model + " is ready.");
    return;
  }
  const size = st.size_hint ? " (" + st.size_hint + " download)" : "";
  set("warn", "AI model needed", (st.detail || "Model not installed") + size + ". Click to install.");
}

async function boot() {
  wireSort();
  wireSettings();
  try {
    const h = await tauriInvoke("health");
    if (h.version) foot("version").textContent = "v" + h.version;
  } catch (_) {}
  connect();
  pollAlerts();
  feed.timer = setInterval(pollAlerts, 5000);
  syncFeedBar();
  foot("feedPause").addEventListener("click", () => setPaused(!feed.paused));
  foot("aiBadge").addEventListener("click", openSettings);
  refreshAiBadge();
  // The narrator can be turned on or off from the tray or the config file, so
  // keep the badge honest instead of only refreshing it on boot.
  ai.badgeTimer = setInterval(refreshAiBadge, 15000);
}
boot();