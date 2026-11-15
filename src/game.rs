//! Launch the selected installed Proton inside the bundled game runtime.
use crate::{packed, probe, runtime::Runtime};
use anyhow::{Context, Result, ensure};
use legion_shared_directories::Source;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) fn has_runtime() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.parent()
                .and_then(Path::parent)
                .map(|p| p.join("runtime.json"))
        })
        .is_some_and(|p| p.is_file())
}

fn directory(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "launch path must be absolute: {}",
        path.display()
    );
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    path.canonicalize().map_err(Into::into)
}

fn output() -> Result<PathBuf> {
    let root = packed::data_root("XDG_STATE_HOME", ".local/state")?.join("runs");
    fs::create_dir_all(&root)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = root.join(format!("{stamp}-{}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    fs::write(
        path.join("build.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version":env!("CARGO_PKG_VERSION"),
            "build":option_env!("LEGION_BUILD_ID").unwrap_or("development")
        }))?,
    )?;
    Ok(path)
}

/// Exercise the real guest graphics path without opening a game or Wine prefix.
pub(crate) fn graphics_check() -> Result<()> {
    let runtime = Runtime::load(None)?;
    let prerequisites = runtime.inspect();
    ensure!(
        prerequisites.passed,
        "graphics prerequisites failed: {}",
        serde_json::to_string_pretty(&prerequisites)?
    );
    let out = output()?;
    eprintln!("legion: Graphics test logs: {}", out.display());
    let args = graphics_args(
        &runtime,
        &runtime.asset(&runtime.manifest.userspace)?,
        &out.join("vm"),
    )?;
    crate::venus::run(&args)
}

/// All hosts use the same virtual GPU and guest Vulkan driver.
fn graphics_args(runtime: &Runtime, userspace: &Path, output: &Path) -> Result<Vec<OsString>> {
    let node = crate::graphics::detect()?.node;
    let m = &runtime.manifest;
    let mut args = vec![
        "--gpu-backend".into(),
        "venus".into(),
        "--qemu".into(),
        runtime.tool("legion-qemu").into(),
        "--kernel".into(),
        runtime.asset(&m.kernel)?.into(),
        "--init".into(),
        runtime.asset(&m.init)?.into(),
        "--userspace".into(),
        userspace.into(),
        "--virtiofsd".into(),
        runtime.tool("legion-virtiofsd").into(),
        "--output".into(),
        output.into(),
        "--render-node".into(),
        node.into(),
        "--memory-mib".into(),
        "2048".into(),
        "--window".into(),
        "true".into(),
        "--window-renderer".into(),
        "desktop".into(),
        "--window-system".into(),
        std::env::var_os("LEGION_WINDOW_SYSTEM").unwrap_or_else(|| "auto".into()),
    ];
    for module in &m.modules {
        args.extend(["--module".into(), runtime.asset(module)?.into()]);
    }
    Ok(args)
}

pub(crate) fn cpu_check(args: &[OsString]) -> Result<()> {
    ensure!(args.is_empty(), "cpu-check takes no arguments");
    let runtime = Runtime::load(None)?;
    let out = output()?;
    eprintln!("legion: CPU test results: {}", out.display());
    let result = probe::run(&probe::Config {
        qemu: runtime.tool("legion-qemu"),
        kernel: runtime.asset(&runtime.manifest.kernel)?,
        init: runtime.asset(&runtime.manifest.init)?,
        output: out.join("cpu"),
        machine: probe::Machine::Q35,
        timeout: std::time::Duration::from_secs(60),
    })?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    ensure!(result.passed, "CPU/KVM checks failed");
    Ok(())
}

struct Request {
    proton: PathBuf,
    executable: PathBuf,
    root: PathBuf,
    prefix: Option<PathBuf>,
    arguments: Vec<String>,
}

fn parse(args: &[OsString]) -> Result<Request> {
    let mut proton = None;
    let mut prefix = None;
    let mut explicit_root = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].to_str() {
            Some("--proton" | "--prefix" | "--game-root") => {
                let value = args
                    .get(index + 1)
                    .context("launch option requires a path")?;
                match args[index].to_str() {
                    Some("--proton") => proton = Some(PathBuf::from(value)),
                    Some("--prefix") => prefix = Some(PathBuf::from(value)),
                    _ => explicit_root = Some(PathBuf::from(value)),
                };
                index += 2;
            }
            Some("--") => {
                index += 1;
                break;
            }
            _ => break,
        }
    }
    let launch = &args[index..];
    let proton_at = launch.iter().position(|p| {
        Path::new(p).file_name().is_some_and(|n| n == "proton")
            && Path::new(p)
                .parent()
                .is_some_and(|root| root.join("files/bin/wine").is_file())
    });
    let exe_at = if let Some(at) = proton_at {
        proton = Some(
            PathBuf::from(&launch[at])
                .parent()
                .context("Proton parent missing")?
                .to_owned(),
        );
        ensure!(
            matches!(
                launch.get(at + 1).and_then(|v| v.to_str()),
                Some("run" | "waitforexitandrun" | "runinprefix")
            ),
            "unsupported Proton launch verb"
        );
        at + 2
    } else {
        if proton.is_none() {
            proton = std::env::var_os("PROTONPATH").map(PathBuf::from);
        }
        launch.iter().position(|p|Path::new(p).extension().is_some_and(|e|e.eq_ignore_ascii_case("exe"))).context("supply legion --proton /installed/Proton -- /path/game.exe, or use legion %command%")?
    };
    let proton = proton
        .context("selected Proton is missing; use --proton or your launcher's Proton command")?
        .canonicalize()?;
    ensure!(
        proton.join("proton").is_file(),
        "selected directory contains no Proton"
    );
    let executable =
        PathBuf::from(launch.get(exe_at).context("game executable is missing")?).canonicalize()?;
    ensure!(executable.is_file(), "game executable is not a file");
    let root = explicit_root
        .or_else(|| std::env::var_os("STEAM_COMPAT_INSTALL_PATH").map(PathBuf::from))
        .filter(|p| executable.starts_with(p))
        .or_else(|| {
            executable
                .ancestors()
                .skip(1)
                .find(|p| p.join("Engine").is_dir())
                .map(Path::to_owned)
        })
        .unwrap_or_else(|| {
            executable
                .parent()
                .expect("absolute file parent")
                .to_owned()
        })
        .canonicalize()?;
    ensure!(
        executable.starts_with(&root),
        "game executable is outside game root"
    );
    let arguments = launch[exe_at + 1..]
        .iter()
        .map(|v| {
            v.to_str()
                .context("game arguments must be UTF-8")
                .map(str::to_owned)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Request {
        proton,
        executable,
        root,
        prefix,
        arguments,
    })
}

fn copy(source: &Path, target: &Path) -> Result<()> {
    ensure!(
        std::process::Command::new("cp")
            .args(["-a", "--reflink=auto", "--"])
            .arg(source)
            .arg(target)
            .status()?
            .success(),
        "copy guest session runtime failed"
    );
    Ok(())
}

pub(crate) fn run(args: &[OsString]) -> Result<()> {
    let request = parse(args)?;
    let runtime = Runtime::load(None)?;
    if let Ok(backend) = std::env::var("LEGION_DISPLAY_BACKEND") {
        ensure!(
            matches!(backend.as_str(), "auto" | "wayland"),
            "Venus requires the guest Wayland display backend; unset LEGION_DISPLAY_BACKEND or use wayland"
        );
    }
    let report = runtime.inspect();
    ensure!(
        report.passed,
        "runtime prerequisites failed: {}",
        serde_json::to_string_pretty(&report)?
    );
    let memory_mib = std::env::var("LEGION_MEMORY_MIB")
        .unwrap_or_else(|_| "12288".into())
        .parse::<u64>()?;
    ensure!(
        (4096..=65536).contains(&memory_mib),
        "LEGION_MEMORY_MIB must be 4096..65536"
    );
    let available = fs::read_to_string("/proc/meminfo")?
        .lines()
        .find_map(|l| {
            l.strip_prefix("MemAvailable:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .context("available RAM is missing")?
        / 1024;
    ensure!(
        available >= memory_mib + 2048,
        "{available} MiB available; this launch needs {} MiB. Close heavy applications or set LEGION_MEMORY_MIB=8192",
        memory_mib + 2048
    );
    let out = output()?;
    eprintln!("legion: Session logs: {}", out.display());
    copy(
        &runtime.asset(&runtime.manifest.userspace)?,
        &out.join("usr"),
    )?;
    let fixture = out.join("usr/share/legion");
    let repair = crate::wine_recipe::enabled(std::env::var("LEGION_WINE_REPAIR").ok().as_deref())?;
    let overlay = if repair {
        Some(crate::wine_recipe::prepare(
            &fixture.join("wine-repair.json"),
            &request.proton,
            &fixture.join("compatibility/ntdll.dll"),
        ).context("optional Wine repair cannot be used with this Proton; unset LEGION_WINE_REPAIR to use stock Proton")?)
    } else {
        None
    };
    let home =
        PathBuf::from(std::env::var_os("HOME").context("HOME is missing")?).canonicalize()?;
    let key = format!(
        "{:x}",
        Sha256::digest(request.executable.as_os_str().as_encoded_bytes())
    );
    let state = packed::data_root("XDG_STATE_HOME", ".local/state")?
        .join("games")
        .join(key);
    fs::create_dir_all(&state)?;
    let mut shares = vec![
        Source {
            tag: "proton-tool".into(),
            directory: request.proton.clone(),
            readonly: true,
        },
        Source {
            tag: "application".into(),
            directory: request.root.clone(),
            readonly: false,
        },
        Source {
            tag: "proton-home".into(),
            directory: home.clone(),
            readonly: false,
        },
    ];
    let selected_prefix = request
        .prefix
        .or_else(|| std::env::var_os("WINEPREFIX").map(PathBuf::from));
    let (compatdata, wine_prefix) = if let Some(selected) = selected_prefix {
        let paths = crate::prefix::resolve(&selected, &state)?;
        shares.push(Source {
            tag: "wine-prefix".into(),
            directory: paths.prefix.clone(),
            readonly: false,
        });
        (paths.compatdata, Some(paths.prefix))
    } else {
        (
            directory(
                &std::env::var_os("STEAM_COMPAT_DATA_PATH")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| state.join("compatdata")),
            )?,
            None,
        )
    };
    shares.push(Source {
        tag: "proton-compatdata".into(),
        directory: compatdata.clone(),
        readonly: false,
    });
    shares.push(Source {
        tag: "proton-cache".into(),
        directory: directory(&state.join("cache"))?,
        readonly: false,
    });
    let _prefix_lock = rustix::fs::open(
        compatdata.join(".legion-session.lock"),
        rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )?;
    rustix::fs::flock(
        &_prefix_lock,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .context("this prefix already has a Legion session")?;
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    ensure!(uid != 0, "run Legion as your desktop user");
    fs::write(
        fixture.join("etc/passwd"),
        format!(
            "root:x:0:0:root:/root:/bin/sh\nlegion:x:{uid}:{gid}:legion:/frames/home:/bin/sh\n"
        ),
    )?;
    fs::write(
        fixture.join("etc/group"),
        format!("root:x:0:\nlegion:x:{gid}:\n"),
    )?;
    let mut launch: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.join("game-launch.json"))?)?;
    let mut overrides = std::env::var("WINEDLLOVERRIDES").unwrap_or_default();
    if repair {
        for (name, value) in [
            ("winmm", "n,b"),
            ("steam_api64", "n,b"),
            ("steamclient64", "n,b"),
            ("lsteamclient", "d"),
            ("dxgi", "n"),
            ("d3d12", "n"),
            ("d3d12core", "n"),
            ("d3d11", "n"),
            ("wineopenxr", "d"),
        ] {
            if !overrides
                .split(';')
                .filter_map(|v| v.split_once('='))
                .any(|(names, _)| {
                    names
                        .split(',')
                        .any(|n| n.trim().eq_ignore_ascii_case(name))
                })
            {
                if !overrides.is_empty() {
                    overrides.push(';');
                }
                overrides.push_str(&format!("{name}={value}"));
            }
        }
    }
    let mut environment: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| {
            k.starts_with("PROTON_")
                || k.starts_with("DXVK_")
                || k.starts_with("VKD3D_")
                || matches!(
                    k.as_str(),
                    "SteamAppId"
                        | "SteamGameId"
                        | "WINEDEBUG"
                        | "LANG"
                        | "LC_ALL"
                        | "LEGION_WORKLOAD_DIRECTORY"
                )
        })
        .collect();
    launch["proton_path"] = serde_json::json!(request.proton);
    launch["vulkan_driver"] = "venus".into();
    launch["nvidia_features"] = false.into();
    launch["display_backend"] = "wayland".into();
    launch["desktop_audio"] = true.into();
    environment.insert(
        "VN_PERF".into(),
        "no_timeline_sync,no_query_feedback".into(),
    );
    environment.insert("VN_FEEDBACK_SPIN_US".into(), "200".into());
    launch["uid"] = uid.into();
    launch["gid"] = gid.into();
    crate::wine_recipe::configure(&mut launch, overlay);
    launch["mangohud"] = std::env::var("MANGOHUD").is_ok_and(|v| v == "1").into();
    launch["environment"] = serde_json::to_value(environment)?;
    launch["host_paths"] = serde_json::json!({"home":home,"proton":request.proton,"application":request.root,"compatdata":compatdata,"wine_prefix":wine_prefix});
    launch["application"] = serde_json::json!({"executable":request.executable.strip_prefix(&request.root)?.to_str().context("non-UTF8 game path")?,"arguments":request.arguments,"dll_overrides":overrides});
    fs::write(
        fixture.join("launch.json"),
        serde_json::to_vec_pretty(&launch)?,
    )?;
    fs::write(out.join("launch.json"), serde_json::to_vec_pretty(&launch)?)?;
    let shares_path = out.join("shares.json");
    fs::write(
        &shares_path,
        serde_json::to_vec_pretty(&legion_shared_directories::Sources {
            schema: 1,
            directories: shares,
        })?,
    )?;
    let display_path = out.join("kms.json");
    fs::write(
        &display_path,
        serde_json::to_vec_pretty(
            &serde_json::json!({"schema":1,"client":"external","application":true,"width":1920,"height":1080,
        "fullscreen":std::env::var("LEGION_FULLSCREEN").is_ok_and(|v|v=="1")}),
        )?,
    )?;
    let mut args = graphics_args(&runtime, &out.join("usr"), &out.join("vm"))?;
    args.extend([
        "--shares".into(),
        shares_path.into(),
        "--workload".into(),
        fixture.join("bootstrap").into(),
        "--workload-config".into(),
        runtime.asset(&runtime.manifest.workload_config)?.into(),
        "--presentation-config".into(),
        display_path.into(),
        "--memory-mib".into(),
        memory_mib.to_string().into(),
        "--interactive".into(),
        "true".into(),
        "--application".into(),
        "true".into(),
        "--audio".into(),
        "desktop".into(),
    ]);
    crate::venus::run(&args)
}

#[cfg(test)]
#[path = "../tests/unit/game.rs"]
mod tests;
