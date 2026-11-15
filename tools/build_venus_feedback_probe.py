#!/usr/bin/env python3
"""Build a private portable Venus driver with bounded semaphore polling."""
import argparse
import json
import os
from pathlib import Path
import subprocess

from benchmark_venus import sha, write
from build import container_security
from build_venus_probe import source, apply_renderer_patch, MESA_SHA256

ROOT=Path(__file__).resolve().parents[1]


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,default=ROOT/'build/venus/feedback-probe')
    parser.add_argument('--jobs',type=int,default=2)
    args=parser.parse_args();output=args.output.resolve()
    if output.exists() or not output.is_relative_to(ROOT/'build') or not 1<=args.jobs<=8:
        parser.error('use a fresh directory under build and 1..8 jobs')
    output.mkdir(parents=True)
    tree=output/'source';build=output/'build'
    source(ROOT/'build/venus/mesa-26.2.6.tar.xz','https://archive.mesa3d.org/mesa-26.2.6.tar.xz',MESA_SHA256,tree)
    patch=ROOT/'patches/mesa-venus-feedback-poll-experimental.patch'
    apply_renderer_patch(tree,patch)
    inside=Path('/build')/output.relative_to(ROOT/'build')
    container=['podman','run','--rm','--userns=keep-id',*container_security('podman'),
        '--user',f'{os.getuid()}:{os.getgid()}','-e','HOME=/build/home',
        '-e','CARGO_HOME=/build/cargo','-e','CARGO_TARGET_DIR=/build/target','-e','RUSTUP_HOME=/opt/rustup',
        '-v',str(ROOT)+':/src:ro','-v',str(ROOT/'build')+':/build:rw','-w','/src','localhost/legion-build:1']
    setup=['meson','setup',str(inside/'build'),str(inside/'source'),'--buildtype=release',
        '--prefix=/usr','--libdir=lib/x86_64-linux-gnu','--wrap-mode=nodownload',
        '-Dvulkan-drivers=virtio','-Dgallium-drivers=','-Dplatforms=x11,wayland','-Dllvm=disabled',
        '-Dglx=disabled','-Degl=disabled','-Dgbm=disabled','-Dopengl=false','-Dgles1=disabled',
        '-Dgles2=disabled','-Dbuild-tests=false','-Dvideo-codecs=','-Dvalgrind=disabled',
        '-Dlibunwind=disabled','-Dlmsensors=disabled']
    with (output/'build.log').open('w') as log:
        for command in [['cargo','build','--release','--offline','-p','legion-proton-workload',
                '--bin','legion-proton-bootstrap',f'-j{args.jobs}'],setup,['ninja','-C',str(inside/'build'),'-j',str(args.jobs)],
                ['meson','install','-C',str(inside/'build'),'--destdir',str(inside/'stage'),'--no-rebuild']]:
            subprocess.run(container+command,stdout=log,stderr=subprocess.STDOUT,check=True)
    driver=output/'stage/usr/lib/x86_64-linux-gnu/libvulkan_virtio.so'
    write(output/'build.json',dict(schema=1,source_sha256=MESA_SHA256,patch_sha256=sha(patch),
        driver_sha256=sha(driver),bootstrap_sha256=sha(ROOT/'build/target/release/legion-proton-bootstrap'),
        scope='opt-in bounded guest semaphore polling; original policy at zero; semaphore and timeout checks unchanged'))
    print(driver)


if __name__=='__main__':main()
