use super::*;

#[test]
fn snapshot_round_trip_and_rejection_boundaries() {
    let value = DesktopInput {
        input: Input::new(65535, 182, true, 0xff_ffff).expect("bounded input"),
        closed: true,
        width: 3840,
        height: 2160,
    };
    let mut wire = [0; BYTES];
    value.encode(&mut wire).expect("encode");
    assert_eq!(DesktopInput::decode(&wire).expect("decode"), value);
    for at in [0, 7, 15, 24, 31] {
        let mut invalid = wire;
        invalid[at] ^= 128;
        assert!(DesktopInput::decode(&invalid).is_err());
    }
    assert!(DesktopInput::decode(&wire[..31]).is_err());
    let mut invalid = wire;
    invalid[16..20].copy_from_slice(&0_u32.to_le_bytes());
    assert!(DesktopInput::decode(&invalid).is_err());
    assert!(
        DesktopInput { width: 0, ..value }
            .encode(&mut wire)
            .is_err()
    );
}
