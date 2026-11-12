//! Native control for the VM's GPU acquisition dependency.
use std::time::Instant;

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use smithay::{
    backend::{
        allocator::{Fourcc, dmabuf::Dmabuf},
        renderer::{Bind, Blit, Offscreen, Renderer, TextureFilter, gles::GlesRenderbuffer},
    },
    reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    utils::Rectangle,
    wayland::dmabuf::get_dmabuf,
};

use crate::{acquire::Queue, acquire_gpu::Gpu};

struct Source {
    buffer: WlBuffer,
    fence: u64,
    _image: Dmabuf,
    started: Instant,
}

pub(crate) struct Acquirer {
    gpu: Gpu,
    target: GlesRenderbuffer,
    queue: Queue<Source>,
    next_frame: u64,
    completed: u64,
    enqueue_ns: u128,
    latency_ns: u128,
}

impl Acquirer {
    pub fn new() -> Result<Self> {
        let (renderer, _) = crate::renderer()?;
        let mut gpu = Gpu::new(renderer, true)?;
        let target = gpu
            .renderer
            .create_buffer(Fourcc::Abgr8888, (1, 1).into())?;
        Ok(Self {
            gpu,
            target,
            queue: Queue::new(3),
            next_frame: 0,
            completed: 0,
            enqueue_ns: 0,
            latency_ns: 0,
        })
    }

    pub fn is_full(&self) -> bool {
        self.queue.is_full()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn push(&mut self, buffer: WlBuffer) -> Result<()> {
        ensure!(!self.is_full(), "native acquire queue full");
        let started = Instant::now();
        let mut image = get_dmabuf(&buffer)?.clone();
        {
            let source = self.gpu.renderer.bind(&mut image)?;
            let mut target = self.gpu.renderer.bind(&mut self.target)?;
            self.gpu.renderer.blit(
                &source,
                &mut target,
                Rectangle::from_size((1, 1).into()),
                Rectangle::from_size((1, 1).into()),
                TextureFilter::Nearest,
            )?;
        }
        self.next_frame = self
            .next_frame
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("native acquire frame overflow"))?;
        self.gpu.fence(self.next_frame)?;
        self.enqueue_ns += started.elapsed().as_nanos();
        self.queue.push(
            self.next_frame,
            self.next_frame,
            Source {
                buffer,
                fence: self.next_frame,
                _image: image,
                started,
            },
        )
    }

    pub fn pop_ready(&mut self) -> Result<Option<WlBuffer>> {
        let gpu = &mut self.gpu;
        // Source buffers remain owned until their own GL fence completes.
        let acquired = self.queue.pop_ready(|source| gpu.ready(source.fence))?;
        if let Some(acquired) = acquired {
            self.completed += 1;
            self.latency_ns += acquired.payload.started.elapsed().as_nanos();
            self.gpu.renderer.cleanup_texture_cache()?;
            Ok(Some(acquired.payload.buffer))
        } else {
            Ok(None)
        }
    }

    pub fn report(&self) -> Value {
        json!({
            "dependency": "gpu_blit_fence", "submitted": self.next_frame,
            "completed": self.completed, "pending": self.gpu.pending(),
            "enqueue_mean_ms": self.enqueue_ns as f64 / self.next_frame.max(1) as f64 / 1e6,
            "latency_mean_ms": self.latency_ns as f64 / self.completed.max(1) as f64 / 1e6,
        })
    }
}
