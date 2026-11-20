//j920
#![no_std]
// SH7-e: the first crate KUMO compiles — no_std, core only, one object.
#[no_mangle]
pub extern "C" fn kumo_add(a: u64, b: u64) -> u64 {
    a.wrapping_add(b)
}
