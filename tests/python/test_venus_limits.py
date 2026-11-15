"""Prevent stress reports from pooling incompatible inputs or trusting bad clocks."""
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import benchmark_venus_proton
import benchmark_venus_ring
import benchmark_venus_sync
import report_venus_limits
from report_venus_limits import summarize


def trial(mode, rate=100, repetition=1):
    config = dict(width=64,height=64,pipeline=dict(shader_rounds=1,draws_per_frame=1))
    cases = []
    for name in ['d3d12_depth1','d3d12_depth2','d3d12_depth3','mixed_depth3']:
        cases.append(dict(mode=name,iterations_per_second=rate,p95_us=10,
            gpu=dict(execution=dict(mean_us=4),negative_submit_intervals=2,
                negative_completion_intervals=0,submit_to_gpu_begin=dict(mean_us=-900)),
            pipeline=dict(gpu_gap=dict(mean_us=6),gpu_active_fraction=.4,
                retirement_latency=dict(p95_us=20)),
            stages={name:dict(mean_us=1) for name in ['record','submit','event_wait']}))
    return dict(study='study',label=mode,profile='submission',repetition=repetition,
        mode=mode,passed=True,config=config,result=dict(config=copy.deepcopy(config),
            gpu_verified=True,cases=cases))


class VenusLimitsTests(unittest.TestCase):
    def test_completion_comparison_pairs_throughput_and_cpu_cost(self):
        records=[]
        for variant,rate,cpu,repeat in [('current',100,10,1),('feedback200',200,7,1),
                ('current',200,20,2),('feedback200',300,16,2)]:
            record=trial('venus',rate,repeat)
            record.update(variant=variant,repeat=repeat,task_cpu_seconds=cpu)
            records.append(record)
        candidate=next(r for r in benchmark_venus_sync.summarize(records) if r['variant']=='feedback200')
        self.assertEqual(candidate['cases']['d3d12_depth3']['relative'],175)
        self.assertEqual(candidate['cases']['d3d12_depth3']['cpu_relative'],75)
        records[-1]['config']['width']=128
        with self.assertRaisesRegex(ValueError,'fixture settings differ'):
            benchmark_venus_sync.summarize(records)

    def test_completion_report_rejects_different_driver_binaries(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);records=[]
            for variant,driver in [('current','driver-a'),('feedback200','driver-b')]:
                child=root/variant;child.mkdir();setting=benchmark_venus_sync.VARIANTS[variant]
                env=dict(protons=[],workloads={},venus_driver_sha256=driver,
                    renderer_worker_sha256='renderer',bootstrap_sha256='bootstrap',memory_mib=4096,
                    venus_perf=setting['perf'],feedback_spin_us=setting['spin'],ring_spin_us=50,ring_stats=False)
                (child/'environment.json').write_text(json.dumps(env))
                record=trial('venus');record.update(variant=variant,directory=variant,repeat=1,task_cpu_seconds=1)
                records.append(record)
            (root/'measurements.json').write_text(json.dumps(records))
            with self.assertRaisesRegex(ValueError,'different runtime binaries'):
                benchmark_venus_sync.report(root)
            self.assertFalse((root/'report.json').exists())

    def test_feedback_polling_requires_legacy_mode_and_a_bounded_budget(self):
        for extra in [['--feedback-spin-us','50'],['--feedback-spin-us','201','--venus-perf','no_timeline_sync']]:
            args=['benchmark_venus_proton.py','--reference-session','unused','--output','unused',
                '--proton','unused',*extra]
            with self.subTest(extra=extra),patch('sys.argv',args),patch('sys.stderr'), \
                    patch.object(benchmark_venus_proton.vulkan,'prepare') as prepare:
                with self.assertRaises(SystemExit):benchmark_venus_proton.main()
                prepare.assert_not_called()

    def test_ring_budget_is_bounded_before_launching_anything(self):
        for value in ['-1','201']:
            args=['benchmark_venus_proton.py','--reference-session','unused','--output','unused',
                '--proton','unused','--ring-spin-us',value]
            with self.subTest(value=value),patch('sys.argv',args),patch('sys.stderr'), \
                    patch.object(benchmark_venus_proton.vulkan,'prepare') as prepare:
                with self.assertRaises(SystemExit):benchmark_venus_proton.main()
                prepare.assert_not_called()

    def test_limits_report_cannot_pool_different_polling_budgets(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            for name,budget in [('control',0),('candidate',50)]:
                study=root/name;study.mkdir()
                env=dict(protons=[],workloads={},renderer_worker_sha256='renderer',
                    bootstrap_sha256='bootstrap',memory_mib=4096,ring_spin_us=budget)
                (study/'environment.json').write_text(json.dumps(env))
                (study/'measurements.json').write_text('[]')
            with patch('sys.argv',['report_venus_limits.py','--root',str(root),
                    '--study','control','--study','candidate']):
                with self.assertRaisesRegex(SystemExit,'different runtime'):
                    report_venus_limits.main()
            self.assertFalse((root/'limits-report.json').exists())

    def test_ring_comparison_pairs_by_repetition(self):
        records=[]
        for budget,rate,repeat in [(0,100,1),(50,50,1),(0,200,2),(50,400,2)]:
            record=trial('venus',rate,repeat)
            record.update(spin_us=budget,repeat=repeat,task_cpu_seconds=3,trial_wall_seconds=2)
            records.append(record)
        rows=benchmark_venus_ring.summarize(records)
        candidate=next(r for r in rows if r['spin_us']==50)
        self.assertEqual(candidate['cases']['d3d12_depth3']['paired_relative_percent'],125)
        self.assertEqual(candidate['cases']['d3d12_depth3']['pairs'],2)
        records[-1]['config']['width']=128
        with self.assertRaisesRegex(ValueError,'configurations differ'):
            benchmark_venus_ring.summarize(records)

    def test_failed_ring_trial_cannot_be_claimed_as_an_optimisation(self):
        record=trial('venus');record.update(spin_us=50,repeat=1,passed=False)
        with self.assertRaisesRegex(ValueError,'failed trials'):
            benchmark_venus_ring.summarize([record])

    def test_draw_stress_cannot_silently_run_the_single_draw_dxvk_fixture(self):
        args=['benchmark_venus_proton.py','--reference-session','unused','--output','unused',
            '--proton','unused','--apis','d3d11','--profiles','draws64']
        with patch('sys.argv',args),patch('sys.stderr'),patch.object(benchmark_venus_proton.vulkan,'prepare') as prepare:
            with self.assertRaises(SystemExit) as error:
                benchmark_venus_proton.main()
            self.assertEqual(error.exception.code,2)
            prepare.assert_not_called()

    def test_ratios_are_paired_before_taking_the_median(self):
        records = [trial('venus',50,1),trial('nvidia',100,1),
            trial('venus',400,2),trial('nvidia',200,2)]
        rows, failures = summarize(records)
        ratio = rows[0]['paired_relative_percent']['d3d12_depth3']
        self.assertEqual(ratio,dict(pairs=2,median=125,minimum=50,maximum=200))
        self.assertEqual(failures,[])
        timing = rows[0]['modes']['venus']['d3d12_depth3']
        self.assertEqual(timing['gpu_execution_us'],4)
        self.assertEqual(timing['invalid_cross_clock_intervals'],4)
        self.assertNotIn('submit_to_gpu_begin',timing)

    def test_different_settings_cannot_be_pooled(self):
        first = trial('venus');second = trial('nvidia')
        second['config']['width'] = 1920
        second['result']['config']['width'] = 1920
        with self.assertRaisesRegex(ValueError,'different fixture settings'):
            summarize([first,second])

    def test_failed_trials_do_not_become_zero_throughput_samples(self):
        failed = dict(study='study',label='failed',passed=False,error='device lost')
        rows, failures = summarize([trial('venus'),failed])
        self.assertEqual(len(failures),1)
        self.assertEqual(rows[0]['paired_relative_percent'],{})

    def test_success_marker_requires_matching_correctness_evidence(self):
        record = trial('venus');record['result']['gpu_verified'] = False
        with self.assertRaisesRegex(ValueError,'inconsistent correctness'):
            summarize([record])


if __name__ == '__main__':
    unittest.main()
