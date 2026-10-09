# sutcac

**An OpenAI-compatible coding agent runtime with four frontends — a terminal UI, a Tauri desktop/web app, an HTTP/WebSocket server, and a headless one-shot mode — plus a Bash-compatible shell and a local hybrid-retrieval index.**

[中文文档](README.CN.md) · [Agent/contributor guide](AGENTS.md)

---

## Features

- **One runtime, four frontends.** `catus-core` owns the session, turn state machine, LLM/subagent channels, and an event queue. The TUI, Tauri app, HTTP server, and headless mode all drive it through the same semantic actions and consume the same `RuntimeEvent`s.
- **OpenAI-compatible streaming LLM client** with tool calling, reasoning deltas, usage tracking, and prompt-prefix–cache-friendly append-only request paths.
- **Rich built-in toolset** — `shell`, `read`, `edit`, `todo`, `use_skill`, `ask_user`, `ask_permission`, `task`/`taskSync`, plus RAG tools.
- **Subagents** defined in Markdown, dispatched synchronously or asynchronously.
- **Agent Skills** ([agentskills.io](https://agentskills.io/specification)) discovered from disk and activated on demand.
- **Agent Memory** — an mdbook store, recalled before and written after each turn by a dedicated memory subagent.
- **MCP client** supporting stdio child processes and remote Streamable HTTP servers, exposed as one gateway tool per server.
- **Local hybrid RAG** (EmbeddingGemma dense lane + Tantivy BM25, fused with RRF) for `rust`/`ts` + Markdown workspaces.
- **A simplified Bash-compatible shell** (`sutcac-sh`) used for command execution, with a tag- and path-based permission model and an audit trail.
- **SQLite session history** with per-workspace `/resume`.

## Repository layout

| Crate / package | What it is |
| --- | --- |
| `catus-core` | Frontend-independent Agent runtime: LLM client, tool dispatch, subagents, MCP client, RAG manager, memory, session history, and the turn state machine. Zero UI dependencies. |
| `catus` | TUI frontend binary (ratatui + crossterm). Also provides the headless `--test` mode. |
| `catus-web` | Tauri 2 + Vue 3 desktop/web frontend (thin Rust bridge + shared UI). |
| `catus-server` | axum + tower HTTP/WebSocket server frontend with an embedded Vue UI. |
| `catus-ui` | Shared npm package: all Vue components, overlays, the Pinia store, payload types, and the transport interface. |
| `catus-rag` | Local hybrid retrieval library (dense + BM25 + RRF). |
| `sutcac-sh` | Simplified Bash-compatible shell (lexer → parser → expansion → execution). Also a library used by `catus-core`. |
| `mcp-calc-server` | Standalone calculator MCP server used to test the MCP client. |

Supporting docs: [`AGENTS.md`](AGENTS.md) (deep implementation guide), [`config.toml.example`](config.toml.example), [`docs/prompt-cache-investigation.md`](docs/prompt-cache-investigation.md).

## Requirements

- **Rust 1.85+** (the workspace uses edition 2024).
- **Node.js + npm** for the web frontends (npm workspaces; run `npm install` at the repo root).
- **Linux (Tauri only):** `webkit2gtk-4.1` development packages.
- **Optional:** a shared `libonnxruntime.so` for the RAG embedding lane (catus can download one — see below), and the `mdbook` binary for building the memory store (optional; skipped when absent).

## Quick start

```bash
# 1. Build the whole workspace
cargo build

# 2. Configure a provider + model
mkdir -p ~/.config/catus
cp config.toml.example ~/.config/catus/config.toml
#   ...then edit api_key / base_url in ~/.config/catus/config.toml

# 3. Run the TUI
cargo run -p catus

# ...or the headless one-shot mode
cargo run -p catus -- --test "explain this repository"
```

### The four frontends

```bash
# Terminal UI
cargo run -p catus
cargo run -p catus -- -w <dir>          # run against another workspace

# Headless one-shot (same event loop, no TUI)
cargo run -p catus -- --test "prompt"

# Tauri desktop/web app
cd catus-web && npm run tauri dev

# HTTP/WebSocket server + embedded web UI  (http://127.0.0.1:3117)
cargo run -p catus-server
cargo run -p catus-server -- -w <dir> --host 127.0.0.1 --port 4000

# Server UI hot-reload dev (proxies /api and /ws to :3117)
cd catus-server/web && npm run dev
```

All frontends accept `-w/--workspace <dir>`: the process switches into that directory before config loading, so the workspace config (`.sutcac/config.toml`), workspace agents/skills, and the session-history cwd all resolve against it.

TUI keys: **Enter** send · **Esc/Ctrl+C** quit · **Up/Down/PageUp/PageDown** scroll · **Home/End** jump. The terminal tab title tracks status and a taskbar animation plays while busy.

## Configuration

Config lives under one XDG base directory (`$XDG_CONFIG_HOME/catus/`, falling back to `~/.config/catus/`) and is initialized from embedded resources on first startup. A workspace file `./.sutcac/config.toml` is merged on top: scalar/table fields override key by key; `[[providers]]` (by `name`), `[[models]]` (by `id`), and `[[mcp.servers]]` (by `name`) merge entry by entry. Changes from `/model`-style commands are saved back to the workspace config when it exists, otherwise to the XDG config.

| Path | Purpose |
| --- | --- |
| `config.toml` | Configuration (see below). |
| `agents/`, `skills/` | Installed agent definitions and skills. |
| `catus.log` | Application log (fixed path; only `log_level` is configurable). |
| `audit.log` | Shell audit trail (fixed). |
| `history/sessions.db` | SQLite session history (fixed); `/resume` lists the current workspace's sessions. |
| `memory/` | Global Agent Memory mdbook store (fixed, shared by all projects). |

### Providers and models

`catus` separates the API vendor endpoint from the model configuration:

```toml
[[providers]]
name = "opencode"
base_url = "https://api.opencode.example.com/v1"
api_key = "sk-..."
session_header = "x-opencode-session"   # optional: one stable session id per conversation

[[models]]
id = "kimi-k2"                # sent as the API `model` field
name = "Kimi K2"              # UI display name (falls back to id)
context_window = "128k"       # 0/omitted = unknown; accepts "512k"/"1M" (1024-based)
provider = "opencode"

[agent.models]                # capability tiers referenced by agent frontmatter
performance = "kimi-k2"       # required
# efficient = "gpt-4o-mini"   # optional, falls back to performance
```

`/model list` lists configured models, `/model <name>` switches (by id or display name), and bare `/model` opens a picker. Changing models rebuilds the LLM client and takes effect on the next request.

### MCP servers

```toml
[[mcp.servers]]
name = "calc"
command = "target/debug/mcp-calc-server"          # stdio (default)

[[mcp.servers]]
name = "remote-calc"
transport = "streamable-http"
url = "https://mcp.example.com/calc"
[mcp.servers.headers]
Authorization = "Bearer sk-..."
```

Each connected server is exposed to the model as one gateway tool named `mcp_{server_name}` (non-alphanumeric characters sanitized to `_`) with `list` / `help` / `invoke` actions. Connection failures are logged; healthy servers are still used.

### RAG

```toml
[rag]
enabled = true
tools_enabled = true     # register rag_search / rag_index
auto_inject = true       # inject top-k context after each user turn
inject_top_k = 8
```

The index lives in the per-workspace `./.sutcac/rag/`. Build it with `/rag index` (or `/rag rebuild`); `rag_search` triggers a lazy load on first use. The embedding lane needs a shared `libonnxruntime.so` — run `catus --setup-ort` (or enable `[rag].enabled`, which auto-provisions ORT through `catus-core/src/ort.rs`).

### Permissions

`[shell].perm_mode` accepts `allow_all`, `allow:<tags>`, `deny:<tags>`, a combination, and the per-command `allow1:<cmds>` / `deny1:<cmds>` forms. Tags are arbitrary (`read`, `write`, `network`, `clipboard`, …). The workspace (`base_dir`) is always readable/writable; other directories are denied unless granted via `read_paths` / `write_paths` or the `ask_permission` tool. Every external command and redirection is checked and recorded in the audit log.

```toml
[shell]
perm_mode = "allow:read deny:network"
read_paths  = ["/home/user/data"]
write_paths = ["/home/user/projects"]
```

See [`config.toml.example`](config.toml.example) for a fully commented reference, including the command-tag shorthand lists.

## Slash commands

| Command | Description |
| --- | --- |
| `/help [command] [subcommand]` | Show general help or per-command help. |
| `/exit` | Quit. |
| `/new [path]` | Start a new session. |
| `/config [set <key> <value>]` | Inspect/edit config fields (three-tab editor in the web UI). |
| `/resume [name]` | Load a session from history (bare `/resume` opens a picker). |
| `/status` | Model, request count, and token usage. |
| `/model [list \| set-performance <model> \| set-efficient <model> \| name]` | List or switch models. |
| `/agent [list \| status \| use <name> <task> \| watch <id> \| close [id] \| disable/enable <name>]` | Manage subagents. |
| `/skill [list \| use <name> \| disable/enable <name>]` | Manage Agent Skills. |
| `/memory [status \| on \| off \| path]` | Toggle/inspect the Agent Memory subsystem. |
| `/permission [grant <tags> \| revoke <tags> \| reset]` | Adjust the main agent's session permissions. |
| `/auto` | Switch the session to `allow_all` (path restrictions kept; grants cleared). |
| `/mcp [list \| status \| disable/enable <name>]` | Inspect/enable MCP servers. |
| `/rag [status \| index [path] \| rebuild \| auto on\|off \| path]` | Manage the RAG index. |

`@agent_name <task>` is shorthand for `/agent use <agent_name> <task>`.

## Built-in tools

| Tool | Description |
| --- | --- |
| `shell` | Run a command in the embedded `sutcac-sh`, subject to the permission model. |
| `read` | Read a text file (1-based line offsets/limits); output is line-numbered. |
| `edit` | Exact single-occurrence text replacement, with a unified diff shown back. |
| `todo` | Main-agent-only session TODO list. |
| `use_skill` | Activate an Agent Skill by name. |
| `ask_user` | Ask the user a structured question (answered via the frontend overlay). |
| `ask_permission` | Request permission tags / directory access after a denial. |
| `task` / `taskSync` | Dispatch a subagent asynchronously / blockingly. |
| `completeTask` | Subagent-only: return a final result to the parent. |
| `rag_search` / `rag_index` | Query / build the local hybrid index (when enabled). |
| `mcp_{server}` | Gateway to one MCP server's tools (`list`/`help`/`invoke`). |

## Subagents

Subagents are Markdown files (e.g. `.sutcac/agents/coder.md`) with YAML frontmatter:

```markdown
---
name: coder
description: Implements focused code changes
model: efficient            # performance | efficient (resolved via [agent.models])
tools: [shell, read, edit, inherit]
permission: allow_all
skills: [commit, inherit]
---
You are a focused coding subagent. ...
```

The `main.md` file is required and supplies the main system prompt, tools, and permissions. A `role: memory` agent drives Agent Memory. Subagents cannot spawn further subagents; they always receive `completeTask` and never `task`/`taskSync`. `fork` context inherits the parent's env/vars/cwd; `create` starts independent.

## Agent Skills

Skills are directories containing a `SKILL.md` file with `name`/`description` frontmatter, discovered from `./.sutcac/skills/`, `$XDG_CONFIG_HOME/catus/skills/`, and `~/.config/catus/skills/` (plus `[agent].skill_paths`). Only metadata is loaded at startup; the model activates a skill via `use_skill`, which injects its body as a system message.

## Agent Memory

When `[agent.memory].enabled` is set (requires a `role: memory` agent, installed by default), catus runs a **recall** pass before the first user turn (injecting relevant memories as a system message) and a **write** pass after each turn (updating the mdbook store at `~/.config/catus/memory`). Configure with `enabled`, `auto_recall`, `auto_write`, and toggle per session with `/memory`.

## Development

```bash
cargo build                       # whole workspace
cargo test --workspace            # all tests
cargo test -p sutcac-sh expand    # a single module's tests
cargo fmt                         # format before committing
```

The shell pieces are also runnable standalone:

```bash
cargo run -p sutcac-sh -- -c "echo hi"     # one-shot command
cargo run -p sutcac-sh -- script.sh        # run a script
cargo run -p sutcac-sh                     # interactive REPL
cargo run -p mcp-calc-server               # calculator MCP server (stdio)
```

Conventions: every source file starts with a `//!` doc comment and unit tests live in an inline `#[cfg(test)] mod tests`. Multi-file modules use the `foo.rs` + `foo/` layout (never `mod.rs`).

## Security notes

- **Never commit** `.sutcac/config.toml` or `~/.config/catus/config.toml` — they contain a plain-text API key.
- The agent executes arbitrary shell commands against the real filesystem; use an appropriate `perm_mode` and only run it in trusted environments. The HTTP server binds `127.0.0.1` by default and carries **no authentication** — local use only.
