#!/usr/bin/env python3
"""Build an isolated GE-Proton11-11 graphics-stack maintenance7 experiment.

This reproduces one ABI-matched test runtime, not an installer for arbitrary
Proton versions. Installed Proton and Legion's packaged runtime are untouched.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

from build import container_security
from build_venus_probe import source

ROOT=Path(__file__).resolve().parents[1]
GE_SHA256='3131bfac872380ef18f3dc80bfc0d54c7d286e5ffbf46bac5f4bd36bd04524cf'
IMAGE='localhost/legion-venus-wine-build:1'
MODULES=[('winevulkan','winevulkan.dll','winevulkan.so'),
         ('win32u','win32u.dll','win32u.so'),
         ('winewayland.drv','winewayland.drv','winewayland.so')]


def digest(path):
    with path.open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()


def run(command, log, **kwargs):
    with log.open('w') as stream:
        subprocess.run([str(arg) for arg in command],stdout=stream,stderr=subprocess.STDOUT,check=True,**kwargs)


def patch_file(tree, patch, log):
    run(['patch','--batch','--forward','--fuzz=0','-p1','-d',tree,'-i',patch],log)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--proton',required=True,type=Path)
    parser.add_argument('--output',type=Path,default=ROOT/'build/venus/winevulkan-repro')
    parser.add_argument('--jobs',type=int,default=2)
    args=parser.parse_args()
    if not 1<=args.jobs<=8:parser.error('--jobs must be 1..8')
    proton=args.proton.resolve(strict=True);output=args.output.resolve()
    if not output.is_relative_to(ROOT/'build') or output.exists():
        parser.error('--output must be a fresh directory under the repository build directory')
    version=(proton/'version').read_text().strip()
    expected=json.loads((ROOT/'config/compatibility.json').read_text())
    if not version.endswith(' GE-Proton11-11') or digest(proton/'files/lib/wine/x86_64-unix/ntdll.so')!=expected['unix_sha256']:
        parser.error('this experiment targets GE-Proton11-11 graphics ABI 49; use a matching private test input')
    os.umask(0o077);output.mkdir(parents=True)
    manifest=json.loads((ROOT/'config/sources.json').read_text())['wine-c53678ab.tar.gz']
    wine=output/'source';ge=output/'ge-source'
    source(ROOT/'build/cache/wine-c53678ab.tar.gz',manifest['url'],manifest['sha256'],wine)
    source(output/'ge-source.tar.gz','https://codeload.github.com/GloriousEggroll/proton-ge-custom/tar.gz/refs/tags/GE-Proton11-11',GE_SHA256,ge)
    shutil.copytree(ge/'wineopenxr',wine/'dlls/wineopenxr')
    patches=sorted((ge/'patches/wine-hotfixes/wine-wayland').glob('*.patch'))
    patches.append(ge/'patches/wine-hotfixes/em-fixups/0001-winewayland-add-SNI-tray-icons-and-native-context-me.patch')
    patches+=sorted((ge/'patches/wine-hotfixes/wineland-child-rendering').glob('*.patch'))
    patches += [p for p in sorted((ge/'patches/wine-hotfixes/em-fixups').glob('*.patch')) if not p.name.startswith('0001-')]
    patches.append(ge/'patches/proton/83-nv_low_latency_wine.patch')
    logs=output/'patch-logs';logs.mkdir()
    for index,patch in enumerate(patches):patch_file(wine,patch,logs/f'{index:03d}.log')
    # Later GE patches alter the shared server protocol even though they do not
    # change these graphics modules. Keep request numbers and reply layouts in
    # sync with the installed server; rebuilding only the graphics patch subset
    # otherwise dispatches presentation requests to unrelated server handlers.
    patch_file(wine,ROOT/'patches/wine-ge-server-protocol-experimental.patch',output/'server-protocol.log')
    run(['./tools/make_requests'],output/'server-headers.log',cwd=wine)
    patch_file(wine,ROOT/'patches/wine-ge-graphics-build-experimental.patch',output/'build-fixes.log')
    recipe=output/'Containerfile'
    recipe.write_text('FROM localhost/legion-build:1\nUSER root\nRUN apt-get update && apt-get install -y --no-install-recommends libxkbregistry-dev && rm -rf /var/lib/apt/lists/*\n')
    with tempfile.TemporaryDirectory(prefix='legion-venus-wine-context-') as context:
        run(['podman','build','-t',IMAGE,'-f',recipe,context],output/'image-build.log')
    container=['podman','run','--rm','--userns=keep-id',*container_security('podman'),
        '--user',f'{os.getuid()}:{os.getgid()}','-e','HOME=/build/home',
        '-e','CFLAGS=-O2 -g -march=x86-64 -mtune=generic',
        '-v',str(ROOT/'build')+':/build:rw']
    inside=Path('/build')/output.relative_to(ROOT/'build')
    def execute(command, work, log):
        run(container+['-w',work,IMAGE,*command],output/log)
    # Generate the release's original interface before applying the upstream
    # generated conversion changes. Its older generator cannot regenerate the
    # recursive maintenance7 chain itself.
    execute(['./autogen.sh'],inside/'source','autogen.log')
    patch_file(wine,ROOT/'patches/winevulkan-maint7-experimental.patch',output/'maintenance7.log')
    build=output/'build';build.mkdir()
    execute([inside/'source/configure','--enable-win64','--disable-tests','--without-x','--with-wayland',
        '--with-vulkan','--with-opengl','--without-gstreamer','--without-alsa','--without-pulse',
        '--without-dbus','--without-cups','--without-udev','--without-freetype'],inside/'build','configure.log')
    targets=[]
    for module,pe,unix in MODULES:
        targets += [f'dlls/{module}/x86_64-windows/{pe}',f'dlls/{module}/{unix}']
    execute(['make',f'-j{args.jobs}',*targets],inside/'build','build.log')
    if '#define WINE_VULKAN_DRIVER_VERSION 49' not in (wine/'include/wine/vulkan_driver.h').read_text():
        raise RuntimeError('prepared graphics ABI differs from the tested ABI')
    destination=output/'proton'
    run(['cp','-a','--reflink=auto',proton,destination],output/'clone.log')
    changes=[]
    for module,pe,unix in MODULES:
        for binary,relative in [(build/'dlls'/module/'x86_64-windows'/pe,Path('files/lib/wine/x86_64-windows')/pe),
                (build/'dlls'/module/unix,Path('files/lib/wine/x86_64-unix')/unix)]:
            target=destination/relative;original=digest(target);target.unlink();shutil.copy2(binary,target)
            changes.append(dict(path=str(relative),before_sha256=original,after_sha256=digest(target)))
    result=dict(schema=1,scope='experimental matched x86_64 WineVulkan/win32u/Wayland stack; no capability override',
        source_proton=str(proton),private_proton=str(destination),version=version,graphics_abi=49,
        wine_source_sha256=manifest['sha256'],ge_source_sha256=GE_SHA256,
        server_protocol_sha256=digest(wine/'server/protocol.def'),
        source_wineserver_sha256=digest(proton/'files/bin/wineserver'),
        cflags='-O2 -g -march=x86-64 -mtune=generic',
        image_id=subprocess.check_output(['podman','image','inspect','--format','{{.Id}}',IMAGE],text=True).strip(),
        patches=[dict(path=str(p.relative_to(ge)),sha256=digest(p)) for p in patches],
        local_patches=[dict(path=str(p.relative_to(ROOT)),sha256=digest(p)) for p in [
            ROOT/'patches/wine-ge-graphics-build-experimental.patch',ROOT/'patches/wine-ge-server-protocol-experimental.patch',
            ROOT/'patches/winevulkan-maint7-experimental.patch']],
        shader_model_override=False,ntdll_replaced=False,files=changes)
    (output/'runtime.json').write_text(json.dumps(result,indent=2)+'\n')
    print(destination)


if __name__=='__main__':main()
