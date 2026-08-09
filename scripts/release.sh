#!/usr/bin/env bash
#
# release.sh — build a signed, notarized, distributable Murmur .dmg.
#
# This is the SHIPPING build (Developer ID + notarization), distinct from
# scripts/dev.sh (local self-signed "murmur dev" identity). It wraps
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
#   ./scripts/release.sh               # signed + notarized (needs the env above)
#   ./scripts/release.sh --self-signed # stable self-signed "murmur dev" identity —
#                                      # internal distribution WITHOUT an Apple account.
#                                      # Gatekeeper warns once per Mac, but the stable
#                                      # signature keeps Accessibility/mic grants across
#                                      # updates. Still builds the updater artifacts.
#   ./scripts/release.sh --unsigned    # ad-hoc, NOT distributable — pipeline test only

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

say()  { printf '\033[1;36m▶ %s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(uname)" = "Darwin" ] || die "macOS only."

MODE="developerid"
case "${1:-}" in
  --unsigned)    MODE="unsigned" ;;
  --self-signed) MODE="selfsigned" ;;
  "")            MODE="developerid" ;;
  *) die "unknown option '$1' — use --self-signed or --unsigned (or no flag for a Developer ID release)." ;;
esac

case "$MODE" in
  unsigned)
    warn "Building UNSIGNED (ad-hoc). For local pipeline testing only — this .dmg"
    warn "won't pass Gatekeeper, and its ad-hoc signature changes every build, so"
    warn "Accessibility/mic grants won't survive updates. Use --self-signed for"
    warn "internal distribution instead."
    # Make sure no stray identity/notarization env sneaks in.
    unset APPLE_SIGNING_IDENTITY APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID \
          APPLE_API_ISSUER APPLE_API_KEY APPLE_API_KEY_PATH 2>/dev/null || true
    ;;

  selfsigned)
    # Internal distribution without an Apple Developer account: sign with the
    # STABLE self-signed "murmur dev" identity (created by scripts/setup.sh), so
    # each Mac's Accessibility/mic grant persists across updates (a stable
    # designated requirement — see AGENTS.md rule #5). It isn't notarized, so
    # Gatekeeper still warns on first open (one-time bypass).
    SELF_ID="murmur dev"
    security find-identity -v -p codesigning 2>/dev/null | grep -qF "$SELF_ID" \
      || die "self-signed identity '$SELF_ID' not found. Run ./scripts/setup.sh first to create + trust it."
    export APPLE_SIGNING_IDENTITY="$SELF_ID"
    # No notarization for a self-signed build.
    unset APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID \
          APPLE_API_ISSUER APPLE_API_KEY APPLE_API_KEY_PATH 2>/dev/null || true
    ok "Signing identity: $SELF_ID (self-signed, not notarized — internal use)"
    ;;

  developerid)
    # --- Signing identity (required) ----------------------------------------
    [ -n "${APPLE_SIGNING_IDENTITY:-}" ] \
      || die "APPLE_SIGNING_IDENTITY unset. e.g. 'Developer ID Application: Your Name (TEAMID)'. See docs/releasing.md (or use --self-signed for internal builds)."
    security find-identity -v -p codesigning 2>/dev/null | grep -qF "$APPLE_SIGNING_IDENTITY" \
      || die "signing identity '$APPLE_SIGNING_IDENTITY' not found in your keychain. Import your Developer ID cert first (docs/releasing.md)."
    ok "Signing identity: $APPLE_SIGNING_IDENTITY"

    # --- Notarization credentials (warn, don't block) -----------------------
    if [ -n "${APPLE_API_KEY:-}" ] && [ -n "${APPLE_API_ISSUER:-}" ] && [ -n "${APPLE_API_KEY_PATH:-}" ]; then
      ok "Notarization: App Store Connect API key"
    elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
      ok "Notarization: Apple ID + app-specific password"
    else
      warn "No notarization credentials set — the build will be SIGNED but NOT"
      warn "notarized, so first-launch Gatekeeper will still warn users. See"
      warn "docs/releasing.md to finish this properly."
    fi
    ;;
esac

# --- Updater signing key (always needed) -------------------------------------
# createUpdaterArtifacts is on, so EVERY build signs the update archive with the
# minisign updater key (separate from Apple code signing). Default to the key
# generated by `npx tauri signer generate -w ~/.murmur/updater.key`.
if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
  DEFAULT_KEY="$HOME/.murmur/updater.key"
  [ -f "$DEFAULT_KEY" ] \
    || die "updater signing key not found. Set \$TAURI_SIGNING_PRIVATE_KEY, or generate one:
    npx tauri signer generate -w $DEFAULT_KEY
  (then put its public key in tauri.conf.json → plugins.updater.pubkey). See docs/releasing.md."
  export TAURI_SIGNING_PRIVATE_KEY="$(cat "$DEFAULT_KEY")"
  ok "Updater key: $DEFAULT_KEY"
fi
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"

VERSION="$(python3 -c "import json;print(json.load(open('src-tauri/tauri.conf.json'))['version'])")"
say "Building Murmur $VERSION (release) — the first cold build compiles"
say "whisper.cpp + llama.cpp + onnxruntime and can take 10–20 minutes."

# `tauri build` runs beforeBuildCommand (npm run build) → cargo build --release
# → sign (hardened runtime + entitlements) → notarize + staple (if creds) → dmg.
npm run tauri build

# --- Un-shadow the build from LaunchServices ---------------------------------
# The build registers extra dev.lgr.murmur bundles with LaunchServices — the
# temporary create-dmg volume (/Volumes/dmg.XXXX) *and* the release .app itself.
# On a dev machine either one collides with a running/installed copy and macOS
# aborts the app (duplicate bundle id). Harmless for end users (they only ever
# have one copy), so we unregister + eject them here — the files stay on disk for
# uploading; they're just no longer LaunchServices-visible.
LSREG="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
for vol in /Volumes/dmg.*; do
  [ -d "$vol/Murmur.app" ] || continue
  "$LSREG" -u "$vol/Murmur.app" >/dev/null 2>&1 || true
  if hdiutil detach "$vol" -force >/dev/null 2>&1; then
    say "ejected DMG volume $(basename "$vol")"
  fi
done
# Unregister the release .app build output (kept on disk for uploading).
"$LSREG" -u "src-tauri/target/release/bundle/macos/Murmur.app" >/dev/null 2>&1 || true

# --- Locate + verify outputs -------------------------------------------------
DMG="$(ls -t src-tauri/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)"
APP="src-tauri/target/release/bundle/macos/Murmur.app"
# Updater artifact + its detached minisign signature.
TARGZ="$(ls -t src-tauri/target/release/bundle/macos/*.app.tar.gz 2>/dev/null | head -1 || true)"
SIG="$(ls -t src-tauri/target/release/bundle/macos/*.app.tar.gz.sig 2>/dev/null | head -1 || true)"

echo
ok "Build complete."
[ -n "$DMG" ] && printf '   DMG:      %s\n' "$DMG"
[ -d "$APP" ] && printf '   App:      %s\n' "$APP"
[ -n "$TARGZ" ] && printf '   Updater:  %s\n' "$TARGZ"

# --- Generate the update manifest (latest.json) ------------------------------
# The app fetches this from plugins.updater.endpoints (a GitHub Releases URL).
# NOTE: CI (.github/workflows/release.yml → tauri-action) is the primary release
# path and generates + uploads latest.json for you. This local manifest is a
# manual fallback: it points at the vN.N.N GitHub release's assets, so upload
# both the .app.tar.gz and this latest.json to that release.
if [ -n "$TARGZ" ] && [ -n "$SIG" ]; then
  ENDPOINT_HOST="https://github.com/letsgetrusty/murmur/releases/download/v$VERSION"
  MANIFEST="src-tauri/target/release/bundle/latest.json"
  TARGZ_NAME="$(basename "$TARGZ")"
  SIGNATURE="$(cat "$SIG")"
  PUB_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  python3 - "$MANIFEST" "$VERSION" "$PUB_DATE" "$SIGNATURE" "$ENDPOINT_HOST/$TARGZ_NAME" <<'PY'
import json, sys
path, version, pub_date, signature, url = sys.argv[1:6]
json.dump({
    "version": version,
    "notes": f"Murmur {version}",
    "pub_date": pub_date,
    "platforms": {
        "darwin-aarch64": {"signature": signature, "url": url},
    },
}, open(path, "w"), indent=2)
PY
  printf '   Manifest: %s\n' "$MANIFEST"
  echo
  say "Manual publish: create the v$VERSION GitHub release and upload both"
  say "$TARGZ_NAME and latest.json to it (CI does this automatically on tag push)."
fi

if [ "$MODE" = "developerid" ] && [ -d "$APP" ]; then
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
elif [ "$MODE" = "selfsigned" ] && [ -d "$APP" ]; then
  echo
  ok "Signed with a stable identity → Accessibility/mic grants persist across updates."
  warn "Not notarized: on first open each teammate gets a Gatekeeper prompt — they"
  warn "right-click the app → Open (or System Settings → Privacy & Security → Open Anyway)."
fi

echo
printf 'Next: test the .dmg on a *different* Mac (or a fresh user account) — that\n'
printf 'is the only real check that Gatekeeper accepts it for a first-time user.\n'
