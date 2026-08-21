#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use ox_editor::{Editor, Geometry};
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
    assert_eq!(registry.len(), 77);
    let expected = [
        ("nvim_buf_get_lines", 1, TypeRef::ArrayOf(&TypeRef::String)),
        ("nvim_buf_get_text", 9, TypeRef::ArrayOf(&TypeRef::String)),
        ("nvim_buf_get_changedtick", 2, TypeRef::Integer),
        ("nvim_win_get_cursor", 1, TypeRef::ArrayOf(&TypeRef::Integer)),
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
