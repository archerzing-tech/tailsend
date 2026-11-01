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

# 2) Manifest: permissions + <service> + FileProvider (tap-to-open inbox)
python3 - "$APP/AndroidManifest.xml" <<'PYEOF'
import os, sys
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
# Tap-to-install received APKs: the installer attributes the install to us,
# and without this declared there is no "install unknown apps" toggle for
# Tailsend — stock Android dead-ends the first install with no way to allow it.
if "REQUEST_INSTALL_PACKAGES" not in s:
    s = s.replace(
        '<uses-permission android:name="android.permission.INTERNET" />',
        '<uses-permission android:name="android.permission.INTERNET" />\n'
        '    <uses-permission android:name="android.permission.REQUEST_INSTALL_PACKAGES" />')
open(p, "w").write(s)
PYEOF

# 2b) FileProvider paths for tap-to-open the inbox. The tauri template ships
# its own FileProvider + @xml/file_paths (camera sample roots) and re-syncs
# both into gen/ on every build — patching file_paths.xml (or adding a
# second provider) is pointless: the append gets clobbered, and it never
# even runs because "FileProvider" is already in the manifest. So point the
# existing provider at our own uniquely-named resource; the sync never
# touches files it doesn't know about.
python3 - "$APP/AndroidManifest.xml" <<'PYEOF'
import sys
p = sys.argv[1]
s = open(p).read()
if "tailsend_paths" not in s:
    if "FILE_PROVIDER_PATHS" in s:
        s = s.replace(
            'android:resource="@xml/file_paths"',
            'android:resource="@xml/tailsend_paths"')
    else:
        # Very old template with no provider at all: add ours.
        s = s.replace(
            '    </application>',
            '        <provider\n'
            '            android:name="androidx.core.content.FileProvider"\n'
            '            android:authorities="${applicationId}.fileprovider"\n'
            '            android:exported="false"\n'
            '            android:grantUriPermissions="true">\n'
            '            <meta-data\n'
            '                android:name="android.support.FILE_PROVIDER_PATHS"\n'
            '                android:resource="@xml/tailsend_paths" />\n'
            '        </provider>\n'
            '\n'
            '    </application>')
open(p, "w").write(s)
PYEOF
mkdir -p "$APP/res/xml"
cat > "$APP/res/xml/tailsend_paths.xml" <<'XML'
<?xml version="1.0" encoding="utf-8"?>
<paths xmlns:android="http://schemas.android.com/apk/res/android">
  <!-- Whole filesDir: the inbox (files/inbox, files/Tailsend) and the Go
       node's state all live under getFilesDir(); Kotlin open_path hands
       files from there to system viewers via this provider. -->
  <files-path name="files" path="." />
</paths>
XML

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
if "POST_NOTIFICATIONS" not in s:
    # Android 13+: notification permission is runtime-granted — ask on first
    # launch so the receiver's ongoing notification actually shows instead of
    # silently vanishing behind an ungranted permission.
    s = s.replace(
        "    TailsendReceiverService.start(this)",
        "    TailsendReceiverService.start(this)\n"
        "    if (android.os.Build.VERSION.SDK_INT >= 33) {\n"
        "      requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1001)\n"
        "    }")
open(p, "w").write(s)
PYEOF

# Assertions — a silent miss here would ship an APK without the service.
MAN="$APP/AndroidManifest.xml"; ACT="$PKG_DIR/MainActivity.kt"
grep -q 'FOREGROUND_SERVICE_SPECIAL_USE' "$MAN"
grep -q 'TailsendReceiverService' "$MAN"
grep -q 'TailsendReceiverService.start(this)' "$ACT"
grep -q 'POST_NOTIFICATIONS' "$MAN"
grep -q 'requestPermissions' "$ACT"
grep -q 'FileProvider' "$MAN"
grep -q 'tailsend_paths' "$MAN"
grep -q 'REQUEST_INSTALL_PACKAGES' "$MAN"
test -f "$APP/res/xml/tailsend_paths.xml"
test -f "$PKG_DIR/TailsendReceiverService.kt"
echo "foreground service applied: $MAN"
