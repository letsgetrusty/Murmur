#!/usr/bin/env python3
"""Local speech regression gate. Python standard library only; no uploads."""
import argparse
from collections import Counter, defaultdict
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import sys
import wave

ROOT = Path(__file__).resolve().parents[1]
POLICY = {"median_relative": 0.20, "median_ms": 20,
          "p95_relative": 0.30, "p95_ms": 50}
METRICS = ("wall_ms", "first_chunk_ms", "buffer_ready_1x_ms", "buffer_ready_2x_ms",
           "estimated_stall_1x_ms", "estimated_stall_2x_ms")


def digest(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def words(text):
    return re.findall(r"\w+(?:['’]\w+)*", text.casefold())


def distance(a, b):
    previous = list(range(len(b) + 1))
    for i, x in enumerate(a, 1):
        row = [i]
        for j, y in enumerate(b, 1):
            row.append(min(row[-1] + 1, previous[j] + 1, previous[j-1] + (x != y)))
        previous = row
    return previous[-1]


def punctuation(text):
    # Include word position: an extra/moved mid-sentence period must be visible.
    position = 0
    marks = Counter()
    for token in re.findall(r"\w+(?:['’]\w+)*|[.,!?;:…]", text.casefold()):
        if re.match(r"\w", token):
            position += 1
        else:
            marks[(position, token)] += 1
    return marks


def accuracy(reference, actual):
    expected, observed = words(reference), words(actual)
    a, b = punctuation(reference), punctuation(actual)
    return {"wer": distance(expected, observed) / max(1, len(expected)),
            "punctuation_errors": sum((a-b).values()) + sum((b-a).values())}


def percentile(values, p):
    return sorted(values)[max(0, math.ceil(len(values) * p) - 1)]


def validate_rows(rows, corpus, repeats, rounds, mixed=True):
    expected = Counter()
    for repeat in range(repeats):
        for engine in ("stt", "tts"):
            for case in corpus[engine]:
                for phase, count in (("first", 1), ("warm", rounds), ("after_idle", 1)):
                    for iteration in range(count):
                        expected[(repeat, engine, case["id"], phase, iteration)] += 1
                if engine == "stt" and case.get("replay"):
                    expected[(repeat, engine, case["id"], "replay", 0)] += 1
        expected[(repeat, "tts", "model", "load", 0)] += 1
        if mixed:
            for iteration in range(rounds):
                expected[(repeat, "stt", "pause", "coexist", iteration)] += 1
                expected[(repeat, "tts", "paragraph", "coexist", iteration)] += 1
    actual = Counter((r["repeat"], r["engine"], r["case"], r["phase"], r["round"]) for r in rows)
    if actual != expected:
        raise ValueError(f"Incomplete/duplicate measurements: missing={expected-actual}, extra={actual-expected}")
    for row in rows:
        required = ["wall_ms"]
        if row["phase"] != "load":
            required.append("audio_secs")
            if row["engine"] == "tts":
                required.extend(METRICS[1:])
                required.append("rms")
                if not isinstance(row.get("waveform_hash"), str):
                    raise ValueError("Missing waveform fingerprint")
            elif not isinstance(row.get("text"), str):
                raise ValueError("Missing transcript")
        for name in required:
            value = row.get(name)
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
                raise ValueError(f"Invalid {name}: {value}")
        if row["phase"] != "load" and row["audio_secs"] <= 0:
            raise ValueError("Empty audio")
        if row["engine"] == "tts" and row["phase"] != "load" and row["rms"] <= .001:
            raise ValueError("Silent TTS output")


def summarize(rows, corpus):
    groups = defaultdict(list)
    quality = {}
    for row in rows:
        groups[f'{row["engine"]}/{row["case"]}/{row["phase"]}'].append(row)
    latency = {}
    for key, samples in groups.items():
        latency[key] = {}
        for metric in METRICS:
            values = [r[metric] for r in samples if metric in r]
            if values:
                latency[key][metric] = {"n": len(values), "median": statistics.median(values),
                                        "p95": percentile(values, .95), "worst": max(values)}
    for case in corpus["stt"]:
        samples = [r for r in rows if r["engine"] == "stt" and r["case"] == case["id"]]
        scores = [accuracy(case["reference"], r["text"]) for r in samples]
        quality["stt/" + case["id"]] = {
            "wer": max(s["wer"] for s in scores),
            "punctuation_errors": max(s["punctuation_errors"] for s in scores),
            "silence_hallucination": not case["reference"] and any(r["text"].strip() for r in samples),
            "transcripts": sorted({r["text"] for r in samples})}
    for case in corpus["tts"]:
        samples = [r for r in rows if r["engine"] == "tts" and r["case"] == case["id"]]
        quality["tts/" + case["id"]] = {"hashes": sorted({r["waveform_hash"] for r in samples}),
            "min_audio_secs": min(r["audio_secs"] for r in samples),
            "max_audio_secs": max(r["audio_secs"] for r in samples)}
    return {"latency": latency, "quality": quality}


def failures(summary, corpus, baseline=None):
    errors = []
    for case in corpus["stt"]:
        key = "stt/" + case["id"]
        scores = summary["quality"][key]
        if scores["silence_hallucination"]:
            errors.append(f"{key}: hallucination on silence")
        for metric, limit in (("wer", case["max_wer"]), ("punctuation_errors", case["max_punctuation_errors"])):
            if baseline:
                limit = min(limit, baseline["quality"][key][metric])
            if scores[metric] > limit + 1e-9:
                errors.append(f"{key}: {metric} {scores[metric]:.4f} exceeds {limit:.4f}")
    if baseline:
        for key, value in summary["quality"].items():
            if key.startswith("tts/") and not set(value["hashes"]).issubset(baseline["quality"][key]["hashes"]):
                errors.append(f"{key}: audio changed; listen to saved WAVs before accepting a new baseline")
        for key, metrics in summary["latency"].items():
            for metric, stats in metrics.items():
                reference = baseline["latency"][key][metric]
                for stat in (["median", "p95"] if key.endswith(("/warm", "/coexist")) else ["median"]):
                    limit = reference[stat] * (1 + POLICY[stat + "_relative"]) + POLICY[stat + "_ms"]
                    if stats[stat] > limit:
                        errors.append(f"{key} {metric} {stat}: {stats[stat]:.1f}ms > {limit:.1f}ms (baseline {reference[stat]:.1f}ms)")
    return errors


def power():
    status = command("pmset", "-g", "batt")
    settings = command("pmset", "-g", "custom")
    # Battery percentage changes during a run; power source/settings must not.
    return {"source": status.splitlines()[0], "settings": settings}


def compatibility(corpus, args):
    models = Path.home() / "Library/Application Support/murmur/models"
    inputs = {"whisper": models / "ggml-small.en.bin", "kokoro": models / "kokoro-v1.0.onnx",
              "corpus": ROOT / "benchmarks/corpus.json", "lexicon": ROOT / "src-tauri/src/dev_terms.tab"}
    for case in corpus["tts"]:
        inputs["voice/" + case["voice"]] = models / "kokoro-voices" / (case["voice"] + ".bin")
    for case in corpus["stt"]:
        path = ROOT / case["wav"]
        with wave.open(str(path)) as f:
            if (f.getnchannels(), f.getsampwidth(), f.getframerate(), f.getcomptype()) != (1, 2, 16000, "NONE"):
                raise ValueError(f"Expected mono 16kHz PCM16: {path}")
        inputs["fixture/" + case["id"]] = path
    return {"schema": 2, "hardware": {"chip": command("sysctl", "-n", "machdep.cpu.brand_string"),
            "ram": command("sysctl", "-n", "hw.memsize"), "arch": platform.machine()},
            "macos": command("sw_vers", "-productVersion"), "macos_build": command("sw_vers", "-buildVersion"),
            "rustc": command("rustc", "--version"), "power": power(),
            "provider": "coreml/cpu_and_gpu", "stt_model": "small.en",
            "inputs": {key: digest(path) for key, path in inputs.items()}, "policy": POLICY,
            "repeats": args.repeats, "rounds": args.rounds, "idle_secs": args.idle_secs}


def source_hashes():
    paths = command("git", "ls-files", "--cached", "--others", "--exclude-standard",
                    "src-tauri", "scripts/bench-speech.py", "benchmarks").splitlines()
    return {p: digest(ROOT / p) for p in sorted(set(paths)) if (ROOT / p).is_file()}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")


def report(summary, errors, baseline):
    lines = ["# Local speech benchmark", "", "FAIL" if errors else "PASS", "",
             "Latency is milliseconds. First/idle/replay have few samples; warm p95 is nearest-rank.", "",
             "| Case / phase | Metric | N | Median | Baseline median | p95 | Worst |",
             "|---|---|---:|---:|---:|---:|---:|"]
    for key, metrics in summary["latency"].items():
        for metric, stats in metrics.items():
            old = f'{baseline["latency"][key][metric]["median"]:.1f}' if baseline else "—"
            lines.append(f'| {key} | {metric} | {stats["n"]} | {stats["median"]:.1f} | {old} | {stats["p95"]:.1f} | {stats["worst"]:.1f} |')
    lines.extend(["", "## Quality", "", "```json", json.dumps(summary["quality"], indent=2), "```", "",
                  "Queue readiness/stalls are software estimates, not audible playback measurements.",
                  "TTS fingerprints detect audio changes, not perceptual quality. Listen to saved WAVs."])
    if errors:
        lines.extend(["", "## Gate failures", ""] + ["- " + e for e in errors])
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("baseline", "compare"), nargs="?", default="compare")
    parser.add_argument("--baseline", type=Path, default=ROOT / ".benchmarks/baseline.json")
    parser.add_argument("--replace-baseline", action="store_true")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--rounds", type=int, default=7)
    parser.add_argument("--idle-secs", type=int, default=20)
    args = parser.parse_args()
    if args.repeats < 3 or args.rounds < 7 or args.idle_secs < 0:
        parser.error("Gating needs at least 3 repeats, 7 warm rounds, and nonnegative idle seconds")
    if args.replace_baseline and args.action != "baseline":
        parser.error("--replace-baseline requires baseline action")
    (ROOT / ".benchmarks").mkdir(exist_ok=True)
    with (ROOT / ".benchmarks/run.lock").open("w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            parser.error("Another benchmark is already running in this checkout")
        return run_suite(args, parser)


def run_suite(args, parser):
    old = None
    if args.action == "compare":
        if not args.baseline.exists():
            parser.error("No local baseline. Run scripts/bench.sh baseline on a known-good revision first.")
        old = json.loads(args.baseline.read_text())
    elif args.baseline.exists() and not args.replace_baseline:
        parser.error("Baseline exists. Replacement requires explicit --replace-baseline after review.")
    corpus = json.loads((ROOT / "benchmarks/corpus.json").read_text())
    comparable = compatibility(corpus, args)  # Missing model/fixture is an error, never a skip.
    if old and old["compatibility"] != comparable:
        changed = [k for k in comparable if comparable[k] != old["compatibility"].get(k)]
        raise ValueError("Incompatible baseline: " + ", ".join(changed))
    folder = ROOT / ".benchmarks/runs" / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    folder.mkdir(parents=True)
    print(f"Report directory: {folder}", flush=True)
    before_build = source_hashes()
    subprocess.run(["cargo", "build", "--locked", "--release", "--features", "bench", "--example", "bench_speech"],
                   cwd=ROOT / "src-tauri", check=True)
    binary = ROOT / "src-tauri/target/release/examples/bench_speech"
    source = source_hashes()
    if before_build != source:
        raise ValueError("Source changed while building; rerun")
    provenance = {"git": command("git", "rev-parse", "HEAD"), "status": command("git", "status", "--short"),
                  "source_hashes": source, "binary_sha256": digest(binary),
                  "thermal_before": command("pmset", "-g", "therm"),
                  "load_before": os.getloadavg()}
    write_json(folder / "provenance.json", provenance)
    rows = []
    env = {k: v for k, v in os.environ.items() if not k.startswith(("KOKORO_", "MURMUR_", "ORT_", "OMP_"))}
    try:
        for repeat in range(args.repeats):
            for engine in (("stt", "tts", "mixed") if repeat % 2 == 0 else ("tts", "stt", "mixed")):
                scenario = "same-process overlap" if engine == "mixed" else f"first, warm, {args.idle_secs}s idle"
                print(f"Repeat {repeat+1}/{args.repeats}: {engine} ({scenario})", flush=True)
                artifacts = folder / f"{repeat}-{engine}"
                with (folder / f"{repeat}-{engine}.log").open("w+") as log:
                    result = subprocess.run([str(binary), engine, str(ROOT), str(ROOT / "benchmarks/corpus.json"),
                                str(args.rounds), str(args.idle_secs), str(artifacts)], env=env,
                                stdout=log, stderr=subprocess.STDOUT, timeout=900 + args.idle_secs)
                    log.seek(0)
                    for line in log:
                        if line.startswith("BENCH "):
                            rows.append({**json.loads(line[6:]), "repeat": repeat})
                    result.check_returncode()
                write_json(folder / "results.json", rows)
        validate_rows(rows, corpus, args.repeats, args.rounds)
        if comparable["power"] != power():
            raise ValueError("Power source/settings changed during benchmark")
        if source != source_hashes():
            raise ValueError("Source changed during benchmark; rerun against one fixed revision")
        provenance["thermal_after"] = command("pmset", "-g", "therm")
        provenance["load_after"] = os.getloadavg()
        summary = summarize(rows, corpus)
        previous = old["summary"] if old else None
        errors = failures(summary, corpus, previous)
        result = {"compatibility": comparable, "provenance": provenance, "summary": summary,
                  "errors": errors, "artifacts": str(folder)}
        write_json(folder / "summary.json", result)
        (folder / "report.md").write_text(report(summary, errors, previous))
        for error in errors:
            print("FAIL: " + error)
        if errors:
            return 1
        if args.action == "baseline":
            args.baseline.parent.mkdir(parents=True, exist_ok=True)
            temporary = args.baseline.with_suffix(".tmp")
            write_json(temporary, result)
            temporary.replace(args.baseline)
            print(f"Baseline saved: {args.baseline}")
        print(f"PASS — report: {folder / 'report.md'}")
        return 0
    except Exception as error:
        write_json(folder / "results.json", rows)
        (folder / "report.md").write_text(f"# Benchmark incomplete — FAIL\n\n{error}\n\nSee process logs in this directory.\n")
        raise


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"Benchmark failed: {error}", file=sys.stderr)
        sys.exit(1)
