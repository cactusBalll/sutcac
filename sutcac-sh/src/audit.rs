//! Audit logging for command execution.
//!
//! Records a trace of command starts, ends, permission denials, and notable
//! side effects. Events can be written as plain text or as JSON lines.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::permissions::PermissionSet;

/// Output format for audit log entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditFormat {
    /// Human-readable single-line text.
    Text,
    /// Machine-readable JSON line.
    Json,
}

impl AuditFormat {
    /// Parse a format string: `text` or `json` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" => Some(AuditFormat::Text),
            "json" => Some(AuditFormat::Json),
            _ => None,
        }
    }
}

/// An auditable event.
#[derive(Debug, Clone)]
pub enum AuditEvent {
    /// A command is about to execute.
    CommandStart {
        cmd: String,
        required: PermissionSet,
    },
    /// A command finished with a status and captured output.
    CommandEnd {
        cmd: String,
        status: i32,
        stdout: String,
        stderr: String,
    },
    /// A command was denied by the permission policy.
    PermissionDenied {
        cmd: String,
        required: PermissionSet,
        reason: String,
    },
    /// A side effect produced by a command (e.g. cd, export, exit).
    SideEffect { cmd: String, description: String },
}

impl AuditEvent {
    /// Event type name used in text and JSON output.
    fn event_name(&self) -> &'static str {
        match self {
            AuditEvent::CommandStart { .. } => "start",
            AuditEvent::CommandEnd { .. } => "end",
            AuditEvent::PermissionDenied { .. } => "denied",
            AuditEvent::SideEffect { .. } => "side_effect",
        }
    }

    /// Render the event as a plain-text line, appending optional metadata.
    pub fn to_text(&self, timestamp: &str, meta: &HashMap<String, String>) -> String {
        let base = match self {
            AuditEvent::CommandStart { cmd, required } => {
                format!("[{}] START cmd={:?} required={}", timestamp, cmd, required)
            }
            AuditEvent::CommandEnd {
                cmd,
                status,
                stdout,
                stderr,
            } => {
                format!(
                    "[{}] END cmd={:?} status={} stdout={:?} stderr={:?}",
                    timestamp, cmd, status, stdout, stderr
                )
            }
            AuditEvent::PermissionDenied {
                cmd,
                required,
                reason,
            } => {
                format!(
                    "[{}] DENIED cmd={:?} required={} reason={:?}",
                    timestamp, cmd, required, reason
                )
            }
            AuditEvent::SideEffect { cmd, description } => {
                format!("[{}] SIDE_EFFECT cmd={:?} {}", timestamp, cmd, description)
            }
        };

        if meta.is_empty() {
            base
        } else {
            let pairs: Vec<String> = meta.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
            format!("{} meta=[{}]", base, pairs.join(","))
        }
    }

    /// Render the event as a single JSON object line, merging optional metadata.
    pub fn to_json(&self, timestamp: &str, meta: &HashMap<String, String>) -> String {
        let mut parts: Vec<String> = vec![
            format!("\"timestamp\":{}", json_string(timestamp)),
            format!("\"event\":\"{}\"", self.event_name()),
        ];

        match self {
            AuditEvent::CommandStart { cmd, required } => {
                parts.push(format!("\"cmd\":{}", json_string(cmd)));
                parts.push(format!("\"required\":\"{}\"", required));
            }
            AuditEvent::CommandEnd {
                cmd,
                status,
                stdout,
                stderr,
            } => {
                parts.push(format!("\"cmd\":{}", json_string(cmd)));
                parts.push(format!("\"status\":{}", status));
                parts.push(format!("\"stdout\":{}", json_string(stdout)));
                parts.push(format!("\"stderr\":{}", json_string(stderr)));
            }
            AuditEvent::PermissionDenied {
                cmd,
                required,
                reason,
            } => {
                parts.push(format!("\"cmd\":{}", json_string(cmd)));
                parts.push(format!("\"required\":\"{}\"", required));
                parts.push(format!("\"reason\":{}", json_string(reason)));
            }
            AuditEvent::SideEffect { cmd, description } => {
                parts.push(format!("\"cmd\":{}", json_string(cmd)));
                parts.push(format!("\"description\":{}", json_string(description)));
            }
        }

        for (k, v) in meta {
            parts.push(format!("\"{}\":{}", k, json_string(v)));
        }

        format!("{{{}}}", parts.join(","))
    }
}

/// Escape a string for JSON, wrapping it in double quotes.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Simple audit logger that writes timestamped events to a `Write` sink.
///
/// The logger is cloneable so that subshells and other nested execution
/// contexts can share the same audit sink.
#[derive(Clone)]
pub struct AuditLogger {
    writer: Arc<Mutex<dyn Write + Send>>,
    format: AuditFormat,
    meta: HashMap<String, String>,
}

impl AuditLogger {
    /// Create a logger with an arbitrary writer.
    pub fn new(writer: Box<dyn Write + Send>, format: AuditFormat) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
            format,
            meta: HashMap::new(),
        }
    }

    /// Create a logger that discards all events.
    pub fn null() -> Self {
        Self {
            writer: Arc::new(Mutex::new(io::sink())),
            format: AuditFormat::Text,
            meta: HashMap::new(),
        }
    }

    /// Create a logger that writes to stderr.
    pub fn stderr() -> Self {
        Self {
            writer: Arc::new(Mutex::new(io::stderr())),
            format: AuditFormat::Text,
            meta: HashMap::new(),
        }
    }

    /// Create a logger that appends to a file.
    pub fn file(path: &Path, format: AuditFormat) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Arc::new(Mutex::new(file)),
            format,
            meta: HashMap::new(),
        })
    }

    /// Change the output format.
    pub fn with_format(mut self, format: AuditFormat) -> Self {
        self.format = format;
        self
    }

    /// Set structured metadata that is appended to every audit entry.
    pub fn with_meta(mut self, meta: HashMap<String, String>) -> Self {
        self.meta = meta;
        self
    }

    /// Write a single event with an ISO-8601 timestamp.
    pub fn log(&self, event: AuditEvent) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // Format as seconds.milliseconds for brevity.
        let secs = now.as_secs();
        let millis = now.subsec_millis();
        let timestamp = format!("{}.{:03}Z", secs, millis);

        let line = match self.format {
            AuditFormat::Text => event.to_text(&timestamp, &self.meta),
            AuditFormat::Json => event.to_json(&timestamp, &self.meta),
        };

        let _ = writeln!(self.writer.lock().unwrap(), "{}", line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionSet;
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

    fn capture_logger(format: AuditFormat) -> (AuditLogger, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let logger = AuditLogger::new(Box::new(CaptureWriter(buf.clone())), format);
        (logger, buf)
    }

    #[test]
    fn log_start_and_end_text() {
        let (logger, buf) = capture_logger(AuditFormat::Text);
        logger.log(AuditEvent::CommandStart {
            cmd: "echo hi".into(),
            required: PermissionSet::empty(),
        });
        logger.log(AuditEvent::CommandEnd {
            cmd: "echo hi".into(),
            status: 0,
            stdout: "hi\n".into(),
            stderr: "".into(),
        });
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("START cmd=\"echo hi\" required=NONE"));
        assert!(text.contains("END cmd=\"echo hi\" status=0 stdout=\"hi\\n\" stderr=\"\""));
    }

    #[test]
    fn log_end_json() {
        let (logger, buf) = capture_logger(AuditFormat::Json);
        logger.log(AuditEvent::CommandEnd {
            cmd: "echo hi".into(),
            status: 0,
            stdout: "hi\n".into(),
            stderr: "".into(),
        });
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("\"event\":\"end\""));
        assert!(text.contains("\"cmd\":\"echo hi\""));
        assert!(text.contains("\"status\":0"));
        assert!(text.contains("\"stdout\":\"hi\\n\""));
        assert!(text.contains("\"stderr\":\"\""));
    }

    #[test]
    fn log_with_meta_text() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let meta = {
            let mut m = HashMap::new();
            m.insert("session".to_string(), "abc".to_string());
            m
        };
        let logger = AuditLogger::new(Box::new(CaptureWriter(buf.clone())), AuditFormat::Text)
            .with_meta(meta);
        logger.log(AuditEvent::CommandStart {
            cmd: "echo hi".into(),
            required: PermissionSet::empty(),
        });
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("meta=[session=abc]"));
    }

    #[test]
    fn log_with_meta_json() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let meta = {
            let mut m = HashMap::new();
            m.insert("session".to_string(), "abc".to_string());
            m
        };
        let logger = AuditLogger::new(Box::new(CaptureWriter(buf.clone())), AuditFormat::Json)
            .with_meta(meta);
        logger.log(AuditEvent::CommandStart {
            cmd: "echo hi".into(),
            required: PermissionSet::empty(),
        });
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("\"session\":\"abc\""));
    }

    #[test]
    fn json_escapes_special_chars() {
        let json = json_string("line1\nline2\t\"quoted\"");
        assert_eq!(json, "\"line1\\nline2\\t\\\"quoted\\\"\"");
    }
}
