#!/usr/bin/env python3
"""Interleaved Venus completion-path tests with identical guest/host binaries."""
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
VARIANTS={
    'current':dict(perf=[],spin=0),
    'feedback50':dict(perf=['no_timeline_sync','no_query_feedback'],spin=50),
    'feedback200':dict(perf=['no_timeline_sync','no_query_feedback'],spin=200),
    'feedback200-query':dict(perf=['no_timeline_sync'],spin=200),
    'query-off':dict(perf=['no_query_feedback'],spin=0),
}


def summarize(records):
    rows=[]
    for profile in dict.fromkeys(r['profile'] for r in records):
        for variant in dict.fromkeys(r['variant'] for r in records):
            selected=[r for r in records if r['profile']==profile and r['variant']==variant]
            if not selected:continue
            if not all(r['passed'] and r['result']['gpu_verified'] for r in selected):
                raise ValueError('failed/correctness-unverified trials cannot be scored')
            row=dict(profile=profile,variant=variant,trials=len(selected),cases={},
                cpu_seconds=statistics.median(r['task_cpu_seconds'] for r in selected))
            for mode in ['d3d12_depth1','d3d12_depth2','d3d12_depth3','mixed_depth3']:
                def case(record):return next(c for c in record['result']['cases'] if c['mode']==mode)
                rates=[case(r)['iterations_per_second'] for r in selected];ratios=[];cpu_ratios=[]
                for record in selected:
                    controls=[r for r in records if r['profile']==profile and r['variant']=='current' and r['repeat']==record['repeat']]
                    if len(controls)>1:raise ValueError('duplicate control in a paired repetition')
                    if not controls:continue
                    control=controls[0]
                    if record['config']!=control['config']:raise ValueError('paired fixture settings differ')
                    ratios.append(100*case(record)['iterations_per_second']/case(control)['iterations_per_second'])
                    cpu_ratios.append(100*record['task_cpu_seconds']/control['task_cpu_seconds'])
                row['cases'][mode]=dict(rate=statistics.median(rates),minimum=min(rates),maximum=max(rates),
                    relative=statistics.median(ratios) if ratios else None,
                    relative_minimum=min(ratios) if ratios else None,relative_maximum=max(ratios) if ratios else None,
                    pairs=len(ratios),cpu_relative=statistics.median(cpu_ratios) if cpu_ratios else None,
                    gpu_us=statistics.median(case(r)['gpu']['execution']['mean_us'] for r in selected),
                    gpu_gap_us=statistics.median(case(r)['pipeline']['gpu_gap']['mean_us'] for r in selected),
                    p95_cadence_us=statistics.median(case(r)['p95_us'] for r in selected))
            rows.append(row)
    return rows


def report(output):
    records=json.loads((output/'measurements.json').read_text());identities=[]
    for record in records:
        env=json.loads((output/record['directory']/'environment.json').read_text())
        variant=VARIANTS[record['variant']]
        if env['venus_perf']!=variant['perf'] or env['feedback_spin_us']!=variant['spin'] or env['ring_spin_us']!=50 or env['ring_stats']:
            raise ValueError('actual tuning differs from the declared variant')
        if record['result']['config']!=record['config']:raise ValueError('saved correctness configuration differs')
        identities.append({k:env[k] for k in ['protons','workloads','venus_driver_sha256','renderer_worker_sha256','bootstrap_sha256','memory_mib']})
    if len({json.dumps(v,sort_keys=True) for v in identities})!=1:
        raise ValueError('variants use different runtime binaries')
    rows=summarize(records);plan=json.loads((output/'plan.json').read_text())
    result=dict(schema=1,trials=len(records),passed=sum(r['passed'] for r in records),expected_trials=plan['expected_trials'],
        inputs=identities[0],variants=VARIANTS,results=rows,
        cpu_scope='inherited perf task-clock:u over complete trial, including startup and validation; not steady-state CPU or whole system CPU',
        scope='offscreen D3D12 shaders with real fence and pixel checks; same binaries, interleaved configuration changes',
        limits=['Single host GPU/driver, unlocked clocks and active desktop.',
            'Shared feedback uses Mesa’s existing semaphore path; polling changes its initial backoff only.',
            'no_query_feedback changes query-result transport; timestamps are checked but this is not a complete query-type qualification.',
            'CPU counters include different numbers of completed frames during fixed-length sampling.',
            'Reported ranges are observed repeats, not confidence intervals.'])
    validation_html=''
    validation_path=output/'validation.json'
    if validation_path.exists():
        validation=json.loads(validation_path.read_text())
        if validation['inputs']!=result['inputs']:
            raise ValueError('validation uses different runtime inputs')
        result['validation']=validation
        validation_html=f'''<h2>Application, heavy-load and component checks</h2>
<p>Fresh Unreal runs completed with visible rendering and exit code zero: {validation['game_control_fps']:.2f} FPS for the current control and {validation['game_feedback_fps']:.2f} FPS for feedback200. This single pair does not establish a game FPS improvement.</p>
<p>The 4K shader pair passed; feedback200 measured {validation['heavy_relative_percent']:.1f}% of control throughput at three frames in flight. In a separate three-repeat light-1080p study, query transport alone measured {validation['query_only_relative_percent']:.1f}% of its control and shared feedback with query transport unchanged measured {validation['feedback_only_relative_percent']:.1f}%. The combined configuration gave the larger improvement in the main study.</p>
<p><a href="../sync-heavy/report.html">Heavy-load study</a> · <a href="../sync-components/report.html">Component study</a> · <a href="../ue5-feedback200/result.json">Application checks</a> · <a href="../ue5-sync-control/result.json">Application control</a> · <a href="../ue5-feedback200/vm-020.png">Rendered scene</a> · <a href="../feedback-probe-repro/build.json">Fresh build provenance</a> · <a href="../feedback-repro-check/measurements.json">Fresh build shader check</a></p>'''
    write(output/'report.json',result)
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    profiles=list(dict.fromkeys(r['profile'] for r in rows));variants=list(dict.fromkeys(r['variant'] for r in rows))
    fig,axes=plt.subplots(1,len(profiles),figsize=(6*len(profiles),4.7),squeeze=False)
    for ax,profile in zip(axes[0],profiles):
        for variant in variants:
            row=next((r for r in rows if r['profile']==profile and r['variant']==variant),None)
            if not row:continue
            cases=[row['cases'][f'd3d12_depth{n}'] for n in [1,2,3]];rates=[c['rate'] for c in cases]
            ax.errorbar([1,2,3],rates,yerr=[[v-c['minimum'] for v,c in zip(rates,cases)],
                [c['maximum']-v for v,c in zip(rates,cases)]],marker='o',capsize=3,label=variant)
        ax.set_title(profile);ax.set_xticks([1,2,3]);ax.set_xlabel('Frames in flight');ax.set_ylabel('Completed synthetic frames/s');ax.legend()
    fig.suptitle('Venus completion path — same binaries, interleaved repeats');fig.tight_layout()
    fig.savefig(output/'comparison.svg');fig.savefig(output/'comparison.png',dpi=150)
    table=[]
    for row in rows:
        one=row['cases']['d3d12_depth1'];three=row['cases']['d3d12_depth3'];ratio=three['relative']
        relative='—' if ratio is None else f'{ratio:.1f}% ({three["relative_minimum"]:.1f}–{three["relative_maximum"]:.1f})'
        table.append(f'<tr><td>{row["profile"]}</td><td>{row["variant"]}</td><td>{row["trials"]}</td><td>{one["rate"]:,.0f}</td><td>{three["rate"]:,.0f}</td><td>{relative}</td><td>{three["gpu_gap_us"]:.1f}</td><td>{row["cpu_seconds"]:.1f}</td></tr>')
    sources=''.join(f'<li><a href="{html.escape(r["directory"])}/environment.json">{html.escape(r["directory"])}</a> · <a href="{html.escape(r["directory"])}-cpu.csv">CPU counter</a></li>' for r in records)
    page='''<!doctype html><meta charset="utf-8"><title>Venus completion path</title><style>body{font:16px system-ui;max-width:1250px;margin:36px auto;padding:0 20px;line-height:1.5;color:#20232b}img{max-width:100%}td,th{padding:10px;border-bottom:1px solid #ddd;text-align:left}table{border-collapse:collapse;width:100%}a{color:#1e5a99}.scroll{overflow:auto}</style>
<h1>Venus completion path</h1><p>All variants use the same experimental Mesa driver, renderer, Proton and executable. The current control uses timeline sync and the previously tested 50 µs host-ring budget. Feedback variants select Mesa's existing no_timeline_sync path plus bounded guest polling; variants without the -query suffix also select no_query_feedback. These options change result transport, not query support. They retain actual semaphore values, timeouts and every validation image check.</p>
<p>'''+f'{result["passed"]}/{result["trials"]} saved trials passed; {result["expected_trials"]} planned.'+'''</p><img src="comparison.svg" alt="Completion throughput by configuration and queue depth"><div class="scroll"><table><tr><th>Profile</th><th>Variant</th><th>Repeats</th><th>Depth 1 frames/s</th><th>Depth 3 frames/s</th><th>Paired relative</th><th>GPU gap µs</th><th>Whole-trial CPU seconds</th></tr>'''+''.join(table)+'''</table></div><p>CPU counters include startup and validation. Percentages are paired before taking medians; whiskers show observed ranges.</p><h2>Limits</h2><ul>'''+''.join('<li>'+html.escape(x)+'</li>' for x in result['limits'])+'''</ul><h2>Evidence</h2><p><a href="report.json">Summary</a> · <a href="measurements.json">Measurements</a> · <a href="plan.json">Plan</a></p><ul>'''+sources+'''</ul>'''
    if validation_html:page=page.replace('<h2>Limits</h2>',validation_html+'<h2>Limits</h2>')
    (output/'report.html').write_text(page);print(output/'report.html')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',required=True,type=Path)
    parser.add_argument('--reference-session',type=Path)
    parser.add_argument('--proton',type=Path)
    parser.add_argument('--profiles',nargs='+',choices=['submission256','light1080','draws64','shader4k'],default=['submission256','light1080'])
    parser.add_argument('--variants',nargs='+',choices=list(VARIANTS),
        default=['current','query-off','feedback50','feedback200','feedback200-query'])
    parser.add_argument('--repetitions',type=int,default=3)
    parser.add_argument('--report-only',action='store_true')
    args=parser.parse_args();output=args.output.resolve()
    if args.report_only:report(output);return
    if not args.reference_session or not args.proton:parser.error('provide reference session and Proton')
    if 'current' not in args.variants or len(set(args.variants))!=len(args.variants) or len(set(args.profiles))!=len(args.profiles) or not 1<=args.repetitions<=5:
        parser.error('use unique profiles and variants, including current, and 1..5 repeats')
    if output.exists() or not output.is_relative_to(ROOT/'build'):parser.error('use a fresh output under build')
    output.mkdir(parents=True)
    base=[sys.executable,str(ROOT/'tools/benchmark_venus_proton.py'),'--reference-session',str(args.reference_session.resolve()),
        '--proton',str(args.proton.resolve()),'--renderer-prefix',str(ROOT/'build/venus/ring-probe/prefix'),
        '--ring-spin-us','50','--venus-driver',str(ROOT/'build/venus/feedback-probe/stage/usr/lib/x86_64-linux-gnu/libvulkan_virtio.so'),
        '--apis','d3d12','--modes','venus','--repetitions','1','--warmup-ms','500','--sample-ms','1500',
        '--memory-mib','4096','--timeout','300','--d3d12',str(ROOT/'build/venus/windows-caps/x86_64-pc-windows-msvc/release/legion-proton-workload.exe')]
    write(output/'plan.json',dict(schema=1,variants=args.variants,profiles=args.profiles,repetitions=args.repetitions,
        expected_trials=len(args.variants)*len(args.profiles)*args.repetitions,command=base))
    records=[]
    for repeat in range(1,args.repetitions+1):
        cases=[(p,v) for p in args.profiles for v in args.variants];random.Random(300+repeat).shuffle(cases)
        for profile,variant in cases:
            name=f'rep{repeat:02d}-{profile}-{variant}';config=VARIANTS[variant];start=time.monotonic()
            command=['perf','stat','-x,','-e','task-clock:u','-o',str(output/(name+'-cpu.csv')),'--',*base,
                '--output',str(output/name),'--profiles',profile,'--feedback-spin-us',str(config['spin'])]
            if config['perf']:command+=['--venus-perf',*config['perf']]
            with (output/(name+'.log')).open('w') as log:code=subprocess.run(command,stdout=log,stderr=subprocess.STDOUT).returncode
            if code:raise SystemExit(f'{name} failed with {code}; inspect log')
            record=json.loads((output/name/'measurements.json').read_text())[0]
            cpu=next(float(s.split(',')[0])/1000 for s in (output/(name+'-cpu.csv')).read_text().splitlines() if ',task-clock:u,' in s)
            record.update(repeat=repeat,variant=variant,directory=name,task_cpu_seconds=cpu,trial_wall_seconds=time.monotonic()-start)
            records.append(record);write(output/'measurements.json',records);print(name,'PASS',flush=True)
    report(output)


if __name__=='__main__':main()
