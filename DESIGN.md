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

## 4. 路线

| 阶段 | 内容 | 状态 |
|---|---|---|
| P1 | 协议 v3 双端（Go + Rust）+ 测试 | Go ✅ / Rust ⏳ |
| P2 | GUI 重设计：确认弹窗、等待确认态、接收/发送两区、设置开关 | ⏳ |
| P3 | tsnet all-in-one（mobile 包 → AAR → Kotlin → 桥 → UI） | ⏳ |
| P4 | v0.2.0 发布（版本号 6 处 + release notes） | ⏳ |
