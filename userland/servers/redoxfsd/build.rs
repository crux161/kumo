//j928
// RedoxFS keeps whole 4 KiB blocks (headers, tree and directory blocks) in locals, several
// deep in one transaction; the loader's 16 KiB default stack is too small. Ask lld for a
// 256 KiB PT_GNU_STACK, on the bare-metal image only (Darwin host tests link Mach-O). — TEAL
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        println!("cargo:rustc-link-arg-bin=redoxfsd=-zstack-size=262144");
    }
}
