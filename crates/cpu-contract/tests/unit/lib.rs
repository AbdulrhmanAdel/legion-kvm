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
