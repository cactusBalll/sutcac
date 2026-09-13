//! The `code` tokenizer for the Tantivy BM25 lane.
//!
//! Rules (design §4.4): lowercase, no stemming, no stopword removal,
//! identifier dual-granularity output — each alphanumeric run produces its
//! whole lowercase form plus segments and sub-tokens split at camelCase /
//! digit boundaries. Index side and query side must register this tokenizer
//! under the same name or BM25 silently degenerates.

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// Name under which the tokenizer must be registered on index and query
/// sides alike (`f_text.set_tokenizer` + `index.tokenizers().add`).
pub const CODE_TOKENIZER_NAME: &str = "code";

/// Dual-granularity code tokenizer (see module docs).
#[derive(Clone, Debug, Default)]
pub struct CodeTokenizer;

impl CodeTokenizer {
    pub fn new() -> Self {
        Self
    }
}

/// Split one alphanumeric run into sub-tokens per the design §4.4 example
/// table: emit the run as a whole, then per underscore segment emit the
/// segment (skipping duplicates already produced) plus its camelCase /
/// digit sub-split.
fn sub_tokens(whole: &str) -> Vec<String> {
    let whole_lower = whole.to_lowercase();
    let segments: Vec<&str> = whole
        .split(|c: char| c == '_' || c == '-')
        .filter(|s| !s.is_empty())
        .collect();
    let mut out = Vec::new();
    // Pass 1: whole segments per §4.4 (`refresh_token2_expiry` keeps
    // `token2` whole in addition to its sub-split).
    for part in &segments {
        let lowered = part.to_lowercase();
        if lowered != whole_lower && !out.contains(&lowered) {
            out.push(lowered);
        }
    }
    // Pass 2: camelCase / digit sub-splits.
    for part in &segments {
        let part_lower = part.to_lowercase();
        for s in camel_split(part) {
            let lowered = s.to_lowercase();
            if lowered != whole_lower && lowered != part_lower && !out.contains(&lowered) {
                out.push(lowered);
            }
        }
    }
    out
}

/// camelCase / digit-boundary sub-split of one lowercase word.
fn camel_split(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    if chars.len() <= 1 {
        return vec![word.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cur = chars[0].to_string();
    for i in 1..chars.len() {
        let prev = chars[i - 1];
        let cur_c = chars[i];
        let prev2 = if i >= 2 { chars[i - 2] } else { '\0' };
        let boundary = (prev.is_lowercase() && cur_c.is_uppercase())
            || (prev.is_ascii_digit() != cur_c.is_ascii_digit())
            || (cur_c.is_lowercase() && prev.is_uppercase() && prev2.is_uppercase());
        if boundary {
            parts.push(std::mem::take(&mut cur));
            cur = String::new();
        }
        cur.extend(cur_c.to_lowercase());
    }
    parts.push(cur);
    parts.into_iter().filter(|s| !s.is_empty()).collect()
}

/// Materialise the full token list for `text` (the stream rebuilds per
/// query; chunk texts are small so this stays cheap).
pub fn tokenize(text: &str) -> Vec<Token> {
    let run_re = regex::bytes::Regex::new(r"[A-Za-z0-9_]+").unwrap();
    let mut tokens = Vec::new();
    let mut position = 0usize;
    for m in run_re.find_iter(text.as_bytes()) {
        let start = m.start();
        let end = m.end();
        let whole = &text[start..end];
        tokens.push(Token {
            text: whole.to_lowercase(),
            offset_from: start,
            offset_to: end,
            position,
            position_length: 1,
        });
        position += 1;
        for sub in sub_tokens(whole) {
            tokens.push(Token {
                text: sub,
                offset_from: start,
                offset_to: end,
                position,
                position_length: 1,
            });
            position += 1;
        }
    }
    tokens
}

impl Tokenizer for CodeTokenizer {
    type TokenStream<'a> = FilterStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        FilterStream {
            tokens: tokenize(text),
            cursor: 0,
        }
    }
}

/// Token stream over a pre-materialised token list.
pub struct FilterStream {
    tokens: Vec<Token>,
    cursor: usize,
}

impl TokenStream for FilterStream {
    fn advance(&mut self) -> bool {
        self.cursor += 1;
        self.cursor <= self.tokens.len()
    }

    fn token(&self) -> &Token {
        &self.tokens[self.cursor - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.cursor - 1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the tokenizer and collect token texts in order.
    fn tokens_for(text: &str) -> Vec<String> {
        let mut tok = CodeTokenizer::new();
        let mut stream = tok.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    #[test]
    fn covers_design_examples() {
        // §4.4 example table, exact order preserved.
        assert_eq!(tokens_for("UserID"), vec!["userid", "user", "id"]);
        assert_eq!(
            tokens_for("refresh_token2_expiry"),
            vec![
                "refresh_token2_expiry",
                "refresh",
                "token2",
                "expiry",
                "token",
                "2",
            ]
        );
        assert_eq!(tokens_for("MAX_RETRY"), vec!["max_retry", "max", "retry"]);
        assert_eq!(tokens_for("toString()"), vec!["tostring", "to", "string"]);
    }

    #[test]
    fn mixed_input() {
        assert_eq!(
            tokens_for("parse_context_window(2)"),
            vec!["parse_context_window", "parse", "context", "window", "2",]
        );
        // No alphanumeric runs -> no tokens.
        assert!(tokens_for("").is_empty());
        assert!(tokens_for("--[]()").is_empty());
    }
}
