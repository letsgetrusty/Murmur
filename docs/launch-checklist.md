# Launch checklist

Owner-only steps to take Murmur from its current internal/unsigned state to a
public release. The code and CI/CD are already in place — these are the gates that
depend on account/repo settings only the owner can flip (repo visibility, Apple
membership, secrets). Details for each live in [`releasing.md`](releasing.md).

## 1. Go public  ← the key unlock

- [x] **Make the repository public.** ✅ Done. This unlocked auto-update +
      anonymous downloads (the updater fetches `…/releases/latest/download/
      latest.json`, which a private repo served only to authenticated users) and
      branch rulesets (disabled on a private free-plan repo).
- [ ] **Enable the branch ruleset** (now available since the repo is public):
      require
      the `check` and `audit` status checks on `main`, admins bypass. Ready-to-run
      `gh api` command + UI steps in [`releasing.md`](releasing.md) →
      "Keeping `main` green".

## 2. Apple signing (can trail the public switch)

Until these are set, CI produces an **unsigned / ad-hoc** build — a Gatekeeper
"unidentified developer" warning on first open, and (because the ad-hoc signature
changes each build) Accessibility/mic grants that don't survive updates. For
**internal distribution without an Apple account**, build with
`./scripts/release.sh --self-signed` instead: it signs with the stable
`murmur dev` identity, so grants persist across updates (still one Gatekeeper
prompt per Mac). Do the Apple steps below before a wide public launch.

- [ ] **Apple Developer Program** membership + a **Developer ID Application**
      certificate ([`releasing.md`](releasing.md) → one-time prerequisites).
- [ ] Add the `APPLE_*` repo secrets (cert, password, identity, notarization
      creds). The *same* release workflow then signs + notarizes automatically —
      no workflow change.

## 3. Cut the first public release

- [x] Updater endpoint in `tauri.conf.json` already points at the public
      `latest.json` (works once the repo is public).
- [ ] Bump the version in **`package.json`**, **`src-tauri/tauri.conf.json`**, and
      **`src-tauri/Cargo.toml`** together (they must match — CI fails the tag
      otherwise).
- [ ] Push a **`vX.Y.Z`** tag → `release.yml` builds, signs the updater artifact,
      and publishes the GitHub Release with the DMG + `latest.json`
      ([`releasing.md`](releasing.md) → "Releasing via CI").

## 4. Marketing / distribution (optional but recommended)

- [ ] Enable **GitHub Pages** for the landing page (`docs/index.html`) — not yet
      enabled.
- [ ] Record the demo walkthrough and replace the "Demo video coming soon"
      placeholder in `docs/index.html`.
