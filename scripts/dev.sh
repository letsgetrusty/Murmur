#!/usr/bin/env bash
#
# dev.sh — run Murmur in development as a *properly signed .app*.
#
# Why this exists (see the debugging session that birthed it):
#   `tauri dev` runs the bare debug binary straight from a shell. For
#   Input Monitoring (the Fn-key event tap), macOS attributes the TCC grant
#   to the *responsible process* — which for a shell-spawned bare binary is
#   the terminal, NOT Murmur. So the Fn tap never receives events, no matter
#   how many times you toggle "murmur" in System Settings.
#
#   This script fixes that on every run by:
#     1. Signing with the stable self-signed "murmur dev" identity
#        (identifier dev.lgr.murmur) so the TCC designated-requirement is
#        constant across rebuilds — grant Input Monitoring once, it sticks.
#     2. Wrapping in a real .app launched via `open` (LaunchServices), so
#        Murmur is its own responsible process and the grant applies.
#
#   The .app's executable is a tiny launcher script that redirects the real
#   binary's output to a log file. (`open --stdout/--stderr` can't be used —
#   those flags take a spawn path Gatekeeper rejects for a self-signed app,
#   failing with -10810. env_logger's stderr also doesn't reach the unified
#   log. The launcher is the only way to both launch cleanly AND get logs.)
#
#   Frontend hot-reload still works: the debug build loads the Vite dev
#   server (localhost:1420), so webview edits are instant. Editing Rust means
#   re-running this script (it does an incremental build).
#
# Usage:      ./scripts/dev.sh
# Tail logs:  tail -f ~/Library/Logs/murmur.log

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

IDENTITY="murmur dev"
BUNDLE_ID="dev.lgr.murmur"
TARGET_DIR="src-tauri/target/debug"
BIN="$TARGET_DIR/murmur"
APP="$TARGET_DIR/Murmur.app"
LOG="$HOME/Library/Logs/murmur.log"
VITE_URL="http://localhost:1420/"

say() { printf '\033[1;36m▶ %s\033[0m\n' "$*"; }
die() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# 0. Preconditions -----------------------------------------------------------
security find-certificate -c "$IDENTITY" >/dev/null 2>&1 \
  || die "Signing identity '$IDENTITY' not in the login keychain. Create a self-signed Code Signing cert with that name (Keychain Access → Certificate Assistant → Create a Certificate → Code Signing), then re-run."

# 1. Vite dev server ---------------------------------------------------------
if curl -s -o /dev/null "$VITE_URL" 2>/dev/null; then
  say "Vite already up on 1420"
else
  say "Starting Vite (npm run dev)…"
  # nohup + detached stdin so the dev server survives this script exiting.
  nohup npm run dev >"$TARGET_DIR/vite.log" 2>&1 </dev/null &
  until curl -s -o /dev/null "$VITE_URL" 2>/dev/null; do sleep 0.3; done
  say "Vite up on 1420"
fi

# 2. Build the debug binary --------------------------------------------------
# --no-default-features matches what `tauri dev` passes.
say "Building (cargo build, incremental)…"
cargo build --manifest-path src-tauri/Cargo.toml --no-default-features

# 3. Assemble the .app -------------------------------------------------------
# Direct binary as the executable (no launcher-script indirection — that broke
# the TCC identity match). The app tees its own logs to ~/Library/Logs/murmur.log,
# so we don't need `open --stdout` (which fails -10810 on a self-signed app).
say "Wrapping in Murmur.app…"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/murmur"
[ -f src-tauri/icons/icon.icns ] && cp src-tauri/icons/icon.icns "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Murmur</string>
  <key>CFBundleDisplayName</key><string>Murmur</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>murmur</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>0.0.0</string>
  <key>CFBundleShortVersionString</key><string>0.0.0</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>NSMicrophoneUsageDescription</key><string>Murmur records audio for dictation.</string>
</dict>
</plist>
PLIST

# 4. Sign with the stable identity ------------------------------------------
say "Signing with '$IDENTITY' (identifier $BUNDLE_ID)…"
codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP"
codesign -dvvv "$APP/Contents/MacOS/murmur" 2>&1 | grep -qi "adhoc" \
  && die "murmur is still ad-hoc signed — signing did not take."
# Fresh bundle at a reused path confuses LaunchServices; clear provenance.
xattr -cr "$APP" 2>/dev/null || true

# 5. Relaunch ----------------------------------------------------------------
say "Relaunching…"
osascript -e 'quit app "Murmur"' >/dev/null 2>&1 || true
pkill -f "Murmur.app/Contents/MacOS/" >/dev/null 2>&1 || true
sleep 1
RUST_LOG="${RUST_LOG:-info}" open "$APP"   # app tees its own log to $LOG

# 6. Confirm the tap actually came up ----------------------------------------
for _ in $(seq 1 20); do grep -qE "Fn-key" "$LOG" 2>/dev/null && break; sleep 0.5; done
if grep -q "Fn-key tap installed and enabled" "$LOG" 2>/dev/null; then
  say "Murmur up — Fn tap enabled."
else
  printf '\033[1;33m⚠ Fn tap not enabled. Grant Accessibility to Murmur and re-run:\033[0m\n'
  grep -E "Fn-key" "$LOG" 2>/dev/null || true
  printf '   System Settings → Privacy & Security → Accessibility → enable Murmur\n'
fi
echo "   Logs:     tail -f $LOG"
echo "   Triggers: hold Fn to dictate · Cmd+Shift+R read-aloud · Alt+Shift+S speed"
