#![no_std]
//j563

//! `identityd`, the interim identity service (PLAN/012 item 3): the one process that reads
//! `/etc/shadow`, answering `verify` and `lookup` over its channel (`kumo_identity::protocol`).
//!
//! The policy is `kumo_identity::Identity`. This crate is the serve loop and the service's own
//! hygiene: the argon2 memory is mapped once at startup, sized for the costliest hash the
//! shadow holds, and wiped after every check, as is the request that carried the password.

use core::sync::atomic::{compiler_fence, Ordering};

use kumo_identity::protocol::{OP_VERIFY, REPLY_MAX, REQUEST_MAX};
use kumo_identity::{verify_password, Block, Identity};

/// The most argon2 memory the service maps: 64 MiB. A hash that asks for more cannot be
/// checked here, so its account cannot log in.
pub const MAX_BLOCKS: usize = 64 * 1024;

/// A known answer: argon2id (m=8, t=1, p=1) of `kumo`, computed on the host. The service checks
/// it at startup, so an argon2 that miscompiles for the image target, or overruns its stack,
/// fails at boot and not at the first login.
pub const SELF_TEST_PHC: &str =
    "$argon2id$v=19$m=8,t=1,p=1$aWRlbnRpdHlkLXNlbGZ0ZXN0$VcQ4HXd8pnjQIUlYP5nPEzP0qnyRp/QoZvQ/Ty8e5f8";
pub const SELF_TEST_BLOCKS: usize = 8;

pub fn self_test(memory: &mut [Block]) -> bool {
    let passed = verify_password(SELF_TEST_PHC, b"kumo", memory) == Ok(true)
        && verify_password(SELF_TEST_PHC, b"kumu", memory) == Ok(false);
    wipe_memory(memory);
    passed
}

/// The blocks to map: enough for every unlocked hash and the self-test, capped at
/// [`MAX_BLOCKS`].
pub fn blocks_to_map(identity: &Identity<'_>) -> usize {
    identity.blocks_needed().clamp(SELF_TEST_BLOCKS, MAX_BLOCKS)
}

pub trait Transport {
    /// The next request frame, or `None` once the peer has closed.
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
    fn send(&mut self, frame: &[u8]);
}

/// Answer requests until the peer closes. Returns how many were served.
pub fn serve<T: Transport>(
    identity: &Identity<'_>,
    memory: &mut [Block],
    transport: &mut T,
) -> u32 {
    // One byte more than any valid request, so an oversized frame decodes as a bad request
    // instead of being cut down to one that might parse.
    let mut request = [0u8; REQUEST_MAX + 1];
    let mut reply = [0u8; REPLY_MAX];
    let mut served = 0u32;
    while let Some(n) = transport.recv(&mut request) {
        let n = n.min(request.len());
        let verified = request[..n].first() == Some(&OP_VERIFY);
        let len = identity.serve(&request[..n], &mut reply, memory);
        wipe(&mut request);
        if verified {
            wipe_memory(memory);
        }
        transport.send(&reply[..len]);
        served = served.wrapping_add(1);
    }
    served
}

/// Zero a buffer that held a password, in a way the compiler keeps.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a valid, exclusive reference.
        unsafe { core::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

/// Zero the argon2 memory: its blocks are derived from the last password checked.
fn wipe_memory(memory: &mut [Block]) {
    memory.fill(Block::default());
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use argon2::password_hash::{PasswordHasher, SaltString};
    use argon2::{Algorithm, Argon2, Params, Version};
    use kumo_identity::protocol::{Reply, Request, STATUS_BAD_REQUEST, STATUS_DENIED};
    use std::collections::VecDeque;
    use std::string::{String, ToString};
    use std::vec::Vec;

    fn phc(password: &str, m_cost: u32, salt: &[u8]) -> String {
        let params = Params::new(m_cost, 1, 1, Some(32)).unwrap();
        let salt = SaltString::encode_b64(salt).unwrap();
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    struct Script {
        requests: VecDeque<Vec<u8>>,
        replies: Vec<Vec<u8>>,
    }

    impl Transport for Script {
        fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
            let frame = self.requests.pop_front()?;
            let n = frame.len().min(buf.len());
            buf[..n].copy_from_slice(&frame[..n]);
            Some(n)
        }

        fn send(&mut self, frame: &[u8]) {
            self.replies.push(frame.to_vec());
        }
    }

    fn frame(request: Request<'_>) -> Vec<u8> {
        let mut buf = [0u8; REQUEST_MAX];
        let n = request.encode_into(&mut buf).unwrap();
        buf[..n].to_vec()
    }

    #[test]
    fn the_self_test_answer_is_argon2id_of_kumo() {
        assert_eq!(phc("kumo", 8, b"identityd-selftest"), SELF_TEST_PHC);
        let mut memory = std::vec![Block::default(); SELF_TEST_BLOCKS];
        assert!(self_test(&mut memory));
        assert!(!self_test(&mut memory[..SELF_TEST_BLOCKS - 1]));
    }

    #[test]
    fn memory_is_sized_for_the_costliest_unlocked_hash_within_the_cap() {
        let small = phc("a", 16, b"0123456789abcdef");
        let large = phc("b", 32, b"0123456789abcdef");
        let shadow = std::format!(
            "root:{small}:1:0:99999:7:::\ncrux:{large}:1:0:99999:7:::\nnobody:*:1:0:99999:7:::\n"
        );
        let passwd = "root:x:0:0::/root:/bin/kumoza\ncrux:x:1000:1000::/home/crux:/bin/kumoza\n";
        let identity = Identity {
            passwd,
            group: "",
            shadow: &shadow,
        };
        assert_eq!(blocks_to_map(&identity), 32);
        let locked = Identity {
            shadow: "crux:!:1:0:99999:7:::\n",
            ..identity
        };
        assert_eq!(blocks_to_map(&locked), SELF_TEST_BLOCKS);
        let costly =
            "crux:$argon2id$v=19$m=4194304,t=1,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA:1:0:99999:7:::\n";
        assert_eq!(
            blocks_to_map(&Identity {
                shadow: costly,
                ..identity
            }),
            MAX_BLOCKS
        );
    }

    #[test]
    fn the_loop_serves_until_close_and_wipes_after_each_check() {
        let hash = phc("pw", 16, b"0123456789abcdef");
        let shadow = std::format!("crux:{hash}:1:0:99999:7:::\n");
        let identity = Identity {
            passwd: "crux:x:1000:1000:crux:/home/crux:/bin/kumoza\n",
            group: "crux:x:1000:\n",
            shadow: &shadow,
        };
        let mut oversized = frame(Request::Lookup { name: "crux" });
        oversized.resize(REQUEST_MAX + 40, b'a');
        let mut script = Script {
            requests: [
                frame(Request::Verify {
                    name: "crux",
                    password: b"pw",
                }),
                frame(Request::Verify {
                    name: "crux",
                    password: b"px",
                }),
                frame(Request::Lookup { name: "crux" }),
                oversized,
            ]
            .into(),
            replies: Vec::new(),
        };
        let mut memory = std::vec![Block::default(); blocks_to_map(&identity)];
        assert_eq!(serve(&identity, &mut memory, &mut script), 4);

        assert!(matches!(Reply::decode(&script.replies[0]), Some(Reply::Ok(s)) if s.uid == 1000));
        assert_eq!(script.replies[1], [STATUS_DENIED]);
        assert!(
            matches!(Reply::decode(&script.replies[2]), Some(Reply::Ok(s)) if s.home == "/home/crux")
        );
        assert_eq!(script.replies[3], [STATUS_BAD_REQUEST]);
        let zero = Block::default();
        assert!(memory.iter().all(|block| block.as_ref() == zero.as_ref()));
    }
}
