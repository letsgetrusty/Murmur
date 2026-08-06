# Releasing Open Wispr (signed + notarized DMG)

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

## Doing a release

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

### The manifest + hosting  ⚠️ TODO before first release

The updater endpoint is currently a **placeholder** in two places that must
point at the same real host:

- `tauri.conf.json` → `plugins.updater.endpoints`
- `scripts/release.sh` → `ENDPOINT_HOST`

Each `release.sh` run (with `createUpdaterArtifacts` on) produces:

- `…/bundle/macos/OpenWispr.app.tar.gz` + `.sig` — the update archive,
- `…/bundle/latest.json` — the manifest (version, notes, pub_date, and per-arch
  `{ signature, url }`).

To publish a release: set the real host in both places above, then upload the
`.app.tar.gz` **and** `latest.json` to it so `endpoints` resolves to that JSON.
The app compares its version to the manifest's and installs if newer.

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
