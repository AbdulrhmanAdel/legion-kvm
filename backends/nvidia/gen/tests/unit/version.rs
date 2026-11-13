use super::*;

#[test]
fn round_trip() {
    let v = DriverVersion::new(535, 129, 3);
    assert_eq!(v.to_string(), "535.129.03");
}

#[test]
fn parse_ok() {
    assert_eq!(
        DriverVersion::parse("535.129.03"),
        Some(DriverVersion::new(535, 129, 3))
    );
}
