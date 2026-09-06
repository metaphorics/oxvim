//! Split multigrid geometry and per-split ui-watched marks (task 3D3).
#![allow(clippy::unwrap_used)]

use ox_editor::extmark::ExtmarkId;
use ox_editor::{BufferFlags, Editor, ExtmarkPlacement, ExtmarkPosition, Geometry};
use ox_rpc::decode;
use ox_text::Buffer;
use ox_types::Object;
use ox_ui::{
    ChromeState, Compositor, Emitter, HlState, LayerKind, MESSAGE_ZINDEX, UiChannels, UiOptions,
    WatchedExtmark,
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
    placement
        .attributes
        .flags
        .set(ox_editor::ExtmarkFlags::UI_WATCHED, true);
    editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(namespace, Some(ExtmarkId::new(1).unwrap()), placement)
        .unwrap();
    let original = editor.current_window().unwrap();
    editor.split_above(tab, original, buffer, true).unwrap();
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
#[expect(
    clippy::expect_used,
    reason = "missing compositor layers should fail this geometry contract by name"
)]
fn split_grids_use_exact_heights_and_message_separator() {
    let editor = editor_with_split();
    let mut highlights = HlState::new();
    let mut compositor = Compositor::new(20, 8);
    compositor
        .refresh_from_editor(&editor, 20, 8, &mut highlights)
        .unwrap();
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
    assert!(
        compositor
            .layers()
            .iter()
            .all(|layer| { layer.kind != LayerKind::Window || layer.grid.id() != 3 })
    );
    assert_eq!(
        upper.grid.height() + 1 + lower.grid.height() + 1 + message.grid.height(),
        8
    );
}

#[test]
fn split_windows_each_carry_watched_marks() {
    let editor = editor_with_split();
    let mut highlights = HlState::new();
    let mut compositor = Compositor::new(20, 8);
    compositor
        .refresh_from_editor(&editor, 20, 8, &mut highlights)
        .unwrap();
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
#[expect(
    clippy::panic,
    reason = "malformed redraw frames must fail this protocol contract immediately"
)]
fn emitter_sends_win_extmark_to_each_split_grid() {
    let editor = editor_with_split();
    let mut highlights = HlState::new();
    let mut compositor = Compositor::new(20, 8);
    compositor
        .refresh_from_editor(&editor, 20, 8, &mut highlights)
        .unwrap();
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
        .unwrap()
        .0;
    let decoded = decode(&frames[&1]).unwrap();
    let Object::Array(frame) = decoded else {
        panic!("redraw frame")
    };
    let Some(Object::Array(events)) = frame.get(2) else {
        panic!("redraw events")
    };
    let mut by_grid: Vec<(i64, i64)> = Vec::new();
    for event in events {
        let Object::Array(parts) = event else {
            continue;
        };
        let Some(Object::String(name)) = parts.first() else {
            continue;
        };
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
    assert_eq!(
        by_grid
            .iter()
            .filter(|(grid, _)| *grid == 2 || *grid == 4)
            .count(),
        2
    );
}

// ---------------------------------------------------------------------------
// Shared-buffer statusline diff regression (task 3D5).
// ---------------------------------------------------------------------------

/// Returns the redraw event array from a decoded `[2, "redraw", […]]` frame.
fn redraw_events(decoded: &Object) -> Vec<&Object> {
    let Object::Array(frame) = decoded else {
        return Vec::new();
    };
    let Some(Object::Array(events)) = frame.get(2) else {
        return Vec::new();
    };
    events.iter().collect()
}

/// Whether the decoded frame contains at least one event with `name`.
fn has_event(decoded: &Object, name: &str) -> bool {
    redraw_events(decoded).iter().any(|event| {
        let Object::Array(parts) = event else {
            return false;
        };
        let Some(Object::String(event_name)) = parts.first() else {
            return false;
        };
        event_name.to_string_lossy() == name
    })
}

/// Every row that received a `grid_line` update for `grid`.
fn grid_line_rows(decoded: &Object, grid: i64) -> Vec<i64> {
    let mut rows = Vec::new();
    for event in redraw_events(decoded) {
        let Object::Array(parts) = event else {
            continue;
        };
        let Some(Object::String(name)) = parts.first() else {
            continue;
        };
        if name.to_string_lossy() != "grid_line" {
            continue;
        }
        for args in parts.iter().skip(1) {
            let Object::Array(args) = args else { continue };
            let (Some(Object::Integer(g)), Some(Object::Integer(row))) =
                (args.first(), args.get(1))
            else {
                continue;
            };
            if *g == grid {
                rows.push(*row);
            }
        }
    }
    rows
}

/// Reconstructs the text written to `grid` at `row` across all `grid_line`
/// argsets in the frame, honouring `startcol` padding and cell `repeat`.
fn grid_line_row_text(decoded: &Object, grid: i64, row: i64) -> String {
    let mut text = String::new();
    for event in redraw_events(decoded) {
        let Object::Array(parts) = event else {
            continue;
        };
        let Some(Object::String(name)) = parts.first() else {
            continue;
        };
        if name.to_string_lossy() != "grid_line" {
            continue;
        }
        for args in parts.iter().skip(1) {
            let Object::Array(args) = args else { continue };
            let (
                Some(Object::Integer(g)),
                Some(Object::Integer(r)),
                Some(Object::Integer(startcol)),
                Some(Object::Array(cells)),
            ) = (args.first(), args.get(1), args.get(2), args.get(3))
            else {
                continue;
            };
            if *g != grid || *r != row {
                continue;
            }
            let startcol = usize::try_from(*startcol).unwrap();
            while text.len() < startcol {
                text.push(' ');
            }
            for cell in cells {
                let Object::Array(cell) = cell else { continue };
                let Some(Object::String(ch)) = cell.first() else {
                    continue;
                };
                let repeat = cell
                    .get(2)
                    .and_then(|v| {
                        if let Object::Integer(n) = v {
                            Some(*n)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(1);
                for _ in 0..repeat {
                    text.push_str(&ch.to_string_lossy());
                }
            }
        }
    }
    text
}

const UPPER_STATUSLINE_ROW: i64 = 3;
const LOWER_STATUSLINE_ROW: i64 = 6;

fn assert_unmodified_statuslines(frame: &Object) {
    let upper = grid_line_row_text(frame, 1, UPPER_STATUSLINE_ROW);
    let lower = grid_line_row_text(frame, 1, LOWER_STATUSLINE_ROW);
    assert!(
        !upper.contains("[+]"),
        "unmodified upper statusline must not contain [+]: {upper:?}"
    );
    assert!(
        !lower.contains("[+]"),
        "unmodified lower statusline must not contain [+]: {lower:?}"
    );
}

fn assert_modified_statusline_diff(frame: &Object) {
    let upper = grid_line_row_text(frame, 1, UPPER_STATUSLINE_ROW);
    let lower = grid_line_row_text(frame, 1, LOWER_STATUSLINE_ROW);
    assert!(
        upper.contains("[+]"),
        "modified upper statusline must contain [+]: {upper:?}"
    );
    assert!(
        lower.contains("[+]"),
        "modified lower statusline must contain [+]: {lower:?}"
    );
    assert!(
        !has_event(frame, "grid_resize"),
        "flag-only frame must not emit grid_resize"
    );
    for text_grid in [2i64, 3, 4] {
        assert!(
            grid_line_rows(frame, text_grid).is_empty(),
            "flag-only frame must not emit grid_line for text grid {text_grid}"
        );
    }
    let default_rows = grid_line_rows(frame, 1);
    assert!(
        !default_rows.is_empty(),
        "flag-only frame must still emit statusline grid_line for grid 1"
    );
    for row in &default_rows {
        assert!(
            *row == UPPER_STATUSLINE_ROW || *row == LOWER_STATUSLINE_ROW,
            "flag-only frame touched unexpected default-grid row {row}: {default_rows:?}"
        );
    }
}

fn assert_cleared_statuslines(frame: &Object) {
    let rows = grid_line_rows(frame, 1);
    assert!(
        rows.contains(&UPPER_STATUSLINE_ROW) && rows.contains(&LOWER_STATUSLINE_ROW),
        "clear frame must re-emit both statusline rows {UPPER_STATUSLINE_ROW} and \
         {LOWER_STATUSLINE_ROW}: {rows:?}"
    );
    let upper = grid_line_row_text(frame, 1, UPPER_STATUSLINE_ROW);
    let lower = grid_line_row_text(frame, 1, LOWER_STATUSLINE_ROW);
    assert!(
        !upper.is_empty() && !upper.contains("[+]"),
        "cleared upper statusline must be re-rendered without [+]: {upper:?}"
    );
    assert!(
        !lower.is_empty() && !lower.contains("[+]"),
        "cleared lower statusline must be re-rendered without [+]: {lower:?}"
    );
}

/// Toggling one shared buffer's modified flag updates every visible split
/// statusline without resizing grids, touching text-grid rows, or moving
/// view state.
#[test]
fn shared_buffer_modified_flag_updates_both_split_statuslines() {
    let mut editor = editor_with_split();
    let buffer = editor.current_buffer().unwrap();
    let tab = editor.current_tabpage().unwrap();
    let windows = editor.tabpage(tab).unwrap().windows();
    assert_eq!(windows.len(), 2, "split fixture has two windows");
    let snapshots: Vec<_> = windows
        .iter()
        .map(|&window| {
            let state = editor.window(window).unwrap();
            (state.cursor, state.topline)
        })
        .collect();

    let mut highlights = HlState::new();
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
    let mut emitter = Emitter::new();
    let mut chrome = ChromeState::new();

    let mut compositor = Compositor::new(20, 8);
    compositor
        .refresh_from_editor(&editor, 20, 8, &mut highlights)
        .unwrap();
    let messages = emitter
        .redraw(&mut channels, &compositor, &mut highlights, &mut chrome)
        .unwrap()
        .0;
    let initial_frame = decode(&messages[&1]).unwrap();
    assert_unmodified_statuslines(&initial_frame);

    editor
        .buffer_mut(buffer)
        .unwrap()
        .flags
        .set(BufferFlags::MODIFIED, true);
    let mut compositor = Compositor::new(20, 8);
    compositor
        .refresh_from_editor(&editor, 20, 8, &mut highlights)
        .unwrap();
    let messages = emitter
        .redraw(&mut channels, &compositor, &mut highlights, &mut chrome)
        .unwrap()
        .0;
    let modified_frame = decode(&messages[&1]).unwrap();
    assert_modified_statusline_diff(&modified_frame);

    editor
        .buffer_mut(buffer)
        .unwrap()
        .flags
        .set(BufferFlags::MODIFIED, false);
    let mut compositor = Compositor::new(20, 8);
    compositor
        .refresh_from_editor(&editor, 20, 8, &mut highlights)
        .unwrap();
    let messages = emitter
        .redraw(&mut channels, &compositor, &mut highlights, &mut chrome)
        .unwrap()
        .0;
    let cleared_frame = decode(&messages[&1]).unwrap();
    assert_cleared_statuslines(&cleared_frame);

    for (index, &window) in windows.iter().enumerate() {
        let state = editor.window(window).unwrap();
        assert_eq!(
            state.cursor, snapshots[index].0,
            "cursor moved for window {window:?}"
        );
        assert_eq!(
            state.topline, snapshots[index].1,
            "topline moved for window {window:?}"
        );
    }
}
