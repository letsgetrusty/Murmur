# Murmur

**Voice to text, for developers.** Fast, native macOS dictation and read-aloud,
with one-key on-device cleanup of what you say. 100% on your Mac — no cloud, no
accounts, no API keys.

## ⬇ Download

### **[Download Murmur for macOS »](https://github.com/letsgetrusty/Murmur/releases/latest)**

Open the `.dmg`, drag **Murmur** to Applications, and launch it. Signed and
notarized by Apple, so there are no Gatekeeper warnings. Updates install with one
click. **Requires macOS 11+ on Apple Silicon (M1 or later).**

## What it does

- **Dictate anywhere** — hold **Fn**, speak, release. On-device Whisper types it
  at your cursor, in any app.
- **Clean it up** — hold **Fn + Ctrl** to run your words through a local LLM
  (fix grammar and punctuation, drop filler) before it's pasted.
- **Read aloud** — **⌘⇧R** speaks the selected text (or the clipboard) in a
  high-quality on-device voice.
- **Private by design** — Whisper, the LLM, and speech all run on your Mac; your
  voice never leaves it. See [PRIVACY.md](PRIVACY.md).

## Build from source

```sh
git clone https://github.com/letsgetrusty/Murmur && cd Murmur
./scripts/setup.sh   # toolchain, deps, signing cert, first build + model
./scripts/dev.sh     # build, sign, launch — use this, NOT `tauri dev`
```

```sh
cd src-tauri && cargo test   # Rust tests
npm test                     # frontend tests (from repo root)
```


Grant **Accessibility** on first run, then re-run `dev.sh` — it uses a stable
self-signed identity so the grant survives rebuilds (a bare `tauri dev` breaks
the Fn tap). Logs stream to `~/Library/Logs/murmur.log`.

Deeper docs: [architecture](docs/voice-tool-architecture.md) ·
[signing & permissions](docs/macos-signing-and-permissions.md) ·
[releasing](docs/releasing.md).

## License

MIT — see [LICENSE](LICENSE). Third-party components and models are attributed in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) (all permissive).
