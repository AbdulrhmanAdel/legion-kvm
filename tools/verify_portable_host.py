#!/usr/bin/env python3
"""Check packaged host-tool startup without AVX-512."""
import argparse
import json
from pathlib import Path
import subprocess


def verify(runtime, output, emulator='qemu-x86_64'):
    runtime=runtime.resolve()
    manifest=json.loads((runtime/'runtime.json').read_text())
    amd=manifest['amdgpu']
    tools=[('qemu',manifest['qemu'],manifest,'--version'),
           ('qemu-amdgpu',amd['qemu'],amd,'--version'),
           ('virtiofsd',manifest['virtiofsd'],manifest,'--version'),
           ('nvidia-backend',manifest['backend'],manifest,'--help')]
    checks={}
    for cpu in ('Haswell,-hle,-rtm','Westmere'):
        for name,program,assets,argument in tools:
            paths=[runtime/assets[key] for key in ('host_loader','host_libraries')]
            binary=runtime/program
            for path in [*paths,binary]:
                if not path.resolve().is_relative_to(runtime):
                    raise ValueError('host runtime asset escapes its directory')
            command=[emulator,'-cpu',cpu,str(paths[0]),'--library-path',str(paths[1]),str(binary),argument]
            reply=subprocess.run(command,capture_output=True,text=True,timeout=30)
            checks[cpu+':'+name]=dict(passed=reply.returncode==0,returncode=reply.returncode,
                                    stderr=reply.stderr,stdout=reply.stdout[:300])
    report=dict(schema=1,passed=all(c['passed'] for c in checks.values()),checks=checks)
    output.write_text(json.dumps(report,indent=2)+'\n')
    if not report['passed']:
        raise RuntimeError('portable host startup checks failed: '+str(output))
    print('All eight non-AVX-512 host-tool checks passed')


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--runtime',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--qemu-user',default='qemu-x86_64')
    a=p.parse_args()
    verify(a.runtime,a.output,a.qemu_user)


if __name__=='__main__':
    main()
