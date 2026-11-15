//! Owned offscreen rendering, with independent fence-protected frame slots.
use super::*;
use legion_proton_workload::{PipelineTiming, RenderImageCheck, verify_shader};
use windows::{
    Win32::Foundation::RECT,
    Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob},
    core::{PCSTR, s},
};

const CAPACITY: usize = 262_144;

#[derive(Clone, Copy)]
enum RetirementWakeup {
    Gpu,
    Timer,
    Yield,
    Busy,
    Cpuid,
}

pub(super) fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: ID3DBlob owns the immutable buffer for the returned borrow's lifetime.
    unsafe { core::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

pub(super) fn compile(source: &[u8], entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    // SAFETY: source has its exact byte count, static strings are terminated, outputs are initialized; no include callbacks or filesystem access.
    let status = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("owned-workload.hlsl"),
            None,
            None,
            entry,
            target,
            (1 << 11) | (1 << 15),
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = status {
        return Err(format!(
            "shader compile: {e}; {}",
            errors
                .as_ref()
                .map(|b| String::from_utf8_lossy(blob_bytes(b)).into_owned())
                .unwrap_or_default()
        ));
    }
    code.ok_or_else(|| "shader compiler returned no bytecode".into())
}

fn create_pipeline(device: &ID3D12Device) -> Result<(ID3D12RootSignature, ID3D12PipelineState)> {
    create_pipeline_source(device, include_bytes!("workload.hlsl"), s!("ps_main"), 2)
}

pub(super) fn create_present_pipeline(
    device: &ID3D12Device,
    overlay: bool,
) -> Result<(ID3D12RootSignature, ID3D12PipelineState)> {
    create_pipeline_source(
        device,
        include_bytes!("present.hlsl"),
        if overlay {
            s!("ps_overlay")
        } else {
            s!("ps_main")
        },
        3,
    )
}

fn create_pipeline_source(
    device: &ID3D12Device,
    source: &[u8],
    pixel: PCSTR,
    constants: u32,
) -> Result<(ID3D12RootSignature, ID3D12PipelineState)> {
    let parameter = D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            Constants: D3D12_ROOT_CONSTANTS {
                ShaderRegister: 0,
                RegisterSpace: 0,
                Num32BitValues: constants,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
    };
    let mut serialized = None;
    // SAFETY: one initialized root parameter with a matching union tag; output owns the serialized signature after this synchronous call.
    unsafe {
        com(
            D3D12SerializeRootSignature(
                &D3D12_ROOT_SIGNATURE_DESC {
                    NumParameters: 1,
                    pParameters: &parameter,
                    Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
                    ..Default::default()
                },
                D3D_ROOT_SIGNATURE_VERSION_1,
                &mut serialized,
                None,
            ),
            "serialize root signature",
        )?;
    }
    let serialized = serialized.ok_or("missing serialized root signature")?;
    // SAFETY: serialized bytes are live and encode the two constants above.
    let root: ID3D12RootSignature = unsafe {
        com(
            device.CreateRootSignature(0, blob_bytes(&serialized)),
            "root signature",
        )?
    };
    let vs = compile(source, s!("vs_main"), s!("vs_5_0"))?;
    let ps = compile(source, pixel, s!("ps_5_0"))?;
    let bytecode = |b: &ID3DBlob| D3D12_SHADER_BYTECODE {
        pShaderBytecode: blob_bytes(b).as_ptr().cast(),
        BytecodeLength: blob_bytes(b).len(),
    };
    let stencil = D3D12_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D12_STENCIL_OP_KEEP,
        StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
        StencilPassOp: D3D12_STENCIL_OP_KEEP,
        StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
    };
    let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root.clone())),
        VS: bytecode(&vs),
        PS: bytecode(&ps),
        SampleMask: u32::MAX,
        RasterizerState: D3D12_RASTERIZER_DESC {
            FillMode: D3D12_FILL_MODE_SOLID,
            CullMode: D3D12_CULL_MODE_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        },
        DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
            DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            FrontFace: stencil,
            BackFace: stencil,
            ..Default::default()
        },
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: 1,
        RTVFormats: [
            DXGI_FORMAT_R8G8B8A8_UNORM,
            DXGI_FORMAT_UNKNOWN,
            DXGI_FORMAT_UNKNOWN,
            DXGI_FORMAT_UNKNOWN,
            DXGI_FORMAT_UNKNOWN,
            DXGI_FORMAT_UNKNOWN,
            DXGI_FORMAT_UNKNOWN,
            DXGI_FORMAT_UNKNOWN,
        ],
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        ..Default::default()
    };
    desc.BlendState.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
        SrcBlend: D3D12_BLEND_ONE,
        DestBlend: D3D12_BLEND_ZERO,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_ZERO,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        LogicOp: D3D12_LOGIC_OP_NOOP,
        RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
        ..Default::default()
    };
    // SAFETY: all descriptors match the shader/target and all borrowed blobs/root remain live.
    let result = unsafe {
        let result = device.CreateGraphicsPipelineState(&desc);
        ManuallyDrop::drop(&mut desc.pRootSignature);
        result
    };
    Ok((root, com(result, "graphics pipeline")?))
}

struct PendingFrame {
    serial: u64,
    seed: u32,
    submit_begin: u64,
    submit_end: u64,
    stages: [u64; 14],
}

struct Slot {
    allocator: ID3D12CommandAllocator,
    commands: ID3D12GraphicsCommandList,
    target: ID3D12Resource,
    queries: GpuQueries,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    pending: Option<PendingFrame>,
    last_seed: Option<u32>,
}

struct Pipeline<'a> {
    gpu: &'a mut Gpu,
    slots: Vec<Slot>,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    _descriptors: ID3D12DescriptorHeap,
    rounds: u32,
    draws: u32,
    expected_cpu_checksum: u64,
    command_stage_probe: bool,
}

impl<'a> Pipeline<'a> {
    fn new(gpu: &'a mut Gpu, rounds: u32, draws: u32) -> Result<Self> {
        let (root, pso) = create_pipeline(&gpu.device)?;
        // SAFETY: three descriptors are indexed using this device's RTV stride.
        let (descriptors, start, stride) = unsafe {
            let heap: ID3D12DescriptorHeap = com(
                gpu.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                        NumDescriptors: 3,
                        ..Default::default()
                    }),
                "pipeline RTV heap",
            )?;
            let start = heap.GetCPUDescriptorHandleForHeapStart();
            let stride = gpu
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
            (heap, start, stride as usize)
        };
        let mut slots = Vec::with_capacity(3);
        for index in 0..3 {
            // SAFETY: no list was submitted yet; each slot owns a distinct DIRECT allocator/list and texture with the same renderable format.
            let (allocator, commands, target, rtv) = unsafe {
                let allocator: ID3D12CommandAllocator = com(
                    gpu.device
                        .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT),
                    "frame allocator",
                )?;
                let commands: ID3D12GraphicsCommandList = com(
                    gpu.device.CreateCommandList(
                        0,
                        D3D12_COMMAND_LIST_TYPE_DIRECT,
                        &allocator,
                        &pso,
                    ),
                    "frame commands",
                )?;
                com(commands.Close(), "close initial frame commands")?;
                let target = resource(
                    &gpu.device,
                    D3D12_HEAP_TYPE_DEFAULT,
                    &gpu.target.GetDesc(),
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                )?;
                let rtv = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: start.ptr + index * stride,
                };
                gpu.device.CreateRenderTargetView(&target, None, rtv);
                (allocator, commands, target, rtv)
            };
            slots.push(Slot {
                allocator,
                commands,
                target,
                queries: GpuQueries::new(&gpu.device, &gpu.queue)?,
                rtv,
                pending: None,
                last_seed: None,
            });
        }
        Ok(Self {
            gpu,
            slots,
            root,
            pso,
            _descriptors: descriptors,
            rounds,
            draws,
            expected_cpu_checksum: 0,
            command_stage_probe: false,
        })
    }

    fn record(&self, index: usize, seed: u32, stages: &mut [u64; 14]) -> Result<u64> {
        let slot = &self.slots[index];
        if slot.pending.is_some() {
            return Err("record before slot fence retirement".into());
        }
        let before = counter()?;
        // SAFETY: retire verified this slot's original fence before clearing pending; resources, descriptors, root constants and viewport all remain live.
        unsafe {
            com(slot.allocator.Reset(), "reset frame allocator")?;
            let allocator_end = if self.command_stage_probe {
                counter()?
            } else {
                before
            };
            com(
                slot.commands.Reset(&slot.allocator, &self.pso),
                "reset frame commands",
            )?;
            let reset_end = if self.command_stage_probe {
                counter()?
            } else {
                before
            };
            slot.queries.begin(&slot.commands);
            slot.commands.SetGraphicsRootSignature(&self.root);
            let constants = [seed, self.rounds];
            slot.commands
                .SetGraphicsRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
            slot.commands.RSSetViewports(&[D3D12_VIEWPORT {
                Width: self.gpu.width as f32,
                Height: self.gpu.height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]);
            slot.commands.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: self.gpu.width as i32,
                bottom: self.gpu.height as i32,
            }]);
            slot.commands
                .OMSetRenderTargets(1, Some(&slot.rtv), false, None);
            slot.commands
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            let state_end = if self.command_stage_probe {
                counter()?
            } else {
                before
            };
            for _ in 0..self.draws {
                slot.commands.DrawInstanced(3, 1, 0, 0);
            }
            let draw_end = if self.command_stage_probe {
                counter()?
            } else {
                before
            };
            slot.queries.end(&slot.commands);
            let query_end = if self.command_stage_probe {
                counter()?
            } else {
                before
            };
            if self.command_stage_probe {
                stages[6..11].copy_from_slice(&[
                    allocator_end - before,
                    reset_end - allocator_end,
                    state_end - reset_end,
                    draw_end - state_end,
                    query_end - draw_end,
                ]);
            }
        }
        let recorded = counter()?;
        stages[0] = recorded - before;
        Ok(recorded)
    }

    fn discard(&self, seed: u32) -> Result<[u64; 14]> {
        self.discard_at(0, seed)
    }

    fn discard_at(&self, index: usize, seed: u32) -> Result<[u64; 14]> {
        let mut stages = [0; 14];
        let before = self.record(index, seed, &mut stages)?;
        // SAFETY: the owned list was reset/recorded above and is never submitted by this diagnostic path.
        let closed = unsafe { self.slots[index].commands.Close() };
        com(closed, "close discarded commands")?;
        stages[11] = counter()? - before;
        Ok(stages)
    }

    fn retirement_iteration(
        &mut self,
        order: [usize; 3],
        wakeup: RetirementWakeup,
        seed: u32,
        frequency: u64,
    ) -> Result<[(u64, [u64; 14]); 3]> {
        match wakeup {
            RetirementWakeup::Gpu => {
                self.submit(0, 0)?;
                self.retire(0)?;
            }
            RetirementWakeup::Timer | RetirementWakeup::Yield => {
                // SAFETY: pauses/yields only this owned fixture's main thread.
                unsafe {
                    Sleep(u32::from(matches!(wakeup, RetirementWakeup::Timer)));
                }
            }
            RetirementWakeup::Busy => {
                let until = counter()? + frequency / 1000;
                while counter()? < until {
                    // Deliberately avoid PAUSE: pause-loop exiting would add another hypervisor transition to this elapsed-time control.
                    core::hint::black_box(until);
                }
            }
            RetirementWakeup::Cpuid => {
                // CPUID leaf zero is an unprivileged x86-64 baseline instruction.
                core::hint::black_box(core::arch::x86_64::__cpuid(0));
            }
        }
        let mut result = [(0, [0; 14]); 3];
        for (position, index) in order.into_iter().enumerate() {
            let before = counter()?;
            let stages = self.discard_at(index, seed.wrapping_add(position as u32))?;
            result[position] = (counter()? - before, stages);
        }
        Ok(result)
    }

    fn retirement_probe(&mut self, config: &Config, frequency: u64) -> Result<()> {
        let mut cases = Vec::with_capacity(6);
        for (name, order, wakeup) in [
            ("submitted_first", [0, 1, 1], RetirementWakeup::Gpu),
            ("other_first", [1, 0, 1], RetirementWakeup::Gpu),
            ("timer_only", [0, 1, 1], RetirementWakeup::Timer),
            ("yield_only", [0, 1, 1], RetirementWakeup::Yield),
            ("busy_only", [0, 1, 1], RetirementWakeup::Busy),
            ("cpuid_only", [0, 1, 1], RetirementWakeup::Cpuid),
        ] {
            self.gpu.wait()?;
            let warm_end = counter()? + frequency * u64::from(config.warmup_ms) / 1000;
            let mut seed = 0xd000_0000_u32;
            while counter()? < warm_end {
                core::hint::black_box(self.retirement_iteration(order, wakeup, seed, frequency)?);
                seed = seed.wrapping_add(3);
            }
            let mut samples = Vec::with_capacity(CAPACITY);
            let fence_before = self.gpu.completed();
            log(&format!("LEGION_CPU_PROFILE_BEGIN retirement_{name}"));
            let start = counter()?;
            let deadline = start + frequency * u64::from(config.sample_ms) / 1000;
            let mut last = start;
            while last < deadline {
                let sample = core::hint::black_box(
                    self.retirement_iteration(order, wakeup, seed, frequency)?,
                );
                seed = seed.wrapping_add(3);
                last = counter()?;
                if samples.len() == samples.capacity() {
                    return Err("retirement sample cap exceeded".into());
                }
                samples.push(sample);
            }
            log(&format!("LEGION_CPU_PROFILE_END retirement_{name}"));
            let fence_after = self.gpu.completed();
            let expected_signals = if matches!(wakeup, RetirementWakeup::Gpu) {
                samples.len() as u64
            } else {
                0
            };
            if fence_after.checked_sub(fence_before) != Some(expected_signals) {
                return Err(
                    "unexpected original fence progression in retirement diagnostic".into(),
                );
            }
            let mut roles = Vec::with_capacity(3);
            for position in 0..3 {
                let mut durations: Vec<_> = samples.iter().map(|s| s[position].0).collect();
                let mut stages = alloc::collections::BTreeMap::new();
                for (index, label) in [
                    (0, "record"),
                    (11, "close_list"),
                    (6, "allocator_reset"),
                    (7, "list_reset"),
                    (8, "state_commands"),
                    (9, "draw_commands"),
                    (10, "resolve_queries"),
                ] {
                    let mut ticks: Vec<_> = samples.iter().map(|s| s[position].1[index]).collect();
                    stages.insert(
                        label,
                        timing_summary(&mut ticks, frequency)
                            .ok_or("no retirement stage samples")?,
                    );
                }
                roles.push(serde_json::json!({"position":position,"slot":order[position],"stages":stages,
                    "timing":timing_summary(&mut durations,frequency).ok_or("no retirement timing")?}));
            }
            let images = [self.verify_slot(0)?, self.verify_slot(1)?];
            cases.push(serde_json::json!({"mode":name,"iterations":samples.len(),"roles":roles,
                "timed_gpu_submissions":expected_signals,"fence_before":fence_before,"fence_after":fence_after,
                "elapsed_ms":(last-start) as f64*1000.0/frequency as f64,"images":images}));
        }
        let output = open("Z:\\frames\\retirement-probe.json", true)?;
        write_handle(
            output.0,
            &serde_json::to_vec_pretty(&serde_json::json!({"schema":1,
            "diagnostic_only":true,"qpc_frequency":frequency,"cases":cases}))
            .map_err(|e| e.to_string())?,
        )
    }

    fn recording_probe(&mut self, config: &Config, frequency: u64) -> Result<()> {
        self.gpu.wait()?;
        let original_draws = self.draws;
        let mut reports = Vec::with_capacity(2);
        for draws in [1, 64] {
            self.draws = draws;
            let mut seed = 0xe000_0000_u32;
            let warm_end = counter()? + frequency * u64::from(config.warmup_ms) / 1000;
            while counter()? < warm_end {
                core::hint::black_box(self.discard(seed)?);
                seed = seed.wrapping_add(1);
            }
            let mut durations = Vec::with_capacity(1_000_000);
            let mut samples = Vec::with_capacity(1_000_000);
            let fence_before = self.gpu.completed();
            let name = format!("record_discard_draws{draws}");
            log(&format!("LEGION_CPU_PROFILE_BEGIN {name}"));
            let start = counter()?;
            let deadline = start + frequency * u64::from(config.sample_ms) / 1000;
            let mut last = start;
            while last < deadline {
                let before = counter()?;
                let stages = core::hint::black_box(self.discard(seed)?);
                seed = seed.wrapping_add(1);
                last = counter()?;
                if samples.len() == samples.capacity() {
                    return Err("discard diagnostic sample capacity exceeded".into());
                }
                durations.push(last - before);
                samples.push(stages);
            }
            log(&format!("LEGION_CPU_PROFILE_END {name}"));
            let fence_after = self.gpu.completed();
            if fence_before != fence_after {
                return Err("original GPU fence changed while discarding commands".into());
            }
            let mut stages = alloc::collections::BTreeMap::new();
            for (index, label) in [
                (0, "record"),
                (11, "close_list"),
                (6, "allocator_reset"),
                (7, "list_reset"),
                (8, "state_commands"),
                (9, "draw_commands"),
                (10, "resolve_queries"),
            ] {
                let mut ticks: Vec<_> = samples.iter().map(|s| s[index]).collect();
                stages.insert(
                    label,
                    timing_summary(&mut ticks, frequency).ok_or("no discard stage samples")?,
                );
            }
            let image = self.verify_slot(0)?;
            reports.push(serde_json::json!({"mode":name,"draws":draws,"iterations":samples.len(),
                "elapsed_ms":(last-start) as f64*1000.0/frequency as f64,
                "timing":timing_summary(&mut durations,frequency).ok_or("no discarded command samples")?,
                "stages":stages,"timed_gpu_submissions":0,"fence_before":fence_before,
                "fence_after":fence_after,"unchanged_image":image}));
        }
        self.draws = original_draws;
        let output = open("Z:\\frames\\recording-only.json", true)?;
        write_handle(output.0,&serde_json::to_vec_pretty(&serde_json::json!({"schema":1,"diagnostic_only":true,
            "workload":"record_close_discard_without_submission","qpc_frequency":frequency,"cases":reports}))
            .map_err(|e|e.to_string())?)
    }

    fn submit(&mut self, index: usize, cpu_rounds: u32) -> Result<()> {
        if self.slots[index].pending.is_some() {
            return Err("attempted reuse before original frame fence".into());
        }
        let serial = self
            .gpu
            .serial
            .checked_add(1)
            .ok_or("fence serial overflow")?;
        let seed = serial as u32;
        let mut stages = [0; 14];
        let before = counter()?;
        let checksum = if cpu_rounds != 0 {
            core::hint::black_box(cpu_work(core::hint::black_box(cpu_rounds)))
        } else {
            0
        };
        stages[2] = counter()? - before;
        if cpu_rounds != 0 && checksum != self.expected_cpu_checksum {
            return Err("render CPU checksum changed".into());
        }
        let submit_begin = self.record(index, seed, &mut stages)?;
        let slot = &mut self.slots[index];
        // SAFETY: recorded DIRECT list and monotonically increasing original fence belong to the same device/queue.
        unsafe {
            com(slot.commands.Close(), "close rendered frame")?;
            let close_end = if self.command_stage_probe {
                counter()?
            } else {
                submit_begin
            };
            self.gpu
                .queue
                .ExecuteCommandLists(&[Some(com(slot.commands.cast(), "frame command cast")?)]);
            let execute_end = if self.command_stage_probe {
                counter()?
            } else {
                submit_begin
            };
            if self.command_stage_probe {
                stages[11] = close_end - submit_begin;
                stages[12] = execute_end - close_end;
            }
        }
        self.gpu.serial = serial;
        // Record ownership before any fallible signal, so Drop retains in-flight resources.
        slot.pending = Some(PendingFrame {
            serial,
            seed,
            submit_begin,
            submit_end: submit_begin,
            stages,
        });
        // SAFETY: live queue and its shared fence; serial increases for every submission.
        let signaled = unsafe { self.gpu.queue.Signal(&self.gpu.fence, serial) };
        com(signaled, "rendered frame signal")?;
        let end = counter()?;
        let frame = slot.pending.as_mut().ok_or("missing submitted frame")?;
        frame.submit_end = end;
        frame.stages[1] = end - submit_begin;
        if self.command_stage_probe {
            frame.stages[13] = end - submit_begin - frame.stages[11] - frame.stages[12];
        }
        Ok(())
    }

    fn retire(&mut self, index: usize) -> Result<(GpuObservation, [u64; 14])> {
        let frame = self.slots[index]
            .pending
            .as_ref()
            .ok_or("retire without submission")?;
        let wait = self.gpu.wait_for(frame.serial)?;
        let verified = counter()?;
        let frame = self.slots[index]
            .pending
            .take()
            .ok_or("missing retired frame")?;
        let [begin, end] = self.slots[index].queries.read_after_fence();
        if end <= begin {
            return Err("rendered frame GPU timestamps did not advance".into());
        }
        self.slots[index].last_seed = Some(frame.seed);
        let mut stages = frame.stages;
        stages[3..6].copy_from_slice(&wait);
        Ok((
            GpuObservation {
                fence: frame.serial,
                gpu_begin_ticks: begin,
                gpu_end_ticks: end,
                submit_begin_qpc: frame.submit_begin,
                submit_end_qpc: frame.submit_end,
                fence_verified_qpc: verified,
            },
            stages,
        ))
    }

    fn verify_slot(&mut self, index: usize) -> Result<RenderImageCheck> {
        let slot = &self.slots[index];
        if slot.pending.is_some() {
            return Err("verification before frame fence".into());
        }
        let seed = slot.last_seed.ok_or("verification of unused slot")?;
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0;
        // SAFETY: live render target and initialized footprint/size outputs.
        unsafe {
            self.gpu.device.GetCopyableFootprints(
                &slot.target.GetDesc(),
                0,
                1,
                0,
                Some(&mut footprint),
                None,
                None,
                Some(&mut total),
            );
        }
        let readback = resource(
            &self.gpu.device,
            D3D12_HEAP_TYPE_READBACK,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: total,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_COPY_DEST,
        )?;
        self.gpu.reset()?;
        transition(
            &self.gpu.commands,
            &slot.target,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        let mut dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(readback.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let mut src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(slot.target.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        // SAFETY: both locations describe live resources in compatible COPY states; manually retained COM pointers are released exactly once after recording.
        unsafe {
            self.gpu
                .commands
                .CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            ManuallyDrop::drop(&mut src.pResource);
            ManuallyDrop::drop(&mut dst.pResource);
        }
        transition(
            &self.gpu.commands,
            &slot.target,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        self.gpu.submit()?;
        self.gpu.wait()?;
        let len = usize::try_from(total).map_err(|_| "shader readback too large")?;
        let mut pointer = ptr::null_mut();
        // SAFETY: the original copy fence completed; the mapped range covers the footprint.
        let check = unsafe {
            com(
                readback.Map(
                    0,
                    Some(&D3D12_RANGE { Begin: 0, End: len }),
                    Some(&mut pointer),
                ),
                "map shader image",
            )?;
            if pointer.is_null() {
                return Err("null shader image mapping".into());
            }
            let check = verify_shader(
                core::slice::from_raw_parts(pointer.cast::<u8>(), len),
                footprint.Footprint.RowPitch as usize,
                self.gpu.width,
                self.gpu.height,
                seed,
                self.rounds,
            );
            readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            check
        };
        check.ok_or_else(|| format!("shader image mismatch in slot {index}, seed {seed}"))
    }

    fn measure(
        &mut self,
        config: &Config,
        depth: usize,
        mixed: bool,
        frequency: u64,
    ) -> Result<Sample> {
        let cpu_rounds = if mixed { config.cpu_rounds } else { 0 };
        self.expected_cpu_checksum = cpu_work(config.cpu_rounds);
        let warm_end = counter()? + frequency * u64::from(config.warmup_ms) / 1000;
        let mut cursor = 0;
        while counter()? < warm_end {
            if self.slots[cursor].pending.is_some() {
                self.retire(cursor)?;
            }
            self.submit(cursor, cpu_rounds)?;
            cursor = (cursor + 1) % depth;
        }
        for offset in 0..depth {
            let i = (cursor + offset) % depth;
            if self.slots[i].pending.is_some() {
                self.retire(i)?;
            }
        }
        let mut image_checks = Vec::with_capacity(depth * 2);
        // Independently prime every slot, including exceptionally short warmups.
        for i in 0..depth {
            self.submit(i, 0)?;
            self.retire(i)?;
            image_checks.push(self.verify_slot(i)?);
        }
        let clock_before = self.gpu.calibrate()?;
        let mut observations = Vec::with_capacity(CAPACITY);
        let mut samples = Vec::with_capacity(CAPACITY);
        let mut cadence = Vec::with_capacity(CAPACITY);
        let profile_name = format!("{}_depth{depth}", if mixed { "mixed" } else { "d3d12" });
        if config.diagnostic_cpu_profile {
            log(&format!("LEGION_CPU_PROFILE_BEGIN {profile_name}"));
        }
        let start = counter()?;
        let deadline = start + frequency * u64::from(config.sample_ms) / 1000;
        let mut last_retire = start;
        let mut outstanding = 0;
        let mut maximum_outstanding = 0;
        cursor = 0;
        while counter()? < deadline {
            if self.slots[cursor].pending.is_some() {
                let (observation, stages) = self.retire(cursor)?;
                let now = counter()?;
                cadence.push(now - last_retire);
                last_retire = now;
                observations.push(observation);
                samples.push(stages);
                outstanding -= 1;
            }
            if observations.len() + outstanding >= CAPACITY {
                return Err("render sample capacity exceeded".into());
            }
            self.submit(cursor, cpu_rounds)?;
            outstanding += 1;
            maximum_outstanding = maximum_outstanding.max(outstanding);
            cursor = (cursor + 1) % depth;
        }
        // Retire every submitted frame in queue order, including the final tail.
        for offset in 0..depth {
            let i = (cursor + offset) % depth;
            if self.slots[i].pending.is_some() {
                let (observation, stages) = self.retire(i)?;
                let now = counter()?;
                cadence.push(now - last_retire);
                last_retire = now;
                observations.push(observation);
                samples.push(stages);
            }
        }
        let elapsed = last_retire - start;
        if config.diagnostic_cpu_profile {
            log(&format!("LEGION_CPU_PROFILE_END {profile_name}"));
        }
        let clock_after = self.gpu.calibrate()?;
        let gpu_frequency = self.slots[0].queries.frequency;
        let timing = summarize_gpu(
            &observations,
            gpu_frequency,
            frequency,
            clock_before,
            clock_after,
        )
        .ok_or("invalid pipelined GPU observations")?;
        let mut gaps = Vec::with_capacity(observations.len().saturating_sub(1));
        for pair in observations.windows(2) {
            gaps.push(
                pair[1]
                    .gpu_begin_ticks
                    .checked_sub(pair[0].gpu_end_ticks)
                    .ok_or("overlapping or reversed DIRECT queue timestamps")?,
            );
        }
        let active: u128 = observations
            .iter()
            .map(|s| u128::from(s.gpu_end_ticks - s.gpu_begin_ticks))
            .sum();
        let span = observations
            .last()
            .ok_or("no completed rendered frames")?
            .gpu_end_ticks
            - observations[0].gpu_begin_ticks;
        let mut latencies: Vec<u64> = observations
            .iter()
            .map(|s| s.fence_verified_qpc - s.submit_begin_qpc)
            .collect();
        for i in 0..depth {
            image_checks.push(self.verify_slot(i)?);
        }
        let mut stages = alloc::collections::BTreeMap::new();
        for (index, name) in [
            "record",
            "submit",
            "cpu_work",
            "fence_query",
            "fence_arm",
            "event_wait",
            "allocator_reset",
            "list_reset",
            "state_commands",
            "draw_commands",
            "resolve_queries",
            "close_list",
            "execute_lists",
            "signal_fence",
        ]
        .into_iter()
        .enumerate()
        {
            if index >= 6 && !self.command_stage_probe {
                break;
            }
            let mut values: Vec<u64> = samples.iter().map(|s| s[index]).collect();
            stages.insert(
                name.into(),
                timing_summary(&mut values, frequency).ok_or("no render stage samples")?,
            );
        }
        let cadence =
            timing_summary(&mut cadence, frequency).ok_or("no completed frame cadence")?;
        Ok(Sample {
            mode: format!("{}_depth{depth}", if mixed { "mixed" } else { "d3d12" }),
            iterations: observations.len() as u64,
            elapsed_ms: elapsed as f64 * 1000.0 / frequency as f64,
            iterations_per_second: observations.len() as f64 * frequency as f64 / elapsed as f64,
            p50_us: cadence.p50_us,
            p95_us: cadence.p95_us,
            p99_us: cadence.p99_us,
            cpu_checksum: if mixed { cpu_work(cpu_rounds) } else { 0 },
            stages,
            gpu: Some(timing),
            pipeline: Some(PipelineTiming {
                frames_in_flight: depth,
                maximum_outstanding,
                completed_frames: observations.len() as u64,
                image_checks,
                retirement_latency: timing_summary(&mut latencies, frequency)
                    .ok_or("no retirement samples")?,
                gpu_gap: timing_summary(&mut gaps, gpu_frequency)
                    .ok_or("no adjacent GPU timestamps")?,
                gpu_active_fraction: active as f64 / span as f64,
            }),
        })
    }
}

impl Drop for Pipeline<'_> {
    fn drop(&mut self) {
        if let Some(serial) = self
            .slots
            .iter()
            .filter_map(|s| s.pending.as_ref().map(|f| f.serial))
            .max()
            && self.gpu.wait_for(serial).is_err()
        {
            // A device failure must not release resources still referenced by submitted work.
            core::mem::forget(core::mem::take(&mut self.slots));
            core::mem::forget(self.root.clone());
            core::mem::forget(self.pso.clone());
            core::mem::forget(self._descriptors.clone());
        }
    }
}

pub(super) fn measure_all(gpu: &mut Gpu, config: &Config, frequency: u64) -> Result<Vec<Sample>> {
    let rounds = config
        .pipeline
        .as_ref()
        .ok_or("missing shader configuration")?
        .shader_rounds;
    let draws = config
        .pipeline
        .as_ref()
        .ok_or("missing draw configuration")?
        .draws_per_frame;
    let mut pipeline = Pipeline::new(gpu, rounds, draws)?;
    pipeline.command_stage_probe = config
        .pipeline
        .as_ref()
        .is_some_and(|p| p.command_stage_probe);
    let mut samples = Vec::with_capacity(6);
    for depth in 1..=3 {
        for mixed in [false, true] {
            log(&format!("stage: render depth {depth}, mixed={mixed}"));
            samples.push(pipeline.measure(config, depth, mixed, frequency)?);
            let progress = open("Z:\\frames\\render-progress.json", true)?;
            write_handle(
                progress.0,
                &serde_json::to_vec_pretty(&samples).map_err(|e| e.to_string())?,
            )?;
        }
    }
    if config.pipeline.as_ref().is_some_and(|p| p.recording_probe) {
        pipeline.recording_probe(config, frequency)?;
    }
    if config.pipeline.as_ref().is_some_and(|p| p.retirement_probe) {
        pipeline.retirement_probe(config, frequency)?;
    }
    Ok(samples)
}
