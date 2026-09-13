//! Shared `tracing` initialization for all catus frontends.
//!
//! Every frontend (TUI, HTTP server, Tauri bridge, headless test mode) writes
//! its log to the fixed XDG location (`<xdg>/catus/catus.log`) through a
//! non-blocking file writer. The level comes from `agent.log_level`
//! (`AppConfig::effective_log_level`); `RUST_LOG` overrides it when set so a
//! trace-level run never requires editing the config file.
//!
//! Log records emitted through the legacy `log` facade by third-party crates
//! (reqwest, rmcp, …) are bridged into tracing via `tracing-log`, keeping the
//! previous simplelog behavior of capturing those lines.
//!
//! The returned [`WorkerGuard`] must be held for the lifetime of the process
//! (kept alive in the caller's frame); dropping it flushes the buffered log
//! events, so it must not be discarded or the tail of the log file may be
//! lost at exit.

use std::path::Path;

use tracing::Level;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::ChronoLocal;

/// Initialize the global tracing subscriber writing to the given log file.
///
/// - `RUST_LOG` wins over `level` when it is set and parseable.
/// - The file is opened in append mode (`rolling::never`), mirroring the old
///   simplelog behavior.
/// - Third-party `log` records are routed into the subscriber.
///
/// Returns the non-blocking writer guard (kept alive by the caller), or
/// `None` when the log directory/file could not be created (logging is then
/// disabled silently, as before).
pub fn init_logging(log_path: &Path, level: Level) -> Option<WorkerGuard> {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(env_filter) => env_filter,
        Err(_) => EnvFilter::new(level.to_string()),
    };

    let timer = ChronoLocal::rfc_3339();
    let (writer, guard) = non_blocking_writer(log_path)?;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(timer)
        .with_ansi(false)
        .with_writer(writer)
        .try_init();

    let _ = tracing_log::LogTracer::init();
    Some(guard)
}

/// Open the log file (append, creating parent directories) and wrap it in a
/// non-blocking writer.
fn non_blocking_writer(
    log_path: &Path,
) -> Option<(tracing_appender::non_blocking::NonBlocking, WorkerGuard)> {
    let file_name = log_path.file_name()?;
    let dir = log_path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    let file_appender = rolling::never(dir, file_name);
    Some(tracing_appender::non_blocking(file_appender))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::xdg_log_path;

    #[test]
    fn log_path_yields_a_file_name_and_parent() {
        let path = xdg_log_path();
        assert!(path.file_name().is_some());
        assert!(path.parent().is_some());
    }
}
