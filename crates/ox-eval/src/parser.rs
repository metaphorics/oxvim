//! Recursive-descent parser for legacy Vimscript expressions.
//!
//! The precedence functions follow `expr1` through `expr9` in
//! `runtime/doc/vimeval.txt`.  The AST owns byte strings through [`OxStr`] and
//! retains source byte spans for diagnostics and evaluator errors.

use ox_types::{OxStr, Typval};

use crate::error::EvalError;
use crate::lexer::{
    CaseSensitivity, InterpolationPart as LexInterpolationPart, Lexer, Span, Token, TokenKind,
};

static FALLBACK_EOF: Token = Token {
    kind: TokenKind::Eof,
    span: Span { start: 0, end: 0 },
};

/// Vim's default maximum expression nesting (see `E1169`).
pub const DEFAULT_MAX_NESTING: usize = 1_000;

/// A parsed expression and its exact source range.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    /// Expression payload.
    pub kind: ExprKind,
    /// Half-open byte range in the original expression.
    pub span: Span,
}

impl Expr {
    fn new(kind: ExprKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// Vim option selection in an `&` expression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionScope {
    /// `&name`: prefer a local value and fall back to global.
    Effective,
    /// `&g:name`: global value.
    Global,
    /// `&l:name`: local value.
    Local,
}

/// Unary `expr7` operators.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnaryOp {
    Not,
    Negate,
    Plus,
}

/// Non-comparison binary operators from `expr2`, `expr3`, `expr5`, and `expr6`.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinaryOp {
    Or,
    And,
    Add,
    Subtract,
    Concat,
    Multiply,
    Divide,
    Modulo,
}

/// Comparison operations from `expr4`.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareOp {
    Equal,
    NotEqual,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
    Match,
    NoMatch,
    Is,
    IsNot,
}

/// Public evaluator-facing Vimscript expression AST.
#[allow(missing_docs)]
#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    /// A number, float, string, or blob literal.
    Literal(Typval),
    /// A string assembled from literal and evaluated expression parts.
    Interpolated(Vec<InterpolatedPart>),
    /// A scoped or unscoped internal variable name.
    Variable(OxStr),
    /// `$NAME`.
    Environment(OxStr),
    /// `&name`, `&g:name`, or `&l:name`.
    Option { scope: OptionScope, name: OxStr },
    /// `@r`.
    Register(u8),
    /// `[expr, ...]`.
    List(Vec<Expr>),
    /// `{key: value, ...}` or `#{literal-key: value, ...}`.
    Dict(Vec<(Expr, Expr)>),
    /// A prefix operation.
    Unary { op: UnaryOp, expr: Box<Expr> },
    /// A left-associative binary operation.
    Binary { op: BinaryOp, left: Box<Expr>, right: Box<Expr> },
    /// A comparison and its optional `#`/`?` case suffix.
    Compare {
        op: CompareOp,
        case: CaseSensitivity,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `condition ? then_expr : else_expr`.
    Ternary {
        condition: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },
    /// Vim's falsy-coalescing `left ?? right`.
    Coalesce { left: Box<Expr>, right: Box<Expr> },
    /// A Funcref or named function call.
    Call { callee: Box<Expr>, args: Vec<Expr> },
    /// Dictionary member access.
    Member { target: Box<Expr>, name: OxStr },
    /// A single subscript.
    Index { target: Box<Expr>, index: Box<Expr> },
    /// An inclusive slice; either bound may be omitted.
    Slice {
        target: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
    },
    /// `receiver->method(args)`; `method` may be a name or lambda.
    MethodCall { receiver: Box<Expr>, method: Box<Expr>, args: Vec<Expr> },
    /// `{arg, ... -> expr}`. `varargs` is true when the parameter list ends
    /// with `...`, matching `get_lambda_tv`'s acceptance of the variadic form.
    Lambda { params: Vec<OxStr>, varargs: bool, body: Box<Expr> },
}

/// One literal or parsed-expression segment in an interpolated string.
#[derive(Clone, Debug, PartialEq)]
pub enum InterpolatedPart {
    /// Decoded literal bytes.
    Literal(OxStr),
    /// Parsed embedded expression.
    Expression(Expr),
}

/// Parser for one complete Vimscript expression.
pub struct Parser<'a> {
    source: &'a [u8],
    max_nesting: usize,
    tokens: Vec<Token>,
    cursor: usize,
    nesting: usize,
}

/// Choose between a parse failure and the byte the lexer refused.
///
/// [`Lexer::tokenize_tolerant`] leaves a synthetic `Eof` at `stop`, the offset
/// it refused to read past, so a parser that ran out of tokens fails *there*
/// while really having needed the refused byte. Upstream lexes lazily and
/// would have hit the byte itself, so its own error is the honest one. A parse
/// failure before `stop` is unrelated and stands. The refusal's own offset is
/// not the comparison point: it can sit inside the token that failed to lex,
/// past the offset the parser reports.
fn resolve_refusal(error: EvalError, refused: &Option<EvalError>, stop: usize) -> EvalError {
    match refused {
        Some(refusal) if error.offset >= stop => refusal.clone(),
        _ => error,
    }
}

impl<'a> Parser<'a> {
    /// Create a parser using [`DEFAULT_MAX_NESTING`].
    #[must_use]
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            max_nesting: DEFAULT_MAX_NESTING,
            tokens: Vec::new(),
            cursor: 0,
            nesting: 0,
        }
    }

    /// Override the parser nesting budget.
    #[must_use]
    pub const fn with_max_nesting(mut self, maximum: usize) -> Self {
        self.max_nesting = maximum;
        self
    }

    /// Parse one complete expression and reject trailing tokens.
    pub fn parse(mut self) -> Result<Expr, EvalError> {
        let (tokens, refused) = Lexer::new(self.source).tokenize_tolerant();
        let stop = tokens.last().map_or(0, |token| token.span.start);
        self.tokens = tokens;
        let expression = self.parse_expr1().map_err(|error| resolve_refusal(error, &refused, stop))?;
        if !matches!(self.current().kind, TokenKind::Eof) || refused.is_some() {
            // Upstream reports the unconsumed remainder verbatim:
            // `e_trailing_arg` is "E488: Trailing characters: %s" (errors.h:123),
            // raised from `eval.c:1251` once `eval0` stops short of the end.
            // A byte the lexer refused counts as remainder too, because
            // `eval0` never looked at it: the expression in front of it was
            // already complete.
            let start = self.current().span.start;
            let rest = String::from_utf8_lossy(&self.source[start..]);
            return Err(EvalError::new("E488", start, format!("Trailing characters: {rest}")));
        }
        Ok(expression)
    }

    /// Parse whitespace-separated expressions, as consumed by `:execute`.
    pub fn parse_many(mut self) -> Result<Vec<Expr>, EvalError> {
        let (tokens, refused) = Lexer::new(self.source).tokenize_tolerant();
        let stop = tokens.last().map_or(0, |token| token.span.start);
        self.tokens = tokens;
        let mut expressions: Vec<Expr> = Vec::new();
        while !matches!(self.current().kind, TokenKind::Eof) {
            if let Some(previous) = expressions.last() {
                let gap = &self.source[previous.span.end..self.current().span.start];
                if !gap.iter().any(|byte| matches!(byte, b' ' | b'\t')) {
                    return Err(EvalError::new(
                        "E15",
                        self.current().span.start,
                        "trailing characters after expression",
                    ));
                }
            }
            expressions.push(self.parse_expr1().map_err(|error| resolve_refusal(error, &refused, stop))?);
        }
        // `:echo`, `:echomsg` and `:execute` loop `eval1` until the line is
        // spent (`eval.c:1846` and `ex_docmd`'s echo handlers), so they do
        // reach a byte the lexer refused and answer E15, not E488.
        match refused {
            Some(error) => Err(error),
            None => Ok(expressions),
        }
    }

    fn parse_expr1(&mut self) -> Result<Expr, EvalError> {
        let offset = self.current().span.start;
        if self.nesting >= self.max_nesting {
            return Err(EvalError::new("E1169", offset, "expression nesting is too deep"));
        }
        self.nesting += 1;
        let result = self.parse_expr1_inner();
        self.nesting -= 1;
        result
    }

    fn parse_expr1_inner(&mut self) -> Result<Expr, EvalError> {
        let left = self.parse_expr2()?;
        if self.take(|kind| matches!(kind, TokenKind::Question)).is_some() {
            let then_expr = self.parse_expr1()?;
            self.require(|kind| matches!(kind, TokenKind::Colon), "E109", "missing ':' after '?' branch")?;
            let else_expr = self.parse_expr1()?;
            let span = left.span.through(else_expr.span);
            return Ok(Expr::new(
                ExprKind::Ternary {
                    condition: Box::new(left),
                    then_expr: Box::new(then_expr),
                    else_expr: Box::new(else_expr),
                },
                span,
            ));
        }
        if self.take(|kind| matches!(kind, TokenKind::Coalesce)).is_some() {
            let right = self.parse_expr1()?;
            let span = left.span.through(right.span);
            return Ok(Expr::new(
                ExprKind::Coalesce { left: Box::new(left), right: Box::new(right) },
                span,
            ));
        }
        Ok(left)
    }

    fn parse_expr2(&mut self) -> Result<Expr, EvalError> {
        let mut left = self.parse_expr3()?;
        while self.take(|kind| matches!(kind, TokenKind::OrOr)).is_some() {
            let right = self.parse_expr3()?;
            left = binary(BinaryOp::Or, left, right);
        }
        Ok(left)
    }

    fn parse_expr3(&mut self) -> Result<Expr, EvalError> {
        let mut left = self.parse_expr4()?;
        while self.take(|kind| matches!(kind, TokenKind::AndAnd)).is_some() {
            let right = self.parse_expr4()?;
            left = binary(BinaryOp::And, left, right);
        }
        Ok(left)
    }

    fn parse_expr4(&mut self) -> Result<Expr, EvalError> {
        let left = self.parse_expr5()?;
        let Some((op, case)) = self.take_comparison() else {
            return Ok(left);
        };
        let right = self.parse_expr5()?;
        let span = left.span.through(right.span);
        Ok(Expr::new(
            ExprKind::Compare { op, case, left: Box::new(left), right: Box::new(right) },
            span,
        ))
    }

    fn parse_expr5(&mut self) -> Result<Expr, EvalError> {
        let mut left = self.parse_expr6()?;
        loop {
            let op = if self.take(|kind| matches!(kind, TokenKind::Plus)).is_some() {
                Some(BinaryOp::Add)
            } else if self.take(|kind| matches!(kind, TokenKind::Minus)).is_some() {
                Some(BinaryOp::Subtract)
            } else if self.take(|kind| matches!(kind, TokenKind::Dot | TokenKind::DotDot)).is_some() {
                Some(BinaryOp::Concat)
            } else {
                None
            };
            let Some(op) = op else { break };
            let right = self.parse_expr6()?;
            left = binary(op, left, right);
        }
        Ok(left)
    }

    fn parse_expr6(&mut self) -> Result<Expr, EvalError> {
        let mut left = self.parse_expr7()?;
        loop {
            let op = if self.take(|kind| matches!(kind, TokenKind::Star)).is_some() {
                Some(BinaryOp::Multiply)
            } else if self.take(|kind| matches!(kind, TokenKind::Slash)).is_some() {
                Some(BinaryOp::Divide)
            } else if self.take(|kind| matches!(kind, TokenKind::Percent)).is_some() {
                Some(BinaryOp::Modulo)
            } else {
                None
            };
            let Some(op) = op else { break };
            let right = self.parse_expr7()?;
            left = binary(op, left, right);
        }
        Ok(left)
    }

    fn parse_expr7(&mut self) -> Result<Expr, EvalError> {
        let mut operators = Vec::new();
        loop {
            let op = if self.take(|kind| matches!(kind, TokenKind::Bang)).is_some() {
                Some(UnaryOp::Not)
            } else if self.take(|kind| matches!(kind, TokenKind::Minus)).is_some() {
                Some(UnaryOp::Negate)
            } else if self.take(|kind| matches!(kind, TokenKind::Plus)).is_some() {
                Some(UnaryOp::Plus)
            } else {
                None
            };
            let Some(op) = op else { break };
            operators.push((op, self.previous_span().start));
        }
        let mut expression = self.parse_expr8()?;
        while let Some((op, start)) = operators.pop() {
            let span = Span::new(start, expression.span.end);
            expression = Expr::new(ExprKind::Unary { op, expr: Box::new(expression) }, span);
        }
        while self.take(|kind| matches!(kind, TokenKind::Arrow)).is_some() {
            expression = self.parse_method_call(expression)?;
            expression = self.parse_expr8_tail(expression)?;
        }
        Ok(expression)
    }

    fn parse_expr8(&mut self) -> Result<Expr, EvalError> {
        let expression = self.parse_expr9()?;
        self.parse_expr8_tail(expression)
    }

    fn parse_expr8_tail(&mut self, mut expression: Expr) -> Result<Expr, EvalError> {
        loop {
            if matches!(self.current().kind, TokenKind::LBracket)
                && expression.span.end == self.current().span.start
            {
                self.advance();
                expression = self.parse_subscript(expression)?;
                continue;
            }
            if self.is_adjacent_member(expression.span.end) {
                self.advance();
                let token = self.advance().clone();
                let name = match token.kind {
                    TokenKind::Identifier(bytes) => bytes,
                    TokenKind::Integer(number) => number.to_string().into_bytes(),
                    _ => return Err(EvalError::new("E15", token.span.start, "member name expected")),
                };
                let span = expression.span.through(token.span);
                expression = Expr::new(
                    ExprKind::Member { target: Box::new(expression), name: OxStr(name) },
                    span,
                );
                continue;
            }
            if matches!(self.current().kind, TokenKind::LParen)
                && expression.span.end == self.current().span.start
            {
                self.advance();
                let (args, end) = self.parse_arguments()?;
                let span = Span::new(expression.span.start, end);
                expression = Expr::new(ExprKind::Call { callee: Box::new(expression), args }, span);
                continue;
            }
            break;
        }
        Ok(expression)
    }

    fn parse_expr9(&mut self) -> Result<Expr, EvalError> {
        let token = self.advance().clone();
        match token.kind {
            TokenKind::Integer(value) => Ok(Expr::new(ExprKind::Literal(Typval::Number(value)), token.span)),
            TokenKind::Float(value) => Ok(Expr::new(ExprKind::Literal(Typval::Float(value)), token.span)),
            TokenKind::String(value) => Ok(Expr::new(ExprKind::Literal(Typval::String(OxStr(value))), token.span)),
            TokenKind::Interpolated(parts) => {
                let parts = parts
                    .into_iter()
                    .map(|part| match part {
                        LexInterpolationPart::Literal(bytes) => {
                            Ok(InterpolatedPart::Literal(OxStr(bytes)))
                        }
                        LexInterpolationPart::Expression(source) => {
                            Parser::new(&source).parse().map(InterpolatedPart::Expression)
                        }
                    })
                    .collect::<Result<Vec<_>, EvalError>>()?;
                Ok(Expr::new(ExprKind::Interpolated(parts), token.span))
            }
            TokenKind::Blob(value) => Ok(Expr::new(ExprKind::Literal(Typval::Blob(value)), token.span)),
            TokenKind::Identifier(value) => {
                let variable = self.parse_variable(value, token.span)?;
                self.parse_detached_call(variable)
            }
            TokenKind::Environment(value) => Ok(Expr::new(ExprKind::Environment(OxStr(value)), token.span)),
            TokenKind::Option { scope, name } => {
                let scope = match scope {
                    Some(b'g') => OptionScope::Global,
                    Some(b'l') => OptionScope::Local,
                    _ => OptionScope::Effective,
                };
                Ok(Expr::new(ExprKind::Option { scope, name: OxStr(name) }, token.span))
            }
            TokenKind::Register(name) => Ok(Expr::new(ExprKind::Register(name), token.span)),
            TokenKind::LParen => {
                let expression = self.parse_expr1()?;
                let close = self.require(|kind| matches!(kind, TokenKind::RParen), "E110", "missing ')'" )?;
                Ok(Expr::new(expression.kind, Span::new(token.span.start, close.span.end)))
            }
            TokenKind::LBracket => self.parse_list(token.span.start),
            TokenKind::HashLBrace => self.parse_literal_dict(token.span.start),
            TokenKind::LBrace if self.brace_is_lambda() => self.parse_lambda(token.span.start),
            TokenKind::LBrace => self.parse_dict(token.span.start),
            // Nothing here can begin an expression. Upstream does not describe
            // the offending token; `e_invexpr2` (errors.h:38) quotes the whole
            // expression back: `E15: Invalid expression: "%s"`.
            _ => Err(self.invalid_expression()),
        }
    }

    /// `e_invexpr2` (errors.h:38): `E15: Invalid expression: "%s"`, quoting the
    /// whole expression rather than the token that could not be parsed.
    fn invalid_expression(&self) -> EvalError {
        let source = String::from_utf8_lossy(self.source);
        EvalError::new("E15", 0, format!("Invalid expression: \"{source}\""))
    }

    fn parse_variable(&mut self, mut name: Vec<u8>, mut span: Span) -> Result<Expr, EvalError> {
        if name.len() == 1
            && is_scope_prefix(name[0])
            && matches!(self.current().kind, TokenKind::Colon)
            && span.end == self.current().span.start
        {
            let colon = self.advance().clone();
            name.push(b':');
            span.end = colon.span.end;
            if span.end == self.current().span.start {
                match &self.current().kind {
                    TokenKind::Identifier(suffix) => {
                        name.extend_from_slice(suffix);
                        span.end = self.advance().span.end;
                    }
                    // `a:0`, `a:000`, `a:1`, ... — variadic lambda arguments.
                    // The raw source is captured so `a:000` (the List) stays
                    // distinct from `a:0` (the count), which both lex as the
                    // integer token `0`.
                    TokenKind::Integer(_) if name.as_slice() == b"a:" => {
                        let suffix = &self.source[self.current().span.start..self.current().span.end];
                        name.extend_from_slice(suffix);
                        span.end = self.advance().span.end;
                    }
                    _ => {}
                }
            }
        }
        Ok(Expr::new(ExprKind::Variable(OxStr(name)), span))
    }

    /// A bare name may be separated from its argument list by white space, so
    /// `substitute ( 'a', 'b', 'c', 'g' )` is a legal legacy call. Only a name
    /// at the head of an expression gets this; in the subscript chain
    /// `d.Fn ()`, `l[0] ()` and `Fn() ()` all stay errors.
    /// Upstream: `eval.c:2783-2786` skips white space before testing for `(`,
    /// while `handle_subscript` (`eval.c:6022-6026`) requires the `(` to be
    /// adjacent.
    fn parse_detached_call(&mut self, callee: Expr) -> Result<Expr, EvalError> {
        if !matches!(self.current().kind, TokenKind::LParen) {
            return Ok(callee);
        }
        self.advance();
        let (args, end) = self.parse_arguments()?;
        let span = Span::new(callee.span.start, end);
        Ok(Expr::new(ExprKind::Call { callee: Box::new(callee), args }, span))
    }

    fn parse_list(&mut self, start: usize) -> Result<Expr, EvalError> {
        let mut items = Vec::new();
        if let Some(close) = self.take(|kind| matches!(kind, TokenKind::RBracket)) {
            return Ok(Expr::new(ExprKind::List(items), Span::new(start, close.span.end)));
        }
        loop {
            items.push(self.parse_expr1()?);
            if self.take(|kind| matches!(kind, TokenKind::Comma)).is_none() {
                break;
            }
            if matches!(self.current().kind, TokenKind::RBracket) {
                break;
            }
        }
        let close = self.require(|kind| matches!(kind, TokenKind::RBracket), "E696", "missing ']'" )?;
        Ok(Expr::new(ExprKind::List(items), Span::new(start, close.span.end)))
    }

    fn parse_dict(&mut self, start: usize) -> Result<Expr, EvalError> {
        let mut entries = Vec::new();
        if let Some(close) = self.take(|kind| matches!(kind, TokenKind::RBrace)) {
            return Ok(Expr::new(ExprKind::Dict(entries), Span::new(start, close.span.end)));
        }
        loop {
            let key = self.parse_expr1()?;
            self.require(|kind| matches!(kind, TokenKind::Colon), "E720", "missing ':' in dictionary")?;
            let value = self.parse_expr1()?;
            entries.push((key, value));
            if self.take(|kind| matches!(kind, TokenKind::Comma)).is_none() {
                break;
            }
            if matches!(self.current().kind, TokenKind::RBrace) {
                break;
            }
        }
        let close = self.require(|kind| matches!(kind, TokenKind::RBrace), "E723", "missing '}'" )?;
        Ok(Expr::new(ExprKind::Dict(entries), Span::new(start, close.span.end)))
    }

    fn parse_literal_dict(&mut self, start: usize) -> Result<Expr, EvalError> {
        let mut entries = Vec::new();
        if let Some(close) = self.take(|kind| matches!(kind, TokenKind::RBrace)) {
            return Ok(Expr::new(ExprKind::Dict(entries), Span::new(start, close.span.end)));
        }
        loop {
            // `get_literal_key` (eval.c:4458-4472) scans raw bytes, not tokens:
            // the key is a run of ASCII alphanumerics, `_` and `-`, and white
            // space may follow it before the colon.
            let key_start = self.current().span.start;
            let mut key_end = key_start;
            while self
                .source
                .get(key_end)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                key_end += 1;
            }
            if key_end == key_start {
                // The key is not a literal key at all, so upstream abandons the
                // dictionary entirely and reports the whole expression as
                // invalid (`eval_dict` FAIL -> `e_invexpr2`, eval.c:4512-4514).
                let rest = String::from_utf8_lossy(&self.source[start..]);
                return Err(EvalError::new("E15", start, format!("Invalid expression: \"{rest}\"")));
            }
            let key = Expr::new(
                ExprKind::Literal(Typval::String(OxStr(self.source[key_start..key_end].to_vec()))),
                Span::new(key_start, key_end),
            );
            let mut colon = key_end;
            while matches!(self.source.get(colon), Some(b' ' | b'\t')) {
                colon += 1;
            }
            if self.source.get(colon) != Some(&b':') {
                let rest = String::from_utf8_lossy(&self.source[colon..]);
                return Err(EvalError::new("E720", colon, format!("Missing colon in Dictionary: {rest}")));
            }
            while self.current().span.start < colon && !matches!(self.current().kind, TokenKind::Eof) {
                self.advance();
            }
            self.require(|kind| matches!(kind, TokenKind::Colon), "E720", "missing ':' in dictionary")?;
            let value = self.parse_expr1()?;
            entries.push((key, value));
            if self.take(|kind| matches!(kind, TokenKind::Comma)).is_none() {
                break;
            }
            if matches!(self.current().kind, TokenKind::RBrace) {
                break;
            }
        }
        let close = self.require(|kind| matches!(kind, TokenKind::RBrace), "E723", "missing '}'" )?;
        Ok(Expr::new(ExprKind::Dict(entries), Span::new(start, close.span.end)))
    }

    fn parse_lambda(&mut self, start: usize) -> Result<Expr, EvalError> {
        let mut params = Vec::new();
        let mut varargs = false;
        if !matches!(self.current().kind, TokenKind::Arrow) {
            loop {
                // The variadic form `...` ends the parameter list; it accepts
                // any number of extra arguments (a:0 / a:000 / a:1 ...).
                if self.take(|kind| matches!(kind, TokenKind::DotDotDot)).is_some() {
                    varargs = true;
                    break;
                }
                let token = self.advance().clone();
                let TokenKind::Identifier(name) = token.kind else {
                    return Err(EvalError::new("E451", token.span.start, "lambda argument name expected"));
                };
                if name.contains(&b':') {
                    return Err(EvalError::new("E451", token.span.start, "lambda arguments must be unscoped"));
                }
                if params.iter().any(|existing: &OxStr| existing.as_bytes() == name.as_slice()) {
                    return Err(EvalError::new(
                        "E853",
                        token.span.start,
                        format!(
                            "Duplicate argument name: {}",
                            String::from_utf8_lossy(&name)
                        ),
                    ));
                }
                params.push(OxStr(name));
                if self.take(|kind| matches!(kind, TokenKind::Comma)).is_none() {
                    break;
                }
            }
        }
        self.require(|kind| matches!(kind, TokenKind::Arrow), "E451", "missing '->' in lambda")?;
        let body = self.parse_expr1()?;
        let close = self.require(|kind| matches!(kind, TokenKind::RBrace), "E451", "missing '}' after lambda")?;
        Ok(Expr::new(
            ExprKind::Lambda { params, varargs, body: Box::new(body) },
            Span::new(start, close.span.end),
        ))
    }

    fn parse_subscript(&mut self, target: Expr) -> Result<Expr, EvalError> {
        let start_span = target.span;
        if let Some(close) = self.take(|kind| matches!(kind, TokenKind::RBracket)) {
            return Ok(Expr::new(
                ExprKind::Slice { target: Box::new(target), start: None, end: None },
                Span::new(start_span.start, close.span.end),
            ));
        }
        let first = if matches!(self.current().kind, TokenKind::Colon) {
            None
        } else {
            Some(Box::new(self.parse_expr1()?))
        };
        if self.take(|kind| matches!(kind, TokenKind::Colon)).is_some() {
            let end = if matches!(self.current().kind, TokenKind::RBracket) {
                None
            } else {
                Some(Box::new(self.parse_expr1()?))
            };
            let close = self.require(|kind| matches!(kind, TokenKind::RBracket), "E111", "missing ']' after slice")?;
            return Ok(Expr::new(
                ExprKind::Slice { target: Box::new(target), start: first, end },
                Span::new(start_span.start, close.span.end),
            ));
        }
        let Some(index) = first else {
            return Err(EvalError::new("E111", self.current().span.start, "subscript expression expected"));
        };
        let close = self.require(|kind| matches!(kind, TokenKind::RBracket), "E111", "missing ']' after subscript")?;
        Ok(Expr::new(
            ExprKind::Index { target: Box::new(target), index },
            Span::new(start_span.start, close.span.end),
        ))
    }

    fn parse_method_call(&mut self, receiver: Expr) -> Result<Expr, EvalError> {
        let arrow_end = self.previous_span().end;
        // `eval_method` (eval.c:2996-3016) does not skip white space before the
        // method name, so a gap after `->` leaves the rest of the expression
        // unparsed and the caller reports the remainder as invalid rather than
        // complaining about the arrow.
        // Tested on the raw byte rather than the next token span, so a trailing
        // `-> ` at the end of the source is caught too.
        if matches!(self.source.get(arrow_end), Some(b' ' | b'\t')) {
            let rest = String::from_utf8_lossy(&self.source[arrow_end..]);
            return Err(EvalError::new("E15", arrow_end, format!("Invalid expression: \"{rest}\"")));
        }
        let is_lambda = matches!(self.current().kind, TokenKind::LBrace);
        let method = if is_lambda {
            let open = self.advance().clone();
            if !self.brace_is_lambda() {
                return Err(EvalError::new("E260", open.span.start, "Missing name after ->"));
            }
            self.parse_lambda(open.span.start)?
        } else {
            let token = self.advance().clone();
            let TokenKind::Identifier(name) = token.kind else {
                return Err(EvalError::new("E260", token.span.start, "Missing name after ->"));
            };
            Expr::new(ExprKind::Variable(OxStr(name)), token.span)
        };
        if !matches!(self.current().kind, TokenKind::LParen) {
            // `e_missingparen` is "E107: Missing parentheses: %s" (errors.h:131);
            // `eval_lambda` passes the literal "lambda" for the `{...}` form.
            let name = if is_lambda {
                "lambda".to_owned()
            } else {
                String::from_utf8_lossy(&self.source[method.span.start..method.span.end]).into_owned()
            };
            return Err(EvalError::new("E107", method.span.start, format!("Missing parentheses: {name}")));
        }
        let open = self.advance().clone();
        if method.span.end != open.span.start {
            return Err(EvalError::new("E274", open.span.start, "No white space allowed before parenthesis"));
        }
        let (args, end) = self.parse_arguments()?;
        let span = Span::new(receiver.span.start, end);
        Ok(Expr::new(
            ExprKind::MethodCall { receiver: Box::new(receiver), method: Box::new(method), args },
            span,
        ))
    }

    fn parse_arguments(&mut self) -> Result<(Vec<Expr>, usize), EvalError> {
        let mut args = Vec::new();
        if let Some(close) = self.take(|kind| matches!(kind, TokenKind::RParen)) {
            return Ok((args, close.span.end));
        }
        loop {
            args.push(self.parse_expr1()?);
            if self.take(|kind| matches!(kind, TokenKind::Comma)).is_none() {
                break;
            }
            if matches!(self.current().kind, TokenKind::RParen) {
                break;
            }
        }
        let close = self.require(|kind| matches!(kind, TokenKind::RParen), "E116", "missing ')' after arguments")?;
        Ok((args, close.span.end))
    }

    fn brace_is_lambda(&self) -> bool {
        let mut index = self.cursor;
        let mut nested = 0_usize;
        loop {
            let Some(token) = self.tokens.get(index) else { return false };
            match token.kind {
                TokenKind::Arrow if nested == 0 => return true,
                TokenKind::Colon if nested == 0 => return false,
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace | TokenKind::HashLBrace => nested += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace if nested > 0 => nested -= 1,
                TokenKind::RBrace | TokenKind::Eof if nested == 0 => return false,
                _ => {}
            }
            index += 1;
        }
    }

    fn take_comparison(&mut self) -> Option<(CompareOp, CaseSensitivity)> {
        let result = match self.current().kind {
            TokenKind::Equal(case) => Some((CompareOp::Equal, case)),
            TokenKind::NotEqual(case) => Some((CompareOp::NotEqual, case)),
            TokenKind::Greater(case) => Some((CompareOp::Greater, case)),
            TokenKind::GreaterEqual(case) => Some((CompareOp::GreaterEqual, case)),
            TokenKind::Less(case) => Some((CompareOp::Less, case)),
            TokenKind::LessEqual(case) => Some((CompareOp::LessEqual, case)),
            TokenKind::Match(case) => Some((CompareOp::Match, case)),
            TokenKind::NoMatch(case) => Some((CompareOp::NoMatch, case)),
            TokenKind::Is(case) => Some((CompareOp::Is, case)),
            TokenKind::IsNot(case) => Some((CompareOp::IsNot, case)),
            _ => None,
        };
        if result.is_some() {
            self.cursor += 1;
        }
        result
    }

    fn current(&self) -> &Token {
        if let Some(token) = self.tokens.get(self.cursor) {
            token
        } else if let Some(token) = self.tokens.last() {
            token
        } else {
            &FALLBACK_EOF
        }
    }

    fn is_adjacent_member(&self, left_end: usize) -> bool {
        if !matches!(self.current().kind, TokenKind::Dot) || left_end != self.current().span.start {
            return false;
        }
        match self.tokens.get(self.cursor + 1) {
            Some(next) => {
                self.current().span.end == next.span.start
                    && matches!(next.kind, TokenKind::Identifier(_) | TokenKind::Integer(_))
            }
            None => false,
        }
    }

    fn advance(&mut self) -> &Token {
        let index = self.cursor;
        if self.cursor + 1 < self.tokens.len() {
            self.cursor += 1;
        }
        if let Some(token) = self.tokens.get(index) {
            token
        } else {
            &FALLBACK_EOF
        }
    }

    fn previous_span(&self) -> Span {
        self.tokens.get(self.cursor.saturating_sub(1)).map_or(Span::default(), |token| token.span)
    }

    fn take(&mut self, predicate: impl FnOnce(&TokenKind) -> bool) -> Option<Token> {
        if predicate(&self.current().kind) {
            let token = self.current().clone();
            self.cursor += 1;
            Some(token)
        } else {
            None
        }
    }

    fn require(
        &mut self,
        predicate: impl FnOnce(&TokenKind) -> bool,
        code: &'static str,
        message: &'static str,
    ) -> Result<Token, EvalError> {
        self.take(predicate)
            .ok_or_else(|| EvalError::new(code, self.current().span.start, message))
    }
}

fn is_scope_prefix(byte: u8) -> bool {
    matches!(byte, b'a' | b'b' | b'g' | b'l' | b's' | b't' | b'v' | b'w')
}

fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    let span = left.span.through(right.span);
    Expr::new(ExprKind::Binary { op, left: Box::new(left), right: Box::new(right) }, span)
}
