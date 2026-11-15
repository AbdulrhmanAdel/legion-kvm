#!/usr/bin/env python3
"""Run owned D3D11/DXVK and D3D12/VKD3D fixtures through selected Proton.

Offscreen correctness/throughput study; results are not game FPS. Uses fresh
private prefixes and caches, and never modifies installed Proton or game data.
"""
import argparse
import json
import os
from pathlib import Path
import random
import re
import signal
import subprocess
import time
from types import SimpleNamespace

import benchmark_venus as vulkan

ROOT = Path(__file__).resolve().parents[1]

# Independent axes, not a Cartesian product of every extreme. All use the
# existing integer shader and full-image oracle; none add diagnostic kicks.
PROFILES = {
    'submission': dict(width=64, height=64, rounds=1, draws=1, cpu_rounds=25000),
    'submission256': dict(width=256, height=256, rounds=1, draws=1, cpu_rounds=25000),
    'draws64': dict(width=64, height=64, rounds=1, draws=64, cpu_rounds=25000),
    'light1080': dict(width=1920, height=1080, rounds=1, draws=1, cpu_rounds=25000),
    'shader64': dict(width=1920, height=1080, rounds=64, draws=1, cpu_rounds=25000),
    'shader256': dict(width=1920, height=1080, rounds=256, draws=1, cpu_rounds=25000),
    'light4k': dict(width=3840, height=2160, rounds=1, draws=1, cpu_rounds=25000),
    'shader4k': dict(width=3840, height=2160, rounds=256, draws=1, cpu_rounds=25000),
    'overdraw64': dict(width=1920, height=1080, rounds=1, draws=64, cpu_rounds=25000),
    'cpu1m': dict(width=1920, height=1080, rounds=1, draws=1, cpu_rounds=1000000),
}


def fixture_config(args, rounds):
    return dict(schema=1,width=args.width,height=args.height,warmup_ms=args.warmup_ms,
        sample_ms=args.sample_ms,cpu_rounds=args.cpu_rounds,gpu_clears=1,gpu_timestamps=True,
        pipeline=dict(shader_rounds=rounds,draws_per_frame=args.draws_per_frame))


def invoke(command, env, directory, log, timeout):
    vulkan.write(directory/'command.json', command)
    with log.open('w') as stream:
        process = subprocess.Popen(command, env=env, stdout=stream,
            stderr=subprocess.STDOUT, cwd=directory, start_new_session=True)
        try:
            return process.wait(timeout=timeout)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()


def environment():
    env = os.environ.copy()
    for key in list(env):
        if key.startswith(('VK_', 'VN_', 'MANGOHUD', 'LEGION_', 'PROTON_', 'DXVK_', 'VKD3D_', 'WINE')) or key in ('LD_PRELOAD', 'LD_LIBRARY_PATH', 'STEAM_COMPAT_DATA_PATH'):
            env.pop(key, None)
    env.update(VK_DRIVER_FILES='/usr/share/vulkan/icd.d/nvidia_icd.json',
               VK_LOADER_LAYERS_DISABLE='~implicit~', LEGION_STARTUP_UI='log')
    return env


def run(args, common, usr, mode, proton, api, rounds, label):
    trial = args.output/label
    trial.mkdir()
    source = args.d3d11 if api == 'd3d11' else args.d3d12
    config = fixture_config(args, rounds)
    vulkan.write(trial/'fixture-config.json', config)
    env = environment()
    start = time.monotonic()
    try:
        if mode == 'native':
            for name in ['compatdata','steam','cache']:
                (trial/name).mkdir()
            env.update(STEAM_COMPAT_DATA_PATH=str(trial/'compatdata'),
                STEAM_COMPAT_CLIENT_INSTALL_PATH=str(trial/'steam'),STEAM_COMPAT_INSTALL_PATH=str(trial),
                SteamAppId='0',SteamGameId='0',WINEDEBUG='-all',PROTON_ENABLE_WAYLAND='1',
                WINEDLLOVERRIDES='dxgi,d3d11,d3d12,d3d12core=n;lsteamclient=d;wineopenxr=d',
                LEGION_WORKLOAD_DIRECTORY='Z:'+str(trial).replace('/','\\'),
                VKD3D_SHADER_CACHE_PATH=str(trial/'cache'),DXVK_LOG_LEVEL='info',VKD3D_DEBUG='info')
            setup = ['python3',str(proton/'proton'),'getcompatpath',str(source)]
            if invoke(setup,env,trial,trial/'prefix.log',90):
                raise RuntimeError('native prefix setup failed')
            cmd = ['python3',str(proton/'proton'),'runinprefix',str(source)]
            code = invoke(cmd,env,trial,trial/'launch.log',args.timeout)
            frames = trial
        else:
            # All trials are sequential; only private userspace configuration changes.
            fixture = usr/'share/legion'
            subprocess.run(['cp',str(source),str(fixture/'workload.exe')],check=True)
            vulkan.write(fixture/'config.json',config)
            vulkan.write(fixture/'launch.json',dict(proton_path='/opt/selected-proton',uid=os.getuid(),gid=os.getgid(),
                vulkan_driver='venus' if mode=='venus' else 'nvidia',nvidia_features=False,
                environment=dict({'VKD3D_DEBUG':'info','DXVK_LOG_LEVEL':'info'},
                    **({'VN_PERF':','.join(args.venus_perf)} if mode=='venus' and args.venus_perf else {}),
                    **({'VN_FEEDBACK_SPIN_US':str(args.feedback_spin_us)} if mode=='venus' and args.feedback_spin_us else {}))))
            directories = [dict(tag='proton-tool',directory=str(proton),readonly=True)]
            if mode == 'nvidia':
                directories += json.loads((args.output/'shares.json').read_text())['directories']
            vulkan.write(trial/'shares.json',dict(schema=1,directories=directories))
            cmd = vulkan.command(common,mode,args.output,trial,usr)
            cmd[cmd.index('--memory-mib')+1] = str(args.memory_mib)
            if mode == 'venus':
                cmd += ['--shares',str(trial/'shares.json')]
                env.update(LD_LIBRARY_PATH=str(args.renderer_prefix/'lib'),LEGION_VENUS_SYNC_FD='1')
                if args.ring_spin_us:
                    env['LEGION_VENUS_RING_SPIN_US']=str(args.ring_spin_us)
                if args.ring_stats:
                    env['LEGION_VENUS_RING_STATS']='1'
            else:
                cmd[cmd.index('--shares-json')+1] = str(trial/'shares.json')
            # GPU init plus full image verification can exceed the dummy Linux workload's budget.
            code = invoke(cmd,env,trial,trial/'launch.log',args.timeout)
            frames = trial/'vm/frames'
        record = dict(label=label,mode=mode,api=api,proton=str(proton),rounds=rounds,
            exit_code=code,elapsed_seconds=time.monotonic()-start,config=config,passed=False)
        if code:
            raise RuntimeError(f'launcher returned {code}')
        result = json.loads((frames/'workload-result.json').read_text())
        if not (result['passed'] and result['gpu_verified'] and result['vendor_id']==0x10de and result['device_id']==args.device_id):
            raise RuntimeError('GPU correctness or identity check failed')
        if mode != 'native':
            if result['xcr0'] != 7:
                raise RuntimeError('guest CPU contract differs')
            enumeration = (frames/'vulkaninfo.stdout').read_text()
            if ('DRIVER_ID_MESA_VENUS' in enumeration) != (mode=='venus'):
                raise RuntimeError('guest did not use the intended Vulkan driver')
        stderr = '\n'.join(p.read_text(errors='replace') for p in frames.glob('*workload.stderr')) if mode!='native' else (trial/'launch.log').read_text(errors='replace')
        if api=='d3d11' and not re.search(r'DXVK: v',stderr):
            raise RuntimeError('DXVK was not identified in logs')
        if api=='d3d12' and 'vkd3d-proton' not in stderr:
            raise RuntimeError('VKD3D-Proton was not identified in logs')
        vulkan.write(trial/'validated-result.json',result)
        record.update(passed=True,result=result,translation_versions=re.findall(r'(?:DXVK: [^\n]+|vkd3d-proton - [^\n]+)',stderr))
    except (OSError,ValueError,RuntimeError,KeyError,subprocess.SubprocessError) as error:
        record = dict(label=label,mode=mode,api=api,proton=str(proton),rounds=rounds,
            elapsed_seconds=time.monotonic()-start,passed=False,error=str(error))
        owned_log=trial/'vm/frames/proton-workload.stdout'
        if owned_log.exists():
            details=[line for line in owned_log.read_text(errors='replace').splitlines()
                if line.startswith('LEGION_PROTON_WORKLOAD_FAILED:')]
            if details:record['failure_detail']=details[-1]
    vulkan.write(trial/'measurement.json',record)
    print(label,'PASS' if record['passed'] else 'FAIL: '+record.get('error',''),flush=True)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference-session',required=True,type=Path)
    parser.add_argument('--output',required=True,type=Path)
    parser.add_argument('--proton',action='append',required=True,type=Path)
    parser.add_argument('--apis',nargs='+',choices=['d3d11','d3d12'],default=['d3d11','d3d12'])
    parser.add_argument('--modes',nargs='+',choices=['venus','nvidia','native'],default=['venus'])
    parser.add_argument('--rounds',nargs='+',type=int,default=[1,64])
    parser.add_argument('--profiles',nargs='+',choices=list(PROFILES),
        help='D3D12 stress profiles; each still measures queue depths 1–3, with and without CPU work')
    parser.add_argument('--draws-per-frame',type=int,default=1)
    parser.add_argument('--cpu-rounds',type=int,default=25000)
    parser.add_argument('--memory-mib',type=int,default=2048)
    parser.add_argument('--ring-spin-us',type=int,default=0,
        help='experimental renderer active-poll budget before its original backoff (0..200 microseconds)')
    parser.add_argument('--ring-stats',action='store_true',help='instrument renderer ring polling; profiling only')
    parser.add_argument('--venus-perf',nargs='+',default=[],
        choices=['no_timeline_sync','no_async_queue_submit','no_multi_ring','no_semaphore_feedback','no_query_feedback'],
        help='explicit Mesa Venus diagnostic performance options; never set by normal launches')
    parser.add_argument('--feedback-spin-us',type=int,default=0)
    parser.add_argument('--venus-driver',type=Path,default=ROOT/'build/venus/stage/usr/lib/x86_64-linux-gnu/libvulkan_virtio.so')
    parser.add_argument('--repetitions',type=int,default=3)
    parser.add_argument('--width',type=int,default=1920)
    parser.add_argument('--height',type=int,default=1080)
    parser.add_argument('--warmup-ms',type=int,default=1000)
    parser.add_argument('--sample-ms',type=int,default=3000)
    parser.add_argument('--timeout',type=int,default=180)
    parser.add_argument('--renderer-prefix',type=Path,default=ROOT/'build/venus/renderer-prefix')
    parser.add_argument('--d3d11',type=Path,default=ROOT/'build/venus/windows-dx11/x86_64-pc-windows-msvc/release/legion-proton-workload.exe')
    parser.add_argument('--d3d12',type=Path,default=ROOT/'build/benchmarks/windows-target/x86_64-pc-windows-msvc/release/legion-proton-workload.exe')
    args = parser.parse_args()
    if not (1<=args.repetitions<=5 and all(1<=n<=256 for n in args.rounds)
            and 64<=args.width<=3840 and 64<=args.height<=2160
            and 1<=args.draws_per_frame<=64 and 1<=args.cpu_rounds<=1000000
            and 2048<=args.memory_mib<=12288
            and 0<=args.ring_spin_us<=200
            and 0<=args.feedback_spin_us<=200
            and 100<=args.warmup_ms<=5000 and 500<=args.sample_ms<=10000 and 60<=args.timeout<=300):
        parser.error('configuration outside bounded probe limits')
    if (args.profiles or args.draws_per_frame!=1) and args.apis!=['d3d12']:
        parser.error('stress profiles and multiple draws require --apis d3d12; the DXVK fixture uses one draw')
    if args.feedback_spin_us and 'no_timeline_sync' not in args.venus_perf:
        parser.error('feedback polling requires the explicit no_timeline_sync experiment')
    args.venus_driver=args.venus_driver.resolve(strict=True)
    if args.feedback_spin_us and b'VN_FEEDBACK_SPIN_US' not in args.venus_driver.read_bytes():
        parser.error('Venus driver lacks the experimental bounded feedback-poll patch')
    args.output=args.output.resolve();args.renderer_prefix=args.renderer_prefix.resolve()
    args.proton=[path.resolve(strict=True) for path in args.proton]
    args.d3d11=args.d3d11.resolve();args.d3d12=args.d3d12.resolve()
    if b'LEGION_VENUS_SYNC_FD' not in (args.renderer_prefix/'libexec/virgl_render_server').read_bytes():
        parser.error('renderer prefix lacks the experimental sync-file patch')
    if (args.ring_spin_us or args.ring_stats) and b'LEGION_VENUS_RING_SPIN_US' not in (args.renderer_prefix/'libexec/virgl_render_server').read_bytes():
        parser.error('renderer prefix lacks the experimental ring-poll patch')
    args.device_id=int(Path('/sys/class/drm/renderD128/device/device').read_text(),0)
    args.output.mkdir(parents=True,exist_ok=False)
    common=SimpleNamespace(reference_session=args.reference_session.resolve(),runtime=ROOT/'build/runtime',
        launcher=ROOT/'build/target/x86_64-unknown-linux-musl/release/legion',workload=ROOT/'build/target/release/legion-proton-bootstrap',
        init=ROOT/'build/target/x86_64-unknown-linux-musl/release/legion-guest-probe',render_node='/dev/dri/renderD128',
        venus_driver=args.venus_driver)
    usr=vulkan.prepare(common,args.output)
    fixture=usr/'share/legion'
    for name,source in [('bootstrap',common.workload),('compositor',ROOT/'build/target/release/legion-guest-compositor')]:
        subprocess.run(['cp',str(source),str(fixture/name)],check=True)
    config=json.loads((common.runtime/'guest/workload.json').read_text())
    # The launcher validates this bounded Linux config, while the bootstrap reads its Windows config above.
    config.update(warmup_ms=5000,sample_ms=10000)
    config['cases']*=10
    for i,case in enumerate(config['cases']):
        config['cases'][i]=dict(case,name=f'budget-{i}')
    vulkan.write(args.output/'config.json',config)
    vulkan.write(args.output/'environment.json',dict(schema=1,scope='offscreen Windows D3D through selected Proton; not game FPS or presentation',
        kernel=os.uname().release,gpu=subprocess.check_output(['nvidia-smi','--query-gpu=name,driver_version','--format=csv'],text=True),
        workloads={api:vulkan.sha(getattr(args,api)) for api in args.apis},
        renderer_worker_sha256=vulkan.sha(args.renderer_prefix/'libexec/virgl_render_server'),
        bootstrap_sha256=vulkan.sha(common.workload),wine_repair=False,cpu_adapter=False,
        ring_spin_us=args.ring_spin_us,ring_stats=args.ring_stats,
        venus_perf=args.venus_perf,
        feedback_spin_us=args.feedback_spin_us,venus_driver_sha256=vulkan.sha(args.venus_driver),
        profiles={name:PROFILES[name] for name in args.profiles} if args.profiles else None,
        expected_trials=len(args.proton)*len(args.apis)*len(args.profiles or args.rounds)*len(args.modes)*args.repetitions,
        warmup_ms=args.warmup_ms,sample_ms=args.sample_ms,memory_mib=args.memory_mib,
        limitations=['single GPU/driver','unlocked clocks and background desktop','fresh private prefix/cache for each trial'],
        protons=[dict(path=str(p),entrypoint_sha256=vulkan.sha(p/'proton'),
            graphics_sha256={str(f.relative_to(p)):vulkan.sha(f) for f in [
                p/'files/lib/wine/x86_64-windows/winevulkan.dll',p/'files/lib/wine/x86_64-unix/winevulkan.so',
                p/'files/lib/wine/x86_64-windows/win32u.dll',p/'files/lib/wine/x86_64-unix/win32u.so',
                p/'files/lib/wine/x86_64-windows/winewayland.drv',p/'files/lib/wine/x86_64-unix/winewayland.so']},
            vkd3d_version=(p/'files/lib/wine/vkd3d-proton/version').read_text()) for p in args.proton]))
    records=[]
    for repetition in range(args.repetitions):
        selected=[(name,PROFILES[name]['rounds']) for name in args.profiles] if args.profiles else [(None,n) for n in args.rounds]
        trials=[(mode,p,api,name,n) for p in args.proton for api in args.apis for name,n in selected for mode in args.modes]
        random.Random(71+repetition).shuffle(trials)
        for mode,proton,api,name,rounds in trials:
            current=SimpleNamespace(**vars(args))
            if name:
                profile=PROFILES[name]
                current.width=profile['width'];current.height=profile['height']
                current.cpu_rounds=profile['cpu_rounds'];current.draws_per_frame=profile['draws']
            label=f'{repetition+1:02d}-{proton.name}-{api}-{name or rounds}-{mode}'
            record=run(current,common,usr,mode,proton,api,rounds,label)
            record['profile']=name;record['repetition']=repetition+1
            vulkan.write(args.output/label/'measurement.json',record)
            records.append(record)
            vulkan.write(args.output/'measurements.json',records)
    # The probes check every texel independently; D3D11's fixed seed also permits cross-backend hash equality.
    for p in args.proton:
        for n in args.rounds:
            hashes={r['result']['image_hash_before'] for r in records if r['passed'] and r['api']=='d3d11' and r['proton']==str(p) and r['rounds']==n}
            if len(hashes)>1:
                raise RuntimeError('D3D11 full-image hashes disagree between backends')
    if any(not r['passed'] for r in records):
        raise SystemExit('Some trials failed; inspect measurements.json and trial logs')


if __name__=='__main__':
    main()
