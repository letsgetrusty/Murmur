# Local speech regression benchmarks

Run benchmarks on your Mac, with installed `small.en` and Kokoro models plus
`am_puck` / `af_heart` voices. Nothing is uploaded or downloaded by the suite.
The harness uses production Whisper decoding, vocabulary/corrections, speculative
STT finalization, Kokoro's persistent worker, text splitting and audio trimming.
It never reads dictation history, changes settings, or captures the microphone.

## Before and after an optimization

```sh
# On a known-good revision, once per hardware/model/test configuration:
./scripts/bench.sh baseline

# After making a change (also the default pre-release gate):
./scripts/bench.sh
```

Each command builds the release harness from current source, then runs STT and
Kokoro in isolation, then a same-process overlap scenario. Allow roughly 5–10 minutes on Apple Silicon; slower hardware
can take longer. Three independent processes per engine each perform a first
pass, seven warm passes, and one pass after 20 seconds idle. STT additionally
replays the pause fixture at recording speed through the actual streaming logic.
The overlap scenario runs pause dictation and paragraph synthesis together to
expose shared-resource regressions; it also collects 21 observations per engine.
There are 21 isolated warm observations per case; first/idle/replay have only three.
A full default run contains 237 measurements.
Engine order alternates between repeats. File caches are **not** flushed: “first”
means fresh process, not cold boot. Only the first case after load/idle measures
that immediate transition; the remaining cases measure the resumed sequence.
Kokoro initialization (including its real shape probes) is timed separately.

Run on the same power source and power settings, close heavy workloads, and avoid
using Murmur during measurements. Different hardware, macOS/compiler versions,
models, voices, fixture hashes, pronunciation dictionary, sampling settings, or
thresholds reject comparison as incompatible. Source/binary hashes and Git state
are recorded as provenance; source changes are expected between measurements.
Keep the baseline's report and WAVs for review. Baselines and all artifacts live
in `.benchmarks/` and are gitignored. Missing models, invalid audio, crashed
processes, source edits during a run, and incomplete measurements **fail**, rather than skip silently.

## Metrics and gates

| Area | Measurements / guard |
|---|---|
| STT latency | Complete WAV → final transcript; real-time replay release → final transcript |
| STT words | Word error rate (insertions + deletions + substitutions / reference words), ignoring case/punctuation |
| STT punctuation | Marks and their word positions; catches misplaced periods and ellipses |
| STT silence | Any nonempty transcript fails |
| Kokoro latency | Model initialization, first synthesized chunk, complete synthesis |
| Kokoro buffering | Estimated buffer-ready time and starvation at 1× / 2× playback |
| Kokoro output | Nonempty, finite, nonsilent PCM and exact waveform fingerprint; saved WAVs for listening |

Each latency metric reports median, nearest-rank p95, worst, and sample count.
Warm/overlap median fails above `baseline × 1.20 + 20ms`; warm/overlap p95 fails above
`baseline × 1.30 + 50ms`. Other phases gate median only, because three samples
cannot support a meaningful tail estimate. Worst is diagnostic. These margins
allow small timing noise; they are not a statistical guarantee. A consistent
small slowdown below these limits can pass, so read the report as well.

STT must meet the corpus's absolute word/punctuation limits **and** must not
worsen the baseline's worst observed score. Word-position punctuation scoring
can also penalize position shifts caused by word errors; raw transcripts make
that visible. TTS waveform differences require listening review: a changed hash
is not proof of worse speech, and an unchanged hash is not proof that the original
pronunciation was good. A baseline is a reference, not a certificate of quality.
Never refresh it just to make a failing optimization green.

After an optimization passes comparison and review, advance the baseline to the
accepted version and retain the previous reports. Otherwise a later change could
lose the new speedup while still passing against an older, slower reference.
Baseline replacement is a deliberate step, never an automatic response to failure.

For an accepted optimization, or a reviewed change that intentionally alters
audio, model, or corpus:

```sh
# Prefer a separate baseline to preserve the previous reference:
./scripts/bench.sh baseline --baseline .benchmarks/reviewed-baseline.json
./scripts/bench.sh compare --baseline .benchmarks/reviewed-baseline.json

# Explicit replacement is available after reviewing quality and timings:
./scripts/bench.sh baseline --replace-baseline
```

For long-idle investigations, use `--idle-secs 300` on **both** baseline and
comparison commands. The default 20-second idle test does not establish behavior
after hours in the background. `--repeats` and `--rounds` can increase sampling;
the gate refuses fewer than three processes and seven warm rounds. There is no
`--no-build` shortcut that could silently measure a stale executable.

`report.md` contains the timing table and quality results; `summary.json` carries
machine/model metadata and comparison failures; `results.json` contains every
observation. Per-process logs and generated TTS WAVs sit beside them. A failed
run still leaves diagnostic artifacts. Baselines are saved only after quality
checks pass; comparisons never update them.

## Coverage and remaining checks

The initial committed corpus covers short dictation, a mid-sentence pause, a
40-second passage crossing Whisper's 30-second window, and silence. TTS covers
two voices, a short clause, a sentence, and a multi-chunk paragraph. These are
synthetic fixtures, not representative accuracy measurements across accents or
microphones. Add consented real recordings with checked references, technical
terms/names, noise and hesitations as failures are discovered. WAVs must be mono
16kHz PCM16. Put cases and explicit quality limits in `benchmarks/corpus.json`;
changed input hashes deliberately require a new baseline.

This gate covers Whisper and **Kokoro**, not native `AVSpeechSynthesizer`.
Queue timing is a software estimate using the real buffer policy; it excludes
AVQueuePlayer, temporary-file I/O, audio-device startup and OS scheduling. It
cannot certify time to first audible sound, actual dropouts, cancellation,
clipboard delivery, native-voice quality, peak memory, or all patterns of contention.
Continue the live Fn/read-aloud/cancel/retrigger smoke test after speech changes.
For mic-start diagnosis the existing `bench_start` remains available:

```sh
cargo run --release --manifest-path src-tauri/Cargo.toml --example bench_start
```

The previous mic gate selected the best attempt and could accept missing real
samples, so it is not used as evidence by this engine benchmark. Microphone and
playback-device benchmarking need a separate controlled end-to-end harness.

CI runs only fast model-free scoring/gate tests and Rust invariants, plus the
existing small real-audio STT smoke check. Hardware performance numbers come
from your local runs, never shared CI timing. Before merging **any** speech
optimization, compare against the unchanged known-good baseline and review the
report; release publishing invokes the comparison automatically. The existing
`publish-release.sh --skip-bench` is an explicit override, not a benchmark pass.
When changing measurement boundaries or scoring semantics, bump the runner
protocol schema and capture a new reference; do not compare unlike measurements.
No finite corpus can guarantee that every future regression is caught.

## Validation status (October 7, 2026)

The initial harness and pause/silence fixes pass the model-free gate tests,
Rust checks, and both real-model pause regression tests. A full local comparison
on October 7 failed Kokoro initialization, first-pass, warm-tail, and buffering
latency limits. Its source hashes and executable hash were identical to the
saved baseline, and all quality results were identical. This demonstrates that
the current timing gate can fail an unchanged executable; it does not establish
a code-induced slowdown or a performance pass. System load increased during
the run, but the measurements do not isolate the cause.

The unchanged baseline and failed report are retained locally under
`.benchmarks/` (run `20261007T144500.809406Z`). Performance validation remains
open: repeat under controlled machine load and review timing stability before
using this gate as evidence for an optimization. Thresholds have not been
relaxed and the baseline has not been replaced. Live microphone, playback,
cancel, and retrigger checks remain necessary outside the engine harness.
