# macOS signing & permissions (dev)

How Murmur gets a stable **Accessibility** grant during development, and why the
dev workflow is `scripts/dev.sh` rather than plain `tauri dev`. This is
macOS-specific plumbing; read it before touching the hotkey/injection paths or
the dev launch flow.

## TL;DR — Murmur needs exactly ONE permission: Accessibility

Both things that look like they need separate permissions are covered by the
single **Accessibility** grant:

- **`enigo`** simulating `Cmd+C`/`Cmd+V` for selection + paste-injection → Accessibility.
- **The Fn-key `CGEventTap`** (`fn_key.rs`) → needs event-listen authorization,
  which on macOS is **satisfied by the Accessibility grant**. No separate Input
  Monitoring grant is required. (Verified empirically on macOS 26: with
  Accessibility granted and Input Monitoring left at "unknown",
  `IOHIDCheckAccess` returns *granted* and the tap delivers Fn events.)

This is the same one-permission model Wispr Flow uses.

## The bug that made it look like Input Monitoring was needed

`fn_key.rs` used to call **`IOHIDRequestAccess`** when it saw Input Monitoring
wasn't explicitly granted. That call records an explicit Input Monitoring
**denial** in TCC — and an explicit denial *overrides* the Accessibility
coupling, so `IOHIDCheckAccess` then returns `denied` forever and the tap stays
off. The fix: gate the tap on `AXIsProcessTrusted()` and **never call
`IOHIDRequestAccess`**. If a stale denial is already recorded, clear it once:
`tccutil reset ListenEvent dev.lgr.murmur`.

## Why grants didn't *stick* across rebuilds

Two macOS behaviors, both fixed by the setup below:

1. **`tauri dev` ad-hoc signs the binary.** `signingIdentity` in
   `tauri.conf.json` only applies to `tauri build` bundles, so every `cargo`
   rebuild in dev produced an ad-hoc signature with a changing identity. TCC
   can't pin a grant to an ad-hoc identity across rebuilds. → Sign with the
   stable `murmur dev` identity (below) and launch as a real `.app`.

2. **A self-signed cert that isn't *trusted* forces cdhash-based TCC matching.**
   Even correctly signed with a stable identity, an **untrusted** cert makes TCC
   fall back to matching the exact **cdhash** instead of the (stable) designated
   requirement. The cdhash changes on every code change → TCC treats each
   rebuild as a new app → re-prompts. Marking the cert **trusted for code
   signing** makes TCC match on the designated requirement, which is stable, so
   the grant survives rebuilds.

Note: the app is launched as a real **`.app` via `open`** (not a bare
shell-launched binary) so it is its own TCC "responsible process". A bare binary
launched from a terminal gets its grants attributed to the *terminal*, not
Murmur.

## The setup

### 1. Stable signing identity: `murmur dev`

A self-signed **Code Signing** certificate named `murmur dev` lives in the login
keychain (valid 2026–2036). It is *not* an Apple identity, so
`security find-identity -v -p codesigning` reports "0 valid identities" — that's
expected and fine; `codesign -s "murmur dev"` still works.

Everything is signed with a **fixed identifier** so the designated requirement
is constant across rebuilds:

```
designated => identifier "dev.lgr.murmur" and certificate leaf = H"148a7574576813f28d7f08be80f4882ec9bfba87"
```

**If the cert is ever lost**, recreate it via Keychain Access →
*Certificate Assistant → Create a Certificate*:
- Name: `murmur dev` · Identity Type: Self Signed Root · Certificate Type:
  **Code Signing**.

Then re-trust it (below) and re-grant Accessibility once.

### 2. Trust the cert for code signing (one-time, privileged)

This is what stops the per-rebuild re-prompting. It writes a trusted
code-signing root into the **System** keychain, so it needs **root** (`sudo`)
and must be run **by the user** (the agent's sandbox blocks system-trust
changes). Without `sudo` it fails with
`SecCertificateAddToKeychain: Write permissions error`:

```bash
security find-certificate -c "murmur dev" -p > /tmp/murmurdev.pem
sudo security add-trusted-cert -d -r trustRoot -p codeSign \
  -k /Library/Keychains/System.keychain /tmp/murmurdev.pem
```

Security note: this trusts *any* code signed by `murmur dev`. The private key is
in your keychain and only you can sign with it, so on a personal machine the
risk is low. Reverse with:

```bash
sudo security remove-trusted-cert -d /tmp/murmurdev.pem
```

The clean alternative is a paid **Developer ID** cert (Apple-anchored, no
self-trust needed) — unnecessary for a personal, locally-run tool. See the
comparison in the project history; short version: don't bother unless you
distribute Murmur to other Macs.

### 3. Run via `scripts/dev.sh`, not `tauri dev`

`scripts/dev.sh` on every run:

1. ensures the Vite dev server is up (frontend hot-reload still works),
2. `cargo build` (incremental),
3. wraps the **direct binary** in a signed **`Murmur.app`** (no launcher-script
   indirection — that broke the TCC identity match),
4. signs the bundle with `murmur dev` / `identifier dev.lgr.murmur`,
5. relaunches via `open` and confirms `Fn-key tap installed and enabled`.

Logs: the app **tees its own logs** to `~/Library/Logs/murmur.log` (see the
`Tee` writer in `lib.rs`). This is why the bundle can be a clean direct binary —
we don't need `open --stdout/--stderr` (which fails with `-10810` on a
self-signed app because that flag takes a spawn path Gatekeeper rejects), and
env_logger's stderr does **not** reach the unified log for a bundled app.

```bash
./scripts/dev.sh                 # build, sign, wrap, launch
tail -f ~/Library/Logs/murmur.log
```

Editing the webview → instant (Vite). Editing Rust → re-run the script.

## Granting permissions (first time, or after recreating the cert)

1. Run `./scripts/dev.sh`.
2. System Settings → Privacy & Security → **Accessibility** → enable **Murmur**.
   (That's the *only* permission needed — do NOT touch Input Monitoring.)
3. The first launch after the cert-trust change also pops a **Keychain** prompt
   to read the API keys (the new signature changed the binary hash). Click
   **Always Allow** so it doesn't recur on future rebuilds.
4. Re-run `./scripts/dev.sh`. `~/Library/Logs/murmur.log` should show
   `Accessibility=true … InputMonitoring=0` and `Fn-key tap installed and enabled`.

With the cert trusted, the Accessibility grant + Keychain "Always Allow" both
persist across rebuilds.

## Quick diagnostics

```bash
# What identity is the running app signed with? (want Authority=murmur dev, not adhoc)
codesign -dvvv src-tauri/target/debug/Murmur.app/Contents/MacOS/murmur | grep -iE "Authority|adhoc|Identifier"

# Designated requirement (must be stable across rebuilds)
codesign -d -r- src-tauri/target/debug/Murmur.app/Contents/MacOS/murmur | grep designated

# Is the cert trusted for code signing?
security dump-trust-settings -d 2>/dev/null | grep -iA2 murmur

# Real permission state is logged at startup:
grep "Fn-key: Accessibility" ~/Library/Logs/murmur.log

# Clear a stuck/poisoned grant, then relaunch to re-evaluate
tccutil reset Accessibility dev.lgr.murmur     # the one that matters
tccutil reset ListenEvent dev.lgr.murmur       # clears any stale Input Monitoring denial
```
