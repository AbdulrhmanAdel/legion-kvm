use super::*;
#[test]
fn optional_playback_has_no_recording_stream_or_host_device_assignment() {
    let mut command = Command::new("qemu");
    configure(&mut command, false);
    assert_eq!(command.get_args().count(), 0);
    configure(&mut command, true);
    let args = command.get_args().collect::<Vec<_>>();
    assert_eq!(args.len(), 6);
    assert_eq!(args[0], "-audiodev");
    assert!(
        [
            "pipewire,id=legion-audio,out.buffer-length=10000,out.frequency=48000",
            "pa,id=legion-audio,out.buffer-length=10000,out.frequency=48000"
        ]
        .iter()
        .any(|value| args[1] == *value)
    );
    assert_eq!(args[2], "-device");
    assert_eq!(args[3], "intel-hda");
    assert_eq!(args[4], "-device");
    assert_eq!(args[5], "hda-output,audiodev=legion-audio");
}
