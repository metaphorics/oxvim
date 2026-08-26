//! Operator and motion algebra, following `ops.c` range normalization.

use ox_text::Position;
use ox_types::{BufHandle, WinHandle};
use thiserror::Error;

use crate::buffer::BufferTextEditRequest;
use crate::extmark::ExtmarkPosition;
use crate::indent::{self, ExprEval, IndentAmount, IndentExprError, Method};
use crate::{BufferStateError, Editor, EditorError, Motion, MotionKind, OptionValue, RegisterContent, RegisterError, RegisterKind};

/// Operation applied to a resolved motion or selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operator {
    /// Delete the range and update delete registers.
    Delete,
    /// Copy the range into yank registers.
    Yank,
    /// Delete the range and enter insert mode.
    Change,
    /// Convert ASCII letters to lowercase.
    Lowercase,
    /// Convert ASCII letters to uppercase.
    Uppercase,
    /// Toggle ASCII letter case.
    ToggleCase,
    /// Shift lines right.
    Indent,
    /// Shift lines left.
    Unindent,
    /// Reindent lines through the unavailable formatting subsystem.
    Format,
}

/// Inclusive editor positions plus the range's operator shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditRange {
    /// Earlier endpoint.
    pub start: Position,
    /// Later endpoint.
    pub end: Position,
    /// Character, line, or block shape.
    pub kind: MotionKind,
    /// Whether `end` belongs to a characterwise range.
    pub inclusive: bool,
}

/// Failures while capturing or applying an operator range.
#[derive(Debug, Error)]
pub enum OperatorError {
    /// Core editor mutation failed.
    #[error(transparent)] Editor(#[from] EditorError),
    /// Buffer text could not be captured.
    #[error(transparent)] Buffer(#[from] BufferStateError),
    /// Register storage rejected the content or name.
    #[error(transparent)] Register(#[from] RegisterError),
    /// A requested cursor line is outside the captured text.
    #[error("cursor line {0} is outside the buffer")] InvalidLine(usize),
    /// The operator needs a subsystem not landed in this editor layer.
    #[error("not implemented: {0}")] NotImplemented(&'static str),
    /// Indent expression evaluation failed.
    #[error(transparent)] Indent(#[from] IndentExprError),
}

/// Cursor and transition produced by an operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperatorResult {
    /// Cursor after the operation.
    pub cursor: Position,
    /// Whether change should transition to insert mode.
    pub enter_insert: bool,
}

impl EditRange {
    /// Normalizes a motion and its origin into ordered endpoints.
    #[must_use]
    pub fn from_motion(origin: Position, motion: Motion) -> Self {
        let (start, end) = if (origin.lnum, origin.col) <= (motion.target.lnum, motion.target.col) { (origin, motion.target) } else { (motion.target, origin) };
        Self { start, end, kind: motion.kind, inclusive: motion.inclusive }
    }
}

enum OperatorEditPlan {
    None,
    DeleteLines {
        start: usize,
        end: usize,
    },
    Batch(Vec<BufferTextEditRequest>),
    Single(BufferTextEditRequest),
}

fn deletion_plan(lines: &[Vec<u8>], range: EditRange, change_linewise: bool) -> OperatorEditPlan {
    match range.kind {
        MotionKind::LineWise
            if change_linewise || (range.start.lnum == 1 && range.end.lnum == lines.len()) =>
        {
            OperatorEditPlan::Single(BufferTextEditRequest {
                start: ExtmarkPosition::new(range.start.lnum - 1, 0),
                end: ExtmarkPosition::new(
                    range.end.lnum - 1,
                    lines[range.end.lnum - 1].len(),
                ),
                replacement: Vec::new(),
            })
        }
        MotionKind::LineWise => OperatorEditPlan::DeleteLines {
            start: range.start.lnum,
            end: range.end.lnum,
        },
        MotionKind::BlockWise => {
            let width = range
                .end
                .col
                .saturating_sub(range.start.col)
                .saturating_add(usize::from(range.inclusive));
            let requests = lines[range.start.lnum - 1..range.end.lnum]
                .iter()
                .enumerate()
                .map(|(row_offset, line)| {
                    let start_col = range.start.col.min(line.len());
                    BufferTextEditRequest {
                        start: ExtmarkPosition::new(
                            range.start.lnum - 1 + row_offset,
                            start_col,
                        ),
                        end: ExtmarkPosition::new(
                            range.start.lnum - 1 + row_offset,
                            start_col.saturating_add(width).min(line.len()),
                        ),
                        replacement: Vec::new(),
                    }
                })
                .collect();
            OperatorEditPlan::Batch(requests)
        }
        MotionKind::CharacterWise => OperatorEditPlan::Single(BufferTextEditRequest {
            start: ExtmarkPosition::new(range.start.lnum - 1, range.start.col),
            end: ExtmarkPosition::new(
                range.end.lnum - 1,
                range
                    .end
                    .col
                    .saturating_add(usize::from(range.inclusive))
                    .min(lines[range.end.lnum - 1].len()),
            ),
            replacement: Vec::new(),
        }),
    }
}

/// Applies an operator through the editor's exact text mutation and undo pipeline.
pub fn apply(editor: &mut Editor, buffer: BufHandle, window: WinHandle, operator: Operator, range: EditRange, register: Option<char>, timestamp: i64, eval: &mut dyn ExprEval) -> Result<OperatorResult, OperatorError> {
    if operator == Operator::Format {
        return apply_reindent(editor, buffer, window, range, timestamp, eval);
    }
    let text = editor.buffer(buffer)?.text()?;
    let old_count = text.line_count();
    let cursor_before = editor.window(window)?.cursor;
    let mut lines = (1..=old_count).map(|lnum| text.line(lnum)).collect::<Result<Vec<_>, _>>().map_err(BufferStateError::from)?;
    let normalized = normalize(&lines, range);
    let shiftwidth = match editor.options().get_buffer(buffer, "shiftwidth") { Ok(OptionValue::Number(width)) if *width > 0 => *width as usize, _ => 2 };
    let plan = match operator {
        Operator::Yank => OperatorEditPlan::None,
        Operator::Delete => deletion_plan(&lines, normalized, false),
        Operator::Change => deletion_plan(&lines, normalized, true),
        Operator::Lowercase | Operator::Uppercase | Operator::ToggleCase => {
            OperatorEditPlan::Batch(mutate_case(&mut lines, normalized, operator))
        }
        Operator::Indent | Operator::Unindent => OperatorEditPlan::Batch(mutate_indent(
            &mut lines,
            normalized,
            operator == Operator::Indent,
            shiftwidth,
        )),
        Operator::Format => unreachable!("Format returns through apply_reindent"),
    };

    // Validate every byte boundary before registers or editor state can change.
    match &plan {
        OperatorEditPlan::Batch(requests) => {
            for request in requests {
                editor.buffer(buffer)?.prepare_buffer_text_edit(request)?;
            }
        }
        OperatorEditPlan::Single(request) => {
            editor.buffer(buffer)?.prepare_buffer_text_edit(request)?;
        }
        OperatorEditPlan::None | OperatorEditPlan::DeleteLines { .. } => {}
    }
    if matches!(operator, Operator::Yank | Operator::Delete | Operator::Change) {
        let content = capture(&lines, normalized)?;
        match operator {
            Operator::Yank => store_yank(editor, register, content)?,
            Operator::Delete | Operator::Change => store_delete(editor, register, content)?,
            _ => {}
        }
    }
    match operator {
        Operator::Delete => mutate_delete(&mut lines, normalized),
        Operator::Change if normalized.kind == MotionKind::LineWise => {
            // `cc` leaves one empty line in place of the changed lines.
            lines[normalized.start.lnum - 1].clear();
            lines.drain(normalized.start.lnum..normalized.end.lnum);
        }
        Operator::Change => mutate_delete(&mut lines, normalized),
        _ => {}
    }

    let cursor = match normalized.kind {
        MotionKind::LineWise => Position { lnum: normalized.start.lnum.min(lines.len().max(1)), col: first_nonblank(lines.get(normalized.start.lnum.saturating_sub(1)).map_or(&[], Vec::as_slice)) },
        _ => Position { lnum: normalized.start.lnum.min(lines.len().max(1)), col: normalized.start.col.min(lines.get(normalized.start.lnum.saturating_sub(1)).map_or(0, Vec::len).saturating_sub(1)) },
    };
    match plan {
        OperatorEditPlan::None => {}
        OperatorEditPlan::DeleteLines { start, end } => {
            editor.replace_buffer_lines(buffer, start, end, &[], cursor_before, cursor, timestamp)?;
        }
        OperatorEditPlan::Batch(requests) => {
            editor.replace_buffer_texts(
                buffer,
                window,
                &requests,
                cursor_before,
                cursor,
                timestamp,
            )?;
        }
        OperatorEditPlan::Single(request) => {
            editor.replace_buffer_text(buffer, &request, cursor_before, cursor, timestamp)?;
        }
    }
    editor.set_window_cursor(window, cursor)?;
    Ok(OperatorResult { cursor, enter_insert: operator == Operator::Change })
}

fn normalize(lines: &[Vec<u8>], mut range: EditRange) -> EditRange {
    range.start.lnum = range.start.lnum.clamp(1, lines.len().max(1));
    range.end.lnum = range.end.lnum.clamp(range.start.lnum, lines.len().max(1));
    // The back-off in ops.c:3517-3539 runs only for an exclusive charwise
    // motion whose endpoint is column zero of a later line. When the origin is
    // on or before the first non-blank of the start line, the operator becomes
    // linewise; otherwise the endpoint becomes the last byte of the start line
    // so `dw`/`db` crossing a line boundary never pull the following line up.
    if range.kind == MotionKind::CharacterWise && !range.inclusive && range.end.lnum > range.start.lnum && range.end.col == 0 {
        let start_line = &lines[range.start.lnum - 1];
        let indent = start_line.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(start_line.len());
        range.end.lnum = range.start.lnum;
        if range.start.col <= indent {
            range.kind = MotionKind::LineWise;
        } else {
            range.end.col = start_line.len();
            range.inclusive = true;
        }
    }
    if range.kind == MotionKind::LineWise { range.start.col = 0; range.end.col = lines[range.end.lnum - 1].len().saturating_sub(1); range.inclusive = true; }
    range.start.col = range.start.col.min(lines[range.start.lnum - 1].len().saturating_sub(1));
    range.end.col = range.end.col.min(lines[range.end.lnum - 1].len().saturating_sub(1));
    range
}

fn capture(lines: &[Vec<u8>], range: EditRange) -> Result<RegisterContent, RegisterError> {
    match range.kind {
        MotionKind::LineWise => RegisterContent::linewise(lines[range.start.lnum - 1..range.end.lnum].to_vec()),
        MotionKind::BlockWise => {
            let width = range.end.col.saturating_sub(range.start.col).saturating_add(usize::from(range.inclusive));
            let rows = lines[range.start.lnum - 1..range.end.lnum].iter().map(|line| line[range.start.col.min(line.len())..range.start.col.saturating_add(width).min(line.len())].to_vec()).collect();
            RegisterContent::new(RegisterKind::BlockWise { width }, rows)
        }
        MotionKind::CharacterWise => {
            let mut rows = Vec::new();
            if range.start.lnum == range.end.lnum {
                let end = range.end.col.saturating_add(usize::from(range.inclusive)).min(lines[range.start.lnum - 1].len());
                rows.push(lines[range.start.lnum - 1][range.start.col.min(end)..end].to_vec());
            } else {
                rows.push(lines[range.start.lnum - 1][range.start.col..].to_vec());
                rows.extend(lines[range.start.lnum..range.end.lnum - 1].iter().cloned());
                let end = range.end.col.saturating_add(usize::from(range.inclusive)).min(lines[range.end.lnum - 1].len());
                rows.push(lines[range.end.lnum - 1][..end].to_vec());
            }
            RegisterContent::new(RegisterKind::CharacterWise, rows)
        }
    }
}

fn store_yank(editor: &mut Editor, register: Option<char>, content: RegisterContent) -> Result<(), RegisterError> { if let Some(name) = register { editor.registers_mut().yank_to(name, content) } else { editor.registers_mut().yank(content); Ok(()) } }
fn store_delete(editor: &mut Editor, register: Option<char>, content: RegisterContent) -> Result<(), RegisterError> { if let Some(name) = register { editor.registers_mut().delete_to(name, content) } else { editor.registers_mut().delete(content); Ok(()) } }

fn mutate_delete(lines: &mut Vec<Vec<u8>>, range: EditRange) {
    match range.kind {
        MotionKind::LineWise => { lines.drain(range.start.lnum - 1..range.end.lnum); if lines.is_empty() { lines.push(Vec::new()); } }
        MotionKind::BlockWise => {
            let width = range.end.col.saturating_sub(range.start.col).saturating_add(usize::from(range.inclusive));
            for line in &mut lines[range.start.lnum - 1..range.end.lnum] { let start = range.start.col.min(line.len()); let end = start.saturating_add(width).min(line.len()); line.drain(start..end); }
        }
        MotionKind::CharacterWise if range.start.lnum == range.end.lnum => {
            let line = &mut lines[range.start.lnum - 1]; let end = range.end.col.saturating_add(usize::from(range.inclusive)).min(line.len()); line.drain(range.start.col.min(end)..end);
        }
        MotionKind::CharacterWise => {
            let end = range.end.col.saturating_add(usize::from(range.inclusive)).min(lines[range.end.lnum - 1].len());
            let suffix = lines[range.end.lnum - 1][end..].to_vec();
            lines[range.start.lnum - 1].truncate(range.start.col);
            lines[range.start.lnum - 1].extend(suffix);
            lines.drain(range.start.lnum..range.end.lnum);
        }
    }
}

fn case_span(line: &[u8], range: EditRange, lnum: usize) -> (usize, usize) {
    if range.kind == MotionKind::BlockWise {
        (
            range.start.col.min(line.len()),
            range.end.col.saturating_add(usize::from(range.inclusive)).min(line.len()),
        )
    } else {
        let start = if lnum == range.start.lnum { range.start.col } else { 0 };
        let end = if lnum == range.end.lnum {
            range.end.col.saturating_add(usize::from(range.inclusive)).min(line.len())
        } else {
            line.len()
        };
        (start.min(line.len()), end)
    }
}

fn mutate_case(lines: &mut [Vec<u8>], range: EditRange, operator: Operator) -> Vec<BufferTextEditRequest> {
    let mut requests = Vec::new();
    for lnum in range.start.lnum..=range.end.lnum {
        let line = &mut lines[lnum - 1];
        let (start, end) = case_span(line, range, lnum);
        if start >= end {
            continue;
        }
        for byte in &mut line[start..end] {
            *byte = match operator {
                Operator::Lowercase => byte.to_ascii_lowercase(),
                Operator::Uppercase => byte.to_ascii_uppercase(),
                Operator::ToggleCase if byte.is_ascii_lowercase() => byte.to_ascii_uppercase(),
                Operator::ToggleCase => byte.to_ascii_lowercase(),
                _ => *byte,
            };
        }
        requests.push(BufferTextEditRequest {
            start: ExtmarkPosition::new(lnum - 1, start),
            end: ExtmarkPosition::new(lnum - 1, end),
            replacement: vec![line[start..end].to_vec()],
        });
    }
    requests
}

fn mutate_indent(lines: &mut [Vec<u8>], range: EditRange, add: bool, width: usize) -> Vec<BufferTextEditRequest> {
    let mut requests = Vec::new();
    for (row_offset, line) in lines[range.start.lnum - 1..range.end.lnum].iter_mut().enumerate() {
        let col = if range.kind == MotionKind::BlockWise { range.start.col.min(line.len()) } else { 0 };
        if add {
            if width == 0 {
                continue;
            }
            line.splice(col..col, std::iter::repeat_n(b' ', width));
            requests.push(BufferTextEditRequest {
                start: ExtmarkPosition::new(range.start.lnum - 1 + row_offset, col),
                end: ExtmarkPosition::new(range.start.lnum - 1 + row_offset, col),
                replacement: vec![vec![b' '; width]],
            });
        } else {
            let remove = line[col..].iter().take(width).take_while(|b| b.is_ascii_whitespace()).count();
            if remove == 0 {
                continue;
            }
            line.drain(col..col + remove);
            requests.push(BufferTextEditRequest {
                start: ExtmarkPosition::new(range.start.lnum - 1 + row_offset, col),
                end: ExtmarkPosition::new(range.start.lnum - 1 + row_offset, col + remove),
                replacement: Vec::new(),
            });
        }
    }
    requests
}

fn first_nonblank(line: &[u8]) -> usize { line.iter().position(|b| !b.is_ascii_whitespace()).map_or(0, |col| col) }

fn apply_reindent(
    editor: &mut Editor,
    buffer: BufHandle,
    window: WinHandle,
    range: EditRange,
    timestamp: i64,
    eval: &mut dyn ExprEval,
) -> Result<OperatorResult, OperatorError> {
    let opts = indent::IndentOptions::capture(editor, buffer);
    let method = indent::resolve_options_method(&opts);
    let text = editor.buffer(buffer)?.text()?;
    let old_count = text.line_count();
    let cursor_before = editor.window(window)?.cursor;
    let mut lines = (1..=old_count)
        .map(|lnum| text.line(lnum))
        .collect::<Result<Vec<_>, _>>()
        .map_err(BufferStateError::from)?;
    let normalized = normalize(&lines, range);
    let start = normalized.start.lnum;
    let end = normalized.end.lnum;
    // Plan phase: evaluate the whole range against the staged snapshot.
    // Nothing here can mutate the editor (eval sees the read-only overlay),
    // so any evaluation error returns with bytes/cursor/undo/ticks untouched.
    let mut requests = Vec::new();
    for lnum in start..=end {
        if method == Method::Lisp && lnum == start && end > start {
            continue;
        }
        let old_lead = indent::leading_len(&lines[lnum - 1]);
        let rest_empty = lines[lnum - 1][old_lead..].iter().all(u8::is_ascii_whitespace);
        let amount = if rest_empty {
            IndentAmount::Columns(0)
        } else {
            let context = indent::IndentEvalContext::new(editor, buffer, &lines);
            indent::amount_for(&context, lnum, method, &opts, eval)?
        };
        let IndentAmount::Columns(target) = amount else { continue };
        let whitespace = indent::whitespace_for(target, &lines[lnum - 1], &opts);
        if whitespace.as_slice() == &lines[lnum - 1][..old_lead] {
            continue;
        }
        lines[lnum - 1].splice(..old_lead, whitespace.iter().copied());
        requests.push(BufferTextEditRequest {
            start: ExtmarkPosition::new(lnum - 1, 0),
            end: ExtmarkPosition::new(lnum - 1, old_lead),
            replacement: vec![whitespace],
        });
    }
    let cursor = Position {
        lnum: start.min(lines.len().max(1)),
        col: first_nonblank(lines.get(start.saturating_sub(1)).map_or(&[], Vec::as_slice)),
    };
    editor.replace_buffer_texts(buffer, window, &requests, cursor_before, cursor, timestamp)?;
    Ok(OperatorResult { cursor, enter_insert: false })
}

