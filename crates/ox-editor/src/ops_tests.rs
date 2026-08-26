#![allow(clippy::unwrap_used)]

use ox_text::{Buffer, Position};

use crate::extmark::{ExtmarkAttributes, ExtmarkGravity, ExtmarkPlacement, ExtmarkPosition};
use crate::ops::{self, EditRange, Operator};
use crate::{Editor, Geometry, MotionKind, NullExprEval, RegisterKind};

fn position(lnum: usize, col: usize) -> Position {
    Position { lnum, col }
}

fn setup(lines: &[&[u8]]) -> (Editor, ox_types::BufHandle, ox_types::WinHandle) {
    let mut editor = Editor::new();
    let owned: Vec<Vec<u8>> = lines.iter().map(|line| line.to_vec()).collect();
    let buffer = editor
        .create_buffer_with(Buffer::from_lines(&owned, false).unwrap(), true)
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    (editor, buffer, window)
}

fn mark_at(
    editor: &mut Editor,
    buffer: ox_types::BufHandle,
    row: usize,
    column: usize,
) -> (crate::extmark::NamespaceId, crate::extmark::ExtmarkId) {
    let state = editor.buffer_mut(buffer).unwrap();
    let namespace = state.extmarks.create_namespace("delete-geometry").unwrap();
    let id = state
        .extmarks
        .set(
            namespace,
            None,
            ExtmarkPlacement::new(ExtmarkPosition::new(row, column)),
        )
        .unwrap();
    (namespace, id)
}

fn place(
    editor: &mut Editor,
    buffer: ox_types::BufHandle,
    namespace: crate::extmark::NamespaceId,
    placement: ExtmarkPlacement,
) -> crate::extmark::ExtmarkId {
    editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .set(namespace, None, placement)
        .unwrap()
}

fn ext_pos(
    editor: &Editor,
    buffer: ox_types::BufHandle,
    namespace: crate::extmark::NamespaceId,
    id: crate::extmark::ExtmarkId,
) -> ExtmarkPosition {
    editor
        .buffer(buffer)
        .unwrap()
        .extmarks
        .get(namespace, id)
        .unwrap()
        .unwrap()
        .position()
}

fn text_bytes(editor: &Editor, buffer: ox_types::BufHandle) -> Vec<u8> {
    editor.buffer(buffer).unwrap().text().unwrap().to_bytes()
}

fn apply_delete(
    editor: &mut Editor,
    buffer: ox_types::BufHandle,
    window: ox_types::WinHandle,
    range: EditRange,
    register: Option<char>,
) {
    let mut eval = NullExprEval;
    ops::apply(
        editor,
        buffer,
        window,
        Operator::Delete,
        range,
        register,
        10,
        &mut eval,
    )
    .unwrap();
}

#[test]
fn characterwise_same_row_delete_moves_marks_and_undo_redo() {
    let (mut editor, buffer, window) = setup(&[b"12345"]);
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let (namespace, after) = mark_at(&mut editor, buffer, 0, 3);
    let before = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(0, 0)),
    );
    let inside = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(0, 1)),
    );
    let tick_before = editor.buffer(buffer).unwrap().changedtick();
    let changelist_before = editor.changelists().len(buffer);

    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 1),
            end: position(1, 2),
            kind: MotionKind::CharacterWise,
            inclusive: true,
        },
        Some('a'),
    );

    assert_eq!(text_bytes(&editor, buffer), b"145");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, before),
        ExtmarkPosition::new(0, 0)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, inside),
        ExtmarkPosition::new(0, 1)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(0, 1)
    );
    assert_eq!(editor.window(window).unwrap().cursor, position(1, 1));
    assert_eq!(editor.changelists().len(buffer), changelist_before + 1);
    assert_eq!(
        editor.buffer(buffer).unwrap().changedtick(),
        tick_before + 1
    );
    assert!(editor.buffer(buffer).unwrap().modified);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_block_len(), 1);
    let unnamed = editor.registers().get('"').unwrap().unwrap();
    assert_eq!(unnamed.kind(), RegisterKind::CharacterWise);
    assert_eq!(unnamed.to_bytes(), b"23");
    assert_eq!(
        editor.registers().get('a').unwrap().unwrap().to_bytes(),
        b"23"
    );

    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    assert_eq!(text_bytes(&editor, buffer), b"12345");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(0, 3)
    );
    assert_eq!(editor.buffer_redo(buffer).unwrap(), Some(1));
    assert_eq!(text_bytes(&editor, buffer), b"145");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(0, 1)
    );
}

#[test]
fn characterwise_outside_mark_ladder_matches_visual_contracts() {
    let (mut editor, buffer, window) = setup(&[b"12345"]);
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let (namespace, mark) = mark_at(&mut editor, buffer, 0, 3);

    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 0),
            end: position(1, 0),
            kind: MotionKind::CharacterWise,
            inclusive: true,
        },
        None,
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, mark),
        ExtmarkPosition::new(0, 2)
    );

    assert!(editor.buffer_undo(buffer).unwrap().is_some());
    editor.sync_buffer_undo(buffer);
    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 0),
            end: position(1, 1),
            kind: MotionKind::CharacterWise,
            inclusive: true,
        },
        None,
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, mark),
        ExtmarkPosition::new(0, 1)
    );

    assert!(editor.buffer_undo(buffer).unwrap().is_some());
    editor.sync_buffer_undo(buffer);
    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 0),
            end: position(1, 2),
            kind: MotionKind::CharacterWise,
            inclusive: true,
        },
        None,
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, mark),
        ExtmarkPosition::new(0, 0)
    );

    assert!(editor.buffer_undo(buffer).unwrap().is_some());
    editor.sync_buffer_undo(buffer);
    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 4),
            end: position(1, 4),
            kind: MotionKind::CharacterWise,
            inclusive: true,
        },
        None,
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, mark),
        ExtmarkPosition::new(0, 3)
    );
}

#[test]
fn characterwise_multiline_delete_joins_rows_and_translates_marks() {
    let (mut editor, buffer, window) = setup(&[b"abcde", b"12345", b"vwxyz"]);
    editor.set_window_cursor(window, position(1, 2)).unwrap();
    let state = editor.buffer_mut(buffer).unwrap();
    let namespace = state.extmarks.create_namespace("multiline").unwrap();
    let before = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(0, 1)),
    );
    let inside = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(1, 2)),
    );
    let after = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(2, 4)),
    );

    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 2),
            end: position(3, 1),
            kind: MotionKind::CharacterWise,
            inclusive: true,
        },
        None,
    );

    assert_eq!(text_bytes(&editor, buffer), b"abxyz");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, before),
        ExtmarkPosition::new(0, 1)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, inside),
        ExtmarkPosition::new(0, 2)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(0, 4)
    );
    assert_eq!(editor.changelists().len(buffer), 1);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_block_len(), 1);

    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    assert_eq!(text_bytes(&editor, buffer), b"abcde\n12345\nvwxyz");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(2, 4)
    );
    assert_eq!(editor.buffer_redo(buffer).unwrap(), Some(1));
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(0, 4)
    );
}

#[test]
fn characterwise_exclusive_endpoint_and_whitespace_promotion() {
    let (mut editor, buffer, window) = setup(&[b"abc", b"def"]);
    editor.set_window_cursor(window, position(1, 1)).unwrap();
    let (namespace, after) = mark_at(&mut editor, buffer, 1, 1);

    // Exclusive end at column 0 of the next line backs off to the start line.
    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 1),
            end: position(2, 0),
            kind: MotionKind::CharacterWise,
            inclusive: false,
        },
        None,
    );
    assert_eq!(text_bytes(&editor, buffer), b"a\ndef");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(1, 1)
    );
    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    editor.sync_buffer_undo(buffer);

    // Delete-only whitespace promotion becomes linewise.
    let (mut editor, buffer, window) = setup(&[b"  ab", b"cd  ", b"keep"]);
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let (namespace, after) = mark_at(&mut editor, buffer, 2, 0);
    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 0),
            end: position(2, 3),
            kind: MotionKind::CharacterWise,
            inclusive: true,
        },
        None,
    );
    assert_eq!(text_bytes(&editor, buffer), b"keep");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(0, 0)
    );
    assert_eq!(
        editor.registers().get('"').unwrap().unwrap().kind(),
        RegisterKind::LineWise
    );
    assert_eq!(editor.changelists().len(buffer), 1);
}

#[test]
fn blockwise_delete_is_one_transaction_with_per_row_geometry() {
    let (mut editor, buffer, window) = setup(&[b"12345", b"abc", b"VWXYZ"]);
    editor.set_window_cursor(window, position(1, 1)).unwrap();
    let state = editor.buffer_mut(buffer).unwrap();
    let namespace = state.extmarks.create_namespace("block").unwrap();
    let before = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(0, 0)),
    );
    let inside = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(0, 2)),
    );
    let after = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(0, 4)),
    );
    let short_row = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(1, 1)),
    );
    let lower = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(2, 3)),
    );

    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 1),
            end: position(3, 3),
            kind: MotionKind::BlockWise,
            inclusive: true,
        },
        Some('b'),
    );

    assert_eq!(text_bytes(&editor, buffer), b"15\na\nVZ");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, before),
        ExtmarkPosition::new(0, 0)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, inside),
        ExtmarkPosition::new(0, 1)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(0, 1)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, short_row),
        ExtmarkPosition::new(1, 1)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, lower),
        ExtmarkPosition::new(2, 1)
    );
    assert_eq!(editor.changelists().len(buffer), 1);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_seq(), 1);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_block_len(), 3);
    let content = editor.registers().get('b').unwrap().unwrap();
    assert!(matches!(
        content.kind(),
        RegisterKind::BlockWise { width: 3 }
    ));
    assert_eq!(
        content.lines(),
        &[b"234".as_slice(), b"bc".as_slice(), b"WXY".as_slice()]
    );

    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    assert_eq!(text_bytes(&editor, buffer), b"12345\nabc\nVWXYZ");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(0, 4)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, lower),
        ExtmarkPosition::new(2, 3)
    );
    assert_eq!(editor.buffer_redo(buffer).unwrap(), Some(1));
    assert_eq!(text_bytes(&editor, buffer), b"15\na\nVZ");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, lower),
        ExtmarkPosition::new(2, 1)
    );
}

#[test]
fn blockwise_delete_beyond_mark_leaves_it_unchanged() {
    let (mut editor, buffer, window) = setup(&[b"12345", b"abcde"]);
    let (namespace, mark) = mark_at(&mut editor, buffer, 0, 1);
    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 3),
            end: position(2, 4),
            kind: MotionKind::BlockWise,
            inclusive: true,
        },
        None,
    );
    assert_eq!(text_bytes(&editor, buffer), b"123\nabc");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, mark),
        ExtmarkPosition::new(0, 1)
    );
}

#[test]
fn linewise_delete_replaces_only_deleted_rows() {
    let (mut editor, buffer, window) = setup(&[b"one", b"two", b"three", b"four"]);
    editor.set_window_cursor(window, position(2, 0)).unwrap();
    let state = editor.buffer_mut(buffer).unwrap();
    let namespace = state.extmarks.create_namespace("linewise").unwrap();
    let before = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(0, 1)),
    );
    let mut inside =
        ExtmarkPlacement::new(ExtmarkPosition::new(1, 0)).with_end(ExtmarkPosition::new(2, 0));
    inside.attributes = ExtmarkAttributes {
        invalidate: true,
        ..ExtmarkAttributes::default()
    };
    let inside = place(&mut editor, buffer, namespace, inside);
    let after = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(3, 2)),
    );

    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(2, 0),
            end: position(3, 0),
            kind: MotionKind::LineWise,
            inclusive: true,
        },
        None,
    );

    assert_eq!(text_bytes(&editor, buffer), b"one\nfour");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, before),
        ExtmarkPosition::new(0, 1)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(1, 2)
    );
    let inside_mark = editor
        .buffer(buffer)
        .unwrap()
        .extmarks
        .get(namespace, inside)
        .unwrap()
        .unwrap();
    assert!(inside_mark.invalid);
    assert_eq!(
        editor.registers().get('"').unwrap().unwrap().kind(),
        RegisterKind::LineWise
    );
    assert_eq!(editor.changelists().len(buffer), 1);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_block_len(), 1);

    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    assert_eq!(text_bytes(&editor, buffer), b"one\ntwo\nthree\nfour");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(3, 2)
    );
    assert_eq!(editor.buffer_redo(buffer).unwrap(), Some(1));
    assert_eq!(
        ext_pos(&editor, buffer, namespace, after),
        ExtmarkPosition::new(1, 2)
    );
}

#[test]
fn linewise_whole_buffer_delete_leaves_one_empty_line() {
    let (mut editor, buffer, window) = setup(&[b"only"]);
    let (namespace, mark) = mark_at(&mut editor, buffer, 0, 2);
    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 0),
            end: position(1, 0),
            kind: MotionKind::LineWise,
            inclusive: true,
        },
        None,
    );
    assert_eq!(text_bytes(&editor, buffer), b"");
    assert_eq!(
        ext_pos(&editor, buffer, namespace, mark),
        ExtmarkPosition::new(0, 0)
    );
}

#[test]
fn delete_text_edit_error_is_atomic() {
    // "한" is three UTF-8 bytes. Targeting the middle byte must fail before any
    // register/text/undo side effects.
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(
            Buffer::from_lines(&[b"a\xed\x95\x9cc".to_vec(), b"xyz".to_vec()], false).unwrap(),
            true,
        )
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    let (namespace, mark) = mark_at(&mut editor, buffer, 0, 0);
    let before_text = text_bytes(&editor, buffer);
    let before_tick = editor.buffer(buffer).unwrap().changedtick();
    let before_modified = editor.buffer(buffer).unwrap().modified;
    let before_changelist = editor.changelists().len(buffer);
    let before_seq = editor.buffer(buffer).unwrap().undo.current_seq();
    let before_register = editor.registers().get('"').unwrap().cloned();

    let mut eval = NullExprEval;
    let err = ops::apply(
        &mut editor,
        buffer,
        window,
        Operator::Delete,
        EditRange {
            start: position(1, 2),
            end: position(2, 2),
            kind: MotionKind::BlockWise,
            inclusive: true,
        },
        Some('z'),
        10,
        &mut eval,
    )
    .unwrap_err();
    assert!(matches!(err, ops::OperatorError::Buffer(_)), "{err:?}");
    assert_eq!(text_bytes(&editor, buffer), before_text);
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), before_tick);
    assert_eq!(editor.buffer(buffer).unwrap().modified, before_modified);
    assert_eq!(
        editor.buffer(buffer).unwrap().undo.current_seq(),
        before_seq
    );
    assert_eq!(editor.changelists().len(buffer), before_changelist);
    assert_eq!(
        ext_pos(&editor, buffer, namespace, mark),
        ExtmarkPosition::new(0, 0)
    );
    assert_eq!(
        editor.registers().get('"').unwrap().cloned(),
        before_register
    );
    assert!(editor.registers().get('z').unwrap().is_none());
}

#[test]
fn left_gravity_mark_collapses_to_delete_start() {
    let (mut editor, buffer, window) = setup(&[b"abcdef"]);
    let state = editor.buffer_mut(buffer).unwrap();
    let namespace = state.extmarks.create_namespace("gravity").unwrap();
    let mut left = ExtmarkPlacement::new(ExtmarkPosition::new(0, 3));
    left.gravity = ExtmarkGravity::Left;
    let left = place(&mut editor, buffer, namespace, left);
    let right = place(
        &mut editor,
        buffer,
        namespace,
        ExtmarkPlacement::new(ExtmarkPosition::new(0, 3)),
    );

    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 2),
            end: position(1, 4),
            kind: MotionKind::CharacterWise,
            inclusive: true,
        },
        None,
    );

    assert_eq!(
        ext_pos(&editor, buffer, namespace, left),
        ExtmarkPosition::new(0, 2)
    );
    assert_eq!(
        ext_pos(&editor, buffer, namespace, right),
        ExtmarkPosition::new(0, 2)
    );
}

#[test]
fn blockwise_delete_batch_one_tick() {
    let (mut editor, buffer, window) = setup(&[b"12345", b"abc", b"VWXYZ"]);
    editor.set_window_cursor(window, position(1, 1)).unwrap();
    let tick = editor.buffer(buffer).unwrap().changedtick();
    let seq = editor.buffer(buffer).unwrap().undo.current_seq();
    apply_delete(
        &mut editor,
        buffer,
        window,
        EditRange {
            start: position(1, 1),
            end: position(3, 3),
            kind: MotionKind::BlockWise,
            inclusive: true,
        },
        Some('b'),
    );
    assert_eq!(text_bytes(&editor, buffer), b"15\na\nVZ");
    assert_eq!(editor.buffer(buffer).unwrap().changedtick(), tick + 1);
    assert_eq!(editor.buffer(buffer).unwrap().undo.current_seq(), seq + 1);
    assert_eq!(editor.buffer_undo(buffer).unwrap(), Some(1));
    assert_eq!(text_bytes(&editor, buffer), b"12345\nabc\nVWXYZ");
}
