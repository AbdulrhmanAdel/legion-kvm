#!/usr/bin/env python3
"""Fetch, compile and assemble the complete runtime inside the build image."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time
import urllib.request

from build_single_file import build as package
from build_wine_delta import build as wine_delta
from package_runtime import libraries, copy_file
from prepare_slim_userspace import prepare as slim
from runtime_licenses import collect as collect_licenses
from host_abi import install as install_host_abi

ROOT = Path('/src')
OUT = Path('/build')
TARGET = OUT/'target'
ARCH = 'x86_64-linux-gnu'


def run(command, cwd=None, env=None):
    subprocess.run([str(arg) for arg in command],cwd=cwd,env=env,check=True)


def sha(path):
    with path.open('rb') as file:
        return hashlib.file_digest(file,'sha256').hexdigest()


def write(path, value):
    path.parent.mkdir(parents=True,exist_ok=True)
    temporary=path.with_name(path.name+'.tmp')
    temporary.write_text(json.dumps(value,indent=2)+'\n')
    temporary.replace(path)


def tree_hash(paths):
    result=hashlib.sha256()
    for path in paths:
        for file in sorted(path.rglob('*')) if path.is_dir() else [path]:
            if file.is_file() and '__pycache__' not in file.parts:
                result.update(str(file.relative_to(ROOT)).encode())
                result.update(file.read_bytes())
    return result.hexdigest()


def step(name, inputs, action):
    marker=OUT/'state'/(name+'.json')
    recipe=dict(script=sha(Path(__file__)),inputs=inputs)
    fingerprint=hashlib.sha256(json.dumps(recipe,sort_keys=True).encode()).hexdigest()
    if marker.exists() and json.loads(marker.read_text()).get('fingerprint')==fingerprint:
        print('Cached:',name,flush=True)
        return
    print('Building:',name,flush=True)
    marker.unlink(missing_ok=True)
    start=time.monotonic()
    action()
    write(marker,dict(fingerprint=fingerprint,seconds=time.monotonic()-start))


def sources():
    result={}
    for name,record in json.loads((ROOT/'config/sources.json').read_text()).items():
        if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*\.tar\.(xz|gz)',name) \
                or not re.fullmatch(r'[0-9a-f]{64}',record['sha256']):
            raise ValueError('invalid source archive record: '+name)
        if not record['url'].startswith('https://'):
            raise ValueError('source archive requires HTTPS: '+name)
        archive=OUT/'cache'/name
        archive.parent.mkdir(parents=True,exist_ok=True)
        if not archive.exists():
            print('Downloading:',name,flush=True)
            temporary=archive.with_suffix('.part')
            with urllib.request.urlopen(record['url'],timeout=120) as source,temporary.open('wb') as dest:
                shutil.copyfileobj(source,dest)
            temporary.replace(archive)
        if sha(archive)!=record['sha256']:
            raise ValueError('source checksum mismatch: '+name)
        directory=OUT/'sources'/name.split('.tar')[0]
        marker=directory.with_name(directory.name+'.source.json')
        if not (directory.is_dir() and marker.is_file()
                and json.loads(marker.read_text()).get('sha256')==record['sha256']):
            directory.parent.mkdir(parents=True,exist_ok=True)
            marker.unlink(missing_ok=True)
            temporary=Path(tempfile.mkdtemp(prefix=directory.name+'-',dir=directory.parent))
            try:
                run(['tar','-xf',archive,'--strip-components=1','-C',temporary])
                if directory.exists():shutil.rmtree(directory)
                temporary.replace(directory)
                write(marker,dict(sha256=record['sha256']))
            finally:
                if temporary.exists():shutil.rmtree(temporary)
        result[name]=directory
    return result


def patch(source, path):
    command=['patch','--batch','--force','--fuzz=0','-p1','-d',source,'-i',path]
    applied=subprocess.run([str(x) for x in command]+['--reverse','--dry-run'],capture_output=True)
    if applied.returncode:
        run(command)


def meson(source, build, options, prefix='/usr', env=None):
    if not (build/'build.ninja').exists():
        run(['meson','setup',build,source,'--buildtype=release','--prefix='+prefix,
             '--libdir=lib/'+ARCH,'--wrap-mode=nodownload',*options],env=env)
    run(['ninja','-C',build,'-j'+str(JOBS)],env=env)


def rust():
    run(['cargo','build','--locked','--release','--workspace','-j'+str(JOBS)],cwd=ROOT)
    run(['cargo','build','--locked','--release','--target','x86_64-unknown-linux-musl',
         '-p','legion-kvm','-p','legion-guest-probe','-j'+str(JOBS)],cwd=ROOT)
    run(['cargo','build','--locked','--release','--manifest-path',ROOT/'backends/nvidia/Cargo.toml',
         '-p','device','--features','vhost-user','--bin','vhost-user-nvgpu','-j'+str(JOBS)])


def kernel(source):
    run(['python3',ROOT/'kernel/build_guest.py','--source',source,'--output',OUT/'kernel',
         '--native-display','--jobs',JOBS])
    for name,directory in [('nvidia',ROOT/'backends/nvidia/driver'),('cpuid',ROOT/'guest-cpuid')]:
        module=OUT/'modules'/name
        shutil.copytree(directory,module,dirs_exist_ok=True)
        run(['make','-C',OUT/'kernel','M='+str(module),'CONFIG_VIRTIO_GPU_NV=m',
             '-j'+str(JOBS),'modules'])


def virgl(source):
    build=OUT/'virgl'
    meson(source,build,['-Ddrm-renderers=amdgpu-experimental','-Dvenus=false','-Dtests=false'])
    run(['meson','install','-C',build,'--destdir',OUT/'host','--no-rebuild'])


def qemu(source):
    patch(source,ROOT/'patches/qemu-runtime.patch')
    build=OUT/'qemu';build.mkdir(exist_ok=True)
    env=os.environ.copy()
    env['PKG_CONFIG_PATH']=str(OUT/'host/usr/lib'/ARCH/'pkgconfig')
    env['LD_LIBRARY_PATH']=str(OUT/'host/usr/lib'/ARCH)
    # pkg-config must resolve the installed prefix through the staging root.
    pc=OUT/'host/usr/lib'/ARCH/'pkgconfig/virglrenderer.pc'
    pc.write_text(pc.read_text().replace('prefix=/usr','prefix='+str(OUT/'host/usr')))
    if not (build/'build.ninja').exists():
        run([source/'configure','--target-list=x86_64-softmmu','--without-default-features',
            '--enable-kvm','--enable-vhost-user','--disable-tcg','--disable-rust',
            '--disable-tools','--disable-guest-agent','--disable-docs','--disable-werror',
            '--enable-pa','--audio-drv-list=pa','--enable-opengl','--enable-virglrenderer',
            '--enable-sdl','--enable-pixman','--extra-cflags=-march=x86-64 -mtune=generic'],cwd=build,env=env)
    run(['ninja','-C',build,'-j'+str(JOBS),'qemu-system-x86_64'],env=env)


def radv(source):
    build=OUT/'mesa'
    options=['-Damdgpu-virtio=true','-Dvulkan-drivers=amd','-Dgallium-drivers=',
        '-Dplatforms=x11,wayland','-Dllvm=disabled','-Dglx=disabled','-Degl=disabled',
        '-Dgbm=disabled','-Dopengl=false','-Dgles1=disabled','-Dgles2=disabled',
        '-Dgallium-vdpau=disabled','-Dgallium-va=disabled','-Dgallium-xa=disabled',
        '-Dgallium-opencl=disabled','-Dgallium-rusticl=false','-Dvalgrind=disabled',
        '-Dbuild-tests=false','-Dlibunwind=disabled','-Dlmsensors=disabled','-Dvideo-codecs=']
    meson(source,build,options)
    run(['meson','install','-C',build,'--destdir',OUT/'radv','--no-rebuild'])


def wine(source):
    patch(source,ROOT/'patches/wine-runtime.patch')
    build=OUT/'wine';build.mkdir(exist_ok=True)
    if not (source/'configure').exists():run([source/'autogen.sh'],cwd=source)
    if not (build/'Makefile').exists():
        run([source/'configure','--enable-win64','--disable-tests','--without-x','--without-wayland',
             '--without-vulkan','--without-opengl','--without-gstreamer','--without-alsa',
             '--without-pulse','--without-dbus','--without-cups','--without-udev','--without-freetype'],cwd=build)
    run(['make','-C',build,'-j'+str(JOBS),'dlls/ntdll/x86_64-windows/ntdll.dll'])


def guest_userspace():
    usr=OUT/'usr'
    if usr.exists():shutil.rmtree(usr)
    programs=[TARGET/'release'/name for name in ('legion-proton-bootstrap',
        'legion-guest-compositor','legion-audio-runtime','legion-cpu-adapter-fixture',
        'liblegion_cpu_adapter.so')]
    slim(Path('/usr'),usr,[Path('/proton')],programs)
    loader=usr/'lib/ld-linux-x86-64.so.2'
    loader.symlink_to('x86_64-linux-gnu/ld-linux-x86-64.so.2')
    if not loader.is_file():
        raise ValueError('guest dynamic loader is missing')
    (usr/'lib/nvidia/wine').mkdir(parents=True,exist_ok=True)
    gbm=usr/'lib'/ARCH/'gbm'
    gbm.mkdir(parents=True,exist_ok=True)
    (gbm/'nvidia-drm_gbm.so').symlink_to('../../libnvidia-allocator.so.1')
    fixture=usr/'share/legion';fixture.mkdir(parents=True,exist_ok=True)
    for source,name in [('legion-proton-bootstrap','bootstrap'),('legion-guest-compositor','compositor'),
                        ('legion-guest-compositor','legion-guest-compositor'),('legion-audio-runtime','audio-runtime'),
                        ('legion-cpu-adapter-fixture','cpu-adapter-fixture'),
                        ('liblegion_cpu_adapter.so','liblegion_cpu_adapter.so')]:
        copy_file(TARGET/'release'/source,fixture/name)
    copy_file(ROOT/'tools/external_proton_bootstrap.py',fixture/'external_proton_bootstrap.py')
    copy_file(ROOT/'config/launch-profile.json',fixture/'game-launch.json')
    copy_file(ROOT/'config/presentation.json',fixture/'config.json')
    wine_delta(Path('/proton'),OUT/'wine/dlls/ntdll/x86_64-windows/ntdll.dll',fixture/'wine-repair.json')
    shutil.copytree('/etc/fonts',usr/'etc/fonts',dirs_exist_ok=True)
    shutil.copytree('/usr/share/fonts',usr/'share/fonts',dirs_exist_ok=True)
    write(usr/'share/glvnd/egl_vendor.d/10_nvidia.json',{'file_format_version':'1.0.0',
          'ICD':{'library_path':'libEGL_nvidia.so.0'}})
    write(usr/'share/vulkan/icd.d/nvidia_icd.json',{'file_format_version':'1.0.0',
          'ICD':{'library_path':'/usr/lib/libGLX_nvidia.so.0','api_version':'1.4.303'}})
    etc=fixture/'etc';etc.mkdir(exist_ok=True)
    for name,text in {'passwd':'root:x:0:0:root:/root:/bin/sh\n','group':'root:x:0:\n',
        'hosts':'127.0.0.1 localhost\n','nsswitch.conf':'hosts: files dns\n','machine-id':'\n'}.items():
        (etc/name).write_text(text)
    # Keep the RADV dependency closure and ICD separate from host driver bindings.
    driver=OUT/'radv/usr/lib'/ARCH/'libvulkan_radeon.so'
    copy_file(driver,usr/'lib'/ARCH/driver.name)
    for name,path in libraries([driver]).items():
        copy_file(path,usr/'lib'/ARCH/name)
    write(usr/'share/vulkan/icd.d/radeon_icd.json',{'file_format_version':'1.0.0',
          'ICD':{'library_path':'/usr/lib/'+ARCH+'/libvulkan_radeon.so','api_version':'1.4.305'}})


def assemble():
    runtime=OUT/'runtime'
    if runtime.exists():shutil.rmtree(runtime)
    (runtime/'bin').mkdir(parents=True)
    static=TARGET/'x86_64-unknown-linux-musl/release'
    copy_file(static/'legion',runtime/'bin/legion')
    for name in ('legion-qemu','legion-amdgpu-qemu','legion-backend','legion-virtiofsd'):
        os.link(runtime/'bin/legion',runtime/'bin'/name)
    programs=[OUT/'qemu/qemu-system-x86_64',TARGET/'release/vhost-user-nvgpu',Path('/usr/libexec/virtiofsd')]
    if not programs[-1].exists():programs[-1]=Path('/usr/lib/virtiofsd')
    for source,name in zip(programs,('qemu-system-x86_64','vhost-user-nvgpu','virtiofsd')):
        copy_file(source,runtime/'libexec'/name)
        run(['strip','--strip-unneeded',runtime/'libexec'/name])
    env=os.environ.copy();env['LD_LIBRARY_PATH']=str(OUT/'host/usr/lib'/ARCH)
    old=os.environ.get('LD_LIBRARY_PATH');os.environ['LD_LIBRARY_PATH']=env['LD_LIBRARY_PATH']
    try:host=libraries(programs)
    finally:
        if old is None:os.environ.pop('LD_LIBRARY_PATH',None)
        else:os.environ['LD_LIBRARY_PATH']=old
    for name,path in host.items():copy_file(path,runtime/'host/lib'/name)
    install_host_abi(ROOT/'config/host-packages.json',OUT/'cache/host-abi',
                     runtime/'host/lib',runtime/'licenses/host-abi')
    copy_file(OUT/'kernel/arch/x86/boot/bzImage',runtime/'guest/kernel')
    copy_file(static/'legion-guest-probe',runtime/'guest/init')
    copy_file(OUT/'kernel/.config',runtime/'guest/kernel.config')
    order=[OUT/'modules/nvidia/virtio_gpu_nv.ko',OUT/'modules/cpuid/legion_cpuid.ko',OUT/'kernel/drivers/misc/ntsync.ko']
    amd=[OUT/'kernel/drivers/virtio/virtio_dma_buf.ko',OUT/'kernel/drivers/gpu/drm/drm_shmem_helper.ko',
         OUT/'kernel/drivers/gpu/drm/drm_kms_helper.ko',OUT/'kernel/drivers/gpu/drm/virtio/virtio-gpu.ko',*order[1:]]
    module_paths={}
    for index,source in enumerate(dict.fromkeys([*order,*amd])):
        relative=f'guest/modules/{index:03d}.ko';copy_file(source,runtime/relative);module_paths[source]=relative
    shutil.copytree(OUT/'usr',runtime/'guest/usr',symlinks=True)
    firmware=OUT/'sources/qemu-11.1.2/pc-bios'
    for name in ('bios-256k.bin','linuxboot_dma.bin','kvmvapic.bin','multiboot_dma.bin','pvh.bin'):
        copy_file(firmware/name,runtime/'share/qemu'/name)
    write(runtime/'guest/workload.json',{'schema':1,'width':1920,'height':1080,'warmup_ms':200,
        'sample_ms':1000,'cases':[{'name':'render','mode':'gpu','kernel':'integer','workers':0,
        'working_set_kib':512,'work_units':1,'gpu_iterations':1}]})
    common=dict(qemu='libexec/qemu-system-x86_64',kernel='guest/kernel',init='guest/init',
        host_loader='host/lib/ld-linux-x86-64.so.2',host_libraries='host/lib',userspace='guest/usr')
    write(runtime/'runtime.json',dict(schema=1,nvidia_driver='0.0.0',**common,qemu_data='share/qemu',
        backend='libexec/vhost-user-nvgpu',virtiofsd='libexec/virtiofsd',modules=[module_paths[x] for x in order],
        workload_config='guest/workload.json',host_driver_libraries=True,
        amdgpu={**common,'modules':[module_paths[x] for x in amd]}))
    copy_file(ROOT/'README.md',runtime/'README.txt')
    collect_licenses(ROOT,OUT/'sources',Path(os.environ['CARGO_HOME']),runtime/'licenses')
    copy_file(ROOT/'config/sources.json',runtime/'licenses/sources.json')
    copy_file(ROOT/'config/host-packages.json',runtime/'licenses/host-packages.json')
    shutil.copytree(ROOT/'patches',runtime/'licenses/patches',dirs_exist_ok=True)
    for folder in (Path('/usr/share/doc'),Path('/usr/share/licenses'),Path('/usr/share/common-licenses')):
        if folder.exists():shutil.copytree(folder,runtime/'licenses'/folder.name,symlinks=True,dirs_exist_ok=True)
    for source,dest in [(OUT/'sources/wine-c53678ab/LICENSE','wine/LICENSE'),
                        (OUT/'sources/wine-c53678ab/COPYING.LIB','wine/COPYING.LIB'),
                        (OUT/'sources/qemu-11.1.2/COPYING','qemu/COPYING')]:
        copy_file(source,runtime/'licenses'/dest)
    run(['python3',ROOT/'tools/verify_portable_host.py','--runtime',runtime,'--output',OUT/'host-portability.json'])
    destination=OUT/'dist/legion'
    destination.parent.mkdir(exist_ok=True)
    destination.unlink(missing_ok=True)
    package(runtime,static/'legion',destination)


def main():
    global JOBS
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--jobs',type=int,default=2)
    JOBS=p.parse_args().jobs
    (OUT/'home').mkdir(exist_ok=True)
    assets=sources()
    identity=os.environ.get('LEGION_BUILD_IMAGE','')
    step('kernel',[identity,sha(ROOT/'kernel/guest.config'),sha(ROOT/'kernel/virtio-gpu-xbgr8888.patch'),
                   sha(ROOT/'config/sources.json'),sha(ROOT/'kernel/build_guest.py'),
                   tree_hash([ROOT/'guest-cpuid',ROOT/'backends/nvidia/driver'])],lambda:kernel(assets['linux-6.18.62.tar.xz']))
    step('virgl',[identity,sha(ROOT/'config/sources.json')],lambda:virgl(assets['virglrenderer-1.3.0.tar.gz']))
    step('qemu',[identity,sha(ROOT/'config/sources.json'),sha(ROOT/'patches/qemu-runtime.patch')],lambda:qemu(assets['qemu-11.1.2.tar.xz']))
    step('radv',[identity,sha(ROOT/'config/sources.json')],lambda:radv(assets['mesa-25.0.7.tar.xz']))
    step('wine',[identity,sha(ROOT/'config/sources.json'),sha(ROOT/'patches/wine-runtime.patch')],lambda:wine(assets['wine-c53678ab.tar.gz']))
    step('rust',[identity,tree_hash([ROOT/'src',ROOT/'crates',ROOT/'backends/nvidia/device',
                                  ROOT/'backends/nvidia/protocol',ROOT/'backends/nvidia/gen',
                                  ROOT/'backends/nvidia/vklayer',ROOT/'backends/nvidia/Cargo.toml',
                                  ROOT/'backends/nvidia/Cargo.lock',ROOT/'Cargo.toml',ROOT/'Cargo.lock'])],rust)
    guest_userspace()
    assemble()


if __name__=='__main__':main()
