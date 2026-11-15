#!/usr/bin/env python3
"""Build isolated Venus benchmark components without changing Legion's package.

First build Legion normally to provide localhost/legion-build:1 and its runtime.
The host needs QEMU with Venus, Vulkan, Meson, Ninja and graphics development
headers. The renderer is installed only under the selected output directory.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
MESA_SHA256 = 'b9436b3e1a8be76fbd4823dc884f90596ab7c6ac5e1867a305e66ffccfb31a92'


def run(command, **kwargs):
    subprocess.run([str(arg) for arg in command], check=True, **kwargs)


def source(archive, url, digest, destination):
    if not archive.exists():
        temporary = archive.with_suffix('.part')
        with urllib.request.urlopen(url, timeout=120) as response, temporary.open('wb') as output:
            shutil.copyfileobj(response, output)
        temporary.replace(archive)
    with archive.open('rb') as stream:
        if hashlib.file_digest(stream, 'sha256').hexdigest() != digest:
            raise ValueError('source checksum mismatch: '+str(archive))
    if not destination.exists():
        with tempfile.TemporaryDirectory(dir=destination.parent, prefix='source-') as directory:
            temporary = Path(directory)
            with tarfile.open(archive) as stream:
                stream.extractall(temporary, filter='data')
            roots = list(temporary.iterdir())
            if len(roots) != 1 or not roots[0].is_dir():
                raise ValueError('source archive must have exactly one root directory')
            roots[0].rename(destination)


def apply_renderer_patch(destination, patch):
    # --force prevents GNU patch from silently trying the opposite direction
    # during this reverse check and incorrectly calling an unpatched tree done.
    command = ['patch', '--batch', '--force', '--fuzz=0', '-p1', '-d', destination, '-i', patch]
    if subprocess.run([str(arg) for arg in command]+['--reverse', '--dry-run'], capture_output=True).returncode:
        run(command)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT/'build/venus')
    parser.add_argument('--jobs', type=int, default=2)
    args = parser.parse_args()
    if not 1 <= args.jobs <= 16:
        parser.error('--jobs must be 1..16')
    output = args.output.resolve()
    if not output.is_relative_to(ROOT/'build'):
        parser.error('--output must be under the repository build directory')
    output.mkdir(parents=True, exist_ok=True)
    source(output/'mesa-26.2.6.tar.xz', 'https://archive.mesa3d.org/mesa-26.2.6.tar.xz',
           MESA_SHA256, output/'mesa-26.2.6')
    manifest = json.loads((ROOT/'config/sources.json').read_text())['virglrenderer-1.3.0.tar.gz']
    source(ROOT/'build/cache/virglrenderer-1.3.0.tar.gz', manifest['url'], manifest['sha256'], output/'renderer-source')
    patch = ROOT/'patches/virgl-venus-sync-fd-experimental.patch'
    apply_renderer_patch(output/'renderer-source', patch)
    # The same generic Debian build environment as Legion, without GPU access.
    container = ['podman', 'run', '--rm', '--userns=keep-id']
    from build import container_security
    container += container_security('podman')
    container += ['--user', f'{os.getuid()}:{os.getgid()}', '-e', 'HOME=/build/home',
        '-e', 'CARGO_HOME=/build/cargo', '-e', 'CARGO_TARGET_DIR=/build/target',
        '-e', 'RUSTUP_HOME=/opt/rustup', '-v', str(ROOT)+':/src:ro',
        '-v', str(ROOT/'build')+':/build:rw', 'localhost/legion-build:1']
    run(container+['cargo', 'build', '--release', '-p', 'legion-kvm', '-p', 'legion-workload-probe',
                   '--features', 'legion-workload-probe/gpu', '-j', args.jobs])
    run(container+['cargo', 'build', '--release', '-p', 'legion-proton-workload', '--bin', 'legion-proton-bootstrap',
                   '-p', 'legion-guest-compositor', '--bin', 'legion-guest-compositor', '-j', args.jobs])
    run(container+['cargo', 'build', '--release', '--target', 'x86_64-unknown-linux-musl',
                   '-p', 'legion-guest-probe', '-j', args.jobs])
    inside = Path('/build')/output.relative_to(ROOT/'build')
    setup = ['meson', 'setup', inside/'mesa-build', inside/'mesa-26.2.6',
        '--buildtype=release', '--prefix=/usr', '--libdir=lib/x86_64-linux-gnu', '--wrap-mode=nodownload',
        '-Dvulkan-drivers=virtio', '-Dgallium-drivers=', '-Dplatforms=x11,wayland',
        '-Dllvm=disabled', '-Dglx=disabled', '-Degl=disabled', '-Dgbm=disabled',
        '-Dopengl=false', '-Dgles1=disabled', '-Dgles2=disabled', '-Dbuild-tests=false',
        '-Dvideo-codecs=', '-Dvalgrind=disabled', '-Dlibunwind=disabled', '-Dlmsensors=disabled']
    if (output/'mesa-build/build.ninja').exists():
        setup.append('--reconfigure')
    run(container+setup)
    run(container+['ninja', '-C', inside/'mesa-build', '-j', args.jobs])
    run(container+['meson', 'install', '-C', inside/'mesa-build', '--destdir', inside/'stage', '--no-rebuild'])
    setup = ['meson', 'setup', output/'renderer-build', output/'renderer-source',
             '--prefix='+str(output/'renderer-prefix'), '--libdir=lib', '--buildtype=release',
             '-Dvenus=true', '-Ddrm-renderers=', '-Dtests=false', '-Dplatforms=egl']
    if (output/'renderer-build/build.ninja').exists():
        setup.append('--reconfigure')
    run(setup)
    run(['ninja', '-C', output/'renderer-build', '-j', args.jobs])
    run(['meson', 'install', '-C', output/'renderer-build', '--no-rebuild'])
    print('Isolated Venus components:', output)
    print('Benchmark launcher:', ROOT/'build/target/release/legion')


if __name__ == '__main__':
    main()
