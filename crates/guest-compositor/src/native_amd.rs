//! Native control: AMD Vulkan clients, linear DMA-BUFs, desktop GPU sampling.
use crate::{
    OwnedClient,
    state::{ClientState, State},
};
use anyhow::{Context, Result, ensure};
use smithay::{
    backend::{
        allocator::{Buffer, Format, Fourcc, Modifier, dmabuf::Dmabuf},
        egl::ffi::{self as egl_ffi, egl},
        renderer::gles::ffi as gl,
    },
    reexports::wayland_server::{Display, ListeningSocket, protocol::wl_buffer::WlBuffer},
    wayland::dmabuf::get_dmabuf,
};
use std::{
    collections::VecDeque,
    ffi::{CStr, c_void},
    fs::{self, File},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

struct Texture {
    dma: Dmabuf,
    id: u32,
}
struct Renderer {
    gl: gl::Gles2,
    display: egl::types::EGLDisplay,
    textures: Vec<Texture>,
    pending: VecDeque<(WlBuffer, gl::types::GLsync)>,
    program: u32,
    vao: u32,
    _context: sdl2::video::GLContext,
    window: sdl2::video::Window,
    _video: sdl2::VideoSubsystem,
    _sdl: sdl2::Sdl,
}
impl Renderer {
    fn new(width: u32, height: u32) -> Result<Self> {
        let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
        let video = sdl.video().map_err(anyhow::Error::msg)?;
        let attrs = video.gl_attr();
        attrs.set_context_profile(sdl2::video::GLProfile::GLES);
        attrs.set_context_version(3, 0);
        let window = video
            .window("Legion native AMD rendering control", width, height)
            .opengl()
            .fullscreen_desktop()
            .build()?;
        let context = window.gl_create_context().map_err(anyhow::Error::msg)?;
        window
            .gl_make_current(&context)
            .map_err(anyhow::Error::msg)?;
        video
            .gl_set_swap_interval(sdl2::video::SwapInterval::Immediate)
            .map_err(anyhow::Error::msg)?;
        let gl = gl::Gles2::load_with(|name| video.gl_get_proc_address(name).cast());
        egl_ffi::make_sure_egl_is_loaded()?;
        // SAFETY: SDL made this owner's live EGL/GLES context current on this thread.
        let display = unsafe { egl::GetCurrentDisplay() };
        ensure!(
            display != egl::NO_DISPLAY,
            "SDL did not create an EGL display"
        );
        let mut result = Self {
            gl,
            display,
            textures: Vec::new(),
            pending: VecDeque::new(),
            program: 0,
            vao: 0,
            window,
            _context: context,
            _video: video,
            _sdl: sdl,
        };
        result.initialize()?;
        Ok(result)
    }
    fn initialize(&mut self) -> Result<()> {
        let vertex=c"#version 300 es\nout vec2 uv;void main(){vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);uv=vec2(p.x,1.0-p.y);gl_Position=vec4(p*2.0-1.0,0,1);}";
        let fragment=c"#version 300 es\n#extension GL_OES_EGL_image_external_essl3 : require\nprecision mediump float;uniform samplerExternalOES image;in vec2 uv;out vec4 color;void main(){color=texture(image,uv);}";
        let mut shaders = Vec::new();
        // SAFETY: this owner's SDL context is current.
        unsafe {
            self.program = self.gl.CreateProgram();
            for (kind, source) in [(gl::VERTEX_SHADER, vertex), (gl::FRAGMENT_SHADER, fragment)] {
                let shader = self.gl.CreateShader(kind);
                shaders.push(shader);
                self.gl
                    .ShaderSource(shader, 1, &source.as_ptr(), std::ptr::null());
                self.gl.CompileShader(shader);
                let mut status = 0;
                self.gl.GetShaderiv(shader, gl::COMPILE_STATUS, &mut status);
                if status == 0 {
                    for shader in shaders {
                        self.gl.DeleteShader(shader);
                    }
                    anyhow::bail!("external-texture shader compilation failed");
                }
                self.gl.AttachShader(self.program, shader);
            }
            self.gl.LinkProgram(self.program);
            for shader in shaders {
                self.gl.DeleteShader(shader);
            }
            let mut linked = 0;
            self.gl
                .GetProgramiv(self.program, gl::LINK_STATUS, &mut linked);
            ensure!(linked != 0, "external-texture program link failed");
            self.gl.GenVertexArrays(1, &mut self.vao);
        }
        Ok(())
    }
    fn import(&mut self, dma: &Dmabuf) -> Result<u32> {
        if let Some(texture) = self.textures.iter().find(|t| t.dma == *dma) {
            return Ok(texture.id);
        }
        ensure!(
            self.textures.len() < 256
                && dma.num_planes() == 1
                && dma.format().modifier == Modifier::Linear
                && !dma.y_inverted(),
            "native control requires a linear single-plane image"
        );
        let fd = dma.handles().next().context("missing image plane")?;
        let attrs = [
            egl::WIDTH as i32,
            dma.size().w,
            egl::HEIGHT as i32,
            dma.size().h,
            0x3271,
            dma.format().code as i32,
            0x3272,
            fd.as_raw_fd(),
            0x3273,
            i32::try_from(dma.offsets().next().context("missing image offset")?)?,
            0x3274,
            i32::try_from(dma.strides().next().context("missing image stride")?)?,
            0x3443,
            0,
            0x3444,
            0,
            egl::NONE as i32,
        ];
        // SAFETY: attributes have a terminating EGL_NONE; the DMA-BUF FD and storage are retained by the texture cache.
        let image = unsafe {
            egl::CreateImageKHR(
                self.display,
                egl::NO_CONTEXT,
                0x3270,
                std::ptr::null_mut(),
                attrs.as_ptr(),
            )
        };
        ensure!(
            image != egl::NO_IMAGE_KHR,
            "native DMA-BUF EGL import failed"
        );
        let mut texture = 0;
        // SAFETY: live current context; generated texture and EGL image belong to this owner.
        unsafe {
            self.gl.GenTextures(1, &mut texture);
            self.gl.BindTexture(gl::TEXTURE_EXTERNAL_OES, texture);
            self.gl.TexParameteri(
                gl::TEXTURE_EXTERNAL_OES,
                gl::TEXTURE_MIN_FILTER,
                gl::LINEAR as i32,
            );
            self.gl.TexParameteri(
                gl::TEXTURE_EXTERNAL_OES,
                gl::TEXTURE_MAG_FILTER,
                gl::LINEAR as i32,
            );
            self.gl
                .EGLImageTargetTexture2DOES(gl::TEXTURE_EXTERNAL_OES, image.cast::<c_void>());
            egl::DestroyImageKHR(self.display, image);
            ensure!(
                self.gl.GetError() == gl::NO_ERROR,
                "native external texture creation failed"
            );
        }
        self.textures.push(Texture {
            dma: dma.clone(),
            id: texture,
        });
        Ok(texture)
    }
    fn draw(&mut self, buffer: WlBuffer) -> Result<()> {
        let texture = self.import(get_dmabuf(&buffer)?)?;
        let (width, height) = self.window.drawable_size();
        // SAFETY: program, VAO and texture were created in this current context.
        unsafe {
            self.gl.BindFramebuffer(gl::FRAMEBUFFER, 0);
            self.gl
                .Viewport(0, 0, i32::try_from(width)?, i32::try_from(height)?);
            self.gl.UseProgram(self.program);
            self.gl.BindVertexArray(self.vao);
            self.gl.ActiveTexture(gl::TEXTURE0);
            self.gl.BindTexture(gl::TEXTURE_EXTERNAL_OES, texture);
            self.gl.DrawArrays(gl::TRIANGLES, 0, 3);
        }
        self.window.gl_swap_window();
        // SAFETY: this new sync covers all queued sampling of the source.
        let fence = unsafe { self.gl.FenceSync(gl::SYNC_GPU_COMMANDS_COMPLETE, 0) };
        ensure!(
            !fence.is_null(),
            "native presentation fence allocation failed"
        );
        self.pending.push_back((buffer, fence));
        Ok(())
    }
    fn release(&mut self) -> Result<()> {
        while let Some((_, fence)) = self.pending.front() {
            // SAFETY: this live fence belongs to this context, remains in the queue, and timeout zero only queries its completion state.
            let status = unsafe { self.gl.ClientWaitSync(*fence, 0, 0) };
            if status == gl::TIMEOUT_EXPIRED {
                break;
            }
            ensure!(
                matches!(status, gl::ALREADY_SIGNALED | gl::CONDITION_SATISFIED),
                "native presentation fence failed"
            );
            let (buffer, fence) = self
                .pending
                .pop_front()
                .context("presentation fence queue changed")?;
            // SAFETY: the signaled sync was removed from its sole owning queue.
            unsafe { self.gl.DeleteSync(fence) };
            buffer.release();
        }
        Ok(())
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: the SDL context and window are still alive during Drop and remain current on this thread.
        unsafe {
            self.gl.Finish();
            for (buffer, fence) in self.pending.drain(..) {
                self.gl.DeleteSync(fence);
                buffer.release();
            }
            for texture in &self.textures {
                self.gl.DeleteTextures(1, &texture.id);
            }
            self.gl.DeleteProgram(self.program);
            self.gl.DeleteVertexArrays(1, &self.vao);
        }
    }
}

pub(crate) fn run(path: &Path) -> Result<()> {
    let cfg: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    let output = Path::new(cfg["output"].as_str().context("missing native output")?);
    let userspace = Path::new(
        cfg["userspace"]
            .as_str()
            .context("missing native userspace")?,
    );
    let node = Path::new(cfg["render_node"].as_str().context("missing AMD node")?);
    let width = u32::try_from(cfg["width"].as_u64().context("missing width")?)?;
    let height = u32::try_from(cfg["height"].as_u64().context("missing height")?)?;
    ensure!(
        (64..=3840).contains(&width) && (64..=2160).contains(&height),
        "invalid native dimensions"
    );
    let seconds = cfg["seconds"].as_u64().context("missing duration")?;
    ensure!((5..=30).contains(&seconds), "invalid native duration");
    let mut renderer = Renderer::new(width, height)?;
    let mut display = Display::<State>::new()?;
    let mut dh = display.handle();
    let formats = [
        Fourcc::Xrgb8888,
        Fourcc::Argb8888,
        Fourcc::Xbgr8888,
        Fourcc::Abgr8888,
    ]
    .map(|code| Format {
        code,
        modifier: Modifier::Linear,
    });
    let mut state = State::forwarding(
        &dh,
        formats.to_vec(),
        fs::metadata(node)?.rdev(),
        width,
        height,
    )?;
    state.gpu_only = true;
    // A dedicated native-control socket in the caller's private runtime dir.
    let listener = ListeningSocket::bind("legion-native-amd")?;
    let log = File::create(output.join("client.log"))?;
    let mut child = OwnedClient(
        Command::new(userspace.join("lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"))
            .args([
                "--library-path",
                userspace
                    .join("lib/x86_64-linux-gnu")
                    .to_str()
                    .context("library path encoding")?,
            ])
            .arg(userspace.join("bin/vkcube"))
            .args([
                "--wsi",
                "wayland",
                "--present_mode",
                "1",
                "--width",
                &width.to_string(),
                "--height",
                &height.to_string(),
                "--c",
                "1000000",
            ])
            .env("WAYLAND_DISPLAY", "legion-native-amd")
            .env("VK_DRIVER_FILES", output.join("radv.json"))
            .env("VK_LOADER_LAYERS_DISABLE", "~implicit~")
            .env("LEGION_CUBE_SECONDS", seconds.to_string())
            .env("LEGION_CUBE_REPORT", output.join("cube-result.json"))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?,
    );
    let start = Instant::now();
    let mut clients = Vec::new();
    let mut frames = 0_u64;
    loop {
        ensure!(
            start.elapsed() < Duration::from_secs(seconds + 20),
            "native rendering control timed out"
        );
        while let Some(stream) = listener.accept()? {
            clients.push(dh.insert_client(stream, Arc::new(ClientState::default()))?);
        }
        display.dispatch_clients(&mut state)?;
        for (dma, notifier) in std::mem::take(&mut state.pending_imports) {
            renderer.import(&dma)?;
            let _ = notifier.successful::<State>();
            state.imports += 1;
        }
        renderer.release()?;
        if renderer.pending.len() < 3
            && let Some(buffer) = state.pending.take()
        {
            renderer.draw(buffer)?;
            frames += 1;
        }
        state.send_callbacks(u32::try_from(start.elapsed().as_millis())?);
        display.flush_clients()?;
        if let Some(status) = child.0.try_wait()? {
            ensure!(status.success(), "native rendering client failed");
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    // SAFETY: GetString returns context-owned NUL-terminated storage while the still-current SDL context is alive.
    let renderer_name = unsafe {
        let p = renderer.gl.GetString(gl::RENDERER);
        if p.is_null() {
            None
        } else {
            Some(CStr::from_ptr(p.cast()).to_string_lossy().into_owned())
        }
    };
    let report = serde_json::json!({"schema":1,"passed":true,"native":true,"frames_presented":frames,"imports":state.imports,
        "host_renderer":renderer_name,"source_commits":state.source_commits,"steady_state_pixel_readbacks":0});
    fs::write(
        output.join("presentation.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
