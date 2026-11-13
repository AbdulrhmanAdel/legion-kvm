use super::*;

#[test]
fn root_and_sys_admin_are_refused_and_nothing_else() {
    let none = Caps::default();
    assert!(refusal(0, none).is_some());
    assert!(refusal(1000, none).is_none());
    let admin = Caps {
        effective: 0,
        permitted: 1 << CAP_SYS_ADMIN,
    };
    assert!(refusal(1000, admin).is_some());
    let other = Caps {
        effective: 1 << 12, // CAP_NET_ADMIN
        permitted: 1 << 12,
    };
    assert!(refusal(1000, other).is_none());
}

#[test]
fn capability_sets_are_read_from_the_status_file() {
    let status = "Name:\tx\nCapInh:\t0000000000000000\nCapPrm:\t000001ffffffffff\n\
                      CapEff:\t0000000000200000\nCapBnd:\t000001ffffffffff\n";
    let c = Caps::parse(status);
    assert_eq!(c.permitted, 0x1ff_ffff_ffff);
    assert_eq!(c.effective, 1 << CAP_SYS_ADMIN);
    assert!(c.has_sys_admin());
    assert_eq!(Caps::parse(""), Caps::default());
}
