//! Command execution.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};

use crate::ast::*;
use crate::audit::{AuditEvent, AuditLogger, AuditScope};
use crate::builtin;
use crate::expand::{ExpandContext, expand_single, expand_word};
use crate::parser::Parser;
use crate::permissions::{CommandPath, PathAccess, PermissionPolicy, PermissionSet};

/// Return a helpful error message when a command cannot be found.
fn command_not_found_message(name: &str) -> String {
    let builtins = builtin::names().join(", ");
    format!(
        "sutcac-sh: {}: command not found. Hint: check spelling and PATH; available built-ins are: {}.",
        name, builtins
    )
}

/// Return a helpful hint for an I/O error when opening a file or running a program.
fn io_error_hint(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::NotFound => "Hint: check that the path exists.",
        std::io::ErrorKind::PermissionDenied => {
            "Hint: check that the current user has permission to access the path."
        }
        _ => "Hint: check that the path exists and is accessible.",
    }
}

/// Return a helpful error message when spawning an external program fails.
fn exec_failed_message(program: &str, e: &std::io::Error) -> String {
    let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
        "Hint: check that the file is executable and the current user has permission."
    } else if e.kind() == std::io::ErrorKind::NotFound {
        "Hint: the executable file was removed after being located; check the path."
    } else {
        "Hint: check that the program exists and is executable."
    };
    format!("sutcac-sh: {}: {}. {}", program, e, hint)
}

/// Return a helpful error message when opening a redirect target fails.
fn redirect_failed_message(path: &str, e: &std::io::Error) -> String {
    format!("sutcac-sh: {}: {}. {}", path, e, io_error_hint(e))
}

/// Mutable shell state shared by builtins and the execution engine.
#[derive(Clone)]
pub struct ShellState {
    pub vars: HashMap<String, String>,
    pub exported: HashSet<String>,
    /// Positional parameters ($1, $2, ...).
    pub args: Vec<String>,
    /// Function definitions.
    pub funcs: HashMap<String, Command>,
    /// Exit status of the last command ($?).
    pub last_status: i32,
    /// Current shell process id ($$).
    pub pid: u32,
    /// Last background job pid ($!).
    pub last_bg_pid: Option<u32>,
    /// Current working directory.
    pub cwd: PathBuf,
    /// Permission policy enforced during execution.
    pub permissions: PermissionPolicy,
    /// Audit logger for command execution traces.
    pub audit_logger: AuditLogger,
    /// Whether the `exit` builtin should terminate the process.  Disabled by
    /// default so the shell can be safely embedded as a library (e.g. inside
    /// catus); the standalone binary enables it.
    pub exit_process: bool,
}

impl ShellState {
    pub fn new() -> Self {
        let pid = std::process::id();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut vars = HashMap::new();
        // Seed with environment variables so $VAR works for inherited env.
        for (k, v) in std::env::vars() {
            vars.insert(k, v);
        }
        Self {
            vars,
            exported: HashSet::new(),
            args: Vec::new(),
            funcs: HashMap::new(),
            last_status: 0,
            pid,
            last_bg_pid: None,
            cwd,
            permissions: PermissionPolicy::allow_all(),
            audit_logger: AuditLogger::stderr(),
            exit_process: false,
        }
    }

    pub fn with_policy_and_logger(permissions: PermissionPolicy, logger: AuditLogger) -> Self {
        let mut state = Self::new();
        state.permissions = permissions;
        state.audit_logger = logger;
        state
    }

    /// Replace the permission policy at runtime.
    pub fn set_permission_policy(&mut self, permissions: PermissionPolicy) {
        self.permissions = permissions;
    }

    /// Replace the audit logger at runtime.
    pub fn set_audit_logger(&mut self, logger: AuditLogger) {
        self.audit_logger = logger;
    }

    pub fn expand_context(&self) -> ExpandContext<'_> {
        ExpandContext::new(&self.vars, &self.args, self.last_status, self.pid)
    }
}

/// The result of executing a command: exit status plus captured stdout/stderr.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn new(status: i32) -> Self {
        Self {
            status,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    pub fn with_output(status: i32, stdout: String, stderr: String) -> Self {
        Self {
            status,
            stdout,
            stderr,
        }
    }
}

/// Return a short human-readable summary of a command for audit logs.
fn command_summary(cmd: &Command) -> String {
    match cmd {
        Command::Simple(s) => {
            if s.words.is_empty() && s.assignments.is_empty() {
                "(null command)".to_string()
            } else if s.words.is_empty() {
                let assigns: Vec<String> = s
                    .assignments
                    .iter()
                    .map(|(n, w)| format!("{}={}", n, w.value))
                    .collect();
                format!("assignments {}", assigns.join(" "))
            } else {
                s.words
                    .iter()
                    .map(|w| w.value.clone())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
        Command::Connection { op, .. } => format!("connection {:?}", op),
        Command::Pipeline(_) => "pipeline".to_string(),
        Command::Group(_) => "{ ... }".to_string(),
        Command::Subshell(_) => "( ... )".to_string(),
        Command::If { .. } => "if".to_string(),
        Command::While { .. } => "while".to_string(),
        Command::For { var, .. } => format!("for {}", var),
        Command::Case { .. } => "case".to_string(),
        Command::FunctionDef { name, .. } => format!("function {}", name),
    }
}

/// Return the permissions required by a command (top-level estimate for audit).
fn command_permissions(
    cmd: &Command,
    policy: &PermissionPolicy,
    state: &ShellState,
) -> PermissionSet {
    match cmd {
        Command::Simple(s) => simple_command_permissions(s, policy, state),
        Command::Pipeline(_) => PermissionSet::read().union(PermissionSet::write()),
        Command::Subshell(_) => PermissionSet::read().union(PermissionSet::write()),
        _ => PermissionSet::empty(),
    }
}

/// Return the permissions required by a simple command, including redirects.
fn simple_command_permissions(
    cmd: &SimpleCommand,
    policy: &PermissionPolicy,
    state: &ShellState,
) -> PermissionSet {
    let mut perms = PermissionSet::empty();
    let mut ctx = state.expand_context();
    for redir in &cmd.redirects {
        let target = expand_single(&redir.target.value, &mut ctx).unwrap_or_default();
        perms = perms.union(redirect_permissions_with_path(redir.kind, &target));
    }
    if let Some(name) = cmd.words.first().map(|w| w.value.as_str()) {
        if let Some(p) = builtin::permissions_for(name) {
            perms = perms.union(p);
        } else if let Some(p) = policy.permissions_for_command(name) {
            perms = perms.union(p.clone());
        } else {
            // External commands without an explicit estimate require full access.
            perms = perms
                .union(PermissionSet::read())
                .union(PermissionSet::write());
        }
    }
    perms
}

/// Paths that are harmless to open for reading or writing.  Redirects to these
/// paths do not require WRITE permission, which is important for patterns like
/// `2>/dev/null` under restrictive policies.
fn is_special_dev(path: &str) -> bool {
    path == "/dev/null"
}

/// Return the permissions required by a redirection kind and target path.
fn redirect_permissions_with_path(kind: RedirectKind, path: &str) -> PermissionSet {
    match kind {
        RedirectKind::Read | RedirectKind::Here | RedirectKind::DupInput => PermissionSet::read(),
        RedirectKind::Write | RedirectKind::Append | RedirectKind::ReadWrite => {
            if is_special_dev(path) {
                PermissionSet::empty()
            } else {
                PermissionSet::write()
            }
        }
        RedirectKind::DupOutput => {
            // Duplicating a file descriptor (e.g. 2>&1) does not itself open a
            // file, but if the target happens to name a special device we can
            // safely allow it without WRITE.
            if is_special_dev(path) {
                PermissionSet::empty()
            } else {
                PermissionSet::write()
            }
        }
    }
}

/// Return true if a string argument looks like a filesystem path.
fn looks_like_path(s: &str, cwd: &std::path::Path) -> bool {
    if s.starts_with('/') || s.starts_with("./") || s.starts_with("../") || s.starts_with('~') {
        return true;
    }
    if s.contains('/') {
        return true;
    }
    // Existing relative file/directory.
    cwd.join(s).exists()
}

/// Extract path arguments from an option-like token such as `-o=/tmp/foo`.
fn extract_path_from_option(s: &str) -> Option<&str> {
    let (_, value) = s.split_once('=')?;
    if value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
    {
        Some(value)
    } else {
        None
    }
}

/// Return the filesystem paths referenced by a simple command's arguments and
/// redirects, classified by access mode.
fn simple_command_paths(
    cmd: &SimpleCommand,
    argv: &[String],
    required: &PermissionSet,
    state: &ShellState,
) -> Vec<CommandPath> {
    let mut paths = Vec::new();

    // Redirect targets are always paths.
    let mut ctx = state.expand_context();
    for redir in &cmd.redirects {
        let target = expand_single(&redir.target.value, &mut ctx).unwrap_or_default();
        let access = match redir.kind {
            RedirectKind::Read | RedirectKind::Here | RedirectKind::DupInput => PathAccess::Read,
            RedirectKind::Write
            | RedirectKind::Append
            | RedirectKind::ReadWrite
            | RedirectKind::DupOutput => PathAccess::Write,
        };
        paths.push(CommandPath::new(target, access));
    }

    // Command argument paths.
    let arg_access = if required.contains(&crate::permissions::Permission::Write) {
        PathAccess::Write
    } else {
        PathAccess::Read
    };

    for arg in argv.iter().skip(1) {
        if looks_like_path(arg, &state.cwd) {
            paths.push(CommandPath::new(arg, arg_access));
        } else if let Some(path_part) = extract_path_from_option(arg) {
            paths.push(CommandPath::new(path_part, arg_access));
        }
    }

    paths
}

pub fn execute_command(cmd: &Command, state: &mut ShellState) -> CommandOutput {
    execute_command_scoped(cmd, state, AuditScope::TopLevel)
}

fn execute_command_scoped(
    cmd: &Command,
    state: &mut ShellState,
    scope: AuditScope,
) -> CommandOutput {
    match cmd {
        Command::Simple(c) => {
            let summary = command_summary(cmd);
            let required = command_permissions(cmd, &state.permissions, state);
            state.audit_logger.log(AuditEvent::CommandStart {
                cmd: summary.clone(),
                required,
                scope,
            });
            let output = execute_simple(c, state, scope);
            state.last_status = output.status;
            state.audit_logger.log(AuditEvent::CommandEnd {
                cmd: summary,
                status: output.status,
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                scope,
            });
            output
        }
        Command::Pipeline(cmds) => {
            let summary = command_summary(cmd);
            let required = command_permissions(cmd, &state.permissions, state);
            state.audit_logger.log(AuditEvent::CommandStart {
                cmd: summary.clone(),
                required,
                scope,
            });
            let output = execute_pipeline(cmds, state);
            state.last_status = output.status;
            state.audit_logger.log(AuditEvent::CommandEnd {
                cmd: summary,
                status: output.status,
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                scope,
            });
            output
        }
        Command::Connection { left, op, right } => {
            // Connection operators (`&&`, `||`, `;`) are not themselves audited;
            // their children are recorded as sub-commands.
            let left_output = execute_command_scoped(left, state, AuditScope::SubCommand);
            let right_output = match op {
                ListOp::Semi => execute_command_scoped(right, state, AuditScope::SubCommand),
                ListOp::And => {
                    if left_output.status == 0 {
                        execute_command_scoped(right, state, AuditScope::SubCommand)
                    } else {
                        left_output.clone()
                    }
                }
                ListOp::Or => {
                    if left_output.status != 0 {
                        execute_command_scoped(right, state, AuditScope::SubCommand)
                    } else {
                        left_output.clone()
                    }
                }
            };

            // Combine stdout/stderr from both sides so chained commands
            // (e.g., `cat file && echo done`) preserve all output.
            let mut stdout = left_output.stdout;
            stdout.push_str(&right_output.stdout);
            let mut stderr = left_output.stderr;
            stderr.push_str(&right_output.stderr);
            let output = CommandOutput::with_output(right_output.status, stdout, stderr);
            state.last_status = output.status;
            output
        }
        Command::Group(body) => {
            let output = execute_list(body, state, AuditScope::SubCommand);
            state.last_status = output.status;
            output
        }
        Command::Subshell(body) => {
            let output = execute_subshell(body, state);
            state.last_status = output.status;
            output
        }
        Command::If {
            cond,
            then_part,
            elifs,
            else_part,
        } => {
            let output = execute_if(cond, then_part, elifs, else_part, state);
            state.last_status = output.status;
            output
        }
        Command::While { cond, body } => {
            let output = execute_while(cond, body, state);
            state.last_status = output.status;
            output
        }
        Command::For { var, words, body } => {
            let output = execute_for(var, words, body, state);
            state.last_status = output.status;
            output
        }
        Command::Case { word, arms } => {
            let output = execute_case(word, arms, state);
            state.last_status = output.status;
            output
        }
        Command::FunctionDef { name, body } => {
            state.funcs.insert(name.clone(), *body.clone());
            CommandOutput::new(0)
        }
    }
}

fn execute_list(cmds: &[Command], state: &mut ShellState, scope: AuditScope) -> CommandOutput {
    let mut output = CommandOutput::new(0);
    for cmd in cmds {
        let next = execute_command_scoped(cmd, state, scope);
        output.status = next.status;
        output.stdout.push_str(&next.stdout);
        output.stderr.push_str(&next.stderr);
    }
    output
}

fn execute_simple(cmd: &SimpleCommand, state: &mut ShellState, scope: AuditScope) -> CommandOutput {
    // Expand assignments and words with command substitution support.  We use a
    // cloned shell state for substitutions so they cannot mutate the parent
    // state; the context is scoped so the original state can be used afterwards.
    let mut subst_state = state.clone();
    let mut executor =
        |inner: &str| -> Result<String, String> { execute_substitution(inner, &mut subst_state) };
    let (expanded_assignments, expanded) = {
        let mut ctx = expand_ctx_with_subst(state, &mut executor);

        // Expand environment assignments first.
        let mut assignments = Vec::new();
        for (name, word) in &cmd.assignments {
            let value = match expand_single(&word.value, &mut ctx) {
                Ok(v) => v,
                Err(e) => {
                    return CommandOutput::with_output(
                        1,
                        String::new(),
                        format!("sutcac-sh: {}", e),
                    );
                }
            };
            assignments.push((name.clone(), value));
        }

        if cmd.words.is_empty() {
            return CommandOutput::new(0);
        }

        let mut expanded_words: Vec<Vec<String>> = Vec::new();
        for word in &cmd.words {
            match expand_word(&word.value, &mut ctx) {
                Ok(fields) => expanded_words.push(fields),
                Err(e) => {
                    return CommandOutput::with_output(
                        1,
                        String::new(),
                        format!("sutcac-sh: {}", e),
                    );
                }
            }
        }
        (assignments, expanded_words)
    };

    // Apply the expanded assignments now that the expansion context is gone.
    for (name, value) in expanded_assignments {
        state.vars.insert(name, value);
    }

    if cmd.words.is_empty() {
        return CommandOutput::new(0);
    }

    // Flatten into a single argv. Each word expansion contributes one or more fields.
    let mut argv: Vec<String> = Vec::new();
    for fields in expanded {
        argv.extend(fields);
    }

    if argv.is_empty() {
        return CommandOutput::new(0);
    }

    let name = &argv[0];
    let args = &argv[1..];

    // Command lookup: function -> builtin -> external.
    let is_function = state.funcs.contains_key(name);
    if is_function {
        return execute_function(&state.funcs.get(name).unwrap().clone(), args, state);
    }

    // Permission check for builtins and external commands. Function bodies are
    // checked recursively when their internal commands execute.
    let required = simple_command_permissions(cmd, &state.permissions, state);

    // Path-based access control: check arguments and redirects before the
    // command is allowed to run.
    let command_paths = simple_command_paths(cmd, &argv, &required, state);
    if let Err(e) = state.permissions.check_paths(&command_paths, &state.cwd) {
        let msg = e.to_string();
        state.audit_logger.log(AuditEvent::PermissionDenied {
            cmd: command_summary(&Command::Simple(cmd.clone())),
            required,
            reason: msg.clone(),
            scope,
        });
        return CommandOutput::with_output(126, String::new(), format!("sutcac-sh: {}", msg));
    }

    if let Err(e) = state.permissions.check_command(name, &required) {
        let msg = e.to_string();
        state.audit_logger.log(AuditEvent::PermissionDenied {
            cmd: command_summary(&Command::Simple(cmd.clone())),
            required,
            reason: msg.clone(),
            scope,
        });
        return CommandOutput::with_output(126, String::new(), format!("sutcac-sh: {}", msg));
    }

    if builtin::is_builtin(name) {
        return execute_builtin_with_redirects(name, args, &cmd.redirects, state);
    }

    execute_external(name, args, &cmd.redirects, state)
}

/// Execute the commands inside a `$(...)` command substitution and return the
/// captured stdout.  The substitution runs in a cloned shell state so variable
/// and directory changes do not leak back to the parent.
fn execute_substitution(inner: &str, state: &mut ShellState) -> Result<String, String> {
    let mut parser = Parser::new(inner).map_err(|e| {
        format!(
            "command substitution: lexer error: {}. Hint: check the inner command syntax.",
            e
        )
    })?;
    let cmds = parser.parse().map_err(|e| {
        format!(
            "command substitution: parse error: {}. Hint: check the inner command syntax.",
            e
        )
    })?;
    let mut stdout = String::new();
    for cmd in cmds {
        let out = execute_command_scoped(&cmd, state, AuditScope::SubCommand);
        state.last_status = out.status;
        stdout.push_str(&out.stdout);
    }
    // Bash strips all trailing newlines from command substitution output.
    Ok(stdout.trim_end_matches('\n').to_string())
}

/// Build an expansion context that can execute `$(...)` command substitutions
/// using a cloned copy of the shell state.
fn expand_ctx_with_subst<'a>(
    state: &'a ShellState,
    executor: &'a mut dyn FnMut(&str) -> Result<String, String>,
) -> ExpandContext<'a> {
    ExpandContext {
        vars: &state.vars,
        args: &state.args,
        last_status: state.last_status,
        pid: state.pid,
        last_bg_pid: state.last_bg_pid,
        subst: Some(executor),
    }
}

fn execute_function(body: &Command, args: &[String], state: &mut ShellState) -> CommandOutput {
    let saved = std::mem::take(&mut state.args);
    state.args = args.to_vec();
    let output = execute_command_scoped(body, state, AuditScope::SubCommand);
    state.args = saved;
    output
}

fn execute_builtin_with_redirects(
    name: &str,
    args: &[String],
    redirects: &[Redirect],
    state: &mut ShellState,
) -> CommandOutput {
    let mut stdout_buf: Vec<u8> = Vec::new();
    let mut stderr_buf: Vec<u8> = Vec::new();

    let status = builtin::run_builtin(name, args, state, &mut stdout_buf, &mut stderr_buf);

    // Apply output redirections.
    let mut stdout_target: Option<File> = None;
    let mut stderr_target: Option<File> = None;
    for redir in redirects {
        match resolve_redirect(redir, state) {
            Ok((fd, kind, path)) => {
                let file = match kind {
                    RedirectKind::Write => OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(&path),
                    RedirectKind::Append => OpenOptions::new()
                        .write(true)
                        .create(true)
                        .append(true)
                        .open(&path),
                    RedirectKind::Read => {
                        // Input redirect on builtin: not useful for most builtins; ignore.
                        continue;
                    }
                    _ => continue,
                };
                match file {
                    Ok(f) => {
                        if fd == 1 || fd == -1 {
                            stdout_target = Some(f);
                        } else if fd == 2 {
                            stderr_target = Some(f);
                        }
                    }
                    Err(e) => {
                        return CommandOutput::with_output(
                            1,
                            String::new(),
                            redirect_failed_message(&path, &e),
                        );
                    }
                }
            }
            Err(e) => {
                return CommandOutput::with_output(1, String::new(), format!("sutcac-sh: {}", e));
            }
        }
    }

    if let Some(mut f) = stdout_target {
        let _ = f.write_all(&stdout_buf);
        stdout_buf.clear();
    }

    if let Some(mut f) = stderr_target {
        let _ = f.write_all(&stderr_buf);
        stderr_buf.clear();
    }

    // Only capture output that actually reached stdout/stderr.
    let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
    let stderr = String::from_utf8_lossy(&stderr_buf).to_string();

    CommandOutput::with_output(status, stdout, stderr)
}

fn execute_external(
    name: &str,
    args: &[String],
    redirects: &[Redirect],
    state: &mut ShellState,
) -> CommandOutput {
    let program = if name.contains('/') {
        name.to_string()
    } else {
        match find_in_path(name) {
            Some(p) => p,
            None => {
                return CommandOutput::with_output(
                    127,
                    String::new(),
                    command_not_found_message(name),
                );
            }
        }
    };

    let mut cmd = ProcessCommand::new(&program);
    cmd.args(args);

    // Apply environment assignments for this command only.
    for (k, v) in &state.vars {
        cmd.env(k, v);
    }

    // Build redirect map: fd -> file path/kind.
    let mut stdin_file: Option<File> = None;
    let mut stdout_file: Option<File> = None;
    let mut stderr_file: Option<File> = None;
    let mut merge_stderr_to_stdout = false;

    for redir in redirects {
        match resolve_redirect(redir, state) {
            Ok((fd, kind, path)) => {
                match kind {
                    RedirectKind::Read => match File::open(&path) {
                        Ok(f) => {
                            if fd == 0 || fd == -1 {
                                stdin_file = Some(f);
                            }
                        }
                        Err(e) => {
                            return CommandOutput::with_output(
                                1,
                                String::new(),
                                redirect_failed_message(&path, &e),
                            );
                        }
                    },
                    RedirectKind::Write => {
                        match OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .open(&path)
                        {
                            Ok(f) => {
                                if fd == 1 || fd == -1 {
                                    stdout_file = Some(f);
                                } else if fd == 2 {
                                    stderr_file = Some(f);
                                }
                            }
                            Err(e) => {
                                return CommandOutput::with_output(
                                    1,
                                    String::new(),
                                    redirect_failed_message(&path, &e),
                                );
                            }
                        }
                    }
                    RedirectKind::Append => {
                        match OpenOptions::new()
                            .write(true)
                            .create(true)
                            .append(true)
                            .open(&path)
                        {
                            Ok(f) => {
                                if fd == 1 || fd == -1 {
                                    stdout_file = Some(f);
                                } else if fd == 2 {
                                    stderr_file = Some(f);
                                }
                            }
                            Err(e) => {
                                return CommandOutput::with_output(
                                    1,
                                    String::new(),
                                    redirect_failed_message(&path, &e),
                                );
                            }
                        }
                    }
                    RedirectKind::DupOutput | RedirectKind::DupInput => {
                        // Simplified: 2>&1 means redirect stderr to same place as stdout.
                        if fd == 2 && path == "1" {
                            merge_stderr_to_stdout = true;
                        }
                    }
                    _ => {}
                }
            }
            Err(e) => {
                return CommandOutput::with_output(1, String::new(), format!("sutcac-sh: {}", e));
            }
        }
    }

    if let Some(f) = stdin_file {
        cmd.stdin(Stdio::from(f));
    }

    // By default capture stdout/stderr for the audit log. If a stream is
    // redirected to a file, leave it out of the captured output.
    let capture_stdout = stdout_file.is_none();
    let capture_stderr = stderr_file.is_none() && !merge_stderr_to_stdout;

    if let Some(f) = stdout_file {
        cmd.stdout(Stdio::from(f));
    } else {
        cmd.stdout(Stdio::piped());
    }

    if let Some(f) = stderr_file {
        cmd.stderr(Stdio::from(f));
    } else if merge_stderr_to_stdout {
        if capture_stdout {
            cmd.stderr(Stdio::piped());
        } else {
            cmd.stderr(Stdio::inherit());
        }
    } else {
        cmd.stderr(Stdio::piped());
    }

    match cmd.output() {
        Ok(output) => {
            let status = output.status.code().unwrap_or(126);
            let mut stdout = String::new();
            let mut stderr = String::new();

            if capture_stdout {
                if merge_stderr_to_stdout {
                    // Merge captured stderr into stdout.
                    let mut combined = output.stdout;
                    combined.extend_from_slice(&output.stderr);
                    stdout = String::from_utf8_lossy(&combined).to_string();
                } else {
                    stdout = String::from_utf8_lossy(&output.stdout).to_string();
                }
            } else if merge_stderr_to_stdout {
                // stderr already inherited the stdout file descriptor.
            }

            if capture_stderr {
                stderr = String::from_utf8_lossy(&output.stderr).to_string();
            }

            CommandOutput::with_output(status, stdout, stderr)
        }
        Err(e) => CommandOutput::with_output(126, String::new(), exec_failed_message(&program, &e)),
    }
}

/// Create an OS pipe with two write ends sharing the same read end.
///
/// This is used to implement `2>&1` for intermediate pipeline stages so that
/// both stdout and stderr of a process write into the pipe consumed by the
/// next stage, instead of stderr leaking onto the terminal.
#[cfg(unix)]
fn create_merged_stdout_stderr_pipe() -> io::Result<(File, File, File)> {
    use std::os::unix::io::{FromRawFd, RawFd};

    let mut fds: [RawFd; 2] = [-1; 2];
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let read_fd = fds[0];
        let write_fd = fds[1];
        let write_dup = libc::dup(write_fd);
        if write_dup < 0 {
            libc::close(read_fd);
            libc::close(write_fd);
            return Err(io::Error::last_os_error());
        }
        Ok((
            File::from_raw_fd(read_fd),
            File::from_raw_fd(write_fd),
            File::from_raw_fd(write_dup),
        ))
    }
}

fn execute_pipeline(cmds: &[Command], state: &mut ShellState) -> CommandOutput {
    // For simplicity, require all pipeline elements to be external simple commands.
    let mut externals: Vec<(String, Vec<String>, Vec<Redirect>)> = Vec::new();

    // Expand pipeline words with command substitution support.
    let mut subst_state = state.clone();
    let mut executor =
        |inner: &str| -> Result<String, String> { execute_substitution(inner, &mut subst_state) };
    let mut ctx = expand_ctx_with_subst(state, &mut executor);

    for cmd in cmds {
        match cmd {
            Command::Simple(c) => {
                let mut argv = Vec::new();
                for word in &c.words {
                    match expand_word(&word.value, &mut ctx) {
                        Ok(fields) => argv.extend(fields),
                        Err(e) => {
                            return CommandOutput::with_output(
                                1,
                                String::new(),
                                format!("sutcac-sh: {}", e),
                            );
                        }
                    }
                }
                if argv.is_empty() {
                    return CommandOutput::with_output(
                        1,
                        String::new(),
                        String::from(
                            "sutcac-sh: invalid null command in pipeline. Hint: every segment of a pipeline must be a non-empty command.",
                        ),
                    );
                }

                // Path-based access control for this pipeline segment.
                let required = simple_command_permissions(c, &state.permissions, state);
                let command_paths = simple_command_paths(c, &argv, &required, state);
                if let Err(e) = state.permissions.check_paths(&command_paths, &state.cwd) {
                    return CommandOutput::with_output(
                        126,
                        String::new(),
                        format!("sutcac-sh: {}", e),
                    );
                }

                let name = argv[0].clone();
                let args = argv[1..].to_vec();
                externals.push((name, args, c.redirects.clone()));
            }
            _ => {
                return CommandOutput::with_output(
                    1,
                    String::new(),
                    String::from(
                        "sutcac-sh: only external commands are supported in pipelines. Hint: avoid builtins, assignments, and compound commands inside pipelines; run them separately.",
                    ),
                );
            }
        }
    }

    if externals.is_empty() {
        return CommandOutput::new(0);
    }
    if externals.len() == 1 {
        let (name, args, redirects) = externals.into_iter().next().unwrap();
        return execute_external(&name, &args, &redirects, state);
    }

    // Build a chain of processes.
    let mut children = Vec::new();
    let mut prev_stdout: Option<Stdio> = None;
    let mut last_stdout: Option<std::process::ChildStdout> = None;
    let mut last_stderr: Option<std::process::ChildStderr> = None;
    let mut last_status = 0;
    let mut last_merge_stderr = false;
    let mut last_capture_stdout = false;

    for (i, (name, args, redirects)) in externals.iter().enumerate() {
        let program = if name.contains('/') {
            name.clone()
        } else {
            match find_in_path(name) {
                Some(p) => p,
                None => {
                    return CommandOutput::with_output(
                        127,
                        String::new(),
                        command_not_found_message(name),
                    );
                }
            }
        };

        let mut cmd = ProcessCommand::new(&program);
        cmd.args(args);

        // Apply redirects for this segment.
        let mut stdin_file: Option<File> = None;
        let mut stdout_file: Option<File> = None;
        let mut stderr_file: Option<File> = None;
        let mut merge_stderr_to_stdout = false;

        for redir in redirects {
            match resolve_redirect(redir, state) {
                Ok((fd, kind, path)) => match kind {
                    RedirectKind::Read => {
                        if let Ok(f) = File::open(&path) {
                            if fd == 0 || fd == -1 {
                                stdin_file = Some(f);
                            }
                        }
                    }
                    RedirectKind::Write | RedirectKind::Append => {
                        let file = if kind == RedirectKind::Write {
                            OpenOptions::new()
                                .write(true)
                                .create(true)
                                .truncate(true)
                                .open(&path)
                        } else {
                            OpenOptions::new()
                                .write(true)
                                .create(true)
                                .append(true)
                                .open(&path)
                        };
                        if let Ok(f) = file {
                            if fd == 1 || fd == -1 {
                                stdout_file = Some(f);
                            } else if fd == 2 {
                                stderr_file = Some(f);
                            }
                        }
                    }
                    RedirectKind::DupOutput | RedirectKind::DupInput => {
                        if fd == 2 && path == "1" {
                            merge_stderr_to_stdout = true;
                        }
                    }
                    _ => {}
                },
                Err(e) => {
                    return CommandOutput::with_output(
                        1,
                        String::new(),
                        format!("sutcac-sh: {}", e),
                    );
                }
            }
        }

        let is_last = i == externals.len() - 1;
        let capture_stdout = is_last && stdout_file.is_none();

        if is_last {
            last_merge_stderr = merge_stderr_to_stdout;
            last_capture_stdout = capture_stdout;
        }

        if i == 0 {
            if let Some(f) = stdin_file {
                cmd.stdin(Stdio::from(f));
            }
        } else {
            cmd.stdin(prev_stdout.take().unwrap());
        }

        if is_last {
            if let Some(f) = stdout_file {
                cmd.stdout(Stdio::from(f));
            } else {
                cmd.stdout(Stdio::piped());
            }
        } else {
            cmd.stdout(Stdio::piped());
        }

        if let Some(f) = stderr_file {
            cmd.stderr(Stdio::from(f));
        } else if is_last && merge_stderr_to_stdout {
            if capture_stdout {
                cmd.stderr(Stdio::piped());
            } else {
                cmd.stderr(Stdio::inherit());
            }
        } else if is_last {
            cmd.stderr(Stdio::piped());
        } else if merge_stderr_to_stdout {
            // For intermediate stages, merge stderr into the stdout pipe so
            // constructs like `cmd 2>&1 | tail` capture stderr in the pipeline
            // instead of writing it to the terminal and corrupting the TUI.
            #[cfg(unix)]
            {
                match create_merged_stdout_stderr_pipe() {
                    Ok((read_end, stdout_write, stderr_write)) => {
                        // Revert the default piped stdout so we can use our
                        // custom pipe for both streams.
                        cmd.stdout(Stdio::from(stdout_write));
                        cmd.stderr(Stdio::from(stderr_write));
                        prev_stdout = Some(Stdio::from(read_end));
                        // Skip the default stdout handling below.
                        match cmd.spawn() {
                            Ok(child) => {
                                children.push(child);
                            }
                            Err(e) => {
                                return CommandOutput::with_output(
                                    126,
                                    String::new(),
                                    exec_failed_message(&program, &e),
                                );
                            }
                        }
                        continue;
                    }
                    Err(_) => {
                        // Fall back to inheriting stderr if pipe creation fails.
                    }
                }
            }
            // Non-Unix fallback (or Unix pipe creation failure): discard stderr
            // so it cannot overwrite the TUI.
            cmd.stderr(Stdio::null());
        } else {
            // Intermediate stages normally inherit stderr; inside a TUI that
            // leaks onto the terminal, so discard it instead.
            cmd.stderr(Stdio::null());
        }

        match cmd.spawn() {
            Ok(mut child) => {
                if is_last {
                    last_stdout = child.stdout.take();
                    last_stderr = child.stderr.take();
                } else {
                    prev_stdout = child.stdout.take().map(Stdio::from);
                }
                children.push(child);
            }
            Err(e) => {
                return CommandOutput::with_output(
                    126,
                    String::new(),
                    exec_failed_message(&program, &e),
                );
            }
        }
    }

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    if let Some(mut reader) = last_stdout {
        let _ = reader.read_to_end(&mut stdout_buf);
    }
    if let Some(mut reader) = last_stderr {
        let _ = reader.read_to_end(&mut stderr_buf);
    }

    // If stderr was merged into stdout, combine the captured bytes.
    if last_merge_stderr && last_capture_stdout {
        stdout_buf.extend_from_slice(&stderr_buf);
        stderr_buf.clear();
    }

    for mut child in children {
        if let Ok(status) = child.wait() {
            last_status = status.code().unwrap_or(126);
        }
    }

    let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
    let stderr = String::from_utf8_lossy(&stderr_buf).to_string();

    CommandOutput::with_output(last_status, stdout, stderr)
}

fn execute_subshell(body: &[Command], state: &mut ShellState) -> CommandOutput {
    // Simulate a subshell by cloning state, executing, and discarding mutations.
    // The audit logger is shared so the trace remains complete.
    let mut subshell_state = state.clone();
    execute_list(body, &mut subshell_state, AuditScope::SubCommand)
}

fn execute_if(
    cond: &Command,
    then_part: &[Command],
    elifs: &[(Command, Vec<Command>)],
    else_part: &[Command],
    state: &mut ShellState,
) -> CommandOutput {
    if execute_command_scoped(cond, state, AuditScope::SubCommand).status == 0 {
        return execute_list(then_part, state, AuditScope::SubCommand);
    }
    for (elif_cond, elif_body) in elifs {
        if execute_command_scoped(elif_cond, state, AuditScope::SubCommand).status == 0 {
            return execute_list(elif_body, state, AuditScope::SubCommand);
        }
    }
    execute_list(else_part, state, AuditScope::SubCommand)
}

fn execute_while(cond: &Command, body: &[Command], state: &mut ShellState) -> CommandOutput {
    let mut output = CommandOutput::new(0);
    loop {
        if execute_command_scoped(cond, state, AuditScope::SubCommand).status != 0 {
            break;
        }
        let next = execute_list(body, state, AuditScope::SubCommand);
        output.status = next.status;
        output.stdout.push_str(&next.stdout);
        output.stderr.push_str(&next.stderr);
    }
    output
}

fn execute_for(
    var: &str,
    words: &[Word],
    body: &[Command],
    state: &mut ShellState,
) -> CommandOutput {
    let items: Vec<String> = if words.is_empty() {
        // Default to "$@".
        state.args.clone()
    } else {
        let mut subst_state = state.clone();
        let mut executor = |inner: &str| -> Result<String, String> {
            execute_substitution(inner, &mut subst_state)
        };
        let mut ctx = expand_ctx_with_subst(state, &mut executor);
        let mut items = Vec::new();
        for word in words {
            match expand_word(&word.value, &mut ctx) {
                Ok(fields) => items.extend(fields),
                Err(e) => {
                    return CommandOutput::with_output(
                        1,
                        String::new(),
                        format!("sutcac-sh: {}", e),
                    );
                }
            }
        }
        items
    };

    let mut output = CommandOutput::new(0);
    for item in items {
        state.vars.insert(var.to_string(), item);
        let next = execute_list(body, state, AuditScope::SubCommand);
        output.status = next.status;
        output.stdout.push_str(&next.stdout);
        output.stderr.push_str(&next.stderr);
    }
    output
}

fn execute_case(word: &Word, arms: &[CaseArm], state: &mut ShellState) -> CommandOutput {
    let mut subst_state = state.clone();
    let mut executor =
        |inner: &str| -> Result<String, String> { execute_substitution(inner, &mut subst_state) };
    let mut ctx = expand_ctx_with_subst(state, &mut executor);
    let value = match expand_single(&word.value, &mut ctx) {
        Ok(v) => v,
        Err(e) => {
            return CommandOutput::with_output(1, String::new(), format!("sutcac-sh: {}", e));
        }
    };
    for arm in arms {
        for pat_word in &arm.patterns {
            let pat = match expand_single(&pat_word.value, &mut ctx) {
                Ok(v) => v,
                Err(e) => {
                    return CommandOutput::with_output(
                        1,
                        String::new(),
                        format!("sutcac-sh: {}", e),
                    );
                }
            };
            // Bash case patterns are literal unless they contain glob chars.
            if crate::glob::matches(&pat, &value) {
                return execute_list(&arm.body, state, AuditScope::SubCommand);
            }
        }
    }
    CommandOutput::new(0)
}

/// Resolve a redirect to (fd, kind, target path/string).
fn resolve_redirect(
    redir: &Redirect,
    state: &ShellState,
) -> Result<(i32, RedirectKind, String), String> {
    let mut ctx = state.expand_context();
    let target = expand_single(&redir.target.value, &mut ctx)?;
    let fd = redir.fd.unwrap_or(default_fd(redir.kind));
    Ok((fd, redir.kind, target))
}

fn default_fd(kind: RedirectKind) -> i32 {
    match kind {
        RedirectKind::Read
        | RedirectKind::Here
        | RedirectKind::ReadWrite
        | RedirectKind::DupInput => 0,
        RedirectKind::Write | RedirectKind::Append | RedirectKind::DupOutput => 1,
    }
}

fn find_in_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;
    use std::io;
    use std::sync::{Arc, Mutex};

    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logger(format: crate::audit::AuditFormat) -> (AuditLogger, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let logger = AuditLogger::new(Box::new(CaptureWriter(buf.clone())), format);
        (logger, buf)
    }

    #[test]
    fn execute_true() {
        let mut state = ShellState::new();
        let cmd = Command::Simple(SimpleCommand {
            words: vec![crate::ast::Word::new("true")],
            ..Default::default()
        });
        assert_eq!(execute_command(&cmd, &mut state).status, 0);
    }

    #[test]
    fn execute_false() {
        let mut state = ShellState::new();
        let cmd = Command::Simple(SimpleCommand {
            words: vec![crate::ast::Word::new("false")],
            ..Default::default()
        });
        assert_eq!(execute_command(&cmd, &mut state).status, 1);
    }

    #[test]
    fn echo_captures_output() {
        let mut state = ShellState::new();
        let cmd = Command::Simple(SimpleCommand {
            words: vec![
                crate::ast::Word::new("echo"),
                crate::ast::Word::new("hello"),
            ],
            ..Default::default()
        });
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout, "hello\n");
        assert_eq!(output.stderr, "");
    }

    #[test]
    fn connection_preserves_left_output() {
        let mut state = ShellState::new();
        let mut parser = Parser::new("echo left && echo right").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert!(
            output.stdout.contains("left"),
            "stdout missing left: {:?}",
            output.stdout
        );
        assert!(
            output.stdout.contains("right"),
            "stdout missing right: {:?}",
            output.stdout
        );
    }

    #[test]
    fn deny_read_blocks_cd() {
        let (logger, buf) = capture_logger(crate::audit::AuditFormat::Text);
        let policy = PermissionPolicy::parse("deny:read").unwrap();
        let mut state = ShellState::with_policy_and_logger(policy, logger);
        let cmd = Command::Simple(SimpleCommand {
            words: vec![crate::ast::Word::new("cd"), crate::ast::Word::new("/tmp")],
            ..Default::default()
        });
        assert_eq!(execute_command(&cmd, &mut state).status, 126);
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("DENIED"));
        assert!(text.contains("cd /tmp"));
    }

    #[test]
    fn deny_write_blocks_export() {
        let policy = PermissionPolicy::parse("deny:write").unwrap();
        let mut state = ShellState::with_policy_and_logger(policy, AuditLogger::null());
        let cmd = Command::Simple(SimpleCommand {
            words: vec![
                crate::ast::Word::new("export"),
                crate::ast::Word::new("FOO=bar"),
            ],
            ..Default::default()
        });
        assert_eq!(execute_command(&cmd, &mut state).status, 126);
    }

    #[test]
    fn deny_read_blocks_test() {
        let policy = PermissionPolicy::parse("deny:read").unwrap();
        let mut state = ShellState::with_policy_and_logger(policy, AuditLogger::null());
        let cmd = Command::Simple(SimpleCommand {
            words: vec![
                crate::ast::Word::new("test"),
                crate::ast::Word::new("-e"),
                crate::ast::Word::new("/"),
            ],
            ..Default::default()
        });
        assert_eq!(execute_command(&cmd, &mut state).status, 126);
    }

    #[test]
    fn allow_read_permits_test() {
        let policy = PermissionPolicy::parse("allow:read").unwrap();
        let mut state = ShellState::with_policy_and_logger(policy, AuditLogger::null());
        let cmd = Command::Simple(SimpleCommand {
            words: vec![
                crate::ast::Word::new("test"),
                crate::ast::Word::new("-e"),
                crate::ast::Word::new("/"),
            ],
            ..Default::default()
        });
        assert_eq!(execute_command(&cmd, &mut state).status, 0);
    }

    #[test]
    fn read_path_blocks_external_read_outside_allowed() {
        let tmp = std::env::temp_dir().join("sutcac_path_test_read");
        let allowed = tmp.join("allowed");
        let denied = tmp.join("denied");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();
        std::fs::write(allowed.join("ok.txt"), "ok").unwrap();
        std::fs::write(denied.join("secret.txt"), "secret").unwrap();

        let mut commands = HashMap::new();
        commands.insert("cat".to_string(), PermissionSet::read());
        let policy = PermissionPolicy::parse_with_commands("allow_all", commands)
            .unwrap()
            .with_paths(&[allowed.clone()], &[]);
        let mut state = ShellState::with_policy_and_logger(policy, AuditLogger::null());

        let cmd = Command::Simple(SimpleCommand {
            words: vec![
                crate::ast::Word::new("cat"),
                crate::ast::Word::new(denied.join("secret.txt").to_str().unwrap()),
            ],
            ..Default::default()
        });
        assert_eq!(execute_command(&cmd, &mut state).status, 126);

        let cmd = Command::Simple(SimpleCommand {
            words: vec![
                crate::ast::Word::new("cat"),
                crate::ast::Word::new(allowed.join("ok.txt").to_str().unwrap()),
            ],
            ..Default::default()
        });
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert!(output.stdout.contains("ok"));
    }

    #[test]
    fn write_path_blocks_redirect_outside_allowed() {
        let tmp = std::env::temp_dir().join("sutcac_path_test_write");
        let allowed = tmp.join("allowed");
        let denied = tmp.join("denied");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();

        let policy = PermissionPolicy::allow_all().with_paths(&[], &[allowed.clone()]);
        let mut state = ShellState::with_policy_and_logger(policy, AuditLogger::null());

        let blocked_cmd = format!(
            "echo blocked > {}",
            denied.join("out.txt").to_str().unwrap()
        );
        let mut parser = Parser::new(&blocked_cmd).unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        assert_eq!(execute_command(&cmd, &mut state).status, 126);

        let ok_cmd = format!("echo ok > {}", allowed.join("out.txt").to_str().unwrap());
        let mut parser = Parser::new(&ok_cmd).unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        let contents = std::fs::read_to_string(allowed.join("out.txt")).unwrap();
        assert!(contents.contains("ok"));
    }

    #[test]
    fn read_path_allows_pipeline_read_within_allowed() {
        let tmp = std::env::temp_dir().join("sutcac_path_test_pipe");
        let allowed = tmp.join("allowed");
        let denied = tmp.join("denied");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();
        std::fs::write(allowed.join("ok.txt"), "ok").unwrap();
        std::fs::write(denied.join("secret.txt"), "secret").unwrap();

        let mut commands = HashMap::new();
        commands.insert("cat".to_string(), PermissionSet::read());
        let policy = PermissionPolicy::parse_with_commands("allow_all", commands)
            .unwrap()
            .with_paths(&[allowed.clone()], &[]);
        let mut state = ShellState::with_policy_and_logger(policy, AuditLogger::null());

        let allowed_pipe = format!("cat {} | cat", allowed.join("ok.txt").to_str().unwrap());
        let mut parser = Parser::new(&allowed_pipe).unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert!(output.stdout.contains("ok"));

        let pipe_cmd = format!("cat {} | cat", denied.join("secret.txt").to_str().unwrap());
        let mut parser = Parser::new(&pipe_cmd).unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        assert_eq!(execute_command(&cmd, &mut state).status, 126);
    }

    #[test]
    fn audit_logs_start_and_end() {
        let (logger, buf) = capture_logger(crate::audit::AuditFormat::Text);
        let mut state = ShellState::with_policy_and_logger(PermissionPolicy::allow_all(), logger);
        let cmd = Command::Simple(SimpleCommand {
            words: vec![crate::ast::Word::new("true")],
            ..Default::default()
        });
        assert_eq!(execute_command(&cmd, &mut state).status, 0);
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("START cmd=\"true\" scope=top"));
        assert!(text.contains("END cmd=\"true\" scope=top status=0"));
    }

    #[test]
    fn connection_operators_are_not_audited_children_are_sub() {
        let (logger, buf) = capture_logger(crate::audit::AuditFormat::Text);
        let mut state = ShellState::with_policy_and_logger(PermissionPolicy::allow_all(), logger);
        let mut parser = Parser::new("echo left && echo right").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            !text.contains("connection"),
            "connection operator should not be audited: {}",
            text
        );
        assert!(text.contains("START cmd=\"echo left\" scope=sub"));
        assert!(text.contains("START cmd=\"echo right\" scope=sub"));
    }

    #[test]
    fn audit_logs_stdout_json() {
        use crate::audit::AuditFormat;
        let (logger, buf) = capture_logger(AuditFormat::Json);
        let mut state = ShellState::with_policy_and_logger(PermissionPolicy::allow_all(), logger);
        let cmd = Command::Simple(SimpleCommand {
            words: vec![crate::ast::Word::new("echo"), crate::ast::Word::new("hi")],
            ..Default::default()
        });
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout, "hi\n");

        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("\"event\":\"end\""));
        assert!(text.contains("\"stdout\":\"hi\\n\""));
        assert!(text.contains("\"stderr\":\"\""));
    }

    #[test]
    fn external_command_captures_output() {
        let mut state = ShellState::new();
        let cmd = Command::Simple(SimpleCommand {
            words: vec![
                crate::ast::Word::new("printf"),
                crate::ast::Word::new("out"),
            ],
            ..Default::default()
        });
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout, "out");
    }

    #[test]
    fn pipeline_captures_stdout() {
        let mut state = ShellState::new();
        let mut parser = Parser::new("/bin/echo hello | /usr/bin/tr a-z A-Z").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout.trim(), "HELLO");
    }

    #[test]
    fn pipeline_merges_stderr_into_stdout() {
        let mut state = ShellState::new();
        // /bin/sh -c 'echo err >&2; echo out' writes to both streams.
        // With 2>&1 the stderr line should appear in the captured stdout.
        let mut parser =
            Parser::new("/bin/sh -c 'echo err >&2; echo out' 2>&1 | /usr/bin/sort").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert!(
            output.stdout.contains("err"),
            "stderr should be merged into stdout, got: {:?}",
            output.stdout
        );
        assert!(
            output.stdout.contains("out"),
            "stdout should contain out, got: {:?}",
            output.stdout
        );
    }

    #[test]
    fn pipeline_intermediate_stderr_is_not_inherited() {
        let mut state = ShellState::new();
        // Intermediate process writes only to stderr; without 2>&1 that stderr
        // is discarded rather than inherited by the parent terminal.
        let mut parser = Parser::new("/bin/sh -c 'echo err >&2' | /bin/cat").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout, "");
        assert_eq!(output.stderr, "");
    }

    #[test]
    fn allow_read_permits_dev_null_redirect() {
        let policy = PermissionPolicy::parse("allow:read").unwrap();
        let mut state = ShellState::with_policy_and_logger(policy, AuditLogger::null());
        let mut parser = Parser::new("echo hi 2>/dev/null").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout, "hi\n");
        assert_eq!(output.stderr, "");
    }

    #[test]
    fn allow_read_permits_git_log_with_config_override() {
        // With no hard-coded READ defaults, git must be explicitly tagged READ
        // via [shell.commands] for allow:read to permit it.
        let mut commands = HashMap::new();
        commands.insert("git".to_string(), PermissionSet::read());
        let policy = PermissionPolicy::parse_with_commands("allow:read", commands).unwrap();
        let mut state = ShellState::with_policy_and_logger(policy, AuditLogger::null());
        let mut parser = Parser::new("git log --oneline -1").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0, "stderr: {}", output.stderr);
        assert!(
            !output.stdout.is_empty(),
            "git log should produce output under allow:read when configured as READ"
        );
    }

    #[test]
    fn command_substitution_expands_inner_output() {
        let mut state = ShellState::new();
        let mut parser = Parser::new("echo $(echo hello)").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout.trim(), "hello");
    }

    #[test]
    fn group_command_accumulates_output() {
        let mut state = ShellState::new();
        let mut parser = Parser::new("{ echo a; echo b; }").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert!(output.stdout.contains("a"));
        assert!(output.stdout.contains("b"));
    }

    #[test]
    fn for_loop_accumulates_output() {
        let mut state = ShellState::new();
        let mut parser = Parser::new("for i in a b c; do echo $i; done").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert!(output.stdout.contains("a"));
        assert!(output.stdout.contains("b"));
        assert!(output.stdout.contains("c"));
    }

    #[test]
    fn literal_braces_are_preserved() {
        let mut state = ShellState::new();
        let mut parser = Parser::new("echo {}").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 0);
        assert!(output.stdout.contains("{}"), "stdout: {:?}", output.stdout);
    }

    #[test]
    fn exit_builtin_does_not_kill_library_context() {
        let mut state = ShellState::new();
        // The library default is exit_process=false, so exit should return a
        // status instead of terminating the process.
        let mut parser = Parser::new("exit 42").unwrap();
        let cmd = parser.parse().unwrap().remove(0);
        let output = execute_command(&cmd, &mut state);
        assert_eq!(output.status, 42);
    }
}
