//! Word expansion: quotes, parameters, tilde, arithmetic, splitting, globbing.

use std::collections::HashMap;

use crate::arith;
use crate::glob;

/// Context needed to expand words.
pub struct ExpandContext<'a> {
    pub vars: &'a HashMap<String, String>,
    /// Positional parameters ($1, $2, ... and $@/$*).
    pub args: &'a [String],
    /// Exit status of the last command ($?).
    pub last_status: i32,
    /// Process id of the shell ($$).
    pub pid: u32,
    /// Last background job pid ($!).
    pub last_bg_pid: Option<u32>,
    /// Optional executor for `$(...)` command substitutions.
    pub subst: Option<&'a mut dyn FnMut(&str) -> Result<String, String>>,
    /// Buffer that accumulates variable side effects produced by `$((...))`
    /// arithmetic expansion (e.g. `++`/`--` and assignments). The caller is
    /// responsible for applying these updates to the shell state after expansion.
    /// `None` means side effects are discarded (used for contexts where the
    /// caller does not need to apply updates).
    pub arith_updates: Option<&'a mut Vec<(String, i64)>>,
}

impl<'a> ExpandContext<'a> {
    pub fn new(
        vars: &'a HashMap<String, String>,
        args: &'a [String],
        last_status: i32,
        pid: u32,
    ) -> Self {
        Self {
            vars,
            args,
            last_status,
            pid,
            last_bg_pid: None,
            subst: None,
            arith_updates: None,
        }
    }
}

#[derive(Debug, Clone)]
struct Segment {
    value: String,
    /// If true, this segment is protected from word splitting and globbing.
    quoted: bool,
    /// If true, this segment starts a new field and does not merge with
    /// adjacent segments. Used for each positional parameter from "$@".
    boundary: bool,
}

/// Expand a single word into zero or more fields.
pub fn expand_word(word: &str, ctx: &mut ExpandContext<'_>) -> Result<Vec<String>, String> {
    let segments = expand_to_segments(word, ctx)?;
    let segments = expand_braces_in_segments(segments);
    let fields = split_words(segments);
    Ok(expand_globs(fields))
}

/// Expand a word but keep the result as a single string. Used for assignments
/// and redirection targets where word splitting does not occur.
pub fn expand_single(word: &str, ctx: &mut ExpandContext<'_>) -> Result<String, String> {
    Ok(expand_to_segments(word, ctx)?
        .into_iter()
        .map(|s| s.value)
        .collect())
}

fn expand_to_segments(word: &str, ctx: &mut ExpandContext<'_>) -> Result<Vec<Segment>, String> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut chars = word.chars().peekable();

    enum State {
        None,
        Single,
        Double,
    }
    let mut state = State::None;

    while let Some(c) = chars.peek().copied() {
        match state {
            State::None => match c {
                '\\' => {
                    chars.next();
                    if let Some(next) = chars.next() {
                        current_seg(&mut segments, false).push(next);
                    }
                }
                '\'' => {
                    chars.next();
                    state = State::Single;
                    current_seg(&mut segments, true);
                }
                '"' => {
                    chars.next();
                    state = State::Double;
                    current_seg(&mut segments, true);
                }
                '$' => {
                    chars.next();
                    expand_dollar(&mut chars, ctx, false, &mut segments)?;
                }
                '~' if segments.is_empty() || is_after_assignment(&segments) => {
                    chars.next();
                    let user = read_tilde_user(&mut chars);
                    let expanded = expand_tilde(&user);
                    current_seg(&mut segments, false).push_str(&expanded);
                }
                _ => {
                    chars.next();
                    current_seg(&mut segments, false).push(c);
                }
            },
            State::Single => match c {
                '\'' => {
                    chars.next();
                    state = State::None;
                }
                _ => {
                    chars.next();
                    current_seg(&mut segments, true).push(c);
                }
            },
            State::Double => match c {
                '"' => {
                    chars.next();
                    state = State::None;
                }
                '\\' => {
                    chars.next();
                    if let Some(next) = chars.next() {
                        match next {
                            '$' | '\\' | '"' | '\n' => {
                                current_seg(&mut segments, true).push(next);
                            }
                            _ => {
                                current_seg(&mut segments, true).push('\\');
                                current_seg(&mut segments, true).push(next);
                            }
                        }
                    }
                }
                '$' => {
                    chars.next();
                    expand_dollar(&mut chars, ctx, true, &mut segments)?;
                }
                _ => {
                    chars.next();
                    current_seg(&mut segments, true).push(c);
                }
            },
        }
    }

    Ok(segments)
}

fn current_seg(segments: &mut Vec<Segment>, quoted: bool) -> &mut String {
    if let Some(last) = segments.last_mut() {
        if last.quoted == quoted {
            return &mut last.value;
        }
    }
    segments.push(Segment {
        value: String::new(),
        quoted,
        boundary: false,
    });
    &mut segments.last_mut().unwrap().value
}

fn push_seg(segments: &mut Vec<Segment>, value: String, quoted: bool) {
    if let Some(last) = segments.last_mut() {
        if last.quoted == quoted {
            last.value.push_str(&value);
            return;
        }
    }
    segments.push(Segment {
        value,
        quoted,
        boundary: false,
    });
}

fn expand_dollar(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    ctx: &mut ExpandContext<'_>,
    in_double_quotes: bool,
    segments: &mut Vec<Segment>,
) -> Result<(), String> {
    match chars.peek() {
        Some(&'(') => {
            chars.next();
            if chars.peek() == Some(&'(') {
                chars.next();
                let expr = read_balanced(chars, '(', ')');
                // Consume the closing ')' of the outer '$('.
                if chars.peek() == Some(&')') {
                    chars.next();
                }
                let expr = expand_arith_expr(&expr, ctx)?;
                let mut arith_vars = ctx.vars.clone();
                let val = arith::evaluate(&expr, &mut arith_vars)
                    .map_err(|e| format!("arithmetic error: {}. Hint: use integer expressions only and avoid division by zero.", e))?;
                if let Some(updates) = ctx.arith_updates.as_mut() {
                    for (name, value) in &arith_vars {
                        let changed =
                            ctx.vars.get(name).map(|s| s.as_str()) != Some(value.as_str());
                        if changed {
                            if let Ok(n) = value.parse::<i64>() {
                                updates.push((name.clone(), n));
                            }
                        }
                    }
                }
                push_seg(segments, val.to_string(), in_double_quotes);
            } else {
                let inner = read_balanced(chars, '(', ')');
                if let Some(executor) = ctx.subst.as_mut() {
                    let output = executor(&inner).map_err(|e| {
                        format!(
                            "command substitution '$(...)' failed: {}. Hint: check that the inner command is valid and supported.",
                            e
                        )
                    })?;
                    push_seg(segments, output, in_double_quotes);
                } else {
                    return Err("command substitution '$(...)' is not implemented. Hint: rewrite the command without '$(...)' (for example, run the inner command separately and use its output explicitly).".to_string());
                }
            }
        }
        Some(&'{') => {
            chars.next();
            let name = read_braced_name(chars);
            let val = expand_param(&name, ctx);
            push_seg(segments, val, in_double_quotes);
        }
        Some(&c) if c.is_ascii_digit() => {
            let n = chars.next().unwrap().to_digit(10).unwrap() as usize;
            let val = positional_arg(n, ctx);
            push_seg(segments, val, in_double_quotes);
        }
        Some(&c) if c == '_' || c.is_ascii_alphabetic() => {
            let name = read_name(chars);
            let val = expand_param(&name, ctx);
            push_seg(segments, val, in_double_quotes);
        }
        Some(&'@') => {
            chars.next();
            if in_double_quotes {
                for arg in ctx.args {
                    segments.push(Segment {
                        value: arg.clone(),
                        quoted: true,
                        boundary: true,
                    });
                }
            } else {
                push_seg(segments, ctx.args.join(" "), false);
            }
        }
        Some(&'*') => {
            chars.next();
            push_seg(segments, ctx.args.join(" "), in_double_quotes);
        }
        Some(&'#') | Some(&'?') | Some(&'$') | Some(&'!') | Some(&'-') => {
            let name = chars.next().unwrap().to_string();
            let val = expand_param(&name, ctx);
            push_seg(segments, val, in_double_quotes);
        }
        _ => {
            push_seg(segments, "$".to_string(), in_double_quotes);
        }
    }
    Ok(())
}

fn is_after_assignment(segments: &[Segment]) -> bool {
    if segments.len() != 1 {
        return false;
    }
    let s = &segments[0].value;
    if let Some(pos) = s.find('=') {
        let name = &s[..pos];
        name.chars()
            .next()
            .map_or(false, |c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    } else {
        false
    }
}

fn read_name(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut name = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_alphanumeric() || c == '_' {
            name.push(c);
            chars.next();
        } else {
            break;
        }
    }
    name
}

fn read_braced_name(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut name = String::new();
    let mut depth = 1;
    while let Some(c) = chars.next() {
        if c == '{' {
            depth += 1;
            name.push(c);
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                break;
            }
            name.push(c);
        } else {
            name.push(c);
        }
    }
    name
}

fn read_tilde_user(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut user = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            user.push(c);
            chars.next();
        } else {
            break;
        }
    }
    user
}

fn expand_tilde(user: &str) -> String {
    if user.is_empty() {
        std::env::var("HOME").unwrap_or_else(|_| String::from("~"))
    } else {
        home_dir_for_user(user).unwrap_or_else(|| format!("~{}", user))
    }
}

fn home_dir_for_user(user: &str) -> Option<String> {
    if let Ok(contents) = std::fs::read_to_string("/etc/passwd") {
        for line in contents.lines() {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 6 && parts[0] == user {
                return Some(parts[5].to_string());
            }
        }
    }
    None
}

fn read_balanced(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    open: char,
    close: char,
) -> String {
    let mut buf = String::new();
    let mut depth = 1usize;
    while let Some(c) = chars.next() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        buf.push(c);
    }
    buf
}

fn expand_param(name: &str, ctx: &ExpandContext<'_>) -> String {
    match name {
        "?" => ctx.last_status.to_string(),
        "$" => ctx.pid.to_string(),
        "!" => ctx.last_bg_pid.map(|p| p.to_string()).unwrap_or_default(),
        "#" => ctx.args.len().to_string(),
        _ => {
            if let Ok(n) = name.parse::<usize>() {
                positional_arg(n, ctx)
            } else {
                ctx.vars.get(name).cloned().unwrap_or_default()
            }
        }
    }
}

fn positional_arg(n: usize, ctx: &ExpandContext<'_>) -> String {
    if n == 0 {
        ctx.vars.get("0").cloned().unwrap_or_default()
    } else if n <= ctx.args.len() {
        ctx.args[n - 1].clone()
    } else {
        String::new()
    }
}

/// Expand $var/${var}/$1/$? etc. inside an arithmetic expression so the
/// arithmetic evaluator sees literal integers.
pub(crate) fn expand_arith_expr(expr: &str, ctx: &ExpandContext<'_>) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = expr.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            match chars.peek() {
                Some(&'{') => {
                    chars.next();
                    let name = read_braced_name(&mut chars);
                    out.push_str(&value_or_zero(&expand_param(&name, ctx)));
                }
                Some(&c) if c.is_ascii_digit() => {
                    let n = chars.next().unwrap().to_digit(10).unwrap() as usize;
                    out.push_str(&value_or_zero(&positional_arg(n, ctx)));
                }
                Some(&c) if c == '_' || c.is_ascii_alphabetic() => {
                    let name = read_name(&mut chars);
                    out.push_str(&value_or_zero(&expand_param(&name, ctx)));
                }
                Some(&'*') | Some(&'@') | Some(&'#') | Some(&'?') | Some(&'$') | Some(&'!')
                | Some(&'-') => {
                    let name = chars.next().unwrap().to_string();
                    out.push_str(&value_or_zero(&expand_param(&name, ctx)));
                }
                _ => {
                    out.push('$');
                }
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn value_or_zero(s: &str) -> String {
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

fn is_ifs(c: char) -> bool {
    c == ' ' || c == '\t' || c == '\n'
}

/// Split segments into fields using a simplified IFS (space, tab, newline).
/// Perform brace expansion on each unquoted segment.
///
/// Quoted segments are left untouched so that `echo "{a,b}"` prints the literal
/// brace string. Each brace expansion result becomes a new segment marked as a
/// boundary so that splitting preserves the generated words.
fn expand_braces_in_segments(segments: Vec<Segment>) -> Vec<Segment> {
    let mut out = Vec::new();
    for seg in segments {
        if seg.quoted {
            out.push(seg);
            continue;
        }
        let expansions = expand_braces(&seg.value);
        for (i, value) in expansions.into_iter().enumerate() {
            out.push(Segment {
                value,
                quoted: false,
                boundary: i > 0 || seg.boundary,
            });
        }
    }
    out
}

/// Expand brace patterns in a string.
///
/// Supports:
/// - Comma-separated alternatives: `{a,b,c}`
/// - Integer sequences: `{1..10}`, `{1..10..2}`
/// - Character sequences: `{a..z}`, `{a..e..2}`
/// - Nested braces: `{a,{b,c}}`
fn expand_braces(value: &str) -> Vec<String> {
    // Find the first top-level opening brace that should be expanded.
    let mut depth = 0;
    let mut start = None;
    for (i, c) in value.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start {
                        let body = &value[start + 1..i];
                        let has_comma = split_brace_body(body).len() > 1;
                        let is_sequence = expand_brace_sequence(&value[start..=i]).is_some();
                        if !body.is_empty() && (has_comma || is_sequence) {
                            let prefix = &value[..start];
                            let suffix = &value[i + 1..];
                            let body_expansions: Vec<String> = if has_comma {
                                split_brace_body(body)
                                    .into_iter()
                                    .flat_map(|part| expand_braces(part))
                                    .collect()
                            } else {
                                expand_brace_sequence(&value[start..=i])
                                    .unwrap_or_else(|| vec![value.to_string()])
                            };
                            let expanded_prefix = expand_braces(prefix);
                            let expanded_suffix = expand_braces(suffix);
                            let mut result = Vec::new();
                            for p in &expanded_prefix {
                                for b in &body_expansions {
                                    for s in &expanded_suffix {
                                        result.push(format!("{}{}{}", p, b, s));
                                    }
                                }
                            }
                            return result;
                        }
                    }
                    // Not an expandable brace; keep scanning.
                    start = None;
                }
            }
            _ => {}
        }
    }
    // No braces to expand; check for `{start..end[..step]}` syntax.
    if let Some(seq) = expand_brace_sequence(value) {
        return seq;
    }
    vec![value.to_string()]
}

/// Split a brace body on top-level commas.
fn split_brace_body(body: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in body.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&body[start..]);
    parts
}

/// Expand a single top-level `{start..end[..step]}` pattern.
fn expand_brace_sequence(value: &str) -> Option<Vec<String>> {
    let inner = value.strip_prefix('{')?.strip_suffix('}')?;
    let parts: Vec<&str> = inner.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let start = parts[0].trim();
    let end = parts[1].trim();
    let step = parts.get(2).map(|s| s.trim());

    if start.len() == 1 && end.len() == 1 {
        let start_ch = start.chars().next()?;
        let end_ch = end.chars().next()?;
        let step = step.and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
        expand_char_sequence(start_ch, end_ch, step)
    } else {
        let start_n = start.parse::<i64>().ok()?;
        let end_n = end.parse::<i64>().ok()?;
        let step = step.and_then(|s| s.parse::<i64>().ok()).unwrap_or(1);
        expand_int_sequence(start_n, end_n, step)
    }
}

fn expand_int_sequence(start: i64, end: i64, step: i64) -> Option<Vec<String>> {
    if step == 0 {
        return None;
    }
    let mut result = Vec::new();
    if start <= end && step > 0 {
        let mut i = start;
        while i <= end {
            result.push(i.to_string());
            i = i.checked_add(step)?;
        }
    } else if start >= end && step < 0 {
        let mut i = start;
        while i >= end {
            result.push(i.to_string());
            i = i.checked_add(step)?;
        }
    } else {
        return None;
    }
    Some(result)
}

fn expand_char_sequence(start: char, end: char, step: usize) -> Option<Vec<String>> {
    if step == 0 {
        return None;
    }
    let start_u = start as u32;
    let end_u = end as u32;
    if start_u == end_u {
        return Some(vec![start.to_string()]);
    }
    let mut result = Vec::new();
    if start_u < end_u {
        let mut u = start_u;
        while u <= end_u {
            if let Some(c) = char::from_u32(u) {
                result.push(c.to_string());
            }
            u = u.checked_add(step as u32)?;
        }
    } else {
        let mut u = start_u;
        while u >= end_u {
            if let Some(c) = char::from_u32(u) {
                result.push(c.to_string());
            }
            u = u.saturating_sub(step as u32);
            if u < end_u {
                break;
            }
        }
    }
    Some(result)
}

fn split_words(segments: Vec<Segment>) -> Vec<Segment> {
    let mut fields: Vec<Segment> = Vec::new();
    let mut current = String::new();
    let mut current_quoted = false;

    fn flush(fields: &mut Vec<Segment>, current: &mut String, current_quoted: &mut bool) {
        if !current.is_empty() {
            fields.push(Segment {
                value: std::mem::take(current),
                quoted: *current_quoted,
                boundary: false,
            });
            *current_quoted = false;
        }
    }

    for seg in segments {
        if seg.boundary && !current.is_empty() {
            flush(&mut fields, &mut current, &mut current_quoted);
        }
        if seg.quoted {
            current.push_str(&seg.value);
            current_quoted = true;
        } else {
            let parts: Vec<&str> = seg.value.split(is_ifs).collect();
            for part in parts {
                if part.is_empty() {
                    flush(&mut fields, &mut current, &mut current_quoted);
                } else {
                    if current.is_empty() {
                        current.push_str(part);
                    } else {
                        flush(&mut fields, &mut current, &mut current_quoted);
                        current.push_str(part);
                    }
                }
            }
        }
    }

    if !current.is_empty() || current_quoted {
        fields.push(Segment {
            value: current,
            quoted: current_quoted,
            boundary: false,
        });
    }

    fields
}

/// Apply pathname expansion to fields that contain unquoted glob characters.
fn expand_globs(fields: Vec<Segment>) -> Vec<String> {
    let mut out = Vec::new();
    for field in fields {
        if field.quoted || !has_glob_char(&field.value) {
            out.push(field.value);
        } else {
            let matches = glob::expand_pathname(&field.value);
            if matches.is_empty() {
                out.push(field.value);
            } else {
                out.extend(matches);
            }
        }
    }
    out
}

fn has_glob_char(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_vars(vars: HashMap<String, String>) -> ExpandContext<'static> {
        ExpandContext {
            vars: Box::leak(Box::new(vars)),
            args: Box::leak(Box::new(vec![String::from("a"), String::from("b")])),
            last_status: 42,
            pid: 1234,
            last_bg_pid: None,
            subst: None,
            arith_updates: None,
        }
    }

    #[test]
    fn basic_expansion() {
        let mut vars = HashMap::new();
        vars.insert("X".into(), "hello".into());
        let mut c = ctx_with_vars(vars);
        assert_eq!(expand_word("$X", &mut c).unwrap(), vec!["hello"]);
        assert_eq!(expand_word("'$X'", &mut c).unwrap(), vec!["$X"]);
        assert_eq!(expand_word("\"$X\"", &mut c).unwrap(), vec!["hello"]);
    }

    #[test]
    fn quoted_glob_is_literal() {
        let mut c = ctx_with_vars(HashMap::new());
        assert_eq!(expand_word("'*.txt'", &mut c).unwrap(), vec!["*.txt"]);
        assert_eq!(expand_word("\"*.txt\"", &mut c).unwrap(), vec!["*.txt"]);
    }

    #[test]
    fn quoted_glob_with_matching_files_is_literal() {
        let dir = std::env::temp_dir().join("sutcac_sh_quoted_glob_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("a.txt")).unwrap();
        std::fs::File::create(dir.join("b.txt")).unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let mut c = ctx_with_vars(HashMap::new());
        assert_eq!(expand_word("'*.txt'", &mut c).unwrap(), vec!["*.txt"]);
        assert_eq!(expand_word("\"*.txt\"", &mut c).unwrap(), vec!["*.txt"]);
        std::env::set_current_dir(original).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn brace_expansion() {
        let mut c = ctx_with_vars(HashMap::new());
        assert_eq!(expand_word("{a,b,c}", &mut c).unwrap(), vec!["a", "b", "c"]);
        assert_eq!(expand_word("{1..3}", &mut c).unwrap(), vec!["1", "2", "3"]);
        assert_eq!(expand_word("{a..c}", &mut c).unwrap(), vec!["a", "b", "c"]);
        assert_eq!(
            expand_word("file{1..3}.txt", &mut c).unwrap(),
            vec!["file1.txt", "file2.txt", "file3.txt"]
        );
        assert_eq!(
            expand_word("{1..5..2}", &mut c).unwrap(),
            vec!["1", "3", "5"]
        );
        assert_eq!(
            expand_word("{a,{b,c}}", &mut c).unwrap(),
            vec!["a", "b", "c"]
        );
        assert_eq!(expand_word("\"{1..3}\"", &mut c).unwrap(), vec!["{1..3}"]);
        assert_eq!(expand_word("{}", &mut c).unwrap(), vec!["{}"]);
    }

    #[test]
    fn special_vars() {
        let mut c = ctx_with_vars(HashMap::new());
        assert_eq!(expand_word("$?", &mut c).unwrap(), vec!["42"]);
        assert_eq!(expand_word("$$", &mut c).unwrap(), vec!["1234"]);
        assert_eq!(expand_word("$#", &mut c).unwrap(), vec!["2"]);
        assert_eq!(expand_word("$1", &mut c).unwrap(), vec!["a"]);
        assert_eq!(expand_word("\"$@\"", &mut c).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn word_splitting() {
        let mut vars = HashMap::new();
        vars.insert("X".into(), "one two".into());
        let mut c = ctx_with_vars(vars);
        assert_eq!(expand_word("$X", &mut c).unwrap(), vec!["one", "two"]);
        assert_eq!(expand_word("\"$X\"", &mut c).unwrap(), vec!["one two"]);
    }

    #[test]
    fn arithmetic_expansion() {
        let mut c = ctx_with_vars(HashMap::new());
        assert_eq!(expand_word("$((2+3))", &mut c).unwrap(), vec!["5"]);
    }

    #[test]
    fn command_substitution_is_error_without_executor() {
        let mut c = ctx_with_vars(HashMap::new());
        assert!(expand_word("$(echo hi)", &mut c).is_err());
    }

    #[test]
    fn command_substitution_with_executor() {
        let mut executor = |inner: &str| -> Result<String, String> {
            if inner == "echo hi" {
                Ok("hi".to_string())
            } else {
                Err("unexpected".to_string())
            }
        };
        let mut c = ExpandContext {
            vars: Box::leak(Box::new(HashMap::new())),
            args: Box::leak(Box::new(vec![String::from("a"), String::from("b")])),
            last_status: 42,
            pid: 1234,
            last_bg_pid: None,
            subst: Some(&mut executor),
            arith_updates: None,
        };
        assert_eq!(expand_word("$(echo hi)", &mut c).unwrap(), vec!["hi"]);
        assert_eq!(expand_word("x$(echo hi)y", &mut c).unwrap(), vec!["xhiy"]);
    }
}
