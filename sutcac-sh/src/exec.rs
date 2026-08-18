//! Command execution.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};

use crate::ast::*;
use crate::audit::{AuditEvent, AuditLogger};
use crate::builtin;
use crate::expand::{ExpandContext, expand_single, expand_word};
use crate::permissions::{PermissionPolicy, PermissionSet};

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
        }
    }

    pub fn with_policy_and_logger(permissions: PermissionPolicy, logger: AuditLogger) -> Self {
        let mut state = Self::new();
        state.permissions = permissions;
        state.audit_logger = logger;
        state
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
fn command_permissions(cmd: &Command, policy: &PermissionPolicy) -> PermissionSet {
    match cmd {
        Command::Simple(s) => simple_command_permissions(s, policy),
        Command::Pipeline(_) => PermissionSet::read().union(PermissionSet::write()),
        Command::Subshell(_) => PermissionSet::read().union(PermissionSet::write()),
        _ => PermissionSet::empty(),
    }
}

/// Return the permissions required by a simple command, including redirects.
fn simple_command_permissions(cmd: &SimpleCommand, policy: &PermissionPolicy) -> PermissionSet {
    let mut perms = PermissionSet::empty();
    for redir in &cmd.redirects {
        perms = perms.union(redirect_permissions(redir.kind));
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

/// Return the permissions required by a redirection kind.
fn redirect_permissions(kind: RedirectKind) -> PermissionSet {
    match kind {
        RedirectKind::Read | RedirectKind::Here | RedirectKind::DupInput => PermissionSet::read(),
        RedirectKind::Write
        | RedirectKind::Append
        | RedirectKind::ReadWrite
        | RedirectKind::DupOutput => PermissionSet::write(),
    }
}

pub fn execute_command(cmd: &Command, state: &mut ShellState) -> CommandOutput {
    let summary = command_summary(cmd);
    let required = command_permissions(cmd, &state.permissions);
    state.audit_logger.log(AuditEvent::CommandStart {
        cmd: summary.clone(),
        required,
    });

    let output = match cmd {
        Command::Simple(c) => execute_simple(c, state),
        Command::Connection { left, op, right } => {
            let left_output = execute_command(left, state);
            let right_output = match op {
                ListOp::Semi => execute_command(right, state),
                ListOp::And => {
                    if left_output.status == 0 {
                        execute_command(right, state)
                    } else {
                        left_output.clone()
                    }
                }
                ListOp::Or => {
                    if left_output.status != 0 {
                        execute_command(right, state)
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
            CommandOutput::with_output(right_output.status, stdout, stderr)
        }
        Command::Pipeline(cmds) => execute_pipeline(cmds, state),
        Command::Group(body) => execute_list(body, state),
        Command::Subshell(body) => execute_subshell(body, state),
        Command::If {
            cond,
            then_part,
            elifs,
            else_part,
        } => execute_if(cond, then_part, elifs, else_part, state),
        Command::While { cond, body } => execute_while(cond, body, state),
        Command::For { var, words, body } => execute_for(var, words, body, state),
        Command::Case { word, arms } => execute_case(word, arms, state),
        Command::FunctionDef { name, body } => {
            state.funcs.insert(name.clone(), *body.clone());
            CommandOutput::new(0)
        }
    };

    state.last_status = output.status;
    state.audit_logger.log(AuditEvent::CommandEnd {
        cmd: summary,
        status: output.status,
        stdout: output.stdout.clone(),
        stderr: output.stderr.clone(),
    });
    output
}

fn execute_list(cmds: &[Command], state: &mut ShellState) -> CommandOutput {
    let mut output = CommandOutput::new(0);
    for cmd in cmds {
        output = execute_command(cmd, state);
    }
    output
}

fn execute_simple(cmd: &SimpleCommand, state: &mut ShellState) -> CommandOutput {
    // Apply environment assignments first.
    for (name, word) in &cmd.assignments {
        let value = match expand_single(&word.value, &state.expand_context()) {
            Ok(v) => v,
            Err(e) => {
                return CommandOutput::with_output(1, String::new(), format!("sutcac-sh: {}", e));
            }
        };
        state.vars.insert(name.clone(), value);
    }

    if cmd.words.is_empty() {
        return CommandOutput::new(0);
    }

    let ctx = state.expand_context();
    let mut expanded_words: Vec<Vec<String>> = Vec::new();
    for word in &cmd.words {
        match expand_word(&word.value, &ctx) {
            Ok(fields) => expanded_words.push(fields),
            Err(e) => {
                return CommandOutput::with_output(1, String::new(), format!("sutcac-sh: {}", e));
            }
        }
    }

    // Flatten into a single argv. Each word expansion contributes one or more fields.
    let mut argv: Vec<String> = Vec::new();
    for fields in expanded_words {
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
    let required = simple_command_permissions(cmd, &state.permissions);
    if let Err(e) = state.permissions.check(&required) {
        let msg = e.to_string();
        state.audit_logger.log(AuditEvent::PermissionDenied {
            cmd: command_summary(&Command::Simple(cmd.clone())),
            required,
            reason: msg.clone(),
        });
        return CommandOutput::with_output(126, String::new(), format!("sutcac-sh: {}", msg));
    }

    if builtin::is_builtin(name) {
        return execute_builtin_with_redirects(name, args, &cmd.redirects, state);
    }

    execute_external(name, args, &cmd.redirects, state)
}

fn execute_function(body: &Command, args: &[String], state: &mut ShellState) -> CommandOutput {
    let saved = std::mem::take(&mut state.args);
    state.args = args.to_vec();
    let output = execute_command(body, state);
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
    for cmd in cmds {
        match cmd {
            Command::Simple(c) => {
                let ctx = state.expand_context();
                let mut argv = Vec::new();
                for word in &c.words {
                    match expand_word(&word.value, &ctx) {
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
    let mut subshell_state = ShellState {
        vars: state.vars.clone(),
        exported: state.exported.clone(),
        args: state.args.clone(),
        funcs: state.funcs.clone(),
        last_status: state.last_status,
        pid: state.pid,
        last_bg_pid: state.last_bg_pid,
        cwd: state.cwd.clone(),
        permissions: state.permissions.clone(),
        audit_logger: state.audit_logger.clone(),
    };
    execute_list(body, &mut subshell_state)
}

fn execute_if(
    cond: &Command,
    then_part: &[Command],
    elifs: &[(Command, Vec<Command>)],
    else_part: &[Command],
    state: &mut ShellState,
) -> CommandOutput {
    if execute_command(cond, state).status == 0 {
        return execute_list(then_part, state);
    }
    for (elif_cond, elif_body) in elifs {
        if execute_command(elif_cond, state).status == 0 {
            return execute_list(elif_body, state);
        }
    }
    execute_list(else_part, state)
}

fn execute_while(cond: &Command, body: &[Command], state: &mut ShellState) -> CommandOutput {
    let mut output = CommandOutput::new(0);
    loop {
        if execute_command(cond, state).status != 0 {
            break;
        }
        output = execute_list(body, state);
    }
    output
}

fn execute_for(
    var: &str,
    words: &[Word],
    body: &[Command],
    state: &mut ShellState,
) -> CommandOutput {
    let ctx = state.expand_context();
    let items: Vec<String> = if words.is_empty() {
        // Default to "$@".
        state.args.clone()
    } else {
        let mut items = Vec::new();
        for word in words {
            match expand_word(&word.value, &ctx) {
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
        output = execute_list(body, state);
    }
    output
}

fn execute_case(word: &Word, arms: &[CaseArm], state: &mut ShellState) -> CommandOutput {
    let ctx = state.expand_context();
    let value = match expand_single(&word.value, &ctx) {
        Ok(v) => v,
        Err(e) => {
            return CommandOutput::with_output(1, String::new(), format!("sutcac-sh: {}", e));
        }
    };
    for arm in arms {
        for pat_word in &arm.patterns {
            let pat = match expand_single(&pat_word.value, &ctx) {
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
                return execute_list(&arm.body, state);
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
    let ctx = state.expand_context();
    let target = expand_single(&redir.target.value, &ctx)?;
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
    fn audit_logs_start_and_end() {
        let (logger, buf) = capture_logger(crate::audit::AuditFormat::Text);
        let mut state = ShellState::with_policy_and_logger(PermissionPolicy::allow_all(), logger);
        let cmd = Command::Simple(SimpleCommand {
            words: vec![crate::ast::Word::new("true")],
            ..Default::default()
        });
        assert_eq!(execute_command(&cmd, &mut state).status, 0);
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("START cmd=\"true\""));
        assert!(text.contains("END cmd=\"true\" status=0"));
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
}
