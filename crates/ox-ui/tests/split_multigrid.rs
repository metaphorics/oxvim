//! Split multigrid geometry and per-split ui-watched marks (task 3D3).
#![allow(clippy::unwrap_used)]

use ox_editor::extmark::ExtmarkId;
use ox_editor::{Editor, ExtmarkPlacement, ExtmarkPosition, Geometry};
use ox_rpc::decode;
use ox_text::Buffer;
use ox_types::Object;
use ox_ui::{
    ChromeState, Compositor, Emitter, HlState, LayerKind, UiChannels, UiOptions, WatchedExtmark,
    MESSAGE_ZINDEX,
};

fn editor_with_split() -> Editor {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(
            Buffer::from_lines(
                &[b"non ui-watched line".to_vec(), b"ui-watched line".to_vec()],
                true,
            )
            .unwrap(),
            true,
        )
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 20, 8).unwrap())
        .unwrap();
    let namespace = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("extmark-ui")
        .unwrap();
    let mut placement = ExtmarkPlacement::new(ExtmarkPosition::new(1, 0));
    placement.attributes.ui_watched = true;
    editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(namespace, Some(ExtmarkId::new(1).unwrap()), placement)
        .unwrap();
    let original = editor.current_window().unwrap();
    editor.split_above(tab, original, buffer).unwrap();
    editor
}

fn window_layers(compositor: &Compositor) -> Vec<&ox_ui::Layer> {
    compositor
        .layers()
        .iter()
        .filter(|layer| layer.kind == LayerKind::Window && layer.window.is_some())
        .collect()
}

#[test]
fn split_grids_use_exact_heights_and_message_separator() {
    let editor = editor_with_split();
    let mut highlights = HlState::new();
    let compositor = Compositor::from_editor(&editor, 20, 8, &mut highlights).unwrap();
    let windows = window_layers(&compositor);
    assert_eq!(windows.len(), 2);
    let upper = windows
        .iter()
        .find(|layer| layer.row == 0)
        .expect("upper split grid");
    let lower = windows
        .iter()
        .find(|layer| layer.row == 4)
        .expect("lower split grid");
    assert_eq!(upper.grid.height(), 3);
    assert_eq!(lower.grid.height(), 2);
    assert_eq!(upper.grid.id(), 4);
    assert_eq!(lower.grid.id(), 2);
    assert_eq!(
        compositor.window_grid(upper.window.unwrap(), &editor),
        Some(4)
    );
    assert_eq!(
        compositor.window_grid(lower.window.unwrap(), &editor),
        Some(2)
    );

    let message = compositor
        .layers()
        .iter()
        .find(|layer| layer.kind == LayerKind::Message)
        .expect("message grid");
    assert_eq!(message.grid.id(), 3);
    assert_eq!(message.grid.height(), 1);
    assert_eq!(message.row, 7);
    assert_eq!(message.zindex, MESSAGE_ZINDEX);
    assert!(compositor.layers().iter().all(|layer| {
        layer.kind != LayerKind::Window || layer.grid.id() != 3
    }));
    assert_eq!(upper.grid.height() + 1 + lower.grid.height() + 1 + message.grid.height(), 8);
}

#[test]
fn split_windows_each_carry_watched_marks() {
    let editor = editor_with_split();
    let mut highlights = HlState::new();
    let compositor = Compositor::from_editor(&editor, 20, 8, &mut highlights).unwrap();
    let windows = window_layers(&compositor);
    for layer in windows {
        assert_eq!(
            layer.watched_extmarks,
            vec![WatchedExtmark {
                namespace: 1,
                mark: 1,
                row: 1,
                col: 16,
                buffer_row: 1,
            }],
            "grid {}",
            layer.grid.id()
        );
    }
}

#[test]
fn emitter_sends_win_extmark_to_each_split_grid() {
    let editor = editor_with_split();
    let mut highlights = HlState::new();
    let compositor = Compositor::from_editor(&editor, 20, 8, &mut highlights).unwrap();
    let mut channels = UiChannels::new();
    channels
        .attach(
            1,
            20,
            8,
            UiOptions {
                ext_linegrid: true,
                ext_multigrid: true,
                ..UiOptions::default()
            },
        )
        .unwrap();
    let frames = Emitter::new()
        .redraw(
            &mut channels,
            &compositor,
            &mut highlights,
            &mut ChromeState::new(),
        )
        .unwrap();
    let decoded = decode(&frames[&1]).unwrap();
    let Object::Array(frame) = decoded else { panic!("redraw frame") };
    let Some(Object::Array(events)) = frame.get(2) else { panic!("redraw events") };
    let mut by_grid: Vec<(i64, i64)> = Vec::new();
    for event in events {
        let Object::Array(parts) = event else { continue };
        let Some(Object::String(name)) = parts.first() else { continue };
        if name.to_string_lossy() != "win_extmark" {
            continue;
        }
        for args in parts.iter().skip(1) {
            let Object::Array(args) = args else { continue };
            let (Some(Object::Integer(grid)), Some(Object::Integer(row))) =
                (args.first(), args.get(4))
            else {
                panic!("win_extmark args: {args:?}");
            };
            by_grid.push((*grid, *row));
        }
    }
    assert!(
        by_grid.iter().any(|(grid, _)| *grid == 2),
        "missing lower-grid win_extmark: {by_grid:?}"
    );
    assert!(
        by_grid.iter().any(|(grid, _)| *grid == 4),
        "missing upper-grid win_extmark: {by_grid:?}"
    );
    assert_eq!(by_grid.iter().filter(|(grid, _)| *grid == 2 || *grid == 4).count(), 2);
}
