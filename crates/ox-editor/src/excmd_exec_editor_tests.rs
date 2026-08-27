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
use crate::{
    AutocmdKind, AutocmdOptions, Editor, Event, ExecError, ExecOutcome, ExExecutor, Geometry, Lookup,
    MapMode, VimExceptionKind,
};

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

    fn copy_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let content = self
            .files
            .borrow()
            .get(from)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"))?;
        self.files.borrow_mut().insert(to.to_path_buf(), content);
        Ok(())
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

#[test]
fn source_percent_expands_to_current_buffer_name() {
    let io = MemoryFileIO::new();
    io.insert("dir/current file.vim", "let g:sourced_percent = 1");
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let mut executor = ExExecutor::with_io(io);

    executor
        .execute_line(&mut editor, "edit dir/current file.vim")
        .unwrap();
    executor.execute_line(&mut editor, "source %").unwrap();

    assert_eq!(
        executor
            .scope()
            .global
            .iter()
            .find(|(name, _)| name.as_bytes() == b"sourced_percent")
            .map(|(_, value)| value),
        Some(&ox_types::Typval::Number(1))
    );
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

#[test]
fn new_creates_second_window_with_fresh_buffer() {
    let (mut editor, mut executor) = setup();
    let original = editor.current_buffer().unwrap();

    executor.execute_line(&mut editor, "new").unwrap();

    assert_eq!(editor.windows().len(), 2);
    assert_ne!(editor.current_buffer(), Some(original));
    assert!(editor
        .buffer(editor.current_buffer().unwrap())
        .unwrap()
        .name()
        .as_bytes()
        .is_empty());
    let current = editor.current_buffer();

    executor.execute_line(&mut editor, "only").unwrap();

    assert_eq!(editor.windows().len(), 1);
    assert_eq!(editor.current_buffer(), current);
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

#[test]
fn resize_wincmd_and_echohl_mutate_existing_editor_state() {
    let (mut editor, mut executor) = setup();
    let original = editor.current_window().unwrap();
    executor.execute_line(&mut editor, "split").unwrap();
    let split = editor.current_window().unwrap();
    assert_ne!(split, original);

    executor.execute_line(&mut editor, "resize 2").unwrap();
    assert_eq!(editor.window_geometry(split).unwrap().height, 2);
    executor.execute_line(&mut editor, "wincmd w").unwrap();
    assert_eq!(editor.current_window(), Some(original));
    executor.execute_line(&mut editor, "echohl Search").unwrap();
    executor.execute_line(&mut editor, "echo 'visible'").unwrap();
    executor.execute_line(&mut editor, "echohl None").unwrap();
    assert!(editor.messages().iter().any(|message| {
        matches!(&message.content, ox_types::Object::String(text) if text.as_bytes() == b"visible")
    }));
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

#[test]
fn autocmd_bang_clears_builtin_popupmenu_group() {
    let (mut editor, mut executor) = setup();
    let group = editor.autocmds().group("nvim.popupmenu").unwrap();
    editor
        .autocmds_mut()
        .register(
            Event::BufEnter,
            "*",
            AutocmdKind::ExString("echo stale".to_owned()),
            AutocmdOptions { group, ..AutocmdOptions::default() },
        )
        .unwrap();

    executor.execute_line(&mut editor, "autocmd! nvim.popupmenu").unwrap();

    assert!(editor.autocmds().is_empty());
    assert_eq!(editor.autocmds().group("nvim.popupmenu"), Some(group));
}

// ---------------------------------------------------------------------------
// The exit sequence: VimLeavePre then VimLeave
// Citations: main.c getout:753-882 (VimLeavePre at 828, VimLeave at 851),
// test/old/testdir/runtest.vim:324 `au VimLeavePre * call EarlyExit(...)`.
// ---------------------------------------------------------------------------

/// A command that ends the process runs `VimLeavePre` and then `VimLeave`
/// before it goes, which is how `runtest.vim` keeps its record when a test
/// function quits: `au VimLeavePre * call EarlyExit(g:testfunc)`.
///
/// The order is asserted rather than just the firing, because upstream
/// separates the two events by the ShaDa write and handlers rely on running
/// before it (`main.c`:828 and :851).
#[test]
fn quit_runs_vimleavepre_then_vimleave() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor
        .execute_line(&mut editor, "autocmd VimLeavePre * let g:pre = 1")
        .unwrap();
    // Reading g:pre here is what pins the order: VimLeave first would leave
    // g:leave unset, since g:pre would not exist yet.
    executor
        .execute_line(&mut editor, "autocmd VimLeave * let g:leave = g:pre + 1")
        .unwrap();
    assert_eq!(
        executor.execute_line(&mut editor, "quit").unwrap(),
        ExecOutcome::Quit(0)
    );
    assert_eq!(global_value(&executor, "pre"), Some(ox_types::Typval::Number(1)));
    assert_eq!(global_value(&executor, "leave"), Some(ox_types::Typval::Number(2)));
}

/// `:qall` and `:cquit` are the same exit, so they carry the same events, and
/// the sequence runs once per process however many quits follow.
#[test]
fn the_exit_sequence_runs_once_for_qall_and_cquit() {
    for command in ["qall", "cquit 3"] {
        let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
        executor
            .execute_line(&mut editor, "autocmd VimLeave * let g:count = get(g:, 'count', 0) + 1")
            .unwrap();
        executor.execute_line(&mut editor, command).unwrap();
        assert_eq!(global_value(&executor, "count"), Some(ox_types::Typval::Number(1)), "{command}");
        // A second quit is past `getout`, so nothing fires again.
        executor.execute_line(&mut editor, "qall").unwrap();
        assert_eq!(global_value(&executor, "count"), Some(ox_types::Typval::Number(1)), "{command}");
    }
}

/// A quit reached from inside a sourced script still runs the sequence: this is
/// `runtest.vim`'s own shape, where the `quit` is inside a function called by a
/// sourced file.
#[test]
fn a_quit_inside_a_sourced_script_runs_the_exit_sequence() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor
        .execute_line(&mut editor, "autocmd VimLeavePre * let g:early = 1")
        .unwrap();
    let script = "func Ender()\n  quit\nendfunc\ncall Ender()\n";
    assert_eq!(
        executor.execute_script(&mut editor, "ender.vim", script).unwrap(),
        ExecOutcome::Quit(0)
    );
    assert!(global_flag(&executor, "early"));
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

// ---------------------------------------------------------------------------
// :redraw / :redrawstatus / :redrawtabline
// Citations: ex_docmd.c ex_redraw/ex_redrawstatus/ex_redrawtabline,
// cursor.c:310-323 check_cursor_lnum.
// ---------------------------------------------------------------------------

/// `:redraw` completes and, through `ex_redraw`'s `validate_cursor` call,
/// clamps a cursor left past the end of the buffer onto the last line.
/// Upstream: `ex_docmd.c` `ex_redraw` → `validate_cursor` →
/// `check_cursor_lnum` (`cursor.c:310-323`).
#[test]
fn redraw_clamps_cursor_past_end_of_buffer() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"one".to_vec(), b"two".to_vec()]);
    let window = editor.current_window().unwrap();
    editor
        .set_window_cursor(window, ox_text::Position { lnum: 99, col: 2 })
        .unwrap();
    executor.execute_line(&mut editor, "redraw").unwrap();
    assert_eq!(editor.window(window).unwrap().cursor.lnum, 2);
    assert_eq!(editor.window(window).unwrap().cursor.col, 2);
}

/// `:redr` is the shortest abbreviation of `:redraw`; `:red` is `:redo`
/// and `:redi` is `:redir`, so the abbreviation must not shift.
/// Upstream: `ex_cmds.lua` table order redo/redir/redraw.
#[test]
fn redraw_abbreviation_and_bang_leave_a_valid_cursor_alone() {
    let (mut editor, mut executor) = setup_with_content(&[b"one".to_vec()]);
    let window = editor.current_window().unwrap();
    executor.execute_line(&mut editor, "redr").unwrap();
    executor.execute_line(&mut editor, "redraw!").unwrap();
    executor.execute_line(&mut editor, "redrawstatus").unwrap();
    executor.execute_line(&mut editor, "redrawt").unwrap();
    assert_eq!(editor.window(window).unwrap().cursor.lnum, 1);
}

/// `:redrawtabline` takes no bang (`ex_cmds.lua` omits BANG), and a
/// disallowed bang is upstream's `e_nobang`: E477, not a trailing-characters
/// error.
///
/// Oracle: `redrawtabline!` → `Vim(redrawtabline):E477: No ! allowed`.
#[test]
fn redrawtabline_rejects_a_bang_with_e477() {
    let (mut editor, mut executor) = setup();
    assert_vim_error(executor.execute_line(&mut editor, "redrawtabline!"), "E477");
}

// ---------------------------------------------------------------------------
// :filetype
// Citations: ex_docmd.c ex_filetype:7886-7949, globals.h:37-60 file names,
// runtime.c do_in_path:430-515.
// ---------------------------------------------------------------------------

/// Installs one runtime root holding the six `:filetype` scripts, each
/// recording that it ran in a distinct global.
fn setup_filetype() -> (Editor, ExExecutor<MemoryFileIO>) {
    let (editor, mut executor) = setup();
    let io = executor.scripts().io().clone();
    io.insert("rt/filetype.vim", "let g:ran_filetype = 1");
    io.insert("rt/ftplugin.vim", "let g:ran_ftplugin = 1");
    io.insert("rt/indent.vim", "let g:ran_indent = 1");
    io.insert("rt/ftoff.vim", "let g:ran_ftoff = 1");
    io.insert("rt/ftplugof.vim", "let g:ran_ftplugof = 1");
    io.insert("rt/indoff.vim", "let g:ran_indoff = 1");
    executor.scripts_mut().add_runtime_root(PathBuf::from("rt"));
    (editor, executor)
}

fn global_flag(executor: &ExExecutor<MemoryFileIO>, name: &str) -> bool {
    global_value(executor, name) == Some(ox_types::Typval::Number(1))
}

/// Bare `:filetype` reports the three enablement states in upstream's
/// exact wording, and starts out entirely off.
/// Upstream: `ex_filetype` — `smsg(0, "filetype detection:%s  plugin:%s  indent:%s", ...)`.
#[test]
fn filetype_reports_state() {
    let (mut editor, mut executor) = setup_filetype();
    executor.execute_line(&mut editor, "filetype").unwrap();
    assert_eq!(
        echo_messages(&editor),
        vec!["filetype detection:OFF  plugin:OFF  indent:OFF"]
    );
}

/// `:filetype plugin indent on` sources filetype, ftplugin, and indent
/// from 'runtimepath' and flips all three states on; the report then shows
/// `ON` for each.
#[test]
fn filetype_plugin_indent_on_sources_all_three() {
    let (mut editor, mut executor) = setup_filetype();
    executor
        .execute_line(&mut editor, "filetype plugin indent on")
        .unwrap();
    assert!(global_flag(&executor, "ran_filetype"));
    assert!(global_flag(&executor, "ran_ftplugin"));
    assert!(global_flag(&executor, "ran_indent"));
    editor.truncate_messages(0);
    executor.execute_line(&mut editor, "filetype").unwrap();
    assert_eq!(
        echo_messages(&editor),
        vec!["filetype detection:ON  plugin:ON  indent:ON"]
    );
}

/// `:filetype plugin on` without detection enabled reports `(on)` for the
/// plugin column, upstream's marker for "requested but detection is off".
/// Reached by turning detection off again after enabling the plugin part.
#[test]
fn filetype_plugin_without_detection_reports_parenthesised_on() {
    let (mut editor, mut executor) = setup_filetype();
    executor
        .execute_line(&mut editor, "filetype plugin on")
        .unwrap();
    executor.execute_line(&mut editor, "filetype off").unwrap();
    assert!(global_flag(&executor, "ran_ftoff"));
    editor.truncate_messages(0);
    executor.execute_line(&mut editor, "filetype").unwrap();
    assert_eq!(
        echo_messages(&editor),
        vec!["filetype detection:OFF  plugin:(on)  indent:OFF"]
    );
}

/// `:filetype indent off` sources only `indoff.vim` and leaves detection
/// alone, unlike the bare `:filetype off` which sources `ftoff.vim`.
#[test]
fn filetype_indent_off_sources_only_indoff() {
    let (mut editor, mut executor) = setup_filetype();
    executor
        .execute_line(&mut editor, "filetype indent off")
        .unwrap();
    assert!(global_flag(&executor, "ran_indoff"));
    assert!(!global_flag(&executor, "ran_ftoff"));
}

/// `:filet` is the shortest abbreviation of `:filetype` (`:filte`/`:filt`
/// belong to `:filter`), and it drives the same command.
#[test]
fn filetype_abbreviation_sources_filetype_script() {
    let (mut editor, mut executor) = setup_filetype();
    executor.execute_line(&mut editor, "filet on").unwrap();
    assert!(global_flag(&executor, "ran_filetype"));
}

/// `:filetype detect` re-fires the `filetypedetect` group's BufRead
/// autocommands, and only that group's.
/// Upstream: `ex_filetype` — `do_doautocmd("filetypedetect BufRead", true, NULL)`.
#[test]
fn filetype_detect_refires_the_filetypedetect_group() {
    let (mut editor, mut executor) = setup_filetype();
    executor
        .execute_line(&mut editor, "augroup filetypedetect")
        .unwrap();
    executor
        .execute_line(&mut editor, "autocmd BufRead * let g:detected = 1")
        .unwrap();
    executor.execute_line(&mut editor, "augroup END").unwrap();
    executor
        .execute_line(&mut editor, "autocmd BufRead * let g:other = 1")
        .unwrap();
    executor.execute_line(&mut editor, "filetype detect").unwrap();
    assert!(global_flag(&executor, "detected"));
    assert!(!global_flag(&executor, "other"));
}

/// An argument that is neither `on`, `off`, nor `detect` raises E475 with
/// the offending text.
/// Upstream: `ex_filetype` — `semsg(_(e_invarg2), arg)`, `errors.h:34`.
#[test]
fn filetype_rejects_unknown_argument_with_e475() {
    let (mut editor, mut executor) = setup_filetype();
    assert_vim_error(executor.execute_line(&mut editor, "filetype nope"), "E475");
}

// ---------------------------------------------------------------------------
// :read / :read !cmd / :write !cmd
// Citations: ex_docmd.c ex_read:6163-6195, ex_cmds.c do_filter:1430-1436,
// ex_docmd.c:2256-2275 usefilter.
// ---------------------------------------------------------------------------

/// `:2read {file}` inserts the file after line 2 and leaves the cursor on
/// the first inserted line, at its first non-blank column.
/// Oracle: `nvim --headless` on `['a','b','c']` + `2read` → a b x y c,
/// cursor line 3.
#[test]
fn read_inserts_file_after_addressed_line() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    executor.scripts().io().insert("in.txt", "x\ny\n");
    executor.execute_line(&mut editor, "2read in.txt").unwrap();
    assert_eq!(buffer_text(&editor), vec!["a", "b", "x", "y", "c"]);
    let window = editor.current_window().unwrap();
    assert_eq!(editor.window(window).unwrap().cursor.lnum, 3);
}

/// `:0read {file}` prepends, which only works because `read` carries ZEROR
/// and line 0 survives address resolution.
/// Oracle: `['a','b']` + `0read` → x y a b, cursor line 1.
#[test]
fn read_zero_address_prepends() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec(), b"b".to_vec()]);
    executor.scripts().io().insert("in.txt", "x\ny\n");
    executor.execute_line(&mut editor, "0read in.txt").unwrap();
    assert_eq!(buffer_text(&editor), vec!["x", "y", "a", "b"]);
    let window = editor.current_window().unwrap();
    assert_eq!(editor.window(window).unwrap().cursor.lnum, 1);
}

/// `:r` is the shortest abbreviation of `:read` and reads the same file.
#[test]
fn read_abbreviation_inserts_file() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.scripts().io().insert("in.txt", "x\n");
    executor.execute_line(&mut editor, "r in.txt").unwrap();
    assert_eq!(buffer_text(&editor), vec!["a", "x"]);
}

/// A cursor column lands on the first non-blank of the inserted line
/// (`beginline(BL_WHITE | BL_FIX)`).
/// Oracle: `1read` of "    indented" leaves cursor col 5 (one-based).
#[test]
fn read_cursor_lands_on_first_non_blank() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor
        .scripts()
        .io()
        .insert("in.txt", "    indented\nsecond\n");
    executor.execute_line(&mut editor, "1read in.txt").unwrap();
    let window = editor.current_window().unwrap();
    assert_eq!(editor.window(window).unwrap().cursor.lnum, 2);
    assert_eq!(editor.window(window).unwrap().cursor.col, 4);
}

/// An unreadable file raises E484.
/// Oracle: `read nosuchfile` → `Vim(read):E484: Can't open file nosuchfile`.
#[test]
fn read_missing_file_raises_e484() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    assert_vim_error(
        executor.execute_line(&mut editor, "read nosuchfile"),
        "E484",
    );
}

/// Bare `:read` in a buffer with no name raises E32.
/// Oracle: `enew | read` → `Vim(read):E32: No file name`.
#[test]
fn read_without_argument_or_name_raises_e32() {
    let (mut editor, mut executor) = setup();
    assert_vim_error(executor.execute_line(&mut editor, "read"), "E32");
}

/// `:read !cmd` inserts the command's standard output after the addressed
/// line and leaves the cursor on the *last* inserted line, unlike the file
/// form. Upstream: `do_filter`:1430-1433 "Put cursor on last new line".
#[test]
fn read_filter_inserts_command_output_and_lands_on_last_line() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec(), b"b".to_vec()]);
    executor
        .execute_line(&mut editor, "1read !printf 'p\\nq\\n'")
        .unwrap();
    assert_eq!(buffer_text(&editor), vec!["a", "p", "q", "b"]);
    let window = editor.current_window().unwrap();
    assert_eq!(editor.window(window).unwrap().cursor.lnum, 3);
}

/// A `|` inside `:read !cmd` belongs to the shell, not to the Ex parser, so
/// the whole pipeline runs as one command.
#[test]
fn read_filter_keeps_the_shell_pipeline() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor
        .execute_line(&mut editor, "1read !printf 'z\\n' | tr z Z")
        .unwrap();
    assert_eq!(buffer_text(&editor), vec!["a", "Z"]);
}

/// A failing filter publishes its exit status in `v:shell_error`.
#[test]
fn read_filter_publishes_shell_error() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "1read !exit 3").unwrap();
    assert_eq!(
        executor
            .scope()
            .vim
            .iter()
            .find(|(name, _)| name.as_bytes() == b"shell_error")
            .map(|(_, value)| value),
        Some(&ox_types::Typval::Number(3))
    );
}

/// `:write !cmd` pipes the addressed lines into the command instead of
/// writing a file named after it, and leaves the buffer alone. The default
/// range is the whole buffer (EX_DFLALL).
#[test]
fn write_filter_pipes_lines_into_the_command() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec(), b"b".to_vec()]);
    let path = std::env::temp_dir().join(format!("oxvim-write-filter-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    executor
        .execute_line(
            &mut editor,
            &format!("write !cat > {}", path.to_string_lossy()),
        )
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
    let _ = std::fs::remove_file(&path);
    // The buffer keeps its (empty) name: no file called "cat > ..." is made.
    let buffer = editor.current_buffer().unwrap();
    assert_eq!(editor.buffer(buffer).unwrap().name().to_string_lossy(), "");
}

// ---------------------------------------------------------------------------
// Address-domain validation: invalid_range, ex_docmd.c:3735-3820.
// ---------------------------------------------------------------------------

/// An ADDR_LINES address past the last line is rejected, not clamped onto the
/// last line, so the buffer is left untouched.
///
/// Oracle: `['a','b','c']` + `99read in.txt` →
/// `Vim(read):E16: Invalid range: 99read in.txt`, buffer still `a b c`.
#[test]
fn out_of_range_address_raises_e16_without_mutating_the_buffer() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    executor.scripts().io().insert("in.txt", "x\ny\n");
    assert_vim_error(executor.execute_line(&mut editor, "99read in.txt"), "E16");
    assert_eq!(buffer_text(&editor), vec!["a", "b", "c"]);
}

/// The rule lives at the dispatch entry, so every ADDR_LINES command gets it,
/// not just `:read`.
///
/// Oracle: `99print` → `Vim(print):E16: Invalid range: 99print`;
/// `5,6delete` → `Vim(delete):E16: Invalid range: 5,6delete`.
#[test]
fn out_of_range_address_raises_e16_for_every_line_addressed_command() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    assert_vim_error(executor.execute_line(&mut editor, "99print"), "E16");
    assert_vim_error(executor.execute_line(&mut editor, "5,6delete"), "E16");
    assert_vim_error(executor.execute_line(&mut editor, "2,9yank"), "E16");
    assert_eq!(buffer_text(&editor), vec!["a", "b", "c"]);
}

/// The last line itself is in range, and `:0read`'s ZEROR line 0 still
/// resolves: the check bounds the upper end only.
#[test]
fn in_range_addresses_survive_the_domain_check() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    executor.scripts().io().insert("in.txt", "x\n");
    executor.execute_line(&mut editor, "3read in.txt").unwrap();
    executor.execute_line(&mut editor, "0read in.txt").unwrap();
    assert_eq!(buffer_text(&editor), vec!["x", "a", "b", "c", "x"]);
}

/// Each address domain gets its own limit, exactly as `invalid_range` bounds
/// them, and `ADDR_OTHER` stays unbounded.
///
/// Oracle, on a three-line buffer with one window and one buffer:
/// `99resize` → no error (ADDR_OTHER, so the address is never checked);
/// `99close` → `Vim(close):E16: Invalid range: 99close` (ADDR_WINDOWS);
/// `99buffer` → `Vim(buffer):E16: Invalid range: 99buffer` (ADDR_BUFFERS).
#[test]
fn each_address_domain_gets_its_own_limit() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    // ADDR_OTHER: unbounded, so the address never reaches the domain check.
    // `:resize` still fails on its own screen-extent limit, which is E36, not
    // the E16 this test is about.
    executor.execute_line(&mut editor, "99bnext").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "99close"), "E16");
    assert_vim_error(executor.execute_line(&mut editor, "99buffer"), "E16");
}

// ---------------------------------------------------------------------------
// :read carries readfile()'s autocommands.
// Citations: fileio.c:336-340 FileReadCmd, fileio.c:631,640 the Pre events,
// fileio.c:1914,1925 the Post events, ex_cmds.c:1236 ShellFilterPost.
// ---------------------------------------------------------------------------

/// `:read {file}` fires `FileReadPre` before the lines land and
/// `FileReadPost` after, both matched against the file name rather than the
/// buffer name.
#[test]
fn read_file_fires_the_file_read_events() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.scripts().io().insert("in.txt", "x\n");
    executor
        .execute_line(&mut editor, "autocmd FileReadPre *.txt let g:pre = line('$')")
        .unwrap();
    executor
        .execute_line(&mut editor, "autocmd FileReadPost *.txt let g:post = line('$')")
        .unwrap();
    executor.execute_line(&mut editor, "1read in.txt").unwrap();
    assert_eq!(buffer_text(&editor), vec!["a", "x"]);
    // Pre runs while the buffer is still one line, Post once it is two: the
    // events straddle the insert instead of both landing on one side.
    assert_eq!(global_value(&executor, "pre"), Some(ox_types::Typval::Number(1)));
    assert_eq!(global_value(&executor, "post"), Some(ox_types::Typval::Number(2)));
}

/// A matching `FileReadCmd` definition replaces the read: the command does
/// none of its own work, so the file's contents never reach the buffer
/// (`fileio.c:336-340`).
#[test]
fn read_file_read_cmd_replaces_the_read() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.scripts().io().insert("in.txt", "x\n");
    executor
        .execute_line(&mut editor, "autocmd FileReadCmd *.txt let g:intercepted = 1")
        .unwrap();
    executor
        .execute_line(&mut editor, "autocmd FileReadPre *.txt let g:unexpected_pre = 1")
        .unwrap();
    executor.execute_line(&mut editor, "1read in.txt").unwrap();
    assert!(global_flag(&executor, "intercepted"));
    // Interception happens before FileReadPre and before the insert.
    assert!(!global_flag(&executor, "unexpected_pre"));
    assert_eq!(buffer_text(&editor), vec!["a"]);
}

/// `:read !cmd` reads with `READ_FILTER`, so it fires the `FilterRead`
/// events rather than the `FileRead` ones, and `do_bang` adds
/// `ShellFilterPost` on the way out.
#[test]
fn read_filter_fires_the_filter_and_shell_events() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor
        .execute_line(&mut editor, "autocmd FilterReadPre * let g:pre = line('$')")
        .unwrap();
    executor
        .execute_line(&mut editor, "autocmd FilterReadPost * let g:post = line('$')")
        .unwrap();
    executor
        .execute_line(&mut editor, "autocmd ShellFilterPost * let g:shell = 1")
        .unwrap();
    executor
        .execute_line(&mut editor, "autocmd FileReadPost * let g:unexpected_file = 1")
        .unwrap();
    executor
        .execute_line(&mut editor, "1read !printf 'p\\n'")
        .unwrap();
    assert_eq!(buffer_text(&editor), vec!["a", "p"]);
    assert_eq!(global_value(&executor, "pre"), Some(ox_types::Typval::Number(1)));
    assert_eq!(global_value(&executor, "post"), Some(ox_types::Typval::Number(2)));
    assert!(global_flag(&executor, "shell"));
    // The filter form is not a file read.
    assert!(!global_flag(&executor, "unexpected_file"));
}

/// `:write !cmd` reads nothing back, so it fires `ShellFilterPost` and no
/// `FilterRead*` events.
#[test]
fn write_filter_fires_shell_filter_post_only() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor
        .execute_line(&mut editor, "autocmd ShellFilterPost * let g:shell = 1")
        .unwrap();
    executor
        .execute_line(&mut editor, "autocmd FilterReadPost * let g:unexpected_read = 1")
        .unwrap();
    executor.execute_line(&mut editor, "write !cat >/dev/null").unwrap();
    assert!(global_flag(&executor, "shell"));
    assert!(!global_flag(&executor, "unexpected_read"));
}

// ---------------------------------------------------------------------------
// :tabnew / :tabedit / :tabonly / :vnew
// Citations: ex_docmd.c ex_splitview:5637, ex_tabonly:5238,
// get_tabpage_arg:4398, window.c win_new_tabpage:4484.
// ---------------------------------------------------------------------------

fn tab_count(editor: &Editor) -> usize {
    editor.tabpages().len()
}

fn current_tab_index(editor: &Editor) -> usize {
    editor
        .current_tabpage()
        .and_then(|tab| editor.tabpage_index(tab))
        .unwrap_or(0)
}

/// `:tabnew` opens a tabpage after the current one, showing a new empty
/// buffer, and makes it current.
///
/// Oracle: from one tabpage, `tabnew` twice gives tabs=3 cur=3 and
/// `bufname('%')` is empty in the new tabpage.
#[test]
fn tabnew_opens_a_tabpage_after_the_current_one() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "tabnew").unwrap();
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (2, 2));
    executor.execute_line(&mut editor, "tabnew").unwrap();
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (3, 3));
    let buffer = editor.current_buffer().unwrap();
    assert_eq!(editor.buffer(buffer).unwrap().name().to_string_lossy(), "");
}

/// `:quit` on the last window of a *non-last* tabpage closes the tabpage; it
/// neither refuses with E444 nor exits the editor.
///
/// `win_close` (`window.c`:2798) reserves E444 for `last_window`, which is one
/// window in the current tabpage *and* one tabpage in the editor. Anything
/// else routes into `close_last_window_tabpage` (`window.c`:2678-2725).
///
/// Oracle, after `tabnew` twice: `quit!` gives tabs=2 cur=2, another gives
/// tabs=1 cur=1, and neither reports an error; the third exits.
#[test]
fn quit_on_the_last_window_of_a_tabpage_closes_the_tabpage() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "tabnew").unwrap();
    assert_eq!(
        executor.execute_line(&mut editor, "quit!").unwrap(),
        ExecOutcome::Completed
    );
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (2, 2));
    assert_eq!(
        executor.execute_line(&mut editor, "quit!").unwrap(),
        ExecOutcome::Completed
    );
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (1, 1));
    // Only now is this the last window of the last tabpage.
    assert_eq!(
        executor.execute_line(&mut editor, "quit!").unwrap(),
        ExecOutcome::Quit(0)
    );
}

/// `:close` takes the same tabpage path, and E444 survives only for the last
/// window of the last tabpage. `alt_tabpage` (`window.c`:3719) enters the
/// *next* tabpage when one follows, so closing the first of three lands on the
/// tabpage that was second.
#[test]
fn close_of_a_tabpages_last_window_enters_the_alternate_tabpage() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "tabnew").unwrap();
    // `:0tabnew` makes the new tabpage both the first and the current one.
    executor.execute_line(&mut editor, "0tabnew").unwrap();
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (4, 1));
    let survivor = editor.tabpages()[1];
    executor.execute_line(&mut editor, "close!").unwrap();
    assert_eq!(tab_count(&editor), 3);
    assert_eq!(editor.current_tabpage(), Some(survivor));
    executor.execute_line(&mut editor, "tabonly").unwrap();
    assert_eq!(tab_count(&editor), 1);
    assert_vim_error(executor.execute_line(&mut editor, "close!"), "E444");
}

/// The address is upstream's `win_new_tabpage(after)` position, so `:0tabnew`
/// becomes the first tabpage, `:$tabnew` the last, and `:{n}tabnew` lands at
/// position `n + 1`.
///
/// Oracle, from three tabpages with the third current: `0tabnew` → tabs=4
/// cur=1; a bare `tabnew` from there → tabs=5 cur=2; `$tabnew` → tabs=6
/// cur=6; `2tabnew` → tabs=7 cur=3.
#[test]
fn tabnew_honors_the_addressed_position() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "0tabnew").unwrap();
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (4, 1));
    // Now current is tab 1 of 4, so an addressless :tabnew lands at 2.
    executor.execute_line(&mut editor, "tabnew").unwrap();
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (5, 2));
    executor.execute_line(&mut editor, "$tabnew").unwrap();
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (6, 6));
    executor.execute_line(&mut editor, "2tabnew").unwrap();
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (7, 3));
}

/// `$` in an ADDR_TABS address is the last *tabpage*, not the last buffer
/// line: `get_address` resolves it per `addr_type` (`ex_docmd.c:3435-3463`).
/// With a one-line buffer and three tabpages the two readings differ, so this
/// pins the domain rather than a coincidence.
#[test]
fn tab_addresses_resolve_in_the_tabpage_domain() {
    let (mut editor, mut executor) = setup_with_content(&[b"only line".to_vec()]);
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "tabnew").unwrap();
    assert_eq!(tab_count(&editor), 3);
    executor.execute_line(&mut editor, "$tabnew").unwrap();
    // Reading `$` as the buffer's last line (1) would have inserted at 2.
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (4, 4));
}

/// `:tabedit {file}` opens the file in a new tabpage, and `:tabe` is its
/// abbreviation.
#[test]
fn tabedit_opens_a_file_in_a_new_tabpage() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.scripts().io().insert("in.txt", "filetext\n");
    executor.execute_line(&mut editor, "tabe in.txt").unwrap();
    assert_eq!(tab_count(&editor), 2);
    assert_eq!(buffer_text(&editor), vec!["filetext"]);
    let buffer = editor.current_buffer().unwrap();
    assert_eq!(editor.buffer(buffer).unwrap().name().to_string_lossy(), "in.txt");
}

/// `:tabonly` keeps the current tabpage and closes the rest; `:tabo` is its
/// abbreviation. A single tabpage is a message, not an error
/// (`ex_docmd.c:5241`).
#[test]
fn tabonly_closes_every_other_tabpage() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "tabnew").unwrap();
    assert_eq!(tab_count(&editor), 3);
    executor.execute_line(&mut editor, "tabo").unwrap();
    assert_eq!((tab_count(&editor), current_tab_index(&editor)), (1, 1));
    executor.execute_line(&mut editor, "tabonly").unwrap();
    assert_eq!(echo_messages(&editor).last().map(String::as_str), Some("Already only one tab page"));
}

/// `:tabonly {n}` keeps tabpage `n` instead of the current one, which is
/// `get_tabpage_arg`'s numeric form.
#[test]
fn tabonly_argument_selects_the_surviving_tabpage() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    let first = editor.current_tabpage().unwrap();
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "tabonly 1").unwrap();
    assert_eq!(tab_count(&editor), 1);
    assert_eq!(editor.current_tabpage(), Some(first));
}

/// A non-numeric `:tabonly` argument is E475 and closes nothing.
///
/// Oracle: `tabonly xyz` → `Vim(tabonly):E475: Invalid argument: xyz`, with
/// the tabpage count unchanged.
#[test]
fn tabonly_rejects_a_non_numeric_argument_with_e475() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "tabnew").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "tabonly xyz"), "E475");
    assert_eq!(tab_count(&editor), 3);
}

/// An out-of-domain tabpage address is E16, from the shared ADDR_TABS bound,
/// and is distinct from the E475 an out-of-range *argument* gets.
#[test]
fn tabonly_rejects_an_out_of_range_address_with_e16() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "tabnew").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "99tabonly"), "E16");
    assert_vim_error(executor.execute_line(&mut editor, "tabonly 99"), "E475");
    assert_eq!(tab_count(&editor), 2);
}

/// `:vnew` splits vertically onto a new empty buffer, unlike `:vsplit` which
/// keeps showing the current one. `:vne` is its abbreviation.
#[test]
fn vnew_splits_onto_a_new_empty_buffer() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    let original = editor.current_buffer().unwrap();
    let before = editor.windows().len();
    executor.execute_line(&mut editor, "vne").unwrap();
    assert_eq!(editor.windows().len(), before + 1);
    let created = editor.current_buffer().unwrap();
    assert_ne!(created, original);
    assert_eq!(editor.buffer(created).unwrap().name().to_string_lossy(), "");
    // :vsplit, by contrast, keeps the current buffer.
    executor.execute_line(&mut editor, "vsplit").unwrap();
    assert_eq!(editor.current_buffer(), Some(created));
}

// ---------------------------------------------------------------------------
// :undo / :redo
// Citations: ex_docmd.c ex_undo:6729, ex_redo:6783, undo.c undo_time:1975,
// u_doit:1899 (the "Already at ..." messages), set_cmd_count:1372.
// ---------------------------------------------------------------------------

/// `:undo` steps back one change and `:redo` forward again; `:u` is the
/// shortest abbreviation of `:undo`.
///
/// Oracle, on a buffer built with one edit: `undo` empties it, `redo`
/// restores it, `u` empties it again.
#[test]
fn undo_and_redo_step_one_change() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "call setline(1, 'x')").unwrap();
    assert_eq!(buffer_text(&editor), vec!["x"]);
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(buffer_text(&editor), vec!["a"]);
    executor.execute_line(&mut editor, "redo").unwrap();
    assert_eq!(buffer_text(&editor), vec!["x"]);
    executor.execute_line(&mut editor, "u").unwrap();
    assert_eq!(buffer_text(&editor), vec!["a"]);
}

/// An address on `:undo` is a *sequence number*, not a step count, and the
/// `COUNT` and `RANGE` spellings mean the same thing because `set_cmd_count`
/// folds both into `line2`. Seeking can therefore move forward.
///
/// Oracle: after one edit, `undo 0` returns to the original state and
/// `undo 1` / `1undo` both return to the edited one.
#[test]
fn undo_with_an_address_seeks_a_sequence() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "call setline(1, 'x')").unwrap();
    executor.execute_line(&mut editor, "undo 0").unwrap();
    assert_eq!(buffer_text(&editor), vec!["a"]);
    // Forward, from sequence 0 back up to 1: not something a run of undos
    // could do.
    executor.execute_line(&mut editor, "undo 1").unwrap();
    assert_eq!(buffer_text(&editor), vec!["x"]);
    executor.execute_line(&mut editor, "undo 0").unwrap();
    executor.execute_line(&mut editor, "1undo").unwrap();
    assert_eq!(buffer_text(&editor), vec!["x"]);
}

/// A sequence that does not exist is E830 and changes nothing.
///
/// Oracle: `undo 99` → `Vim(undo):E830: Undo number 99 not found`.
#[test]
fn undo_with_an_unknown_sequence_raises_e830() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "call setline(1, 'x')").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "undo 99"), "E830");
    assert_eq!(buffer_text(&editor), vec!["x"]);
}

/// Running out of history is a message, not an error (`undo.c:1935,1948`).
#[test]
fn undo_and_redo_report_the_ends_of_the_history() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(
        echo_messages(&editor).last().map(String::as_str),
        Some("Already at oldest change")
    );
    executor.execute_line(&mut editor, "redo").unwrap();
    assert_eq!(
        echo_messages(&editor).last().map(String::as_str),
        Some("Already at newest change")
    );
}

/// `:redo` takes no count of any kind: its table entry carries neither RANGE
/// nor COUNT, so an address is E481 rather than three redos. `:red` is its
/// abbreviation.
///
/// Oracle: `3redo` → `Vim(redo):E481: No range allowed`.
#[test]
fn redo_rejects_a_count_with_e481() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "call setline(1, 'x')").unwrap();
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "3redo"), "E481");
    // Still undone: the rejected command did nothing.
    assert_eq!(buffer_text(&editor), vec!["a"]);
    executor.execute_line(&mut editor, "red").unwrap();
    assert_eq!(buffer_text(&editor), vec!["x"]);
}

// ---------------------------------------------------------------------------
// Undo-block grouping.
// Citations: undo.c u_savecommon:388-500,616 (new header only when synced),
// u_sync:2704-2717, ex_undojoin:2800-2816, u_undoredo:1665,
// input.c may_sync_undo:1300-1306; eval/funcs.c f_changenr:604-607;
// undo.c f_undotree:3243-3263, u_eval_tree:3193-3221.
// ---------------------------------------------------------------------------

/// Everything a single command does is one undo block, so one `:undo` puts
/// every line back.
///
/// Oracle (`nvim -u NONE --headless`, three-line file):
/// `setline(1,['A','B','C'])` → `changenr()` 1, `seq_last` 1, one
/// `undotree().entries`; after `undo` the buffer is `a b c` again.
#[test]
fn one_command_is_one_undo_block() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    executor.execute_line(&mut editor, "call setline(1, ['A','B','C'])").unwrap();
    assert_eq!(buffer_text(&editor), vec!["A", "B", "C"]);
    executor.execute_line(&mut editor, "let g:seq = changenr()").unwrap();
    assert_eq!(global_value(&executor, "seq"), Some(ox_types::Typval::Number(1)));
    executor.execute_line(&mut editor, "let g:last = undotree().seq_last").unwrap();
    assert_eq!(global_value(&executor, "last"), Some(ox_types::Typval::Number(1)));
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(buffer_text(&editor), vec!["a", "b", "c"]);
    executor.execute_line(&mut editor, "let g:after = changenr()").unwrap();
    assert_eq!(global_value(&executor, "after"), Some(ox_types::Typval::Number(0)));
}

/// Two Ex command lines run from a script join the same block, because
/// nothing between them returns to the main loop to read a typed key. The
/// second `:undo` therefore has nothing left to undo.
///
/// Oracle: two separate `call setline()` lines both report `changenr()` 1 and
/// `seq_last` 1, and the second `undo` leaves the original text.
#[test]
fn two_scripted_commands_join_one_undo_block() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    executor.execute_line(&mut editor, "call setline(1, 'A')").unwrap();
    executor.execute_line(&mut editor, "let g:first = changenr()").unwrap();
    executor.execute_line(&mut editor, "call setline(2, 'B')").unwrap();
    executor.execute_line(&mut editor, "let g:second = changenr()").unwrap();
    assert_eq!(buffer_text(&editor), vec!["A", "B", "c"]);
    assert_eq!(global_value(&executor, "first"), Some(ox_types::Typval::Number(1)));
    assert_eq!(
        global_value(&executor, "second"),
        Some(ox_types::Typval::Number(1)),
        "the second command joined the open block"
    );
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(buffer_text(&editor), vec!["a", "b", "c"]);
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(buffer_text(&editor), vec!["a", "b", "c"]);
    assert_eq!(
        echo_messages(&editor).last().map(String::as_str),
        Some("Already at oldest change")
    );
}

/// A typed key closes the open block first, so two commands typed at the
/// prompt are two undo steps. `feedkeys()` queues *typed* input, which is
/// what upstream reports through `gotchars` (`input.c:2495-2497`).
///
/// Oracle: `feedkeys('x','xt')` then `feedkeys('dd','xt')` report
/// `changenr()` 1 then 2, and two undos walk back through both.
#[test]
fn a_typed_key_closes_the_block_so_two_commands_are_two_steps() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"aa".to_vec(), b"bb".to_vec(), b"cc".to_vec()]);
    executor.execute_line(&mut editor, "call feedkeys('x', 'xt')").unwrap();
    executor.execute_line(&mut editor, "let g:first = changenr()").unwrap();
    executor.execute_line(&mut editor, "call feedkeys('dd', 'xt')").unwrap();
    executor.execute_line(&mut editor, "let g:second = changenr()").unwrap();
    assert_eq!(global_value(&executor, "first"), Some(ox_types::Typval::Number(1)));
    assert_eq!(global_value(&executor, "second"), Some(ox_types::Typval::Number(2)));
    assert_eq!(buffer_text(&editor), vec!["bb", "cc"]);
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(buffer_text(&editor), vec!["a", "bb", "cc"]);
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(buffer_text(&editor), vec!["aa", "bb", "cc"]);
}

/// Keys a mapping produced are not typed keys, so two changes made by one
/// mapping stay in one block.
///
/// Oracle: `nnoremap q ddx` then `feedkeys('q','xt')` reports `changenr()` 1,
/// and one `undo` restores all three original lines.
#[test]
fn a_mapping_making_two_changes_is_one_undo_block() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"aa".to_vec(), b"bb".to_vec(), b"cc".to_vec()]);
    executor.execute_line(&mut editor, "nnoremap q ddx").unwrap();
    executor.execute_line(&mut editor, "call feedkeys('q', 'xt')").unwrap();
    assert_eq!(buffer_text(&editor), vec!["b", "cc"]);
    executor.execute_line(&mut editor, "let g:seq = changenr()").unwrap();
    assert_eq!(global_value(&executor, "seq"), Some(ox_types::Typval::Number(1)));
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(buffer_text(&editor), vec!["aa", "bb", "cc"]);
}

/// `:g/pattern/d` deletes each matching line separately and they group.
///
/// Oracle: `g/x/d` over `x1 y x2 y x3` reports `changenr()` 1 and one `undo`
/// restores all five lines.
#[test]
fn a_global_delete_is_one_undo_block() {
    let (mut editor, mut executor) = setup_with_content(&[
        b"x1".to_vec(),
        b"y".to_vec(),
        b"x2".to_vec(),
        b"y".to_vec(),
        b"x3".to_vec(),
    ]);
    executor.execute_line(&mut editor, "g/x/d").unwrap();
    assert_eq!(buffer_text(&editor), vec!["y", "y"]);
    executor.execute_line(&mut editor, "let g:seq = changenr()").unwrap();
    assert_eq!(global_value(&executor, "seq"), Some(ox_types::Typval::Number(1)));
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(buffer_text(&editor), vec!["x1", "y", "x2", "y", "x3"]);
}

/// `:undojoin` reopens the closed block so the next change joins it, and one
/// `undo` then takes both back together.
///
/// Oracle: after two typed changes (`changenr()` 2),
/// `undojoin | call setline(1,'J')` still reports 2, and `undo` returns to
/// state 1.
#[test]
fn undojoin_puts_the_next_change_in_the_previous_block() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"aa".to_vec(), b"bb".to_vec(), b"cc".to_vec()]);
    executor.execute_line(&mut editor, "call feedkeys('x', 'xt')").unwrap();
    executor.execute_line(&mut editor, "call feedkeys('dd', 'xt')").unwrap();
    executor.execute_line(&mut editor, "undojoin | call setline(1, 'J')").unwrap();
    assert_eq!(buffer_text(&editor), vec!["J", "cc"]);
    executor.execute_line(&mut editor, "let g:seq = changenr()").unwrap();
    assert_eq!(global_value(&executor, "seq"), Some(ox_types::Typval::Number(2)));
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(
        buffer_text(&editor),
        vec!["a", "bb", "cc"],
        "the joined change went back with the block it joined"
    );
}

/// `:undojoin` after an undo is E790, because the header it would reopen is
/// the one the undo moved off.
///
/// Both of upstream's rejecting shapes are covered: an undo that landed back
/// at the original state (`b_u_newhead` set, `b_u_curhead` set) and an undo
/// that stopped on an earlier header.
#[test]
fn undojoin_after_an_undo_raises_e790() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "call setline(1, 'A')").unwrap();
    executor.execute_line(&mut editor, "undo").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "undojoin"), "E790");

    // Two blocks, undone by one step: the current state carries a header and
    // there is a newer one ahead of it.
    let (mut editor, mut executor) =
        setup_with_content(&[b"aa".to_vec(), b"bb".to_vec(), b"cc".to_vec()]);
    executor.execute_line(&mut editor, "call feedkeys('x', 'xt')").unwrap();
    executor.execute_line(&mut editor, "call feedkeys('dd', 'xt')").unwrap();
    executor.execute_line(&mut editor, "undo").unwrap();
    executor.execute_line(&mut editor, "let g:seq = changenr()").unwrap();
    assert_eq!(global_value(&executor, "seq"), Some(ox_types::Typval::Number(1)));
    assert_vim_error(executor.execute_line(&mut editor, "undojoin"), "E790");
}

/// `undotree()` reports the active branch oldest-first with the abandoned
/// branch under `alt`, and `synced` tracks whether a block is still open.
///
/// Oracle: two typed changes, an undo, then a third change gives
/// `seq_last` 3, `entries` seqs `[1, 3]` and `alt` `[2]`.
#[test]
fn undotree_reports_the_branch_shape_and_the_sync_flag() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"aa".to_vec(), b"bb".to_vec(), b"cc".to_vec()]);
    executor.execute_line(&mut editor, "call feedkeys('x', 'xt')").unwrap();
    executor.execute_line(&mut editor, "let g:open = undotree().synced").unwrap();
    assert_eq!(global_value(&executor, "open"), Some(ox_types::Typval::Number(0)));
    executor.execute_line(&mut editor, "call feedkeys('dd', 'xt')").unwrap();
    executor.execute_line(&mut editor, "undo").unwrap();
    executor.execute_line(&mut editor, "let g:closed = undotree().synced").unwrap();
    assert_eq!(global_value(&executor, "closed"), Some(ox_types::Typval::Number(1)));
    executor.execute_line(&mut editor, "call feedkeys('x', 'xt')").unwrap();
    executor
        .execute_line(&mut editor, "let g:seqs = map(copy(undotree().entries), 'v:val.seq')")
        .unwrap();
    assert_eq!(
        global_value(&executor, "seqs"),
        Some(ox_types::Typval::list(vec![
            ox_types::Typval::Number(1),
            ox_types::Typval::Number(3),
        ]))
    );
    executor
        .execute_line(&mut editor, "let g:alt = map(copy(undotree().entries[-1].alt), 'v:val.seq')")
        .unwrap();
    assert_eq!(
        global_value(&executor, "alt"),
        Some(ox_types::Typval::list(vec![ox_types::Typval::Number(2)]))
    );
    executor.execute_line(&mut editor, "let g:last = undotree().seq_last").unwrap();
    assert_eq!(global_value(&executor, "last"), Some(ox_types::Typval::Number(3)));
}

/// Saving mid-block and then adding a change to the same block still sets
/// `'modified'`: the block's sequence has not moved, so the saved state has
/// to be identified by how many edits that block held.
#[test]
fn a_change_joining_a_saved_block_still_sets_modified() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec(), b"b".to_vec()]);
    executor.execute_line(&mut editor, "call setline(1, 'A')").unwrap();
    let buffer = editor.current_buffer().expect("current buffer");
    editor.buffer_mut(buffer).expect("buffer").mark_saved();
    assert!(!editor.buffer(buffer).expect("buffer").modified);
    executor.execute_line(&mut editor, "call setline(2, 'B')").unwrap();
    assert!(
        editor.buffer(buffer).expect("buffer").modified,
        "a change that joined the saved block must still mark the buffer modified"
    );
}

// ---------------------------------------------------------------------------
// winnr() counts within one tabpage.
// Citation: eval/window.c get_winnr:278-292 (tp_lastwin for "$").
// ---------------------------------------------------------------------------

/// `winnr('$')` is the current tabpage's window count, not the editor's.
///
/// Only reachable once more than one tabpage can exist, which is why this was
/// latent: with three single-window tabpages upstream answers 1, and 2 after a
/// `:vnew` in the current one.
///
/// Oracle: `:vnew` with eight tabpages open reports `wins=1->2`.
#[test]
fn winnr_counts_windows_in_the_current_tabpage_only() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "tabnew").unwrap();
    // Three tabpages, one window each: the editor holds three windows.
    assert_eq!(editor.windows().len(), 3);
    executor.execute_line(&mut editor, "let g:count = winnr('$')").unwrap();
    assert_eq!(global_value(&executor, "count"), Some(ox_types::Typval::Number(1)));
    executor.execute_line(&mut editor, "vnew").unwrap();
    executor.execute_line(&mut editor, "let g:after = winnr('$')").unwrap();
    assert_eq!(global_value(&executor, "after"), Some(ox_types::Typval::Number(2)));
    assert_eq!(editor.windows().len(), 4);
}

/// `winnr()` numbers the current window within its own tabpage, so the first
/// window of a later tabpage is 1 and not its editor-wide index.
#[test]
fn winnr_numbers_the_current_window_within_its_tabpage() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "tabnew").unwrap();
    executor.execute_line(&mut editor, "let g:nr = winnr()").unwrap();
    assert_eq!(global_value(&executor, "nr"), Some(ox_types::Typval::Number(1)));
}

// ---------------------------------------------------------------------------
// :retab
// Citations: indent.c ex_retab:1436-1617, tabstop_fromto:220-243.
// ---------------------------------------------------------------------------

/// A whitespace run containing a tab is rebuilt for the new `'tabstop'`,
/// measured with the old one: a single tab spanning eight columns becomes two
/// tabs at `ts=4`. Runs of spaces alone are left untouched without `!`.
///
/// Oracle: `["\tone", "        two", "a\t\tb"]` at ts=8 + `retab 4` →
/// `["\t\tone", "        two", "a\t\t\t\tb"]`, ts=4.
#[test]
fn retab_rebuilds_tab_runs_for_the_new_tabstop() {
    let (mut editor, mut executor) = setup_with_content(&[
        b"\tone".to_vec(),
        b"        two".to_vec(),
        b"a\t\tb".to_vec(),
    ]);
    executor.execute_line(&mut editor, "set noexpandtab tabstop=8").unwrap();
    executor.execute_line(&mut editor, "retab 4").unwrap();
    assert_eq!(buffer_text(&editor), vec!["\t\tone", "        two", "a\t\t\t\tb"]);
    executor.execute_line(&mut editor, "let g:ts = &tabstop").unwrap();
    assert_eq!(global_value(&executor, "ts"), Some(ox_types::Typval::Number(4)));
}

/// Without a new value `:retab` normalises against the current `'tabstop'`,
/// which leaves already-correct text alone. `:ret` is the abbreviation.
#[test]
fn retab_without_an_argument_keeps_the_tabstop() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"\tone".to_vec(), b"    three".to_vec()]);
    executor.execute_line(&mut editor, "set noexpandtab tabstop=8").unwrap();
    executor.execute_line(&mut editor, "ret").unwrap();
    assert_eq!(buffer_text(&editor), vec!["\tone", "    three"]);
    executor.execute_line(&mut editor, "let g:ts = &tabstop").unwrap();
    assert_eq!(global_value(&executor, "ts"), Some(ox_types::Typval::Number(8)));
}

/// `'expandtab'` turns every rebuilt run into spaces.
///
/// Oracle: `"\tone"` at ts=8 with `expandtab` + `retab` → eight spaces.
#[test]
fn retab_expands_to_spaces_under_expandtab() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"\tone".to_vec(), b"a\t\tb".to_vec()]);
    executor.execute_line(&mut editor, "set expandtab tabstop=8").unwrap();
    executor.execute_line(&mut editor, "retab").unwrap();
    assert_eq!(
        buffer_text(&editor),
        vec!["        one", &format!("a{}b", " ".repeat(15))]
    );
}

/// `!` also rebuilds runs of more than one space, but only when the rewrite is
/// no longer than the original: eight spaces become a tab while two spaces
/// stay two spaces, because a tab there would render differently
/// (`indent.c:1495,1509`).
///
/// Oracle: `["        eight", "a        b", "  x", "   y"]` at ts=8 + `retab!`
/// → `["\teight", "a\t b", "  x", "   y"]`.
#[test]
fn retab_bang_rebuilds_space_runs_that_can_shorten() {
    let (mut editor, mut executor) = setup_with_content(&[
        b"        eight".to_vec(),
        b"a        b".to_vec(),
        b"  x".to_vec(),
        b"   y".to_vec(),
    ]);
    executor.execute_line(&mut editor, "set noexpandtab tabstop=8").unwrap();
    executor.execute_line(&mut editor, "retab!").unwrap();
    assert_eq!(buffer_text(&editor), vec!["\teight", "a\t b", "  x", "   y"]);
}

/// Without `!` a run of spaces is never touched, even one that could shorten.
#[test]
fn retab_without_bang_leaves_space_runs_alone() {
    let (mut editor, mut executor) = setup_with_content(&[b"        eight".to_vec()]);
    executor.execute_line(&mut editor, "set noexpandtab tabstop=8").unwrap();
    executor.execute_line(&mut editor, "retab").unwrap();
    assert_eq!(buffer_text(&editor), vec!["        eight"]);
}

/// `-indentonly` stops after the leading run, so an interior tab survives.
///
/// Oracle: `"\tone\ttwo"` at ts=8 + `retab -indentonly 4` → `"\t\tone\ttwo"`.
#[test]
fn retab_indentonly_leaves_interior_whitespace() {
    let (mut editor, mut executor) = setup_with_content(&[b"\tone\ttwo".to_vec()]);
    executor.execute_line(&mut editor, "set noexpandtab tabstop=8").unwrap();
    executor.execute_line(&mut editor, "retab -indentonly 4").unwrap();
    assert_eq!(buffer_text(&editor), vec!["\t\tone\ttwo"]);
}

/// A non-numeric argument is E475 and changes nothing.
///
/// Oracle: `retab xyz` → `Vim(retab):E475: Invalid argument: xyz`.
#[test]
fn retab_rejects_a_non_numeric_argument_with_e475() {
    let (mut editor, mut executor) = setup_with_content(&[b"\tone".to_vec()]);
    assert_vim_error(executor.execute_line(&mut editor, "retab xyz"), "E475");
    assert_eq!(buffer_text(&editor), vec!["\tone"]);
}

/// A scan that carries the line past `MAXCOL` is E1240, upstream's
/// `emsg_text_too_long` (`indent.c:1425-1433`, `1563-1567`), and the line keeps
/// the bytes it had from the point the scan gave up.
///
/// This is the ceiling `test_retab.vim`'s `RetabLoop()` relies on. The columns
/// are measured with the *old* `'tabstop'`, so the ceiling does not depend on
/// the value being installed: at `'tabstop'` 4000 a run of 536_871 tabs is
/// 2_147_484_000 columns wide, past `MAXCOL`, whatever `:retab` is given.
/// Without it `retab 4` rebuilds such a run a thousand times larger on every
/// pass, so `while 1 / set ts=4000 / retab 4` never returns.
#[test]
fn retab_past_maxcol_is_e1240_and_leaves_the_line_alone() {
    let wide = vec![b'\t'; 536_871];
    let (mut editor, mut executor) = setup_with_content(&[wide.clone(), b"\tsecond".to_vec()]);
    executor.execute_line(&mut editor, "set noexpandtab tabstop=4000").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "%retab 4000"), "E1240");
    let text = buffer_text(&editor);
    assert_eq!(text[0].len(), wide.len());
    // Upstream's break abandons the rest of the buffer too.
    assert_eq!(text[1], "\tsecond");
}

/// One column below the ceiling is rewritten as usual, so this is a column
/// ceiling and not a size limit on `:retab`.
#[test]
fn retab_just_below_maxcol_still_rebuilds() {
    // 536_870 tabs at 'tabstop' 4000 is 2_147_480_000 columns, under MAXCOL.
    let wide = vec![b'\t'; 536_870];
    let (mut editor, mut executor) = setup_with_content(&[wide]);
    executor.execute_line(&mut editor, "set noexpandtab tabstop=4000").unwrap();
    executor.execute_line(&mut editor, "retab 4000").unwrap();
    assert_eq!(buffer_text(&editor)[0].len(), 536_870);
}

/// A `'vartabstop'` list is reported rather than silently reduced to one of
/// its values: this port has no `'vartabstop'` option at all.
#[test]
fn retab_reports_the_vartabstop_form() {
    let (mut editor, mut executor) = setup_with_content(&[b"\tone".to_vec()]);
    let error = executor.execute_line(&mut editor, "retab 4,8").unwrap_err();
    assert!(
        matches!(&error, ExecError::NotImplemented(name) if name.contains("vartabstop")),
        "unexpected error: {error:?}"
    );
}

/// Only the addressed lines are rebuilt; `:retab`'s default range is the whole
/// buffer (EX_DFLALL), so an explicit range has to be what narrows it.
#[test]
fn retab_only_touches_the_addressed_lines() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"\tone".to_vec(), b"\ttwo".to_vec()]);
    executor.execute_line(&mut editor, "set noexpandtab tabstop=8").unwrap();
    executor.execute_line(&mut editor, "1retab 4").unwrap();
    assert_eq!(buffer_text(&editor), vec!["\t\tone", "\ttwo"]);
}

// ---------------------------------------------------------------------------
// :hide / :sleep / :z / :scriptencoding / :argdelete
// Citations: ex_docmd.c ex_hide:5369, ex_sleep:6459, parse_count:1395,
// ex_cmds.c ex_z:3154, runtime.c ex_scriptencoding:2946,
// arglist.c ex_argdelete:759, arglist_del_files:352.
// ---------------------------------------------------------------------------

/// `:hide` closes the current window and keeps its buffer loaded. `:hid` is
/// the abbreviation.
///
/// Oracle: `split` then `hide` takes `winnr('$')` from 2 back to 1.
#[test]
fn hide_closes_the_current_window() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    let buffer = editor.current_buffer().unwrap();
    executor.execute_line(&mut editor, "split").unwrap();
    assert_eq!(editor.windows().len(), 2);
    executor.execute_line(&mut editor, "hid").unwrap();
    assert_eq!(editor.windows().len(), 1);
    // The buffer survives: :hide is win_close(win, false, ...).
    assert!(editor.buffer(buffer).is_ok());
}

/// The last window cannot be hidden.
#[test]
fn hide_refuses_the_last_window() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    assert_vim_error(executor.execute_line(&mut editor, "hide"), "E444");
}

/// `:sleep` accepts a count with an `m` suffix, which is what the shared count
/// parse had to stop rejecting: upstream takes the digits greedily and leaves
/// the suffix in the argument (`parse_count`, ex_docmd.c:1401).
#[test]
fn sleep_accepts_a_millisecond_suffix() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "sleep 1m").unwrap();
    executor.execute_line(&mut editor, "sl 1m").unwrap();
}

/// A suffix other than `m` is E475 reporting the *remaining* argument, not the
/// whole one.
///
/// Oracle: `sleep 5x` → `Vim(sleep):E475: Invalid argument: x`.
#[test]
fn sleep_rejects_an_unknown_suffix_with_e475() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    let error = executor.execute_line(&mut editor, "sleep 5x").unwrap_err();
    assert_vim_error(Err(error), "E475");
    // The message carries only the tail after the count, as upstream does, and
    // the `Vim(sleep):` prefix `get_exception_string` adds.
    // Oracle: `sleep 1x` → `Vim(sleep):E475: Invalid argument: x`.
    let error = executor.execute_line(&mut editor, "sleep 5x").unwrap_err();
    let ExecError::Vim(exception) = &error else { panic!("expected a Vim error: {error:?}") };
    assert_eq!(exception.message(), "Vim(sleep):E475: Invalid argument: x");
}

/// A zero count is E939, because `sleep` carries no ZEROR
/// (`parse_count`, ex_docmd.c:1420-1425).
///
/// Oracle: `sleep 0m` → `Vim(sleep):E939: Positive count required`.
#[test]
fn sleep_rejects_a_zero_count_with_e939() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    assert_vim_error(executor.execute_line(&mut editor, "sleep 0m"), "E939");
    // A ZEROR command still accepts zero.
    executor.scripts().io().insert("in.txt", "x\n");
    executor.execute_line(&mut editor, "0read in.txt").unwrap();
}

/// `:scriptencoding` outside a sourced file is E167.
#[test]
fn scriptencoding_outside_a_script_raises_e167() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    assert_vim_error(
        executor.execute_line(&mut editor, "scriptencoding utf-8"),
        "E167",
    );
}

/// `:z` prints a window of lines after the addressed one and leaves the cursor
/// on the last of them; a trailing number sets the window size.
///
/// Oracle, `scroll=3` and ten lines: `5z` prints l5..l10 with cursor 10, and
/// `5z3` prints l5..l7 with cursor 7.
#[test]
fn z_prints_a_window_of_lines() {
    let (mut editor, mut executor) = setup_with_content(&z_lines());
    executor.execute_line(&mut editor, "set scroll=3").unwrap();
    executor.execute_line(&mut editor, "5z").unwrap();
    assert_eq!(echo_messages(&editor), vec!["l5", "l6", "l7", "l8", "l9", "l10"]);
    let window = editor.current_window().unwrap();
    assert_eq!(editor.window(window).unwrap().cursor.lnum, 10);
}

/// Each leading kind character picks a different window around the address.
///
/// Oracle: `5z3` → l5..l7 cursor 7; `5z-3` → l3..l5 cursor 5; `5z.3` →
/// l4..l6 cursor 6; `5z^3` → l1..l2 cursor 2; `5z+3` → l6..l8 cursor 8.
#[test]
fn z_kind_characters_select_the_window() {
    for (command, expected, cursor) in [
        ("5z3", vec!["l5", "l6", "l7"], 7),
        ("5z-3", vec!["l3", "l4", "l5"], 5),
        ("5z.3", vec!["l4", "l5", "l6"], 6),
        ("5z^3", vec!["l1", "l2"], 2),
        ("5z+3", vec!["l6", "l7", "l8"], 8),
    ] {
        let (mut editor, mut executor) = setup_with_content(&z_lines());
        executor.execute_line(&mut editor, "set scroll=3").unwrap();
        executor.execute_line(&mut editor, command).unwrap();
        assert_eq!(echo_messages(&editor), expected, "{command}");
        let window = editor.current_window().unwrap();
        assert_eq!(editor.window(window).unwrap().cursor.lnum, cursor, "{command}");
    }
}

/// The `=` form brackets the addressed line with rules and leaves the cursor
/// on it, and its window is two lines wider than the count asks for
/// (`ex_cmds.c:3195-3197`).
///
/// Oracle: `5z=3` prints l3, l4, a rule, l5, a rule, l6, l7, cursor 5.
#[test]
fn z_equals_form_brackets_the_addressed_line() {
    let (mut editor, mut executor) = setup_with_content(&z_lines());
    executor.execute_line(&mut editor, "set scroll=3 columns=80").unwrap();
    executor.execute_line(&mut editor, "5z=3").unwrap();
    let rule = "-".repeat(79);
    assert_eq!(
        echo_messages(&editor),
        vec!["l3", "l4", rule.as_str(), "l5", rule.as_str(), "l6", "l7"]
    );
    let window = editor.current_window().unwrap();
    assert_eq!(editor.window(window).unwrap().cursor.lnum, 5);
}

/// A non-numeric size is E144.
#[test]
fn z_rejects_a_non_numeric_size_with_e144() {
    let (mut editor, mut executor) = setup_with_content(&z_lines());
    assert_vim_error(executor.execute_line(&mut editor, "5z=x"), "E144");
}

fn z_lines() -> Vec<Vec<u8>> {
    (1..=10).map(|index| format!("l{index}").into_bytes()).collect()
}

/// `:argdelete {name}` drops matching entries, and a wildcard matches several.
///
/// Oracle: `args a.txt b.txt c.txt` + `argdelete b.txt` → `['a.txt','c.txt']`;
/// `args a.txt b.txt` + `argdelete *.txt` → `[]`.
#[test]
fn argdelete_removes_entries_by_name_and_pattern() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "args a.txt b.txt c.txt").unwrap();
    executor.execute_line(&mut editor, "argdelete b.txt").unwrap();
    assert_eq!(arglist_names(&editor), vec!["a.txt", "c.txt"]);
    executor.execute_line(&mut editor, "argd *.txt").unwrap();
    assert!(arglist_names(&editor).is_empty());
}

/// An address removes entries by position instead.
///
/// Oracle: `args a.txt b.txt c.txt d.txt` + `2,3argdelete` →
/// `['a.txt','d.txt']`.
#[test]
fn argdelete_removes_the_addressed_entries() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor
        .execute_line(&mut editor, "args a.txt b.txt c.txt d.txt")
        .unwrap();
    executor.execute_line(&mut editor, "2,3argdelete").unwrap();
    assert_eq!(arglist_names(&editor), vec!["a.txt", "d.txt"]);
}

/// A name matching nothing is E480, and a bare `:argdelete` with no current
/// entry is E610. They are different errors and both leave the list alone.
///
/// Oracle: `argdelete zzz` → `Vim(argdelete):E480: No match: zzz`; a bare
/// `argdelete` on an empty list → `Vim(argdelete):E610: No argument to delete`.
#[test]
fn argdelete_reports_no_match_and_no_argument_separately() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "args a.txt").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "argdelete zzz"), "E480");
    assert_eq!(arglist_names(&editor), vec!["a.txt"]);
    executor.execute_line(&mut editor, "argdelete").unwrap();
    assert!(arglist_names(&editor).is_empty());
    assert_vim_error(executor.execute_line(&mut editor, "argdelete"), "E610");
}

/// An address and a name argument together are E475.
#[test]
fn argdelete_rejects_an_address_with_an_argument() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "args a.txt b.txt").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "1argdelete a.txt"), "E475");
    assert_eq!(arglist_names(&editor), vec!["a.txt", "b.txt"]);
}

fn arglist_names(editor: &Editor) -> Vec<String> {
    editor
        .arglist()
        .names()
        .iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// :lockvar / :unlockvar
// Citation: eval/vars.c ex_lockvar:1554.
// ---------------------------------------------------------------------------

/// `:lockvar` marks a variable and `:unlockvar` releases it; assigning to a
/// locked one is E741. `:lockv` and `:unlo` are the abbreviations.
///
/// Oracle: `let g:v = 1 | lockvar g:v` then `let g:v = 2` →
/// `Vim(let):E741: Value is locked: g:v`, and the value stays 1.
#[test]
fn lockvar_blocks_assignment_until_unlocked() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "let g:v = 1").unwrap();
    executor.execute_line(&mut editor, "lockv g:v").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "let g:v = 2"), "E741");
    assert_eq!(global_value(&executor, "v"), Some(ox_types::Typval::Number(1)));
    executor.execute_line(&mut editor, "unlo g:v").unwrap();
    executor.execute_line(&mut editor, "let g:v = 3").unwrap();
    assert_eq!(global_value(&executor, "v"), Some(ox_types::Typval::Number(3)));
}

/// The bang is upstream's depth of -1 rather than the default 2, and
/// `:unlockvar!` releases it again.
#[test]
fn lockvar_bang_locks_and_unlocks() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "let g:d = {'a': [1]}").unwrap();
    executor.execute_line(&mut editor, "lockvar! g:d").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "let g:d = {}"), "E741");
    executor.execute_line(&mut editor, "unlockvar! g:d").unwrap();
    executor.execute_line(&mut editor, "let g:d = {}").unwrap();
}

/// A leading digit run is the depth, not a variable name, and the depth is
/// passed through: depth 0 locks the *variable* (E1122) while any deeper value
/// locks its *value* (E741).
///
/// Oracle, on `[1]`: `lockvar 0` then reassigning gives
/// `Vim(let):E1122: Variable is locked: g:l0`, while `lockvar 1` and
/// `lockvar 2` give `Vim(let):E741: Value is locked: g:lN`. That difference is
/// what proves the digits reached the engine as a depth.
#[test]
fn lockvar_passes_the_explicit_depth_through() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    for (depth, code) in [("0", "E1122"), ("1", "E741"), ("2", "E741")] {
        let name = format!("g:l{depth}");
        executor
            .execute_line(&mut editor, &format!("let {name} = [1]"))
            .unwrap();
        executor
            .execute_line(&mut editor, &format!("lockvar {depth} {name}"))
            .unwrap();
        assert_vim_error(
            executor.execute_line(&mut editor, &format!("let {name} = [9]")),
            code,
        );
    }
}

/// Several names in one command are all locked, as `ex_unletlock` walks them.
#[test]
fn lockvar_locks_every_named_variable() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    executor.execute_line(&mut editor, "let g:p = 1").unwrap();
    executor.execute_line(&mut editor, "let g:q = 1").unwrap();
    executor.execute_line(&mut editor, "lockvar g:p g:q").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "let g:p = 2"), "E741");
    assert_vim_error(executor.execute_line(&mut editor, "let g:q = 2"), "E741");
}

/// `:lockvar` needs a name (NEEDARG), so a bare one is E471.
///
/// Oracle: `lockvar` → `Vim(lockvar):E471: Argument required`.
#[test]
fn lockvar_without_a_name_raises_e471() {
    let (mut editor, mut executor) = setup_with_content(&[b"a".to_vec()]);
    assert_vim_error(executor.execute_line(&mut editor, "lockvar"), "E471");
}

// ---------------------------------------------------------------------------
// :fold / :foldopen / :foldclose
// Citations: ex_docmd.c ex_fold:8019, ex_foldopen:8028, fold.c
// foldManualAllowed:522, foldCreate:538, foldCreateMarkers:1554,
// opFoldRange:386.
// ---------------------------------------------------------------------------

fn fold_ranges(editor: &Editor) -> Vec<(usize, usize)> {
    let buffer = editor.current_buffer().unwrap();
    editor
        .buffer(buffer)
        .unwrap()
        .folds
        .folds()
        .iter()
        .map(|fold| (fold.range.start.row, fold.range.end.row))
        .collect()
}

fn fold_states(editor: &Editor) -> Vec<crate::fold::FoldState> {
    let buffer = editor.current_buffer().unwrap();
    editor
        .buffer(buffer)
        .unwrap()
        .folds
        .folds()
        .iter()
        .map(|fold| fold.state)
        .collect()
}

/// `:{range}fold` records a closed fold over the addressed lines, and `:fo` is
/// the abbreviation. The range is inclusive one-based, the fold half-open
/// zero-based.
///
/// Oracle: `2,4fold` on six lines gives `foldclosed(3) == 2` and
/// `foldlevel(3) == 1`.
#[test]
fn fold_creates_a_closed_manual_fold() {
    let (mut editor, mut executor) = setup_with_content(&fold_lines());
    executor.execute_line(&mut editor, "set foldmethod=manual").unwrap();
    executor.execute_line(&mut editor, "2,4fo").unwrap();
    assert_eq!(fold_ranges(&editor), vec![(1, 4)]);
    assert_eq!(fold_states(&editor), vec![crate::fold::FoldState::Closed]);
}

/// A `'foldmethod'` that is neither `manual` nor `marker` is E350, and the
/// buffer is left alone.
///
/// Oracle: with `foldmethod=indent`, `1,2fold` →
/// `Vim(fold):E350: Cannot create fold with current 'foldmethod'`, text
/// unchanged. Same for `expr`.
#[test]
fn fold_rejects_a_computed_foldmethod_with_e350() {
    for method in ["indent", "expr", "syntax", "diff"] {
        let (mut editor, mut executor) = setup_with_content(&fold_lines());
        executor
            .execute_line(&mut editor, &format!("set foldmethod={method}"))
            .unwrap();
        assert_vim_error(executor.execute_line(&mut editor, "1,2fold"), "E350");
        assert_eq!(buffer_text(&editor).len(), 6, "{method}");
    }
}

/// The guard reads the real option, so switching back to `manual` lets the
/// same command through. Without that wiring the E350 branch could never fire
/// and this pair would be indistinguishable.
#[test]
fn fold_guard_follows_the_foldmethod_option() {
    let (mut editor, mut executor) = setup_with_content(&fold_lines());
    executor.execute_line(&mut editor, "set foldmethod=indent").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "1,2fold"), "E350");
    executor.execute_line(&mut editor, "set foldmethod=manual").unwrap();
    executor.execute_line(&mut editor, "1,2fold").unwrap();
    assert_eq!(fold_ranges(&editor), vec![(0, 2)]);
}

/// Under `'foldmethod'` of `marker`, `:fold` writes the `'foldmarker'` pair
/// into the text instead of recording a range.
///
/// Oracle: `1,3fold` with `foldmethod=marker` gives
/// `a1{{{`, `a2`, `a3}}}`.
#[test]
fn fold_under_marker_writes_the_markers() {
    let (mut editor, mut executor) = setup_with_content(&fold_lines());
    executor.execute_line(&mut editor, "set foldmethod=marker").unwrap();
    executor.execute_line(&mut editor, "1,3fold").unwrap();
    assert_eq!(
        buffer_text(&editor),
        vec!["a1{{{", "a2", "a3}}}", "a4", "a5", "a6"]
    );
    // No manual range was recorded: the markers are the fold.
    assert!(fold_ranges(&editor).is_empty());
}

/// `:foldopen` opens a fold over the addressed lines and `:foldclose` closes
/// it again.
///
/// Oracle: after `2,4fold`, `3foldopen` gives `foldclosed(3) == -1` and
/// `3foldclose` gives 2 again.
#[test]
fn foldopen_and_foldclose_toggle_the_fold() {
    let (mut editor, mut executor) = setup_with_content(&fold_lines());
    executor.execute_line(&mut editor, "set foldmethod=manual").unwrap();
    executor.execute_line(&mut editor, "2,4fold").unwrap();
    executor.execute_line(&mut editor, "3foldopen").unwrap();
    assert_eq!(fold_states(&editor), vec![crate::fold::FoldState::Open]);
    executor.execute_line(&mut editor, "3foldclose").unwrap();
    assert_eq!(fold_states(&editor), vec![crate::fold::FoldState::Closed]);
}

/// A line with no fold is E490, for both directions.
///
/// Oracle: `5foldopen` and `5foldclose` with no fold there both report
/// `E490: No fold found`.
#[test]
fn foldopen_without_a_fold_raises_e490() {
    let (mut editor, mut executor) = setup_with_content(&fold_lines());
    executor.execute_line(&mut editor, "set foldmethod=manual").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "5foldopen"), "E490");
    assert_vim_error(executor.execute_line(&mut editor, "5foldclose"), "E490");
}

/// A fold already in the requested state is *not* an error: upstream's
/// `setManualFoldWin` records `DONE_FOLD` without `DONE_ACTION`, and E490
/// needs `DONE_NOTHING`.
///
/// Oracle: two `4foldopen` in a row on one fold both succeed silently.
#[test]
fn foldopen_on_an_already_open_fold_is_not_an_error() {
    let (mut editor, mut executor) = setup_with_content(&fold_lines());
    executor.execute_line(&mut editor, "set foldmethod=manual").unwrap();
    executor.execute_line(&mut editor, "4,5fold").unwrap();
    executor.execute_line(&mut editor, "4foldopen").unwrap();
    executor.execute_line(&mut editor, "4foldopen").unwrap();
    assert_eq!(fold_states(&editor), vec![crate::fold::FoldState::Open]);
}

/// `:foldopen!` is the recursive form, so a nested fold opens too where the
/// plain form leaves it closed.
#[test]
fn foldopen_bang_opens_nested_folds() {
    let (mut editor, mut executor) = setup_with_content(&fold_lines());
    executor.execute_line(&mut editor, "set foldmethod=manual").unwrap();
    executor.execute_line(&mut editor, "2,3fold").unwrap();
    executor.execute_line(&mut editor, "1,6fold").unwrap();
    assert_eq!(fold_ranges(&editor), vec![(0, 6), (1, 3)]);
    executor.execute_line(&mut editor, "2foldopen!").unwrap();
    assert_eq!(
        fold_states(&editor),
        vec![crate::fold::FoldState::Open, crate::fold::FoldState::Open]
    );
}

fn fold_lines() -> Vec<Vec<u8>> {
    (1..=6).map(|index| format!("a{index}").into_bytes()).collect()
}

// ---------------------------------------------------------------------------
// <f-args> expansion in user command bodies
// Citations: usercmd.c `uc_split_args` (1189-1302) and the `ct_ARGS`/`quote==2`
// case (1501-1559); test/old/testdir/check.vim defines every `Check*` command
// as `command -nargs=1 CheckX call CheckX(<f-args>)`.
// ---------------------------------------------------------------------------

/// Echoes `string([<f-args>])` from a `-nargs=*` user command so the expansion
/// is observable as the argument list the body actually received.
fn invoke_with_f_args(arguments: &str) -> String {
    let (mut editor, mut executor) = setup();
    executor
        .execute_line(&mut editor, "command -nargs=* FArgs echo string([<f-args>])")
        .unwrap();
    executor
        .execute_line(&mut editor, &format!("FArgs {arguments}"))
        .unwrap();
    match &editor.messages().last().unwrap().content {
        ox_types::Object::String(text) => text.to_string_lossy().into_owned(),
        other => panic!("expected a string message, got {other:?}"),
    }
}

/// The normal case: `<f-args>` splits on whitespace into one quoted argument
/// each, and runs of whitespace collapse.
/// Upstream: `usercmd.c:1262-1270` emits `", "` for each whitespace run.
#[test]
fn f_args_splits_arguments_on_whitespace() {
    assert_eq!(invoke_with_f_args("one"), "['one']");
    assert_eq!(invoke_with_f_args("one two"), "['one', 'two']");
    assert_eq!(invoke_with_f_args("one   two  three"), "['one', 'two', 'three']");
}

/// Boundary: an empty argument list expands to *nothing*, so the body sees zero
/// arguments rather than one empty string.
/// Upstream: `usercmd.c:1503-1512` returns length 0 for `quote == 2`.
#[test]
fn f_args_with_no_arguments_expands_to_nothing() {
    assert_eq!(invoke_with_f_args(""), "[]");
}

/// A backslash-escaped space joins two words into one argument and `\\`
/// collapses to a single backslash.
/// Upstream: `usercmd.c:1252-1261`.
#[test]
fn f_args_honours_backslash_escapes() {
    assert_eq!(invoke_with_f_args(r"a\ b c"), "['a b', 'c']");
    assert_eq!(invoke_with_f_args(r"a\\b"), r"['a\b']");
}

/// A double quote inside an argument is escaped, not left to terminate the
/// generated string. Checked against the splitter directly because the Ex
/// argument parser truncates the command line at `"` before the expansion
/// runs, which is a separate defect in command parsing.
/// Upstream: `usercmd.c:1259-1261`.
#[test]
fn f_args_escapes_embedded_double_quote() {
    assert_eq!(crate::excmd_exec::split_command_arguments(r#"he"llo"#), r#""he\"llo""#);
    assert_eq!(crate::excmd_exec::split_command_arguments("a b"), r#""a", "b""#);
    assert_eq!(crate::excmd_exec::split_command_arguments(""), "");
}

/// `<f-args>` is the construct that `check.vim` relies on: the whole oldtest
/// suite routes `CheckFeature x` through `call CheckFeature(<f-args>)`. Before
/// the expansion existed the literal `<f-args>` reached the expression parser
/// and raised `E15`, aborting the file before any test ran.
#[test]
fn f_args_carries_check_vim_style_dispatch() {
    let (mut editor, mut executor) = setup();
    // check.vim defines the command in its own script; the test file that calls
    // it is parsed afterwards, which is what makes the invocation resolvable.
    executor
        .execute_script(
            &mut editor,
            "check.vim",
            "function Feature(name)\nlet g:seen = a:name\nendfunction\ncommand -nargs=1 CheckFeature call Feature(<f-args>)",
        )
        .unwrap();

    executor.execute_line(&mut editor, "CheckFeature arabic").unwrap();

    let seen = executor
        .scope()
        .global
        .iter()
        .find(|(name, _)| name.as_bytes() == b"seen")
        .and_then(|(_, value)| match value {
            ox_types::Typval::String(text) => Some(text.to_string_lossy().into_owned()),
            _ => None,
        });
    assert_eq!(seen.as_deref(), Some("arabic"));
}

// ---------------------------------------------------------------------------
// Mapping execution: `:normal[!]`, `feedkeys()` and the map modifiers.
// Citations: `src/nvim/ex_docmd.c` `ex_normal`/`exec_normal_cmd`/`exec_normal`
// (7133-7291), `src/nvim/mapping.c` `str_to_mapargs` (400-451) and `do_map`'s
// `<unique>` rejection (802), `src/nvim/keycodes.c` `replace_termcodes`,
// `src/nvim/getchar.c` `vgetorpeek`'s `ex_normal_busy` escape and mapping
// timeout.
// ---------------------------------------------------------------------------

/// Reads a global set by a mapping, as a plain string.
fn global_text(executor: &ExExecutor<MemoryFileIO>, name: &str) -> Option<String> {
    match global_value(executor, name) {
        Some(ox_types::Typval::String(text)) => Some(text.to_string_lossy().into_owned()),
        _ => None,
    }
}

/// `ins_typebuf(cmd, REMAP_YES, ...)` (`ex_docmd.c:7266`): `:normal` puts its
/// argument through the typeahead, so mapping lookup sees it. Before this the
/// argument bypassed typeahead entirely and `,x` ran as the two literal keys.
#[test]
fn normal_applies_a_mapping_and_normal_bang_does_not() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap ,x :let g:hit = 'yes'<CR>").unwrap();

    executor.execute_line(&mut editor, "normal ,x").unwrap();
    assert_eq!(global_text(&executor, "hit").as_deref(), Some("yes"));

    executor.execute_line(&mut editor, "let g:hit = 'no'").unwrap();
    executor.execute_line(&mut editor, "normal! ,x").unwrap();
    assert_eq!(global_text(&executor, "hit").as_deref(), Some("no"));
}

/// A key right-hand side reaches the buffer through the same path, including
/// the `<Esc>` that ends the insert: `replace_termcodes` turns the notation
/// into one byte before the mapping is stored.
#[test]
fn normal_applies_a_mapping_to_keys_with_decoded_notation() {
    let (mut editor, mut executor) = setup_with_content(&[b"aaa".to_vec(), b"bbb".to_vec()]);
    executor.execute_line(&mut editor, "nnoremap ,q ix<Esc>").unwrap();

    executor.execute_line(&mut editor, "normal ,q").unwrap();

    assert_eq!(buffer_text(&editor), vec!["xaaa".to_owned(), "bbb".to_owned()]);
}

/// `nmap` expands the produced keys again, `nnoremap` does not
/// (`ins_typebuf`'s `noremap` argument).
#[test]
fn normal_honors_the_recursion_flag_of_the_mapping_it_ran() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap ,m :let g:deep = 'ran'<CR>").unwrap();
    executor.execute_line(&mut editor, "nmap ,n ,m").unwrap();
    executor.execute_line(&mut editor, "normal ,n").unwrap();
    assert_eq!(global_text(&executor, "deep").as_deref(), Some("ran"));

    executor.execute_line(&mut editor, "let g:deep = 'not'").unwrap();
    executor.execute_line(&mut editor, "nnoremap ,p ,m").unwrap();
    executor.execute_line(&mut editor, "normal ,p").unwrap();
    assert_eq!(global_text(&executor, "deep").as_deref(), Some("not"));
}

/// `save_typeahead`/`restore_typeahead` (`ex_docmd.c:7096,7103`): `:normal`
/// must neither consume nor lose input that was already queued.
#[test]
fn normal_leaves_previously_queued_input_alone() {
    let (mut editor, mut executor) = setup_with_content(&[b"aaa".to_vec()]);
    executor.execute_line(&mut editor, "call feedkeys('x', 't')").unwrap();
    let queued = editor.typeahead().len();
    assert_eq!(queued, 1, "feedkeys() without 'x' only queues");

    executor.execute_line(&mut editor, "normal! $").unwrap();

    assert_eq!(editor.typeahead().len(), queued, "the queued key survived");
    assert_eq!(buffer_text(&editor), vec!["aaa".to_owned()], "and was not executed");
}

/// `vgetorpeek`'s `ex_normal_busy` escape: an argument that ends inside Insert
/// or Cmdline mode gets ESC, so it cannot hang and cannot leak the mode.
/// Insert's ESC also moves the caret left, which is what makes the following
/// `x` delete `o` rather than the character after it.
#[test]
fn normal_escapes_out_of_a_half_finished_insert_or_command_line() {
    let (mut editor, mut executor) = setup_with_content(&[b"aaa".to_vec()]);
    executor.execute_line(&mut editor, "normal! ihello").unwrap();
    executor.execute_line(&mut editor, "normal! x").unwrap();
    assert_eq!(buffer_text(&editor), vec!["hellaaa".to_owned()]);

    // A command line with no terminating CR is abandoned, not executed.
    executor.execute_line(&mut editor, "normal! :let g:never = 1").unwrap();
    assert!(global_value(&executor, "never").is_none());
}

/// `ex_normal`'s per-line loop (`ex_docmd.c:7189-7198`): with a range the
/// argument runs once for each addressed line, from column zero.
#[test]
fn normal_with_a_range_repeats_once_per_addressed_line() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"aaa".to_vec(), b"bbb".to_vec(), b"ccc".to_vec()]);

    executor.execute_line(&mut editor, "2,3normal! Ax").unwrap();

    assert_eq!(
        buffer_text(&editor),
        vec!["aaa".to_owned(), "bbbx".to_owned(), "cccx".to_owned()]
    );
}

/// `str_to_mapargs` (`mapping.c:400-451`) strips the modifier prefix before
/// reading the left-hand side. Scanning the whole argument for the modifiers
/// and leaving them in place made the modifier itself the left-hand side, so
/// `nnoremap <silent> ,x :cmd<CR>` registered `<silent>` and `,x` did nothing.
#[test]
fn map_modifiers_are_stripped_before_the_left_hand_side() {
    for modifiers in ["<silent>", "<nowait>", "<unique>", "<special>", "<buffer> <silent>", "<silent><nowait>"] {
        let (mut editor, mut executor) = setup();
        executor
            .execute_line(&mut editor, &format!("nnoremap {modifiers} ,x :let g:hit = 'yes'<CR>"))
            .unwrap();
        executor.execute_line(&mut editor, "normal ,x").unwrap();
        assert_eq!(
            global_text(&executor, "hit").as_deref(),
            Some("yes"),
            "mapping with {modifiers} did not run"
        );
    }
}

/// `<unique>` rejects a second definition of the same left-hand side
/// (`do_map`'s `retval = 5`, `mapping.c:802`), and the first one survives.
#[test]
fn unique_rejects_a_second_definition_of_the_same_lhs() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap <unique> ,x :let g:hit = 'first'<CR>").unwrap();

    assert_vim_error(
        executor.execute_line(&mut editor, "nnoremap <unique> ,x :let g:hit = 'second'<CR>"),
        "E227",
    );

    executor.execute_line(&mut editor, "normal ,x").unwrap();
    assert_eq!(global_text(&executor, "hit").as_deref(), Some("first"));
}

/// `<expr>`'s right-hand side is an expression re-evaluated on every use, and
/// its *result* is the key sequence (`eval_map_expr`, `mapping.c`).
#[test]
fn expr_mapping_evaluates_its_right_hand_side_into_keys() {
    let (mut editor, mut executor) = setup_with_content(&[b"aaa".to_vec()]);
    executor.execute_line(&mut editor, "let g:pick = 'i1'").unwrap();
    executor.execute_line(&mut editor, "nnoremap <expr> ,e g:pick . \"\\x1b\"").unwrap();

    executor.execute_line(&mut editor, "normal ,e").unwrap();
    assert_eq!(buffer_text(&editor), vec!["1aaa".to_owned()]);

    // Re-evaluated, not captured at definition time.
    executor.execute_line(&mut editor, "let g:pick = 'i2'").unwrap();
    executor.execute_line(&mut editor, "normal ,e").unwrap();
    assert_eq!(buffer_text(&editor), vec!["21aaa".to_owned()]);
}

/// `<Leader>` expands to `mapleader`'s text, read from the live scope: a
/// script that sets `g:mapleader` and defines the mapping on the next line
/// must see the new value, not the one it started with.
#[test]
fn leader_expands_from_the_value_set_earlier_in_the_same_script() {
    let (mut editor, mut executor) = setup();

    executor
        .execute_script(
            &mut editor,
            "leader.vim",
            "let g:mapleader = ','\nnnoremap <Leader>z :let g:hit = 'yes'<CR>\nnormal ,z",
        )
        .unwrap();

    assert_eq!(global_text(&executor, "hit").as_deref(), Some("yes"));
}

/// `vgetorpeek`'s mapping timeout: with the queue exhausted an incomplete
/// mapping behaves as if it timed out. The longest *complete* match already
/// queued wins; with no complete match the keys are released literally rather
/// than dropped.
#[test]
fn an_incomplete_mapping_times_out_at_the_end_of_the_queue() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap ,x :let g:hit = 'short'<CR>").unwrap();
    executor.execute_line(&mut editor, "nnoremap ,xy :let g:hit = 'long'<CR>").unwrap();

    executor.execute_line(&mut editor, "normal ,x").unwrap();
    assert_eq!(global_text(&executor, "hit").as_deref(), Some("short"));

    executor.execute_line(&mut editor, "normal ,xy").unwrap();
    assert_eq!(global_text(&executor, "hit").as_deref(), Some("long"));

    // Prefix with no complete match behind it: the keys run as themselves.
    let (mut editor, mut executor) = setup_with_content(&[b"aaa".to_vec()]);
    executor.execute_line(&mut editor, "nnoremap zzq ix<Esc>").unwrap();
    executor.execute_line(&mut editor, "normal zzx").unwrap();
    assert_eq!(buffer_text(&editor), vec!["aa".to_owned()], "`x` deleted a character");
}

/// A buffer-local mapping wins over a global one with the same left-hand side
/// and is invisible from another buffer (`input.c:2319-2438`).
#[test]
fn a_buffer_local_mapping_shadows_the_global_one_under_normal() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap ,b :let g:hit = 'global'<CR>").unwrap();
    executor.execute_line(&mut editor, "nnoremap <buffer> ,b :let g:hit = 'local'<CR>").unwrap();

    executor.execute_line(&mut editor, "normal ,b").unwrap();
    assert_eq!(global_text(&executor, "hit").as_deref(), Some("local"));

    executor.execute_line(&mut editor, "enew").unwrap();
    executor.execute_line(&mut editor, "normal ,b").unwrap();
    assert_eq!(global_text(&executor, "hit").as_deref(), Some("global"));
}

/// `:unmap` of a left-hand side that is not mapped is `E31`
/// (`do_map`'s `retval = 2`, `mapping.c`), not a silent success.
#[test]
fn unmap_without_a_mapping_is_e31() {
    let (mut editor, mut executor) = setup();
    assert_vim_error(executor.execute_line(&mut editor, "nunmap ,zzz"), "E31");

    executor.execute_line(&mut editor, "nnoremap ,zzz :let g:hit = 1<CR>").unwrap();
    executor.execute_line(&mut editor, "nunmap ,zzz").unwrap();
    assert_vim_error(executor.execute_line(&mut editor, "nunmap ,zzz"), "E31");
}

/// Task 64's typed-versus-mapped distinction must survive `:normal` going
/// through the typeahead: `ins_typebuf`'s `nottyped` argument is true, so the
/// keys never reach `may_sync_undo` and everything one `:normal` does is one
/// undo block.
#[test]
fn a_mapping_run_by_normal_stays_one_undo_block() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"aa".to_vec(), b"bb".to_vec(), b"cc".to_vec()]);
    executor.execute_line(&mut editor, "nnoremap ,q ddx").unwrap();

    executor.execute_line(&mut editor, "normal ,q").unwrap();
    assert_eq!(buffer_text(&editor), vec!["b".to_owned(), "cc".to_owned()]);

    executor.execute_line(&mut editor, "undo").unwrap();
    assert_eq!(
        buffer_text(&editor),
        vec!["aa".to_owned(), "bb".to_owned(), "cc".to_owned()],
        "one undo restored both changes"
    );
}

/// `if (++mapdepth >= p_mmd) { emsg(e_recursive_mapping) }` followed by
/// `flush_buffers(FLUSH_MINIMAL)` and `return map_result_fail`
/// (`vgetorpeek`, `input.c:2513-2518`): a mapping whose right-hand side
/// re-triggers it must stop, not expand forever, and it stops with a *message*
/// and a discarded queue rather than a thrown exception.
///
/// Oracle, v0.13.0-dev-1390: `:try | normal ,x | catch | ... | endtry` around
/// `nmap ,x ,x` catches nothing and the following line still runs.
#[test]
fn a_self_recursive_mapping_stops_at_maxmapdepth() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nmap ,x ,x").unwrap();

    executor.execute_line(&mut editor, "normal ,x").unwrap();
    assert_eq!(last_error_message(&editor).as_deref(), Some("E223: recursive mapping"));

    // The depth counter is per key consumed, not cumulative: a mapping that
    // does terminate still works afterwards.
    executor.execute_line(&mut editor, "nunmap ,x").unwrap();
    executor.execute_line(&mut editor, "nnoremap ,y :let g:hit = 'yes'<CR>").unwrap();
    executor.execute_line(&mut editor, "normal ,y").unwrap();
    assert_eq!(global_text(&executor, "hit").as_deref(), Some("yes"));
}

/// A low `'maxmapdepth'` makes the limit observable rather than inferred.
/// Every mapping application counts, including the one that installs the Ex
/// command, so with a limit of 2 the one-hop `,c` runs and the three-hop `,a`
/// does not.
#[test]
fn maxmapdepth_bounds_how_far_a_mapping_chain_expands() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "set maxmapdepth=2").unwrap();
    executor.execute_line(&mut editor, "nnoremap ,c :let g:hit = 'yes'<CR>").unwrap();
    executor.execute_line(&mut editor, "nmap ,b ,c").unwrap();
    executor.execute_line(&mut editor, "nmap ,a ,b").unwrap();

    executor.execute_line(&mut editor, "normal ,a").unwrap();
    assert_eq!(last_error_message(&editor).as_deref(), Some("E223: recursive mapping"));
    assert_eq!(global_text(&executor, "hit"), None, "the chain never reached ,c");

    executor.execute_line(&mut editor, "normal ,c").unwrap();
    assert_eq!(global_text(&executor, "hit").as_deref(), Some("yes"));
}

/// The most recent error message, as text.
fn last_error_message(editor: &Editor) -> Option<String> {
    editor
        .messages()
        .iter()
        .rev()
        .find(|message| message.kind == crate::MessageKind::Error)
        .and_then(|message| match &message.content {
            ox_types::Object::String(text) => Some(text.to_string_lossy().into_owned()),
            _ => None,
        })
}

/// `nv_csearch`: `;` and `,` with no previous `f`/`t` fail in `searchc`, and
/// the `clearopbeep` that follows runs `flush_buffers(FLUSH_MINIMAL)` — which
/// discards the rest of the `:normal` argument, because `ex_normal` stuffs it
/// with `nottyped = true` and that counts into `tb_maplen`.
///
/// Oracle, v0.13.0-dev-1390: with `['aaa','bbb','ccc']` and the cursor at 1,1,
/// `:normal! ,x` leaves `aaa` and `:normal! x,x` leaves `aa`.
#[test]
fn an_error_in_normal_discards_the_rest_of_its_argument() {
    let (mut editor, mut executor) = setup_with_content(&[b"aaa".to_vec(), b"bbb".to_vec()]);
    executor.execute_line(&mut editor, "normal! ,x").unwrap();
    assert_eq!(buffer_text(&editor)[0], "aaa", "',' failed, so 'x' never ran");

    executor.execute_line(&mut editor, "normal! x,x").unwrap();
    assert_eq!(buffer_text(&editor)[0], "aa", "the first 'x' ran, the second did not");

    // A find that succeeds still repeats, and the keys after it still run.
    let (mut editor, mut executor) = setup_with_content(&[b"abcabc".to_vec()]);
    executor.execute_line(&mut editor, "normal! fbx;x").unwrap();
    assert_eq!(buffer_text(&editor)[0], "acac");

    // A find whose target is absent fails the same way.
    let (mut editor, mut executor) = setup_with_content(&[b"abc".to_vec()]);
    executor.execute_line(&mut editor, "normal! fzx").unwrap();
    assert_eq!(buffer_text(&editor)[0], "abc", "'fz' found nothing, so 'x' never ran");
}

/// Nothing in `do_one_cmd` strips a trailing CR from an Ex argument, and
/// `str_to_mapargs` (`mapping.c:463-475`) takes the mapping rhs verbatim from
/// `skipwhite(lhs_end)` to the end. So a right-hand side that ends in a bare
/// CR — the shape `execute("nnoremap ,x :let g:b=3\r")` produces — runs its
/// command line instead of being abandoned by the implicit ESC.
///
/// Oracle, v0.13.0-dev-1390: all three `g:` values below are set.
#[test]
fn a_trailing_cr_survives_into_normal_and_a_mapping_rhs() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "normal! :let g:direct = 'one'\r").unwrap();
    assert_eq!(global_text(&executor, "direct").as_deref(), Some("one"));

    executor.execute_line(&mut editor, "nnoremap ,x :let g:mapped = 'two'\r").unwrap();
    executor.execute_line(&mut editor, "normal ,x").unwrap();
    assert_eq!(global_text(&executor, "mapped").as_deref(), Some("two"));

    // The `<CR>` notation form must keep working alongside the raw byte.
    executor.execute_line(&mut editor, "nnoremap ,y :let g:noted = 'three'<CR>").unwrap();
    executor.execute_line(&mut editor, "normal ,y").unwrap();
    assert_eq!(global_text(&executor, "noted").as_deref(), Some("three"));

    // `<q-args>` expands `ea.arg` as written. Oracle: `execute("T69Q \rhi")`
    // gives `"\rhi"` and `execute("T69R hi\r")` gives `"hi\r"`, so neither end
    // of a user command's argument is trimmed of anything but space and tab.
    executor
        .execute_line(&mut editor, "command! -nargs=1 T69Q let g:qargs = <q-args>")
        .unwrap();
    executor.execute_line(&mut editor, "T69Q \rhi").unwrap();
    assert_eq!(global_text(&executor, "qargs").as_deref(), Some("\rhi"));
    executor.execute_line(&mut editor, "T69Q hi\r").unwrap();
    assert_eq!(global_text(&executor, "qargs").as_deref(), Some("hi\r"));
}

/// The trailing-space half of the same rule. `:map` is EX_TRLBAR *with*
/// EX_NOTRLCOM, so `del_trailing_spaces` never reaches it and the spaces stay
/// in the right-hand side — the rhs below still executes with three spaces
/// and a CR after it. `:edit` has no EX_NOTRLCOM, so its file name loses
/// them.
#[test]
fn a_map_keeps_trailing_spaces_and_edit_does_not() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap ,z :let g:spaced = 'a'   \r").unwrap();
    executor.execute_line(&mut editor, "normal ,z").unwrap();
    assert_eq!(global_text(&executor, "spaced").as_deref(), Some("a"));

    executor.execute_line(&mut editor, "edit Xt69trail   ").unwrap();
    let buffer = editor.current_buffer().expect("a buffer is current");
    assert_eq!(
        editor
            .buffer(buffer)
            .expect("buffer exists")
            .name()
            .to_string_lossy(),
        "Xt69trail",
        "an unescaped trailing space is removed for a TRLBAR command"
    );
}

// ---------------------------------------------------------------------------
// `maparg()` and `:map {lhs}`, which read the same mapping state.
// Citations: `src/nvim/mapping.c` `get_maparg` (2148-2227),
// `mapblock_fill_dict` (2090-2146), `check_map` (2010-2061), `showmap`
// (211-275), `do_map`'s listing passes (698-793) and `map_add`'s script
// context (501-537); `src/nvim/message.c` `str2special` (2084-2166).
// Every expectation below is the observed answer of
// `.references/neovim/build/bin/nvim`, v0.13.0-dev-1390.
// ---------------------------------------------------------------------------

/// The messages a listing added, in order, starting at `from`. Slicing rather
/// than clearing keeps every assertion in one editor, so a later listing
/// cannot be fooled by state a reset would have discarded.
fn listing_rows(editor: &Editor, from: usize) -> Vec<String> {
    editor.messages()[from..]
        .iter()
        .map(|message| match &message.content {
            ox_types::Object::String(text) => text.to_string_lossy().into_owned(),
            other => panic!("expected a string message, got {other:?}"),
        })
        .collect()
}

/// Reads one key out of `maparg({lhs}, {mode}, 0, 1)`.
fn maparg_key(
    editor: &mut Editor,
    executor: &mut ExExecutor<MemoryFileIO>,
    lhs: &str,
    mode: &str,
    key: &str,
) -> Option<ox_types::Typval> {
    executor
        .execute_line(editor, &format!("let g:t71 = maparg('{lhs}', '{mode}', 0, 1)"))
        .unwrap();
    let dict = match global_value(executor, "t71") {
        Some(ox_types::Typval::Dict(dict)) => dict,
        other => panic!("maparg did not answer a dictionary: {other:?}"),
    };
    let entries = dict.borrow();
    entries
        .entries
        .iter()
        .find(|(name, _)| name.as_bytes() == key.as_bytes())
        .map(|(_, value)| value.clone())
}

/// `maparg()`'s compatible `rhs` is `m_orig_str`, the right-hand side *as
/// written*, while its string form is `str2special` of `m_str`, the replaced
/// form (`mapping.c:2114-2117,2200-2210`).
///
/// For a `:`-shaped right-hand side those are two different strings held in
/// two different places: parsing the command text into `Vec<ExCommand>` cannot
/// print back to either, so both have to be recorded at registration.
#[test]
fn maparg_answers_the_right_hand_side_as_written_and_as_replaced() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap ,a :let g:hit=1<CR>").unwrap();
    // `<lt>` decodes to `<`, so the two forms differ here as well.
    executor.execute_line(&mut editor, "nnoremap ,b a<lt>b").unwrap();
    executor.execute_line(&mut editor, "nnoremap ,c <Nop>").unwrap();

    // Oracle: `maparg(',a','n',0,1).rhs` is ':let g:hit=1<CR>' — the notation,
    // not the carriage return it decodes to.
    assert_eq!(
        maparg_key(&mut editor, &mut executor, ",a", "n", "rhs"),
        Some(ox_types::Typval::String(ox_types::OxStr::from(":let g:hit=1<CR>")))
    );
    assert_eq!(
        maparg_key(&mut editor, &mut executor, ",b", "n", "rhs"),
        Some(ox_types::Typval::String(ox_types::OxStr::from("a<lt>b")))
    );
    assert_eq!(
        maparg_key(&mut editor, &mut executor, ",c", "n", "rhs"),
        Some(ox_types::Typval::String(ox_types::OxStr::from("<Nop>")))
    );

    // The string form renders the replaced bytes instead. Oracle:
    // ':let g:hit=1<CR>', 'a<b' and '<Nop>'.
    executor
        .execute_line(&mut editor, "let g:s = maparg(',a','n') . '|' . maparg(',b','n') . '|' . maparg(',c','n')")
        .unwrap();
    assert_eq!(
        global_text(&executor, "s").as_deref(),
        Some(":let g:hit=1<CR>|a<b|<Nop>")
    );

    // A left-hand side nothing defines answers the empty string and, for the
    // dictionary form, an empty dictionary (`mapping.c:2219-2222`).
    executor.execute_line(&mut editor, "let g:miss = maparg(',zz','n')").unwrap();
    assert_eq!(global_text(&executor, "miss").as_deref(), Some(""));
    executor.execute_line(&mut editor, "let g:missd = len(maparg(',zz','n',0,1))").unwrap();
    assert_eq!(global_value(&executor, "missd"), Some(ox_types::Typval::Number(0)));
}

/// Each flag `mapblock_fill_dict` reports comes from a *different* field, so
/// each one is exercised on its own mapping: a test that set several at once
/// would pass with any single flag wired and the rest constant.
#[test]
fn maparg_reports_each_recorded_flag_independently() {
    let (mut editor, mut executor) = setup();
    for line in [
        "nnoremap ,plain yy",
        "nmap ,remap yy",
        "nnoremap <silent> ,silent yy",
        "nnoremap <nowait> ,nowait yy",
        "nnoremap <buffer> ,buffer yy",
        "nnoremap <expr> ,expr 'yy'",
        "nmap <script> ,script yy",
        "vnoremap ,visual yy",
        "inoremap ,insert yy",
        "noremap ,all yy",
    ] {
        executor.execute_line(&mut editor, line).unwrap();
    }

    // Oracle for `,plain`: every flag zero except `noremap`.
    for key in ["silent", "nowait", "buffer", "expr", "script", "abbr", "replace_keycodes"] {
        assert_eq!(
            maparg_key(&mut editor, &mut executor, ",plain", "n", key),
            Some(ox_types::Typval::Number(0)),
            "{key} should be 0 for a plain :nnoremap"
        );
    }
    let one = Some(ox_types::Typval::Number(1));
    assert_eq!(maparg_key(&mut editor, &mut executor, ",plain", "n", "noremap"), one);
    assert_eq!(
        maparg_key(&mut editor, &mut executor, ",remap", "n", "noremap"),
        Some(ox_types::Typval::Number(0)),
        ":nmap remaps, so noremap is 0"
    );
    assert_eq!(maparg_key(&mut editor, &mut executor, ",silent", "n", "silent"), one);
    assert_eq!(maparg_key(&mut editor, &mut executor, ",nowait", "n", "nowait"), one);
    assert_eq!(maparg_key(&mut editor, &mut executor, ",buffer", "n", "buffer"), one);
    assert_eq!(maparg_key(&mut editor, &mut executor, ",expr", "n", "expr"), one);
    // `<script>` is `REMAP_SCRIPT`: `script` is 1 *and* the compatible
    // `noremap` is 1, which is the pair that tells it from `:noremap`.
    assert_eq!(maparg_key(&mut editor, &mut executor, ",script", "n", "script"), one);
    assert_eq!(maparg_key(&mut editor, &mut executor, ",script", "n", "noremap"), one);
    assert_eq!(
        maparg_key(&mut editor, &mut executor, ",plain", "n", "script"),
        Some(ox_types::Typval::Number(0)),
        ":noremap without <script> reports script 0 with the same noremap 1"
    );
    // `scriptversion` is hard-coded to 1 upstream as well.
    assert_eq!(maparg_key(&mut editor, &mut executor, ",plain", "n", "scriptversion"), one);

    // `mode` and `mode_bits` are `map_mode_to_chars` and the raw `MODE_*` set.
    // Oracle: 'n'/1, 'v'/66, 'i'/16, ' '/71.
    for (lhs, mode, chars, bits) in [
        (",plain", "n", "n", 1),
        (",visual", "v", "v", 66),
        (",insert", "i", "i", 16),
        (",all", "n", " ", 71),
    ] {
        assert_eq!(
            maparg_key(&mut editor, &mut executor, lhs, mode, "mode"),
            Some(ox_types::Typval::String(ox_types::OxStr::from(chars))),
            "mode chars for {lhs}"
        );
        assert_eq!(
            maparg_key(&mut editor, &mut executor, lhs, mode, "mode_bits"),
            Some(ox_types::Typval::Number(bits)),
            "mode bits for {lhs}"
        );
    }
}

/// `map_add` copies `current_sctx` and adds `SOURCING_LNUM` to its line
/// (`mapping.c:530-537`). Inside a function body those are two different
/// numbers — the `:function`'s own line and the body-relative line — so the
/// test puts the `:map` on body line two of a function defined on line three,
/// where neither addend alone produces the answer.
#[test]
fn maparg_lnum_inside_a_function_adds_the_body_line_to_the_definition_line() {
    let (mut editor, mut executor) = setup();
    executor
        .execute_script(
            &mut editor,
            "t71.vim",
            "let g:one = 1\nlet g:two = 2\nfunc! T71M()\n  let g:three = 3\n  nnoremap ,f qq\nendfunc\ncall T71M()\nnnoremap ,g qq",
        )
        .unwrap();

    // Oracle: definition line 3 plus body line 2 is 5.
    assert_eq!(
        maparg_key(&mut editor, &mut executor, ",f", "n", "lnum"),
        Some(ox_types::Typval::Number(5))
    );
    // At script level the physical line is the whole answer.
    assert_eq!(
        maparg_key(&mut editor, &mut executor, ",g", "n", "lnum"),
        Some(ox_types::Typval::Number(8))
    );
}

/// `do_map` prints instead of defining when the right-hand side is missing
/// (`mapping.c:873-883`), and `showmap` lays each row out as three mode
/// columns, the lhs padded past twelve, the remap marker, the buffer-local
/// marker, then the rhs.
#[test]
fn map_lists_matching_mappings_and_says_so_when_none_match() {
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap ,a :let g:hit=1<CR>").unwrap();
    executor.execute_line(&mut editor, "nmap ,b ,a").unwrap();
    executor.execute_line(&mut editor, "nmap <script> ,c qq").unwrap();
    executor.execute_line(&mut editor, "nnoremap ,d <Nop>").unwrap();

    // Oracle: `execute('nmap ,a')` is "\n\nn  ,a          * :let g:hit=1<CR>",
    // which is one blank leading row and one mapping row.
    let mut seen = editor.messages().len();
    executor.execute_line(&mut editor, "nmap ,a").unwrap();
    assert_eq!(
        listing_rows(&editor, seen),
        vec![String::new(), "n  ,a          * :let g:hit=1<CR>".to_owned()]
    );

    // A remapping mapping has a blank where `*` was; `<script>` has `&`;
    // `<Nop>` prints as the literal `<Nop>` because `m_str` is empty.
    for (lhs, expected) in [
        (",b", "n  ,b            ,a"),
        (",c", "n  ,c          & qq"),
        (",d", "n  ,d          * <Nop>"),
    ] {
        seen = editor.messages().len();
        executor.execute_line(&mut editor, &format!("nmap {lhs}")).unwrap();
        assert_eq!(listing_rows(&editor, seen)[1], expected);
    }

    // `msg`, not `emsg` (`mapping.c:879`): no match is a message, and the
    // command still succeeds.
    seen = editor.messages().len();
    executor.execute_line(&mut editor, "nmap ,zz").unwrap();
    assert_eq!(
        listing_rows(&editor, seen),
        vec![String::new(), "No mapping found".to_owned()]
    );

    // A shorter lhs matches every mapping it prefixes, because upstream
    // compares only `min(keylen, len)` bytes (`mapping.c:769`).
    seen = editor.messages().len();
    executor.execute_line(&mut editor, "nmap ,").unwrap();
    assert_eq!(listing_rows(&editor, seen).len(), 5, "four mappings and the blank row");
}

/// The listing order is two independent rules — the buffer-local table is
/// walked before the global one (`mapping.c:698-726`), and within a table a
/// bucket is newest-first because `map_add` pushes onto its head
/// (`mapping.c:545-547`) — plus the buckets themselves ascending by first lhs
/// byte. Each is exercised where the others cannot decide it.
#[test]
fn map_lists_buffer_local_mappings_first_and_each_bucket_newest_first() {
    // Newest-first, with every mapping in one scope and one bucket so neither
    // locality nor bucket order decides anything.
    let (mut editor, mut executor) = setup();
    for lhs in [",p", ",q", ",r"] {
        executor.execute_line(&mut editor, &format!("nnoremap {lhs} qq")).unwrap();
    }
    let mut seen = editor.messages().len();
    executor.execute_line(&mut editor, "nmap ,").unwrap();
    let rows = listing_rows(&editor, seen);
    assert_eq!(
        rows[1..].iter().map(|row| row[3..5].to_owned()).collect::<Vec<_>>(),
        vec![",r".to_owned(), ",q".to_owned(), ",p".to_owned()]
    );

    // Locality, with the *oldest* mapping buffer-local so newest-first alone
    // would put it last.
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap <buffer> ,p qq").unwrap();
    executor.execute_line(&mut editor, "nnoremap ,q qq").unwrap();
    seen = editor.messages().len();
    executor.execute_line(&mut editor, "nmap ,").unwrap();
    let rows = listing_rows(&editor, seen);
    assert_eq!(rows[1], "n  ,p          *@qq", "the local mapping comes first and is marked @");
    assert_eq!(rows[2], "n  ,q          * qq");

    // Buckets ascend by first lhs byte. `+z` is defined *first*, so the
    // newest-first rule on its own would put `,z` ahead of it; only the bucket
    // key produces this order.
    let (mut editor, mut executor) = setup();
    executor.execute_line(&mut editor, "nnoremap +z qq").unwrap();
    executor.execute_line(&mut editor, "nnoremap ,z qq").unwrap();
    seen = editor.messages().len();
    executor.execute_line(&mut editor, "nmap").unwrap();
    let rows = listing_rows(&editor, seen);
    assert_eq!(rows[1][3..5], *"+z");
    assert_eq!(rows[2][3..5], *",z");
}

/// `prompt_setprompt()` on an ordinary buffer rewrites only the stored prompt
/// text: visible lines move solely while 'buftype' is "prompt"
/// (`f_prompt_setprompt`, `eval/buffer.c:963-1014`).
#[test]
fn prompt_setprompt_leaves_ordinary_buffer_first_line_unchanged() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"keep this line".to_vec(), b"second".to_vec()]);
    let buffer = editor.current_buffer().unwrap();
    let state = editor.buffer_mut(buffer).unwrap();
    let namespace = state.extmarks.create_namespace("ordinary").unwrap();
    let anchored = state
        .set_extmark_recorded(
            namespace,
            None,
            crate::ExtmarkPlacement::new(crate::ExtmarkPosition::new(0, 0)),
            false,
        )
        .unwrap();

    executor
        .execute_line(&mut editor, "call prompt_setprompt(0, 'cmd: ')")
        .unwrap();

    let state = editor.buffer(buffer).unwrap();
    assert_eq!(
        state.text().unwrap().line(1).unwrap(),
        b"keep this line".to_vec(),
        "an ordinary buffer's first line is unchanged"
    );
    assert_eq!(state.text().unwrap().line(2).unwrap(), b"second".to_vec());
    assert_eq!(state.prompt(), b"cmd: ", "the prompt is still stored");
    assert_eq!(
        state.extmarks.get(namespace, anchored).unwrap().unwrap().placement.position,
        crate::ExtmarkPosition::new(0, 0),
        "no splice reaches the extmark"
    );
}

/// A prompt on a later line is replaced on its state-owned live row. Stale
/// prompt-change marks and cursor placement cannot redirect the mutation into
/// scrollback; the same row drives replacement and extmark splice geometry
/// (f_prompt_setprompt, eval/buffer.c:974-1000).
#[test]
fn prompt_setprompt_replaces_live_prompt_line_with_exact_extmark_movement() {
    let (mut editor, mut executor) = setup_with_content(&[
        b"scrollback above".to_vec(),
        b"cmd: input".to_vec(),
        b"tail".to_vec(),
    ]);
    let buffer = editor.current_buffer().unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "buftype", crate::options::OptionValue::String("prompt".to_owned()))
        .unwrap();
    editor
        .buffer_mut(buffer)
        .unwrap()
        .set_prompt(b"cmd: ".to_vec(), 2);
    editor
        .set_local_mark(buffer, ':', ox_text::Position { lnum: 9, col: 999 })
        .unwrap();
    let state = editor.buffer_mut(buffer).unwrap();
    let namespace = state.extmarks.create_namespace("live-prompt").unwrap();
    let end_of_input = state
        .set_extmark_recorded(
            namespace,
            None,
            crate::ExtmarkPlacement::new(crate::ExtmarkPosition::new(1, 10)),
            false,
        )
        .unwrap();
    let spanning = state
        .set_extmark_recorded(
            namespace,
            None,
            crate::ExtmarkPlacement::new(crate::ExtmarkPosition::new(1, 5))
                .with_end(crate::ExtmarkPosition::new(1, 10)),
            false,
        )
        .unwrap();
    let above = state
        .set_extmark_recorded(
            namespace,
            None,
            crate::ExtmarkPlacement::new(crate::ExtmarkPosition::new(0, 7)),
            false,
        )
        .unwrap();
    let below = state
        .set_extmark_recorded(
            namespace,
            None,
            crate::ExtmarkPlacement::new(crate::ExtmarkPosition::new(2, 0)),
            false,
        )
        .unwrap();

    executor
        .execute_line(&mut editor, "call prompt_setprompt(0, 'floob: ')")
        .unwrap();

    let state = editor.buffer(buffer).unwrap();
    assert_eq!(
        state.text().unwrap().line(1).unwrap(),
        b"scrollback above".to_vec(),
        "the line above the prompt is untouched"
    );
    assert_eq!(
        state.text().unwrap().line(2).unwrap(),
        b"floob: input".to_vec(),
        "only the prompt prefix on the live prompt/input line is replaced"
    );
    assert_eq!(
        state.text().unwrap().line(3).unwrap(),
        b"tail".to_vec(),
        "the line below the prompt is untouched"
    );
    let marks = &state.extmarks;
    assert_eq!(
        marks.get(namespace, end_of_input).unwrap().unwrap().placement.position,
        crate::ExtmarkPosition::new(1, 12),
        "the end-of-input mark shifts by the prompt byte delta"
    );
    let spanned = marks.get(namespace, spanning).unwrap().unwrap();
    assert_eq!(spanned.placement.position, crate::ExtmarkPosition::new(1, 7));
    assert_eq!(
        spanned.placement.end.map(|end| end.position),
        Some(crate::ExtmarkPosition::new(1, 12))
    );
    assert_eq!(
        marks.get(namespace, above).unwrap().unwrap().placement.position,
        crate::ExtmarkPosition::new(0, 7),
        "marks on other lines do not move"
    );
    assert_eq!(
        marks.get(namespace, below).unwrap().unwrap().placement.position,
        crate::ExtmarkPosition::new(2, 0),
        "marks on other lines do not move"
    );

    // A stale prompt-change mark cannot redirect the next update into
    // scrollback: the state-owned prompt row remains authoritative.
    editor
        .set_local_mark(buffer, ':', ox_text::Position { lnum: 1, col: 999 })
        .unwrap();
    executor
        .execute_line(&mut editor, "call prompt_setprompt(0, 'next: ')")
        .unwrap();
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(
        state.text().unwrap().line(1).unwrap(),
        b"scrollback above".to_vec(),
        "scrollback remains byte-identical despite the stale mark"
    );
    assert_eq!(state.text().unwrap().line(2).unwrap(), b"next: input".to_vec());
    assert_eq!(state.text().unwrap().line(3).unwrap(), b"tail".to_vec());
    assert_eq!(
        state.marks.get(':').unwrap(),
        Some(ox_text::Position { lnum: 2, col: 6 }),
        "the prompt boundary returns to the authoritative live row"
    );
}

#[test]
fn prompt_setprompt_stores_unloaded_prompt_without_resident_text() {
    let (mut editor, mut executor) = setup();
    let buffer = editor
        .create_buffer_with(
            Buffer::from_lines(&[b"scrollback".to_vec(), b"old: input".to_vec()], true).unwrap(),
            true,
        )
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "buftype", crate::options::OptionValue::String("prompt".to_owned()))
        .unwrap();
    editor
        .buffer_mut(buffer)
        .unwrap()
        .set_prompt(b"old: ".to_vec(), 2);
    editor.unload_buffer(buffer).unwrap();

    executor
        .execute_line(
            &mut editor,
            &format!("call prompt_setprompt({}, 'stored: ')", i64::from(buffer)),
        )
        .unwrap();

    let state = editor.buffer(buffer).unwrap();
    assert!(!state.loaded);
    assert_eq!(state.prompt(), b"stored: ");
    assert!(state.text().is_err(), "unloaded assignment does not synthesize resident text");
}

#[test]
fn loading_shorter_prompt_buffer_clamps_authoritative_prompt_row() {
    let mut state = crate::BufferState::new(
        Buffer::from_lines(
            &[b"one".to_vec(), b"two".to_vec(), b"old: input".to_vec()],
            true,
        )
        .unwrap(),
        true,
    );
    state.set_prompt(b"old: ".to_vec(), 3);
    state.unload().unwrap();

    state.load(Buffer::from_lines(&[b"replacement".to_vec()], true).unwrap());

    assert_eq!(state.prompt_start(), 1);
    assert_eq!(state.text().unwrap().line(1).unwrap(), b"replacement".to_vec());
}

#[test]
fn prompt_replacement_stays_out_of_undo_without_clearing_earlier_change() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"before".to_vec(), b"old: input".to_vec()]);
    let buffer = editor.current_buffer().unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "buftype", crate::options::OptionValue::String("prompt".to_owned()))
        .unwrap();
    editor
        .buffer_mut(buffer)
        .unwrap()
        .set_prompt(b"old: ".to_vec(), 2);
    editor
        .set_local_mark(buffer, ':', ox_text::Position { lnum: 2, col: 5 })
        .unwrap();
    executor
        .execute_line(&mut editor, "call setline(1, 'edited')")
        .unwrap();
    editor.sync_buffer_undo(buffer);

    executor
        .execute_line(&mut editor, "call prompt_setprompt(0, 'new: ')")
        .unwrap();
    executor.execute_line(&mut editor, "undo").unwrap();

    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.text().unwrap().line(1).unwrap(), b"before".to_vec());
    assert_eq!(
        state.text().unwrap().line(2).unwrap(),
        b"new: input".to_vec(),
        "undoing the earlier edit cannot restore the old prompt"
    );
}

#[test]
fn prompt_setprompt_col999_discards_input_with_exact_extmark_geometry() {
    let (mut editor, mut executor) = setup_with_content(&[b"input".to_vec()]);
    let buffer = editor.current_buffer().unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "buftype", crate::options::OptionValue::String("prompt".to_owned()))
        .unwrap();
    let state = editor.buffer_mut(buffer).unwrap();
    let namespace = state.extmarks.create_namespace("prompt-col999").unwrap();
    let mark = state
        .set_extmark_recorded(
            namespace,
            None,
            crate::ExtmarkPlacement::new(crate::ExtmarkPosition::new(0, 5)),
            false,
        )
        .unwrap();
    let changedtick = state.changedtick();

    executor
        .execute_line(&mut editor, r#"call setpos("':", [0, 1, 999, 0])"#)
        .unwrap();
    executor
        .execute_line(&mut editor, "call prompt_setprompt(0, 'discard > ')")
        .unwrap();

    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.text().unwrap().line(1).unwrap(), b"discard > ".to_vec());
    assert_eq!(
        state.extmarks.get(namespace, mark).unwrap().unwrap().placement.position,
        crate::ExtmarkPosition::new(0, 10)
    );
    assert_eq!(
        state.marks.get(':').unwrap(),
        Some(ox_text::Position { lnum: 1, col: 10 })
    );
    assert_eq!(state.changedtick(), changedtick + 1);
}
/// An empty stored prompt means the default `% `; setting a new prompt on
/// a prompt buffer with plain input discards the input, while input that
/// already carries the default prefix is preserved.
#[test]
fn prompt_setprompt_discards_plain_input_without_default_prefix() {
    let (mut editor, mut executor) = setup_with_content(&[b"input".to_vec()]);
    let buffer = editor.current_buffer().unwrap();
    editor
        .options_mut()
        .set_buffer(
            buffer,
            "buftype",
            crate::options::OptionValue::String("prompt".to_owned()),
        )
        .unwrap();

    executor
        .execute_line(&mut editor, "call prompt_setprompt(0, 'cmd: ')")
        .unwrap();

    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.text().unwrap().line(1).unwrap(), b"cmd: ".to_vec());
    assert_eq!(state.prompt(), b"cmd: ");
}

#[test]
fn prompt_setprompt_replaces_default_prefix_and_preserves_input() {
    let (mut editor, mut executor) = setup_with_content(&[b"% second".to_vec()]);
    let buffer = editor.current_buffer().unwrap();
    editor
        .options_mut()
        .set_buffer(
            buffer,
            "buftype",
            crate::options::OptionValue::String("prompt".to_owned()),
        )
        .unwrap();

    executor
        .execute_line(&mut editor, "call prompt_setprompt(0, 'cmd: ')")
        .unwrap();

    let state = editor.buffer(buffer).unwrap();
    assert_eq!(
        state.text().unwrap().line(1).unwrap(),
        b"cmd: second".to_vec()
    );
    assert_eq!(state.prompt(), b"cmd: ");
}

/// `prompt_getprompt()` returns the effective prompt text, reporting the
/// default `% ` for an empty stored prefix on a prompt buffer.
#[test]
fn prompt_getprompt_reports_effective_default() {
    let (mut editor, mut executor) = setup();
    let buffer = editor.current_buffer().unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "buftype", crate::options::OptionValue::String("prompt".to_owned()))
        .unwrap();

    executor
        .execute_line(&mut editor, "let g:p = prompt_getprompt(0)")
        .unwrap();
    assert_eq!(
        global_value(&executor, "p"),
        Some(ox_types::Typval::String(ox_types::OxStr::from("% "))),
        "empty stored prompt returns the default `% `"
    );

    executor
        .execute_line(&mut editor, "call prompt_setprompt(0, 'cmd: ')")
        .unwrap();
    executor
        .execute_line(&mut editor, "let g:p = prompt_getprompt(0)")
        .unwrap();
    assert_eq!(
        global_value(&executor, "p"),
        Some(ox_types::Typval::String(ox_types::OxStr::from("cmd: ")))
    );
}

/// When the special `':'` mark records a previous complete prompt/input
/// prefix longer than the stored prompt text, replacement splices that
/// entire prefix (`f_prompt_setprompt` happy path: `mark.col`).
#[test]
fn prompt_replace_geometry_uses_complete_mark_prefix() {
    let (mut editor, mut executor) =
        setup_with_content(&[b"xxxxcmd: leftover".to_vec()]);
    let buffer = editor.current_buffer().unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "buftype", crate::options::OptionValue::String("prompt".to_owned()))
        .unwrap();
    editor
        .buffer_mut(buffer)
        .unwrap()
        .set_prompt(b"cmd: ".to_vec(), 1);
    editor
        .set_local_mark(buffer, ':', ox_text::Position { lnum: 1, col: 9 })
        .unwrap();
    let state = editor.buffer_mut(buffer).unwrap();
    let namespace = state.extmarks.create_namespace("complete-prefix").unwrap();
    let leftover = state
        .set_extmark_recorded(
            namespace,
            None,
            crate::ExtmarkPlacement::new(crate::ExtmarkPosition::new(0, 9)),
            false,
        )
        .unwrap();
    let inside_owned = state
        .set_extmark_recorded(
            namespace,
            None,
            crate::ExtmarkPlacement::new(crate::ExtmarkPosition::new(0, 6)),
            false,
        )
        .unwrap();

    executor
        .execute_line(&mut editor, "call prompt_setprompt(0, 'next: ')")
        .unwrap();

    let state = editor.buffer(buffer).unwrap();
    assert_eq!(
        state.text().unwrap().line(1).unwrap(),
        b"next: leftover".to_vec(),
        "the complete previous prompt/input prefix is replaced, not a suffix of the stored prompt"
    );
    assert_eq!(
        state.extmarks.get(namespace, leftover).unwrap().unwrap().placement.position,
        crate::ExtmarkPosition::new(0, 6)
    );
    assert_eq!(
        state.extmarks.get(namespace, inside_owned).unwrap().unwrap().placement.position,
        crate::ExtmarkPosition::new(0, 6)
    );
    assert_eq!(
        state.marks.get(':').unwrap(),
        Some(ox_text::Position { lnum: 1, col: 6 })
    );
}
