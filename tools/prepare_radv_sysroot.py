#!/usr/bin/env python3
"""Prepare a private RADV build sysroot from checksum-pinned Debian packages."""
import argparse
import hashlib
import json
import lzma
from pathlib import Path
import shutil
import subprocess
import urllib.request


PACKAGES = '''libc6 libc6-dev linux-libc-dev libgcc-14-dev libgcc-s1 libstdc++6
libstdc++-14-dev libdrm2 libdrm-amdgpu1 libdrm-dev libelf1t64 libelf-dev
libexpat1 libexpat1-dev libzstd1 libzstd-dev zlib1g zlib1g-dev libffi8 libffi-dev
libwayland-client0 libwayland-server0 libwayland-dev wayland-protocols
libx11-6 libx11-xcb1 libx11-dev libx11-xcb-dev libxcb1 libxcb1-dev
libxrandr2 libxrandr-dev libxrender1 libxrender-dev libxext6 libxext-dev
libxcb-dri3-0 libxcb-dri3-dev libxcb-present0 libxcb-present-dev
libxcb-randr0 libxcb-randr0-dev libxcb-shm0 libxcb-shm0-dev
libxcb-sync1 libxcb-sync-dev libxcb-xfixes0 libxcb-xfixes0-dev
libxcb-render0 libxcb-render0-dev libxcb-shape0 libxcb-shape0-dev
libxshmfence1 libxshmfence-dev libxau6 libxau-dev libxdmcp6 libxdmcp-dev
libbsd0 libmd0 libvulkan1 libvulkan-dev x11proto-dev xtrans-dev
xorg-sgml-doctools'''.split()


def extract(package, output):
    members = subprocess.check_output(['ar', 't', str(package)], text=True).splitlines()
    members = [name for name in members if name.startswith('data.tar')]
    if len(members) != 1:
        raise ValueError('package must contain exactly one data archive')
    member = members[0]
    flag = {'.xz': '-Jxf', '.gz': '-zxf', '.zst': '--zstd'}.get(Path(member).suffix)
    if flag is None:
        raise ValueError('unsupported package compression: '+member)
    command = ['tar', flag, *(['-xf'] if flag == '--zstd' else []),
               '-', '--no-same-owner', '-C', str(output)]
    producer = subprocess.Popen(['ar', 'p', str(package), member], stdout=subprocess.PIPE)
    try:
        subprocess.run(command, stdin=producer.stdout, check=True)
    finally:
        producer.stdout.close()
        status = producer.wait()
    if status:
        raise RuntimeError('package reader failed: '+str(package))


def prepare(index, cache, output, download, extra_cache=None, packages=None):
    packages = PACKAGES if packages is None else packages
    entries = {}
    with lzma.open(index, 'rt') as source:
        for block in source.read().split('\n\n'):
            item = dict(line.split(': ', 1) for line in block.splitlines()
                        if ': ' in line and not line.startswith(' '))
            if item.get('Package') in packages:
                entries[item['Package']] = item
    missing = set(packages)-entries.keys()
    if missing:
        raise ValueError('package index lacks: '+', '.join(sorted(missing)))
    output.mkdir(parents=True, exist_ok=True)
    cache.mkdir(parents=True, exist_ok=True)
    records = []
    for name in packages:
        item = entries[name]
        filename = Path(item['Filename'])
        if filename.is_absolute() or '..' in filename.parts:
            raise ValueError('invalid package URL path')
        package = cache/filename.name
        if not package.is_file() and extra_cache is not None:
            candidate = extra_cache/filename.name
            if candidate.is_file():
                package = candidate
        if not package.is_file():
            if not download:
                raise ValueError('missing package '+filename.name+'; supply it or use --download')
            temporary = package.with_suffix('.part')
            with urllib.request.urlopen('https://deb.debian.org/debian/'+filename.as_posix(),
                                        timeout=30) as source, temporary.open('wb') as destination:
                shutil.copyfileobj(source, destination)
            temporary.replace(package)
        with package.open('rb') as source:
            digest = hashlib.file_digest(source, 'sha256').hexdigest()
        if digest != item['SHA256']:
            raise ValueError('package checksum mismatch: '+filename.name)
        extract(package, output)
        records.append(dict(package=name, version=item['Version'], filename=item['Filename'], sha256=digest))
    for name, target in [('lib', 'usr/lib'), ('lib64', 'usr/lib64'),
                         ('bin', 'usr/bin'), ('sbin', 'usr/sbin')]:
        path = output/name
        if not path.exists() and not path.is_symlink():
            path.symlink_to(target)
    (output/'legion-sysroot-packages.json').write_text(json.dumps(records, indent=2)+'\n')
    print(json.dumps(dict(sysroot=str(output.resolve()), verified_packages=len(records),
                          host_installation=False)))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('index', 'cache', 'output'):
        parser.add_argument('--'+name, type=Path, required=True)
    parser.add_argument('--extra-cache', type=Path)
    parser.add_argument('--download', action='store_true')
    args = parser.parse_args()
    prepare(args.index, args.cache, args.output, args.download, args.extra_cache)
