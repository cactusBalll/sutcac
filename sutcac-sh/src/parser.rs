//! Recursive-descent parser for the shell grammar.

use crate::ast::*;
use crate::lexer::{Lexer, Token};

#[derive(Debug, Clone)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parse error: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

pub struct Parser<'a> {
    lexer: Lexer<'a>,
    current: Token,
    next: Token,
}

impl<'a> Parser<'a> {
    pub fn new(input: &'a str) -> Result<Self, ParseError> {
        let mut lexer = Lexer::new(input);
        let current = lexer.next_token();
        let next = lexer.next_token();
        Ok(Self {
            lexer,
            current,
            next,
        })
    }

    pub fn parse(&mut self) -> Result<Vec<Command>, ParseError> {
        let mut cmds = Vec::new();
        self.skip_newlines();
        while !self.at_eof() {
            cmds.push(self.parse_list()?);
            self.skip_newlines();
        }
        Ok(cmds)
    }

    // ------------------------------------------------------------------
    // Low-level helpers
    // ------------------------------------------------------------------

    fn bump(&mut self) {
        self.current = self.next.clone();
        self.next = self.lexer.next_token();
    }

    fn peek(&self) -> &Token {
        &self.next
    }

    fn at_eof(&self) -> bool {
        self.current == Token::Eof
    }

    fn skip_newlines(&mut self) {
        while self.current == Token::Newline {
            self.bump();
        }
    }

    fn expect(&mut self, expected: Token) -> Result<(), ParseError> {
        if self.current == expected {
            self.bump();
            Ok(())
        } else {
            Err(ParseError(format!(
                "expected {:?}, found {:?}",
                expected, self.current
            )))
        }
    }

    fn word(&mut self) -> Result<Word, ParseError> {
        match &self.current {
            Token::Word(s) | Token::Assignment(_, s) => {
                let w = Word::new(s.clone());
                self.bump();
                Ok(w)
            }
            Token::Number(n) => {
                let w = Word::new(n.to_string());
                self.bump();
                Ok(w)
            }
            _ => Err(ParseError(format!(
                "expected word, found {:?}",
                self.current
            ))),
        }
    }

    fn current_word(&self) -> Option<&str> {
        match &self.current {
            Token::Word(s) | Token::Assignment(_, s) => Some(s),
            _ => None,
        }
    }

    fn word_is(&self, s: &str) -> bool {
        self.current_word() == Some(s)
    }

    fn peek_is_redirect(&self) -> bool {
        matches!(
            self.peek(),
            Token::Less
                | Token::Greater
                | Token::GreaterGreater
                | Token::LessLess
                | Token::LessGreater
                | Token::LessAmp
                | Token::GreaterAmp
        )
    }

    // ------------------------------------------------------------------
    // Lists and pipelines
    // ------------------------------------------------------------------

    fn at_terminator(&self, terminators: &[&str]) -> bool {
        if self.current == Token::RBrace || self.current == Token::RParen {
            return true;
        }
        if terminators.contains(&";;") && self.at_dsemi() {
            return true;
        }
        if let Some(w) = self.current_word() {
            terminators.iter().any(|t| *t == w)
        } else {
            false
        }
    }

    fn at_dsemi(&self) -> bool {
        self.current == Token::Semi && self.peek() == &Token::Semi
    }

    fn parse_list(&mut self) -> Result<Command, ParseError> {
        self.parse_list_until(&[])
    }

    fn parse_list_until(&mut self, terminators: &[&str]) -> Result<Command, ParseError> {
        let mut left = self.parse_pipeline_until(terminators)?;
        loop {
            if self.at_terminator(terminators) || self.at_eof() {
                break;
            }
            let op = match &self.current {
                Token::Semi => ListOp::Semi,
                Token::AmpAmp => ListOp::And,
                Token::PipePipe => ListOp::Or,
                _ => break,
            };
            self.bump();
            if self.at_terminator(terminators) || self.at_eof() {
                // Trailing separator before a terminator.
                break;
            }
            let right = self.parse_pipeline_until(terminators)?;
            left = Command::Connection {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_pipeline_until(&mut self, terminators: &[&str]) -> Result<Command, ParseError> {
        if self.at_terminator(terminators) || self.at_eof() {
            return Err(ParseError("empty command".into()));
        }
        let mut cmds = vec![self.parse_command()?];
        while self.current == Token::Pipe {
            self.bump();
            if self.at_terminator(terminators) || self.at_eof() {
                return Err(ParseError("empty pipeline segment".into()));
            }
            cmds.push(self.parse_command()?);
        }
        if cmds.len() == 1 {
            Ok(cmds.into_iter().next().unwrap())
        } else {
            Ok(Command::Pipeline(cmds))
        }
    }

    fn parse_command(&mut self) -> Result<Command, ParseError> {
        // Function definition: function name [()] { ... }
        if self.word_is("function") {
            return self.parse_function_def();
        }

        // Function definition: name() { ... }
        if let Some(name) = self.current_word().map(|s| s.to_string()) {
            if is_valid_name(&name) && self.peek() == &Token::LParen {
                return self.parse_function_def_with_name(&name);
            }
        }

        // Compound commands
        if self.word_is("if") {
            return self.parse_if();
        }
        if self.word_is("while") {
            return self.parse_while();
        }
        if self.word_is("for") {
            return self.parse_for();
        }
        if self.word_is("case") {
            return self.parse_case();
        }
        if self.current == Token::LBrace {
            return self.parse_group();
        }
        if self.current == Token::LParen {
            return self.parse_subshell();
        }

        self.parse_simple_command(None)
    }

    // ------------------------------------------------------------------
    // Simple command
    // ------------------------------------------------------------------

    fn parse_simple_command(&mut self, first_word: Option<Word>) -> Result<Command, ParseError> {
        let mut cmd = SimpleCommand::default();
        if let Some(w) = first_word {
            cmd.words.push(w);
        }
        loop {
            match &self.current {
                Token::Word(_) | Token::Number(_) => {
                    if self.peek_is_redirect() {
                        // Leading fd number: e.g. 2>
                        if let Token::Number(n) = &self.current {
                            let fd = *n as i32;
                            self.bump();
                            cmd.redirects.push(self.parse_redirect_with_fd(Some(fd))?);
                        } else {
                            cmd.words.push(self.word()?);
                        }
                    } else {
                        cmd.words.push(self.word()?);
                    }
                }
                Token::Assignment(name, value) => {
                    if cmd.words.is_empty() {
                        // Leading assignment: applies to the command (environment variable).
                        cmd.assignments
                            .push((name.clone(), Word::new(value.clone())));
                    } else {
                        // Assignment after the command word is an ordinary argument,
                        // e.g. `export FOO=bar` or `make CC=gcc`.
                        cmd.words.push(Word::new(format!("{}={}", name, value)));
                    }
                    self.bump();
                    // A redirect may follow an assignment or an argument.
                    if self.redirect_follows() {
                        cmd.redirects.push(self.parse_redirect()?);
                    }
                }
                Token::Less
                | Token::Greater
                | Token::GreaterGreater
                | Token::LessLess
                | Token::LessGreater
                | Token::LessAmp
                | Token::GreaterAmp => {
                    cmd.redirects.push(self.parse_redirect()?);
                }
                _ => break,
            }
        }
        Ok(Command::Simple(cmd))
    }

    fn redirect_follows(&self) -> bool {
        matches!(
            self.current,
            Token::Less
                | Token::Greater
                | Token::GreaterGreater
                | Token::LessLess
                | Token::LessGreater
                | Token::LessAmp
                | Token::GreaterAmp
        )
    }

    fn parse_redirect(&mut self) -> Result<Redirect, ParseError> {
        self.parse_redirect_with_fd(None)
    }

    fn parse_redirect_with_fd(&mut self, fd: Option<i32>) -> Result<Redirect, ParseError> {
        let kind = match self.current {
            Token::Less => RedirectKind::Read,
            Token::Greater => RedirectKind::Write,
            Token::GreaterGreater => RedirectKind::Append,
            Token::LessLess => RedirectKind::Here,
            Token::LessGreater => RedirectKind::ReadWrite,
            Token::LessAmp => RedirectKind::DupInput,
            Token::GreaterAmp => RedirectKind::DupOutput,
            _ => {
                return Err(ParseError(format!(
                    "expected redirection, found {:?}",
                    self.current
                )));
            }
        };
        self.bump();
        let target = self.word()?;
        Ok(Redirect { fd, kind, target })
    }

    // ------------------------------------------------------------------
    // Compound commands
    // ------------------------------------------------------------------

    fn parse_if(&mut self) -> Result<Command, ParseError> {
        self.bump(); // if
        let cond = Box::new(self.parse_list_until(&["then"])?);
        self.skip_newlines();
        self.expect_word("then")?;
        self.skip_newlines();
        let then_part = self.parse_compound_body(&["else", "elif", "fi"])?;
        let mut elifs = Vec::new();
        while self.word_is("elif") {
            self.bump();
            let elif_cond = self.parse_list_until(&["then"])?;
            self.skip_newlines();
            self.expect_word("then")?;
            self.skip_newlines();
            let elif_body = self.parse_compound_body(&["else", "elif", "fi"])?;
            elifs.push((elif_cond, elif_body));
        }
        let else_part = if self.word_is("else") {
            self.bump();
            self.skip_newlines();
            self.parse_compound_body(&["fi"])?
        } else {
            Vec::new()
        };
        self.expect_word("fi")?;
        Ok(Command::If {
            cond,
            then_part,
            elifs,
            else_part,
        })
    }

    fn parse_while(&mut self) -> Result<Command, ParseError> {
        self.bump(); // while
        let cond = Box::new(self.parse_list_until(&["do"])?);
        self.skip_newlines();
        self.expect_word("do")?;
        self.skip_newlines();
        let body = self.parse_compound_body(&["done"])?;
        self.expect_word("done")?;
        Ok(Command::While { cond, body })
    }

    fn parse_for(&mut self) -> Result<Command, ParseError> {
        self.bump(); // for
        let var = match &self.current {
            Token::Word(s) => {
                let v = s.clone();
                self.bump();
                v
            }
            _ => return Err(ParseError("expected variable name after 'for'".into())),
        };
        let words = if self.word_is("in") {
            self.bump();
            let mut ws = Vec::new();
            while !self.word_is("do") && !self.at_eof() && self.current != Token::Semi {
                ws.push(self.word()?);
            }
            ws
        } else {
            // Default to "$@"; represented as empty list.
            Vec::new()
        };
        if self.current == Token::Semi {
            self.bump();
        }
        self.skip_newlines();
        self.expect_word("do")?;
        self.skip_newlines();
        let body = self.parse_compound_body(&["done"])?;
        self.expect_word("done")?;
        Ok(Command::For { var, words, body })
    }

    fn parse_case(&mut self) -> Result<Command, ParseError> {
        self.bump(); // case
        let word = self.word()?;
        self.skip_newlines();
        self.expect_word("in")?;
        self.skip_newlines();
        let mut arms = Vec::new();
        while !self.word_is("esac") && !self.at_eof() {
            arms.push(self.parse_case_arm()?);
            self.skip_newlines();
        }
        self.expect_word("esac")?;
        Ok(Command::Case { word, arms })
    }

    fn parse_case_arm(&mut self) -> Result<CaseArm, ParseError> {
        let mut patterns = vec![self.word()?];
        while self.current == Token::Pipe {
            self.bump();
            patterns.push(self.word()?);
        }
        self.expect(Token::RParen)?;
        self.skip_newlines();
        let mut body = Vec::new();
        while !self.at_dsemi() && !self.word_is("esac") && !self.at_eof() {
            body.push(self.parse_list_until(&[";;", "esac"])?);
            self.skip_newlines();
        }
        self.consume_dsemicolon();
        self.skip_newlines();
        Ok(CaseArm { patterns, body })
    }

    fn consume_dsemicolon(&mut self) {
        if self.current == Token::Semi {
            self.bump();
            if self.current == Token::Semi {
                self.bump();
            }
        }
    }

    fn parse_group(&mut self) -> Result<Command, ParseError> {
        self.bump(); // {
        self.skip_newlines();
        let body = self.parse_compound_body(&["}"])?;
        self.expect(Token::RBrace)?;
        Ok(Command::Group(body))
    }

    fn parse_subshell(&mut self) -> Result<Command, ParseError> {
        self.bump(); // (
        self.skip_newlines();
        let body = self.parse_compound_body(&[")"])?;
        self.expect(Token::RParen)?;
        Ok(Command::Subshell(body))
    }

    fn parse_compound_body(&mut self, terminators: &[&str]) -> Result<Vec<Command>, ParseError> {
        let mut body = Vec::new();
        loop {
            self.skip_newlines();
            if self.at_eof() {
                break;
            }
            if self.at_terminator(terminators) {
                break;
            }
            body.push(self.parse_list_until(terminators)?);
            self.skip_newlines();
        }
        Ok(body)
    }

    // ------------------------------------------------------------------
    // Function definitions
    // ------------------------------------------------------------------

    fn parse_function_def(&mut self) -> Result<Command, ParseError> {
        self.bump(); // function
        let name = match &self.current {
            Token::Word(s) => {
                let n = s.clone();
                self.bump();
                n
            }
            _ => return Err(ParseError("expected function name".into())),
        };
        if self.current == Token::LParen {
            self.bump();
            self.expect(Token::RParen)?;
        }
        let body = Box::new(self.parse_command()?);
        Ok(Command::FunctionDef { name, body })
    }

    fn parse_function_def_with_name(&mut self, name: &str) -> Result<Command, ParseError> {
        let name = name.to_string();
        self.bump(); // name
        self.expect(Token::LParen)?;
        self.expect(Token::RParen)?;
        let body = Box::new(self.parse_command()?);
        Ok(Command::FunctionDef { name, body })
    }

    // ------------------------------------------------------------------
    // Word expectations
    // ------------------------------------------------------------------

    fn expect_word(&mut self, s: &str) -> Result<(), ParseError> {
        if self.word_is(s) {
            self.bump();
            Ok(())
        } else {
            Err(ParseError(format!(
                "expected '{}', found {:?}",
                s, self.current
            )))
        }
    }
}

fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple() {
        let mut p = Parser::new("echo hello").unwrap();
        let cmds = p.parse().unwrap();
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Command::Simple(s) => {
                assert_eq!(s.words.len(), 2);
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn parse_if() {
        let mut p = Parser::new("if true; then echo yes; fi").unwrap();
        let cmds = p.parse().unwrap();
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Command::If { .. }));
    }

    #[test]
    fn parse_for() {
        let mut p = Parser::new("for i in a b c; do echo $i; done").unwrap();
        let cmds = p.parse().unwrap();
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Command::For { .. }));
    }

    #[test]
    fn parse_function() {
        let mut p = Parser::new("f() { echo hi; }").unwrap();
        let cmds = p.parse().unwrap();
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Command::FunctionDef { .. }));
    }

    #[test]
    fn parse_case() {
        let mut p = Parser::new("case x in a) echo A ;; b) echo B ;; esac").unwrap();
        let cmds = p.parse().unwrap();
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Command::Case { .. }));
    }

    #[test]
    fn parse_redirect_with_fd() {
        let mut p = Parser::new("cmd 2>err.txt").unwrap();
        let cmds = p.parse().unwrap();
        match &cmds[0] {
            Command::Simple(s) => {
                assert_eq!(s.redirects.len(), 1);
                assert_eq!(s.redirects[0].fd, Some(2));
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn parse_assignment_as_argument_after_command() {
        // `export FOO=bar` must treat `FOO=bar` as an argument, not an assignment.
        let mut p = Parser::new("export FOO=bar").unwrap();
        let cmds = p.parse().unwrap();
        match &cmds[0] {
            Command::Simple(s) => {
                assert_eq!(s.assignments.len(), 0);
                assert_eq!(s.words.len(), 2);
                assert_eq!(s.words[0].value, "export");
                assert_eq!(s.words[1].value, "FOO=bar");
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn parse_leading_assignment() {
        // `FOO=bar echo hi` must treat `FOO=bar` as an environment assignment.
        let mut p = Parser::new("FOO=bar echo hi").unwrap();
        let cmds = p.parse().unwrap();
        match &cmds[0] {
            Command::Simple(s) => {
                assert_eq!(s.assignments.len(), 1);
                assert_eq!(s.assignments[0].0, "FOO");
                assert_eq!(s.assignments[0].1.value, "bar");
                assert_eq!(s.words.len(), 2);
                assert_eq!(s.words[0].value, "echo");
                assert_eq!(s.words[1].value, "hi");
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn parse_multiple_leading_assignments() {
        let mut p = Parser::new("A=1 B=2 cmd").unwrap();
        let cmds = p.parse().unwrap();
        match &cmds[0] {
            Command::Simple(s) => {
                assert_eq!(s.assignments.len(), 2);
                assert_eq!(s.assignments[0].0, "A");
                assert_eq!(s.assignments[0].1.value, "1");
                assert_eq!(s.assignments[1].0, "B");
                assert_eq!(s.assignments[1].1.value, "2");
                assert_eq!(s.words.len(), 1);
                assert_eq!(s.words[0].value, "cmd");
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn parse_assignment_as_argument() {
        // Any NAME=value after the command word is an argument.
        let mut p = Parser::new("cmd FOO=bar").unwrap();
        let cmds = p.parse().unwrap();
        match &cmds[0] {
            Command::Simple(s) => {
                assert_eq!(s.assignments.len(), 0);
                assert_eq!(s.words.len(), 2);
                assert_eq!(s.words[0].value, "cmd");
                assert_eq!(s.words[1].value, "FOO=bar");
            }
            _ => panic!("expected simple command"),
        }
    }
}
