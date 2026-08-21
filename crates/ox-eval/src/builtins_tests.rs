//! Behavioral cases drawn from `runtime/doc/builtin.txt` and oldtests.

use std::cell::Cell;

use ox_types::{OxStr, Special, Typval};

use crate::builtins::{Builtins, BUILTINS};
use crate::error::EvalErrorKind;
use crate::eval::{BuiltinHost, Evaluator, NoRegex, RegexEngine};
use crate::parser::Parser;
use crate::scope::Scope;

fn text(value: &str) -> Typval { Typval::String(OxStr::from(value)) }
fn number(value: i64) -> Typval { Typval::Number(value) }
fn list(values: &[i64]) -> Typval { Typval::List(values.iter().copied().map(Typval::Number).collect()) }
fn call(name: &str, args: Vec<Typval>) -> crate::Result<Typval> {
    let mut builtins = Builtins::without_regex();
    builtins.call(&OxStr::from(name), args, &mut Scope::new())
}

// `runtime/doc/builtin.txt` function sections and
// `test/old/testdir/test_functions.vim` are the oracle for these table cases.
macro_rules! case {
    ($name:ident, $function:literal, [$($arg:expr),* $(,)?], $expected:expr) => {
        #[doc = concat!("Oracle: `runtime/doc/builtin.txt` `", $function, "()` section.")]
        #[test]
        fn $name() {
            assert_eq!(call($function, vec![$($arg),*]).unwrap(), $expected);
        }
    };
}

case!(abs_positive, "abs", [number(7)], number(7));
case!(abs_negative, "abs", [number(-7)], number(7));
case!(abs_float, "abs", [Typval::Float(-1.5)], Typval::Float(1.5));
case!(bit_and, "and", [number(6), number(3)], number(2));
case!(bit_or, "or", [number(4), number(3)], number(7));
case!(bit_xor, "xor", [number(7), number(3)], number(4));
case!(ceil_fraction, "ceil", [Typval::Float(1.2)], Typval::Float(2.0));
case!(floor_fraction, "floor", [Typval::Float(1.8)], Typval::Float(1.0));
case!(sqrt_square, "sqrt", [number(9)], Typval::Float(3.0));
case!(pow_integer_inputs, "pow", [number(2), number(3)], Typval::Float(8.0));
case!(float2nr_positive, "float2nr", [Typval::Float(3.9)], number(3));
case!(float2nr_negative, "float2nr", [Typval::Float(-3.9)], number(-3));
case!(trunc_positive, "trunc", [Typval::Float(4.8)], number(4));
case!(trunc_negative, "trunc", [Typval::Float(-4.8)], number(-4));
case!(empty_zero, "empty", [number(0)], number(1));
case!(empty_nonzero, "empty", [number(1)], number(0));
case!(empty_string, "empty", [text("")], number(1));
case!(empty_nonempty_string, "empty", [text("x")], number(0));
case!(empty_list, "empty", [list(&[])], number(1));
case!(empty_dict, "empty", [Typval::Dict(vec![])], number(1));
case!(len_string_bytes, "len", [text("abc")], number(3));
case!(len_list, "len", [list(&[1, 2, 3])], number(3));
case!(len_dict, "len", [Typval::Dict(vec![(OxStr::from("a"), number(1))])], number(1));
case!(strlen_unicode_bytes, "strlen", [text("é")], number(2));
case!(strcharlen_unicode, "strcharlen", [text("é")], number(1));
case!(strchars_ascii, "strchars", [text("abc")], number(3));
case!(toupper_ascii, "toupper", [text("aBc")], text("ABC"));
case!(tolower_ascii, "tolower", [text("aBc")], text("abc"));
case!(trim_default, "trim", [text("  a \n")], text("a"));
case!(trim_mask, "trim", [text("xxabcxx"), text("x")], text("abc"));
case!(trim_left, "trim", [text("xxabcxx"), text("x"), number(1)], text("abcxx"));
case!(trim_right, "trim", [text("xxabcxx"), text("x"), number(2)], text("xxabc"));
case!(join_default, "join", [Typval::List(vec![text("a"), text("b")])], text("a b"));
case!(join_custom, "join", [Typval::List(vec![text("a"), text("b")]), text(",")], text("a,b"));
case!(repeat_string, "repeat", [text("ab"), number(3)], text("ababab"));
case!(repeat_string_zero, "repeat", [text("ab"), number(0)], text(""));
case!(repeat_list, "repeat", [list(&[1, 2]), number(2)], list(&[1, 2, 1, 2]));
case!(reverse_string, "reverse", [text("abc")], text("cba"));
case!(reverse_unicode, "reverse", [text("aé")], text("éa"));
case!(reverse_list, "reverse", [list(&[1, 2, 3])], list(&[3, 2, 1]));
case!(stridx_found, "stridx", [text("abcdef"), text("cd")], number(2));
case!(stridx_missing, "stridx", [text("abcdef"), text("xy")], number(-1));
case!(stridx_empty, "stridx", [text("abc"), text("")], number(0));
case!(strridx_found, "strridx", [text("ababa"), text("ba")], number(3));
case!(strridx_bounded, "strridx", [text("a,b,c"), text(","), number(2)], number(1));
case!(strridx_missing, "strridx", [text("ababa"), text("x")], number(-1));
case!(strpart_middle, "strpart", [text("abcdef"), number(2), number(3)], text("cde"));
case!(strpart_past_end, "strpart", [text("abc"), number(9), number(2)], text(""));
case!(escape_chars, "escape", [text("a.b"), text(".")], text("a\\.b"));
case!(add_list, "add", [list(&[1, 2]), number(3)], list(&[1, 2, 3]));
case!(add_blob, "add", [Typval::Blob(vec![1, 2]), number(3)], Typval::Blob(vec![1, 2, 3]));
case!(copy_number, "copy", [number(4)], number(4));
case!(copy_list, "copy", [list(&[1, 2])], list(&[1, 2]));
case!(deepcopy_nested, "deepcopy", [Typval::List(vec![list(&[1])])], Typval::List(vec![list(&[1])]));
case!(count_list, "count", [list(&[1, 2, 1]), number(1)], number(2));
case!(count_string, "count", [text("aaaa"), text("aa")], number(2));
case!(get_list, "get", [list(&[4, 5]), number(1)], number(5));
case!(get_list_negative, "get", [list(&[4, 5]), number(-1)], number(5));
case!(get_list_default, "get", [list(&[4]), number(9), number(7)], number(7));
case!(get_blob, "get", [Typval::Blob(vec![8]), number(0)], number(8));
case!(get_dict, "get", [Typval::Dict(vec![(OxStr::from("k"), number(9))]), text("k")], number(9));
case!(has_key_true, "has_key", [Typval::Dict(vec![(OxStr::from("k"), number(1))]), text("k")], number(1));
case!(has_key_false, "has_key", [Typval::Dict(vec![]), text("k")], number(0));
case!(index_found, "index", [list(&[4, 5, 4]), number(5)], number(1));
case!(index_missing, "index", [list(&[4, 5]), number(9)], number(-1));
case!(insert_front, "insert", [list(&[2, 3]), number(1)], list(&[1, 2, 3]));
case!(insert_middle, "insert", [list(&[1, 3]), number(2), number(1)], list(&[1, 2, 3]));
case!(keys_dict, "keys", [Typval::Dict(vec![(OxStr::from("a"), number(1)), (OxStr::from("b"), number(2))])], Typval::List(vec![text("a"), text("b")]));
case!(values_dict, "values", [Typval::Dict(vec![(OxStr::from("a"), number(1)), (OxStr::from("b"), number(2))])], list(&[1, 2]));
case!(items_dict, "items", [Typval::Dict(vec![(OxStr::from("a"), number(1))])], Typval::List(vec![Typval::List(vec![text("a"), number(1)])]));
case!(max_list, "max", [list(&[1, 9, 2])], number(9));
case!(min_list, "min", [list(&[1, -2, 9])], number(-2));
case!(max_empty, "max", [list(&[])], number(0));
case!(range_single, "range", [number(4)], list(&[0, 1, 2, 3]));
case!(range_bounds, "range", [number(2), number(4)], list(&[2, 3, 4]));
case!(range_stride, "range", [number(2), number(8), number(3)], list(&[2, 5, 8]));
case!(range_negative_stride, "range", [number(3), number(1), number(-1)], list(&[3, 2, 1]));
case!(remove_list_item, "remove", [list(&[1, 2, 3]), number(1)], number(2));
case!(remove_list_range, "remove", [list(&[1, 2, 3, 4]), number(1), number(2)], list(&[2, 3]));
case!(remove_dict_item, "remove", [Typval::Dict(vec![(OxStr::from("a"), number(3))]), text("a")], number(3));
case!(sort_numbers, "sort", [list(&[3, 1, 2]), text("n")], list(&[1, 2, 3]));
case!(sort_stable_equal, "sort", [list(&[2, 1, 2]), text("n")], list(&[1, 2, 2]));
case!(uniq_adjacent, "uniq", [list(&[1, 1, 2, 1])], list(&[1, 2, 1]));
case!(extend_lists, "extend", [list(&[1]), list(&[2, 3])], list(&[1, 2, 3]));
case!(flatten_one, "flatten", [Typval::List(vec![number(1), list(&[2, 3])])], list(&[1, 2, 3]));
case!(flatten_depth_zero, "flatten", [Typval::List(vec![list(&[1])]), number(0)], Typval::List(vec![list(&[1])]));
case!(blob2list_basic, "blob2list", [Typval::Blob(vec![0, 255])], list(&[0, 255]));
case!(list2blob_basic, "list2blob", [list(&[0, 255])], Typval::Blob(vec![0, 255]));
case!(list2str_bytes, "list2str", [list(&[65, 66])], text("AB"));
case!(str2list_bytes, "str2list", [text("AB")], list(&[65, 66]));
case!(char2nr_ascii, "char2nr", [text("A")], number(65));
case!(char2nr_empty, "char2nr", [text("")], number(0));
case!(nr2char_ascii, "nr2char", [number(65)], text("A"));
case!(nr2char_unicode, "nr2char", [number(0xE9)], text("é"));
case!(str2nr_decimal, "str2nr", [text("123")], number(123));
case!(str2nr_negative, "str2nr", [text("-42")], number(-42));
case!(str2nr_hex_explicit, "str2nr", [text("0xff"), number(16)], number(255));
case!(str2nr_binary_explicit, "str2nr", [text("0b101"), number(2)], number(5));
case!(str2nr_octal_explicit, "str2nr", [text("0o17"), number(8)], number(15));
case!(str2nr_default_is_decimal, "str2nr", [text("0xff")], number(0));
case!(str2float_basic, "str2float", [text("1.25")], Typval::Float(1.25));
case!(type_number, "type", [number(1)], number(1));
case!(type_string, "type", [text("x")], number(2));
case!(type_list, "type", [list(&[])], number(4));
case!(type_dict, "type", [Typval::Dict(vec![])], number(5));
case!(type_float, "type", [Typval::Float(1.0)], number(6));
case!(type_bool, "type", [Typval::Bool(true)], number(7));
case!(type_null, "type", [Typval::Special(Special::Null)], number(8));
case!(string_number, "string", [number(12)], text("12"));
case!(string_text_quotes, "string", [text("a")], text("'a'"));
case!(string_bool, "string", [Typval::Bool(true)], text("v:true"));
case!(json_encode_null, "json_encode", [Typval::Special(Special::Null)], text("null"));
case!(json_encode_bool, "json_encode", [Typval::Bool(true)], text("true"));
case!(json_encode_list, "json_encode", [list(&[1, 2])], text("[1,2]"));
case!(json_encode_dict_order, "json_encode", [Typval::Dict(vec![(OxStr::from("b"), number(2)), (OxStr::from("a"), number(1))])], text("{\"b\":2,\"a\":1}"));
case!(json_decode_null, "json_decode", [text("null")], Typval::Special(Special::Null));
case!(json_decode_bool, "json_decode", [text("false")], Typval::Bool(false));
case!(json_decode_list, "json_decode", [text("[1,2]")], list(&[1, 2]));

#[test]
fn generated_inventory_is_complete_sorted_and_unique() {
    // `src/nvim/eval.lua:37-14147`: independent source parse versus codegen.
    let root = std::env::var("OXVIM_REF_ROOT").unwrap();
    let source = std::fs::read_to_string(format!("{root}/src/nvim/eval.lua")).unwrap();
    let source_names: std::collections::BTreeSet<&str> = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("name = '").and_then(|value| value.strip_suffix("',")))
        .collect();
    let generated_names: std::collections::BTreeSet<&str> = BUILTINS.iter().map(|spec| spec.name).collect();
    assert_eq!(generated_names, source_names);
    assert_eq!(BUILTINS.len(), generated_names.len());
    assert!(BUILTINS.windows(2).all(|pair| pair[0].name < pair[1].name));
}

#[test]
fn implemented_specs_match_upstream_args_and_method_flag() {
    // `src/nvim/eval.lua:37-14147`: all public overloads are folded by name.
    let root = std::env::var("OXVIM_REF_ROOT").unwrap();
    let source = std::fs::read_to_string(format!("{root}/src/nvim/eval.lua")).unwrap();
    let parsed = parse_source_specs(&source);
    assert_eq!(parsed.len(), BUILTINS.len());
    for spec in BUILTINS {
        assert_eq!(parsed.get(spec.name), Some(&(spec.min_args, spec.max_args, spec.method)), "{}", spec.name);
    }
}

fn parse_source_specs(source: &str) -> std::collections::BTreeMap<String, (usize, Option<usize>, bool)> {
    let mut result = std::collections::BTreeMap::new();
    let mut in_entry = false;
    let mut name = None;
    let mut args = (0, Some(0));
    let mut method = false;
    for line in source.lines() {
        if !in_entry && line.starts_with("  ") && !line.starts_with("    ") && line.ends_with(" = {") {
            in_entry = true; name = None; args = (0, Some(0)); method = false; continue;
        }
        if !in_entry { continue; }
        if line == "  }," {
            if let Some(name) = name.take() {
                result.entry(name).and_modify(|entry: &mut (usize, Option<usize>, bool)| {
                    entry.0 = entry.0.min(args.0);
                    entry.1 = match (entry.1, args.1) { (Some(left), Some(right)) => Some(left.max(right)), _ => None };
                    entry.2 |= method;
                }).or_insert((args.0, args.1, method));
            }
            in_entry = false; continue;
        }
        let field = line.strip_prefix("    ").filter(|value| !value.starts_with(' '));
        let Some(field) = field else { continue };
        if let Some(value) = field.strip_prefix("name = '").and_then(|value| value.strip_suffix("',")) { name = Some(value.to_owned()); }
        if let Some(value) = field.strip_prefix("base = " ).and_then(|value| value.strip_suffix(',')) { method = value.parse::<usize>().is_ok_and(|value| value > 0); }
        if let Some(value) = field.strip_prefix("args = " ).and_then(|value| value.strip_suffix(',')) {
            if let Ok(count) = value.parse::<usize>() { args = (count, Some(count)); }
            else if let Some(body) = value.strip_prefix("{ " ).and_then(|value| value.strip_suffix(" }")) {
                let numbers: Vec<usize> = body.split(',').filter_map(|value| value.trim().parse().ok()).collect();
                args = match numbers.as_slice() { [only] => (*only, None), [minimum, maximum, ..] => (*minimum, Some(*maximum)), _ => args };
            }
        }
    }
    result
}

#[test]
fn arity_errors_are_vim_compatible() {
    assert_eq!(call("abs", vec![]).unwrap_err().code, "E119");
    assert_eq!(call("abs", vec![number(1), number(2)]).unwrap_err().code, "E118");
}

#[test]
fn non_pure_builtin_is_typed_not_implemented() {
    let error = call("append", vec![number(1), text("x")]).unwrap_err();
    assert_eq!(error.kind, EvalErrorKind::NotImplemented(OxStr::from("append")));
    let wrong_arity = call("api_info", vec![number(1)]).unwrap_err();
    assert_eq!(wrong_arity.kind, EvalErrorKind::NotImplemented(OxStr::from("api_info")));
}

#[test]
fn unknown_builtin_is_typed_not_implemented() {
    let error = call("definitely_missing", vec![]).unwrap_err();
    assert_eq!(error.kind, EvalErrorKind::NotImplemented(OxStr::from("definitely_missing")));
}

#[test]
fn method_call_injects_receiver_for_flagged_builtin() {
    // `runtime/doc/builtin.txt: add()` method form.
    let expression = Parser::new(b"[1, 2]->add(3)").parse().unwrap();
    let regex = NoRegex;
    let mut builtins = Builtins::without_regex();
    let result = Evaluator::new(&mut builtins, &regex).eval(&expression, &mut Scope::new()).unwrap();
    assert_eq!(result, list(&[1, 2, 3]));
}

#[test]
fn chained_method_calls_preserve_receiver_order() {
    let expression = Parser::new(b"[1]->add(2)->add(3)").parse().unwrap();
    let regex = NoRegex;
    let mut builtins = Builtins::without_regex();
    let result = Evaluator::new(&mut builtins, &regex).eval(&expression, &mut Scope::new()).unwrap();
    assert_eq!(result, list(&[1, 2, 3]));
}

#[test]
fn map_string_callback_uses_v_val_and_v_key() {
    // `test/old/testdir/test_listdict.vim`: expression callback form.
    let result = call("map", vec![list(&[2, 3]), text("v:val * 2 + v:key")]).unwrap();
    assert_eq!(result, list(&[4, 7]));
}

#[test]
fn filter_string_callback_uses_v_val() {
    let result = call("filter", vec![list(&[0, 2, 0, 3]), text("v:val")]).unwrap();
    assert_eq!(result, list(&[2, 3]));
}

#[test]
fn nr2char_invalid_scalar_has_error_code() {
    // `test/old/testdir/test_functions.vim`: invalid Unicode scalar.
    assert_eq!(call("nr2char", vec![number(0x11_0000)]).unwrap_err().code, "E1280");
}

#[test]
fn range_zero_stride_has_error_code() {
    assert_eq!(call("range", vec![number(1), number(3), number(0)]).unwrap_err().code, "E726");
}

#[test]
fn list_as_number_has_error_code() {
    assert_eq!(call("abs", vec![list(&[])]).unwrap_err().code, "E745");
}

#[test]
fn sort_expression_comparator_receives_both_values() {
    // `runtime/doc/builtin.txt` `sort()` comparator form.
    let result = call("sort", vec![list(&[3, 1, 2]), text("v:key - v:val")]).unwrap();
    assert_eq!(result, list(&[1, 2, 3]));
}

#[test]
fn sort_default_comparator_uses_strings() {
    // `test/old/testdir/test_functions.vim` `Test_sort_numbers()` and
    // the default comparator in `item_compare` (typval.c:1192-1310): values
    // are converted to strings, so `sort([2, 10])` is `[10, 2]`.
    let result = call("sort", vec![list(&[2, 10])]).unwrap();
    assert_eq!(result, list(&[10, 2]));
    // A String compared against a non-String sorts as a leading quote.
    let mixed = call("sort", vec![Typval::List(vec![number(0), text("x")])]).unwrap();
    assert_eq!(mixed, Typval::List(vec![text("x"), number(0)]));
}

#[test]
fn sort_numeric_mode() {
    // `n` mode: each value is stringified and its leading number parsed, so
    // numeric values order numerically while strings order as 0.
    let result = call("sort", vec![list(&[2, 10, 1]), text("n")]).unwrap();
    assert_eq!(result, list(&[1, 2, 10]));
    let mixed = call("sort", vec![Typval::List(vec![text("a"), number(5), number(3)]), text("n")]).unwrap();
    assert_eq!(mixed, Typval::List(vec![text("a"), number(3), number(5)]));
}

#[test]
fn sort_integer_mode() {
    // `N` mode: integer comparison via `tv_get_number`.
    let result = call("sort", vec![list(&[10, 2, -1]), text("N")]).unwrap();
    assert_eq!(result, list(&[-1, 2, 10]));
}

#[test]
fn sort_float_mode() {
    // `f` mode: float comparison via `tv_get_float`.
    let values = Typval::List(vec![Typval::Float(2.5), Typval::Float(1.5), Typval::Float(10.0)]);
    let result = call("sort", vec![values, text("f")]).unwrap();
    assert_eq!(result, Typval::List(vec![Typval::Float(1.5), Typval::Float(2.5), Typval::Float(10.0)]));
}

#[test]
fn sort_ignore_case_mode() {
    // `i` mode: case-insensitive string sort.
    let result = call("sort", vec![Typval::List(vec![text("banana"), text("Apple"), text("cherry")]), text("i")]).unwrap();
    assert_eq!(result, Typval::List(vec![text("Apple"), text("banana"), text("cherry")]));
}

#[test]
fn sort_locale_mode_is_byte_wise_fallback() {
    // `l` mode is documented to sort by the locale of the running system; this
    // port uses a byte-wise fallback for the C-locale `strcoll` comparison, so
    // it matches the default ordering here.
    let result = call("sort", vec![Typval::List(vec![text("banana"), text("Apple")]), text("l")]).unwrap();
    assert_eq!(result, Typval::List(vec![text("Apple"), text("banana")]));
    let numbers = call("sort", vec![list(&[2, 10]), text("l")]).unwrap();
    assert_eq!(numbers, list(&[10, 2]));
}

#[test]
fn str2nr_base_zero_is_rejected() {
    // `test/old/testdir/test_functions.vim`: only 2/8/10/16 are valid bases;
    // base 0 is rejected with E474 (f_str2nr, strings.c:2593-2598).
    let error = call("str2nr", vec![text("0xff"), number(0)]).unwrap_err();
    assert_eq!(error.code, "E474");
    let error = call("str2nr", vec![text("123"), number(1)]).unwrap_err();
    assert_eq!(error.code, "E474");
}

#[test]
fn str2nr_allows_whitespace_after_sign() {
    // `test/old/testdir/test_functions.vim` `Test_str2nr()`: whitespace after
    // the sign is skipped.
    assert_eq!(call("str2nr", vec![text("+ 1")]).unwrap(), number(1));
    assert_eq!(call("str2nr", vec![text("- 1")]).unwrap(), number(-1));
    assert_eq!(call("str2nr", vec![text(" - 42 ")]).unwrap(), number(-42));
    assert_eq!(call("str2nr", vec![text("+ 10"), number(16)]).unwrap(), number(16));
}

#[test]
fn str2nr_prefix_rules_follow_force_mode() {
    // `test/old/testdir/test_functions.vim` `Test_str2nr()`: with an explicit
    // base the "0b"/"0o"/"0x" prefix is consumed (STR2NR_FORCE), and text
    // after the parsed digits is ignored.
    assert_eq!(call("str2nr", vec![text("0101"), number(8)]).unwrap(), number(65));
    assert_eq!(call("str2nr", vec![text("0o0101"), number(8)]).unwrap(), number(65));
    assert_eq!(call("str2nr", vec![text("-0b101"), number(2)]).unwrap(), number(-5));
    assert_eq!(call("str2nr", vec![text("0Xabcdef"), number(16)]).unwrap(), number(11259375));
    assert_eq!(call("str2nr", vec![text("12"), number(2)]).unwrap(), number(1));
    assert_eq!(call("str2nr", vec![text("18"), number(8)]).unwrap(), number(1));
    assert_eq!(call("str2nr", vec![text("1g"), number(16)]).unwrap(), number(1));
}

#[test]
fn nested_comparison_propagates_recursion_error() {
    // `test/old/testdir/test_listdict.vim`: recursive compare guard.
    let mut value = number(1);
    for _ in 0..101 { value = Typval::List(vec![value]); }
    assert_eq!(call("count", vec![Typval::List(vec![value.clone()]), value]).unwrap_err().code, "E724");
}

struct LiteralRegex { calls: Cell<usize> }
impl RegexEngine for LiteralRegex {
    fn is_match(&self, text: &OxStr, pattern: &OxStr, _ignore_case: bool) -> crate::Result<bool> {
        self.calls.set(self.calls.get() + 1);
        Ok(text.as_bytes().windows(pattern.as_bytes().len()).any(|window| window == pattern.as_bytes()))
    }
    fn split(&self, text: &OxStr, pattern: &OxStr, keep_empty: bool) -> crate::Result<Vec<OxStr>> {
        self.calls.set(self.calls.get() + 1);
        let source = text.to_string_lossy();
        let pattern = pattern.to_string_lossy();
        Ok(source.split(pattern.as_ref()).filter(|part| keep_empty || !part.is_empty()).map(OxStr::from).collect())
    }
    fn find(&self, text: &OxStr, pattern: &OxStr, start: usize) -> crate::Result<Option<(usize, usize)>> {
        self.calls.set(self.calls.get() + 1);
        Ok(text.as_bytes().get(start..).and_then(|tail| tail.windows(pattern.as_bytes().len()).position(|window| window == pattern.as_bytes())).map(|position| (start + position, start + position + pattern.as_bytes().len())))
    }
    fn substitute(&self, text: &OxStr, pattern: &OxStr, replacement: &OxStr, _flags: &OxStr) -> crate::Result<OxStr> {
        self.calls.set(self.calls.get() + 1);
        Ok(OxStr(text.to_string_lossy().replacen(pattern.to_string_lossy().as_ref(), replacement.to_string_lossy().as_ref(), 1).into_bytes()))
    }
}

#[test]
fn split_uses_regex_engine_seam() {
    let regex = LiteralRegex { calls: Cell::new(0) };
    let mut builtins = Builtins::new(&regex);
    let result = builtins.call(&OxStr::from("split"), vec![text("a,b"), text(",")], &mut Scope::new()).unwrap();
    assert_eq!(result, Typval::List(vec![text("a"), text("b")]));
    assert_eq!(regex.calls.get(), 1);
}

#[test]
fn match_family_uses_regex_engine_seam() {
    let regex = LiteralRegex { calls: Cell::new(0) };
    let mut builtins = Builtins::new(&regex);
    assert_eq!(builtins.call(&OxStr::from("match"), vec![text("abc"), text("b")], &mut Scope::new()).unwrap(), number(1));
    assert_eq!(builtins.call(&OxStr::from("matchend"), vec![text("abc"), text("b")], &mut Scope::new()).unwrap(), number(2));
    assert_eq!(builtins.call(&OxStr::from("matchstr"), vec![text("abc"), text("b")], &mut Scope::new()).unwrap(), text("b"));
    assert_eq!(regex.calls.get(), 3);
}

#[test]
fn match_family_honors_count_and_list_inputs() {
    let regex = LiteralRegex { calls: Cell::new(0) };
    let mut builtins = Builtins::new(&regex);
    assert_eq!(builtins.call(&OxStr::from("match"), vec![text("ababa"), text("ba"), number(0), number(2)], &mut Scope::new()).unwrap(), number(3));
    assert_eq!(builtins.call(&OxStr::from("match"), vec![Typval::List(vec![text("x"), text("ab"), text("ab")]), text("b"), number(0), number(2)], &mut Scope::new()).unwrap(), number(2));
}

#[test]
fn substitute_uses_regex_engine_seam() {
    let regex = LiteralRegex { calls: Cell::new(0) };
    let mut builtins = Builtins::new(&regex);
    let result = builtins.call(&OxStr::from("substitute"), vec![text("aba"), text("b"), text("x"), text("")], &mut Scope::new()).unwrap();
    assert_eq!(result, text("axa"));
    assert_eq!(regex.calls.get(), 1);
}

#[test]
fn regex_builtin_without_engine_is_typed_error() {
    assert_eq!(call("split", vec![text("a b")]).unwrap_err().code, "E54");
}
