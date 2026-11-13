//! Source-derived qualification model.
#![no_std]

pub mod kuser;
mod profile;
pub use profile::{ControlRequest, control_request};

pub const REGISTER_PROCESS: u32 = 0x1337;
pub const REGISTER_HANDLER: u32 = 0x336933;
pub const POLL_READY: u32 = 0x1338;
pub const BYPASS_MAGIC: u64 = 0x6969_6969_6969_6969;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vendor {
    Intel,
    Amd,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Registers {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}
impl Registers {
    pub const fn new(eax: u32, ebx: u32, ecx: u32, edx: u32) -> Self {
        Self { eax, ebx, ecx, edx }
    }
    pub const fn from_wide(value: [u64; 4]) -> Self {
        Self::new(
            value[0] as u32,
            value[1] as u32,
            value[2] as u32,
            value[3] as u32,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Privilege {
    Kernel,
    Ring1,
    Ring2,
    User,
}
impl Privilege {
    pub fn from_ring(ring: u8) -> Option<Self> {
        match ring {
            0 => Some(Self::Kernel),
            1 => Some(Self::Ring1),
            2 => Some(Self::Ring2),
            3 => Some(Self::User),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Context {
    /// Opaque process identity; zero models PsGetCurrentProcess returning null.
    pub process: u64,
    pub privilege: Privilege,
    pub stack_privilege: Privilege,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct State {
    pub owner: u64,
    /// The RDX payload is independent of the actual tracked process identity.
    pub declared_process_id: u64,
    pub handler: u64,

    pub ready: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Route {
    Native,
    Profile,
    Register,
    Handler,
    PollPending,
    PollConsumed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub registers: [u64; 4],
    pub route: Route,
    pub exit_vm: bool,
}

impl State {
    /// Execute the CPUID state transition only.
    pub fn cpuid(
        &mut self,
        vendor: Vendor,
        context: Context,
        input: [u64; 4],
        native: Registers,
    ) -> Outcome {
        let leaf = input[0] as u32;
        let subleaf = input[2] as u32;
        let user = match vendor {
            Vendor::Amd => context.privilege == Privilege::User,
            Vendor::Intel => context.privilege != Privilege::Kernel,
        };
        if user {
            match control_request(vendor, input[0]) {
                ControlRequest::Register => {
                    if self.owner == 0 && context.process != 0 {
                        self.owner = context.process;
                        self.declared_process_id = input[3];
                    }
                    return native_outcome(vendor, leaf, subleaf, native, Route::Register);
                }
                request if self.owner != 0 && self.owner == context.process => {
                    match request {
                        ControlRequest::Ready if self.ready => {
                            self.ready = false;
                            let mut registers = input;
                            registers[0] = 1;
                            return Outcome {
                                registers,
                                route: Route::PollConsumed,
                                exit_vm: false,
                            };
                        }
                        ControlRequest::Ready => {
                            return native_outcome(
                                vendor,
                                leaf,
                                subleaf,
                                native,
                                Route::PollPending,
                            );
                        }
                        ControlRequest::Handler => {
                            if self.handler == 0 {
                                self.handler = input[2];
                            }
                            return native_outcome(vendor, leaf, subleaf, native, Route::Handler);
                        }
                        _ => {}
                    }
                    if let Some(reply) = profile::reply(vendor, leaf, subleaf, native) {
                        return Outcome {
                            registers: profile::guest_registers(
                                vendor, leaf, subleaf, false, true, reply,
                            ),
                            route: Route::Profile,
                            exit_vm: false,
                        };
                    }
                }
                _ => {}
            }
        }
        let mut reply = native;
        let mut exit_vm = false;
        if !user {
            match vendor {
                Vendor::Amd => match leaf {
                    1 => reply.ecx |= 1 << 31,
                    0x4000_0000 => {
                        reply = Registers::new(0x4000_0001, 0x706d_6953, 0x7653_656c, 0x2020_206d)
                    }
                    0x4000_0001 => reply = Registers::new(0x3023_7648, 0, 0, 0),
                    0x4141_4141
                        if subleaf == 0x4141_4141
                            && context.stack_privilege == Privilege::Kernel =>
                    {
                        exit_vm = true
                    }
                    _ => {}
                },
                Vendor::Intel => match input[0] {
                    1 => reply.ecx |= 1 << 31,
                    0x4000_0000 => reply = Registers::new(0x4000_0001, 0x6570_7948, 0x6762_4472, 0),
                    0x4000_0001 => reply = Registers::new(0x3023_7648, 0, 0, 0),
                    _ => {}
                },
            }
        }
        let mut outcome = native_outcome(vendor, leaf, subleaf, reply, Route::Native);
        outcome.exit_vm = exit_vm;
        outcome
    }
}

fn native_outcome(
    vendor: Vendor,
    leaf: u32,
    subleaf: u32,
    native: Registers,
    route: Route,
) -> Outcome {
    Outcome {
        registers: profile::guest_registers(vendor, leaf, subleaf, true, true, native),
        route,
        exit_vm: false,
    }
}

/// Query the target's ordinary user-mode table without mutating protocol state.
pub fn profile_reply(
    vendor: Vendor,
    leaf: u32,
    subleaf: u32,
    native: Registers,
) -> Option<[u64; 4]> {
    profile::reply(vendor, leaf, subleaf, native)
        .map(|reply| profile::guest_registers(vendor, leaf, subleaf, false, true, reply))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyscallRoute {
    Original,
    Handler,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyscallOutcome {
    pub route: SyscallRoute,
    pub rax: u64,
    pub rcx: u64,
    pub r11: u64,
    pub xmm4: [u64; 2],
    pub xmm5: [u64; 2],
}

/// Model the shared AMD/Intel ASM register contract, without executing LSTAR, SWAPGS or SYSRET.
pub fn syscall(
    state: State,
    process: u64,
    rax: u64,
    rcx: u64,
    r11: u64,
    xmm5: [u64; 2],
) -> SyscallOutcome {
    let tracked = state.owner != 0 && state.owner == process && state.handler != 0;
    let bypass = tracked && xmm5[0] == BYPASS_MAGIC;
    let redirect = tracked && !bypass;
    SyscallOutcome {
        route: if redirect {
            SyscallRoute::Handler
        } else {
            SyscallRoute::Original
        },
        rax: if redirect { rcx } else { u64::from(rax as u32) },
        rcx: if redirect { state.handler } else { rcx },
        r11,
        xmm4: [u64::from(rax as u32), 0],
        xmm5: if bypass { [0, 0] } else { xmm5 },
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
