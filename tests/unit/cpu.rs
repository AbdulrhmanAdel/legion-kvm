use super::*;

#[test]
fn topology_extension_follows_actual_host_capability_and_vendor() {
    assert_eq!(
        boot_cpu_for_info("vendor_id : AuthenticAMD\nflags : avx topoext\n"),
        format!("{BOOT_CPU},+topoext")
    );
    for info in [
        "vendor_id : AuthenticAMD\nflags : avx\n",
        "vendor_id : GenuineIntel\nflags : avx topoext\n",
    ] {
        assert_eq!(boot_cpu_for_info(info), BOOT_CPU);
    }
}
