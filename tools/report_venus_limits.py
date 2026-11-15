#!/usr/bin/env python3
"""Report saved D3D12 stress trials; never launch a workload."""
import argparse
import html
import json
from pathlib import Path
import statistics

MODES = ['venus', 'nvidia', 'native']


def summarize(records):
    groups = {}
    failures = []
    for record in records:
        if not record['passed']:
            failures.append(dict(study=record['study'],label=record['label'],error=record.get('error'),
                detail=record.get('failure_detail')))
            continue
        result = record['result']
        if not result['gpu_verified'] or result['config'] != record['config']:
            raise ValueError('saved trial has inconsistent correctness or configuration evidence')
        profile = record['profile']
        group = groups.setdefault(profile, dict(profile=profile,config=record['config'],trials=[]))
        if group['config'] != record['config']:
            raise ValueError('different fixture settings cannot be pooled under one profile')
        group['trials'].append(record)
    rows = []
    for profile, group in groups.items():
        trials = group['trials']
        row = dict(profile=profile,config=group['config'],modes={},paired_relative_percent={})
        for mode in MODES:
            selected = [r for r in trials if r['mode']==mode]
            if not selected:
                continue
            cases = {}
            for name in ['d3d12_depth1','d3d12_depth2','d3d12_depth3','mixed_depth3']:
                values = [next(c for c in r['result']['cases'] if c['mode']==name) for r in selected]
                def median(get):
                    return statistics.median(get(c) for c in values)
                cases[name] = dict(trials=len(values),
                    frames_per_second=median(lambda c:c['iterations_per_second']),
                    min_frames_per_second=min(c['iterations_per_second'] for c in values),
                    max_frames_per_second=max(c['iterations_per_second'] for c in values),
                    p95_cadence_us=median(lambda c:c['p95_us']),
                    gpu_execution_us=median(lambda c:c['gpu']['execution']['mean_us']),
                    gpu_gap_us=median(lambda c:c['pipeline']['gpu_gap']['mean_us']),
                    gpu_active_percent=100*median(lambda c:c['pipeline']['gpu_active_fraction']),
                    retirement_p95_us=median(lambda c:c['pipeline']['retirement_latency']['p95_us']),
                    record_us=median(lambda c:c['stages']['record']['mean_us']),
                    submit_us=median(lambda c:c['stages']['submit']['mean_us']),
                    event_wait_us=median(lambda c:c['stages']['event_wait']['mean_us']),
                    invalid_cross_clock_intervals=sum(c['gpu']['negative_submit_intervals']+
                        c['gpu']['negative_completion_intervals'] for c in values))
            row['modes'][mode] = cases
        for name in ['d3d12_depth1','d3d12_depth3','mixed_depth3']:
            ratios = []
            for venus in [r for r in trials if r['mode']=='venus']:
                controls = [r for r in trials if r['mode']=='nvidia' and r['study']==venus['study']
                    and r['repetition']==venus['repetition']]
                if len(controls)>1:
                    raise ValueError('duplicate control trial in a paired repetition')
                if controls:
                    def rate(r):
                        return next(c['iterations_per_second'] for c in r['result']['cases'] if c['mode']==name)
                    ratios.append(100*rate(venus)/rate(controls[0]))
            if ratios:
                row['paired_relative_percent'][name] = dict(pairs=len(ratios),median=statistics.median(ratios),
                    minimum=min(ratios),maximum=max(ratios))
        rows.append(row)
    return rows, failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',type=Path,default=Path('build/venus'))
    parser.add_argument('--study',action='append',default=[])
    args = parser.parse_args()
    root = args.root.resolve()
    studies = args.study or ['limits-screen']
    records = []
    environments = {}
    for name in studies:
        directory = root/name
        environments[name] = json.loads((directory/'environment.json').read_text())
        for record in json.loads((directory/'measurements.json').read_text()):
            records.append(dict(record,study=name))
    identities = {json.dumps(dict({key:e[key] for key in ['protons','workloads','renderer_worker_sha256',
        'bootstrap_sha256','memory_mib']},ring_spin_us=e.get('ring_spin_us',0),
        ring_stats=e.get('ring_stats',False),venus_perf=e.get('venus_perf',[]),
        feedback_spin_us=e.get('feedback_spin_us',0),venus_driver_sha256=e.get('venus_driver_sha256')),
        sort_keys=True) for e in environments.values()}
    if len(identities)!=1:
        raise SystemExit('Selected studies use different runtime, workload or memory inputs; report them separately')
    rows, failures = summarize(records)
    order = ['submission','submission256','draws64','light1080','shader64','shader256','light4k','shader4k','overdraw64','cpu1m']
    rows.sort(key=lambda r:order.index(r['profile']))
    report = dict(schema=1,scope='owned offscreen D3D12/VKD3D stress sweep; not game FPS or a complete Vulkan feature test',
        trials=len(records),passed=sum(r['passed'] for r in records),failures=failures,results=rows,
        expected_trials=sum(e.get('expected_trials',0) for e in environments.values()),
        environments=environments,
        limits=['Single GPU, driver and selected x86_64 Proton runtime; clocks unlocked and desktop active.',
            'Warmup and sampling exclude startup and full-image verification; each trial has fresh prefix/cache.',
            'Repeated draws cover the same full-screen target; this is overdraw and command recording, not unique materials.',
            'The shader uses deterministic integer arithmetic compiled to shader model 5.0 bytecode.',
            'No textures, upload/readback bandwidth timing, descriptor churn, VRAM exhaustion, ray tracing or SM 6.x feature qualification.',
            'GPU execution and gap use same-clock timestamps. CPU-to-GPU calibrated intervals are excluded because they can be negative.',
            'Venus requires the experimental sync-file wait patch; both guest backends use identical patched Proton graphics.',
            'Maximum reported throughput is bounded by these settings, not the absolute hardware or protocol limit.'])
    selection=root/'limits-followup-selection.json'
    if selection.exists():report['follow_up_selection']=json.loads(selection.read_text())
    (root/'limits-report.json').write_text(json.dumps(report,indent=2)+'\n')
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    fig, axes = plt.subplots(2,1,figsize=(12,8),sharex=True)
    x = list(range(len(rows)))
    colors = {'venus':'#3878ad','nvidia':'#60864b'}
    for offset, mode in [(-.18,'nvidia'),(.18,'venus')]:
        values = [r['modes'].get(mode,{}).get('d3d12_depth3',{}).get('frames_per_second',float('nan')) for r in rows]
        axes[0].bar([i+offset for i in x],values,width=.34,label=mode,color=colors[mode])
        low=[v-r['modes'].get(mode,{}).get('d3d12_depth3',{}).get('min_frames_per_second',v) for v,r in zip(values,rows)]
        high=[r['modes'].get(mode,{}).get('d3d12_depth3',{}).get('max_frames_per_second',v)-v for v,r in zip(values,rows)]
        axes[0].errorbar([i+offset for i in x],values,yerr=[low,high],fmt='none',color='#333',capsize=3)
    if any('native' in r['modes'] for r in rows):
        values=[r['modes'].get('native',{}).get('d3d12_depth3',{}).get('frames_per_second',float('nan')) for r in rows]
        axes[0].plot(x,values,'ko',label='native host control')
    axes[0].set_yscale('log');axes[0].set_ylabel('Completed frames/s (log scale)')
    axes[0].set_title('D3D12 synthetic stress — three frames in flight');axes[0].legend()
    for name,label,marker in [('d3d12_depth1','one frame in flight','o'),('d3d12_depth3','three frames in flight','s')]:
        values = [r['paired_relative_percent'].get(name,{}).get('median',float('nan')) for r in rows]
        low=[v-r['paired_relative_percent'].get(name,{}).get('minimum',v) for v,r in zip(values,rows)]
        high=[r['paired_relative_percent'].get(name,{}).get('maximum',v)-v for v,r in zip(values,rows)]
        axes[1].errorbar(x,values,yerr=[low,high],marker=marker,label=label,capsize=3)
    axes[1].axhline(100,color='#666',linestyle='--',linewidth=1)
    axes[1].set_ylabel('Venus / paired NVIDIA throughput (%)');axes[1].legend()
    axes[1].set_xticks(x,[r['profile'] for r in rows],rotation=25,ha='right')
    fig.text(.5,.01,'Same selected Proton on both guest backends • synthetic frames, not game FPS • see report for repeat counts',ha='center',fontsize=9)
    fig.tight_layout(rect=(0,.025,1,1));fig.savefig(root/'limits-comparison.svg');fig.savefig(root/'limits-comparison.png',dpi=150)
    table = []
    details = []
    for row in rows:
        name = row['profile'];config=row['config'];pipe=config['pipeline']
        rate = lambda mode: row['modes'].get(mode,{}).get('d3d12_depth3',{}).get('frames_per_second')
        show = lambda value: f'{value:,.1f}' if value is not None else '—'
        rel = row['paired_relative_percent'].get('d3d12_depth3',{})
        table.append(f'<tr><td>{name}</td><td>{config["width"]}×{config["height"]}</td><td>{pipe["shader_rounds"]}</td><td>{pipe["draws_per_frame"]}</td><td>{show(rate("nvidia"))}</td><td>{show(rate("venus"))}</td><td>{show(rate("native"))}</td><td>{show(rel.get("median"))}% ({rel.get("pairs",0)} pairs)</td></tr>')
        for mode,cases in row['modes'].items():
            c=cases['d3d12_depth3'];one=cases['d3d12_depth1'];mixed=cases['mixed_depth3']
            details.append(f'<tr><td>{name}</td><td>{mode}</td><td>{c["frames_per_second"]/one["frames_per_second"]:.2f}×</td><td>{c["gpu_execution_us"]:.1f}</td><td>{c["gpu_gap_us"]:.1f}</td><td>{c["gpu_active_percent"]:.1f}%</td><td>{c["record_us"]+c["submit_us"]:.1f}</td><td>{c["p95_cadence_us"]:.1f}</td><td>{mixed["frames_per_second"]:,.1f}</td></tr>')
    links = ''.join(f'<li><a href="{html.escape(s)}/measurements.json">{html.escape(s)} measurements</a> · <a href="{html.escape(s)}/environment.json">inputs</a></li>' for s in studies)
    failure_text = html.escape(json.dumps(failures,indent=2)) if failures else 'No failed trials in these saved measurements.'
    page = '''<!doctype html><meta charset="utf-8"><title>Legion Venus synthetic limits</title>
<style>body{font:16px system-ui;max-width:1250px;margin:36px auto;padding:0 20px;line-height:1.5;color:#20232b}img{max-width:100%}td,th{padding:9px;border-bottom:1px solid #ddd;text-align:left}table{border-collapse:collapse;width:100%}a{color:#1e5a99}.scroll{overflow:auto}</style>
<h1>Venus synthetic limits</h1><p>Owned D3D12 shaders through VKD3D, in actual KVM guests. The same maintenance7-enabled Proton runtime is used on both transports. Each configuration measures one, two and three frames in flight, with and without deterministic CPU work. Every validation image is checked against the independent pixel oracle.</p>
<p>'''+f'{report["passed"]}/{report["trials"]} saved trials passed; {report["expected_trials"]} trials planned.'+''' The chart shows median completed throughput and paired ratios; the JSON preserves individual timings and observed ranges. These are synthetic frame rates, not game performance.</p>
<img src="limits-comparison.svg" alt="Throughput and relative performance across synthetic stress profiles">
<h2>Three frames in flight, GPU work only</h2><p>Selected profiles are repeated after the initial sweep; counts vary. Native host controls use the same Proton but the host CPU and memory environment, so they are shown separately.</p><div class="scroll"><table><tr><th>Profile</th><th>Resolution</th><th>Shader rounds</th><th>Draws/frame</th><th>NVIDIA frames/s</th><th>Venus frames/s</th><th>Host frames/s</th><th>Venus relative</th></tr>'''+''.join(table)+'''</table></div>
<h2>Where time goes</h2><p>Queue gain compares three frames in flight with one. GPU active time is execution divided by the span from first GPU begin to last GPU end; it is not whole-device utilization. CPU record+submit and frame cadence use the process clock. Mixed throughput adds the configured CPU work (25,000 rounds normally; 1,000,000 in cpu1m).</p>
<div class="scroll"><table><tr><th>Profile</th><th>Backend</th><th>Queue gain</th><th>GPU execution µs</th><th>GPU gap µs</th><th>GPU active</th><th>CPU record+submit µs</th><th>p95 cadence µs</th><th>Mixed frames/s</th></tr>'''+''.join(details)+'''</table></div>
<h2>Failures</h2><pre>'''+failure_text+'''</pre><p>The 64×64 submission test can hit the fixture's strict requirement that every rendered frame have distinct GPU begin/end timestamps. An equal pair gives no usable per-frame GPU duration here and is excluded, rather than scored as zero throughput. This is a measurement rejection, not proof of a render or device-loss failure. Vulkan timestamps are discrete counter readings; see the <a href="https://docs.vulkan.org/spec/latest/chapters/queries.html">timestamp query specification</a>. The 256×256 profile tests a larger minimum workload.</p><h2>Limits of this study</h2><ul>'''+''.join('<li>'+html.escape(s)+'</li>' for s in report['limits'])+'''</ul>
<h2>Evidence</h2><ul>'''+links+'''</ul><p><a href="limits-report.json">Machine-readable report</a> · <a href="maintenance7-fix.html">Earlier real application test</a></p>'''
    (root/'limits-report.html').write_text(page)
    print(root/'limits-report.html')


if __name__ == '__main__':
    main()
