#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Scoped verification for the language-core contracts landed with the
//! `test_eval_stuff` apply pass (E963/vvars, curly names, recursion and
//! opener error precedence, `silent!`, `:execute` coercions, `:for`
//! iterables/E1098, scriptversion concat, bare `version`).

use crate::{Editor, ExExecutor, ExecError, ExecOutcome, TestEditorAccess};
use ox_types::Typval;

fn vim_error_message(result: Result<ExecOutcome, ExecError>) -> String {
    match result {
        Err(ExecError::Vim(exception)) => exception.message().clone(),
        Err(other) => panic!("expected ExecError::Vim, got {other:?}"),
        Ok(_) => panic!("expected error, got Ok"),
    }
}

#[test]
fn vvar_type_errors_match_before_set_vvar() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    executor
        .execute_script(&editor, "t.vim", "let v_o = v:oldfiles")
        .unwrap();
    assert!(
        executor
            .execute_script(&editor, "t.vim", "let v:oldfiles = ''")
            .is_err()
    );
    let message = vim_error_message(executor.execute_script(&editor, "t.vim", "let v:errors = ''"));
    assert!(message.contains("E963"), "got {message}");
    assert!(
        message.contains("Setting v:errors to value with wrong type"),
        "got {message}"
    );
    let message = vim_error_message(executor.execute_script(&editor, "t.vim", "let v:errmsg = []"));
    assert!(message.contains("E730"), "got {message}");
    // Same-type assignments keep working.
    executor
        .execute_script(&editor, "t.vim", "let v:errors = [1]")
        .unwrap();
    executor
        .execute_script(&editor, "t.vim", "let v:errmsg = 'ok'")
        .unwrap();
}

#[test]
fn numeric_and_version_vvars_are_seeded() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    executor.execute_script(
        &editor,
        "t.vim",
        "let g:a = [v:numbersize, v:numbermax > 9999999, v:numbermin < -9999999, v:version, v:versionlong / 10000, v:versionlong > 8011525]",
    ).unwrap();
    let Typval::List(values) = executor
        .scope()
        .get_scoped(ox_eval::ScopeKind::Global, "a".as_bytes(), 0)
        .unwrap()
        .clone()
    else {
        panic!("g:a should be a list");
    };
    let items = values.borrow().items.clone();
    assert!(matches!(items[0], Typval::Number(64)));
    assert!(matches!(items[1], Typval::Number(1)));
    assert!(matches!(items[2], Typval::Number(1)));
    assert!(matches!(items[3], Typval::Number(801)));
    assert!(matches!(items[4], Typval::Number(801)));
    assert!(matches!(items[5], Typval::Number(1)));
}

#[test]
fn bare_version_reads_vvar_and_is_read_only() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    executor
        .execute_script(&editor, "t.vim", "let g:v = version")
        .unwrap();
    assert!(matches!(
        executor
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, "v".as_bytes(), 0)
            .unwrap(),
        Typval::Number(801)
    ));
    let message = vim_error_message(executor.execute_script(&editor, "t.vim", "let version = 1"));
    assert!(message.contains("E46"), "got {message}");
    assert!(
        message.contains("Cannot change read-only variable \"version\""),
        "got {message}"
    );
}

#[test]
fn curly_brace_name_resolves_on_the_right_hand_side() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    executor
        .execute_script(&editor, "t.vim", "let g:gvar = 'gvar'\nlet gname = 'gvar'\nlet {'g:'.gname} = {'g:'.gname}\nlet g:out = s:nothing")
        .unwrap_err();
    // `let {'g:'.gname} = {'g:'.gname}` reads g:gvar through the curly name.
    executor
        .execute_script(
            &editor,
            "t.vim",
            "let g:gvar = 'gvar'\nlet gname = 'gvar'\nlet { 'g:' . gname } = { 'g:' . gname }",
        )
        .unwrap();
    let Typval::String(value) = executor
        .scope()
        .get_scoped(ox_eval::ScopeKind::Global, "gvar".as_bytes(), 0)
        .unwrap()
        .clone()
    else {
        panic!("g:gvar should be a string");
    };
    assert_eq!(value.to_string_lossy(), "gvar");
}

#[test]
fn deep_recursion_reports_e1169_over_missing_endif() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    let source = format!("if {}", "(".repeat(1002));
    let message = vim_error_message(executor.execute_script(&editor, "t.vim", &source));
    assert!(
        message.contains("E1169: Expression too recursive: (("),
        "got {message}"
    );
}

#[test]
fn silent_bang_suppresses_command_errors() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    executor
        .execute_script(&editor, "t.vim", "silent! echo 0{1-$\"x\"\nlet g:after = 1")
        .unwrap();
    assert!(matches!(
        executor
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, "after".as_bytes(), 0)
            .unwrap(),
        Typval::Number(1)
    ));
}

#[test]
fn execute_rejects_containers_with_string_errors() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    for (source, code) in [
        ("execute v:_null_list", "E730"),
        ("execute v:_null_dict", "E731"),
        ("execute v:_null_blob", "E976"),
    ] {
        let message = vim_error_message(executor.execute_script(&editor, "t.vim", source));
        assert!(message.contains(code), "{source} gave {message}");
    }
    executor
        .execute_script(&editor, "t.vim", "execute v:_null_string")
        .unwrap();
}

#[test]
fn for_iterates_strings_and_blobs_and_rejects_others() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    executor
        .execute_script(
            &editor,
            "t.vim",
            "let g:n = 0\nfor c in v:_null_string\nlet g:n += 1\nendfor\nfor b in 0z0102\nlet g:n += b\nendfor",
        )
        .unwrap();
    assert!(matches!(
        executor
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, "n".as_bytes(), 0)
            .unwrap(),
        Typval::Number(3)
    ));
    for source in ["for x in 99", "for x in {'a': 9}"] {
        let message = vim_error_message(executor.execute_script(&editor, "t.vim", source));
        assert!(message.contains("E1098"), "{source} gave {message}");
        assert!(
            message.contains("String, List or Blob required"),
            "{source} gave {message}"
        );
    }
}

#[test]
fn scriptversion_one_concatenates_float_shaped_numbers() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    executor
        .execute_script(&editor, "t.vim", "let vers = 1.2.3")
        .unwrap();
    let Typval::String(value) = executor
        .scope()
        .get_scoped(ox_eval::ScopeKind::Global, "vers".as_bytes(), 0)
        .unwrap()
        .clone()
    else {
        panic!("g:vers should be a string");
    };
    assert_eq!(value.to_string_lossy(), "123");
    let message = vim_error_message(executor.execute_script(&editor, "t.vim", "let f = .5"));
    assert!(message.contains("E15"), "got {message}");
}

#[test]
fn null_blob_survives_vvar_version_churn() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());

    // Seed the scope so that editor.vvars receives the round-tripped
    // _null_blob (as Object::Array([]) — the Typval::Blob → Object mapping).
    // After this call sync_scope_into_editor has stamped scope.synced[Vim]
    // to the editor's vvars_version, so the stamp is NOT stale yet.
    executor
        .execute_script(&editor, "t.vim", "let v:testing = 1")
        .unwrap();

    // Bump the editor vvars version directly, WITHOUT going through
    // execute_script (which would re-stamp via sync_scope_into_editor).
    // This makes scope.synced[Vim] stale relative to editor.vvars_version(),
    // so the next execute_script's sync_editor_into_scope will REBUILD
    // scope.vim from editor.vvars() — where _null_blob is Object::Array([])
    // (a List, type 3), not a Blob (type 10).  The production repair at
    // sync_editor_into_scope must overwrite it back to Typval::Blob([]).
    let _ = editor.editor_mut().vvars_mut();

    // After the forced rebuild, v:_null_blob must still be an empty Blob.
    executor
        .execute_script(
            &editor,
            "t.vim",
            "let g:blobtype = type(v:_null_blob)\nlet g:blobstring = string(v:_null_blob)\nlet g:bloblen = len(v:_null_blob)",
        )
        .unwrap();

    let scope = executor.scope();
    let blobtype = scope
        .get_scoped(ox_eval::ScopeKind::Global, b"blobtype", 0)
        .unwrap()
        .clone();
    assert!(
        matches!(blobtype, Typval::Number(10)),
        "type should be Blob"
    );

    let blobstring = scope
        .get_scoped(ox_eval::ScopeKind::Global, b"blobstring", 0)
        .unwrap()
        .clone();
    assert!(
        matches!(blobstring, Typval::String(s) if s.to_string_lossy() == "0z"),
        "string should be 0z"
    );

    let bloblen = scope
        .get_scoped(ox_eval::ScopeKind::Global, b"bloblen", 0)
        .unwrap()
        .clone();
    assert!(matches!(bloblen, Typval::Number(0)), "len should be 0");

    let v_blob = scope
        .get_scoped(ox_eval::ScopeKind::Vim, b"_null_blob", 0)
        .unwrap()
        .clone();
    assert!(
        matches!(v_blob, Typval::Blob(bytes) if bytes.is_empty()),
        "v:_null_blob should be an empty Blob"
    );

    // :execute on a Blob reports E976, not E121 (undefined) or a display string.
    let message =
        vim_error_message(executor.execute_script(&editor, "t.vim", "execute v:_null_blob"));
    assert!(
        message.contains("E976"),
        "execute v:_null_blob gave {message}"
    );
    assert!(
        message.contains("Using a Blob as a String"),
        "execute v:_null_blob gave {message}"
    );

    // Direct assignment remains read-only E46.
    let message =
        vim_error_message(executor.execute_script(&editor, "t.vim", "let v:_null_blob = 0z01"));
    assert!(message.contains("E46"), "let v:_null_blob gave {message}");
    assert!(
        message.contains("Cannot change read-only variable"),
        "let v:_null_blob gave {message}"
    );
}

#[test]
fn curly_target_expands_quoted_and_nested_names() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    // `make_expanded_name` (eval.c:5769): the `}` inside the quoted piece
    // must not close the group, and a group body may hold a full
    // expression — here a dict literal with member access.
    executor
        .execute_script(
            &editor,
            "t.vim",
            "let g:{'a}b'} = 1\nlet g:{ {'k': 'ey'}.k } = 5",
        )
        .unwrap();
    assert_eq!(
        executor
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, b"a}b", 0)
            .unwrap(),
        &Typval::Number(1)
    );
    assert_eq!(
        executor
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, b"ey", 0)
            .unwrap(),
        &Typval::Number(5)
    );
}

#[test]
fn curly_name_propagates_expression_recursion_error() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    // A committed expression error inside the braces surfaces as E1169; it
    // must not be masked by a dictionary fallback.
    let source = format!("let g:x = {{{}", "(".repeat(1002));
    let message = vim_error_message(executor.execute_script(&editor, "t.vim", &source));
    assert!(message.contains("E1169"), "got {message}");
}

#[test]
fn for_iterates_one_byte_on_malformed_utf8() {
    let mut executor = ExExecutor::new();
    let editor = TestEditorAccess::new(Editor::new());
    // `list2str` builds invalid UTF-8: two stray continuation bytes must
    // yield two one-byte items (not one grouped item), a truncated
    // sequence yields one byte per byte, and valid text stays per-scalar.
    executor
        .execute_script(
            &editor,
            "t.vim",
            "let g:n = 0\nfor c in list2str([128, 129])\nlet g:n += 1\nendfor\nlet g:t = 0\nfor c in list2str([226, 130, 97])\nlet g:t += 1\nendfor\nlet g:v = 0\nfor c in 'héllo'\nlet g:v += 1\nendfor",
        )
        .unwrap();
    assert_eq!(
        executor
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, b"n", 0)
            .unwrap(),
        &Typval::Number(2)
    );
    assert_eq!(
        executor
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, b"t", 0)
            .unwrap(),
        &Typval::Number(3)
    );
    assert_eq!(
        executor
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, b"v", 0)
            .unwrap(),
        &Typval::Number(5)
    );
}
