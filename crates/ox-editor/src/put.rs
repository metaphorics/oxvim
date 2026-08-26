use ox_text::Position;

use crate::buffer::BufferTextEditRequest;
use crate::extmark::ExtmarkPosition;
use crate::motion;
use crate::register::{RegisterContent, RegisterError, RegisterKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PutDirection {
    Before,
    After,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PutEdit {
    Splice(BufferTextEditRequest),
    InsertLines {
        after_lnum: usize,
        lines: Vec<Vec<u8>>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PutPlan {
    pub(crate) edits: Vec<PutEdit>,
    pub(crate) cursor_before: Position,
    pub(crate) cursor_after: Position,
}

#[must_use]
pub(crate) fn put_origin(
    lines: &[Vec<u8>],
    cursor: Position,
    kind: RegisterKind,
    direction: PutDirection,
) -> Position {
    if matches!(kind, RegisterKind::LineWise) {
        return Position {
            lnum: match direction {
                PutDirection::Before => cursor.lnum.saturating_sub(1),
                PutDirection::After => cursor.lnum,
            },
            col: 0,
        };
    }

    let lnum = cursor.lnum.clamp(1, lines.len().max(1));
    let line = lines.get(lnum - 1).map_or(&[][..], Vec::as_slice);
    let col = match direction {
        PutDirection::Before => cursor.col,
        PutDirection::After => motion::next_char_boundary(line, cursor.col).min(line.len()),
    };
    Position { lnum, col }
}

pub(crate) fn plan_put(
    lines: &[Vec<u8>],
    origin: Position,
    content: &RegisterContent,
    count: usize,
    cursor_before: Position,
) -> Result<PutPlan, RegisterError> {
    let count = count.max(1);
    match content.kind() {
        RegisterKind::CharacterWise => {
            plan_characterwise(origin, content, count, cursor_before)
        }
        RegisterKind::LineWise => plan_linewise(origin, content, count, cursor_before),
        RegisterKind::BlockWise { width } => {
            plan_blockwise(lines, origin, content, count, cursor_before, width)
        }
    }
}

fn plan_characterwise(
    origin: Position,
    content: &RegisterContent,
    count: usize,
    cursor_before: Position,
) -> Result<PutPlan, RegisterError> {
    if content.lines().len() == 1 {
        let line = &content.lines()[0];
        let byte_len = line
            .len()
            .checked_mul(count)
            .ok_or(RegisterError::PositionOverflow)?;
        if byte_len == 0 {
            return Ok(PutPlan {
                edits: Vec::new(),
                cursor_before,
                cursor_after: cursor_before,
            });
        }

        let mut payload = Vec::with_capacity(byte_len);
        for _ in 0..count {
            payload.extend_from_slice(line);
        }
        let col = origin
            .col
            .checked_add(byte_len - last_scalar_len(&payload))
            .ok_or(RegisterError::PositionOverflow)?;
        return Ok(PutPlan {
            edits: vec![splice(origin, vec![payload])],
            cursor_before,
            cursor_after: Position {
                lnum: origin.lnum,
                col,
            },
        });
    }

    let bytes = content.to_bytes();
    let byte_len = bytes
        .len()
        .checked_mul(count)
        .ok_or(RegisterError::PositionOverflow)?;
    let mut stream = Vec::with_capacity(byte_len);
    for _ in 0..count {
        stream.extend_from_slice(&bytes);
    }
    let replacement = stream
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    Ok(PutPlan {
        edits: vec![splice(origin, replacement)],
        cursor_before,
        cursor_after: origin,
    })
}

fn plan_linewise(
    origin: Position,
    content: &RegisterContent,
    count: usize,
    cursor_before: Position,
) -> Result<PutPlan, RegisterError> {
    let row_count = content
        .lines()
        .len()
        .checked_mul(count)
        .ok_or(RegisterError::PositionOverflow)?;
    let mut repeated = Vec::with_capacity(row_count);
    for _ in 0..count {
        repeated.extend(content.lines().iter().cloned());
    }
    let lnum = origin
        .lnum
        .checked_add(1)
        .ok_or(RegisterError::PositionOverflow)?;
    let cursor_after = Position {
        lnum,
        col: first_nonblank(&repeated[0]),
    };
    Ok(PutPlan {
        edits: vec![PutEdit::InsertLines {
            after_lnum: origin.lnum,
            lines: repeated,
        }],
        cursor_before,
        cursor_after,
    })
}

fn plan_blockwise(
    lines: &[Vec<u8>],
    origin: Position,
    content: &RegisterContent,
    count: usize,
    cursor_before: Position,
    width: usize,
) -> Result<PutPlan, RegisterError> {
    let first_target = origin
        .lnum
        .checked_sub(1)
        .ok_or(RegisterError::PositionOverflow)?;
    let mut edits = Vec::with_capacity(content.lines().len().saturating_add(1));
    let mut tail = Vec::new();

    for (row_index, row) in content.lines().iter().enumerate() {
        let target = first_target
            .checked_add(row_index)
            .ok_or(RegisterError::PositionOverflow)?;
        let padding = width.saturating_sub(row.len());
        let unit_len = row
            .len()
            .checked_add(padding)
            .ok_or(RegisterError::PositionOverflow)?;

        if let Some(target_line) = lines.get(target) {
            let shortline = origin.col >= target_line.len();
            let inserted = if shortline {
                build_short_row(origin.col - target_line.len(), row, padding, count)?
            } else {
                repeat_unit(row, padding, count, unit_len)?
            };
            if !inserted.is_empty() {
                let col = if shortline {
                    target_line.len()
                } else {
                    origin.col
                };
                edits.push(PutEdit::Splice(BufferTextEditRequest {
                    start: ExtmarkPosition::new(target, col),
                    end: ExtmarkPosition::new(target, col),
                    replacement: vec![inserted],
                }));
            }
        } else {
            tail.push(build_short_row(origin.col, row, padding, count)?);
        }
    }

    if !tail.is_empty() {
        edits.push(PutEdit::InsertLines {
            after_lnum: lines.len(),
            lines: tail,
        });
    }
    Ok(PutPlan {
        edits,
        cursor_before,
        cursor_after: origin,
    })
}

fn build_short_row(
    leading_spaces: usize,
    row: &[u8],
    padding: usize,
    count: usize,
) -> Result<Vec<u8>, RegisterError> {
    let unit_len = row
        .len()
        .checked_add(padding)
        .ok_or(RegisterError::PositionOverflow)?;
    let repeated_len = unit_len
        .checked_mul(count - 1)
        .ok_or(RegisterError::PositionOverflow)?;
    let len = leading_spaces
        .checked_add(repeated_len)
        .and_then(|len| len.checked_add(row.len()))
        .ok_or(RegisterError::PositionOverflow)?;
    let mut inserted = Vec::with_capacity(len);
    inserted.resize(leading_spaces, b' ');
    for _ in 1..count {
        inserted.extend_from_slice(row);
        inserted.resize(inserted.len() + padding, b' ');
    }
    inserted.extend_from_slice(row);
    Ok(inserted)
}

fn repeat_unit(
    row: &[u8],
    padding: usize,
    count: usize,
    unit_len: usize,
) -> Result<Vec<u8>, RegisterError> {
    let len = unit_len
        .checked_mul(count)
        .ok_or(RegisterError::PositionOverflow)?;
    let mut inserted = Vec::with_capacity(len);
    for _ in 0..count {
        inserted.extend_from_slice(row);
        inserted.resize(inserted.len() + padding, b' ');
    }
    Ok(inserted)
}

fn splice(origin: Position, replacement: Vec<Vec<u8>>) -> PutEdit {
    let position = ExtmarkPosition::new(origin.lnum - 1, origin.col);
    PutEdit::Splice(BufferTextEditRequest {
        start: position,
        end: position,
        replacement,
    })
}

fn first_nonblank(line: &[u8]) -> usize {
    line.iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(0)
}

fn last_scalar_len(bytes: &[u8]) -> usize {
    let mut len = usize::from(!bytes.is_empty());
    while len < bytes.len() && bytes[bytes.len() - len] & 0b1100_0000 == 0b1000_0000 {
        len += 1;
    }
    len.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(lnum: usize, col: usize) -> Position {
        Position { lnum, col }
    }

    fn splice_replacement(plan: &PutPlan) -> &[Vec<u8>] {
        let PutEdit::Splice(request) = &plan.edits[0] else {
            panic!("expected splice");
        };
        &request.replacement
    }

    #[test]
    fn one_line_count_expands_and_cursor_uses_last_scalar_start() {
        let content = RegisterContent::characterwise("한X".as_bytes()).unwrap();
        let plan = plan_put(&[b"ab".to_vec()], position(1, 1), &content, 2, position(1, 0))
            .unwrap();

        assert_eq!(splice_replacement(&plan), &["한X한X".as_bytes().to_vec()]);
        assert_eq!(plan.cursor_after, position(1, 8));
        assert_eq!(last_scalar_len("한".as_bytes()), 3);
        assert_eq!(last_scalar_len(b"X"), 1);
    }

    #[test]
    fn multiline_count_joins_copy_boundaries() {
        let content = RegisterContent::characterwise(b"x\ny").unwrap();
        let plan = plan_put(&[b"ab".to_vec()], position(1, 1), &content, 2, position(1, 0))
            .unwrap();

        assert_eq!(
            splice_replacement(&plan),
            &[b"x".to_vec(), b"yx".to_vec(), b"y".to_vec()]
        );
        assert_eq!(plan.cursor_after, position(1, 1));
    }

    #[test]
    fn linewise_count_repeats_vertically_and_finds_first_nonblank() {
        let content = RegisterContent::linewise(vec![b"  x".to_vec(), b"y".to_vec()]).unwrap();
        let plan = plan_put(&[b"one".to_vec()], position(0, 0), &content, 2, position(1, 0))
            .unwrap();

        assert_eq!(
            plan.edits,
            vec![PutEdit::InsertLines {
                after_lnum: 0,
                lines: vec![b"  x".to_vec(), b"y".to_vec(), b"  x".to_vec(), b"y".to_vec()],
            }]
        );
        assert_eq!(plan.cursor_after, position(1, 2));
        assert_eq!(first_nonblank(b" \t "), 0);
    }

    #[test]
    fn blockwise_distinguishes_short_rows_from_following_text() {
        let content = RegisterContent::blockwise(vec![b"Q".to_vec(), b"R".to_vec()], 2).unwrap();
        let plan = plan_put(
            &[b"abcdef".to_vec(), b"a".to_vec()],
            position(1, 3),
            &content,
            2,
            position(1, 2),
        )
        .unwrap();

        assert_eq!(splice_replacement(&plan), &[b"Q Q ".to_vec()]);
        let PutEdit::Splice(short) = &plan.edits[1] else {
            panic!("expected short-row splice");
        };
        assert_eq!(short.start, ExtmarkPosition::new(1, 1));
        assert_eq!(short.replacement, vec![b"  R R".to_vec()]);
    }

    #[test]
    fn blockwise_materializes_uniform_eof_tail_rows() {
        let content = RegisterContent::blockwise(
            vec![b"Q".to_vec(), b"R".to_vec(), b"S".to_vec()],
            1,
        )
        .unwrap();
        let plan = plan_put(&[b"abc".to_vec()], position(1, 2), &content, 1, position(1, 1))
            .unwrap();

        assert_eq!(splice_replacement(&plan), &[b"Q".to_vec()]);
        assert_eq!(
            plan.edits[1],
            PutEdit::InsertLines {
                after_lnum: 1,
                lines: vec![b"  R".to_vec(), b"  S".to_vec()],
            }
        );
    }

    #[test]
    fn count_overflow_is_reported_before_expansion() {
        let one_line = RegisterContent::characterwise(b"xx").unwrap();
        assert_eq!(
            plan_put(&[], position(1, 0), &one_line, usize::MAX, position(1, 0)),
            Err(RegisterError::PositionOverflow)
        );

        let linewise = RegisterContent::linewise(vec![b"x".to_vec(), b"y".to_vec()]).unwrap();
        assert_eq!(
            plan_put(&[], position(0, 0), &linewise, usize::MAX, position(1, 0)),
            Err(RegisterError::PositionOverflow)
        );
    }

    #[test]
    fn empty_characterwise_content_produces_empty_plan() {
        let content = RegisterContent::characterwise(b"").unwrap();
        let before = position(1, 0);
        let plan = plan_put(&[Vec::new()], before, &content, 3, before).unwrap();

        assert!(plan.edits.is_empty());
        assert_eq!(plan.cursor_after, before);
    }
}
