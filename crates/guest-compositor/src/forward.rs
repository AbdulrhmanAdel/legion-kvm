//! Native DMA-BUF forwarding control.
#[path = "forward_proton.rs"]
mod proton;
pub(crate) use proton::run as run_proton;
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use smithay::{
    backend::{
        allocator::{Buffer, Format, Fourcc, Modifier, dmabuf::Dmabuf},
        renderer::{ExportMem, ImportDma, Renderer},
    },
    reexports::wayland_server::{
        Display, ListeningSocket, Resource, protocol::wl_buffer::WlBuffer,
    },
    utils::Rectangle,
    wayland::dmabuf::{ImportNotifier, get_dmabuf},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop, event_created_child,
    protocol::{wl_buffer, wl_callback, wl_compositor, wl_registry, wl_surface},
};
use wayland_protocols::{
    wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_feedback_v1 as feedback,
        zwp_linux_dmabuf_v1 as dmabuf,
    },
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

use crate::{
    OwnedClient,
    state::{ClientState, State},
};

struct ForwardedBuffer {
    source: WlBuffer,
    outer: wl_buffer::WlBuffer,
    busy: bool,
    retired: bool,
}

#[derive(Default)]
struct Host {
    compositor: Option<wl_compositor::WlCompositor>,
    factory: Option<dmabuf::ZwpLinuxDmabufV1>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    shell_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    configured: bool,
    configured_size: Option<(i32, i32)>,
    fullscreen: bool,
    main_device: Option<u64>,
    table: Vec<Option<Format>>,
    indices: BTreeSet<u16>,
    formats: Vec<Format>,
    error: Option<String>,
    closed: bool,
    pending: HashMap<u64, (Dmabuf, ImportNotifier)>,
    buffers: Vec<ForwardedBuffer>,
    next_import: u64,
    imports: u64,
    frames: u64,
    releases: u64,
    frame_callbacks: u64,
    frame_pending: bool,
}

fn table(bytes: &[u8]) -> Result<Vec<Option<Format>>> {
    ensure!(
        bytes.len().is_multiple_of(16),
        "invalid DMA-BUF format table extent"
    );
    bytes
        .as_chunks::<16>()
        .0
        .iter()
        .map(|row| {
            let code = u32::from_ne_bytes(row[..4].try_into()?);
            let modifier = u64::from_ne_bytes(row[8..].try_into()?);
            Ok(Fourcc::try_from(code).ok().map(|code| Format {
                code,
                modifier: Modifier::from(modifier),
            }))
        })
        .collect()
}

fn read_table(file: File, size: u32) -> Result<Vec<Option<Format>>> {
    ensure!(
        size <= 16 * 1024 * 1024,
        "DMA-BUF feedback table exceeds 16 MiB"
    );
    let mut bytes = vec![0; usize::try_from(size)?];
    // SCM_RIGHTS duplicates share an open-file position.
    file.read_exact_at(&mut bytes, 0)?;
    table(&bytes)
}

impl Host {
    fn finish_feedback(&mut self) -> Result<()> {
        let mut formats = Vec::new();
        for index in std::mem::take(&mut self.indices) {
            if let Some(format) = self.table.get(usize::from(index)).with_context(|| {
                format!(
                    "feedback index {index} outside {} table entries",
                    self.table.len()
                )
            })? {
                formats.push(*format);
            }
        }
        ensure!(
            !formats.is_empty(),
            "desktop advertised no usable DMA-BUF formats"
        );
        self.formats = formats;
        Ok(())
    }
    fn import(
        &mut self,
        dma: Dmabuf,
        notifier: ImportNotifier,
        qh: &QueueHandle<Self>,
    ) -> Result<()> {
        ensure!(
            self.buffers.len() + self.pending.len() < 64,
            "forwarding fixture exceeds 64 buffers"
        );
        self.next_import = self
            .next_import
            .checked_add(1)
            .context("import ID overflow")?;
        let parameters = self
            .factory
            .as_ref()
            .context("no DMA-BUF factory")?
            .create_params(qh, self.next_import);
        let modifier = u64::from(dma.format().modifier);
        for (index, ((fd, offset), stride)) in dma
            .handles()
            .zip(dma.offsets())
            .zip(dma.strides())
            .enumerate()
        {
            parameters.add(
                fd,
                u32::try_from(index)?,
                offset,
                stride,
                u32::try_from(modifier >> 32)?,
                u32::try_from(modifier & 0xffff_ffff)?,
            );
        }
        let flags = if dma.y_inverted() {
            params::Flags::YInvert
        } else {
            params::Flags::empty()
        };
        parameters.create(dma.size().w, dma.size().h, dma.format().code as u32, flags);
        self.pending.insert(self.next_import, (dma, notifier));
        Ok(())
    }

    fn submit(&mut self, source: &WlBuffer, qh: &QueueHandle<Self>) -> Result<bool> {
        let Some(buffer) = self
            .buffers
            .iter_mut()
            .find(|buffer| buffer.source == *source)
        else {
            return Ok(false);
        };
        ensure!(
            !buffer.busy && !buffer.retired,
            "application reused a buffer before release"
        );
        let surface = self.surface.as_ref().context("no host surface")?;
        surface.attach(Some(&buffer.outer), 0, 0);
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        if !self.frame_pending {
            surface.frame(qh, ());
            self.frame_pending = true;
        }
        buffer.busy = true;
        surface.commit();
        self.frames += 1;
        Ok(true)
    }

    fn retire(&mut self, source: &WlBuffer) {
        for buffer in &mut self.buffers {
            if buffer.source == *source {
                buffer.retired = true;
            }
        }
        self.buffers.retain(|buffer| {
            if buffer.retired && !buffer.busy {
                buffer.outer.destroy();
                false
            } else {
                true
            }
        });
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Host {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" if version >= 4 => {
                    state.compositor = Some(registry.bind(name, 4, qh, ()))
                }
                "xdg_wm_base" => state.wm = Some(registry.bind(name, 1, qh, ())),
                "zwp_linux_dmabuf_v1" if version >= 4 => {
                    let factory: dmabuf::ZwpLinuxDmabufV1 = registry.bind(name, 4, qh, ());
                    factory.get_default_feedback(qh, ());
                    state.factory = Some(factory);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<feedback::ZwpLinuxDmabufFeedbackV1, ()> for Host {
    fn event(
        state: &mut Self,
        _: &feedback::ZwpLinuxDmabufFeedbackV1,
        event: feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let outcome = (|| -> Result<()> {
            match event {
                feedback::Event::FormatTable { fd, size } => {
                    state.table = read_table(File::from(fd), size)?;
                }
                feedback::Event::MainDevice { device } => {
                    state.main_device = Some(u64::from_ne_bytes(device.as_slice().try_into()?))
                }
                feedback::Event::TrancheFormats { indices } => {
                    ensure!(indices.len().is_multiple_of(2), "invalid tranche indices");
                    for index in indices.as_chunks::<2>().0 {
                        state.indices.insert(u16::from_ne_bytes(*index));
                    }
                }
                feedback::Event::Done => {
                    state.finish_feedback()?;
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            state.error = Some(format!("{error:#}"));
        }
    }
}

impl Dispatch<params::ZwpLinuxBufferParamsV1, u64> for Host {
    fn event(
        state: &mut Self,
        parameters: &params::ZwpLinuxBufferParamsV1,
        event: params::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            params::Event::Created { buffer } => {
                if let Some((_dma, notifier)) = state.pending.remove(id) {
                    match notifier.successful::<State>() {
                        Ok(source) => {
                            state.buffers.push(ForwardedBuffer {
                                source,
                                outer: buffer,
                                busy: false,
                                retired: false,
                            });
                            state.imports += 1;
                        }
                        Err(_) => buffer.destroy(),
                    }
                } else {
                    buffer.destroy();
                    state.error = Some("unexpected host buffer creation".into());
                }
                parameters.destroy();
            }
            params::Event::Failed => {
                if let Some((_dma, notifier)) = state.pending.remove(id) {
                    notifier.failed();
                }
                state.error = Some("desktop rejected application DMA-BUF".into());
                parameters.destroy();
            }
            _ => {}
        }
    }
    event_created_child!(Host, params::ZwpLinuxBufferParamsV1, [0 => (wl_buffer::WlBuffer, ())]);
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Host {
    fn event(
        state: &mut Self,
        object: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(buffer) = state
                .buffers
                .iter_mut()
                .find(|buffer| buffer.outer == *object)
                && buffer.busy
            {
                if buffer.source.is_alive() {
                    buffer.source.release();
                }
                buffer.busy = false;
                state.releases += 1;
            }
            state.buffers.retain(|buffer| {
                if buffer.retired && !buffer.busy {
                    buffer.outer.destroy();
                    false
                } else {
                    true
                }
            });
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Host {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.frame_pending = false;
        state.frame_callbacks += 1;
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Host {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for Host {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Host {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Close => state.closed = true,
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
                ..
            } => {
                if width > 0 && height > 0 {
                    state.configured_size = Some((width, height));
                }
                state.fullscreen = states.as_chunks::<4>().0.iter().any(|bytes| {
                    u32::from_ne_bytes(*bytes) == xdg_toplevel::State::Fullscreen as u32
                });
            }
            _ => {}
        }
    }
}
delegate_noop!(Host: ignore wl_compositor::WlCompositor);
delegate_noop!(Host: ignore wl_surface::WlSurface);
delegate_noop!(Host: ignore dmabuf::ZwpLinuxDmabufV1);

fn pump(queue: &mut EventQueue<Host>, host: &mut Host) -> Result<()> {
    queue.dispatch_pending(host)?;
    if let Some(guard) = queue.prepare_read() {
        match guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
    }
    queue.dispatch_pending(host)?;
    match queue.flush() {
        Ok(()) => {}
        Err(wayland_client::backend::WaylandError::Io(error))
            if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(error) = host.error.take() {
        bail!("host Wayland: {error}");
    }
    Ok(())
}

fn preview(buffer: &WlBuffer, output: &Path, name: &str) -> Result<u64> {
    preview_image(buffer, output, name, true)
}

fn preview_image(
    buffer: &WlBuffer,
    output: &Path,
    name: &str,
    require_nonuniform: bool,
) -> Result<u64> {
    let dma = get_dmabuf(buffer).context("preview requires DMA-BUF")?;
    let (mut renderer, _) = crate::renderer()?;
    let texture = renderer.import_dmabuf(dma, None)?;
    let mapping =
        renderer.copy_texture(&texture, Rectangle::from_size(dma.size()), Fourcc::Abgr8888)?;
    let bytes = renderer.map_texture(&mapping)?;
    let first = bytes.get(..4).context("empty preview")?;
    ensure!(
        !require_nonuniform
            || bytes
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel.as_slice() != first),
        "forwarded image is uniform"
    );
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    fs::write(output.join(name), bytes)?;
    drop(mapping);
    drop(texture);
    renderer.cleanup_texture_cache()?;
    Ok(hasher.finish())
}

fn desktop(width: i32, height: i32) -> Result<(Connection, EventQueue<Host>, Host)> {
    desktop_with_fullscreen(width, height, false)
}

fn desktop_with_fullscreen(
    width: i32,
    height: i32,
    fullscreen: bool,
) -> Result<(Connection, EventQueue<Host>, Host)> {
    let connection =
        Connection::connect_to_env().context("connect to real host Wayland desktop")?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut host = Host::default();
    queue.roundtrip(&mut host)?;
    queue.roundtrip(&mut host)?;
    ensure!(host.error.is_none(), "host feedback: {:?}", host.error);
    ensure!(
        !host.formats.is_empty(),
        "host requires linux-dmabuf feedback v4"
    );
    let surface = host
        .compositor
        .as_ref()
        .context("host requires wl_compositor v4")?
        .create_surface(&qh, ());
    let shell_surface = host
        .wm
        .as_ref()
        .context("host requires xdg-shell")?
        .get_xdg_surface(&surface, &qh, ());
    let toplevel = shell_surface.get_toplevel(&qh, ());
    toplevel.set_title("Legion — display check".into());
    toplevel.set_app_id("legion-dmabuf-forward".into());
    toplevel.set_min_size(width, height);
    toplevel.set_max_size(width, height);
    if fullscreen {
        toplevel.set_fullscreen(None);
    }
    shell_surface.set_window_geometry(0, 0, width, height);
    host.shell_surface = Some(shell_surface);
    host.toplevel = Some(toplevel);
    surface.commit();
    host.surface = Some(surface);
    queue.roundtrip(&mut host)?;
    ensure!(host.configured, "host window was not configured");
    Ok((connection, queue, host))
}

pub fn run(args: &[OsString]) -> Result<()> {
    ensure!(
        matches!(args.len(), 6 | 7),
        "expected --native-forward <frames> <userspace> <scratch> <width> <height> <seconds> [acquire-barrier]"
    );
    let frames = PathBuf::from(&args[0]);
    let userspace = PathBuf::from(&args[1]);
    let scratch = PathBuf::from(&args[2]);
    let width: i32 = args[3].to_str().context("invalid width")?.parse()?;
    let height: i32 = args[4].to_str().context("invalid height")?.parse()?;
    let seconds: u64 = args[5].to_str().context("invalid duration")?.parse()?;
    let mut acquirer = args
        .get(6)
        .map(|value| -> Result<bool> {
            Ok(value.to_str().context("invalid acquire barrier")?.parse()?)
        })
        .transpose()?
        .unwrap_or(false)
        .then(crate::native_acquire::Acquirer::new)
        .transpose()?;
    ensure!(
        (1..=3840).contains(&width) && (1..=2160).contains(&height) && (15..=30).contains(&seconds),
        "invalid forwarding dimensions/duration"
    );
    let (_connection, mut queue, mut host) = desktop(width, height)?;
    let qh = queue.handle();
    let device = host
        .main_device
        .context("host omitted DMA-BUF main device")?;
    let surface = host
        .surface
        .clone()
        .context("missing configured host surface")?;
    let mut display = Display::<State>::new()?;
    let mut dh = display.handle();
    let mut state = State::forwarding(
        &dh,
        host.formats.clone(),
        device,
        u32::try_from(width)?,
        u32::try_from(height)?,
    )?;
    let socket_path = scratch.join("legion-forward-0");
    let listener = ListeningSocket::bind_absolute(socket_path.clone())?;
    let metrics_dir = scratch.join("legion-benchmark");
    fs::create_dir(&metrics_dir)?;
    let log_path = scratch.join("vkcube.log");
    let log = File::create(&log_path)?;
    let mut client = OwnedClient(Command::new(userspace.join("lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"))
        .arg("--library-path").arg(std::env::join_paths([userspace.join("lib"), userspace.join("lib/x86_64-linux-gnu")])?)
        .arg(userspace.join("bin/vkcube"))
        .args(["--wsi", "wayland", "--width", &width.to_string(), "--height", &height.to_string(), "--present_mode", "1", "--c", "1000000"])
        .env("XDG_RUNTIME_DIR", &scratch).env("WAYLAND_DISPLAY", &socket_path)
        .env("MANGOHUD", "1")
        .env("MANGOHUD_CONFIG", format!("no_display,control=legion-forward-%p,log_duration=10,log_interval=0,output_folder={},fps_limit=0,cpu_stats=0,gpu_stats=0", metrics_dir.display()))
        .stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?);
    let start = Instant::now();
    let mut clients = Vec::new();
    let mut logger = None;
    let mut logging_started = None;
    let mut first_hash = None;
    let mut last_hash = None;
    loop {
        ensure!(
            !host.closed,
            "host window closed before benchmark completion"
        );
        ensure!(
            start.elapsed() < Duration::from_secs(60),
            "native forwarding timed out"
        );
        pump(&mut queue, &mut host)?;
        while let Some(stream) = listener.accept()? {
            clients.push(dh.insert_client(stream, Arc::new(ClientState::default()))?);
        }
        display.dispatch_clients(&mut state)?;
        if let Some(error) = state.error.take() {
            bail!("source Wayland: {error}");
        }
        for (dma, notifier) in state.pending_imports.drain(..) {
            host.import(dma, notifier, &qh)?;
        }
        for buffer in state.destroyed_buffers.drain(..) {
            host.retire(&buffer);
        }
        let finishing = logging_started
            .is_some_and(|at: Instant| at.elapsed() >= Duration::from_secs(seconds - 2));
        let next = if let Some(acquirer) = acquirer.as_mut() {
            if !finishing
                && !acquirer.is_full()
                && let Some(buffer) = state.pending.take()
            {
                acquirer.push(buffer)?;
            }
            acquirer.pop_ready()?
        } else {
            state.pending.take()
        };
        if let Some(buffer) = next {
            let dma = get_dmabuf(&buffer).context("application supplied non-DMA-BUF")?;
            ensure!(
                dma.size().w == width && dma.size().h == height,
                "application render size changed"
            );
            if host.submit(&buffer, &qh)? {
                // Two diagnostic reads only: before logging and after its full ten-second window.
                if first_hash.is_none() {
                    first_hash = Some(preview(&buffer, &frames, "forward-first.rgba")?);
                }
                if logging_started.is_some_and(|at: Instant| at.elapsed() > Duration::from_secs(11))
                    && last_hash.is_none()
                {
                    last_hash = Some(preview(&buffer, &frames, "forward-last.rgba")?);
                }
            } else {
                state.pending = Some(buffer);
            }
        }
        state.send_callbacks(u32::try_from(start.elapsed().as_millis())?);
        display.flush_clients()?;
        if logger.is_none() && first_hash.is_some() && start.elapsed() > Duration::from_secs(2) {
            let address =
                SocketAddr::from_abstract_name(format!("legion-forward-{}", client.0.id()))?;
            if let Ok(mut socket) = UnixStream::connect_addr(&address) {
                socket.write_all(b":logging=1;")?;
                logger = Some(socket);
                logging_started = Some(Instant::now());
            }
        }
        ensure!(
            logger.is_some() || start.elapsed() < Duration::from_secs(8),
            "forward logger did not start"
        );
        if let Some(status) = client.0.try_wait()? {
            bail!(
                "vkcube exited early {status}: {}",
                fs::read_to_string(&log_path)?
            );
        }
        if finishing && acquirer.as_ref().is_none_or(|acquirer| acquirer.is_empty()) {
            break;
        }
        std::thread::yield_now();
    }
    ensure!(
        host.frames > 0 && host.releases > 0 && host.frame_callbacks > 0,
        "host omitted frame/release callbacks"
    );
    ensure!(
        last_hash.is_some() && first_hash != last_hash,
        "forwarded application did not animate"
    );
    ensure!(
        host.configured_size
            .is_none_or(|size| size == (width, height)),
        "host window size differs: {:?}",
        host.configured_size
    );
    surface.attach(None, 0, 0);
    surface.commit();
    state.close_clients();
    display.flush_clients()?;
    let end = Instant::now();
    while client.0.try_wait()?.is_none() && end.elapsed() < Duration::from_secs(1) {
        pump(&mut queue, &mut host)?;
        display.dispatch_clients(&mut state)?;
        display.flush_clients()?;
        std::thread::sleep(Duration::from_millis(5));
    }
    for entry in fs::read_dir(&metrics_dir)? {
        let entry = entry?;
        ensure!(
            entry.metadata()?.len() <= 64 * 1024 * 1024,
            "benchmark log exceeds 64 MiB"
        );
        fs::copy(entry.path(), frames.join(entry.file_name()))?;
    }
    let acquisition = acquirer
        .as_ref()
        .map(crate::native_acquire::Acquirer::report);
    println!(
        "{}",
        json!({"schema":1,"passed":true,"mode":"native_dmabuf_forward","virtual_machine":false,"width":width,"height":height,"configured_size":host.configured_size,"host_main_device":device,"advertised_formats":host.formats.len(),"buffer_imports":host.imports,"frames_forwarded":host.frames,"host_buffer_releases":host.releases,"host_frame_callbacks":host.frame_callbacks,"elapsed_seconds":start.elapsed().as_secs_f64(),"cpu_pixel_transport":false,"steady_state_readbacks":0,"diagnostic_readbacks":2,"animation_verified":true,"native_implicit_sync":true,"present_mode":"mailbox","uncapped":true,"acquisition":acquisition,"client_log":fs::read_to_string(log_path)?})
    );
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/forward.rs"]
mod tests;
