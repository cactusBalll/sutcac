//! ONNX Runtime shared-library provisioning for the RAG embedding lane.
//!
//! `fastembed` is compiled with `ort-load-dynamic`, so the embedding model
//! only runs when `ORT_DYLIB_PATH` points at a shared `libonnxruntime`
//! library. This module makes that automatic:
//!
//! 1. An existing `ORT_DYLIB_PATH` that resolves to a file is respected.
//! 2. Otherwise a previously downloaded copy under the XDG cache dir
//!    (`<xdg>/catus/ort/`) is reused.
//! 3. Otherwise the release archive is downloaded from the official GitHub
//!    releases page and unpacked, then `ORT_DYLIB_PATH` is injected into the
//!    process environment.
//!
//! [`ensure_dylib_env`] is awaited from `bootstrap_runtime` whenever
//! `[rag].enabled` is set (before the embedding model loads lazily), and from
//! the `catus --setup-ort` CLI command.

use std::path::{Path, PathBuf};

/// ONNX Runtime release used when nothing else is installed. Matches the
/// release tag format `v<version>` on the microsoft/onnxruntime releases
/// page (e.g. `v1.30.0` → `onnxruntime-linux-x64-1.30.0.tgz`).
pub const ORT_VERSION: &str = "1.28.0";

/// Base XDG cache directory for the downloaded runtime
/// (`<xdg>/catus/ort/`).
pub fn cache_dir() -> PathBuf {
    crate::config::xdg_catus_dir().join("ort")
}

/// Platform tag used in the release archive name, e.g. `linux-x64` for
/// `onnxruntime-linux-x64-1.30.0.tgz`.
fn platform_tag() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("macos", "aarch64") => Ok("osx-arm64"),
        ("macos", "x86_64") => Ok("osx-x64"),
        ("windows", "x86_64") => Ok("win-x64"),
        (os, arch) => Err(format!(
            "no onnxruntime release mapping for {os}/{arch}; set ORT_DYLIB_PATH manually"
        )),
    }
}

/// Shared-library file names to look for after unpacking, best match first.
/// The archives ship a versioned real file plus `.so` symlinks; pointing
/// `ORT_DYLIB_PATH` at the real file avoids depending on symlink support.
fn dylib_candidates(os: &str) -> &'static [&'static str] {
    match os {
        "windows" => &["onnxruntime.dll"],
        "macos" => &["libonnxruntime.dylib"],
        _ => &["libonnxruntime.so"],
    }
}

/// GitHub release download URL for this platform's `.tgz`/`.zip` archive.
fn archive_url(version: &str) -> Result<String, String> {
    let tag = platform_tag()?;
    let ext = if std::env::consts::OS == "windows" {
        "zip"
    } else {
        "tgz"
    };
    Ok(format!(
        "https://github.com/microsoft/onnxruntime/releases/download/v{version}/onnxruntime-{tag}-{version}.{ext}"
    ))
}

/// Return `ORT_DYLIB_PATH` when it already points at an existing file.
fn existing_env_dylib() -> Option<PathBuf> {
    let value = std::env::var("ORT_DYLIB_PATH").ok()?;
    let path = PathBuf::from(&value);
    path.is_file().then_some(path)
}

/// True when `path` looks like the shared library this platform needs.
fn is_dylib(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    dylib_candidates(std::env::consts::OS)
        .iter()
        .any(|base| lower == *base || lower.starts_with(&format!("{base}.")))
}

/// Find a previously downloaded library under `root`, preferring real files
/// over symlinks so the deepest versioned file wins.
fn find_cached_dylib(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(bool, usize, PathBuf)> = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if is_dylib(&path) {
                let real = !meta.file_type().is_symlink();
                let score = path.to_string_lossy().len();
                let better = match &best {
                    None => true,
                    Some((b_real, b_score, _)) => (*b_real, *b_score) < (real, score),
                };
                if better {
                    best = Some((real, score, path));
                }
            }
        }
    }
    best.map(|(_, _, path)| path)
}

/// Install the ONNX Runtime shared library (download from GitHub releases
/// when not cached) and inject `ORT_DYLIB_PATH`. Idempotent: an existing
/// valid `ORT_DYLIB_PATH` or cached download short-circuits. Blocking.
pub fn setup_dylib_blocking(version: &str) -> Result<PathBuf, String> {
    if let Some(path) = existing_env_dylib() {
        return Ok(path);
    }
    let root = cache_dir();
    if let Some(path) = find_cached_dylib(&root) {
        set_env(&path);
        return Ok(path);
    }
    let url = archive_url(version)?;
    download_and_extract(&url, &root)?;
    let Some(path) = find_cached_dylib(&root) else {
        return Err(format!(
            "archive unpacked but no shared library found under {}",
            root.display()
        ));
    };
    set_env(&path);
    Ok(path)
}

/// Async wrapper around [`setup_dylib_blocking`] (the download blocks the
/// calling thread; bootstrap awaits this on the runtime worker).
pub async fn ensure_dylib_env(version: &str) -> Result<PathBuf, String> {
    let version = version.to_string();
    tokio::task::spawn_blocking(move || setup_dylib_blocking(&version))
        .await
        .map_err(|e| format!("onnxruntime setup task failed: {e}"))?
}

/// Download the release archive and unpack it into `dest_dir`. The archive's
/// own top-level directory (`onnxruntime-linux-x64-<version>/…`) is preserved,
/// so each version lands in its own subdirectory. Blocking; `Archive::unpack`
/// refuses path-traversal entries.
fn download_and_extract(url: &str, dest_dir: &Path) -> Result<(), String> {
    tracing::info!(target: "rag", "downloading onnxruntime from {url}");
    let bytes = download(url)?;
    if url.ends_with(".zip") {
        return Err(
            "windows .zip archives are not supported; set ORT_DYLIB_PATH manually".to_string(),
        );
    }
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("cannot create {}: {e}", dest_dir.display()))?;
    unpack_tgz(&bytes, dest_dir)?;
    tracing::info!(target: "rag", "onnxruntime unpacked into {}", dest_dir.display());
    Ok(())
}

/// Fetch `url` into memory with a fresh reqwest client.
fn download(url: &str) -> Result<Vec<u8>, String> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
        "onnxruntime download requires an async runtime; use ensure_dylib_env".to_string()
    })?;
    runtime
        .block_on(async {
            let response = reqwest::Client::new()
                .get(url)
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("download failed: HTTP {status} for {url}"));
            }
            response
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| format!("download body failed: {e}"))
        })
        .map_err(|e: String| format!("onnxruntime download failed ({url}): {e}"))
}

/// Unpack the gzip-compressed tarball into `dest_dir`, keeping the archive's
/// leading versioned directory.
fn unpack_tgz(bytes: &[u8], dest_dir: &Path) -> Result<(), String> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    tar::Archive::new(decoder)
        .unpack(dest_dir)
        .map_err(|e| format!("unpacking failed: {e}"))
}

/// Publish the library path into this process's environment.
fn set_env(path: &Path) {
    // SAFETY: single-threaded bootstrap / CLI paths only; mirrors the
    // `builtin::export` shell builtin, which also mutates the process env.
    unsafe { std::env::set_var("ORT_DYLIB_PATH", path) };
    tracing::info!(target: "rag", "ORT_DYLIB_PATH={}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_tag_matches_current_triple() {
        let tag = platform_tag();
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => assert_eq!(tag.unwrap(), "linux-x64"),
            ("linux", "aarch64") => assert_eq!(tag.unwrap(), "linux-arm64"),
            ("macos", "aarch64") => assert_eq!(tag.unwrap(), "osx-arm64"),
            ("macos", "x86_64") => assert_eq!(tag.unwrap(), "osx-x64"),
            ("windows", "x86_64") => assert_eq!(tag.unwrap(), "win-x64"),
            _ => assert!(tag.is_err()),
        }
    }

    #[test]
    fn archive_url_uses_release_layout() {
        let url = archive_url("1.30.0").unwrap();
        assert!(url.starts_with(
            "https://github.com/microsoft/onnxruntime/releases/download/v1.30.0/onnxruntime-"
        ));
        assert!(url.ends_with("-1.30.0.tgz"));
    }

    #[test]
    fn find_cached_dylib_prefers_real_versioned_file() {
        let root = std::env::temp_dir().join(format!("catus-ort-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let lib = root.join("onnxruntime-linux-x64-1.30.0/lib");
        std::fs::create_dir_all(&lib).unwrap();
        let real = lib.join("libonnxruntime.so.1.30.0");
        std::fs::write(&real, b"fake").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, lib.join("libonnxruntime.so")).unwrap();
        let found = find_cached_dylib(&root).unwrap();
        assert_eq!(found, real);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_cached_dylib_rejects_unrelated_files() {
        let root = std::env::temp_dir().join(format!("catus-ort-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("libonnxruntime_providers_shared.so"), b"x").unwrap();
        assert!(find_cached_dylib(&root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
