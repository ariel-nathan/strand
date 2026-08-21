//! Hand-written lexer (§4.6).

use std::fmt;

use crate::diag::Diagnostic;

/// Byte offsets into the source, plus a line/column for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub col: u32,
}

impl Span {
    fn new(start: usize, end: usize, line: u32, col: u32) -> Self {
        Self { start, end, line, col }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Int(i64),
    Float(f64),
    Str(String),
    Ident(String),

    // Keywords. `Ok`/`Err`/`Some`/`None` are ordinary identifiers, resolved as
    // constructors by the checker rather than reserved here.
    Fn,
    Let,
    Var,
    If,
    Else,
    Return,
    Match,
    Type,
    True,
    False,
    Actor,
    View,
    Scope,
    Spawn,
    For,
    In,

    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Semi,
    Dot,
    Arrow,
    FatArrow,
    Question,

    Eq,
    EqEq,
    BangEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Pipe,
    AmpAmp,
    PipePipe,

    Eof,
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Tok::Int(v) => return write!(f, "int literal `{v}`"),
            Tok::Float(v) => return write!(f, "float literal `{v}`"),
            Tok::Str(v) => return write!(f, "string literal {v:?}"),
            Tok::Ident(v) => return write!(f, "`{v}`"),
            Tok::Fn => "fn",
            Tok::Let => "let",
            Tok::Var => "var",
            Tok::If => "if",
            Tok::Else => "else",
            Tok::Return => "return",
            Tok::Match => "match",
            Tok::Type => "type",
            Tok::True => "true",
            Tok::False => "false",
            Tok::Actor => "actor",
            Tok::View => "view",
            Tok::Scope => "scope",
            Tok::Spawn => "spawn",
            Tok::For => "for",
            Tok::In => "in",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::Comma => ",",
            Tok::Colon => ":",
            Tok::Semi => ";",
            Tok::Dot => ".",
            Tok::Arrow => "->",
            Tok::FatArrow => "=>",
            Tok::Question => "?",
            Tok::Eq => "=",
            Tok::EqEq => "==",
            Tok::BangEq => "!=",
            Tok::Lt => "<",
            Tok::LtEq => "<=",
            Tok::Gt => ">",
            Tok::GtEq => ">=",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::Bang => "!",
            Tok::Pipe => "|",
            Tok::AmpAmp => "&&",
            Tok::PipePipe => "||",
            Tok::Eof => "end of file",
        };
        write!(f, "`{s}`")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

pub fn lex(src: &str) -> Result<Vec<Token>, Diagnostic> {
    Lexer::new(src).run()
}

/// Lexes the whole input, stepping over bad bytes instead of stopping at the
/// first one.
///
/// `lex` keeps its stop-at-the-first-error contract, which is what a batch
/// compile wants. An editor asks on every keystroke, when the buffer is usually
/// mid-edit and briefly invalid, and one stray character must not blank out the
/// rest of the file.
pub fn lex_recovering(src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    Lexer::new(src).run_recovering()
}

/// Past this many lexical errors the file is garbage rather than mid-edit —
/// a binary opened by mistake, say. Lexing continues so the token stream stays
/// whole; only the reporting stops.
const MAX_LEX_ERRORS: usize = 100;

struct Lexer<'src> {
    bytes: &'src [u8],
    text: &'src str,
    pos: usize,
    line: u32,
    col: u32,
}

impl<'src> Lexer<'src> {
    fn new(text: &'src str) -> Self {
        Self { bytes: text.as_bytes(), text, pos: 0, line: 1, col: 1 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.pos + n).copied()
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

    fn error<T>(&self, message: impl Into<String>) -> Result<T, Diagnostic> {
        self.error_at(self.pos, self.line, self.col, message)
    }

    /// Reports at a saved position. Needed wherever the offending character has
    /// already been consumed, so the caret lands on it rather than past it.
    fn error_at<T>(
        &self,
        start: usize,
        line: u32,
        col: u32,
        message: impl Into<String>,
    ) -> Result<T, Diagnostic> {
        let span = Span { start, end: start + 1, line, col };
        Err(Diagnostic::new(span, message))
    }

    /// `run`, but a failed scanner records the problem and the loop carries on
    /// from the next byte rather than returning.
    fn run_recovering(mut self) -> (Vec<Token>, Vec<Diagnostic>) {
        let mut out = Vec::new();
        let mut errors: Vec<Diagnostic> = Vec::new();
        loop {
            self.skip_trivia();
            let (line, col, start) = (self.line, self.col, self.pos);
            let Some(b) = self.peek() else {
                out.push(Token { tok: Tok::Eof, span: Span::new(start, start, line, col) });
                return (out, errors);
            };

            let scanned = match b {
                b'0'..=b'9' => self.number(),
                b'"' => self.string(),
                b if b.is_ascii_alphabetic() || b == b'_' => Ok(self.ident_or_keyword()),
                _ => self.punctuation(),
            };

            match scanned {
                Ok(tok) => out.push(Token { tok, span: Span::new(start, self.pos, line, col) }),
                Err(diagnostic) => {
                    if errors.len() < MAX_LEX_ERRORS {
                        errors.push(diagnostic);
                    }
                    // Every scanner consumes before it fails, but a future one
                    // might not, and standing still here would spin forever.
                    if self.pos == start {
                        self.bump();
                    }
                }
            }
        }
    }

    fn run(mut self) -> Result<Vec<Token>, Diagnostic> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia();
            let (line, col, start) = (self.line, self.col, self.pos);
            let Some(b) = self.peek() else {
                out.push(Token { tok: Tok::Eof, span: Span::new(start, start, line, col) });
                return Ok(out);
            };

            let tok = match b {
                b'0'..=b'9' => self.number()?,
                b'"' => self.string()?,
                b if b.is_ascii_alphabetic() || b == b'_' => self.ident_or_keyword(),
                _ => self.punctuation()?,
            };
            out.push(Token { tok, span: Span::new(start, self.pos, line, col) });
        }
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => {
                    self.bump();
                }
                // Line comments only; no block comments in the POC.
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    fn ident_or_keyword(&mut self) -> Tok {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.bump();
            } else {
                break;
            }
        }
        match &self.text[start..self.pos] {
            "fn" => Tok::Fn,
            "let" => Tok::Let,
            "var" => Tok::Var,
            "if" => Tok::If,
            "else" => Tok::Else,
            "return" => Tok::Return,
            "match" => Tok::Match,
            "type" => Tok::Type,
            "true" => Tok::True,
            "false" => Tok::False,
            "actor" => Tok::Actor,
            "view" => Tok::View,
            "scope" => Tok::Scope,
            "spawn" => Tok::Spawn,
            "for" => Tok::For,
            "in" => Tok::In,
            word => Tok::Ident(word.to_string()),
        }
    }

    fn number(&mut self) -> Result<Tok, Diagnostic> {
        let start = self.pos;
        while matches!(self.peek(), Some(b) if b.is_ascii_digit() || b == b'_') {
            self.bump();
        }

        // A dot only joins the number when a digit follows, so `1.max(2)` still
        // lexes as int, dot, ident.
        let is_float =
            self.peek() == Some(b'.') && matches!(self.peek_at(1), Some(d) if d.is_ascii_digit());
        if is_float {
            self.bump();
            while matches!(self.peek(), Some(b) if b.is_ascii_digit() || b == b'_') {
                self.bump();
            }
        }

        let raw: String = self.text[start..self.pos].chars().filter(|c| *c != '_').collect();
        if is_float {
            match raw.parse::<f64>() {
                Ok(v) => Ok(Tok::Float(v)),
                Err(_) => self.error(format!("invalid float literal `{raw}`")),
            }
        } else {
            match raw.parse::<i64>() {
                Ok(v) => Ok(Tok::Int(v)),
                Err(_) => self.error(format!("integer literal `{raw}` does not fit in an int")),
            }
        }
    }

    fn string(&mut self) -> Result<Tok, Diagnostic> {
        self.bump(); // opening quote
        let mut out = String::new();
        loop {
            let Some(b) = self.bump() else {
                return self.error("unterminated string literal");
            };
            match b {
                b'"' => return Ok(Tok::Str(out)),
                b'\n' => return self.error("newline in string literal"),
                b'\\' => {
                    let Some(esc) = self.bump() else {
                        return self.error("unterminated escape sequence");
                    };
                    match esc {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'\\' => out.push('\\'),
                        b'"' => out.push('"'),
                        other => {
                            return self.error(format!("unknown escape `\\{}`", other as char))
                        }
                    }
                }
                _ => {
                    // `bump` walks bytes but the source is text, so re-decode
                    // the full character and skip its continuation bytes.
                    let ch = self.text[self.pos - 1..].chars().next().unwrap();
                    for _ in 1..ch.len_utf8() {
                        self.bump();
                    }
                    out.push(ch);
                }
            }
        }
    }

    fn punctuation(&mut self) -> Result<Tok, Diagnostic> {
        let (line, col) = (self.line, self.col);
        let b = self.bump().expect("caller checked there is a byte");

        // `next` present => two-char token, else the one-char fallback.
        macro_rules! or_single {
            ($next:expr, $both:expr, $single:expr) => {
                if self.peek() == Some($next) {
                    self.bump();
                    $both
                } else {
                    $single
                }
            };
        }

        Ok(match b {
            b'(' => Tok::LParen,
            b')' => Tok::RParen,
            b'{' => Tok::LBrace,
            b'}' => Tok::RBrace,
            b'[' => Tok::LBracket,
            b']' => Tok::RBracket,
            b',' => Tok::Comma,
            b':' => Tok::Colon,
            b';' => Tok::Semi,
            b'.' => Tok::Dot,
            b'?' => Tok::Question,
            b'+' => Tok::Plus,
            b'*' => Tok::Star,
            b'/' => Tok::Slash,
            b'%' => Tok::Percent,
            b'-' => or_single!(b'>', Tok::Arrow, Tok::Minus),
            b'!' => or_single!(b'=', Tok::BangEq, Tok::Bang),
            b'<' => or_single!(b'=', Tok::LtEq, Tok::Lt),
            b'>' => or_single!(b'=', Tok::GtEq, Tok::Gt),
            b'|' => or_single!(b'|', Tok::PipePipe, Tok::Pipe),
            b'=' => {
                if self.peek() == Some(b'>') {
                    self.bump();
                    Tok::FatArrow
                } else {
                    or_single!(b'=', Tok::EqEq, Tok::Eq)
                }
            }
            b'&' => {
                if self.peek() == Some(b'&') {
                    self.bump();
                    Tok::AmpAmp
                } else {
                    // §4.2 has no bitwise operators, so a lone `&` is always a typo.
                    return self
                        .error_at(self.pos - 1, line, col, "unexpected `&`")
                        .map_err(|d| d.with_help("Strand has no bitwise operators; use `&&`"));
                }
            }
            other => {
                let ch = if other.is_ascii() {
                    (other as char).to_string()
                } else {
                    self.text[self.pos - 1..].chars().next().unwrap().to_string()
                };
                return self.error_at(self.pos - 1, line, col, format!("unexpected character `{ch}`"));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).expect("lex failed").into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn lexes_a_function_signature() {
        assert_eq!(
            toks("fn addTodo(title: string): int { 1 }"),
            vec![
                Tok::Fn,
                Tok::Ident("addTodo".into()),
                Tok::LParen,
                Tok::Ident("title".into()),
                Tok::Colon,
                Tok::Ident("string".into()),
                Tok::RParen,
                Tok::Colon,
                Tok::Ident("int".into()),
                Tok::LBrace,
                Tok::Int(1),
                Tok::RBrace,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn distinguishes_one_and_two_char_operators() {
        assert_eq!(
            toks("= == != < <= > >= -> => | || && ? !"),
            vec![
                Tok::Eq,
                Tok::EqEq,
                Tok::BangEq,
                Tok::Lt,
                Tok::LtEq,
                Tok::Gt,
                Tok::GtEq,
                Tok::Arrow,
                Tok::FatArrow,
                Tok::Pipe,
                Tok::PipePipe,
                Tok::AmpAmp,
                Tok::Question,
                Tok::Bang,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn floats_need_a_digit_after_the_dot() {
        assert_eq!(toks("1.5"), vec![Tok::Float(1.5), Tok::Eof]);
        // Method call on an int literal must not swallow the dot.
        assert_eq!(
            toks("1.max(2)"),
            vec![
                Tok::Int(1),
                Tok::Dot,
                Tok::Ident("max".into()),
                Tok::LParen,
                Tok::Int(2),
                Tok::RParen,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn underscores_separate_digits() {
        assert_eq!(toks("1_000_000"), vec![Tok::Int(1_000_000), Tok::Eof]);
    }

    #[test]
    fn strings_handle_escapes_and_unicode() {
        assert_eq!(toks(r#""a\nb""#), vec![Tok::Str("a\nb".into()), Tok::Eof]);
        assert_eq!(toks(r#""quote:\"""#), vec![Tok::Str("quote:\"".into()), Tok::Eof]);
        assert_eq!(toks(r#""héllo→""#), vec![Tok::Str("héllo→".into()), Tok::Eof]);
    }

    #[test]
    fn line_comments_are_trivia() {
        assert_eq!(toks("1 // trailing\n2"), vec![Tok::Int(1), Tok::Int(2), Tok::Eof]);
    }

    #[test]
    fn keywords_are_not_identifiers() {
        assert_eq!(toks("match"), vec![Tok::Match, Tok::Eof]);
        // Constructors stay identifiers; the checker gives them meaning.
        assert_eq!(toks("Ok"), vec![Tok::Ident("Ok".into()), Tok::Eof]);
    }

    #[test]
    fn reports_position_of_bad_input() {
        let err = lex("let a = 1\nlet b = #").unwrap_err();
        assert_eq!((err.line(), err.col()), (2, 9));
        assert!(err.message.contains('#'), "message was: {}", err.message);
    }

    #[test]
    fn rejects_unterminated_string() {
        assert!(lex("\"abc").unwrap_err().message.contains("unterminated"));
    }

    #[test]
    fn spans_point_at_the_token() {
        let tokens = lex("fn  main").unwrap();
        assert_eq!((tokens[0].span.start, tokens[0].span.end), (0, 2));
        assert_eq!((tokens[1].span.start, tokens[1].span.end), (4, 8));
        assert_eq!(tokens[1].span.col, 5);
    }
}
