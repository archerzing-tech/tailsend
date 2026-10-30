#!/bin/zsh
# Tailsend 演示：节点发现 → 本机回环收发 → 接收端保持运行
cd "$(dirname "$0")"
clear
echo "═══════════════ Tailsend 演示 ═══════════════"
echo
echo "① 节点发现：tailsend list"
echo "──────────────────────────────────────────"
./tailsend list
echo
echo "② 回环演示：后台启动接收端 → 发送一个目录（含中文名 + 50MB 文件）"
echo "──────────────────────────────────────────"
DEMO=/tmp/tailsend-demo
INBOX=/tmp/tailsend-demo-inbox
rm -rf "$DEMO" "$INBOX"
mkdir -p "$DEMO"
echo "你好，这是 Tailsend 发来的文件" > "$DEMO/你好.txt"
head -c 52428800 /dev/zero > "$DEMO/demo-50MB.bin"
./tailsend receive -bind 127.0.0.1 -dir "$INBOX" &
RECV=$!
sleep 1
./tailsend send 127.0.0.1:4782 "$DEMO"
echo
echo "③ 接收端落盘的文件："
echo "──────────────────────────────────────────"
find "$INBOX" -type f
echo
echo "接收端仍在运行（Ctrl-C 退出）。你可以继续往里发文件试试："
echo "  ./tailsend send 127.0.0.1:4782 <任意文件或目录>"
echo "  ./tailsend send <文件>          ← 不写对方名，出节点列表让你选"
echo
wait $RECV
