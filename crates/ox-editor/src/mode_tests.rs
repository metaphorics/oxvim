//! Keystroke-sequence behavior oracle.
//!
//! Families derive from `test/old/testdir/test_normal.vim`, `test_visual.vim`,
//! `test_textobjects.vim`, and `test_search.vim`.

use ox_text::{Buffer, Position};

use crate::{Editor, Geometry, Mode, ModeMachine};

fn position(lnum: usize, col: usize) -> Position { Position { lnum, col } }

fn run(text: &str, cursor: Position, keys: &str) -> (String, Position, &'static str, Editor, ox_types::BufHandle) {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true).unwrap();
    let tab = editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, cursor).unwrap();
    let mut machine = ModeMachine::default();
    machine.feed_keys(&mut editor, keys).unwrap();
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
behavior!(visual_line_delete, "one\ntwo\nthree", position(2,0), "Vd", "one\nthree", position(2,0), "normal");
behavior!(visual_swap_anchor, "one", position(1,0), "vlo", "one", position(1,0), "visual");
behavior!(insert_plain, "one", position(1,0), "iX\u{1b}", "Xone", position(1,0), "normal");
behavior!(append_plain, "one", position(1,0), "aX\u{1b}", "oXne", position(1,1), "normal");
behavior!(append_line, "one", position(1,0), "AX\u{1b}", "oneX", position(1,3), "normal");
behavior!(insert_newline, "one", position(1,1), "i\nt\u{1b}", "o\ntne", position(2,0), "normal");
behavior!(insert_backspace, "one", position(1,1), "i\u{8}\u{1b}", "ne", position(1,0), "normal");
behavior!(insert_backspace_join, "one\ntwo", position(2,0), "i\u{8}\u{1b}", "onetwo", position(1,2), "normal");
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
behavior!(visual_uppercase, "one", position(1,0), "vllU", "ONE", position(1,0), "normal");
behavior!(visual_reselect, "one", position(1,0), "vldgv", "e", position(1,1), "visual");

behavior!(multiline_delete_promotes_linewise, "one\n\ntwo", position(1,0), "d}", "two", position(1,0), "normal");
behavior!(vertical_operator_is_linewise, "abc\ndef\nghi", position(1,1), "dj", "ghi", position(1,0), "normal");
behavior!(counted_search, "x a a", position(1,0), "2/a\n", "x a a", position(1,4), "normal");
behavior!(explicit_one_G, "one\ntwo\nthree", position(3,0), "1G", "one\ntwo\nthree", position(1,0), "normal");
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

#[test]
fn unavailable_reindent_is_typed() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer_with(Buffer::from_bytes(b"one").unwrap(), true).unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let mut machine = ModeMachine::default();
    let error = machine.feed_keys(&mut editor, "==").unwrap_err();
    assert!(matches!(error, crate::ModeError::Operator(crate::OperatorError::NotImplemented(_))));
}

#[test]
fn failed_search_reports_e486() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer_with(Buffer::from_bytes(b"one").unwrap(), true).unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap()).unwrap();
    let mut machine = ModeMachine::default();
    let error = machine.feed_keys(&mut editor, "/missing\n").unwrap_err();
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
    assert!(machine.run_once(&mut editor).unwrap());
    assert_eq!(editor.window(window).unwrap().cursor, position(1,1));
    assert!(!machine.run_once(&mut editor).unwrap());
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
behavior!(generated_append_01, "x", position(1,0), "Ax\u{1b}", "xx", position(1,1), "normal");
behavior!(generated_append_02, "x", position(1,0), "Axx\u{1b}", "xxx", position(1,2), "normal");
behavior!(generated_append_03, "x", position(1,0), "Axxx\u{1b}", "xxxx", position(1,3), "normal");
behavior!(generated_append_04, "x", position(1,0), "Axxxx\u{1b}", "xxxxx", position(1,4), "normal");
behavior!(generated_append_05, "x", position(1,0), "Axxxxx\u{1b}", "xxxxxx", position(1,5), "normal");
behavior!(generated_append_06, "x", position(1,0), "Axxxxxx\u{1b}", "xxxxxxx", position(1,6), "normal");
behavior!(generated_append_07, "x", position(1,0), "Axxxxxxx\u{1b}", "xxxxxxxx", position(1,7), "normal");
behavior!(generated_append_08, "x", position(1,0), "Axxxxxxxx\u{1b}", "xxxxxxxxx", position(1,8), "normal");
behavior!(generated_append_09, "x", position(1,0), "Axxxxxxxxx\u{1b}", "xxxxxxxxxx", position(1,9), "normal");
behavior!(generated_append_10, "x", position(1,0), "Axxxxxxxxxx\u{1b}", "xxxxxxxxxxx", position(1,10), "normal");
behavior!(generated_append_11, "x", position(1,0), "Axxxxxxxxxxx\u{1b}", "xxxxxxxxxxxx", position(1,11), "normal");
behavior!(generated_append_12, "x", position(1,0), "Axxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxx", position(1,12), "normal");
behavior!(generated_append_13, "x", position(1,0), "Axxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxx", position(1,13), "normal");
behavior!(generated_append_14, "x", position(1,0), "Axxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxx", position(1,14), "normal");
behavior!(generated_append_15, "x", position(1,0), "Axxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxx", position(1,15), "normal");
behavior!(generated_append_16, "x", position(1,0), "Axxxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxxx", position(1,16), "normal");
behavior!(generated_append_17, "x", position(1,0), "Axxxxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxxxx", position(1,17), "normal");
behavior!(generated_append_18, "x", position(1,0), "Axxxxxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxxxxx", position(1,18), "normal");
behavior!(generated_append_19, "x", position(1,0), "Axxxxxxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxxxxxx", position(1,19), "normal");
behavior!(generated_append_20, "x", position(1,0), "Axxxxxxxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxxxxxxx", position(1,20), "normal");
behavior!(generated_append_21, "x", position(1,0), "Axxxxxxxxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxxxxxxxx", position(1,21), "normal");
behavior!(generated_append_22, "x", position(1,0), "Axxxxxxxxxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxxxxxxxxx", position(1,22), "normal");
behavior!(generated_append_23, "x", position(1,0), "Axxxxxxxxxxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxxxxxxxxxx", position(1,23), "normal");
behavior!(generated_append_24, "x", position(1,0), "Axxxxxxxxxxxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxxxxxxxxxxx", position(1,24), "normal");
behavior!(generated_append_25, "x", position(1,0), "Axxxxxxxxxxxxxxxxxxxxxxxxx\u{1b}", "xxxxxxxxxxxxxxxxxxxxxxxxxx", position(1,25), "normal");
behavior!(generated_delete_01, "abcdefghijklmnopqrstuvwxyz", position(1,0), "1x", "bcdefghijklmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_02, "abcdefghijklmnopqrstuvwxyz", position(1,0), "2x", "cdefghijklmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_03, "abcdefghijklmnopqrstuvwxyz", position(1,0), "3x", "defghijklmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_04, "abcdefghijklmnopqrstuvwxyz", position(1,0), "4x", "efghijklmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_05, "abcdefghijklmnopqrstuvwxyz", position(1,0), "5x", "fghijklmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_06, "abcdefghijklmnopqrstuvwxyz", position(1,0), "6x", "ghijklmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_07, "abcdefghijklmnopqrstuvwxyz", position(1,0), "7x", "hijklmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_08, "abcdefghijklmnopqrstuvwxyz", position(1,0), "8x", "ijklmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_09, "abcdefghijklmnopqrstuvwxyz", position(1,0), "9x", "jklmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_10, "abcdefghijklmnopqrstuvwxyz", position(1,0), "10x", "klmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_11, "abcdefghijklmnopqrstuvwxyz", position(1,0), "11x", "lmnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_12, "abcdefghijklmnopqrstuvwxyz", position(1,0), "12x", "mnopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_13, "abcdefghijklmnopqrstuvwxyz", position(1,0), "13x", "nopqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_14, "abcdefghijklmnopqrstuvwxyz", position(1,0), "14x", "opqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_15, "abcdefghijklmnopqrstuvwxyz", position(1,0), "15x", "pqrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_16, "abcdefghijklmnopqrstuvwxyz", position(1,0), "16x", "qrstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_17, "abcdefghijklmnopqrstuvwxyz", position(1,0), "17x", "rstuvwxyz", position(1,0), "normal");
behavior!(generated_delete_18, "abcdefghijklmnopqrstuvwxyz", position(1,0), "18x", "stuvwxyz", position(1,0), "normal");
behavior!(generated_delete_19, "abcdefghijklmnopqrstuvwxyz", position(1,0), "19x", "tuvwxyz", position(1,0), "normal");
behavior!(generated_delete_20, "abcdefghijklmnopqrstuvwxyz", position(1,0), "20x", "uvwxyz", position(1,0), "normal");
behavior!(generated_delete_21, "abcdefghijklmnopqrstuvwxyz", position(1,0), "21x", "vwxyz", position(1,0), "normal");
behavior!(generated_delete_22, "abcdefghijklmnopqrstuvwxyz", position(1,0), "22x", "wxyz", position(1,0), "normal");
behavior!(generated_delete_23, "abcdefghijklmnopqrstuvwxyz", position(1,0), "23x", "xyz", position(1,0), "normal");
behavior!(generated_delete_24, "abcdefghijklmnopqrstuvwxyz", position(1,0), "24x", "yz", position(1,0), "normal");
behavior!(generated_delete_25, "abcdefghijklmnopqrstuvwxyz", position(1,0), "25x", "z", position(1,0), "normal");
behavior!(generated_right_01, "abcdefghijklmnopqrstuvwxyz", position(1,0), "1l", "abcdefghijklmnopqrstuvwxyz", position(1,1), "normal");
behavior!(generated_right_02, "abcdefghijklmnopqrstuvwxyz", position(1,0), "2l", "abcdefghijklmnopqrstuvwxyz", position(1,2), "normal");
behavior!(generated_right_03, "abcdefghijklmnopqrstuvwxyz", position(1,0), "3l", "abcdefghijklmnopqrstuvwxyz", position(1,3), "normal");
behavior!(generated_right_04, "abcdefghijklmnopqrstuvwxyz", position(1,0), "4l", "abcdefghijklmnopqrstuvwxyz", position(1,4), "normal");
behavior!(generated_right_05, "abcdefghijklmnopqrstuvwxyz", position(1,0), "5l", "abcdefghijklmnopqrstuvwxyz", position(1,5), "normal");
behavior!(generated_right_06, "abcdefghijklmnopqrstuvwxyz", position(1,0), "6l", "abcdefghijklmnopqrstuvwxyz", position(1,6), "normal");
behavior!(generated_right_07, "abcdefghijklmnopqrstuvwxyz", position(1,0), "7l", "abcdefghijklmnopqrstuvwxyz", position(1,7), "normal");
behavior!(generated_right_08, "abcdefghijklmnopqrstuvwxyz", position(1,0), "8l", "abcdefghijklmnopqrstuvwxyz", position(1,8), "normal");
behavior!(generated_right_09, "abcdefghijklmnopqrstuvwxyz", position(1,0), "9l", "abcdefghijklmnopqrstuvwxyz", position(1,9), "normal");
behavior!(generated_right_10, "abcdefghijklmnopqrstuvwxyz", position(1,0), "10l", "abcdefghijklmnopqrstuvwxyz", position(1,10), "normal");
behavior!(generated_right_11, "abcdefghijklmnopqrstuvwxyz", position(1,0), "11l", "abcdefghijklmnopqrstuvwxyz", position(1,11), "normal");
behavior!(generated_right_12, "abcdefghijklmnopqrstuvwxyz", position(1,0), "12l", "abcdefghijklmnopqrstuvwxyz", position(1,12), "normal");
behavior!(generated_right_13, "abcdefghijklmnopqrstuvwxyz", position(1,0), "13l", "abcdefghijklmnopqrstuvwxyz", position(1,13), "normal");
behavior!(generated_right_14, "abcdefghijklmnopqrstuvwxyz", position(1,0), "14l", "abcdefghijklmnopqrstuvwxyz", position(1,14), "normal");
behavior!(generated_right_15, "abcdefghijklmnopqrstuvwxyz", position(1,0), "15l", "abcdefghijklmnopqrstuvwxyz", position(1,15), "normal");
behavior!(generated_right_16, "abcdefghijklmnopqrstuvwxyz", position(1,0), "16l", "abcdefghijklmnopqrstuvwxyz", position(1,16), "normal");
behavior!(generated_right_17, "abcdefghijklmnopqrstuvwxyz", position(1,0), "17l", "abcdefghijklmnopqrstuvwxyz", position(1,17), "normal");
behavior!(generated_right_18, "abcdefghijklmnopqrstuvwxyz", position(1,0), "18l", "abcdefghijklmnopqrstuvwxyz", position(1,18), "normal");
behavior!(generated_right_19, "abcdefghijklmnopqrstuvwxyz", position(1,0), "19l", "abcdefghijklmnopqrstuvwxyz", position(1,19), "normal");
behavior!(generated_right_20, "abcdefghijklmnopqrstuvwxyz", position(1,0), "20l", "abcdefghijklmnopqrstuvwxyz", position(1,20), "normal");
behavior!(generated_right_21, "abcdefghijklmnopqrstuvwxyz", position(1,0), "21l", "abcdefghijklmnopqrstuvwxyz", position(1,21), "normal");
behavior!(generated_right_22, "abcdefghijklmnopqrstuvwxyz", position(1,0), "22l", "abcdefghijklmnopqrstuvwxyz", position(1,22), "normal");
behavior!(generated_right_23, "abcdefghijklmnopqrstuvwxyz", position(1,0), "23l", "abcdefghijklmnopqrstuvwxyz", position(1,23), "normal");
behavior!(generated_right_24, "abcdefghijklmnopqrstuvwxyz", position(1,0), "24l", "abcdefghijklmnopqrstuvwxyz", position(1,24), "normal");
behavior!(generated_right_25, "abcdefghijklmnopqrstuvwxyz", position(1,0), "25l", "abcdefghijklmnopqrstuvwxyz", position(1,25), "normal");
behavior!(generated_line_delete_01, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "1dd", "l02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_02, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "2dd", "l03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_03, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "3dd", "l04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_04, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "4dd", "l05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_05, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "5dd", "l06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_06, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "6dd", "l07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_07, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "7dd", "l08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_08, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "8dd", "l09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_09, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "9dd", "l10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_10, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "10dd", "l11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_11, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "11dd", "l12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_12, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "12dd", "l13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_13, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "13dd", "l14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_14, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "14dd", "l15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_15, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "15dd", "l16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_16, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "16dd", "l17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_17, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "17dd", "l18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_18, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "18dd", "l19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_19, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "19dd", "l20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_20, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "20dd", "l21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_21, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "21dd", "l22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_22, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "22dd", "l23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_23, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "23dd", "l24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_24, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "24dd", "l25\nl26\nl27\nl28\nl29\nl30", position(1,0), "normal");
behavior!(generated_line_delete_25, "l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30", position(1,0), "25dd", "l26\nl27\nl28\nl29\nl30", position(1,0), "normal");
