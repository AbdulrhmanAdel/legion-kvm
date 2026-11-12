use std::{
    borrow::Cow,
    fs::{self, File},
    os::fd::{AsFd, BorrowedFd},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use smithay::{
    backend::input::KeyState,
    delegate_xwayland_shell,
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
    },
    reexports::{
        calloop::EventLoop,
        wayland_server::{DisplayHandle, Resource, protocol::wl_surface::WlSurface},
    },
    utils::{IsAlive, Logical, Rectangle, Serial},
    wayland::{
        seat::WaylandFocus,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, XwmId},
    },
};

use crate::state::State;

#[derive(Clone, Debug, PartialEq)]
pub enum Focus {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl IsAlive for Focus {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(s) => s.is_alive(),
            Self::X11(s) => s.alive(),
        }
    }
}
impl WaylandFocus for Focus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(s) => Some(Cow::Borrowed(s)),
            Self::X11(s) => s.wl_surface().map(Cow::Owned),
        }
    }
}
impl KeyboardTarget<State> for Focus {
    fn enter(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
        }
    }
    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11(s) => KeyboardTarget::leave(s, seat, data, serial),
        }
    }
    fn key(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
        }
    }
    fn modifiers(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        mods: ModifiersState,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::modifiers(s, seat, data, mods, serial),
            Self::X11(s) => KeyboardTarget::modifiers(s, seat, data, mods, serial),
        }
    }
}

pub(crate) struct Bridge {
    events: EventLoop<'static, State>,
}
impl Bridge {
    pub fn start(config: &Value, dh: &DisplayHandle) -> Result<Option<Self>> {
        let enabled = config
            .get("xwayland")
            .map_or(Some(false), Value::as_bool)
            .context("xwayland setting must be boolean")?;
        if !enabled {
            return Ok(None);
        }
        match fs::remove_file("/frames/xwayland-ready.json") {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let events = EventLoop::try_new()?;
        let handle = events.handle();
        let wm_handle = handle.clone();
        let log = File::create("/frames/xwayland.log")?;
        let env = std::env::vars_os().filter(|(k, _)| {
            matches!(
                k.to_str(),
                Some(
                    "LD_LIBRARY_PATH"
                        | "__EGL_VENDOR_LIBRARY_DIRS"
                        | "FONTCONFIG_FILE"
                        | "FONTCONFIG_PATH"
                )
            )
        });
        let (server, client) = XWayland::spawn(dh, None, env, true, Stdio::null(), log, |_| ())
            .context("start guest XWayland")?;
        handle
            .insert_source(server, move |event, _, state: &mut State| {
                let result = match event {
                    XWaylandEvent::Ready {
                        x11_socket,
                        display_number,
                    } => (|| -> Result<()> {
                        state.xwm = Some(
                            X11Wm::start_wm(wm_handle.clone(), x11_socket, client.clone())
                                .map_err(|error| {
                                    anyhow::anyhow!("attach X11 window manager: {error}")
                                })?,
                        );
                        fs::write(
                            "/frames/xwayland-ready.json.tmp",
                            serde_json::to_vec(&json!({"display":format!(":{display_number}")}))?,
                        )?;
                        fs::rename(
                            "/frames/xwayland-ready.json.tmp",
                            "/frames/xwayland-ready.json",
                        )?;
                        Ok(())
                    })(),
                    XWaylandEvent::Error => {
                        Err(anyhow::anyhow!("guest XWayland failed before readiness"))
                    }
                };
                if let Err(e) = result {
                    state.error = Some(format!("XWayland: {e:#}"));
                }
            })
            .map_err(|e| anyhow::anyhow!("register XWayland source: {e}"))?;
        Ok(Some(Self { events }))
    }
    pub fn dispatch(&mut self, state: &mut State) -> Result<()> {
        self.events
            .dispatch(Duration::ZERO, state)
            .context("dispatch X11 events")
    }
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.events.as_fd()
    }
}

impl State {
    fn focus_latest_x11(&mut self) {
        if let Some(window) = self
            .x11_windows
            .iter()
            .rev()
            .find(|window| {
                window.is_mapped()
                    && !window.is_override_redirect()
                    && window.wl_surface().is_some()
            })
            .cloned()
        {
            self.focus_window(Focus::X11(window));
        } else {
            self.clear_window_selection();
        }
    }
    fn remember_x11(&mut self, window: X11Surface) {
        if !self.x11_windows.contains(&window) {
            self.x11_windows.push(window);
        }
    }
    fn x11_result(&mut self, result: Result<(), impl std::fmt::Display>) {
        if let Err(e) = result {
            self.error = Some(format!("X11 window management: {e}"));
        }
    }
}
impl XwmHandler for State {
    fn xwm_state(&mut self, id: XwmId) -> &mut X11Wm {
        let wm = self
            .xwm
            .as_mut()
            .expect("XWM callbacks require attached window manager");
        assert_eq!(wm.id(), id);
        wm
    }
    fn new_window(&mut self, _: XwmId, _: X11Surface) {}
    fn new_override_redirect_window(&mut self, _: XwmId, _: X11Surface) {}
    fn map_window_request(&mut self, _: XwmId, window: X11Surface) {
        let mut geometry = window.geometry();
        if geometry.size.w <= 0 || geometry.size.h <= 0 {
            geometry.size = (self.width, self.height).into();
        }
        self.x11_result(window.configure(geometry));
        self.x11_result(window.set_mapped(true));
        self.x11_result(window.set_activated(true));
        self.remember_x11(window);
        self.focus_latest_x11();
        self.x11_maps += 1;
    }
    fn mapped_override_redirect_window(&mut self, _: XwmId, window: X11Surface) {
        self.remember_x11(window);
    }
    fn unmapped_window(&mut self, _: XwmId, window: X11Surface) {
        self.x11_windows.retain(|w| *w != window);
        self.focus_latest_x11();
    }
    fn destroyed_window(&mut self, _: XwmId, window: X11Surface) {
        self.x11_windows.retain(|w| *w != window);
        self.focus_latest_x11();
    }
    fn configure_request(
        &mut self,
        _: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _: Option<Reorder>,
    ) {
        let mut geometry = window.geometry();
        if let Some(x) = x {
            geometry.loc.x = x;
        }
        if let Some(y) = y {
            geometry.loc.y = y;
        }
        for (requested, dimension) in [(w, &mut geometry.size.w), (h, &mut geometry.size.h)] {
            if let Some(size) = requested {
                match i32::try_from(size).ok().filter(|v| *v > 0 && *v <= 16384) {
                    Some(value) => *dimension = value,
                    None => {
                        self.error = Some("invalid X11 window dimensions".into());
                        return;
                    }
                }
            }
        }
        self.x11_result(window.configure(geometry));
    }
    fn configure_notify(
        &mut self,
        _: XwmId,
        _: X11Surface,
        _: Rectangle<i32, Logical>,
        _: Option<u32>,
    ) {
    }
    fn resize_request(&mut self, _: XwmId, _: X11Surface, _: u32, _: ResizeEdge) {}
    fn move_request(&mut self, _: XwmId, _: X11Surface, _: u32) {}
    fn fullscreen_request(&mut self, _: XwmId, window: X11Surface) {
        self.x11_result(window.set_fullscreen(true));
        self.x11_result(window.configure(Rectangle::from_size((self.width, self.height).into())));
    }
    fn disconnected(&mut self, _: XwmId) {
        if !std::path::Path::new("/frames/proton-client-exited").exists() {
            self.error = Some("XWayland disconnected before application exit".into());
        }
    }
}
impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell
    }
    fn surface_associated(&mut self, _: XwmId, wl_surface: WlSurface, _: X11Surface) {
        self._output.enter(&wl_surface);
        self.focus_latest_x11();
        self.x11_associations += 1;
    }
}
delegate_xwayland_shell!(State);
