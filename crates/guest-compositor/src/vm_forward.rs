//! Guest producer for the opt-in host presentation interface.
use crate::{
    OwnedClient, acquire, acquire_gpu,
    frame_transport::{Cached, Transport, call, ioctl},
    state::{ClientState, State},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use smithay::{
    backend::{
        allocator::{Buffer, Format, Fourcc, Modifier, dmabuf::Dmabuf},
        renderer::{
            Bind, Blit, ExportMem, ImportDma, Offscreen, Renderer, TextureFilter,
            gles::{GlesMapping, GlesRenderbuffer, GlesTexture},
        },
    },
    reexports::wayland_server::{Display, ListeningSocket, protocol::wl_buffer::WlBuffer},
    utils::Rectangle,
    wayland::dmabuf::get_dmabuf,
};
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn word(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("validated fixed packet"))
}
fn wide(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("validated fixed packet"))
}

fn forwarding_timeout(application: bool, benchmark: bool, seconds: u64) -> Option<Duration> {
    // The host launcher controls explicitly bounded application tests.
    (!application).then(|| Duration::from_secs(if benchmark { seconds + 60 } else { 55 }))
}

fn register(fd: &File, source: &WlBuffer) -> Result<u64> {
    let dma = get_dmabuf(source).context("non-DMA-BUF image")?;
    ensure!(
        dma.num_planes() == 1,
        "v1 NVIDIA export adapter requires a single-plane image"
    );
    let plane: BorrowedFd<'_> = dma.handles().next().context("missing image FD")?;
    let mut prime = [0_u8; 12];
    prime[8..12].copy_from_slice(&plane.as_raw_fd().to_le_bytes());
    ioctl(fd, 0xc00c_642e, &mut prime)?;
    let handle = word(&prime, 0);
    let result = (|| -> Result<u64> {
        let mut data = [0_u8; 48];
        for (at, value) in [
            (0, handle),
            (4, u32::try_from(dma.size().w)?),
            (8, u32::try_from(dma.size().h)?),
            (12, dma.format().code as u32),
            (24, dma.strides().next().context("missing stride")?),
            (28, dma.offsets().next().context("missing offset")?),
            (44, u32::from(dma.y_inverted())),
        ] {
            data[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        data[16..24].copy_from_slice(&u64::from(dma.format().modifier).to_le_bytes());
        call(fd, 2, &mut data)?;
        Ok(wide(&data, 32))
    })();
    let mut close = [0_u8; 8];
    close[..4].copy_from_slice(&handle.to_le_bytes());
    ioctl(fd, 0x4008_6409, &mut close)?;
    result
}
struct Acquisition {
    fence: Option<u64>,
    // Keep the queued read's resources alive until its fence completes.
    _mapping: Option<GlesMapping>,
    _texture: Option<GlesTexture>,
    _source: Dmabuf,
    started: Instant,
    diagnostic: bool,
}

fn live_stage(enabled: bool, stage: &str) -> Result<()> {
    if enabled {
        fs::write(
            "/frames/forward-stage.tmp",
            serde_json::to_vec(&json!({"schema":1,"stage":stage}))?,
        )?;
        fs::rename("/frames/forward-stage.tmp", "/frames/forward-stage.json")?;
    }
    Ok(())
}

fn input_keymap(fd: &File, generation: u32) -> Result<String> {
    use legion_display_channel::input_events as events;
    let mut result = Vec::new();
    loop {
        let mut bytes = [0; events::BYTES];
        events::keymap_request(generation, u32::try_from(result.len())?, &mut bytes)?;
        call(fd, 13, &mut bytes)?;
        let (total, chunk) = events::keymap_chunk(&bytes, generation, result.len())?;
        ensure!(
            !chunk.is_empty() || result.len() == total,
            "empty keymap chunk before end"
        );
        result.extend_from_slice(chunk);
        if result.len() == total {
            return String::from_utf8(result).context("host keymap is not UTF-8");
        }
    }
}

fn receive_ordered_input(fd: &File, state: &mut State, now_us: u64) -> Result<(bool, u32)> {
    use legion_display_channel::input_events as events;
    let mut bytes = [0; events::BYTES];
    events::Batch::request(state.input_capture_mode(), &mut bytes)?;
    call(fd, 12, &mut bytes)?;
    let batch = events::Batch::decode(&bytes)?;
    let closed = batch.closed;
    let pending = batch.pending;
    if apply_ordered_input(fd, state, batch, now_us)? {
        Ok((closed, pending))
    } else {
        Ok((closed, 0))
    }
}

fn apply_ordered_input(
    fd: &File,
    state: &mut State,
    batch: legion_display_channel::input_events::Batch,
    now_us: u64,
) -> Result<bool> {
    if batch.keymap_generation != 0
        && batch.keymap_generation != state.ordered_input.keymap_generation
    {
        match input_keymap(fd, batch.keymap_generation) {
            Ok(keymap) => {
                state.reset_ordered_input((now_us / 1000) as u32);
                if let Some(keyboard) = state.seat.get_keyboard() {
                    keyboard.set_keymap_from_string(state, keymap)?;
                }
                state.ordered_input.keymap_generation = batch.keymap_generation;
            }
            Err(error) => {
                // A layout switch may race the chunk transfer.
                state.reset_ordered_input((now_us / 1000) as u32);
                state.ordered_input.map_retries += 1;
                if state.ordered_input.map_retries.is_power_of_two() {
                    eprintln!("host keymap changed or could not be transferred: {error:#}");
                }
                return Ok(false);
            }
        }
    }
    state.ordered_input_batch(batch, now_us);
    Ok(true)
}

pub fn run(config: &Value) -> Result<()> {
    ensure!(
        config["schema"] == 1,
        "unknown VM forwarding configuration schema"
    );
    ensure!(
        std::path::Path::new("/modules/load-order").is_file(),
        "VM forwarding requires prepared guest"
    );
    let width = u32::try_from(config["width"].as_u64().context("missing width")?)?;
    let height = u32::try_from(config["height"].as_u64().context("missing height")?)?;
    let seconds = config["seconds"].as_u64().context("missing duration")?;
    let external = config["external_client"] == true;
    let benchmark = config["benchmark_client"] == true;
    let application = config["application_client"] == true;
    let timeout = forwarding_timeout(application, benchmark, seconds);
    let diagnostics = application && config["diagnostic_frames"] == true;
    let live_telemetry = diagnostics || config["live_telemetry"] == true;
    ensure!(
        !application || (external && !benchmark),
        "application forwarding requires an external non-benchmark client"
    );
    ensure!(
        !benchmark || external,
        "benchmark mode requires an external client"
    );
    ensure!(
        (15..=if benchmark { 240 } else { 30 }).contains(&seconds) && config["uncapped"] == true,
        "invalid bounded uncapped forwarding duration"
    );
    let acquire_mode = config
        .get("acquire_mode")
        .and_then(Value::as_str)
        .unwrap_or("async");
    ensure!(
        matches!(acquire_mode, "async" | "blocking"),
        "unknown acquire mode"
    );
    let asynchronous = acquire_mode == "async";
    let mut pacer = crate::frame_pacing::Pacer::new(
        config
            .get("frame_pacing")
            .and_then(Value::as_str)
            .unwrap_or("unpaced"),
    )?;
    let mut transport = Transport::new(
        config
            .get("frame_transport")
            .and_then(Value::as_str)
            .unwrap_or("separate"),
    )?;
    let (renderer, path) = crate::renderer()?;
    let fd = OpenOptions::new().read(true).write(true).open(&path)?;
    let mut info = vec![0_u8; 4112];
    if transport.is_notified() {
        info[..4]
            .copy_from_slice(&(if transport.is_polled() { 6_u32 } else { 4_u32 }).to_le_bytes());
    }
    call(&fd, 1, &mut info)?;
    ensure!(
        matches!(word(&info, 0), 1..=8) && word(&info, 4) <= 256,
        "unsupported display protocol"
    );
    ensure!(
        !external || word(&info, 0) >= 7,
        "owned Proton window requires the paired desktop-input backend and guest module"
    );
    ensure!(
        !transport.is_combined() || word(&info, 0) >= 2,
        "combined frame transport requires a protocol v2 backend"
    );
    ensure!(
        !transport.is_notified() || word(&info, 0) >= 4,
        "notified frame transport requires a protocol v4 backend"
    );
    ensure!(
        !transport.is_polled() || word(&info, 0) >= 6,
        "polled frame transport requires a protocol v6 backend"
    );
    let supported = renderer.egl_context().display().dmabuf_texture_formats();
    let mut formats = Vec::new();
    for index in 0..usize::try_from(word(&info, 4))? {
        let at = 16 + index * 16;
        if let Ok(code) = Fourcc::try_from(word(&info, at)) {
            let format = Format {
                code,
                modifier: Modifier::from(wide(&info, at + 8)),
            };
            if supported.contains(&format) {
                formats.push(format);
            }
        }
    }
    ensure!(
        !formats.is_empty(),
        "no shared source/desktop image layouts"
    );
    let mut display = Display::<State>::new()?;
    let mut dh = display.handle();
    let mut gpu = acquire_gpu::Gpu::new(renderer, asynchronous)?;
    let mut producer_timeline_acquires = 0_u64;
    let mut state = State::with_formats(
        &dh,
        None,
        formats,
        fs::metadata(&path)?.rdev(),
        width,
        height,
    )?;
    let explicit_sync = if config["explicit_sync"] == true {
        ensure!(
            external,
            "explicit synchronization requires an external producer"
        );
        state.enable_explicit_sync(&dh, OpenOptions::new().read(true).write(true).open(path)?)?
    } else {
        false
    };
    let ordered_input = external && word(&info, 0) >= 8;
    state.track_buffers = true;
    state.gpu_only = external;
    state.diagnose_surfaces = live_telemetry;
    if ordered_input {
        use legion_display_channel::input_events as events;
        let mut bytes = [0; events::BYTES];
        events::Batch::request(events::CaptureMode::None, &mut bytes)?;
        call(&fd, 12, &mut bytes)?;
        let batch = events::Batch::decode(&bytes)?;
        ensure!(
            !batch.closed,
            "presentation window closed before workload startup"
        );
        if batch.relative_supported {
            state.enable_relative_input(&dh);
        }
        apply_ordered_input(&fd, &mut state, batch, 0)?;
    }
    let mut xwayland = crate::xwayland::Bridge::start(config, &dh)?;
    // These locals retain a single target FBO without self-references: target borrows buffer, and both drop before the owning renderer/context.
    let mut acquire_buffer: Option<GlesRenderbuffer> = if asynchronous {
        Some(
            gpu.renderer
                .create_buffer(Fourcc::Abgr8888, (1, 1).into())?,
        )
    } else {
        None
    };
    let mut acquire_target = acquire_buffer
        .as_mut()
        .map(|buffer| gpu.renderer.bind(buffer))
        .transpose()?;
    let listener = ListeningSocket::bind(if external {
        "legion-proton-0"
    } else {
        "legion-vm-forward-0"
    })?;
    let log = File::create("/tmp/vkcube-forward.log")?;
    fs::create_dir("/tmp/legion-forward-metrics")?;
    let mut client = if external {
        None
    } else {
        Some(OwnedClient(Command::new("/usr/bin/vkcube")
        .args(["--wsi","wayland","--width",&width.to_string(),"--height",&height.to_string(),"--present_mode","1","--c","1000000"])
        .env("WAYLAND_DISPLAY","legion-vm-forward-0").env("MANGOHUD","1")
        .env("MANGOHUD_CONFIG","no_display,control=legion-vm-forward-%p,log_duration=10,log_interval=0,output_folder=/tmp/legion-forward-metrics,fps_limit=0,cpu_stats=0,gpu_stats=0")
        .stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?))
    };
    let start = Instant::now();
    let mut clients = Vec::new();
    let mut images = Vec::<Cached>::new();
    let mut frames = 0_u64;
    let mut next_frame = 0_u64;
    let mut ignored_shm = 0_u64;
    let mut returned = 0_u64;
    let mut logger = None;
    let mut logging_at = None;
    let mut input_poll = Instant::now();
    let mut input_queries = 0_u64;
    let mut host_closed = false;
    let mut forwarding_started = None;
    let mut animation = crate::animation::Animation::default();
    let mut next_animation_diagnostic = start;
    let mut diagnostic_readbacks = 0_u64;
    let mut uniform_startup_images = 0_u64;
    let mut next_startup_diagnostic = start;
    let mut barrier_ns = 0_u128;
    let mut barrier_samples = 0_u64;
    let mut submit_ns = 0_u128;
    let mut submit_samples = 0_u64;
    let mut stage_ns = [0_u128; 4];
    let mut latency_ns = 0_u128;
    let mut latency_samples = 0_u64;
    let mut poll_ns = 0_u128;
    let mut poll_calls = 0_u64;
    let mut pending_polls = 0_u64;
    let mut queue_peak = 0_usize;
    let mut acquisitions = acquire::Queue::<Acquisition>::new(3);
    let mut last_release_poll = Instant::now();
    let mut last_live = start;
    let mut exit_marker = (benchmark || application)
        .then(|| crate::exit_marker::ExitMarker::new("/frames/proton-client-exited", start));
    let mut forwarded_cadence = crate::frame_timing::Cadence::default();
    let mut loop_cadence = crate::frame_timing::Cadence::default();
    let mut acquire_latency = crate::frame_timing::Distribution::default();
    let mut transport_latency = crate::frame_timing::Distribution::default();
    let mut registration_latency = crate::frame_timing::Distribution::default();
    let mut forward_wait = crate::forward_wait::ForwardWait::new(config)?;
    loop {
        if live_telemetry {
            loop_cadence.record(Instant::now());
        }
        if live_telemetry && last_live.elapsed() >= Duration::from_secs(1) {
            let report = json!({"schema":1,"elapsed_ms":start.elapsed().as_millis(),
            "source_commits":state.source_commits,"superseded":state.superseded,
            "inactive_surface_buffers":state.inactive_surface_buffers,
            "discarded_on_selection_change":state.discarded_on_selection_change,
            "presentation_switches":state.presentation_switches,
            "source_pending":state.pending.is_some(),"shm_ignored":ignored_shm,
            "explicit_sync":{"pending_acquires":state.pending_sync_acquires(),"timelines":crate::syncobj::report()},
            "registered_images":images.len(),"acquiring":acquisitions.len(),
            "gpu_fences":gpu.pending(),"submitted":frames,"returned":returned,
            "host_closed":host_closed,"input_queries":input_queries,
                "exit_marker_checks":exit_marker.as_ref().map_or(0, crate::exit_marker::ExitMarker::checks),
                "exit_marker_interval_ms":crate::exit_marker::INTERVAL.as_millis(),
            "keyboard_events":state.keyboard_events,"pointer_events":state.pointer_events,
            "input_generation":state.last_input.generation(),
            "ordered_input":ordered_input,"ordered_events":state.ordered_input.events,
            "input_overflows":state.ordered_input.overflows,"keymap_generation":state.ordered_input.keymap_generation,
            "keymap_retries":state.ordered_input.map_retries,"relative_input":state.ordered_input.relative_supported,
            "pointer_capture_active":state.ordered_input.capture_active,
            "pointer_state":state.input_diagnostics(),
            "surfaces":state.surface_diagnostics(),
            "pacer":pacer.report(),
            "frame_timing":{
                "window_ms":last_live.elapsed().as_secs_f64()*1000.0,
                "event_loop_intervals":loop_cadence.take(),
                "exit_marker_checks":exit_marker.as_mut().map(crate::exit_marker::ExitMarker::take_timing),
                "source_intervals":state.source_cadence.take(),
                "forwarded_intervals":forwarded_cadence.take(),
                "acquire_latency":acquire_latency.take(),
                "transport_submit":transport_latency.take(),
                "buffer_registration":registration_latency.take()
            }});
            fs::write("/frames/forward-live.tmp", serde_json::to_vec(&report)?)?;
            fs::rename("/frames/forward-live.tmp", "/frames/forward-live.json")?;
            last_live = Instant::now();
        }
        ensure!(
            timeout.is_none_or(|limit| start.elapsed() < limit),
            "guest forwarding timed out"
        );
        while let Some(stream) = listener.accept()? {
            clients.push(dh.insert_client(stream, Arc::new(ClientState::default()))?);
        }
        display.dispatch_clients(&mut state)?;
        state.poll_explicit_sync(&dh)?;
        if let Some(bridge) = xwayland.as_mut() {
            bridge.dispatch(&mut state)?;
        }
        if external
            && input_poll.elapsed() >= Duration::from_millis(if ordered_input { 1 } else { 10 })
        {
            if ordered_input {
                for _ in 0..8 {
                    let (closed, pending) = receive_ordered_input(
                        &fd,
                        &mut state,
                        u64::try_from(start.elapsed().as_micros())?,
                    )?;
                    host_closed |= closed;
                    input_queries += 1;
                    if pending == 0 {
                        break;
                    }
                }
            } else {
                let mut packet = [0; legion_display_channel::desktop_input::BYTES];
                call(&fd, 11, &mut packet)?;
                let snapshot =
                    legion_display_channel::desktop_input::DesktopInput::decode(&packet)?;
                state.desktop_input(snapshot, u32::try_from(start.elapsed().as_millis())?);
                host_closed |= snapshot.closed;
                input_queries += 1;
            }
            input_poll = Instant::now();
        }
        if let Some(error) = state.error.take() {
            anyhow::bail!("source import: {error}");
        }
        if external
            && state
                .pending
                .as_ref()
                .is_some_and(|buffer| get_dmabuf(buffer).is_err())
        {
            // Wine can attach SHM placeholders/decorations before its Vulkan swapchain.
            if state.pending.is_some() {
                state.discard_pending();
                ignored_shm += 1;
            }
        }
        let finishing = host_closed
            || exit_marker
                .as_mut()
                .is_some_and(|marker| marker.exited(Instant::now()))
            || (!application
                && logging_at.is_some_and(|at: Instant| {
                    at.elapsed()
                        >= Duration::from_secs(if external { seconds } else { seconds - 2 })
                }));
        // One displayed source leaves two application buffers available.
        let release_needed = images.iter().filter(|image| image.frame.is_some()).count() > 1
            || images
                .iter()
                .any(|image| image.retired && image.frame.is_some());
        if !pacer.enabled()
            || (release_needed
                && (transport.is_notified()
                    || last_release_poll.elapsed() >= Duration::from_micros(10)))
        {
            returned += if pacer.enabled() && !transport.is_notified() {
                transport.pull(&fd, &mut images)?
            } else {
                transport.poll_if_due(&fd, &mut images, state.pending.is_some())?
            };
            last_release_poll = Instant::now();
        }
        for destroyed in state.destroyed_buffers.drain(..) {
            if let Some(image) = images.iter_mut().find(|image| image.source == destroyed) {
                if !acquisitions.contains(image.id) {
                    let mut data = image.id.to_le_bytes();
                    call(&fd, 5, &mut data)?;
                }
                image.retired = true;
            }
        }
        while let Some(acquired) = acquisitions.pop_ready(|payload| {
            let begin = Instant::now();
            let ready = match payload.fence {
                Some(frame) => gpu.ready(frame)?,
                None => true,
            };
            if payload.fence.is_some() {
                poll_ns += begin.elapsed().as_nanos();
                poll_calls += 1;
                pending_polls += u64::from(!ready);
            }
            Ok(ready)
        })? {
            let image = images
                .iter_mut()
                .find(|image| image.id == acquired.resource)
                .context("missing acquiring image")?;
            if image.retired {
                let mut data = image.id.to_le_bytes();
                call(&fd, 5, &mut data)?;
                if let Some(point) = image.release_point.take() {
                    point.signal().context("release retired acquired source")?;
                }
                pacer.cancel_unsubmitted();
            } else {
                if !acquired.payload.diagnostic {
                    let elapsed = acquired.payload.started.elapsed();
                    latency_ns += elapsed.as_nanos();
                    latency_samples += 1;
                    if live_telemetry {
                        acquire_latency.record(elapsed);
                    }
                }
                let submit_start = live_telemetry.then(Instant::now);
                returned +=
                    transport.submit(&fd, &mut images, acquired.resource, acquired.frame)?;
                if let Some(begin) = submit_start {
                    transport_latency.record(begin.elapsed());
                    forwarded_cadence.record(Instant::now());
                }
                live_stage(diagnostics && frames == 0, "submitted-first-frame")?;
                frames += 1;
                if frames == 1 {
                    fs::write("/frames/gpu-frame-ready", [])?;
                }
                forwarding_started.get_or_insert_with(Instant::now);
            }
        }
        images.retain(|image| {
            !image.retired || image.frame.is_some() || acquisitions.contains(image.id)
        });
        if !finishing
            && !acquisitions.is_full()
            && pacer.ready(&fd)?
            && let Some(buffer) = state.pending.take()
        {
            pacer.consume()?;
            if !images.iter().any(|image| image.source == buffer) {
                let registration_start = live_telemetry.then(Instant::now);
                let id = register(&fd, &buffer)?;
                if let Some(begin) = registration_start {
                    registration_latency.record(begin.elapsed());
                }
                images.push(Cached {
                    source: buffer.clone(),
                    id,
                    frame: None,
                    acquire_started: None,
                    submitted: None,
                    retired: false,
                    release_point: None,
                });
            }
            let image = images
                .iter_mut()
                .find(|image| image.source == buffer)
                .context("missing image")?;
            ensure!(
                image.frame.is_none() && !image.retired && !acquisitions.contains(image.id),
                "source reused leased buffer"
            );
            ensure!(
                image.release_point.is_none(),
                "source reused a pending release point"
            );
            image.release_point = state.acquire_pending_release();
            let producer_ready = image.release_point.is_some();
            producer_timeline_acquires += u64::from(producer_ready);
            let dma = get_dmabuf(&buffer).context("missing image")?;
            ensure!(
                (external
                    && (1..=3840).contains(&dma.size().w)
                    && (1..=2160).contains(&dma.size().h))
                    || (dma.size().w == i32::try_from(width)?
                        && dma.size().h == i32::try_from(height)?),
                "source image size changed"
            );
            // A game's black loading frame is valid.
            let diagnostic = if diagnostics {
                diagnostic_readbacks < 5 && Instant::now() >= next_startup_diagnostic
            } else {
                !application
                    && ((animation.first().is_none() && Instant::now() >= next_startup_diagnostic)
                        || (logging_at
                            .is_some_and(|at: Instant| at.elapsed() > Duration::from_secs(1))
                            && animation.needs_sample()
                            && Instant::now() >= next_animation_diagnostic))
            };
            let begin = Instant::now();
            live_stage(diagnostics && frames == 0, "acquiring-initial-image")?;
            let renderer = &mut gpu.renderer;
            image.acquire_started = Some(begin);
            let texture = if diagnostic || !asynchronous {
                Some(renderer.import_dmabuf(dma, None)?)
            } else {
                None
            };
            let mut source_image = dma.clone();
            let imported_at = Instant::now();
            // Async acquisition reads the same source image entirely on the GPU, then fences that command stream.
            let mapping = if diagnostic || !asynchronous {
                let size = if diagnostic {
                    dma.size()
                } else {
                    (1, 1).into()
                };
                Some(renderer.copy_texture(
                    texture.as_ref().context("missing diagnostic texture")?,
                    Rectangle::from_size(size),
                    Fourcc::Abgr8888,
                )?)
            } else if producer_ready {
                // The producer timeline already proves this image is ready for host use.
                None
            } else {
                // Smithay caches the EGLImage/renderbuffer/FBO by allocation.
                let source = renderer.bind(&mut source_image)?;
                live_stage(diagnostics && frames == 0, "blitting-initial-image")?;
                renderer.blit(
                    &source,
                    acquire_target.as_mut().context("missing acquire target")?,
                    Rectangle::from_size((1, 1).into()),
                    Rectangle::from_size((1, 1).into()),
                    TextureFilter::Nearest,
                )?;
                None
            };
            let copied_at = Instant::now();
            live_stage(diagnostics && frames == 0, "initial-copy-submitted")?;
            if diagnostic {
                let bytes = renderer
                    .map_texture(mapping.as_ref().context("missing diagnostic mapping")?)?;
                diagnostic_readbacks += 1;
                let first = bytes.get(..4).context("empty diagnostic")?;
                let nonuniform = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel.as_slice() != first);
                if !benchmark
                    && !application
                    && !nonuniform
                    && animation.first().is_none()
                    && xwayland.is_some()
                {
                    // XWayland accelerates startup/helper windows too.
                    uniform_startup_images += 1;
                    next_startup_diagnostic = Instant::now() + Duration::from_millis(250);
                } else {
                    ensure!(
                        benchmark || application || nonuniform,
                        "uniform source image"
                    );
                    if diagnostics {
                        next_startup_diagnostic = Instant::now()
                            + Duration::from_secs(match diagnostic_readbacks {
                                1 => 1,
                                2 => 15,
                                3 => 60,
                                _ => 120,
                            });
                        if diagnostic_readbacks > 1 {
                            fs::write("/frames/vm-forward-last.rgba", bytes)?;
                        }
                        fs::write(
                            "/frames/diagnostic-image.json",
                            serde_json::to_vec(&json!({
                                "width":dma.size().w,"height":dma.size().h,"format":"rgba8","sample_only":true,
                                "nonuniform":nonuniform,"elapsed_ms":start.elapsed().as_millis(),
                                "sample":diagnostic_readbacks,"surfaces":state.surface_diagnostics()
                            }))?,
                        )?;
                    }
                    let mut hash = DefaultHasher::new();
                    bytes.hash(&mut hash);
                    match animation.observe(hash.finish()) {
                        crate::animation::Sample::First => {
                            fs::write("/frames/vm-forward-first.rgba", bytes)?
                        }
                        crate::animation::Sample::Changed => {
                            fs::write("/frames/vm-forward-last.rgba", bytes)?
                        }
                        crate::animation::Sample::Repeated => {
                            next_animation_diagnostic = Instant::now() + Duration::from_millis(37);
                        }
                    }
                }
            } else if !asynchronous {
                let bytes =
                    renderer.map_texture(mapping.as_ref().context("missing blocking mapping")?)?;
                ensure!(bytes.len() == 4, "invalid one-pixel barrier extent");
                barrier_ns += begin.elapsed().as_nanos();
                barrier_samples += 1;
            }
            next_frame += 1;
            if asynchronous {
                let fence = if diagnostic || producer_ready {
                    None
                } else {
                    let (create_ns, flush_ns) = gpu.fence(next_frame)?;
                    stage_ns[2] += create_ns;
                    stage_ns[3] += flush_ns;
                    Some(next_frame)
                };
                if !diagnostic {
                    submit_ns += begin.elapsed().as_nanos();
                    submit_samples += 1;
                    stage_ns[0] += imported_at.duration_since(begin).as_nanos();
                    stage_ns[1] += copied_at.duration_since(imported_at).as_nanos();
                }
                acquisitions.push(
                    image.id,
                    next_frame,
                    Acquisition {
                        fence,
                        _mapping: mapping,
                        _texture: texture,
                        _source: source_image,
                        started: begin,
                        diagnostic,
                    },
                )?;
                queue_peak = queue_peak.max(acquisitions.len());
            } else {
                drop(mapping);
                drop(texture);
                let resource = image.id;
                let submit_start = live_telemetry.then(Instant::now);
                returned += transport.submit(&fd, &mut images, resource, next_frame)?;
                if let Some(begin) = submit_start {
                    transport_latency.record(begin.elapsed());
                    forwarded_cadence.record(Instant::now());
                }
                frames += 1;
                if frames == 1 {
                    fs::write("/frames/gpu-frame-ready", [])?;
                }
                forwarding_started.get_or_insert_with(Instant::now);
            }
            gpu.renderer.cleanup_texture_cache()?;
        }
        state.send_callbacks(u32::try_from(start.elapsed().as_millis())?);
        display.flush_clients()?;
        if external && logging_at.is_none() && animation.first().is_some() {
            logging_at = Some(Instant::now());
        }
        if !external
            && logger.is_none()
            && animation.first().is_some()
            && start.elapsed() > Duration::from_secs(2)
        {
            let address = SocketAddr::from_abstract_name(format!(
                "legion-vm-forward-{}",
                client.as_ref().context("missing vkcube child")?.0.id()
            ))?;
            if let Ok(mut socket) = UnixStream::connect_addr(&address) {
                socket.write_all(b":logging=1;")?;
                logger = Some(socket);
                logging_at = Some(Instant::now());
            }
        }
        ensure!(
            external || logger.is_some() || start.elapsed() < Duration::from_secs(8),
            "guest logger did not start"
        );
        ensure!(
            client
                .as_mut()
                .map(|client| client.0.try_wait())
                .transpose()?
                .flatten()
                .is_none(),
            "application exited early: {}",
            fs::read_to_string("/tmp/vkcube-forward.log")?
        );
        if finishing && acquisitions.is_empty() {
            break;
        }
        if let Some(bridge) = &xwayland {
            if release_needed || !pacer.enabled() {
                forward_wait.wait(
                    [
                        display.as_fd(),
                        listener.as_fd(),
                        fd.as_fd(),
                        bridge.as_fd(),
                    ],
                    !acquisitions.is_empty(),
                )?;
            } else {
                forward_wait.wait(
                    [display.as_fd(), listener.as_fd(), bridge.as_fd()],
                    !acquisitions.is_empty(),
                )?;
            }
        } else if release_needed || !pacer.enabled() {
            forward_wait.wait(
                [display.as_fd(), listener.as_fd(), fd.as_fd()],
                !acquisitions.is_empty(),
            )?;
        } else {
            forward_wait.wait(
                [display.as_fd(), listener.as_fd()],
                !acquisitions.is_empty(),
            )?;
        }
    }
    ensure!(
        gpu.pending() == 0,
        "acquire fences left pending at shutdown"
    );
    let active_end = Instant::now();
    let forwarding_started = forwarding_started.context("no frame reached the host presenter")?;
    let active_seconds = active_end.duration_since(forwarding_started).as_secs_f64();
    let source_commits = state.source_commits;
    let superseded = state.superseded;
    let pending_source = u64::from(state.pending.is_some());
    ensure!(
        source_commits
            == superseded
                + next_frame
                + pending_source
                + ignored_shm
                + state.shm_skipped
                + state.inactive_surface_buffers
                + state.discarded_on_selection_change,
        "source mailbox accounting disagrees: commits={source_commits}, superseded={superseded}, acquired={next_frame}, pending={pending_source}, ignored_shm={ignored_shm}, skipped_shm={}, inactive={}, selection_discarded={}",
        state.shm_skipped,
        state.inactive_surface_buffers,
        state.discarded_on_selection_change
    );
    if explicit_sync {
        state.discard_pending();
    }
    state.close_clients();
    display.flush_clients()?;
    for image in &mut images {
        if !image.retired {
            let mut data = image.id.to_le_bytes();
            call(&fd, 5, &mut data)?;
            image.retired = true;
        }
    }
    let end = Instant::now();
    while images.iter().any(|image| image.frame.is_some()) && end.elapsed() < Duration::from_secs(3)
    {
        returned += transport.pull(&fd, &mut images)?;
        std::thread::sleep(Duration::from_micros(100));
    }
    ensure!(
        returned == frames && images.iter().all(|image| image.frame.is_none()),
        "host omitted completed buffer releases"
    );
    let mut events = [0_u8; 24];
    if transport.is_notified() {
        events = crate::frame_transport::final_event_accounting(&fd, frames)?;
        ensure!(
            wide(&events, 8) == frames,
            "host event acknowledgements disagree"
        );
        ensure!(
            !transport.is_polled() || wide(&events, 16) == 0,
            "polled completions requested interrupts"
        );
    }
    let mut stats = [0_u8; 40];
    call(&fd, 6, &mut stats)?;
    ensure!(
        wide(&stats, 8) == frames && wide(&stats, 16) == frames && wide(&stats, 24) > 0,
        "host display counters disagree: imports={}, commits={}, releases={}, callbacks={}, frames={frames}, source_commits={source_commits}, skipped_shm={}",
        wide(&stats, 0),
        wide(&stats, 8),
        wide(&stats, 16),
        wide(&stats, 24),
        ignored_shm + state.shm_skipped
    );
    fs::write(
        "/frames/animation-diagnostics.json",
        serde_json::to_vec_pretty(&json!({
            "schema":1,"verified":animation.verified(),"late_samples":animation.attempts(),
            "readbacks":diagnostic_readbacks,"host_closed":host_closed,"source_commits":source_commits,
            "frames":frames,"elapsed_seconds":start.elapsed().as_secs_f64(),
            "sampling_seconds":logging_at.map(|at|at.elapsed().as_secs_f64())
        }))?,
    )?;
    if external {
        // Keep Wine's Wayland connection alive through Present/destructor completion.
        let end = Instant::now();
        while !std::path::Path::new("/frames/proton-client-exited").is_file() {
            ensure!(
                end.elapsed() < Duration::from_secs(15),
                "owned Proton client did not finish shutdown"
            );
            display.dispatch_clients(&mut state)?;
            if let Some(bridge) = xwayland.as_mut() {
                bridge.dispatch(&mut state)?;
            }
            state.discard_pending();
            state.poll_explicit_sync(&dh)?;
            state.send_callbacks(u32::try_from(start.elapsed().as_millis())?);
            display.flush_clients()?;
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    if explicit_sync {
        let end = Instant::now();
        loop {
            display.dispatch_clients(&mut state)?;
            state.poll_explicit_sync(&dh)?;
            state.discard_pending();
            display.flush_clients()?;
            if state.pending_sync_acquires() == 0 {
                break;
            }
            ensure!(
                end.elapsed() < Duration::from_secs(3),
                "surface acquire shutdown timed out"
            );
            std::thread::sleep(Duration::from_micros(100));
        }
        if let Some(error) = state.error.take() {
            anyhow::bail!("surface synchronization: {error}");
        }
    }
    ensure!(
        state.pending_sync_acquires() == 0,
        "surface acquires remain pending"
    );
    if explicit_sync {
        crate::syncobj::validate_releases()?;
    }
    // Validation must not disconnect Wine while it is still completing Present/destructors; report failure only after the client shutdown wait.
    ensure!(
        benchmark || application || animation.verified(),
        "source animation was not verified"
    );
    let end = Instant::now();
    while client
        .as_mut()
        .map(|client| client.0.try_wait())
        .transpose()?
        .flatten()
        .is_none()
        && end.elapsed() < Duration::from_secs(1)
    {
        if external {
            break;
        }
        display.dispatch_clients(&mut state)?;
        display.flush_clients()?;
        std::thread::sleep(Duration::from_millis(5));
    }
    for entry in fs::read_dir("/tmp/legion-forward-metrics")? {
        let entry = entry?;
        ensure!(
            entry.metadata()?.len() <= 64 * 1024 * 1024,
            "metrics too large"
        );
        fs::copy(
            entry.path(),
            std::path::Path::new("/frames").join(entry.file_name()),
        )?;
    }
    let stages = stage_ns.map(|ns| {
        if submit_samples == 0 {
            0.0
        } else {
            ns as f64 / submit_samples as f64 / 1e6
        }
    });
    let timeline_only = asynchronous && producer_timeline_acquires == next_frame;
    let acquire_dependency = if !asynchronous {
        "readback_map"
    } else if timeline_only {
        "producer_timeline"
    } else if producer_timeline_acquires > 0 {
        "producer_timeline_or_gpu_blit"
    } else {
        "gpu_blit_fence"
    };
    let acquire_fence_type = if !asynchronous {
        "none"
    } else if timeline_only {
        "drm_syncobj"
    } else {
        "gl_sync"
    };
    let mut report = json!({
        "schema": 1,
        "passed": true,
        "mode": "vm_dmabuf_forward",
        "virtual_machine": true,
        "width": width,
        "height": height,
        "frames_forwarded": frames,
        "host_buffer_releases": returned,
        "host_imports": wide(&stats, 0),
        "host_frame_callbacks": wide(&stats, 24),
        "host_fence_waits": wide(&stats, 32),
        "frame_transport": transport.mode(),
        "transport_rpc": transport.report(),
        "completion_events": {"batches":wide(&events, 0), "acknowledgements":wide(&events, 8), "interrupt_requests":wide(&events, 16)},
        "acquire_mode": acquire_mode,
        "acquire_dependency":acquire_dependency,
        "acquire_fence_type":acquire_fence_type,
        "acquire_barrier_pixels": u32::from(!timeline_only),
        "acquire_barrier_mean_ms": if barrier_samples == 0 { 0.0 } else { barrier_ns as f64 / barrier_samples as f64 / 1e6 },
        "blocking_acquire_samples": barrier_samples,
        "acquire_submit_mean_ms": if submit_samples == 0 { 0.0 } else { submit_ns as f64 / submit_samples as f64 / 1e6 },
        "acquire_latency_mean_ms": if latency_samples == 0 { 0.0 } else { latency_ns as f64 / latency_samples as f64 / 1e6 },
        "acquire_poll_mean_ms": if poll_calls == 0 { 0.0 } else { poll_ns as f64 / poll_calls as f64 / 1e6 },
        "acquire_stage_mean_ms": { "source_import": stages[0], "gpu_blit": stages[1], "fence_creation": stages[2], "flush": stages[3] },
        "acquire_poll_calls": poll_calls,
        "acquire_pending_polls": pending_polls,
        "acquire_queue_peak": queue_peak,
        "acquire_fences_pending_at_shutdown": gpu.pending(),
        "steady_state_cpu_maps": barrier_samples,
        "steady_state_gpu_readbacks": barrier_samples,
        "full_frame_cpu_transport": false,
        "diagnostic_readbacks": diagnostic_readbacks,
        "uniform_startup_images": uniform_startup_images,
        "animation_verified": animation.verified(),
        "benchmark_client": benchmark,
        "present_mode": "mailbox",
        "uncapped": true,
        "vsync_requested": false,
        "elapsed_seconds": start.elapsed().as_secs_f64(),
        "delivered_fps": frames as f64 / start.elapsed().as_secs_f64(),
        "client_log": fs::read_to_string("/tmp/vkcube-forward.log")?,
    });
    report["producer_timeline_acquires"] = json!(producer_timeline_acquires);
    report["active_forward_seconds"] = json!(active_seconds);
    report["application_client"] = json!(application);
    report["initialization_seconds"] =
        json!(forwarding_started.duration_since(start).as_secs_f64());
    report["host_commits_per_active_second"] = json!(frames as f64 / active_seconds);
    report["rate_scope"] = json!("host submissions; frame callbacks do not prove physical scanout");
    report["input_queries"] = json!(input_queries);
    report["explicit_sync"] = json!({"enabled":explicit_sync,"pending_acquires":state.pending_sync_acquires(),"timelines":crate::syncobj::report()});
    report["xwayland"] = json!({
        "enabled": xwayland.is_some(),
        "mapped_windows": state.x11_maps,
        "surface_associations": state.x11_associations,
    });
    report["keyboard_events_forwarded"] = json!(state.keyboard_events);
    report["pointer_events_forwarded"] = json!(state.pointer_events);
    report["host_close_requested"] = json!(host_closed);
    report["frame_pacing"] = pacer.report();
    report["forward_wait"] = forward_wait.report()?;
    report["source_commits"] = json!(source_commits);
    report["sources_acquired"] = json!(next_frame);
    report["frames_superseded_before_acquire"] = json!(superseded);
    report["pending_source_at_shutdown"] = json!(pending_source);
    report["inactive_surface_buffers"] = json!(state.inactive_surface_buffers);
    report["discarded_on_selection_change"] = json!(state.discarded_on_selection_change);
    report["release_poll_policy"] = json!(if pacer.enabled() {
        "replacement_or_retirement"
    } else {
        "continuous_control"
    });
    report["external_client"] = json!(external);
    report["shm_placeholders_ignored"] = json!(ignored_shm + state.shm_skipped);
    if external {
        fs::write(
            "/frames/proton-forward-result.json",
            serde_json::to_vec_pretty(&report)?,
        )?;
    }
    println!("{report}");
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/vm_forward_timeout_tests.rs"]
mod timeout_tests;
