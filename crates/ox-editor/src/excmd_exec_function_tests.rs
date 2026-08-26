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

use ox_eval::{Scope, ScopeKind};
use ox_text::Buffer;
use ox_types::{Object, OxStr, Typval};

use crate::script::{FileIO, ScriptCtx, SourceContext};
use crate::userfunc::{UserFunctions, MAX_FUNC_DEPTH};
use crate::{
    AutocmdKind, AutocmdOptions, Editor, Event, ExecError, ExExecutor, Geometry, LuaExec,
    LuaExecError, RuntimeRoot, VimExceptionKind,
};

// ---------------------------------------------------------------------------
// Deterministic in-memory FileIO for source/autoload tests.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct MemoryFileIO {
    files: Rc<RefCell<BTreeMap<PathBuf, String>>>,
}

impl MemoryFileIO {
    fn new() -> Self {
        Self {
            files: Rc::new(RefCell::new(BTreeMap::new())),
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
    fn write_bytes(&self, path: &Path, contents: &[u8], append: bool) -> io::Result<()> {
        let contents = std::str::from_utf8(contents)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8"))?;
        let mut files = self.files.borrow_mut();
        if append {
            files.entry(path.to_path_buf()).or_default().push_str(contents);
        } else {
            files.insert(path.to_path_buf(), contents.to_owned());
        }
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

fn register_text(editor: &Editor, name: char) -> String {
    String::from_utf8(editor.registers().get(name).unwrap().unwrap().to_bytes()).unwrap()
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
// Family: default arguments
// (eval/userfunc.c get_function_args / call_user_func; test_user_func.vim
//  Test_default_arg, Test_default_argument_expression_error_while_inside_of_
//  a_try_block)
// ---------------------------------------------------------------------------

#[test]
fn default_args_fill_omitted_positionals() {
    // call_user_func: omitted defaulted parameters are bound to the value of
    // their expression; supplied values win. test_user_func.vim: Log().
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        "function! Sum(x, y = 2)\nreturn a:x + a:y\nendfunction",
    );
    script(&mut exec, &mut editor, "let g:a = Sum(40)");
    script(&mut exec, &mut editor, "let g:b = Sum(40, 3)");
    assert_eq!(global_number(exec.scope(), "a"), Some(42));
    assert_eq!(global_number(exec.scope(), "b"), Some(43));
}

#[test]
fn default_args_do_not_count_toward_a0() {
    // call_user_func: a:0 is max(argcount - uf_args, 0); defaults fill
    // positionals and never count as varargs. test_user_func.vim: Args().
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        "function! Args(mandatory, optional = v:null, ...)\nreturn deepcopy(a:)\nendfunction",
    );
    script(&mut exec, &mut editor, "let g:one = Args(1)");
    script(&mut exec, &mut editor, "let g:three = Args(1, 2, 3)");
    let value = |scope: &Scope, name: &str| {
        scope
            .global
            .iter()
            .find(|(key, _)| key.as_bytes() == name.as_bytes())
            .map(|(_, value)| value.clone())
    };
    let entry = |scope: &Scope, name: &str, key: &[u8]| match value(scope, name) {
        Some(Typval::Dict(cell)) => {
            let data = cell.borrow();
            data.entries
                .iter()
                .find(|(entry, _)| entry.as_bytes() == key)
                .map(|(_, value)| value.clone())
        }
        _ => panic!("expected dict for {name}"),
    };
    assert_eq!(
        entry(exec.scope(), "one", b"mandatory"),
        Some(Typval::Number(1))
    );
    assert_eq!(
        entry(exec.scope(), "one", b"optional"),
        Some(Typval::Special(ox_types::Special::Null))
    );
    assert_eq!(entry(exec.scope(), "one", b"0"), Some(Typval::Number(0)));
    assert_eq!(
        entry(exec.scope(), "three", b"optional"),
        Some(Typval::Number(2))
    );
    assert_eq!(entry(exec.scope(), "three", b"0"), Some(Typval::Number(1)));
    assert_eq!(entry(exec.scope(), "three", b"1"), Some(Typval::Number(3)));
}

#[test]
fn default_args_still_e118_when_exceeding_total() {
    // check_user_func_argcount: too many arguments still applies to the full
    // positional count. test_user_func.vim: `call Log(1,2,3)` → E118.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        "function! Sum2(x, y = 2)\nreturn a:x + a:y\nendfunction",
    );
    let err = exec
        .execute_script(&mut editor, "<test>", "call Sum2(1, 2, 3)")
        .unwrap_err();
    assert_eq!(error_code(&err), "E118");
}

#[test]
fn default_args_still_e119_below_required() {
    // check_user_func_argcount: required = uf_args - uf_def_args.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        "function! Req(a, b, c = 1)\nreturn a:c\nendfunction",
    );
    let err = exec
        .execute_script(&mut editor, "<test>", "call Req(1)")
        .unwrap_err();
    assert_eq!(error_code(&err), "E119");
}

#[test]
fn non_default_argument_after_default_is_e989() {
    // get_function_args: E989. test_user_func.vim: MakeBadFunc().
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let err = exec
        .execute_script(&mut editor, "<test>", "function Bad(a, b=1, c)\nendfunction")
        .unwrap_err();
    assert_eq!(error_code(&err), "E989");
}

#[test]
fn white_space_before_comma_is_e1068() {
    // get_function_args: E1068. test_user_func.vim:
    // `fu F(a=1 ,) | endf` → E1068.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let err = exec
        .execute_script(&mut editor, "<test>", "function W(a=1 ,)\nendfunction")
        .unwrap_err();
    assert_eq!(error_code(&err), "E1068");
}

#[test]
fn default_expression_may_contain_commas_strings_and_nesting() {
    // get_function_args walks the default expression like eval1, so commas
    // inside strings/lists/dicts and parens inside strings do not split the
    // argument list or end it early.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        "function! L(x, y = [1, 2, 3])\nreturn len(a:y) + a:x\nendfunction",
    );
    script(&mut exec, &mut editor, "let g:list = L(1)");
    assert_eq!(global_number(exec.scope(), "list"), Some(4));
    script(
        &mut exec,
        &mut editor,
        "function! S(x, y = \"a,b)\")\nreturn len(a:y)\nendfunction",
    );
    script(&mut exec, &mut editor, "let g:str = S(0)");
    assert_eq!(global_number(exec.scope(), "str"), Some(4));
}

#[test]
fn default_expression_evaluates_in_caller_scope_and_aborts_call() {
    // call_user_func evaluates defaults before entering the frame; an error
    // surfaces from the call site and the body never runs.
    // test_user_func.vim:
    // Test_default_argument_expression_error_while_inside_of_a_try_block.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        concat!(
            "function! s:f(v = s:undefined_variable)\n",
            "let s:entered_fn_body = 1\n",
            "return a:v\n",
            "endfunction\n",
            "let g:caught = 0\n",
            "try\n",
            "call s:f()\n",
            "catch\n",
            "let g:caught = 1\n",
            "let g:msg = v:exception\n",
            "endtry\n",
            "let g:entered = exists('s:entered_fn_body')"
        ),
    );
    assert_eq!(global_number(exec.scope(), "caught"), Some(1));
    // Oracle: an error raised while evaluating a default argument escapes
    // through `:call`, so `v:exception` is `Vim(call):E121: ...`.
    assert!(global_string(exec.scope(), "msg")
        .unwrap()
        .starts_with("Vim(call):E121: Undefined variable: s:undefined_variable"));
    assert_eq!(global_number(exec.scope(), "entered"), Some(0));
}

#[test]
fn evaluator_error_inside_user_function_enters_caller_catch_frame() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        concat!(
            "function! BrokenBuiltin()\n",
            "return screenrow()\n",
            "endfunction\n",
            "let g:caught = 0\n",
            "let g:finalized = 0\n",
            "try\n",
            "call BrokenBuiltin()\n",
            "catch\n",
            "let g:caught = 1\n",
            "let g:exception = v:exception\n",
            "let g:throwpoint = v:throwpoint\n",
            "finally\n",
            "let g:finalized = 1\n",
            "endtry"
        ),
    );
    assert_eq!(global_number(exec.scope(), "caught"), Some(1));
    assert_eq!(global_number(exec.scope(), "finalized"), Some(1));
    assert_eq!(global_string(exec.scope(), "exception").as_deref(), Some("Vim(call):E117: not implemented: screenrow"));
    assert_eq!(global_string(exec.scope(), "throwpoint").as_deref(), Some("function BrokenBuiltin[1]..script <test>[1]"));
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
    scope.set(b"captured", Typval::Number(123)).unwrap();
    let sig = UserFunctions::parse_signature("Clo() closure").unwrap();
    funcs
        .define(sig, vec!["return l:captured".to_owned()], SourceContext::default(), false, &scope)
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
        .define(sig, vec![], SourceContext::default(), false, &Scope::new())
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

#[test]
fn delfunction_removes_registry_entries_and_bang_ignores_missing() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_script(&mut editor, "delete.vim", "function! DeleteMe()\nreturn 1\nendfunction\ndelfunction DeleteMe\ndelfunction! DeleteMe").unwrap();
    let error = exec.execute_line(&mut editor, "call DeleteMe()").unwrap_err();
    assert_eq!(error_code(&error), "E117");
    let error = exec.execute_line(&mut editor, "delfunction DeleteMe").unwrap_err();
    assert_eq!(error_code(&error), "E130");
}

#[test]
fn delfunction_rejects_the_active_function() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let error = exec.execute_script(&mut editor, "active.vim", "function! Active()\ndelfunction Active\nendfunction\ncall Active()").unwrap_err();
    assert_eq!(error_code(&error), "E131");
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
// Family: command resolution happens when a line runs, and re-sourcing a
// script reuses its SID (ex_docmd.c find_ex_command / do_one_cmd;
// runtime.c:2226,2333 find_script_by_name + sc_seq)
// ---------------------------------------------------------------------------

#[test]
fn a_user_command_is_resolved_when_its_line_runs_not_when_it_is_parsed() {
    // find_ex_command consults the live command table inside do_one_cmd, so a
    // :command created earlier in the same script — or in a script sourced by
    // it — is visible on a later line even though the whole body was read
    // first. check.vim's CheckFunction reaches test bodies exactly this way.
    let io = MemoryFileIO::new();
    io.insert("/guard.vim", "command! -nargs=1 T69Guard let g:guarded = <q-args>");
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(
        &mut editor,
        "/main.vim",
        "command! -nargs=1 T69Same let g:same = <q-args>\n\
         T69Same here\n\
         source /guard.vim\n\
         function! T69Body()\n\
         T69Guard inside\n\
         endfunction\n\
         call T69Body()",
    )
    .unwrap();
    assert_eq!(global_string(exec.scope(), "same").as_deref(), Some("here"));
    assert_eq!(global_string(exec.scope(), "guarded").as_deref(), Some("inside"));
}

#[test]
fn an_unresolvable_command_still_reports_e492_after_the_retry() {
    // The retry is a re-resolution, not a rescue: a name no :command ever
    // created reports E492 exactly as before.
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let err = exec
        .execute_script(&mut editor, "/main.vim", "T69NeverDefined arg")
        .unwrap_err();
    assert_eq!(error_code(&err), "E492");
}

#[test]
fn re_sourcing_a_script_keeps_its_script_local_variables() {
    // do_source looks the file up with find_script_by_name and reuses its SID
    // (runtime.c:2226,2335), so `if exists('s:did_load') | finish | endif`
    // — setup.vim:50-53, which guards the `comclear` that wipes every user
    // command — short-circuits on the second sourcing.
    let io = MemoryFileIO::new();
    io.insert(
        "/guarded.vim",
        "let g:runs = get(g:, 'runs', 0) + 1\n\
         if exists('s:did_load')\n\
         finish\n\
         endif\n\
         let s:did_load = 1\n\
         let g:bodies = get(g:, 'bodies', 0) + 1",
    );
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(
        &mut editor,
        "/main.vim",
        "source /guarded.vim\nsource /guarded.vim\nsource /guarded.vim",
    )
    .unwrap();
    assert_eq!(global_number(exec.scope(), "runs"), Some(3));
    assert_eq!(global_number(exec.scope(), "bodies"), Some(1));
    // One registry entry for the file, not one per sourcing event.
    assert_eq!(
        exec.scripts().script_names(),
        vec![(1, "/main.vim"), (2, "/guarded.vim")]
    );
}

#[test]
fn a_reloaded_script_redefines_its_own_command_and_function_but_a_stranger_cannot() {
    // "can be replaced with ! and when sourcing the same script again, but
    // only once": usercmd.c:940-948 and eval/userfunc.c:2856-2863 both key on
    // (sc_sid, sc_seq). Same SID, new sequence — silent replace. Different
    // SID — E174/E122.
    let io = MemoryFileIO::new();
    io.insert(
        "/defs.vim",
        "command -nargs=1 T69Dup let g:dup = <q-args>\nfunc T69Fn()\nendfunc",
    );
    io.insert("/other.vim", "command -nargs=1 T69Dup let g:dup = 'other'");
    io.insert("/otherfn.vim", "func T69Fn()\nendfunc");
    // Two definitions inside *one* sourcing share a sequence number, so the
    // reload exemption must not cover them.
    io.insert(
        "/twice.vim",
        "command -nargs=1 T69Once echo 1\ncommand -nargs=1 T69Once echo 2",
    );
    io.insert("/twicefn.vim", "func T69Twice()\nendfunc\nfunc T69Twice()\nendfunc");
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(io);
    exec.execute_script(&mut editor, "/main.vim", "source /defs.vim\nsource /defs.vim")
        .unwrap();
    let command_err = exec
        .execute_script(&mut editor, "/caller.vim", "source /other.vim")
        .unwrap_err();
    assert_eq!(error_code(&command_err), "E174");
    let function_err = exec
        .execute_script(&mut editor, "/caller2.vim", "source /otherfn.vim")
        .unwrap_err();
    assert_eq!(error_code(&function_err), "E122");
    let same_seq_command = exec
        .execute_script(&mut editor, "/caller3.vim", "source /twice.vim")
        .unwrap_err();
    assert_eq!(error_code(&same_seq_command), "E174");
    let same_seq_function = exec
        .execute_script(&mut editor, "/caller4.vim", "source /twicefn.vim")
        .unwrap_err();
    assert_eq!(error_code(&same_seq_function), "E122");
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

/// `let $VAR = value` changes the *process* environment, so a child process
/// sees it.
///
/// `ex_let_env` (`eval/vars.c`:1349-1351) assigns through `vim_setenv_ext`,
/// which is `os_setenv`. Recording the value only in the script scope is not a
/// smaller version of that, it is a different behavior with teeth:
/// `setup.vim:115` sandboxes the home directory with
/// `let $HOME = expand(getcwd() . '/XfakeHOME')`, and `runtest.vim:472` cleans
/// up with `call system('rm -rf  ' .. file)`. A file named `Xdir ~ dir`
/// word-splits there, so the shell expands `~` against the child's HOME. With
/// the assignment kept out of the environment the child inherits the real home
/// directory and the cleanup deletes it.
///
/// The child here is the shell `system()` uses, and HOME is a throwaway path
/// that is never written to.
#[test]
fn let_env_assignment_reaches_child_processes() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let sandbox = std::env::temp_dir().join(format!("ox-editor-fakehome-{}", std::process::id()));
    let sandbox = sandbox.to_string_lossy().into_owned();
    let restore = std::env::var_os("HOME");

    exec.execute_script(
        &mut editor,
        "<let-env>",
        &format!(
            "let $HOME = '{sandbox}'\n\
             let g:vim_side = $HOME\n\
             let g:child_side = substitute(system('printf %s \"$HOME\"'), '\\n', '', 'g')\n\
             let g:child_tilde = substitute(system('printf %s ~'), '\\n', '', 'g')"
        ),
    )
    .unwrap();

    let vim_side = global_string(exec.scope(), "vim_side");
    let child_side = global_string(exec.scope(), "child_side");
    let child_tilde = global_string(exec.scope(), "child_tilde");
    // Put the process back before asserting, whatever happened.
    assert!(match restore {
        Some(home) => ox_sys::set_env("HOME", home),
        None => ox_sys::unset_env("HOME"),
    });

    assert_eq!(vim_side.as_deref(), Some(sandbox.as_str()));
    assert_eq!(child_side.as_deref(), Some(sandbox.as_str()));
    // The shell's own `~` is the sandbox too, which is the expansion that
    // `rm -rf ~` in a suite cleanup lands on.
    assert_eq!(child_tilde.as_deref(), Some(sandbox.as_str()));
}

/// `unlet $VAR` is the process-wide unset (`vim_unsetenv_ext`), so a child no
/// longer sees it either.
#[test]
fn unlet_env_removes_the_variable_from_child_processes() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_script(
        &mut editor,
        "<unlet-env>",
        "let $OXVIM_TEST_UNLET = 'present'\n\
         let g:before = substitute(system('printf %s \"$OXVIM_TEST_UNLET\"'), '\\n', '', 'g')\n\
         unlet $OXVIM_TEST_UNLET\n\
         let g:after = substitute(system('printf %s \"$OXVIM_TEST_UNLET\"'), '\\n', '', 'g')\n\
         let g:read_back = $OXVIM_TEST_UNLET",
    )
    .unwrap();

    assert_eq!(global_string(exec.scope(), "before").as_deref(), Some("present"));
    assert_eq!(global_string(exec.scope(), "after").as_deref(), Some(""));
    assert_eq!(global_string(exec.scope(), "read_back").as_deref(), Some(""));
    assert!(std::env::var_os("OXVIM_TEST_UNLET").is_none());
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

/// `ExExecutor::call_builtin` is the entry point the Lua `vim.fn` bridge comes
/// in through. It used to forward straight into a job-only dispatcher that
/// served five names -- `jobstop`, `jobpid`, `chansend`/`jobsend`, `jobwait` --
/// and ended in a bare `unreachable!()`, so every other name reached that
/// panic. The rows below are one per class of name the audit found behind it,
/// each arranged to answer differently if only its own route were wrong:
/// a name the old dispatcher did serve, the two Process-family names it did
/// not, an editor-stateful name from another family, a typval-only name, a
/// regex-backed typval-only name (which also proves the host carries a regex
/// engine rather than `Builtins::without_regex`), and an unknown name, which
/// must raise `E117` instead of aborting the process.
#[test]
fn call_builtin_serves_every_family_instead_of_panicking_outside_the_job_arms() {
    let mut editor = editor_with_lines(&["alpha"]);
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    let call = |exec: &mut ExExecutor<MemoryFileIO>, editor: &mut Editor, name: &str, args: Vec<Typval>| {
        exec.call_builtin(editor, &OxStr::from(name), args)
    };

    // Served before and after: an unknown job id waits to -3.
    assert_eq!(
        call(&mut exec, &mut editor, "jobwait", vec![Typval::list(vec![Typval::Number(9_999)])]).unwrap(),
        Typval::list(vec![Typval::Number(-3)]),
    );

    // Process family, no arm before: jobstart returns a live channel id, and
    // the job it started reaches exit status 0.
    let job = call(
        &mut exec,
        &mut editor,
        "jobstart",
        vec![Typval::list(vec![Typval::String(OxStr::from("true"))])],
    )
    .unwrap();
    let Typval::Number(id) = job else { panic!("jobstart did not answer a channel id: {job:?}") };
    assert!(id > 0, "jobstart answered {id}");
    assert_eq!(
        call(&mut exec, &mut editor, "jobwait", vec![Typval::list(vec![Typval::Number(id)]), Typval::Number(5_000)]).unwrap(),
        Typval::list(vec![Typval::Number(0)]),
    );

    // Process family, no arm before: system() runs through 'shell'.
    assert_eq!(
        call(&mut exec, &mut editor, "system", vec![Typval::String(OxStr::from("printf ok"))]).unwrap(),
        Typval::String(OxStr::from("ok")),
    );

    // Another editor-stateful family entirely.
    assert_eq!(
        call(&mut exec, &mut editor, "bufnr", vec![Typval::String(OxStr::from("%"))]).unwrap(),
        Typval::Number(1),
    );

    // Typval-only, no editor state.
    assert_eq!(
        call(&mut exec, &mut editor, "printf", vec![Typval::String(OxStr::from("%d-%s")), Typval::Number(7), Typval::String(OxStr::from("x"))]).unwrap(),
        Typval::String(OxStr::from("7-x")),
    );

    // Regex-backed typval-only: `Builtins::without_regex()` answers E54 here.
    assert_eq!(
        call(&mut exec, &mut editor, "substitute", vec![
            Typval::String(OxStr::from("aXbXc")),
            Typval::String(OxStr::from("X")),
            Typval::String(OxStr::from("-")),
            Typval::String(OxStr::from("g")),
        ])
        .unwrap(),
        Typval::String(OxStr::from("a-b-c")),
    );

    // An unknown name is an error, not a panic and not an abort.
    let error = call(&mut exec, &mut editor, "nosuchbuiltin", Vec::new()).unwrap_err();
    assert!(error.to_string().contains("nosuchbuiltin"), "{error}");
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
fn systemlist_uses_job_channels_for_shell_and_argv_forms() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "let g:shell = systemlist('printf \"hello\\n\\n\"')").unwrap();
    exec.execute_line(&mut editor, "let g:argv = systemlist(['printf', 'one\\ntwo\\n'])").unwrap();
    exec.execute_line(&mut editor, "let g:kept = systemlist('printf \"x\\n\"', '', 1)").unwrap();
    exec.execute_line(&mut editor, "let g:failed = systemlist('printf bad; exit 7')").unwrap();

    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"shell", 0).unwrap(), &Typval::list(vec![Typval::String(OxStr::from("hello")), Typval::String(OxStr::from(""))]));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"argv", 0).unwrap(), &Typval::list(vec![Typval::String(OxStr::from("one")), Typval::String(OxStr::from("two"))]));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"kept", 0).unwrap(), &Typval::list(vec![Typval::String(OxStr::from("x")), Typval::String(OxStr::from(""))]));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"failed", 0).unwrap(), &Typval::list(vec![Typval::String(OxStr::from("bad"))]));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Vim, b"shell_error", 0).unwrap(), &Typval::Number(7));
}

#[test]
fn window_screen_and_state_builtins_use_editor_state() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    let window = editor.create_tabpage(buffer, Geometry::new(0, 0, 20, 6).unwrap()).unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_script(&mut editor, "builtins.vim", concat!(
        "call setline(1, '口x')\n",
        "let b:answer = 42\n",
        "let g:id = win_getid()\n",
        "let g:width = winwidth(0)\n",
        "let g:height = winheight(0)\n",
        "let g:attr = screenattr(1, 1)\n",
        "let g:char = screenchar(1, 1)\n",
        "let g:chars = screenchars(1, 1)\n",
        "let g:text = screenstring(1, 1)\n",
        "let g:missing = screenchar(-1, -1)\n",
        "let g:bufvar = getbufvar(bufnr('%'), 'answer')\n",
        "let g:command = fullcommand('res')\n",
        "let g:event = eventhandler()\n",
    )).unwrap();

    assert_eq!(global_number(exec.scope(), "id"), Some(i64::from(window)));
    assert_eq!(global_number(exec.scope(), "width"), Some(20));
    assert_eq!(global_number(exec.scope(), "height"), Some(6));
    assert_eq!(global_number(exec.scope(), "attr"), Some(0));
    assert_eq!(global_number(exec.scope(), "char"), Some(i64::from('口' as u32)));
    assert_eq!(global_number(exec.scope(), "missing"), Some(-1));
    assert_eq!(global_number(exec.scope(), "bufvar"), Some(42));
    assert_eq!(global_string(exec.scope(), "command").as_deref(), Some("resize"));
    assert_eq!(global_number(exec.scope(), "event"), Some(0));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"chars", 0).unwrap(), &Typval::list(vec![Typval::Number(i64::from('口' as u32))]));
    assert_eq!(global_string(exec.scope(), "text").as_deref(), Some("口"));
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

/// `expand()` applies a `:`-modifier chain after a special token the way
/// upstream `f_expand` does: `eval_vars` resolves the token base
/// (ex_docmd.c:7551), then `modify_fname` (eval/fs.c:69) — here
/// `ox_eval::apply_filename_modifiers` — eats the rest. This is the
/// termdebug shape: `expand('%:p')` inside the `-break-insert` command
/// (test_plugin_termdebug.vim Test_termdebug_break_command_builder).
///
/// The buffer name is absolute and non-existent so `:p` is deterministic:
/// `absolute_name` only consults the filesystem (`fs::canonicalize`) for
/// paths that exist. An empty base short-circuits to "" — upstream's
/// `eval_vars` marks the result invalid and `f_expand` returns "".
#[test]
fn expand_builtin_applies_filename_modifiers_to_special_tokens() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    let directory = std::env::temp_dir().join(format!("ox-expand-mod-{}", std::process::id()));
    let stored = directory.join("XTD_break_cmd.c");
    let stored = stored.to_string_lossy().into_owned();
    editor.buffer_mut(buffer).unwrap().set_name(OxStr::from(stored.as_str()));
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "let g:exact = expand('%')").unwrap();
    exec.execute_line(&mut editor, "let g:absolute = expand('%:p')").unwrap();
    exec.execute_line(&mut editor, "let g:head = expand('%:p:h')").unwrap();
    exec.execute_line(&mut editor, "let g:tail = expand('%:t')").unwrap();

    assert_eq!(global_string(exec.scope(), "exact").as_deref(), Some(stored.as_str()));
    assert_eq!(global_string(exec.scope(), "absolute").as_deref(), Some(stored.as_str()));
    let parent = directory.to_string_lossy().into_owned();
    assert_eq!(global_string(exec.scope(), "head").as_deref(), Some(parent.as_str()));
    assert_eq!(global_string(exec.scope(), "tail").as_deref(), Some("XTD_break_cmd.c"));

    // An unnamed buffer has an empty base: both the exact token and the
    // modifier chain yield "". `<afile>` outside an autocommand is the
    // non-path token with the same empty-base rule.
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "let g:blank = expand('%')").unwrap();
    exec.execute_line(&mut editor, "let g:blank_p = expand('%:p')").unwrap();
    exec.execute_line(&mut editor, "let g:afile = expand('<afile>')").unwrap();
    exec.execute_line(&mut editor, "let g:afile_h = expand('<afile>:h')").unwrap();

    assert_eq!(global_string(exec.scope(), "blank").as_deref(), Some(""));
    assert_eq!(global_string(exec.scope(), "blank_p").as_deref(), Some(""));
    assert_eq!(global_string(exec.scope(), "afile").as_deref(), Some(""));
    assert_eq!(global_string(exec.scope(), "afile_h").as_deref(), Some(""));
}

/// While a FileReadPre action runs, `<amatch>`, `<afile>`, `<afile>:h`, and
/// `<abuf>` resolve from the active autocmd context installed for that action.
#[test]
fn expand_builtin_resolves_autocmd_special_tokens_during_event() {
    let mut editor = editor_with_lines(&["initial"]);
    let buffer = editor.current_buffer().unwrap();
    let io = MemoryFileIO::new();
    io.insert("dir/target.txt", "content\n");
    let mut exec = ExExecutor::with_io(io);

    for (pattern, body) in [
        ("*.txt", "let g:m = expand('<amatch>')"),
        ("*.txt", "let g:f = expand('<afile>')"),
        ("*.txt", "let g:fh = expand('<afile>:h')"),
        ("*.txt", "let g:b = expand('<abuf>')"),
    ] {
        editor
            .autocmds_mut()
            .register(
                Event::FileReadPre,
                pattern,
                AutocmdKind::ExString(body.to_owned()),
                AutocmdOptions::default(),
            )
            .unwrap();
    }
    exec.execute_line(&mut editor, "1read dir/target.txt").unwrap();

    let expected_match = std::env::current_dir()
        .unwrap()
        .join("dir/target.txt")
        .to_string_lossy()
        .into_owned();
    assert_eq!(global_string(exec.scope(), "m").as_deref(), Some(expected_match.as_str()));
    assert_eq!(global_string(exec.scope(), "f").as_deref(), Some("dir/target.txt"));
    assert_eq!(global_string(exec.scope(), "fh").as_deref(), Some("dir"));
    let expected_buf = i64::from(buffer).to_string();
    assert_eq!(global_string(exec.scope(), "b").as_deref(), Some(expected_buf.as_str()));
}

/// Nested FileReadPre actions replace then restore the outer active context,
/// and outside any event the autocmd tokens stay empty.
#[test]
fn expand_builtin_restores_autocmd_context_after_nested_event() {
    let mut editor = editor_with_lines(&["initial"]);
    let io = MemoryFileIO::new();
    io.insert("outer.txt", "outer\n");
    io.insert("nested.txt", "nested\n");
    let mut exec = ExExecutor::with_io(io);

    for (pattern, body) in [
        ("nested.txt", "let g:inner_file = expand('<afile>')"),
        ("outer.txt", "let g:outer_pre = expand('<afile>')"),
        ("outer.txt", "1read nested.txt"),
        ("outer.txt", "let g:outer_post = expand('<afile>')"),
    ] {
        editor
            .autocmds_mut()
            .register(
                Event::FileReadPre,
                pattern,
                AutocmdKind::ExString(body.to_owned()),
                AutocmdOptions::default(),
            )
            .unwrap();
    }
    exec.execute_line(&mut editor, "1read outer.txt").unwrap();

    assert_eq!(global_string(exec.scope(), "outer_pre").as_deref(), Some("outer.txt"));
    assert_eq!(global_string(exec.scope(), "inner_file").as_deref(), Some("nested.txt"));
    assert_eq!(global_string(exec.scope(), "outer_post").as_deref(), Some("outer.txt"));

    exec.execute_line(&mut editor, "let g:outside = expand('<afile>')").unwrap();
    assert_eq!(global_string(exec.scope(), "outside").as_deref(), Some(""));
}

/// `expand()` resolves `~` and `$NAME` the way `ExpandOne` does, through
/// `expand_env_esc` (`os/env.c`).
///
/// Returning the pattern verbatim is not a smaller answer: `setup.vim`:115
/// builds its sandbox with `expand(getcwd() . '/XfakeHOME')`, and a caller that
/// hands an unexpanded `~` to a shell lets the *shell* expand it against its
/// own environment instead. HOME is a throwaway path here and nothing is
/// written to it.
#[test]
fn expand_builtin_resolves_home_and_environment_variables() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let mut exec = ExExecutor::new();
    let sandbox = std::env::temp_dir().join(format!("ox-editor-expand-{}", std::process::id()));
    let sandbox = sandbox.to_string_lossy().into_owned();
    let restore = std::env::var_os("HOME");
    ox_sys::set_env("HOME", &sandbox);
    ox_sys::set_env("OXVIM_TEST_EXPAND_DIR", "/sentinel");

    exec.execute_script(
        &mut editor,
        "<expand>",
        "let g:tilde = expand('~')\n\
         let g:tilde_path = expand('~/XfakeHOME')\n\
         let g:dollar = expand('$OXVIM_TEST_EXPAND_DIR/x')\n\
         let g:braced = expand('${OXVIM_TEST_EXPAND_DIR}/y')\n\
         let g:unset = expand('$OXVIM_TEST_EXPAND_MISSING/z')\n\
         let g:interior = expand('/keep/~/as-is')",
    )
    .unwrap();

    let values: Vec<Option<String>> = ["tilde", "tilde_path", "dollar", "braced", "unset", "interior"]
        .iter()
        .map(|name| global_string(exec.scope(), name))
        .collect();
    assert!(match restore {
        Some(home) => ox_sys::set_env("HOME", home),
        None => ox_sys::unset_env("HOME"),
    });
    ox_sys::unset_env("OXVIM_TEST_EXPAND_DIR");

    assert_eq!(values[0].as_deref(), Some(sandbox.as_str()));
    assert_eq!(values[1].as_deref(), Some(format!("{sandbox}/XfakeHOME").as_str()));
    assert_eq!(values[2].as_deref(), Some("/sentinel/x"));
    assert_eq!(values[3].as_deref(), Some("/sentinel/y"));
    // An unset variable stays literal, as `vim_getenv` returning NULL leaves it.
    assert_eq!(values[4].as_deref(), Some("$OXVIM_TEST_EXPAND_MISSING/z"));
    // Only a leading `~` is a home reference.
    assert_eq!(values[5].as_deref(), Some("/keep/~/as-is"));
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

#[test]
fn redir_silent_function_pattern_lists_matching_signatures() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        "function Test_Alpha(required, optional = {'x': 1}) abort\nendfunction\nfunction Test_Beta(...) range\nendfunction\nfunction Other()\nendfunction",
    );

    script(&mut exec, &mut editor, "redir @q\nsilent function /^Test_\nredir END");

    assert!(editor.messages().is_empty());
    assert_eq!(
        register_text(&editor, 'q'),
        "function Test_Alpha(required, optional = {'x': 1}) abort\nfunction Test_Beta(...) range"
    );
}

#[test]
fn redirected_function_list_global_substitute_rewrites_every_signature() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    script(
        &mut exec,
        &mut editor,
        "function Test_Alpha()\nendfunction\nfunction Test_Beta()\nendfunction\nfunction Test_Gamma()\nendfunction",
    );
    script(
        &mut exec,
        &mut editor,
        concat!(
            "redir @q\n",
            "silent function /^Test_\n",
            "redir END\n",
            "let g:listed = substitute(@q, 'function \\(\\k*()\\)', '\\1', 'g')"
        ),
    );
    assert_eq!(
        global_string(exec.scope(), "listed").as_deref(),
        Some("Test_Alpha()\nTest_Beta()\nTest_Gamma()")
    );
}

#[test]
fn redir_register_replaces_appends_and_keeps_unsilenced_output_visible() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_line(&mut editor, "let @q = 'old'").unwrap();
    exec.execute_line(&mut editor, "redir @q").unwrap();
    assert_eq!(register_text(&editor, 'q'), "");
    exec.execute_line(&mut editor, "echo 'first'").unwrap();
    assert_eq!(register_text(&editor, 'q'), "first");
    exec.execute_line(&mut editor, "redir END").unwrap();
    assert_eq!(register_text(&editor, 'q'), "first");
    assert_eq!(editor.messages().len(), 1);

    script(&mut exec, &mut editor, "redir @q>>\nsilent echo 'second'\nredir END");
    assert_eq!(register_text(&editor, 'q'), "firstsecond");
    assert_eq!(editor.messages().len(), 1);

    script(&mut exec, &mut editor, "redir @w\nsilent echon 'a'\nsilent echon 'b'\nredir END");
    assert_eq!(register_text(&editor, 'w'), "ab");
}

#[test]
fn redir_variable_replaces_then_appends() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());

    exec.execute_line(&mut editor, "let g:captured = 'old'").unwrap();
    exec.execute_line(&mut editor, "redir => g:captured").unwrap();
    assert_eq!(global_string(exec.scope(), "captured").as_deref(), Some(""));
    exec.execute_line(&mut editor, "silent echo 'one'").unwrap();
    assert_eq!(global_string(exec.scope(), "captured").as_deref(), Some(""));
    exec.execute_line(&mut editor, "redir END").unwrap();
    assert_eq!(global_string(exec.scope(), "captured").as_deref(), Some("one"));

    script(&mut exec, &mut editor, "redir =>> g:captured\nsilent echo 'two'\nredir END");
    assert_eq!(global_string(exec.scope(), "captured").as_deref(), Some("onetwo"));
}

#[test]
fn redir_file_replaces_then_appends() {
    let io = MemoryFileIO::new();
    io.insert("capture.txt", "old");
    let files = Rc::clone(&io.files);
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(io);

    exec.execute_line(&mut editor, "redir > capture.txt").unwrap();
    assert_eq!(files.borrow().get(Path::new("capture.txt")).map(String::as_str), Some(""));
    exec.execute_line(&mut editor, "silent echo 'one'").unwrap();
    assert_eq!(files.borrow().get(Path::new("capture.txt")).map(String::as_str), Some("one"));
    exec.execute_line(&mut editor, "redir END").unwrap();
    assert_eq!(files.borrow().get(Path::new("capture.txt")).map(String::as_str), Some("one"));
    script(&mut exec, &mut editor, "redir >> capture.txt\nsilent echo 'two'\nredir END");
}

#[test]
fn nested_redir_is_e930_and_preserves_active_target() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::with_io(MemoryFileIO::new());
    exec.execute_line(&mut editor, "redir @q").unwrap();

    let error = exec.execute_line(&mut editor, "redir @w").unwrap_err();
    assert_eq!(error_code(&error), "E930");

    exec.execute_line(&mut editor, "silent echo 'still active'").unwrap();
    exec.execute_line(&mut editor, "redir END").unwrap();
    assert_eq!(register_text(&editor, 'q'), "still active");
    assert!(editor.registers().get('w').unwrap().is_none());
}

#[test]
fn function_builtin_constructs_named_and_bound_references() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &mut editor,
        "function-ref.vim",
        "let g:plain = function('extend')\nlet g:bound = function('extend', [1], {'answer': 42})",
    ).unwrap();
    let Typval::Funcref(plain) = exec.scope().get_scoped(ScopeKind::Global, b"plain", 0).unwrap() else { panic!("expected plain Funcref") };
    assert_eq!(plain.name, OxStr::from("extend"));
    let Typval::Partial(bound) = exec.scope().get_scoped(ScopeKind::Global, b"bound", 0).unwrap() else { panic!("expected bound Partial") };
    assert_eq!(bound.args, vec![Typval::Number(1)]);
    assert!(matches!(bound.dict.as_deref(), Some([(key, Typval::Number(42))]) if key == &OxStr::from("answer")));
}

#[test]
fn function_builtin_reports_name_and_binding_errors() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    for (line, code) in [
        ("call function('MissingFunction')", "E700"),
        ("call function('extend', [], 1)", "E922"),
        ("call function('extend', 1)", "E923"),
    ] {
        let error = exec.execute_line(&mut editor, line).unwrap_err();
        assert_eq!(error_code(&error), code, "{line}");
    }
}

// eval/userfunc.c handle_defer_one 3487-3524, called from call_user_func's
// cleanup (1272) — `writefile(..., 'D')` deletes its file when the enclosing
// function returns, whatever the outcome was, and the nesting is per frame.
//
// Four cases, each isolating one part of the contract so no part can be
// dropped: the file must exist *inside* the frame (so a defer that deletes
// immediately fails), be gone *after* it (so a defer that never runs fails), an
// inner frame's defer must not take the outer frame's file with it (so a single
// shared list fails), and a frame that aborts must still delete (so running the
// deletes only on the success path fails).
#[test]
fn writefile_defer_flag_deletes_per_frame_on_return_and_on_abort() {
    let root = std::env::temp_dir().join(format!("ox-editor-defer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let base = root.display().to_string();

    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &mut editor,
        "defer.vim",
        &format!(
            "func Inner()\n\
             call writefile(['i'], '{base}/inner', 'D')\n\
             let g:inner_inside = filereadable('{base}/inner')\n\
             endfunc\n\
             func Outer()\n\
             call writefile(['o'], '{base}/outer', 'D')\n\
             call Inner()\n\
             let g:inner_after = filereadable('{base}/inner')\n\
             let g:outer_inside = filereadable('{base}/outer')\n\
             endfunc\n\
             call Outer()\n\
             let g:outer_after = filereadable('{base}/outer')\n\
             func Aborts()\n\
             call writefile(['a'], '{base}/aborted', 'D')\n\
             throw 'boom'\n\
             endfunc\n\
             try\n\
             call Aborts()\n\
             catch\n\
             let g:caught = v:exception\n\
             endtry\n\
             let g:aborted_after = filereadable('{base}/aborted')"
        ),
    )
    .unwrap();

    let flag = |name: &[u8]| exec.scope().get_scoped(ScopeKind::Global, name, 0).cloned();
    assert_eq!(flag(b"inner_inside"), Ok(Typval::Number(1)), "file missing inside its own frame");
    assert_eq!(flag(b"inner_after"), Ok(Typval::Number(0)), "inner frame's defer did not run");
    assert_eq!(flag(b"outer_inside"), Ok(Typval::Number(1)), "inner frame took the outer frame's file");
    assert_eq!(flag(b"outer_after"), Ok(Typval::Number(0)), "outer frame's defer did not run");
    assert!(matches!(&flag(b"caught"), Ok(Typval::String(text)) if text.to_string_lossy().contains("boom")));
    assert_eq!(flag(b"aborted_after"), Ok(Typval::Number(0)), "an aborted frame's defer did not run");

    let _ = std::fs::remove_dir_all(&root);
}

// eval/funcs.c f_system through os_system, os/shell.c shell_build_argv 60-97 —
// a String command runs through 'shell' + 'shellcmdflag', the second argument
// is the child's standard input and its pipe is closed, and a shell that
// cannot be spawned is reported through v:shell_error rather than raised.
//
// The E677 this replaces was fatal: `test_cmdline.vim` poisons $PATH and never
// restores it, so `system()` in runtest.vim's cleanup aborted FinishTesting()
// before it wrote `messages`, losing a 45-test record.
//
// One case per part: the input argument (a `cat` that echoes it back proves
// both that the input arrives and that its pipe is closed, since `cat` would
// otherwise never exit), 'shellcmdflag' (a flag the option names and nothing
// else supplies), and an unreachable 'shell' (which must not raise and must
// report -1).
#[test]
fn system_uses_the_shell_options_feeds_input_and_never_raises_on_a_bad_shell() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &mut editor,
        "system.vim",
        "set shell=/bin/sh shellcmdflag=-c\n\
         let g:echoed = system('echo ok')\n\
         let g:fed = system('cat', '123')\n\
         let g:listed = systemlist('cat', '123')\n\
         set shellcmdflag=-cx\n\
         let g:traced = system('true')\n\
         let g:traced_error = v:shell_error\n\
         set shellcmdflag=-c\n\
         set shell=/nonexistent/ox-no-shell\n\
         let g:missing = system('echo unreachable')\n\
         let g:missing_error = v:shell_error",
    )
    .unwrap();

    let global = |name: &[u8]| exec.scope().get_scoped(ScopeKind::Global, name, 0).cloned();
    assert_eq!(global(b"echoed"), Ok(Typval::String(OxStr::from("ok\n"))));
    // The input argument reaches the child and its pipe is closed afterwards.
    assert_eq!(global(b"fed"), Ok(Typval::String(OxStr::from("123"))));
    assert_eq!(global(b"listed"), Ok(Typval::list(vec![Typval::String(OxStr::from("123"))])));
    // `-cx` traces to standard error, which `os_system` merges into the output,
    // so only a 'shellcmdflag' that is actually read produces this.
    assert!(
        matches!(&global(b"traced"), Ok(Typval::String(text)) if text.to_string_lossy().contains("true")),
        "shellcmdflag was not used: {:?}",
        global(b"traced")
    );
    assert_eq!(global(b"traced_error"), Ok(Typval::Number(0)));
    // An unreachable 'shell' is `v:shell_error` == -1 and no exception.
    assert_eq!(global(b"missing"), Ok(Typval::String(OxStr::from(""))));
    assert_eq!(global(b"missing_error"), Ok(Typval::Number(-1)));
}