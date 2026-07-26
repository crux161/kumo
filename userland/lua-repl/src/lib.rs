//j485
//j486
//j487
//j488
#![no_std]

//! Build seam between KUMO's freestanding REPL process and the vendored Piccolo VM.
//!
//! KUMO carries a small `core` + `alloc` port of Piccolo 0.3.3. The `piccolo-vm` feature is enabled
//! by default. This slice accepts one finite input line and returns one bounded display value;
//! persistent state, Lua `print`, and channel-backed host functions remain later slices.

extern crate alloc;

#[cfg(feature = "piccolo-vm")]
pub use piccolo as vm;

#[cfg(feature = "piccolo-vm")]
use alloc::vec::Vec;

/// Maximum displayed result bytes returned to the target process.
#[cfg(feature = "piccolo-vm")]
pub const MAX_OUTPUT_BYTES: usize = 192;

/// A successful one-shot evaluation.
#[cfg(feature = "piccolo-vm")]
#[derive(Debug, Eq, PartialEq)]
pub struct Evaluation {
    pub output: Vec<u8>,
    pub truncated: bool,
}

/// A bounded failure class; VM details intentionally remain off the shell's stdout channel.
#[cfg(feature = "piccolo-vm")]
#[derive(Debug)]
pub enum EvalError {
    Vm(vm::StaticError),
    InstructionLimit,
}

#[cfg(feature = "piccolo-vm")]
impl From<vm::StaticError> for EvalError {
    fn from(error: vm::StaticError) -> Self {
        Self::Vm(error)
    }
}

#[cfg(feature = "piccolo-vm")]
struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

#[cfg(feature = "piccolo-vm")]
impl BoundedOutput {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }
}

#[cfg(feature = "piccolo-vm")]
impl vm::io::Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> Result<usize, vm::io::Error> {
        let take = core::cmp::min(
            bytes.len(),
            MAX_OUTPUT_BYTES.saturating_sub(self.bytes.len()),
        );
        self.bytes.extend_from_slice(&bytes[..take]);
        self.truncated |= take != bytes.len();
        Ok(bytes.len())
    }
}

/// Compile and execute one submitted line.
///
/// A line is first treated as an expression (`return <line>`), then retried as a Lua chunk so
/// explicit `return` and statement-only input work too. Execution and displayed output are bounded
/// so a submitted chunk cannot permanently occupy the synchronous Stage-A shell. — KESTREL
#[cfg(feature = "piccolo-vm")]
pub fn evaluate_line(source: &[u8]) -> Result<Evaluation, EvalError> {
    const FUEL_PER_ROUND: i32 = 4096;
    const MAX_FUEL_ROUNDS: usize = 64;

    let mut expression = Vec::with_capacity(b"return ".len() + source.len());
    expression.extend_from_slice(b"return ");
    expression.extend_from_slice(source);
    let mut lua = vm::Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = match vm::Closure::load(ctx, Some("kumo-input"), expression.as_slice()) {
            Ok(closure) => closure,
            Err(_) => vm::Closure::load(ctx, Some("kumo-input"), source)?,
        };
        Ok(ctx.stash(vm::Executor::start(ctx, closure.into(), ())))
    })?;

    let mut complete = false;
    for _ in 0..MAX_FUEL_ROUNDS {
        complete = lua.enter(|ctx| {
            let mut fuel = vm::Fuel::with(FUEL_PER_ROUND);
            ctx.fetch(&executor).step(ctx, &mut fuel)
        });
        if complete {
            break;
        }
    }
    if !complete {
        return Err(EvalError::InstructionLimit);
    }

    lua.try_enter(|ctx| {
        let values: vm::Variadic<Vec<vm::Value<'_>>> = ctx.fetch(&executor).take_result(ctx)??;
        let mut output = BoundedOutput::new();
        for (index, value) in values.into_iter().enumerate() {
            if index != 0 {
                vm::io::Write::write_all(&mut output, b"\t")?;
            }
            value.display(&mut output)?;
        }
        Ok(Evaluation {
            output: output.bytes,
            truncated: output.truncated,
        })
    })
    .map_err(EvalError::from)
}

#[cfg(all(test, feature = "piccolo-vm"))]
mod tests {
    use super::{
        evaluate_line,
        vm::{Lua, Value},
        EvalError, MAX_OUTPUT_BYTES,
    };

    #[test]
    fn freestanding_vm_constructs_core_math_library() {
        let mut lua = Lua::core();
        assert!(lua.total_memory() > 0);
        lua.enter(|ctx| assert!(matches!(ctx.get_global("math"), Value::Table(_))));
    }

    #[test]
    fn expression_executes_through_piccolo() {
        assert_eq!(
            evaluate_line(b"math.floor(41.75) + 1").unwrap().output,
            b"42"
        );
    }

    #[test]
    fn explicit_chunk_returns_multiple_values() {
        assert_eq!(
            evaluate_line(b"return math.floor(8.9), 'kumo'")
                .unwrap()
                .output,
            b"8\tkumo"
        );
    }

    #[test]
    fn statement_fallback_and_parse_errors_are_distinct() {
        assert_eq!(evaluate_line(b"answer = 42").unwrap().output, b"");
        assert!(matches!(evaluate_line(b"("), Err(EvalError::Vm(_))));
    }

    #[test]
    fn displayed_values_are_bounded() {
        let mut source = alloc::vec![b'\''];
        source.extend(core::iter::repeat_n(b'x', 300));
        source.push(b'\'');
        let evaluation = evaluate_line(&source).unwrap();
        assert_eq!(evaluation.output.len(), MAX_OUTPUT_BYTES);
        assert!(evaluation.truncated);
    }

    #[test]
    fn execution_is_bounded() {
        assert!(matches!(
            evaluate_line(b"(function() while true do end end)()"),
            Err(EvalError::InstructionLimit)
        ));
    }
}
