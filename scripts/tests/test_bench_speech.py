"""Fast, model-free tests for the local benchmark gate itself."""
import copy
import importlib.util
import contextlib
import io
import json
import tempfile
from unittest import mock
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("bench_speech", Path(__file__).parents[1] / "bench-speech.py")
b = importlib.util.module_from_spec(spec)
spec.loader.exec_module(b)

CORPUS = {"stt": [{"id": "speech", "reference": "We should keep going.", "max_wer": .3,
                   "max_punctuation_errors": 2, "replay": True}],
          "tts": [{"id": "voice"}]}


def rows():
    result = []
    for engine in ("stt", "tts"):
        for case in CORPUS[engine]:
            phases = ["first", "warm", "after_idle"] + (["replay"] if engine == "stt" else [])
            for phase in phases:
                row = {"repeat": 0, "round": 0, "engine": engine, "case": case["id"],
                       "phase": phase, "wall_ms": 100, "audio_secs": 2}
                if engine == "stt":
                    row["text"] = case["reference"]
                else:
                    row.update({metric: 100 for metric in b.METRICS})
                    row.update(rms=.1, waveform_hash="123")
                result.append(row)
    result.append({"repeat": 0, "round": 0, "engine": "tts", "case": "model", "phase": "load", "wall_ms": 200})
    return result


class Scoring(unittest.TestCase):
    def test_word_edits_and_case(self):
        self.assertEqual(b.accuracy("One two three.", "one two three!")["wer"], 0)
        for actual in ("one three", "one four three", "one two extra three"):
            self.assertAlmostEqual(b.accuracy("one two three", actual)["wer"], 1/3)

    def test_punctuation_position_and_ellipsis(self):
        self.assertEqual(b.accuracy("Keep going.", "Keep. going")["punctuation_errors"], 2)
        self.assertEqual(b.accuracy("Keep going.", "Keep... going.")["punctuation_errors"], 3)
        self.assertEqual(b.accuracy("Keep going.", "Keep… going.")["punctuation_errors"], 1)

    def test_percentile_is_nearest_rank(self):
        self.assertEqual(b.percentile(list(range(1, 22)), .95), 20)

    def test_complete_and_finite_data_required(self):
        data = rows()
        b.validate_rows(data, CORPUS, 1, 1, mixed=False)
        for invalid in (data[:-1], data + [data[0]]):
            with self.assertRaises(ValueError):
                b.validate_rows(invalid, CORPUS, 1, 1, mixed=False)
        for value in (float("nan"), float("inf"), -1, None):
            invalid = copy.deepcopy(data)
            invalid[0]["wall_ms"] = value
            with self.assertRaises(ValueError):
                b.validate_rows(invalid, CORPUS, 1, 1, mixed=False)

    def test_latency_noise_tolerated_but_slowdown_fails(self):
        base = b.summarize(rows(), CORPUS)
        candidate = copy.deepcopy(base)
        candidate["latency"]["stt/speech/warm"]["wall_ms"].update(median=139, p95=179)
        self.assertEqual(b.failures(candidate, CORPUS, base), [])
        candidate["latency"]["stt/speech/warm"]["wall_ms"]["p95"] = 181
        self.assertTrue(any("p95" in e for e in b.failures(candidate, CORPUS, base)))

    def test_faster_does_not_hide_quality_loss(self):
        base = b.summarize(rows(), CORPUS)
        data = rows()
        data[0]["text"] = "We should. keep going."
        data[0]["wall_ms"] = 1
        self.assertTrue(any("punctuation_errors" in e for e in b.failures(b.summarize(data, CORPUS), CORPUS, base)))
        candidate = copy.deepcopy(base)
        candidate["quality"]["tts/voice"]["hashes"] = ["changed"]
        self.assertTrue(any("audio changed" in e for e in b.failures(candidate, CORPUS, base)))

    def test_silence_rejects_punctuation_only_hallucination(self):
        corpus = copy.deepcopy(CORPUS)
        corpus["stt"][0]["reference"] = ""
        data = rows()
        for row in data:
            if row["engine"] == "stt":
                row["text"] = "..."
        self.assertTrue(any("hallucination" in e for e in b.failures(b.summarize(data, corpus), corpus)))

class Completeness(unittest.TestCase):
    def test_same_process_scenario_cannot_be_omitted(self):
        with self.assertRaisesRegex(ValueError, "Incomplete"):
            b.validate_rows(rows(), CORPUS, 1, 1)

    def test_missing_tts_pcm_metric_fails_closed(self):
        data = rows()
        for row in data:
            if row["engine"] == "tts" and row["phase"] == "warm":
                del row["first_chunk_ms"]
        with self.assertRaisesRegex(ValueError, "first_chunk_ms"):
            b.validate_rows(data, CORPUS, 1, 1, mixed=False)


class BaselineSafety(unittest.TestCase):
    def test_existing_baseline_is_not_overwritten_implicitly(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".benchmarks").mkdir()
            baseline = root / ".benchmarks/baseline.json"
            baseline.write_text("keep this reference")
            with mock.patch.object(b, "ROOT", root), mock.patch("sys.argv", ["bench", "baseline"]):
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                    b.main()
            self.assertEqual(baseline.read_text(), "keep this reference")

    def test_incompatible_hardware_rejected_before_building(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".benchmarks").mkdir()
            (root / "benchmarks").mkdir()
            (root / "benchmarks/corpus.json").write_text(json.dumps(CORPUS))
            (root / ".benchmarks/baseline.json").write_text(json.dumps({"compatibility": {"hardware": "old"}}))
            with mock.patch.object(b, "ROOT", root), mock.patch("sys.argv", ["bench", "compare"]):
                with mock.patch.object(b, "compatibility", return_value={"hardware": "new"}):
                    with mock.patch.object(b.subprocess, "run") as run:
                        with self.assertRaisesRegex(ValueError, "Incompatible baseline: hardware"):
                            b.main()
                        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
