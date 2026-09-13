//! RAG tools: `rag_search` (hybrid retrieval) and `rag_index` (index
//! building / refresh). Both follow the `McpServerTool` pattern: the tools
//! hold `Arc<RagManager>` and handle heavy blocking work through
//! `tokio::task::spawn_blocking` so the runtime's workers stay free.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use super::{Tool, ToolCall, ToolContext, ToolDefinition, ToolResult};
use crate::rag::RagManager;

/// `rag_search` — hybrid dense+BM25 retrieval over the workspace index.
pub struct RagSearchTool {
    manager: Arc<RagManager>,
}

impl RagSearchTool {
    pub fn new(manager: Arc<RagManager>) -> Self {
        Self { manager }
    }
}

const RAG_SEARCH_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "rag_search",
    "description": "Search the workspace's local hybrid index (semantic + keyword) for code or documentation chunks relevant to a query. The index must exist (call rag_index first if empty).",
    "parameters": {
      "type": "object",
      "properties": {
        "query": {
          "type": "string",
          "description": "Natural-language question or an identifier/code snippet to find."
        },
        "k": {
          "type": "integer",
          "description": "Optional max number of results (default 5)."
        },
        "path_prefix": {
          "type": "string",
          "description": "Optional path prefix filter, e.g. 'src/mcp/'."
        }
      },
      "required": ["query"]
    }
  }
}"#;

/// `rag_index` — (re)build / update the workspace index.
pub struct RagIndexTool {
    manager: Arc<RagManager>,
}

impl RagIndexTool {
    pub fn new(manager: Arc<RagManager>) -> Self {
        Self { manager }
    }
}

const RAG_INDEX_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "rag_index",
    "description": "Add files or directories to the workspace search index. Only whitelisted text files (code and Markdown) are indexed. Without arguments indexes the whole current workspace.",
    "parameters": {
      "type": "object",
      "properties": {
        "paths": {
          "type": "array",
          "items": { "type": "string" },
          "description": "Optional list of files or directories to index; relative paths resolve against the workspace."
        }
      }
    }
  }
}"#;

/// Format hits into a compact LLM-readable listing.
fn format_hits(hits: &[catus_rag::Hit]) -> String {
    if hits.is_empty() {
        return "(no matches)".to_string();
    }
    let mut out = String::new();
    for hit in hits {
        let snip = hit.text.lines().collect::<Vec<_>>().join("\n");
        out.push_str(&format!(
            "## {}:{} (score {:.3})\n{}\n---\n",
            hit.path,
            if hit.symbol.is_empty() {
                "?"
            } else {
                hit.symbol.as_str()
            },
            hit.score,
            snip
        ));
    }
    out
}

/// Minimize a `paths: [...]` argument; errors on the wrong shape.
fn parse_paths(args: &Value) -> Result<Vec<PathBuf>, String> {
    match args.get("paths") {
        None => Ok(vec![PathBuf::from(".")]),
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err("paths items must be strings".to_string());
                };
                out.push(PathBuf::from(s));
            }
            Ok(out)
        }
        Some(_) => Err("paths must be an array of strings".to_string()),
    }
}

fn malformed(call: &ToolCall, detail: &str) -> ToolResult {
    ToolResult {
        call: call.clone(),
        status: 2,
        stdout: String::new(),
        stderr: format!("catus: tool call format error: {}", detail),
        interaction: None,
    }
}

impl Tool for RagSearchTool {
    fn name(&self) -> &str {
        "rag_search"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(RAG_SEARCH_SCHEMA).expect("rag_search schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .and_then(|v| v.get("query").and_then(|q| q.as_str()).map(str::to_string))
            .unwrap_or_else(|| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Ok(args) = serde_json::from_str::<Value>(&call.arguments) else {
                return malformed(
                    call,
                    "arguments must be a JSON object with a string \"query\" field",
                );
            };
            let Some(query) = args
                .get("query")
                .and_then(|q| q.as_str())
                .map(str::to_string)
            else {
                return malformed(call, "missing string field \"query\"");
            };
            let k = args
                .get("k")
                .and_then(|k| k.as_u64())
                .map(|k| k.min(50) as usize)
                .unwrap_or(5);
            let path_prefix = args
                .get("path_prefix")
                .and_then(|p| p.as_str())
                .map(str::to_string);
            let manager = self.manager.clone();
            tracing::info!(target: "rag", "rag_search: query='{}' k={}", query, k);
            let search_query = query.clone();
            let result =
                tokio::task::spawn_blocking(move || manager.search(&search_query, k, path_prefix))
                    .await;
            match result {
                Ok(Ok(hits)) => ToolResult {
                    call: call.clone(),
                    status: 0,
                    stdout: format_hits(&hits),
                    stderr: String::new(),
                    interaction: None,
                },
                Ok(Err(e)) => ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("rag_search failed: {}", e),
                    interaction: None,
                },
                Err(e) => ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("rag_search task failed: {}", e),
                    interaction: None,
                },
            }
        })
    }
}

impl Tool for RagIndexTool {
    fn name(&self) -> &str {
        "rag_index"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(RAG_INDEX_SCHEMA).expect("rag_index schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .and_then(|v| v.get("paths").cloned())
            .map(|p| p.to_string())
            .unwrap_or_else(|| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _ctx: &'a mut ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Ok(args) = serde_json::from_str::<Value>(&call.arguments) else {
                return malformed(
                    call,
                    "arguments must be a JSON object (optionally with \"paths\": [..])",
                );
            };
            let paths = match parse_paths(&args) {
                Ok(paths) => paths,
                Err(e) => return malformed(call, &e),
            };
            let manager = self.manager.clone();
            let result = tokio::task::spawn_blocking(move || manager.index_paths(&paths)).await;
            match result {
                Ok(Ok(summary)) => ToolResult {
                    call: call.clone(),
                    status: 0,
                    stdout: summary,
                    stderr: String::new(),
                    interaction: None,
                },
                Ok(Err(e)) => ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("rag_index failed: {}", e),
                    interaction: None,
                },
                Err(e) => ToolResult {
                    call: call.clone(),
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("rag_index task failed: {}", e),
                    interaction: None,
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catus_rag::config::RagConfig;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("catus-tool-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manager(tag: &str) -> Arc<RagManager> {
        let mut cfg = RagConfig::default();
        cfg.enabled = true;
        let dir = temp_dir(tag);
        RagManager::with_embedder_factory(dir, cfg, crate::rag::RagManager::fake_embedder_factory())
    }

    #[test]
    fn tool_names_and_definitions() {
        let search = RagSearchTool::new(manager("names"));
        let index = RagIndexTool::new(manager("names"));
        assert_eq!(search.name(), "rag_search");
        assert_eq!(index.name(), "rag_index");
        assert_eq!(search.definition().function.name, "rag_search");
        assert_eq!(index.definition().function.name, "rag_index");
    }

    #[test]
    fn parse_paths_defaults_to_workspace() {
        assert_eq!(
            parse_paths(&serde_json::json!({})).unwrap(),
            vec![PathBuf::from(".")]
        );
        assert_eq!(
            parse_paths(&serde_json::json!({"paths": ["src", "docs/a.md"]})).unwrap(),
            vec![PathBuf::from("src"), PathBuf::from("docs/a.md")]
        );
        assert_eq!(
            parse_paths(&serde_json::json!({"paths": [1]})).unwrap_err(),
            "paths items must be strings"
        );
    }

    #[test]
    fn format_hits_compact() {
        let hit = catus_rag::Hit {
            doc_id: 1,
            score: 0.03,
            path: "src/lib.rs".into(),
            lang: "rust".into(),
            symbol: "main".into(),
            text: "fn main() {}".into(),
            source: catus_rag::SourceLane::Both,
        };
        let out = format_hits(&[hit]);
        assert!(out.contains("src/lib.rs:main"), "{}", out);
    }
}
