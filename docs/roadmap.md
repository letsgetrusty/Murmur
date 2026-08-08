# Roadmap — voice for developers

Exploratory ideas for making Murmur excellent for its primary use: talking to
coding agents (Claude Code, Codex) instead of typing, and listening to their
output instead of reading it. These are candidates, not commitments — the shipped
scope is still what's in [AGENTS.md](../AGENTS.md).

## Thesis

Code and agent output is written **for the eye**. Speech needs it rewritten **for
the ear**. Murmur is uniquely placed to be that translation layer — *the voice
layer for coding agents*. Most items below fall out of this one idea.

## Priority order

1. Deterministic `speakable()` read-aloud transform (A1)
2. LLM "summary" / "read the point" read-aloud mode (A2)
3. Listening controls — skip code block, repeat, pause (B)
4. Coding-agent refine preset + project vocabulary (C)
5. Voice Q&A over a selection (D)

---

## A. Read-aloud: speak output for the ear

The highest-leverage area. Two tiers.

### A1. Deterministic "speakable" pre-processor  · _not started_

Transform selected text before it hits the synthesizer. Pure text processing —
fast, no ML, deterministic, and unit-testable (pairs well with the Vitest / Rust
test suites). Ship as a **"Clean"** read-aloud mode.

- **Code blocks** → spoken stub: ` ```rust … 12 lines``` ` → "Rust code block, 12
  lines." (configurable: read / summarize / skip)
- **Diffs** → "Diff: settings.js, 3 added, 1 removed" instead of `+`/`-` lines
- **Hashes / SHAs / UUIDs / base64** → "commit d4ed9c6" or "a base64 blob" —
  never spell out long hex
- **Paths & identifiers** → `settings.js:42` → "settings.js, line 42"
- **Markdown** → strip `**`, `#`, table pipes so it doesn't read "asterisk
  asterisk"; headers get a pause/emphasis
- **URLs** → domain or "link", not the query string

> Before: "star star fixed the bug star star in src slash commands dot r s colon
> forty-two…"
> After: "Fixed the bug in commands.rs, line 42."

Lives in the TTS path (transform the selection before `speaker.speak()`).

### A2. LLM "summarize for speech" mode  · _not started_

Reuse the on-device Qwen model (`llm.rs` `transform` seam) to condense the
selection before reading. Modes: **Verbatim / Clean / Summary**.

- Example: highlight a long Claude Code turn → "Claude made three changes — fixed
  the mic picker, updated the docs, committed — and is asking whether to push."
- **"Read the point" variant (killer feature):** coding agents almost always end
  with a question or proposed next step ("Want me to push?"). Extract the summary
  **plus the decision being asked for**, read that, skip the code. Maps exactly to
  the Claude Code loop.
- Tradeoff: summary adds latency *before speech starts* — only summarize above a
  length threshold; keep Verbatim instant.

## B. Listening controls  · _not started_

Long output is unusable by ear without navigation.

- Skip the current code block / jump to next paragraph
- Repeat last sentence; pause/resume (Esc-to-stop and speed cycling already exist)
- Subtle audio cues for state (record start/stop, "pasted") for eyes-free use

## C. Smarter dictation *into* the agent  · _not started_

- **Refine profiles.** Build on the editable refine prompt + model picker: add
  switchable presets — **Agent prompt / Commit message / Prose**. The Agent
  profile keeps technical terms, preserves `backticked` paths, formats multi-step
  asks as imperative steps.
- **Project vocabulary / custom dictionary.** Whisper mangles library and
  repo-specific names (Tauri, cpal, Qwen, module names). Feed the refine LLM a
  glossary — ideally **auto-harvested from the current repo's symbols** — to
  correct mis-hearings.
- **Spoken punctuation / symbols** for dictating actual code ("open paren",
  "dash dash", "snake case user id" → `user_id`). Hardest item; a later dedicated
  mode.

## D. Voice Q&A over a selection  · _not started_

Highlight an error or stack trace, hold a key, ask "what does this mean?", hear
the explanation. Turns read-aloud into an on-device voice assistant over whatever
is on screen — a natural extension of the existing local LLM.

---

## Design principles

- **Keep a "read verbatim" override.** Aggressive omission risks dropping
  something wanted — modes must be an explicit, discoverable setting, never a
  silent default.
- **On-device only.** Everything above runs locally (deterministic transforms or
  the bundled Qwen model) — no content leaves the Mac, consistent with the
  privacy scope.
- **Test the transforms.** The deterministic `speakable()` work is table-driven
  input→spoken cases — cover it in the existing test suites.
