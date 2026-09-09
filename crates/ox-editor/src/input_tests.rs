//! Behavioral input tests extracted from upstream autocmd, input, and mapping tests.

use std::collections::BTreeSet;

use ox_types::BufHandle;

use crate::Editor;
use crate::autocmd::{
    AugroupId, AutocmdContext, AutocmdError, AutocmdFilter, AutocmdKind, AutocmdOptions, Autocmds,
    EVENT_COUNT, Event, PatternKind,
};
use crate::mapping::{
    Lookup, MapMode, MapModes, MapScope, MappingAction, MappingError, MappingOptions, Mappings,
};
use crate::options::OptionValue;
use crate::typeahead::{
    K_SPECIAL, KE_FILLER, KS_EXTRA, KS_SPECIAL, KS_ZERO, Key, KeyDecodeError, Keys, Remap,
    Typeahead, TypeaheadError, TypeaheadFlags,
};

fn buffer(value: i64) -> BufHandle {
    BufHandle::try_from(value).unwrap()
}

fn ex(value: &str) -> AutocmdKind {
    AutocmdKind::ExString(value.to_owned())
}

fn context(buffer: Option<BufHandle>, file_name: Option<&str>) -> AutocmdContext<'_> {
    AutocmdContext {
        buffer,
        file_name: file_name.map(str::as_bytes),
        match_name: None,
        // A top-level event is not raised inside a non-nested outer autocmd,
        // so nesting is permitted and no event-level gate applies.
        nested: true,
        data: None,
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
    assert_eq!(
        Event::from_name("FileEncoding"),
        Some(Event::EncodingChanged)
    );
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
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("echo gone"),
            &AutocmdOptions {
                group,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    let removed = autocmds.delete_group(group).unwrap();
    assert_eq!(removed.len(), 1);
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
fn autocmd_patterns_ignore_empty_items_but_require_one_pattern() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "hi,,bye,",
            &ex("echo"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    assert_eq!(autocmds.len(), 2);
    assert_eq!(
        autocmds.register_legacy(
            &[Event::BufEnter],
            ",,",
            &ex("echo"),
            &AutocmdOptions::default()
        ),
        Err(AutocmdError::EmptyPattern)
    );
}

#[test]
fn empty_event_list_is_rejected() {
    assert_eq!(
        Autocmds::new().register_api(&[], "*", &ex("echo"), &AutocmdOptions::default()),
        Err(AutocmdError::EmptyEvent)
    );
    assert_eq!(
        Autocmds::new().register_legacy(&[], "*", &ex("echo"), &AutocmdOptions::default()),
        Err(AutocmdError::EmptyEvent)
    );
}

#[test]
fn abuf_requires_registration_buffer() {
    assert_eq!(
        Autocmds::new().register_legacy(
            &[Event::BufEnter],
            "<abuf>",
            &ex("echo"),
            &AutocmdOptions::default()
        ),
        Err(AutocmdError::MissingBuffer)
    );
}

#[test]
fn abuf_matches_only_selected_buffer() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "<abuf>",
            &ex("echo local"),
            &AutocmdOptions {
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
    let api_id = autocmds
        .register_api(
            &[Event::BufReadPost],
            "*.rs,*.lua",
            &ex("echo"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    // One API call shares one API id across all event × pattern entries.
    let defs = autocmds.definitions();
    assert_eq!(defs.len(), 2);
    assert!(defs[0].entry_id < defs[1].entry_id);
    assert_eq!(defs[0].api_id, Some(api_id));
    assert_eq!(defs[1].api_id, Some(api_id));
}

#[test]
fn api_id_is_shared_across_events_and_patterns() {
    let mut autocmds = Autocmds::new();
    let api_id = autocmds
        .register_api(
            &[Event::BufEnter, Event::BufLeave],
            "*.rs,*.lua",
            &ex("echo"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let defs = autocmds.definitions();
    assert_eq!(defs.len(), 4);
    for def in &defs {
        assert_eq!(def.api_id, Some(api_id));
    }
    // Deleting by API id removes every sibling.
    let removed = autocmds.delete_api_id(api_id);
    assert_eq!(removed.len(), 4);
    assert!(autocmds.is_empty());
}

#[test]
fn legacy_entries_have_no_api_id() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("legacy"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let def = &autocmds.definitions()[0];
    assert_eq!(def.api_id, None);
    let action = &autocmds
        .plan(Event::BufEnter, context(None, Some("x")))
        .ready[0];
    assert_eq!(action.api_id, None);
}

#[test]
fn escaped_comma_is_literal_pattern_text() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufReadPost],
            r"foo\,bar",
            &ex("echo"),
            &AutocmdOptions::default(),
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
        .register_legacy(
            &[Event::BufReadPost],
            "*.rs",
            &ex("echo"),
            &AutocmdOptions::default(),
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
fn explicit_match_name_preserves_event_and_file_context() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::FileType],
            "python",
            &ex("echo"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let target = buffer(7);
    let plan = autocmds.plan(
        Event::FileType,
        AutocmdContext {
            buffer: Some(target),
            file_name: Some("src/main.py".as_bytes()),
            match_name: Some(b"python"),
            nested: true,
            data: None,
        },
    );
    let action = &plan.ready[0];
    assert_eq!(action.match_name, ox_types::OxStr::from("python"));
    assert_eq!(action.file_name, ox_types::OxStr::from("src/main.py"));
    assert_eq!(action.buffer, Some(target));
}

#[test]
fn question_pattern_matches_one_character() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufReadPost],
            "file?.c",
            &ex("echo"),
            &AutocmdOptions::default(),
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
        .register_legacy(
            &[Event::BufReadPost],
            "*.{c,h}",
            &ex("echo"),
            &AutocmdOptions::default(),
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
        .register_legacy(
            &[Event::BufReadPost],
            "src/*.rs",
            &ex("echo"),
            &AutocmdOptions::default(),
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
            .register_legacy(
                &[Event::BufEnter],
                "*",
                &ex(text),
                &AutocmdOptions {
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
            AutocmdKind::VimscriptFunction(_) => "vimscript",
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
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("named"),
            &AutocmdOptions {
                group: named,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("default"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    // Definition order, not group, determines firing: "named" was registered
    // first even though the default group sorts low.
    assert_eq!(
        autocmds
            .plan(Event::BufEnter, context(None, Some("x")))
            .ready[0]
            .group,
        named
    );
}

#[test]
fn plan_then_abandon_keeps_once_definition() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("once"),
            &AutocmdOptions {
                once: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    let plan = autocmds.plan(Event::BufEnter, context(None, Some("x")));
    assert_eq!(plan.ready.len(), 1);
    // The plan is abandoned (never executed): a later plan still sees it.
    assert_eq!(
        autocmds
            .plan(Event::BufEnter, context(None, Some("x")))
            .ready
            .len(),
        1
    );
    assert_eq!(autocmds.len(), 1);
}

#[test]
fn executed_once_definition_is_consumed_at_execution() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("once"),
            &AutocmdOptions {
                once: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    let plan = autocmds.plan(Event::BufEnter, context(None, Some("x")));
    let entry_id = plan.ready[0].entry_id;
    // The host acknowledges execution, consuming the definition.
    let removed = autocmds.consume_once(entry_id);
    assert!(removed.is_some());
    assert!(
        autocmds
            .plan(Event::BufEnter, context(None, Some("x")))
            .ready
            .is_empty()
    );
    assert!(autocmds.is_empty());
    // A second call with the same id removes nothing.
    assert!(autocmds.consume_once(entry_id).is_none());
}

#[test]
fn consume_once_ignores_non_once_definitions() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("keep"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let plan = autocmds.plan(Event::BufEnter, context(None, Some("x")));
    assert!(autocmds.consume_once(plan.ready[0].entry_id).is_none());
    assert_eq!(autocmds.len(), 1);
}

#[test]
fn once_consumes_only_one_entry_not_api_siblings() {
    let mut autocmds = Autocmds::new();
    let api_id = autocmds
        .register_api(
            &[Event::BufEnter],
            "*.rs,*.lua",
            &ex("once"),
            &AutocmdOptions {
                once: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    // Two entries share one API id; consuming one does not remove the other.
    let plan = autocmds.plan(Event::BufEnter, context(None, Some("x.rs")));
    assert_eq!(plan.ready.len(), 1);
    let removed = autocmds.consume_once(plan.ready[0].entry_id);
    assert!(removed.is_some());
    assert_eq!(autocmds.len(), 1);
    // The sibling still has the same API id.
    let remaining = &autocmds.definitions()[0];
    assert_eq!(remaining.api_id, Some(api_id));
}

#[test]
fn non_nested_outer_suppresses_whole_nested_event() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(&[Event::User], "*", &ex("late"), &AutocmdOptions::default())
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::User],
            "*",
            &ex("now"),
            &AutocmdOptions {
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
            file_name: Some(b"x"),
            match_name: None,
            nested: false,
            data: None,
        },
    );
    assert!(plan.ready.is_empty());
}

#[test]
fn nested_outer_plans_all_matching_inner_actions() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::User],
            "*",
            &ex("plain"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::User],
            "*",
            &ex("nested"),
            &AutocmdOptions {
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
            file_name: Some(b"x"),
            match_name: None,
            nested: true,
            data: None,
        },
    );
    let values: Vec<_> = plan
        .ready
        .iter()
        .map(|action| match &action.kind {
            AutocmdKind::ExString(value) => value.as_str(),
            AutocmdKind::VimscriptFunction(_) => "vimscript",
            AutocmdKind::LuaCallback(_) => "lua",
        })
        .collect();
    assert_eq!(values, ["plain", "nested"]);
}

#[test]
fn default_context_plans_top_level_events() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("top-level"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let plan = autocmds.plan(Event::BufEnter, AutocmdContext::default());
    assert_eq!(plan.ready.len(), 1);
    assert_eq!(plan.ready[0].kind, ex("top-level"));
}

#[test]
fn eventignore_suppresses_planning() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("echo"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    autocmds.ignore(Event::BufEnter);
    assert!(
        autocmds
            .plan(Event::BufEnter, context(None, Some("x")))
            .ready
            .is_empty()
    );
}

#[test]
fn event_unignore_restores_planning() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("echo"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    autocmds.ignore(Event::BufEnter);
    autocmds.unignore(Event::BufEnter);
    assert_eq!(
        autocmds
            .plan(Event::BufEnter, context(None, Some("x")))
            .ready
            .len(),
        1
    );
}

#[test]
fn clear_by_event_preserves_other_events() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("a"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::BufLeave],
            "*",
            &ex("b"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let removed = autocmds.clear(&AutocmdFilter {
        events: Some(&[Event::BufEnter]),
        ..AutocmdFilter::default()
    });
    assert_eq!(removed.len(), 1);
    assert_eq!(autocmds.len(), 1);
}

#[test]
fn clear_by_pattern_list_removes_exact_patterns() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "a,b,c",
            &ex("x"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let patterns = vec!["a".to_owned(), "c".to_owned()];
    let removed = autocmds.clear(&AutocmdFilter {
        events: Some(&[Event::BufEnter]),
        patterns: Some(&patterns),
        ..AutocmdFilter::default()
    });
    assert_eq!(removed.len(), 2);
    assert_eq!(autocmds.len(), 1);
}

#[test]
fn filter_combines_or_within_and_across_classes() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "a",
            &ex("1"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "b",
            &ex("2"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::BufLeave],
            "a",
            &ex("3"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::BufLeave],
            "b",
            &ex("4"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    // events OR within, patterns OR within, AND across: BufEnter+b and BufLeave+a.
    let patterns = vec!["a".to_owned(), "b".to_owned()];
    let defs = autocmds.query(&AutocmdFilter {
        events: Some(&[Event::BufEnter, Event::BufLeave]),
        patterns: Some(&patterns),
        ..AutocmdFilter::default()
    });
    assert_eq!(defs.len(), 4);
    // Restrict to BufEnter only: a and b.
    let defs = autocmds.query(&AutocmdFilter {
        events: Some(&[Event::BufEnter]),
        patterns: Some(&patterns),
        ..AutocmdFilter::default()
    });
    assert_eq!(defs.len(), 2);
    // Restrict to pattern a only across both events: BufEnter+a and BufLeave+a.
    let pat_a = vec!["a".to_owned()];
    let defs = autocmds.query(&AutocmdFilter {
        events: Some(&[Event::BufEnter, Event::BufLeave]),
        patterns: Some(&pat_a),
        ..AutocmdFilter::default()
    });
    assert_eq!(defs.len(), 2);
    assert!(defs.iter().all(|d| d.pattern == "a"));
}

#[test]
fn filter_preserves_registration_order() {
    let mut autocmds = Autocmds::new();
    let group = autocmds.create_group("g", false).unwrap();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "z",
            &ex("1"),
            &AutocmdOptions {
                group,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "z",
            &ex("2"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "z",
            &ex("3"),
            &AutocmdOptions {
                group,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    let defs = autocmds.query(&AutocmdFilter {
        patterns: Some(&["z".to_owned()]),
        ..AutocmdFilter::default()
    });
    // Global registration order, not group-sorted.
    assert_eq!(defs.len(), 3);
    assert_eq!(defs[0].group, group);
    assert_eq!(defs[1].group, AugroupId::default());
    assert_eq!(defs[2].group, group);
}

#[test]
fn augroup_clear_preserves_group_identity() {
    let mut autocmds = Autocmds::new();
    let group = autocmds.create_group("keep", false).unwrap();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("x"),
            &AutocmdOptions {
                group,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    let removed = autocmds.clear_group(group).unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(autocmds.group("keep"), Some(group));
}

#[test]
fn action_preserves_callback_description_and_group_name() {
    let mut autocmds = Autocmds::new();
    let group = autocmds.create_group("api", false).unwrap();
    autocmds
        .register_api(
            &[Event::User],
            "Build",
            &AutocmdKind::LuaCallback(91),
            &AutocmdOptions {
                group,
                description: Some("build callback".to_owned()),
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    let action = &autocmds
        .plan(Event::User, context(None, Some("Build")))
        .ready[0];
    assert_eq!(action.kind, AutocmdKind::LuaCallback(91));
    assert_eq!(action.group_name.as_deref(), Some("api"));
    assert_eq!(action.description.as_deref(), Some("build callback"));
    assert!(action.api_id.is_some());
}

#[test]
fn canonical_buffer_pattern_serializes_buffer_n() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_api(
            &[Event::BufEnter],
            "<abuf>",
            &ex("echo"),
            &AutocmdOptions {
                buffer: Some(buffer(5)),
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    let def = &autocmds.definitions()[0];
    assert_eq!(def.pattern, "<buffer=5>");
    assert_eq!(def.buffer, Some(buffer(5)));

    // <buffer> and <buffer=0> also canonicalize.
    let mut autocmds2 = Autocmds::new();
    autocmds2
        .register_api(
            &[Event::BufEnter],
            "<buffer>",
            &ex("echo"),
            &AutocmdOptions {
                buffer: Some(buffer(7)),
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    assert_eq!(autocmds2.definitions()[0].pattern, "<buffer=7>");

    let mut autocmds3 = Autocmds::new();
    autocmds3
        .register_api(
            &[Event::BufEnter],
            "<buffer=0>",
            &ex("echo"),
            &AutocmdOptions {
                buffer: Some(buffer(9)),
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    assert_eq!(autocmds3.definitions()[0].pattern, "<buffer=9>");
}

#[test]
fn explicit_buffer_n_pattern_resolves_to_handle() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_api(
            &[Event::BufEnter],
            "<buffer=12>",
            &ex("echo"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let def = &autocmds.definitions()[0];
    assert_eq!(def.pattern, "<buffer=12>");
    assert_eq!(def.buffer, Some(buffer(12)));
    assert_eq!(
        autocmds
            .plan(Event::BufEnter, context(Some(buffer(12)), Some("x")))
            .ready
            .len(),
        1
    );
}

#[test]
fn invalid_buffer_pattern_is_rejected() {
    assert_eq!(
        Autocmds::new().register_api(
            &[Event::BufEnter],
            "<buffer=abc>",
            &ex("echo"),
            &AutocmdOptions::default(),
        ),
        Err(AutocmdError::InvalidBufferPattern(
            "<buffer=abc>".to_owned()
        ))
    );
}

#[test]
fn api_group_deletion_removes_entries() {
    let mut autocmds = Autocmds::new();
    let group = autocmds.create_group("apidel", false).unwrap();
    autocmds
        .register_api(
            &[Event::BufEnter],
            "*",
            &ex("api"),
            &AutocmdOptions {
                group,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    assert_eq!(autocmds.len(), 1);
    let removed = autocmds.delete_group(group).unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0], ex("api"));
    assert_eq!(autocmds.group("apidel"), None);
    assert!(autocmds.is_empty());
}

#[test]
fn legacy_group_deletion_tombstones_entries() {
    let mut autocmds = Autocmds::new();
    let group = autocmds.create_group("legacy", false).unwrap();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("legacy"),
            &AutocmdOptions {
                group,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    // Legacy deletion preserves entries under a tombstone.
    autocmds.delete_group_legacy(group).unwrap();
    assert_eq!(autocmds.group("legacy"), None);
    assert_eq!(autocmds.len(), 1);
    // The tombstoned entry is still globally queryable.
    let defs = autocmds.query(&AutocmdFilter::default());
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].group_name.as_deref(), Some("--Deleted--"));
    // Recreating the same name allocates a new live group id.
    let recreated = autocmds.create_group("legacy", false).unwrap();
    assert_ne!(recreated, group);
    // Old entries remain under the old tombstoned group.
    assert_eq!(autocmds.len(), 1);
    let old_def = &autocmds.query(&AutocmdFilter::default())[0];
    assert_eq!(old_def.group, group);
    assert_eq!(old_def.group_name.as_deref(), Some("--Deleted--"));
}

#[test]
fn first_user_group_id_is_two_and_popupmenu_is_one() {
    let mut autocmds = Autocmds::new();
    let user = autocmds.create_group("user", false).unwrap();
    assert_eq!(user, AugroupId(2));
    assert_eq!(autocmds.group("nvim.popupmenu"), Some(AugroupId(1)));
}

#[test]
fn group_allocation_remains_monotonic_after_core_group_deletion() {
    let mut autocmds = Autocmds::new();
    let first = autocmds.create_group("first", false).unwrap();
    assert_eq!(first, AugroupId(2));
    let second = autocmds.create_group("second", false).unwrap();
    assert_eq!(second, AugroupId(3));
    autocmds.delete_group(AugroupId(1)).unwrap();
    let third = autocmds.create_group("third", false).unwrap();
    assert_eq!(third, AugroupId(4));
}

#[test]
fn deleting_popupmenu_does_not_reuse_its_id() {
    let mut autocmds = Autocmds::new();
    autocmds.delete_group(AugroupId(1)).unwrap();
    let user = autocmds.create_group("user", false).unwrap();
    assert_eq!(user, AugroupId(2));
}

#[test]
fn delete_entry_returns_removed_payload() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("payload"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let entry_id = autocmds.definitions()[0].entry_id;
    let removed = autocmds.delete_entry(entry_id);
    assert_eq!(removed, Some(ex("payload")));
    assert!(autocmds.is_empty());
    // Deleting again returns None.
    assert!(autocmds.delete_entry(entry_id).is_none());
}

#[test]
fn is_entry_live_liveness_check() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("a"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let entry_id = autocmds.definitions()[0].entry_id;
    assert!(autocmds.is_entry_live(entry_id));
    // After deletion the entry is no longer live.
    autocmds.delete_entry(entry_id);
    assert!(!autocmds.is_entry_live(entry_id));
    // A clear also removes entries.
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("b"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let entry_id2 = autocmds.definitions()[0].entry_id;
    assert!(autocmds.is_entry_live(entry_id2));
    autocmds.clear(&AutocmdFilter::default());
    assert!(!autocmds.is_entry_live(entry_id2));
}

#[test]
fn register_is_atomic_on_malformed_pattern() {
    let mut autocmds = Autocmds::new();
    // A valid pattern followed by a malformed buffer pattern must fail
    // without appending any entry or consuming any counter.
    let entry_count_before = autocmds.len();
    let next_entry_before = autocmds.definitions().first().map_or(0, |d| d.entry_id.0);
    let result = autocmds.register_api(
        &[Event::BufEnter],
        "*.rs,<buffer=abc>",
        &ex("echo"),
        &AutocmdOptions::default(),
    );
    assert!(result.is_err());
    assert_eq!(autocmds.len(), entry_count_before);
    // No API id was consumed: the next successful registration gets id 1.
    let api_id = autocmds
        .register_api(
            &[Event::BufEnter],
            "*.rs",
            &ex("ok"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    assert_eq!(api_id, 1);
    // Entry ids also start fresh.
    let def = &autocmds.definitions()[0];
    assert_eq!(def.entry_id.0, next_entry_before + 1);
    assert_eq!(def.api_id, Some(1));
}

#[test]
fn register_legacy_is_atomic_on_malformed_pattern() {
    let mut autocmds = Autocmds::new();
    let before = autocmds.len();
    let result = autocmds.register_legacy(
        &[Event::BufEnter],
        "*.rs,<buffer=abc>",
        &ex("echo"),
        &AutocmdOptions::default(),
    );
    assert!(result.is_err());
    assert_eq!(autocmds.len(), before);
}

#[test]
fn remove_buffer_returns_removed_payloads() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "<abuf>",
            &ex("local"),
            &AutocmdOptions {
                buffer: Some(buffer(3)),
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("global"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let removed = autocmds.remove_buffer(buffer(3));
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0], ex("local"));
    assert_eq!(autocmds.len(), 1);
}

#[test]
fn filter_by_buffer_matches_only_buffer_local() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "<abuf>",
            &ex("local"),
            &AutocmdOptions {
                buffer: Some(buffer(3)),
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("global"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let bufs = [buffer(3)];
    let defs = autocmds.query(&AutocmdFilter {
        buffers: Some(&bufs),
        ..AutocmdFilter::default()
    });
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].pattern, "<buffer=3>");
}

#[test]
fn filter_by_api_id() {
    let mut autocmds = Autocmds::new();
    let api_id = autocmds
        .register_api(
            &[Event::BufEnter],
            "*",
            &ex("api"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    autocmds
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &ex("legacy"),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let defs = autocmds.query(&AutocmdFilter {
        api_id: Some(api_id),
        ..AutocmdFilter::default()
    });
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].api_id, Some(api_id));
}

// Typeahead tests cite keycodes.h:15-20,32-45,70-89 and input.c:922-1027.

#[test]
fn keys_encode_plain_ascii_without_expansion() {
    assert_eq!(Keys::encode(b"abc").as_bytes(), b"abc");
}

#[test]
fn keys_quote_zero_byte() {
    assert_eq!(
        Keys::encode(&[0]).as_bytes(),
        [K_SPECIAL, KS_ZERO, KE_FILLER]
    );
}

#[test]
fn keys_quote_literal_special_marker() {
    assert_eq!(
        Keys::encode(&[K_SPECIAL]).as_bytes(),
        [K_SPECIAL, KS_SPECIAL, KE_FILLER]
    );
}

#[test]
fn keys_mixed_round_trip_preserves_bytes() {
    assert_eq!(
        Keys::encode(&[b'a', 0, K_SPECIAL, b'z']).decode().unwrap(),
        [
            Key::Byte(b'a'),
            Key::Byte(0),
            Key::Byte(K_SPECIAL),
            Key::Byte(b'z')
        ]
    );
}

#[test]
fn encoded_named_special_key_decodes_as_special() {
    let keys = Keys::special(KS_EXTRA, 7).unwrap();
    assert_eq!(keys.decode().unwrap(), [Key::Special(KS_EXTRA, 7)]);
}

#[test]
fn special_key_rejects_third_byte_below_range() {
    assert_eq!(
        Keys::special(KS_EXTRA, 1),
        Err(KeyDecodeError::InvalidThirdByte(1))
    );
}

#[test]
fn special_key_rejects_third_byte_above_range() {
    assert_eq!(
        Keys::special(KS_EXTRA, 0x80),
        Err(KeyDecodeError::InvalidThirdByte(0x80))
    );
}

#[test]
fn encoded_key_rejects_one_byte_truncation() {
    assert_eq!(
        Keys::from_encoded(vec![K_SPECIAL]),
        Err(KeyDecodeError::Truncated(0))
    );
}

#[test]
fn encoded_key_rejects_two_byte_truncation() {
    assert_eq!(
        Keys::from_encoded(vec![K_SPECIAL, KS_EXTRA]),
        Err(KeyDecodeError::Truncated(0))
    );
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
    input
        .push(&keys("head"), 0, TypeaheadFlags::default())
        .unwrap();
    assert_eq!(input.as_bytes(), b"headtail");
}

#[test]
fn typeahead_push_inserts_at_middle_offset() {
    let mut input = Typeahead::new();
    input.append(&keys("ac"), TypeaheadFlags::default());
    input
        .push(&keys("b"), 1, TypeaheadFlags::default())
        .unwrap();
    assert_eq!(input.as_bytes(), b"abc");
}

#[test]
fn typeahead_push_inserts_at_end_offset() {
    let mut input = Typeahead::new();
    input.append(&keys("a"), TypeaheadFlags::default());
    input
        .push(&keys("b"), 1, TypeaheadFlags::default())
        .unwrap();
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
    input.append(
        &Keys::special(KS_EXTRA, 9).unwrap(),
        TypeaheadFlags::default(),
    );
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
fn feedkeys_i_inserts_before_pending_input() {
    let mut input = Typeahead::new();
    input.append(&keys("tail"), TypeaheadFlags::default());
    assert!(!input.feedkeys(&keys("head"), "i").unwrap());
    assert_eq!(input.as_bytes(), b"headtail");
}

#[test]
fn feedkeys_n_marks_injected_input_not_remappable() {
    let mut input = Typeahead::new();
    input.feedkeys(&keys("x"), "n").unwrap();
    assert_eq!(input.front_flags().unwrap().remap, Remap::No);
}

#[test]
fn feedkeys_x_requests_immediate_execution() {
    let mut input = Typeahead::new();
    assert!(input.feedkeys(&keys("x"), "x").unwrap());
    assert_eq!(input.as_bytes(), b"x");
}

#[test]
fn feedkeys_bang_appends_after_pending_input() {
    let mut input = Typeahead::new();
    input.append(&keys("pending"), TypeaheadFlags::default());
    input.feedkeys(&keys("later"), "!").unwrap();
    assert_eq!(input.as_bytes(), b"pendinglater");
}

#[test]
fn feedkeys_low_level_uses_event_key_path() {
    let mut input = Typeahead::new();
    input.feedkeys(&keys("x"), "L").unwrap();
    assert_eq!(
        input.pop().unwrap(),
        Some(Key::Special(KS_EXTRA, crate::KE_EVENT))
    );
    assert_eq!(input.pop().unwrap(), Some(Key::Byte(b'x')));
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
    assert_eq!(
        MappingAction::parse_rhs("<NoP>", "\\", "\\").unwrap(),
        MappingAction::Nop
    );
}

#[test]
fn mapping_rhs_encodes_plain_keys() {
    assert_eq!(
        MappingAction::parse_rhs("abc", "\\", "\\").unwrap(),
        MappingAction::Keys(keys("abc"))
    );
}

/// A command-shaped right-hand side keeps the text it was parsed from
/// (upstream's `m_str`), because `maparg()`'s string form and `:map`'s listing
/// render that text and a `Vec<ExCommand>` cannot be printed back to it.
#[test]
fn mapping_rhs_parses_cmd_form_with_ex_parser() {
    let MappingAction::ExCommands {
        keys: rhs,
        commands,
    } = MappingAction::parse_rhs("<Cmd>echo hi<CR>", "\\", "\\").unwrap()
    else {
        panic!("expected parsed Ex commands");
    };
    assert_eq!(commands.len(), 1);
    assert_eq!(rhs.as_bytes(), b"<Cmd>echo hi\r");
}

#[test]
fn mapping_rhs_parses_colon_command_form() {
    let MappingAction::ExCommands {
        keys: rhs,
        commands,
    } = MappingAction::parse_rhs(":echo hi<CR>", "\\", "\\").unwrap()
    else {
        panic!("expected parsed Ex commands");
    };
    assert_eq!(commands.len(), 1);
    assert_eq!(rhs.as_bytes(), b":echo hi\r");
}

#[test]
fn mapping_rhs_defines_unknown_ex_command_as_keys() {
    // Upstream defines the mapping regardless (`map_add` never parses the
    // body); E492 surfaces when the mapping fires. The pre-parsed Ex form is
    // a fast path only, so an unparseable body degrades to plain keys.
    assert!(matches!(
        MappingAction::parse_rhs("<Cmd>definitelynotacommand<CR>", "\\", "\\"),
        Ok(MappingAction::Keys(_))
    ));
    assert!(matches!(
        MappingAction::parse_rhs(":<C-U>call nope()<CR>", "\\", "\\"),
        Ok(MappingAction::Keys(_))
    ));
}

/// `replace_termcodes` (`keycodes.c`): a key right-hand side is notation, not
/// literal text. `nnoremap ,q ix<Esc>` used to insert the six characters
/// `<Esc>` writes instead of leaving Insert mode.
#[test]
fn mapping_rhs_decodes_key_notation_into_bytes() {
    assert_eq!(
        MappingAction::parse_rhs("ix<Esc>", "\\", "\\").unwrap(),
        MappingAction::Keys(keys("ix\u{1b}"))
    );
    assert_eq!(
        MappingAction::parse_rhs("o<Tab><CR><BS><Space><lt><Bar>", "\\", "\\").unwrap(),
        MappingAction::Keys(keys("o\t\r\u{8} <|"))
    );
    assert_eq!(
        MappingAction::parse_rhs("<C-u><C-A><C-?>", "\\", "\\").unwrap(),
        MappingAction::Keys(keys("\u{15}\u{1}\u{7f}"))
    );
}

/// `<Leader>`/`<LocalLeader>` expand to `mapleader`'s *text*, so they can be
/// several bytes, and the two leaders are independent.
#[test]
fn mapping_notation_expands_both_leaders() {
    assert_eq!(
        Keys::parse_notation("<Leader>x<LocalLeader>y", ",,", "-"),
        keys(",,x-y")
    );
}

/// Known special and modified keys use the internal keycode representation.
/// Unknown names and malformed notation stay literal.
#[test]
fn mapping_notation_encodes_known_names_and_preserves_unknown_names() {
    assert_eq!(
        Keys::parse_notation("<F2>", "\\", "\\"),
        Keys::from_encoded(vec![K_SPECIAL, b'k', b'2']).unwrap()
    );
    assert_eq!(Keys::parse_notation("<Up>", "\\", "\\"), keys("<Up>"));
    assert_eq!(
        Keys::parse_notation("<M-x>", "\\", "\\"),
        Keys::from_encoded(vec![K_SPECIAL, 0xfc, 8, b'x']).unwrap()
    );
    assert_eq!(
        Keys::parse_notation("<*C-I>", "\\", "\\"),
        Keys::from_encoded(vec![K_SPECIAL, 0xfc, 4, b'I']).unwrap()
    );
    assert_eq!(Keys::parse_notation("<C-", "\\", "\\"), keys("<C-"));
    assert_eq!(Keys::parse_notation("a<b", "\\", "\\"), keys("a<b"));
}

#[test]
fn map_rejects_empty_lhs() {
    assert!(matches!(
        Mappings::new().map(
            Keys::default(),
            MappingAction::Nop,
            MappingOptions::default()
        ),
        Err(MappingError::EmptyLhs)
    ));
}

#[test]
fn map_rejects_empty_modes() {
    assert!(matches!(
        Mappings::new().map(
            keys("x"),
            MappingAction::Nop,
            MappingOptions {
                modes: MapModes::NONE,
                ..MappingOptions::default()
            }
        ),
        Err(MappingError::EmptyModes)
    ));
}

#[test]
fn exact_mapping_lookup_returns_consumed_length() {
    let mut mappings = Mappings::new();
    mappings
        .map(keys("aa"), MappingAction::Nop, map_options(MapMode::Normal))
        .unwrap();
    assert!(matches!(
        mappings.lookup(b"aa", MapMode::Normal, None),
        Lookup::Exact(_, 2)
    ));
}

#[test]
fn unrelated_mapping_lookup_returns_none() {
    let mut mappings = Mappings::new();
    mappings
        .map(keys("aa"), MappingAction::Nop, map_options(MapMode::Normal))
        .unwrap();
    assert_eq!(mappings.lookup(b"z", MapMode::Normal, None), Lookup::None);
}

#[test]
fn proper_prefix_waits_for_more_input() {
    let mut mappings = Mappings::new();
    mappings
        .map(
            keys("abc"),
            MappingAction::Nop,
            map_options(MapMode::Normal),
        )
        .unwrap();
    assert!(matches!(
        mappings.lookup(b"ab", MapMode::Normal, None),
        Lookup::Prefix(None)
    ));
}

#[test]
fn exact_match_waits_when_longer_candidate_exists() {
    let mut mappings = Mappings::new();
    mappings
        .map(keys("aa"), MappingAction::Nop, map_options(MapMode::Normal))
        .unwrap();
    mappings
        .map(
            keys("aaa"),
            MappingAction::Nop,
            map_options(MapMode::Normal),
        )
        .unwrap();
    assert!(matches!(
        mappings.lookup(b"aa", MapMode::Normal, None),
        Lookup::Prefix(Some(_))
    ));
}

#[test]
fn nowait_exact_match_wins_over_longer_candidate() {
    let mut mappings = Mappings::new();
    let mut options = map_options(MapMode::Normal);
    options.flags.set(crate::MapFlags::NOWAIT, true);
    mappings
        .map(keys("aa"), MappingAction::Nop, options)
        .unwrap();
    mappings
        .map(
            keys("aaa"),
            MappingAction::Nop,
            map_options(MapMode::Normal),
        )
        .unwrap();
    assert!(matches!(
        mappings.lookup(b"aa", MapMode::Normal, None),
        Lookup::Exact(_, 2)
    ));
}

#[test]
fn longest_complete_lhs_wins_with_extra_typeahead() {
    let mut mappings = Mappings::new();
    mappings
        .map(
            keys("a"),
            MappingAction::Callback(1),
            map_options(MapMode::Normal),
        )
        .unwrap();
    mappings
        .map(
            keys("ab"),
            MappingAction::Callback(2),
            map_options(MapMode::Normal),
        )
        .unwrap();
    let Lookup::Exact(mapping, length) = mappings.lookup(b"abc", MapMode::Normal, None) else {
        panic!("expected exact mapping");
    };
    assert_eq!(length, 2);
    assert_eq!(mapping.action, MappingAction::Callback(2));
}

#[test]
fn buffer_local_mapping_precedes_global_mapping() {
    let mut mappings = Mappings::new();
    mappings
        .map(
            keys("x"),
            MappingAction::Callback(1),
            map_options(MapMode::Normal),
        )
        .unwrap();
    let mut local = map_options(MapMode::Normal);
    local.scope = MapScope::Buffer(buffer(4));
    mappings
        .map(keys("x"), MappingAction::Callback(2), local)
        .unwrap();
    let Lookup::Exact(mapping, _) = mappings.lookup(b"x", MapMode::Normal, Some(buffer(4))) else {
        panic!("expected local mapping");
    };
    assert_eq!(mapping.action, MappingAction::Callback(2));
}

#[test]
fn global_mapping_is_fallback_when_local_does_not_match() {
    let mut mappings = Mappings::new();
    mappings
        .map(
            keys("x"),
            MappingAction::Callback(1),
            map_options(MapMode::Normal),
        )
        .unwrap();
    let mut local = map_options(MapMode::Normal);
    local.scope = MapScope::Buffer(buffer(4));
    mappings
        .map(keys("y"), MappingAction::Callback(2), local)
        .unwrap();
    let Lookup::Exact(mapping, _) = mappings.lookup(b"x", MapMode::Normal, Some(buffer(4))) else {
        panic!("expected global mapping");
    };
    assert_eq!(mapping.action, MappingAction::Callback(1));
}

#[test]
fn mapping_lookup_filters_by_mode() {
    let mut mappings = Mappings::new();
    mappings
        .map(keys("x"), MappingAction::Nop, map_options(MapMode::Insert))
        .unwrap();
    assert_eq!(mappings.lookup(b"x", MapMode::Normal, None), Lookup::None);
    assert!(matches!(
        mappings.lookup(b"x", MapMode::Insert, None),
        Lookup::Exact(_, 1)
    ));
}

#[test]
fn noremap_records_nonrecursive_policy() {
    let mut mappings = Mappings::new();
    mappings
        .noremap(keys("x"), MappingAction::Nop, map_options(MapMode::Normal))
        .unwrap();
    let Lookup::Exact(mapping, _) = mappings.lookup(b"x", MapMode::Normal, None) else {
        panic!("expected mapping");
    };
    assert!(!mapping.options.flags.contains(crate::MapFlags::REMAP));
}

#[test]
fn map_records_recursive_policy_by_default() {
    let mut mappings = Mappings::new();
    mappings
        .map(keys("x"), MappingAction::Nop, map_options(MapMode::Normal))
        .unwrap();
    let Lookup::Exact(mapping, _) = mappings.lookup(b"x", MapMode::Normal, None) else {
        panic!("expected mapping");
    };
    assert!(mapping.options.flags.contains(crate::MapFlags::REMAP));
}

#[test]
fn later_mapping_replaces_overlapping_mode_only() {
    let mut mappings = Mappings::new();
    let modes = MapMode::Normal | MapMode::Visual;
    mappings
        .map(
            keys("x"),
            MappingAction::Callback(1),
            MappingOptions {
                modes,
                ..MappingOptions::default()
            },
        )
        .unwrap();
    mappings
        .map(
            keys("x"),
            MappingAction::Callback(2),
            map_options(MapMode::Normal),
        )
        .unwrap();
    let Lookup::Exact(normal, _) = mappings.lookup(b"x", MapMode::Normal, None) else {
        panic!("normal");
    };
    let Lookup::Exact(visual, _) = mappings.lookup(b"x", MapMode::Visual, None) else {
        panic!("visual");
    };
    assert_eq!(normal.action, MappingAction::Callback(2));
    assert_eq!(visual.action, MappingAction::Callback(1));
}

#[test]
fn unmap_removes_selected_mode_and_preserves_other_mode() {
    let mut mappings = Mappings::new();
    mappings
        .map(
            keys("x"),
            MappingAction::Nop,
            MappingOptions {
                modes: MapMode::Normal | MapMode::Visual,
                ..MappingOptions::default()
            },
        )
        .unwrap();
    assert_eq!(
        mappings.unmap(&keys("x"), MapMode::Normal.into(), MapScope::Global),
        1
    );
    assert_eq!(mappings.lookup(b"x", MapMode::Normal, None), Lookup::None);
    assert!(matches!(
        mappings.lookup(b"x", MapMode::Visual, None),
        Lookup::Exact(_, 1)
    ));
}

#[test]
fn mapclear_affects_only_selected_scope() {
    let mut mappings = Mappings::new();
    mappings
        .map(keys("x"), MappingAction::Nop, map_options(MapMode::Normal))
        .unwrap();
    let mut local = map_options(MapMode::Normal);
    local.scope = MapScope::Buffer(buffer(2));
    mappings.map(keys("y"), MappingAction::Nop, local).unwrap();
    assert_eq!(
        mappings.mapclear(MapMode::Normal.into(), MapScope::Global),
        1
    );
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
    mappings
        .map(keys("xy"), MappingAction::Nop, map_options(MapMode::Normal))
        .unwrap();
    let mut input = Typeahead::new();
    input.append(&keys("xy"), TypeaheadFlags::default());
    assert!(matches!(
        mappings.lookup_typeahead(&input, MapMode::Normal, None),
        Lookup::Exact(_, 2)
    ));
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
    mappings
        .abbreviate("#i", MappingAction::Nop, MapScope::Global, true)
        .unwrap();
    assert!(mappings.lookup_abbreviation("value#i", ' ', None).is_some());
}

#[test]
fn nonkeyword_ending_abbreviation_requires_whitespace_boundary() {
    let mut mappings = Mappings::new();
    mappings
        .abbreviate("a-", MappingAction::Nop, MapScope::Global, true)
        .unwrap();
    assert!(mappings.lookup_abbreviation(" a-", ' ', None).is_some());
    assert!(mappings.lookup_abbreviation("xa-", ' ', None).is_none());
}

#[test]
fn abbreviation_triggers_on_nonkeyword_delimiter() {
    let mut mappings = Mappings::new();
    mappings
        .abbreviate(
            "teh",
            MappingAction::Keys(keys("the")),
            MapScope::Global,
            true,
        )
        .unwrap();
    assert_eq!(
        mappings.lookup_abbreviation("teh", ' ', None).unwrap().lhs,
        "teh"
    );
}

#[test]
fn abbreviation_does_not_trigger_on_keyword_character() {
    let mut mappings = Mappings::new();
    mappings
        .abbreviate("teh", MappingAction::Nop, MapScope::Global, true)
        .unwrap();
    assert!(mappings.lookup_abbreviation("teh", 'x', None).is_none());
}

#[test]
fn abbreviation_requires_start_of_word_boundary() {
    let mut mappings = Mappings::new();
    mappings
        .abbreviate("teh", MappingAction::Nop, MapScope::Global, true)
        .unwrap();
    assert!(mappings.lookup_abbreviation("ateh", ' ', None).is_none());
}

#[test]
fn buffer_abbreviation_precedes_global_abbreviation() {
    let mut mappings = Mappings::new();
    mappings
        .abbreviate("teh", MappingAction::Callback(1), MapScope::Global, true)
        .unwrap();
    mappings
        .abbreviate(
            "teh",
            MappingAction::Callback(2),
            MapScope::Buffer(buffer(3)),
            true,
        )
        .unwrap();
    let found = mappings
        .lookup_abbreviation("teh", ' ', Some(buffer(3)))
        .unwrap();
    assert_eq!(found.action, MappingAction::Callback(2));
}

#[test]
fn unabbreviate_removes_only_selected_scope() {
    let mut mappings = Mappings::new();
    mappings
        .abbreviate("teh", MappingAction::Nop, MapScope::Global, true)
        .unwrap();
    mappings
        .abbreviate("teh", MappingAction::Nop, MapScope::Buffer(buffer(3)), true)
        .unwrap();
    assert!(mappings.unabbreviate("teh", MapScope::Global));
    assert_eq!(mappings.abbreviation_len(), 1);
}

#[test]
fn abbrevclear_removes_one_scope() {
    let mut mappings = Mappings::new();
    mappings
        .abbreviate("teh", MappingAction::Nop, MapScope::Global, true)
        .unwrap();
    mappings
        .abbreviate(
            "recieve",
            MappingAction::Nop,
            MapScope::Buffer(buffer(3)),
            true,
        )
        .unwrap();
    assert_eq!(mappings.abbrevclear(MapScope::Global), 1);
    assert_eq!(mappings.abbreviation_len(), 1);
}

#[test]
fn remove_buffer_clears_local_maps_and_abbreviations() {
    let mut mappings = Mappings::new();
    let scope = MapScope::Buffer(buffer(5));
    mappings
        .map(
            keys("x"),
            MappingAction::Nop,
            MappingOptions {
                scope,
                modes: MapMode::Normal.into(),
                ..MappingOptions::default()
            },
        )
        .unwrap();
    mappings
        .abbreviate("teh", MappingAction::Nop, scope, true)
        .unwrap();
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
    editor
        .autocmds_mut()
        .register_legacy(
            &[Event::BufEnter],
            "<abuf>",
            &ex("local"),
            &AutocmdOptions {
                buffer: Some(handle),
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    editor
        .mappings_mut()
        .map(
            keys("x"),
            MappingAction::Nop,
            MappingOptions {
                scope: MapScope::Buffer(handle),
                modes: MapMode::Normal.into(),
                ..MappingOptions::default()
            },
        )
        .unwrap();
    editor.wipe_buffer(handle).unwrap();
    assert!(editor.autocmds().is_empty());
    assert_eq!(editor.mappings().mapping_len(), 0);
}

#[test]
fn editor_wipe_preserves_global_and_other_buffer_state() {
    let mut editor = Editor::new();
    let wiped = editor.create_buffer(true).unwrap();
    let kept = editor.create_buffer(true).unwrap();
    editor
        .mappings_mut()
        .map(keys("g"), MappingAction::Nop, map_options(MapMode::Normal))
        .unwrap();
    editor
        .mappings_mut()
        .map(
            keys("k"),
            MappingAction::Nop,
            MappingOptions {
                scope: MapScope::Buffer(kept),
                modes: MapMode::Normal.into(),
                ..MappingOptions::default()
            },
        )
        .unwrap();
    editor.wipe_buffer(wiped).unwrap();
    assert_eq!(editor.mappings().mapping_len(), 2);
    assert!(matches!(
        editor.mappings().lookup(b"g", MapMode::Normal, None),
        Lookup::Exact(_, 1)
    ));
    assert!(matches!(
        editor.mappings().lookup(b"k", MapMode::Normal, Some(kept)),
        Lookup::Exact(_, 1)
    ));
}

/// `str2special` renders a C0 control byte through the special-key table when
/// it has an entry and as `<C-x>` when it does not (`message.c:2141-2148`,
/// `keycodes.c:292-297`). Each byte below picks a different arm, so a mutation
/// to any one of them shows up here rather than only in whichever arm a
/// single-case test happened to hit.
///
/// Oracle, v0.13.0-dev-1390: `keytrans(nr2char(n))` for n in 1..=32.
#[test]
fn special_notation_names_each_class_of_control_byte() {
    use crate::typeahead::special_notation;

    for (byte, expected) in [
        (0x01, "<C-A>"),
        (0x08, "<C-H>"),
        (0x09, "<Tab>"),
        (0x0a, "<NL>"),
        (0x0d, "<CR>"),
        (0x1a, "<C-Z>"),
        (0x1b, "<Esc>"),
        (0x1c, "<C-\\>"),
        (0x1f, "<C-_>"),
    ] {
        assert_eq!(
            special_notation(&[byte], false, false),
            expected,
            "byte {byte:#04x}"
        );
    }

    // A space and a `<` are conditional on the two flags, and each flag gates
    // only its own byte.
    assert_eq!(special_notation(b" <", false, false), " <");
    assert_eq!(special_notation(b" <", true, false), "<Space><");
    assert_eq!(special_notation(b" <", false, true), " <lt>");

    // Printable text and multi-byte characters pass through byte for byte.
    assert_eq!(
        special_notation("a\u{00e9}\u{4e2d}z".as_bytes(), true, true),
        "a\u{00e9}\u{4e2d}z"
    );

    // The two quoting pairs `Keys::encode` produces stand for the bytes they
    // quote, as `mb_unescape` does before `str2special` looks at them.
    assert_eq!(
        special_notation(&[K_SPECIAL, KS_ZERO, KE_FILLER], false, false),
        "<Nul>"
    );
    assert_eq!(
        special_notation(&[K_SPECIAL, b'k', b'b'], false, false),
        "<t_kb>",
        "a named special key this port cannot name prints its termcap pair"
    );
}

/// `map_mode_to_chars` (`mapping.c:170-208`) is a chain of alternatives whose
/// order matters: insert-plus-cmdline wins over insert, and the four `:map`
/// modes together collapse to a blank. One case per arm.
#[test]
fn mode_chars_and_bits_cover_every_map_mode_to_chars_arm() {
    let of = |modes: MapModes| modes.to_chars();
    assert_eq!(of(MapModes::MAP_BANG), "!");
    assert_eq!(of(MapMode::Insert.into()), "i");
    assert_eq!(of(MapMode::LangArg.into()), "l");
    assert_eq!(of(MapMode::CommandLine.into()), "c");
    assert_eq!(of(MapModes::MAP), " ");
    assert_eq!(of(MapMode::Normal.into()), "n");
    assert_eq!(of(MapMode::OperatorPending.into()), "o");
    assert_eq!(of(MapMode::Terminal.into()), "t");
    assert_eq!(of(MapMode::Visual | MapMode::Select), "v");
    assert_eq!(of(MapMode::Visual.into()), "x");
    assert_eq!(of(MapMode::Select.into()), "s");
    assert_eq!(of(MapMode::Normal | MapMode::OperatorPending), "no");

    // The discriminants are upstream's `MODE_*` values, which `maparg()`
    // reports verbatim as `mode_bits` (`state_defs.h:21-28`).
    assert_eq!(MapModes::one(MapMode::Normal).bits(), 0x01);
    assert_eq!(MapModes::one(MapMode::Visual).bits(), 0x02);
    assert_eq!(MapModes::one(MapMode::OperatorPending).bits(), 0x04);
    assert_eq!(MapModes::one(MapMode::CommandLine).bits(), 0x08);
    assert_eq!(MapModes::one(MapMode::Insert).bits(), 0x10);
    assert_eq!(MapModes::one(MapMode::LangArg).bits(), 0x20);
    assert_eq!(MapModes::one(MapMode::Select).bits(), 0x40);
    assert_eq!(MapModes::one(MapMode::Terminal).bits(), 0x80);
    assert_eq!(MapModes::MAP.bits(), 0x47);
}

/// `get_map_mode` (`mapping.c:988-1023`) over `maparg()`'s mode string: only
/// the first character decides, an unknown or empty string means `:map`, and
/// the `n`-not-followed-by-`o` guard keeps `noremap` out of Normal mode.
#[test]
fn mode_string_parsing_matches_get_map_mode() {
    for (text, expected) in [
        ("i", MapModes::one(MapMode::Insert)),
        ("l", MapModes::one(MapMode::LangArg)),
        ("c", MapModes::one(MapMode::CommandLine)),
        ("n", MapModes::one(MapMode::Normal)),
        ("nx", MapModes::one(MapMode::Normal)),
        ("v", MapMode::Visual | MapMode::Select),
        ("x", MapModes::one(MapMode::Visual)),
        ("s", MapModes::one(MapMode::Select)),
        ("o", MapModes::one(MapMode::OperatorPending)),
        ("t", MapModes::one(MapMode::Terminal)),
        ("", MapModes::MAP),
        ("z", MapModes::MAP),
        ("noremap", MapModes::MAP),
    ] {
        assert_eq!(
            MapModes::from_mode_string(text),
            expected,
            "mode string {text:?}"
        );
    }
}

/// `check_map` with `exact` set (`mapping.c:2036`) tests the mode overlap and
/// the exact key length, and searches the buffer-local table before the global
/// one. Each of the three is exercised where the other two cannot decide it.
#[test]
fn exact_mapping_lookup_tests_mode_length_and_locality_separately() {
    let mut mappings = Mappings::new();
    mappings
        .map(keys("xy"), MappingAction::Nop, map_options(MapMode::Normal))
        .unwrap();
    mappings
        .map(keys("z"), MappingAction::Nop, map_options(MapMode::Insert))
        .unwrap();

    // Length: a prefix of a registered lhs is not an exact match.
    assert!(
        mappings
            .find_exact(b"xy", MapMode::Normal.into(), None)
            .is_some()
    );
    assert!(
        mappings
            .find_exact(b"x", MapMode::Normal.into(), None)
            .is_none()
    );
    assert!(
        mappings
            .find_exact(b"xyz", MapMode::Normal.into(), None)
            .is_none()
    );

    // Mode: the same lhs in the wrong mode does not match.
    assert!(
        mappings
            .find_exact(b"z", MapMode::Insert.into(), None)
            .is_some()
    );
    assert!(
        mappings
            .find_exact(b"z", MapMode::Normal.into(), None)
            .is_none()
    );

    // Locality: a buffer-local mapping shadows the global one with the same
    // lhs, and is reported as local.
    let mut local = map_options(MapMode::Normal);
    local.scope = MapScope::Buffer(buffer(7));
    mappings
        .map(keys("xy"), MappingAction::Keys(keys("local")), local)
        .unwrap();
    let (found, is_local) = mappings
        .find_exact(b"xy", MapMode::Normal.into(), Some(buffer(7)))
        .unwrap();
    assert!(is_local);
    assert_eq!(found.action, MappingAction::Keys(keys("local")));
    let (found, is_local) = mappings
        .find_exact(b"xy", MapMode::Normal.into(), Some(buffer(8)))
        .unwrap();
    assert!(!is_local, "another buffer sees the global mapping");
    assert_eq!(found.action, MappingAction::Nop);
}

fn windowed_editor() -> (Editor, BufHandle, ox_types::TabHandle) {
    let mut editor = Editor::new();
    let caller = editor.create_buffer(true).unwrap();
    let tab = editor
        .create_tabpage(caller, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    (editor, caller, tab)
}

#[test]
fn callback_args_carry_present_data_and_omit_absent_data() {
    let mut autocmds = Autocmds::new();
    autocmds
        .register_legacy(&[Event::User], "*", &ex("x"), &AutocmdOptions::default())
        .unwrap();
    let data = ox_types::Object::Integer(7);
    let present = autocmds.plan(
        Event::User,
        AutocmdContext {
            data: Some(&data),
            ..context(None, Some("Build"))
        },
    );
    let action = &present.ready[0];
    assert_eq!(action.data, Some(data.clone()));
    let present_args = action.callback_args().unwrap();
    let [ox_types::Object::Dict(entries)] = present_args.as_slice() else {
        panic!("expected one callback dictionary");
    };
    assert!(
        entries
            .iter()
            .any(|(key, value)| key.to_string_lossy().as_ref() == "data" && value == &data)
    );

    let absent = autocmds.plan(Event::User, context(None, Some("Build")));
    let action = &absent.ready[0];
    assert_eq!(action.data, None);
    let absent_args = action.callback_args().unwrap();
    let [ox_types::Object::Dict(entries)] = absent_args.as_slice() else {
        panic!("expected one callback dictionary");
    };
    assert!(
        entries
            .iter()
            .all(|(key, _)| key.to_string_lossy().as_ref() != "data")
    );
}

#[test]
fn buffer_context_runs_in_first_visible_target_window_and_restores() {
    let (mut editor, caller, _) = windowed_editor();
    let caller_window = editor.current_window().unwrap();
    let target = editor.create_buffer(true).unwrap();
    let target_tab = editor
        .create_tabpage(target, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let target_window = editor.tabpage(target_tab).unwrap().current_window();
    editor.set_current_window(caller_window).unwrap();

    let outcome = editor
        .in_buffer_context(
            Event::User,
            target,
            |editor: &mut Editor| -> Result<(), crate::editor::EditorError> {
                assert_eq!(editor.current_window(), Some(target_window));
                assert_eq!(editor.current_buffer(), Some(target));
                // A handler buffer switch inside the entered window is undone.
                editor.set_current_buffer(caller, crate::editor::BufferRelease::KeepLoaded)?;
                Ok(())
            },
        )
        .unwrap();
    outcome.unwrap().unwrap();
    assert_eq!(editor.current_window(), Some(caller_window));
    assert_eq!(editor.current_buffer(), Some(caller));
    assert_eq!(editor.window(target_window).unwrap().buffer, target);
}

#[test]
fn buffer_context_displays_hidden_target_in_caller_window() {
    let (mut editor, caller, _) = windowed_editor();
    let hidden = editor.create_buffer(true).unwrap();
    let outcome = editor
        .in_buffer_context(
            Event::User,
            hidden,
            |editor: &mut Editor| -> Result<(), crate::editor::EditorError> {
                assert_eq!(editor.current_buffer(), Some(hidden));
                Ok(())
            },
        )
        .unwrap();
    outcome.unwrap().unwrap();
    assert_eq!(editor.current_buffer(), Some(caller));
    assert!(
        editor
            .windows()
            .into_iter()
            .all(|window| editor.window(window).unwrap().buffer != hidden)
    );
}

#[test]
fn buffer_context_restores_nested_hidden_targets() {
    let (mut editor, caller, tab) = windowed_editor();
    let outer = editor.create_buffer(true).unwrap();
    let inner = editor.create_buffer(true).unwrap();
    let outcome = editor
        .in_buffer_context(
            Event::User,
            outer,
            |editor: &mut Editor| -> Result<(), crate::editor::EditorError> {
                assert_eq!(editor.current_buffer(), Some(outer));
                let nested = editor
                    .in_buffer_context(
                        Event::User,
                        inner,
                        |editor: &mut Editor| -> Result<(), crate::editor::EditorError> {
                            assert_eq!(editor.current_buffer(), Some(inner));
                            Ok(())
                        },
                    )
                    .unwrap();
                nested.unwrap().unwrap();
                assert_eq!(editor.current_buffer(), Some(outer));
                Ok(())
            },
        )
        .unwrap();
    outcome.unwrap().unwrap();
    assert_eq!(editor.current_buffer(), Some(caller));
    assert_eq!(
        editor.current_window(),
        Some(editor.tabpage(tab).unwrap().current_window())
    );
}

#[test]
fn buffer_context_restores_on_injected_host_error() {
    let (mut editor, caller, _) = windowed_editor();
    let hidden = editor.create_buffer(true).unwrap();
    let outcome = editor
        .in_buffer_context(
            Event::User,
            hidden,
            |editor: &mut Editor| -> Result<(), &str> {
                assert_eq!(editor.current_buffer(), Some(hidden));
                Err("injected")
            },
        )
        .unwrap();
    assert_eq!(outcome, Some(Err("injected")));
    assert_eq!(editor.current_buffer(), Some(caller));
}

#[test]
fn buffer_context_tolerates_target_wipe_during_execution() {
    let (mut editor, caller, _) = windowed_editor();
    let target = editor.create_buffer(true).unwrap();
    let target_tab = editor
        .create_tabpage(target, crate::layout::Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let target_window = editor.tabpage(target_tab).unwrap().current_window();
    let caller_window = editor.current_window().unwrap();
    let outcome = editor
        .in_buffer_context(Event::User, target, |editor: &mut Editor| {
            // The handler moves off the target, then wipes it.
            editor
                .set_current_buffer(caller, crate::editor::BufferRelease::KeepLoaded)
                .map_err(|_| ())?;
            editor.wipe_buffer(target).map_err(|_| ())?;
            Ok::<(), ()>(())
        })
        .unwrap();
    outcome.unwrap().unwrap();
    assert!(editor.buffer(target).is_err());
    assert_eq!(editor.current_window(), Some(caller_window));
    assert_eq!(editor.window(target_window).unwrap().buffer, caller);
}

#[test]
fn buffer_context_skips_ignored_events_without_switching() {
    let (mut editor, caller, _) = windowed_editor();
    let hidden = editor.create_buffer(true).unwrap();
    editor.autocmds_mut().ignore(Event::User);
    let mut ran = false;
    let outcome = editor
        .in_buffer_context(Event::User, hidden, |_: &mut Editor| {
            ran = true;
            Ok::<(), ()>(())
        })
        .unwrap();
    assert_eq!(outcome, None);
    assert!(!ran);
    assert_eq!(editor.current_buffer(), Some(caller));
}

#[test]
fn buffer_context_skips_window_ignoring_event_for_sibling() {
    let (mut editor, _caller, tab) = windowed_editor();
    let target = editor.create_buffer(true).unwrap();
    let caller_window = editor.current_window().unwrap();
    let target_window = editor
        .split_vertical(tab, caller_window, target, true)
        .unwrap();
    editor
        .set_window_buffer(
            caller_window,
            target,
            crate::editor::BufferRelease::KeepLoaded,
        )
        .unwrap();
    editor.set_current_window(target_window).unwrap();
    editor
        .options_mut()
        .set_window(
            target_window,
            "eventignorewin",
            OptionValue::String("User".to_owned()),
        )
        .unwrap();

    let mut ran_in = None;
    let outcome = editor
        .in_buffer_context(Event::User, target, |editor: &mut Editor| {
            ran_in = editor.current_window();
            Ok::<(), ()>(())
        })
        .unwrap();
    outcome.unwrap().unwrap();
    // The ignoring window was skipped; the sibling that displays the target
    // ran the handler instead.
    assert_eq!(ran_in, Some(caller_window));
    assert_eq!(editor.current_buffer(), Some(target));
}

#[test]
fn buffer_context_skips_execution_when_every_window_ignores() {
    let (mut editor, _caller, _tab) = windowed_editor();
    let target = editor.create_buffer(true).unwrap();
    editor
        .set_current_buffer(target, crate::editor::BufferRelease::KeepLoaded)
        .unwrap();
    let window = editor.current_window().unwrap();
    editor
        .options_mut()
        .set_window(
            window,
            "eventignorewin",
            OptionValue::String("User".to_owned()),
        )
        .unwrap();

    let mut ran = false;
    let outcome = editor
        .in_buffer_context(Event::User, target, |_: &mut Editor| {
            ran = true;
            Ok::<(), ()>(())
        })
        .unwrap();
    assert_eq!(outcome, None);
    assert!(!ran);
    assert_eq!(editor.current_buffer(), Some(target));
}

#[test]
fn buffer_context_ignores_unknown_eventignorewin_names() {
    let (mut editor, caller, _tab) = windowed_editor();
    let target = editor.create_buffer(true).unwrap();
    editor
        .set_current_buffer(target, crate::editor::BufferRelease::KeepLoaded)
        .unwrap();
    let window = editor.current_window().unwrap();
    editor
        .options_mut()
        .set_window(
            window,
            "eventignorewin",
            OptionValue::String("NotAnEvent".to_owned()),
        )
        .unwrap();

    let mut ran = false;
    let outcome = editor
        .in_buffer_context(Event::User, target, |_: &mut Editor| {
            ran = true;
            Ok::<(), ()>(())
        })
        .unwrap();
    outcome.unwrap().unwrap();
    assert!(ran);
    assert_eq!(editor.current_buffer(), Some(target));
    assert_eq!(editor.current_window(), Some(window));
    let _ = caller;
}
