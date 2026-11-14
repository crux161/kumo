//j839
//j896
// The resident ax88 proof chain runs EP0 exchanges at its deepest point; the loader's
// 16 KiB default stack overflowed there twice (J836 sampler, J839 re-arm). Ask lld for
// a 32 KiB PT_GNU_STACK in this image only. — TIMBERDOODLE
fn main() {
    // Only the bare-metal ELF binary needs this; Darwin host tests link Mach-O. — KESTREL
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        println!("cargo:rustc-link-arg-bin=drv-xhci=-zstack-size=32768");
    }
}
