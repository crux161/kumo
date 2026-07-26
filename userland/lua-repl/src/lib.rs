//j485
//j486
//j487
#![no_std]

//! Build seam between KUMO's freestanding REPL process and the vendored Piccolo VM.
//!
//! KUMO carries a small `core` + `alloc` port of Piccolo 0.3.3. The `piccolo-vm` feature is enabled
//! by default. The first target evaluator deliberately runs one fixed expression; interactive
//! channel input and Lua host functions remain later slices.

#[cfg(feature = "piccolo-vm")]
pub use piccolo as vm;

/// First target evaluator proof: compile and run a chunk that resolves the core `math` table.
#[cfg(feature = "piccolo-vm")]
pub fn evaluate_fixed_expression() -> Result<i64, vm::StaticError> {
    const SOURCE: &[u8] = b"return math.floor(41.75) + 1";

    let mut lua = vm::Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = vm::Closure::load(ctx, Some("kumo-fixed"), SOURCE)?;
        Ok(ctx.stash(vm::Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<i64>(&executor)
}

#[cfg(all(test, feature = "piccolo-vm"))]
mod tests {
    use super::{
        evaluate_fixed_expression,
        vm::{Lua, Value},
    };

    #[test]
    fn freestanding_vm_constructs_core_math_library() {
        let mut lua = Lua::core();
        assert!(lua.total_memory() > 0);
        lua.enter(|ctx| assert!(matches!(ctx.get_global("math"), Value::Table(_))));
    }

    #[test]
    fn fixed_expression_executes_through_piccolo() {
        assert_eq!(evaluate_fixed_expression().unwrap(), 42);
    }
}
