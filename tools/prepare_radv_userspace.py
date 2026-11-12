#!/usr/bin/env python3
"""Add RADV and its exact ELF dependency closure to a private guest baseline."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess


def needed(path):
    text = subprocess.check_output(['readelf', '-d', str(path)], text=True)
    return re.findall(r'\(NEEDED\).*\[([^]]+)\]', text)


def prepare(baseline, radv, sysroot, output):
    baseline = baseline.resolve(strict=True)
    radv = radv.resolve(strict=True)
    sysroot = sysroot.resolve(strict=True)
    output = output.absolute()
    if output.exists() or output.is_symlink():
        raise ValueError('output must be a new private userspace directory')
    if not (baseline/'bin/vulkaninfo').is_file():
        raise ValueError('baseline lacks vulkaninfo')
    if not (radv/'result.json').is_file():
        raise ValueError('supply the completed RADV build directory')
    build = json.loads((radv/'result.json').read_text())
    if build['amdgpu_virtio'] is not True or build['cpu_baseline'] != 'x86-64':
        raise ValueError('RADV build lacks the required guest policy')
    library = radv/'stage/usr/lib/x86_64-linux-gnu/libvulkan_radeon.so'
    if hashlib.sha256(library.read_bytes()).hexdigest() != build['sha256']:
        raise ValueError('RADV build checksum mismatch')
    output.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(['cp', '-a', '--reflink=auto', '--', str(baseline), str(output)], check=True)
    destination = output/'lib/x86_64-linux-gnu'
    destination.mkdir(parents=True, exist_ok=True)
    if not destination.resolve().is_relative_to(output.resolve()):
        raise ValueError('baseline library directory escapes staging')
    queue = [library]
    copied, aliases = {}, {}
    roots = [sysroot/'usr/lib/x86_64-linux-gnu', sysroot/'lib/x86_64-linux-gnu']
    while queue:
        source = queue.pop()
        name = source.name
        if name in copied:
            continue
        target = destination/name
        if target.exists() or target.is_symlink():
            target.unlink()
        shutil.copy2(source, target)
        copied[name] = hashlib.sha256(target.read_bytes()).hexdigest()
        for name in needed(source):
            if Path(name).name != name or not name:
                raise ValueError('unsupported dependency name')
            candidates = [root/name for root in roots]
            match = next((path for path in candidates if path.is_file()), None)
            if match is None:
                raise ValueError('sysroot lacks runtime dependency '+name)
            real = match.resolve(strict=True)
            if not real.is_relative_to(sysroot):
                raise ValueError('sysroot dependency escapes its root')
            aliases[name] = real.name
            queue.append(real)
    for name, real in aliases.items():
        if name == real:
            continue
        target = destination/name
        if target.exists() or target.is_symlink():
            target.unlink()
        target.symlink_to(real)
    icds = output/'share/vulkan/icd.d'
    if not icds.resolve().is_relative_to(output.resolve()):
        raise ValueError('ICD staging directory escapes baseline')
    icds.mkdir(parents=True, exist_ok=True)
    for path in icds.iterdir():
        if path.is_file() or path.is_symlink():
            path.unlink()
    source_icds = list((radv/'stage/usr/share/vulkan/icd.d').glob('*radeon*.json'))
    if len(source_icds) != 1:
        raise ValueError('RADV build must supply exactly one ICD manifest')
    icd = json.loads(source_icds[0].read_text())
    icd['ICD']['library_path'] = '/usr/lib/x86_64-linux-gnu/libvulkan_radeon.so'
    (icds/'radeon_icd.json').write_text(json.dumps(icd, indent=2)+'\n')
    notices = output/'share/doc/legion-radv'
    shutil.copytree(radv/'stage/usr/share/doc/legion-radv', notices, dirs_exist_ok=True)
    shutil.copy2(radv/'result.json', notices/'build.json')
    record = dict(userspace=str(output.resolve()), mesa_version=build['mesa_version'],
                  amdgpu_virtio=True, libraries=copied, aliases=aliases,
                  hardware_rendering_verified=False, host_installation=False)
    (output/'share/legion-radv.json').write_text(json.dumps(record, indent=2)+'\n')
    print(json.dumps(dict(userspace=record['userspace'], shared_libraries=len(copied),
                         driver='RADV with AMDGPU virtio', hardware_rendering_verified=False)))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('baseline', 'radv', 'sysroot', 'output'):
        parser.add_argument('--'+name, type=Path, required=True)
    args = parser.parse_args()
    prepare(args.baseline, args.radv, args.sysroot, args.output)
