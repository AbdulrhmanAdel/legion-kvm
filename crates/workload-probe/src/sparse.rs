use anyhow::{Context, Result, ensure};
use ash::{Entry, vk};
use serde_json::json;
use std::{ffi::CStr, mem::ManuallyDrop, path::Path};
mod sparse_upload;

struct Probe {
    entry: ManuallyDrop<Entry>,
    instance: ash::Instance,
    device: Option<ash::Device>,
    buffers: Vec<vk::Buffer>,
    memory: Vec<vk::DeviceMemory>,
    fences: Vec<vk::Fence>,
    pools: Vec<vk::CommandPool>,
    inflight: bool,
}

impl Drop for Probe {
    fn drop(&mut self) {
        // On failed submission/wait, retain handles and the loaded library until process teardown rather than destroying resources still in use.
        if self.inflight {
            return;
        }
        // SAFETY: every successful submission has completed its fence; this owner uniquely owns all handles, and loader/instance outlive device.
        unsafe {
            if let Some(device) = &self.device {
                for pool in &self.pools {
                    device.destroy_command_pool(*pool, None);
                }
                for fence in &self.fences {
                    device.destroy_fence(*fence, None);
                }
                for buffer in &self.buffers {
                    device.destroy_buffer(*buffer, None);
                }
                for memory in &self.memory {
                    device.free_memory(*memory, None);
                }
                device.destroy_device(None);
            }
            self.instance.destroy_instance(None);
            ManuallyDrop::drop(&mut self.entry);
        }
    }
}

impl Probe {
    fn new() -> Result<Self> {
        // SAFETY: the owner retains the dynamically loaded entry through all device calls, including process teardown after a failed GPU wait.
        let entry = unsafe { Entry::load() }.context("load Vulkan")?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"legion-sparse-probe")
            .api_version(vk::API_VERSION_1_1);
        // SAFETY: the descriptor borrows live stack data for this synchronous call.
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
        }?;
        Ok(Self {
            entry: ManuallyDrop::new(entry),
            instance,
            device: None,
            buffers: Vec::new(),
            memory: Vec::new(),
            fences: Vec::new(),
            pools: Vec::new(),
            inflight: false,
        })
    }

    fn exercise(&mut self) -> Result<serde_json::Value> {
        // SAFETY: live instance; returned physical handles remain owned by it.
        let physicals = unsafe { self.instance.enumerate_physical_devices() }?;
        let mut chosen = None;
        for physical in physicals {
            // SAFETY: the enumerated handle belongs to the live instance.
            let props = unsafe { self.instance.get_physical_device_properties(physical) };
            if matches!(
                props.device_type,
                vk::PhysicalDeviceType::DISCRETE_GPU | vk::PhysicalDeviceType::INTEGRATED_GPU
            ) {
                ensure!(chosen.is_none(), "ambiguous hardware GPU selection");
                chosen = Some((physical, props));
            }
        }
        let (physical, props) = chosen.context("no hardware GPU; no software fallback")?;
        // SAFETY: immutable queries against the live physical device.
        let (features, families, memory) = unsafe {
            (
                self.instance.get_physical_device_features(physical),
                self.instance
                    .get_physical_device_queue_family_properties(physical),
                self.instance
                    .get_physical_device_memory_properties(physical),
            )
        };
        ensure!(
            features.sparse_binding != 0
                && features.sparse_residency_buffer != 0
                && features.sparse_residency_aliased != 0,
            "required sparse-buffer features unavailable"
        );
        let family = families
            .iter()
            .position(|q| {
                q.queue_flags
                    .contains(vk::QueueFlags::SPARSE_BINDING | vk::QueueFlags::TRANSFER)
            })
            .context("no sparse transfer queue")?;
        let priorities = [1.0];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(u32::try_from(family)?)
            .queue_priorities(&priorities)];
        let enabled = vk::PhysicalDeviceFeatures::default()
            .sparse_binding(true)
            .sparse_residency_buffer(true)
            .sparse_residency_aliased(true);
        // SAFETY: required features were queried; descriptors and queue data remain live throughout device creation; no custom allocator is used.
        let device = unsafe {
            self.instance.create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queues)
                    .enabled_features(&enabled),
                None,
            )
        }?;
        self.device = Some(device);
        let device = self.device.as_ref().context("missing device")?.clone();
        // SAFETY: queue zero was requested in this live device's family.
        let queue = unsafe { device.get_device_queue(u32::try_from(family)?, 0) };
        let uploads = self.verify_uploads(queue, u32::try_from(family)?, &memory)?;
        let mut cases = Vec::new();
        for count in [1_u64, 5000] {
            // A large virtual range with one shared physical page is a legal sparse alias test; no shader or transfer accesses the aliases.
            let virtual_size = count.checked_mul(65536).context("virtual size overflow")?;
            let info = vk::BufferCreateInfo::default()
                .size(virtual_size)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
                .flags(
                    vk::BufferCreateFlags::SPARSE_BINDING
                        | vk::BufferCreateFlags::SPARSE_RESIDENCY
                        | vk::BufferCreateFlags::SPARSE_ALIASED,
                );
            // SAFETY: features support these creation flags; descriptor is live.
            let buffer = unsafe { device.create_buffer(&info, None) }?;
            self.buffers.push(buffer);
            // SAFETY: buffer belongs to this device and remains live.
            let req = unsafe { device.get_buffer_memory_requirements(buffer) };
            ensure!(
                req.alignment > 0 && req.alignment <= 4 * 1024 * 1024,
                "unexpected sparse page alignment"
            );
            let memory_type = (0..memory.memory_type_count)
                .find(|&i| {
                    req.memory_type_bits & (1 << i) != 0
                        && memory.memory_types[i as usize]
                            .property_flags
                            .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                })
                .context("no device-local sparse memory type")?;
            // SAFETY: memory type is allowed by the buffer; only one physical sparse page is allocated, bounded to 4 MiB above.
            let allocation = unsafe {
                device.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.alignment)
                        .memory_type_index(memory_type),
                    None,
                )
            }?;
            self.memory.push(allocation);
            let pages = virtual_size.div_ceil(req.alignment);
            let binds: Vec<_> = (0..pages)
                .map(|i| {
                    vk::SparseMemoryBind::default()
                        .resource_offset(i * req.alignment)
                        .size(req.alignment.min(virtual_size - i * req.alignment))
                        .memory(allocation)
                        .memory_offset(0)
                })
                .collect();
            for unbind in [false, true] {
                let unbound: Vec<_> = if unbind {
                    binds
                        .iter()
                        .map(|b| vk::SparseMemoryBind {
                            memory: vk::DeviceMemory::null(),
                            ..*b
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let mapping = [vk::SparseBufferMemoryBindInfo::default()
                    .buffer(buffer)
                    .binds(if unbind { &unbound } else { &binds })];
                let submit = [vk::BindSparseInfo::default().buffer_binds(&mapping)];
                // SAFETY: fence descriptor is initialized and belongs to device.
                let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;
                self.fences.push(fence);
                self.inflight = true;
                println!("SPARSE_STAGE pages={pages} unbind={unbind} submit");
                // SAFETY: sparse queue is owned, all buffers/memory/descriptors are live and aliasing is enabled.
                unsafe { device.queue_bind_sparse(queue, &submit, fence) }
                    .with_context(|| format!("sparse submission pages={pages} unbind={unbind}"))?;
                // SAFETY: owned fence and device remain live; finite 5 s wait.
                unsafe { device.wait_for_fences(&[fence], true, 5_000_000_000) }
                    .with_context(|| format!("sparse completion pages={pages} unbind={unbind}"))?;
                self.inflight = false;
                println!("SPARSE_STAGE pages={pages} unbind={unbind} complete");
                if count == 1 && !unbind {
                    self.verify_page(queue, u32::try_from(family)?, &memory, buffer)?;
                }
            }
            cases.push(json!({"pages":pages,"virtual_bytes":virtual_size,"physical_bytes":req.alignment,"bind_and_unbind_completed":true}));
        }
        // SAFETY: Vulkan specifies a NUL-terminated fixed device-name array.
        let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy();
        Ok(
            json!({"schema":1,"passed":true,"gpu_verified":false,"gpu_transfer_verified":true,
            "scope":"sparse binding/unbinding, fence completion and GPU transfer readback; no rendered image claim",
            "gpu":name,"cases":cases,"cpu_uploads":uploads}),
        )
    }

    fn verify_page(
        &mut self,
        queue: vk::Queue,
        family: u32,
        properties: &vk::PhysicalDeviceMemoryProperties,
        source: vk::Buffer,
    ) -> Result<()> {
        const BYTES: u64 = 4096;
        const PATTERN: u32 = 0x51a7_c0de;
        let device = self.device.as_ref().context("missing device")?;
        // SAFETY: descriptors use live data and the enabled transfer queue; all resulting handles are immediately retained by this owner.
        let (buffer, pool) = unsafe {
            let buffer = device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(BYTES)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )?;
            self.buffers.push(buffer);
            let pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(family),
                None,
            )?;
            self.pools.push(pool);
            (buffer, pool)
        };
        // SAFETY: the created buffer belongs to this live device.
        let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        ensure!(
            requirements.size <= 16 * 1024 * 1024,
            "readback allocation too large"
        );
        let index = (0..properties.memory_type_count)
            .find(|&i| {
                requirements.memory_type_bits & (1 << i) != 0
                    && properties.memory_types[i as usize].property_flags.contains(
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )
            })
            .context("no coherent readback memory")?;
        // SAFETY: type and size satisfy this buffer's queried requirements.
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(index),
                None,
            )
        }?;
        self.memory.push(memory);
        // SAFETY: uniquely owned, compatible buffer/memory at aligned offset 0.
        unsafe { device.bind_buffer_memory(buffer, memory, 0) }?;
        // SAFETY: pool belongs to the selected transfer queue family.
        let command = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }?[0];
        // SAFETY: this fresh command buffer is recorded once, on one thread.
        unsafe {
            device.begin_command_buffer(command, &vk::CommandBufferBeginInfo::default())?;
            device.cmd_fill_buffer(command, source, 0, BYTES, PATTERN);
            device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_READ)],
                &[],
                &[],
            );
            device.cmd_copy_buffer(
                command,
                source,
                buffer,
                &[vk::BufferCopy::default().size(BYTES)],
            );
            device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::HOST_READ)],
                &[],
                &[],
            );
            device.end_command_buffer(command)?;
        }
        // SAFETY: fence descriptor is initialized; this owner retains the fence.
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;
        self.fences.push(fence);
        self.inflight = true;
        println!("SPARSE_TRANSFER submit");
        let commands = [command];
        // SAFETY: queue access is serialized; all referenced resources remain owned until completion, or are retained until process teardown.
        unsafe {
            device.queue_submit(
                queue,
                &[vk::SubmitInfo::default().command_buffers(&commands)],
                fence,
            )?;
            device.wait_for_fences(&[fence], true, 5_000_000_000)?;
        }
        self.inflight = false;
        // SAFETY: GPU writes have completed with a host-read barrier, memory is coherent and host visible, and this range is within the allocation.
        let pointer = unsafe { device.map_memory(memory, 0, BYTES, vk::MemoryMapFlags::empty()) }?;
        // SAFETY: the mapped 4096-byte range is initialized by the completed copy, aligned to u32, and remains mapped throughout this read.
        let matched = unsafe { std::slice::from_raw_parts(pointer.cast::<u32>(), 1024) }
            .iter()
            .all(|&word| word == PATTERN);
        // SAFETY: uniquely owned mapping; no references are used after unmap.
        unsafe { device.unmap_memory(memory) };
        ensure!(matched, "sparse GPU readback pattern mismatch");
        println!("SPARSE_TRANSFER readback_verified bytes={BYTES}");
        Ok(())
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 2,
        "expected configuration and new result path"
    );
    let path = Path::new(&args[1]);
    ensure!(!path.exists(), "result must not exist");
    let mut probe = Probe::new()?;
    let result = probe.exercise();
    let report = match &result {
        Ok(value) => value.clone(),
        Err(error) => {
            json!({"schema":1,"passed":false,"gpu_verified":false,"error":format!("{error:#}")})
        }
    };
    std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    result.map(|_| ())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("sparse probe failed: {error:#}");
        std::process::exit(1);
    }
}
