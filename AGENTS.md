# AGENTS.md — Voice Tool

macOS desktop app (Tauri v2, Rust backend + webview) combining dictation and
read-aloud TTS, with on-device LLM refinement (Fn+Ctrl) over your dictation.
Personal daily-driver tool; also a Rust-business content artifact.

**Design notes live in `docs/voice-tool-architecture.md` (build order + macOS
gotchas). When this file and the architecture doc disagree, ask.**

---

## Scope — do not violate
- **macOS only.** Do not add Windows/Linux abstractions, cfg gates, or "portable"
  layers. Pick the simplest macOS-native path every time.
- **Single-user tool.** No accounts, sync, multi-user, or telemetry. (Public
  distribution as a signed/notarized DMG is now in scope — see
  `docs/releasing.md` — but the app stays single-user and phone-home-free.)
- **Free & open source under MIT** (`LICENSE`). Keep it MIT-clean: only pull in
  permissively-licensed deps — no GPL/AGPL code statically linked into the
  binary. Ask before adding anything copyleft.
- **Fully on-device.** Everything runs locally — no cloud providers, API keys, or
  network calls except model downloads. Do not re-add cloud backends (Groq,
  OpenRouter, ElevenLabs) without asking.
- Lead with the Rust engine; the webview is just overlay + settings UI.

## Stack — use these, don't substitute without asking
- App: Tauri v2 · hotkeys: `tauri-plugin-global-shortcut`
- Audio in: `cpal` · audio out: AVFoundation via `objc2`
  (`AVSpeechSynthesizer` for native TTS, `AVQueuePlayer` for Kokoro)
- STT: local `whisper-rs` (whisper.cpp, Metal). Model name in `stt_model`.
- Inject/selection: `enigo` + `arboard`
- TTS: native `AVSpeechSynthesizer` via `objc2` — **default**; local neural
  **Kokoro** (`kokoro-en`: ONNX via `ort`, CoreML) optional. Selected by
  `tts_provider` in config. Both on-device. **`kokoro-en` runs with
  `default-features = false`** — its default `g2p-espeak` backend statically
  links GPL-3.0 espeak-ng, incompatible with our MIT license; Kokoro uses its
  cmudict G2P instead. Do not re-enable it (see `THIRD-PARTY-NOTICES.md`).
- Refinement (Fn+Ctrl): local Qwen3 1.7B via `llama-cpp-2` (embedded
  llama.cpp, Metal).
- `reqwest` is used only to download models from HuggingFace.
- async: `tokio` · native FFI: `objc2*`

## Hard rules — these prevent silent, hard-to-debug failures
1. **Register global hotkeys on the main thread** or they silently fail on macOS.
2. **Inject via clipboard paste + simulated `Cmd+V`**, never per-key synthetic
   typing. Restore the clipboard by **watching the change-count, not a fixed
   timer**. Do not preserve binary clipboard contents (avoids double-paste from
   clipboard managers).
3. **Selection capture = simulate `Cmd+C` → read clipboard → restore.** Do not rely
   on the AX selected-text API; it's inconsistent across apps.
4. **Fn hold-to-dictate is a `CGEventTap` (`fn_key.rs`) gated on Accessibility
   alone** — never call `IOHIDRequestAccess`: it records an Input Monitoring
   denial that overrides the Accessibility coupling and silently wedges the tap
   off. Default chords: dictation `Cmd+Shift+D`, read-aloud `Cmd+Shift+R`, cycle
   speed `Cmd+Ctrl+S` (all hold-to-talk except speed). Two chord traps, both
   confirmed on-device: **Option-based** chords (e.g. `Alt+…`) are swallowed by
   macOS special-character input, and **`Cmd+Space`-family** chords are eaten by
   Spotlight/input-source switching — neither fires as a global shortcut, so
   avoid both.
5. **Use a stable signing identity** so Accessibility/Screen Recording grants
   survive rebuilds. Never produce a flow that re-prompts the user to grant
   Accessibility on every build.
6. **Surface microphone-permission failure explicitly** — without it macOS feeds
   empty audio silently and recording appears to work with a flat waveform.

## Backends behind traits
STT, TTS, and the refine LLM each sit behind a trait (`Transcriber`, `Speaker`,
`LlmChat`); all implementations are **on-device**. TTS has two backends selected
by `tts_provider` (native `AVSpeechSynthesizer` default, local neural Kokoro
optional); STT and refine are single-impl but keep the seam. Don't hardcode a
provider at a call site.

## Current status
Phases 0–3 shipped: Fn / chord dictation (on-device Whisper, `whisper-rs` with
Metal) with clipboard-paste injection, Fn+Ctrl LLM refinement (local Qwen3),
read-aloud TTS (native `AVSpeechSynthesizer` by default; read-aloud falls back to
the clipboard when nothing is selected), a settings window (hotkeys, engines,
mic/voice, local usage insights), a first-run onboarding window (Accessibility +
microphone grants and live model-download progress; gated on the
`onboarding_done` config flag, re-openable from the tray "Setup…" item),
auto-update (`tauri-plugin-updater` + minisign-signed artifacts; startup +
tray "Check for Updates…" checks surface an install banner in Settings —
`update.rs`; the endpoint points at the public releases `latest.json`, which
resolves once the repo is public — see `docs/launch-checklist.md`), and SQLite
dictation history. The TTS backend is
chosen via `tts_provider` in config; the local Whisper model (default `small.en`,
name in `stt_model`) auto-downloads to `<app-support>/murmur/models/` on first
run (and via `setup.sh`). Read-aloud can also use local neural **Kokoro**
(`tts_provider = "kokoro"`): its ONNX model + voice packs auto-download to
`…/models/` on first selection (opt-in, ~310 MB, so not fetched by `setup.sh`).
Refinement is a built-in feature: hold **Fn + Ctrl** (the modifier is configurable
in Settings) while dictating and the transcript is cleaned up by the LLM using an
editable prompt before it's pasted (falls back to the raw transcript if the LLM
call fails). It runs through one chat seam (`llm.rs`: `LlmChat` trait +
`transform`, `LocalChat`). The two dictation paths share one recorder lifecycle via
the `DictationMode` enum (Plain / Refine). A voice-macros/commands feature and the
cloud provider backends were prototyped and removed to keep the app focused and
fully on-device. The dictation / refine / read-aloud feature set above is the full
intended scope — no further phases are planned. Build order + macOS gotchas for the
shipped work: `docs/voice-tool-architecture.md` §7.

## Commands
- First-time setup: `./scripts/setup.sh` — toolchain check, `npm install`,
  create + trust the `murmur dev` signing cert, build, and fetch the local
  Whisper model. See the README "Getting Started".
- Dev: `./scripts/dev.sh` — rebuilds the frontend (`npm run build`, since the
  binary embeds `../dist` at compile time — a stale dist means frontend edits
  silently don't ship), builds + signs the Rust binary (stable `murmur dev`
  identity), wraps in `Murmur.app`, launches via `open`. Use this, **not**
  `npm run tauri dev`:
  bare `tauri dev` ad-hoc-signs a shell-launched binary, which breaks the Fn-key
  tap and TCC grant persistence on macOS. Murmur needs exactly ONE permission —
  **Accessibility** (it also authorizes the Fn CGEventTap; no Input Monitoring).
  See `docs/macos-signing-and-permissions.md`. Logs: `~/Library/Logs/murmur.log`.
- Rust check (from `src-tauri/`): `cargo check`
- Rust tests: `cargo test` (includes an IPC contract test that asserts the JS
  `constants.js` names match the Rust events/commands — keep them in sync).
- Frontend tests (from repo root): `npm test` (Vitest — pure helpers + shared
  constants; `npm run test:watch` to iterate).
- Settings-window layout check: `npm run ui-diff` (or `node scripts/ui-diff.mjs
  [pane]`) renders the design reference (`docs/design/reference.html`) and the
  real `frontend/settings.html` at identical size, extracts computed box metrics
  (padding, size, radius, font, gap) for mapped elements from both, and prints a
  mismatch table — exits non-zero on any diff beyond `--tol` (default 1.5px).
  Run it after touching the settings CSS/HTML; it catches the "off by a few px"
  bugs (leaked padding, wrong line-height) that eyeballing two screenshots
  misses. It measures **static** layout only (frontend JS is stripped), so
  JS-populated bits (history rows, live speed segments) need the live window.
- Live-window screenshots: `npm run ui-shot` (or `node scripts/ui-shot.mjs
  [pane…]`) captures the real settings **WKWebView** one PNG per pane into
  `docs/design/shots/` (gitignored). Covers what `ui-diff` can't — WebKit-
  specific rendering, the native traffic lights/window chrome, and everything
  the frontend JS populates (history rows, mic label, live usage, speed
  segments). It builds + signs the debug `.app` via `dev.sh --build-only`, then
  launches it per pane with `MURMUR_UI_SHOT`/`MURMUR_UI_PANE` set — debug-only
  hooks in `show_main_window` deep-link the pane (`window.__LAUNCH_PANE`, no IPC
  command) and write the window's CGWindowID so `screencapture -l` grabs exactly
  that window. Needs Screen Recording permission for the terminal. `--no-build`
  reuses the current bundle.
- Release: **cut every release with `./scripts/publish-release.sh`** — the one
  command for it. It auto-computes the next version from the highest release tag
  (`--minor` / `--major` / an explicit `X.Y.Z` to override; `--dry-run` to
  preview + show which mode), bumps all three manifests **and**
  `package-lock.json`, commits, and pushes `main`. It then **auto-detects how to
  build**:
  - **Apple signing configured** (the `APPLE_CERTIFICATE` GitHub secret exists —
    it now is): the script just pushes the `vX.Y.Z` tag and the CI `release.yml`
    workflow builds the **signed + notarized** DMG + updater artifacts and creates
    the GitHub release. Building locally would race CI, so it doesn't.
  - **Fails closed:** if it can't confirm Apple signing, it **aborts** rather than
    silently self-signing. A self-signed release only happens when you *explicitly*
    pass `--self-signed` (kept as an escape hatch for a lapsed account / a fork) —
    so a `gh` hiccup or a stray run can never ship an un-notarized build.
  - **Do NOT hand-bump the version or create/push a `vX.Y.Z` tag yourself.** The
    script owns both and refuses a tag that already exists, so a manual tag just
    blocks it. A bare tag push *does* now publish (via CI), but only the script
    keeps the version/tag/manifests in lockstep.
  - `./scripts/release.sh` is only the *builder* underneath (`--self-signed` for
    the stable internal identity, `--unsigned` to test bundling, no flag for a
    Developer ID + notarized build — needs certs/creds in env; see
    `docs/releasing.md`). Call it directly only to test a build, not to release.
  - Bundle config: `tauri.conf.json` targets `["app","dmg"]`, hardened-runtime
    `entitlements.plist` — minimal, just `audio-input`, since the binary is fully
    statically linked.
- Lint/format: a pre-commit hook (`.githooks/pre-commit`) runs `cargo fmt --check`
  + `cargo clippy -D warnings` on Rust changes. Keep the crate clean; bypass a WIP
  commit with `git commit --no-verify`.
- CI (`.github/workflows/ci.yml`) gates every PR/push to `main` with two jobs:
  `check` (fmt, clippy, Rust tests, frontend build + `npm test`) and `audit`
  (RustSec + `npm audit`). Make both **required status checks** in branch
  protection so they block merges — see `docs/releasing.md` → "Keeping `main`
  green".
- Dependency audits run in CI (the `audit` job) and on demand:
  - Rust: `./scripts/audit.sh` (RustSec / cargo-audit). Accepted advisories with
    rationale live in `src-tauri/.cargo/audit.toml`; anything not listed there
    fails. Unmaintained-crate *warnings* (the Linux-only gtk-rs GTK3 bindings a
    macOS build never compiles, etc.) don't fail the audit.
  - JS: `./scripts/npm-audit.sh`. Shipped (`dependencies`) deps are zero-
    tolerance; the dev toolchain blocks only CRITICAL. **Accepted, do not
    "fix":** vite/esbuild carry moderate/high advisories (GHSA-67mh-4wv8-2f99
    et al.) fixable only by a breaking vite major bump; they affect only the
    local dev server, never the shipped app (which embeds the pre-built frontend)
    or CI (`vitest run`, no server). Revisit when Tauri's Vite baseline moves.
  - Not in the pre-commit hook: results depend on the advisory DB (network), not
    your edits.

## Conventions
- **Changing the app icon:** the icon is compiled *into* the binary by
  `generate_context!` (not just read from the bundle's `icon.icns` at runtime),
  so after editing `src-tauri/icons/*` you must force a recompile —
  `cargo clean -p murmur && ./scripts/dev.sh` — or the old icon stays baked in
  and no macOS icon-cache clearing will fix it. `tray.png` is a monochrome
  template macOS tints itself; don't color it.
- Module layout per `docs/voice-tool-architecture.md` §4 (`audio.rs`, `stt.rs`,
  `inject.rs`, `selection.rs`, `tts.rs`, `config.rs`).
- Ask before adding any dependency not in the stack list above.
- Keep idle memory low — this exists partly to *not* be an 800MB Electron app.
- Prefer small, reviewable commits per phase milestone.
