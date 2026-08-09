# Murmur

**Voice to text, for developers.** Fast, native macOS dictation and read-aloud,
with one-key on-device cleanup of what you say. 100% on your Mac — no cloud, no
accounts, no API keys.

## ⬇ Download

### **[Download Murmur for macOS »](https://github.com/letsgetrusty/murmur/releases/latest)**

Open `Murmur_<version>_aarch64.dmg` and drag **Murmur** to Applications.

**Requires macOS 11+ on Apple Silicon (M1 or later).**

> Not notarized yet, so macOS shows an "unidentified developer" warning the first
> time. Right-click Murmur → **Open** → **Open** (or System Settings → Privacy &
> Security → **Open Anyway**) — once only.

On first launch Murmur walks you through the one permission it needs
(**Accessibility**) plus your **microphone**. New versions install with a
one-click "Restart to update" — nothing happens behind your back.

## What it does

- **Dictate anywhere** — hold **Fn**, speak, release. On-device Whisper types it
  at your cursor, in any app.
- **Clean it up** — hold **Fn + Ctrl** to run your words through a local LLM
  (fix grammar and punctuation, drop filler) before it's pasted.
- **Read aloud** — **⌘⇧R** speaks the selected text (or the clipboard), in the
  built-in macOS voice or a higher-quality neural voice.
- **Private by design** — Whisper, the LLM, and speech all run on your Mac; your
  voice never leaves it. See [PRIVACY.md](PRIVACY.md).

### Shortcuts

| Action | Keys |
| --- | --- |
| Dictate | Hold **Fn** &nbsp;·&nbsp; or **⌘⇧D** |
| Dictate & refine | Hold **Fn + Ctrl** |
| Read aloud | **⌘⇧R** |
| Cycle read-aloud speed | **⌘⌃S** |
| Cancel dictation | **Esc** |

Shortcuts are rebindable in Settings; the **Fn** gesture is fixed. Dictation
history and usage insights live in the settings window.

## Build from source

```sh
git clone https://github.com/letsgetrusty/murmur && cd murmur
./scripts/setup.sh   # toolchain check, deps, signing cert, first build + model
./scripts/dev.sh     # build, sign, launch — use this, NOT `tauri dev`
```

`dev.sh` signs with a stable self-signed identity so the Accessibility grant
survives rebuilds (a bare `tauri dev` breaks the Fn tap). Grant **Accessibility**
to Murmur on first run, then re-run `dev.sh`.

```sh
cd src-tauri && cargo test          # Rust tests
npm test                            # frontend tests
```

A pre-commit hook runs `cargo fmt --check` + `cargo clippy -D warnings` on Rust
changes. Logs stream to `~/Library/Logs/murmur.log`.

Deeper docs: [architecture](docs/voice-tool-architecture.md) ·
[signing & permissions](docs/macos-signing-and-permissions.md) ·
[releasing](docs/releasing.md).

## License

MIT — see [LICENSE](LICENSE). Third-party components and models are attributed in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) (all permissive; no copyleft
linked into the binary).
