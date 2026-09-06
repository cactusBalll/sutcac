//! Shared YAML frontmatter parsing for Markdown-based definitions.
//!
//! Used by both Agent Skills (`SKILL.md`) and Agent definitions (`*.md`).

use serde::de::DeserializeOwned;

/// Split file contents into frontmatter and body.
///
/// Frontmatter must start with `---` on its own line and end with a matching
/// `---` line. If there is no frontmatter, the whole file is treated as body.
pub fn split_frontmatter(contents: &str) -> Result<(String, String), String> {
    let trimmed = contents.trim_start();
    if !trimmed.starts_with("---") {
        return Ok((String::new(), contents.to_string()));
    }

    // Find the end delimiter, skipping the first line.
    let after_open = &trimmed[3..];
    let mut rest_lines = after_open.lines();
    let first = rest_lines.next().unwrap_or("");
    if !first.trim().is_empty() {
        return Err("frontmatter opening delimiter must be on its own line".to_string());
    }

    let mut frontmatter_lines = Vec::new();
    let mut found_close = false;
    for line in rest_lines {
        if line.trim() == "---" {
            found_close = true;
            break;
        }
        frontmatter_lines.push(line);
    }

    if !found_close {
        return Err("unclosed frontmatter delimiter".to_string());
    }

    let body_lines: Vec<&str> = after_open
        .lines()
        .skip(frontmatter_lines.len() + 2) // +2 for opening blank + closing delimiter
        .collect();

    Ok((frontmatter_lines.join("\n"), body_lines.join("\n")))
}

/// Parse YAML frontmatter into any deserializable type.
pub fn parse_frontmatter<T: DeserializeOwned + Default>(frontmatter: &str) -> Result<T, String> {
    if frontmatter.trim().is_empty() {
        return Ok(T::default());
    }
    serde_saphyr::from_str(frontmatter).map_err(|e| format!("invalid YAML frontmatter: {}", e))
}

/// Validate a short identifier used for skill/agent names.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name.len() > 64 {
        return Err("name must be at most 64 characters".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        return Err("name must not contain consecutive hyphens".to_string());
    }
    for ch in name.chars() {
        if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '-' {
            return Err(format!(
                "name contains invalid character '{}'; only lowercase letters, digits, and hyphens are allowed",
                ch
            ));
        }
    }
    Ok(())
}
