//! Simple glob matching for case patterns and pathname expansion.

/// Match a single glob pattern against a value.
pub fn matches(pattern: &str, value: &str) -> bool {
    match_internal(pattern, value)
}

/// Match any of several `|` separated patterns.
pub fn matches_any(patterns: &str, value: &str) -> bool {
    patterns.split('|').any(|p| matches(p.trim(), value))
}

fn match_internal(pattern: &str, value: &str) -> bool {
    // Greedy star: track backtrack positions.
    let mut star_p = None::<usize>;
    let mut star_v = None::<usize>;

    // Convert to indexable char vectors for backtracking simplicity.
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let mut pi = 0usize;
    let mut vi = 0usize;

    loop {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    star_p = Some(pi);
                    star_v = Some(vi);
                    pi += 1;
                    continue;
                }
                '?' => {
                    if vi < v.len() {
                        pi += 1;
                        vi += 1;
                        continue;
                    }
                }
                '[' => {
                    if vi < v.len() {
                        let (matched, consumed) = match_bracket(&p, pi, v[vi]);
                        if matched {
                            pi = consumed;
                            vi += 1;
                            continue;
                        }
                    }
                }
                c => {
                    if vi < v.len() && c == v[vi] {
                        pi += 1;
                        vi += 1;
                        continue;
                    }
                }
            }
        } else if vi >= v.len() {
            return true; // both consumed
        }

        // Mismatch: backtrack to last '*'.
        if let Some(sv) = star_v {
            pi = star_p.unwrap() + 1;
            vi = sv + 1;
            star_v = Some(vi);
            if vi > v.len() {
                return false;
            }
            continue;
        }

        return false;
    }
}

/// Parse a bracket expression starting at `start` in pattern `p`.
/// Returns (matched, index_after_bracket).
fn match_bracket(p: &[char], start: usize, ch: char) -> (bool, usize) {
    let mut i = start + 1;
    let mut negated = false;
    if i < p.len() && p[i] == '!' {
        negated = true;
        i += 1;
    }
    let mut matched = false;
    while i < p.len() && p[i] != ']' {
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            let lo = p[i];
            let hi = p[i + 2];
            if ch >= lo && ch <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == ch {
                matched = true;
            }
            i += 1;
        }
    }
    if i < p.len() && p[i] == ']' {
        i += 1;
    }
    (matched != negated, i)
}

/// Return filesystem entries matching a glob pattern.
///
/// Patterns without a `/` are matched against the current directory.  Patterns
/// with a directory prefix (e.g. `sutcac-sh/src/*.rs`) are matched against the
/// entries in that prefix directory.
pub fn expand_pathname(pattern: &str) -> Vec<String> {
    let mut results = Vec::new();

    if let Some(slash_pos) = pattern.rfind('/') {
        let dir_part = &pattern[..slash_pos];
        let file_pattern = &pattern[slash_pos + 1..];
        let dir = if dir_part.is_empty() {
            std::path::PathBuf::from(".")
        } else {
            std::path::PathBuf::from(dir_part)
        };
        let prefix = if dir_part.is_empty() {
            String::new()
        } else {
            format!("{}/", dir_part)
        };
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(name) = entry.file_name().into_string() {
                    if matches(file_pattern, &name) {
                        results.push(format!("{}{}", prefix, name));
                    }
                }
            }
        }
    } else {
        let dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(name) = entry.file_name().into_string() {
                    if matches(pattern, &name) {
                        results.push(name);
                    }
                }
            }
        }
    }

    results.sort();
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basic() {
        assert!(matches("*.txt", "file.txt"));
        assert!(!matches("*.txt", "file.doc"));
        assert!(matches("?at", "cat"));
        assert!(matches("[abc]at", "cat"));
        assert!(!matches("[!abc]at", "cat"));
        assert!(matches("a*b", "acb"));
    }

    #[test]
    fn expand_pathname_with_directory_prefix() {
        let base = std::env::temp_dir().join("sutcac-glob-test");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub/a.rs"), "").unwrap();
        std::fs::write(base.join("sub/b.txt"), "").unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&base).unwrap();
        let got = expand_pathname("sub/*.rs");
        std::env::set_current_dir(&original).unwrap();
        assert_eq!(got, vec!["sub/a.rs"]);
    }
}
