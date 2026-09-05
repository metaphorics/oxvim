//! Buffer search backed by `ox-regex` (`search.c`).
//!
//! Selection stops at the first eligible match like `searchit`; count
//! metadata lives on the separate `searchcount()` scan, so [`SearchResult`]
//! carries no totals. The scan internals below are shared with the
//! `searchpair` and `searchcount` builtins so every caller gets the same
//! UTF-8-safe, multiline, zero-width match semantics.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::Instant;

use ox_regex::{CompileError, ExecError, Magic, Position as RegexPosition, Prog, Text};
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
    /// Whether a line offset form was parsed (`search.c` `off.line`), including `+0`/`-0`.
    pub has_line_offset: bool,
}

/// A resolved search match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    /// Cursor destination after offsets.
    pub target: Position,
    /// Whether the parsed offset anchors at the end of the match.
    pub use_end: bool,
    /// Parsed logical-line delta applied to the match position.
    pub line_delta: isize,
    /// Whether a line offset form was parsed (`search.c` `off.line`), including `+0`/`-0`.
    pub has_line_offset: bool,
    /// Whether selecting this match crossed an end of the buffer.
    pub wrapped: bool,
}

/// Search compilation, execution, and lookup failures.
#[derive(Debug, Error)]
pub enum SearchError {
    /// Pattern compilation failed.
    #[error(transparent)]
    Compile(#[from] CompileError),
    /// Bounded regex execution failed.
    #[error(transparent)]
    Execute(#[from] ExecError),
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
pub struct SearchState {
    pattern: Option<String>,
    direction: Option<SearchDirection>,
    offset: SearchOffset,
}

impl SearchState {
    /// Returns the retained pattern, if any.
    #[must_use]
    pub fn last_pattern(&self) -> Option<&str> {
        self.pattern.as_deref()
    }

    /// Executes and retains a new search expression.
    ///
    /// # Errors
    ///
    /// Returns an error when an empty expression has no retained pattern, the
    /// buffer is not valid UTF-8, regex compilation or execution fails, or no
    /// match exists in the permitted search span.
    pub fn search(
        &mut self,
        lines: &[Vec<u8>],
        cursor: Position,
        expression: &str,
        direction: SearchDirection,
        count: usize,
        wrapscan: bool,
    ) -> Result<SearchResult, SearchError> {
        let (pattern, offset) = parse_expression(expression, direction);
        let pattern = if pattern.is_empty() {
            self.pattern.clone().ok_or(SearchError::NoPreviousPattern)?
        } else {
            pattern.to_owned()
        };
        let result = run(lines, cursor, &pattern, direction, offset, count, wrapscan)?;
        self.pattern = Some(pattern);
        self.direction = Some(direction);
        self.offset = offset;
        Ok(result)
    }

    /// Repeats the retained search, optionally in the opposite direction.
    ///
    /// # Errors
    ///
    /// Returns an error when no search is retained, the buffer is not valid
    /// UTF-8, regex compilation or execution fails, or no match exists in the
    /// permitted search span.
    pub fn repeat(
        &self,
        lines: &[Vec<u8>],
        cursor: Position,
        opposite: bool,
        count: usize,
        wrapscan: bool,
    ) -> Result<SearchResult, SearchError> {
        let pattern = self
            .pattern
            .as_deref()
            .ok_or(SearchError::NoPreviousPattern)?;
        let mut direction = self.direction.ok_or(SearchError::NoPreviousPattern)?;
        if opposite {
            direction = match direction {
                SearchDirection::Forward => SearchDirection::Backward,
                SearchDirection::Backward => SearchDirection::Forward,
            };
        }
        run(
            lines,
            cursor,
            pattern,
            direction,
            self.offset,
            count,
            wrapscan,
        )
    }
}

fn parse_expression(expression: &str, direction: SearchDirection) -> (&str, SearchOffset) {
    let delimiter = match direction {
        SearchDirection::Forward => '/',
        SearchDirection::Backward => '?',
    };
    let Some(slash) = rfind_unescaped(expression, delimiter) else {
        return (expression, SearchOffset::default());
    };
    let suffix = &expression[slash + 1..];
    if suffix.is_empty() {
        return (expression, SearchOffset::default());
    }
    let (use_end, number) = if let Some(rest) = suffix.strip_prefix('e') {
        (true, rest)
    } else {
        (false, suffix)
    };
    if !number.is_empty() && number.parse::<isize>().is_err() {
        return (expression, SearchOffset::default());
    }
    let delta = if number.is_empty() {
        0
    } else {
        number.parse().unwrap_or(0)
    };
    let offset = if use_end {
        SearchOffset {
            use_end: true,
            character_delta: delta,
            line_delta: 0,
            has_line_offset: false,
        }
    } else {
        SearchOffset {
            use_end: false,
            character_delta: 0,
            line_delta: delta,
            has_line_offset: true,
        }
    };
    (expression.get(..slash).unwrap_or(expression), offset)
}

fn rfind_unescaped(expression: &str, delimiter: char) -> Option<usize> {
    expression.char_indices().rev().find_map(|(index, ch)| {
        if ch != delimiter {
            return None;
        }
        let escapes = expression.as_bytes()[..index]
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        (escapes % 2 == 0).then_some(index)
    })
}

fn run(
    lines: &[Vec<u8>],
    cursor: Position,
    pattern: &str,
    direction: SearchDirection,
    offset: SearchOffset,
    count: usize,
    wrapscan: bool,
) -> Result<SearchResult, SearchError> {
    let text = SearchText::new(lines)?;
    let prog = compile_search(pattern, Magic::Magic)?;
    let program = Program::Single(&prog);
    // Selection stops at the first eligible match like `searchit`
    // (`src/nvim/search.c:900`): the full-list walk survives below only
    // as the slow path for counts past one match cycle and backward
    // wraps. Totals live on the separate `searchcount()` scan, so
    // stopping early changes no reported field.
    let cursor_byte = text.byte_of(cursor);
    let count = count.max(1);
    let found = select_lazy(
        &text,
        program,
        cursor,
        cursor_byte,
        direction,
        count,
        wrapscan,
    )?;
    let (span, wrapped) = match found {
        Some(Select::Found { span, wrapped }) => (span, wrapped),
        Some(Select::NeedFull) => {
            let matches = CandidateScan::new(&text, SearchDirection::Forward, None, None)
                .scan_all(program)?;
            let (index, wrapped) =
                select_full_index(&matches, cursor_byte, direction, count, wrapscan, pattern)?;
            (matches[index].span, wrapped)
        }
        None => return Err(SearchError::PatternNotFound(pattern.to_owned())),
    };
    let selected = span;
    let mut base_byte = if offset.use_end {
        previous_boundary(text.as_str(), selected.end.byte)
    } else {
        selected.start.byte
    };
    base_byte = shift_characters(text.as_str(), base_byte, offset.character_delta);
    let base = position_of(&text, base_byte);
    let target_line = base
        .lnum
        .saturating_add_signed(offset.line_delta)
        .clamp(1, lines.len().max(1));
    let target = Position {
        lnum: target_line,
        col: if offset.line_delta == 0 {
            base.col.min(lines[target_line - 1].len().saturating_sub(1))
        } else {
            0
        },
    };
    Ok(SearchResult {
        target,
        use_end: offset.use_end,
        line_delta: offset.line_delta,
        has_line_offset: offset.has_line_offset,
        wrapped,
    })
}

/// Outcome of the lazy first-cycle selection.
enum Select {
    /// The count-th eligible match and whether reaching it wrapped.
    Found { span: MatchSpan, wrapped: bool },
    /// Matches exist but the count exceeds one full match cycle (forward)
    /// or a backward wrap needs the whole list: use [`select_full_index`].
    NeedFull,
}

/// Selects the count-th eligible match without collecting the list,
/// stopping as soon as the answer is known. Returns `None` when no match
/// exists at all. `count` is already normalized to at least one.
fn select_lazy(
    text: &SearchText,
    program: Program<'_>,
    cursor: Position,
    cursor_byte: usize,
    direction: SearchDirection,
    count: usize,
    wrapscan: bool,
) -> Result<Option<Select>, SearchError> {
    match direction {
        SearchDirection::Forward => {
            select_lazy_forward(text, program, cursor, cursor_byte, count, wrapscan)
        }
        SearchDirection::Backward => {
            select_lazy_backward(text, program, cursor_byte, count, wrapscan)
        }
    }
}

/// Forward lazy selection: steps from the cursor line, advancing past
/// each match end exactly like [`CandidateScan::scan_all`], and takes the
/// count-th match starting past the cursor. Same-line matches at or
/// before the cursor are skipped by eligibility; starting at the line
/// (not the cursor byte) keeps the wrapped sweep gapless. With `wrapscan`
/// the scan's own wrapped sweep replaces the index walk's wrap branch.
fn select_lazy_forward(
    text: &SearchText,
    program: Program<'_>,
    cursor: Position,
    cursor_byte: usize,
    count: usize,
    wrapscan: bool,
) -> Result<Option<Select>, SearchError> {
    let mut scan = CandidateScan::new(text, SearchDirection::Forward, None, None);
    if wrapscan {
        scan = scan.with_wrap();
    }
    let mut from = text.line_start(cursor.lnum.max(1));
    let mut yielded = 0usize;
    // Earliest same-line match at or before the cursor: the wrapped sweep
    // starts at the cursor line, so this window is never re-scanned. When
    // nothing else is yielded it is `matches[0]`, exactly what the
    // full-list walk wraps to.
    let mut first_skipped: Option<MatchSpan> = None;
    // End byte of the last counted match: the wrapped sweep advances past
    // match starts, so yields overlapping it are skipped to keep the public
    // after-end progression both sweeps must share.
    let mut last_end: Option<usize> = None;
    loop {
        match scan.step(program, from, true)? {
            Step::Found(candidate) => {
                from = scan.advance_after_end(&candidate);
                if scan.sweep == Sweep::First && candidate.span.start.byte <= cursor_byte {
                    if first_skipped.is_none() {
                        first_skipped = Some(candidate.span);
                    }
                    continue;
                }
                if last_end.is_some_and(|end| candidate.span.start.byte < end) {
                    continue;
                }
                yielded += 1;
                last_end = Some(candidate.span.end.byte);
                if yielded == count {
                    return Ok(Some(Select::Found {
                        span: candidate.span,
                        wrapped: scan.sweep == Sweep::Wrapped,
                    }));
                }
            }
            // Without a deadline the timeout arm is unreachable; like
            // `scan_all`, a stop ends the sweep with what it found.
            Step::Exhausted | Step::TimedOut => {
                // Same-line matches at or before the cursor are skipped by
                // eligibility and sit outside the wrapped sweep; when
                // nothing else was counted, the earliest of them is
                // `matches[0]` — exactly what the full walk wraps to for a
                // single count. Larger counts need the full walk's cyclic
                // advance.
                if yielded == 0
                    && wrapscan
                    && count == 1
                    && let Some(span) = first_skipped
                {
                    return Ok(Some(Select::Found {
                        span,
                        wrapped: true,
                    }));
                }
                // No counted and no skipped yield means no match anywhere:
                // the first sweep saw nothing outside its skip window and
                // the wrapped sweep saw nothing either.
                if yielded == 0 && first_skipped.is_none() {
                    return Ok(None);
                }
                // Matches exist but the answer lies past the buffer end
                // with wrapping off, or past one full cycle: the former is
                // not found, the latter needs the full walk.
                if !wrapscan {
                    return Ok(None);
                }
                return Ok(Some(Select::NeedFull));
            }
        }
    }
}

/// Backward lazy selection: steps forward from 0 with the after-end
/// advance, yielding the identical candidate set and order `scan_all`
/// collects — this preserves the pinned non-overlapping `?` semantics
/// that the backward index's past-start advance would change. Keeps the
/// last `count` eligible matches and stops at the first yield past the
/// cursor; wraps fall through to [`select_full_index`].
fn select_lazy_backward(
    text: &SearchText,
    program: Program<'_>,
    cursor_byte: usize,
    count: usize,
    wrapscan: bool,
) -> Result<Option<Select>, SearchError> {
    let mut scan = CandidateScan::new(text, SearchDirection::Forward, None, None);
    let mut from = 0usize;
    let mut ring: VecDeque<Candidate> = VecDeque::new();
    let mut pushed = 0usize;
    // A stop (or the first yield past the cursor) ends the sweep with the
    // matches found so far; the ring below decides from those.
    while let Step::Found(candidate) = scan.step(program, from, true)? {
        from = scan.advance_after_end(&candidate);
        if candidate.span.start.byte >= cursor_byte {
            break;
        }
        pushed += 1;
        if ring.len() == count {
            ring.pop_front();
        }
        ring.push_back(candidate);
    }
    if pushed >= count {
        // `ring` holds the last `count` eligible matches in ascending
        // order; the answer is the earliest of them.
        if let Some(answer) = ring.pop_front() {
            return Ok(Some(Select::Found {
                span: answer.span,
                wrapped: false,
            }));
        }
    }
    if !wrapscan {
        return Ok(None);
    }
    Ok(Some(Select::NeedFull))
}

/// Reference selection over a fully collected match list: the pre-lazy
/// index walk verbatim. Slow path for counts past one full match cycle
/// and backward wraps, where early stopping cannot apply. Returns the
/// selected index and whether selection wrapped.
fn select_full_index(
    matches: &[Candidate],
    cursor_byte: usize,
    direction: SearchDirection,
    count: usize,
    wrapscan: bool,
    pattern: &str,
) -> Result<(usize, bool), SearchError> {
    if matches.is_empty() {
        return Err(SearchError::PatternNotFound(pattern.to_owned()));
    }
    let eligible = |index: usize| match direction {
        SearchDirection::Forward => matches[index].span.start.byte > cursor_byte,
        SearchDirection::Backward => matches[index].span.start.byte < cursor_byte,
    };
    let mut index = match direction {
        SearchDirection::Forward => (0..matches.len()).find(|index| eligible(*index)),
        SearchDirection::Backward => (0..matches.len()).rev().find(|index| eligible(*index)),
    };
    let mut wrapped = false;
    if index.is_none() && wrapscan {
        wrapped = true;
        index = Some(match direction {
            SearchDirection::Forward => 0,
            SearchDirection::Backward => matches.len() - 1,
        });
    }
    let Some(mut index) = index else {
        return Err(SearchError::PatternNotFound(pattern.to_owned()));
    };
    for _ in 1..count {
        match direction {
            SearchDirection::Forward if index + 1 < matches.len() => index += 1,
            SearchDirection::Backward if index > 0 => index -= 1,
            SearchDirection::Forward if wrapscan => {
                index = 0;
                wrapped = true;
            }
            SearchDirection::Backward if wrapscan => {
                index = matches.len() - 1;
                wrapped = true;
            }
            _ => return Err(SearchError::PatternNotFound(pattern.to_owned())),
        }
    }
    Ok((index, wrapped))
}

// ---------------------------------------------------------------------------
// Shared scan engine
// ---------------------------------------------------------------------------

/// Compiles `pattern` with the requested magic mode (`search.c`
/// `search_regcomp`).
///
/// # Errors
///
/// Returns [`SearchError::Compile`] when the pattern is rejected.
pub(crate) fn compile_search(pattern: &str, magic: Magic) -> Result<Prog, SearchError> {
    ox_regex::compile(pattern, magic).map_err(SearchError::from)
}

/// Injects Vim's case modifier when effective `'ignorecase'` demands it.
/// A trailing `\c` applies to the whole pattern, so a leading `\%#=[012]`
/// engine selector is preserved untouched, and an explicit `\c`/`\C`
/// already in the pattern always wins over the option.
#[must_use]
pub(crate) fn pattern_with_case(pattern: &str, ignorecase: bool) -> Cow<'_, str> {
    if !ignorecase {
        return Cow::Borrowed(pattern);
    }
    let body = pattern.strip_prefix("\\%#=").unwrap_or(pattern);
    if body.contains("\\c") || body.contains("\\C") {
        return Cow::Borrowed(pattern);
    }
    Cow::Owned(format!("{pattern}\\c"))
}

/// UTF-8-validated buffer text for one scan.
///
/// Lines are validated once and joined into a single [`Text`], so a scan
/// costs one allocation regardless of line count, and every editor position
/// maps onto the flattened byte offsets the regex engine reports.
pub(crate) struct SearchText {
    text: Text,
    line_starts: Vec<usize>,
}

impl SearchText {
    /// Validates `lines` as UTF-8 and flattens them with line separators.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::InvalidUtf8`] when any line is not valid UTF-8.
    pub(crate) fn new(lines: &[Vec<u8>]) -> Result<Self, SearchError> {
        let capacity = lines
            .iter()
            .map(|line| line.len().saturating_add(1))
            .sum::<usize>();
        let mut joined = String::with_capacity(capacity);
        let mut line_starts = Vec::with_capacity(lines.len());
        for (index, line) in lines.iter().enumerate() {
            if index != 0 {
                joined.push('\n');
            }
            line_starts.push(joined.len());
            let slice = std::str::from_utf8(line).map_err(|_| SearchError::InvalidUtf8)?;
            joined.push_str(slice);
        }
        Ok(Self {
            text: Text::new(joined),
            line_starts,
        })
    }

    /// The flattened input behind the compiled text model.
    pub(crate) fn as_str(&self) -> &str {
        self.text.as_str()
    }

    /// The regex engine's view of the buffer.
    pub(crate) fn as_regex_text(&self) -> &Text {
        &self.text
    }

    /// Total flattened byte length.
    pub(crate) fn len(&self) -> usize {
        self.text.len()
    }

    /// Number of logical buffer lines.
    pub(crate) fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Byte offset of a one-based line's first character.
    pub(crate) fn line_start(&self, lnum: usize) -> usize {
        self.line_starts
            .get(lnum.saturating_sub(1))
            .copied()
            .unwrap_or(self.text.len())
    }

    /// Flattened byte offset of an editor position. The column is added
    /// without clamping, mirroring the cursor arithmetic `search()` has
    /// always used for eligibility comparisons.
    pub(crate) fn byte_of(&self, pos: Position) -> usize {
        self.line_start(pos.lnum.max(1)).saturating_add(pos.col)
    }

    /// Line and byte-column coordinates of a UTF-8 boundary byte offset.
    pub(crate) fn position_of(&self, byte: usize) -> Option<RegexPosition> {
        self.text.position(byte)
    }

    /// The next UTF-8 scalar boundary strictly after `byte`.
    pub(crate) fn scalar_after(&self, byte: usize) -> usize {
        let slice = self.text.as_str();
        let mut next = byte.min(slice.len()).saturating_add(1);
        while next < slice.len() && !slice.is_char_boundary(next) {
            next += 1;
        }
        next.min(slice.len())
    }

    /// The previous UTF-8 scalar boundary strictly before `byte`.
    pub(crate) fn scalar_before(&self, byte: usize) -> usize {
        let slice = self.text.as_str();
        let mut previous = byte.min(slice.len()).saturating_sub(1);
        while previous > 0 && !slice.is_char_boundary(previous) {
            previous -= 1;
        }
        previous
    }
}

/// Line and column coordinates of a boundary byte offset.
pub(crate) fn position_of(text: &SearchText, byte: usize) -> RegexPosition {
    text.position_of(byte).unwrap_or(RegexPosition {
        lnum: 1,
        col: 0,
        byte: 0,
    })
}

/// The editor position of a match endpoint.
#[must_use]
pub(crate) fn editor_position(at: RegexPosition) -> Position {
    Position {
        lnum: at.lnum,
        col: at.col,
    }
}

/// One located match reduced to the positions the pair and count engines
/// consume; capture vectors are dropped at the scan boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MatchSpan {
    /// Inclusive match start.
    pub start: RegexPosition,
    /// Exclusive match end.
    pub end: RegexPosition,
}

/// Which alternation branch of a [`PairProgram`] a candidate matched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PairBranch {
    /// The `{start}` token.
    Start,
    /// The `{end}` token.
    End,
    /// The `{middle}` token.
    Middle,
}

/// A candidate plus, for pair programs, the alternation branch it matched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Candidate {
    /// The matched span.
    pub span: MatchSpan,
    /// The alternation branch taken; single-pattern scans answer [`PairBranch::Start`].
    pub branch: PairBranch,
}

/// The programs one scan step consults: a plain pattern, or the pair token
/// set in its current nesting phase.
#[derive(Clone, Copy)]
pub(crate) enum Program<'a> {
    /// One compiled pattern.
    Single(&'a Prog),
    /// The pair token set; the flag selects the nested phase, which drops
    /// the middle token exactly like upstream's pat2 switch.
    Pair(&'a PairProgram, bool),
}

impl Program<'_> {
    /// The candidate at `at`, if any active program matches there.
    fn find_at(
        &self,
        text: &SearchText,
        at: RegexPosition,
    ) -> Result<Option<Candidate>, SearchError> {
        let found = match self {
            Program::Single(prog) => {
                ox_regex::try_exec_at(prog, text.as_regex_text(), at)?.map(|found| {
                    (
                        MatchSpan {
                            start: found.start,
                            end: found.end,
                        },
                        PairBranch::Start,
                    )
                })
            }
            Program::Pair(pair, nested) => pair.match_at(text.as_regex_text(), at, *nested)?,
        };
        Ok(found.map(|(span, branch)| Candidate { span, branch }))
    }
}

/// The three `searchpair()` token programs, compiled independently so token
/// identity never depends on capture numbering inside user patterns
/// (`do_searchpair`'s `pat2`/`pat3`, merged per position here).
pub(crate) struct PairProgram {
    start: Prog,
    end: Prog,
    middle: Option<Prog>,
}

impl PairProgram {
    /// Compiles each token under `Magic::Magic` (`do_searchpair` uses magic
    /// semantics regardless of `'magic'`), wrapped in a group with an inline
    /// `\m` reset like upstream, with effective `'ignorecase'` applied.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Compile`] for the first rejected token.
    pub(crate) fn compile(
        start: &str,
        middle: &str,
        end: &str,
        ignorecase: bool,
    ) -> Result<Self, SearchError> {
        let token = |source: &str| {
            let wrapped = format!("\\({}\\m\\)", pattern_with_case(source, ignorecase));
            compile_search(&wrapped, Magic::Magic)
        };
        // An empty `middle` gets no program: an empty third branch would
        // match at every position (upstream shares `pat2` instead).
        let middle = if middle.is_empty() {
            None
        } else {
            Some(token(middle)?)
        };
        Ok(Self {
            start: token(start)?,
            end: token(end)?,
            middle,
        })
    }

    /// Runs the active token programs from `at` and merges their candidates
    /// by position, ties breaking start → end → middle, exactly like the
    /// leftmost-position, leftmost-branch scan of upstream's alternation.
    /// Token identity comes from which program matched, never from capture
    /// indices, so user groups inside a token cannot reclassify it. The
    /// nested phase simply stops consulting the middle token.
    ///
    /// # Errors
    ///
    /// Propagates bounded-execution failures.
    pub(crate) fn match_at(
        &self,
        text: &Text,
        at: RegexPosition,
        nested: bool,
    ) -> Result<Option<(MatchSpan, PairBranch)>, SearchError> {
        let tokens = [
            (Some(&self.start), PairBranch::Start, 0usize),
            (Some(&self.end), PairBranch::End, 1usize),
            (self.middle.as_ref(), PairBranch::Middle, 2usize),
        ];
        let mut best: Option<(MatchSpan, PairBranch, usize)> = None;
        for (prog, branch, rank) in tokens {
            let Some(prog) = prog else {
                continue;
            };
            if nested && branch == PairBranch::Middle {
                continue;
            }
            if let Some(found) = ox_regex::try_exec_at(prog, text, at)? {
                let key = (found.start.byte, rank);
                let better = match &best {
                    None => true,
                    Some((span, _, best_rank)) => key < (span.start.byte, *best_rank),
                };
                if better {
                    best = Some((
                        MatchSpan {
                            start: found.start,
                            end: found.end,
                        },
                        branch,
                        rank,
                    ));
                }
            }
        }
        Ok(best.map(|(span, branch, _)| (span, branch)))
    }
}

/// Outcome of one lazy candidate step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Step {
    /// A candidate within the permitted span.
    Found(Candidate),
    /// No candidate remains.
    Exhausted,
    /// The monotonic deadline elapsed between candidates.
    TimedOut,
}

/// Which sweep of a wrapping scan a step belongs to. Upstream scans the
/// buffer at most twice per search (`searchit`'s `for (loop = 0; loop <= 1; loop++)`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Sweep {
    First,
    Wrapped,
}

/// Incremental backward frontier index: every candidate starting before
/// `scanned_to` is held in `matches`, in ascending order.
struct BackwardIndex {
    matches: Vec<Candidate>,
    scanned_to: usize,
    complete: bool,
}

/// Lazy, allocation-conscious candidate scan in one direction.
///
/// Forward scans execute the program from a moving byte frontier and never
/// materialize an index; backward scans extend a bounded frontier only as far
/// as each step requires. A wrapping scan sweeps the buffer at most twice,
/// mirroring `searchit`: the wrapped sweep accepts candidates without the
/// cursor-relative threshold the first sweep applies.
pub(crate) struct CandidateScan<'a> {
    text: &'a SearchText,
    direction: SearchDirection,
    /// Inclusive one-based stop line; `None` is unbounded.
    stop_line: Option<usize>,
    deadline: Option<Instant>,
    wrap: bool,
    sweep: Sweep,
    backward: Option<BackwardIndex>,
    /// Backward index for the wrapped sweep, covering starts at or after the
    /// first sweep's initial bound.
    wrapped_backward: Option<BackwardIndex>,
    /// Forward scan cursor for the wrapped sweep.
    wrapped_frontier: usize,
    /// Exclusive byte limit of the wrapped forward sweep: the start line of
    /// the first sweep's initial position.
    wrapped_limit: usize,
    /// The first step's scan position, captured for the wrap complement regions.
    first_from: Option<usize>,
}

impl<'a> CandidateScan<'a> {
    /// Builds a scan over `text`. `stop_line` bounds the span inclusively in
    /// both directions; `deadline` stops the scan between candidates.
    pub(crate) fn new(
        text: &'a SearchText,
        direction: SearchDirection,
        stop_line: Option<usize>,
        deadline: Option<Instant>,
    ) -> Self {
        let backward = (direction == SearchDirection::Backward).then(|| BackwardIndex {
            matches: Vec::new(),
            scanned_to: Self::region_start(text, stop_line),
            complete: false,
        });
        Self {
            text,
            direction,
            stop_line,
            deadline,
            wrap: false,
            sweep: Sweep::First,
            backward,
            wrapped_backward: None,
            wrapped_frontier: 0,
            wrapped_limit: 0,
            first_from: None,
        }
    }

    /// Lets a wrapped scan sweep the complement of its first pass.
    #[must_use]
    pub(crate) fn with_wrap(mut self) -> Self {
        self.wrap = true;
        self
    }

    fn region_start(text: &SearchText, stop_line: Option<usize>) -> usize {
        stop_line.map_or(0, |line| text.line_start(line.max(1)))
    }

    fn timed_out(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Returns the next candidate beyond `from`, or at `from` when
    /// `inclusive` (the one-shot `c`-flag cursor inclusion). Steps must use
    /// non-increasing `from` within a sweep. The program is a per-step
    /// parameter so the pair state machine can switch alternations as its
    /// nesting depth changes.
    ///
    /// # Errors
    ///
    /// Propagates [`SearchError::Execute`] for bounded-execution failures.
    pub(crate) fn step(
        &mut self,
        program: Program<'_>,
        from: usize,
        inclusive: bool,
    ) -> Result<Step, SearchError> {
        if self.first_from.is_none() {
            self.first_from = Some(from);
        }
        match self.direction {
            SearchDirection::Forward => self.step_forward(program, from, inclusive),
            SearchDirection::Backward => self.step_backward(program, from, inclusive),
        }
    }

    /// One pair-loop step: consults the token set in its current nesting
    /// phase (`start`/`end` only while nested, all three otherwise).
    ///
    /// # Errors
    ///
    /// Propagates [`SearchError::Execute`] for bounded-execution failures.
    pub(crate) fn step_pair(
        &mut self,
        program: &PairProgram,
        nested: bool,
        from: usize,
        inclusive: bool,
    ) -> Result<Step, SearchError> {
        self.step(Program::Pair(program, nested), from, inclusive)
    }

    fn step_forward(
        &mut self,
        program: Program<'_>,
        from: usize,
        inclusive: bool,
    ) -> Result<Step, SearchError> {
        if self.sweep == Sweep::Wrapped {
            return self.step_forward_wrapped(program);
        }
        let len = self.text.len();
        let mut threshold = if inclusive {
            from.min(len)
        } else {
            self.text.scalar_after(from.min(len))
        };
        loop {
            if self.timed_out() {
                return Ok(Step::TimedOut);
            }
            if threshold >= len {
                return self.wrap_forward_exhausted(program);
            }
            let Some(start) = self.text.position_of(threshold) else {
                threshold = self.text.scalar_after(threshold);
                continue;
            };
            let Some(candidate) = program.find_at(self.text, start)? else {
                return self.wrap_forward_exhausted(program);
            };
            if self
                .stop_line
                .is_some_and(|stop| candidate.span.start.lnum > stop)
            {
                return Ok(Step::Exhausted);
            }
            return Ok(Step::Found(candidate));
        }
    }

    /// On first-sweep exhaustion, a wrapping scan enters its second sweep
    /// over the lines strictly before the first sweep's start line, without
    /// any cursor threshold (`searchit`'s second loop stops when it reaches
    /// the start line again).
    fn wrap_forward_exhausted(&mut self, program: Program<'_>) -> Result<Step, SearchError> {
        if !self.wrap || self.sweep == Sweep::Wrapped {
            return Ok(Step::Exhausted);
        }
        self.sweep = Sweep::Wrapped;
        self.wrapped_frontier = 0;
        let start_line = self
            .first_from
            .and_then(|from| self.text.position_of(from))
            .map_or(self.text.line_count() + 1, |start| start.lnum);
        self.wrapped_limit = self.text.line_start(start_line);
        self.step_forward_wrapped(program)
    }

    fn step_forward_wrapped(&mut self, program: Program<'_>) -> Result<Step, SearchError> {
        let mut threshold = self.wrapped_frontier;
        loop {
            if self.timed_out() {
                return Ok(Step::TimedOut);
            }
            if threshold >= self.wrapped_limit {
                return Ok(Step::Exhausted);
            }
            let Some(start) = self.text.position_of(threshold) else {
                threshold = self.text.scalar_after(threshold);
                continue;
            };
            let Some(candidate) = program.find_at(self.text, start)? else {
                return Ok(Step::Exhausted);
            };
            self.wrapped_frontier = self.advance_past(&candidate);
            if self
                .stop_line
                .is_some_and(|stop| candidate.span.start.lnum > stop)
            {
                return Ok(Step::Exhausted);
            }
            return Ok(Step::Found(candidate));
        }
    }

    fn step_backward(
        &mut self,
        program: Program<'_>,
        from: usize,
        inclusive: bool,
    ) -> Result<Step, SearchError> {
        let len = self.text.len();
        // Backward candidates must start strictly before this byte bound;
        // `searchit`'s comparison is `start < start_pos + extra_col`, where
        // `extra_col` is the start character's length under `c`.
        let bound = if inclusive {
            self.text.scalar_after(from.min(len))
        } else {
            from.min(len)
        };
        if self.sweep == Sweep::Wrapped {
            return self.step_backward_wrapped(program, bound);
        }
        let Some(index) = self.backward.as_mut() else {
            return Ok(Step::Exhausted);
        };
        Self::extend_backward(self.text, self.deadline, program, index, bound)?;
        while index
            .matches
            .last()
            .is_some_and(|candidate| candidate.span.start.byte >= bound)
        {
            index.matches.pop();
        }
        Self::skip_nested_middle(index, program);
        if let Some(last) = index.matches.last() {
            return Ok(Step::Found(*last));
        }
        self.step_backward_wrapped_on_exhaustion(program, from, inclusive)
    }

    /// Executes candidates from `index.scanned_to` up to `bound`, extending
    /// the frontier one match at a time. A static receiver keeps the mutable
    /// index borrow separate from the shared text.
    fn extend_backward(
        text: &SearchText,
        deadline: Option<Instant>,
        program: Program<'_>,
        index: &mut BackwardIndex,
        bound: usize,
    ) -> Result<(), SearchError> {
        while !index.complete && index.scanned_to < bound {
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return Ok(());
            }
            let Some(start) = text.position_of(index.scanned_to) else {
                index.scanned_to = text.scalar_after(index.scanned_to);
                continue;
            };
            let Some(candidate) = program.find_at(text, start)? else {
                index.complete = true;
                break;
            };
            // The pair/count cursor policy resumes one scalar past the
            // match start, so overlapping backward candidates stay
            // indexable (`searchit`'s `extra_col` model).
            index.scanned_to = text.scalar_after(candidate.span.start.byte);
            index.matches.push(candidate);
        }
        Ok(())
    }

    /// Drops `Middle`-branch candidates from the back of `index.matches`
    /// when the active program is a nested pair. The pair state machine
    /// consults only `start`/`end` while nested (`do_searchpair`'s pat2
    /// switch), so a `Middle` cached during an earlier outer-level step
    /// must not be returned once nesting opened. Popping is safe: backward
    /// scans consume rightmost-first and move left, so a skipped `Middle`
    /// sits to the right of every subsequent step and is never revisited.
    fn skip_nested_middle(index: &mut BackwardIndex, program: Program<'_>) {
        if matches!(program, Program::Pair(_, true)) {
            while index
                .matches
                .last()
                .is_some_and(|c| c.branch == PairBranch::Middle)
            {
                index.matches.pop();
            }
        }
    }

    fn step_backward_wrapped_on_exhaustion(
        &mut self,
        program: Program<'_>,
        from: usize,
        inclusive: bool,
    ) -> Result<Step, SearchError> {
        if !self.wrap || self.sweep == Sweep::Wrapped {
            return Ok(Step::Exhausted);
        }
        // The wrapped sweep covers the starts the first sweep's bound
        // excluded; it accepts every candidate it finds ("always accept a
        // position after wrapping around").
        let first_from = self.first_from.unwrap_or(from);
        let len = self.text.len();
        let bound = if inclusive {
            self.text.scalar_after(first_from.min(len))
        } else {
            first_from.min(len)
        };
        self.sweep = Sweep::Wrapped;
        self.wrapped_backward = Some(BackwardIndex {
            matches: Vec::new(),
            scanned_to: bound,
            complete: false,
        });
        self.step_backward_wrapped(program, len)
    }

    fn step_backward_wrapped(
        &mut self,
        program: Program<'_>,
        bound: usize,
    ) -> Result<Step, SearchError> {
        let Some(index) = self.wrapped_backward.as_mut() else {
            return Ok(Step::Exhausted);
        };
        Self::extend_backward(self.text, self.deadline, program, index, self.text.len())?;
        while index
            .matches
            .last()
            .is_some_and(|candidate| candidate.span.start.byte >= bound)
        {
            index.matches.pop();
        }
        Self::skip_nested_middle(index, program);
        if let Some(last) = index.matches.last() {
            return Ok(Step::Found(*last));
        }
        Ok(Step::Exhausted)
    }

    /// Exhaustive candidate list of one forward sweep. Each scan resumes at
    /// the prior match end, matching the public `/` continuation policy.
    /// Zero-width matches advance one scalar so `\zs` patterns and multibyte
    /// text terminate.
    ///
    /// Forward scanners only: a backward scanner's step intentionally
    /// re-returns its last candidate, which only the pair loop consumes
    /// through its repeat-position guard.
    ///
    /// # Errors
    ///
    /// Propagates [`SearchError::Execute`] for bounded-execution failures.
    pub(crate) fn scan_all(&mut self, program: Program<'_>) -> Result<Vec<Candidate>, SearchError> {
        debug_assert_eq!(self.direction, SearchDirection::Forward);
        let mut all = Vec::new();
        let mut from = 0usize;
        while let Step::Found(candidate) = self.step(program, from, true)? {
            from = self.advance_after_end(&candidate);
            all.push(candidate);
        }
        Ok(all)
    }
    /// Resume one scalar after the match start. Pair/count scans use this
    /// frontier to preserve overlapping candidates and guarantee progress.
    fn advance_past(&self, candidate: &Candidate) -> usize {
        self.text.scalar_after(candidate.span.start.byte)
    }

    /// Where the public after-end scan resumes after `candidate`: past the
    /// match end like upstream's `'cpoptions'` `l` policy; a zero-width
    /// match advances one scalar so overlap and multibyte text cannot loop.
    fn advance_after_end(&self, candidate: &Candidate) -> usize {
        let end = candidate.span.end.byte;
        if end > candidate.span.start.byte {
            end.min(self.text.len())
        } else {
            self.text.scalar_after(candidate.span.start.byte)
        }
    }
}

/// One bounded `searchcount()` scan (`update_search_stat`'s slow path).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CountScan {
    /// Matches whose start is at or before the reference position.
    pub current: i64,
    /// Matches found before any limit hit.
    pub total: i64,
    /// Whether the reference position sits inside a counted match.
    pub exact_match: bool,
    /// `0` complete, `1` deadline passed, `2` max count exceeded.
    pub incomplete: i64,
    /// Whether at least one candidate was located, which decides
    /// cacheability the way `done_search` does upstream.
    pub found_any: bool,
}

/// Counts forward matches from the buffer start with no wrap, honoring
/// `maxcount` and `deadline` exactly as `update_search_stat` does: the
/// deadline is checked after a candidate is found but before it is counted,
/// and the scan stops after `maxcount + 1` matches.
///
/// `pos` is the reference position as `(lnum, col, coladd)` — a one-based
/// line, a zero-based column, and the position's `coladd`; located matches
/// always carry `coladd` zero.
///
/// # Errors
///
/// Propagates [`SearchError::Execute`] for bounded-execution failures.
pub(crate) fn scan_count(
    text: &SearchText,
    prog: &Prog,
    pos: (i64, i64, i64),
    maxcount: i64,
    deadline: Option<Instant>,
) -> Result<CountScan, SearchError> {
    let mut scan = CandidateScan::new(text, SearchDirection::Forward, None, deadline);
    let mut current = 0i64;
    let mut total = 0i64;
    let mut exact_match = false;
    let mut incomplete = 0i64;
    let mut found_any = false;
    let mut from = 0usize;
    while let Step::Found(candidate) = scan.step(Program::Single(prog), from, true)? {
        found_any = true;
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            incomplete = 1;
            break;
        }
        total = total.saturating_add(1);
        let key = |at: RegexPosition| {
            (
                i64::try_from(at.lnum).unwrap_or(i64::MAX),
                i64::try_from(at.col).unwrap_or(i64::MAX),
                0i64,
            )
        };
        if key(candidate.span.start) <= pos {
            current = total;
            if pos < key(candidate.span.end) {
                exact_match = true;
            }
        }
        if maxcount > 0 && total > maxcount {
            incomplete = 2;
            break;
        }
        from = scan.advance_past(&candidate);
    }
    Ok(CountScan {
        current,
        total,
        exact_match,
        incomplete,
        found_any,
    })
}

/// Editor-owned `searchcount()` cache (`search.c`'s `update_search_stat`
/// statics). One record per [`crate::Editor`]; a scan that finds no match
/// leaves no cache, mirroring upstream's "no last position" behavior.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SearchCountState {
    /// Pattern the cached scan ran with.
    pub(crate) pattern: String,
    /// Buffer the cached scan ran against.
    pub(crate) buffer: ox_types::BufHandle,
    /// Buffer change tick at scan time.
    pub(crate) changedtick: u64,
    /// Reference position the cached scan was computed for.
    pub(crate) pos: Position,
    /// Matches at or before `pos`.
    pub(crate) current: i64,
    /// Total counted matches.
    pub(crate) total: i64,
    /// Whether `pos` sat inside a counted match.
    pub(crate) exact_match: bool,
    /// `0` complete, `1` deadline passed, `2` max count exceeded.
    pub(crate) incomplete: i64,
    /// The `maxcount` the cached scan used (`last_maxcount`).
    pub(crate) maxcount: i64,
}

fn previous_boundary(text: &str, exclusive: usize) -> usize {
    let mut byte = exclusive.saturating_sub(1).min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn shift_characters(text: &str, start: usize, delta: isize) -> usize {
    let mut byte = start.min(text.len());
    if delta >= 0 {
        for _ in 0..delta.cast_unsigned() {
            if byte >= text.len() {
                break;
            }
            byte += text[byte..].chars().next().map_or(0, char::len_utf8);
        }
    } else {
        for _ in 0..delta.unsigned_abs() {
            if byte == 0 {
                break;
            }
            byte = previous_boundary(text, byte);
        }
    }
    byte.min(text.len().saturating_sub(usize::from(!text.is_empty())))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn lines(data: &[&str]) -> Vec<Vec<u8>> {
        data.iter().map(|line| line.as_bytes().to_vec()).collect()
    }

    fn public_search(
        data: &[&str],
        cursor: Position,
        pattern: &str,
        direction: SearchDirection,
    ) -> SearchResult {
        run(
            &lines(data),
            cursor,
            pattern,
            direction,
            SearchOffset::default(),
            1,
            true,
        )
        .unwrap()
    }

    /// The public `/` policy resumes at the match end (search.c's
    /// vi-compatible progression), so `aa` on `aaaa` answers two
    /// non-overlapping matches instead of the overlapping starts an
    /// after-start sweep would list.
    #[test]
    fn public_scan_progression_skips_overlapping_tail_matches() {
        let result = public_search(
            &["aaaa"],
            Position { lnum: 1, col: 0 },
            "aa",
            SearchDirection::Forward,
        );
        assert_eq!(result.target, Position { lnum: 1, col: 2 });
    }
    #[test]
    fn public_scan_progression_is_multibyte_safe() {
        let result = public_search(
            &["éééé"],
            Position { lnum: 1, col: 0 },
            "éé",
            SearchDirection::Forward,
        );
        assert_eq!(result.target, Position { lnum: 1, col: 4 });
    }

    /// A same-line match before the cursor is still found on wrap: the
    /// first sweep skips it by eligibility and the wrapped sweep starts
    /// at the cursor line, so the driver remembers it as `matches[0]`.
    #[test]
    fn public_forward_wrap_finds_same_line_match_before_cursor() {
        let result = public_search(
            &["foo"],
            Position { lnum: 1, col: 2 },
            "foo",
            SearchDirection::Forward,
        );
        assert_eq!(result.target, Position { lnum: 1, col: 0 });
        assert!(result.wrapped);
        // Counts past the last match wrap to `matches[0]` and keep
        // advancing, so the count selects within the wrapped list.
        for (count, col) in [(2usize, 4), (3usize, 8)] {
            let result = run(
                &lines(&["foo foo foo"]),
                Position { lnum: 1, col: 10 },
                "foo",
                SearchDirection::Forward,
                SearchOffset::default(),
                count,
                true,
            )
            .unwrap();
            assert_eq!(result.target, Position { lnum: 1, col });
            assert!(result.wrapped);
        }
    }

    /// Selections past one full match cycle and backward wraps fall through
    /// to the reference walk: the lazy drivers report `NeedFull` and the
    /// collected list decides with cyclic wrap arithmetic.
    /// The wrapped sweep keeps the public after-end progression: on
    /// `["aaaa", "b"]` with the cursor past the last match, a counted wrap
    /// must not offer the overlapping start byte 1.
    #[test]
    fn public_wrapped_search_keeps_after_end_progression() {
        for (count, col) in [(1usize, 0), (2usize, 2)] {
            let result = run(
                &lines(&["aaaa", "b"]),
                Position { lnum: 2, col: 0 },
                "aa",
                SearchDirection::Forward,
                SearchOffset::default(),
                count,
                true,
            )
            .unwrap();
            assert_eq!(result.target, Position { lnum: 1, col });
            assert!(result.wrapped);
        }
    }

    #[test]
    fn public_selection_falls_back_past_one_match_cycle() {
        // Forward count past the only match: wraps to `matches[0]` twice.
        let result = run(
            &lines(&["foo"]),
            Position { lnum: 1, col: 3 },
            "foo",
            SearchDirection::Forward,
            SearchOffset::default(),
            3,
            true,
        )
        .unwrap();
        assert_eq!(result.target, Position { lnum: 1, col: 0 });
        assert!(result.wrapped);
        // Backward wrap with nothing eligible: the last match overall.
        let result = run(
            &lines(&["foo"]),
            Position { lnum: 1, col: 0 },
            "foo",
            SearchDirection::Backward,
            SearchOffset::default(),
            1,
            true,
        )
        .unwrap();
        assert_eq!(result.target, Position { lnum: 1, col: 0 });
        assert!(result.wrapped);
    }

    /// Backward public searches sweep the same non-overlapping list and pick
    /// the nearest earlier match, exactly like `N`.
    #[test]
    fn public_backward_search_picks_the_nearest_earlier_match() {
        let result = public_search(
            &["aaaa"],
            Position { lnum: 1, col: 3 },
            "aa",
            SearchDirection::Backward,
        );
        assert_eq!(result.target, Position { lnum: 1, col: 2 });
    }

    /// Backward pair indexing advances one scalar from the match start, so
    /// the overlapping candidate between two matches stays indexable
    /// (`é` is two bytes; the middle match starts at byte 2).
    #[test]
    fn backward_pair_index_advances_one_scalar_from_the_start() {
        let text = SearchText::new(&lines(&["éééé"])).unwrap();
        let prog = compile_search("éé", Magic::Magic).unwrap();
        let mut index = BackwardIndex {
            matches: Vec::new(),
            scanned_to: 0,
            complete: false,
        };
        CandidateScan::extend_backward(&text, None, Program::Single(&prog), &mut index, text.len())
            .unwrap();
        let starts: Vec<usize> = index.matches.iter().map(|c| c.span.start.byte).collect();
        assert_eq!(starts, vec![0, 2, 4]);
    }

    /// A user capture group inside `{start}` must not demote the `{end}`
    /// wrapper to `Middle`: branch identity comes from the program that
    /// matched, not the capture index.
    #[test]
    fn pair_branches_ignore_user_capture_groups() {
        let program = PairProgram::compile("\\(a\\)", "", "b", false).unwrap();
        let text = Text::new("ab");
        let start = RegexPosition {
            lnum: 1,
            col: 0,
            byte: 0,
        };
        let at_end = RegexPosition {
            lnum: 1,
            col: 1,
            byte: 1,
        };
        let (span, branch) = program.match_at(&text, start, false).unwrap().unwrap();
        assert_eq!(branch, PairBranch::Start);
        assert_eq!(span.end.byte, 1);
        let (_, branch) = program.match_at(&text, at_end, false).unwrap().unwrap();
        assert_eq!(branch, PairBranch::End);
    }

    /// Tokens matching at the same position tie-break start → end → middle,
    /// and the winner carries its own span.
    #[test]
    fn same_position_tokens_tie_break_start_end_middle() {
        let program = PairProgram::compile("ab", "", "abc", false).unwrap();
        let text = Text::new("abc");
        let at = RegexPosition {
            lnum: 1,
            col: 0,
            byte: 0,
        };
        let (span, branch) = program.match_at(&text, at, false).unwrap().unwrap();
        assert_eq!(branch, PairBranch::Start);
        assert_eq!(span.end.byte, 2);
    }

    /// A middle token winning its own position classifies as `Middle` only
    /// at the outer level.
    #[test]
    fn middle_token_participates_only_at_the_outer_level() {
        let program = PairProgram::compile("a", "m", "b", false).unwrap();
        let text = Text::new("am");
        let at_middle = RegexPosition {
            lnum: 1,
            col: 1,
            byte: 1,
        };
        let (_, outer) = program.match_at(&text, at_middle, false).unwrap().unwrap();
        assert_eq!(outer, PairBranch::Middle);
        assert!(program.match_at(&text, at_middle, true).unwrap().is_none());
    }

    /// Effective `'ignorecase'` appends the case modifier, preserving a
    /// leading engine selector, and explicit `\c`/`\C` always win.
    #[test]
    fn case_injection_preserves_engine_selectors_and_explicit_overrides() {
        assert_eq!(pattern_with_case("foo", false), "foo");
        assert_eq!(pattern_with_case("foo", true), "foo\\c");
        assert_eq!(pattern_with_case("\\%#=1foo", true), "\\%#=1foo\\c");
        assert_eq!(pattern_with_case("foo\\Cbar", true), "foo\\Cbar");
        assert_eq!(pattern_with_case("\\%#=2foo\\c", true), "\\%#=2foo\\c");
    }

    /// The injected case mode reaches count scans: `foo` matches `Foo`
    /// under `'ignorecase'`, and an explicit `\C` overrides the option.
    #[test]
    fn count_scan_honors_effective_ignorecase_and_explicit_case() {
        let text = SearchText::new(&lines(&["Foo"])).unwrap();
        let caseless = compile_search(&pattern_with_case("foo", true), Magic::Magic).unwrap();
        let scan = scan_count(&text, &caseless, (1, 0, 0), i64::MAX, None).unwrap();
        assert_eq!(scan.total, 1);
        assert!(scan.exact_match);
        let casesensitive =
            compile_search(&pattern_with_case("foo\\C", true), Magic::Magic).unwrap();
        let scan = scan_count(&text, &casesensitive, (1, 0, 0), i64::MAX, None).unwrap();
        assert_eq!(scan.total, 0);
    }

    /// Backward pair candidate storage is phase-safe: a `Middle` cached
    /// during the outer-level first step is not returned once an `End`
    /// opens a nested level, and stays skipped until the depth returns to
    /// the outer level. Simulates the `do_searchpair` nesting transitions
    /// over `if ELIF END END` scanned backward from the end. The tokens do
    /// not embed one another, matching the word-boundary discipline
    /// upstream requires (`\<if\>` in `test_search.vim`).
    #[test]
    fn backward_pair_index_skips_cached_middle_while_nested() {
        let text = SearchText::new(&lines(&["if ELIF END END"])).unwrap();
        let program = PairProgram::compile("if", "ELIF", "END", false).unwrap();
        // Backward scan from the end of the line (byte 15).
        let mut scan = CandidateScan::new(&text, SearchDirection::Backward, None, None);
        // Step 1 — outer level: the nearest candidate is the second `END`
        // (an End), which closes the pair. Before it, the outer-level index
        // also caches the `ELIF` Middle.
        let step = scan.step_pair(&program, false, 15, false).unwrap();
        let candidate = match step {
            Step::Found(c) => c,
            other => panic!("first step: {other:?}"),
        };
        assert_eq!(candidate.branch, PairBranch::End);
        assert_eq!(candidate.span.start.byte, 12);
        // Step 2 — now nested (the End opened a level going backward).
        // The cached `ELIF` Middle at byte 3 must be skipped; the next
        // candidate is the first `END` at byte 8.
        let step = scan
            .step_pair(&program, true, candidate.span.start.byte, false)
            .unwrap();
        let candidate = match step {
            Step::Found(c) => c,
            other => panic!("nested step: {other:?}"),
        };
        assert_eq!(
            candidate.branch,
            PairBranch::End,
            "nested step must not return the cached Middle"
        );
        assert_eq!(candidate.span.start.byte, 8);
        // Step 3 — still nested: the `if` Start at byte 0 closes the level.
        let step = scan
            .step_pair(&program, true, candidate.span.start.byte, false)
            .unwrap();
        let candidate = match step {
            Step::Found(c) => c,
            other => panic!("closing step: {other:?}"),
        };
        assert_eq!(candidate.branch, PairBranch::Start);
        assert_eq!(candidate.span.start.byte, 0);
        // Step 4 — back to outer level: the skipped `ELIF` Middle is gone
        // (it sat to the right of the closing Start and is never revisited),
        // so the scan is exhausted.
        let step = scan
            .step_pair(&program, false, candidate.span.start.byte, false)
            .unwrap();
        assert_eq!(step, Step::Exhausted);
    }

    #[test]
    fn wrapped_backward_pair_progresses_across_candidates() {
        let text = SearchText::new(&lines(&["if END"])).unwrap();
        let program = PairProgram::compile("if", "", "END", false).unwrap();
        let mut scan = CandidateScan::new(&text, SearchDirection::Backward, None, None).with_wrap();

        // The first sweep has no candidate strictly before byte 0, so the
        // rightmost End is the first candidate from the wrapped sweep.
        let step = scan.step_pair(&program, false, 0, false).unwrap();
        let candidate = match step {
            Step::Found(c) => c,
            other => panic!("first wrapped step: {other:?}"),
        };
        assert_eq!(
            (candidate.branch, candidate.span.start.byte),
            (PairBranch::End, 3)
        );

        // The next call must consume that End rather than yield byte 3 again.
        let step = scan
            .step_pair(&program, true, candidate.span.start.byte, false)
            .unwrap();
        let candidate = match step {
            Step::Found(c) => c,
            other => panic!("second wrapped step: {other:?}"),
        };
        assert_eq!(
            (candidate.branch, candidate.span.start.byte),
            (PairBranch::Start, 0)
        );
        assert_eq!(
            scan.step_pair(&program, false, candidate.span.start.byte, false)
                .unwrap(),
            Step::Exhausted
        );
    }
}
