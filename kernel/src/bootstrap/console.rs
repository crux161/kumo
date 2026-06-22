use core::fmt;

pub struct Writer;

impl fmt::Write for Writer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        write(text.as_bytes());
        Ok(())
    }
}

pub fn write(bytes: &[u8]) {
    // P6-e: when the Sora console server is live (and parked), `klog!` traffic rides the
    // console channel — Sora renders it via `DebugWrite`. Early boot, panic (the Tower
    // disables routing), and anything that runs while Sora itself is current fall back
    // to the direct path below.
    if crate::usermode::try_console_route(bytes) {
        return;
    }
    // J247: the fallback is ownership-aware, not a raw device write. Before the J246
    // framebuffer handoff the kernel owns the glass and this paints via the HAL; after
    // handoff the HAL cursor is dormant, so `klog!` (e.g. the final Stage-A check block,
    // emitted with `CONSOLE_ROUTE` off) is queued to the drv-fb owner rather than dropped.
    crate::usermode::console_write_without_switch(bytes);
}

pub fn write_str(text: &str) {
    write(text.as_bytes());
}

/// Collects formatted output into a fixed buffer, flushing only when full.
/// Call [`LineBuf::flush`] after `write_fmt` to emit any trailing partial buffer.
struct LineBuf<const N: usize> {
    buf: [u8; N],
    pos: usize,
}

impl<const N: usize> LineBuf<N> {
    fn flush(&mut self) {
        if self.pos > 0 {
            write(&self.buf[..self.pos]);
            self.pos = 0;
        }
    }
}

impl<const N: usize> fmt::Write for LineBuf<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut bytes = text.as_bytes();
        while !bytes.is_empty() {
            if self.pos == self.buf.len() {
                write(&self.buf);
                self.pos = 0;
            }
            let n = bytes.len().min(self.buf.len() - self.pos);
            self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
            self.pos += n;
            bytes = &bytes[n..];
        }
        Ok(())
    }
}

pub fn write_fmt(args: fmt::Arguments<'_>) {
    use fmt::Write;

    let mut buf = LineBuf::<256> {
        buf: [0; 256],
        pos: 0,
    };
    let _ = buf.write_fmt(args);
    buf.flush();
}
