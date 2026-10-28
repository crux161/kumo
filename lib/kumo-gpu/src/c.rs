//j643
//j644
//j645
//j646

//! Borrowed C query boundary for the pinned Mesa scaffold. No FD, handle, allocation or
//! Rust layout crosses it. The caller keeps the same Client across calls. — KESTREL

use core::{ffi::c_void, mem::size_of};

use crate::client::{Client, Error, Transport};

pub const VERSION: u32 = 4;
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
    /// This service's own window, not the hardware MMU's address range. `mmu_features`
    /// carries the attested register word the consumer derives that range from. — SAKER
    pub va_start: u64,
    pub va_end: u64,
    pub max_buffer_bytes: u64,
    pub shader_present: u64,
    /// Raw `MMU_FEATURES`; bits [7:0] are the hardware virtual-address width.
    pub mmu_features: u32,
    /// Raw `TILER_FEATURES`, or zero when the backend reports it unsupported. Zero is the
    /// only field here with an "absent" meaning, so C must not decode it as a shape. — SAKER
    pub tiler_features: u32,
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
    let result: Result<Info, Error> = (|| {
        let info = client.info()?;
        let topology = client.topology()?;
        let mmu = client.mmu()?;
        // Tiler properties are optional. NotSupported means the shape is unknown, not that the
        // device is unusable, so the query still succeeds and reports zero for C to skip; any
        // other failure still aborts and publishes nothing. — SAKER
        let tiler_features = match client.tiler() {
            Ok(tiler) => tiler.tiler_features,
            Err(Error::Server(crate::Status::NotSupported)) => 0,
            Err(error) => return Err(error),
        };
        Ok(Info {
            gpu_id: info.gpu_id,
            cs_features: info.cs_features,
            va_start: crate::VA_START,
            va_end: crate::VA_END,
            max_buffer_bytes: crate::MAX_BUFFER_BYTES,
            shader_present: topology.shader_present,
            mmu_features: mmu.mmu_features,
            tiler_features,
        })
    })();
    match result {
        Ok(info) => {
            unsafe { out.write(info) };
            OK
        }
        Err(Error::InvalidRequest) => INVALID_REQUEST,
        Err(Error::Transport(_)) => TRANSPORT,
        Err(Error::Protocol) => PROTOCOL,
        Err(Error::DeviceLost | Error::RequestIdsExhausted) => DEVICE_LOST,
        Err(Error::Server(status)) => SERVER_ERROR + status as u32,
    }
}

const _: () = assert!(size_of::<Info>() == 48);
// Pin both tail offsets. `tiler_features` exactly fills the padding that followed
// `mmu_features`, so version4's Info is the same *size* as version3's and a size check alone
// cannot distinguish them — only the version field and these offsets can. — SAKER
const _: () = assert!(core::mem::offset_of!(Info, mmu_features) == 40);
const _: () = assert!(core::mem::offset_of!(Info, tiler_features) == 44);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::TransportError, Command, DeviceInfo, DeviceMmu, DeviceTiler, DeviceTopology, Reply,
        Request, Response, Status, REPLY_LEN, REQUEST_LEN,
    };

    #[derive(Default)]
    struct Wire {
        sent: u32,
        command: Option<Command>,
        closed: u32,
        failure: bool,
        mmu_failure: bool,
        tiler_unsupported: bool,
        grant: bool,
    }
    impl Transport for Wire {
        type Handle = u32;
        fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
            let request = Request::decode(bytes).unwrap();
            assert_eq!(request.id, self.sent + 1);
            self.sent = request.id;
            self.command = Some(request.command);
            Ok(())
        }
        fn receive(
            &mut self,
            bytes: &mut [u8; REPLY_LEN + 1],
        ) -> Result<(usize, Option<u32>), TransportError> {
            let result = if self.failure {
                Err(Status::Busy)
            } else if self.mmu_failure && self.command == Some(Command::GetMmuFeatures) {
                Err(Status::Busy)
            } else if self.command == Some(Command::GetTopology) {
                Ok(Response::Topology(DeviceTopology {
                    shader_present: 0x500000005,
                }))
            } else if self.command == Some(Command::GetMmuFeatures) {
                Ok(Response::Mmu(DeviceMmu {
                    mmu_features: 0x2830,
                }))
            } else if self.command == Some(Command::GetTilerFeatures) {
                if self.tiler_unsupported {
                    Err(Status::NotSupported)
                } else {
                    Ok(Response::Tiler(DeviceTiler {
                        tiler_features: 0x809,
                    }))
                }
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
                assert_eq!(out.shader_present, 0x500000005);
                assert_eq!(out.mmu_features, 0x2830);
                assert_eq!(out.tiler_features, 0x809);
            }
        });
        assert!(client.info().is_ok());
    }

    #[test]
    fn an_unsupported_tiler_query_degrades_to_zero_without_failing_the_report() {
        let mut client = Client::new(Wire {
            tiler_unsupported: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            let mut out = Info {
                tiler_features: 99,
                ..Info::default()
            };
            let query = api.query.unwrap();
            // NotSupported is the one status that means "this optional property is absent",
            // so the rest of the report is still published and the client stays live. — SAKER
            assert_eq!(unsafe { query(api.context, &mut out) }, OK);
            assert_eq!(out.tiler_features, 0);
            assert_eq!(out.gpu_id, 0xa8670005);
            assert_eq!(out.mmu_features, 0x2830);
            assert_eq!(out.shader_present, 0x500000005);
        });
        assert!(!client.is_lost());
        assert!(client.info().is_ok());
    }

    #[test]
    fn a_failing_final_query_publishes_no_partially_filled_report() {
        let mut client = Client::new(Wire {
            mmu_failure: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            let mut out = Info {
                gpu_id: 99,
                shader_present: 99,
                mmu_features: 99,
                ..Info::default()
            };
            let query = api.query.unwrap();
            // GetInfo and GetTopology both succeed, so a naive implementation would already
            // have written identity and topology. Nothing may be published. — SAKER
            assert_eq!(
                unsafe { query(api.context, &mut out) },
                SERVER_ERROR + Status::Busy as u32
            );
            assert_eq!(
                (out.gpu_id, out.shader_present, out.mmu_features),
                (0, 0, 0)
            );
            assert_eq!((out.va_start, out.va_end), (0, 0));
        });
        // Busy is recoverable: the client stays live and the next full query succeeds.
        assert!(!client.is_lost());
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
