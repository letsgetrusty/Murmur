# macOS signing & permissions (dev)

How Murmur gets stable **Accessibility** and **Input Monitoring** grants during
development, and why the dev workflow is `scripts/dev.sh` rather than plain
`tauri dev`. This is macOS-specific plumbing; read it before touching the
hotkey/injection paths or the dev launch flow.

## The problem

Dictation needs two TCC permissions:

- **Input Monitoring** — the Fn-key event tap (`fn_key.rs`, a `CGEventTap`).
- **Accessibility** — `enigo` simulating `Cmd+C`/`Cmd+V` for selection + inject.

Getting them to *stick* fought three separate macOS behaviors:

1. **`tauri dev` ad-hoc signs the binary.** `signingIdentity` in
   `tauri.conf.json` only applies to `tauri build` bundles, so every `cargo`
   rebuild in dev produced an ad-hoc signature with a changing identity. TCC
   can't pin a grant to an ad-hoc identity across rebuilds.

2. **A bare, shell-launched binary is not its own "responsible process."** For
   Input Monitoring, macOS attributes the grant to the *responsible* process —
   which for a binary launched from a terminal is the **terminal**, not Murmur.
   So toggling "murmur" in System Settings never affected the actual check
   (`IOHIDCheckAccess` kept returning denied). Running as a real **`.app`
   launched via `open`** (LaunchServices) makes Murmur its own responsible
   process, and the grant applies.

3. **A self-signed cert that isn't *trusted* forces cdhash-based TCC matching.**
   Even correctly signed with a stable identity, an **untrusted** cert makes TCC
   fall back to matching the exact **cdhash** instead of the (stable) designated
   requirement. The cdhash changes on every code change → TCC treats each
   rebuild as a new app → re-prompts for permission. Marking the cert
   **trusted for code signing** makes TCC match on the designated requirement,
   which is stable, so the grant survives rebuilds.

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

Then re-trust it (below) and re-grant the two permissions once.

### 2. Trust the cert for code signing (one-time, privileged)

This is what stops the per-rebuild re-prompting. It writes a trusted
code-signing root into the **System** keychain, so it needs admin auth and must
be run **by the user** (the agent's sandbox blocks system-trust changes):

```bash
security find-certificate -c "murmur dev" -p > /tmp/murmurdev.pem
security add-trusted-cert -d -r trustRoot -p codeSign \
  -k /Library/Keychains/System.keychain /tmp/murmurdev.pem
```

Security note: this trusts *any* code signed by `murmur dev`. The private key is
in your keychain and only you can sign with it, so on a personal machine the
risk is low. Reverse with:

```bash
security remove-trusted-cert -d /tmp/murmurdev.pem
```

The clean alternative is a paid **Developer ID** cert (Apple-anchored, no
self-trust needed) — unnecessary for a personal, locally-run tool. See the
comparison in the project history; short version: don't bother unless you
distribute Murmur to other Macs.

### 3. Run via `scripts/dev.sh`, not `tauri dev`

`scripts/dev.sh` on every run:

1. ensures the Vite dev server is up (frontend hot-reload still works),
2. `cargo build` (incremental),
3. wraps the binary in a signed **`Murmur.app`** whose `CFBundleExecutable` is a
   tiny **launcher script** that `exec`s the real binary with stderr/stdout
   redirected to `~/Library/Logs/murmur.log`,
4. signs the nested binary *and* the bundle with `murmur dev` /
   `identifier dev.lgr.murmur`,
5. relaunches via `open` and confirms `Fn-key tap installed and enabled`.

Why the launcher script: `open --stdout/--stderr` fails with `-10810` on a
self-signed app (that flag takes a spawn path Gatekeeper rejects), and
env_logger's stderr does **not** reach the unified log. The launcher `exec`s in
place (same PID) so the LaunchServices-assigned responsible process is preserved
— which is what keeps Input Monitoring working — while still capturing logs.

```bash
./scripts/dev.sh                 # build, sign, wrap, launch
tail -f ~/Library/Logs/murmur.log
```

Editing the webview → instant (Vite). Editing Rust → re-run the script.

## Granting permissions (first time, or after recreating the cert)

1. Run `./scripts/dev.sh`.
2. System Settings → Privacy & Security:
   - **Input Monitoring** → enable **Murmur**
   - **Accessibility** → enable **Murmur**
3. Re-run `./scripts/dev.sh`. `~/Library/Logs/murmur.log` should show
   `Fn-key tap installed and enabled`, and injection should log
   `The application has the permission to simulate input`.

With the cert trusted (step 2 above), these grants persist across rebuilds.

## Quick diagnostics

```bash
# What identity is the running app signed with? (want Authority=murmur dev, not adhoc)
codesign -dvvv src-tauri/target/debug/Murmur.app/Contents/MacOS/murmur-bin | grep -iE "Authority|adhoc|Identifier"

# Designated requirement (must be stable across rebuilds)
codesign -d -r- src-tauri/target/debug/Murmur.app/Contents/MacOS/murmur-bin | grep designated

# Is the cert trusted for code signing?
security dump-trust-settings -d 2>/dev/null | grep -iA2 murmur

# Reset a stuck TCC grant to re-trigger the prompt
tccutil reset ListenEvent dev.lgr.murmur      # Input Monitoring
tccutil reset Accessibility dev.lgr.murmur     # Accessibility
```
