# AGENTS.md — sutcac project guide

This file is written for AI coding agents that need to understand and modify the project. Everything below reflects the actual code and configuration in this repository.

## Project overview

`sutcac` is a Rust workspace containing:

- `sutcac-sh`: a simplified Bash-compatible shell interpreter.
- `catus`: an Agent tool prototype that talks to OpenAI-compatible chat-completion APIs via `reqwest`, renders a `ratatui` TUI, and uses `sutcac-sh` as its shell tool.

It is a learning/implementation project: the shell tokenises input by hand, parses it into an AST with a recursive-descent parser, expands shell words (parameters, tilde, arithmetic, globs), and executes commands including builtins, external programs, functions, pipelines, and compound statements.

Key facts:

- Workspace root: `/home/undatus63/sutcac`
- Crates: `sutcac-sh` (shell library/binary), `catus` (Agent TUI binary)
- Rust edition: 2024 (requires Rust 1.85+ / Cargo that supports edition 2024)
- Supporting document: `bash.md` (Chinese) explains Bash internals and likely served as the design reference.

## Project structure

```text
/home/undatus63/sutcac
├── Cargo.toml           # Workspace manifest, declares sutcac-sh and catus members
├── Cargo.lock           # Dependency lock file
├── bash.md              # Human-readable Bash execution flow reference (Chinese)
├── AGENTS.md            # This file
├── .sutcac
│   └── config.toml.example   # Example shared TOML config (copy to config.toml)
├── sutcac-sh
│   ├── Cargo.toml       # Package manifest for sutcac-sh
│   ├── .gitignore       # Ignores /target
│   └── src
│       ├── main.rs      # CLI binary: -c, script file, interactive REPL
│       ├── lib.rs       # Public module re-exports
│       ├── config.rs    # TOML config loader for the [shell] section
│       ├── lexer.rs     # Hand-written shell lexer
│       ├── parser.rs    # Recursive-descent parser
│       ├── ast.rs       # Abstract syntax tree definitions
│       ├── expand.rs    # Word expansion (quotes, $var, $((...)), globs, etc.)
│       ├── glob.rs      # Glob matching and pathname expansion
│       ├── arith.rs     # Integer arithmetic parser/evaluator
│       ├── builtin.rs   # Shell builtins
│       └── exec.rs      # Command execution engine
└── catus
    ├── Cargo.toml       # Package manifest for catus
    ├── .gitignore       # Ignores /target
    └── src
        ├── main.rs      # TUI event loop and async orchestration
        ├── lib.rs       # Public module re-exports
        ├── app.rs       # Application state and Q-A/tool logic
        ├── config.rs    # Full TOML config loader
        ├── llm.rs       # OpenAI-compatible streaming/non-streaming client
        ├── message.rs   # Conversation message types
        ├── tool.rs      # Shell tool parsing and execution
        └── tui.rs       # ratatui layout and terminal helpers
```

## Technology stack

- **Language:** Rust (edition 2024)
- **Build tool:** Cargo
- **Testing:** Cargo built-in unit tests (`#[cfg(test)]` modules inside each source file)
- **External crates:**
  - `sutcac-sh`: `serde`, `toml`, `dirs`
  - `catus`: `reqwest`, `tokio`, `eventsource-stream`, `futures`, `ratatui`, `crossterm`, `serde`, `serde_json`, `toml`, `dirs`, `unicode-width`, plus `sutcac-sh` (path dependency)
- No framework-specific tooling, no CI/deployment configuration present.

## Configuration

Both `sutcac-sh` and `catus` read a single shared TOML configuration file:

1. `./.sutcac/config.toml` (current working directory) — preferred.
2. `$XDG_CONFIG_HOME/catus/config.toml` — fallback.
3. `~/.config/catus/config.toml` — final fallback.

Create the file from the provided example:

```bash
cp .sutcac/config.toml.example .sutcac/config.toml
# edit .sutcac/config.toml and set api.api_key
```

### Config schema

```toml
[api]
base_url = "https://api.openai.com/v1"
api_key = "sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
model = "gpt-4o-mini"

[agent]
system_prompt = "You are a helpful assistant ..."
max_tool_rounds = 30
history_path = ".sutcac/history"  # optional directory for JSON history files
log_path = ".sutcac/catus.log"    # optional; defaults to .sutcac/catus.log
log_level = "info"                # optional; trace | debug | info | warn | error

[shell]
perm_mode = "allow_all"        # allow_all | allow:<tags> | deny:<tags> | allow:<tags> deny:<tags> | allow1:<cmds> | deny1:<cmds>
audit_log = "/tmp/sutcac-audit.log"
audit_format = "text"          # text | json

# Optional structured metadata appended to every audit log entry.
[shell.audit_meta]
session = "example-session"

# Per-command permission tags. Any external command not listed here defaults
# to READ+WRITE. Use this section to mark commands as READ-only or assign
# custom tags such as `network`.
[shell.commands]
awk = ["read"]
basename = ["read"]
cat = ["read"]
cmp = ["read"]
comm = ["read"]
cut = ["read"]
diff = ["read"]
dirname = ["read"]
file = ["read"]
find = ["read"]
git = ["read"]
grep = ["read"]
head = ["read"]
less = ["read"]
ls = ["read"]
more = ["read"]
nl = ["read"]
printf = ["read"]
ps = ["read"]
pwd = ["read"]
readlink = ["read"]
realpath = ["read"]
sed = ["read"]
sort = ["read"]
strings = ["read"]
tail = ["read"]
tr = ["read"]
uniq = ["read"]
wc = ["read"]
whereis = ["read"]
which = ["read"]
xargs = ["read"]

# Custom-tag examples:
curl = ["network", "read"]
```

- `[api]` and `[agent]` are used only by `catus`.
- `[shell]` is used by both `catus` (when invoking the embedded shell) and `sutcac-sh` (when run standalone).
- `perm_mode` supports arbitrary custom tags (for example `allow:read,network`).
  `allow` and `deny` clauses can be combined: the required permission set must be
  covered by the allow set and must not intersect the deny set.  `allow1:<cmds>`
  and `deny1:<cmds>` override everything else for individual commands (`deny1`
  wins over `allow1`).
- `api_key` is stored in plain text in this prototype.

## Build and run

All commands are run from the workspace root (`/home/undatus63/sutcac`).

```bash
# Build the whole workspace
cargo build

# Build release binary
cargo build --release

# Run the Agent TUI
cargo run -p catus

# Run a one-shot test prompt without the TUI (useful for CI/headless checks)
cargo run -p catus -- --test "List the files in the current directory"

# Run the interactive shell
cargo run -p sutcac-sh

# Execute a single command with the shell
cargo run -p sutcac-sh -- -c "echo hello"

# Run a script file with the shell
cargo run -p sutcac-sh -- path/to/script.sh
```

### catus TUI controls

- `Enter` — send the current input to the Agent.
- `Esc` / `Ctrl+C` — quit.
- `Up` / `Down` / `PageUp` / `PageDown` — scroll the conversation history.
- `Home` — jump to the top of the history.
- `End` — scroll to the bottom of the history.
- `/resume` — list available saved histories.
- `/resume <name>` — load `<history_dir>/<name>.json` and continue that conversation.

The Agent streams the assistant reply. The assistant may invoke the `shell` tool using the standard OpenAI-compatible function-calling protocol (a `tool_calls` entry with `function.name == "shell"` and `arguments.command`). The command is executed through `sutcac-sh`, the output is returned to the model as a `tool` role message, and the assistant is called again (up to `max_tool_rounds` times).

History display conventions:

- `YOU:` user messages are highlighted (bold blue).
- `AI:` final assistant answers are highlighted (bold green).
- Intermediate assistant messages that triggered tool calls are dimmed.
- Shell tool results are shown as a green (`●`) or red (`●`) dot followed by the truncated stdout (20 lines max); red means a non-zero exit status.
- When `agent.history_path` is set to a directory, catus saves the current session to a timestamped JSON file in that directory on exit. Previous histories are not loaded automatically; use `/resume <name>` to load `<history_dir>/<name>.json`.
- Recoverable LLM/API errors are shown as dim event messages in the history area; fatal errors are logged to `log_path` and exit after restoring the terminal.

The `sutcac-sh` binary supports three invocation modes:

1. `sutcac-sh -c "commands"` — runs the command string and exits with the last command status.
2. `sutcac-sh file.sh` — reads the file and executes it.
3. `sutcac-sh` — interactive mode powered by `rustyline`: line editing (Emacs key bindings), up/down history navigation, Tab filename completion, and persistent history saved to `~/.local/share/sutcac-sh/history` (or the platform-equivalent data directory). `PS1` / `PS2` prompts are still honoured.

## Testing

Tests are embedded in each source module and run with:

```bash
cargo test --workspace
```

Current test coverage (all passing):

- `arith::tests` — arithmetic evaluation
- `builtin::tests` — echo and test builtins
- `exec::tests` — executing `true`/`false`
- `expand::tests` — parameter expansion, quoting, word splitting, arithmetic expansion
- `glob::tests` — glob pattern matching
- `lexer::tests` — tokenisation
- `parser::tests` — parsing various command constructs
- `catus::config::tests` — TOML config parsing
- `catus::tool::tests` — tool-call extraction

There are no integration tests, benchmarks, or doctest examples in the current code.

## Code organisation

### Library (`sutcac-sh/src/lib.rs`)

`lib.rs` re-exports every module so the binary and `catus` can use `sutcac_sh::*`:

```rust
pub mod arith;
pub mod ast;
pub mod audit;
pub mod builtin;
pub mod config;
pub mod exec;
pub mod expand;
pub mod glob;
pub mod lexer;
pub mod parser;
pub mod permissions;
```

### Shell binary (`sutcac-sh/src/main.rs`)

The binary handles I/O, CLI argument parsing, the interactive read-print loop, and top-level parse-error handling. It loads the shared TOML config (`[shell]` section) to build a `PermissionPolicy` and `AuditLogger`, then constructs a `ShellState` and repeatedly parses input, calling `execute_command` from `exec.rs` for each parsed command.

### Lexer (`lexer.rs`)

A hand-written lexer that produces tokens such as `Word`, `Assignment(name, value)`, `Number`, redirection operators, control operators, parentheses/braces, `Newline`, and `Eof`. It handles:

- Comments (`#` to end of line)
- Single and double quotes
- Backslash escaping
- Dollar constructs: `$((...))`, `$(...)`, `${...}`
- File-descriptor prefixes for redirections (e.g. `2>`)
- Assignment words (`VAR=value`)

Reserved words such as `if`, `then`, `while`, etc. are **not** recognised by the lexer; the parser treats them as ordinary words when appropriate.

### Parser (`parser.rs`)

A recursive-descent parser that builds the AST defined in `ast.rs`. It supports:

- Simple commands (words, assignments, redirects)
- Lists with `;`, `&&`, `||`
- Pipelines with `|`
- Compound commands: `if`/`then`/`else`/`elif`/`fi`, `while`/`do`/`done`, `for`/`in`/`do`/`done`, `case`/`in`/`esac`, `{ group; }`, `( subshell )`
- Function definitions: `name() { ... }` and `function name { ... }`
- Redirections including fd prefixes (`2>file`, `2>&1`)

### AST (`ast.rs`)

The AST uses a single `Command` enum plus supporting structs (`SimpleCommand`, `Word`, `Redirect`, `CaseArm`). `Word` currently stores the raw source string and is expanded later.

### Expansion (`expand.rs`)

Performs POSIX-like word expansion in this order:

1. Quote removal (single/double)
2. Tilde expansion (`~`, `~user` via `/etc/passwd`)
3. Parameter expansion (`$VAR`, `${VAR}`, `$1`, `$?`, `$$`, `$#`, `$@`, `$*`)
4. Arithmetic expansion (`$((expr))`)
5. Word splitting (simplified IFS = space / tab / newline)
6. Pathname expansion (glob in the current directory only)

Command substitution `$(cmd)` is parsed by the lexer but **not implemented** at execution time — `expand.rs` logs an error and discards the command.

### Glob (`glob.rs`)

Implements `*`, `?`, and bracket expressions (`[abc]`, `[!abc]`, ranges `[a-z]`) using greedy backtracking. `expand_pathname` lists entries in the current directory that match a pattern.

### Arithmetic (`arith.rs`)

A recursive-descent evaluator for integer arithmetic expressions with Bash-like precedence:

- Ternary `?:`
- Logical `||`, `&&`, `!`
- Bitwise `|`, `^`, `&`, `~`
- Comparisons `==`, `!=`, `<`, `<=`, `>`, `>=`
- Shifts `<<`, `>>`
- Add/sub, mul/div/mod, power `**`
- Unary `+`, `-`, `!`, `~`
- Variables and assignment operators (`=`, `+=`, `-=`, `*=`, `/=`, `%=`)

Division/modulo by zero produces an error. Integer overflow uses wrapping arithmetic; number parsing reports overflow as an error.

### Builtins (`builtin.rs`)

Implemented builtins:

- `:` / `true` / `false`
- `echo` (supports `-n`)
- `cd` (with `HOME` fallback)
- `pwd`
- `export` (marks variables exported and calls `std::env::set_var`)
- `exit`
- `shift`
- `test` / `[` (basic unary and binary checks)

### Execution (`exec.rs`)

The execution engine maintains `ShellState`, which holds variables, exported names, positional parameters, function definitions, the last exit status, PID info, and current working directory.

Execution features:

- Simple commands: assignments, then function → builtin → external lookup.
- External commands are run with `std::process::Command` using the current `PATH`.
- Redirections: `<`, `>`, `>>`, `<>`, `<&`, `>&` (simplified fd duplication).
- Pipelines: only external simple commands are supported inside pipelines.
- Subshells are simulated by cloning `ShellState`, executing, and discarding mutations.
- Functions save/restore positional parameters.
- `if`, `while`, `for`, `case` use standard shell semantics.

### catus Agent (`catus/src/`)

- `config.rs` loads the shared TOML file and exposes `AppConfig`.
- `message.rs` defines conversation roles and messages.
- `llm.rs` implements an OpenAI-compatible streaming client over SSE.
- `tool.rs` implements the OpenAI-compatible `shell` function tool, parses `tool_calls` entries, and executes the extracted command through `sutcac-sh`.
- `app.rs` maintains conversation state, the embedded `ShellState`, and the Q-A/tool loop.
- `tui.rs` renders a three-pane ratatui layout (history, input, status bar).
- `main.rs` wires the TUI event loop, LLM streaming task, and tool execution together under `tokio`.

## Code style and conventions

- Standard Rust 2024 formatting. Use `cargo fmt` before committing.
- Each source file begins with a doc comment (`//!`) describing its purpose.
- Module-level unit tests live in `#[cfg(test)] mod tests` inside the same file.
- Error messages are prefixed with `sutcac-sh:` and printed to stderr.
- Shell variable expansion uses an `ExpandContext` passed through the expansion functions.
- AST nodes are `Clone` and `Debug`/`PartialEq` where useful for tests.

## Security considerations

- The shell `exec`s external programs found in `PATH`; do not run untrusted scripts without sandboxing.
- `builtin::export` uses `unsafe { std::env::set_var(...) }` to mutate the process environment.
- `export` may call `std::env::set_var` with user-controlled values, which is documented as unsafe in Rust because it can race with other threads reading the environment.
- Tilde expansion reads `/etc/passwd` directly.
- There is no input validation beyond parsing; malformed arithmetic can produce runtime errors but the evaluator is integer-only and wraps on overflow.
- Command substitution (`$(...)`) is lexed but not implemented, so malicious commands inside `$()` are currently ignored rather than executed.
- `catus` stores the API key in plain text in the TOML config file. Do not commit `config.toml` to version control (it is ignored by `.gitignore`).
- The Agent can execute arbitrary shell commands; run it with an appropriate `perm_mode` and only in environments you trust.

## Deployment

There is no deployment, packaging, or CI configuration in the repository. The project is built and run locally with Cargo.
