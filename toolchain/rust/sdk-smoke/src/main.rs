//j857
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
}
