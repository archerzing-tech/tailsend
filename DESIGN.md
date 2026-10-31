# Tailsend 设计文档

Tailsend 只做两件事：**发送**和**接收**。所有设计围绕这两件事。

## 0. 现状与方向

- 双实现同一协议：Go CLI（仓库根）+ Tauri 2 GUI（`app/`，桌面 + Android），HTTP :4782。
- 协议 v2：`POST /api/upload/begin|chunk|complete|abort` 分块并行 + v1 multipart 兼容回退。
- 两个正在合流的方向：
  1. **确认后传输（consent）**：收到的传输必须经用户确认，字节才开始流动。
  2. **all-in-one（tsnet）**：把 Tailscale 内嵌进 app（userspace WireGuard），手机不再依赖官方 Tailscale App 即可成为 tailnet 节点。

## 1. 协议 v3：确认后传输

### 流程

```
发送端                          接收端
  │ POST /api/request             │
  │ {sender, count, total,        │ → 弹窗：发送方 / N 个文件 / 总大小
  │  files:[{name,size}…]}        │    [拒绝] [接收]（30 秒倒计时）
  │ ◄──────────────────────────── │
  │ 200 {ok, id}      ← 接受      │ （403 {reason:"denied|unattended"} 拒绝
  │                   或自动接受   │  408 {reason:"timeout"} 30 秒无应答）
  │                               │
  │ POST /api/upload/begin        │
  │  X-Tailsend-Request: <id>     │ → 校验 id 有效、未过期、覆盖 (name,size)
  │ → chunk × N → complete        │
```

### 规则

- `/api/request` 一次请求覆盖**整批**文件（一并确认，不逐个弹窗）。文件清单全量携带（发送端上限 5000 个，超出接收端直接拒绝整批）。
- 接受后接收端建立**授权** `id → {files: name→size, expires}`，TTL 10 分钟；`begin` 必须带 `X-Tailsend-Request` 且 name+size 与授权一致；v1 上传只校验 name（v1 的 size 在字节后才知道，但 name 在文件分节之前就到达，仍可拦截）。
- **门控是条件性的**：接收端开「确认」才强制授权头。这带来双向兼容：
  - 新发送端 → 旧接收端：`/api/request` 返回 404 → 视为无确认机制，照旧直发。
  - 旧发送端 → 新接收端（确认关）：无授权头也放行（行为与今天一致）。
  - 旧发送端 → 新接收端（确认开）：403，错误信息指明「请升级发送端或在本端关闭确认」。
- 404/501 仍只表示「无分块协议」；403 一律是拒绝/需授权，不触发 v1 回退。

### 确认策略（谁被询问）

| 端 | 默认 | 说明 |
|---|---|---|
| Go CLI `receive` | 自动接受 | NAS/无头场景是拥有者自己的机器；`-ask` 开启 TTY 逐次询问（非 TTY 时视为拒绝） |
| GUI（桌面/手机） | **每次确认** | 设置里可切「自动接收，不再询问」；手机锁屏期间 30 秒无应答 → 发送端收到超时 |
| tsnet 节点（P3） | 跟随 GUI | 节点协议层复用同一 gate |

`/api/ping` 增加 `confirm: true|false` 字段，发送端 UI 可提前提示「对方需要确认」。

## 2. 客户端 UX（GUI 重设计）

两个一级区域：**发送**（目标 + 队列）与**接收**（确认请求 + 收件箱）。

- **接收弹窗**（桌面居中 modal / 手机底部 sheet）：发送方、N 个文件 · 总大小、前几个文件名的 tile 列表、倒计时环；[拒绝] [接收]。这是全 app 唯一必须人工应答的界面，视觉上是最高优先级事件（超出 toast 级别）。
- **发送端**：目标探测响应里 `confirm:true` → 队列/链路条显示「等待对方确认…」；403 → 队列项错误态「对方拒绝了本次传输」；408 → 「对方未响应」。整批确认失败 = 队列整批标错，可整体重发。
- **收件箱**不变（文件 tile + 进度实时涨），确认请求历史暂不持久化——拒绝即消失，接受即进入传输。
- 设置项：确认开关（默认确认）、令牌；P3 后增加内置节点（登录/退出、本机身份）。

## 3. all-in-one（tsnet，方案 B）

- `tailscale.com/tsnet` 内嵌：userspace WireGuard，无 TUN/VPN 权限，手机不再需要官方 Tailscale App。
- 分层：`mobile/` Go 包封装 tsnet 节点（Start/Login/Listen/Dial + 复用 receive.go 的 http.Handler）→ gomobile bind 出 AAR → Kotlin 前台服务承载节点 → Tauri 桥暴露状态 → UI 登录引导 + 节点列表（`s.Status()` 替代 `/api/peers` 代答 hack）。
- **协议不变**：tsnet 模式下收发走同一套 v2+v3 协议，与 CLI/GUI 互通；consent gate 同时服务普通监听与 tsnet 监听。
- 登录：AuthKey（headless）或 `AwaitStartupURL`（GUI 弹 webview 让用户浏览器授权）。
- root `main.go` 不 import tsnet，CLI 二进制不变胖；tsnet 只进 `mobile/` 包。
- 阶段：P0 在 Mac 上跑通 tsnet 节点收发（尖刺）→ P1 AAR → P2 Kotlin 服务 + Tauri 桥 → P3 UI。
- **P0 尖刺结论（2026-10-31，`cmd/tsnet-spike`）**：登录（浏览器授权、状态持久 `~/.tailsend/tsnet-spike`）、入站（`s.Listen` 供 tailnet 访问）、出站（`s.Dial` → 对端 receiver HTTP 往返成功）三项全通。
  - **出站冷启动需预热**：全新会话首轮 dial 常在 ~30s 内超时（disco/DERP 路径未热）；对端有任一流量（如 TSMP ping）或重试一轮后即通。产品实现必须带重试与宽松超时（尖刺用 3×30s）。
  - **同 NAT 环境无直连**：本机 ISP 为 CGNAT（en0 即 100.x/23），两节点 hairpin 打洞不成，走 DERP 中继（sfo，RTT ~350ms）。文件传输出中继可用但慢，属可接受 MVP 底线。
  - **调试必须设 `Logf`**：tsnet 默认把内部 disco/derp/netcheck 日志吞进 logtail，不设 `Logf` 则拨号失败完全无迹可查。
- **P1 Go 侧（2026-10-31）✅**：① `core/` 包承载全部协议引擎——`Receiver{Dir,Token,Max,Host,Version,Gate,Peers,Logf,AdvertiseIP}` 挂任意 `net.Listener`（`Serve/Stop`），`/api/peers` 改注入式 Peers func，发送端进度走 `Progress` 回调；CLI 收缩成壳（flags/TTY prompt/进度渲染），wire 协议零变化。② `mobile/` 包 = tsnet 节点 + core 引擎的 gomobile 绑定面：`NewNode(hostname,dir,version)` → 异步 `Start` → `Status()` JSON 轮询（state: starting/needs_login/running/error）→ `LoginURL()`（从 Logf 行 `go to: https://login.tailscale.com/a/…` 捕获）→ `SetPrompt(Prompt 接口)`（Kotlin 实现，桥到 GUI 确认弹窗）→ `StartReceiver(token,confirm)` → `PingTarget/SendFiles`（JSON 结果）。go.mod 用 `go get -tool` 记录 gobind；root/core 不 import tsnet，CLI 二进制保持 ~10.8MB。
- **P2 Kotlin + Tauri 桥（2026-10-31）**：桥 = 本仓库内插件 crate `app/src-tauri/plugins/tsnet/`（tauri-plugin-tsnet，`links` + build.rs `tauri_plugin::Builder.android_path("android")`，CLI 靠 build 脚本输出的 `cargo:android_library_path` 自动把模块注入 gen/android 的 tauri.settings.gradle 与 app/tauri.build.gradle.kts——无需手工 patch gradle）。
  - **Kotlin** `TsnetNodePlugin`（@TauriPlugin，按方法名路由 `@Command`）：companion 持进程级 `Mobile.Node` 单例（ survives webview 重载；前台通知仍归 TailsendReceiverService）；`ensure` 对 error 态节点 shutdown 重建；Prompt.ask 挂 35s CompletableFuture（>Go 30s deadline，超时裁决权留给 Go）；登录跳转 `open_login` 用 ACTION_VIEW intent。
  - **Rust**：`TsnetNode<R>` managed state + `run_mobile_plugin_async`；命令 `node_status/ensure/start_receiver/stop_receiver/pending/respond/ping_target/send_files/open_login`；consent 中继 = 插件 setup 里 400ms 轮询 Kotlin pending → emit 同名 `incoming-request` 事件（payload files 无 size，UI fmtSize 天然容忍），`request_respond` 对非 Rust 的 "n…" id 兜底转发 node.respond——UI 确认弹窗零改动。
  - **发送嫁接（零 UI 改动）**：node running 时 `send_data_begin` 检测到后把 JS 字节暂存 app_data/send-tmp 临时文件（Slot::NodeTemp），`send_data_end` 调 Go SendFiles（consent+上传都由 Go 完成）后删临时文件；`request_send` 预检跳过（Go 自己 request）。node 模式 ping 走 `node_ping_target`（Go PingResult 无 your_ip）。前提：`core.SetDialContext(s.Dial)` 把发送引擎的三个 http client 全部改走 tsnet userspace dialer（普通 net dialer 无 VPN 打不通 100.x——手机没有系统 VPN）。
  - **收件箱合并**：Android save_dir 钉到 app_data/Tailsend（dirs::download_dir 在沙箱无意义）；inbox_list/inbox_delete 扫 app_data/Tailsend + app_data/inbox（node 收件箱）双根。
  - **UI**：内置节点卡片（3s 轮询 node_status；off→启动 / starting / needs_login→打开浏览器登录 / running→IP+tailnet+接收开关（token/confirm 复用现有设置）/ error→重试）；桌面 node_status 报错 → 卡片隐藏。
  - **AAR 不入库**：`apply-node.sh`（gomobile bind → plugins/tsnet/android/libs/，断言 libgojni.so）；release.yml android job 加 setup-go + apply-node 步骤。
- **AAR 产出（已验证，2026-10-31）**：`PATH=$HOME/go/bin:$PATH gomobile bind -target=android -androidapi 24 -o tailsend.aar ./mobile`（go.mod 有 tool gobind；首次全量编译约 4-5 分钟）。产物 ~63MB（4 ABI 各 ~40MB libgojni.so——tsnet 本体大）；接 Kotlin 时 APK 分 ABI 已有，实际进包 arm64+armv7 各 ~40MB，可接受但需在发版说明里注明。生成的 Kotlin 面：`Mobile.newNode(h,d,v)` / `node.status()/loginURL()/ip()/setPrompt(Prompt)/startReceiver(token,confirm)/stopReceiver()/pingTarget()/shutdown()` / `Prompt.ask(sender,count:Long,total:Long,namesJSON)`。

## 4. 路线

| 阶段 | 内容 | 状态 |
|---|---|---|
| P1 | 协议 v3 双端（Go + Rust）+ 测试 | ✅ Go 3c6cd56 / Rust 5fb4020，互操作矩阵 6 场景全过 |
| P2 | GUI：确认弹窗、等待确认态、确认开关（发送/接收两区待后续迭代） | ✅ 弹窗/开关/等待态已落地 |
| P3 | tsnet all-in-one（mobile 包 → AAR → Kotlin → 桥 → UI） | ⏳ P0 尖刺 + core/ + mobile/ + P2 Kotlin/桥/UI ✅（2026-10-31，commit 后补）；剩真机验证 |
| P4 | v0.2.0 发布（版本号 6 处 + release notes） | ⏳ |
