//! NVOS41: an event record returned through a caller-owned pointer.
use super::*;

impl NvidiaBackend {
    pub(super) fn dispatch_event_data(
        &self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        params: &[u8],
        reply: &mut [u8],
    ) -> usize {
        // NVOS41 is 16 bytes; NvUnixEvent is 16 bytes including its tail pad.
        if self.current_data_len != 16 || params.len() != 32 {
            return self.write_error_resp(reply, Status::IoctlFailed, cookie, libc::EINVAL);
        }
        if params[..8] == [0; 8] {
            return self.write_error_resp(reply, Status::IoctlFailed, cookie, libc::EFAULT);
        }
        // Reading an event consumes it.
        if reply.len() < size_of::<MsgHeader>() + size_of::<IoctlResp>() + 32 {
            return self.write_error_resp(reply, Status::BufferTooSmall, cookie, 0);
        }
        self.dispatch_nested(
            cookie, host_fd, request, params, reply, 16, 0, 0, None, None,
        )
    }
}

#[cfg(test)]
#[path = "../../tests/unit/nvidia/legion_event_data.rs"]
mod tests;
