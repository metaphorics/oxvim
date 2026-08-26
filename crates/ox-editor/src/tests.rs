#![allow(clippy::unwrap_used)]

use std::fs;

use ox_text::{Buffer, Position};
use ox_types::{BufHandle, WinHandle};

use crate::layout::{Anchor, Frame, Geometry, Layout, RelativeTo, TabpageState, WinConfig, WindowState};
use crate::marks::{Changelists, Jumplist, LocalMarks, MarkLocation};
use crate::options::{OptionError, OptionStore, OptionValue, OPTION_COUNT, OPTION_METADATA};
use crate::register::{RegisterContent, Registers};
use crate::Editor;
use crate::BufferRelease;

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
    assert!(editor.buffer(first).unwrap().loaded);
    assert!(editor.buffer(first).unwrap().hidden);

    let tab = editor
        .create_tabpage(first, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    assert_eq!(editor.buffer(first).unwrap().attachments, 1);
    assert!(!editor.buffer(first).unwrap().hidden);
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
    assert!(detached.loaded && detached.hidden);
    detached.attach().unwrap();
    detached.detach(false);
    assert!(!detached.loaded && !detached.hidden);
    assert!(detached.text().is_err());
    assert!(detached.attach().is_err());
    detached.load(Buffer::from_lines(&[b"reloaded".to_vec()], false).unwrap());
    assert_eq!(detached.text().unwrap().line(1).unwrap(), b"reloaded");
}

#[test]
fn text_mutations_advance_all_ticks_and_splice_marks() {
    let text = Buffer::from_lines(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], false)
        .unwrap();
    let mut state = crate::BufferState::new(text, true);
    state.marks.set('a', position(3, 0)).unwrap();

    state
        .append_lines(1, &[b"x".to_vec(), b"y".to_vec()], position(1, 0), 1)
        .unwrap();
    assert_eq!(state.changedtick(), 1);
    assert_eq!(state.changedtick_diag, 1);
    assert_eq!(state.changedtick_fold, 1);
    assert_eq!(state.marks.get('a').unwrap(), Some(position(5, 0)));

    state
        .delete_lines(2, 4, position(2, 0), 2)
        .unwrap();
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
    let switched = editor.split_vertical(tab, original, first).unwrap();

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
    assert!(editor.buffer(second).unwrap().hidden);
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
    assert_eq!(layout.window_geometry(w1).unwrap(), Geometry::new(0, 0, 5, 7).unwrap());
    assert_eq!(layout.window_geometry(w2).unwrap(), Geometry::new(0, 5, 4, 7).unwrap());
    layout
        .split_horizontal(w2, w3, WindowState::new(buffer, position(1, 0)))
        .unwrap();
    assert_eq!(layout.window_geometry(w2).unwrap(), Geometry::new(0, 5, 4, 4).unwrap());
    assert_eq!(layout.window_geometry(w3).unwrap(), Geometry::new(4, 5, 4, 3).unwrap());
    assert_eq!(layout.winnr(w1).unwrap(), 1);
    assert_eq!(layout.winnr(w2).unwrap(), 2);
    assert_eq!(layout.winnr(w3).unwrap(), 3);

    layout.close(w2).unwrap();
    assert_eq!(layout.window_by_winnr(2).unwrap(), w3);
    layout.resize(Geometry::new(2, 3, 10, 8).unwrap()).unwrap();
    layout.equalize().unwrap();
    assert_eq!(layout.window_geometry(w1).unwrap(), Geometry::new(2, 3, 5, 8).unwrap());
    assert_eq!(layout.window_geometry(w3).unwrap(), Geometry::new(2, 8, 5, 8).unwrap());
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

    assert_eq!(layout.window_geometry(w1).unwrap(), Geometry::new(0, 0, 3, 7).unwrap());
    assert_eq!(layout.window_geometry(w2).unwrap(), Geometry::new(0, 3, 3, 7).unwrap());
    assert_eq!(layout.window_geometry(w3).unwrap(), Geometry::new(0, 6, 3, 7).unwrap());
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

    assert_eq!(layout.window_geometry(w1).unwrap(), Geometry::new(0, 0, 5, 7).unwrap());
    assert_eq!(layout.window_geometry(w2).unwrap(), Geometry::new(0, 5, 4, 4).unwrap());
    assert_eq!(layout.window_geometry(w3).unwrap(), Geometry::new(4, 5, 2, 3).unwrap());
    assert_eq!(layout.window_geometry(w4).unwrap(), Geometry::new(4, 7, 2, 3).unwrap());
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

    assert!(editor.split_vertical(tab, current, unloaded).is_err());
    let config = WinConfig::new(RelativeTo::Editor, Anchor::NorthWest, 0.0, 0.0, 3, 2).unwrap();
    assert!(editor.open_float(tab, unloaded, config).is_err());
    assert_eq!(editor.tabpage(tab).unwrap().layout().window_count(), tiled_count);
    assert_eq!(editor.tabpage(tab).unwrap().floating_windows().len(), float_count);
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
    for (window, zindex) in [(window_handle(2), 80), (window_handle(3), 20), (window_handle(4), 80)] {
        let mut config = WinConfig::new(RelativeTo::Editor, Anchor::NorthWest, 0.0, 0.0, 4, 2)
            .unwrap();
        config.zindex = zindex;
        tab.add_float(window, WindowState::new(buffer, position(1, 0)), config)
            .unwrap();
    }
    let ordered: Vec<_> = tab.floating_windows().map(|float| float.window).collect();
    assert_eq!(ordered, vec![window_handle(3), window_handle(2), window_handle(4)]);
}

#[test]
fn generated_option_table_matches_authoritative_source_count() {
    let source_path = std::env::var("OXVIM_REF_ROOT")
        .map(|root| format!("{root}/src/nvim/options.lua"))
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../codegen/upstream/options.lua").to_owned());
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
    assert_eq!(store.get_global("background").unwrap(), &OptionValue::String("dark".into()));
    assert_eq!(store.get_global("shell").unwrap(), &OptionValue::String("sh".into()));
    assert_eq!(store.get_buffer(buffer, "ts").unwrap(), &OptionValue::Number(8));

    store.set_buffer(buffer, "tabstop", OptionValue::Number(4)).unwrap();
    assert_eq!(store.get_buffer(buffer, "ts").unwrap(), &OptionValue::Number(4));
    store
        .set_window(window, "number", OptionValue::Boolean(true))
        .unwrap();
    assert_eq!(store.get_window(window, "nu").unwrap(), &OptionValue::Boolean(true));

    assert!(matches!(
        store.set_buffer(buffer, "tabstop", OptionValue::String("4".into())),
        Err(OptionError::TypeMismatch { .. })
    ));
    assert!(matches!(
        store.set_global("tabstop", OptionValue::Number(2)),
        Err(OptionError::WrongScope { .. })
    ));
    assert!(store
        .set_buffer(buffer, "formatoptions", OptionValue::String("tt".into()))
        .is_err());
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
        .set('a', RegisterContent::linewise(vec![b"inserted".to_vec()]).unwrap())
        .unwrap();

    let content = editor.registers().get('a').unwrap().cloned().unwrap();
    editor.put_content(buffer, position(1, 0), &content, 10).unwrap();
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.text().unwrap().line(2).unwrap(), b"inserted");
    assert_eq!(state.text().unwrap().line(3).unwrap(), b"two");
    assert_eq!(state.changedtick(), 1);
    assert_eq!(state.changedtick_diag, 1);
    assert_eq!(state.changedtick_fold, 1);
    assert_eq!(state.undo.current_seq(), 1);
    assert_eq!(editor.local_mark(buffer, 'a').unwrap(), Some(position(3, 0)));
    assert_eq!(editor.window(window).unwrap().cursor, position(3, 1));
    assert_eq!(editor.window(window).unwrap().topline, 3);
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
        .replace_buffer_lines(
            buffer,
            2,
            2,
            &[b"x".to_vec(), b"y".to_vec()],
            position(2, 0),
            position(5, 0),
            10,
        )
        .unwrap();
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.changedtick(), 1);
    assert_eq!(state.changedtick_diag, 1);
    assert_eq!(state.changedtick_fold, 1);
    assert_eq!(editor.local_mark(buffer, 'a').unwrap(), Some(position(5, 0)));
    assert_eq!(editor.window(window).unwrap().cursor, position(5, 1));
    assert_eq!(editor.window(window).unwrap().topline, 5);
    assert_eq!(editor.changelists().len(buffer), 1);

    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.text().unwrap().line(2).unwrap(), b"two");
    assert_eq!(
        state.text().unwrap().to_bytes(),
        b"one\ntwo\nthree\nfour"
    );
    assert_eq!(state.changedtick(), 2);
    assert_eq!(state.changedtick_diag, 2);
    assert_eq!(state.changedtick_fold, 2);
    assert_eq!(editor.local_mark(buffer, 'a').unwrap(), Some(position(4, 0)));
    assert_eq!(editor.window(window).unwrap().cursor, position(4, 1));
    assert_eq!(editor.window(window).unwrap().topline, 4);
    assert_eq!(editor.changelists().len(buffer), 2);

    assert_eq!(editor.buffer_redo(buffer).unwrap(), Some(1));
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(state.text().unwrap().line(2).unwrap(), b"x");
    assert_eq!(state.text().unwrap().line(3).unwrap(), b"y");
    assert_eq!(
        state.text().unwrap().to_bytes(),
        b"one\nx\ny\nthree\nfour"
    );
    assert_eq!(state.changedtick(), 3);
    assert_eq!(state.changedtick_diag, 3);
    assert_eq!(state.changedtick_fold, 3);
    assert_eq!(editor.local_mark(buffer, 'a').unwrap(), Some(position(5, 0)));
    assert_eq!(editor.window(window).unwrap().cursor, position(5, 1));
    assert_eq!(editor.window(window).unwrap().topline, 5);
    assert_eq!(editor.changelists().len(buffer), 3);

    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    let state = editor.buffer(buffer).unwrap();
    assert_eq!(
        state.text().unwrap().to_bytes(),
        b"one\ntwo\nthree\nfour"
    );
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
    assert_eq!(registers.get('a').unwrap().unwrap().to_bytes(), b"leftright");
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
    let lines: Vec<_> = jumps.entries().iter().map(|entry| entry.position.lnum).collect();
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
    let mut context = crate::script::ScriptCtx::<crate::script::RealFileIO>::new(
        crate::script::RealFileIO,
    );
    context.set_runtime_roots_from_rtp("/first,,/second,");
    let roots: Vec<&std::path::Path> = context.runtime_roots().iter().map(|root| root.path()).collect();
    assert_eq!(roots, vec![std::path::Path::new("/first"), std::path::Path::new("/second")]);
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
    assert_eq!(editor.buffer(buffer).unwrap().text().unwrap().to_bytes(), before);
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
