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
   off. `Cmd+Shift+Space` is the alternative dictation chord; read-aloud is the
   `Cmd+Shift+R` chord. Avoid Option-based chords (e.g. `Alt+A`) — macOS swallows
   them for special-character input.
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
`update.rs`, endpoint is a TODO placeholder), and SQLite dictation history. The TTS backend is
chosen via `tts_provider` in config; the local Whisper model (default `small.en`,
name in `stt_model`) auto-downloads to `<app-support>/openwispr/models/` on first
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
  create + trust the `openwispr dev` signing cert, build, and fetch the local
  Whisper model. See the README "Getting Started".
- Dev: `./scripts/dev.sh` — rebuilds the frontend (`npm run build`, since the
  binary embeds `../dist` at compile time — a stale dist means frontend edits
  silently don't ship), builds + signs the Rust binary (stable `openwispr dev`
  identity), wraps in `OpenWispr.app`, launches via `open`. Use this, **not**
  `npm run tauri dev`:
  bare `tauri dev` ad-hoc-signs a shell-launched binary, which breaks the Fn-key
  tap and TCC grant persistence on macOS. Open Wispr needs exactly ONE permission —
  **Accessibility** (it also authorizes the Fn CGEventTap; no Input Monitoring).
  See `docs/macos-signing-and-permissions.md`. Logs: `~/Library/Logs/openwispr.log`.
- Rust check (from `src-tauri/`): `cargo check`
- Rust tests: `cargo test` (includes an IPC contract test that asserts the JS
  `constants.js` names match the Rust events/commands — keep them in sync).
- Frontend tests (from repo root): `npm test` (Vitest — pure helpers + shared
  constants; `npm run test:watch` to iterate).
- Release (signed + notarized DMG): `./scripts/release.sh` — needs a Developer
  ID cert + notarization credentials in env; see `docs/releasing.md`. Bump the
  version in `package.json`, `tauri.conf.json`, and `Cargo.toml` first.
  `./scripts/release.sh --unsigned` tests the bundling without a cert. (Bundle
  config: `tauri.conf.json` targets `["app","dmg"]`, hardened-runtime
  `entitlements.plist` — minimal, just `audio-input`, since the binary is fully
  statically linked.)
- Lint/format: a pre-commit hook (`.githooks/pre-commit`) runs `cargo fmt --check`
  + `cargo clippy -D warnings` on Rust changes. Keep the crate clean; bypass a WIP
  commit with `git commit --no-verify`.
- Dependency audit (on demand, not in the hook): `./scripts/audit.sh` (RustSec).

## Conventions
- **Changing the app icon:** the icon is compiled *into* the binary by
  `generate_context!` (not just read from the bundle's `icon.icns` at runtime),
  so after editing `src-tauri/icons/*` you must force a recompile —
  `cargo clean -p openwispr && ./scripts/dev.sh` — or the old icon stays baked in
  and no macOS icon-cache clearing will fix it. `tray.png` is a monochrome
  template macOS tints itself; don't color it.
- Module layout per `docs/voice-tool-architecture.md` §4 (`audio.rs`, `stt.rs`,
  `inject.rs`, `selection.rs`, `tts.rs`, `config.rs`).
- Ask before adding any dependency not in the stack list above.
- Keep idle memory low — this exists partly to *not* be an 800MB Electron app.
- Prefer small, reviewable commits per phase milestone.
