//j857
//j897
#![feature(restricted_std)]
//! SH0 (PLAN/022): the first program built against the prebuilt `aarch64-unknown-kumo`
//! sysroot with NO `-Z build-std`, from two crates and two source files. Its stdout line
//! proves relibc's stdio init (J856); the ring line is the durable serial record.
//! — TIMBERDOODLE

include!("../../std-rt/glue.rs");

mod info;

fn main() {
    use std::collections::HashMap;
    use std::time::Instant;

    let start = Instant::now();
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for word in info::words() {
        *counts.entry(word).or_insert(0) += 1;
    }
    let elapsed = start.elapsed();

    println!("sdk-smoke: stdout via relibc stdio -> OK");
    let line = format!(
        "sdk-smoke: multi-file std image on kumo: lib={} words={} pairs={} elapsed_us={} -> OK\n",
        sdk_smoke_lib::label(),
        info::words().len(),
        counts.len(),
        elapsed.as_micros()
    );
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, line.as_bytes());
    dev_null();
}

/// j897: `run` grants no stdin and no filesystem, so reaching `main` at all already proves
/// std's start-up seated /dev/null on fd 0 (it aborts when it cannot). This opens the device
/// again by path, as a program would, and checks each half of its contract.
fn dev_null() {
    use std::io::{Read, Write};
    use std::os::unix::fs::FileTypeExt;

    let result = (|| -> std::io::Result<bool> {
        let mut null = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")?;
        let wrote = null.write(b"hello")?;
        let mut buf = [0u8; 8];
        let read = null.read(&mut buf)?;
        let char_device = null.metadata()?.file_type().is_char_device();
        Ok(wrote == 5 && read == 0 && char_device)
    })();
    let line: &[u8] = match result {
        Ok(true) => b"sdk-smoke: /dev/null write=5 read=0 chardev without an fs grant -> OK\n",
        Ok(false) => b"sdk-smoke: /dev/null contract -> FAIL\n",
        Err(_) => b"sdk-smoke: /dev/null open -> FAIL\n",
    };
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, line);
}
