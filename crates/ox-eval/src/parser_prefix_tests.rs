#![allow(clippy::unwrap_used)]

use crate::parse_expression_prefix;
use crate::parser::ExprKind;

#[test]
fn parse_expression_prefix_parses_indexed_lvalue() {
    let (expr, consumed) = parse_expression_prefix(b"g:l[1]").unwrap();
    assert_eq!(consumed, b"g:l[1]".len());
    assert!(matches!(expr.kind, ExprKind::Index { .. }));
}

#[test]
fn parse_expression_prefix_allows_trailing_remainder() {
    let source = b"g:l[1] trailing";
    let (expr, consumed) = parse_expression_prefix(source).unwrap();
    assert_eq!(consumed, b"g:l[1]".len());
    assert!(consumed < source.len());
    assert!(matches!(expr.kind, ExprKind::Index { .. }));
}

#[test]
fn normalize_list_index_negative_and_bounds() {
    use crate::{list_slice_bounds, normalize_list_index};
    assert_eq!(normalize_list_index(3, -1), Some(2));
    assert_eq!(normalize_list_index(3, 3), None);
    assert_eq!(list_slice_bounds(4, Some(1), Some(2)), (1, 3));
}
