//! The UVM commands a guest may send, and where their descriptors sit.

pub mod v535_129_03;
pub mod v580_178_04;
pub mod v595_104_02;
pub mod v595_71_05;
pub mod v615_71_09;
pub mod v615_78_08;

use crate::version::DriverVersion;

/// What a descriptor inside a UVM parameter block refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fd {
    /// `/dev/nvidiactl`, the RM control device.
    Ctl,
    /// `/dev/nvidia-uvm` itself.
    Uvm,
    /// Something this backend did not open -- a dma-buf.
    Foreign,
}

/// One descriptor inside a parameter block, and the handle that goes with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdSlot {
    pub kind: Fd,
    /// Byte offset of the `NvS32` descriptor.
    pub at: usize,
    /// Byte offset of the `NvHandle` the descriptor indexes, when there is one.
    pub handle: Option<usize>,
}

impl FdSlot {
    pub const fn new(kind: Fd, at: usize, handle: Option<usize>) -> Self {
        Self { kind, at, handle }
    }
}

/// One UVM command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UvmCmd {
    pub num: u32,
    /// `sizeof` the parameter struct.
    pub params_size: u32,
    pub fds: &'static [FdSlot],
}

impl UvmCmd {
    pub const fn new(num: u32, params_size: u32, fds: &'static [FdSlot]) -> Self {
        Self {
            num,
            params_size,
            fds,
        }
    }
}

/// The `UVM_INIT_FLAGS_*` one release defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitFlags {
    pub disable_hmm: Option<u64>,
    pub multi_process_sharing_mode: u64,
    pub disable_pageable_access: Option<u64>,
    pub disable_pageable_migrations: Option<u64>,
    pub mask: u64,
}

impl InitFlags {
    /// What the backend sends, whatever the guest asked for.
    pub const fn ours(&self) -> u64 {
        let mut f = self.multi_process_sharing_mode;
        if let Some(b) = self.disable_hmm {
            f |= b;
        }
        if let Some(b) = self.disable_pageable_access {
            f |= b;
        }
        f
    }

    /// Whether this release can be told to refuse pageable access outright.
    pub const fn can_disable_pageable_access(&self) -> bool {
        self.disable_pageable_access.is_some()
    }
}

struct Profile {
    version: DriverVersion,
    /// A function rather than the slice, for the reason given in [`crate::rmctrl`]: a static read in another static's initializer is const-evaluated and the table can end up copied per use.
    cmd: fn() -> &'static [UvmCmd],
    init: fn() -> InitFlags,
}

/// Profiles in ascending version order.
static PROFILES: &[Profile] = &[
    Profile {
        version: DriverVersion::new(535, 129, 3),
        cmd: || v535_129_03::CMD,
        init: || v535_129_03::INIT,
    },
    Profile {
        version: DriverVersion::new(580, 178, 4),
        cmd: || v580_178_04::CMD,
        init: || v580_178_04::INIT,
    },
    Profile {
        version: DriverVersion::new(595, 71, 5),
        cmd: || v595_71_05::CMD,
        init: || v595_71_05::INIT,
    },
    Profile {
        version: DriverVersion::new(595, 104, 2),
        cmd: || v595_104_02::CMD,
        init: || v595_104_02::INIT,
    },
    Profile {
        version: DriverVersion::new(615, 71, 9),
        cmd: || v615_71_09::CMD,
        init: || v615_71_09::INIT,
    },
    Profile {
        version: DriverVersion::new(615, 78, 8),
        cmd: || v615_78_08::CMD,
        init: || v615_78_08::INIT,
    },
];

/// The UVM table for one host release.
#[derive(Clone, Copy)]
pub struct Selected {
    pub cmd: &'static [UvmCmd],
    pub init: InitFlags,
    /// The host release has a table of its own.
    pub exact: bool,
}

/// The table for a host driver version: its own, or the nearest older one.
pub fn select(v: DriverVersion) -> Option<Selected> {
    PROFILES
        .iter()
        .rev()
        .find(|p| p.version <= v)
        .map(|p| Selected {
            cmd: (p.cmd)(),
            init: (p.init)(),
            exact: p.version == v,
        })
}

fn find(table: &'static [UvmCmd], num: u32) -> Option<&'static UvmCmd> {
    table
        .binary_search_by_key(&num, |c| c.num)
        .ok()
        .map(|i| &table[i])
}

impl Selected {
    /// The entry for this command, if every table that has to agree does.
    pub fn entry(&self, num: u32) -> Option<&'static UvmCmd> {
        let e = find(self.cmd, num)?;
        if self.exact {
            return Some(e);
        }
        PROFILES
            .iter()
            .all(|p| find((p.cmd)(), num).is_some_and(|o| o == e))
            .then_some(e)
    }

    /// Whether the backend must prove pageable access is off rather than ask for it.
    pub const fn pageable_must_be_off(&self) -> bool {
        !self.init.can_disable_pageable_access()
    }

    /// The flags word the backend sends for `UVM_INITIALIZE`.
    pub const fn init_flags(&self) -> u64 {
        self.init.ours()
    }
}

/// The versions with tables of their own, ascending.
pub fn supported_versions() -> impl Iterator<Item = DriverVersion> {
    PROFILES.iter().map(|p| p.version)
}

#[cfg(test)]
#[path = "../../tests/unit/uvm/mod.rs"]
mod tests;
