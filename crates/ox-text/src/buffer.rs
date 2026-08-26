//! Rope-backed text storage with Vim's one-based, line-oriented contract.
//!
//! In memory, each logical line excludes its line break, matching `ml_get()`.
//! [`Buffer::to_bytes`] restores `\n` separators and emits a final `\n` only
//! when the buffer has end-of-line state.

use ropey::Rope;
use thiserror::Error;

/// Errors produced by line-oriented buffer operations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BufferError {
    /// A one-based line number or inclusive range was outside the buffer.
    #[error("line range {start}..={end} is outside 1..={line_count}")]
    LineRange {
        /// First requested line.
        start: usize,
        /// Last requested line.
        end: usize,
        /// Current logical line count.
        line_count: usize,
    },
    /// An input line contained a line separator.
    #[error("a logical line must not contain a newline")]
    NewlineInLine,
    /// Input bytes were not UTF-8 and cannot be represented by `ropey`.
    #[error("rope text must be valid UTF-8")]
    InvalidUtf8,
    /// A byte offset was outside the serialized buffer.
    #[error("byte offset {offset} is outside 0..={byte_len}")]
    ByteOffset {
        /// Requested byte offset.
        offset: usize,
        /// Serialized buffer length.
        byte_len: usize,
    },
    /// A byte offset split a UTF-8 code point.
    #[error("byte offset {0} is not a UTF-8 boundary")]
    NotCharBoundary(usize),
    /// A batch of splices was not strictly ascending and row-disjoint.
    #[error("batch splices must be ascending and row-disjoint")]
    OverlappingSplices,
}

/// A rope-backed Vim buffer.
#[derive(Clone, Debug)]
pub struct Buffer {
    rope: Rope,
    has_eol: bool,
    changedtick: u64,
}

/// One inclusive row replacement in a disjoint batch.
pub struct LineSplice<'a> {
    /// First one-based line.
    pub start: usize,
    /// Last one-based line.
    pub end: usize,
    /// Replacement logical lines, borrowed for the duration of the batch.
    pub lines: &'a [Vec<u8>],
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    /// Creates an empty, one-line buffer without a final line break.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rope: Rope::new(),
            has_eol: false,
            changedtick: 0,
        }
    }

    /// Loads serialized UTF-8 buffer bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BufferError> {
        let text = std::str::from_utf8(bytes).map_err(|_| BufferError::InvalidUtf8)?;
        Ok(Self {
            rope: Rope::from_str(text),
            has_eol: text.ends_with('\n'),
            changedtick: 0,
        })
    }

    /// Builds a buffer from newline-free logical lines.
    pub fn from_lines(lines: &[Vec<u8>], has_eol: bool) -> Result<Self, BufferError> {
        validate_lines(lines)?;
        let normalized: &[Vec<u8>] = if lines.is_empty() { &[Vec::new()] } else { lines };
        let mut bytes = Vec::new();
        for (index, line) in normalized.iter().enumerate() {
            if index != 0 {
                bytes.push(b'\n');
            }
            bytes.extend_from_slice(line);
        }
        if has_eol {
            bytes.push(b'\n');
        }
        Self::from_bytes(&bytes)
    }

    /// Returns the number of logical lines. It is always at least one.
    #[must_use]
    pub fn line_count(&self) -> usize {
        let rope_lines = self.rope.len_lines();
        if self.has_eol && rope_lines > 1 {
            rope_lines - 1
        } else {
            rope_lines
        }
    }

    /// Returns whether serialized text ends in a line break.
    #[must_use]
    pub const fn has_eol(&self) -> bool {
        self.has_eol
    }

    /// Returns the current change counter.
    #[must_use]
    pub const fn changedtick(&self) -> u64 {
        self.changedtick
    }

    /// Returns one logical line without its line break.
    pub fn line(&self, lnum: usize) -> Result<Vec<u8>, BufferError> {
        self.check_line(lnum)?;
        let slice = self.rope.line(lnum - 1);
        let mut bytes = slice.to_string().into_bytes();
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        Ok(bytes)
    }

    /// Serializes the buffer using line-feed separators.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.rope.to_string().into_bytes()
    }

    /// Returns the zero-based serialized byte offset of a one-based line.
    ///
    /// Accepts one past the logical line count, the EOF pseudo-line, which
    /// maps to the full serialized byte length. A final line without a line
    /// break contributes no terminator, so that length numerically subtracts
    /// the absent `\n` (memline.c:4078-4162). Any higher line is rejected.
    pub fn byte_of_line(&self, lnum: usize) -> Result<usize, BufferError> {
        let line_count = self.line_count();
        if lnum == 0 || lnum > line_count + 1 {
            return Err(BufferError::LineRange {
                start: lnum,
                end: lnum,
                line_count,
            });
        }
        if lnum == line_count + 1 {
            // EOF pseudo-line: the whole serialized stream. A noeol final
            // line is simply absent from the rope, so `len_bytes` equals the
            // serialized length without the missing terminator.
            return Ok(self.rope.len_bytes());
        }
        Ok(self.rope.line_to_byte(lnum - 1))
    }

    /// Returns the one-based line containing a serialized byte offset.
    ///
    /// Any in-range offset maps to a line; an offset that splits a UTF-8 code
    /// point resolves to the line containing that code point, because `byte_to_line`
    /// counts single-byte line breaks rather than requiring a char boundary
    /// (memline.c:4078-4141).
    pub fn lnum_of_byte(&self, offset: usize) -> Result<usize, BufferError> {
        let len = self.rope.len_bytes();
        if offset > len {
            return Err(BufferError::ByteOffset {
                offset,
                byte_len: len,
            });
        }
        if offset == len && self.has_eol {
            // Past the trailing line break: the last logical line.
            return Ok(self.line_count());
        }
        Ok(self.rope.byte_to_line(offset) + 1)
    }

    /// Replaces the inclusive line range with newline-free logical lines.
    ///
    /// An empty replacement deletes the range. Deleting every line leaves the
    /// canonical empty Vim buffer: one empty line without end-of-line state.
    ///
    /// The rope is spliced over the replaced span only, so the cost tracks the
    /// edited region instead of the buffer size.
    pub fn replace_lines(
        &mut self,
        start: usize,
        end: usize,
        lines: &[Vec<u8>],
    ) -> Result<(), BufferError> {
        self.check_range(start, end)?;
        validate_lines(lines)?;
        self.splice_span(start, end, lines)?;
        self.changedtick = self.changedtick.wrapping_add(1);
        Ok(())
    }

    /// Applies every splice against the pre-edit line map, then mutates
    /// bottom-up so earlier row numbers stay stable. A non-empty batch bumps
    /// [`changedtick`](Self::changedtick) once; an empty slice is a no-op.
    pub fn replace_lines_disjoint(
        &mut self,
        splices: &[LineSplice<'_>],
    ) -> Result<(), BufferError> {
        let mut prev_end = None;
        for splice in splices {
            self.check_range(splice.start, splice.end)?;
            validate_lines(splice.lines)?;
            if prev_end.is_some_and(|prev| splice.start <= prev) {
                return Err(BufferError::OverlappingSplices);
            }
            prev_end = Some(splice.end);
        }
        for splice in splices.iter().rev() {
            self.splice_span(splice.start, splice.end, splice.lines)?;
        }
        if !splices.is_empty() {
            self.changedtick = self.changedtick.wrapping_add(1);
        }
        Ok(())
    }

    fn splice_span(
        &mut self,
        start: usize,
        end: usize,
        lines: &[Vec<u8>],
    ) -> Result<(), BufferError> {
        let line_count = self.line_count();
        let mut text = join_lines(lines)?;
        if !lines.is_empty() && (end < line_count || self.has_eol) {
            // The replacement is followed by more text, or by end-of-line
            // state, so it keeps a terminator of its own.
            text.push('\n');
        }
        let mut from = self.char_of_line(start);
        let to = self.char_of_line(end + 1);
        if lines.is_empty() && end == line_count {
            if start == 1 {
                self.has_eol = false;
            } else if !self.has_eol {
                // The final line carries no terminator of its own, so the
                // deletion has to swallow the one before `start` instead.
                from -= 1;
            }
        }
        self.rope.remove(from..to);
        if !text.is_empty() {
            self.rope.insert(from, &text);
        }
        Ok(())
    }

    /// Inserts logical lines after `lnum`; zero inserts before the first line.
    pub fn append_lines(&mut self, lnum: usize, lines: &[Vec<u8>]) -> Result<(), BufferError> {
        let line_count = self.line_count();
        if lnum > line_count {
            return Err(BufferError::LineRange {
                start: lnum,
                end: lnum,
                line_count,
            });
        }
        validate_lines(lines)?;
        if !lines.is_empty() {
            let mut text = join_lines(lines)?;
            if lnum == line_count && !self.has_eol {
                // Appending past an unterminated final line: the block brings
                // the separator that line lacks ahead of itself.
                text.insert(0, '\n');
            } else {
                text.push('\n');
            }
            let at = self.char_of_line(lnum + 1);
            self.rope.insert(at, &text);
        }
        self.changedtick = self.changedtick.wrapping_add(1);
        Ok(())
    }

    /// Deletes an inclusive range of logical lines.
    pub fn delete_lines(&mut self, start: usize, end: usize) -> Result<(), BufferError> {
        self.replace_lines(start, end, &[])
    }

    /// Changes final-EOL state and counts it as one mutation.
    pub fn set_eol(&mut self, has_eol: bool) {
        if self.has_eol != has_eol {
            let len = self.rope.len_chars();
            if has_eol {
                self.rope.insert(len, "\n");
            } else if len != 0 {
                self.rope.remove(len - 1..len);
            }
            self.has_eol = has_eol;
        }
        self.changedtick = self.changedtick.wrapping_add(1);
    }

    /// Returns the char offset at which one-based logical line `lnum` begins.
    ///
    /// One past the logical line count maps to the rope end, so an insertion
    /// after the final line and a deletion through it share one address space.
    fn char_of_line(&self, lnum: usize) -> usize {
        if lnum > self.rope.len_lines() {
            self.rope.len_chars()
        } else {
            self.rope.line_to_char(lnum - 1)
        }
    }

    fn check_line(&self, lnum: usize) -> Result<(), BufferError> {
        if lnum == 0 || lnum > self.line_count() {
            return Err(BufferError::LineRange {
                start: lnum,
                end: lnum,
                line_count: self.line_count(),
            });
        }
        Ok(())
    }

    fn check_range(&self, start: usize, end: usize) -> Result<(), BufferError> {
        if start == 0 || start > end || end > self.line_count() {
            return Err(BufferError::LineRange {
                start,
                end,
                line_count: self.line_count(),
            });
        }
        Ok(())
    }
}

fn validate_lines(lines: &[Vec<u8>]) -> Result<(), BufferError> {
    if lines.iter().any(|line| line.contains(&b'\n')) {
        return Err(BufferError::NewlineInLine);
    }
    if lines.iter().any(|line| std::str::from_utf8(line).is_err()) {
        return Err(BufferError::InvalidUtf8);
    }
    Ok(())
}

/// Joins validated newline-free logical lines with line-feed separators.
fn join_lines(lines: &[Vec<u8>]) -> Result<String, BufferError> {
    let mut text = String::with_capacity(lines.iter().map(|line| line.len() + 1).sum());
    for (index, line) in lines.iter().enumerate() {
        if index != 0 {
            text.push('\n');
        }
        text.push_str(std::str::from_utf8(line).map_err(|_| BufferError::InvalidUtf8)?);
    }
    Ok(text)
}
