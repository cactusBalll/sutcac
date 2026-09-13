//! Document chunking (design §4.1).
//!
//! Code (`.rs` / `.ts` / `.js`): tree-sitter AST splits at natural function /
//! type boundaries — no chunk may cross a function edge; oversized items get
//! brace-level re-splits. Markdown: chapter-level split at headings while
//! keeping fenced code blocks intact. Other whitelisted files: sliding
//! windows of lines with paragraph preference.

use std::path::Path;

/// Chunk metadata destined for the index (filtering + display).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChunkMeta {
    /// Source path (workspace-relative or absolute as provided).
    pub path: String,
    /// Language tag (`rust`, `ts`, `md`, ...).
    pub lang: String,
    /// Symbol name when the chunk came from an AST node, else "".
    pub symbol: String,
    /// 1-based start line of the chunk in the source file.
    pub start_line: usize,
}

/// One retrieved unit before embedding.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub meta: ChunkMeta,
    /// Text fed to both embedding and BM25 (`{path}\n{signature}\n{body}`).
    pub text: String,
}

/// Estimated char cap per chunk (the encoder's real cap is ~2048 tokens).
const MAX_CHARS: usize = 4_800;
/// Fallback sliding-window size in lines.
const WINDOW_LINES: usize = 120;

/// Split `text` (contents of `path`) into chunks.
pub fn split(path: &Path, lang: &str, text: &str) -> Vec<Chunk> {
    let raw: Vec<(String, String, usize)> = match lang {
        "rust" => tree_split::<false>(text).unwrap_or_else(|| window_split(text)),
        "ts" | "tsx" | "js" | "jsx" => {
            tree_split::<true>(text).unwrap_or_else(|| window_split(text))
        }
        "md" | "markdown" => heading_split(text, "#"),
        _ => window_split(text),
    };
    let path_str = path.display().to_string();
    let mut chunks = Vec::new();
    for (symbol, body, start_line) in raw {
        for part in hard_clamp(body, MAX_CHARS) {
            let signature = part
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            // Assembled format: `{path}\n{signature}\n{body}` — prefixing the
            // path boosts "find implementation by API name" queries.
            let text = format!("{}\n{}\n{}", path_str, signature, part);
            chunks.push(Chunk {
                meta: ChunkMeta {
                    path: path_str.clone(),
                    lang: lang.to_string(),
                    symbol: symbol.clone(),
                    start_line,
                },
                text,
            });
        }
    }
    chunks
}

/// Detect the language tag for a path via its extension; feeds both chunking
/// and the `lang` metadata field.
pub fn lang_of(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust",
        Some("ts") | Some("tsx") => "ts",
        Some("js") | Some("jsx") => "js",
        Some("py") => "py",
        Some("go") => "go",
        Some("c") | Some("h") => "c",
        Some("cpp") | Some("cc") | Some("hpp") => "cpp",
        Some("java") => "java",
        Some("kt") => "kt",
        Some("rb") => "rb",
        Some("php") => "php",
        Some("cs") => "cs",
        Some("sh") => "sh",
        Some("lua") => "lua",
        Some("sql") => "sql",
        Some("toml") => "toml",
        Some("yaml") | Some("yml") => "yaml",
        Some("json") => "json",
        Some("proto") => "proto",
        Some("md") | Some("markdown") => "md",
        _ => "text",
    }
}

// ---------------------------------------------------------------------------
// tree-sitter code splitting
// ---------------------------------------------------------------------------

const RUST: ParseState = ParseState {
    fn_node: "function_item",
    class_node: "struct_item",
    extra: &["enum_item", "trait_item", "mod_item", "type_item"],
};

const TS: ParseState = ParseState {
    fn_node: "function_declaration",
    class_node: "class_declaration",
    extra: &[
        "lexical_declaration",
        "variable_declaration",
        "abstract_class_declaration",
    ],
};

struct ParseState {
    fn_node: &'static str,
    class_node: &'static str,
    extra: &'static [&'static str],
}

/// Parse `text` with rust (TS=true) or typescript (TS=false) and return
/// `(symbol, text, start_line)` triples; `None` on parse/setup failure
/// (falls back to [`window_split`] upstream).
fn tree_split<const TS_GRAM: bool>(text: &str) -> Option<Vec<(String, String, usize)>> {
    let mut parser = tree_sitter::Parser::new();
    let lang: tree_sitter::Language = if TS_GRAM {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    } else {
        tree_sitter_rust::LANGUAGE.into()
    };
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(text, None)?;
    let state = if TS_GRAM { &TS } else { &RUST };
    let mut out = Vec::new();
    let root = tree.root_node();
    walk_top(text, root, state, &mut out);
    Some(out)
}

/// Walk direct children of `node`, splitting on item kinds; impl/class
/// containers are recursed one level so their methods become chunks.
fn walk_top<'a>(
    text: &'a str,
    node: tree_sitter::Node<'a>,
    state: &ParseState,
    out: &mut Vec<(String, String, usize)>,
) {
    for child in node.children(&mut node.walk()) {
        let kind = child.kind();
        let is_item =
            kind == state.fn_node || kind == state.class_node || state.extra.contains(&kind);
        let is_container = kind == "impl_item" || kind == "class_body";
        if is_item {
            if let Some(triple) = item_chunk(text, &child, state) {
                out.push(triple);
            }
        } else if is_container {
            for inner in child.children(&mut child.walk()) {
                let inner_kind = inner.kind();
                if inner_kind == state.fn_node
                    || inner_kind == state.class_node
                    || state.extra.contains(&inner_kind)
                {
                    if let Some(triple) = item_chunk(text, &inner, state) {
                        out.push(triple);
                    }
                }
            }
        }
    }
}

/// Extract one top-level item as a chunk; the symbol comes from the node's
/// `name` child when present.
fn item_chunk(
    text: &str,
    node: &tree_sitter::Node,
    _state: &ParseState,
) -> Option<(String, String, usize)> {
    let mut symbol = String::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.field_name() == Some("name") {
                symbol = cursor
                    .node()
                    .utf8_text(text.as_bytes())
                    .unwrap_or("")
                    .to_string();
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    let body = node.utf8_text(text.as_bytes()).ok()?;
    Some((symbol, body.to_string(), node.start_position().row + 1))
}

// ---------------------------------------------------------------------------
// Markdown / fallback splitting
// ---------------------------------------------------------------------------

/// Split markdown at ATX headings while keeping fenced code blocks intact.
fn heading_split(text: &str, _marker: &str) -> Vec<(String, String, usize)> {
    let mut parts = Vec::new();
    let mut in_fence = false;
    let mut heading: String = String::new();
    let mut buf = String::new();
    let mut buf_start = 1usize;
    let mut line_no = 0usize;
    for line in text.lines() {
        line_no += 1;
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with('#') && !buf.is_empty() {
            parts.push((
                std::mem::take(&mut heading),
                std::mem::take(&mut buf),
                buf_start,
            ));
            buf_start = line_no;
        }
        if !in_fence && line.starts_with('#') {
            heading = std::mem::take(&mut buf);
            buf_start = line_no;
        }
        buf.push_str(line);
        buf.push('\n');
    }
    parts.push((heading, buf, buf_start));
    // Drop pure-whitespace chunks.
    parts.retain(|(_, body, _)| !body.trim().is_empty());
    if parts.is_empty() {
        window_split(text)
    } else {
        parts
    }
}

/// Paragraph / sliding-window split used for non-code files.
fn window_split(text: &str) -> Vec<(String, String, usize)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut buf_lines = 0usize;
    let mut start_line = 0usize;
    for (idx, line) in text.lines().enumerate() {
        if buf_lines == 0 {
            start_line = idx + 1;
        }
        buf.push_str(line);
        buf.push('\n');
        buf_lines += 1;
        if buf_lines >= WINDOW_LINES {
            out.push((String::new(), std::mem::take(&mut buf), start_line));
            buf_lines = 0;
        }
    }
    if !buf.trim().is_empty() || out.is_empty() {
        out.push((String::new(), buf, start_line.max(1)));
    }
    out
}

/// Re-split an oversized body at brace-depth-0 boundaries (design §4.1
/// "二次切分"); last-resort byte clamp for a giant single line.
fn hard_clamp(body: String, max_chars: usize) -> Vec<String> {
    if body.len() <= max_chars {
        return vec![body];
    }
    let mut parts: Vec<String> = Vec::new();
    let (mut current, mut depth) = (String::new(), 0i32);
    for line in body.lines() {
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;
        let overflow = current.len() + line.len() > max_chars;
        if !current.is_empty() && (overflow && depth <= 0 || current.len() >= max_chars) {
            parts.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
        depth += opens - closes;
    }
    if !current.is_empty() {
        parts.push(current);
    }
    if parts.is_empty() {
        vec![body.chars().take(max_chars).collect()]
    } else {
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_split_uses_function_boundaries() {
        let src = r#"
            fn alpha(x: u32) -> u32 {
                x
            }

            fn beta() {
                beta();
            }

            mod inner {
                fn gamma() {
                    gamma();
                }
            }
        "#;
        let chunks = split(Path::new("m.rs"), "rust", src);
        let symbols: Vec<&str> = chunks.iter().map(|c| c.meta.symbol.as_str()).collect();
        // `mod_item` is also a split kind; methods inside mod chunks join in.
        assert!(symbols.contains(&"alpha"), "{:?}", symbols);
        assert!(symbols.contains(&"beta"), "{:?}", symbols);
        let mut lines: Vec<usize> = chunks.iter().map(|c| c.meta.start_line).collect();
        lines.sort();
        assert_eq!(lines[0], 2);
        // Every chunk text carries the assembled path + signature prefix.
        assert!(chunks.iter().all(|c| c.text.starts_with("m.rs\n")));
    }

    #[test]
    fn md_split_keeps_fences_intact() {
        let src = "# Title\n\ntext\n```rs\nfn fence() {}\n```\n# Next\n\nbody\n";
        let chunks = split(Path::new("r.md"), "md", src);
        let bodies: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(chunks.len(), 2, "{:?}", bodies);
        assert!(bodies[0].contains("fence() {}"));
        assert!(bodies[1].contains("body"));
    }

    #[test]
    fn oversized_function_is_resplit() {
        let inner = "    let x = 1;\n    let y = x + 1;\n".repeat(200);
        let src = format!("fn big() {{\n{}}}\n", inner);
        let chunks = split(Path::new("b.rs"), "rust", &src);
        assert!(
            chunks.len() >= 2,
            "expected a re-split, got {} chunk(s)",
            chunks.len()
        );
        assert!(
            chunks.iter().all(|c| c.text.len() <= MAX_CHARS + 200),
            "sizes {:?}",
            chunks.iter().map(|c| c.text.len()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ts_split_works() {
        let src = r#"
            function alpha() {
                a();
            }
            class Widget {
                method() {
                    m();
                }
            }
        "#;
        let chunks = split(Path::new("w.ts"), "ts", src);
        assert!(chunks.iter().any(|c| c.meta.symbol == "alpha"));
    }
}
