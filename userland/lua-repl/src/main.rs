//j485
//j486
//j487
//j488
//j489
#![no_std]
#![no_main]

//! `lua-repl` — KUMO's Lua process.
//!
//! Two shapes, one binary. Launched with `-i` it is an **interactive session**: it parks on stdin,
//! evaluates each line the shell submits against one long-lived VM (so `x = 7` on one line and
//! `x * 6` on the next agree about `x`), and answers each stdin message with exactly one stdout
//! message. That one-message-per-line framing is the whole protocol — the shell never has to guess
//! how much output a line produced, or poll for more.
//!
//! Every interactive reply ends with this process's own prompt, which is what keeps the framing
//! honest: a line that prints nothing still has a reply to send, so the shell's read can never
//! block waiting for output that was never coming. The shell relays bytes and never has to know
//! what a Lua prompt looks like; the session ends when the reply stops arriving at all.
//!
//! Launched without it, the historical one-shot form: one line in, one `lua-repl: <value>` out,
//! exit. Both run the same session type; the difference is only how many lines arrive and how the
//! transcript is dressed.
//!
//! The session ends when the shell closes stdin, or when a line is `exit` / `quit`. The VM never
//! gets a handle to anything: `print` reaches the terminal only through the stdout channel this
//! process was granted.

use kumo_abi::{unpack_argv, Handle};
use kumo_rt::{channel_read, channel_write, debug_write, process_exit, startup, vmo_read};

extern crate alloc;

use alloc::vec::Vec;

kumo_rt::entry!(main);

/// Largest stdin message accepted, matching Sora's line buffer.
const MAX_INPUT_BYTES: usize = 256;

/// This process's prompt, appended to every interactive reply.
const PROMPT: &[u8] = b"lua> ";

/// Greeting written before the first stdin read, so the session announces itself.
const BANNER: &[u8] = b"kumo lua (piccolo) - 'exit' to leave\n";

fn emit(stdout: Handle, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if stdout.0 == 0 {
        debug_write(bytes.as_ptr(), bytes.len());
    } else if channel_write(stdout, bytes.as_ptr(), bytes.len()) != 0 {
        const ERR: &[u8] = b"lua-repl: stdout write failed\n";
        debug_write(ERR.as_ptr(), ERR.len());
    }
}

/// True when the launcher asked for an interactive session (`-i` anywhere in argv).
fn wants_interactive(argv: Option<Handle>) -> bool {
    let Some(handle) = argv else {
        return false;
    };
    let mut buf = [0u8; 256];
    if vmo_read(handle, 0, buf.as_mut_ptr(), buf.len()) != 0 {
        return false;
    }
    let mut interactive = false;
    for arg in unpack_argv(&buf) {
        if arg == b"-i" {
            interactive = true;
        }
    }
    interactive
}

/// Trim one submitted line: drop the line terminator and surrounding ASCII space.
fn trim(line: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = line.len();
    while end > start && matches!(line[end - 1], b'\r' | b'\n' | b' ' | b'\t') {
        end -= 1;
    }
    while start < end && matches!(line[start], b' ' | b'\t') {
        start += 1;
    }
    &line[start..end]
}

/// Render one evaluated line into the reply transcript.
///
/// Interactive output reads like a Lua prompt — print output verbatim, the value on its own line,
/// nothing at all for a statement. The one-shot form keeps its `lua-repl: ` prefix so the existing
/// `run lua-repl <expression>` transcript is unchanged.
fn render(reply: &mut Vec<u8>, outcome: &lua_repl::LineOutcome, interactive: bool) {
    reply.extend_from_slice(&outcome.printed);
    match &outcome.error {
        Some(lua_repl::EvalError::InstructionLimit) => {
            reply.extend_from_slice(if interactive {
                b"instruction limit\n"
            } else {
                b"lua-repl: instruction limit\n"
            });
        }
        Some(lua_repl::EvalError::Vm(_)) => {
            reply.extend_from_slice(if interactive {
                b"error\n"
            } else {
                b"lua-repl: evaluation error\n"
            });
        }
        None => {
            if interactive {
                if !outcome.value.is_empty() {
                    reply.extend_from_slice(&outcome.value);
                    if outcome.truncated {
                        reply.extend_from_slice(b"...");
                    }
                    reply.push(b'\n');
                }
            } else {
                reply.extend_from_slice(b"lua-repl: ");
                if outcome.value.is_empty() {
                    reply.extend_from_slice(b"ok");
                } else {
                    reply.extend_from_slice(&outcome.value);
                }
                if outcome.truncated {
                    reply.extend_from_slice(b"...");
                }
                reply.push(b'\n');
            }
        }
    }
}

#[no_mangle]
extern "C" fn main(
    bootstrap_handle: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    kumo_rt::init();
    let startup = startup(Handle(bootstrap_handle as u32));
    let stdin = startup.stdin.unwrap_or(Handle(0));
    let stdout = startup.stdout.unwrap_or(Handle(0));
    let interactive = wants_interactive(startup.argv);

    if stdin.0 == 0 {
        emit(stdout, b"lua-repl: no stdin\n");
        process_exit(1);
    }

    let Ok(mut session) = lua_repl::LuaSession::new() else {
        emit(stdout, b"lua-repl: vm unavailable\n");
        process_exit(1);
    };

    if interactive {
        // Announce before the first read: this reply is the shell's cue that the session is live.
        let mut hello = Vec::with_capacity(BANNER.len() + PROMPT.len());
        hello.extend_from_slice(BANNER);
        hello.extend_from_slice(PROMPT);
        emit(stdout, &hello);
    }

    let mut input = [0u8; MAX_INPUT_BYTES];
    let mut evaluated_any = false;
    loop {
        // Parks until the shell submits a line; returns 0 once the shell closes its writer, which
        // is how a session ends without needing a quit word.
        let read = channel_read(stdin, input.as_mut_ptr(), input.len());
        if read == 0 || read == u64::MAX {
            if !evaluated_any {
                emit(stdout, b"lua-repl: input unavailable\n");
                process_exit(1);
            }
            process_exit(0);
        }

        // One reply per stdin message: the shell reads exactly one message back and knows the line
        // is finished, however many `print` calls it made.
        let mut reply = Vec::new();
        let mut quit = false;
        for line in input[..read as usize].split(|byte| *byte == b'\n') {
            let line = trim(line);
            if line.is_empty() {
                continue;
            }
            if line == b"exit" || line == b"quit" {
                quit = true;
                break;
            }
            evaluated_any = true;
            let outcome = session.eval_line(line);
            render(&mut reply, &outcome, interactive);
        }

        if quit {
            // Say nothing: the closed channel *is* the goodbye, and the shell takes its prompt
            // back the moment its read comes up empty.
            process_exit(0);
        }
        if interactive {
            reply.extend_from_slice(PROMPT);
        }
        emit(stdout, &reply);
        if !evaluated_any && !interactive {
            // A one-shot launch whose only line was blank: say so rather than parking on a stdin
            // that will never carry anything else.
            emit(stdout, b"lua-repl: empty input\n");
            process_exit(1);
        }
    }
}
