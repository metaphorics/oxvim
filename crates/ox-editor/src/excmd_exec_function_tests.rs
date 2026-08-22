#![allow(clippy::unwrap_used)]

//! Behavioral tests for Task 8e Ex execution — function/source families.
//!
//! Citations (READ-ONLY spec under .references/neovim/):
//! * src/nvim/ex_docmd.c — ex_function, ex_call, do_source, <SNR> expansion,
//!   line continuation (getline_equal), :finish, nested sourcing.
//! * src/nvim/eval/userfunc.c — function definition flags (abort/range/dict/
//!   closure), call-frame l:/a: save+restore, varargs (a:0/a:000), named
//!   parameter binding, 'maxfuncdepth' E132, E117 unknown function,
//!   E118/E119 argument count, E122 redefinition, function! replacement.
//! * src/nvim/runtime.c — autoload name-to-path resolution (autoload/ dir
//!   mapping), load-once registry, scriptnames SID allocation.
//! * test/old/testdir/test_user_func.vim — function definition, bang, args,
//!   varargs, flags, recursion, E117/E118/E119/E122.
//! * test/old/testdir/test_source.vim — sourcing, SID, s: isolation,
//!   <SID>/<SNR>, :finish, nested source, line continuation.
//! * test/old/testdir/test_autoload.vim — autoload path resolution, load-once.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use ox_eval::Scope;
use ox_text::Buffer;
use ox_types::Typval;

use crate::script::{FileIO, ScriptCtx};
use crate::userfunc::{UserFunctions, MAX_FUNC_DEPTH};
use crate::{Editor, ExecError, ExExecutor, Geometry, RuntimeRoot, VimExceptionKind};

// ---------------------------------------------------------------------------
// Deterministic in-memory FileIO for source/autoload tests.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MemoryFileIO {
    files: RefCell<BTreeMap<PathBuf, String>>,
}

impl MemoryFileIO {
    fn new() -> Self {
        Self {
            files: RefCell::new(BTreeMap::new()),
        }
    }
    fn insert(&self, path: impl Into<PathBuf>, contents: impl Into<String>) {
        self.files.borrow_mut().insert(path.into(), contents.into());
    }
}

impl FileIO for MemoryFileIO {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.files
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("not found: {}", path.display())))
    }
    fn write_string(&self, path: &Path, contents: &str) -> io::Result<()> {
        self.files.borrow_mut().insert(path.to_path_buf(), contents.to_owned());
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

fn script(exec: &mut ExExecutor<MemoryFileIO>, editor: &mut Editor, text: &str) {
    exec.execute_script(editor, "<test>", text).unwrap();
}

fn global_number(scope: &Scope, name: &str) -> Option<i64> {
    scope
        .global
        .iter()
        .find(|(k, _)| k.as_bytes() == name.as_bytes())
        .and_then(|(_, v)| if let Typval::Number(n) = v { Some(*n) } else { None })
}

fn error_code(err: &ExecError) -> String {
    match err {
        ExecError::Vim(e) => match &e.kind {
            VimExceptionKind::Error(code) => code.clone(),
            VimExceptionKind::Throw => "Throw".to_owned(),
        },
        other => panic!("expected Vim error, got {other:?}"),
    }
}

fn editor_with_lines(lines: &[&str]) -> Editor {
    let mut editor = Editor::new();
    let owned: Vec<Vec<u8>> = lines.iter().map(|l| l.as_bytes().to_vec()).collect();
    let buffer = editor
        .create_buffer_with(Buffer::from_lines(&owned, false).unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    editor
}

// ---------------------------------------------------------------------------
// Family: function definition / call / return
// (eval/userfunc.c: ex_function, ex_call, get_return_value;
//  test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn function_define_and_call_sets_global() {
    // eval/userfunc.c: ex_function defines a user function; ex_call invokes
    // it and the body's side effects reach script-global scope.
    // test_user_func.vim: function definition and call with side effects.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! Greet()\nlet g:hit = 1\nendfunction");
    script(&mut exec, &mut editor, "call Greet()");
    assert_eq!(global_number(exec.scope(), "hit"), Some(1));
}

#[test]
fn function_return_value_via_expression_call() {
    // eval/userfunc.c: get_return_value — :return value reaches the caller
    // through the expression evaluator (BuiltinHost::call).
    // test_user_func.vim: function returning a computed value.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! Add(a, b)\nreturn a + b\nendfunction");
    script(&mut exec, &mut editor, "let g:r = Add(2, 3)");
    assert_eq!(global_number(exec.scope(), "r"), Some(5));
}

#[test]
fn eval_builtin_evaluates_string_in_current_scope() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "let g:source_value = 42");
    script(&mut exec, &mut editor, "let g:evaluated = eval('g:source_value')");
    assert_eq!(global_number(exec.scope(), "evaluated"), Some(42));
}

#[test]
fn function_empty_signature_calls_cleanly() {
    // eval/userfunc.c: a function with no parameters accepts exactly zero
    // arguments and executes its body.
    // test_user_func.vim: empty-argument function.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! Mark()\nlet g:marked = 1\nendfunction");
    script(&mut exec, &mut editor, "call Mark()");
    assert_eq!(global_number(exec.scope(), "marked"), Some(1));
}

#[test]
fn function_bare_return_yields_zero() {
    // eval/userfunc.c: a bare :return with no expression yields numeric 0
    // (rettv set to &tv_zero). test_user_func.vim: return without argument.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! NoRet()\nreturn\nendfunction");
    script(&mut exec, &mut editor, "let g:r = NoRet()");
    assert_eq!(global_number(exec.scope(), "r"), Some(0));
}

// ---------------------------------------------------------------------------
// Family: bang replacement (function!)
// (eval/userfunc.c: ex_function `!` handling; test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn function_bang_replaces_existing() {
    // eval/userfunc.c: ex_function with `!` replaces an existing definition
    // instead of raising E122. test_user_func.vim: function! replacement.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! Rep()\nlet g:v = 1\nendfunction");
    script(&mut exec, &mut editor, "function! Rep()\nlet g:v = 2\nendfunction");
    script(&mut exec, &mut editor, "call Rep()");
    assert_eq!(global_number(exec.scope(), "v"), Some(2));
}

#[test]
fn function_redefine_without_bang_yields_e122() {
    // eval/userfunc.c: defining an existing function without `!` raises
    // E122 "Function already exists, add ! to replace it".
    // test_user_func.vim: E122 on redefinition.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! Dup()\nendfunction");
    let err = exec
        .execute_script(&mut editor, "<test>", "function Dup()\nendfunction")
        .unwrap_err();
    assert_eq!(error_code(&err), "E122");
}

// ---------------------------------------------------------------------------
// Family: named args / varargs
// (eval/userfunc.c: named parameter binding, a:0, a:000;
//  test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn named_arg_accessible_via_a_prefix() {
    // eval/userfunc.c: named parameters are bound to a:{name} inside the
    // function body. test_user_func.vim: a: prefix argument access.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! Id(x)\nreturn a:x\nendfunction");
    script(&mut exec, &mut editor, "let g:r = Id(42)");
    assert_eq!(global_number(exec.scope(), "r"), Some(42));
}

#[test]
fn varargs_count_a0_reflects_extra_args() {
    // eval/userfunc.c: `...` collects extra arguments; a:0 is their count.
    // test_user_func.vim: varargs a:0.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! Va(first, ...)\nreturn a:0\nendfunction");
    script(&mut exec, &mut editor, "let g:r = Va(1, 2, 3)");
    assert_eq!(global_number(exec.scope(), "r"), Some(2));
}

#[test]
fn varargs_a000_list_holds_extra_values() {
    // eval/userfunc.c: a:000 is the list of vararg values.
    // test_user_func.vim: varargs a:000 list.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! Va2(...)\nreturn a:000\nendfunction");
    script(&mut exec, &mut editor, "let g:r = Va2(10, 20)");
    let val = exec
        .scope()
        .global
        .iter()
        .find(|(k, _)| k.as_bytes() == b"r")
        .map(|(_, v)| v.clone())
        .expect("g:r should be set");
    match val {
        Typval::List(cell) => {
            let data = cell.borrow();
            assert_eq!(data.items.len(), 2);
            match &data.items[0] {
                Typval::Number(n) => assert_eq!(*n, 10),
                _ => panic!("expected first item Number(10)"),
            }
            match &data.items[1] {
                Typval::Number(n) => assert_eq!(*n, 20),
                _ => panic!("expected second item Number(20)"),
            }
        }
        _ => panic!("expected list for a:000"),
    }
}

#[test]
fn too_many_args_yields_e118() {
    // eval/userfunc.c: supplying more arguments than declared (without
    // varargs) raises E118 "Too many arguments".
    // test_user_func.vim: E118.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! One(x)\nendfunction");
    let err = exec
        .execute_script(&mut editor, "<test>", "call One(1, 2)")
        .unwrap_err();
    assert_eq!(error_code(&err), "E118");
}

#[test]
fn not_enough_args_yields_e119() {
    // eval/userfunc.c: supplying fewer arguments than declared raises
    // E119 "Not enough arguments". test_user_func.vim: E119.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "function! Two(x, y)\nendfunction");
    let err = exec
        .execute_script(&mut editor, "<test>", "call Two(1)")
        .unwrap_err();
    assert_eq!(error_code(&err), "E119");
}

// ---------------------------------------------------------------------------
// Family: l: / a: scope isolation and restoration
// (eval/userfunc.c: call_def_function saves/restores caller l: and a:;
//  test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn local_and_argument_scope_isolated_and_restored() {
    // eval/userfunc.c: call_def_function saves and restores the caller's
    // l: and a: scopes; function-local l: vars do not leak out, and the
    // caller's l: vars survive the call.
    // test_user_func.vim: scope isolation.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(&mut exec, &mut editor, "let l:outer = 7");
    script(
        &mut exec,
        &mut editor,
        "function! Iso()\nlet l:inner = 99\nendfunction",
    );
    script(&mut exec, &mut editor, "call Iso()");
    // caller l:outer survives, l:inner does not leak
    let local = &exec.scope().local;
    assert!(local
        .iter()
        .any(|(k, v)| k.as_bytes() == b"outer" && matches!(v, Typval::Number(7))));
    assert!(!local.iter().any(|(k, _)| k.as_bytes() == b"inner"));
    // a: scope is restored to empty (no active function)
    assert!(exec.scope().argument.is_empty());
}

// ---------------------------------------------------------------------------
// Family: abort / range / dict / closure flag storage
// (eval/userfunc.c: flag parsing in ex_function; test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn parse_signature_records_abort_flag() {
    // eval/userfunc.c: the `abort` flag makes a function stop on the first
    // uncaught error. test_user_func.vim: abort flag.
    let sig = UserFunctions::parse_signature("F() abort").unwrap();
    assert!(sig.flags.abort);
    assert!(!sig.flags.range);
    assert!(!sig.flags.dict);
    assert!(!sig.flags.closure);
}

#[test]
fn parse_signature_records_range_dict_closure_flags() {
    // eval/userfunc.c: range/dict/closure flags are parsed after the ')'.
    // test_user_func.vim: range, dict, closure flags.
    let sig = UserFunctions::parse_signature("F() range dict closure").unwrap();
    assert!(sig.flags.range);
    assert!(sig.flags.dict);
    assert!(sig.flags.closure);
    assert!(!sig.flags.abort);
}

#[test]
fn closure_captures_defining_local_scope() {
    // eval/userfunc.c: a `closure` function captures the defining l: scope
    // so it can reference locals after the defining block ends.
    // test_user_func.vim: closure captures.
    let mut funcs = UserFunctions::new();
    let mut scope = Scope::new();
    scope.set(b"captured", Typval::Number(123));
    let sig = UserFunctions::parse_signature("Clo() closure").unwrap();
    funcs
        .define(sig, vec!["return l:captured".to_owned()], 0, false, &scope)
        .unwrap();
    let func = funcs.get("Clo", 0).unwrap();
    assert!(func.flags.closure);
    let found = func
        .captured
        .iter()
        .find(|(k, _)| k.as_bytes() == b"captured");
    assert!(found.is_some());
    let (_, val) = found.unwrap();
    if let Typval::Number(n) = val {
        assert_eq!(*n, 123);
    } else {
        panic!("expected Number(123) in captured scope");
    }
}

#[test]
fn range_function_receives_firstline_and_lastline() {
    // eval/userfunc.c: a `range` function receives a:firstline / a:lastline
    // from the call's line range. test_user_func.vim: range function.
    let mut editor = editor_with_lines(&["alpha", "beta", "gamma"]);
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        "function! RangeFn() range\nlet g:fl = a:firstline\nlet g:ll = a:lastline\nendfunction",
    );
    script(&mut exec, &mut editor, "1,3call RangeFn()");
    assert_eq!(global_number(exec.scope(), "fl"), Some(1));
    assert_eq!(global_number(exec.scope(), "ll"), Some(3));
}

// ---------------------------------------------------------------------------
// Family: recursion limit E132, unknown E117
// (eval/userfunc.c: 'maxfuncdepth', E117; test_user_func.vim)
// ---------------------------------------------------------------------------

#[test]
fn recursion_exceeds_maxfuncdepth_e132() {
    // eval/userfunc.c: 'maxfuncdepth' (default 100) limits call depth;
    // exceeding it raises E132. test_user_func.vim: recursion limit.
    let mut funcs = UserFunctions::new();
    let sig = UserFunctions::parse_signature("Recurse()").unwrap();
    funcs
        .define(sig, vec![], 0, false, &Scope::new())
        .unwrap();
    let mut scope = Scope::new();
    for _ in 0..MAX_FUNC_DEPTH {
        funcs
            .begin_call("Recurse", 0, vec![], 1, 1, &mut scope)
            .unwrap();
    }
    let err = funcs
        .begin_call("Recurse", 0, vec![], 1, 1, &mut scope)
        .unwrap_err();
    assert_eq!(err.code, "E132");
}

#[test]
fn call_unknown_function_yields_e117() {
    // eval/userfunc.c: calling a function not in the table raises
    // E117 "Unknown function". test_user_func.vim: E117.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let err = exec
        .execute_script(&mut editor, "<test>", "call DoesNotExist()")
        .unwrap_err();
    assert_eq!(error_code(&err), "E117");
}

// ---------------------------------------------------------------------------
// Family: source SID allocation, s: isolation, <SID>/<SNR>
// (runtime.c: do_source SID; ex_docmd.c: <SNR> expansion, s: scope;
//  test_source.vim)
// ---------------------------------------------------------------------------

#[test]
fn source_allocates_monotonic_sids() {
    // runtime.c: do_source allocates a monotonic SID per sourcing event;
    // scriptnames lists them in allocation order. test_source.vim: SID.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(&mut editor, "a.vim", "let g:xa = 1").unwrap();
    exec.execute_script(&mut editor, "b.vim", "let g:xb = 2").unwrap();
    let names = exec.scripts().script_names();
    assert_eq!(names, vec![(1, "a.vim"), (2, "b.vim")]);
}

#[test]
fn s_scope_isolated_between_scripts() {
    // runtime.c / ex_docmd.c: each sourced script gets its own s: scope;
    // s: variables from one script are not visible in another.
    // test_source.vim: script-local variable isolation.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(&mut editor, "a.vim", "let s:secret = 42")
        .unwrap();
    let err = exec
        .execute_script(&mut editor, "b.vim", "echo s:secret")
        .unwrap_err();
    assert_eq!(error_code(&err), "E121");
}

#[test]
fn s_function_canonical_name_and_snr_resolution() {
    // ex_docmd.c: s:Name canonicalizes to <SNR>{sid}_Name; <SID>Name
    // resolves the same way. <SNR> in sourced lines expands to <SNR>{sid}_.
    // test_source.vim: <SID>/s: function names.
    assert_eq!(
        UserFunctions::canonical_name("s:Foo", 3),
        "<SNR>3_Foo"
    );
    assert_eq!(
        UserFunctions::canonical_name("<SID>Foo", 3),
        "<SNR>3_Foo"
    );
    assert_eq!(UserFunctions::canonical_name("Global", 3), "Global");

    // <SNR> token expansion in sourced lines.
    let ctx: ScriptCtx<MemoryFileIO> = ScriptCtx::new(MemoryFileIO::new());
    assert_eq!(ctx.expand_snr("let x = <SNR>", 1), "let x = <SNR>1_");
    assert_eq!(ctx.expand_snr("let x = plain", 1), "let x = plain");

    // Define an s: function and call it from within the same script.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &mut editor,
        "lib.vim",
        "function! s:Helper()\nlet g:helped = 1\nendfunction\ncall s:Helper()",
    )
    .unwrap();
    assert!(exec.functions().contains("<SNR>1_Helper", 0));
    assert_eq!(global_number(exec.scope(), "helped"), Some(1));
}

#[test]
fn script_local_dictionary_member_function_can_be_defined_and_called() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_script(
        &mut editor,
        "plugin.vim",
        "let s:logger = {}\nfunction! s:logger.on_stdout()\nlet g:called = 1\nendfunction\ncall s:logger.on_stdout()",
    )
    .unwrap();

    assert_eq!(global_number(exec.scope(), "called"), Some(1));
}

#[test]
fn lowercase_bare_function_name_still_yields_e128() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let error = exec
        .execute_script(&mut editor, "plugin.vim", "function! lowercase()\nendfunction")
        .unwrap_err();
    assert_eq!(error_code(&error), "E128");
}

#[test]
fn lowercase_script_local_function_name_is_allowed() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &mut editor,
        "plugin.vim",
        "function! s:lowercase()\nlet g:called_local = 1\nendfunction\ncall s:lowercase()",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "called_local"), Some(1));
}

#[test]
fn same_script_function_call_keeps_live_script_scope() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &mut editor,
        "plugin.vim",
        "let s:state = {'value': 42}\nfunction! Main()\nlet g:from_script = s:state.value\nendfunction\ncall Main()",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "from_script"), Some(42));
}

// ---------------------------------------------------------------------------
// Family: continuation / comment joining
// (ex_docmd.c: getline_equal; test_source.vim)
// ---------------------------------------------------------------------------

#[test]
fn continuation_joining_and_comment_skipping() {
    // ex_docmd.c: getline_equal — a line whose first non-blank is `\`
    // continues the previous logical line; comment lines (first non-blank
    // `"`) are skipped and do not break a pending continuation.
    // test_source.vim: line continuation.
    let ctx: ScriptCtx<MemoryFileIO> = ScriptCtx::new(MemoryFileIO::new());
    let lines = ctx
        .join_logical_lines("let g:x = 1\n\" a comment\nlet g:y = 2 +\n\\  3")
        .unwrap();
    let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, vec!["let g:x = 1", "let g:y = 2 +  3"]);
}

// ---------------------------------------------------------------------------
// Family: :finish, nested source restoration
// (ex_docmd.c: ex_finish, do_source; test_source.vim)
// ---------------------------------------------------------------------------

#[test]
fn finish_terminates_sourced_script() {
    // ex_docmd.c: :finish terminates sourcing the current script; commands
    // after :finish are not executed. test_source.vim: :finish.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &mut editor,
        "fin.vim",
        "let g:before = 1\nfinish\nlet g:after = 2",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "before"), Some(1));
    assert_eq!(global_number(exec.scope(), "after"), None);
}

#[test]
fn nested_source_restores_caller_sid_and_script_scope() {
    // ex_docmd.c / runtime.c: sourcing a script from within another script
    // pushes a new SID/s: scope and restores the caller's on return.
    // test_source.vim: nested sourcing.
    let io = MemoryFileIO::new();
    io.insert(
        "/inner.vim",
        "let s:inner_var = 99\nlet g:inner_ran = 1",
    );
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(
        &mut editor,
        "/outer.vim",
        "let s:outer_var = 1\nsource /inner.vim\nlet g:after_inner = s:outer_var",
    )
    .unwrap();
    // inner script ran
    assert_eq!(global_number(exec.scope(), "inner_ran"), Some(1));
    // after returning from inner, outer's s: scope is restored
    assert_eq!(global_number(exec.scope(), "after_inner"), Some(1));
    // SID registry reflects both scripts
    let names = exec.scripts().script_names();
    assert_eq!(names, vec![(1, "/outer.vim"), (2, "/inner.vim")]);
}

// ---------------------------------------------------------------------------
// Family: autoload path resolution and load-once
// (runtime.c: autoload name-to-path, load-once; test_autoload.vim)
// ---------------------------------------------------------------------------

#[test]
fn autoload_path_resolution_and_load_once() {
    // runtime.c: a#b#c resolves to autoload/a/b.vim under a runtime root;
    // autoload scripts are sourced once per session. test_autoload.vim.
    let io = MemoryFileIO::new();
    io.insert(
        "/rt/autoload/mylib.vim",
        "function! mylib#Greet()\nlet g:auto_ran = 1\nendfunction",
    );
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(io);
    exec.scripts_mut()
        .add_runtime_root(RuntimeRoot::new(PathBuf::from("/rt")));

    // Calling mylib#Greet() triggers autoload resolution + source-once.
    exec.execute_script(&mut editor, "<caller>", "call mylib#Greet()")
        .unwrap();
    assert_eq!(global_number(exec.scope(), "auto_ran"), Some(1));

    // The autoload script was registered as sourced-once.
    assert!(exec
        .scripts()
        .is_sourced_once(&PathBuf::from("/rt/autoload/mylib.vim")));
}
