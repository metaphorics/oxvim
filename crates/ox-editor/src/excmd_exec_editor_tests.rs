//! Behavioral tests for ExExecutor editor-integration commands.
//!
//! Covers `:edit`/`:write` via in-memory FileIO, modified state and E37
//! close/quit, split/vsplit/close, bnext/bprev/buffer, buffer naming/content/save,
//! map/noremap/unmap/mapclear families across modes, augroup/autocmd
//! registration/clearing, and `:command` definition/bang replacement/execution.
//!
//! Citations:
//! - `src/nvim/ex_docmd.c`: `do_cmdline` dispatch, `ex_edit`, `ex_write`,
//!   `ex_close`, `ex_splitview`, `ex_buffer_all`, `ex_map`, `ex_autocmd`,
//!   `ex_command` — command semantics and error codes (E32, E37, E45, E174,
//!   E183, E184, E212, E444, E484, E492).
//! - `src/nvim/runtime.c`: augroup and autocmd registration lifecycle,
//!   scriptnames registry.
//! - `test/old/testdir/test_usercommands.vim`: user command definition,
//!   bang replacement (`command!`), and `delcommand` patterns.
//! - `src/nvim/mapping.c`: map/noremap/unmap/mapclear mode dispatch
//!   (`mapping.c:624-629,1455-1529`).
//! - `src/nvim/autocmd.c`: autocmd registration and augroup scoping
//!   (`autocmd.c:887-957,1865-1890`).

#![allow(clippy::unwrap_used)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ox_text::Buffer;

use crate::script::FileIO;
use crate::{Editor, ExecError, ExecOutcome, ExExecutor, Geometry, Lookup, MapMode, VimExceptionKind};

// ---------------------------------------------------------------------------
// In-memory FileIO for :edit/:write tests
// ---------------------------------------------------------------------------

/// In-memory file store backing the FileIO seam, so tests can drive
/// `:edit`/`:write` without touching the real filesystem.
#[derive(Clone, Default)]
struct MemoryFileIO {
    files: Rc<RefCell<HashMap<PathBuf, String>>>,
}

impl MemoryFileIO {
    fn new() -> Self {
        Self::default()
    }

    fn insert(&self, path: &str, content: &str) {
        self.files
            .borrow_mut()
            .insert(PathBuf::from(path), content.to_owned());
    }

    fn content(&self, path: &str) -> Option<String> {
        self.files.borrow().get(&PathBuf::from(path)).cloned()
    }
}

impl FileIO for MemoryFileIO {
    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        self.files
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"))
    }

    fn write_string(&self, path: &Path, contents: &str) -> std::io::Result<()> {
        self.files
            .borrow_mut()
            .insert(path.to_path_buf(), contents.to_owned());
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.borrow().contains_key(path)
    }

    fn canonicalize(&self, path: &Path) -> PathBuf {
        path.to_path_buf()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Creates an editor with one empty listed buffer displayed in a tabpage,
/// plus an executor backed by an empty in-memory file store.
fn setup() -> (Editor, ExExecutor<MemoryFileIO>) {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    (editor, ExExecutor::with_io(MemoryFileIO::new()))
}

/// Creates an editor whose buffer starts with `lines` (EOL-terminated),
/// plus an executor backed by an empty in-memory file store.
fn setup_with_content(lines: &[Vec<u8>]) -> (Editor, ExExecutor<MemoryFileIO>) {
    let mut editor = Editor::new();
    let text = Buffer::from_lines(lines, true).unwrap();
    let buffer = editor.create_buffer_with(text, true).unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    (editor, ExExecutor::with_io(MemoryFileIO::new()))
}

/// Asserts that a result is a Vim exception with the given error code.
fn assert_vim_error(result: Result<ExecOutcome, ExecError>, expected_code: &str) {
    match result {
        Err(ExecError::Vim(exception)) => match &exception.kind {
            VimExceptionKind::Error(code) => assert_eq!(
                code, expected_code,
                "expected E{expected_code}, got E{code}: {}",
                exception.message()
            ),
            other => panic!("expected Error({expected_code}), got {other:?}"),
        },
        other => panic!("expected ExecError::Vim({expected_code}), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// :edit / :write via in-memory FileIO
// Citations: ex_docmd.c ex_edit (E32, E484), ex_write (E32, E45, E212)
// ---------------------------------------------------------------------------

/// `:edit {file}` loads file content through the FileIO seam into a new
/// buffer and switches the current window to it.
/// Upstream: `ex_docmd.c` `ex_edit` → `do_ecmd` → `readfile`.
#[test]
fn edit_loads_file_content_into_new_buffer() {
    let (mut editor, mut executor) = setup();
    executor.scripts().io().insert("test.txt", "hello world");
    executor
        .execute_line(&mut editor, "edit test.txt")
        .unwrap();

    let current = editor.current_buffer().unwrap();
    let state = editor.buffer(current).unwrap();
    assert_eq!(state.name().to_string_lossy(), "test.txt");
    assert_eq!(
        state.text().unwrap().line(1).unwrap(),
        b"hello world".to_vec()
    );
}

/// `:edit` with no file argument raises E32 ("No file name").
/// Upstream: `ex_docmd.c` `ex_edit` — `*e_fname == NUL` → E32.
#[test]
fn edit_empty_path_raises_e32() {
    let (mut editor, mut executor) = setup();
    let result = executor.execute_line(&mut editor, "edit");
    assert_vim_error(result, "E32");
}

/// `:write {file}` persists the current buffer's text through the FileIO
/// seam, matching `ex_write` → `buf_write` in `bufwrite.c`.
/// Upstream: `ex_docmd.c` `ex_write`.
#[test]
fn write_persists_buffer_content_through_fileio() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"line1".to_vec(), b"line2".to_vec()]);
    executor
        .execute_line(&mut editor, "write output.txt")
        .unwrap();

    assert_eq!(
        executor.scripts().io().content("output.txt"),
        Some("line1\nline2\n".to_owned())
    );
}

/// `:write {file}` sets the buffer name to the destination path and clears
/// the modified flag, matching `buf_write` → `buf_changedtick` save logic.
/// Upstream: `bufwrite.c:1727-1738`, `undo.c:2818-2824`.
#[test]
fn write_sets_buffer_name_and_clears_modified() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"content".to_vec()]);
    let buffer = editor.current_buffer().unwrap();
    editor.buffer_mut(buffer).unwrap().modified = true;

    executor
        .execute_line(&mut editor, "write saved.txt")
        .unwrap();

    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.name().to_string_lossy(), "saved.txt");
    assert!(!state.modified);
}

// ---------------------------------------------------------------------------
// Modified state and E37 close/quit
// Citations: ex_docmd.c ex_close (E37, E444), ex_edit (E37)
// ---------------------------------------------------------------------------

/// `:close` on a modified buffer without `!` raises E37.
/// Upstream: `ex_docmd.c` `ex_close` — `buf_modified` check before
/// `win_close`.
#[test]
fn close_modified_buffer_without_bang_raises_e37() {
    let (mut editor, mut executor) = setup();
    // Need two windows so :close can target one.
    executor.execute_line(&mut editor, "split").unwrap();
    assert_eq!(editor.windows().len(), 2);

    let buffer = editor.create_buffer(true).unwrap();
    editor
        .set_current_buffer(buffer, crate::BufferRelease::KeepLoaded)
        .unwrap();
    editor.buffer_mut(buffer).unwrap().modified = true;

    let result = executor.execute_line(&mut editor, "close");
    assert_vim_error(result, "E37");
    // Window count unchanged because close was refused.
    assert_eq!(editor.windows().len(), 2);
}

/// `:close!` on a modified buffer succeeds, discarding the modified check.
/// Upstream: `ex_docmd.c` `ex_close` — `forceit` skips the E37 guard.
#[test]
fn close_modified_buffer_with_bang_succeeds() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "split").unwrap();
    assert_eq!(editor.windows().len(), 2);

    let buffer = editor.current_buffer().unwrap();
    editor.buffer_mut(buffer).unwrap().modified = true;

    executor.execute_line(&mut editor, "close!").unwrap();
    assert_eq!(editor.windows().len(), 1);
}

/// `:edit {other}` on a modified buffer without `!` raises E37, because
/// `ex_edit` checks the current buffer's modified flag before replacing it.
/// Upstream: `ex_docmd.c` `ex_edit` — `buf_modified` → E37.
#[test]
fn edit_modified_buffer_without_bang_raises_e37() {
    let (mut editor, mut executor) = setup();
    let buffer = editor.current_buffer().unwrap();
    editor.buffer_mut(buffer).unwrap().modified = true;

    let result = executor.execute_line(&mut editor, "edit other.txt");
    assert_vim_error(result, "E37");
}

// ---------------------------------------------------------------------------
// Split / vsplit / close
// Citations: ex_docmd.c ex_splitview (E36), ex_close (E444)
// ---------------------------------------------------------------------------

/// `:split` creates a second tiled window showing the current buffer.
/// Upstream: `ex_docmd.c` `ex_splitview` → `win_split`.
#[test]
fn split_creates_second_window() {
    let (mut editor, mut executor) = setup();
    assert_eq!(editor.windows().len(), 1);

    executor.execute_line(&mut editor, "split").unwrap();
    assert_eq!(editor.windows().len(), 2);
}

/// `:vsplit` creates a second tiled window with a vertical split.
/// Upstream: `ex_docmd.c` `ex_splitview` with `WSP_VSPLIT`.
#[test]
fn vsplit_creates_second_window() {
    let (mut editor, mut executor) = setup();
    assert_eq!(editor.windows().len(), 1);

    executor.execute_line(&mut editor, "vsplit").unwrap();
    assert_eq!(editor.windows().len(), 2);
}

/// `:close` on an unmodified buffer in a multi-window tabpage closes the
/// current window and reduces the window count.
/// Upstream: `ex_docmd.c` `ex_close` → `win_close`.
#[test]
fn close_reduces_window_count() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "split").unwrap();
    assert_eq!(editor.windows().len(), 2);

    executor.execute_line(&mut editor, "close").unwrap();
    assert_eq!(editor.windows().len(), 1);
}

// ---------------------------------------------------------------------------
// bnext / bprev / buffer
// Citations: ex_docmd.c ex_buffer_all (E85, E86, E93)
// ---------------------------------------------------------------------------

/// `:bnext` cycles the current window to the next buffer in allocation
/// order, wrapping at the end.
/// Upstream: `ex_docmd.c` `ex_buffer_all` → `buflist_getnext`.
#[test]
fn bnext_cycles_to_next_buffer() {
    let (mut editor, mut executor) = setup();
    let buf1 = editor.current_buffer().unwrap();
    let buf2 = editor.create_buffer(true).unwrap();
    assert_eq!(editor.current_buffer(), Some(buf1));

    executor.execute_line(&mut editor, "bnext").unwrap();
    assert_eq!(editor.current_buffer(), Some(buf2));
}

/// `:bprev` cycles the current window to the previous buffer, wrapping at
/// the beginning.
/// Upstream: `ex_docmd.c` `ex_buffer_all` → `buflist_getprev`.
#[test]
fn bprev_cycles_to_previous_buffer() {
    let (mut editor, mut executor) = setup();
    let buf1 = editor.current_buffer().unwrap();
    let buf2 = editor.create_buffer(true).unwrap();
    // Switch to buf2 so bprev goes back to buf1.
    editor
        .set_current_buffer(buf2, crate::BufferRelease::KeepLoaded)
        .unwrap();
    assert_eq!(editor.current_buffer(), Some(buf2));

    executor.execute_line(&mut editor, "bprev").unwrap();
    assert_eq!(editor.current_buffer(), Some(buf1));
}

/// `:buffer {N}` switches the current window to the buffer with handle N.
/// Upstream: `ex_docmd.c` `ex_buffer_all` → `buflist_findnr`.
#[test]
fn buffer_switches_to_specified_handle() {
    let (mut editor, mut executor) = setup();
    let buf1 = editor.current_buffer().unwrap();
    let buf2 = editor.create_buffer(true).unwrap();
    assert_eq!(editor.current_buffer(), Some(buf1));

    executor.execute_line(&mut editor, "buffer 2").unwrap();
    assert_eq!(editor.current_buffer(), Some(buf2));
}

// ---------------------------------------------------------------------------
// Buffer naming / content / save
// Citations: ex_docmd.c ex_edit (name assignment), ex_write (E32)
// ---------------------------------------------------------------------------

/// `:edit {path}` sets the buffer name to the file path, matching
/// `do_ecmd` → `setfname`.
/// Upstream: `ex_docmd.c` `ex_edit`.
#[test]
fn edit_sets_buffer_name_to_path() {
    let (mut editor, mut executor) = setup();
    executor.scripts().io().insert("myfile.txt", "data");
    executor
        .execute_line(&mut editor, "edit myfile.txt")
        .unwrap();

    let current = editor.current_buffer().unwrap();
    assert_eq!(
        editor.buffer(current).unwrap().name().to_string_lossy(),
        "myfile.txt"
    );
}

/// `:write` with no file argument on an unnamed buffer raises E32
/// ("No file name"), because there is no buffer name to fall back on.
/// Upstream: `ex_docmd.c` `ex_write` — `*fname == NUL && buf->b_ffname == NULL`.
#[test]
fn write_unnamed_buffer_without_path_raises_e32() {
    let (mut editor, mut executor) = setup();
    let result = executor.execute_line(&mut editor, "write");
    assert_vim_error(result, "E32");
}

// ---------------------------------------------------------------------------
// Map / noremap / unmap / mapclear families across modes
// Citations: mapping.c:624-629,1455-1529; ex_docmd.c ex_map
// ---------------------------------------------------------------------------

/// `:nmap {lhs} {rhs}` registers a global normal-mode mapping that is
/// found by `lookup` in Normal mode.
/// Upstream: `mapping.c` `map_add` via `ex_map` with `MODE_NORMAL`.
#[test]
fn nmap_registers_normal_mode_mapping() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nmap a b").unwrap();

    match editor.mappings().lookup(b"a", MapMode::Normal, None) {
        Lookup::Exact(mapping, len) => {
            assert_eq!(len, 1);
            assert!(mapping.options.remap, "nmap should allow remap");
        }
        other => panic!("expected Exact lookup in Normal mode, got {other:?}"),
    }
}

/// `:nnoremap {lhs} {rhs}` registers a normal-mode mapping with
/// `remap = false`, so the rhs is not itself subject to mapping.
/// Upstream: `mapping.c` `map_add` with `noremap` flag.
#[test]
fn nnoremap_registers_non_remap_mapping() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap x y").unwrap();

    match editor.mappings().lookup(b"x", MapMode::Normal, None) {
        Lookup::Exact(mapping, _) => {
            assert!(
                !mapping.options.remap,
                "nnoremap should set remap = false"
            );
        }
        other => panic!("expected Exact lookup, got {other:?}"),
    }
}

/// `:nunmap {lhs}` removes a previously defined normal-mode mapping so
/// `lookup` no longer finds it.
/// Upstream: `mapping.c` `map_remove` via `ex_unmap`.
#[test]
fn nunmap_removes_mapping_from_mode() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nmap a b").unwrap();
    assert!(matches!(
        editor.mappings().lookup(b"a", MapMode::Normal, None),
        Lookup::Exact(_, _)
    ));

    executor.execute_line(&mut editor, "nunmap a").unwrap();
    assert!(matches!(
        editor.mappings().lookup(b"a", MapMode::Normal, None),
        Lookup::None
    ));
}

/// `:nmapclear` removes all normal-mode mappings in one call.
/// Upstream: `mapping.c` `map_clear` via `ex_mapclear`.
#[test]
fn nmapclear_removes_all_normal_mappings() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nmap a b").unwrap();
    executor.execute_line(&mut editor, "nmap c d").unwrap();
    assert_eq!(editor.mappings().mapping_len(), 2);

    executor.execute_line(&mut editor, "nmapclear").unwrap();
    assert_eq!(editor.mappings().mapping_len(), 0);
}

/// `:imap {lhs} {rhs}` registers a mapping visible only in Insert mode,
/// not in Normal mode.
/// Upstream: `mapping.c` `map_add` with `MODE_INSERT`.
#[test]
fn imap_registers_insert_mode_mapping() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "imap jk <Esc>").unwrap();

    assert!(matches!(
        editor.mappings().lookup(b"jk", MapMode::Insert, None),
        Lookup::Exact(_, _)
    ));
    assert!(matches!(
        editor.mappings().lookup(b"jk", MapMode::Normal, None),
        Lookup::None
    ));
}

// ---------------------------------------------------------------------------
// Augroup / autocmd registration and clearing
// Citations: autocmd.c:887-957,1865-1890; ex_docmd.c ex_autocmd
// ---------------------------------------------------------------------------

/// `:autocmd {event} {pat} {cmd}` registers one definition in the default
/// augroup, increasing the autocmd store length.
/// Upstream: `autocmd.c` `do_autocmd` → `augroup_add`.
#[test]
fn autocmd_registers_definition_in_default_group() {
    let (mut editor, mut executor) = setup();
    assert!(editor.autocmds().is_empty());

    executor
        .execute_line(&mut editor, "autocmd BufReadPost *.txt echo hi")
        .unwrap();
    assert_eq!(editor.autocmds().len(), 1);
}

/// `:augroup {name}` ... `:augroup END` scopes autocmd registration to a
/// named group. The group persists after END resets the current group.
/// Upstream: `autocmd.c` `augroup_setup` / `ex_autocmd` group tracking.
#[test]
fn augroup_create_end_scopes_registration() {
    let (mut editor, mut executor) = setup();

    executor
        .execute_line(&mut editor, "augroup MyGroup")
        .unwrap();
    executor
        .execute_line(&mut editor, "autocmd BufReadPost *.txt echo hi")
        .unwrap();
    executor
        .execute_line(&mut editor, "augroup END")
        .unwrap();

    assert_eq!(editor.autocmds().len(), 1);
    assert!(
        editor.autocmds().group("MyGroup").is_some(),
        "augroup MyGroup should exist after END"
    );
}

// ---------------------------------------------------------------------------
// :command definition / bang replacement / execution / deletion
// Citations: ex_docmd.c ex_command (E174, E183), test_usercommands.vim
// ---------------------------------------------------------------------------

/// `:command {Name} {body}` defines a user command, and invoking `{Name}`
/// executes the body — here `:echo 'hello'` pushes a message.
/// Upstream: `ex_docmd.c` `ex_command` → `uc_add`; `test_usercommands.vim`
/// defines and invokes user commands.
#[test]
fn command_defines_and_invokes_user_command() {
    let (mut editor, mut executor) = setup();
    executor
        .execute_line(&mut editor, "command MyCmd echo 'hello'")
        .unwrap();

    executor.execute_line(&mut editor, "MyCmd").unwrap();

    let last = editor.messages().last().unwrap();
    match &last.content {
        ox_types::Object::String(s) => assert_eq!(s.to_string_lossy(), "hello"),
        other => panic!("expected string message 'hello', got {other:?}"),
    }
}

/// `:command! {Name} {body}` replaces an existing user command's body.
/// Invoking after replacement executes the new body, not the old.
/// Upstream: `ex_docmd.c` `ex_command` with `forceit` → `uc_add` replace;
/// `test_usercommands.vim` tests `command!` redefinition.
#[test]
fn command_bang_replaces_existing_definition() {
    let (mut editor, mut executor) = setup();
    executor
        .execute_line(&mut editor, "command MyCmd echo 'old'")
        .unwrap();
    executor
        .execute_line(&mut editor, "command! MyCmd echo 'new'")
        .unwrap();

    executor.execute_line(&mut editor, "MyCmd").unwrap();

    let last = editor.messages().last().unwrap();
    match &last.content {
        ox_types::Object::String(s) => assert_eq!(s.to_string_lossy(), "new"),
        other => panic!("expected string message 'new', got {other:?}"),
    }
}

/// `:delcommand {Name}` removes a user command so subsequent invocation
/// fails with E492 ("not an editor command").
/// Upstream: `ex_docmd.c` `ex_delcommand` → `uc_delete`;
/// `test_usercommands.vim` tests `delcommand`.
#[test]
fn delcommand_removes_user_command() {
    let (mut editor, mut executor) = setup();
    executor
        .execute_line(&mut editor, "command MyCmd echo 'hi'")
        .unwrap();
    // Invocation works before deletion.
    executor.execute_line(&mut editor, "MyCmd").unwrap();
    assert!(!editor.messages().is_empty());

    executor
        .execute_line(&mut editor, "delcommand MyCmd")
        .unwrap();

    let result = executor.execute_line(&mut editor, "MyCmd");
    assert!(result.is_err(), "invoking deleted command should fail");
    let error = result.unwrap_err();
    assert!(
        error.to_string().contains("E492") || error.to_string().contains("not an editor command"),
        "expected E492, got: {error}"
    );
}

// ---------------------------------------------------------------------------
// Buffer-seam builtins: setline() / getline() through ox_eval::BufferHost
// Citations: src/nvim/eval/buffer.c f_setline/f_getline (set_buffer_lines,
// get_buffer_lines), runtime/doc/vimfn.txt setline()/getline().
// ---------------------------------------------------------------------------

fn buffer_text(editor: &Editor) -> Vec<String> {
    let buffer = editor.current_buffer().unwrap();
    let state = editor.buffer(buffer).unwrap();
    let text = state.text().unwrap();
    (1..=text.line_count())
        .map(|lnum| String::from_utf8_lossy(&text.line(lnum).unwrap()).into_owned())
        .collect()
}

fn global_value(executor: &ExExecutor<MemoryFileIO>, name: &str) -> Option<ox_types::Typval> {
    executor
        .scope()
        .global
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .map(|(_, value)| value.clone())
}

/// `setline()` on an existing line replaces it and reports 0 (FALSE).
/// Upstream: `eval/buffer.c` `set_buffer_lines` — `lnum <= ml_line_count`
/// takes the `ml_replace` path.
#[test]
fn setline_replaces_existing_line() {
    let (mut editor, mut executor) = setup_with_content(&[b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]);
    executor.execute_line(&mut editor, "let g:r = setline(2, 'BETA')").unwrap();
    assert_eq!(global_value(&executor, "r"), Some(ox_types::Typval::Number(0)));
    assert_eq!(buffer_text(&editor), vec!["alpha", "BETA", "gamma"]);
}

/// `setline(lnum, text)` with `lnum` just past the end appends the line.
/// Upstream: `set_buffer_lines` — the `ml_append` path at
/// `lnum == ml_line_count + 1`; further out fails with 1 and writes nothing.
#[test]
fn setline_appends_past_end_and_fails_beyond() {
    let (mut editor, mut executor) = setup_with_content(&[b"alpha".to_vec(), b"beta".to_vec()]);
    executor.execute_line(&mut editor, "let g:ok = setline(3, 'gamma')").unwrap();
    assert_eq!(global_value(&executor, "ok"), Some(ox_types::Typval::Number(0)));
    assert_eq!(buffer_text(&editor), vec!["alpha", "beta", "gamma"]);
    executor.execute_line(&mut editor, "let g:fail = setline(9, 'x')").unwrap();
    assert_eq!(global_value(&executor, "fail"), Some(ox_types::Typval::Number(1)));
    assert_eq!(buffer_text(&editor), vec!["alpha", "beta", "gamma"]);
}

/// `setline(lnum, [items])` writes the items onto consecutive lines,
/// replacing in range and appending past the end.
/// Upstream: `set_buffer_lines` list loop; builtin.txt setline().
#[test]
fn setline_list_form_replaces_and_appends() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    executor.execute_line(&mut editor, "let g:r = setline(2, ['X', 'Y', 'Z'])").unwrap();
    assert_eq!(global_value(&executor, "r"), Some(ox_types::Typval::Number(0)));
    assert_eq!(buffer_text(&editor), vec!["a", "X", "Y", "Z"]);
}

/// `:call setline(...)` reaches the same seam through `ex_call`.
#[test]
fn call_setline_mutates_buffer() {
    let (mut editor, mut executor) = setup_with_content(&[b"one".to_vec()]);
    executor.execute_line(&mut editor, "call setline(1, 'uno')").unwrap();
    assert_eq!(buffer_text(&editor), vec!["uno"]);
}

/// `getline(lnum)` returns the line as a String; `getline(start, end)`
/// returns the inclusive range as a List; out-of-range single reads are "".
/// Upstream: `eval/buffer.c` `get_buffer_lines` single/list branches.
#[test]
fn getline_single_and_range_forms() {
    let (mut editor, mut executor) = setup_with_content(&[b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
    executor.execute_line(&mut editor, "let g:one = getline(1)").unwrap();
    executor.execute_line(&mut editor, "let g:rest = getline(2, 3)").unwrap();
    executor.execute_line(&mut editor, "let g:none = getline(99)").unwrap();
    let string_of = |value: ox_types::Typval| match value {
        ox_types::Typval::String(text) => text.to_string_lossy().into_owned(),
        other => panic!("expected String, got {other:?}"),
    };
    assert_eq!(global_value(&executor, "one").map(string_of), Some("one".to_owned()));
    assert_eq!(global_value(&executor, "none").map(string_of), Some(String::new()));
    match global_value(&executor, "rest") {
        Some(ox_types::Typval::List(list)) => {
            let items = list.borrow().items.clone();
            assert_eq!(items.len(), 2);
            assert_eq!(string_of(items[0].clone()), "two");
            assert_eq!(string_of(items[1].clone()), "three");
        }
        other => panic!("expected List, got {other:?}"),
    }
}

/// setline writes survive the single-writer pipeline: the modified flag
/// flips like any other buffer mutation.
#[test]
fn setline_marks_buffer_modified() {
    let (mut editor, mut executor) = setup_with_content(&[b"saved".to_vec()]);
    let buffer = editor.current_buffer().unwrap();
    assert!(!editor.buffer(buffer).unwrap().modified);
    executor.execute_line(&mut editor, "call setline(1, 'changed')").unwrap();
    assert!(editor.buffer(buffer).unwrap().modified);
}

/// String line addresses translate per `tv_get_lnum`: `"."` is the cursor
/// line, `"'a"` the local mark, and an unset mark reads as line 0 → "".
/// Upstream: `eval/typval.c` `tv_get_lnum` → `var2fpos`.
#[test]
fn getline_string_addresses_resolve_cursor_and_marks() {
    let (mut editor, mut executor) = setup_with_content(&[
        b"one".to_vec(),
        b"two".to_vec(),
        b"three".to_vec(),
    ]);
    let buffer = editor.current_buffer().unwrap();
    editor.set_local_mark(buffer, 'a', ox_text::Position { lnum: 3, col: 1 }).unwrap();
    executor.execute_line(&mut editor, "normal! 2G").unwrap();
    executor.execute_line(&mut editor, "let g:dot = getline('.')").unwrap();
    executor.execute_line(&mut editor, "let g:mark = getline(\"'a\")").unwrap();
    executor.execute_line(&mut editor, "let g:unset = getline(\"'z\")").unwrap();
    executor.execute_line(&mut editor, "let g:range = getline('.', '$')").unwrap();
    let string_of = |value: ox_types::Typval| match value {
        ox_types::Typval::String(text) => text.to_string_lossy().into_owned(),
        other => panic!("expected String, got {other:?}"),
    };
    assert_eq!(global_value(&executor, "dot").map(string_of), Some("two".to_owned()));
    assert_eq!(global_value(&executor, "mark").map(string_of), Some("three".to_owned()));
    assert_eq!(global_value(&executor, "unset").map(string_of), Some(String::new()));
    match global_value(&executor, "range") {
        Some(ox_types::Typval::List(list)) => {
            let items = list.borrow().items.clone();
            assert_eq!(items.len(), 2);
            assert_eq!(string_of(items[0].clone()), "two");
            assert_eq!(string_of(items[1].clone()), "three");
        }
        other => panic!("expected List, got {other:?}"),
    }
}

/// `:bwipeout!` moves the displaying window onto another buffer and
/// removes the target; the modified guard fires without `!`.
/// Upstream: `ex_cmds.c` ex_bwipe/do_buffer.
#[test]
fn bwipeout_replaces_window_buffer_and_wipes() {
    let (mut editor, mut executor) = setup_with_content(&[b"first".to_vec()]);
    executor.execute_line(&mut editor, "enew").unwrap();
    executor.execute_line(&mut editor, "call setline(1, 'scratch')").unwrap();
    let target = editor.current_buffer().unwrap();
    executor.execute_line(&mut editor, "bwipeout").unwrap_err(); // E89: modified
    executor.execute_line(&mut editor, "bwipeout!").unwrap();
    assert!(editor.buffer(target).is_err());
    let current = editor.current_buffer().unwrap();
    assert_ne!(current, target);
    assert_eq!(buffer_text(&editor), vec!["first"]);
}

// ---------------------------------------------------------------------------
// :print / :p
// Citations: src/nvim/ex_docmd.c ex_print; src/nvim/ex_cmds.c print_line,
// print_line_no_prefix (numbering via 'number' + number_width).
// ---------------------------------------------------------------------------

fn echo_messages(editor: &Editor) -> Vec<String> {
    editor
        .messages()
        .iter()
        .filter(|message| message.kind == crate::MessageKind::Echo)
        .map(|message| match &message.content {
            ox_types::Object::String(text) => text.to_string_lossy().into_owned(),
            other => panic!("expected string message, got {other:?}"),
        })
        .collect()
}

/// `:{range}p` sends each addressed line to the message sink as its own
/// Echo message and leaves the cursor on the last printed line.
/// Upstream: `ex_docmd.c` `ex_print` loop over `eap->line1..eap->line2`,
/// then `curwin->w_cursor.lnum = eap->line2`.
#[test]
fn print_addrressed_lines_to_message_sink() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
    executor.execute_line(&mut editor, "2p").unwrap();
    assert_eq!(echo_messages(&editor), vec!["two"]);
    let window = editor.current_window().unwrap();
    assert_eq!(editor.window(window).unwrap().cursor.lnum, 2);
    assert_eq!(editor.window(window).unwrap().cursor.col, 0);
}

/// `:1,3print` prints the explicit inclusive range.
#[test]
fn print_explicit_range() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
    executor.execute_line(&mut editor, "1,3print").unwrap();
    assert_eq!(echo_messages(&editor), vec!["one", "two", "three"]);
}

/// `:%print` prints the whole buffer.
#[test]
fn percent_print_whole_buffer() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"one".to_vec(), b"two".to_vec()]);
    executor.execute_line(&mut editor, "%print").unwrap();
    assert_eq!(echo_messages(&editor), vec!["one", "two"]);
}

/// With 'number' set, each printed line is prefixed by its right-aligned
/// line number padded to the width of the last line number.
/// Upstream: `ex_cmds.c` `print_line_no_prefix` — `curwin->w_p_nu` and
/// `number_width(curwin)`; `msg_prt_line` appends the text.
#[test]
fn print_with_number_option_numbers_lines() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
    executor.execute_line(&mut editor, "set number").unwrap();
    executor.execute_line(&mut editor, "%p").unwrap();
    assert_eq!(echo_messages(&editor), vec!["1 one", "2 two", "3 three"]);
}

/// `:print` on an empty buffer raises E749 before printing anything.
/// Upstream: `ex_print` — `ML_EMPTY` → `e_empty_buffer`.
#[test]
fn print_empty_buffer_raises_e749() {
    let (mut editor, mut executor) = setup();
    assert_vim_error(executor.execute_line(&mut editor, "print"), "E749");
}

/// `%g/pat/p` prints every matching line through `:print` as the nested
/// default command (`:g` addresses a range like the other line commands).
/// Upstream: `ex_docmd.c` `ex_global` → default `"print"` subcommand.
#[test]
fn global_nested_print_outputs_matches() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
    executor.execute_line(&mut editor, "%g/o/p").unwrap();
    assert_eq!(echo_messages(&editor), vec!["one", "two"]);
}
