use super::*;

fn v(a: u32, b: u32, c: u32) -> DriverVersion {
    DriverVersion::new(a, b, c)
}

#[test]
fn tables_are_sorted_and_unique() {
    for p in PROFILES {
        let t = (p.ctrl)();
        assert!(
            t.windows(2).all(|w| w[0].cmd < w[1].cmd),
            "{} ctrl",
            p.version
        );
        let c = (p.class)();
        assert!(
            c.windows(2).all(|w| w[0].class_id < w[1].class_id),
            "{} class",
            p.version
        );
    }
}

#[test]
fn every_release_has_both_tables() {
    for p in PROFILES {
        assert!(!(p.ctrl)().is_empty(), "{} ctrl", p.version);
        assert!(!(p.class)().is_empty(), "{} class", p.version);
    }
}

/// The point of the whole file.
#[test]
fn denied_controls_are_in_the_tables_and_still_refused() {
    for (cmd, name) in DENY {
        let found = PROFILES
            .iter()
            .any(|p| find_ctrl((p.ctrl)(), *cmd).is_some());
        assert!(found, "{name} is in no table; the deny entry is stale");
        let sel = select(v(615, 71, 9)).unwrap();
        assert!(sel.ctrl_rule(*cmd).is_none(), "{name} passed the allowlist");
    }
}

/// The three classes RM allocates as clients carry no privilege flag in RS_ENTRY, because they never reach the check that wants one -- RM names them in `bClientAlloc` and routes them to `serverAllocClient`.
#[test]
fn the_client_classes_are_allowed_everywhere() {
    for (id, name) in [
        (0x00000000, "NV01_ROOT"),
        (0x00000001, "NV01_ROOT_NON_PRIV"),
        (0x00000041, "NV01_ROOT_CLIENT"),
    ] {
        for p in PROFILES {
            assert!(
                find_class((p.class)(), id).is_some(),
                "{name} missing from {}",
                p.version
            );
        }
        assert!(
            select(v(615, 71, 9)).unwrap().class_entry(id).is_some(),
            "{name}"
        );
        assert!(
            select(v(615, 80, 0)).unwrap().class_entry(id).is_some(),
            "{name} on a fallback"
        );
    }
}

#[test]
fn an_exact_table_is_the_release_s_own() {
    let sel = select(v(615, 71, 9)).unwrap();
    assert!(sel.exact);
    assert!(std::ptr::eq(sel.ctrl, v615_71_09::CTRL));
    assert!(std::ptr::eq(sel.class, v615_71_09::CLASS));
}

#[test]
fn an_unknown_release_falls_back_and_says_so() {
    let sel = select(v(615, 80, 0)).unwrap();
    assert!(!sel.exact);
    assert!(std::ptr::eq(sel.ctrl, v615_78_08::CTRL));
    assert!(select(v(500, 0, 0)).is_none());
}

#[test]
fn host_615_78_08_uses_its_generated_control_layout() {
    let sel = select(v(615, 78, 8)).unwrap();
    assert!(sel.exact);
    assert!(std::ptr::eq(sel.ctrl, v615_78_08::CTRL));
    assert!(matches!(
        sel.ctrl_rule(0x00000214),
        Some(CtrlAllow::Exact(384))
    ));
    assert!(sel.ctrl_rule(0x2080018d).is_none());
}

/// RM's control entry point tries the deprecated table, then the GSP-forwarded commands, then the resource server, and the privilege rule is different in each.
#[test]
fn the_paths_that_carry_no_control_flag() {
    let sel = select(v(615, 71, 9)).unwrap();

    // Rewritten into a modern command whose flags do permit it.
    assert_eq!(
        sel.ctrl_rule(0x2080_1301), // NV2080_CTRL_CMD_FB_GET_INFO
        Some(CtrlAllow::Exact(16)),
        "RM rewrites FB_GET_INFO into FB_GET_INFO_V2 and serves it"
    );
    assert_eq!(sel.ctrl_rule(0x2080_1802), Some(CtrlAllow::Exact(16)));

    // Forwarded to GSP firmware, which RM does not size.
    for cmd in [0x2080_852e, 0x2080_a612, 0x2080_a618, 0x2080_9009] {
        assert_eq!(
            sel.ctrl_rule(cmd),
            Some(CtrlAllow::UpTo(sel.gss.max_params)),
            "{cmd:#x} sets the GSS-legacy bit and RM forwards it"
        );
    }

    // Sent to a BinaryApi object, whose control forwards anything.
    assert_eq!(
        sel.ctrl_rule(0x2081_0108),
        Some(CtrlAllow::UpTo(sel.gss.max_params)),
        "NV2081_BINAPI is a non-privileged class and its control is a passthrough"
    );
}

/// The same three paths refuse.
#[test]
fn the_same_paths_still_refuse() {
    let sel = select(v(615, 71, 9)).unwrap();

    // NV0073_CTRL_CMD_DP_SET_MSA_PROPERTIES: in RM's deprecated table, and rewritten into a V2 command RM marks privileged.
    assert_eq!(sel.ctrl_rule(0x0073_136a), None);
    assert!(
        find_deprec(sel.deprec, 0x0073_136a).is_some_and(|e| !e.allowed),
        "the refusal has to come from the deprecated table, not from a miss"
    );

    // RM's own rule for a GSP-forwarded command: the privileged bits set in full means root, and the backend is not root on the guest's behalf. 0x2080c52e is 0x2080852e with the privileged half set.
    assert!(sel.gss.holds(0x2080_c52e));
    assert!(sel.gss.needs_root(0x2080_c52e));
    assert_eq!(sel.ctrl_rule(0x2080_c52e), None);

    // The privileged twin of the catch-all class.
    assert_eq!(sel.class_entry(0x2082), None);
    assert_eq!(sel.ctrl_rule(0x2082_0108), None);
}

/// Every catch-all range belongs to a class the class table permits.
#[test]
fn a_catch_all_range_is_backed_by_an_allowed_class() {
    for p in PROFILES {
        let sel = select(p.version).unwrap();
        for r in (p.catch_all)() {
            assert!(
                sel.class_entry(r.class_id).is_some(),
                "{} admits commands for class {:#x} it will not allocate",
                p.version,
                r.class_id
            );
        }
    }
}

/// The ceiling differs between releases -- 580.178.04 says 1 MiB where 615.71.09 says 2 -- so a fallback takes the smallest rather than the one that happens to be nearest.
#[test]
fn a_fallback_caps_at_the_smallest_any_release_gives() {
    let smallest = PROFILES.iter().map(|p| (p.gss)().max_params).min().unwrap();
    let newest = select(v(615, 71, 9)).unwrap();
    assert_eq!(newest.max_params(), newest.gss.max_params);
    assert!(
        smallest < newest.gss.max_params,
        "the releases agree on the ceiling; this test is no longer testing anything"
    );
    assert_eq!(select(v(615, 80, 0)).unwrap().max_params(), smallest);
}

/// On a fallback, a control only one release permits is refused: the backend would be permitting on the strength of a table that does not describe the host.
#[test]
fn a_fallback_needs_every_release_to_agree() {
    let only_newest = v615_71_09::CTRL
        .iter()
        .find(|e| {
            PROFILES
                .iter()
                .any(|p| find_ctrl((p.ctrl)(), e.cmd).is_none())
        })
        .expect("the releases differ somewhere");
    let exact = select(v(615, 71, 9)).unwrap();
    assert!(exact.ctrl_rule(only_newest.cmd).is_some());
    let fallback = select(v(615, 80, 0)).unwrap();
    assert!(fallback.ctrl_rule(only_newest.cmd).is_none());
}

/// A control every release agrees on survives the fallback, or the rule above would be refusing everything.
#[test]
fn a_fallback_still_permits_what_every_release_permits() {
    let common = v615_71_09::CTRL
        .iter()
        .find(|e| {
            PROFILES.iter().all(|p| {
                find_ctrl((p.ctrl)(), e.cmd).is_some_and(|o| o.params_size == e.params_size)
            })
        })
        .expect("the releases agree somewhere");
    assert!(
        select(v(615, 80, 0))
            .unwrap()
            .ctrl_rule(common.cmd)
            .is_some()
    );
}
