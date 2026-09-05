use ox_eval::ScopeKind;
use ox_types::Typval;

use crate::excmd_exec::{ExExecutor, ExecError, VimExceptionKind};
use crate::{Editor, TestEditorAccess, VimException};

fn exec_error_code(error: ExecError) -> String {
    match error {
        ExecError::Vim(VimException {
            kind: VimExceptionKind::Error(code),
            ..
        }) => code,
        other => panic!("expected vim exception, got {other:?}"),
    }
}

#[test]
#[expect(clippy::unwrap_used, reason = "test asserts expected success outcomes")]
fn lvalue_dict_member_and_index_assignment() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:d = {'k': 1}").unwrap();
    exec.execute_line(&editor, "let g:d.k = 9").unwrap();
    exec.execute_line(&editor, "let g:check = g:d['k']")
        .unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"check", 0),
        Ok(&Typval::Number(9))
    );
}

#[test]
#[expect(clippy::unwrap_used, reason = "test asserts expected success outcomes")]
fn lvalue_list_index_supports_negative_indices() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:l = [10, 20, 30]")
        .unwrap();
    exec.execute_line(&editor, "let g:l[-1] = 99").unwrap();
    exec.execute_line(&editor, "let g:check = g:l[-1]").unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"check", 0),
        Ok(&Typval::Number(99))
    );
}

#[test]
#[expect(clippy::unwrap_used, reason = "test asserts expected success outcomes")]
fn lvalue_list_bounded_slice_replaces_in_place() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:l = [1, 2, 3, 4]")
        .unwrap();
    exec.execute_line(&editor, "let g:l[1:2] = [8, 9]").unwrap();
    let check = exec.scope().get_scoped(ScopeKind::Global, b"l", 0).unwrap();
    let Typval::List(list) = check else {
        panic!("expected list")
    };
    assert_eq!(
        list.borrow().items,
        vec![
            Typval::Number(1),
            Typval::Number(8),
            Typval::Number(9),
            Typval::Number(4)
        ]
    );
}

#[test]
#[expect(clippy::unwrap_used, reason = "test asserts expected success outcomes")]
fn lvalue_list_bounded_slice_negative_bounds() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:l = [1, 2, 3, 4]")
        .unwrap();
    exec.execute_line(&editor, "let g:l[-2:-1] = [88, 99]")
        .unwrap();
    let check = exec.scope().get_scoped(ScopeKind::Global, b"l", 0).unwrap();
    let Typval::List(list) = check else {
        panic!("expected list")
    };
    assert_eq!(
        list.borrow().items,
        vec![
            Typval::Number(1),
            Typval::Number(2),
            Typval::Number(88),
            Typval::Number(99)
        ]
    );
}

#[test]
#[expect(clippy::unwrap_used, reason = "test asserts expected error codes")]
fn lvalue_list_slice_exact_length_errors() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:l = [1, 2, 3, 4]")
        .unwrap();

    let err = exec
        .execute_line(&editor, "let g:l[1:2] = [8]")
        .unwrap_err();
    assert_eq!(exec_error_code(err), "E712");

    let err = exec
        .execute_line(&editor, "let g:l[1:2] = [8, 9, 10]")
        .unwrap_err();
    assert_eq!(exec_error_code(err), "E710");
}

#[test]
#[expect(clippy::unwrap_used, reason = "test asserts expected success outcomes")]
fn lvalue_list_unbounded_extends() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:l = [1, 2, 3, 4]")
        .unwrap();
    exec.execute_line(&editor, "let g:l[3:] = [8, 9]").unwrap();
    let check = exec.scope().get_scoped(ScopeKind::Global, b"l", 0).unwrap();
    let Typval::List(list) = check else {
        panic!("expected list")
    };
    assert_eq!(
        list.borrow().items,
        vec![
            Typval::Number(1),
            Typval::Number(2),
            Typval::Number(3),
            Typval::Number(8),
            Typval::Number(9),
        ]
    );
}

#[test]
#[expect(clippy::unwrap_used, reason = "test asserts expected success outcomes")]
fn lvalue_destructure_rest_tail() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let [g:a, g:b; g:rest] = [1, 2, 3]")
        .unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"a", 0),
        Ok(&Typval::Number(1))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"b", 0),
        Ok(&Typval::Number(2))
    );
    let rest = exec
        .scope()
        .get_scoped(ScopeKind::Global, b"rest", 0)
        .unwrap();
    let Typval::List(rest) = rest else {
        panic!("expected list")
    };
    assert_eq!(rest.borrow().items, vec![Typval::Number(3)]);
}

#[test]
#[expect(clippy::unwrap_used, reason = "test asserts expected error code")]
fn lvalue_rejects_subscripted_register_with_e488() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    let err = exec.execute_line(&editor, "let @a[0] = 'x'").unwrap_err();
    assert_eq!(exec_error_code(err), "E488");
}
#[test]
#[expect(clippy::unwrap_used, reason = "test asserts expected error code")]
fn lvalue_unlet_option_reports_e488_for_option_target() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    // `command_unlet` rejects `&`-prefixed targets at `unlet_name_garbage`
    // before the lvalue remove path (which reports E518) is ever reached, so
    // the dispatch-visible code is E488, not E518.
    let err = exec.execute_line(&editor, "unlet &report").unwrap_err();
    assert_eq!(exec_error_code(err), "E488");
}

#[test]
#[expect(clippy::unwrap_used, reason = "test asserts expected error code")]
fn lvalue_locked_list_element_reports_e741() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:l = [1, 2]").unwrap();
    exec.execute_line(&editor, "lockvar g:l").unwrap();
    let err = exec.execute_line(&editor, "let g:l[0] = 5").unwrap_err();
    assert_eq!(exec_error_code(err), "E741");
}
