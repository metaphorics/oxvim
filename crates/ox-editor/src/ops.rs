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

/// Applies an operator through the editor's line mutation and undo pipeline.
pub fn apply(editor: &mut Editor, buffer: BufHandle, window: WinHandle, operator: Operator, range: EditRange, register: Option<char>, timestamp: i64, eval: &mut dyn ExprEval) -> Result<OperatorResult, OperatorError> {
    if operator == Operator::Format {
        return apply_reindent(editor, buffer, window, range, timestamp, eval);
    }
    let text = editor.buffer(buffer)?.text()?;
    let old_count = text.line_count();
    let cursor_before = editor.window(window)?.cursor;
    let mut lines = (1..=old_count).map(|lnum| text.line(lnum)).collect::<Result<Vec<_>, _>>().map_err(BufferStateError::from)?;
    let mut normalized = normalize(&lines, range);
    if operator == Operator::Delete && normalized.kind == MotionKind::CharacterWise && normalized.start.lnum < normalized.end.lnum {
        let starts_in_indent = lines[normalized.start.lnum - 1][..normalized.start.col].iter().all(u8::is_ascii_whitespace);
        let suffix = &lines[normalized.end.lnum - 1][normalized.end.col.saturating_add(usize::from(normalized.inclusive)).min(lines[normalized.end.lnum - 1].len())..];
        if starts_in_indent && suffix.iter().all(u8::is_ascii_whitespace) { normalized.kind = MotionKind::LineWise; normalized.start.col = 0; normalized.end.col = lines[normalized.end.lnum - 1].len().saturating_sub(1); normalized.inclusive = true; }
    }
    let shiftwidth = match editor.options().get_buffer(buffer, "shiftwidth") { Ok(OptionValue::Number(width)) if *width > 0 => *width as usize, _ => 2 };
    enum DeleteEditPlan {
        LineWise,
        BlockWise(Vec<BufferTextEditRequest>),
        CharacterWise(BufferTextEditRequest),
    }
    let delete_plan = (operator == Operator::Delete).then(|| match normalized.kind {
        MotionKind::LineWise if normalized.start.lnum == 1 && normalized.end.lnum == old_count => {
            DeleteEditPlan::CharacterWise(BufferTextEditRequest {
                start: ExtmarkPosition::new(0, 0),
                end: ExtmarkPosition::new(old_count - 1, lines[old_count - 1].len()),
                replacement: Vec::new(),
            })
        }
        MotionKind::LineWise => DeleteEditPlan::LineWise,
        MotionKind::BlockWise => {
            let width = normalized
                .end
                .col
                .saturating_sub(normalized.start.col)
                .saturating_add(usize::from(normalized.inclusive));
            let requests = lines[normalized.start.lnum - 1..normalized.end.lnum]
                .iter()
                .enumerate()
                .map(|(row_offset, line)| {
                    let start_col = normalized.start.col.min(line.len());
                    BufferTextEditRequest {
                        start: ExtmarkPosition::new(
                            normalized.start.lnum - 1 + row_offset,
                            start_col,
                        ),
                        end: ExtmarkPosition::new(
                            normalized.start.lnum - 1 + row_offset,
                            start_col.saturating_add(width).min(line.len()),
                        ),
                        replacement: Vec::new(),
                    }
                })
                .collect();
            DeleteEditPlan::BlockWise(requests)
        }
        MotionKind::CharacterWise => DeleteEditPlan::CharacterWise(BufferTextEditRequest {
            start: ExtmarkPosition::new(normalized.start.lnum - 1, normalized.start.col),
            end: ExtmarkPosition::new(
                normalized.end.lnum - 1,
                normalized
                    .end
                    .col
                    .saturating_add(usize::from(normalized.inclusive))
                    .min(lines[normalized.end.lnum - 1].len()),
            ),
            replacement: Vec::new(),
        }),
    });
    if let Some(plan) = &delete_plan {
        match plan {
            DeleteEditPlan::LineWise => {}
            DeleteEditPlan::BlockWise(requests) => {
                for request in requests {
                    editor.buffer(buffer)?.prepare_buffer_text_edit(request)?;
                }
            }
            DeleteEditPlan::CharacterWise(request) => {
                editor.buffer(buffer)?.prepare_buffer_text_edit(request)?;
            }
        }
    }
    let content = capture(&lines, normalized)?;
    match operator {
        Operator::Yank => store_yank(editor, register, content)?,
        Operator::Delete => {
            store_delete(editor, register, content)?;
            mutate_delete(&mut lines, normalized);
        }
        Operator::Change => {
            // A linewise change clears the first line and drops the rest
            // (`ops.c:888-901`: OP_CHANGE deletes the lines except the first,
            // then truncates it), so `cc` leaves an empty line behind.
            store_delete(editor, register, content)?;
            if normalized.kind == MotionKind::LineWise {
                lines[normalized.start.lnum - 1].clear();
                lines.drain(normalized.start.lnum..normalized.end.lnum);
            } else {
                mutate_delete(&mut lines, normalized);
            }
        }
        Operator::Lowercase | Operator::Uppercase | Operator::ToggleCase => mutate_case(&mut lines, normalized, operator),
        Operator::Indent | Operator::Unindent => mutate_indent(&mut lines, normalized, operator == Operator::Indent, shiftwidth),
        Operator::Format => unreachable!("Format returns through apply_reindent"),
    }
    let cursor = match normalized.kind {
        MotionKind::LineWise => Position { lnum: normalized.start.lnum.min(lines.len().max(1)), col: first_nonblank(lines.get(normalized.start.lnum.saturating_sub(1)).map_or(&[], Vec::as_slice)) },
        _ => Position { lnum: normalized.start.lnum.min(lines.len().max(1)), col: normalized.start.col.min(lines.get(normalized.start.lnum.saturating_sub(1)).map_or(0, Vec::len).saturating_sub(1)) },
    };
    if let Some(plan) = delete_plan {
        match plan {
            DeleteEditPlan::LineWise => {
                editor.replace_buffer_lines(
                    buffer,
                    normalized.start.lnum,
                    normalized.end.lnum,
                    &[],
                    cursor_before,
                    cursor,
                    timestamp,
                )?;
            }
            DeleteEditPlan::BlockWise(requests) => {
                editor.replace_buffer_texts(
                    buffer,
                    window,
                    &requests,
                    cursor_before,
                    cursor,
                    timestamp,
                )?;
            }
            DeleteEditPlan::CharacterWise(request) => {
                editor.replace_buffer_text(
                    buffer,
                    &request,
                    cursor_before,
                    cursor,
                    timestamp,
                )?;
            }
        }
    } else if operator != Operator::Yank {
        editor.replace_buffer_lines(buffer, 1, old_count, &lines, cursor_before, cursor, timestamp)?;
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

fn mutate_case(lines: &mut [Vec<u8>], range: EditRange, operator: Operator) {
    for lnum in range.start.lnum..=range.end.lnum {
        let line = &mut lines[lnum - 1];
        let (start, end) = if range.kind == MotionKind::BlockWise {
            (range.start.col.min(line.len()), range.end.col.saturating_add(usize::from(range.inclusive)).min(line.len()))
        } else {
            let start = if lnum == range.start.lnum { range.start.col } else { 0 };
            let end = if lnum == range.end.lnum { range.end.col.saturating_add(usize::from(range.inclusive)).min(line.len()) } else { line.len() };
            (start.min(line.len()), end)
        };
        for byte in &mut line[start.min(end)..end] { *byte = match operator { Operator::Lowercase => byte.to_ascii_lowercase(), Operator::Uppercase => byte.to_ascii_uppercase(), Operator::ToggleCase if byte.is_ascii_lowercase() => byte.to_ascii_uppercase(), Operator::ToggleCase => byte.to_ascii_lowercase(), _ => *byte }; }
    }
}

fn mutate_indent(lines: &mut [Vec<u8>], range: EditRange, add: bool, width: usize) {
    for line in &mut lines[range.start.lnum - 1..range.end.lnum] {
        let col = if range.kind == MotionKind::BlockWise { range.start.col.min(line.len()) } else { 0 };
        if add { line.splice(col..col, std::iter::repeat_n(b' ', width)); }
        else { let remove = line[col..].iter().take(width).take_while(|b| b.is_ascii_whitespace()).count(); line.drain(col..col + remove); }
    }
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

