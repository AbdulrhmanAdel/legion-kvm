use std::io::Write;

use anyhow::{Context, Result, ensure};

/// Build a minimal uncompressed newc initramfs without host cpio or root access.
pub fn write_initramfs(mut output: impl Write, init: &[u8]) -> Result<()> {
    write_with_modules(&mut output, init, &[])
}

pub fn write_with_modules(mut output: impl Write, init: &[u8], modules: &[Vec<u8>]) -> Result<()> {
    write_with_payload(&mut output, init, modules, None)
}

pub fn write_with_payload(
    mut output: impl Write,
    init: &[u8],
    modules: &[Vec<u8>],
    draw: Option<&[u8]>,
) -> Result<()> {
    write_with_display(&mut output, init, modules, draw, None)
}

pub fn write_with_display(
    mut output: impl Write,
    init: &[u8],
    modules: &[Vec<u8>],
    draw: Option<&[u8]>,
    display: Option<&[u8]>,
) -> Result<()> {
    write_with_workload(&mut output, init, modules, draw, display, None, None)
}

pub fn write_with_workload(
    mut output: impl Write,
    init: &[u8],
    modules: &[Vec<u8>],
    draw: Option<&[u8]>,
    display: Option<&[u8]>,
    workload: Option<&[u8]>,
    config: Option<&[u8]>,
) -> Result<()> {
    ensure!(
        workload.is_some() == config.is_some(),
        "workload and configuration must be paired"
    );
    entry(&mut output, "dev", 0o040755, 1, &[], 0, 0)?;
    // initramfs does not automatically mount devtmpfs.
    entry(&mut output, "dev/console", 0o020600, 2, &[], 5, 1)?;
    entry(&mut output, "init", 0o100755, 3, init, 0, 0)?;
    let mut inode = 4;
    if !modules.is_empty() {
        entry(&mut output, "modules", 0o040755, inode, &[], 0, 0)?;
        inode += 1;
        let mut order = String::new();
        for (index, bytes) in modules.iter().enumerate() {
            let name = format!("{index:03}.ko");
            order.push_str(&name);
            order.push('\n');
            entry(
                &mut output,
                &format!("modules/{name}"),
                0o100600,
                inode,
                bytes,
                0,
                0,
            )?;
            inode = inode.checked_add(1).context("cpio inode overflow")?;
        }
        entry(
            &mut output,
            "modules/load-order",
            0o100600,
            inode,
            order.as_bytes(),
            0,
            0,
        )?;
        inode = inode.checked_add(1).context("cpio inode overflow")?;
    }
    if let Some(draw) = draw {
        entry(&mut output, "probe-draw", 0o100755, inode, draw, 0, 0)?;
        inode = inode.checked_add(1).context("cpio inode overflow")?;
    }
    if let Some(display) = display {
        entry(&mut output, "probe-display", 0o100755, inode, display, 0, 0)?;
        inode = inode.checked_add(1).context("cpio inode overflow")?;
    }
    if let (Some(workload), Some(config)) = (workload, config) {
        entry(
            &mut output,
            "probe-workload",
            0o100755,
            inode,
            workload,
            0,
            0,
        )?;
        inode = inode.checked_add(1).context("cpio inode overflow")?;
        entry(
            &mut output,
            "workload-config.json",
            0o100600,
            inode,
            config,
            0,
            0,
        )?;
        inode = inode.checked_add(1).context("cpio inode overflow")?;
    }
    entry(&mut output, "TRAILER!!!", 0, inode, &[], 0, 0)?;
    output.flush().context("flush initramfs")
}

fn entry(
    output: &mut impl Write,
    name: &str,
    mode: u32,
    inode: u32,
    data: &[u8],
    rdev_major: u32,
    rdev_minor: u32,
) -> Result<()> {
    let name_size = u32::try_from(name.len().checked_add(1).context("cpio name overflow")?)?;
    let size = u32::try_from(data.len()).context("cpio member exceeds 4 GiB")?;
    let header = format!(
        "070701{inode:08x}{mode:08x}{:08x}{:08x}{:08x}{:08x}{size:08x}{:08x}{:08x}{rdev_major:08x}{rdev_minor:08x}{name_size:08x}{:08x}",
        0, 0, 1, 0, 0, 0, 0
    );
    ensure!(header.len() == 110, "invalid cpio header length");
    output.write_all(header.as_bytes())?;
    output.write_all(name.as_bytes())?;
    output.write_all(&[0])?;
    pad(output, (110 + u64::from(name_size)) % 4)?;
    output.write_all(data)?;
    pad(output, u64::from(size) % 4)
}

fn pad(output: &mut impl Write, remainder: u64) -> Result<()> {
    if remainder != 0 {
        output.write_all(&[0; 3][..usize::try_from(4 - remainder)?])?;
    }
    Ok(())
}
