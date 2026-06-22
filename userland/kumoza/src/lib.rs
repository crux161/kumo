#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

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

/// Help text for the current Kumoza builtin scaffold.
pub const HELP_TEXT: &[u8] = b"KUMO Sora userspace shell (scaffold)\n\
    builtins: cat <path>, echo [-n], false, help, ls, run <program>, true, wc <path>\n\
    other commands run via kernel shell\n";

/// Tokenize a line into a single `Command`. Splits on ASCII whitespace, except inside
/// single (`'`) or double (`"`) quotes, where whitespace is literal and the quote marks
/// are stripped. Outside quotes a backslash escapes the next character (`a\ b` → `a b`,
/// `\"` → a literal `"`); inside quotes a backslash is itself literal. Adjacent quoted and
/// unquoted runs concatenate into one word (`a"b c"d` → `ab cd`); empty quotes (`""`) yield
/// an empty argument; an unterminated quote is closed at end of line. Returns `None` for an
/// empty or all-whitespace line.
pub fn tokenize(line: &str) -> Option<Command> {
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        // Outside quotes, a backslash escapes the next character (it becomes literal word
        // content); inside quotes the backslash is itself literal (handled by the `Some` arm).
        if escaped {
            word.push(c);
            in_word = true;
            escaped = false;
            continue;
        }
        match quote {
            // Inside quotes: the closing quote ends the span, everything else is literal.
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    word.push(c);
                }
            }
            None if c == '\\' => escaped = true,
            // A quote opens a span and starts a word even if the span is empty.
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            // Unquoted whitespace closes the current word (if any).
            None if c.is_ascii_whitespace() => {
                if in_word {
                    words.push(core::mem::take(&mut word));
                    in_word = false;
                }
            }
            None => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    let mut words = words.into_iter();
    let name = words.next()?;
    let args: Vec<String> = words.collect();
    Some(Command { name, args })
}

/// Iterate the command lines in an **autoexec manifest** — the input-less boot
/// script Sora runs from the initrd (`etc/autoexec`). Each yielded line is a shell
/// command (`run <prog>`, `echo …`, …), dispatched by the same evaluator as
/// interactive input, so the manifest speaks the shell's own syntax.
///
/// Lines are ASCII-whitespace-trimmed (so a trailing `\r` from CRLF is dropped);
/// blank lines and `#` comment lines are skipped. The yielded slices borrow from
/// `manifest`, so this is `no_std` and allocation-free — it runs in Sora directly.
pub fn autoexec_lines(manifest: &[u8]) -> impl Iterator<Item = &[u8]> {
    manifest
        .split(|&b| b == b'\n')
        .map(|line| line.trim_ascii())
        .filter(|line| !line.is_empty() && line[0] != b'#')
}

/// Parse a line into a `Statement`: a pipeline of one or more commands split on each
/// top-level `|`, each command with its `<`/`>` redirects lifted out into the statement's
/// redirect list. A `|`, `<`, or `>` inside single/double quotes is literal. Returns `None`
/// when the line holds no command at all (empty, whitespace, or only separators); empty
/// segments such as a trailing `|` are skipped rather than treated as a syntax error.
///
/// Redirect convention — `source` → `dest` follows the data flow: `n> path` is
/// `Fd(n)` → `Path` (default `n` = 1), `n< path` is `Path` → `Fd(n)` (default `n` = 0), and
/// `n>&m` is `Fd(n)` → `Fd(m)`. Redirects are parsed into the structure but not yet *applied*
/// (no execution model), just like a multi-stage pipeline.
pub fn parse(line: &str) -> Option<Statement> {
    let mut commands = Vec::new();
    let mut redirects = Vec::new();
    for segment in split_pipeline(line) {
        if let Some((cmd, mut segment_redirects)) = parse_segment(segment) {
            commands.push(cmd);
            redirects.append(&mut segment_redirects);
        }
    }
    if commands.is_empty() {
        return None;
    }
    Some(Statement {
        pipeline: Pipeline { commands },
        redirects,
    })
}

/// Split a line into pipeline segments on each top-level `|`. A `|` inside single (`'`) or
/// double (`"`) quotes, or escaped with a backslash, is literal and does not split. Segments
/// are returned in order and borrow from `line`; `lex_segment` strips and unescapes each.
fn split_pipeline(line: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        // An escaped char is skipped here (kept literal); `lex_segment` removes the backslash.
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None if c == '\\' => escaped = true,
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '|' => {
                segments.push(&line[start..i]);
                start = i + 1; // `|` is one ASCII byte
            }
            None => {}
        }
    }
    segments.push(&line[start..]);
    segments
}

/// One lexed token within a pipeline segment: a word, or a redirect operator with its
/// optional leading file-descriptor (`2>`) and direction (`>` output vs `<` input).
enum Tok {
    Word(String),
    Redir { fd: Option<u32>, output: bool },
}

/// Lex a pipeline segment into words and redirect operators, honouring single/double quotes
/// (a quoted `<`/`>` is literal). A run of digits immediately before an unquoted operator is
/// its fd prefix (`2>` → fd 2); otherwise the operator carries no fd. Repeated operators
/// (`>>`) lex as separate tokens — the segment parser keeps only the last before a target,
/// since the current `Redirect` type has no append flag.
fn lex_segment(segment: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in segment.chars() {
        // Outside quotes a backslash escapes the next char into literal word content (so
        // `\<`, `\>`, `\|` are not operators); inside quotes the backslash is itself literal.
        if escaped {
            word.push(c);
            in_word = true;
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    word.push(c);
                }
            }
            None if c == '\\' => escaped = true,
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c == '<' || c == '>' => {
                // A bare-digit word touching the operator is its fd; anything else is a word.
                let fd = if in_word && word.bytes().all(|b| b.is_ascii_digit()) {
                    let parsed = word.parse::<u32>().ok();
                    word.clear();
                    parsed
                } else {
                    if in_word {
                        toks.push(Tok::Word(core::mem::take(&mut word)));
                    }
                    None
                };
                in_word = false;
                toks.push(Tok::Redir {
                    fd,
                    output: c == '>',
                });
            }
            None if c.is_ascii_whitespace() => {
                if in_word {
                    toks.push(Tok::Word(core::mem::take(&mut word)));
                    in_word = false;
                }
            }
            None => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        toks.push(Tok::Word(word));
    }
    toks
}

/// Resolve a redirect target word: `&N` is a file-descriptor dup (`>&1`), otherwise a path.
fn redirect_target(word: String) -> RedirectTarget {
    if let Some(digits) = word.strip_prefix('&') {
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(fd) = digits.parse::<u32>() {
                return RedirectTarget::Fd(fd);
            }
        }
    }
    RedirectTarget::Path(word)
}

/// Parse one pipeline segment into its `Command` and the redirects lifted out of it. The word
/// following a redirect operator is that redirect's target, not a command argument. Returns
/// `None` when the segment has no command word (e.g. it was empty or only a redirect).
fn parse_segment(segment: &str) -> Option<(Command, Vec<Redirect>)> {
    let mut words: Vec<String> = Vec::new();
    let mut redirects: Vec<Redirect> = Vec::new();
    let mut pending: Option<(Option<u32>, bool)> = None;
    for tok in lex_segment(segment) {
        match tok {
            // A new operator before the previous one got a target drops the previous
            // (this is how `>>` collapses to a single output redirect).
            Tok::Redir { fd, output } => pending = Some((fd, output)),
            Tok::Word(w) => match pending.take() {
                Some((fd, true)) => redirects.push(Redirect {
                    source: RedirectTarget::Fd(fd.unwrap_or(1)),
                    dest: redirect_target(w),
                }),
                Some((fd, false)) => redirects.push(Redirect {
                    source: redirect_target(w),
                    dest: RedirectTarget::Fd(fd.unwrap_or(0)),
                }),
                None => words.push(w),
            },
        }
    }
    let mut words = words.into_iter();
    let name = words.next()?;
    let args: Vec<String> = words.collect();
    Some((Command { name, args }, redirects))
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

/// Render output-only builtins. Returns `true` when `cmd` was handled.
///
/// Sora still owns authority-bearing effects such as `ls`, `run`, and forwarding
/// unknown commands. This helper is the first pure shell-owned output path so a
/// future standalone Kumoza process can reuse the exact same builtin behavior.
pub fn write_builtin_output(cmd: &Command, mut write: impl FnMut(&[u8])) -> bool {
    if cmd.name == "echo" {
        // `echo -n` suppresses the trailing newline; the flag itself is not printed.
        let suppress_newline = cmd.args.first().map(String::as_str) == Some("-n");
        let args = if suppress_newline {
            &cmd.args[1..]
        } else {
            &cmd.args[..]
        };
        for (index, arg) in args.iter().enumerate() {
            if index > 0 {
                write(b" ");
            }
            write(arg.as_bytes());
        }
        if !suppress_newline {
            write(b"\n");
        }
        true
    } else if cmd.name == "help" {
        write(HELP_TEXT);
        true
    } else if cmd.name == "true" || cmd.name == "false" {
        // Recognised no-output builtins. Their exit status is not observable yet (no `&&`,
        // `||`, or `$?`), so both are simply consumed here rather than forwarded as unknown.
        true
    } else {
        false
    }
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
    fn tokenize_run_program_command() {
        // The exec-vertical `run <program>` builtin: name "run", first arg = program.
        let cmd = tokenize("run hello").unwrap();
        assert_eq!(cmd.name, "run");
        assert_eq!(cmd.args.first().map(String::as_str), Some("hello"));
    }

    #[test]
    fn tokenize_double_quotes_keep_internal_spaces() {
        let cmd = tokenize("echo \"hello world\"").unwrap();
        assert_eq!(cmd.name, "echo");
        assert_eq!(cmd.args, alloc::vec!["hello world"]);
    }

    #[test]
    fn tokenize_single_quotes_keep_internal_spaces() {
        let cmd = tokenize("run 'my program' now").unwrap();
        assert_eq!(cmd.name, "run");
        assert_eq!(cmd.args, alloc::vec!["my program", "now"]);
    }

    #[test]
    fn tokenize_quotes_concatenate_adjacent_runs() {
        // `a"b c"d` is one word with the quote marks stripped: `ab cd`.
        let cmd = tokenize("echo a\"b c\"d").unwrap();
        assert_eq!(cmd.args, alloc::vec!["ab cd"]);
    }

    #[test]
    fn tokenize_empty_quotes_yield_empty_arg() {
        let cmd = tokenize("echo \"\"").unwrap();
        assert_eq!(cmd.name, "echo");
        assert_eq!(cmd.args, alloc::vec![""]);
    }

    #[test]
    fn tokenize_unterminated_quote_closes_at_eol() {
        let cmd = tokenize("echo \"hi there").unwrap();
        assert_eq!(cmd.args, alloc::vec!["hi there"]);
    }

    #[test]
    fn tokenize_backslash_escapes_space_and_quote() {
        // `a\ b` is one argument; `\"` is a literal quote, not a span opener.
        assert_eq!(tokenize("echo a\\ b").unwrap().args, alloc::vec!["a b"]);
        assert_eq!(tokenize("echo \\\"").unwrap().args, alloc::vec!["\""]);
    }

    #[test]
    fn tokenize_backslash_inside_quotes_is_literal() {
        // A documented simplification: inside quotes the backslash does not escape.
        assert_eq!(tokenize("echo 'a\\b'").unwrap().args, alloc::vec!["a\\b"]);
    }

    #[test]
    fn parse_backslash_escapes_pipe_and_redirect() {
        // `\|` and `\>` are literal — neither splits a pipeline nor opens a redirect.
        let piped = parse("echo a\\|b").unwrap();
        assert_eq!(piped.pipeline.commands.len(), 1);
        assert_eq!(piped.pipeline.commands[0].args, alloc::vec!["a|b"]);

        let redir = parse("echo a\\>b").unwrap();
        assert_eq!(redir.pipeline.commands[0].args, alloc::vec!["a>b"]);
        assert!(redir.redirects.is_empty());
    }

    #[test]
    fn autoexec_empty_manifest_has_no_lines() {
        assert_eq!(autoexec_lines(b"").count(), 0);
        assert_eq!(autoexec_lines(b"\n\n  \n").count(), 0);
    }

    #[test]
    fn autoexec_yields_command_lines_skipping_comments_and_blanks() {
        let manifest = b"# KUMO autoexec\nrun hello\n\n# again\nrun hello\necho hi\n";
        let lines: Vec<&[u8]> = autoexec_lines(manifest).collect();
        assert_eq!(
            lines,
            alloc::vec![&b"run hello"[..], &b"run hello"[..], &b"echo hi"[..]]
        );
    }

    #[test]
    fn autoexec_trims_whitespace_and_crlf() {
        // CRLF line endings and surrounding spaces must not leak into the command.
        let manifest = b"  run hello  \r\n\t# comment\r\necho hi\r\n";
        let lines: Vec<&[u8]> = autoexec_lines(manifest).collect();
        assert_eq!(lines, alloc::vec![&b"run hello"[..], &b"echo hi"[..]]);
    }

    #[test]
    fn parse_produces_statement() {
        let stmt = parse("cat file.txt").unwrap();
        assert_eq!(stmt.pipeline.commands.len(), 1);
        assert_eq!(stmt.pipeline.commands[0].name, "cat");
        assert!(stmt.redirects.is_empty());
    }

    #[test]
    fn parse_splits_pipeline_into_stages() {
        let stmt = parse("ls | grep foo | wc").unwrap();
        let names: Vec<&str> = stmt
            .pipeline
            .commands
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, alloc::vec!["ls", "grep", "wc"]);
        assert_eq!(stmt.pipeline.commands[1].args, alloc::vec!["foo"]);
    }

    #[test]
    fn parse_pipe_inside_quotes_is_literal() {
        let stmt = parse("echo \"a | b\"").unwrap();
        assert_eq!(stmt.pipeline.commands.len(), 1);
        assert_eq!(stmt.pipeline.commands[0].args, alloc::vec!["a | b"]);
    }

    #[test]
    fn parse_skips_empty_pipeline_segments() {
        // A trailing `|` leaves one executable stage — not (yet) a syntax error.
        let stmt = parse("ls |").unwrap();
        assert_eq!(stmt.pipeline.commands.len(), 1);
        assert_eq!(stmt.pipeline.commands[0].name, "ls");
    }

    #[test]
    fn parse_no_command_is_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
        assert_eq!(parse(" | "), None);
    }

    #[test]
    fn parse_output_redirect_to_path() {
        let stmt = parse("echo hi > out.txt").unwrap();
        assert_eq!(stmt.pipeline.commands.len(), 1);
        assert_eq!(stmt.pipeline.commands[0].name, "echo");
        assert_eq!(stmt.pipeline.commands[0].args, alloc::vec!["hi"]);
        assert_eq!(
            stmt.redirects,
            alloc::vec![Redirect {
                source: RedirectTarget::Fd(1),
                dest: RedirectTarget::Path("out.txt".into()),
            }]
        );
    }

    #[test]
    fn parse_input_redirect_from_path() {
        let stmt = parse("sort < in.txt").unwrap();
        assert_eq!(stmt.pipeline.commands[0].name, "sort");
        assert!(stmt.pipeline.commands[0].args.is_empty());
        assert_eq!(
            stmt.redirects,
            alloc::vec![Redirect {
                source: RedirectTarget::Path("in.txt".into()),
                dest: RedirectTarget::Fd(0),
            }]
        );
    }

    #[test]
    fn parse_fd_prefixed_and_attached_redirect() {
        // `2>err` — explicit fd, no space between operator and target.
        let stmt = parse("cmd 2>err").unwrap();
        assert_eq!(stmt.pipeline.commands[0].name, "cmd");
        assert_eq!(
            stmt.redirects,
            alloc::vec![Redirect {
                source: RedirectTarget::Fd(2),
                dest: RedirectTarget::Path("err".into()),
            }]
        );
    }

    #[test]
    fn parse_fd_dup_redirect() {
        let stmt = parse("cmd 2>&1").unwrap();
        assert_eq!(stmt.pipeline.commands[0].name, "cmd");
        assert_eq!(
            stmt.redirects,
            alloc::vec![Redirect {
                source: RedirectTarget::Fd(2),
                dest: RedirectTarget::Fd(1),
            }]
        );
    }

    #[test]
    fn parse_append_collapses_to_single_output_redirect() {
        // `>>` has no append flag in the current `Redirect` type, so it parses as one output
        // redirect (truncate vs append is not yet distinguished).
        let stmt = parse("echo hi >> log").unwrap();
        assert_eq!(
            stmt.redirects,
            alloc::vec![Redirect {
                source: RedirectTarget::Fd(1),
                dest: RedirectTarget::Path("log".into()),
            }]
        );
    }

    #[test]
    fn parse_quoted_redirect_operator_is_literal() {
        let stmt = parse("echo \">\"").unwrap();
        assert_eq!(stmt.pipeline.commands[0].args, alloc::vec![">"]);
        assert!(stmt.redirects.is_empty());
    }

    #[test]
    fn parse_redirect_within_a_pipeline_stage() {
        let stmt = parse("ls | sort > out").unwrap();
        assert_eq!(stmt.pipeline.commands.len(), 2);
        assert_eq!(stmt.pipeline.commands[1].name, "sort");
        assert_eq!(
            stmt.redirects,
            alloc::vec![Redirect {
                source: RedirectTarget::Fd(1),
                dest: RedirectTarget::Path("out".into()),
            }]
        );
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

    #[test]
    fn write_builtin_output_echoes_args() {
        let cmd = tokenize("echo hello little cloud").unwrap();
        let mut out = Vec::new();

        assert!(write_builtin_output(&cmd, |bytes| out.extend_from_slice(bytes)));

        assert_eq!(out, b"hello little cloud\n");
    }

    #[test]
    fn write_builtin_output_echoes_blank_line() {
        let cmd = tokenize("echo").unwrap();
        let mut out = Vec::new();

        assert!(write_builtin_output(&cmd, |bytes| out.extend_from_slice(bytes)));

        assert_eq!(out, b"\n");
    }

    #[test]
    fn write_builtin_output_prints_help() {
        let cmd = tokenize("help").unwrap();
        let mut out = Vec::new();

        assert!(write_builtin_output(&cmd, |bytes| out.extend_from_slice(bytes)));

        assert_eq!(out, HELP_TEXT);
    }

    #[test]
    fn write_builtin_output_leaves_effectful_commands_to_sora() {
        let cmd = tokenize("run hello").unwrap();
        let mut out = Vec::new();

        assert!(!write_builtin_output(&cmd, |bytes| out.extend_from_slice(bytes)));

        assert!(out.is_empty());
    }

    #[test]
    fn write_builtin_output_echo_dash_n_suppresses_newline() {
        let cmd = tokenize("echo -n hi there").unwrap();
        let mut out = Vec::new();

        assert!(write_builtin_output(&cmd, |bytes| out.extend_from_slice(bytes)));

        assert_eq!(out, b"hi there");
    }

    #[test]
    fn write_builtin_output_echo_dash_n_alone_emits_nothing() {
        let cmd = tokenize("echo -n").unwrap();
        let mut out = Vec::new();

        assert!(write_builtin_output(&cmd, |bytes| out.extend_from_slice(bytes)));

        assert!(out.is_empty());
    }

    #[test]
    fn write_builtin_output_true_and_false_handled_without_output() {
        for name in ["true", "false"] {
            let cmd = tokenize(name).unwrap();
            let mut out = Vec::new();

            assert!(
                write_builtin_output(&cmd, |bytes| out.extend_from_slice(bytes)),
                "{name} should be a handled builtin"
            );
            assert!(out.is_empty(), "{name} should produce no output");
        }
    }
}
