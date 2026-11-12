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
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct EventHost {
        calls: Arc<AtomicUsize>,
        status: u32,
    }

    impl HostDriver for EventHost {
        fn ioctl(&self, _: RawFd, _: u64, outer: &mut [u8]) -> std::result::Result<(), i32> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(outer.len(), 16);
            let pointer = u64::from_le_bytes(outer[..8].try_into().expect("event pointer"));
            assert_ne!(pointer, 0x7fff_0000_1000);
            assert_ne!(pointer, 0);
            if self.status == 0 {
                // SAFETY: dispatch_nested substitutes its own live, guarded 16-byte allocation for this synchronous mock ioctl call.
                unsafe { std::slice::from_raw_parts_mut(pointer as *mut u8, 16) }
                    .copy_from_slice(&[3, 0, 0, 0, 7, 0, 0, 0, 11, 0, 0, 0, 13, 0, 0, 0]);
                outer[8..12].copy_from_slice(&1_u32.to_le_bytes());
            }
            outer[12..16].copy_from_slice(&self.status.to_le_bytes());
            Ok(())
        }
    }

    #[test]
    fn event_pointer_is_translated_and_complete_native_reply_preserved() {
        for status in [0_u32, 0x59] {
            let mut backend = NvidiaBackend::for_test();
            backend.current_data_len = 16;
            let calls = Arc::new(AtomicUsize::new(0));
            backend.set_host(Box::new(EventHost {
                calls: calls.clone(),
                status,
            }));
            let mut params = [0_u8; 32];
            params[..8].copy_from_slice(&0x7fff_0000_1000_u64.to_le_bytes());
            let mut reply = [0_u8; 96];
            let written = backend.dispatch_event_data(0, -1, 0, &params, &mut reply);
            let at = size_of::<MsgHeader>() + size_of::<IoctlResp>();
            assert_eq!(written, at + 32);
            assert_eq!(&reply[at..at + 8], &params[..8]);
            assert_eq!(&reply[at + 12..at + 16], &status.to_le_bytes());
            if status == 0 {
                assert_eq!(&reply[at + 8..at + 12], &1_u32.to_le_bytes());
                assert_eq!(
                    &reply[at + 16..at + 32],
                    &[3, 0, 0, 0, 7, 0, 0, 0, 11, 0, 0, 0, 13, 0, 0, 0]
                );
            } else {
                assert_eq!(&reply[at + 16..at + 32], &[0; 16]);
            }
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn invalid_event_shapes_and_short_replies_do_not_consume_host_events() {
        let mut backend = NvidiaBackend::for_test();
        backend.current_data_len = 16;
        let calls = Arc::new(AtomicUsize::new(0));
        backend.set_host(Box::new(EventHost {
            calls: calls.clone(),
            status: 0,
        }));
        let mut params = [0_u8; 33];
        params[..8].copy_from_slice(&0x7fff_0000_1000_u64.to_le_bytes());
        for size in [0, 16, 31, 33] {
            backend.dispatch_event_data(0, -1, 0, &params[..size], &mut [0; 96]);
        }
        backend.dispatch_event_data(0, -1, 0, &params[..32], &mut [0; 16]);
        backend.dispatch_event_data(0, -1, 0, &[0; 32], &mut [0; 96]);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}
