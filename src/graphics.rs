//! Select graphics by the loaded DRM driver, with an optional explicit node.
use crate::doctor::{self, RenderDevice};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Graphics {
    pub node: PathBuf,
    pub driver: String,
}

pub(crate) fn select(devices: &[RenderDevice], requested: Option<&Path>) -> Result<Graphics> {
    let eligible = |device: &&RenderDevice| {
        device.read_write_access
            && matches!(
                device.driver.as_deref(),
                Some("nvidia" | "amdgpu" | "i915" | "xe")
            )
    };
    let device = if let Some(node) = requested {
        devices
            .iter()
            .filter(eligible)
            .find(|device| device.node == node)
            .context("selected render node is unavailable or unsupported")?
    } else {
        // Preserve the established NVIDIA default on mixed systems.
        devices
            .iter()
            .filter(eligible)
            .find(|device| device.driver.as_deref() == Some("nvidia"))
            .or_else(|| devices.iter().find(eligible))
            .context("no accessible hardware Vulkan render node")?
    };
    Ok(Graphics {
        node: device.node.clone(),
        driver: device.driver.clone().context("missing DRM driver")?,
    })
}

pub(crate) fn detect() -> Result<Graphics> {
    let node = std::env::var_os("LEGION_RENDER_NODE")
        .map(std::fs::canonicalize)
        .transpose()?;
    select(&doctor::render_devices()?, node.as_deref())
}

pub(crate) fn pci_id(node: &Path) -> Result<(u16, u16)> {
    let device = Path::new("/sys/class/drm")
        .join(node.file_name().context("render node name missing")?)
        .join("device");
    let read = |name: &str| -> Result<u16> {
        let value = std::fs::read_to_string(device.join(name))
            .with_context(|| format!("read selected GPU PCI {name}"))?;
        u16::from_str_radix(value.trim().trim_start_matches("0x"), 16)
            .with_context(|| format!("invalid selected GPU PCI {name}"))
    };
    Ok((read("vendor")?, read("device")?))
}

#[cfg(test)]
#[path = "../tests/unit/graphics.rs"]
mod tests;
