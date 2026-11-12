#!/usr/bin/env python3
"""Add explicit AMD assets to a copy of an existing Legion runtime."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
from package_runtime import libraries, host_notices, copy_file, strip_release


def main():
    p=argparse.ArgumentParser()
    for name in ('base','output','launcher','qemu','kernel','init','userspace','compositor','cube'):
        p.add_argument('--'+name,type=Path,required=True)
    p.add_argument('--module',type=Path,action='append',required=True)
    a=p.parse_args()
    root=a.output.resolve()
    if root.exists(): raise ValueError('output already exists')
    subprocess.run(['cp','-a','--reflink=auto',str(a.base.resolve()),str(root)],check=True)
    # Replace hardlinked entry points together; do not touch the source runtime.
    for name in ('legion','legion-qemu','legion-backend','legion-virtiofsd','legion-amdgpu-qemu'):
        dst=root/'bin'/name
        dst.unlink(missing_ok=True)
        if name=='legion': copy_file(a.launcher,dst)
        else: os.link(root/'bin/legion',dst)
    copy_file(a.qemu,root/'libexec/qemu-amdgpu')
    strip_release(root/'libexec/qemu-amdgpu')
    copy_file(a.kernel,root/'guest/amd/kernel')
    copy_file(a.init,root/'guest/amd/init')
    modules=[]
    for i, module in enumerate(a.module):
        rel=f'guest/amd/modules/{i:03d}.ko'
        copy_file(module,root/rel)
        modules.append(rel)
    usr=root/'guest/amd/usr'
    shutil.copytree(a.userspace,usr,symlinks=True)
    # Keep established launcher repairs and adapters, with RADV-qualified tools.
    fixture=usr/'share/legion'
    shutil.copytree(root/'guest/usr/share/legion',fixture,symlinks=True,dirs_exist_ok=True)
    for name in ('bootstrap','audio-runtime'):
        copy_file(a.userspace/'share/legion'/name,fixture/name)
    copy_file(a.compositor,fixture/'legion-guest-compositor')
    copy_file(a.compositor,fixture/'compositor')
    copy_file(a.cube,usr/'bin/vkcube')
    host=libraries([a.qemu.resolve()])
    loaders=[n for n in host if n.startswith('ld-linux-x86-64')]
    if len(loaders)!=1: raise ValueError('expected one host loader')
    for name, path in host.items(): copy_file(path,root/'host/amd/lib'/name)
    packages=host_notices(root/'host/amd',host)
    manifest=json.loads((root/'runtime.json').read_text())
    manifest['amdgpu']=dict(qemu='libexec/qemu-amdgpu',kernel='guest/amd/kernel',init='guest/amd/init',
        modules=modules,userspace='guest/amd/usr',host_loader='host/amd/lib/'+loaders[0],host_libraries='host/amd/lib')
    (root/'runtime.json').write_text(json.dumps(manifest,indent=2)+'\n')
    (root/'host/amd/packages.json').write_text(json.dumps(packages,indent=2)+'\n')
    print(root)

if __name__=='__main__': main()
