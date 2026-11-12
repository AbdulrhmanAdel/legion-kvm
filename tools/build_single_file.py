#!/usr/bin/env python3
"""Append a compressed, length-delimited runtime to the static Rust launcher."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import struct
import subprocess
import tempfile


def build(runtime, launcher, output):
    runtime = runtime.resolve(strict=True)
    if output.exists():
        raise ValueError('output must be a new file')
    if not (runtime/'runtime.json').is_file():
        raise ValueError('runtime manifest is missing')
    # Refresh hashes only in this explicit build tree.
    records = []
    for path in sorted(runtime.rglob('*')):
        if path.is_symlink() or not path.is_file() or path.name == 'SHA256SUMS':
            continue
        relative = path.relative_to(runtime).as_posix()
        if any(c in relative for c in '\n\r\\'):
            raise ValueError('unsupported checksum path')
        with path.open('rb') as source:
            digest = hashlib.file_digest(source, 'sha256').hexdigest()
        records.append(f'{digest}  {relative}\n')
    (runtime/'SHA256SUMS').write_text(''.join(records))
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='legion-pack-', dir=output.parent) as temporary:
        compressed = Path(temporary)/'runtime.zst'
        process = subprocess.Popen(['zstd','-q','-T2','-10','-o',str(compressed)], stdin=subprocess.PIPE)
        seen = {}
        try:
            for path in sorted(runtime.rglob('*')):
                name = path.relative_to(runtime).as_posix().encode()
                meta = path.lstat()
                if stat.S_ISDIR(meta.st_mode):
                    kind, data, length = 1, b'', 0
                elif stat.S_ISLNK(meta.st_mode):
                    kind, data = 2, os.readlink(path).encode(); length = len(data)
                elif stat.S_ISREG(meta.st_mode):
                    key = (meta.st_dev, meta.st_ino)
                    if key in seen:
                        kind, data = 3, seen[key]; length = len(data)
                    else:
                        seen[key] = name; kind, data, length = 0, None, meta.st_size
                else:
                    raise ValueError('unsupported runtime file type')
                process.stdin.write(bytes([kind])+struct.pack('<IQI',len(name),length,meta.st_mode&0o777)+name)
                if data is None:
                    with path.open('rb') as source:
                        shutil.copyfileobj(source, process.stdin)
                else:
                    process.stdin.write(data)
            process.stdin.write(b'\xff'); process.stdin.close()
            if process.wait() != 0:
                raise RuntimeError('runtime compression failed')
        finally:
            if process.poll() is None:
                process.kill(); process.wait()
        with compressed.open('rb') as source:
            digest = hashlib.file_digest(source,'sha256').digest()
        with output.open('xb') as destination:
            for path in (launcher, compressed):
                with path.open('rb') as source:
                    shutil.copyfileobj(source,destination)
            destination.write(b'LEGION_BUNDLE_01'+struct.pack('<Q',compressed.stat().st_size)+digest)
        output.chmod(0o755)
    with output.open('rb') as source:
        checksum=hashlib.file_digest(source,'sha256').hexdigest()
    output.with_name(output.name+'.sha256').write_text(checksum+'  '+output.name+'\n')
    result=dict(binary=str(output.resolve()),bytes=output.stat().st_size,sha256=checksum,
                format='ELF + zstd runtime records + versioned SHA-256 footer')
    output.with_name(output.name+'.build.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result))


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('runtime','launcher','output'):
        parser.add_argument('--'+name,type=Path,required=True)
    args=parser.parse_args();build(args.runtime,args.launcher,args.output)
