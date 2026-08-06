# CLAUDE.md — Voice Tool

macOS desktop app (Tauri v2, Rust backend + webview) combining dictation,
read-aloud TTS, and knowledge-base–grounded generation on selected text.
Personal daily-driver tool; also a Rust-business content artifact.

**Full design plan lives in `docs/voice-tool-architecture.md`. Implement phase by
phase from it. When this file and the architecture doc disagree, ask.**

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
- TTS: native `AVSpeechSynthesizer` via `objc2` — **default**; ElevenLabs via
  `reqwest` optional (cloud). Selected by `tts_provider` in config.
- Refinement (Fn+Ctrl): OpenRouter via `reqwest`
- Vector store: `lancedb` · embeddings: Voyage/OpenAI via `reqwest` (or `fastembed`)
- Generation (Phase 4, KB): Anthropic Messages API via `reqwest`
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
STT, TTS, embeddings, and generation each sit behind a trait; selection is via
config. Defaults are **on-device**: local Whisper (`whisper-rs`) STT + native
`AVSpeechSynthesizer` TTS, with cloud (Groq / ElevenLabs) as opt-in alternatives.
Generation (Phase 4) is still cloud. Don't hardcode a provider at a call site.

## Current status
Phases 0–3 shipped: Fn / chord dictation (on-device Whisper by default, `whisper-rs`
with Metal; Groq cloud optional) with clipboard-paste injection, Fn+Ctrl LLM
refinement (OpenRouter), read-aloud TTS (native `AVSpeechSynthesizer` by default,
ElevenLabs optional; read-aloud falls back to the clipboard when nothing is
selected), a settings window (hotkeys, API keys, mic/voice, per-provider usage &
cost), and SQLite dictation history. STT/TTS backends are chosen via
`stt_provider`/`tts_provider` in config; the local Whisper model (default
`small.en`) auto-downloads to `<app-support>/murmur/models/` on first run (and via
`setup.sh`).
Also shipped: **voice Macros** (`macros.rs`) — a dedicated chord (default
`Cmd+Shift+M`) records like dictation, but instead of pasting the transcript, an
OpenRouter classifier (`MacroMatcher` trait) maps the spoken phrase to one of the
user's configured macros and pastes that macro's canned response (or nothing when
no macro clearly matches). Managed in the Settings "Macros" tab; macro runs are not
recorded to dictation history. The three dictation paths share one recorder
lifecycle via the `DictationMode` enum (Plain / Refine / Macro).
**Next: Phase 4 — knowledge-base–grounded generation** (`kb.rs` is still a stub):
ingest → embed → LanceDB → top-k → Anthropic Messages over selected text. Phase 5
(screen context) is optional/later. Phase definitions: `docs/voice-tool-architecture.md` §7.

## Commands
- First-time setup: `./scripts/setup.sh` — toolchain check, `npm install`,
  create + trust the `murmur dev` signing cert, build, and store API keys. See the
  README "Getting Started".
- Dev: `./scripts/dev.sh` — builds, signs (stable `murmur dev` identity), wraps
  in `Murmur.app`, launches via `open`. Use this, **not** `npm run tauri dev`:
  bare `tauri dev` ad-hoc-signs a shell-launched binary, which breaks the Fn-key
  tap and TCC grant persistence on macOS. Murmur needs exactly ONE permission —
  **Accessibility** (it also authorizes the Fn CGEventTap; no Input Monitoring).
  See `docs/macos-signing-and-permissions.md`. Logs: `~/Library/Logs/murmur.log`.
- Rust check (from `src-tauri/`): `cargo check`
- Rust tests: `cargo test`
- Release build: `npm run tauri build`
- Lint/format: a pre-commit hook (`.githooks/pre-commit`) runs `cargo fmt --check`
  + `cargo clippy -D warnings` on Rust changes. Keep the crate clean; bypass a WIP
  commit with `git commit --no-verify`.
- Dependency audit (on demand, not in the hook): `./scripts/audit.sh` (RustSec).

## Conventions
- Module layout per `docs/voice-tool-architecture.md` §4 (`audio.rs`, `stt.rs`,
  `inject.rs`, `selection.rs`, `tts.rs`, `kb.rs`, `config.rs`, `secrets.rs`).
- Ask before adding any dependency not in the stack list above.
- Keep idle memory low — this exists partly to *not* be an 800MB Electron app.
- Prefer small, reviewable commits per phase milestone.
