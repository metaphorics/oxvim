//! `bufload()` file-read lifecycle pinning.
//!
//! The four read outcomes and the single event family each one may fire
//! (`call_bufload_with_events`, upstream `readfile`, `.references/neovim/
//! src/nvim/fileio.c:428-516`):
//!
//! | read outcome                    | events                      |
//! |---------------------------------|-----------------------------|
//! | probe and read succeed          | `BufReadPre`, `BufReadPost` |
//! | file missing (`NotFound`)       | `BufNewFile`                |
//! | unreadable or non-regular name  | none                        |
//! | read fails after a good probe   | none (upstream's E200)      |

use crate::excmd_exec::ExExecutor;
use crate::script::{FileIO, RealFileIO};
use crate::{AutocmdKind, AutocmdOptions, Event};
use ox_types::{OxStr, Typval};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// In-memory [`FileIO`] that can force a `read_to_string` failure on demand:
/// `locked` paths fail with `PermissionDenied` (present but unreadable), and
/// `fail_next` fails the nth read, where 1 counts from the first `bufload()`
/// read — so `fail_next = 2` lets the probe succeed and kills the reload
/// after `BufReadPre`.
#[derive(Clone, Default)]
struct FaultFileIO {
    files: Rc<RefCell<HashMap<PathBuf, String>>>,
    locked: Rc<RefCell<Vec<PathBuf>>>,
    fail_next: Rc<RefCell<usize>>,
}

impl FaultFileIO {
    fn insert(&self, path: &str, content: &str) {
        self.files
            .borrow_mut()
            .insert(PathBuf::from(path), content.to_owned());
    }

    /// Forces every read of `path` to fail as unreadable.
    fn lock(&self, path: &str) {
        self.locked.borrow_mut().push(PathBuf::from(path));
    }

    /// Fails the read that is `count` calls away, counting from 1.
    fn fail_read(&self, count: usize) {
        *self.fail_next.borrow_mut() = count;
    }
}

impl FileIO for FaultFileIO {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        if self
            .locked
            .borrow()
            .iter()
            .any(|locked| locked.as_path() == path)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("unreadable: {}", path.display()),
            ));
        }
        let contents = self
            .files
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file not found"))?;
        let mut fail_next = self.fail_next.borrow_mut();
        if *fail_next == 1 {
            return Err(io::Error::other("read failed after the probe"));
        }
        if *fail_next > 1 {
            *fail_next -= 1;
        }
        Ok(contents)
    }

    fn write_string(&self, path: &Path, contents: &str) -> io::Result<()> {
        self.files
            .borrow_mut()
            .insert(path.to_path_buf(), contents.to_owned());
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.borrow().contains_key(path)
            && !self
                .locked
                .borrow()
                .iter()
                .any(|locked| locked.as_path() == path)
    }

    fn canonicalize(&self, path: &Path) -> PathBuf {
        path.to_path_buf()
    }
}

/// One editor plus the injected [`FileIO`] store, with the three
/// `BufRead*`-family events recorded into `g:order` by a plugin-shaped autocmd.
fn setup_with_io<F: FileIO>(io: F) -> (crate::TestEditorAccess, ExExecutor<F>) {
    let mut editor = crate::Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor
        .create_tabpage(buffer, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let access = crate::TestEditorAccess::new(editor);
    let mut executor = ExExecutor::with_io(io);
    executor.execute_line(&access, "let g:order = []").unwrap();
    for event in ["BufReadPre", "BufReadPost", "BufNewFile"] {
        executor
            .execute_line(
                &access,
                &format!("autocmd {event} * call add(g:order, '{event}')"),
            )
            .unwrap();
    }
    (access, executor)
}

/// The list value of one global; every pinned value here is `g:`-scoped so
/// the assertion reads the same surface a plugin would.
fn global_list<F: FileIO>(executor: &ExExecutor<F>, name: &str) -> Vec<String> {
    let Some((_, value)) = executor
        .scope()
        .global
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
    else {
        panic!("g:{name} was not assigned");
    };
    let Typval::List(items) = value else {
        panic!("g:{name} is not a list: {value:?}");
    };
    items
        .borrow()
        .items
        .iter()
        .map(|item| match item {
            Typval::String(text) => text.to_string_lossy().into_owned(),
            other => panic!("g:{name} holds a non-string: {other:?}"),
        })
        .collect()
}

/// The lifecycle events `bufload()` fired, in order.
fn order_events<F: FileIO>(executor: &ExExecutor<F>) -> Vec<String> {
    global_list(executor, "order")
}

/// The text `bufload()` left in the loaded buffer.
fn buffer_lines<F: FileIO>(executor: &ExExecutor<F>) -> Vec<String> {
    global_list(executor, "lines")
}

fn load<F: FileIO>(
    executor: &mut ExExecutor<F>,
    access: &crate::TestEditorAccess,
    name: &str,
) {
    executor
        .execute_line(access, &format!("let g:buf = bufadd('{name}') | call bufload(g:buf)"))
        .unwrap();
    executor
        .execute_line(access, "let g:lines = getbufline(g:buf, 1, '$')")
        .unwrap();
}

/// The probe reads before `BufReadPre` and the reload reads after it; the
/// success family only fires when both went through.
#[test]
fn bufload_successful_read_fires_read_pre_then_read_post() {
    let io = FaultFileIO::default();
    io.insert("Xpresent", "one\ntwo\n");
    let (access, mut executor) = setup_with_io(io);
    load(&mut executor, &access, "Xpresent");

    assert_eq!(order_events(&executor), ["BufReadPre", "BufReadPost"]);
    assert_eq!(buffer_lines(&executor), ["one", "two"]);
}

/// A present-but-unreadable file loads empty and fires nothing: upstream
/// `readfile` exits with E200 before any event and `open_buffer` then fires
/// no post event (`fileio.c:516`).
#[test]
fn bufload_unreadable_file_fires_no_event() {
    let io = FaultFileIO::default();
    io.insert("Xlocked", "one\n");
    io.lock("Xlocked");
    let (access, mut executor) = setup_with_io(io);
    load(&mut executor, &access, "Xlocked");

    assert_eq!(order_events(&executor), Vec::<String>::new());
    assert_eq!(buffer_lines(&executor), [""]);
}

/// The reload after `BufReadPre` can fail even though the probe succeeded;
/// like upstream's E200 exit it fires no post event. The fault is aimed at
/// the second read so the probe itself succeeds.
#[test]
fn bufload_failed_read_after_successful_probe_fires_no_read_post() {
    let io = FaultFileIO::default();
    io.insert("Xvanishing", "one\n");
    io.fail_read(2);
    let (access, mut executor) = setup_with_io(io);
    load(&mut executor, &access, "Xvanishing");

    assert_eq!(
        order_events(&executor),
        ["BufReadPre"],
        "the pre event fired, the failed reload fired nothing"
    );
    assert_eq!(buffer_lines(&executor), [""]);
}

/// `bufload()` of a named normal buffer whose file is missing fires
/// `BufNewFile` and loads empty — never the read family.
#[test]
fn bufload_missing_file_fires_bufnewfile_only() {
    let (access, mut executor) = setup_with_io(FaultFileIO::default());
    load(&mut executor, &access, "Xabsent");

    assert_eq!(order_events(&executor), ["BufNewFile"]);
    assert_eq!(buffer_lines(&executor), [""]);
}

/// `bufload()` on a buffer whose name holds bytes that are not valid UTF-8
/// probes and reads the exact Unix path — a real file stored under a raw
/// 0xff name — so the read family fires with the file's text. A lossy
/// reconstruction of the name would miss the file and take the `BufNewFile`
/// path with empty text while keeping the raw name.
#[cfg(unix)]
#[test]
fn bufload_reads_file_whose_name_is_not_utf8() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let root = std::env::temp_dir().join(format!("ox-bufload-bytes-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(OsStr::from_bytes(b"Xbytes-\xff")), "one\ntwo\n").unwrap();
    let (access, mut executor) = setup_with_io(RealFileIO);
    // Single-quoted Vimscript strings keep backslashes literal, so the raw
    // byte enters through the double-quoted `\xff` escape.
    let dir = root.to_str().expect("temp root is valid UTF-8");
    executor
        .execute_line(
            &access,
            &format!("let g:buf = bufadd(\"{dir}/Xbytes-\\xff\") | call bufload(g:buf)"),
        )
        .unwrap();
    executor
        .execute_line(&access, "let g:lines = getbufline(g:buf, 1, '$')")
        .unwrap();

    assert_eq!(order_events(&executor), ["BufReadPre", "BufReadPost"]);
    assert_eq!(buffer_lines(&executor), ["one", "two"]);
    let _ = std::fs::remove_dir_all(&root);
}

/// A `BufReadPre` action can synchronously call another editor-stateful
/// builtin. Three buffer-local handlers exercise two nested `bufload()` calls
/// and an editor query at the third level; a leaked borrow guard would panic
/// before the final text and lifecycle sequence are observed.
#[test]
fn bufload_read_pre_handler_reenters_two_levels() {
    let io = FaultFileIO::default();
    io.insert("Xouter", "outer\n");
    io.insert("Xmiddle", "middle\n");
    io.insert("Xinner", "inner\n");
    let (access, mut executor) = setup_with_io(io);
    let (outer, middle, inner) = {
        let mut editor = access.editor_mut();
        (
            super::add_unloaded_buffer(&mut editor, &OxStr::from("Xouter")).unwrap(),
            super::add_unloaded_buffer(&mut editor, &OxStr::from("Xmiddle")).unwrap(),
            super::add_unloaded_buffer(&mut editor, &OxStr::from("Xinner")).unwrap(),
        )
    };

    for (buffer, next) in [(outer, middle), (middle, inner)] {
        access
            .editor_mut()
            .autocmds_mut()
            .register_legacy(
                &[Event::BufReadPre],
                "<buffer>",
                &AutocmdKind::ExString(format!("call bufload({})", i64::from(next))),
                &AutocmdOptions {
                    buffer: Some(buffer),
                    nested: true,
                    ..AutocmdOptions::default()
                },
            )
            .unwrap();
    }
    access
        .editor_mut()
        .autocmds_mut()
        .register_legacy(
            &[Event::BufReadPre],
            "<buffer>",
            &AutocmdKind::ExString(format!("let g:deep = [bufname({})]", i64::from(outer))),
            &AutocmdOptions {
                buffer: Some(inner),
                nested: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();

    executor
        .execute_line(&access, &format!("call bufload({})", i64::from(outer)))
        .unwrap();

    assert_eq!(
        order_events(&executor),
        [
            "BufReadPre",
            "BufReadPre",
            "BufReadPre",
            "BufReadPost",
            "BufReadPost",
            "BufReadPost"
        ]
    );
    assert_eq!(global_list(&executor, "deep"), ["Xouter"]);

    for (buffer, expected) in [(outer, "outer"), (middle, "middle"), (inner, "inner")] {
        executor
            .execute_line(
                &access,
                &format!("let g:lines = getbufline({}, 1, '$')", i64::from(buffer)),
            )
            .unwrap();
        assert_eq!(buffer_lines(&executor), [expected]);
    }
}
