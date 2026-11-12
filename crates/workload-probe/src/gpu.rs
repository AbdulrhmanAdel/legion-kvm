//! Offscreen graphics with three owned in-flight slots and GPU timestamps.
use anyhow::{Context, Result, ensure};
use ash::{Entry, vk};
use serde_json::{Value, json};
use std::ptr::NonNull;

const SLOTS: usize = 3;
const VERIFY_SEED: u32 = 0x1357_9bdf;

#[derive(Default)]
struct Slot {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    command: vk::CommandBuffer,
    fence: vk::Fence,
    inflight: bool,
}

pub struct Gpu {
    _entry: Entry,
    instance: ash::Instance,
    device: Option<ash::Device>,
    queue: vk::Queue,
    pool: vk::CommandPool,
    queries: vk::QueryPool,
    render_pass: vk::RenderPass,
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    shaders: Vec<vk::ShaderModule>,
    slots: Vec<Slot>,
    next: usize,
    readback: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    mapped: Option<NonNull<u8>>,
    width: u32,
    height: u32,
    iterations: u32,
    timestamp_period: f64,
    timestamp_bits: u32,
    pub info: Value,
}

impl Gpu {
    pub fn new(width: u32, height: u32, iterations: u32) -> Result<Self> {
        // SAFETY: Entry owns the loaded Vulkan library until after the instance and all device resources have been destroyed.
        let entry = unsafe { Entry::load()? };
        let app = vk::ApplicationInfo::default()
            .application_name(c"Legion CPU/GPU workload")
            .api_version(vk::API_VERSION_1_1);
        // SAFETY: all referenced create-info data outlives this call; no callbacks.
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )?
        };
        let mut gpu = Self {
            _entry: entry,
            instance,
            device: None,
            queue: vk::Queue::null(),
            pool: vk::CommandPool::null(),
            queries: vk::QueryPool::null(),
            render_pass: vk::RenderPass::null(),
            pipeline: vk::Pipeline::null(),
            layout: vk::PipelineLayout::null(),
            shaders: Vec::new(),
            slots: Vec::new(),
            next: 0,
            readback: vk::Buffer::null(),
            readback_memory: vk::DeviceMemory::null(),
            mapped: None,
            width,
            height,
            iterations,
            timestamp_period: 0.0,
            timestamp_bits: 0,
            info: Value::Null,
        };
        gpu.initialize()?;
        Ok(gpu)
    }

    fn initialize(&mut self) -> Result<()> {
        // SAFETY: this instance is live and owns the returned physical handles.
        let devices = unsafe { self.instance.enumerate_physical_devices()? };
        let pd = devices
            .into_iter()
            .find(|&pd| {
                // SAFETY: pd was enumerated from this live instance.
                let p = unsafe { self.instance.get_physical_device_properties(pd) };
                matches!(
                    p.device_type,
                    vk::PhysicalDeviceType::DISCRETE_GPU | vk::PhysicalDeviceType::INTEGRATED_GPU
                )
            })
            .context("no hardware Vulkan device; refusing software fallback")?;
        // SAFETY: pd is live for the lifetime of this instance.
        let props = unsafe { self.instance.get_physical_device_properties(pd) };
        // SAFETY: same live physical device; returns owned queue descriptions.
        let families = unsafe {
            self.instance
                .get_physical_device_queue_family_properties(pd)
        };
        let family = families
            .iter()
            .position(|q| {
                q.queue_flags.contains(vk::QueueFlags::GRAPHICS) && q.timestamp_valid_bits > 0
            })
            .context("no timestamp-capable graphics queue")?;
        self.timestamp_bits = families[family].timestamp_valid_bits;
        self.timestamp_period = f64::from(props.limits.timestamp_period);
        ensure!(
            self.timestamp_period > 0.0,
            "invalid Vulkan timestamp period"
        );
        self.info = json!({"name":props.device_name_as_c_str()?.to_string_lossy(),"vendor_id":props.vendor_id,"device_id":props.device_id,"driver_version":props.driver_version,"api_version":props.api_version,"timestamp_period_ns":self.timestamp_period,"timestamp_valid_bits":self.timestamp_bits,"frames_in_flight":SLOTS});
        let priorities = [1.0];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family as u32)
            .queue_priorities(&priorities)];
        // SAFETY: the family supports graphics, queue zero exists, and all backing references remain live throughout creation.
        let device = unsafe {
            self.instance.create_device(
                pd,
                &vk::DeviceCreateInfo::default().queue_create_infos(&queues),
                None,
            )?
        };
        self.device = Some(device);
        let d = self.device.as_ref().expect("device was installed");
        // SAFETY: queue zero was requested from this device/family.
        self.queue = unsafe { d.get_device_queue(family as u32, 0) };
        // SAFETY: valid family; only this owner records/resets its command buffers.
        self.pool = unsafe {
            d.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(family as u32)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?
        };
        // SAFETY: six timestamp queries cover two non-overlapping indices per slot.
        self.queries = unsafe {
            d.create_query_pool(
                &vk::QueryPoolCreateInfo::default()
                    .query_type(vk::QueryType::TIMESTAMP)
                    .query_count((SLOTS * 2) as u32),
                None,
            )?
        };
        let attachment = [vk::AttachmentDescription::default()
            .format(vk::Format::R8G8B8A8_UNORM)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
        let color = [vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        }];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color)];
        let dependencies = [
            vk::SubpassDependency::default()
                .src_subpass(vk::SUBPASS_EXTERNAL)
                .dst_subpass(0)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
            vk::SubpassDependency::default()
                .src_subpass(0)
                .dst_subpass(vk::SUBPASS_EXTERNAL)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
        ];
        // SAFETY: attachment/subpass references are valid for this call.
        self.render_pass = unsafe {
            d.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachment)
                    .subpasses(&subpasses)
                    .dependencies(&dependencies),
                None,
            )?
        };
        let range = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::FRAGMENT,
            offset: 0,
            size: 8,
        }];
        // SAFETY: an eight-byte scalar push-constant block is within core limits.
        self.layout = unsafe {
            d.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&range),
                None,
            )?
        };
        for data in [
            include_bytes!("../shaders/workload.vert.spv").as_slice(),
            include_bytes!("../shaders/workload.frag.spv").as_slice(),
        ] {
            let code = ash::util::read_spv(&mut std::io::Cursor::new(data))?;
            // SAFETY: owned aligned SPIR-V words were decoded from the included shaders and remain alive throughout module creation.
            self.shaders.push(unsafe {
                d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)?
            });
        }
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.shaders[0])
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.shaders[1])
                .name(c"main"),
        ];
        let input = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: self.width as f32,
            height: self.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: self.width,
                height: self.height,
            },
        }];
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewports(&viewports)
            .scissors(&scissors);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .line_width(1.0);
        let msaa = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let colors = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&colors);
        let pipelines = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&msaa)
            .color_blend_state(&blend)
            .layout(self.layout)
            .render_pass(self.render_pass)
            .subpass(0)];
        // SAFETY: all pipeline references, modules and the render pass are live; there are no descriptors or vertex buffers to bind.
        match unsafe { d.create_graphics_pipelines(vk::PipelineCache::null(), &pipelines, None) } {
            Ok(pipelines) => self.pipeline = pipelines[0],
            Err((partial, error)) => {
                for pipeline in partial {
                    // SAFETY: these are partially created owned handles and have never been referenced by any command buffer.
                    unsafe { d.destroy_pipeline(pipeline, None) };
                }
                return Err(error.into());
            }
        }
        // SAFETY: physical device remains live for this instance's lifetime.
        let memory = unsafe { self.instance.get_physical_device_memory_properties(pd) };
        // SAFETY: this owner exclusively owns the pool and requests three primaries.
        let commands = unsafe {
            d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(SLOTS as u32),
            )?
        };
        for command in commands {
            self.slots.push(Slot {
                command,
                ..Slot::default()
            });
            let slot = self
                .slots
                .last_mut()
                .expect("slot was inserted before allocation");
            let image = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .extent(vk::Extent3D {
                    width: self.width,
                    height: self.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC);
            // SAFETY: checked probe dimensions, one color attachment/layer/mip.
            slot.image = unsafe { d.create_image(&image, None)? };
            // SAFETY: slot.image is a live image belonging to this device.
            let requirements = unsafe { d.get_image_memory_requirements(slot.image) };
            let ty = memory_type(
                &memory,
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )?;
            // SAFETY: allocation size/type satisfy the queried image requirements.
            slot.memory = unsafe {
                d.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(ty),
                    None,
                )?
            };
            // SAFETY: matching allocation, aligned zero offset, not already bound.
            unsafe { d.bind_image_memory(slot.image, slot.memory, 0)? };
            let subresource = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            // SAFETY: the view covers the image's only color mip/layer.
            slot.view = unsafe {
                d.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(slot.image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .subresource_range(subresource),
                    None,
                )?
            };
            let views = [slot.view];
            // SAFETY: dimensions and view format match this render pass.
            slot.framebuffer = unsafe {
                d.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(self.render_pass)
                        .attachments(&views)
                        .width(self.width)
                        .height(self.height)
                        .layers(1),
                    None,
                )?
            };
            // SAFETY: an initially signaled owned fence permits the first reuse.
            slot.fence = unsafe {
                d.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )?
            };
        }
        let bytes = u64::from(self.width) * u64::from(self.height) * 4;
        // SAFETY: sized readback buffer is used only as transfer destination.
        self.readback = unsafe {
            d.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )?
        };
        // SAFETY: this is the live readback buffer's memory requirement query.
        let req = unsafe { d.get_buffer_memory_requirements(self.readback) };
        let flags = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let ty = memory_type(
            &memory,
            req.memory_type_bits,
            flags | vk::MemoryPropertyFlags::HOST_CACHED,
        )
        .or_else(|_| memory_type(&memory, req.memory_type_bits, flags))?;
        // SAFETY: allocation matches size/type requirements and supports coherent CPU access.
        self.readback_memory = unsafe {
            d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(ty),
                None,
            )?
        };
        // SAFETY: allocation meets buffer requirements with aligned zero offset.
        unsafe { d.bind_buffer_memory(self.readback, self.readback_memory, 0)? };
        // SAFETY: this range lies within the owned HOST_VISIBLE allocation; only verify() exposes it after the transfer fence and HOST_READ barrier.
        self.mapped = NonNull::new(
            unsafe { d.map_memory(self.readback_memory, 0, bytes, vk::MemoryMapFlags::empty())? }
                .cast(),
        );
        ensure!(self.mapped.is_some(), "null readback mapping");
        Ok(())
    }

    fn complete(&mut self, index: usize) -> Result<Option<u64>> {
        let d = self.device.as_ref().context("missing device")?;
        let slot = &mut self.slots[index];
        if !slot.inflight {
            return Ok(None);
        }
        // SAFETY: this owned fence belongs to this slot's last submission.
        unsafe { d.wait_for_fences(&[slot.fence], true, 5_000_000_000)? };
        let mut ticks = [0_u64; 2];
        // SAFETY: fence completion makes both owned query indices available; the output slice has exactly two 64-bit elements.
        unsafe {
            d.get_query_pool_results(
                self.queries,
                index as u32 * 2,
                &mut ticks,
                vk::QueryResultFlags::TYPE_64,
            )?
        };
        let mask = if self.timestamp_bits == 64 {
            u64::MAX
        } else {
            (1_u64 << self.timestamp_bits) - 1
        };
        let delta = ticks[1].wrapping_sub(ticks[0]) & mask;
        slot.inflight = false;
        Ok(Some((delta as f64 * self.timestamp_period) as u64))
    }

    pub fn acquire(&mut self) -> Result<Option<u64>> {
        self.complete(self.next)
    }

    pub fn submit(&mut self, seed: u32) -> Result<()> {
        let d = self.device.as_ref().context("missing device")?;
        let slot = &mut self.slots[self.next];
        ensure!(!slot.inflight, "GPU slot reused before completion");
        let query = self.next as u32 * 2;
        let clear = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        }];
        let pass = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(slot.framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: self.width,
                    height: self.height,
                },
            })
            .clear_values(&clear);
        let mut constants = [0; 8];
        constants[..4].copy_from_slice(&seed.to_ne_bytes());
        constants[4..].copy_from_slice(&self.iterations.to_ne_bytes());
        // SAFETY: this slot's prior fence completed before reset.
        unsafe {
            d.reset_fences(&[slot.fence])?;
            d.reset_command_buffer(slot.command, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(
                slot.command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            d.cmd_reset_query_pool(slot.command, self.queries, query, 2);
            d.cmd_write_timestamp(
                slot.command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                self.queries,
                query,
            );
            d.cmd_begin_render_pass(slot.command, &pass, vk::SubpassContents::INLINE);
            d.cmd_bind_pipeline(slot.command, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            d.cmd_push_constants(
                slot.command,
                self.layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                &constants,
            );
            d.cmd_draw(slot.command, 3, 1, 0, 0);
            d.cmd_end_render_pass(slot.command);
            d.cmd_write_timestamp(
                slot.command,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                self.queries,
                query + 1,
            );
            d.end_command_buffer(slot.command)?;
        }
        let commands = [slot.command];
        // SAFETY: command storage lives through submission; slot resources are retained and not reused until complete() observes this fence.
        unsafe {
            d.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&commands)],
                slot.fence,
            )?
        };
        slot.inflight = true;
        self.next = (self.next + 1) % SLOTS;
        Ok(())
    }

    pub fn drain(&mut self) -> Result<Vec<u64>> {
        let mut samples = Vec::with_capacity(SLOTS);
        for i in 0..self.slots.len() {
            if let Some(ns) = self.complete(i)? {
                samples.push(ns);
            }
        }
        Ok(samples)
    }

    pub fn verify(&mut self) -> Result<u64> {
        self.drain()?;
        let index = self.next;
        self.submit(VERIFY_SEED)?;
        self.drain()?;
        let d = self.device.as_ref().context("missing device")?;
        let slot = &self.slots[index];
        let region = [vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: self.width,
                height: self.height,
                depth: 1,
            })];
        let barrier = [vk::BufferMemoryBarrier::default()
            .buffer(self.readback)
            .offset(0)
            .size(vk::WHOLE_SIZE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)];
        // SAFETY: every GPU slot was drained.
        unsafe {
            d.reset_fences(&[slot.fence])?;
            d.reset_command_buffer(slot.command, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(slot.command, &vk::CommandBufferBeginInfo::default())?;
            d.cmd_copy_image_to_buffer(
                slot.command,
                slot.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback,
                &region,
            );
            d.cmd_pipeline_barrier(
                slot.command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                &barrier,
                &[],
            );
            d.end_command_buffer(slot.command)?;
        }
        let commands = [slot.command];
        // SAFETY: this completed slot is exclusively reused for validation, then fully waited before exposing the coherent mapped memory.
        unsafe {
            d.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&commands)],
                slot.fence,
            )?;
            d.wait_for_fences(&[slot.fence], true, 5_000_000_000)?;
        }
        let bytes = (u64::from(self.width) * u64::from(self.height) * 4) as usize;
        let ptr = self.mapped.context("no readback mapping")?;
        // SAFETY: the live mapping covers bytes, and no transfer or render is in flight.
        let data = unsafe { std::slice::from_raw_parts(ptr.as_ptr(), bytes) };
        for gy in 0..8 {
            for gx in 0..8 {
                let (x, y) = (gx * (self.width - 1) / 7, gy * (self.height - 1) / 7);
                let mut v = x.wrapping_mul(1664525) ^ y.wrapping_mul(1013904223) ^ VERIFY_SEED;
                for _ in 0..self.iterations {
                    v = (v ^ (v >> 16)).wrapping_mul(2246822519);
                    v = (v ^ (v >> 13)).wrapping_mul(3266489917);
                }
                let expected = [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255];
                let at = ((u64::from(y) * u64::from(self.width) + u64::from(x)) * 4) as usize;
                ensure!(
                    data[at..at + 4] == expected,
                    "GPU known-answer mismatch at {x},{y}"
                );
            }
        }
        Ok(data.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &n| {
            (h ^ u64::from(n)).wrapping_mul(0x100000001b3)
        }))
    }
}

fn memory_type(
    memory: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    flags: vk::MemoryPropertyFlags,
) -> Result<u32> {
    (0..memory.memory_type_count)
        .find(|&i| {
            bits & (1 << i) != 0
                && memory.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
        })
        .context("no matching Vulkan memory type")
}

impl Drop for Gpu {
    fn drop(&mut self) {
        if let Some(d) = &self.device {
            // SAFETY: all handles belong to this device or are null after partial initialization.
            unsafe {
                let _ = d.device_wait_idle();
                if self.mapped.is_some() {
                    d.unmap_memory(self.readback_memory);
                }
                for slot in &self.slots {
                    d.destroy_fence(slot.fence, None);
                    d.destroy_framebuffer(slot.framebuffer, None);
                    d.destroy_image_view(slot.view, None);
                    d.destroy_image(slot.image, None);
                    d.free_memory(slot.memory, None);
                }
                d.destroy_buffer(self.readback, None);
                d.free_memory(self.readback_memory, None);
                d.destroy_command_pool(self.pool, None);
                d.destroy_query_pool(self.queries, None);
                d.destroy_pipeline(self.pipeline, None);
                d.destroy_pipeline_layout(self.layout, None);
                d.destroy_render_pass(self.render_pass, None);
                for &module in &self.shaders {
                    d.destroy_shader_module(module, None);
                }
                d.destroy_device(None);
            }
        }
        // SAFETY: all child objects are gone; Entry still owns the library.
        unsafe { self.instance.destroy_instance(None) };
    }
}
