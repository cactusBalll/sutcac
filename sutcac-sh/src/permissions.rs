//! Permission model for command execution.
//!
//! The shell can restrict commands that read from or write to the external
//! environment. Permissions are checked before a command (or redirection) is
//! executed; a denial returns a non-zero status and is recorded in the audit log.
//!
//! In addition to the built-in `Read` and `Write` classes, arbitrary custom
//! permission tags (for example `network`, `clipboard`, `display`) can be used
//! both in the policy and when assigning permissions to individual external
//! commands.

use std::collections::{HashMap, HashSet};
use std::fmt;

/// A single permission class.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Read from the external environment (file system, current directory, etc.).
    Read,
    /// Write to the external environment (files, directories, process env, etc.).
    Write,
    /// A custom permission tag defined by the user.
    Custom(String),
}

impl Permission {
    pub fn name(&self) -> String {
        match self {
            Permission::Read => "READ".to_string(),
            Permission::Write => "WRITE".to_string(),
            Permission::Custom(s) => s.clone(),
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// A set of zero or more `Permission` values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PermissionSet {
    bits: u8,
    custom: HashSet<String>,
}

impl PermissionSet {
    pub fn read() -> Self {
        Self {
            bits: 1,
            custom: HashSet::new(),
        }
    }

    pub fn write() -> Self {
        Self {
            bits: 2,
            custom: HashSet::new(),
        }
    }

    pub fn empty() -> Self {
        Self {
            bits: 0,
            custom: HashSet::new(),
        }
    }

    pub fn insert(&mut self, perm: Permission) {
        match perm {
            Permission::Read => self.bits |= 1,
            Permission::Write => self.bits |= 2,
            Permission::Custom(s) => {
                self.custom.insert(s.to_ascii_uppercase());
            }
        }
    }

    pub fn contains(&self, perm: &Permission) -> bool {
        match perm {
            Permission::Read => (self.bits & 1) != 0,
            Permission::Write => (self.bits & 2) != 0,
            Permission::Custom(s) => self.custom.contains(s),
        }
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
            custom: self.custom.union(&other.custom).cloned().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bits == 0 && self.custom.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = Permission> + '_ {
        let mut perms = Vec::new();
        if (self.bits & 1) != 0 {
            perms.push(Permission::Read);
        }
        if (self.bits & 2) != 0 {
            perms.push(Permission::Write);
        }
        for tag in &self.custom {
            perms.push(Permission::Custom(tag.clone()));
        }
        perms.into_iter()
    }

    /// Return true if every permission in `other` is contained in this set.
    pub fn contains_all(&self, other: &Self) -> bool {
        other.iter().all(|p| self.contains(&p))
    }

    /// Return a new set containing only permissions present in both sets.
    pub fn intersection(&self, other: &Self) -> Self {
        let mut result = Self::empty();
        if (self.bits & other.bits & 1) != 0 {
            result.insert(Permission::Read);
        }
        if (self.bits & other.bits & 2) != 0 {
            result.insert(Permission::Write);
        }
        for tag in self.custom.intersection(&other.custom) {
            result.insert(Permission::Custom(tag.clone()));
        }
        result
    }

    /// Return a new set containing permissions in `self` but not in `other`.
    pub fn difference(&self, other: &Self) -> Self {
        let mut result = Self::empty();
        if (self.bits & 1) != 0 && (other.bits & 1) == 0 {
            result.insert(Permission::Read);
        }
        if (self.bits & 2) != 0 && (other.bits & 2) == 0 {
            result.insert(Permission::Write);
        }
        for tag in self.custom.difference(&other.custom) {
            result.insert(Permission::Custom(tag.clone()));
        }
        result
    }
}

impl fmt::Display for PermissionSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<String> = self.iter().map(|p| p.to_string()).collect();
        names.sort();
        if names.is_empty() {
            write!(f, "NONE")
        } else {
            write!(f, "{}", names.join(","))
        }
    }
}

/// Policy mode: how to evaluate a required permission set.
#[derive(Debug, Clone)]
pub enum PermissionMode {
    /// Allow everything (default).
    AllowAll,
    /// Deny the listed permissions; all others are allowed.
    Deny(PermissionSet),
    /// Allow only the listed permissions; all others are denied.
    AllowOnly(PermissionSet),
}

/// A policy describing which permissions are allowed.
#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    pub mode: PermissionMode,
    /// Per-command permission overrides. A command name maps to the set of
    /// permissions it requires. If a command is absent, the default estimate
    /// (READ+WRITE for external commands, builtin-specific otherwise) is used.
    pub command_permissions: HashMap<String, PermissionSet>,
}

/// Error returned when a required permission is denied.
#[derive(Debug, Clone)]
pub struct PermissionError {
    pub denied: PermissionSet,
}

impl fmt::Display for PermissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let denied = self.denied.to_string().to_ascii_lowercase();
        write!(
            f,
            "permission denied: {}. Hint: the current shell policy restricts this operation. Adjust `perm_mode` in the config (e.g., allow:{} or allow_all).",
            self.denied, denied
        )
    }
}

impl std::error::Error for PermissionError {}

impl PermissionPolicy {
    /// Default policy that allows all permissions.
    pub fn allow_all() -> Self {
        Self {
            mode: PermissionMode::AllowAll,
            command_permissions: default_command_permissions(),
        }
    }

    /// Parse a simple policy string:
    /// - `allow_all`
    /// - `deny:<tags>` (comma-separated)
    /// - `allow:<tags>` (comma-separated)
    ///
    /// Tags are case-insensitive. Built-in tags: `read`, `write`. Any other
    /// tag becomes a custom permission.
    pub fn parse(s: &str) -> Option<Self> {
        Self::parse_with_commands(s, HashMap::new())
    }

    /// Create a policy from a mode string and a command-permission map.
    pub fn parse_with_commands(
        s: &str,
        command_permissions: HashMap<String, PermissionSet>,
    ) -> Option<Self> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("allow_all") {
            let mut policy = Self::allow_all();
            // User-provided entries override built-in defaults.
            for (name, perms) in command_permissions {
                policy.command_permissions.insert(name, perms);
            }
            return Some(policy);
        }

        let mode = if let Some(tags) = s.strip_prefix("deny:") {
            PermissionMode::Deny(parse_set(tags))
        } else if let Some(tags) = s.strip_prefix("allow:") {
            PermissionMode::AllowOnly(parse_set(tags))
        } else {
            return None;
        };

        let mut policy = Self {
            mode,
            command_permissions: default_command_permissions(),
        };
        for (name, perms) in command_permissions {
            policy.command_permissions.insert(name, perms);
        }
        Some(policy)
    }

    /// Look up the permission set required by an external command.
    pub fn permissions_for_command(&self, name: &str) -> Option<&PermissionSet> {
        self.command_permissions.get(name)
    }

    /// Check whether `required` permissions are allowed under this policy.
    pub fn check(&self, required: &PermissionSet) -> Result<(), PermissionError> {
        match &self.mode {
            PermissionMode::AllowAll => Ok(()),
            PermissionMode::Deny(denied) => {
                let blocked = denied.intersection(required);
                if blocked.is_empty() {
                    Ok(())
                } else {
                    Err(PermissionError { denied: blocked })
                }
            }
            PermissionMode::AllowOnly(allowed) => {
                let blocked = required.difference(allowed);
                if blocked.is_empty() {
                    Ok(())
                } else {
                    Err(PermissionError { denied: blocked })
                }
            }
        }
    }
}

fn parse_set(s: &str) -> PermissionSet {
    let mut set = PermissionSet::empty();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_uppercase().as_str() {
            "READ" => set.insert(Permission::Read),
            "WRITE" => set.insert(Permission::Write),
            other => set.insert(Permission::Custom(other.to_string())),
        }
    }
    set
}

/// Built-in permission estimates for common read-only coreutils.
fn default_command_permissions() -> HashMap<String, PermissionSet> {
    let read_only: PermissionSet = {
        let mut s = PermissionSet::empty();
        s.insert(Permission::Read);
        s
    };

    let mut map = HashMap::new();
    for cmd in [
        "cat", "cut", "find", "grep", "head", "ls", "more", "ps", "pwd", "sed", "sort", "tail",
        "tr", "uniq", "wc",
    ] {
        map.insert(cmd.to_string(), read_only.clone());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_set_operations() {
        let mut set = PermissionSet::empty();
        assert!(!set.contains(&Permission::Read));
        assert!(!set.contains(&Permission::Write));

        set.insert(Permission::Read);
        assert!(set.contains(&Permission::Read));
        assert!(!set.contains(&Permission::Write));

        let both = set.union(PermissionSet::write());
        assert!(both.contains(&Permission::Read));
        assert!(both.contains(&Permission::Write));
    }

    #[test]
    fn parse_policy() {
        assert!(matches!(
            PermissionPolicy::parse("allow_all").unwrap().mode,
            PermissionMode::AllowAll
        ));

        let deny_write = PermissionPolicy::parse("deny:write").unwrap();
        deny_write.check(&PermissionSet::read()).unwrap();
        assert!(deny_write.check(&PermissionSet::write()).is_err());
        assert!(
            deny_write
                .check(&PermissionSet::read().union(PermissionSet::write()))
                .is_err()
        );

        let allow_read = PermissionPolicy::parse("allow:read").unwrap();
        allow_read.check(&PermissionSet::read()).unwrap();
        assert!(allow_read.check(&PermissionSet::write()).is_err());

        let allow_both = PermissionPolicy::parse("allow:read,write").unwrap();
        allow_both
            .check(&PermissionSet::read().union(PermissionSet::write()))
            .unwrap();
    }

    #[test]
    fn custom_tags() {
        let policy = PermissionPolicy::parse("allow:read,network").unwrap();
        let mut network_only = PermissionSet::empty();
        network_only.insert(Permission::Custom("network".to_string()));
        policy.check(&network_only).unwrap();

        let mut write = PermissionSet::empty();
        write.insert(Permission::Write);
        assert!(policy.check(&write).is_err());
    }

    #[test]
    fn default_read_only_commands() {
        let policy = PermissionPolicy::allow_all();
        assert!(
            policy
                .permissions_for_command("grep")
                .unwrap()
                .contains(&Permission::Read)
        );
        assert!(
            !policy
                .permissions_for_command("grep")
                .unwrap()
                .contains(&Permission::Write)
        );
    }
}
