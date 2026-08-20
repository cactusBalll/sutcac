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
    /// Combined allow/deny restriction.  `allow` lists tags that must cover
    /// every required permission; `deny` lists tags that must not appear in
    /// the required set.
    Restrict {
        allow: PermissionSet,
        deny: PermissionSet,
    },
}

/// A policy describing which permissions are allowed.
#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    pub mode: PermissionMode,
    /// Per-command permission overrides. A command name maps to the set of
    /// permissions it requires. If a command is absent, the default estimate
    /// (READ+WRITE for external commands, builtin-specific otherwise) is used.
    pub command_permissions: HashMap<String, PermissionSet>,
    /// Commands that are always allowed, regardless of the mode.
    pub allow1: HashSet<String>,
    /// Commands that are always denied, regardless of the mode.
    pub deny1: HashSet<String>,
}

/// Error returned when a required permission is denied.
#[derive(Debug, Clone)]
pub struct PermissionError {
    pub denied: PermissionSet,
    /// Optional human-readable reason appended to the default message.
    pub reason: Option<String>,
}

impl PermissionError {
    fn new(denied: PermissionSet) -> Self {
        Self {
            denied,
            reason: None,
        }
    }

    fn with_reason(denied: PermissionSet, reason: impl Into<String>) -> Self {
        Self {
            denied,
            reason: Some(reason.into()),
        }
    }
}

impl fmt::Display for PermissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let denied = self.denied.to_string().to_ascii_lowercase();
        if let Some(reason) = &self.reason {
            write!(f, "permission denied: {}. {}", self.denied, reason)
        } else {
            write!(
                f,
                "permission denied: {}. Hint: the current shell policy restricts this operation. Adjust `perm_mode` in the config (e.g., allow:{} or allow_all).",
                self.denied, denied
            )
        }
    }
}

impl std::error::Error for PermissionError {}

impl PermissionPolicy {
    /// Default policy that allows all permissions.
    pub fn allow_all() -> Self {
        Self {
            mode: PermissionMode::AllowAll,
            command_permissions: HashMap::new(),
            allow1: HashSet::new(),
            deny1: HashSet::new(),
        }
    }

    /// Parse a permission policy string.
    ///
    /// Supported clauses (separated by whitespace):
    /// - `allow_all`
    /// - `allow:<tags>` – required permissions must be a subset of these tags
    /// - `deny:<tags>` – required permissions must not intersect these tags
    /// - `allow1:<cmds>` – always allow these commands
    /// - `deny1:<cmds>` – always deny these commands
    ///
    /// `allow` and `deny` can be combined.  `allow1`/`deny1` take precedence
    /// over everything else; `deny1` beats `allow1`.
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
            for (name, perms) in command_permissions {
                policy.command_permissions.insert(name, perms);
            }
            return Some(policy);
        }

        if s.is_empty() {
            return None;
        }

        let mut allow = PermissionSet::empty();
        let mut deny = PermissionSet::empty();
        let mut allow1 = HashSet::new();
        let mut deny1 = HashSet::new();
        let mut has_allow = false;
        let mut has_deny = false;

        for token in s.split_whitespace() {
            let lower = token.to_ascii_lowercase();
            if let Some(tags) = lower.strip_prefix("allow:") {
                allow = allow.union(parse_set(tags));
                has_allow = true;
            } else if let Some(tags) = lower.strip_prefix("deny:") {
                deny = deny.union(parse_set(tags));
                has_deny = true;
            } else if let Some(cmds) = lower.strip_prefix("allow1:") {
                for cmd in cmds.split(',') {
                    let cmd = cmd.trim();
                    if !cmd.is_empty() {
                        allow1.insert(cmd.to_ascii_lowercase());
                    }
                }
            } else if let Some(cmds) = lower.strip_prefix("deny1:") {
                for cmd in cmds.split(',') {
                    let cmd = cmd.trim();
                    if !cmd.is_empty() {
                        deny1.insert(cmd.to_ascii_lowercase());
                    }
                }
            } else {
                return None;
            }
        }

        let mode = if has_allow && has_deny {
            PermissionMode::Restrict { allow, deny }
        } else if has_allow {
            PermissionMode::AllowOnly(allow)
        } else if has_deny {
            PermissionMode::Deny(deny)
        } else {
            PermissionMode::AllowAll
        };

        let mut policy = Self {
            mode,
            command_permissions: HashMap::new(),
            allow1,
            deny1,
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
                    Err(PermissionError::new(blocked))
                }
            }
            PermissionMode::AllowOnly(allowed) => {
                let blocked = required.difference(allowed);
                if blocked.is_empty() {
                    Ok(())
                } else {
                    Err(PermissionError::new(blocked))
                }
            }
            PermissionMode::Restrict { allow, deny } => {
                let missing = required.difference(allow);
                let blocked = deny.intersection(required);
                let denied = missing.union(blocked);
                if denied.is_empty() {
                    Ok(())
                } else {
                    Err(PermissionError::new(denied))
                }
            }
        }
    }

    /// Check a single command by name.  `allow1`/`deny1` take precedence over
    /// the regular allow/deny sets.
    pub fn check_command(
        &self,
        name: &str,
        required: &PermissionSet,
    ) -> Result<(), PermissionError> {
        if command_name_in_set(&self.deny1, name) {
            return Err(PermissionError::with_reason(
                required.clone(),
                format!("deny1:{} explicitly blocks this command.", name),
            ));
        }
        if command_name_in_set(&self.allow1, name) {
            return Ok(());
        }
        self.check(required)
    }
}

/// Return true if `name` (or its basename) appears in `set`.
fn command_name_in_set(set: &HashSet<String>, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if set.contains(&lower) {
        return true;
    }
    if let Some(base) = std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
    {
        set.contains(&base.to_ascii_lowercase())
    } else {
        false
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
    fn combined_allow_and_deny() {
        let policy = PermissionPolicy::parse("allow:read,write deny:network").unwrap();
        assert!(matches!(policy.mode, PermissionMode::Restrict { .. }));

        // READ+WRITE is allowed and does not touch NETWORK.
        policy
            .check(&PermissionSet::read().union(PermissionSet::write()))
            .unwrap();

        // NETWORK is explicitly denied.
        let mut network_only = PermissionSet::empty();
        network_only.insert(Permission::Custom("network".to_string()));
        assert!(policy.check(&network_only).is_err());

        // EXEC is not in the allow set.
        let mut exec = PermissionSet::empty();
        exec.insert(Permission::Custom("exec".to_string()));
        assert!(policy.check(&exec).is_err());
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
    fn allow1_and_deny1_override_mode() {
        // Default READ+WRITE required for unknown externals.
        let policy = PermissionPolicy::parse("allow:read allow1:mytool deny1:rm").unwrap();

        // mytool is allowed even though it would normally need WRITE.
        policy
            .check_command("mytool", &PermissionSet::write())
            .unwrap();

        // rm is denied regardless of the allow set.
        assert!(policy.check_command("rm", &PermissionSet::read()).is_err());

        // Other commands still fall back to the mode.
        assert!(
            policy
                .check_command("other", &PermissionSet::write())
                .is_err()
        );
        policy
            .check_command("other", &PermissionSet::read())
            .unwrap();
    }

    #[test]
    fn allow1_matches_basename() {
        let policy = PermissionPolicy::parse("deny:write allow1:/usr/bin/git").unwrap();
        policy
            .check_command("/usr/bin/git", &PermissionSet::write())
            .unwrap();
    }

    #[test]
    fn command_permissions_from_map() {
        let mut commands = HashMap::new();
        let mut read_only = PermissionSet::empty();
        read_only.insert(Permission::Read);
        commands.insert("git".to_string(), read_only);

        let policy = PermissionPolicy::parse_with_commands("allow:read", commands);
        assert!(policy.is_some());
        let policy = policy.unwrap();
        assert_eq!(
            policy.permissions_for_command("git"),
            Some(&PermissionSet::read())
        );
        assert!(policy.permissions_for_command("unknown").is_none());
    }
}
