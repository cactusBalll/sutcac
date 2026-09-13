//! Scope-aware config editing (`set_config_field_in` / `remove_config_field_in`).
//!
//! These tests run as their own process (integration test) because they
//! mutate process-global state: the current directory (the workspace config
//! is resolved against it) and `XDG_CONFIG_HOME` (the global config path).
//! In-process unit tests would race the history/resume tests, which depend
//! on the cwd.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use catus_core::app::App;
use catus_core::config::{AgentConfig, AppConfig, AppDirs, ConfigScope, ModelEntry};
use catus_core::llm::Provider;

/// Serialize the cwd/XDG mutation across this binary's tests (only one).
static ISOLATION_LOCK: Mutex<()> = Mutex::new(());

fn test_config(dir: &Path) -> AppConfig {
    AppConfig {
        providers: vec![Provider {
            name: "test".to_string(),
            base_url: "https://example.com".to_string(),
            api_key: "test".to_string(),
            session_header: None,
        }],
        models: vec![ModelEntry {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            context_window: 4096,
            provider: "test".to_string(),
        }],
        agent: AgentConfig::default(),
        shell: None,
        mcp: None,
        rag: None,
        dirs: AppDirs {
            history: None,
            memory: dir.join("memory"),
        },
    }
}

/// Guard restoring the cwd and `XDG_CONFIG_HOME` on drop.
struct Isolation {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous_xdg: Option<std::ffi::OsString>,
    previous_cwd: PathBuf,
}

impl Drop for Isolation {
    fn drop(&mut self) {
        match self.previous_xdg.take() {
            // SAFETY: tests in this binary are serialized by the lock.
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        std::env::set_current_dir(&self.previous_cwd).unwrap();
    }
}

fn isolate(root: &Path) -> Isolation {
    let lock = ISOLATION_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let xdg = root.join("xdg");
    let catus = xdg.join("catus");
    std::fs::create_dir_all(&catus).unwrap();
    let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
    let previous_cwd = std::env::current_dir().unwrap();
    // SAFETY: tests in this binary are serialized by the lock.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
    std::env::set_current_dir(root).unwrap();
    Isolation {
        _lock: lock,
        previous_xdg,
        previous_cwd,
    }
}

#[test]
fn config_scope_editing_respects_precedence() {
    let root = std::env::temp_dir().join(format!("catus_config_scope_it_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _isolation = isolate(&root);
    std::fs::write(
        catus_core::config::xdg_config_path(),
        "# global base\n[agent]\nmax_tool_rounds = 7\nlog_level = \"info\"\n",
    )
    .unwrap();

    let mut app = App::new(test_config(&root));

    // 1. Snapshot: global file exists with its values, workspace not yet.
    let scopes = app.config_scopes();
    assert_eq!(scopes.len(), 2);
    assert!(!scopes[0].exists, "workspace config should not exist yet");
    assert!(scopes[1].exists, "global config should exist");
    let rounds = scopes[1]
        .fields
        .iter()
        .find(|(k, _)| k == "agent.max_tool_rounds")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(rounds, "7");

    // 2. Setting a workspace field creates the file and wins at runtime.
    let msg = app
        .set_config_field_in(ConfigScope::Workspace, "agent.max_tool_rounds", "12")
        .unwrap();
    assert!(msg.contains("workspace override"));
    let ws_path = catus_core::config::workspace_config_path();
    assert!(ws_path.exists());
    let ws_text = std::fs::read_to_string(&ws_path).unwrap();
    assert!(ws_text.contains("max_tool_rounds = 12"));
    assert_eq!(app.config.agent.max_tool_rounds, 12);
    assert_eq!(app.max_tool_rounds, 12);

    // Untouched content in the global file survives scope writes.
    let global_text = std::fs::read_to_string(catus_core::config::xdg_config_path()).unwrap();
    assert!(global_text.contains("# global base"));

    // 3. Writing the global key while the workspace overrides it leaves the
    // effective value untouched.
    let msg = app
        .set_config_field_in(ConfigScope::Global, "agent.max_tool_rounds", "99")
        .unwrap();
    assert!(msg.contains("overrides"));
    assert_eq!(app.config.agent.max_tool_rounds, 12);
    let global_text = std::fs::read_to_string(catus_core::config::xdg_config_path()).unwrap();
    assert!(global_text.contains("max_tool_rounds = 99"));

    // 4. Removing the workspace override falls back to the global value.
    let msg = app
        .remove_config_field_in(ConfigScope::Workspace, "agent.max_tool_rounds")
        .unwrap();
    assert!(msg.contains("effective value: 99"));
    let ws_text = std::fs::read_to_string(&ws_path).unwrap();
    assert!(!ws_text.contains("max_tool_rounds"));
    assert_eq!(app.config.agent.max_tool_rounds, 99);
    assert_eq!(app.max_tool_rounds, 99);

    // 5. Removing the global value falls back to the built-in default.
    app.remove_config_field_in(ConfigScope::Global, "agent.max_tool_rounds")
        .unwrap();
    assert_eq!(
        app.config.agent.max_tool_rounds,
        AppConfig::default().agent.max_tool_rounds
    );

    // 6. Invalid values are rejected before anything is written.
    let err = app
        .set_config_field_in(ConfigScope::Workspace, "agent.max_tool_rounds", "abc")
        .unwrap_err();
    assert!(err.to_string().contains("expects a number"));

    // 7. Unknown keys are rejected.
    let err = app
        .set_config_field_in(ConfigScope::Workspace, "nope.key", "1")
        .unwrap_err();
    assert!(err.to_string().contains("unknown config field"));

    let _ = std::fs::remove_dir_all(&root);
}

/// The extended field set: bool/list conversion, runtime effects (memory
/// flags, shell path restrictions), and precedence on global writes.
#[test]
fn config_extended_fields_effects() {
    let root = std::env::temp_dir().join(format!("catus_config_fields_it_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _isolation = isolate(&root);
    std::fs::write(
        catus_core::config::xdg_config_path(),
        "[agent]\nmax_tool_rounds = 7\n",
    )
    .unwrap();

    let mut app = App::new(test_config(&root));

    // Specs cover the extended field set with their kinds.
    let specs = app.config_field_specs();
    let kinds: Vec<(&str, String)> = specs
        .iter()
        .map(|s| (s.key, format!("{:?}", s.kind).to_lowercase()))
        .collect();
    assert!(kinds.contains(&("agent.auto_include_skills", "bool".to_string())));
    assert!(kinds.contains(&("shell.read_paths", "list".to_string())));
    assert!(kinds.contains(&("agent.models.performance", "string".to_string())));
    assert!(kinds.contains(&("agent.models.efficient", "string".to_string())));
    assert_eq!(kinds.len(), 11);

    // Effective display lists every spec key.
    let fields = app.config_fields();
    assert_eq!(fields.len(), 11);

    // 1. bool field with a runtime effect (read live per dispatch).
    app.set_config_field_in(ConfigScope::Workspace, "agent.memory.auto_recall", "false")
        .unwrap();
    assert!(!app.config.agent.memory.auto_recall);

    // 2. list field rebuilds the shell policy (paths are canonicalized, so
    // the directories must exist).
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    let msg = app
        .set_config_field_in(
            ConfigScope::Workspace,
            "shell.read_paths",
            &format!("{}, {}", root.join("a").display(), root.join("b").display()),
        )
        .unwrap();
    assert!(msg.contains("workspace override"));
    let expected = vec![
        root.join("a").display().to_string(),
        root.join("b").display().to_string(),
    ];
    let shell = app.config.shell.as_ref().unwrap();
    assert_eq!(shell.read_paths, Some(expected.clone()));
    let summary = app.permission_summary();
    assert!(summary.contains("read: 2 dir(s)"), "summary: {}", summary);

    // 3. writing the same key globally while workspace overrides it does not
    // change the runtime, and the message says so.
    let msg = app
        .set_config_field_in(ConfigScope::Global, "shell.read_paths", "/elsewhere")
        .unwrap();
    assert!(msg.contains("overrides"));
    assert_eq!(
        app.config.shell.as_ref().unwrap().read_paths,
        Some(expected)
    );

    // 4. removing the workspace list falls back to the global value
    // (written in step 3).
    app.remove_config_field_in(ConfigScope::Workspace, "shell.read_paths")
        .unwrap();
    let shell = app.config.shell.as_ref().unwrap();
    assert_eq!(shell.read_paths, Some(vec!["/elsewhere".to_string()]));

    // 5. tier fields validate the reference against the configured
    // `[[models]]` list before anything is written.
    let msg = app
        .set_config_field_in(
            ConfigScope::Workspace,
            "agent.models.performance",
            "test-model",
        )
        .unwrap();
    assert!(msg.contains("workspace override"));
    assert_eq!(app.config.agent.models.performance, "test-model");

    let err = app
        .set_config_field_in(ConfigScope::Workspace, "agent.models.efficient", "nosuch")
        .unwrap_err();
    assert!(err.to_string().contains("does not match any configured"));

    app.set_config_field_in(
        ConfigScope::Workspace,
        "agent.models.efficient",
        "test-model",
    )
    .unwrap();
    assert_eq!(
        app.config.agent.models.efficient.as_deref(),
        Some("test-model")
    );

    // Removing the efficient tier falls back to the performance model.
    app.remove_config_field_in(ConfigScope::Workspace, "agent.models.efficient")
        .unwrap();
    assert_eq!(app.config.agent.models.efficient, None);

    // 6. invalid bool values are rejected before anything is written.
    let err = app
        .set_config_field_in(ConfigScope::Workspace, "agent.memory.auto_recall", "yes")
        .unwrap_err();
    assert!(err.to_string().contains("expects true or false"));

    let _ = std::fs::remove_dir_all(&root);
}
