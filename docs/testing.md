# Testing — keeping the core experience from regressing

Murmur's core is dictation (STT) and read-aloud (TTS). Most regressions we've hit
there were **latency, timing, or audio-quality** bugs — a model that throttles,
a synth that falls back to CPU, a start cue that delays the mic so the first
words are lost. None of those are catchable by ordinary unit tests, because they
need controlled hardware, the actual models, and sometimes live audio devices.
CI runs a small real-audio smoke check, but shared runners do not provide stable
performance measurements. Testing is **two layers**.

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

## Layer 2 — local performance and quality gate

See [Local speech benchmarks](benchmarks.md) for the corpus, metrics, limits,
report format, and coverage boundaries.

```sh
./scripts/bench.sh baseline   # capture a known-good local reference once
./scripts/bench.sh            # compare current release build against it
python3 -m unittest discover -s scripts/tests -v  # fast scoring/gate tests
```

The local harness calls production Whisper and Kokoro code. It measures repeated
first/warm/idle runs, replays the pause fixture through streaming finalization,
gates word/punctuation accuracy, and detects changes in generated TTS audio.
Missing inputs fail rather than skip. `publish-release.sh` runs the comparison
before tagging; `--skip-bench` remains an explicit override. Hardware performance
is measured locally; CI runs model-free gate tests and the existing real-audio
STT smoke test. Native speech, microphone/device startup and actual audible
playback still need a live smoke test; software queue estimates cannot certify
them. The old `bench_stt`, `bench_tts`, and `bench_start` examples remain diagnostic
tools, not the new baseline comparison gate.

### Digital-silence regression

Repeated real-model runs exposed `[end]` being returned for an all-zero WAV,
even though a single silence smoke test passed. Exactly zero samples now return
an empty transcript before model loading/inference. No amplitude threshold is
used: the fast test also checks that quiet nonzero audio still enters the model
path. The full local corpus keeps silence as a zero-tolerance quality case.

### STT pause-context regression

Pause detection now triggers a speculative decode of the **whole recording so far**. New snapshots replace earlier text. If speech continues after the latest snapshot, releasing the hotkey decodes the complete recording; it never concatenates independent pause fragments. This gives Whisper both sides of a mid-sentence pause to revise punctuation and word choices. If the cached snapshot already covers speech through release, it can still be reused. Continuing speech may incur more release-time inference than the old tail-only path; accuracy takes priority.

The synthetic `mid-sentence-pause.wav` fixture reproduces “make. is to keep” with the old split decoding. Its expected continuous sentence is guarded with `small.en`:

```sh
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features pause_fixture_preserves_sentence_context -- --ignored --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features continued_speech_matches_whole_recording_fixture -- --ignored --nocapture
```

Fast tests also cover speculative periods/ellipses being replaced, legitimate final punctuation remaining intact, quiet tails, failed snapshots, and cancellation. The model, decoder sampling strategy, vocabulary, and user corrections are unchanged. These tests address context lost at artificial boundaries; they do not establish general word accuracy across accents, microphones, or background noise.
