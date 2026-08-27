//! Pure indentation engine tests grounded in `test/old/testdir/test_cindent.vim`
//! and Neovim's `get_c_indent()` / `get_lisp_indent()` contracts.

use ox_text::Buffer;

use crate::indent::{
    self, CinTrigger, Cino, IndentAmount, IndentExprError, IndentOptions, Method, cinkeys_trigger,
    cindent, indent_columns, lisp_indent, resolve_method, resolve_options_method, whitespace_for,
};
use crate::{Editor, ExprEval, IndentEvalContext, NullExprEval, OptionValue};

fn lines(text: &str) -> Vec<Vec<u8>> {
    text.lines().map(str::as_bytes).map(<[u8]>::to_vec).collect()
}

fn cols(amount: IndentAmount) -> usize {
    match amount {
        IndentAmount::Columns(value) => value,
        IndentAmount::LeaveAlone => panic!("expected column amount, got LeaveAlone"),
    }
}

fn cino_opts() -> IndentOptions {
    IndentOptions {
        shiftwidth: 4,
        tabstop: 4,
        expandtab: true,
        cindent: true,
        autoindent: true,
        cinoptions: Cino::parse("", 4),
        ..IndentOptions::default()
    }
}

fn capture_buffer_options(editor: &Editor, buffer: ox_types::BufHandle) -> IndentOptions {
    IndentOptions::capture(editor, buffer)
}

#[test]
fn shiftwidth_zero_resolves_to_tabstop_when_captured() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"{").unwrap(), true)
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "shiftwidth", OptionValue::Number(0))
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "tabstop", OptionValue::Number(4))
        .unwrap();
    let opts = capture_buffer_options(&editor, buffer);
    assert_eq!(opts.shiftwidth, 4);
    let source = lines("{\nstmt;\n}");
    assert_eq!(cols(cindent(&source, 2, &opts)), 4);
}

#[test]
fn expandtab_uses_spaces_and_noexpandtab_uses_tabs() {
    let spaces = IndentOptions {
        shiftwidth: 8,
        tabstop: 8,
        expandtab: true,
        ..IndentOptions::default()
    };
    assert_eq!(whitespace_for(8, b"", &spaces), b"        ");

    let tabs = IndentOptions {
        shiftwidth: 8,
        tabstop: 8,
        expandtab: false,
        ..IndentOptions::default()
    };
    assert_eq!(whitespace_for(8, b"", &tabs), b"\t");
    assert_eq!(whitespace_for(10, b"\t", &tabs), b"\t  ");
}

#[test]
fn preserveindent_reuses_existing_leading_whitespace() {
    let opts = IndentOptions {
        shiftwidth: 4,
        tabstop: 8,
        expandtab: false,
        preserveindent: true,
        ..IndentOptions::default()
    };
    assert_eq!(whitespace_for(10, b"\t  ", &opts), b"\t  \t");
}

#[test]
fn copyindent_reuses_source_whitespace_on_new_lines() {
    let opts = IndentOptions {
        shiftwidth: 4,
        tabstop: 8,
        expandtab: false,
        copyindent: true,
        ..IndentOptions::default()
    };
    let source = b"\t    body";
    assert_eq!(
        indent::smart_newline_indent(source, false, &opts),
        b"\t    "
    );
}

#[test]
fn sibling_lines_inside_brace_share_block_indent_not_offset_ramp() {
    // `test_cindent.vim` Test_cindent_01 first brace block.
    let source = lines(
        "{\n\
         if (test)\n\
         \tcmd1;\n\
         cmd2;\n\
         }",
    );
    let opts = cino_opts();
    assert_eq!(cols(cindent(&source, 2, &opts)), 4);
    assert_eq!(cols(cindent(&source, 3, &opts)), 8);
    assert_eq!(cols(cindent(&source, 4, &opts)), 4);
    assert_ne!(cols(cindent(&source, 3, &opts)), cols(cindent(&source, 4, &opts)));
}

#[test]
fn case_and_default_labels_use_cinoptions_offsets() {
    let source = lines(
        "switch (x) {\n\
         case 1:\n\
         stmt;\n\
         default:\n\
         break;\n\
         }",
    );
    let opts = cino_opts();
    assert_eq!(cols(cindent(&source, 2, &opts)), 4);
    assert_eq!(cols(cindent(&source, 3, &opts)), 8);
    assert_eq!(cols(cindent(&source, 4, &opts)), 4);
    assert_eq!(cols(cindent(&source, 5, &opts)), 8);
}

#[test]
fn continuation_line_indents_after_unclosed_control_head() {
    let source = lines(
        "{\n\
         if (test)\n\
         cmd1;\n\
         cmd2;\n\
         }",
    );
    let opts = cino_opts();
    assert_eq!(cols(cindent(&source, 3, &opts)), 8);
    assert_eq!(cols(cindent(&source, 4, &opts)), 4);
}

#[test]
fn line_comment_and_block_comment_indents_are_observable() {
    let block = lines("{\n/* block\n * line\n */");
    let opts = cino_opts();
    assert_eq!(cols(cindent(&block, 2, &opts)), 4);
    assert_eq!(cols(cindent(&block, 3, &opts)), 4);

    let line_comment = lines("{\n// comment\nstmt;\n}");
    assert_eq!(cols(cindent(&line_comment, 2, &opts)), 4);
    assert_eq!(cols(cindent(&line_comment, 3, &opts)), 4);
}

#[test]
fn rawstring_interior_leaves_indent_alone() {
    // `test_cindent.vim` Test_cindent_rawstring: statement after raw string closes.
    let source = lines(
        "int main() {\n\
         R\"(\n\
         )\";\n\
         statement;",
    );
    let opts = cino_opts();
    match cindent(&source, 2, &opts) {
        IndentAmount::LeaveAlone => {}
        other => panic!("raw string line should stay untouched, got {other:?}"),
    }
    assert_eq!(cols(cindent(&source, 4, &opts)), 4);
}

#[test]
fn paren_expression_indents_from_opening_paren() {
    let source = lines(
        "func(a,\n\
         b,\n\
         c);",
    );
    let opts = cino_opts();
    assert_eq!(cols(cindent(&source, 2, &opts)), 4);
    assert_eq!(cols(cindent(&source, 3, &opts)), 4);
}

#[test]
fn lisp_indent_aligns_body_to_opening_list() {
    let source = lines("(defun foo ()\n  (bar\n   baz))");
    let opts = IndentOptions {
        shiftwidth: 2,
        tabstop: 8,
        expandtab: true,
        lisp: true,
        autoindent: true,
        ..IndentOptions::default()
    };
    assert_eq!(cols(lisp_indent(&source, 2, &opts)), 2);
    assert_eq!(cols(lisp_indent(&source, 3, &opts)), 3);
}

#[test]
fn cinkeys_and_indentkeys_gate_open_line_triggers() {
    let mut opts = cino_opts();
    assert!(cinkeys_trigger(&opts, CinTrigger::OpenForward));
    assert!(cinkeys_trigger(&opts, CinTrigger::OpenBrace));
    assert!(cinkeys_trigger(&opts, CinTrigger::Colon));

    opts.indentexpr = "MyIndent()".to_owned();
    opts.indentkeys = "0{,0}".to_owned();
    assert!(!cinkeys_trigger(&opts, CinTrigger::OpenForward));
    assert!(cinkeys_trigger(&opts, CinTrigger::OpenBrace));
}

#[test]
fn method_precedence_prefers_lisp_expr_cindent_and_equalprg() {
    let mut opts = IndentOptions::default();
    assert_eq!(resolve_options_method(&opts), Method::Cindent);

    opts.lisp = true;
    assert_eq!(resolve_options_method(&opts), Method::Lisp);

    opts.lispoptions_expr = true;
    opts.indentexpr = "MyIndent()".to_owned();
    assert_eq!(resolve_options_method(&opts), Method::Expr);

    opts.lisp = false;
    assert_eq!(resolve_options_method(&opts), Method::Expr);

    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "equalprg", OptionValue::String("cat".to_owned()))
        .unwrap();
    assert_eq!(resolve_method(&editor, buffer), None);
}

#[test]
fn indent_columns_counts_tabs_with_tabstop() {
    let opts = IndentOptions {
        tabstop: 8,
        ..IndentOptions::default()
    };
    assert_eq!(indent_columns(b"\t  x", &opts), 10);
}

#[test]
fn null_expr_eval_is_usable_from_mode_layer() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    let mut eval = NullExprEval;
    let source = lines("{\nstmt;\n}");
    let opts = cino_opts();
    let context = IndentEvalContext::new(&editor, buffer, &source);
    assert_eq!(
        indent::amount_for(
            &context,
            2,
            Method::Cindent,
            &opts,
            &mut eval,
        )
        .unwrap(),
        IndentAmount::Columns(4)
    );
}

#[test]
fn expression_error_propagates_from_amount_for() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    let mut eval = NullExprEval;
    let source = lines("stmt;");
    let opts = IndentOptions {
        indentexpr: "UnavailableExpr()".to_owned(),
        ..IndentOptions::default()
    };
    let context = IndentEvalContext::new(&editor, buffer, &source);
    let result = indent::amount_for(
        &context,
        1,
        Method::Expr,
        &opts,
        &mut eval,
    );
    assert!(matches!(result, Err(IndentExprError::Failed(_))));
}

#[test]
fn closing_brace_outdents_to_scope_base() {
    let source = lines("{\nif (test)\n    cmd1;\n}");
    let opts = cino_opts();
    assert_eq!(cols(cindent(&source, 4, &opts)), 0);
}

#[test]
fn blank_line_inside_block_keeps_block_indent() {
    let source = lines("{\n    stmt;\n\n    cmd;");
    let opts = cino_opts();
    assert_eq!(cols(cindent(&source, 3, &opts)), 4);
}

#[test]
fn open_forward_indents_brace_member_by_block_indent() {
    // `o` from the line after `{` pre-splits into a blank line; `fix_line_indent`
    // must give the block indent (shiftwidth), not a leftover-line ramp.
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    let mut eval = NullExprEval;
    let source = lines("{\nstmt;");
    let opts = cino_opts();
    let context = IndentEvalContext::new(&editor, buffer, &source);
    let whitespace = indent::fix_line_indent(
        &context,
        2,
        CinTrigger::OpenForward,
        &opts,
        &mut eval,
    )
    .unwrap()
    .unwrap();
    assert_eq!(whitespace, b"    ");
}

#[test]
fn existing_line_after_opening_brace_uses_block_indent() {
    // Existing-line amount (T64/`<C-F>`): the line after `{` is a block member,
    // not a leftover-line ramp and not a continuation of the brace line itself.
    let source = lines("int {\n1M1");
    let opts = IndentOptions {
        shiftwidth: 2,
        tabstop: 8,
        expandtab: true,
        cindent: true,
        autoindent: true,
        cinoptions: Cino::parse("", 2),
        ..IndentOptions::default()
    };
    assert_eq!(cols(cindent(&source, 2, &opts)), 2);
}

#[test]
fn existing_line_continuation_inside_brace_adds_one_shiftwidth() {
    // Existing-line amount (T66/`=`): an unterminated prior member is a
    // continuation of the first statement in the block, not of the `{` line.
    // Blank lines between members are skipped; a `}` outdents to the opener.
    let source = lines("int {\n1M1\n\n2M2\n}");
    let opts = IndentOptions {
        shiftwidth: 2,
        tabstop: 8,
        expandtab: true,
        cindent: true,
        autoindent: true,
        cinoptions: Cino::parse("", 2),
        ..IndentOptions::default()
    };
    assert_eq!(cols(cindent(&source, 2, &opts)), 2);
    assert_eq!(cols(cindent(&source, 4, &opts)), 4);
    assert_eq!(cols(cindent(&source, 5, &opts)), 0);
    let staged = lines("int {\n  1M1\n\n2M2\n}");
    assert_eq!(cols(cindent(&staged, 4, &opts)), 4);
}

#[test]
fn existing_line_reindent_matches_open_forward_after_brace() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    let mut eval = NullExprEval;
    let opts = cino_opts();
    let existing = lines("{\nstmt;");
    let context = IndentEvalContext::new(&editor, buffer, &existing);
    let reindent = indent::amount_for(&context, 2, Method::Cindent, &opts, &mut eval).unwrap();
    let open_forward = indent::fix_line_indent(
        &context,
        2,
        CinTrigger::OpenForward,
        &opts,
        &mut eval,
    )
    .unwrap()
    .unwrap();
    assert_eq!(reindent, IndentAmount::Columns(4));
    assert_eq!(open_forward, b"    ");
}

#[test]
fn amount_for_expr_reads_context_view() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(ox_text::Buffer::from_bytes(b"aaaa\nbbbb").unwrap(), true)
        .unwrap();
    // Live buffer stays unindented; staged overlay already shows a 4-space lead
    // on line 1 that the expression for line 2 must observe.
    let staged = vec![b"    aaaa".to_vec(), b"bbbb".to_vec()];
    let context = IndentEvalContext::new(&editor, buffer, &staged);
    struct PriorLeadEval;
    impl ExprEval for PriorLeadEval {
        fn eval_indentexpr(
            &mut self,
            context: &IndentEvalContext<'_>,
            lnum: usize,
            _expression: &str,
        ) -> Result<i64, IndentExprError> {
            assert!(lnum >= 2);
            let prior = &context.lines()[lnum - 2];
            let lead = prior.iter().take_while(|b| b.is_ascii_whitespace()).count();
            Ok(i64::try_from(lead).unwrap())
        }
    }
    let mut eval = PriorLeadEval;
    let opts = IndentOptions {
        indentexpr: "PriorLead()".to_owned(),
        expandtab: true,
        shiftwidth: 4,
        tabstop: 4,
        ..IndentOptions::default()
    };
    assert_eq!(
        indent::amount_for(&context, 2, Method::Expr, &opts, &mut eval).unwrap(),
        IndentAmount::Columns(4)
    );
    // Error propagation through the context signature is unchanged.
    let mut fail = NullExprEval;
    assert!(matches!(
        indent::amount_for(&context, 2, Method::Expr, &opts, &mut fail),
        Err(IndentExprError::Failed(_))
    ));
}
