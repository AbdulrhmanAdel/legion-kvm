#!/usr/bin/env python3
"""Build a packaged launcher in an isolated Linux toolchain."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def source_identity(root):
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
    changes = subprocess.check_output(['git', 'diff', 'HEAD', '--binary'], cwd=root)
    return revision if not changes else revision+'-dirty-'+hashlib.sha256(changes).hexdigest()[:12]


def container_security(engine):
    if engine != 'podman':
        return []
    security = json.loads(subprocess.check_output(
        [engine, 'info', '--format', '{{json .Host.Security}}'], text=True))
    # Keep the user's existing labels, especially on the read-only Proton tree.
    # Only this disposable build container runs without SELinux separation.
    return ['--security-opt', 'label=disable'] if security.get('selinuxEnabled') else []


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--proton', type=Path, help='optional Proton installation for extra dependency discovery')
    parser.add_argument('--wine-repair', action='store_true', help='include the version-specific optional Wine repair (requires matching --proton)')
    parser.add_argument('--output', type=Path, default=Path('build'))
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--engine', choices=['podman', 'docker'])
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    build_id = source_identity(root)
    proton = args.proton.resolve(strict=True) if args.proton else None
    if args.wine_repair:
        if proton is None:
            parser.error('--wine-repair requires --proton')
        expected = json.loads((root/'config/compatibility.json').read_text())
        for key, relative in [('source_sha256', 'files/lib/wine/x86_64-windows/ntdll.dll'),
                              ('unix_sha256', 'files/lib/wine/x86_64-unix/ntdll.so')]:
            with (proton/relative).open('rb') as file:
                if hashlib.file_digest(file, 'sha256').hexdigest() != expected[key]:
                    parser.error('selected Proton does not match the optional Wine repair input')
    if not 1 <= args.jobs <= 64:
        parser.error('--jobs must be between 1 and 64')
    engine = args.engine or next((name for name in ('podman','docker') if shutil.which(name)),None)
    if engine is None:
        parser.error('install rootless Podman or an accessible Docker engine')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    with (output/'build.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        image = 'localhost/legion-build:1'
        subprocess.run([engine,'build','-t',image,'--file',str(root/'build-support/Containerfile'),str(root)],check=True)
        identity = subprocess.check_output([engine,'image','inspect',image,'--format','{{.Id}}'],text=True).strip()
        command = [engine,'run','--rm']
        command += container_security(engine)
        if engine == 'podman':
            command += ['--userns=keep-id']
        command += ['--user',f'{os.getuid()}:{os.getgid()}',
            '-e','HOME=/build/home','-e','CARGO_HOME=/build/cargo',
            '-e','CARGO_TARGET_DIR=/build/target','-e','RUSTUP_HOME=/opt/rustup',
            '-e','LEGION_BUILD_IMAGE='+identity,
            '-e','LEGION_BUILD_ID='+build_id,
            '-e','LEGION_WINE_REPAIR='+('1' if args.wine_repair else '0'),
            '-v',str(root)+':/src:ro','-v',str(output)+':/build:rw']
        if proton is not None:
            command += ['-v',str(proton)+':/proton:ro']
        command += [image,'python3','/src/tools/build_runtime.py','--jobs',str(args.jobs)]
        subprocess.run(command, check=True)
    print('Packaged executable:',output/'dist/legion')


if __name__ == '__main__':
    main()
