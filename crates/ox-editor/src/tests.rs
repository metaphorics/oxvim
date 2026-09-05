#![allow(clippy::unwrap_used)]

use std::fs;

use ox_text::{Buffer, Position};
use ox_types::{BufHandle, WinHandle};

use crate::BufferRelease;
use crate::Editor;
use crate::layout::{
    Anchor, Frame, Geometry, Layout, RelativeTo, TabpageState, WinConfig, WindowState,
};
use crate::marks::{Changelists, Jumplist, LocalMarks, MarkLocation};
use crate::options::{OPTION_COUNT, OPTION_METADATA, OptionError, OptionStore, OptionValue};
use crate::register::{ClipboardProvider, RegisterContent, RegisterError, Registers, Selection};

fn buffer_handle(value: i64) -> BufHandle {
    BufHandle::try_from(value).unwrap()
}

fn window_handle(value: i64) -> WinHandle {
    WinHandle::try_from(value).unwrap()
}

fn position(lnum: usize, col: usize) -> Position {
    Position { lnum, col }
}

#[test]
fn buffer_lifecycle_is_monotonic_and_tracks_hidden_state() {
    let mut editor = Editor::new();
    let first = editor.create_buffer(true).unwrap();
    assert_eq!(i64::from(first), 1);
    assert!(editor.buffer(first).unwrap().residency.is_loaded());
    assert!(editor.buffer(first).unwrap().residency.is_hidden());

    let tab = editor
        .create_tabpage(first, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    assert_eq!(editor.buffer(first).unwrap().attachments, 1);
    assert!(!editor.buffer(first).unwrap().residency.is_hidden());
    assert!(editor.wipe_buffer(first).is_err());

    editor.close_window(tab, window, true).unwrap_err();
    let second = editor.create_buffer(false).unwrap();
    assert_eq!(i64::from(second), 2);

    let mut detached = crate::BufferState::new(
        Buffer::from_lines(&[b"resident".to_vec()], false).unwrap(),
        true,
    );
    detached.attach().unwrap();
    detached.detach(true);
    assert!(detached.residency.is_loaded() && detached.residency.is_hidden());
    detached.attach().unwrap();
    detached.detach(false);
    assert!(!detached.residency.is_loaded() && !detached.residency.is_hidden());
    assert!(detached.text().is_err());
    assert!(detached.attach().is_err());
    detached.load(Buffer::from_lines(&[b"reloaded".to_vec()], false).unwrap());
    assert_eq!(detached.text().unwrap().line(1).unwrap(), b"reloaded");
}

#[test]
fn text_mutations_advance_all_ticks_and_splice_marks() {
    let text = Buffer::from_lines(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], false).unwrap();
    let mut state = crate::BufferState::new(text, true);
    state.marks.set('a', position(3, 0)).unwrap();

    state
        .append_lines(1, &[b"x".to_vec(), b"y".to_vec()], position(1, 0), 1)
        .unwrap();
    assert_eq!(state.changedtick(), 1);
    assert_eq!(state.changedtick_diag, 1);
    assert_eq!(state.changedtick_fold, 1);
    assert_eq!(state.marks.get('a').unwrap(), Some(position(5, 0)));

    state.delete_lines(2, 4, position(2, 0), 2).unwrap();
    assert_eq!(state.changedtick(), 2);
    assert_eq!(state.changedtick_diag, 2);
    assert_eq!(state.changedtick_fold, 2);
    assert_eq!(state.marks.get('a').unwrap(), Some(position(2, 0)));
}

#[test]
fn editor_window_state_has_one_owner_and_balanced_attachments() {
    let mut editor = Editor::new();
    let first = editor.create_buffer(true).unwrap();
    let second = editor.create_buffer(true).unwrap();
    let tab = editor
        .create_tabpage(first, Geometry::new(0, 0, 20, 10).unwrap())
        .unwrap();
    let original = editor.tabpage(tab).unwrap().current_window();
    let switched = editor.split_vertical(tab, original, first, true).unwrap();

    editor
        .set_window_buffer(switched, second, BufferRelease::KeepLoaded)
        .unwrap();
    editor.set_window_cursor(switched, position(1, 2)).unwrap();
    assert_eq!(editor.window(switched).unwrap().buffer, second);
    assert_eq!(editor.window(switched).unwrap().cursor, position(1, 2));
    assert_eq!(editor.buffer(first).unwrap().attachments, 1);
    assert_eq!(editor.buffer(second).unwrap().attachments, 1);

    let removed = editor.close_window(tab, switched, true).unwrap();
    assert_eq!(removed.buffer, second);
    assert_eq!(removed.cursor, position(1, 2));
    assert_eq!(editor.buffer(first).unwrap().attachments, 1);
    assert_eq!(editor.buffer(second).unwrap().attachments, 0);
    assert!(editor.buffer(second).unwrap().residency.is_hidden());
}

#[test]
fn frame_tree_matches_naive_equal_partition_geometry() {
    let w1 = window_handle(1);
    let w2 = window_handle(2);
    let w3 = window_handle(3);
    let buffer = buffer_handle(1);
    let geometry = Geometry::new(0, 0, 9, 7).unwrap();
    let mut layout = Layout::new(w1, WindowState::new(buffer, position(1, 0)), geometry).unwrap();

    layout
        .split_vertical(w1, w2, WindowState::new(buffer, position(1, 0)))
        .unwrap();
    assert_eq!(
        layout.window_geometry(w1).unwrap(),
        Geometry::new(0, 0, 5, 7).unwrap()
    );
    assert_eq!(
        layout.window_geometry(w2).unwrap(),
        Geometry::new(0, 5, 4, 7).unwrap()
    );
    layout
        .split_horizontal(w2, w3, WindowState::new(buffer, position(1, 0)))
        .unwrap();
    assert_eq!(
        layout.window_geometry(w2).unwrap(),
        Geometry::new(0, 5, 4, 4).unwrap()
    );
    assert_eq!(
        layout.window_geometry(w3).unwrap(),
        Geometry::new(4, 5, 4, 3).unwrap()
    );
    assert_eq!(layout.winnr(w1).unwrap(), 1);
    assert_eq!(layout.winnr(w2).unwrap(), 2);
    assert_eq!(layout.winnr(w3).unwrap(), 3);

    layout.close(w2).unwrap();
    assert_eq!(layout.window_by_winnr(2).unwrap(), w3);
    layout.resize(Geometry::new(2, 3, 10, 8).unwrap()).unwrap();
    layout.equalize().unwrap();
    assert_eq!(
        layout.window_geometry(w1).unwrap(),
        Geometry::new(2, 3, 5, 8).unwrap()
    );
    assert_eq!(
        layout.window_geometry(w3).unwrap(),
        Geometry::new(2, 8, 5, 8).unwrap()
    );
}

#[test]
fn closing_split_preserves_root_geometry_across_repeated_cycles() {
    let w1 = window_handle(1);
    let buffer = buffer_handle(1);
    let geometry = Geometry::new(0, 0, 80, 24).unwrap();
    let mut layout = Layout::new(w1, WindowState::new(buffer, position(1, 0)), geometry).unwrap();

    for id in 2..=8 {
        let split = window_handle(id);
        layout
            .split_horizontal(w1, split, WindowState::new(buffer, position(1, 0)))
            .unwrap();
        layout.close(split).unwrap();
        assert_eq!(layout.window_geometry(w1).unwrap(), geometry);
    }
}

#[test]
fn three_same_axis_splits_equalize_into_equal_thirds() {
    let w1 = window_handle(1);
    let w2 = window_handle(2);
    let w3 = window_handle(3);
    let buffer = buffer_handle(1);
    let geometry = Geometry::new(0, 0, 9, 7).unwrap();
    let mut layout = Layout::new(w1, WindowState::new(buffer, position(1, 0)), geometry).unwrap();

    layout
        .split_vertical(w1, w2, WindowState::new(buffer, position(1, 0)))
        .unwrap();
    layout
        .split_vertical(w2, w3, WindowState::new(buffer, position(1, 0)))
        .unwrap();
    layout.equalize().unwrap();

    assert_eq!(
        layout.window_geometry(w1).unwrap(),
        Geometry::new(0, 0, 3, 7).unwrap()
    );
    assert_eq!(
        layout.window_geometry(w2).unwrap(),
        Geometry::new(0, 3, 3, 7).unwrap()
    );
    assert_eq!(
        layout.window_geometry(w3).unwrap(),
        Geometry::new(0, 6, 3, 7).unwrap()
    );
}

#[test]
fn split_uses_immediate_parent_axis_not_deepest_matching_ancestor() {
    let w1 = window_handle(1);
    let w2 = window_handle(2);
    let w3 = window_handle(3);
    let w4 = window_handle(4);
    let buffer = buffer_handle(1);
    let geometry = Geometry::new(0, 0, 9, 7).unwrap();
    let mut layout = Layout::new(w1, WindowState::new(buffer, position(1, 0)), geometry).unwrap();

    layout
        .split_vertical(w1, w2, WindowState::new(buffer, position(1, 0)))
        .unwrap();
    layout
        .split_horizontal(w2, w3, WindowState::new(buffer, position(1, 0)))
        .unwrap();
    layout
        .split_vertical(w3, w4, WindowState::new(buffer, position(1, 0)))
        .unwrap();

    assert_eq!(
        layout.window_geometry(w1).unwrap(),
        Geometry::new(0, 0, 5, 7).unwrap()
    );
    assert_eq!(
        layout.window_geometry(w2).unwrap(),
        Geometry::new(0, 5, 4, 4).unwrap()
    );
    assert_eq!(
        layout.window_geometry(w3).unwrap(),
        Geometry::new(4, 5, 2, 3).unwrap()
    );
    assert_eq!(
        layout.window_geometry(w4).unwrap(),
        Geometry::new(4, 7, 2, 3).unwrap()
    );
    assert_eq!(layout.winnr(w1).unwrap(), 1);
    assert_eq!(layout.winnr(w2).unwrap(), 2);
    assert_eq!(layout.winnr(w3).unwrap(), 3);
    assert_eq!(layout.winnr(w4).unwrap(), 4);

    // A vertical split of a window inside a Column nested under a root Row
    // must create a Row around that target within the Column, not insert the
    // new window into the root Row.
    match layout.root() {
        Frame::Row { children, .. } => {
            assert_eq!(children.len(), 2);
            assert!(matches!(&children[0], Frame::Leaf(leaf) if leaf.window == w1));
            match &children[1] {
                Frame::Column { children, .. } => {
                    assert_eq!(children.len(), 2);
                    assert!(matches!(&children[0], Frame::Leaf(leaf) if leaf.window == w2));
                    match &children[1] {
                        Frame::Row { children, .. } => {
                            assert_eq!(children.len(), 2);
                            assert!(matches!(&children[0], Frame::Leaf(leaf) if leaf.window == w3));
                            assert!(matches!(&children[1], Frame::Leaf(leaf) if leaf.window == w4));
                        }
                        _ => panic!("expected Row wrapping w3 and w4"),
                    }
                }
                _ => panic!("expected Column wrapping w2 and the new Row"),
            }
        }
        _ => panic!("expected root Row"),
    }
}

#[test]
fn same_axis_split_three_containers_deep_joins_its_immediate_parent() {
    // The root-to-leaf child path used to be collected leaf-first, so from the
    // third container down the descent followed the wrong branch and reached a
    // leaf where a container was required. test_window_cmd.vim crashed the
    // process there. Upstream `win_split_ins` inserts the new frame next to
    // the target inside the target's own parent row.
    let w1 = window_handle(1);
    let w2 = window_handle(2);
    let w3 = window_handle(3);
    let w4 = window_handle(4);
    let w5 = window_handle(5);
    let buffer = buffer_handle(1);
    let geometry = Geometry::new(0, 0, 9, 7).unwrap();
    let mut layout = Layout::new(w1, WindowState::new(buffer, position(1, 0)), geometry).unwrap();

    layout
        .split_vertical(w1, w2, WindowState::new(buffer, position(1, 0)))
        .unwrap();
    layout
        .split_horizontal(w2, w3, WindowState::new(buffer, position(1, 0)))
        .unwrap();
    layout
        .split_vertical(w3, w4, WindowState::new(buffer, position(1, 0)))
        .unwrap();
    layout
        .split_vertical(w3, w5, WindowState::new(buffer, position(1, 0)))
        .unwrap();

    match layout.root() {
        Frame::Row { children, .. } => match &children[1] {
            Frame::Column { children, .. } => match &children[1] {
                Frame::Row { children, .. } => {
                    assert_eq!(children.len(), 3);
                    assert!(matches!(&children[0], Frame::Leaf(leaf) if leaf.window == w3));
                    assert!(matches!(&children[1], Frame::Leaf(leaf) if leaf.window == w5));
                    assert!(matches!(&children[2], Frame::Leaf(leaf) if leaf.window == w4));
                }
                _ => panic!("expected the Row holding w3 to absorb the same-axis split"),
            },
            _ => panic!("expected Column wrapping w2 and the nested Row"),
        },
        _ => panic!("expected root Row"),
    }
    assert_eq!(layout.winnr(w3).unwrap(), 3);
    assert_eq!(layout.winnr(w5).unwrap(), 4);
    assert_eq!(layout.winnr(w4).unwrap(), 5);
}

#[test]
fn failed_window_creation_on_unloaded_buffer_is_atomic() {
    let mut editor = Editor::new();
    let loaded = editor.create_buffer(true).unwrap();
    let unloaded = editor.create_buffer(true).unwrap();
    editor.unload_buffer(unloaded).unwrap();
    let tab = editor
        .create_tabpage(loaded, Geometry::new(0, 0, 20, 10).unwrap())
        .unwrap();
    let current = editor.tabpage(tab).unwrap().current_window();
    let tiled_count = editor.tabpage(tab).unwrap().layout().window_count();
    let float_count = editor.tabpage(tab).unwrap().floating_windows().len();

    assert!(editor.split_vertical(tab, current, unloaded, true).is_err());
    let config = WinConfig::new(RelativeTo::Editor, Anchor::NorthWest, 0.0, 0.0, 3, 2).unwrap();
    assert!(editor.open_float(tab, unloaded, config).is_err());
    assert_eq!(
        editor.tabpage(tab).unwrap().layout().window_count(),
        tiled_count
    );
    assert_eq!(
        editor.tabpage(tab).unwrap().floating_windows().len(),
        float_count
    );
    assert_eq!(editor.buffer(unloaded).unwrap().attachments, 0);
}

#[test]
fn floats_are_stably_sorted_by_zindex() {
    let buffer = buffer_handle(1);
    let tiled = window_handle(1);
    let layout = Layout::new(
        tiled,
        WindowState::new(buffer, position(1, 0)),
        Geometry::new(0, 0, 20, 10).unwrap(),
    )
    .unwrap();
    let mut tab = TabpageState::new(layout);
    for (window, zindex) in [
        (window_handle(2), 80),
        (window_handle(3), 20),
        (window_handle(4), 80),
    ] {
        let mut config =
            WinConfig::new(RelativeTo::Editor, Anchor::NorthWest, 0.0, 0.0, 4, 2).unwrap();
        config.zindex = zindex;
        tab.add_float(window, WindowState::new(buffer, position(1, 0)), config)
            .unwrap();
    }
    let ordered: Vec<_> = tab.floating_windows().map(|float| float.window).collect();
    assert_eq!(
        ordered,
        vec![window_handle(3), window_handle(2), window_handle(4)]
    );
}

#[test]
fn generated_option_table_matches_authoritative_source_count() {
    let source_path = std::env::var("OXVIM_REF_ROOT").map_or_else(
        |_| {
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../codegen/upstream/options.lua"
            )
            .to_owned()
        },
        |root| format!("{root}/src/nvim/options.lua"),
    );
    let source = fs::read_to_string(source_path).unwrap();
    let source_count = source
        .lines()
        .filter(|line| line.starts_with("      full_name = "))
        .count();
    assert_eq!(OPTION_COUNT, source_count);
    assert_eq!(OPTION_METADATA.len(), source_count);
    assert_eq!(source_count, 378);
}

#[test]
fn option_defaults_aliases_scopes_and_validation_work() {
    let mut store = OptionStore::new();
    let buffer = buffer_handle(7);
    let window = window_handle(9);
    assert_eq!(
        store.get_global("background").unwrap(),
        &OptionValue::String("dark".into())
    );
    assert_eq!(
        store.get_global("shell").unwrap(),
        &OptionValue::String("sh".into())
    );
    assert_eq!(
        store.get_buffer(buffer, "ts").unwrap(),
        &OptionValue::Number(8)
    );

    store
        .set_buffer(buffer, "tabstop", OptionValue::Number(4))
        .unwrap();
    assert_eq!(
        store.get_buffer(buffer, "ts").unwrap(),
        &OptionValue::Number(4)
    );
    store
        .set_window(window, "number", OptionValue::Boolean(true))
        .unwrap();
    assert_eq!(
        store.get_window(window, "nu").unwrap(),
        &OptionValue::Boolean(true)
    );

    assert!(matches!(
        store.set_buffer(buffer, "tabstop", OptionValue::String("4".into())),
        Err(OptionError::TypeMismatch { .. })
    ));
    assert!(matches!(
        store.set_global("tabstop", OptionValue::Number(2)),
        Err(OptionError::WrongScope { .. })
    ));
    assert!(
        store
            .set_buffer(buffer, "formatoptions", OptionValue::String("tt".into()))
            .is_err()
    );
}

#[test]
fn editor_register_put_updates_text_undo_ticks_marks_and_changes() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(
            Buffer::from_lines(&[b"one".to_vec(), b"two".to_vec()], false).unwrap(),
            true,
        )
        .unwrap();
    editor.set_local_mark(buffer, 'a', position(2, 0)).unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 20, 10).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(2, 1)).unwrap();
    editor.set_window_topline(window, 2).unwrap();
    editor
        .registers_mut()
        .set(
            'a',
            RegisterContent::linewise(vec![b"inserted".to_vec()]).unwrap(),
        )
        .unwrap();

    let content = editor.registers().get('a').unwrap().cloned().unwrap();
    editor
        .put_content(buffer, position(1, 0), &content, 10)
        .unwrap();
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.text().unwrap().line(2).unwrap(), b"inserted");
    assert_eq!(state.text().unwrap().line(3).unwrap(), b"two");
    assert_eq!(state.changedtick(), 1);
    assert_eq!(state.changedtick_diag, 1);
    assert_eq!(state.changedtick_fold, 1);
    assert_eq!(state.undo.current_seq(), 1);
    assert_eq!(
        editor.local_mark(buffer, 'a').unwrap(),
        Some(position(3, 0))
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(3, 1));
    // Linewise put after line 1 inserts "inserted" at line 2; the topline
    // was 2, and the insertion is exactly at the topline, so the new line
    // is displayed (topline stays at 2), matching `mark_adjust_buf` +
    // `update_topline` in Neovim.
    assert_eq!(editor.window(window).unwrap().topline, 2);
    assert_eq!(editor.changelists().len(buffer), 1);
}

#[test]
fn undo_redo_replay_through_ticks_marks_and_winpos() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(
            Buffer::from_lines(
                &[
                    b"one".to_vec(),
                    b"two".to_vec(),
                    b"three".to_vec(),
                    b"four".to_vec(),
                ],
                false,
            )
            .unwrap(),
            true,
        )
        .unwrap();
    editor.set_local_mark(buffer, 'a', position(4, 0)).unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 20, 10).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(4, 1)).unwrap();
    editor.set_window_topline(window, 4).unwrap();

    editor
        .replace_buffer_lines(crate::LineReplaceRequest {
            buffer,
            start: 2,
            end: 2,
            lines: &[b"x".to_vec(), b"y".to_vec()],
            cursor_before: position(2, 0),
            cursor_after: position(5, 0),
            timestamp: 10,
        })
        .unwrap();
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.changedtick(), 1);
    assert_eq!(state.changedtick_diag, 1);
    assert_eq!(state.changedtick_fold, 1);
    assert_eq!(
        editor.local_mark(buffer, 'a').unwrap(),
        Some(position(5, 0))
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(5, 1));
    assert_eq!(editor.window(window).unwrap().topline, 5);
    assert_eq!(editor.changelists().len(buffer), 1);

    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.text().unwrap().line(2).unwrap(), b"two");
    assert_eq!(state.text().unwrap().to_bytes(), b"one\ntwo\nthree\nfour");
    assert_eq!(state.changedtick(), 2);
    assert_eq!(state.changedtick_diag, 2);
    assert_eq!(state.changedtick_fold, 2);
    assert_eq!(
        editor.local_mark(buffer, 'a').unwrap(),
        Some(position(4, 0))
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(4, 1));
    assert_eq!(editor.window(window).unwrap().topline, 4);
    assert_eq!(editor.changelists().len(buffer), 2);

    assert_eq!(editor.buffer_redo(buffer).unwrap(), Some(1));
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.text().unwrap().line(2).unwrap(), b"x");
    assert_eq!(state.text().unwrap().line(3).unwrap(), b"y");
    assert_eq!(state.text().unwrap().to_bytes(), b"one\nx\ny\nthree\nfour");
    assert_eq!(state.changedtick(), 3);
    assert_eq!(state.changedtick_diag, 3);
    assert_eq!(state.changedtick_fold, 3);
    assert_eq!(
        editor.local_mark(buffer, 'a').unwrap(),
        Some(position(5, 0))
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(5, 1));
    assert_eq!(editor.window(window).unwrap().topline, 5);
    assert_eq!(editor.changelists().len(buffer), 3);

    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.text().unwrap().to_bytes(), b"one\ntwo\nthree\nfour");
    assert_eq!(editor.buffer_undo(buffer).unwrap(), None);
    assert_eq!(editor.buffer_redo(buffer).unwrap(), Some(1));
}

#[test]
fn registers_rotate_append_and_concatenate() {
    let mut registers = Registers::new();
    let first = RegisterContent::linewise(vec![b"first".to_vec()]).unwrap();
    let second = RegisterContent::linewise(vec![b"second".to_vec()]).unwrap();
    registers.delete(first.clone());
    registers.delete(second.clone());
    assert_eq!(registers.get('1').unwrap(), Some(&second));
    assert_eq!(registers.get('2').unwrap(), Some(&first));

    let yank = RegisterContent::characterwise(b"yank").unwrap();
    registers.yank(yank.clone());
    assert_eq!(registers.get('0').unwrap(), Some(&yank));
    assert_eq!(registers.get('1').unwrap(), Some(&second));

    registers
        .set('a', RegisterContent::characterwise(b"left").unwrap())
        .unwrap();
    registers
        .set('A', RegisterContent::characterwise(b"right").unwrap())
        .unwrap();
    assert_eq!(
        registers.get('a').unwrap().unwrap().to_bytes(),
        b"leftright"
    );
}

#[test]
fn unnamed_register_alias_transitions_through_every_writer() {
    let mut registers = Registers::new();

    // A fresh bank points nowhere: the unnamed register reads empty and the
    // target query answers '"', like `get_register_name(-1)` upstream.
    assert_eq!(registers.unnamed_target_name(), '"');
    assert_eq!(registers.get('"').unwrap(), None);

    // A plain yank lands in register 0 and points the unnamed register at it.
    let first = RegisterContent::linewise(vec![b"first".to_vec()]).unwrap();
    registers.yank(first.clone());
    assert_eq!(registers.unnamed_target_name(), '0');
    assert_eq!(registers.get('0').unwrap(), Some(&first));
    assert_eq!(registers.get('"').unwrap(), Some(&first));

    // Overwriting the target slot is visible through unnamed reads without
    // another pointer update.
    let overwritten = RegisterContent::linewise(vec![b"over".to_vec()]).unwrap();
    registers.set('0', overwritten.clone()).unwrap();
    assert_eq!(registers.get('"').unwrap(), Some(&overwritten));

    // An explicit yank points at the canonical lowercase destination.
    let named = RegisterContent::characterwise(b"named").unwrap();
    registers.yank_to('b', named.clone()).unwrap();
    assert_eq!(registers.unnamed_target_name(), 'b');
    assert_eq!(registers.get('"').unwrap(), Some(&named));

    // Uppercase appends and still points at the lowercase slot.
    let upper = RegisterContent::characterwise(b"!").unwrap();
    registers.yank_to('B', upper).unwrap();
    assert_eq!(registers.unnamed_target_name(), 'b');
    assert_eq!(registers.get('"').unwrap().unwrap().to_bytes(), b"named!");

    // Ordinary named set leaves the pointer alone; overwriting the selected
    // slot changes subsequent unnamed reads.
    registers
        .set('b', RegisterContent::characterwise(b"other").unwrap())
        .unwrap();
    assert_eq!(registers.unnamed_target_name(), 'b');
    assert_eq!(registers.get('"').unwrap().unwrap().to_bytes(), b"other");

    // Small characterwise delete points at `-`; rotating delete points at `1`.
    registers.delete(RegisterContent::characterwise(b"x").unwrap());
    assert_eq!(registers.unnamed_target_name(), '-');
    assert_eq!(registers.get('"').unwrap().unwrap().to_bytes(), b"x");

    registers.delete(RegisterContent::linewise(vec![b"line".to_vec()]).unwrap());
    assert_eq!(registers.unnamed_target_name(), '1');
    assert_eq!(registers.get('"').unwrap().unwrap().to_bytes(), b"line");
    assert_eq!(
        registers.get('"').unwrap().unwrap().getreg_bytes(),
        b"line\n"
    );

    // The black hole discards the write and leaves the pointer unchanged.
    registers
        .yank_to('_', RegisterContent::characterwise(b"gone").unwrap())
        .unwrap();
    assert_eq!(registers.unnamed_target_name(), '1');
    assert_eq!(registers.get('_').unwrap(), None);

    // A direct unnamed set writes physical register 0 and selects it.
    registers
        .set('"', RegisterContent::characterwise(b"direct").unwrap())
        .unwrap();
    assert_eq!(registers.get('0').unwrap().unwrap().to_bytes(), b"direct");
    assert_eq!(registers.unnamed_target_name(), '0');

    // `setreg`'s unnamed flag re-points without copying content.
    registers
        .set_from_setreg('z', RegisterContent::characterwise(b"flag").unwrap(), false)
        .unwrap();
    registers.set_unnamed_target('z');
    assert_eq!(registers.unnamed_target_name(), 'z');
    assert_eq!(registers.get('"').unwrap().unwrap().to_bytes(), b"flag");

    // Clipboard yank keeps a provider snapshot and the selection name.
    let mut provider = TestClipboard;
    let clip = RegisterContent::characterwise(b"clip").unwrap();
    registers
        .yank_to_with_clipboard('+', clip.clone(), &mut provider)
        .unwrap();
    assert_eq!(registers.unnamed_target_name(), '+');
    assert_eq!(registers.get('"').unwrap(), Some(&clip));

    // `set_with_clipboard` on a selection does not move the pointer.
    registers
        .set_with_clipboard(
            '*',
            RegisterContent::characterwise(b"star").unwrap(),
            &mut provider,
        )
        .unwrap();
    assert_eq!(registers.unnamed_target_name(), '+');

    // Non-pointable and unsupported `set_unnamed_target` names are ignored.
    registers.set_unnamed_target('/');
    assert_eq!(registers.unnamed_target_name(), '+');
    registers.set_unnamed_target('!');
    assert_eq!(registers.unnamed_target_name(), '+');
}

struct TestClipboard;

impl ClipboardProvider for TestClipboard {
    fn set(
        &mut self,
        _selection: Selection,
        _content: &RegisterContent,
    ) -> Result<(), RegisterError> {
        Ok(())
    }
}

#[test]
fn jump_and_change_histories_truncate_and_stay_buffer_local() {
    let buffer1 = buffer_handle(1);
    let buffer2 = buffer_handle(2);
    let mut jumps = Jumplist::new();
    for line in 1..=3 {
        jumps.push(MarkLocation::in_buffer(buffer1, position(line, 0)));
    }
    assert_eq!(jumps.backward().unwrap().position, position(3, 0));
    assert_eq!(jumps.backward().unwrap().position, position(2, 0));
    jumps.push(MarkLocation::in_buffer(buffer1, position(9, 0)));
    let lines: Vec<_> = jumps
        .entries()
        .iter()
        .map(|entry| entry.position.lnum)
        .collect();
    assert_eq!(lines, vec![1, 2, 9]);

    let mut changes = Changelists::new();
    changes.push(buffer1, position(2, 0));
    changes.push(buffer2, position(8, 0));
    changes.push(buffer1, position(4, 0));
    assert_eq!(changes.len(buffer1), 2);
    assert_eq!(changes.len(buffer2), 1);
    assert_eq!(changes.backward(buffer1), Some(position(4, 0)));
    changes.splice_buffer(buffer1, 1, 0, 2);
    assert_eq!(changes.entries(buffer1).unwrap()[0], position(4, 0));
    assert_eq!(changes.entries(buffer2).unwrap()[0], position(8, 0));
}

#[test]
fn local_mark_name_validation_is_exact() {
    let mut marks = LocalMarks::new();
    marks.set('a', position(1, 0)).unwrap();
    marks.set('^', position(2, 0)).unwrap();
    assert!(marks.set('A', position(1, 0)).is_err());
    assert!(marks.set('0', position(1, 0)).is_err());
}

// runtime.c runtimepath_default — the startup 'runtimepath' layout: XDG
// config entries, XDG data site entries, the runtime tree, then the
// mirrored after entries in reverse order.
#[test]
fn runtimepath_default_layout_matches_upstream_order() {
    let config_dirs = vec!["/etc/one".to_owned(), "/etc/two".to_owned()];
    let data_dirs = vec!["/d/one".to_owned(), "/d/two".to_owned()];
    let built = crate::script::build_runtimepath(
        Some("/cfg"),
        &config_dirs,
        Some("/dat"),
        &data_dirs,
        std::path::Path::new("/rt/"),
    );
    assert_eq!(
        built,
        "/cfg/nvim,\
         /etc/one/nvim,/etc/two/nvim,\
         /dat/nvim/site,/d/one/nvim/site,/d/two/nvim/site,\
         /rt,\
         /d/two/nvim/site/after,/d/one/nvim/site/after,/dat/nvim/site/after,\
         /etc/two/nvim/after,/etc/one/nvim/after,/cfg/nvim/after"
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(",")
    );
    // --clean shape: no XDG entries, only the runtime tree.
    assert_eq!(
        crate::script::build_runtimepath(None, &[], None, &[], std::path::Path::new("/rt")),
        "/rt"
    );
}

// runtime.c do_in_runtimepath — runtime search roots follow the
// 'runtimepath' value, skipping empty entries.
#[test]
fn runtime_roots_follow_runtimepath_entries() {
    let mut context =
        crate::script::ScriptCtx::<crate::script::RealFileIO>::new(crate::script::RealFileIO);
    context.set_runtime_roots_from_rtp("/first,,/second,");
    let roots: Vec<&std::path::Path> = context
        .runtime_roots()
        .iter()
        .map(super::script::RuntimeRoot::path)
        .collect();
    assert_eq!(
        roots,
        vec![
            std::path::Path::new("/first"),
            std::path::Path::new("/second")
        ]
    );
}

#[test]
fn replace_buffer_text_out_of_range_leaves_state_unchanged() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_lines(&[b"abc".to_vec()], false).unwrap(), true)
        .unwrap();
    let before = editor.buffer(buffer).unwrap().text().unwrap().to_bytes();
    let err = editor
        .replace_buffer_text(
            buffer,
            &crate::buffer::BufferTextEditRequest {
                start: crate::extmark::ExtmarkPosition::new(0, usize::MAX),
                end: crate::extmark::ExtmarkPosition::new(0, usize::MAX),
                replacement: vec![b"X".to_vec()],
            },
            position(1, 0),
            position(1, 0),
            0,
        )
        .unwrap_err();
    assert!(matches!(
        err,
        crate::editor::EditorError::Buffer(crate::buffer::BufferStateError::TextEdit(
            crate::buffer::BufferTextEditError::OutOfRange
        ))
    ));
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
        before
    );
}

#[test]
fn extmark_splice_is_total_for_unbounded_positions() {
    let mut marks = crate::Extmarks::new();
    let namespace = marks.create_namespace("total").unwrap();
    let id = marks
        .set(
            namespace,
            None,
            crate::extmark::ExtmarkPlacement::new(crate::extmark::ExtmarkPosition::new(
                usize::MAX / 2,
                usize::MAX / 2,
            )),
        )
        .unwrap();
    marks.splice(crate::extmark::TextSplice {
        start: crate::extmark::ExtmarkPosition::new(0, 0),
        old_extent: crate::extmark::TextExtent::EMPTY,
        new_extent: crate::extmark::TextExtent::new(3, 2),
    });
    let (_, undo) = marks.splice_recording(crate::extmark::TextSplice {
        start: crate::extmark::ExtmarkPosition::new(usize::MAX / 4, usize::MAX / 4),
        old_extent: crate::extmark::TextExtent::EMPTY,
        new_extent: crate::extmark::TextExtent::new(0, usize::MAX / 8),
    });
    marks.undo_splice(&undo);
    marks.redo_splice(&undo);
    let position = marks.get(namespace, id).unwrap().unwrap().position();
    assert!(position.row >= usize::MAX / 2);
    assert!(position.column >= usize::MAX / 2 || position.row > usize::MAX / 2);
}

// ──────────────────────────────────────────────────────────────────
// Topline-follows-content tests (nvim_buf_set_lines / nvim_buf_set_text)
// ──────────────────────────────────────────────────────────────────

/// Creates an editor with a single window showing `lines`, with the window
/// cursor at `cursor` and topline at `topline`. The `height` parameter is the
/// desired text-row height; the geometry is sized `height + 1` to account for
/// the command-line row that `tiled_window_text_height` subtracts.
fn editor_with_scrolled_window(
    lines: &[&[u8]],
    cursor: Position,
    topline: usize,
    height: usize,
) -> (Editor, BufHandle, WinHandle) {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(
            Buffer::from_lines(&lines.iter().map(|l| l.to_vec()).collect::<Vec<_>>(), false)
                .unwrap(),
            true,
        )
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 20, height + 1).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, cursor).unwrap();
    editor.set_window_topline(window, topline).unwrap();
    (editor, buffer, window)
}

/// Replaces lines [start, end] (1-based, inclusive) with `replacement` via
/// `replace_buffer_lines`, exercising the line-splice topline path. When
/// `start > end` (pure insertion before `start`), uses `append_buffer_lines`
/// to match the `nvim_buf_set_lines` dispatch.
fn replace_lines(
    editor: &mut Editor,
    buffer: BufHandle,
    start: usize,
    end: usize,
    replacement: &[&[u8]],
) {
    let replacement_vec: Vec<Vec<u8>> = replacement.iter().map(|l| l.to_vec()).collect();
    if start > end {
        editor
            .append_buffer_lines(buffer, start - 1, &replacement_vec, position(start, 0), 10)
            .unwrap();
    } else {
        editor
            .replace_buffer_lines(crate::LineReplaceRequest {
                buffer,
                start,
                end,
                lines: &replacement_vec,
                cursor_before: position(start, 0),
                cursor_after: position(start, 0),
                timestamp: 10,
            })
            .unwrap();
    }
}

#[test]
fn topline_follows_content_when_editing_above_topline() {
    // Buffer: aaa bbb ccc ddd www xxx yyy zzz; topline=www(5), cursor=zzz(8).
    let (mut editor, buffer, window) = editor_with_scrolled_window(
        &[
            b"aaa", b"bbb", b"ccc", b"ddd", b"www", b"xxx", b"yyy", b"zzz",
        ],
        position(8, 0),
        5,
        4,
    );
    // Replace lines 1-2 (aaa, bbb) with one line (aaabbb): topline 5→4.
    replace_lines(&mut editor, buffer, 1, 2, &[b"aaabbb"]);
    assert_eq!(editor.window(window).unwrap().topline, 4);
    assert_eq!(editor.window(window).unwrap().cursor, position(7, 0));
}

#[test]
fn topline_stays_when_replacing_topline_line() {
    let (mut editor, buffer, window) = editor_with_scrolled_window(
        &[b"aaabbb", b"ccc", b"ddd", b"www", b"xxx", b"yyy", b"zzz"],
        position(7, 0),
        4,
        4,
    );
    // Replace topline (www at line 4) with wwweeee: topline stays at 4.
    replace_lines(&mut editor, buffer, 4, 4, &[b"wwweeee"]);
    assert_eq!(editor.window(window).unwrap().topline, 4);
    assert_eq!(editor.window(window).unwrap().cursor, position(7, 0));
}

#[test]
fn topline_follows_content_when_inserting_at_topline_cursor_offscreen() {
    let (mut editor, buffer, window) = editor_with_scrolled_window(
        &[
            b"aaabbb", b"ccc", b"ddd", b"wwweeee", b"xxx", b"yyy", b"zzz",
        ],
        position(7, 0),
        4,
        4,
    );
    // Insert mmm before topline (line 4): cursor shifts to 8, which is off-
    // screen (topline=4, height=4, bottom=7), so update_topline scrolls
    // topline to 5 to keep the cursor visible.
    replace_lines(&mut editor, buffer, 4, 3, &[b"mmm"]);
    assert_eq!(editor.window(window).unwrap().topline, 5);
    assert_eq!(editor.window(window).unwrap().cursor, position(8, 0));
}

#[test]
fn topline_stays_when_inserting_at_topline_cursor_visible() {
    let (mut editor, buffer, window) = editor_with_scrolled_window(
        &[
            b"aaabbb", b"ccc", b"ddd", b"mmm", b"wwweeee", b"xxx", b"yyy", b"zzz",
        ],
        position(7, 0),
        5,
        4,
    );
    // Insert mmmeeeee before topline (line 5): cursor shifts to 8, which is
    // visible (topline=5, height=4, bottom=8), so topline stays at 5 and the
    // new line is displayed.
    replace_lines(&mut editor, buffer, 5, 4, &[b"mmmeeeee"]);
    assert_eq!(editor.window(window).unwrap().topline, 5);
    assert_eq!(editor.window(window).unwrap().cursor, position(8, 0));
}

#[test]
fn topline_clamps_above_splice_on_pure_deletion() {
    // Delete lines 4-6 (ddd, www, xxx) around topline=5.
    let (mut editor, buffer, window) = editor_with_scrolled_window(
        &[
            b"aaa", b"bbb", b"ccc", b"ddd", b"www", b"xxx", b"yyy", b"zzz",
        ],
        position(7, 0),
        5,
        4,
    );
    replace_lines(&mut editor, buffer, 4, 6, &[]);
    assert_eq!(editor.window(window).unwrap().topline, 3);
    assert_eq!(editor.window(window).unwrap().cursor, position(4, 0));
}

#[test]
fn topline_unchanged_when_deleting_just_before_topline() {
    let (mut editor, buffer, window) = editor_with_scrolled_window(
        &[
            b"aaa", b"bbb", b"ccc", b"ddd", b"www", b"xxx", b"yyy", b"zzz",
        ],
        position(7, 0),
        5,
        4,
    );
    // Delete 1 line just before topline (line 4 = ddd).
    replace_lines(&mut editor, buffer, 4, 4, &[]);
    assert_eq!(editor.window(window).unwrap().topline, 4);
    assert_eq!(editor.window(window).unwrap().cursor, position(6, 0));
}

#[test]
fn topline_unchanged_when_deleting_far_before_topline() {
    let (mut editor, buffer, window) = editor_with_scrolled_window(
        &[
            b"aaa", b"bbb", b"ccc", b"ddd", b"www", b"xxx", b"yyy", b"zzz",
        ],
        position(7, 0),
        5,
        4,
    );
    // Delete 3 lines far before topline (lines 1-3).
    replace_lines(&mut editor, buffer, 1, 3, &[]);
    assert_eq!(editor.window(window).unwrap().topline, 2);
    assert_eq!(editor.window(window).unwrap().cursor, position(4, 0));
}

#[test]
fn topline_shifts_when_replacing_just_before_topline_with_more_lines() {
    let (mut editor, buffer, window) = editor_with_scrolled_window(
        &[
            b"aaa", b"bbb", b"ccc", b"ddd", b"www", b"xxx", b"yyy", b"zzz",
        ],
        position(7, 0),
        5,
        4,
    );
    // Replace 1 line just before topline (ddd) with 2 lines (eee, fff).
    replace_lines(&mut editor, buffer, 4, 4, &[b"eee", b"fff"]);
    assert_eq!(editor.window(window).unwrap().topline, 6);
    assert_eq!(editor.window(window).unwrap().cursor, position(8, 0));
}

#[test]
fn topline_in_split_windows_both_adjusted() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(
            Buffer::from_lines(
                &[
                    b"aaa", b"bbb", b"ccc", b"ddd", b"www", b"xxx", b"yyy", b"zzz",
                ]
                .iter()
                .map(|l| l.to_vec())
                .collect::<Vec<_>>(),
                false,
            )
            .unwrap(),
            true,
        )
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 20, 12).unwrap())
        .unwrap();
    let top_win = editor.tabpage(tab).unwrap().current_window();
    // Split: top window shows from line 1, bottom from line 5.
    editor.set_window_cursor(top_win, position(1, 0)).unwrap();
    let bottom_win = editor.split_horizontal(tab, top_win, buffer, true).unwrap();
    editor
        .set_window_cursor(bottom_win, position(8, 0))
        .unwrap();
    editor.set_window_topline(bottom_win, 5).unwrap();

    // Replace lines 1-2 (aaa, bbb) with one line (aaabbb).
    replace_lines(&mut editor, buffer, 1, 2, &[b"aaabbb"]);

    // Top window: topline stays at 1 (edit is at topline, replacement).
    assert_eq!(editor.window(top_win).unwrap().topline, 1);
    // Bottom window: topline shifts 5→4 (content follows).
    assert_eq!(editor.window(bottom_win).unwrap().topline, 4);
}

// ──────────────────────────────────────────────────────────────────
// Cursor adjustment tests (nvim_buf_set_text / nvim_buf_set_lines)
// Mirrors buffer_spec.lua: cursor above/inside/below × visible/hidden
// × NORMAL/INSERT/virtualedit, matching fix_pos_col (api/buffer.c:1304).
// ──────────────────────────────────────────────────────────────────

/// Creates an editor with a single window showing `lines`, cursor at
/// `cursor`. Geometry is 80×24.
fn editor_with_text(lines: &[&[u8]], cursor: Position) -> (Editor, BufHandle, WinHandle) {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(
            Buffer::from_lines(&lines.iter().map(|l| l.to_vec()).collect::<Vec<_>>(), false)
                .unwrap(),
            true,
        )
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, cursor).unwrap();
    (editor, buffer, window)
}

/// Replaces a byte range via `replace_buffer_text`, exercising the
/// `splice_text_positions` cursor-adjustment path (`nvim_buf_set_text`).
fn set_text(
    editor: &mut Editor,
    buffer: BufHandle,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
    replacement: &[&[u8]],
) {
    editor
        .replace_buffer_text(
            buffer,
            &crate::buffer::BufferTextEditRequest {
                start: crate::extmark::ExtmarkPosition::new(start_row, start_col),
                end: crate::extmark::ExtmarkPosition::new(end_row, end_col),
                replacement: replacement.iter().map(|l| l.to_vec()).collect(),
            },
            position(start_row + 1, start_col),
            position(start_row + 1, start_col),
            10,
        )
        .unwrap();
}

// ── #22526: text added right at cursor position ──

#[test]
fn set_text_at_cursor_normal_mode_shifts_cursor_right() {
    // buffer_spec.lua: "updates the cursor position in NORMAL mode"
    // Buffer "abcd", cursor on 'c' (col 2), insert "xxx" at (0,2).
    // NORMAL: cursor sits on a char, so col+1 > end_col → "after range"
    // path shifts cursor by the replacement delta.
    let (mut editor, buffer, window) = editor_with_text(&[b"abcd"], position(1, 2));
    set_text(&mut editor, buffer, 0, 2, 0, 2, &[b"xxx"]);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 5));
}

#[test]
fn set_text_at_cursor_insert_mode_keeps_cursor() {
    // buffer_spec.lua: "updates the cursor position only in non-current
    // window when in INSERT mode"
    // INSERT: cursor is between chars, col+0 > end_col is false → cursor
    // stays put in the current window.
    let (mut editor, buffer, window) = editor_with_text(&[b"abcd"], position(1, 2));
    editor.set_edit_mode(crate::BufferEditMode::Insert);
    set_text(&mut editor, buffer, 0, 2, 0, 2, &[b"xxx"]);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 2));
}

#[test]
fn set_text_at_cursor_normal_mode_non_current_window_shifts() {
    // buffer_spec.lua: same test, non-current window should shift.
    // Split, put cursor in both at col 2, edit from the second window.
    let (mut editor, buffer, win1) = editor_with_text(&[b"abcd"], position(1, 2));
    let tab = editor.current_tabpage().unwrap();
    let win2 = editor.split_horizontal(tab, win1, buffer, true).unwrap();
    editor.set_window_cursor(win2, position(1, 2)).unwrap();
    // Edit from win2 (current), NORMAL mode. win1 is non-current → NORMAL.
    set_text(&mut editor, buffer, 0, 2, 0, 2, &[b"xxx"]);
    assert_eq!(editor.window(win1).unwrap().cursor, position(1, 5));
    assert_eq!(editor.window(win2).unwrap().cursor, position(1, 5));
}

#[test]
fn set_text_at_cursor_insert_mode_non_current_window_shifts() {
    // buffer_spec.lua: INSERT in current window, non-current window is
    // always NORMAL → non-current cursor shifts.
    let (mut editor, buffer, win1) = editor_with_text(&[b"abcd"], position(1, 2));
    let tab = editor.current_tabpage().unwrap();
    let win2 = editor.split_horizontal(tab, win1, buffer, true).unwrap();
    editor.set_window_cursor(win2, position(1, 2)).unwrap();
    editor.set_edit_mode(crate::BufferEditMode::Insert);
    set_text(&mut editor, buffer, 0, 2, 0, 2, &[b"xxx"]);
    // Current window (win2, INSERT): cursor stays
    assert_eq!(editor.window(win2).unwrap().cursor, position(1, 2));
    // Non-current window (win1, treated as NORMAL): cursor shifts
    assert_eq!(editor.window(win1).unwrap().cursor, position(1, 5));
}

// ── cursor inside replaced range ──

#[test]
fn set_text_cursor_inside_range_normal_clamps_to_end_minus_one() {
    // buffer_spec.lua: "adjusts cursor line and column to keep it inside
    // replacement range"
    // Cursor on 'n' in 'finally' (row 2, col 6), replace rows 0-2 cols
    // 15-11 with 2 lines. Cursor collapses to replacement end - 1 (NORMAL).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(3, 6),
    );
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"the line we do not want", b"but hopefully"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(2, 12));
}

#[test]
fn set_text_cursor_inside_range_insert_clamps_to_end() {
    // buffer_spec.lua: "adjusts cursor column if replacement ends at cursor
    // row, at cursor column in INSERT mode"
    // Same geometry, INSERT mode → cursor at replacement end (not end-1).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(3, 10),
    );
    editor.set_edit_mode(crate::BufferEditMode::Insert);
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"1", b"this 2", b"and then"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(3, 8));
}

#[test]
fn set_text_cursor_inside_range_normal_at_end_row_after_col() {
    // buffer_spec.lua: "adjusts cursor column if replacement ends at cursor
    // row, after cursor column"
    // Cursor at (2,10), replacement ends at (2,11). NORMAL: col+1 > 11
    // is false (11 > 11), so cursor goes through inside-range path,
    // collapses to change_end - 1 = 7.
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(3, 10),
    );
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"1", b"this 2", b"and then"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(3, 7));
}

#[test]
fn set_text_single_line_interior_edit_normal() {
    // buffer_spec.lua: "adjusts cursor column if replacement is inside of
    // a single line"
    // Cursor at (2,10), replace cols 4-11 on row 2 with "then".
    // change_start=4, change_end=8. Cursor 10 > 8 → clamp to 8-1=7 (NORMAL).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(3, 10),
    );
    set_text(&mut editor, buffer, 2, 4, 2, 11, &[b"then"]);
    assert_eq!(editor.window(window).unwrap().cursor, position(3, 7));
}

#[test]
fn set_text_cursor_after_range_on_end_row_shifts() {
    // buffer_spec.lua: "updates the cursor position"
    // Cursor at (0,11) on '!', replace cols 6-11 with "foo" (3 bytes).
    // NORMAL: col+1 > end_col (12 > 11) → after-range path, shift by
    // change_end - end_col = 9 - 11 = -2 → col = 9.
    let (mut editor, buffer, window) = editor_with_text(&[b"hello world!"], position(1, 11));
    set_text(&mut editor, buffer, 0, 6, 0, 11, &[b"foo"]);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 9));
}

#[test]
fn set_text_empty_replacement_clamps_to_start() {
    // buffer_spec.lua: "adjusts cursor line and column if replacement is
    // empty and start_col == 0"
    // Cursor at (1,8), replace rows 0-2 cols 0-4 with empty.
    // Cursor collapses to row 0, col 0.
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(2, 8),
    );
    set_text(&mut editor, buffer, 0, 0, 2, 4, &[]);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 0));
}

// ── virtualedit tests ──

#[test]
fn set_text_virtualedit_inside_range_not_after_eol_clamps_to_end_minus_one() {
    // buffer_spec.lua: "adjusts cursor line/col to keep inside replacement
    // range if not after eol"
    // Cursor at (1,34) on 't' in 'want', virtualedit=all, coladd=0.
    // Replace rows 0-2 cols 15-11 with 2 lines. Cursor collapses to
    // (1,12) with coladd=0 (NORMAL, old_coladd==0).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(2, 34),
    );
    editor.window_mut(window).unwrap().coladd = 0;
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"the line we do not want", b"but hopefully"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(2, 12));
    assert_eq!(editor.window(window).unwrap().coladd, 0);
}

#[test]
fn set_text_virtualedit_after_eol_row_shorter_preserves_screen_col() {
    // buffer_spec.lua: "does not change cursor screen column when cursor
    // >EOL and row got shorter"
    // Cursor at (1,34) with coladd=5 (screen col 39). After edit, new row
    // "but hopefully the last one" has len 26. Cursor at (1,26) with
    // coladd=13 (screen col 39 preserved).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(2, 34),
    );
    editor.window_mut(window).unwrap().coladd = 5;
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"the line we do not want", b"but hopefully"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(2, 26));
    assert_eq!(editor.window(window).unwrap().coladd, 13);
}

#[test]
fn set_text_virtualedit_after_eol_row_longer_preserves_screen_col() {
    // buffer_spec.lua: "does not change cursor screen column when cursor
    // is after eol and row got longer"
    // Cursor at (0,19) with coladd=21 (screen col 40). After edit, new
    // row 0 "This should be the line we do not want" has len 38.
    // Cursor at (0,38) with coladd=2 (screen col 40 preserved).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(1, 19),
    );
    editor.window_mut(window).unwrap().coladd = 21;
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"the line we do not want", b"but hopefully"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 38));
    assert_eq!(editor.window(window).unwrap().coladd, 2);
}

#[test]
fn set_text_virtualedit_after_eol_small_coladd_row_extends_past() {
    // buffer_spec.lua: "does not change cursor screen column when cursor
    // is after eol and row extended past cursor column"
    // Cursor at (0,19) with coladd=3 (screen col 22). After edit, row 0
    // has len 38 > 22. Cursor stays at (0,22) with coladd=0.
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(1, 19),
    );
    editor.window_mut(window).unwrap().coladd = 3;
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"the line we do not want", b"but hopefully"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 22));
    assert_eq!(editor.window(window).unwrap().coladd, 0);
}

// ── set_lines on invisible/hidden buffers ──

#[test]
fn set_lines_on_invisible_buffer_does_not_move_cursor() {
    // buffer_spec.lua: "set_lines of invisible buffer doesn't move cursor
    // in current window"
    // Create a buffer not shown in any window, set its lines, verify the
    // current window's cursor is unchanged.
    let (mut editor, visible, window) = editor_with_text(
        &[b"Who would win?", b"A real window", b"with proper text"],
        position(3, 15),
    );
    let invisible = editor.create_buffer(false).unwrap();
    // Set lines on the invisible buffer
    editor
        .replace_buffer_lines(crate::LineReplaceRequest {
            buffer: invisible,
            start: 1,
            end: 1,
            lines: &[b"or some".to_vec(), b"scratchy text".to_vec()],
            cursor_before: position(1, 0),
            cursor_after: position(1, 0),
            timestamp: 10,
        })
        .unwrap();
    // Current window cursor must not have moved
    assert_eq!(editor.window(window).unwrap().cursor, position(3, 15));
    assert_eq!(editor.window(window).unwrap().buffer, visible);
}

#[test]
fn set_lines_on_hidden_buffer_does_not_move_cursor() {
    // buffer_spec.lua: "set_lines on hidden buffer preserves previous
    // window #9741"
    // Create a hidden buffer, set its lines, verify the current window's
    // cursor and buffer are unchanged.
    let (mut editor, visible, window) =
        editor_with_text(&[b"visible buffer line 1", b"line 2"], position(1, 0));
    let hidden = editor.create_buffer(false).unwrap();
    editor
        .replace_buffer_lines(crate::LineReplaceRequest {
            buffer: hidden,
            start: 1,
            end: 1,
            lines: &[b"hidden buffer line 1".to_vec(), b"line 2".to_vec()],
            cursor_before: position(1, 0),
            cursor_after: position(1, 0),
            timestamp: 10,
        })
        .unwrap();
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 0));
    assert_eq!(editor.window(window).unwrap().buffer, visible);
}

// ── deletion at cursor position ──

#[test]
fn set_text_delete_at_cursor_normal_keeps_position() {
    // buffer_spec.lua: "leaves cursor at the same position in NORMAL mode"
    // Cursor on 'b' (col 1), delete 'b' (cols 1-2). NORMAL: col+1 > 2
    // is false (2 > 2), inside-range: col 1 > change_end 1 is false →
    // cursor stays at col 1, now on 'c'.
    let (mut editor, buffer, window) = editor_with_text(&[b"abcd"], position(1, 1));
    set_text(&mut editor, buffer, 0, 1, 0, 2, &[]);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 1));
}

#[test]
fn set_text_delete_at_cursor_insert_keeps_position() {
    // buffer_spec.lua: "maintains INSERT-mode cursor position"
    // INSERT: cursor after 'a' (col 1), delete 'b'. col+0 > 2 is false,
    // inside-range: col 1 > 1 is false → cursor stays at col 1.
    let (mut editor, buffer, window) = editor_with_text(&[b"abcd"], position(1, 1));
    editor.set_edit_mode(crate::BufferEditMode::Insert);
    set_text(&mut editor, buffer, 0, 1, 0, 2, &[]);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 1));
}

// ── cursor at start_row, before start_col ──

#[test]
fn set_text_cursor_before_start_col_on_start_row_stays() {
    // buffer_spec.lua: "maintains cursor position if at start_row, but
    // before start_col"
    // Cursor at (0,14), replace from (0,15) to (2,11). Cursor is before
    // the edit on the same row → stays at (0,14).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(1, 14),
    );
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"the line we do not want", b"but hopefully"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 14));
}

#[test]
fn set_text_cursor_at_start_col_on_start_row_stays() {
    // buffer_spec.lua: "maintains cursor position if at start_row and
    // column is still valid"
    // Cursor at (0,15) = start_col. NORMAL: col+1 > end_col? 16 > 11?
    // No (end_row=2). Inside-range: col 15 > change_end 13? Yes, but
    // only if row == new_end_row. new_end_row = 1, row = 0 → no clamp.
    // Cursor stays at (0,15).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(1, 15),
    );
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"the line we do not want", b"but hopefully"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 15));
}

// ── cursor column clamped to line end ──

#[test]
fn set_text_cursor_clamped_to_line_end_normal() {
    // buffer_spec.lua: "does not move cursor column after end of a line"
    // Cursor at (1,2) on last '!', delete rows 0-1 cols 33-3. Cursor
    // collapses to row 0, then check_cursor_col clamps to len-1 = 32.
    let (mut editor, buffer, window) = editor_with_text(
        &[b"This should be the only line here", b"!!!"],
        position(2, 2),
    );
    set_text(&mut editor, buffer, 0, 33, 1, 3, &[]);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 32));
}

#[test]
fn set_text_cursor_clamped_to_line_start() {
    // buffer_spec.lua: "does not move cursor column before start of a line"
    // Buffer "\n!!!", cursor at (1,2), delete rows 0-1 cols 0-3.
    // Cursor collapses to row 0, col 0.
    let (mut editor, buffer, window) = editor_with_text(&[b"", b"!!!"], position(2, 2));
    set_text(&mut editor, buffer, 0, 0, 1, 3, &[]);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 0));
}

// ── cursor inside range, row after start_row got smaller ──

#[test]
fn set_text_cursor_row_after_start_smaller_normal() {
    // buffer_spec.lua: "adjusts cursor to valid column in row after
    // start_row if it got smaller"
    // Cursor at (1,31) on 'w' in 'want', replace rows 0-2 cols 15-11
    // with 3 lines. Cursor row 1 < new_end_row 2, so no row clamp.
    // Column 31 > line len 6 → check_cursor_col clamps to 5 (NORMAL).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(2, 31),
    );
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"1", b"then 2", b"and then"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(2, 5));
}

#[test]
fn set_text_cursor_row_after_start_smaller_insert() {
    // buffer_spec.lua: "adjusts cursor to valid column in row after
    // start_row if it got smaller in INSERT mode"
    // Same as above but INSERT → check_cursor_col clamps to 6 (len).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(2, 31),
    );
    editor.set_edit_mode(crate::BufferEditMode::Insert);
    set_text(
        &mut editor,
        buffer,
        0,
        15,
        2,
        11,
        &[b"1", b"then 2", b"and then"],
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(2, 6));
}

// ── cursor inside range, start_row got smaller (single line result) ──

#[test]
fn set_text_start_row_smaller_normal_clamps_to_change_end_minus_one() {
    // buffer_spec.lua: "adjusts cursor column to keep it valid if
    // start_row got smaller"
    // Cursor at (0,19) on 't' in 'first', replace rows 0-2 cols 15-24
    // with "last" (4 bytes). change_start=15, change_end=19.
    // Cursor 19 > 19? No → no clamp in adjust. check_cursor_col: 19 >=
    // len 19 → clamp to 18 (NORMAL).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(1, 19),
    );
    set_text(&mut editor, buffer, 0, 15, 2, 24, &[b"last"]);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 18));
}

#[test]
fn set_text_start_row_smaller_insert_clamps_to_change_end() {
    // buffer_spec.lua: "adjusts cursor column to keep it valid if
    // start_row decreased in INSERT mode"
    // Same but INSERT → check_cursor_col clamps to 19 (len, INSERT).
    let (mut editor, buffer, window) = editor_with_text(
        &[
            b"This should be first",
            b"then there is a line we do not want",
            b"and finally the last one",
        ],
        position(1, 19),
    );
    editor.set_edit_mode(crate::BufferEditMode::Insert);
    set_text(&mut editor, buffer, 0, 15, 2, 24, &[b"last"]);
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 19));
}

// ──────────────────────────────────────────────────────────────────
// Split cursor reset tests
// ──────────────────────────────────────────────────────────────────

/// `:new` creates a split showing a fresh empty buffer. The new window's
/// cursor must start at line 1, not inherit the original window's cursor
/// position. Mirrors upstream `win_enter_ext` resetting `w_cursor` for a
/// buffer that has never been displayed.
#[test]
fn split_with_new_buffer_resets_cursor_to_line_one() {
    let (mut editor, _buffer, window) = editor_with_text(
        &[b"line1", b"line2", b"line3", b"line4", b"line5", b"line6"],
        position(6, 0),
    );
    let tab = editor.current_tabpage().unwrap();
    let new_buf = editor.create_buffer(true).unwrap();
    let new_win = editor.split_above(tab, window, new_buf, true).unwrap();
    assert_eq!(
        editor.window(new_win).unwrap().cursor,
        position(1, 0),
        "new buffer window should start at line 1, not inherit cursor from original window"
    );
}

/// `:split` (same buffer) inherits the cursor so both panes show the same
/// view. This is the complementary case to the new-buffer reset.
#[test]
fn split_with_same_buffer_inherits_cursor() {
    let (mut editor, buffer, window) = editor_with_text(
        &[b"line1", b"line2", b"line3", b"line4", b"line5", b"line6"],
        position(6, 0),
    );
    let tab = editor.current_tabpage().unwrap();
    let split_win = editor.split_above(tab, window, buffer, true).unwrap();
    assert_eq!(
        editor.window(split_win).unwrap().cursor,
        position(6, 0),
        "same-buffer split should inherit the cursor"
    );
}

#[test]
fn extmark_splice_undo_redo_keep_position_index_at_range_edges() {
    // Guards the incremental position index: a mark that moves across a
    // query edge must leave its old window and appear in the new one after
    // splice, undo, and redo. A stale `by_position` entry fails the edge
    // queries below even though full-range queries would still pass.
    let mut marks = crate::Extmarks::new();
    let namespace = marks.create_namespace("index-edges").unwrap();
    for row in 0..10 {
        marks
            .set(
                namespace,
                None,
                crate::extmark::ExtmarkPlacement::new(crate::extmark::ExtmarkPosition::new(row, 0)),
            )
            .unwrap();
    }
    let rows_in = |marks: &crate::Extmarks, first_row: usize, last_row: usize| -> Vec<usize> {
        marks
            .query(
                namespace,
                crate::extmark::ExtmarkPosition::new(first_row, 0),
                crate::extmark::ExtmarkPosition::new(last_row, 0),
                None,
            )
            .unwrap()
            .iter()
            .map(|mark| mark.position().row)
            .collect()
    };
    // Newline insertion at row 3 rides the right-gravity mark to row 4 and
    // shifts every later row down one.
    let (_, undo) = marks.splice_recording(crate::extmark::TextSplice {
        start: crate::extmark::ExtmarkPosition::new(3, 0),
        old_extent: crate::extmark::TextExtent::EMPTY,
        new_extent: crate::extmark::TextExtent::new(1, 0),
    });
    assert_eq!(rows_in(&marks, 0, 3), vec![0, 1, 2]);
    assert_eq!(rows_in(&marks, 4, 4), vec![4]);
    marks.undo_splice(&undo);
    assert_eq!(rows_in(&marks, 0, 3), vec![0, 1, 2, 3]);
    marks.redo_splice(&undo);
    assert_eq!(rows_in(&marks, 0, 3), vec![0, 1, 2]);
    assert_eq!(rows_in(&marks, 4, 4), vec![4]);
}
