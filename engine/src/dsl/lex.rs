//! Lexer for `.keds`. Produces a flat token stream that the parser walks.
//!
//! Lexical structure follows `04-dsl-grammar.md` §"Lexical structure":
//! line + block comments, decimal numbers (with `_` separators), strings,
//! time literals (`@…`), dB literals (`-3dB` / `-inf`), and punctuation.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Ident(String),
    String(String),
    Integer(i64),
    Float(f64),
    /// Anything starting with `@`. Stored as the raw text after the `@` so
    /// the parser can interpret each form (HMS, samples, sec, ms, keywords)
    /// in context.
    TimeLit(TimeForm),
    /// Dimension-suffixed numbers. `dB`, `ms`, `sec`, `samples`, `Hz`. The
    /// suffix is the textual identifier; the parser decides whether it's
    /// expected at this position.
    Suffixed { value: f64, suffix: String },
    /// `-inf` keyword used as a dB sentinel.
    NegInf,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Dash,
    LParen,
    RParen,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TimeForm {
    Samples(u64),
    Seconds(f64),
    Milliseconds(f64),
    Hms { h: u32, m: u32, s: f64 },
    Cursor,
    Start,
    End,
    SelectionIn,
    SelectionOut,
}

#[derive(Debug, Clone)]
pub struct Spanned {
    pub token: Token,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, PartialEq)]
pub struct LexError {
    pub line: u32,
    pub col: u32,
    pub message: String,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lex error at {}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for LexError {}

pub fn lex(input: &str) -> Result<Vec<Spanned>, LexError> {
    let mut lexer = Lexer::new(input);
    let mut out = Vec::new();
    loop {
        let s = lexer.next_token()?;
        let is_eof = matches!(s.token, Token::Eof);
        out.push(s);
        if is_eof {
            break;
        }
    }
    Ok(out)
}

struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn err(&self, message: impl Into<String>) -> LexError {
        LexError {
            line: self.line,
            col: self.col,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.input.get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    fn skip_ws_and_comments(&mut self) -> Result<(), LexError> {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => {
                    self.bump();
                }
                Some(b'#') => {
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    self.bump();
                    self.bump();
                    let start_line = self.line;
                    let start_col = self.col - 2;
                    loop {
                        match self.peek() {
                            None => {
                                return Err(LexError {
                                    line: start_line,
                                    col: start_col,
                                    message: "unterminated /* … */ comment".into(),
                                });
                            }
                            Some(b'*') if self.peek_at(1) == Some(b'/') => {
                                self.bump();
                                self.bump();
                                break;
                            }
                            _ => {
                                self.bump();
                            }
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn next_token(&mut self) -> Result<Spanned, LexError> {
        self.skip_ws_and_comments()?;
        let line = self.line;
        let col = self.col;
        let Some(b) = self.peek() else {
            return Ok(Spanned {
                token: Token::Eof,
                line,
                col,
            });
        };
        let token = match b {
            b'{' => {
                self.bump();
                Token::LBrace
            }
            b'}' => {
                self.bump();
                Token::RBrace
            }
            b':' => {
                self.bump();
                Token::Colon
            }
            b',' => {
                self.bump();
                Token::Comma
            }
            b'(' => {
                self.bump();
                Token::LParen
            }
            b')' => {
                self.bump();
                Token::RParen
            }
            b'-' => {
                // Could be a dash separator, a negative number, or `-inf`.
                // Look ahead for digits / "inf".
                if self.peek_at(1).is_some_and(|c| c.is_ascii_digit() || c == b'.') {
                    self.read_number()?
                } else if self.starts_with_inf(1) {
                    self.bump(); // consume '-'
                    self.consume_keyword(b"inf");
                    Token::NegInf
                } else {
                    self.bump();
                    Token::Dash
                }
            }
            b'+' if self
                .peek_at(1)
                .is_some_and(|c| c.is_ascii_digit() || c == b'.') =>
            {
                self.read_number()?
            }
            b'"' => self.read_string()?,
            b'@' => self.read_time_literal()?,
            b if b.is_ascii_digit() => self.read_number()?,
            b if is_ident_start(b) => self.read_ident_or_keyword()?,
            _ => {
                return Err(self.err(format!("unexpected character `{}`", b as char)));
            }
        };
        Ok(Spanned { token, line, col })
    }

    fn starts_with_inf(&self, offset: usize) -> bool {
        self.input
            .get(self.pos + offset..self.pos + offset + 3)
            == Some(b"inf")
            && !self
                .input
                .get(self.pos + offset + 3)
                .copied()
                .is_some_and(is_ident_continue)
    }

    fn consume_keyword(&mut self, kw: &[u8]) {
        for _ in 0..kw.len() {
            self.bump();
        }
    }

    fn read_string(&mut self) -> Result<Token, LexError> {
        self.bump(); // opening quote
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string literal")),
                Some(b'"') => {
                    self.bump();
                    return Ok(Token::String(out));
                }
                Some(b'\\') => {
                    self.bump();
                    let esc = self
                        .bump()
                        .ok_or_else(|| self.err("escape past end of input"))?;
                    let c = match esc {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'n' => '\n',
                        b't' => '\t',
                        other => return Err(self.err(format!("bad escape `\\{}`", other as char))),
                    };
                    out.push(c);
                }
                Some(b) => {
                    out.push(b as char);
                    self.bump();
                }
            }
        }
    }

    fn read_number(&mut self) -> Result<Token, LexError> {
        let start = self.pos;
        let mut saw_dot = false;
        if matches!(self.peek(), Some(b'+') | Some(b'-')) {
            self.bump();
        }
        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' | b'_' => {
                    self.bump();
                }
                b'.' if !saw_dot
                    && self
                        .peek_at(1)
                        .is_some_and(|c| c.is_ascii_digit()) =>
                {
                    saw_dot = true;
                    self.bump();
                }
                _ => break,
            }
        }
        let raw: String = self.input[start..self.pos]
            .iter()
            .filter(|&&b| b != b'_')
            .map(|&b| b as char)
            .collect();
        // Optional dimension suffix immediately following the number
        // (no whitespace).
        let suffix = self.read_optional_suffix();
        if let Some(sfx) = suffix {
            let value: f64 = raw
                .parse()
                .map_err(|_| self.err(format!("invalid number `{raw}`")))?;
            Ok(Token::Suffixed { value, suffix: sfx })
        } else if saw_dot {
            let value: f64 = raw
                .parse()
                .map_err(|_| self.err(format!("invalid float `{raw}`")))?;
            Ok(Token::Float(value))
        } else {
            let value: i64 = raw
                .parse()
                .map_err(|_| self.err(format!("invalid integer `{raw}`")))?;
            Ok(Token::Integer(value))
        }
    }

    fn read_optional_suffix(&mut self) -> Option<String> {
        if !self.peek().is_some_and(is_suffix_start) {
            return None;
        }
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_ident_continue(b) {
                self.bump();
            } else {
                break;
            }
        }
        let s: String = self.input[start..self.pos]
            .iter()
            .map(|&b| b as char)
            .collect();
        Some(s)
    }

    fn read_ident_or_keyword(&mut self) -> Result<Token, LexError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_ident_continue(b) {
                self.bump();
            } else {
                break;
            }
        }
        let raw: String = self.input[start..self.pos]
            .iter()
            .map(|&b| b as char)
            .collect();
        // Handle `-inf` already; bare `inf` alone isn't a keyword in our
        // grammar (positive infinity has no dB representation).
        Ok(Token::Ident(raw))
    }

    fn read_time_literal(&mut self) -> Result<Token, LexError> {
        self.bump(); // '@'
        // Either a keyword identifier (cursor, start, end, selection.in,
        // selection.out) or a numeric form (HMS, samples, sec, ms).
        match self.peek() {
            Some(b) if b.is_ascii_alphabetic() => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if is_ident_continue(c) || c == b'.' {
                        self.bump();
                    } else {
                        break;
                    }
                }
                let raw: String = self.input[start..self.pos]
                    .iter()
                    .map(|&b| b as char)
                    .collect();
                let form = match raw.as_str() {
                    "cursor" => TimeForm::Cursor,
                    "start" => TimeForm::Start,
                    "end" => TimeForm::End,
                    "selection.in" => TimeForm::SelectionIn,
                    "selection.out" => TimeForm::SelectionOut,
                    other => {
                        return Err(self.err(format!("unknown time keyword `@{other}`")));
                    }
                };
                Ok(Token::TimeLit(form))
            }
            _ => {
                // Numeric: HH:MM:SS.sss, or N{s,sec,ms}
                let start = self.pos;
                while let Some(b) = self.peek() {
                    match b {
                        b'0'..=b'9' | b'_' | b'.' | b':' => {
                            self.bump();
                        }
                        _ => break,
                    }
                }
                let raw: String = self.input[start..self.pos]
                    .iter()
                    .filter(|&&b| b != b'_')
                    .map(|&b| b as char)
                    .collect();
                let suffix = self.read_optional_suffix();

                if raw.contains(':') {
                    // HMS form
                    let parts: Vec<&str> = raw.split(':').collect();
                    if parts.len() != 3 {
                        return Err(self.err(format!("bad HMS literal `@{raw}`")));
                    }
                    let h: u32 = parts[0]
                        .parse()
                        .map_err(|_| self.err("bad hour"))?;
                    let m: u32 = parts[1]
                        .parse()
                        .map_err(|_| self.err("bad minute"))?;
                    let s: f64 = parts[2]
                        .parse()
                        .map_err(|_| self.err("bad seconds"))?;
                    return Ok(Token::TimeLit(TimeForm::Hms { h, m, s }));
                }

                let value: f64 = raw
                    .parse()
                    .map_err(|_| self.err(format!("bad numeric time `@{raw}`")))?;
                let form = match suffix.as_deref() {
                    Some("s") | Some("samples") => TimeForm::Samples(value as u64),
                    Some("sec") => TimeForm::Seconds(value),
                    Some("ms") => TimeForm::Milliseconds(value),
                    Some(other) => {
                        return Err(self.err(format!("unknown time suffix `{other}`")));
                    }
                    None => return Err(self.err("numeric time literal needs a suffix")),
                };
                Ok(Token::TimeLit(form))
            }
        }
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_suffix_start(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first(input: &str) -> Token {
        lex(input).unwrap().into_iter().next().unwrap().token
    }

    #[test]
    fn lexes_punctuation() {
        let tokens: Vec<Token> = lex("{ }: , -").unwrap().into_iter().map(|s| s.token).collect();
        assert_eq!(
            tokens,
            vec![
                Token::LBrace,
                Token::RBrace,
                Token::Colon,
                Token::Comma,
                Token::Dash,
                Token::Eof
            ]
        );
    }

    #[test]
    fn skips_line_and_block_comments() {
        let t: Vec<Token> = lex("# comment\n  /* multi\nline */ 42")
            .unwrap()
            .into_iter()
            .map(|s| s.token)
            .collect();
        assert_eq!(t, vec![Token::Integer(42), Token::Eof]);
    }

    #[test]
    fn integer_with_underscores_drops_them() {
        assert_eq!(first("96_000"), Token::Integer(96_000));
    }

    #[test]
    fn float_literal() {
        assert_eq!(first("1.5"), Token::Float(1.5));
    }

    #[test]
    fn db_suffix_yields_suffixed_token() {
        assert_eq!(
            first("-3dB"),
            Token::Suffixed {
                value: -3.0,
                suffix: "dB".into()
            }
        );
        assert_eq!(
            first("+6dB"),
            Token::Suffixed {
                value: 6.0,
                suffix: "dB".into()
            }
        );
        assert_eq!(
            first("0dB"),
            Token::Suffixed {
                value: 0.0,
                suffix: "dB".into()
            }
        );
    }

    #[test]
    fn neg_inf_keyword() {
        assert_eq!(first("-inf"), Token::NegInf);
    }

    #[test]
    fn duration_with_ms_suffix() {
        assert_eq!(
            first("250ms"),
            Token::Suffixed {
                value: 250.0,
                suffix: "ms".into()
            }
        );
    }

    #[test]
    fn time_literal_hms_form() {
        assert_eq!(
            first("@00:01:23.456"),
            Token::TimeLit(TimeForm::Hms {
                h: 0,
                m: 1,
                s: 23.456
            })
        );
    }

    #[test]
    fn time_literal_keyword_forms() {
        assert_eq!(first("@cursor"), Token::TimeLit(TimeForm::Cursor));
        assert_eq!(first("@start"), Token::TimeLit(TimeForm::Start));
        assert_eq!(
            first("@selection.out"),
            Token::TimeLit(TimeForm::SelectionOut)
        );
    }

    #[test]
    fn time_literal_numeric_with_suffix() {
        assert_eq!(first("@1.5sec"), Token::TimeLit(TimeForm::Seconds(1.5)));
        assert_eq!(first("@250ms"), Token::TimeLit(TimeForm::Milliseconds(250.0)));
        assert_eq!(first("@123s"), Token::TimeLit(TimeForm::Samples(123)));
    }

    #[test]
    fn string_with_escapes() {
        assert_eq!(
            first("\"a\\\"b\\nc\""),
            Token::String("a\"b\nc".into())
        );
    }

    #[test]
    fn ident_after_at_keyword_can_have_dots() {
        // selection.in / selection.out cases.
        assert_eq!(
            first("@selection.in"),
            Token::TimeLit(TimeForm::SelectionIn)
        );
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(lex("\"oops").is_err());
    }

    #[test]
    fn unterminated_block_comment_errors() {
        assert!(lex("/* hi").is_err());
    }
}
