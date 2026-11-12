//! CPU upload -> GPU memory -> CPU readback across coherent memory types.
use anyhow::{Context, Result, ensure};
use ash::vk;
use serde_json::json;

impl super::Probe {
    pub(super) fn verify_uploads(
        &mut self,
        queue: vk::Queue,
        family: u32,
        properties: &vk::PhysicalDeviceMemoryProperties,
    ) -> Result<Vec<serde_json::Value>> {
        const BYTES: u64 = 4 * 1024 * 1024;
        let device = self.device.as_ref().context("missing device")?;
        // SAFETY: this family is enabled for transfers; the owned pool remains live until every submission completes, or until process teardown.
        let pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(family),
                None,
            )
        }?;
        self.pools.push(pool);
        let mut results = Vec::new();
        for index in 0..properties.memory_type_count {
            let flags = properties.memory_types[index as usize].property_flags;
            if !flags.contains(
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            ) {
                continue;
            }
            let mut pair = Vec::new();
            for host in [true, false] {
                // SAFETY: initialized descriptor with bounded size and usage.
                let buffer = unsafe {
                    device.create_buffer(
                        &vk::BufferCreateInfo::default().size(BYTES).usage(
                            vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
                        ),
                        None,
                    )
                }?;
                self.buffers.push(buffer);
                // SAFETY: live buffer belonging to this device.
                let req = unsafe { device.get_buffer_memory_requirements(buffer) };
                ensure!(req.size <= 16 * 1024 * 1024, "upload allocation too large");
                let memory_type = if host {
                    if req.memory_type_bits & (1 << index) == 0 {
                        break;
                    }
                    index
                } else {
                    (0..properties.memory_type_count)
                        .find(|&i| {
                            req.memory_type_bits & (1 << i) != 0
                                && properties.memory_types[i as usize]
                                    .property_flags
                                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                        })
                        .context("no local upload target memory")?
                };
                // SAFETY: selected type is allowed and queried size is bounded.
                let allocation = unsafe {
                    device.allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(req.size)
                            .memory_type_index(memory_type),
                        None,
                    )
                }?;
                self.memory.push(allocation);
                // SAFETY: compatible memory, aligned offset zero, unique buffer.
                unsafe { device.bind_buffer_memory(buffer, allocation, 0) }?;
                pair.push((buffer, allocation));
            }
            if pair.len() != 2 {
                continue;
            }
            let (staging, allocation) = pair[0];
            let target = pair[1].0;
            // Index is bounded to 4 MiB / 4, so conversion and increment fit.
            let pattern = |word: usize| 0x9e37_79b9_u32.wrapping_mul(word as u32 + 1);
            // SAFETY: coherent host-visible allocation, owned bounded range; no GPU work uses this buffer while CPU initializes it.
            let pointer =
                unsafe { device.map_memory(allocation, 0, BYTES, vk::MemoryMapFlags::empty()) }?;
            // SAFETY: the mapped 4 MiB is aligned to u32 and exclusively owned.
            unsafe { std::slice::from_raw_parts_mut(pointer.cast::<u32>(), BYTES as usize / 4) }
                .iter_mut()
                .enumerate()
                .for_each(|(i, v)| *v = pattern(i));
            // SAFETY: no references remain; this owner created the mapping.
            unsafe { device.unmap_memory(allocation) };
            println!("CPU_UPLOAD type={index} flags={flags:?} bytes={BYTES} submit");
            transfer_buffers(
                device,
                queue,
                pool,
                (staging, target),
                BYTES,
                &mut self.fences,
                &mut self.inflight,
            )?;
            // Clear after upload completion so a skipped/incorrect download cannot pass by reading the original CPU pattern back.
            // SAFETY: upload fence completed; coherent mapped range is owned.
            let pointer =
                unsafe { device.map_memory(allocation, 0, BYTES, vk::MemoryMapFlags::empty()) }?;
            // SAFETY: same aligned, live, exclusively mapped range as above.
            unsafe { std::slice::from_raw_parts_mut(pointer.cast::<u32>(), BYTES as usize / 4) }
                .fill(0);
            // SAFETY: references have expired; the mapping is uniquely owned.
            unsafe { device.unmap_memory(allocation) };
            transfer_buffers(
                device,
                queue,
                pool,
                (target, staging),
                BYTES,
                &mut self.fences,
                &mut self.inflight,
            )?;
            // SAFETY: coherent memory, completed download and host-read barrier.
            let pointer =
                unsafe { device.map_memory(allocation, 0, BYTES, vk::MemoryMapFlags::empty()) }?;
            // SAFETY: GPU initialized this full range; no GPU access is pending.
            let mismatch =
                unsafe { std::slice::from_raw_parts(pointer.cast::<u32>(), BYTES as usize / 4) }
                    .iter()
                    .enumerate()
                    .find_map(|(i, &v)| (v != pattern(i)).then_some((i, v, pattern(i))));
            // SAFETY: no borrowed references remain; this owner mapped it.
            unsafe { device.unmap_memory(allocation) };
            ensure!(
                mismatch.is_none(),
                "CPU upload type={index} readback mismatch: {mismatch:?}"
            );
            println!("CPU_UPLOAD type={index} readback_verified bytes={BYTES}");
            results.push(json!({"memory_type":index,"flags":format!("{flags:?}"),"bytes":BYTES,"verified":true}));
        }
        ensure!(!results.is_empty(), "no coherent upload memory type tested");
        Ok(results)
    }
}

fn transfer_buffers(
    device: &ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    buffers: (vk::Buffer, vk::Buffer),
    bytes: u64,
    fences: &mut Vec<vk::Fence>,
    inflight: &mut bool,
) -> Result<()> {
    // SAFETY: live pool in the enabled transfer family, one fresh command.
    let command = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }?[0];
    // SAFETY: exclusive recording; both owned buffers cover `bytes`.
    unsafe {
        device.begin_command_buffer(command, &vk::CommandBufferBeginInfo::default())?;
        device.cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::HOST_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::TRANSFER_WRITE)],
            &[],
            &[],
        );
        device.cmd_copy_buffer(
            command,
            buffers.0,
            buffers.1,
            &[vk::BufferCopy::default().size(bytes)],
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
    // SAFETY: initialized descriptor, owned device and retained fence.
    let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;
    fences.push(fence);
    *inflight = true;
    let commands = [command];
    // SAFETY: exclusive queue; all resources retained through completion, or deliberately retained until process teardown after a failed wait.
    unsafe {
        device.queue_submit(
            queue,
            &[vk::SubmitInfo::default().command_buffers(&commands)],
            fence,
        )?;
        device.wait_for_fences(&[fence], true, 5_000_000_000)?;
    }
    *inflight = false;
    Ok(())
}
