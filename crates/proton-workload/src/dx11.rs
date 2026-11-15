//! Owned D3D11/DXVK probe. Correctness readbacks are outside timed rendering.
use super::*;
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0,
                D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            },
            Direct3D11::*,
        },
    },
    core::{BOOL, s},
};

struct Gpu {
    context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    readback: ID3D11Texture2D,
    queries: Vec<ID3D11Query>,
    adapter: DXGI_ADAPTER_DESC,
    width: u32,
    height: u32,
    rounds: u32,
    frequency: u64,
}

impl Gpu {
    fn new(config: &Config, frequency: u64) -> Result<Self> {
        let rounds = config
            .pipeline
            .as_ref()
            .ok_or("D3D11 probe requires pipeline configuration")?
            .shader_rounds;
        let (mut device, mut context) = (None, None);
        // SAFETY: initialized outputs, hardware-only device selection, and live feature-level slice.
        unsafe {
            com(
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_FLAG::default(),
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                ),
                "D3D11 hardware device",
            )?;
        }
        let device = device.ok_or("missing D3D11 device")?;
        let context = context.ok_or("missing D3D11 context")?;
        let dxgi: IDXGIDevice = com(device.cast(), "D3D11 DXGI device")?;
        // SAFETY: live COM interfaces and driver-owned adapter description returned by value.
        let adapter = unsafe {
            com(
                com(dxgi.GetAdapter(), "D3D11 adapter")?.GetDesc(),
                "adapter description",
            )?
        };
        if adapter.VendorId != 0x10de && adapter.VendorId != 0x1002 && adapter.VendorId != 0x8086 {
            return Err("refusing a non-hardware D3D11 adapter".into());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: config.width,
            Height: config.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let (mut texture, mut readback, mut rtv) = (None, None, None);
        // SAFETY: initialized descriptors and output pointers; staging has no bind flags and read-only CPU access.
        unsafe {
            com(
                device.CreateTexture2D(&desc, None, Some(&mut texture)),
                "render texture",
            )?;
            com(
                device.CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        ..desc
                    },
                    None,
                    Some(&mut readback),
                ),
                "readback texture",
            )?;
            com(
                device.CreateRenderTargetView(
                    texture.as_ref().ok_or("missing render texture")?,
                    None,
                    Some(&mut rtv),
                ),
                "render view",
            )?;
        }
        let vs = pipeline::compile(include_bytes!("workload.hlsl"), s!("vs_main"), s!("vs_5_0"))?;
        let ps = pipeline::compile(include_bytes!("workload.hlsl"), s!("ps_main"), s!("ps_5_0"))?;
        let (mut vertex, mut pixel, mut constants, mut raster) = (None, None, None, None);
        let values = [7_u32, rounds, 0, 0];
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: values.as_ptr().cast(),
            ..Default::default()
        };
        // SAFETY: shader bytecode and initial constants live for these synchronous creation calls.
        unsafe {
            com(
                device.CreateVertexShader(pipeline::blob_bytes(&vs), None, Some(&mut vertex)),
                "vertex shader",
            )?;
            com(
                device.CreatePixelShader(pipeline::blob_bytes(&ps), None, Some(&mut pixel)),
                "pixel shader",
            )?;
            com(
                device.CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: 16,
                        Usage: D3D11_USAGE_IMMUTABLE,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        ..Default::default()
                    },
                    Some(&initial),
                    Some(&mut constants),
                ),
                "constant buffer",
            )?;
            com(
                device.CreateRasterizerState(
                    &D3D11_RASTERIZER_DESC {
                        FillMode: D3D11_FILL_SOLID,
                        CullMode: D3D11_CULL_NONE,
                        DepthClipEnable: true.into(),
                        ..Default::default()
                    },
                    Some(&mut raster),
                ),
                "rasterizer",
            )?;
            context.VSSetShader(vertex.as_ref(), None);
            context.PSSetShader(pixel.as_ref(), None);
            context.PSSetConstantBuffers(0, Some(&[constants]));
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.RSSetState(raster.as_ref());
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: config.width as f32,
                Height: config.height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            context.OMSetRenderTargets(Some(&[rtv]), None);
        }
        let mut queries = Vec::new();
        for _ in 0..3 {
            let mut query = None;
            // SAFETY: event query needs no begin and returns a BOOL after the preceding draw completes.
            unsafe {
                com(
                    device.CreateQuery(
                        &D3D11_QUERY_DESC {
                            Query: D3D11_QUERY_EVENT,
                            MiscFlags: 0,
                        },
                        Some(&mut query),
                    ),
                    "completion query",
                )?;
            }
            queries.push(query.ok_or("missing query")?);
        }
        Ok(Self {
            context,
            texture: texture.ok_or("missing texture")?,
            readback: readback.ok_or("missing readback")?,
            queries,
            adapter,
            width: config.width,
            height: config.height,
            rounds,
            frequency,
        })
    }
    fn submit(&self, index: usize) {
        // SAFETY: the context retains the live pipeline/target; each query slot is reused only after wait.
        unsafe {
            self.context.Draw(3, 0);
            self.context.End(&self.queries[index]);
            self.context.Flush();
        }
    }
    fn wait(&self, index: usize) -> Result<()> {
        let start = counter()?;
        loop {
            let mut ready = BOOL(0);
            // SAFETY: the event query's BOOL output has exact required size and is live during the call.
            unsafe {
                com(
                    self.context.GetData(
                        &self.queries[index],
                        Some((&mut ready as *mut BOOL).cast()),
                        core::mem::size_of::<BOOL>() as u32,
                        D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
                    ),
                    "wait completion query",
                )?;
            }
            if ready.as_bool() {
                return Ok(());
            }
            if counter()?.saturating_sub(start) > self.frequency * 10 {
                return Err("D3D11 GPU completion timed out".into());
            }
            core::hint::spin_loop();
        }
    }
    fn verify(&self) -> Result<u64> {
        self.submit(0);
        self.wait(0)?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: textures have identical dimensions/format; Map waits for the copy and the mapping stays live until Unmap.
        unsafe {
            self.context.CopyResource(&self.readback, &self.texture);
            com(
                self.context
                    .Map(&self.readback, 0, D3D11_MAP_READ, 0, Some(&mut mapped)),
                "map readback",
            )?;
        }
        let len = (mapped.RowPitch as usize)
            .checked_mul(self.height as usize)
            .ok_or("invalid mapped extent")?;
        // SAFETY: successful texture mapping exposes height rows, each RowPitch bytes long.
        let bytes = unsafe { core::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
        let check = legion_proton_workload::verify_shader(
            bytes,
            mapped.RowPitch as usize,
            self.width,
            self.height,
            7,
            self.rounds,
        );
        // SAFETY: this is the same mapped subresource; bytes are no longer accessed afterwards.
        unsafe {
            self.context.Unmap(&self.readback, 0);
        }
        Ok(check
            .ok_or("D3D11 full-image shader validation failed")?
            .hash)
    }
    fn measure(&self, milliseconds: u32, record: bool) -> Result<Sample> {
        let start = counter()?;
        let deadline = start + self.frequency * u64::from(milliseconds) / 1000;
        let mut serial = 0_usize;
        let mut intervals = Vec::new();
        let mut previous = start;
        while counter()? < deadline {
            let slot = serial % 3;
            if serial >= 3 {
                self.wait(slot)?;
                let now = counter()?;
                if record {
                    intervals.push(now - previous);
                }
                previous = now;
            }
            self.submit(slot);
            serial += 1;
        }
        for index in serial.saturating_sub(3)..serial {
            self.wait(index % 3)?;
        }
        let elapsed = counter()?.saturating_sub(start) as f64 / self.frequency as f64;
        let timing = timing_summary(&mut intervals, self.frequency).unwrap_or(
            legion_proton_workload::Timing {
                mean_us: 0.0,
                p50_us: 0.0,
                p95_us: 0.0,
                p99_us: 0.0,
            },
        );
        Ok(Sample {
            mode: "d3d11_shader_3_in_flight".into(),
            iterations: serial as u64,
            elapsed_ms: elapsed * 1000.0,
            iterations_per_second: serial as f64 / elapsed,
            p50_us: timing.p50_us,
            p95_us: timing.p95_us,
            p99_us: timing.p99_us,
            cpu_checksum: 0,
            stages: Default::default(),
            gpu: None,
            pipeline: None,
        })
    }
}

pub(super) fn run() -> Result<()> {
    let config = read_config()?;
    if config.presentation.is_some() {
        return Err("D3D11 probe currently supports offscreen rendering only".into());
    }
    let mut frequency = 0;
    // SAFETY: valid writable output for this synchronous OS call.
    if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 || frequency <= 0 {
        return Err("QPC unavailable".into());
    }
    let gpu = Gpu::new(&config, frequency as u64)?;
    log("stage: verify D3D11 output before timing");
    let before = gpu.verify()?;
    gpu.measure(config.warmup_ms, false)?;
    let sample = gpu.measure(config.sample_ms, true)?;
    log("stage: verify D3D11 output after timing");
    let after = gpu.verify()?;
    let name_end = gpu
        .adapter
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(gpu.adapter.Description.len());
    let report = Report {
        schema: 1,
        passed: before == after,
        gpu_verified: before == after,
        workload: "windows_d3d11_shader_3_in_flight".into(),
        adapter: String::from_utf16_lossy(&gpu.adapter.Description[..name_end]),
        vendor_id: gpu.adapter.VendorId,
        device_id: gpu.adapter.DeviceId,
        // SAFETY: CPUID is available on x86_64; the guest contract guarantees OSXSAVE for XGETBV.
        logical_cpus: unsafe { GetActiveProcessorCount(u16::MAX) },
        xcr0: if core::arch::x86_64::__cpuid(1).ecx & (1 << 27) != 0 {
            // SAFETY: OSXSAVE was checked immediately above.
            unsafe { core::arch::x86_64::_xgetbv(0) }
        } else {
            0
        },
        qpc_frequency: frequency as u64,
        diagnostic_completion_kicks: 0,
        timestamp_validation_copy_ticks: Vec::new(),
        expected_cpu_checksum: 0,
        config,
        image_hash_before: before,
        image_hash_after: after,
        cases: vec![sample],
    };
    let output = open("Z:\\frames\\workload-result.json", true)?;
    write_handle(
        output.0,
        &serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )?;
    log("LEGION_PROTON_WORKLOAD_PASSED");
    Ok(())
}
