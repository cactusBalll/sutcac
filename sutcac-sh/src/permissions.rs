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
use std::path::{Path, PathBuf};

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

    pub fn read_write() -> Self {
        Self {
            bits: 3,
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

/// Access mode for a filesystem path referenced by a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    Read,
    Write,
}

/// A filesystem path referenced by a command, classified by intended access.
#[derive(Debug, Clone)]
pub struct CommandPath {
    pub path: PathBuf,
    pub access: PathAccess,
}

impl CommandPath {
    pub fn new(path: impl Into<PathBuf>, access: PathAccess) -> Self {
        Self {
            path: path.into(),
            access,
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
    /// Allowed read-only paths. Empty means only the base directory
    /// (see `base_dir`) is readable when a base directory is set.
    pub read_paths: Vec<PathBuf>,
    /// Allowed read-write paths. Empty means only the base directory
    /// (see `base_dir`) is writable when a base directory is set.
    pub write_paths: Vec<PathBuf>,
    /// Directory that is always readable and writable regardless of the
    /// path lists (typically the workspace). Its presence also enables the
    /// path checks: when it is set, every path outside the base directory
    /// and the configured lists is denied. `None` keeps the legacy behavior
    /// where empty path lists disable the path restriction entirely.
    pub base_dir: Option<PathBuf>,
    /// Tags granted interactively for the rest of the session. They bypass
    /// the mode restrictions (including `deny` lists) for matching checks.
    pub session_grants: PermissionSet,
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
            read_paths: Vec::new(),
            write_paths: Vec::new(),
            base_dir: None,
            session_grants: PermissionSet::empty(),
        }
    }

    /// Set the always-readable/writable base directory (typically the
    /// workspace). The path is canonicalized; if that fails the policy keeps
    /// the legacy unrestricted-path behavior.
    pub fn with_base_dir(mut self, path: &Path) -> Self {
        self.base_dir = path.canonicalize().ok();
        self
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
            read_paths: Vec::new(),
            write_paths: Vec::new(),
            base_dir: None,
            session_grants: PermissionSet::empty(),
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
    ///
    /// Interactively granted session tags count as allowed even when the
    /// mode would deny them.
    pub fn check(&self, required: &PermissionSet) -> Result<(), PermissionError> {
        let granted = self.session_grants.clone();
        match &self.mode {
            PermissionMode::AllowAll => Ok(()),
            PermissionMode::Deny(denied) => {
                let blocked = denied.intersection(required).difference(&granted);
                if blocked.is_empty() {
                    Ok(())
                } else {
                    Err(PermissionError::new(blocked))
                }
            }
            PermissionMode::AllowOnly(allowed) => {
                let allowed = allowed.clone().union(granted.clone());
                let blocked = required.difference(&allowed);
                if blocked.is_empty() {
                    Ok(())
                } else {
                    Err(PermissionError::new(blocked))
                }
            }
            PermissionMode::Restrict { allow, deny } => {
                let allowed = allow.clone().union(granted.clone());
                let missing = required.difference(&allowed);
                let blocked = deny.intersection(required).difference(&granted);
                let denied = missing.union(blocked);
                if denied.is_empty() {
                    Ok(())
                } else {
                    Err(PermissionError::new(denied))
                }
            }
        }
    }

    /// Grant a permission tag interactively for the rest of the session.
    ///
    /// `tag` is case-insensitive; `read`/`write` map to the built-in classes
    /// and anything else becomes a custom tag. Multiple tags may be given
    /// comma-separated (e.g. `"read,write"`).
    pub fn grant_tag(&mut self, tag: &str) {
        let granted = parse_set(tag);
        self.session_grants = self.session_grants.clone().union(granted);
    }

    /// Revoke previously granted session tags. `tags` is a comma-separated
    /// list; unknown tags are ignored.
    pub fn revoke_grants(&mut self, tags: &str) {
        self.session_grants = self.session_grants.difference(&parse_set(tags));
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

    /// Check whether the filesystem paths referenced by a command are allowed.
    ///
    /// Path checks are enabled when `base_dir` is set (everything outside the
    /// base directory and the configured lists is denied) or when any of the
    /// path lists is non-empty. Otherwise the feature is disabled and all
    /// paths are allowed.
    /// `cwd` is the shell's current working directory, used to resolve
    /// relative paths; containment is always checked against the base
    /// directory (falling back to `cwd` when unset), so changing the working
    /// directory cannot widen access.
    pub fn check_paths(&self, paths: &[CommandPath], cwd: &Path) -> Result<(), PermissionError> {
        if self.base_dir.is_none() && self.read_paths.is_empty() && self.write_paths.is_empty() {
            return Ok(());
        }
        for cp in paths {
            let resolved = resolve_check_path(&cp.path, cwd);
            let Some(resolved) = resolved else {
                return Err(PermissionError::with_reason(
                    match cp.access {
                        PathAccess::Read => PermissionSet::read(),
                        PathAccess::Write => PermissionSet::write(),
                    },
                    format!("cannot resolve path {:?}", cp.path),
                ));
            };
            match cp.access {
                PathAccess::Read => {
                    if !self.is_readable(&resolved, cwd) {
                        return Err(PermissionError::with_reason(
                            PermissionSet::read(),
                            format!("read path {:?} is outside allowed paths", cp.path),
                        ));
                    }
                }
                PathAccess::Write => {
                    if !self.is_writable(&resolved, cwd) {
                        return Err(PermissionError::with_reason(
                            PermissionSet::write(),
                            format!("write path {:?} is outside allowed paths", cp.path),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn is_readable(&self, resolved: &Path, cwd: &Path) -> bool {
        self.path_within_any(resolved, &[self.allowed_base(cwd)])
            || self.path_within_any(resolved, &self.read_paths)
            || self.path_within_any(resolved, &self.write_paths)
    }

    fn is_writable(&self, resolved: &Path, cwd: &Path) -> bool {
        self.path_within_any(resolved, &[self.allowed_base(cwd)])
            || self.path_within_any(resolved, &self.write_paths)
    }

    /// The directory that is always readable and writable. Paths are checked
    /// against this base (not the live working directory), so `cd` cannot
    /// widen access.
    fn allowed_base(&self, cwd: &Path) -> PathBuf {
        self.base_dir.clone().unwrap_or_else(|| cwd.to_path_buf())
    }

    fn path_within_any(&self, resolved: &Path, allowed: &[PathBuf]) -> bool {
        allowed.iter().any(|base| resolved.starts_with(base))
    }

    /// Replace the allowed path lists. Paths are canonicalized; entries that fail
    /// to resolve are silently ignored.
    pub fn with_paths(mut self, read_paths: &[PathBuf], write_paths: &[PathBuf]) -> Self {
        self.read_paths = canonicalize_paths(read_paths);
        self.write_paths = canonicalize_paths(write_paths);
        self
    }

    /// Grant additional read/write path access for the rest of the session
    /// (typically from an `ask_permission` interaction). Paths are resolved
    /// against `cwd` and canonicalized the same way as `check_paths`
    /// (nonexistent paths fall back to their nearest existing ancestor).
    /// Writable paths are implicitly readable.
    pub fn grant_paths(&mut self, read: &[PathBuf], write: &[PathBuf], cwd: &Path) {
        for path in read {
            Self::push_unique_path(&mut self.read_paths, path, cwd);
        }
        for path in write {
            Self::push_unique_path(&mut self.write_paths, path, cwd);
            Self::push_unique_path(&mut self.read_paths, path, cwd);
        }
    }

    fn push_unique_path(paths: &mut Vec<PathBuf>, path: &Path, cwd: &Path) {
        if let Some(resolved) = resolve_check_path(path, cwd) {
            if !paths.contains(&resolved) {
                paths.push(resolved);
            }
        }
    }
}

/// Canonicalize a list of paths, skipping entries that do not exist.
fn canonicalize_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().filter_map(|p| p.canonicalize().ok()).collect()
}

/// Resolve a path for containment checks. Returns the canonical path if it
/// exists, otherwise canonicalizes the nearest existing ancestor and appends
/// the remaining suffix. Returns `None` if no ancestor can be resolved.
fn resolve_check_path(path: &Path, cwd: &Path) -> Option<PathBuf> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    if let Ok(canon) = abs.canonicalize() {
        return Some(canon);
    }

    let mut current = abs.as_path();
    while let Some(parent) = current.parent() {
        if let Ok(canon_parent) = parent.canonicalize() {
            if let Ok(suffix) = abs.strip_prefix(parent) {
                return Some(canon_parent.join(suffix));
            }
        }
        current = parent;
    }

    None
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

    #[test]
    fn empty_path_lists_allow_all_paths_without_base_dir() {
        // Legacy behavior: without a base directory, empty path lists
        // disable the path restriction entirely.
        let policy = PermissionPolicy::allow_all();
        policy
            .check_paths(
                &[CommandPath::new("/etc/passwd", PathAccess::Read)],
                Path::new("/"),
            )
            .unwrap();
    }

    #[test]
    fn base_dir_enables_workspace_only_restriction() {
        let tmp = std::env::temp_dir();
        let base = tmp.join("sutcac_base_ws");
        let outside = tmp.join("sutcac_base_outside");
        let _ = std::fs::create_dir_all(&base);
        let _ = std::fs::create_dir_all(&outside);

        // Base directory set, empty path lists: only the base directory is
        // readable and writable.
        let policy = PermissionPolicy::allow_all().with_base_dir(&base);
        policy
            .check_paths(&[CommandPath::new(&base, PathAccess::Read)], &outside)
            .unwrap();
        policy
            .check_paths(&[CommandPath::new(&base, PathAccess::Write)], &outside)
            .unwrap();
        policy
            .check_paths(
                &[CommandPath::new(base.join("new.txt"), PathAccess::Write)],
                &outside,
            )
            .unwrap();
        assert!(
            policy
                .check_paths(&[CommandPath::new(&outside, PathAccess::Read)], &base)
                .is_err()
        );
        assert!(
            policy
                .check_paths(&[CommandPath::new(&outside, PathAccess::Write)], &base)
                .is_err()
        );
    }

    #[test]
    fn cd_outside_base_dir_does_not_widen_access() {
        let tmp = std::env::temp_dir();
        let base = tmp.join("sutcac_cd_base");
        let outside = tmp.join("sutcac_cd_outside");
        let _ = std::fs::create_dir_all(&base);
        let _ = std::fs::create_dir_all(&outside);

        // The agent cd'd into `outside`; the live cwd must NOT become an
        // allowed root when a base directory is set.
        let policy = PermissionPolicy::allow_all().with_base_dir(&base);
        assert!(
            policy
                .check_paths(&[CommandPath::new(&outside, PathAccess::Read)], &outside)
                .is_err()
        );
        assert!(
            policy
                .check_paths(&[CommandPath::new(&outside, PathAccess::Write)], &outside)
                .is_err()
        );
        // Relative paths still resolve against the live cwd.
        let relative = policy.check_paths(
            &[CommandPath::new(
                Path::new("../sutcac_cd_base/sub/rel.txt"),
                PathAccess::Write,
            )],
            &outside,
        );
        assert!(relative.is_ok());
    }

    #[test]
    fn read_paths_restrict_read_access() {
        let tmp = std::env::temp_dir();
        let allowed = tmp.join("sutcac_allowed");
        let _ = std::fs::create_dir_all(&allowed);
        let denied = tmp.join("sutcac_denied");
        let _ = std::fs::create_dir_all(&denied);

        let policy = PermissionPolicy::allow_all().with_paths(&[allowed.clone()], &[]);
        // Use allowed as cwd so cwd itself is allowed, but denied remains outside.
        policy
            .check_paths(&[CommandPath::new(&allowed, PathAccess::Read)], &allowed)
            .unwrap();
        assert!(
            policy
                .check_paths(&[CommandPath::new(&denied, PathAccess::Read)], &allowed)
                .is_err()
        );
    }

    #[test]
    fn write_paths_require_writable_list() {
        let tmp = std::env::temp_dir();
        let read_only = tmp.join("sutcac_readonly");
        let writable = tmp.join("sutcac_writable");
        let _ = std::fs::create_dir_all(&read_only);
        let _ = std::fs::create_dir_all(&writable);

        let policy =
            PermissionPolicy::allow_all().with_paths(&[read_only.clone()], &[writable.clone()]);
        // Use writable as cwd so cwd is writable, but read_only remains outside.
        policy
            .check_paths(&[CommandPath::new(&read_only, PathAccess::Read)], &writable)
            .unwrap();
        assert!(
            policy
                .check_paths(
                    &[CommandPath::new(&read_only, PathAccess::Write)],
                    &writable
                )
                .is_err()
        );
        policy
            .check_paths(&[CommandPath::new(&writable, PathAccess::Write)], &writable)
            .unwrap();
    }

    #[test]
    fn write_paths_are_readable() {
        let tmp = std::env::temp_dir();
        let writable = tmp.join("sutcac_rw");
        let _ = std::fs::create_dir_all(&writable);

        let policy = PermissionPolicy::allow_all().with_paths(&[], &[writable.clone()]);
        policy
            .check_paths(&[CommandPath::new(&writable, PathAccess::Read)], &tmp)
            .unwrap();
    }

    #[test]
    fn nonexistent_path_within_allowed_parent_is_ok() {
        let tmp = std::env::temp_dir();
        let allowed = tmp.join("sutcac_parent");
        let _ = std::fs::create_dir_all(&allowed);
        let child = allowed.join("does_not_exist_yet.txt");

        let policy = PermissionPolicy::allow_all().with_paths(&[], &[allowed.clone()]);
        policy
            .check_paths(&[CommandPath::new(&child, PathAccess::Write)], &tmp)
            .unwrap();
    }

    #[test]
    fn cwd_is_always_readable_and_writable() {
        let tmp = std::env::temp_dir().join("sutcac_cwd_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let file_in_cwd = tmp.join("in_cwd.txt");
        std::fs::write(&file_in_cwd, "data").unwrap();

        // Allowed paths only include an unrelated directory.
        let unrelated = std::env::temp_dir().join("sutcac_cwd_unrelated");
        let outside = std::env::temp_dir().join("sutcac_cwd_outside");
        let _ = std::fs::remove_dir_all(&unrelated);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let policy = PermissionPolicy::allow_all().with_paths(&[unrelated.clone()], &[]);

        // Reading and writing under cwd is allowed even though cwd is not in
        // the configured read_paths/write_paths.
        policy
            .check_paths(&[CommandPath::new(&file_in_cwd, PathAccess::Read)], &tmp)
            .unwrap();
        policy
            .check_paths(
                &[CommandPath::new(
                    tmp.join("new_file.txt"),
                    PathAccess::Write,
                )],
                &tmp,
            )
            .unwrap();

        // A path outside both cwd and allowed lists is still blocked.
        assert!(
            policy
                .check_paths(&[CommandPath::new(&outside, PathAccess::Read)], &tmp)
                .is_err()
        );
    }

    #[test]
    fn grant_paths_grants_session_path_access() {
        let tmp = std::env::temp_dir();
        let base = tmp.join("sutcac_grant_base");
        let readable = tmp.join("sutcac_grant_read");
        let writable = tmp.join("sutcac_grant_write");
        let other = tmp.join("sutcac_grant_other");
        for dir in [&base, &readable, &writable, &other] {
            let _ = std::fs::create_dir_all(dir);
        }

        let mut policy = PermissionPolicy::allow_all().with_base_dir(&base);

        // Before the grant everything outside the base is denied.
        assert!(
            policy
                .check_paths(&[CommandPath::new(&readable, PathAccess::Read)], &base)
                .is_err()
        );

        policy.grant_paths(&[readable.clone()], &[writable.clone()], &base);

        // Granted read path is readable but not writable.
        policy
            .check_paths(&[CommandPath::new(&readable, PathAccess::Read)], &base)
            .unwrap();
        assert!(
            policy
                .check_paths(&[CommandPath::new(&readable, PathAccess::Write)], &base)
                .is_err()
        );

        // Granted write path is writable and implicitly readable.
        policy
            .check_paths(&[CommandPath::new(&writable, PathAccess::Write)], &base)
            .unwrap();
        policy
            .check_paths(&[CommandPath::new(&writable, PathAccess::Read)], &base)
            .unwrap();

        // Unrelated paths are still denied.
        assert!(
            policy
                .check_paths(&[CommandPath::new(&other, PathAccess::Read)], &base)
                .is_err()
        );

        // Duplicate grants do not pile up.
        policy.grant_paths(&[readable.clone()], &[], &base);
        assert_eq!(policy.read_paths.len(), 2); // readable + writable
        assert_eq!(policy.write_paths.len(), 1); // writable
    }

    #[test]
    fn grant_paths_resolves_nonexistent_children() {
        let tmp = std::env::temp_dir();
        let base = tmp.join("sutcac_grant_nb_base");
        let target = tmp.join("sutcac_grant_nb_target");
        let _ = std::fs::create_dir_all(&base);
        let _ = std::fs::create_dir_all(&target);

        let mut policy = PermissionPolicy::allow_all().with_base_dir(&base);
        // Grant a not-yet-existing file path: it resolves via its existing
        // parent and lands in the write list.
        policy.grant_paths(
            &[],
            &[tmp.join("sutcac_grant_nb_target/new_file.txt")],
            &base,
        );
        policy
            .check_paths(
                &[CommandPath::new(
                    target.join("new_file.txt"),
                    PathAccess::Write,
                )],
                &base,
            )
            .unwrap();
        // Sibling files in the same directory remain denied: a granted file
        // path only covers that file.
        assert!(
            policy
                .check_paths(
                    &[CommandPath::new(
                        target.join("other.txt"),
                        PathAccess::Write
                    )],
                    &base,
                )
                .is_err()
        );
    }

    fn custom_set(tag: &str) -> PermissionSet {
        let mut set = PermissionSet::empty();
        set.insert(Permission::Custom(tag.to_string()));
        set
    }

    #[test]
    fn session_grant_overrides_mode() {
        let mut policy = PermissionPolicy::parse("allow:read deny:write").unwrap();
        assert!(policy.check(&PermissionSet::write()).is_err());
        policy.grant_tag("write");
        assert!(policy.check(&PermissionSet::write()).is_ok());
        // Session grants persist across checks.
        assert!(policy.check(&PermissionSet::write()).is_ok());
        // Unrelated tags are still denied.
        assert!(policy.check(&custom_set("NETWORK")).is_err());
    }

    #[test]
    fn revoke_grants_removes_session_tags() {
        let mut policy = PermissionPolicy::parse("deny:write").unwrap();
        policy.grant_tag("write,my_tag");
        assert!(policy.check(&PermissionSet::write()).is_ok());
        policy.revoke_grants("WRITE");
        assert!(policy.check(&PermissionSet::write()).is_err());
        assert!(policy.check(&custom_set("MY_TAG")).is_ok());
        policy.revoke_grants("unknown_tag");
        assert!(policy.check(&custom_set("MY_TAG")).is_ok());
    }

    #[test]
    fn grant_tag_is_case_insensitive_and_accepts_lists() {
        let mut policy = PermissionPolicy::parse("allow:read").unwrap();
        policy.grant_tag("Read,WRITE");
        assert!(policy.check(&PermissionSet::read_write()).is_ok());
        policy.grant_tag("my_tag");
        assert!(policy.check(&custom_set("MY_TAG")).is_ok());
    }
}
