use super::Sample;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::FileExt};

const DRIVER: &str = "/usr/lib/libnvidia-eglcore.so.615.78.08";
const YIELD_RET: u64 = 0x97b18c;
const BACKOFF_RET: u64 = 0xa8bc22;
const OUTER_RET: u64 = 0x9b21d7;

struct Mapping<'a> {
    start: u64,
    end: u64,
    offset: u64,
    readable: bool,
    executable: bool,
    path: &'a str,
}
impl<'a> Mapping<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let mut fields = line.split_whitespace();
        let (start, end) = fields.next()?.split_once('-')?;
        let perms = fields.next()?.as_bytes();
        let offset = u64::from_str_radix(fields.next()?, 16).ok()?;
        fields.next()?;
        fields.next()?;
        Some(Self {
            start: u64::from_str_radix(start, 16).ok()?,
            end: u64::from_str_radix(end, 16).ok()?,
            offset,
            readable: perms.first() == Some(&b'r'),
            executable: perms.get(2) == Some(&b'x'),
            path: fields.next().unwrap_or_default(),
        })
    }
    fn contains(&self, address: u64, length: u64) -> bool {
        address >= self.start
            && address
                .checked_add(length)
                .is_some_and(|end| end <= self.end)
    }
    fn file_offset(&self, address: u64) -> Option<u64> {
        address.checked_sub(self.start)?.checked_add(self.offset)
    }
}

fn read<const N: usize>(mem: &fs::File, maps: &[Mapping<'_>], address: u64) -> Result<[u8; N]> {
    ensure!(
        maps.iter()
            .any(|m| m.readable && m.contains(address, N as u64)),
        "scalar lies outside a readable mapping"
    );
    let mut bytes = [0; N];
    mem.read_exact_at(&mut bytes, address)?;
    Ok(bytes)
}

fn offset(address: u64, delta: u64) -> Result<u64> {
    address.checked_add(delta).context("address overflow")
}

fn resolve(maps: &[Mapping<'_>], address: u64) -> Result<Value> {
    let map = maps
        .iter()
        .find(|m| m.executable && m.contains(address, 1))
        .context("callback is not in an executable mapping")?;
    Ok(json!({"path":map.path,"file_offset":map.file_offset(address),"address":address}))
}

fn sample_context(sample: &Sample, base: u64) -> Option<(u64, bool)> {
    if sample.regs.len() != 18 || sample.abi != 2 {
        return None;
    }
    match sample.ip.checked_sub(base)? {
        YIELD_RET
            if sample.stack_words.len() >= 9
                && sample.stack_words[0].checked_sub(base) == Some(BACKOFF_RET)
                && sample.stack_words[8].checked_sub(base) == Some(OUTER_RET) =>
        {
            Some((sample.regs[15], true)) // R13 preserved by yield helper.
        }
        0xa8b30c | 0xa8b30e if sample.stack_words.first()?.checked_sub(base) == Some(0xa7f2aa) => {
            Some((sample.regs[5], false)) // RDI preserved by this scalar reader.
        }
        _ => None,
    }
}

fn snapshot(pid: i32, sample: &Sample, maps: &[Mapping<'_>], base: u64) -> Result<Value> {
    let mem = fs::File::open(format!("/proc/{pid}/mem"))?;
    // Fingerprint the exact disassembled PAUSE/RET and indirect-call return.
    ensure!(
        read::<3>(&mem, maps, offset(base, YIELD_RET - 2)?)? == [0xf3, 0x90, 0xc3],
        "yield signature differs"
    );
    ensure!(
        read::<6>(&mem, maps, offset(base, BACKOFF_RET - 6)?)? == [0xff, 0x95, 0x88, 2, 0, 0],
        "backoff signature differs"
    );
    let (context, in_backoff) =
        sample_context(sample, base).context("unrecognized sample frame")?;
    let channel = u32::from_le_bytes(read::<4>(&mem, maps, offset(context, 0x1060)?)?);
    ensure!(channel < 4096, "channel index exceeds diagnostic bound");
    let scalar =
        |address| -> Result<u64> { Ok(u64::from_le_bytes(read::<8>(&mem, maps, address)?)) };
    let reader = scalar(offset(context, 0x1900)?)?;
    let global = offset(base, 0x259ec60)?;
    let first = scalar(offset(global, 0x5e0)?)?;
    let second = scalar(offset(global, 0x5e8)?)?;
    let index = u64::from(channel) * 8;
    let clock = scalar(offset(base, 0x259d960 + 0x290)?)?;
    let word = |delta| -> Result<u32> {
        Ok(u32::from_le_bytes(read::<4>(
            &mem,
            maps,
            offset(context, delta)?,
        )?))
    };
    let flags = word(0x318)?;
    let fallback = word(0x224)?;
    let mut progress_source = json!({"flags_318":flags,"fallback_224":fallback,
        "reader_count":word(0x22e3c)?,"last_progress":scalar(offset(context,0x1070)?)?,
        "fallback_progress":scalar(offset(context,0x1188)?)?});
    if reader.checked_sub(base) == Some(0xa886b0) && flags & 1 == 0 && fallback == 0 {
        let object = scalar(offset(context, 0x1470)?)?;
        let surface = scalar(offset(object, 0x58)?)?;
        // a8b2e0 shifts the 32-bit 0x800000bb selector by four, yielding 0xbb0.
        let address = offset(surface, 0xbb0)?;
        let map = maps
            .iter()
            .find(|m| m.contains(address, 4))
            .context("progress surface not mapped")?;
        progress_source["semaphore"] = json!({"address":address,"mapping_path":map.path,
        "mapping_start":map.start,"mapping_end":map.end,"mapping_offset":map.offset,
        "value":match read::<4>(&mem,maps,address) {
            Ok(value)=>json!({"u32":u32::from_le_bytes(value)}),
            Err(error)=>json!({"unavailable":format!("{error:#}")}),
        }});
    }
    Ok(json!({"context":context,"channel":channel,
        "sample_site":sample.ip.checked_sub(base),"in_backoff":in_backoff,
        "sampled_target":in_backoff.then(||sample.stack_words[4]),
        "sampled_category":in_backoff.then(||sample.stack_words[6]),
        "progress_source":progress_source,
        "cached_progress":scalar(offset(context, 0x1078)?)?,
        "reader":resolve(maps,reader)?,"clock":resolve(maps,clock)?,
        "first_counter":scalar(offset(first,index)?)?,
        "second_counter":scalar(offset(second,index)?)?}))
}

#[derive(Default)]
pub(super) struct Probe {
    snapshots: Vec<Value>,
    last_ns: Option<u64>,
}
impl Probe {
    pub(super) fn due(&self, now: u64) -> bool {
        self.snapshots.len() < 8
            && self
                .last_ns
                .is_none_or(|last| now.saturating_sub(last) >= 1_000_000_000)
    }
    pub(super) fn observe(&mut self, pid: i32, tid: i32, samples: &[Sample], now: u64) {
        if !self.due(now) {
            return;
        }
        let Ok(text) = fs::read_to_string(format!("/proc/{pid}/maps")) else {
            return;
        };
        let maps: Vec<_> = text.lines().filter_map(Mapping::parse).collect();
        let Some(driver) = maps.iter().find(|m| m.path == DRIVER && m.executable) else {
            return;
        };
        let Some(base) = driver.start.checked_sub(driver.offset) else {
            return;
        };
        let Some(sample) = samples.iter().rev().find(|s| {
            i32::try_from(s.pid).ok() == Some(pid)
                && i32::try_from(s.tid).ok() == Some(tid)
                && now
                    .checked_sub(s.time_ns)
                    .is_some_and(|age| age <= 50_000_000)
                && driver.contains(s.ip, 1)
                && sample_context(s, base).is_some()
        }) else {
            return;
        };
        self.last_ns = Some(now);
        let result = match snapshot(pid, sample, &maps, base) {
            Ok(value) => value,
            Err(error) => json!({"unavailable":format!("{error:#}")}),
        };
        self.snapshots
            .push(json!({"sample_time_ns":sample.time_ns,"read_time_ns":now,"values":result}));
    }
    pub(super) fn report(&self) -> Value {
        json!({"build_specific":true,"read_only":true,"snapshots":self.snapshots,
            "uncertainty":"post-sample, non-atomic scalar reads; counters may advance and context may expire before reading"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_requires_the_exact_caller_and_register_abi() {
        let base = 0x10000000;
        let mut sample = Sample {
            ip: base + YIELD_RET,
            pid: 7,
            tid: 9,
            time_ns: 0,
            abi: 2,
            regs: vec![0; 18],
            stack_words: vec![0; 9],
        };
        sample.regs[15] = 0x1234;
        sample.regs[5] = 0x5678;
        sample.stack_words[0] = base + BACKOFF_RET;
        sample.stack_words[8] = base + OUTER_RET;
        assert_eq!(sample_context(&sample, base), Some((0x1234, true)));
        sample.stack_words[8] += 1;
        assert_eq!(sample_context(&sample, base), None);
        sample.ip = base + 0xa8b30e;
        sample.stack_words[0] = base + 0xa7f2aa;
        assert_eq!(sample_context(&sample, base), Some((0x5678, false)));
        sample.stack_words.clear();
        assert_eq!(sample_context(&sample, base), None);
        sample.abi = 1;
        assert_eq!(sample_context(&sample, base), None);
    }
    #[test]
    fn mappings_reject_overflow_and_cross_boundary_reads() {
        let m = Mapping::parse("1000-2000 r-xp 00001000 00:01 1 /driver.so").expect("map");
        assert!(m.contains(0x1ff8, 8));
        assert!(!m.contains(0x1ffc, 8));
        assert!(!m.contains(u64::MAX - 1, 8));
        assert_eq!(m.file_offset(0x1234), Some(0x1234));
        assert!(resolve(&[m], 0x2000).is_err());
        assert!(offset(u64::MAX, 1).is_err());
        assert!(Mapping::parse("bad maps").is_none());
    }
}
