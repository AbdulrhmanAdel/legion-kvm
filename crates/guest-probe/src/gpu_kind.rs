//! Explicit driver selection for disposable guest graphics probes.
use anyhow::{Result, bail, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuKind {
    Nvidia,
    Amd,
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
                "nvidia" => Self::Nvidia,
                "amdgpu" => Self::Amd,
                _ => bail!("unsupported guest GPU probe driver: {value}"),
            });
        }
        Ok(selected.unwrap_or(Self::Nvidia))
    }

    pub fn icd(self) -> &'static str {
        match self {
            Self::Nvidia => "/usr/share/vulkan/icd.d/nvidia_icd.json",
            Self::Amd => "/usr/share/vulkan/icd.d/radeon_icd.json",
        }
    }

    pub fn vendor_id(self) -> &'static str {
        match self {
            Self::Nvidia => "0x10de",
            Self::Amd => "0x1002",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn driver_selection_is_explicit_and_unambiguous() -> Result<()> {
        assert_eq!(
            GpuKind::from_cmdline("console=ttyS0 legion.vulkan=1")?,
            GpuKind::Nvidia
        );
        let amd = GpuKind::from_cmdline("console=ttyS0 legion.gpu=amdgpu")?;
        assert_eq!(amd.vendor_id(), "0x1002");
        assert!(amd.icd().ends_with("radeon_icd.json"));
        assert!(GpuKind::from_cmdline("legion.gpu=unknown").is_err());
        assert!(GpuKind::from_cmdline("legion.gpu=nvidia legion.gpu=amdgpu").is_err());
        Ok(())
    }
}
