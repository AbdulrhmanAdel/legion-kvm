//! Pointers inside an RM control's parameters.

use abi::rmctrl::{MAX_EMBEDDED_BYTES, RmCtrlEntry};
use protocol::segments::Segments;

use crate::guarded::{GuardPool, Lease};

/// Status codes, from `nvstatuscodes.h`.
pub(super) const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
pub(super) const NV_ERR_NO_MEMORY: u32 = 0x51;

/// Every embedded pointer of one call together.
const MAX_TOTAL_BYTES: usize = 2 * MAX_EMBEDDED_BYTES;

/// One pointer of a control, resolved against the block the guest sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Slot {
    pub ptr_offset: usize,
    /// What RM will copy, from the count field in the guest's own block.
    pub len: usize,
    pub copy_in: bool,
    pub copy_out: bool,
}

/// What a control's pointers need, or the status to refuse it with.
fn sent<'b>(
    segs: Option<Segments<'b>>,
    legacy: Option<(usize, &'b [u8])>,
    ptr_offset: usize,
) -> Option<(&'b [u8], Source)> {
    segs.and_then(|s| s.find(ptr_offset as u32))
        .map(|b| (b, Source::Segment))
        .or_else(|| {
            legacy
                .filter(|(off, _)| *off == ptr_offset)
                .map(|(_, b)| (b, Source::Legacy))
        })
}

/// Which encoding a guest's bytes arrived in, which is also how far its own idea of their length can be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// A segment table, from a guest reading this same table.
    Segment,
    /// The single pointer a v0.1 guest describes in the request struct.
    Legacy,
}

pub(super) fn plan(
    entry: &RmCtrlEntry,
    params: &[u8],
    segs: Option<Segments<'_>>,
    legacy: Option<(usize, &[u8])>,
) -> Result<Vec<Slot>, u32> {
    if entry.refuse {
        return Err(NV_ERR_NOT_SUPPORTED);
    }
    // The offsets in the table are the release's struct.
    if params.len() < entry.params_size {
        return Err(NV_ERR_NOT_SUPPORTED);
    }

    let mut slots = Vec::with_capacity(entry.ptrs.len());
    let mut total = 0usize;
    for p in entry.ptrs {
        // A null pointer is RM's own "nothing here": `RMAPI_PARAM_COPY_INIT` sizes the copy to zero and the control reads the field's null-ness in its own right.
        let guest = u64::from_le_bytes(
            params[p.ptr_offset..p.ptr_offset + 8]
                .try_into()
                .expect("params_size covers the pointer field"),
        );
        if guest == 0 {
            continue;
        }

        let Some(len) = p.bytes(params) else {
            // The count is one a guest wrote.
            return Err(NV_ERR_NOT_SUPPORTED);
        };
        total = total.saturating_add(len);
        if total > MAX_TOTAL_BYTES {
            return Err(NV_ERR_NOT_SUPPORTED);
        }

        if p.copy_in && len > 0 {
            match sent(segs, legacy, p.ptr_offset) {
                // A segment comes from a guest deriving its length from this same table, from the same count in the same field.
                Some((bytes, Source::Segment)) if bytes.len() == len => {}
                Some((_, Source::Segment)) | None => return Err(NV_ERR_NOT_SUPPORTED),

                // A v0.1 guest derived its length from a table of its own, and for some controls derived it differently: it sends `engineCount` bytes where this table reads `engineCount` four-byte entries.
                Some((bytes, Source::Legacy)) if bytes.len() <= len => {}
                Some((_, Source::Legacy)) => return Err(NV_ERR_NOT_SUPPORTED),
            }
        }

        slots.push(Slot {
            ptr_offset: p.ptr_offset,
            len,
            copy_in: p.copy_in,
            copy_out: p.copy_out,
        });
    }
    Ok(slots)
}

/// Whether the reply the guest left room for holds what RM will write back.
pub(super) fn reply_fits(slots: &[Slot], have: usize, segmented: bool) -> bool {
    let lens: Vec<usize> = slots
        .iter()
        .filter(|s| s.copy_out && s.len > 0)
        .map(|s| s.len)
        .collect();
    if lens.is_empty() {
        // Nothing goes back this way, so a guest that sent no deep block at all -- every pointer null, as a sizing call makes them -- is fine.
        return true;
    }
    if !segmented {
        // A v0.1 guest copies the deep block straight back to its one pointer, so there can be only one buffer and it goes back raw.
        return lens.len() == 1 && lens[0] <= have;
    }
    protocol::segments::encoded_len(&lens).is_some_and(|n| n <= have)
}

/// The buffers one call's pointers address, held until the ioctl returns.
pub(super) struct Embedded<'a> {
    bufs: Vec<(Slot, Lease<'a>, [u8; 8])>,
}

impl<'a> Embedded<'a> {
    /// Give every pointer in `params` a buffer of this process's.
    pub fn install(
        slots: &[Slot],
        params: &mut [u8],
        segs: Option<Segments<'_>>,
        legacy: Option<(usize, &[u8])>,
        pool: &'a std::cell::RefCell<GuardPool>,
    ) -> Result<Self, u32> {
        let mut bufs = Vec::with_capacity(slots.len());
        for &slot in slots {
            // A pointer RM copies nothing through still has to be an address of ours rather than the guest's: the control may read whether the field is null, and a guest address in it is the thing this whole path exists to prevent.
            let mut buf = GuardPool::lease(pool, slot.len.max(1)).ok_or(NV_ERR_NO_MEMORY)?;
            buf.as_mut_slice().fill(0);

            if slot.copy_in
                && let Some((bytes, _)) = sent(segs, legacy, slot.ptr_offset)
            {
                // `plan` refused any length but this one.
                buf.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
            }

            let saved: [u8; 8] = params[slot.ptr_offset..slot.ptr_offset + 8]
                .try_into()
                .expect("plan checked the block covers every pointer field");
            let host = buf.as_mut_ptr() as u64;
            params[slot.ptr_offset..slot.ptr_offset + 8].copy_from_slice(&host.to_le_bytes());
            bufs.push((slot, buf, saved));
        }
        Ok(Self { bufs })
    }

    /// Put the guest's own pointer values back, before the block is replied with.
    pub fn restore(&self, params: &mut [u8]) {
        for (slot, _, saved) in &self.bufs {
            params[slot.ptr_offset..slot.ptr_offset + 8].copy_from_slice(saved);
        }
    }

    /// What goes back to the guest: one segment per pointer RM wrote.
    pub fn reply(&self) -> Vec<(u32, &[u8])> {
        self.bufs
            .iter()
            .filter(|(s, _, _)| s.copy_out && s.len > 0)
            .map(|(s, b, _)| (s.ptr_offset as u32, &b.as_slice()[..s.len]))
            .collect()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/nvidia/rmctrl.rs"]
mod tests;
