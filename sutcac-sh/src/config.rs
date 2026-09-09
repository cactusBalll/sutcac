//! Configuration loader for sutcac-sh.
//!
//! Reads the `[shell]` section from the shared TOML configuration files. The
//! XDG config (`~/.config/catus/config.toml`) is the base; the workspace
//! config (`./.sutcac/config.toml`) is merged on top when present and its
//! set fields win. If neither file is found, sensible defaults are used
//! (allow all). The audit trail is always written to
//! `~/.config/catus/audit.log` and is not configurable.

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
    /// Shorthand for read-write commands: `rw = ["cp", "ls", ...]` is
    /// equivalent to listing each command under `[shell.commands]` with
    /// `tags = ["read", "write"]`. Entries in `commands` take precedence.
    pub rw: Option<Vec<String>>,
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
            audit_format: Some("text".to_string()),
            audit_meta: None,
            commands: None,
            read: None,
            write: None,
            rw: None,
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
    pub shell: Option<PartialShellConfig>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self { shell: None }
    }
}

/// Presence-preserving form of [`ShellConfig`] used while loading and
/// merging: a field is `None` only when the file did not set it, so the
/// workspace config can override the XDG base without pulling in
/// `ShellConfig`'s own defaults (`ShellConfig` uses `#[serde(default)]`,
/// which fills missing fields and hides whether they were written).
#[derive(Debug, Deserialize)]
struct PartialShellConfig {
    pub perm_mode: Option<String>,
    pub audit_format: Option<String>,
    pub audit_meta: Option<HashMap<String, String>>,
    pub commands: Option<HashMap<String, Vec<String>>>,
    pub read: Option<Vec<String>>,
    pub write: Option<Vec<String>>,
    pub rw: Option<Vec<String>>,
    pub read_paths: Option<Vec<String>>,
    pub write_paths: Option<Vec<String>>,
}

impl PartialShellConfig {
    /// Overlay fields set in `over` (the workspace config) onto `self`
    /// (the XDG base). List and map values replace wholesale.
    fn overlay_from(&mut self, over: PartialShellConfig) {
        if over.perm_mode.is_some() {
            self.perm_mode = over.perm_mode;
        }
        if over.audit_format.is_some() {
            self.audit_format = over.audit_format;
        }
        if over.audit_meta.is_some() {
            self.audit_meta = over.audit_meta;
        }
        if over.commands.is_some() {
            self.commands = over.commands;
        }
        if over.read.is_some() {
            self.read = over.read;
        }
        if over.write.is_some() {
            self.write = over.write;
        }
        if over.rw.is_some() {
            self.rw = over.rw;
        }
        if over.read_paths.is_some() {
            self.read_paths = over.read_paths;
        }
        if over.write_paths.is_some() {
            self.write_paths = over.write_paths;
        }
    }

    fn into_config(self) -> ShellConfig {
        ShellConfig {
            perm_mode: self.perm_mode,
            audit_format: self.audit_format,
            audit_meta: self.audit_meta,
            commands: self.commands,
            read: self.read,
            write: self.write,
            rw: self.rw,
            read_paths: self.read_paths,
            write_paths: self.write_paths,
        }
    }
}

impl ShellConfig {
    /// Load the shell configuration: the XDG config is the base and the
    /// workspace config (when present) overrides its set fields.
    ///
    /// Returns `None` when neither file exists.
    pub fn load() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let mut base = read_shell_section(&Self::xdg_config())?;
        let workspace = read_shell_section(&Self::workspace_config())?;
        match (base.as_mut(), workspace) {
            (Some(base), Some(workspace)) => base.overlay_from(workspace),
            (None, workspace @ Some(_)) => {
                base = workspace;
            }
            _ => {}
        }
        Ok(base.map(PartialShellConfig::into_config))
    }

    /// The workspace configuration file (`./.sutcac/config.toml`).
    fn workspace_config() -> PathBuf {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".sutcac")
            .join("config.toml")
    }

    /// The XDG configuration file (`~/.config/catus/config.toml`).
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
        if let Some(names) = &self.rw {
            for name in names {
                let mut set = PermissionSet::empty();
                set.insert(Permission::Read);
                set.insert(Permission::Write);
                map.insert(name.clone(), set);
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

    /// Build an audit logger writing to the fixed XDG audit log
    /// (`~/.config/catus/audit.log`); falls back to a null logger when the
    /// file cannot be opened.
    pub fn audit_logger(&self) -> AuditLogger {
        let format = self
            .audit_format
            .as_deref()
            .and_then(AuditFormat::parse)
            .unwrap_or(AuditFormat::Text);

        let meta = self.audit_meta.clone().unwrap_or_default();

        AuditLogger::file(&Self::xdg_audit_log_path(), format)
            .unwrap_or_else(|_| AuditLogger::null().with_format(format))
            .with_meta(meta)
    }

    /// The audit log path (`~/.config/catus/audit.log`). Not configurable.
    fn xdg_audit_log_path() -> std::path::PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| {
            let mut home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            home.push(".config");
            home
        });
        let mut path = base;
        path.push("catus");
        path.push("audit.log");
        path
    }
}

/// Read and parse the `[shell]` section of a TOML file, if it exists.
fn read_shell_section(
    path: &std::path::Path,
) -> Result<Option<PartialShellConfig>, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)?;
    let file: ConfigFile = toml::from_str(&contents)?;
    Ok(file.shell)
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
    }

    #[test]
    fn removed_audit_log_key_is_ignored() {
        // `audit_log` is no longer configurable; old files keep loading and
        // the unknown key is ignored.
        let input = "[shell]\naudit_log = \".sutcac/audit.log\"\n";
        let file: ConfigFile = toml::from_str(input).unwrap();
        let shell = file.shell.unwrap().into_config();
        assert_eq!(shell.audit_format.as_deref(), None);
    }

    #[test]
    fn default_config_values() {
        let shell = ShellConfig::default();
        assert_eq!(shell.perm_mode.as_deref(), Some("allow_all"));
        assert_eq!(shell.audit_format.as_deref(), Some("text"));
    }

    #[test]
    fn workspace_overlay_wins_over_xdg_base() {
        let mut base: PartialShellConfig = toml::from_str::<ConfigFile>(
            "[shell]\nperm_mode = \"deny:write\"\naudit_format = \"json\"\nread = [\"cat\"]\n",
        )
        .unwrap()
        .shell
        .unwrap();
        let over: PartialShellConfig =
            toml::from_str::<ConfigFile>("[shell]\nperm_mode = \"allow_all\"\nread = [\"ls\"]\n")
                .unwrap()
                .shell
                .unwrap();
        base.overlay_from(over);
        let base = base.into_config();
        assert_eq!(base.perm_mode.as_deref(), Some("allow_all"));
        // Fields not set in the workspace config keep the XDG values.
        assert_eq!(base.audit_format.as_deref(), Some("json"));
        assert_eq!(base.read.as_deref(), Some(&["ls".to_string()][..]));
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
        let shell = file.shell.unwrap().into_config();
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
    fn shorthand_rw_list() {
        let input = r#"
[shell]
perm_mode = "allow:read"
rw = ["cp", "ls"]
"#;
        let file: ConfigFile = toml::from_str(input).unwrap();
        let shell = file.shell.unwrap().into_config();
        let policy = shell.permission_policy();
        let perms = policy.permissions_for_command("cp").unwrap();
        assert!(perms.contains(&Permission::Read));
        assert!(perms.contains(&Permission::Write));
        assert_eq!(
            policy.permissions_for_command("ls"),
            Some(&PermissionSet::read_write())
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
        let shell = file.shell.unwrap().into_config();
        let policy = shell.permission_policy();
        let perms = policy.permissions_for_command("git").unwrap();
        assert!(!perms.contains(&Permission::Write));
        assert!(perms.contains(&Permission::Custom("NETWORK".to_string())));
    }
}
