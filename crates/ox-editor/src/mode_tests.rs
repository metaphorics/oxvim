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

behavior!(multiline_delete_promotes_linewise, "one\n\ntwo", position(1,0), "d}", "\ntwo", position(1,0), "normal");
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
    // 'wrapscan' off stops the search at the buffer end instead of wrapping
    // (`search.c:933-944`).
    let error = machine.feed_keys(&mut editor, "/two\n").unwrap_err();
    assert!(matches!(error, crate::ModeError::Search(crate::SearchError::PatternNotFound(pattern)) if pattern == "two"));
}

#[test]
fn block_ragged_yank_keeps_rectangle_width() {
    let (_, _, _, editor, _) = run("abcdef\nx\nuvwxyz", position(1,2), "\u{16}lljjy");
    let unnamed = editor.registers().get('"').unwrap().unwrap();
    assert_eq!(unnamed.kind(), crate::RegisterKind::BlockWise { width: 3 });
    assert_eq!(unnamed.to_bytes(), b"cde\n\nwxy");
}
