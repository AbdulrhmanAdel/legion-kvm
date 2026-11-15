#!/usr/bin/env python3
"""Render saved maintenance7 experiment evidence; never launch a workload."""
import argparse
import html
import json
from pathlib import Path

UPSTREAM = 'https://github.com/wine-mirror/wine/commit/0245a17f2bde9258fe93bd7b8433a8e7d3bc485a'


def load(path):
    return json.loads(path.read_text())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path('build/venus'))
    args = parser.parse_args()
    root = args.root.resolve()
    trial = root/'ue5-maint7-fixed'
    result = load(trial/'result.json')
    fixture = root/'maint7-repro-caps/01-proton-d3d12-64-venus'
    measurement = load(fixture/'measurement.json')
    caps = load(fixture/'vm/frames/d3d12-capabilities.json')
    runtime = load(root/'winevulkan-repro/runtime.json')
    launch = load(trial/'usr/share/legion/launch.json')
    if not result['passed'] or not measurement['passed']:
        raise SystemExit('Both saved application and shader checks must pass')
    if result['diagnostic_shader_model'] or 'VKD3D_SHADER_MODEL' in launch.get('environment', {}):
        raise SystemExit('This report requires a run without a shader-model override')
    if runtime['ntdll_replaced'] or result['cpu_adapter'] or result['wine_repair']:
        raise SystemExit('This report requires the isolated graphics experiment')
    venus_fps = float((trial/'application/BenchmarkResults.txt').read_text().splitlines()[0])
    nvidia_fps = float((root/'ue5-nvidia-control/application/BenchmarkResults.txt').read_text().splitlines()[0])
    stock = load(root/'caps-render-verified/01-GE-Proton11-11-x86_64-d3d12-64-venus/vm/frames/d3d12-capabilities.json')
    control_caps = load(root/'caps-render-verified/01-GE-Proton11-11-x86_64-d3d12-64-nvidia/vm/frames/d3d12-capabilities.json')
    captures = result['captures']
    capture = next(c['path'] for c in captures if c.get('image_analysis', {}).get('nonuniform'))
    capture = str(Path(capture).relative_to(root))
    report = dict(schema=1, scope='single RTX 4070 Ti SUPER / NVIDIA 615.78.08 / GE-Proton11-11 x86_64 experiment',
        finding='maintenance7 forwarding restores automatic native-driver workaround selection through Venus',
        shader_model_override=None, stock_venus_caps=stock, maintenance7_venus_caps=caps,
        nvidia_control_caps=control_caps,
        shader_check_passed=measurement['passed'], application_passed=result['passed'],
        application_exit_code=result['exit_code'], completed_scanouts=result['kms-result.json']['completed_scanouts'],
        venus_fps=venus_fps, nvidia_control_fps=nvidia_fps, relative_percent=100*venus_fps/nvidia_fps,
        upstream_fix=UPSTREAM, runtime='winevulkan-repro/runtime.json', screenshot=capture,
        limits=['One run per game configuration; unlocked clocks and background desktop.',
            'The NVIDIA control uses stock graphics modules; this is not a pure transport comparison.',
            'Venus uses the experimental sync-file renderer patch and Legion external-texture QEMU display.',
            'Only x86_64 GE-Proton11-11 was tested; other Proton versions and 32-bit need qualification.',
            'SM 6.8 reporting and one shader/game test do not validate every advertised feature, ray tracing or DLSS.',
            'Normal Legion launches still use the existing backend.'])
    fresh = root/'maint7-final-caps/01-proton-d3d12-64-venus/measurement.json'
    if fresh.exists():
        check = load(fresh)
        if not check['passed']:
            raise SystemExit('The fresh builder output failed its shader check')
        report['fresh_build'] = dict(runtime='winevulkan-final-build/runtime.json',
            shader_check=str(fresh.relative_to(root)), passed=True,
            source_validation='winevulkan-final-build/source-validation.json')
    (root/'maintenance7-fix.json').write_text(json.dumps(report, indent=2)+'\n')
    def sm(value):
        return f'{value >> 4}.{value & 15}'
    limits = ''.join('<li>'+html.escape(v)+'</li>' for v in report['limits'])
    page = f'''<!doctype html><meta charset="utf-8"><title>Legion Venus maintenance7 experiment</title>
<style>body{{font:16px system-ui;max-width:1000px;margin:36px auto;padding:0 20px;line-height:1.5;color:#20232b}}img{{max-width:100%}}td,th{{padding:12px;border-bottom:1px solid #ddd;text-align:left}}table{{border-collapse:collapse;width:100%}}a{{color:#1e5a99}}</style>
<h1>Unreal completes through Venus without a shader-model override</h1>
<p>The isolated Wine maintenance7 backport exposes the native driver underneath Venus, allowing VKD3D's existing driver workarounds to apply. The owned D3D12 shader, timestamp and pixel checks pass; the UE5 D3D12 benchmark completes with visible rendering and exit code zero.</p>
<table><tr><th>Configuration</th><th>Automatic shader model</th><th>Game result, 1920 × 1080</th></tr>
<tr><td>Stock GE11 / Venus</td><td>{sm(stock['highest_shader_model'])}</td><td>DirectX 12 capability dialog</td></tr>
<tr><td>Maintenance7 backport / Venus</td><td>{sm(caps['highest_shader_model'])}</td><td>{venus_fps:.2f} FPS</td></tr>
<tr><td>Stock GE11 / Legion NVIDIA control</td><td>{sm(control_caps['highest_shader_model'])}</td><td>{nvidia_fps:.2f} FPS</td></tr></table>
<p>The two successful game runs are single observations, not a performance parity claim. The patched Venus run is {report['relative_percent']:.1f}% of this control.</p>
<img src="{html.escape(capture)}" alt="Actual Unreal scene rendered through patched Wine and Venus">
<h2>What changed</h2><p>A private Proton copy replaces six matched WineVulkan, win32u and Wine Wayland graphics binaries. No shader-model override, ntdll replacement or CPU adapter is used. The installed Proton is untouched. The test rebuild also preserves the release's server protocol declarations: omitting two later GE protocol changes caused a server crash in the first game attempt, despite the offscreen shader probe passing.</p>
<p>The standards-based part is <a href="{UPSTREAM}">Wine's maintenance7 support</a>. The release-specific rebuild is a test vehicle; it should not become a per-Proton DLL injection scheme. Production integration needs a coherent runtime with this extension support.</p>
<h2>Evidence</h2><ul>
<li><a href="maintenance7-fix.json">Machine-readable findings</a> · <a href="winevulkan-repro/runtime.json">Source and binary provenance</a> · <a href="winevulkan-repro/protocol-validation.json">Exact server request-order comparison</a></li>
<li><a href="winevulkan-final-build/runtime.json">Fresh builder output provenance</a> · <a href="maint7-final-caps/01-proton-d3d12-64-venus/measurement.json">Fresh build shader check</a></li>
<li><a href="maint7-repro-caps/01-proton-d3d12-64-venus/measurement.json">Owned shader check</a> · <a href="maint7-repro-caps/01-proton-d3d12-64-venus/vm/frames/d3d12-capabilities.json">Automatic capabilities</a></li>
<li><a href="ue5-maint7-fixed/result.json">Game transport and window checks</a> · <a href="ue5-maint7-fixed/application/BenchmarkResults.txt">Game FPS result</a> · <a href="ue5-maint7-fixed/trial/vm/frames/proton-workload.stderr">Wine/VKD3D log</a></li>
<li><a href="ue5-nvidia-control/application/BenchmarkResults.txt">NVIDIA control</a> · <a href="compatibility-gap.html">Earlier shader-override diagnosis</a> · <a href="proton-report.html">Earlier DXVK/VKD3D study</a></li></ul>
<h2>Qualification limits</h2><ul>{limits}</ul>'''
    (root/'maintenance7-fix.html').write_text(page)
    print(root/'maintenance7-fix.html')


if __name__ == '__main__':
    main()
