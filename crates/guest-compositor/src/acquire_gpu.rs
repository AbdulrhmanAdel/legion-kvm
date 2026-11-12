//! Context-owned GL fences.
use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use smithay::backend::renderer::gles::{GlesRenderer, ffi};

pub(crate) struct Gpu {
    // This owner deletes syncs with their context current before teardown.
    pub renderer: GlesRenderer,
    fences: HashMap<u64, ffi::types::GLsync>,
}

impl Gpu {
    pub fn new(mut renderer: GlesRenderer, asynchronous: bool) -> Result<Self> {
        if asynchronous {
            renderer.with_context(|gl| -> Result<()> {
                ensure!(
                    gl.FenceSync.is_loaded()
                        && gl.ClientWaitSync.is_loaded()
                        && gl.DeleteSync.is_loaded(),
                    "renderer requires GL sync support"
                );
                Ok(())
            })??;
        }
        Ok(Self {
            renderer,
            fences: HashMap::new(),
        })
    }

    pub fn fence(&mut self, frame: u64) -> Result<(u128, u128)> {
        ensure!(
            self.fences.len() < 3 && !self.fences.contains_key(&frame),
            "invalid acquire fence allocation"
        );
        let (fence, create_ns, flush_ns) = self.renderer.with_context(|gl| -> Result<_> {
            let at = Instant::now();
            // SAFETY: with_context makes this owner's live GLES context current, and new checked these entrypoints.
            let fence = unsafe { gl.FenceSync(ffi::SYNC_GPU_COMMANDS_COMPLETE, 0) };
            let create_ns = at.elapsed().as_nanos();
            ensure!(!fence.is_null(), "GL acquire fence creation failed");
            let at = Instant::now();
            // SAFETY: same live/current context; Flush submits without changing framebuffer bindings or waiting for this fence's completion.
            unsafe { gl.Flush() };
            Ok((fence, create_ns, at.elapsed().as_nanos()))
        })??;
        self.fences.insert(frame, fence);
        Ok((create_ns, flush_ns))
    }

    pub fn ready(&mut self, frame: u64) -> Result<bool> {
        let fence = *self.fences.get(&frame).context("unknown acquire fence")?;
        let ready = self.renderer.with_context(|gl| -> Result<bool> {
            // SAFETY: this sync is owned by self.fences in this current context.
            match unsafe { gl.ClientWaitSync(fence, 0, 0) } {
                ffi::ALREADY_SIGNALED | ffi::CONDITION_SATISFIED => {
                    // SAFETY: this owned, signaled sync is deleted once here; its registry entry is removed before another query.
                    unsafe { gl.DeleteSync(fence) };
                    Ok(true)
                }
                ffi::TIMEOUT_EXPIRED => Ok(false),
                _ => bail!("GL acquire fence query failed"),
            }
        })??;
        if ready {
            self.fences.remove(&frame);
        }
        Ok(ready)
    }

    pub fn pending(&self) -> usize {
        self.fences.len()
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        let fences = &mut self.fences;
        let _ = self.renderer.with_context(|gl| {
            for (_, fence) in fences.drain() {
                // SAFETY: these remaining syncs belong to this owner's current context and have not been deleted.
                unsafe { gl.DeleteSync(fence) };
            }
        });
        // If making the context current failed, its destruction releases the remaining GL objects.
    }
}
