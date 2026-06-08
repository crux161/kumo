use core::fmt;

pub struct Writer;

impl fmt::Write for Writer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        write(text.as_bytes());
        Ok(())
    }
}

pub fn write(bytes: &[u8]) {
    kumo_hal::active::early_console_write(bytes);
}

pub fn write_str(text: &str) {
    write(text.as_bytes());
}

pub fn write_fmt(args: fmt::Arguments<'_>) {
    use fmt::Write;

    let _ = Writer.write_fmt(args);
}
