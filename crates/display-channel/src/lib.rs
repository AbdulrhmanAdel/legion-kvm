//! Bounded, single-producer/single-consumer RGBA transport for an owned VM.
pub mod desktop_input;
pub mod input_events;
pub mod rpc;

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use memmap2::{MmapMut, MmapOptions};

const MAGIC: u64 = 0x4c47_4e32_5247_4241;
const HEADER_BYTES: usize = 4096;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const FREE: u32 = 0;
const WRITING: u32 = 1;
const READY: u32 = 2;
const READING: u32 = 3;

#[repr(C, align(64))]
struct Slot {
    state: AtomicU32,
    rgba: AtomicU32,
    frame: AtomicU64,
    input: AtomicU64,
    gpu_ns: AtomicU64,
    copy_ns: AtomicU64,
    kind: AtomicU32,
}

#[repr(C, align(64))]
struct Header {
    magic: AtomicU64,
    version: AtomicU32,
    producer: AtomicU32,
    consumer: AtomicU32,
    stop: AtomicU32,
    width: AtomicU32,
    height: AtomicU32,
    input: AtomicU64,
    slots: [Slot; 2],
}

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_BYTES);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Input(u64);

impl Input {
    pub fn new(x: u16, y: u16, space: bool, generation: u32) -> Result<Self> {
        ensure!(generation <= 0xff_ffff, "input generation exhausted");
        Ok(Self(
            u64::from(x)
                | u64::from(y) << 16
                | u64::from(space) << 32
                | u64::from(generation) << 40,
        ))
    }
    pub fn generation(self) -> u32 {
        (self.0 >> 40) as u32
    }
    pub fn x(self) -> u16 {
        self.0 as u16
    }
    pub fn y(self) -> u16 {
        (self.0 >> 16) as u16
    }
    pub fn space(self) -> bool {
        self.0 & (1 << 32) != 0
    }
    pub fn rgba(self, frame: u64) -> [u8; 4] {
        [
            (self.x() >> 8) as u8,
            (self.y() >> 8) as u8,
            if self.space() {
                224
            } else {
                32 + ((frame / 2) % 128) as u8
            },
            255,
        ]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FrameMeta {
    pub kind: FrameKind,
    pub number: u64,
    pub input: Input,
    pub rgba: [u8; 4],
    pub gpu_ns: u64,
    pub copy_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    InputColor,
    Application,
    Unknown,
}

pub struct Channel {
    base: NonNull<u8>,
    pixels: NonNull<u8>,
    len: usize,
    width: u32,
    height: u32,
    file: Option<File>,
    write_combining: bool,
    // Never borrow the mmap as a slice after publishing its raw address.
    _mapping: Mapping,
}

enum Mapping {
    File {
        _map: MmapMut,
    },
    Bar {
        _header: MmapMut,
        _pixels: MmapMut,
    },
    #[cfg(test)]
    Test {
        _allocation: tests::TestAllocation,
    },
}

impl Channel {
    pub fn create(width: u32, height: u32) -> Result<Self> {
        let frame_bytes = image_bytes(width, height)?;
        let len = HEADER_BYTES
            .checked_add(
                frame_bytes
                    .checked_mul(2)
                    .context("display size overflow")?,
            )
            .context("display size overflow")?
            .checked_next_power_of_two()
            .context("display size overflow")?;
        ensure!(len <= MAX_BYTES, "display channel exceeds 128 MiB");
        let fd = rustix::fs::memfd_create(
            c"legion-display",
            rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
        )?;
        let file = File::from(fd);
        file.set_len(u64::try_from(len)?)?;
        rustix::fs::fcntl_add_seals(
            &file,
            rustix::fs::SealFlags::SHRINK
                | rustix::fs::SealFlags::GROW
                | rustix::fs::SealFlags::SEAL,
        )?;
        // SAFETY: this newly created, size-sealed private memfd is initialized before its path is exposed.
        let mut map = unsafe { MmapOptions::new().len(len).map_mut(&file)? };
        let base = NonNull::new(map.as_mut_ptr()).context("null display mapping")?;
        // SAFETY: the allocation contains the checked header and two slots.
        let pixels = unsafe { NonNull::new_unchecked(base.as_ptr().add(HEADER_BYTES)) };
        let channel = Self {
            base,
            pixels,
            len,
            width,
            height,
            file: Some(file),
            write_combining: false,
            _mapping: Mapping::File { _map: map },
        };
        channel.initialize();
        Ok(channel)
    }

    /// Attach another cooperating process to the private host memfd.
    /// # Safety
    /// The file must be the channel created by the owning Legion instance.
    pub unsafe fn attach_host(file: File) -> Result<Self> {
        let seals = rustix::fs::fcntl_get_seals(&file)?;
        ensure!(
            seals.contains(rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW),
            "display memfd must be size-sealed"
        );
        let len = usize::try_from(file.metadata()?.len())?;
        // SAFETY: caller supplies the owned protocol file; seals prevent resize.
        unsafe { Self::attach(file, len) }
    }

    /// Attach the guest's ivshmem BAR, whose size comes from PCI resources.
    /// # Safety
    /// The BAR must expose this owning VM's initialized display channel.
    pub unsafe fn attach_bar(file: File, len: usize) -> Result<Self> {
        // SAFETY: caller guarantees the BAR's ownership, extent and lifetime.
        unsafe { Self::attach(file, len) }
    }

    /// Map the control page uncached and the disjoint frame pages with WC.
    /// # Safety
    /// Both descriptors must refer to the same owned ivshmem BAR, via `resource2` and `resource2_wc` respectively.
    pub unsafe fn attach_bar_wc(header: File, pixels: File, len: usize) -> Result<Self> {
        ensure!(cfg!(target_arch = "x86_64"), "WC transport requires x86-64");
        ensure!(
            (8192..=MAX_BYTES).contains(&len) && len.is_power_of_two(),
            "invalid display extent"
        );
        // SAFETY: caller guarantees BAR ownership and bounds.
        let mut header_map = unsafe { MmapOptions::new().len(HEADER_BYTES).map_mut(&header)? };
        // SAFETY: same BAR contract; offset plus length equals its checked size.
        let mut pixel_map = unsafe {
            MmapOptions::new()
                .offset(HEADER_BYTES as u64)
                .len(len - HEADER_BYTES)
                .map_mut(&pixels)?
        };
        let channel = Self {
            base: NonNull::new(header_map.as_mut_ptr()).context("null display header")?,
            pixels: NonNull::new(pixel_map.as_mut_ptr()).context("null display pixels")?,
            len,
            width: 0,
            height: 0,
            file: Some(header),
            write_combining: true,
            _mapping: Mapping::Bar {
                _header: header_map,
                _pixels: pixel_map,
            },
        };
        channel.validate_protocol()
    }

    ///
    /// # Safety
    /// Requires the protocol ownership and mapping-lifetime guarantees of attach_host or attach_bar; the caller selects the appropriate check.
    unsafe fn attach(file: File, len: usize) -> Result<Self> {
        ensure!(
            (8192..=MAX_BYTES).contains(&len) && len.is_power_of_two(),
            "invalid display extent"
        );
        // SAFETY: forwarding the caller's protocol, size and lifetime contract.
        let mut map = unsafe { MmapOptions::new().len(len).map_mut(&file)? };
        let base = NonNull::new(map.as_mut_ptr()).context("null display mapping")?;
        // SAFETY: checked extent includes the header and both payload slots.
        let pixels = unsafe { NonNull::new_unchecked(base.as_ptr().add(HEADER_BYTES)) };
        let channel = Self {
            base,
            pixels,
            len,
            width: 0,
            height: 0,
            file: Some(file),
            write_combining: false,
            _mapping: Mapping::File { _map: map },
        };
        channel.validate_protocol()
    }

    fn validate_protocol(mut self) -> Result<Self> {
        ensure!(
            self.header().magic.load(Ordering::Acquire) == MAGIC
                && self.header().version.load(Ordering::Relaxed) == 2,
            "unknown display protocol"
        );
        self.width = self.header().width.load(Ordering::Relaxed);
        self.height = self.header().height.load(Ordering::Relaxed);
        ensure!(
            image_bytes(self.width, self.height)? <= self.slot_bytes(),
            "image exceeds display slot"
        );
        Ok(self)
    }

    fn finish_pixel_writes(&self) {
        if self.write_combining {
            #[cfg(all(target_arch = "x86_64", not(miri)))]
            // SAFETY: SFENCE is available on x86-64 and changes no memory contents.
            unsafe {
                std::arch::asm!("sfence", options(nostack, preserves_flags));
            }
        }
    }

    fn initialize(&self) {
        let slot = || Slot {
            state: AtomicU32::new(FREE),
            rgba: AtomicU32::new(0),
            frame: AtomicU64::new(0),
            input: AtomicU64::new(0),
            gpu_ns: AtomicU64::new(0),
            copy_ns: AtomicU64::new(0),
            kind: AtomicU32::new(0),
        };
        let header = Header {
            magic: AtomicU64::new(0),
            version: AtomicU32::new(2),
            producer: AtomicU32::new(0),
            consumer: AtomicU32::new(0),
            stop: AtomicU32::new(0),
            width: AtomicU32::new(self.width),
            height: AtomicU32::new(self.height),
            input: AtomicU64::new(0),
            slots: [slot(), slot()],
        };
        // SAFETY: newly allocated page-aligned storage, large enough for Header; initialize runs exactly once, before any peer receives the address.
        unsafe { self.base.cast::<Header>().as_ptr().write(header) };
        self.header().magic.store(MAGIC, Ordering::Release);
    }

    fn header(&self) -> &Header {
        // SAFETY: storage is initialized, page-aligned and retained by self.
        unsafe { self.base.cast::<Header>().as_ref() }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn mapping_len(&self) -> usize {
        self.len
    }
    pub fn proc_path(&self) -> Result<PathBuf> {
        let file = self
            .file
            .as_ref()
            .context("test channel has no descriptor")?;
        Ok(PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            file.as_raw_fd()
        )))
    }
    pub fn input(&self) -> Input {
        Input(self.header().input.load(Ordering::Acquire))
    }
    pub fn stopped(&self) -> bool {
        self.header().stop.load(Ordering::Acquire) != 0
    }
    pub fn stop(&self) {
        self.header().stop.store(1, Ordering::Release);
    }
    fn slot_bytes(&self) -> usize {
        (self.len - HEADER_BYTES) / 2
    }

    pub fn producer(&self) -> Result<Producer<'_>> {
        ensure!(
            self.header()
                .producer
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok(),
            "display already has a producer"
        );
        Ok(Producer { channel: self })
    }
    pub fn consumer(&self) -> Result<Consumer<'_>> {
        ensure!(
            self.header()
                .consumer
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok(),
            "display already has a consumer"
        );
        Ok(Consumer { channel: self })
    }
}

pub struct Producer<'a> {
    channel: &'a Channel,
}
impl Producer<'_> {
    pub fn publish(&mut self, pixels: &[u8], mut meta: FrameMeta) -> Result<bool> {
        ensure!(
            pixels.len() == image_bytes(self.channel.width, self.channel.height)?,
            "incorrect frame length"
        );
        for (index, slot) in self.channel.header().slots.iter().enumerate() {
            if slot
                .state
                .compare_exchange(FREE, WRITING, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            let start = std::time::Instant::now();
            let offset = index * self.channel.slot_bytes();
            // SAFETY: WRITING grants this sole producer exclusive slot access.
            unsafe {
                copy_pixels(
                    pixels.as_ptr(),
                    self.channel.pixels.as_ptr().add(offset),
                    pixels.len(),
                    self.channel.write_combining,
                )
            };
            self.channel.finish_pixel_writes();
            meta.copy_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
            slot.rgba
                .store(u32::from_le_bytes(meta.rgba), Ordering::Relaxed);
            slot.frame.store(meta.number, Ordering::Relaxed);
            slot.input.store(meta.input.0, Ordering::Relaxed);
            slot.gpu_ns.store(meta.gpu_ns, Ordering::Relaxed);
            slot.copy_ns.store(meta.copy_ns, Ordering::Relaxed);
            slot.kind.store(
                match meta.kind {
                    FrameKind::InputColor => 0,
                    FrameKind::Application => 1,
                    FrameKind::Unknown => 2,
                },
                Ordering::Relaxed,
            );
            slot.state.store(READY, Ordering::Release);
            return Ok(true);
        }
        Ok(false)
    }
}
impl Drop for Producer<'_> {
    fn drop(&mut self) {
        self.channel.header().producer.store(0, Ordering::Release);
    }
}

pub struct Consumer<'a> {
    channel: &'a Channel,
}
impl Consumer<'_> {
    pub fn set_input(&mut self, input: Input) {
        self.channel
            .header()
            .input
            .store(input.0, Ordering::Release);
    }
    pub fn read(&mut self) -> Option<Frame<'_>> {
        let index = self
            .channel
            .header()
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.state.load(Ordering::Acquire) == READY)
            .max_by_key(|(_, slot)| slot.frame.load(Ordering::Relaxed))
            .map(|(index, _)| index)?;
        let slot = &self.channel.header().slots[index];
        if slot
            .state
            .compare_exchange(READY, READING, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            let meta = FrameMeta {
                kind: match slot.kind.load(Ordering::Relaxed) {
                    0 => FrameKind::InputColor,
                    1 => FrameKind::Application,
                    _ => FrameKind::Unknown,
                },
                number: slot.frame.load(Ordering::Relaxed),
                input: Input(slot.input.load(Ordering::Relaxed)),
                rgba: slot.rgba.load(Ordering::Relaxed).to_le_bytes(),
                gpu_ns: slot.gpu_ns.load(Ordering::Relaxed),
                copy_ns: slot.copy_ns.load(Ordering::Relaxed),
            };
            for (other_index, other) in self.channel.header().slots.iter().enumerate() {
                if other_index != index
                    && other.state.load(Ordering::Acquire) == READY
                    && other.frame.load(Ordering::Relaxed) < meta.number
                {
                    let _ = other.state.compare_exchange(
                        READY,
                        FREE,
                        Ordering::Release,
                        Ordering::Relaxed,
                    );
                }
            }
            return Some(Frame {
                channel: self.channel,
                index,
                meta,
            });
        }
        None
    }
}
impl Drop for Consumer<'_> {
    fn drop(&mut self) {
        self.channel.header().consumer.store(0, Ordering::Release);
    }
}

pub struct Frame<'a> {
    channel: &'a Channel,
    index: usize,
    pub meta: FrameMeta,
}
impl Frame<'_> {
    pub fn pixels(&self) -> &[u8] {
        let len = self.channel.width as usize * self.channel.height as usize * 4;
        let offset = self.index * self.channel.slot_bytes();
        // SAFETY: this READING guard owns the validated slot until Drop.
        unsafe { std::slice::from_raw_parts(self.channel.pixels.as_ptr().add(offset), len) }
    }
}
impl Drop for Frame<'_> {
    fn drop(&mut self) {
        self.channel.header().slots[self.index]
            .state
            .store(FREE, Ordering::Release);
    }
}

///
/// # Safety
/// The source and destination must cover `len` bytes, must not overlap, and must remain exclusively readable/writable respectively until return.
unsafe fn copy_pixels(src: *const u8, dst: *mut u8, len: usize, write_combining: bool) {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    if write_combining
        && src.align_offset(16) == 0
        && dst.align_offset(16) == 0
        && std::is_x86_feature_detected!("sse4.1")
    {
        // SAFETY: the feature and alignment checks meet the intrinsic preconditions; the caller guarantees the full nonoverlapping range.
        unsafe { copy_wc_aligned(src, dst, len) };
        return;
    }
    let _ = write_combining;
    // SAFETY: forwarding the caller's valid, nonoverlapping byte ranges.
    unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
}

/// Copy mapped GPU readback efficiently without requiring AVX or AVX-512.
/// # Safety
/// Requires SSE4.1, 16-byte aligned nonoverlapping ranges valid for `len` bytes, and a caller SFENCE before publishing or consuming the destination.
#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "sse4.1")]
unsafe fn copy_wc_aligned(src: *const u8, dst: *mut u8, len: usize) {
    use std::arch::x86_64::{__m128i, _mm_stream_load_si128, _mm_stream_si128};
    let blocks = len / 64;
    for block in 0..blocks {
        // SAFETY: each group reads and writes one complete 64-byte block within the caller's aligned ranges.
        unsafe {
            let s = src.add(block * 64).cast::<__m128i>();
            let d = dst.add(block * 64).cast::<__m128i>();
            let a = _mm_stream_load_si128(s);
            let b = _mm_stream_load_si128(s.add(1));
            let c = _mm_stream_load_si128(s.add(2));
            let e = _mm_stream_load_si128(s.add(3));
            _mm_stream_si128(d, a);
            _mm_stream_si128(d.add(1), b);
            _mm_stream_si128(d.add(2), c);
            _mm_stream_si128(d.add(3), e);
        }
    }
    let copied = blocks * 64;
    // SAFETY: only the remaining in-bounds tail is copied; the ranges do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(src.add(copied), dst.add(copied), len - copied) };
}

fn image_bytes(width: u32, height: u32) -> Result<usize> {
    ensure!(
        width != 0 && height != 0 && width <= 3840 && height <= 2160,
        "display dimensions must be 1..3840 by 1..2160"
    );
    usize::try_from(width)?
        .checked_mul(usize::try_from(height)?)
        .and_then(|n| n.checked_mul(4))
        .context("image size overflow")
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
