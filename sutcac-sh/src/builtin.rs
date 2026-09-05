//! Built-in shell commands.

use std::collections::HashMap;
use std::io::Write;
use std::process;
use std::sync::LazyLock;

use crate::audit::AuditEvent;
use crate::exec::ShellState;
use crate::permissions::PermissionSet;

/// Interface for a shell built-in command.
///
/// Each implementation is registered once in the global `BUILTINS` table.
/// Adding a new built-in no longer requires editing a central `match`;
/// just implement `Builtin` and add the type to `all_builtins()`.
pub trait Builtin: Send + Sync {
    /// The command name used for lookup, e.g. `"echo"` or `"["`.
    fn name(&self) -> &'static str;

    /// Permissions required to execute this built-in.
    fn permissions(&self) -> PermissionSet;

    /// Execute the built-in and return its exit status.
    fn run(
        &self,
        args: &[String],
        state: &mut ShellState,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32;
}

/// Return all built-in implementations.
pub fn all_builtins() -> Vec<Box<dyn Builtin>> {
    vec![
        Box::new(ColonBuiltin),
        Box::new(TrueBuiltin),
        Box::new(FalseBuiltin),
        Box::new(EchoBuiltin),
        Box::new(EditBuiltin),
        Box::new(CdBuiltin),
        Box::new(PwdBuiltin),
        Box::new(ExportBuiltin),
        Box::new(ExitBuiltin),
        Box::new(ShiftBuiltin),
        Box::new(TestBuiltin::new("test")),
        Box::new(TestBuiltin::new("[")),
    ]
}

/// Global registry mapping command names to their `Builtin` implementation.
static BUILTINS: LazyLock<HashMap<&'static str, Box<dyn Builtin>>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    for builtin in all_builtins() {
        map.insert(builtin.name(), builtin);
    }
    map
});

/// Return `true` if `name` names a known built-in command.
pub fn is_builtin(name: &str) -> bool {
    BUILTINS.contains_key(name)
}

/// Return the permissions required by a built-in command, or `None` if not found.
pub fn permissions_for(name: &str) -> Option<PermissionSet> {
    BUILTINS.get(name).map(|b| b.permissions())
}

/// Return the names of all registered built-in commands, sorted.
pub fn names() -> Vec<&'static str> {
    let mut names: Vec<_> = BUILTINS.keys().copied().collect();
    names.sort();
    names
}

/// Run a built-in command by name. Returns the exit status.
pub fn run_builtin(
    name: &str,
    args: &[String],
    state: &mut ShellState,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    match BUILTINS.get(name) {
        Some(builtin) => builtin.run(args, state, stdout, stderr),
        None => {
            let _ = writeln!(
                stderr,
                "sutcac-sh: {}: not a builtin. Hint: use a valid built-in command name.",
                name
            );
            127
        }
    }
}

struct ColonBuiltin;
impl Builtin for ColonBuiltin {
    fn name(&self) -> &'static str {
        ":"
    }

    fn permissions(&self) -> PermissionSet {
        PermissionSet::empty()
    }

    fn run(
        &self,
        _args: &[String],
        _state: &mut ShellState,
        _stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        0
    }
}

struct TrueBuiltin;
impl Builtin for TrueBuiltin {
    fn name(&self) -> &'static str {
        "true"
    }

    fn permissions(&self) -> PermissionSet {
        PermissionSet::empty()
    }

    fn run(
        &self,
        _args: &[String],
        _state: &mut ShellState,
        _stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        0
    }
}

struct FalseBuiltin;
impl Builtin for FalseBuiltin {
    fn name(&self) -> &'static str {
        "false"
    }

    fn permissions(&self) -> PermissionSet {
        PermissionSet::empty()
    }

    fn run(
        &self,
        _args: &[String],
        _state: &mut ShellState,
        _stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        1
    }
}

struct EchoBuiltin;
impl Builtin for EchoBuiltin {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn permissions(&self) -> PermissionSet {
        // echo itself only writes to stdout; any file access comes from redirects.
        PermissionSet::empty()
    }

    fn run(
        &self,
        args: &[String],
        _state: &mut ShellState,
        stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        let mut newline = true;
        let mut start = 0;
        if let Some(first) = args.first() {
            if first == "-n" {
                newline = false;
                start = 1;
            }
        }
        let output = args[start..].join(" ");
        let _ = write!(stdout, "{}", output);
        if newline {
            let _ = writeln!(stdout);
        }
        0
    }
}

struct EditBuiltin;
impl Builtin for EditBuiltin {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn permissions(&self) -> PermissionSet {
        // edit reads the file, then writes it back.
        PermissionSet::read().union(PermissionSet::write())
    }

    fn run(
        &self,
        args: &[String],
        state: &mut ShellState,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let [file, old, new] = match args {
            [file, old, new] => [file, old, new],
            _ => {
                let _ = writeln!(
                    stderr,
                    "sutcac-sh: edit: usage: edit FILE OLD NEW. Hint: OLD must appear exactly once in FILE and is replaced by NEW."
                );
                return 2;
            }
        };

        let content = match std::fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                let _ = writeln!(
                    stderr,
                    "sutcac-sh: edit: {}: {}. Hint: check that the file exists and contains valid UTF-8 text.",
                    file, e
                );
                return 1;
            }
        };

        let occurrences = content.matches(old.as_str()).count();
        match occurrences {
            0 => {
                let _ = writeln!(
                    stderr,
                    "sutcac-sh: edit: {}: old text not found. Hint: re-read the file first to confirm its current content, then retry with an exact match.",
                    file
                );
                1
            }
            1 => {
                let new_content = content.replacen(old.as_str(), new.as_str(), 1);
                if let Err(e) = std::fs::write(file, &new_content) {
                    let _ = writeln!(
                        stderr,
                        "sutcac-sh: edit: {}: {}. Hint: check write permissions on the file.",
                        file, e
                    );
                    return 1;
                }
                state.audit_logger.log(AuditEvent::SideEffect {
                    cmd: "edit".into(),
                    description: format!(
                        "edited {}: {} bytes -> {} bytes",
                        file,
                        content.len(),
                        new_content.len()
                    ),
                });
                0
            }
            n => {
                let _ = writeln!(
                    stderr,
                    "sutcac-sh: edit: {}: old text matches {} locations. Hint: include more surrounding context in OLD so the match is unique.",
                    file, n
                );
                1
            }
        }
    }
}

struct CdBuiltin;
impl Builtin for CdBuiltin {
    fn name(&self) -> &'static str {
        "cd"
    }

    fn permissions(&self) -> PermissionSet {
        // cd is treated as read-only: each shell tool call restores the
        // original working directory after execution, so it cannot permanently
        // change the Agent's environment.
        PermissionSet::read()
    }

    fn run(
        &self,
        args: &[String],
        state: &mut ShellState,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let target = if args.is_empty() {
            match state.vars.get("HOME") {
                Some(h) => h.clone(),
                None => {
                    let _ = writeln!(
                        stderr,
                        "sutcac-sh: cd: HOME not set. Hint: specify a directory explicitly, e.g., cd /path/to/dir."
                    );
                    return 1;
                }
            }
        } else {
            args[0].clone()
        };
        let path = std::path::PathBuf::from(&target);
        match std::env::set_current_dir(&path) {
            Ok(()) => {
                state.cwd = std::env::current_dir().unwrap_or(path);
                state
                    .vars
                    .insert("PWD".into(), state.cwd.to_string_lossy().into_owned());
                state.audit_logger.log(AuditEvent::SideEffect {
                    cmd: "cd".into(),
                    description: format!("changed directory to {}", state.cwd.display()),
                });
                0
            }
            Err(e) => {
                let _ = writeln!(
                    stderr,
                    "sutcac-sh: cd: {}: {}. Hint: check that the directory exists and is accessible.",
                    target, e
                );
                1
            }
        }
    }
}

struct PwdBuiltin;
impl Builtin for PwdBuiltin {
    fn name(&self) -> &'static str {
        "pwd"
    }

    fn permissions(&self) -> PermissionSet {
        // pwd reads the current working directory.
        PermissionSet::read()
    }

    fn run(
        &self,
        _args: &[String],
        _state: &mut ShellState,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        match std::env::current_dir() {
            Ok(p) => {
                let _ = writeln!(stdout, "{}", p.display());
                0
            }
            Err(e) => {
                let _ = writeln!(
                    stderr,
                    "sutcac-sh: pwd: {}. Hint: the current working directory may have been removed or permissions changed.",
                    e
                );
                1
            }
        }
    }
}

struct ExportBuiltin;
impl Builtin for ExportBuiltin {
    fn name(&self) -> &'static str {
        "export"
    }

    fn permissions(&self) -> PermissionSet {
        // export modifies the process environment.
        PermissionSet::write()
    }

    fn run(
        &self,
        args: &[String],
        state: &mut ShellState,
        stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        if args.is_empty() {
            for (k, v) in &state.vars {
                if state.exported.contains(k) {
                    let _ = writeln!(stdout, "export {}=\"{}\"", k, v);
                }
            }
            return 0;
        }
        let mut changed = Vec::new();
        for arg in args {
            if let Some(pos) = arg.find('=') {
                let name = arg[..pos].to_string();
                let value = arg[pos + 1..].to_string();
                state.vars.insert(name.clone(), value.clone());
                state.exported.insert(name.clone());
                unsafe { std::env::set_var(&name, value) };
                changed.push(name);
            } else {
                state.exported.insert(arg.clone());
                if let Some(v) = state.vars.get(arg) {
                    unsafe { std::env::set_var(arg, v) };
                }
                changed.push(arg.clone());
            }
        }
        if !changed.is_empty() {
            state.audit_logger.log(AuditEvent::SideEffect {
                cmd: "export".into(),
                description: format!("exported variables: {}", changed.join(", ")),
            });
        }
        0
    }
}

struct ExitBuiltin;
impl Builtin for ExitBuiltin {
    fn name(&self) -> &'static str {
        "exit"
    }

    fn permissions(&self) -> PermissionSet {
        // exit terminates the shell process.
        PermissionSet::write()
    }

    fn run(
        &self,
        args: &[String],
        state: &mut ShellState,
        _stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        let code = args
            .first()
            .and_then(|s| s.parse().ok())
            .unwrap_or(state.last_status);
        state.audit_logger.log(AuditEvent::SideEffect {
            cmd: "exit".into(),
            description: format!("exiting shell with code {}", code),
        });
        if state.exit_process {
            process::exit(code);
        }
        code
    }
}

struct ShiftBuiltin;
impl Builtin for ShiftBuiltin {
    fn name(&self) -> &'static str {
        "shift"
    }

    fn permissions(&self) -> PermissionSet {
        // shift mutates shell state (positional parameters).
        PermissionSet::write()
    }

    fn run(
        &self,
        args: &[String],
        state: &mut ShellState,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let n = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
        if n > state.args.len() {
            let _ = writeln!(
                stderr,
                "sutcac-sh: shift: can't shift that many (only {} positional argument(s) available). Hint: use $# to check the argument count.",
                state.args.len()
            );
            return 1;
        }
        state.args = state.args[n..].to_vec();
        0
    }
}

struct TestBuiltin {
    name: &'static str,
}

impl TestBuiltin {
    fn new(name: &'static str) -> Self {
        Self { name }
    }
}

impl Builtin for TestBuiltin {
    fn name(&self) -> &'static str {
        self.name
    }

    fn permissions(&self) -> PermissionSet {
        // test may read the file system via -e/-f/-d.
        PermissionSet::read()
    }

    fn run(
        &self,
        args: &[String],
        _state: &mut ShellState,
        _stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        // Drop trailing ']' if invoked as '['.
        let args = if self.name == "[" && args.last().map(|s| s.as_str()) == Some("]") {
            &args[..args.len() - 1]
        } else {
            args
        };

        if args.is_empty() {
            return 1;
        }

        match args.len() {
            1 => {
                // -n string or string non-empty
                if args[0].is_empty() { 1 } else { 0 }
            }
            2 => match args[0].as_str() {
                "-n" => {
                    if args[1].is_empty() {
                        1
                    } else {
                        0
                    }
                }
                "-z" => {
                    if args[1].is_empty() {
                        0
                    } else {
                        1
                    }
                }
                "-e" => {
                    if path_exists(&args[1]) {
                        0
                    } else {
                        1
                    }
                }
                "-f" => {
                    if is_file(&args[1]) {
                        0
                    } else {
                        1
                    }
                }
                "-d" => {
                    if is_dir(&args[1]) {
                        0
                    } else {
                        1
                    }
                }
                _ => 1,
            },
            3 => {
                let a = &args[0];
                let op = args[1].as_str();
                let b = &args[2];
                match op {
                    "=" => {
                        if a == b {
                            0
                        } else {
                            1
                        }
                    }
                    "!=" => {
                        if a != b {
                            0
                        } else {
                            1
                        }
                    }
                    "-eq" => compare_ints(a, b, |x, y| x == y),
                    "-ne" => compare_ints(a, b, |x, y| x != y),
                    "-lt" => compare_ints(a, b, |x, y| x < y),
                    "-le" => compare_ints(a, b, |x, y| x <= y),
                    "-gt" => compare_ints(a, b, |x, y| x > y),
                    "-ge" => compare_ints(a, b, |x, y| x >= y),
                    _ => 1,
                }
            }
            _ => 1,
        }
    }
}

fn compare_ints(a: &str, b: &str, op: fn(i64, i64) -> bool) -> i32 {
    match (a.parse::<i64>(), b.parse::<i64>()) {
        (Ok(x), Ok(y)) => {
            if op(x, y) {
                0
            } else {
                1
            }
        }
        _ => 1,
    }
}

fn path_exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

fn is_file(path: &str) -> bool {
    std::path::Path::new(path).is_file()
}

fn is_dir(path: &str) -> bool {
    std::path::Path::new(path).is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, args: &[String], state: &mut ShellState) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let status = run_builtin(name, args, state, &mut out, &mut err);
        (
            status,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn echo_builtin() {
        let mut state = ShellState::new();
        let (s, out, _) = run("echo", &["hello".into(), "world".into()], &mut state);
        assert_eq!(s, 0);
        assert_eq!(out, "hello world\n");
    }

    #[test]
    fn test_builtin() {
        let mut state = ShellState::new();
        assert_eq!(
            run("test", &["5".into(), "-eq".into(), "5".into()], &mut state).0,
            0
        );
        assert_eq!(
            run("test", &["5".into(), "-ne".into(), "3".into()], &mut state).0,
            0
        );
        assert_eq!(run("test", &["-n".into(), "".into()], &mut state).0, 1);
    }

    #[test]
    fn edit_builtin_replaces_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello world\n").unwrap();
        let mut state = ShellState::new();
        let (s, _, err) = run(
            "edit",
            &[
                file.to_string_lossy().into_owned(),
                "world".into(),
                "rust".into(),
            ],
            &mut state,
        );
        assert_eq!(s, 0, "stderr: {}", err);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello rust\n");
    }

    #[test]
    fn edit_builtin_fails_when_old_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello world\n").unwrap();
        let mut state = ShellState::new();
        let (s, _, err) = run(
            "edit",
            &[
                file.to_string_lossy().into_owned(),
                "missing".into(),
                "rust".into(),
            ],
            &mut state,
        );
        assert_eq!(s, 1);
        assert!(err.contains("old text not found"), "stderr: {}", err);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world\n");
    }

    #[test]
    fn edit_builtin_fails_when_old_matches_multiple_locations() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "aaa bbb aaa\n").unwrap();
        let mut state = ShellState::new();
        let (s, _, err) = run(
            "edit",
            &[
                file.to_string_lossy().into_owned(),
                "aaa".into(),
                "xxx".into(),
            ],
            &mut state,
        );
        assert_eq!(s, 1);
        assert!(err.contains("matches 2 locations"), "stderr: {}", err);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "aaa bbb aaa\n");
    }

    #[test]
    fn edit_builtin_requires_exactly_three_args() {
        let mut state = ShellState::new();
        let (s, _, err) = run("edit", &["only-file".into()], &mut state);
        assert_eq!(s, 2);
        assert!(err.contains("usage: edit FILE OLD NEW"), "stderr: {}", err);
    }

    #[test]
    fn edit_builtin_fails_for_missing_file() {
        let mut state = ShellState::new();
        let (s, _, err) = run(
            "edit",
            &["/nonexistent/path/file.txt".into(), "a".into(), "b".into()],
            &mut state,
        );
        assert_eq!(s, 1);
        assert!(err.contains("No such file"), "stderr: {}", err);
    }

    #[test]
    fn builtin_permissions() {
        assert!(permissions_for(":").unwrap().is_empty());
        assert!(permissions_for("true").unwrap().is_empty());
        assert!(permissions_for("false").unwrap().is_empty());
        assert!(permissions_for("echo").unwrap().is_empty());
        assert!(
            permissions_for("edit")
                .unwrap()
                .contains(&crate::permissions::Permission::Read)
        );
        assert!(
            permissions_for("edit")
                .unwrap()
                .contains(&crate::permissions::Permission::Write)
        );
        assert!(
            permissions_for("pwd")
                .unwrap()
                .contains(&crate::permissions::Permission::Read)
        );
        assert!(
            permissions_for("cd")
                .unwrap()
                .contains(&crate::permissions::Permission::Read)
        );
        assert!(
            permissions_for("export")
                .unwrap()
                .contains(&crate::permissions::Permission::Write)
        );
        assert!(
            permissions_for("exit")
                .unwrap()
                .contains(&crate::permissions::Permission::Write)
        );
        assert!(
            permissions_for("shift")
                .unwrap()
                .contains(&crate::permissions::Permission::Write)
        );
        assert!(
            permissions_for("test")
                .unwrap()
                .contains(&crate::permissions::Permission::Read)
        );
        assert!(
            permissions_for("[")
                .unwrap()
                .contains(&crate::permissions::Permission::Read)
        );
    }
}
