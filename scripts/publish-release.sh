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
#   ./scripts/publish-release.sh 1.2.3        # pin an explicit version (escape hatch)
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
BUMP="patch"; EXPLICIT=""; DRYRUN=0; FORCE_SELFSIGN=0
for a in "$@"; do
  case "$a" in
    --major|major) BUMP="major" ;;
    --minor|minor) BUMP="minor" ;;
    --patch|patch) BUMP="patch" ;;
    --dry-run|-n)  DRYRUN=1 ;;
    --self-signed) FORCE_SELFSIGN=1 ;;
    -h|--help)
      grep -E '^#( |$)' "$0" | sed -E 's/^# ?//'; exit 0 ;;
    v[0-9]*.[0-9]*.[0-9]* | [0-9]*.[0-9]*.[0-9]*) EXPLICIT="${a#v}" ;;
    *) die "unknown arg '$a' — use [--major|--minor|--patch] [--self-signed] [--dry-run] [X.Y.Z]" ;;
  esac
done

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
  if [ "$FORCE_SELFSIGN" -eq 1 ]; then
    ok "[dry run] would build a SELF-SIGNED release and publish $TAG (--self-signed) — no changes made."
  elif [ "$APPLE_CI" -eq 1 ]; then
    ok "[dry run] would tag $TAG and let CI build the signed + notarized release — no changes made."
  else
    die "[dry run] Apple signing not detected — a real run would ABORT. Pass --self-signed to build self-signed on purpose, or fix gh/secrets (gh auth status; gh secret list)."
  fi
  exit 0
fi

# --- Fail closed: never ship a self-signed release by accident ----------------
# Default path is notarized-via-CI. If we can't confirm Apple signing (no
# APPLE_CERTIFICATE secret, or `gh secret list` couldn't run), abort rather than
# silently self-signing. A self-signed release must be an explicit choice.
if [ "$APPLE_CI" -eq 0 ] && [ "$FORCE_SELFSIGN" -eq 0 ]; then
  die "Apple signing not detected (no APPLE_CERTIFICATE secret, or 'gh secret list' failed) — refusing to cut a self-signed release.
Releases are notarized via CI. Debug with: gh auth status && gh secret list
To build a self-signed release on purpose, re-run with --self-signed."
fi
[ "$FORCE_SELFSIGN" -eq 1 ] && warn "Building a SELF-SIGNED (not notarized) release — --self-signed was given."

# --- Remaining preconditions (mutating steps follow) --------------------------
# The "murmur dev" cert is only needed for the local self-signed build; the CI
# path signs with the Developer ID cert stored in GitHub secrets.
if [ "$APPLE_CI" -eq 0 ]; then
  security find-identity -v -p codesigning 2>/dev/null | grep -qF "murmur dev" \
    || die "self-signed 'murmur dev' cert not found — run ./scripts/setup.sh first."
fi

BRANCH="$(git branch --show-current)"
[ "$BRANCH" = "main" ] || warn "not on main (on '$BRANCH') — the release tag will point at this branch."
git diff --quiet && git diff --cached --quiet || die "working tree not clean — commit or stash first."

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
if ! git diff --quiet; then
  git add package.json package-lock.json src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock
  git commit -q -m "release: $TAG"
  ok "committed release: $TAG"
fi
say "Pushing main…"
git push -q origin "$BRANCH"

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
