//! Host-side GPU presentation.
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use anyhow::{Context, Result, bail, ensure};
use legion_display_channel::{Input, desktop_input::DesktopInput};
mod cursor;
mod input_events;
use rustix::event::{PollFd, PollFlags, poll};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop, event_created_child,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat,
        wl_shm, wl_surface,
    },
};
use wayland_protocols::{
    wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_feedback_v1 as feedback,
        zwp_linux_dmabuf_v1 as dmabuf,
    },
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub fourcc: u32,
    pub modifier: u64,
}
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub main_device: u64,
    pub formats: Vec<Format>,
}
pub struct Plane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}
pub struct Image {
    pub id: u64,
    pub width: i32,
    pub height: i32,
    pub format: Format,
    pub y_inverted: bool,
    pub planes: Vec<Plane>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    pub id: u64,
    pub frame: u64,
}
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub imports: u64,
    pub commits: u64,
    pub releases: u64,
    pub callbacks: u64,
    pub fence_waits: u64,
}
#[derive(Debug, Default, PartialEq, Eq)]
enum Lease {
    #[default]
    Idle,
    Submitted(u64),
    ConsumerReleased(u64),
}
impl Lease {
    fn begin(&mut self, frame: u64) -> Result<()> {
        ensure!(
            *self == Self::Idle && frame > 0,
            "image still leased or invalid frame"
        );
        *self = Self::Submitted(frame);
        Ok(())
    }
    fn consumer_release(&mut self) {
        if let Self::Submitted(frame) = *self {
            *self = Self::ConsumerReleased(frame);
        }
    }
    fn complete(&mut self, ready: bool) -> Option<u64> {
        if ready && let Self::ConsumerReleased(frame) = *self {
            *self = Self::Idle;
            Some(frame)
        } else {
            None
        }
    }
}
struct Cached {
    image: Image,
    buffer: wl_buffer::WlBuffer,
    lease: Lease,
    retired: bool,
}

fn planes_complete(planes: &[Plane]) -> Result<bool> {
    let mut fds = planes
        .iter()
        .map(|p| PollFd::new(&p.fd, PollFlags::OUT))
        .collect::<Vec<_>>();
    poll(
        &mut fds,
        Some(&rustix::time::Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }),
    )?;
    ensure!(
        !fds.iter().any(|fd| fd
            .revents()
            .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)),
        "DMA-BUF completion poll failed"
    );
    Ok(fds.iter().all(|fd| fd.revents().contains(PollFlags::OUT)))
}
#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    cursor: Option<cursor::DesktopCursor>,
    factory: Option<dmabuf::ZwpLinuxDmabufV1>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    shell: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    input: Option<DesktopInput>,
    events: input_events::Capture,
    device: Option<u64>,
    table: Vec<Format>,
    indices: BTreeSet<u16>,
    formats: Vec<Format>,
    configured: bool,
    configured_size: Option<(i32, i32)>,
    fullscreen: bool,
    error: Option<String>,
    pending: HashMap<u64, Image>,
    images: HashMap<u64, Cached>,
    released: VecDeque<Release>,
    frame_pending: bool,
    current: Option<u64>,
    stats: Stats,
}
pub struct Presenter {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    width: i32,
    height: i32,
    input_test: bool,
    input_test_start: Option<std::time::Instant>,
    input_test_stage: u8,
    fullscreen_retry_pending: bool,
}

impl Presenter {
    /// Connect to the caller's real Wayland desktop and create a fixed-size window. # Errors Requires linux-dmabuf feedback v4 and a responsive desktop compositor.
    pub fn connect(width: i32, height: i32) -> Result<Self> {
        Self::connect_with_fullscreen(width, height, false)
    }

    /// Create a window, optionally requesting genuine desktop fullscreen. # Errors Requires the same desktop interfaces as `connect`.
    pub fn connect_with_fullscreen(width: i32, height: i32, fullscreen: bool) -> Result<Self> {
        ensure!(
            (1..=3840).contains(&width) && (1..=2160).contains(&height),
            "invalid display size"
        );
        let connection = Connection::connect_to_env().context("connect host Wayland display")?;
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        connection.display().get_registry(&qh, ());
        let mut state = State {
            input: Some(DesktopInput {
                input: Input::new(0, 0, false, 0)?,
                closed: false,
                width: width as u32,
                height: height as u32,
            }),
            ..State::default()
        };
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        state.initialize_cursor(&qh)?;
        ensure!(state.error.is_none(), "display feedback: {:?}", state.error);
        ensure!(
            !state.formats.is_empty() && state.device.is_some(),
            "host requires DMA-BUF v4 feedback"
        );
        let surface = state
            .compositor
            .as_ref()
            .context("host requires wl_compositor v4")?
            .create_surface(&qh, ());
        let shell = state
            .wm
            .as_ref()
            .context("host requires xdg-shell")?
            .get_xdg_surface(&surface, &qh, ());
        let top = shell.get_toplevel(&qh, ());
        top.set_title("Legion v2 — guest GPU forwarding".into());
        top.set_app_id("legion-vm-forward".into());
        top.set_min_size(width, height);
        top.set_max_size(width, height);
        if fullscreen {
            top.set_fullscreen(None);
        }
        // The initial xdg-shell commit deliberately has no buffer.
        surface.commit();
        state.surface = Some(surface);
        state.shell = Some(shell);
        state.toplevel = Some(top);
        queue.roundtrip(&mut state)?;
        ensure!(state.configured, "host window not configured");
        Ok(Self {
            connection,
            queue,
            state,
            width,
            height,
            input_test: std::env::var_os("LEGION_DESKTOP_INPUT_TEST").is_some_and(|v| v == "1"),
            input_test_start: None,
            input_test_stage: 0,
            fullscreen_retry_pending: fullscreen,
        })
    }
    /// Size selected by the actual desktop compositor, when specified.
    pub fn configured_size(&self) -> Option<(i32, i32)> {
        self.state.configured_size
    }

    /// Last fullscreen state acknowledged by the desktop, after pumping events.
    pub fn is_fullscreen(&self) -> bool {
        self.state.fullscreen
    }

    pub fn info(&self) -> Result<DeviceInfo> {
        Ok(DeviceInfo {
            main_device: self.state.device.context("missing main device")?,
            formats: self.state.formats.clone(),
        })
    }

    /// Descriptor used by the backend's existing event pump.
    pub fn event_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.connection.as_fd()
    }

    pub fn has_releases(&self) -> bool {
        !self.state.released.is_empty()
    }
    /// Snapshot only this owned window's pointer/Space/close state.
    pub fn input(&self) -> Result<DesktopInput> {
        self.state.input.context("desktop input is not initialized")
    }
    /// Drain ordered input received only for this presenter's Wayland window.
    pub fn input_events(&mut self, bytes: &mut [u8]) -> Result<()> {
        let mode = legion_display_channel::input_events::Batch::capture_request(bytes)?;
        self.state.update_capture(mode, &self.queue.handle());
        let snapshot = self.input()?;
        let mut batch = self.state.events.batch(snapshot);
        batch.relative_supported =
            self.state.events.relative.is_some() && self.state.events.constraints.is_some();
        batch.encode(bytes)
    }
    /// Fetch a bounded chunk of the current host XKB map.
    pub fn input_keymap(&self, bytes: &mut [u8]) -> Result<()> {
        legion_display_channel::input_events::keymap_reply(
            &self.state.events.keymap,
            self.state.events.keymap_generation,
            bytes,
        )
    }
    /// Import host-owned descriptors once per allocation.
    pub fn register(&mut self, image: Image) -> Result<()> {
        ensure!(
            (1..=3840).contains(&image.width) && (1..=2160).contains(&image.height),
            "image dimensions exceed presentation bounds"
        );
        ensure!(
            (1..=4).contains(&image.planes.len()) && image.planes.iter().all(|p| p.stride > 0),
            "invalid image planes"
        );
        ensure!(
            self.state.images.len() + self.state.pending.len() < 64,
            "image registry full"
        );
        ensure!(
            !self.state.images.contains_key(&image.id)
                && !self.state.pending.contains_key(&image.id),
            "duplicate image ID"
        );
        ensure!(
            self.state.formats.contains(&image.format),
            "unadvertised image layout"
        );
        let id = image.id;
        let qh = self.queue.handle();
        let p = self
            .state
            .factory
            .as_ref()
            .context("missing DMA-BUF factory")?
            .create_params(&qh, id);
        for (index, plane) in image.planes.iter().enumerate() {
            p.add(
                plane.fd.as_fd(),
                u32::try_from(index)?,
                plane.offset,
                plane.stride,
                u32::try_from(image.format.modifier >> 32)?,
                u32::try_from(image.format.modifier & 0xffff_ffff)?,
            );
        }
        p.create(
            image.width,
            image.height,
            image.format.fourcc,
            if image.y_inverted {
                params::Flags::YInvert
            } else {
                params::Flags::empty()
            },
        );
        self.state.pending.insert(id, image);
        self.queue.roundtrip(&mut self.state)?;
        self.check()?;
        ensure!(
            self.state.images.contains_key(&id),
            "host omitted image import result"
        );
        Ok(())
    }
    /// Present a completed guest image.
    pub fn present(&mut self, id: u64, frame: u64) -> Result<()> {
        self.pump()?;
        let image = self
            .state
            .images
            .get_mut(&id)
            .context("unknown presentation image")?;
        if (image.image.width, image.image.height) != (self.width, self.height) {
            let (width, height) = (image.image.width, image.image.height);
            if let Some(top) = &self.state.toplevel {
                top.set_min_size(width, height);
                top.set_max_size(width, height);
            }
            if let Some(shell) = &self.state.shell {
                shell.set_window_geometry(0, 0, width, height);
            }
            self.width = width;
            self.height = height;
            if let Some(input) = &mut self.state.input {
                input.width = width as u32;
                input.height = height as u32;
            }
        }
        ensure!(!image.retired, "image retired");
        image.lease.begin(frame)?;
        let surface = self.state.surface.as_ref().context("missing surface")?;
        surface.attach(Some(&image.buffer), 0, 0);
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        if !self.state.frame_pending {
            surface.frame(&self.queue.handle(), ());
            self.state.frame_pending = true;
        }
        surface.commit();
        // Some desktops discard fullscreen requests made before the window maps.
        if self.fullscreen_retry_pending && self.state.stats.callbacks > 0 {
            self.fullscreen_retry_pending = false;
            if !self.state.fullscreen {
                self.state
                    .toplevel
                    .as_ref()
                    .context("missing toplevel")?
                    .set_fullscreen(None);
            }
        }
        self.state.current = Some(id);
        self.state.stats.commits += 1;
        self.input_test_start
            .get_or_insert_with(std::time::Instant::now);
        self.flush()?;
        Ok(())
    }
    pub fn retire(&mut self, id: u64) -> Result<()> {
        self.state
            .images
            .get_mut(&id)
            .context("unknown retirement image")?
            .retired = true;
        if self.state.current == Some(id) {
            if let Some(surface) = &self.state.surface {
                surface.attach(None, 0, 0);
                surface.commit();
            }
            self.state.current = None;
            self.flush()?;
        }
        self.collect_completed()?;
        Ok(())
    }
    /// Process desktop events and complete releases only after host DMA-BUF fences. # Errors Reports disconnection, import errors and GPU poll errors.
    pub fn pump(&mut self) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        if self.input_test
            && let Some(start) = self.input_test_start
        {
            let stage = if start.elapsed().as_secs() >= 14 {
                4
            } else {
                (start.elapsed().as_secs() / 2).min(3) as u8
            };
            if stage > self.input_test_stage {
                // Opt-in test injection remains inside this owned surface's snapshot.
                self.state.update_input(
                    u16::from(stage) * 16000,
                    u16::from(stage) * 12000,
                    stage == 2,
                );
                self.input_test_stage = stage;
                if stage == 4
                    && let Some(snapshot) = &mut self.state.input
                {
                    snapshot.closed = true;
                }
            }
        }
        if let Some(read) = self.queue.prepare_read() {
            match read.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        self.check()?;
        self.collect_completed()?;
        self.flush()
    }
    fn flush(&self) -> Result<()> {
        match self.queue.flush() {
            Ok(()) => Ok(()),
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }
    fn check(&mut self) -> Result<()> {
        if let Some(error) = self.state.error.take() {
            bail!("host presentation: {error}");
        }
        Ok(())
    }
    fn collect_completed(&mut self) -> Result<()> {
        for (&id, image) in &mut self.state.images {
            if matches!(image.lease, Lease::ConsumerReleased(_)) {
                // POLLOUT on a real host DMA-BUF waits for all readers and writers in its reservation.
                let ready = planes_complete(&image.image.planes)?;
                if ready {
                    ensure!(
                        self.state.released.len() < 64,
                        "unconsumed release queue full"
                    );
                    let frame = image
                        .lease
                        .complete(true)
                        .context("invalid completed lease")?;
                    self.state.released.push_back(Release { id, frame });
                    self.state.stats.releases += 1;
                } else {
                    self.state.stats.fence_waits += 1;
                }
            }
        }
        self.state.images.retain(|_, image| {
            if image.retired && image.lease == Lease::Idle {
                image.buffer.destroy();
                false
            } else {
                true
            }
        });
        Ok(())
    }
    pub fn pop_release(&mut self) -> Option<Release> {
        self.state.released.pop_front()
    }
    pub fn stats(&self) -> Stats {
        self.state.stats
    }
}

impl State {
    fn update_input(&mut self, x: u16, y: u16, space: bool) {
        let Some(snapshot) = &mut self.input else {
            return;
        };
        if (
            snapshot.input.x(),
            snapshot.input.y(),
            snapshot.input.space(),
        ) == (x, y, space)
        {
            return;
        }
        match Input::new(x, y, space, snapshot.input.generation() + 1) {
            Ok(input) => snapshot.input = input,
            Err(error) => self.error = Some(error.to_string()),
        }
    }
    fn pointer_position(&mut self, x: f64, y: f64) {
        if let Some(snapshot) = self.input {
            let coordinate = |value: f64, extent: u32| -> u16 {
                (value / f64::from(extent) * 65535.0).clamp(0.0, 65535.0) as u16
            };
            self.update_input(
                coordinate(x, snapshot.width),
                coordinate(y, snapshot.height),
                snapshot.input.space(),
            );
        }
    }
    fn space(&mut self, down: bool) {
        if let Some(snapshot) = self.input {
            self.update_input(snapshot.input.x(), snapshot.input.y(), down);
        }
    }
}

use std::os::fd::AsFd;
impl Dispatch<wl_registry::WlRegistry, ()> for State {
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
                "wl_seat" => state.seat = Some(registry.bind(name, version.min(7), qh, ())),
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwp_relative_pointer_manager_v1" => {
                    state.events.relative_manager = Some(registry.bind(name, 1, qh, ()));
                    state.initialize_relative(qh);
                }
                "zwp_pointer_constraints_v1" => {
                    state.events.constraints = Some(registry.bind(name, 1, qh, ()));
                }
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
impl Dispatch<feedback::ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &feedback::ZwpLinuxDmabufFeedbackV1,
        event: feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let result = (|| -> Result<()> {
            match event {
                feedback::Event::FormatTable { fd, size } => {
                    ensure!(
                        size <= 16 * 1024 * 1024 && size % 16 == 0,
                        "invalid format table"
                    );
                    let mut bytes = vec![0; usize::try_from(size)?];
                    File::from(fd).read_exact_at(&mut bytes, 0)?;
                    state.table = bytes
                        .as_chunks::<16>()
                        .0
                        .iter()
                        .map(|row| {
                            Ok(Format {
                                fourcc: u32::from_ne_bytes(row[..4].try_into()?),
                                modifier: u64::from_ne_bytes(row[8..].try_into()?),
                            })
                        })
                        .collect::<Result<_>>()?;
                }
                feedback::Event::MainDevice { device } => {
                    state.device = Some(u64::from_ne_bytes(device.as_slice().try_into()?))
                }
                feedback::Event::TrancheFormats { indices } => {
                    ensure!(indices.len() % 2 == 0, "invalid feedback indices");
                    for index in indices.as_chunks::<2>().0 {
                        state.indices.insert(u16::from_ne_bytes(*index));
                    }
                }
                feedback::Event::Done => {
                    state.formats = std::mem::take(&mut state.indices)
                        .into_iter()
                        .map(|index| {
                            state
                                .table
                                .get(usize::from(index))
                                .copied()
                                .context("invalid format index")
                        })
                        .collect::<Result<_>>()?;
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = result {
            state.error = Some(format!("{error:#}"));
        }
    }
}
impl Dispatch<params::ZwpLinuxBufferParamsV1, u64> for State {
    fn event(
        state: &mut Self,
        p: &params::ZwpLinuxBufferParamsV1,
        event: params::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            params::Event::Created { buffer } => {
                if let Some(image) = state.pending.remove(id) {
                    state.images.insert(
                        *id,
                        Cached {
                            image,
                            buffer,
                            lease: Lease::Idle,
                            retired: false,
                        },
                    );
                    state.stats.imports += 1;
                } else {
                    buffer.destroy();
                    state.error = Some("unexpected import completion".into());
                }
                p.destroy();
            }
            params::Event::Failed => {
                state.pending.remove(id);
                state.error = Some("host rejected GPU image".into());
                p.destroy();
            }
            _ => {}
        }
    }
    event_created_child!(State, params::ZwpLinuxBufferParamsV1,[0=>(wl_buffer::WlBuffer,())]);
}
impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(image) = state
                .images
                .values_mut()
                .find(|image| image.buffer == *buffer)
        {
            image.lease.consumer_release();
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.frame_pending = false;
        state.stats.callbacks += 1;
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
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
impl Dispatch<xdg_surface::XdgSurface, ()> for State {
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
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Close => {
                if let Some(input) = &mut state.input {
                    input.closed = true;
                }
            }
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
impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
                state.initialize_relative(qh);
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if !caps.contains(wl_seat::Capability::Keyboard) {
                state.space(false);
                state.events.focus(true, false);
                state.update_capture(legion_display_channel::input_events::CaptureMode::None, qh);
                if let Some(keyboard) = state.keyboard.take() {
                    keyboard.release();
                }
            }
            if !caps.contains(wl_seat::Capability::Pointer) {
                state.update_capture(legion_display_channel::input_events::CaptureMode::None, qh);
                state.events.focus(false, false);
                if let Some(relative) = state.events.relative.take() {
                    relative.destroy();
                }
                if let Some(pointer) = state.pointer.take() {
                    pointer.release();
                }
            }
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
                ..
            } if state.surface.as_ref() == Some(&surface) => {
                state.events.pointer_serial = Some(serial);
                state.update_cursor();
                state.events.focus(false, true);
                state.event_pointer_position(surface_x, surface_y);
                state.pointer_position(surface_x, surface_y)
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.event_pointer_position(surface_x, surface_y);
                state.pointer_position(surface_x, surface_y);
            }
            wl_pointer::Event::Leave { .. } => {
                state.events.pointer_serial = None;
                state.events.focus(false, false);
            }
            wl_pointer::Event::Button {
                button,
                state: wayland_client::WEnum::Value(button_state),
                ..
            } => {
                state
                    .events
                    .button(button, button_state == wl_pointer::ButtonState::Pressed);
            }
            wl_pointer::Event::Axis {
                axis: wayland_client::WEnum::Value(axis),
                value,
                ..
            } => {
                state.events.wheel[axis as usize].value += value;
                state.events.wheel[axis as usize].changed = true;
            }
            wl_pointer::Event::AxisDiscrete {
                axis: wayland_client::WEnum::Value(axis),
                discrete,
            } => {
                state.events.wheel[axis as usize].v120 = discrete.saturating_mul(120);
                state.events.wheel[axis as usize].changed = true;
            }
            wl_pointer::Event::AxisValue120 {
                axis: wayland_client::WEnum::Value(axis),
                value120,
            } => {
                state.events.wheel[axis as usize].v120 = value120;
                state.events.wheel[axis as usize].changed = true;
            }
            wl_pointer::Event::AxisStop {
                axis: wayland_client::WEnum::Value(axis),
                ..
            } => {
                state.events.wheel[axis as usize].stop = true;
                state.events.wheel[axis as usize].changed = true;
            }
            wl_pointer::Event::AxisSource {
                axis_source: wayland_client::WEnum::Value(source),
            } => state.events.axis_source = source as u32,
            wl_pointer::Event::Frame => state.events.flush_wheel(),
            _ => {}
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: wayland_client::WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                if let Err(error) = state.events.keymap(fd, size) {
                    state.error = Some(format!("host keymap: {error:#}"));
                }
            }
            wl_keyboard::Event::Enter { surface, keys, .. }
                if state.surface.as_ref() == Some(&surface) =>
            {
                state.events.enter_keys(&keys);
                state.space(
                    keys.as_chunks::<4>()
                        .0
                        .iter()
                        .any(|key| u32::from_ne_bytes(*key) == 57),
                );
            }
            wl_keyboard::Event::Key {
                key: code,
                state: wayland_client::WEnum::Value(key),
                ..
            } => {
                if code == 57 {
                    state.space(key == wl_keyboard::KeyState::Pressed);
                }
                state
                    .events
                    .key(code, key == wl_keyboard::KeyState::Pressed);
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => state
                .events
                .modifiers(mods_depressed, mods_latched, mods_locked, group),
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state
                    .events
                    .push(legion_display_channel::input_events::Event::Repeat {
                        rate: rate.clamp(0, 1000) as u32,
                        delay: delay.clamp(0, 60_000) as u32,
                    })
            }
            wl_keyboard::Event::Leave { .. } => {
                state.space(false);
                state.events.focus(true, false);
                state.update_capture(legion_display_channel::input_events::CaptureMode::None, qh);
            }
            _ => {}
        }
    }
}
delegate_noop!(State:ignore wl_compositor::WlCompositor);
delegate_noop!(State:ignore wl_shm::WlShm);
delegate_noop!(State:ignore wl_surface::WlSurface);
delegate_noop!(State:ignore dmabuf::ZwpLinuxDmabufV1);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_snapshot_tracks_coordinates_and_actual_key_state_without_repeats() {
        let mut state = State {
            input: Some(DesktopInput {
                input: Input::new(0, 0, false, 0).expect("initial input"),
                closed: false,
                width: 1280,
                height: 720,
            }),
            ..State::default()
        };
        state.pointer_position(640.0, 360.0);
        assert_eq!(state.input.expect("input").input.x(), 32767);
        state.space(true);
        let pressed = state.input.expect("input").input;
        state.space(true);
        assert_eq!(state.input.expect("input").input, pressed);
        state.space(false);
        assert!(!state.input.expect("input").input.space());
        assert_eq!(
            state.input.expect("input").input.generation(),
            pressed.generation() + 1
        );
        state.pointer_position(-2.0, 2000.0);
        assert_eq!(
            (
                state.input.expect("input").input.x(),
                state.input.expect("input").input.y()
            ),
            (0, 65535)
        );
    }
    #[test]
    fn lease_requires_consumer_release_and_gpu_completion() {
        let mut lease = Lease::Idle;
        lease.begin(1).expect("initial frame");
        assert_eq!(lease.complete(true), None);
        assert!(lease.begin(2).is_err());
        lease.consumer_release();
        assert_eq!(lease.complete(false), None);
        assert!(lease.begin(2).is_err());
        assert_eq!(lease.complete(true), Some(1));
        assert_eq!(lease.complete(true), None);
        lease.begin(2).expect("completed buffer can be reused");
        lease.consumer_release();
        lease.consumer_release();
        assert_eq!(lease.complete(true), Some(2));
    }
    #[test]
    fn idle_or_duplicate_notifications_never_create_releases() {
        let mut lease = Lease::Idle;
        lease.consumer_release();
        assert_eq!(lease.complete(true), None);
        assert!(lease.begin(0).is_err());
    }
    #[test]
    #[cfg(not(miri))]
    fn write_readiness_is_not_reported_while_a_completion_fd_is_blocked() {
        use rustix::pipe::{PipeFlags, pipe_with};
        let (reader, writer) =
            pipe_with(PipeFlags::NONBLOCK | PipeFlags::CLOEXEC).expect("completion pipe");
        let bytes = [0_u8; 4096];
        while rustix::io::write(&writer, &bytes).is_ok() {}
        let planes = [Plane {
            fd: writer,
            offset: 0,
            stride: 4,
        }];
        assert!(!planes_complete(&planes).expect("not ready"));
        let mut bytes = [0_u8; 4096];
        rustix::io::read(&reader, &mut bytes).expect("complete pending use");
        assert!(planes_complete(&planes).expect("ready after completion"));
    }
}
