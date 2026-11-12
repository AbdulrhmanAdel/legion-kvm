#!/usr/bin/env python3
"""Build Legion's QEMU with the DRM native-context device, without installing it."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess


def build(source, output, jobs, native_display=False):
    source = source.resolve(strict=True)
    display_patch = Path(__file__).resolve().parents[1]/'patches/qemu-amdgpu-native-display.patch'
    if native_display:
        patch = ['patch', '--batch', '--force', '--fuzz=0', '-p1', '-d', str(source), '-i', str(display_patch)]
        applied = subprocess.run([*patch, '--reverse', '--dry-run'], capture_output=True)
        if applied.returncode:
            subprocess.run([*patch, '--dry-run'], check=True)
            subprocess.run(patch, check=True)
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    command = [str(source/'configure'), '--target-list=x86_64-softmmu',
               '--without-default-features', '--enable-kvm', '--enable-vhost-user',
               '--disable-tcg', '--disable-tools', '--disable-guest-agent',
               '--disable-docs', '--disable-werror', '--enable-pa',
               '--audio-drv-list=pa', '--enable-opengl', '--enable-virglrenderer',
               '--enable-sdl', '--enable-pixman',
               '--extra-cflags=-march=x86-64 -mtune=generic']
    environment = os.environ.copy()
    for key in ('CFLAGS', 'CXXFLAGS', 'LDFLAGS', 'LD_PRELOAD'):
        environment.pop(key, None)
    (output/'configure-command.json').write_text(json.dumps(command, indent=2)+'\n')
    with (output/'configure.log').open('w') as log:
        subprocess.run(command, cwd=output, env=environment, stdout=log,
                       stderr=subprocess.STDOUT, check=True)
    with (output/'build.log').open('w') as log:
        subprocess.run(['ninja', '-C', str(output), '-j', str(jobs),
                        'qemu-system-x86_64'], env=environment, stdout=log,
                       stderr=subprocess.STDOUT, check=True)
    binary = output/'qemu-system-x86_64'
    properties = subprocess.run([str(binary), '-device', 'virtio-gpu-gl,help'],
                                check=True, capture_output=True, text=True)
    if 'drm_native_context' not in properties.stdout + properties.stderr:
        raise RuntimeError('built QEMU lacks the DRM native-context device option')
    if native_display and 'drm-rendernode' not in properties.stdout + properties.stderr:
        raise RuntimeError('built QEMU lacks explicit DRM context device selection')
    (output/'native-context-properties.txt').write_text(properties.stdout+properties.stderr)
    displays = subprocess.run([str(binary), '-display', 'help'],
                              check=True, capture_output=True, text=True)
    if 'egl-headless' not in displays.stdout + displays.stderr:
        raise RuntimeError('built QEMU lacks the headless EGL display backend')
    result = dict(binary=str(binary), sha256=hashlib.file_digest(binary.open('rb'), 'sha256').hexdigest(),
                  source=str(source), qemu_version=(source/'VERSION').read_text().strip(),
                  cpu_baseline='x86-64', native_context_device=True,
                  gpu_rendering_verified=False, host_installation=False)
    if native_display:
        result['native_display_patch_sha256'] = hashlib.sha256(display_patch.read_bytes()).hexdigest()
        result['native_display_scope'] = 'experimental external-texture scanout fixture'
    (output/'result.json').write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--native-display', action='store_true',
                        help='apply the experimental display patch to the prepared source')
    args = parser.parse_args()
    if not 1 <= args.jobs <= 16:
        parser.error('jobs must be 1..16')
    build(args.source, args.output, args.jobs, args.native_display)
