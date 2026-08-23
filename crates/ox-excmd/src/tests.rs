use std::collections::HashSet;

use crate::{
    AddressBase, COMMANDS, CmdlineContext, CmdlineSpecial, ErrorCode, ExpansionPart, ModifierKind,
    Parser, RangeKind, RangeSeparator, ResolvedCommand, UserCommandMatch, UserCommandProvider,
    command_spec, expand_with, resolve_command, scan_expansions,
};

fn parse_one(input: &str) -> crate::ExCommand {
    let parsed = Parser::new().parse(input);
    assert!(parsed.is_ok(), "{input:?}: {parsed:?}");
    let mut commands = parsed.unwrap_or_default();
    assert_eq!(commands.len(), 1, "{input:?}");
    commands.remove(0)
}

// src/nvim/ex_docmd.c:3057-3165 (find_ex_command): built-ins use the first
// table-prefix match, with the one-letter substitute special case and user
// commands as fallback.
macro_rules! resolution_case {
    ($name:ident, $typed:literal, $canonical:literal) => {
        #[test]
        fn $name() {
            let resolved = resolve_command($typed, &crate::NoUserCommands);
            assert_eq!(resolved.map(|command| command.name().to_owned()), Ok($canonical.to_owned()));
        }
    };
}

resolution_case!(resolve_append, "a", "append");
resolution_case!(resolve_abbreviate, "ab", "abbreviate");
resolution_case!(resolve_abclear, "abc", "abclear");
resolution_case!(resolve_aboveleft, "abo", "aboveleft");
resolution_case!(resolve_argadd, "arga", "argadd");
resolution_case!(resolve_argdelete, "argd", "argdelete");
resolution_case!(resolve_argdo, "argdo", "argdo");
resolution_case!(resolve_buffer, "b", "buffer");
resolution_case!(resolve_bnext, "bn", "bnext");
resolution_case!(resolve_bprevious, "bp", "bprevious");
resolution_case!(resolve_change, "c", "change");
resolution_case!(resolve_cfile_order, "cf", "cfile");
resolution_case!(resolve_delete, "d", "delete");
resolution_case!(resolve_edit, "e", "edit");
resolution_case!(resolve_echo, "ec", "echo");
resolution_case!(resolve_echoerr, "echoe", "echoerr");
resolution_case!(resolve_echomsg, "echom", "echomsg");
resolution_case!(resolve_global, "g", "global");
resolution_case!(resolve_help, "h", "help");
resolution_case!(resolve_insert, "i", "insert");
resolution_case!(resolve_join, "j", "join");
resolution_case!(resolve_k_mark, "k", "k");
resolution_case!(resolve_list, "l", "list");
resolution_case!(resolve_move, "m", "move");
resolution_case!(resolve_next, "n", "next");
resolution_case!(resolve_print, "p", "print");
resolution_case!(resolve_quit, "q", "quit");
resolution_case!(resolve_read, "r", "read");
resolution_case!(resolve_substitute_special, "s", "substitute");
resolution_case!(resolve_undo, "u", "undo");
resolution_case!(resolve_vglobal, "v", "vglobal");
resolution_case!(resolve_write, "w", "write");
resolution_case!(resolve_xit, "x", "xit");
resolution_case!(resolve_yank, "y", "yank");
resolution_case!(resolve_bang_symbol, "!", "!");

#[test]
fn generated_table_has_current_upstream_count() {
    assert_eq!(COMMANDS.len(), 564);
}

#[test]
fn generated_table_names_are_unique() {
    let names: HashSet<_> = COMMANDS.iter().map(|spec| spec.name).collect();
    assert_eq!(names.len(), COMMANDS.len());
}

#[test]
fn generated_substitute_metadata_is_complete() {
    let spec = command_spec("substitute");
    assert!(spec.is_some());
    let spec = spec.unwrap_or(&COMMANDS[0]);
    assert_eq!(spec.abbr, "s");
    assert!(spec.flags.contains(crate::CommandFlags::RANGE));
    assert!(spec.flags.contains(crate::CommandFlags::EXTRA));
}

struct Users;
impl UserCommandProvider for Users {
    fn resolve_user_command(&self, typed: &str) -> UserCommandMatch {
        match typed {
            "Build" => UserCommandMatch::Match("BuildAll".to_owned()),
            "Build2" => UserCommandMatch::Match("Build2".to_owned()),
            "Amb" => UserCommandMatch::Ambiguous,
            _ => UserCommandMatch::None,
        }
    }
}

#[test]
fn user_command_is_uppercase_fallback() {
    assert_eq!(
        resolve_command("Build", &Users),
        Ok(ResolvedCommand::User("BuildAll".to_owned()))
    );
}

#[test]
fn builtin_precedes_user_provider() {
    assert_eq!(
        resolve_command("Next", &Users).map(|command| command.name().to_owned()),
        Ok("Next".to_owned())
    );
}

#[test]
fn ambiguous_user_command_is_reported() {
    assert_eq!(
        resolve_command("Amb", &Users),
        Err(crate::ResolveError::AmbiguousUserCommand)
    );
}

#[test]
fn user_command_name_may_contain_digits() {
    let commands = Parser::with_user_commands(&Users).parse("Build2 arg").unwrap_or_default();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command.name(), "Build2");
    assert_eq!(commands[0].args, "arg");
}

// src/nvim/ex_docmd.c:2826-2949 (parse_cmd_address): ranges accept numeric/
// current/last/mark/search addresses with signed offsets and comma/semicolon
// evaluation semantics; `%` whole-buffer shorthand and omitted endpoints are
// handled by the same function.
macro_rules! address_case {
    ($name:ident, $input:literal, $base:pat, $offsets:expr) => {
        #[test]
        fn $name() {
            let command = parse_one(concat!($input, "print"));
            let range = command.range.unwrap_or_else(|| unreachable!());
            let address = range.start.unwrap_or_else(|| unreachable!());
            assert!(matches!(address.base, $base));
            assert_eq!(address.offsets, $offsets);
        }
    };
}

fn search_value_matches(base: &AddressBase, expected: &str, forward: bool) -> bool {
    match (base, forward) {
        (AddressBase::ForwardSearch(value), true) | (AddressBase::BackwardSearch(value), false) => {
            value == expected
        }
        _ => false,
    }
}

address_case!(range_line_zero, "0", AddressBase::Line(0), Vec::<i64>::new());
address_case!(range_line_one, "1", AddressBase::Line(1), Vec::<i64>::new());
address_case!(range_line_large, "1234", AddressBase::Line(1234), Vec::<i64>::new());
address_case!(range_current, ".", AddressBase::Current, Vec::<i64>::new());
address_case!(range_last, "$", AddressBase::Last, Vec::<i64>::new());
address_case!(range_mark_a, "'a", AddressBase::Mark('a'), Vec::<i64>::new());
address_case!(range_mark_angle, "'<", AddressBase::Mark('<'), Vec::<i64>::new());

#[test]
fn range_forward_search() {
    let command = parse_one("/needle/print");
    let range = command.range.unwrap_or_else(|| unreachable!());
    let address = range.start.unwrap_or_else(|| unreachable!());
    assert!(search_value_matches(&address.base, "needle", true));
}

#[test]
fn range_backward_search() {
    let command = parse_one("?needle?print");
    let range = command.range.unwrap_or_else(|| unreachable!());
    let address = range.start.unwrap_or_else(|| unreachable!());
    assert!(search_value_matches(&address.base, "needle", false));
}

address_case!(range_plus_default, "+", AddressBase::Current, vec![1]);
address_case!(range_minus_default, "-", AddressBase::Current, vec![-1]);
address_case!(range_plus_number, ".+12", AddressBase::Current, vec![12]);
address_case!(range_minus_number, "$-3", AddressBase::Last, vec![-3]);
address_case!(range_multiple_offsets, "10+2-4+", AddressBase::Line(10), vec![2, -4, 1]);

#[test]
fn whole_buffer_range_is_explicit() {
    let range = parse_one("%print").range.unwrap_or_else(|| unreachable!());
    assert_eq!(range.kind, RangeKind::WholeBuffer);
    assert!(matches!(range.start.map(|address| address.base), Some(AddressBase::Line(1))));
    assert!(matches!(range.end.map(|address| address.base), Some(AddressBase::Last)));
}

#[test]
fn comma_range_does_not_advance_cursor() {
    let range = parse_one("1,5print").range.unwrap_or_else(|| unreachable!());
    assert_eq!(
        range.kind,
        RangeKind::Pair { separator: RangeSeparator::Comma, cursor_advance: false }
    );
}

#[test]
fn semicolon_range_advances_cursor() {
    let range = parse_one("/one/;?two?print").range.unwrap_or_else(|| unreachable!());
    assert_eq!(
        range.kind,
        RangeKind::Pair { separator: RangeSeparator::Semicolon, cursor_advance: true }
    );
}

#[test]
fn omitted_first_range_address_means_current() {
    let range = parse_one(",5print").range.unwrap_or_else(|| unreachable!());
    assert!(matches!(range.start.map(|address| address.base), Some(AddressBase::Current)));
}

#[test]
fn omitted_second_range_address_means_current() {
    let range = parse_one("5,print").range.unwrap_or_else(|| unreachable!());
    assert!(matches!(range.end.map(|address| address.base), Some(AddressBase::Current)));
}

#[test]
fn chained_range_retains_final_two_addresses() {
    let range = parse_one("1,2,3print").range.unwrap_or_else(|| unreachable!());
    assert!(matches!(range.start.map(|address| address.base), Some(AddressBase::Line(2))));
    assert!(matches!(range.end.map(|address| address.base), Some(AddressBase::Line(3))));
}

#[test]
fn earlier_semicolon_in_chain_records_cursor_advance() {
    let range = parse_one("1;2,3print").range.unwrap_or_else(|| unreachable!());
    assert_eq!(
        range.kind,
        RangeKind::Pair { separator: RangeSeparator::Comma, cursor_advance: true }
    );
}

// src/nvim/ex_docmd.c:2464-2725 (parse_command_modifiers) and 3167-3227
// (cmdmods[]): modifiers have minimum prefixes, preserve stack order, and
// permit counts for the count-bearing forms.
macro_rules! modifier_case {
    ($name:ident, $typed:literal, $kind:path) => {
        #[test]
        fn $name() {
            let command = parse_one(concat!($typed, " print"));
            assert_eq!(command.modifiers.len(), 1);
            assert_eq!(command.modifiers[0].kind, $kind);
        }
    };
}

modifier_case!(modifier_aboveleft, "abo", ModifierKind::AboveLeft);
modifier_case!(modifier_belowright, "bel", ModifierKind::BelowRight);
modifier_case!(modifier_botright, "bo", ModifierKind::BotRight);
modifier_case!(modifier_browse, "bro", ModifierKind::Browse);
modifier_case!(modifier_confirm, "conf", ModifierKind::Confirm);
modifier_case!(modifier_horizontal, "hor", ModifierKind::Horizontal);
modifier_case!(modifier_keepalt, "keepa", ModifierKind::KeepAlt);
modifier_case!(modifier_keepjumps, "keepj", ModifierKind::KeepJumps);
modifier_case!(modifier_keepmarks, "kee", ModifierKind::KeepMarks);
modifier_case!(modifier_keeppatterns, "keepp", ModifierKind::KeepPatterns);
modifier_case!(modifier_leftabove, "lefta", ModifierKind::LeftAbove);
modifier_case!(modifier_noautocmd, "noa", ModifierKind::NoAutocmd);
modifier_case!(modifier_sandbox, "san", ModifierKind::Sandbox);
modifier_case!(modifier_vertical, "vert", ModifierKind::Vertical);

#[test]
fn modifiers_preserve_stack_order() {
    let command = parse_one("silent keepjumps vertical print");
    let kinds: Vec<_> = command.modifiers.iter().map(|modifier| modifier.kind).collect();
    assert_eq!(kinds, [ModifierKind::Silent, ModifierKind::KeepJumps, ModifierKind::Vertical]);
}

#[test]
fn silent_bang_is_part_of_modifier() {
    let command = parse_one("silent! print");
    assert!(command.modifiers[0].bang);
}

#[test]
fn verbose_modifier_carries_count() {
    let command = parse_one("3verbose print");
    assert_eq!(command.modifiers[0].count, Some(3));
}

#[test]
fn silent_modifier_carries_count() {
    let command = parse_one("3silent print");
    assert_eq!(command.modifiers[0].count, Some(3));
}

#[test]
fn tab_modifier_carries_count() {
    let command = parse_one("2tab print");
    assert_eq!(command.modifiers[0].count, Some(2));
}

// ":filter {pat} cmd" — the optional `!` and delimited pattern are part of
// the modifier (parse_command_modifiers 'f' case: ex_docmd.c:2558-2591).
#[test]
fn filter_modifier_parses_and_retains_pattern() {
    let command = parse_one("filter /foo/ print");
    assert_eq!(command.modifiers.len(), 1);
    assert_eq!(command.modifiers[0].kind, ModifierKind::Filter);
    assert_eq!(command.modifiers[0].pattern.as_deref(), Some("foo"));
    assert!(!command.modifiers[0].bang);
    assert_eq!(command.command.name(), "print");
}

#[test]
fn filter_modifier_bang_conveys_force() {
    let command = parse_one("filter! /foo/ print");
    assert_eq!(command.modifiers.len(), 1);
    assert_eq!(command.modifiers[0].kind, ModifierKind::Filter);
    assert!(command.modifiers[0].bang);
    assert_eq!(command.modifiers[0].pattern.as_deref(), Some("foo"));
    assert_eq!(command.command.name(), "print");
}

#[test]
fn filter_modifier_accepts_bare_pattern() {
    let command = parse_one("filter foo print");
    assert_eq!(command.modifiers.len(), 1);
    assert_eq!(command.modifiers[0].kind, ModifierKind::Filter);
    assert_eq!(command.modifiers[0].pattern.as_deref(), Some("foo"));
    assert_eq!(command.command.name(), "print");
}

#[test]
fn filter_modifier_pattern_may_abut_the_command() {
    let command = parse_one("filter /foo/print");
    assert_eq!(command.modifiers.len(), 1);
    assert_eq!(command.modifiers[0].kind, ModifierKind::Filter);
    assert_eq!(command.modifiers[0].pattern.as_deref(), Some("foo"));
    assert_eq!(command.command.name(), "print");
}

#[test]
fn filter_without_pattern_is_not_a_modifier() {
    // ":filter" alone falls through to the builtin (NEEDARG -> E471).
    assert_eq!(
        Parser::new().parse("filter").expect_err("filter requires an argument").code,
        ErrorCode::E471
    );
}

// ":filter /foo/" has no nested command, so it routes to the builtin filter
// command (upstream parse_command_modifiers falls through for the 'f' case
// when no command follows the pattern).
#[test]
fn filter_pattern_without_nested_command_is_builtin() {
    let command = parse_one("filter /foo/");
    assert!(command.modifiers.is_empty());
    assert_eq!(command.command.name(), "filter");
    assert_eq!(command.args, "/foo/");
}

#[test]
fn filter_pattern_with_nested_command_stays_modifier() {
    let command = parse_one("filter /foo/ print");
    assert_eq!(command.modifiers.len(), 1);
    assert_eq!(command.modifiers[0].kind, ModifierKind::Filter);
    assert_eq!(command.modifiers[0].pattern.as_deref(), Some("foo"));
    assert_eq!(command.command.name(), "print");
}

// ":hide" and ":hide | cmd" are the builtin command; only ":hide cmd" is a
// modifier (parse_command_modifiers 'h' case: ex_docmd.c:2597-2603).
#[test]
fn hide_alone_is_the_builtin_command() {
    let command = parse_one("hide");
    assert_eq!(command.command.name(), "hide");
    assert!(command.modifiers.is_empty());
}

#[test]
fn hide_followed_by_bar_is_the_builtin_command() {
    let parsed = Parser::new().parse("hide | print").unwrap_or_default();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].command.name(), "hide");
    assert!(parsed[0].modifiers.is_empty());
    assert_eq!(parsed[1].command.name(), "print");
}

#[test]
fn hide_is_a_modifier_when_a_command_follows() {
    let command = parse_one("hide print");
    assert_eq!(command.modifiers.len(), 1);
    assert_eq!(command.modifiers[0].kind, ModifierKind::Hide);
    assert_eq!(command.command.name(), "print");
}

// src/nvim/ex_docmd.c:4112-4165 (separate_nextcmd): EX_TRLBAR gates generic
// bar splitting; commands such as :normal and :global own their remaining
// argument text.
#[test]
fn substitute_splits_at_trailing_bar() {
    let commands = Parser::new().parse("s/a/b/ | echo done").unwrap_or_default();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].args, "/a/b/");
    assert_eq!(commands[1].command.name(), "echo");
}

// vimgrep-family patterns are regexes that may contain `|`; the leading
// pattern (+ g/j/f flags) is skipped before bar scanning so the command
// splits after the file argument (separate_nextcmd via skip_grep_pat:
// ex_docmd.c:4112-4165 and 3840-3854).
#[test]
fn vimgrep_pattern_bar_is_not_a_separator() {
    let commands = Parser::new().parse("vimgrep /foo|bar/ f | copen").unwrap_or_default();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].command.name(), "vimgrep");
    assert_eq!(commands[0].args, "/foo|bar/ f");
    assert_eq!(commands[1].command.name(), "copen");
}

#[test]
fn vimgrepadd_pattern_bar_is_not_a_separator() {
    let commands = Parser::new().parse("vimgrepadd /a|b/ file | echo x").unwrap_or_default();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].command.name(), "vimgrepadd");
    assert_eq!(commands[0].args, "/a|b/ file");
    assert_eq!(commands[1].command.name(), "echo");
}

#[test]
fn lvimgrep_pattern_with_flags_splits_after_files() {
    let commands = Parser::new().parse("lvimgrep /a|b/g file | print").unwrap_or_default();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].command.name(), "lvimgrep");
    assert_eq!(commands[0].args, "/a|b/g file");
}

#[test]
fn substitute_pattern_may_contain_bar() {
    let commands = Parser::new().parse(r"s/a\|b/c/ | print").unwrap_or_default();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].args, r"/a\|b/c/");
}

#[test]
fn substitute_g_flag_form_is_one_letter_command() {
    let command = parse_one("sg/foo/bar/");
    assert_eq!(command.command.name(), "substitute");
    assert_eq!(command.args, "g/foo/bar/");
}

#[test]
fn mark_command_directly_precedes_mark_name() {
    let command = parse_one("kz");
    assert_eq!(command.command.name(), "k");
    assert_eq!(command.args, "z");
}

#[test]
fn escaped_bar_stays_in_argument() {
    let command = parse_one(r"write file\|name");
    assert_eq!(command.args, r"file\|name");
}

#[test]
fn normal_consumes_bar() {
    let command = parse_one("normal |x");
    assert_eq!(command.args, "|x");
}

#[test]
fn global_consumes_bar() {
    let command = parse_one("global/foo/print | echo later");
    assert_eq!(command.args, "/foo/print | echo later");
}

#[test]
fn write_splits_at_bar() {
    let commands = Parser::new().parse("write file | print").unwrap_or_default();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].args, "file");
}

#[test]
fn echo_splits_at_top_level_bar() {
    let commands = Parser::new().parse("echo 'a|b' | print").unwrap_or_default();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].args, "'a|b'");
}

#[test]
fn echo_boolean_or_is_not_a_separator() {
    let commands = Parser::new().parse("echo left || right | print").unwrap_or_default();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].args, "left || right");
}

#[test]
fn append_owns_argument_bar() {
    assert_eq!(parse_one("append |text").args, "|text");
}

#[test]
fn change_owns_argument_bar() {
    assert_eq!(parse_one("change |text").args, "|text");
}

#[test]
fn insert_owns_argument_bar() {
    assert_eq!(parse_one("insert |text").args, "|text");
}

/// ":write !cmd" and ":read !cmd" spend the "!" on `usefilter` and keep the
/// rest of the line as one shell command (ex_docmd.c:2256-2275, 2291-2313).
#[test]
fn write_filter_owns_shell_pipeline() {
    let command = parse_one("write !cat | sed s/a/b/");
    assert!(command.usefilter);
    assert!(!command.bang);
    assert_eq!(command.args, "cat | sed s/a/b/");
}

#[test]
fn read_filter_owns_shell_pipeline() {
    let command = parse_one("read !printf a|b");
    assert!(command.usefilter);
    assert_eq!(command.args, "printf a|b");
}

/// ":r!cmd" is the same filter form: the bang is consumed by `usefilter`
/// rather than left as a force flag (ex_docmd.c:2269-2271).
#[test]
fn read_bang_selects_the_filter_and_clears_the_bang() {
    let command = parse_one("read!printf a|b");
    assert!(command.usefilter);
    assert!(!command.bang);
    assert_eq!(command.args, "printf a|b");
}

/// ":read file" is an ordinary TRLBAR command: no filter, and a bar still
/// separates the next command.
#[test]
fn read_file_is_not_a_filter_and_splits_at_bar() {
    let commands = Parser::new().parse("read one.txt | print").unwrap_or_default();
    assert_eq!(commands.len(), 2);
    assert!(!commands[0].usefilter);
    assert_eq!(commands[0].args, "one.txt");
    assert_eq!(commands[1].command.name(), "print");
}

/// ":w!" is still a forced write, not a filter (only ":r!" maps its bang
/// onto `usefilter`).
#[test]
fn write_bang_stays_a_force_flag() {
    let command = parse_one("write! out.txt");
    assert!(!command.usefilter);
    assert!(command.bang);
    assert_eq!(command.args, "out.txt");
}

#[test]
fn leading_colons_and_spaces_are_skipped() {
    let command = parse_one("  :: print");
    assert_eq!(command.command.name(), "print");
}

#[test]
fn leading_quote_is_a_comment() {
    assert!(Parser::new().parse("\" comment").unwrap_or_default().is_empty());
}

#[test]
fn quote_starts_comment_without_preceding_space() {
    let commands = Parser::new().parse("write foo\"ignored | print").unwrap_or_default();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].args, "foo");
}

#[test]
fn at_command_initial_quote_is_register_argument() {
    let command = parse_one("@\"");
    assert_eq!(command.args, "\"");
}

#[test]
fn redir_at_quote_is_not_comment() {
    let command = parse_one("redir @\"");
    assert_eq!(command.args, "@\"");
}

#[test]
fn delete_extracts_register() {
    let command = parse_one("delete a");
    assert_eq!(command.register, Some('a'));
    assert!(command.args.is_empty());
}

#[test]
fn delete_extracts_quoted_register() {
    let command = parse_one("delete \"+");
    assert_eq!(command.register, Some('+'));
}

#[test]
fn delete_extracts_register_then_count() {
    let command = parse_one("delete a 4");
    assert_eq!(command.register, Some('a'));
    assert_eq!(command.count, Some(4));
}

#[test]
fn comment_after_delete_register_is_skipped() {
    let command = parse_one("delete a \" comment");
    assert_eq!(command.register, Some('a'));
    assert!(command.args.is_empty());
}

#[test]
fn buffer_extracts_count() {
    let command = parse_one("buffer 7");
    assert_eq!(command.count, Some(7));
}

#[test]
fn bang_is_recorded() {
    assert!(parse_one("write! file").bang);
}

#[test]
fn raw_arguments_preserve_internal_space() {
    assert_eq!(parse_one("write one  two").args, "one  two");
}

#[test]
fn expression_arguments_use_ox_eval() {
    let command = parse_one("eval 1 + 2");
    assert!(command.parse_expression_args().is_ok());
}

// src/nvim/ex_docmd.c:7488-7519 (find_cmdline_var) and 7551 onward (eval_vars):
// command-line specials are recognized only when unescaped; <lt> emits a
// literal less-than sign.
struct Context;
impl CmdlineContext for Context {
    fn resolve(&self, special: CmdlineSpecial) -> Option<String> {
        match special {
            CmdlineSpecial::CurrentFile => Some("current.txt".to_owned()),
            CmdlineSpecial::AlternateFile => Some("alternate.txt".to_owned()),
            CmdlineSpecial::CurrentWord => Some("word".to_owned()),
            _ => None,
        }
    }
}

macro_rules! expansion_case {
    ($name:ident, $input:literal, $special:path) => {
        #[test]
        fn $name() {
            let parts = scan_expansions($input);
            assert!(parts.iter().any(|part| matches!(part, ExpansionPart::Placeholder { special: $special, .. })));
        }
    };
}

expansion_case!(expand_current_file, "%", CmdlineSpecial::CurrentFile);
expansion_case!(expand_alternate_file, "#", CmdlineSpecial::AlternateFile);
expansion_case!(expand_current_word, "<cword>", CmdlineSpecial::CurrentWord);
expansion_case!(expand_current_big_word, "<cWORD>", CmdlineSpecial::CurrentBigWord);
expansion_case!(expand_cursor_file, "<cfile>", CmdlineSpecial::CurrentFileUnderCursor);
expansion_case!(expand_cursor_expression, "<cexpr>", CmdlineSpecial::CurrentExpression);
expansion_case!(expand_autocmd_file, "<afile>", CmdlineSpecial::AutocmdFile);
expansion_case!(expand_autocmd_buffer, "<abuf>", CmdlineSpecial::AutocmdBuffer);
expansion_case!(expand_autocmd_match, "<amatch>", CmdlineSpecial::AutocmdMatch);
expansion_case!(expand_script_file, "<sfile>", CmdlineSpecial::ScriptFile);
expansion_case!(expand_script_line, "<slnum>", CmdlineSpecial::ScriptLine);
expansion_case!(expand_script_stack, "<stack>", CmdlineSpecial::ScriptStack);
expansion_case!(expand_script_definition, "<script>", CmdlineSpecial::ScriptDefinition);
expansion_case!(expand_script_file_line, "<sflnum>", CmdlineSpecial::ScriptFileLine);
expansion_case!(expand_script_id, "<SID>", CmdlineSpecial::ScriptId);

#[test]
fn escaped_percent_is_literal() {
    assert_eq!(scan_expansions(r"\%"), [ExpansionPart::Literal { text: "%".to_owned(), span: 0..2 }]);
}

#[test]
fn escaped_angle_special_is_literal() {
    assert_eq!(scan_expansions(r"\<cword>"), [ExpansionPart::Literal { text: "<cword>".to_owned(), span: 0..8 }]);
}

#[test]
fn less_than_escape_is_literal() {
    assert_eq!(scan_expansions("<lt>"), [ExpansionPart::Literal { text: "<".to_owned(), span: 0..4 }]);
}

#[test]
fn escaped_less_than_escape_keeps_token_spelling() {
    assert_eq!(
        scan_expansions(r"\<lt>"),
        [ExpansionPart::Literal { text: "<lt>".to_owned(), span: 0..5 }]
    );
}

#[test]
fn host_resolves_known_values_and_preserves_unknown() {
    assert_eq!(expand_with("% # <afile> <cword>", &Context), "current.txt alternate.txt <afile> word");
}

#[test]
fn literal_segments_surround_placeholders() {
    let parts = scan_expansions("pre%post");
    assert_eq!(parts.len(), 3);
    assert!(matches!(&parts[0], ExpansionPart::Literal { text, .. } if text == "pre"));
    assert!(matches!(&parts[2], ExpansionPart::Literal { text, .. } if text == "post"));
}

// Error sites correspond to checks in do_one_cmd (ex_docmd.c:1979 onward)
// and resolve_command/find_ex_command: E492 unknown command, E481 disallowed
// range, E488 trailing input, and E471 missing required argument.
macro_rules! error_case {
    ($name:ident, $input:literal, $code:path) => {
        #[test]
        fn $name() {
            let error = Parser::new().parse($input).expect_err("input must fail");
            assert_eq!(error.code, $code);
            assert!(error.offset <= $input.len());
        }
    };
}

error_case!(error_unknown_command, "doesnotexist", ErrorCode::E492);
error_case!(error_unknown_after_colon, ":doesnotexist", ErrorCode::E492);
error_case!(error_forced_ho_abbreviation, "ho", ErrorCode::E492);
error_case!(error_forced_def_abbreviation, "def", ErrorCode::E492);
error_case!(error_range_not_allowed, "2echo hi", ErrorCode::E481);
error_case!(error_percent_not_allowed, "%echo hi", ErrorCode::E481);
error_case!(error_bang_not_allowed, "print!", ErrorCode::E488);
error_case!(error_trailing_characters, "undo extra", ErrorCode::E488);
error_case!(error_argument_required, "badd", ErrorCode::E471);
error_case!(error_unterminated_search, "/open print", ErrorCode::E488);

#[test]
fn unknown_command_error_offset_is_byte_accurate() {
    let error = Parser::new().parse(":  Bogus").expect_err("input must fail");
    assert_eq!(error.offset, 3);
}
