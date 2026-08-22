# Testing — keeping the core experience from regressing

Murmur's core is dictation (STT) and read-aloud (TTS). Most regressions we've hit
there were **latency, timing, or audio-quality** bugs — a model that throttles,
a synth that falls back to CPU, a start cue that delays the mic so the first
words are lost. None of those are catchable by ordinary unit tests, because they
need real Metal, the actual models, and live audio hardware — none of which exist
on CI runners. So testing is **two layers**.

## Layer 1 — unit tests (deterministic logic, runs in CI)

Pure functions with no hardware/model dependency, run by `cargo test` on every
push (the `check` job in `.github/workflows/ci.yml`) and locally:

```
cargo test --manifest-path src-tauri/Cargo.toml
npm test          # frontend helpers + the shared-constants IPC contract
```

What's covered (extend these when you touch the logic):

- **Text → chunks** (`split_for_tts`): sentence-only breaks, the fast first-chunk
  comma rule, word preservation, no mid-word breaks, adversarial inputs.
- **Edge-silence trim** (`trim_silence`): removes Kokoro's padding, keeps a guard
  so onsets/tails are never clipped, returns all-silence unchanged.
- **Chunk-gap policy**: last chunk seamless, sentence vs. soft-break gaps.
- **Transcript cleanup** (`strip_nonspeech`): drops non-speech placeholders,
  never eats real words (e.g. keeps "(net)").
- **Config** defaults/round-trip, **history** store, and the **IPC contract**
  test that asserts `constants.js` names match the Rust events/commands.

Rule of thumb: if a bug can be reproduced with an in-memory input and no audio
device, it belongs here.

## Layer 2 — performance gate (local, pre-release)

`scripts/bench.sh` runs the three benchmark examples against fixed thresholds and
**fails** if a core path regressed. It can't run in CI (needs Metal + models +
audio), so it's a **pre-release gate**: `publish-release.sh` runs it before
tagging and **aborts the release** on a breach — right before the code would
reach auto-updating users.

```
scripts/bench.sh              # build the release benches + run the gate
scripts/bench.sh --no-build   # reuse already-built benches
```

The benches (also useful standalone for diagnosis — see `src-tauri/examples/`):

| Bench | Measures | Gate |
|---|---|---|
| `bench_stt` | Whisper realtime factor (per model) | `small.en` ≥ 10× (caught `medium.en` throttling to ~2.7×) |
| `bench_tts` | Kokoro synth realtime factor | ≥ 2× (must beat 1× or playback stalls) |
| `bench_start` | mic-ready latency after Fn press | ≤ 250ms (caught the cue-before-mic delay that lost first words) |

A bench whose model/hardware is **missing SKIPS** (a missing model isn't a
regression); a bench that **runs and is below threshold FAILS**. Thresholds sit
well below measured-healthy on an M4 (STT ~26×, TTS ~4.5×, mic-ready ~75ms) so
normal variance never trips them — override per-machine via env
(`STT_MIN_REALTIME`, `TTS_MIN_REALTIME`, `MIC_MAX_MS`).

Bypass the gate only when you can't run it (no models on the machine):

```
./scripts/publish-release.sh --skip-bench
```

### Why these three thresholds catch what they catch

- **STT throttle** — `small.en` holds 25×+ but `medium.en` thermally throttled to
  ~2.7× under sustained load. A 10× floor passes the shipped default and fails a
  slow model or a decode regression.
- **TTS below realtime** — if synth drops under 1× (CoreML falling back to CPU, or
  a heavier model), read-aloud stalls mid-sentence. A 2× floor keeps a safety
  margin.
- **Slow mic start** — the mic itself goes live in ~75ms; the "snappiness
  regressed" bug came from a cold output-device wake (up to ~0.5–1s) sitting in
  front of the mic open. The fix keeps the cue off the mic path (detached
  thread); the ≤250ms gate fails if anything slow gets back in front of capture.

## What isn't covered (and why)

- **Transcription accuracy / voice quality** — subjective and model-dependent;
  guarded by the `bench_stt` ignored test (`MURMUR_TEST_WAV`) for spot checks and
  by listening during `scripts/dev.sh`.
- **Full end-to-end (key → paste, selection → speech)** — needs Accessibility +
  a focused app; exercised manually and via `scripts/sim-onboarding.sh` for the
  first-run flow. A scripted e2e smoke is a possible Layer 3 if these paths start
  regressing.
