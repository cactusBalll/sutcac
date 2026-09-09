//! catus-core: the frontend-independent Agent runtime.
//!
//! This crate holds everything that does not depend on a specific user
//! interface: the LLM client, tool dispatch, subagents, session history,
//! memory, and the turn state machine in [`app`] orchestrated by
//! [`runtime::Runtime`]. Frontends (the `catus` TUI today, Web or ACP
//! adapters later) drive the runtime through semantic actions and consume
//! [`runtime::RuntimeEvent`]s.

pub mod agents;
pub mod app;
pub mod config;
pub mod frontmatter;
pub mod history;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod message;
pub mod resources;
pub mod runtime;
pub mod skills;
pub mod subagent;
pub mod tool;
