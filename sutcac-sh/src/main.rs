//! sutcac-sh: a simplified bash-compatible shell.

use std::borrow::Cow;
use std::io::{self, Write};

use rustyline::completion::{self, Completer, FilenameCompleter};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::validate::Validator;
use rustyline::{Editor, Helper};
use sutcac_sh::config::ShellConfig;
use sutcac_sh::exec::{CommandOutput, ShellState, execute_command};
use sutcac_sh::parser::Parser;

/// Rustyline helper that provides filename completion and history-based hints.
struct ShellHelper {
    completer: FilenameCompleter,
    hinter: HistoryHinter,
}

impl ShellHelper {
    fn new() -> Self {
        Self {
            completer: FilenameCompleter::new(),
            hinter: HistoryHinter::new(),
        }
    }
}

impl Completer for ShellHelper {
    type Candidate = completion::Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        self.completer.complete(line, pos, ctx)
    }
}

impl Highlighter for ShellHelper {
    /// Render history hints in a dim, italic style so they are clearly
    /// distinguishable from text the user has actually typed.
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("\x1b[2;3m{}\x1b[0m", hint))
    }
}

impl Hinter for ShellHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, ctx: &rustyline::Context<'_>) -> Option<Self::Hint> {
        self.hinter.hint(line, pos, ctx)
    }
}

impl Validator for ShellHelper {}
impl Helper for ShellHelper {}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let (permissions, audit_logger) = match ShellConfig::load() {
        Ok(Some(cfg)) => (cfg.permission_policy(), cfg.audit_logger()),
        Ok(None) => {
            let cfg = ShellConfig::default();
            (cfg.permission_policy(), cfg.audit_logger())
        }
        Err(e) => {
            eprintln!("sutcac-sh: failed to load config: {}", e);
            let cfg = ShellConfig::default();
            (cfg.permission_policy(), cfg.audit_logger())
        }
    };

    let mut state = ShellState::with_policy_and_logger(permissions, audit_logger);

    if args.len() > 2 && args[1] == "-c" {
        let command = &args[2];
        run_commands(command, &mut state);
        std::process::exit(state.last_status);
    }

    if args.len() > 1 {
        let path = &args[1];
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                run_commands(&contents, &mut state);
                std::process::exit(state.last_status);
            }
            Err(e) => {
                eprintln!("sutcac-sh: {}: {}", path, e);
                std::process::exit(126);
            }
        }
    }

    // Interactive mode.
    if let Err(e) = run_interactive(&mut state) {
        eprintln!("sutcac-sh: {}", e);
    }
}

fn run_interactive(state: &mut ShellState) -> Result<(), Box<dyn std::error::Error>> {
    let mut rl = Editor::<ShellHelper, rustyline::history::DefaultHistory>::new()?;
    rl.set_helper(Some(ShellHelper::new()));

    let history_path = dirs::data_dir().map(|d| d.join("sutcac-sh").join("history"));

    if let Some(ref p) = history_path {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = rl.load_history(p);
    }

    let mut pending = String::new();
    loop {
        let ps1 = state
            .vars
            .get("PS1")
            .cloned()
            .unwrap_or_else(|| "sutcac-sh$ ".to_string());
        let ps2 = state
            .vars
            .get("PS2")
            .cloned()
            .unwrap_or_else(|| "> ".to_string());

        let prompt = if pending.is_empty() { &ps1 } else { &ps2 };

        match rl.readline(prompt) {
            Ok(line) => {
                pending.push_str(&line);
                pending.push('\n');

                match try_parse(&pending) {
                    ParseResult::Ok(cmds) => {
                        add_complete_history(&mut rl, &pending);
                        pending.clear();
                        for cmd in cmds {
                            let output = execute_command(&cmd, state);
                            print_output(&output);
                        }
                    }
                    ParseResult::Incomplete => {
                        // Keep pending and show PS2 next iteration.
                    }
                    ParseResult::Err(e) => {
                        eprintln!("sutcac-sh: {}", e);
                        add_complete_history(&mut rl, &pending);
                        pending.clear();
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                pending.clear();
                println!("^C");
            }
            Err(ReadlineError::Eof) => {
                if !pending.is_empty() {
                    eprintln!("sutcac-sh: unexpected EOF");
                }
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }

    if let Some(ref p) = history_path {
        let _ = rl.save_history(p);
    }

    Ok(())
}

enum ParseResult {
    Ok(Vec<sutcac_sh::ast::Command>),
    Incomplete,
    Err(String),
}

/// Return true if the input contains an unclosed single or double quote.
/// This is used to trigger multi-line input mode before the lexer/parser
/// sees the incomplete token as a regular word.
fn has_unmatched_quotes(input: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;

    for c in input.chars() {
        if escape {
            escape = false;
            continue;
        }
        match c {
            '\\' if in_double => escape = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {}
        }
    }

    in_single || in_double
}

fn add_complete_history<H, I>(rl: &mut Editor<H, I>, input: &str)
where
    H: Helper,
    I: rustyline::history::History,
{
    let full = input.trim_end_matches('\n');
    if !full.is_empty() {
        let _ = rl.add_history_entry(full);
    }
}

fn try_parse(input: &str) -> ParseResult {
    if has_unmatched_quotes(input) {
        return ParseResult::Incomplete;
    }

    match Parser::new(input) {
        Ok(mut parser) => match parser.parse() {
            Ok(cmds) => ParseResult::Ok(cmds),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("expected") && (msg.contains("EOF") || msg.contains("Eof")) {
                    ParseResult::Incomplete
                } else {
                    ParseResult::Err(msg)
                }
            }
        },
        Err(e) => ParseResult::Err(e.to_string()),
    }
}

fn print_output(output: &CommandOutput) {
    if !output.stdout.is_empty() {
        print!("{}", output.stdout);
        let _ = io::stdout().flush();
    }
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
        let _ = io::stderr().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmatched_double_quote_triggers_incomplete() {
        assert!(matches!(try_parse("echo \"hello"), ParseResult::Incomplete));
    }

    #[test]
    fn unmatched_single_quote_triggers_incomplete() {
        assert!(matches!(try_parse("echo 'hello"), ParseResult::Incomplete));
    }

    #[test]
    fn matched_quotes_parse_ok() {
        assert!(matches!(try_parse("echo \"hello\""), ParseResult::Ok(_)));
    }

    #[test]
    fn escaped_quote_does_not_trigger_incomplete() {
        assert!(matches!(
            try_parse("echo \"hello\\\"\""),
            ParseResult::Ok(_)
        ));
    }
}

fn run_commands(input: &str, state: &mut ShellState) {
    match Parser::new(input) {
        Ok(mut parser) => match parser.parse() {
            Ok(cmds) => {
                for cmd in cmds {
                    let output = execute_command(&cmd, state);
                    print_output(&output);
                }
            }
            Err(e) => {
                eprintln!("sutcac-sh: {}", e);
                state.last_status = 2;
            }
        },
        Err(e) => {
            eprintln!("sutcac-sh: {}", e);
            state.last_status = 2;
        }
    }
}
