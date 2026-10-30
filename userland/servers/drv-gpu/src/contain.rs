//j710
//j709

//! G8d-a: the contained-close contract. A session that leaves service is closed in exactly
//! one of two states, and the two are never mixed.
//!
//! *Proven quiescence.* A successful soft reset stopped the MCU and every group, so no GPU
//! state can still reference the client's mappings. Only then is the session closed: its
//! tracked metadata is dropped, the backend is told to retire its own claims, and the exact
//! released counts are reported.
//!
//! *Unproven quiescence.* A failed reset proves nothing. Nothing is released — not the
//! session metadata, not a single DMA mapping — and the result is `Poisoned`: the caller
//! parks and never regrants. A later observer can prove the leak; it can never be repaired
//! by guessing that the GPU stopped.
//!
//! This module owns the ordering and the record. It does not reset anything (the caller
//! passes the result), does not close channels (transport state outlives neither path and
//! belongs to the caller), and does not reclaim kernel VMOs (G8d-b). — TIMBERDOODLE

use core::fmt;

use crate::protocol::session::{Backend, Session, SessionRelease};
use crate::{ResetError, ResetPath};

/// Which side of the contract a containment attempt landed on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainmentState {
    /// Not an observable result of [`Containment::run`]: the entry state, named so the
    /// record's vocabulary is complete and a caller cannot mistake "not released" for
    /// "failed reset". — TIMBERDOODLE
    Quiescing,
    /// The reset was confirmed; metadata and driver claims were released.
    Released,
    /// The reset failed; everything was retained and the session may never be served again.
    Poisoned,
}

/// The outcome of one contained close, including the exact counts it released.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Containment {
    pub state: ContainmentState,
    /// The scripted or hardware reset result that decided the path.
    pub reset: Result<ResetPath, ResetError>,
    /// Counts dropped by [`Session::close`]. Always [`SessionRelease::NONE`] while poisoned:
    /// this value is the release proof, so it may never describe a retention.
    pub release: SessionRelease,
}

impl Containment {
    /// Run the close tail after service has stopped accepting requests.
    ///
    /// The caller must already have stopped reading the client channel and finished or
    /// abandoned any in-flight submission. On `Ok` the session is closed, then the backend
    /// retires; on `Err` neither is touched. — TIMBERDOODLE
    #[must_use]
    pub fn run(
        reset: Result<ResetPath, ResetError>,
        session: &mut Session,
        backend: &mut impl Backend,
    ) -> Self {
        match reset {
            Ok(path) => {
                let release = session.close();
                // Close only after the reset, and retire only after close: the ordering is the
                // contract, so neither step can free backing the other still depends on.
                backend.retire();
                Self {
                    state: ContainmentState::Released,
                    reset: Ok(path),
                    release,
                }
            }
            Err(error) => Self {
                state: ContainmentState::Poisoned,
                reset: Err(error),
                release: SessionRelease::NONE,
            },
        }
    }

    /// True when nothing was released and this session may never serve again.
    pub fn retained(&self) -> bool {
        self.state == ContainmentState::Poisoned
    }
}

impl fmt::Display for Containment {
    /// The one structured record of a close. Fields are stable and space-separated so a boot
    /// log assertion can match the whole line. — TIMBERDOODLE
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reset = if self.reset.is_ok() { "ok" } else { "fail" };
        let retained = if self.retained() { "yes" } else { "no" };
        write!(
            f,
            "drv-gpu: containment reset={} buffers={} bindings={} waits={} retained={}",
            reset, self.release.buffers, self.release.bindings, self.release.waits, retained
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::protocol::session::SubmitError;
    use crate::protocol::{
        BindFlags, Binding, Command, DeviceInfo, Request, Response, Status, Submission, PAGE_BYTES,
        VA_START,
    };
    use std::vec::Vec;

    #[derive(Default)]
    struct Fake {
        /// Driver-side claims the backend would have to release. One per created buffer.
        claims: Vec<u32>,
        retire_calls: u32,
        info_calls: u32,
    }

    impl Backend for Fake {
        fn info(&mut self) -> Result<DeviceInfo, Status> {
            self.info_calls += 1;
            Ok(DeviceInfo {
                gpu_id: 0xa867_0005,
                cs_features: 0x7085f,
            })
        }
        fn create_vm(&mut self) -> Result<(), Status> {
            Ok(())
        }
        fn create_buffer(&mut self, id: u32, _bytes: u64) -> Result<(), Status> {
            self.claims.push(id);
            Ok(())
        }
        fn bind(&mut self, _: Binding) -> Result<(), Status> {
            Ok(())
        }
        fn unbind(&mut self, _: &[Binding]) -> Result<(), Status> {
            Ok(())
        }
        fn destroy_buffer(&mut self, _: u32, _: &[Binding]) -> Result<(), Status> {
            Ok(())
        }
        fn create_group(&mut self) -> Result<(), Status> {
            Ok(())
        }
        fn submit(&mut self, _: Submission) -> Result<(), SubmitError> {
            Ok(())
        }
        fn retire(&mut self) {
            self.retire_calls += 1;
            self.claims.clear();
        }
    }

    /// A session plus its backend and a private monotonic request ID, because ID acceptance
    /// is part of the session and the tests must speak the wire's own rule. — TIMBERDOODLE
    struct Harness {
        session: Session,
        backend: Fake,
        id: u32,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                session: Session::default(),
                backend: Fake::default(),
                id: 0,
            }
        }

        fn send(&mut self, command: Command) -> Result<Response, Status> {
            self.id += 1;
            self.session.dispatch(
                Request {
                    id: self.id,
                    command,
                },
                &mut self.backend,
            )
        }

        /// Two live buffers, two bindings and one armed fence: enough of each kind that an
        /// accidental zero or a swapped count is visible in the record. — TIMBERDOODLE
        fn populated() -> Self {
            let mut h = Self::new();
            h.send(Command::VmCreate).unwrap();
            h.send(Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
            h.send(Command::BufferCreate {
                bytes: 2 * PAGE_BYTES,
            })
            .unwrap();
            h.send(Command::VmBind(binding(1, VA_START))).unwrap();
            h.send(Command::VmBind(binding(2, VA_START + PAGE_BYTES)))
                .unwrap();
            h.send(Command::GroupCreate { vm: 1 }).unwrap();
            h.send(Command::Submit(Submission {
                group: 1,
                stream_va: VA_START,
                stream_bytes: 48,
                signal_point: 1,
            }))
            .unwrap();
            h.send(Command::FenceWait { group: 1, point: 1 }).unwrap();
            h
        }
    }

    fn binding(buffer: u32, va: u64) -> Binding {
        Binding {
            vm: 1,
            buffer,
            va,
            offset: 0,
            bytes: PAGE_BYTES,
            flags: BindFlags(BindFlags::READ_ONLY | BindFlags::NO_EXECUTE),
        }
    }

    fn exact_release() -> SessionRelease {
        SessionRelease {
            buffers: 2,
            bindings: 2,
            waits: 1,
            bytes: 3 * PAGE_BYTES,
        }
    }

    #[test]
    fn a_confirmed_reset_closes_the_session_and_releases_driver_claims() {
        let mut h = Harness::populated();
        let containment =
            Containment::run(Ok(ResetPath::Interrupt), &mut h.session, &mut h.backend);
        assert_eq!(containment.state, ContainmentState::Released);
        assert!(!containment.retained());
        assert_eq!(containment.reset, Ok(ResetPath::Interrupt));
        assert_eq!(containment.release, exact_release());
        // Retire ran exactly once and the driver's own claims are gone.
        assert_eq!(h.backend.retire_calls, 1);
        assert!(h.backend.claims.is_empty());
        // The session is terminal: no further request may reach the backend.
        assert_eq!(h.send(Command::GetInfo), Err(Status::DeviceLost));
        assert_eq!(h.backend.info_calls, 0);
        // ... and the record states exactly what was released.
        assert_eq!(
            std::format!("{}", containment),
            "drv-gpu: containment reset=ok buffers=2 bindings=2 waits=1 retained=no"
        );
    }

    #[test]
    fn a_failed_reset_retains_everything_and_poisons_the_session() {
        let mut h = Harness::populated();
        let containment = Containment::run(
            Err(ResetError::Timeout { rawstat: 0 }),
            &mut h.session,
            &mut h.backend,
        );
        assert_eq!(containment.state, ContainmentState::Poisoned);
        assert!(containment.retained());
        assert_eq!(containment.reset, Err(ResetError::Timeout { rawstat: 0 }));
        // The release proof is empty: a poisoned result may never look like a release.
        assert_eq!(containment.release, SessionRelease::NONE);
        assert_eq!(h.backend.retire_calls, 0);
        assert_eq!(h.backend.claims.as_slice(), &[1, 2]);
        // Nothing was closed: the session still serves and still holds every object, which
        // the later exact-count close proves.
        assert_eq!(
            h.send(Command::GetInfo),
            Ok(Response::Info(DeviceInfo {
                gpu_id: 0xa867_0005,
                cs_features: 0x7085f,
            }))
        );
        assert_eq!(h.backend.info_calls, 1);
        assert_eq!(h.session.close(), exact_release());
        // Even after an explicit close the backend was never told to retire: only a proven
        // reset may cross that line, and this path never proved one.
        assert_eq!(h.backend.retire_calls, 0);
        assert_eq!(h.backend.claims.as_slice(), &[1, 2]);
        assert_eq!(
            std::format!("{}", containment),
            "drv-gpu: containment reset=fail buffers=0 bindings=0 waits=0 retained=yes"
        );
    }

    #[test]
    fn an_empty_session_reports_a_real_success_with_zero_counts() {
        let (mut session, mut backend) = (Session::default(), Fake::default());
        let containment = Containment::run(
            Ok(ResetPath::PolledAfterTimeout),
            &mut session,
            &mut backend,
        );
        assert_eq!(containment.state, ContainmentState::Released);
        assert_eq!(containment.release, SessionRelease::NONE);
        assert_eq!(backend.retire_calls, 1);
        assert_eq!(
            std::format!("{}", containment),
            "drv-gpu: containment reset=ok buffers=0 bindings=0 waits=0 retained=no"
        );
    }
}
