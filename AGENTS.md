# AGENTS.md

Guide for AI agents working in this repo. All commands run from the workspace root.

## Overview

Rust workspace (edition 2024, needs Rust 1.85+) with six crates:

- `sutcac-sh` — simplified Bash-compatible shell: hand-written lexer → recursive-descent parser (`ast.rs`) → word expansion (`expand.rs`, `glob.rs`, `arith.rs`) → execution (`exec.rs`). Also a library consumed by catus-core.
- `catus-core` — the frontend-independent Agent runtime: OpenAI-compatible streaming client (`llm.rs`), tool dispatch (`tool/`, `tool.rs`), subagents (`subagent.rs`), MCP client (`mcp.rs`), SQLite session history (`history.rs`, rusqlite/bundled, `~/.config/catus/history/sessions.db`), Agent Memory (`memory.rs`), and the turn state machine in `app.rs` orchestrated by `runtime.rs`. Zero UI dependencies (no ratatui/crossterm). `bootstrap.rs` holds the shared startup for all frontends (`load_config`, `bootstrap_runtime`).
- `catus` — the TUI frontend binary: ratatui UI (`ui/`), terminal lifecycle + OSC (`tui.rs`), and TUI-owned view state (`ui/state.rs` = `UiState`: input line, chat scroll, overlays). It depends on `catus-core` and drives the runtime through semantic actions (`App::handle_input_line`, `complete_interaction`, …) while consuming `RuntimeEvent`s from `Runtime::next_event`. The headless `--test` mode runs the same event loop.
- `catus-web` — the Tauri 2 + Vue 3 web frontend (`catus-web/`): `src-tauri/` is a thin Rust bridge (a single actor task owns the `Runtime`, serving Tauri commands `send_input`/`complete_interaction`/`cancel_interaction`/`overlay_action`/`get_snapshot`/`get_subagent_messages`/`quit_app` and emitting `runtime-event` (serialized `RuntimeEvent`), `snapshot` (serialized `AppSnapshot`), `startup-error`); `src/` is a thin Vue shell (Tauri transport + entry), with all components/overlays/store shared from `catus-ui`. Run with `cd catus-web && npm run tauri dev`; accepts `-w/--workspace <dir>`. Linux needs the webkit2gtk-4.1 dev packages.
- `catus-server` — the axum + tower HTTP/WebSocket server frontend (`catus-server/`): mirrors the Tauri bridge (`src/actor.rs` = the single `Runtime` owner with the same `Command` enum; events broadcast instead of emitted), exposes the same seven actions as REST routes (`/api/input`, `/api/interaction/complete`, `/api/interaction/cancel`, `/api/overlay`, `/api/snapshot`, `/api/subagents/{id}/messages`, `/api/quit`) plus `GET /ws` (frames `{"channel": "runtime-event"|"snapshot"|"startup-error"|"app-quit", "payload": ...}`; initial snapshot on connect; multi-client broadcast), `GET /api/completion?input=…` (slash-command candidates served straight from the static `BUILT_IN_REGISTRY`, no actor round-trip; the web UI completes plain text from its own session history), and `POST /api/config` + `POST /api/config/remove` (scope-aware config editing: `App::set_config_field_in`/`remove_config_field_in` write into the workspace `./.sutcac/config.toml` or the global XDG `config.toml` via `toml_edit` (comments/unknown keys preserved), snapshotting per-scope state in `AppSnapshot.config_scopes`; the runtime only adopts a value when its scope wins — workspace overrides global key by key; the editor is a three-tab layout (effective/workspace/global) opened from the status-bar `config` button). Editable fields are spec-driven (`CONFIG_FIELD_SPECS` in `catus-core/src/app.rs`: 9 fields — int `agent.max_tool_rounds`, strings `agent.log_level`/`shell.perm_mode`, bools `agent.auto_include_skills`/`agent.memory.{enabled,auto_recall,auto_write}`, lists `shell.{read,write}_paths`; `App::apply_config_field_effect` documents each field's live effect, e.g. bool/list values convert from `true|false` and comma-separated strings). The status-bar model name opens the model picker (the bare-`/model` menu; `switch_model` takes the model id, which `/model list` now shows per entry). The Vue UI (`web/`, thin shell over `catus-ui` with an HTTP/WS transport and auto-reconnect) is embedded via `rust-embed` from `web/dist` (debug builds read from disk; `--static-dir` overrides). Binds `127.0.0.1:3117` by default (`--host/--port`), accepts `-w/--workspace <dir>`. No auth — local use only.
- `catus-ui` — the shared npm package (`catus-ui/`, npm workspaces): all frontend components + overlays, the Pinia store (`stores/runtime.ts`), TS payload types, `CatusApp.vue`, and the `CatusTransport` interface (`transport.ts`: 7 command methods + `subscribe(onRuntimeEvent/onSnapshot/onStartupError/onQuit)`). Backends are injected per frontend (`catus-web/src/transport/tauri.ts`, `catus-server/web/src/transport/http.ts`).
- `mcp-calc-server` — standalone calculator MCP server used to test catus's MCP client support. Exposes arithmetic tools (`sum`, `sub`, `mul`, `div`, `modulo`). Serves stdio by default, or Streamable HTTP with `--http <addr>`.

### Frontend/runtime boundary (for future Web / ACP frontends)

- **Core owns**: session state (`App`), the turn state machine, LLM/subagent channels, and a `RuntimeEvent` queue (`StreamText/StreamReasoning/ToolCallAdded/UsageUpdated/MessagesChanged/InteractionRequested/SubagentEvent/TurnComplete`), drained via `App::take_event` / `Runtime::next_event`.
- **Frontends own**: all view state (`UiState`), key/mouse mapping, modal pages. `UiState::apply_request` maps core `UiRequest`s (from `CommandOutcome::ui`) onto TUI overlays; other frontends may realize them differently or ignore them.
- **Interactions are events**: tools pausing for user input emit `RuntimeEvent::InteractionRequested`; frontends answer via `complete_interaction`/`cancel_interaction` and then `Runtime::maybe_resume_stream`. The runtime gates stream starts on pending memory passes internally.
- Slash commands never touch UI state; they return `CommandOutcome { handled, record_history, ui }`.

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

## Agent Definitions

`catus` also supports Markdown-based Agent definitions. Each agent is a single `.md` file (e.g. `.sutcac/agents/coder.md`) with YAML frontmatter followed by a Markdown body used as the agent's system prompt.

### Required frontmatter

- `name`: must match the file stem.
- `description`: short description for agent selection.

### Optional frontmatter

- `model`: capability tier, `performance` or `efficient`. The subagent runtime resolves the tier through the `[agent.models]` mapping in config.toml (`efficient` falls back to `performance` when unconfigured); agents without `model` use the parent's current model.
- `tools`: allowed tool names (single value or list). May include `inherit` to inherit the parent's toolset. Subagents never receive `task`/`taskSync`; they always receive `completeTask`.
- `permission`: sutcac-sh permission string for the agent's `ShellState`.
- `skills`: skill names (single value or list). May include `inherit` to inherit the parent's active skills.

### Search paths

Agent definitions are discovered from:

1. `./.sutcac/agents/`
2. `$XDG_CONFIG_HOME/catus/agents/`
3. `~/.config/catus/agents/`

Additional paths can be added via `[agent].agent_paths` in `config.toml`.

### Agent roles

Agents with special responsibilities are marked through a `role` frontmatter field instead of relying on file names:

- `role: main` — the main agent. `main.md` carries this role implicitly; only one agent may hold it.
- `role: memory` — the Agent Memory subagent (see "Agent Memory" below); only one agent may hold it. It stays visible in `/agent list` and can still be dispatched manually via `task`/`@name`, but catus also drives it automatically around each user turn.
- Agents without a `role` are plain subagents. Duplicate role claims make `AgentRegistry::discover` fail; unknown role values make the definition invalid.

### Main agent

The file `main.md` is required. Its body becomes the main system prompt, and its frontmatter controls the main agent's tools and permissions. If `main.md` is missing, the binary prints a warning and exits.

### Subagents

- The main agent can dispatch tasks to subagents via the `task` (async) or `taskSync` (blocks until completion) tools.
- Subagents report results by calling `completeTask`.
- Subagents cannot spawn further subagents.
- Subagent permissions always follow their declaration: both context modes rebuild the shell's `PermissionPolicy` from the agent's `permission` frontmatter (falling back to the configured `[shell]` policy), so parent session grants or `/auto` never leak into subagents. `fork` still inherits the parent's env/vars/cwd.
- Use `@agent_name <task>` in the input line as a shorthand for `/agent use <agent_name> <task>`.
- Use `/agent list`, `/agent status`, `/agent use`, `/agent watch`, and `/agent close` to manage and observe subagents.

## Agent Memory

`catus` maintains long-term memory as a single global mdbook project (`book.toml` + `src/SUMMARY.md` + topical chapter files) shared by all workspaces, driven by the `role: memory` subagent. The store distinguishes cross-workspace memory (`src/global/` chapters) from per-workspace memory (`src/workspaces/<slug>/` chapters, indexed by absolute path in `src/workspaces/README.md`). The recall/write judgment rules and the mdbook layout live in the memory agent's definition (`agents/memory.md`); `catus-core/src/memory.rs` carries only the control-chain contract (pass type, store/workspace paths, the exact `completeTask` JSON schema, size limits) plus result parsing and `MemoryState`. Both passes run through the regular subagent runtime in `create` mode (independent context/toolbox/shell permissions; `App` spawns them with `parent_call_id: None` and intercepts their events in `handle_subagent_event`). The memory subagent is path-confined to the memory store: `SubagentRunner::build_runtime_context` sets its `PermissionPolicy.base_dir` and shell cwd to `config.dirs.memory`, so it can read/write the store but never the workspace.

- **Recall** (before the main LLM request): `submit_user_message` dispatches the pass; the runtime withholds `start_llm_stream` while `awaiting_memory_recall()` (`Runtime::maybe_resume_stream` gates internally, and the stream starts when the recall pass completes). The memory agent decides whether the request is a simple task (no memory, `{"recall": false}`); otherwise it searches the store and returns `{"recall": true, "memory": "..."}`, which is injected into the main conversation as a system message. A failed/unparseable pass degrades to "no memory".
- **Write** (after the turn): `handle_llm_done`'s turn-complete branch dispatches a background summarize pass with a transcript of the current user turn (`memory::format_transcript`). The agent updates the mdbook store (initializing the scaffold if missing; `mdbook build` is optional and skipped when the binary is absent) and returns `{"written": bool, "summary": ...}`; the result is logged as an event message.
- Configured under `[agent.memory]`: `enabled` (master switch, default off), `auto_recall`, `auto_write`. The store lives at the fixed path `~/.config/catus/memory`. Enabling requires a `role: memory` agent definition — otherwise catus warns and keeps the subsystem disabled. catus installs a default `agents/memory.md` into the XDG directory on first startup; `--install-project-config` installs one into `.sutcac/`.
- Runtime toggle: `/memory status|on|off|path`; the session toggle is persisted via `STATE_MEMORY_ENABLED` in `session_state` and restored by `/resume`. Interrupted passes are not restarted on resume. The memory agent is still callable manually (`task`/`@memory <question>`) for one-off memory queries.

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
cargo run -p catus -- -w <dir> --test "p" # -w/--workspace: run against another
                                          # working directory (workspace config,
                                          # agents/skills, and session cwd)

cd catus-web && npm run tauri dev         # Agent web frontend (Tauri + Vue)

cargo run -p catus-server                 # Agent HTTP/WS server + web UI (127.0.0.1:3117)
cargo run -p catus-server -- -w <dir> --port 4000  # another workspace/port
cd catus-server/web && npm run dev        # UI hot-reload dev (proxies /api + /ws to :3117)

cargo run -p mcp-calc-server              # calculator MCP server (stdio)
cargo test -p mcp-calc-server             # unit + integration tests against catus MCP client
```

All four frontends (TUI, Tauri web, HTTP server, headless) accept `-w/--workspace <dir>`: the process switches into the directory before config loading, so the workspace config (`<dir>/.sutcac/config.toml`), workspace agents/skills, and the session-history cwd are all resolved against it.

npm packages are managed as a root-level npm workspace (`package.json` at the repo root: `catus-ui`, `catus-web`, `catus-server/web`; `npm install` at the root sets up everything).

No CI, lint config, or integration tests exist. Tests currently pass (~105 + 4 in sutcac-sh, ~197 in catus-core, ~43 in catus, ~12 in catus-server, ~9 in mcp-calc-server).

## Configuration

All paths (logs, audit trail, session history, memory store) live under one XDG base directory: `$XDG_CONFIG_HOME/catus/` (fallback `~/.config/catus/`):

- `config.toml` — configuration (initialized from embedded resources on first startup)
- `agents/`, `skills/` — installed agent definitions and skills
- `catus.log` — application log (fixed; only `log_level` is configurable)
- `audit.log` — shell audit trail (fixed)
- `history/sessions.db` — SQLite session history (fixed); each session row records the working directory it was created in, and `/resume` lists only the current workspace's sessions
- `memory/` — global Agent Memory mdbook store (fixed, shared by all projects)

Config loading (`AppConfig::load` in `catus-core/src/config.rs`):

1. If the XDG directory does not exist, it is created and initialized from the embedded resources (`resources::install_xdg_config`; existing files are never overwritten).
2. The XDG `config.toml` is loaded as the base.
3. If `./.sutcac/config.toml` (workspace config) exists, it is merged on top: scalar/table fields set in the workspace override the XDG base; `[[providers]]` (by `name`), `[[models]]` (by `id`) and `[[mcp.servers]]` (by `name`) are merged entry by entry (same-key entries merged field-wise, new entries appended). Config changes from `/model`-style commands are saved back to the workspace config when it exists, otherwise to the XDG config.

The removed keys `agent.history_path`, `agent.log_path`, `agent.memory.path`, and `shell.audit_log` are silently ignored in old config files. Copy the root-level `config.toml.example` to `~/.config/catus/config.toml` (or `.sutcac/config.toml` for per-workspace overrides) and fill in the provider API keys. Sections: `[[providers]]`/`[[models]]`/`[agent]` for catus only; `[shell]` shared by both (sutcac-sh applies the same XDG-base + workspace-merge loading to the `[shell]` section); `[mcp]` for catus only.

### Providers and models (`[[providers]]`, `[[models]]`)

`catus` separates the API vendor endpoint from the model configuration:

- `[[providers]]` defines an API vendor endpoint: `name`, `base_url`, `api_key`, and an optional `session_header` — the request header name used to carry one stable session ID per conversation (e.g. `"x-opencode-session"` for the opencode platform). Omit `session_header` for vendors that do not need it; the header is never sent when unset.
- `[[models]]` defines a concrete model: `id` (sent as the API `model` field), `name` (UI display name, falls back to `id`), `context_window` (tokens; 0 = unknown; accepts human-readable strings like `"512k"`/`"1M"` — 1024-based, case-insensitive, see `parse_context_window` in `catus-core/src/config.rs`), and `provider` (references a provider's `name`). Models carry no tier field; capability tiers are assigned in `[agent.models]`.

`AppConfig::resolve_models()` validates the configuration (unknown provider references are rejected) and produces `llm::Model`s with their provider attached. `[agent.models]` maps the two tiers (`performance` required, `efficient` optional) to model ids or display names; `AppConfig::validate_tier_models()` enforces that `performance` references a configured model, and `resolve_tier_models()` yields the per-tier `Model` (unset `efficient` falls back to `performance`). `App` starts on the first configured model; `/model list` lists them, `/model <name>` switches by id or display name, and bare `/model` opens a picker overlay. Switching rebuilds the `LlmClient`, so it takes effect on the next LLM request.

### MCP client (`[mcp]`)

`catus` can act as an MCP client and expose tools from external MCP servers to the LLM.

- Configure servers under `[[mcp.servers]]` with `name` plus either stdio settings (`command`, `args`, `env`) or remote settings (`transport = "streamable-http"`, `url`, optional `headers` for e.g. `Authorization`).
- Two transports: stdio child processes (`rmcp` `transport-child-process`, default) and remote Streamable HTTP endpoints (`rmcp` `transport-streamable-http-client-reqwest`). The legacy SSE transport was removed in `rmcp` 3.x and is not supported.
- Each connected server is exposed to the LLM as one gateway tool named `mcp_{server_name}` (e.g. `mcp_filesystem`); the LLM lists/inspects/invokes that server's tools through its `list`/`help`/`invoke` actions instead of the server's tools being advertised individually.
- Connection failures are logged; successful servers are still used.
- Use `/mcp list` to see configured servers and discovered tools, and `/mcp status` for connection counts.

### Permission model (`[shell]`)

Checked before every external command and redirection; denials return non-zero and are recorded in the audit log (`audit.rs`, text or JSON format).

- `perm_mode = "allow_all" | allow:<tags> | deny:<tags> | allow:<tags> deny:<tags> | allow1:<cmds> | deny1:<cmds>`
- Tags are arbitrary custom strings (`read`, `write`, `network`, …); required set must be covered by allow and disjoint from deny. `deny1` beats `allow1`.
- Per-command tags come from `[shell.commands]`; unlisted external commands default to READ+WRITE. Shorthand `read = [...]` / `write = [...]` / `rw = [...]` under `[shell]` tags whole command lists (`rw` grants both READ and WRITE); explicit `commands` entries win.
- Path-based access control: the workspace (session startup directory, `PermissionPolicy.base_dir`) is always readable and writable; every other directory is denied by default. `read_paths = ["/home/user/data"]` additionally allows reading from those directories; `write_paths = ["/home/user/projects"]` additionally allows writing (writable directories are implicitly readable). Extra directories can be granted for the session via `ask_permission` (see below). To lift the restriction entirely set `read_paths = ["/"]` and `write_paths = ["/"]`. Relative command paths are resolved against the live shell cwd, but containment is always checked against `base_dir`, so `cd` cannot widen access. In the standalone sutcac-sh library (no base dir set), empty path lists keep the legacy behavior of disabling the restriction.

## Non-obvious implementation facts

- Command substitution `$(...)` IS implemented: expansion takes an executor callback (`ExpandContext.subst`) that `exec.rs` wires up; it runs in a **cloned** `ShellState` so substitutions cannot mutate parent state. Without an executor (bare library use) it errors.
- Pipelines only support **external simple commands** as elements (see `execute_pipeline` in `exec.rs`).
- Subshells `( ... )` are simulated by cloning `ShellState` and discarding mutations.
- The lexer does not recognise reserved words (`if`, `while`, …) — the parser treats them contextually.
- Tilde expansion reads `/etc/passwd` directly; no NSS.
- `builtin::export` uses `unsafe { std::env::set_var }`.
- Arithmetic (`arith.rs`) is integer-only with wrapping overflow; div/mod by zero is an error.
- MCP tools are collapsed behind one gateway tool per server: each connected server registers an `McpServerTool` (`tool/mcp.rs`, names `mcp_{server}`, non-`[a-zA-Z0-9_-]` characters sanitized to `_`) with `list` (tool names + one-line descriptions), `help` (one tool's JSON input schema), and `invoke` (execute) actions. `McpManager` (`mcp.rs`) caches each server's tool catalog (`CachedTool`) at connect time, so only `invoke` traffic reaches the server; `/mcp list` reads the cached catalogs.
- All LLM-callable tools implement the `Tool` trait defined in `tool.rs` (dyn-safe via a boxed-future `execute`, no `async-trait` dependency) and live in a `Toolbox` on `App`. Two sources: built-ins, one per file under `tool/` (`tool/shell.rs` = `ShellTool`, `tool/skill.rs` = `SkillTool`, `tool/todo.rs` = `TodoTool` — a main-agent-only session TODO list stored in `App.todos`, never advertised to subagents), and MCP gateway tools (`McpServerTool` in `tool/mcp.rs`). `tool.rs` is generic infrastructure — `ToolCall` carries no per-tool semantics (argument parsing lives in each tool, e.g. `parse_command` in `tool/shell.rs`), and `App::new` is the composition root that registers the built-ins. `app.rs::run_pending_tool` dispatches purely by advertised tool name through `Toolbox::get`; `llm.rs` receives the full definition list from the caller and hardcodes nothing.
- `SkillTool` (`use_skill`) activates skills via tool calls, not text markers: it loads the skill instructions through a `ToolContext` (`shell_state`, `skill_registry`, `active_skills`, `messages`) passed in by `App` at dispatch time.
- `AskPermissionTool` (`ask_permission`, `tool/ask_permission.rs`) lets the model request one or more permission tags after a denial (`{"tags": [...]}`, comma-separated strings accepted too) and/or directory read/write access when a path was rejected (`{"read_paths": [...], "write_paths": [...]}`, relative paths resolved against the shell cwd). It returns an `InteractionRequest` like `ask_user`; the user picks "Allow for this session" / "Deny" in the ask overlay, and `App::complete_interaction` applies the choice via `PermissionPolicy::grant_tag` / `grant_paths` (`sutcac-sh`). Grants persist for the rest of the session, bypass the mode's allow/deny sets (tags), and extend the path whitelists (paths). Inside subagents the interaction is rejected like `ask_user`.
- `AskUserTool` (`ask_user`, `tool/ask_user.rs`) never blocks on user input — `run_pending_tool` awaits tools inline in the event loop, so a blocking tool would deadlock. Instead `execute` validates the questions and returns a `ToolResult` whose `interaction` field is `Some(InteractionRequest)`; `run_pending_tool` then stashes the call in `App.pending_interaction`, queues `RuntimeEvent::InteractionRequested`, and returns without pushing a result message. The frontend's ask page collects answers (one question at a time; the last row is always an "Other" free-text option), and `App::complete_interaction` / `cancel_interaction` append the final `ToolResult` message; the caller then resumes the turn via `Runtime::maybe_resume_stream`.

## Conventions

- Every source file starts with a `//!` doc comment; unit tests in a `#[cfg(test)] mod tests` in the same file.
- Multi-file modules use the `foo.rs` + `foo/` layout — never `mod.rs`. The parent file (`app.rs`, `tool.rs`, `ui.rs`) declares `mod bar;` and submodule files live in the matching `foo/` directory (`tool/shell.rs`, `tool/skill.rs`).
- Shell error messages go to stderr prefixed `sutcac-sh:`.
- AST nodes derive `Clone`, `Debug`, `PartialEq` where useful; expansion functions take an `ExpandContext`.

## catus TUI specifics

- Rendering lives in `catus/src/ui/` (`chat.rs` = history/input/status bar; `overlay.rs` = modal pages); `tui.rs` manages the terminal raw-mode lifecycle and emits OSC sequences: the tab title tracks app status (`catus · 正在输出`/`空闲`/`错误`), the taskbar shows an indeterminate ConEmu `OSC 9;4;3` animation while busy and hides on completion, and a BEL alert rings when a busy turn returns to idle. TUI interaction logic lives in `catus/src/ui.rs` + `ui/`; core turn/interaction logic lives in `catus-core/src/app.rs` + `runtime.rs`.
- Keys: Enter send, Esc/Ctrl+C quit, Up/Down/PageUp/PageDown scroll, Home/End jump.
- `/resume <name>` loads a session from the SQLite history (`~/.config/catus/history/sessions.db`, filtered by the current working directory); bare `/resume` opens a List-based picker overlay (↑/↓ select, Enter load, Esc cancel); `/status` opens a Table overlay with model/requests/token usage. Overlays swallow keys before the input line (`ui::overlay::handle_overlay_key` on `UiState`).
- Session history lives in the dedicated `catus-core/src/history.rs` + `history/` subsystem (`SessionStore`, rusqlite/bundled). Persistence is incremental: `App::persist_session` rewrites the full snapshot after every LLM turn, tool result, usage report, subagent event, and on exit. A session stores main + subagent messages, token usage, request count, provider session id, current model, active skills, pending tool calls / `ask_user` interaction, the main agent's TODO list, and shell cwd/vars. Resume restores all of it; still-running subagents restart their turn loop from the saved messages (`SubagentManager::restore`), and `task`/`taskSync` dispatches interrupted mid-flight are answered with an error tool result instead of being re-run. Old JSON history files are ignored.
- The `ask_user` tool emits `RuntimeEvent::InteractionRequested`; the TUI opens the question overlay (`Overlay::Ask`): one question at a time with progress `i/N`, ↑/↓ move across options plus a final "Other" row where typing edits the text; Enter chooses the focused option (single-select) or confirms the checked options (multi-select, Space toggles); Esc cancels the whole question and reports "user cancelled" back to the model.
- `/mcp list` shows configured MCP servers and their discovered tools; `/mcp status` shows how many servers are connected.
- `/model list` lists configured models (current one marked); `/model <name>` switches by id or display name; bare `/model` opens the model picker overlay. Switching rebuilds the `LlmClient` and takes effect on the next request.
- `/permission [grant <tags> | revoke <tags> | reset]` inspects and adjusts the main agent's session permission policy (grants apply only to the main agent; subagents always use the permissions declared in their definition); `/auto` switches the session policy to `allow_all` (path restrictions kept, session grants cleared).
- Status bar keeps a compact right-aligned `ctx N tok | total M`; full details are in the /status page.
- Streaming requests set `stream_options.include_usage`; the returned `Usage` (incl. cached tokens) accumulates in `App.usage`.

## Security notes

- Do not commit `.sutcac/config.toml` or `~/.config/catus/config.toml` (they contain a plain-text API key).
- The Agent executes arbitrary shell commands via the real filesystem/process; use an appropriate `perm_mode` in trusted environments only.
