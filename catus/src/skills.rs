//! Agent Skills discovery and loading.
//!
//! Implements the [Agent Skills](https://agentskills.io/specification) format:
//! each skill is a directory containing a `SKILL.md` file with YAML frontmatter
//! followed by Markdown instructions.  At startup only the metadata (name and
//! description) is loaded; the full instructions are read only when a skill is
//! activated.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Parsed metadata from a skill's `SKILL.md` frontmatter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillMetadata {
    /// Optional license name or reference.
    pub license: Option<String>,
    /// Optional environment/compatibility note.
    pub compatibility: Option<String>,
    /// Arbitrary string key/value metadata.
    pub metadata: HashMap<String, String>,
    /// Optional space-separated list of pre-approved tools.
    pub allowed_tools: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    license: Option<String>,
    compatibility: Option<String>,
    #[serde(rename = "allowed-tools")]
    allowed_tools: Option<String>,
    metadata: HashMap<String, String>,
}

/// A discovered Agent Skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Skill identifier, matching the parent directory name.
    pub name: String,
    /// Short description for matching skills to tasks.
    pub description: String,
    /// Root directory of the skill.
    pub root: PathBuf,
    /// Parsed frontmatter metadata.
    pub meta: SkillMetadata,
    /// Full Markdown instructions, loaded lazily.
    pub instructions: Option<String>,
}

impl Skill {
    /// Load a skill from its directory.
    ///
    /// Only metadata is parsed eagerly; instructions remain on disk until
    /// [`Skill::load_instructions`] is called.
    pub fn load(root: &Path) -> Result<Self, String> {
        let skill_md = root.join("SKILL.md");
        if !skill_md.is_file() {
            return Err(format!(
                "{} does not contain a SKILL.md file",
                root.display()
            ));
        }

        let contents = std::fs::read_to_string(&skill_md)
            .map_err(|e| format!("failed to read {}: {}", skill_md.display(), e))?;

        let (frontmatter, _body) = split_frontmatter(&contents)?;
        let frontmatter: Frontmatter = parse_frontmatter(&frontmatter)?;

        let name = frontmatter
            .name
            .as_ref()
            .ok_or_else(|| "missing required frontmatter field: name".to_string())?
            .clone();
        validate_name(&name)?;

        let expected_dir_name = root.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name != expected_dir_name {
            return Err(format!(
                "skill name '{}' does not match directory name '{}'",
                name, expected_dir_name
            ));
        }

        let description = frontmatter
            .description
            .as_ref()
            .ok_or_else(|| "missing required frontmatter field: description".to_string())?
            .clone();
        if description.is_empty() {
            return Err("description must be non-empty".to_string());
        }
        if description.len() > 1024 {
            return Err("description must be at most 1024 characters".to_string());
        }

        let meta = SkillMetadata {
            license: frontmatter.license,
            compatibility: frontmatter.compatibility,
            metadata: frontmatter.metadata,
            allowed_tools: frontmatter.allowed_tools,
        };

        Ok(Self {
            name,
            description,
            root: root.to_path_buf(),
            meta,
            instructions: None,
        })
    }

    /// Return the full instructions, reading from disk if necessary.
    pub fn load_instructions(&mut self) -> Result<&str, String> {
        if self.instructions.is_none() {
            let path = self.root.join("SKILL.md");
            let contents = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
            let (_, body) = split_frontmatter(&contents)?;
            self.instructions = Some(body.to_string());
        }
        Ok(self.instructions.as_deref().unwrap_or(""))
    }

    /// Return instructions if already loaded, without touching disk.
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }
}

/// A registry of all discovered skills.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { skills: Vec::new() }
    }

    /// Discover skills under each search path.
    ///
    /// Each search path is expected to contain skill subdirectories.  Errors
    /// for individual skills are logged and skipped; only fatal I/O errors are
    /// returned.
    pub fn discover(search_paths: &[PathBuf]) -> Result<Self, String> {
        let mut registry = Self::new();
        for base in search_paths {
            if !base.is_dir() {
                continue;
            }
            let entries = std::fs::read_dir(base)
                .map_err(|e| format!("failed to read skill directory {}: {}", base.display(), e))?;
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("failed to read entry in {}: {}", base.display(), e);
                        continue;
                    }
                };
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                match Skill::load(&path) {
                    Ok(skill) => {
                        log::info!("discovered skill '{}' at {}", skill.name, path.display());
                        registry.skills.push(skill);
                    }
                    Err(e) => {
                        log::warn!("skipping invalid skill at {}: {}", path.display(), e);
                    }
                }
            }
        }
        registry.skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(registry)
    }

    /// Return default skill search paths, mirroring the config-file search order.
    pub fn default_paths() -> Vec<PathBuf> {
        vec![workspace_skills_dir(), xdg_skills_dir()]
    }

    /// Return the skill with the given name, if any.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Return a mutable reference to the skill with the given name.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Skill> {
        self.skills.iter_mut().find(|s| s.name == name)
    }

    /// Activate a skill, loading its full instructions.
    pub fn activate(&mut self, name: &str) -> Result<Option<&Skill>, String> {
        if let Some(skill) = self.get_mut(name) {
            skill.load_instructions()?;
            return Ok(Some(&*skill));
        }
        Ok(None)
    }

    /// Return the names and descriptions of all discovered skills.
    pub fn names_and_descriptions(&self) -> Vec<(&str, &str)> {
        self.skills
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str()))
            .collect()
    }

    /// Return true if no skills were discovered.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Number of discovered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Iterator over all skills.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter()
    }
}

fn workspace_skills_dir() -> PathBuf {
    let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    path.push(".sutcac");
    path.push("skills");
    path
}

fn xdg_skills_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| {
        let mut home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.push(".config");
        home
    });
    let mut path = base;
    path.push("catus");
    path.push("skills");
    path
}

/// Split file contents into frontmatter and body.
///
/// Frontmatter must start with `---` on its own line and end with a matching
/// `---` line.  If there is no frontmatter, the whole file is treated as body.
fn split_frontmatter(contents: &str) -> Result<(String, String), String> {
    let trimmed = contents.trim_start();
    if !trimmed.starts_with("---") {
        return Ok((String::new(), contents.to_string()));
    }

    // Find the end delimiter, skipping the first line.
    let after_open = &trimmed[3..];
    let mut rest_lines = after_open.lines();
    let first = rest_lines.next().unwrap_or("");
    if !first.trim().is_empty() {
        return Err("frontmatter opening delimiter must be on its own line".to_string());
    }

    let mut frontmatter_lines = Vec::new();
    let mut found_close = false;
    for line in rest_lines {
        if line.trim() == "---" {
            found_close = true;
            break;
        }
        frontmatter_lines.push(line);
    }

    if !found_close {
        return Err("unclosed frontmatter delimiter".to_string());
    }

    let body_lines: Vec<&str> = after_open
        .lines()
        .skip(frontmatter_lines.len() + 2) // +2 for opening blank + closing delimiter
        .collect();

    Ok((frontmatter_lines.join("\n"), body_lines.join("\n")))
}

fn parse_frontmatter(frontmatter: &str) -> Result<Frontmatter, String> {
    if frontmatter.trim().is_empty() {
        return Ok(Frontmatter::default());
    }
    serde_saphyr::from_str(frontmatter).map_err(|e| format!("invalid YAML frontmatter: {}", e))
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name.len() > 64 {
        return Err("name must be at most 64 characters".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        return Err("name must not contain consecutive hyphens".to_string());
    }
    for ch in name.chars() {
        if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '-' {
            return Err(format!(
                "name contains invalid character '{}'; only lowercase letters, digits, and hyphens are allowed",
                ch
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
        std::fs::create_dir_all(root).unwrap();
        let contents = format!(
            "---\nname: {}\ndescription: {}\n---\n{}",
            name, description, body
        );
        std::fs::write(root.join("SKILL.md"), contents).unwrap();
    }

    #[test]
    fn parse_valid_skill() {
        let dir = std::env::temp_dir().join(format!("catus_skill_valid_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let skill_dir = dir.join("my-skill");
        write_skill(
            &skill_dir,
            "my-skill",
            "Does a thing.",
            "# Instructions\nRun it.",
        );

        let mut skill = Skill::load(&skill_dir).unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "Does a thing.");
        assert!(skill.instructions().is_none());
        assert_eq!(
            skill.load_instructions().unwrap(),
            "# Instructions\nRun it."
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_mismatched_directory_name() {
        let dir =
            std::env::temp_dir().join(format!("catus_skill_name_mismatch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let skill_dir = dir.join("wrong-name");
        write_skill(&skill_dir, "my-skill", "Does a thing.", "");

        let err = Skill::load(&skill_dir).unwrap_err();
        assert!(err.contains("does not match directory name"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_invalid_name() {
        let dir = std::env::temp_dir().join(format!("catus_skill_invalid_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let skill_dir = dir.join("BadName");
        write_skill(&skill_dir, "BadName", "Does a thing.", "");

        let err = Skill::load(&skill_dir).unwrap_err();
        assert!(err.contains("invalid character"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_sorts_skills() {
        let dir = std::env::temp_dir().join(format!("catus_skill_discover_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        write_skill(&dir.join("z-skill"), "z-skill", "Z skill.", "");
        write_skill(&dir.join("a-skill"), "a-skill", "A skill.", "");

        let registry = SkillRegistry::discover(&[dir.clone()]).unwrap();
        assert_eq!(registry.len(), 2);
        let names: Vec<&str> = registry.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a-skill", "z-skill"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn activate_loads_instructions() {
        let dir = std::env::temp_dir().join(format!("catus_skill_activate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let skill_dir = dir.join("lazy-skill");
        write_skill(&skill_dir, "lazy-skill", "Lazy.", "Detailed instructions.");

        let mut registry = SkillRegistry::discover(&[dir.clone()]).unwrap();
        let skill = registry.get("lazy-skill").unwrap();
        assert!(skill.instructions().is_none());

        registry.activate("lazy-skill").unwrap();
        let skill = registry.get("lazy-skill").unwrap();
        assert_eq!(skill.instructions(), Some("Detailed instructions."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_optional_metadata() {
        let dir = std::env::temp_dir().join(format!("catus_skill_meta_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let skill_dir = dir.join("meta-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: meta-skill
description: A skill with metadata.
license: MIT
compatibility: Requires git.
metadata:
  author: test
  version: "1.0"
allowed-tools: Bash(git:*) Read
---
Body here.
"#,
        )
        .unwrap();

        let skill = Skill::load(&skill_dir).unwrap();
        assert_eq!(skill.meta.license.as_deref(), Some("MIT"));
        assert_eq!(skill.meta.compatibility.as_deref(), Some("Requires git."));
        assert_eq!(
            skill.meta.metadata.get("author").map(|s| s.as_str()),
            Some("test")
        );
        assert_eq!(
            skill.meta.allowed_tools.as_deref(),
            Some("Bash(git:*) Read")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
