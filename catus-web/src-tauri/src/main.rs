// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

fn main() {
    // The workspace is selected by changing into the directory before the
    // runtime boots, mirroring the `catus` TUI binary.
    if let Some(dir) = workspace_from_args() {
        if let Err(e) = enter_workspace(&dir) {
            eprintln!("catus-web: {}", e);
            std::process::exit(1);
        }
    }
    catus_web_lib::run()
}

/// Extract the last `-w/--workspace <dir>` (or `--workspace=<dir>`) argument.
fn workspace_from_args() -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    let mut workspace = None;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-w" || arg == "--workspace" {
            workspace = args.get(i + 1).map(PathBuf::from);
            i += 2;
        } else if let Some(rest) = arg.strip_prefix("--workspace=") {
            workspace = Some(PathBuf::from(rest));
            i += 1;
        } else {
            i += 1;
        }
    }
    workspace
}

fn enter_workspace(dir: &std::path::Path) -> Result<(), String> {
    let resolved = dir
        .canonicalize()
        .map_err(|e| format!("cannot use workspace '{}': {}", dir.display(), e))?;
    if !resolved.is_dir() {
        return Err(format!(
            "workspace '{}' is not a directory",
            resolved.display()
        ));
    }
    std::env::set_current_dir(&resolved)
        .map_err(|e| format!("cannot enter workspace '{}': {}", resolved.display(), e))
}
