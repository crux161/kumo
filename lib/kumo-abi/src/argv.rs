//! `argv` — the wire format Sora uses to hand a program its arguments.
//!
//! A native KUMO program receives a **read-only argv VMO** handle in `x1` (or `0` for
//! none); the program `vmo_read`s the VMO and walks it with [`unpack_argv`]. The format
//! is deliberately trivial and allocation-free at both ends:
//!
//! ```text
//! [argc: u32 LE][arg0 bytes]\0[arg1 bytes]\0 … [arg(argc-1) bytes]\0
//! ```
//!
//! Trailing bytes (the VMO is a fixed page, zero-padded) are ignored — [`unpack_argv`]
//! takes exactly `argc` entries. By convention `argv[0]` is the program name, like
//! `execve`. There is no NUL inside a shell token, so NUL is a safe separator.

/// Bytes of the fixed-size `argc` header that precedes the packed argument bytes.
pub const ARGV_HEADER_LEN: usize = 4;

/// Pack `args` into `buf` as `[argc][arg0\0 arg1\0 …]`. Returns the number of bytes
/// written, or `None` if `buf` is too small to hold the header plus every argument and
/// its separator.
pub fn pack_argv(args: &[&[u8]], buf: &mut [u8]) -> Option<usize> {
    let header = buf.get_mut(..ARGV_HEADER_LEN)?;
    header.copy_from_slice(&(args.len() as u32).to_le_bytes());
    let mut pos = ARGV_HEADER_LEN;
    for arg in args {
        let end = pos.checked_add(arg.len())?.checked_add(1)?;
        if end > buf.len() {
            return None;
        }
        buf[pos..pos + arg.len()].copy_from_slice(arg);
        buf[pos + arg.len()] = 0;
        pos = end;
    }
    Some(pos)
}

/// Iterate the packed arguments in `buf` (as written by [`pack_argv`]), borrowing each
/// from `buf`. Yields exactly `argc` entries; a truncated `buf` yields fewer rather than
/// faulting.
pub fn unpack_argv(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    let argc = match buf.get(..ARGV_HEADER_LEN) {
        Some(h) => u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize,
        None => 0,
    };
    buf.get(ARGV_HEADER_LEN..)
        .unwrap_or(&[])
        .split(|byte| *byte == 0)
        .take(argc)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    #[test]
    fn round_trips_arguments() {
        let mut buf = [0u8; 64];
        let n = pack_argv(&[b"args", b"alpha", b"beta"], &mut buf).expect("pack");
        let got: Vec<&[u8]> = unpack_argv(&buf[..n]).collect();
        assert_eq!(got, std::vec![&b"args"[..], &b"alpha"[..], &b"beta"[..]]);
    }

    #[test]
    fn trailing_zero_padding_is_ignored() {
        // A fixed page is zero-padded past the packed bytes; argc bounds the walk.
        let mut buf = [0u8; 64];
        pack_argv(&[b"only"], &mut buf).expect("pack");
        let got: Vec<&[u8]> = unpack_argv(&buf).collect();
        assert_eq!(got, std::vec![&b"only"[..]]);
    }

    #[test]
    fn empty_argv_yields_nothing() {
        let mut buf = [0u8; 16];
        let n = pack_argv(&[], &mut buf).expect("pack");
        assert_eq!(unpack_argv(&buf[..n]).count(), 0);
    }

    #[test]
    fn pack_rejects_overflow() {
        let mut buf = [0u8; 8];
        assert_eq!(pack_argv(&[b"too-long-for-this-buffer"], &mut buf), None);
    }
}
