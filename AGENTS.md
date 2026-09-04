# AGENTS.md

Guide for AI agents working in this repo. All commands run from the workspace root.

## Overview

Rust workspace (edition 2024, needs Rust 1.85+) with three crates:

- `sutcac-sh` — simplified Bash-compatible shell: hand-written lexer → recursive-descent parser (`ast.rs`) → word expansion (`expand.rs`, `glob.rs`, `arith.rs`) → execution (`exec.rs`). Also a library consumed by catus.
- `catus` — Agent TUI binary: OpenAI-compatible streaming client (`llm.rs`), ratatui UI, executes the model's shell tool calls through embedded `sutcac-sh` (`tool/`, `app.rs`). Also an MCP client: configured MCP servers are connected at startup and their tools are exposed to the LLM (`mcp.rs`).
- `mcp-calc-server` — standalone calculator MCP server used to test catus's MCP client support. Exposes arithmetic tools (`sum`, `sub`, `mul`, `div`, `modulo`). Serves stdio by default, or Streamable HTTP with `--http <addr>`.

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
- The LLM activates a skill by calling the built-in `use_skill` tool (`SkillTool` in `tool/skill.rs`); activation loads the full `SKILL.md` body and injects it as a system message. Re-activation is idempotent.
- Use `/skill list` to show discovered skills, `/skill use <name>` to activate a skill from the TUI, or bare `/skill` to open a picker overlay.

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

cargo run -p mcp-calc-server              # calculator MCP server (stdio)
cargo test -p mcp-calc-server             # unit + integration tests against catus MCP client
```

No CI, lint config, or integration tests exist. Tests currently pass (~64 + 4 in sutcac-sh, ~144 in catus, ~9 in mcp-calc-server).

## Configuration

Both binaries read the same TOML file, first match wins:

1. `./.sutcac/config.toml`
2. `$XDG_CONFIG_HOME/catus/config.toml`
3. `~/.config/catus/config.toml`

Copy the root-level `config.toml.example` to `.sutcac/config.toml` and fill in `[api].api_key`. Sections: `[api]`/`[agent]` for catus only; `[shell]` shared by both; `[mcp]` for catus only.

### MCP client (`[mcp]`)

`catus` can act as an MCP client and expose tools from external MCP servers to the LLM.

- Configure servers under `[[mcp.servers]]` with `name` plus either stdio settings (`command`, `args`, `env`) or remote settings (`transport = "streamable-http"`, `url`, optional `headers` for e.g. `Authorization`).
- Two transports: stdio child processes (`rmcp` `transport-child-process`, default) and remote Streamable HTTP endpoints (`rmcp` `transport-streamable-http-client-reqwest`). The legacy SSE transport was removed in `rmcp` 3.x and is not supported.
- Tool names are prefixed with `{server_name}__` to avoid collisions and identify the owning server, e.g. `filesystem__read_file`.
- Connection failures are logged; successful servers are still used.
- Use `/mcp list` to see configured servers and discovered tools, and `/mcp status` for connection counts.

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
- MCP tool names are prefixed with `{server_name}__` so the LLM can call the right server; each discovered tool is wrapped in an `McpTool` (`mcp.rs`) holding a shared `Arc<McpManager>`.
- All LLM-callable tools implement the `Tool` trait defined in `tool.rs` (dyn-safe via a boxed-future `execute`, no `async-trait` dependency) and live in a `Toolbox` on `App`. Two sources: built-ins, one per file under `tool/` (`tool/shell.rs` = `ShellTool`, `tool/skill.rs` = `SkillTool`), and MCP-converted (`McpTool` in `mcp.rs`). `tool.rs` is generic infrastructure — `ToolCall` carries no per-tool semantics (argument parsing lives in each tool, e.g. `parse_command` in `tool/shell.rs`), and `App::new` is the composition root that registers the built-ins. `app.rs::run_pending_tool` dispatches purely by advertised tool name through `Toolbox::get`; `llm.rs` receives the full definition list from the caller and hardcodes nothing.
- `SkillTool` (`use_skill`) activates skills via tool calls, not text markers: it loads the skill instructions through a `ToolContext` (`shell_state`, `skill_registry`, `active_skills`, `messages`) passed in by `App` at dispatch time.
- `AskUserTool` (`ask_user`, `tool/ask_user.rs`) never blocks on user input — `run_pending_tool` awaits tools inline in the event loop, so a blocking tool would deadlock. Instead `execute` validates the questions and returns a `ToolResult` whose `interaction` field is `Some(InteractionRequest)`; `run_pending_tool` then stashes the call in `App.pending_interaction`, opens `Overlay::Ask`, and returns without pushing a result message. The overlay collects answers (one question at a time; the last row is always an "Other" free-text option), and `App::complete_interaction` / `cancel_interaction` append the final `ToolResult` message and resume the turn via `AppAction::StartStream`.

## Conventions

- Every source file starts with a `//!` doc comment; unit tests in a `#[cfg(test)] mod tests` in the same file.
- Multi-file modules use the `foo.rs` + `foo/` layout — never `mod.rs`. The parent file (`app.rs`, `tool.rs`, `ui.rs`) declares `mod bar;` and submodule files live in the matching `foo/` directory (`tool/shell.rs`, `tool/skill.rs`).
- Shell error messages go to stderr prefixed `sutcac-sh:`.
- AST nodes derive `Clone`, `Debug`, `PartialEq` where useful; expansion functions take an `ExpandContext`.

## catus TUI specifics

- Rendering lives in `catus/src/ui/` (`chat.rs` = history/input/status bar; `overlay.rs` = modal pages); `tui.rs` manages the terminal raw-mode lifecycle and emits OSC sequences: the tab title tracks app status (`catus · 正在输出`/`空闲`/`错误`), the taskbar shows an indeterminate ConEmu `OSC 9;4;3` animation while busy and hides on completion, and a BEL alert rings when a busy turn returns to idle. Interaction logic stays in `app.rs`.
- Keys: Enter send, Esc/Ctrl+C quit, Up/Down/PageUp/PageDown scroll, Home/End jump.
- `/resume <name>` loads `<history_dir>/<name>.json`; bare `/resume` opens a List-based picker overlay (↑/↓ select, Enter load, Esc cancel); `/status` opens a Table overlay with model/requests/token usage. Overlays swallow keys before the input line (`App::handle_overlay_key`).
- The `ask_user` tool opens the question overlay (`Overlay::Ask`): one question at a time with progress `i/N`, ↑/↓ move across options plus a final "Other" row where typing edits the text; Enter chooses the focused option (single-select) or confirms the checked options (multi-select, Space toggles); Esc cancels the whole question and reports "user cancelled" back to the model.
- `/mcp list` shows configured MCP servers and their discovered tools; `/mcp status` shows how many servers are connected.
- Status bar keeps a compact right-aligned `ctx N tok | total M`; full details are in the /status page.
- Streaming requests set `stream_options.include_usage`; the returned `Usage` (incl. cached tokens) accumulates in `App.usage`.

## Security notes

- Do not commit `.sutcac/config.toml` (contains a plain-text API key).
- The Agent executes arbitrary shell commands via the real filesystem/process; use an appropriate `perm_mode` in trusted environments only.
