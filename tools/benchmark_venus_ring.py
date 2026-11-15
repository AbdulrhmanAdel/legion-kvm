#!/usr/bin/env python3
"""Interleave Venus ring-poll budgets with identical binaries and shader checks."""
import argparse
import html
import json
from pathlib import Path
import random
import statistics
import subprocess
import sys
import time

from benchmark_venus import write

ROOT=Path(__file__).resolve().parents[1]


def summarize(records):
    rows=[]
    profiles=list(dict.fromkeys(r['profile'] for r in records))
    budgets=sorted({r['spin_us'] for r in records})
    for profile in profiles:
        for budget in budgets:
            selected=[r for r in records if r['profile']==profile and r['spin_us']==budget]
            if not selected:continue
            if not all(r['passed'] for r in selected):
                raise ValueError('failed trials cannot be scored as an optimisation')
            entry=dict(profile=profile,spin_us=budget,trials=len(selected),cases={})
            for mode in ['d3d12_depth1','d3d12_depth2','d3d12_depth3','mixed_depth3']:
                def case(r):return next(c for c in r['result']['cases'] if c['mode']==mode)
                rates=[case(r)['iterations_per_second'] for r in selected]
                ratios=[]
                for record in selected:
                    controls=[r for r in records if r['profile']==profile and r['spin_us']==0 and r['repeat']==record['repeat']]
                    if len(controls)>1:raise ValueError('duplicate control within a paired repetition')
                    if len(controls)!=1:continue
                    control=controls[0]
                    if record['config']!=control['config']:
                        raise ValueError('paired fixture configurations differ')
                    ratios.append(100*case(record)['iterations_per_second']/case(control)['iterations_per_second'])
                entry['cases'][mode]=dict(frames_per_second=statistics.median(rates),
                    minimum=min(rates),maximum=max(rates),
                    paired_relative_percent=statistics.median(ratios) if ratios else None,
                    paired_minimum=min(ratios) if ratios else None,paired_maximum=max(ratios) if ratios else None,
                    pairs=len(ratios),
                    gpu_gap_us=statistics.median(case(r)['pipeline']['gpu_gap']['mean_us'] for r in selected),
                    p95_cadence_us=statistics.median(case(r)['p95_us'] for r in selected))
            entry['whole_trial_cpu_seconds']=statistics.median(r['task_cpu_seconds'] for r in selected)
            entry['whole_trial_wall_seconds']=statistics.median(r['trial_wall_seconds'] for r in selected)
            entry['cpu_scope']='inherited perf task-clock:u over entire launcher trial, including startup and validation; not total system CPU or steady-state CPU'
            rows.append(entry)
    return rows


def report(output):
    records=json.loads((output/'measurements.json').read_text())
    inputs=[]
    guest_policies=[]
    for record in records:
        env=json.loads((output/record['directory']/'environment.json').read_text())
        if env['ring_stats'] or env['ring_spin_us']!=record['spin_us']:
            raise ValueError('counter instrumentation or polling budget differs from the run plan')
        inputs.append({key:env[key] for key in ['protons','workloads','renderer_worker_sha256','bootstrap_sha256','memory_mib']})
        guest_policies.append(dict(venus_perf=env.get('venus_perf',[]),
            feedback_spin_us=env.get('feedback_spin_us',0),venus_driver_sha256=env.get('venus_driver_sha256')))
        if not record['result']['gpu_verified'] or record['result']['config']!=record['config']:
            raise ValueError('correctness evidence differs from the scored workload')
    if len({json.dumps(value,sort_keys=True) for value in inputs})!=1:
        raise ValueError('A/B inputs differ beyond the polling budget')
    if len({json.dumps(value,sort_keys=True) for value in guest_policies})!=1:
        raise ValueError('guest synchronisation settings differ in a ring-only comparison')
    rows=summarize(records)
    result=dict(schema=1,trials=len(records),passed=sum(r['passed'] for r in records),results=rows,
        inputs=inputs[0],plan=json.loads((output/'plan.json').read_text()),
        scope='same renderer binary; only bounded host-ring polling budget differs; no ring counters during these measurements',
        limits=['Offscreen D3D12 integer shader; not game FPS.',
            'Single host GPU and driver; clocks unlocked and desktop active.',
            'Polling can consume more CPU. CPU counters include startup and validation; they are not per-frame or steady-state CPU.',
            'All fence and semaphore completion checks remain enabled; no fake completion or timing override.'])
    validation_path=output/'validation.json'
    validation_html=''
    if validation_path.exists():
        validation=json.loads(validation_path.read_text())
        if validation['renderer_worker_sha256']!=inputs[0]['renderer_worker_sha256']:
            raise ValueError('application/large-workload validation uses a different renderer')
        result['validation']=validation
        validation_html=f'''<h2>Large-workload and application checks</h2>
<p>The 4K shader pair passed at both budgets. The 50 µs run achieved {validation['heavy_relative_percent']:.1f}% of its zero-budget control; this single pair does not establish an improvement for GPU-bound work.</p>
<p>Unreal completed with visible rendering and exit code zero at {validation['application_fps']:.2f} FPS, versus {validation['historical_application_fps']:.2f} FPS in the earlier Venus run. The earlier result is historical, not an interleaved game control. No shader-model override or CPU adapter was used.</p>
<p><a href="../ring-heavy/report.html">4K measurements</a> · <a href="../ue5-ring50/result.json">Application checks</a> · <a href="../ue5-ring50/application/BenchmarkResults.txt">Application FPS</a> · <a href="../ue5-ring50/vm-020.png">Rendered scene</a> · <a href="../ring-probe/build.json">Renderer source and patch provenance</a></p>'''
    write(output/'report.json',result)
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    profiles=list(dict.fromkeys(r['profile'] for r in rows));budgets=sorted({r['spin_us'] for r in rows})
    fig,axes=plt.subplots(1,len(profiles),figsize=(6*len(profiles),4.6),squeeze=False)
    for ax,profile in zip(axes[0],profiles):
        for budget in budgets:
            row=next((r for r in rows if r['profile']==profile and r['spin_us']==budget),None)
            if not row:continue
            cases=[row['cases'][f'd3d12_depth{depth}'] for depth in [1,2,3]]
            rates=[c['frames_per_second'] for c in cases]
            ax.errorbar([1,2,3],rates,yerr=[[v-c['minimum'] for v,c in zip(rates,cases)],
                [c['maximum']-v for v,c in zip(rates,cases)]],marker='o',capsize=4,label=f'{budget} µs')
        ax.set_title(profile);ax.set_xlabel('Frames in flight');ax.set_ylabel('Completed synthetic frames/s');ax.set_xticks([1,2,3]);ax.legend(title='Polling budget')
    fig.suptitle('Venus ring polling — interleaved repeats, unchanged renderer binary')
    fig.tight_layout();fig.savefig(output/'comparison.svg');fig.savefig(output/'comparison.png',dpi=150)
    table=[]
    for row in rows:
        one=row['cases']['d3d12_depth1'];three=row['cases']['d3d12_depth3']
        gain=three['paired_relative_percent']
        ratio='—' if gain is None else f'{gain:.1f}% ({three["paired_minimum"]:.1f}–{three["paired_maximum"]:.1f})'
        table.append(f'<tr><td>{row["profile"]}</td><td>{row["spin_us"]}</td><td>{row["trials"]}</td><td>{one["frames_per_second"]:,.0f}</td><td>{three["frames_per_second"]:,.0f}</td><td>{ratio}</td><td>{three["gpu_gap_us"]:.1f}</td><td>{row["whole_trial_cpu_seconds"]:.1f}</td></tr>')
    sources=''.join(f'<li><a href="{html.escape(r["directory"])}/environment.json">{html.escape(r["directory"])}</a> · <a href="{html.escape(r["directory"])}-cpu.csv">CPU counter</a></li>' for r in records)
    page='''<!doctype html><meta charset="utf-8"><title>Venus ring polling experiment</title>
<style>body{font:16px system-ui;max-width:1250px;margin:36px auto;padding:0 20px;line-height:1.5;color:#20232b}img{max-width:100%}td,th{padding:10px;border-bottom:1px solid #ddd;text-align:left}table{border-collapse:collapse;width:100%}a{color:#1e5a99}.scroll{overflow:auto}</style>
<h1>Reducing Venus command-ring overhead</h1><p>The experiment replaces the first bounded period of scheduler yields after a command batch with CPU pause instructions. After that deadline, the original backoff and idle notification scheme continues. Zero uses the original policy; no fence completion is inferred from polling.</p>
<p>The same renderer, selected Proton, shader and guest configuration are used for every budget. Each repetition shuffles the order of profiles and budgets. Ring counters are disabled. Full shader image checks and real fence waits remain enabled.</p>
<img src="comparison.svg" alt="Completed throughput by queue depth and polling budget">
<div class="scroll"><table><tr><th>Profile</th><th>Budget µs</th><th>Repeats</th><th>Depth 1 frames/s</th><th>Depth 3 frames/s</th><th>Paired relative, depth 3</th><th>GPU gap µs</th><th>Whole-trial CPU seconds</th></tr>'''+''.join(table)+'''</table></div>
<p>Percentages are paired within a repetition before taking the median. Whiskers show observed ranges, not confidence intervals. CPU counters include startup and validation and are not a steady-state CPU cost per frame.</p><h2>Limits</h2><ul>'''+''.join('<li>'+html.escape(v)+'</li>' for v in result['limits'])+'''</ul><h2>Evidence</h2><p><a href="report.json">Summary JSON</a> · <a href="measurements.json">Saved trial measurements</a> · <a href="plan.json">Run plan</a></p><ul>'''+sources+'''</ul>'''
    if validation_html:
        page=page.replace('<h2>Limits</h2>',validation_html+'<h2>Limits</h2>')
    (output/'report.html').write_text(page)
    print(output/'report.html')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',required=True,type=Path)
    parser.add_argument('--reference-session',type=Path)
    parser.add_argument('--proton',type=Path)
    parser.add_argument('--renderer-prefix',type=Path,default=ROOT/'build/venus/ring-probe/prefix')
    parser.add_argument('--profiles',nargs='+',choices=['submission256','light1080','draws64','shader4k'],default=['submission256','light1080'])
    parser.add_argument('--budgets',nargs='+',type=int,default=[0,20,50])
    parser.add_argument('--repetitions',type=int,default=3)
    parser.add_argument('--report-only',action='store_true')
    args=parser.parse_args();output=args.output.resolve()
    if args.report_only:report(output);return
    if not args.reference_session or not args.proton:parser.error('provide --reference-session and --proton')
    if not 1<=args.repetitions<=5 or 0 not in args.budgets or len(set(args.budgets))!=len(args.budgets) or len(set(args.profiles))!=len(args.profiles) or any(not 0<=v<=200 for v in args.budgets):
        parser.error('use 1..5 repeats and unique budgets in 0..200, including the zero control')
    if output.exists() or not output.is_relative_to(ROOT/'build'):parser.error('use a fresh directory under build')
    output.mkdir(parents=True)
    base=[sys.executable,str(ROOT/'tools/benchmark_venus_proton.py'),'--reference-session',str(args.reference_session.resolve()),
        '--proton',str(args.proton.resolve()),'--renderer-prefix',str(args.renderer_prefix.resolve()),'--apis','d3d12',
        '--modes','venus','--repetitions','1','--warmup-ms','500','--sample-ms','1500','--memory-mib','4096','--timeout','300',
        '--d3d12',str(ROOT/'build/venus/windows-caps/x86_64-pc-windows-msvc/release/legion-proton-workload.exe')]
    write(output/'plan.json',dict(schema=1,profiles=args.profiles,budgets=args.budgets,repetitions=args.repetitions,
        expected_trials=len(args.profiles)*len(args.budgets)*args.repetitions,command=base))
    records=[]
    for repeat in range(1,args.repetitions+1):
        cases=[(profile,budget) for profile in args.profiles for budget in args.budgets]
        random.Random(100+repeat).shuffle(cases)
        for profile,budget in cases:
            name=f'rep{repeat:02d}-{profile}-spin{budget}'
            start=time.monotonic()
            command=['perf','stat','-x,','-e','task-clock:u','-o',str(output/(name+'-cpu.csv')),'--',
                *base,'--profiles',profile,'--ring-spin-us',str(budget),'--output',str(output/name)]
            with (output/(name+'.log')).open('w') as log:code=subprocess.run(command,stdout=log,stderr=subprocess.STDOUT).returncode
            if code:raise SystemExit(f'{name} failed with {code}; inspect its log')
            record=json.loads((output/name/'measurements.json').read_text())[0]
            cpu=next(float(line.split(',')[0])/1000 for line in (output/(name+'-cpu.csv')).read_text().splitlines() if ',task-clock:u,' in line)
            record.update(repeat=repeat,spin_us=budget,directory=name,task_cpu_seconds=cpu,
                trial_wall_seconds=time.monotonic()-start)
            records.append(record);write(output/'measurements.json',records)
            print(name,'PASS',flush=True)
    report(output)


if __name__=='__main__':main()
