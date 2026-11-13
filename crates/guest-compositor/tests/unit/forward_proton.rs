use super::*;
#[test]
fn external_control_checks_bounds_and_supported_pacing_before_connecting() {
    let valid = json!({"schema":1,"external_client":true,"uncapped":true,
            "acquire_mode":"async","frame_pacing":"callbacks","width":1280,"height":720,"seconds":20});
    assert_eq!(
        settings(&valid).expect("bounded owned control"),
        (1280, 720, 20)
    );
    for (key, value) in [
        ("width", json!(0)),
        ("height", json!(5000)),
        ("seconds", json!(31)),
        ("external_client", json!(false)),
        ("acquire_mode", json!("none")),
        ("frame_pacing", json!("unknown")),
    ] {
        let mut invalid = valid.clone();
        invalid[key] = value;
        assert!(settings(&invalid).is_err(), "{key}");
    }
    let mut unpaced = valid.clone();
    unpaced["frame_pacing"] = json!("unpaced");
    assert_eq!(
        settings(&unpaced).expect("unpaced control"),
        (1280, 720, 20)
    );
    let mut benchmark = valid.clone();
    benchmark["benchmark_client"] = json!(true);
    benchmark["seconds"] = json!(180);
    assert_eq!(
        settings(&benchmark).expect("bounded benchmark"),
        (1280, 720, 180)
    );
    benchmark["seconds"] = json!(241);
    assert!(settings(&benchmark).is_err());
}
