//! The built-in `edit` tool.
//!
//! [`EditTool`] performs a precise file edit by exact text replacement. The
//! edit itself runs through the embedded `sutcac-sh` `edit` builtin (via
//! [`execute_shell_command`]), so it goes through the shell's permission
//! checks and audit logging like any other command. After a successful edit
//! the tool renders a unified diff of the change with `diff-match-patch-rs`
//! and includes it in the result shown to the model and the user.

use std::future::Future;
use std::pin::Pin;

use diff_match_patch_rs::{DiffMatchPatch, Efficient, Ops};
use serde::Deserialize;

use super::shell::execute_shell_command;
use super::{Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};

/// The built-in `edit` tool.
pub struct EditTool;

/// JSON Schema for the edit tool.
const EDIT_TOOL_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "edit",
    "description": "Perform an exact text replacement in a file. old_string must appear exactly once in the file; it is replaced by new_string. Prefer this over sed/perl for precise edits.",
    "parameters": {
      "type": "object",
      "properties": {
        "file_path": {
          "type": "string",
          "description": "Path of the file to edit."
        },
        "old_string": {
          "type": "string",
          "description": "Exact text to replace. Must match exactly one location in the file; include surrounding context to make it unique."
        },
        "new_string": {
          "type": "string",
          "description": "Replacement text."
        }
      },
      "required": ["file_path", "old_string", "new_string"]
    }
  }
}"#;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(EDIT_TOOL_SCHEMA).expect("edit tool schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        parse_arguments(&call.arguments)
            .map(|args| format!("edit {}", args.file_path))
            .unwrap_or_else(|| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(args) = parse_arguments(&call.arguments) else {
                tracing::warn!("malformed tool call: {}", call.arguments);
                return ToolResult {
                    call: call.clone(),
                    status: 2,
                    stdout: String::new(),
                    stderr: format!(
                        "catus: tool call format error: arguments must be a single JSON object {{\"file_path\": \"...\", \"old_string\": \"...\", \"new_string\": \"...\"}}; got: {}",
                        call.arguments
                    ),
                    interaction: None,
                };
            };

            tracing::info!(
                "edit tool: {} ({} -> {} bytes)",
                args.file_path,
                args.old_string.len(),
                args.new_string.len()
            );

            // Capture the pre-edit content for the diff. Failure to read here
            // only means no diff is shown; the edit itself reports errors.
            let old_content = std::fs::read_to_string(&args.file_path).ok();

            let command = format!(
                "edit {} {} {}",
                shell_quote(&args.file_path),
                shell_quote(&args.old_string),
                shell_quote(&args.new_string)
            );
            let mut output = execute_shell_command(&command, ctx.shell_state);

            if output.status == 0 {
                if let (Some(old), Ok(new)) =
                    (&old_content, std::fs::read_to_string(&args.file_path))
                {
                    let diff = unified_diff(old, &new, &args.file_path);
                    if !diff.is_empty() {
                        output.stdout.push_str(&diff);
                    }
                }
            }

            tracing::info!(
                "edit tool finished: status={} stdout_len={} stderr_len={}",
                output.status,
                output.stdout.len(),
                output.stderr.len()
            );
            ToolResult {
                call: call.clone(),
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
                interaction: None,
            }
        })
    }
}

/// Arguments of an `edit` tool call.
#[derive(Debug, Deserialize)]
struct EditArguments {
    file_path: String,
    old_string: String,
    new_string: String,
}

/// Extract the edit arguments from a tool call's `arguments` string.
///
/// Only the canonical form is accepted: an object with exactly the three
/// string fields. Anything else yields `None` so the caller can report a
/// malformed tool call back to the model.
fn parse_arguments(arguments: &str) -> Option<EditArguments> {
    serde_json::from_str::<EditArguments>(arguments).ok()
}

/// Quote a string as a single shell word so it survives lexing unchanged.
///
/// Everything is wrapped in double quotes; `"`, `\` and `$` are backslash
/// escaped (the only characters the expander treats specially inside double
/// quotes). Newlines and single quotes pass through untouched.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// One row of a rendered unified diff.
struct Row {
    /// `None` for added lines (absent from the old file).
    old_no: Option<usize>,
    /// `None` for removed lines (absent from the new file).
    new_no: Option<usize>,
    /// Prefix character: `' '` context, `'-'` removed, `'+'` added.
    prefix: char,
    text: String,
}

/// Render a unified diff of `old` → `new` for display after an edit.
///
/// The diff is computed with `diff-match-patch-rs` (semantic cleanup applied)
/// and then lifted to whole lines: a line is classified as removed/added when
/// any deleted/inserted span covers it. Returns an empty string when the
/// texts are identical.
fn unified_diff(old: &str, new: &str, path: &str) -> String {
    let dmp = DiffMatchPatch::new();
    let Ok(mut diffs) = dmp.diff_main::<Efficient>(old, new) else {
        return String::new();
    };
    DiffMatchPatch::diff_cleanup_semantic(&mut diffs);

    let rows = diff_rows(&diffs, old, new);
    if !rows.iter().any(|r| r.prefix != ' ') {
        return String::new();
    }

    let mut out = String::new();
    out.push_str(&format!("--- a/{}\n+++ b/{}\n", path, path));
    let context = 3;
    let changes: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.prefix != ' ')
        .map(|(i, _)| i)
        .collect();

    let mut hunk_start = 0usize;
    while hunk_start < changes.len() {
        let first = changes[hunk_start];
        let mut last = first;
        let mut next = hunk_start + 1;
        while next < changes.len() && changes[next] <= last + 2 * context + 1 {
            last = changes[next];
            next += 1;
        }
        let start = first.saturating_sub(context);
        let end = (last + context + 1).min(rows.len());
        write_hunk(&mut out, &rows[start..end]);
        hunk_start = next;
    }
    out
}

/// Lift the byte-level diffs to whole-line rows.
fn diff_rows(diffs: &[diff_match_patch_rs::dmp::Diff<u8>], old: &str, new: &str) -> Vec<Row> {
    let old_line_of = line_indexer(old);
    let new_line_of = line_indexer(new);

    let mut old_deleted = vec![false; old.lines().count().max(1)];
    let mut new_added = vec![false; new.lines().count().max(1)];

    let mut old_pos = 0usize;
    let mut new_pos = 0usize;
    for d in diffs {
        let len = d.data().len();
        match d.op() {
            Ops::Equal => {
                old_pos += len;
                new_pos += len;
            }
            Ops::Delete => {
                let start = old_line_of(old_pos);
                let end = old_line_of(old_pos + len - 1) + 1;
                for line in &mut old_deleted[start..end] {
                    *line = true;
                }
                old_pos += len;
            }
            Ops::Insert => {
                let start = new_line_of(new_pos);
                let end = new_line_of(new_pos + len - 1) + 1;
                for line in &mut new_added[start..end] {
                    *line = true;
                }
                new_pos += len;
            }
        }
    }

    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // Merge the two classified sequences. Runs of removed old lines and added
    // new lines are emitted side by side, as unified diff does.
    let mut rows = Vec::new();
    let (mut oi, mut ni) = (0usize, 0usize);
    while oi < old_lines.len() || ni < new_lines.len() {
        if oi < old_lines.len() && ni < new_lines.len() && !old_deleted[oi] && !new_added[ni] {
            rows.push(Row {
                old_no: Some(oi + 1),
                new_no: Some(ni + 1),
                prefix: ' ',
                text: old_lines[oi].to_string(),
            });
            oi += 1;
            ni += 1;
        } else if oi < old_lines.len() && old_deleted[oi] {
            rows.push(Row {
                old_no: Some(oi + 1),
                new_no: None,
                prefix: '-',
                text: old_lines[oi].to_string(),
            });
            oi += 1;
        } else if ni < new_lines.len() && new_added[ni] {
            rows.push(Row {
                old_no: None,
                new_no: Some(ni + 1),
                prefix: '+',
                text: new_lines[ni].to_string(),
            });
            ni += 1;
        } else if oi < old_lines.len() && ni >= new_lines.len() {
            // One side is exhausted while the other still has unflagged
            // lines (e.g. the edit appended text to a file's last line
            // without a trailing newline): the leftover old lines pair with
            // the just-emitted added rows, so render them as removed.
            rows.push(Row {
                old_no: Some(oi + 1),
                new_no: None,
                prefix: '-',
                text: old_lines[oi].to_string(),
            });
            oi += 1;
        } else if ni < new_lines.len() && oi >= old_lines.len() {
            // Mirror case: old side exhausted, leftover new lines render
            // as added instead of producing an out-of-range old number.
            rows.push(Row {
                old_no: None,
                new_no: Some(ni + 1),
                prefix: '+',
                text: new_lines[ni].to_string(),
            });
            ni += 1;
        } else {
            // Classifications disagree (a partial-line change made one side
            // look deleted while the other looks untouched): fall back to
            // pairing the remaining lines as context.
            rows.push(Row {
                old_no: Some(oi + 1),
                new_no: Some(ni + 1),
                prefix: ' ',
                text: new_lines[ni].to_string(),
            });
            oi += 1;
            ni += 1;
        }
    }
    rows
}

/// Build a closure mapping a byte offset to its zero-based line index.
fn line_indexer(text: &str) -> impl Fn(usize) -> usize {
    let starts: Vec<usize> = text
        .bytes()
        .enumerate()
        .filter(|(_, b)| *b == b'\n')
        .map(|(i, _)| i + 1)
        .collect();
    move |offset| starts.partition_point(|&s| s <= offset)
}

/// Append one `@@` hunk to the output.
fn write_hunk(out: &mut String, rows: &[Row]) {
    let old_start = rows.iter().find_map(|r| r.old_no).unwrap_or(0);
    let new_start = rows.iter().find_map(|r| r.new_no).unwrap_or(0);
    let old_count = rows.iter().filter(|r| r.old_no.is_some()).count();
    let new_count = rows.iter().filter(|r| r.new_no.is_some()).count();
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        old_start, old_count, new_start, new_count
    ));
    for r in rows {
        out.push(r.prefix);
        out.push_str(&r.text);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRegistry;
    use crate::tool::Toolbox;
    use sutcac_sh::exec::ShellState;

    fn edit_call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_abc".to_string(),
            name: "edit".to_string(),
            arguments: arguments.to_string(),
        }
    }

    fn test_shell_state() -> ShellState {
        let mut state = ShellState::new();
        state.audit_logger = sutcac_sh::audit::AuditLogger::null();
        state
    }

    fn test_context<'a>(
        shell_state: &'a mut ShellState,
        skill_registry: &'a mut SkillRegistry,
        active_skills: &'a mut Vec<String>,
        messages: &'a mut Vec<crate::message::Message>,
    ) -> ToolContext<'a> {
        let toolbox: &'a Toolbox = Box::leak(Box::new(Toolbox::default()));
        ToolContext {
            shell_state,
            skill_registry,
            active_skills,
            todos: Box::leak(Box::new(crate::tool::TodoList::new())),
            messages,
            toolbox,
            agent_registry: None,
            subagents: None,
            current_agent: None,
            is_main_agent: true,
        }
    }

    fn temp_file(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn edit_tool_replaces_and_shows_diff() {
        let dir = tempfile::tempdir().unwrap();
        let file = temp_file(&dir, "a.txt", "line one\nline two\nline three\n");
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = EditTool;
        let args = format!(
            r#"{{"file_path":{:?},"old_string":"line two","new_string":"line 2"}}"#,
            file
        );
        let result = tool.execute(&edit_call(&args), &mut ctx).await;
        assert_eq!(result.status, 0, "stderr: {}", result.stderr);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "line one\nline 2\nline three\n"
        );
        assert!(
            result.stdout.contains("--- a/"),
            "stdout: {}",
            result.stdout
        );
        assert!(
            result.stdout.contains("-line two"),
            "stdout: {}",
            result.stdout
        );
        assert!(
            result.stdout.contains("+line 2"),
            "stdout: {}",
            result.stdout
        );
        assert!(result.stdout.contains("@@"), "stdout: {}", result.stdout);
    }

    #[tokio::test]
    async fn edit_tool_fails_when_old_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let file = temp_file(&dir, "a.txt", "hello world\n");
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = EditTool;
        let args = format!(
            r#"{{"file_path":{:?},"old_string":"missing","new_string":"x"}}"#,
            file
        );
        let result = tool.execute(&edit_call(&args), &mut ctx).await;
        assert_ne!(result.status, 0);
        assert!(result.stderr.contains("old text not found"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world\n");
    }

    #[tokio::test]
    async fn edit_tool_handles_multiline_and_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let file = temp_file(&dir, "a.txt", "fn main() {\n    println!(\"hi\");\n}\n");
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = EditTool;
        let old = "println!(\"hi\");";
        let new = "println!(\"it's ok\");\nprintln!(\"done\");";
        let args = format!(
            "{{\"file_path\":{:?},\"old_string\":{:?},\"new_string\":{:?}}}",
            file, old, new
        );
        let result = tool.execute(&edit_call(&args), &mut ctx).await;
        assert_eq!(result.status, 0, "stderr: {}", result.stderr);
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("it's ok"));
        assert!(result.stdout.contains("+    println!(\"it's ok\");"));
    }

    #[tokio::test]
    async fn edit_tool_rejects_malformed_arguments() {
        let mut ctx_fields = (
            test_shell_state(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = EditTool;
        let result = tool
            .execute(
                &edit_call(r#"{"file_path":"a.txt","old_string":"x"}"#),
                &mut ctx,
            )
            .await;
        assert_eq!(result.status, 2);
        assert!(result.stderr.contains("format error"));
    }

    #[test]
    fn shell_quote_escapes_double_quote_specials() {
        assert_eq!(shell_quote("abc"), "\"abc\"");
        assert_eq!(shell_quote("it's"), "\"it's\"");
        assert_eq!(shell_quote("a\nb"), "\"a\nb\"");
        assert_eq!(shell_quote(""), "\"\"");
        assert_eq!(shell_quote("x \"y\" \\ $z"), "\"x \\\"y\\\" \\\\ \\$z\"");
    }

    #[test]
    fn unified_diff_renders_changed_line() {
        let diff = unified_diff("a\nb\nc\n", "a\nB\nc\n", "f.txt");
        assert!(diff.contains("--- a/f.txt\n+++ b/f.txt\n"), "{}", diff);
        assert!(diff.contains("@@ -1,3 +1,3 @@"), "{}", diff);
        assert!(diff.contains("-b\n"), "{}", diff);
        assert!(diff.contains("+B\n"), "{}", diff);
        assert!(diff.contains(" a\n"), "{}", diff);
    }

    #[test]
    fn unified_diff_is_empty_for_identical_texts() {
        assert_eq!(unified_diff("same\n", "same\n", "f.txt"), "");
    }

    #[test]
    fn unified_diff_handles_append_to_unterminated_last_line() {
        // Regression: the old fallback row indexed new_lines out of bounds
        // (panic "len is 173 but the index is 173") when an edit appended
        // text to the last line of a file without a trailing newline.
        let diff = unified_diff("A\nB", "A\nBB", "f.txt");
        assert!(diff.contains(" A\n"), "{}", diff);
        assert!(diff.contains("+BB\n"), "{}", diff);
        assert!(diff.contains("-B\n"), "{}", diff);
        assert!(diff.contains("@@ -1,2 +1,2 @@"), "{}", diff);
    }

    #[test]
    fn unified_diff_handles_trim_of_unterminated_last_line() {
        // Mirror case: the old fallback produced an out-of-range old line
        // number when text was removed from the end of such a file.
        let diff = unified_diff("A\nBB", "A\nB", "f.txt");
        assert!(diff.contains(" A\n"), "{}", diff);
        assert!(diff.contains("-BB\n"), "{}", diff);
        assert!(diff.contains("+B\n"), "{}", diff);
        assert!(diff.contains("@@ -1,2 +1,2 @@"), "{}", diff);
    }

    #[test]
    fn unified_diff_covers_multiple_hunks() {
        let old: String = (1..=20).map(|i| format!("l{}\n", i)).collect();
        let new: String = (1..=20)
            .map(|i| {
                if i == 2 {
                    "x2\n".to_string()
                } else if i == 18 {
                    "x18\n".to_string()
                } else {
                    format!("l{}\n", i)
                }
            })
            .collect();
        let diff = unified_diff(&old, &new, "f.txt");
        assert_eq!(diff.matches("@@ -").count(), 2, "{}", diff);
        assert!(diff.contains("-l2\n"), "{}", diff);
        assert!(diff.contains("+x18\n"), "{}", diff);
    }
}
