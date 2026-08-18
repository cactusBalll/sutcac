//! Arithmetic expression parser and evaluator.
//!
//! Supports integer arithmetic with bash-like precedence:
//! ternary, ||, &&, |, ^, &, ==/!=, comparisons, shifts, +/-, */%, **,
//! unary +/-!~, variables, parentheses, and assignment operators.

use std::collections::HashMap;

pub fn evaluate(expr: &str, vars: &mut HashMap<String, String>) -> Result<i64, String> {
    let mut p = Parser::new(expr, vars)?;
    let value = p.parse_expr()?;
    p.skip_ws();
    if p.peek().is_some() {
        return Err(format!("unexpected '{}' in arithmetic", p.peek().unwrap()));
    }
    Ok(value)
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    vars: &'a mut HashMap<String, String>,
}

impl<'a> Parser<'a> {
    fn new(expr: &'a str, vars: &'a mut HashMap<String, String>) -> Result<Self, String> {
        Ok(Self {
            chars: expr.chars().peekable(),
            vars,
        })
    }

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    fn advance(&mut self) -> Option<char> {
        self.chars.next()
    }

    fn skip_ws(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), String> {
        match self.advance() {
            Some(c) if c == expected => Ok(()),
            Some(c) => Err(format!("expected '{}', found '{}'", expected, c)),
            None => Err(format!("expected '{}', found end of expression", expected)),
        }
    }

    fn parse_expr(&mut self) -> Result<i64, String> {
        self.parse_ternary()
    }

    fn parse_ternary(&mut self) -> Result<i64, String> {
        let cond = self.parse_or()?;
        self.skip_ws();
        if self.peek() == Some('?') {
            self.advance();
            let true_val = self.parse_expr()?;
            self.skip_ws();
            self.expect(':')?;
            let false_val = self.parse_ternary()?;
            Ok(if cond != 0 { true_val } else { false_val })
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self) -> Result<i64, String> {
        let mut left = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('|') && self.peek_next() != Some('|') {
                self.advance();
                let right = self.parse_and()?;
                left = if left != 0 || right != 0 { 1 } else { 0 };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<i64, String> {
        let mut left = self.parse_bw_or()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('&') && self.peek_next() != Some('&') {
                self.advance();
                let right = self.parse_bw_or()?;
                left = if left != 0 && right != 0 { 1 } else { 0 };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_bw_or(&mut self) -> Result<i64, String> {
        let mut left = self.parse_bw_xor()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('|') && self.peek_next() == Some('|') {
                self.advance();
                self.advance();
                let right = self.parse_bw_xor()?;
                left = if left != 0 || right != 0 { 1 } else { 0 };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_bw_xor(&mut self) -> Result<i64, String> {
        let mut left = self.parse_bw_and()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('^') {
                self.advance();
                let right = self.parse_bw_and()?;
                left = left ^ right;
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_bw_and(&mut self) -> Result<i64, String> {
        let mut left = self.parse_eq()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('&') && self.peek_next() == Some('&') {
                self.advance();
                self.advance();
                let right = self.parse_eq()?;
                left = if left != 0 && right != 0 { 1 } else { 0 };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_eq(&mut self) -> Result<i64, String> {
        let mut left = self.parse_rel()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('=') && self.peek_next() == Some('=') {
                self.advance();
                self.advance();
                let right = self.parse_rel()?;
                left = if left == right { 1 } else { 0 };
            } else if self.peek() == Some('!') && self.peek_next() == Some('=') {
                self.advance();
                self.advance();
                let right = self.parse_rel()?;
                left = if left != right { 1 } else { 0 };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_rel(&mut self) -> Result<i64, String> {
        let mut left = self.parse_shift()?;
        loop {
            self.skip_ws();
            let op = match self.peek() {
                Some('<') if self.peek_next() == Some('=') => 'l', // <=
                Some('>') if self.peek_next() == Some('=') => 'g', // >=
                Some('<') if self.peek_next() != Some('<') => '<',
                Some('>') if self.peek_next() != Some('>') => '>',
                _ => break,
            };
            self.advance();
            if op == 'l' || op == 'g' {
                self.advance();
            }
            let right = self.parse_shift()?;
            left = match op {
                '<' => {
                    if left < right {
                        1
                    } else {
                        0
                    }
                }
                '>' => {
                    if left > right {
                        1
                    } else {
                        0
                    }
                }
                'l' => {
                    if left <= right {
                        1
                    } else {
                        0
                    }
                }
                'g' => {
                    if left >= right {
                        1
                    } else {
                        0
                    }
                }
                _ => unreachable!(),
            };
        }
        Ok(left)
    }

    fn parse_shift(&mut self) -> Result<i64, String> {
        let mut left = self.parse_add()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('<') && self.peek_next() == Some('<') {
                self.advance();
                self.advance();
                let right = self.parse_add()?;
                left = left << right;
            } else if self.peek() == Some('>') && self.peek_next() == Some('>') {
                self.advance();
                self.advance();
                let right = self.parse_add()?;
                left = left >> right;
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_add(&mut self) -> Result<i64, String> {
        let mut left = self.parse_mul()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => {
                    self.advance();
                    let right = self.parse_mul()?;
                    left = left.wrapping_add(right);
                }
                Some('-') if self.peek_next() != Some('-') => {
                    self.advance();
                    let right = self.parse_mul()?;
                    left = left.wrapping_sub(right);
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_mul(&mut self) -> Result<i64, String> {
        let mut left = self.parse_pow()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('*') if self.peek_next() != Some('*') => {
                    self.advance();
                    let right = self.parse_pow()?;
                    left = left.wrapping_mul(right);
                }
                Some('/') => {
                    self.advance();
                    let right = self.parse_pow()?;
                    if right == 0 {
                        return Err("division by zero".into());
                    }
                    left = left.wrapping_div(right);
                }
                Some('%') => {
                    self.advance();
                    let right = self.parse_pow()?;
                    if right == 0 {
                        return Err("division by zero".into());
                    }
                    left = left.wrapping_rem(right);
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_pow(&mut self) -> Result<i64, String> {
        let base = self.parse_unary()?;
        self.skip_ws();
        if self.peek() == Some('*') && self.peek_next() == Some('*') {
            self.advance();
            self.advance();
            let exp = self.parse_pow()?;
            Ok(pow_i64(base, exp))
        } else {
            Ok(base)
        }
    }

    fn parse_unary(&mut self) -> Result<i64, String> {
        self.skip_ws();
        match self.peek() {
            Some('+') => {
                self.advance();
                self.parse_unary()
            }
            Some('-') => {
                self.advance();
                Ok(-self.parse_unary()?)
            }
            Some('!') => {
                self.advance();
                Ok(if self.parse_unary()? == 0 { 1 } else { 0 })
            }
            Some('~') => {
                self.advance();
                Ok(!self.parse_unary()?)
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<i64, String> {
        self.skip_ws();
        match self.peek() {
            Some('(') => {
                self.advance();
                let val = self.parse_expr()?;
                self.skip_ws();
                self.expect(')')?;
                Ok(val)
            }
            Some('$') => {
                self.advance();
                let name = self.read_name()?;
                Ok(self.get_var(&name))
            }
            Some(c) if c.is_ascii_digit() => self.read_number(),
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let name = self.read_name()?;
                self.skip_ws();
                // Assignment operators
                let op = match self.peek() {
                    Some('=') if self.peek_next() != Some('=') => Some('='),
                    Some('+') if self.peek_next() == Some('=') => Some('+'),
                    Some('-') if self.peek_next() == Some('=') => Some('-'),
                    Some('*') if self.peek_next() == Some('=') => Some('*'),
                    Some('/') if self.peek_next() == Some('=') => Some('/'),
                    Some('%') if self.peek_next() == Some('=') => Some('%'),
                    _ => None,
                };
                if let Some(op) = op {
                    self.advance();
                    if op != '=' {
                        self.advance();
                    }
                    let val = self.parse_expr()?;
                    let old = self.get_var(&name);
                    let new = match op {
                        '=' => val,
                        '+' => old.wrapping_add(val),
                        '-' => old.wrapping_sub(val),
                        '*' => old.wrapping_mul(val),
                        '/' => {
                            if val == 0 {
                                return Err("division by zero".into());
                            }
                            old.wrapping_div(val)
                        }
                        '%' => {
                            if val == 0 {
                                return Err("division by zero".into());
                            }
                            old.wrapping_rem(val)
                        }
                        _ => unreachable!(),
                    };
                    self.set_var(&name, new);
                    Ok(new)
                } else {
                    Ok(self.get_var(&name))
                }
            }
            Some(c) => Err(format!("unexpected '{}' in arithmetic", c)),
            None => Err("unexpected end of arithmetic expression".into()),
        }
    }

    fn read_number(&mut self) -> Result<i64, String> {
        let mut val: i64 = 0;
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() {
                self.advance();
                val = val
                    .checked_mul(10)
                    .and_then(|v| v.checked_add((c as u8 - b'0') as i64))
                    .ok_or("integer overflow")?;
            } else {
                break;
            }
        }
        Ok(val)
    }

    fn read_name(&mut self) -> Result<String, String> {
        let mut name = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                name.push(c);
                self.advance();
            } else {
                break;
            }
        }
        if name.is_empty() {
            Err("expected variable name".into())
        } else {
            Ok(name)
        }
    }

    fn peek_next(&mut self) -> Option<char> {
        let mut it = self.chars.clone();
        it.next();
        it.peek().copied()
    }

    fn get_var(&self, name: &str) -> i64 {
        self.vars
            .get(name)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    fn set_var(&mut self, name: &str, value: i64) {
        self.vars.insert(name.to_string(), value.to_string());
    }
}

fn pow_i64(base: i64, exp: i64) -> i64 {
    if exp < 0 {
        return 0;
    }
    let mut result = 1i64;
    let mut b = base;
    let mut e = exp;
    while e > 0 {
        if e & 1 == 1 {
            result = result.wrapping_mul(b);
        }
        b = b.wrapping_mul(b);
        e >>= 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> i64 {
        let mut vars = HashMap::new();
        evaluate(expr, &mut vars).unwrap()
    }

    #[test]
    fn basic_arith() {
        assert_eq!(eval("2+3*4"), 14);
        assert_eq!(eval("(2+3)*4"), 20);
        assert_eq!(eval("10/3"), 3);
        assert_eq!(eval("10%3"), 1);
    }

    #[test]
    fn comparisons() {
        assert_eq!(eval("5 == 5"), 1);
        assert_eq!(eval("5 != 3"), 1);
        assert_eq!(eval("5 < 3"), 0);
        assert_eq!(eval("5 >= 3"), 1);
    }

    #[test]
    fn logical() {
        assert_eq!(eval("1 && 0"), 0);
        assert_eq!(eval("1 || 0"), 1);
        assert_eq!(eval("!0"), 1);
    }

    #[test]
    fn assignment() {
        let mut vars = HashMap::new();
        assert_eq!(evaluate("x=5", &mut vars).unwrap(), 5);
        assert_eq!(evaluate("x+=3", &mut vars).unwrap(), 8);
        assert_eq!(evaluate("x*2", &mut vars).unwrap(), 16);
    }
}
