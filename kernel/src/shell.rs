//! A tiny Stage-A serial command shell — KUMO's first interactive surface.
//!
//! The command *dispatch* here is pure (host-tested): it takes a whole line plus a
//! snapshot of kernel state and writes output through a `core::fmt::Write` sink. The
//! kernel's serial REPL owns the byte loop / line editing and feeds it submitted
//! lines. This is deliberately minimal — a handful of built-ins over data the kernel
//! already has — and runs in-kernel over the serial console for now. It is the
//! ancestor of Kumoza, which takes over once userspace + a `ttyd` server exist.

use core::fmt::Write;

/// A snapshot of kernel state the built-in commands report on. `uptime_ns` is
/// refreshed by the REPL just before each dispatch.
#[derive(Clone, Copy, Debug)]
pub struct ShellEnv {
    pub arch: &'static str,
    pub abi_version: u32,
    pub usable_frames: u64,
    pub usable_bytes: u64,
    pub total_bytes: u64,
    pub heap_kib: u64,
    pub uptime_ns: u64,
    pub preempt_ticks: u64,
    pub preempt_switches: u64,
}

/// One row of the `ps` table — a kernel object from the task substrate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskInfo {
    pub koid: u64,
    pub kind: &'static str,
    pub state: &'static str,
    pub label: &'static str,
}

pub const PROMPT: &str = "[MUREX>";

const HELP: &str = "commands:\r\n\
     help            this list\r\n\
     ver             kernel identity\r\n\
     mem             memory accounting\r\n\
     ps              task/thread table\r\n\
     ticks           timer scheduler ticks\r\n\
     uptime          time since boot\r\n\
     echo <text>     print text\r\n\
     clear           clear the screen\r\n";

/// Run one command line, writing any output through `out`. An empty/whitespace line
/// produces nothing.
pub fn run_command(line: &str, env: &ShellEnv, tasks: &[TaskInfo], out: &mut dyn Write) {
    let line = line.trim();
    let mut parts = line.split_whitespace();
    let Some(cmd) = parts.next() else {
        return;
    };

    match cmd {
        "help" => {
            let _ = out.write_str(HELP);
        }
        "ver" | "banner" => {
            let _ = write!(
                out,
                "KUMO Ziwei - capability microkernel\r\narch={} abi=v{}\r\n",
                env.arch, env.abi_version
            );
        }
        "mem" => {
            let _ = write!(
                out,
                "usable {} MiB ({} frames) / {} MiB total; heap {} KiB\r\n",
                env.usable_bytes >> 20,
                env.usable_frames,
                env.total_bytes >> 20,
                env.heap_kib
            );
        }
        "ps" => {
            let _ = out.write_str(" koid  kind     state       label\r\n");
            for t in tasks {
                let _ = write!(
                    out,
                    " {:>4}  {:<8} {:<11} {}\r\n",
                    t.koid, t.kind, t.state, t.label
                );
            }
        }
        "ticks" => {
            let _ = write!(
                out,
                "timer scheduler ticks={} switches={}\r\n",
                env.preempt_ticks, env.preempt_switches
            );
        }
        "uptime" => {
            let secs = env.uptime_ns / 1_000_000_000;
            let millis = (env.uptime_ns % 1_000_000_000) / 1_000_000;
            let _ = write!(out, "uptime {}.{:03} s\r\n", secs, millis);
        }
        "echo" => {
            let rest = line.strip_prefix("echo").unwrap_or("").trim_start();
            let _ = write!(out, "{}\r\n", rest);
        }
        "clear" => {
            // ANSI clear + home (this REPL runs on a serial terminal).
            let _ = out.write_str("\x1b[2J\x1b[H");
        }
        other => {
            let _ = write!(out, "unknown command: {} (try 'help')\r\n", other);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;

    fn env() -> ShellEnv {
        ShellEnv {
            arch: "aarch64",
            abi_version: 1,
            usable_frames: 100,
            usable_bytes: 100 << 20,
            total_bytes: 128 << 20,
            heap_kib: 1024,
            uptime_ns: 1_234_000_000,
            preempt_ticks: 7,
            preempt_switches: 3,
        }
    }

    const TASKS: &[TaskInfo] = &[
        TaskInfo {
            koid: 2,
            kind: "process",
            state: "-",
            label: "kernel",
        },
        TaskInfo {
            koid: 3,
            kind: "thread",
            state: "terminated",
            label: "kdemo-a",
        },
    ];

    fn run(line: &str) -> String {
        let mut out = String::new();
        run_command(line, &env(), TASKS, &mut out);
        out
    }

    #[test]
    fn help_lists_builtins() {
        let out = run("help");
        for cmd in [
            "help", "ver", "mem", "ps", "ticks", "uptime", "echo", "clear",
        ] {
            assert!(out.contains(cmd), "help missing '{cmd}'");
        }
    }

    #[test]
    fn ps_lists_tasks() {
        let out = run("ps");
        assert!(out.contains("koid"));
        assert!(out.contains("kernel"));
        assert!(out.contains("kdemo-a"));
        assert!(out.contains("terminated"));
        assert!(out.contains(" 3  ")); // the thread koid in the table
    }

    #[test]
    fn ver_reports_identity() {
        let out = run("ver");
        assert!(out.contains("arch=aarch64"));
        assert!(out.contains("abi=v1"));
    }

    #[test]
    fn mem_reports_accounting() {
        let out = run("mem");
        assert!(out.contains("100 MiB"));
        assert!(out.contains("100 frames"));
        assert!(out.contains("128 MiB total"));
        assert!(out.contains("1024 KiB"));
    }

    #[test]
    fn uptime_formats_seconds_and_millis() {
        assert!(run("uptime").contains("1.234 s"));
    }

    #[test]
    fn ticks_reports_scheduler_tick_counters() {
        let out = run("ticks");
        assert!(out.contains("ticks=7"));
        assert!(out.contains("switches=3"));
    }

    #[test]
    fn echo_repeats_the_rest() {
        assert_eq!(run("echo hi there").trim_end(), "hi there");
        assert_eq!(run("echo").trim_end(), "");
    }

    #[test]
    fn unknown_command_is_reported() {
        assert!(run("frobnicate x").contains("unknown command: frobnicate"));
    }

    #[test]
    fn blank_line_is_silent() {
        assert_eq!(run("   "), "");
        assert_eq!(run(""), "");
    }

    #[test]
    fn leading_whitespace_is_tolerated() {
        assert!(run("   help").contains("commands"));
    }
}
