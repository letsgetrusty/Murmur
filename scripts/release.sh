#!/usr/bin/env bash
#
# release.sh — build a signed, notarized, distributable Open Wispr .dmg.
#
# This is the SHIPPING build (Developer ID + notarization), distinct from
# scripts/dev.sh (local self-signed "openwispr dev" identity). It wraps
# `tauri build`, which — given the right env — compiles a release binary, signs
# it with your Developer ID under the hardened runtime + entitlements.plist,
# notarizes it with Apple, staples the ticket, and packages a .dmg.
#
# Full prerequisites + how to obtain each credential: docs/releasing.md.
#
# Quick reference — export these before running (or put them in a gitignored
# .env you `source`):
#   APPLE_SIGNING_IDENTITY   "Developer ID Application: Your Name (TEAMID)"
#   Notarization — ONE of:
#     (a) App Store Connect API key (recommended for CI):
#         APPLE_API_ISSUER, APPLE_API_KEY, APPLE_API_KEY_PATH
#     (b) Apple ID + app-specific password:
#         APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID
#
# Usage:
#   ./scripts/release.sh              # signed + notarized (needs the env above)
#   ./scripts/release.sh --unsigned   # ad-hoc, NOT distributable — pipeline test

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

say()  { printf '\033[1;36m▶ %s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(uname)" = "Darwin" ] || die "macOS only."

UNSIGNED=0
[ "${1:-}" = "--unsigned" ] && UNSIGNED=1

if [ "$UNSIGNED" -eq 1 ]; then
  warn "Building UNSIGNED (ad-hoc). For local pipeline testing only — this .dmg"
  warn "will NOT pass Gatekeeper on other Macs. Omit --unsigned for a real release."
  # Make sure no stray identity/notarization env sneaks in.
  unset APPLE_SIGNING_IDENTITY APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID \
        APPLE_API_ISSUER APPLE_API_KEY APPLE_API_KEY_PATH 2>/dev/null || true
else
  # --- Signing identity (required) ------------------------------------------
  [ -n "${APPLE_SIGNING_IDENTITY:-}" ] \
    || die "APPLE_SIGNING_IDENTITY unset. e.g. 'Developer ID Application: Your Name (TEAMID)'. See docs/releasing.md"
  security find-identity -v -p codesigning 2>/dev/null | grep -qF "$APPLE_SIGNING_IDENTITY" \
    || die "signing identity '$APPLE_SIGNING_IDENTITY' not found in your keychain. Import your Developer ID cert first (docs/releasing.md)."
  ok "Signing identity: $APPLE_SIGNING_IDENTITY"

  # --- Notarization credentials (warn, don't block) -------------------------
  if [ -n "${APPLE_API_KEY:-}" ] && [ -n "${APPLE_API_ISSUER:-}" ] && [ -n "${APPLE_API_KEY_PATH:-}" ]; then
    ok "Notarization: App Store Connect API key"
  elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
    ok "Notarization: Apple ID + app-specific password"
  else
    warn "No notarization credentials set — the build will be SIGNED but NOT"
    warn "notarized, so first-launch Gatekeeper will still warn users. See"
    warn "docs/releasing.md to finish this properly."
  fi
fi

VERSION="$(python3 -c "import json;print(json.load(open('src-tauri/tauri.conf.json'))['version'])")"
say "Building Open Wispr $VERSION (release) — the first cold build compiles"
say "whisper.cpp + llama.cpp + onnxruntime and can take 10–20 minutes."

# `tauri build` runs beforeBuildCommand (npm run build) → cargo build --release
# → sign (hardened runtime + entitlements) → notarize + staple (if creds) → dmg.
npm run tauri build

# --- Locate + verify outputs -------------------------------------------------
DMG="$(ls -t src-tauri/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)"
APP="src-tauri/target/release/bundle/macos/OpenWispr.app"

echo
ok "Build complete."
[ -n "$DMG" ] && printf '   DMG:  %s\n' "$DMG"
[ -d "$APP" ] && printf '   App:  %s\n' "$APP"

if [ "$UNSIGNED" -eq 0 ] && [ -d "$APP" ]; then
  echo
  say "Verifying signature + notarization…"
  # Gatekeeper assessment: "accepted" + "source=Notarized Developer ID" is the goal.
  spctl -a -vvv --type execute "$APP" 2>&1 | sed 's/^/   /' || warn "spctl assessment failed — see above."
  # Stapled ticket present?
  if xcrun stapler validate "$APP" >/dev/null 2>&1; then
    ok "Notarization ticket stapled."
  else
    warn "No stapled ticket (expected if notarization was skipped)."
  fi
fi

echo
printf 'Next: test the .dmg on a *different* Mac (or a fresh user account) — that\n'
printf 'is the only real check that Gatekeeper accepts it for a first-time user.\n'
