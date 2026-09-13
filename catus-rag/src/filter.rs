//! Indexable-file filter.
//!
//! Whitelist based: an extension is indexable only when it belongs to the
//! built-in code / Markdown whitelists (`FilterConfig::CODE_EXTS` /
//! `DOC_EXTS`) or is added in `extra_exts`. Concretely excluded components
//! (`filter.exclude`) always win.

use std::path::{Path, PathBuf};

use crate::config::FilterConfig;
use crate::error::RagResult;

/// Maximum single-file size fed to the pipeline (larger files are skipped;
/// the embedding encoder reads at most ~2k tokens per chunk but whole-file
/// chunks must still be materialised in memory).
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// Decide whether `path` should be indexed.
pub fn is_indexable(path: &Path, cfg: &FilterConfig) -> bool {
    if is_excluded(path, &cfg.exclude) {
        return false;
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    FilterConfig::CODE_EXTS.contains(&ext.as_str())
        || FilterConfig::DOC_EXTS.contains(&ext.as_str())
        || cfg.extra_exts.iter().any(|e| e.trim_matches('.') == ext)
}

/// True when any path component (file name or ancestor directory) is listed
/// in `exclude`.
fn is_excluded(path: &Path, exclude: &[String]) -> bool {
    path.components().any(|c| match &c {
        std::path::Component::Normal(name) => {
            let name = name.to_string_lossy();
            exclude.iter().any(|ex| *ex == name.as_ref())
        }
        _ => false,
    })
}

/// Expand `input` (a file or directory) into the indexable file list.
///
/// Directories are walked (BFS, `walkdir`); symlinks are not followed, and
/// oversized or non-whitelisted files are skipped. Invalid paths are ignored
/// rather than failing the whole batch.
pub fn collect_indexable(paths: &[PathBuf], cfg: &FilterConfig) -> RagResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    for root in paths {
        if root.is_file() {
            if is_indexable(root, cfg) {
                out.push(root.to_path_buf());
            }
            continue;
        }
        let walker = walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                e.path()
                    .file_name()
                    .map(|n| !cfg.exclude.contains(&n.to_string_lossy().into_owned()))
                    .unwrap_or(true)
            });
        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if is_indexable(path, cfg) {
                if let Ok(meta) = entry.metadata() {
                    if meta.len() <= MAX_FILE_BYTES {
                        out.push(path.to_path_buf());
                    }
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FilterConfig {
        FilterConfig::default()
    }

    #[test]
    fn whitelisted_exts_pass() {
        assert!(is_indexable(Path::new("src/lib.rs"), &cfg()));
        assert!(is_indexable(Path::new("docs/README.md"), &cfg()));
        assert!(is_indexable(Path::new("a/b/main.tsx"), &cfg()));
        assert!(!is_indexable(Path::new("Cargo.lock"), &cfg()));
        assert!(!is_indexable(Path::new("pic.png"), &cfg()));
    }

    #[test]
    fn excluded_wins() {
        assert!(!is_indexable(Path::new("/w/target/debug/lib.rs"), &cfg()));
        assert!(!is_indexable(Path::new(".git/config.sh"), &cfg()));
        assert!(!is_indexable(
            Path::new("x/node_modules/pkg/index.js"),
            &cfg()
        ));
    }

    #[test]
    fn extra_exts_accepted() {
        let mut c = cfg();
        c.extra_exts.push("txt".to_string());
        assert!(is_indexable(Path::new("notes.txt"), &c));
        assert!(!is_indexable(Path::new("notes.txt"), &cfg()));
    }
}
