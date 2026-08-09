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
# Usage:  ./scripts/publish-release.sh 0.1.1
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

# --- Args + preconditions -----------------------------------------------------
VERSION="${1:-}"
[ -n "$VERSION" ] || die "usage: ./scripts/publish-release.sh X.Y.Z"
echo "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' || die "version must be X.Y.Z (got '$VERSION')"
TAG="v$VERSION"

[ "$(uname)" = "Darwin" ] || die "macOS only."
command -v gh >/dev/null 2>&1 || die "GitHub CLI (gh) not found — https://cli.github.com"
gh auth status >/dev/null 2>&1 || die "gh not authenticated — run: gh auth login"
security find-identity -v -p codesigning 2>/dev/null | grep -qF "murmur dev" \
  || die "self-signed 'murmur dev' cert not found — run ./scripts/setup.sh first."

git rev-parse -q --verify "refs/tags/$TAG" >/dev/null 2>&1 && die "tag $TAG already exists locally."
git ls-remote --exit-code --tags origin "$TAG" >/dev/null 2>&1 && die "tag $TAG already exists on origin."
gh release view "$TAG" >/dev/null 2>&1 && die "a GitHub release $TAG already exists."

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
say "Creating GitHub release $TAG…"
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
