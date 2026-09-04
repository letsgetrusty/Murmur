#!/usr/bin/env bash
# Layer 2 — core-experience performance gate. Runs the STT / TTS / dictation-start
# benchmarks against fixed thresholds and FAILS (non-zero exit) if any core path
# regressed. Not CI (needs Metal, the models, and audio hardware); this is a
# LOCAL pre-release gate — publish-release.sh runs it before tagging.
#
#   scripts/bench.sh            # run the gate (build release benches if needed)
#   scripts/bench.sh --no-build # reuse already-built benches
#
# A bench whose model/hardware is missing SKIPS with a warning (a missing model
# is not a regression); a bench that RUNS and is below threshold FAILS the gate.
# See docs/testing.md for the rationale and how to tune thresholds.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODELS="$HOME/Library/Application Support/murmur/models"
BIN="$REPO/src-tauri/target/release/examples"
STT_MODEL="small" # the shipped default — gate what users actually run

# ── Thresholds ──────────────────────────────────────────────────────────────
# Generous margin below measured-healthy so normal variance never trips them,
# but a real regression does. Measured healthy on an M4: STT ~26x, TTS ~4.5x,
# mic-ready ~75ms. Override via env for a different machine.
STT_MIN_REALTIME="${STT_MIN_REALTIME:-10}" # small held 25x+ (same size/arch as small.en); medium throttles to ~2.7x
TTS_MIN_REALTIME="${TTS_MIN_REALTIME:-2}"  # must stay above 1x or playback stalls
MIC_MAX_MS="${MIC_MAX_MS:-250}"            # mic live well under the ~0.5-1s that lost first words

fail=0
note() { printf '\n\033[1;36m▶ %s\033[0m\n' "$1"; }
skip() { printf '\033[1;33m⊘ SKIP: %s\033[0m\n' "$1"; }
bad()  { printf '\033[1;31m✗ %s\033[0m\n' "$1"; fail=1; }

# Run a gated bench, echo the interesting lines, and record a failure if it
# exited non-zero (a threshold breach). Isolated so one bench's failure doesn't
# abort the others. Usage: run_gate "<fail msg>" <cmd...>
run_gate() {
  local msg="$1"; shift
  "$@" 2>&1 | grep -E "GATE|WARM avg|iter [0-9]|mic open"
  [ "${PIPESTATUS[0]}" -eq 0 ] || bad "$msg"
}

if [ "${1:-}" != "--no-build" ]; then
  note "Building release benches…"
  ( cd "$REPO/src-tauri" && cargo build --release \
      --example bench_stt --example bench_tts --example bench_start >/dev/null ) \
    || { echo "bench build failed"; exit 1; }
fi

# ── STT ──────────────────────────────────────────────────────────────────────
note "STT gate ($STT_MODEL, ≥${STT_MIN_REALTIME}x realtime)"
if [ ! -f "$MODELS/ggml-$STT_MODEL.bin" ]; then
  skip "whisper model ggml-$STT_MODEL.bin not downloaded"
else
  WAV="$(mktemp -t murmur-bench).wav"
  # Deterministic ~15s clip so the realtime factor is comparable run-to-run.
  say -o "${WAV%.wav}.aiff" "This is a fixed benchmark clip for measuring how fast the on device speech model turns audio into text on Apple Silicon, so we can catch a throughput regression before it ships to anyone."
  afconvert -f WAVE -d LEI16@16000 -c 1 "${WAV%.wav}.aiff" "$WAV"
  run_gate "STT below ${STT_MIN_REALTIME}x realtime" \
    env MURMUR_GATE_MIN_REALTIME="$STT_MIN_REALTIME" "$BIN/bench_stt" "$STT_MODEL" "$WAV" 5
  rm -f "$WAV" "${WAV%.wav}.aiff"
fi

# ── TTS ──────────────────────────────────────────────────────────────────────
note "TTS gate (Kokoro, ≥${TTS_MIN_REALTIME}x realtime)"
if [ ! -f "$MODELS/kokoro-v1.0.onnx" ]; then
  skip "kokoro model not downloaded (opt-in ~310MB)"
else
  run_gate "TTS synth below ${TTS_MIN_REALTIME}x realtime" \
    env MURMUR_GATE_MIN_REALTIME="$TTS_MIN_REALTIME" "$BIN/bench_tts" cpu_and_gpu 5
fi

# ── Dictation start ───────────────────────────────────────────────────────────
note "Dictation-start gate (mic-ready ≤${MIC_MAX_MS}ms)"
run_gate "mic-ready over ${MIC_MAX_MS}ms" \
  env MURMUR_GATE_MAX_MIC_MS="$MIC_MAX_MS" "$BIN/bench_start"

echo
if [ "$fail" -ne 0 ]; then
  printf '\033[1;31m✗ bench gate FAILED — a core path regressed. Do not release.\033[0m\n'
  exit 1
fi
printf '\033[1;32m✓ bench gate passed — STT / TTS / dictation-start within thresholds.\033[0m\n'
