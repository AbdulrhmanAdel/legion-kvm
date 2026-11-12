//! Optional desktop playback through QEMU's unprivileged desktop backends.
use std::{os::unix::fs::FileTypeExt, path::PathBuf, process::Command};

fn desktop_driver() -> &'static str {
    // Follow the user's desktop session; never select a hardware sink here.
    if let Some(root) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        let socket_exists = |name| {
            root.join(name)
                .metadata()
                .is_ok_and(|m| m.file_type().is_socket())
        };
        // This also uses PipeWire through its PulseAudio compatibility server.
        if !socket_exists("pulse/native") && socket_exists("pipewire-0") {
            return "pipewire";
        }
    }
    "pa"
}

pub fn configure(command: &mut Command, desktop: bool) {
    if desktop {
        command.args([
            "-audiodev",
            // Keep the emulated buffer below typical 40 ms guest buffers.
            &format!(
                "{},id=legion-audio,out.buffer-length=10000,out.frequency=48000",
                desktop_driver()
            ),
            "-device",
            "intel-hda",
            "-device",
            "hda-output,audiodev=legion-audio",
        ]);
    }
}

#[cfg(test)]
mod tests {
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
}
