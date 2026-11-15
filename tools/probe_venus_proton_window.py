#!/usr/bin/env python3
"""Exercise Proton's Wayland swapchain through Venus and the guest KMS display.

Runs the owned D3D12 fixture or a private copy of an explicitly selected app.
Captures only the VM framebuffer through QMP, or its exact named Wayland
toplevel using the optional capture-toplevel helper and grim -T.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
from types import SimpleNamespace

import benchmark_venus as vulkan
from benchmark_venus_proton import environment

ROOT=Path(__file__).resolve().parents[1]


def qmp_capture(socket_path, output):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(3)
        client.connect(str(socket_path))
        stream=client.makefile('rwb',buffering=0)
        json.loads(stream.readline())
        for request in [dict(execute='qmp_capabilities'),dict(execute='screendump',arguments=dict(filename=str(output),format='png'))]:
            stream.write((json.dumps(request)+'\n').encode())
            while True:
                result=json.loads(stream.readline())
                if 'event' not in result:
                    break
            if 'error' in result:
                raise RuntimeError(str(result['error']))


def inspect_capture(path):
    from PIL import Image
    with Image.open(path) as image:
        colors=image.convert('RGB').getcolors(maxcolors=32)
        return dict(nonuniform=colors is None,sha256=vulkan.sha(path),width=image.width,height=image.height)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference-session',required=True,type=Path)
    parser.add_argument('--output',required=True,type=Path)
    parser.add_argument('--proton',required=True,type=Path)
    parser.add_argument('--application-dir',type=Path)
    parser.add_argument('--executable',default='Benchmark.exe')
    parser.add_argument('--argument',action='append',default=[])
    parser.add_argument('--compatdata-seed',type=Path)
    parser.add_argument('--memory-mib',type=int,default=4096)
    parser.add_argument('--timeout',type=int,default=300)
    parser.add_argument('--qemu',type=Path,default=ROOT/'build/qemu/qemu-system-x86_64')
    parser.add_argument('--launcher',type=Path,default=ROOT/'build/target/x86_64-unknown-linux-musl/release/legion')
    parser.add_argument('--renderer-prefix',type=Path,default=ROOT/'build/venus/renderer-prefix')
    parser.add_argument('--ring-spin-us',type=int,default=0)
    parser.add_argument('--venus-driver',type=Path,default=ROOT/'build/venus/stage/usr/lib/x86_64-linux-gnu/libvulkan_virtio.so')
    parser.add_argument('--venus-perf',nargs='+',default=[],
        choices=['no_timeline_sync','no_async_queue_submit','no_multi_ring','no_semaphore_feedback','no_query_feedback'])
    parser.add_argument('--feedback-spin-us',type=int,default=0)
    parser.add_argument('--exception-trace',action='store_true')
    parser.add_argument('--audio',action='store_true',help='provide the same guest desktop audio device as normal game launches')
    parser.add_argument('--diagnostic-shader-model',choices=['6_6'],
        help='capability-gate experiment only; does not establish missing feature support')
    args=parser.parse_args()
    if not 2048<=args.memory_mib<=12288 or not 60<=args.timeout<=360 or not 0<=args.ring_spin_us<=200 or not 0<=args.feedback_spin_us<=200:
        parser.error('memory or duration outside bounded probe limits')
    if args.feedback_spin_us and 'no_timeline_sync' not in args.venus_perf:
        parser.error('feedback polling requires the explicit no_timeline_sync experiment')
    args.venus_driver=args.venus_driver.resolve(strict=True)
    if args.feedback_spin_us and b'VN_FEEDBACK_SPIN_US' not in args.venus_driver.read_bytes():
        parser.error('Venus driver lacks the experimental feedback-poll patch')
    if not args.application_dir and (args.argument or args.compatdata_seed):
        parser.error('application arguments/state require --application-dir')
    if args.diagnostic_shader_model and not args.application_dir:
        parser.error('shader-model gate experiment requires an explicit application')
    if Path(args.executable).is_absolute() or '..' in Path(args.executable).parts or not args.executable.lower().endswith('.exe'):
        parser.error('executable must be a relative Windows executable')
    args.output=args.output.resolve();args.proton=args.proton.resolve(strict=True)
    os.umask(0o077)
    args.output.mkdir(parents=True,exist_ok=False)
    common=SimpleNamespace(reference_session=args.reference_session.resolve(),runtime=ROOT/'build/runtime',
        launcher=args.launcher.resolve(strict=True),workload=ROOT/'build/target/release/legion-proton-bootstrap',
        init=ROOT/'build/target/x86_64-unknown-linux-musl/release/legion-guest-probe',render_node='/dev/dri/renderD128',
        venus_driver=args.venus_driver)
    usr=vulkan.prepare(common,args.output);fixture=usr/'share/legion'
    for name,source in [('bootstrap',common.workload),('compositor',ROOT/'build/target/release/legion-guest-compositor'),
            ('workload.exe',ROOT/'build/benchmarks/windows-target/x86_64-pc-windows-msvc/release/legion-proton-workload.exe')]:
        subprocess.run(['cp',str(source),str(fixture/name)],check=True)
    launch=dict(proton_path='/opt/selected-proton',uid=os.getuid(),gid=os.getgid(),vulkan_driver='venus',
        nvidia_features=False,display_backend='wayland',
        environment={'VKD3D_DEBUG':'info','DXVK_LOG_LEVEL':'info'})
    launch['desktop_audio']=args.audio
    if args.venus_perf:launch['environment']['VN_PERF']=','.join(args.venus_perf)
    if args.feedback_spin_us:launch['environment']['VN_FEEDBACK_SPIN_US']=str(args.feedback_spin_us)
    if args.diagnostic_shader_model:
        launch['environment']['VKD3D_SHADER_MODEL']=args.diagnostic_shader_model
    if args.exception_trace:
        launch['exception_trace']=True
        launch['thread_snapshots']=True
    shares=[dict(tag='proton-tool',directory=str(args.proton),readonly=True)]
    if args.application_dir:
        subprocess.run(['cp','-a','--reflink=auto',str(args.application_dir.resolve(strict=True)),str(args.output/'application')],check=True)
        shares.append(dict(tag='application',directory=str(args.output/'application'),readonly=False))
        launch['application']=dict(executable=args.executable,arguments=args.argument,
            dll_overrides='dxgi,d3d11,d3d12,d3d12core=n;lsteamclient=d;wineopenxr=d')
        if args.compatdata_seed:
            subprocess.run(['cp','-a','--reflink=auto',str(args.compatdata_seed.resolve(strict=True)),str(args.output/'compatdata')],check=True)
            shares.append(dict(tag='proton-compatdata',directory=str(args.output/'compatdata'),readonly=False))
        for name in ['BenchmarkResults.txt','BenchmarkResultsPerFrame.csv']:
            (args.output/'application'/name).unlink(missing_ok=True)
    vulkan.write(fixture/'launch.json',launch)
    vulkan.write(fixture/'config.json',dict(schema=1,width=1920,height=1080,warmup_ms=500,sample_ms=1000,
        cpu_rounds=25000,gpu_clears=1,presentation=dict(seconds=15,resize=False,fragment_iterations=16)))
    budget=json.loads((common.runtime/'guest/workload.json').read_text())
    budget.update(warmup_ms=5000,sample_ms=10000)
    case=budget['cases'][0]
    budget['cases']=[dict(case,name=f'budget-{i}') for i in range(max(1,(args.timeout-90+14)//15))]
    vulkan.write(args.output/'config.json',budget)
    vulkan.write(args.output/'proton-shares.json',dict(schema=1,directories=shares))
    vulkan.write(args.output/'kms.json',dict(schema=1,client='external',application=bool(args.application_dir),width=1920,height=1080,diagnostic_shm=args.exception_trace))
    renderer=args.renderer_prefix.resolve(strict=True)
    if args.ring_spin_us and b'LEGION_VENUS_RING_SPIN_US' not in (renderer/'libexec/virgl_render_server').read_bytes():
        parser.error('renderer lacks the experimental ring-poll patch')
    env=environment();env.update(LD_LIBRARY_PATH=str(renderer/'lib'),LEGION_VENUS_SYNC_FD='1')
    if args.ring_spin_us:env['LEGION_VENUS_RING_SPIN_US']=str(args.ring_spin_us)
    trial=args.output/'trial';trial.mkdir();frames=trial/'vm/frames'
    command=vulkan.command(common,'venus',args.output,trial,usr)
    command[command.index('--qemu')+1]=str(args.qemu.resolve(strict=True))
    command+=['--qemu-data',str(common.runtime/'share/qemu')]
    command[command.index('--memory-mib')+1]=str(args.memory_mib)
    command+=['--shares',str(args.output/'proton-shares.json'),'--presentation-config',str(args.output/'kms.json'),
        '--window','true','--window-renderer','desktop','--window-system','wayland','--application',str(bool(args.application_dir)).lower()]
    if args.audio:
        command+=['--audio','desktop']
    metadata=dict(schema=1,proton=str(args.proton),proton_sha256=vulkan.sha(args.proton/'proton'),
        venus_driver_sha256=vulkan.sha(common.venus_driver),renderer_worker_sha256=vulkan.sha(renderer/'libexec/virgl_render_server'),
        wine_repair=False,cpu_adapter=False,arguments=args.argument,application_source=str(args.application_dir),
        ring_spin_us=args.ring_spin_us,
        venus_perf=args.venus_perf,feedback_spin_us=args.feedback_spin_us,
        diagnostic_shader_model=args.diagnostic_shader_model,
        memory_mib=args.memory_mib,qemu=str(args.qemu.resolve()),qemu_sha256=vulkan.sha(args.qemu),
        scope='experimental Venus guest KMS scanout; VM captures only; no game-performance parity claim',captures=[])
    if args.application_dir:
        metadata['executable_sha256']=vulkan.sha(args.output/'application'/args.executable)
    start=time.monotonic()
    with tempfile.TemporaryDirectory(prefix='legion-venus-window-') as sockets:
        qmp=Path(sockets)/'qmp.sock';command+=['--qmp',str(qmp)]
        vulkan.write(args.output/'command.json',command)
        with (args.output/'launch.log').open('w') as log:
            child=subprocess.Popen(command,env=env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
            last_capture=0
            try:
                while child.poll() is None:
                    elapsed=time.monotonic()-start
                    if elapsed>args.timeout:
                        metadata['timed_out']=True
                        break
                    if (frames/'kms-first-frame.json').exists() and elapsed-last_capture>=10:
                        last_capture=elapsed
                        output=args.output/f'vm-{int(elapsed):03d}.png'
                        try:
                            qmp_capture(qmp,output)
                            metadata['captures'].append(dict(seconds=elapsed,path=str(output)))
                        except (OSError,RuntimeError,ValueError) as error:
                            metadata.setdefault('capture_errors',[]).append(str(error))
                            helper=ROOT/'build/venus/capture-toplevel'
                            if helper.is_file():
                                try:
                                    title=f'QEMU (legion-venus-probe-{child.pid}-0)'
                                    identifier=subprocess.check_output([str(helper),title],text=True,timeout=5).strip()
                                    if not identifier or '\n' in identifier:
                                        raise RuntimeError('VM toplevel is ambiguous')
                                    subprocess.run(['grim','-T',identifier,str(output)],check=True,timeout=5,capture_output=True)
                                    metadata['captures'].append(dict(seconds=elapsed,path=str(output),source='exact_vm_toplevel'))
                                except (OSError,RuntimeError,subprocess.SubprocessError) as capture_error:
                                    metadata.setdefault('capture_errors',[]).append(str(capture_error))
                    time.sleep(.25)
            finally:
                if child.poll() is None:
                    os.killpg(child.pid,signal.SIGTERM)
                    try:child.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid,signal.SIGKILL);child.wait()
        metadata.update(exit_code=child.returncode,elapsed_seconds=time.monotonic()-start)
    for name in ['kms-result.json','proton-forward-result.json','workload-result.json']:
        if (frames/name).is_file():
            metadata[name]=json.loads((frames/name).read_text())
    metadata['scanout_verified']=metadata.get('kms-result.json',{}).get('completed_scanouts',0)>30
    for capture in metadata['captures']:
        try:
            capture['image_analysis']=inspect_capture(Path(capture['path']))
        except (ImportError,OSError,ValueError) as error:
            capture['image_analysis_error']=str(error)
    metadata['visible_nonuniform_frame_observed']=any(c.get('image_analysis',{}).get('nonuniform',False) for c in metadata['captures'])
    metadata['transport_passed']=child.returncode==0 and metadata['scanout_verified']
    metadata['passed']=metadata['transport_passed'] and metadata['visible_nonuniform_frame_observed']
    vulkan.write(args.output/'result.json',metadata)
    print(json.dumps(metadata,indent=2))
    if not metadata['passed']:
        raise SystemExit('Venus presentation check failed; inspect launch.log and guest frames logs')


if __name__=='__main__':
    main()
