//! The byte-oriented I/O traits Piccolo needs to compile and display values.
//!
//! With `std`, these are the standard library traits. On a freestanding target, the small
//! implementations below cover in-memory source buffers and output buffers; KUMO channel adapters
//! can implement the same traits in the REPL crate.

#[cfg(feature = "std")]
pub use std::io::{BufRead, BufReader, Error, ErrorKind, Read, Write};

#[cfg(not(feature = "std"))]
mod nostd {
    use alloc::vec::Vec;
    use core::{cmp, fmt};

    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    pub enum ErrorKind {
        Interrupted,
        Other,
    }

    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    pub struct Error {
        kind: ErrorKind,
    }

    impl Error {
        pub const fn new(kind: ErrorKind) -> Self {
            Self { kind }
        }

        pub const fn kind(self) -> ErrorKind {
            self.kind
        }
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("byte I/O error")
        }
    }

    impl core::error::Error for Error {}

    pub trait Read {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error>;

        fn read_to_end(&mut self, out: &mut Vec<u8>) -> Result<usize, Error> {
            let start = out.len();
            let mut buf = [0; 256];
            loop {
                match self.read(&mut buf)? {
                    0 => return Ok(out.len() - start),
                    read => out.extend_from_slice(&buf[..read]),
                }
            }
        }
    }

    impl<R: Read + ?Sized> Read for &mut R {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
            (**self).read(buf)
        }
    }

    impl Read for &[u8] {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
            let read = cmp::min(self.len(), buf.len());
            buf[..read].copy_from_slice(&self[..read]);
            *self = &self[read..];
            Ok(read)
        }
    }

    pub trait BufRead: Read {
        fn fill_buf(&mut self) -> Result<&[u8], Error>;
        fn consume(&mut self, amount: usize);
    }

    pub struct BufReader<R> {
        inner: R,
        buffer: Vec<u8>,
        position: usize,
        capacity: usize,
    }

    impl<R: Read> BufReader<R> {
        pub fn new(inner: R) -> Self {
            Self::with_capacity(8 * 1024, inner)
        }

        pub fn with_capacity(capacity: usize, inner: R) -> Self {
            Self {
                inner,
                buffer: Vec::new(),
                position: 0,
                capacity: capacity.max(3),
            }
        }
    }

    impl<R: Read> BufRead for BufReader<R> {
        fn fill_buf(&mut self) -> Result<&[u8], Error> {
            if self.position == self.buffer.len() {
                self.buffer.resize(self.capacity, 0);
                let read = self.inner.read(&mut self.buffer)?;
                self.buffer.truncate(read);
                self.position = 0;
            }
            Ok(&self.buffer[self.position..])
        }

        fn consume(&mut self, amount: usize) {
            self.position = cmp::min(self.position.saturating_add(amount), self.buffer.len());
        }
    }

    impl<R: Read> Read for BufReader<R> {
        fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
            let buffered = self.fill_buf()?;
            let read = cmp::min(buffered.len(), out.len());
            out[..read].copy_from_slice(&buffered[..read]);
            self.consume(read);
            Ok(read)
        }
    }

    pub trait Write {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Error>;

        fn flush(&mut self) -> Result<(), Error> {
            Ok(())
        }

        fn write_all(&mut self, mut buf: &[u8]) -> Result<(), Error> {
            while !buf.is_empty() {
                let written = self.write(buf)?;
                if written == 0 {
                    return Err(Error::new(ErrorKind::Other));
                }
                buf = &buf[written..];
            }
            Ok(())
        }

        fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> Result<(), Error> {
            struct Adapter<'a, W: ?Sized> {
                writer: &'a mut W,
                error: Option<Error>,
            }

            impl<W: Write + ?Sized> fmt::Write for Adapter<'_, W> {
                fn write_str(&mut self, value: &str) -> fmt::Result {
                    self.writer.write_all(value.as_bytes()).map_err(|error| {
                        self.error = Some(error);
                        fmt::Error
                    })
                }
            }

            let mut adapter = Adapter {
                writer: self,
                error: None,
            };
            match fmt::write(&mut adapter, args) {
                Ok(()) => Ok(()),
                Err(_) => Err(adapter
                    .error
                    .unwrap_or_else(|| Error::new(ErrorKind::Other))),
            }
        }
    }

    impl<W: Write + ?Sized> Write for &mut W {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
            (**self).write(buf)
        }

        fn flush(&mut self) -> Result<(), Error> {
            (**self).flush()
        }
    }

    impl Write for Vec<u8> {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
            self.extend_from_slice(buf);
            Ok(buf.len())
        }
    }
}

#[cfg(not(feature = "std"))]
pub use nostd::{BufRead, BufReader, Error, ErrorKind, Read, Write};

/// Skips a leading UTF-8 BOM and Unix shebang, matching `luaL_loadfile`.
pub fn skip_prefix<R: BufRead>(reader: &mut R) -> Result<(), Error> {
    if {
        let buf = reader.fill_buf()?;
        buf.len() >= 3 && buf[0] == 0xef && buf[1] == 0xbb && buf[2] == 0xbf
    } {
        reader.consume(3);
    }

    let has_shebang = reader.fill_buf()?.first() == Some(&b'#');
    if has_shebang {
        reader.consume(1);
        loop {
            let to_consume = {
                let buf = reader.fill_buf()?;
                buf.iter()
                    .position(|byte| *byte == b'\n')
                    .unwrap_or(buf.len())
            };
            if to_consume == 0 {
                break;
            }
            reader.consume(to_consume);
        }
    }

    Ok(())
}

/// Wraps a Lua source stream in a buffered reader after skipping its optional prefix.
pub fn buffered_read<R: Read>(reader: R) -> Result<BufReader<R>, Error> {
    let mut reader = BufReader::new(reader);
    skip_prefix(&mut reader)?;
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skip_prefix() {
        let test_file = [
            0xef, 0xbb, 0xbf, b'#', 0x00, 0x00, 0x00, 0xff, b'\n', 0x1, 0x2, 0x3,
        ];
        let mut reader = BufReader::with_capacity(3, &test_file[..]);

        skip_prefix(&mut reader).unwrap();

        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, vec![b'\n', 0x1, 0x2, 0x3]);
    }
}
