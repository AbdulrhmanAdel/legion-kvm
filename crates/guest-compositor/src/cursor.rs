//! Small Wayland cursor images go through virtio-GPU's hardware cursor.
use crate::state::State;
use anyhow::{Context, Result, ensure};
use drm::{
    buffer::{Buffer, DrmFourcc},
    control::{Device, crtc, dumbbuffer::DumbBuffer},
};
use smithay::{
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    reexports::wayland_server::protocol::{wl_shm::Format, wl_surface::WlSurface},
    wayland::{
        compositor::{BufferAssignment, SurfaceAttributes, with_states},
        shm::{BufferData, with_buffer_contents},
    },
};

const SIDE: usize = 64;
struct Sprite {
    pixels: Vec<u8>,
    hotspot: (i32, i32),
}

fn copy_sprite(bytes: &[u8], data: BufferData, scale: i32, hotspot: (i32, i32)) -> Result<Sprite> {
    let (w, h, stride, offset) = (
        usize::try_from(data.width)?,
        usize::try_from(data.height)?,
        usize::try_from(data.stride)?,
        usize::try_from(data.offset)?,
    );
    ensure!(
        (1..=256).contains(&w) && (1..=256).contains(&h) && scale > 0,
        "invalid cursor dimensions"
    );
    ensure!(
        matches!(data.format, Format::Argb8888 | Format::Xrgb8888),
        "unsupported cursor pixel format"
    );
    ensure!(
        stride >= w * 4
            && stride
                .checked_mul(h)
                .and_then(|n| offset.checked_add(n))
                .is_some_and(|end| end <= bytes.len()),
        "cursor pixels exceed SHM buffer"
    );
    let logical_w = (w / (scale as usize)).max(1);
    let logical_h = (h / (scale as usize)).max(1);
    let divisor = logical_w.max(logical_h).max(SIDE);
    let tw = (logical_w * SIDE / divisor).max(1);
    let th = (logical_h * SIDE / divisor).max(1);
    let mut pixels = vec![0; SIDE * SIDE * 4];
    for y in 0..th {
        for x in 0..tw {
            let source = offset + (y * h / th) * stride + (x * w / tw) * 4;
            let target = (y * SIDE + x) * 4;
            pixels[target..target + 4].copy_from_slice(&bytes[source..source + 4]);
            if data.format == Format::Xrgb8888 {
                pixels[target + 3] = 255;
            }
        }
    }
    let hx = (i64::from(hotspot.0) * tw as i64 / logical_w as i64).clamp(0, 63) as i32;
    let hy = (i64::from(hotspot.1) * th as i64 / logical_h as i64).clamp(0, 63) as i32;
    Ok(Sprite {
        pixels,
        hotspot: (hx, hy),
    })
}

fn default_sprite() -> Sprite {
    let mut pixels = vec![0; SIDE * SIDE * 4];
    for y in 0..24 {
        for x in 0..=y / 2 {
            let edge = x == 0 || x == y / 2 || y == 23;
            let at = (y * SIDE + x) * 4;
            let c = if edge { 0 } else { 255 };
            pixels[at..at + 4].copy_from_slice(&[c, c, c, 255]);
        }
    }
    Sprite {
        pixels,
        hotspot: (0, 0),
    }
}

fn surface_sprite(surface: &WlSurface, time: u32) -> Result<Option<Sprite>> {
    let (buffer, scale, hotspot, callbacks) = with_states(surface, |states| {
        let mut attributes = states.cached_state.get::<SurfaceAttributes>();
        let current = attributes.current();
        let buffer = current.buffer.take();
        let hotspot = states
            .data_map
            .get::<CursorImageSurfaceData>()
            .and_then(|data| data.lock().ok())
            .map(|data| (data.hotspot.x, data.hotspot.y))
            .unwrap_or((0, 0));
        (
            buffer,
            current.buffer_scale,
            hotspot,
            std::mem::take(&mut current.frame_callbacks),
        )
    });
    let result = if let Some(BufferAssignment::NewBuffer(buffer)) = buffer {
        let result = with_buffer_contents(&buffer, |pointer, len, data| {
            // SAFETY: Smithay retains the SHM mapping throughout this closure.
            // copy_sprite checks all offsets, strides and extents before reading.
            let bytes = unsafe { std::slice::from_raw_parts(pointer, len) };
            copy_sprite(bytes, data, scale, hotspot)
        })
        .context("map client cursor SHM");
        buffer.release();
        Some(result??)
    } else if matches!(buffer, Some(BufferAssignment::Removed)) {
        Some(Sprite {
            pixels: vec![0; SIDE * SIDE * 4],
            hotspot,
        })
    } else {
        None
    };
    for callback in callbacks {
        callback.done(time);
    }
    Ok(result)
}

pub(crate) struct Cursor {
    buffer: DumbBuffer,
    generation: Option<u64>,
    position: Option<(i32, i32)>,
    visible: bool,
    hotspot: (i32, i32),
}
// The scanout uses legacy KMS; virtio-GPU supports these cursor ioctls alongside it.
#[allow(
    deprecated,
    reason = "cursor must share the existing legacy KMS scanout"
)]
impl Cursor {
    pub(crate) fn new(card: &impl Device) -> Result<Self> {
        let buffer =
            card.create_dumb_buffer((SIDE as u32, SIDE as u32), DrmFourcc::Argb8888, 32)?;
        let mut cursor = Self {
            buffer,
            generation: None,
            position: None,
            visible: false,
            hotspot: (0, 0),
        };
        cursor.write(card, default_sprite())?;
        Ok(cursor)
    }
    fn write(&mut self, card: &impl Device, sprite: Sprite) -> Result<()> {
        let pitch = usize::try_from(self.buffer.pitch())?;
        let mut map = card.map_dumb_buffer(&mut self.buffer)?;
        ensure!(
            pitch >= SIDE * 4 && pitch * SIDE <= map.len(),
            "invalid virtual cursor buffer layout"
        );
        for y in 0..SIDE {
            map[y * pitch..y * pitch + SIDE * 4]
                .copy_from_slice(&sprite.pixels[y * SIDE * 4..(y + 1) * SIDE * 4]);
        }
        self.hotspot = sprite.hotspot;
        Ok(())
    }
    pub(crate) fn update(
        &mut self,
        card: &impl Device,
        crtc: crtc::Handle,
        state: &State,
        time: u32,
    ) -> Result<()> {
        if self.generation != Some(state.cursor_generation) {
            self.visible = !matches!(state.cursor_image, CursorImageStatus::Hidden);
            if self.visible {
                let sprite = match &state.cursor_image {
                    CursorImageStatus::Surface(surface) => surface_sprite(surface, time)?,
                    CursorImageStatus::Named(_) => Some(default_sprite()),
                    CursorImageStatus::Hidden => None,
                };
                if let Some(sprite) = sprite {
                    self.write(card, sprite)?;
                }
            }
            card.set_cursor2(crtc, self.visible.then_some(&self.buffer), self.hotspot)
                .context("update virtual hardware cursor")?;
            self.generation = Some(state.cursor_generation);
        }
        let position = state.cursor_position();
        if self.visible && self.position != Some(position) {
            card.move_cursor(crtc, position)
                .context("move virtual hardware cursor")?;
            self.position = Some(position);
        }
        Ok(())
    }
    pub(crate) fn destroy(self, card: &impl Device, crtc: crtc::Handle) {
        let _ = card.set_cursor2::<DumbBuffer>(crtc, None, (0, 0));
        let _ = card.destroy_dumb_buffer(self.buffer);
    }
}

#[cfg(test)]
#[path = "../tests/unit/cursor.rs"]
mod tests;
