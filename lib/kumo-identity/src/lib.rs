#![no_std]
//j561
//j562
//j563

//! The interim identity layer's policy (PLAN/012): `/etc/passwd`, `/etc/group` and
//! `/etc/shadow` parsing, shared by the image builder that writes the files and by the
//! `identityd` service that reads them, so the two cannot drift.
//!
//! Everything here borrows from the file text and allocates nothing. Identity is userspace
//! policy over capabilities (`DESIGN/012` §0): an account names a subject, and "root" grants
//! nothing by itself.

pub mod protocol;
pub mod verify;

/// argon2's 1 KiB working block, so a caller can size and map the memory a check needs.
pub use argon2::Block;
pub use verify::{blocks_needed, verify_password, Identity, VerifyError};

/// One `/etc/passwd` line: `name:x:uid:gid:gecos:home:shell`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Account<'a> {
    pub name: &'a str,
    pub uid: u32,
    pub gid: u32,
    pub gecos: &'a str,
    pub home: &'a str,
    pub shell: &'a str,
}

impl<'a> Account<'a> {
    /// Parse one line. The password field must be `x`: hashes live only in `/etc/shadow`.
    pub fn parse(line: &'a str) -> Option<Self> {
        let mut fields = line.split(':');
        let name = fields.next().filter(|name| valid_name(name))?;
        if fields.next()? != "x" {
            return None;
        }
        let uid = fields.next()?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        let gecos = fields.next()?;
        let home = fields.next().filter(|home| home.starts_with('/'))?;
        let shell = fields.next().filter(|shell| shell.starts_with('/'))?;
        if fields.next().is_some() {
            return None;
        }
        Some(Self {
            name,
            uid,
            gid,
            gecos,
            home,
            shell,
        })
    }

    /// Whether this account may start a session at all: the conventional "no login" shells
    /// refuse it, whatever the password says.
    pub fn may_log_in(&self) -> bool {
        !(self.shell.ends_with("/nologin") || self.shell.ends_with("/false"))
    }

    /// uid 0 chooses the root prompt and, from M20, a broader capability bundle. It is never a
    /// kernel bypass.
    pub fn is_root(&self) -> bool {
        self.uid == 0
    }
}

/// One `/etc/shadow` line: `name:hash:lastchg:min:max:warn:inactive:expire:reserved`. Only the
/// first two fields mean anything yet; the rest must be present, as on Linux.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shadow<'a> {
    pub name: &'a str,
    pub hash: &'a str,
}

impl<'a> Shadow<'a> {
    pub fn parse(line: &'a str) -> Option<Self> {
        let mut fields = line.split(':');
        let name = fields.next().filter(|name| valid_name(name))?;
        let hash = fields.next()?;
        if fields.count() != 7 {
            return None;
        }
        Some(Self { name, hash })
    }

    /// A locked account cannot log in by password: `!` or `*` in front, or no hash at all. An
    /// empty field means "no password" on Linux; here it means locked, never open.
    pub fn is_locked(&self) -> bool {
        self.hash.is_empty() || self.hash.starts_with('!') || self.hash.starts_with('*')
    }
}

/// One `/etc/group` line: `name:x:gid:member,member`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Group<'a> {
    pub name: &'a str,
    pub gid: u32,
    pub members: &'a str,
}

impl<'a> Group<'a> {
    pub fn parse(line: &'a str) -> Option<Self> {
        let mut fields = line.split(':');
        let name = fields.next().filter(|name| valid_name(name))?;
        if fields.next()? != "x" {
            return None;
        }
        let gid = fields.next()?.parse().ok()?;
        let members = fields.next()?;
        if fields.next().is_some() || !members.split(',').all(|m| m.is_empty() || valid_name(m)) {
            return None;
        }
        Some(Self { name, gid, members })
    }
}

/// The meaningful lines of an identity file: blank lines and `#` comments are skipped.
pub fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// The `/etc/passwd` entry for `name`, if the file has a valid one.
pub fn account<'a>(passwd: &'a str, name: &str) -> Option<Account<'a>> {
    lines(passwd)
        .filter_map(Account::parse)
        .find(|account| account.name == name)
}

/// The `/etc/shadow` entry for `name`, if the file has a valid one.
pub fn shadow<'a>(shadow: &'a str, name: &str) -> Option<Shadow<'a>> {
    lines(shadow)
        .filter_map(Shadow::parse)
        .find(|entry| entry.name == name)
}

/// A user or group name as `useradd` accepts it: a lowercase letter or `_`, then lowercase
/// letters, digits, `_` or `-`, at most 32 bytes. Nothing that could break a field.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && matches!(bytes[0], b'a'..=b'z' | b'_')
        && bytes
            .iter()
            .all(|&b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// Write a shadow line for `name` with `hash`, last changed `days` after 1970-01-01.
pub fn write_shadow_line(
    out: &mut impl core::fmt::Write,
    name: &str,
    hash: &str,
    days: u64,
) -> core::fmt::Result {
    writeln!(out, "{name}:{hash}:{days}:0:99999:7:::")
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::string::String;

    const PASSWD: &str = "# INDRA\n\
        root:x:0:0:root:/root:/bin/kumoza\n\
        crux:x:1000:1000:crux:/home/crux:/bin/kumoza\n\
        \n\
        nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n";

    #[test]
    fn accounts_parse_and_only_shells_may_log_in() {
        let root = account(PASSWD, "root").unwrap();
        assert!(root.is_root() && root.may_log_in());
        assert_eq!((root.home, root.shell), ("/root", "/bin/kumoza"));
        let crux = account(PASSWD, "crux").unwrap();
        assert_eq!((crux.uid, crux.gid, crux.home), (1000, 1000, "/home/crux"));
        assert!(!crux.is_root() && crux.may_log_in());
        let nobody = account(PASSWD, "nobody").unwrap();
        assert!(!nobody.may_log_in());
        assert_eq!(account(PASSWD, "ava"), None);
    }

    #[test]
    fn malformed_passwd_lines_are_refused() {
        for line in [
            "root:$argon2id$v=19$m=1,t=1,p=1$c2FsdA$aGFzaA:0:0:root:/root:/bin/kumoza",
            "root:x:zero:0:root:/root:/bin/kumoza",
            "root:x:0:0:root:root:/bin/kumoza",
            "root:x:0:0:root:/root:/bin/kumoza:extra",
            "Root:x:0:0:root:/root:/bin/kumoza",
            "ro ot:x:0:0:root:/root:/bin/kumoza",
            "root:x:0:0:root:/root",
            "",
        ] {
            assert_eq!(Account::parse(line), None, "{line}");
        }
    }

    #[test]
    fn shadow_locks_bang_star_and_empty_hashes() {
        let text = "root:!:20384:0:99999:7:::\n\
            crux:$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g:20384:0:99999:7:::\n\
            nobody:*:20384:0:99999:7:::\n\
            empty::20384:0:99999:7:::\n";
        assert!(shadow(text, "root").unwrap().is_locked());
        assert!(shadow(text, "nobody").unwrap().is_locked());
        assert!(shadow(text, "empty").unwrap().is_locked());
        let crux = shadow(text, "crux").unwrap();
        assert!(!crux.is_locked());
        assert!(crux.hash.starts_with("$argon2id$v=19$"));
        // Missing trailing fields are refused, so a cut line cannot pass as valid.
        assert_eq!(Shadow::parse("crux:!:20384"), None);
    }

    #[test]
    fn written_shadow_lines_read_back() {
        let mut out = String::new();
        write_shadow_line(&mut out, "crux", "!", 20384).unwrap();
        assert_eq!(out, "crux:!:20384:0:99999:7:::\n");
        assert_eq!(
            Shadow::parse(out.trim_end()),
            Some(Shadow {
                name: "crux",
                hash: "!"
            })
        );
    }

    #[test]
    fn groups_parse_with_valid_members_only() {
        let wheel = Group::parse("wheel:x:10:root,crux").unwrap();
        assert_eq!((wheel.gid, wheel.members), (10, "root,crux"));
        assert_eq!(Group::parse("crux:x:1000:").unwrap().members, "");
        assert_eq!(Group::parse("wheel:x:10:root,Crux"), None);
        assert_eq!(Group::parse("wheel:x:ten:"), None);
    }
}
