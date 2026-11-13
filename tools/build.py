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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--proton', type=Path, required=True)
    parser.add_argument('--output', type=Path, default=Path('build'))
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--engine', choices=['podman', 'docker'])
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    proton = args.proton.resolve(strict=True)
    expected = json.loads((root/'config/compatibility.json').read_text())
    for key, relative in [('source_sha256', 'files/lib/wine/x86_64-windows/ntdll.dll'),
                          ('unix_sha256', 'files/lib/wine/x86_64-unix/ntdll.so')]:
        with (proton/relative).open('rb') as file:
            if hashlib.file_digest(file, 'sha256').hexdigest() != expected[key]:
                parser.error('selected Proton does not match the supported compatibility input')
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
        if engine == 'podman':
            command += ['--userns=keep-id']
        command += ['--user',f'{os.getuid()}:{os.getgid()}',
            '-e','HOME=/build/home','-e','CARGO_HOME=/build/cargo',
            '-e','CARGO_TARGET_DIR=/build/target','-e','RUSTUP_HOME=/opt/rustup',
            '-e','LEGION_BUILD_IMAGE='+identity,
            '-v',str(root)+':/src:ro','-v',str(output)+':/build:rw',
            '-v',str(proton)+':/proton:ro',image,
            'python3','/src/tools/build_runtime.py','--jobs',str(args.jobs)]
        subprocess.run(command, check=True)
    print('Packaged executable:',output/'dist/legion')


if __name__ == '__main__':
    main()
