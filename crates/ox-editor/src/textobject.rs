//! Text-object selection (`textobject.c`).

use ox_text::Position;

use crate::{EditRange, MotionKind};

/// Resolves an `i`/`a` text object around `cursor`.
pub fn resolve(lines: &[Vec<u8>], cursor: Position, inner: bool, object: char, count: usize) -> Option<EditRange> {
    match object {
        'w' => word(lines, cursor, inner, false, count),
        'W' => word(lines, cursor, inner, true, count),
        'b' | '(' | ')' => delimited(lines, cursor, inner, b'(', b')', count),
        '[' | ']' => delimited(lines, cursor, inner, b'[', b']', count),
        '{' | '}' | 'B' => delimited(lines, cursor, inner, b'{', b'}', count),
        '<' | '>' => delimited(lines, cursor, inner, b'<', b'>', count),
        '"' | '\'' | '`' => quoted(lines, cursor, inner, object as u8, count),
        'p' => paragraph(lines, cursor, inner, count),
        's' => sentence(lines, cursor, inner, count),
        _ => None,
    }
}

fn flattened(lines: &[Vec<u8>]) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = Vec::new(); let mut starts = Vec::new();
    for (index, line) in lines.iter().enumerate() { starts.push(bytes.len()); bytes.extend_from_slice(line); if index + 1 < lines.len() { bytes.push(b'\n'); } }
    (bytes, starts)
}
fn offset(starts: &[usize], pos: Position) -> Option<usize> { starts.get(pos.lnum.saturating_sub(1)).copied().map(|start| start.saturating_add(pos.col)) }
fn position(lines: &[Vec<u8>], starts: &[usize], at: usize) -> Position { let index = starts.partition_point(|s| *s <= at).saturating_sub(1); Position { lnum: index + 1, col: at.saturating_sub(starts[index]).min(lines[index].len().saturating_sub(1)) } }
fn range(lines: &[Vec<u8>], starts: &[usize], start: usize, end: usize, kind: MotionKind) -> EditRange { EditRange { start: position(lines, starts, start), end: position(lines, starts, end), kind, inclusive: true } }
fn class(byte: u8, big: bool) -> u8 { if byte.is_ascii_whitespace() { 0 } else if big || byte.is_ascii_alphanumeric() || byte == b'_' { 1 } else { 2 } }

fn word(lines: &[Vec<u8>], cursor: Position, inner: bool, big: bool, count: usize) -> Option<EditRange> {
    let (bytes, starts) = flattened(lines); let mut at = offset(&starts, cursor)?.min(bytes.len().checked_sub(1)?);
    if class(bytes[at], big) == 0 { while at + 1 < bytes.len() && class(bytes[at], big) == 0 { at += 1; } }
    let c = class(bytes[at], big); let mut start = at; while start > 0 && class(bytes[start - 1], big) == c { start -= 1; }
    let mut end = at;
    for index in 0..count.max(1) {
        let current = class(bytes[end], big);
        while end + 1 < bytes.len() && class(bytes[end + 1], big) == current { end += 1; }
        if index + 1 < count.max(1) {
            while end + 1 < bytes.len() && class(bytes[end + 1], big) == 0 { end += 1; }
            if end + 1 < bytes.len() { end += 1; }
        }
    }
    if !inner { while end + 1 < bytes.len() && class(bytes[end + 1], big) == 0 { end += 1; } }
    if inner { while end > start && class(bytes[end], big) == 0 { end -= 1; } }
    Some(range(lines, &starts, start, end, MotionKind::CharacterWise))
}

fn delimited(lines: &[Vec<u8>], cursor: Position, inner: bool, open: u8, close: u8, count: usize) -> Option<EditRange> {
    let (bytes, starts) = flattened(lines); let at = offset(&starts, cursor)?; let mut left = None; let mut depth = 0usize;
    for index in (0..=at.min(bytes.len().saturating_sub(1))).rev() { if bytes[index] == close { depth += 1; } else if bytes[index] == open { if depth == 0 { left = Some(index); break; } depth -= 1; } }
    let mut left = left?; let mut right = matching_close(&bytes, left, open, close)?;
    for _ in 1..count.max(1) { left = (0..left).rev().find(|index| bytes[*index] == open)?; right = matching_close(&bytes, left, open, close)?; }
    if inner { left += 1; right = right.saturating_sub(1); if right < left { right = left; } }
    Some(range(lines, &starts, left, right, MotionKind::CharacterWise))
}
fn matching_close(bytes: &[u8], open_at: usize, open: u8, close: u8) -> Option<usize> { let mut depth = 0usize; for (index, byte) in bytes.iter().enumerate().skip(open_at) { if *byte == open { depth += 1; } else if *byte == close { depth -= 1; if depth == 0 { return Some(index); } } } None }

fn quoted(lines: &[Vec<u8>], cursor: Position, inner: bool, quote: u8, count: usize) -> Option<EditRange> {
    let line = lines.get(cursor.lnum.checked_sub(1)?)?; let escaped = |index: usize| index > 0 && line[index - 1] == b'\\';
    let mut left = (0..=cursor.col.min(line.len().saturating_sub(1))).rev().find(|index| line[*index] == quote && !escaped(*index))?;
    let mut right = (left + 1..line.len()).find(|index| line[*index] == quote && !escaped(*index))?;
    for _ in 1..count.max(1) { left = (0..left).rev().find(|index| line[*index] == quote && !escaped(*index))?; right = (right + 1..line.len()).find(|index| line[*index] == quote && !escaped(*index))?; }
    if inner { left += 1; right = right.saturating_sub(1); }
    Some(EditRange { start: Position { lnum: cursor.lnum, col: left }, end: Position { lnum: cursor.lnum, col: right }, kind: MotionKind::CharacterWise, inclusive: true })
}

fn paragraph(lines: &[Vec<u8>], cursor: Position, inner: bool, count: usize) -> Option<EditRange> {
    let mut start = cursor.lnum.clamp(1, lines.len()); while start > 1 && !lines[start - 2].is_empty() { start -= 1; }
    let mut end = cursor.lnum.clamp(1, lines.len()); for _ in 0..count.max(1) { while end < lines.len() && !lines[end].is_empty() { end += 1; } if !inner { while end < lines.len() && lines[end].is_empty() { end += 1; } } }
    Some(EditRange { start: Position { lnum: start, col: 0 }, end: Position { lnum: end, col: lines[end - 1].len().saturating_sub(1) }, kind: MotionKind::LineWise, inclusive: true })
}

fn sentence(lines: &[Vec<u8>], cursor: Position, inner: bool, count: usize) -> Option<EditRange> {
    let (bytes, starts) = flattened(lines); let at = offset(&starts, cursor)?.min(bytes.len().checked_sub(1)?);
    let mut start = at; while start > 0 { if matches!(bytes[start - 1], b'.' | b'!' | b'?') { break; } start -= 1; }
    while start < bytes.len() && bytes[start].is_ascii_whitespace() { start += 1; }
    let mut end = start;
    for index in 0..count.max(1) {
        while end + 1 < bytes.len() && !matches!(bytes[end], b'.' | b'!' | b'?') { end += 1; }
        if index + 1 < count.max(1) {
            while end + 1 < bytes.len() && bytes[end + 1].is_ascii_whitespace() { end += 1; }
            if end + 1 < bytes.len() { end += 1; }
        }
    }
    if !inner { while end + 1 < bytes.len() && bytes[end + 1].is_ascii_whitespace() { end += 1; } }
    Some(range(lines, &starts, start, end, MotionKind::CharacterWise))
}
