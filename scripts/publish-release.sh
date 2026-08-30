#!/usr/bin/env bash
#
# publish-release.sh — cut and publish a Murmur release in one step.
#
# Default path (Apple signing configured — the APPLE_CERTIFICATE GitHub secret
# exists): bump the version, push main, push a vX.Y.Z tag — and stop. The Release
# workflow (.github/workflows/release.yml) then builds a SIGNED + notarized DMG +
# updater artifacts and creates the GitHub release. Building/uploading locally
# here would RACE and clobber that, so we don't.
#
# It FAILS CLOSED: if it can't confirm Apple signing, it ABORTS rather than
# silently shipping a self-signed build. A self-signed release only happens when
# you *explicitly* ask with --self-signed (bump, push main, run
# scripts/release.sh --self-signed, and upload Murmur.dmg + the versioned DMG +
# *.app.tar.gz + .sig + latest.json to a new GitHub release). This keeps an
# escape hatch (lapsed Apple account, a fork) without an accidental footgun.
#
# The version is worked out automatically from the highest released tag, so you
# never hand-type (and can't fat-finger) a number:
#   ./scripts/publish-release.sh              # patch bump  (0.1.3 → 0.1.4)
#   ./scripts/publish-release.sh --minor      # minor bump  (0.1.3 → 0.2.0)
#   ./scripts/publish-release.sh --major      # major bump  (0.1.3 → 1.0.0)
#   ./scripts/publish-release.sh --dry-run    # print the next version + mode, no changes
#   ./scripts/publish-release.sh --self-signed  # deliberate self-signed release (no notarization)
#   ./scripts/publish-release.sh --local      # build + notarize on THIS Mac (0 CI minutes)
#   ./scripts/publish-release.sh --skip-bench   # skip the perf gate (no models/hardware)
#   ./scripts/publish-release.sh 1.2.3        # pin an explicit version (escape hatch)
#
# --local: build + sign + notarize the Developer ID release HERE (via
# scripts/release.sh) and upload the artifacts, instead of on GitHub's macOS
# runner. Saves the whole ~440s CI build (~73 billed min at the 10x macOS rate)
# and reuses your warm cache, at the cost of tying up your Mac + holding the
# Apple creds locally (export them or source a gitignored .env — see
# docs/releasing.md). The bump commit is tagged `[skip ci]` and a guard job in
# release.yml skips the CI build when the artifacts are already uploaded, so a
# --local release never also triggers a redundant CI build.
#
# Before tagging it runs scripts/bench.sh — the core-experience performance gate
# (STT/TTS/dictation-start latency) — and ABORTS on a regression. See
# docs/testing.md.
# The first release (no tags yet) ships the current manifest version.
#
# Prereqs: gh (authenticated) + a clean working tree on main. --self-signed also
# needs the "murmur dev" cert (./scripts/setup.sh).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

say()  { printf '\033[1;36m▶ %s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# --- Parse args ---------------------------------------------------------------
# No hand-typed number by default: bump keyword (patch|minor|major) or an
# explicit X.Y.Z escape hatch, plus an optional --dry-run.
BUMP="patch"; EXPLICIT=""; DRYRUN=0; FORCE_SELFSIGN=0; SKIP_BENCH=0; LOCAL=0
for a in "$@"; do
  case "$a" in
    --major|major) BUMP="major" ;;
    --minor|minor) BUMP="minor" ;;
    --patch|patch) BUMP="patch" ;;
    --dry-run|-n)  DRYRUN=1 ;;
    --self-signed) FORCE_SELFSIGN=1 ;;
    --skip-bench)  SKIP_BENCH=1 ;;
    --local)       LOCAL=1 ;;
    -h|--help)
      grep -E '^#( |$)' "$0" | sed -E 's/^# ?//'; exit 0 ;;
    v[0-9]*.[0-9]*.[0-9]* | [0-9]*.[0-9]*.[0-9]*) EXPLICIT="${a#v}" ;;
    *) die "unknown arg '$a' — use [--major|--minor|--patch] [--self-signed|--local] [--skip-bench] [--dry-run] [X.Y.Z]" ;;
  esac
done
[ "$LOCAL" -eq 1 ] && [ "$FORCE_SELFSIGN" -eq 1 ] && die "--local and --self-signed are mutually exclusive."

[ "$(uname)" = "Darwin" ] || die "macOS only."
command -v gh >/dev/null 2>&1 || die "GitHub CLI (gh) not found — https://cli.github.com"
gh auth status >/dev/null 2>&1 || die "gh not authenticated — run: gh auth login"

# Signed via CI, or self-signed locally? Releases are notarized via CI: if the
# APPLE_CERTIFICATE GitHub secret exists, the Release workflow builds a signed +
# notarized DMG on tag push, so we only bump + tag + push. We FAIL CLOSED — a
# self-signed release only happens when explicitly requested with --self-signed,
# never as a silent fallback (so a `gh` hiccup or a stray run can't ship an
# un-notarized build). See the guard after the dry-run block.
APPLE_CI=0
if gh secret list 2>/dev/null | awk '{print $1}' | grep -qx APPLE_CERTIFICATE; then
  APPLE_CI=1
fi
[ "$FORCE_SELFSIGN" -eq 1 ] && APPLE_CI=0

# --- Work out the next version automatically ----------------------------------
# Base it on the highest released tag so it always increases monotonically (the
# updater requires that). First release (no tags yet) ships the current manifest
# version. --minor/--major change the step; an explicit X.Y.Z overrides.
git fetch --tags --quiet origin 2>/dev/null || true
LATEST="$(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' | sed 's/^v//' | sort -V | tail -1)"
if [ -n "$EXPLICIT" ]; then
  VERSION="$EXPLICIT"
elif [ -z "$LATEST" ]; then
  VERSION="$(node -p "require('./src-tauri/tauri.conf.json').version")"
  say "No prior release tags — shipping the current manifest version."
else
  MAJOR="${LATEST%%.*}"; rest="${LATEST#*.}"; MINOR="${rest%%.*}"; PATCH="${rest##*.}"
  case "$BUMP" in
    major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
    minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
    patch) PATCH=$((PATCH + 1)) ;;
  esac
  VERSION="$MAJOR.$MINOR.$PATCH"
fi
echo "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' || die "computed version invalid: '$VERSION'"
TAG="v$VERSION"
say "Next release: $TAG${LATEST:+  (${EXPLICIT:+explicit}${EXPLICIT:-$BUMP bump from v$LATEST})}"

# --- Guard: the tag / release must not already exist --------------------------
git rev-parse -q --verify "refs/tags/$TAG" >/dev/null 2>&1 && die "tag $TAG already exists locally."
git ls-remote --exit-code --tags origin "$TAG" >/dev/null 2>&1 && die "tag $TAG already exists on origin."
gh release view "$TAG" >/dev/null 2>&1 && die "a GitHub release $TAG already exists."

if [ "$DRYRUN" -eq 1 ]; then
  if [ "$LOCAL" -eq 1 ]; then
    ok "[dry run] would build + notarize $TAG LOCALLY (Developer ID) and upload the artifacts — CI build skipped. No changes made."
  elif [ "$FORCE_SELFSIGN" -eq 1 ]; then
    ok "[dry run] would build a SELF-SIGNED release and publish $TAG (--self-signed) — no changes made."
  elif [ "$APPLE_CI" -eq 1 ]; then
    ok "[dry run] would tag $TAG and let CI build the signed + notarized release — no changes made."
  else
    die "[dry run] Apple signing not detected — a real run would ABORT. Pass --self-signed to build self-signed on purpose, or fix gh/secrets (gh auth status; gh secret list)."
  fi
  exit 0
fi

# --- --local preflight: verify Developer ID + notarization creds NOW ----------
# We build locally with scripts/release.sh, so we don't need the CI signing
# secret — but we DO need the local Developer ID cert, notarization creds, and
# the updater key. Check them before mutating anything, so a missing credential
# aborts up front rather than after the version bump + push.
if [ "$LOCAL" -eq 1 ]; then
  [ -n "${APPLE_SIGNING_IDENTITY:-}" ] \
    || die "--local needs APPLE_SIGNING_IDENTITY (e.g. 'Developer ID Application: … (TEAMID)'). Export it or source your gitignored .env — see docs/releasing.md."
  security find-identity -v -p codesigning 2>/dev/null | grep -qF "$APPLE_SIGNING_IDENTITY" \
    || die "signing identity '$APPLE_SIGNING_IDENTITY' not found in your keychain — import your Developer ID cert (docs/releasing.md)."
  if ! { [ -n "${APPLE_API_KEY:-}" ] && [ -n "${APPLE_API_ISSUER:-}" ] && [ -n "${APPLE_API_KEY_PATH:-}" ]; } \
     && ! { [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; }; then
    die "--local needs notarization creds — either the App Store Connect API key (APPLE_API_ISSUER/APPLE_API_KEY/APPLE_API_KEY_PATH) or Apple ID + app-specific password (APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID). Without them users get Gatekeeper warnings. See docs/releasing.md."
  fi
  [ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ] || [ -f "$HOME/.murmur/updater.key" ] \
    || die "--local needs the minisign updater key — set \$TAURI_SIGNING_PRIVATE_KEY or create $HOME/.murmur/updater.key (docs/releasing.md)."
  ok "Local Developer ID + notarization creds present — building on this Mac."
fi

# --- Fail closed: never ship a self-signed release by accident ----------------
# Default path is notarized-via-CI. If we can't confirm Apple signing (no
# APPLE_CERTIFICATE secret, or `gh secret list` couldn't run), abort rather than
# silently self-signing. A self-signed release must be an explicit choice.
# (--local signs locally with the Developer ID cert checked above, so it doesn't
# need the CI secret and is exempt.)
if [ "$LOCAL" -eq 0 ] && [ "$APPLE_CI" -eq 0 ] && [ "$FORCE_SELFSIGN" -eq 0 ]; then
  die "Apple signing not detected (no APPLE_CERTIFICATE secret, or 'gh secret list' failed) — refusing to cut a self-signed release.
Releases are notarized via CI. Debug with: gh auth status && gh secret list
To build a self-signed release on purpose, re-run with --self-signed."
fi
[ "$FORCE_SELFSIGN" -eq 1 ] && warn "Building a SELF-SIGNED (not notarized) release — --self-signed was given."

# --- Remaining preconditions (mutating steps follow) --------------------------
# The "murmur dev" cert is only needed for the local self-signed build; the CI
# path signs with the Developer ID cert stored in GitHub secrets. (--local uses
# the Developer ID cert, verified in its preflight above.)
if [ "$APPLE_CI" -eq 0 ] && [ "$LOCAL" -eq 0 ]; then
  security find-identity -v -p codesigning 2>/dev/null | grep -qF "murmur dev" \
    || die "self-signed 'murmur dev' cert not found — run ./scripts/setup.sh first."
fi

BRANCH="$(git branch --show-current)"
[ "$BRANCH" = "main" ] || warn "not on main (on '$BRANCH') — the release tag will point at this branch."
git diff --quiet && git diff --cached --quiet || die "working tree not clean — commit or stash first."

# --- Core-experience performance gate (Layer 2) -------------------------------
# Fail closed on a latency/throughput regression before it reaches auto-updating
# users. Runs the STT/TTS/dictation-start benches against thresholds; a missing
# model/hardware SKIPS (not a regression), a real breach ABORTS the release.
# Bypass with --skip-bench (e.g. on a machine without the models). See
# docs/testing.md.
if [ "$SKIP_BENCH" -eq 1 ]; then
  warn "skipping the performance gate (--skip-bench) — core latency is unverified."
else
  say "Running the core-experience performance gate (scripts/bench.sh)…"
  "$REPO_ROOT/scripts/bench.sh" || die "performance gate failed — a core path regressed. Fix it, or re-run with --skip-bench to override."
fi

# --- 1. Set the version in all three manifests --------------------------------
say "Setting version → $VERSION"
npm version "$VERSION" --no-git-tag-version --allow-same-version >/dev/null
# tauri.conf.json: first "version" key.
perl -0pi -e 's/("version"\s*:\s*")[0-9][^"]*(")/${1}'"$VERSION"'${2}/' src-tauri/tauri.conf.json
# Cargo.toml: the [package] version (first version= after [package]).
perl -0pi -e 's/(\[package\][^\[]*?\bversion\s*=\s*")[^"]*(")/${1}'"$VERSION"'${2}/s' src-tauri/Cargo.toml
# Keep Cargo.lock's own package entry in sync so `cargo` doesn't rewrite it later.
perl -0pi -e 's/(name = "murmur"\nversion = ")[^"]*(")/${1}'"$VERSION"'${2}/' src-tauri/Cargo.lock 2>/dev/null || true

# Sanity: all three agree.
pj=$(node -p "require('./package.json').version")
tj=$(node -p "require('./src-tauri/tauri.conf.json').version")
cg=$(grep -m1 '^version = ' src-tauri/Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
[ "$pj" = "$VERSION" ] && [ "$tj" = "$VERSION" ] && [ "$cg" = "$VERSION" ] \
  || die "version mismatch after bump: package.json=$pj tauri.conf=$tj Cargo.toml=$cg"
ok "package.json / tauri.conf.json / Cargo.toml → $VERSION"

# --- 2. Commit + push the bump ------------------------------------------------
# For --local the build happens here, not in CI, so tag the bump commit
# `[skip ci]` — that skips the redundant CI check on the branch push AND the
# tag-triggered release build (the guard job in release.yml is the backstop).
COMMIT_MSG="release: $TAG"
[ "$LOCAL" -eq 1 ] && COMMIT_MSG="release: $TAG [skip ci]"
if ! git diff --quiet; then
  git add package.json package-lock.json src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock
  git commit -q -m "$COMMIT_MSG"
  ok "committed release: $TAG"
fi
say "Pushing main…"
git push -q origin "$BRANCH"

# --- 3-local. Build + notarize on this Mac, then publish ----------------------
# scripts/release.sh compiles (reusing the warm local cache), signs with the
# Developer ID cert, notarizes + staples via Apple, and writes latest.json. We
# then create the GitHub release with all the artifacts. No CI build is spent.
if [ "$LOCAL" -eq 1 ]; then
  say "Building + notarizing $TAG locally (Developer ID) — on this Mac, 0 CI minutes."
  ./scripts/release.sh

  DMG="$(ls -t src-tauri/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)"
  TARGZ="$(ls -t src-tauri/target/release/bundle/macos/*.app.tar.gz 2>/dev/null | head -1 || true)"
  SIG="$(ls -t src-tauri/target/release/bundle/macos/*.app.tar.gz.sig 2>/dev/null | head -1 || true)"
  LATEST="src-tauri/target/release/bundle/latest.json"
  [ -f "$DMG" ]    || die "no .dmg produced — see the build output above."
  [ -f "$TARGZ" ]  || die "no updater .app.tar.gz produced."
  [ -f "$SIG" ]    || die "no updater signature (.sig) produced."
  [ -f "$LATEST" ] || die "no latest.json produced (needed for auto-update)."

  # Stable-named copy so the landing page's fixed download URL always resolves.
  STABLE_DMG="$(dirname "$DMG")/Murmur.dmg"
  cp -f "$DMG" "$STABLE_DMG"

  say "Creating GitHub release ${TAG} + uploading artifacts…"
  # Creates the tag (at the pushed bump commit) and the release together. The
  # tag may trigger release.yml, but its guard job sees Murmur.dmg already
  # present and skips the build.
  gh release create "$TAG" \
    --target "$BRANCH" \
    --title "Murmur $VERSION" \
    --notes "Download **Murmur.dmg** below to install (macOS 11+, Apple Silicon). Existing installs update automatically." \
    "$STABLE_DMG" "$DMG" "$TARGZ" "$SIG" "$LATEST"

  echo
  ok "Published $TAG (built + notarized locally — no CI build)."
  printf '   Release:  %s\n' "$(gh release view "$TAG" --json url --jq .url)"
  printf '   Download: https://github.com/letsgetrusty/Murmur/releases/latest/download/Murmur.dmg\n'
  exit 0
fi

# --- 3. Cut the release ------------------------------------------------------
# Apple configured: tag it and let CI build the signed + notarized release. The
# tag must point at the just-pushed bump commit (release.yml checks that the tag
# matches the app version), so push main first (done above), then the tag.
if [ "$APPLE_CI" -eq 1 ]; then
  say "Apple signing configured — tagging $TAG; CI will build, sign, notarize, and publish."
  git tag -a "$TAG" -m "release $TAG"
  git push -q origin "$TAG"
  ok "Pushed $TAG — the Release workflow is building the notarized DMG."
  # Watch the run to completion, best-effort. This is a COURTESY report only: the
  # tag is pushed and CI owns the build regardless, so we poll resiliently (a
  # dropped connection must not look like a release failure — `gh run watch` isn't,
  # so we don't use it) and never `die`. Apple notarization can be slow (30+ min).
  sleep 5
  run_id="$(gh run list --workflow=release.yml -L 1 --json databaseId --jq '.[0].databaseId' 2>/dev/null || true)"
  if [ -n "${run_id:-}" ]; then
    say "Watching CI run $run_id (build + notarize)…  Ctrl-C is safe — CI keeps going."
    status=""; tries=0
    while [ "$tries" -lt 300 ]; do  # ~100 min cap; transient errors just retry
      status="$(gh run view "$run_id" --json status --jq '.status' 2>/dev/null || true)"
      [ "$status" = "completed" ] && break
      tries=$((tries + 1)); sleep 20
    done
    if [ "$status" = "completed" ]; then
      conclusion="$(gh run view "$run_id" --json conclusion --jq '.conclusion' 2>/dev/null || echo unknown)"
      [ "$conclusion" = "success" ] \
        && ok "Published $TAG (signed + notarized by CI)." \
        || warn "CI run finished '$conclusion' — the release may not have published. Inspect: gh run view $run_id --log-failed"
    else
      warn "Still building — the tag is pushed and CI will finish on its own. Watch it in Actions."
    fi
  fi
  printf '   Release:  https://github.com/letsgetrusty/Murmur/releases/tag/%s\n' "$TAG"
  printf '   Download: https://github.com/letsgetrusty/Murmur/releases/latest/download/Murmur.dmg\n'
  exit 0
fi

# --- Self-signed fallback: build the DMG + updater artifacts locally ----------
say "Building self-signed release (first cold build compiles the C++ deps —"
say "whisper.cpp + llama.cpp + onnxruntime — and can take 10–20 minutes)…"
./scripts/release.sh --self-signed

# --- 4. Locate artifacts ------------------------------------------------------
DMG="$(ls -t src-tauri/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)"
TARGZ="$(ls -t src-tauri/target/release/bundle/macos/*.app.tar.gz 2>/dev/null | head -1 || true)"
SIG="$(ls -t src-tauri/target/release/bundle/macos/*.app.tar.gz.sig 2>/dev/null | head -1 || true)"
LATEST="src-tauri/target/release/bundle/latest.json"
[ -f "$DMG" ]    || die "no .dmg produced — see the build output above."
[ -f "$TARGZ" ]  || die "no updater .app.tar.gz produced."
[ -f "$SIG" ]    || die "no updater signature (.sig) produced."
[ -f "$LATEST" ] || die "no latest.json produced (needed for auto-update)."

# Stable-named copy so the landing page's fixed download URL always resolves.
STABLE_DMG="$(dirname "$DMG")/Murmur.dmg"
cp -f "$DMG" "$STABLE_DMG"

# --- 5. Create the GitHub release + upload assets -----------------------------
say "Creating GitHub release ${TAG}…"
gh release create "$TAG" \
  --target "$BRANCH" \
  --title "Murmur $VERSION" \
  --notes "Download **Murmur.dmg** below to install (macOS 11+, Apple Silicon). Existing installs update automatically." \
  "$STABLE_DMG" "$DMG" "$TARGZ" "$SIG" "$LATEST"

echo
ok "Published $TAG"
printf '   Release:  %s\n' "$(gh release view "$TAG" --json url --jq .url)"
printf '   Download: https://github.com/letsgetrusty/Murmur/releases/latest/download/Murmur.dmg\n'
say "Test the .dmg on another Mac / fresh user to confirm the Gatekeeper + permission flow."
