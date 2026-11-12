//! Match the page extent already reserved by the x86 guest window allocator.
use crate::error::{DeviceError, Result};

const WINDOW_PAGE_SIZE: u64 = 4096;

pub fn page_extent(length: u64) -> Result<u64> {
    length
        .checked_add(WINDOW_PAGE_SIZE - 1)
        .map(|value| value & !(WINDOW_PAGE_SIZE - 1))
        .filter(|&value| value != 0)
        .ok_or_else(|| {
            DeviceError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "window length is empty or cannot be page aligned",
            ))
        })
}

/// Both placement and withdrawal must name the same complete page extent.
pub fn transport_extent(length: u64) -> Result<u64> {
    let extent = page_extent(length)?;
    // Diagnostic control only, used with the same frozen binary in A/B runs.
    if std::env::var_os("LEGION_DIAG_RAW_WINDOW_LENGTHS").is_some_and(|value| value == "1") {
        Ok(length)
    } else {
        Ok(extent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_subpage_objects_still_get_complete_kvm_pages() {
        assert_eq!(page_extent(0xc70).expect("RUSD page"), 4096);
        assert_eq!(page_extent(1).expect("smallest mapping"), 4096);
        assert_eq!(page_extent(4096).expect("aligned mapping"), 4096);
        assert_eq!(page_extent(4097).expect("partial last page"), 8192);
    }

    #[test]
    fn empty_and_overflowing_extents_are_rejected() {
        assert!(page_extent(0).is_err());
        assert!(page_extent(u64::MAX).is_err());
        assert!(page_extent(u64::MAX - 4095).is_ok());
        assert!(page_extent(u64::MAX - 4094).is_err());
    }
}
