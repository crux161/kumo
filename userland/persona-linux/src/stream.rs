//j915
//! A message tail retained by a byte-stream descriptor after a short read.

#[derive(Clone, Copy)]
pub struct ReadTail<const N: usize> {
    bytes: [u8; N],
    at: usize,
    len: usize,
}

impl<const N: usize> ReadTail<N> {
    pub const EMPTY: Self = Self {
        bytes: [0; N],
        at: 0,
        len: 0,
    };

    pub fn is_empty(&self) -> bool {
        self.at == self.len
    }

    pub fn storage(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    pub fn received(&mut self, len: usize) -> bool {
        if len > N {
            return false;
        }
        self.at = 0;
        self.len = len;
        true
    }

    pub fn take(&mut self, out: &mut [u8]) -> usize {
        let count = out.len().min(self.len - self.at);
        out[..count].copy_from_slice(&self.bytes[self.at..self.at + count]);
        self.at += count;
        count
    }
}

#[cfg(test)]
mod tests {
    use super::ReadTail;

    #[test]
    fn short_and_zero_reads_preserve_every_byte_in_order() {
        let mut tail = ReadTail::<8>::EMPTY;
        tail.storage().copy_from_slice(b"ab\0defgh");
        assert!(tail.received(8));
        assert_eq!(tail.take(&mut []), 0);
        let mut short = [0; 3];
        assert_eq!(tail.take(&mut short), 3);
        assert_eq!(&short, b"ab\0");
        let mut rest = [0; 8];
        assert_eq!(tail.take(&mut rest), 5);
        assert_eq!(&rest[..5], b"defgh");
        assert!(tail.is_empty());
        assert_eq!(tail.take(&mut short), 0);
    }

    #[test]
    fn status_values_cannot_become_stream_lengths() {
        let mut tail = ReadTail::<4>::EMPTY;
        tail.storage().copy_from_slice(b"data");
        assert!(tail.received(4));
        assert!(!tail.received(u32::MAX as usize));
        assert!(!tail.received(5));
        let mut out = [0; 4];
        assert_eq!(tail.take(&mut out), 4);
        assert_eq!(&out, b"data");
        assert!(tail.received(0));
        assert!(tail.is_empty());
    }
}
