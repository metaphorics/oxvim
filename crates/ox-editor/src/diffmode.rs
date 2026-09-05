//! Diff mode: `:diffthis`, `:diffoff` and `:diffupdate`.
//!
//! Upstream lives in `src/nvim/diff.c`: `ex_diffthis` (diff.c:1483) calls
//! `diff_win_options` (diff.c:1531), which saves the window's diff-adjacent
//! options into `w_p_*_save` on first entry, applies the diff defaults from
//! `nvim_diff_win_options` — `scrollbind`/`cursorbind` on, `wrap` off,
//! `foldmethod=diff`, `foldcolumn` from `'diffopt'` (default 2), `foldenable`
//! on, `foldlevel` 0 — and only then sets `'diff'` locally. `ex_diffoff`
//! (diff.c:1597) reverses those for the current window, or for every diff
//! window of the tabpage with `!`, restoring what was saved and dropping the
//! shared diff blocks when no diff window remains. `ex_diffupdate`
//! (diff.c:1073) rebuilds the blocks from every diff window's current text.
//!
//! The blocks themselves are computed here with the xdiff-equivalent Myers
//! diff from the `similar` crate, one pass per compared buffer against the
//! reference buffer (`tp_diffbuf[0]`). `diff_infold` (diff.c:3547) is what
//! folds display: with `'foldmethod'` = `diff` the *changed* lines plus
//! `'diffopt'`'s `context:` lines (default 6) around them stay visible while
//! everything farther away folds shut, and buffers with no blocks at all fold
//! every line.

use std::collections::HashMap;

use ox_types::{BufHandle, TabHandle, WinHandle};
use similar::{Algorithm, DiffTag, TextDiff};

use crate::Editor;
use crate::fold::FoldRange;
use crate::options::OptionValue;

/// Upstream's `DB_COUNT` (`diff.h`): at most eight buffers take part in one
/// tabpage's diff.
const MAX_DIFF_BUFFERS: usize = 8;

/// The option values `diff_win_options` (diff.c:1542-1599) saves on first
/// entry into diff mode and `ex_diffoff` (diff.c:1616-1645) restores. The
/// presence of the entry is upstream's `w_p_diff_saved`.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
struct SavedWindowOptions {
    scrollbind: bool,
    cursorbind: bool,
    wrap: bool,
    foldmethod: String,
    foldcolumn: String,
    foldenable: bool,
    foldlevel: i64,
}

/// One changed region shared between the reference buffer and one compared
/// buffer — upstream's `diff_T` flattened to a pair. Start lines are 1-based;
/// a count of zero marks the line after which lines were inserted on the
/// other side (`xdiff_out`, diff.c:4283).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffBlock {
    /// 1-based start line and line count in the reference buffer.
    pub base: (usize, usize),
    /// The compared buffer.
    pub other_buffer: BufHandle,
    /// 1-based start line and line count in the compared buffer.
    pub other: (usize, usize),
}

/// A tabpage's computed blocks plus the changedtick each participating buffer
/// carried when they were computed — the port's `tp_diff_invalid`.
#[derive(Clone, Debug)]
struct TabBlocks {
    blocks: Vec<DiffBlock>,
    ticks: Vec<(BufHandle, u64)>,
}

/// The `'diffopt'` flags this module consumes (`diffopt_changed`,
/// diff.c:2660-2716).
#[allow(clippy::struct_excessive_bools)]
struct DiffOpts {
    context: usize,
    foldcolumn: usize,
    followwrap: bool,
    icase: bool,
    iwhite: bool,
    iwhiteeol: bool,
}

impl DiffOpts {
    /// Reads the flags from the global `'diffopt'` value. `context:` defaults
    /// to 6 and clamps 0 to 1, `foldcolumn:` to 2 (diff.c:2710-2713).
    fn read(editor: &Editor) -> Self {
        let text = match editor.options().get_global("diffopt") {
            Ok(OptionValue::String(value)) => value.as_str(),
            _ => "",
        };
        let mut opts = Self {
            context: 6,
            foldcolumn: 2,
            followwrap: false,
            icase: false,
            iwhite: false,
            iwhiteeol: false,
        };
        for item in text.split(',') {
            let item = item.trim();
            if let Some(value) = item.strip_prefix("context:") {
                opts.context = value.trim().parse().unwrap_or(6).max(1);
            } else if let Some(value) = item.strip_prefix("foldcolumn:") {
                opts.foldcolumn = value.trim().parse().unwrap_or(2).clamp(0, 9);
            } else {
                match item {
                    "followwrap" => opts.followwrap = true,
                    "icase" => opts.icase = true,
                    "iwhite" | "iwhiteall" => opts.iwhite = true,
                    "iwhiteeol" => opts.iwhiteeol = true,
                    _ => {}
                }
            }
        }
        opts
    }
}

/// Diff-mode editor state: saved window options (`w_p_diff_saved` and friends)
/// and the per-tabpage diff blocks (`tp_first_diff`).
#[derive(Clone, Debug, Default)]
pub struct DiffState {
    saved: HashMap<WinHandle, SavedWindowOptions>,
    blocks: HashMap<TabHandle, TabBlocks>,
}

/// # Errors
///
/// Returns an error string when no current window exists.
pub fn diffthis(editor: &mut Editor) -> Result<(), String> {
    let window = editor
        .current_window()
        .ok_or_else(|| "E444: No current window".to_owned())?;
    diff_win_options(editor, window)
}

/// `diff_win_options` (diff.c:1531): save the diff-adjacent options on first
/// entry, apply the diff defaults, then set `'diff'` locally last and add the
/// window's buffer to the tabpage's diff set.
fn diff_win_options(editor: &mut Editor, window: WinHandle) -> Result<(), String> {
    editor.window(window).map_err(|error| error.to_string())?;
    let already_diff = window_bool(editor, window, "diff");
    let opts = DiffOpts::read(editor);

    if !already_diff {
        let wrap = if opts.followwrap {
            // `followwrap` leaves 'wrap' alone and nothing is saved for it.
            false
        } else {
            window_bool(editor, window, "wrap")
        };
        editor.diff.saved.insert(
            window,
            SavedWindowOptions {
                scrollbind: window_bool(editor, window, "scrollbind"),
                cursorbind: window_bool(editor, window, "cursorbind"),
                wrap,
                foldmethod: window_text(editor, window, "foldmethod"),
                foldcolumn: window_text(editor, window, "foldcolumn"),
                foldenable: window_bool(editor, window, "foldenable"),
                foldlevel: window_number(editor, window, "foldlevel"),
            },
        );
    }

    // diff.c:1542-1556 — 'scrollbind' and 'cursorbind' on; 'wrap' off unless
    // 'diffopt' has followwrap.
    set_bool(editor, window, "scrollbind", true);
    set_bool(editor, window, "cursorbind", true);
    if !opts.followwrap {
        set_bool(editor, window, "wrap", false);
    }
    // diff.c:1564-1566 — 'foldmethod' is set locally, after the save.
    set_text(editor, window, "foldmethod", "diff");
    // diff.c:1568-1585 — 'foldcolumn' from 'diffopt', 'foldenable' on,
    // 'foldlevel' 0.
    set_text(editor, window, "foldcolumn", &opts.foldcolumn.to_string());
    set_bool(editor, window, "foldenable", true);
    set_number(editor, window, "foldlevel", 0);
    // diff.c:1595-1600 — add "hor" to 'scrollopt' once, then 'diff' locally.
    add_scrollopt_hor(editor);
    set_bool(editor, window, "diff", true);

    ensure_blocks(editor);
    Ok(())
}

/// `ex_diffoff` (diff.c:1597): turn `'diff'` off for the current window, or
/// for every diff window of the current tabpage with `!`, restoring the
/// options saved by [`diff_win_options`].
///
/// # Errors
///
/// Returns an error string when no current tabpage or window exists.
pub fn diffoff(editor: &mut Editor, bang: bool) -> Result<(), String> {
    let tab = editor
        .current_tabpage()
        .ok_or_else(|| "E444: No current tabpage".to_owned())?;
    let targets: Vec<WinHandle> = if bang {
        editor
            .tabpage_windows(tab)
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|&window| window_bool(editor, window, "diff"))
            .collect()
    } else {
        editor
            .current_window()
            .ok_or_else(|| "E444: No current window".to_owned())
            .map(|window| vec![window])?
    };

    let followwrap = DiffOpts::read(editor).followwrap;
    for window in targets {
        // diff.c:1610-1612 — with `!` only windows actually in diff mode are
        // processed; without it the current window always is.
        set_bool(editor, window, "diff", false);
        let Some(saved) = editor.diff.saved.get(&window).cloned() else {
            continue;
        };
        // Restore only settings still left over from diff mode
        // (diff.c:1616-1645).
        if window_bool(editor, window, "scrollbind") {
            set_bool(editor, window, "scrollbind", saved.scrollbind);
        }
        if window_bool(editor, window, "cursorbind") {
            set_bool(editor, window, "cursorbind", saved.cursorbind);
        }
        if !followwrap && !window_bool(editor, window, "wrap") && saved.wrap {
            set_bool(editor, window, "wrap", true);
        }
        let foldmethod = if saved.foldmethod.is_empty() {
            "manual".to_owned()
        } else {
            saved.foldmethod.clone()
        };
        set_text(editor, window, "foldmethod", &foldmethod);
        let foldcolumn = if saved.foldcolumn.is_empty() {
            "0".to_owned()
        } else {
            saved.foldcolumn.clone()
        };
        set_text(editor, window, "foldcolumn", &foldcolumn);
        if window_number(editor, window, "foldlevel") == 0 {
            set_number(editor, window, "foldlevel", saved.foldlevel);
        }
        // Only restore 'foldenable' when 'foldmethod' is not "manual",
        // otherwise the diff folds would keep showing (diff.c:1638-1641).
        if window_bool(editor, window, "foldenable") {
            let restore = if foldmethod == "manual" {
                false
            } else {
                saved.foldenable
            };
            set_bool(editor, window, "foldenable", restore);
        }
    }

    // diff.c:1653-1660 — drop the blocks when no diff window remains, and
    // take "hor" back out of 'scrollopt' (it is never restored per window).
    let any_diff = editor
        .tabpage_windows(tab)
        .map_err(|error| error.to_string())?
        .into_iter()
        .any(|window| window_bool(editor, window, "diff"));
    #[allow(clippy::if_not_else)]
    if !any_diff {
        editor.diff.blocks.remove(&tab);
        remove_scrollopt_hor(editor);
    } else {
        ensure_blocks(editor);
    }
    Ok(())
}

/// `ex_diffupdate` (diff.c:1073): rebuild the tabpage's blocks from every
/// diff window's current text. Upstream's `!` forces the external `diff`
/// program; with only the internal computation present both spellings do the
/// same work.
pub fn diffupdate(editor: &mut Editor) {
    if let Some(tab) = editor.current_tabpage() {
        editor.diff.blocks.remove(&tab);
    }
    ensure_blocks(editor);
}

/// Recomputes the current tabpage's blocks unless every participating buffer
/// still has the changedtick the last computation saw (`tp_diff_invalid`,
/// diff.c:3555-3559).
pub fn ensure_blocks(editor: &mut Editor) {
    let Some(tab) = editor.current_tabpage() else {
        return;
    };
    let participants = diff_participants(editor, tab);
    let ticks: Vec<(BufHandle, u64)> = participants
        .iter()
        .map(|&(_, buffer)| {
            (
                buffer,
                editor
                    .buffer(buffer)
                    .map_or(0, crate::buffer::BufferState::changedtick),
            )
        })
        .collect();
    if let Some(existing) = editor.diff.blocks.get(&tab)
        && existing.ticks == ticks
    {
        return;
    }
    let opts = DiffOpts::read(editor);
    let blocks = compute_blocks(editor, &participants, &opts);
    editor.diff.blocks.insert(tab, TabBlocks { blocks, ticks });
}

/// The windows of `tab` whose `'diff'` is set, deduplicated by buffer in
/// window order — the port's `tp_diffbuf[]` population (`diff_buf_add`,
/// diff.c:190-210, capped at `DB_COUNT`).
fn diff_participants(editor: &Editor, tab: TabHandle) -> Vec<(WinHandle, BufHandle)> {
    let mut participants: Vec<(WinHandle, BufHandle)> = Vec::new();
    let mut buffers: Vec<BufHandle> = Vec::new();
    for window in editor.tabpage_windows(tab).unwrap_or_default() {
        if !window_bool(editor, window, "diff") {
            continue;
        }
        let Ok(state) = editor.window(window) else {
            continue;
        };
        let buffer = state.buffer;
        if buffers.contains(&buffer) {
            continue;
        }
        if buffers.len() == MAX_DIFF_BUFFERS {
            break;
        }
        buffers.push(buffer);
        participants.push((window, buffer));
    }
    participants
}

/// Diffs every compared buffer against the reference buffer
/// (`ex_diffupdate`, diff.c:1089-1110).
fn compute_blocks(
    editor: &Editor,
    participants: &[(WinHandle, BufHandle)],
    opts: &DiffOpts,
) -> Vec<DiffBlock> {
    let Some(&(_, base_buffer)) = participants.first() else {
        return Vec::new();
    };
    let Ok(base_lines) = crate::excmd_exec::buffer_lines(editor, base_buffer) else {
        return Vec::new();
    };
    let base_keys: Vec<Vec<u8>> = base_lines.iter().map(|l| compare_key(l, opts)).collect();
    let base_text = lines_to_text(&base_keys);
    let mut blocks: Vec<DiffBlock> = Vec::new();
    for &(_, buffer) in participants.iter().skip(1) {
        let Ok(lines) = crate::excmd_exec::buffer_lines(editor, buffer) else {
            continue;
        };
        let keys: Vec<Vec<u8>> = lines.iter().map(|l| compare_key(l, opts)).collect();
        let other_text = lines_to_text(&keys);
        let diff = TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .diff_lines(&base_text, &other_text);
        for operation in diff.ops() {
            if operation.tag() == DiffTag::Equal {
                continue;
            }
            // `xdiff_out` (diff.c:4283) records hunks as 1-based starts with
            // zero counts marking pure insertions.
            let block = DiffBlock {
                base: (operation.old_range().start + 1, operation.old_range().len()),
                other_buffer: buffer,
                other: (operation.new_range().start + 1, operation.new_range().len()),
            };
            // Merge hunks that touch in both buffers, the way upstream's
            // block list keeps one block per contiguous change.
            if let Some(last) = blocks.last_mut()
                && last.other_buffer == buffer
                && last.base.0 + last.base.1 == block.base.0
                && last.other.0 + last.other.1 == block.other.0
            {
                last.base.1 += block.base.1;
                last.other.1 += block.other.1;
                continue;
            }
            blocks.push(block);
        }
    }
    blocks
}

/// Joins line keys with newlines for `similar`'s line-based diff, which
/// splits on `\n` and reports per-line operations.
fn lines_to_text(lines: &[Vec<u8>]) -> String {
    lines
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `diff_infold` (diff.c:3547) for the current window as closed fold ranges
/// over a `line_count`-line buffer: the lines away from every change fold
/// shut while each change and `'diffopt'`'s `context:` lines around it stay
/// visible. Empty when the window is not in diff mode or its buffer has no
/// diff partner; when there are no blocks at all every line folds
/// (diff.c:3575-3577).
pub fn diff_fold_ranges(editor: &mut Editor, line_count: usize) -> Vec<FoldRange> {
    let Some(tab) = editor.current_tabpage() else {
        return Vec::new();
    };
    let Some(window) = editor.current_window() else {
        return Vec::new();
    };
    let Some(buffer) = editor.window(window).ok().map(|state| state.buffer) else {
        return Vec::new();
    };
    if !window_bool(editor, window, "diff") {
        return Vec::new();
    }
    let participants = diff_participants(editor, tab);
    if participants.len() < 2 || !participants.iter().any(|&(_, b)| b == buffer) {
        return Vec::new();
    }
    ensure_blocks(editor);
    let Some(entry) = editor.diff.blocks.get(&tab) else {
        return Vec::new();
    };
    // The reference window sees every block; a compared window its own.
    let spans: Vec<(usize, usize)> = entry
        .blocks
        .iter()
        .filter(|block| buffer == participants[0].1 || block.other_buffer == buffer)
        .map(|block| {
            if buffer == participants[0].1 {
                block.base
            } else {
                block.other
            }
        })
        .collect();
    let context = DiffOpts::read(editor).context;
    fold_ranges_around(spans, context, line_count)
}

/// Turns the visible spans (1-based start, count) into the complementary
/// closed fold ranges, half-open zero-based rows.
fn fold_ranges_around(
    spans: Vec<(usize, usize)>,
    context: usize,
    line_count: usize,
) -> Vec<FoldRange> {
    if line_count == 0 {
        return Vec::new();
    }
    if spans.is_empty() {
        // "Return if there are no diff blocks. All lines will be folded."
        return FoldRange::lines(0, line_count).into_iter().collect();
    }
    let mut ranges: Vec<FoldRange> = Vec::new();
    let mut next_free_row = 0;
    for (start, count) in spans {
        let visible_first = start.saturating_sub(context).max(1) - 1;
        let visible_past = (start + count - 1 + context).min(line_count);
        if next_free_row < visible_first
            && let Ok(range) = FoldRange::lines(next_free_row, visible_first)
        {
            ranges.push(range);
        }
        next_free_row = next_free_row.max(visible_past);
    }
    if next_free_row < line_count
        && let Ok(range) = FoldRange::lines(next_free_row, line_count)
    {
        ranges.push(range);
    }
    ranges
}

/// Adds "hor" to `'scrollopt'` when missing (diff.c:1595-1597).
fn add_scrollopt_hor(editor: &mut Editor) {
    let value = match editor.options().get_global("scrollopt") {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => return,
    };
    if value.bytes().any(|byte| byte == b'h') {
        return;
    }
    let next = if value.is_empty() {
        "hor".to_owned()
    } else {
        format!("{value},hor")
    };
    let _ = editor
        .options_mut()
        .set_global("scrollopt", OptionValue::String(next));
}

/// Removes "hor" from `'scrollopt'` when no diff windows remain
/// (diff.c:1666-1669).
fn remove_scrollopt_hor(editor: &mut Editor) {
    let value = match editor.options().get_global("scrollopt") {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => return,
    };
    let kept: Vec<&str> = value
        .split(',')
        .filter(|item| !item.bytes().any(|byte| byte == b'h'))
        .collect();
    if kept.len() == value.split(',').count() {
        return;
    }
    let _ = editor
        .options_mut()
        .set_global("scrollopt", OptionValue::String(kept.join(",")));
}

fn window_bool(editor: &Editor, window: WinHandle, name: &str) -> bool {
    matches!(
        editor.options().get_window(window, name),
        Ok(OptionValue::Boolean(true))
    )
}

fn window_number(editor: &Editor, window: WinHandle, name: &str) -> i64 {
    match editor.options().get_window(window, name) {
        Ok(OptionValue::Number(value)) => *value,
        _ => 0,
    }
}

fn window_text(editor: &Editor, window: WinHandle, name: &str) -> String {
    match editor.options().get_window(window, name) {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => String::new(),
    }
}

fn set_bool(editor: &mut Editor, window: WinHandle, name: &str, value: bool) {
    let _ = editor
        .options_mut()
        .set_window(window, name, OptionValue::Boolean(value));
}

fn set_number(editor: &mut Editor, window: WinHandle, name: &str, value: i64) {
    let _ = editor
        .options_mut()
        .set_window(window, name, OptionValue::Number(value));
}

fn set_text(editor: &mut Editor, window: WinHandle, name: &str, value: &str) {
    let _ = editor
        .options_mut()
        .set_window(window, name, OptionValue::String(value.to_owned()));
}

/// The byte key two lines are compared under, applying the `'diffopt'`
/// whitespace and case flags. Upstream folds case for xdiff by hand
/// (diff.c:816-818) and passes whitespace flags to xdiff; here the same
/// effect comes from normalizing each line before the comparison.
fn compare_key(line: &[u8], opts: &DiffOpts) -> Vec<u8> {
    let mut key = line.to_vec();
    if opts.iwhiteeol {
        while key.last().is_some_and(u8::is_ascii_whitespace) {
            key.pop();
        }
    }
    if opts.iwhite {
        key.retain(|byte| !byte.is_ascii_whitespace());
    }
    if opts.icase {
        key.make_ascii_lowercase();
    }
    key
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! Every expectation below is the value the oracle at
    //! `.references/neovim/build/bin/nvim` (v0.13.0-dev-1390) answers for the
    //! same script, recorded next to each assertion.

    use ox_eval::ScopeKind;
    use ox_types::Typval;

    use super::*;
    use crate::TestEditorAccess;
    use crate::excmd_exec::ExExecutor;
    use crate::options::OptionValue;
    use crate::{Editor, Geometry};

    /// An editor with one listed buffer shown in one 80x24 window.
    fn editor() -> Editor {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        editor
    }

    /// Runs `script` and answers the numeric globals `names` in order.
    fn numbers(script: &str, names: &[&str]) -> Vec<i64> {
        let editor = TestEditorAccess::new(editor());
        let mut exec = ExExecutor::new();
        exec.execute_script(&editor, "diffmode.vim", script)
            .unwrap();
        names
            .iter()
            .map(|name| {
                match exec
                    .scope()
                    .get_scoped(ScopeKind::Global, name.as_bytes(), 0)
                    .unwrap_or_else(|error| panic!("no g:{name}: {error:?}"))
                {
                    Typval::Number(value) => *value,
                    other => panic!("expected a Number in g:{name}, got {other:?}"),
                }
            })
            .collect()
    }

    /// Runs `script` and answers the current window's string option `name`.
    fn window_string(script: &str, name: &str) -> String {
        let editor = TestEditorAccess::new(editor());
        let mut exec = ExExecutor::new();
        exec.execute_script(&editor, "diffmode.vim", script)
            .unwrap();
        let window = editor.editor().current_window().unwrap();
        match editor.editor().options().get_window(window, name) {
            Ok(OptionValue::String(value)) => value.clone(),
            other => panic!("expected a String for {name}, got {other:?}"),
        }
    }

    /// Runs `script` and answers the current window's boolean option `name`.
    fn current_window_bool(script: &str, name: &str) -> bool {
        let editor = TestEditorAccess::new(editor());
        let mut exec = ExExecutor::new();
        exec.execute_script(&editor, "diffmode.vim", script)
            .unwrap();
        let window = editor.editor().current_window().unwrap();
        matches!(
            editor.editor().options().get_window(window, name),
            Ok(OptionValue::Boolean(true))
        )
    }

    // diff.c:1531 — `:diffthis` sets the diff window options. Oracle
    // (`set foldmethod=marker foldcolumn=4` then `diffthis`): &diff=1,
    // &foldmethod="diff", &foldcolumn="2", &scrollbind=1, &cursorbind=1,
    // &wrap=0.
    #[test]
    fn diffthis_sets_the_diff_window_options() {
        let script = "set foldmethod=marker foldcolumn=4\ndiffthis";
        assert!(current_window_bool(script, "diff"));
        assert_eq!(window_string(script, "foldmethod"), "diff");
        assert_eq!(window_string(script, "foldcolumn"), "2");
        assert!(current_window_bool(script, "scrollbind"));
        assert!(current_window_bool(script, "cursorbind"));
        assert!(!current_window_bool(script, "wrap"));
    }

    // diff.c:1597 — `:diffoff` restores the options saved on entry. Oracle
    // (`set foldmethod=marker foldcolumn=4` then `diffthis` then `diffoff`):
    // &diff=0, &foldmethod="marker", &foldcolumn="4", &scrollbind=0,
    // &cursorbind=0, &wrap=1.
    #[test]
    fn diffoff_restores_the_saved_options() {
        let script = "set foldmethod=marker foldcolumn=4\ndiffthis\ndiffoff";
        assert!(!current_window_bool(script, "diff"));
        assert_eq!(window_string(script, "foldmethod"), "marker");
        assert_eq!(window_string(script, "foldcolumn"), "4");
        assert!(!current_window_bool(script, "scrollbind"));
        assert!(!current_window_bool(script, "cursorbind"));
        assert!(current_window_bool(script, "wrap"));
    }

    // diff.c:1610 — `:diffoff!` clears every diff window of the tabpage and
    // drops the shared blocks when none remain.
    #[test]
    fn diffoff_bang_clears_every_diff_window() {
        let editor = TestEditorAccess::new(editor());
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "diffmode.vim",
            "call setline(1, range(50))\n\
             diffthis\n\
             new\n\
             call setline(1, range(50))\n\
             diffthis\n\
             diffoff!",
        )
        .unwrap();
        let tab = editor.editor().current_tabpage().unwrap();
        let windows = editor.editor().tabpage_windows(tab).unwrap();
        for window in windows {
            assert!(
                !window_bool(&editor.editor(), window, "diff"),
                "window {window:?} still in diff mode"
            );
        }
        assert!(!editor.editor().diff.blocks.contains_key(&tab));
    }

    fn window_bool(editor: &Editor, window: WinHandle, name: &str) -> bool {
        matches!(
            editor.options().get_window(window, name),
            Ok(OptionValue::Boolean(true))
        )
    }

    // diff.c:3547 — with `'foldmethod'` = `diff` the lines away from every
    // change fold shut. Oracle (50 lines, line 26 changed, default context):
    // fce10=19 fc49=33 fce49=50 fc26=-1 fl26=0 fl10=1.
    #[test]
    fn diff_folds_fold_away_from_changes() {
        let values = numbers(
            "call setline(1, range(50))\n\
             diffthis\n\
             new\n\
             call setline(1, map(range(50), 'v:val == 25 ? \"diff\" : v:val'))\n\
             diffthis\n\
             let g:a = foldclosedend(10)\n\
             let g:b = foldclosed(49)\n\
             let g:c = foldclosedend(49)\n\
             let g:d = foldclosed(26)\n\
             let g:e = foldlevel(26)\n\
             let g:f = foldlevel(10)",
            &["a", "b", "c", "d", "e", "f"],
        );
        assert_eq!(values, vec![19, 33, 50, -1, 0, 1]);
    }

    // diff.c:3575 — when the diff buffers are identical there are no blocks,
    // so every line folds. Oracle: fc1=1 fce1=50 fc25=1 fl25=1.
    #[test]
    fn identical_diff_buffers_fold_every_line() {
        let values = numbers(
            "call setline(1, range(50))\n\
             diffthis\n\
             new\n\
             call setline(1, range(50))\n\
             diffthis\n\
             let g:a = foldclosed(1)\n\
             let g:b = foldclosedend(1)\n\
             let g:c = foldclosed(25)\n\
             let g:d = foldlevel(25)",
            &["a", "b", "c", "d"],
        );
        assert_eq!(values, vec![1, 50, 1, 1]);
    }

    // diff.c:3551 — a diff window whose buffer has no diff partner folds
    // nothing. Oracle (single `diffthis`): fc10=-1 fl10=0.
    #[test]
    fn a_diff_window_without_a_partner_folds_nothing() {
        let values = numbers(
            "call setline(1, range(50))\n\
             diffthis\n\
             let g:a = foldclosed(10)\n\
             let g:b = foldlevel(10)",
            &["a", "b"],
        );
        assert_eq!(values, vec![-1, 0]);
    }

    // diff.c:2710 — `diffopt=context:N` shrinks the visible window around
    // each change. Oracle (`set diffopt=filler,context:2`, line 26 changed):
    // fce10=23 fc49=29 fce49=50 fc26=-1.
    #[test]
    fn diffopt_context_shrinks_the_visible_window() {
        let values = numbers(
            "set diffopt=filler,context:2\n\
             call setline(1, range(50))\n\
             diffthis\n\
             new\n\
             call setline(1, map(range(50), 'v:val == 25 ? \"diff\" : v:val'))\n\
             diffthis\n\
             let g:a = foldclosedend(10)\n\
             let g:b = foldclosed(49)\n\
             let g:c = foldclosedend(49)\n\
             let g:d = foldclosed(26)",
            &["a", "b", "c", "d"],
        );
        assert_eq!(values, vec![23, 29, 50, -1]);
    }

    // diff.c:2713 — `diffopt=foldcolumn:N` overrides the default fold column.
    // Oracle (`set diffopt=foldcolumn:4` then `diffthis`): &foldcolumn="4".
    #[test]
    fn diffopt_foldcolumn_overrides_the_default() {
        assert_eq!(
            window_string("set diffopt=foldcolumn:4\ndiffthis", "foldcolumn"),
            "4"
        );
    }

    // diff.c:1551 — `diffopt=followwrap` leaves `'wrap'` alone. Oracle
    // (`set diffopt=followwrap` then `diffthis`): &wrap=1.
    #[test]
    fn diffopt_followwrap_keeps_wrap_on() {
        assert!(current_window_bool(
            "set diffopt=followwrap\ndiffthis",
            "wrap"
        ));
    }

    // diff.c:816 — `diffopt=icase` treats case variants as equal, so buffers
    // differing only in case have no blocks and fold every line. Oracle
    // (['Foo','bar','Baz'] vs ['foo','bar','baz']): fc1=1 fce1=3.
    #[test]
    fn diffopt_icase_treats_case_variants_as_equal() {
        let values = numbers(
            "set diffopt=icase\n\
             call setline(1, ['Foo','bar','Baz'])\n\
             diffthis\n\
             new\n\
             call setline(1, ['foo','bar','baz'])\n\
             diffthis\n\
             let g:a = foldclosed(1)\n\
             let g:b = foldclosedend(1)",
            &["a", "b"],
        );
        assert_eq!(values, vec![1, 3]);
    }

    // diff.c:808 — `diffopt=iwhite` ignores whitespace, so buffers differing
    // only in whitespace have no blocks and fold every line. Oracle
    // (['a  b','c'] vs ['a b','c']): fc1=1 fce1=2.
    #[test]
    fn diffopt_iwhite_ignores_whitespace() {
        let values = numbers(
            "set diffopt=iwhite\n\
             call setline(1, ['a  b','c'])\n\
             diffthis\n\
             new\n\
             call setline(1, ['a b','c'])\n\
             diffthis\n\
             let g:a = foldclosed(1)\n\
             let g:b = foldclosedend(1)",
            &["a", "b"],
        );
        assert_eq!(values, vec![1, 2]);
    }

    // diff.c:1073 — `:diffupdate` rebuilds the blocks after an edit. Oracle
    // (identical buffers, all folded; edit line 26 to "diff"; `diffupdate`):
    // fc26=-1 fce10=19.
    #[test]
    fn diffupdate_recomputes_after_an_edit() {
        let values = numbers(
            "call setline(1, range(50))\n\
             diffthis\n\
             new\n\
             call setline(1, range(50))\n\
             diffthis\n\
             let g:before = foldclosed(10)\n\
             call setline(26, 'diff')\n\
             diffupdate\n\
             let g:a = foldclosed(26)\n\
             let g:b = foldclosedend(10)",
            &["a", "b"],
        );
        assert_eq!(values, vec![-1, 19]);
    }
}
