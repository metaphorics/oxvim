//! Integration tests for the `ox-ui` redraw pipeline (emitter, grid, compositor, chrome).
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use ox_editor::extmark::ExtmarkHighlightMode;
use ox_editor::{Editor, ExtmarkPlacement, ExtmarkPosition, Geometry};
use ox_rpc::decode;
use ox_text::Buffer;
use ox_types::{Dict, Object, OxStr};
use ox_ui::{
    premix_color, Cell, ChromeState, Compositor, ContentChunk, Emitter, Grid, Highlight, HlAttrs,
    HlState, Layer, LayerKind, MessageState, ModeInfo, UiChannel, UiChannels, UiEvent, UiOptions,
    MESSAGE_ZINDEX,
};

#[test]
fn grid_line_coalesces_styled_runs_and_repeats() {
    let old = Grid::new(1, 6, 1).unwrap();
    let mut grid = old.clone();
    grid.put(0, 1, "a", 7, 1).unwrap();
    grid.put(0, 2, "a", 7, 1).unwrap();
    grid.put(0, 3, "b", 7, 1).unwrap();
    grid.put(0, 4, "c", 9, 1).unwrap();

    let lines = grid.diff(&old);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].start_col, 1);
    // ui_events.in.h grid_line data: repeated equal cells carry [text, attr, repeat],
    // and a following cell with unchanged attr omits attr.
    assert_eq!(
        lines[0].cells,
        vec![
            Object::Array(vec![Object::String(OxStr::from("a")), Object::Integer(7), Object::Integer(2)]),
            Object::Array(vec![Object::String(OxStr::from("b"))]),
            Object::Array(vec![Object::String(OxStr::from("c")), Object::Integer(9)]),
        ]
    );
}

#[test]
fn replacing_wide_cell_clears_continuation() {
    let mut grid = Grid::new(1, 3, 1).unwrap();
    grid.put(0, 0, "界", 2, 2).unwrap();
    assert_eq!(grid.cell(0, 1).unwrap().width, 0);
    grid.put(0, 0, "x", 2, 1).unwrap();
    assert_eq!(grid.cell(0, 1).unwrap(), &Cell::blank());
}

#[test]
fn width_aware_text_groups_combining_marks_and_wide_cells() {
    let mut grid = Grid::new(1, 5, 1).unwrap();
    grid.write_text(0, 0, "é界x", 3).unwrap();
    assert_eq!(grid.cell(0, 0).unwrap().text, OxStr::from("é"));
    assert_eq!(grid.cell(0, 1).unwrap().width, 2);
    assert_eq!(grid.cell(0, 2).unwrap().width, 0);
    assert_eq!(grid.cell(0, 3).unwrap().text, OxStr::from("x"));
}

#[test]
fn capability_implications_normalize_and_legacy_attach_is_rejected() {
    let options = UiOptions::from_dict(&Dict(vec![
        (OxStr::from("ext_messages"), Object::Boolean(true)),
    ]));
    assert!(options.ext_linegrid);
    assert!(options.ext_cmdline);
    assert!(UiChannel::new(1, 10, 3, UiOptions::default()).is_err());
}

#[test]
fn wrapped_diff_reaches_last_column_and_continues_at_zero() {
    let old = Grid::new(1, 4, 2).unwrap();
    let mut grid = old.clone();
    grid.put(0, 1, "x", 0, 1).unwrap();
    grid.set_wrap(0, true).unwrap();
    let lines = grid.diff(&old);
    assert_eq!(lines.len(), 2);
    assert_eq!((lines[0].row, lines[0].start_col, lines[0].wrap), (0, 1, true));
    assert_eq!(lines[0].cells.len(), 2);
    assert_eq!(
        lines[0].cells[1],
        Object::Array(vec![Object::String(OxStr::from(" ")), Object::Integer(0), Object::Integer(2)])
    );
    assert_eq!((lines[1].row, lines[1].start_col), (1, 0));
}

#[test]
fn batch_bytes_match_hand_computed_grid_line_frame() {
    let mut channel = UiChannel::new(4, 10, 5, UiOptions { ext_linegrid: true, ..UiOptions::default() }).unwrap();
    channel.begin();
    channel.emit(UiEvent::new("grid_line", vec![
        Object::Integer(1),
        Object::Integer(0),
        Object::Integer(0),
        Object::Array(vec![Object::Array(vec![Object::String(OxStr::from("x")), Object::Integer(5)])]),
        Object::Boolean(false),
    ])).unwrap();
    let bytes = channel.flush().unwrap();
    assert_eq!(
        bytes,
        vec![
            0x93, 0x02, 0xa6, b'r', b'e', b'd', b'r', b'a', b'w', 0x92,
            0x92, 0xa9, b'g', b'r', b'i', b'd', b'_', b'l', b'i', b'n', b'e',
            0x95, 0x01, 0x00, 0x00, 0x91, 0x92, 0xa1, b'x', 0x05, 0xc2,
            0x92, 0xa5, b'f', b'l', b'u', b's', b'h', 0x90,
        ]
    );
}

#[test]
fn compositor_layers_splits_float_and_message_in_upstream_order() {
    let mut left = Grid::new(2, 3, 2).unwrap();
    left.put(0, 0, "L", 0, 1).unwrap();
    let mut right = Grid::new(3, 3, 2).unwrap();
    right.put(0, 0, "R", 0, 1).unwrap();
    right.put(0, 1, "R", 0, 1).unwrap();
    let mut floating = Grid::new(4, 2, 1).unwrap();
    floating.put(0, 0, "F", 0, 1).unwrap();
    let mut message = Grid::new(5, 2, 1).unwrap();
    message.put(0, 0, "M", 0, 1).unwrap();

    let mut compositor = Compositor::new(6, 2);
    compositor.push_layer(Layer::new(left, 0, 0, 0, LayerKind::Window));
    compositor.push_layer(Layer::new(right, 0, 3, 0, LayerKind::Window));
    compositor.push_layer(Layer::new(floating, 0, 2, 500, LayerKind::Float));
    let mut message_layer = Layer::new(message, 0, 2, 0, LayerKind::Message);
    message_layer.opaque = false;
    compositor.push_layer(message_layer);
    assert_eq!(compositor.layers()[3].zindex, MESSAGE_ZINDEX);

    let screen = compositor.compose(&mut HlState::new()).unwrap();
    assert_eq!(screen.grid.cell(0, 0).unwrap().text, OxStr::from("L"));
    assert_eq!(screen.grid.cell(0, 4).unwrap().text, OxStr::from("R"));
    assert_eq!(screen.grid.cell(0, 2).unwrap().text, OxStr::from("M"));
}

#[test]
fn winblend_premixes_rgb_with_integer_rounding() {
    assert_eq!(premix_color(0xff0000, 0x0000ff, 50), 0x800080);
    assert_eq!(premix_color(0xffffff, 0x000000, 20), 0xcccccc);

    let mut highlights = HlState::new();
    let top = Highlight { rgb: HlAttrs { foreground: Some(0xff0000), background: Some(0x00ff00), ..HlAttrs::default() }, ..Highlight::default() };
    let bottom = Highlight { rgb: HlAttrs { foreground: Some(0x0000ff), background: Some(0x000000), ..HlAttrs::default() }, ..Highlight::default() };
    let (top_id, _) = highlights.intern(top).unwrap();
    let (bottom_id, _) = highlights.intern(bottom).unwrap();
    let (mixed_id, event) = highlights.premix(top_id, bottom_id, 50).unwrap();
    assert!(event.is_some());
    let mixed = highlights.get(mixed_id).unwrap();
    assert_eq!(mixed.rgb.foreground, Some(0x800080));
    assert_eq!(mixed.rgb.background, Some(0x008000));
}


#[test]
fn rendering_does_not_define_undefined_named_groups() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_lines(&[b"hello".to_vec()], true).unwrap(), true)
        .unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 10, 4).unwrap()).unwrap();
    let namespace = editor.buffer_mut(buffer).unwrap().extmarks.create_namespace("test").unwrap();
    let mut placement = ExtmarkPlacement::new(ExtmarkPosition::new(0, 0))
        .with_end(ExtmarkPosition::new(0, 5));
    placement.attributes.highlight_group = Some("Comment".to_string());
    placement.attributes.highlight_mode = Some(ExtmarkHighlightMode::Combine);
    editor.buffer_mut(buffer).unwrap().extmarks.set(namespace, None, placement).unwrap();

    let mut highlights = HlState::new();
    let compositor = Compositor::from_editor(&editor, 10, 4, &mut highlights).unwrap();
    let screen = compositor.compose(&mut highlights).unwrap();
    assert!(highlights.group_id(&OxStr::from("Comment")).is_none());
    assert_eq!(screen.grid.cell(0, 0).unwrap().hl_id, 0);
}

#[test]
fn higher_priority_extmark_highlight_wins() {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_lines(&[b"12345".to_vec()], true).unwrap(), true)
        .unwrap();
    editor.create_tabpage(buffer, Geometry::new(0, 0, 15, 10).unwrap()).unwrap();
    let namespace = editor.buffer_mut(buffer).unwrap().extmarks.create_namespace("test").unwrap();
    let mut first = ExtmarkPlacement::new(ExtmarkPosition::new(0, 0))
        .with_end(ExtmarkPosition::new(0, 2));
    first.attributes.highlight_group = Some("Comment".to_string());
    first.attributes.priority = 20;
    let mut second = ExtmarkPlacement::new(ExtmarkPosition::new(0, 0))
        .with_end(ExtmarkPosition::new(0, 2));
    second.attributes.highlight_group = Some("String".to_string());
    second.attributes.priority = 10;
    {
        let extmarks = &mut editor.buffer_mut(buffer).unwrap().extmarks;
        extmarks.set(namespace, None, first).unwrap();
        extmarks.set(namespace, None, second).unwrap();
    }
    let mut highlights = HlState::with_default_syntax_groups();
    let compositor = Compositor::from_editor(&editor, 15, 10, &mut highlights).unwrap();
    let screen = compositor.compose(&mut highlights).unwrap();
    let comment = highlights.group_id(&OxStr::from("Comment")).unwrap();
    let string = highlights.group_id(&OxStr::from("String")).unwrap();
    assert_eq!(screen.grid.cell(0, 0).unwrap().hl_id, comment);
    assert_eq!(screen.grid.cell(0, 1).unwrap().hl_id, comment);
    assert_ne!(screen.grid.cell(0, 0).unwrap().hl_id, string);
}

#[test]
fn blend_mode_mixes_rgb_and_differs_from_combine() {
    let base = Highlight {
        rgb: HlAttrs {
            foreground: Some(0x0000ff),
            background: Some(0x000000),
            ..HlAttrs::default()
        },
        ..Highlight::default()
    };
    let veil = Highlight {
        rgb: HlAttrs {
            foreground: Some(0xff0000),
            background: Some(0x00ff00),
            blend: Some(50),
            ..HlAttrs::default()
        },
        ..Highlight::default()
    };
    for (expected_fg, expected_bg, expected_blend, mode) in [
        (Some(0xff0000), Some(0x00ff00), Some(50), ExtmarkHighlightMode::Combine),
        (Some(0x800080), Some(0x008000), None, ExtmarkHighlightMode::Blend),
    ] {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_lines(&[b"hello".to_vec()], true).unwrap(), true)
            .unwrap();
        editor.create_tabpage(buffer, Geometry::new(0, 0, 10, 4).unwrap()).unwrap();
        let namespace = editor.buffer_mut(buffer).unwrap().extmarks.create_namespace("test").unwrap();
        let mut under = ExtmarkPlacement::new(ExtmarkPosition::new(0, 0))
            .with_end(ExtmarkPosition::new(0, 5));
        under.attributes.highlight_group = Some("Base".to_string());
        let mut over = ExtmarkPlacement::new(ExtmarkPosition::new(0, 0))
            .with_end(ExtmarkPosition::new(0, 5));
        over.attributes.highlight_group = Some("Veil".to_string());
        over.attributes.priority = 10;
        over.attributes.highlight_mode = Some(mode);
        {
            let extmarks = &mut editor.buffer_mut(buffer).unwrap().extmarks;
            extmarks.set(namespace, None, under).unwrap();
            extmarks.set(namespace, None, over).unwrap();
        }
        let mut highlights = HlState::new();
        highlights.define_group("Base", base.clone()).unwrap();
        highlights.define_group("Veil", veil.clone()).unwrap();
        let compositor = Compositor::from_editor(&editor, 10, 4, &mut highlights).unwrap();
        let screen = compositor.compose(&mut highlights).unwrap();
        let rendered = highlights.get(screen.grid.cell(0, 0).unwrap().hl_id).unwrap();
        assert_eq!(rendered.rgb.foreground, expected_fg, "foreground for {mode:?}");
        assert_eq!(rendered.rgb.background, expected_bg, "background for {mode:?}");
        assert_eq!(rendered.rgb.blend, expected_blend, "blend for {mode:?}");
    }
}

#[test]
fn combine_preserves_base_default_flag() {
    let mut highlights = HlState::new();
    let base = Highlight {
        rgb: HlAttrs { bold: true, ..HlAttrs::default() },
        default_flag: true,
        ..Highlight::default()
    };
    let overlay = Highlight {
        rgb: HlAttrs { italic: true, ..HlAttrs::default() },
        ..Highlight::default()
    };
    let (base_id, _) = highlights.intern(base).unwrap();
    let (overlay_id, _) = highlights.intern(overlay).unwrap();
    let (combined_id, _) = highlights.combine(base_id, overlay_id).unwrap();
    let combined = highlights.get(combined_id).unwrap();
    assert!(combined.default_flag, "base default flag was overwritten with false");
    assert!(combined.rgb.bold);
    assert!(combined.rgb.italic);
}

#[test]
fn highlight_ids_are_stable_and_definitions_emit_on_change_once() {
    let mut state = HlState::new();
    let highlight = Highlight { rgb: HlAttrs { bold: true, foreground: Some(0x112233), ..HlAttrs::default() }, ..Highlight::default() };
    let (first_id, first_event) = state.intern(highlight.clone()).unwrap();
    let (second_id, second_event) = state.intern(highlight).unwrap();
    assert_eq!(first_id, 1);
    assert_eq!(second_id, first_id);
    assert!(first_event.is_some());
    assert!(second_event.is_none());

    let changed = Highlight { rgb: HlAttrs { italic: true, ..HlAttrs::default() }, ..Highlight::default() };
    assert!(state.redefine(first_id, changed.clone()).unwrap().is_some());
    assert!(state.redefine(first_id, changed).unwrap().is_none());
}

#[test]
fn capabilities_route_multigrid_and_messages_per_channel() {
    let mut grid = Grid::new(2, 4, 2).unwrap();
    grid.put(0, 0, "x", 0, 1).unwrap();
    let mut compositor = Compositor::new(4, 2);
    compositor.push_layer(Layer::new(grid, 0, 0, 0, LayerKind::Window));

    let mut channels = UiChannels::new();
    channels.attach(1, 4, 2, UiOptions { ext_linegrid: true, ext_multigrid: true, ext_messages: true, ..UiOptions::default() }).unwrap();
    channels.attach(2, 4, 2, UiOptions { ext_linegrid: true, ..UiOptions::default() }).unwrap();
    let mut chrome = ChromeState::new();
    chrome.show_message(MessageState {
        kind: OxStr::from("echo"),
        content: vec![ContentChunk::new(0, "hello")],
        replace_last: false,
        history: false,
        append: false,
        id: Object::Nil,
        trigger: OxStr::from(""),
    });

    let frames = Emitter::new().redraw(&mut channels, &compositor, &mut HlState::new(), &mut chrome).unwrap();
    let names_one = event_names(decode(&frames[&1]).unwrap());
    let names_two = event_names(decode(&frames[&2]).unwrap());
    assert!(names_one.contains(&"win_pos".to_owned()));
    assert!(names_one.contains(&"msg_show".to_owned()));
    assert!(!names_two.contains(&"win_pos".to_owned()));
    assert!(!names_two.contains(&"msg_show".to_owned()));
    assert_eq!(names_one.last().map(String::as_str), Some("flush"));
    assert_eq!(names_two.last().map(String::as_str), Some("flush"));
}

#[test]
fn multigrid_initializes_grid_one_and_filters_external_message_layer() {
    let mut message = Grid::new(5, 4, 1).unwrap();
    message.write_text(0, 0, "msg", 0).unwrap();
    let mut compositor = Compositor::new(4, 2);
    compositor.push_layer(Layer::new(message, 1, 0, 0, LayerKind::Message));
    let mut channels = UiChannels::new();
    channels.attach(7, 4, 2, UiOptions { ext_multigrid: true, ext_messages: true, ..UiOptions::default() }).unwrap();
    let mut chrome = ChromeState::new();
    chrome.show_message(MessageState {
        kind: OxStr::from("echo"), content: vec![ContentChunk::new(0, "msg")], replace_last: false,
        history: false, append: false, id: Object::Nil, trigger: OxStr::from(""),
    });
    let frame = Emitter::new().redraw(&mut channels, &compositor, &mut HlState::new(), &mut chrome).unwrap();
    let decoded = decode(&frame[&7]).unwrap();
    let names = event_names(decoded.clone());
    assert!(has_grid_resize(&decoded, 1));
    assert!(!names.contains(&"msg_set_pos".to_owned()));
    assert!(names.contains(&"msg_show".to_owned()));
}

#[test]
fn clearing_fallback_message_emits_blanking_grid_line() {
    let compositor = Compositor::new(8, 2);
    let mut channels = UiChannels::new();
    channels.attach(8, 8, 2, UiOptions { ext_linegrid: true, ..UiOptions::default() }).unwrap();
    let mut chrome = ChromeState::new();
    chrome.show_message(MessageState {
        kind: OxStr::from("echo"), content: vec![ContentChunk::new(0, "hello")], replace_last: false,
        history: false, append: false, id: Object::Nil, trigger: OxStr::from(""),
    });
    let mut emitter = Emitter::new();
    emitter.redraw(&mut channels, &compositor, &mut HlState::new(), &mut chrome).unwrap();
    chrome.clear_message();
    let frame = emitter.redraw(&mut channels, &compositor, &mut HlState::new(), &mut chrome).unwrap();
    assert!(event_names(decode(&frame[&8]).unwrap()).contains(&"grid_line".to_owned()));
}

#[test]
fn initial_redraw_emits_startup_metadata_once() {
    let compositor = Compositor::new(8, 3);
    let mut channels = UiChannels::new();
    channels.attach(12, 8, 3, UiOptions { ext_linegrid: true, ..UiOptions::default() }).unwrap();
    let mut emitter = Emitter::new();
    let mut highlights = HlState::new();
    let mut chrome = ChromeState::new();

    let first = emitter.redraw(&mut channels, &compositor, &mut highlights, &mut chrome).unwrap();
    let first_names = event_names(decode(&first[&12]).unwrap());
    for required in ["option_set", "default_colors_set", "hl_attr_define", "mode_info_set"] {
        assert!(first_names.contains(&required.to_owned()), "missing startup event {required}: {first_names:?}");
    }
    assert_eq!(first_names.last().map(String::as_str), Some("flush"));

    let second = emitter.redraw(&mut channels, &compositor, &mut highlights, &mut chrome).unwrap();
    let second_names = event_names(decode(&second[&12]).unwrap());
    for startup in ["option_set", "default_colors_set", "mode_info_set"] {
        assert!(!second_names.contains(&startup.to_owned()), "repeated startup event {startup}");
    }
}

#[test]
fn mode_events_emit_only_on_transition_and_preserve_order() {
    let mut chrome = ChromeState::new();
    chrome.set_mode_info(true, vec![ModeInfo {
        cursor_shape: OxStr::from("block"),
        cell_percentage: 100,
        attr_id: None,
        short_name: OxStr::from("n"),
        name: OxStr::from("normal"),
    }]);
    chrome.set_mode("normal", 0);
    chrome.set_mode("normal", 0);
    chrome.set_mode("insert", 1);
    let events = chrome.take_events();
    let names: Vec<_> = events.iter().map(|event| event.name.to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["mode_info_set", "mode_change", "mode_change"]);
}

#[test]
fn multigrid_without_ext_messages_renders_message_once_via_compositor_grid() {
    let mut message = Grid::new(5, 4, 1).unwrap();
    message.write_text(0, 0, "msg", 0).unwrap();
    let mut compositor = Compositor::new(4, 2);
    compositor.push_layer(Layer::new(message, 1, 0, 0, LayerKind::Message));
    let mut channels = UiChannels::new();
    channels.attach(
        9, 4, 2,
        UiOptions { ext_linegrid: true, ext_multigrid: true, ..UiOptions::default() },
    ).unwrap();
    let frames = Emitter::new().redraw(&mut channels, &compositor, &mut HlState::new(), &mut ChromeState::new()).unwrap();
    let decoded = decode(&frames[&9]).unwrap();
    // The compositor message grid is the single render path (msg_set_pos + its own grid).
    assert!(event_names(decoded.clone()).contains(&"msg_set_pos".to_owned()));
    // The message text lands on the compositor message grid, not duplicated into grid one.
    assert!(grid_text(&decoded, 5).contains("msg"));
    assert!(!grid_text(&decoded, 1).contains("msg"));
}

#[test]
fn float_compindex_is_distinct_ordered_and_stable_not_channel_id() {
    let mut first = Grid::new(3, 2, 1).unwrap();
    first.put(0, 0, "A", 0, 1).unwrap();
    let mut second = Grid::new(4, 2, 1).unwrap();
    second.put(0, 0, "B", 0, 1).unwrap();
    let mut compositor = Compositor::new(6, 2);
    compositor.push_layer(Layer::new(first, 0, 0, 300, LayerKind::Float));
    compositor.push_layer(Layer::new(second, 0, 3, 300, LayerKind::Float));
    let mut channels = UiChannels::new();
    channels.attach(
        7, 6, 2,
        UiOptions { ext_linegrid: true, ext_multigrid: true, ..UiOptions::default() },
    ).unwrap();
    let frame = Emitter::new().redraw(&mut channels, &compositor, &mut HlState::new(), &mut ChromeState::new()).unwrap();
    let decoded = decode(&frame[&7]).unwrap();
    // Two equal-z-index floats get distinct compindexes in compositor (insertion) order,
    // independent of the channel id.
    let compindexes = win_float_compindexes(&decoded);
    assert_eq!(compindexes.len(), 2);
    assert_eq!(compindexes[&3], 1);
    assert_eq!(compindexes[&4], 2);
    assert!(compindexes.values().all(|&index| index != 7));
    // Stable across a second redraw.
    let frame2 = Emitter::new().redraw(&mut channels, &compositor, &mut HlState::new(), &mut ChromeState::new()).unwrap();
    let decoded2 = decode(&frame2[&7]).unwrap();
    assert_eq!(win_float_compindexes(&decoded2), compindexes);
}

fn grid_text(frame: &Object, target: i64) -> String {
    let Object::Array(frame) = frame else { return String::new() };
    let Some(Object::Array(events)) = frame.get(2) else { return String::new() };
    let mut out = String::new();
    for event in events {
        let Object::Array(parts) = event else { continue };
        let Some(Object::String(name)) = parts.first() else { continue };
        if *name != OxStr::from("grid_line") { continue; }
        let Some(Object::Array(args)) = parts.get(1) else { continue };
        let Some(Object::Integer(grid)) = args.first() else { continue };
        if *grid != target { continue; }
        let Some(Object::Array(cells)) = args.get(3) else { continue };
        for cell_repeat in cells {
            let Object::Array(cell) = cell_repeat else { continue };
            if let Some(Object::String(text)) = cell.first() { out.push_str(&text.to_string_lossy()); }
        }
    }
    out
}

fn win_float_compindexes(frame: &Object) -> BTreeMap<i64, i64> {
    let Object::Array(frame) = frame else { return BTreeMap::new() };
    let Some(Object::Array(events)) = frame.get(2) else { return BTreeMap::new() };
    let mut map = BTreeMap::new();
    for event in events {
        let Object::Array(parts) = event else { continue };
        let Some(Object::String(name)) = parts.first() else { continue };
        if *name != OxStr::from("win_float_pos") { continue; }
        let Some(Object::Array(args)) = parts.get(1) else { continue };
        let (Some(Object::Integer(grid)), Some(Object::Integer(compindex))) =
            (args.first(), args.get(8)) else { continue };
        map.insert(*grid, *compindex);
    }
    map
}

fn has_grid_resize(frame: &Object, grid: i64) -> bool {
    let Object::Array(frame) = frame else { return false };
    let Some(Object::Array(events)) = frame.get(2) else { return false };
    events.iter().any(|event| {
        let Object::Array(parts) = event else { return false };
        matches!(parts.first(), Some(Object::String(name)) if *name == OxStr::from("grid_resize"))
            && matches!(parts.get(1), Some(Object::Array(args)) if args.first() == Some(&Object::Integer(grid)))
    })
}

fn event_names(frame: Object) -> Vec<String> {
    let Object::Array(frame) = frame else { return Vec::new() };
    let Some(Object::Array(events)) = frame.get(2) else { return Vec::new() };
    events.iter().filter_map(|event| {
        let Object::Array(parts) = event else { return None };
        let Some(Object::String(name)) = parts.first() else { return None };
        Some(name.to_string_lossy().into_owned())
    }).collect()
}
