//! Owned native Proton control using the same app-facing mailbox services.
use super::{desktop_with_fullscreen, preview_image, pump};
use crate::{
    frame_pacing::Pacer,
    native_acquire::Acquirer,
    state::{ClientState, State},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use smithay::reexports::wayland_server::{Display, ListeningSocket, Resource};
use std::{
    fs,
    os::fd::AsFd,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

fn settings(config: &Value) -> Result<(i32, i32, u64)> {
    ensure!(
        config["schema"] == 1
            && config["external_client"] == true
            && config["uncapped"] == true
            && config["acquire_mode"] == "async"
            && matches!(
                config["frame_pacing"].as_str(),
                Some("callbacks" | "unpaced")
            ),
        "unsupported native forwarding control"
    );
    let width = i32::try_from(config["width"].as_u64().context("missing width")?)?;
    let height = i32::try_from(config["height"].as_u64().context("missing height")?)?;
    let seconds = config["seconds"].as_u64().context("missing duration")?;
    ensure!(
        (64..=3840).contains(&width)
            && (64..=2160).contains(&height)
            && (15..=if config["benchmark_client"] == true {
                240
            } else {
                30
            })
                .contains(&seconds),
        "invalid native forwarding dimensions/duration"
    );
    Ok((width, height, seconds))
}

pub(crate) fn run(config: &Value) -> Result<()> {
    let (width, height, seconds) = settings(config)?;
    let benchmark = config["benchmark_client"] == true;
    let frames = Path::new("/frames");
    let fullscreen = config
        .get("forward_fullscreen")
        .map_or(Some(false), Value::as_bool)
        .context("fullscreen setting must be boolean")?;
    let (_connection, mut queue, mut host) = desktop_with_fullscreen(width, height, fullscreen)?;
    let qh = queue.handle();
    let device = host
        .main_device
        .context("host omitted DMA-BUF main device")?;
    let surface = host.surface.clone().context("missing host surface")?;
    let mut display = Display::<State>::new()?;
    let mut dh = display.handle();
    let mut state = State::forwarding(
        &dh,
        host.formats.clone(),
        device,
        u32::try_from(width)?,
        u32::try_from(height)?,
    )?;
    // Wine commits synchronized child surfaces and temporary SHM placeholders.
    state.gpu_only = true;
    let listener = ListeningSocket::bind("legion-proton-0")?;
    let mut acquirer = Acquirer::new()?;
    let mut pacer = Pacer::new(
        config["frame_pacing"]
            .as_str()
            .context("missing frame pacing")?,
    )?;
    let start = Instant::now();
    let mut clients = Vec::new();
    let mut first_at = None;
    let mut first_hash = None;
    let mut last_hash = None;
    let mut detach_at = None;
    let mut cancelled = 0_u64;
    let mut ready_source = None;
    let exited_marker = frames.join("proton-client-exited");
    let mut marker_poll = Instant::now();
    let mut finishing = false;
    let mut forward_wait = crate::forward_wait::ForwardWait::new(config)?;
    let mut fullscreen_retry_pending = fullscreen;
    eprintln!("LEGION_PROTON_NATIVE_FORWARD_READY");
    loop {
        ensure!(
            start.elapsed() < Duration::from_secs(if benchmark { seconds + 60 } else { 60 }),
            "native Proton forwarding timed out"
        );
        ensure!(!host.closed, "owned native forwarding window closed early");
        pump(&mut queue, &mut host)?;
        if fullscreen_retry_pending && host.frame_callbacks > 0 {
            fullscreen_retry_pending = false;
            if !host.fullscreen {
                host.toplevel
                    .as_ref()
                    .context("missing desktop toplevel")?
                    .set_fullscreen(None);
            }
        }
        while let Some(stream) = listener.accept()? {
            clients.push(dh.insert_client(stream, Arc::new(ClientState::default()))?);
        }
        display.dispatch_clients(&mut state)?;
        if let Some(error) = state.error.take() {
            anyhow::bail!("native source Wayland: {error}");
        }
        for (dma, notifier) in state.pending_imports.drain(..) {
            host.import(dma, notifier, &qh)?;
        }
        for buffer in state.destroyed_buffers.drain(..) {
            host.retire(&buffer);
        }
        if !finishing && marker_poll.elapsed() >= Duration::from_millis(10) {
            finishing = exited_marker.exists();
            marker_poll = Instant::now();
        }
        if ready_source.is_none() {
            ready_source = acquirer.pop_ready()?;
        }
        if let Some(buffer) = ready_source.take() {
            if buffer.is_alive() && !finishing {
                if !host.submit(&buffer, &qh)? {
                    // create_immed lets the client commit before the desktop's asynchronous import reply.
                    ready_source = Some(buffer);
                } else if first_hash.is_none() {
                    first_hash = Some(preview_image(
                        &buffer,
                        frames,
                        "native-forward-first.rgba",
                        !benchmark,
                    )?);
                    first_at = Some(Instant::now());
                } else if last_hash.is_none()
                    && first_at.is_some_and(|at: Instant| {
                        at.elapsed()
                            >= Duration::from_secs(if benchmark { 11 } else { seconds / 2 })
                    })
                {
                    last_hash = Some(preview_image(
                        &buffer,
                        frames,
                        "native-forward-last.rgba",
                        !benchmark,
                    )?);
                }
            } else {
                // Its genuine GPU acquisition completed.
                if buffer.is_alive() {
                    buffer.release();
                }
                cancelled += 1;
                pacer.cancel_unsubmitted();
            }
        }
        if !finishing
            && pacer.callbacks_ready(host.frame_callbacks)?
            && acquirer.is_empty()
            && ready_source.is_none()
            && let Some(buffer) = state.pending.take()
        {
            acquirer.push(buffer)?;
            pacer.consume()?;
        }
        if finishing {
            if let Some(buffer) = state.pending.take()
                && buffer.is_alive()
            {
                buffer.release();
            }
            if acquirer.is_empty() && ready_source.is_none() {
                let detached = detach_at.get_or_insert_with(|| {
                    surface.attach(None, 0, 0);
                    surface.commit();
                    Instant::now()
                });
                ensure!(
                    detached.elapsed() < Duration::from_secs(5),
                    "native desktop leases did not drain"
                );
                if host.frames == host.releases {
                    break;
                }
            }
        }
        state.send_callbacks(u32::try_from(start.elapsed().as_millis())?);
        display.flush_clients()?;
        forward_wait.wait(
            [display.as_fd(), listener.as_fd(), _connection.as_fd()],
            !acquirer.is_empty(),
        )?;
    }
    ensure!(
        host.frames > 0
            && host.frames == host.releases
            && host.frame_callbacks > 0
            && acquirer.is_empty(),
        "native forwarding did not drain original fences/releases"
    );
    ensure!(
        benchmark || (last_hash.is_some() && first_hash != last_hash),
        "native forwarded image did not animate"
    );
    ensure!(
        host.configured_size
            .is_none_or(|size| size == (width, height)),
        "native desktop size changed"
    );
    let active = first_at
        .context("missing native forwarding interval")?
        .elapsed()
        .as_secs_f64();
    let acquisition = acquirer.report();
    let wait_report = forward_wait.report()?;
    ensure!(
        acquisition["pending"] == 0
            && acquisition["submitted"] == acquisition["completed"]
            && acquisition["completed"].as_u64() == Some(host.frames + cancelled),
        "native acquisition accounting did not drain"
    );
    let report = json!({"schema":1,"passed":true,"mode":"native_proton_dmabuf_forward",
        "virtual_machine":false,"width":width,"height":height,"frames_forwarded":host.frames,
        "host_buffer_releases":host.releases,"host_frame_callbacks":host.frame_callbacks,
        "host_commits_per_active_second":host.frames as f64/active,"active_forward_seconds":active,
        "source_commits":state.source_commits,"frames_superseded_before_acquire":state.superseded,
        "shm_placeholders_ignored":state.shm_skipped,"cancelled_after_acquire":cancelled,
        "buffer_imports":host.imports,"animation_verified":last_hash.is_some() && first_hash != last_hash,"benchmark_client":benchmark,"diagnostic_readbacks":2,
        "full_frame_cpu_transport":false,"steady_state_cpu_maps":0,"steady_state_gpu_readbacks":0,
        "acquire_fences_pending_at_shutdown":acquisition["pending"],"acquisition":acquisition,
        "frame_pacing":pacer.report(),"forward_wait":wait_report,"fullscreen":host.fullscreen,
        "configured_size":host.configured_size,"present_mode":"mailbox","uncapped":true,
        "host_transport":"native DMA-BUF import and real desktop wl_buffer.release",
        "comparison_scope":"same app-facing globals/callback policy; native and VM host transports differ"});
    fs::write(
        frames.join("proton-forward-result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
}
