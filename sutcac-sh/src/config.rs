//! Configuration loader for sutcac-sh.
//!
//! Reads the `[shell]` section from a single TOML configuration file.
//! The file is searched in this order:
//!
//! 1. `./.sutcac/config.toml` (current working directory)
//! 2. `$XDG_CONFIG_HOME/catus/config.toml`
//! 3. `~/.config/catus/config.toml`
//!
//! If no file is found, sensible defaults are used (allow all, no audit log).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::audit::{AuditFormat, AuditLogger};
use crate::permissions::{Permission, PermissionPolicy, PermissionSet};

/// The `[shell]` section of the shared TOML configuration file.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ShellConfig {
    /// Permission mode string, e.g. `allow_all`, `deny:write`, `allow:read`.
    pub perm_mode: Option<String>,
    /// Optional path to an audit log file.
    pub audit_log: Option<String>,
    /// Audit log format: `text` or `json`.
    pub audit_format: Option<String>,
    /// Optional structured metadata appended to every audit log entry.
    pub audit_meta: Option<HashMap<String, String>>,
    /// Per-command permission overrides. Each command maps to a list of tags
    /// such as `["read"]`, `["read", "network"]` or `["custom_tag"]`.
    pub commands: Option<HashMap<String, Vec<String>>>,
    /// Shorthand for read-only commands: `read = ["awk", "cat", ...]` is
    /// equivalent to listing each command under `[shell.commands]` with
    /// `tags = ["read"]`. Entries in `commands` take precedence.
    pub read: Option<Vec<String>>,
    /// Shorthand for writable commands: `write = ["mkdir", "touch", ...]` is
    /// equivalent to listing each command under `[shell.commands]` with
    /// `tags = ["write"]`. Entries in `commands` take precedence.
    pub write: Option<Vec<String>>,
    /// Allowed read-only paths. Commands that read files may only access paths
    /// inside these directories (or inside `write_paths`). Empty means no restriction.
    pub read_paths: Option<Vec<String>>,
    /// Allowed read-write paths. Commands that write files may only access paths
    /// inside these directories. Empty means no restriction.
    pub write_paths: Option<Vec<String>>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            perm_mode: Some("allow_all".to_string()),
            audit_log: None,
            audit_format: Some("text".to_string()),
            audit_meta: None,
            commands: None,
            read: None,
            write: None,
            read_paths: None,
            write_paths: None,
        }
    }
}

/// Wrapper for the full TOML file. Unknown fields are ignored so that
/// `catus`-specific settings can live in the same file.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct ConfigFile {
    pub shell: Option<ShellConfig>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self { shell: None }
    }
}

impl ShellConfig {
    /// Load the shell configuration from the first available config file.
    /// Returns `None` if no file is found.
    pub fn load() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        if let Some(path) = Self::find_config_file() {
            let contents = std::fs::read_to_string(&path)?;
            let file: ConfigFile = toml::from_str(&contents)?;
            Ok(file.shell)
        } else {
            Ok(None)
        }
    }

    /// Search for the shared TOML config file in the standard locations.
    pub fn find_config_file() -> Option<PathBuf> {
        let candidates = [Self::workspace_config(), Self::xdg_config()];
        for path in &candidates {
            if path.exists() {
                return Some(path.clone());
            }
        }
        None
    }

    fn workspace_config() -> PathBuf {
        let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        path.push(".sutcac");
        path.push("config.toml");
        path
    }

    fn xdg_config() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| {
            let mut home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            home.push(".config");
            home
        });
        let mut path = base;
        path.push("catus");
        path.push("config.toml");
        path
    }

    /// Build a permission policy from the configured `perm_mode` and
    /// per-command permission overrides.
    pub fn permission_policy(&self) -> PermissionPolicy {
        let command_permissions = self.parse_command_permissions();
        let mut policy = match &self.perm_mode {
            Some(s) => PermissionPolicy::parse_with_commands(s, command_permissions)
                .unwrap_or_else(PermissionPolicy::allow_all),
            None => {
                let mut policy = PermissionPolicy::allow_all();
                for (name, perms) in command_permissions {
                    policy.command_permissions.insert(name, perms);
                }
                policy
            }
        };
        policy.read_paths = self.canonicalize_paths(&self.read_paths);
        policy.write_paths = self.canonicalize_paths(&self.write_paths);
        policy
    }

    fn canonicalize_paths(&self, paths: &Option<Vec<String>>) -> Vec<std::path::PathBuf> {
        paths
            .as_ref()
            .map(|list| {
                list.iter()
                    .map(std::path::PathBuf::from)
                    .filter_map(|p| p.canonicalize().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn parse_command_permissions(&self) -> HashMap<String, PermissionSet> {
        let mut map = HashMap::new();
        // Shorthand lists first so that explicit `commands` entries win.
        for (names, perm) in [
            (&self.read, Permission::Read),
            (&self.write, Permission::Write),
        ] {
            if let Some(names) = names {
                for name in names {
                    let mut set = PermissionSet::empty();
                    set.insert(perm.clone());
                    map.insert(name.clone(), set);
                }
            }
        }
        if let Some(commands) = &self.commands {
            for (name, tags) in commands {
                let mut set = PermissionSet::empty();
                for tag in tags {
                    match tag.trim().to_ascii_uppercase().as_str() {
                        "READ" => set.insert(Permission::Read),
                        "WRITE" => set.insert(Permission::Write),
                        other => set.insert(Permission::Custom(other.to_string())),
                    }
                }
                map.insert(name.clone(), set);
            }
        }
        map
    }

    /// Build an audit logger from the configured `audit_log` and `audit_format`.
    pub fn audit_logger(&self) -> AuditLogger {
        let format = self
            .audit_format
            .as_deref()
            .and_then(AuditFormat::parse)
            .unwrap_or(AuditFormat::Text);

        let meta = self.audit_meta.clone().unwrap_or_default();

        match &self.audit_log {
            Some(path) => {
                let path = std::path::Path::new(path);
                AuditLogger::file(path, format)
                    .unwrap_or_else(|_| AuditLogger::null().with_format(format))
                    .with_meta(meta)
            }
            None => AuditLogger::null().with_format(format).with_meta(meta),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_file() {
        let input = r#"
[api]
api_key = "sk-test"

[shell]
perm_mode = "deny:write"
audit_format = "json"
"#;
        let file: ConfigFile = toml::from_str(input).unwrap();
        let shell = file.shell.unwrap();
        assert_eq!(shell.perm_mode.as_deref(), Some("deny:write"));
        assert_eq!(shell.audit_format.as_deref(), Some("json"));
        assert!(shell.audit_log.is_none());
    }

    #[test]
    fn default_config_values() {
        let shell = ShellConfig::default();
        assert_eq!(shell.perm_mode.as_deref(), Some("allow_all"));
        assert_eq!(shell.audit_format.as_deref(), Some("text"));
        assert!(shell.audit_log.is_none());
    }

    #[test]
    fn shorthand_read_write_lists() {
        let input = r#"
[shell]
perm_mode = "allow:read deny:write"
read = ["awk", "cat"]
write = ["mkdir"]
"#;
        let file: ConfigFile = toml::from_str(input).unwrap();
        let shell = file.shell.unwrap();
        let policy = shell.permission_policy();
        assert_eq!(
            policy.permissions_for_command("awk"),
            Some(&PermissionSet::read())
        );
        assert_eq!(
            policy.permissions_for_command("mkdir"),
            Some(&PermissionSet::write())
        );
        assert!(policy.permissions_for_command("curl").is_none());
    }

    #[test]
    fn explicit_commands_override_shorthand() {
        let input = r#"
[shell]
read = ["git"]

[shell.commands]
git = ["network", "read"]
"#;
        let file: ConfigFile = toml::from_str(input).unwrap();
        let shell = file.shell.unwrap();
        let policy = shell.permission_policy();
        let perms = policy.permissions_for_command("git").unwrap();
        assert!(!perms.contains(&Permission::Write));
        assert!(perms.contains(&Permission::Custom("NETWORK".to_string())));
    }
}
