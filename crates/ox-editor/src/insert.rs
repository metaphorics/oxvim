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
