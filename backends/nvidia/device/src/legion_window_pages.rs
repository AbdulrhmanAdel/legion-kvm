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
#[path = "../tests/unit/legion_window_pages.rs"]
mod tests;
