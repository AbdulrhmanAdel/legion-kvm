// Per-version ABI tables.

pub mod v535_129_03;
pub mod v580_178_04;
pub mod v595_71_05;

use crate::version::DriverVersion;

/// How the device must handle one escape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoctlKind {
    /// Copy params in, call the host ioctl, copy params out.
    Simple,
    /// Carries an embedded file descriptor that must be translated from a guest handle to a host fd before the host ioctl is issued.
    FdCarrying,
    /// Establishes a host mapping.
    Mapping,
    /// `NV_ESC_RM_CONTROL`: nested dispatch on the embedded control command.
    RmControl,
    /// NVOS41 carries one fixed-size output event record.
    RmEventData,
    /// `NV_ESC_RM_ALLOC`: nested dispatch on the embedded allocation class.
    RmAlloc,
    /// `NV_ESC_RM_VID_HEAP_CONTROL`: a union whose active arm depends on the requested function.
    VidHeapControl,
    /// `NV_ESC_RM_IDLE_CHANNELS`: carries three guest pointer arrays.
    IdleChannels,
    /// A variable-length byte array with no fixed parameter struct, sized by the ioctl number at call time.
    Bytes,
}

/// One entry in a per-version ioctl table.
pub struct IoctlEntry {
    /// The `NV_ESC_*` escape, i.e. bits 7:0 of the Linux ioctl number.
    pub escape: u32,
    /// `sizeof` the parameter struct for this driver version, or `None` when the ioctl is variable length.
    pub param_size: Option<u32>,
    pub kind: IoctlKind,
}

/// A supported driver version and the table it selects.
struct Profile {
    version: DriverVersion,
    table: fn() -> &'static [IoctlEntry],
}

/// Profiles in ascending version order.
static PROFILES: &[Profile] = &[
    Profile {
        version: DriverVersion::new(535, 129, 3),
        table: v535_129_03::table,
    },
    Profile {
        version: DriverVersion::new(580, 178, 4),
        table: v580_178_04::table,
    },
    Profile {
        version: DriverVersion::new(595, 71, 5),
        table: v595_71_05::table,
    },
];

/// Select the profile for `v`.
pub fn table_for(v: DriverVersion) -> Option<&'static [IoctlEntry]> {
    PROFILES
        .iter()
        .rev()
        .find(|p| p.version <= v)
        .map(|p| (p.table)())
}

/// Whether a driver version resolves to a table at all.
pub fn is_supported(v: DriverVersion) -> bool {
    table_for(v).is_some()
}

/// Look up one escape in a table.
pub fn lookup(table: &'static [IoctlEntry], escape: u32) -> Option<&'static IoctlEntry> {
    table.iter().find(|e| e.escape == escape)
}

/// The versions with a profile of their own, ascending.
pub fn supported_versions() -> impl Iterator<Item = DriverVersion> {
    PROFILES.iter().map(|p| p.version)
}

#[cfg(test)]
#[path = "../../tests/unit/versions/mod.rs"]
mod tests;
