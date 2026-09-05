//! Include/define search (`search.c` `find_pattern_in_path`).

use ox_text::Position;
use ox_types::{Object, OxStr};

use crate::editor::{Editor, Message, MessageKind};
use crate::motion::next_ident_at;

/// Identifier versus `#define` search.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentSearchKind {
    /// Any whole-word or regexp match (`FIND_ANY`).
    Any,
    /// `#define` lines only (`FIND_DEFINE`).
    Define,
}

/// Display, list, jump, or split (`ACTION_*`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentSearchAction {
    /// Show the Nth match (`[i` / `:isearch`).
    Show,
    /// List every match (`[I` / `:ilist`).
    List,
    /// Jump to the Nth match (`[ CTRL-I` / `:ijump`).
    Goto,
    /// Split, then jump (`CTRL-W i` / `:isplit`).
    Split,
}

/// One match from `:ijump`/`:isearch`/`:djump`/`:dsearch`.
pub struct IdentSearchHit {
    /// 1-based line.
    pub lnum: usize,
    /// 0-based byte column of the match.
    pub col: usize,
    /// Line text, lossy-decoded.
    pub text: String,
    /// File containing the match, when it is not the current buffer.
    pub filename: Option<String>,
}

/// Failed ident/define search (`E349`/`E387`/`E388`/`E389`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentSearchError {
    /// Vim error code.
    pub code: &'static str,
    /// Message text without the `E123: ` prefix.
    pub message: String,
}

/// Collect matches in `[start_lnum, end_lnum]`.
#[must_use]
pub fn collect_hits(
    lines: &[Vec<u8>],
    pattern: &[u8],
    whole: bool,
    kind: IdentSearchKind,
    start_lnum: usize,
    end_lnum: usize,
) -> Vec<IdentSearchHit> {
    collect_hits_in(lines, pattern, whole, kind, start_lnum, end_lnum, None)
}

fn collect_hits_in(
    lines: &[Vec<u8>],
    pattern: &[u8],
    whole: bool,
    kind: IdentSearchKind,
    start_lnum: usize,
    end_lnum: usize,
    filename: Option<&str>,
) -> Vec<IdentSearchHit> {
    let mut hits = Vec::new();
    let start = start_lnum.max(1);
    let end = end_lnum.min(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let lnum = index + 1;
        if lnum < start || lnum > end {
            continue;
        }
        if kind == IdentSearchKind::Define {
            let trimmed = line
                .iter()
                .position(|byte| !byte.is_ascii_whitespace())
                .unwrap_or(0);
            if !line[trimmed..].starts_with(b"#define") {
                continue;
            }
        }

        let col = if whole {
            next_ident_at(line, 0, pattern)
        } else if pattern.is_empty() {
            None
        } else {
            line.windows(pattern.len())
                .position(|window| window == pattern)
        };
        let Some(col) = col else { continue };
        hits.push(IdentSearchHit {
            lnum,
            col,
            text: String::from_utf8_lossy(line).into_owned(),
            filename: filename.map(str::to_owned),
        });
    }
    hits
}

/// Walk the buffer in order; on `#include`, search that file before the rest.
#[must_use]
pub fn collect_hits_with_includes(
    lines: &[Vec<u8>],
    pattern: &[u8],
    whole: bool,
    kind: IdentSearchKind,
    start_lnum: usize,
    end_lnum: usize,
    relative_to: Option<&std::path::Path>,
) -> Vec<IdentSearchHit> {
    let cwd = std::env::current_dir().ok();
    let start = start_lnum.max(1);
    let end = end_lnum.min(lines.len());
    let mut hits = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let lnum = index + 1;
        if lnum < start || lnum > end {
            continue;
        }
        if let Some(name) = include_file_name(line) {
            let mut candidates = Vec::new();
            if let Some(base) = relative_to {
                candidates.push(base.join(&name));
            }
            if let Some(dir) = &cwd {
                candidates.push(dir.join(&name));
            }
            candidates.push(std::path::PathBuf::from(&name));
            if let Some(path) = candidates.into_iter().find(|path| path.is_file())
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                let included: Vec<Vec<u8>> = text
                    .split('\n')
                    .map(|item| item.as_bytes().to_vec())
                    .collect();
                let stored = path.to_string_lossy().into_owned();
                hits.extend(collect_hits_in(
                    &included,
                    pattern,
                    whole,
                    kind,
                    1,
                    included.len(),
                    Some(&stored),
                ));
            }
            continue;
        }
        hits.extend(
            collect_hits_in(std::slice::from_ref(line), pattern, whole, kind, 1, 1, None)
                .into_iter()
                .map(|mut hit| {
                    hit.lnum = lnum;
                    hit
                }),
        );
    }
    hits
}

fn include_file_name(line: &[u8]) -> Option<String> {
    let trimmed = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(0);
    let rest = line.get(trimmed..)?;
    if !rest.starts_with(b"#include") {
        return None;
    }
    let after = rest.get(b"#include".len()..)?;
    let start = after.iter().position(|byte| !byte.is_ascii_whitespace())?;
    let name = &after[start..];
    let name = name
        .strip_prefix(b"\"")
        .or_else(|| name.strip_prefix(b"<"))
        .unwrap_or(name);
    let end = name
        .iter()
        .position(|byte| matches!(*byte, b'"' | b'>' | b' ' | b'\t'))
        .unwrap_or(name.len());
    let name = std::str::from_utf8(&name[..end]).ok()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Show, list, jump, or split for the collected matches.
///
/// # Errors
///
/// Returns an error when no requested match exists, the match is on the
/// current line, the editor has no current buffer, tab page, or window, or a
/// requested split, buffer switch, or cursor move fails.
pub fn apply(
    editor: &mut Editor,
    hits: &[IdentSearchHit],
    action: IdentSearchAction,
    count: usize,
    current_lnum: usize,
    kind: IdentSearchKind,
) -> Result<(), IdentSearchError> {
    match action {
        IdentSearchAction::List => {
            if hits.is_empty() {
                return Err(not_found(kind));
            }
            let listing = hits
                .iter()
                .enumerate()
                .map(|(index, hit)| format!("  {}: {:4} {}", index + 1, hit.lnum, hit.text))
                .collect::<Vec<_>>()
                .join("\n");
            push_search_message(editor, &listing);
            Ok(())
        }
        IdentSearchAction::Show | IdentSearchAction::Goto | IdentSearchAction::Split => {
            let index = count.max(1) - 1;
            let Some(hit) = hits.get(index) else {
                return Err(not_found(kind));
            };
            if hit.filename.is_none() && hit.lnum == current_lnum {
                return Err(IdentSearchError {
                    code: "E387",
                    message: "Match is on current line".to_owned(),
                });
            }
            if action == IdentSearchAction::Show {
                push_search_message(editor, &hit.text);
                return Ok(());
            }
            if action == IdentSearchAction::Split {
                split_current(editor)?;
            }
            jump_to(editor, hit)
        }
    }
}

fn push_search_message(editor: &mut Editor, text: &str) {
    editor.push_info_message(Message {
        kind: MessageKind::Echo,
        content: Object::String(OxStr::from(text)),
        history: false,
        leading_newline: false,
    });
}

fn not_found(kind: IdentSearchKind) -> IdentSearchError {
    if kind == IdentSearchKind::Define {
        IdentSearchError {
            code: "E388",
            message: "Couldn't find definition".to_owned(),
        }
    } else {
        IdentSearchError {
            code: "E389",
            message: "Couldn't find pattern".to_owned(),
        }
    }
}

fn split_current(editor: &mut Editor) -> Result<(), IdentSearchError> {
    let buffer = editor.current_buffer().ok_or_else(|| IdentSearchError {
        code: "E749",
        message: "Empty buffer".to_owned(),
    })?;
    let tab = editor.current_tabpage().ok_or_else(|| IdentSearchError {
        code: "E749",
        message: "No current tabpage".to_owned(),
    })?;
    let window = editor.current_window().ok_or_else(|| IdentSearchError {
        code: "E749",
        message: "No current window".to_owned(),
    })?;
    let created = editor
        .split_above(tab, window, buffer, true)
        .map_err(|error| IdentSearchError {
            code: "E36",
            message: error.to_string(),
        })?;
    editor
        .set_current_window(created)
        .map_err(|error| IdentSearchError {
            code: "E36",
            message: error.to_string(),
        })?;

    Ok(())
}

fn jump_to(editor: &mut Editor, hit: &IdentSearchHit) -> Result<(), IdentSearchError> {
    if let Some(filename) = &hit.filename {
        let bytes = std::fs::read(filename).unwrap_or_default();
        let text = ox_text::Buffer::from_bytes(&bytes).unwrap_or_else(|_| ox_text::Buffer::new());
        let handle = editor
            .create_buffer_with(text, true)
            .map_err(|error| IdentSearchError {
                code: "E948",
                message: error.to_string(),
            })?;
        if let Ok(buffer) = editor.buffer_mut(handle) {
            buffer.set_name(OxStr::from(filename.as_str()));
            buffer.mark_saved();
        }
        editor
            .set_current_buffer(handle, crate::BufferRelease::KeepLoaded)
            .map_err(|error| IdentSearchError {
                code: "E948",
                message: error.to_string(),
            })?;
    }
    let window = editor.current_window().ok_or_else(|| IdentSearchError {
        code: "E749",
        message: "No current window".to_owned(),
    })?;

    editor
        .set_window_cursor(
            window,
            Position {
                lnum: hit.lnum,
                col: hit.col,
            },
        )
        .map_err(|error| IdentSearchError {
            code: "E16",
            message: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquoted_include_name_is_extracted() {
        assert_eq!(
            include_file_name(b"#include Xinclude"),
            Some("Xinclude".to_owned())
        );
    }

    #[test]
    fn fifth_start_match_is_in_the_include_file() {
        let dir = std::env::temp_dir().join(format!("ox-include-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let include = dir.join("Xinclude");
        std::fs::write(
            &include,
            "/* test text test tex start here\n\t\tsome text\n\t\ttest text\n\t\tstart OK if found this line\n\tstart found wrong line\ntest text\n",
        )
        .unwrap();
        let lines = vec![
            b"#include Xinclude".to_vec(),
            b"".to_vec(),
            b"".to_vec(),
            b"/* test text test tex start here".to_vec(),
            b"\t\tsome text".to_vec(),
            b"\t\ttest text".to_vec(),
            b"\t\tstart OK if found this line".to_vec(),
            b"\tstart found wrong line".to_vec(),
            b"test text".to_vec(),
        ];
        let hits = collect_hits_with_includes(
            &lines,
            b"start",
            true,
            IdentSearchKind::Any,
            1,
            lines.len(),
            Some(dir.as_path()),
        );
        assert_eq!(
            hits.len(),
            6,
            "three in the include, then three in the buffer"
        );
        assert_eq!(hits[4].text, "\t\tstart OK if found this line");
        assert_eq!(hits[4].filename, None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
