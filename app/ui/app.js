// Tailsend UI — talks to the Rust core through Tauri commands and events.
// One DOM serves both platforms: desktop (sidebar) and mobile (bottom tabs);
// body[data-platform] switches the skeleton. Controls carry data-action so
// one handler drives every instance.
const T = window.__TAURI__;
const invoke = T.core.invoke;
const listen = T.event.listen;

const DATA_CAP = 1024 * 1024 * 1024;
const CHUNK = 1024 * 1024;

const $ = (id) => document.getElementById(id);
const $$ = (sel) => [...document.querySelectorAll(sel)];
const mount = (name) => $$(`[data-mount="${name}"]`);
const setMount = (name, fn) => mount(name).forEach(fn);

let platform = "desktop";

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

// 设备头像：按 OS 出线稿图标（桌面/手机/服务器），与文件瓦片同一笔画家族
const OS_AVATAR = {
  macOS: '<svg viewBox="0 0 24 24"><rect x="4" y="5" width="16" height="10.5" rx="1.8"/><path d="M2.5 18.5h19"/></svg>',
  windows: '<svg viewBox="0 0 24 24"><rect x="4" y="5" width="16" height="10.5" rx="1.8"/><path d="M2.5 18.5h19"/></svg>',
  linux: '<svg viewBox="0 0 24 24"><rect x="4.5" y="4.5" width="15" height="6.5" rx="1.5"/><rect x="4.5" y="13" width="15" height="6.5" rx="1.5"/><path d="M7.5 7.75h.01M7.5 16.25h.01"/></svg>',
  iOS: '<svg viewBox="0 0 24 24"><rect x="7.5" y="3.5" width="9" height="17" rx="2"/><path d="M11 17.8h2"/></svg>',
  android: '<svg viewBox="0 0 24 24"><rect x="7.5" y="3.5" width="9" height="17" rx="2"/><path d="M11 17.8h2"/></svg>',
};
const osAvatar = (os) => OS_AVATAR[os] ||
  '<svg viewBox="0 0 24 24"><rect x="4.5" y="5" width="15" height="10.5" rx="2"/><path d="M2.5 18.5h19"/></svg>';

/* ------------------------------------------------------------- platform */

async function detectPlatform() {
  try {
    const q = new URLSearchParams(location.search).get("platform");
    const saved = (() => { try { return localStorage.getItem("ts.platform.v1"); } catch { return null; } })();
    if (q || saved) {
      platform = q || saved;
    } else {
      // The embedded node answers only on Android; the desktop stub rejects.
      try { await invoke("node_status"); platform = "mobile"; }
      catch { platform = "desktop"; }
    }
  } catch { platform = "desktop"; }
  document.body.dataset.platform = platform;
  // 移动端没有拖放，选择区文案按平台换说法（DOM 共享，只差这句 copy）
  const mobile = platform === "mobile";
  $$(".drop-title").forEach((el) => { el.textContent = mobile ? "添加要发送的文件" : "拖入文件或文件夹"; });
  $$(".drop-hint").forEach((el) => { el.textContent = mobile ? "支持一次多选；超过 1GB 请用「选择文件」" : "超过 1GB 的文件请用「选择文件」走原生通道"; });
}

/* ---------------------------------------------------------------- status */

let recvSaveDir = "";
let desktopListening = null; // desktop receiver state, from get_status
let recvAddrs = { lan: "" }; // reachable addresses advertised by the receiver

async function loadStatus() {
  try {
    const s = await invoke("get_status");
    $("ver").textContent = "v" + s.version;
    setMount("self-ver", (el) => { el.textContent = "Tailsend v" + s.version; });
    const r = s.receiver;
    recvSaveDir = r.save_dir || "";
    setMount("recv-hint", (el) => {
      el.hidden = !recvSaveDir;
      el.textContent = "文件保存在 " + recvSaveDir;
    });
    desktopListening = !!r.listening;
    recvAddrs.lan = ((r.advertise || []).find((ip) => ip && !ip.startsWith("100."))) || "";
    syncHero();
    bannerHide();
  } catch (e) {
    toast("状态获取失败：" + e, "err");
  }
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

/* ---------------------------------------------------------- embedded node */
// The Android build can run its own tailnet node (no official Tailscale app
// needed). Desktop has no node: node_status errors there, so the platform
// reads as desktop and every node UI stays dark.

let nodeStatus = null;
let nodeRunning = false; // running → sends and peer probes route through Go

const nodeVerbs = {
  off: "未启用",
  starting: "启动中",
  needs_login: "等待登录",
  running: "已连接",
  error: "出错",
};

async function pollNode() {
  let st = null;
  try { st = await invoke("node_status"); } catch { /* desktop: no node */ }
  nodeStatus = st || null;
  nodeRunning = !!st && st.state === "running";
  syncNodeUI();
  // 手机上节点跟着 app 进程走：进程被杀后重启，node 归零，UI 只能干等用户
  // 点「启动」。开机自己 ensure 一次——创建即自启（登录态在 filesDir，是
  // 持久的），LocalSend 式的「打开就能用」。
  if (platform === "mobile" && (!st || st.state === "off") && !autoNodeTried) {
    autoNodeTried = true;
    invoke("node_ensure").then(pollNode).catch(() => {});
  }
  // LocalSend 逻辑：节点在跑，接收就默认是开的，不让人记得去开开关。
  // 用户用开关显式关过（ts.recv.useroff）则尊重，不再自动拉起。
  if (platform === "mobile" && st && st.state === "running" && st.receiver !== "on"
      && !autoRecvTried && localStorage.getItem("ts.recv.useroff") !== "1") {
    autoRecvTried = true;
    invoke("node_start_receiver", {
      token: localStorage.getItem("ts.token.v1") || "",
      confirm: localStorage.getItem("ts.confirm.v1") !== "0",
    }).then(pollNode).catch(() => {});
  }
  if (!st || st.state !== "running") autoRecvTried = false;
  // 手机发送的进度住在 Go 引擎里（Status.sending），事件层看不见——
  // 趁 3s 轮询映射到正在发送的那一行，让发送条真的动起来。
  const s = st && st.sending;
  if (s && sending) {
    const item = items.find((i) => i.status === "sending" && i.rel === s.rel);
    if (item && item.loaded !== s.got) {
      item.loaded = s.got;
      updateRow(item);
    }
  }
  // Go 接收器的收件确认只住在 Kotlin 的 pending 表里，Rust 的
  // incoming-request 事件看不见——没人问就永远不弹窗，发送端 30 秒后超时
  // 拒收。节点在跑时随本次轮询捎带拉一次，转成同样的弹窗。
  if (platform === "mobile" && st && st.state === "running") {
    try {
      const pd = await invoke("node_pending");
      const asks = (pd && pd.asks) || [];
      const seen = new Set();
      for (const a of asks) {
        if (!a || !a.id) continue;
        seen.add(a.id);
        if (goAsks.has(a.id)) continue;
        goAsks.add(a.id);
        const names = Array.isArray(a.names) ? a.names : [];
        showRequest({
          id: a.id,
          sender: a.sender,
          count: a.count,
          total: a.total,
          files: names.map((n) => ({ name: String(n), size: 0 })),
        });
      }
      // 已应答/已超时的 ask 从 Go 侧消失了——清掉我们的路由标记。
      for (const id of goAsks) if (!seen.has(id)) goAsks.delete(id);
    } catch { /* 节点暂未就绪，下一轮再试 */ }
  }
}

function nodeErr(e) { toast(String((e && e.message) || e), "err"); }

async function nodeEnsure() {
  try { await invoke("node_ensure"); } catch (e) { nodeErr(e); }
  pollNode();
}

async function nodeLogin() {
  try { await invoke("node_open_login"); } catch (e) { nodeErr(e); }
}

async function nodeToggleRecv() {
  const st = nodeStatus;
  const recvOn = !!st && st.state === "running" && st.receiver === "on";
  const t = localStorage.getItem("ts.token.v1") || "";
  const confirm = localStorage.getItem("ts.confirm.v1") !== "0";
  try {
    if (recvOn) {
      await invoke("node_stop_receiver");
      localStorage.setItem("ts.recv.useroff", "1"); // keep auto-start off it
    } else {
      await invoke("node_start_receiver", { token: t, confirm });
      localStorage.removeItem("ts.recv.useroff");
    }
    toast(recvOn ? "已关闭 Tailscale 接收（同一 Wi-Fi 的局域网接收不受影响）"
                 : "接收已开启，收件地址 " + ((st && st.ip) || "") + ":4782", "ok");
  } catch (e) { nodeErr(e); }
  pollNode();
}

// 接收页的状态：LocalSend 式的一行安静文字，不占版面、不做动画。
// on=接收中（说清哪条路），warn=有一说一（局域网能收就说能收）。
function syncHero() {
  let mode = "warn";
  let text = "";
  if (platform === "mobile") {
    const st = nodeStatus;
    const state = st ? st.state || "off" : "off";
    // 接收能力 = 两条通道任一活着就算「接收中」：局域网接收器常驻
    // （Rust 侧），tailnet 接收看内置节点。
    const lan = desktopListening === true;
    const tsOn = state === "running" && st.receiver === "on";
    if (lan || tsOn) {
      mode = "on";
      const parts = [];
      if (lan) parts.push("局域网" + (recvAddrs.lan ? " " + recvAddrs.lan + ":4782" : ""));
      if (tsOn) parts.push("Tailscale " + (st.ip || "") + ":4782");
      text = "接收中 · " + parts.join(" · ");
    } else if (state === "starting") { text = "节点启动中…"; }
    else if (state === "needs_login") { text = "同一 Wi-Fi 可收 · 跨网接收需登录（「本机」页）"; }
    else if (state === "error") { text = "节点出错 · 局域网仍可收；跨网在「本机」页重试"; }
    else { text = "同一 Wi-Fi 可收 · 跨网接收需启动节点（「本机」页）"; }
  } else {
    if (desktopListening === null) { text = "检测中…"; }
    else if (desktopListening) { mode = "on"; text = "接收中 · 等待对方发送"; }
    else { text = "接收端未启动 · 会在几十秒内自动重试"; }
  }
  mount("recv-status").forEach((el) => {
    el.classList.toggle("on", mode === "on");
    el.classList.toggle("warn", mode === "warn");
  });
  setMount("recv-meta", (el) => { el.textContent = text; });
}

// One sync paints every node-aware surface: node card, tab badge, the
// send page guard, and the receive master switch.
function syncNodeUI() {
  const st = nodeStatus;
  const state = st ? st.state || "off" : null;
  const isMobile = platform === "mobile";

  setMount("node-card", (el) => { el.hidden = !isMobile || !st; });

  // Tab badge dot: gray off, amber busy/login, green running, red error.
  setMount("badge-node", (el) => {
    el.classList.remove("g", "y", "x", "pulse");
    if (!isMobile || !st) return;
    if (state === "running") el.classList.add("g");
    else if (state === "error") el.classList.add("x");
    else if (state === "needs_login" || state === "starting" || state === "off" && st) el.classList.add("y", "pulse");
  });

  // Send page guard: login/starting/error are preconditions on mobile.
  const guards = mount("send-guard");
  guards.forEach((el) => {
    if (!isMobile || !st || state === "running") { el.hidden = true; el.innerHTML = ""; return; }
    el.hidden = false;
    el.classList.toggle("err", state === "error");
    if (state === "off") {
      el.innerHTML = `<span class="g-copy"><b>先启动内置节点</b>
        <span class="g-sub">启动后这台手机就是 tailnet 节点，不装官方客户端也能收发</span></span>
        <button class="primary" data-guard="ensure">启动节点</button>`;
    } else if (state === "starting") {
      el.innerHTML = `<span class="g-copy"><b>节点启动中…</b>
        <span class="g-sub">一般几秒内完成</span></span>`;
    } else if (state === "needs_login") {
      el.innerHTML = `<span class="g-copy"><b>等待登录认证</b>
        <span class="g-sub">在浏览器完成 Tailscale 授权后这里会自动就绪</span></span>
        <button class="primary" data-guard="login">去登录</button>`;
    } else if (state === "error") {
      el.innerHTML = `<span class="g-copy"><b>节点出错</b>
        <span class="g-sub">${esc(st.error || "未知错误")}</span></span>
        <button class="ghost" data-guard="ensure">重试</button>`;
    }
    el.querySelectorAll("[data-guard]").forEach((b) => {
      b.onclick = b.dataset.guard === "login" ? nodeLogin : nodeEnsure;
    });
  });

  setMount("node-state", (el) => { el.textContent = st ? (nodeVerbs[state] || state) : ""; });

  // Node card body (本机 tab, mobile only).
  mount("node-body").forEach((body) => {
    if (!isMobile || !st) { body.innerHTML = ""; return; }
    if (state === "off") {
      body.innerHTML = `
        <p class="node-hint">登录后这台手机本身就是 tailnet 节点，不装官方 Tailscale 也能收发。协议与桌面端完全一致。</p>
        <div class="node-actions"><button class="primary" data-nact="ensure">启动内置节点</button></div>`;
    } else if (state === "starting") {
      body.innerHTML = `<p class="node-hint">正在启动，一般几秒内完成…</p>`;
    } else if (state === "needs_login") {
      body.innerHTML = `
        <p class="node-hint">在浏览器里完成 Tailscale 授权后，这里会自动变成「已连接」。</p>
        <div class="node-actions"><button class="primary" data-nact="login">打开浏览器登录</button></div>`;
    } else if (state === "running") {
      const recvOn = st.receiver === "on";
      body.innerHTML = `
        <p class="node-meta">本机节点 <b class="mono">${esc(st.ip || "")}</b>${st.tailnet ? " · " + esc(st.tailnet) : ""}</p>
        <div class="node-actions">
          <button class="${recvOn ? "ghost" : "primary"}" data-nact="recv">${recvOn ? "关闭 Tailscale 接收" : "开启 Tailscale 接收"}</button>
        </div>
        <p class="node-hint">这个开关只管跨网（Tailscale）接收；同一 Wi-Fi 的局域网接收始终可用。收到的文件都在「接收」页。</p>`;
    } else if (state === "error") {
      body.innerHTML = `
        <p class="node-hint err">${esc(st.error || "节点出错")}</p>
        <div class="node-actions"><button class="ghost" data-nact="ensure">重试</button></div>`;
    }
    body.querySelectorAll("[data-nact]").forEach((b) => {
      const a = b.dataset.nact;
      b.onclick = a === "ensure" ? nodeEnsure : a === "login" ? nodeLogin : nodeToggleRecv;
    });
  });

  syncHero();
}

// Peer probe: through the embedded node when it is up, otherwise the system
// route. Both answer { name, version } about the receiver at addr.
async function pingTarget(addr) {
  if (!nodeRunning) return invoke("ping_peer", { target: addr });
  const r = await invoke("node_ping_target", { target: addr });
  if (r && r.ok) return { online: true, name: r.name, version: r.version, your_ip: r.your_ip || "" };
  throw new Error((r && r.error) || "unreachable");
}

/* ---------------------------------------------------------------- peers */

// Track online transitions so a node that just came online can flash its card.
const prevOnline = new Map(); // "ip|name" -> bool
let peersLoading = false;
let peerFilter = "";

// Saved targets: mobile has no tailscale CLI, so there this list *is* the
// peer list; on desktop it pins frequently-used nodes so they survive
// discovery hiccups. Each entry is probed live for its status dot.
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

const peerKey = (p) => p.ip + "|" + p.name; // names alone can collide

// True when a Tailsend receiver answers at addr (short, token-authed ping).
async function probeAddr(addr) {
  try {
    const r = await pingTarget(addr);
    if (r.your_ip) rememberSelfIp(r.your_ip);
    return true;
  } catch {
    return false;
  }
}

// Sort rank: sendable first, then "node up but receiver down", then offline.
const rank = (p) => (p.recv === true ? 0 : p.online ? 1 : 2);

async function probeSaved() {
  await Promise.all(loadSaved().map(async (t) => {
    try {
      const r = await pingTarget(t.addr);
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
let selfName = "";
function rememberSelfIp(ip) {
  if (!ip) return;
  try { localStorage.setItem("ts.myip", ip); } catch { /* ignore */ }
  if (selfIp === ip) return;
  selfIp = ip;
  paintSelfIp();
}

function paintSelfIp() {
  setMount("selfip", (el) => {
    el.hidden = !selfIp;
    el.innerHTML = "本机 tailnet 地址：<b class=\"mono\">" + esc(selfIp) + "</b>（对方发送时填这个）";
  });
}

// ---------------------------------------------------------------- discovery
// Mobile has no `tailscale status`. Receivers that do have the CLI answer
// /api/peers with their view of the tailnet — so asking any reachable peer
// (typically an always-on NAS) enumerates the whole tailnet.

const DISC_KEY = "ts.disc.v1";

function loadDisc() {
  try { return JSON.parse(localStorage.getItem(DISC_KEY)) || []; }
  catch { return []; }
}

function storeDisc(list) {
  try { localStorage.setItem(DISC_KEY, JSON.stringify(list.slice(0, 64))); }
  catch { /* private mode */ }
}

async function discoverViaPeers() {
  // Only nodes whose receiver just answered can tell us anything.
  const sources = [...savedOnline.entries()].filter(([, st]) => st.online).map(([addr]) => addr);
  if (!sources.length) return;
  try {
    const d = await invoke("discover_peers", { targets: sources });
    if (Array.isArray(d.peers) && d.peers.length) storeDisc(d.peers);
  } catch { /* older receivers everywhere — favorites still work */ }
}

async function loadPeers() {
  if (peersLoading) return;
  peersLoading = true;
  try {
    // LAN direct channel: sweep the local subnets in parallel with tailnet
    // discovery (~1s) so the same Wi-Fi works with Tailscale down.
    const lanP = invoke("lan_scan").then((v) => { lanPeers = v || []; }).catch(() => {});
    // 内置节点在跑：tsnet 自己就握着整个 tailnet 的设备表，不依赖 CLI
    // （手机上 CLI 永远不存在）。失败或未跑再走 list_peers/discover 老路。
    let d = null;
    if (nodeRunning) {
      try {
        const np = await invoke("node_peers");
        if (np && Array.isArray(np.peers)) d = { error: null, self: np.self || null, peers: np.peers };
      } catch { /* fall back below */ }
    }
    if (!d) d = await invoke("list_peers");
    const saved = loadSaved();
    if (d.error) {
      // 手机上 list_peers 永远失败（没有 CLI）——这不是故障，只是路线没选对；
      // 设备表的来源是内置节点。提示要引导启动/登录，而不是甩一句吓人的报错。
      if (platform === "mobile") {
        bannerHide();
        const hint = !nodeRunning
          ? "内置节点还没就绪——在「本机」页启动并登录后，设备会出现在这里"
          : "设备列表还没就绪，几秒后自动重试";
        mount("peers").forEach((n) => { n.innerHTML = `<div class="empty">${hint}</div>`; });
        peers = [];
      } else {
        // 修复引导必须能活过下面的 renderPeers（LAN 合并会重绘列表）——记住
        // 状态，renderPeers 在本该出现死胡同空文案的地方画修复面板。
        peersFixState = d.state;
        if (saved.length) {
          peers = [];
        } else {
          bannerShow(d.error + "。不影响接收；也可以直接输入对方 IP 发送。");
          mount("peers").forEach((n) => { n.innerHTML = peersFixHtml(d.state); });
          peers = [];
        }
        // 自愈：服务没跑就自动拉起（macOS 静默、Linux 走 polkit、Windows 弹
        // 一次 UAC）。两次判定——刚唤醒时 status 慢不该弹 UAC，连续两轮都
        // daemon_down 才是真没在跑。
        if (platform === "desktop" && d.state === "daemon_down") {
          if (daemonDownStruck && !autoSvcTried) {
            autoSvcTried = true;
            invoke("tailscale_start_service")
              .then(() => { toast("正在拉起 Tailscale 服务…", "ok"); boostPeers(); })
              .catch(() => {});
          } else {
            daemonDownStruck = true;
          }
        } else if (d.state !== "daemon_down") {
          daemonDownStruck = false;
        }
      }
    } else {
      bannerHide();
      peersFixState = null; // discovery is healthy again — the panel is moot
      if (d.self && d.self.name) {
        if (d.self.ip) rememberSelfIp(d.self.ip);
        if (selfName !== d.self.name) {
          selfName = d.self.name;
          setMount("self-name", (el) => { el.textContent = selfName; });
        }
      }
      // Belt and braces: some tailscale builds list self inside Peer too —
      // the source filters it, but stale answers shouldn't resurrect it.
      peers = (d.peers || []).filter((p) => !selfIp || p.ip !== selfIp);
    }
    peers.forEach((p) => { p.recv = undefined; }); // receiver probe pending
    // Merge saved targets, probing the ones discovery can't see (mobile).
    await probeSaved();
    if (d.error) await discoverViaPeers(); // no local CLI → ask reachable peers
    saved.forEach((t) => {
      const host = hostOf(t.addr);
      const known = peers.find((p) => p.ip === host || p.ip === t.addr);
      const st = savedOnline.get(t.addr);
      if (known) {
        known.saved = true;
        if (st) { known.recv = st.online; known.online = st.online; }
        return;
      }
      const st2 = st || {};
      peers.push({
        ip: t.addr,
        name: st2.name || t.label || t.addr,
        online: !!st2.online,
        recv: !!st2.online,
        os: "",
        fqdn: "",
        saved: true,
      });
    });
    // Nodes learned from peers' views (mobile discovery): merged after saved
    // so a favorite always wins on addr collision; dedup by IP.
    loadDisc().forEach((n) => {
      if (!n.ip) return;
      const known = peers.find((p) => p.ip === n.ip || hostOf(p.ip) === n.ip);
      if (known) return;
      peers.push({
        ip: n.ip,
        name: n.name || n.ip,
        online: !!n.online,
        recv: undefined,
        os: n.os || "",
        fqdn: "",
        saved: false,
        disc: true,
      });
    });
    // 局域网直连：把同网扫描到的设备并进列表（Tailscale 不在也有东西可发）。
    // 同一台设备两条路都通时会显示两行（100.x + 局域网 IP），LAN 行带徽章。
    await lanP;
    mergeLanPeers();
    peers.sort((a, b) => (b.online ? 1 : 0) - (a.online ? 1 : 0));
    renderPeers(); // immediate paint: discovery status + saved probes

    // Green means "Tailsend receiver is actually up" — tailscale's online
    // flag alone can't tell us that, so ask the peers it thinks are online.
    // A node that's up without Tailsend running shows amber, not green.
    await Promise.all(
      peers
        .filter((p) => p.online && p.recv === undefined)
        .map(async (p) => { p.recv = await probeAddr(p.ip); })
    );
    peers.forEach((p) => {
      const k = p.ip + "|" + p.name;
      const up = p.recv === true;
      p.fresh = prevOnline.get(k) === false && up;
      prevOnline.set(k, up);
    });
    peers.sort((a, b) => rank(a) - rank(b));
    // Drop selections whose peer vanished from the tailnet (manual ones stay).
    if (targets.length) {
      const alive = targets.filter(
        (t) => t.key.startsWith("m|") || peers.some((p) => peerKey(p) === t.key)
      );
      if (alive.length !== targets.length) {
        targets = alive;
        updateTargetUI();
      }
    }
    renderPeers();
  } catch (e) {
    mount("peers").forEach((n) => { n.innerHTML = '<div class="empty">读取失败</div>'; });
    toast("设备读取失败：" + e, "err");
  } finally {
    peersLoading = false;
  }
}

/* ------------------------------------------------- Tailscale fix guidance */
// Desktop discovery fails for exactly three reasons; each gets one button
// that actually moves the user forward (不让用户猜，能想到的都给到).
const peersFix = {
  cli_missing: { btn: "安装 Tailscale", act: "download",
    tip: "没有找到 Tailscale——装好并登录后，设备会自动出现在这里" },
  daemon_down: { btn: "启动 Tailscale", act: "start",
    tip: "Tailscale 服务没在运行——点按钮拉起它（Windows 会弹一次管理员授权）" },
  logged_out: { btn: "连接 Tailscale", act: "login",
    tip: "Tailscale 已装好但还没连接——点按钮在浏览器完成登录" },
};
function peersFixHtml(state) {
  const f = peersFix[state];
  if (!f) return '<div class="empty">暂时无法读取设备列表</div>';
  return `<div class="empty">${f.tip}<br><button class="fix-btn" data-peers-fix="${f.act}">${f.btn}</button></div>`;
}
let peersBoost = null; // fix action fired → poll fast so the list snaps in
function mergeLanPeers() {
  for (const l of lanPeers) {
    if (!l.ip) continue;
    const hit = peers.find((p) => p.ip === l.ip);
    if (hit) { hit.lan = true; continue; }
    peers.push({
      ip: l.ip,
      name: l.name || l.ip,
      online: true, // replied to the probe — it's up on this network
      recv: undefined,
      os: "",
      fqdn: "",
      saved: false,
      lan: true,
    });
  }
}
function boostPeers() {
  if (peersBoost) return;
  const until = Date.now() + 120000;
  peersBoost = setInterval(() => {
    if (Date.now() > until) { clearInterval(peersBoost); peersBoost = null; return; }
    loadPeers();
  }, 4000);
}
async function peersFixAction(act) {
  try {
    if (act === "download") {
      await invoke("open_url", { url: "https://tailscale.com/download" });
    } else if (act === "start") {
      await invoke("tailscale_start_service");
      toast("已发起启动——授权通过后设备会自动出现", "ok");
    } else if (act === "login") {
      const url = await invoke("tailscale_login");
      if (!url) toast("Tailscale 已在连接状态，几秒后设备自动出现", "ok");
      else toast("登录页已在浏览器打开，完成后这里自动刷新", "ok");
    }
  } catch (e) {
    toast(String(e), "err");
  }
  boostPeers();
}

function renderPeers() {
  const ready = peers.filter((p) => p.recv === true || (p.recv === undefined && p.online)).length;
  setMount("peers-count", (el) => { el.textContent = peers.length ? `· ${ready}/${peers.length} 可发送` : ""; });
  const f = peerFilter.toLowerCase();
  const visible = f
    ? peers.filter((p) => (p.name || "").toLowerCase().includes(f) || (p.ip || "").toLowerCase().includes(f))
    : peers;
  if (!visible.length) {
    // 发现失败且没有任何设备可显示时，不能死胡同式地丢一句空文案——修复
    // 面板（带按钮）才是出路。
    const html = !peers.length && peersFixState
      ? peersFixHtml(peersFixState)
      : `<div class="empty">${peers.length ? "没有匹配的设备" : "tailnet 里还没有其他设备"}</div>`;
    mount("peers").forEach((el) => { el.innerHTML = html; });
    return;
  }
  // 状态语义：绿点 = 对方 Tailsend 真能收（可发送）；琥珀 = 节点在线但对方
  // Tailsend 没开；灰 = 离线。徽章只说结论——探测中的暂态不给字，免噪音。
  const sClass = (p) =>
    p.recv === true ? "s-ok" : p.recv === false && p.online ? "s-idle"
    : p.online ? "s-on" : "s-probe";
  const stat = (p) =>
    p.recv === true ? '<i class="stat b-ok">可发送</i>'
    : p.recv === false && p.online ? '<i class="stat b-idle">未运行</i>'
    : p.lan ? '<i class="stat b-lan">局域网</i>'
    : !p.online ? '<i class="stat b-dim">离线</i>'
    : "";
  // 本机固定在列表最上面：标出、不可选。用户明确定调过——保留可见性，
  // 点击解释原因，避免"我的手机怎么不在列表里"的困惑。
  const selfRow = selfIp
    ? `<div class="dev self" data-self="1" title="这台设备就是本机，不能发给自己">
        <span class="ava me" aria-hidden="true"><svg viewBox="0 0 1024 1024"><use href="#bird"/></svg><i class="dot"></i></span>
        <span class="dmeta">
          <span class="dname"><span class="nm">${esc(selfName || "本机")}</span></span>
          <span class="dsub"><span class="ip mono">${esc(selfIp)}</span><i class="stat b-dim">本机</i></span>
        </span>
      </div>`
    : "";
  if (!visible.length) {
    const msg = peers.length ? "没有匹配的设备" : "tailnet 里还没有其他设备";
    mount("peers").forEach((el) => {
      el.innerHTML = selfRow + `<div class="empty">${msg}</div>`;
    });
    return;
  }
  const html = visible.map((p) => {
    const i = peers.indexOf(p);
    const sel = targets.some((t) => t.key === peerKey(p));
    return `
    <div class="dev ${p.recv === true || (p.online && p.recv !== false) ? "" : "off"} ${sel ? "sel" : ""} ${p.fresh ? "fresh" : ""}"
         data-i="${i}" role="button" tabindex="0" aria-pressed="${sel}"
         title="${esc(p.fqdn || p.name)} · ${esc(p.ip)}">
      <span class="ava ${sClass(p)}" aria-hidden="true">${osAvatar(p.os)}<i class="dot"></i></span>
      <span class="dmeta">
        <span class="dname">${p.saved ? '<i class="star" title="常用设备">★</i>' : ""}<span class="nm">${esc(p.name)}</span></span>
        <span class="dsub"><span class="ip mono">${esc(p.ip)}</span>${p.lan ? '<i class="stat b-lan">局域网</i>' : ""}${stat(p)}</span>
      </span>
      ${p.saved ? `<button class="fx-del" data-del="${esc(p.ip)}" title="移除常用">✕</button>` : ""}
      <i class="ck" aria-hidden="true">✓</i>
    </div>`;
  }).join("");
  mount("peers").forEach((el) => { el.innerHTML = selfRow + html; });
}

function choosePeer(i) {
  const p = peers[i];
  if (!p) return;
  // 离线设备点了也发不过去，直接拦在勾选这一步。
  // 未就绪的设备点了也发不过去：离线（灰）与「未运行」（对方 Tailsend 没开，
  // 琥珀）都拦在勾选这一步；「在线探测中」是暂态，放行让探测收敛。
  if (!p.online || p.recv === false) {
    toast(`${p.name || p.ip} ${p.online ? "的 Tailsend 未运行" : "离线"}，无法选择`, "err");
    return;
  }
  const key = peerKey(p);
  const addr = p.ip || p.name;
  const hit = targets.find((t) => t.key === key);
  // 勾选语义：点击在目标集合里加入/移出（同地址只保留一条，避免重复投递）
  targets = hit ? targets.filter((t) => t !== hit)
    : [...targets.filter((t) => t.addr !== addr), { key, label: p.name, addr }];
  $$(".manual-in").forEach((n) => { n.value = ""; });
  renderPeers();
  updateTargetUI();
}

function setManualTarget(input) {
  const v = (input && input.value || "").trim();
  if (!v) { toast("请输入对方的 IP 或主机名", "err"); return; }
  const hit = targets.find((t) => t.addr === v);
  targets = hit ? targets.filter((t) => t !== hit)
    : [...targets.filter((t) => t.addr !== v), { key: "m|" + v, label: v, addr: v }];
  if (!hit) saveTarget(v, v); // 只在真正加入时收藏；取消选择不把收藏复活
  $$(".manual-in").forEach((n) => { n.value = ""; });
  renderPeers();
  updateTargetUI();
}

async function saveReceiverToken(value) {
  const v = (value || "").trim();
  try {
    await invoke("set_receiver_token", { token: v });
    try { localStorage.setItem("ts.token.v1", v); } catch { /* ignore */ }
    setMount("token-row", (el) => { el.hidden = true; });
    bindAll("token", (b) => b.setAttribute("aria-expanded", "false"));
    toast(v ? "接收令牌已启用，接收端已重启" : "接收令牌已清除，接收端已重启", "ok");
    loadStatus();
  } catch (e) {
    toast("令牌保存失败：" + e, "err");
  }
}

function updateTargetUI() {
  const n = targets.length;
  const f = items.filter((i) => i.status === "ready").length;
  setMount("sendline", (el) => {
    el.textContent =
      !n && !f ? "选择文件，再点下面的设备"
      : !n ? `已选 ${f} 个文件 · 点下面的设备选发给谁`
      : !f ? `将发给 ${n === 1 ? targets[0].label : n + " 台设备"} · 先添加文件`
      : `发送 ${f} 个文件给 ${n === 1 ? targets[0].label : n + " 台设备"}`;
  });
  updateSendBtn();
}

function updateSendBtn() {
  const dis = sending || !targets.length || !items.some((i) => i.status === "ready");
  $$('[data-action="send"]').forEach((b) => { b.disabled = dis; });
}

/* ---------------------------------------------------------------- tabs */

function setTab(tab) {
  document.body.dataset.tab = tab;
  $$('[data-action="tab"]').forEach((b) => {
    b.setAttribute("aria-pressed", String(b.dataset.tab === tab));
  });
}

/* ---------------------------------------------------------------- queue */

// 系统文件图标：桌面端经 file_icons 命令取 Finder 同款 PNG（按扩展名缓存并
// 持久化，同一类型只取一次）；取不到（移动端/旧后端）退回 FILE_ICONS 线稿。
// 缓存存完整 data URL（Rust 只回裸 base64，<img src> 缺前缀会渲染成破图「?」）。
// v3：v2 存过没加前缀的裸 base64，换键作废。
const ICON_KEY = "ts.icons.v3";
const iconCache = (() => {
  try { return JSON.parse(localStorage.getItem(ICON_KEY)) || {}; }
  catch { return {}; }
})();
let iconsUnavailable = false;
const extOf = (name) => { const m = /\.([a-z0-9]+)$/i.exec(name || ""); return m ? m[1].toLowerCase() : ""; };

async function fetchIcons(entries) {
  if (iconsUnavailable) return;
  const reps = new Map(); // ext -> 首个真实路径（iconForFile 按路径取图）
  for (const e of entries) {
    const k = extOf(e.rel) || "__none__";
    if (k in iconCache || reps.has(k)) continue;
    reps.set(k, e.path);
  }
  if (!reps.size) return;
  const keys = [...reps.keys()];
  const paths = keys.map((k) => reps.get(k));
  try {
    const res = await invoke("file_icons", { paths });
    paths.forEach((p, idx) => { if (res && res[idx]) iconCache[keys[idx]] = "data:image/png;base64," + res[idx]; });
    try {
      const all = Object.keys(iconCache);
      if (all.length > 48) all.slice(0, all.length - 48).forEach((k) => delete iconCache[k]);
      localStorage.setItem(ICON_KEY, JSON.stringify(iconCache));
    } catch { /* 私密模式存不了，内存缓存仍然有效 */ }
    renderQueue();
  } catch { iconsUnavailable = true; }
}

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
  updateTargetUI();
  updateSendBtn();
}

async function addFromPaths(paths) {
  if (!paths || !paths.length) return;
  try {
    const st = await invoke("stat_paths", { paths });
    st.forEach(addItem);
    if (!st.length) toast("所选位置没有文件", "err");
    else fetchIcons(st);
  } catch (e) {
    toast("读取所选文件失败：" + e, "err");
  }
}

function addFromFile(f) {
  if (f.size > DATA_CAP) {
    toast(`「${f.name}」超过界面发送上限 ${fmtSize(DATA_CAP)}，请用「选择文件」走原生通道`, "err");
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
  updateTargetUI();
  updateSendBtn();
}

function removeItem(id) {
  if (sending) return;
  items = items.filter((i) => i.id !== id);
  renderQueue();
  updateTargetUI();
  updateSendBtn();
}

function renderQueue() {
  const total = items.reduce((s, i) => s + i.size, 0);
  setMount("queue-count", (el) => {
    el.textContent = items.length ? `· ${items.length} 个 · ${fmtSize(total)}` : "";
  });
  const html = items.map((i) => {
    const k = fileKind(i.rel);
    const src = i.mode === "path" ? iconCache[extOf(i.rel) || "__none__"] : null;
    return `
    <li class="qi ${i.status}" data-id="${i.id}" title="${esc(i.rel)}">
      <span class="itic k-${k}${src ? " sys" : ""}" aria-hidden="true">${src ? `<img src="${src}" alt="">` : FILE_ICONS[k]}</span>
      <span class="fmeta">
        <span class="frel">${esc(i.rel)}</span>
        <span class="fbar"><i style="width:${i.size ? (100 * i.loaded / i.size).toFixed(1) : 0}%"></i></span>
      </span>
      <span class="fsize">${i.status === "sending" ? fmtSize(i.loaded) + " / " : ""}${fmtSize(i.size)}</span>
      <span class="fstate">${i.status === "done" ? "✓" : i.status === "error" ? "✗" : ""}</span>
      ${!sending && i.status === "error" ? `<button class="fx retry" data-retry="${i.id}" title="重新发送">↻</button>` : ""}
      ${!sending && i.status !== "sending" ? `<button class="fx" data-x="${i.id}" title="移除">✕</button>` : ""}
    </li>`;
  }).join("");
  mount("queue").forEach((el) => {
    el.innerHTML = html;
    /* 空队列整卡隐藏。不用 :has(.queue:empty)——WebKit 在内容从空变
       非空时不重求 :has，卡片会一直 display:none，切页签才“出现”。 */
    const panel = el.closest(".queue-panel");
    if (panel) panel.hidden = items.length === 0;
  });
}

function updateRow(item) {
  mount("queue").forEach((el) => {
    const li = el.querySelector(`[data-id="${item.id}"]`);
    if (!li) return;
    const fill = li.querySelector(".fbar i");
    if (fill) fill.style.width = (item.size ? 100 * item.loaded / item.size : 0).toFixed(1) + "%";
    const size = li.querySelector(".fsize");
    if (size) size.textContent = fmtSize(item.loaded) + " / " + fmtSize(item.size);
  });
}

/* ------------------------------------------------------- incoming requests */

let reqQueue = [];
let reqCur = null; // { id }
let reqTimer = null;
let autoRecvTried = false; // one auto-start attempt per node-running stretch
let autoNodeTried = false; // one node-ensure attempt per app launch (mobile)
let peersFixState = null; // discovery failure state for the fix panel (desktop)
let daemonDownStruck = false; // daemon_down seen on the previous poll
let autoSvcTried = false; // one auto service-start per app launch (desktop)
// 内置节点（Go 接收器）的收件确认 id 集合：这些请求经 Kotlin pending 中转，
// Rust 的 request_respond 够不着它们，应答时要走 node_respond 分开路由。
const goAsks = new Set();

function showRequest(p) {
  reqQueue.push(p);
  if (!reqCur) nextRequest();
}

function nextRequest() {
  const p = reqQueue.shift();
  if (!p) { reqCur = null; $("req-backdrop").hidden = true; return; }
  reqCur = { id: p.id };
  $("req-letter").textContent = (p.sender || "?").trim().charAt(0).toUpperCase() || "?";
  $("req-title").textContent = `${p.sender || "未知设备"} 想发给你 ${p.count} 个文件`;
  $("req-sub").innerHTML = `约 ${fmtSize(p.total)} · <b id="req-timer">30</b> 秒内不确认将自动取消`;
  const list = $("req-files");
  const shown = (p.files || []).slice(0, 8);
  list.innerHTML = shown.map((f) => {
    const k = fileKind(f.name);
    return `<li><span class="itic k-${k}" aria-hidden="true">${FILE_ICONS[k]}</span><span class="rfn">${esc(f.name)}</span><span class="rfs">${f.size ? fmtSize(f.size) : ""}</span></li>`;
  }).join("") + (p.count > shown.length ? `<li class="rfmore">… 等共 ${p.count} 个文件</li>` : "");
  const ring = $("req-ring");
  ring.classList.remove("low");
  const sub = $("req-sub");
  sub.classList.remove("low");
  setReqRing(30);
  $("req-backdrop").hidden = false;
  let left = 30;
  clearInterval(reqTimer);
  reqTimer = setInterval(() => {
    left -= 1;
    const t = $("req-timer");
    if (t) t.textContent = Math.max(left, 0);
    setReqRing(left);
    if (left <= 10) { ring.classList.add("low"); sub.classList.add("low"); }
    if (left <= 0) { // UI 层关窗；接收端 30 秒未应答会自行取消授权
      clearInterval(reqTimer);
      dismissRequest();
    }
  }, 1000);
}

function setReqRing(left) {
  $("req-ring").style.setProperty("--p", Math.max(0, Math.min(100, (100 * left) / 30)) + "%");
}

// Close without answering (timer ran out / request replaced): the receiver
// side times the request out on its own — this only clears our dialog.
function dismissRequest() {
  clearInterval(reqTimer);
  reqCur = null;
  $("req-backdrop").hidden = true;
  nextRequest();
}

async function answerRequest(accept) {
  const id = reqCur && reqCur.id;
  clearInterval(reqTimer);
  reqCur = null;
  $("req-backdrop").hidden = true;
  if (id) {
    try {
      // Go 接收器的确认要回给 Kotlin；Rust 接收器的走 oneshot 表。
      if (goAsks.has(id)) {
        goAsks.delete(id);
        await invoke("node_respond", { id, accept });
      } else {
        await invoke("request_respond", { id, accept });
      }
    }
    catch (e) { toast(String((e && e.message) || e), "err"); }
  }
  nextRequest();
}

/* ---------------------------------------------------------------- sending */

function updateOverall(doneBytes, totalBytes, t0) {
  const pct = totalBytes ? Math.min(100, (100 * doneBytes) / totalBytes) : 0;
  setMount("ofill", (el) => { el.style.width = pct.toFixed(1) + "%"; });
  let text = "";
  if (totalBytes) {
    text = `${fmtSize(doneBytes)} / ${fmtSize(totalBytes)}`;
    const secs = (Date.now() - t0) / 1000;
    if (secs > 0.5) text += ` · ${(doneBytes / 1e6 / secs).toFixed(1)} MB/s`;
  }
  setMount("otext", (el) => { el.textContent = text; });
}

function setSendLabel(text) {
  $$('[data-action="send"]').forEach((b) => { b.textContent = text; });
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

async function sendData(item, addr, reqId = "") {
  const { id } = await invoke("send_data_begin", { target: addr, name: item.rel, size: item.size, reqId: reqId || null });
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
  if (!targets.length) { toast("先点选下面的设备（可多选），或输入对方 IP", "err"); return; }
  if (!ready.length) { toast("没有要发送的文件", "err"); return; }
  // 勾选后掉线/对方停了 Tailsend 的设备（手动输入的 IP 不在设备列表里，
  // 交给发送流程去报错；探测中的暂态不拦）
  const dead = targets.filter((t) => {
    const p = peers.find((x) => peerKey(x) === t.key);
    return p && (!p.online || p.recv === false);
  });
  if (dead.length) {
    targets = targets.filter((t) => !dead.includes(t));
    renderPeers();
    updateTargetUI();
    toast(`${dead.map((d) => d.label).join("、")} 已离线，已从本次发送移除`, "err");
    if (!targets.length) return;
  }
  sending = true;
  document.body.classList.add("sending");
  updateSendBtn();
  ready.forEach((i) => { i.tried = 0; i.okN = 0; i.error = null; });

  // 1:N — every file goes to every target, one target at a time so progress
  // stays readable and a dead target can't garble the others' results.
  const perTarget = targets.map((t) => ({ t, ok: 0, fail: 0 }));
  const totalBytes = ready.reduce((s, i) => s + i.size, 0) * targets.length;
  let doneBytes = 0;
  const t0 = Date.now();
  let okc = 0, failc = 0;

  for (const pt of perTarget) {
    // One consent round trip per target: the whole batch is confirmed on the
    // receiver together, so a multi-file send pops one dialog, not many.
    // Sends below carry the grant id; old receivers answer "" and proceed.
    setSendLabel(`等待 ${pt.t.label} 确认…`);
    const entries = ready.map((i) => ({ path: i.path, rel: i.rel, size: i.size }));
    let reqId = "";
    try {
      reqId = nodeRunning ? "" : (await invoke("request_send", { target: pt.t.addr, entries })) || "";
    } catch (err) {
      // Refused / nobody there / timeout: the whole batch fails for this
      // target — say so once, keep the other targets going.
      pt.fail += ready.length; failc += ready.length;
      const why = String((err && err.message) || err);
      ready.forEach((i) => { i.tried++; i.error = why; });
      toast(`→ ${pt.t.label}：${why}`, "err");
      continue;
    }
    setSendLabel("发送中…");
    for (const item of ready) {
      item.status = "sending";
      item.loaded = 0;
      renderQueue();
      try {
        if (item.mode === "path") {
          const batch = await invoke("send_files", {
            target: pt.t.addr,
            entries: [{ path: item.path, rel: item.rel, size: item.size }],
            reqId: reqId || null,
          });
          await new Promise((res, rej) => {
            // The Rust task fires events from a spawned task; a fast local
            // push can beat the invoke() reply back into JS. Late results
            // are parked in doneBatches so the wait still settles.
            const hit = doneBatches.get(batch);
            if (hit) {
              doneBatches.delete(batch);
              hit.ok ? res() : rej(new Error(hit.error || "发送失败"));
              return;
            }
            pendingBatch.set("p:" + batch, { item, res, rej });
          });
        } else {
          await sendData(item, pt.t.addr, reqId);
        }
        pt.ok++; okc++; item.okN++;
      } catch (err) {
        pt.fail++; failc++;
        toast(`「${item.rel}」→ ${pt.t.label} 发送失败：${String((err && err.message) || err)}`, "err");
      }
      doneBytes += item.size;
      item.tried++;
      // perTarget is a snapshot on purpose: targets[] may change mid-send
      // (selection is still live), and completion must not depend on it.
      if (item.tried >= perTarget.length) {
        item.status = item.okN === item.tried ? "done" : "error";
        if (item.status === "error") item.error = "部分目标接收失败，详见提示";
        item.loaded = item.okN ? item.size : 0;
      }
      renderQueue();
      updateOverall(doneBytes, totalBytes, t0);
    }
  }

  sending = false;
  document.body.classList.remove("sending");
  setSendLabel("发送");
  renderQueue(); // 恢复各行 ✕（发送期间删除钮整体隐藏）
  updateTargetUI();
  updateSendBtn();
  perTarget.forEach((x) => { if (x.ok > 0) saveTarget(x.t.addr, x.t.label); }); // remember where sends succeed
  const bad = perTarget.filter((x) => x.fail > 0).map((x) => x.t.label);
  toast(
    failc
      ? `发送完成：${okc} 成功 / ${failc} 失败${bad.length ? `（${bad.join("、")}）` : ""}`
      : `已发送 ${okc} 个文件给 ${perTarget.length} 个目标`,
    failc ? "err" : "ok"
  );
  loadInbox();
}

/* ---------------------------------------------------------------- inbox */

// 类型 → 配色 tile 里的小图标（stroke 线稿，CSS 上色）；收件箱和发送队列共用
const FILE_ICONS = {
  img: '<svg viewBox="0 0 24 24"><rect x="3.5" y="4.5" width="17" height="15" rx="2.5"/><circle cx="9" cy="10" r="1.6"/><path d="M5.5 17.5l4-4 3.5 3.5 3-3 2.5 2.5"/></svg>',
  vid: '<svg viewBox="0 0 24 24"><rect x="3" y="5.5" width="13" height="13" rx="2.5"/><path d="M16 10l4.5-2.5v9L16 14"/></svg>',
  aud: '<svg viewBox="0 0 24 24"><path d="M9.5 17V6.5l9-2v10.5"/><circle cx="7.2" cy="17.5" r="2.2"/><circle cx="16.2" cy="14.5" r="2.2"/></svg>',
  zip: '<svg viewBox="0 0 24 24"><rect x="4.5" y="3.5" width="15" height="17" rx="2.5"/><path d="M12 4.5v2.5m0 2.5V12m0 2.5V17"/></svg>',
  doc: '<svg viewBox="0 0 24 24"><path d="M7 3.5h6L17.5 8v11a2 2 0 0 1-2 2h-8a2 2 0 0 1-2-2V5.5a2 2 0 0 1 2-2z"/><path d="M12.8 3.7V8.2h4.5M9.5 13h5m-5 3.5h5"/></svg>',
  bin: '<svg viewBox="0 0 24 24"><path d="M7 3.5h6L17.5 8v11a2 2 0 0 1-2 2h-8a2 2 0 0 1-2-2V5.5a2 2 0 0 1 2-2z"/><path d="M12.8 3.7V8.2h4.5"/></svg>',
};

function fileKind(name) {
  const m = /\.([a-z0-9]+)$/i.exec(name);
  const e = m ? m[1].toLowerCase() : "";
  if (["png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "bmp", "svg", "avif", "tiff", "ico"].includes(e)) return "img";
  if (["mp4", "mov", "mkv", "avi", "webm", "m4v", "flv", "wmv"].includes(e)) return "vid";
  if (["mp3", "wav", "flac", "aac", "ogg", "m4a", "opus", "wma"].includes(e)) return "aud";
  if (["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "dmg", "iso"].includes(e)) return "zip";
  if (["pdf", "doc", "docx", "pages", "txt", "md", "rtf", "csv", "xls", "xlsx", "ppt", "pptx", "key", "numbers"].includes(e)) return "doc";
  return "bin";
}

// 两次点击确认：第一次点亮为「确认删除」，2.6 秒内再点才真删
let armedDelete = null;
let lastInboxKey = null; // 空 inbox 的 key 是 ""，初始必须不同才能画出空态文案

const inboxKey = (files) =>
  files.map((f) => `${f.incoming ? "~" : "+"}${f.path} ${f.size}/${f.total || 0} ${f.mtime}`).join("|");

// 有文件从「无接收」变「接收中」的那个轮询沿，把界面带到接收页。
// null = 首轮只观察（避免 webview 刷新/残留 temp 时一打开就跳页）。
let hadIncoming = null;

async function loadInbox() {
  try {
    const d = await invoke("inbox_list");
    const hasIn = d.files.some((f) => f.incoming);
    if (hasIn && hadIncoming === false) setTab("recv");
    hadIncoming = hasIn;
    setMount("inbox-count", (el) => { el.textContent = d.total ? `（${d.total}）` : ""; });
    setMount("badge-inbox", (el) => {
      el.hidden = !d.total;
      el.textContent = d.total > 99 ? "99+" : d.total;
    });
    const key = inboxKey(d.files);
    if (key === lastInboxKey) return; // 列表没变就别动 DOM，保留悬停/确认态
    lastInboxKey = key;
    if (!d.files.length) {
      knownInbox.clear();
      armedDelete = null;
      mount("inbox").forEach((el) => {
        el.innerHTML = '<div class="empty">还没有收到文件——在另一台设备上选择本机作为目标发送即可</div>';
      });
      return;
    }
    const html = d.files.map((f) => {
      const base = f.path.split("/").pop();
      const dir = f.path.includes("/") ? f.path.slice(0, f.path.lastIndexOf("/")) : "";
      const k = fileKind(base);
      // root|path 才是唯一键：同名文件在两个收件根各有一份（局域网收的 vs
      // 内置节点收的）时，它们是两份独立文件，各自的删除/打开只针对自己。
      const armKey = (f.root || 0) + "|" + f.path;
      const fresh = !f.incoming && !knownInbox.has(armKey);
      let bar = "";
      let sub;
      if (f.incoming) {
        // total 缺席（Go 端或 v1 临时文件）→ 流动条；有 total → 真百分比
        const pct = f.total ? Math.min(100, Math.round((f.size / f.total) * 100)) : 0;
        bar = `<span class="ibar${f.total ? "" : " indet"}"><i${f.total ? ` style="width:${pct}%"` : ""}></i></span>`;
        sub = `接收中 · 已收 ${fmtSize(f.size)}${f.total ? " / " + fmtSize(f.total) : ""}`;
      } else {
        sub = `${dir ? esc(dir) + " · " : ""}${fmtSize(f.size)} · ${fmtTime(f.mtime)}`;
      }
      const del = f.incoming ? "" :
        `<button class="idel${armedDelete === armKey ? " arm" : ""}" data-path="${esc(f.path)}" data-root="${f.root || 0}"
           title="删除" aria-label="删除 ${esc(base)}">${armedDelete === armKey ? "确认删除" : "✕"}</button>`;
      return `<li class="${f.incoming ? "in " : ""}${fresh ? "new" : ""}" data-path="${esc(f.path)}" data-root="${f.root || 0}" title="${esc(f.path)}">
        <span class="itic k-${k}">${FILE_ICONS[k]}</span>
        <span class="imeta"><span class="iname">${esc(base)}</span><span class="isub">${sub}</span>${bar}</span>${del}
      </li>`;
    }).join("");
    mount("inbox").forEach((el) => { el.innerHTML = html; });
    d.files.forEach((f) => { if (!f.incoming) knownInbox.add((f.root || 0) + "|" + f.path); });
    if (armedDelete && !d.files.some((f) => (f.root || 0) + "|" + f.path === armedDelete)) armedDelete = null;
  } catch (_) { /* receiver dir may not exist yet */ }
}

/* ---------------------------------------------------------------- wiring */

// Bind a handler to every instance of a data-action control.
// bindAll runs fn(el) once per instance immediately — for state sync;
// onAll assigns it as onclick.
function bindAll(action, fn) {
  $$(`[data-action="${action}"]`).forEach(fn);
}
const onAll = (action, fn) => bindAll(action, (el) => { el.onclick = fn; });

function wire() {
  bindAll("manual-use", (b) => {
    b.onclick = () => setManualTarget(b.closest(".manual").querySelector(".manual-in"));
  });
  $$(".manual-in").forEach((inp) => {
    inp.addEventListener("keydown", (e) => { if (e.key === "Enter") setManualTarget(inp); });
  });
  bindAll("peer-search", (inp) => {
    inp.addEventListener("input", () => {
      peerFilter = inp.value.trim();
      renderPeers();
    });
  });
  bindAll("send", (b) => { b.onclick = send; });

  // Tabs.
  bindAll("tab", (b) => { b.onclick = () => setTab(b.dataset.tab); });

  onAll("open-inbox", async () => {
    try { await invoke("open_inbox"); } catch (e) { toast(String(e), "err"); }
  });
  // 手机没有"打开文件夹"的概念（沙盒里也没有系统文件管理器可跳）——收到的
  // 文件直接点列表里的文件名就能打开。留着这个按钮只会让人点了个寂寞。
  if (platform === "mobile") {
    bindAll("open-inbox", (b) => { b.hidden = true; });
  }
  // 确认接收开关（协议 v3）：默认开——收到的传输先确认再落盘。
  const applyConfirm = () => {
    let on = true;
    try { on = localStorage.getItem("ts.confirm.v1") !== "0"; } catch { /* ignore */ }
    bindAll("confirm", (b) => {
      b.setAttribute("aria-pressed", String(on));
    });
  };
  applyConfirm();
  bindAll("confirm", (b) => {
    b.onclick = async () => {
      const on = b.getAttribute("aria-pressed") !== "true";
      try {
        localStorage.setItem("ts.confirm.v1", on ? "1" : "0");
      } catch { /* ignore */ }
      applyConfirm();
      try { await invoke("set_confirm", { confirm: on }); } catch (e) { toast(String((e && e.message) || e), "err"); }
      toast(on ? "接收前会先确认每个传输" : "已开启自动接收，不再询问", "ok");
    };
  });
  $("req-deny").onclick = () => answerRequest(false);
  $("req-accept").onclick = () => answerRequest(true);

  bindAll("token", (b) => {
    b.onclick = () => {
      const rows = mount("token-row");
      const anyOpen = rows.some((r) => !r.hidden);
      rows.forEach((r) => { r.hidden = anyOpen; });
      bindAll("token", (x) => x.setAttribute("aria-expanded", String(!anyOpen)));
      if (!anyOpen) {
        const inp = rows.find((r) => !r.hidden && r.offsetParent !== null);
        (inp && inp.querySelector(".rx-token") || rows[0] && rows[0].querySelector(".rx-token"))?.focus();
      }
    };
  });
  bindAll("token-save", (b) => {
    b.onclick = () => saveReceiverToken(b.closest(".token-row").querySelector(".rx-token").value);
  });
  $$(".rx-token").forEach((inp) => {
    inp.addEventListener("keydown", (e) => { if (e.key === "Enter") saveReceiverToken(inp.value); });
  });
  // 收件箱删除 + 点文件交给系统打开：事件委托挂在列表上（列表整体重绘）
  mount("inbox").forEach((el) => {
    el.addEventListener("click", async (e) => {
      const btn = e.target.closest(".idel");
      if (!btn) {
        // 点文件名 → 系统查看器（接收中的临时文件不可开）
        const li = e.target.closest("li[data-path]");
        if (li && !li.classList.contains("in")) {
          try { await invoke("open_inbox", { path: li.dataset.path, root: Number(li.dataset.root || 0) }); }
          catch (err) { toast(String(err), "err"); }
        }
        return;
      }
      const root = Number(btn.closest("li[data-path]")?.dataset.root || 0);
      const p = btn.dataset.path;
      const armKey = root + "|" + p;
      if (armedDelete !== armKey) {
        armedDelete = armKey;
        mount("inbox").forEach((n) => {
          n.querySelectorAll(`.idel[data-path="${CSS.escape(p)}"][data-root="${root}"]`).forEach((b) => {
            b.classList.add("arm");
            b.textContent = "确认删除";
          });
        });
        setTimeout(() => {
          if (armedDelete === armKey) {
            armedDelete = null;
            mount("inbox").forEach((n) => {
              n.querySelectorAll(`.idel[data-path="${CSS.escape(p)}"][data-root="${root}"]`).forEach((b) => {
                b.classList.remove("arm");
                b.textContent = "✕";
              });
            });
          }
        }, 2600);
        return;
      }
      armedDelete = null;
      btn.disabled = true;
      try {
        await invoke("inbox_delete", { path: p, root });
        toast(`已删除 ${p.split("/").pop()}`);
      } catch (err) {
        toast(String(err), "err");
      }
      lastInboxKey = "";
      loadInbox();
    });
  });
  // 设备卡片：点选 + 键盘 + 移除常用（事件委托，网格整体重绘）
  mount("peers").forEach((el) => {
    el.addEventListener("click", (e) => {
      const fix = e.target.closest("[data-peers-fix]");
      if (fix) {
        peersFixAction(fix.dataset.peersFix);
        return;
      }
      const del = e.target.closest(".fx-del");
      if (del) {
        e.stopPropagation();
        removeTarget(del.dataset.del);
        loadPeers();
        return;
      }
      const row = e.target.closest(".dev");
      if (row && row.dataset.self) {
        toast("这台设备就是本机——不能发给自己，选其他设备吧");
        return;
      }
      if (row) choosePeer(+row.dataset.i);
    });
    el.addEventListener("keydown", (e) => {
      if (e.key !== "Enter" && e.key !== " ") return;
      const row = e.target.closest(".dev");
      if (row && row.dataset.self) {
        e.preventDefault();
        toast("这台设备就是本机——不能发给自己，选其他设备吧");
        return;
      }
      if (row) {
        e.preventDefault();
        choosePeer(+row.dataset.i);
      }
    });
  });
  mount("queue").forEach((el) => {
    el.addEventListener("click", (e) => {
      const retry = e.target.closest(".fx.retry");
      if (retry) {
        const it = items.find((x) => x.id === +retry.dataset.retry);
        if (!it) return;
        it.status = "ready";
        it.loaded = 0;
        it.okN = 0;
        it.tried = 0;
        it.error = null;
        renderQueue();
        updateSendBtn();
        return;
      }
      const x = e.target.closest(".fx");
      if (x) removeItem(+x.dataset.x);
    });
  });

  /* macOS 的 NSOpenPanel 关闭后 WKWebView 的渲染管线可能整个停住：
     DOM 已更新，但页面停在旧帧，直到下一个窗口事件才重绘。
     style 层的强制合成唤不醒它，需要窗口层的刺激——
     1px 尺寸抖动（肉眼无感）+ 焦点唤回，逼窗口系统重排重绘。 */
  const repaint = async () => {
    document.body.style.transform = "translateZ(0)";
    void document.body.offsetHeight;
    requestAnimationFrame(() => { document.body.style.transform = ""; });
    try {
      const W = window.__TAURI__ && window.__TAURI__.window;
      if (W && W.getCurrentWindow && W.PhysicalSize) {
        const w = W.getCurrentWindow();
        const s = await w.outerSize();
        if (s) {
          await w.setSize(new W.PhysicalSize(s.width, s.height + 1));
          await w.setSize(new W.PhysicalSize(s.width, s.height));
        }
        await w.setFocus();
      }
    } catch {}
  };

  onAll("pick", async () => {
    try {
      const r = await invoke("pick_files");
      if (r && r.length) await addFromPaths(r);
      repaint();
    } catch (e) {
      const why = String(e);
      if (/cancel/i.test(why)) return;
      // 手机上原生选择器不可用就退到 webview 选择器——按钮只有一个，
      // 不能让唯一的入口死掉。
      if (platform === "mobile") { $$(".file-in").forEach((i) => i.click()); return; }
      toast(why, "err");
    }
  });
  onAll("pickdir", async () => {
    try {
      const r = await invoke("pick_folders");
      if (r && r.length) await addFromPaths(r);
      repaint();
    } catch (e) { if (!/cancel/i.test(String(e))) toast(String(e), "err"); }
  });
  $$(".file-in").forEach((inp) => {
    inp.addEventListener("change", (e) => {
      [...e.target.files].forEach(addFromFile);
      e.target.value = "";
      repaint();
    });
  });
  $$(".dir-in").forEach((inp) => {
    inp.addEventListener("change", (e) => {
      [...e.target.files].forEach(addFromFile);
      e.target.value = "";
      repaint();
    });
  });

  // Drag & drop is handled by the Tauri webview (desktop only).
  listen("tauri://drag-enter", () => mount("drop").forEach((d) => d.classList.add("over")));
  listen("tauri://drag-leave", () => mount("drop").forEach((d) => d.classList.remove("over")));
  listen("tauri://drag-drop", (e) => {
    mount("drop").forEach((d) => d.classList.remove("over"));
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
    } else {
      // No waiter yet: the invoke() reply is still in flight — park the
      // outcome for the wait to pick up instead of dropping it.
      doneBatches.set(p.batch, p);
      if (doneBatches.size > 64) {
        doneBatches.delete(doneBatches.keys().next().value);
      }
    }
  });

  // Restore the last known tailnet address until a fresh probe confirms it.
  try { const ip = localStorage.getItem("ts.myip"); if (ip) rememberSelfIp(ip); } catch { /* ignore */ }
  // Re-apply a saved receiver token at boot (the core starts with none).
  try {
    const t = localStorage.getItem("ts.token.v1");
    if (t) {
      $$(".rx-token").forEach((inp) => { inp.value = t; });
      invoke("set_receiver_token", { token: t }).catch(() => {});
    }
  } catch { /* ignore */ }
  // Re-apply the confirmation preference at boot (default: confirm).
  try {
    invoke("set_confirm", { confirm: localStorage.getItem("ts.confirm.v1") !== "0" }).catch(() => {});
  } catch { /* ignore */ }
  // Incoming transfer requests (protocol v3) pop the consent dialog.
  listen("incoming-request", (e) => showRequest(e.payload));

  setTab("send");
  updateTargetUI();
  paintSelfIp();
  loadStatus();
  loadPeers();
  loadInbox();
  pollNode();
  setInterval(pollNode, 3000);
  setInterval(loadStatus, 10000);
  setInterval(loadPeers, 15000);
  setInterval(loadInbox, 3000);
  // Event-driven refresh: the moment the user switches to the app is when
  // fresh peer data matters most — don't make them wait out the interval.
  window.addEventListener("focus", loadPeers);
}

let peers = [];
let lanPeers = []; // LAN direct-channel replies from the latest lan_scan sweep
let targets = []; // [{ key, label, addr }] — a send fans out to every entry (1:N)
let items = [];
let itemSeq = 0;
let sending = false;
const pendingBatch = new Map(); // batch id -> { item, res, rej }
const doneBatches = new Map(); // send-file events that arrived before their waiter
const knownInbox = new Set();

(async () => {
  await detectPlatform();
  wire();
})();
