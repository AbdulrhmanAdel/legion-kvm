//! Recognize Wine's shared-page backing names without trusting an arbitrary mapping at the fixed address.
fn split(bytes: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let at = bytes.iter().position(|b| *b == delimiter)?;
    Some((&bytes[..at], &bytes[at + 1..]))
}
fn hex(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(u8::is_ascii_hexdigit)
}

pub(super) fn recognized(path: &[u8], uid: u32) -> bool {
    let path = path.strip_suffix(b" (deleted)").unwrap_or(path);
    if path == b"/memfd:wine-mapping" {
        return true;
    }
    let Some(path) = path.strip_prefix(b"/tmp/.wine-") else {
        return false;
    };
    let Some((owner, path)) = split(path, b'/') else {
        return false;
    };
    if core::str::from_utf8(owner)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        != Some(uid)
    {
        return false;
    }
    let Some(path) = path.strip_prefix(b"server-") else {
        return false;
    };
    let Some((server, name)) = split(path, b'/') else {
        return false;
    };
    let Some((device, inode)) = split(server, b'-') else {
        return false;
    };
    hex(device) && hex(inode) && name.strip_prefix(b"tmpmap-").is_some_and(hex)
}

#[cfg(test)]
#[path = "../tests/unit/kuser_backing.rs"]
mod tests;
