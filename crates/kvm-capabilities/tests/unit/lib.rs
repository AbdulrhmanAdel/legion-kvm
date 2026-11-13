use super::*;

fn fixture(sync_mmu: u32, max: u32, recommended: u32) -> Capabilities {
    collect(|request, cap| {
        Ok(if request == API_VERSION {
            12
        } else {
            match cap {
                16 => sync_mmu,
                66 => max,
                9 => recommended,
                10 => 512,
                _ => 1,
            }
        })
    })
    .expect("fixture query")
}

#[test]
fn preserves_numeric_limits_and_separates_fast_path_support() {
    let caps = fixture(1, 4096, 16);
    assert_eq!(caps.memory_slots, Some(512));
    assert!(caps.cpu_probe_issues(32).is_empty());
    assert!(caps.fixed_backing_prerequisites_present());
    let caps = fixture(0, 4096, 16);
    assert!(caps.cpu_probe_issues(32).is_empty());
    assert!(!caps.fixed_backing_prerequisites_present());
}

#[test]
fn uses_documented_old_host_limits_without_confusing_recommendation_with_maximum() {
    assert!(fixture(1, 0, 8).cpu_probe_issues(9).len() == 1);
    assert!(fixture(1, 0, 0).cpu_probe_issues(4).is_empty());
    assert!(fixture(1, 0, 0).cpu_probe_issues(5).len() == 1);
    assert!(!fixture(1, 32, 8).cpu_probe_issues(0).is_empty());
}

#[test]
fn unknown_api_stops_queries_and_ioctl_errors_remain_errors() {
    let mut calls = 0;
    let caps = collect(|_, _| {
        calls += 1;
        Ok(11)
    })
    .expect("unknown API report");
    assert_eq!(calls, 1);
    assert!(!caps.fixed_backing_prerequisites_present());
    assert!(!caps.cpu_probe_issues(1).is_empty());
    let error = collect(|_, _| Err(io::Error::from_raw_os_error(libc::ENOTTY)))
        .expect_err("failed query must not become an unsupported-capability result");
    assert_eq!(error.raw_os_error(), Some(libc::ENOTTY));
}
