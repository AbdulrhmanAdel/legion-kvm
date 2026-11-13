use super::*;

fn ext(name: &str) -> ExtensionProperties {
    let mut e = ExtensionProperties {
        name: [0; 256],
        spec_version: 1,
    };
    for (d, s) in e.name.iter_mut().zip(name.bytes()) {
        *d = s as c_char;
    }
    e
}

fn names(v: &[ExtensionProperties]) -> Vec<String> {
    v.iter()
        .map(|e| {
            unsafe { CStr::from_ptr(e.name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn every_extension_vkrt_saw_fail_is_hidden() {
    for e in [
        "VK_KHR_acceleration_structure",
        "VK_KHR_ray_query",
        "VK_KHR_ray_tracing_pipeline",
        "VK_NV_ray_tracing",
        "VK_NVX_binary_import",
        "VK_NV_optical_flow",
        "VK_NV_cuda_kernel_launch",
    ] {
        assert!(is_hidden(e.as_bytes()), "{e}");
    }
}

/// A prefix of a hidden name is a different extension.
#[test]
fn names_match_whole() {
    assert!(!is_hidden(b"VK_KHR_ray_query_x"));
    assert!(!is_hidden(b"VK_KHR_ray"));
    assert!(!is_hidden(b"VK_KHR_deferred_host_operations"));
}

#[test]
fn the_rest_pass_in_order() {
    let all = vec![
        ext("VK_KHR_swapchain"),
        ext("VK_KHR_ray_query"),
        ext("VK_EXT_mesh_shader"),
    ];
    assert_eq!(
        names(&filter(all)),
        ["VK_KHR_swapchain", "VK_EXT_mesh_shader"]
    );
}

#[test]
fn a_short_buffer_is_incomplete() {
    let kept = vec![ext("a"), ext("b"), ext("c")];
    let mut n = 0;
    assert_eq!(
        unsafe { copy_out(&kept, &mut n, std::ptr::null_mut()) },
        VK_SUCCESS
    );
    assert_eq!(n, 3);
    let mut buf = [ext(""); 2];
    n = 2;
    assert_eq!(
        unsafe { copy_out(&kept, &mut n, buf.as_mut_ptr()) },
        VK_INCOMPLETE
    );
    assert_eq!(
        (n, names(&buf)),
        (2, vec!["a".to_string(), "b".to_string()])
    );
}

#[test]
fn hidden_feature_bits_are_cleared_and_others_kept() {
    #[repr(C)]
    struct As {
        base: BaseOut,
        bools: [u32; 5],
    }
    #[repr(C)]
    struct Other {
        base: BaseOut,
        bools: [u32; 2],
    }
    let mut other = Other {
        base: BaseOut {
            s_type: 1000202000,
            p_next: std::ptr::null_mut(),
        },
        bools: [1; 2],
    };
    let mut a = As {
        base: BaseOut {
            s_type: 1000150013,
            p_next: &mut other.base,
        },
        bools: [1; 5],
    };
    unsafe { clear_features(&mut a as *mut As as *mut c_void) };
    assert_eq!(a.bools, [0; 5]);
    assert_eq!(other.bools, [1; 2]);
}

#[test]
fn a_device_asking_for_a_hidden_extension_is_caught() {
    let list = [c"VK_KHR_swapchain".as_ptr(), c"VK_NV_optical_flow".as_ptr()];
    let mut ci = DeviceCreateInfo {
        s_type: 3,
        p_next: std::ptr::null(),
        flags: 0,
        queue_create_info_count: 0,
        p_queue_create_infos: std::ptr::null(),
        enabled_layer_count: 0,
        pp_enabled_layer_names: std::ptr::null(),
        enabled_extension_count: 2,
        pp_enabled_extension_names: list.as_ptr(),
        p_enabled_features: std::ptr::null(),
    };
    assert!(unsafe { asks_hidden(&ci) });
    ci.enabled_extension_count = 1;
    assert!(!unsafe { asks_hidden(&ci) });
}
