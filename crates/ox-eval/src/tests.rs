#![allow(clippy::unwrap_used, clippy::expect_used)]

use ox_types::{Funcref, OxStr, Special, Typval};

use crate::eval::{BuiltinHost, Evaluator, RegexEngine};
use crate::lexer::{Lexer, TokenKind};
use crate::parser::Parser;
use crate::scope::{OptionScope, Scope, ScopeKind};
use crate::{EvalError, Result};

#[derive(Default)]
struct Host;

impl BuiltinHost for Host {
    fn call(&mut self, name: &OxStr, mut args: Vec<Typval>, _scope: &mut Scope) -> Result<Typval> {
        match name.as_bytes() {
            b"id" => args.into_iter().next().ok_or_else(|| EvalError::new("E119", 0, "argument required")),
            b"len" => match args.first() {
                Some(Typval::List(v)) => Ok(Typval::Number(v.len() as i64)),
                Some(Typval::String(v)) => Ok(Typval::Number(v.as_bytes().len() as i64)),
                Some(Typval::Dict(v)) => Ok(Typval::Number(v.len() as i64)),
                _ => Err(EvalError::new("E701", 0, "invalid type for len")),
            },
            b"first" => args.into_iter().next().ok_or_else(|| EvalError::new("E119", 0, "argument required")),
            b"sum" => {
                let mut total = 0;
                for arg in args { if let Typval::Number(n) = arg { total += n; } }
                Ok(Typval::Number(total))
            }
            b"append" => {
                if args.len() != 2 { return Err(EvalError::new("E119", 0, "two arguments required")); }
                let rhs = args.pop().unwrap();
                let lhs = args.pop().unwrap();
                match lhs {
                    Typval::List(mut list) => { list.push(rhs); Ok(Typval::List(list)) }
                    _ => Err(EvalError::new("E714", 0, "List required")),
                }
            }
            _ => Err(EvalError::new("E117", 0, format!("Unknown function: {}", name.to_string_lossy()))),
        }
    }
}

struct Regex;

impl RegexEngine for Regex {
    fn is_match(&self, text: &OxStr, pattern: &OxStr, ignore_case: bool) -> Result<bool> {
        let mut text = text.as_bytes().to_vec();
        let mut pattern = pattern.as_bytes().to_vec();
        if ignore_case {
            text.make_ascii_lowercase();
            pattern.make_ascii_lowercase();
        }
        let matched = if pattern.starts_with(b"^") && pattern.ends_with(b"$") {
            text == pattern[1..pattern.len() - 1]
        } else {
            text.windows(pattern.len()).any(|window| window == pattern)
        };
        Ok(matched)
    }
}

fn value(source: &[u8]) -> Typval {
    let expression = Parser::new(source).parse().unwrap();
    let mut host = Host;
    let regex = Regex;
    let mut evaluator = Evaluator::new(&mut host, &regex);
    evaluator.eval(&expression, &mut Scope::new()).unwrap()
}

fn value_in(source: &[u8], scope: &mut Scope) -> Typval {
    let expression = Parser::new(source).parse().unwrap();
    let mut host = Host;
    let regex = Regex;
    let mut evaluator = Evaluator::new(&mut host, &regex);
    evaluator.eval(&expression, scope).unwrap()
}

fn error(source: &[u8]) -> EvalError {
    match Parser::new(source).parse() {
        Ok(expression) => {
            let mut host = Host;
            let regex = Regex;
            let mut evaluator = Evaluator::new(&mut host, &regex);
            evaluator.eval(&expression, &mut Scope::new()).unwrap_err()
        }
        Err(error) => error,
    }
}

macro_rules! eval_cases {
    ($(($name:ident, $source:expr, $expected:expr, $citation:expr)),+ $(,)?) => {$(
        #[test]
        fn $name() {
            let _upstream_source = $citation;
            assert_eq!(value($source), $expected);
        }
    )+};
}

eval_cases!(
    (precedence_multiply_add, b"1 + 2 * 3", Typval::Number(7), "vimeval.txt:856-867"),
    (precedence_parentheses, b"(1 + 2) * 3", Typval::Number(9), "vimeval.txt:886"),
    (precedence_subtract_left, b"10 - 4 - 2", Typval::Number(4), "eval.c:2424"),
    (precedence_divide_left, b"20 / 4 / 2", Typval::Number(2), "eval.c:2580"),
    (precedence_mixed_arithmetic, b"10 - 2 * 3 + 4", Typval::Number(8), "vimeval.txt:856-867"),
    (precedence_and_before_or, b"1 || 0 && 0", Typval::Number(1), "vimeval.txt:831-835"),
    (precedence_and_or_left, b"0 && 1 || 1", Typval::Number(1), "eval.c:2013-2134"),
    (precedence_ternary_low, b"1 ? 2 + 3 : 4 * 5", Typval::Number(5), "vimeval.txt:907-923"),
    (ternary_false_branch, b"0 ? 2 + 3 : 4 * 5", Typval::Number(20), "eval.c:1915"),
    (unary_not_before_add, b"!0 + !0", Typval::Number(2), "vimeval.txt:868-870"),
    (unary_chaining, b"!-!1", Typval::Number(1), "eval.c:2829"),
    (unary_double_minus, b"+--5", Typval::Number(5), "vimeval.txt:1152-1167"),
    (literal_decimal, b"12345", Typval::Number(12345), "vimeval.txt:1331-1353"),
    (literal_hex, b"0x1f", Typval::Number(31), "vimeval.txt:1333-1343"),
    (literal_legacy_octal, b"010", Typval::Number(8), "vimeval.txt:1333-1343"),
    (literal_explicit_octal, b"0o77", Typval::Number(63), "vimeval.txt:1333-1343"),
    (literal_binary, b"0b1010", Typval::Number(10), "vimeval.txt:1333-1343"),
    (literal_float, b"1.25", Typval::Float(1.25), "vimeval.txt:1354-1377"),
    (literal_exponent, b"1.0e-2", Typval::Float(0.01), "vimeval.txt:1354-1377"),
    (literal_double_escapes, b"\"a\\nb\\tc\\\"\\\\\"", Typval::String(OxStr(b"a\nb\tc\"\\".to_vec())), "vimeval.txt:1413-1448"),
    (literal_single_quote, b"'a''b'", Typval::String(OxStr(b"a'b".to_vec())), "vimeval.txt:1461-1474"),
    (literal_blob, b"0z0102.0304", Typval::Blob(vec![1,2,3,4]), "vimeval.txt:1451-1458"),
    (literal_nested_list, b"[1, 'two', [3]]", Typval::List(vec![Typval::Number(1), Typval::String(OxStr::from("two")), Typval::List(vec![Typval::Number(3)])]), "vimeval.txt:882"),
    (literal_list_trailing_comma, b"[1, 2,]", Typval::List(vec![Typval::Number(1), Typval::Number(2)]), "eval.c:3902"),
    (literal_dict, b"{'a': 1, 'b': 2,}", Typval::Dict(vec![(OxStr::from("a"), Typval::Number(1)), (OxStr::from("b"), Typval::Number(2))]), "vimeval.txt:883"),
    (literal_hash_dict, b"#{a: 1, b: 2}", Typval::Dict(vec![(OxStr::from("a"), Typval::Number(1)), (OxStr::from("b"), Typval::Number(2))]), "vimeval.txt:884"),
    (coerce_not_text, b"!'hello'", Typval::Number(1), "typval.c:4292-4311"),
    (coerce_not_number_text, b"!'123'", Typval::Number(0), "typval.c:4306-4311"),
    (coerce_not_zero_text, b"!'0'", Typval::Number(1), "typval.c:4306-4311"),
    (coerce_numeric_prefix, b"1 + '20abc'", Typval::Number(21), "eval.c:2351-2409"),
    (coerce_non_numeric, b"1 + 'abc'", Typval::Number(1), "typval.c:4306-4311"),
    (coerce_two_strings_add, b"'10' + '20'", Typval::Number(30), "vimeval.txt:1114-1120"),
    (concat_numbers_dot, b"1 . 2", Typval::String(OxStr::from("12")), "vimeval.txt:1117-1120"),
    (concat_numbers_dotdot, b"1 .. 2", Typval::String(OxStr::from("12")), "vimeval.txt:1097-1107"),
    (float_promotion, b"1.5 + 2", Typval::Float(3.5), "eval.c:2358-2408"),
    (bool_number_coercion, b"v:true + 2", Typval::Number(3), "typval.c:4313-4316"),
    (coalesce_true, b"v:true ?? 456", Typval::Bool(true), "test_expr.vim:68"),
    (coalesce_false, b"v:false ?? 456", Typval::Number(456), "test_expr.vim:76"),
    (coalesce_zero, b"0 ?? 456", Typval::Number(456), "test_expr.vim:77"),
    (coalesce_number, b"123 ?? 456", Typval::Number(123), "test_expr.vim:69"),
    (coalesce_empty_string, b"'' ?? 456", Typval::Number(456), "test_expr.vim:78"),
    (coalesce_zero_string_truthy, b"'0' ?? 456", Typval::String(OxStr::from("0")), "typval.c:4778-4804"),
    (coalesce_empty_list, b"[] ?? 456", Typval::Number(456), "test_expr.vim:80"),
    (coalesce_nonempty_list, b"[0] ?? 456", Typval::List(vec![Typval::Number(0)]), "test_expr.vim:72"),
    (coalesce_empty_dict, b"{} ?? 456", Typval::Number(456), "test_expr.vim:81"),
    (coalesce_null, b"v:null ?? 456", Typval::Number(456), "test_expr.vim:84"),
    (compare_string_equal, b"'abc' == 'abc'", Typval::Number(1), "vimeval.txt:993-1017"),
    (compare_case_sensitive, b"'abc' ==# 'ABC'", Typval::Number(0), "vimeval.txt:1075-1081"),
    (compare_case_insensitive, b"'abc' ==? 'ABC'", Typval::Number(1), "vimeval.txt:1078-1081"),
    (compare_string_number, b"'0' == 0", Typval::Number(1), "vimeval.txt:1062-1069"),
    (compare_non_numeric_string_number, b"'abc' == 0", Typval::Number(1), "vimeval.txt:1062-1069"),
    (compare_strings_no_numeric_coercion, b"'0' == '00'", Typval::Number(0), "vimeval.txt:1071-1077"),
    (compare_float_number, b"1.0 == 1", Typval::Number(1), "eval.c:6815"),
    (compare_list_equal, b"[1, 2] == [1, 2]", Typval::Number(1), "vimeval.txt:1025-1029"),
    (compare_list_not_equal, b"[1, 2] != [1, 3]", Typval::Number(1), "vimeval.txt:1025-1029"),
    (compare_dict_equal, b"{'a': 1} == {'a': 1}", Typval::Number(1), "vimeval.txt:1031"),
    (identity_distinct_lists, b"[1] is [1]", Typval::Number(0), "vimeval.txt:1049-1053"),
    (identity_distinct_lists_not, b"[1] isnot [1]", Typval::Number(1), "vimeval.txt:1049-1053"),
    (index_string_first, b"'axb'[0]", Typval::String(OxStr::from("a")), "vimeval.txt:1182-1202"),
    (index_string_last, b"'axb'[2]", Typval::String(OxStr::from("b")), "vimeval.txt:1193-1202"),
    (index_string_oob, b"'axb'[3]", Typval::String(OxStr::from("")), "vimeval.txt:1200-1202"),
    (index_string_negative, b"'axb'[-1]", Typval::String(OxStr::from("")), "vimeval.txt:1200-1202"),
    (slice_string_prefix, b"'editor'[:3]", Typval::String(OxStr::from("edit")), "vimeval.txt:1214-1228"),
    (slice_string_middle, b"'editor'[2:4]", Typval::String(OxStr::from("ito")), "vimeval.txt:1214-1228"),
    (slice_string_negative, b"'editor'[-3:]", Typval::String(OxStr::from("tor")), "vimeval.txt:1230-1241"),
    (index_list_first, b"[10, 20, 30][0]", Typval::Number(10), "vimeval.txt:1204-1211"),
    (index_list_negative, b"[10, 20, 30][-1]", Typval::Number(30), "vimeval.txt:1204-1211"),
    (slice_list_middle, b"[1, 2, 3, 4][1:2]", Typval::List(vec![Typval::Number(2), Typval::Number(3)]), "vimeval.txt:1243-1249"),
    (slice_list_prefix, b"[1, 2, 3][:1]", Typval::List(vec![Typval::Number(1), Typval::Number(2)]), "vimeval.txt:1243-1249"),
    (slice_list_clamped, b"[1, 2][0:8]", Typval::List(vec![Typval::Number(1), Typval::Number(2)]), "test_listdict.vim:37"),
    (slice_list_empty_high, b"[1, 2][8:]", Typval::List(vec![]), "test_listdict.vim:38"),
    (slice_list_empty_low, b"[1, 2][-3:-1]", Typval::List(vec![]), "test_listdict.vim:47"),
    (dict_index_string, b"{'name': 'ox'}['name']", Typval::String(OxStr::from("ox")), "vimeval.txt:1266-1280"),
    (dict_member, b"{'name': 'ox'}.name", Typval::String(OxStr::from("ox")), "vimeval.txt:1266-1280"),
    (dict_index_number_key, b"{0: 'zero'}[0]", Typval::String(OxStr::from("zero")), "test_expr.vim:111"),
    (dict_nested_members, b"{'a': {'b': 99}}.a.b", Typval::Number(99), "vimeval.txt:1266-1280"),
    (dict_member_then_index, b"{'a': [10, 20]}.a[1]", Typval::Number(20), "vimeval.txt:1173-1179"),
    (lambda_nullary, b"{-> 42}()", Typval::Number(42), "vimeval.txt:1608-1626"),
    (lambda_binary, b"{a, b -> a + b}(10, 20)", Typval::Number(30), "test_lambda.vim:56"),
    (lambda_unary, b"{x -> x * 2}(21)", Typval::Number(42), "vimeval.txt:1611-1626"),
    (lambda_returns_list, b"{x -> [x, x + 1]}(3)", Typval::List(vec![Typval::Number(3), Typval::Number(4)]), "vimeval.txt:1641-1647"),
    (lambda_variadic_count, b"{... -> a:0}(1, 2, 3)", Typval::Number(3), "test_lambda.vim:55-58"),
    (lambda_variadic_list, b"{... -> a:000}(7, 8)", Typval::List(vec![Typval::Number(7), Typval::Number(8)]), "userfunc.txt:a:000"),
    (lambda_variadic_index, b"{... -> a:1}(10, 20)", Typval::Number(10), "userfunc.txt:a:1"),
    (lambda_named_then_variadic, b"{a, ... -> a + a:0}(1, 2, 3)", Typval::Number(3), "test_lambda.vim:55-62"),
    (lambda_extra_args_accepted, b"{a -> a}(1, 2, 3)", Typval::Number(1), "userfunc.c:396 uf_varargs=true"),
    (string_multibyte_fold_equal, b"\"\xc3\x84\" ==? \"\xc3\xa4\"", Typval::Number(1), "mbyte.c:mb_stricmp"),
    (string_multibyte_fold_sensitive, b"\"\xc3\x84\" ==# \"\xc3\xa4\"", Typval::Number(0), "vimeval.txt:1075-1081"),
    (turkish_dotted_i_fold_equal, b"\"\xc4\xb0\" ==? \"i\xcc\x87\"", Typval::Number(1), "mbyte.c:mb_stricmp"),
    (turkish_dotted_i_fold_equal_reverse, b"\"i\xcc\x87\" ==? \"\xc4\xb0\"", Typval::Number(1), "mbyte.c:mb_stricmp"),
    (turkish_dotted_i_fold_sensitive, b"\"\xc4\xb0\" ==# \"i\xcc\x87\"", Typval::Number(0), "vimeval.txt:1075-1081"),
    (casefold_ordering_lower_first, b"'a' <? 'B'", Typval::Number(1), "mbyte.c:mb_stricmp"),
    (casefold_ordering_upper_greater, b"'B' >? 'a'", Typval::Number(1), "mbyte.c:mb_stricmp"),
    (turkish_dotted_i_fold_ordering, b"\"\xc4\xb0\" >? \"I\"", Typval::Number(1), "mbyte.c:mb_stricmp"),
    (method_len, b"[1, 2, 3]->len()", Typval::Number(3), "vimeval.txt:1292-1305"),
    (method_chain, b"[1, 2, 3]->len()->id()", Typval::Number(3), "vimeval.txt:1300-1305"),
    (method_receiver_first, b"10->sum(20, 30)", Typval::Number(60), "vimeval.txt:1295-1298")
);

macro_rules! error_cases {
    ($(($name:ident, $source:expr, $code:expr, $citation:expr)),+ $(,)?) => {$(
        #[test]
        fn $name() {
            let _upstream_source = $citation;
            assert_eq!(error($source).code, $code);
        }
    )+};
}

error_cases!(
    (error_missing_ternary_colon, b"1 ? 2", "E109", "eval.c:1971"),
    (error_missing_parenthesis, b"(1 + 2", "E110", "eval.c:2758"),
    (error_list_index_high, b"[1, 2][5]", "E684", "typval.c:949"),
    (error_list_compare_scalar, b"[1] == 1", "E691", "eval.c:6789"),
    (error_dict_compare_scalar, b"{} == 1", "E735", "eval.c:6803"),
    (error_missing_dict_key, b"{'a': 1}['b']", "E716", "eval.c:3383"),
    (error_dict_as_number, b"{} + 1", "E728", "typval.c:4177"),
    (error_list_as_string, b"[] . 'a'", "E730", "typval.c:4223"),
    (error_dict_as_string, b"{} . 'a'", "E731", "typval.c:4224"),
    (error_list_as_number, b"[1] + 1", "E745", "typval.c:4176"),
    (error_float_modulo, b"1.5 % 2", "E804", "eval.c:2543"),
    (error_odd_blob, b"0z123", "E973", "eval.c:3497"),
    (error_undefined_variable, b"missing", "E121", "vimeval.txt:1683-1687"),
    (error_undefined_function, b"missing()", "E117", "eval.c:E117"),
    (error_duplicate_dict_key, b"{'a': 1, 'a': 2}", "E721", "eval.c:E721"),
    (error_duplicate_lambda_param, b"{a, a -> a + a}(1, 2)", "E853", "test_lambda.vim:61"),
    (error_duplicate_lambda_param_types, b"{list, list -> 1}([1], [2])", "E853", "userfunc.c:134"),
    (error_list_relational_compare, b"[1] > [2]", "E692", "eval.c:6789-6791"),
    (error_dict_relational_compare, b"{'a': 1} > {'a': 0}", "E736", "eval.c:6822-6828"),
    (error_float_string_concat, b"1.5 .. 'x'", "E806", "vimeval.txt:1121-1131"),
    (error_chained_comparison, b"1 < 2 < 3", "E15", "vimeval.txt:824-896"),
    (error_dictionary_slice, b"{'a': 1}[0:0]", "E719", "eval.c:eval_index_inner E719")
);

#[test]
fn newline_without_marker_ends_expression() {
    // runtime/doc/vimscript.txt:line-continuation.
    assert_eq!(value(b"1\n+ 2"), Typval::Number(1));
}

#[test]
fn unary_not_float_zero() {
    // eval.c:2829-2875.
    assert_eq!(value(b"!0.0"), Typval::Number(1));
}

#[test]
fn unary_not_float_nonzero() {
    // eval.c:2829-2875.
    assert_eq!(value(b"!1.5"), Typval::Number(0));
}

#[test]
fn same_variable_preserves_list_identity() {
    // runtime/doc/vimeval.txt:1049-1053.
    let mut scope = Scope::new();
    scope.set(b"xs", Typval::List(vec![Typval::Number(1)]));
    assert_eq!(value_in(b"xs is xs", &mut scope), Typval::Number(1));
}

#[test]
fn repeated_list_index_preserves_nested_identity() {
    // runtime/doc/vimeval.txt:1049-1053 and 1182-1211.
    let mut scope = Scope::new();
    scope.set(b"xs", Typval::List(vec![Typval::List(vec![Typval::Number(1)])]));
    assert_eq!(value_in(b"xs[0] is xs[0]", &mut scope), Typval::Number(1));
}

#[test]
fn repeated_dict_member_preserves_nested_identity() {
    // runtime/doc/vimeval.txt:1049-1053 and 1266-1280.
    let mut scope = Scope::new();
    scope.set(b"d", Typval::Dict(vec![(OxStr::from("x"), Typval::List(vec![]))]));
    assert_eq!(value_in(b"d.x is d.x", &mut scope), Typval::Number(1));
}

#[test]
fn scope_local_precedes_argument_and_global() {
    // runtime/doc/vimeval.txt:1690-1701, brief resolution contract.
    let mut scope = Scope::new();
    scope.global.push((OxStr::from("x"), Typval::Number(1)));
    scope.argument.push((OxStr::from("x"), Typval::Number(2)));
    scope.local.push((OxStr::from("x"), Typval::Number(3)));
    assert_eq!(value_in(b"x", &mut scope), Typval::Number(3));
}

#[test]
fn scope_argument_precedes_global() {
    // runtime/doc/vimeval.txt:1690-1701, brief resolution contract.
    let mut scope = Scope::new();
    scope.global.push((OxStr::from("x"), Typval::Number(1)));
    scope.argument.push((OxStr::from("x"), Typval::Number(2)));
    assert_eq!(value_in(b"x", &mut scope), Typval::Number(2));
}

#[test]
fn explicit_global_scope() {
    // runtime/doc/vimeval.txt:1690-1701.
    let mut scope = Scope::new();
    scope.global.push((OxStr::from("x"), Typval::Number(7)));
    assert_eq!(value_in(b"g:x", &mut scope), Typval::Number(7));
}

#[test]
fn bare_global_scope_returns_dictionary() {
    // runtime/doc/vimeval.txt:1703-1707.
    let mut scope = Scope::new();
    scope.global.push((OxStr::from("x"), Typval::Number(7)));
    assert_eq!(value_in(b"g:", &mut scope), Typval::Dict(vec![(OxStr::from("x"), Typval::Number(7))]));
}

#[test]
fn bare_scope_preserves_identity() {
    // runtime/doc/vimeval.txt:1703-1707 and 1049-1053.
    assert_eq!(value(b"g: is g:"), Typval::Number(1));
}

#[test]
fn vim_scope_is_read_only() {
    // runtime/doc/vimeval.txt:E46; brief section 3.
    let error = Scope::new().set_scoped(ScopeKind::Vim, b"x", 4, Typval::Number(1)).unwrap_err();
    assert_eq!(error.code, "E46");
}

#[test]
fn environment_is_byte_keyed() {
    // runtime/doc/vimeval.txt:889; integration value supplied by Scope.
    let mut scope = Scope::new();
    scope.set_env(b"OX", Typval::String(OxStr(vec![0xff])));
    assert_eq!(value_in(b"$OX", &mut scope), Typval::String(OxStr(vec![0xff])));
}

#[test]
fn effective_option_prefers_local() {
    // runtime/doc/vimeval.txt:885; integration value supplied by Scope.
    let mut scope = Scope::new();
    scope.set_option(OptionScope::Global, b"nu", Typval::Number(1));
    scope.set_option(OptionScope::Local, b"nu", Typval::Number(2));
    assert_eq!(value_in(b"&nu", &mut scope), Typval::Number(2));
}

#[test]
fn register_lookup() {
    // runtime/doc/vimeval.txt:890; integration value supplied by Scope.
    let mut scope = Scope::new();
    scope.set_register(b"a", Typval::String(OxStr::from("text")));
    assert_eq!(value_in(b"@a", &mut scope), Typval::String(OxStr::from("text")));
}

#[test]
fn closure_captures_snapshot() {
    // runtime/doc/vimeval.txt:1627-1639.
    let expression = Parser::new(b"{x -> x + captured}").parse().unwrap();
    let mut scope = Scope::new();
    scope.set(b"captured", Typval::Number(4));
    let mut host = Host;
    let regex = Regex;
    let mut evaluator = Evaluator::new(&mut host, &regex);
    let closure = evaluator.eval(&expression, &mut scope).unwrap();
    scope.set(b"captured", Typval::Number(100));
    let mut call_scope = Scope::new();
    call_scope.set(b"f", closure);
    let call = Parser::new(b"f(6)").parse().unwrap();
    assert_eq!(evaluator.eval(&call, &mut call_scope).unwrap(), Typval::Number(10));
}

#[test]
fn lambda_parameter_shadows_captured_local() {
    // runtime/doc/vimeval.txt:1611-1639.
    let mut scope = Scope::new();
    scope.set(b"x", Typval::Number(99));
    assert_eq!(value_in(b"{x -> x}(1)", &mut scope), Typval::Number(1));
}

#[test]
fn regex_seam_receives_case_suffix() {
    // runtime/doc/vimeval.txt:1083-1094.
    assert_eq!(value(b"'AbC' =~? 'abc'"), Typval::Number(1));
}

#[test]
fn regex_negative_match() {
    // runtime/doc/vimeval.txt:1083-1094.
    assert_eq!(value(b"'abc' !~# 'z'"), Typval::Number(1));
}

#[test]
fn lexer_preserves_invalid_utf8_string() {
    // runtime/doc/vimeval.txt:1446-1448.
    let tokens = Lexer::new(b"\"\\xff\"").tokenize().unwrap();
    assert!(matches!(&tokens[0].kind, TokenKind::String(bytes) if bytes == &[0xff]));
}

#[test]
fn lexer_skips_line_continuation() {
    // runtime/doc/vimeval.txt:925-930.
    let tokens = Lexer::new(b"1\n\\ + 2").tokenize().unwrap();
    assert!(matches!(tokens[1].kind, TokenKind::Plus));
}

#[test]
fn lexer_skips_continuation_comment_line() {
    // runtime/doc/vimscript.txt:328-351.
    assert_eq!(value(b"1\n  \"\\ note\n  \\ + 2"), Typval::Number(3));
}

#[test]
fn parser_recursion_limit_is_typed() {
    // brief section 2; E1169 recursion guard.
    let error = Parser::new(b"(((1)))").with_max_nesting(2).parse().unwrap_err();
    assert_eq!(error.code, "E1169");
}

#[test]
fn evaluator_condition_uses_numeric_coercion() {
    // ox-types boundary and ex_eval.c:865-872.
    assert!(!Evaluator::<Host, Regex>::condition_number(&Typval::String(OxStr::from("0"))).unwrap());
}

#[test]
fn coalesce_uses_tv2bool_not_numeric_coercion() {
    // typval.c:4778-4817.
    assert_eq!(value(b"'0' ?? 9"), Typval::String(OxStr::from("0")));
}

#[test]
fn special_null_numeric_coercion() {
    // typval.c:4315-4316.
    assert!(!Evaluator::<Host, Regex>::condition_number(&Typval::Special(Special::Null)).unwrap());
}

#[test]
fn invalid_utf8_compared_case_sensitively_byte_wise() {
    // mb_stricmp fallback: invalid byte sequences are compared byte-for-byte,
    // never folded. Equal raw bytes are equal; differing bytes differ.
    assert_eq!(value(b"'\\xff' ==? '\\xff'"), Typval::Number(1));
    assert_eq!(value(b"'\\xff' ==? '\\xfe'"), Typval::Number(0));
    assert_eq!(value(b"'\\xc3' ==? '\\xc3'"), Typval::Number(1));
    // A lone lead byte that is not a complete sequence compares byte-wise.
    assert_eq!(value(b"'\\xc3' ==? '\\xc3\\xa4'"), Typval::Number(0));
}

#[test]
fn variadic_lambda_partial_binds_leading_args() {
    // test/old/testdir/test_lambda.vim:55-58. A `...` lambda accessed through a
    // partially-bound Funcref: bound args fill a:1.. before call-time extras.
    let expression = Parser::new(b"{... -> [a:1, a:2, a:3]}").parse().unwrap();
    let mut host = Host;
    let regex = Regex;
    let mut evaluator = Evaluator::new(&mut host, &regex);
    let closure = evaluator.eval(&expression, &mut Scope::new()).unwrap();
    let Typval::Partial(funcref) = closure else { panic!("expected a Partial") };
    let bound = Typval::Partial(Funcref {
        name: funcref.name,
        args: vec![Typval::String(OxStr::from("one")), Typval::String(OxStr::from("two"))],
        dict: None,
    });
    let mut scope = Scope::new();
    scope.set(b"cb", bound);
    let call = Parser::new(b"cb('three')").parse().unwrap();
    let mut call_host = Host;
    let registry = evaluator.closure_registry().clone();
    let mut callee = Evaluator::new(&mut call_host, &regex).with_closure_registry(registry);
    let result = callee.eval(&call, &mut scope).unwrap();
    assert_eq!(
        result,
        Typval::List(vec![
            Typval::String(OxStr::from("one")),
            Typval::String(OxStr::from("two")),
            Typval::String(OxStr::from("three")),
        ])
    );
}

#[test]
fn closure_resolves_across_evaluator_sharing_registry() {
    // Finding 3: a Partial stored in one scope resolves to its original closure
    // when invoked by a different Evaluator that shares the closure registry.
    let mut first_host = Host;
    let mut second_host = Host;
    let regex = Regex;
    let mut creator = Evaluator::new(&mut first_host, &regex);
    let mut caller = Evaluator::new(&mut second_host, &regex)
        .with_closure_registry(creator.closure_registry().clone());

    // Define `{x -> x + base}` and keep the Partial in a scope.
    let define = Parser::new(b"{x -> x + base}").parse().unwrap();
    let mut scope = Scope::new();
    scope.set(b"base", Typval::Number(40));
    let closure = creator.eval(&define, &mut scope).unwrap();
    let mut store = Scope::new();
    store.set(b"f", closure);
    let call = Parser::new(b"f(2)").parse().unwrap();

    // A second evaluator that shares the registry resolves the stored Partial
    // to the closure created by the first evaluator, including its capture.
    assert_eq!(caller.eval(&call, &mut store).unwrap(), Typval::Number(42));
}

#[test]
fn closure_from_isolated_evaluator_is_not_callable() {
    // Two independent registries each create a <lambda>0. A Partial from the
    // first registry must not resolve to the second registry's closure with the
    // same local index; instead it returns E117.
    let mut first_host = Host;
    let mut second_host = Host;
    let regex = Regex;
    let mut creator = Evaluator::new(&mut first_host, &regex);
    let mut caller = Evaluator::new(&mut second_host, &regex);

    let define_a = Parser::new(b"{x -> x + 1}").parse().unwrap();
    let define_b = Parser::new(b"{x -> x * 2}").parse().unwrap();
    let closure_a = creator.eval(&define_a, &mut Scope::new()).unwrap();
    // Ensure caller's registry also has a closure at index 0.
    let _closure_b = caller.eval(&define_b, &mut Scope::new()).unwrap();
    let mut store = Scope::new();
    store.set(b"f", closure_a);
    let call = Parser::new(b"f(5)").parse().unwrap();
    assert_eq!(caller.eval(&call, &mut store).unwrap_err().code, "E117");
}
