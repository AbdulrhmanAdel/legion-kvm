mod archive;
mod audio;
mod cpu;
mod doctor;
mod game;
mod graphics;
mod memory;
mod packed;
mod prefix;
mod probe;
mod process;
mod qemu_policy;
mod request;
mod runtime;
mod session;
mod startup;
mod venus;
mod wine_recipe;

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

const HELP: &str = "Legion KVM launcher

Usage:
  legion <launcher command and arguments...>
  legion run --proton /path/to/Proton --prefix /path/to/prefix -- /path/to/application.exe
  legion auto [--] <launcher command and arguments...>
  legion doctor
  legion doctor --graphics
  legion inspect-launch -- <launcher command and arguments...>
  legion cpu-check
  legion readme
  legion --version

Steam launch options: /path/to/legion %command%

The launcher detects the host CPU topology and graphics driver.
AVX-512 is not a host requirement. The guest uses reduced XSTATE.
Runtime assets include a guest kernel and are loaded from the packaged runtime.
Run as your desktop user with access to /dev/kvm and the graphics device.
Installed Proton runs unchanged by default; optional Wine repairs are version-checked.

LEGION_RENDER_NODE selects a DRM render node on systems with multiple GPUs.
LEGION=1 enables the VM when using the optional 'legion auto' wrapper.
LEGION_MEMORY_MIB sets guest memory in MiB; the default is 12288.
Games use the guest Wayland compositor through Venus.
LEGION_STARTUP_UI=auto|window|log|off controls startup feedback.
LEGION_STARTUP_TIMEOUT_SECONDS sets the startup deadline (default 180; 30..900).
LEGION_WINDOW_SYSTEM=auto|wayland|x11 selects the host window system.
LEGION_WINE_REPAIR=1 opts into an included, version-specific Wine repair.
Logs are saved under XDG_STATE_HOME/legion, normally ~/.local/state/legion.";

fn main() {
    let mut argv = std::env::args_os();
    let program = argv.next().unwrap_or_default();
    let args: Vec<_> = argv.collect();
    let tool = std::path::Path::new(&program)
        .file_name()
        .and_then(|s| s.to_str());
    let result = match tool {
        Some(kind @ ("legion-qemu" | "legion-render-server" | "legion-virtiofsd")) => {
            runtime::exec_tool(kind, &args)
        }
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
    if args.first().is_some_and(|arg| arg == "auto") {
        let tail = &args[1..];
        let tail = tail.strip_prefix(&[OsString::from("--")]).unwrap_or(tail);
        anyhow::ensure!(!tail.is_empty(), "auto wrapper needs a launcher command");
        let enabled = match std::env::var("LEGION") {
            Ok(value) if value == "1" => true,
            Ok(value) if value == "0" || value.is_empty() => false,
            Err(std::env::VarError::NotPresent) => false,
            _ => bail!("LEGION must be 1 (enabled) or 0 (original launcher command)"),
        };
        if !enabled {
            // exec preserves the runner's arguments, environment, cwd, signals
            // and exit status, without extracting a runtime or initializing KVM.
            return Err(std::process::Command::new(&tail[0]).args(&tail[1..]).exec())
                .context("execute original launcher command");
        }
        let forwarded: Vec<_> = std::iter::once(OsString::from("run"))
            .chain(tail.iter().cloned())
            .collect();
        packed::enter(&forwarded)?;
        return game::run(tail);
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
        Some("venus-probe" | "drm-native-probe") => venus::run(tail)?,
        Some("run") => game::run(tail)?,
        Some("inspect-launch") => game::inspect_launch(tail)?,
        Some("--") => game::run(tail)?,
        Some("cpu-check") => game::cpu_check(tail)?,
        Some("doctor") if tail == [OsString::from("--graphics")] => game::graphics_check()?,
        Some("doctor") if tail.is_empty() && game::has_runtime() => runtime::doctor(tail)?,
        Some("runtime-doctor") => runtime::doctor(tail)?,
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
