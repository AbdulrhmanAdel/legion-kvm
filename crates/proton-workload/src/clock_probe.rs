//! Diagnostic GPU/monotonic-raw clock pairs.
#[cfg(unix)]
mod linux {
    use anyhow::{Context, Result, ensure};
    use ash::{Entry, vk};
    use std::{
        ffi::CStr,
        io::Write,
        time::{Duration, Instant},
    };

    struct OwnedInstance {
        handle: ash::Instance,
        _entry: Entry,
    }
    impl Drop for OwnedInstance {
        fn drop(&mut self) {
            // SAFETY: device owners drop first; this owner uniquely destroys the instance while its dynamically loaded entry remains live.
            unsafe {
                self.handle.destroy_instance(None);
            }
        }
    }

    struct Clock {
        device: ash::Device,
        _instance: OwnedInstance,
        extension: ash::ext::calibrated_timestamps::Device,
        metadata: serde_json::Value,
    }
    impl Drop for Clock {
        fn drop(&mut self) {
            // SAFETY: this diagnostic never submits work and calibration calls are synchronous.
            unsafe {
                self.device.destroy_device(None);
            }
        }
    }

    fn raw_ns() -> Result<u64> {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid writable timespec for this synchronous clock query.
        let status = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut value) };
        ensure!(
            status == 0,
            "clock_gettime: {}",
            std::io::Error::last_os_error()
        );
        u64::try_from(value.tv_sec)?
            .checked_mul(1_000_000_000)
            .and_then(|s| {
                u64::try_from(value.tv_nsec)
                    .ok()
                    .and_then(|ns| s.checked_add(ns))
            })
            .context("monotonic clock outside u64 nanoseconds")
    }

    impl Clock {
        fn new(vendor: u32, device_id: u32) -> Result<Self> {
            // SAFETY: load the installed Vulkan loader; OwnedInstance retains the entry until its device and instance have been destroyed.
            let entry = unsafe { Entry::load() }.context("load Vulkan")?;
            let app = vk::ApplicationInfo::default()
                .application_name(c"legion-gpu-clock-probe")
                .api_version(vk::API_VERSION_1_2);
            // SAFETY: initialized instance descriptor references live app data; no surface, layers, custom allocator or instance extensions needed.
            let handle = unsafe {
                entry.create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app),
                    None,
                )
            }
            .context("create clock-probe instance")?;
            let instance = OwnedInstance {
                handle,
                _entry: entry,
            };
            // SAFETY: instance remains live for enumeration and property queries.
            let devices = unsafe { instance.handle.enumerate_physical_devices() }
                .context("enumerate GPUs")?;
            let mut chosen = None;
            for physical in devices {
                // SAFETY: enumerated physical device belongs to this instance.
                let props = unsafe { instance.handle.get_physical_device_properties(physical) };
                if (vendor == 0
                    && device_id == 0
                    && props.device_type != vk::PhysicalDeviceType::CPU)
                    || (props.vendor_id == vendor && props.device_id == device_id)
                {
                    ensure!(
                        chosen.is_none(),
                        "ambiguous GPUs with the requested identifiers"
                    );
                    chosen = Some((physical, props));
                }
            }
            let (physical, props) = chosen.context("requested hardware GPU not found")?;
            // SAFETY: physical device belongs to the live instance; this query returns owned extension properties without retaining output pointers.
            let extensions = unsafe {
                instance
                    .handle
                    .enumerate_device_extension_properties(physical)
            }
            .context("enumerate device extensions")?;
            ensure!(
                extensions.iter().any(|p| {
                    // SAFETY: Vulkan initializes each fixed-size extension name as a NUL-terminated string within its property record.
                    let name = unsafe { CStr::from_ptr(p.extension_name.as_ptr()) };
                    name == ash::ext::calibrated_timestamps::NAME
                }),
                "VK_EXT_calibrated_timestamps is unavailable"
            );
            let ext =
                ash::ext::calibrated_timestamps::Instance::new(&instance._entry, &instance.handle);
            // SAFETY: the device extension is advertised by this physical device; the queried output is owned by the extension wrapper.
            let domains = unsafe { ext.get_physical_device_calibrateable_time_domains(physical) }
                .context("enumerate clock domains")?;
            ensure!(
                domains.contains(&vk::TimeDomainEXT::DEVICE)
                    && domains.contains(&vk::TimeDomainEXT::CLOCK_MONOTONIC_RAW),
                "GPU and CLOCK_MONOTONIC_RAW domains are required"
            );
            // SAFETY: live enumerated physical device, output records are owned.
            let families = unsafe {
                instance
                    .handle
                    .get_physical_device_queue_family_properties(physical)
            };
            let (family, queue) = families
                .iter()
                .enumerate()
                .find(|(_, p)| {
                    p.queue_flags.contains(vk::QueueFlags::GRAPHICS) && p.timestamp_valid_bits > 0
                })
                .context("no timestamp-capable graphics queue")?;
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(u32::try_from(family)?)
                .queue_priorities(&[1.0])];
            let names = [ash::ext::calibrated_timestamps::NAME.as_ptr()];
            // SAFETY: queue family and extension were checked above; slices stay live through device creation and no optional features are enabled.
            let device = unsafe {
                instance.handle.create_device(
                    physical,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&queues)
                        .enabled_extension_names(&names),
                    None,
                )
            }
            .context("create clock-probe device")?;
            let extension = ash::ext::calibrated_timestamps::Device::new(&instance.handle, &device);
            // SAFETY: Vulkan's device_name field is an initialized NUL-terminated byte array in the owned physical-device property record.
            let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy();
            let metadata = serde_json::json!({"kind":"metadata","schema":1,
                "vendor_id":props.vendor_id,"device_id":props.device_id,"name":name,
                "timestamp_period_ns":props.limits.timestamp_period,
                "timestamp_valid_bits":queue.timestamp_valid_bits});
            Ok(Self {
                device,
                _instance: instance,
                extension,
                metadata,
            })
        }

        fn sample(&self, sequence: u64) -> Result<serde_json::Value> {
            let info = [
                vk::CalibratedTimestampInfoEXT::default().time_domain(vk::TimeDomainEXT::DEVICE),
                vk::CalibratedTimestampInfoEXT::default()
                    .time_domain(vk::TimeDomainEXT::CLOCK_MONOTONIC_RAW),
            ];
            let before = raw_ns()?;
            // SAFETY: both time domains were verified; initialized descriptors and live device/extension outlive this synchronous query.
            let (ticks, deviation) = unsafe { self.extension.get_calibrated_timestamps(&info) }
                .context("sample GPU/raw clocks")?;
            let after = raw_ns()?;
            ensure!(ticks.len() == 2, "missing calibrated timestamps");
            Ok(serde_json::json!({"kind":"clock","sequence":sequence,
                "gpu_ticks":ticks[0],"monotonic_raw_ns":ticks[1],
                "call_begin_ns":before,"call_end_ns":after,"max_deviation_ns":deviation}))
        }
    }

    pub fn run() -> Result<()> {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        ensure!(
            args.len() == 4,
            "usage: legion-gpu-clock-probe VENDOR_ID DEVICE_ID DURATION_MS INTERVAL_MS"
        );
        let vendor = args[0].parse()?;
        let device = args[1].parse()?;
        let duration: u64 = args[2].parse()?;
        let interval: u64 = args[3].parse()?;
        ensure!(
            (100..=120_000).contains(&duration) && (10..=1000).contains(&interval),
            "clock sampling requires duration 100..120000 ms and interval 10..1000 ms"
        );
        let clock = Clock::new(vendor, device)?;
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        writeln!(out, "{}", clock.metadata)?;
        out.flush()?;
        let start = Instant::now();
        let mut sequence = 0;
        while start.elapsed() < Duration::from_millis(duration) {
            writeln!(out, "{}", clock.sample(sequence)?)?;
            out.flush()?;
            sequence += 1;
            std::thread::sleep(Duration::from_millis(interval));
        }
        Ok(())
    }
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(not(unix))]
fn main() {}
