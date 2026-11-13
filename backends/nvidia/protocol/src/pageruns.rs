pub const PAGE_RUNS: u32 = u32::MAX - 1;

/// The most runs one registration may describe.
pub const MAX_RUNS: usize = 1024;

/// One run: a guest-physical address and a length, both whole pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub gpa: u64,
    pub len: u64,
}

/// Bytes a table of `n` runs takes.
pub const fn encoded_len(n: usize) -> usize {
    8 + n * 16
}

/// Write a table of runs into `out`.
pub fn encode(out: &mut [u8], runs: &[Run]) -> Option<usize> {
    if runs.len() > MAX_RUNS {
        return None;
    }
    let total = encoded_len(runs.len());
    let out = out.get_mut(..total)?;
    out.fill(0);
    out[0..4].copy_from_slice(&(runs.len() as u32).to_le_bytes());
    for (i, r) in runs.iter().enumerate() {
        let e = 8 + i * 16;
        out[e..e + 8].copy_from_slice(&r.gpa.to_le_bytes());
        out[e + 8..e + 16].copy_from_slice(&r.len.to_le_bytes());
    }
    Some(total)
}

/// A table of runs, as it arrived.
#[derive(Clone, Copy, Debug)]
pub struct Runs<'a> {
    bytes: &'a [u8],
    count: usize,
}

impl<'a> Runs<'a> {
    /// Read a table of runs.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let count = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
        if count == 0 || count > MAX_RUNS || bytes.len() < encoded_len(count) {
            return None;
        }
        Some(Runs { bytes, count })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = Run> + 'a {
        let bytes = self.bytes;
        (0..self.count).map(move |i| {
            let e = 8 + i * 16;
            Run {
                gpa: u64::from_le_bytes(bytes[e..e + 8].try_into().expect("checked in parse")),
                len: u64::from_le_bytes(bytes[e + 8..e + 16].try_into().expect("checked in parse")),
            }
        })
    }
}

#[cfg(test)]
#[path = "../tests/unit/pageruns.rs"]
mod tests;
