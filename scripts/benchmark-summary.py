"""Summarize scratch JSONL; optionally gate a matched before/after capture.

Standard library only. Queries and result paths never enter the summary.
Exit 2 means evidence is incomplete/incompatible; exit 1 means the gate failed.
"""
import argparse
import collections
import json
import math
import re
import sys


CELL = ("case", "shape", "kind", "order", "requested", "execution", "phase", "step")
COMPATIBLE = ("schema", "workload_hash", "corpus", "limit", "samples", "warmups",
              "rounds", "machine", "power", "target", "rustc", "profile", "opt_level",
              "rustflags", "os_cache", "background_connections")
COMPATIBLE += ("timed_validation", "reference_snapshot_sha256", "corpus_fingerprint", "daemon_runtime", "generation")


def percentile(values, fraction):
    """Nearest rank, including the largest observation at p100."""
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


def load(path):
    cells = collections.defaultdict(list)
    metadata = None
    complete = False
    verification = set()
    endpoints = []
    with open(path, encoding="utf-8") as source:
        for line in source:
            row = json.loads(line)
            if complete:
                raise ValueError("data after completion marker")
            if row["type"] == "metadata":
                if metadata is not None:
                    raise ValueError("multiple metadata records")
                metadata = row
            elif row["type"] == "sample":
                cells[tuple(row[field] for field in CELL)].append(row)
            elif row["type"] == "verification":
                if not row["ordered_equal"]:
                    raise ValueError("failed correctness verification")
                verification.add((row["round"], row["case"], row["requested"]))
            elif row["type"] == "daemon_stats":
                endpoints.append(row)
            elif row["type"] == "complete":
                complete = True
    if metadata is None or not complete or not cells:
        raise ValueError("incomplete capture")
    for observations in cells.values():
        for row in observations:
            if (row["round"], row["case"], row["requested"]) not in verification:
                raise ValueError("sample lacks untimed correctness verification")
            if metadata.get("schema", 1) >= 2:
                if row.get("validated") is not True or row.get("expected_emitted") != row["emitted"]:
                    raise ValueError("sample lacks successful timed-answer validation")
    return metadata, cells, endpoints


def summarize(cells):
    summary = []
    for key, rows in sorted(cells.items()):
        times = [row["elapsed_ns"] for row in rows]
        rounds = collections.defaultdict(list)
        for row in rows:
            rounds[row["round"]].append(row["elapsed_ns"])
        medians = [percentile(values, .5) for values in rounds.values()]
        summary.append(dict(zip(CELL, key), samples=len(times),
                            p50_ms=percentile(times, .5) / 1e6,
                            p95_ms=percentile(times, .95) / 1e6,
                            p99_ms=percentile(times, .99) / 1e6,
                            round_p50_min_ms=min(medians) / 1e6,
                            round_p50_max_ms=max(medians) / 1e6,
                            emitted_min=min(row["emitted"] for row in rows),
                            emitted_max=max(row["emitted"] for row in rows)))
    return summary


def compare(before, after, args):
    bm, bc, _ = before
    am, ac, _ = after
    for metadata in (bm, am):
        if metadata.get("schema", 1) < 2 or metadata.get("timed_validation") != "ordered_full_entry_sha256":
            raise ValueError("comparison requires validated schema-2 captures")
        if not metadata.get("reference_snapshot_sha256") or not metadata.get("corpus_fingerprint"):
            raise ValueError("comparison requires a reference snapshot identity and complexity fingerprint")
    for field in COMPATIBLE:
        if bm.get(field) != am.get(field):
            raise ValueError(f"incompatible {field}")
    for metadata in (bm, am):
        if any(metadata[field] == "unknown" for field in ("daemon_build", "machine", "power")):
            raise ValueError("comparison needs known daemon build, machine, and power context")
        if metadata["corpus"]["records"] is None or metadata["corpus"]["volumes"] is None:
            raise ValueError("comparison needs known corpus record and volume counts")
        if metadata["corpus"]["delta_state"] == "unknown":
            raise ValueError("comparison needs known delta state")
    if bc.keys() != ac.keys():
        raise ValueError("case/step/phase/execution coverage changed")
    if args.target_case not in {key[0] for key in bc}:
        raise ValueError("target case does not exist")
    if bm["rounds"] < 3:
        raise ValueError("comparison needs at least three rounds")
    comparisons = []
    worthwhile = False
    passed = True
    for key in sorted(bc):
        if dict(zip(CELL, key))["phase"] != "warm":
            continue
        old, new = bc[key], ac[key]
        # Every cell must have enough samples in every round. Switching routes
        # mid-run cannot silently pool local and RPC or hide missing samples.
        for observations, metadata in ((old, bm), (new, am)):
            counts = collections.Counter(row["round"] for row in observations)
            if set(counts) != set(range(metadata["rounds"])) or any(count != metadata["samples"] for count in counts.values()):
                raise ValueError("missing samples or route switched within a cell")
            for round_no in counts:
                samples = [row["sample"] for row in observations if row["round"] == round_no]
                if set(samples) != set(range(1, metadata["samples"] + 1)):
                    raise ValueError("missing or duplicate sample identity")
        if len(old) < args.min_samples or len(new) < args.min_samples:
            raise ValueError("insufficient warm samples per cell")
        old_times = [row["elapsed_ns"] for row in old]
        new_times = [row["elapsed_ns"] for row in new]
        deltas = {}
        for label, quantile in (("p50", .5), ("p95", .95)):
            previous, current = percentile(old_times, quantile), percentile(new_times, quantile)
            change = (current / previous - 1) * 100 if previous else (0 if not current else math.inf)
            delta_ms = (current - previous) / 1e6
            deltas[label + "_change_pct"] = change
            deltas[label + "_change_ms"] = delta_ms
            if change > args.max_regression_pct and delta_ms > args.noise_floor_ms:
                passed = False
            if key[0] == args.target_case and label == "p50" and change <= -args.min_benefit_pct and delta_ms <= -args.noise_floor_ms:
                worthwhile = True
        comparisons.append(dict(zip(CELL, key), **deltas))
    return {"passed": passed and worthwhile, "worthwhile_cell": worthwhile,
            "no_material_regressions": passed, "cells": comparisons}


def summarize_strata(metadata, cells):
    """Uniform fixed-panel mixture, not an estimate of real query frequency."""
    generation = metadata.get("generation")
    if not generation:
        return []
    descriptors = {row["case"]: row for row in generation["cases"]}
    grouped = collections.defaultdict(list)
    for key, rows in cells.items():
        case, shape, kind, order, requested, execution, phase, step = key
        if phase != "warm":
            continue
        if case not in descriptors or step != 0:
            raise ValueError("generated sample lacks stratum descriptor")
        descriptor = descriptors[case]
        if shape != descriptor["shape"] + "/" + descriptor["bucket"]:
            raise ValueError("generated sample stratum differs from descriptor")
        grouped[(descriptor["shape"], descriptor["bucket"], kind, order, requested, execution)].append((descriptor, rows))
    result = []
    for key, members in sorted(grouped.items()):
        if len({len(rows) for _, rows in members}) != 1:
            raise ValueError("unequal query weights in stratum")
        times = [row["elapsed_ns"] for _, rows in members for row in rows]
        counts = [descriptor["matches"] for descriptor, _ in members]
        result.append(dict(zip(("shape", "bucket", "kind", "order", "requested", "execution"), key),
                           queries=len(members), samples=len(times), matches_min=min(counts), matches_max=max(counts),
                           p50_ms=percentile(times, .5) / 1e6, p95_ms=percentile(times, .95) / 1e6,
                           p99_ms=percentile(times, .99) / 1e6,
                           interpretation="equal-weight sampled query panel; not a user-frequency distribution"))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture")
    parser.add_argument("--before")
    parser.add_argument("--target-case", help="predeclare the optimization's benefit case")
    parser.add_argument("--min-benefit-pct", type=float, default=10)
    parser.add_argument("--max-regression-pct", type=float, default=5)
    parser.add_argument("--noise-floor-ms", type=float, default=.1)
    parser.add_argument("--min-samples", type=int, default=100)
    args = parser.parse_args()
    try:
        if (not 0 < args.min_benefit_pct <= 100 or args.max_regression_pct < 0
                or args.noise_floor_ms < 0 or args.min_samples < 1
                or not all(math.isfinite(value) for value in
                           (args.min_benefit_pct, args.max_regression_pct, args.noise_floor_ms))):
            raise ValueError("invalid gate thresholds")
        after = load(args.capture)
        metadata, cells, endpoints = after
        memory = []
        for row in endpoints:
            memory.append({"round": row["round"], "case": row["case"], "requested": row["requested"],
                           **{point: {key: int(value) for key, value in re.findall(
                               r"(private_usage|working_set|peak_working_set|page_faults)=(\d+)", row[point])}
                              for point in ("before", "after")}})
        report = {"metadata": metadata, "cells": summarize(cells), "strata": summarize_strata(metadata, cells), "daemon_memory_endpoints": memory,
                  "timed_answers_validated": metadata.get("schema", 1) >= 2,
                  "corpus_identified": bool(metadata.get("reference_snapshot_sha256"))}
        code = 0
        if args.before:
            if not args.target_case:
                raise ValueError("comparison needs a predeclared --target-case")
            report["gate"] = {"min_benefit_pct": args.min_benefit_pct,
                              "max_regression_pct": args.max_regression_pct,
                              "noise_floor_ms": args.noise_floor_ms,
                              "min_samples": args.min_samples,
                              **compare(load(args.before), after, args)}
            code = 0 if report["gate"]["passed"] else 1
        print(json.dumps(report, indent=2, allow_nan=False))
        return code
    except (ValueError, KeyError, OSError) as error:
        print(f"Invalid benchmark evidence: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
