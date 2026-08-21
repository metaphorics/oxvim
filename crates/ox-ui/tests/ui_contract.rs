#![allow(clippy::unwrap_used)]

use ox_rpc::decode;
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
