#!/usr/bin/env bash
# Build the embedded-node AAR (gomobile bind of tailsend/mobile) into the
# tsnet plugin's android/libs. gen/ is not in the repo, so this runs after
# every fresh checkout / CI machine / `tauri android init` (local + CI).
# The AAR is ~63MB and never committed. Idempotent by nature (rebuild = ok).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"   # repo root
LIBS="$ROOT/app/src-tauri/plugins/tsnet/android/libs"
AAR="$LIBS/tailsend-mobile.aar"

mkdir -p "$LIBS"

if ! command -v gomobile >/dev/null 2>&1; then
  echo "gomobile not found — installing (needs Go toolchain)" >&2
  go install golang.org/x/mobile/cmd/gomobile@latest
  GOFLAGS= go -C "$ROOT" get -tool golang.org/x/mobile/cmd/gobind 2>/dev/null || true
fi
export PATH="$HOME/go/bin:$PATH"
command -v gobind >/dev/null 2>&1 || { echo "gobind missing — run: go get -tool golang.org/x/mobile/cmd/gobind" >&2; exit 1; }

echo "gomobile bind → $AAR (a few minutes on a cold cache)"
(cd "$ROOT" && gomobile bind -target=android -androidapi 24 -o "$AAR" ./mobile)

# Assertions — a silent miss here ships an APK without the node.
test -s "$AAR"
unzip -l "$AAR" | /usr/bin/grep -q 'jni/arm64-v8a/libgojni.so'
echo "AAR ready: $(du -h "$AAR" | cut -f1) $AAR"
