//! Hand-written shell lexer.

use std::iter::Peekable;
use std::str::Chars;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Word(String),
    Assignment(String, String), // name, raw_value
    Number(i64),
    // Operators / punctuation
    Pipe,
    PipePipe,
    Amp,
    AmpAmp,
    Semi,
    LParen,
    RParen,
    /// Standalone arithmetic expression: `(( ... ))`.
    ArithExpr(String),
    Less,
    Greater,
    GreaterGreater,
    LessLess,
    LessGreater,
    LessAmp,
    GreaterAmp,
    // Misc
    Newline,
    Eof,
}

pub struct Lexer<'a> {
    input: &'a str,
    chars: Peekable<Chars<'a>>,
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input,
            chars: input.chars().peekable(),
            pos: 0,
        }
    }

    pub fn next_token(&mut self) -> Token {
        self.skip_whitespace();
        if let Some(&c) = self.chars.peek() {
            match c {
                '\n' => {
                    self.advance();
                    Token::Newline
                }
                '#' => {
                    self.skip_comment();
                    self.next_token()
                }
                ';' => {
                    self.advance();
                    Token::Semi
                }
                '|' => {
                    self.advance();
                    if self.peek_char_eq('|') {
                        self.advance();
                        Token::PipePipe
                    } else {
                        Token::Pipe
                    }
                }
                '&' => {
                    self.advance();
                    if self.peek_char_eq('&') {
                        self.advance();
                        Token::AmpAmp
                    } else {
                        Token::Amp
                    }
                }
                '(' => {
                    self.advance(); // consume '('
                    if self.peek_char_eq('(') {
                        self.advance(); // consume second '('
                        Token::ArithExpr(self.read_arith_expr())
                    } else {
                        Token::LParen
                    }
                }
                ')' => {
                    self.advance();
                    Token::RParen
                }
                '<' => {
                    self.advance();
                    match self.chars.peek() {
                        Some(&'<') => {
                            self.advance();
                            Token::LessLess
                        }
                        Some(&'>') => {
                            self.advance();
                            Token::LessGreater
                        }
                        Some('&') => {
                            self.advance();
                            Token::LessAmp
                        }
                        _ => Token::Less,
                    }
                }
                '>' => {
                    self.advance();
                    match self.chars.peek() {
                        Some('>') => {
                            self.advance();
                            Token::GreaterGreater
                        }
                        Some('&') => {
                            self.advance();
                            Token::GreaterAmp
                        }
                        _ => Token::Greater,
                    }
                }
                '0'..='9' => self.read_number_or_word(),
                _ => self.read_word(),
            }
        } else {
            Token::Eof
        }
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.chars.next();
        if let Some(ch) = c {
            self.pos += ch.len_utf8();
        }
        c
    }

    fn peek_char_eq(&mut self, c: char) -> bool {
        self.chars.peek() == Some(&c)
    }

    fn skip_whitespace(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c == ' ' || c == '\t' || c == '\r' {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn skip_comment(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c == '\n' {
                break;
            }
            self.advance();
        }
    }

    /// Read the body of a standalone `(( ... ))` arithmetic command.
    /// Called after the opening `((` has been consumed.
    fn read_arith_expr(&mut self) -> String {
        let start = self.pos;
        let mut depth = 2;
        let mut in_single = false;
        let mut in_double = false;
        let mut escape = false;

        while let Some(&c) = self.chars.peek() {
            if escape {
                self.advance();
                escape = false;
                continue;
            }
            if in_single {
                self.advance();
                if c == '\'' {
                    in_single = false;
                }
                continue;
            }
            if in_double {
                self.advance();
                match c {
                    '"' => in_double = false,
                    '\\' => escape = true,
                    _ => {}
                }
                continue;
            }
            match c {
                '\\' => {
                    self.advance();
                    escape = true;
                }
                '\'' => {
                    self.advance();
                    in_single = true;
                }
                '"' => {
                    self.advance();
                    in_double = true;
                }
                '(' => {
                    self.advance();
                    depth += 1;
                }
                ')' => {
                    self.advance();
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {
                    self.advance();
                }
            }
        }

        let raw = &self.input[start..self.pos];
        // Strip the closing `))`.
        if raw.len() >= 2 {
            raw[..raw.len() - 2].to_string()
        } else {
            raw.to_string()
        }
    }

    fn read_number_or_word(&mut self) -> Token {
        let start = self.pos;
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }
        let num_end = self.pos;
        // A number followed immediately by a redirection operator is a file
        // descriptor prefix (e.g. `2>file`, `2>&1`). Otherwise treat it as a
        // word if more non-terminator characters follow.
        if let Some(&c) = self.chars.peek() {
            if c == '<' || c == '>' {
                let num_str = &self.input[start..num_end];
                return Token::Number(num_str.parse().unwrap_or(0));
            }
            if !c.is_whitespace()
                && c != '\n'
                && c != ';'
                && c != '|'
                && c != '&'
                && c != '('
                && c != ')'
            {
                return self.read_word_from(start);
            }
        }
        let num_str = &self.input[start..num_end];
        Token::Number(num_str.parse().unwrap_or(0))
    }

    fn read_word(&mut self) -> Token {
        let start = self.pos;
        self.read_word_from(start)
    }

    fn read_word_from(&mut self, start: usize) -> Token {
        let mut quote_state = QuoteState::None;
        let mut dollar_state = DollarState::None;
        let mut has_assignment = false;
        let mut _eq_pos_valid = true; // tracks whether an unquoted '=' could start an assignment
        let mut name_len: Option<usize> = None;

        while let Some(&c) = self.chars.peek() {
            match quote_state {
                QuoteState::None => {
                    match dollar_state {
                        DollarState::None => match c {
                            '\\' => {
                                self.advance();
                                if self.advance().is_some() {
                                    _eq_pos_valid = false;
                                }
                            }
                            '\'' => {
                                quote_state = QuoteState::Single;
                                _eq_pos_valid = false;
                                self.advance();
                            }
                            '"' => {
                                quote_state = QuoteState::Double;
                                _eq_pos_valid = false;
                                self.advance();
                            }
                            '$' => {
                                self.advance();
                                if self.chars.peek() == Some(&'(') {
                                    self.advance();
                                    if self.chars.peek() == Some(&'(') {
                                        self.advance();
                                        dollar_state = DollarState::Arith(2);
                                    } else {
                                        dollar_state = DollarState::Command(1);
                                    }
                                } else if self.chars.peek() == Some(&'{') {
                                    self.advance();
                                    dollar_state = DollarState::Brace(1);
                                }
                            }
                            c if c.is_whitespace()
                                || c == ';'
                                || c == '|'
                                || c == '&'
                                || c == '<'
                                || c == '>'
                                || c == '#'
                                || c == '\n' =>
                            {
                                break;
                            }
                            '(' | ')' => {
                                let current = &self.input[start..self.pos];
                                if current.is_empty() {
                                    break;
                                }
                                if c == '(' && is_valid_name(current) {
                                    break;
                                }
                                // A paren terminates the current word. Parens that
                                // belong to $((...)) or $(...) are consumed while
                                // dollar_state is Arith or Command, not here.
                                break;
                            }
                            '=' => {
                                if _eq_pos_valid {
                                    let current_len = self.pos - start;
                                    if is_valid_name(&self.input[start..self.pos]) {
                                        name_len = Some(current_len);
                                        has_assignment = true;
                                    }
                                    _eq_pos_valid = false;
                                }
                                self.advance();
                            }
                            _ => {
                                self.advance();
                            }
                        },
                        DollarState::Arith(depth) => match c {
                            '(' => {
                                self.advance();
                                dollar_state = DollarState::Arith(depth + 1);
                            }
                            ')' => {
                                self.advance();
                                if depth == 1 {
                                    dollar_state = DollarState::None;
                                } else {
                                    dollar_state = DollarState::Arith(depth - 1);
                                }
                            }
                            _ => {
                                self.advance();
                            }
                        },
                        DollarState::Brace(depth) => match c {
                            '{' => {
                                self.advance();
                                dollar_state = DollarState::Brace(depth + 1);
                            }
                            '}' => {
                                self.advance();
                                if depth == 1 {
                                    dollar_state = DollarState::None;
                                } else {
                                    dollar_state = DollarState::Brace(depth - 1);
                                }
                            }
                            _ => {
                                self.advance();
                            }
                        },
                        DollarState::Command(depth) => match c {
                            '(' => {
                                self.advance();
                                dollar_state = DollarState::Command(depth + 1);
                            }
                            ')' => {
                                self.advance();
                                if depth == 1 {
                                    dollar_state = DollarState::None;
                                } else {
                                    dollar_state = DollarState::Command(depth - 1);
                                }
                            }
                            _ => {
                                self.advance();
                            }
                        },
                    }
                }
                QuoteState::Single => {
                    _eq_pos_valid = false;
                    self.advance();
                    if self.chars.peek() == Some(&'\'') {
                        self.advance();
                        quote_state = QuoteState::None;
                    }
                }
                QuoteState::Double => match c {
                    '"' => {
                        self.advance();
                        quote_state = QuoteState::None;
                    }
                    '\\' => {
                        self.advance();
                        self.advance();
                    }
                    _ => {
                        self.advance();
                    }
                },
            }
        }

        let raw = &self.input[start..self.pos];
        if has_assignment {
            if let Some(nlen) = name_len {
                let name = raw[..nlen].to_string();
                let value = raw[nlen + 1..].to_string();
                return Token::Assignment(name, value);
            }
        }

        classify_word(raw)
    }
}

#[derive(Debug, Clone, Copy)]
enum QuoteState {
    None,
    Single,
    Double,
}

#[derive(Debug, Clone, Copy)]
enum DollarState {
    None,
    Arith(usize),
    Brace(usize),
    Command(usize),
}

fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn classify_word(s: &str) -> Token {
    // Reserved words are recognised by the parser based on context, not the lexer,
    // so that ordinary words like `then` can be used as arguments.
    Token::Word(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(input: &str) -> Vec<Token> {
        let mut lexer = Lexer::new(input);
        let mut out = Vec::new();
        loop {
            let t = lexer.next_token();
            if t == Token::Eof {
                break;
            }
            out.push(t);
        }
        out
    }

    #[test]
    fn simple_command() {
        let t = tokens("echo hello world");
        assert_eq!(
            t,
            vec![
                Token::Word("echo".into()),
                Token::Word("hello".into()),
                Token::Word("world".into()),
            ]
        );
    }

    #[test]
    fn operators_and_redirects() {
        let t = tokens("a && b || c; d | e > f");
        assert_eq!(
            t,
            vec![
                Token::Word("a".into()),
                Token::AmpAmp,
                Token::Word("b".into()),
                Token::PipePipe,
                Token::Word("c".into()),
                Token::Semi,
                Token::Word("d".into()),
                Token::Pipe,
                Token::Word("e".into()),
                Token::Greater,
                Token::Word("f".into()),
            ]
        );
    }

    #[test]
    fn assignment_and_quotes() {
        let t = tokens("FOO=bar echo 'single quote'");
        assert_eq!(
            t,
            vec![
                Token::Assignment("FOO".into(), "bar".into()),
                Token::Word("echo".into()),
                Token::Word("'single quote'".into()),
            ]
        );
    }

    #[test]
    fn reserved_words_as_words() {
        let t = tokens("if true; then echo hi; fi");
        assert_eq!(
            t,
            vec![
                Token::Word("if".into()),
                Token::Word("true".into()),
                Token::Semi,
                Token::Word("then".into()),
                Token::Word("echo".into()),
                Token::Word("hi".into()),
                Token::Semi,
                Token::Word("fi".into()),
            ]
        );
    }
}
