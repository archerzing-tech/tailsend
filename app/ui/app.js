// Tailsend UI — talks to the Rust core through Tauri commands and events.
const T = window.__TAURI__;
const invoke = T.core.invoke;
const listen = T.event.listen;
const { open } = T.dialog || {};

const DATA_CAP = 1024 * 1024 * 1024;
const CHUNK = 1024 * 1024;

const $ = (id) => document.getElementById(id);

let peers = [];
let target = null; // { label, addr } — addr is what we dial (IP when from the list)
let selKey = null; // selected peer identity "ip|name"; names alone can collide
let items = [];
let itemSeq = 0;
let sending = false;
const pendingBatch = new Map(); // batch id -> { item, res, rej }
const knownInbox = new Set();

/* ---------------------------------------------------------------- utils */

const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({
  "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
}[c]));

function fmtSize(n) {
  if (!Number.isFinite(n)) return "";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return (i ? n.toFixed(1) : n) + " " + u[i];
}

const fmtTime = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

function toast(msg, type = "info") {
  if (!msg) return;
  const t = document.createElement("div");
  t.className = "toast " + type;
  t.textContent = msg;
  $("toasts").appendChild(t);
  setTimeout(() => t.remove(), 4500);
}

const OS_ICON = {
  macOS: "", linux: "🐧", windows: "🪟", android: "🤖", iOS: "",
};
const osIcon = (os) => OS_ICON[os] || "🖥";

/* ---------------------------------------------------------------- status */

async function loadStatus() {
  try {
    const s = await invoke("get_status");
    $("ver").textContent = "v" + s.version;
    const r = s.receiver;
    const chip = $("chip-recv");
    if (r.listening) {
      chip.className = "seg ok";
      chip.innerHTML = '<i class="dot"></i>接收中 <b class="mono">' + esc(r.addr) + "</b>";
    } else {
      chip.className = "seg warn";
      chip.innerHTML = '<i class="dot"></i>接收端未运行';
      const b = document.createElement("button");
      b.className = "ghost";
      b.textContent = "重试";
      b.onclick = retryReceiver;
      chip.appendChild(b);
    }
    const dir = $("chip-dir");
    dir.hidden = false;
    dir.title = r.save_dir;
    dir.textContent = "保存到 " + r.save_dir;
    bannerHide();
  } catch (e) {
    toast("状态获取失败：" + e, "err");
  }
}

async function retryReceiver() {
  try {
    await invoke("restart_receiver");
    toast("接收端已启动", "ok");
  } catch (e) {
    toast("接收端仍无法启动：" + e, "err");
  }
  loadStatus();
}

let bannerShownFor = "";
function bannerShow(text) {
  if (bannerShownFor === text) return;
  bannerShownFor = text;
  const b = $("banner");
  b.textContent = text;
  b.hidden = false;
}
function bannerHide() {
  bannerShownFor = "";
  $("banner").hidden = true;
}

/* ---------------------------------------------------------------- peers */

// Track online transitions so a node that just came online can flash its row.
const prevOnline = new Map(); // "ip|name" -> bool
let peersLoading = false;

// Saved targets: mobile has no tailscale CLI, so there this list *is* the
// peer list; on desktop it pins frequently-used nodes so they survive
// discovery hiccups. Each entry is probed live for its online dot.
const SAVED_KEY = "ts.saved.v1";
const savedOnline = new Map(); // addr -> { online, name?, version?, your_ip? }

function loadSaved() {
  try { return JSON.parse(localStorage.getItem(SAVED_KEY)) || []; }
  catch { return []; }
}

function storeSaved(list) {
  try { localStorage.setItem(SAVED_KEY, JSON.stringify(list)); } catch { /* private mode */ }
}

function saveTarget(addr, label) {
  if (!addr) return;
  const list = loadSaved().filter((t) => t.addr !== addr);
  list.unshift({ addr, label: label || addr });
  storeSaved(list.slice(0, 32));
}

function removeTarget(addr) {
  storeSaved(loadSaved().filter((t) => t.addr !== addr));
}

// "1.2.3.4:4782" -> "1.2.3.4" (tailnet addresses are IPv4)
const hostOf = (a) => (/^\d+\.\d+\.\d+\.\d+:\d+$/.test(a) ? a.slice(0, a.indexOf(":")) : a);

async function probeSaved() {
  await Promise.all(loadSaved().map(async (t) => {
    try {
      const r = await invoke("ping_peer", { target: t.addr });
      savedOnline.set(t.addr, { online: true, name: r.name, version: r.version, your_ip: r.your_ip });
      if (r.your_ip) rememberSelfIp(r.your_ip);
      // Adopt the peer's real hostname once we learn it.
      if (r.name && t.label !== r.name) {
        storeSaved(loadSaved().map((x) => (x.addr === t.addr ? { ...x, label: r.name } : x)));
      }
    } catch {
      savedOnline.set(t.addr, { online: false });
    }
  }));
}

let selfIp = "";
function rememberSelfIp(ip) {
  if (!ip) return;
  try { localStorage.setItem("ts.myip", ip); } catch { /* ignore */ }
  if (selfIp === ip) return;
  selfIp = ip;
  const el = $("selfip");
  if (el) {
    el.hidden = false;
    el.innerHTML = "本机 tailnet 地址：<b class=\"mono\">" + esc(ip) + "</b>（对方发送时填这个）";
  }
}

async function loadPeers() {
  if (peersLoading) return;
  peersLoading = true;
  const btn = $("btn-peers-refresh");
  if (btn) btn.disabled = true;
  const el = $("peers");
  try {
    const d = await invoke("list_peers");
    const saved = loadSaved();
    if (d.error) {
      // Saved targets still render below — only nag when there is nothing.
      if (!saved.length) {
        bannerShow(d.error + "。不影响接收；也可以直接输入对方 IP 发送。");
        el.innerHTML = '<div class="empty">暂时无法读取节点列表</div>';
      }
      peers = [];
    } else {
      bannerHide();
      if (d.self && d.self.name) {
        $("chip-self").innerHTML = "本机：<b>" + esc(d.self.name) + "</b>" +
          (d.self.ip ? ' <span class="mono">' + esc(d.self.ip) + "</span>" : "");
        if (d.self.ip) rememberSelfIp(d.self.ip);
      }
      peers = d.peers || [];
    }
    peers.forEach((p) => {
      const k = p.ip + "|" + p.name;
      p.fresh = prevOnline.get(k) === false && p.online;
      prevOnline.set(k, p.online);
    });
    // Merge saved targets, probing the ones discovery can't see (mobile).
    await probeSaved();
    saved.forEach((t) => {
      const host = hostOf(t.addr);
      const known = peers.find((p) => p.ip === host || p.ip === t.addr);
      if (known) { known.saved = true; return; }
      const st = savedOnline.get(t.addr) || {};
      peers.push({
        ip: t.addr,
        name: st.name || t.label || t.addr,
        online: !!st.online,
        os: "",
        fqdn: "",
        saved: true,
      });
    });
    peers.sort((a, b) => (b.online ? 1 : 0) - (a.online ? 1 : 0));
    // Drop a stale selection if its peer vanished from the tailnet.
    if (selKey && !peers.some((p) => p.ip + "|" + p.name === selKey)) {
      selKey = null;
      target = null;
      updateTargetUI();
    }
    renderPeers();
  } catch (e) {
    el.innerHTML = '<div class="empty">读取失败</div>';
    toast("节点读取失败：" + e, "err");
  } finally {
    peersLoading = false;
    if (btn) btn.disabled = false;
  }
}

function renderPeers() {
  const el = $("peers");
  const online = peers.filter((p) => p.online).length;
  $("peers-count").textContent = peers.length ? `（${online}/${peers.length} 在线）` : "";
  if (!peers.length) {
    el.innerHTML = '<div class="empty">tailnet 里没有其他节点</div>';
    return;
  }
  el.innerHTML = peers.map((p, i) => {
    const sel = selKey !== null && p.ip + "|" + p.name === selKey;
    return `
    <div class="peer ${p.online ? "" : "off"} ${sel ? "sel" : ""} ${p.fresh ? "fresh" : ""}" data-i="${i}" title="${esc(p.fqdn || p.name)}${p.online ? " · 在线" : " · 离线"}">
      <i class="led" aria-hidden="true"></i>
      <span class="meta">
        <span class="name">${p.saved ? '<i class="star" title="收藏的目标">★</i>' : ""}${esc(p.name)}${sel ? '<i class="selmark">已选</i>' : ""}</span>
        <span class="sub mono">${esc(p.ip)}${p.fqdn && p.fqdn !== p.name ? " · " + esc(p.fqdn) : ""}</span>
      </span>
      <span class="os">${osIcon(p.os)}</span>
      ${p.saved ? `<button class="fx-del" data-del="${esc(p.ip)}" title="取消收藏">✕</button>` : ""}
    </div>`;
  }).join("");
  el.querySelectorAll(".peer").forEach((n) => {
    n.onclick = () => selectPeer(+n.dataset.i);
  });
  el.querySelectorAll(".fx-del").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      removeTarget(b.dataset.del);
      loadPeers();
    };
  });
}

function selectPeer(i) {
  const p = peers[i];
  if (!p) return;
  selKey = p.ip + "|" + p.name;
  target = { label: p.name, addr: p.ip || p.name };
  $("manual").value = "";
  renderPeers();
  updateTargetUI();
}

function setManualTarget() {
  const v = $("manual").value.trim();
  if (!v) { toast("请输入对方的 IP 或主机名", "err"); return; }
  selKey = null;
  target = { label: v, addr: v };
  saveTarget(v, v);
  renderPeers();
  updateTargetUI();
}

function updateTargetUI() {
  $("target-label").textContent = target ? "→ " + target.label : "未选择目标";
  const st = $("strip-target");
  st.textContent = target ? target.label : "选择目标";
  st.classList.toggle("set", !!target);
  updateSendBtn();
}

function updateSendBtn() {
  $("btn-send").disabled = sending || !target || !items.some((i) => i.status === "ready");
}

/* ---------------------------------------------------------------- queue */

function addItem(entry) {
  if (items.some((i) => i.rel === entry.rel && i.size === entry.size)) return;
  items.push({
    id: ++itemSeq,
    mode: "path",
    path: entry.path,
    rel: entry.rel,
    size: entry.size,
    status: "ready",
    loaded: 0,
  });
  renderQueue();
  updateSendBtn();
}

async function addFromPaths(paths) {
  if (!paths || !paths.length) return;
  try {
    const st = await invoke("stat_paths", { paths });
    st.forEach(addItem);
    if (!st.length) toast("所选位置没有文件", "err");
  } catch (e) {
    toast("读取所选文件失败：" + e, "err");
  }
}

function addFromFile(f) {
  if (f.size > DATA_CAP) {
    toast(`「${f.name}」超过界面发送上限 ${fmtSize(DATA_CAP)}，请在桌面端用「打开文件选择器」`, "err");
    return;
  }
  items.push({
    id: ++itemSeq,
    mode: "data",
    file: f,
    rel: f.webkitRelativePath || f.name,
    size: f.size,
    status: "ready",
    loaded: 0,
  });
  renderQueue();
  updateSendBtn();
}

function removeItem(id) {
  if (sending) return;
  items = items.filter((i) => i.id !== id);
  renderQueue();
  updateSendBtn();
}

function renderQueue() {
  const el = $("queue");
  el.innerHTML = items.map((i) => `
    <li class="qi ${i.status}" data-id="${i.id}">
      <span class="fmeta">
        <span class="frel">${esc(i.rel)}</span>
        <span class="fbar"><i style="width:${i.size ? (100 * i.loaded / i.size).toFixed(1) : 0}%"></i></span>
      </span>
      <span class="fsize">${i.status === "sending" ? fmtSize(i.loaded) + " / " : ""}${fmtSize(i.size)}</span>
      <span class="fstate">${i.status === "done" ? "✓" : i.status === "error" ? "✗" : i.status === "sending" ? "↑" : ""}</span>
      ${i.status !== "sending" ? `<button class="fx" data-x="${i.id}" title="移除">✕</button>` : ""}
    </li>`).join("");
  el.querySelectorAll(".fx").forEach((b) => { b.onclick = () => removeItem(+b.dataset.x); });
}

function updateRow(item) {
  const li = $("queue").querySelector(`[data-id="${item.id}"]`);
  if (!li) return;
  const fill = li.querySelector(".fbar i");
  if (fill) fill.style.width = (item.size ? 100 * item.loaded / item.size : 0).toFixed(1) + "%";
  const size = li.querySelector(".fsize");
  if (size) size.textContent = fmtSize(item.loaded) + " / " + fmtSize(item.size);
}

/* ---------------------------------------------------------------- sending */

function updateOverall(doneBytes, totalBytes, t0) {
  const pct = totalBytes ? Math.min(100, (100 * doneBytes) / totalBytes) : 0;
  $("ofill").style.width = pct.toFixed(1) + "%";
  let text = "";
  if (totalBytes) {
    text = `${fmtSize(doneBytes)} / ${fmtSize(totalBytes)}`;
    const secs = (Date.now() - t0) / 1000;
    if (secs > 0.5) text += ` · ${(doneBytes / 1e6 / secs).toFixed(1)} MB/s`;
  }
  $("otext").textContent = text;
}

function b64(buf) {
  const u = new Uint8Array(buf);
  let s = "";
  const CH = 0x8000;
  for (let i = 0; i < u.length; i += CH) {
    s += String.fromCharCode.apply(null, u.subarray(i, i + CH));
  }
  return btoa(s);
}

async function sendData(item) {
  const { id } = await invoke("send_data_begin", { target: target.addr, name: item.rel, size: item.size });
  pendingBatch.set("d:" + id, { item, res: () => {}, rej: () => {} });
  try {
    for (let off = 0; off < item.size; off += CHUNK) {
      const buf = await item.file.slice(off, off + CHUNK).arrayBuffer();
      await invoke("send_data_chunk", { id, b64: b64(buf) });
      item.loaded = Math.min(off + CHUNK, item.size);
      updateRow(item);
    }
    await invoke("send_data_end", { id });
  } finally {
    pendingBatch.delete("d:" + id);
  }
}

async function send() {
  const ready = items.filter((i) => i.status === "ready");
  if (!target) { toast("先在左侧选择目标节点，或输入对方 IP", "err"); return; }
  if (!ready.length) { toast("没有要发送的文件", "err"); return; }
  sending = true;
  document.body.classList.add("sending");
  updateSendBtn();

  let doneBytes = 0;
  const totalBytes = ready.reduce((s, i) => s + i.size, 0);
  const t0 = Date.now();
  let okc = 0, failc = 0;

  for (const item of ready) {
    item.status = "sending";
    item.loaded = 0;
    renderQueue();
    try {
      if (item.mode === "path") {
        const batch = await invoke("send_files", {
          target: target.addr,
          entries: [{ path: item.path, rel: item.rel, size: item.size }],
        });
        await new Promise((res, rej) => pendingBatch.set("p:" + batch, { item, res, rej }));
      } else {
        await sendData(item);
      }
      item.status = "done";
      item.loaded = item.size;
      okc++;
    } catch (err) {
      item.status = "error";
      item.error = String((err && err.message) || err);
      failc++;
      toast(`「${item.rel}」发送失败：${item.error}`, "err");
    }
    doneBytes += item.size;
    renderQueue();
    updateOverall(doneBytes, totalBytes, t0);
  }

  sending = false;
  document.body.classList.remove("sending");
  updateSendBtn();
  if (okc > 0) saveTarget(target.addr, target.label); // remember where sends succeed
  toast(`发送完成：${okc}/${okc + failc} 个文件`, failc ? "err" : "ok");
  loadInbox();
}

/* ---------------------------------------------------------------- inbox */

async function loadInbox() {
  try {
    const d = await invoke("inbox_list");
    $("inbox-count").textContent = d.total ? `（${d.total}）` : "";
    const el = $("inbox");
    if (!d.files.length) {
      knownInbox.clear();
      el.innerHTML = '<div class="empty">还没有收到文件——在另一台设备上选择本机作为目标发送即可</div>';
      return;
    }
    el.innerHTML = d.files.map((f) => `
      <li class="${knownInbox.has(f.path) ? "" : "new"}">
        <span class="ipath">${esc(f.path)}</span>
        <span class="isize">${fmtSize(f.size)}</span>
        <span class="itime">${fmtTime(f.mtime)}</span>
      </li>`).join("");
    d.files.forEach((f) => knownInbox.add(f.path));
  } catch (_) { /* receiver dir may not exist yet */ }
}

/* ---------------------------------------------------------------- wiring */

function wire() {
  $("btn-peers-refresh").onclick = loadPeers;
  $("btn-manual").onclick = setManualTarget;
  $("manual").addEventListener("keydown", (e) => { if (e.key === "Enter") setManualTarget(); });
  $("btn-send").onclick = send;
  $("btn-open").onclick = async () => {
    try { await invoke("open_inbox"); } catch (e) { toast(String(e), "err"); }
  };
  $("btn-pick").onclick = async () => {
    try {
      const r = await invoke("pick_files");
      if (r && r.length) await addFromPaths(r);
    } catch (e) { if (!/cancel/i.test(String(e))) toast(String(e), "err"); }
  };
  $("btn-pickdir").onclick = async () => {
    try {
      const r = await invoke("pick_folders");
      if (r && r.length) await addFromPaths(r);
    } catch (e) { if (!/cancel/i.test(String(e))) toast(String(e), "err"); }
  };
  $("btn-inputfile").onclick = () => $("file-in").click();
  $("file-in").addEventListener("change", (e) => {
    [...e.target.files].forEach(addFromFile);
    e.target.value = "";
  });
  $("dir-in").addEventListener("change", (e) => {
    [...e.target.files].forEach(addFromFile);
    e.target.value = "";
  });

  // Drag & drop is handled by the Tauri webview (desktop only).
  listen("tauri://drag-enter", () => $("drop").classList.add("over"));
  listen("tauri://drag-leave", () => $("drop").classList.remove("over"));
  listen("tauri://drag-drop", (e) => {
    $("drop").classList.remove("over");
    const paths = e.payload && e.payload.paths;
    if (paths && paths.length) addFromPaths(paths);
  });

  listen("send-progress", (e) => {
    const p = e.payload;
    const pb = pendingBatch.get("p:" + p.batch);
    if (pb) {
      pb.item.loaded = p.loaded;
      updateRow(pb.item);
    }
  });
  listen("send-file", (e) => {
    const p = e.payload;
    const pb = pendingBatch.get("p:" + p.batch);
    if (pb) {
      pendingBatch.delete("p:" + p.batch);
      p.ok ? pb.res() : pb.rej(new Error(p.error || "发送失败"));
    }
  });

  // Restore the last known tailnet address until a fresh probe confirms it.
  try { const ip = localStorage.getItem("ts.myip"); if (ip) rememberSelfIp(ip); } catch { /* ignore */ }

  loadStatus();
  loadPeers();
  loadInbox();
  setInterval(loadStatus, 10000);
  setInterval(loadPeers, 15000);
  setInterval(loadInbox, 3000);
  // Event-driven refresh: the moment the user switches to the app is when
  // fresh peer data matters most — don't make them wait out the interval.
  window.addEventListener("focus", loadPeers);
}

wire();
