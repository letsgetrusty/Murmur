# Murmur

A fast, native macOS voice tool: **dictation** and **read-aloud**, with
on-device LLM refinement and voice macros over your speech. Built as
a Tauri v2 app with a Rust engine and a thin webview for the overlay and
settings — deliberately *not* an 800MB Electron app.

Personal daily-driver tool. macOS only, single user, no accounts or telemetry.

---

## What it does

- **Dictation** — hold **Fn** (or press the chord `Cmd+Shift+Space`), speak, and
  release. Audio is transcribed with Groq Whisper and pasted at your cursor.
- **Refined dictation** — hold **Fn+Ctrl** while dictating to run the transcript
  through an LLM (via OpenRouter) that cleans up spoken filler into polished
  text before it's pasted. Falls back to the raw transcript if the call fails,
  so a dropped request never loses your words.
- **Read-aloud** — select text and press **`Cmd+Shift+R`** to hear it, press again
  to stop. With nothing selected it reads the **clipboard** instead — handy for
  mouse-capturing terminal apps where you can't drag-select. Uses ElevenLabs when
  a key is set, falling back to the macOS system voice (`AVSpeechSynthesizer`).
- **Playback speed** — `Alt+Shift+S` cycles read-aloud speed.
- **Overlay pill** — a small status pill appears at the bottom of the screen
  holding the *active window* (not wherever the mouse happens to be) and shows
  recording / transcribing / done state.
- **Settings window** — rebind hotkeys, manage API keys (stored in Keychain, each
  with a link to where you get one), pick microphone and voice, and see
  per-provider usage & cost.
- **Dictation history** — every dictation is saved to a local SQLite database
  (with an enable toggle and retention cap) and browsable in the settings window.

### Hotkeys

| Action | Trigger |
| --- | --- |
| Dictate | Hold **Fn** &nbsp;·&nbsp; or `Cmd+Shift+Space` |
| Refined dictation | Hold **Fn+Ctrl** |
| Read-aloud (toggle) | `Cmd+Shift+R` |
| Cycle read-aloud speed | `Alt+Shift+S` |
| Cancel in-flight dictation | `Esc` |

The chord bindings are configurable in the settings window; the Fn gesture is a
hardware event tap and is fixed.

---

## Requirements

- **macOS only** (11+). The app leans on macOS-native APIs (CoreGraphics event
  taps, AVFoundation, the Accessibility API) with no cross-platform abstraction.
- **Toolchain**: Xcode Command Line Tools (`xcode-select --install`), Rust via
  [rustup](https://rustup.rs) (the pinned version installs automatically from
  `rust-toolchain.toml`), and Node 18+ (`.nvmrc` pins 22).
- **Accessibility permission** — the one grant Murmur needs. It authorizes both
  paste-injection and the Fn-key event tap. Grant it under *System Settings →
  Privacy & Security → Accessibility*. See
  [`docs/macos-signing-and-permissions.md`](docs/macos-signing-and-permissions.md).
- **API keys** (stored in Keychain, never in config or source):
  - Groq — Whisper transcription (**required**)
  - OpenRouter — refined-dictation LLM (optional)
  - ElevenLabs — read-aloud voice (optional; system voice is the fallback)

The repo is private — you'll need collaborator access to clone it.

---

## Getting Started

From a fresh clone, one script does the machine setup:

```sh
git clone <repo-url> && cd murmur
./scripts/setup.sh      # toolchain check · npm install · create+trust the
                        # 'murmur dev' signing cert · build · store API keys
./scripts/dev.sh        # build, sign, wrap in Murmur.app, launch
```

`setup.sh` is idempotent and walks you through it. It handles everything that
*can* be automated; two steps are yours to do once:

1. **Grant Accessibility.** On the first `./scripts/dev.sh`, macOS won't have the
   grant yet — enable **Murmur** under *System Settings → Privacy & Security →
   Accessibility*, then re-run `./scripts/dev.sh`.
2. **API keys.** `setup.sh` offers to store them; you just paste the values. The
   Groq key is required. To (re)set one later:

   ```sh
   ./src-tauri/target/debug/murmur set-key groq        # or openrouter | elevenlabs
   # ...or manage them in the settings window.
   ```

Why the signing dance? A stable, *trusted* self-signed identity keeps the
Accessibility grant from re-prompting on every rebuild — see
[`docs/macos-signing-and-permissions.md`](docs/macos-signing-and-permissions.md)
for the full story.

## Development

```sh
./scripts/dev.sh        # build, sign (stable 'murmur dev' identity), wrap in
                        # Murmur.app, and relaunch. Use this, NOT `tauri dev`.
```

`./scripts/dev.sh` signs with a stable identity so the Accessibility grant
survives rebuilds. A bare `tauri dev` ad-hoc-signs the binary, which breaks the
Fn-key tap and permission persistence.

```sh
cd src-tauri && cargo check     # type-check
cd src-tauri && cargo test      # tests
npm run tauri build             # release build
```

A **pre-commit hook** (enabled by `setup.sh` via `git config core.hooksPath
.githooks`) runs `cargo fmt --check` + `cargo clippy -D warnings` on any commit
that touches Rust — bypass a WIP commit with `git commit --no-verify`. Check
dependencies against the RustSec advisory database on demand with
`./scripts/audit.sh` (it's intentionally not in the hook).

Logs stream to `~/Library/Logs/murmur.log`.

---

## Architecture

Rust engine with each capability behind a trait (STT, TTS, refinement,
macro-matching) so providers are chosen by config, not hardcoded at call sites. The
webview is only the overlay and settings UI.

Modules (`src-tauri/src/`): `audio` (cpal capture) · `stt` (Groq Whisper) ·
`refine` (OpenRouter cleanup) · `tts` (ElevenLabs / AVSpeechSynthesizer) ·
`inject` (clipboard-paste injection) · `selection` (clipboard-based capture) ·
`fn_key` (CGEventTap Fn trigger) · `hotkeys` (global-shortcut chords) ·
`focus` (active-window screen for overlay placement) · `history` (SQLite) ·
`usage` · `config` · `secrets` (Keychain).

The full design lives in [`docs/voice-tool-architecture.md`](docs/voice-tool-architecture.md).

### Stack

Tauri v2 · `tauri-plugin-global-shortcut` · `cpal` (audio in) · `rodio` (audio
out) · `reqwest` (cloud APIs) · `enigo` + `arboard` (inject/selection) ·
`objc2*` (native macOS FFI) · `rusqlite` (history) · `keyring` (secrets) ·
`tokio` (async).
