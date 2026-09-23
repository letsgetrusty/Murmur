# Kokoro latency profiling

Measured locally on 2026-09-23: Apple M4, 16 GB RAM, battery power with Low Power Mode disabled. Release build, installed Kokoro v1.0 FP32 model, `am_puck`, synthesis speed 1.0. Playback speed is separate. No model downloads or playback are performed by this benchmark.

## Findings

Changing compute policy alone did not eliminate latency spikes. More than 99% of measured synthesis time was inside ONNX inference; text processing and input preparation usually took 3–10 ms. A CoreML policy specifies eligible devices, not where every operator actually executes. Timing cannot prove GPU/ANE use or CPU fallback.

Initial comparison: three warm rounds of identical short (36 characters), medium (85), and long (166) samples. Each round runs short → medium → long; a final sequence follows 15 seconds idle. Times below are milliseconds. The long sample produces roughly 10.4 seconds of audio.

| Policy | Short warm median | Medium warm median | Long warm median | Long after idle |
| --- | ---: | ---: | ---: | ---: |
| CoreML CPU + GPU | 787 | 1266 | 2342 | 5520 |
| CoreML CPU + Neural Engine | 604 | 1155 | 2140 | 5798 |
| CoreML all | 675 | 1200 | 2358 | 5593 |
| ONNX CPU | 557 | 1147 | 2187 | 5761 |
| CoreML CPU only | 609 | 1112 | 2187 | 5476 |

“After idle” is one resumed sequence, not a fresh idle period before each sample. All policies had short-sample outliers too. Run order and background system activity can affect results; these small samples do not establish a winning device.

## Dispatch experiment

A diagnostic session holds model, voice, text preparation, and CoreML CPU + GPU policy constant, changing only `Session::run` versus `Session::run_async`. Both use kokoro-en's public G2P, chunking, and tokenization functions. Unlike the library loader, the diagnostic loader does not issue model-shape probes; compare its warm runs with each other, not cold-load times with the library.

| Dispatch | Warm rounds | Short median / worst | Medium median / worst | Long median / worst |
| --- | ---: | ---: | ---: | ---: |
| Sync | 4 | 709 / 1045 | 1258 / 1423 | 2335 / 2510 |
| Async | 4 | 1297 / 1481 | 1257 / 1566 | 2617 / 5801 |
| Sync confirmation | 6 | 636 / 775 | 1209 / 1457 | 2373 / 2761 |

The confirmation used 30 seconds idle; the resumed sequence took 1243 / 1255 / 2422 ms. Synchronous dispatch reduced observed worst-case long-sample latency, but did not eliminate variability. Audio duration and RMS matched between modes; this is a sanity check, not a listening or bit-for-bit quality test.

The next implementation candidate is synchronous inference on a dedicated blocking worker, preserving asynchronous callers and the existing Speaker interface. It must not run on the UI or async executor thread. At the time of this experiment, the application still used kokoro-en's existing inference implementation. See the production implementation section below for the subsequent change.

Other experiments with global thread pools (2/4 threads), disabling spinning, and user-initiated macOS QoS did not consistently improve latency. ONNX's flush-denormals setting is x86-only and cannot explain an M4 improvement. No experimental pool tuning is applied to the app.

## Reproduce

```sh
python3 scripts/profile-tts.py
MURMUR_TTS_DISPATCH=sync python3 scripts/profile-tts.py --no-build --modes cpu_and_gpu --rounds 6 --idle-secs 30
MURMUR_TTS_DISPATCH=async python3 scripts/profile-tts.py --no-build --modes cpu_and_gpu --rounds 6 --idle-secs 30
```

Run sequentially with no other inference benchmarks active. The runner saves hardware/power/revision metadata, full provider logs, and JSON measurements under its printed output directory (`--output` overrides it). It fails on missing assets, invalid/nonfinite/silent output, incomplete runs, or recognized fallback messages. This log check is not proof of per-operator device assignment. `MURMUR_TTS_TRACE=1` enables detailed ORT logging.

For thread-pool experiments only, set `MURMUR_TTS_RUNTIME=tuned`, optionally `MURMUR_TTS_THREADS=2` or `4`, `MURMUR_TTS_SPIN=0`, and `MURMUR_TTS_QOS=1`. These switches affect the standalone benchmark only. `MURMUR_TTS_FLUSH=1` is retained for reproducing the discarded experiment; it has no effect on ARM.

The app now logs inference versus other/queue time for each chunk at debug level, and surfaces slow synthesis (at least two seconds and below 2× realtime) at info level in `~/Library/Logs/murmur.log`. This helps distinguish inference stalls from preparation/queue delays without recording spoken text.

Provider semantics: [ONNX Runtime CoreML documentation](https://onnxruntime.ai/docs/execution-providers/CoreML-ExecutionProvider.html). Denormal implementation: [ONNX Runtime source](https://github.com/microsoft/onnxruntime/blob/main/onnxruntime/core/common/denormal.cc).

## Background-worker follow-up (2026-09-23)

Tested the proposed boundary itself: a persistent Rust thread owns the direct synchronous session, receives text over a channel, and replies through a Tokio oneshot. Caller wall time includes preparation, request/response transport, and inference. Production code was unchanged during this experiment.

Four sequential processes ran in current → worker → worker → current order, each with six warm rounds plus first and post-idle sequences (20 seconds idle). Total: 96 syntheses, including 72 warm calls, or 12 warm observations per case per implementation. Same M4, battery power (49% at start), no recorded thermal/performance warnings, same model/voice/CoreML CPU+GPU policy. Clippy overlapped the first process's initialization briefly; warm measurements followed it. These are local observations, not a general hardware guarantee.

| Sample | Current warm median | Worker warm median | Latency reduction | Current worst | Worker worst |
| --- | ---: | ---: | ---: | ---: | ---: |
| Short, 36 chars | 701 ms | 558 ms | 20.4% | 1852 ms | 856 ms |
| Medium, 85 chars | 1373 ms | 1135 ms | 17.3% | 1626 ms | 1615 ms |
| Long, 166 chars | 2801 ms | 2118 ms | 24.4% | 6641 ms | 2599 ms |

Each pair favored the worker on all three medians. Across all warm calls, synthesis time fell from 67.11 to 48.47 seconds (27.8% less time), producing the same 222.9 seconds of audio: throughput increased from 3.32× to 4.60× realtime (38.4%). Typical per-case speedup was 1.21–1.32×. The largest observed stall reduction was 60.9% for the long sample; medium-sample worst latency barely changed. These maxima are sample maxima, not latency guarantees or robust tail percentiles.

The short sample following idle took 719/757 ms on the current path versus 625/662 ms on the worker. First synthesis after loading was 717/709 ms versus 715/1044 ms: no consistent first-synthesis improvement. Worker loading skips the library's shape probes, so its shorter loading time is not evidence that dispatch improves cold startup. Actual time to audible playback was not measured; buffering and playback startup still contribute.

Every waveform had the same duration and deterministic sample-bit checksum for its text across all 96 runs. Median non-inference overhead was about 5 ms for both implementations. This supports retaining model output while changing execution scheduling, though checksum agreement is not a listening test.

Recommendation: worthwhile to implement for roughly 17–24% lower typical chunk-synthesis latency and substantially fewer large stalls on this machine. Do not promise 2× overall speed or instant startup. Before production adoption, retain model/voice error handling and verify cancellation, concurrent previews, and the async/UI responsiveness boundary.

[Raw measurements](benchmarks/tts-worker-2026-09-23.csv). Reproduce each block sequentially (use a unique output directory per block):

```sh
MURMUR_TTS_DISPATCH=kokoro python3 scripts/profile-tts.py --modes cpu_and_gpu --rounds 6 --idle-secs 20 --output /tmp/tts-1-current
MURMUR_TTS_DISPATCH=worker python3 scripts/profile-tts.py --no-build --modes cpu_and_gpu --rounds 6 --idle-secs 20 --output /tmp/tts-2-worker
MURMUR_TTS_DISPATCH=worker python3 scripts/profile-tts.py --no-build --modes cpu_and_gpu --rounds 6 --idle-secs 20 --output /tmp/tts-3-worker
MURMUR_TTS_DISPATCH=kokoro python3 scripts/profile-tts.py --no-build --modes cpu_and_gpu --rounds 6 --idle-secs 20 --output /tmp/tts-4-current
```

## Production implementation

`src-tauri/src/kokoro.rs` now owns one persistent blocking worker and ONNX session. Model loading, dynamic-shape probes, voice loading, G2P, and synchronous inference all run there. Callers await a bounded request channel; the UI and async executor remain responsive. Kokoro-en still supplies phonemes and tokens with its GPL feature disabled. The model, audio samples, native default provider, and AVQueuePlayer playback path are unchanged.

The worker lazily caches Murmur's raw f32 voice packs and preserves automatic CoreML → CPU fallback (explicit `coreml` fails rather than silently changing provider). Missing/corrupt voices and model errors return to the caller. Stop or a superseding read invalidates queued read/preview requests; stale previews cannot start playing. A native call already in progress completes, but no further phoneme chunks start for that cancelled request. Warm-up and preview-cache generation share the same worker.

Use `MURMUR_TTS_DISPATCH=production` with `scripts/profile-tts.py` to benchmark the actual implementation, including probes, voice caching, and the bounded channel. `kokoro` remains the previous library implementation for comparisons; `worker` is the earlier diagnostic prototype.

Production-worker verification (four warm rounds, 20-second idle, same samples and CoreML policy): short median/worst **552/556 ms**, medium **1173/1198 ms**, long **2186/2299 ms**. Resumed sequence: **570/1273/2036 ms**. Waveform checksums match the original engine for all samples. [Production measurements](benchmarks/tts-production-2026-09-23.csv).

Local integration checks compare exact waveforms for two voices and multi-chunk text, exercise missing voices, concurrent requests, queue cancellation, and confirm an executor timer advances during native inference. The headless AVQueuePlayer check covers startup/progress and Stop; it did not observe natural playback completion. Live completion verification was unavailable because the computer-use service could not start. This remains a manual check in the signed app.
