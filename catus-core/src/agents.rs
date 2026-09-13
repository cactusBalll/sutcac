//! Agent definitions: Markdown files with YAML frontmatter.
//!
//! Each agent is a single `.md` file (e.g. `coder.md`) containing YAML
//! frontmatter followed by Markdown body. The file name is the agent name.
//!
//! Supported frontmatter fields:
//! - `name`: optional, must match file name if present.
//! - `description`: short human-readable description (required).
//! - `model`: capability tier, `performance` or `efficient`.
//! - `tools`: list of allowed tool names; may include "inherit".
//! - `permission`: sutcac-sh permission string.
//! - `skills`: list of skill names; may include "inherit".
//! - `role`: special responsibility marker, `main` or `memory`. Agents with a
//!   special role are managed by catus itself (e.g. the memory subagent behind
//!   the Agent Memory subsystem); plain subagents omit the field.
//!
//! The tier maps to a concrete model through `[agent.models]` in config.toml.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::ModelTier;
use crate::frontmatter::{parse_frontmatter, split_frontmatter, validate_name};

/// Special responsibility assigned through an agent's `role` frontmatter
/// field. Plain subagents have no role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// The main agent (defined by `main.md`; role is implicit there).
    Main,
    /// The memory subagent managing the mdbook-based Agent Memory store.
    Memory,
}

impl AgentRole {
    /// Parse a role label. Only `main` and `memory` are accepted.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "main" => Ok(Self::Main),
            "memory" => Ok(Self::Memory),
            other => Err(format!(
                "unknown agent role '{}'; expected 'main' or 'memory'",
                other
            )),
        }
    }

    /// The role label as written in the frontmatter.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Memory => "memory",
        }
    }
}

/// Parsed metadata from an agent definition's YAML frontmatter.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct AgentFrontmatter {
    name: Option<String>,
    description: Option<String>,
    model: Option<String>,
    tools: Option<OneOrMany<String>>,
    permission: Option<String>,
    skills: Option<OneOrMany<String>>,
    role: Option<String>,
}

/// Helper accepting either a single value or a list.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::One(v) => vec![v],
            OneOrMany::Many(v) => v,
        }
    }
}

/// A discovered agent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    /// Agent identifier, matching the file stem.
    pub name: String,
    /// Short description for selecting an agent for a task.
    pub description: String,
    /// Optional capability tier (`performance` or `efficient`).
    pub model_tier: Option<ModelTier>,
    /// Allowed tool names. May contain "inherit".
    pub allowed_tools: Vec<String>,
    /// Optional sutcac-sh permission string.
    pub permission: Option<String>,
    /// Skill names to activate. May contain "inherit".
    pub skills: Vec<String>,
    /// Special responsibility marker (`main`/`memory`); `None` for plain
    /// subagents. `main.md` gets an implicit `Main` role.
    pub role: Option<AgentRole>,
    /// Markdown body used as the system prompt.
    pub body: String,
    /// Source file path.
    pub source_path: PathBuf,
}

impl AgentDefinition {
    /// Load an agent definition from a Markdown file.
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.is_file() {
            return Err(format!(
                "agent definition is not a file: {}",
                path.display()
            ));
        }

        let contents = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;

        let (frontmatter, body) = split_frontmatter(&contents)?;
        let frontmatter: AgentFrontmatter = parse_frontmatter(&frontmatter)?;

        let name = path
            .file_stem()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("invalid agent file name: {}", path.display()))?
            .to_string();
        validate_name(&name)?;

        if let Some(ref fm_name) = frontmatter.name {
            if fm_name != &name {
                return Err(format!(
                    "agent name '{}' does not match file name '{}'",
                    fm_name, name
                ));
            }
        }

        let description = frontmatter
            .description
            .ok_or_else(|| "missing required frontmatter field: description".to_string())?;
        if description.is_empty() {
            return Err("description must be non-empty".to_string());
        }
        if description.len() > 1024 {
            return Err("description must be at most 1024 characters".to_string());
        }

        let model_tier = frontmatter
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ModelTier::parse)
            .transpose()?;
        let allowed_tools = frontmatter
            .tools
            .map(|v| {
                v.into_vec()
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let permission = frontmatter
            .permission
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let skills = frontmatter
            .skills
            .map(|v| {
                v.into_vec()
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let role = match frontmatter.role.as_deref().map(str::trim) {
            None | Some("") => {
                // `main.md` carries an implicit main role.
                if name == "main" {
                    Some(AgentRole::Main)
                } else {
                    None
                }
            }
            Some(raw) => Some(AgentRole::parse(raw)?),
        };

        Ok(Self {
            name,
            description,
            model_tier,
            allowed_tools,
            permission,
            skills,
            role,
            body: body.to_string(),
            source_path: path.to_path_buf(),
        })
    }

    /// Whether this agent wants to inherit tools/skills from its parent.
    pub fn inherits_tools(&self) -> bool {
        self.allowed_tools.iter().any(|s| s == "inherit")
    }

    /// Whether this agent wants to inherit skills from its parent.
    pub fn inherits_skills(&self) -> bool {
        self.skills.iter().any(|s| s == "inherit")
    }

    /// Return tool names with "inherit" entries removed.
    pub fn explicit_tools(&self) -> Vec<String> {
        self.allowed_tools
            .iter()
            .filter(|s| s != &"inherit")
            .cloned()
            .collect()
    }

    /// Return skill names with "inherit" entries removed.
    pub fn explicit_skills(&self) -> Vec<String> {
        self.skills
            .iter()
            .filter(|s| s != &"inherit")
            .cloned()
            .collect()
    }
}

/// Registry of all discovered agent definitions.
#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    agents: Vec<AgentDefinition>,
    /// Names of agents temporarily disabled for dispatch (session-scoped).
    /// Special-role agents (`main`, `memory`) cannot be disabled. Restoring
    /// saved subagents ignores this flag so `/resume` keeps working.
    disabled: Vec<String>,
}

impl AgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            agents: Vec::new(),
            disabled: Vec::new(),
        }
    }

    /// Discover agent definitions under each search path.
    ///
    /// Each search path is expected to contain `.md` files. Errors for
    /// individual files are logged and skipped; fatal I/O errors are returned.
    pub fn discover(search_paths: &[PathBuf]) -> Result<Self, String> {
        let mut registry = Self::new();
        for base in search_paths {
            if !base.is_dir() {
                continue;
            }
            let entries = std::fs::read_dir(base)
                .map_err(|e| format!("failed to read agent directory {}: {}", base.display(), e))?;
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("failed to read entry in {}: {}", base.display(), e);
                        continue;
                    }
                };
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                match AgentDefinition::load(&path) {
                    Ok(agent) => {
                        tracing::info!("discovered agent '{}' at {}", agent.name, path.display());
                        registry.agents.push(agent);
                    }
                    Err(e) => {
                        tracing::warn!("skipping invalid agent at {}: {}", path.display(), e);
                    }
                }
            }
        }
        registry.agents.sort_by(|a, b| a.name.cmp(&b.name));
        registry.validate_roles()?;
        Ok(registry)
    }

    /// Return default agent search paths, mirroring the config-file search order.
    pub fn default_paths() -> Vec<PathBuf> {
        vec![workspace_agents_dir(), xdg_agents_dir()]
    }

    /// Return the agent with the given name, if any.
    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// Return the first agent carrying the given role, if any.
    pub fn get_by_role(&self, role: AgentRole) -> Option<&AgentDefinition> {
        self.agents.iter().find(|a| a.role == Some(role))
    }

    /// Ensure each special role is held by at most one agent.
    fn validate_roles(&self) -> Result<(), String> {
        for role in [AgentRole::Main, AgentRole::Memory] {
            let holders: Vec<&str> = self
                .agents
                .iter()
                .filter(|a| a.role == Some(role))
                .map(|a| a.name.as_str())
                .collect();
            if holders.len() > 1 {
                return Err(format!(
                    "role '{}' is claimed by multiple agents: {}",
                    role.as_str(),
                    holders.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// Return true if no agents were discovered.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Number of discovered agents.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Iterator over all agents.
    pub fn iter(&self) -> impl Iterator<Item = &AgentDefinition> {
        self.agents.iter()
    }

    /// Return the names and descriptions of all agents.
    pub fn names_and_descriptions(&self) -> Vec<(&str, &str)> {
        self.agents
            .iter()
            .map(|a| (a.name.as_str(), a.description.as_str()))
            .collect()
    }

    /// Whether an agent is temporarily disabled for dispatch.
    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled.iter().any(|n| n == name)
    }

    /// Disable or re-enable an agent for dispatch. Callers must refuse to
    /// disable special-role agents (`main`/`memory`).
    pub fn set_disabled(&mut self, name: &str, disabled: bool) {
        if disabled {
            if !self.is_disabled(name) {
                self.disabled.push(name.to_string());
            }
        } else {
            self.disabled.retain(|n| n != name);
        }
    }

    /// The disabled agent names (persisted as session state).
    pub fn disabled_names(&self) -> &[String] {
        &self.disabled
    }

    /// Restore the disabled set (session resume).
    pub fn set_disabled_names(&mut self, names: Vec<String>) {
        self.disabled = names;
    }
}

fn workspace_agents_dir() -> PathBuf {
    let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    path.push(".sutcac");
    path.push("agents");
    path
}

fn xdg_agents_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| {
        let mut home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.push(".config");
        home
    });
    let mut path = base;
    path.push("catus");
    path.push("agents");
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_agent(dir: &Path, name: &str, description: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let contents = format!(
            "---\nname: {}\ndescription: {}\n---\n{}",
            name, description, body
        );
        std::fs::write(dir.join(format!("{}.md", name)), contents).unwrap();
    }

    #[test]
    fn parse_valid_agent() {
        let dir = std::env::temp_dir().join(format!("catus_agent_valid_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_agent(&dir, "coder", "Writes code.", "You are a coder.");

        let agent = AgentDefinition::load(&dir.join("coder.md")).unwrap();
        assert_eq!(agent.name, "coder");
        assert_eq!(agent.description, "Writes code.");
        assert_eq!(agent.body, "You are a coder.");
        assert!(agent.allowed_tools.is_empty());
        assert!(agent.skills.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_missing_description() {
        let dir = std::env::temp_dir().join(format!("catus_agent_no_desc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.md"), "---\nname: bad\n---\nBody").unwrap();

        let err = AgentDefinition::load(&dir.join("bad.md")).unwrap_err();
        assert!(err.contains("description"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_mismatched_file_name() {
        let dir = std::env::temp_dir().join(format!("catus_agent_name_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("wrong.md"),
            "---\nname: other\ndescription: desc\n---\nBody",
        )
        .unwrap();

        let err = AgentDefinition::load(&dir.join("wrong.md")).unwrap_err();
        assert!(err.contains("does not match file name"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_sorts_agents() {
        let dir = std::env::temp_dir().join(format!("catus_agent_discover_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_agent(&dir, "z-agent", "Z.", "Z agent.");
        write_agent(&dir, "a-agent", "A.", "A agent.");

        let registry = AgentRegistry::discover(&[dir.clone()]).unwrap();
        assert_eq!(registry.len(), 2);
        let names: Vec<&str> = registry.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a-agent", "z-agent"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_tools_and_skills_as_one_or_many() {
        let dir = std::env::temp_dir().join(format!("catus_agent_lists_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("multi.md"),
            "---\nname: multi\ndescription: D.\ntools: [shell, edit]\nskills: [rust, inherit]\n---\nBody",
        )
        .unwrap();

        let agent = AgentDefinition::load(&dir.join("multi.md")).unwrap();
        assert_eq!(agent.allowed_tools, vec!["shell", "edit"]);
        assert_eq!(agent.skills, vec!["rust", "inherit"]);
        assert!(agent.inherits_skills());
        assert_eq!(agent.explicit_skills(), vec!["rust"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_single_tool_and_skill() {
        let dir = std::env::temp_dir().join(format!("catus_agent_single_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("single.md"),
            "---\nname: single\ndescription: D.\ntools: shell\nskills: rust\n---\nBody",
        )
        .unwrap();

        let agent = AgentDefinition::load(&dir.join("single.md")).unwrap();
        assert_eq!(agent.allowed_tools, vec!["shell"]);
        assert_eq!(agent.skills, vec!["rust"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_model_tier_frontmatter() {
        let dir = std::env::temp_dir().join(format!("catus_agent_tier_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fast.md"),
            "---\nname: fast\ndescription: D.\nmodel: efficient\n---\nBody",
        )
        .unwrap();

        let agent = AgentDefinition::load(&dir.join("fast.md")).unwrap();
        assert_eq!(agent.model_tier, Some(ModelTier::Efficient));

        // Unknown tier values make the definition invalid.
        std::fs::write(
            dir.join("bad.md"),
            "---\nname: bad\ndescription: D.\nmodel: turbo\n---\nBody",
        )
        .unwrap();
        let err = AgentDefinition::load(&dir.join("bad.md")).unwrap_err();
        assert!(err.contains("unknown model tier"), "unexpected: {}", err);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_role_frontmatter() {
        let dir = std::env::temp_dir().join(format!("catus_agent_role_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(
            dir.join("memory.md"),
            "---\nname: memory\ndescription: D.\nrole: memory\n---\nBody",
        )
        .unwrap();
        let agent = AgentDefinition::load(&dir.join("memory.md")).unwrap();
        assert_eq!(agent.role, Some(AgentRole::Memory));

        // main.md gets an implicit main role.
        std::fs::write(
            dir.join("main.md"),
            "---\nname: main\ndescription: D.\n---\nBody",
        )
        .unwrap();
        let agent = AgentDefinition::load(&dir.join("main.md")).unwrap();
        assert_eq!(agent.role, Some(AgentRole::Main));

        // Plain agents have no role.
        std::fs::write(
            dir.join("coder.md"),
            "---\nname: coder\ndescription: D.\n---\nBody",
        )
        .unwrap();
        let agent = AgentDefinition::load(&dir.join("coder.md")).unwrap();
        assert_eq!(agent.role, None);

        // Unknown role values make the definition invalid.
        std::fs::write(
            dir.join("bad.md"),
            "---\nname: bad\ndescription: D.\nrole: janitor\n---\nBody",
        )
        .unwrap();
        let err = AgentDefinition::load(&dir.join("bad.md")).unwrap_err();
        assert!(err.contains("unknown agent role"), "unexpected: {}", err);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_get_by_role_and_rejects_duplicates() {
        let dir =
            std::env::temp_dir().join(format!("catus_agent_roles_reg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_agent(&dir, "main", "Main.", "Main agent.");
        std::fs::write(
            dir.join("memory.md"),
            "---\nname: memory\ndescription: M.\nrole: memory\n---\nMemory agent.",
        )
        .unwrap();

        let registry = AgentRegistry::discover(&[dir.clone()]).unwrap();
        assert_eq!(
            registry
                .get_by_role(AgentRole::Memory)
                .map(|a| a.name.as_str()),
            Some("memory")
        );
        assert_eq!(
            registry
                .get_by_role(AgentRole::Main)
                .map(|a| a.name.as_str()),
            Some("main")
        );
        drop(registry);

        // A second memory-role agent makes discovery fail.
        std::fs::write(
            dir.join("other.md"),
            "---\nname: other\ndescription: O.\nrole: memory\n---\nBody",
        )
        .unwrap();
        let err = AgentRegistry::discover(&[dir.clone()]).unwrap_err();
        assert!(err.contains("memory"), "unexpected: {}", err);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
