#!/usr/bin/env bash
#
# publish-release.sh — cut and publish a self-signed Murmur release in one step.
#
# For the internal / self-signed flow (no Apple Developer account). It:
#   1. sets the version across package.json, src-tauri/tauri.conf.json, Cargo.toml
#   2. commits the bump and pushes main
#   3. builds a self-signed .dmg + updater artifacts (scripts/release.sh --self-signed)
#   4. creates the GitHub release for the vX.Y.Z tag and uploads:
#        • Murmur.dmg          — stable name, so the landing page's direct-download
#                                link (releases/latest/download/Murmur.dmg) works
#        • Murmur_<v>_aarch64.dmg — the versioned copy (nice on the releases page)
#        • *.app.tar.gz + .sig — the updater artifact (name matches latest.json)
#        • latest.json         — so existing installs auto-update
#
# The tag push triggers the Release workflow, but its publish step is gated on
# Apple signing, so it no-ops here and can't clobber this upload.
#
# The version is worked out automatically from the highest released tag, so you
# never hand-type (and can't fat-finger) a number:
#   ./scripts/publish-release.sh            # patch bump  (0.1.3 → 0.1.4)
#   ./scripts/publish-release.sh --minor    # minor bump  (0.1.3 → 0.2.0)
#   ./scripts/publish-release.sh --major    # major bump  (0.1.3 → 1.0.0)
#   ./scripts/publish-release.sh --dry-run  # print the next version, change nothing
#   ./scripts/publish-release.sh 1.2.3      # pin an explicit version (escape hatch)
# The first release (no tags yet) ships the current manifest version.
#
# Prereqs: gh (authenticated), the "murmur dev" cert (./scripts/setup.sh), and a
# clean working tree on main.

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
BUMP="patch"; EXPLICIT=""; DRYRUN=0
for a in "$@"; do
  case "$a" in
    --major|major) BUMP="major" ;;
    --minor|minor) BUMP="minor" ;;
    --patch|patch) BUMP="patch" ;;
    --dry-run|-n)  DRYRUN=1 ;;
    -h|--help)
      grep -E '^#( |$)' "$0" | sed -E 's/^# ?//'; exit 0 ;;
    v[0-9]*.[0-9]*.[0-9]* | [0-9]*.[0-9]*.[0-9]*) EXPLICIT="${a#v}" ;;
    *) die "unknown arg '$a' — use [--major|--minor|--patch] [--dry-run] [X.Y.Z]" ;;
  esac
done

[ "$(uname)" = "Darwin" ] || die "macOS only."
command -v gh >/dev/null 2>&1 || die "GitHub CLI (gh) not found — https://cli.github.com"
gh auth status >/dev/null 2>&1 || die "gh not authenticated — run: gh auth login"

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
  ok "[dry run] would build a self-signed release and publish $TAG — no changes made."
  exit 0
fi

# --- Remaining preconditions (mutating steps follow) --------------------------
security find-identity -v -p codesigning 2>/dev/null | grep -qF "murmur dev" \
  || die "self-signed 'murmur dev' cert not found — run ./scripts/setup.sh first."

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

# --- 3. Build the self-signed release (dmg + updater artifacts + latest.json) --
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
printf '   Download: https://github.com/letsgetrusty/murmur/releases/latest/download/Murmur.dmg\n'
say "Test the .dmg on another Mac / fresh user to confirm the Gatekeeper + permission flow."
