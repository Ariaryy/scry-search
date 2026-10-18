import argparse
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "benchmark_summary", Path(__file__).resolve().parents[1] / "benchmark-summary.py")
summary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(summary)


def capture(ns=10_000_000):
    metadata = {field: "fixed" for field in summary.COMPATIBLE}
    metadata.update(schema=3, samples=40, rounds=3, daemon_build="build-a",
                    timed_validation="ordered_full_entry_sha256", reference_snapshot_sha256="fixture-snapshot", corpus_fingerprint={"records":130},
                    machine="fixture", power="AC-fixed-plan",
                    corpus={"records": 130, "volumes": 1, "delta_state": "fixed-overlay"})
    key = ("target", "substring", "substring", "relevance", "rpc", "rpc", "warm", 0)
    rows = [dict(zip(summary.CELL, key), round=round_no, sample=sample,
                 elapsed_ns=ns, emitted=50, expected_emitted=50, validated=True,answer_sha256='a'*64)
            for round_no in range(3) for sample in range(1, 41)]
    return metadata, {key: rows}, []


class EvidenceTests(unittest.TestCase):
    def test_strata_keep_routes_separate_and_expose_exact_counts(self):
        metadata, cells, _ = capture()
        metadata["generation"] = {"cases": [{"case": "target", "shape": "prefix-1", "bucket": "lt1k", "matches": 130}]}
        key = next(iter(cells))
        key = (key[0], "prefix-1/lt1k", *key[2:])
        cells = {key: next(iter(cells.values()))}
        result = summary.summarize_strata(metadata, cells)
        self.assertEqual((result[0]["queries"], result[0]["samples"], result[0]["matches_min"]), (1, 120, 130))
        self.assertEqual(result[0]["execution"], "rpc")
        cells[(*key[:5], "shared_index", *key[6:])] = cells[key]
        self.assertEqual(len(summary.summarize_strata(metadata, cells)), 2)

    def setUp(self):
        self.args = argparse.Namespace(target_case="target", min_samples=100,
                                       min_benefit_pct=10, max_regression_pct=5,
                                       noise_floor_ms=.1)

    def test_nearest_rank_and_round_variation(self):
        self.assertEqual(summary.percentile([3, 1, 4, 2], .5), 2)
        self.assertEqual(summary.percentile([3, 1, 4, 2], .99), 4)
        cell = summary.summarize(capture()[1])[0]
        self.assertEqual((cell["samples"], cell["p50_ms"], cell["round_p50_max_ms"]), (120, 10, 10))

    def test_improvement_passes(self):
        self.assertTrue(summary.compare(capture(), capture(8_000_000), self.args)["passed"])

    def test_equal_counts_with_changed_ordered_answers_cannot_pass(self):
        after=capture(8_000_000)
        for row in next(iter(after[1].values())):
            row['answer_sha256']='b'*64
        with self.assertRaisesRegex(ValueError,'ordered answers'):
            summary.compare(capture(),after,self.args)
        old=capture()
        old[0]['schema']=2
        with self.assertRaisesRegex(ValueError,'schema-3'):
            summary.compare(old,capture(),self.args)

    def test_no_benefit_and_regression_fail(self):
        self.assertFalse(summary.compare(capture(), capture(), self.args)["passed"])
        self.assertFalse(summary.compare(capture(), capture(12_000_000), self.args)["passed"])

    def test_material_tail_regression_fails_despite_median_win(self):
        after = capture(8_000_000)
        for row in list(after[1].values())[0][:10]:
            row["elapsed_ns"] = 20_000_000
        self.assertFalse(summary.compare(capture(), after, self.args)["passed"])

    def test_workload_or_route_change_rejected(self):
        after = capture()
        after[0]["workload_hash"] = "different"
        with self.assertRaisesRegex(ValueError, "workload_hash"):
            summary.compare(capture(), after, self.args)
        after = capture()
        key = next(iter(after[1]))
        after[1][(*key[:5], "shared_index", *key[6:])] = after[1].pop(key)
        with self.assertRaisesRegex(ValueError, "coverage"):
            summary.compare(capture(), after, self.args)

    def test_unknown_context_and_missing_round_rejected(self):
        after = capture()
        after[0]["daemon_build"] = "unknown"
        with self.assertRaisesRegex(ValueError, "known daemon"):
            summary.compare(capture(), after, self.args)
        after = capture()
        list(after[1].values())[0].pop()
        with self.assertRaisesRegex(ValueError, "missing samples"):
            summary.compare(capture(), after, self.args)

    def test_capture_requires_completion_and_correctness(self):
        metadata, cells, _ = capture()
        rows = [{"type": "metadata", **metadata}]
        rows += [{"type": "sample", **row} for row in next(iter(cells.values()))]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "capture.jsonl"
            def save():
                path.write_text("\n".join(json.dumps(row) for row in rows), encoding="utf-8")
            save()
            with self.assertRaisesRegex(ValueError, "incomplete"):
                summary.load(path)
            rows.append({"type": "complete"})
            save()
            with self.assertRaisesRegex(ValueError, "verification"):
                summary.load(path)
            for round_no in range(3):
                rows.insert(-1, {"type": "verification", "round": round_no,
                                 "case": "target", "requested": "rpc", "ordered_equal": True})
            save()
            self.assertEqual(len(next(iter(summary.load(path)[1].values()))), 120)
            rows[1]["validated"] = False
            save()
            with self.assertRaisesRegex(ValueError,"timed-answer validation"):
                summary.load(path)
            rows[1]["validated"] = True
            rows[1]["emitted"] = 0
            save()
            with self.assertRaisesRegex(ValueError,"timed-answer validation"):
                summary.load(path)

    def test_duplicate_sample_cannot_replace_a_missing_observation(self):
        after = capture()
        rows = next(iter(after[1].values()))
        rows[1]["sample"] = rows[0]["sample"]
        with self.assertRaisesRegex(ValueError, "duplicate sample"):
            summary.compare(capture(), after, self.args)

    def test_legacy_or_unidentified_capture_cannot_pass_gate(self):
        for field, value in [("schema",1),("reference_snapshot_sha256",None),("corpus_fingerprint",None)]:
            after = capture()
            after[0][field] = value
            with self.assertRaises(ValueError):
                summary.compare(capture(),after,self.args)


if __name__ == "__main__":
    unittest.main()
