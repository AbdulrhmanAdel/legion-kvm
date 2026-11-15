//! Explicit driver selection for disposable guest graphics probes.
use anyhow::{Result, bail, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuKind {
    Venus,
}

impl GpuKind {
    pub fn from_cmdline(cmdline: &str) -> Result<Self> {
        let mut selected = None;
        for value in cmdline
            .split_whitespace()
            .filter_map(|word| word.strip_prefix("legion.gpu="))
        {
            ensure!(selected.is_none(), "duplicate guest GPU selection");
            selected = Some(match value {
                "venus" => Self::Venus,
                _ => bail!("unsupported guest GPU probe driver: {value}"),
            });
        }
        Ok(selected.unwrap_or(Self::Venus))
    }

    pub fn icd(self) -> &'static str {
        match self {
            Self::Venus => "/usr/share/vulkan/icd.d/virtio_icd.json",
        }
    }

    pub fn vendor_id(self) -> &'static str {
        match self {
            Self::Venus => "hardware via Venus",
        }
    }
}

/// Reject a loader's fallback GPU before starting any workload or application.
pub fn verify_host_gpu(cmdline: &str, summary: &str) -> Result<()> {
    let parameter = |name: &str| -> Result<Option<u16>> {
        let mut values = cmdline
            .split_whitespace()
            .filter_map(|word| word.strip_prefix(name));
        let value = values
            .next()
            .map(|value| u16::from_str_radix(value, 16))
            .transpose()?;
        ensure!(values.next().is_none(), "duplicate host GPU identity");
        Ok(value)
    };
    let vendor = parameter("legion.gpu_vendor=")?;
    let device = parameter("legion.gpu_device=")?;
    // Standalone developer probes may omit the host identity; normal sessions supply both.
    if vendor.is_none() && device.is_none() {
        return Ok(());
    }
    let (Some(vendor), Some(device)) = (vendor, device) else {
        bail!("incomplete host GPU identity");
    };
    let ids = |name: &str| -> Result<Vec<u16>> {
        summary
            .lines()
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == name).then_some(value.trim())
            })
            .map(|value| {
                u16::from_str_radix(value.trim_start_matches("0x"), 16).map_err(Into::into)
            })
            .collect()
    };
    let vendors = ids("vendorID")?;
    let devices = ids("deviceID")?;
    ensure!(
        vendors == [vendor] && devices == [device],
        "selected GPU {vendor:04x}:{device:04x} is not the only guest Vulkan device (vendors {vendors:x?}, devices {devices:x?}); enable its host Vulkan driver and check VK_DRIVER_FILES"
    );
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/gpu_kind.rs"]
mod tests;
