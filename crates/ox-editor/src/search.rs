//! Buffer search backed by `ox-regex` (`search.c`).

use ox_regex::{CompileError, ExecError, Magic, Text};
use ox_text::Position;
use thiserror::Error;

/// Direction of a buffer search.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchDirection {
    /// Find matches after the cursor.
    Forward,
    /// Find matches before the cursor.
    Backward,
}

/// Parsed Vim search offset.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchOffset {
    /// Place the cursor on the inclusive match end.
    pub use_end: bool,
    /// Move this many characters from the selected match position.
    pub character_delta: isize,
    /// Move this many logical lines after choosing the match.
    pub line_delta: isize,
}

/// A resolved search match and count metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    /// Cursor destination after offsets.
    pub target: Position,
    /// One-based match index in buffer order.
    pub ordinal: usize,
    /// Total match count.
    pub total: usize,
    /// Whether selecting this match crossed an end of the buffer.
    pub wrapped: bool,
}

/// Search compilation, execution, and lookup failures.
#[derive(Debug, Error)]
pub enum SearchError {
    /// Pattern compilation failed.
    #[error(transparent)] Compile(#[from] CompileError),
    /// Bounded regex execution failed.
    #[error(transparent)] Execute(#[from] ExecError),
    /// No match exists in the permitted search span.
    #[error("E486: Pattern not found: {0}")]
    PatternNotFound(String),
    /// Repeat was requested before any successful search.
    #[error("no previous search pattern")]
    NoPreviousPattern,
    /// Buffer bytes cannot be adapted to the regex text model.
    #[error("search input is not UTF-8")]
    InvalidUtf8,
}

/// Last-pattern state used by empty searches and `n`/`N`.
#[derive(Clone, Debug, Default)]
pub struct SearchState { last_pattern: Option<String>, last_direction: Option<SearchDirection>, last_offset: SearchOffset }

impl SearchState {
    /// Returns the retained pattern, if any.
    #[must_use] pub fn last_pattern(&self) -> Option<&str> { self.last_pattern.as_deref() }

    /// Executes and retains a new search expression.
    pub fn search(&mut self, lines: &[Vec<u8>], cursor: Position, expression: &str, direction: SearchDirection, count: usize, wrapscan: bool) -> Result<SearchResult, SearchError> {
        let (pattern, offset) = parse_expression(expression, direction);
        let pattern = if pattern.is_empty() { self.last_pattern.clone().ok_or(SearchError::NoPreviousPattern)? } else { pattern.to_owned() };
        let result = run(lines, cursor, &pattern, direction, offset, count, wrapscan)?;
        self.last_pattern = Some(pattern); self.last_direction = Some(direction); self.last_offset = offset;
        Ok(result)
    }

    /// Repeats the retained search, optionally in the opposite direction.
    pub fn repeat(&self, lines: &[Vec<u8>], cursor: Position, opposite: bool, count: usize, wrapscan: bool) -> Result<SearchResult, SearchError> {
        let pattern = self.last_pattern.as_deref().ok_or(SearchError::NoPreviousPattern)?;
        let mut direction = self.last_direction.ok_or(SearchError::NoPreviousPattern)?;
        if opposite { direction = match direction { SearchDirection::Forward => SearchDirection::Backward, SearchDirection::Backward => SearchDirection::Forward }; }
        run(lines, cursor, pattern, direction, self.last_offset, count, wrapscan)
    }
}

fn parse_expression(expression: &str, direction: SearchDirection) -> (&str, SearchOffset) {
    let delimiter = match direction { SearchDirection::Forward => '/', SearchDirection::Backward => '?' };
    let Some(slash) = rfind_unescaped(expression, delimiter) else { return (expression, SearchOffset::default()); };
    let suffix = &expression[slash + 1..];
    if suffix.is_empty() { return (expression, SearchOffset::default()); }
    let (use_end, number) = if let Some(rest) = suffix.strip_prefix('e') { (true, rest) } else { (false, suffix) };
    if !number.is_empty() && number.parse::<isize>().is_err() { return (expression, SearchOffset::default()); }
    let delta = if number.is_empty() { 0 } else { number.parse().map_or(0, |value| value) };
    let offset = if use_end { SearchOffset { use_end: true, character_delta: delta, line_delta: 0 } } else { SearchOffset { use_end: false, character_delta: 0, line_delta: delta } };
    (expression.get(..slash).map_or(expression, |pattern| pattern), offset)
}

fn rfind_unescaped(expression: &str, delimiter: char) -> Option<usize> {
    expression.char_indices().rev().find_map(|(index, ch)| {
        if ch != delimiter { return None; }
        let escapes = expression.as_bytes()[..index].iter().rev().take_while(|byte| **byte == b'\\').count();
        (escapes % 2 == 0).then_some(index)
    })
}

fn run(lines: &[Vec<u8>], cursor: Position, pattern: &str, direction: SearchDirection, offset: SearchOffset, count: usize, wrapscan: bool) -> Result<SearchResult, SearchError> {
    let strings = lines.iter().map(|line| std::str::from_utf8(line).map(str::to_owned).map_err(|_| SearchError::InvalidUtf8)).collect::<Result<Vec<_>, _>>()?;
    let text = Text::from_lines(strings.iter().map(String::as_str));
    let prog = ox_regex::compile(pattern, Magic::Magic)?;
    let mut matches = Vec::new(); let mut start = ox_regex::Position { lnum: 1, col: 0, byte: 0 };
    while let Some(found) = ox_regex::try_exec_at(&prog, &text, start)? {
        let next = found.end.byte.max(found.start.byte.saturating_add(1));
        matches.push(found);
        if next >= text.len() { break; }
        let Some(position) = text.position(next) else { break; }; start = position;
    }
    if matches.is_empty() { return Err(SearchError::PatternNotFound(pattern.to_owned())); }
    let cursor_byte = byte_of(lines, cursor);
    let eligible = |index: usize| match direction { SearchDirection::Forward => matches[index].start.byte > cursor_byte, SearchDirection::Backward => matches[index].start.byte < cursor_byte };
    let mut index = match direction { SearchDirection::Forward => (0..matches.len()).find(|index| eligible(*index)), SearchDirection::Backward => (0..matches.len()).rev().find(|index| eligible(*index)) };
    let mut wrapped = false;
    if index.is_none() && wrapscan { wrapped = true; index = Some(match direction { SearchDirection::Forward => 0, SearchDirection::Backward => matches.len() - 1 }); }
    let Some(mut index) = index else { return Err(SearchError::PatternNotFound(pattern.to_owned())); };
    for _ in 1..count.max(1) {
        match direction {
            SearchDirection::Forward if index + 1 < matches.len() => index += 1,
            SearchDirection::Backward if index > 0 => index -= 1,
            SearchDirection::Forward if wrapscan => { index = 0; wrapped = true; }
            SearchDirection::Backward if wrapscan => { index = matches.len() - 1; wrapped = true; }
            _ => return Err(SearchError::PatternNotFound(pattern.to_owned())),
        }
    }
    let selected = &matches[index];
    let mut base_byte = if offset.use_end { previous_boundary(text.as_str(), selected.end.byte) } else { selected.start.byte };
    base_byte = shift_characters(text.as_str(), base_byte, offset.character_delta);
    let base = text.position(base_byte).map_or(selected.start, |position| position);
    let target_line = base.lnum.saturating_add_signed(offset.line_delta).clamp(1, lines.len().max(1));
    let target = Position { lnum: target_line, col: if offset.line_delta == 0 { base.col.min(lines[target_line - 1].len().saturating_sub(1)) } else { 0 } };
    Ok(SearchResult { target, ordinal: index + 1, total: matches.len(), wrapped })
}

fn byte_of(lines: &[Vec<u8>], pos: Position) -> usize { lines.iter().take(pos.lnum.saturating_sub(1)).map(|line| line.len().saturating_add(1)).sum::<usize>().saturating_add(pos.col) }

fn previous_boundary(text: &str, exclusive: usize) -> usize {
    let mut byte = exclusive.saturating_sub(1).min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) { byte -= 1; }
    byte
}

fn shift_characters(text: &str, start: usize, delta: isize) -> usize {
    let mut byte = start.min(text.len());
    if delta >= 0 {
        for _ in 0..delta as usize {
            if byte >= text.len() { break; }
            byte += text[byte..].chars().next().map_or(0, char::len_utf8);
        }
    } else {
        for _ in 0..delta.unsigned_abs() {
            if byte == 0 { break; }
            byte = previous_boundary(text, byte);
        }
    }
    byte.min(text.len().saturating_sub(usize::from(!text.is_empty())))
}
