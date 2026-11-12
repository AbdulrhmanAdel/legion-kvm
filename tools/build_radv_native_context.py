#!/usr/bin/env python3
"""Build guest-only RADV with AMDGPU virtio enabled against a supplied sysroot."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


def compiler_wrapper(path, compiler, arguments):
    path.write_text('#!'+sys.executable+'\nimport os, sys\nos.execv('+repr(compiler)+
                    ', '+repr([compiler, *arguments])+' + sys.argv[1:])\n')
    path.chmod(0o755)


def versions(path, prefix):
    text = subprocess.check_output(['readelf', '--version-info', str(path)], text=True)
    return sorted(set(re.findall(prefix+r'([0-9]+(?:\.[0-9]+)+)', text)),
                  key=lambda value: tuple(map(int, value.split('.'))))


def build(source, sysroot, output, jobs, resume=False):
    source = source.resolve(strict=True)
    sysroot = sysroot.resolve(strict=True)
    if not (sysroot/'lib/x86_64-linux-gnu/libc.so.6').is_file() or not (sysroot/'lib64/ld-linux-x86-64.so.2').is_file():
        raise ValueError('sysroot lacks the libc linker paths; include its merged-/usr lib and lib64 links')
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if (output/'build').exists() and not resume:
        raise ValueError('use a new output directory; existing builds are preserved')
    gcc = sorted((sysroot/'usr/lib/gcc/x86_64-linux-gnu').glob('*/crtbeginS.o'))
    if not gcc:
        raise ValueError('sysroot lacks GCC startup objects')
    gcc = gcc[-1].parent
    cxx_headers = sysroot/'usr/include/c++'/gcc.name
    if not cxx_headers.is_dir():
        raise ValueError('sysroot lacks matching C++ development headers')
    libraries = sysroot/'usr/lib/x86_64-linux-gnu'
    # -B also changes GCC's intrinsic-header search. Select the guest startup
    # objects/runtime without feeding GCC 14 intrinsics to the host compiler.
    runtime = output/'compiler-runtime'
    runtime.mkdir(exist_ok=True)
    for path in gcc.iterdir():
        if path.is_file() and path.suffix in ('.o', '.a', '.so'):
            target = runtime/path.name
            if not target.exists() and not target.is_symlink():
                target.symlink_to(path.resolve())
    base = ['--sysroot='+str(sysroot), '-isystem', str(sysroot/'usr/include/x86_64-linux-gnu'),
            '-B'+str(runtime)+'/', '-B'+str(libraries)+'/', '-L'+str(gcc), '-L'+str(libraries),
            '-march=x86-64', '-mtune=generic']
    c = str(output/'guest-cc')
    cxx = str(output/'guest-cxx')
    compiler_wrapper(Path(c), shutil.which('gcc'), base)
    compiler_wrapper(Path(cxx), shutil.which('g++'), [*base, '-nostdinc++',
        '-isystem', str(cxx_headers),
        '-isystem', str(sysroot/'usr/include/x86_64-linux-gnu/c++'/gcc.name)])
    cross = output/'guest-cross.ini'
    cross.write_text("[binaries]\nc = "+repr(c)+"\ncpp = "+repr(cxx)+
        "\nar = 'ar'\nstrip = 'strip'\npkg-config = 'pkg-config'\n"+
        "[host_machine]\nsystem = 'linux'\ncpu_family = 'x86_64'\ncpu = 'x86_64'\nendian = 'little'\n"+
        "[properties]\nneeds_exe_wrapper = true\nsys_root = "+repr(str(sysroot))+
        "\npkg_config_libdir = "+repr([str(libraries/'pkgconfig'), str(sysroot/'usr/share/pkgconfig')])+"\n")
    command = ['meson', 'setup', str(output/'build'), str(source), '--cross-file', str(cross),
        '--wrap-mode=nodownload', '--buildtype=release', '--prefix=/usr',
        '--libdir=lib/x86_64-linux-gnu', '-Damdgpu-virtio=true', '-Dvulkan-drivers=amd',
        '-Dgallium-drivers=', '-Dplatforms=x11,wayland', '-Dllvm=disabled',
        '-Dglx=disabled', '-Degl=disabled', '-Dgbm=disabled', '-Dopengl=false',
        '-Dgles1=disabled', '-Dgles2=disabled', '-Dgallium-vdpau=disabled',
        '-Dgallium-va=disabled', '-Dgallium-xa=disabled', '-Dgallium-opencl=disabled',
        '-Dgallium-rusticl=false', '-Dvalgrind=disabled', '-Dbuild-tests=false',
        '-Dlibunwind=disabled', '-Dlmsensors=disabled', '-Dvideo-codecs=']
    environment = os.environ.copy()
    for key in ('CFLAGS', 'CXXFLAGS', 'CPPFLAGS', 'LDFLAGS', 'CC', 'CXX',
                'PKG_CONFIG_PATH', 'PKG_CONFIG_LIBDIR', 'PKG_CONFIG_SYSROOT_DIR', 'LD_PRELOAD'):
        environment.pop(key, None)
    previous = output/'configure-command.json'
    if resume and previous.exists() and json.loads(previous.read_text()) != command:
        raise ValueError('resume inputs differ from the recorded build; use a new output')
    previous.write_text(json.dumps(command, indent=2)+'\n')
    if resume and (output/'build/meson-private/coredata.dat').is_file():
        command = [*command[:2], '--reconfigure', *command[2:]]
    for name, invocation in [('configure', command),
            ('compile', ['ninja', '-C', str(output/'build'), '-j', str(jobs)]),
            ('install', ['meson', 'install', '-C', str(output/'build'),
                         '--destdir', str(output/'stage'), '--no-rebuild'])]:
        with (output/(name+'.log')).open('w') as log:
            subprocess.run(invocation, env=environment, stdout=log,
                           stderr=subprocess.STDOUT, check=True)
    library = output/'stage/usr/lib/x86_64-linux-gnu/libvulkan_radeon.so'
    dynamic = subprocess.check_output(['readelf', '-d', str(library)], text=True)
    needed = re.findall(r'\(NEEDED\).*\[([^]]+)\]', dynamic)
    if any('LLVM' in name for name in needed):
        raise ValueError('minimal RADV build unexpectedly depends on LLVM')
    glibc = versions(library, 'GLIBC_')
    provided = versions(libraries/'libc.so.6', 'GLIBC_')
    if tuple(map(int, glibc[-1].split('.'))) > tuple(map(int, provided[-1].split('.'))):
        raise ValueError('RADV requires a newer libc than the supplied guest sysroot')
    glibcxx = versions(library, 'GLIBCXX_')
    provided_cxx = versions(libraries/'libstdc++.so.6', 'GLIBCXX_')
    if glibcxx and tuple(map(int, glibcxx[-1].split('.'))) > tuple(map(int, provided_cxx[-1].split('.'))):
        raise ValueError('RADV requires a newer C++ runtime than the supplied guest sysroot')
    options = json.loads((output/'build/meson-info/intro-buildoptions.json').read_text())
    if not next(entry['value'] for entry in options if entry['name'] == 'amdgpu-virtio'):
        raise ValueError('built RADV without AMDGPU virtio support')
    with library.open('rb') as handle:
        digest = hashlib.file_digest(handle, 'sha256').hexdigest()
    result = dict(library=str(library), bytes=library.stat().st_size, sha256=digest,
        mesa_version=(source/'VERSION').read_text().strip(), amdgpu_virtio=True,
        cpu_baseline='x86-64', source=str(source), sysroot=str(sysroot), glibc_versions=glibc,
        glibcxx_versions=glibcxx, needed=needed,
        hardware_rendering_verified=False, host_installation=False)
    notices = output/'stage/usr/share/doc/legion-radv'
    notices.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source/'docs/license.rst', notices/'license.rst')
    shutil.copytree(source/'licenses', notices/'licenses', dirs_exist_ok=True)
    if (source/'debian/copyright').is_file():
        shutil.copy2(source/'debian/copyright', notices/'debian-copyright')
    (output/'result.json').write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('source', 'sysroot', 'output'):
        parser.add_argument('--'+name, type=Path, required=True)
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--resume', action='store_true')
    args = parser.parse_args()
    if not 1 <= args.jobs <= 16:
        parser.error('jobs must be 1..16')
    build(args.source, args.sysroot, args.output, args.jobs, args.resume)
