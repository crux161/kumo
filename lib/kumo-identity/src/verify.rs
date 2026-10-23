//j563

//! Password checks and the service's answers (PLAN/012 item 3), with no allocation: argon2id
//! runs in memory the caller provides, which `identityd` maps once.

use argon2::password_hash::{Output, PasswordHash};
use argon2::{Algorithm, Argon2, Block, Params, Version};

use crate::protocol::{Reply, Request, Subject, REPLY_MAX};
use crate::{lines, Account, Group, Shadow};

/// Why a stored hash could not be checked. Every one of these refuses the login.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifyError {
    /// Not a PHC string.
    Malformed,
    /// Not argon2id version 19.
    Unsupported,
    /// Needs more memory than the caller provided.
    TooCostly,
    /// argon2 refused its inputs.
    Hash,
}

fn parse(phc: &str) -> Result<(PasswordHash<'_>, Params), VerifyError> {
    let hash = PasswordHash::new(phc).map_err(|_| VerifyError::Malformed)?;
    if hash.algorithm != argon2::ARGON2ID_IDENT || hash.version != Some(Version::V0x13.into()) {
        return Err(VerifyError::Unsupported);
    }
    let params = Params::try_from(&hash).map_err(|_| VerifyError::Malformed)?;
    Ok((hash, params))
}

/// The argon2 blocks (1 KiB each) checking `phc` needs.
pub fn blocks_needed(phc: &str) -> Result<usize, VerifyError> {
    parse(phc).map(|(_, params)| params.block_count())
}

/// Whether `password` matches the argon2id PHC string `phc`. The comparison is constant-time.
pub fn verify_password(
    phc: &str,
    password: &[u8],
    memory: &mut [Block],
) -> Result<bool, VerifyError> {
    let (hash, params) = parse(phc)?;
    let blocks = params.block_count();
    let memory = memory.get_mut(..blocks).ok_or(VerifyError::TooCostly)?;
    let expected = hash.hash.ok_or(VerifyError::Malformed)?;
    let mut salt = [0u8; 64];
    let salt = hash
        .salt
        .ok_or(VerifyError::Malformed)?
        .decode_b64(&mut salt)
        .map_err(|_| VerifyError::Malformed)?;
    let mut out = [0u8; Output::MAX_LENGTH];
    let out = out
        .get_mut(..expected.len())
        .ok_or(VerifyError::Malformed)?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(password, salt, out, memory)
        .map_err(|_| VerifyError::Hash)?;
    let actual = Output::new(out).map_err(|_| VerifyError::Hash)?;
    Ok(actual == expected)
}

/// The three identity files, as the service holds them.
#[derive(Clone, Copy, Debug)]
pub struct Identity<'a> {
    pub passwd: &'a str,
    pub group: &'a str,
    pub shadow: &'a str,
}

impl<'a> Identity<'a> {
    pub fn lookup(&self, name: &str) -> Option<Subject<'a>> {
        let account = crate::account(self.passwd, name)?;
        Some(self.subject(account))
    }

    fn subject(&self, account: Account<'a>) -> Subject<'a> {
        let group = lines(self.group)
            .filter_map(Group::parse)
            .find(|group| group.gid == account.gid)
            .map_or("", |group| group.name);
        Subject {
            name: account.name,
            uid: account.uid,
            gid: account.gid,
            group,
            home: account.home,
            shell: account.shell,
            root: account.is_root(),
            may_log_in: account.may_log_in(),
        }
    }

    /// A login: the account must exist, have a shell that allows login, an unlocked shadow
    /// entry, and a password that matches. Anything else is `None`.
    pub fn verify(&self, name: &str, password: &[u8], memory: &mut [Block]) -> Option<Subject<'a>> {
        let account = crate::account(self.passwd, name).filter(Account::may_log_in)?;
        let entry = crate::shadow(self.shadow, name).filter(|entry| !entry.is_locked())?;
        verify_password(entry.hash, password, memory)
            .ok()
            .filter(|&matched| matched)
            .map(|_| self.subject(account))
    }

    /// The most memory any login here can need: the largest unlocked hash's block count.
    pub fn blocks_needed(&self) -> usize {
        lines(self.shadow)
            .filter_map(Shadow::parse)
            .filter(|entry| !entry.is_locked())
            .filter_map(|entry| blocks_needed(entry.hash).ok())
            .max()
            .unwrap_or(0)
    }

    /// Answer one request frame into `reply`; returns the reply's length.
    pub fn serve(
        &self,
        request: &[u8],
        reply: &mut [u8; REPLY_MAX],
        memory: &mut [Block],
    ) -> usize {
        let answer = match Request::decode(request) {
            Some(Request::Verify { name, password }) => self
                .verify(name, password, memory)
                .map_or(Reply::Denied, Reply::Ok),
            Some(Request::Lookup { name }) => self.lookup(name).map_or(Reply::NotFound, Reply::Ok),
            None => Reply::BadRequest,
        };
        answer
            .encode_into(reply)
            .or_else(|| Reply::BadRequest.encode_into(reply))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::protocol::{STATUS_BAD_REQUEST, STATUS_DENIED};
    use argon2::password_hash::{PasswordHasher, SaltString};
    use std::string::{String, ToString};
    use std::vec::Vec;

    /// A real argon2id PHC string with small parameters, so the tests stay quick.
    fn phc(password: &str, salt: &[u8]) -> String {
        let params = Params::new(64, 1, 1, None).unwrap();
        let salt = SaltString::encode_b64(salt).unwrap();
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    fn memory(blocks: usize) -> Vec<Block> {
        std::vec![Block::default(); blocks]
    }

    #[test]
    fn a_hash_verifies_only_its_own_password() {
        let hash = phc("correct horse", b"0123456789abcdef");
        let mut mem = memory(64);
        assert_eq!(blocks_needed(&hash), Ok(64));
        assert_eq!(verify_password(&hash, b"correct horse", &mut mem), Ok(true));
        assert_eq!(
            verify_password(&hash, b"correct horsf", &mut mem),
            Ok(false)
        );
        assert_eq!(verify_password(&hash, b"", &mut mem), Ok(false));
        assert_eq!(
            verify_password(&hash, b"correct horse", &mut mem[..63]),
            Err(VerifyError::TooCostly)
        );
    }

    #[test]
    fn other_algorithms_and_garbage_are_refused() {
        let mut mem = memory(64);
        let argon2i = phc("x", b"0123456789abcdef").replace("$argon2id$", "$argon2i$");
        assert_eq!(
            verify_password(&argon2i, b"x", &mut mem),
            Err(VerifyError::Unsupported)
        );
        for bad in ["", "!", "*", "$argon2id$", "plaintext"] {
            assert!(verify_password(bad, b"x", &mut mem).is_err(), "{bad}");
        }
    }

    #[test]
    fn logins_need_an_account_a_shell_an_unlocked_hash_and_the_password() {
        let crux_hash = phc("crux-pass", b"saltsaltsaltsalt");
        let root_hash = phc("root-pass", b"SALTSALTSALTSALT");
        let shadow = std::format!(
            "root:{root_hash}:20384:0:99999:7:::\n\
             crux:{crux_hash}:20384:0:99999:7:::\n\
             nobody:{crux_hash}:20384:0:99999:7:::\n\
             ghost:{crux_hash}:20384:0:99999:7:::\n"
        );
        let identity = Identity {
            passwd: "root:x:0:0:root:/root:/bin/kumoza\n\
                     crux:x:1000:1000:crux:/home/crux:/bin/kumoza\n\
                     nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n",
            group: "root:x:0:\ncrux:x:1000:\nnogroup:x:65534:\n",
            shadow: &shadow,
        };
        let mut mem = memory(identity.blocks_needed());
        assert_eq!(identity.blocks_needed(), 64);
        let crux = identity.verify("crux", b"crux-pass", &mut mem).unwrap();
        assert_eq!(
            (crux.uid, crux.group, crux.home),
            (1000, "crux", "/home/crux")
        );
        assert!(!crux.root);
        assert!(
            identity
                .verify("root", b"root-pass", &mut mem)
                .unwrap()
                .root
        );
        // Wrong password, someone else's password, a nologin shell, no passwd entry.
        assert_eq!(identity.verify("crux", b"root-pass", &mut mem), None);
        assert_eq!(identity.verify("root", b"crux-pass", &mut mem), None);
        assert_eq!(identity.verify("nobody", b"crux-pass", &mut mem), None);
        assert_eq!(identity.verify("ghost", b"crux-pass", &mut mem), None);
        // A locked account is refused before argon2 runs.
        let locked = Identity {
            shadow: "crux:!:20384:0:99999:7:::\n",
            ..identity
        };
        assert_eq!(locked.blocks_needed(), 0);
        assert_eq!(locked.verify("crux", b"crux-pass", &mut []), None);
    }

    #[test]
    fn the_service_answers_frames_and_never_says_why_a_login_failed() {
        let hash = phc("pw", b"0123456789abcdef");
        let shadow = std::format!("crux:{hash}:1:0:99999:7:::\n");
        let identity = Identity {
            passwd: "crux:x:1000:1000:crux:/home/crux:/bin/kumoza\n",
            group: "crux:x:1000:\n",
            shadow: &shadow,
        };
        let mut mem = memory(64);
        let mut request = [0u8; crate::protocol::REQUEST_MAX];
        let mut reply = [0u8; REPLY_MAX];
        let ask = |request: Request<'_>, buf: &mut [u8]| request.encode_into(buf).unwrap();

        let n = ask(
            Request::Verify {
                name: "crux",
                password: b"pw",
            },
            &mut request,
        );
        let m = identity.serve(&request[..n], &mut reply, &mut mem);
        let Some(Reply::Ok(subject)) = Reply::decode(&reply[..m]) else {
            panic!("login refused");
        };
        assert_eq!(subject.name, "crux");

        for (name, password) in [("crux", &b"nope"[..]), ("ava", b"pw")] {
            let n = ask(Request::Verify { name, password }, &mut request);
            let m = identity.serve(&request[..n], &mut reply, &mut mem);
            assert_eq!(&reply[..m], &[STATUS_DENIED]);
        }

        let n = ask(Request::Lookup { name: "crux" }, &mut request);
        let m = identity.serve(&request[..n], &mut reply, &mut mem);
        assert!(matches!(Reply::decode(&reply[..m]), Some(Reply::Ok(_))));
        let n = ask(Request::Lookup { name: "ava" }, &mut request);
        let m = identity.serve(&request[..n], &mut reply, &mut mem);
        assert_eq!(Reply::decode(&reply[..m]), Some(Reply::NotFound));

        let m = identity.serve(b"\x07junk", &mut reply, &mut mem);
        assert_eq!(&reply[..m], &[STATUS_BAD_REQUEST]);
        // The reply carries no hash.
        assert!(!reply.windows(9).any(|w| w == b"$argon2id"));
    }
}
