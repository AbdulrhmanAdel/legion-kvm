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
mod tests {
    use super::*;

    fn v(a: u32, b: u32, c: u32) -> DriverVersion {
        DriverVersion::new(a, b, c)
    }

    #[test]
    fn tables_are_sorted_and_unique() {
        for p in PROFILES {
            let t = (p.cmd)();
            assert!(!t.is_empty(), "{}", p.version);
            assert!(
                t.windows(2).all(|w| w[0].num < w[1].num),
                "{} is not sorted by command number",
                p.version
            );
        }
    }

    /// Every descriptor and every handle has to lie inside the struct it is an offset into.
    #[test]
    fn a_uvm_command_carries_at_most_one_descriptor() {
        for p in PROFILES {
            for c in (p.cmd)() {
                assert!(
                    c.fds.len() <= 1,
                    "{}: {:#x} carries {} descriptors; the guest is told of one",
                    p.version,
                    c.num,
                    c.fds.len()
                );
            }
        }
    }

    #[test]
    fn every_descriptor_fits_its_parameter_block() {
        for p in PROFILES {
            for c in (p.cmd)() {
                for f in c.fds {
                    assert!(
                        f.at + 4 <= c.params_size as usize,
                        "{} command {:#x}: descriptor at {} does not fit {} bytes",
                        p.version,
                        c.num,
                        f.at,
                        c.params_size
                    );
                    if let Some(h) = f.handle {
                        assert!(
                            h + 4 <= c.params_size as usize,
                            "{} command {:#x}: handle at {h} does not fit {} bytes",
                            p.version,
                            c.num,
                            c.params_size
                        );
                        assert_ne!(h, f.at, "the handle and the descriptor cannot be one field");
                    }
                }
            }
        }
    }

    /// The flags the backend sends have to be flags the release defines.
    #[test]
    fn the_flags_we_send_are_flags_the_release_has() {
        for p in PROFILES {
            let f = (p.init)();
            assert_eq!(
                f.ours() & !f.mask,
                0,
                "{} sends a bit outside its mask",
                p.version
            );
            assert!(
                f.ours() & f.multi_process_sharing_mode != 0,
                "{} does not get sharing mode, and the backend holds one UVM file for every guest process",
                p.version
            );
        }
    }

    /// The releases genuinely differ here, and the whole point of reading the flags per release rather than writing them down is that 535.129.03 has no bit for pageable access.
    #[test]
    fn the_older_releases_cannot_ask_for_pageable_access_to_be_off() {
        let old = select(v(535, 129, 3)).unwrap();
        assert!(!old.init.can_disable_pageable_access());
        assert!(old.pageable_must_be_off());
        assert_eq!(old.init.mask, 0x3);

        let new = select(v(615, 71, 9)).unwrap();
        assert!(new.init.can_disable_pageable_access());
        assert!(!new.pageable_must_be_off());
        assert_eq!(new.init.mask, 0x7);
        assert!(new.init_flags() & new.init.disable_pageable_access.unwrap() != 0);
    }

    #[test]
    fn an_exact_table_is_the_release_s_own() {
        for want in supported_versions() {
            let s = select(want).expect("a release with a table selects it");
            assert!(s.exact, "{want}");
        }
    }

    #[test]
    fn an_unknown_release_falls_back_and_says_so() {
        assert!(select(v(500, 0, 0)).is_none(), "older than every table");
        let s = select(v(615, 80, 0)).expect("newer than every table falls back");
        assert!(!s.exact);
    }

    /// On a fallback a command the releases describe differently is refused.
    #[test]
    fn a_fallback_needs_every_release_to_agree() {
        let differs = v615_71_09::CMD
            .iter()
            .find(|c| PROFILES.iter().any(|p| find((p.cmd)(), c.num) != Some(c)))
            .expect("the releases differ somewhere");
        assert!(select(v(615, 71, 9)).unwrap().entry(differs.num).is_some());
        assert!(select(v(615, 80, 0)).unwrap().entry(differs.num).is_none());
    }

    /// ...and one they agree on survives, or the rule above refuses everything.
    #[test]
    fn a_fallback_still_permits_what_every_release_agrees_on() {
        let same = v615_71_09::CMD
            .iter()
            .find(|c| PROFILES.iter().all(|p| find((p.cmd)(), c.num) == Some(*c)))
            .expect("the releases agree somewhere");
        assert!(select(v(615, 80, 0)).unwrap().entry(same.num).is_some());
    }

    /// The descriptor-carrying commands, named.
    #[test]
    fn the_commands_that_carry_a_descriptor() {
        let s = select(v(615, 71, 9)).unwrap();
        let carrying: Vec<_> = s.cmd.iter().filter(|c| !c.fds.is_empty()).collect();
        assert_eq!(carrying.len(), 8, "615.71.09 carries eight");

        // UVM_REGISTER_GPU_VASPACE: a control descriptor and the client handle it indexes, four bytes apart, after the 16-byte UUID.
        let e = s.entry(25).expect("UVM_REGISTER_GPU_VASPACE");
        assert_eq!(e.fds, &[FdSlot::new(Fd::Ctl, 16, Some(20))]);

        // UVM_IMPORT_DMA_BUF: a descriptor for something this backend never opened, and so no handle beside it.
        let e = s.entry(83).expect("UVM_IMPORT_DMA_BUF");
        assert_eq!(e.fds, &[FdSlot::new(Fd::Foreign, 16, None)]);

        // UVM_MM_INITIALIZE takes a UVM descriptor, not a control one.
        let e = s.entry(75).expect("UVM_MM_INITIALIZE");
        assert_eq!(e.fds, &[FdSlot::new(Fd::Uvm, 0, None)]);
    }
}
