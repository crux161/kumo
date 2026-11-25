//j929
//J944_r10_001
//! A client session that survives its server's restart (SH8-c, DESIGN/002 §2, DESIGN/010 §3).
//!
//! A filesystem server killed mid-request takes its sessions with it, but not the mount's
//! control endpoint: Sora holds a duplicate of the server end, so the client's `fs_mount`
//! stays open and a restarted server inherits it. [`Reconnecting`] wraps one session:
//! 1. A request whose session has closed mints a new session over the control endpoint
//!    ([`SessionLink::reconnect`]).
//! 2. It reopens every handle the caller holds, by the path and options it was opened with
//!    (`OPEN_CREATE` dropped: a reopen must never resurrect a deleted file).
//! 3. It retries the request once per new session, a bounded number of times.
//!
//! Callers see stable handles: the ones this wrapper issued, translated to whatever the
//! current server issued.
//!
//! **Idempotency.** Reads, stats, listings, opens and offset-carrying writes are naturally
//! idempotent. Four answers are ambiguous on a *retry*, where the first attempt may have
//! committed before the server died, and count as success: `Mkdir` → `EXISTS`, `Unlink` or
//! `Rmdir` → `NOT_FOUND`, and `Rename` → `NOT_FOUND` with the target present (D4, the NFS
//! client rule).
//!
//! **What does not come back.** A handle whose file was unlinked (or renamed by someone else)
//! before the crash cannot be reopened. It answers [`VFS_IO_ERROR`] from then on, like an NFS
//! stale handle. An advisory lock is not silently re-taken: the handle's next `Lock` answers
//! [`VFS_IO_ERROR`] once (Linux loses flocks across an NFS server reboot too).

use crate::{
    encode_open_ok, encode_status, parse_open_reply, parse_open_reply_id, Request, MAX_PATH,
    OPEN_CREATE, VFS_BAD_HANDLE, VFS_EXISTS, VFS_IO_ERROR, VFS_NOT_FOUND, VFS_OK,
};

/// How a session exchange failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// The server end is gone (the server died, or this session was refused).
    Closed,
    /// Anything else: the request was not sent or the reply was malformed.
    Failed,
}

/// The transport under [`Reconnecting`]: one live session plus the means to mint another.
pub trait SessionLink {
    /// Send `request` on the current session (carrying a *copy* of `vmo` when it is not 0, so a
    /// retry can carry it again) and read the reply into `reply`.
    fn exchange(&mut self, request: &[u8], vmo: u32, reply: &mut [u8]) -> Result<usize, LinkError>;

    /// Replace the current session with a new one over the mount's control endpoint. False
    /// when no new session can be had (no control endpoint, or it closed too).
    fn reconnect(&mut self) -> bool;
}

/// New sessions one request may use before it gives up with [`VFS_IO_ERROR`].
pub const MAX_RECONNECTS: u32 = 3;

#[derive(Clone, Copy)]
struct Entry {
    /// The current server's handle; 0 when the file could not be reopened (stale).
    server: u32,
    options: u8,
    path: [u8; MAX_PATH],
    path_len: u16,
    locked: bool,
    lock_lost: bool,
}

/// A session that reconnects, holding up to `N` open handles.
pub struct Reconnecting<L, const N: usize> {
    pub link: L,
    entries: [Option<Entry>; N],
    /// Sessions this client has had to mint (diagnostics and proofs).
    pub reconnects: u32,
}

impl<L: SessionLink, const N: usize> Reconnecting<L, N> {
    pub const fn new(link: L) -> Self {
        Self {
            link,
            entries: [None; N],
            reconnects: 0,
        }
    }

    fn entry(&self, handle: u32) -> Option<&Entry> {
        self.entries
            .get(usize::try_from(handle).ok()?.checked_sub(1)?)?
            .as_ref()
    }

    fn entry_mut(&mut self, handle: u32) -> Option<&mut Entry> {
        self.entries
            .get_mut(usize::try_from(handle).ok()?.checked_sub(1)?)?
            .as_mut()
    }

    /// One request, as the server would answer it, with this wrapper's stable handles.
    /// `vmo` is the handle the request carries (0 for none); the caller keeps it.
    pub fn call(&mut self, request: &[u8], vmo: u32, reply: &mut [u8]) -> usize {
        let Some(decoded) = Request::decode(request) else {
            return status(reply, crate::VFS_BAD_REQUEST);
        };
        // Answers that need no server, or that a lost session decides.
        if let Some(handle) = handle_of(&decoded) {
            let Some(entry) = self.entry(handle) else {
                return status(reply, VFS_BAD_HANDLE);
            };
            if entry.server == 0 {
                // A stale handle has nothing on the server to close.
                if let Request::Close { .. } = decoded {
                    if let Some(slot) = self.slot(handle) {
                        self.entries[slot] = None;
                    }
                    return status(reply, VFS_OK);
                }
                return status(reply, VFS_IO_ERROR);
            }
            if let Request::Lock { .. } = decoded {
                if entry.lock_lost {
                    if let Some(entry) = self.entry_mut(handle) {
                        entry.lock_lost = false;
                    }
                    return status(reply, VFS_IO_ERROR);
                }
            }
        }
        let mut retry = false;
        let mut attempts = 0;
        loop {
            // A reopen that failed leaves the handle stale; never send its 0.
            if handle_of(&decoded)
                .and_then(|handle| self.entry(handle))
                .is_some_and(|entry| entry.server == 0)
            {
                return status(reply, VFS_IO_ERROR);
            }
            let mut frame = [0u8; 1024];
            let Some(n) = self.translate(&decoded, &mut frame) else {
                return status(reply, VFS_IO_ERROR);
            };
            match self.link.exchange(&frame[..n], vmo, reply) {
                Ok(len) => return self.answered(&decoded, retry, reply, len),
                Err(LinkError::Failed) => return status(reply, VFS_IO_ERROR),
                Err(LinkError::Closed) => {}
            }
            // The session is gone. A close needs no server: the new one never saw the handle.
            if let Request::Close { handle } = decoded {
                if let Some(slot) = self.slot(handle) {
                    self.entries[slot] = None;
                }
                self.recover();
                return status(reply, VFS_OK);
            }
            if attempts == MAX_RECONNECTS || !self.recover() {
                return status(reply, VFS_IO_ERROR);
            }
            attempts += 1;
            retry = true;
        }
    }

    fn slot(&self, handle: u32) -> Option<usize> {
        let slot = usize::try_from(handle).ok()?.checked_sub(1)?;
        self.entries.get(slot)?.as_ref().map(|_| slot)
    }

    /// Mint a new session and reopen every handle on it. False when no session can be had.
    fn recover(&mut self) -> bool {
        loop {
            if !self.link.reconnect() {
                return false;
            }
            self.reconnects += 1;
            if self.reopen_all() {
                return true;
            }
            if self.reconnects > MAX_RECONNECTS * 4 {
                return false;
            }
        }
    }

    /// Reopen each handle by path. False when the new session itself closed meanwhile.
    fn reopen_all(&mut self) -> bool {
        for slot in 0..N {
            let Some(entry) = self.entries[slot] else {
                continue;
            };
            if entry.server == 0 {
                continue;
            }
            let path = &entry.path[..entry.path_len as usize];
            let mut frame = [0u8; 1024];
            let Some(n) = (Request::OpenOptions {
                path,
                options: entry.options & !OPEN_CREATE,
            })
            .encode_into(&mut frame) else {
                continue;
            };
            let mut reply = [0u8; 64];
            let server = match self.link.exchange(&frame[..n], 0, &mut reply) {
                Ok(len) => parse_open_reply(&reply[..len]).map_or(0, |(handle, _)| handle),
                Err(LinkError::Closed) => return false,
                Err(LinkError::Failed) => 0,
            };
            if let Some(entry) = self.entries[slot].as_mut() {
                entry.server = server;
                entry.lock_lost |= entry.locked;
                entry.locked = false;
            }
        }
        true
    }

    /// Re-encode `request` with the current server's handle.
    fn translate(&self, request: &Request<'_>, frame: &mut [u8]) -> Option<usize> {
        let mut request = *request;
        if let Some(handle) = handle_of(&request) {
            let server = self.entry(handle)?.server;
            set_handle(&mut request, server);
        }
        request.encode_into(frame)
    }

    /// Bookkeeping for a reply, and the ambiguous-retry rules (D4).
    fn answered(
        &mut self,
        request: &Request<'_>,
        retry: bool,
        reply: &mut [u8],
        len: usize,
    ) -> usize {
        let code = reply.first().copied().unwrap_or(VFS_IO_ERROR);
        match *request {
            Request::Open { path } => self.opened(path, crate::OPEN_READ, reply, len),
            Request::OpenOptions { path, options } => self.opened(path, options, reply, len),
            Request::Close { handle } if code == VFS_OK => {
                if let Some(slot) = self.slot(handle) {
                    self.entries[slot] = None;
                }
                len
            }
            Request::Lock { handle, operation } if code == VFS_OK => {
                if let Some(entry) = self.entry_mut(handle) {
                    entry.locked = operation != crate::LOCK_UN;
                }
                len
            }
            Request::Rename { from, to } => {
                let ok = code == VFS_OK || (retry && code == VFS_NOT_FOUND && self.exists(to));
                if ok {
                    self.renamed(from, to);
                    status(reply, VFS_OK)
                } else {
                    len
                }
            }
            Request::Mkdir { .. } if retry && code == VFS_EXISTS => status(reply, VFS_OK),
            Request::Unlink { .. } | Request::Rmdir { .. } if retry && code == VFS_NOT_FOUND => {
                status(reply, VFS_OK)
            }
            _ => len,
        }
    }

    fn opened(&mut self, path: &[u8], options: u8, reply: &mut [u8], len: usize) -> usize {
        let Ok((server, size, id)) = parse_open_reply_id(&reply[..len]) else {
            return len;
        };
        let Some(slot) = self.entries.iter().position(Option::is_none) else {
            // No room to remember it: close it on the server and refuse, as a full table does.
            let mut frame = [0u8; 16];
            if let Some(n) = (Request::Close { handle: server }).encode_into(&mut frame) {
                let mut scratch = [0u8; 8];
                let _ = self.link.exchange(&frame[..n], 0, &mut scratch);
            }
            return status(reply, VFS_BAD_HANDLE);
        };
        let mut stored = [0u8; MAX_PATH];
        stored[..path.len()].copy_from_slice(path);
        self.entries[slot] = Some(Entry {
            server,
            options,
            path: stored,
            path_len: path.len() as u16,
            locked: false,
            lock_lost: false,
        });
        // The client sees its stable slot as the handle, but the file's id is the server's:
        // it names the file, not the session (J944_r10_001).
        let frame = encode_open_ok(slot as u32 + 1, size, id);
        let n = frame.len().min(reply.len());
        reply[..n].copy_from_slice(&frame[..n]);
        n
    }

    /// Whether `path` names something (a retry's evidence that a rename landed).
    fn exists(&mut self, path: &[u8]) -> bool {
        let mut frame = [0u8; 512];
        let Some(n) = (Request::Stat { path }).encode_into(&mut frame) else {
            return false;
        };
        let mut reply = [0u8; 32];
        matches!(self.link.exchange(&frame[..n], 0, &mut reply), Ok(len) if len > 0 && reply[0] == VFS_OK)
    }

    /// Keep remembered paths true across this client's own renames: `from` and everything
    /// under it now live under `to`.
    fn renamed(&mut self, from: &[u8], to: &[u8]) {
        for entry in self.entries.iter_mut().flatten() {
            let path = &entry.path[..entry.path_len as usize];
            let tail = if path == from {
                Some(&[][..])
            } else {
                path.strip_prefix(from)
                    .filter(|rest| rest.first() == Some(&b'/'))
            };
            let Some(tail) = tail else { continue };
            let len = to.len() + tail.len();
            if len > MAX_PATH {
                entry.server = 0; // it can no longer be named; treat as stale
                continue;
            }
            let mut moved = [0u8; MAX_PATH];
            moved[..to.len()].copy_from_slice(to);
            moved[to.len()..len].copy_from_slice(tail);
            entry.path = moved;
            entry.path_len = len as u16;
        }
    }
}

fn handle_of(request: &Request<'_>) -> Option<u32> {
    match *request {
        Request::Read { handle, .. }
        | Request::ReadVmo { handle, .. }
        | Request::Write { handle, .. }
        | Request::WriteVmo { handle, .. }
        | Request::SetLen { handle, .. }
        | Request::Flush { handle }
        | Request::Lock { handle, .. }
        | Request::Close { handle } => Some(handle),
        _ => None,
    }
}

fn set_handle(request: &mut Request<'_>, server: u32) {
    match request {
        Request::Read { handle, .. }
        | Request::ReadVmo { handle, .. }
        | Request::Write { handle, .. }
        | Request::WriteVmo { handle, .. }
        | Request::SetLen { handle, .. }
        | Request::Flush { handle }
        | Request::Lock { handle, .. }
        | Request::Close { handle } => *handle = server,
        _ => {}
    }
}

fn status(reply: &mut [u8], code: u8) -> usize {
    match reply.first_mut() {
        Some(first) => {
            *first = encode_status(code)[0];
            1
        }
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_write_reply, OPEN_READ, OPEN_WRITE};

    /// A fake server: one file table that survives "restarts", handles that do not.
    struct Fake {
        epoch: u32,
        files: [(&'static [u8], bool); 4],
        dirs: [(&'static [u8], bool); 2],
        die_before: Option<u32>,
        die_after: Option<u32>,
        calls: u32,
        open: [Option<(u32, usize)>; 8],
        log: [u8; 64],
        log_len: usize,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                epoch: 1,
                files: [
                    (b"/a", true),
                    (b"/b", false),
                    (b"/c", false),
                    (b"/gone", true),
                ],
                dirs: [(b"/d", false), (b"/x", false)],
                die_before: None,
                die_after: None,
                calls: 0,
                open: [None; 8],
                log: [0; 64],
                log_len: 0,
            }
        }
        fn file(&mut self, path: &[u8]) -> Option<usize> {
            self.files.iter().position(|(p, live)| *p == path && *live)
        }
    }

    impl SessionLink for Fake {
        fn exchange(
            &mut self,
            request: &[u8],
            _vmo: u32,
            reply: &mut [u8],
        ) -> Result<usize, LinkError> {
            self.calls += 1;
            if self.die_before == Some(self.calls) {
                return Err(LinkError::Closed);
            }
            let request = Request::decode(request).ok_or(LinkError::Failed)?;
            self.log[self.log_len] = match request {
                Request::OpenOptions { .. } => b'o',
                Request::Write { .. } => b'w',
                Request::Close { .. } => b'c',
                Request::Mkdir { .. } => b'm',
                Request::Unlink { .. } => b'u',
                Request::Rename { .. } => b'r',
                Request::Stat { .. } => b's',
                _ => b'?',
            };
            self.log_len += 1;
            let len = match request {
                Request::OpenOptions { path, .. } => match self.file(path) {
                    Some(file) => {
                        let slot = self.open.iter().position(Option::is_none).unwrap();
                        let server = self.epoch * 100 + slot as u32;
                        self.open[slot] = Some((server, file));
                        let frame = encode_open_ok(server, 7, 0);
                        reply[..frame.len()].copy_from_slice(&frame);
                        frame.len()
                    }
                    None => status(reply, VFS_NOT_FOUND),
                },
                Request::Write { handle, data, .. } => {
                    if self.open.iter().flatten().any(|(h, _)| *h == handle) {
                        let frame = crate::encode_write_ok(data.len() as u32);
                        reply[..frame.len()].copy_from_slice(&frame);
                        frame.len()
                    } else {
                        status(reply, VFS_BAD_HANDLE)
                    }
                }
                Request::Close { handle } => {
                    let slot = self
                        .open
                        .iter()
                        .position(|o| o.is_some_and(|(h, _)| h == handle));
                    match slot {
                        Some(slot) => {
                            self.open[slot] = None;
                            status(reply, VFS_OK)
                        }
                        None => status(reply, VFS_BAD_HANDLE),
                    }
                }
                Request::Mkdir { path } => match self.dirs.iter_mut().find(|(p, _)| *p == path) {
                    Some((_, live)) if *live => status(reply, VFS_EXISTS),
                    Some((_, live)) => {
                        *live = true;
                        status(reply, VFS_OK)
                    }
                    None => status(reply, VFS_NOT_FOUND),
                },
                Request::Unlink { path } => match self.file(path) {
                    Some(file) => {
                        self.files[file].1 = false;
                        status(reply, VFS_OK)
                    }
                    None => status(reply, VFS_NOT_FOUND),
                },
                Request::Rename { from, to } => match self.file(from) {
                    Some(file) => {
                        self.files[file].1 = false;
                        let target = self.files.iter().position(|(p, _)| *p == to).unwrap();
                        self.files[target].1 = true;
                        status(reply, VFS_OK)
                    }
                    None => status(reply, VFS_NOT_FOUND),
                },
                Request::Stat { path } => {
                    if self.file(path).is_some() {
                        status(reply, VFS_OK)
                    } else {
                        status(reply, VFS_NOT_FOUND)
                    }
                }
                _ => status(reply, crate::VFS_NOT_SUPPORTED),
            };
            // The server committed, then died before the reply reached the client.
            if self.die_after == Some(self.calls) {
                return Err(LinkError::Closed);
            }
            Ok(len)
        }

        fn reconnect(&mut self) -> bool {
            self.epoch += 1;
            self.open = [None; 8];
            true
        }
    }

    fn call(c: &mut Reconnecting<Fake, 4>, request: Request<'_>) -> ([u8; 32], usize) {
        let mut frame = [0u8; 600];
        let n = request.encode_into(&mut frame).unwrap();
        let mut reply = [0u8; 32];
        let len = c.call(&frame[..n], 0, &mut reply);
        (reply, len)
    }

    #[test]
    fn handles_stay_stable_across_a_restart_and_the_write_is_retried() {
        let mut c = Reconnecting::<Fake, 4>::new(Fake::new());
        let (reply, len) = call(
            &mut c,
            Request::OpenOptions {
                path: b"/a",
                options: OPEN_READ | OPEN_WRITE | OPEN_CREATE,
            },
        );
        let (handle, _) = parse_open_reply(&reply[..len]).unwrap();
        assert_eq!(handle, 1, "the wrapper's own handle, not the server's 100");
        c.link.die_before = Some(c.link.calls + 1);
        let (reply, len) = call(
            &mut c,
            Request::Write {
                handle,
                offset: 0,
                data: b"abc",
            },
        );
        assert_eq!(parse_write_reply(&reply[..len]), Ok(3));
        assert_eq!(c.reconnects, 1);
        // Reopened without CREATE, on the new server's handle.
        assert_eq!(c.entry(1).unwrap().server, 200);
        assert_eq!(call(&mut c, Request::Close { handle }).0[0], VFS_OK);
        assert_eq!(&c.link.log[..c.link.log_len], b"oowc");
    }

    #[test]
    fn ambiguous_retries_count_as_success() {
        let mut c = Reconnecting::<Fake, 4>::new(Fake::new());
        c.link.die_after = Some(1);
        assert_eq!(call(&mut c, Request::Mkdir { path: b"/d" }).0[0], VFS_OK);
        c.link.die_after = Some(c.link.calls + 1);
        assert_eq!(
            call(&mut c, Request::Unlink { path: b"/gone" }).0[0],
            VFS_OK
        );
        c.link.die_after = Some(c.link.calls + 1);
        assert_eq!(
            call(
                &mut c,
                Request::Rename {
                    from: b"/a",
                    to: b"/b"
                }
            )
            .0[0],
            VFS_OK
        );
        // Without a lost reply the same answers stand.
        assert_eq!(
            call(&mut c, Request::Mkdir { path: b"/d" }).0[0],
            VFS_EXISTS
        );
        assert_eq!(
            call(&mut c, Request::Unlink { path: b"/gone" }).0[0],
            VFS_NOT_FOUND
        );
        assert_eq!(
            call(
                &mut c,
                Request::Rename {
                    from: b"/a",
                    to: b"/b"
                }
            )
            .0[0],
            VFS_NOT_FOUND
        );
    }

    #[test]
    fn a_file_gone_at_the_restart_turns_stale_and_close_still_succeeds() {
        let mut c = Reconnecting::<Fake, 4>::new(Fake::new());
        let (reply, len) = call(
            &mut c,
            Request::OpenOptions {
                path: b"/gone",
                options: OPEN_WRITE,
            },
        );
        let (handle, _) = parse_open_reply(&reply[..len]).unwrap();
        c.link.files[3].1 = false; // unlinked by someone, then the server died
        c.link.die_before = Some(c.link.calls + 1);
        assert_eq!(
            call(
                &mut c,
                Request::Write {
                    handle,
                    offset: 0,
                    data: b"x"
                }
            )
            .0[0],
            VFS_IO_ERROR
        );
        assert_eq!(
            call(
                &mut c,
                Request::Write {
                    handle,
                    offset: 0,
                    data: b"x"
                }
            )
            .0[0],
            VFS_IO_ERROR
        );
        assert_eq!(call(&mut c, Request::Close { handle }).0[0], VFS_OK);
        assert_eq!(call(&mut c, Request::Close { handle }).0[0], VFS_BAD_HANDLE);
    }

    #[test]
    fn a_rename_this_client_made_moves_its_remembered_path() {
        let mut c = Reconnecting::<Fake, 4>::new(Fake::new());
        let (reply, len) = call(
            &mut c,
            Request::OpenOptions {
                path: b"/a",
                options: OPEN_WRITE,
            },
        );
        let (handle, _) = parse_open_reply(&reply[..len]).unwrap();
        assert_eq!(
            call(
                &mut c,
                Request::Rename {
                    from: b"/a",
                    to: b"/c"
                }
            )
            .0[0],
            VFS_OK
        );
        c.link.die_before = Some(c.link.calls + 1);
        let (reply, len) = call(
            &mut c,
            Request::Write {
                handle,
                offset: 0,
                data: b"ok",
            },
        );
        assert_eq!(parse_write_reply(&reply[..len]), Ok(2), "reopened as /c");
    }

    #[test]
    fn a_lost_lock_is_reported_once_and_no_session_means_io_error() {
        struct Dead;
        impl SessionLink for Dead {
            fn exchange(&mut self, _: &[u8], _: u32, _: &mut [u8]) -> Result<usize, LinkError> {
                Err(LinkError::Closed)
            }
            fn reconnect(&mut self) -> bool {
                false
            }
        }
        let mut dead = Reconnecting::<Dead, 2>::new(Dead);
        let mut frame = [0u8; 64];
        let n = (Request::Stat { path: b"/a" })
            .encode_into(&mut frame)
            .unwrap();
        let mut reply = [0u8; 8];
        assert_eq!(dead.call(&frame[..n], 0, &mut reply), 1);
        assert_eq!(reply[0], VFS_IO_ERROR);

        let mut c = Reconnecting::<Fake, 4>::new(Fake::new());
        let (reply, len) = call(
            &mut c,
            Request::OpenOptions {
                path: b"/a",
                options: OPEN_WRITE,
            },
        );
        let (handle, _) = parse_open_reply(&reply[..len]).unwrap();
        c.entry_mut(handle).unwrap().locked = true;
        c.link.die_before = Some(c.link.calls + 1);
        let _ = call(
            &mut c,
            Request::Write {
                handle,
                offset: 0,
                data: b"z",
            },
        );
        assert_eq!(
            call(
                &mut c,
                Request::Lock {
                    handle,
                    operation: crate::LOCK_EX
                }
            )
            .0[0],
            VFS_IO_ERROR
        );
    }
}
