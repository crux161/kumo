#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

/// A single command: name + whitespace-delimited arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Command {
    pub name: String,
    pub args: Vec<String>,
}

/// A pipeline: `cmd1 | cmd2 | cmd3`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
}

/// I/O redirection target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedirectTarget {
    /// Redirect to/from a file path.
    Path(String),
    /// Redirect to/from a file descriptor number.
    Fd(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redirect {
    pub source: RedirectTarget,
    pub dest: RedirectTarget,
}

/// A complete parsed statement: a pipeline with optional redirects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Statement {
    pub pipeline: Pipeline,
    pub redirects: Vec<Redirect>,
}

/// Tokenize a line into a single `Command`. Splits on ASCII whitespace.
/// Returns `None` for an empty or all-whitespace line.
pub fn tokenize(line: &str) -> Option<Command> {
    let mut words = line.split_ascii_whitespace();
    let name = words.next()?.to_owned();
    let args: Vec<String> = words.map(|w| w.to_owned()).collect();
    Some(Command { name, args })
}

/// Parse a line into a `Statement`. For now, only handles a single command
/// (no pipes, no redirects). Returns `None` for empty input.
pub fn parse(line: &str) -> Option<Statement> {
    let cmd = tokenize(line)?;
    Some(Statement {
        pipeline: Pipeline {
            commands: alloc::vec![cmd],
        },
        redirects: Vec::new(),
    })
}

/// Evaluator scaffold: dispatch a parsed `Statement` to a handler.
/// The handler receives each `Command`; the evaluator owns pipeline/redirect
/// orchestration (not yet implemented).
pub fn evaluate(statement: &Statement, mut handler: impl FnMut(&Command) -> bool) -> bool {
    for cmd in &statement.pipeline.commands {
        if !handler(cmd) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    #[test]
    fn tokenize_empty_line() {
        assert_eq!(tokenize(""), None);
        assert_eq!(tokenize("   "), None);
    }

    #[test]
    fn tokenize_single_word() {
        let cmd = tokenize("ls").unwrap();
        assert_eq!(cmd.name, "ls");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn tokenize_command_with_args() {
        let cmd = tokenize("echo hello world").unwrap();
        assert_eq!(cmd.name, "echo");
        assert_eq!(cmd.args, alloc::vec!["hello", "world"]);
    }

    #[test]
    fn tokenize_extra_whitespace() {
        let cmd = tokenize("  grep   -rn  foo  ").unwrap();
        assert_eq!(cmd.name, "grep");
        assert_eq!(cmd.args, alloc::vec!["-rn", "foo"]);
    }

    #[test]
    fn parse_produces_statement() {
        let stmt = parse("cat file.txt").unwrap();
        assert_eq!(stmt.pipeline.commands.len(), 1);
        assert_eq!(stmt.pipeline.commands[0].name, "cat");
        assert!(stmt.redirects.is_empty());
    }

    #[test]
    fn evaluate_runs_all_commands() {
        let stmt = parse("cmd1 arg1").unwrap();
        let mut seen = Vec::new();
        let ok = evaluate(&stmt, |cmd| {
            seen.push((cmd.name.clone(), cmd.args.clone()));
            true
        });
        assert!(ok);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "cmd1");
    }

    #[test]
    fn evaluate_stops_on_false() {
        let stmt = parse("fail").unwrap();
        let mut count = 0;
        let ok = evaluate(&stmt, |_cmd| {
            count += 1;
            false
        });
        assert!(!ok);
        assert_eq!(count, 1);
    }
}
