#![allow(clippy::unwrap_used)]

use crate::parser::ExprKind;
use crate::{Parser, list_slice_bounds, normalize_list_index};

#[test]
fn parse_expression_prefix_parses_indexed_lvalue() {
    let expr = Parser::new(b"g:l[1]").parse().unwrap();
    assert_eq!(expr.span.end, b"g:l[1]".len());
    assert!(matches!(expr.kind, ExprKind::Index { .. }));
}

#[test]
fn parse_expression_prefix_rejects_trailing_remainder() {
    // The free prefix function is gone; the current `Parser::parse`
    // contract reports leftover input as E488 (eval.c:1251). The span of
    // a complete parse still ends exactly at the expression's last byte.
    let trailing = Parser::new(b"g:l[1] trailing").parse().unwrap_err();
    assert_eq!(trailing.code, "E488");
}

#[test]
fn normalize_list_index_negative_and_bounds() {
    assert_eq!(normalize_list_index(3, -1), Some(2));
    assert_eq!(normalize_list_index(3, 3), None);
    assert_eq!(list_slice_bounds(4, Some(1), Some(2)), (1, 3));
}
