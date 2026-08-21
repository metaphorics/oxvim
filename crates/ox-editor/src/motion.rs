//! Normal-mode motions and their operator semantics.

use ox_text::Position;

/// The shape an operator assigns to a motion range (`ops.c`: motion_type).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionKind {
    /// A byte-column range that may span lines.
    CharacterWise,
    /// Complete logical lines.
    LineWise,
    /// A rectangular byte-column range.
    BlockWise,
}

/// A resolved motion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Motion {
    /// Resolved destination cursor.
    pub target: Position,
    /// Range shape supplied to an operator.
    pub kind: MotionKind,
    /// Whether the destination byte belongs to the range.
    pub inclusive: bool,
    /// Whether normal execution records the origin in the jumplist.
    pub is_jump: bool,
}

/// Direction used by character-find motions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FindDirection {
    /// Search toward greater columns.
    Forward,
    /// Search toward smaller columns.
    Backward,
}

/// Repeatable `f`/`F`/`t`/`T` motion state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FindMotion {
    /// Search direction.
    pub direction: FindDirection,
    /// Stop one character before the target.
    pub till: bool,
    /// Target byte.
    pub target: u8,
}

fn line_len(lines: &[Vec<u8>], lnum: usize) -> usize {
    lines.get(lnum.saturating_sub(1)).map_or(0, Vec::len)
}

fn clamp(lines: &[Vec<u8>], mut pos: Position) -> Position {
    pos.lnum = pos.lnum.clamp(1, lines.len().max(1));
    pos.col = pos.col.min(line_len(lines, pos.lnum).saturating_sub(1));
    pos
}

fn classify(byte: u8, big: bool) -> u8 {
    if byte.is_ascii_whitespace() { 0 } else if big || byte.is_ascii_alphanumeric() || byte == b'_' { 1 } else { 2 }
}

fn flatten(lines: &[Vec<u8>]) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = Vec::new();
    let mut starts = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        starts.push(bytes.len());
        bytes.extend_from_slice(line);
        if index + 1 < lines.len() { bytes.push(b'\n'); }
    }
    (bytes, starts)
}

fn offset_of(starts: &[usize], pos: Position) -> usize {
    starts.get(pos.lnum.saturating_sub(1)).copied().map_or(pos.col, |start| start.saturating_add(pos.col))
}

fn pos_of(lines: &[Vec<u8>], starts: &[usize], offset: usize) -> Position {
    let line = starts.partition_point(|start| *start <= offset).saturating_sub(1);
    Position { lnum: line + 1, col: offset.saturating_sub(starts[line]).min(lines[line].len().saturating_sub(1)) }
}

fn word_forward(lines: &[Vec<u8>], start: Position, count: usize, big: bool, end: bool) -> Position {
    let (bytes, starts) = flatten(lines);
    if bytes.is_empty() { return Position { lnum: 1, col: 0 }; }
    let mut at = offset_of(&starts, start).min(bytes.len() - 1);
    for _ in 0..count.max(1) {
        if end {
            while at + 1 < bytes.len() && classify(bytes[at + 1], big) == 0 { at += 1; }
            let class = classify(bytes[at], big);
            if class != 0 { while at + 1 < bytes.len() && classify(bytes[at + 1], big) == class { at += 1; } }
            else if at + 1 < bytes.len() { at += 1; let c = classify(bytes[at], big); while at + 1 < bytes.len() && classify(bytes[at + 1], big) == c { at += 1; } }
        } else {
            let class = classify(bytes[at], big);
            while at + 1 < bytes.len() && classify(bytes[at + 1], big) == class { at += 1; }
            while at + 1 < bytes.len() && classify(bytes[at + 1], big) == 0 { at += 1; }
            if at + 1 < bytes.len() { at += 1; }
        }
    }
    pos_of(lines, &starts, at)
}

fn word_backward(lines: &[Vec<u8>], start: Position, count: usize, big: bool, end: bool) -> Position {
    let (bytes, starts) = flatten(lines);
    if bytes.is_empty() { return Position { lnum: 1, col: 0 }; }
    let mut at = offset_of(&starts, start).min(bytes.len() - 1);
    for _ in 0..count.max(1) {
        if at == 0 { break; }
        at -= 1;
        while at > 0 && classify(bytes[at], big) == 0 { at -= 1; }
        if end {
            let class = classify(bytes[at], big);
            while at > 0 && classify(bytes[at - 1], big) == class { at -= 1; }
            if at > 0 { at -= 1; while at > 0 && classify(bytes[at], big) == 0 { at -= 1; } }
        } else {
            let class = classify(bytes[at], big);
            while at > 0 && classify(bytes[at - 1], big) == class { at -= 1; }
        }
    }
    pos_of(lines, &starts, at)
}

/// Resolves a complete one- or two-key normal motion.
pub fn resolve(lines: &[Vec<u8>], start: Position, command: &str, count: usize, startofline: bool, viewport: (usize, usize)) -> Option<Motion> {
    let count = count.max(1);
    let mut target = start;
    let mut kind = MotionKind::CharacterWise;
    let mut inclusive = false;
    let mut is_jump = false;
    match command {
        "h" => target.col = target.col.saturating_sub(count),
        "l" => target.col = target.col.saturating_add(count),
        "j" => { target.lnum = target.lnum.saturating_add(count); kind = MotionKind::LineWise; },
        "k" => { target.lnum = target.lnum.saturating_sub(count); kind = MotionKind::LineWise; },
        "0" => target.col = 0,
        "^" => target.col = lines.get(start.lnum - 1)?.iter().position(|b| !b.is_ascii_whitespace()).map_or(0, |col| col),
        "$" => { target.lnum = target.lnum.saturating_add(count - 1); target.col = line_len(lines, target.lnum).saturating_sub(1); inclusive = true; }
        "g_" => { target.lnum = target.lnum.saturating_add(count - 1); target.col = lines.get(target.lnum.saturating_sub(1))?.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(0, |col| col); inclusive = true; }
        "w" => target = word_forward(lines, start, count, false, false),
        "W" => target = word_forward(lines, start, count, true, false),
        "e" => { target = word_forward(lines, start, count, false, true); inclusive = true; }
        "E" => { target = word_forward(lines, start, count, true, true); inclusive = true; }
        "b" => target = word_backward(lines, start, count, false, false),
        "B" => target = word_backward(lines, start, count, true, false),
        "ge" => { target = word_backward(lines, start, count, false, true); inclusive = true; }
        "gE" => { target = word_backward(lines, start, count, true, true); inclusive = true; }
        "gg" => { target.lnum = count.min(lines.len().max(1)); target.col = if startofline { first_nonblank(lines, target.lnum) } else { start.col }; kind = MotionKind::LineWise; is_jump = true; }
        "G_count" => { target.lnum = count.min(lines.len().max(1)); target.col = if startofline { first_nonblank(lines, target.lnum) } else { start.col }; kind = MotionKind::LineWise; is_jump = true; }
        "G" => { target.lnum = lines.len().max(1); target.col = if startofline { first_nonblank(lines, target.lnum) } else { start.col }; kind = MotionKind::LineWise; is_jump = true; }
        "{" => { for _ in 0..count { target.lnum = previous_blank(lines, target.lnum); } target.col = 0; is_jump = true; }
        "}" => { for _ in 0..count { target.lnum = next_blank(lines, target.lnum); } target.col = 0; is_jump = true; }
        "(" => { target = sentence_boundary(lines, start, count, false); is_jump = true; }
        ")" => { target = sentence_boundary(lines, start, count, true); is_jump = true; }
        "H" => { target.lnum = viewport.0.saturating_add(count - 1).min(viewport.1); target.col = first_nonblank(lines, target.lnum); kind = MotionKind::LineWise; is_jump = true; }
        "M" => { target.lnum = viewport.0.saturating_add(viewport.1.saturating_sub(viewport.0) / 2); target.col = first_nonblank(lines, target.lnum); kind = MotionKind::LineWise; is_jump = true; }
        "L" => { target.lnum = viewport.1.saturating_sub(count - 1).max(viewport.0); target.col = first_nonblank(lines, target.lnum); kind = MotionKind::LineWise; is_jump = true; }
        "%" => { target = matching_pair(lines, start)?; inclusive = true; is_jump = true; }
        _ => return None,
    }
    Some(Motion { target: clamp(lines, target), kind, inclusive, is_jump })
}

/// Resolves a repeatable character-find motion on the current line.
pub fn resolve_find(lines: &[Vec<u8>], start: Position, find: FindMotion, count: usize) -> Option<Motion> {
    let line = lines.get(start.lnum.checked_sub(1)?)?;
    let mut found = start.col;
    for _ in 0..count.max(1) {
        found = match find.direction {
            FindDirection::Forward => line.get(found.saturating_add(1)..)?.iter().position(|b| *b == find.target)?.saturating_add(found + 1),
            FindDirection::Backward => line.get(..found)?.iter().rposition(|b| *b == find.target)?,
        };
    }
    let col = if find.till { match find.direction { FindDirection::Forward => found.saturating_sub(1), FindDirection::Backward => found.saturating_add(1).min(line.len().saturating_sub(1)) } } else { found };
    Some(Motion { target: Position { lnum: start.lnum, col }, kind: MotionKind::CharacterWise, inclusive: !find.till, is_jump: false })
}

fn first_nonblank(lines: &[Vec<u8>], lnum: usize) -> usize { lines.get(lnum.saturating_sub(1)).and_then(|line| line.iter().position(|b| !b.is_ascii_whitespace())).map_or(0, |col| col) }
fn previous_blank(lines: &[Vec<u8>], lnum: usize) -> usize { (1..lnum).rev().find(|n| lines[*n - 1].is_empty()).map_or(1, |line| line) }
fn next_blank(lines: &[Vec<u8>], lnum: usize) -> usize { ((lnum + 1)..=lines.len()).find(|n| lines[*n - 1].is_empty()).map_or(lines.len().max(1), |line| line) }

fn sentence_boundary(lines: &[Vec<u8>], start: Position, count: usize, forward: bool) -> Position {
    let (bytes, starts) = flatten(lines);
    if bytes.is_empty() { return Position { lnum: 1, col: 0 }; }
    let mut at = offset_of(&starts, start).min(bytes.len() - 1);
    for _ in 0..count.max(1) {
        if forward {
            while at + 1 < bytes.len() { at += 1; if matches!(bytes[at - 1], b'.' | b'!' | b'?') && (bytes[at].is_ascii_whitespace()) { while at < bytes.len() && bytes[at].is_ascii_whitespace() { at += 1; } break; } }
            at = at.min(bytes.len() - 1);
        } else {
            at = at.saturating_sub(1);
            while at > 0 { if matches!(bytes[at - 1], b'.' | b'!' | b'?') && bytes[at].is_ascii_whitespace() { while at < bytes.len() && bytes[at].is_ascii_whitespace() { at += 1; } break; } at -= 1; }
        }
    }
    pos_of(lines, &starts, at)
}

fn matching_pair(lines: &[Vec<u8>], start: Position) -> Option<Position> {
    let (bytes, starts) = flatten(lines);
    let mut at = offset_of(&starts, start);
    while at < bytes.len() && !matches!(bytes[at], b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'<' | b'>') { at += 1; }
    let token = *bytes.get(at)?;
    let (mate, direction) = match token { b'(' => (b')', 1isize), b'[' => (b']', 1), b'{' => (b'}', 1), b'<' => (b'>', 1), b')' => (b'(', -1), b']' => (b'[', -1), b'}' => (b'{', -1), b'>' => (b'<', -1), _ => return None };
    let mut depth = 1usize;
    let mut cursor = at as isize;
    while depth != 0 {
        cursor += direction;
        if cursor < 0 || cursor as usize >= bytes.len() { return None; }
        let byte = bytes[cursor as usize];
        if byte == token { depth += 1; } else if byte == mate { depth -= 1; }
    }
    Some(pos_of(lines, &starts, cursor as usize))
}
