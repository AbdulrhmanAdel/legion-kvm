#!/usr/bin/env python3
"""Compare native Vulkan, Venus vtest, Venus KVM and Legion NVIDIA forwarding.

Uses an existing doctor --graphics session for read-only NVIDIA bindings.
Results are offscreen Linux Vulkan microbenchmarks, not game FPS estimates.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import re
import signal
import socket
import statistics
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]


def write(path, value):
    path.write_text(json.dumps(value, indent=2)+'\n')


def sha(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def prepare(args, output):
    usr = output/'usr'
    reference = args.reference_session
    baseline = reference/'guest/usr' if (reference/'runtime.json').is_file() else reference/'usr'
    subprocess.run(['cp', '-a', '--reflink=auto', str(baseline), str(usr)], check=True)
    driver = args.venus_driver.resolve(strict=True)
    subprocess.run(['cp', str(driver), str(usr/'lib/x86_64-linux-gnu/libvulkan_virtio.so')], check=True)
    icd = json.loads((driver.parents[2]/'share/vulkan/icd.d/virtio_icd.x86_64.json').read_text())
    icd['ICD']['library_path'] = '/usr/lib/x86_64-linux-gnu/libvulkan_virtio.so'
    write(usr/'share/vulkan/icd.d/virtio_icd.json', icd)
    icd['ICD']['library_path'] = str(driver)
    write(output/'vtest-icd.json', icd)
    bindings_path = reference/'vm/frames/guest-libraries.json'
    bindings = json.loads(bindings_path.read_text()) if bindings_path.is_file() else dict(schema=1, files=[])
    listing = subprocess.check_output(['ldconfig', '-p'], text=True)
    paths = {}
    for line in listing.splitlines():
        if '=>' in line:
            alias, path = line.split('=>', 1)
            paths.setdefault(alias.split()[0], Path(path.strip()).resolve())
            real = Path(path.strip()).resolve()
            paths.setdefault(real.name, real)
    directories = {}
    for file in bindings['files']:
        source = paths[file['target_name']]
        if source.name != file['source_name']:
            raise ValueError('host driver changed since reference session; run doctor --graphics again')
        previous = directories.setdefault(file['tag'], str(source.parent))
        if previous != str(source.parent):
            raise ValueError('inconsistent driver share')
    write(output/'libraries.json', bindings)
    write(output/'shares.json', dict(schema=1, directories=[
        dict(tag=tag, directory=directory, readonly=True) for tag, directory in directories.items()]))
    return usr


def command(args, mode, output, trial, usr):
    runtime = args.runtime.resolve()
    manifest = json.loads((runtime/'runtime.json').read_text())
    if mode in ('native', 'vtest'):
        return [str(args.workload.resolve()), str(output/'config.json'), str(trial/'workload-result.json')]
    venus = mode == 'venus'
    cmd = [str(args.launcher.resolve()), 'drm-native-probe' if venus else 'gpu-workload-probe',
           '--qemu', '/usr/bin/qemu-system-x86_64' if venus else str(runtime/'bin/legion-qemu'),
           '--kernel', str(runtime/manifest['kernel']), '--init', str(args.init.resolve()),
           '--userspace', str(usr), '--virtiofsd', str(runtime/'bin/legion-virtiofsd'),
           '--output', str(trial/'vm'), '--memory-mib', '2048',
           '--workload', str(args.workload.resolve()), '--workload-config', str(output/'config.json')]
    modules = manifest['amdgpu']['modules'] if venus and manifest['schema'] == 1 else manifest['modules']
    if venus:
        cmd += ['--gpu-backend', 'venus', '--render-node', args.render_node]
    else:
        cmd += ['--backend', str(runtime/'bin/legion-backend'), '--gpu-compute', 'true',
                '--shares-json', str(output/'shares.json'), '--driver-libraries-json', str(output/'libraries.json')]
    for module in modules:
        cmd += ['--module', str(runtime/module)]
    return cmd


def cpu_snapshot(groups):
    result = {}
    for path in Path('/proc').glob('[0-9]*/stat'):
        try:
            fields = path.read_text().rsplit(')', 1)[1].split()
            if int(fields[2]) in groups:
                result[(path.parent.name, fields[19])] = int(fields[11])+int(fields[12])
        except (OSError, ValueError, IndexError):
            pass
    return result


def run(args, mode, output, label, usr):
    trial = output/label
    trial.mkdir()
    env = os.environ.copy()
    for key in list(env):
        if key.startswith(('VK_', 'VN_', 'MANGOHUD', 'LEGION_', 'PROTON_')) or key in ('LD_PRELOAD', 'LD_LIBRARY_PATH'):
            env.pop(key, None)
    env.update(VK_DRIVER_FILES='/usr/share/vulkan/icd.d/nvidia_icd.json',
               VK_LOADER_LAYERS_DISABLE='~implicit~', LEGION_STARTUP_UI='log')
    if mode in ('venus', 'vtest') and args.renderer_prefix:
        env['LD_LIBRARY_PATH'] = str(args.renderer_prefix.resolve()/'lib')
    if mode in ('venus', 'vtest') and args.sync_fd:
        env['LEGION_VENUS_SYNC_FD'] = '1'
    server = None
    temporary = tempfile.TemporaryDirectory(prefix='legion-venus-bench-')
    cmd = command(args, mode, output, trial, usr)
    write(trial/'command.json', cmd)
    groups = set()
    try:
        with (trial/'server.log').open('w') as log:
            if mode == 'vtest':
                sock = str(Path(temporary.name)/'vtest.sock')
                program = str(args.renderer_prefix.resolve()/'bin/virgl_test_server') if args.renderer_prefix else 'virgl_test_server'
                server = subprocess.Popen([program, '--no-virgl', '--venus', '--socket-path', sock],
                    env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                groups.add(server.pid)
                deadline = time.monotonic()+5
                while not Path(sock).exists():
                    if server.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError('vtest server failed; inspect '+str(trial))
                    time.sleep(.02)
                env.update(VK_DRIVER_FILES=str(output/'vtest-icd.json'), VN_DEBUG='vtest', VTEST_SOCKET_NAME=sock)
            baseline = cpu_snapshot(groups)
            ticks = dict(baseline)
            start = time.monotonic()
            with (trial/'launch.log').open('w') as launch_log:
                process = subprocess.Popen(cmd, env=env, stdout=launch_log,
                    stderr=subprocess.STDOUT, start_new_session=True)
                groups.add(process.pid)
                try:
                    while process.poll() is None:
                        for key, value in cpu_snapshot(groups).items():
                            ticks[key] = max(ticks.get(key, 0), value)
                        if time.monotonic()-start > 120:
                            raise TimeoutError('benchmark exceeded 120 seconds')
                        time.sleep(.1)
                    if process.returncode:
                        raise RuntimeError('benchmark failed; inspect '+str(trial/'launch.log'))
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
            wall = time.monotonic()-start
            path = trial/'workload-result.json' if mode in ('native', 'vtest') else trial/'vm/frames/workload-result.json'
            result = json.loads(path.read_text())
            if result.get('passed') is not True or result.get('gpu_verified') is not True:
                raise RuntimeError('correctness validation failed: '+str(path))
            for case in result['cases']:
                if case['gpu']['vendor_id'] != 0x10de or case['gpu']['device_id'] != args.device_id:
                    raise RuntimeError('wrong GPU; refusing misleading comparison')
                if not case['gpu_known_answer_verified']:
                    raise RuntimeError('image check failed')
            record = dict(mode=mode, label=label, wall_seconds=wall,
                sampled_process_group_cpu_seconds=sum(value-baseline.get(key, 0) for key, value in ticks.items())/os.sysconf('SC_CLK_TCK'),
                cpu_scope='sampled process groups at 10 Hz, including startup; detached renderer processes may be omitted; not total system CPU',
                result_path=str(path), cases=result['cases'])
            write(trial/'measurement.json', record)
            print(label, {c['case']['name']: round(c['frames_per_second'], 1) for c in result['cases']}, flush=True)
            return record
    finally:
        if server is not None:
            try:
                os.killpg(server.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            server.wait(timeout=5)
        temporary.cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference-session', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--runtime', type=Path, default=ROOT/'build/runtime')
    parser.add_argument('--launcher', type=Path, default=ROOT/'build/target/x86_64-unknown-linux-musl/release/legion')
    parser.add_argument('--workload', type=Path, default=ROOT/'build/target/release/legion-workload-probe')
    parser.add_argument('--init', type=Path, default=ROOT/'build/target/x86_64-unknown-linux-musl/release/legion-guest-probe')
    parser.add_argument('--venus-driver', type=Path, default=ROOT/'build/venus/stage/usr/lib/x86_64-linux-gnu/libvulkan_virtio.so')
    parser.add_argument('--render-node', default='/dev/dri/renderD128')
    parser.add_argument('--renderer-prefix', type=Path)
    parser.add_argument('--sync-fd', action='store_true', help='enable the isolated renderer fence-wait experiment')
    parser.add_argument('--device-id', type=lambda value: int(value, 0))
    parser.add_argument('--repetitions', type=int, default=5)
    parser.add_argument('--width', type=int, default=1920)
    parser.add_argument('--height', type=int, default=1080)
    parser.add_argument('--iterations', nargs='+', type=int, default=[1, 64, 512])
    parser.add_argument('--modes', nargs='+', choices=['native', 'vtest', 'venus', 'nvidia'], default=['native', 'vtest', 'venus'])
    args = parser.parse_args()
    if args.device_id is None:
        args.device_id = int((Path('/sys/class/drm')/Path(args.render_node).name/'device/device').read_text().strip(), 0)
    if args.sync_fd and (not args.renderer_prefix or b'LEGION_VENUS_SYNC_FD' not in (args.renderer_prefix/'libexec/virgl_render_server').read_bytes()):
        parser.error('--sync-fd requires a renderer prefix built with the experimental patch')
    if not 1 <= args.repetitions <= 10:
        parser.error('--repetitions must be 1..10')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    if not 64 <= args.width <= 3840 or not 64 <= args.height <= 2160 or not all(1 <= n <= 4096 for n in args.iterations):
        parser.error('dimensions must be 64..3840 x 64..2160; iterations must be 1..4096')
    config = dict(schema=1, width=args.width, height=args.height, warmup_ms=1000, sample_ms=3000,
        cases=[dict(name=name, mode='gpu', kernel='integer', workers=0, working_set_kib=512,
                    work_units=1, gpu_iterations=iterations) for name, iterations in [(f'iterations-{n}', n) for n in args.iterations]])
    write(output/'config.json', config)
    write(output/'environment.json', dict(kernel=os.uname().release, workload_sha256=sha(args.workload),
        venus_driver_sha256=sha(args.venus_driver), configuration=config,
        gpu=subprocess.check_output(['nvidia-smi', '--query-gpu=name,driver_version,clocks.gr,clocks.mem,temperature.gpu,utilization.gpu', '--format=csv'], text=True),
        renderer_prefix=str(args.renderer_prefix), sync_fd=args.sync_fd,
        renderer_sha256=sha(args.renderer_prefix/'lib/libvirglrenderer.so') if args.renderer_prefix else sha(Path('/usr/lib/libvirglrenderer.so.1')),
        scope='offscreen Linux Vulkan; no presentation, DXVK, VKD3D or game FPS claim'))
    usr = prepare(args, output)
    records = []
    for repetition in range(args.repetitions):
        modes = list(args.modes)
        random.Random(37+repetition).shuffle(modes)
        for mode in modes:
            records.append(run(args, mode, output, f'{repetition+1:02d}-{mode}', usr))
            write(output/'measurements.json', records)
    summary = {}
    for mode in args.modes:
        summary[mode] = {}
        for name in (case['name'] for case in config['cases']):
            cases = [case for record in records if record['mode']==mode for case in record['cases'] if case['case']['name']==name]
            hashes = {case['gpu_known_answer_hash'] for case in cases}
            if len(hashes) != 1:
                raise RuntimeError('inconsistent image validation hashes')
            summary[mode][name] = dict(median_fps=statistics.median(c['frames_per_second'] for c in cases),
                minimum_fps=min(c['frames_per_second'] for c in cases), maximum_fps=max(c['frames_per_second'] for c in cases),
                median_p95_ms=statistics.median(c['frame_intervals']['p95_ms'] for c in cases),
                median_gpu_p50_ms=statistics.median(c['gpu_execution']['p50_ms'] for c in cases))
    for case in config['cases']:
        hashes = {c['gpu_known_answer_hash'] for r in records for c in r['cases'] if c['case']['name']==case['name']}
        if len(hashes) != 1:
            raise RuntimeError('image hashes differ between graphics backends')
    write(output/'summary.json', summary)
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
