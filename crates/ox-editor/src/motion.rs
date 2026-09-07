//! Normal-mode motions and their operator semantics.

use ox_text::Position;

/// The shape an operator assigns to a motion range (`ops.c`: `motion_type`).
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
    /// Vertical motions keep `w_curswant` (`move.c` `nv_up`/`nv_down`).
    pub keep_curswant: bool,
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
    pos.col = lines.get(pos.lnum.saturating_sub(1)).map_or(0, |line| {
        let col = pos.col.min(prev_char_boundary(line, line.len()));
        prev_char_boundary(line, col.saturating_add(1))
    });
    pos
}
/// Returns the first UTF-8 scalar boundary strictly after `col`.
///
/// Invalid UTF-8 falls back to one-byte movement, matching the editor's
/// existing treatment of undecodable buffer contents.
#[must_use]
pub fn next_char_boundary(line: &[u8], col: usize) -> usize {
    let col = col.min(line.len());
    let Some(text) = std::str::from_utf8(line).ok() else {
        return col.saturating_add(1).min(line.len());
    };
    let mut next = col.saturating_add(1).min(line.len());
    while next < line.len() && !text.is_char_boundary(next) {
        next += 1;
    }
    next
}

/// Returns the last UTF-8 scalar boundary strictly before `col`.
///
/// Invalid UTF-8 falls back to one-byte movement, matching the editor's
/// existing treatment of undecodable buffer contents.
#[must_use]
pub fn prev_char_boundary(line: &[u8], col: usize) -> usize {
    let col = col.min(line.len());
    let Some(text) = std::str::from_utf8(line).ok() else {
        return col.saturating_sub(1);
    };
    let mut previous = col.saturating_sub(1);
    while previous > 0 && !text.is_char_boundary(previous) {
        previous -= 1;
    }
    previous
}

fn classify(byte: u8, big: bool) -> u8 {
    if byte.is_ascii_whitespace() {
        0
    } else if big || byte.is_ascii_alphanumeric() || byte == b'_' {
        1
    } else {
        2
    }
}

fn flatten(lines: &[Vec<u8>]) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = Vec::new();
    let mut starts = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        starts.push(bytes.len());
        bytes.extend_from_slice(line);
        if index + 1 < lines.len() {
            bytes.push(b'\n');
        }
    }
    (bytes, starts)
}

fn offset_of(starts: &[usize], pos: Position) -> usize {
    starts
        .get(pos.lnum.saturating_sub(1))
        .copied()
        .map_or(pos.col, |start| start.saturating_add(pos.col))
}

fn pos_of(lines: &[Vec<u8>], starts: &[usize], offset: usize) -> Position {
    let line = starts
        .partition_point(|start| *start <= offset)
        .saturating_sub(1);
    Position {
        lnum: line + 1,
        col: offset
            .saturating_sub(starts[line])
            .min(lines[line].len().saturating_sub(1)),
    }
}

fn word_forward(
    lines: &[Vec<u8>],
    start: Position,
    count: usize,
    big: bool,
    end: bool,
) -> Position {
    let (bytes, starts) = flatten(lines);
    if bytes.is_empty() {
        return Position { lnum: 1, col: 0 };
    }
    let mut at = offset_of(&starts, start).min(bytes.len() - 1);
    for _ in 0..count.max(1) {
        if end {
            while at + 1 < bytes.len() && classify(bytes[at + 1], big) == 0 {
                at += 1;
            }
            let class = classify(bytes[at], big);
            if class != 0 {
                while at + 1 < bytes.len() && classify(bytes[at + 1], big) == class {
                    at += 1;
                }
            } else if at + 1 < bytes.len() {
                at += 1;
                let c = classify(bytes[at], big);
                while at + 1 < bytes.len() && classify(bytes[at + 1], big) == c {
                    at += 1;
                }
            }
        } else {
            let class = classify(bytes[at], big);
            while at + 1 < bytes.len() && classify(bytes[at + 1], big) == class {
                at += 1;
            }
            while at + 1 < bytes.len() && classify(bytes[at + 1], big) == 0 {
                at += 1;
            }
            if at + 1 < bytes.len() {
                at += 1;
            }
        }
    }
    pos_of(lines, &starts, at)
}

fn word_backward(
    lines: &[Vec<u8>],
    start: Position,
    count: usize,
    big: bool,
    end: bool,
) -> Position {
    let (bytes, starts) = flatten(lines);
    if bytes.is_empty() {
        return Position { lnum: 1, col: 0 };
    }
    let mut at = offset_of(&starts, start).min(bytes.len() - 1);
    for _ in 0..count.max(1) {
        if at == 0 {
            break;
        }
        at -= 1;
        while at > 0 && classify(bytes[at], big) == 0 {
            at -= 1;
        }
        if end {
            let class = classify(bytes[at], big);
            while at > 0 && classify(bytes[at - 1], big) == class {
                at -= 1;
            }
            if at > 0 {
                at -= 1;
                while at > 0 && classify(bytes[at], big) == 0 {
                    at -= 1;
                }
            }
        } else {
            let class = classify(bytes[at], big);
            while at > 0 && classify(bytes[at - 1], big) == class {
                at -= 1;
            }
        }
    }
    pos_of(lines, &starts, at)
}

/// Resolves a complete one- or two-key normal motion.
#[must_use]
pub fn resolve(
    lines: &[Vec<u8>],
    start: Position,
    command: &str,
    count: usize,
    startofline: bool,
    viewport: (usize, usize),
) -> Option<Motion> {
    let count = count.max(1);
    let mut motion = resolve_command(lines, start, command, count, startofline, viewport)?;
    motion.target = clamp(lines, motion.target);
    Some(motion)
}

/// Dispatches a motion command to its resolver family, mirroring the arm
/// layout of `nv_*` handlers in `normal.c`.
fn resolve_command(
    lines: &[Vec<u8>],
    start: Position,
    command: &str,
    count: usize,
    startofline: bool,
    viewport: (usize, usize),
) -> Option<Motion> {
    match command {
        "h" | "l" | "0" | "^" | "|" | "$" | "g_" => charwise_motion(lines, start, command, count),
        "j" | "k" => Some(vertical_motion(start, command, count)),
        "w" | "W" | "e" | "E" | "b" | "B" | "ge" | "gE" => {
            word_motion(lines, start, command, count)
        }
        "gg" | "G" | "G_count" => Some(goto_motion(lines, start, command, count, startofline)),
        "{" | "}" => Some(blank_motion(lines, start, command, count)),
        "(" | ")" => Some(sentence_motion(lines, start, command, count)),
        "H" | "M" | "L" => Some(viewport_motion(lines, command, count, viewport)),
        "%" | "[#" | "]#" | "[/" | "[*" | "]/" | "]*" | "gD" | "gd" => {
            jump_motion(lines, start, command)
        }
        _ => None,
    }
}

/// Horizontal and line-end motions (`h`, `l`, `0`, `^`, `|`, `$`, `g_`).
fn charwise_motion(
    lines: &[Vec<u8>],
    start: Position,
    command: &str,
    count: usize,
) -> Option<Motion> {
    let mut target = start;
    let mut inclusive = false;
    match command {
        "h" => {
            for _ in 0..count {
                target.col = prev_char_boundary(lines.get(target.lnum - 1)?, target.col);
            }
        }
        "l" => {
            for _ in 0..count {
                target.col = next_char_boundary(lines.get(target.lnum - 1)?, target.col);
            }
        }
        "0" => target.col = 0,
        "^" => {
            target.col = lines
                .get(start.lnum - 1)?
                .iter()
                .position(|b| !b.is_ascii_whitespace())
                .unwrap_or(0);
        }
        "|" => {
            target.col = count
                .saturating_sub(1)
                .min(line_len(lines, target.lnum).saturating_sub(1));
        }
        "$" => {
            target.lnum = target.lnum.saturating_add(count - 1);
            target.col = line_len(lines, target.lnum).saturating_sub(1);
            inclusive = true;
        }
        "g_" => {
            target.lnum = target.lnum.saturating_add(count - 1);
            target.col = lines
                .get(target.lnum.saturating_sub(1))?
                .iter()
                .rposition(|b| !b.is_ascii_whitespace())
                .unwrap_or(0);
            inclusive = true;
        }
        _ => return None,
    }
    Some(Motion {
        target,
        kind: MotionKind::CharacterWise,
        inclusive,
        is_jump: false,
        keep_curswant: false,
    })
}

/// Vertical motions (`j`, `k`) that keep the curswant unchanged.
fn vertical_motion(start: Position, command: &str, count: usize) -> Motion {
    let lnum = if command == "j" {
        start.lnum.saturating_add(count)
    } else {
        start.lnum.saturating_sub(count)
    };
    Motion {
        target: Position {
            lnum,
            col: start.col,
        },
        kind: MotionKind::LineWise,
        inclusive: false,
        is_jump: false,
        keep_curswant: true,
    }
}

/// Word motions (`w`, `W`, `e`, `E`, `b`, `B`, `ge`, `gE`); the `e`-shaped
/// variants are inclusive.
fn word_motion(lines: &[Vec<u8>], start: Position, command: &str, count: usize) -> Option<Motion> {
    let (big, end, backward) = match command {
        "w" => (false, false, false),
        "W" => (true, false, false),
        "e" => (false, true, false),
        "E" => (true, true, false),
        "b" => (false, false, true),
        "B" => (true, false, true),
        "ge" => (false, true, true),
        "gE" => (true, true, true),
        _ => return None,
    };
    let target = if backward {
        word_backward(lines, start, count, big, end)
    } else {
        word_forward(lines, start, count, big, end)
    };
    Some(Motion {
        target,
        kind: MotionKind::CharacterWise,
        inclusive: end,
        is_jump: false,
        keep_curswant: false,
    })
}

/// Absolute-line motions (`gg`, `G`, `G_count`).
fn goto_motion(
    lines: &[Vec<u8>],
    start: Position,
    command: &str,
    count: usize,
    startofline: bool,
) -> Motion {
    let lnum = if command == "G" {
        lines.len().max(1)
    } else {
        count.min(lines.len().max(1))
    };
    let col = if startofline {
        first_nonblank(lines, lnum)
    } else {
        start.col
    };
    Motion {
        target: Position { lnum, col },
        kind: MotionKind::LineWise,
        inclusive: false,
        is_jump: true,
        keep_curswant: false,
    }
}

/// Paragraph motions (`{`, `}`): column 0 of the counted blank line.
fn blank_motion(lines: &[Vec<u8>], start: Position, command: &str, count: usize) -> Motion {
    let mut target = start;
    for _ in 0..count {
        target.lnum = if command == "{" {
            previous_blank(lines, target.lnum)
        } else {
            next_blank(lines, target.lnum)
        };
    }
    target.col = 0;
    Motion {
        target,
        kind: MotionKind::CharacterWise,
        inclusive: false,
        is_jump: true,
        keep_curswant: false,
    }
}

/// Sentence motions (`(`, `)`).
fn sentence_motion(lines: &[Vec<u8>], start: Position, command: &str, count: usize) -> Motion {
    let target = sentence_boundary(lines, start, count, command == ")");
    Motion {
        target,
        kind: MotionKind::CharacterWise,
        inclusive: false,
        is_jump: true,
        keep_curswant: false,
    }
}

/// Viewport motions (`H`, `M`, `L`).
fn viewport_motion(
    lines: &[Vec<u8>],
    command: &str,
    count: usize,
    viewport: (usize, usize),
) -> Motion {
    let lnum = match command {
        "H" => viewport.0.saturating_add(count - 1).min(viewport.1),
        "M" => viewport
            .0
            .saturating_add(viewport.1.saturating_sub(viewport.0) / 2),
        _ => viewport.1.saturating_sub(count - 1).max(viewport.0),
    };
    let target = Position {
        lnum,
        col: first_nonblank(lines, lnum),
    };
    Motion {
        target,
        kind: MotionKind::LineWise,
        inclusive: false,
        is_jump: true,
        keep_curswant: false,
    }
}

/// Jump motions: `%`, `[#`, `]#`, `[/`, `[*`, `]/`, `]*`, `gD`, `gd`; only `%`
/// is inclusive.
fn jump_motion(lines: &[Vec<u8>], start: Position, command: &str) -> Option<Motion> {
    let (target, inclusive) = match command {
        "%" => (matching_pair(lines, start)?, true),
        "[#" => (adjacent_hash(lines, start, false)?, false),
        "]#" => (adjacent_hash(lines, start, true)?, false),
        "[/" | "[*" => (comment_boundary(lines, start, false)?, false),
        "]/" | "]*" => (comment_boundary(lines, start, true)?, false),
        "gD" | "gd" => (goto_declaration(lines, start, command == "gd")?, false),
        _ => return None,
    };
    Some(Motion {
        target,
        kind: MotionKind::CharacterWise,
        inclusive,
        is_jump: true,
        keep_curswant: false,
    })
}

/// Resolves a repeatable character-find motion on the current line.
#[must_use]
pub fn resolve_find(
    lines: &[Vec<u8>],
    start: Position,
    find: FindMotion,
    count: usize,
) -> Option<Motion> {
    let line = lines.get(start.lnum.checked_sub(1)?)?;
    let mut found = start.col;
    for _ in 0..count.max(1) {
        found = match find.direction {
            FindDirection::Forward => line
                .get(found.saturating_add(1)..)?
                .iter()
                .position(|b| *b == find.target)?
                .saturating_add(found + 1),
            FindDirection::Backward => {
                line.get(..found)?.iter().rposition(|b| *b == find.target)?
            }
        };
    }
    let col = if find.till {
        match find.direction {
            FindDirection::Forward => found.saturating_sub(1),
            FindDirection::Backward => found.saturating_add(1).min(line.len().saturating_sub(1)),
        }
    } else {
        found
    };
    Some(Motion {
        target: Position {
            lnum: start.lnum,
            col,
        },
        kind: MotionKind::CharacterWise,
        inclusive: !find.till,
        is_jump: false,
        keep_curswant: false,
    })
}

fn first_nonblank(lines: &[Vec<u8>], lnum: usize) -> usize {
    lines
        .get(lnum.saturating_sub(1))
        .and_then(|line| line.iter().position(|b| !b.is_ascii_whitespace()))
        .unwrap_or(0)
}
fn previous_blank(lines: &[Vec<u8>], lnum: usize) -> usize {
    (1..lnum)
        .rev()
        .find(|n| lines[*n - 1].is_empty())
        .unwrap_or(1)
}
fn next_blank(lines: &[Vec<u8>], lnum: usize) -> usize {
    ((lnum + 1)..=lines.len())
        .find(|n| lines[*n - 1].is_empty())
        .unwrap_or(lines.len().max(1))
}

/// A `.`/`!`/`?` ends a sentence only when trailing closers `)]"'` give way to
/// whitespace or the end of the text (`textobject.c:103-131`).
fn sentence_terminated(bytes: &[u8], at: usize) -> bool {
    let mut j = at + 1;
    while j < bytes.len() && matches!(bytes[j], b')' | b']' | b'"' | b'\'') {
        j += 1;
    }
    j >= bytes.len() || bytes[j].is_ascii_whitespace()
}

fn sentence_boundary(lines: &[Vec<u8>], start: Position, count: usize, forward: bool) -> Position {
    let (bytes, starts) = flatten(lines);
    if bytes.is_empty() {
        return Position { lnum: 1, col: 0 };
    }
    let mut at = offset_of(&starts, start).min(bytes.len() - 1);
    let skip_tail = |at: usize| {
        let mut at = at;
        while at < bytes.len()
            && (bytes[at].is_ascii_whitespace() || matches!(bytes[at], b')' | b']' | b'"' | b'\''))
        {
            at += 1;
        }
        at.min(bytes.len() - 1)
    };
    for _ in 0..count.max(1) {
        if forward {
            while at + 1 < bytes.len() {
                at += 1;
                if matches!(bytes[at - 1], b'.' | b'!' | b'?')
                    && sentence_terminated(&bytes, at - 1)
                {
                    at = skip_tail(at);
                    break;
                }
            }
            at = at.min(bytes.len() - 1);
        } else {
            at = at.saturating_sub(1);
            while at > 0 {
                if matches!(bytes[at - 1], b'.' | b'!' | b'?')
                    && sentence_terminated(&bytes, at - 1)
                {
                    at = skip_tail(at);
                    break;
                }
                at -= 1;
            }
        }
    }
    pos_of(lines, &starts, at)
}

fn matching_pair(lines: &[Vec<u8>], start: Position) -> Option<Position> {
    if let Some(target) = match_comment(lines, start) {
        return Some(target);
    }
    let line = lines.get(start.lnum.checked_sub(1)?)?;
    if hash_at_or_before(line, start.col) {
        return match_hash(lines, start);
    }
    if let Some(target) = match_brace(lines, start) {
        return Some(target);
    }
    match_hash(lines, start)
}

fn match_brace(lines: &[Vec<u8>], start: Position) -> Option<Position> {
    let (bytes, starts) = flatten(lines);
    let mut at = offset_of(&starts, start);
    while at < bytes.len()
        && !matches!(
            bytes[at],
            b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'<' | b'>'
        )
    {
        at += 1;
    }
    let token = *bytes.get(at)?;
    let (mate, direction) = match token {
        b'(' => (b')', 1isize),
        b'[' => (b']', 1),
        b'{' => (b'}', 1),
        b'<' => (b'>', 1),
        b')' => (b'(', -1),
        b']' => (b'[', -1),
        b'}' => (b'{', -1),
        b'>' => (b'<', -1),
        _ => return None,
    };
    let mut depth = 1usize;
    let mut cursor = at;
    while depth != 0 {
        cursor = cursor.checked_add_signed(direction)?;
        let byte = *bytes.get(cursor)?;
        if byte == token {
            depth += 1;
        } else if byte == mate {
            depth -= 1;
        }
    }
    Some(pos_of(lines, &starts, cursor))
}

fn match_comment(lines: &[Vec<u8>], start: Position) -> Option<Position> {
    let line = lines.get(start.lnum.checked_sub(1)?)?;
    let col = start.col;
    let forward = if (line.get(col) == Some(&b'/') && line.get(col + 1) == Some(&b'*'))
        || (line.get(col) == Some(&b'*') && col > 0 && line[col - 1] == b'/')
    {
        true
    } else if (line.get(col) == Some(&b'*') && line.get(col + 1) == Some(&b'/'))
        || (line.get(col) == Some(&b'/') && col > 0 && line[col - 1] == b'*')
    {
        false
    } else {
        return None;
    };
    let (bytes, starts) = flatten(lines);
    let at = offset_of(&starts, start);
    if forward {
        for index in at.saturating_add(2)..bytes.len().saturating_sub(1) {
            if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                return Some(pos_of(lines, &starts, index + 1));
            }
        }
    } else {
        for index in (1..at).rev() {
            if bytes[index] == b'*' && bytes[index - 1] == b'/' {
                return Some(pos_of(lines, &starts, index - 1));
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
enum HashKind {
    If,
    El,
    Endif,
}

fn hash_kind(line: &[u8]) -> Option<(usize, HashKind)> {
    let hash = line.iter().position(|byte| !byte.is_ascii_whitespace())?;
    if line[hash] != b'#' {
        return None;
    }
    let rest = line[hash + 1..]
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map_or(line.len(), |index| hash + 1 + index);
    let kind = if line[rest..].starts_with(b"if") {
        HashKind::If
    } else if line[rest..].starts_with(b"endif") {
        HashKind::Endif
    } else if line[rest..].starts_with(b"el") {
        HashKind::El
    } else {
        return None;
    };
    Some((hash, kind))
}

fn hash_at_or_before(line: &[u8], col: usize) -> bool {
    hash_kind(line).is_some_and(|(hash, _)| col <= hash)
}

fn match_hash(lines: &[Vec<u8>], start: Position) -> Option<Position> {
    let (_, kind) = hash_kind(lines.get(start.lnum.checked_sub(1)?)?)?;
    let dir: isize = match kind {
        HashKind::Endif => -1,
        HashKind::If | HashKind::El => 1,
    };
    let mut depth = 0i32;
    let mut lnum = start.lnum;
    loop {
        lnum = lnum.checked_add_signed(dir)?;
        if lnum == 0 || lnum > lines.len() {
            return None;
        }
        let Some((col, next)) = hash_kind(&lines[lnum - 1]) else {
            continue;
        };
        if dir > 0 {
            match next {
                HashKind::If => depth += 1,
                HashKind::El | HashKind::Endif if depth == 0 => {
                    return Some(Position { lnum, col });
                }
                HashKind::Endif => depth -= 1,
                HashKind::El => {}
            }
        } else {
            match next {
                HashKind::If if depth == 0 => {
                    return Some(Position { lnum, col });
                }
                HashKind::If => depth -= 1,
                HashKind::Endif => depth += 1,
                HashKind::El => {}
            }
        }
    }
}

fn adjacent_hash(lines: &[Vec<u8>], start: Position, forward: bool) -> Option<Position> {
    let mut depth = 0i32;
    let mut lnum = start.lnum;
    let dir: isize = if forward { 1 } else { -1 };
    loop {
        lnum = lnum.checked_add_signed(dir)?;
        if lnum == 0 || lnum > lines.len() {
            return None;
        }
        let Some((col, kind)) = hash_kind(&lines[lnum - 1]) else {
            continue;
        };
        if forward {
            match kind {
                HashKind::If => depth += 1,
                HashKind::El | HashKind::Endif if depth == 0 => {
                    return Some(Position { lnum, col });
                }
                HashKind::Endif => depth -= 1,
                HashKind::El => {}
            }
        } else {
            match kind {
                HashKind::Endif => depth += 1,
                HashKind::If | HashKind::El if depth == 0 => {
                    return Some(Position { lnum, col });
                }
                HashKind::If => depth -= 1,
                HashKind::El => {}
            }
        }
    }
}

fn comment_boundary(lines: &[Vec<u8>], start: Position, forward: bool) -> Option<Position> {
    let (bytes, starts) = flatten(lines);
    if bytes.len() < 2 {
        return None;
    }
    let last = bytes.len() - 1;
    let mut at = offset_of(&starts, start).min(last);
    if forward {
        while at < last {
            if bytes[at] == b'*' && bytes[at + 1] == b'/' {
                return Some(pos_of(lines, &starts, at + 1));
            }
            at += 1;
        }
    } else {
        while at > 0 {
            at -= 1;
            if at > 0 && bytes[at] == b'*' && bytes[at - 1] == b'/' {
                return Some(pos_of(lines, &starts, at - 1));
            }
        }
    }
    None
}

/// `gD`/`gd` (`normal.c` `nv_gd`/`find_decl`): first whole-word occurrence of
/// the identifier under the cursor that is not inside a C comment or string.
/// `gD` searches from the file start. `gd` starts at the current `{` and walks
/// back through the non-blank function header (K&R). A match whose `}` ends
/// before the cursor is a closed inner block and is skipped (`1gd`).
fn goto_declaration(lines: &[Vec<u8>], start: Position, local: bool) -> Option<Position> {
    let ident = ident_under(lines, start)?.to_vec();
    let (from, par_lnum) = local_search_start(lines, start, local);
    let mut found = None;
    for (index, line) in lines.iter().enumerate().skip(from) {
        let lnum = index + 1;
        if lnum >= start.lnum {
            return found;
        }
        let mut col = 0;
        while let Some(hit) = next_ident_at(line, col, &ident) {
            col = hit + ident.len();
            if !ident_not_in_comment_or_string(line, hit) {
                if found.is_some() {
                    return found;
                }
                continue;
            }
            if local && closed_inner_block(lines, index, hit, start.lnum) {
                continue;
            }
            if !local {
                return Some(Position { lnum, col: hit });
            }
            if lnum >= par_lnum {
                return found.or(Some(Position { lnum, col: hit }));
            }
            found = Some(Position { lnum, col: hit });
        }
    }
    found
}

pub(crate) fn ident_under(lines: &[Vec<u8>], start: Position) -> Option<&[u8]> {
    let line = lines.get(start.lnum.checked_sub(1)?)?;
    if start.col >= line.len() || classify(line[start.col], false) != 1 {
        return None;
    }
    let mut begin = start.col;
    while begin > 0 && classify(line[begin - 1], false) == 1 {
        begin -= 1;
    }
    let mut end = start.col + 1;
    while end < line.len() && classify(line[end], false) == 1 {
        end += 1;
    }
    Some(&line[begin..end])
}

pub(crate) fn next_ident_at(line: &[u8], mut col: usize, ident: &[u8]) -> Option<usize> {
    while col + ident.len() <= line.len() {
        if &line[col..col + ident.len()] == ident {
            let before_ok = col == 0 || classify(line[col - 1], false) != 1;
            let after = col + ident.len();
            let after_ok = after == line.len() || classify(line[after], false) != 1;
            if before_ok && after_ok {
                return Some(col);
            }
        }
        col += 1;
    }
    None
}

/// `normal.c` `is_ident`: `line[offset]` is outside a C comment or string.
fn ident_not_in_comment_or_string(line: &[u8], offset: usize) -> bool {
    let mut in_comment = false;
    let mut in_string = 0u8;
    let mut prev = 0u8;
    for &byte in line.iter().take(offset) {
        if in_string != 0 {
            if prev != b'\\' && byte == in_string {
                in_string = 0;
            }
        } else if !in_comment && (byte == b'"' || byte == b'\'') {
            in_string = byte;
        } else if in_comment {
            if prev == b'*' && byte == b'/' {
                in_comment = false;
            }
        } else if prev == b'/' && byte == b'*' {
            in_comment = true;
        } else if prev == b'/' && byte == b'/' {
            return false;
        }
        prev = byte;
    }
    !in_comment && in_string == 0
}

fn local_search_start(lines: &[Vec<u8>], start: Position, local: bool) -> (usize, usize) {
    if !local {
        return (0, 1);
    }
    let Some(open) = enclosing_open_brace(lines, start) else {
        return (0, 1);
    };
    let mut from = open;
    while from > 0 && !lines[from - 1].iter().all(u8::is_ascii_whitespace) {
        from -= 1;
    }
    (from, open + 1)
}

fn enclosing_open_brace(lines: &[Vec<u8>], start: Position) -> Option<usize> {
    let mut depth = 0i32;
    for index in (0..start.lnum).rev() {
        let line = &lines[index];
        let last = if index + 1 == start.lnum {
            start.col.min(line.len())
        } else {
            line.len()
        };
        for col in (0..last).rev() {
            match line[col] {
                b'}' => depth += 1,
                b'{' if depth == 0 => return Some(index),
                b'{' => depth -= 1,
                _ => {}
            }
        }
    }
    None
}

fn closed_inner_block(
    lines: &[Vec<u8>],
    from_line: usize,
    from_col: usize,
    cursor_lnum: usize,
) -> bool {
    matching_close_brace(lines, from_line, from_col)
        .is_some_and(|(close_line, _)| close_line + 1 < cursor_lnum)
}

fn matching_close_brace(
    lines: &[Vec<u8>],
    from_line: usize,
    from_col: usize,
) -> Option<(usize, usize)> {
    let mut depth = 0i32;
    for (index, line) in lines.iter().enumerate().skip(from_line) {
        let start_col = if index == from_line { from_col } else { 0 };
        for (col, &byte) in line.iter().enumerate().skip(start_col) {
            match byte {
                b'{' => depth += 1,
                b'}' if depth == 0 => return Some((index, col)),
                b'}' => depth -= 1,
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gd_jumps_to_the_first_whole_word() {
        let lines = vec![b"int x;".to_vec(), b"return x;".to_vec()];
        let start = Position { lnum: 2, col: 7 };
        let motion = resolve(&lines, start, "gD", 1, true, (1, 2)).unwrap();
        assert_eq!(motion.target, Position { lnum: 1, col: 4 });
    }

    #[test]
    fn w_from_return_lands_on_the_next_word() {
        let lines = vec![b"      return x;".to_vec()];
        let start = Position { lnum: 1, col: 6 };
        let motion = resolve(&lines, start, "W", 1, true, (1, 1)).unwrap();
        assert_eq!(motion.target, Position { lnum: 1, col: 13 });
    }

    #[test]
    fn gd_skips_a_match_inside_a_block_comment() {
        let lines = vec![
            b"/* int x; */".to_vec(),
            b"int x;".to_vec(),
            b"return x;".to_vec(),
        ];
        let start = Position { lnum: 3, col: 7 };
        let motion = resolve(&lines, start, "gD", 1, true, (1, 3)).unwrap();
        assert_eq!(motion.target, Position { lnum: 2, col: 4 });
    }

    #[test]
    fn gd_prefers_the_function_parameter() {
        let lines = vec![
            b"int x;".to_vec(),
            b"".to_vec(),
            b"int func(int x)".to_vec(),
            b"{".to_vec(),
            b"      return x;".to_vec(),
            b"}".to_vec(),
        ];
        let start = Position { lnum: 5, col: 13 };
        let motion = resolve(&lines, start, "gd", 1, true, (1, 6)).unwrap();
        assert_eq!(motion.target, Position { lnum: 3, col: 13 });
    }

    #[test]
    fn percent_jumps_between_block_comment_markers() {
        let lines = vec![b"/*".to_vec(), b" * body".to_vec(), b" */".to_vec()];
        let open = resolve(&lines, Position { lnum: 1, col: 0 }, "%", 1, true, (1, 3)).unwrap();
        assert_eq!(open.target, Position { lnum: 3, col: 2 });
        let close = resolve(&lines, open.target, "%", 1, true, (1, 3)).unwrap();
        assert_eq!(close.target, Position { lnum: 1, col: 0 });
    }

    #[test]
    fn percent_walks_if_elif_else_endif() {
        let lines = vec![
            b"#if FOO".to_vec(),
            b"#elif BAR".to_vec(),
            b"#else".to_vec(),
            b"#endif".to_vec(),
        ];
        let elif = resolve(&lines, Position { lnum: 1, col: 0 }, "%", 1, true, (1, 4)).unwrap();
        assert_eq!(elif.target.lnum, 2);
        let else_arm = resolve(&lines, elif.target, "%", 1, true, (1, 4)).unwrap();
        assert_eq!(else_arm.target.lnum, 3);
        let endif = resolve(&lines, else_arm.target, "%", 1, true, (1, 4)).unwrap();
        assert_eq!(endif.target.lnum, 4);
        let back = resolve(&lines, endif.target, "%", 1, true, (1, 4)).unwrap();
        assert_eq!(back.target.lnum, 1);
    }

    #[test]
    fn percent_skips_nested_hash_if() {
        let lines = vec![
            b"/* Test pressing % on #if, #else #elsif and #endif,".to_vec(),
            b" * with nested #if".to_vec(),
            b" */".to_vec(),
            b"#if FOO".to_vec(),
            b"/* ... */".to_vec(),
            b"#  if BAR".to_vec(),
            b"/* ... */".to_vec(),
            b"#  endif".to_vec(),
            b"#elif BAR".to_vec(),
            b"/* ... */".to_vec(),
            b"#else".to_vec(),
            b"/* ... */".to_vec(),
            b"#endif".to_vec(),
        ];
        let motion = resolve(&lines, Position { lnum: 4, col: 0 }, "%", 1, true, (1, 13)).unwrap();
        assert_eq!(
            motion.target,
            Position { lnum: 9, col: 0 },
            "nested #if must not steal the #elif"
        );
    }

    #[test]
    fn close_bracket_hash_skips_nested_if() {
        let lines = vec![
            b"/* Test pressing % on #if, #else #elsif and #endif,".to_vec(),
            b" * with nested #if".to_vec(),
            b" */".to_vec(),
            b"#if FOO".to_vec(),
            b"/* ... */".to_vec(),
            b"#  if BAR".to_vec(),
            b"/* ... */".to_vec(),
            b"#  endif".to_vec(),
            b"#elif BAR".to_vec(),
            b"/* ... */".to_vec(),
            b"#else".to_vec(),
            b"/* ... */".to_vec(),
            b"#endif".to_vec(),
        ];
        let motion = resolve(&lines, Position { lnum: 5, col: 0 }, "]#", 1, true, (1, 13)).unwrap();
        assert_eq!(motion.target, Position { lnum: 9, col: 0 });
    }
}
