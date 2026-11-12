use crate::state::{ClientState, State};
use anyhow::{Result, ensure};
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use std::{fs, os::unix::fs::MetadataExt, sync::Arc, time::Duration};

pub fn run() -> Result<()> {
    let mut display = Display::<State>::new()?;
    let mut dh = display.handle();
    let (renderer, render_path) = crate::renderer()?;
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read("/usr/share/legion/config.json")?)?;
    let width = u32::try_from(
        config["width"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing benchmark width"))?,
    )?;
    let height = u32::try_from(
        config["height"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing benchmark height"))?,
    )?;
    let mut state = State::new(
        &dh,
        renderer,
        fs::metadata(render_path)?.rdev(),
        width,
        height,
    )?;
    let listener = ListeningSocket::bind("legion-proton-0")?;
    eprintln!("LEGION_PROTON_WAYLAND_READY");
    loop {
        while let Some(stream) = listener.accept()? {
            dh.insert_client(stream, Arc::new(ClientState::default()))?;
        }
        display.dispatch_clients(&mut state)?;
        ensure!(
            state.pending.is_none(),
            "offscreen benchmark unexpectedly presented a window buffer"
        );
        ensure!(
            state.error.is_none(),
            "offscreen Wayland import failed: {:?}",
            state.error
        );
        display.flush_clients()?;
        std::thread::sleep(Duration::from_millis(1));
    }
}
