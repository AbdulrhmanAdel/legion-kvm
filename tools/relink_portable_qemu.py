#!/usr/bin/env python3
"""Relink existing generic QEMU objects with baseline C runtime startup objects."""
import argparse,json,shlex,subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
for n in ('build','baseline','output'):p.add_argument('--'+n,type=Path,required=True)
a=p.parse_args()
commands=json.loads(subprocess.check_output(['ninja','-C',str(a.build),'-t','compdb','-x'],text=True))
entry=next(e for e in commands if e.get('output')=='qemu-system-x86_64')
cmd=shlex.split(entry['command']);cmd.insert(1,'-B'+str(a.baseline.resolve())+'/')
cmd[cmd.index('-o')+1]=str(a.output.resolve())
subprocess.run(cmd,cwd=a.build,check=True)
subprocess.run(['strip','--strip-unneeded',str(a.output)],check=True)
a.output.with_suffix('.relink.json').write_text(json.dumps({'build':str(a.build.resolve()),'command':cmd},indent=2)+'\n')
