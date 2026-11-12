#!/usr/bin/env python3
"""Build NVIDIA's open EGL/GBM helper for the guest's baseline x86-64 ISA."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile


def build(source, sysroot, linker, headers, output):
    source = source.resolve(strict=True)
    sysroot = sysroot.resolve(strict=True)
    linker = linker.resolve(strict=True)
    headers = headers.resolve(strict=True)
    output = output.absolute()
    if output.exists() or output.is_symlink():
        raise ValueError('output must be a new private library path')
    output.parent.mkdir(parents=True, exist_ok=True)
    names = ['gbm-display.c', 'gbm-handle.c', 'gbm-mutex.c',
             'gbm-platform.c', 'gbm-surface.c', 'gbm-utils.c']
    sources = [source/'src'/name for name in names]
    for path in sources:
        if not path.is_file():
            raise ValueError(f'missing EGL/GBM source: {path}')
    header_hashes = {}
    with tempfile.TemporaryDirectory(prefix='legion-egl-gbm-', dir=output.parent) as temporary:
        work = Path(temporary)
        include = work/'include'
        include.mkdir()
        for name in ['EGL', 'KHR', 'libdrm']:
            shutil.copytree(headers/name, include/name)
        for name in ['gbm.h', 'xf86drm.h', 'xf86drmMode.h']:
            shutil.copy2(headers/name, include/name)
        for name in ['eglexternalplatform.h', 'eglexternalplatformversion.h']:
            shutil.copy2(headers/'EGL'/name, include/name)
        for path in include.rglob('*'):
            if path.is_file():
                header_hashes[str(path.relative_to(include))] = hashlib.sha256(path.read_bytes()).hexdigest()
        library = work/'library.so'
        command = [str(linker), '-isystem', str(sysroot/'usr/include/x86_64-linux-gnu'),
                   '-shared', '-fPIC', '-O2', '-march=x86-64', '-mtune=generic',
                   '-fvisibility=hidden', '-D_GNU_SOURCE',
                   '-I'+str(include), '-I'+str(include/'libdrm'), '-I'+str(source/'external'),
                   '-Wl,-soname,libnvidia-egl-gbm.so.1', '-o', str(library)]
        command += [str(path) for path in sources]
        command += ['-l:libgbm.so.1', '-l:libdrm.so.2', '-ldl', '-pthread']
        subprocess.run(command, check=True)
        # The library becomes visible only after a successful complete link.
        shutil.copy2(library, output)
    provenance = dict(
        schema=1, isa='x86-64 baseline', source=str(source), sysroot=str(sysroot),
        linker=str(linker), headers=str(headers),
        source_sha256={str(p.relative_to(source)): hashlib.sha256(p.read_bytes()).hexdigest()
                       for p in source.rglob('*') if p.is_file() and '.git' not in p.parts},
        header_sha256=header_hashes,
        sha256=hashlib.sha256(output.read_bytes()).hexdigest())
    output.with_name(output.name+'.provenance.json').write_text(json.dumps(provenance, indent=2)+'\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['source', 'sysroot', 'linker', 'output']:
        parser.add_argument('--'+name, required=True, type=Path)
    parser.add_argument('--headers', default=Path('/usr/include'), type=Path)
    args = parser.parse_args()
    build(args.source, args.sysroot, args.linker, args.headers, args.output)
