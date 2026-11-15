use crate::{
    OwnedClient,
    state::{ClientState, State},
};
use anyhow::{Context, Result, ensure};
use drm::{
    Device,
    buffer::{self, DrmFourcc, DrmModifier, PlanarBuffer},
    control::{self, Device as ControlDevice, connector, crtc, framebuffer},
};
use serde_json::json;
use smithay::{
    backend::allocator::{Buffer, Format, Fourcc, Modifier, dmabuf::Dmabuf},
    reexports::wayland_server::{Display, ListeningSocket, protocol::wl_buffer::WlBuffer},
    wayland::dmabuf::get_dmabuf,
};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

struct Card(File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

struct Image {
    dma: Dmabuf,
    fb: framebuffer::Handle,
}
struct Description {
    gem: buffer::Handle,
    format: DrmFourcc,
    size: (u32, u32),
    stride: u32,
    offset: u32,
}
impl PlanarBuffer for Description {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> DrmFourcc {
        self.format
    }
    // Legacy ADDFB2 accepts linear storage without the modifier flag.
    fn modifier(&self) -> Option<DrmModifier> {
        None
    }
    fn pitches(&self) -> [u32; 4] {
        [self.stride, 0, 0, 0]
    }
    fn offsets(&self) -> [u32; 4] {
        [self.offset, 0, 0, 0]
    }
    fn handles(&self) -> [Option<buffer::Handle>; 4] {
        [Some(self.gem), None, None, None]
    }
}

struct Scanout {
    card: Card,
    crtc: crtc::Handle,
    connector: connector::Handle,
    mode: control::Mode,
    images: Vec<Image>,
    handles: Vec<buffer::Handle>,
    current: Option<WlBuffer>,
    queued: Option<WlBuffer>,
    frames: u64,
    cursor: Option<crate::cursor::Cursor>,
}
impl Drop for Scanout {
    fn drop(&mut self) {
        if let Some(cursor) = self.cursor.take() {
            cursor.destroy(&self.card, self.crtc);
        }
        // Detach scanout before releasing client storage and destroying FBs.
        let _ = self.card.set_crtc(self.crtc, None, (0, 0), &[], None);
        for buffer in self.queued.take().into_iter().chain(self.current.take()) {
            buffer.release();
        }
        for image in &self.images {
            let _ = self.card.destroy_framebuffer(image.fb);
        }
        for handle in &self.handles {
            let _ = self.card.close_buffer(*handle);
        }
    }
}

impl Scanout {
    fn open(requested: Option<(u16, u16)>) -> Result<Self> {
        for entry in fs::read_dir("/dev/dri")? {
            let path = entry?.path();
            if !path
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"card"))
            {
                continue;
            }
            let card = Card(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&path)?,
            );
            if card.get_driver()?.name().as_encoded_bytes() != b"virtio_gpu" {
                continue;
            }
            card.acquire_master_lock()
                .context("acquire guest virtual display")?;
            let resources = card.resource_handles()?;
            for connector in resources.connectors() {
                let info = card.get_connector(*connector, true)?;
                if info.state() != connector::State::Connected {
                    continue;
                }
                let mode = *info
                    .modes()
                    .iter()
                    .find(|mode| requested.is_none_or(|size| mode.size() == size))
                    .context("virtual display does not offer the requested mode")?;
                let encoder = info
                    .encoders()
                    .first()
                    .context("virtual display has no encoder")?;
                let encoder = card.get_encoder(*encoder)?;
                let crtc = *resources
                    .filter_crtcs(encoder.possible_crtcs())
                    .first()
                    .context("virtual display has no CRTC")?;
                let cursor = Some(crate::cursor::Cursor::new(&card)?);
                return Ok(Self {
                    card,
                    crtc,
                    connector: *connector,
                    mode,
                    images: Vec::new(),
                    handles: Vec::new(),
                    current: None,
                    queued: None,
                    frames: 0,
                    cursor,
                });
            }
        }
        anyhow::bail!("no connected guest virtio-GPU display")
    }

    fn import(&mut self, dma: &Dmabuf) -> Result<framebuffer::Handle> {
        if let Some(image) = self.images.iter().find(|image| image.dma == *dma) {
            return Ok(image.fb);
        }
        ensure!(
            self.images.len() < 256,
            "scanout import cache exceeds 256 images"
        );
        let (width, height) = self.mode.size();
        ensure!(
            dma.size().w == i32::from(width) && dma.size().h == i32::from(height),
            "scanout image {:?} does not match the display mode {width}x{height}",
            dma.size()
        );
        ensure!(
            dma.num_planes() == 1
                && matches!(dma.format().code, Fourcc::Xrgb8888 | Fourcc::Xbgr8888)
                && dma.format().modifier == Modifier::Linear
                && !dma.y_inverted(),
            "scanout requires non-inverted, single-plane linear opaque RGB8888"
        );
        let gem = self
            .card
            .prime_fd_to_buffer(dma.handles().next().context("missing DMA-BUF plane")?)?;
        // PRIME import may return an already-owned GEM handle for another FD.
        if !self.handles.contains(&gem) {
            self.handles.push(gem);
        }
        let description = Description {
            gem,
            format: dma.format().code,
            size: (u32::from(width), u32::from(height)),
            stride: dma.strides().next().context("missing stride")?,
            offset: dma.offsets().next().context("missing offset")?,
        };
        let fb = self
            .card
            .add_planar_framebuffer(&description, control::FbCmd2Flags::empty())
            .context("create linear guest scanout framebuffer")?;
        self.images.push(Image {
            dma: dma.clone(),
            fb,
        });
        Ok(fb)
    }

    fn submit(&mut self, buffer: WlBuffer) -> Result<()> {
        ensure!(self.queued.is_none(), "scanout page flip is already queued");
        let fb = self.import(get_dmabuf(&buffer).context("non-DMA-BUF scanout image")?)?;
        if self.current.is_none() {
            self.card.set_crtc(
                self.crtc,
                Some(fb),
                (0, 0),
                &[self.connector],
                Some(self.mode),
            )?;
            self.current = Some(buffer);
            self.frames += 1;
        } else {
            self.card
                .page_flip(self.crtc, fb, control::PageFlipFlags::EVENT, None)?;
            self.queued = Some(buffer);
        }
        Ok(())
    }

    fn complete(&mut self) -> Result<bool> {
        let events = match self.card.receive_events() {
            Ok(events) => events,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let mut completed = false;
        for event in events {
            if matches!(event, control::Event::PageFlip(_)) {
                let next = self
                    .queued
                    .take()
                    .context("unexpected guest page-flip completion")?;
                if let Some(previous) = self.current.replace(next) {
                    previous.release();
                }
                self.frames += 1;
                completed = true;
            }
        }
        Ok(completed)
    }
}

pub(crate) fn run() -> Result<()> {
    ensure!(
        std::path::Path::new("/modules/load-order").is_file(),
        "KMS fixture requires prepared guest"
    );
    let settings: serde_json::Value =
        serde_json::from_slice(&fs::read("/frames/kms-presentation.json")?)?;
    let external = settings["client"] == "external";
    let application = external && settings["application"] == true;
    let seconds = settings["seconds"].as_u64().unwrap_or(15);
    ensure!(
        external || (5..=30).contains(&seconds),
        "invalid graphics check duration"
    );
    ensure!(
        settings["schema"] == 1 && (external || settings["client"] == "cube"),
        "unsupported KMS presentation fixture configuration"
    );
    // SDL may publish its tiled window size as the preferred EDID mode.
    // An explicitly sized fixture must still use its requested render mode.
    let requested = settings["width"]
        .as_u64()
        .zip(settings["height"].as_u64())
        .map(|(w, h)| Ok::<_, anyhow::Error>((u16::try_from(w)?, u16::try_from(h)?)))
        .transpose()?;
    let mut scanout = Scanout::open(requested)?;
    let (width, height) = scanout.mode.size();
    let mut display = Display::<State>::new()?;
    let mut dh = display.handle();
    // Only the virtual KMS device's supported scanout layout is advertised.
    let formats = vec![
        Format {
            code: Fourcc::Xrgb8888,
            modifier: Modifier::Linear,
        },
        Format {
            code: Fourcc::Argb8888,
            modifier: Modifier::Linear,
        },
        Format {
            code: Fourcc::Xbgr8888,
            modifier: Modifier::Linear,
        },
        Format {
            code: Fourcc::Abgr8888,
            modifier: Modifier::Linear,
        },
    ];
    let mut state = State::forwarding(
        &dh,
        formats,
        scanout.card.0.metadata()?.rdev(),
        u32::from(width),
        u32::from(height),
    )?;
    state.gpu_only = true;
    state.diagnose_surfaces = settings["diagnostic_shm"] == true;
    let listener = ListeningSocket::bind(if external {
        "legion-proton-0"
    } else {
        "legion-kms-0"
    })?;
    let client_log = File::create("/frames/kms-client.log")?;
    let mut client = if external {
        None
    } else {
        Some(OwnedClient(
            Command::new("/usr/bin/vkcube")
                .args([
                    "--wsi",
                    "wayland",
                    "--width",
                    &width.to_string(),
                    "--height",
                    &height.to_string(),
                    "--c",
                    "1000000",
                    "--present_mode",
                    "1",
                ])
                .env("WAYLAND_DISPLAY", "legion-kms-0")
                .env(
                    "LEGION_CUBE_SECONDS",
                    settings["seconds"].as_u64().unwrap_or(15).to_string(),
                )
                .env("LEGION_CUBE_REPORT", "/frames/cube-result.json")
                .env("VK_LOADER_LAYERS_DISABLE", "~implicit~")
                .envs((settings["trace_protocol"] == true).then_some(("WAYLAND_DEBUG", "client")))
                .stdin(Stdio::null())
                .stdout(client_log.try_clone()?)
                .stderr(client_log)
                .spawn()?,
        ))
    };
    let start = Instant::now();
    let mut diagnostic_at = Instant::now();
    let mut input = crate::evdev::Input::open()?;
    let mut input_at = Instant::now();
    let mut clients = Vec::new();
    loop {
        ensure!(
            application
                || start.elapsed() < Duration::from_secs(if external { 60 } else { seconds + 10 }),
            "KMS presentation fixture timed out"
        );
        while let Some(stream) = listener.accept()? {
            clients.push(dh.insert_client(stream, Arc::new(ClientState::default()))?);
        }
        display.dispatch_clients(&mut state)?;
        if input_at.elapsed() >= Duration::from_millis(1) {
            input_at = Instant::now();
            let now = u64::try_from(start.elapsed().as_micros())?;
            state.ordered_input_batch(input.poll(u32::from(width), u32::from(height), now)?, now);
        }
        for (dma, notifier) in std::mem::take(&mut state.pending_imports) {
            match scanout.import(&dma) {
                Ok(_) => {
                    let _ = notifier.successful::<State>();
                    state.imports += 1;
                }
                Err(error) => {
                    notifier.failed();
                    return Err(error);
                }
            }
        }
        if scanout.complete()? {
            state.send_callbacks(u32::try_from(start.elapsed().as_millis())?);
        }
        if scanout.queued.is_none()
            && let Some(buffer) = state.pending.take()
        {
            scanout.submit(buffer)?;
            if scanout.frames == 1 {
                fs::write(
                    "/frames/kms-first-frame.json",
                    b"{\"schema\":1,\"presented\":true}\n",
                )?;
            }
        }
        state.send_callbacks(u32::try_from(start.elapsed().as_millis())?);
        if scanout.current.is_some()
            && let Some(cursor) = &mut scanout.cursor
        {
            cursor.update(
                &scanout.card,
                scanout.crtc,
                &state,
                u32::try_from(start.elapsed().as_millis())?,
            )?;
        }
        display.flush_clients()?;
        if state.diagnose_surfaces && diagnostic_at.elapsed() >= Duration::from_secs(1) {
            diagnostic_at = Instant::now();
            fs::write(
                "/frames/kms-diagnostics.json.tmp",
                serde_json::to_vec(&json!({
                    "schema":1,"surfaces":state.surface_diagnostics(),
                    "completed_scanouts":scanout.frames,"imports":state.imports,
                    "source_commits":state.source_commits,"shm_skipped":state.shm_skipped,
                    "input":state.input_diagnostics()
                    ,"keyboard_events":state.keyboard_events,"pointer_events":state.pointer_events
                }))?,
            )?;
            fs::rename(
                "/frames/kms-diagnostics.json.tmp",
                "/frames/kms-diagnostics.json",
            )?;
        }
        if let Some(status) = client
            .as_mut()
            .map(|child| child.0.try_wait())
            .transpose()?
            .flatten()
        {
            ensure!(
                status.success(),
                "KMS Vulkan client failed: {}",
                fs::read_to_string("/frames/kms-client.log")?
            );
            break;
        }
        if external && std::path::Path::new("/frames/proton-client-exited").is_file() {
            break;
        }
        if !external && start.elapsed() >= Duration::from_secs(seconds) {
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    ensure!(
        application || scanout.frames > 30,
        "KMS fixture displayed too few frames"
    );
    let report = json!({"schema":1,"passed":true,"transport":"virtio_gpu_kms_linear_dmabuf",
        "completed_scanouts":scanout.frames,"imports":state.imports,"width":width,"height":height,
        "elapsed_seconds":start.elapsed().as_secs_f64(),"steady_state_pixel_readbacks":0,
        "desktop_capture_verified":false});
    fs::write(
        "/frames/kms-result.json",
        serde_json::to_vec_pretty(&report)?,
    )?;
    if external {
        let mut forward = report.clone();
        forward["full_frame_cpu_transport"] = false.into();
        fs::write(
            "/frames/proton-forward-result.json",
            serde_json::to_vec_pretty(&forward)?,
        )?;
    }
    // Keep the final scanout owned while the desktop harness captures it.
    std::thread::sleep(Duration::from_secs(2));
    println!("{report}");
    Ok(())
}
