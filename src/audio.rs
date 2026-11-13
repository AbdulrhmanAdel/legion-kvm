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
#[path = "../tests/unit/audio.rs"]
mod tests;
