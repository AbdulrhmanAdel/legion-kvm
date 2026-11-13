use super::*;
#[test]
fn accepts_both_observed_wine_backings_without_machine_specific_ids() {
    assert!(recognized(b"/memfd:wine-mapping (deleted)", 1000));
    assert!(recognized(
        b"/tmp/.wine-1000/server-1d-2a7dd08/tmpmap-43c575eb (deleted)",
        1000
    ));
    assert!(recognized(
        b"/tmp/.wine-1001/server-abcd-1234/tmpmap-1f",
        1001
    ));
}
#[test]
fn rejects_unrelated_files_wrong_owner_and_malformed_paths() {
    for name in [
        b"/tmp/ordinary".as_slice(),
        b"/memfd:not-wine",
        b"/tmp/.wine-1001/server-1d-1234/tmpmap-ab",
        b"/tmp/.wine-1000/server-xy-1234/tmpmap-ab",
        b"/tmp/.wine-1000/server-1d-1234/unrelated",
        b"/tmp/.wine-1000/server-1d-1234/tmpmap-ab/extra",
    ] {
        assert!(!recognized(name, 1000));
    }
}
