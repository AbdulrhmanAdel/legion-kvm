//! Capture only this window's ordered Wayland input.
use super::*;
use legion_display_channel::input_events::{
    Batch, CaptureMode, Event, MAX_KEYMAP, Queue, TimedEvent,
};
use std::time::Instant;
use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{
        zwp_confined_pointer_v1, zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
    },
    relative_pointer::zv1::client::{zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1},
};

#[derive(Default)]
pub(super) struct Wheel {
    pub value: f64,
    pub v120: i32,
    pub stop: bool,
    pub changed: bool,
}
pub(super) struct Capture {
    start: Instant,
    queue: Queue,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    keyboard_focus: bool,
    pointer_focus: bool,
    pub pointer_serial: Option<u32>,
    pub keymap: String,
    pub keymap_generation: u32,
    pub wheel: [Wheel; 2],
    pub axis_source: u32,
    pub relative_manager: Option<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
    pub relative: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    pub constraints: Option<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>,
    locked: Option<zwp_locked_pointer_v1::ZwpLockedPointerV1>,
    confined: Option<zwp_confined_pointer_v1::ZwpConfinedPointerV1>,
    mode: CaptureMode,
    active: bool,
    modifiers: [u32; 4],
}
impl Default for Capture {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            queue: Queue::default(),
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            keyboard_focus: false,
            pointer_focus: false,
            pointer_serial: None,
            keymap: String::new(),
            keymap_generation: 0,
            wheel: Default::default(),
            axis_source: 0,
            relative_manager: None,
            relative: None,
            constraints: None,
            locked: None,
            confined: None,
            mode: CaptureMode::None,
            active: false,
            modifiers: [0; 4],
        }
    }
}
impl Capture {
    pub(super) fn pointer_locked(&self) -> bool {
        self.mode == CaptureMode::Lock
    }
    pub fn modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.modifiers = [depressed, latched, locked, group];
        self.push(Event::Modifiers {
            depressed,
            latched,
            locked,
            group,
        });
    }
    pub fn enter_keys(&mut self, bytes: &[u8]) {
        self.keys.clear();
        self.keyboard_focus = true;
        self.push(Event::KeyboardRestore);
        for bytes in bytes.as_chunks::<4>().0 {
            let code = u32::from_ne_bytes(*bytes);
            if code < 768 && self.keys.insert(code) {
                self.push(Event::HeldKey { code });
            }
        }
        self.push(Event::Focus {
            keyboard: true,
            focused: true,
        });
    }
    pub fn push(&mut self, event: Event) {
        self.queue.push(TimedEvent {
            time_us: self.start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
            event,
        });
    }
    pub fn key(&mut self, code: u32, down: bool) {
        if code >= 768 || !self.keyboard_focus {
            return;
        }
        let changed = if down {
            self.keys.insert(code)
        } else {
            self.keys.remove(&code)
        };
        if changed {
            self.push(Event::Key { code, down });
        }
    }
    pub fn button(&mut self, code: u32, down: bool) {
        if !(0x110..=0x11f).contains(&code) || (!self.pointer_focus && !self.active) {
            return;
        }
        let changed = if down {
            self.buttons.insert(code)
        } else {
            self.buttons.remove(&code)
        };
        if changed {
            self.push(Event::Button { code, down });
        }
    }
    pub fn focus(&mut self, keyboard: bool, focused: bool) {
        if keyboard {
            self.keyboard_focus = focused;
            if !focused {
                self.keys.clear();
                self.buttons.clear();
            }
        } else {
            self.pointer_focus = focused;
            if !focused {
                self.buttons.clear();
            }
        }
        self.push(Event::Focus { keyboard, focused });
    }
    pub fn flush_wheel(&mut self) {
        for axis in 0..2 {
            let pending = std::mem::take(&mut self.wheel[axis]);
            if pending.changed && self.pointer_focus {
                self.push(Event::Axis {
                    axis: axis as u32,
                    value: pending.value,
                    v120: pending.v120,
                    stop: pending.stop,
                    source: self.axis_source,
                });
            }
        }
    }
    pub fn keymap(&mut self, fd: OwnedFd, size: u32) -> Result<()> {
        ensure!(
            size > 0 && size as usize <= MAX_KEYMAP + 1,
            "host keymap too large"
        );
        let mut bytes = vec![0; size as usize];
        File::from(fd).read_exact_at(&mut bytes, 0)?;
        if bytes.last() == Some(&0) {
            bytes.pop();
        }
        ensure!(!bytes.contains(&0), "embedded NUL in host keymap");
        let keymap = String::from_utf8(bytes)?;
        if self.keymap != keymap {
            self.keymap = keymap;
            self.keymap_generation = self
                .keymap_generation
                .checked_add(1)
                .context("keymap generation exhausted")?;
            self.queue
                .reset(self.start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
            // Discarded old-layout events must not strand focus or modifier state.
            self.push(Event::KeyboardRestore);
            if self.keyboard_focus {
                for code in self.keys.iter().copied().collect::<Vec<_>>() {
                    self.push(Event::HeldKey { code });
                }
            }
            self.push(Event::Focus {
                keyboard: true,
                focused: self.keyboard_focus,
            });
            self.push(Event::Focus {
                keyboard: false,
                focused: self.pointer_focus,
            });
            let [depressed, latched, locked, group] = self.modifiers;
            self.modifiers(depressed, latched, locked, group);
        }
        Ok(())
    }
    pub fn batch(&mut self, snapshot: DesktopInput) -> Batch {
        let mut batch = Batch {
            width: snapshot.width,
            height: snapshot.height,
            closed: snapshot.closed,
            capture_active: self.active,
            keymap_generation: self.keymap_generation,
            ..Batch::default()
        };
        self.queue.drain(&mut batch);
        batch
    }
    fn capture(&mut self, active: bool) {
        if self.active != active {
            self.active = active;
            self.push(Event::Capture { active });
        }
    }
}

impl State {
    pub(super) fn initialize_relative(&mut self, qh: &QueueHandle<Self>) {
        if self.events.relative.is_none()
            && let (Some(manager), Some(pointer)) = (&self.events.relative_manager, &self.pointer)
        {
            self.events.relative = Some(manager.get_relative_pointer(pointer, qh, ()));
        }
    }
    pub(super) fn event_pointer_position(&mut self, x: f64, y: f64) {
        if !self.events.pointer_focus {
            return;
        }
        if let Some(input) = self.input {
            self.events.push(Event::Absolute {
                x: (x / f64::from(input.width)).clamp(-16.0, 16.0),
                y: (y / f64::from(input.height)).clamp(-16.0, 16.0),
            });
        }
    }
    pub(super) fn update_capture(&mut self, requested: CaptureMode, qh: &QueueHandle<Self>) {
        let mode = if self.events.keyboard_focus {
            requested
        } else {
            CaptureMode::None
        };
        if mode == self.events.mode {
            return;
        }
        if let Some(locked) = self.events.locked.take() {
            locked.destroy();
        }
        if let Some(confined) = self.events.confined.take() {
            confined.destroy();
        }
        self.events.capture(false);
        self.events.mode = mode;
        self.update_cursor();
        if let (Some(manager), Some(surface), Some(pointer)) =
            (&self.events.constraints, &self.surface, &self.pointer)
        {
            use zwp_pointer_constraints_v1::Lifetime;
            match mode {
                CaptureMode::Lock => {
                    self.events.locked = Some(manager.lock_pointer(
                        surface,
                        pointer,
                        None,
                        Lifetime::Persistent,
                        qh,
                        (),
                    ))
                }
                CaptureMode::Confine => {
                    self.events.confined = Some(manager.confine_pointer(
                        surface,
                        pointer,
                        None,
                        Lifetime::Persistent,
                        qh,
                        (),
                    ))
                }
                CaptureMode::None => {}
            }
        }
    }
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
            && (state.events.pointer_focus || state.events.active)
        {
            state.events.push(Event::Relative {
                dx,
                dy,
                raw_dx: dx_unaccel,
                raw_dy: dy_unaccel,
            });
        }
    }
}
impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => state.events.capture(true),
            zwp_locked_pointer_v1::Event::Unlocked => state.events.capture(false),
            _ => {}
        }
    }
}
impl Dispatch<zwp_confined_pointer_v1::ZwpConfinedPointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        event: zwp_confined_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_confined_pointer_v1::Event::Confined => state.events.capture(true),
            zwp_confined_pointer_v1::Event::Unconfined => state.events.capture(false),
            _ => {}
        }
    }
}
delegate_noop!(State:ignore zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1);
delegate_noop!(State:ignore zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> DesktopInput {
        DesktopInput {
            input: Input::new(0, 0, false, 0).expect("input"),
            closed: false,
            width: 1920,
            height: 1080,
        }
    }
    #[test]
    fn all_keys_buttons_and_focus_loss_remain_ordered_and_window_scoped() {
        let mut capture = Capture::default();
        capture.key(30, true);
        assert!(capture.batch(snapshot()).events.is_empty());
        capture.focus(true, true);
        capture.key(30, true);
        capture.key(30, true);
        capture.key(30, false);
        capture.focus(false, true);
        capture.button(0x110, true);
        capture.button(0x110, false);
        capture.focus(true, false);
        let events = capture
            .batch(snapshot())
            .events
            .into_iter()
            .map(|e| e.event)
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![
                Event::Focus {
                    keyboard: true,
                    focused: true
                },
                Event::Key {
                    code: 30,
                    down: true
                },
                Event::Key {
                    code: 30,
                    down: false
                },
                Event::Focus {
                    keyboard: false,
                    focused: true
                },
                Event::Button {
                    code: 0x110,
                    down: true
                },
                Event::Button {
                    code: 0x110,
                    down: false
                },
                Event::Focus {
                    keyboard: true,
                    focused: false
                }
            ]
        );
    }
    #[test]
    fn wheel_value_and_discrete_units_are_one_event_per_axis_frame() {
        let mut capture = Capture::default();
        capture.focus(false, true);
        capture.wheel[0] = Wheel {
            value: -10.0,
            v120: -120,
            changed: true,
            ..Wheel::default()
        };
        capture.flush_wheel();
        capture.flush_wheel();
        let batch = capture.batch(snapshot());
        assert_eq!(batch.events.len(), 2);
        assert_eq!(
            batch.events[1].event,
            Event::Axis {
                axis: 0,
                value: -10.0,
                v120: -120,
                stop: false,
                source: 0
            }
        );
    }
}
