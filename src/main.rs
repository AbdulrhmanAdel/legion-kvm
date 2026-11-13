mod archive;
mod audio;
mod cpu;
mod doctor;
mod driver_libraries;
mod game;
mod gpu_transport;
mod graphics;
mod graphics_benchmark;
mod memory;
mod native_bridge;
mod native_context;
mod packed;
mod prefix;
mod presentation;
mod probe;
mod process;
mod qemu_policy;
mod request;
mod runtime;
mod session;
mod startup;
mod wine_recipe;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

const HELP: &str = "Legion KVM launcher

Usage:
  legion <launcher command and arguments...>
  legion run --proton /path/to/Proton --prefix /path/to/prefix -- /path/to/application.exe
  legion doctor
  legion cpu-check
  legion readme
  legion --version

Steam launch options: /path/to/legion %command%

The launcher detects the host CPU topology and graphics driver.
AVX-512 is not a host requirement. The guest uses reduced XSTATE.
Runtime assets include a guest kernel and are loaded from the packaged runtime.
Run as your desktop user with access to /dev/kvm and the graphics device.
Selected Proton modules must match the runtime compatibility recipe.

LEGION_RENDER_NODE selects a DRM render node on systems with multiple GPUs.
LEGION_MEMORY_MIB sets guest memory in MiB; the default is 12288.
LEGION_DISPLAY_BACKEND=auto|wayland|xwayland selects Wine's display backend on NVIDIA.
LEGION_STARTUP_UI=auto|window|log|off controls startup feedback.
Logs are saved under XDG_STATE_HOME/legion, normally ~/.local/state/legion.";

fn main() {
    let mut argv = std::env::args_os();
    let program = argv.next().unwrap_or_default();
    let args: Vec<_> = argv.collect();
    let tool = std::path::Path::new(&program)
        .file_name()
        .and_then(|s| s.to_str());
    let result = match tool {
        Some(
            kind @ ("legion-qemu" | "legion-amdgpu-qemu" | "legion-backend" | "legion-virtiofsd"),
        ) => runtime::exec_tool(kind, &args),
        _ => run(args),
    };
    if let Err(error) = result {
        eprintln!("legion: {error:#}");
        std::process::exit(1);
    }
}

fn run(args: Vec<OsString>) -> Result<()> {
    if args.len() == 1 && args[0] == "--version" {
        println!(
            "legion {} ({})",
            env!("CARGO_PKG_VERSION"),
            option_env!("LEGION_BUILD_ID").unwrap_or("development")
        );
        return Ok(());
    }
    if args.is_empty() || (args.len() == 1 && matches!(args[0].to_str(), Some("--help" | "-h"))) {
        println!("{HELP}");
        return Ok(());
    }
    packed::enter(&args)?;
    let Some((command, tail)) = args.split_first() else {
        println!("{HELP}");
        return Ok(());
    };
    match command.to_str() {
        Some("readme") if tail.is_empty() => {
            let runtime = runtime::Runtime::load(None)?;
            print!(
                "{}",
                std::fs::read_to_string(runtime.asset(std::path::Path::new("README.txt"))?)?
            );
        }
        Some("graphics-benchmark") => graphics_benchmark::run(tail)?,
        Some("drm-native-probe") => native_context::run(tail)?,
        Some("run") => game::run(tail)?,
        Some("--") => game::run(tail)?,
        Some("cpu-check") => game::cpu_check(tail)?,
        Some("doctor") if tail.is_empty() && game::has_runtime() => runtime::doctor(tail)?,
        Some("runtime-doctor") => runtime::doctor(tail)?,
        Some("benchmark") => runtime::benchmark(tail)?,
        Some("internal-qemu-exec") => qemu_policy::exec(tail)?,
        Some("--help" | "-h") => println!("{HELP}"),
        Some("doctor") if tail.is_empty() => print_json(&doctor::inspect()?)?,
        Some("plan") => {
            let Some((separator, argv)) = tail.split_first() else {
                bail!("expected plan -- <launcher command>");
            };
            anyhow::ensure!(separator == "--", "expected plan -- <launcher command>");
            print_json(&request::LaunchRequest::capture(argv)?)?;
        }
        Some("gpu-native-forward-probe") => {
            let mut config = native_bridge::ForwardConfig {
                compositor: PathBuf::new(),
                userspace: PathBuf::new(),
                output: PathBuf::new(),
                width: 1920,
                height: 1080,
                seconds: 15,
                acquire_barrier: false,
            };
            let (pairs, remainder) = tail.as_chunks::<2>();
            for pair in pairs {
                match pair[0].to_str() {
                    Some("--compositor") => config.compositor = (&pair[1]).into(),
                    Some("--acquire-barrier") => {
                        config.acquire_barrier = pair[1]
                            .to_str()
                            .context("invalid acquire barrier")?
                            .parse()?
                    }
                    Some("--userspace") => config.userspace = (&pair[1]).into(),
                    Some("--output") => config.output = (&pair[1]).into(),
                    Some("--seconds") => {
                        config.seconds = pair[1].to_str().context("invalid duration")?.parse()?
                    }
                    Some("--width") => {
                        config.width = pair[1].to_str().context("invalid width")?.parse()?
                    }
                    Some("--height") => {
                        config.height = pair[1].to_str().context("invalid height")?.parse()?
                    }
                    _ => bail!("unknown native forwarding option: {:?}", pair[0]),
                }
            }
            anyhow::ensure!(remainder.is_empty(), "each forwarding option needs a value");
            print_json(&native_bridge::run_forward(&config)?)?;
        }
        Some("cpu-probe") => {
            let mut config = probe::Config {
                qemu: PathBuf::from("qemu-system-x86_64"),
                kernel: PathBuf::new(),
                init: PathBuf::new(),
                output: PathBuf::new(),
                machine: probe::Machine::Q35,
                timeout: Duration::from_secs(30),
            };
            let (pairs, remainder) = tail.as_chunks::<2>();
            for pair in pairs {
                let option = pair[0].to_str().context("option name must be UTF-8")?;
                let value = &pair[1];
                match option {
                    "--kernel" => config.kernel = value.into(),
                    "--init" => config.init = value.into(),
                    "--output" => config.output = value.into(),
                    "--qemu" => config.qemu = value.into(),
                    "--machine" => {
                        config.machine = match value.to_str() {
                            Some("q35") => probe::Machine::Q35,
                            Some("microvm") => probe::Machine::Microvm,
                            _ => bail!("machine must be q35 or microvm"),
                        };
                    }
                    "--timeout" => {
                        let seconds: u64 = value.to_str().context("invalid timeout")?.parse()?;
                        anyhow::ensure!(
                            (1..=300).contains(&seconds),
                            "timeout must be 1..=300 seconds"
                        );
                        config.timeout = Duration::from_secs(seconds);
                    }
                    _ => bail!("unknown cpu-probe option: {option}"),
                }
            }
            anyhow::ensure!(remainder.is_empty(), "each cpu-probe option needs a value");
            print_json(&probe::run(&config)?)?;
        }
        Some(
            "gpu-transport-probe"
            | "gpu-driver-probe"
            | "gpu-vulkan-probe"
            | "gpu-workload-probe"
            | "gpu-present-probe"
            | "gpu-application-probe"
            | "gpu-native-bridge-probe",
        ) => {
            let native_probe = command == "gpu-native-bridge-probe";
            let driver_probe = command != "gpu-transport-probe" && !native_probe;
            let present_probe = matches!(
                command.to_str(),
                Some("gpu-present-probe" | "gpu-application-probe" | "gpu-native-bridge-probe")
            );
            let vulkan_probe = matches!(
                command.to_str(),
                Some("gpu-vulkan-probe" | "gpu-workload-probe")
            ) || present_probe;
            let mut boot = gpu_transport::DriverBoot {
                kernel: PathBuf::new(),
                init: PathBuf::new(),
                modules: Vec::new(),
                vulkan: None,
                ram_mode: memory::Mode::default(),
                qemu_hugepages: qemu_policy::HugePages::default(),
                memory_mib: None,
            };
            let mut vulkan = gpu_transport::VulkanBoot {
                compute: false,
                userspace: PathBuf::new(),
                virtiofsd: PathBuf::new(),
                draw: None,
                presentation: None,
                workload: None,
                forward_display: None,
                desktop_audio: false,
                shares: Vec::new(),
                driver_libraries: Default::default(),
            };
            let mut display = presentation::Config {
                guest: PathBuf::new(),
                presenter: PathBuf::new(),
                seconds: 15,
                width: 640,
                height: 360,
                self_test: true,
                application: command == "gpu-application-probe" || native_probe,
                uncapped: false,
                gpu_forward: false,
                acquire_mode: None,
                frame_transport: None,
                frame_pacing: None,
            };
            let mut backend = PathBuf::new();
            let mut output = PathBuf::new();
            let mut qemu = PathBuf::from("qemu-system-x86_64");
            let mut machine = probe::Machine::Q35;
            let mut native_cpus: Option<OsString> = None;
            let mut workload = PathBuf::new();
            let mut workload_config = PathBuf::new();
            let mut workload_timeout_seconds = None;
            let mut interactive = false;
            let (pairs, remainder) = tail.as_chunks::<2>();
            for pair in pairs {
                match pair[0].to_str() {
                    Some("--backend") => backend = (&pair[1]).into(),
                    Some("--output") => output = (&pair[1]).into(),
                    Some("--qemu") => qemu = (&pair[1]).into(),
                    Some("--kernel") if driver_probe => boot.kernel = (&pair[1]).into(),
                    Some("--init") if driver_probe => boot.init = (&pair[1]).into(),
                    Some("--module") if driver_probe => boot.modules.push((&pair[1]).into()),
                    Some("--userspace") if vulkan_probe => vulkan.userspace = (&pair[1]).into(),
                    Some("--shares-json") if vulkan_probe && !native_probe => {
                        let path = PathBuf::from(&pair[1]);
                        anyhow::ensure!(
                            std::fs::metadata(&path)?.len()
                                <= legion_shared_directories::MAX_MANIFEST_BYTES,
                            "directory-share specification exceeds 64 KiB"
                        );
                        let shares: legion_shared_directories::Sources =
                            serde_json::from_slice(&std::fs::read(&path)?)?;
                        vulkan.shares = shares.resolve()?.directories;
                    }
                    Some("--virtiofsd") if vulkan_probe => vulkan.virtiofsd = (&pair[1]).into(),
                    Some("--gpu-compute") if driver_probe && vulkan_probe => {
                        vulkan.compute = pair[1]
                            .to_str()
                            .context("invalid GPU compute flag")?
                            .parse()?;
                    }
                    Some("--draw") if vulkan_probe => vulkan.draw = Some((&pair[1]).into()),
                    Some("--workload") if command == "gpu-workload-probe" => {
                        workload = (&pair[1]).into()
                    }
                    Some("--workload-config") if command == "gpu-workload-probe" => {
                        workload_config = (&pair[1]).into()
                    }
                    Some("--workload-timeout-seconds") if command == "gpu-workload-probe" => {
                        let seconds = pair[1]
                            .to_str()
                            .context("invalid workload timeout")?
                            .parse::<u64>()?;
                        anyhow::ensure!(
                            (60..=3600).contains(&seconds),
                            "workload timeout must be 60..3600 seconds"
                        );
                        workload_timeout_seconds = Some(seconds);
                    }
                    Some("--interactive") if command == "gpu-workload-probe" => {
                        interactive = pair[1]
                            .to_str()
                            .context("invalid interactive flag")?
                            .parse()?;
                    }
                    Some("--forward-display") if command == "gpu-workload-probe" => {
                        let value = pair[1].to_str().context("invalid forwarding size")?;
                        let (width, height) = value
                            .split_once('x')
                            .context("forwarding size must be WIDTHxHEIGHT")?;
                        let (width, height) = (width.parse::<u32>()?, height.parse::<u32>()?);
                        anyhow::ensure!(
                            (64..=3840).contains(&width) && (64..=2160).contains(&height),
                            "invalid forwarding size"
                        );
                        vulkan.forward_display = Some((width, height));
                    }
                    Some("--audio") if vulkan_probe => {
                        vulkan.desktop_audio = match pair[1].to_str() {
                            Some("off") => false,
                            Some("desktop") => true,
                            _ => bail!("--audio must be off or desktop"),
                        };
                    }
                    Some("--ram-mode") if driver_probe => {
                        boot.ram_mode = match pair[1].to_str() {
                            Some("aligned") => memory::Mode::Aligned,
                            Some("legacy") => memory::Mode::Legacy,
                            _ => bail!("RAM mode must be aligned or legacy"),
                        };
                    }
                    Some("--qemu-hugepages") if driver_probe => {
                        boot.qemu_hugepages = qemu_policy::HugePages::parse(&pair[1])?;
                    }
                    Some("--driver-libraries-json") if vulkan_probe => {
                        vulkan.driver_libraries =
                            serde_json::from_slice(&std::fs::read(&pair[1])?)?;
                    }
                    Some("--memory-mib") if driver_probe => {
                        let size = pair[1].to_str().context("invalid guest memory")?.parse()?;
                        anyhow::ensure!(
                            (512..=65536).contains(&size),
                            "guest memory must be 512..65536 MiB"
                        );
                        boot.memory_mib = Some(size);
                    }
                    Some("--guest-display") if present_probe => display.guest = (&pair[1]).into(),
                    Some("--presenter") if present_probe => display.presenter = (&pair[1]).into(),
                    Some("--seconds") if present_probe => {
                        display.seconds = pair[1].to_str().context("invalid duration")?.parse()?
                    }
                    Some("--width") if present_probe => {
                        display.width = pair[1].to_str().context("invalid width")?.parse()?
                    }
                    Some("--height") if present_probe => {
                        display.height = pair[1].to_str().context("invalid height")?.parse()?
                    }
                    Some("--self-test") if present_probe => {
                        display.self_test = pair[1]
                            .to_str()
                            .context("invalid self-test flag")?
                            .parse()?
                    }
                    Some("--uncapped") if present_probe => {
                        display.uncapped =
                            pair[1].to_str().context("invalid uncapped flag")?.parse()?
                    }
                    Some("--native-cpus") if native_probe => native_cpus = Some(pair[1].clone()),
                    Some("--frame-transport") if command == "gpu-application-probe" => {
                        display.frame_transport = Some(match pair[1].to_str() {
                            Some("combined") => presentation::FrameTransport::Combined,
                            Some("separate") => presentation::FrameTransport::Separate,
                            Some("notified") => presentation::FrameTransport::Notified,
                            Some("polled") => presentation::FrameTransport::Polled,
                            _ => bail!(
                                "frame transport must be polled, notified, combined or separate"
                            ),
                        });
                    }
                    Some("--acquire-mode") if command == "gpu-application-probe" => {
                        display.acquire_mode = Some(match pair[1].to_str() {
                            Some("async") => presentation::AcquireMode::Async,
                            Some("blocking") => presentation::AcquireMode::Blocking,
                            _ => bail!("acquire mode must be async or blocking"),
                        });
                    }
                    Some("--frame-pacing") if command == "gpu-application-probe" => {
                        display.frame_pacing = Some(match pair[1].to_str() {
                            Some("callbacks") => presentation::FramePacing::Callbacks,
                            Some("unpaced") => presentation::FramePacing::Unpaced,
                            _ => bail!("frame pacing must be callbacks or unpaced"),
                        });
                    }
                    Some("--gpu-forward") if command == "gpu-application-probe" => {
                        display.gpu_forward = pair[1]
                            .to_str()
                            .context("invalid forwarding flag")?
                            .parse()?;
                    }
                    Some("--machine") => {
                        machine = match pair[1].to_str() {
                            Some("q35") => probe::Machine::Q35,
                            Some("microvm") => probe::Machine::Microvm,
                            _ => bail!("machine must be q35 or microvm"),
                        };
                    }
                    _ => bail!("unknown GPU probe option: {:?}", pair[0]),
                }
            }
            anyhow::ensure!(
                remainder.is_empty(),
                "each gpu-transport-probe option needs a value"
            );
            if vulkan_probe {
                if command == "gpu-workload-probe" {
                    anyhow::ensure!(
                        !interactive
                            || (vulkan.forward_display.is_some()
                                && workload_timeout_seconds.is_none()),
                        "--interactive true requires --forward-display and excludes --workload-timeout-seconds"
                    );
                    anyhow::ensure!(
                        !workload.as_os_str().is_empty() && !workload_config.as_os_str().is_empty(),
                        "--workload and --workload-config are required"
                    );
                    let config: legion_workload_probe::Config =
                        serde_json::from_slice(&std::fs::read(&workload_config)?)?;
                    config.validate()?;
                    vulkan.workload = Some(gpu_transport::WorkloadBoot {
                        program: workload,
                        config,
                        timeout_seconds: workload_timeout_seconds,
                        interactive,
                    });
                }
                anyhow::ensure!(
                    !vulkan.userspace.as_os_str().is_empty(),
                    "--userspace is required"
                );
                anyhow::ensure!(
                    native_probe || !vulkan.virtiofsd.as_os_str().is_empty(),
                    "--virtiofsd is required"
                );
                if present_probe {
                    display.validate()?;
                    vulkan.presentation = Some(display);
                }
                if native_probe {
                    print_json(&native_bridge::run(
                        vulkan
                            .presentation
                            .as_ref()
                            .context("missing native presentation config")?,
                        &vulkan.userspace,
                        &output,
                        native_cpus.as_deref(),
                    )?)?;
                    return Ok(());
                }
                boot.vulkan = Some(vulkan);
            }
            print_json(&gpu_transport::run(
                &backend,
                &qemu,
                &output,
                machine,
                driver_probe.then_some(&boot),
            )?)?;
        }
        _ if game::has_runtime() => game::run(&args)?,
        _ => bail!("game launch requires the packaged Legion executable"),
    }
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout().lock(), value)?;
    println!();
    Ok(())
}
