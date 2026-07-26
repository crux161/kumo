//j485
//j486
#![no_std]

//! Build seam between KUMO's freestanding REPL process and the vendored Piccolo VM.
//!
//! KUMO carries a small `core` + `alloc` port of Piccolo 0.3.3. The `piccolo-vm` feature is enabled
//! by default so normal metal-image builds prove the freestanding VM graph before the binary starts
//! constructing a Lua state or exposing channel-backed host functions.

#[cfg(feature = "piccolo-vm")]
pub use piccolo as vm;

#[cfg(all(test, feature = "piccolo-vm"))]
mod tests {
    use super::vm::{Lua, Value};

    #[test]
    fn freestanding_vm_constructs_core_math_library() {
        let mut lua = Lua::core();
        assert!(lua.total_memory() > 0);
        lua.enter(|ctx| assert!(matches!(ctx.get_global("math"), Value::Table(_))));
    }
}
