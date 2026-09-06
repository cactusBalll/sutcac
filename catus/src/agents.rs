//! Agent definitions: Markdown files with YAML frontmatter.
//!
//! Each agent is a single `.md` file (e.g. `coder.md`) containing YAML
//! frontmatter followed by Markdown body. The file name is the agent name.
//!
//! Supported frontmatter fields:
//! - `name`: optional, must match file name if present.
//! - `description`: short human-readable description (required).
//! - `model`: tier label such as "性能" or "效率".
//! - `tools`: list of allowed tool names; may include "inherit".
//! - `permission`: sutcac-sh permission string.
//! - `skills`: list of skill names; may include "inherit".

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::frontmatter::{parse_frontmatter, split_frontmatter, validate_name};

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
    /// Optional model tier label (e.g. "性能", "效率").
    pub model_tier: Option<String>,
    /// Allowed tool names. May contain "inherit".
    pub allowed_tools: Vec<String>,
    /// Optional sutcac-sh permission string.
    pub permission: Option<String>,
    /// Skill names to activate. May contain "inherit".
    pub skills: Vec<String>,
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
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
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

        Ok(Self {
            name,
            description,
            model_tier,
            allowed_tools,
            permission,
            skills,
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
}

impl AgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { agents: Vec::new() }
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
                        log::warn!("failed to read entry in {}: {}", base.display(), e);
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
                        log::info!("discovered agent '{}' at {}", agent.name, path.display());
                        registry.agents.push(agent);
                    }
                    Err(e) => {
                        log::warn!("skipping invalid agent at {}: {}", path.display(), e);
                    }
                }
            }
        }
        registry.agents.sort_by(|a, b| a.name.cmp(&b.name));
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
}
