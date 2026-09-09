//! Static asset serving: the embedded Vue UI (release) or a filesystem
//! directory (debug builds read `web/dist` from disk automatically;
//! `--static-dir` overrides both for development).

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// The embedded `web/dist` bundle. Debug builds read files from disk at
/// runtime, so `npm run build --workspace catus-server-web` output is
/// picked up without recompiling; release builds embed the bundle.
#[derive(RustEmbed)]
#[folder = "web/dist"]
struct EmbeddedAssets;

/// Source of the static UI assets.
#[derive(Debug, Clone)]
pub enum StaticAssets {
    /// The `rust-embed` bundle (`web/dist`).
    Embedded,
    /// A filesystem directory overriding the embedded bundle.
    Directory(PathBuf),
}

impl StaticAssets {
    pub fn new(dir: Option<PathBuf>) -> Self {
        match dir {
            Some(path) => StaticAssets::Directory(path),
            None => StaticAssets::Embedded,
        }
    }
}

/// Serve one asset; unknown extension-less paths fall back to `index.html`
/// (SPA routing). Returns 503 while `web/dist` has not been built yet.
pub async fn serve(assets: &StaticAssets, uri: &Uri) -> Response {
    let path = sanitize_path(uri.path());
    match lookup(assets, &path).await {
        Some((mime, bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| internal_error()),
        None => not_found(),
    }
}

/// Normalize the request path: strip the leading `/`, map `/` to
/// `index.html`, and resolve extension-less paths to `index.html` (SPA).
/// Rejects traversal attempts.
fn sanitize_path(raw: &str) -> String {
    let trimmed = raw.trim_start_matches('/');
    let decoded = urldecode(trimmed);
    if decoded.split('/').any(|seg| seg == "..") {
        return String::new();
    }
    if decoded.is_empty() {
        return "index.html".to_string();
    }
    // Extension-less deep links fall back to the SPA entry point.
    if decoded
        .rsplit('/')
        .next()
        .is_some_and(|last| !last.contains('.') && !decoded.starts_with("assets/"))
    {
        return "index.html".to_string();
    }
    decoded
}

fn urldecode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte as char);
                    i += 3;
                } else {
                    out.push('%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

/// Look up one asset; `None` when missing or unreadable.
async fn lookup(assets: &StaticAssets, path: &str) -> Option<(String, Vec<u8>)> {
    if path.is_empty() {
        return None;
    }
    match assets {
        StaticAssets::Embedded => {
            let file = EmbeddedAssets::get(path)?;
            Some((mime_of(path), file.data.to_vec()))
        }
        StaticAssets::Directory(root) => {
            let full = root.join(path);
            let data = tokio::fs::read(&full).await.ok()?;
            Some((mime_of(path), data))
        }
    }
}

fn mime_of(path: &str) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        "static UI not built; run `npm install && npm run build --workspace catus-server-web`",
    )
        .into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "asset error").into_response()
}

#[cfg(test)]
mod tests {
    use super::{sanitize_path, urldecode};

    #[test]
    fn maps_root_to_index() {
        assert_eq!(sanitize_path("/"), "index.html");
        assert_eq!(sanitize_path(""), "index.html");
    }

    #[test]
    fn keeps_asset_paths() {
        assert_eq!(sanitize_path("/assets/app.js"), "assets/app.js");
        assert_eq!(sanitize_path("/favicon.ico"), "favicon.ico");
    }

    #[test]
    fn spa_fallback_for_extension_less_paths() {
        assert_eq!(sanitize_path("/chat"), "index.html");
        assert_eq!(sanitize_path("/foo/bar"), "index.html");
    }

    #[test]
    fn rejects_traversal() {
        assert_eq!(sanitize_path("/../secret"), "");
        assert_eq!(sanitize_path("/assets/%2e%2e/secret"), "");
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(urldecode("a%20b+c"), "a b c");
    }
}
