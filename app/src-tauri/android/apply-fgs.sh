#!/usr/bin/env bash
# Inject the Tailsend foreground-service into a freshly `tauri android init`ed
# project. gen/ is not in the repo, so this runs after every init (CI + local).
# Idempotent: re-running on an already-patched tree is a no-op.
set -euo pipefail

SRC_DIR="$(cd "$(dirname "$0")" && pwd)"
APP="app/src-tauri/gen/android/app/src/main"
# Allow running from repo root or from app/
[ -d "$APP" ] || APP="src-tauri/gen/android/app/src/main"
[ -d "$APP" ] || { echo "gen/android project not found — run 'tauri android init' first" >&2; exit 1; }

PKG_DIR="$APP/java/io/tailsend/app"

# 1) Service source
cp "$SRC_DIR/TailsendReceiverService.kt" "$PKG_DIR/TailsendReceiverService.kt"

# 2) Manifest: permissions + <service>
python3 - "$APP/AndroidManifest.xml" <<'PYEOF'
import sys
p = sys.argv[1]
s = open(p).read()
if "TailsendReceiverService" not in s:
    s = s.replace(
        '<uses-permission android:name="android.permission.INTERNET" />',
        '<uses-permission android:name="android.permission.INTERNET" />\n'
        '    <uses-permission android:name="android.permission.FOREGROUND_SERVICE" />\n'
        '    <uses-permission android:name="android.permission.FOREGROUND_SERVICE_SPECIAL_USE" />\n'
        '    <uses-permission android:name="android.permission.POST_NOTIFICATIONS" />')
    s = s.replace(
        '    </application>',
        '        <service\n'
        '            android:name=".TailsendReceiverService"\n'
        '            android:exported="false"\n'
        '            android:foregroundServiceType="specialUse">\n'
        '            <property\n'
        '                android:name="android.app.PROPERTY_SPECIAL_USE_FGS_SUBTYPE"\n'
        '                android:value="Tailnet file transfer receiver stays reachable in background" />\n'
        '        </service>\n'
        '\n'
        '    </application>')
    open(p, "w").write(s)
PYEOF

# 3) MainActivity: start the service on launch (foreground start is allowed)
python3 - "$PKG_DIR/MainActivity.kt" <<'PYEOF'
import sys
p = sys.argv[1]
s = open(p).read()
if "TailsendReceiverService" not in s:
    s = s.replace(
        "    super.onCreate(savedInstanceState)",
        "    super.onCreate(savedInstanceState)\n"
        "    TailsendReceiverService.start(this)")
    open(p, "w").write(s)
PYEOF

# Assertions — a silent miss here would ship an APK without the service.
MAN="$APP/AndroidManifest.xml"; ACT="$PKG_DIR/MainActivity.kt"
grep -q 'FOREGROUND_SERVICE_SPECIAL_USE' "$MAN"
grep -q 'TailsendReceiverService' "$MAN"
grep -q 'TailsendReceiverService.start(this)' "$ACT"
test -f "$PKG_DIR/TailsendReceiverService.kt"
echo "foreground service applied: $MAN"
