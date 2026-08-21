//! Core insert-mode editing (`edit.c`/`insert.c`).

use ox_text::Position;
use ox_types::{BufHandle, WinHandle};

use crate::{BufferStateError, Editor, EditorError};

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

/// Splits the current line at the insertion cursor.
pub fn newline(editor: &mut Editor, buffer: BufHandle, window: WinHandle, cursor: Position, timestamp: i64) -> Result<Position, EditorError> {
    let line = line(editor, buffer, cursor.lnum)?; let col = cursor.col.min(line.len());
    let replacement = [line[..col].to_vec(), line[col..].to_vec()]; let after = Position { lnum: cursor.lnum + 1, col: 0 };
    editor.replace_buffer_lines(buffer, cursor.lnum, cursor.lnum, &replacement, cursor, after, timestamp)?;
    editor.set_window_cursor(window, after)?; Ok(after)
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
