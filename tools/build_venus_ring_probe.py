#!/usr/bin/env python3
"""Build a private renderer for bounded command-ring polling A/B tests."""
import argparse
import json
from pathlib import Path
import subprocess

from benchmark_venus import sha, write
from build_venus_probe import source, apply_renderer_patch

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,default=ROOT/'build/venus/ring-probe')
    parser.add_argument('--jobs',type=int,default=2)
    args=parser.parse_args();output=args.output.resolve()
    if output.exists() or not output.is_relative_to(ROOT/'build') or not 1<=args.jobs<=8:
        parser.error('use a fresh output directory under build and 1..8 jobs')
    output.mkdir(parents=True)
    manifest=json.loads((ROOT/'config/sources.json').read_text())['virglrenderer-1.3.0.tar.gz']
    tree=output/'source';build=output/'build';prefix=output/'prefix'
    source(ROOT/'build/cache/virglrenderer-1.3.0.tar.gz',manifest['url'],manifest['sha256'],tree)
    patches=[ROOT/'patches/virgl-venus-sync-fd-experimental.patch',ROOT/'patches/virgl-venus-ring-poll-experimental.patch']
    for patch in patches:apply_renderer_patch(tree,patch)
    commands=[['meson','setup',str(build),str(tree),'--prefix='+str(prefix),'--libdir=lib',
        '--buildtype=release','-Dvenus=true','-Ddrm-renderers=','-Dtests=false','-Dplatforms=egl'],
        ['ninja','-C',str(build),'-j',str(args.jobs)],
        ['meson','install','-C',str(build),'--no-rebuild']]
    with (output/'build.log').open('w') as log:
        for command in commands:subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,check=True)
    write(output/'build.json',dict(schema=1,source=manifest,patches={str(p.relative_to(ROOT)):sha(p) for p in patches},
        worker_sha256=sha(prefix/'libexec/virgl_render_server'),
        scope='experimental bounded ring polling; original policy at spin budget zero; sync-file fence wait unchanged'))
    print(prefix)


if __name__=='__main__':main()
