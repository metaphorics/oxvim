#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::cell::RefCell;
use std::rc::Rc;

use ox_editor::{AutocmdAction, Editor, Geometry, K_SPECIAL, KS_EXTRA};
use ox_text::Buffer;

use crate::{ApiError, Dict, Object, OxStr, TypeRef};

fn dict(entries: &[(&str, Object)]) -> Dict {
    Dict(entries.iter().map(|(key, value)| (OxStr::from(*key), value.clone())).collect())
}

fn editor_with_lines(lines: &[&str]) -> (Editor, crate::BufHandle, crate::TabHandle, crate::WinHandle) {
    let mut editor = Editor::new();
    let lines = lines.iter().map(|line| line.as_bytes().to_vec()).collect::<Vec<_>>();
    let buffer = editor
        .create_buffer_with(Buffer::from_lines(&lines, false).unwrap(), true)
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    (editor, buffer, tab, window)
}

#[test]
fn buffer_lines_honor_negative_clamping_and_strict_errors() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one", "two", "three"]);
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&mut editor, buffer, -2, -1, true),
        Ok(vec![OxStr::from("three")])
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&mut editor, buffer, -99, 99, false),
        Ok(vec![OxStr::from("one"), OxStr::from("two"), OxStr::from("three")])
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&mut editor, buffer, -99, 1, true),
        Err(ApiError::validation("Index out of bounds"))
    );
    crate::buffer::nvim_buf_set_lines(
        &mut editor,
        buffer,
        1,
        2,
        true,
        vec![OxStr::from("replaced")],
    )
    .unwrap();
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&mut editor, buffer, 0, -1, true).unwrap(),
        [OxStr::from("one"), OxStr::from("replaced"), OxStr::from("three")]
    );
}

#[test]
fn buffer_mutations_advance_changedtick() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one"]);
    let before = crate::buffer::nvim_buf_get_changedtick(&mut editor, buffer).unwrap();
    crate::buffer::nvim_buf_set_text(
        &mut editor,
        buffer,
        0,
        1,
        0,
        2,
        vec![OxStr::from("X")],
    )
    .unwrap();
    assert!(crate::buffer::nvim_buf_get_changedtick(&mut editor, buffer).unwrap() > before);
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&mut editor, buffer, 0, -1, true).unwrap(),
        [OxStr::from("oXe")]
    );
}

#[test]
fn forced_buffer_delete_rehomes_attached_windows() {
    let (mut editor, buffer, _, window) = editor_with_lines(&["one"]);
    crate::buffer::nvim_buf_delete(
        &mut editor,
        buffer,
        dict(&[("force", Object::Boolean(true))]),
    )
    .unwrap();
    assert!(editor.buffer(buffer).is_err());
    assert_ne!(editor.window(window).unwrap().buffer, buffer);
}

#[test]
fn cursor_columns_clamp_but_rows_validate() {
    let (mut editor, _, _, window) = editor_with_lines(&["abc", "z"]);
    crate::window::nvim_win_set_cursor(&mut editor, window, vec![1, 99]).unwrap();
    assert_eq!(crate::window::nvim_win_get_cursor(&mut editor, window), Ok(vec![1, 3]));
    assert_eq!(
        crate::window::nvim_win_set_cursor(&mut editor, window, vec![3, 0]),
        Err(ApiError::validation("Cursor row outside buffer"))
    );
}

#[test]
fn floating_windows_validate_round_trip_and_close() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one"]);
    let invalid = dict(&[
        ("relative", Object::String(OxStr::from("editor"))),
        ("row", Object::Float(1.0)),
        ("col", Object::Float(2.0)),
        ("width", Object::Integer(0)),
        ("height", Object::Integer(2)),
    ]);
    assert!(matches!(
        crate::window::nvim_open_win(&mut editor, buffer, false, invalid),
        Err(ApiError::Validation(_))
    ));

    let config = dict(&[
        ("relative", Object::String(OxStr::from("editor"))),
        ("row", Object::Float(1.0)),
        ("col", Object::Float(2.0)),
        ("width", Object::Integer(10)),
        ("height", Object::Integer(2)),
        ("border", Object::String(OxStr::from("single"))),
    ]);
    let float = crate::window::nvim_open_win(&mut editor, buffer, false, config).unwrap();
    let returned = crate::window::nvim_win_get_config(&mut editor, float).unwrap();
    assert_eq!(returned.get(&OxStr::from("width")), Some(&Object::Integer(10)));
    assert_eq!(crate::window::nvim_win_get_position(&mut editor, float), Ok(vec![2, 3]));
    crate::window::nvim_win_close(&mut editor, float, false).unwrap();
    assert_eq!(crate::window::nvim_win_is_valid(&mut editor, float), Ok(false));
}

#[test]
fn tabpage_lists_and_selects_real_windows() {
    let (mut editor, buffer, tab, window) = editor_with_lines(&["one"]);
    let second = editor.split_vertical(tab, window, buffer).unwrap();
    assert_eq!(
        crate::tabpage::nvim_tabpage_list_wins(&mut editor, tab).unwrap(),
        vec![window, second]
    );
    crate::tabpage::nvim_tabpage_set_win(&mut editor, tab, window).unwrap();
    assert_eq!(crate::tabpage::nvim_tabpage_get_win(&mut editor, tab), Ok(window));
}

#[test]
fn option_value_scope_distinguishes_global_and_local() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    crate::global::nvim_set_option_value(
        &mut editor,
        OxStr::from("autocomplete"),
        Object::Boolean(false),
        dict(&[("scope", Object::String(OxStr::from("global")))]),
    )
    .unwrap();
    crate::global::nvim_set_option_value(
        &mut editor,
        OxStr::from("autocomplete"),
        Object::Boolean(true),
        dict(&[("scope", Object::String(OxStr::from("local")))]),
    )
    .unwrap();
    assert_eq!(
        crate::global::nvim_get_option_value(
            &mut editor,
            OxStr::from("autocomplete"),
            dict(&[("scope", Object::String(OxStr::from("global")))])
        ),
        Ok(Object::Boolean(false))
    );
    assert_eq!(
        crate::global::nvim_get_option_value(
            &mut editor,
            OxStr::from("autocomplete"),
            dict(&[("scope", Object::String(OxStr::from("local")))])
        ),
        Ok(Object::Boolean(true))
    );
}

#[test]
fn core_registry_metadata_matches_cross_family_sample() {
    let registry = crate::core().unwrap();
    assert_eq!(registry.len(), 262);
    let expected = [
        ("nvim_buf_get_lines", 1, TypeRef::ArrayOf(&TypeRef::String)),
        ("nvim_buf_get_text", 9, TypeRef::ArrayOf(&TypeRef::String)),
        ("nvim_buf_get_changedtick", 2, TypeRef::Integer),
        ("nvim_win_get_cursor", 1, TypeRef::Named("ArrayOf(Integer, 2)")),
        ("nvim_open_win", 6, TypeRef::Window),
        ("nvim_win_set_hl_ns", 10, TypeRef::Void),
        ("nvim_tabpage_list_wins", 1, TypeRef::ArrayOf(&TypeRef::Window)),
        ("nvim_tabpage_set_win", 12, TypeRef::Void),
        ("nvim_get_option_value", 9, TypeRef::Object),
        ("nvim_echo", 7, TypeRef::Object),
    ];
    for (name, since, returns) in expected {
        let metadata = registry.get(name).unwrap().0;
        assert_eq!((metadata.since, metadata.returns), (since, returns), "{name}");
        if name.starts_with("nvim_win_") {
            assert!(metadata.method, "{name} must advertise a window receiver");
        }
    }
}

#[test]
fn registry_dispatch_converts_objects_and_preserves_api_errors() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one", "two"]);
    let registry = crate::core().unwrap();
    let dispatch = registry.get("nvim_buf_line_count").unwrap().1;
    assert_eq!(dispatch(&mut editor, &[Object::Buffer(buffer)]), Ok(Object::Integer(2)));
    assert_eq!(
        dispatch(&mut editor, &[Object::String(OxStr::from("bad"))]),
        Err(ApiError::exception(
            "Wrong type for argument 1 when calling nvim_buf_line_count, expecting Buffer"
        ))
    );
}

#[test]
fn buffer_delete_rehomes_windows_without_force() {
    // Rehoming windows off a deleted buffer must not depend on `force`;
    // `force` only overrides unsaved-change protection (buffer.c:1039-1059).
    let (mut editor, buffer, _, window) = editor_with_lines(&["one"]);
    crate::buffer::nvim_buf_delete(&mut editor, buffer, dict(&[])).unwrap();
    assert!(editor.buffer(buffer).is_err());
    assert_ne!(editor.window(window).unwrap().buffer, buffer);
}

#[test]
fn buffer_delete_rehomes_windows_with_explicit_false_force() {
    let (mut editor, buffer, _, window) = editor_with_lines(&["one"]);
    crate::buffer::nvim_buf_delete(
        &mut editor,
        buffer,
        dict(&[("force", Object::Boolean(false))]),
    )
    .unwrap();
    assert!(editor.buffer(buffer).is_err());
    assert_ne!(editor.window(window).unwrap().buffer, buffer);
}

#[test]
fn cursor_column_rejects_values_above_maxcol_before_clamping() {
    // Upstream rejects col > MAXCOL before the silent clamp
    // (api/window.c:122-130, pos_defs.h MAXCOL = 0x7fffffff).
    let (mut editor, _, _, window) = editor_with_lines(&["abc", "z"]);
    assert_eq!(
        crate::window::nvim_win_set_cursor(&mut editor, window, vec![1, i64::MAX]),
        Err(ApiError::validation("Invalid cursor column: out of range"))
    );
    assert_eq!(
        crate::window::nvim_win_set_cursor(&mut editor, window, vec![1, -1]),
        Err(ApiError::validation("Invalid cursor column: out of range"))
    );
    // MAXCOL itself is accepted, then clamped to the line length.
    crate::window::nvim_win_set_cursor(&mut editor, window, vec![1, 0x7fff_ffff]).unwrap();
    assert_eq!(crate::window::nvim_win_get_cursor(&mut editor, window), Ok(vec![1, 3]));
}

#[derive(Clone)]
struct CapturingAutocmds(Rc<RefCell<Vec<u64>>>);

impl crate::AutocmdExecutor for CapturingAutocmds {
    fn execute(&mut self, action: &AutocmdAction) -> Result<(), String> {
        self.0.borrow_mut().push(action.id);
        Ok(())
    }
}

#[test]
fn autocmd_round_trip_shape_clear_and_definition_order() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    let first = crate::autocmd::nvim_create_autocmd(
        &mut editor, Object::String(OxStr::from("BufEnter")),
        dict(&[("pattern", Object::String(OxStr::from("*.rs"))), ("command", Object::String(OxStr::from("first")))]),
    ).unwrap();
    let second = crate::autocmd::nvim_create_autocmd(
        &mut editor, Object::String(OxStr::from("BufEnter")),
        dict(&[("pattern", Object::String(OxStr::from("*.rs"))), ("command", Object::String(OxStr::from("second")))]),
    ).unwrap();
    let returned = crate::autocmd::nvim_get_autocmds(&mut editor, dict(&[("event", Object::String(OxStr::from("BufEnter")))])).unwrap();
    assert_eq!(returned.len(), 2);
    let keys = returned[0].iter().map(|(key, _)| key.to_string_lossy().into_owned()).collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys, ["command", "desc", "event", "group", "id", "once", "pattern"].into_iter().map(str::to_owned).collect());
    let captured = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(&editor, Box::new(CapturingAutocmds(captured.clone())));
    crate::autocmd::nvim_exec_autocmds(&mut editor, Object::String(OxStr::from("BufEnter")), dict(&[("pattern", Object::String(OxStr::from("file.rs")))])).unwrap();
    assert_eq!(&*captured.borrow(), &[first as u64, second as u64]);
    crate::autocmd::nvim_clear_autocmds(&mut editor, dict(&[("event", Object::String(OxStr::from("BufEnter"))), ("pattern", Object::String(OxStr::from("*.rs")))] )).unwrap();
    assert!(crate::autocmd::nvim_get_autocmds(&mut editor, dict(&[])).unwrap().is_empty());
}

#[test]
fn extmark_details_order_limit_delete_and_clear() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one", "two"]);
    let namespace = crate::extmark::nvim_create_namespace(&mut editor, OxStr::from("tests")).unwrap();
    let first = crate::extmark::nvim_buf_set_extmark(&mut editor, buffer, namespace, 0, 1, dict(&[("right_gravity", Object::Boolean(false)), ("end_row", Object::Integer(1)), ("end_col", Object::Integer(2)), ("hl_group", Object::String(OxStr::from("Visual")))] )).unwrap();
    let second = crate::extmark::nvim_buf_set_extmark(&mut editor, buffer, namespace, 1, 0, dict(&[])).unwrap();
    let marks = crate::extmark::nvim_buf_get_extmarks(&mut editor, buffer, namespace, Object::Array(vec![Object::Integer(0), Object::Integer(0)]), Object::Integer(-1), dict(&[("details", Object::Boolean(true)), ("limit", Object::Integer(1))])).unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0][0], Object::Integer(first));
    let Object::Dict(details) = &marks[0][3] else { panic!("missing details") };
    assert_eq!(details.get(&OxStr::from("right_gravity")), Some(&Object::Boolean(false)));
    assert!(crate::extmark::nvim_buf_del_extmark(&mut editor, buffer, namespace, second).unwrap());
    crate::extmark::nvim_buf_clear_namespace(&mut editor, buffer, namespace, 0, -1).unwrap();
    assert!(crate::extmark::nvim_buf_get_extmark_by_id(&mut editor, buffer, namespace, first, dict(&[])).unwrap().is_empty());
}

#[test]
fn context_channel_and_ui_round_trip() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    editor.vvars_mut().0.push((OxStr::from("answer"), Object::Integer(42)));
    let context = crate::context::nvim_get_context(&mut editor, dict(&[("types", Object::Array(vec![Object::String(OxStr::from("gvars")), Object::String(OxStr::from("bufs"))]))])).unwrap();
    editor.vvars_mut().0.clear();
    crate::context::nvim_load_context(&mut editor, context).unwrap();
    assert_eq!(editor.vvars().get(&OxStr::from("answer")), Some(&Object::Integer(42)));

    crate::channel::nvim_set_client_info(&mut editor, OxStr::from("tests"), dict(&[("major", Object::Integer(1))]), OxStr::from("remote"), dict(&[]), dict(&[])).unwrap();
    let info = crate::channel::nvim_get_chan_info(&mut editor, 1).unwrap();
    assert_eq!(info.iter().map(|(key, _)| key.to_string_lossy().into_owned()).collect::<std::collections::BTreeSet<_>>(), ["client", "id", "mode", "stream"].into_iter().map(str::to_owned).collect());
    assert!(crate::channel::nvim_set_client_info(&mut editor, OxStr::from("bad"), dict(&[]), OxStr::from("invalid"), dict(&[]), dict(&[])).is_err());

    crate::ui::nvim_ui_attach(&mut editor, 80, 24, dict(&[("ext_linegrid", Object::Boolean(true)), ("ext_hlstate", Object::Boolean(true))])).unwrap();
    assert_eq!(crate::ui::nvim_list_uis(&mut editor).unwrap().len(), 1);
    crate::ui::nvim_set_hl(&mut editor, 0, OxStr::from("Task11b"), dict(&[("fg", Object::Integer(0x112233)), ("bold", Object::Boolean(true))])).unwrap();
    let highlight = crate::ui::nvim_get_hl(&mut editor, 0, dict(&[("name", Object::String(OxStr::from("Task11b")))])).unwrap();
    assert_eq!(highlight.get(&OxStr::from("foreground")), Some(&Object::Integer(0x112233)));
    crate::ui::nvim_set_hl_ns(&mut editor, 9).unwrap();
    assert_eq!(crate::ui::nvim_get_hl_ns(&mut editor, dict(&[])), Ok(9));
}

#[test]
fn new_family_metadata_and_dispatch_are_registered() {
    let registry = crate::core().unwrap();
    for (name, since, deprecated) in [("nvim_create_autocmd", 9, None), ("nvim_buf_set_extmark", 7, None), ("nvim_get_context", 6, None), ("nvim_ui_attach", 1, None), ("nvim_buf_get_number", 1, Some(2))] {
        let metadata = registry.get(name).unwrap().0;
        assert_eq!((metadata.since, metadata.deprecated_since), (since, deprecated));
    }
    let mut editor = Editor::new();
    let dispatch = registry.get("nvim_create_namespace").unwrap().1;
    assert_eq!(dispatch(&mut editor, &[Object::String(OxStr::from("dispatch"))]), Ok(Object::Integer(1)));
    let dispatch = registry.get("nvim_list_uis").unwrap().1;
    assert_eq!(dispatch(&mut editor, &[]), Ok(Object::Array(Vec::new())));
}

#[test]
fn open_win_split_creates_tiled_window() {
    let (mut editor, buffer, _, window) = editor_with_lines(&["one"]);
    let split = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[("split", Object::String(OxStr::from("right")))]),
    )
    .unwrap();
    assert_ne!(split, window);
    // Tiled windows carry no float config: `relative` is empty.
    let config = crate::window::nvim_win_get_config(&mut editor, split).unwrap();
    assert_eq!(
        config.get(&OxStr::from("relative")),
        Some(&Object::String(OxStr::from("")))
    );
}

#[test]
fn open_win_split_honors_four_way_direction() {
    // "left"/"above" place the new window before the target; "right"/"below"
    // place it after, matching upstream split directions. Preorder order shows
    // the side each direction lands on.
    let (mut editor, buffer, tab, window) = editor_with_lines(&["one"]);
    let left = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[("split", Object::String(OxStr::from("left")))]),
    )
    .unwrap();
    assert_eq!(editor.tabpage(tab).unwrap().windows(), vec![left, window]);

    let (mut editor, buffer, tab, window) = editor_with_lines(&["one"]);
    let right = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[("split", Object::String(OxStr::from("right")))]),
    )
    .unwrap();
    assert_eq!(editor.tabpage(tab).unwrap().windows(), vec![window, right]);

    let (mut editor, buffer, tab, window) = editor_with_lines(&["one"]);
    let above = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[("split", Object::String(OxStr::from("above")))]),
    )
    .unwrap();
    assert_eq!(editor.tabpage(tab).unwrap().windows(), vec![above, window]);

    let (mut editor, buffer, tab, window) = editor_with_lines(&["one"]);
    let below = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[("split", Object::String(OxStr::from("below")))]),
    )
    .unwrap();
    assert_eq!(editor.tabpage(tab).unwrap().windows(), vec![window, below]);
}

#[test]
fn open_win_split_honors_config_win_target_on_any_tabpage() {
    let (mut editor, buffer, tab, _) = editor_with_lines(&["one"]);
    let other_tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let far = editor.tabpage(other_tab).unwrap().current_window();
    // The `win` config selects the split target even though it lives on a
    // different (non-current) tabpage.
    let split = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[
            ("split", Object::String(OxStr::from("right"))),
            ("win", Object::Window(far)),
        ]),
    )
    .unwrap();
    // The new window joins its target on `other_tab`, not the current tab.
    assert_eq!(editor.tabpage(other_tab).unwrap().windows(), vec![far, split]);
    assert!(!editor.tabpage(tab).unwrap().windows().contains(&split));
    // Passing a floating window as the split target is rejected.
    let float = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("editor"))),
            ("row", Object::Float(0.0)),
            ("col", Object::Float(0.0)),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(2)),
        ]),
    )
    .unwrap();
    assert_eq!(
        crate::window::nvim_open_win(
            &mut editor,
            buffer,
            false,
            dict(&[
                ("split", Object::String(OxStr::from("right"))),
                ("win", Object::Window(float)),
            ]),
        ),
        Err(ApiError::exception("Cannot split a floating window"))
    );
}

#[test]
fn open_win_external_reports_typed_not_implemented() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one"]);
    assert_eq!(
        crate::window::nvim_open_win(
            &mut editor,
            buffer,
            false,
            dict(&[("external", Object::Boolean(true))]),
        ),
        Err(ApiError::exception(
            "Not implemented: external floating windows require a UI layer"
        ))
    );
}

#[test]
fn open_win_accepts_style_focusable_hide_noautocmd() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one"]);
    let float = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("editor"))),
            ("row", Object::Float(1.0)),
            ("col", Object::Float(2.0)),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(2)),
            ("style", Object::String(OxStr::from("minimal"))),
            ("focusable", Object::Boolean(false)),
            ("hide", Object::Boolean(false)),
            ("noautocmd", Object::Boolean(true)),
            ("zindex", Object::Integer(10)),
        ]),
    )
    .unwrap();
    assert_eq!(crate::window::nvim_win_get_position(&mut editor, float), Ok(vec![1, 2]));
    assert_eq!(crate::window::nvim_win_is_valid(&mut editor, float), Ok(true));
    // Unknown style values are rejected.
    let invalid = dict(&[
        ("relative", Object::String(OxStr::from("editor"))),
        ("row", Object::Float(0.0)),
        ("col", Object::Float(0.0)),
        ("width", Object::Integer(1)),
        ("height", Object::Integer(1)),
        ("style", Object::String(OxStr::from("fancy"))),
    ]);
    assert!(matches!(
        crate::window::nvim_open_win(&mut editor, buffer, false, invalid),
        Err(ApiError::Validation(_))
    ));
}

#[test]
fn open_win_bufpos_supplies_default_row_and_col() {
    // bufpos ([line, column]) is valid only with relative="win" and supplies
    // row/col defaults: row=1 (NW anchor), col=0 when neither is given
    // (api/window.c:1307-1320). With a [0, 0] bufpos the float is anchored
    // to the first buffer cell, so the resolved geometry is the default offset.
    let (mut editor, buffer, _, _) = editor_with_lines(&["one"]);
    let float = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("win"))),
            ("bufpos", Object::Array(vec![Object::Integer(0), Object::Integer(0)])),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(2)),
        ]),
    )
    .unwrap();
    assert_eq!(crate::window::nvim_win_get_position(&mut editor, float), Ok(vec![1, 0]));
}

#[test]
fn open_win_bufpos_changes_resolved_geometry() {
    // Buffer-relative float placement must depend on the supplied bufpos:
    // [1, 0] and [20, 40] resolve to different screen cells in the window.
    let owned: Vec<String> = (0..30).map(|i| format!("line {}", i)).collect();
    let lines: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
    let (mut editor, buffer, _, _) = editor_with_lines(&lines);
    let first = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("win"))),
            ("bufpos", Object::Array(vec![Object::Integer(1), Object::Integer(0)])),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(2)),
        ]),
    )
    .unwrap();
    let first_pos = crate::window::nvim_win_get_position(&mut editor, first).unwrap();
    let second = crate::window::nvim_open_win(
        &mut editor,
        buffer,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("win"))),
            ("bufpos", Object::Array(vec![Object::Integer(20), Object::Integer(40)])),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(2)),
        ]),
    )
    .unwrap();
    let second_pos = crate::window::nvim_win_get_position(&mut editor, second).unwrap();
    assert_ne!(first_pos, second_pos);
    assert_eq!(first_pos, vec![2, 0]);
    assert_eq!(second_pos, vec![21, 40]);
}

#[test]
fn open_win_rejects_invalid_bufpos_combinations() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one"]);
    // A one-element "array" is rejected with a typed Validation error (no panic).
    assert_eq!(
        crate::window::nvim_open_win(
            &mut editor,
            buffer,
            false,
            dict(&[
                ("relative", Object::String(OxStr::from("win"))),
                ("bufpos", Object::Array(vec![Object::Integer(2)])),
                ("width", Object::Integer(10)),
                ("height", Object::Integer(2)),
            ]),
        ),
        Err(ApiError::validation("Invalid 'config.bufpos': expected [line, column] array of length 2"))
    );
    // bufpos anchors float geometry to a window's text, so it is only valid
    // together with relative="win".
    assert_eq!(
        crate::window::nvim_open_win(
            &mut editor,
            buffer,
            false,
            dict(&[
                ("relative", Object::String(OxStr::from("editor"))),
                ("bufpos", Object::Array(vec![Object::Integer(2), Object::Integer(3)])),
                ("width", Object::Integer(10)),
                ("height", Object::Integer(2)),
            ]),
        ),
        Err(ApiError::validation("Invalid 'config.bufpos': only valid when relative is 'win'"))
    );
}

#[test]
fn open_win_accepts_highlight_tuple_borders_and_titles() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one"]);
    let config = dict(&[
        ("relative", Object::String(OxStr::from("editor"))),
        ("row", Object::Float(1.0)),
        ("col", Object::Float(2.0)),
        ("width", Object::Integer(10)),
        ("height", Object::Integer(2)),
        ("border", Object::Array(vec![
            Object::Array(vec![
                Object::String(OxStr::from("+")),
                Object::String(OxStr::from("MyCorner")),
            ]),
            Object::String(OxStr::from("x")),
        ])),
        ("title", Object::Array(vec![Object::Array(vec![
            Object::String(OxStr::from("Doc")),
            Object::String(OxStr::from("FloatTitle")),
        ])])),
        ("footer", Object::Array(vec![Object::String(OxStr::from("read-only"))])),
    ]);
    let float = crate::window::nvim_open_win(&mut editor, buffer, false, config).unwrap();
    assert_eq!(crate::window::nvim_win_is_valid(&mut editor, float), Ok(true));
}

#[test]
fn replace_termcodes_translates_named_special_keys() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    let mut termcodes = |input: &str, do_lt: bool, special: bool| {
        crate::global::nvim_replace_termcodes(
            &mut editor,
            OxStr::from(input),
            true,
            do_lt,
            special,
        )
        .unwrap()
    };
    // Named special keys become the internal three-byte keycode form.
    assert_eq!(termcodes("<Up>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'u']);
    assert_eq!(termcodes("<Down>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'd']);
    assert_eq!(termcodes("<Left>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'l']);
    assert_eq!(termcodes("<Right>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'r']);
    assert_eq!(termcodes("<Home>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'h']);
    assert_eq!(termcodes("<End>", true, true).as_bytes(), &[K_SPECIAL, b'@', b'7']);
    assert_eq!(termcodes("<Del>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'D']);
    assert_eq!(termcodes("<PageUp>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'P']);
    assert_eq!(termcodes("<PageDown>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'N']);
    assert_eq!(termcodes("<F1>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'1']);
    assert_eq!(termcodes("<F10>", true, true).as_bytes(), &[K_SPECIAL, b'k', b';']);
    assert_eq!(termcodes("<F11>", true, true).as_bytes(), &[K_SPECIAL, b'F', b'1']);
    assert_eq!(termcodes("<F12>", true, true).as_bytes(), &[K_SPECIAL, b'F', b'2']);
    // <BS> and <Tab> are special keys (K_BS, K_TAB), not literal control bytes.
    assert_eq!(termcodes("<BS>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'b']);
    assert_eq!(termcodes("<Tab>", true, true).as_bytes(), &[K_SPECIAL, KS_EXTRA, 54]);
    // Editing/document keys (K_INS, K_HELP, K_UNDO).
    assert_eq!(termcodes("<Insert>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'I']);
    assert_eq!(termcodes("<Ins>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'I']);
    assert_eq!(termcodes("<Help>", true, true).as_bytes(), &[K_SPECIAL, b'%', b'1']);
    assert_eq!(termcodes("<Undo>", true, true).as_bytes(), &[K_SPECIAL, b'&', b'8']);
    // Shifted Tab (K_S_TAB).
    assert_eq!(termcodes("<S-Tab>", true, true).as_bytes(), &[K_SPECIAL, b'k', b'B']);
    // Keypad keys (k0-k9 and k-prefixed navigation/arithmetic).
    assert_eq!(termcodes("<k0>", true, true).as_bytes(), &[K_SPECIAL, b'K', b'C']);
    assert_eq!(termcodes("<kUp>", true, true).as_bytes(), &[K_SPECIAL, b'K', b'u']);
    assert_eq!(termcodes("<kEnd>", true, true).as_bytes(), &[K_SPECIAL, b'K', b'4']);
    assert_eq!(termcodes("<kPlus>", true, true).as_bytes(), &[K_SPECIAL, b'K', b'6']);
    // Shifted and control cursor keys.
    assert_eq!(termcodes("<S-Up>", true, true).as_bytes(), &[K_SPECIAL, KS_EXTRA, 4]);
    assert_eq!(termcodes("<S-Down>", true, true).as_bytes(), &[K_SPECIAL, KS_EXTRA, 5]);
    assert_eq!(termcodes("<S-Left>", true, true).as_bytes(), &[K_SPECIAL, b'#', b'4']);
    assert_eq!(termcodes("<S-Right>", true, true).as_bytes(), &[K_SPECIAL, b'%', b'i']);
    assert_eq!(termcodes("<S-Home>", true, true).as_bytes(), &[K_SPECIAL, b'#', b'2']);
    assert_eq!(termcodes("<C-Left>", true, true).as_bytes(), &[K_SPECIAL, KS_EXTRA, 85]);
    assert_eq!(termcodes("<C-Right>", true, true).as_bytes(), &[K_SPECIAL, KS_EXTRA, 86]);
    assert_eq!(termcodes("<C-Home>", true, true).as_bytes(), &[K_SPECIAL, KS_EXTRA, 87]);
    assert_eq!(termcodes("<C-End>", true, true).as_bytes(), &[K_SPECIAL, KS_EXTRA, 88]);
    // Function keys beyond F12 (computed K_F13..K_F63 bytes).
    assert_eq!(termcodes("<F13>", true, true).as_bytes(), &[K_SPECIAL, b'F', b'3']);
    assert_eq!(termcodes("<F20>", true, true).as_bytes(), &[K_SPECIAL, b'F', b'A']);
    assert_eq!(termcodes("<F40>", true, true).as_bytes(), &[K_SPECIAL, b'F', b'U']);
    assert_eq!(termcodes("<F41>", true, true).as_bytes(), &[K_SPECIAL, b'F', b'V']);
    assert_eq!(termcodes("<F46>", true, true).as_bytes(), &[K_SPECIAL, b'F', b'a']);
    assert_eq!(termcodes("<F63>", true, true).as_bytes(), &[K_SPECIAL, b'F', b'r']);
    // Shifted function keys and extra xterm keys.
    assert_eq!(termcodes("<S-F1>", true, true).as_bytes(), &[K_SPECIAL, KS_EXTRA, 6]);
    assert_eq!(termcodes("<S-F12>", true, true).as_bytes(), &[K_SPECIAL, KS_EXTRA, 17]);
    assert_eq!(termcodes("<xUp>", true, true).as_bytes(), &[K_SPECIAL, KS_EXTRA, 65]);
    // Literal control keys remain single bytes.
    assert_eq!(termcodes("<CR>", true, true).as_bytes(), &[b'\r']);
    assert_eq!(termcodes("<Esc>", true, true).as_bytes(), &[0x1b]);
    assert_eq!(termcodes("<Space>", true, true).as_bytes(), &[b' ']);
    // <lt> only translates when do_lt is set; special=false leaves keycodes.
    assert_eq!(termcodes("<lt>", true, true).as_bytes(), &[b'<']);
    assert_eq!(termcodes("<lt>", false, true).as_bytes(), b"<lt>");
    assert_eq!(termcodes("<CR>", true, false).as_bytes(), b"<CR>");
}

#[test]
fn clear_autocmds_omitted_group_only_targets_default_group() {
    // api.txt `nvim_clear_autocmds()`: an omitted group matches autocommands
    // that are in NO group (the default augroup), not every group.
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    let group_id = crate::autocmd::nvim_create_augroup(
        &mut editor,
        OxStr::from("mine"),
        dict(&[("clear", Object::Boolean(false))]),
    )
    .unwrap();
    crate::autocmd::nvim_create_autocmd(
        &mut editor,
        Object::String(OxStr::from("BufEnter")),
        dict(&[("command", Object::String(OxStr::from("default")))]),
    )
    .unwrap();
    crate::autocmd::nvim_create_autocmd(
        &mut editor,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("group", Object::Integer(group_id)),
            ("command", Object::String(OxStr::from("grouped"))),
        ]),
    )
    .unwrap();
    crate::autocmd::nvim_clear_autocmds(
        &mut editor,
        dict(&[("event", Object::String(OxStr::from("BufEnter")))]),
    )
    .unwrap();
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(&mut editor, dict(&[("event", Object::String(OxStr::from("BufEnter")))]))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(&mut editor, dict(&[("group", Object::Integer(group_id))]))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn get_extmarks_all_namespaces_and_mark_id_bounds() {
    // api.txt `nvim_buf_get_extmarks()`: ns_id -1 queries every namespace, and
    // start/end may be valid extmark ids whose positions define the bounds.
    let (mut editor, buffer, _, _) = editor_with_lines(&["one", "two", "three"]);
    let ns_a = crate::extmark::nvim_create_namespace(&mut editor, OxStr::from("a")).unwrap();
    let ns_b = crate::extmark::nvim_create_namespace(&mut editor, OxStr::from("b")).unwrap();
    let m1 = crate::extmark::nvim_buf_set_extmark(&mut editor, buffer, ns_a, 0, 0, dict(&[])).unwrap();
    let m2 = crate::extmark::nvim_buf_set_extmark(&mut editor, buffer, ns_b, 2, 0, dict(&[])).unwrap();

    let all = crate::extmark::nvim_buf_get_extmarks(
        &mut editor, buffer, -1,
        Object::Array(vec![Object::Integer(0), Object::Integer(0)]),
        Object::Integer(-1),
        dict(&[]),
    )
    .unwrap();
    assert_eq!(all.len(), 2);
    for id in [m1, m2] {
        assert!(all.iter().any(|mark| mark[0] == Object::Integer(id)));
    }

    // Positive integer bounds are extmark ids resolved within the namespace.
    let in_ns = crate::extmark::nvim_buf_get_extmarks(
        &mut editor, buffer, ns_a,
        Object::Integer(m1), Object::Integer(m1),
        dict(&[]),
    )
    .unwrap();
    assert_eq!(in_ns.len(), 1);
    assert_eq!(in_ns[0][0], Object::Integer(m1));

    // All-namespace queries cannot resolve an id bound to one namespace.
    assert!(crate::extmark::nvim_buf_get_extmarks(
        &mut editor, buffer, -1,
        Object::Integer(m2), Object::Integer(-1),
        dict(&[]),
    )
    .is_err());
}

#[test]
fn set_extmark_strict_rejects_out_of_buffer_and_line() {
    // api.txt `nvim_buf_set_extmark()` `strict` (default true): the mark is not
    // placed if the line is past end-of-buffer or the column past end-of-line.
    let (mut editor, buffer, _, _) = editor_with_lines(&["one", "two"]);
    let ns = crate::extmark::nvim_create_namespace(&mut editor, OxStr::from("s")).unwrap();
    assert!(crate::extmark::nvim_buf_set_extmark(&mut editor, buffer, ns, 5, 0, dict(&[])).is_err());
    assert!(crate::extmark::nvim_buf_set_extmark(&mut editor, buffer, ns, 0, 50, dict(&[])).is_err());
    assert!(crate::extmark::nvim_buf_set_extmark(
        &mut editor, buffer, ns, 0, 0,
        dict(&[("end_row", Object::Integer(9)), ("end_col", Object::Integer(0))]),
    )
    .is_err());
    // strict=false allows out-of-range placement.
    let id = crate::extmark::nvim_buf_set_extmark(
        &mut editor, buffer, ns, 5, 50,
        dict(&[("strict", Object::Boolean(false))]),
    )
    .unwrap();
    let marks = crate::extmark::nvim_buf_get_extmarks(
        &mut editor, buffer, ns,
        Object::Integer(0), Object::Integer(-1),
        dict(&[]),
    )
    .unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0][0], Object::Integer(id));
}

#[test]
fn highlight_groups_are_namespace_scoped_and_activatible() {
    // api.txt `nvim_set_hl()`/`nvim_get_hl()`: namespaces scope highlight
    // groups (ns 0 is global) and `nvim_set_hl_ns()` activates a namespace's
    // distinct definitions.
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    crate::ui::nvim_set_hl(&mut editor, 0, OxStr::from("Scope"), dict(&[("fg", Object::Integer(0x111111))]))
        .unwrap();
    crate::ui::nvim_set_hl(&mut editor, 7, OxStr::from("Scope"), dict(&[("fg", Object::Integer(0x777777))]))
        .unwrap();
    let global = crate::ui::nvim_get_hl(&mut editor, 0, dict(&[("name", Object::String(OxStr::from("Scope")))])).unwrap();
    assert_eq!(global.get(&OxStr::from("foreground")), Some(&Object::Integer(0x111111)));
    let scoped = crate::ui::nvim_get_hl(&mut editor, 7, dict(&[("name", Object::String(OxStr::from("Scope")))])).unwrap();
    assert_eq!(scoped.get(&OxStr::from("foreground")), Some(&Object::Integer(0x777777)));
    // A namespace that has not defined the group reports not found.
    assert!(crate::ui::nvim_get_hl(&mut editor, 9, dict(&[("name", Object::String(OxStr::from("Scope")))])).is_err());
    // set_hl_ns switches the active namespace, and edits to it stay distinct.
    crate::ui::nvim_set_hl_ns(&mut editor, 7).unwrap();
    assert_eq!(crate::ui::nvim_get_hl_ns(&mut editor, dict(&[])), Ok(7));
    crate::ui::nvim_set_hl(&mut editor, 7, OxStr::from("Scope"), dict(&[("fg", Object::Integer(0x060606))])).unwrap();
    let updated = crate::ui::nvim_get_hl(&mut editor, 7, dict(&[("name", Object::String(OxStr::from("Scope")))])).unwrap();
    assert_eq!(updated.get(&OxStr::from("foreground")), Some(&Object::Integer(0x060606)));
    assert_eq!(
        crate::ui::nvim_get_hl(&mut editor, 0, dict(&[("name", Object::String(OxStr::from("Scope")))])).unwrap()
            .get(&OxStr::from("foreground")),
        Some(&Object::Integer(0x111111))
    );
}

#[test]
fn set_client_info_accepts_msgpack_rpc_type() {
    // api.txt `nvim_set_client_info()`: "msgpack-rpc" is a valid client type.
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    crate::channel::nvim_set_client_info(
        &mut editor,
        OxStr::from("rpc"),
        dict(&[("major", Object::Integer(1))]),
        OxStr::from("msgpack-rpc"),
        dict(&[]),
        dict(&[]),
    )
    .unwrap();
    let info = crate::channel::nvim_get_chan_info(&mut editor, 1).unwrap();
    let Object::Dict(client) = info.get(&OxStr::from("client")).unwrap() else { panic!("missing client dict") };
    assert_eq!(client.get(&OxStr::from("type")), Some(&Object::String(OxStr::from("msgpack-rpc"))));
    assert_eq!(client.get(&OxStr::from("name")), Some(&Object::String(OxStr::from("rpc"))));
}
