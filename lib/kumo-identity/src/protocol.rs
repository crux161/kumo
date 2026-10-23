//j563

//! The `identityd` channel protocol (PLAN/012): one request message, one reply message.
//!
//! ```text
//! verify:  [OP_VERIFY][name_len][name][password...]
//! lookup:  [OP_LOOKUP][name...]
//! reply:   [status] then, for STATUS_OK:
//!          [uid u32 le][gid u32 le][flags][name_len][name][group_len][group]
//!          [home_len][home][shell_len][shell]
//! ```
//!
//! A refused `verify` is `STATUS_DENIED` whatever the reason (no such user, locked, wrong
//! password, a shell that refuses login), so the reply cannot be used to find accounts. A reply
//! never carries a hash.

pub const OP_VERIFY: u8 = 1;
pub const OP_LOOKUP: u8 = 2;

pub const STATUS_OK: u8 = 0;
pub const STATUS_DENIED: u8 = 1;
pub const STATUS_NOT_FOUND: u8 = 2;
pub const STATUS_BAD_REQUEST: u8 = 3;

/// Longest password a request carries.
pub const MAX_PASSWORD: usize = 128;
/// Largest request: op, name length, a 32-byte name, and the longest password.
pub const REQUEST_MAX: usize = 2 + 32 + MAX_PASSWORD;
/// Largest reply the service writes.
pub const REPLY_MAX: usize = 512;

/// `identityd`'s bootstrap: one read-only VMO holding the three files,
/// `[passwd_len u32 le][group_len u32 le][shadow_len u32 le]` and then the files in that order.
/// Sora builds it from the initrd; nothing else is handed the shadow's bytes this way.
pub const BUNDLE_HEADER_LEN: usize = 12;
/// The largest bundle `identityd` maps.
pub const BUNDLE_MAX: usize = 64 * 1024;

/// The bundle header for files of these lengths, `None` when the bundle would not fit.
pub fn bundle_header(
    passwd: usize,
    group: usize,
    shadow: usize,
) -> Option<[u8; BUNDLE_HEADER_LEN]> {
    let total = BUNDLE_HEADER_LEN
        .checked_add(passwd)?
        .checked_add(group)?
        .checked_add(shadow)?;
    if total > BUNDLE_MAX {
        return None;
    }
    let mut header = [0u8; BUNDLE_HEADER_LEN];
    for (slot, len) in header.chunks_exact_mut(4).zip([passwd, group, shadow]) {
        slot.copy_from_slice(&(len as u32).to_le_bytes());
    }
    Some(header)
}

/// The three files a bundle carries, as `[passwd, group, shadow]`.
pub fn bundle_files(bytes: &[u8]) -> Option<[&str; 3]> {
    let header = bytes.get(..BUNDLE_HEADER_LEN)?;
    let mut at = BUNDLE_HEADER_LEN;
    let mut files = [""; 3];
    for (file, len) in files.iter_mut().zip(header.chunks_exact(4)) {
        let len = u32::from_le_bytes(len.try_into().ok()?) as usize;
        let end = at.checked_add(len)?;
        *file = core::str::from_utf8(bytes.get(at..end)?).ok()?;
        at = end;
    }
    Some(files)
}

const FLAG_ROOT: u8 = 1;
const FLAG_MAY_LOG_IN: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request<'a> {
    Verify { name: &'a str, password: &'a [u8] },
    Lookup { name: &'a str },
}

impl<'a> Request<'a> {
    pub fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        match *self {
            Request::Verify { name, password } => {
                if !crate::valid_name(name) || password.len() > MAX_PASSWORD {
                    return None;
                }
                let len = 2 + name.len() + password.len();
                let frame = out.get_mut(..len)?;
                frame[0] = OP_VERIFY;
                frame[1] = name.len() as u8;
                frame[2..2 + name.len()].copy_from_slice(name.as_bytes());
                frame[2 + name.len()..].copy_from_slice(password);
                Some(len)
            }
            Request::Lookup { name } => {
                if !crate::valid_name(name) {
                    return None;
                }
                let frame = out.get_mut(..1 + name.len())?;
                frame[0] = OP_LOOKUP;
                frame[1..].copy_from_slice(name.as_bytes());
                Some(1 + name.len())
            }
        }
    }

    pub fn decode(bytes: &'a [u8]) -> Option<Self> {
        match *bytes.first()? {
            OP_VERIFY => {
                let name_len = *bytes.get(1)? as usize;
                let name = core::str::from_utf8(bytes.get(2..2 + name_len)?).ok()?;
                let password = bytes.get(2 + name_len..)?;
                (crate::valid_name(name) && password.len() <= MAX_PASSWORD)
                    .then_some(Request::Verify { name, password })
            }
            OP_LOOKUP => {
                let name = core::str::from_utf8(&bytes[1..]).ok()?;
                crate::valid_name(name).then_some(Request::Lookup { name })
            }
            _ => None,
        }
    }
}

/// Who an account is, as a session needs to know it. Nothing secret.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Subject<'a> {
    pub name: &'a str,
    pub uid: u32,
    pub gid: u32,
    /// The primary group's name, or empty when `/etc/group` has none for `gid`.
    pub group: &'a str,
    pub home: &'a str,
    pub shell: &'a str,
    pub root: bool,
    pub may_log_in: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reply<'a> {
    Ok(Subject<'a>),
    Denied,
    NotFound,
    BadRequest,
}

impl<'a> Reply<'a> {
    pub fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        let subject = match *self {
            Reply::Ok(subject) => subject,
            Reply::Denied => return status(out, STATUS_DENIED),
            Reply::NotFound => return status(out, STATUS_NOT_FOUND),
            Reply::BadRequest => return status(out, STATUS_BAD_REQUEST),
        };
        let mut writer = Writer { out, len: 0 };
        writer.put(&[STATUS_OK])?;
        writer.put(&subject.uid.to_le_bytes())?;
        writer.put(&subject.gid.to_le_bytes())?;
        let flags = if subject.root { FLAG_ROOT } else { 0 }
            | if subject.may_log_in {
                FLAG_MAY_LOG_IN
            } else {
                0
            };
        writer.put(&[flags])?;
        for field in [subject.name, subject.group, subject.home, subject.shell] {
            writer.put(&[u8::try_from(field.len()).ok()?])?;
            writer.put(field.as_bytes())?;
        }
        Some(writer.len)
    }

    pub fn decode(bytes: &'a [u8]) -> Option<Self> {
        match *bytes.first()? {
            STATUS_DENIED => return Some(Reply::Denied),
            STATUS_NOT_FOUND => return Some(Reply::NotFound),
            STATUS_BAD_REQUEST => return Some(Reply::BadRequest),
            STATUS_OK => {}
            _ => return None,
        }
        let uid = u32::from_le_bytes(bytes.get(1..5)?.try_into().ok()?);
        let gid = u32::from_le_bytes(bytes.get(5..9)?.try_into().ok()?);
        let flags = *bytes.get(9)?;
        let mut at = 10;
        let mut field = || -> Option<&'a str> {
            let len = *bytes.get(at)? as usize;
            let text = core::str::from_utf8(bytes.get(at + 1..at + 1 + len)?).ok()?;
            at += 1 + len;
            Some(text)
        };
        let (name, group, home, shell) = (field()?, field()?, field()?, field()?);
        Some(Reply::Ok(Subject {
            name,
            uid,
            gid,
            group,
            home,
            shell,
            root: flags & FLAG_ROOT != 0,
            may_log_in: flags & FLAG_MAY_LOG_IN != 0,
        }))
    }
}

fn status(out: &mut [u8], status: u8) -> Option<usize> {
    *out.first_mut()? = status;
    Some(1)
}

struct Writer<'b> {
    out: &'b mut [u8],
    len: usize,
}

impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.len.checked_add(bytes.len())?;
        self.out.get_mut(self.len..end)?.copy_from_slice(bytes);
        self.len = end;
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_and_bad_ones_are_refused() {
        let mut buf = [0u8; REQUEST_MAX];
        for request in [
            Request::Verify {
                name: "crux",
                password: b"hunter2",
            },
            Request::Verify {
                name: "root",
                password: b"",
            },
            Request::Lookup { name: "nobody" },
        ] {
            let n = request.encode_into(&mut buf).unwrap();
            assert_eq!(Request::decode(&buf[..n]), Some(request));
        }
        let long = [b'x'; MAX_PASSWORD + 1];
        assert_eq!(
            Request::Verify {
                name: "crux",
                password: &long
            }
            .encode_into(&mut buf),
            None
        );
        assert_eq!(Request::Lookup { name: "Crux" }.encode_into(&mut buf), None);
        for bad in [&b""[..], b"\x09crux", b"\x01\x09crux", b"\x02", b"\x02a:b"] {
            assert_eq!(Request::decode(bad), None);
        }
    }

    #[test]
    fn replies_round_trip() {
        let mut buf = [0u8; REPLY_MAX];
        let subject = Subject {
            name: "crux",
            uid: 1000,
            gid: 1000,
            group: "crux",
            home: "/home/crux",
            shell: "/bin/kumoza",
            root: false,
            may_log_in: true,
        };
        for reply in [
            Reply::Ok(subject),
            Reply::Denied,
            Reply::NotFound,
            Reply::BadRequest,
        ] {
            let n = reply.encode_into(&mut buf).unwrap();
            assert_eq!(Reply::decode(&buf[..n]), Some(reply));
        }
        assert_eq!(Reply::decode(&[9]), None);
        assert_eq!(Reply::decode(&[STATUS_OK, 1, 2]), None);
    }

    #[test]
    fn bundles_carry_three_files_and_refuse_short_or_oversized_ones() {
        let (passwd, group, shadow) = ("root:x:0:0::/root:/bin/kumoza\n", "root:x:0:\n", "");
        let mut bytes = [0u8; 64];
        let header = bundle_header(passwd.len(), group.len(), shadow.len()).unwrap();
        bytes[..BUNDLE_HEADER_LEN].copy_from_slice(&header);
        let body = BUNDLE_HEADER_LEN + passwd.len();
        bytes[BUNDLE_HEADER_LEN..body].copy_from_slice(passwd.as_bytes());
        bytes[body..body + group.len()].copy_from_slice(group.as_bytes());
        let len = body + group.len();
        assert_eq!(bundle_files(&bytes[..len]), Some([passwd, group, shadow]));
        // Trailing page padding is ignored; a truncated file is not.
        assert_eq!(bundle_files(&bytes), Some([passwd, group, shadow]));
        assert_eq!(bundle_files(&bytes[..len - 1]), None);
        assert_eq!(bundle_files(&bytes[..BUNDLE_HEADER_LEN - 1]), None);
        assert_eq!(bundle_header(BUNDLE_MAX, 0, 0), None);
        assert_eq!(bundle_header(usize::MAX, 1, 0), None);
    }
}
