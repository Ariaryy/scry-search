import copy
import importlib.util
from pathlib import Path
import unittest
import tempfile
import json
from test_benchmark_summary import capture

spec = importlib.util.spec_from_file_location('evidence', Path(__file__).parents[1] / 'benchmark-evidence.py')
evidence = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evidence)


def control():
    pairs = [(capture(), capture(11_000_000)), (capture(), capture(9_000_000)), (capture(), capture())]
    return ({'same_binary_control': True, 'daemon_sha256': {'baseline': 'base', 'candidate': 'base'}}, pairs[0][0][0], pairs)


def candidate(ns=8_000_000):
    pairs = [(capture(), capture(ns)) for _ in range(3)]
    return ({'same_binary_control': False, 'daemon_sha256': {'baseline': 'base', 'candidate': 'candidate'}}, pairs[0][0][0], pairs)


class PairedEvidenceTests(unittest.TestCase):
    def test_completed_panel_checks_real_capture_files_and_pair_coverage(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            (root/'context.json').write_text(json.dumps({'pairs':3}),encoding='utf-8')
            (root/'complete.json').write_text('{"complete":true}',encoding='utf-8')
            for pair in range(3):
                for label in ['baseline','candidate']:
                    metadata,cells,_=capture()
                    rows=[{'type':'metadata',**metadata}]
                    rows.extend({'type':'verification','round':r,'case':'target','requested':'rpc','ordered_equal':True} for r in range(3))
                    rows.extend({'type':'sample',**row} for row in next(iter(cells.values())))
                    rows.append({'type':'complete'})
                    (root/f'pair-{pair}-{label}.jsonl').write_text('\n'.join(json.dumps(r) for r in rows),encoding='utf-8')
            self.assertEqual(len(evidence.panel(root)[2]),3)
            (root/'pair-2-candidate.jsonl').unlink()
            with self.assertRaises(OSError):
                evidence.panel(root)

    def test_consistent_benefit_exceeds_independent_observed_noise(self):
        result = evidence.evaluate(control(), candidate(), 'target')
        self.assertTrue(result['passed'])
        self.assertTrue(result['engineering_gate_not_significance_test'])

    def test_noise_sized_or_inconsistent_benefit_is_not_a_win(self):
        self.assertFalse(evidence.evaluate(control(), candidate(9_500_000), 'target')['passed'])
        changed = candidate()
        changed[2][-1] = (capture(), capture())
        self.assertFalse(evidence.evaluate(control(), changed, 'target')['passed'])

    def test_sustained_tail_regression_cannot_hide_under_median_benefit(self):
        changed = candidate()
        for _, after in changed[2][:2]:
            for row in next(iter(after[1].values()))[:10]:
                row['elapsed_ns'] = 20_000_000
        self.assertFalse(evidence.evaluate(control(), changed, 'target')['passed'])

    def test_context_and_baseline_identity_must_match(self):
        changed = candidate()
        changed[1]['machine'] = 'other'
        with self.assertRaisesRegex(ValueError, 'context'):
            evidence.evaluate(control(), changed, 'target')
        changed = candidate()
        changed[0]['daemon_sha256']['baseline'] = 'other'
        with self.assertRaisesRegex(ValueError, 'baseline'):
            evidence.evaluate(control(), changed, 'target')

    def test_control_requires_identical_binaries_and_target_must_exist(self):
        invalid = copy.deepcopy(control())
        invalid[0]['same_binary_control'] = False
        with self.assertRaisesRegex(ValueError, 'identical'):
            evidence.evaluate(invalid)
        invalid=copy.deepcopy(control())
        invalid[0]['runner_sha256']={'baseline':'old-client','candidate':'new-client'}
        with self.assertRaisesRegex(ValueError,'runner/client'):
            evidence.evaluate(invalid)
        with self.assertRaisesRegex(ValueError, 'absent'):
            evidence.evaluate(control(), candidate(), 'missing')


if __name__ == '__main__':
    unittest.main()
