#!/usr/bin/env python3
"""Build the generic guest in a separate output tree, never install on host."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess


def build(source, output, jobs, native_display=False):
    source = source.resolve(strict=True)
    display_patch = Path(__file__).with_name('virtio-gpu-xbgr8888.patch').resolve()
    if native_display:
        patch = ['patch', '--batch', '--force', '--fuzz=0', '-p1', '-d', str(source), '-i', str(display_patch)]
        applied = subprocess.run([*patch, '--reverse', '--dry-run'], capture_output=True)
        if applied.returncode:
            subprocess.run([*patch, '--dry-run'], check=True)
            subprocess.run(patch, check=True)
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    fragment = Path(__file__).with_name('guest.config').resolve()
    env = dict(os.environ, ARCH='x86_64', KCONFIG_ALLCONFIG=str(fragment),
               KBUILD_BUILD_USER='legion', KBUILD_BUILD_HOST='builder')
    # Do not inherit workstation-specific flags into the distributed kernel.
    for name in ('KCFLAGS', 'KCPPFLAGS', 'CFLAGS', 'RUSTFLAGS', 'KRUSTFLAGS'):
        env.pop(name, None)
    command = ['make', '-C', str(source), 'O='+str(output)]
    with (output/'configure.log').open('w') as log:
        subprocess.run([*command, 'allnoconfig'], env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    config = dict(line.split('=', 1) for line in (output/'.config').read_text().splitlines()
                  if line.startswith('CONFIG_') and '=' in line)
    required = ('64BIT','X86_64','SMP','CPU_SUP_INTEL','CPU_SUP_AMD','KVM_GUEST',
                'IA32_EMULATION','COMPAT_32BIT_TIME','RSEQ','NUMA','FUSE_FS','VIRTIO_FS','VIRTIO_PCI','DRM',
                'SND_HDA_INTEL','SND_PROC_FS','SND_PCM_TIMER','SND_TIMER','USER_NS','SECCOMP_FILTER','BPF_JIT','KPROBES','KALLSYMS','BASIC_MODVERSIONS',
                'SHMEM','TMPFS','MEMFD_CREATE','FILE_LOCKING','PROC_FS','SYSFS','DEVTMPFS')
    missing = [name for name in required if config.get('CONFIG_'+name) != 'y']
    if (missing or config.get('CONFIG_X86_NATIVE_CPU') == 'y'
            or config.get('CONFIG_NTSYNC') != 'm'
            or config.get('CONFIG_DRM_VIRTIO_GPU') != 'm'):
        raise RuntimeError('guest configuration did not retain required policy: '+str(missing))
    with (output/'build.log').open('w') as log:
        subprocess.run([*command, '-j'+str(jobs), 'bzImage', 'modules'], env=env,
                       stdout=log, stderr=subprocess.STDOUT, check=True)
    image = output/'arch/x86/boot/bzImage'
    result = dict(kernel=str(image), kernel_bytes=image.stat().st_size,
                  kernel_release=(output/'include/config/kernel.release').read_text().strip(),
                  config_sha256=hashlib.sha256((output/'.config').read_bytes()).hexdigest(),
                  cpu_target='generic x86-64', compiler=env.get('CC', 'gcc'),
                  modules=[str(p.relative_to(output)) for p in output.rglob('*.ko')],
                  host_installation=False)
    if native_display:
        result['native_display_patch_sha256'] = hashlib.sha256(display_patch.read_bytes()).hexdigest()
    (output/'result.json').write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--jobs', type=int, default=4)
    parser.add_argument('--native-display', action='store_true',
                        help='apply guest-only XBGR8888 scanout support to the prepared source')
    args = parser.parse_args()
    if not 1 <= args.jobs <= 16:
        parser.error('jobs must be 1..16')
    build(args.source, args.output, args.jobs, args.native_display)
