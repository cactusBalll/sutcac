//! Integration tests for sutcac-sh.
//!
//! Each test runs a shell script under the standalone `sutcac-sh` binary and
//! compares stdout/stderr against a `.expected` file produced from GNU bash.
//!
//! Tests run in a temporary working directory that contains a permissive
//! `.sutcac/config.toml` so that redirections and external commands used by
//! the scripts are not blocked by the default permission policy.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn sutcac_sh_binary() -> PathBuf {
    project_root()
        .join("..")
        .join("target")
        .join("debug")
        .join("sutcac-sh")
}

/// Create a temporary working directory with a permissive shell config so that
/// integration scripts can use redirections and external commands freely.
fn temp_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create temp directory");
    let config_dir = dir.path().join(".sutcac");
    fs::create_dir_all(&config_dir).expect("failed to create .sutcac config dir");
    fs::write(
        config_dir.join("config.toml"),
        "[shell]\nperm_mode = \"allow_all\"\naudit_log = \"/dev/null\"\n",
    )
    .expect("failed to write permissive test config");
    dir
}

fn run_in_temp<I, S>(args: I) -> std::process::Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let dir = temp_workspace();
    Command::new(sutcac_sh_binary())
        .args(args)
        .current_dir(dir.path())
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {}", sutcac_sh_binary().display(), e))
}

fn run_script(name: &str) -> String {
    let root = project_root();
    let script = root
        .join("tests")
        .join("scripts")
        .join(format!("{}.sh", name));
    let expected = root
        .join("tests")
        .join("scripts")
        .join(format!("{}.expected", name));

    assert!(
        script.exists(),
        "script {} does not exist",
        script.display()
    );
    assert!(
        expected.exists(),
        "expected output {} does not exist",
        expected.display()
    );

    let output = run_in_temp([&script]);
    let mut got = String::new();
    got.push_str(&String::from_utf8_lossy(&output.stdout));
    got.push_str(&String::from_utf8_lossy(&output.stderr));
    got
}

fn expected_output(name: &str) -> String {
    let path = project_root()
        .join("tests")
        .join("scripts")
        .join(format!("{}.expected", name));
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

fn assert_script_matches(name: &str) {
    let got = run_script(name);
    let expected = expected_output(name);
    if got != expected {
        eprintln!("--- expected ---\n{}", expected);
        eprintln!("--- got ---\n{}", got);
        panic!("integration script {} output mismatch", name);
    }
}

#[test]
fn if_arith_conditions() {
    assert_script_matches("test_if_arith");
}

#[test]
fn arithmetic_for_loops() {
    assert_script_matches("test_arith_for");
}

#[test]
fn brace_expansion() {
    assert_script_matches("test_brace_expansion");
}

#[test]
fn nested_control_flow() {
    assert_script_matches("test_nested_control");
}

#[test]
fn variable_assignment_with_and_or() {
    assert_script_matches("test_var_and");
}

#[test]
fn redirections() {
    assert_script_matches("test_redirections");
}

#[test]
fn pipelines() {
    assert_script_matches("test_pipelines");
}

#[test]
fn command_substitution() {
    assert_script_matches("test_command_substitution");
}

#[test]
fn quotes_and_escaping() {
    assert_script_matches("test_quotes");
}

#[test]
fn special_parameters() {
    assert_script_matches("test_special_params");
}

#[test]
fn glob_expansion() {
    assert_script_matches("test_glob");
}

#[test]
fn functions() {
    assert_script_matches("test_functions");
}

#[test]
fn while_loops() {
    assert_script_matches("test_while");
}

#[test]
fn case_statements() {
    assert_script_matches("test_case");
}

#[test]
fn for_word_loops() {
    assert_script_matches("test_for_words");
}

#[test]
fn builtins() {
    assert_script_matches("test_builtins");
}

/// Sanity check that running the shell with `-c` works and the binary exists.
#[test]
fn binary_smoke() {
    let output = run_in_temp(["-c", "echo hello"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
}
