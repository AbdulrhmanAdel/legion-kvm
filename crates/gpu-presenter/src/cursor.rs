//! A small built-in desktop arrow avoids theme-file access in the GPU sandbox.
use super::*;
use std::{io::Write, os::fd::AsFd};
use wayland_client::protocol::wl_shm_pool;

pub(super) struct DesktopCursor {
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    _pool: wl_shm_pool::WlShmPool,
    _memory: File,
}

const SIZE: usize = 24;

fn arrow_pixels() -> Vec<u8> {
    let polygon = [
        (1., 1.),
        (1., 20.),
        (6., 15.),
        (10., 23.),
        (13., 22.),
        (9., 14.),
        (17., 14.),
    ];
    let mut mask = [[false; SIZE]; SIZE];
    for (y, row) in mask.iter_mut().enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            let (x, y) = (x as f64 + 0.5, y as f64 + 0.5);
            let mut previous = polygon[polygon.len() - 1];
            for &point in &polygon {
                if (point.1 > y) != (previous.1 > y)
                    && x < (previous.0 - point.0) * (y - point.1) / (previous.1 - point.1) + point.0
                {
                    *pixel = !*pixel;
                }
                previous = point;
            }
        }
    }
    let mut bytes = Vec::with_capacity(SIZE * SIZE * 4);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let interior = x > 0
                && y > 0
                && x + 1 < SIZE
                && y + 1 < SIZE
                && mask[y][x - 1]
                && mask[y][x + 1]
                && mask[y - 1][x]
                && mask[y + 1][x];
            let color: u32 = if !mask[y][x] {
                0
            } else if interior {
                0xffff_ffff
            } else {
                0xff00_0000
            };
            bytes.extend_from_slice(&color.to_ne_bytes());
        }
    }
    bytes
}

impl State {
    pub(super) fn initialize_cursor(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let shm = self
            .shm
            .as_ref()
            .context("desktop cursor requires wl_shm")?;
        let mut memory = File::from(rustix::fs::memfd_create(
            "legion-cursor",
            rustix::fs::MemfdFlags::CLOEXEC,
        )?);
        let bytes = arrow_pixels();
        memory.write_all(&bytes)?;
        let pool = shm.create_pool(memory.as_fd(), bytes.len() as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            SIZE as i32,
            SIZE as i32,
            SIZE as i32 * 4,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        let surface = self
            .compositor
            .as_ref()
            .context("missing compositor")?
            .create_surface(qh, ());
        self.cursor = Some(DesktopCursor {
            surface,
            buffer,
            _pool: pool,
            _memory: memory,
        });
        Ok(())
    }

    pub(super) fn update_cursor(&mut self) {
        let (Some(pointer), Some(serial)) = (&self.pointer, self.events.pointer_serial) else {
            return;
        };
        if self.events.pointer_locked() {
            pointer.set_cursor(serial, None, 0, 0);
        } else if let Some(cursor) = &self.cursor {
            pointer.set_cursor(serial, Some(&cursor.surface), 1, 1);
            cursor.surface.attach(Some(&cursor.buffer), 0, 0);
            cursor.surface.damage_buffer(0, 0, SIZE as i32, SIZE as i32);
            cursor.surface.commit();
        }
    }
}
delegate_noop!(State:ignore wl_shm_pool::WlShmPool);
