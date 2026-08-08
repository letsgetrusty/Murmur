# Releasing Open Wispr (signed + notarized DMG)

> For the end-to-end path to a public release (repo visibility, signing, first
> tag, landing page), see [`launch-checklist.md`](launch-checklist.md). This doc
> is the deep-dive on the signing + release mechanics it references.

How to turn the code into a `.dmg` a stranger can download and run without
Gatekeeper blocking it. This is the **distribution** build. It's separate from
local development — for that, keep using `./scripts/dev.sh` with the self-signed
`openwispr dev` identity (see `docs/macos-signing-and-permissions.md`).

Two different signatures are involved, don't confuse them:

| | `dev.sh` (local) | `release.sh` (shipping) |
|---|---|---|
| Identity | self-signed `openwispr dev` | **Developer ID Application** (Apple-issued) |
| Notarized | no | **yes** |
| Runs on other Macs | no (Gatekeeper blocks) | yes |
| Cost | free | Apple Developer Program ($99/yr) |

---

## One-time prerequisites

### 1. Apple Developer Program membership

$99/year at <https://developer.apple.com/programs/>. Required — a Developer ID
certificate can only be issued to a paid account. Note your **Team ID** (a
10-char code, e.g. `AB12CD34EF`) from <https://developer.apple.com/account> →
Membership.

### 2. A "Developer ID Application" certificate

This is the cert that signs apps for distribution *outside* the App Store.

- Xcode → Settings → Accounts → your team → **Manage Certificates** → `+` →
  **Developer ID Application**. It's created and installed into your login
  keychain automatically.
- Or via the portal: <https://developer.apple.com/account/resources/certificates>
  → `+` → Developer ID Application → follow the CSR steps → download → double-click
  to install.

Confirm it's present:

```sh
security find-identity -v -p codesigning | grep "Developer ID Application"
# → "Developer ID Application: Your Name (TEAMID)"
```

The full string (including `(TEAMID)`) is your `APPLE_SIGNING_IDENTITY`.

### 3. A notarization credential

Notarization = Apple scans the signed app and issues a ticket so Gatekeeper
trusts it. Pick **one** of these:

**(a) App Store Connect API key — recommended (works headless / in CI):**

- <https://appstoreconnect.apple.com/access/integrations/api> → generate a key
  with the **Developer** role. Download the `.p8` file (you can only download it
  once — keep it safe).
- You get: the **Key ID**, the **Issuer ID**, and the `.p8` file path.

**(b) Apple ID + app-specific password — simplest to set up:**

- <https://account.apple.com> → Sign-In & Security → **App-Specific Passwords** →
  generate one for "Open Wispr notarization".
- You get: your Apple ID email, that password, and your Team ID.

---

## Releasing via CI (recommended)

`.github/workflows/release.yml` builds + publishes automatically when you push a
**version tag**. Merges to `main` do nothing on their own — releasing is a
deliberate act, so you control exactly when users get an update.

**To cut a release:**

1. Bump the version in **all three**: `package.json`, `src-tauri/tauri.conf.json`,
   `src-tauri/Cargo.toml`. Commit + merge to `main`.
2. Tag it and push the tag:
   ```sh
   git tag v0.2.0 && git push origin v0.2.0
   ```

CI (an Apple-Silicon runner) then builds, signs the updater artifact, and creates
a **GitHub Release** for the tag with the `.dmg` + `latest.json` + `.app.tar.gz`
attached. The workflow first fails fast if the tag doesn't match the app version.

### Secrets

Repo → Settings → Secrets and variables → Actions:

| Secret | When | Purpose |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | **now** | Updater (minisign) signing — the contents of `~/.openwispr/updater.key`. Without it, auto-update won't work. Our key has no password, so `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` can be left unset. |
| `APPLE_CERTIFICATE` + `APPLE_CERTIFICATE_PASSWORD` + `APPLE_SIGNING_IDENTITY` | later | Developer ID signing (base64 of the `.p12`, its password, and the identity string). |
| `APPLE_ID` + `APPLE_PASSWORD` + `APPLE_TEAM_ID` | later | Notarization (or use the App Store Connect API-key trio). |

**Until the Apple secrets are set, CI still runs** — it just produces an
**ad-hoc / unsigned** build. It installs, but downloaders hit a Gatekeeper
"unidentified developer" warning (right-click → Open). Add the Apple secrets and
the *same* workflow notarizes — no other change. Fine for internal/testing;
notarize before a public launch.

> **Public-repo requirement:** the updater endpoint is
> `…/releases/latest/download/latest.json`. GitHub serves release assets of a
> **private** repo only to authenticated users, so auto-update (and public
> downloads) start working once the repo is public.

---

## Keeping `main` green (branch protection)

`.github/workflows/ci.yml` runs on every PR and push to `main` and defines two
jobs:

- **`check`** (macOS) — `cargo fmt --check`, `cargo clippy -D warnings`, the Rust
  test suite, and the frontend build + `npm test` (Vitest).
- **`audit`** (Ubuntu) — RustSec (`cargo-audit`) and `npm audit`, so a
  known-vulnerable dependency can't land on `main`. Only the advisories
  explicitly accepted in `src-tauri/.cargo/audit.toml` and `scripts/npm-audit.sh`
  are tolerated; anything new fails the job. (See AGENTS.md → Commands for what's
  accepted and why.)

These jobs **report** status but don't **block** a merge until you mark them
**required** in branch protection — a repo setting, not something the workflow
can enable itself. Do it once the repo is set up:

Use a **branch ruleset** (GitHub's current system; "classic branch protection"
is legacy).

> **Requires a public repo (or GitHub Pro).** Rulesets and branch protection are
> disabled on a **private** free-plan repo — creating one returns
> `403 "Upgrade to GitHub Pro or make this repository public"` (in the UI and via
> the API alike). So this becomes available at the same moment the repo goes
> public (see the public-repo note above). Do it as part of going public.

- **UI:** Repo → Settings → Rules → **Rulesets** → **New branch ruleset** →
  name it `main`, set **Enforcement status: Active**, add a **Target** →
  **Include default branch** → under **Rules** enable **Require status checks to
  pass** and add **`check`** and **`audit`** (also tick **Require branches to be
  up to date before merging**) → **Create**. (Optionally enable **Require a pull
  request before merging** for a PR-based flow.)
- **CLI:**
  ```sh
  gh api -X POST repos/letsgetrusty/OpenWispr/rulesets \
    --input - <<'JSON'
  {
    "name": "main",
    "target": "branch",
    "enforcement": "active",
    "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
    "rules": [
      {
        "type": "required_status_checks",
        "parameters": {
          "strict_required_status_checks_policy": true,
          "required_status_checks": [
            { "context": "check" },
            { "context": "audit" }
          ]
        }
      }
    ]
  }
  JSON
  ```

Until then, treat a red CI run as a stop sign by convention. The local
pre-commit hook (`fmt` + `clippy`) is a first line of defense but is easy to
bypass (`--no-verify`) and doesn't run the tests or audits — CI is the backstop.

---

## Releasing locally (manual fallback)

The CI path above is preferred; use `./scripts/release.sh` for a local build.
Export the credentials (put them in a **gitignored** `.env` and `source` it — do
not commit them):

```sh
export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"

# Notarization — option (a), API key:
export APPLE_API_ISSUER="xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
export APPLE_API_KEY="XXXXXXXXXX"                 # the Key ID
export APPLE_API_KEY_PATH="$HOME/keys/AuthKey_XXXXXXXXXX.p8"

# …or option (b), Apple ID:
# export APPLE_ID="you@example.com"
# export APPLE_PASSWORD="abcd-efgh-ijkl-mnop"      # app-specific password
# export APPLE_TEAM_ID="TEAMID"
```

Then:

```sh
./scripts/release.sh
```

The script validates your env, then runs `tauri build`, which:

1. builds the frontend (`npm run build`),
2. compiles a **release** binary (`cargo build --release` — the first cold build
   compiles whisper.cpp + llama.cpp + onnxruntime and takes **10–20 min**),
3. signs it with your Developer ID under the **hardened runtime** +
   `src-tauri/entitlements.plist`,
4. notarizes with Apple and **staples** the ticket,
5. packages `OpenWispr_<version>_aarch64.dmg`.

Output lands in `src-tauri/target/release/bundle/dmg/`. The script finishes by
running `spctl` and `stapler validate` on the result.

### Bump the version first

Every release needs a higher version (also required once the auto-updater
exists). Update it in **all three**:

- `package.json` → `version`
- `src-tauri/tauri.conf.json` → `version`
- `src-tauri/Cargo.toml` → `version`

### Pipeline test without a certificate

To confirm the bundling works before you have the Apple account:

```sh
./scripts/release.sh --unsigned
```

Produces an ad-hoc-signed `.dmg`. It builds and mounts, but **won't run on other
Macs** — it only proves the compile + packaging path.

---

## Verifying the result

```sh
APP=src-tauri/target/release/bundle/macos/OpenWispr.app

# Gatekeeper verdict — want "accepted" and "source=Notarized Developer ID"
spctl -a -vvv --type execute "$APP"

# Hardened runtime + Developer ID authority
codesign -dvvv "$APP" 2>&1 | grep -E "Authority|flags|runtime"

# Notarization ticket is stapled (works offline)
xcrun stapler validate "$APP"
```

The only conclusive test is opening the `.dmg` on a **different Mac** (or a fresh
user account that has never run the dev build) — that's the real first-time-user
Gatekeeper path.

---

## Auto-updates

Open Wispr ships with `tauri-plugin-updater`. On launch (and from the tray's
**Check for Updates…**) it fetches a manifest, and if a newer, validly-signed
release exists, the Settings window shows an **Install & Restart** banner.

Two things make this work:

### The updater signing key (separate from Apple signing)

Update archives are signed with a **minisign** keypair — a different trust root
from your Developer ID. The app only installs an update whose signature matches
the **public key baked into `tauri.conf.json` → `plugins.updater.pubkey`**. This
proves the update came from you *before* macOS ever evaluates the new bundle.

A keypair already exists at `~/.openwispr/updater.key` (public half in the
config). It has no password. To rotate it (e.g. to set a password you alone
control), regenerate and update the config:

```sh
npx tauri signer generate -w ~/.openwispr/updater.key -f
# copy the printed public key into tauri.conf.json → plugins.updater.pubkey
```

**Guard the private key like the Apple cert** — losing it means you can't ship
updates users will accept (they'd have to re-download manually). `release.sh`
reads it from `~/.openwispr/updater.key` (override with `TAURI_SIGNING_PRIVATE_KEY`
/ `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`). It's `.gitignore`d — never commit it.

### The manifest + hosting

The updater endpoint is **GitHub Releases**:
`tauri.conf.json` → `plugins.updater.endpoints` →
`https://github.com/letsgetrusty/OpenWispr/releases/latest/download/latest.json`.

- **Via CI (normal path):** tauri-action generates `latest.json` + the signed
  `.app.tar.gz` and attaches them to the release automatically. Nothing to do.
- **Via `release.sh` (manual):** it also writes `…/bundle/latest.json` pointing
  at the `vN.N.N` release assets — create that GitHub release and upload both
  `OpenWispr.app.tar.gz` and `latest.json` to it.

The app fetches the manifest, compares its version, and installs if newer. As
noted above, release assets are only publicly fetchable once the repo is public.

> This build targets Apple Silicon → the manifest's platform key is
> `darwin-aarch64`. Add `darwin-x86_64` / `darwin-universal` entries if you ever
> ship those.

## Notes / gotchas

- **Why not sandboxed?** Open Wispr needs the Accessibility API (paste + the Fn
  tap) and full clipboard access, which the App Sandbox forbids. So it ships with
  the hardened runtime but no sandbox — that's fine for Developer ID distribution
  (only the App Store requires the sandbox).
- **Entitlements are deliberately minimal** (`entitlements.plist`): just
  `audio-input`, because everything is statically linked into one binary (no
  bundled dylibs → no library-validation exception needed). If a hardened-runtime
  crash ever appears *inside* ML inference, the usual fix is adding
  `com.apple.security.cs.allow-jit`; we don't ship it preemptively.
- **Architecture:** this builds for the host arch (Apple Silicon → `aarch64`). A
  universal binary would need `tauri build --target universal-apple-darwin` and
  an Intel toolchain; skip it unless you actually need Intel support.
- **Keep the signing identity stable** across releases so users' Accessibility
  grants survive updates (same principle as the dev cert — see the CLAUDE.md
  hard rules).
- **Don't run a release build next to a running dev app.** A release build
  registers extra `ai.openwispr.app` bundles with LaunchServices — the temporary
  `create-dmg` volume (`/Volumes/dmg.*`) *and* the release `.app` itself. With the
  dev build (or an installed copy) also around, macOS sees duplicate bundle ids
  and aborts the app (`SIGABRT`) — it looks like a random crash loop.
  `release.sh` now unregisters + ejects both after building to prevent this, but
  if you ever hit it: quit all instances, `lsregister -u` the extra paths, eject
  `/Volumes/dmg.*`, remove extra `OpenWispr.app` copies, and rebuild so exactly
  one bundle is registered. Only affects dev machines — end users have a single
  installed copy that updates in place. (Verified via the local updater E2E test:
  a v0.1.0 app pre-downloads + verifies + installs v0.1.1 and relaunches; the
  crash was only this dev-side bundle collision, not the updater.)
