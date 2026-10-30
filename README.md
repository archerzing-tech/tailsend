# Tailsend

[![CI](https://github.com/archerzing-tech/tailsend/actions/workflows/ci.yml/badge.svg)](https://github.com/archerzing-tech/tailsend/actions/workflows/ci.yml)
[![Release](https://github.com/archerzing-tech/tailsend/actions/workflows/release.yml/badge.svg)](https://github.com/archerzing-tech/tailsend/actions/workflows/release.yml)

在 [Tailscale](https://tailscale.com) 网络里的多台机器之间互传文件的小工具。

- **`app/` — 图形界面版（Tauri 2）**：桌面（macOS / Windows / Linux）+ 手机（Android / iOS），见 [app/README.md](app/README.md)
- **根目录 — 命令行版（Go）**：适合 NAS、服务器、脚本场景

两者走同一个传输协议（HTTP :4782），可以互相收发——比如手机 App 发文件给 NAS 上常驻的 CLI 接收端。

只有两个功能（发送 / 接收）加一个节点列表：

- **节点列表** — 发现 tailnet 里的所有节点（在线状态、IP、主机名、系统），从列表里选对方
- **发送** — 把文件或目录发给选中的节点
- **接收** — 在本机启动接收端，等别人发文件过来

## 前置条件：必须先安装并登录 Tailscale

**Tailsend 不自带组网能力，它完全依赖 [Tailscale](https://tailscale.com) 传输。** 没装 Tailscale 或没连上 tailnet 时，节点列表为空、收发都会失败。开始之前：

1. **每台要收发文件的设备都装好 Tailscale 并登录同一个 tailnet**（官网注册免费，最多 100 台设备）；
   - 电脑：[tailscale.com/download](https://tailscale.com/download)（macOS / Windows / Linux）
   - 手机：应用商店搜 "Tailscale"（Android / iOS）
2. **桌面端（GUI 和 Go CLI）还需要 `tailscale` 命令行可用**——节点发现靠它执行 `tailscale status --json`。Linux/Windows 装官方包即自带；macOS 若用 App Store 版 Tailscale，Tailsend 会自动找 `/Applications/Tailscale.app` 内置的 CLI，无需额外配置；
3. **手机端不用 CLI**：装 Tailscale App 并连上（VPN 图标亮起）即可；Tailsend 手机版的节点列表用「收藏目标」代替，首次使用输入对方 IP，之后自动记住并探测在线（安卓版支持 Android 10+，arm64）；
4. 确认设备间能互通：终端里 `tailscale status` 能看到对方、`ping 100.x.y.z`（对方的 Tailscale IP）能通，Tailsend 就能用。

自建/第三方控制面（如 Headscale）也支持——只要设备上的 Tailscale 客户端正常工作即可，Tailsend 不关心控制面是谁。

## 工作原理

```
发送端                         接收端
tailsend send ──HTTP:4782──▶ tailsend receive
     │                            │
     └────── WireGuard 加密隧道（Tailscale 提供）──────┘
```

- 通过 `tailscale status --json` 发现节点，无需手动记 IP；
- 传输就是 tailnet 内的普通 HTTP，加密由 Tailscale（WireGuard）保证，不用配证书；
- 文件流式传输（`multipart` 流），内存占用与文件大小无关，GB 级文件也没问题；
- 接收端默认只绑定 Tailscale IP（`100.x.x.x`），局域网里的其他设备摸不到它。

## 安装

需要 Go 1.21+，零第三方依赖：

```bash
git clone <this-repo> tailsend && cd tailsend
go build -o tailsend .
```

交叉编译示例：

```bash
# Linux (x86_64 / ARM，适合 NAS、树莓派、VPS)
GOOS=linux   GOARCH=amd64 go build -o tailsend-linux-amd64 .
GOOS=linux   GOARCH=arm64 go build -o tailsend-linux-arm64 .
# Windows
GOOS=windows GOARCH=amd64 go build -o tailsend.exe .
# macOS (Apple Silicon)
GOOS=darwin  GOARCH=arm64 go build -o tailsend-macos-arm64 .
```

把编译好的 `tailsend` 放进各机器的 `PATH`（如 `/usr/local/bin/`）。

## 使用

**第一步：在接收的机器上启动接收端**（想收文件的每台机器都跑一次）

```console
$ tailsend receive
Tailsend receiver v0.0.1

  this machine : homelab-nas
  saving to    : /home/me/Downloads/Tailsend
  auth         : none — every node your Tailscale ACLs allow can send (add -token to require a secret)
  reachable at :
                 http://100.101.1.8:4782

Waiting for files… (Ctrl-C to stop)
```

**第二步：在发送的机器上看看有哪些节点**

```console
$ tailsend list
This machine: lisa-mbp (100.64.0.5)
  STATUS     ADDRESS          HOSTNAME      OS       LAST SEEN
  ● online   100.101.1.8      homelab-nas   linux
  ● online   100.122.53.56    vps-hk        linux
  ○ offline  100.99.0.3       office-pc     windows  3h ago

Send with: tailsend send <hostname> <file-or-directory>
```

**第三步：发送**

```bash
tailsend send homelab-nas ~/report.pdf          # 单个文件
tailsend send homelab-nas ~/photos ~/notes.md   # 多个目标，目录递归
tailsend send 100.101.1.8 backup.tar.gz         # 直接用 IP
tailsend send 100.101.1.8:4782 backup.tar.gz    # IP + 自定义端口
tailsend send ~/movie.mkv                       # 不写对方名字 → 出列表让你选
```

发送端输出：

```console
$ tailsend send homelab-nas ~/photos
Sending 132 files (412.6 MB) to homelab-nas:4782
Receiver "homelab-nas" saves into /home/me/Downloads/Tailsend
[1/132] photos/IMG_0001.jpg (2.1 MB)
  ↑ 412.6 MB / 412.6 MB (100%)  38.2 MB/s
Done: 132/132 files sent in 11.2s
```

对端实时打印收到的东西：

```
14:32:05 ← photos/IMG_0001.jpg (2.1 MB) from lisa-mbp
```

目录会保持相对结构落在接收目录下（`photos/IMG_0001.jpg`），中文名等非 ASCII 文件名也没问题。

## 命令参考

### `tailsend list`

无需参数。第一行是本机，表格是 tailnet 里的其他节点（在线的排前面）。节点名匹配支持：主机名、完整 FQDN、唯一子串（`send nas` 可匹配 `homelab-nas`）、IP。有重名歧义时会列出候选让你用 IP。

### `tailsend send [flags] [peer] <path>...`

| flag | 说明 |
|---|---|
| `-port N` | 对端接收端口，默认 `4782` |
| `-token STR` | 对端开了 `-token` 时传入同样的密钥 |

- `<path>` 可以是文件或目录，目录递归展开；
- 交互选择：`tailsend send <文件>` 省略 peer 时会打印编号列表让你选；
- 目标是 IP 时可写 `ip:port` 覆盖默认端口；
- 任何一个文件失败其余继续传，最终退出码非 0。

### `tailsend receive [flags]`

| flag | 说明 |
|---|---|
| `-port N` | 监听端口，默认 `4782` |
| `-dir PATH` | 保存目录，默认 `~/Downloads/Tailsend`（无 Downloads 时为 `./tailsend-inbox`） |
| `-bind ADDR` | 绑定地址，默认本机 Tailscale IPv4；`-bind 0.0.0.0` 会暴露给局域网，慎用 |
| `-token STR` | 要求发送方携带的共享密钥 |
| `-max BYTES` | 拒收超过此大小的单文件（0 不限） |

- 同名文件不覆盖，自动存为 `name (1).ext`、`name (2).ext`；
- 文件先写 `.part` 临时文件，传完原子改名——连接中断不会留下半截文件；
- 路径经过清洗，`../` 之类的穿越路径会被拍平到保存目录内。

## 推荐加上 `-token`

tailnet 里能路由到对方 4782 端口的节点（取决于你的 ACL）都能给它发文件。两端约定同一个密钥即可：

```bash
# 接收端
tailsend receive -token "our-secret"
# 发送端
tailsend send -token "our-secret" homelab-nas file.zip
```

## 让接收端常驻

**nohup（最快）：**

```bash
nohup tailsend receive >> ~/.tailsend.log 2>&1 &
```

**Linux systemd（`/etc/systemd/system/tailsend.service`）：**

```ini
[Unit]
Description=Tailsend receiver
After=tailscaled.service
Wants=tailscaled.service

[Service]
ExecStart=/usr/local/bin/tailsend receive
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

然后 `systemctl enable --now tailsend`。

**macOS：** 可写成 launchd 的 `LaunchAgent`，或直接放进「登录项」。

## 注意事项

- **ACL**：默认 ACL 全放行，开箱即用；如果你的 tailnet 配了自定义 ACL，需要放行节点间 TCP `4782`（或你自定义的端口）。
- **离线节点**：对方不在线时发送会失败（先 ping 检查，5 秒超时），不会傻等。
- **不支持断点续传**：失败重发整个文件即可；`tailscale file send`（Taildrop）是另一条路，本工具不依赖它，Linux/Windows/全平台行为一致。
- **receiver 不验发件人身份**：`sender` 字段只是展示用；安全边界是 Tailscale ACL + 可选 `-token`。
- 这台机器的 Tailscale 未登录/未连接时，`list`/`send`/`receive`（默认绑定模式）都会给出明确报错。

## 项目结构

```
tailsend/
├── main.go          # CLI 入口与子命令分发
├── peers.go         # 节点发现（tailscale status）、名称解析、list
├── send.go          # 发送端：目录展开、multipart 流上传、进度条、交互选节点
├── receive.go       # 接收端：HTTP 服务、流式落盘、防穿越、token 校验
├── util.go          # 通用小工具函数
└── app/             # 图形界面版（Tauri 2，桌面 + 手机），见 app/README.md
    ├── ui/          # 前端：index.html + app.css + app.js（无打包器）
    └── src-tauri/   # Rust 核心：发现 / 接收端 / 发送，与 Go 版同一协议
```

## 协议（Go / Rust 两端共用）

| | |
|---|---|
| 发现 | `tailscale status --json`（CLI 与 GUI 桌面端共用这一条路径） |
| 握手 | `GET /api/ping` → `{"ok":true,"name":…,"version":…,"your_ip":…}`（`your_ip` = 调用方在接收端眼里看到的 IP，供移动端得知自己的 tailnet 地址） |
| 传输 | `POST /api/upload`，multipart；`sender` 字段 + `file` 部分，文件名 = 百分号编码的相对路径 |
| 鉴权 | 可选共享密钥，放 `X-Tailsend-Token` 头，常数时间比较 |
| 安全 | 路径清洗（拍平 `../`）、同名自动 `(1)`、`.part` 临时文件 + 原子改名、可限单文件大小 |
