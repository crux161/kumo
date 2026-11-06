//j806
//! NV8b normal shutdown (Base2.1 §3.6.1, legacy PCIe semantics). Delete all I/O
//! queues first. Keep EN1/SHN1 and all DMA/process custody until the machine resets:
//! a Controller Reset after SHST2 may undo media power-off readiness. A Ready
//! frame authorizes power transition, NEVER buffer reclamation. — TIMBERDOODLE
use crate::{start, Error, RegisterIo, CC, CC_CONFIG, CSTS};
pub const BUDGET_NS: u64 = 30_000_000_000;
pub const LEN: usize = 32;
const NORMAL: u32 = 1 << 14;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ready {
    pub session: u32,
    pub cc: u32,
    pub csts: u32,
}
impl Ready {
    pub fn valid_for(&self, session: u32) -> bool {
        session != 0
            && self.session == session
            && self.cc == CC_CONFIG | 1 | NORMAL
            && self.csts == 9
    }
    pub fn encode(&self) -> [u8; LEN] {
        let mut b = [0; LEN];
        b[..8].copy_from_slice(b"NV8SHUTD");
        for (at, x) in [
            (8, 1),
            (12, self.session),
            (16, self.cc),
            (20, self.csts),
            (24, 7),
        ] {
            b[at..at + 4].copy_from_slice(&x.to_le_bytes());
        }
        b
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != LEN || &b[..8] != b"NV8SHUTD" {
            return None;
        }
        let word = |at| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        // flags7 = Flush complete, I/O queues deleted, DMA retained. No release bit.
        // Unknown fields/modes fail before the parent forwards power. — TIMBERDOODLE
        if word(8) != 1 || word(24) != 7 || word(28) != 0 {
            return None;
        }
        let r = Self {
            session: word(12),
            cc: word(16),
            csts: word(20),
        };
        r.valid_for(r.session).then_some(r)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    pub cc: u32,
    pub csts: u32,
    pub elapsed_ns: u64,
}
/// Caller has stopped clients, Flushed, deleted I/O SQs then CQs, and holds every
/// buffer. On ANY failure leave EN/SHN untouched and retain all custody. — TIMBERDOODLE
pub fn normal(io: &mut impl RegisterIo) -> Result<Report, Error> {
    let cc = io.read32(CC);
    let state = io.read32(CSTS);
    if cc == u32::MAX || state == u32::MAX || state & 2 != 0 {
        return Err(Error::Fatal);
    }
    if cc != CC_CONFIG | 1 || state != 1 {
        return Err(Error::State);
    }
    let begin = start(io)?;
    let deadline = begin.checked_add(BUDGET_NS).ok_or(Error::Clock)?;
    let wanted = cc | NORMAL;
    io.write32(CC, wanted);
    io.barrier();
    if io.read32(CC) != wanted {
        return Err(Error::Programming);
    }
    let mut delay = 1_000_000;
    for _ in 0..4096 {
        let state = io.read32(CSTS);
        let now = start(io)?;
        if now < begin {
            return Err(Error::Clock);
        }
        if state == u32::MAX || state & 2 != 0 {
            return Err(Error::Fatal);
        }
        if !matches!(state, 1 | 5 | 9) {
            return Err(Error::State);
        }
        if now > deadline {
            return Err(Error::Timeout);
        }
        if state & 12 == 8 {
            return Ok(Report {
                cc: wanted,
                csts: state,
                elapsed_ns: now - begin,
            });
        }
        if now == deadline {
            return Err(Error::Timeout);
        }
        if !io.sleep_ns(delay.min(deadline - now)) {
            return Err(Error::Sleep);
        }
        delay = (delay * 2).min(50_000_000);
    }
    Err(Error::Clock)
}
#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;
    struct Device {
        cc: u32,
        now: u64,
        polls: u8,
        fail: u8,
        writes: Vec<u32>,
    }
    impl RegisterIo for Device {
        fn read32(&mut self, o: u16) -> u32 {
            match o {
                CC => self.cc,
                CSTS => {
                    if self.cc & NORMAL == 0 {
                        1
                    } else {
                        self.polls += u8::from(self.polls < 10);
                        match self.fail {
                            1 => 5,
                            2 => 7,
                            3 => 13,
                            _ => {
                                if self.polls < 3 {
                                    5
                                } else {
                                    9
                                }
                            }
                        }
                    }
                }
                _ => panic!("unexpected register"),
            }
        }
        fn write32(&mut self, o: u16, v: u32) {
            assert_eq!(o, CC);
            self.cc = v;
            self.writes.push(v);
        }
        fn barrier(&mut self) {}
        fn now_ns(&mut self) -> u64 {
            self.now
        }
        fn sleep_ns(&mut self, n: u64) -> bool {
            self.now += n;
            self.fail != 4
        }
    }
    fn device(fail: u8) -> Device {
        Device {
            cc: (6 << 16) | (4 << 20) | 1,
            now: 0,
            polls: 0,
            fail,
            writes: Vec::new(),
        }
    }
    #[test]
    fn normal_shutdown_waits_for_complete_and_never_resets_or_recycles() {
        let mut d = device(0);
        let r = normal(&mut d).unwrap();
        assert_eq!(r.csts, 9);
        assert!(r.elapsed_ns > 0);
        assert_eq!(d.writes, std::vec![((6 << 16) | (4 << 20) | 1 | NORMAL)]);
        assert_eq!(d.cc & 1, 1);
        let ready = Ready {
            session: 7,
            cc: r.cc,
            csts: r.csts,
        };
        assert_eq!(Ready::decode(&ready.encode()), Some(ready));
        assert!(!ready.valid_for(8));
        for (at, v) in [
            (8, 2),
            (12, 0),
            (16, 0),
            (20, 1),
            (20, 5),
            (20, 11),
            (21, 1),
            (18, 0xfe),
            (24, 3),
            (28, 1),
        ] {
            let mut b = ready.encode();
            b[at] = v;
            assert!(Ready::decode(&b).is_none());
        }
        assert!(Ready::decode(&ready.encode()[..31]).is_none());
        let disabled = Ready {
            cc: r.cc & !1,
            ..ready
        };
        assert!(!disabled.valid_for(7));
        let abrupt = Ready {
            cc: (r.cc & !(3 << 14)) | (2 << 14),
            ..ready
        };
        assert!(!abrupt.valid_for(7));
    }
    #[test]
    fn timeout_fatal_reserved_or_sleep_failure_leaves_enable_and_custody_intact() {
        for (fail, error) in [
            (1, Error::Timeout),
            (2, Error::Fatal),
            (3, Error::State),
            (4, Error::Sleep),
        ] {
            let mut d = device(fail);
            assert_eq!(normal(&mut d), Err(error));
            assert_eq!(d.cc & 1, 1);
            assert_eq!(d.cc & (3 << 14), NORMAL);
            assert_eq!(d.writes.len(), 1);
        }
        let mut d = device(0);
        d.cc &= !1;
        assert_eq!(normal(&mut d), Err(Error::State));
        assert!(d.writes.is_empty());
    }
}
