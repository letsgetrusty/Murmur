#!/usr/bin/env python3
"""Compare installed Kokoro compute configurations; no downloads or playback.

Uses only Python's standard library and the Rust benchmark. Run configurations
sequentially to avoid competing inference workloads. Full logs and JSON results
are kept for inspection, including provider initialization/fallback messages.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import statistics
import subprocess

ROOT = Path(__file__).resolve().parent.parent
MODES = ("cpu_and_gpu", "cpu_and_neural_engine", "all", "cpu", "cpu_only")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--idle-secs", type=int, default=15)
    parser.add_argument("--modes", nargs="+", choices=MODES, default=list(MODES))
    parser.add_argument("--output", type=Path, default=Path("/tmp") / ("murmur-tts-profile-" + datetime.datetime.now().strftime("%Y%m%d-%H%M%S")))
    args = parser.parse_args()
    if args.rounds < 1 or args.idle_secs < 0:
        parser.error("rounds must be positive and idle-secs nonnegative")
    args.output.mkdir(parents=True, exist_ok=True)
    if not args.no_build:
        subprocess.run(["cargo", "build", "--manifest-path", str(ROOT / "src-tauri/Cargo.toml"), "--release", "--example", "bench_tts"], cwd=ROOT, check=True)
    binary = ROOT / "src-tauri/target/release/examples/bench_tts"
    metadata = {"rounds": args.rounds, "idle_secs": args.idle_secs, "modes": args.modes,
                "dispatch": os.environ.get("MURMUR_TTS_DISPATCH", "kokoro"),
                "runtime": os.environ.get("MURMUR_TTS_RUNTIME", "legacy"),
                "threads": os.environ.get("MURMUR_TTS_THREADS", "0"),
                "spin": os.environ.get("MURMUR_TTS_SPIN", "1"),
                "flush": os.environ.get("MURMUR_TTS_FLUSH", "0"),
                "qos": os.environ.get("MURMUR_TTS_QOS", "0")}
    for name, cmd in (("hardware", ["sysctl", "-n", "machdep.cpu.brand_string"]), ("power", ["pmset", "-g", "batt"]), ("revision", ["git", "rev-parse", "HEAD"])):
        metadata[name] = subprocess.check_output(cmd, cwd=ROOT, text=True).strip()
    (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    all_rows = []
    failures = []
    for mode in args.modes:
        print(f"Profiling {mode}…", flush=True)
        env = os.environ.copy()
        for key in list(env):
            if key.startswith("KOKORO_ORT_") or key.startswith("KOKORO_COREML_") or key == "MURMUR_GATE_MIN_REALTIME":
                env.pop(key)
        env.update(MURMUR_TTS_PROFILE="1", MURMUR_TTS_IDLE_SECS=str(args.idle_secs))
        log_path = args.output / f"{mode}.log"
        with log_path.open("w") as log:
            try:
                result = subprocess.run([str(binary), mode, str(args.rounds)], env=env, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, timeout=600)
                code = result.returncode
            except subprocess.TimeoutExpired:
                code = -1
        contents = log_path.read_text()
        rows = [json.loads(line.removeprefix("PROFILE ")) for line in contents.splitlines() if line.startswith("PROFILE ")]
        fallback = "falling back to CPU" in contents or "now using CPU provider after fallback" in contents
        if code or fallback or len(rows) != (args.rounds + 2) * 3:
            failures.append(mode)
            print(f"  FAILED/invalid comparison: exit={code}, fallback={fallback}; see {log_path}", flush=True)
            continue
        all_rows.extend(rows)
        (args.output / "results.json").write_text(json.dumps(all_rows, indent=2) + "\n")
        for case in ("short", "medium", "long"):
            warm = [r for r in rows if r["phase"] == "warm" and r["case"] == case]
            post = next(r for r in rows if r["phase"] == "after_idle" and r["case"] == case)
            print(f"  {case}: median {statistics.median(r['wall_ms'] for r in warm):.0f}ms, worst {max(r['wall_ms'] for r in warm):.0f}ms, post-idle {post['wall_ms']:.0f}ms; inference {statistics.median(r['inference_ms']/r['wall_ms']*100 for r in warm):.1f}%", flush=True)
    print(f"Results: {args.output}", flush=True)
    if failures:
        raise SystemExit("Invalid configurations: " + ", ".join(failures))


if __name__ == "__main__":
    main()
