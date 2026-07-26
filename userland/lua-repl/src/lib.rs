//j485
//j486
//j487
//j488
//j489
#![no_std]

//! Build seam between KUMO's freestanding REPL process and the vendored Piccolo VM.
//!
//! KUMO carries a small `core` + `alloc` port of Piccolo 0.3.3. The `piccolo-vm` feature is enabled
//! by default. Two entry points share one implementation: [`evaluate_line`] runs a line in a VM it
//! throws away, and [`LuaSession`] keeps one VM alive across lines so globals persist — the
//! difference between evaluating Lua and *having* Lua. Both bound what a line may do: its own
//! instruction budget, its own `print` budget, and a bounded displayed result, so a runaway line
//! ends that line rather than occupying the shell. Further host functions remain later slices.

extern crate alloc;

#[cfg(feature = "piccolo-vm")]
pub use piccolo as vm;

#[cfg(feature = "piccolo-vm")]
use alloc::{rc::Rc, vec::Vec};
#[cfg(feature = "piccolo-vm")]
use core::cell::RefCell;

/// Maximum displayed result bytes returned to the target process.
#[cfg(feature = "piccolo-vm")]
pub const MAX_OUTPUT_BYTES: usize = 192;

/// Maximum bytes all `print` calls may submit during one chunk.
#[cfg(feature = "piccolo-vm")]
pub const MAX_PRINT_BYTES: usize = MAX_OUTPUT_BYTES * 4;

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
    limit: usize,
}

/// Where a `print` call's already-rendered line goes. Implemented blanket-style for closures so
/// the one-shot API can keep taking one, and separately by [`PrintToBuffer`] so a session can own
/// a *nameable* sink type — a closure's type cannot appear in [`LuaSession`]'s own type.
#[cfg(feature = "piccolo-vm")]
trait PrintSink {
    fn emit(&mut self, bytes: &[u8]);
}

#[cfg(feature = "piccolo-vm")]
impl<F: FnMut(&[u8])> PrintSink for F {
    fn emit(&mut self, bytes: &[u8]) {
        self(bytes)
    }
}

/// A session's print sink: append to a buffer the session drains after each line.
#[cfg(feature = "piccolo-vm")]
struct PrintToBuffer(Rc<RefCell<Vec<u8>>>);

#[cfg(feature = "piccolo-vm")]
impl PrintSink for PrintToBuffer {
    fn emit(&mut self, bytes: &[u8]) {
        self.0.borrow_mut().extend_from_slice(bytes);
    }
}

#[cfg(feature = "piccolo-vm")]
struct PrintState<S> {
    sink: S,
    remaining: usize,
}

#[cfg(feature = "piccolo-vm")]
impl BoundedOutput {
    fn new() -> Self {
        Self::with_limit(MAX_OUTPUT_BYTES)
    }

    fn with_limit(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
            limit,
        }
    }

    fn mark_truncation(&mut self) {
        if self.truncated {
            let marker = b"...";
            self.bytes.truncate(self.limit.saturating_sub(marker.len()));
            self.bytes.extend_from_slice(marker);
        }
    }
}

#[cfg(feature = "piccolo-vm")]
impl vm::io::Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> Result<usize, vm::io::Error> {
        let take = core::cmp::min(bytes.len(), self.limit.saturating_sub(self.bytes.len()));
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
    evaluate_line_with_print(source, |_| {})
}

/// Compile and execute one submitted line with a bounded Lua `print` host function.
///
/// The sink owns the actual output authority; Piccolo receives no handle and can only submit one
/// already-rendered line at a time through this callback. — KESTREL
#[cfg(feature = "piccolo-vm")]
pub fn evaluate_line_with_print<F>(source: &[u8], print_sink: F) -> Result<Evaluation, EvalError>
where
    F: FnMut(&[u8]) + 'static,
{
    let mut lua = vm::Lua::core();
    let print_state = Rc::new(RefCell::new(PrintState {
        sink: print_sink,
        remaining: MAX_PRINT_BYTES,
    }));
    install_print(&mut lua, &print_state)?;
    run_chunk(&mut lua, source)
}

/// Bind the bounded host `print` into `lua`'s globals. The sink owns the actual output authority;
/// Piccolo receives no handle and can only submit one already-rendered line at a time.
#[cfg(feature = "piccolo-vm")]
fn install_print<S>(
    lua: &mut vm::Lua,
    print_state: &Rc<RefCell<PrintState<S>>>,
) -> Result<(), vm::StaticError>
where
    S: PrintSink + 'static,
{
    lua.try_enter(|ctx| {
        let print =
            vm::Callback::from_fn_with(&ctx, Rc::clone(print_state), |state, _, _, mut stack| {
                let mut state = state.borrow_mut();
                if state.remaining <= b"...\n".len() {
                    if state.remaining != 0 {
                        state.sink.emit(b"...\n");
                        state.remaining = 0;
                    }
                    stack.clear();
                    return Ok(vm::CallbackReturn::Return);
                }

                let line_budget =
                    core::cmp::min(MAX_OUTPUT_BYTES, state.remaining - b"...\n".len());
                let mut output = BoundedOutput::with_limit(line_budget - 1);
                for (index, value) in (&stack).into_iter().enumerate() {
                    if index != 0 {
                        vm::io::Write::write_all(&mut output, b"\t")?;
                    }
                    value.display(&mut output)?;
                }
                stack.clear();
                output.mark_truncation();
                output.bytes.push(b'\n');
                state.remaining -= output.bytes.len();
                state.sink.emit(&output.bytes);
                Ok(vm::CallbackReturn::Return)
            });
        ctx.set_global("print", print)?;
        Ok(())
    })
}

/// Compile and run one line against an existing VM, returning its displayed result.
///
/// The VM is borrowed, never created here — that is what lets a session keep its globals while a
/// one-shot evaluation throws its VM away.
#[cfg(feature = "piccolo-vm")]
fn run_chunk(lua: &mut vm::Lua, source: &[u8]) -> Result<Evaluation, EvalError> {
    const FUEL_PER_ROUND: i32 = 4096;
    const MAX_FUEL_ROUNDS: usize = 64;

    let mut expression = Vec::with_capacity(b"return ".len() + source.len());
    expression.extend_from_slice(b"return ");
    expression.extend_from_slice(source);
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

/// What one line of an interactive session produced.
///
/// Kept as data rather than pre-rendered text so the caller decides the presentation: the child
/// process writes a terminal transcript, the tests read the fields.
#[cfg(feature = "piccolo-vm")]
#[derive(Debug, Default)]
pub struct LineOutcome {
    /// Everything the line's `print` calls emitted, already newline-terminated.
    pub printed: Vec<u8>,
    /// The line's displayed result — empty for a statement that returns nothing.
    pub value: Vec<u8>,
    /// The result was too long to render in full.
    pub truncated: bool,
    /// Set when the line failed. The session survives it: globals from earlier lines are intact
    /// and the next line runs normally.
    pub error: Option<EvalError>,
}

/// A Lua session that outlives the line that started it.
///
/// One `vm::Lua` is created up front and every line runs against it, so `x = 7` on one line and
/// `x * 6` on the next see the same globals — which is the whole difference between running a
/// language and having one. The per-line bounds are unchanged from the one-shot path: each line
/// gets its own instruction budget and its own print budget, so a runaway line ends *that line*
/// rather than the session.
#[cfg(feature = "piccolo-vm")]
pub struct LuaSession {
    lua: vm::Lua,
    printed: Rc<RefCell<Vec<u8>>>,
    print_state: Rc<RefCell<PrintState<PrintToBuffer>>>,
}

#[cfg(feature = "piccolo-vm")]
impl LuaSession {
    /// Build a session with the core library and the bounded host `print` installed.
    pub fn new() -> Result<Self, EvalError> {
        let mut lua = vm::Lua::core();
        let printed = Rc::new(RefCell::new(Vec::new()));
        let print_state = Rc::new(RefCell::new(PrintState {
            sink: PrintToBuffer(Rc::clone(&printed)),
            remaining: MAX_PRINT_BYTES,
        }));
        install_print(&mut lua, &print_state)?;
        Ok(Self {
            lua,
            printed,
            print_state,
        })
    }

    /// Run one line against the session's persistent globals.
    pub fn eval_line(&mut self, source: &[u8]) -> LineOutcome {
        // Each line starts with a full print budget and an empty transcript; the budget is what
        // stops one chatty loop from consuming the whole session's output allowance.
        self.printed.borrow_mut().clear();
        self.print_state.borrow_mut().remaining = MAX_PRINT_BYTES;

        let result = run_chunk(&mut self.lua, source);
        let printed = core::mem::take(&mut *self.printed.borrow_mut());
        match result {
            Ok(evaluation) => LineOutcome {
                printed,
                value: evaluation.output,
                truncated: evaluation.truncated,
                error: None,
            },
            Err(error) => LineOutcome {
                printed,
                error: Some(error),
                ..LineOutcome::default()
            },
        }
    }
}

#[cfg(all(test, feature = "piccolo-vm"))]
mod tests {
    use alloc::{rc::Rc, vec::Vec};
    use core::cell::RefCell;

    use super::{
        evaluate_line, evaluate_line_with_print,
        vm::{Lua, Value},
        EvalError, LuaSession, MAX_OUTPUT_BYTES, MAX_PRINT_BYTES,
    };

    fn session() -> LuaSession {
        LuaSession::new().expect("session")
    }

    #[test]
    fn session_globals_survive_across_lines() {
        let mut lua = session();
        assert!(lua.eval_line(b"x = 7").error.is_none());
        let outcome = lua.eval_line(b"x * 6");
        assert!(outcome.error.is_none());
        assert_eq!(outcome.value, b"42");
    }

    #[test]
    fn session_functions_and_tables_survive_across_lines() {
        let mut lua = session();
        assert!(lua
            .eval_line(b"function double(n) return n * 2 end")
            .error
            .is_none());
        assert!(lua.eval_line(b"t = { 20, 1 }").error.is_none());
        assert_eq!(lua.eval_line(b"double(t[1]) + t[2]").value, b"41");
    }

    #[test]
    fn a_failed_line_does_not_poison_the_session() {
        let mut lua = session();
        lua.eval_line(b"kept = 'still here'");
        assert!(matches!(lua.eval_line(b"(").error, Some(EvalError::Vm(_))));
        assert!(matches!(
            lua.eval_line(b"nosuchfunction()").error,
            Some(EvalError::Vm(_))
        ));
        let outcome = lua.eval_line(b"kept");
        assert!(outcome.error.is_none());
        assert_eq!(outcome.value, b"still here");
    }

    #[test]
    fn a_runaway_line_ends_that_line_not_the_session() {
        let mut lua = session();
        lua.eval_line(b"before = 1");
        assert!(matches!(
            lua.eval_line(b"while true do end").error,
            Some(EvalError::InstructionLimit)
        ));
        assert_eq!(lua.eval_line(b"before + 1").value, b"2");
    }

    #[test]
    fn session_print_output_is_captured_per_line() {
        let mut lua = session();
        let outcome = lua.eval_line(b"print('hello', 42); return 'done'");
        assert_eq!(outcome.printed, b"hello\t42\n");
        assert_eq!(outcome.value, b"done");

        // The next line starts with an empty transcript rather than inheriting the last one.
        let outcome = lua.eval_line(b"print('again')");
        assert_eq!(outcome.printed, b"again\n");
        assert!(outcome.value.is_empty());
    }

    #[test]
    fn each_session_line_gets_its_own_print_budget() {
        let mut lua = session();
        let first = lua.eval_line(b"for i = 1, 500 do print(i) end");
        assert_eq!(first.printed.len(), MAX_PRINT_BYTES);
        assert!(first.printed.ends_with(b"...\n"));
        // Budget is per line, so a chatty line does not silence the ones after it.
        assert_eq!(lua.eval_line(b"print('after')").printed, b"after\n");
    }

    #[test]
    fn session_results_are_bounded_like_the_one_shot_path() {
        let mut lua = session();
        let mut source = alloc::vec![b'\''];
        source.extend(core::iter::repeat_n(b'x', 300));
        source.push(b'\'');
        let outcome = lua.eval_line(&source);
        assert_eq!(outcome.value.len(), MAX_OUTPUT_BYTES);
        assert!(outcome.truncated);
    }

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

    #[test]
    fn print_host_function_emits_bounded_tab_separated_lines() {
        let printed = Rc::new(RefCell::new(Vec::<Vec<u8>>::new()));
        let captured = Rc::clone(&printed);
        let evaluation = evaluate_line_with_print(
            b"local answer = 42; print('hello', answer); print(nil, true); return answer",
            move |line| captured.borrow_mut().push(line.to_vec()),
        )
        .unwrap();

        assert_eq!(evaluation.output, b"42");
        assert_eq!(
            printed.borrow().as_slice(),
            [b"hello\t42\n".as_slice(), b"nil\ttrue\n".as_slice()]
        );
    }

    #[test]
    fn print_host_function_marks_truncated_output_and_keeps_newline() {
        let mut source = alloc::vec![b'p', b'r', b'i', b'n', b't', b'(', b'\''];
        source.extend(core::iter::repeat_n(b'x', 300));
        source.extend_from_slice(b"')");
        let printed = Rc::new(RefCell::new(Vec::<Vec<u8>>::new()));
        let captured = Rc::clone(&printed);
        evaluate_line_with_print(&source, move |line| {
            captured.borrow_mut().push(line.to_vec())
        })
        .unwrap();

        let printed = printed.borrow();
        assert_eq!(printed[0].len(), MAX_OUTPUT_BYTES);
        assert!(printed[0].ends_with(b"...\n"));
    }

    #[test]
    fn print_host_function_caps_all_output_from_one_chunk() {
        let printed = Rc::new(RefCell::new(Vec::<Vec<u8>>::new()));
        let captured = Rc::clone(&printed);
        evaluate_line_with_print(b"for i = 1, 500 do print(i) end", move |line| {
            captured.borrow_mut().push(line.to_vec())
        })
        .unwrap();

        let printed = printed.borrow();
        assert_eq!(printed.iter().map(Vec::len).sum::<usize>(), MAX_PRINT_BYTES);
        assert_eq!(printed.last().unwrap(), b"...\n");
    }
}
