//! Presented AMD cube session using the same runtime assets as game launches.
use anyhow::{Context, Result, bail, ensure};
use std::{ffi::OsString, fs, path::PathBuf};

pub(crate) fn run(args: &[OsString]) -> Result<()> {
    let mut runtime = None;
    let mut output = None;
    let mut seconds = 15_u64;
    let (pairs, remainder) = args.as_chunks::<2>();
    for pair in pairs {
        match pair[0].to_str() {
            Some("--runtime") => runtime = Some(PathBuf::from(&pair[1])),
            Some("--output") => output = Some(PathBuf::from(&pair[1])),
            Some("--seconds") => {
                seconds = pair[1].to_str().context("duration encoding")?.parse()?
            }
            _ => bail!("unknown graphics-benchmark option"),
        }
    }
    ensure!(remainder.is_empty(), "missing option value");
    ensure!(
        (5..=30).contains(&seconds),
        "duration must be 5..30 seconds"
    );
    let selection = crate::graphics::detect()?;
    let node = crate::graphics::require_amd(&selection)?;
    let runtime = crate::runtime::Runtime::load(runtime.as_deref())?;
    let amd = runtime
        .manifest
        .amdgpu
        .as_ref()
        .context("runtime has no AMD graphics assets")?;
    let output = output.context("--output is required")?;
    fs::create_dir(&output)?;
    let output = output.canonicalize()?;
    let settings = output.join("presentation.json");
    fs::write(
        &settings,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema":1,"client":"cube","width":1920,"height":1080,"seconds":seconds,"fullscreen":true
        }))?,
    )?;
    let usr = runtime.asset(&amd.userspace)?;
    let mut args = vec![
        OsString::from("--qemu"),
        runtime.tool("legion-amdgpu-qemu").into(),
        "--kernel".into(),
        runtime.asset(&amd.kernel)?.into(),
        "--init".into(),
        runtime.asset(&amd.init)?.into(),
        "--userspace".into(),
        usr.clone().into(),
        "--virtiofsd".into(),
        runtime.tool("legion-virtiofsd").into(),
        "--output".into(),
        output.join("vm").into(),
        "--render-node".into(),
        node.as_os_str().to_owned(),
        "--presentation".into(),
        usr.join("share/legion/legion-guest-compositor").into(),
        "--presentation-config".into(),
        settings.into(),
        "--window".into(),
        "true".into(),
        "--window-renderer".into(),
        "desktop".into(),
    ];
    for module in &amd.modules {
        args.extend(["--module".into(), runtime.asset(module)?.into()]);
    }
    crate::native_context::run(&args)
}
