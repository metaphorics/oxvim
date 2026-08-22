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
fn list(values: &[i64]) -> Typval { Typval::list(values.iter().copied().map(Typval::Number).collect()) }
fn funcref(name: &str) -> Typval { Typval::Funcref(ox_types::Funcref { name: OxStr::from(name), args: vec![], dict: None, registry: None }) }
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
case!(empty_dict, "empty", [Typval::dict(vec![])], number(1));
case!(len_string_bytes, "len", [text("abc")], number(3));
case!(len_list, "len", [list(&[1, 2, 3])], number(3));
case!(len_dict, "len", [Typval::dict(vec![(OxStr::from("a"), number(1))])], number(1));
case!(strlen_unicode_bytes, "strlen", [text("é")], number(2));
case!(strcharlen_unicode, "strcharlen", [text("é")], number(1));
case!(strchars_ascii, "strchars", [text("abc")], number(3));
case!(toupper_ascii, "toupper", [text("aBc")], text("ABC"));
case!(tolower_ascii, "tolower", [text("aBc")], text("abc"));
case!(trim_default, "trim", [text("  a \n")], text("a"));
case!(trim_mask, "trim", [text("xxabcxx"), text("x")], text("abc"));
case!(trim_left, "trim", [text("xxabcxx"), text("x"), number(1)], text("abcxx"));
case!(trim_right, "trim", [text("xxabcxx"), text("x"), number(2)], text("xxabc"));

#[test]
fn setenv_sets_numeric_value_and_null_unsets() {
    const NAME: &str = "OXVIM_TEST_EVAL_SETENV";
    assert_eq!(call("setenv", vec![text(NAME), number(123)]).unwrap(), number(0));
    assert_eq!(std::env::var_os(NAME).as_deref(), Some(std::ffi::OsStr::new("123")));
    assert_eq!(call("setenv", vec![text(NAME), Typval::Special(Special::Null)]).unwrap(), number(0));
    assert_eq!(std::env::var_os(NAME), None);
}

case!(join_default, "join", [Typval::list(vec![text("a"), text("b")])], text("a b"));
case!(join_custom, "join", [Typval::list(vec![text("a"), text("b")]), text(",")], text("a,b"));
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
case!(deepcopy_nested, "deepcopy", [Typval::list(vec![list(&[1])])], Typval::list(vec![list(&[1])]));
case!(count_list, "count", [list(&[1, 2, 1]), number(1)], number(2));
case!(count_string, "count", [text("aaaa"), text("aa")], number(2));
case!(get_list, "get", [list(&[4, 5]), number(1)], number(5));
case!(get_list_negative, "get", [list(&[4, 5]), number(-1)], number(5));
case!(get_list_default, "get", [list(&[4]), number(9), number(7)], number(7));
case!(get_blob, "get", [Typval::Blob(vec![8]), number(0)], number(8));
case!(get_dict, "get", [Typval::dict(vec![(OxStr::from("k"), number(9))]), text("k")], number(9));
case!(has_key_true, "has_key", [Typval::dict(vec![(OxStr::from("k"), number(1))]), text("k")], number(1));
case!(has_key_false, "has_key", [Typval::dict(vec![]), text("k")], number(0));
// `runtime/doc/builtin.txt` `has()`: the `nvim-X.Y[.Z]` probe compares
// against the version this build targets (0.13.0, matching
// `ox_rpc::metadata::API_LEVEL = 15`); anything beyond that target is 0.
case!(has_nvim_current_minor, "has", [text("nvim-0.13")], number(1));
case!(has_nvim_current_patch, "has", [text("nvim-0.13.0")], number(1));
case!(has_nvim_older_release, "has", [text("nvim-0.10")], number(1));
case!(has_nvim_newer_minor, "has", [text("nvim-0.14")], number(0));
case!(has_nvim_newer_patch, "has", [text("nvim-0.13.1")], number(0));
case!(has_nvim_newer_major, "has", [text("nvim-1.0")], number(0));
case!(has_nvim_extra_component, "has", [text("nvim-0.13.0.1")], number(0));
case!(has_nvim_not_a_version, "has", [text("nvim-dev")], number(0));
case!(has_multi_byte, "has", [text("multi_byte")], number(1));
case!(has_unknown_feature, "has", [text("bogus-feature")], number(0));

/// `has("unix")`/`has("win32")`/`has("macunix")` mirror the target family the
/// binary was compiled for (`f_has` in eval/funcs.c).
#[test]
fn has_platform_matches_target_family() {
    assert_eq!(call("has", vec![text("unix")]).unwrap(), number(i64::from(cfg!(unix))));
    assert_eq!(call("has", vec![text("win32")]).unwrap(), number(i64::from(cfg!(windows))));
    assert_eq!(call("has", vec![text("macunix")]).unwrap(), number(i64::from(cfg!(target_os = "macos"))));
}

/// `has()` rejects a zero-argument call with E119 per its eval.lua arity row.
#[test]
fn has_rejects_missing_argument() {
    let error = call("has", vec![]).unwrap_err();
    assert_eq!(error.kind, EvalErrorKind::Vim);
    assert_eq!(error.code, "E119");
}
case!(index_found, "index", [list(&[4, 5, 4]), number(5)], number(1));
case!(index_missing, "index", [list(&[4, 5]), number(9)], number(-1));
case!(insert_front, "insert", [list(&[2, 3]), number(1)], list(&[1, 2, 3]));
case!(insert_middle, "insert", [list(&[1, 3]), number(2), number(1)], list(&[1, 2, 3]));
case!(keys_dict, "keys", [Typval::dict(vec![(OxStr::from("a"), number(1)), (OxStr::from("b"), number(2))])], Typval::list(vec![text("a"), text("b")]));
case!(values_dict, "values", [Typval::dict(vec![(OxStr::from("a"), number(1)), (OxStr::from("b"), number(2))])], list(&[1, 2]));
case!(items_dict, "items", [Typval::dict(vec![(OxStr::from("a"), number(1))])], Typval::list(vec![Typval::list(vec![text("a"), number(1)])]));
case!(max_list, "max", [list(&[1, 9, 2])], number(9));
case!(min_list, "min", [list(&[1, -2, 9])], number(-2));
case!(max_empty, "max", [list(&[])], number(0));
case!(range_single, "range", [number(4)], list(&[0, 1, 2, 3]));
case!(range_bounds, "range", [number(2), number(4)], list(&[2, 3, 4]));
case!(range_stride, "range", [number(2), number(8), number(3)], list(&[2, 5, 8]));
case!(range_negative_stride, "range", [number(3), number(1), number(-1)], list(&[3, 2, 1]));
case!(remove_list_item, "remove", [list(&[1, 2, 3]), number(1)], number(2));
case!(remove_list_range, "remove", [list(&[1, 2, 3, 4]), number(1), number(2)], list(&[2, 3]));
case!(remove_dict_item, "remove", [Typval::dict(vec![(OxStr::from("a"), number(3))]), text("a")], number(3));
case!(sort_numbers, "sort", [list(&[3, 1, 2]), text("n")], list(&[1, 2, 3]));
case!(sort_stable_equal, "sort", [list(&[2, 1, 2]), text("n")], list(&[1, 2, 2]));
case!(uniq_adjacent, "uniq", [list(&[1, 1, 2, 1])], list(&[1, 2, 1]));
case!(extend_lists, "extend", [list(&[1]), list(&[2, 3])], list(&[1, 2, 3]));
case!(flatten_one, "flatten", [Typval::list(vec![number(1), list(&[2, 3])])], list(&[1, 2, 3]));
case!(flatten_depth_zero, "flatten", [Typval::list(vec![list(&[1])]), number(0)], Typval::list(vec![list(&[1])]));
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
case!(type_dict, "type", [Typval::dict(vec![])], number(5));
case!(type_float, "type", [Typval::Float(1.0)], number(6));
case!(type_bool, "type", [Typval::Bool(true)], number(7));
case!(type_null, "type", [Typval::Special(Special::Null)], number(8));
case!(string_number, "string", [number(12)], text("12"));
case!(string_text_quotes, "string", [text("a")], text("'a'"));
case!(string_bool, "string", [Typval::Bool(true)], text("v:true"));
case!(json_encode_null, "json_encode", [Typval::Special(Special::Null)], text("null"));
case!(json_encode_bool, "json_encode", [Typval::Bool(true)], text("true"));
case!(json_encode_list, "json_encode", [list(&[1, 2])], text("[1,2]"));
case!(json_encode_dict_order, "json_encode", [Typval::dict(vec![(OxStr::from("b"), number(2)), (OxStr::from("a"), number(1))])], text("{\"b\":2,\"a\":1}"));
case!(json_decode_null, "json_decode", [text("null")], Typval::Special(Special::Null));
case!(json_decode_bool, "json_decode", [text("false")], Typval::Bool(false));
case!(json_decode_list, "json_decode", [text("[1,2]")], list(&[1, 2]));

#[test]
fn generated_inventory_is_complete_sorted_and_unique() {
    // `src/nvim/eval.lua:37-14147`: independent source parse versus codegen.
    let source_path = std::env::var("OXVIM_REF_ROOT")
        .map(|root| format!("{root}/src/nvim/eval.lua"))
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../codegen/upstream/eval.lua").to_owned());
    let source = std::fs::read_to_string(source_path).unwrap();
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
    let source_path = std::env::var("OXVIM_REF_ROOT")
        .map(|root| format!("{root}/src/nvim/eval.lua"))
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../codegen/upstream/eval.lua").to_owned());
    let source = std::fs::read_to_string(source_path).unwrap();
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
    let mixed = call("sort", vec![Typval::list(vec![number(0), text("x")])]).unwrap();
    assert_eq!(mixed, Typval::list(vec![text("x"), number(0)]));
}

#[test]
fn sort_numeric_mode() {
    // `n` mode: each value is stringified and its leading number parsed, so
    // numeric values order numerically while strings order as 0.
    let result = call("sort", vec![list(&[2, 10, 1]), text("n")]).unwrap();
    assert_eq!(result, list(&[1, 2, 10]));
    let mixed = call("sort", vec![Typval::list(vec![text("a"), number(5), number(3)]), text("n")]).unwrap();
    assert_eq!(mixed, Typval::list(vec![text("a"), number(3), number(5)]));
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
    let values = Typval::list(vec![Typval::Float(2.5), Typval::Float(1.5), Typval::Float(10.0)]);
    let result = call("sort", vec![values, text("f")]).unwrap();
    assert_eq!(result, Typval::list(vec![Typval::Float(1.5), Typval::Float(2.5), Typval::Float(10.0)]));
}

#[test]
fn sort_ignore_case_mode() {
    // `i` mode: case-insensitive string sort.
    let result = call("sort", vec![Typval::list(vec![text("banana"), text("Apple"), text("cherry")]), text("i")]).unwrap();
    assert_eq!(result, Typval::list(vec![text("Apple"), text("banana"), text("cherry")]));
}

#[test]
fn sort_locale_mode_is_byte_wise_fallback() {
    // `l` mode is documented to sort by the locale of the running system; this
    // port uses a byte-wise fallback for the C-locale `strcoll` comparison, so
    // it matches the default ordering here.
    let result = call("sort", vec![Typval::list(vec![text("banana"), text("Apple")]), text("l")]).unwrap();
    assert_eq!(result, Typval::list(vec![text("Apple"), text("banana")]));
    let numbers = call("sort", vec![list(&[2, 10]), text("l")]).unwrap();
    assert_eq!(numbers, list(&[10, 2]));
}

#[test]
fn sort_callback_stops_after_first_failure_and_retains_first_error() {
    // Once the comparator errors, `sort()` must not invoke it again for later
    // pairs, and the original error must be returned rather than overwritten.
    let mut scope = Scope::new();
    let counter = Typval::list(vec![]);
    scope.set(b"counter", counter.clone());
    let values = Typval::list(vec![number(3), number(1), number(2)]);
    let callback = text("add(counter, 1) + missing");
    let mut builtins = Builtins::without_regex();
    let error = builtins.call(&OxStr::from("sort"), vec![values, callback], &mut scope).unwrap_err();
    assert_eq!(error.code, "E121");
    assert_eq!(counter, Typval::list(vec![number(1)]));
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
    for _ in 0..101 { value = Typval::list(vec![value]); }
    assert_eq!(call("count", vec![Typval::list(vec![value.clone()]), value]).unwrap_err().code, "E724");
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
    fn substitute(&self, text: &OxStr, pattern: &OxStr, replacement: &OxStr, flags: &OxStr) -> crate::Result<OxStr> {
        self.calls.set(self.calls.get() + 1);
        let source = text.to_string_lossy();
        let pattern = pattern.to_string_lossy();
        let replacement = replacement.to_string_lossy();
        let replaced = if flags.as_bytes().contains(&b'g') {
            source.replace(pattern.as_ref(), replacement.as_ref())
        } else {
            source.replacen(pattern.as_ref(), replacement.as_ref(), 1)
        };
        Ok(OxStr(replaced.into_bytes()))
    }
}

#[test]
fn split_uses_regex_engine_seam() {
    let regex = LiteralRegex { calls: Cell::new(0) };
    let mut builtins = Builtins::new(&regex);
    let result = builtins.call(&OxStr::from("split"), vec![text("a,b"), text(",")], &mut Scope::new()).unwrap();
    assert_eq!(result, Typval::list(vec![text("a"), text("b")]));
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
    assert_eq!(builtins.call(&OxStr::from("match"), vec![Typval::list(vec![text("x"), text("ab"), text("ab")]), text("b"), number(0), number(2)], &mut Scope::new()).unwrap(), number(2));
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


fn eval_builtin(source: &[u8], mut scope: Scope) -> (Typval, Scope) {
    let expression = Parser::new(source).parse().unwrap();
    let regex = NoRegex;
    let mut builtins = Builtins::without_regex();
    let result = Evaluator::new(&mut builtins, &regex).eval(&expression, &mut scope).unwrap();
    (result, scope)
}

#[test]
fn assignment_clone_shares_list_mutation() {
    let shared = list(&[1]);
    let mut scope = Scope::new();
    scope.set(b"a", shared.clone());
    scope.set(b"b", shared);
    let (result, scope) = eval_builtin(b"add(a, 2)", scope);
    assert_eq!(result, list(&[1, 2]));
    assert_eq!(scope.get(b"b", 0).unwrap(), &list(&[1, 2]));
}

#[test]
fn identity_and_equality_distinguish_shared_lists() {
    let mut scope = Scope::new();
    let shared = list(&[1]);
    scope.set(b"a", shared.clone());
    scope.set(b"alias", shared);
    scope.set(b"equal", list(&[1]));
    assert_eq!(eval_builtin(b"a is alias", scope.clone()).0, number(1));
    assert_eq!(eval_builtin(b"a is equal", scope.clone()).0, number(0));
    assert_eq!(eval_builtin(b"a == equal", scope).0, number(1));
}

#[test]
fn copy_is_outer_independent_but_keeps_nested_aliases() {
    let nested = list(&[1]);
    let source = Typval::list(vec![nested.clone()]);
    let copied = call("copy", vec![source.clone()]).unwrap();
    call("add", vec![copied.clone(), number(9)]).unwrap();
    assert_eq!(call("len", vec![source.clone()]).unwrap(), number(1));
    let Typval::List(copied_ref) = copied else { panic!("List expected") };
    let copied_nested = copied_ref.borrow().items[0].clone();
    call("add", vec![copied_nested, number(2)]).unwrap();
    assert_eq!(nested, list(&[1, 2]));
}

#[test]
fn deepcopy_reproduces_cycles_and_breaks_source_aliases() {
    let source = Typval::list(vec![]);
    call("add", vec![source.clone(), source.clone()]).unwrap();
    assert_eq!(call("string", vec![source.clone()]).unwrap(), text("[[...]]"));
    let copied = call("deepcopy", vec![source.clone()]).unwrap();
    assert_eq!(call("string", vec![copied.clone()]).unwrap(), text("[[...]]"));
    let (Typval::List(source_ref), Typval::List(copy_ref)) = (&source, &copied) else { panic!("Lists expected") };
    assert!(!std::rc::Rc::ptr_eq(source_ref, copy_ref));
    let Typval::List(cycle_ref) = &copy_ref.borrow().items[0] else { panic!("cycle expected") };
    assert!(std::rc::Rc::ptr_eq(copy_ref, cycle_ref));
}

#[test]
fn cycle_equality_terminates_coinductively() {
    let left = Typval::list(vec![]);
    let right = Typval::list(vec![]);
    call("add", vec![left.clone(), left.clone()]).unwrap();
    call("add", vec![right.clone(), right.clone()]).unwrap();
    let mut scope = Scope::new();
    scope.set(b"left", left);
    scope.set(b"right", right);
    assert_eq!(eval_builtin(b"left == right", scope).0, number(1));
}

#[test]
fn shallow_and_deep_locks_enforce_mutation_and_report_state() {
    let nested = list(&[1]);
    let shallow = Typval::list(vec![nested.clone()]);
    crate::lock_value(&shallow, false).unwrap();
    assert_eq!(crate::is_locked_value(&shallow).unwrap(), number(2));
    assert_eq!(call("add", vec![shallow, number(2)]).unwrap_err().code, "E741");
    assert_eq!(crate::is_locked_value(&nested).unwrap(), number(0));

    let deep_nested = list(&[1]);
    let deep = Typval::list(vec![deep_nested.clone()]);
    crate::lock_value(&deep, true).unwrap();
    assert_eq!(crate::is_locked_value(&deep).unwrap(), number(3));
    assert_eq!(crate::is_locked_value(&deep_nested).unwrap(), number(3));
    assert_eq!(call("add", vec![deep_nested, number(2)]).unwrap_err().code, "E741");
}

#[test]
fn lambda_callbacks_cover_map_filter_sort_foreach_reduce() {
    assert_eq!(eval_builtin(b"map([1, 2, 3], {k, v -> v * 2})", Scope::new()).0, list(&[2, 4, 6]));
    assert_eq!(eval_builtin(b"filter([1, 2, 3, 4], {k, v -> v % 2})", Scope::new()).0, list(&[1, 3]));
    assert_eq!(eval_builtin(b"sort([3, 1, 2], {a, b -> a - b})", Scope::new()).0, list(&[1, 2, 3]));
    assert_eq!(eval_builtin(b"foreach([1, 2], {k, v -> v * 9})", Scope::new()).0, list(&[1, 2]));
    assert_eq!(eval_builtin(b"reduce([1, 2, 3], {a, v -> a + v})", Scope::new()).0, number(6));
}

#[test]
fn mapnew_and_flattennew_do_not_mutate_inputs() {
    let source = list(&[1, 2]);
    let mut scope = Scope::new();
    scope.set(b"xs", source.clone());
    assert_eq!(eval_builtin(b"mapnew(xs, {k, v -> v + 10})", scope).0, list(&[11, 12]));
    assert_eq!(source, list(&[1, 2]));

    let nested = Typval::list(vec![number(1), list(&[2, 3])]);
    assert_eq!(call("flattennew", vec![nested.clone()]).unwrap(), list(&[1, 2, 3]));
    assert_eq!(nested, Typval::list(vec![number(1), list(&[2, 3])]));
}

#[test]
fn string_expression_and_funcref_callbacks_use_shared_dispatch() {
    assert_eq!(call("map", vec![list(&[1, 2]), text("v:val * 3")]).unwrap(), list(&[3, 6]));
    let and = Typval::Funcref(ox_types::Funcref { name: OxStr::from("and"), args: vec![], dict: None, registry: None });
    assert_eq!(call("map", vec![list(&[3, 3]), and]).unwrap(), list(&[0, 1]));
}

#[test]
fn callback_structural_mutation_is_rejected_and_lock_restored() {
    let mut scope = Scope::new();
    scope.set(b"xs", list(&[1, 2]));
    let expression = Parser::new(b"map(xs, {k, v -> add(xs, 9)})").parse().unwrap();
    let regex = NoRegex;
    let mut builtins = Builtins::without_regex();
    let error = Evaluator::new(&mut builtins, &regex).eval(&expression, &mut scope).unwrap_err();
    assert_eq!(error.code, "E741");
    let xs = scope.get(b"xs", 0).unwrap().clone();
    assert_eq!(call("add", vec![xs, number(3)]).unwrap(), list(&[1, 2, 3]));
}


#[test]
fn named_funcref_callbacks_cover_collection_builtins() {
    assert_eq!(call("map", vec![list(&[3, 3]), funcref("and")]).unwrap(), list(&[0, 1]));
    assert_eq!(call("filter", vec![list(&[3, 3]), funcref("and")]).unwrap(), list(&[3]));
    assert_eq!(call("foreach", vec![list(&[3, 3]), funcref("and")]).unwrap(), list(&[3, 3]));
    assert_eq!(call("reduce", vec![list(&[1, 2, 3]), funcref("or")]).unwrap(), number(3));
    let sorted = call("sort", vec![list(&[1, 2]), funcref("and")]).unwrap();
    assert_eq!(call("len", vec![sorted]).unwrap(), number(2));
}

#[test]
fn string_expression_callbacks_cover_collection_builtins() {
    assert_eq!(call("map", vec![list(&[1, 2]), text("v:val * 3")]).unwrap(), list(&[3, 6]));
    assert_eq!(call("filter", vec![list(&[1, 2, 3]), text("v:val % 2")]).unwrap(), list(&[1, 3]));
    assert_eq!(call("foreach", vec![list(&[1, 2]), text("v:val * 9")]).unwrap(), list(&[1, 2]));
    assert_eq!(call("reduce", vec![list(&[1, 2, 3]), text("v:key + v:val")]).unwrap(), number(6));
    assert_eq!(call("sort", vec![list(&[3, 1, 2]), text("v:key - v:val")]).unwrap(), list(&[1, 2, 3]));
}

#[test]
fn scope_lockvar_facade_reports_all_container_lock_states() {
    let direct = list(&[]);
    let Typval::List(reference) = &direct else { panic!("List expected") };
    reference.borrow_mut().lock.locked = true;
    let mut scope = Scope::new();
    scope.set(b"direct", direct);
    scope.set(b"shallow", list(&[]));
    scope.set(b"deep", list(&[]));
    assert_eq!(scope.islocked(b"missing", 0).unwrap_err().code, "E121");
    assert_eq!(scope.islocked(b"direct", 0).unwrap(), 1);
    scope.lockvar(b"shallow", false, 0).unwrap();
    scope.lockvar(b"deep", true, 0).unwrap();
    assert_eq!(scope.islocked(b"shallow", 0).unwrap(), 2);
    assert_eq!(scope.islocked(b"deep", 0).unwrap(), 3);
}


#[test]
fn callback_collections_support_blob_and_string_inputs() {
    assert_eq!(call("map", vec![Typval::Blob(vec![1, 2]), text("v:val + 1")]).unwrap(), Typval::Blob(vec![2, 3]));
    assert_eq!(call("mapnew", vec![Typval::Blob(vec![1, 2]), text("v:val + 2")]).unwrap(), Typval::Blob(vec![3, 4]));
    assert_eq!(call("filter", vec![Typval::Blob(vec![1, 2, 3]), text("v:val % 2")]).unwrap(), Typval::Blob(vec![1, 3]));
    assert_eq!(call("foreach", vec![Typval::Blob(vec![1, 2]), text("v:val + 9")]).unwrap(), Typval::Blob(vec![1, 2]));

    assert_eq!(call("map", vec![text("ab"), text("'x'")]).unwrap(), text("xx"));
    assert_eq!(call("mapnew", vec![text("ab"), text("'y'")]).unwrap(), text("yy"));
    assert_eq!(call("filter", vec![text("abc"), text("v:key % 2")]).unwrap(), text("b"));
    assert_eq!(call("foreach", vec![text("ab"), text("v:key")]).unwrap(), text("ab"));
}

#[test]
fn reduce_supports_blob_and_string_inputs() {
    assert_eq!(call("reduce", vec![Typval::Blob(vec![1, 2, 3]), text("v:key + v:val")]).unwrap(), number(6));
    assert_eq!(call("reduce", vec![text("abc"), text("v:key")]).unwrap(), text("a"));
}


#[test]
fn map_exposes_prior_mutations_and_keeps_them_after_later_error() {
    let shared = list(&[1, 2]);
    let mut scope = Scope::new();
    scope.set(b"xs", shared.clone());
    assert_eq!(eval_builtin(b"map(xs, {k, v -> k ? xs[0] : 9})", scope).0, list(&[9, 9]));

    let partial = list(&[1, 2]);
    let mut scope = Scope::new();
    scope.set(b"xs", partial.clone());
    let expression = Parser::new(b"map(xs, {k, v -> k ? missing : 9})").parse().unwrap();
    let regex = NoRegex;
    let mut builtins = Builtins::without_regex();
    assert_eq!(Evaluator::new(&mut builtins, &regex).eval(&expression, &mut scope).unwrap_err().code, "E121");
    assert_eq!(partial, list(&[9, 2]));
    assert_eq!(call("add", vec![partial, number(3)]).unwrap(), list(&[9, 2, 3]));
}

#[test]
fn string_callbacks_preserve_invalid_bytes() {
    let raw = Typval::String(OxStr(vec![0xff, b'a']));
    assert_eq!(call("map", vec![raw.clone(), text("v:val")]).unwrap(), raw);
    assert_eq!(call("filter", vec![raw.clone(), text("1")]).unwrap(), raw);
    assert_eq!(call("foreach", vec![raw.clone(), text("v:val")]).unwrap(), raw);
    assert_eq!(call("reduce", vec![raw, text("v:key")]).unwrap(), Typval::String(OxStr(vec![0xff])));
}

// ── Buffer-seam builtins: getline / setline ───────────────────────────
// Upstream: src/nvim/eval/buffer.c set_buffer_lines / get_buffer_lines,
// f_setline / f_getline; runtime/doc/vimfn.txt setline() / getline();
// test/old/testdir/test_bufline.vim covers the same surface per-buffer.

/// Minimal seam double: a flat line list with 1-based addressing, exactly
/// the operations `BufferHost` promises.
#[derive(Default)]
struct FakeBuffer {
    lines: Vec<String>,
    cursor: Option<i64>,
    marks: Vec<(char, i64)>,
}

impl FakeBuffer {
    fn new(lines: &[&str]) -> Self {
        Self { lines: lines.iter().map(|line| (*line).to_owned()).collect(), cursor: None, marks: Vec::new() }
    }
}

impl crate::eval::BufferHost for FakeBuffer {
    fn line_count(&self) -> crate::Result<usize> {
        Ok(self.lines.len())
    }

    fn get_line(&self, lnum: usize) -> crate::Result<Option<OxStr>> {
        Ok(self.lines.get(lnum - 1).map(|line| OxStr::from(line.as_str())))
    }

    fn replace_line(&mut self, lnum: usize, text: &OxStr) -> crate::Result<()> {
        self.lines[lnum - 1] = text.to_string_lossy().into_owned();
        Ok(())
    }

    fn append_line(&mut self, text: &OxStr) -> crate::Result<()> {
        self.lines.push(text.to_string_lossy().into_owned());
        Ok(())
    }

    fn address_line(&self, address: &str) -> crate::Result<Option<i64>> {
        let mut chars = address.chars();
        match chars.next() {
            Some('.') if chars.next().is_none() => Ok(self.cursor),
            Some('\'') => Ok(chars.next().and_then(|name| {
                self.marks.iter().find(|(mark, _)| *mark == name).map(|(_, line)| *line)
            })),
            _ => Ok(None),
        }
    }
}

fn buffer_call(lines: &[&str], name: &str, args: Vec<Typval>) -> (crate::Result<Typval>, FakeBuffer) {
    let mut buffer = FakeBuffer::new(lines);
    let result = crate::builtins::call_buffer_builtin(&mut buffer, name, args);
    (result, buffer)
}

fn texts(values: &[&str]) -> Typval {
    Typval::list(values.iter().map(|value| text(value)).collect())
}

#[test]
fn setline_replaces_existing_line_and_returns_zero() {
    // f_setline → set_buffer_lines: `lnum <= ml_line_count` replaces.
    let (result, buffer) = buffer_call(&["a", "b", "c"], "setline", vec![number(2), text("x")]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "x", "c"]);
}

#[test]
fn setline_appends_just_past_the_last_line() {
    // builtin.txt setline(): "When {lnum} is just below the last line the
    // {text} will be added below the last line."
    let (result, buffer) = buffer_call(&["a", "b", "c"], "setline", vec![number(4), text("d")]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "b", "c", "d"]);
}

#[test]
fn setline_beyond_line_count_plus_one_fails_without_writing() {
    // set_buffer_lines: `lnum > ml_line_count + 1` breaks with FAIL (1).
    let (result, buffer) = buffer_call(&["a", "b", "c"], "setline", vec![number(5), text("x")]);
    assert_eq!(result.unwrap(), number(1));
    assert_eq!(buffer.lines, vec!["a", "b", "c"]);
}

#[test]
fn setline_below_one_fails_without_writing() {
    // set_buffer_lines: `lnum < 1` reports FAIL before any write.
    let (result, buffer) = buffer_call(&["a", "b"], "setline", vec![number(0), text("x")]);
    assert_eq!(result.unwrap(), number(1));
    assert_eq!(buffer.lines, vec!["a", "b"]);
}

#[test]
fn setline_empty_list_always_succeeds_and_writes_nothing() {
    // set_buffer_lines: "not appending anything always succeeds".
    let (result, buffer) = buffer_call(&["a"], "setline", vec![number(1), texts(&[])]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a"]);
}

#[test]
fn setline_list_replaces_then_appends_consecutive_lines() {
    // builtin.txt setline(): equivalent to one setline() per item, so line
    // 2 of three is replaced and items past the end are appended.
    let (result, buffer) =
        buffer_call(&["a", "b", "c"], "setline", vec![number(2), texts(&["x", "y", "z"])]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "x", "y", "z"]);
}

#[test]
fn setline_list_starting_out_of_bounds_fails_unchanged() {
    // The loop's first iteration hits `lnum > ml_line_count + 1`.
    let (result, buffer) = buffer_call(&["a", "b", "c"], "setline", vec![number(9), texts(&["x", "y"])]);
    assert_eq!(result.unwrap(), number(1));
    assert_eq!(buffer.lines, vec!["a", "b", "c"]);
}

#[test]
fn setline_appends_list_into_empty_tail_starting_at_last_plus_one() {
    // A 1-line buffer growing to three: first item appends, later items
    // append behind it because the count grows with each write.
    let (result, buffer) = buffer_call(&["a"], "setline", vec![number(2), texts(&["x", "y"])]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "x", "y"]);
}

#[test]
fn setline_converts_non_string_types_like_string() {
    // typval_tostring(_, false): non-Strings use their string() rendering.
    let (result, buffer) = buffer_call(&["a"], "setline", vec![number(1), number(42)]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["42"]);
    // A List nested as the single item of the outer list renders like
    // string(): "[7, 8]".
    let nested = Typval::list(vec![list(&[7, 8])]);
    let (result, buffer) = buffer_call(&["a"], "setline", vec![number(1), nested]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["[7, 8]"]);
}

#[test]
fn setline_dollar_address_targets_last_line() {
    // tv_get_lnum → var2fpos("$") resolves to the last line.
    let (result, buffer) = buffer_call(&["a", "b", "c"], "setline", vec![text("$"), text("x")]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "b", "x"]);
}

#[test]
fn getline_single_line_returns_string() {
    let (result, buffer) = buffer_call(&["a", "b", "c"], "getline", vec![number(2)]);
    assert_eq!(result.unwrap(), text("b"));
    assert_eq!(buffer.lines, vec!["a", "b", "c"]);
}

#[test]
fn getline_out_of_range_single_yields_empty_string() {
    // builtin.txt getline(): "smaller than 1 or bigger than the number of
    // lines in the buffer, an empty string is returned".
    let (result, _) = buffer_call(&["a", "b"], "getline", vec![number(0)]);
    assert_eq!(result.unwrap(), text(""));
    let (result, _) = buffer_call(&["a", "b"], "getline", vec![number(9)]);
    assert_eq!(result.unwrap(), text(""));
}

#[test]
fn getline_range_returns_inclusive_list() {
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", vec![number(1), number(3)]);
    let expected = Typval::list(vec![text("a"), text("b"), text("c")]);
    assert_eq!(result.unwrap(), expected);
}

#[test]
fn getline_range_clamps_and_omits_missing_lines() {
    // get_buffer_lines: start clamps up to 1, end clamps down to
    // ml_line_count; non-existing lines are silently omitted.
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", vec![number(0), number(2)]);
    let expected = Typval::list(vec![text("a"), text("b")]);
    assert_eq!(result.unwrap(), expected);
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", vec![number(2), number(99)]);
    let expected = Typval::list(vec![text("b"), text("c")]);
    assert_eq!(result.unwrap(), expected);
}

#[test]
fn getline_inverted_or_negative_range_yields_empty_list() {
    // get_buffer_lines: `start < 0 || end < start` → empty List.
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", vec![number(3), number(2)]);
    assert_eq!(result.unwrap(), texts(&[]));
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", vec![number(-1), number(2)]);
    assert_eq!(result.unwrap(), texts(&[]));
}

#[test]
fn getline_dollar_address_reads_last_line() {
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", vec![text("$")]);
    assert_eq!(result.unwrap(), text("c"));
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", vec![number(2), text("$")]);
    let expected = Typval::list(vec![text("b"), text("c")]);
    assert_eq!(result.unwrap(), expected);
}

// tv_get_lnum: after a non-positive numeric conversion of a String, the
// address translates through var2fpos — "." is the cursor, "'x" a mark.
#[test]
fn getline_dot_and_mark_addresses_translate_through_the_seam() {
    let mut buffer = FakeBuffer::new(&["a", "b", "c"]);
    buffer.cursor = Some(2);
    buffer.marks = vec![('a', 3)];
    let result = crate::builtins::call_buffer_builtin(&mut buffer, "getline", vec![text(".")]);
    assert_eq!(result.unwrap(), text("b"));
    let result = crate::builtins::call_buffer_builtin(&mut buffer, "getline", vec![text("'a")]);
    assert_eq!(result.unwrap(), text("c"));
    let result = crate::builtins::call_buffer_builtin(
        &mut buffer,
        "getline",
        vec![text("'a"), text("$")],
    );
    assert_eq!(result.unwrap(), Typval::list(vec![text("c")]));
}

#[test]
fn getline_unresolved_address_degrades_to_zero() {
    // var2fpos returns NULL for an unset mark or an unknown address; the
    // lnum stays 0 and getline("'z") reads no line.
    let mut buffer = FakeBuffer::new(&["a", "b"]);
    buffer.cursor = None;
    let result = crate::builtins::call_buffer_builtin(&mut buffer, "getline", vec![text("'z")]);
    assert_eq!(result.unwrap(), text(""));
    let result = crate::builtins::call_buffer_builtin(&mut buffer, "getline", vec![text("w0")]);
    assert_eq!(result.unwrap(), text(""));
}

#[test]
fn setline_accepts_string_addresses() {
    let mut buffer = FakeBuffer::new(&["a", "b", "c"]);
    buffer.cursor = Some(2);
    let result =
        crate::builtins::call_buffer_builtin(&mut buffer, "setline", vec![text("."), text("x")]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "x", "c"]);
    // An unresolvable address keeps the failure result of lnum 0.
    let result =
        crate::builtins::call_buffer_builtin(&mut buffer, "setline", vec![text("'z"), text("y")]);
    assert_eq!(result.unwrap(), number(1));
    assert_eq!(buffer.lines, vec!["a", "x", "c"]);
}

#[test]
fn buffer_builtins_check_arity_from_generated_specs() {
    // eval.lua rows: getline {1,2}, setline {2,2} → E119/E118.
    let (result, _) = buffer_call(&["a"], "getline", vec![]);
    assert_eq!(result.unwrap_err().code, "E119");
    let (result, _) = buffer_call(&["a"], "setline", vec![number(1)]);
    assert_eq!(result.unwrap_err().code, "E119");
    let (result, _) = buffer_call(&["a"], "setline", vec![number(1), text("x"), number(3)]);
    assert_eq!(result.unwrap_err().code, "E118");
}

#[test]
fn typval_dispatcher_leaves_buffer_builtins_unimplemented() {
    // `Builtins` alone has no buffer; only hosts routing through
    // `call_buffer_builtin` serve getline/setline.
    assert!(crate::builtins::is_buffer_builtin("getline"));
    assert!(crate::builtins::is_buffer_builtin("setline"));
    assert!(!crate::builtins::is_buffer_builtin("getbufline"));
    let error = call("setline", vec![number(1), text("x")]).unwrap_err();
    assert!(matches!(error.kind, crate::EvalErrorKind::NotImplemented(_)));
}

#[test]
fn fnamemodify_obeys_filename_modifier_order() {
    // cmdline.txt `filename-modifiers`: :h/:t and repeated :r/:e.
    assert_eq!(call("fnamemodify", vec![text("src/archive.tar.gz"), text(":h")]).unwrap(), text("src"));
    assert_eq!(call("fnamemodify", vec![text("src/archive.tar.gz"), text(":t:r:r")]).unwrap(), text("archive"));
    assert_eq!(call("fnamemodify", vec![text("src/archive.tar.gz"), text(":e:e")]).unwrap(), text("tar.gz"));
    assert_eq!(call("fnamemodify", vec![text(".nvimrc"), text(":r")]).unwrap(), text(".nvimrc"));
    assert_eq!(call("fnamemodify", vec![text("src/"), text(":h")]).unwrap(), text("src"));
    assert_eq!(call("fnamemodify", vec![text("src/"), text(":t")]).unwrap(), text(""));
    assert_eq!(call("fnamemodify", vec![text("src/x"), text(":8:t")]).unwrap(), text("x"));
    assert_eq!(call("fnamemodify", vec![text(""), text(":h")]).unwrap(), text("."));
}

#[test]
fn fnamemodify_full_relative_and_home_forms() {
    let current = std::env::current_dir().unwrap();
    let absolute = call("fnamemodify", vec![text("src/file.rs"), text(":p")]).unwrap();
    assert_eq!(absolute, text(&current.join("src/file.rs").to_string_lossy()));
    assert_eq!(call("fnamemodify", vec![absolute.clone(), text(":.")]).unwrap(), text("src/file.rs"));
    if let Some(home) = std::env::var_os("HOME") {
        let path = std::path::PathBuf::from(home).join("file");
        assert_eq!(call("fnamemodify", vec![text(&path.to_string_lossy()), text(":~")]).unwrap(), text("~/file"));
    }
    assert_eq!(call("fnamemodify", vec![text("file"), text(":unsupported")]).unwrap(), text("file"));
}

#[test]
fn fnamemodify_substitutions_use_regex_seam() {
    let regex = LiteralRegex { calls: Cell::new(0) };
    let mut builtins = Builtins::new(&regex);
    let mut scope = Scope::new();
    assert_eq!(builtins.call(&OxStr::from("fnamemodify"), vec![text("src/version.c"), text(":s?version?main?")], &mut scope).unwrap(), text("src/main.c"));
    assert_eq!(builtins.call(&OxStr::from("fnamemodify"), vec![text("a/a/a"), text(":gs?a?b?")], &mut scope).unwrap(), text("b/b/b"));
    assert_eq!(regex.calls.get(), 2);
}

#[test]
fn simplify_preserves_relative_and_trailing_separators() {
    // vimfn.txt simplify(): simplify("./dir/.././/file/") == "./file/".
    assert_eq!(call("simplify", vec![text("./dir/.././/file/")]).unwrap(), text("./file/"));
    assert_eq!(call("simplify", vec![text("///one//two/../three")]).unwrap(), text("/one/three"));
}

#[test]
fn filesystem_predicates_and_resolve_use_real_file_types() {
    let root = std::env::temp_dir().join(format!("ox-eval-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("directory")).unwrap();
    std::fs::write(root.join("file"), b"data").unwrap();
    assert_eq!(call("filereadable", vec![text(&root.join("file").to_string_lossy())]).unwrap(), number(1));
    assert_eq!(call("filereadable", vec![text(&root.join("directory").to_string_lossy())]).unwrap(), number(0));
    assert_eq!(call("isdirectory", vec![text(&root.join("directory").to_string_lossy())]).unwrap(), number(1));
    assert_eq!(call("isdirectory", vec![text(&root.join("missing").to_string_lossy())]).unwrap(), number(0));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("directory"), root.join("link")).unwrap();
        assert_eq!(call("resolve", vec![text(&root.join("link/").to_string_lossy())]).unwrap(), text(&format!("{}/", root.join("directory").to_string_lossy())));
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn getcwd_accepts_upstream_optional_numeric_selectors() {
    let expected = text(&std::env::current_dir().unwrap().to_string_lossy());
    assert_eq!(call("getcwd", vec![]).unwrap(), expected);
    assert_eq!(call("getcwd", vec![number(-1), number(-1), number(-1)]).unwrap(), expected);
}

#[test]
fn glob_supports_sorted_string_list_and_recursive_results() {
    let root = std::env::temp_dir().join(format!("ox-eval-glob-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
    std::fs::write(root.join("b.txt"), b"b").unwrap();
    std::fs::write(root.join("a.txt"), b"a").unwrap();
    std::fs::write(root.join("nested/deeper/c.txt"), b"c").unwrap();
    let first = root.join("a.txt").to_string_lossy().into_owned();
    let second = root.join("b.txt").to_string_lossy().into_owned();
    assert_eq!(call("glob", vec![text(&root.join("*.txt").to_string_lossy())]).unwrap(), text(&format!("{first}\n{second}")));
    assert_eq!(call("glob", vec![text(&root.join("*.txt").to_string_lossy()), number(0), number(1)]).unwrap(), Typval::list(vec![text(&first), text(&second)]));
    let recursive = call("glob", vec![text(&root.join("**/*.txt").to_string_lossy()), number(0), number(1)]).unwrap();
    let Typval::List(items) = recursive else { panic!("list expected") };
    assert_eq!(items.borrow().items.len(), 3);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn globpath_joins_each_directory_and_honors_alllinks() {
    let root = std::env::temp_dir().join(format!("ox-eval-globpath-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("one")).unwrap();
    std::fs::create_dir_all(root.join("two")).unwrap();
    std::fs::write(root.join("one/item.vim"), b"one").unwrap();
    std::fs::write(root.join("two/item.vim"), b"two").unwrap();
    let paths = format!("{},{}", root.join("one").to_string_lossy(), root.join("two").to_string_lossy());
    assert_eq!(call("globpath", vec![text(&paths), text("*.vim"), number(0), number(1)]).unwrap(), Typval::list(vec![text(&root.join("one/item.vim").to_string_lossy()), text(&root.join("two/item.vim").to_string_lossy())]));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("missing"), root.join("one/dangling")).unwrap();
        assert_eq!(call("glob", vec![text(&root.join("one/dangling").to_string_lossy()), number(0), number(0), number(0)]).unwrap(), text(""));
        assert_eq!(call("glob", vec![text(&root.join("one/dangling").to_string_lossy()), number(0), number(0), number(1)]).unwrap(), text(&root.join("one/dangling").to_string_lossy()));
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn executable_checks_mode_bits_for_explicit_paths() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = std::env::temp_dir().join(format!("ox-eval-executable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let program = root.join("program");
    std::fs::write(&program, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(call("executable", vec![text(&program.to_string_lossy())]).unwrap(), number(0));
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(call("executable", vec![text(&program.to_string_lossy())]).unwrap(), number(1));
    assert_eq!(call("executable", vec![text("sh")]).unwrap(), number(1));
    std::fs::remove_dir_all(root).unwrap();
}

// f_printf: strings.c C-style formatting with flags, width, precision.
#[test]
fn printf_formats_strings_numbers_and_radices() {
    assert_eq!(call("printf", vec![text("Screen (%u lines)"), number(42)]).unwrap(), text("Screen (42 lines)"));
    assert_eq!(call("printf", vec![text("<%x>"), number(255)]).unwrap(), text("<ff>"));
    assert_eq!(call("printf", vec![text("%5.2d|%-5s|%05d"), number(3), text("ab"), number(42)]).unwrap(), text("   03|ab   |00042"));
    assert_eq!(call("printf", vec![text("100%% and %s"), text("done")]).unwrap(), text("100% and done"));
    assert!(call("printf", vec![text("%d %d"), number(1)]).is_err());
}
