# CLAUDE.md — Voice Tool

macOS desktop app (Tauri v2, Rust backend + webview) combining dictation and
read-aloud TTS, with on-device LLM refinement (Fn+Ctrl) over your dictation.
Personal daily-driver tool; also a Rust-business content artifact.

**Design notes live in `docs/voice-tool-architecture.md` (build order + macOS
gotchas). When this file and the architecture doc disagree, ask.**

---

## Scope — do not violate
- **macOS only.** Do not add Windows/Linux abstractions, cfg gates, or "portable"
  layers. Pick the simplest macOS-native path every time.
- **Personal tool.** No accounts, sync, multi-user, telemetry, or distribution
  packaging.
- Lead with the Rust engine; the webview is just overlay + settings UI.

## Stack — use these, don't substitute without asking
- App: Tauri v2 · hotkeys: `tauri-plugin-global-shortcut`
- Audio in: `cpal` · audio out: `rodio`
- STT: local `whisper-rs` (whisper.cpp, Metal) — **default**; Groq/OpenAI Whisper
  via `reqwest` optional (cloud). Selected by `stt_provider` in config.
- Inject/selection: `enigo` + `arboard`
- TTS: native `AVSpeechSynthesizer` via `objc2` — **default**; local neural
  **Kokoro** (`kokoro-en`: ONNX via `ort` + bundled espeak-ng, CoreML) and
  ElevenLabs (cloud, `reqwest`) optional. Selected by `tts_provider` in config.
- Refinement (Fn+Ctrl): local Qwen3 1.7B via `llama-cpp-2` (embedded
  llama.cpp, Metal) — **default**; OpenRouter via `reqwest` optional (cloud).
  Selected by `llm_provider` in config.
- Secrets: `keyring` · async: `tokio` · native FFI: `objc2*`

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
6. **Secrets go in Keychain via `keyring`.** Never write API keys to config files,
   source, or logs.
7. **Surface microphone-permission failure explicitly** — without it macOS feeds
   empty audio silently and recording appears to work with a flat waveform.

## Backends behind traits
STT, TTS, and the refine LLM each sit behind a trait; selection is via config.
Defaults are **on-device**: local Whisper (`whisper-rs`) STT + native
`AVSpeechSynthesizer` TTS, with cloud (Groq / ElevenLabs) as opt-in alternatives.
Refine defaults to a shared embedded local LLM (Qwen3 via llama.cpp), OpenRouter
optional. Don't hardcode a provider at a call site.

## Current status
Phases 0–3 shipped: Fn / chord dictation (on-device Whisper by default, `whisper-rs`
with Metal; Groq cloud optional) with clipboard-paste injection, Fn+Ctrl LLM
refinement (OpenRouter), read-aloud TTS (native `AVSpeechSynthesizer` by default,
ElevenLabs optional; read-aloud falls back to the clipboard when nothing is
selected), a settings window (hotkeys, API keys, mic/voice, per-provider usage &
cost), and SQLite dictation history. STT/TTS backends are chosen via
`stt_provider`/`tts_provider` in config; the local Whisper model (default
`small.en`) auto-downloads to `<app-support>/openwispr/models/` on first run (and via
`setup.sh`). Read-aloud can also use local neural **Kokoro** (`tts_provider =
"kokoro"`): its ONNX model + voice packs auto-download to `…/models/` on first
selection (opt-in, ~310 MB, so not fetched by `setup.sh`).
Refinement is a built-in feature: hold **Fn + Ctrl** (the modifier is configurable
in Settings) while dictating and the transcript is cleaned up by the LLM using an
editable prompt before it's pasted (falls back to the raw transcript if the LLM
call fails). It runs through one chat seam (`llm.rs`: `LlmChat` trait +
`transform`, `LocalChat`/`OpenRouterChat`). The two dictation paths share one
recorder lifecycle via the `DictationMode` enum (Plain / Refine). A
voice-macros/commands feature was prototyped and removed to keep the app focused.
The dictation / refine / read-aloud feature set above is the full intended scope —
no further phases are planned. Build order + macOS gotchas for the shipped work:
`docs/voice-tool-architecture.md` §7.

## Commands
- First-time setup: `./scripts/setup.sh` — toolchain check, `npm install`,
  create + trust the `openwispr dev` signing cert, build, and store API keys. See the
  README "Getting Started".
- Dev: `./scripts/dev.sh` — builds, signs (stable `openwispr dev` identity), wraps
  in `OpenWispr.app`, launches via `open`. Use this, **not** `npm run tauri dev`:
  bare `tauri dev` ad-hoc-signs a shell-launched binary, which breaks the Fn-key
  tap and TCC grant persistence on macOS. Open Wispr needs exactly ONE permission —
  **Accessibility** (it also authorizes the Fn CGEventTap; no Input Monitoring).
  See `docs/macos-signing-and-permissions.md`. Logs: `~/Library/Logs/openwispr.log`.
- Rust check (from `src-tauri/`): `cargo check`
- Rust tests: `cargo test`
- Release build: `npm run tauri build`
- Lint/format: a pre-commit hook (`.githooks/pre-commit`) runs `cargo fmt --check`
  + `cargo clippy -D warnings` on Rust changes. Keep the crate clean; bypass a WIP
  commit with `git commit --no-verify`.
- Dependency audit (on demand, not in the hook): `./scripts/audit.sh` (RustSec).

## Conventions
- Module layout per `docs/voice-tool-architecture.md` §4 (`audio.rs`, `stt.rs`,
  `inject.rs`, `selection.rs`, `tts.rs`, `config.rs`, `secrets.rs`).
- Ask before adding any dependency not in the stack list above.
- Keep idle memory low — this exists partly to *not* be an 800MB Electron app.
- Prefer small, reviewable commits per phase milestone.
