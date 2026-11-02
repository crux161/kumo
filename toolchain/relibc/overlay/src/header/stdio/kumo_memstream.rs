//j738
//! Write-only dynamic memory streams for Mesa's compiler diagnostics. — SAKER

use super::{Buffer, FILE, Pending, Writer, helpers};
use crate::{
    c_str::CStr,
    c_vec::CVec,
    header::errno::{EINVAL, ENOMEM, EOVERFLOW},
    io::{self, Write},
    platform::{ERRNO, types::*},
};
use alloc::{boxed::Box, vec::Vec};
use core::{mem, ptr};

struct MemoryWriter {
    buffer: CVec<u8>,
    position: usize,
    buffer_out: *mut *mut c_char,
    size_out: *mut size_t,
    saved_terminator: Option<(usize, u8)>,
}

// FILE's recursive mutex serializes every access to this writer. Output pointers
// belong to the caller and follow open_memstream's flush/close lifetime. — SAKER
unsafe impl Send for MemoryWriter {}

impl MemoryWriter {
    fn restore_terminator(&mut self) {
        if let Some((offset, byte)) = self.saved_terminator.take() {
            self.buffer[offset] = byte;
        }
    }

    fn seek(&mut self, offset: off_t, whence: c_int) -> io::Result<off_t> {
        let base = match whence {
            super::SEEK_SET => 0,
            super::SEEK_CUR => self.position as off_t,
            super::SEEK_END => (self.buffer.len() - 1) as off_t,
            _ => return Err(io::Error::from_raw_os_error(EINVAL)),
        };
        let next = base
            .checked_add(offset)
            .filter(|n| *n >= 0 && (*n as u64) < isize::MAX as u64)
            .ok_or_else(|| io::Error::from_raw_os_error(EINVAL))?;
        self.restore_terminator();
        self.position = next as usize;
        Ok(next)
    }
}

impl Write for MemoryWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .position
            .checked_add(bytes.len())
            .filter(|n| *n < isize::MAX as usize)
            .ok_or_else(|| io::Error::from_raw_os_error(EOVERFLOW))?;
        if bytes.is_empty() {
            return Ok(0);
        }
        self.restore_terminator();
        let needed = end.saturating_add(1).saturating_sub(self.buffer.len());
        self.buffer
            .reserve(needed)
            .map_err(|_| io::Error::from_raw_os_error(ENOMEM))?;
        while self.buffer.len() <= end {
            self.buffer
                .push(0)
                .map_err(|_| io::Error::from_raw_os_error(ENOMEM))?;
        }
        self.buffer[self.position..end].copy_from_slice(bytes);
        self.position = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.restore_terminator();
        let size = self.position.min(self.buffer.len() - 1);
        // A flush after seeking backwards publishes a shorter, terminated view;
        // retain the overwritten byte for subsequent seek/write operations. — SAKER
        self.saved_terminator = Some((size, self.buffer[size]));
        self.buffer[size] = 0;
        unsafe {
            ptr::write(self.buffer_out, self.buffer.as_mut_ptr().cast());
            ptr::write(self.size_out, size);
        }
        Ok(())
    }
}

impl Pending for MemoryWriter {
    fn pending(&self) -> size_t {
        0
    }
}

impl Writer for MemoryWriter {
    fn purge(&mut self) {}

    fn is_memstream(&self) -> bool {
        true
    }

    fn memory_seek(&mut self, offset: off_t, whence: c_int) -> Option<io::Result<off_t>> {
        Some(self.seek(offset, whence))
    }

    fn memory_position(&self) -> Option<off_t> {
        Some(self.position as off_t)
    }
}

impl Drop for MemoryWriter {
    fn drop(&mut self) {
        // CVec uses C allocation. After fclose the published storage belongs to
        // the caller and must remain valid for free(), unlike a Rust Vec. — SAKER
        mem::replace(&mut self.buffer, CVec::new()).leak();
    }
}

/// See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/open_memstream.html>.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn open_memstream(
    buffer_out: *mut *mut c_char,
    size_out: *mut size_t,
) -> *mut FILE {
    if buffer_out.is_null() || size_out.is_null() {
        ERRNO.set(EINVAL);
        return ptr::null_mut();
    }
    // No FD is minted. The FILE owns a memory writer; the placeholder File is
    // non-owning and all descriptor operations refuse it. — SAKER
    let mut stream = match helpers::_fdopen(-1, CStr::from_bytes_with_nul(b"w\0").unwrap()) {
        Ok(stream) => stream,
        Err(error) => {
            ERRNO.set(error.0);
            return ptr::null_mut();
        }
    };
    stream.file.reference = true;
    let mut buffer = CVec::new();
    if buffer.push(0).is_err() {
        ERRNO.set(ENOMEM);
        return ptr::null_mut();
    }
    stream.read_buf = Buffer::Owned(Vec::new());
    stream.orientation = -1;
    stream.writer = Box::new(MemoryWriter {
        buffer,
        position: 0,
        buffer_out,
        size_out,
        saved_terminator: None,
    });
    Box::into_raw(stream)
}
