#!/usr/bin/env python3
"""Render the saved Venus/Proton study without running any graphics workload."""
import argparse
import html
import json
from pathlib import Path
import statistics


def sample(record):
    mode='d3d11_shader_3_in_flight' if record['api']=='d3d11' else 'd3d12_depth3'
    return next(case for case in record['result']['cases'] if case['mode']==mode)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',type=Path,default=Path('build/venus'))
    args=parser.parse_args();root=args.root.resolve()
    studies={name:json.loads((root/name/'measurements.json').read_text()) for name in ['proton-qualification','proton-alternate','proton-native-control']}
    primary=studies['proton-qualification'];rows=[]
    for api in ['d3d11','d3d12']:
        for rounds in [1,64]:
            selected=[r for r in primary if r['api']==api and r['rounds']==rounds]
            assert len(selected)==6 and all(r['passed'] for r in selected)
            entry=dict(api=api,rounds=rounds)
            for mode in ['nvidia','venus']:
                values=[sample(r) for r in selected if r['mode']==mode]
                entry[mode]=dict(median_fps=statistics.median(v['iterations_per_second'] for v in values),
                    min_fps=min(v['iterations_per_second'] for v in values),max_fps=max(v['iterations_per_second'] for v in values),
                    median_p95_us=statistics.median(v['p95_us'] for v in values))
            ratios=[]
            for r in selected:
                if r['mode']=='venus':
                    reference=next(n for n in selected if n['mode']=='nvidia' and n['label'][:2]==r['label'][:2])
                    ratios.append(100*sample(r)['iterations_per_second']/sample(reference)['iterations_per_second'])
            entry['relative_percent']=dict(median=statistics.median(ratios),minimum=min(ratios),maximum=max(ratios))
            rows.append(entry)
    compatibility=[]
    for name,records in studies.items():
        compatibility.append(dict(study=name,trials=len(records),passed=sum(r['passed'] for r in records),
            protons=sorted({Path(r['proton']).name for r in records}),versions=sorted({v for r in records for v in r.get('translation_versions',[]) if 'Using' not in v})))
    applications={}
    for name in ['ue5-d3d12','ue5-d3d11','ue5-d3d12-diagnostic','ue5-d3d12-dialog2']:
        path=root/name/'result.json'
        if path.exists():applications[name]=json.loads(path.read_text())
    control=root/'ue5-nvidia-control/application/BenchmarkResults.txt'
    presentation=json.loads((root/'proton-window-final-mode/result.json').read_text())
    report=dict(schema=1,scope='offscreen Windows D3D through stock Proton; synthetic throughput is not game FPS',
        hardware='RTX 4070 Ti SUPER / NVIDIA 615.78.08',resolution=[1920,1080],
        primary_repetitions=3,primary_warmup_ms=500,primary_sample_ms=1500,
        image_validation='every rendered validation texel checked against independent integer shader oracle; D3D11 full-image hashes also match across backends',
        limitations=['single GPU and driver; unlocked clocks and background desktop',
            'D3D11 and D3D12 fixtures differ; compare backends within each API',
            'D3D11 has explicit per-frame flush and three query slots; D3D12 table uses three in-flight frames without mixed CPU load',
            'alternate Proton and native control have one repetition each; compatibility checks only',
            'experimental sync-file renderer patch enabled for Venus',
            'ray tracing, DLSS and broad game compatibility are not qualified'],
        results=rows,compatibility=compatibility,applications=applications,presentation=presentation,
        nvidia_game_control=control.read_text() if control.exists() else None,
        application_finding='UE displays: DirectX 12 is not supported on your system. The exact capability or initialization mismatch remains unresolved.',
        diagnostic_dialog='ue5-d3d12-dialog2/trial/vm/frames/diagnostic-shm-2.png')
    (root/'proton-report.json').write_text(json.dumps(report,indent=2)+'\n')
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    values=[r['relative_percent']['median'] for r in rows]
    fig,ax=plt.subplots(figsize=(9,4.8))
    ax.bar(range(4),values,color=['#376ea5','#376ea5','#5b8f6b','#5b8f6b'])
    ax.errorbar(range(4),values,yerr=[[v-r['relative_percent']['minimum'] for v,r in zip(values,rows)],
        [r['relative_percent']['maximum']-v for v,r in zip(values,rows)]],fmt='none',capsize=5,color='#333333')
    for i,v in enumerate(values):ax.text(i,v+3,f'{v:.1f}%',ha='center')
    ax.axhline(100,color='#555555',linestyle='--',linewidth=1)
    ax.set_xticks(range(4),['DXVK\nlight','DXVK\n64 shader rounds','VKD3D\nlight','VKD3D\n64 shader rounds'])
    ax.set_ylim(0,130);ax.set_ylabel('Venus / paired Legion NVIDIA throughput (%)')
    ax.set_title('Stock GE-Proton11-11 — offscreen Windows D3D, 1920 × 1080')
    fig.text(.5,.02,'Three repeats • whiskers show observed range • synthetic throughput, not game FPS',ha='center',fontsize=9)
    fig.tight_layout(rect=(0,.045,1,1));fig.savefig(root/'proton-comparison.svg');fig.savefig(root/'proton-comparison.png',dpi=150)
    table=''.join(f'<tr><td>{r["api"].upper()}</td><td>{r["rounds"]}</td><td>{r["nvidia"]["median_fps"]:,.0f}</td><td>{r["venus"]["median_fps"]:,.0f}</td><td>{r["relative_percent"]["median"]:.1f}%</td></tr>' for r in rows)
    checks=''.join(f'<li>{html.escape(", ".join(c["protons"]))}: {c["passed"]}/{c["trials"]} passed; {html.escape("; ".join(c["versions"]))}</li>' for c in compatibility)
    sources=''.join(f'<li><a href="{name}/measurements.json">{name}: measurements</a> · <a href="{name}/environment.json">environment</a></li>' for name in studies)
    page='''<!doctype html><meta charset="utf-8"><title>Legion Venus Proton study</title>
<style>body{font:16px system-ui;max-width:1050px;margin:36px auto;padding:0 20px;line-height:1.5;color:#20232b}img{max-width:100%}td,th{padding:12px;border-bottom:1px solid #ddd;text-align:left}table{border-collapse:collapse;width:100%}a{color:#1e5a99}</style>
<h1>Venus through Proton: DXVK and VKD3D</h1>
<p>RTX 4070 Ti SUPER, NVIDIA 615.78.08. Mesa Venus 26.2.6, experimental sync-file renderer wait. Stock Proton, no Wine overlay or CPU adapter. Tests run in actual KVM guests.</p>
<img src="proton-comparison.svg" alt="Venus throughput relative to Legion NVIDIA">
<p>These are offscreen shader probes, not game FPS. Three interleaved repetitions, 500 ms warmup and 1500 ms sampling per case. Every validation texel is checked. D3D12 also tests queue depths 1–3 and mixed CPU work; the table selects depth 3 without mixed CPU work.</p>
<table><tr><th>API</th><th>Shader rounds</th><th>Legion NVIDIA frames/s</th><th>Venus frames/s</th><th>Paired relative throughput</th></tr>'''+table+'''</table>
<h2>Compatibility checks</h2><ul>'''+checks+'''</ul>
<h2>Presentation</h2><p>The owned D3D12 presentation fixture renders and passes pixel checks through Venus. Stock QEMU produced a blank window; Legion's existing external-texture display path displays the pattern. Guest KMS counters alone did not catch the blank window, so window captures are checked separately.</p>
<p>The final probe also selects its explicitly requested KMS mode, avoiding a mismatch when a tiled SDL window changes the preferred EDID resolution.</p>
<p><a href="proton-window-final-mode/vm-010.png">Visible test pattern</a> · <a href="proton-window-capture/verified-window.png">Blank stock-QEMU control</a> · <a href="proton-window-final-mode/result.json">Final presentation measurements</a></p>
<h2>Unreal Engine application</h2><p>The current NVIDIA backend completed the 1080p D3D12 benchmark at 64.31 FPS using stock GE-Proton11-11. Initial Venus D3D11 and D3D12 attempts did not complete. A longer D3D12 diagnostic run, with desktop audio enabled, exposes an otherwise invisible software dialog: “DirectX 12 is not supported on your system.” The exact capability or initialization mismatch remains unresolved. This is not a passing game-compatibility result.</p>
<p><a href="ue5-d3d12-dialog2/trial/vm/frames/diagnostic-shm-2.png">Captured UE error dialog</a> · <a href="ue5-d3d12-dialog2/trial/vm/frames/proton-workload.stderr">Diagnostic Wine/VKD3D log</a></p>
<p><a href="ue5-nvidia-control/visible-window.png">Current-backend game image</a> · <a href="ue5-nvidia-control/application/BenchmarkResults.txt">Current-backend game result</a> · <a href="ue5-d3d12/trial/vm/report.json">Initial Venus D3D12 report</a> · <a href="ue5-d3d11/result.json">Initial Venus D3D11 report</a></p>
<h2>Limits</h2><ul>'''+''.join('<li>'+html.escape(v)+'</li>' for v in report['limitations'])+'''</ul>
<h2>Evidence</h2><ul>'''+sources+'''</ul><p><a href="proton-report.json">Machine-readable report</a></p>'''
    if (root/'compatibility-gap.json').exists():
        page=page.replace('<h1>Venus through Proton: DXVK and VKD3D</h1>',
            '<h1>Venus through Proton: DXVK and VKD3D</h1><p><b>Follow-up:</b> <a href="compatibility-gap.html">Unreal now completes in a diagnostic shader-model experiment; see the capability-gap findings.</a></p>')
    if (root/'maintenance7-fix.json').exists():
        report['follow_up']='maintenance7-fix.json'
        (root/'proton-report.json').write_text(json.dumps(report,indent=2)+'\n')
        page=page.replace('<h1>Venus through Proton: DXVK and VKD3D</h1>',
            '<h1>Venus through Proton: DXVK and VKD3D</h1><p><b>Latest result:</b> <a href="maintenance7-fix.html">The maintenance7 backport completes Unreal without a shader-model override.</a> The initial application findings below are historical.</p>')
    (root/'proton-report.html').write_text(page)
    print(root/'proton-report.html')


if __name__=='__main__':main()
