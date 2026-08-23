#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::cell::RefCell;
use std::rc::Rc;

use ox_editor::{AutocmdAction, Editor, Geometry, K_SPECIAL, KS_EXTRA};
use ox_text::Buffer;

use crate::{ApiError, Dict, Object, OxStr, TypeRef};

fn dict(entries: &[(&str, Object)]) -> Dict {
    Dict(entries.iter().map(|(key, value)| (OxStr::from(*key), value.clone())).collect())
}

#[derive(Default)]
struct RecordingExecutor {
    commands: Vec<crate::ExCommand>,
    message: Option<&'static str>,
}

impl crate::CommandExecutor for RecordingExecutor {
    fn execute(&mut self, editor: &mut Editor, commands: &[crate::ExCommand]) -> Result<(), ApiError> {
        self.commands.extend_from_slice(commands);
        if let Some(message) = self.message {
            editor.push_message(ox_editor::Message {
                kind: ox_editor::MessageKind::Echo,
                content: Object::String(OxStr::from(message)),
                history: true,
            });
        }
        Ok(())
    }
}

#[test]
fn nvim_cmd_decodes_structure_and_captures_output() {
    let mut editor = Editor::new();
    let mut executor = RecordingExecutor { commands: Vec::new(), message: Some("captured") };
    let command = dict(&[
        ("cmd", Object::String(OxStr::from("delete"))),
        ("count", Object::Integer(3)),
        ("mods", Object::Dict(dict(&[
            ("silent", Object::Boolean(true)),
            ("keepjumps", Object::Boolean(true)),
            ("vertical", Object::Boolean(true)),
            ("verbose", Object::Integer(2)),
        ]))),
    ]);
    let result = crate::execute_nvim_cmd(
        &mut editor,
        &command,
        &dict(&[("output", Object::Boolean(true))]),
        &mut executor,
    )
    .unwrap();

    assert_eq!(result, OxStr::from("captured"));
    assert!(editor.messages().is_empty());
    let parsed = executor.commands.first().expect("one parsed command");
    assert!(!parsed.bang);
    assert_eq!(parsed.count, None);
    assert_eq!(parsed.args, "");
    assert!(parsed.range.is_none());
    assert_eq!(parsed.modifiers.len(), 4);

    let edit = dict(&[
        ("cmd", Object::String(OxStr::from("edit"))),
        ("bang", Object::Boolean(true)),
        ("args", Object::Array(vec![Object::String(OxStr::from("file.txt"))])),
    ]);
    crate::execute_nvim_cmd(&mut editor, &edit, &Dict(Vec::new()), &mut executor).unwrap();
    let parsed = executor.commands.last().expect("parsed edit command");
    assert!(parsed.bang);
    assert_eq!(parsed.args, "file.txt");

    let ranged = dict(&[
        ("cmd", Object::String(OxStr::from("delete"))),
        ("range", Object::Array(vec![Object::Integer(2), Object::Integer(4)])),
    ]);
    crate::execute_nvim_cmd(&mut editor, &ranged, &Dict(Vec::new()), &mut executor).unwrap();
    assert!(executor.commands.last().expect("parsed ranged command").range.is_some());
}

#[test]
fn nvim_cmd_rejects_invalid_structured_fields() {
    let mut editor = Editor::new();
    let mut executor = RecordingExecutor::default();
    for command in [
        dict(&[]),
        dict(&[("cmd", Object::String(OxStr::from("echo"))), ("range", Object::Array(vec![Object::Integer(-1)]))]),
        dict(&[("cmd", Object::String(OxStr::from("echo"))), ("mods", Object::Dict(dict(&[("split", Object::String(OxStr::from("sideways")))])))]),
        dict(&[("cmd", Object::String(OxStr::from("echo"))), ("mystery", Object::Boolean(true))]),
    ] {
        assert!(crate::execute_nvim_cmd(&mut editor, &command, &Dict(Vec::new()), &mut executor).is_err());
    }
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
    crate::ui::nvim_set_hl(
        &mut editor,
        0,
        OxStr::from("Named"),
        dict(&[
            ("fg", Object::String(OxStr::from("LightGrey"))),
            ("bg", Object::String(OxStr::from("DarkGrey"))),
            ("ctermfg", Object::String(OxStr::from("White"))),
        ]),
    )
    .unwrap();
    let named = crate::ui::nvim_get_hl(&mut editor, 0, dict(&[("name", Object::String(OxStr::from("Named")))])).unwrap();
    assert_eq!(named.get(&OxStr::from("foreground")), Some(&Object::Integer(0xc0c0c0)));
    assert_eq!(named.get(&OxStr::from("background")), Some(&Object::Integer(0x808080)));
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

fn set_option_value(
    editor: &mut Editor,
    name: &str,
    value: Object,
    opts: &[(&str, Object)],
) -> Result<Object, ApiError> {
    crate::global::nvim_set_option_value(
        editor,
        OxStr::from(name),
        value,
        dict(&opts.iter().map(|(key, value)| (*key, value.clone())).collect::<Vec<_>>()),
    )
}

fn get_option_value(editor: &mut Editor, name: &str) -> Result<Object, ApiError> {
    crate::global::nvim_get_option_value(editor, OxStr::from(name), Dict(Vec::new()))
}

fn object_dict(entries: &[(&str, Object)]) -> Object {
    Object::Dict(dict(entries))
}

#[test]
fn set_option_value_returns_assigned_scalars_like_upstream() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    // Upstream nvim_set_option_value returns the assigned value (v0.13-dev
    // structured option returns); verified against the reference binary.
    assert_eq!(
        set_option_value(&mut editor, "number", Object::Boolean(true), &[]),
        Ok(Object::Boolean(true))
    );
    assert_eq!(
        set_option_value(&mut editor, "tabstop", Object::Integer(3), &[]),
        Ok(Object::Integer(3))
    );
    assert_eq!(
        set_option_value(&mut editor, "undolevels", Object::Integer(100), &[]),
        Ok(Object::Integer(100))
    );
    assert_eq!(
        set_option_value(&mut editor, "background", Object::String(OxStr::from("dark")), &[]),
        Ok(Object::String(OxStr::from("dark")))
    );
    assert_eq!(
        set_option_value(&mut editor, "wildcharm", Object::String(OxStr::from("23")), &[]),
        Ok(Object::Integer(23))
    );
}

#[test]
fn set_option_value_returns_structured_list_forms_like_upstream() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    // Flag list ('shortmess', Flags): each character becomes a key.
    assert_eq!(
        set_option_value(&mut editor, "shortmess", Object::String(OxStr::from("ltToOCF")), &[]),
        Ok(object_dict(&[
            ("l", Object::Boolean(true)),
            ("t", Object::Boolean(true)),
            ("T", Object::Boolean(true)),
            ("o", Object::Boolean(true)),
            ("O", Object::Boolean(true)),
            ("C", Object::Boolean(true)),
            ("F", Object::Boolean(true)),
        ]))
    );
    // Comma flag list ('whichwrap', FlagsComma): each item becomes a key.
    assert_eq!(
        set_option_value(&mut editor, "whichwrap", Object::String(OxStr::from("b,s")), &[]),
        Ok(object_dict(&[
            ("b", Object::Boolean(true)),
            ("s", Object::Boolean(true)),
        ]))
    );
    // Comma list ('wildignore', OneComma): items become an Array.
    assert_eq!(
        set_option_value(&mut editor, "wildignore", Object::String(OxStr::from("*.o,*.obj")), &[]),
        Ok(Object::Array(vec![
            Object::String(OxStr::from("*.o")),
            Object::String(OxStr::from("*.obj")),
        ]))
    );
    // 'matchpairs' is a plain OneComma list upstream: `(:)` items stay strings.
    assert_eq!(
        set_option_value(&mut editor, "matchpairs", Object::String(OxStr::from("(:),{:}")), &[]),
        Ok(Object::Array(vec![
            Object::String(OxStr::from("(:)")),
            Object::String(OxStr::from("{:}")),
        ]))
    );
    // Colon map ('listchars', OneCommaColon): items become key/value pairs.
    assert_eq!(
        set_option_value(&mut editor, "listchars", Object::String(OxStr::from("eol:~,tab:>-")), &[]),
        Ok(object_dict(&[
            ("eol", Object::String(OxStr::from("~"))),
            ("tab", Object::String(OxStr::from(">-"))),
        ]))
    );
    assert_eq!(
        set_option_value(&mut editor, "fillchars", Object::String(OxStr::from("vert:|,fold:-")), &[]),
        Ok(object_dict(&[
            ("vert", Object::String(OxStr::from("|"))),
            ("fold", Object::String(OxStr::from("-"))),
        ]))
    );
}

#[test]
fn set_option_value_accepts_structured_inputs_like_upstream() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    // Array input joins into the canonical comma string and returns the
    // structured form; duplicate items drop on NoDup lists ('wildignore').
    assert_eq!(
        set_option_value(
            &mut editor,
            "wildignore",
            Object::Array(vec![
                Object::String(OxStr::from("*.a")),
                Object::String(OxStr::from("*.b")),
            ]),
            &[]
        ),
        Ok(Object::Array(vec![
            Object::String(OxStr::from("*.a")),
            Object::String(OxStr::from("*.b")),
        ]))
    );
    assert_eq!(
        set_option_value(
            &mut editor,
            "wildignore",
            Object::Array(vec![
                Object::String(OxStr::from("*.a")),
                Object::String(OxStr::from("*.a")),
            ]),
            &[]
        ),
        Ok(Object::Array(vec![Object::String(OxStr::from("*.a"))]))
    );
    // Dict input for a flag list keeps truthy keys.
    assert_eq!(
        set_option_value(
            &mut editor,
            "shortmess",
            object_dict(&[("a", Object::Boolean(true)), ("o", Object::Boolean(true))]),
            &[]
        ),
        Ok(object_dict(&[
            ("a", Object::Boolean(true)),
            ("o", Object::Boolean(true)),
        ]))
    );
    // Dict input for a colon map joins key:value pairs, bare flags stay bare,
    // and the joined result is sorted like upstream optval_from_obj().
    assert_eq!(
        set_option_value(
            &mut editor,
            "fillchars",
            object_dict(&[("vert", Object::String(OxStr::from("|"))), ("fold", Object::Boolean(true))]),
            &[]
        ),
        Ok(object_dict(&[
            ("fold", Object::Boolean(true)),
            ("vert", Object::String(OxStr::from("|"))),
        ]))
    );
}

#[test]
fn set_option_value_dry_run_returns_value_without_storing() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    assert_eq!(
        set_option_value(
            &mut editor,
            "shortmess",
            Object::String(OxStr::from("filnxtToOF")),
            &[("dry_run", Object::Boolean(true))]
        ),
        Ok(object_dict(&[
            ("f", Object::Boolean(true)),
            ("i", Object::Boolean(true)),
            ("l", Object::Boolean(true)),
            ("n", Object::Boolean(true)),
            ("x", Object::Boolean(true)),
            ("t", Object::Boolean(true)),
            ("T", Object::Boolean(true)),
            ("o", Object::Boolean(true)),
            ("O", Object::Boolean(true)),
            ("F", Object::Boolean(true)),
        ]))
    );
    // dry_run must not modify the stored value.
    assert_eq!(
        get_option_value(&mut editor, "shortmess"),
        Ok(Object::String(OxStr::from("ltToOCF")))
    );
}

#[test]
fn legacy_option_setters_return_nil_like_upstream() {
    let (mut editor, buffer, _, window) = editor_with_lines(&["one"]);
    // The deprecated setters are void upstream; their RPC responses are nil.
    assert_eq!(
        crate::global::nvim_set_option(&mut editor, OxStr::from("ignorecase"), Object::Boolean(true)),
        Ok(())
    );
    assert_eq!(
        crate::buffer::nvim_buf_set_option(
            &mut editor,
            buffer,
            OxStr::from("expandtab"),
            Object::Boolean(false)
        ),
        Ok(())
    );
    assert_eq!(
        crate::window::nvim_win_set_option(
            &mut editor,
            window,
            OxStr::from("cursorline"),
            Object::Boolean(true)
        ),
        Ok(())
    );
    assert_eq!(
        get_option_value(&mut editor, "ignorecase"),
        Ok(Object::Boolean(true))
    );
}

#[test]
fn option_setter_error_shapes_match_upstream() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one"]);
    // nvim_set_option_value reports validation errors in upstream's
    // "Invalid '<name>': expected a valid type" shape.
    assert_eq!(
        set_option_value(&mut editor, "ignorecase", Object::Integer(3), &[]),
        Err(ApiError::validation(
            "Invalid 'ignorecase': expected a valid type, got Integer"
        ))
    );
    assert_eq!(
        set_option_value(&mut editor, "ignorecase", Object::Array(Vec::new()), &[]),
        Err(ApiError::validation(
            "Invalid 'ignorecase': expected a valid type, got Array"
        ))
    );
    // The deprecated setters first reject non-scalar values, then report the
    // deep "Invalid value for option" exception with the offending literal.
    assert_eq!(
        crate::global::nvim_set_option(&mut editor, OxStr::from("ignorecase"), Object::Array(Vec::new())),
        Err(ApiError::validation("Invalid 'value': expected valid option type, got Array"))
    );
    assert_eq!(
        crate::global::nvim_set_option(&mut editor, OxStr::from("ignorecase"), Object::Integer(3)),
        Err(ApiError::exception(
            "Invalid value for option 'ignorecase': expected boolean, got number 3"
        ))
    );
    assert_eq!(
        crate::buffer::nvim_buf_set_option(
            &mut editor,
            buffer,
            OxStr::from("expandtab"),
            Object::String(OxStr::from("x"))
        ),
        Err(ApiError::exception(
            "Invalid value for option 'expandtab': expected boolean, got string \"x\""
        ))
    );
    // api/options.c validate_option_value_args rejects an unknown operation by
    // name and a merge into a boolean option as a conflict.
    assert_eq!(
        set_option_value(
            &mut editor,
            "wildignore",
            Object::String(OxStr::from("*.x")),
            &[("operation", Object::String(OxStr::from("bogus")))]
        ),
        Err(ApiError::validation(
            "Invalid 'operation': expected 'set', 'append', 'prepend', or 'remove'"
        ))
    );
    assert_eq!(
        set_option_value(
            &mut editor,
            "ignorecase",
            Object::Boolean(true),
            &[("operation", Object::String(OxStr::from("append")))]
        ),
        Err(ApiError::validation("Conflict: 'append' not allowed with boolean options"))
    );
}

// ---------------------------------------------------------------------------
// Runtime-file search over 'runtimepath'
// ---------------------------------------------------------------------------

/// An in-memory directory tree, so the ordering rules can be exercised without
/// a real filesystem. Every entry is an absolute path; a trailing `/` marks a
/// directory, and every parent directory of a listed path exists.
struct MemoryFileIO {
    dirs: std::collections::BTreeSet<String>,
    files: std::collections::BTreeSet<String>,
}

impl MemoryFileIO {
    fn new(entries: &[&str]) -> Self {
        let mut io = Self { dirs: std::collections::BTreeSet::new(), files: std::collections::BTreeSet::new() };
        for entry in entries {
            let path = entry.trim_end_matches('/');
            if entry.ends_with('/') {
                io.dirs.insert(path.to_owned());
            } else {
                io.files.insert(path.to_owned());
            }
            let mut parent = std::path::Path::new(path).parent();
            while let Some(directory) = parent.filter(|directory| directory.as_os_str().len() > 1) {
                io.dirs.insert(directory.to_string_lossy().into_owned());
                parent = directory.parent();
            }
        }
        io
    }

    /// Component-wise wildcard match, the way a shell glob and upstream's
    /// `gen_expand_wildcards()` both treat `*`: it never spans a separator.
    fn matches(pattern: &str, path: &str) -> bool {
        let (pattern, path): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), path.split('/').collect());
        pattern.len() == path.len()
            && pattern.iter().zip(&path).all(|(part, name)| crate::runtime::wildcard(part.as_bytes(), name.as_bytes()))
    }
}

impl crate::FileIO for MemoryFileIO {
    fn expand(&self, pattern: &str, kind: crate::MatchKind) -> Vec<std::path::PathBuf> {
        let candidates: Box<dyn Iterator<Item = &String>> = match kind {
            crate::MatchKind::Dirs => Box::new(self.dirs.iter()),
            crate::MatchKind::Files => Box::new(self.files.iter()),
            crate::MatchKind::DirsAndFiles => Box::new(self.dirs.iter().chain(self.files.iter())),
        };
        let mut found: Vec<String> =
            candidates.filter(|path| Self::matches(pattern, path)).cloned().collect();
        found.sort();
        found.into_iter().map(std::path::PathBuf::from).collect()
    }

    fn is_dir(&self, path: &std::path::Path) -> bool {
        self.dirs.contains(path.to_string_lossy().as_ref())
    }

    fn is_readable(&self, path: &std::path::Path) -> bool {
        self.files.contains(path.to_string_lossy().as_ref())
    }
}

/// Builds an editor whose 'runtimepath'/'packpath' and filesystem are the
/// supplied ones, with nothing inherited from the host.
fn runtime_editor(runtimepath: &str, packpath: &str, entries: &[&str]) -> Editor {
    let mut editor = Editor::new();
    for (name, value) in [("runtimepath", runtimepath), ("packpath", packpath)] {
        editor
            .options_mut()
            .set_global(name, ox_editor::OptionValue::String(value.to_owned()))
            .expect("option is settable");
    }
    crate::set_file_io(&editor, Box::new(MemoryFileIO::new(entries)));
    editor
}

fn list_paths(editor: &mut Editor) -> Vec<String> {
    crate::channel::nvim_list_runtime_paths(editor)
        .expect("listing succeeds")
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn runtime_file(editor: &mut Editor, name: &str, all: bool) -> Vec<String> {
    crate::channel::nvim_get_runtime_file(editor, OxStr::from(name), all)
        .expect("lookup succeeds")
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn get_named(editor: &Editor, patterns: &[&str], all: bool, is_lua: bool) -> Vec<String> {
    let patterns: Vec<String> = patterns.iter().map(|pattern| (*pattern).to_owned()).collect();
    crate::runtime_get_named(editor, &patterns, all, is_lua)
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

const TREE: &[&str] = &[
    "/a/lua/shared.lua",
    "/a/lua/onlya.lua",
    "/a/plugin/x.vim",
    "/b/after/lua/shared.lua",
    "/b/after/plugin/x.vim",
    "/c/lua/shared.lua",
    "/c/plugin/x.vim",
    "/nolua/plugin/shared.lua",
    "/w/p1/lua/shared.lua",
    "/w/p2/lua/shared.lua",
    "/pk/pack/vendor/start/bundle/lua/shared.lua",
    "/pk/pack/vendor/start/bundle/after/lua/shared.lua",
];

// runtime.c do_in_cached_path — `all` decides whether the walk collects every
// match or stops at the first. Three entries all hold the file, so a search
// that ignored `all` would answer with three paths either way, and one that
// ignored 'runtimepath' order would not stop on /a.
#[test]
fn runtime_file_lookup_honors_the_all_flag() {
    let mut editor = runtime_editor("/a,/b/after,/c", "", TREE);
    assert_eq!(
        runtime_file(&mut editor, "lua/shared.lua", true),
        ["/a/lua/shared.lua", "/b/after/lua/shared.lua", "/c/lua/shared.lua"]
    );
    assert_eq!(runtime_file(&mut editor, "lua/shared.lua", false), ["/a/lua/shared.lua"]);
    assert_eq!(get_named(&editor, &["lua/shared.lua"], false, true), ["/a/lua/shared.lua"]);
}

// runtime.c do_in_cached_path — the walk follows 'runtimepath' left to right,
// so reordering the same three entries reorders every answer. A search that
// sorted its results, or read the option once and cached it, would return the
// first block's answer here too.
#[test]
fn runtime_file_lookup_follows_runtimepath_order() {
    let mut editor = runtime_editor("/c,/a", "", TREE);
    assert_eq!(runtime_file(&mut editor, "lua/shared.lua", true), ["/c/lua/shared.lua", "/a/lua/shared.lua"]);
    assert_eq!(runtime_file(&mut editor, "lua/shared.lua", false), ["/c/lua/shared.lua"]);

    editor
        .options_mut()
        .set_global("runtimepath", ox_editor::OptionValue::String("/a,/c".to_owned()))
        .expect("option is settable");
    assert_eq!(runtime_file(&mut editor, "lua/shared.lua", true), ["/a/lua/shared.lua", "/c/lua/shared.lua"]);
    assert_eq!(runtime_file(&mut editor, "lua/shared.lua", false), ["/a/lua/shared.lua"]);
}

// runtime.c runtime_search_path_build — the first pass stops at the first
// `after` entry and the rest of 'runtimepath' is appended from there, so an
// `after` entry in the middle keeps its place. Partitioning the entries into
// non-after then after would move /b/after behind /c and answer with the same
// list as the tail-after case below, which is what makes the pair a test.
#[test]
fn after_entries_keep_their_runtimepath_position() {
    let mut middle = runtime_editor("/a,/b/after,/c", "", TREE);
    assert_eq!(list_paths(&mut middle), ["/a", "/b/after", "/c"]);

    let mut tail = runtime_editor("/a,/c,/b/after", "", TREE);
    assert_eq!(list_paths(&mut tail), ["/a", "/c", "/b/after"]);

    assert_ne!(list_paths(&mut middle), list_paths(&mut tail));
}

// runtime.c runtime_search_path_build — an entry that is also a 'packpath'
// entry splices its start bundles in directly behind itself, while the
// bundles' `after` directories wait for the pass that runs once every
// non-after entry is placed. Appending the after dir next to its bundle, or
// putting the bundles at the end, both reorder this list.
#[test]
fn package_bundles_follow_their_packpath_entry_and_after_dirs_come_last() {
    let mut editor = runtime_editor("/a,/pk,/c", "/pk", TREE);
    assert_eq!(
        list_paths(&mut editor),
        ["/a", "/pk", "/pk/pack/vendor/start/bundle", "/c", "/pk/pack/vendor/start/bundle/after"]
    );
}

// runtime.c expand_rtp_entry — a wildcard entry expands to the directories it
// matches, in sorted order, and a directory already on the path is not placed
// again when a later entry names it. Without the dedup /w/p1 would appear
// twice; without the expansion /w/* would contribute nothing.
#[test]
fn wildcard_entries_expand_and_repeats_collapse() {
    let mut editor = runtime_editor("/w/*,/c,/w/p1", "", TREE);
    assert_eq!(list_paths(&mut editor), ["/w/p1", "/w/p2", "/c"]);
    assert_eq!(
        runtime_file(&mut editor, "lua/shared.lua", true),
        ["/w/p1/lua/shared.lua", "/w/p2/lua/shared.lua", "/c/lua/shared.lua"]
    );
}

// runtime.c runtime_get_named — with `is_lua` an entry that has no `lua/`
// subdirectory is skipped entirely, which is how `require` avoids probing
// every runtime directory. nvim_get_runtime_file has no such filter, so the
// same tree answers differently through the two entry points.
#[test]
fn lua_lookup_skips_entries_without_a_lua_directory() {
    let mut editor = runtime_editor("/nolua,/a", "", TREE);
    assert_eq!(list_paths(&mut editor), ["/nolua", "/a"]);
    assert_eq!(get_named(&editor, &["plugin/shared.lua"], true, true), Vec::<String>::new());
    assert_eq!(get_named(&editor, &["plugin/shared.lua"], true, false), ["/nolua/plugin/shared.lua"]);
    assert_eq!(runtime_file(&mut editor, "plugin/shared.lua", true), ["/nolua/plugin/shared.lua"]);
}

// api/vim.c nvim_get_runtime_file — the name may hold several whitespace
// separated patterns and may glob, and DIP_DIRFILE lets it match directories
// as well as files. All three are tried under one entry before moving on.
#[test]
fn runtime_file_lookup_expands_multiple_patterns_and_directories() {
    let mut editor = runtime_editor("/a,/c", "", TREE);
    assert_eq!(
        runtime_file(&mut editor, "lua/onlya.lua plugin/x.vim", true),
        ["/a/lua/onlya.lua", "/a/plugin/x.vim", "/c/plugin/x.vim"]
    );
    assert_eq!(
        runtime_file(&mut editor, "lua/*.lua", true),
        ["/a/lua/onlya.lua", "/a/lua/shared.lua", "/c/lua/shared.lua"]
    );
    assert_eq!(runtime_file(&mut editor, "lua", true), ["/a/lua", "/c/lua"]);
}

// runtime.c runtime_get_named — patterns are literal readable-file probes, so
// an entry contributes at most one path per pattern and a directory never
// answers. `all` stops the walk on the first hit, as it does for the search.
#[test]
fn lua_lookup_probes_literal_paths_in_order() {
    let editor = runtime_editor("/a,/c", "", TREE);
    assert_eq!(
        get_named(&editor, &["lua/onlya.lua", "lua/shared.lua"], true, true),
        ["/a/lua/onlya.lua", "/a/lua/shared.lua", "/c/lua/shared.lua"]
    );
    assert_eq!(get_named(&editor, &["lua/onlya.lua", "lua/shared.lua"], false, true), ["/a/lua/onlya.lua"]);
    assert_eq!(get_named(&editor, &["lua/*.lua"], true, true), Vec::<String>::new());
    assert_eq!(get_named(&editor, &["lua"], true, true), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// vim.opt append / prepend / remove
// ---------------------------------------------------------------------------

fn merge_option(editor: &mut Editor, name: &str, value: &str, operation: &str) -> Result<Object, ApiError> {
    set_option_value(
        editor,
        name,
        Object::String(OxStr::from(value)),
        &[("operation", Object::String(OxStr::from(operation)))],
    )
}

fn option_text(editor: &mut Editor, name: &str) -> String {
    match get_option_value(editor, name) {
        Ok(Object::String(value)) => value.to_string_lossy().into_owned(),
        other => panic!("expected a string option, got {other:?}"),
    }
}

// option.c get_option_newval — the comma-list merges `vim.opt.rtp:append()`,
// `:prepend()` and `:remove()` compile to, checked against the reference
// binary: appending an entry already present is a no-op, and removing one that
// is absent leaves the value alone.
#[test]
fn comma_list_options_append_prepend_and_remove() {
    let mut editor = runtime_editor("/a,/b", "", &[]);
    assert_eq!(
        merge_option(&mut editor, "runtimepath", "/c", "append"),
        Ok(Object::Array(vec![
            Object::String(OxStr::from("/a")),
            Object::String(OxStr::from("/b")),
            Object::String(OxStr::from("/c")),
        ]))
    );
    assert_eq!(option_text(&mut editor, "runtimepath"), "/a,/b,/c");
    merge_option(&mut editor, "runtimepath", "/z", "prepend").expect("prepend succeeds");
    assert_eq!(option_text(&mut editor, "runtimepath"), "/z,/a,/b,/c");
    merge_option(&mut editor, "runtimepath", "/b", "remove").expect("remove succeeds");
    assert_eq!(option_text(&mut editor, "runtimepath"), "/z,/a,/c");
    merge_option(&mut editor, "runtimepath", "/a", "append").expect("duplicate append succeeds");
    assert_eq!(option_text(&mut editor, "runtimepath"), "/z,/a,/c");
    merge_option(&mut editor, "runtimepath", "/nope", "remove").expect("absent remove succeeds");
    assert_eq!(option_text(&mut editor, "runtimepath"), "/z,/a,/c");
    // Removing the first and the last item each take exactly one comma with them.
    merge_option(&mut editor, "runtimepath", "/z", "remove").expect("remove succeeds");
    assert_eq!(option_text(&mut editor, "runtimepath"), "/a,/c");
    merge_option(&mut editor, "runtimepath", "/c", "remove").expect("remove succeeds");
    assert_eq!(option_text(&mut editor, "runtimepath"), "/a");
}

// option.c stropt_concat_with_comma — an empty original value takes no
// separator, and a flag-list option is not comma separated at all.
#[test]
fn merge_adds_a_separator_only_where_the_option_has_one() {
    let mut editor = runtime_editor("", "", &[]);
    merge_option(&mut editor, "runtimepath", "/only", "append").expect("append succeeds");
    assert_eq!(option_text(&mut editor, "runtimepath"), "/only");

    editor
        .options_mut()
        .set_global("shortmess", ox_editor::OptionValue::String("filnx".to_owned()))
        .expect("option is settable");
    merge_option(&mut editor, "shortmess", "tI", "append").expect("append succeeds");
    assert_eq!(option_text(&mut editor, "shortmess"), "filnxtI");
    merge_option(&mut editor, "shortmess", "l", "remove").expect("remove succeeds");
    assert_eq!(option_text(&mut editor, "shortmess"), "finxtI");
}

// option.c get_option_newval — a number option adds, multiplies and subtracts
// rather than concatenating, so `prepend` on 'scrolloff' is a product.
#[test]
fn number_options_merge_arithmetically() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    let mut apply = |operation: &str, start: i64, value: i64| {
        // 'scrolloff' is global-local, so reset it through the same target the
        // merge reads its old value from.
        set_option_value(&mut editor, "scrolloff", Object::Integer(start), &[]).expect("reset succeeds");
        set_option_value(
            &mut editor,
            "scrolloff",
            Object::Integer(value),
            &[("operation", Object::String(OxStr::from(operation)))],
        )
    };
    assert_eq!(apply("append", 5, 3), Ok(Object::Integer(8)));
    assert_eq!(apply("prepend", 5, 3), Ok(Object::Integer(15)));
    assert_eq!(apply("remove", 5, 3), Ok(Object::Integer(2)));
}

// option.c stropt_handle_keymatch — for a `key:value` comma list, an appended
// item replaces the entry with the same key instead of adding a second one,
// and a removal matches on the key. A plain comma-list merge would leave
// `fold:-` in place beside `fold:.`.
#[test]
fn key_value_options_merge_on_the_key() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    editor
        .options_mut()
        .set_global("fillchars", ox_editor::OptionValue::String("vert:|,fold:-".to_owned()))
        .expect("option is settable");
    merge_option(&mut editor, "fillchars", "fold:.", "append").expect("append succeeds");
    assert_eq!(option_text(&mut editor, "fillchars"), "vert:|,fold:.");
    merge_option(&mut editor, "fillchars", "vert:|", "remove").expect("remove succeeds");
    assert_eq!(option_text(&mut editor, "fillchars"), "fold:.");
}

// option.c option_expand — an option flagged `expand` substitutes `$VAR` and a
// leading `~` before the merge, so `vim.opt.rtp:prepend('~/x')` stores an
// absolute path. An unset variable is left standing.
#[test]
fn expand_flagged_options_substitute_home_and_environment() {
    // Read from the ambient environment rather than mutating it: `set_var` is
    // unsafe, and this crate forbids unsafe code.
    let home = std::env::var("HOME").expect("HOME is set for the test process");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let mut editor = runtime_editor("/a", "", &[]);

    merge_option(&mut editor, "runtimepath", "~/tp", "prepend").expect("prepend succeeds");
    assert_eq!(option_text(&mut editor, "runtimepath"), format!("{home}/tp,/a"));
    merge_option(&mut editor, "runtimepath", "$CARGO_MANIFEST_DIR/x", "append").expect("append succeeds");
    assert_eq!(option_text(&mut editor, "runtimepath"), format!("{home}/tp,/a,{manifest}/x"));
    merge_option(&mut editor, "runtimepath", "${CARGO_MANIFEST_DIR}/y", "append").expect("append succeeds");
    assert_eq!(
        option_text(&mut editor, "runtimepath"),
        format!("{home}/tp,/a,{manifest}/x,{manifest}/y")
    );
    // An unset variable and a `~` that does not open a path component both
    // stay literal, and an option without the expand flag is never touched.
    merge_option(&mut editor, "runtimepath", "$OXVIM_TEST_RTP_UNSET/z", "append").expect("append succeeds");
    assert!(option_text(&mut editor, "runtimepath").ends_with(",$OXVIM_TEST_RTP_UNSET/z"));
    merge_option(&mut editor, "runtimepath", "~tilde", "append").expect("append succeeds");
    assert!(option_text(&mut editor, "runtimepath").ends_with(",~tilde"));
    merge_option(&mut editor, "wildignore", "~/w", "append").expect("append succeeds");
    assert_eq!(option_text(&mut editor, "wildignore"), "~/w");
}

// api/options.c nvim_set_option_value — `dry_run` still merges and returns the
// result, but leaves the option where it was.
#[test]
fn dry_run_merges_without_storing() {
    let mut editor = runtime_editor("/a", "", &[]);
    assert_eq!(
        set_option_value(
            &mut editor,
            "runtimepath",
            Object::String(OxStr::from("/b")),
            &[
                ("operation", Object::String(OxStr::from("append"))),
                ("dry_run", Object::Boolean(true)),
            ]
        ),
        Ok(Object::Array(vec![Object::String(OxStr::from("/a")), Object::String(OxStr::from("/b"))]))
    );
    assert_eq!(option_text(&mut editor, "runtimepath"), "/a");
}

// ---------------------------------------------------------------------------
// Mappings and the Ex-command / Lua hosts
// ---------------------------------------------------------------------------

fn set_keymap(editor: &mut Editor, mode: &str, lhs: &str, rhs: &str, opts: &[(&str, Object)]) -> Result<(), ApiError> {
    crate::keymap::nvim_set_keymap(editor, OxStr::from(mode), OxStr::from(lhs), OxStr::from(rhs), dict(opts))
}

fn keymaps(editor: &mut Editor, mode: &str) -> Vec<Dict> {
    crate::keymap::nvim_get_keymap(editor, OxStr::from(mode))
        .expect("listing succeeds")
        .into_iter()
        .map(|entry| match entry {
            Object::Dict(entry) => entry,
            other => panic!("expected a dictionary, got {other:?}"),
        })
        .collect()
}

fn field(entry: &Dict, key: &str) -> Option<Object> {
    entry.get(&OxStr::from(key)).cloned()
}

// mapping.c mapblock_fill_dict — a mapping set through the API comes back with
// upstream's key set and values, read off the reference binary. `noremap`
// tracks the option rather than the default, `desc` appears only when given,
// and `<Leader>` in the lhs is replaced by the default backslash.
#[test]
fn set_keymap_round_trips_through_get_keymap() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    set_keymap(&mut editor, "n", "<Leader>x", ":echo \"hi\"<CR>", &[
        ("noremap", Object::Boolean(true)),
        ("silent", Object::Boolean(true)),
        ("desc", Object::String(OxStr::from("probe cmd"))),
    ])
    .expect("set succeeds");
    set_keymap(&mut editor, "n", "gp", "gP", &[]).expect("set succeeds");

    let maps = keymaps(&mut editor, "n");
    assert_eq!(maps.len(), 2);
    let leader = maps.iter().find(|entry| field(entry, "lhs") == Some(Object::String(OxStr::from("\\x")))).expect("leader mapping");
    assert_eq!(field(leader, "rhs"), Some(Object::String(OxStr::from(":echo \"hi\"<CR>"))));
    assert_eq!(field(leader, "noremap"), Some(Object::Integer(1)));
    assert_eq!(field(leader, "silent"), Some(Object::Integer(1)));
    assert_eq!(field(leader, "desc"), Some(Object::String(OxStr::from("probe cmd"))));
    assert_eq!(field(leader, "mode"), Some(Object::String(OxStr::from("n"))));
    assert_eq!(field(leader, "mode_bits"), Some(Object::Integer(1)));
    assert_eq!(field(leader, "buffer"), Some(Object::Integer(0)));
    assert_eq!(field(leader, "buf"), Some(Object::Integer(0)));
    assert_eq!(field(leader, "abbr"), Some(Object::Integer(0)));
    assert_eq!(field(leader, "scriptversion"), Some(Object::Integer(1)));

    let plain = maps.iter().find(|entry| field(entry, "lhs") == Some(Object::String(OxStr::from("gp")))).expect("plain mapping");
    assert_eq!(field(plain, "noremap"), Some(Object::Integer(0)));
    assert_eq!(field(plain, "silent"), Some(Object::Integer(0)));
    assert_eq!(field(plain, "desc"), None);

    crate::keymap::nvim_del_keymap(&mut editor, OxStr::from("n"), OxStr::from("gp")).expect("del succeeds");
    assert_eq!(keymaps(&mut editor, "n").len(), 1);
}

// mapping.c modify_keymap/keymap_array — the mode string selects the mode set,
// so a mapping set in one mode is invisible in another and the `:map` modes
// (the empty string) see it while a single unrelated mode does not.
#[test]
fn keymap_modes_select_which_mappings_are_visible() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    set_keymap(&mut editor, "n", "za", "zA", &[]).expect("set succeeds");
    set_keymap(&mut editor, "i", "zb", "zB", &[]).expect("set succeeds");
    set_keymap(&mut editor, "!", "zc", "zC", &[]).expect("set succeeds");

    assert_eq!(keymaps(&mut editor, "n").len(), 1);
    // 'i' sees its own mapping and the `:map!` one, which covers insert.
    assert_eq!(keymaps(&mut editor, "i").len(), 2);
    assert_eq!(keymaps(&mut editor, "c").len(), 1);
    assert_eq!(keymaps(&mut editor, "o").len(), 0);
    // The empty mode is `:map`: normal, visual, select and operator-pending.
    assert_eq!(keymaps(&mut editor, "").len(), 1);
    let bang = keymaps(&mut editor, "c").first().cloned().expect("cmdline mapping");
    assert_eq!(field(&bang, "mode"), Some(Object::String(OxStr::from("!"))));
    assert_eq!(field(&bang, "mode_bits"), Some(Object::Integer(24)));
}

// mapping.c modify_keymap — the rejections, each with upstream's own message.
#[test]
fn keymap_rejections_match_upstream() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    assert_eq!(
        set_keymap(&mut editor, "zz", "a", "b", &[]),
        Err(ApiError::validation("Invalid mode shortname: \"zz\""))
    );
    assert_eq!(
        set_keymap(&mut editor, "nv", "a", "b", &[]),
        Err(ApiError::validation("Invalid mode shortname: \"nv\""))
    );
    assert_eq!(set_keymap(&mut editor, "n", "", "b", &[]), Err(ApiError::validation("Invalid (empty) LHS")));
    assert_eq!(
        set_keymap(&mut editor, "n", "a", "b", &[("bogus", Object::Boolean(true))]),
        Err(ApiError::validation("invalid key: bogus"))
    );
    assert_eq!(
        set_keymap(&mut editor, "n", "a", "b", &[("replace_keycodes", Object::Boolean(true))]),
        Err(ApiError::validation("\"replace_keycodes\" requires \"expr\""))
    );
    assert_eq!(
        crate::keymap::nvim_del_keymap(&mut editor, OxStr::from("n"), OxStr::from("nosuch")),
        Err(ApiError::exception("E31: No such mapping"))
    );
    set_keymap(&mut editor, "n", "zr", "zR", &[]).expect("set succeeds");
    assert_eq!(
        set_keymap(&mut editor, "n", "zr", "zR", &[("unique", Object::Boolean(true))]),
        Err(ApiError::exception("E227: Mapping already exists for zr"))
    );
}

// api/buffer.c nvim_buf_set_keymap/nvim_buf_get_keymap — a buffer-local
// mapping is reported by the buffer listing with its handle, and never by the
// global one, which is the distinction between the two scopes.
#[test]
fn buffer_keymaps_stay_out_of_the_global_listing() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one"]);
    set_keymap(&mut editor, "n", "gg", "gG", &[]).expect("global set succeeds");
    crate::keymap::nvim_buf_set_keymap(
        &mut editor,
        buffer,
        OxStr::from("n"),
        OxStr::from("gb"),
        OxStr::from("gB"),
        dict(&[("desc", Object::String(OxStr::from("buffer local")))]),
    )
    .expect("buffer set succeeds");

    let global = keymaps(&mut editor, "n");
    assert_eq!(global.len(), 1);
    assert_eq!(field(&global[0], "lhs"), Some(Object::String(OxStr::from("gg"))));

    let local = crate::keymap::nvim_buf_get_keymap(&mut editor, buffer, OxStr::from("n"))
        .expect("buffer listing succeeds");
    assert_eq!(local.len(), 1);
    let Object::Dict(entry) = &local[0] else { panic!("expected a dictionary") };
    assert_eq!(field(entry, "lhs"), Some(Object::String(OxStr::from("gb"))));
    assert_eq!(field(entry, "buffer"), Some(Object::Integer(1)));
    assert_eq!(field(entry, "buf"), Some(Object::Integer(i64::from(buffer))));
    assert_eq!(field(entry, "desc"), Some(Object::String(OxStr::from("buffer local"))));

    crate::keymap::nvim_buf_del_keymap(&mut editor, buffer, OxStr::from("n"), OxStr::from("gb"))
        .expect("buffer del succeeds");
    assert!(crate::keymap::nvim_buf_get_keymap(&mut editor, buffer, OxStr::from("n")).expect("listing").is_empty());
    assert_eq!(keymaps(&mut editor, "n").len(), 1);
}

// api/vim.c nvim_exec2 / nvim_cmd / nvim_command run through the installed
// Ex-command host, and `output` decides whether the messages the script
// produced come back. Without a host installed they say so rather than
// claiming the function does not exist.
#[test]
fn exec_functions_run_through_the_installed_command_host() {
    let mut editor = Editor::new();
    assert_eq!(
        crate::global::nvim_command(&mut editor, OxStr::from("write")),
        Err(ApiError::exception("no Ex-command host is installed"))
    );

    crate::set_command_executor(
        &editor,
        Box::new(RecordingExecutor { commands: Vec::new(), message: Some("captured") }),
    );
    assert_eq!(crate::global::nvim_command(&mut editor, OxStr::from("write")), Ok(()));
    assert_eq!(
        crate::global::nvim_exec2(&mut editor, OxStr::from("echo 'x'"), Dict(Vec::new())),
        Ok(Dict(Vec::new()))
    );
    assert_eq!(
        crate::global::nvim_exec2(
            &mut editor,
            OxStr::from("echo 'x'"),
            dict(&[("output", Object::Boolean(true))])
        ),
        Ok(dict(&[("output", Object::String(OxStr::from("captured")))]))
    );
    assert_eq!(
        crate::global::nvim_cmd(
            &mut editor,
            dict(&[("cmd", Object::String(OxStr::from("write")))]),
            dict(&[("output", Object::Boolean(true))])
        ),
        Ok(OxStr::from("captured"))
    );
    // The host is put back after every call, so a second one still finds it.
    assert_eq!(crate::global::nvim_command(&mut editor, OxStr::from("write")), Ok(()));
}

struct EchoingLua;

impl crate::LuaExecutor for EchoingLua {
    fn exec(&mut self, editor: &mut Editor, code: &str, args: Vec<Object>) -> Result<Object, String> {
        // Prove the host receives the editor as well as the chunk.
        editor.push_message(ox_editor::Message {
            kind: ox_editor::MessageKind::Echo,
            content: Object::String(OxStr::from(code)),
            history: false,
        });
        Ok(Object::Array(args))
    }
}

// api/vim.c nvim_exec_lua hands the chunk and its arguments to the Lua host
// and returns what the host produced.
#[test]
fn exec_lua_runs_through_the_installed_lua_host() {
    let mut editor = Editor::new();
    assert_eq!(
        crate::global::nvim_exec_lua(&mut editor, OxStr::from("return 1"), Vec::new()),
        Err(ApiError::exception("no Lua host is installed"))
    );
    crate::set_lua_executor(&editor, Box::new(EchoingLua));
    assert_eq!(
        crate::global::nvim_exec_lua(&mut editor, OxStr::from("return ..."), vec![Object::Integer(7)]),
        Ok(Object::Array(vec![Object::Integer(7)]))
    );
    assert_eq!(
        editor.messages().last().map(|message| message.content.clone()),
        Some(Object::String(OxStr::from("return ...")))
    );
}
