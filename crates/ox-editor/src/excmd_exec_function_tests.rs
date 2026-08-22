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
use std::rc::Rc;

use ox_eval::Scope;
use ox_text::Buffer;
use ox_types::{Object, OxStr, Typval};

use crate::script::{FileIO, ScriptCtx};
use crate::userfunc::{UserFunctions, MAX_FUNC_DEPTH};
use crate::{
    AutocmdKind, AutocmdOptions, Editor, Event, ExecError, ExExecutor, Geometry, LuaExec,
    LuaExecError, RuntimeRoot, VimExceptionKind,
};

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

fn global_string(scope: &Scope, name: &str) -> Option<String> {
    scope
        .global
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .and_then(|(_, value)| match value {
            Typval::String(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
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

// option.c did_set_runtimepackpath — runtime searches are glued to the
// 'runtimepath' option: `:set runtimepath=` re-roots them, and the
// compound `let &runtimepath ..=` form appends new roots exactly like
// oldtest runtest.vim does. test_autoload.vim / test_options.vim.
#[test]
fn runtime_lookups_follow_runtimepath_option() {
    let io = MemoryFileIO::new();
    io.insert("/first/colors/sample.vim", "let g:scheme_first = 1");
    io.insert("/second/colors/other.vim", "let g:scheme_second = 1");
    io.insert(
        "/appended/autoload/extra.vim",
        "function! extra#Ping()\nlet g:extra_ran = 1\nendfunction",
    );
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(io);

    // :set re-roots the search: /first only, /second unreachable.
    exec.execute_line(&mut editor, "set runtimepath=/first").unwrap();
    exec.execute_line(&mut editor, "colorscheme sample").unwrap();
    assert_eq!(global_number(exec.scope(), "scheme_first"), Some(1));
    let error = exec
        .execute_line(&mut editor, "colorscheme other")
        .unwrap_err();
    assert_eq!(error_code(&error), "E185");

    // `let &runtimepath ..=` appends a search root (runtest.vim:151).
    exec.execute_line(&mut editor, "let &runtimepath ..= ',/appended'")
        .unwrap();
    assert_eq!(
        editor.options().get_global("runtimepath").unwrap(),
        &crate::options::OptionValue::String("/first,/appended".to_owned())
    );
    // Autoload resolution consults the appended root.
    exec.execute_script(&mut editor, "<caller>", "call extra#Ping()")
        .unwrap();
    assert_eq!(global_number(exec.scope(), "extra_ran"), Some(1));

    // A rewritten 'runtimepath' replaces the whole search list.
    exec.execute_line(&mut editor, "set runtimepath=/second").unwrap();
    exec.execute_line(&mut editor, "colorscheme other").unwrap();
    assert_eq!(global_number(exec.scope(), "scheme_second"), Some(1));
}

// option.c stropt_expand_envvar + assign_option parity: a `:set` write is
// visible to `&opt` reads inside the same command batch (the eval scope
// mirror), and expand-flag options expand `$VAR`/`${VAR}` in the value.
// setup.vim:85 (`set rtp=$VIM/vimfiles,$VIMRUNTIME,...`) needs both.
#[test]
fn set_write_is_scope_visible_and_expands_env_vars() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_script(&mut editor, "<set>", "set runtimepath=/plain\nlet g:seen = &runtimepath")
        .unwrap();
    assert_eq!(global_string(exec.scope(), "seen").as_deref(), Some("/plain"));

    ox_sys::set_env("OXVIM_TEST_SET_EXPAND", "/expanded");
    exec.execute_script(
        &mut editor,
        "<set>",
        "set runtimepath=$OXVIM_TEST_SET_EXPAND,${OXVIM_TEST_SET_EXPAND}/after\nlet g:expanded = &runtimepath",
    )
    .unwrap();
    assert_eq!(
        global_string(exec.scope(), "expanded").as_deref(),
        Some("/expanded,/expanded/after")
    );

    // An unset variable stays literal, like upstream vim_getenv returning
    // NULL (option_expand leaves the text unchanged).
    exec.execute_script(&mut editor, "<set>", "set runtimepath=$OXVIM_TEST_UNSET_VAR/x\nlet g:literal = &runtimepath")
        .unwrap();
    assert_eq!(
        global_string(exec.scope(), "literal").as_deref(),
        Some("$OXVIM_TEST_UNSET_VAR/x")
    );
}
#[test]
fn colorscheme_sources_runtime_file_then_fires_matching_autocmd() {
    let io = MemoryFileIO::new();
    io.insert(
        "/first/colors/sample.vim",
        "highlight Sample guifg=blue\nlet g:scheme_body = 1",
    );
    io.insert(
        "/second/colors/sample.vim",
        "let g:wrong_runtime_root = 1",
    );
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(io);
    exec.scripts_mut().add_runtime_root(RuntimeRoot::new(PathBuf::from("/first")));
    exec.scripts_mut().add_runtime_root(RuntimeRoot::new(PathBuf::from("/second")));
    exec.execute_line(
        &mut editor,
        "autocmd ColorScheme sample let g:event_colors_name = g:colors_name",
    )
    .unwrap();

    exec.execute_line(&mut editor, "colorscheme sample").unwrap();

    assert_eq!(global_number(exec.scope(), "scheme_body"), Some(1));
    assert_eq!(global_number(exec.scope(), "wrong_runtime_root"), None);
    assert_eq!(global_string(exec.scope(), "colors_name").as_deref(), Some("sample"));
    assert_eq!(
        global_string(exec.scope(), "event_colors_name").as_deref(),
        Some("sample")
    );
    assert_eq!(editor.highlights()["Sample"]["guifg"], "blue");
}

#[test]
fn colorscheme_missing_runtime_file_is_e185_without_state_or_event() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.scripts_mut().add_runtime_root(RuntimeRoot::new(PathBuf::from("/rt")));
    exec.execute_line(&mut editor, "let g:colors_name = 'before'").unwrap();
    exec.execute_line(
        &mut editor,
        "autocmd ColorScheme * let g:unexpected_colorscheme_event = 1",
    )
    .unwrap();

    let error = exec.execute_line(&mut editor, "colorscheme missing").unwrap_err();

    assert_eq!(error_code(&error), "E185");
    assert!(error.to_string().contains("Cannot find color scheme 'missing'"));
    assert_eq!(global_string(exec.scope(), "colors_name").as_deref(), Some("before"));
    assert_eq!(global_number(exec.scope(), "unexpected_colorscheme_event"), None);
}

#[derive(Default)]
struct ColorschemeLua {
    callback_colors_name: Option<String>,
}

impl LuaExec for ColorschemeLua {
    fn execute_chunk(
        &mut self,
        _editor: &mut Editor,
        _code: &str,
        _args: Vec<Object>,
    ) -> Result<Object, LuaExecError> {
        Ok(Object::Nil)
    }

    fn execute_file(&mut self, _editor: &mut Editor, _path: &Path) -> Result<(), LuaExecError> {
        Ok(())
    }

    fn invoke_callback(
        &mut self,
        editor: &mut Editor,
        _reference: usize,
        _args: Vec<Object>,
    ) -> Result<(), LuaExecError> {
        self.callback_colors_name = editor
            .gvars()
            .get(&OxStr::from("colors_name"))
            .and_then(|value| match value {
                Object::String(value) => Some(value.to_string_lossy().into_owned()),
                _ => None,
            });
        Ok(())
    }
}

#[test]
fn colorscheme_lua_autocmd_observes_and_preserves_new_global_name() {
    let io = MemoryFileIO::new();
    io.insert("/rt/colors/luaonly.lua", "");
    let host = Rc::new(RefCell::new(ColorschemeLua::default()));
    let mut editor = Editor::new();
    editor
        .autocmds_mut()
        .register(
            Event::ColorScheme,
            "luaonly",
            AutocmdKind::LuaCallback(7),
            AutocmdOptions::default(),
        )
        .unwrap();
    let mut exec = ExExecutor::with_io(io);
    exec.scripts_mut().add_runtime_root(RuntimeRoot::new(PathBuf::from("/rt")));
    exec.set_lua_exec(host.clone());

    exec.execute_line(&mut editor, "colorscheme luaonly").unwrap();

    assert_eq!(host.borrow().callback_colors_name.as_deref(), Some("luaonly"));
    assert_eq!(global_string(exec.scope(), "colors_name").as_deref(), Some("luaonly"));
}

#[test]
fn job_callbacks_bind_the_options_dictionary_as_self() {
    let mut editor = editor_with_lines(&[""]);
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        "let s:logger = {'events': []}\nfunction s:logger.on_stdout(id, data, event)\ncall add(self.events, a:event)\nendfunction\nlet s:logger.on_stderr = s:logger.on_stdout\nfunction s:logger.on_exit(id, data, event)\ncall add(self.events, a:event)\nendfunction\nlet g:job = jobstart(['sh', '-c', 'printf job-output'], s:logger)\nlet g:pid_ok = jobpid(g:job) > 0\nlet g:statuses = jobwait([g:job], 2000)\nlet g:exit_code = g:statuses[0]\nlet g:event_count = len(s:logger.events)",
    );
    assert_eq!(global_number(exec.scope(), "pid_ok"), Some(1));
    assert_eq!(global_number(exec.scope(), "exit_code"), Some(0));
    assert!(global_number(exec.scope(), "event_count").is_some_and(|count| count >= 2));
}


#[test]
fn script_local_calls_inside_persisted_functions_use_defining_sid() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &mut editor,
        "plugin.vim",
        "function! s:Helper()\nlet g:called = 17\nendfunction\nfunction! Entry()\ncall s:Helper()\nendfunction",
    )
    .unwrap();

    exec.execute_line(&mut editor, "call Entry()").unwrap();
    assert_eq!(global_number(exec.scope(), "called"), Some(17));
}

#[test]
fn same_script_local_function_name_stays_isolated_after_source_returns() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &mut editor,
        "one.vim",
        "function! s:Helper()\nlet g:which = 1\nendfunction\nfunction! One()\ncall s:Helper()\nendfunction",
    )
    .unwrap();
    exec.execute_script(
        &mut editor,
        "two.vim",
        "function! s:Helper()\nlet g:which = 2\nendfunction\nfunction! Two()\ncall s:Helper()\nendfunction",
    )
    .unwrap();

    exec.execute_line(&mut editor, "call One()").unwrap();
    assert_eq!(global_number(exec.scope(), "which"), Some(1));
    exec.execute_line(&mut editor, "call Two()").unwrap();
    assert_eq!(global_number(exec.scope(), "which"), Some(2));
}

#[test]
fn nested_finish_returns_control_to_sourcing_caller() {
    let io = MemoryFileIO::new();
    io.insert("/inner.vim", "let g:inner_before = 1\nfinish\nlet g:inner_after = 1");
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(
        &mut editor,
        "/outer.vim",
        "source /inner.vim\nlet g:outer_after = 1",
    )
    .unwrap();

    assert_eq!(global_number(exec.scope(), "inner_before"), Some(1));
    assert_eq!(global_number(exec.scope(), "inner_after"), None);
    assert_eq!(global_number(exec.scope(), "outer_after"), Some(1));
}

#[test]
fn finish_outside_sourced_script_is_e168() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    assert_eq!(error_code(&exec.execute_line(&mut editor, "finish").unwrap_err()), "E168");
}

#[test]
fn exists_reports_editor_options_functions_commands_and_autocmds() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(
        &mut editor,
        "exists.vim",
        "function! s:Local()\nendfunction\naugroup ExistGroup\nautocmd BufEnter *.rs let g:event_fired = 1\naugroup END\nlet g:opt = exists('&number')\nlet g:short_opt = exists('+nu')\nlet g:func = exists('*s:Local')\nlet g:command = exists(':set')\nlet g:abbrev = exists(':se')\nlet g:event = exists('##BufEnter')\nlet g:group = exists('#ExistGroup')\nlet g:registered = exists('#ExistGroup#BufEnter')\nlet g:missing = exists('#ExistGroup#BufLeave')",
    )
    .unwrap();

    for name in ["opt", "short_opt", "func", "event", "group", "registered"] {
        assert_eq!(global_number(exec.scope(), name), Some(1), "{name}");
    }
    assert_eq!(global_number(exec.scope(), "command"), Some(2));
    assert_eq!(global_number(exec.scope(), "abbrev"), Some(1));
    assert_eq!(global_number(exec.scope(), "missing"), Some(0));
}

#[test]
fn system_builtin_captures_stdout_and_exit_status() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "let g:out = system('printf oxvim')").unwrap();
    assert!(matches!(
        exec.scope().global.iter().find(|(key, _)| key.as_bytes() == b"out"),
        Some((_, Typval::String(value))) if value.as_bytes() == b"oxvim"
    ));
    assert!(matches!(
        exec.scope().vim.iter().find(|(key, _)| key.as_bytes() == b"shell_error"),
        Some((_, Typval::Number(0)))
    ));

    exec.execute_line(&mut editor, "call system('exit 7')").unwrap();
    assert!(matches!(
        exec.scope().vim.iter().find(|(key, _)| key.as_bytes() == b"shell_error"),
        Some((_, Typval::Number(7)))
    ));
}

#[test]
fn expand_builtin_reads_current_buffer_and_preserves_paths() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor.buffer_mut(buffer).unwrap().set_name("test_functions.vim".into());
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "let g:name = expand('%')").unwrap();
    exec.execute_line(&mut editor, "let g:path = expand('/tmp/build')").unwrap();

    assert!(matches!(
        exec.scope().global.iter().find(|(key, _)| key.as_bytes() == b"name"),
        Some((_, Typval::String(value))) if value.as_bytes() == b"test_functions.vim"
    ));
    assert!(matches!(
        exec.scope().global.iter().find(|(key, _)| key.as_bytes() == b"path"),
        Some((_, Typval::String(value))) if value.as_bytes() == b"/tmp/build"
    ));
}

// ---------------------------------------------------------------------------
// :language (os/lang.c ex_language; oldtest test_excmd.vim Test_language_cmd)
// ---------------------------------------------------------------------------

fn vim_string(scope: &Scope, name: &str) -> Option<String> {
    scope
        .vim
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .and_then(|(_, value)| match value {
            Typval::String(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
}

#[test]
fn language_messages_sets_env_and_vim_vars() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    // `lang mess C` exercises the abbreviated keyword form oldtest's
    // runtest.vim uses ("Output all messages in English").
    exec.execute_line(&mut editor, "lang mess C").unwrap();

    assert_eq!(vim_string(exec.scope(), "lang").as_deref(), Some("C"));
    assert_eq!(vim_string(exec.scope(), "ctype").as_deref(), Some("C"));
    assert_eq!(vim_string(exec.scope(), "lc_time").as_deref(), Some("C"));
    assert_eq!(vim_string(exec.scope(), "collate").as_deref(), Some("C"));
    assert_eq!(std::env::var_os("LC_ALL").as_deref(), Some(std::ffi::OsStr::new("")));
    assert_eq!(std::env::var_os("LC_MESSAGES").as_deref(), Some(std::ffi::OsStr::new("C")));
}

#[test]
fn language_without_keyword_sets_lang_and_language_env() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_line(&mut editor, "language C").unwrap();

    assert_eq!(vim_string(exec.scope(), "lang").as_deref(), Some("C"));
    assert_eq!(std::env::var_os("LANG").as_deref(), Some(std::ffi::OsStr::new("C")));
    assert_eq!(std::env::var_os("LANGUAGE").as_deref(), Some(std::ffi::OsStr::new("")));
    assert_eq!(std::env::var_os("LC_ALL").as_deref(), Some(std::ffi::OsStr::new("")));
    assert_eq!(std::env::var_os("LC_MESSAGES").as_deref(), Some(std::ffi::OsStr::new("C")));
}

#[test]
fn language_ctype_leaves_lang_and_messages_env_untouched() {
    ox_sys::set_env("LANG", "ox-language-sentinel");
    ox_sys::set_env("LC_MESSAGES", "ox-language-sentinel");
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_line(&mut editor, "language ctype C").unwrap();

    assert_eq!(vim_string(exec.scope(), "ctype").as_deref(), Some("C"));
    assert_eq!(
        std::env::var_os("LANG").as_deref(),
        Some(std::ffi::OsStr::new("ox-language-sentinel"))
    );
    assert_eq!(
        std::env::var_os("LC_MESSAGES").as_deref(),
        Some(std::ffi::OsStr::new("ox-language-sentinel"))
    );
}

#[test]
fn language_rejected_locale_is_e197_for_ctype_and_time() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    for keyword in ["ctype", "time"] {
        let error = exec
            .execute_line(&mut editor, &format!("language {keyword} non_existing_lang.bad"))
            .unwrap_err();
        assert_eq!(error_code(&error), "E197");
        assert!(
            error.to_string().contains("Cannot set language to \"non_existing_lang.bad\""),
            "{error}"
        );
    }
}

#[test]
fn language_without_name_reports_current_locale() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(&mut editor, "language C").unwrap();

    exec.execute_line(&mut editor, "language messages").unwrap();
    let last = editor.messages().last().unwrap();
    assert_eq!(last.content, Object::String(OxStr::from("Current messages language: \"C\"")));

    exec.execute_line(&mut editor, "language").unwrap();
    let last = editor.messages().last().unwrap();
    assert_eq!(last.content, Object::String(OxStr::from("Current language: \"C\"")));
}
