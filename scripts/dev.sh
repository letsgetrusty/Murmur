#!/usr/bin/env bash
#
# dev.sh — run Open Wispr in development as a *properly signed .app*.
#
# Why this exists (see the debugging session that birthed it):
#   `tauri dev` runs the bare debug binary straight from a shell. For
#   Input Monitoring (the Fn-key event tap), macOS attributes the TCC grant
#   to the *responsible process* — which for a shell-spawned bare binary is
#   the terminal, NOT Open Wispr. So the Fn tap never receives events, no matter
#   how many times you toggle "openwispr" in System Settings.
#
#   This script fixes that on every run by:
#     1. Signing with the stable self-signed "openwispr dev" identity
#        (identifier ai.openwispr.app) so the TCC designated-requirement is
#        constant across rebuilds — grant Input Monitoring once, it sticks.
#     2. Wrapping in a real .app launched via `open` (LaunchServices), so
#        Open Wispr is its own responsible process and the grant applies.
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
# Tail logs:  tail -f ~/Library/Logs/openwispr.log

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

IDENTITY="openwispr dev"
BUNDLE_ID="ai.openwispr.app"
TARGET_DIR="src-tauri/target/debug"
BIN="$TARGET_DIR/openwispr"
APP="$TARGET_DIR/OpenWispr.app"
LOG="$HOME/Library/Logs/openwispr.log"
VITE_URL="http://localhost:1420/"

say() { printf '\033[1;36m▶ %s\033[0m\n' "$*"; }
die() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# 0. Preconditions -----------------------------------------------------------
security find-certificate -c "$IDENTITY" >/dev/null 2>&1 \
  || die "Signing identity '$IDENTITY' not in the login keychain. Run ./scripts/setup.sh to create and trust it (first-time setup), then re-run this."

# Fresh clone has no node_modules (gitignored); setup.sh normally installs them,
# but guard here so a bare `dev.sh` still works.
[ -d node_modules ] || { say "Installing frontend deps (npm install)…"; npm install; }

# 1. Build the frontend ------------------------------------------------------
# The built binary embeds ../dist (frontendDist) at compile time — it does NOT
# read from the Vite dev server. So we must rebuild dist here, or frontend edits
# silently won't ship. Cheap (~100 ms) and keeps the deployed app in sync.
say "Building frontend (npm run build)…"
npm run build >"$TARGET_DIR/vite-build.log" 2>&1 || die "frontend build failed — see $TARGET_DIR/vite-build.log"

# 2. Build the debug binary --------------------------------------------------
# --no-default-features matches what `tauri dev` passes.
say "Building (cargo build, incremental)…"
cargo build --manifest-path src-tauri/Cargo.toml --no-default-features

# 3. Assemble the .app -------------------------------------------------------
# Direct binary as the executable (no launcher-script indirection — that broke
# the TCC identity match). The app tees its own logs to ~/Library/Logs/openwispr.log,
# so we don't need `open --stdout` (which fails -10810 on a self-signed app).
say "Wrapping in OpenWispr.app…"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/openwispr"
[ -f src-tauri/icons/icon.icns ] && cp src-tauri/icons/icon.icns "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Open Wispr</string>
  <key>CFBundleDisplayName</key><string>Open Wispr</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>openwispr</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>0.0.0</string>
  <key>CFBundleShortVersionString</key><string>0.0.0</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>NSMicrophoneUsageDescription</key><string>Open Wispr records audio for dictation.</string>
</dict>
</plist>
PLIST

# 4. Sign with the stable identity ------------------------------------------
say "Signing with '$IDENTITY' (identifier $BUNDLE_ID)…"
codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP"
codesign -dvvv "$APP/Contents/MacOS/openwispr" 2>&1 | grep -qi "adhoc" \
  && die "openwispr is still ad-hoc signed — signing did not take."
# Fresh bundle at a reused path confuses LaunchServices; clear provenance.
xattr -cr "$APP" 2>/dev/null || true

# 5. Relaunch ----------------------------------------------------------------
say "Relaunching…"
osascript -e 'quit app "Open Wispr"' >/dev/null 2>&1 || true
pkill -f "OpenWispr.app/Contents/MacOS/" >/dev/null 2>&1 || true
sleep 1
RUST_LOG="${RUST_LOG:-info}" open "$APP"   # app tees its own log to $LOG

# 6. Confirm the tap actually came up ----------------------------------------
for _ in $(seq 1 20); do grep -qE "Fn-key" "$LOG" 2>/dev/null && break; sleep 0.5; done
if grep -q "Fn-key tap installed and enabled" "$LOG" 2>/dev/null; then
  say "Open Wispr up — Fn tap enabled."
else
  printf '\033[1;33m⚠ Fn tap not enabled. Grant Accessibility to Open Wispr and re-run:\033[0m\n'
  grep -E "Fn-key" "$LOG" 2>/dev/null || true
  printf '   System Settings → Privacy & Security → Accessibility → enable Open Wispr\n'
fi
echo "   Logs:     tail -f $LOG"
echo "   Triggers: hold Fn to dictate · Cmd+Shift+R read-aloud · Alt+Shift+S speed"
