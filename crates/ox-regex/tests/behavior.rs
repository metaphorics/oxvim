//! Behavioral parity cases cited to Neovim's pattern documentation and regex oldtests.

use ox_regex::{compile, exec, exec_at, try_exec, Engine, ExecError, Magic, Position, Text};

fn matched(pattern: &str, magic: Magic, text: &Text) -> Option<String> {
    let prog = compile(pattern, magic).unwrap_or_else(|error| panic!("compile {pattern:?}: {error}"));
    let found = exec(&prog, text)?;
    Some(text.as_str()[found.start.byte..found.end.byte].to_owned())
}

macro_rules! case {
    ($name:ident, $citation:literal, $pattern:literal, $magic:expr, $text:literal, $expected:expr) => {
        #[test]
        fn $name() {
            let _spec = $citation;
            assert_eq!(matched($pattern, $magic, &Text::from($text)), $expected.map(str::to_owned));
        }
    };
}

// pattern.txt:331-380 (/pattern, /branch, /concat, /piece, /atom).
case!(literal_search, "pattern.txt:331-380", "cat", Magic::Magic, "a cat", Some("cat"));
case!(first_branch_wins, "pattern.txt:331-380", "foo\\|foobar", Magic::Magic, "foobar", Some("foo"));
case!(second_branch_matches, "pattern.txt:331-380", "cat\\|dog", Magic::Magic, "a dog", Some("dog"));
case!(concat_matches, "pattern.txt:331-380", "f[0-9]b", Magic::Magic, "f7b", Some("f7b"));
case!(conjunction_requires_both, "pattern.txt:341-351", "foo...\\&foobar", Magic::Magic, "foobar", Some("foobar"));
case!(empty_pattern_matches, "regexp.c:vim_regexec empty-match behavior", "", Magic::Magic, "abc", Some(""));

// pattern.txt:405-447 (/magic).
case!(magic_dot, "pattern.txt:405-447", ".", Magic::Magic, "x", Some("x"));
case!(magic_escaped_dot, "pattern.txt:405-447", "\\.", Magic::Magic, "a.b", Some("."));
case!(nomagic_dot_literal, "pattern.txt:405-447", ".", Magic::NoMagic, "a.b", Some("."));
case!(nomagic_escaped_dot, "pattern.txt:405-447", "\\.", Magic::NoMagic, "x", Some("x"));
case!(very_magic_group, "pattern.txt:424-443", "\\v(a|b)+", Magic::Magic, "abba", Some("abba"));
case!(very_nomagic_dot_literal, "pattern.txt:424-443", "\\V.", Magic::Magic, "a.b", Some("."));
case!(very_nomagic_escaped_dot, "pattern.txt:424-443", "\\V\\.", Magic::Magic, "z", Some("z"));
case!(magic_switch_mid_pattern, "pattern.txt:420-447", "a\\V.b", Magic::Magic, "a.b", Some("a.b"));
case!(nomagic_switch_then_star, "pattern.txt:420-447", "\\Ma\\*", Magic::Magic, "aaa", Some("aaa"));
case!(very_magic_literal_escape, "pattern.txt:424-443", "\\vfoo\\.bar", Magic::Magic, "foo.bar", Some("foo.bar"));
case!(very_magic_lookahead, "pattern.txt:424-443,676-690", "\\v(foo)@=foo", Magic::Magic, "foo", Some("foo"));
case!(very_magic_noncapturing, "pattern.txt:424-443,371-380", "\\v%(foo|bar)", Magic::Magic, "bar", Some("bar"));
case!(very_magic_optional_sequence, "pattern.txt:424-443,1229-1248", "\\vr%[ead]", Magic::Magic, "read", Some("read"));
case!(very_magic_position_assertion, "pattern.txt:424-443,955-980", "\\v%2cb", Magic::Magic, "ab", Some("b"));
case!(magic_switch_changes_group_close, "pattern.txt:420-443", "\\v(foo\\M\\)", Magic::Magic, "foo", Some("foo"));

// pattern.txt:453-480, 599-671 (/multi, /star, /\+, /\{).
case!(star_zero, "pattern.txt:453-473", "ab*", Magic::Magic, "a", Some("a"));
case!(star_many, "pattern.txt:453-473", "ab*", Magic::Magic, "abbb", Some("abbb"));
case!(plus_many, "pattern.txt:453-473", "ab\\+", Magic::Magic, "abbb", Some("abbb"));
case!(plus_requires_one, "pattern.txt:453-473", "ab\\+", Magic::Magic, "a", None);
case!(equals_zero, "pattern.txt:453-473", "ab\\=c", Magic::Magic, "ac", Some("ac"));
case!(question_one, "pattern.txt:453-473", "ab\\?c", Magic::Magic, "abc", Some("abc"));
case!(exact_count, "pattern.txt:463-473", "a\\{3}", Magic::Magic, "aaaa", Some("aaa"));
case!(bounded_greedy, "pattern.txt:463-473", "a\\{2,3}", Magic::Magic, "aaaa", Some("aaa"));
case!(open_upper_bound, "pattern.txt:463-473", "a\\{2,}", Magic::Magic, "aaaa", Some("aaaa"));
case!(omitted_lower_bound, "pattern.txt:463-473", "a\\{,2}b", Magic::Magic, "aab", Some("aab"));
case!(nongreedy_star, "pattern.txt:469-473", "a.\\{-}b", Magic::Magic, "axxbxxb", Some("axxb"));
case!(nongreedy_range, "pattern.txt:469-473", "a\\{-2,4}a", Magic::Magic, "aaaaa", Some("aaa"));

// pattern.txt:676-792 (/\@=, /\@!, /\@<=, /\@<!, /\@>).
case!(positive_lookahead, "pattern.txt:676-690", "foo\\(bar\\)\\@=", Magic::Magic, "foobar", Some("foo"));
case!(positive_lookahead_fails, "pattern.txt:676-690", "foo\\(bar\\)\\@=", Magic::Magic, "foobaz", None);
case!(negative_lookahead, "pattern.txt:692-720", "foo\\(bar\\)\\@!", Magic::Magic, "foobaz", Some("foo"));
case!(negative_lookahead_fails, "pattern.txt:692-720", "foo\\(bar\\)\\@!", Magic::Magic, "foobar", None);
case!(positive_lookbehind, "pattern.txt:722-758", "\\(foo\\)\\@<=bar", Magic::Magic, "foobar", Some("bar"));
case!(negative_lookbehind, "pattern.txt:764-781", "\\(foo\\)\\@<!bar", Magic::Magic, "xxbar", Some("bar"));
case!(negative_lookbehind_fails, "pattern.txt:764-781", "\\(foo\\)\\@<!bar", Magic::Magic, "foobar", None);
case!(lookbehind_stops_after_previous_line, "pattern.txt:734-738", "\\(foo\\nbar\\n\\)\\@<=baz", Magic::Magic, "foo\nbar\nbaz", None);
case!(limited_lookbehind, "pattern.txt:752-758", "<\\@1<=span", Magic::Magic, "<span", Some("span"));
case!(limited_lookbehind_cross_line_long_prefix, "pattern.txt:752-758", "\\(\\_.\\{6}\\)\\@5<=span", Magic::Magic, "abcd\nwxyzspan", Some("span"));
case!(atomic_group_prevents_backtrack, "pattern.txt:783-792", "\\(a*\\)\\@>a", Magic::Magic, "aaa", None);

// pattern.txt:803-874 (/^, /$, /\<, /\>, /\zs, /\ze).
case!(line_start, "pattern.txt:803-818", "^foo", Magic::Magic, "x\nfoo", Some("foo"));
case!(line_end, "pattern.txt:820-835", "foo$", Magic::Magic, "foo\nbar", Some("foo"));
case!(mid_caret_literal, "pattern.txt:803-811", "a^b", Magic::Magic, "a^b", Some("a^b"));
case!(mid_dollar_literal, "pattern.txt:820-827", "a$b", Magic::Magic, "a$b", Some("a$b"));
case!(word_start, "pattern.txt:844-847", "\\<cat", Magic::Magic, "a cat", Some("cat"));
case!(word_end, "pattern.txt:849-852", "cat\\>", Magic::Magic, "cats cat!", Some("cat"));
case!(set_match_start, "pattern.txt:854-865", "foo\\zsbar", Magic::Magic, "foobar", Some("bar"));
case!(last_set_match_start_wins, "pattern.txt:854-865", "f\\zso\\zso", Magic::Magic, "foo", Some("o"));
case!(set_match_end, "pattern.txt:867-874", "foo\\zebar", Magic::Magic, "foobar", Some("foo"));
case!(set_both_boundaries, "pattern.txt:854-874", "x\\zsa\\zey", Magic::Magic, "xay", Some("a"));

#[test]
fn set_start_multiplier_is_compile_error() {
    let _spec = "pattern.txt:854-865";
    assert!(compile("\\zs*", Magic::Magic).is_err());
}

// pattern.txt:934-1014 (/\%l, /\%c, /\%v) and 895-932 (/\%V, /\%#, /\%'m).
case!(line_number_anchor, "pattern.txt:934-953", "\\%2lfoo", Magic::Magic, "bar\nfoo", Some("foo"));
case!(line_greater_anchor, "pattern.txt:934-953", "\\%>1lfoo", Magic::Magic, "bar\nfoo", Some("foo"));
case!(line_less_anchor, "pattern.txt:934-953", "\\%<2lbar", Magic::Magic, "bar\nfoo", Some("bar"));
case!(column_anchor, "pattern.txt:955-980", "\\%3cc", Magic::Magic, "abc", Some("c"));
case!(column_greater_anchor, "pattern.txt:955-980", "\\%>2cc", Magic::Magic, "abc", Some("c"));
case!(virtual_column_anchor, "pattern.txt:981-1014", "\\%2vb", Magic::Magic, "abc", Some("b"));
case!(virtual_column_expands_tab, "pattern.txt:981-1014", "\\%9vX", Magic::Magic, "\tX", Some("X"));

#[test]
fn visual_anchor_uses_text_context() {
    let _spec = "pattern.txt:895-903";
    let text = Text::from("abc").with_visual(1, 2);
    assert_eq!(matched("\\%Vb", Magic::Magic, &text), Some("b".to_owned()));
}

#[test]
fn cursor_anchor_uses_text_context() {
    let _spec = "pattern.txt:881-893";
    let text = Text::from("abc").with_cursor(1);
    assert_eq!(matched("\\%#b", Magic::Magic, &text), Some("b".to_owned()));
}

#[test]
fn mark_anchor_uses_text_context() {
    let _spec = "pattern.txt:905-932";
    let text = Text::from("abc").with_mark('m', 2);
    assert_eq!(matched("\\%'mc", Magic::Magic, &text), Some("c".to_owned()));
}

// pattern.txt:1103-1228 (/collection, /character-classes).
case!(collection_members, "pattern.txt:1103-1130", "[xyz]", Magic::Magic, "abz", Some("z"));
case!(negated_collection, "pattern.txt:1103-1130", "[^a]", Magic::Magic, "ab", Some("b"));
case!(collection_range, "pattern.txt:1131-1160", "[a-c]\\+", Magic::Magic, "xxabccz", Some("abcc"));
case!(literal_close_in_collection, "pattern.txt:1103-1130", "[]a]", Magic::Magic, "]", Some("]"));
case!(posix_digit, "pattern.txt:1161-1228", "[[:digit:]]\\+", Magic::Magic, "a123", Some("123"));
case!(posix_alpha_unicode, "test_regexp_utf8.vim:137-152", "[[:alpha:]]\\+", Magic::Magic, "123Motör", Some("Motör"));
case!(posix_print_unicode, "test_regexp_utf8.vim:41", "[[:print:]]\\+", Magic::Magic, "Motörhead", Some("Motörhead"));
case!(digit_class, "pattern.txt:519-536", "\\d\\+", Magic::Magic, "a123", Some("123"));
case!(non_digit_class, "pattern.txt:519-536", "\\D\\+", Magic::Magic, "12ab", Some("ab"));
case!(word_class, "pattern.txt:519-536", "\\w\\+", Magic::Magic, "!?a_2", Some("a_2"));
case!(head_class, "pattern.txt:519-536", "\\h\\w*", Magic::Magic, "2_name", Some("_name"));
case!(hex_class, "pattern.txt:519-536", "\\x\\+", Magic::Magic, "z1aF", Some("1aF"));
case!(octal_class, "pattern.txt:519-536", "\\o\\+", Magic::Magic, "89 701", Some("701"));
case!(space_class_excludes_newline, "pattern.txt:519-539", "\\s\\+", Magic::Magic, "a\n \tb", Some(" \t"));
case!(ident_without_digit, "pattern.txt:511-518", "\\I\\+", Magic::Magic, "2_name", Some("_name"));
case!(keyword_without_digit, "pattern.txt:511-518", "\\K\\+", Magic::Magic, "7word", Some("word"));
case!(keyword_matches_non_ascii_default_iskeyword, "test_functions.vim:Test_matchstrlist", "\\k\\+", Magic::Magic, "😊😊", Some("😊😊"));
case!(print_without_digit, "pattern.txt:511-518", "\\P\\+", Magic::Magic, "2abc", Some("abc"));
case!(decimal_character_atom, "pattern.txt:574-579", "\\%d65", Magic::Magic, "A", Some("A"));
case!(hex_character_atom, "pattern.txt:574-579", "\\%x2a", Magic::Magic, "*", Some("*"));
case!(octal_character_atom, "pattern.txt:574-579", "\\%o101", Magic::Magic, "A", Some("A"));
case!(unicode_character_atom, "pattern.txt:574-579", "\\%u20ac", Magic::Magic, "€", Some("€"));

// pattern.txt:537-546 and 1230-1248 (/\_, /\n, /\%[]), multiline model.
case!(literal_newline, "pattern.txt:541-547", "a\\nb", Magic::Magic, "a\nb", Some("a\nb"));
case!(newline_dot, "pattern.txt:493-495", "a\\_.b", Magic::Magic, "a\nb", Some("a\nb"));
case!(newline_space, "pattern.txt:537-539", "a\\_s\\+b", Magic::Magic, "a\n \tb", Some("a\n \tb"));
case!(newline_collection, "pattern.txt:1103-1108", "a\\_[x]b", Magic::Magic, "a\nb", Some("a\nb"));
case!(ordinary_dot_stops_at_line, "pattern.txt:493-495", "a.b", Magic::Magic, "a\nb", None);
case!(ordinary_negated_collection_stops_at_line, "pattern.txt:1103-1108", "a[^x]b", Magic::Magic, "a\nb", None);
case!(optional_atom_full, "pattern.txt:1229-1248", "r\\%[ead]", Magic::Magic, "read", Some("read"));
case!(optional_atom_prefix, "pattern.txt:1229-1248", "r\\%[ead]", Magic::Magic, "reaX", Some("rea"));
case!(optional_atom_empty_suffix, "pattern.txt:1229-1248", "r\\%[ead]", Magic::Magic, "rX", Some("r"));

// pattern.txt:548-563 (/1..9, /c, /C) and old regex tests.
case!(captured_backreference, "test_regexp_latin.vim:75-81", "\\(e\\)\\1", Magic::Magic, "three", Some("ee"));
case!(word_backreference, "pattern.txt:548-551", "\\(ab\\)\\1", Magic::Magic, "zabab", Some("abab"));
case!(ignore_case_flag, "pattern.txt:1260-1282", "\\cfoo", Magic::Magic, "FOO", Some("FOO"));
case!(case_flag_last_wins, "pattern.txt:1260-1282", "\\cfoo\\C", Magic::Magic, "FOO foo", Some("foo"));
case!(unicode_ignore_case, "pattern.txt:1260-1282", "\\cö", Magic::Magic, "Ö", Some("Ö"));
case!(unicode_ignore_case_range, "pattern.txt:1103-1160,1260-1282", "\\c[À-Ö]", Magic::Magic, "à", Some("à"));
case!(nested_capture_oracle, "test_regexp_latin.vim:54-59", "\\(\\(a[a-d] \\)*\\)\\(x\\)", Magic::Magic, "aa ab x", Some("aa ab x"));
case!(optional_capture_oracle, "test_regexp_latin.vim:66-70", "\\(abc\\>\\)\\?\\s*\\(def\\)", Magic::Magic, "abc def", Some("abc def"));

#[test]
fn captures_report_byte_positions() {
    let _spec = "pattern.txt:548-551; substitution-relevant capture reporting";
    let text = Text::from("é abab");
    let prog = compile("\\(ab\\)\\1", Magic::Magic).unwrap();
    let found = exec(&prog, &text).unwrap();
    assert_eq!((found.start.byte, found.end.byte), (3, 7));
    assert_eq!((found.captures[0].as_ref().unwrap().start.byte, found.captures[0].as_ref().unwrap().end.byte), (3, 5));
}

#[test]
fn multiline_positions_are_line_and_byte_based() {
    let _spec = "pattern.txt:934-965; multiline regexec model";
    let text = Text::from_lines(["é", "abc"]);
    let prog = compile("a", Magic::Magic).unwrap();
    let found = exec(&prog, &text).unwrap();
    assert_eq!(found.start, Position { lnum: 2, col: 0, byte: 3 });
    assert_eq!(found.end, Position { lnum: 2, col: 1, byte: 4 });
}

#[test]
fn exec_at_starts_search_at_supplied_position() {
    let _spec = "regexp.c:vim_regexec startcol entry points";
    let text = Text::from("cat cat");
    let prog = compile("cat", Magic::Magic).unwrap();
    let found = exec_at(&prog, &text, Position { lnum: 1, col: 4, byte: 4 }).unwrap();
    assert_eq!((found.start.byte, found.end.byte), (4, 7));
}

#[test]
fn automatic_engine_prefers_nfa_for_regular_patterns() {
    let _spec = "regexp.c:16135-16143";
    assert_eq!(compile("abc", Magic::Magic).unwrap().engine(), Engine::Nfa);
}

#[test]
fn automatic_engine_routes_backrefs_to_bounded_bt() {
    let _spec = "regexp.c:16135-16167; task-06 engine-selection rule";
    assert_eq!(compile("\\(a\\)\\1", Magic::Magic).unwrap().engine(), Engine::Backtracking);
}

#[test]
fn automatic_engine_routes_lookbehind_to_bounded_bt() {
    let _spec = "regexp.c:16135-16167; regexp.c RF_LOOKBH:582";
    assert_eq!(compile("\\(a\\)\\@<=b", Magic::Magic).unwrap().engine(), Engine::Backtracking);
}

#[test]
fn engine_prefix_forces_backtracking() {
    let _spec = "pattern.txt:383-402; regexp.c:16108-16143";
    assert_eq!(compile("\\%#=1abc", Magic::Magic).unwrap().engine(), Engine::Backtracking);
}

#[test]
fn engine_prefix_forces_nfa_with_backref_fallback() {
    let _spec = "pattern.txt:383-402; regexp.c:16108-16143";
    let prog = compile("\\%#=2\\(a\\)\\1", Magic::Magic).unwrap();
    assert_eq!(prog.engine(), Engine::Nfa);
    assert_eq!(exec(&prog, &Text::from("aa")).map(|m| m.end.byte), Some(2));
}

#[test]
fn forced_nfa_preserves_lookahead_capture_for_backref() {
    let _spec = "pattern.txt:676-690,548-551; regexp.c recursive_regmatch";
    let prog = compile("\\%#=2\\(foo\\)\\@=\\1", Magic::Magic).unwrap();
    let found = exec(&prog, &Text::from("foo")).unwrap();
    assert_eq!((found.start.byte, found.end.byte), (0, 3));
    assert_eq!((found.captures[0].as_ref().unwrap().start.byte, found.captures[0].as_ref().unwrap().end.byte), (0, 3));
}

#[test]
fn automatic_engine_routes_pathological_range_to_bt() {
    let _spec = "regexp.c:10969-10973";
    assert_eq!(compile("a\\{0,501}", Magic::Magic).unwrap().engine(), Engine::Backtracking);
    assert_eq!(compile("a*", Magic::Magic).unwrap().engine(), Engine::Nfa);
}

#[test]
fn malformed_group_is_typed_compile_error() {
    let _spec = "pattern.txt:371-380";
    assert!(compile("\\(abc", Magic::Magic).is_err());
}

#[test]
fn lookaround_suffix_without_atom_is_compile_error() {
    let _spec = "test_functions.vim:Test_matchstrlist";
    assert!(compile("\\@=", Magic::Magic).is_err());
}

#[test]
fn forward_or_open_group_backref_is_rejected() {
    let _spec = "test_regexp_latin.vim:80-81";
    assert!(compile("\\(e\\1\\)", Magic::Magic).is_err());
}

#[test]
fn reversed_range_is_typed_compile_error() {
    let _spec = "pattern.txt:1103-1160";
    assert!(compile("[z-a]", Magic::Magic).is_err());
}

#[test]
fn invalid_engine_selector_is_rejected() {
    let _spec = "regexp.c:16108-16125";
    assert!(compile("\\%#=9abc", Magic::Magic).is_err());
}

#[test]
fn explicit_step_limit_is_typed() {
    let _spec = "regexp.c:14295-14299 NFA_MAX_STATES";
    let prog = compile("a*", Magic::Magic).unwrap().with_limits(2, 1024);
    assert_eq!(try_exec(&prog, &Text::from("aaaa")), Err(ExecError::StepLimit));
}

#[test]
fn explicit_recursion_limit_is_typed() {
    let _spec = "regexp.c bt_regexec depth guards; test_regexp_latin.vim:123-126";
    let prog = compile("\\%#=1a*", Magic::Magic).unwrap().with_limits(1_000, 2);
    assert_eq!(try_exec(&prog, &Text::from("aaaa")), Err(ExecError::RecursionLimit));
}
