# Privacy Policy

**Open Wispr does not collect, transmit, or store any of your personal data on
any server. Everything happens on your Mac.**

There are no accounts, no analytics, no telemetry, and no tracking of any kind.

## What stays on your device

- **Your voice.** Audio captured for dictation is transcribed locally by the
  on-device Whisper model and then discarded. It is never uploaded anywhere.
- **Your text.** Refinement (the Fn+Ctrl cleanup) and read-aloud run entirely on
  local models. The text you dictate or have read aloud never leaves your Mac.
- **Dictation history.** If enabled, transcripts are stored in a local SQLite
  database under `~/Library/Application Support/openwispr/`. You can search,
  delete individual entries, clear all history, or turn recording off in
  Settings. Nothing is synced or backed up by Open Wispr.
- **Clipboard & selected text** are read/written only transiently to paste
  dictated text and to capture a selection for read-aloud. They are not stored or
  transmitted.

## The only network connections Open Wispr makes

1. **Model downloads (one-time).** On first use it downloads its ML models
   (Whisper, Qwen3, and optionally Kokoro) from Hugging Face. These are ordinary
   file downloads; no personal data is sent.
2. **Update checks.** On launch and when you choose "Check for Updates…", it
   requests a small manifest from the update server to see if a newer version
   exists. This is a plain version check; it sends no personal data.

Beyond these, Open Wispr makes no network requests. It has no backend.

## Permissions it requests (macOS)

- **Microphone** — to hear you when you dictate.
- **Accessibility** — to paste dictated text at your cursor and to use the Fn key
  as the dictation trigger.

These are used solely for the app's core features, locally.

## Changes

If this policy ever changes, the updated version will ship with the app and be
posted in the project repository.

_Questions: <bogdan@letsgetrusty.com>_
