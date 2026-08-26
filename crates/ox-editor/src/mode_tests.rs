//! Keystroke-sequence behavior oracle.
//!
//! Families derive from `test/old/testdir/test_normal.vim`, `test_visual.vim`,
//! `test_textobjects.vim`, and `test_search.vim`.

use ox_text::{Buffer, Position};

use crate::extmark::{ExtmarkGravity, ExtmarkPlacement, ExtmarkPosition};
use crate::indent::{ExprEval, IndentEvalContext, IndentExprError};
use crate::insert::InsertError;
use crate::ops::OperatorError;
use crate::{
    BufferRelease, Editor, Geometry, InsertState, Keys, MapMode, MappingAction, MappingOptions,
    Mode, ModeError, ModeMachine, NullExprEval, OptionValue, TypeaheadFlags,
};

fn position(lnum: usize, col: usize) -> Position { Position { lnum, col } }

fn run(text: &str, cursor: Position, keys: &str) -> (String, Position, &'static str, Editor, ox_types::BufHandle) {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true).unwrap();
    let tab = editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, cursor).unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, keys, &mut eval).unwrap();
    let output = String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap();
    let cursor = editor.window(window).unwrap().cursor;
    let mode = match machine.mode() { Mode::Normal(_) => "normal", Mode::Insert(_) => "insert", Mode::Visual(_) => "visual", Mode::Cmdline(_) => "cmdline", Mode::OperatorPending(_) => "operator" };
    (output, cursor, mode, editor, buffer)
}

macro_rules! behavior {
    ($name:ident, $text:expr, $cursor:expr, $keys:expr, $expected:expr, $after:expr, $mode:expr) => {
        #[test]
        fn $name() {
            let (text, cursor, mode, _, _) = run($text, $cursor, $keys);
            assert_eq!(text, $expected);
            assert_eq!(cursor, $after);
            assert_eq!(mode, $mode);
        }
    };
}

behavior!(delete_word, "one two", position(1,0), "dw", "two", position(1,0), "normal");
behavior!(delete_end_word, "one two", position(1,0), "de", " two", position(1,0), "normal");
behavior!(delete_to_line_end, "one two", position(1,4), "d$", "one ", position(1,3), "normal");
behavior!(delete_line, "one\ntwo\nthree", position(2,0), "dd", "one\nthree", position(2,0), "normal");
behavior!(delete_two_lines, "one\ntwo\nthree", position(1,0), "2dd", "three", position(1,0), "normal");
behavior!(change_inner_quote, "a \"two\" b", position(1,4), "ci\"X\u{1b}", "a \"X\" b", position(1,3), "normal");
behavior!(delete_inner_parens, "a (two) b", position(1,4), "di(", "a () b", position(1,3), "normal");
behavior!(delete_around_parens, "a (two) b", position(1,4), "da(", "a  b", position(1,2), "normal");
behavior!(delete_inner_word, "one two", position(1,5), "diw", "one ", position(1,3), "normal");
behavior!(delete_around_word, "one two three", position(1,5), "daw", "one three", position(1,4), "normal");
behavior!(visual_delete, "one", position(1,0), "vld", "e", position(1,0), "normal");
behavior!(visual_char_delete_with_x, "one", position(1,0), "vlx", "e", position(1,0), "normal");
behavior!(visual_char_delete_with_capital_x, "one", position(1,0), "vlX", "e", position(1,0), "normal");
behavior!(visual_line_delete, "one\ntwo\nthree", position(2,0), "Vd", "one\nthree", position(2,0), "normal");
behavior!(visual_swap_anchor, "one", position(1,0), "vlo", "one", position(1,0), "visual");
behavior!(insert_plain, "one", position(1,0), "iX\u{1b}", "Xone", position(1,0), "normal");
behavior!(append_plain, "one", position(1,0), "aX\u{1b}", "oXne", position(1,1), "normal");
behavior!(append_line, "one", position(1,0), "AX\u{1b}", "oneX", position(1,3), "normal");
behavior!(insert_newline, "one", position(1,1), "i\nt\u{1b}", "o\ntne", position(2,0), "normal");
behavior!(insert_backspace, "one", position(1,1), "i\u{8}\u{1b}", "ne", position(1,0), "normal");
behavior!(insert_backspace_join, "one\ntwo", position(2,0), "i\u{8}\u{1b}", "onetwo", position(1,2), "normal");
behavior!(move_left_by_unicode_scalars, "A한글あ漢Z", position(1,13), "3h", "A한글あ漢Z", position(1,4), "normal");
behavior!(move_right_by_unicode_scalars, "A한글あ漢Z", position(1,1), "3l", "A한글あ漢Z", position(1,10), "normal");
behavior!(combining_mark_motion_is_codepoint_based, "가\u{327}A", position(1,5), "h", "가\u{327}A", position(1,3), "normal");

#[test]
fn normal_put_dispatches_register_shapes_and_directions() {
    let cases = [
        (
            "one\ntwo",
            position(1, 0),
            "a",
            crate::RegisterContent::linewise(vec![b"inserted".to_vec()]).unwrap(),
            "P",
            "inserted\none\ntwo",
            position(1, 0),
        ),
        (
            "one\ntwo",
            position(1, 0),
            "a",
            crate::RegisterContent::linewise(vec![b"inserted".to_vec()]).unwrap(),
            "p",
            "one\ninserted\ntwo",
            position(2, 0),
        ),
        (
            "한글",
            position(1, 0),
            "a",
            crate::RegisterContent::characterwise(b"X").unwrap(),
            "p",
            "한X글",
            position(1, 3),
        ),
        (
            "abc\ndef",
            position(1, 1),
            "a",
            crate::RegisterContent::blockwise(vec![b"Q".to_vec(), b"R".to_vec()], 1).unwrap(),
            "P",
            "aQbc\ndRef",
            position(1, 1),
        ),
        (
            "abc\ndef",
            position(2, 1),
            "a",
            crate::RegisterContent::blockwise(
                vec![b"Q".to_vec(), b"R".to_vec(), b"S".to_vec()],
                1,
            )
            .unwrap(),
            "P",
            "abc\ndQef\n R\n S",
            position(2, 1),
        ),
    ];

    for (text, cursor, register, content, command, expected, expected_cursor) in cases {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true)
            .unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor
            .tabpage(editor.current_tabpage().unwrap())
            .unwrap()
            .current_window();
        editor.set_window_cursor(window, cursor).unwrap();
        editor.registers_mut().set('a', content).unwrap();

        let mut machine = ModeMachine::default();
        let mut eval = NullExprEval;
        machine
            .feed_keys(&mut editor, &format!("\"{register}{command}"), &mut eval)
            .unwrap();

        assert_eq!(
            String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
            expected
        );
        assert_eq!(editor.window(window).unwrap().cursor, expected_cursor);
    }
}

behavior!(
    normal_put_with_empty_register_is_noop,
    "one",
    position(1, 0),
    "\"zp",
    "one",
    position(1, 0),
    "normal"
);

#[test]
fn normal_put_applies_count_per_shape() {
    let cases = [
        (
            "ab",
            position(1, 0),
            crate::RegisterContent::characterwise(b"X").unwrap(),
            "3p",
            "aXXXb",
            position(1, 3),
        ),
        (
            "ab",
            position(1, 0),
            crate::RegisterContent::characterwise(b"X").unwrap(),
            "3P",
            "XXXab",
            position(1, 2),
        ),
        (
            "one\ntwo",
            position(1, 0),
            crate::RegisterContent::linewise(vec![b"  x".to_vec(), b"y".to_vec()]).unwrap(),
            "2p",
            "one\n  x\ny\n  x\ny\ntwo",
            position(2, 2),
        ),
        (
            "one\ntwo",
            position(1, 0),
            crate::RegisterContent::linewise(vec![b"  x".to_vec(), b"y".to_vec()]).unwrap(),
            "2P",
            "  x\ny\n  x\ny\none\ntwo",
            position(1, 2),
        ),
        (
            "abc\ndef",
            position(1, 1),
            crate::RegisterContent::blockwise(vec![b"Q".to_vec(), b"R".to_vec()], 1).unwrap(),
            "2P",
            "aQQbc\ndRRef",
            position(1, 1),
        ),
    ];

    for (text, cursor, content, keys, expected, expected_cursor) in cases {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true)
            .unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor
            .tabpage(editor.current_tabpage().unwrap())
            .unwrap()
            .current_window();
        editor.set_window_cursor(window, cursor).unwrap();
        editor.registers_mut().set('a', content).unwrap();

        let mut machine = ModeMachine::default();
        let mut eval = NullExprEval;
        machine
            .feed_keys(&mut editor, &format!("\"a{keys}"), &mut eval)
            .unwrap();

        assert_eq!(
            String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
            expected
        );
        assert_eq!(editor.window(window).unwrap().cursor, expected_cursor);
    }
}

#[test]
fn normal_put_cursor_on_first_byte_of_last_scalar() {
    let cases = [
        ("p", "a한Xb", position(1, 4)),
        ("2p", "a한X한Xb", position(1, 8)),
    ];

    for (keys, expected, expected_cursor) in cases {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"ab").unwrap(), true)
            .unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor
            .tabpage(editor.current_tabpage().unwrap())
            .unwrap()
            .current_window();
        editor.set_window_cursor(window, position(1, 0)).unwrap();
        editor
            .registers_mut()
            .set(
                'a',
                crate::RegisterContent::characterwise("한X".as_bytes()).unwrap(),
            )
            .unwrap();

        let mut machine = ModeMachine::default();
        let mut eval = NullExprEval;
        machine
            .feed_keys(&mut editor, &format!("\"a{keys}"), &mut eval)
            .unwrap();

        assert_eq!(
            String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
            expected
        );
        assert_eq!(editor.window(window).unwrap().cursor, expected_cursor);
    }
}

#[test]
fn normal_put_multiline_charwise_count_and_cursor() {
    let cases = [
        ("2p", "ax\nyx\nyb", position(1, 1)),
        ("2P", "x\nyx\nyab", position(1, 0)),
    ];

    for (keys, expected, expected_cursor) in cases {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"ab").unwrap(), true)
            .unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor
            .tabpage(editor.current_tabpage().unwrap())
            .unwrap()
            .current_window();
        editor.set_window_cursor(window, position(1, 0)).unwrap();
        editor
            .registers_mut()
            .set(
                'a',
                crate::RegisterContent::characterwise(b"x\ny").unwrap(),
            )
            .unwrap();

        let mut machine = ModeMachine::default();
        let mut eval = NullExprEval;
        machine
            .feed_keys(&mut editor, &format!("\"a{keys}"), &mut eval)
            .unwrap();

        assert_eq!(
            String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
            expected
        );
        assert_eq!(editor.window(window).unwrap().cursor, expected_cursor);
    }
}

#[test]
fn normal_blockwise_put_width_padding_with_count() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"abc\ndef").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    editor
        .registers_mut()
        .set(
            'a',
            crate::RegisterContent::blockwise(vec![b"Q".to_vec(), b"RS".to_vec()], 2).unwrap(),
        )
        .unwrap();

    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "\"a2p", &mut eval).unwrap();

    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "aQ Q bc\ndRSRSef"
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 1));
}

#[test]
fn normal_blockwise_put_eof_materializes_in_one_transaction() {
    let variants = [
        (
            "abc\ndef\nghi",
            "aQbc\ndRef\ngShi",
            [
                ExtmarkPosition::new(0, 2),
                ExtmarkPosition::new(0, 1),
                ExtmarkPosition::new(0, 4),
            ],
        ),
        (
            "abc",
            "aQbc\n R\n S",
            [
                ExtmarkPosition::new(0, 2),
                ExtmarkPosition::new(0, 1),
                ExtmarkPosition::new(0, 4),
            ],
        ),
    ];

    for (text, expected, expected_marks) in variants {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true)
            .unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor
            .tabpage(editor.current_tabpage().unwrap())
            .unwrap()
            .current_window();
        editor.set_window_cursor(window, position(1, 1)).unwrap();

        let ns = editor
            .buffer_mut(buffer)
            .unwrap()
            .extmarks
            .create_namespace("put-transaction")
            .unwrap();
        let mut right_boundary = ExtmarkPlacement::new(ExtmarkPosition::new(0, 1));
        right_boundary.gravity = ExtmarkGravity::Right;
        let mut left_boundary = ExtmarkPlacement::new(ExtmarkPosition::new(0, 1));
        left_boundary.gravity = ExtmarkGravity::Left;
        let mut after_insertion = ExtmarkPlacement::new(ExtmarkPosition::new(0, 3));
        after_insertion.gravity = ExtmarkGravity::Right;
        let right_id = editor
            .buffer_mut(buffer)
            .unwrap()
            .extmarks
            .set(ns, None, right_boundary)
            .unwrap();
        let left_id = editor
            .buffer_mut(buffer)
            .unwrap()
            .extmarks
            .set(ns, None, left_boundary)
            .unwrap();
        let after_id = editor
            .buffer_mut(buffer)
            .unwrap()
            .extmarks
            .set(ns, None, after_insertion)
            .unwrap();

        editor
            .registers_mut()
            .set(
                'a',
                crate::RegisterContent::blockwise(
                    vec![b"Q".to_vec(), b"R".to_vec(), b"S".to_vec()],
                    1,
                )
                .unwrap(),
            )
            .unwrap();

        let changelist_before = editor.changelists().len(buffer);
        let seq_before = editor.buffer(buffer).unwrap().undo.current_seq();
        let original = editor.buffer(buffer).unwrap().text().unwrap().to_bytes();
        let original_marks = [
            ExtmarkPosition::new(0, 1),
            ExtmarkPosition::new(0, 1),
            ExtmarkPosition::new(0, 3),
        ];

        let mut machine = ModeMachine::default();
        let mut eval = NullExprEval;
        machine.feed_keys(&mut editor, "\"aP", &mut eval).unwrap();

        assert_eq!(
            String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
            expected
        );
        assert_eq!(
            editor
                .buffer(buffer)
                .unwrap()
                .extmarks
                .get(ns, right_id)
                .unwrap()
                .unwrap()
                .position(),
            expected_marks[0]
        );
        assert_eq!(
            editor
                .buffer(buffer)
                .unwrap()
                .extmarks
                .get(ns, left_id)
                .unwrap()
                .unwrap()
                .position(),
            expected_marks[1]
        );
        assert_eq!(
            editor
                .buffer(buffer)
                .unwrap()
                .extmarks
                .get(ns, after_id)
                .unwrap()
                .unwrap()
                .position(),
            expected_marks[2]
        );
        assert_eq!(editor.changelists().len(buffer), changelist_before + 1);
        assert_eq!(
            editor.buffer(buffer).unwrap().undo.current_seq(),
            seq_before + 1
        );

        assert!(editor.buffer_undo(buffer).unwrap().is_some());
        assert_eq!(
            editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
            original
        );
        assert_eq!(
            editor
                .buffer(buffer)
                .unwrap()
                .extmarks
                .get(ns, right_id)
                .unwrap()
                .unwrap()
                .position(),
            original_marks[0]
        );
        assert_eq!(
            editor
                .buffer(buffer)
                .unwrap()
                .extmarks
                .get(ns, left_id)
                .unwrap()
                .unwrap()
                .position(),
            original_marks[1]
        );
        assert_eq!(
            editor
                .buffer(buffer)
                .unwrap()
                .extmarks
                .get(ns, after_id)
                .unwrap()
                .unwrap()
                .position(),
            original_marks[2]
        );
        assert!(editor.buffer_undo(buffer).unwrap().is_none());

        assert!(editor.buffer_redo(buffer).unwrap().is_some());
        assert_eq!(
            String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
            expected
        );
        assert_eq!(
            editor
                .buffer(buffer)
                .unwrap()
                .extmarks
                .get(ns, right_id)
                .unwrap()
                .unwrap()
                .position(),
            expected_marks[0]
        );
        assert_eq!(
            editor
                .buffer(buffer)
                .unwrap()
                .extmarks
                .get(ns, left_id)
                .unwrap()
                .unwrap()
                .position(),
            expected_marks[1]
        );
        assert_eq!(
            editor
                .buffer(buffer)
                .unwrap()
                .extmarks
                .get(ns, after_id)
                .unwrap()
                .unwrap()
                .position(),
            expected_marks[2]
        );
    }
}

#[test]
fn normal_blockwise_put_shifts_extmark_columns() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"abc\ndef").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 1)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("put-geometry")
        .unwrap();
    let id = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(
            ns,
            None,
            ExtmarkPlacement::new(ExtmarkPosition::new(0, 2)),
        )
        .unwrap();
    editor
        .registers_mut()
        .set(
            'a',
            crate::RegisterContent::blockwise(vec![b"Q".to_vec(), b"R".to_vec()], 1).unwrap(),
        )
        .unwrap();

    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "\"aP", &mut eval).unwrap();

    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "aQbc\ndRef"
    );
    let mark = editor
        .buffer(buffer)
        .unwrap()
        .extmarks
        .get(ns, id)
        .unwrap()
        .unwrap();
    assert_eq!(mark.position(), ExtmarkPosition::new(0, 3));
}

behavior!(adjust_number_preserves_embedded_token_neighbors, "abc 12 def", position(1,0), "\u{1}", "abc 13 def", position(1,5), "normal");
behavior!(adjust_number_applies_count, "10", position(1,0), "5\u{1}", "15", position(1,1), "normal");
behavior!(adjust_number_ctrl_x_decrements, "13", position(1,0), "\u{18}", "12", position(1,1), "normal");
behavior!(adjust_number_includes_sign_after_letters, "abc-9xxx", position(1,3), "\u{1}", "abc-8xxx", position(1,4), "normal");
behavior!(adjust_number_grows_when_order_of_magnitude_increases, "abc999xxx", position(1,2), "\u{1}", "abc1000xxx", position(1,6), "normal");
behavior!(adjust_number_hex_prefix, "0xff", position(1,0), "\u{1}", "0x100", position(1,4), "normal");
behavior!(adjust_number_bin_prefix, "0b11", position(1,0), "\u{1}", "0b100", position(1,4), "normal");
behavior!(adjust_number_prefers_nearest_decimal_over_later_hex, "9 0x10", position(1,0), "\u{1}", "10 0x10", position(1,1), "normal");
behavior!(adjust_number_ctrl_x_prefers_nearest_decimal_over_later_hex, "9 0x10", position(1,0), "\u{18}", "8 0x10", position(1,0), "normal");
behavior!(adjust_number_later_hex_wins_when_cursor_is_on_it, "9 0x10", position(1,2), "\u{1}", "9 0x11", position(1,5), "normal");
behavior!(adjust_number_hex_digit_run_is_hex_not_decimal, "9 0x19", position(1,4), "\u{1}", "9 0x1a", position(1,5), "normal");
behavior!(adjust_number_prefers_nearest_decimal_over_later_bin, "1 0b01", position(1,0), "\u{1}", "2 0b01", position(1,0), "normal");
behavior!(adjust_number_clamps_typed_count_to_upstream_max, "0", position(1,0), "9999999999\u{1}", "999999999", position(1,8), "normal");
behavior!(adjust_number_clamps_typed_count_ctrl_x, "0", position(1,0), "9999999999\u{18}", "-999999999", position(1,9), "normal");
behavior!(adjust_number_pads_leading_zeros_000, "000", position(1,0), "\u{1}", "001", position(1,2), "normal");
behavior!(adjust_number_pads_leading_zeros_007, "007", position(1,0), "\u{1}", "008", position(1,2), "normal");
behavior!(adjust_number_pads_leading_zeros_ctrl_x, "001", position(1,0), "\u{18}", "000", position(1,2), "normal");
behavior!(adjust_number_hex_prefixed_decrement_keeps_padding, "0x0ff", position(1,0), "\u{18}", "0x0fe", position(1,4), "normal");
behavior!(adjust_number_bin_prefixed_decrement_keeps_padding, "0b010", position(1,0), "\u{18}", "0b001", position(1,4), "normal");
behavior!(adjust_number_hex_case_follows_last_alpha_digit, "0xABc", position(1,0), "\u{1}", "0xabd", position(1,4), "normal");
behavior!(adjust_number_hex_case_follows_uppercase_marker, "0X10", position(1,0), "\u{1}", "0X11", position(1,3), "normal");
behavior!(adjust_number_hex_mixed_pair_last_lower, "0xAb", position(1,0), "\u{1}", "0xac", position(1,3), "normal");
behavior!(adjust_number_hex_mixed_pair_last_upper, "0xaB", position(1,0), "\u{1}", "0xAC", position(1,3), "normal");
behavior!(adjust_number_minus_excluded_from_pad_width, "-007", position(1,0), "\u{1}", "-006", position(1,3), "normal");
behavior!(adjust_number_i64_max_plus_one, "9223372036854775807", position(1,0), "\u{1}", "9223372036854775808", position(1,18), "normal");
behavior!(adjust_number_i64_min_minus_one, "-9223372036854775808", position(1,0), "\u{18}", "-9223372036854775809", position(1,19), "normal");
behavior!(adjust_number_u64_max_plus_one_wraps_negative, "18446744073709551615", position(1,0), "\u{1}", "-18446744073709551615", position(1,20), "normal");
behavior!(adjust_number_u64_overflow_parse_saturates_to_max, "18446744073709551616", position(1,0), "\u{18}", "18446744073709551615", position(1,19), "normal");
behavior!(adjust_number_u64_max_minus_one, "18446744073709551615", position(1,0), "\u{18}", "18446744073709551614", position(1,19), "normal");
behavior!(adjust_number_zero_from_negative, "-1", position(1,0), "\u{1}", "0", position(1,0), "normal");
behavior!(adjust_number_zero_minus_one_is_negative_one, "0", position(1,0), "\u{18}", "-1", position(1,1), "normal");
behavior!(adjust_number_i64_min_plus_one, "-9223372036854775808", position(1,0), "\u{1}", "-9223372036854775807", position(1,19), "normal");
#[test]
fn adjust_number_splices_exact_token_span_for_extmarks_undo_and_ticks() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"abc999xxx").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 2)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("number-geometry")
        .unwrap();
    let mark_at = |editor: &mut Editor, col: usize| {
        editor
            .buffer_mut(buffer)
            .unwrap()
            .extmarks
            .set(ns, None, ExtmarkPlacement::new(ExtmarkPosition::new(0, col)))
            .unwrap()
    };
    let before = mark_at(&mut editor, 2);
    let start = mark_at(&mut editor, 3);
    let inside = mark_at(&mut editor, 5);
    let old_end = mark_at(&mut editor, 6);
    let after = mark_at(&mut editor, 7);
    let tick = editor.buffer(buffer).unwrap().changedtick();
    let changelist = editor.changelists().len(buffer);

    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "\u{1}", &mut eval).unwrap();

    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "abc1000xxx"
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 6));
    let pos = |editor: &Editor, id| {
        editor
            .buffer(buffer)
            .unwrap()
            .extmarks
            .get(ns, id)
            .unwrap()
            .unwrap()
            .position()
    };
    assert_eq!(pos(&editor, before), ExtmarkPosition::new(0, 2));
    assert_eq!(pos(&editor, start), ExtmarkPosition::new(0, 7));
    assert_eq!(pos(&editor, inside), ExtmarkPosition::new(0, 7));
    assert_eq!(pos(&editor, old_end), ExtmarkPosition::new(0, 7));
    assert_eq!(pos(&editor, after), ExtmarkPosition::new(0, 8));
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), tick + 1);
    assert_eq!(editor.changelists().len(buffer), changelist + 1);

    machine.feed_keys(&mut editor, "u", &mut eval).unwrap();
    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "abc999xxx"
    );
    assert_eq!(pos(&editor, before), ExtmarkPosition::new(0, 2));
    assert_eq!(pos(&editor, start), ExtmarkPosition::new(0, 3));
    assert_eq!(pos(&editor, inside), ExtmarkPosition::new(0, 5));
    assert_eq!(pos(&editor, old_end), ExtmarkPosition::new(0, 6));
    assert_eq!(pos(&editor, after), ExtmarkPosition::new(0, 7));
}
#[test]
fn adjust_number_padded_hex_preserves_extmarks_undo_and_ticks() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"zz0x0ffyy").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 2)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("number-geometry-pad")
        .unwrap();
    let mark_at = |editor: &mut Editor, col: usize| {
        editor
            .buffer_mut(buffer)
            .unwrap()
            .extmarks
            .set(ns, None, ExtmarkPlacement::new(ExtmarkPosition::new(0, col)))
            .unwrap()
    };
    let before = mark_at(&mut editor, 1);
    let start = mark_at(&mut editor, 2);
    let inside = mark_at(&mut editor, 4);
    let old_end = mark_at(&mut editor, 7);
    let after = mark_at(&mut editor, 8);
    let tick = editor.buffer(buffer).unwrap().changedtick();
    let seq = editor.buffer(buffer).unwrap().undo.current_seq();

    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "\u{18}", &mut eval).unwrap();

    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "zz0x0feyy"
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 6));
    let pos = |editor: &Editor, id| {
        editor
            .buffer(buffer)
            .unwrap()
            .extmarks
            .get(ns, id)
            .unwrap()
            .unwrap()
            .position()
    };
    assert_eq!(pos(&editor, before), ExtmarkPosition::new(0, 1));
    assert_eq!(pos(&editor, start), ExtmarkPosition::new(0, 7));
    assert_eq!(pos(&editor, inside), ExtmarkPosition::new(0, 7));
    assert_eq!(pos(&editor, old_end), ExtmarkPosition::new(0, 7));
    assert_eq!(pos(&editor, after), ExtmarkPosition::new(0, 8));
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), tick + 1);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_seq(), seq + 1);

    machine.feed_keys(&mut editor, "u", &mut eval).unwrap();
    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "zz0x0ffyy"
    );
    assert_eq!(pos(&editor, before), ExtmarkPosition::new(0, 1));
    assert_eq!(pos(&editor, start), ExtmarkPosition::new(0, 2));
    assert_eq!(pos(&editor, inside), ExtmarkPosition::new(0, 4));
    assert_eq!(pos(&editor, old_end), ExtmarkPosition::new(0, 7));
    assert_eq!(pos(&editor, after), ExtmarkPosition::new(0, 8));
}

#[test]
fn adjust_number_overflow_parse_splices_max_and_undo() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"18446744073709551616").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("number-geometry-overflow")
        .unwrap();
    let mark = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(ns, None, ExtmarkPlacement::new(ExtmarkPosition::new(0, 5)))
        .unwrap();
    let tick = editor.buffer(buffer).unwrap().changedtick();
    let seq = editor.buffer(buffer).unwrap().undo.current_seq();

    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "\u{18}", &mut eval).unwrap();

    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "18446744073709551615"
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 19));
    assert_eq!(
        editor.buffer(buffer).unwrap().extmarks.get(ns, mark).unwrap().unwrap().position(),
        ExtmarkPosition::new(0, 20)
    );
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), tick + 1);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_seq(), seq + 1);

    machine.feed_keys(&mut editor, "u", &mut eval).unwrap();
    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "18446744073709551616"
    );
    assert_eq!(
        editor.buffer(buffer).unwrap().extmarks.get(ns, mark).unwrap().unwrap().position(),
        ExtmarkPosition::new(0, 5)
    );
}

behavior!(replace_count_beyond_remaining_characters_is_noop, "ab", position(1,1), "3rX", "ab", position(1,1), "normal");
behavior!(normal_replace_preserves_cursor_and_repeats_scalars, "abcd", position(1,1), "2rX", "aXXd", position(1,1), "normal");
behavior!(normal_replace_counts_cjk_scalars_not_bytes, "한글a", position(1,0), "2rX", "XXa", position(1,0), "normal");
behavior!(visual_charwise_replace_repeats_per_scalar, "abcd", position(1,1), "vlrX", "aXXd", position(1,1), "normal");
behavior!(visual_charwise_replace_counts_cjk_scalars, "한글a", position(1,0), "vlrX", "XXa", position(1,0), "normal");
behavior!(visual_blockwise_replace_per_line_scalars, "abcd\nefgh", position(1,0), "\u{16}ljrX", "XXcd\nXXgh", position(1,0), "normal");
#[test]
fn visual_blockwise_replace_moves_interior_extmark_with_right_gravity() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"12345\n12345").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(2, 0)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("replace-geometry")
        .unwrap();
    let id = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(
            ns,
            None,
            ExtmarkPlacement::new(ExtmarkPosition::new(0, 2)),
        )
        .unwrap();

    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine
        .feed_keys(&mut editor, "0\u{16}llkr1", &mut eval)
        .unwrap();

    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "11145\n11145"
    );
    let mark = editor
        .buffer(buffer)
        .unwrap()
        .extmarks
        .get(ns, id)
        .unwrap()
        .unwrap();
    assert_eq!(mark.position(), ExtmarkPosition::new(0, 3));
}
#[test]
fn normal_replace_preserves_exterior_extmark_columns() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"12345").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("replace-geometry")
        .unwrap();
    let id = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(
            ns,
            None,
            ExtmarkPlacement::new(ExtmarkPosition::new(0, 2)),
        )
        .unwrap();

    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "0r2", &mut eval).unwrap();

    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "22345"
    );
    let mark = editor
        .buffer(buffer)
        .unwrap()
        .extmarks
        .get(ns, id)
        .unwrap()
        .unwrap();
    assert_eq!(mark.position(), ExtmarkPosition::new(0, 2));
}

behavior!(normal_replace_typed_cr_inserts_one_line_break, "abcdef", position(1,1), "3r\r", "a\nef", position(2,0), "normal");
behavior!(normal_replace_typed_nl_inserts_one_line_break, "abcdef", position(1,1), "3r\n", "a\nef", position(2,0), "normal");
behavior!(normal_replace_typed_cr_multibyte_span, "a한글d", position(1,1), "2r\r", "a\nd", position(2,0), "normal");
behavior!(normal_replace_quoted_cr_embeds_literal_cr, "abcd", position(1,1), "r\u{16}\r", "a\rcd", position(1,1), "normal");
behavior!(normal_replace_quoted_nl_embeds_nul, "abcd", position(1,1), "r\u{16}\n", "a\x00cd", position(1,1), "normal");
behavior!(normal_replace_quoted_cr_via_ctrl_q, "abcd", position(1,1), "r\u{11}\r", "a\rcd", position(1,1), "normal");
behavior!(normal_replace_quote_pending_escape_aborts, "abcd", position(1,1), "r\u{16}\u{1b}", "abcd", position(1,1), "normal");
behavior!(normal_replace_quoted_ctrl_v_embeds_literal, "abcd", position(1,1), "r\u{16}\u{16}", "a\u{16}cd", position(1,1), "normal");
behavior!(normal_replace_typed_cr_beyond_remaining_is_noop, "ab", position(1,1), "3r\r", "ab", position(1,1), "normal");
behavior!(visual_charwise_typed_cr_stays_literal, "abcd", position(1,1), "vlr\r", "a\r\rd", position(1,1), "normal");
behavior!(visual_charwise_typed_nl_stays_nul, "abcd", position(1,1), "vlr\n", "a\x00\x00d", position(1,1), "normal");
behavior!(visual_linewise_typed_cr_stays_literal, "abcd", position(1,1), "Vr\r", "\r\r\r\r", position(1,0), "normal");
behavior!(visual_blockwise_quoted_cr_keeps_literal_cr, "98765\n98765\n98765", position(1,0), "02l\u{16}2jr\u{16}\r", "98\r65\n98\r65\n98\r65", position(1,2), "normal");
behavior!(visual_blockwise_quoted_nl_keeps_nul, "98765\n98765\n98765", position(1,0), "02l\u{16}2jr\u{16}\n", "98\x0065\n98\x0065\n98\x0065", position(1,2), "normal");

#[test]
fn normal_replace_typed_cr_full_invariants() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"abcdef").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 1)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("replace-break")
        .unwrap();
    let mut mark = ExtmarkPlacement::new(ExtmarkPosition::new(0, 2));
    mark.gravity = ExtmarkGravity::Right;
    let id = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(ns, None, mark)
        .unwrap();
    let tick = editor.buffer(buffer).unwrap().changedtick();
    let seq = editor.buffer(buffer).unwrap().undo.current_seq();
    let changelist = editor.changelists().len(buffer);
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "3r\r", &mut eval).unwrap();
    let text = editor.buffer(buffer).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"a\nef");
    assert_eq!(text.line_count(), 2);
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), tick + 1);
    assert_eq!(editor.buffer(buffer).unwrap().changedtick_diag, 1);
    assert_eq!(editor.buffer(buffer).unwrap().changedtick_fold, 1);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_seq(), seq + 1);
    assert_eq!(editor.changelists().len(buffer), changelist + 1);
    assert_eq!(
        editor
            .buffer(buffer)
            .unwrap()
            .extmarks
            .get(ns, id)
            .unwrap()
            .unwrap()
            .position(),
        ExtmarkPosition::new(1, 0)
    );
    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"abcdef"
    );
    assert_eq!(editor.buffer_undo(buffer).unwrap(), None);
    assert_eq!(editor.buffer_redo(buffer).unwrap(), Some(1));
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"a\nef"
    );
}

#[test]
fn normal_replace_typed_cr_autoindents() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"    abcdef").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 4)).unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "autoindent", OptionValue::Boolean(true))
        .unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "3r\r", &mut eval).unwrap();
    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "    \n    def"
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(2, 3));

    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"    abcdef").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 4)).unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "autoindent", OptionValue::Boolean(false))
        .unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "3r\r", &mut eval).unwrap();
    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "    \ndef"
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(2, 0));
}

#[test]
fn visual_blockwise_typed_cr_splits_every_row() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"123456789\n123456789\n123456789").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("block-break")
        .unwrap();
    let mut mark = ExtmarkPlacement::new(ExtmarkPosition::new(1, 6));
    mark.gravity = ExtmarkGravity::Right;
    let id = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(ns, None, mark)
        .unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine
        .feed_keys(&mut editor, "05l\u{16}2jr\r", &mut eval)
        .unwrap();
    let text = editor.buffer(buffer).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"12345\n789\n12345\n789\n12345\n789");
    assert_eq!(text.line_count(), 6);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 5));
    assert_eq!(
        editor
            .buffer(buffer)
            .unwrap()
            .extmarks
            .get(ns, id)
            .unwrap()
            .unwrap()
            .position(),
        ExtmarkPosition::new(3, 0)
    );
    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"123456789\n123456789\n123456789"
    );
    assert_eq!(editor.buffer_redo(buffer).unwrap(), Some(1));
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"12345\n789\n12345\n789\n12345\n789"
    );
}

#[test]
fn visual_blockwise_replace_batch_one_tick() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"12345\n12345\n12345").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("block-tick")
        .unwrap();
    let mut mark = ExtmarkPlacement::new(ExtmarkPosition::new(1, 2));
    mark.gravity = ExtmarkGravity::Right;
    let id = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(ns, None, mark)
        .unwrap();
    let tick = editor.buffer(buffer).unwrap().changedtick();
    let diag = editor.buffer(buffer).unwrap().changedtick_diag;
    let fold = editor.buffer(buffer).unwrap().changedtick_fold;
    let seq = editor.buffer(buffer).unwrap().undo.current_seq();
    let changelist = editor.changelists().len(buffer);
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine
        .feed_keys(&mut editor, "0\u{16}2jrX", &mut eval)
        .unwrap();
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"X2345\nX2345\nX2345"
    );
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), tick + 1);
    assert_eq!(editor.buffer(buffer).unwrap().changedtick_diag, diag + 1);
    assert_eq!(editor.buffer(buffer).unwrap().changedtick_fold, fold + 1);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_seq(), seq + 1);
    assert_eq!(editor.changelists().len(buffer), changelist + 1);
    assert_eq!(
        editor
            .buffer(buffer)
            .unwrap()
            .extmarks
            .get(ns, id)
            .unwrap()
            .unwrap()
            .position(),
        ExtmarkPosition::new(1, 2)
    );
    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"12345\n12345\n12345"
    );
}

#[test]
fn visual_charwise_multirow_replace_one_tick() {
    let (text, _, _, editor, buffer) = run("ab\nxy\npq", position(1, 0), "vjjrZ");
    assert_eq!(text, "ZZ\nZZ\nZq");
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), 1);

    let (text, _, _, editor, buffer) = run("abcd", position(1, 1), "vlrX");
    assert_eq!(text, "aXXd");
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), 1);
}

#[test]
fn visual_replace_empty_selection_zero_tick() {
    let (text, _, _, editor, buffer) = run("\n\n", position(1, 0), "\u{16}2jrX");
    assert_eq!(text, "\n\n");
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), 0);
    assert_eq!(editor.changelists().len(buffer), 0);
}

#[test]
fn blockwise_put_batch_one_tick() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"abc\ndef").unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor
        .tabpage(editor.current_tabpage().unwrap())
        .unwrap()
        .current_window();
    editor.set_window_cursor(window, position(1, 1)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("put-tick")
        .unwrap();
    let id = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(
            ns,
            None,
            ExtmarkPlacement::new(ExtmarkPosition::new(0, 2)),
        )
        .unwrap();
    editor
        .registers_mut()
        .set(
            'a',
            crate::RegisterContent::blockwise(vec![b"Q".to_vec(), b"R".to_vec()], 1).unwrap(),
        )
        .unwrap();
    let tick = editor.buffer(buffer).unwrap().changedtick();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "\"aP", &mut eval).unwrap();
    assert_eq!(
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
        "aQbc\ndRef"
    );
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), tick + 1);
    let mark = editor
        .buffer(buffer)
        .unwrap()
        .extmarks
        .get(ns, id)
        .unwrap()
        .unwrap();
    assert_eq!(mark.position(), ExtmarkPosition::new(0, 3));
}

#[test]
fn normal_join_moves_extmark_with_splice_geometry() {
    for (text, keys, expected, cursor_col, mark_col) in [
        ("12345\n222", "J", "12345 222", 5, 6),
        ("left\n)", "J", "left)", 4, 4),
        ("Done.\nNext", "J", "Done.  Next", 5, 7),
    ] {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true)
            .unwrap();
        editor
            .options_mut()
            .set_global("joinspaces", OptionValue::Boolean(true))
            .unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor
            .tabpage(editor.current_tabpage().unwrap())
            .unwrap()
            .current_window();
        editor.set_window_cursor(window, position(1, 0)).unwrap();
        let ns = editor
            .buffer_mut(buffer)
            .unwrap()
            .extmarks
            .create_namespace("join-geometry")
            .unwrap();
        let id = editor
            .buffer_mut(buffer)
            .unwrap()
            .extmarks
            .set(
                ns,
                None,
                ExtmarkPlacement::new(ExtmarkPosition::new(1, 0)),
            )
            .unwrap();
        let mut machine = ModeMachine::default();
        let mut eval = NullExprEval;
        machine.feed_keys(&mut editor, keys, &mut eval).unwrap();
        assert_eq!(
            String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(),
            expected
        );
        assert_eq!(editor.window(window).unwrap().cursor, position(1, cursor_col));
        let mark = editor
            .buffer(buffer)
            .unwrap()
            .extmarks
            .get(ns, id)
            .unwrap()
            .unwrap();
        assert_eq!(mark.position(), ExtmarkPosition::new(0, mark_col));
        assert!(editor.buffer_undo(buffer).unwrap().is_some());
        let mark = editor
            .buffer(buffer)
            .unwrap()
            .extmarks
            .get(ns, id)
            .unwrap()
            .unwrap();
        assert_eq!(mark.position(), ExtmarkPosition::new(1, 0));
    }
}

behavior!(join_two_lines_inserts_space_and_cursor, "12345\n1", position(1,0), "J", "12345 1", position(1,5), "normal");
behavior!(join_on_last_line_is_noop, "12345", position(1,0), "J", "12345", position(1,0), "normal");
behavior!(join_strips_leading_whitespace, "abc\n  def", position(1,0), "J", "abc def", position(1,3), "normal");
behavior!(visual_multiline_join, "12345\n222\n333\n444", position(2,0), "VGJ", "12345\n222 333 444", position(2,7), "normal");
behavior!(join_preserves_one_trailing_space, "left \n  right", position(1,0), "J", "left right", position(1,5), "normal");
behavior!(join_preserves_two_trailing_spaces, "left  \nright", position(1,0), "J", "left  right", position(1,6), "normal");
behavior!(join_before_closing_paren_inserts_no_space, "left\n  )", position(1,0), "J", "left)", position(1,4), "normal");
behavior!(join_trailing_tab_inserts_no_space, "left\t\nright", position(1,0), "J", "left\tright", position(1,5), "normal");
behavior!(join_empty_right_inserts_no_space, "left\n   ", position(1,0), "J", "left", position(1,4), "normal");
behavior!(visual_join_before_paren_inserts_no_space, "left\n  )\nkeep", position(1,0), "VjJ", "left)\nkeep", position(1,4), "normal");

fn run_join_with_options(
    text: &str,
    keys: &str,
    joinspaces: bool,
    formatoptions: &str,
    comments: &str,
) -> (String, Position) {
    run_join_with_options_and_cpoptions(text, keys, joinspaces, formatoptions, comments, None)
}

fn run_join_with_options_and_cpoptions(
    text: &str,
    keys: &str,
    joinspaces: bool,
    formatoptions: &str,
    comments: &str,
    cpoptions: Option<&str>,
) -> (String, Position) {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true)
        .unwrap();
    editor
        .options_mut()
        .set_global("joinspaces", OptionValue::Boolean(joinspaces))
        .unwrap();
    if let Some(cpoptions) = cpoptions {
        editor
            .options_mut()
            .set_global("cpoptions", OptionValue::String(cpoptions.to_owned()))
            .unwrap();
    }
    editor
        .options_mut()
        .set_buffer(buffer, "formatoptions", OptionValue::String(formatoptions.to_owned()))
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "comments", OptionValue::String(comments.to_owned()))
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, keys, &mut eval).unwrap();
    let output = String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap();
    let cursor = editor.window(window).unwrap().cursor;
    (output, cursor)
}

#[test]
fn joinspaces_punctuation_and_existing_spaces() {
    let cases = [
        (false, "Done.\nNext", "Done. Next"),
        (true, "Done.\nNext", "Done.  Next"),
        (true, "Done?\nNext", "Done?  Next"),
        (true, "Done!\nNext", "Done!  Next"),
        (true, "Done. \nNext", "Done.  Next"),
        (true, "Done.  \nNext", "Done.  Next"),
        (false, "Done. \nNext", "Done. Next"),
        (true, "plain\nNext", "plain Next"),
    ];
    for (joinspaces, text, expected) in cases {
        let (output, cursor) = run_join_with_options(text, "J", joinspaces, "tcq", "://");
        assert_eq!(output, expected, "joinspaces={joinspaces} text={text:?}");
        assert_eq!(cursor.lnum, 1);
        assert_eq!(cursor.col, text.lines().next().unwrap().len());
    }
}

#[test]
fn formatoptions_multibyte_join_spacing() {
    let cases = [
        ("M", "한\nx", "한x"),
        ("M", "x\n한", "x한"),
        ("B", "한\n글", "한글"),
        ("B", "한\nx", "한 x"),
        ("B", "x\n한", "x 한"),
        ("", "한\n글", "한 글"),
    ];
    for (formatoptions, text, expected) in cases {
        let (output, _) = run_join_with_options(text, "J", false, formatoptions, "://");
        assert_eq!(output, expected, "fo={formatoptions:?} text={text:?}");
    }
}

#[test]
fn formatoptions_j_comment_leader_rules() {
    let comments = "s1:/*,mb:*,ex:*/,://";
    let cases = [
        ("tcqj", "// comment1\n// comment2", "// comment1 comment2"),
        ("tcq", "// comment1\n// comment2", "// comment1 // comment2"),
        ("tcqj", "code\n// comment", "code // comment"),
        ("tcqj", "i++; // comment1\n           // comment2", "i++; // comment1 comment2"),
        ("tcqj", "/* start\n */", "/* start */"),
        ("tcqj", "/* keep */\n// next", "/* keep */ // next"),
    ];
    for (formatoptions, text, expected) in cases {
        let (output, _) = run_join_with_options(text, "3J", false, formatoptions, comments);
        assert_eq!(output, expected, "fo={formatoptions:?} text={text:?}");
    }
}

#[test]
fn join_comment_block_close_and_trailing_reopen() {
    let comments = "s1:/*,mb:*,ex:*/,://";
    let (output, _) = run_join_with_options(
        "/* head\n */\n// next();",
        "3J",
        false,
        "tcqj",
        comments,
    );
    assert_eq!(output, "/* head */ // next();");

    let (output, _) = run_join_with_options(
        "/* head\n */ // continuation\n// tail",
        "3J",
        false,
        "tcqj",
        comments,
    );
    assert_eq!(output, "/* head */ // continuation tail");
}

#[test]
fn visual_join_comment_and_joinspaces_geometry() {
    let (output, cursor) = run_join_with_options(
        "// one\n// two\n// three",
        "VGJ",
        false,
        "tcqj",
        "://",
    );
    assert_eq!(output, "// one two three");
    assert_eq!(cursor, position(1, 10));

    let (output, cursor) = run_join_with_options("Done.\nNext\nTail", "VjJ", true, "tcq", "://");
    assert_eq!(output, "Done.  Next\nTail");
    assert_eq!(cursor, position(1, 5));
}

#[test]
fn join_multiline_cursor_follows_final_boundary_unless_cpo_q() {
    let (output, cursor) = run_join_with_options("aa\nbbb\ncccc", "3J", false, "tcq", "://");
    assert_eq!(output, "aa bbb cccc");
    assert_eq!(cursor, position(1, 6));

    let (output, cursor) = run_join_with_options("222\n333\n444", "VGJ", false, "tcq", "://");
    assert_eq!(output, "222 333 444");
    assert_eq!(cursor, position(1, 7));

    let (output, cursor) = run_join_with_options_and_cpoptions(
        "aa\nbbb\ncccc",
        "3J",
        false,
        "tcq",
        "://",
        Some("aABceFs_q"),
    );
    assert_eq!(output, "aa bbb cccc");
    assert_eq!(cursor, position(1, 2));
}

behavior!(open_below, "one", position(1,0), "oX\u{1b}", "one\nX", position(2,0), "normal");
behavior!(open_above, "one", position(1,0), "OX\u{1b}", "X\none", position(1,0), "normal");
behavior!(search_forward, "one two one", position(1,0), "/two\n", "one two one", position(1,4), "normal");
behavior!(search_end_offset, "one two", position(1,0), "/two/e\n", "one two", position(1,6), "normal");
behavior!(search_wrap, "one two one", position(1,9), "/two\n", "one two one", position(1,4), "normal");
behavior!(search_repeat, "one two one two", position(1,0), "/two\nn", "one two one two", position(1,12), "normal");
behavior!(search_opposite, "one two one two", position(1,12), "/one\nN", "one two one two", position(1,8), "normal");
behavior!(find_and_repeat, "a-b-c-d", position(1,0), "f-;", "a-b-c-d", position(1,3), "normal");
behavior!(find_till, "a-b-c", position(1,0), "tc", "a-b-c", position(1,3), "normal");
behavior!(percent_pair, "a(b(c)d)e", position(1,0), "%", "a(b(c)d)e", position(1,7), "normal");
behavior!(paragraph_motion, "one\n\ntwo", position(1,0), "}", "one\n\ntwo", position(2,0), "normal");
behavior!(uppercase_operator, "one two", position(1,0), "gUw", "ONE two", position(1,0), "normal");
behavior!(lowercase_operator, "ONE TWO", position(1,0), "guw", "one TWO", position(1,0), "normal");
behavior!(indent_line, "one", position(1,0), ">>", "        one", position(1,8), "normal");
behavior!(unindent_line, "  one", position(1,0), "<<", "one", position(1,0), "normal");

behavior!(delete_counted_inner_words, "one two three", position(1,0), "d2iw", " three", position(1,0), "normal");
behavior!(delete_inner_brackets, "a [two] b", position(1,4), "di[", "a [] b", position(1,3), "normal");
behavior!(delete_inner_braces, "a {two} b", position(1,4), "di{", "a {} b", position(1,3), "normal");
behavior!(delete_inner_angles, "a <two> b", position(1,4), "di<", "a <> b", position(1,3), "normal");
behavior!(delete_inner_apostrophe, "a 'two' b", position(1,4), "di'", "a '' b", position(1,3), "normal");
behavior!(delete_inner_sentence, "One. Two!", position(1,6), "dis", "One. ", position(1,4), "normal");
behavior!(delete_inner_paragraph, "one\ntwo\n\nthree", position(2,0), "dip", "\nthree", position(1,0), "normal");
behavior!(delete_word_big, "one-two three", position(1,0), "dW", "three", position(1,0), "normal");
behavior!(backward_word, "one two", position(1,5), "b", "one two", position(1,4), "normal");
behavior!(backward_word_end, "one two", position(1,5), "ge", "one two", position(1,2), "normal");
behavior!(last_nonblank, "one   ", position(1,0), "g_", "one   ", position(1,2), "normal");
behavior!(find_backward, "a-b-c", position(1,4), "F-", "a-b-c", position(1,3), "normal");
behavior!(find_reverse_repeat, "a-b-c-d", position(1,6), "F-,", "a-b-c-d", position(1,5), "normal");
behavior!(search_backward, "one two one", position(1,10), "?two\n", "one two one", position(1,4), "normal");
behavior!(search_line_offset, "a\nb\nc", position(1,0), "/b/+1\n", "a\nb\nc", position(3,0), "normal");
behavior!(visual_block_delete, "abcd\nefgh", position(1,0), "\u{16}ljd", "cd\ngh", position(1,0), "normal");
behavior!(visual_block_delete_with_x, "abcd\nefgh", position(1,0), "\u{16}ljx", "cd\ngh", position(1,0), "normal");
behavior!(visual_uppercase, "one", position(1,0), "vllU", "ONE", position(1,0), "normal");
behavior!(visual_reselect, "one", position(1,0), "vldgv", "e", position(1,1), "visual");

behavior!(multiline_delete_promotes_linewise, "one\n\ntwo", position(1,0), "d}", "\ntwo", position(1,0), "normal");
behavior!(vertical_operator_is_linewise, "abc\ndef\nghi", position(1,1), "dj", "ghi", position(1,0), "normal");
behavior!(counted_search, "x a a", position(1,0), "2/a\n", "x a a", position(1,4), "normal");
behavior!(explicit_one_g, "one\ntwo\nthree", position(3,0), "1G", "one\ntwo\nthree", position(1,0), "normal");
behavior!(screen_bottom_uses_window_height, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "L", "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(24,0), "normal");
behavior!(visual_block_horizontal_corner, "abcd\nefgh", position(1,0), "\u{16}ljO", "abcd\nefgh", position(2,0), "visual");
behavior!(visual_block_uppercase, "abcd\nefgh", position(1,0), "\u{16}ljU", "ABcd\nEFgh", position(1,0), "normal");
behavior!(delete_right_is_exclusive, "abc", position(1,0), "dl", "bc", position(1,0), "normal");
behavior!(delete_counted_right_is_exclusive, "abcd", position(1,0), "d2l", "cd", position(1,0), "normal");
behavior!(change_word_uses_end_motion, "one two", position(1,0), "cwX\u{1b}", "X two", position(1,0), "normal");
behavior!(counted_inner_sentences_advance, "One. Two. Three.", position(1,0), "d2is", " Three.", position(1,0), "normal");
behavior!(backward_search_end_offset, "foo xx foo", position(1,9), "?foo?e\n", "foo xx foo", position(1,9), "normal");
behavior!(search_end_character_offset, "fooXX\nnext", position(1,0), "/foo/e+2\n", "fooXX\nnext", position(1,4), "normal");
behavior!(visual_counted_motion, "abcde", position(1,0), "v2ld", "de", position(1,0), "normal");
behavior!(visual_g_operator, "one two", position(1,0), "vegU", "ONE two", position(1,0), "normal");
behavior!(visual_text_object, "one two", position(1,5), "viwd", "one ", position(1,3), "normal");

// `test/old/testdir/test_cindent.vim` Test_cindent_01: sibling statements inside a
// brace share the block indent; `=` must not apply a per-line offset ramp.
#[test]
fn reindent_applies_cindent_not_line_offset_ramp() {
    let text = "{\nif (test)\ncmd1;\ncmd2;\n}";
    let expected = "{\n    if (test)\n        cmd1;\n    cmd2;\n}";
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true)
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "cindent", OptionValue::Boolean(true))
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "expandtab", OptionValue::Boolean(true))
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "shiftwidth", crate::OptionValue::Number(4))
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "tabstop", crate::OptionValue::Number(4))
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(5, 0)).unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "=gg", &mut eval).unwrap();
    let output =
        String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap();
    assert_eq!(output, expected);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 0));
    assert!(matches!(machine.mode(), Mode::Normal(_)));
}



#[test]
fn failed_search_reports_e486() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer_with(Buffer::from_bytes(b"one").unwrap(), true).unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    let error = machine.feed_keys(&mut editor, "/missing\n", &mut eval).unwrap_err();
    assert!(matches!(error, crate::ModeError::Search(crate::SearchError::PatternNotFound(pattern)) if pattern == "missing"));
}

#[test]
fn state_loop_checks_then_executes_typeahead() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer_with(Buffer::from_bytes(b"one").unwrap(), true).unwrap();
    let tab = editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.typeahead_mut().append(&crate::Keys::from("l"), crate::TypeaheadFlags::default());
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    assert!(machine.run_once(&mut editor, &mut eval).unwrap());
    assert_eq!(editor.window(window).unwrap().cursor, position(1,1));
    assert!(!machine.run_once(&mut editor, &mut eval).unwrap());
}

#[test]
fn yank_inner_word_updates_zero_and_unnamed_registers() {
    let (_, _, _, editor, _) = run("one two", position(1,5), "yiw");
    assert_eq!(editor.registers().get('0').unwrap().unwrap().to_bytes(), b"two");
    assert_eq!(editor.registers().get('"').unwrap().unwrap().to_bytes(), b"two");
}

#[test]
fn operator_motion_is_one_undo_entry() {
    let (text, _, _, mut editor, buffer) = run("one two", position(1,0), "dw");
    assert_eq!(text, "two");
    assert!(editor.buffer_undo(buffer).unwrap().is_some());
    assert_eq!(editor.buffer(buffer).unwrap().text().unwrap().to_bytes(), b"one two");
    assert!(editor.buffer_undo(buffer).unwrap().is_none());
}

#[test]
fn jump_motions_record_only_jump_origins() {
    let (_, _, _, editor, _) = run("one\ntwo\nthree", position(2,0), "jgg");
    assert_eq!(editor.jumplist().len(), 1);
    assert_eq!(editor.jumplist().entries()[0].position, position(3,0));
}
// Real behavior matrix (replaces earlier padded A/x/l/dd repetition).  Each
// family cites the upstream function or oldtest that defines it.

// Operator and motion counts multiply (`normal.c:1145-1158`: "If you give a
// count before AND after the operator, they are multiplied").
behavior!(counts_before_and_after_operator_multiply, "one two three four five six seven eight", position(1,0), "2d3w", "seven eight", position(1,0), "normal");

// `cc` clears the line in place and enters insert (`ops.c:888-901`: OP_CHANGE
// deletes the other lines, then truncates the first).
behavior!(change_line_clears_and_enters_insert, "one\ntwo", position(1,0), "cc", "\ntwo", position(1,0), "insert");

// An exclusive charwise motion that ends in column zero of the next line backs
// onto the previous row, so a cross-line `dw` never joins lines
// (`ops.c:3517-3539`).
behavior!(cross_line_dw_never_joins, "alpha gamma\nbeta", position(1,6), "dw", "alpha \nbeta", position(1,5), "normal");
behavior!(cross_line_dw_indent_end_backs_off, "aa bb\ncc", position(1,3), "dw", "aa \ncc", position(1,2), "normal");

// An exclusive charwise motion that ends past column zero of the next line is
// allowed to join lines, so `d)` from the start of a sentence deletes the
// sentence and pulls the next one up (`ops.c:3517-3539`).
behavior!(delete_sentence_cross_line_joins, "one.\n  two.", position(1,0), "d)", "two.", position(1,0), "normal");

// Quote objects select the pair under the cursor, skip escaped quotes, and
// include the quotes themselves when `count >= 2` (`textobject.c:1539-1745`,
// `current_quote`; adjacent pairs never combine).
behavior!(quote_count_two_includes_quotes, "a \"x y\" b \"p q\" c", position(1,3), "d2i\"", "a  b \"p q\" c", position(1,2), "normal");
behavior!(quote_object_targets_current_pair, "hi \"pp\" there \"qq\" now", position(1,15), "di\"", "hi \"pp\" there \"\" now", position(1,15), "normal");
behavior!(quote_object_skips_escaped_quotes, "x \"a \\\"b\\\" c\" y", position(1,3), "ci\"Z\u{1b}", "x \"Z\" y", position(1,3), "normal");

// A `.`/`!`/`?` ends a sentence only after trailing `)]"'` closers give way to
// whitespace; applies to the `)`/`(` motions and the `as`/`is` objects
// (`textobject.c:103-131`, `find_sent`).
behavior!(sentence_motion_skips_trailing_closers, "a.) b.", position(1,0), ")", "a.) b.", position(1,4), "normal");
behavior!(sentence_object_ends_after_closers, "One.) Two.", position(1,7), "dis", "One.) ", position(1,5), "normal");

// Block visual keeps its virtual edge columns across short rows; a row without
// cells at those columns contributes no bytes but keeps the rectangle width
// (`ops.c:2223-2231`, `block_prep` is_short accounting).
behavior!(block_ragged_delete_keeps_short_row, "abcdef\nx\nuvwxyz", position(1,2), "\u{16}lljjd", "abf\nx\nuvz", position(1,2), "normal");
behavior!(block_ragged_uppercase_wide_edges, "abcde\nz\nqrstu", position(1,0), "\u{16}llljjU", "ABCDe\nZ\nQRSTu", position(1,0), "normal");

#[test]
fn nowrap_search_reports_pattern_not_found_at_end() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer_with(Buffer::from_bytes(b"one two").unwrap(), true).unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    editor.options_mut().set_global("wrapscan", crate::OptionValue::Boolean(false)).unwrap();
    let window = editor.tabpage(editor.current_tabpage().unwrap()).unwrap().current_window();
    editor.set_window_cursor(window, position(1,4)).unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    // 'wrapscan' off stops the search at the buffer end instead of wrapping
    // (`search.c:933-944`).
    let error = machine.feed_keys(&mut editor, "/two\n", &mut eval).unwrap_err();
    assert!(matches!(error, crate::ModeError::Search(crate::SearchError::PatternNotFound(pattern)) if pattern == "two"));
}

#[test]
fn block_ragged_yank_keeps_rectangle_width() {
    let (_, _, _, editor, _) = run("abcdef\nx\nuvwxyz", position(1,2), "\u{16}lljjy");
    let unnamed = editor.registers().get('"').unwrap().unwrap();
    assert_eq!(unnamed.kind(), crate::RegisterKind::BlockWise { width: 3 });
    assert_eq!(unnamed.to_bytes(), b"cde\n\nwxy");
}

#[test]
fn ex_cmdline_completes_and_aborts_without_executing_in_the_machine() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;

    machine.feed_keys(&mut editor, ":echo 1+1\r", &mut eval).unwrap();
    assert_eq!(machine.take_ex_command().as_deref(), Some("echo 1+1"));
    assert!(matches!(machine.mode(), Mode::Normal(_)));

    machine.feed_keys(&mut editor, ":quit\u{1b}", &mut eval).unwrap();
    assert_eq!(machine.take_ex_command(), None);
    assert!(matches!(machine.mode(), Mode::Normal(_)));
}

#[test]
fn run_once_expands_mapped_keys_before_mode_execution() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    editor.mappings_mut().map(
        Keys::from("Q"),
        MappingAction::Keys(Keys::from("iX\u{1b}")),
        MappingOptions { modes: MapMode::Normal.into(), ..MappingOptions::default() },
    ).unwrap();
    editor.typeahead_mut().append(&Keys::from("Q"), TypeaheadFlags::default());
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;

    while machine.run_once(&mut editor, &mut eval).unwrap() {}

    assert_eq!(editor.buffer(buffer).unwrap().text().unwrap().to_bytes(), b"X");
    assert!(matches!(machine.mode(), Mode::Normal(_)));
}

#[test]
fn append_runs_after_the_window_buffer_shrank_under_the_cursor() {
    // A window keeps its cursor across a buffer switch, so switching to a
    // shorter buffer leaves `w_cursor.lnum` past the last line. Upstream pulls
    // it back with `check_cursor_lnum` (cursor.c) before the command runs;
    // indexing the line list with the stale value crashed the process on
    // test_visual.vim.
    let mut editor = Editor::new();
    let long = editor.create_buffer_with(Buffer::from_bytes(b"one\ntwo\nthree").unwrap(), true).unwrap();
    let tab = editor.create_tabpage(long, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(3, 0)).unwrap();

    let short = editor.create_buffer_with(Buffer::from_bytes(b"x").unwrap(), true).unwrap();
    editor.set_window_buffer(window, short, BufferRelease::KeepLoaded).unwrap();

    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    machine.feed_keys(&mut editor, "aY\u{1b}", &mut eval).unwrap();

    assert_eq!(editor.buffer(short).unwrap().text().unwrap().to_bytes(), b"xY");
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 1));
}


struct FailingIndentEval;
impl ExprEval for FailingIndentEval {
    fn eval_indentexpr(
        &mut self,
        _context: &IndentEvalContext<'_>,
        _lnum: usize,
        _expression: &str,
    ) -> Result<i64, IndentExprError> {
        Err(IndentExprError::Failed("fail".into()))
    }
}

struct FixedIndentEval(i64);
impl ExprEval for FixedIndentEval {
    fn eval_indentexpr(
        &mut self,
        _context: &IndentEvalContext<'_>,
        _lnum: usize,
        _expression: &str,
    ) -> Result<i64, IndentExprError> {
        Ok(self.0)
    }
}

struct ScriptIndentEval {
    results: Vec<Result<i64, IndentExprError>>,
    calls: Vec<usize>,
}
impl ExprEval for ScriptIndentEval {
    fn eval_indentexpr(
        &mut self,
        _context: &IndentEvalContext<'_>,
        lnum: usize,
        _expression: &str,
    ) -> Result<i64, IndentExprError> {
        self.calls.push(lnum);
        let idx = self.calls.len() - 1;
        self.results.get(idx).cloned().unwrap_or(Ok(0))
    }
}

struct StagedObservingEval {
    staged_leads: Vec<usize>,
    live_leads: Vec<usize>,
}
impl ExprEval for StagedObservingEval {
    fn eval_indentexpr(
        &mut self,
        context: &IndentEvalContext<'_>,
        lnum: usize,
        _expression: &str,
    ) -> Result<i64, IndentExprError> {
        if lnum >= 2 {
            let staged = context.lines()[lnum - 2]
                .iter()
                .take_while(|b| b.is_ascii_whitespace())
                .count();
            let live = context
                .editor()
                .buffer(context.buffer())
                .map_err(|err| IndentExprError::Failed(err.to_string()))?
                .text()
                .map_err(|err| IndentExprError::Failed(err.to_string()))?
                .line(lnum - 1)
                .map_err(|err| IndentExprError::Failed(err.to_string()))?;
            let live_lead = live.iter().take_while(|b| b.is_ascii_whitespace()).count();
            self.staged_leads.push(staged);
            self.live_leads.push(live_lead);
        }
        Ok(4)
    }
}

fn set_indentexpr(editor: &mut Editor, buffer: ox_types::BufHandle) {
    editor
        .options_mut()
        .set_buffer(buffer, "indentexpr", OptionValue::String("Fail()".into()))
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "expandtab", OptionValue::Boolean(true))
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "shiftwidth", OptionValue::Number(4))
        .unwrap();
    editor
        .options_mut()
        .set_buffer(buffer, "tabstop", OptionValue::Number(4))
        .unwrap();
}

#[test]
fn insert_newline_eval_failure_keeps_insert_mode_and_text() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"if (x) {").unwrap(), true)
        .unwrap();
    set_indentexpr(&mut editor, buffer);
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(1, 8)).unwrap();
    let mut machine = ModeMachine::default();
    let mut ok_eval = NullExprEval;
    machine.feed_keys(&mut editor, "i", &mut ok_eval).unwrap();
    let before = editor.buffer(buffer).unwrap().text().unwrap().to_bytes();
    let cursor_before = editor.window(window).unwrap().cursor;
    let mut fail = FailingIndentEval;
    let err = machine.feed_keys(&mut editor, "\r", &mut fail).unwrap_err();
    assert!(matches!(
        err,
        ModeError::Insert(InsertError::Indent(_))
    ));
    assert_eq!(editor.buffer(buffer).unwrap().text().unwrap().to_bytes(), before);
    assert_eq!(editor.window(window).unwrap().cursor, cursor_before);
    assert_eq!(machine.mode(), &Mode::Insert(InsertState));
    let mut ok_eval = NullExprEval;
    machine.feed_keys(&mut editor, "x", &mut ok_eval).unwrap();
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"if (x) {x"
    );
}

#[test]
fn operator_eval_failure_restores_pending_state_and_text() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"aaaa\nbbbb\ncccc").unwrap(), true)
        .unwrap();
    set_indentexpr(&mut editor, buffer);
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let mut machine = ModeMachine::default();
    let mut ok_eval = NullExprEval;
    machine.feed_keys(&mut editor, "=", &mut ok_eval).unwrap();
    let pending = machine.mode().clone();
    assert!(matches!(pending, Mode::OperatorPending(_)));
    let bytes_before = editor.buffer(buffer).unwrap().text().unwrap().to_bytes();
    let cursor_before = editor.window(window).unwrap().cursor;
    let tick_before = editor.buffer(buffer).unwrap().changedtick();
    let seq_before = editor.buffer(buffer).unwrap().undo.current_seq();
    let modified_before = editor.buffer(buffer).unwrap().modified;
    let mut fail = FailingIndentEval;
    let err = machine.feed_keys(&mut editor, "G", &mut fail).unwrap_err();
    assert!(matches!(
        err,
        ModeError::Operator(OperatorError::Indent(_))
    ));
    assert_eq!(machine.mode(), &pending);
    assert_eq!(editor.buffer(buffer).unwrap().text().unwrap().to_bytes(), bytes_before);
    assert_eq!(editor.window(window).unwrap().cursor, cursor_before);
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), tick_before);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_seq(), seq_before);
    assert_eq!(editor.buffer(buffer).unwrap().modified, modified_before);
    let mut fixed = FixedIndentEval(4);
    machine.feed_keys(&mut editor, "G", &mut fixed).unwrap();
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"    aaaa\n    bbbb\n    cccc"
    );
    assert!(matches!(machine.mode(), Mode::Normal(_)));
}

#[test]
fn reindent_failure_leaves_no_partial_edits() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"aaaa\nbbbb\ncccc").unwrap(), true)
        .unwrap();
    set_indentexpr(&mut editor, buffer);
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let ns = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("reindent-fail")
        .unwrap();
    editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(
            ns,
            None,
            ExtmarkPlacement::new(ExtmarkPosition::new(1, 0)),
        )
        .unwrap();
    let bytes_before = editor.buffer(buffer).unwrap().text().unwrap().to_bytes();
    let cursor_before = editor.window(window).unwrap().cursor;
    let tick_before = editor.buffer(buffer).unwrap().changedtick();
    let seq_before = editor.buffer(buffer).unwrap().undo.current_seq();
    let modified_before = editor.buffer(buffer).unwrap().modified;
    let changelist_before = editor.changelists().len(buffer);
    let extmarks_before = editor.buffer(buffer).unwrap().extmarks.clone();
    let mut eval = ScriptIndentEval {
        results: vec![Ok(4), Err(IndentExprError::Failed("line2".into())), Ok(4)],
        calls: Vec::new(),
    };
    let mut machine = ModeMachine::default();
    let err = machine.feed_keys(&mut editor, "=G", &mut eval).unwrap_err();
    assert!(matches!(
        err,
        ModeError::Operator(OperatorError::Indent(_))
    ));
    assert_eq!(eval.calls, vec![1, 2]);
    assert_eq!(editor.buffer(buffer).unwrap().text().unwrap().to_bytes(), bytes_before);
    assert_eq!(editor.window(window).unwrap().cursor, cursor_before);
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), tick_before);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_seq(), seq_before);
    assert_eq!(editor.buffer(buffer).unwrap().modified, modified_before);
    assert_eq!(editor.changelists().len(buffer), changelist_before);
    assert_eq!(editor.buffer(buffer).unwrap().extmarks, extmarks_before);
}

#[test]
fn reindent_success_is_one_undo_block() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"aaaa\nbbbb\ncccc").unwrap(), true)
        .unwrap();
    set_indentexpr(&mut editor, buffer);
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = FixedIndentEval(4);
    machine.feed_keys(&mut editor, "=G", &mut eval).unwrap();
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"    aaaa\n    bbbb\n    cccc"
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 4));
    assert!(matches!(machine.mode(), Mode::Normal(_)));
    assert!(editor.buffer_undo(buffer).unwrap().is_some());
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"aaaa\nbbbb\ncccc"
    );
    assert!(editor.buffer_undo(buffer).unwrap().is_none());
}

#[test]
fn reindent_evaluator_observes_staged_prior_lines() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(b"aaaa\nbbbb\ncccc").unwrap(), true)
        .unwrap();
    set_indentexpr(&mut editor, buffer);
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = StagedObservingEval {
        staged_leads: Vec::new(),
        live_leads: Vec::new(),
    };
    machine.feed_keys(&mut editor, "=G", &mut eval).unwrap();
    assert_eq!(eval.staged_leads, vec![4, 4]);
    assert_eq!(eval.live_leads, vec![0, 0]);
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        b"    aaaa\n    bbbb\n    cccc"
    );
}

// Operator-pending `v`/`V` forcing and `/`/`?` search motions.
behavior!(operator_charwise_force_backward_search, "\n12345\ntest-me", position(1,0), "dv?-m?\n", "me", position(1,0), "normal");
behavior!(operator_forward_search_delete, "one two three", position(1,0), "d/two\n", "two three", position(1,0), "normal");
behavior!(operator_charwise_force_vertical_is_not_linewise, "abc\ndef\nghi", position(1,1), "dvj", "aef\nghi", position(1,1), "normal");
behavior!(operator_linewise_force_search, "one two\nthree", position(1,0), "dV/two\n", "three", position(1,0), "normal");
behavior!(operator_pending_escape_aborts, "one two", position(1,0), "d\u{1b}l", "one two", position(1,1), "normal");
behavior!(operator_search_escape_aborts, "one two", position(1,0), "d/two\u{1b}l", "one two", position(1,1), "normal");
behavior!(operator_search_end_offset_is_inclusive, "one two three", position(1,0), "d/two/e\n", " three", position(1,0), "normal");
behavior!(operator_search_end_character_offset_is_inclusive, "one two three", position(1,0), "d/two/e+1\n", "three", position(1,0), "normal");
behavior!(operator_charwise_force_toggles_search_end_inclusive, "one two three", position(1,0), "dv/two/e\n", "o three", position(1,0), "normal");
behavior!(operator_search_line_offset_is_linewise, "one two\nthree four\nfive", position(1,0), "d/two/+1\n", "five", position(1,0), "normal");
behavior!(operator_search_zero_line_offset_is_linewise, "one two\nthree four\nfive", position(1,0), "d/two/+0\n", "three four\nfive", position(1,0), "normal");
behavior!(operator_search_invalid_regex_returns_to_normal, "one two", position(1,0), "d/[\nx", "ne two", position(1,0), "normal");

#[test]
fn operator_search_missing_match_does_not_mutate() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer_with(Buffer::from_bytes(b"one two").unwrap(), true).unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    editor.options_mut().set_global("wrapscan", crate::OptionValue::Boolean(false)).unwrap();
    let window = editor.tabpage(editor.current_tabpage().unwrap()).unwrap().current_window();
    editor.set_window_cursor(window, position(1, 4)).unwrap();
    let mut machine = ModeMachine::default();
    let mut eval = NullExprEval;
    let tick = editor.buffer(buffer).unwrap().changedtick();
    machine.feed_keys(&mut editor, "d/missing\n", &mut eval).unwrap();
    assert_eq!(String::from_utf8(editor.buffer(buffer).unwrap().text().unwrap().to_bytes()).unwrap(), "one two");
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 4));
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), tick);
    assert!(matches!(machine.mode(), Mode::Normal(_)));
}
