use super::*;
#[test]
fn stock_proton_is_default_and_has_no_version_specific_injection() -> Result<()> {
    assert!(!enabled(None)?);
    assert!(!enabled(Some("0"))?);
    assert!(enabled(Some("1"))?);
    assert!(enabled(Some("auto")).is_err());
    let mut launch = serde_json::json!({"application":{"executable":"game.exe"},"cpu_adapter":true,"resolver_bridge":true});
    configure(&mut launch, None);
    assert!(launch["wine_overlay"].is_null());
    for key in [
        "ephemeral_wine_overlay",
        "cpu_adapter",
        "kuser_worker",
        "nt_syscall_audit",
        "wine_dispatch_audit",
        "resolver_bridge",
    ] {
        assert_eq!(launch[key], false, "{key}");
    }
    assert_eq!(launch["application"]["executable"], "game.exe");
    configure(&mut launch, Some(serde_json::json!({"verified":"recipe"})));
    assert_eq!(launch["cpu_adapter"], true);
    assert_eq!(launch["wine_overlay"]["verified"], "recipe");
    Ok(())
}
#[test]
fn recipe_checks_bounds_size_and_result_digest() {
    let mut r = Recipe {
        schema: 1,
        source_sha256: String::new(),
        unix_sha256: String::new(),
        target_sha256: format!("{:x}", Sha256::digest(b"old-new")),
        target_bytes: 7,
        pieces: vec![
            Piece::Copy {
                source: 0,
                length: 3,
            },
            Piece::Literal {
                data: "2d6e6577".into(),
            },
        ],
    };
    assert_eq!(apply(&r, b"old").expect("valid recipe"), b"old-new");
    assert!(apply(&r, b"x").is_err());
    r.target_bytes = 6;
    assert!(apply(&r, b"old").is_err());
    r.target_bytes = 7;
    r.target_sha256 = "bad".into();
    assert!(apply(&r, b"old").is_err());
}
