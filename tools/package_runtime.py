#!/usr/bin/env python3
"""Assemble an explicit, relocatable NVIDIA runtime from explicit inputs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess


FIXTURE_FILES = ('bootstrap', 'compositor', 'legion-guest-compositor', 'audio-runtime')


def copy_file(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source.resolve(strict=True), destination)


def strip_release(path):
    with path.open('rb') as source:
        elf = source.read(4) == b'\x7fELF'
    if elf:
        subprocess.run(['strip', '--strip-unneeded', str(path)], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def libraries(programs):
    result = {}
    for program in programs:
        output = subprocess.run(['ldd', str(program)], check=True, capture_output=True, text=True).stdout
        if 'not found' in output:
            raise ValueError('build input has unresolved host libraries')
        for line in output.splitlines():
            match = re.search(r'(?:=>\s+)?(/\S+)\s+\(', line)
            if not match:
                continue
            path = Path(match[1])
            if path.name in result and result[path.name].resolve() != path.resolve():
                raise ValueError('host library basename collision: '+path.name)
            result[path.name] = path
    return result


def host_notices(root, host):
    """Retain local package provenance/notices when building on an Arch host."""
    if shutil.which('pacman') is None:
        return {}
    packages = {}
    for source in host.values():
        reply = subprocess.run(['pacman', '-Qqo', str(source)], capture_output=True, text=True)
        if reply.returncode:
            continue
        for package in reply.stdout.splitlines():
            if package in packages:
                continue
            version = subprocess.run(['pacman', '-Q', package], capture_output=True, text=True, check=True)
            packages[package] = version.stdout.strip()
            directory = Path('/usr/share/licenses')/package
            if directory.is_dir():
                shutil.copytree(directory, root/'licenses'/package, symlinks=True, dirs_exist_ok=True)
    return packages


def build(args):
    for relative in ('bin/vulkaninfo', 'share/legion/bootstrap',
                     'share/vulkan/icd.d/nvidia_icd.json'):
        if not (args.userspace/relative).is_file():
            raise ValueError('input is not a prepared guest userspace: '+relative)
    root = args.output.resolve()
    root.mkdir(mode=0o700)
    (root/'bin').mkdir()
    copy_file(args.launcher, root/'bin/legion')
    for name in ('legion-qemu', 'legion-backend', 'legion-virtiofsd'):
        # Hardlinks preserve argv[0] even when the launcher canonicalizes a
        # tool path. They also occupy one archive payload, not four binaries.
        os.link(root/'bin/legion', root/'bin'/name)
    for source, relative in ((args.qemu, 'libexec/qemu-system-x86_64'),
                             (args.backend, 'libexec/vhost-user-nvgpu'),
                             (args.virtiofsd, 'libexec/virtiofsd'),
                             (args.kernel, 'guest/kernel'), (args.init, 'guest/init'),
                             (args.workload_config, 'guest/workload.json')):
        copy_file(source, root/relative)
        if relative.startswith('libexec/'):
            strip_release(root/relative)
    data = root/'share/qemu'
    data.mkdir(parents=True)
    # This q35/direct-Linux appliance needs only x86 boot ROMs, not every
    # architecture's firmware in the host distribution's QEMU package.
    for name in ('bios-256k.bin', 'linuxboot_dma.bin', 'kvmvapic.bin', 'multiboot_dma.bin', 'pvh.bin'):
        copy_file(args.qemu_data/name, data/name)
    modules = []
    for index, module in enumerate(args.module):
        relative = f'guest/modules/{index:03d}.ko'
        copy_file(module, root/relative)
        modules.append(relative)
    kernel_config = getattr(args, 'kernel_config', None)
    if kernel_config is not None:
        copy_file(kernel_config, root/'guest/kernel.config')
    host = libraries([args.qemu.resolve(strict=True), args.virtiofsd.resolve(strict=True)])
    loaders = [name for name in host if name.startswith('ld-linux-x86-64')]
    if len(loaders) != 1:
        raise ValueError('expected one Linux x86-64 dynamic loader')
    for name, source in host.items():
        copy_file(source, root/'host/lib'/name)
    source_usr = args.userspace.resolve(strict=True)
    usr = root/'guest/usr'
    usr.mkdir(parents=True)
    for entry in source_usr.iterdir():
        if entry.name == 'share' and entry.is_dir():
            (usr/'share').mkdir()
            for child in entry.iterdir():
                if child.name.startswith('legion-'):
                    continue
                subprocess.run(['cp', '-a', '--reflink=auto', '--', str(child), str(usr/'share')], check=True)
        else:
            subprocess.run(['cp', '-a', '--reflink=auto', '--', str(entry), str(usr)], check=True)
    fixture = usr/'share/legion'
    fixture.mkdir()
    for name in FIXTURE_FILES:
        source = source_usr/'share/legion'/name
        if source.exists():
            copy_file(source, fixture/name)
    copy_file(Path(__file__).with_name('external_proton_bootstrap.py'), fixture/'external_proton_bootstrap.py')
    shutil.copytree(source_usr/'share/legion/etc', fixture/'etc', symlinks=True)
    for relative in ('lib/nvidia/wine', 'lib/nvidia/wine32'):
        target = usr/relative
        if target.is_dir():
            if not target.resolve().is_relative_to(root):
                raise ValueError('optional NVIDIA Windows helpers escape staging')
            shutil.rmtree(target)
    for path in usr.rglob('*'):
        if (getattr(args, 'host_driver_libraries', False)
                and not path.name.startswith('libnvidia-egl-gbm.so')
                and path.name.startswith(('libnvidia-', 'libcuda.so', 'libnvcuvid.so',
                                          'libGLX_nvidia.so', 'libEGL_nvidia.so'))):
            if path.is_file() or path.is_symlink():
                path.unlink()
                continue
        if path.is_file() and path.suffix.lower() in ('.dll', '.exe'):
            raise ValueError('Windows binaries are excluded from this alpha: '+str(path.relative_to(usr)))
    manifest = dict(schema=1, nvidia_driver=args.nvidia_driver,
        qemu='libexec/qemu-system-x86_64', qemu_data='share/qemu',
        backend='libexec/vhost-user-nvgpu', virtiofsd='libexec/virtiofsd',
        host_loader='host/lib/'+loaders[0], host_libraries='host/lib',
        kernel='guest/kernel', init='guest/init', modules=modules,
        userspace='guest/usr', workload_config='guest/workload.json',
        host_driver_libraries=getattr(args, 'host_driver_libraries', False))
    (root/'runtime.json').write_text(json.dumps(manifest, indent=2)+'\n')
    (root/'README.txt').write_text(Path(__file__).parents[1].joinpath('README.md').read_text())
    licenses = root/'licenses'
    licenses.mkdir()
    for source in args.license:
        copy_file(source, licenses/source.parent.name/source.name)
    packages = host_notices(root, host)
    provenance = dict(schema=1, scope='Portable NVIDIA runtime',
        nvidia_driver=args.nvidia_driver,
        host_libraries=sorted(host), host_library_packages=packages,
        packaged_proton=False, packaged_wine=False,
        packaged_games=False, packaged_prefixes=False, nvidia_windows_helpers=False,
        limitation='Application startup requires a matching compatibility recipe.')
    (root/'BUILD.json').write_text(json.dumps(provenance, indent=2)+'\n')
    records = []
    for path in sorted(root.rglob('*')):
        if path.is_symlink() or not path.is_file():
            continue
        relative = path.relative_to(root).as_posix()
        if any(character in relative for character in '\n\r\\'):
            raise ValueError('unsupported checksum filename')
        with path.open('rb') as source:
            digest = hashlib.file_digest(source, 'sha256').hexdigest()
        records.append(f'{digest}  {relative}\n')
    (root/'SHA256SUMS').write_text(''.join(records))
    print(json.dumps(dict(output=str(root), files=len(records), windows_files=0)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('launcher', 'qemu', 'qemu-data', 'backend', 'virtiofsd', 'kernel', 'init',
                 'workload-config', 'userspace', 'output'):
        parser.add_argument('--'+name, required=True, type=Path)
    parser.add_argument('--module', action='append', required=True, type=Path)
    parser.add_argument('--kernel-config', type=Path,
                        help='retain the exact built guest kernel configuration')
    parser.add_argument('--license', action='append', default=[], type=Path)
    parser.add_argument('--nvidia-driver', required=True)
    parser.add_argument('--host-driver-libraries', action='store_true',
                        help='use read-only installed NVIDIA binaries instead of shipping copies')
    args = parser.parse_args()
    if not re.fullmatch(r'[0-9]+(?:\.[0-9]+)+', args.nvidia_driver):
        parser.error('invalid NVIDIA driver version')
    build(args)


if __name__ == '__main__':
    main()
