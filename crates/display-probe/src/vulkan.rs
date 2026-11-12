use std::ptr::NonNull;

use anyhow::{Context, Result, ensure};
use ash::{Entry, vk};

/// Owns every Vulkan resource, including partially initialized construction.
pub struct Gpu {
    _entry: Entry,
    instance: ash::Instance,
    device: Option<ash::Device>,
    queue: vk::Queue,
    image: vk::Image,
    image_memory: vk::DeviceMemory,
    buffer: vk::Buffer,
    buffer_memory: vk::DeviceMemory,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    mapped: Option<NonNull<u8>>,
    bytes: usize,
    width: u32,
    height: u32,
    name: String,
}

impl Gpu {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let bytes = usize::try_from(width)?
            .checked_mul(usize::try_from(height)?)
            .and_then(|n| n.checked_mul(4))
            .context("frame size overflow")?;
        // SAFETY: the owned guest runtime supplies the Vulkan loader.
        let entry = unsafe { Entry::load()? };
        let app = vk::ApplicationInfo::default()
            .application_name(c"Legion display probe")
            .api_version(vk::API_VERSION_1_1);
        // SAFETY: app and create-info storage live through this synchronous call.
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
            image: vk::Image::null(),
            image_memory: vk::DeviceMemory::null(),
            buffer: vk::Buffer::null(),
            buffer_memory: vk::DeviceMemory::null(),
            pool: vk::CommandPool::null(),
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            mapped: None,
            bytes,
            width,
            height,
            name: String::new(),
        };
        gpu.initialize()?;
        Ok(gpu)
    }

    fn initialize(&mut self) -> Result<()> {
        // SAFETY: instance is live; returned physical handles belong to it.
        let devices = unsafe { self.instance.enumerate_physical_devices()? };
        let pd = devices
            .into_iter()
            .find(|pd| {
                // SAFETY: each handle came from this live instance.
                let p = unsafe { self.instance.get_physical_device_properties(*pd) };
                p.vendor_id == 0x10de && p.device_type != vk::PhysicalDeviceType::CPU
            })
            .context("no NVIDIA hardware Vulkan device")?;
        // SAFETY: pd is a live physical-device handle from this instance.
        let props = unsafe { self.instance.get_physical_device_properties(pd) };
        self.name = props.device_name_as_c_str()?.to_string_lossy().into_owned();
        // SAFETY: pd is live; this call writes no application memory.
        let queues = unsafe {
            self.instance
                .get_physical_device_queue_family_properties(pd)
        };
        let family = u32::try_from(
            queues
                .iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .context("no graphics queue")?,
        )?;
        let priority = [1.0];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&priority)];
        // SAFETY: queue family and priority count were queried and validated; every create-info reference remains alive for the synchronous call.
        self.device = Some(unsafe {
            self.instance.create_device(
                pd,
                &vk::DeviceCreateInfo::default().queue_create_infos(&queue_info),
                None,
            )?
        });
        let d = self.device.as_ref().context("missing Vulkan device")?;
        // SAFETY: queue zero was requested from this family at creation.
        self.queue = unsafe { d.get_device_queue(family, 0) };
        let image_info = vk::ImageCreateInfo::default()
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
            .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: dimensions and format describe a transfer-only owned image.
        self.image = unsafe { d.create_image(&image_info, None)? };
        // SAFETY: this image belongs to this device and is still live.
        let req = unsafe { d.get_image_memory_requirements(self.image) };
        // SAFETY: physical device is live for the full instance lifetime.
        let memory = unsafe { self.instance.get_physical_device_memory_properties(pd) };
        let image_type = memory_type(
            &memory,
            req.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        // SAFETY: size and type match the queried image requirements.
        self.image_memory = unsafe {
            d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(image_type),
                None,
            )?
        };
        // SAFETY: memory allocation satisfies image size/type/alignment; offset 0.
        unsafe { d.bind_image_memory(self.image, self.image_memory, 0)? };
        // SAFETY: buffer size is checked; it is used only as transfer destination.
        self.buffer = unsafe {
            d.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(u64::try_from(self.bytes)?)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )?
        };
        // SAFETY: this live buffer belongs to this device.
        let req = unsafe { d.get_buffer_memory_requirements(self.buffer) };
        let ty = memory_type(
            &memory,
            req.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE
                | vk::MemoryPropertyFlags::HOST_COHERENT
                | vk::MemoryPropertyFlags::HOST_CACHED,
        )
        .or_else(|_| {
            memory_type(
                &memory,
                req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
        })?;
        // SAFETY: allocation matches buffer requirements and supports CPU mapping.
        self.buffer_memory = unsafe {
            d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(ty),
                None,
            )?
        };
        // SAFETY: allocation meets buffer requirements, with aligned offset zero.
        unsafe { d.bind_buffer_memory(self.buffer, self.buffer_memory, 0)? };
        // SAFETY: requested byte range lies within this HOST_VISIBLE allocation.
        self.mapped = NonNull::new(
            unsafe {
                d.map_memory(
                    self.buffer_memory,
                    0,
                    u64::try_from(self.bytes)?,
                    vk::MemoryMapFlags::empty(),
                )?
            }
            .cast(),
        );
        ensure!(self.mapped.is_some(), "Vulkan returned a null mapping");
        // SAFETY: family is valid for this device; this pool has one owner.
        self.pool = unsafe {
            d.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?
        };
        // SAFETY: pool is live and allocation count is exactly one.
        self.cmd = *unsafe {
            d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?
        }
        .first()
        .context("no command buffer")?;
        // SAFETY: default fence has no imported handles or callback pointers.
        self.fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None)? };
        Ok(())
    }

    pub fn render(&mut self, rgba: [u8; 4]) -> Result<()> {
        let d = self.device.as_ref().context("missing Vulkan device")?;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barrier = [vk::ImageMemoryBarrier::default()
            .image(self.image)
            .subresource_range(range)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)];
        // SAFETY: the previous render waited for its fence and CPU readback ended before this mutable borrow.
        unsafe {
            d.reset_command_pool(self.pool, vk::CommandPoolResetFlags::empty())?;
            d.reset_fences(&[self.fence])?;
        }
        // SAFETY: this reset primary command buffer is exclusively owned here.
        unsafe {
            d.begin_command_buffer(
                self.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?
        };
        // SAFETY: live image and valid ranges; UNDEFINED intentionally discards prior pixels.
        unsafe {
            d.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barrier,
            );
            d.cmd_clear_color_image(
                self.cmd,
                self.image,
                vk::ImageLayout::GENERAL,
                &vk::ClearColorValue {
                    float32: rgba.map(|v| f32::from(v) / 255.0),
                },
                &[range],
            );
        }
        let barrier = [vk::ImageMemoryBarrier::default()
            .image(self.image)
            .subresource_range(range)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)];
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
        let host_barrier = [vk::BufferMemoryBarrier::default()
            .buffer(self.buffer)
            .offset(0)
            .size(u64::try_from(self.bytes)?)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)];
        // SAFETY: image clear is made visible to the transfer read; destination buffer is sized for the tightly packed full image.
        unsafe {
            d.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barrier,
            );
            d.cmd_copy_image_to_buffer(
                self.cmd,
                self.image,
                vk::ImageLayout::GENERAL,
                self.buffer,
                &region,
            );
            d.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                &host_barrier,
                &[],
            );
            d.end_command_buffer(self.cmd)?;
        }
        let commands = [self.cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&commands)];
        // SAFETY: queue and command buffer belong to this device, submit data lives through the call, and no resources are reused before the fence.
        unsafe {
            d.queue_submit(self.queue, &submit, self.fence)?;
            d.wait_for_fences(&[self.fence], true, 5_000_000_000)?;
        }
        Ok(())
    }

    pub fn pixels(&self) -> &[u8] {
        match self.mapped {
            // SAFETY: mapping remains live until Drop.
            Some(ptr) => unsafe { std::slice::from_raw_parts(ptr.as_ptr(), self.bytes) },
            None => &[],
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
}

fn memory_type(
    p: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    flags: vk::MemoryPropertyFlags,
) -> Result<u32> {
    (0..p.memory_type_count)
        .find(|index| {
            bits & (1 << index) != 0
                && p.memory_types[*index as usize]
                    .property_flags
                    .contains(flags)
        })
        .context("no suitable Vulkan memory type")
}

impl Drop for Gpu {
    fn drop(&mut self) {
        if let Some(d) = &self.device {
            // SAFETY: all handles were initialized to null before construction; Vulkan accepts null destruction handles.
            unsafe {
                let _ = d.device_wait_idle();
                if self.mapped.is_some() {
                    d.unmap_memory(self.buffer_memory);
                }
                d.destroy_fence(self.fence, None);
                d.destroy_command_pool(self.pool, None);
                d.destroy_buffer(self.buffer, None);
                d.destroy_image(self.image, None);
                d.free_memory(self.buffer_memory, None);
                d.free_memory(self.image_memory, None);
                d.destroy_device(None);
            }
        }
        // SAFETY: the device is gone and the retained Entry is still live.
        unsafe { self.instance.destroy_instance(None) };
    }
}
