//j562
//j563

//! INDRA's identity files for the initrd, and `cargo xtask passwd` (PLAN/012).
//!
//! `etc/hostname`, `etc/passwd` and `etc/group` are fixed here. `etc/shadow` is built from a
//! file **outside the repository**, `$KUMO_SHADOW` or else `~/.config/kumo/shadow`, as the
//! minisign keys live outside it. Only `cargo xtask passwd` writes that file, and it keeps only
//! argon2id hashes: passwords never enter git, a journal or a log. Without it, every account
//! ships locked. — TIMBERDOODLE

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::Argon2;
use kumo_identity::{account, lines, write_shadow_line, Account, Shadow};

pub const HOSTNAME: &str = "INDRA\n";

/// uids as PLAN/012 suggests: root 0, the first user 1000, `nobody` 65534 with a shell that
/// refuses login.
pub const PASSWD: &str = "root:x:0:0:root:/root:/bin/kumoza\n\
crux:x:1000:1000:crux:/home/crux:/bin/kumoza\n\
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n";

pub const GROUP: &str = "root:x:0:\ncrux:x:1000:\nnogroup:x:65534:\n";

fn shadow_source() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("KUMO_SHADOW") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set; set KUMO_SHADOW")?;
    Ok(PathBuf::from(home).join(".config/kumo/shadow"))
}

fn read_source() -> Result<(PathBuf, String), String> {
    let path = shadow_source()?;
    match fs::read_to_string(&path) {
        Ok(text) => Ok((path, text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok((path, String::new())),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

/// The `etc/shadow` an image ships.
pub fn shadow_file() -> Result<String, String> {
    let (path, source) = read_source()?;
    render_shadow(&source).map_err(|error| format!("{}: {error}", path.display()))
}

/// One line per `/etc/passwd` account, in its order. An account that may log in keeps its line
/// from `source`, verbatim, when there is one; otherwise it is locked (`!`). `nobody` is always
/// `*`. A source line that does not parse, or names someone not in `/etc/passwd`, fails the
/// build rather than shipping something nobody meant.
fn render_shadow(source: &str) -> Result<String, String> {
    for line in lines(source) {
        let entry = Shadow::parse(line).ok_or_else(|| format!("malformed line '{line}'"))?;
        if account(PASSWD, entry.name).is_none() {
            return Err(format!("'{}' is not in etc/passwd", entry.name));
        }
    }
    let mut out = String::new();
    for account in lines(PASSWD).filter_map(Account::parse) {
        let kept = lines(source)
            .find(|line| Shadow::parse(line).is_some_and(|entry| entry.name == account.name));
        match kept {
            Some(line) if account.may_log_in() => {
                out.push_str(line);
                out.push('\n');
            }
            _ => {
                let locked = if account.may_log_in() { "!" } else { "*" };
                out.push_str(&format!("{}:{locked}::0:99999:7:::\n", account.name));
            }
        }
    }
    Ok(out)
}

/// `cargo xtask passwd <user>`: set one account's password from this terminal.
pub fn passwd(args: &[String]) -> Result<(), String> {
    let [user] = args else {
        return Err("usage: cargo xtask passwd <root|crux>".to_owned());
    };
    let account = account(PASSWD, user).ok_or_else(|| format!("passwd: no user '{user}'"))?;
    if !account.may_log_in() {
        return Err(format!(
            "passwd: '{user}' cannot log in, so it keeps no password"
        ));
    }
    let password = read_new_password(user)?;
    let hash = hash_password(&password)?;
    drop(password);
    let (path, source) = read_source()?;
    let updated = replace_line(&source, user, &hash, days_since_epoch())?;
    write_private(&path, &updated)?;
    println!(
        "passwd: {user}'s password is set (argon2id) in {}; rebuild the image to ship it",
        path.display()
    );
    Ok(())
}

fn replace_line(source: &str, user: &str, hash: &str, days: u64) -> Result<String, String> {
    let mut out = String::new();
    let mut replaced = false;
    for line in source.lines() {
        if Shadow::parse(line).is_some_and(|entry| entry.name == user) {
            write_shadow_line(&mut out, user, hash, days).map_err(|e| e.to_string())?;
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        write_shadow_line(&mut out, user, hash, days).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn write_private(path: &PathBuf, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|error| format!("write {}: {error}", path.display()))
}

fn read_new_password(user: &str) -> Result<String, String> {
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|error| format!("passwd needs a terminal: {error}"))?;
    let first = prompt_hidden(&tty, &format!("New KUMO password for {user}: "))?;
    if first.is_empty() {
        return Err("passwd: an empty password is refused; nothing changed".to_owned());
    }
    let second = prompt_hidden(&tty, "Retype it: ")?;
    if first != second {
        return Err("passwd: the two did not match; nothing changed".to_owned());
    }
    Ok(first)
}

/// Prompt on the terminal and read one line with echo off, restoring the terminal afterwards
/// even when the read fails.
fn prompt_hidden(tty: &File, prompt: &str) -> Result<String, String> {
    let mut out = tty;
    out.write_all(prompt.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| error.to_string())?;
    let fd = tty.as_raw_fd();
    // SAFETY: `termios` is plain data that `tcgetattr` fills in completely on success.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
        return Err("passwd: /dev/tty is not a terminal".to_owned());
    }
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    quiet.c_lflag |= libc::ECHONL;
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    let mut line = String::new();
    let read = BufReader::new(tty).read_line(&mut line);
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    read.map_err(|error| error.to_string())?;
    let end = line.trim_end_matches(['\n', '\r']).len();
    line.truncate(end);
    Ok(line)
}

/// argon2id with the crate's defaults (m=19456 KiB, t=2, p=1, OWASP's minimum) and a fresh
/// 16-byte salt from the kernel's generator.
fn hash_password(password: &str) -> Result<String, String> {
    let mut salt = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut salt))
        .map_err(|error| format!("salt: {error}"))?;
    let salt = SaltString::encode_b64(&salt).map_err(|error| error.to_string())?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| error.to_string())
}

fn days_since_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() / 86_400)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::password_hash::{PasswordHash, PasswordVerifier};

    #[test]
    fn the_shipped_files_parse() {
        let accounts: Vec<_> = lines(PASSWD).filter_map(Account::parse).collect();
        assert_eq!(accounts.len(), 3);
        assert!(lines(GROUP).all(|line| kumo_identity::Group::parse(line).is_some()));
        assert_eq!(HOSTNAME, "INDRA\n");
    }

    #[test]
    fn without_a_source_every_account_is_locked() {
        let shadow = render_shadow("").unwrap();
        assert_eq!(
            shadow,
            "root:!::0:99999:7:::\ncrux:!::0:99999:7:::\nnobody:*::0:99999:7:::\n"
        );
        assert!(lines(&shadow).all(|line| Shadow::parse(line).unwrap().is_locked()));
    }

    #[test]
    fn a_set_password_ships_and_nobody_stays_locked() {
        let source =
            "crux:$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g:20384:0:99999:7:::\n\
            nobody:$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g:20384:0:99999:7:::\n";
        let shadow = render_shadow(source).unwrap();
        assert!(shadow.contains("crux:$argon2id$v=19$"));
        assert!(shadow.contains("root:!::"));
        assert!(shadow.contains("nobody:*::"));
    }

    #[test]
    fn a_bad_source_fails_the_build() {
        assert!(render_shadow("crux:!:20384").is_err());
        assert!(render_shadow("ava:!:20384:0:99999:7:::\n").is_err());
    }

    #[test]
    fn replacing_a_line_keeps_the_others() {
        let source = "root:old:1:0:99999:7:::\ncrux:old:1:0:99999:7:::\n";
        let out = replace_line(source, "crux", "new", 2).unwrap();
        assert_eq!(out, "root:old:1:0:99999:7:::\ncrux:new:2:0:99999:7:::\n");
        let added = replace_line("", "root", "new", 2).unwrap();
        assert_eq!(added, "root:new:2:0:99999:7:::\n");
    }

    #[test]
    fn hashes_are_argon2id_and_verify() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        let parsed = PasswordHash::new(&hash).unwrap();
        assert!(Argon2::default()
            .verify_password(b"correct horse", &parsed)
            .is_ok());
        assert!(Argon2::default()
            .verify_password(b"wrong", &parsed)
            .is_err());
        assert_ne!(
            hash,
            hash_password("correct horse").unwrap(),
            "salts differ"
        );
        // identityd's no-alloc verifier accepts what this tool writes, at the shipped cost.
        let blocks = kumo_identity::blocks_needed(&hash).unwrap();
        assert_eq!(blocks, 19456);
        let mut memory = vec![kumo_identity::Block::default(); blocks];
        assert_eq!(
            kumo_identity::verify_password(&hash, b"correct horse", &mut memory),
            Ok(true)
        );
        assert_eq!(
            kumo_identity::verify_password(&hash, b"correct horsf", &mut memory),
            Ok(false)
        );
    }
}
