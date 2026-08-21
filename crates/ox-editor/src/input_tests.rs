//! Behavioral input tests extracted from upstream autocmd, input, and mapping tests.

use std::collections::BTreeSet;

use ox_types::BufHandle;

use crate::autocmd::{
    AutocmdContext, AutocmdError, AutocmdKind, AutocmdOptions, Autocmds, AugroupId,
    DeleteAutocmds, Event, PatternKind, EVENT_COUNT,
};
use crate::mapping::{
    Lookup, MapMode, MapModes, MapScope, MappingAction, MappingError, MappingOptions, Mappings,
};
use crate::typeahead::{
    Key, KeyDecodeError, Keys, Remap, Typeahead, TypeaheadError, TypeaheadFlags, KE_FILLER,
    KS_EXTRA, KS_SPECIAL, KS_ZERO, K_SPECIAL,
};
use crate::Editor;

fn buffer(value: i64) -> BufHandle {
    BufHandle::try_from(value).unwrap()
}

fn ex(value: &str) -> AutocmdKind {
    AutocmdKind::ExString(value.to_owned())
}

fn context<'a>(buffer: Option<BufHandle>, file_name: Option<&'a str>) -> AutocmdContext<'a> {
    AutocmdContext {
        buffer,
        file_name,
        // A top-level event is not raised inside a non-nested outer autocmd,
        // so nesting is permitted and no event-level gate applies.
        nested: true,
    }
}

fn map_options(mode: MapMode) -> MappingOptions {
    MappingOptions {
        modes: mode.into(),
        ..MappingOptions::default()
    }
}

fn keys(value: &str) -> Keys {
    Keys::from(value)
}

// Autocmd event table and matching semantics cite:
// src/nvim/auevents.lua:5-161; src/nvim/autocmd.c:887-1028,1865-1890;
// src/nvim/fileio.c:3694-3869; test/old/testdir/test_autocmd.vim.

#[test]
fn event_table_has_every_upstream_event() {
    assert_eq!(EVENT_COUNT, 146);
    assert_eq!(Event::ALL.len(), 146);
}

#[test]
fn event_table_names_are_unique() {
    let names: BTreeSet<_> = Event::ALL.iter().map(|event| event.as_str()).collect();
    assert_eq!(names.len(), EVENT_COUNT);
}

#[test]
fn event_aliases_resolve_to_canonical_events() {
    assert_eq!(Event::from_name("BufCreate"), Some(Event::BufAdd));
    assert_eq!(Event::from_name("BufRead"), Some(Event::BufReadPost));
    assert_eq!(Event::from_name("BufWrite"), Some(Event::BufWritePre));
    assert_eq!(Event::from_name("FileEncoding"), Some(Event::EncodingChanged));
}

#[test]
fn unknown_event_name_is_rejected() {
    assert_eq!(Event::from_name("NotAnEvent"), None);
}

#[test]
fn event_pattern_kinds_cover_file_buffer_and_match_text() {
    assert_eq!(Event::BufReadPost.pattern_kind(), PatternKind::File);
    assert_eq!(Event::CursorMoved.pattern_kind(), PatternKind::Buffer);
    assert_eq!(Event::User.pattern_kind(), PatternKind::None);
}

#[test]
fn augroup_creation_is_idempotent() {
    let mut autocmds = Autocmds::new();
    let first = autocmds.create_group("build", false).unwrap();
    let second = autocmds.create_group("build", false).unwrap();
    assert_eq!(first, second);
}

#[test]
fn empty_augroup_name_is_rejected() {
    assert_eq!(
        Autocmds::new().create_group("", false),
        Err(AutocmdError::EmptyGroupName)
    );
}

#[test]
fn deleting_augroup_removes_name_and_definitions() {
    let mut autocmds = Autocmds::new();
    let group = autocmds.create_group("gone", false).unwrap();
    autocmds
        .register(
            Event::BufEnter,
            "*",
            ex("echo gone"),
            AutocmdOptions {
                group,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    autocmds.delete_group(group).unwrap();
    assert_eq!(autocmds.group("gone"), None);
    assert!(autocmds.is_empty());
}

#[test]
fn unknown_augroup_is_rejected() {
    assert_eq!(
        Autocmds::new().clear_group(AugroupId(44)),
        Err(AutocmdError::UnknownGroup(AugroupId(44)))
    );
}

#[test]
fn empty_autocmd_pattern_is_rejected() {
    assert_eq!(
        Autocmds::new().register(
            Event::BufEnter,
            "",
            ex("echo"),
            AutocmdOptions::default()
        ),
        Err(AutocmdError::EmptyPattern)
    );
}

#[test]
fn abuf_requires_registration_buffer() {
    assert_eq!(
        Autocmds::new().register(
            Event::BufEnter,
            "<abuf>",
            ex("echo"),
            AutocmdOptions::default()
        ),
        Err(AutocmdError::MissingBuffer)
    );
}

#[test]
fn abuf_matches_only_selected_buffer() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::BufEnter,
            "<abuf>",
            ex("echo local"),
            AutocmdOptions {
                buffer: Some(buffer(3)),
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    assert_eq!(
        autocmds
            .plan(Event::BufEnter, context(Some(buffer(3)), Some("x")))
            .ready
            .len(),
        1
    );
    assert!(
        autocmds
            .plan(Event::BufEnter, context(Some(buffer(4)), Some("x")))
            .ready
            .is_empty()
    );
}

#[test]
fn comma_pattern_list_registers_in_source_order() {
    let mut autocmds = Autocmds::new();
    let ids = autocmds
        .register(
            Event::BufReadPost,
            "*.rs,*.lua",
            ex("echo"),
            AutocmdOptions::default(),
        )
        .unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids[0] < ids[1]);
}

#[test]
fn escaped_comma_is_literal_pattern_text() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::BufReadPost,
            r"foo\,bar",
            ex("echo"),
            AutocmdOptions::default(),
        )
        .unwrap();
    assert_eq!(
        autocmds
            .plan(Event::BufReadPost, context(None, Some("foo,bar")))
            .ready
            .len(),
        1
    );
}

#[test]
fn star_pattern_matches_filename_tail() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::BufReadPost,
            "*.rs",
            ex("echo"),
            AutocmdOptions::default(),
        )
        .unwrap();
    assert_eq!(
        autocmds
            .plan(Event::BufReadPost, context(None, Some("src/main.rs")))
            .ready
            .len(),
        1
    );
}

#[test]
fn question_pattern_matches_one_character() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::BufReadPost,
            "file?.c",
            ex("echo"),
            AutocmdOptions::default(),
        )
        .unwrap();
    assert_eq!(
        autocmds
            .plan(Event::BufReadPost, context(None, Some("file1.c")))
            .ready
            .len(),
        1
    );
    assert!(
        autocmds
            .plan(Event::BufReadPost, context(None, Some("file10.c")))
            .ready
            .is_empty()
    );
}

#[test]
fn brace_pattern_expands_alternatives() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::BufReadPost,
            "*.{c,h}",
            ex("echo"),
            AutocmdOptions::default(),
        )
        .unwrap();
    assert_eq!(
        autocmds
            .plan(Event::BufReadPost, context(None, Some("main.h")))
            .ready
            .len(),
        1
    );
}

#[test]
fn slash_pattern_matches_full_path_not_tail() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::BufReadPost,
            "src/*.rs",
            ex("echo"),
            AutocmdOptions::default(),
        )
        .unwrap();
    assert_eq!(
        autocmds
            .plan(Event::BufReadPost, context(None, Some("src/main.rs")))
            .ready
            .len(),
        1
    );
    assert!(
        autocmds
            .plan(Event::BufReadPost, context(None, Some("other/main.rs")))
            .ready
            .is_empty()
    );
}

#[test]
fn definitions_fire_in_registration_order_regardless_of_group() {
    let mut autocmds = Autocmds::new();
    let first = autocmds.create_group("first", false).unwrap();
    let second = autocmds.create_group("second", false).unwrap();
    for (group, text) in [(second, "second"), (first, "first-a"), (first, "first-b")] {
        autocmds
            .register(
                Event::BufEnter,
                "*",
                ex(text),
                AutocmdOptions {
                    group,
                    ..AutocmdOptions::default()
                },
            )
            .unwrap();
    }
    let plan = autocmds.plan(Event::BufEnter, context(None, Some("x")));
    let values: Vec<_> = plan
        .ready
        .iter()
        .map(|action| match &action.kind {
            AutocmdKind::ExString(value) => value.as_str(),
            AutocmdKind::LuaCallback(_) => "lua",
        })
        .collect();
    // augroups filter but never reorder (autocmd.c:80-83): firing is global
    // definition order, so the first-registered (group "second") leads.
    assert_eq!(values, ["second", "first-a", "first-b"]);
}

#[test]
fn definitions_fire_in_registration_order_across_groups() {
    let mut autocmds = Autocmds::new();
    let named = autocmds.create_group("named", false).unwrap();
    autocmds
        .register(
            Event::BufEnter,
            "*",
            ex("named"),
            AutocmdOptions {
                group: named,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    autocmds
        .register(
            Event::BufEnter,
            "*",
            ex("default"),
            AutocmdOptions::default(),
        )
        .unwrap();
    // Definition order, not group, determines firing: "named" was registered
    // first even though the default group sorts low.
    assert_eq!(
        autocmds.plan(Event::BufEnter, context(None, Some("x"))).ready[0].group,
        named
    );
}

#[test]
fn plan_then_abandon_keeps_once_definition() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::BufEnter,
            "*",
            ex("once"),
            AutocmdOptions {
                once: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    let plan = autocmds.plan(Event::BufEnter, context(None, Some("x")));
    assert_eq!(plan.ready.len(), 1);
    // The plan is abandoned (never executed): a later plan still sees it.
    assert_eq!(autocmds.plan(Event::BufEnter, context(None, Some("x"))).ready.len(), 1);
    assert_eq!(autocmds.len(), 1);
}

#[test]
fn executed_once_definition_is_consumed_at_execution() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::BufEnter,
            "*",
            ex("once"),
            AutocmdOptions {
                once: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    let plan = autocmds.plan(Event::BufEnter, context(None, Some("x")));
    let action_id = plan.ready[0].id;
    // The host acknowledges execution, consuming the definition.
    assert!(autocmds.consume_once(action_id));
    assert!(autocmds.plan(Event::BufEnter, context(None, Some("x"))).ready.is_empty());
    assert!(autocmds.is_empty());
    // A second, unrelated id removes nothing.
    assert!(!autocmds.consume_once(action_id));
}

#[test]
fn consume_once_ignores_non_once_definitions() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::BufEnter,
            "*",
            ex("keep"),
            AutocmdOptions::default(),
        )
        .unwrap();
    let plan = autocmds.plan(Event::BufEnter, context(None, Some("x")));
    assert!(!autocmds.consume_once(plan.ready[0].id));
    assert_eq!(autocmds.len(), 1);
}

#[test]
fn non_nested_outer_suppresses_whole_nested_event() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::User,
            "*",
            ex("late"),
            AutocmdOptions::default(),
        )
        .unwrap();
    autocmds
        .register(
            Event::User,
            "*",
            ex("now"),
            AutocmdOptions {
                nested: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    // The outer autocmd is not ++nested: the whole nested event is suppressed
    // (autocmd.c:1465-1468), regardless of any candidate's own nested flag.
    let plan = autocmds.plan(
        Event::User,
        AutocmdContext {
            buffer: None,
            file_name: Some("x"),
            nested: false,
        },
    );
    assert!(plan.ready.is_empty());
}

#[test]
fn nested_outer_plans_all_matching_inner_actions() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(
            Event::User,
            "*",
            ex("plain"),
            AutocmdOptions::default(),
        )
        .unwrap();
    autocmds
        .register(
            Event::User,
            "*",
            ex("nested"),
            AutocmdOptions {
                nested: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    // The outer autocmd is ++nested: every matching inner action plans
    // normally. Candidate flags never partition the event (autocmd.c:2000-2002).
    let plan = autocmds.plan(
        Event::User,
        AutocmdContext {
            buffer: None,
            file_name: Some("x"),
            nested: true,
        },
    );
    let values: Vec<_> = plan
        .ready
        .iter()
        .map(|action| match &action.kind {
            AutocmdKind::ExString(value) => value.as_str(),
            AutocmdKind::LuaCallback(_) => "lua",
        })
        .collect();
    assert_eq!(values, ["plain", "nested"]);
}

#[test]
fn eventignore_suppresses_planning() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(Event::BufEnter, "*", ex("echo"), AutocmdOptions::default())
        .unwrap();
    autocmds.ignore(Event::BufEnter);
    assert!(autocmds.plan(Event::BufEnter, context(None, Some("x"))).ready.is_empty());
}

#[test]
fn event_unignore_restores_planning() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register(Event::BufEnter, "*", ex("echo"), AutocmdOptions::default())
        .unwrap();
    autocmds.ignore(Event::BufEnter);
    autocmds.unignore(Event::BufEnter);
    assert_eq!(autocmds.plan(Event::BufEnter, context(None, Some("x"))).ready.len(), 1);
}

#[test]
fn delete_by_event_preserves_other_events() {
    let mut autocmds = Autocmds::new();
    autocmds.register(Event::BufEnter, "*", ex("a"), AutocmdOptions::default()).unwrap();
    autocmds.register(Event::BufLeave, "*", ex("b"), AutocmdOptions::default()).unwrap();
    assert_eq!(
        autocmds.delete(DeleteAutocmds {
            event: Some(Event::BufEnter),
            ..DeleteAutocmds::default()
        }).unwrap(),
        1
    );
    assert_eq!(autocmds.len(), 1);
}

#[test]
fn delete_pattern_list_removes_exact_patterns() {
    let mut autocmds = Autocmds::new();
    autocmds.register(Event::BufEnter, "a,b,c", ex("x"), AutocmdOptions::default()).unwrap();
    assert_eq!(
        autocmds.delete(DeleteAutocmds {
            event: Some(Event::BufEnter),
            pattern: Some("a,c"),
            ..DeleteAutocmds::default()
        }).unwrap(),
        2
    );
    assert_eq!(autocmds.len(), 1);
}

#[test]
fn augroup_clear_preserves_group_identity() {
    let mut autocmds = Autocmds::new();
    let group = autocmds.create_group("keep", false).unwrap();
    autocmds.register(Event::BufEnter, "*", ex("x"), AutocmdOptions { group, ..AutocmdOptions::default() }).unwrap();
    assert_eq!(autocmds.clear_group(group).unwrap(), 1);
    assert_eq!(autocmds.group("keep"), Some(group));
}

#[test]
fn action_preserves_callback_description_and_group_name() {
    let mut autocmds = Autocmds::new();
    let group = autocmds.create_group("api", false).unwrap();
    autocmds.register(
        Event::User,
        "Build",
        AutocmdKind::LuaCallback(91),
        AutocmdOptions {
            group,
            description: Some("build callback".to_owned()),
            ..AutocmdOptions::default()
        },
    ).unwrap();
    let action = &autocmds.plan(Event::User, context(None, Some("Build"))).ready[0];
    assert_eq!(action.kind, AutocmdKind::LuaCallback(91));
    assert_eq!(action.group_name.as_deref(), Some("api"));
    assert_eq!(action.description.as_deref(), Some("build callback"));
}

// Typeahead tests cite keycodes.h:15-20,32-45,70-89 and input.c:922-1027.

#[test]
fn keys_encode_plain_ascii_without_expansion() {
    assert_eq!(Keys::encode(b"abc").as_bytes(), b"abc");
}

#[test]
fn keys_quote_zero_byte() {
    assert_eq!(Keys::encode(&[0]).as_bytes(), [K_SPECIAL, KS_ZERO, KE_FILLER]);
}

#[test]
fn keys_quote_literal_special_marker() {
    assert_eq!(Keys::encode(&[K_SPECIAL]).as_bytes(), [K_SPECIAL, KS_SPECIAL, KE_FILLER]);
}

#[test]
fn keys_mixed_round_trip_preserves_bytes() {
    assert_eq!(
        Keys::encode(&[b'a', 0, K_SPECIAL, b'z']).decode().unwrap(),
        [Key::Byte(b'a'), Key::Byte(0), Key::Byte(K_SPECIAL), Key::Byte(b'z')]
    );
}

#[test]
fn encoded_named_special_key_decodes_as_special() {
    let keys = Keys::special(KS_EXTRA, 7).unwrap();
    assert_eq!(keys.decode().unwrap(), [Key::Special(KS_EXTRA, 7)]);
}

#[test]
fn special_key_rejects_third_byte_below_range() {
    assert_eq!(Keys::special(KS_EXTRA, 1), Err(KeyDecodeError::InvalidThirdByte(1)));
}

#[test]
fn special_key_rejects_third_byte_above_range() {
    assert_eq!(Keys::special(KS_EXTRA, 0x80), Err(KeyDecodeError::InvalidThirdByte(0x80)));
}

#[test]
fn encoded_key_rejects_one_byte_truncation() {
    assert_eq!(Keys::from_encoded(vec![K_SPECIAL]), Err(KeyDecodeError::Truncated(0)));
}

#[test]
fn encoded_key_rejects_two_byte_truncation() {
    assert_eq!(Keys::from_encoded(vec![K_SPECIAL, KS_EXTRA]), Err(KeyDecodeError::Truncated(0)));
}

#[test]
fn encoded_key_rejects_invalid_third_byte() {
    assert_eq!(
        Keys::from_encoded(vec![K_SPECIAL, KS_EXTRA, 1]),
        Err(KeyDecodeError::InvalidThirdByte(1))
    );
}

#[test]
fn encoded_literal_rejects_invalid_filler() {
    assert_eq!(
        Keys::from_encoded(vec![K_SPECIAL, KS_ZERO, b'Q']),
        Err(KeyDecodeError::InvalidFiller(b'Q'))
    );
}

#[test]
fn typeahead_push_zero_inserts_at_front() {
    let mut input = Typeahead::new();
    input.append(&keys("tail"), TypeaheadFlags::default());
    input.push(&keys("head"), 0, TypeaheadFlags::default()).unwrap();
    assert_eq!(input.as_bytes(), b"headtail");
}

#[test]
fn typeahead_push_inserts_at_middle_offset() {
    let mut input = Typeahead::new();
    input.append(&keys("ac"), TypeaheadFlags::default());
    input.push(&keys("b"), 1, TypeaheadFlags::default()).unwrap();
    assert_eq!(input.as_bytes(), b"abc");
}

#[test]
fn typeahead_push_inserts_at_end_offset() {
    let mut input = Typeahead::new();
    input.append(&keys("a"), TypeaheadFlags::default());
    input.push(&keys("b"), 1, TypeaheadFlags::default()).unwrap();
    assert_eq!(input.as_bytes(), b"ab");
}

#[test]
fn typeahead_push_rejects_out_of_range_offset() {
    let mut input = Typeahead::new();
    assert_eq!(
        input.push(&keys("x"), 1, TypeaheadFlags::default()),
        Err(TypeaheadError::OffsetOutOfRange { offset: 1, len: 0 })
    );
}

#[test]
fn typeahead_keylen_returns_available_prefix() {
    let mut input = Typeahead::new();
    input.append(&keys("abcd"), TypeaheadFlags::default());
    assert_eq!(input.keylen(2), b"ab");
    assert_eq!(input.keylen(20), b"abcd");
}

#[test]
fn typeahead_preserves_front_flags() {
    let mut input = Typeahead::new();
    let flags = TypeaheadFlags {
        remap: Remap::No,
        modes: MapMode::Insert.into(),
        buffer: Some(buffer(2)),
        mapped: true,
        silent: true,
    };
    input.append(&keys("x"), flags);
    assert_eq!(input.front_flags(), Some(flags));
}

#[test]
fn typeahead_peek_does_not_consume() {
    let mut input = Typeahead::new();
    input.append(&keys("x"), TypeaheadFlags::default());
    assert_eq!(input.peek().unwrap(), Some(Key::Byte(b'x')));
    assert_eq!(input.len(), 1);
}

#[test]
fn typeahead_peek_decodes_special_key_atomically() {
    let mut input = Typeahead::new();
    input.append(&Keys::special(KS_EXTRA, 9).unwrap(), TypeaheadFlags::default());
    assert_eq!(input.peek().unwrap(), Some(Key::Special(KS_EXTRA, 9)));
    assert_eq!(input.len(), 3);
}

#[test]
fn typeahead_pop_consumes_one_logical_key() {
    let mut input = Typeahead::new();
    input.append(&Keys::encode(&[0, b'x']), TypeaheadFlags::default());
    assert_eq!(input.pop().unwrap(), Some(Key::Byte(0)));
    assert_eq!(input.as_bytes(), b"x");
}

#[test]
fn typeahead_consume_is_bounded_by_length() {
    let mut input = Typeahead::new();
    input.append(&keys("xy"), TypeaheadFlags::default());
    assert_eq!(input.consume(8), 2);
    assert!(input.is_empty());
}

#[test]
fn typeahead_flush_clears_bytes_and_metadata() {
    let mut input = Typeahead::new();
    input.append(&keys("xy"), TypeaheadFlags::default());
    input.flush();
    assert!(input.is_empty());
    assert_eq!(input.front_flags(), None);
}

#[test]
fn empty_typeahead_peek_and_pop_return_none() {
    let mut input = Typeahead::new();
    assert_eq!(input.peek().unwrap(), None);
    assert_eq!(input.pop().unwrap(), None);
}

// Mapping tests cite input.c:2319-2438 and mapping.c:502-909,1026-1083,
// 1455-1622; test/old/testdir/test_mapping.vim and test/functional/vimscript/map_spec.lua.

#[test]
fn every_map_mode_has_a_distinct_bit() {
    let modes = [
        MapMode::Normal,
        MapMode::Visual,
        MapMode::Select,
        MapMode::OperatorPending,
        MapMode::Insert,
        MapMode::CommandLine,
        MapMode::LangArg,
        MapMode::Terminal,
    ];
    for (index, mode) in modes.iter().enumerate() {
        for other in modes.iter().skip(index + 1) {
            assert!(!MapModes::one(*mode).intersects(MapModes::one(*other)));
        }
    }
}

#[test]
fn map_and_map_bang_mode_sets_match_command_families() {
    assert!(MapModes::MAP.contains(MapMode::Normal));
    assert!(MapModes::MAP.contains(MapMode::Visual));
    assert!(MapModes::MAP_BANG.contains(MapMode::Insert));
    assert!(MapModes::MAP_BANG.contains(MapMode::CommandLine));
    assert!(!MapModes::MAP.contains(MapMode::Insert));
}

#[test]
fn mapping_rhs_parses_nop_case_insensitively() {
    assert_eq!(MappingAction::parse_rhs("<NoP>").unwrap(), MappingAction::Nop);
}

#[test]
fn mapping_rhs_encodes_plain_keys() {
    assert_eq!(
        MappingAction::parse_rhs("abc").unwrap(),
        MappingAction::Keys(keys("abc"))
    );
}

#[test]
fn mapping_rhs_parses_cmd_form_with_ex_parser() {
    let MappingAction::ExCommands(commands) = MappingAction::parse_rhs("<Cmd>echo hi<CR>").unwrap() else {
        panic!("expected parsed Ex commands");
    };
    assert_eq!(commands.len(), 1);
}

#[test]
fn mapping_rhs_parses_colon_command_form() {
    let MappingAction::ExCommands(commands) = MappingAction::parse_rhs(":echo hi<CR>").unwrap() else {
        panic!("expected parsed Ex commands");
    };
    assert_eq!(commands.len(), 1);
}

#[test]
fn mapping_rhs_rejects_unknown_ex_command() {
    assert!(matches!(
        MappingAction::parse_rhs("<Cmd>definitelynotacommand<CR>"),
        Err(MappingError::ExCommand(_))
    ));
}

#[test]
fn map_rejects_empty_lhs() {
    assert!(matches!(
        Mappings::new().map(Keys::default(), MappingAction::Nop, MappingOptions::default()),
        Err(MappingError::EmptyLhs)
    ));
}

#[test]
fn map_rejects_empty_modes() {
    assert!(matches!(
        Mappings::new().map(
            keys("x"),
            MappingAction::Nop,
            MappingOptions { modes: MapModes::NONE, ..MappingOptions::default() }
        ),
        Err(MappingError::EmptyModes)
    ));
}

#[test]
fn exact_mapping_lookup_returns_consumed_length() {
    let mut mappings = Mappings::new();
    mappings.map(keys("aa"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    assert!(matches!(
        mappings.lookup(b"aa", MapMode::Normal, None),
        Lookup::Exact(_, 2)
    ));
}

#[test]
fn unrelated_mapping_lookup_returns_none() {
    let mut mappings = Mappings::new();
    mappings.map(keys("aa"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    assert_eq!(mappings.lookup(b"z", MapMode::Normal, None), Lookup::None);
}

#[test]
fn proper_prefix_waits_for_more_input() {
    let mut mappings = Mappings::new();
    mappings.map(keys("abc"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    assert!(matches!(
        mappings.lookup(b"ab", MapMode::Normal, None),
        Lookup::Prefix(None)
    ));
}

#[test]
fn exact_match_waits_when_longer_candidate_exists() {
    let mut mappings = Mappings::new();
    mappings.map(keys("aa"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    mappings.map(keys("aaa"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    assert!(matches!(
        mappings.lookup(b"aa", MapMode::Normal, None),
        Lookup::Prefix(Some(_))
    ));
}

#[test]
fn nowait_exact_match_wins_over_longer_candidate() {
    let mut mappings = Mappings::new();
    let mut options = map_options(MapMode::Normal);
    options.nowait = true;
    mappings.map(keys("aa"), MappingAction::Nop, options).unwrap();
    mappings.map(keys("aaa"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    assert!(matches!(
        mappings.lookup(b"aa", MapMode::Normal, None),
        Lookup::Exact(_, 2)
    ));
}

#[test]
fn longest_complete_lhs_wins_with_extra_typeahead() {
    let mut mappings = Mappings::new();
    mappings.map(keys("a"), MappingAction::Callback(1), map_options(MapMode::Normal)).unwrap();
    mappings.map(keys("ab"), MappingAction::Callback(2), map_options(MapMode::Normal)).unwrap();
    let Lookup::Exact(mapping, length) = mappings.lookup(b"abc", MapMode::Normal, None) else {
        panic!("expected exact mapping");
    };
    assert_eq!(length, 2);
    assert_eq!(mapping.action, MappingAction::Callback(2));
}

#[test]
fn buffer_local_mapping_precedes_global_mapping() {
    let mut mappings = Mappings::new();
    mappings.map(keys("x"), MappingAction::Callback(1), map_options(MapMode::Normal)).unwrap();
    let mut local = map_options(MapMode::Normal);
    local.scope = MapScope::Buffer(buffer(4));
    mappings.map(keys("x"), MappingAction::Callback(2), local).unwrap();
    let Lookup::Exact(mapping, _) = mappings.lookup(b"x", MapMode::Normal, Some(buffer(4))) else {
        panic!("expected local mapping");
    };
    assert_eq!(mapping.action, MappingAction::Callback(2));
}

#[test]
fn global_mapping_is_fallback_when_local_does_not_match() {
    let mut mappings = Mappings::new();
    mappings.map(keys("x"), MappingAction::Callback(1), map_options(MapMode::Normal)).unwrap();
    let mut local = map_options(MapMode::Normal);
    local.scope = MapScope::Buffer(buffer(4));
    mappings.map(keys("y"), MappingAction::Callback(2), local).unwrap();
    let Lookup::Exact(mapping, _) = mappings.lookup(b"x", MapMode::Normal, Some(buffer(4))) else {
        panic!("expected global mapping");
    };
    assert_eq!(mapping.action, MappingAction::Callback(1));
}

#[test]
fn mapping_lookup_filters_by_mode() {
    let mut mappings = Mappings::new();
    mappings.map(keys("x"), MappingAction::Nop, map_options(MapMode::Insert)).unwrap();
    assert_eq!(mappings.lookup(b"x", MapMode::Normal, None), Lookup::None);
    assert!(matches!(mappings.lookup(b"x", MapMode::Insert, None), Lookup::Exact(_, 1)));
}

#[test]
fn noremap_records_nonrecursive_policy() {
    let mut mappings = Mappings::new();
    mappings.noremap(keys("x"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    let Lookup::Exact(mapping, _) = mappings.lookup(b"x", MapMode::Normal, None) else {
        panic!("expected mapping");
    };
    assert!(!mapping.options.remap);
}

#[test]
fn map_records_recursive_policy_by_default() {
    let mut mappings = Mappings::new();
    mappings.map(keys("x"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    let Lookup::Exact(mapping, _) = mappings.lookup(b"x", MapMode::Normal, None) else {
        panic!("expected mapping");
    };
    assert!(mapping.options.remap);
}

#[test]
fn later_mapping_replaces_overlapping_mode_only() {
    let mut mappings = Mappings::new();
    let modes = MapMode::Normal | MapMode::Visual;
    mappings.map(keys("x"), MappingAction::Callback(1), MappingOptions { modes, ..MappingOptions::default() }).unwrap();
    mappings.map(keys("x"), MappingAction::Callback(2), map_options(MapMode::Normal)).unwrap();
    let Lookup::Exact(normal, _) = mappings.lookup(b"x", MapMode::Normal, None) else { panic!("normal"); };
    let Lookup::Exact(visual, _) = mappings.lookup(b"x", MapMode::Visual, None) else { panic!("visual"); };
    assert_eq!(normal.action, MappingAction::Callback(2));
    assert_eq!(visual.action, MappingAction::Callback(1));
}

#[test]
fn unmap_removes_selected_mode_and_preserves_other_mode() {
    let mut mappings = Mappings::new();
    mappings.map(keys("x"), MappingAction::Nop, MappingOptions {
        modes: MapMode::Normal | MapMode::Visual,
        ..MappingOptions::default()
    }).unwrap();
    assert_eq!(mappings.unmap(&keys("x"), MapMode::Normal.into(), MapScope::Global), 1);
    assert_eq!(mappings.lookup(b"x", MapMode::Normal, None), Lookup::None);
    assert!(matches!(mappings.lookup(b"x", MapMode::Visual, None), Lookup::Exact(_, 1)));
}

#[test]
fn mapclear_affects_only_selected_scope() {
    let mut mappings = Mappings::new();
    mappings.map(keys("x"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    let mut local = map_options(MapMode::Normal);
    local.scope = MapScope::Buffer(buffer(2));
    mappings.map(keys("y"), MappingAction::Nop, local).unwrap();
    assert_eq!(mappings.mapclear(MapMode::Normal.into(), MapScope::Global), 1);
    assert_eq!(mappings.mapping_len(), 1);
}

#[test]
fn timeout_length_is_data_for_later_input_loop() {
    let mut mappings = Mappings::new();
    assert_eq!(mappings.timeout_len_ms(), 1_000);
    mappings.set_timeout_len_ms(250);
    assert_eq!(mappings.timeout_len_ms(), 250);
}

#[test]
fn lookup_typeahead_uses_stack_bytes() {
    let mut mappings = Mappings::new();
    mappings.map(keys("xy"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    let mut input = Typeahead::new();
    input.append(&keys("xy"), TypeaheadFlags::default());
    assert!(matches!(mappings.lookup_typeahead(&input, MapMode::Normal, None), Lookup::Exact(_, 2)));
}

#[test]
fn abbreviation_rejects_empty_lhs() {
    assert!(matches!(
        Mappings::new().abbreviate("", MappingAction::Nop, MapScope::Global, true),
        Err(MappingError::InvalidAbbreviation(_))
    ));
}

#[test]
fn abbreviation_rejects_whitespace() {
    assert!(matches!(
        Mappings::new().abbreviate("two words", MappingAction::Nop, MapScope::Global, true),
        Err(MappingError::InvalidAbbreviation(_))
    ));
}

#[test]
fn abbreviation_rejects_mixed_keyword_classes() {
    assert!(matches!(
        Mappings::new().abbreviate("a-b", MappingAction::Nop, MapScope::Global, true),
        Err(MappingError::InvalidAbbreviation(_))
    ));
}

#[test]
fn vi_style_nonkeyword_prefix_abbreviation_is_valid() {
    let mut mappings = Mappings::new();
    mappings.abbreviate("#i", MappingAction::Nop, MapScope::Global, true).unwrap();
    assert!(mappings.lookup_abbreviation("value#i", ' ', None).is_some());
}

#[test]
fn nonkeyword_ending_abbreviation_requires_whitespace_boundary() {
    let mut mappings = Mappings::new();
    mappings.abbreviate("a-", MappingAction::Nop, MapScope::Global, true).unwrap();
    assert!(mappings.lookup_abbreviation(" a-", ' ', None).is_some());
    assert!(mappings.lookup_abbreviation("xa-", ' ', None).is_none());
}

#[test]
fn abbreviation_triggers_on_nonkeyword_delimiter() {
    let mut mappings = Mappings::new();
    mappings.abbreviate("teh", MappingAction::Keys(keys("the")), MapScope::Global, true).unwrap();
    assert_eq!(mappings.lookup_abbreviation("teh", ' ', None).unwrap().lhs, "teh");
}

#[test]
fn abbreviation_does_not_trigger_on_keyword_character() {
    let mut mappings = Mappings::new();
    mappings.abbreviate("teh", MappingAction::Nop, MapScope::Global, true).unwrap();
    assert!(mappings.lookup_abbreviation("teh", 'x', None).is_none());
}

#[test]
fn abbreviation_requires_start_of_word_boundary() {
    let mut mappings = Mappings::new();
    mappings.abbreviate("teh", MappingAction::Nop, MapScope::Global, true).unwrap();
    assert!(mappings.lookup_abbreviation("ateh", ' ', None).is_none());
}

#[test]
fn buffer_abbreviation_precedes_global_abbreviation() {
    let mut mappings = Mappings::new();
    mappings.abbreviate("teh", MappingAction::Callback(1), MapScope::Global, true).unwrap();
    mappings.abbreviate("teh", MappingAction::Callback(2), MapScope::Buffer(buffer(3)), true).unwrap();
    let found = mappings.lookup_abbreviation("teh", ' ', Some(buffer(3))).unwrap();
    assert_eq!(found.action, MappingAction::Callback(2));
}

#[test]
fn unabbreviate_removes_only_selected_scope() {
    let mut mappings = Mappings::new();
    mappings.abbreviate("teh", MappingAction::Nop, MapScope::Global, true).unwrap();
    mappings.abbreviate("teh", MappingAction::Nop, MapScope::Buffer(buffer(3)), true).unwrap();
    assert!(mappings.unabbreviate("teh", MapScope::Global));
    assert_eq!(mappings.abbreviation_len(), 1);
}

#[test]
fn abbrevclear_removes_one_scope() {
    let mut mappings = Mappings::new();
    mappings.abbreviate("teh", MappingAction::Nop, MapScope::Global, true).unwrap();
    mappings.abbreviate("recieve", MappingAction::Nop, MapScope::Buffer(buffer(3)), true).unwrap();
    assert_eq!(mappings.abbrevclear(MapScope::Global), 1);
    assert_eq!(mappings.abbreviation_len(), 1);
}

#[test]
fn remove_buffer_clears_local_maps_and_abbreviations() {
    let mut mappings = Mappings::new();
    let scope = MapScope::Buffer(buffer(5));
    mappings.map(keys("x"), MappingAction::Nop, MappingOptions {
        scope,
        modes: MapMode::Normal.into(),
        ..MappingOptions::default()
    }).unwrap();
    mappings.abbreviate("teh", MappingAction::Nop, scope, true).unwrap();
    mappings.remove_buffer(buffer(5));
    assert_eq!(mappings.mapping_len(), 0);
    assert_eq!(mappings.abbreviation_len(), 0);
}

// Editor integration covers the real 9a wipe path.

#[test]
fn editor_exposes_owned_input_subsystems() {
    let editor = Editor::new();
    assert!(editor.autocmds().is_empty());
    assert_eq!(editor.mappings().mapping_len(), 0);
    assert!(editor.typeahead().is_empty());
}

#[test]
fn editor_wipe_removes_buffer_local_autocmds_and_mappings() {
    let mut editor = Editor::new();
    let handle = editor.create_buffer(true).unwrap();
    editor.autocmds_mut().register(
        Event::BufEnter,
        "<abuf>",
        ex("local"),
        AutocmdOptions { buffer: Some(handle), ..AutocmdOptions::default() },
    ).unwrap();
    editor.mappings_mut().map(
        keys("x"),
        MappingAction::Nop,
        MappingOptions {
            scope: MapScope::Buffer(handle),
            modes: MapMode::Normal.into(),
            ..MappingOptions::default()
        },
    ).unwrap();
    editor.wipe_buffer(handle).unwrap();
    assert!(editor.autocmds().is_empty());
    assert_eq!(editor.mappings().mapping_len(), 0);
}

#[test]
fn editor_wipe_preserves_global_and_other_buffer_state() {
    let mut editor = Editor::new();
    let wiped = editor.create_buffer(true).unwrap();
    let kept = editor.create_buffer(true).unwrap();
    editor.mappings_mut().map(keys("g"), MappingAction::Nop, map_options(MapMode::Normal)).unwrap();
    editor.mappings_mut().map(
        keys("k"),
        MappingAction::Nop,
        MappingOptions {
            scope: MapScope::Buffer(kept),
            modes: MapMode::Normal.into(),
            ..MappingOptions::default()
        },
    ).unwrap();
    editor.wipe_buffer(wiped).unwrap();
    assert_eq!(editor.mappings().mapping_len(), 2);
    assert!(matches!(editor.mappings().lookup(b"g", MapMode::Normal, None), Lookup::Exact(_, 1)));
    assert!(matches!(editor.mappings().lookup(b"k", MapMode::Normal, Some(kept)), Lookup::Exact(_, 1)));
}
