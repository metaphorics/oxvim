#![forbid(unsafe_code)]
//! Vim-compatible regular-expression parsing and matching.
//!
//! Automatic compilation follows Neovim's NFA-first policy (`regexp.c:16135-16167`),
//! while routing patterns that require this crate's recursive backreference or
//! variable-width lookbehind support directly to the bounded backtracking engine.
//! Unicode case-insensitive matching uses `char::to_lowercase`; unusual full-fold
//! edges may differ from Neovim's utf8proc-backed folding.

mod bt;
mod nfa;
mod parser;

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use parser::Parser;
use thiserror::Error;

const DEFAULT_STEP_LIMIT: usize = 1_000_000;
const DEFAULT_DEPTH_LIMIT: usize = 1_024;

/// Initial interpretation of punctuation in a Vim pattern.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Magic {
    /// All non-alphanumeric ASCII punctuation is special unless escaped.
    VeryMagic,
    /// Vim's normal `'magic'` interpretation.
    #[default]
    Magic,
    /// Vim's normal `'nomagic'` interpretation.
    NoMagic,
    /// Only escaped punctuation is special.
    VeryNoMagic,
}

/// Compiled execution strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Engine {
    /// Thompson instruction program executed with ordered Pike threads.
    Nfa,
    /// Explicitly bounded recursive backtracking.
    Backtracking,
}

/// A parsed and engine-selected Vim regular expression.
#[derive(Clone, Debug)]
pub struct Prog {
    expr: parser::Expr,
    capture_count: usize,
    ignore_case: bool,
    engine: Engine,
    /// The pattern contains `\N`: two NFA threads at one (pc, pos) can
    /// carry different captures and diverge again later, so the NFA's
    /// visited-set dedup must compare capture vectors (see `nfa`).
    has_backref: bool,
    step_limit: usize,
    depth_limit: usize,
}

impl Prog {
    /// Returns the engine selected when the pattern was compiled.
    #[must_use]
    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// Returns the number of capturing groups in the pattern.
    #[must_use]
    pub fn capture_count(&self) -> usize {
        self.capture_count
    }

    /// Replaces the execution step and recursion limits.
    #[must_use]
    pub fn with_limits(mut self, step_limit: usize, depth_limit: usize) -> Self {
        self.step_limit = step_limit;
        self.depth_limit = depth_limit;
        self
    }
}

/// Failure to parse or compile a Vim pattern.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum CompileError {
    /// The grammar is malformed at the given byte offset.
    #[error("regex syntax error at byte {offset}: {message}")]
    Syntax {
        /// Pattern byte offset at which parsing failed.
        offset: usize,
        /// Stable description of the grammar failure.
        message: &'static str,
    },
    /// A recognized escape prefix was followed by an unsupported code.
    #[error("invalid escape \\{escape} at byte {offset}")]
    InvalidEscape {
        /// Pattern byte offset at which the escape starts.
        offset: usize,
        /// Unsupported escaped character.
        escape: char,
    },
    /// The `\%#=` engine prefix contains a value other than zero, one, or two.
    #[error("invalid engine selector; expected \\%#=0, \\%#=1, or \\%#=2")]
    InvalidEngine,
}

/// Bounded matcher failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ExecError {
    /// The configured transition budget was exhausted.
    #[error("regular-expression step limit exceeded")]
    StepLimit,
    /// The configured recursive backtracking depth was exhausted.
    #[error("regular-expression recursion limit exceeded")]
    RecursionLimit,
}

/// A location in the multiline input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Position {
    /// One-based line number.
    pub lnum: usize,
    /// Zero-based byte column within the line.
    pub col: usize,
    /// Zero-based byte offset within the complete input.
    pub byte: usize,
}

/// The byte-precise extent of one capturing group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capture {
    /// Inclusive capture start position.
    pub start: Position,
    /// Exclusive capture end position.
    pub end: Position,
}

/// A complete match and its explicit capturing groups.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Match {
    /// Inclusive match start, adjusted by the last matching `\zs`.
    pub start: Position,
    /// Exclusive match end, adjusted by the last matching `\ze`.
    pub end: Position,
    /// Captures one through nine; unmatched groups are `None`.
    pub captures: Vec<Option<Capture>>,
}

/// UTF-8 multiline search input plus optional editor position context.
#[derive(Clone, Debug, Default)]
pub struct Text {
    bytes: String,
    line_starts: Vec<usize>,
    cursor: Option<usize>,
    visual: Option<RangeInclusive<usize>>,
    marks: BTreeMap<char, usize>,
}

impl Text {
    /// Creates text from a string whose newline bytes delimit buffer lines.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        let bytes = text.into();
        let mut line_starts = vec![0];
        for (offset, byte) in bytes.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(offset + 1);
            }
        }
        Self {
            bytes,
            line_starts,
            cursor: None,
            visual: None,
            marks: BTreeMap::new(),
        }
    }

    /// Creates text from buffer lines, inserting one newline between adjacent lines.
    pub fn from_lines<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut bytes = String::new();
        for (index, line) in lines.into_iter().enumerate() {
            if index != 0 {
                bytes.push('\n');
            }
            bytes.push_str(line.as_ref());
        }
        Self::new(bytes)
    }

    /// Returns the flattened UTF-8 input.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.bytes
    }

    /// Returns the flattened input length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether the flattened input is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns the number of logical buffer lines.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Sets the byte position tested by the `\%#` cursor assertion.
    #[must_use]
    pub fn with_cursor(mut self, byte: usize) -> Self {
        self.cursor = self.valid_boundary(byte).then_some(byte);
        self
    }

    /// Sets the inclusive byte range tested by the `\%V` visual assertion.
    #[must_use]
    pub fn with_visual(mut self, start: usize, end: usize) -> Self {
        self.visual = (start <= end && self.valid_boundary(start) && self.valid_boundary(end))
            .then_some(start..=end);
        self
    }

    /// Sets a named mark byte position tested by `\%'m`.
    #[must_use]
    pub fn with_mark(mut self, name: char, byte: usize) -> Self {
        if self.valid_boundary(byte) {
            self.marks.insert(name, byte);
        }
        self
    }

    /// Converts a UTF-8 boundary byte offset into line and byte-column coordinates.
    #[must_use]
    pub fn position(&self, byte: usize) -> Option<Position> {
        if !self.valid_boundary(byte) {
            return None;
        }
        let line = self
            .line_starts
            .partition_point(|start| *start <= byte)
            .saturating_sub(1);
        Some(Position {
            lnum: line + 1,
            col: byte - self.line_starts[line],
            byte,
        })
    }

    fn valid_boundary(&self, byte: usize) -> bool {
        byte <= self.bytes.len() && self.bytes.is_char_boundary(byte)
    }

    fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    fn visual_contains(&self, byte: usize) -> bool {
        self.visual
            .as_ref()
            .is_some_and(|range| range.contains(&byte))
    }

    fn mark(&self, name: char) -> Option<usize> {
        self.marks.get(&name).copied()
    }
}

impl From<&str> for Text {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Parses a Vim pattern and selects its execution engine.
///
/// # Errors
///
/// Returns [`CompileError::InvalidEngine`] for a malformed `\%#=` engine
/// selector and the parser's [`CompileError`] (invalid escape, unterminated
/// group or collection, backref before a closed group, …) for a
/// syntactically invalid pattern.
pub fn compile(pattern: &str, magic: Magic) -> Result<Prog, CompileError> {
    let (selection, body) = parse_engine_selector(pattern)?;
    let parsed = Parser::new(body, magic).parse()?;
    let complex =
        parsed.features.backref || parsed.features.lookbehind || parsed.features.complex_repeat;
    let engine = match selection {
        Some(engine) => engine,
        None if complex => Engine::Backtracking,
        None => Engine::Nfa,
    };
    Ok(Prog {
        expr: parsed.expr,
        capture_count: parsed.captures,
        ignore_case: parsed.ignore_case,
        engine,
        has_backref: parsed.features.backref,
        step_limit: DEFAULT_STEP_LIMIT,
        depth_limit: DEFAULT_DEPTH_LIMIT,
    })
}

/// Searches the complete input, suppressing a bounded-execution error as no match.
#[must_use]
pub fn exec(prog: &Prog, text: &Text) -> Option<Match> {
    try_exec(prog, text).ok().flatten()
}

/// Searches at or after `start`, suppressing a bounded-execution error as no match.
#[must_use]
pub fn exec_at(prog: &Prog, text: &Text, start: Position) -> Option<Match> {
    try_exec_at(prog, text, start).ok().flatten()
}

/// Searches the complete input and reports bounded-execution errors.
///
/// # Errors
///
/// Propagates [`Self::try_exec_at`]'s [`ExecError`] — the step or depth
/// limit exhausted while either engine executes the program.
pub fn try_exec(prog: &Prog, text: &Text) -> Result<Option<Match>, ExecError> {
    let start = Position {
        lnum: 1,
        col: 0,
        byte: 0,
    };
    try_exec_at(prog, text, start)
}

/// Searches at or after `start` and reports bounded-execution errors.
///
/// # Errors
///
/// Returns [`ExecError`] when the selected engine exhausts the program's
/// step or depth limit while searching; a plain no-match is `Ok(None)`.
pub fn try_exec_at(prog: &Prog, text: &Text, start: Position) -> Result<Option<Match>, ExecError> {
    if text.position(start.byte) != Some(start) {
        return Ok(None);
    }
    let raw = match prog.engine {
        Engine::Backtracking => bt::search(prog, text, start.byte)?,
        Engine::Nfa => nfa::search(prog, text, start.byte)?,
    };
    Ok(raw.map(|state| state.into_match(text, prog.capture_count)))
}

fn parse_engine_selector(pattern: &str) -> Result<(Option<Engine>, &str), CompileError> {
    if !pattern.starts_with("\\%#=") {
        return Ok((None, pattern));
    }
    if pattern.len() < 5 {
        return Err(CompileError::InvalidEngine);
    }
    match pattern.as_bytes()[4] {
        b'0' => Ok((None, &pattern[5..])),
        b'1' => Ok((Some(Engine::Backtracking), &pattern[5..])),
        b'2' => Ok((Some(Engine::Nfa), &pattern[5..])),
        _ => Err(CompileError::InvalidEngine),
    }
}
