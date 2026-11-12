#!/usr/bin/env python3
"""Replace build-host DSOs with checksum-verified baseline Arch packages."""
import argparse,hashlib,json,re,shutil,subprocess,tarfile,urllib.request
from pathlib import Path


def index(path):
    records={}
    with tarfile.open(path) as archive:
        for member in archive:
            if not member.name.endswith('/desc'): continue
            text=archive.extractfile(member).read().decode()
            fields={block.splitlines()[0].strip('%'):block.splitlines()[1:] for block in text.strip().split('\n\n')}
            records[fields['NAME'][0]]={k:v[0] for k,v in fields.items() if v}
    return records


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for n in ('source','output','cache'): p.add_argument('--'+n,type=Path,required=True)
    p.add_argument('--resume',action='store_true')
    p.add_argument('--mirror',default='https://mirror.zyner.org/archlinux')
    a=p.parse_args();a.cache.mkdir(parents=True,exist_ok=True)
    if a.output.exists() and not a.resume:raise ValueError('output must be new')
    if not a.output.exists(): subprocess.run(['cp','-a','--reflink=auto',str(a.source.resolve()),str(a.output.resolve())],check=True)
    repos={repo:index(Path('/var/lib/pacman/sync')/(repo+'.db')) for repo in ('core','extra')}
    info=json.loads((a.source/'BUILD.json').read_text())
    names=set(info['host_library_packages'])|set(json.loads((a.source/'host/amd/packages.json').read_text()))|{'virtiofsd','sdl3'}
    libroot=a.cache/'baseline';libroot.mkdir(exist_ok=True)
    packages={}
    def extract(name):
        if name in packages:return
        repo=next((r for r,items in repos.items() if name in items),None)
        if repo is None:raise ValueError('no baseline package: '+name)
        entry=repos[repo][name];filename=entry['FILENAME'];file=a.cache/filename
        if not file.exists():
            url=f'{a.mirror}/{repo}/os/x86_64/{filename}'
            print('download',filename,flush=True)
            try: urllib.request.urlretrieve(url,file)
            except urllib.error.HTTPError as error:
                if error.code!=404:raise
                urllib.request.urlretrieve(f'https://archive.archlinux.org/packages/{name[0]}/{name}/{filename}',file)
        checksum=hashlib.file_digest(file.open('rb'),'sha256').hexdigest()
        if checksum!=entry['SHA256SUM']:raise ValueError('checksum mismatch: '+filename)
        # Package archives are inspected/extracted by bsdtar into an isolated
        # build directory. No scripts or package hooks are executed.
        subprocess.run(['bsdtar','-xf',str(file.resolve()),'-C',str(libroot.resolve()),'usr/lib','usr/bin','usr/share/licenses'],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,check=False)
        packages[name]=dict(version=entry['VERSION'],sha256=checksum,repository=repo,architecture=entry['ARCH'])
    for name in sorted(names):extract(name)
    def find(name):
        direct=libroot/'usr/lib'/name
        if direct.exists():return direct
        matches=[p for p in (libroot/'usr/lib').rglob(name) if p.exists()]
        if not matches:return None
        return matches[0]
    for relative in ('host/lib','host/amd/lib'):
        directory=a.output/relative
        pending=[p.name for p in directory.iterdir() if p.is_file()];done=set()
        if relative=='host/amd/lib':pending.append('libSDL3.so.0')
        while pending:
            name=pending.pop()
            if name in done:continue
            baseline=find(name)
            if baseline is None:
                owner=subprocess.run(['pacman','-Qqo','/usr/lib/'+name],capture_output=True,text=True)
                package=owner.stdout.strip() if owner.returncode==0 else {'libSDL3.so.0':'sdl3'}.get(name)
                if not package:raise ValueError('no baseline owner: '+name)
                extract(package);baseline=find(name)
            if baseline is None:raise ValueError('missing baseline DSO: '+name)
            needed=subprocess.check_output(['readelf','-d',str(baseline.resolve())],text=True)
            pending.extend(re.findall(r'NEEDED.*\[(.*?)\]',needed))
            old=directory/name;old.unlink(missing_ok=True);shutil.copy2(baseline.resolve(strict=True),old);done.add(name)
    virtio=next(libroot.rglob('virtiofsd'),None)
    if virtio is None:raise ValueError('baseline virtiofsd missing')
    (a.output/'libexec/virtiofsd').unlink()
    shutil.copy2(virtio,a.output/'libexec/virtiofsd')
    notices=libroot/'usr/share/licenses'
    if notices.exists():shutil.copytree(notices,a.output/'licenses/portable-host',dirs_exist_ok=True,symlinks=True)
    info['host_runtime_cpu_baseline']='x86_64 baseline repository packages; Haswell execution test required'
    info['portable_host_packages']=packages
    (a.output/'BUILD.json').write_text(json.dumps(info,indent=2)+'\n')
    (a.cache/'packages.json').write_text(json.dumps(packages,indent=2)+'\n')

if __name__=='__main__':main()
