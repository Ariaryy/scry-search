"""Summarize paired processes and optionally gate against an independent A/A panel.

Finite observed noise envelopes are engineering checks, not significance tests.
Uses the same per-query completeness/context checks as benchmark-summary.py.
"""
import argparse
import importlib.util
import json
import math
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location('benchmark_summary', Path(__file__).with_name('benchmark-summary.py'))
summary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(summary)


def panel(directory):
    directory = Path(directory)
    if not json.loads((directory / 'complete.json').read_text(encoding='utf-8')).get('complete'):
        raise ValueError('incomplete process panel')
    context = json.loads((directory / 'context.json').read_text(encoding='utf-8'))
    if context['pairs'] < 3:
        raise ValueError('at least three independent pairs required')
    args = argparse.Namespace(target_case='', min_benefit_pct=10, max_regression_pct=5, noise_floor_ms=.1, min_samples=100)
    pairs = []
    metadata = None
    for pair in range(context['pairs']):
        before = summary.load(directory / f'pair-{pair}-baseline.jsonl')
        after = summary.load(directory / f'pair-{pair}-candidate.jsonl')
        args.target_case = next((key[0] for key in before[1] if key[-2] == 'warm'), '')
        summary.compare(before, after, args)
        if metadata:
            for key in summary.COMPATIBLE:
                if metadata.get(key) != before[0].get(key):
                    raise ValueError(f'process context changed: {key}')
        metadata = before[0]
        pairs.append((before, after))
    return context, metadata, pairs


def changes(pairs):
    result = {}
    for before, after in pairs:
        for key, rows in before[1].items():
            if key[-2] != 'warm':
                continue
            previous = [r['elapsed_ns'] / 1e6 for r in rows]
            current = [r['elapsed_ns'] / 1e6 for r in after[1][key]]
            record = {}
            for label, q in [('p50', .5), ('p95', .95)]:
                old, new = summary.percentile(previous, q), summary.percentile(current, q)
                record[label + '_ms'] = new - old
                record[label + '_pct'] = ((new / old - 1) * 100) if old else (0 if not new else math.inf)
                record[label + '_before_ms'] = old
                record[label + '_after_ms'] = new
            result.setdefault(key, []).append(record)
    return result


def evaluate(control, experiment=None, target=None):
    cc, cm, cp = control
    if not cc.get('same_binary_control') or len(set(cc['daemon_sha256'].values())) != 1:
        raise ValueError('control requires identical daemon binaries')
    if 'runner_sha256' in cc and len(set(cc['runner_sha256'].values())) != 1:
        raise ValueError('control requires identical runner/client binaries')
    noise = changes(cp)
    envelopes = {key: {label: max(abs(r[label + '_ms']) for r in rows) for label in ['p50', 'p95']}
                 for key, rows in noise.items()}
    report = {'schema': 1, 'control_pairs': len(cp), 'engineering_gate_not_significance_test': True,
              'limits': 'Finite observed A/A envelope; does not bound future noise. Keep raw per-query/process results.',
              'control_cells': [dict(zip(summary.CELL, key), p50_noise_ms=value['p50'], p95_noise_ms=value['p95'])
                                for key, value in sorted(envelopes.items())]}
    if experiment is None:
        return report
    ec, em, ep = experiment
    if not target:
        raise ValueError('predeclared target case required')
    if cc['daemon_sha256']['baseline'] != ec['daemon_sha256']['baseline']:
        raise ValueError('control and experiment require the same baseline binary')
    if cc.get('runner_sha256', {}).get('baseline') != ec.get('runner_sha256', {}).get('baseline'):
        raise ValueError('control and experiment require the same baseline runner')
    for key in summary.COMPATIBLE:
        if cm.get(key) != em.get(key):
            raise ValueError(f'control context differs: {key}')
    delta = changes(ep)
    if set(delta) != set(envelopes):
        raise ValueError('control query/route coverage differs')
    benefits, regressions, cells = [], [], []
    for key, rows in sorted(delta.items()):
        limits = {label: max(.1, envelopes[key][label] * 1.25) for label in ['p50', 'p95']}
        # A target needs consistent improvement in every independent pair.
        improved = key[0] == target and all(r['p50_pct'] <= -10 and r['p50_ms'] <= -limits['p50'] for r in rows)
        # A sustained regression needs two thirds of pairs, above the
        # predeclared relative/absolute and independent observed-noise floors.
        flags = sum(any(r[label + '_pct'] > 5 and r[label + '_ms'] > limits[label]
                        for label in ['p50', 'p95']) for r in rows)
        regressed = flags >= math.ceil(len(rows) * 2 / 3)
        if improved:
            benefits.append(key)
        if regressed:
            regressions.append(key)
        cells.append(dict(zip(summary.CELL, key), pair_changes=rows, noise_floors_ms=limits,
                          consistent_target_benefit=improved, sustained_regression=regressed))
    if not any(key[0] == target for key in delta):
        raise ValueError('target case absent from panel')
    report.update(passed=bool(benefits) and not regressions, target_case=target, experiment_pairs=len(ep),
                  benefit_cells=len(benefits), sustained_regression_cells=len(regressions), cells=cells,
                  rule='target: every pair >=10% and 0.1ms and 1.25x independent A/A envelope; regression: >=2/3 pairs >5% and both floors')
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--control', required=True)
    parser.add_argument('--experiment')
    parser.add_argument('--target-case')
    args = parser.parse_args()
    try:
        if args.experiment and Path(args.control).resolve() == Path(args.experiment).resolve():
            raise ValueError('experiment must be independent of the control panel')
        result = evaluate(panel(args.control), panel(args.experiment) if args.experiment else None, args.target_case)
        json.dump(result, sys.stdout, indent=2)
        print()
        return 0 if result.get('passed', True) else 1
    except (ValueError, OSError, KeyError, TypeError) as error:
        json.dump({'passed': False, 'evidence_error': str(error)}, sys.stdout)
        print()
        return 2


if __name__ == '__main__':
    sys.exit(main())
