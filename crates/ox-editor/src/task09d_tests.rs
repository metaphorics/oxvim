#![allow(clippy::unwrap_used)]

use ox_text::{Buffer, Position as TextPosition};

use crate::decoration::{
    BufCallbackId, CallbackPhase, DecorItem, DecorOrigin, DecorPos, DecorProviderDef, DecorRange,
    Decorations, LineCallbackId, ProviderId, RangeCallbackId, StartCallbackId,
    VirtTextChunk as DecorTextChunk, VirtualText as DecorVirtualText, WinCallbackId, WindowId,
};
use crate::extmark::{
    ExtmarkAttributes, ExtmarkEnd, ExtmarkGravity, ExtmarkId, ExtmarkPlacement,
    ExtmarkPosition, Extmarks, TextExtent, VirtualTextChunk,
};
use crate::fold::{
    indent_levels, FoldComputeResult, FoldError, FoldMethod, FoldRefresh, FoldState, Folds,
    HostFoldKind, Position,
};
use crate::BufferState;

fn placement(row: usize, column: usize, gravity: ExtmarkGravity) -> ExtmarkPlacement {
    let mut placement = ExtmarkPlacement::new(ExtmarkPosition::new(row, column));
    placement.gravity = gravity;
    placement
}

fn one_mark(
    marks: &mut Extmarks,
    row: usize,
    column: usize,
    gravity: ExtmarkGravity,
) -> (crate::extmark::NamespaceId, ExtmarkId) {
    let namespace = marks.create_namespace("test").unwrap();
    let id = marks
        .set(namespace, None, placement(row, column, gravity))
        .unwrap();
    (namespace, id)
}

fn mark_position(
    marks: &Extmarks,
    namespace: crate::extmark::NamespaceId,
    id: ExtmarkId,
) -> ExtmarkPosition {
    marks.get(namespace, id).unwrap().unwrap().position()
}

// Gravity and splice behavior: src/nvim/marktree.c:1921-2073 and
// src/nvim/api/extmark.c:414-415,440-441.
macro_rules! insertion_gravity_test {
    ($name:ident, $row:expr, $column:expr, $gravity:expr, $start_row:expr, $start_column:expr, $extent:expr, $expected_row:expr, $expected_column:expr) => {
        #[test]
        fn $name() {
            let mut marks = Extmarks::new();
            let (namespace, id) = one_mark(&mut marks, $row, $column, $gravity);
            marks.splice(crate::extmark::TextSplice {
                start: ExtmarkPosition::new($start_row, $start_column),
                old_extent: TextExtent::EMPTY,
                new_extent: $extent,
            });
            assert_eq!(
                mark_position(&marks, namespace, id),
                ExtmarkPosition::new($expected_row, $expected_column)
            );
        }
    };
}

insertion_gravity_test!(insert_same_point_right_moves, 2, 4, ExtmarkGravity::Right, 2, 4, TextExtent::new(0, 3), 2, 7);
insertion_gravity_test!(insert_same_point_left_stays, 2, 4, ExtmarkGravity::Left, 2, 4, TextExtent::new(0, 3), 2, 4);
insertion_gravity_test!(insert_before_mark_shifts_column, 2, 7, ExtmarkGravity::Right, 2, 4, TextExtent::new(0, 3), 2, 10);
insertion_gravity_test!(insert_after_mark_leaves_mark, 2, 2, ExtmarkGravity::Right, 2, 4, TextExtent::new(0, 3), 2, 2);
insertion_gravity_test!(insert_multiline_right_moves_to_new_end, 2, 4, ExtmarkGravity::Right, 2, 4, TextExtent::new(2, 1), 4, 1);
insertion_gravity_test!(insert_multiline_left_stays_at_start, 2, 4, ExtmarkGravity::Left, 2, 4, TextExtent::new(2, 1), 2, 4);
insertion_gravity_test!(insert_multiline_suffix_moves_rows, 5, 9, ExtmarkGravity::Right, 2, 4, TextExtent::new(2, 1), 7, 9);
insertion_gravity_test!(insert_other_row_is_unchanged, 1, 9, ExtmarkGravity::Right, 2, 4, TextExtent::new(2, 1), 1, 9);

macro_rules! replacement_gravity_test {
    ($name:ident, $column:expr, $gravity:expr, $expected:expr) => {
        #[test]
        fn $name() {
            let mut marks = Extmarks::new();
            let (namespace, id) = one_mark(&mut marks, 1, $column, $gravity);
            marks.splice(crate::extmark::TextSplice {

                start: ExtmarkPosition::new(1, 2),

                old_extent: TextExtent::new(0, 4),

                new_extent: TextExtent::new(0, 2),

            });
            assert_eq!(mark_position(&marks, namespace, id), ExtmarkPosition::new(1, $expected));
        }
    };
}

replacement_gravity_test!(replace_interior_left_collapses_to_start, 4, ExtmarkGravity::Left, 2);
replacement_gravity_test!(replace_interior_right_collapses_to_new_end, 4, ExtmarkGravity::Right, 4);
replacement_gravity_test!(replace_old_end_left_collapses_to_start, 6, ExtmarkGravity::Left, 2);
replacement_gravity_test!(replace_old_end_right_moves_to_new_end, 6, ExtmarkGravity::Right, 4);
replacement_gravity_test!(replace_before_range_is_unchanged, 1, ExtmarkGravity::Right, 1);
replacement_gravity_test!(replace_after_range_shifts_by_delta, 8, ExtmarkGravity::Right, 6);

#[test]
fn range_start_and_end_use_independent_gravity() {
    let mut marks = Extmarks::new();
    let namespace = marks.create_namespace("range").unwrap();
    let mut mark = placement(1, 2, ExtmarkGravity::Left);
    mark.end = Some(ExtmarkEnd {
        position: ExtmarkPosition::new(1, 5),
        gravity: ExtmarkGravity::Right,
    });
    let id = marks.set(namespace, None, mark).unwrap();
    marks.splice(crate::extmark::TextSplice {

        start: ExtmarkPosition::new(1, 2),

        old_extent: TextExtent::EMPTY,

        new_extent: TextExtent::new(0, 2),

    });
    let mark = marks.get(namespace, id).unwrap().unwrap();
    assert_eq!(mark.position(), ExtmarkPosition::new(1, 2));
    assert_eq!(mark.placement.end.unwrap().position, ExtmarkPosition::new(1, 7));
}

#[test]
fn default_start_gravity_is_right() {
    assert_eq!(
        ExtmarkPlacement::new(ExtmarkPosition::new(0, 0)).gravity,
        ExtmarkGravity::Right
    );
}

#[test]
fn default_end_gravity_is_left() {
    assert_eq!(
        ExtmarkEnd::new(ExtmarkPosition::new(0, 1)).gravity,
        ExtmarkGravity::Left
    );
}

// Invalidation: src/nvim/extmark.c:414-480 and api/extmark.c:204-206,430-434.
#[test]
fn complete_range_deletion_invalidates_when_requested() {
    let mut marks = Extmarks::new();
    let namespace = marks.create_namespace("invalid").unwrap();
    let mut mark = ExtmarkPlacement::new(ExtmarkPosition::new(1, 2))
        .with_end(ExtmarkPosition::new(1, 5));
    mark.attributes.invalidate = true;
    let id = marks.set(namespace, None, mark).unwrap();
    let result = marks.splice(crate::extmark::TextSplice {
     start: ExtmarkPosition::new(1, 1),
     old_extent: TextExtent::new(0, 6),
     new_extent: TextExtent::EMPTY,
 });
    assert_eq!(result.invalidated, 1);
    assert!(marks.get(namespace, id).unwrap().unwrap().invalid);
}

#[test]
fn complete_range_deletion_without_invalidate_keeps_visible() {
    let mut marks = Extmarks::new();
    let namespace = marks.create_namespace("valid").unwrap();
    let id = marks
        .set(
            namespace,
            None,
            ExtmarkPlacement::new(ExtmarkPosition::new(1, 2))
                .with_end(ExtmarkPosition::new(1, 5)),
        )
        .unwrap();
    marks.splice(crate::extmark::TextSplice {

        start: ExtmarkPosition::new(1, 1),

        old_extent: TextExtent::new(0, 6),

        new_extent: TextExtent::EMPTY,

    });
    assert!(!marks.get(namespace, id).unwrap().unwrap().invalid);
}

#[test]
fn partial_range_deletion_does_not_invalidate() {
    let mut marks = Extmarks::new();
    let namespace = marks.create_namespace("partial").unwrap();
    let mut mark = ExtmarkPlacement::new(ExtmarkPosition::new(1, 1))
        .with_end(ExtmarkPosition::new(1, 8));
    mark.attributes.invalidate = true;
    let id = marks.set(namespace, None, mark).unwrap();
    marks.splice(crate::extmark::TextSplice {

        start: ExtmarkPosition::new(1, 3),

        old_extent: TextExtent::new(0, 2),

        new_extent: TextExtent::EMPTY,

    });
    assert!(!marks.get(namespace, id).unwrap().unwrap().invalid);
}

// Namespace/id/query behavior: src/nvim/api/extmark.c:47-70,235-254,276-292,329-372.
#[test]
fn named_namespace_creation_is_idempotent() {
    let mut marks = Extmarks::new();
    assert_eq!(
        marks.create_namespace("same").unwrap(),
        marks.create_namespace("same").unwrap()
    );
}

#[test]
fn anonymous_namespace_creation_is_fresh() {
    let mut marks = Extmarks::new();
    assert_ne!(
        marks.create_namespace("").unwrap(),
        marks.create_namespace("").unwrap()
    );
}

#[test]
fn namespace_isolation_allows_equal_local_ids() {
    let mut marks = Extmarks::new();
    let first = marks.create_namespace("first").unwrap();
    let second = marks.create_namespace("second").unwrap();
    let first_id = marks
        .set(first, None, ExtmarkPlacement::new(ExtmarkPosition::new(0, 0)))
        .unwrap();
    let second_id = marks
        .set(second, None, ExtmarkPlacement::new(ExtmarkPosition::new(1, 0)))
        .unwrap();
    assert_eq!(first_id, second_id);
    assert_ne!(marks.get(first, first_id).unwrap(), marks.get(second, second_id).unwrap());
}

#[test]
fn requested_id_updates_allocator() {
    let mut marks = Extmarks::new();
    let namespace = marks.create_namespace("ids").unwrap();
    let requested = ExtmarkId::new(8).unwrap();
    marks
        .set(
            namespace,
            Some(requested),
            ExtmarkPlacement::new(ExtmarkPosition::new(0, 0)),
        )
        .unwrap();
    assert_eq!(
        marks
            .set(
                namespace,
                None,
                ExtmarkPlacement::new(ExtmarkPosition::new(0, 1)),
            )
            .unwrap()
            .get(),
        9
    );
}

#[test]
fn updating_mark_preserves_id_and_changes_position() {
    let mut marks = Extmarks::new();
    let (namespace, id) = one_mark(&mut marks, 0, 0, ExtmarkGravity::Right);
    marks
        .update(
            namespace,
            id,
            ExtmarkPlacement::new(ExtmarkPosition::new(3, 4)),
        )
        .unwrap();
    assert_eq!(mark_position(&marks, namespace, id), ExtmarkPosition::new(3, 4));
}

#[test]
fn delete_reports_presence_then_absence() {
    let mut marks = Extmarks::new();
    let (namespace, id) = one_mark(&mut marks, 0, 0, ExtmarkGravity::Right);
    assert!(marks.delete(namespace, id).unwrap());
    assert!(!marks.delete(namespace, id).unwrap());
}

#[test]
fn clear_uses_inclusive_bounds() {
    let mut marks = Extmarks::new();
    let namespace = marks.create_namespace("clear").unwrap();
    for column in 0..3 {
        marks
            .set(
                namespace,
                None,
                ExtmarkPlacement::new(ExtmarkPosition::new(0, column)),
            )
            .unwrap();
    }
    assert_eq!(
        marks
            .clear(
                namespace,
                ExtmarkPosition::new(0, 1),
                ExtmarkPosition::new(0, 2),
            )
            .unwrap(),
        2
    );
}

fn queried_columns(reverse: bool, limit: Option<usize>) -> Vec<usize> {
    let mut marks = Extmarks::new();
    let namespace = marks.create_namespace("query").unwrap();
    for column in [3, 1, 2] {
        marks
            .set(
                namespace,
                None,
                ExtmarkPlacement::new(ExtmarkPosition::new(0, column)),
            )
            .unwrap();
    }
    let (first, last) = if reverse {
        (ExtmarkPosition::new(0, 3), ExtmarkPosition::new(0, 1))
    } else {
        (ExtmarkPosition::new(0, 1), ExtmarkPosition::new(0, 3))
    };
    marks
        .query(namespace, first, last, limit)
        .unwrap()
        .into_iter()
        .map(|mark| mark.position().column)
        .collect()
}

#[test]
fn query_orders_forward_by_position() {
    assert_eq!(queried_columns(false, None), vec![1, 2, 3]);
}

#[test]
fn query_reverses_exact_traversal_order() {
    assert_eq!(queried_columns(true, None), vec![3, 2, 1]);
}

#[test]
fn query_forward_limit_applies_after_ordering() {
    assert_eq!(queried_columns(false, Some(2)), vec![1, 2]);
}

#[test]
fn query_reverse_limit_applies_after_reversal() {
    assert_eq!(queried_columns(true, Some(2)), vec![3, 2]);
}

#[test]
fn query_zero_limit_is_empty() {
    assert!(queried_columns(false, Some(0)).is_empty());
}

#[test]
fn query_all_merges_namespaces_in_position_order() {
    let mut marks = Extmarks::new();
    for (name, column) in [("a", 2), ("b", 1)] {
        let namespace = marks.create_namespace(name).unwrap();
        marks
            .set(
                namespace,
                None,
                ExtmarkPlacement::new(ExtmarkPosition::new(0, column)),
            )
            .unwrap();
    }
    let columns: Vec<_> = marks
        .query_all(
            ExtmarkPosition::new(0, 0),
            ExtmarkPosition::new(0, 3),
            None,
        )
        .into_iter()
        .map(|mark| mark.position().column)
        .collect();
    assert_eq!(columns, vec![1, 2]);
}

// Attribute shapes and priority data: src/nvim/decoration_defs.h:11-16,67-80,102-120.
#[test]
fn virtual_text_preserves_chunk_highlight_order() {
    let mut attributes = ExtmarkAttributes::default();
    attributes.virtual_text.push(VirtualTextChunk {
        text: "hint".into(),
        highlight_groups: vec!["First".into(), "Second".into()],
    });
    assert_eq!(attributes.virtual_text[0].highlight_groups, ["First", "Second"]);
}

#[test]
fn virtual_lines_preserve_line_and_chunk_shape() {
    let mut attributes = ExtmarkAttributes::default();
    attributes.virtual_lines = vec![
        vec![VirtualTextChunk::new("one")],
        vec![VirtualTextChunk::new("two"), VirtualTextChunk::new("three")],
    ];
    assert_eq!(attributes.virtual_lines.len(), 2);
    assert_eq!(attributes.virtual_lines[1].len(), 2);
}

#[test]
fn highlight_sign_and_priority_round_trip() {
    let mut marks = Extmarks::new();
    let namespace = marks.create_namespace("attrs").unwrap();
    let mut mark = ExtmarkPlacement::new(ExtmarkPosition::new(0, 0));
    mark.attributes.highlight_group = Some("Search".into());
    mark.attributes.sign_text = Some("!".into());
    mark.attributes.priority = 200;
    let id = marks.set(namespace, None, mark).unwrap();
    let attributes = &marks.get(namespace, id).unwrap().unwrap().placement.attributes;
    assert_eq!(attributes.highlight_group.as_deref(), Some("Search"));
    assert_eq!(attributes.sign_text.as_deref(), Some("!"));
    assert_eq!(attributes.priority, 200);
}

#[test]
fn render_ordered_sorts_low_priority_first_then_insertion() {
    let mut marks = Extmarks::new();
    let namespace = marks.create_namespace("order").unwrap();
    let mut late_high = ExtmarkPlacement::new(ExtmarkPosition::new(0, 0))
        .with_end(ExtmarkPosition::new(0, 2));
    late_high.attributes.highlight_group = Some("Comment".into());
    late_high.attributes.priority = 20;
    let mut early_low = ExtmarkPlacement::new(ExtmarkPosition::new(0, 0))
        .with_end(ExtmarkPosition::new(0, 2));
    early_low.attributes.highlight_group = Some("String".into());
    early_low.attributes.priority = 10;
    let high_id = marks.set(namespace, None, late_high).unwrap();
    let low_id = marks.set(namespace, None, early_low).unwrap();
    let ordered: Vec<_> = marks.render_ordered().into_iter().map(|mark| mark.id).collect();
    assert_eq!(ordered, vec![low_id, high_id]);
}

// Fold methods, nesting, lazy invalidation and z-state operations:
// src/nvim/fold.c:321-361,432-478,535-655,763-829,1122-1275,2841-2862.
#[test]
fn indent_levels_divide_spaces_by_shiftwidth() {
    assert_eq!(indent_levels(&[b"root".as_slice(), b"    child"], 4).unwrap(), vec![0, 1]);
}

#[test]
fn indent_levels_expand_tabs_to_shiftwidth() {
    assert_eq!(indent_levels(&[b"root".as_slice(), b"\tchild"], 4).unwrap(), vec![0, 1]);
}

#[test]
fn indent_levels_resolve_interior_blank_to_lower_surrounding() {
    assert_eq!(
        indent_levels(
            &[b"root".as_slice(), b"    child", b"", b"    next", b"end"],
            4,
        )
        .unwrap(),
        vec![0, 1, 1, 1, 0]
    );
}

#[test]
fn indent_levels_blank_takes_lower_of_surrounding_levels() {
    // fold.txt:54-61: a blank line takes the level above or below, lower.
    // [2, blank, 1] -> the blank resolves to 1, not the preceding 2.
    assert_eq!(
        indent_levels(&[b"    a".as_slice(), b"", b"  b"], 2).unwrap(),
        vec![2, 1, 1]
    );
}

#[test]
fn indent_levels_trailing_blank_run_resolves_to_zero() {
    // A trailing blank run has no concrete level below it; the last line is
    // forced to zero and the blanks above resolve to the lower of 1 and 0.
    assert_eq!(
        indent_levels(&[b"a".as_slice(), b"  b", b"", b""], 2).unwrap(),
        vec![0, 1, 0, 0]
    );
}

#[test]
fn indent_levels_all_blank_buffer_is_zero() {
    // First and last lines are always defined (fold.c:2852-2854); with no
    // concrete neighbor either side every blank resolves to zero.
    assert_eq!(indent_levels(&[b"".as_slice(), b"", b""], 4).unwrap(), vec![0, 0, 0]);
}

#[test]
fn zero_shiftwidth_is_rejected() {
    assert_eq!(indent_levels(&[b"x".as_slice()], 0), Err(FoldError::ZeroShiftWidth));
}

fn nested_manual_folds() -> Folds {
    let mut folds = Folds::new();
    folds
        .create_manual(Position::new(0, 0), Position::new(10, 0))
        .unwrap();
    folds
        .create_manual(Position::new(2, 0), Position::new(5, 0))
        .unwrap();
    folds
}

#[test]
fn manual_fold_normalizes_reversed_endpoints() {
    let mut folds = Folds::new();
    let range = folds
        .create_manual(Position::new(4, 0), Position::new(1, 0))
        .unwrap();
    assert_eq!(range.start, Position::new(1, 0));
    assert_eq!(range.end, Position::new(4, 0));
}

#[test]
fn manual_fold_rejects_empty_range() {
    let mut folds = Folds::new();
    assert_eq!(
        folds.create_manual(Position::new(1, 0), Position::new(1, 0)),
        Err(FoldError::EmptyRange)
    );
}

#[test]
fn nested_manual_ranges_receive_depths() {
    let folds = nested_manual_folds();
    assert_eq!(folds.folds()[0].depth, 1);
    assert_eq!(folds.folds()[1].depth, 2);
}

#[test]
fn deepest_fold_returns_nested_range() {
    let folds = nested_manual_folds();
    assert_eq!(folds.deepest_fold(Position::new(3, 0)).unwrap().depth, 2);
}

#[test]
fn fold_level_counts_all_containing_ranges() {
    let folds = nested_manual_folds();
    assert_eq!(folds.level_at(Position::new(3, 0)), 2);
}

#[test]
fn delete_manual_at_prefers_deepest() {
    let mut folds = nested_manual_folds();
    let removed = folds.delete_manual_at(Position::new(3, 0), false).unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(folds.folds().len(), 1);
}

#[test]
fn recursive_delete_removes_descendants() {
    let mut folds = nested_manual_folds();
    assert_eq!(folds.delete_manual_at(Position::new(1, 0), true).unwrap().len(), 2);
    assert!(folds.folds().is_empty());
}

#[test]
fn zo_opens_outermost_closed_fold() {
    let mut folds = nested_manual_folds();
    assert!(folds.open(Position::new(3, 0)).unwrap());
    assert_eq!(folds.folds()[0].state, FoldState::Open);
    assert_eq!(folds.folds()[1].state, FoldState::Closed);
}

#[test]
fn repeated_zo_opens_nested_fold() {
    let mut folds = nested_manual_folds();
    folds.open(Position::new(3, 0)).unwrap();
    folds.open(Position::new(3, 0)).unwrap();
    assert!(folds.folds().iter().all(|fold| fold.state == FoldState::Open));
}

#[test]
fn zc_closes_deepest_visible_fold() {
    let mut folds = nested_manual_folds();
    folds.open_all();
    folds.close(Position::new(3, 0)).unwrap();
    assert_eq!(folds.folds()[0].state, FoldState::Open);
    assert_eq!(folds.folds()[1].state, FoldState::Closed);
}

#[test]
fn za_toggles_closed_fold_open() {
    let mut folds = nested_manual_folds();
    folds.toggle(Position::new(3, 0)).unwrap();
    assert_eq!(folds.folds()[0].state, FoldState::Open);
}

#[test]
fn za_toggles_deepest_open_fold_closed() {
    let mut folds = nested_manual_folds();
    folds.open_all();
    folds.toggle(Position::new(3, 0)).unwrap();
    assert_eq!(folds.folds()[1].state, FoldState::Closed);
}

#[test]
fn z_o_opens_descendants_recursively() {
    let mut folds = nested_manual_folds();
    assert_eq!(folds.open_recursive(Position::new(3, 0)).unwrap(), 2);
    assert!(folds.folds().iter().all(|fold| fold.state == FoldState::Open));
}

#[test]
fn z_c_closes_outer_fold_recursively() {
    let mut folds = nested_manual_folds();
    folds.open_all();
    assert_eq!(folds.close_recursive(Position::new(3, 0)).unwrap(), 1);
    assert_eq!(folds.folds()[0].state, FoldState::Closed);
}

#[test]
fn z_r_opens_every_fold() {
    let mut folds = nested_manual_folds();
    assert_eq!(folds.open_all(), 2);
    assert!(folds.folds().iter().all(|fold| fold.state == FoldState::Open));
}

#[test]
fn z_m_closes_every_fold() {
    let mut folds = nested_manual_folds();
    folds.open_all();
    assert_eq!(folds.close_all(), 2);
    assert!(folds.folds().iter().all(|fold| fold.state == FoldState::Closed));
}

#[test]
fn edit_invalidation_is_lazy() {
    let mut folds = Folds::new();
    folds.set_method(FoldMethod::Indent);
    folds.refresh(1, &[b"root".as_slice(), b"  child"]).unwrap();
    assert!(!folds.is_dirty());
    folds.invalidate(2);
    assert!(folds.is_dirty());
    assert_eq!(folds.cached_changedtick(), Some(1));
}

#[test]
fn indent_refresh_recomputes_on_requested_tick() {
    let mut folds = Folds::new();
    folds.set_method(FoldMethod::Indent);
    let refresh = folds.refresh(4, &[b"root".as_slice(), b"        child"]).unwrap();
    assert!(matches!(refresh, FoldRefresh::Ready { changedtick: 4, .. }));
    assert_eq!(folds.cached_changedtick(), Some(4));
}

#[test]
fn expr_refresh_returns_typed_host_request() {
    let mut folds = Folds::new();
    folds.set_method(FoldMethod::Expr);
    let refresh = folds.refresh(7, &[b"x".as_slice()]).unwrap();
    assert!(matches!(
        refresh,
        FoldRefresh::Host(request) if request.kind == HostFoldKind::Expr && request.changedtick == 7
    ));
}

#[test]
fn syntax_refresh_returns_typed_host_request() {
    let mut folds = Folds::new();
    folds.set_method(FoldMethod::Syntax);
    assert!(matches!(
        folds.refresh(8, &[b"x".as_slice()]).unwrap(),
        FoldRefresh::Host(request) if request.kind == HostFoldKind::Syntax
    ));
}

#[test]
fn diff_refresh_returns_typed_host_request() {
    let mut folds = Folds::new();
    folds.set_method(FoldMethod::Diff);
    assert!(matches!(
        folds.refresh(9, &[b"x".as_slice()]).unwrap(),
        FoldRefresh::Host(request) if request.kind == HostFoldKind::Diff
    ));
}

#[test]
fn stale_host_result_is_rejected() {
    let mut folds = Folds::new();
    folds.set_method(FoldMethod::Expr);
    let request = match folds.refresh(2, &[b"x".as_slice()]).unwrap() {
        FoldRefresh::Host(request) => request,
        FoldRefresh::Ready { .. } => unreachable!(),
    };
    folds.invalidate(3);
    assert_eq!(
        folds.apply_host_result(FoldComputeResult {
            request,
            ranges: Vec::new(),
        }),
        Err(FoldError::StaleResult)
    );
}

#[test]
fn manual_foldtext_request_carries_depth() {
    let folds = nested_manual_folds();
    let inner = folds.folds()[1].range;
    assert_eq!(folds.fold_text_request(inner, 4).unwrap().level, 2);
}

// Modified/read-only and saved undo point: src/nvim/change.c:619-632,
// src/nvim/undo.c:2503-2513,2818-2824, and src/nvim/buffer.c:1445-1450.
fn state_with_lines(lines: &[&[u8]]) -> BufferState {
    BufferState::new(
        Buffer::from_lines(&lines.iter().map(|line| line.to_vec()).collect::<Vec<_>>(), false)
            .unwrap(),
        true,
    )
}

fn text_pos(line: usize) -> TextPosition {
    TextPosition { lnum: line, col: 0 }
}

#[test]
fn new_buffer_is_unmodified_and_writable() {
    let state = state_with_lines(&[b"a"]);
    assert!(!state.modified);
    assert!(!state.readonly);
}

#[test]
fn line_replacement_sets_modified() {
    let mut state = state_with_lines(&[b"a"]);
    state
        .replace_lines(1, 1, &[b"b".to_vec()], text_pos(1), text_pos(1), 1)
        .unwrap();
    assert!(state.modified);
}

#[test]
fn line_append_sets_modified() {
    let mut state = state_with_lines(&[b"a"]);
    state
        .append_lines(1, &[b"b".to_vec()], text_pos(1), 1)
        .unwrap();
    assert!(state.modified);
}

#[test]
fn line_delete_sets_modified() {
    let mut state = state_with_lines(&[b"a", b"b"]);
    state.delete_lines(2, 2, text_pos(2), 1).unwrap();
    assert!(state.modified);
}

#[test]
fn mark_saved_clears_modified_and_records_tick() {
    let mut state = state_with_lines(&[b"a"]);
    state
        .replace_lines(1, 1, &[b"b".to_vec()], text_pos(1), text_pos(1), 1)
        .unwrap();
    state.mark_saved();
    assert!(!state.modified);
    assert_eq!(state.saved_changedtick(), state.changedtick());
}

#[test]
fn undo_away_from_saved_point_sets_modified() {
    let mut state = state_with_lines(&[b"a"]);
    state
        .replace_lines(1, 1, &[b"b".to_vec()], text_pos(1), text_pos(1), 1)
        .unwrap();
    state.mark_saved();
    state.undo().unwrap();
    assert!(state.modified);
}

#[test]
fn redo_to_saved_point_clears_modified() {
    let mut state = state_with_lines(&[b"a"]);
    state
        .replace_lines(1, 1, &[b"b".to_vec()], text_pos(1), text_pos(1), 1)
        .unwrap();
    state.mark_saved();
    state.undo().unwrap();
    state.redo().unwrap();
    assert!(!state.modified);
}

#[test]
fn undo_to_initial_saved_point_clears_modified() {
    let mut state = state_with_lines(&[b"a"]);
    state
        .replace_lines(1, 1, &[b"b".to_vec()], text_pos(1), text_pos(1), 1)
        .unwrap();
    state.undo().unwrap();
    assert!(!state.modified);
}

#[test]
fn readonly_is_policy_data_not_mutation_suppression() {
    let mut state = state_with_lines(&[b"a"]);
    state.readonly = true;
    state
        .replace_lines(1, 1, &[b"b".to_vec()], text_pos(1), text_pos(1), 1)
        .unwrap();
    assert!(state.readonly && state.modified);
}

#[test]
fn eol_content_change_sets_modified() {
    let mut state = state_with_lines(&[b"a"]);
    state.set_eol(true).unwrap();
    assert!(state.modified);
}

#[test]
fn restoring_saved_eol_clears_modified_when_undo_matches() {
    let mut state = state_with_lines(&[b"a"]);
    state.set_eol(true).unwrap();
    assert!(state.modified);
    // Restoring the saved final-EOL with no other pending edits returns the
    // buffer to the saved undo point, so 'modified' clears.
    state.set_eol(false).unwrap();
    assert!(!state.modified);
}

#[test]
fn restoring_saved_eol_keeps_modified_with_pending_edits() {
    let mut state = state_with_lines(&[b"a"]);
    state.set_eol(true).unwrap();
    state
        .replace_lines(1, 1, &[b"b".to_vec()], text_pos(1), text_pos(1), 1)
        .unwrap();
    // Restoring the saved EOL keeps 'modified' set because pending text edits
    // still diverge from the saved undo point.
    state.set_eol(false).unwrap();
    assert!(state.modified);
}

#[test]
fn buffer_pipeline_splices_extmarks_and_invalidates_folds() {
    let mut state = state_with_lines(&[b"a", b"b"]);
    let namespace = state.extmarks.create_namespace("pipeline").unwrap();
    let id = state
        .extmarks
        .set(
            namespace,
            None,
            ExtmarkPlacement::new(ExtmarkPosition::new(1, 0)),
        )
        .unwrap();
    state.folds.set_method(FoldMethod::Indent);
    state.folds.refresh(state.changedtick(), &[b"a".as_slice(), b"b"]).unwrap();
    state
        .append_lines(1, &[b"inserted".to_vec()], text_pos(1), 1)
        .unwrap();
    assert_eq!(
        state.extmarks.get(namespace, id).unwrap().unwrap().position(),
        ExtmarkPosition::new(2, 0)
    );
    assert!(state.folds.is_dirty());
}

#[test]
fn buffer_load_resets_resident_extmarks_and_folds() {
    let mut state = state_with_lines(&[b"a"]);
    state.extmarks.create_namespace("old").unwrap();
    state
        .folds
        .create_manual(Position::new(0, 0), Position::new(1, 0))
        .unwrap();
    state.load(Buffer::from_lines(&[b"new".to_vec()], false).unwrap());
    assert!(state.extmarks.namespace("old").is_none());
    assert!(state.folds.folds().is_empty());
}

// Provider lifecycle and aggregation: src/nvim/decoration_provider.c:108-284 and
// src/nvim/decoration.c:567-570,737-751.
fn row_range(row: u32) -> DecorRange {
    DecorRange {
        start: DecorPos { row, col: 0 },
        end: DecorPos { row, col: 10 },
    }
}

fn provider_item(
    provider: ProviderId,
    window: WindowId,
    row: u32,
    priority: u32,
) -> DecorItem {
    DecorItem::for_provider(
        provider,
        CallbackPhase::Line,
        window,
        row_range(row),
        priority,
        None,
        None,
        None,
    )
}

#[test]
fn providers_remain_in_registration_order() {
    let mut decorations = Decorations::new();
    let first = decorations.register(DecorProviderDef::default()).unwrap();
    let second = decorations.register(DecorProviderDef::default()).unwrap();
    assert_eq!(decorations.provider_order(), [first, second]);
}

#[test]
fn provider_update_preserves_registration_order() {
    let mut decorations = Decorations::new();
    let first = decorations.register(DecorProviderDef::default()).unwrap();
    let second = decorations.register(DecorProviderDef::default()).unwrap();
    decorations
        .update(
            first,
            DecorProviderDef {
                line: Some(LineCallbackId::new(9)),
                ..DecorProviderDef::default()
            },
        )
        .unwrap();
    assert_eq!(decorations.provider_order(), [first, second]);
}

#[test]
fn provider_removal_compacts_order_without_reordering() {
    let mut decorations = Decorations::new();
    let first = decorations.register(DecorProviderDef::default()).unwrap();
    let middle = decorations.register(DecorProviderDef::default()).unwrap();
    let last = decorations.register(DecorProviderDef::default()).unwrap();
    decorations.remove(middle).unwrap();
    assert_eq!(decorations.provider_order(), [first, last]);
}

#[test]
fn phase_plan_keeps_provider_order_per_phase() {
    let mut decorations = Decorations::new();
    let first = decorations
        .register(DecorProviderDef {
            start: Some(StartCallbackId::new(1)),
            buf: Some(BufCallbackId::new(2)),
            win: Some(WinCallbackId::new(3)),
            line: Some(LineCallbackId::new(4)),
            ..DecorProviderDef::default()
        })
        .unwrap();
    let second = decorations
        .register(DecorProviderDef {
            line: Some(LineCallbackId::new(5)),
            ..DecorProviderDef::default()
        })
        .unwrap();
    let redraw = decorations.begin_redraw(10).unwrap();
    let plan = redraw.phase_plans();
    assert_eq!(plan.start[0].0, first);
    assert_eq!(plan.buf[0].0, first);
    assert_eq!(plan.win[0].0, first);
    assert_eq!(plan.line.iter().map(|entry| entry.0).collect::<Vec<_>>(), vec![first, second]);
}

#[test]
fn disabled_provider_is_absent_from_phase_plan() {
    let mut decorations = Decorations::new();
    let provider = decorations
        .register(DecorProviderDef {
            line: Some(LineCallbackId::new(1)),
            ..DecorProviderDef::default()
        })
        .unwrap();
    decorations.set_enabled(provider, false).unwrap();
    let redraw = decorations.begin_redraw(1).unwrap();
    assert!(redraw.phase_plans().line.is_empty());
}

#[test]
fn ephemeral_decoration_is_visible_only_during_own_redraw() {
    let mut decorations = Decorations::new();
    let provider = decorations.register(DecorProviderDef::default()).unwrap();
    let window = WindowId(1);
    {
        let mut redraw = decorations.begin_redraw(1).unwrap();
        redraw.push_ephemeral(provider_item(provider, window, 2, 10)).unwrap();
        assert_eq!(redraw.query_line(&[], window, 2).len(), 1);
        redraw.end().unwrap();
    }
    let redraw = decorations.begin_redraw(2).unwrap();
    assert!(redraw.query_line(&[], window, 2).is_empty());
}

#[test]
fn dropping_redraw_discards_ephemeral_decorations() {
    let mut decorations = Decorations::new();
    let provider = decorations.register(DecorProviderDef::default()).unwrap();
    let window = WindowId(1);
    {
        let mut redraw = decorations.begin_redraw(1).unwrap();
        redraw.push_ephemeral(provider_item(provider, window, 2, 10)).unwrap();
    }
    let redraw = decorations.begin_redraw(2).unwrap();
    assert!(redraw.query_line(&[], window, 2).is_empty());
}

#[test]
fn query_filters_ephemeral_by_window() {
    let mut decorations = Decorations::new();
    let provider = decorations.register(DecorProviderDef::default()).unwrap();
    let mut redraw = decorations.begin_redraw(1).unwrap();
    redraw
        .push_ephemeral(provider_item(provider, WindowId(1), 2, 10))
        .unwrap();
    assert!(redraw.query_line(&[], WindowId(2), 2).is_empty());
}

#[test]
fn query_filters_ephemeral_by_line() {
    let mut decorations = Decorations::new();
    let provider = decorations.register(DecorProviderDef::default()).unwrap();
    let mut redraw = decorations.begin_redraw(1).unwrap();
    redraw
        .push_ephemeral(provider_item(provider, WindowId(1), 2, 10))
        .unwrap();
    assert!(redraw.query_line(&[], WindowId(1), 3).is_empty());
}

#[test]
fn aggregation_orders_lower_priority_first() {
    let mut decorations = Decorations::new();
    let provider = decorations.register(DecorProviderDef::default()).unwrap();
    let window = WindowId(1);
    let mut redraw = decorations.begin_redraw(1).unwrap();
    redraw.push_ephemeral(provider_item(provider, window, 2, 30)).unwrap();
    redraw.push_ephemeral(provider_item(provider, window, 2, 10)).unwrap();
    let priorities: Vec<_> = redraw
        .query_line(&[], window, 2)
        .into_iter()
        .map(|item| item.priority)
        .collect();
    assert_eq!(priorities, vec![10, 30]);
}

#[test]
fn equal_priority_extmark_precedes_provider_output() {
    let mut decorations = Decorations::new();
    let provider = decorations.register(DecorProviderDef::default()).unwrap();
    let window = WindowId(1);
    let persistent = DecorItem {
        origin: DecorOrigin::Extmark {
            namespace: 1,
            mark_id: 1,
            order: 0,
        },
        window: Some(window),
        range: row_range(2),
        priority: 10,
        winblend: None,
        virt_text: None,
        virt_lines: None,
    };
    let mut redraw = decorations.begin_redraw(1).unwrap();
    redraw.push_ephemeral(provider_item(provider, window, 2, 10)).unwrap();
    assert!(matches!(
        redraw.query_line(&[persistent], window, 2)[0].origin,
        DecorOrigin::Extmark { .. }
    ));
}

#[test]
fn equal_priority_providers_follow_registration_order() {
    let mut decorations = Decorations::new();
    let first = decorations.register(DecorProviderDef::default()).unwrap();
    let second = decorations.register(DecorProviderDef::default()).unwrap();
    let window = WindowId(1);
    let mut redraw = decorations.begin_redraw(1).unwrap();
    redraw.push_ephemeral(provider_item(second, window, 2, 10)).unwrap();
    redraw.push_ephemeral(provider_item(first, window, 2, 10)).unwrap();
    let origins: Vec<_> = redraw
        .query_line(&[], window, 2)
        .into_iter()
        .map(|item| item.origin)
        .collect();
    assert!(matches!(origins[0], DecorOrigin::Provider { provider, .. } if provider == first));
    assert!(matches!(origins[1], DecorOrigin::Provider { provider, .. } if provider == second));
}

#[test]
fn aggregation_preserves_winblend_and_virtual_text() {
    let mut decorations = Decorations::new();
    let provider = decorations.register(DecorProviderDef::default()).unwrap();
    let window = WindowId(1);
    let item = DecorItem::for_provider(
        provider,
        CallbackPhase::Line,
        window,
        row_range(2),
        10,
        Some(20),
        Some(DecorVirtualText {
            chunks: vec![DecorTextChunk {
                text: "hint".into(),
                hl_group: Some("Comment".into()),
            }],
            ..DecorVirtualText::default()
        }),
        None,
    );
    let mut redraw = decorations.begin_redraw(1).unwrap();
    redraw.push_ephemeral(item).unwrap();
    let output = redraw.query_line(&[], window, 2);
    assert_eq!(output[0].winblend, Some(20));
    assert_eq!(output[0].virt_text.as_ref().unwrap().chunks[0].text, "hint");
}

#[test]
fn whole_line_deletion_invalidates_point_mark() {
    let mut marks = Extmarks::new();
    let namespace = marks.create_namespace("point-invalid").unwrap();
    let mut point = ExtmarkPlacement::new(ExtmarkPosition::new(2, 3));
    point.attributes.invalidate = true;
    let id = marks.set(namespace, None, point).unwrap();
    marks.splice(crate::extmark::TextSplice {

        start: ExtmarkPosition::new(2, 0),

        old_extent: TextExtent::new(1, 0),

        new_extent: TextExtent::EMPTY,

    });
    assert!(marks.get(namespace, id).unwrap().unwrap().invalid);
}

#[test]
fn undo_restores_invalidated_extmark_range_and_position() {
    let mut state = state_with_lines(&[b"a", b"b", b"c"]);
    let namespace = state.extmarks.create_namespace("undo-invalid").unwrap();
    let mut placement = ExtmarkPlacement::new(ExtmarkPosition::new(1, 0))
        .with_end(ExtmarkPosition::new(2, 0));
    placement.attributes.invalidate = true;
    let id = state.extmarks.set(namespace, None, placement).unwrap();
    state.delete_lines(2, 2, text_pos(2), 1).unwrap();
    assert!(state.extmarks.get(namespace, id).unwrap().unwrap().invalid);
    state.undo().unwrap();
    let restored = state.extmarks.get(namespace, id).unwrap().unwrap();
    assert!(!restored.invalid);
    assert_eq!(restored.position(), ExtmarkPosition::new(1, 0));
    assert_eq!(restored.placement.end.unwrap().position, ExtmarkPosition::new(2, 0));
}

#[test]
fn inserting_lines_above_manual_fold_splices_its_rows() {
    let mut state = state_with_lines(&[b"a", b"b", b"c", b"d"]);
    state
        .folds
        .create_manual(Position::new(2, 0), Position::new(4, 0))
        .unwrap();
    state
        .append_lines(1, &[b"x".to_vec(), b"y".to_vec()], text_pos(1), 1)
        .unwrap();
    assert_eq!(state.folds.folds()[0].range.start.row, 4);
    assert_eq!(state.folds.folds()[0].range.end.row, 6);
}

#[test]
fn range_callbacks_are_present_in_phase_plan_order() {
    let mut decorations = Decorations::new();
    let first = decorations
        .register(DecorProviderDef {
            range: Some(RangeCallbackId::new(1)),
            ..DecorProviderDef::default()
        })
        .unwrap();
    let second = decorations
        .register(DecorProviderDef {
            range: Some(RangeCallbackId::new(2)),
            ..DecorProviderDef::default()
        })
        .unwrap();
    let redraw = decorations.begin_redraw(1).unwrap();
    assert_eq!(
        redraw
            .phase_plans()
            .range
            .into_iter()
            .map(|entry| entry.0)
            .collect::<Vec<_>>(),
        vec![first, second]
    );
}

#[test]
fn undo_splices_extmark_created_after_original_edit() {
    let mut state = state_with_lines(&[b"a", b"b", b"c"]);
    state
        .append_lines(1, &[b"inserted".to_vec()], text_pos(1), 1)
        .unwrap();
    let namespace = state.extmarks.create_namespace("late").unwrap();
    let id = state
        .extmarks
        .set(
            namespace,
            None,
            ExtmarkPlacement::new(ExtmarkPosition::new(3, 0)),
        )
        .unwrap();
    state.undo().unwrap();
    assert_eq!(
        state.extmarks.get(namespace, id).unwrap().unwrap().position(),
        ExtmarkPosition::new(2, 0)
    );
}

#[test]
fn undo_line_edit_preserves_unsaved_eol_modified_state() {
    let mut state = state_with_lines(&[b"a"]);
    state.mark_saved();
    state.set_eol(true).unwrap();
    state
        .replace_lines(1, 1, &[b"b".to_vec()], text_pos(1), text_pos(1), 1)
        .unwrap();
    state.undo().unwrap();
    assert!(state.modified);
}
