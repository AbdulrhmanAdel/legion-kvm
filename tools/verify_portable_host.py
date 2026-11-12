#!/usr/bin/env python3
"""Execute packaged host tools under non-AVX512 CPU models before distribution."""
import argparse,json,subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--runtime',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
p.add_argument('--qemu-user',default='qemu-x86_64')
a=p.parse_args();r=a.runtime.resolve();checks={}
for cpu in ('Haswell,-hle,-rtm','Westmere'):
    tools=[('qemu-system-x86_64','host/lib','--version'),('qemu-amdgpu','host/amd/lib','--version'),('virtiofsd','host/lib','--version'),('vhost-user-nvgpu','host/lib','--help')]
    for name,lib,arg in tools:
        command=[a.qemu_user,'-cpu',cpu,str(r/lib/'ld-linux-x86-64.so.2'),'--library-path',str(r/lib),str(r/'libexec'/name),arg]
        reply=subprocess.run(command,capture_output=True,text=True,timeout=30)
        checks[cpu+':'+name]={'passed':reply.returncode==0,'returncode':reply.returncode,'stderr':reply.stderr,'stdout':reply.stdout[:300]}
report={'schema':1,'passed':all(c['passed'] for c in checks.values()),'scope':'Emulated host-tool startup without AVX512; not a physical gameplay qualification','checks':checks}
a.output.write_text(json.dumps(report,indent=2)+'\n')
if not report['passed']:raise SystemExit('portable host startup checks failed: '+str(a.output))
print('all 8 non-AVX512 host-tool startup checks passed')
