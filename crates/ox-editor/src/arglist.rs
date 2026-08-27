//! The global argument list: state for `:args`/`:next`/`:argdo` and the
//! `argc()`/`argv()`/`argidx()`/`arglistid()` builtins.
//!
//! Semantics follow Neovim `src/nvim/arglist.c`: `global_alist` holds the
//! names and the current index (upstream `w_arg_idx`; every Oxvim window
//! shares the single global list, so the index lives beside the names and
//! `arglistid()` is always the global list's id `0`). Window-local lists
//! (`:arglocal`) are not modeled; the window-argument forms of the builtins
//! still resolve windows and report the shared list.

use ox_eval::builtins::builtin_spec;
use ox_eval::error::{EvalError, Result};
use ox_types::{OxStr, Typval, WinHandle};

use crate::editor::Editor;

/// Error from an out-of-range argument-list target (`do_argfile`
/// arglist.c 606-616: one entry reports E163, before-first E164,
/// beyond-last E165).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArgRangeError {
    /// Vim error code.
    pub code: &'static str,
    /// Vim error message.
    pub message: &'static str,
}

/// The global argument list (`arglist.c` `global_alist`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArgList {
    names: Vec<OxStr>,
    index: usize,
}

impl ArgList {
    /// Creates an empty list; the index of an empty list is zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries (`argc()`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether the list has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// All entry names in order.
    #[must_use]
    pub fn names(&self) -> &[OxStr] {
        &self.names
    }

    /// The name at `index`, if present.
    #[must_use]
    pub fn name(&self, index: usize) -> Option<&OxStr> {
        self.names.get(index)
    }

    /// Index of the current entry (`argidx()`); zero for an empty list.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index.min(self.names.len().saturating_sub(1))
    }

    /// Redefines the whole list (`do_arglist` AL_SET): the index restarts
    /// at the first entry, matching `alist_set` after a clear.
    pub fn set(&mut self, names: Vec<OxStr>) {
        self.names = names;
        self.index = 0;
    }

    /// Moves the current index; callers bounds-check first
    /// ([`ArgList::check_target`]).
    pub fn set_index(&mut self, index: usize) {
        self.index = index;
    }

    /// Bounds-checks a target index the way `do_argfile` does and returns
    /// the entry index.
    ///
    /// # Errors
    /// E163 when the list holds at most one entry, E164 before the first
    /// entry, E165 beyond the last entry.
    pub fn check_target(&self, target: i64) -> std::result::Result<usize, ArgRangeError> {
        if target < 0 || target >= self.names.len() as i64 {
            if self.names.len() <= 1 {
                return Err(ArgRangeError { code: "E163", message: "There is only one file to edit" });
            }
            if target < 0 {
                return Err(ArgRangeError { code: "E164", message: "Cannot go before first file" });
            }
            return Err(ArgRangeError { code: "E165", message: "Cannot go beyond last file" });
        }
        Ok(target as usize)
    }
}

/// Whether `name` is an argument-list builtin served from editor state.
pub(crate) fn is_arglist_builtin(name: &str) -> bool {
    matches!(name, "argc" | "argv" | "argidx" | "arglistid")
}

/// Dispatches `argc`/`argv`/`argidx`/`arglistid` against the editor's
/// argument list. Arity comes from the generated `eval.lua` metadata.
pub(crate) fn call(editor: &Editor, name: &str, args: Vec<Typval>) -> Result<Typval> {
    check_arity(name, args.len())?;
    match name {
        "argc" => argc(editor, &args),
        "argv" => argv(editor, &args),
        "argidx" => Ok(Typval::Number(editor.arglist().index() as i64)),
        "arglistid" => arglistid(editor, &args),
        _ => unreachable!("arglist builtin predicate and dispatcher disagree"),
    }
}

fn check_arity(name: &str, count: usize) -> Result<()> {
    let spec = builtin_spec(name).ok_or_else(|| EvalError::not_implemented(OxStr::from(name)))?;
    if count < spec.min_args {
        return Err(EvalError::new("E119", 0, format!("Not enough arguments for function: {name}")));
    }
    if spec.max_args.is_some_and(|maximum| count > maximum) {
        return Err(EvalError::new("E118", 0, format!("Too many arguments for function: {name}")));
    }
    Ok(())
}

fn argc(editor: &Editor, args: &[Typval]) -> Result<Typval> {
    // f_argc (arglist.c 1201): no argument or -1 reports the global list's
    // count; every resolvable window shares that list here, and an
    // unresolvable window argument reports -1.
    if let Some(value) = args.first() {
        if number_arg(value)? != -1 && resolve_window(editor, value).is_none() {
            return Ok(Typval::Number(-1));
        }
    }
    Ok(Typval::Number(editor.arglist().len() as i64))
}

fn argv(editor: &Editor, args: &[Typval]) -> Result<Typval> {
    // f_argv (arglist.c 1249): no argument (or index -1) returns the whole
    // list, an in-range index its name, anything else an empty string; an
    // unresolvable window argument selects an empty list.
    let list_available = match args.get(1) {
        None | Some(Typval::Number(-1)) => true,
        Some(value) => resolve_window(editor, value).is_some(),
    };
    let arglist = editor.arglist();
    let index = match args.first() {
        None => return Ok(name_list(arglist.names())),
        Some(value) => number_arg(value)?,
    };
    if index == -1 {
        return Ok(if list_available { name_list(arglist.names()) } else { Typval::list(Vec::new()) });
    }
    if list_available && index >= 0 {
        if let Some(name) = arglist.name(index as usize) {
            return Ok(Typval::String(name.clone()));
        }
    }
    Ok(Typval::String(OxStr::from("")))
}


fn name_list(names: &[OxStr]) -> Typval {
    Typval::list(names.iter().map(|name| Typval::String(name.clone())).collect())
}

fn resolve_window(editor: &Editor, value: &Typval) -> Option<WinHandle> {
    let number = number_arg(value).ok()?;
    let handle = WinHandle::try_from(number).ok()?;
    editor.window(handle).ok().map(|_| handle)
}

fn number_arg(value: &Typval) -> Result<i64> {
    match value {
        Typval::Number(value) => Ok(*value),
        Typval::Bool(value) => Ok(i64::from(*value)),
        _ => Err(EvalError::new("E745", 0, "Using a non-Number as a Number")),
    }
}

fn arglistid(editor: &Editor, args: &[Typval]) -> Result<Typval> {
    // f_arglistid (arglist.c 1228): the current window's list id — the
    // global list's id is 0 — or -1 when a window argument does not
    // resolve (-1 itself names the current window in find_tabwin). All
    // list-selecting arguments name the shared global list.
    for value in args {
        if number_arg(value)? != -1 && resolve_window(editor, value).is_none() {
            return Ok(Typval::Number(-1));
        }
    }
    Ok(Typval::Number(0))
}

/// Splits a `:args`/`:next` file list into names, honoring backslash
/// escapes (`do_one_arg`, arglist.c 236-261: a backslash keeps the next
/// character, whitespace separates items, backticks do not group in this
/// form).
pub(crate) fn split_file_list(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let bytes = rest.as_bytes();
        let mut item = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\\' && index + 1 < bytes.len() {
                item.push(bytes[index + 1]);
                index += 2;
                continue;
            }
            if bytes[index].is_ascii_whitespace() {
                break;
            }
            item.push(bytes[index]);
            index += 1;
        }
        items.push(String::from_utf8_lossy(&item).into_owned());
        rest = rest[index..].trim_start();
    }
    items
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeSet, HashMap};
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use super::*;
    use crate::script::{FileEntry, FileIO, FileKind, FileMetadata};
    use crate::{Editor, ExExecutor, Geometry};

    // In-memory FileIO so `:args`/`:next` can load buffers and expand
    // wildcards without touching the real filesystem.
    #[derive(Clone, Default)]
    struct MemoryFileIO {
        files: Rc<RefCell<HashMap<PathBuf, String>>>,
    }

    impl MemoryFileIO {
        fn new() -> Self {
            Self::default()
        }

        fn insert(&self, path: &str, content: &str) {
            self.files.borrow_mut().insert(PathBuf::from(path), content.to_owned());
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
            self.files.borrow_mut().insert(path.to_path_buf(), contents.to_owned());
            Ok(())
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.borrow().contains_key(path)
        }

        fn metadata(&self, path: &Path, _follow_links: bool) -> std::io::Result<FileMetadata> {
            let len = self.files.borrow().get(path).map_or(0, String::len) as u64;
            if len == 0 && !self.files.borrow().contains_key(path) {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"));
            }
            Ok(FileMetadata { kind: FileKind::File, len, modified: None, mode: 0 })
        }

        fn read_dir(&self, path: &Path) -> std::io::Result<Vec<FileEntry>> {
            // expand_glob walks "" for relative patterns but the seam sees
            // "."; treat them as the same directory.
            let directory = if path.as_os_str() == "." { Path::new("") } else { path };
            let mut names = BTreeSet::new();
            for key in self.files.borrow().keys() {
                if key.parent() == Some(directory) {
                    if let Some(name) = key.file_name() {
                        names.insert(name.to_os_string());
                    }
                }
            }
            Ok(names.into_iter().map(|name| FileEntry { path: directory.join(&name), name }).collect())
        }

        fn canonicalize(&self, path: &Path) -> PathBuf {
            path.to_path_buf()
        }

        fn copy_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            let content = self.files
                .borrow()
                .get(from)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"))?;
            self.files.borrow_mut().insert(to.to_path_buf(), content);
            Ok(())
        }
    }

    fn setup() -> (Editor, ExExecutor<MemoryFileIO>) {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
        (editor, ExExecutor::with_io(MemoryFileIO::new()))
    }

    fn global(executor: &ExExecutor<MemoryFileIO>, name: &str) -> Typval {
        executor
            .scope()
            .global
            .iter()
            .find(|(key, _)| key.as_bytes() == name.as_bytes())
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("g:{name} was not assigned"))
    }

    fn text(value: Typval) -> String {
        let Typval::String(value) = value else { panic!("expected a string") };
        value.to_string_lossy().into_owned()
    }

    fn script(executor: &mut ExExecutor<MemoryFileIO>, editor: &mut Editor, source: &str) {
        executor
            .execute_script(editor, "<arglist-test>", source)
            .unwrap_or_else(|error| panic!("script failed: {error}\n{source}"));
    }

    #[test]
    fn check_target_reports_the_do_argfile_error_shape() {
        // do_argfile (arglist.c 606-616).
        let mut list = ArgList::new();
        assert_eq!(list.check_target(0), Err(ArgRangeError { code: "E163", message: "There is only one file to edit" }));
        list.set(vec![OxStr::from("only")]);
        assert_eq!(list.check_target(1), Err(ArgRangeError { code: "E163", message: "There is only one file to edit" }));
        list.set(vec![OxStr::from("a"), OxStr::from("b")]);
        assert_eq!(list.check_target(-1), Err(ArgRangeError { code: "E164", message: "Cannot go before first file" }));
        assert_eq!(list.check_target(2), Err(ArgRangeError { code: "E165", message: "Cannot go beyond last file" }));
        assert_eq!(list.check_target(1), Ok(1));
        assert_eq!(list.index(), 0);
    }

    #[test]
    fn argc_argv_argidx_and_arglistid_report_the_global_list() {
        let (mut editor, mut executor) = setup();
        editor
            .arglist_mut()
            .set(vec![OxStr::from("one.vim"), OxStr::from("two.vim")]);
        script(
            &mut executor,
            &mut editor,
            "let g:c = argc()\n\
             let g:negc = argc(-1)\n\
             let g:badwin = argc(99)\n\
             let g:win = argc(1000)\n\
             let g:all = argv()\n\
             let g:zero = argv(0)\n\
             let g:minus = argv(-1)\n\
             let g:past = argv(9)\n\
             let g:idx = argidx()\n\
             let g:id = arglistid()\n\
             let g:badid = arglistid(99)",
        );
        assert_eq!(global(&executor, "c"), Typval::Number(2));
        assert_eq!(global(&executor, "negc"), Typval::Number(2));
        assert_eq!(global(&executor, "badwin"), Typval::Number(-1));
        // Window handles are allocated from one; 1000 does not resolve.
        assert_eq!(global(&executor, "win"), Typval::Number(-1));
        assert_eq!(
            global(&executor, "all"),
            Typval::list(vec![Typval::String(OxStr::from("one.vim")), Typval::String(OxStr::from("two.vim"))])
        );
        assert_eq!(text(global(&executor, "zero")), "one.vim");
        assert_eq!(
            global(&executor, "minus"),
            Typval::list(vec![Typval::String(OxStr::from("one.vim")), Typval::String(OxStr::from("two.vim"))])
        );
        assert_eq!(text(global(&executor, "past")), "");
        assert_eq!(global(&executor, "idx"), Typval::Number(0));
        assert_eq!(global(&executor, "id"), Typval::Number(0));
        assert_eq!(global(&executor, "badid"), Typval::Number(-1));
    }

    #[test]
    fn argc_rejects_wrong_arity() {
        let (mut editor, mut executor) = setup();
        let error = executor
            .execute_script(&mut editor, "<arity>", "let g:x = argc(1, 2)")
            .unwrap_err()
            .to_string();
        assert!(error.contains("E118"), "unexpected error: {error}");
    }

    #[test]
    fn args_redefines_lists_and_moves_through_entries() {
        let (mut editor, mut executor) = setup();
        executor.scripts().io(); // the IO seam is shared with :args edits
        script(
            &mut executor,
            &mut editor,
            "args one.vim two.vim three.vim\nlet g:first = expand('%')\nlet g:idx = argidx()",
        );
        assert_eq!(text(global(&executor, "first")), "one.vim");
        assert_eq!(global(&executor, "idx"), Typval::Number(0));
        script(&mut executor, &mut editor, "next\nlet g:second = expand('%')\nlet g:idx = argidx()");
        assert_eq!(text(global(&executor, "second")), "two.vim");
        assert_eq!(global(&executor, "idx"), Typval::Number(1));
        script(&mut executor, &mut editor, "args");
        let listing = editor
            .messages()
            .last()
            .map(|message| match &message.content {
                ox_types::Object::String(value) => value.to_string_lossy().into_owned(),
                _ => String::new(),
            })
            .unwrap_or_default();
        assert_eq!(listing, "one.vim  [two.vim]  three.vim");
    }

    #[test]
    fn next_and_previous_report_out_of_range_errors() {
        let (mut editor, mut executor) = setup();
        script(&mut executor, &mut editor, "args a.vim b.vim\nnext");
        assert_eq!(editor.arglist().index(), 1);
        let error = executor.execute_script(&mut editor, "<e165>", "next").unwrap_err().to_string();
        assert!(error.contains("E165"), "unexpected error: {error}");
        script(&mut executor, &mut editor, "previous");
        assert_eq!(editor.arglist().index(), 0);
        let error = executor.execute_script(&mut editor, "<e164>", "previous").unwrap_err().to_string();
        assert!(error.contains("E164"), "unexpected error: {error}");
        let error = executor.execute_script(&mut editor, "<e163>", "args solo.vim\nnext").unwrap_err().to_string();
        assert!(error.contains("E163"), "unexpected error: {error}");
    }

    #[test]
    fn previous_count_overflow_reports_e164() {
        // ex_previous (arglist.c 564-572) only clamps when the current
        // index itself is already past the list end; an ordinary count
        // that runs before the first entry reports E164 through
        // do_argfile.
        let (mut editor, mut executor) = setup();
        script(&mut executor, &mut editor, "args a.vim b.vim c.vim\nnext\nnext");
        assert_eq!(editor.arglist().index(), 2);
        let error = executor.execute_script(&mut editor, "<e164b>", "99previous").unwrap_err().to_string();
        assert!(error.contains("E164"), "unexpected error: {error}");
    }

    #[test]
    fn argdo_executes_the_command_in_every_entry() {
        let (mut editor, mut executor) = setup();
        script(
            &mut executor,
            &mut editor,
            "args one.vim two.vim three.vim\n\
             let g:seen = []\n\
             argdo call add(g:seen, expand('%'))",
        );
        assert_eq!(
            global(&executor, "seen"),
            Typval::list(vec![
                Typval::String(OxStr::from("one.vim")),
                Typval::String(OxStr::from("two.vim")),
                Typval::String(OxStr::from("three.vim")),
            ])
        );
        assert_eq!(editor.arglist().index(), 2);
    }

    #[test]
    fn argdo_range_limits_the_visited_entries() {
        let (mut editor, mut executor) = setup();
        script(
            &mut executor,
            &mut editor,
            "args a.vim b.vim c.vim d.vim\n\
             let g:seen = []\n\
             2,3argdo call add(g:seen, expand('%'))",
        );
        assert_eq!(
            global(&executor, "seen"),
            Typval::list(vec![Typval::String(OxStr::from("b.vim")), Typval::String(OxStr::from("c.vim"))])
        );
    }

    #[test]
    fn argdo_requires_a_command_and_tolerates_empty_lists() {
        let (mut editor, mut executor) = setup();
        let error = executor.execute_script(&mut editor, "<e471>", "argdo").unwrap_err().to_string();
        assert!(error.contains("E471"), "unexpected error: {error}");
        script(&mut executor, &mut editor, "argdo echo x");
    }

    #[test]
    fn next_revisits_buffers_without_duplicating_them() {
        let (mut editor, mut executor) = setup();
        script(&mut executor, &mut editor, "args one.vim two.vim\nnext\nprevious");
        let named = editor
            .buffers()
            .into_iter()
            .filter(|&buffer| {
                editor.buffer(buffer).is_ok_and(|state| state.name().as_bytes() == b"one.vim")
            })
            .count();
        assert_eq!(named, 1);
        let current = editor.current_buffer().unwrap();
        assert!(editor.buffer(current).is_ok_and(|state| state.name().as_bytes() == b"one.vim"));
    }

    #[test]
    fn args_expands_wildcards_and_keeps_unmatched_names() {
        // expand_wildcards with EW_NOTFOUND (arglist.c 432): patterns
        // expand to sorted matches, unmatched names stay literal.
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
        let io = MemoryFileIO::new();
        io.insert("b2.vim", "");
        io.insert("b1.vim", "");
        io.insert("a.vim", "");
        let mut executor = ExExecutor::with_io(io);
        script(&mut executor, &mut editor, "args b*.vim missing.vim a.vim");
        assert_eq!(
            editor.arglist().names(),
            [OxStr::from("b1.vim"), OxStr::from("b2.vim"), OxStr::from("missing.vim"), OxStr::from("a.vim")].as_slice()
        );
    }

    #[test]
    fn split_file_list_honors_escapes_and_whitespace() {
        // do_one_arg (arglist.c 236-261).
        assert_eq!(split_file_list(""), Vec::<String>::new());
        assert_eq!(split_file_list("a b"), vec!["a", "b"]);
        assert_eq!(split_file_list("a\\ b c"), vec!["a b", "c"]);
        assert_eq!(split_file_list("  spaced   out  "), vec!["spaced", "out"]);
    }
}
