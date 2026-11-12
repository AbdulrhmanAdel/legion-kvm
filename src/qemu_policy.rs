//! Apply process-wide memory policy in an exec stage owned only by QEMU.
use std::{ffi::OsString, os::unix::process::CommandExt, path::Path, process::Command};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HugePages {
    #[default]
    Off,
    System,
}

impl HugePages {
    pub fn parse(value: &std::ffi::OsStr) -> Result<Self> {
        match value.to_str() {
            Some("off") => Ok(Self::Off),
            Some("system") => Ok(Self::System),
            _ => bail!("QEMU hugepages must be off or system"),
        }
    }
}

pub fn command(program: &Path, policy: HugePages) -> Result<Command> {
    if policy == HugePages::System {
        return Ok(Command::new(program));
    }
    let mut command = Command::new(std::env::current_exe().context("locate Legion exec stage")?);
    command.arg("internal-qemu-exec").arg("--").arg(program);
    Ok(command)
}

/// Called in a fresh, single-threaded Legion child immediately before exec.
pub fn exec(args: &[OsString]) -> Result<()> {
    let Some((separator, argv)) = args.split_first() else {
        bail!("expected internal-qemu-exec -- <QEMU command>");
    };
    ensure!(separator == "--", "expected exec argument separator");
    let Some((program, arguments)) = argv.split_first() else {
        bail!("QEMU exec command is missing");
    };
    rustix::thread::disable_transparent_huge_pages(true)
        .context("disable transparent hugepages for the QEMU child")?;
    ensure!(
        rustix::thread::transparent_huge_pages_are_disabled()
            .context("verify QEMU child memory policy")?,
        "QEMU child did not retain its hugepage policy"
    );
    eprintln!("LEGION_QEMU_POLICY hugepages=off scope=qemu-process");
    let error = Command::new(program).args(arguments).exec();
    Err(error).with_context(|| format!("exec QEMU command {program:?}"))
}
