//! Integration tests for exact Vim register key encodings.

use ox_editor::{
    Editor, ExExecutor, K_SPECIAL, KE_FILLER, KS_SPECIAL, Keys, RegisterKind, TestEditorAccess,
};
use ox_types::{Object, OxStr, Typval};

fn string(value: &str) -> Typval {
    Typval::String(OxStr::from(value))
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "the integration test must stop when register setup or assertions fail"
)]
fn register_builtins_preserve_exact_kinds_append_and_errors() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();

    executor
        .execute_line(&editor, "let @a = \"line\\n\"")
        .unwrap();
    {
        let ed = editor.editor();
        let linewise = ed.registers().get('a').unwrap().unwrap();
        assert_eq!(linewise.kind(), RegisterKind::LineWise);
        assert_eq!(linewise.getreg_bytes(), b"line\n");
    }

    executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![string("b"), string("wide"), string("b9")],
        )
        .unwrap();
    executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![string("B"), string("tail"), string("c")],
        )
        .unwrap();
    assert_eq!(
        executor
            .call_builtin(&editor, &OxStr::from("getregtype"), vec![string("b")])
            .unwrap(),
        string("v")
    );
    assert_eq!(
        executor
            .call_builtin(&editor, &OxStr::from("getreg"), vec![string("b")])
            .unwrap(),
        string("wide\ntail")
    );

    let two_lines = Typval::list(vec![string("one"), string("two")]);
    let error = executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![string("="), two_lines, string("v")],
        )
        .unwrap_err();
    assert!(error.to_string().contains("E883"), "{error}");
    let error = executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![Typval::list(Vec::new()), Typval::Number(2)],
        )
        .unwrap_err();
    assert!(error.to_string().contains("E730"), "{error}");
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "the integration test must stop when script execution or lookup fails"
)]
fn modified_cjk_and_special_keys_keep_vim_internal_bytes() {
    let expected = vec![
        K_SPECIAL, 0xfc, 0x08, 0xe2, K_SPECIAL, KS_SPECIAL, KE_FILLER, 0xa6, K_SPECIAL, b'k', b'2',
    ];
    assert_eq!(
        Keys::parse_notation("<M-…><F2>", "\\", "\\").as_bytes(),
        expected
    );

    let raw = [K_SPECIAL, 0xfc, 0x08, 0xe2, K_SPECIAL, 0xa6];
    assert_eq!(
        Keys::escape_ks(&raw).as_bytes(),
        [
            K_SPECIAL, 0xfc, 0x08, 0xe2, K_SPECIAL, KS_SPECIAL, KE_FILLER, 0xa6
        ]
    );

    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    executor
        .execute_script(
            &editor,
            "modified.vim",
            "nnoremap <M-…> <Cmd>let g:seen += 1<CR>\n\
         let g:seen = 0\n\
         call feedkeys(\"\\<M-…>\", 'xt')\n\
         func KeyExpr()\n\
           return \"\\<M-…>\"\n\
         endfunc\n\
         nmap <expr> <F2> KeyExpr()\n\
         call feedkeys(\"\\<F2>\", 'xt')",
        )
        .unwrap();
    assert_eq!(
        executor.evaluate_expression(&editor, "g:seen").unwrap(),
        Typval::Number(2)
    );
}

#[test]
fn key_notation_simplification_drops_zero_modifier_frame() {
    assert_eq!(
        Keys::parse_notation("<C-m>", "\\", "\\").as_bytes(),
        &[0x0d]
    );
    assert_eq!(
        Keys::parse_notation("<C-[>", "\\", "\\").as_bytes(),
        &[0x1b]
    );
    assert_eq!(Keys::parse_notation("<S-a>", "\\", "\\").as_bytes(), b"A");
}

#[test]
fn escape_ks_preserves_arbitrary_complete_k_special_triple() {
    let raw = [K_SPECIAL, 0x41, 0xff];
    assert_eq!(Keys::escape_ks(&raw).as_bytes(), raw);
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "the integration test must stop when register setup or lookup fails"
)]
fn let_register_cr_triggers_linewise_while_keeping_cr_content() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    executor
        .execute_line(&editor, r#"let @a = "abc\r""#)
        .unwrap();
    {
        let ed = editor.editor();
        let content = ed.registers().get('a').unwrap().unwrap();
        assert_eq!(content.kind(), RegisterKind::LineWise);
        assert_eq!(content.getreg_bytes(), b"abc\r\n");
        assert_eq!(content.getreg_lines(), vec![b"abc\r".to_vec()]);
    }
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "the integration test must stop when register setup or assertions fail"
)]
fn getreginfo_reports_line_character_and_block_shapes() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();

    executor
        .execute_line(&editor, "let @a = \"one\\ntwo\\n\"")
        .unwrap();
    let info = executor
        .call_builtin(&editor, &OxStr::from("getreginfo"), vec![string("a")])
        .unwrap();
    assert_eq!(
        info,
        Typval::dict(vec![
            (
                OxStr::from("regcontents"),
                Typval::list(vec![string("one"), string("two")]),
            ),
            (OxStr::from("regtype"), string("V")),
            (OxStr::from("isunnamed"), Typval::Bool(false)),
        ])
    );

    executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![string("c"), string("text"), string("v")],
        )
        .unwrap();
    let info = executor
        .call_builtin(&editor, &OxStr::from("getreginfo"), vec![string("c")])
        .unwrap();
    assert_eq!(
        info,
        Typval::dict(vec![
            (
                OxStr::from("regcontents"),
                Typval::list(vec![string("text")]),
            ),
            (OxStr::from("regtype"), string("v")),
            (OxStr::from("isunnamed"), Typval::Bool(false)),
        ])
    );

    executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![string("b"), string("wide"), string("b9")],
        )
        .unwrap();
    let info = executor
        .call_builtin(&editor, &OxStr::from("getreginfo"), vec![string("b")])
        .unwrap();
    assert_eq!(
        info,
        Typval::dict(vec![
            (
                OxStr::from("regcontents"),
                Typval::list(vec![string("wide")]),
            ),
            (OxStr::from("regtype"), string("\u{16}9")),
            (OxStr::from("isunnamed"), Typval::Bool(false)),
        ])
    );
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "the integration test must stop when register setup or assertions fail"
)]
fn getreginfo_tracks_unnamed_alias_and_overwritten_target() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();

    executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![string("z"), string("val"), string("u")],
        )
        .unwrap();
    let expected_unnamed = Typval::dict(vec![
        (
            OxStr::from("regcontents"),
            Typval::list(vec![string("val")]),
        ),
        (OxStr::from("regtype"), string("v")),
        (OxStr::from("points_to"), string("z")),
    ]);
    for name in ["\"", "", "@"] {
        let info = executor
            .call_builtin(&editor, &OxStr::from("getreginfo"), vec![string(name)])
            .unwrap();
        assert_eq!(info, expected_unnamed, "getreginfo({name:?})");
    }

    executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![string("z"), string("next")],
        )
        .unwrap();
    let info = executor
        .call_builtin(&editor, &OxStr::from("getreginfo"), vec![string("\"")])
        .unwrap();
    assert_eq!(
        info,
        Typval::dict(vec![
            (
                OxStr::from("regcontents"),
                Typval::list(vec![string("next")]),
            ),
            (OxStr::from("regtype"), string("v")),
            (OxStr::from("points_to"), string("z")),
        ])
    );
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "the integration test must stop when register setup or assertions fail"
)]
fn getreginfo_handles_expression_empty_and_black_hole_registers() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();

    executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![string("="), string("1+1")],
        )
        .unwrap();
    let info = executor
        .call_builtin(&editor, &OxStr::from("getreginfo"), vec![string("=")])
        .unwrap();
    assert_eq!(
        info,
        Typval::dict(vec![
            (
                OxStr::from("regcontents"),
                Typval::list(vec![string("1+1")]),
            ),
            (OxStr::from("regtype"), string("v")),
            (OxStr::from("isunnamed"), Typval::Bool(false)),
        ])
    );

    for name in ["y", "!", "%", "#"] {
        let info = executor
            .call_builtin(&editor, &OxStr::from("getreginfo"), vec![string(name)])
            .unwrap();
        assert_eq!(info, Typval::dict(Vec::new()), "getreginfo({name:?})");
    }

    let info = executor
        .call_builtin(&editor, &OxStr::from("getreginfo"), vec![string("_")])
        .unwrap();
    assert_eq!(
        info,
        Typval::dict(vec![
            (OxStr::from("regcontents"), Typval::list(vec![string("")]),),
            (OxStr::from("regtype"), string("v")),
            (OxStr::from("isunnamed"), Typval::Bool(false)),
        ])
    );
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "the integration test must stop when register setup or assertions fail"
)]
fn getreginfo_honors_exact_names_vregister_and_errors() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();

    executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![string("a"), string("oneslot"), string("u")],
        )
        .unwrap();
    for (name, isunnamed) in [("A", false), ("a", true)] {
        let info = executor
            .call_builtin(&editor, &OxStr::from("getreginfo"), vec![string(name)])
            .unwrap();
        assert_eq!(
            info,
            Typval::dict(vec![
                (
                    OxStr::from("regcontents"),
                    Typval::list(vec![string("oneslot")]),
                ),
                (OxStr::from("regtype"), string("v")),
                (OxStr::from("isunnamed"), Typval::Bool(isunnamed)),
            ]),
            "getreginfo({name:?})"
        );
    }

    executor
        .call_builtin(
            &editor,
            &OxStr::from("setreg"),
            vec![string("b"), string("wide"), string("b9")],
        )
        .unwrap();
    editor
        .editor_mut()
        .vvars_mut()
        .insert(OxStr::from("register"), Object::String(OxStr::from("b")));
    let info = executor
        .call_builtin(&editor, &OxStr::from("getreginfo"), vec![])
        .unwrap();
    assert_eq!(
        info,
        Typval::dict(vec![
            (
                OxStr::from("regcontents"),
                Typval::list(vec![string("wide")]),
            ),
            (OxStr::from("regtype"), string("\u{16}9")),
            (OxStr::from("isunnamed"), Typval::Bool(false)),
        ])
    );

    let error = executor
        .call_builtin(
            &editor,
            &OxStr::from("getreginfo"),
            vec![string("a"), string("b")],
        )
        .unwrap_err();
    assert!(error.to_string().contains("E118"), "{error}");

    let error = executor
        .call_builtin(
            &editor,
            &OxStr::from("getreginfo"),
            vec![Typval::list(Vec::new())],
        )
        .unwrap_err();
    assert!(error.to_string().contains("E730"), "{error}");

    let error = executor
        .call_builtin(
            &editor,
            &OxStr::from("getreginfo"),
            vec![Typval::dict(Vec::new())],
        )
        .unwrap_err();
    assert!(error.to_string().contains("E731"), "{error}");
}
