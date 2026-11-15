#!/usr/bin/env python3
"""Prepare guest userspace from application and runtime dependencies."""
import argparse
from collections import deque
import json
import os
from pathlib import Path
import posixpath
import re
import shutil
import subprocess


BINARIES = '''sh bash dash env python3 python3.13 vulkaninfo vkcube Xwayland xauth xkbcomp
cat cp mv mkdir chmod ln readlink rm rmdir sed grep egrep fgrep cut tr head tail
uname realpath which locale localedef id ls find sort dirname basename stat wc
touch expr printf true false getent getconf ldd ldconfig tar gzip xz xrandr
xdg-user-dir xdg-user-dirs-update sleep test timeout date tee uniq file'''.split()
LIBRARIES = '''libgbm.so.1 libdrm.so.2 libvulkan.so.1 libEGL.so.1 libGL.so.1 libGLX.so.0 libGLdispatch.so.0
libGLESv2.so.2 libwayland-client.so.0 libwayland-cursor.so.0 libwayland-egl.so.1
libxkbcommon.so.0 libasound.so.2 libX11.so.6 libXext.so.6 libXcursor.so.1
libXrandr.so.2 libXfixes.so.3 libXi.so.6 libXrender.so.1 libfreetype.so.6
libfontconfig.so.1 libnss_dns.so.2 libnss_files.so.2 libresolv.so.2 libudev.so.1
libdbus-1.so.3 libcurl.so.4 libssl.so.3 libcrypto.so.3 libpulse.so.0'''.split()
# NVIDIA's Vulkan X11 surface path loads these dynamically rather than through
# DT_NEEDED. EGL/glamor alone does not exercise Vulkan's presentation queries.
X11_LIBRARIES = '''libX11-xcb.so.1 libxcb.so.1 libxcb-dri3.so.0 libxcb-present.so.0
libxcb-sync.so.1 libxcb-xfixes.so.0 libxcb-glx.so.0 libxcb-randr.so.0
libxshmfence.so.1'''.split()
LIBRARIES += X11_LIBRARIES
# Platform SONAMEs used by several Proton generations. Keep these in the
# portable baseline even when no installed Proton is supplied at build time.
PROTON_PLATFORM_LIBRARIES = '''ld-linux-x86-64.so.2 ld-linux.so.2 libc.so.6 libdl.so.2 libm.so.6
libpthread.so.0 libresolv.so.2 librt.so.1 libgcc_s.so.1 libstdc++.so.6
libGL.so.1 libGLX.so.0 libEGL.so.1 libX11.so.6 libX11-xcb.so.1 libXext.so.6
libXfixes.so.3 libXrandr.so.2 libasound.so.2 libblas.so.3 libbrotlidec.so.1
libbz2.so.1.0 libcairo.so.2 libdrm.so.2 libdrm_amdgpu.so.1 libgio-2.0.so.0
libglib-2.0.so.0 libgmodule-2.0.so.0 libgobject-2.0.so.0 libgnutls.so.30
libgssapi_krb5.so.2 libgudev-1.0.so.0 libjpeg.so.62 liblapack.so.3 liblcms2.so.2
libmpg123.so.0 libnettle.so.8 libnghttp2.so.14 libogg.so.0 libopenal.so.1 libopus.so.0
libpango-1.0.so.0 libpangocairo-1.0.so.0 libpcsclite.so.1 libpipewire-0.3.so.0
libpsl.so.5 libpulse.so.0 libsndfile.so.1 libspeex.so.1 libsqlite3.so.0
libtheoradec.so.1 libtheoraenc.so.1 libudev.so.1 libusb-1.0.so.0 libva.so.2
libva-drm.so.2 libva-x11.so.2 libvdpau.so.1 libvorbis.so.0 libvorbisenc.so.2
libwayland-client.so.0 libwayland-cursor.so.0 libwayland-egl.so.1 libxcb.so.1
libz.so.1 libzstd.so.1'''.split()
DATA = ['etc', 'lib/locale/C.utf8', 'share/fonts', 'share/fontconfig', 'share/X11',
        'share/alsa', 'share/zoneinfo', 'share/doc', 'share/ca-certificates',
        'share/egl/egl_external_platform.d', 'share/vulkan',
        'share/legion']


def vendor(name):
    # The baseline's source-qualified generic EGL/GBM helper must stay private;
    # a workstation build can contain instructions hidden by our guest profile.
    if name.startswith('libnvidia-egl-gbm.so'):
        return False
    return name.startswith(('libnvidia-', 'libcuda.so', 'libnvcuvid.so',
                            'libGLX_nvidia.so', 'libEGL_nvidia.so', 'libvulkan_nvidia.so'))


def virtual_absolute(path):
    for prefix, replacement in [('/usr/', ''), ('/lib64/', 'lib/'),
                                ('/lib/', 'lib/'), ('/bin/', 'bin/'),
                                ('/sbin/', 'bin/'), ('/etc/', 'etc/')]:
        if path.startswith(prefix):
            return replacement+path[len(prefix):]
    raise ValueError('unmapped absolute userspace symlink: '+path)


def link_target(relative, target):
    relative = virtual_absolute(target) if target.startswith('/') else posixpath.join(str(relative.parent), target)
    normalized = posixpath.normpath(relative)
    if normalized.startswith('../') or normalized == '..':
        raise ValueError('userspace symlink escapes its root')
    return Path(normalized)


def elf_class(path):
    try:
        with path.open('rb') as source:
            header = source.read(5)
        return header[4] if header[:4] == b'\x7fELF' else None
    except OSError:
        return None


def dependencies(path):
    output = subprocess.run(['readelf', '-d', '-l', str(path)], check=True,
                            capture_output=True, text=True).stdout
    needed = re.findall(r'\(NEEDED\).*\[([^]]+)\]', output)
    interpreter = re.search(r'Requesting program interpreter:\s*([^]\s]+)', output)
    return needed, interpreter[1] if interpreter else None


class Baseline:
    def __init__(self, source, output):
        self.source, self.output = source, output
        self.queue, self.included, self.processed = deque(), set(), set()
        self.external = {}
        self.protons = []
        self.missing = set()
        self.external_missing = set()
        self.provided = set()
        self.library_index = {}
        for folder, _, names in os.walk(source/'lib', followlinks=False):
            for name in names:
                if '.so' in name and not vendor(name):
                    self.library_index.setdefault(name, []).append((Path(folder)/name).relative_to(source))

    def resolve(self, relative):
        seen = set()
        while (self.source/relative).is_symlink():
            if relative in seen:
                raise ValueError('userspace symlink cycle')
            seen.add(relative)
            relative = link_target(relative, os.readlink(self.source/relative))
        return relative

    def include(self, relative):
        relative = Path(relative)
        if relative in self.included:
            return
        source, target = self.source/relative, self.output/relative
        if not source.exists() and not source.is_symlink():
            return
        if vendor(source.name) or source.suffix.lower() in ('.dll', '.exe'):
            return
        self.included.add(relative)
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_symlink():
            target.symlink_to(os.readlink(source))
            self.include(link_target(relative, os.readlink(source)))
        elif source.is_dir():
            target.mkdir(exist_ok=True)
            for child in source.iterdir():
                if child.name not in ('__pycache__', 'compatibility'):
                    self.include(child.relative_to(self.source))
        else:
            shutil.copy2(source, target)
            if elf_class(source):
                self.queue.append(source)

    def library(self, name, architecture, required=True, external=False, provider=None):
        if (architecture, name) in self.provided:
            return
        if vendor(name):
            return
        if provider is not None and (provider, architecture, name) in self.external:
            return
        multiarch = 'i386-linux-gnu' if architecture == 1 else 'x86_64-linux-gnu'
        candidates = [Path(directory)/name for directory in [f'lib/{multiarch}', 'lib', f'lib/{multiarch}/pulseaudio']]
        candidates += self.library_index.get(name, [])
        for relative in candidates:
            real = self.source/self.resolve(relative)
            if real.is_file() and elf_class(real) == architecture:
                self.include(relative)
                self.provided.add((architecture, name))
                return
        # Extracted Debian packages can omit ldconfig-generated SONAME links.
        # Recover only a library that actually declares the requested SONAME.
        stem = name.split('.so')[0]
        for candidate_name, paths in self.library_index.items():
            if not candidate_name.startswith(stem):
                continue
            for relative in paths:
                real = self.source/self.resolve(relative)
                if not real.is_file() or elf_class(real) != architecture:
                    continue
                dynamic = subprocess.run(['readelf','-d',str(real)],check=True,capture_output=True,text=True).stdout
                soname = re.search(r'\(SONAME\).*\[([^]]+)\]',dynamic)
                if soname is not None and soname[1] == name:
                    self.include(relative)
                    target = self.output/'lib'/multiarch/name
                    target.parent.mkdir(parents=True,exist_ok=True)
                    if not target.exists() and not target.is_symlink():
                        target.symlink_to(posixpath.relpath(str(relative),str(Path('lib')/multiarch)))
                    self.provided.add((architecture,name))
                    return
        if required:
            (self.external_missing if external else self.missing).add((architecture, name))

    def inspect_proton(self, proton):
        self.protons.append(proton)
        objects = []
        for parent in [proton/'files/bin', proton/'files/lib', proton/'files/lib64']:
            if not parent.is_dir():
                continue
            for path in parent.rglob('*'):
                if path.is_file() and elf_class(path) and path.resolve().is_relative_to(proton):
                    real = path.resolve()
                    objects.append(real)
                    self.external[(proton, elf_class(real), path.name)] = real
        self.queue.extend(objects)

    def close(self):
        while self.queue:
            path = self.queue.popleft()
            if path in self.processed:
                continue
            self.processed.add(path)
            architecture = elf_class(path)
            needed, interpreter = dependencies(path)
            if interpreter:
                self.include(virtual_absolute(interpreter))
            for name in needed:
                provider = next((root for root in self.protons if path.is_relative_to(root)),None)
                self.library(name, architecture, external=provider is not None, provider=provider)


def prepare(source, output, protons, programs=()):
    source = source.resolve(strict=True)
    if not (source/'bin/vulkaninfo').is_file():
        raise ValueError('a qualified guest baseline is required')
    for name in ('Xwayland', 'xkbcomp'):
        if not (source/'bin'/name).is_file():
            raise ValueError('XWayland profile requires baseline utility '+name)
    output = output.resolve()
    output.mkdir(mode=0o700)
    baseline = Baseline(source, output)
    baseline.queue.extend(Path(program).resolve(strict=True) for program in programs)
    for proton in protons:
        baseline.inspect_proton(proton.resolve(strict=True))
    for name in BINARIES:
        baseline.include('bin/'+name)
    for name in LIBRARIES:
        baseline.library(name, 2, required=name in X11_LIBRARIES or name in
                         ('libgbm.so.1', 'libdrm.so.2'))
    for name in ('libfreetype.so.6', 'libfontconfig.so.1'):
        baseline.library(name, 1)
    for name in PROTON_PLATFORM_LIBRARIES:
        for architecture in (1, 2):
            baseline.library(name, architecture, required=False)
    for relative in DATA:
        baseline.include(relative)
    for path in (source/'lib').glob('python3.*'):
        if path.is_dir():
            baseline.include(path.relative_to(source))
    # ALSA, EGL, Vulkan and optional overlays use plugins outside DT_NEEDED.
    for directory in ['lib/x86_64-linux-gnu/alsa-lib', 'lib/x86_64-linux-gnu/mangohud',
                      'lib/i386-linux-gnu/alsa-lib']:
        baseline.include(directory)
    for architecture in ('x86_64-linux-gnu', 'i386-linux-gnu'):
        baseline.include('lib/'+architecture+'/gstreamer-1.0')
    baseline.include('lib/ld-linux.so.2')
    baseline.close()
    if baseline.missing:
        raise ValueError('unresolved platform dependencies: '+str(sorted(baseline.missing)))
    # The runtime builder installs its source-built Venus ICD after slimming.
    for directory in [output/'share/glvnd/egl_vendor.d', output/'share/vulkan/icd.d']:
        if directory.is_dir():
            for path in directory.glob('*.json'):
                path.unlink()
    files = [p for p in output.rglob('*') if p.is_file() and not p.is_symlink()]
    before_strip = sum(p.stat().st_size for p in files)
    for path in files:
        if elf_class(path):
            subprocess.run(['strip', '--strip-debug', str(path)], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    result = dict(schema=1, source_files=len(files), bytes_before_strip=before_strip,
                  bytes=sum(p.stat().st_size for p in files), inspected_elf_objects=len(baseline.processed),
                  protons=[p.name for p in protons], driver_libraries='bundled Venus; host driver stays outside the guest',
                  optional_tool_dependencies_absent_in_full_baseline=sorted(baseline.external_missing),
                  excluded=['Proton binaries','NVIDIA proprietary driver binaries','Mesa software renderers',
                            'locale generators/data sources','debug information'])
    output.with_name(output.name+'-manifest.json').write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--proton', required=True, action='append', type=Path)
    args = parser.parse_args()
    prepare(args.source, args.output, args.proton)
