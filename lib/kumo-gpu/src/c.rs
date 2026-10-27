//j643

//! Borrowed C query boundary for the pinned Mesa scaffold. No FD, handle, allocation or
//! Rust layout crosses it. The caller keeps the same Client across calls. — KESTREL

use core::{ffi::c_void, mem::size_of};

use crate::client::{Client, Error, Transport};

pub const VERSION: u32 = 1;
pub const OK: u32 = 0;
pub const BAD_ARGUMENT: u32 = 1;
pub const INVALID_REQUEST: u32 = 2;
pub const TRANSPORT: u32 = 3;
pub const PROTOCOL: u32 = 4;
pub const DEVICE_LOST: u32 = 5;
pub const SERVER_ERROR: u32 = 16;

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Info {
    pub gpu_id: u32,
    pub cs_features: u32,
    /// Service limits, not the hardware MMU's complete address range.
    pub va_start: u64,
    pub va_end: u64,
    pub max_buffer_bytes: u64,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Api {
    pub version: u32,
    pub bytes: u32,
    pub context: *mut c_void,
    pub query: Option<unsafe extern "C" fn(*mut c_void, *mut Info) -> u32>,
}

/// The C consumer may call query synchronously during `f` only. It must not retain any
/// pointer, reenter the client, race another call or unwind across C. Calling the raw
/// callback requires a live exclusive context and a valid aligned writable Info.
pub fn with_api<T: Transport, R>(client: &mut Client<T>, f: impl FnOnce(&Api) -> R) -> R {
    f(&Api {
        version: VERSION,
        bytes: size_of::<Api>() as u32,
        context: (client as *mut Client<T>).cast(),
        query: Some(query::<T>),
    })
}

unsafe extern "C" fn query<T: Transport>(context: *mut c_void, out: *mut Info) -> u32 {
    if context.is_null() || out.is_null() {
        return BAD_ARGUMENT;
    }
    // The scoped C contract supplies exclusive live pointers; null refuses before I/O.
    // Clear the result even on failure so an old identity cannot look successful. — KESTREL
    unsafe { out.write(Info::default()) };
    let client = unsafe { &mut *context.cast::<Client<T>>() };
    match client.info() {
        Ok(info) => {
            unsafe {
                out.write(Info {
                    gpu_id: info.gpu_id,
                    cs_features: info.cs_features,
                    va_start: crate::VA_START,
                    va_end: crate::VA_END,
                    max_buffer_bytes: crate::MAX_BUFFER_BYTES,
                })
            };
            OK
        }
        Err(Error::InvalidRequest) => INVALID_REQUEST,
        Err(Error::Transport(_)) => TRANSPORT,
        Err(Error::Protocol) => PROTOCOL,
        Err(Error::DeviceLost | Error::RequestIdsExhausted) => DEVICE_LOST,
        Err(Error::Server(status)) => SERVER_ERROR + status as u32,
    }
}

const _: () = assert!(size_of::<Info>() == 32);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::TransportError, DeviceInfo, Reply, Request, Response, Status, REPLY_LEN,
        REQUEST_LEN,
    };

    #[derive(Default)]
    struct Wire {
        sent: u32,
        closed: u32,
        failure: bool,
        grant: bool,
    }
    impl Transport for Wire {
        type Handle = u32;
        fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
            let request = Request::decode(bytes).unwrap();
            assert_eq!(request.id, self.sent + 1);
            self.sent = request.id;
            Ok(())
        }
        fn receive(
            &mut self,
            bytes: &mut [u8; REPLY_LEN + 1],
        ) -> Result<(usize, Option<u32>), TransportError> {
            let result = if self.failure {
                Err(Status::Busy)
            } else {
                Ok(Response::Info(DeviceInfo {
                    gpu_id: 0xa8670005,
                    cs_features: 0x7085f,
                }))
            };
            bytes[..REPLY_LEN].copy_from_slice(
                &Reply {
                    id: self.sent,
                    result,
                }
                .encode(),
            );
            self.failure = false;
            Ok((REPLY_LEN, self.grant.then_some(77)))
        }
        fn close_handle(&mut self, handle: u32) {
            assert_eq!(handle, 77);
            self.closed += 1;
        }
    }

    #[test]
    fn borrowed_queries_keep_ids_and_preserve_recoverable_server_errors() {
        let mut client = Client::new(Wire {
            failure: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            assert_eq!(api.version, VERSION);
            assert_eq!(api.bytes as usize, size_of::<Api>());
            let mut out = Info {
                gpu_id: 99,
                ..Info::default()
            };
            let query = api.query.unwrap();
            assert_eq!(
                unsafe { query(api.context, &mut out) },
                SERVER_ERROR + Status::Busy as u32
            );
            assert_eq!(out.gpu_id, 0);
            for _ in 0..2 {
                assert_eq!(unsafe { query(api.context, &mut out) }, OK);
                assert_eq!(out.gpu_id, 0xa8670005);
                assert_eq!(
                    (out.va_start, out.va_end, out.max_buffer_bytes),
                    (crate::VA_START, crate::VA_END, crate::MAX_BUFFER_BYTES)
                );
            }
        });
        assert!(client.info().is_ok());
    }

    #[test]
    fn rejected_grants_and_loss_keep_the_shared_client_contract() {
        let mut client = Client::new(Wire {
            grant: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            let mut out = Info::default();
            let query = api.query.unwrap();
            assert_eq!(unsafe { query(api.context, &mut out) }, PROTOCOL);
            assert_eq!(unsafe { query(api.context, &mut out) }, DEVICE_LOST);
        });
        assert!(client.is_lost());
        // Client's existing tests prove exact rejected-handle closure and no further I/O;
        // this test proves C observes the same loss rather than a recreated session. — KESTREL
    }

    #[test]
    fn null_arguments_refuse_without_consuming_request_ids() {
        let mut client = Client::new(Wire::default());
        with_api(&mut client, |api| {
            let query = api.query.unwrap();
            let mut out = Info::default();
            assert_eq!(
                unsafe { query(core::ptr::null_mut(), &mut out) },
                BAD_ARGUMENT
            );
            assert_eq!(
                unsafe { query(api.context, core::ptr::null_mut()) },
                BAD_ARGUMENT
            );
            assert_eq!(unsafe { query(api.context, &mut out) }, OK);
        });
    }
}
