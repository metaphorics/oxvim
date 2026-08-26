//! Core insert-mode editing (`edit.c`/`insert.c`).

use ox_text::Position;
use ox_types::{BufHandle, WinHandle};
use thiserror::Error;

use crate::buffer::BufferTextEditRequest;
use crate::extmark::ExtmarkPosition;
use crate::indent::{self, CinTrigger, ExprEval, IndentExprError};
use crate::{BufferStateError, Editor, EditorError};

/// Insert-mode editing failures, combining core editor errors and inherited
/// indent-expression evaluation failures so either can be threaded through
/// [`crate::ModeError`] without being swallowed.
#[derive(Debug, Error)]
pub enum InsertError {
    /// Core editor operation failed.
    #[error(transparent)]
    Editor(#[from] EditorError),
    /// Indent expression evaluation failed.
    #[error(transparent)]
    Indent(#[from] IndentExprError),
}

fn line(editor: &Editor, buffer: BufHandle, lnum: usize) -> Result<Vec<u8>, EditorError> {
    editor.buffer(buffer)?.text()?.line(lnum).map_err(BufferStateError::from).map_err(EditorError::from)
}

/// Inserts one Unicode scalar at the insertion cursor.
pub fn insert_char(editor: &mut Editor, buffer: BufHandle, window: WinHandle, cursor: Position, ch: char, timestamp: i64) -> Result<Position, EditorError> {
    let mut line = line(editor, buffer, cursor.lnum)?;
    let col = cursor.col.min(line.len());
    let mut encoded = [0; 4]; let bytes = ch.encode_utf8(&mut encoded).as_bytes();
    line.splice(col..col, bytes.iter().copied());
    let after = Position { lnum: cursor.lnum, col: col + bytes.len() };
    editor.replace_buffer_lines(buffer, cursor.lnum, cursor.lnum, &[line], cursor, after, timestamp)?;
    editor.set_window_cursor(window, after)?;
    Ok(after)
}

/// Splits the current line at the insertion cursor, indenting the new line
/// when an indent method is active (`edit.c` newline + `fix_indent`).
pub fn newline(editor: &mut Editor, buffer: BufHandle, window: WinHandle, cursor: Position, timestamp: i64, eval: &mut dyn ExprEval) -> Result<Position, InsertError> {
    let line = line(editor, buffer, cursor.lnum)?;
    let col = cursor.col.min(line.len());
    let opts = indent::IndentOptions::capture(editor, buffer);
    let source_prefix = &line[..col];
    let smart = indent::smart_source_trigger(source_prefix, false, &opts);
    let mut indent = indent::smart_newline_indent(source_prefix, smart, &opts);
    let text = editor.buffer(buffer)?.text().map_err(EditorError::from)?;
    let mut lines = (1..=text.line_count())
        .map(|lnum| text.line(lnum))
        .collect::<Result<Vec<_>, _>>()
        .map_err(BufferStateError::from)
        .map_err(EditorError::from)?;
    let suffix = line[col..].to_vec();
    let mut second = indent.clone();
    second.extend_from_slice(&suffix);
    lines[cursor.lnum - 1] = line[..col].to_vec();
    lines.insert(cursor.lnum, second);
    {
        let context = indent::IndentEvalContext::new(editor, buffer, &lines);
        if let Some(whitespace) = indent::fix_line_indent(&context, cursor.lnum + 1, CinTrigger::OpenForward, &opts, eval)? {
            indent = whitespace;
        }
    }
    let request = BufferTextEditRequest {
        start: ExtmarkPosition::new(cursor.lnum - 1, col),
        end: ExtmarkPosition::new(cursor.lnum - 1, col),
        replacement: vec![Vec::new(), indent.clone()],
    };
    let after = Position { lnum: cursor.lnum + 1, col: indent.len() };
    editor.replace_buffer_text(buffer, &request, cursor, after, timestamp)?;
    editor.set_window_cursor(window, after)?;
    Ok(after)
}

fn option_number(editor: &Editor, buffer: BufHandle, name: &str, fallback: i64) -> i64 {
    match editor.options().get_buffer(buffer, name) {
        Ok(crate::OptionValue::Number(value)) => *value,
        _ => fallback,
    }
}

fn pad_width(col: usize, width: usize) -> usize {
    let width = width.max(1);
    width - col % width
}

fn commit_line_splice(
    editor: &mut Editor,
    buffer: BufHandle,
    window: WinHandle,
    cursor: Position,
    after: Position,
    timestamp: i64,
    start_col: usize,
    end_col: usize,
    inserted: Vec<u8>,
) -> Result<Position, EditorError> {
    let request = BufferTextEditRequest {
        start: ExtmarkPosition::new(cursor.lnum - 1, start_col),
        end: ExtmarkPosition::new(cursor.lnum - 1, end_col),
        replacement: vec![inserted],
    };
    editor.replace_buffer_text(buffer, &request, cursor, after, timestamp)?;
    editor.set_window_cursor(window, after)?;
    Ok(after)
}

/// Inserts Tab as spaces or a tab byte using `'expandtab'`/`'softtabstop'`/`'shiftwidth'`.
pub fn insert_tab(
    editor: &mut Editor,
    buffer: BufHandle,
    window: WinHandle,
    cursor: Position,
    timestamp: i64,
) -> Result<Position, EditorError> {
    let line = line(editor, buffer, cursor.lnum)?;
    let col = cursor.col.min(line.len());
    let opts = indent::IndentOptions::capture(editor, buffer);
    let sts = option_number(editor, buffer, "softtabstop", 0);
    let in_indent = col <= indent::leading_len(&line);
    let inserted = if opts.expandtab {
        vec![b' '; pad_width(col, opts.shiftwidth)]
    } else if sts > 0 {
        vec![b' '; pad_width(col, usize::try_from(sts).unwrap_or(1).max(1))]
    } else if in_indent {
        vec![b' '; pad_width(col, opts.shiftwidth)]
    } else {
        vec![b'\t']
    };
    let after = Position {
        lnum: cursor.lnum,
        col: col + inserted.len(),
    };
    commit_line_splice(editor, buffer, window, cursor, after, timestamp, col, col, inserted)
}

fn shifted_indent_columns(current: usize, shiftwidth: usize, add: bool) -> usize {
    let shiftwidth = shiftwidth.max(1);
    let stops = current / shiftwidth;
    let extra = current % shiftwidth;
    if add {
        stops.saturating_add(1).saturating_mul(shiftwidth)
    } else if extra == 0 {
        stops.saturating_sub(1).saturating_mul(shiftwidth)
    } else {
        stops.saturating_mul(shiftwidth)
    }
}

/// Adds or removes one `'shiftwidth'` of leading indentation (`ins_shift`).
pub fn adjust_indent(
    editor: &mut Editor,
    buffer: BufHandle,
    window: WinHandle,
    cursor: Position,
    add: bool,
    timestamp: i64,
) -> Result<Position, EditorError> {
    let line = line(editor, buffer, cursor.lnum)?;
    let opts = indent::IndentOptions::capture(editor, buffer);
    let old_lead = indent::leading_len(&line);
    let current_cols = indent::indent_columns(&line, &opts);
    let target_cols = shifted_indent_columns(current_cols, opts.shiftwidth, add);
    let whitespace = indent::whitespace_for(target_cols, &line, &opts);
    if whitespace.as_slice() == &line[..old_lead] {
        return Ok(cursor);
    }
    let new_lead = whitespace.len();
    let after = Position {
        lnum: cursor.lnum,
        col: cursor.col.saturating_sub(old_lead).saturating_add(new_lead),
    };
    commit_line_splice(
        editor,
        buffer,
        window,
        cursor,
        after,
        timestamp,
        0,
        old_lead,
        whitespace,
    )
}

/// Reindents the current line through the indent engine (`!^F` / `fixthisline`).
pub fn force_reindent(
    editor: &mut Editor,
    buffer: BufHandle,
    window: WinHandle,
    cursor: Position,
    timestamp: i64,
    eval: &mut dyn ExprEval,
) -> Result<Position, InsertError> {
    let line = line(editor, buffer, cursor.lnum)?;
    let opts = indent::IndentOptions::capture(editor, buffer);
    if opts.indentexpr.is_empty() && !opts.lisp && !opts.cindent {
        return Ok(cursor);
    }
    let text = editor.buffer(buffer)?.text().map_err(EditorError::from)?;
    let lines = (1..=text.line_count())
        .map(|lnum| text.line(lnum))
        .collect::<Result<Vec<_>, _>>()
        .map_err(BufferStateError::from)
        .map_err(EditorError::from)?;
    let old_lead = indent::leading_len(&line);
    let method = indent::resolve_options_method(&opts);
    let context = indent::IndentEvalContext::new(editor, buffer, &lines);
    let indent::IndentAmount::Columns(target) = indent::amount_for(&context, cursor.lnum, method, &opts, eval)?
    else {
        return Ok(cursor);
    };
    let whitespace = indent::whitespace_for(target, &line, &opts);
    if whitespace.as_slice() == &line[..old_lead] {
        return Ok(cursor);
    }
    let new_lead = whitespace.len();
    let after = Position {
        lnum: cursor.lnum,
        col: cursor.col.saturating_sub(old_lead).saturating_add(new_lead),
    };
    Ok(commit_line_splice(
        editor,
        buffer,
        window,
        cursor,
        after,
        timestamp,
        0,
        old_lead,
        whitespace,
    )?)
}

/// Deletes the previous character or joins with the previous line when allowed.
pub fn backspace(editor: &mut Editor, buffer: BufHandle, window: WinHandle, cursor: Position, allow_join: bool, timestamp: i64) -> Result<Position, EditorError> {
    if cursor.col > 0 {
        let mut line = line(editor, buffer, cursor.lnum)?; let mut start = cursor.col.min(line.len()).saturating_sub(1);
        while start > 0 && !std::str::from_utf8(&line).map_or(true, |text| text.is_char_boundary(start)) { start -= 1; }
        line.drain(start..cursor.col.min(line.len())); let after = Position { lnum: cursor.lnum, col: start };
        editor.replace_buffer_lines(buffer, cursor.lnum, cursor.lnum, &[line], cursor, after, timestamp)?; editor.set_window_cursor(window, after)?; return Ok(after);
    }
    if cursor.lnum <= 1 || !allow_join { return Ok(cursor); }
    let previous = line(editor, buffer, cursor.lnum - 1)?; let current = line(editor, buffer, cursor.lnum)?;
    let col = previous.len(); let mut joined = previous; joined.extend(current); let after = Position { lnum: cursor.lnum - 1, col };
    editor.replace_buffer_lines(buffer, cursor.lnum - 1, cursor.lnum, &[joined], cursor, after, timestamp)?; editor.set_window_cursor(window, after)?; Ok(after)
}

/// Leaves insertion on the character before the insertion point, as `stop_insert()` does.
pub fn normal_cursor(editor: &mut Editor, window: WinHandle, cursor: Position) -> Result<Position, EditorError> {
    let after = Position { lnum: cursor.lnum, col: cursor.col.saturating_sub(1) }; editor.set_window_cursor(window, after)?; Ok(after)
}
