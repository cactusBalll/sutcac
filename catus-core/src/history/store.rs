//! SQLite storage for session history (`sessions.db`).
//!
//! Owns the schema and all SQL. Callers in `crate::app` only deal with the
//! snapshot types defined in the parent module.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, params};

use super::{
    MAIN_AGENT_ID, SessionMeta, SessionSnapshot, SessionSummary, SubagentRecord, SubagentSnapshot,
};
use crate::message::{Message, Role};

/// Current schema version; bump when the layout changes and add a migration.
const SCHEMA_VERSION: i64 = 2;

/// SQL result type. `rusqlite::Error` covers open, query, and commit errors.
type Result<T> = std::result::Result<T, rusqlite::Error>;

/// A SQLite-backed store of conversation sessions.
///
/// The connection sits behind a mutex so the store is `Sync`: `App` holds it
/// and async command futures (`+ Send`) borrow `&App`.
pub struct SessionStore {
    conn: std::sync::Mutex<Connection>,
}

impl SessionStore {
    /// Open (creating if necessary) the database at `<dir>/sessions.db`.
    /// Missing parent directories are created automatically.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(e.to_string()),
            )
        })?;
        Self::open_path(&dir.join("sessions.db"))
    }
    /// Open the database at an explicit path. Used by tests and callers that
    /// manage the file location themselves.
    pub fn open_path(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn: std::sync::Mutex::new(conn),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database. Used by tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: std::sync::Mutex::new(conn),
        };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                session_id TEXT NOT NULL DEFAULT '',
                model_id TEXT NOT NULL DEFAULT '',
                request_count INTEGER NOT NULL DEFAULT 0,
                usage_json TEXT NOT NULL DEFAULT '{}',
                active_skills_json TEXT NOT NULL DEFAULT '[]',
                tool_rounds INTEGER NOT NULL DEFAULT 0,
                cwd TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS messages (
                session_id INTEGER NOT NULL,
                agent_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL DEFAULT '',
                reasoning_content TEXT NOT NULL DEFAULT '',
                tool_call_id TEXT,
                had_tool_calls INTEGER NOT NULL DEFAULT 0,
                tool_calls_json TEXT NOT NULL DEFAULT '[]',
                PRIMARY KEY (session_id, agent_id, seq)
            );
            CREATE TABLE IF NOT EXISTS subagents (
                session_id INTEGER NOT NULL,
                subagent_id TEXT NOT NULL,
                name TEXT NOT NULL DEFAULT '',
                task TEXT NOT NULL DEFAULT '',
                state TEXT NOT NULL DEFAULT 'idle',
                mode TEXT NOT NULL DEFAULT 'create',
                parent_call_id TEXT,
                result TEXT,
                error TEXT,
                PRIMARY KEY (session_id, subagent_id)
            );
            CREATE TABLE IF NOT EXISTS session_state (
                session_id INTEGER NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (session_id, key)
            );",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            conn.execute(&format!("PRAGMA user_version = {SCHEMA_VERSION}"), [])?;
        } else if version < SCHEMA_VERSION {
            // v1 -> v2: add the per-session working-directory column.
            if version < 2 {
                let has_cwd = conn
                    .prepare("PRAGMA table_info(sessions)")?
                    .query_map([], |r| r.get::<_, String>(1))?
                    .any(|col| col.as_deref() == Ok("cwd"));
                if !has_cwd {
                    conn.execute(
                        "ALTER TABLE sessions ADD COLUMN cwd TEXT NOT NULL DEFAULT ''",
                        [],
                    )?;
                }
            }
            conn.execute(&format!("PRAGMA user_version = {SCHEMA_VERSION}"), [])?;
        } else if version > SCHEMA_VERSION {
            return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "history database schema version {version} is newer than supported {SCHEMA_VERSION}"
                    ),
                ),
            )));
        }
        Ok(())
    }

    /// Create a new session row and return its id. The name must be unique;
    /// callers generate timestamp-based names. `cwd` records the working
    /// directory the session belongs to (sessions are listed per workspace).
    pub fn create_session(
        &self,
        name: &str,
        session_id: &str,
        model_id: &str,
        cwd: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO sessions (name, created_at, updated_at, session_id, model_id, cwd)
             VALUES (?1, ?2, ?2, ?3, ?4, ?5)",
            params![name, now, session_id, model_id, cwd],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Persist per-session metadata (upsert by id) and refresh `updated_at`.
    pub fn save_meta(&self, meta: &SessionMeta) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET name=?2, updated_at=?3, session_id=?4, model_id=?5,
             request_count=?6, usage_json=?7, active_skills_json=?8, tool_rounds=?9
             WHERE id=?1",
            params![
                meta.id,
                meta.name,
                chrono::Utc::now().timestamp_millis(),
                meta.session_id,
                meta.model_id,
                meta.request_count as i64,
                serde_json::to_string(&meta.usage).unwrap_or_default(),
                serde_json::to_string(&meta.active_skills).unwrap_or_default(),
                meta.tool_rounds as i64,
            ],
        )?;
        Ok(())
    }

    /// Replace all messages of one agent (main or subagent) within a session.
    pub fn replace_messages(
        &self,
        session: i64,
        agent_id: &str,
        messages: &[Message],
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE session_id=?1 AND agent_id=?2",
            params![session, agent_id],
        )?;
        for (seq, m) in messages.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages
                 (session_id, agent_id, seq, role, content, reasoning_content,
                  tool_call_id, had_tool_calls, tool_calls_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session,
                    agent_id,
                    seq as i64,
                    m.role.as_str(),
                    m.content,
                    m.reasoning_content,
                    m.tool_call_id,
                    m.had_tool_calls,
                    serde_json::to_string(&m.tool_calls).unwrap_or_default(),
                ],
            )?;
        }
        tx.commit()
    }

    /// Replace all subagent records and their messages within a session.
    pub fn replace_subagents(&self, session: i64, subs: &[SubagentSnapshot]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        for sub in subs {
            tx.execute(
                "DELETE FROM messages WHERE session_id=?1 AND agent_id=?2",
                params![session, sub.record.id],
            )?;
        }
        tx.execute(
            "DELETE FROM subagents WHERE session_id=?1",
            params![session],
        )?;
        for sub in subs {
            let r = &sub.record;
            tx.execute(
                "INSERT INTO subagents
                 (session_id, subagent_id, name, task, state, mode, parent_call_id, result, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session,
                    r.id,
                    r.name,
                    r.task,
                    r.state,
                    r.mode,
                    r.parent_call_id,
                    r.result,
                    r.error
                ],
            )?;
            for (seq, m) in sub.messages.iter().enumerate() {
                tx.execute(
                    "INSERT INTO messages
                     (session_id, agent_id, seq, role, content, reasoning_content,
                      tool_call_id, had_tool_calls, tool_calls_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        session,
                        r.id,
                        seq as i64,
                        m.role.as_str(),
                        m.content,
                        m.reasoning_content,
                        m.tool_call_id,
                        m.had_tool_calls,
                        serde_json::to_string(&m.tool_calls).unwrap_or_default(),
                    ],
                )?;
            }
        }
        tx.commit()
    }

    /// Replace the whole key/value state of a session.
    pub fn replace_state(&self, session: i64, state: &HashMap<String, String>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM session_state WHERE session_id=?1",
            params![session],
        )?;
        for (key, value) in state {
            tx.execute(
                "INSERT INTO session_state (session_id, key, value) VALUES (?1, ?2, ?3)",
                params![session, key, value],
            )?;
        }
        tx.commit()
    }

    /// List sessions of one working directory, newest first.
    pub fn list_sessions(&self, cwd: &str) -> Result<Vec<SessionSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, updated_at, model_id
             FROM sessions WHERE cwd = ?1 ORDER BY updated_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![cwd], |r| {
            Ok(SessionSummary {
                id: r.get(0)?,
                name: r.get(1)?,
                updated_at: r.get(2)?,
                model_id: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Find a session id by name within one working directory.
    pub fn find_session(&self, name: &str, cwd: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id FROM sessions WHERE name=?1 AND cwd=?2",
            params![name, cwd],
            |r| r.get::<_, i64>(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    }

    /// Load the full snapshot of a session, or `None` when the id is unknown.
    pub fn load_session(&self, id: i64) -> Result<Option<SessionSnapshot>> {
        let conn = self.conn.lock().unwrap();
        let meta = match conn.query_row(
            "SELECT id, name, created_at, updated_at, session_id, model_id,
                    request_count, usage_json, active_skills_json, tool_rounds
             FROM sessions WHERE id=?1",
            params![id],
            |r| {
                let usage_json: String = r.get(7)?;
                let skills_json: String = r.get(8)?;
                Ok(SessionMeta {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    created_at: r.get(2)?,
                    updated_at: r.get(3)?,
                    session_id: r.get(4)?,
                    model_id: r.get(5)?,
                    request_count: r.get::<_, i64>(6)? as u64,
                    usage: serde_json::from_str(&usage_json).unwrap_or_default(),
                    active_skills: serde_json::from_str(&skills_json).unwrap_or_default(),
                    tool_rounds: r.get::<_, i64>(9)? as usize,
                })
            },
        ) {
            Ok(meta) => meta,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e),
        };

        let mut snapshot = SessionSnapshot {
            meta,
            ..Default::default()
        };

        // Main agent messages.
        snapshot.messages = Self::load_agent_messages(&conn, id, MAIN_AGENT_ID)?;

        // Subagent records + messages.
        let mut stmt = conn.prepare(
            "SELECT subagent_id, name, task, state, mode, parent_call_id, result, error
             FROM subagents WHERE session_id=?1 ORDER BY rowid",
        )?;
        let records: Vec<SubagentRecord> = stmt
            .query_map(params![id], |r| {
                Ok(SubagentRecord {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    task: r.get(2)?,
                    state: r.get(3)?,
                    mode: r.get(4)?,
                    parent_call_id: r.get(5)?,
                    result: r.get(6)?,
                    error: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for record in records {
            let messages = Self::load_agent_messages(&conn, id, &record.id)?;
            snapshot
                .subagents
                .push(SubagentSnapshot { record, messages });
        }

        // Key/value state.
        let mut stmt = conn.prepare("SELECT key, value FROM session_state WHERE session_id=?1")?;
        let state: std::result::Result<HashMap<String, String>, _> = stmt
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect();
        snapshot.state = state?;
        Ok(Some(snapshot))
    }

    /// Load one agent's messages using an already-locked connection.
    ///
    /// Takes `&Connection` instead of `&self` so it can be called while the
    /// caller holds the store mutex (a `std::sync::Mutex` is not reentrant).
    fn load_agent_messages(
        conn: &Connection,
        session: i64,
        agent_id: &str,
    ) -> Result<Vec<Message>> {
        let mut stmt = conn.prepare(
            "SELECT role, content, reasoning_content, tool_call_id, had_tool_calls,
                    tool_calls_json
             FROM messages WHERE session_id=?1 AND agent_id=?2 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![session, agent_id], |r| {
            let role_str: String = r.get(0)?;
            let role = match role_str.as_str() {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" => Role::Tool,
                _ => Role::Event,
            };
            let tool_calls_json: String = r.get(5)?;
            Ok(Message {
                role,
                content: r.get(1)?,
                reasoning_content: r.get(2)?,
                tool_call_id: r.get(3)?,
                had_tool_calls: r.get(4)?,
                tool_calls: serde_json::from_str(&tool_calls_json).unwrap_or_default(),
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{STATE_PENDING_TOOL_CALLS, tool_calls_from_json, tool_calls_to_json};
    use super::*;
    use crate::llm::Usage;
    use crate::tool::ToolCall;

    fn sample_tool_call() -> ToolCall {
        ToolCall {
            id: "call-1".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"ls"}"#.to_string(),
        }
    }

    #[test]
    fn create_list_find_roundtrip() {
        let store = SessionStore::open_in_memory().unwrap();
        let id = store.create_session("s1", "catus-1", "m1", "/w").unwrap();
        let summaries = store.list_sessions("/w").unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "s1");
        assert_eq!(store.find_session("s1", "/w").unwrap(), Some(id));
        assert_eq!(store.find_session("nope", "/w").unwrap(), None);
    }

    #[test]
    fn duplicate_session_name_rejected() {
        let store = SessionStore::open_in_memory().unwrap();
        store.create_session("dup", "a", "m", "/w").unwrap();
        assert!(store.create_session("dup", "b", "m", "/w").is_err());
    }

    #[test]
    fn messages_roundtrip_with_tool_calls_and_reasoning() {
        let store = SessionStore::open_in_memory().unwrap();
        let id = store.create_session("s", "sid", "m", "/w").unwrap();

        let mut assistant = Message::assistant("partial");
        assistant.reasoning_content = "thinking".to_string();
        assistant.had_tool_calls = true;
        assistant.tool_calls = vec![sample_tool_call()];
        let messages = vec![
            Message::user("hi"),
            assistant,
            Message::tool("result body", "call-9"),
            Message::event("note"),
        ];
        store
            .replace_messages(id, MAIN_AGENT_ID, &messages)
            .unwrap();

        let snap = store.load_session(id).unwrap().unwrap();
        assert_eq!(snap.messages.len(), 4);
        assert_eq!(snap.messages[0].content, "hi");
        assert_eq!(snap.messages[1].reasoning_content, "thinking");
        assert!(snap.messages[1].had_tool_calls);
        assert_eq!(snap.messages[1].tool_calls, vec![sample_tool_call()]);
        assert_eq!(snap.messages[2].tool_call_id.as_deref(), Some("call-9"));
        assert_eq!(snap.messages[3].role, Role::Event);

        // Rewrite with fewer rows; the old ones must not come back.
        store
            .replace_messages(id, MAIN_AGENT_ID, &[Message::user("only")])
            .unwrap();
        let snap = store.load_session(id).unwrap().unwrap();
        assert_eq!(snap.messages.len(), 1);
        assert_eq!(snap.messages[0].content, "only");
    }

    #[test]
    fn subagents_and_state_roundtrip() {
        let store = SessionStore::open_in_memory().unwrap();
        let id = store.create_session("s", "sid", "m", "/w").unwrap();

        let record = SubagentRecord {
            id: "subagent-0-coder".to_string(),
            name: "coder".to_string(),
            task: "do things".to_string(),
            state: "streaming".to_string(),
            mode: "fork".to_string(),
            parent_call_id: Some("call-task".to_string()),
            result: None,
            error: None,
        };
        let subs = vec![SubagentSnapshot {
            record,
            messages: vec![Message::assistant("working")],
        }];
        store.replace_subagents(id, &subs).unwrap();

        let mut state = HashMap::new();
        state.insert(
            STATE_PENDING_TOOL_CALLS.to_string(),
            tool_calls_to_json(&[sample_tool_call()]),
        );
        state.insert(
            super::super::STATE_SHELL_CWD.to_string(),
            "/tmp".to_string(),
        );
        store.replace_state(id, &state).unwrap();

        let snap = store.load_session(id).unwrap().unwrap();
        assert_eq!(snap.subagents.len(), 1);
        assert_eq!(snap.subagents[0].record.name, "coder");
        assert_eq!(
            snap.subagents[0].record.parent_call_id.as_deref(),
            Some("call-task")
        );
        assert_eq!(snap.subagents[0].messages.len(), 1);
        assert_eq!(
            tool_calls_from_json(snap.state.get(STATE_PENDING_TOOL_CALLS).unwrap()),
            vec![sample_tool_call()]
        );
        assert_eq!(
            snap.state.get(super::super::STATE_SHELL_CWD).unwrap(),
            "/tmp"
        );
    }

    #[test]
    fn load_unknown_session_returns_none() {
        let store = SessionStore::open_in_memory().unwrap();
        assert!(store.load_session(12345).unwrap().is_none());
    }

    #[test]
    fn meta_roundtrip() {
        let store = SessionStore::open_in_memory().unwrap();
        let id = store.create_session("s", "sid", "m", "/w").unwrap();
        store
            .save_meta(&SessionMeta {
                id,
                name: "s".to_string(),
                created_at: 0,
                updated_at: 0,
                session_id: "catus-42".to_string(),
                model_id: "test-model".to_string(),
                request_count: 3,
                usage: Usage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cached_tokens: 10,
                },
                active_skills: vec!["demo".to_string()],
                tool_rounds: 7,
            })
            .unwrap();
        let snap = store.load_session(id).unwrap().unwrap();
        assert_eq!(snap.meta.session_id, "catus-42");
        assert_eq!(snap.meta.model_id, "test-model");
        assert_eq!(snap.meta.request_count, 3);
        assert_eq!(snap.meta.usage.prompt_tokens, 100);
        assert_eq!(snap.meta.active_skills, vec!["demo".to_string()]);
        assert_eq!(snap.meta.tool_rounds, 7);
    }

    #[test]
    fn list_sessions_orders_newest_first() {
        let store = SessionStore::open_in_memory().unwrap();
        let a = store.create_session("a", "x", "m", "/w").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _b = store.create_session("b", "x", "m", "/w").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Touch a so it becomes newest again.
        store
            .save_meta(&SessionMeta {
                id: a,
                name: "a".to_string(),
                created_at: 0,
                updated_at: 0,
                session_id: "x".to_string(),
                model_id: "m".to_string(),
                request_count: 0,
                usage: Usage::default(),
                active_skills: Vec::new(),
                tool_rounds: 0,
            })
            .unwrap();
        let names: Vec<String> = store
            .list_sessions("/w")
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn sessions_are_filtered_by_cwd() {
        let store = SessionStore::open_in_memory().unwrap();
        store.create_session("here", "x", "m", "/ws-a").unwrap();
        store.create_session("there", "x", "m", "/ws-b").unwrap();
        store.create_session("legacy", "x", "m", "").unwrap();

        let names: Vec<String> = store
            .list_sessions("/ws-a")
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["here"]);
        // The empty filter only matches legacy (pre-cwd) rows; the app never
        // lists with an empty working directory.
        assert_eq!(store.list_sessions("").unwrap().len(), 1);
        assert_eq!(store.find_session("here", "/ws-a").unwrap().is_some(), true);
        assert_eq!(store.find_session("here", "/ws-b").unwrap(), None);
    }

    #[test]
    fn v1_database_is_migrated_with_data_intact() {
        // Build a legacy v1 database by hand: no cwd column, user_version 1.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                session_id TEXT NOT NULL DEFAULT '',
                model_id TEXT NOT NULL DEFAULT '',
                request_count INTEGER NOT NULL DEFAULT 0,
                usage_json TEXT NOT NULL DEFAULT '{}',
                active_skills_json TEXT NOT NULL DEFAULT '[]',
                tool_rounds INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO sessions (name, created_at, updated_at) VALUES ('old', 1, 1);
            PRAGMA user_version = 1;",
        )
        .unwrap();
        drop(conn);

        // Opening migrates the schema and keeps the old row loadable.
        let store = SessionStore::open_path(&db).unwrap();
        let version: i64 = {
            let conn = store.conn.lock().unwrap();
            conn.query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(version, super::SCHEMA_VERSION);
        let id = store.find_session("old", "").unwrap().unwrap();
        let snap = store.load_session(id).unwrap().unwrap();
        assert_eq!(snap.meta.name, "old");
        // New sessions can still be created on the migrated database.
        store.create_session("new", "x", "m", "/ws").unwrap();
        assert_eq!(store.list_sessions("/ws").unwrap().len(), 1);
    }
}
