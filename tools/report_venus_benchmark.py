#!/usr/bin/env python3
"""Summarize the Venus transport experiment; optionally plot with matplotlib."""
import argparse
import html
import json
from pathlib import Path
import statistics


def records(root, name):
    return json.loads((root/name/'measurements.json').read_text())


def case_for(record, iterations):
    return next(case for case in record['cases'] if case['case']['gpu_iterations']==iterations)


def aggregate(data, mode, iterations):
    cases = [case_for(record, iterations) for record in data if record['mode']==mode]
    return dict(fps=statistics.median(c['frames_per_second'] for c in cases),
        min_fps=min(c['frames_per_second'] for c in cases), max_fps=max(c['frames_per_second'] for c in cases),
        p95_ms=statistics.median(c['frame_intervals']['p95_ms'] for c in cases),
        client_cpu_us_per_frame=statistics.median(c['cpu_process_seconds']/c['completed_frames']*1e6 for c in cases))


def paired_ratios(data, iterations):
    values = []
    for record in data:
        if record['mode'] != 'venus':
            continue
        reference = next(r for r in data if r['mode']=='nvidia' and r['label'].split('-')[0]==record['label'].split('-')[0])
        values.append(100*case_for(record, iterations)['frames_per_second']/case_for(reference, iterations)['frames_per_second'])
    return dict(median=statistics.median(values), minimum=min(values), maximum=max(values))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path('build/venus'))
    args = parser.parse_args()
    root = args.root.resolve()
    rows = []
    for label, iterations, original, changed in [
        ('1080p light', 1, 'baseline-run', 'sync-fd-repeat'),
        ('1080p medium', 64, 'baseline-run', 'sync-fd-repeat'),
        ('1080p heavier', 512, 'baseline-run', 'sync-fd-repeat'),
        ('4K GPU-bound', 4096, 'gpu-bound-4k', 'sync-fd-4k'),
    ]:
        before, after = records(root, original), records(root, changed)
        selected = [case_for(r, iterations) for r in before+after]
        assert len({c['gpu_known_answer_hash'] for c in selected}) == 1, 'backend image hashes disagree'
        rows.append(dict(workload=label, fragment_iterations=iterations,
            current_legion=aggregate(after, 'nvidia', iterations),
            stock_venus=aggregate(before, 'venus', iterations),
            experimental_venus=aggregate(after, 'venus', iterations),
            stock_relative_percent=paired_ratios(before, iterations),
            experimental_relative_percent=paired_ratios(after, iterations)))
    report = dict(scope='offscreen deterministic Linux Vulkan; three in-flight frames; not game FPS or display latency',
        repetitions=3, warmup_ms=1000, sample_ms=3000, image_validation='64 known-answer texels and identical full-image readback hashes across every backend and repeat',
        limitations=['single GPU and driver: RTX 4070 Ti SUPER / NVIDIA 615.78.08',
            'no locked clocks or isolated desktop; runs are interleaved and ranges are retained',
            'stock and patched renderer series have separate paired Legion controls',
            'no presentation, DXVK/VKD3D, ray tracing or DLSS qualification',
            'client CPU excludes host renderer costs; process-group CPU sampling may omit detached renderer workers'],
        finding='Stock renderer vkWaitForFences enters NVIDIA libnvidia-glcore poll([],0,10); experimental sync-file waiting removes this polling path',
        renderer_patch='../../patches/virgl-venus-sync-fd-experimental.patch', results=rows)
    (root/'report.json').write_text(json.dumps(report, indent=2)+'\n')
    plot = False
    try:
        import matplotlib
        matplotlib.use('Agg')
        import matplotlib.pyplot as plt
        fig, axes = plt.subplots(1, 2, figsize=(12, 4.7), gridspec_kw=dict(width_ratios=[1.7, 1]))
        ax = axes[0]
        for offset, key, label, color in [(-.18, 'stock_relative_percent', 'Stock Venus', '#bd4640'),
                (.18, 'experimental_relative_percent', 'Venus with sync-file wait', '#286cb6')]:
            values = [row[key]['median'] for row in rows]
            positions = [i+offset for i in range(len(rows))]
            ax.bar(positions, values, width=.34, color=color, label=label)
            ax.errorbar(positions, values,
                yerr=[[v-row[key]['minimum'] for v,row in zip(values,rows)], [row[key]['maximum']-v for v,row in zip(values,rows)]],
                fmt='none', ecolor='#333333', capsize=3, linewidth=1)
            for x, value in zip(positions, values):
                ax.text(x, value+3, f'{value:.1f}%', ha='center', fontsize=8)
        ax.axhline(100, color='#555555', linestyle='--', linewidth=1)
        ax.set_xticks(range(4), [r['workload'].replace(' ', '\n', 1) for r in rows])
        ax.set_ylim(0, 120)
        ax.set_ylabel('Throughput relative to paired Legion NVIDIA control (%)')
        ax.set_title('Offscreen throughput; bars show median, whiskers show range')
        ax.legend(loc='upper left', fontsize=8)
        ax = axes[1]
        values = [rows[-1][key]['p95_ms'] for key in ['current_legion','stock_venus','experimental_venus']]
        ax.bar(range(3), values, color=['#54815b','#bd4640','#286cb6'])
        for i,v in enumerate(values):ax.text(i,v+.4,f'{v:.2f} ms',ha='center',fontsize=9)
        ax.set_xticks(range(3), ['Current\nLegion','Stock\nVenus','Sync-file\nVenus'])
        ax.set_ylim(0,25)
        ax.set_ylabel('p95 offscreen frame interval (ms)')
        ax.set_title('4K GPU-bound frame intervals')
        fig.suptitle('Venus feasibility — RTX 4070 Ti SUPER, NVIDIA 615.78.08', fontsize=13)
        fig.text(.5,.015,'Three repeats per path • synthetic Vulkan workload • no game FPS or display-latency claim',ha='center',fontsize=9)
        fig.tight_layout(rect=(0,.045,1,.94))
        fig.savefig(root/'comparison.svg')
        fig.savefig(root/'comparison.png',dpi=150)
        plot = True
    except ImportError:
        pass
    table = ''.join('<tr><td>'+html.escape(row['workload'])+'</td>'+''.join(
        f'<td>{row[key]["fps"]:,.1f}<br><small>{row[key]["min_fps"]:,.1f}–{row[key]["max_fps"]:,.1f}</small></td>'
        for key in ['current_legion','stock_venus','experimental_venus'])+'</tr>' for row in rows)
    limitations = ''.join('<li>'+html.escape(value)+'</li>' for value in report['limitations'])
    sources = ''.join(f'<li><a href="{name}/measurements.json">{name}: raw measurements</a> · <a href="{name}/environment.json">environment</a></li>'
        for name in ['baseline-run','gpu-bound-4k','renderer-control','renderer-sync-fd','sync-fd-repeat','sync-fd-4k'])
    page = '''<!doctype html><meta charset="utf-8"><title>Legion Venus benchmark</title>
<style>body{font:16px system-ui;max-width:1100px;margin:36px auto;padding:0 20px;color:#20232b;line-height:1.5}table{border-collapse:collapse;width:100%}td,th{padding:12px;text-align:left;border-bottom:1px solid #ddd}small{color:#616774}img{width:100%}a{color:#1e5a99}</style>
<h1>Legion Venus transport experiment</h1><p>Single-machine feasibility study: RTX 4070 Ti SUPER, Ryzen 7 9800X3D, NVIDIA 615.78.08. Mesa Venus 26.2.6; virglrenderer 1.3.0; QEMU 11.1.2; guest kernel 6.18.62.</p>
<p>These are synthetic offscreen frames, not game FPS. Each condition has three interleaved repeats, 1 second of warmup and 3 seconds of sampling per workload, with three GPU frames in flight. Full-image validation hashes match across backends.</p>
'''+('<img src="comparison.svg" alt="Throughput and frame interval comparison">' if plot else '')+'''
<h2>Median offscreen frames per second</h2><p>Small text shows the observed minimum–maximum across three trials. Stock and experimental series were measured separately with their own interleaved controls.</p>
<table><tr><th>Workload</th><th>Current Legion NVIDIA</th><th>Stock Venus</th><th>Experimental sync-file Venus</th></tr>'''+table+'''</table>
<h2>Identified stall</h2><p>Stack traces place repeated 10 ms sleeps inside NVIDIA's vkWaitForFences, called by the renderer's queue thread. The isolated patch exports the queue-completion fence using VK_KHR_external_fence_fd, waits for its sync file, closes the descriptor and then retires the completion. It does not skip GPU synchronization.</p>
<p><a href="server.stack">Stack trace</a> · <a href="server.strace">Renderer syscall trace</a> · <a href="client.strace">Client syscall trace</a> · <a href="experimental-sync-fd.patch">Tested patch</a> · <a href="report.json">Machine-readable report</a></p>
<h2>Limits</h2><ul>'''+limitations+'''</ul><p>The patch and Venus backend remain experimental. Normal game launches and the packaged executable are unchanged by this experiment.</p>
<h2>Raw data</h2><ul>'''+sources+'</ul>'
    (root/'report.html').write_text(page)
    print(root/'report.html')


if __name__ == '__main__':
    main()
