# Launch checklist

Owner-only steps to take Open Wispr from its current internal/unsigned state to a
public release. The code and CI/CD are already in place — these are the gates that
depend on account/repo settings only the owner can flip (repo visibility, Apple
membership, secrets). Details for each live in [`releasing.md`](releasing.md).

## 1. Go public  ← the key unlock

- [ ] **Make the repository public.** This single switch unlocks three things at
      once, so several items below are blocked until it's done:
  - **Auto-update + public downloads** — the updater fetches
    `…/releases/latest/download/latest.json`; GitHub serves a *private* repo's
    release assets only to authenticated users, so auto-update and the README /
    landing-page download links only work once the repo is public.
  - **Branch rulesets / required status checks** — disabled on a private
    free-plan repo (creating one returns `403 "Upgrade to GitHub Pro or make this
    repository public"`).
- [ ] **Enable the branch ruleset** (do this right after going public): require
      the `check` and `audit` status checks on `main`, admins bypass. Ready-to-run
      `gh api` command + UI steps in [`releasing.md`](releasing.md) →
      "Keeping `main` green".

## 2. Apple signing (can trail the public switch)

Until these are set, CI still produces a working build — just **unsigned /
ad-hoc**, so downloaders hit a Gatekeeper "unidentified developer" warning
(right-click → Open). Fine for internal testing; do before a wide public launch.

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
