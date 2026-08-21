use crate::parser::{Anchor, CharClass, ClassItem, ClassKind, Compare, Expr, LookKind};
use crate::{Capture, ExecError, Match, Position, Prog, Text};

#[derive(Clone, Debug)]
pub(crate) struct State {
    pub(crate) pos: usize,
    captures: Vec<Option<(usize, usize)>>,
    opens: Vec<Option<usize>>,
    start_override: Option<usize>,
    end_override: Option<usize>,
}

impl State {
    pub(crate) fn new(pos: usize, capture_count: usize) -> Self {
        Self {
            pos,
            captures: vec![None; capture_count],
            opens: vec![None; capture_count],
            start_override: None,
            end_override: None,
        }
    }

    pub(crate) fn set_search_start(&mut self, pos: usize) {
        if self.start_override.is_none() {
            self.start_override = Some(pos);
        }
    }

    pub(crate) fn open_capture(&mut self, index: usize) {
        if let Some(slot) = self.opens.get_mut(index - 1) {
            *slot = Some(self.pos);
        }
    }

    pub(crate) fn close_capture(&mut self, index: usize) {
        if let (Some(Some(start)), Some(slot)) = (self.opens.get(index - 1), self.captures.get_mut(index - 1)) {
            *slot = Some((*start, self.pos));
        }
    }

    pub(crate) fn set_match_start(&mut self) {
        self.start_override = Some(self.pos);
    }

    pub(crate) fn set_match_end(&mut self) {
        self.end_override = Some(self.pos);
    }

    pub(crate) fn into_match(self, text: &Text, capture_count: usize) -> Match {
        let start_byte = match self.start_override { Some(byte) => byte, None => 0 };
        let end_byte = match self.end_override { Some(byte) => byte, None => self.pos };
        let fallback = Position { lnum: 1, col: 0, byte: 0 };
        let start = match text.position(start_byte) { Some(position) => position, None => fallback };
        let end = match text.position(end_byte) { Some(position) => position, None => start };
        let captures = self
            .captures
            .into_iter()
            .take(capture_count)
            .map(|span| {
                span.and_then(|(capture_start, capture_end)| {
                    Some(Capture {
                        start: text.position(capture_start)?,
                        end: text.position(capture_end)?,
                    })
                })
            })
            .collect();
        Match { start, end, captures }
    }
}

struct Context<'a> {
    prog: &'a Prog,
    text: &'a Text,
    steps: usize,
    search_start: usize,
}

pub(crate) fn search(prog: &Prog, text: &Text, from: usize) -> Result<Option<State>, ExecError> {
    let mut context = Context { prog, text, steps: 0, search_start: from };
    for candidate in candidate_offsets(text.as_str(), from) {
        context.search_start = candidate;
        let initial = State::new(candidate, prog.capture_count);
        if let Some(mut state) = match_expr(&prog.expr, initial, &mut context, 0)?.into_iter().next() {
            if state.start_override.is_none() {
                state.start_override = Some(candidate);
            }
            return Ok(Some(state));
        }
    }
    Ok(None)
}

pub(crate) fn match_at(
    prog: &Prog,
    text: &Text,
    expr: &Expr,
    state: State,
    search_start: usize,
) -> Result<Vec<State>, ExecError> {
    let mut context = Context { prog, text, steps: 0, search_start };
    match_expr(expr, state, &mut context, 0)
}

fn match_expr(
    expr: &Expr,
    state: State,
    context: &mut Context<'_>,
    depth: usize,
) -> Result<Vec<State>, ExecError> {
    context.steps = context.steps.checked_add(1).ok_or(ExecError::StepLimit)?;
    if context.steps > context.prog.step_limit {
        return Err(ExecError::StepLimit);
    }
    if depth > context.prog.depth_limit {
        return Err(ExecError::RecursionLimit);
    }
    match expr {
        Expr::Empty => Ok(vec![state]),
        Expr::Literal(expected) => match next_char(context.text.as_str(), state.pos) {
            Some((actual, next)) if chars_equal(*expected, actual, context.prog.ignore_case) => {
                Ok(vec![State { pos: next, ..state }])
            }
            _ => Ok(Vec::new()),
        },
        Expr::Any { newline } => match next_char(context.text.as_str(), state.pos) {
            Some((actual, next)) if *newline || actual != '\n' => Ok(vec![State { pos: next, ..state }]),
            _ => Ok(Vec::new()),
        },
        Expr::Class(class) => match next_char(context.text.as_str(), state.pos) {
            Some((actual, next)) if class_matches(class, actual, context.prog.ignore_case) => {
                Ok(vec![State { pos: next, ..state }])
            }
            _ => Ok(Vec::new()),
        },
        Expr::Concat(parts) => {
            let mut states = vec![state];
            for part in parts {
                let mut next = Vec::new();
                for current in states {
                    next.extend(match_expr(part, current, context, depth + 1)?);
                }
                if next.is_empty() {
                    return Ok(next);
                }
                states = next;
            }
            Ok(states)
        }
        Expr::Alt(branches) => {
            let mut states = Vec::new();
            for branch in branches {
                states.extend(match_expr(branch, state.clone(), context, depth + 1)?);
            }
            Ok(states)
        }
        Expr::And(parts) => {
            let Some((last, requirements)) = parts.split_last() else {
                return Ok(vec![state]);
            };
            for requirement in requirements {
                if match_expr(requirement, state.clone(), context, depth + 1)?.is_empty() {
                    return Ok(Vec::new());
                }
            }
            match_expr(last, state, context, depth + 1)
        }
        Expr::Repeat { expr, min, max, greedy } => {
            let mut states = Vec::new();
            repeat(expr, *min, *max, *greedy, state, 0, context, depth + 1, &mut states)?;
            Ok(states)
        }
        Expr::Group { index, expr } => {
            let capture_start = state.pos;
            let mut states = match_expr(expr, state, context, depth + 1)?;
            if let Some(index) = index {
                for result in &mut states {
                    if let Some(slot) = result.captures.get_mut(index - 1) {
                        *slot = Some((capture_start, result.pos));
                    }
                }
            }
            Ok(states)
        }
        Expr::OptionalSeq(parts) => optional_sequence(parts, state, context, depth + 1),
        Expr::Anchor(anchor) if anchor_matches(anchor, state.pos, context.text) => Ok(vec![state]),
        Expr::Anchor(_) => Ok(Vec::new()),
        Expr::Look { expr, kind, limit } => match_look(expr, *kind, *limit, state, context, depth + 1),
        Expr::Backref(index) => match_backref(*index, state, context),
        Expr::SetStart => Ok(vec![State { start_override: Some(state.pos), ..state }]),
        Expr::SetEnd => Ok(vec![State { end_override: Some(state.pos), ..state }]),
    }
}

#[allow(clippy::too_many_arguments)]
fn repeat(
    expr: &Expr,
    min: usize,
    max: Option<usize>,
    greedy: bool,
    state: State,
    count: usize,
    context: &mut Context<'_>,
    depth: usize,
    results: &mut Vec<State>,
) -> Result<(), ExecError> {
    if depth > context.prog.depth_limit {
        return Err(ExecError::RecursionLimit);
    }
    let can_continue = max.is_none_or(|maximum| count < maximum);
    if !greedy && count >= min {
        results.push(state.clone());
    }
    if can_continue {
        for next in match_expr(expr, state.clone(), context, depth + 1)? {
            if next.pos == state.pos {
                if count + 1 >= min && greedy {
                    results.push(next);
                } else if count + 1 >= min && !greedy {
                    results.push(next);
                }
                continue;
            }
            repeat(expr, min, max, greedy, next, count + 1, context, depth + 1, results)?;
        }
    }
    if greedy && count >= min {
        results.push(state);
    }
    Ok(())
}

fn optional_sequence(
    parts: &[Expr],
    state: State,
    context: &mut Context<'_>,
    depth: usize,
) -> Result<Vec<State>, ExecError> {
    let mut levels = vec![vec![state]];
    for part in parts {
        let mut next = Vec::new();
        let Some(previous) = levels.last().cloned() else {
            break;
        };
        for current in previous {
            next.extend(match_expr(part, current, context, depth + 1)?);
        }
        if next.is_empty() {
            break;
        }
        levels.push(next);
    }
    let mut states = Vec::new();
    while let Some(level) = levels.pop() {
        states.extend(level);
    }
    Ok(states)
}

fn match_look(
    expr: &Expr,
    kind: LookKind,
    limit: Option<usize>,
    state: State,
    context: &mut Context<'_>,
    depth: usize,
) -> Result<Vec<State>, ExecError> {
    match kind {
        LookKind::Ahead => {
            let position = state.pos;
            let mut results = match_expr(expr, state, context, depth + 1)?;
            for result in &mut results {
                result.pos = position;
            }
            Ok(results)
        }
        LookKind::NotAhead => {
            if match_expr(expr, state.clone(), context, depth + 1)?.is_empty() {
                Ok(vec![state])
            } else {
                Ok(Vec::new())
            }
        }
        LookKind::Atomic => {
            Ok(match_expr(expr, state, context, depth + 1)?.into_iter().take(1).collect())
        }
        LookKind::Behind | LookKind::NotBehind => {
            let earliest = lookbehind_earliest(context.text.as_str(), state.pos, limit);
            let mut found = false;
            for candidate in candidate_offsets(context.text.as_str(), earliest) {
                if candidate > state.pos {
                    break;
                }
                let probe = State::new(candidate, context.prog.capture_count);
                if match_expr(expr, probe, context, depth + 1)?
                    .into_iter()
                    .any(|result| result.pos == state.pos)
                {
                    found = true;
                    break;
                }
            }
            let positive = kind == LookKind::Behind;
            if found == positive { Ok(vec![state]) } else { Ok(Vec::new()) }
        }
    }
}

fn match_backref(index: usize, state: State, context: &Context<'_>) -> Result<Vec<State>, ExecError> {
    let Some(Some((start, end))) = state.captures.get(index - 1) else {
        return Ok(Vec::new());
    };
    let Some(captured) = context.text.as_str().get(*start..*end) else {
        return Ok(Vec::new());
    };
    let Some(candidate) = context.text.as_str().get(state.pos..) else {
        return Ok(Vec::new());
    };
    let consumed = if context.prog.ignore_case {
        prefix_case_folded(candidate, captured)
    } else {
        candidate.starts_with(captured).then_some(captured.len())
    };
    Ok(consumed.map_or_else(Vec::new, |length| vec![State { pos: state.pos + length, ..state }]))
}

fn prefix_case_folded(candidate: &str, captured: &str) -> Option<usize> {
    let mut candidate_chars = candidate.char_indices();
    let mut end = 0;
    for expected in captured.chars() {
        let (offset, actual) = candidate_chars.next()?;
        if !chars_equal(expected, actual, true) {
            return None;
        }
        end = offset + actual.len_utf8();
    }
    Some(end)
}

fn anchor_matches(anchor: &Anchor, offset: usize, text: &Text) -> bool {
    let bytes = text.as_str().as_bytes();
    match anchor {
        Anchor::LineStart => offset == 0 || bytes.get(offset.wrapping_sub(1)) == Some(&b'\n'),
        Anchor::LineEnd => offset == bytes.len() || bytes.get(offset) == Some(&b'\n'),
        Anchor::FileStart => offset == 0,
        Anchor::FileEnd => offset == bytes.len(),
        Anchor::WordStart => {
            next_char(text.as_str(), offset).is_some_and(|(ch, _)| is_word(ch))
                && previous_char(text.as_str(), offset).is_none_or(|ch| !is_word(ch))
        }
        Anchor::WordEnd => {
            previous_char(text.as_str(), offset).is_some_and(is_word)
                && next_char(text.as_str(), offset).is_none_or(|(ch, _)| !is_word(ch))
        }
        Anchor::Line(compare, expected) => text.position(offset).is_some_and(|pos| compare_number(*compare, pos.lnum, *expected)),
        Anchor::Column(compare, expected) => {
            text.position(offset).is_some_and(|pos| compare_number(*compare, pos.col + 1, *expected))
        }
        Anchor::VirtualColumn(compare, expected) => {
            compare_number(*compare, virtual_column(text.as_str(), offset), *expected)
        }
        Anchor::Visual => text.visual_contains(offset),
        Anchor::Cursor => text.cursor() == Some(offset),
        Anchor::Mark(name) => text.mark(*name) == Some(offset),
    }
}

fn compare_number(compare: Compare, actual: usize, expected: usize) -> bool {
    match compare {
        Compare::Equal => actual == expected,
        Compare::Less => actual < expected,
        Compare::Greater => actual > expected,
    }
}

fn previous_line_start(text: &str, offset: usize) -> usize {
    let before = &text.as_bytes()[..offset];
    let current_start = before.iter().rposition(|byte| *byte == b'\n').map_or(0, |index| index + 1);
    if current_start == 0 {
        return 0;
    }
    before[..current_start - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1)
}

fn lookbehind_earliest(text: &str, pos: usize, limit: Option<usize>) -> usize {
    let Some(limit) = limit else {
        return previous_line_start(text, pos);
    };
    let bytes = text.as_bytes();
    let line_start = bytes[..pos]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    if pos - line_start >= limit {
        pos - limit
    } else {
        let previous_start = previous_line_start(text, pos);
        line_start
            .saturating_sub(1)
            .saturating_sub(limit)
            .max(previous_start)
    }
}

fn virtual_column(text: &str, offset: usize) -> usize {
    let line_start = text.as_bytes()[..offset]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let mut column = 0;
    for ch in text[line_start..offset].chars() {
        column += if ch == '\t' { 8 - (column % 8) } else { 1 };
    }
    column + 1
}

fn simple_lower(ch: char) -> char {
    match ch.to_lowercase().next() {
        Some(lower) => lower,
        None => ch,
    }
}

pub(crate) fn class_matches(class: &CharClass, ch: char, ignore_case: bool) -> bool {
    if ch == '\n' {
        return class.include_newline;
    }
    let matched = class.items.iter().any(|item| match item {
        ClassItem::Char(expected) => chars_equal(*expected, ch, ignore_case),
        ClassItem::Range(start, end) if ignore_case => {
            let folded = simple_lower(ch);
            simple_lower(*start) <= folded && folded <= simple_lower(*end)
        }
        ClassItem::Range(start, end) => *start <= ch && ch <= *end,
        ClassItem::Kind(kind) => kind_matches(*kind, ch),
    });
    matched != class.negated
}

fn kind_matches(kind: ClassKind, ch: char) -> bool {
    match kind {
        ClassKind::Alnum => ch.is_alphanumeric(),
        ClassKind::Alpha => ch.is_alphabetic(),
        ClassKind::Blank => matches!(ch, ' ' | '\t'),
        ClassKind::Cntrl => ch.is_control(),
        ClassKind::Digit => ch.is_ascii_digit(),
        ClassKind::Graph => !ch.is_control() && !ch.is_whitespace(),
        ClassKind::Lower => ch.is_lowercase(),
        ClassKind::Print => !ch.is_control(),
        ClassKind::Punct => ch.is_ascii_punctuation(),
        ClassKind::Space => matches!(ch, ' ' | '\t'),
        ClassKind::Upper => ch.is_uppercase(),
        ClassKind::Xdigit | ClassKind::Hex => ch.is_ascii_hexdigit(),
        ClassKind::Word => is_word(ch),
        ClassKind::Head => ch == '_' || ch.is_alphabetic(),
        ClassKind::Octal => matches!(ch, '0'..='7'),
        ClassKind::Ident | ClassKind::Keyword => ch == '_' || ch.is_alphanumeric(),
        ClassKind::IdentNoDigit | ClassKind::KeywordNoDigit => ch == '_' || (ch.is_alphabetic() && !ch.is_numeric()),
        ClassKind::File => !ch.is_control() && !ch.is_whitespace(),
        ClassKind::FileNoDigit => !ch.is_control() && !ch.is_whitespace() && !ch.is_numeric(),
        ClassKind::PrintNoDigit => !ch.is_control() && !ch.is_numeric(),
    }
}

pub(crate) fn chars_equal(expected: char, actual: char, ignore_case: bool) -> bool {
    expected == actual || (ignore_case && expected.to_lowercase().eq(actual.to_lowercase()))
}

fn next_char(text: &str, offset: usize) -> Option<(char, usize)> {
    let ch = text.get(offset..)?.chars().next()?;
    Some((ch, offset + ch.len_utf8()))
}

fn previous_char(text: &str, offset: usize) -> Option<char> {
    text.get(..offset)?.chars().next_back()
}

fn is_word(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn candidate_offsets(text: &str, from: usize) -> impl Iterator<Item = usize> + '_ {
    text.char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .filter(move |offset| *offset >= from)
}
