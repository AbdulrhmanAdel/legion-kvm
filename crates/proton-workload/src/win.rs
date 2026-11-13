//! Windows-only FFI boundary.
use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::{
    alloc::{GlobalAlloc, Layout},
    ffi::c_void,
    mem::ManuallyDrop,
    ptr,
};
use legion_proton_workload::{
    Config, GpuClockCalibration, GpuObservation, Report, Sample, cpu_work, percentile,
    summarize_gpu, timing_summary, verify_red,
};
use windows::{
    Win32::Graphics::{
        Direct3D::D3D_FEATURE_LEVEL_12_0,
        Direct3D12::*,
        Dxgi::{Common::*, *},
    },
    core::Interface,
};

type Result<T> = core::result::Result<T, String>;
#[path = "cpu_audit.rs"]
mod cpu_audit;
#[path = "nt_audit.rs"]
mod nt_audit;
#[path = "pipeline.rs"]
mod pipeline;
#[path = "present.rs"]
mod present;

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    // The compiler calls this with the Win64 stack-probe ABI.
    fn __chkstk();
}
#[used]
static STACK_PROBE_IMPORT: unsafe extern "system" fn() = __chkstk;

// MSVC's marker for floating-point code; there is no CRT initialization needed.
#[unsafe(no_mangle)]
static _fltused: i32 = 0;

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetProcessHeap() -> *mut c_void;
    fn HeapAlloc(heap: *mut c_void, flags: u32, size: usize) -> *mut c_void;
    fn HeapFree(heap: *mut c_void, flags: u32, memory: *mut c_void) -> i32;
    fn ExitProcess(code: u32) -> !;
    fn GetStdHandle(which: u32) -> *mut c_void;
    fn WriteFile(
        handle: *mut c_void,
        data: *const u8,
        len: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn FlushFileBuffers(handle: *mut c_void) -> i32;
    fn ReadFile(
        handle: *mut c_void,
        data: *mut u8,
        len: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CreateFileW(
        path: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetEnvironmentVariableW(name: *const u16, value: *mut u16, size: u32) -> u32;
    fn QueryPerformanceCounter(value: *mut i64) -> i32;
    fn QueryPerformanceFrequency(value: *mut i64) -> i32;
    fn GetActiveProcessorCount(group: u16) -> u32;
    fn CreateEventW(
        security: *mut c_void,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn Sleep(milliseconds: u32);
}

struct Heap;
// SAFETY: each allocation comes from the process heap, supports up to 16-byte alignment on Win64, and is freed through that same heap.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > 16 {
            return ptr::null_mut();
        }
        // SAFETY: GetProcessHeap is valid throughout process lifetime; size is nonzero.
        unsafe { HeapAlloc(GetProcessHeap(), 0, layout.size().max(1)).cast() }
    }
    unsafe fn dealloc(&self, memory: *mut u8, _: Layout) {
        // SAFETY: GlobalAlloc's caller supplies an allocation from this allocator.
        unsafe {
            HeapFree(GetProcessHeap(), 0, memory.cast());
        }
    }
}
#[global_allocator]
static HEAP: Heap = Heap;

struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: Handle uniquely owns a successful CreateFileW/CreateEventW result.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn write_handle(handle: *mut c_void, bytes: &[u8]) -> Result<()> {
    let mut written = 0;
    let len = u32::try_from(bytes.len()).map_err(|_| "output too large")?;
    // SAFETY: buffer and length outlive synchronous WriteFile; no overlapped I/O.
    let ok = unsafe { WriteFile(handle, bytes.as_ptr(), len, &mut written, ptr::null_mut()) };
    if ok == 0 || written != len {
        return Err("WriteFile failed or was short".into());
    }
    Ok(())
}

fn log(message: &str) {
    // SAFETY: requesting this process's standard output handle.
    let handle = unsafe { GetStdHandle((-11_i32) as u32) };
    let _ = write_handle(handle, message.as_bytes());
    let _ = write_handle(handle, b"\r\n");
    // SAFETY: this process's stdout handle is valid for a synchronous flush.
    let _ = unsafe { FlushFileBuffers(handle) };
}

fn open(path: &str, write: bool) -> Result<Handle> {
    let directory: Vec<u16> = "LEGION_WORKLOAD_DIRECTORY\0".encode_utf16().collect();
    let mut value = vec![0_u16; 32768];
    // SAFETY: the name is terminated and the writable buffer has the supplied capacity.
    let length = unsafe {
        GetEnvironmentVariableW(directory.as_ptr(), value.as_mut_ptr(), value.len() as u32)
    } as usize;
    if length >= value.len() {
        return Err("workload directory exceeds the Windows path limit".into());
    }
    let override_path = if length != 0 {
        path.strip_prefix("Z:\\frames\\")
            .map(|name| {
                let directory = String::from_utf16(&value[..length])
                    .map_err(|_| "workload directory is not valid UTF-16")?;
                Ok::<_, &str>(format!(
                    "{}\\{name}",
                    directory.trim_end_matches(['\\', '/'])
                ))
            })
            .transpose()?
    } else {
        None
    };
    let path = override_path.as_deref().unwrap_or(path);
    let wide: Vec<u16> = path.encode_utf16().chain(core::iter::once(0)).collect();
    // SAFETY: path is NUL terminated; no security/template pointers.
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            if write { 0x4000_0000 } else { 0x8000_0000 },
            1,
            ptr::null_mut(),
            if write { 2 } else { 3 },
            0x80,
            ptr::null_mut(),
        )
    };
    if h as isize == -1 {
        return Err(format!("cannot open {path}"));
    }
    Ok(Handle(h))
}

fn read_config() -> Result<Config> {
    let file = open("Z:\\frames\\fixture-config.json", false)?;
    let mut bytes = vec![0_u8; 16 * 1024];
    let mut count = 0;
    // SAFETY: writable buffer has the requested capacity, synchronous file owned here.
    if unsafe {
        ReadFile(
            file.0,
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            &mut count,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err("read configuration failed".into());
    }
    bytes.truncate(count as usize);
    let config: Config = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if !config.valid() {
        return Err("invalid bounded configuration".into());
    }
    Ok(config)
}

fn counter() -> Result<u64> {
    let mut v = 0;
    // SAFETY: writable output is valid for this synchronous OS call.
    if unsafe { QueryPerformanceCounter(&mut v) } == 0 || v < 0 {
        return Err("QPC failed".into());
    }
    Ok(v as u64)
}

fn com<T>(result: windows::core::Result<T>, operation: &str) -> Result<T> {
    result.map_err(|e| format!("{operation}: HRESULT {:#010x}", e.code().0))
}

struct GpuQueries {
    heap: ID3D12QueryHeap,
    readback: ID3D12Resource,
    mapped: core::ptr::NonNull<u8>,
    frequency: u64,
}

impl GpuQueries {
    fn new(device: &ID3D12Device, queue: &ID3D12CommandQueue) -> Result<Self> {
        let mut heap: Option<ID3D12QueryHeap> = None;
        // SAFETY: live DIRECT queue; initialized TIMESTAMP descriptor and output match the requested query interface and two indices recorded below.
        let frequency = unsafe {
            com(
                device.CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 2,
                        NodeMask: 0,
                    },
                    &mut heap,
                ),
                "timestamp query heap",
            )?;
            com(queue.GetTimestampFrequency(), "GPU timestamp frequency")?
        };
        if frequency == 0 {
            return Err("zero GPU timestamp frequency".into());
        }
        let heap = heap.ok_or("missing timestamp query heap")?;
        let readback = resource(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 16,
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
        let mut data = ptr::null_mut();
        // SAFETY: initialized 16-byte CPU-readable resource; the output remains owned and mapped until Drop.
        unsafe {
            com(
                readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 16 }), Some(&mut data)),
                "map timestamp readback",
            )?;
        }
        let mapped = core::ptr::NonNull::new(data.cast::<u8>()).ok_or("null timestamp readback")?;
        Ok(Self {
            heap,
            readback,
            mapped,
            frequency,
        })
    }

    fn begin(&self, commands: &ID3D12GraphicsCommandList) {
        // SAFETY: live timestamp heap, index zero is in range.
        unsafe {
            commands.EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, 0);
        }
    }

    fn end(&self, commands: &ID3D12GraphicsCommandList) {
        // SAFETY: both timestamp indices were written in this command list; the live 16-byte destination is in COPY_DEST and offset zero is aligned.
        unsafe {
            commands.EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            commands.ResolveQueryData(
                &self.heap,
                D3D12_QUERY_TYPE_TIMESTAMP,
                0,
                2,
                &self.readback,
                0,
            );
        }
    }

    fn read_after_fence(&self) -> [u64; 2] {
        // SAFETY: called after verifying the submission's original fence.
        let bytes = unsafe { ptr::read_volatile(self.mapped.as_ptr().cast::<[[u8; 8]; 2]>()) };
        [u64::from_le_bytes(bytes[0]), u64::from_le_bytes(bytes[1])]
    }
}

impl Drop for GpuQueries {
    fn drop(&mut self) {
        // SAFETY: exactly one successful Map is owned here, with no CPU writes; the COM resource is still live while Drop runs.
        unsafe {
            self.readback
                .Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
        }
    }
}

struct FrameCpuTimes {
    record: u64,
    submit: u64,
    submit_begin_qpc: u64,
    submit_end_qpc: u64,
}

struct Gpu {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    commands: ID3D12GraphicsCommandList,
    target: ID3D12Resource,
    _descriptors: ID3D12DescriptorHeap,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    fence: ID3D12Fence,
    event: Handle,
    serial: u64,
    width: u32,
    height: u32,
    adapter_name: String,
    vendor_id: u32,
    device_id: u32,
    qpc_frequency: u64,
    diagnostic_fence: Option<ID3D12Fence>,
    diagnostic_kicks: core::cell::Cell<u64>,
    queries: Option<GpuQueries>,
    timestamp_validation_copy_ticks: Vec<u64>,
}

impl Gpu {
    fn new(config: &Config, qpc_frequency: u64) -> Result<Self> {
        log("stage: DXGI adapter and D3D12 device");
        // SAFETY: COM output types match their requested interfaces; descriptors are fully initialized and remain valid for each synchronous create call.
        let (device, adapter_name, vendor_id, device_id) = unsafe {
            let factory: IDXGIFactory1 = com(CreateDXGIFactory1(), "CreateDXGIFactory1")?;
            let mut chosen = None;
            for index in 0..32 {
                let Ok(adapter) = factory.EnumAdapters1(index) else {
                    break;
                };
                let desc = com(adapter.GetDesc1(), "adapter descriptor")?;
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let mut device: Option<ID3D12Device> = None;
                if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_ok() {
                    let length = desc
                        .Description
                        .iter()
                        .position(|v| *v == 0)
                        .unwrap_or(desc.Description.len());
                    chosen = Some((
                        device.ok_or("missing D3D12 device")?,
                        String::from_utf16_lossy(&desc.Description[..length]),
                        desc.VendorId,
                        desc.DeviceId,
                    ));
                    break;
                }
            }
            chosen.ok_or("no hardware D3D12 feature-level-12 adapter")?
        };
        log(&format!(
            "adapter: {adapter_name} {vendor_id:04x}:{device_id:04x}"
        ));
        // SAFETY: queue/allocator/list types agree (DIRECT); fence and heap are owned by this device.
        let (queue, allocator, commands, descriptors, rtv, fence) = unsafe {
            let queue: ID3D12CommandQueue = com(
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                }),
                "command queue",
            )?;
            let allocator: ID3D12CommandAllocator = com(
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT),
                "allocator",
            )?;
            let commands: ID3D12GraphicsCommandList = com(
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None),
                "command list",
            )?;
            com(commands.Close(), "close initial list")?;
            let descriptors: ID3D12DescriptorHeap = com(
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    ..Default::default()
                }),
                "RTV heap",
            )?;
            let rtv = descriptors.GetCPUDescriptorHandleForHeapStart();
            let fence: ID3D12Fence = com(device.CreateFence(0, D3D12_FENCE_FLAG_NONE), "fence")?;
            (queue, allocator, commands, descriptors, rtv, fence)
        };
        let target = resource(
            &device,
            D3D12_HEAP_TYPE_DEFAULT,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(config.width),
                Height: config.height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        )?;
        // SAFETY: target format supports RTV; descriptor points into our live RTV heap.
        unsafe {
            device.CreateRenderTargetView(&target, None, rtv);
        }
        // SAFETY: anonymous auto-reset event, initially unsignaled, no security pointer.
        let event = unsafe { CreateEventW(ptr::null_mut(), 0, 0, ptr::null()) };
        if event.is_null() {
            return Err("CreateEventW failed".into());
        }
        let event = Handle(event);
        let diagnostic_fence = if config.diagnostic_completion_kick {
            // SAFETY: optional separate fence on the same live device.
            Some(com(
                unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) },
                "diagnostic fence",
            )?)
        } else {
            None
        };
        let queries = if config.gpu_timestamps {
            Some(GpuQueries::new(&device, &queue)?)
        } else {
            None
        };
        Ok(Self {
            device,
            queue,
            allocator,
            commands,
            target,
            _descriptors: descriptors,
            rtv,
            fence,
            event,
            serial: 0,
            width: config.width,
            height: config.height,
            adapter_name,
            vendor_id,
            device_id,
            qpc_frequency,
            diagnostic_fence,
            diagnostic_kicks: core::cell::Cell::new(0),
            queries,
            timestamp_validation_copy_ticks: Vec::new(),
        })
    }

    fn reset(&self) -> Result<()> {
        // SAFETY: the previous submission completed before allocator/list reuse.
        unsafe {
            com(self.allocator.Reset(), "reset allocator")?;
            com(
                self.commands.Reset(&self.allocator, None),
                "reset command list",
            )
        }
    }

    fn submit(&mut self) -> Result<()> {
        // SAFETY: recorded list belongs to this queue's device; interface cast is checked.
        unsafe {
            com(self.commands.Close(), "close command list")?;
            self.queue
                .ExecuteCommandLists(&[Some(com(self.commands.cast(), "command list cast")?)]);
            self.serial += 1;
            com(self.queue.Signal(&self.fence, self.serial), "queue signal")
        }
    }

    fn completed(&self) -> u64 {
        // SAFETY: fence remains live for this synchronous query.
        unsafe { self.fence.GetCompletedValue() }
    }

    fn wait(&self) -> Result<[u64; 3]> {
        self.wait_for(self.serial)
    }

    fn wait_for(&self, serial: u64) -> Result<[u64; 3]> {
        let before = counter()?;
        let completed = self.completed();
        let mut query_ticks = counter()? - before;
        if completed == u64::MAX {
            return Err("D3D12 device removed".into());
        }
        let mut arm_ticks = 0;
        let mut wait_ticks = 0;
        let mut wait_status = 0;
        if completed < serial {
            let before = counter()?;
            // SAFETY: live fence and owned event, only one outstanding submission.
            let armed = unsafe {
                self.fence
                    .SetEventOnCompletion(serial, windows::Win32::Foundation::HANDLE(self.event.0))
            };
            com(armed, "fence event")?;
            arm_ticks = counter()? - before;
            let before = counter()?;
            // SAFETY: the owned event remains open throughout the bounded wait.
            wait_status = unsafe {
                WaitForSingleObject(
                    self.event.0,
                    if self.diagnostic_fence.is_some() {
                        100
                    } else {
                        10_000
                    },
                )
            };
            if wait_status == 0x102
                && let Some(fence) = self.diagnostic_fence.as_ref()
            {
                let prior = self.completed();
                // SAFETY: queue and separate diagnostic fence share this device; original fence/value remain untouched.
                let queued = unsafe { self.queue.Signal(fence, self.serial) };
                com(queued, "diagnostic queue signal")?;
                self.diagnostic_kicks.set(self.diagnostic_kicks.get() + 1);
                // SAFETY: the original event is still armed for the original fence.
                wait_status = unsafe { WaitForSingleObject(self.event.0, 1000) };
                log(&format!(
                    "diagnostic queue signal: requested={}, before={prior}, after={}, wait={wait_status:#x}",
                    self.serial,
                    self.completed()
                ));
            }
            wait_ticks = counter()? - before;
        }
        let before = counter()?;
        let completed = self.completed();
        query_ticks += counter()? - before;
        if wait_status != 0 || completed < serial || completed == u64::MAX {
            let diagnostic = serde_json::json!({"schema":1,"qpc_frequency":self.qpc_frequency,"wait_status":wait_status,"requested":self.serial,"completed":completed,"query_ticks":query_ticks,"arm_ticks":arm_ticks,"event_wait_ticks":wait_ticks});
            let file = open("Z:\\frames\\fence-failure.json", true)?;
            write_handle(
                file.0,
                &serde_json::to_vec_pretty(&diagnostic).map_err(|e| e.to_string())?,
            )?;
            return Err(format!(
                "GPU fence wait={wait_status:#x}, requested={}, completed={completed}; see fence-failure.json",
                self.serial
            ));
        }
        Ok([query_ticks, arm_ticks, wait_ticks])
    }

    fn begin_frame(&mut self, clears: u32) -> Result<FrameCpuTimes> {
        let before = counter()?;
        self.reset()?;
        if let Some(queries) = &self.queries {
            queries.begin(&self.commands);
        }
        for i in 0..clears {
            let color = if i + 1 == clears {
                [1.0, 0.0, 0.0, 1.0]
            } else {
                [0.0, 1.0, 0.0, 1.0]
            };
            // SAFETY: target is in RENDER_TARGET state with its live RTV descriptor.
            unsafe {
                self.commands.ClearRenderTargetView(self.rtv, &color, None);
            }
        }
        if let Some(queries) = &self.queries {
            queries.end(&self.commands);
        }
        let recorded = counter()?;
        self.submit()?;
        let submitted = counter()?;
        Ok(FrameCpuTimes {
            record: recorded - before,
            submit: submitted - recorded,
            submit_begin_qpc: recorded,
            submit_end_qpc: submitted,
        })
    }

    fn calibrate(&self) -> Result<GpuClockCalibration> {
        let call_begin_qpc = counter()?;
        let mut gpu_ticks = 0;
        let mut qpc_ticks = 0;
        // SAFETY: live timestamp-capable DIRECT queue with valid writable GPU/QPC outputs for the synchronous calibration call.
        let result = unsafe {
            self.queue
                .GetClockCalibration(&mut gpu_ticks, &mut qpc_ticks)
        };
        com(result, "GPU/QPC clock calibration")?;
        Ok(GpuClockCalibration {
            gpu_ticks,
            qpc_ticks,
            call_begin_qpc,
            call_end_qpc: counter()?,
        })
    }

    fn verify(&mut self) -> Result<u64> {
        self.begin_frame(1)?;
        self.wait()?;
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0;
        // SAFETY: target descriptor and footprint/size output pointers are valid.
        unsafe {
            self.device.GetCopyableFootprints(
                &self.target.GetDesc(),
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
            &self.device,
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
        self.reset()?;
        if let Some(queries) = &self.queries {
            queries.begin(&self.commands);
        }
        transition(
            &self.commands,
            &self.target,
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
            pResource: ManuallyDrop::new(Some(self.target.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        // SAFETY: locations describe live, compatible resources in COPY states; retained COM clones are released once the synchronous recording call ends.
        unsafe {
            self.commands.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            ManuallyDrop::drop(&mut dst.pResource);
            ManuallyDrop::drop(&mut src.pResource);
        }
        transition(
            &self.commands,
            &self.target,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        if let Some(queries) = &self.queries {
            queries.end(&self.commands);
        }
        self.submit()?;
        self.wait()?;
        if let Some(queries) = &self.queries {
            let [begin, end] = queries.read_after_fence();
            let elapsed = end
                .checked_sub(begin)
                .ok_or("reversed validation copy timestamps")?;
            log(&format!(
                "GPU timestamp validation copy: {elapsed} ticks at {} Hz",
                queries.frequency
            ));
            self.timestamp_validation_copy_ticks.push(elapsed);
        }
        let len = usize::try_from(total).map_err(|_| "readback too large")?;
        let range = D3D12_RANGE { Begin: 0, End: len };
        let mut data = ptr::null_mut();
        // SAFETY: fence completion makes the full readback available; range fits the allocation.
        let verified = unsafe {
            com(
                readback.Map(0, Some(&range), Some(&mut data)),
                "map readback",
            )?;
            if data.is_null() {
                return Err("null readback pointer".into());
            }
            let hash = verify_red(
                core::slice::from_raw_parts(data.cast::<u8>(), len),
                footprint.Footprint.RowPitch as usize,
                self.width as usize,
                self.height as usize,
            );
            readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            hash
        };
        verified.ok_or_else(|| "D3D12 readback did not contain every expected red pixel".into())
    }
}

fn resource(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
    desc: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource> {
    let mut result = None;
    // SAFETY: descriptor is initialized, heap masks select the sole device node, and the requested COM output type is ID3D12Resource.
    unsafe {
        com(
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: heap,
                    CreationNodeMask: 1,
                    VisibleNodeMask: 1,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                desc,
                state,
                None,
                &mut result,
            ),
            "create resource",
        )?;
    }
    result.ok_or_else(|| "missing created resource".into())
}

fn transition(
    commands: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    // SAFETY: initialized union tag matches Transition, resource is retained during recording.
    unsafe {
        commands.ResourceBarrier(core::slice::from_ref(&barrier));
        ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
    }
}

fn measure(gpu: &mut Gpu, config: &Config, mode: &str, frequency: u64) -> Result<Sample> {
    let run = |gpu: &mut Gpu| -> Result<(u64, [u64; 6], Option<GpuObservation>)> {
        let mut stages = [0; 6];
        let frame = if mode != "cpu" {
            let frame = gpu.begin_frame(config.gpu_clears)?;
            stages[0] = frame.record;
            stages[1] = frame.submit;
            Some(frame)
        } else {
            None
        };
        let before = counter()?;
        let checksum = if mode != "d3d12" {
            cpu_work(core::hint::black_box(config.cpu_rounds))
        } else {
            0
        };
        stages[2] = counter()? - before;
        let mut observation = None;
        if mode != "cpu" {
            let wait = gpu.wait().map_err(|e| {
                format!(
                    "{e}; record_ticks={}, submit_ticks={}, cpu_ticks={}",
                    stages[0], stages[1], stages[2]
                )
            })?;
            stages[3..].copy_from_slice(&wait);
            if let (Some(queries), Some(frame)) = (&gpu.queries, frame) {
                let fence_verified_qpc = counter()?;
                let [gpu_begin_ticks, gpu_end_ticks] = queries.read_after_fence();
                observation = Some(GpuObservation {
                    fence: gpu.serial,
                    gpu_begin_ticks,
                    gpu_end_ticks,
                    submit_begin_qpc: frame.submit_begin_qpc,
                    submit_end_qpc: frame.submit_end_qpc,
                    fence_verified_qpc,
                });
            }
        }
        Ok((checksum, stages, observation))
    };
    let warm_end = counter()? + frequency * u64::from(config.warmup_ms) / 1000;
    while counter()? < warm_end {
        core::hint::black_box(run(gpu)?);
    }
    let clock_before = if mode != "cpu" && gpu.queries.is_some() {
        Some(gpu.calibrate()?)
    } else {
        None
    };
    let start = counter()?;
    let end = start + frequency * u64::from(config.sample_ms) / 1000;
    let expected = if mode == "d3d12" {
        0
    } else {
        cpu_work(config.cpu_rounds)
    };
    let mut times = Vec::with_capacity(1_048_576);
    let mut stage_samples = Vec::with_capacity(1_048_576);
    let mut observations = Vec::with_capacity(if clock_before.is_some() { 1_048_576 } else { 0 });
    let mut iterations = 0_u64;
    let mut last = start;
    while last < end {
        let before = counter()?;
        let (checksum, stages, observation) = core::hint::black_box(run(gpu)?);
        if checksum != expected {
            return Err("CPU checksum changed".into());
        }
        last = counter()?;
        if times.len() == times.capacity() {
            return Err("timing sample capacity exceeded".into());
        }
        times.push(last - before);
        stage_samples.push(stages);
        if let Some(observation) = observation {
            observations.push(observation);
        }
        iterations += 1;
    }
    let gpu_timing = if let (Some(before), Some(queries)) = (clock_before, &gpu.queries) {
        if observations.len() != iterations as usize {
            return Err("missing completed GPU timestamp samples".into());
        }
        Some(
            summarize_gpu(
                &observations,
                queries.frequency,
                frequency,
                before,
                gpu.calibrate()?,
            )
            .ok_or("invalid completed GPU timestamp samples")?,
        )
    } else {
        None
    };
    times.sort_unstable();
    let elapsed = (last - start) as f64 / frequency as f64;
    let us = |ticks| ticks as f64 * 1_000_000.0 / frequency as f64;
    let mut stage_stats = alloc::collections::BTreeMap::new();
    let mut ticks = Vec::with_capacity(stage_samples.len());
    for (index, name) in [
        "record",
        "submit",
        "cpu_work",
        "fence_query",
        "fence_arm",
        "event_wait",
    ]
    .into_iter()
    .enumerate()
    {
        ticks.clear();
        ticks.extend(stage_samples.iter().map(|values| values[index]));
        stage_stats.insert(
            name.into(),
            timing_summary(&mut ticks, frequency).ok_or("no stage samples")?,
        );
    }
    Ok(Sample {
        mode: mode.into(),
        iterations,
        elapsed_ms: elapsed * 1000.0,
        iterations_per_second: iterations as f64 / elapsed,
        p50_us: us(percentile(&times, 50)),
        p95_us: us(percentile(&times, 95)),
        p99_us: us(percentile(&times, 99)),
        cpu_checksum: expected,
        stages: stage_stats,
        gpu: gpu_timing,
        pipeline: None,
    })
}

fn run() -> Result<()> {
    if cpu_audit::observe_if_requested()? {
        return Ok(());
    }
    let config = read_config()?;
    let mut frequency = 0;
    // SAFETY: writable frequency output lives across the synchronous OS call.
    if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 || frequency <= 0 {
        return Err("QPC frequency unavailable".into());
    }
    if config.presentation.is_some() {
        return present::run(&config, frequency as u64);
    }
    // Protocol self-tests use their own handlers and remain separate from rendering measurements.
    cpu_audit::run_if_requested()?;
    nt_audit::run_if_requested()?;
    let mut gpu = Gpu::new(&config, frequency as u64)?;
    log("stage: verify D3D12 GPU output before timing");
    let before = gpu.verify()?;
    let mut cases = Vec::with_capacity(3);
    for mode in if config.pipeline.is_some() {
        &["cpu"][..]
    } else {
        &["cpu", "d3d12", "mixed"][..]
    } {
        log(&format!("stage: measure {mode}"));
        cases.push(measure(&mut gpu, &config, mode, frequency as u64)?);
        let progress = open("Z:\\frames\\workload-progress.json", true)?;
        write_handle(
            progress.0,
            &serde_json::to_vec_pretty(&cases).map_err(|e| e.to_string())?,
        )?;
    }
    if config.pipeline.is_some() {
        cases.extend(pipeline::measure_all(&mut gpu, &config, frequency as u64)?);
    }
    log("stage: verify D3D12 GPU output after timing");
    let after = gpu.verify()?;
    // SAFETY: all processor groups is a documented selector; no pointers are passed.
    let logical_cpus = unsafe { GetActiveProcessorCount(u16::MAX) };
    let features = core::arch::x86_64::__cpuid(1).ecx;
    let xcr0 = if features & ((1 << 26) | (1 << 27)) == ((1 << 26) | (1 << 27)) {
        // SAFETY: CPUID confirms XSAVE and OSXSAVE; selector zero is defined.
        unsafe { core::arch::x86_64::_xgetbv(0) }
    } else {
        0
    };
    let report = Report {
        schema: 1,
        passed: before == after,
        gpu_verified: before == after,
        workload: if config.pipeline.as_ref().is_some_and(|p| p.retirement_probe) {
            "windows_d3d12_allocator_retirement_diagnostic"
        } else if config.pipeline.as_ref().is_some_and(|p| p.recording_probe) {
            "windows_d3d12_recording_discard_diagnostic"
        } else if config
            .pipeline
            .as_ref()
            .is_some_and(|p| p.command_stage_probe)
        {
            "windows_d3d12_shader_cpu_v4_pipelined_command_stages"
        } else if config.pipeline.is_some() {
            "windows_d3d12_shader_cpu_v4_pipelined"
        } else if config.gpu_timestamps {
            "windows_d3d12_clear_cpu_v3_gpu_timestamps"
        } else {
            "windows_d3d12_clear_cpu_v2_stages"
        }
        .into(),
        adapter: gpu.adapter_name.clone(),
        vendor_id: gpu.vendor_id,
        device_id: gpu.device_id,
        logical_cpus,
        xcr0,
        qpc_frequency: frequency as u64,
        diagnostic_completion_kicks: gpu.diagnostic_kicks.get(),
        timestamp_validation_copy_ticks: gpu.timestamp_validation_copy_ticks.clone(),
        expected_cpu_checksum: cpu_work(config.cpu_rounds),
        config,
        image_hash_before: before,
        image_hash_after: after,
        cases,
    };
    let bytes = serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?;
    let output = open("Z:\\frames\\workload-result.json", true)?;
    write_handle(output.0, &bytes)?;
    log("LEGION_PROTON_WORKLOAD_PASSED");
    Ok(())
}

#[unsafe(no_mangle)]
extern "system" fn mainCRTStartup() -> ! {
    let code = match run().and_then(|()| nt_audit::finish()) {
        Ok(()) => 0,
        Err(e) => {
            log(&format!("LEGION_PROTON_WORKLOAD_FAILED: {e}"));
            if let Ok(file) = open("Z:\\frames\\workload-error.txt", true) {
                let _ = write_handle(file.0, e.as_bytes());
            }
            1
        }
    };
    // Retain the owned application's exit intent separately from Wine/Proton's eventual process status, including failures after successful rendering.
    // SAFETY: observes the owned Windows process's identity only.
    let pid = unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };
    if let Ok(output) = open(
        &format!("Z:\\frames\\workload-exit-intent-{pid}.json"),
        true,
    ) {
        let _ = write_handle(output.0, &format!("{{\"code\":{code}}}").into_bytes());
    }
    // SAFETY: only terminates this owned workload process after resources drop.
    unsafe { ExitProcess(code) }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    log("LEGION_PROTON_WORKLOAD_PANIC");
    // SAFETY: terminates only this owned workload on an unrecoverable invariant failure.
    unsafe { ExitProcess(3) }
}
