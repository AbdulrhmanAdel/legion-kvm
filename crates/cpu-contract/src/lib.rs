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
mod tests {
    use super::*;
    const NATIVE: Registers = Registers::new(0x1234, 0x8000_0000, 0x9000_0000, u32::MAX);
    fn user(process: u64) -> Context {
        Context {
            process,
            privilege: Privilege::User,
            stack_privilege: Privilege::User,
        }
    }

    #[test]
    fn registration_is_first_process_owned_and_does_not_signal_readiness() {
        let mut state = State::default();
        state.cpuid(Vendor::Amd, user(17), [0x1337, 0, 0, 0], NATIVE);
        state.cpuid(Vendor::Amd, user(18), [0x1337, 0, 0, 99], NATIVE);
        assert_eq!(
            state,
            State {
                owner: 17,
                ..State::default()
            }
        );
        assert_eq!(
            state
                .cpuid(Vendor::Amd, user(17), [0x1338, 1, 2, 3], NATIVE)
                .route,
            Route::PollPending
        );
    }

    #[test]
    fn ready_is_owned_one_shot_and_preserves_untouched_wide_registers() {
        let mut state = State {
            owner: 17,
            ready: true,
            ..State::default()
        };
        let input = [0x1338, u64::MAX, 1 << 63, 0x1234_5678_9abc_def0];
        state.cpuid(Vendor::Amd, user(18), input, NATIVE);
        assert!(state.ready);
        assert_eq!(
            state.cpuid(Vendor::Amd, user(17), input, NATIVE).registers,
            [1, input[1], input[2], input[3]]
        );
        assert!(!state.ready);
        assert_eq!(
            state.cpuid(Vendor::Amd, user(17), input, NATIVE).route,
            Route::PollPending
        );
    }

    #[test]
    fn handler_is_first_nonzero_value_and_protocol_alias_widths_differ() {
        for vendor in [Vendor::Amd, Vendor::Intel] {
            let mut state = State::default();
            state.cpuid(vendor, user(17), [(1 << 32) | 0x1337, 0, 0, 88], NATIVE);
            assert_eq!(state.owner != 0, vendor == Vendor::Amd);
            state.cpuid(vendor, user(17), [0x1337, 0, 0, 88], NATIVE);
            for handler in [0, 0x1234, 0x5678] {
                state.cpuid(vendor, user(17), [0x336933, 0, handler, 0], NATIVE);
            }
            assert_eq!(state.handler, 0x1234);
            state.cpuid(vendor, user(17), [(1 << 32) | 0x336933, 0, 99, 0], NATIVE);
            assert_eq!(state.handler, 0x1234);
        }
    }

    #[test]
    fn xmm5_controls_forwarding_and_r11_never_selects_the_route() {
        let state = State {
            owner: 17,
            handler: 0x1000,
            ..State::default()
        };
        let forwarded = syscall(
            state,
            17,
            u64::MAX,
            0x2000,
            0x1337_1337_1337_1337,
            [BYPASS_MAGIC, u64::MAX],
        );
        assert_eq!(forwarded.route, SyscallRoute::Original);
        assert_eq!(forwarded.rax, u64::from(u32::MAX));
        assert_eq!(forwarded.xmm4, [u64::from(u32::MAX), 0]);
        assert_eq!(forwarded.xmm5, [0, 0]);
        let redirected = syscall(state, 17, 0x55, 0x2000, BYPASS_MAGIC, [0, 99]);
        assert_eq!(redirected.route, SyscallRoute::Handler);
        assert_eq!(
            (redirected.rax, redirected.rcx, redirected.r11),
            (0x2000, 0x1000, BYPASS_MAGIC)
        );
        assert_eq!(redirected.xmm5, [0, 99]);
        assert_eq!(
            syscall(state, 18, 0x55, 0x2000, 3, [BYPASS_MAGIC, 99]).xmm5,
            [BYPASS_MAGIC, 99]
        );
    }
}
