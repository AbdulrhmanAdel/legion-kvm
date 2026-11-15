#!/usr/bin/env python3
"""Run an external application benchmark inside the selected runtime."""
import json
import ctypes
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import time
import sys

FRAMES = Path('/frames')
FIXTURE = Path('/usr/share/legion')


def validate(config):
    if config.get('schema') != 1 or not (0 if config.get('full_run') else 1) <= config.get('appid', -1) <= 2**32-1:
        raise ValueError('invalid external benchmark identity')
    path = PurePosixPath(config['executable'])
    if path.is_absolute() or '..' in path.parts or not str(path).endswith('.exe'):
        raise ValueError('benchmark executable must be a relative executable path')
    args = config.get('arguments', [])
    if (not isinstance(args, list) or len(args) > 16
            or any(not isinstance(a, str) or len(a) > 512 or '\0' in a for a in args)):
        raise ValueError('invalid bounded benchmark arguments')
    if not 15 <= config.get('startup_seconds', 0) <= (240 if config.get('full_run') else 45):
        raise ValueError('invalid bounded external application duration')
    if config.get('vkd3d_config', '') not in ('', 'single_queue', 'no_upload_hvv'):
        raise ValueError('unsupported controlled graphics diagnostic')
    if config.get('dxvk_binding_model', 'default') not in ('default', 'descriptor_sets'):
        raise ValueError('unsupported DXVK binding diagnostic')
    if config.get('forward_wait', 'spin') not in ('spin', 'events', 'hybrid'):
        raise ValueError('unsupported forwarding wait diagnostic')
    retry = config.get('forward_idle_retry_us', 1000)
    if type(retry) is not int or not 100 <= retry <= 1000:
        raise ValueError('idle retry must be 100..1000 microseconds')
    if config.get('forward_pacing', 'callbacks') not in ('callbacks', 'unpaced'):
        raise ValueError('unsupported forwarding pacing diagnostic')
    if not isinstance(config.get('forward_fullscreen', False), bool):
        raise ValueError('fullscreen diagnostic must be boolean')
    if not isinstance(config.get('application_share', False), bool):
        raise ValueError('application share must be boolean')
    if config.get('application_share') and not config.get('full_run'):
        raise ValueError('application share requires a full benchmark run')
    if not isinstance(config.get('diagnostic_resources', False), bool):
        raise ValueError('resource diagnostic must be boolean')
    if not isinstance(config.get('diagnostic_queue_profile', False), bool):
        raise ValueError('queue timeline diagnostic must be boolean')
    if not isinstance(config.get('diagnostic_cpu_samples', False), bool):
        raise ValueError('CPU sampling diagnostic must be boolean')
    if not isinstance(config.get('diagnostic_scheduler_trace', False), bool):
        raise ValueError('scheduler diagnostic must be boolean')
    delay = config.get('diagnostic_scheduler_delay_seconds', 0)
    if type(delay) is not int or not 0 <= delay <= 30:
        raise ValueError('scheduler delay must be 0..30 seconds')
    if delay and not config.get('diagnostic_scheduler_trace'):
        raise ValueError('scheduler delay requires scheduler tracing')
    if config.get('diagnostic_cpu_samples') and config.get('diagnostic_scheduler_trace'):
        raise ValueError('select one CPU or scheduler profiler')
    return config


def save(name, data):
    (FRAMES / name).write_text(json.dumps(data, indent=2)+'\n')


def require_success(status, timed_out, device_loss_abort):
    if status or timed_out or device_loss_abort:
        raise RuntimeError(f'external application failed: status={status}, timeout={timed_out}, device_loss_abort={device_loss_abort}')


def needs_xwayland(proton):
    """Inspect the selected tool rather than assuming a Proton family/version."""
    for library in ('lib', 'lib64'):
        directory = proton/'files'/library/'wine/x86_64-unix'
        if directory.is_dir() and any(p.name.startswith('winewayland') for p in directory.iterdir()):
            return False
    return True


def xwayland_display(ready):
    display = ready.get('display')
    if not isinstance(display, str) or not display.startswith(':') or not display[1:].isascii() or not display[1:].isdigit() or len(display) > 12:
        raise ValueError('invalid guest XWayland display')
    return display


def main():
    native = '--native' in sys.argv
    if not native and (os.getuid() != 0 or Path('/proc/1/exe').resolve() != Path('/init')
            or 'legion.vulkan=1' not in Path('/proc/cmdline').read_text().split()):
        raise RuntimeError('external bootstrap requires the disposable Legion guest')
    launch = json.loads((FIXTURE/'launch.json').read_text())
    config = validate(json.loads((FIXTURE/'external-benchmark.json').read_text()))
    if native and config.get('diagnostic_scheduler_trace'):
        raise ValueError('scheduler trace requires the disposable Linux guest')
    uid, gid = launch['uid'], launch['gid']
    if uid == 0 or gid == 0 or not Path(launch['proton_path']).is_absolute():
        raise ValueError('invalid benchmark user/runtime')
    full_run = config.get('full_run', False)
    if native and not full_run:
        raise ValueError('native external launch requires full-run mode')
    if native and os.getuid() != uid:
        raise ValueError('native benchmark must use its unprivileged configured user')
    shared_application = config.get('application_share', False)
    if shared_application and native:
        raise ValueError('application share requires the disposable guest')
    install = (Path('/run/legion-shares/application') if shared_application else
               FRAMES/'benchmark' if full_run else Path('/usr/share/legion-benchmark'))
    deadline = time.monotonic()+30
    while full_run and not shared_application and not (FRAMES/'benchmark-ready').is_file():
        if time.monotonic() > deadline:
            raise RuntimeError('private application copy did not become ready')
        time.sleep(.05)
    exe = install/config['executable']
    if not exe.is_file() or not exe.resolve().is_relative_to(install):
        raise ValueError('benchmark executable missing or outside private installation')
    if not full_run:
        # Only this disposable guest's mount namespace is changed. The copied
        # application tree is read-only; Saved and prefix/cache writes go to /frames.
        saved = FRAMES/'benchmark-saved'
        saved.mkdir()
        shutil.copytree(install/'b1/Saved/Config', saved/'Config')
        for path in [saved, *saved.rglob('*')]:
            os.chown(path, uid, gid)
        # Linux mount(2): both paths are NUL-terminated local guest paths. The
        # guest identity check above runs before this privileged guest-only call.
        libc = ctypes.CDLL(None, use_errno=True)
        libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p,
                               ctypes.c_ulong, ctypes.c_void_p]
        libc.mount.restype = ctypes.c_int
        if libc.mount(os.fsencode(saved), os.fsencode(install/'b1/Saved'), None, 4096, None):
            raise OSError(ctypes.get_errno(), 'bind private benchmark Saved directory')
    client = Path('/usr/share/legion-steam-client')
    if (client/'linux64/steamclient.so').is_file():
        sdk = FRAMES/'home/.steam'
        sdk.mkdir(exist_ok=True)
        (sdk/'sdk64').symlink_to(client/'linux64')
        if not native:
            os.chown(sdk, uid, gid)
    runtime = f'/run/user/{uid}'
    use_xwayland = not native and needs_xwayland(Path(launch['proton_path']))
    if use_xwayland:
        if not Path('/usr/bin/Xwayland').is_file():
            raise RuntimeError('selected Proton requires the guest XWayland executable')
        Path('/tmp/.X11-unix').mkdir(exist_ok=True)
        os.chmod('/tmp/.X11-unix', 0o1777)
    env = dict(PATH='/usr/bin', HOME='/frames/home', PWD=str(install), XDG_RUNTIME_DIR=runtime,
               XDG_CACHE_HOME='/frames/cache', PYTHONHOME='/usr', PYTHONDONTWRITEBYTECODE='1',
               LD_LIBRARY_PATH='/usr/lib:/usr/lib/x86_64-linux-gnu:/usr/lib/i386-linux-gnu',
               LANG='C.UTF-8', WAYLAND_DISPLAY='legion-host-0' if native and not config.get('native_forward') else 'legion-proton-0', PROTON_ENABLE_WAYLAND='1',
               STEAM_COMPAT_DATA_PATH='/frames/compatdata',
               STEAM_COMPAT_CLIENT_INSTALL_PATH=str(client) if client.is_dir() else '/frames/steam',
               STEAM_COMPAT_INSTALL_PATH=str(install), SteamAppId=str(config['appid']), SteamGameId=str(config['appid']),
               WINEDLLOVERRIDES='dxgi,d3d11,d3d12,d3d12core=n', WINEDEBUG='-all',
               DXVK_LOG_LEVEL='info', VKD3D_DEBUG='warn', VKD3D_SHADER_CACHE_PATH='/frames/cache',
               __EGL_VENDOR_LIBRARY_DIRS='/usr/share/glvnd/egl_vendor.d',
               FONTCONFIG_FILE='/usr/etc/fonts/fonts.conf', FONTCONFIG_PATH='/usr/etc/fonts',
               VK_DRIVER_FILES='/usr/share/vulkan/icd.d/nvidia_icd.json')
    if config.get('vkd3d_config'):
        env['VKD3D_CONFIG'] = config['vkd3d_config']
    if config.get('dxvk_binding_model') == 'descriptor_sets':
        (FRAMES/'dxvk-benchmark.conf').write_text(
            'dxvk.enableDescriptorHeap = False\ndxvk.enableDescriptorBuffer = False\n')
        env['DXVK_CONFIG_FILE'] = 'Z:\\frames\\dxvk-benchmark.conf'
    def demote():
        os.setgroups([])
        os.setgid(gid)
        os.setuid(uid)
    save('proton-forward-config.json', dict(schema=1, external_client=True, width=1920, height=1080,
         seconds=config['startup_seconds'] if full_run else 30, benchmark_client=full_run, uncapped=True, acquire_mode='async', frame_transport='polled', frame_pacing=config.get('forward_pacing', 'callbacks'), forward_wait=config.get('forward_wait', 'spin'), forward_idle_retry_us=config.get('forward_idle_retry_us', 1000), forward_fullscreen=config.get('forward_fullscreen', False), explicit_sync=config.get('explicit_sync', True), xwayland=use_xwayland))
    server_log = (FRAMES/'wayland.log').open('w')
    direct = native and not config.get('native_forward')
    server_env = dict(env)
    if native:
        server_env['WAYLAND_DISPLAY'] = 'legion-host-0'
    server = None if direct else subprocess.Popen(
        [str(FIXTURE/'compositor'), '--proton-native-forward' if native else '--proton-forward'],
        env=server_env, stdout=server_log, stderr=server_log, preexec_fn=None if native else demote)
    phases = []
    profiler = None
    try:
        deadline = time.monotonic()+10
        while server is not None and not Path(runtime, 'legion-proton-0').exists():
            if server.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('benchmark Wayland server failed to become ready')
            time.sleep(.05)
        if use_xwayland:
            ready = FRAMES/'xwayland-ready.json'
            while not ready.is_file():
                if server.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('guest XWayland failed to become ready; inspect xwayland.log')
                time.sleep(.05)
            env['DISPLAY'] = xwayland_display(json.loads(ready.read_text()))
            env.pop('PROTON_ENABLE_WAYLAND', None)
        save('display-backend.json', dict(schema=1, backend='xwayland' if use_xwayland else 'wayland',
                                         display=env.get('DISPLAY')))
        for phase, verb in [('prefix_setup', 'getcompatpath'), ('benchmark', 'runinprefix')]:
            save('startup-status.json', dict(schema=1, stage='preparing-runtime' if phase == 'prefix_setup'
                                           else 'starting-application'))
            command = ['/usr/bin/python3', launch['proton_path']+'/proton', verb, str(exe)]
            if phase == 'benchmark':
                command.extend(config.get('arguments', []))
            save(f'{phase}-command.json', command)
            start = time.monotonic()
            resources = None
            if config.get('diagnostic_resources') and phase == 'benchmark':
                from benchmark_resource_probe import ResourceProbe
                resources = ResourceProbe(uid)
            if config.get('diagnostic_cpu_samples') and phase == 'benchmark':
                from benchmark_cpu_probe import CpuProbe
                profiler = CpuProbe(uid, FRAMES)
            if config.get('diagnostic_scheduler_trace') and phase == 'benchmark':
                from benchmark_scheduler_probe import SchedulerProbe
                profiler = SchedulerProbe(uid, FRAMES,
                    start_delay_seconds=config.get('diagnostic_scheduler_delay_seconds', 0))
            child_env = dict(env)
            if config.get('diagnostic_queue_profile') and phase == 'benchmark':
                from benchmark_queue_probe import environment
                child_env.update(environment())
            with (FRAMES/f'proton-{phase}.stdout').open('w') as stdout, (FRAMES/f'proton-{phase}.stderr').open('w') as stderr:
                child = subprocess.Popen(command, cwd=install, env=child_env, stdout=stdout, stderr=stderr, preexec_fn=None if native else demote)
                timed_out = False
                device_loss_abort = False
                deadline = time.monotonic()+config['startup_seconds']
                snapshot_at = time.monotonic()+10
                while child.poll() is None and time.monotonic() < deadline:
                    if resources is not None:
                        resources.sample()
                    if profiler is not None:
                        profiler.sample()
                    if config.get('diagnostic_stop_on_device_loss') and phase == 'benchmark':
                        error_path = FRAMES/f'proton-{phase}.stderr'
                        with error_path.open('rb') as errors:
                            errors.seek(max(0, error_path.stat().st_size-8192))
                            lost = b'VK_ERROR_DEVICE_LOST' in errors.read()
                        if lost:
                            device_loss_abort = True
                            # This server belongs to this private prefix in the
                            # disposable guest. Never signal a host Wine session.
                            subprocess.run([launch['proton_path']+'/files/bin/wineserver', '-k'],
                                env=dict(env, WINEPREFIX='/frames/compatdata/pfx'),
                                preexec_fn=None if native else demote, timeout=5, check=False)
                            break
                    if not full_run and time.monotonic() >= snapshot_at:
                        rows = []
                        for process in Path('/proc').glob('[0-9]*'):
                            try:
                                commandline = (process/'cmdline').read_bytes().replace(b'\0', b' ').decode(errors='replace')
                                if any(name in commandline.lower() for name in ['wine', '/proton ']):
                                    rows.append(dict(pid=int(process.name), commandline=commandline,
                                                     status=(process/'status').read_text(),
                                                     wait_channel=(process/'wchan').read_text()))
                            except (OSError, ProcessLookupError):
                                continue
                        save(f'{phase}-processes.json', rows)
                        snapshot_at = float('inf')
                    time.sleep(.1)
                if child.poll() is None:
                    timed_out = True
                    child.terminate()
                    try:
                        status = child.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        status = child.wait()
                else:
                    status = child.returncode
            phases.append(dict(phase=phase, elapsed_seconds=time.monotonic()-start,
                               status=status, timed_out=timed_out, device_loss_abort=device_loss_abort))
            if resources is not None:
                save('benchmark-resources.json', resources.finish())
            if config.get('diagnostic_queue_profile') and phase == 'benchmark':
                from benchmark_queue_probe import finish
                save('benchmark-queue-result.json', finish(FRAMES, status, timed_out))
            if profiler is not None:
                save('benchmark-scheduler-result.json' if config.get('diagnostic_scheduler_trace')
                     else 'benchmark-cpu-samples.json', profiler.finish())
            save('external-benchmark-phases.json', phases)
            require_success(status, timed_out, device_loss_abort)
        if full_run:
            for name in ['BenchmarkResults.txt', 'BenchmarkResultsPerFrame.csv']:
                if not (install/name).is_file() or (install/name).stat().st_size == 0:
                    raise RuntimeError(f'external application did not produce {name}')
        (FRAMES/'proton-client-exited').touch()
        if server is not None:
            server.wait(timeout=10)
        save('external-benchmark-launch.json', dict(schema=1, phases=phases, source_fps=None, full_run=full_run,
             performance_comparison_qualified=False, note='external application exit status; analyze application result files separately'))
        # The generic guest runner can consume this status, without claiming
        # the owned pixel/checksum proof or a completed performance benchmark.
        save('workload-result.json', dict(schema=1, passed=(server is None or server.returncode == 0),
             gpu_verified=False, workload='external_benchmark_startup', source_fps=None))
    finally:
        if profiler is not None:
            profiler.finish()
        if server is not None and server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
        server_log.close()


if __name__ == '__main__':
    main()
