# AGENTS.md

Guide for AI agents working in this repo. All commands run from the workspace root.

## Overview

Rust workspace (edition 2024, needs Rust 1.85+) with two crates:

- `sutcac-sh` — simplified Bash-compatible shell: hand-written lexer → recursive-descent parser (`ast.rs`) → word expansion (`expand.rs`, `glob.rs`, `arith.rs`) → execution (`exec.rs`). Also a library consumed by catus.
- `catus` — Agent TUI binary: OpenAI-compatible streaming client (`llm.rs`), ratatui UI, executes the model's shell tool calls through embedded `sutcac-sh` (`tool.rs`, `app.rs`).

`bash.md` (Chinese) is the Bash-internals design reference.

## Agent Skills

`catus` supports [Agent Skills](https://agentskills.io/specification): directories containing a `SKILL.md` file with YAML frontmatter (`name`, `description`, plus optional fields) followed by Markdown instructions.

### Search paths

Skills are discovered from directories that mirror the config-file search order:

1. `./.sutcac/skills/`
2. `$XDG_CONFIG_HOME/catus/skills/`
3. `~/.config/catus/skills/`

Each search directory should contain skill subdirectories (e.g. `my-skill/SKILL.md`). Additional paths can be added via `[agent].skill_paths` in `config.toml`.

### Runtime behavior

- At startup, only skill metadata (`name` and `description`) is loaded.
- If `[agent].auto_include_skills` is true (default), the skill catalog is appended to the system prompt.
- Use `/skill list` to show discovered skills, `/skill use <name>` to activate a skill, or bare `/skill` to open a picker overlay.
- Activating a skill loads its full `SKILL.md` body and injects it as a system message.

## Commands

```bash
cargo build                          # whole workspace
cargo test --workspace               # all tests; unit tests live in #[cfg(test)] mod tests inside each source file
cargo test -p sutcac-sh expand       # single module's tests by name filter
cargo fmt                            # before committing

cargo run -p sutcac-sh -- -c "echo hi"   # one-shot command
cargo run -p sutcac-sh -- script.sh      # script file
cargo run -p sutcac-sh                   # interactive REPL (rustyline)

cargo run -p catus                        # Agent TUI
cargo run -p catus -- --test "prompt"     # headless one-shot test prompt
```

No CI, lint config, or integration tests exist. Tests currently pass (~64 + 4 in sutcac-sh, 41 in catus).

## Configuration

Both binaries read the same TOML file, first match wins:

1. `./.sutcac/config.toml`
2. `$XDG_CONFIG_HOME/catus/config.toml`
3. `~/.config/catus/config.toml`

Copy the root-level `config.toml.example` to `.sutcac/config.toml` and fill in `[api].api_key`. Sections: `[api]`/`[agent]` for catus only; `[shell]` shared by both.

### Permission model (`[shell]`)

Checked before every external command and redirection; denials return non-zero and are recorded in the audit log (`audit.rs`, text or JSON format).

- `perm_mode = "allow_all" | allow:<tags> | deny:<tags> | allow:<tags> deny:<tags> | allow1:<cmds> | deny1:<cmds>`
- Tags are arbitrary custom strings (`read`, `write`, `network`, …); required set must be covered by allow and disjoint from deny. `deny1` beats `allow1`.
- Per-command tags come from `[shell.commands]`; unlisted external commands default to READ+WRITE. Shorthand `read = [...]` / `write = [...]` under `[shell]` tags whole command lists; explicit `commands` entries win.
- Path-based access control: `read_paths = ["/home/user/data"]` restricts command arguments and input redirects to those directories; `write_paths = ["/home/user/projects"]` restricts output redirects and the arguments of commands that require WRITE. Empty lists disable the restriction. Commands must be tagged READ to read from `read_paths` without also being in `write_paths`.

## Non-obvious implementation facts

- Command substitution `$(...)` IS implemented: expansion takes an executor callback (`ExpandContext.subst`) that `exec.rs` wires up; it runs in a **cloned** `ShellState` so substitutions cannot mutate parent state. Without an executor (bare library use) it errors.
- Pipelines only support **external simple commands** as elements (see `execute_pipeline` in `exec.rs`).
- Subshells `( ... )` are simulated by cloning `ShellState` and discarding mutations.
- The lexer does not recognise reserved words (`if`, `while`, …) — the parser treats them contextually.
- Tilde expansion reads `/etc/passwd` directly; no NSS.
- `builtin::export` uses `unsafe { std::env::set_var }`.
- Arithmetic (`arith.rs`) is integer-only with wrapping overflow; div/mod by zero is an error.

## Conventions

- Every source file starts with a `//!` doc comment; unit tests in a `#[cfg(test)] mod tests` in the same file.
- Shell error messages go to stderr prefixed `sutcac-sh:`.
- AST nodes derive `Clone`, `Debug`, `PartialEq` where useful; expansion functions take an `ExpandContext`.

## catus TUI specifics

- Rendering lives in `catus/src/ui/` (`chat.rs` = history/input/status bar; `overlay.rs` = modal pages); `tui.rs` only manages terminal raw-mode lifecycle. Interaction logic stays in `app.rs`.
- Keys: Enter send, Esc/Ctrl+C quit, Up/Down/PageUp/PageDown scroll, Home/End jump.
- `/resume <name>` loads `<history_dir>/<name>.json`; bare `/resume` opens a List-based picker overlay (↑/↓ select, Enter load, Esc cancel); `/status` opens a Table overlay with model/requests/token usage. Overlays swallow keys before the input line (`App::handle_overlay_key`).
- Status bar keeps a compact right-aligned `ctx N tok | total M`; full details are in the /status page.
- Streaming requests set `stream_options.include_usage`; the returned `Usage` (incl. cached tokens) accumulates in `App.usage`.

## Security notes

- Do not commit `.sutcac/config.toml` (contains a plain-text API key).
- The Agent executes arbitrary shell commands via the real filesystem/process; use an appropriate `perm_mode` in trusted environments only.
