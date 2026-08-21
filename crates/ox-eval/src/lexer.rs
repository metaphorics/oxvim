//! Byte-oriented lexer for legacy Vimscript expressions.
//!
//! The lexer never decodes source text as UTF-8.  Every [`Span`] is expressed
//! in byte offsets into the original input and string tokens retain arbitrary
//! bytes.

use crate::error::EvalError;

/// A half-open byte range in the expression source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Span {
    /// First byte belonging to the syntax item.
    pub start: usize,
    /// First byte after the syntax item.
    pub end: usize,
}

impl Span {
    /// Construct a half-open source span.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Return a span covering both input spans.
    #[must_use]
    pub const fn through(self, other: Self) -> Self {
        Self { start: self.start, end: other.end }
    }
}

/// A comparison operator's explicit case-selection suffix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaseSensitivity {
    /// Respect the current `'ignorecase'` setting.
    Default,
    /// The `#` suffix: compare case-sensitively.
    MatchCase,
    /// The `?` suffix: compare case-insensitively.
    IgnoreCase,
}

/// Tokens accepted by the legacy expression parser.
#[allow(missing_docs)]
#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    /// End of input.
    Eof,
    /// A signed 64-bit integer literal (the sign is a separate token).
    Integer(i64),
    /// An IEEE-754 floating-point literal.
    Float(f64),
    /// A decoded single- or double-quoted byte string.
    String(Vec<u8>),
    /// A decoded `0z` hexadecimal blob.
    Blob(Vec<u8>),
    /// An internal variable or function name.
    Identifier(Vec<u8>),
    /// `$NAME` without the leading dollar sign.
    Environment(Vec<u8>),
    /// `&name`, `&g:name`, or `&l:name`.
    Option { scope: Option<u8>, name: Vec<u8> },
    /// `@r`; the payload is the register-name byte.
    Register(u8),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Dot,
    DotDot,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    AndAnd,
    OrOr,
    Question,
    Coalesce,
    Arrow,
    HashLBrace,
    Equal(CaseSensitivity),
    NotEqual(CaseSensitivity),
    Greater(CaseSensitivity),
    GreaterEqual(CaseSensitivity),
    Less(CaseSensitivity),
    LessEqual(CaseSensitivity),
    Match(CaseSensitivity),
    NoMatch(CaseSensitivity),
    Is(CaseSensitivity),
    IsNot(CaseSensitivity),
}

/// One token and its exact source range.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    /// Token payload.
    pub kind: TokenKind,
    /// Half-open byte range in the original source.
    pub span: Span,
}

/// Byte-oriented Vimscript expression lexer.
pub struct Lexer<'a> {
    source: &'a [u8],
    offset: usize,
    at_line_start: bool,
}

impl<'a> Lexer<'a> {
    /// Create a lexer over an expression byte slice.
    #[must_use]
    pub const fn new(source: &'a [u8]) -> Self {
        Self { source, offset: 0, at_line_start: true }
    }

    /// Tokenize the complete expression, including one trailing [`TokenKind::Eof`].
    pub fn tokenize(mut self) -> Result<Vec<Token>, EvalError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_layout();
            let start = self.offset;
            let kind = match self.peek(0) {
                None => TokenKind::Eof,
                Some(b'\n') => TokenKind::Eof,
                Some(b'0') if matches!(self.peek(1), Some(b'z' | b'Z')) => self.lex_blob()?,
                Some(b'0'..=b'9') => self.lex_number()?,
                Some(b'\'') => self.lex_single_string()?,
                Some(b'"') => self.lex_double_string()?,
                Some(b'$') => self.lex_environment()?,
                Some(b'&') if self.peek(1) == Some(b'&') => {
                    self.offset += 2;
                    TokenKind::AndAnd
                }
                Some(b'&') => self.lex_option()?,
                Some(b'@') => self.lex_register()?,
                Some(b'#') if self.peek(1) == Some(b'{') => {
                    self.offset += 2;
                    TokenKind::HashLBrace
                }
                Some(b'(') => self.one(TokenKind::LParen),
                Some(b')') => self.one(TokenKind::RParen),
                Some(b'[') => self.one(TokenKind::LBracket),
                Some(b']') => self.one(TokenKind::RBracket),
                Some(b'{') => self.one(TokenKind::LBrace),
                Some(b'}') => self.one(TokenKind::RBrace),
                Some(b',') => self.one(TokenKind::Comma),
                Some(b':') => self.one(TokenKind::Colon),
                Some(b'+') => self.one(TokenKind::Plus),
                Some(b'*') => self.one(TokenKind::Star),
                Some(b'/') => self.one(TokenKind::Slash),
                Some(b'%') => self.one(TokenKind::Percent),
                Some(b'-') if self.peek(1) == Some(b'>') => {
                    self.offset += 2;
                    TokenKind::Arrow
                }
                Some(b'-') => self.one(TokenKind::Minus),
                Some(b'.') if self.peek(1) == Some(b'.') => {
                    self.offset += 2;
                    TokenKind::DotDot
                }
                Some(b'.') => self.one(TokenKind::Dot),
                Some(b'?') if self.peek(1) == Some(b'?') => {
                    self.offset += 2;
                    TokenKind::Coalesce
                }
                Some(b'?') => self.one(TokenKind::Question),
                Some(b'|') if self.peek(1) == Some(b'|') => {
                    self.offset += 2;
                    TokenKind::OrOr
                }
                Some(b'=') if self.peek(1) == Some(b'=') => {
                    self.offset += 2;
                    TokenKind::Equal(self.lex_case_suffix())
                }
                Some(b'=') if self.peek(1) == Some(b'~') => {
                    self.offset += 2;
                    TokenKind::Match(self.lex_case_suffix())
                }
                Some(b'!') if self.peek(1) == Some(b'=') => {
                    self.offset += 2;
                    TokenKind::NotEqual(self.lex_case_suffix())
                }
                Some(b'!') if self.peek(1) == Some(b'~') => {
                    self.offset += 2;
                    TokenKind::NoMatch(self.lex_case_suffix())
                }
                Some(b'!') => self.one(TokenKind::Bang),
                Some(b'>') if self.peek(1) == Some(b'=') => {
                    self.offset += 2;
                    TokenKind::GreaterEqual(self.lex_case_suffix())
                }
                Some(b'>') => {
                    self.offset += 1;
                    TokenKind::Greater(self.lex_case_suffix())
                }
                Some(b'<') if self.peek(1) == Some(b'=') => {
                    self.offset += 2;
                    TokenKind::LessEqual(self.lex_case_suffix())
                }
                Some(b'<') => {
                    self.offset += 1;
                    TokenKind::Less(self.lex_case_suffix())
                }
                Some(byte) if is_name_start(byte) => self.lex_identifier(),
                Some(byte) => {
                    return Err(EvalError::new(
                        "E15",
                        start,
                        format!("invalid character 0x{byte:02x} in expression"),
                    ));
                }
            };
            let eof = matches!(kind, TokenKind::Eof);
            tokens.push(Token { kind, span: Span::new(start, self.offset) });
            if eof {
                return Ok(tokens);
            }
            self.at_line_start = false;
        }
    }

    fn peek(&self, ahead: usize) -> Option<u8> {
        self.source.get(self.offset + ahead).copied()
    }

    fn one(&mut self, kind: TokenKind) -> TokenKind {
        self.offset += 1;
        kind
    }

    fn skip_layout(&mut self) {
        loop {
            while matches!(self.peek(0), Some(b' ' | b'\t')) {
                self.offset += 1;
            }
            if self.peek(0) == Some(b'\n') {
                let mut ahead = 1;
                while matches!(self.peek(ahead), Some(b' ' | b'\t')) {
                    ahead += 1;
                }
                if self.peek(ahead) == Some(b'"')
                    && self.peek(ahead + 1) == Some(b'\\')
                    && self.peek(ahead + 2) == Some(b' ')
                {
                    self.offset += ahead + 3;
                    while !matches!(self.peek(0), None | Some(b'\n')) {
                        self.offset += 1;
                    }
                    self.at_line_start = true;
                    continue;
                }
                if self.peek(ahead) != Some(b'\\') {
                    break;
                }
                self.offset += ahead + 1;
                while matches!(self.peek(0), Some(b' ' | b'\t')) {
                    self.offset += 1;
                }
                self.at_line_start = false;
                continue;
            }
            if self.at_line_start && self.peek(0) == Some(b'\\') {
                self.offset += 1;
                while matches!(self.peek(0), Some(b' ' | b'\t')) {
                    self.offset += 1;
                }
                self.at_line_start = false;
                continue;
            }
            // In a continued expression Vim permits a standalone comment line
            // beginning with `"\ `; it contributes no expression tokens.
            if self.at_line_start
                && self.peek(0) == Some(b'"')
                && self.peek(1) == Some(b'\\')
                && self.peek(2) == Some(b' ')
            {
                while !matches!(self.peek(0), None | Some(b'\n')) {
                    self.offset += 1;
                }
                continue;
            }
            break;
        }
    }

    fn lex_case_suffix(&mut self) -> CaseSensitivity {
        match self.peek(0) {
            Some(b'#') => {
                self.offset += 1;
                CaseSensitivity::MatchCase
            }
            Some(b'?') => {
                self.offset += 1;
                CaseSensitivity::IgnoreCase
            }
            _ => CaseSensitivity::Default,
        }
    }

    fn lex_identifier(&mut self) -> TokenKind {
        let start = self.offset;
        self.offset += 1;
        if self.offset == start + 1
            && matches!(self.source[start], b'g' | b'b' | b'w' | b't' | b's' | b'l' | b'a' | b'v')
            && self.peek(0) == Some(b':')
            && self.peek(1).is_some_and(is_name_start)
        {
            self.offset += 1;
        }
        while matches!(self.peek(0), Some(byte) if is_name_continue(byte)) {
            self.offset += 1;
        }
        let bytes = self.source[start..self.offset].to_vec();
        if bytes.as_slice() == b"is#" {
            return TokenKind::Is(CaseSensitivity::MatchCase);
        }
        if bytes.as_slice() == b"isnot#" {
            return TokenKind::IsNot(CaseSensitivity::MatchCase);
        }
        let case = if self.peek(0) == Some(b'#') || self.peek(0) == Some(b'?') {
            self.lex_case_suffix()
        } else {
            CaseSensitivity::Default
        };
        match bytes.as_slice() {
            b"is" => TokenKind::Is(case),
            b"isnot" => TokenKind::IsNot(case),
            _ => TokenKind::Identifier(bytes),
        }
    }

    fn lex_number(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        if self.peek(0) == Some(b'0') {
            match self.peek(1) {
                Some(b'x' | b'X') => return self.lex_based_integer(start, 16, 2),
                Some(b'o' | b'O') => return self.lex_based_integer(start, 8, 2),
                Some(b'b' | b'B') => return self.lex_based_integer(start, 2, 2),
                _ => {}
            }
        }
        while matches!(self.peek(0), Some(b'0'..=b'9')) {
            self.offset += 1;
        }
        if self.peek(0) == Some(b'.') && matches!(self.peek(1), Some(b'0'..=b'9')) {
            self.offset += 1;
            while matches!(self.peek(0), Some(b'0'..=b'9')) {
                self.offset += 1;
            }
            if matches!(self.peek(0), Some(b'e' | b'E')) {
                let exponent = self.offset;
                self.offset += 1;
                if matches!(self.peek(0), Some(b'+' | b'-')) {
                    self.offset += 1;
                }
                let digits = self.offset;
                while matches!(self.peek(0), Some(b'0'..=b'9')) {
                    self.offset += 1;
                }
                if self.offset == digits {
                    return Err(EvalError::new("E15", exponent, "missing float exponent"));
                }
            }
            let text = std::str::from_utf8(&self.source[start..self.offset])
                .map_err(|_| EvalError::new("E15", start, "invalid float literal"))?;
            return text
                .parse::<f64>()
                .map(TokenKind::Float)
                .map_err(|_| EvalError::new("E15", start, "invalid float literal"));
        }
        let old_octal = self.source[start] == b'0'
            && self.offset > start + 1
            && self.source[start + 1..self.offset]
                .iter()
                .all(|byte| matches!(byte, b'0'..=b'7'));
        let (base, digits_start) = if old_octal {
            (8, start + 1)
        } else {
            (10, start)
        };
        let digits = &self.source[digits_start..self.offset];
        let digits = if digits.is_empty() { &self.source[start..self.offset] } else { digits };
        parse_integer(digits, base, start).map(TokenKind::Integer)
    }

    fn lex_based_integer(
        &mut self,
        start: usize,
        base: u32,
        prefix_len: usize,
    ) -> Result<TokenKind, EvalError> {
        self.offset += prefix_len;
        let digits_start = self.offset;
        while matches!(self.peek(0), Some(byte) if byte.is_ascii_hexdigit()) {
            self.offset += 1;
        }
        if self.offset == digits_start {
            return Err(EvalError::new("E15", start, "missing digits after numeric prefix"));
        }
        let digits = &self.source[digits_start..self.offset];
        if digits
            .iter()
            .any(|byte| match hex_value(*byte) {
                Some(digit) => u32::from(digit) >= base,
                None => true,
            })
        {
            return Err(EvalError::new("E15", start, "digit is invalid for numeric base"));
        }
        parse_integer(digits, base, start).map(TokenKind::Integer)
    }

    fn lex_blob(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 2;
        let mut digits = Vec::new();
        while let Some(byte) = self.peek(0) {
            if byte.is_ascii_hexdigit() {
                digits.push(byte);
                self.offset += 1;
            } else if byte == b'.' && self.peek(1).is_some_and(|next| next.is_ascii_hexdigit()) {
                self.offset += 1;
            } else {
                break;
            }
        }
        if matches!(self.peek(0), Some(byte) if is_name_continue(byte)) {
            return Err(EvalError::new("E973", start, "invalid character in blob literal"));
        }
        if digits.len() % 2 != 0 {
            return Err(EvalError::new("E973", start, "blob literal should have an even number of hex characters"));
        }
        let mut bytes = Vec::with_capacity(digits.len() / 2);
        for pair in digits.chunks_exact(2) {
            let high = hex_value(pair[0]).ok_or_else(|| EvalError::new("E973", start, "invalid blob digit"))?;
            let low = hex_value(pair[1]).ok_or_else(|| EvalError::new("E973", start, "invalid blob digit"))?;
            bytes.push((high << 4) | low);
        }
        Ok(TokenKind::Blob(bytes))
    }

    fn lex_single_string(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 1;
        let mut bytes = Vec::new();
        loop {
            match self.peek(0) {
                None => return Err(EvalError::new("E115", start, "missing single quote")),
                Some(b'\'') if self.peek(1) == Some(b'\'') => {
                    bytes.push(b'\'');
                    self.offset += 2;
                }
                Some(b'\'') => {
                    self.offset += 1;
                    return Ok(TokenKind::String(bytes));
                }
                Some(byte) => {
                    bytes.push(byte);
                    self.offset += 1;
                }
            }
        }
    }

    fn lex_double_string(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 1;
        let mut bytes = Vec::new();
        let mut nul_seen = false;
        loop {
            match self.peek(0) {
                None => return Err(EvalError::new("E114", start, "missing double quote")),
                Some(b'"') => {
                    self.offset += 1;
                    return Ok(TokenKind::String(bytes));
                }
                Some(b'\\') => {
                    self.offset += 1;
                    let escaped = self.lex_escape(start)?;
                    if !nul_seen {
                        if escaped.contains(&0) {
                            let before_nul = match escaped.iter().position(|byte| *byte == 0) {
                                Some(position) => position,
                                None => escaped.len(),
                            };
                            bytes.extend_from_slice(&escaped[..before_nul]);
                            nul_seen = true;
                        } else {
                            bytes.extend_from_slice(&escaped);
                        }
                    }
                }
                Some(byte) => {
                    if !nul_seen {
                        bytes.push(byte);
                    }
                    self.offset += 1;
                }
            }
        }
    }

    fn lex_escape(&mut self, string_start: usize) -> Result<Vec<u8>, EvalError> {
        let escape_offset = self.offset.saturating_sub(1);
        let Some(byte) = self.peek(0) else {
            return Err(EvalError::new("E114", string_start, "unfinished string escape"));
        };
        self.offset += 1;
        let simple = match byte {
            b'b' => Some(0x08),
            b'e' => Some(0x1b),
            b'f' => Some(0x0c),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'\\' => Some(b'\\'),
            b'"' => Some(b'"'),
            _ => None,
        };
        if let Some(value) = simple {
            return Ok(vec![value]);
        }
        if matches!(byte, b'0'..=b'7') {
            let mut value = u32::from(byte - b'0');
            for _ in 1..3 {
                let Some(next @ b'0'..=b'7') = self.peek(0) else { break };
                value = value * 8 + u32::from(next - b'0');
                self.offset += 1;
            }
            return Ok(vec![(value & 0xff) as u8]);
        }
        if matches!(byte, b'x' | b'X') {
            let value = self.read_hex_escape(2, escape_offset)?;
            return Ok(vec![value as u8]);
        }
        if matches!(byte, b'u' | b'U') {
            let limit = if byte == b'u' { 4 } else { 8 };
            let value = self.read_hex_escape(limit, escape_offset)?;
            let Some(character) = char::from_u32(value) else {
                return Err(EvalError::new("E114", escape_offset, "invalid Unicode escape"));
            };
            let mut encoded = [0; 4];
            return Ok(character.encode_utf8(&mut encoded).as_bytes().to_vec());
        }
        if byte == b'<' {
            let name_start = self.offset;
            while !matches!(self.peek(0), None | Some(b'>')) {
                self.offset += 1;
            }
            if self.peek(0) != Some(b'>') {
                return Err(EvalError::new("E114", escape_offset, "unfinished special key escape"));
            }
            let name = &self.source[name_start..self.offset];
            self.offset += 1;
            return decode_special_key(name, escape_offset);
        }
        // As in Vim, an unrecognized escape keeps the escaped byte and drops
        // only the backslash.
        Ok(vec![byte])
    }

    fn read_hex_escape(&mut self, limit: usize, offset: usize) -> Result<u32, EvalError> {
        let mut value = 0_u32;
        let mut count = 0;
        while count < limit {
            let Some(byte) = self.peek(0) else { break };
            let Some(digit) = hex_value(byte) else { break };
            value = value * 16 + u32::from(digit);
            count += 1;
            self.offset += 1;
        }
        if count == 0 {
            Err(EvalError::new("E114", offset, "hex escape requires at least one digit"))
        } else {
            Ok(value)
        }
    }

    fn lex_environment(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 1;
        let name_start = self.offset;
        while matches!(self.peek(0), Some(byte) if is_name_continue(byte)) {
            self.offset += 1;
        }
        if self.offset == name_start {
            Err(EvalError::new("E15", start, "environment variable name is missing"))
        } else {
            Ok(TokenKind::Environment(self.source[name_start..self.offset].to_vec()))
        }
    }

    fn lex_option(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 1;
        let scope = if matches!((self.peek(0), self.peek(1)), (Some(b'g' | b'l'), Some(b':'))) {
            let scope = self.peek(0);
            self.offset += 2;
            scope
        } else {
            None
        };
        let name_start = self.offset;
        while matches!(self.peek(0), Some(byte) if is_name_continue(byte)) {
            self.offset += 1;
        }
        if self.offset == name_start {
            Err(EvalError::new("E112", start, "option name is missing"))
        } else {
            Ok(TokenKind::Option { scope, name: self.source[name_start..self.offset].to_vec() })
        }
    }

    fn lex_register(&mut self) -> Result<TokenKind, EvalError> {
        let start = self.offset;
        self.offset += 1;
        let Some(name) = self.peek(0) else {
            return Err(EvalError::new("E15", start, "register name is missing"));
        };
        self.offset += 1;
        Ok(TokenKind::Register(name))
    }
}

fn parse_integer(digits: &[u8], base: u32, offset: usize) -> Result<i64, EvalError> {
    let text = std::str::from_utf8(digits)
        .map_err(|_| EvalError::new("E15", offset, "invalid integer literal"))?;
    i64::from_str_radix(text, base)
        .map_err(|_| EvalError::new("E15", offset, "integer literal is out of range"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_name_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'#')
}

fn decode_special_key(name: &[u8], offset: usize) -> Result<Vec<u8>, EvalError> {
    let lower: Vec<u8> = name.iter().map(u8::to_ascii_lowercase).collect();
    let value = match lower.as_slice() {
        b"bs" => Some(0x08),
        b"tab" => Some(b'\t'),
        b"nl" => Some(b'\n'),
        b"cr" | b"return" | b"enter" => Some(b'\r'),
        b"esc" => Some(0x1b),
        b"space" => Some(b' '),
        b"lt" => Some(b'<'),
        b"bslash" => Some(b'\\'),
        b"bar" => Some(b'|'),
        b"del" => Some(0x7f),
        [b'c', b'-', key] => Some(if *key == b'?' { 0x7f } else { key & 0x1f }),
        _ => None,
    };
    value
        .map(|byte| vec![byte])
        .ok_or_else(|| EvalError::new("E114", offset, "unsupported special key escape"))
}
