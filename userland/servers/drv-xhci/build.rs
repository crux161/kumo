//j839
// The resident ax88 proof chain runs EP0 exchanges at its deepest point; the loader's
// 16 KiB default stack overflowed there twice (J836 sampler, J839 re-arm). Ask lld for
// a 32 KiB PT_GNU_STACK in this image only. — TIMBERDOODLE
fn main() {
    println!("cargo:rustc-link-arg=-zstack-size=32768");
}
