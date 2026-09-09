//! Embedded default resources for catus.
//!
//! The default project configuration (config template, agent definitions and
//! skills) lives in `catus/resources/` and is embedded into the binary with
//! `include_str!`. `catus --install-project-config` dumps these files into
//! `./.sutcac/` so a fresh project only needs its provider credentials filled
//! in before catus can run.
//!
//! Existing files are never overwritten: the installer skips them so user
//! edits (and API keys) survive a re-run.

use std::path::{Path, PathBuf};

/// Default config template. Fill in the provider and model entries and catus
/// runs directly.
pub const CONFIG_TOML: &str = include_str!("../resources/config.toml");
/// Default main agent definition.
pub const AGENT_MAIN_MD: &str = include_str!("../resources/agents/main.md");
/// Default coder subagent definition.
pub const AGENT_CODER_MD: &str = include_str!("../resources/agents/coder.md");
/// Default memory subagent definition (Agent Memory subsystem).
pub const AGENT_MEMORY_MD: &str = include_str!("../resources/agents/memory.md");
/// Example skill: git commit workflow.
pub const SKILL_COMMIT_MD: &str = include_str!("../resources/skills/commit/SKILL.md");

/// Files written by [`install_project_config`] / [`install_into`], relative
/// to the install root (`.sutcac/` or the XDG directory).
pub const PROJECT_FILES: &[(&str, &str)] = &[
    ("config.toml", CONFIG_TOML),
    ("agents/main.md", AGENT_MAIN_MD),
    ("agents/coder.md", AGENT_CODER_MD),
    ("agents/memory.md", AGENT_MEMORY_MD),
    ("skills/commit/SKILL.md", SKILL_COMMIT_MD),
];

/// What happened to a single installed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallAction {
    /// File did not exist and was written.
    Written,
    /// File already existed and was left untouched.
    Skipped,
}

/// Result of one [`install_project_config`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// Target directory the files were installed into.
    pub target_dir: PathBuf,
    /// Per-file outcome, in [`PROJECT_FILES`] order.
    pub files: Vec<(&'static str, InstallAction)>,
}

impl InstallReport {
    /// Number of files written by this run.
    pub fn written(&self) -> usize {
        self.files
            .iter()
            .filter(|(_, a)| *a == InstallAction::Written)
            .count()
    }

    /// Number of existing files skipped by this run.
    pub fn skipped(&self) -> usize {
        self.files
            .iter()
            .filter(|(_, a)| *a == InstallAction::Skipped)
            .count()
    }
}

/// Dump the embedded default resources into `<target_root>/.sutcac/`.
///
/// Missing parent directories are created; files that already exist are
/// skipped so user configuration is never overwritten.
pub fn install_project_config(target_root: &Path) -> std::io::Result<InstallReport> {
    install_into(&target_root.join(".sutcac"))
}

/// Initialize the XDG configuration directory from the embedded resources.
///
/// Called by [`crate::config::AppConfig::load`] when
/// `~/.config/catus/` (see [`crate::config::xdg_catus_dir`]) does not exist
/// yet; also used by `catus --install-project-config` for opt-in installs.
pub fn install_xdg_config() -> std::io::Result<InstallReport> {
    let dir = crate::config::xdg_catus_dir();
    std::fs::create_dir_all(&dir)?;
    install_into(&dir)
}

/// Dump the embedded default resources directly into `target_dir`.
///
/// Missing parent directories are created; files that already exist are
/// skipped so user configuration is never overwritten.
pub fn install_into(target_dir: &Path) -> std::io::Result<InstallReport> {
    let mut files = Vec::new();

    for (relative, contents) in PROJECT_FILES {
        let path = target_dir.join(relative);
        let action = if path.exists() {
            InstallAction::Skipped
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, contents)?;
            InstallAction::Written
        };
        files.push((*relative, action));
    }

    Ok(InstallReport {
        target_dir: target_dir.to_path_buf(),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_resources_are_non_empty() {
        assert!(CONFIG_TOML.contains("[[providers]]"));
        assert!(CONFIG_TOML.contains("[[models]]"));
        assert!(CONFIG_TOML.contains("[agent.models]"));
        assert!(AGENT_MAIN_MD.starts_with("---\nname: main\n"));
        assert!(AGENT_CODER_MD.starts_with("---\nname: coder\n"));
        assert!(AGENT_MEMORY_MD.starts_with("---\nname: memory\n"));
        assert!(AGENT_MEMORY_MD.contains("role: memory"));
        assert!(SKILL_COMMIT_MD.starts_with("---\nname: commit\n"));
    }

    #[test]
    fn install_writes_all_files_once_and_skips_on_rerun() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let report = install_project_config(root).unwrap();
        assert_eq!(report.target_dir, root.join(".sutcac"));
        assert_eq!(report.written(), PROJECT_FILES.len());
        assert_eq!(report.skipped(), 0);
        for (relative, _) in PROJECT_FILES {
            assert!(root.join(".sutcac").join(relative).is_file());
        }

        // A second run must not overwrite anything.
        let report = install_project_config(root).unwrap();
        assert_eq!(report.written(), 0);
        assert_eq!(report.skipped(), PROJECT_FILES.len());
    }

    #[test]
    fn install_into_writes_directly_into_target_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("custom-root");
        let report = install_into(&target).unwrap();
        assert_eq!(report.target_dir, target);
        assert_eq!(report.written(), PROJECT_FILES.len());
        for (relative, _) in PROJECT_FILES {
            assert!(target.join(relative).is_file());
        }
        // Re-running keeps existing files.
        let report = install_into(&target).unwrap();
        assert_eq!(report.skipped(), PROJECT_FILES.len());
    }

    #[test]
    fn installed_config_parses_and_validates() {
        let cfg: crate::config::AppConfig = toml::from_str(CONFIG_TOML).unwrap();
        cfg.resolve_models().unwrap();
        cfg.validate_tier_models().unwrap();
    }

    #[test]
    fn embedded_agent_definitions_parse() {
        for (name, contents) in [
            ("main", AGENT_MAIN_MD),
            ("coder", AGENT_CODER_MD),
            ("memory", AGENT_MEMORY_MD),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("{}.md", name));
            std::fs::write(&path, contents).unwrap();
            crate::agents::AgentDefinition::load(&path).unwrap();
        }
    }
}
