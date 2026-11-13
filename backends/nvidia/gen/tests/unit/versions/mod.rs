use super::*;

#[test]
fn exact_version_selects_its_own_profile() {
    let t = table_for(DriverVersion::new(580, 178, 4)).expect("580.178.04 has a profile");
    assert!(std::ptr::eq(t, v580_178_04::table()));
}

#[test]
fn version_between_profiles_selects_the_lower_one() {
    // 570.x sits between the 535 and 580 profiles.
    let t = table_for(DriverVersion::new(570, 86, 15)).expect("falls back to 535");
    assert!(std::ptr::eq(t, v535_129_03::table()));
}

#[test]
fn version_above_every_profile_selects_the_highest() {
    let t = table_for(DriverVersion::new(610, 43, 2)).expect("falls back to 595");
    assert!(std::ptr::eq(t, v595_71_05::table()));
}

#[test]
fn version_below_every_profile_is_unsupported() {
    assert!(table_for(DriverVersion::new(470, 0, 0)).is_none());
    assert!(!is_supported(DriverVersion::new(470, 0, 0)));
}

#[test]
fn profiles_are_sorted_ascending() {
    let vs: Vec<_> = supported_versions().collect();
    let mut sorted = vs.clone();
    sorted.sort();
    assert_eq!(vs, sorted, "PROFILES must stay in ascending order");
}
