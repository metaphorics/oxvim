//! Generated builtin metadata and typval-only builtin implementations.

use std::cmp::Ordering;
use std::fmt::Write as _;

use ox_types::{Funcref, OxStr, Special, Typval};
use serde_json::Value as JsonValue;

use crate::error::{EvalError, Result};
use crate::eval::{compare_bytes, BuiltinHost, Evaluator, RegexEngine};
use crate::parser::Parser;
use crate::scope::Scope;

const MAX_CONTAINER_DEPTH: usize = 100;

/// Declarative metadata recovered from Neovim's `eval.lua`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinSpec {
    /// Public Vimscript function name.
    pub name: &'static str,
    /// Minimum accepted argument count.
    pub min_args: usize,
    /// Maximum accepted argument count, or `None` for varargs.
    pub max_args: Option<usize>,
    /// Help signature from `eval.lua`.
    pub signature: &'static str,
    /// Whether `eval.lua` permits method-call syntax for an overload.
    pub method: bool,
}

include!(concat!(env!("OUT_DIR"), "/builtins_gen.rs"));

/// Look up generated builtin metadata by exact name.
#[must_use]
pub fn builtin_spec(name: &str) -> Option<&'static BuiltinSpec> {
    BUILTINS.binary_search_by_key(&name, |spec| spec.name).ok().map(|index| &BUILTINS[index])
}

/// Typval-only builtin dispatcher. Regex operations use only the supplied seam.
pub struct Builtins<'a> {
    regex: Option<&'a dyn RegexEngine>,
}

impl<'a> Builtins<'a> {
    /// Create a dispatcher with regex-backed builtins enabled through `regex`.
    #[must_use]
    pub const fn new(regex: &'a dyn RegexEngine) -> Self {
        Self { regex: Some(regex) }
    }

    /// Create a dispatcher whose regex-backed operations return a typed error.
    #[must_use]
    pub const fn without_regex() -> Self {
        Self { regex: None }
    }

    fn dispatch(&mut self, name: &str, args: Vec<Typval>, scope: &mut Scope) -> Result<Typval> {
        let spec = builtin_spec(name).ok_or_else(|| EvalError::not_implemented(OxStr::from(name)))?;
        if !is_implemented(name) {
            return Err(EvalError::not_implemented(OxStr::from(name)));
        }
        check_arity(spec, args.len())?;
        match name {
            "abs" => absolute(&args[0]),
            "add" => add(args),
            "and" => binary_number(&args, |left, right| left & right),
            "blob2list" => blob2list(&args[0]),
            "ceil" => float_unary(&args[0], f64::ceil),
            "char2nr" => char2nr(&args),
            "copy" => Ok(args[0].clone()),
            "count" => count(&args),
            "deepcopy" => deep_copy(&args[0], 0),
            "empty" => Ok(Typval::Number(i64::from(is_empty(&args[0])))),
            "escape" => escape(&args),
            "extend" | "extendnew" => extend(args),
            "filter" => self.filter_or_map(args, scope, false),
            "flatten" | "flattennew" => flatten(&args),
            "float2nr" | "trunc" => float_to_number(&args[0]),
            "floor" => float_unary(&args[0], f64::floor),
            "get" => get(&args),
            "has_key" => has_key(&args),
            "index" => index(&args),
            "insert" => insert(args),
            "items" => dict_projection(&args[0], Projection::Items),
            "join" => join(&args),
            "json_decode" => json_decode(&args[0]),
            "json_encode" => json_encode(&args[0]),
            "keys" => dict_projection(&args[0], Projection::Keys),
            "len" | "strlen" => length(&args[0], name == "strlen"),
            "list2blob" => list2blob(&args[0]),
            "list2str" => list2str(&args),
            "map" => self.filter_or_map(args, scope, true),
            "match" | "matchend" | "matchstr" => self.regex_match(name, &args),
            "max" => extremum(&args[0], true),
            "min" => extremum(&args[0], false),
            "nr2char" => nr2char(&args),
            "or" => binary_number(&args, |left, right| left | right),
            "pow" => float_binary(&args, f64::powf),
            "range" => range(&args),
            "remove" => remove(args),
            "repeat" => repeat(&args),
            "reverse" => reverse(args),
            "sort" => self.sort(args, scope),
            "split" => self.regex_split(&args),
            "sqrt" => float_unary(&args[0], f64::sqrt),
            "str2float" => str2float(&args[0]),
            "str2list" => str2list(&args),
            "str2nr" => str2nr(&args),
            "strcharlen" | "strchars" => strcharlen(&args[0]),
            "stridx" => string_index(&args, false),
            "string" => Ok(Typval::String(vim_string(&args[0], 0)?)),
            "strpart" => strpart(&args),
            "strridx" => string_index(&args, true),
            "substitute" => self.regex_substitute(&args),
            "tolower" => change_case(&args[0], false),
            "toupper" => change_case(&args[0], true),
            "trim" => trim(&args),
            "type" => Ok(Typval::Number(i64::from(args[0].vartype()))),
            "uniq" => uniq(args),
            "values" => dict_projection(&args[0], Projection::Values),
            "xor" => binary_number(&args, |left, right| left ^ right),
            _ => Err(EvalError::not_implemented(OxStr::from(name))),
        }
    }

    fn regex(&self) -> Result<&dyn RegexEngine> {
        self.regex.ok_or_else(|| EvalError::new("E54", 0, "regular-expression engine is not installed"))
    }

    fn regex_split(&self, args: &[Typval]) -> Result<Typval> {
        let text = string_arg(&args[0])?;
        let pattern = args.get(1).map_or_else(|| Ok(OxStr::from("\\s\\+")), string_arg)?;
        let keep_empty = args.get(2).is_some_and(Typval::is_truthy);
        self.regex()?.split(&text, &pattern, keep_empty).map(|parts| {
            Typval::List(parts.into_iter().map(Typval::String).collect())
        })
    }

    fn regex_match(&self, name: &str, args: &[Typval]) -> Result<Typval> {
        let pattern = string_arg(&args[1])?;
        let start_number = args.get(2).map(number_arg).transpose()?.unwrap_or(0).max(0);
        let occurrence = args.get(3).map(number_arg).transpose()?.unwrap_or(1).max(1) as usize;
        match &args[0] {
            Typval::List(values) => {
                let start = usize::try_from(start_number).unwrap_or(usize::MAX);
                let mut seen = 0usize;
                for (index, value) in values.iter().enumerate().skip(start) {
                    let text = string_arg(value)?;
                    if self.regex()?.is_match(&text, &pattern, false)? {
                        seen += 1;
                        if seen == occurrence {
                            return Ok(if name == "matchstr" { Typval::String(text) } else { Typval::Number(saturating_i64(index)) });
                        }
                    }
                }
                Ok(if name == "matchstr" { Typval::String(OxStr(Vec::new())) } else { Typval::Number(-1) })
            }
            value => {
                let text = string_arg(value)?;
                let mut search_start = usize::try_from(start_number).unwrap_or(usize::MAX);
                let mut found = None;
                for _ in 0..occurrence {
                    found = self.regex()?.find(&text, &pattern, search_start)?;
                    let Some((match_start, match_end)) = found else { break };
                    search_start = if match_end > match_start { match_end } else { match_start.saturating_add(1) };
                }
                Ok(match (name, found) {
                    ("matchstr", Some((start, end))) => Typval::String(OxStr(text.as_bytes()[start..end].to_vec())),
                    ("matchstr", None) => Typval::String(OxStr(Vec::new())),
                    ("matchend", Some((_, end))) => Typval::Number(saturating_i64(end)),
                    ("match", Some((start, _))) => Typval::Number(saturating_i64(start)),
                    (_, None) => Typval::Number(-1),
                    _ => Typval::Number(-1),
                })
            }
        }
    }

    fn regex_substitute(&self, args: &[Typval]) -> Result<Typval> {
        let text = string_arg(&args[0])?;
        let pattern = string_arg(&args[1])?;
        let replacement = string_arg(&args[2])?;
        let flags = string_arg(&args[3])?;
        self.regex()?.substitute(&text, &pattern, &replacement, &flags).map(Typval::String)
    }

    fn filter_or_map(&mut self, mut args: Vec<Typval>, scope: &mut Scope, mapping: bool) -> Result<Typval> {
        let callback = args.pop().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
        let container = args.pop().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
        match container {
            Typval::List(values) => {
                let mut result = Vec::with_capacity(values.len());
                for (key, value) in values.into_iter().enumerate() {
                    let mapped = self.eval_callback(&callback, Typval::Number(saturating_i64(key)), value.clone(), scope)?;
                    if mapping {
                        result.push(mapped);
                    } else if mapped.is_truthy() {
                        result.push(value);
                    }
                }
                Ok(Typval::List(result))
            }
            Typval::Dict(values) => {
                let mut result = Vec::with_capacity(values.len());
                for (key, value) in values {
                    let mapped = self.eval_callback(&callback, Typval::String(key.clone()), value.clone(), scope)?;
                    if mapping {
                        result.push((key, mapped));
                    } else if mapped.is_truthy() {
                        result.push((key, value));
                    }
                }
                Ok(Typval::Dict(result))
            }
            _ => Err(EvalError::new("E1251", 0, "List, Dictionary, Blob or String required")),
        }
    }

    fn eval_callback(&mut self, callback: &Typval, key: Typval, value: Typval, scope: &Scope) -> Result<Typval> {
        match callback {
            Typval::String(expression) => {
                let parsed = Parser::new(expression.as_bytes()).parse()?;
                let mut callback_scope = scope.snapshot();
                set_pair(&mut callback_scope.vim, b"key", key);
                set_pair(&mut callback_scope.vim, b"val", value);
                let regex = RegexRef(self.regex);
                Evaluator::new(self, &regex).eval(&parsed, &mut callback_scope)
            }
            Typval::Funcref(funcref) | Typval::Partial(funcref) if funcref.registry.is_none() => {
                let mut callback_args = funcref.args.clone();
                callback_args.push(key);
                callback_args.push(value);
                self.dispatch(&funcref.name.to_string_lossy(), callback_args, &mut scope.snapshot())
            }
            _ => Err(EvalError::new("E921", 0, "Invalid callback argument")),
        }
    }

    fn sort(&mut self, args: Vec<Typval>, scope: &mut Scope) -> Result<Typval> {
        let Some(Typval::List(values)) = args.first() else {
            return Err(EvalError::new("E714", 0, "List required"));
        };
        let mut values = values.clone();
        // Resolve the kind of comparison from the optional second argument,
        // mirroring `parse_sort_uniq_args` (funcs.c:1551-1593).
        let mode = match args.get(1) {
            None => SortMode::Default,
            Some(Typval::Number(number)) => match *number {
                0 => SortMode::Default,
                1 => SortMode::IgnoreCase,
                _ => return Err(EvalError::new("E474", 0, "Invalid argument")),
            },
            Some(Typval::String(text)) => match text.as_bytes() {
                b"" => SortMode::Default,
                b"i" => SortMode::IgnoreCase,
                b"n" => SortMode::Numeric,
                b"N" => SortMode::Integer,
                b"f" => SortMode::Float,
                b"l" => SortMode::Locale,
                _ => SortMode::Callback(Typval::String((*text).clone())),
            },
            Some(Typval::Funcref(_) | Typval::Partial(_)) => SortMode::Callback(args[1].clone()),
            _ => return Err(EvalError::new("E921", 0, "Invalid callback argument")),
        };
        let mut failure = None;
        values.sort_by(|left, right| {
            let result = match &mode {
                // Default and locale sorting compare the stringified values
                // (`sort([2, 10])` is `[10, 2]`, because "10" sorts before
                // "2"). `l` mode uses the documented byte-wise fallback for
                // the C-locale `strcoll` comparison, so it behaves exactly
                // like the default here.
                SortMode::Default | SortMode::Locale => Ok(sort_string_pair(left, right, false)),
                SortMode::IgnoreCase => Ok(sort_string_pair(left, right, true)),
                SortMode::Numeric => Ok(sort_numeric(left).total_cmp(&sort_numeric(right))),
                SortMode::Integer => Ok(sort_integer(left).cmp(&sort_integer(right))),
                SortMode::Float => Ok(sort_float(left).total_cmp(&sort_float(right))),
                SortMode::Callback(callback) => self.eval_callback(callback, left.clone(), right.clone(), scope).and_then(|value| number_arg(&value)).map(|value| value.cmp(&0)),
            };
            match result { Ok(ordering) => ordering, Err(error) => { failure = Some(error); Ordering::Equal } }
        });
        if let Some(error) = failure { return Err(error); }
        Ok(Typval::List(values))
    }
}

/// How the builtin `sort()`/`uniq()` comparison behaves (parse_sort_uniq_args).
enum SortMode {
    Default,
    IgnoreCase,
    Locale,
    Numeric,
    Integer,
    Float,
    Callback(Typval),
}

/// Encode a value for string comparison, following upstream `item_compare`
/// (typval.c:1228-1257): a raw String is used as-is, any other value is
/// stringified, and a String compared against a non-String sorts as a single
/// quote (`'`) so it precedes other encoded values.
fn sort_string_key(value: &Typval, peer_is_string: bool, depth: usize) -> Result<OxStr> {
    match value {
        Typval::String(text) if peer_is_string => Ok((*text).clone()),
        Typval::String(_) => Ok(OxStr::from("'")),
        _ => vim_string(value, depth),
    }
}

fn sort_string_pair(left: &Typval, right: &Typval, ignore_case: bool) -> Ordering {
    match (sort_string_key(left, matches!(right, Typval::String(_)), 0), sort_string_key(right, matches!(left, Typval::String(_)), 0)) {
        (Ok(left), Ok(right)) => match compare_bytes(left.as_bytes(), right.as_bytes(), ignore_case) { -1 => Ordering::Less, 1 => Ordering::Greater, _ => Ordering::Equal },
        _ => Ordering::Equal,
    }
}

/// Numeric mode `n`: upstream converts each value to a string and parses its
/// leading number, and a string value sorts as `0` (it becomes a single quote
/// that `strtod` reads as 0).
fn sort_numeric(value: &Typval) -> f64 {
    match value {
        Typval::String(_) => 0.0,
        _ => match vim_string(value, 0) { Ok(encoded) => leading_float(encoded.as_bytes()), Err(_) => 0.0 },
    }
}

/// Number mode `N`: integer comparison (`tv_get_number`), tolerant of
/// non-number values, matching upstream's integer key computation.
fn sort_integer(value: &Typval) -> i64 {
    match value {
        Typval::Number(number) => *number,
        Typval::Bool(boolean) => i64::from(*boolean),
        Typval::String(text) => parse_integer_prefix(text.as_bytes(), 10).unwrap_or(0),
        Typval::Special(Special::Null) => 0,
        _ => 0,
    }
}

/// Float mode `f`: float comparison (`tv_get_float`).
fn sort_float(value: &Typval) -> f64 {
    match value {
        Typval::Number(number) => *number as f64,
        Typval::Float(number) => *number,
        Typval::String(text) => leading_float(text.as_bytes()),
        _ => 0.0,
    }
}

/// Parse the leading decimal float of `bytes`, like `strtod`.
fn leading_float(bytes: &[u8]) -> f64 {
    String::from_utf8_lossy(bytes)
        .trim_start()
        .chars()
        .take_while(|character| character.is_ascii_digit() || matches!(character, '+' | '-' | '.' | 'e' | 'E'))
        .collect::<String>()
        .parse()
        .unwrap_or(0.0)
}

impl BuiltinHost for Builtins<'_> {
    fn call(&mut self, name: &OxStr, args: Vec<Typval>, scope: &mut Scope) -> Result<Typval> {
        self.dispatch(&name.to_string_lossy(), args, scope)
    }

    fn call_method(&mut self, name: &OxStr, args: Vec<Typval>, scope: &mut Scope) -> Result<Typval> {
        let name_text = name.to_string_lossy();
        let spec = builtin_spec(&name_text).ok_or_else(|| EvalError::not_implemented(name.clone()))?;
        if !spec.method {
            return Err(EvalError::new("E276", 0, format!("Cannot use function as a method: {name_text}")));
        }
        self.dispatch(&name_text, args, scope)
    }
}

struct RegexRef<'a>(Option<&'a dyn RegexEngine>);

impl RegexEngine for RegexRef<'_> {
    fn is_match(&self, text: &OxStr, pattern: &OxStr, ignore_case: bool) -> Result<bool> {
        self.0.ok_or_else(|| EvalError::new("E54", 0, "regular-expression engine is not installed"))?.is_match(text, pattern, ignore_case)
    }
    fn split(&self, text: &OxStr, pattern: &OxStr, keep_empty: bool) -> Result<Vec<OxStr>> {
        self.0.ok_or_else(|| EvalError::new("E54", 0, "regular-expression engine is not installed"))?.split(text, pattern, keep_empty)
    }
    fn find(&self, text: &OxStr, pattern: &OxStr, start: usize) -> Result<Option<(usize, usize)>> {
        self.0.ok_or_else(|| EvalError::new("E54", 0, "regular-expression engine is not installed"))?.find(text, pattern, start)
    }
    fn substitute(&self, text: &OxStr, pattern: &OxStr, replacement: &OxStr, flags: &OxStr) -> Result<OxStr> {
        self.0.ok_or_else(|| EvalError::new("E54", 0, "regular-expression engine is not installed"))?.substitute(text, pattern, replacement, flags)
    }
}

fn check_arity(spec: &BuiltinSpec, count: usize) -> Result<()> {
    if count < spec.min_args {
        return Err(EvalError::new("E119", 0, format!("Not enough arguments for function: {}", spec.name)));
    }
    if spec.max_args.is_some_and(|maximum| count > maximum) {
        return Err(EvalError::new("E118", 0, format!("Too many arguments for function: {}", spec.name)));
    }
    Ok(())
}

fn is_implemented(name: &str) -> bool {
    matches!(name,
        "abs" | "add" | "and" | "blob2list" | "ceil" | "char2nr" | "copy" | "count" |
        "deepcopy" | "empty" | "escape" | "extend" | "extendnew" | "filter" | "flatten" |
        "flattennew" | "float2nr" | "floor" | "get" | "has_key" | "index" | "insert" | "items" |
        "join" | "json_decode" | "json_encode" | "keys" | "len" | "strlen" | "list2blob" | "list2str" | "map" |
        "match" | "matchend" | "matchstr" | "max" | "min" | "nr2char" | "or" | "pow" | "range" |
        "remove" | "repeat" | "reverse" | "sort" | "split" | "sqrt" | "str2float" | "str2list" |
        "str2nr" | "strcharlen" | "strchars" | "stridx" | "string" | "strpart" | "strridx" |
        "substitute" | "tolower" | "toupper" | "trim" | "trunc" | "type" | "uniq" | "values" | "xor"
    )
}

fn number_arg(value: &Typval) -> Result<i64> {
    match value {
        Typval::Number(number) => Ok(*number),
        Typval::Bool(value) => Ok(i64::from(*value)),
        Typval::Special(Special::Null) => Ok(0),
        Typval::String(value) => Ok(parse_integer_prefix(value.as_bytes(), 10).unwrap_or(0)),
        Typval::Float(_) => Err(EvalError::new("E805", 0, "Using a Float as a Number")),
        Typval::List(_) => Err(EvalError::new("E745", 0, "Using a List as a Number")),
        Typval::Dict(_) => Err(EvalError::new("E728", 0, "Using a Dictionary as a Number")),
        _ => Err(EvalError::new("E745", 0, "Using invalid value as a Number")),
    }
}

fn float_arg(value: &Typval) -> Result<f64> {
    match value {
        Typval::Float(value) => Ok(*value),
        Typval::Number(value) => Ok(*value as f64),
        _ => Err(EvalError::new("E808", 0, "Number or Float required")),
    }
}

fn string_arg(value: &Typval) -> Result<OxStr> {
    match value {
        Typval::String(value) => Ok(value.clone()),
        Typval::Number(value) => Ok(OxStr(value.to_string().into_bytes())),
        Typval::Bool(value) => Ok(OxStr::from(if *value { "v:true" } else { "v:false" })),
        Typval::Special(Special::Null) => Ok(OxStr::from("v:null")),
        Typval::Float(_) => Err(EvalError::new("E806", 0, "Using a Float as a String")),
        Typval::List(_) => Err(EvalError::new("E730", 0, "Using a List as a String")),
        Typval::Dict(_) => Err(EvalError::new("E731", 0, "Using a Dictionary as a String")),
        _ => Err(EvalError::new("E729", 0, "Using invalid value as a String")),
    }
}

fn absolute(value: &Typval) -> Result<Typval> {
    match value {
        Typval::Float(value) => Ok(Typval::Float(value.abs())),
        _ => Ok(Typval::Number(number_arg(value)?.saturating_abs())),
    }
}

fn binary_number(args: &[Typval], operation: impl FnOnce(i64, i64) -> i64) -> Result<Typval> {
    Ok(Typval::Number(operation(number_arg(&args[0])?, number_arg(&args[1])?)))
}

fn float_unary(value: &Typval, operation: impl FnOnce(f64) -> f64) -> Result<Typval> {
    Ok(Typval::Float(operation(float_arg(value)?)))
}

fn float_binary(args: &[Typval], operation: impl FnOnce(f64, f64) -> f64) -> Result<Typval> {
    Ok(Typval::Float(operation(float_arg(&args[0])?, float_arg(&args[1])?)))
}

fn float_to_number(value: &Typval) -> Result<Typval> {
    let value = float_arg(value)?;
    let number = if value.is_nan() { 0 } else if value >= i64::MAX as f64 { i64::MAX } else if value <= i64::MIN as f64 { i64::MIN } else { value.trunc() as i64 };
    Ok(Typval::Number(number))
}

fn add(mut args: Vec<Typval>) -> Result<Typval> {
    let value = args.pop().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
    match args.pop() {
        Some(Typval::List(mut values)) => { values.push(value); Ok(Typval::List(values)) }
        Some(Typval::Blob(mut values)) => {
            let number = number_arg(&value)?;
            let byte = u8::try_from(number).map_err(|_| EvalError::new("E1230", 0, "Blob value must be in range 0 to 255"))?;
            values.push(byte);
            Ok(Typval::Blob(values))
        }
        _ => Err(EvalError::new("E897", 0, "List or Blob required")),
    }
}

fn is_empty(value: &Typval) -> bool {
    match value {
        Typval::Number(value) => *value == 0,
        Typval::Float(value) => *value == 0.0,
        Typval::String(value) => value.as_bytes().is_empty(),
        Typval::Blob(value) => value.is_empty(),
        Typval::List(value) => value.is_empty(),
        Typval::Dict(value) => value.is_empty(),
        Typval::Bool(value) => !value,
        Typval::Special(Special::Null) => true,
        Typval::Funcref(value) | Typval::Partial(value) => value.name.as_bytes().is_empty(),
        Typval::Channel(value) | Typval::Job(value) => *value == 0,
    }
}

fn length(value: &Typval, bytes_only: bool) -> Result<Typval> {
    let length = match value {
        Typval::String(value) => if bytes_only { value.as_bytes().len() } else { value.as_bytes().len() },
        Typval::Blob(value) => value.len(),
        Typval::List(value) => value.len(),
        Typval::Dict(value) => value.len(),
        _ => string_arg(value)?.as_bytes().len(),
    };
    Ok(Typval::Number(saturating_i64(length)))
}

fn strcharlen(value: &Typval) -> Result<Typval> {
    let value = string_arg(value)?;
    Ok(Typval::Number(saturating_i64(String::from_utf8_lossy(value.as_bytes()).chars().count())))
}

fn change_case(value: &Typval, upper: bool) -> Result<Typval> {
    let value = string_arg(value)?;
    let text = String::from_utf8_lossy(value.as_bytes());
    let changed = if upper { text.to_uppercase() } else { text.to_lowercase() };
    Ok(Typval::String(OxStr(changed.into_bytes())))
}

fn trim(args: &[Typval]) -> Result<Typval> {
    let value = string_arg(&args[0])?;
    let mask = args.get(1).map(string_arg).transpose()?;
    let direction = args.get(2).map(number_arg).transpose()?.unwrap_or(0);
    let bytes = value.as_bytes();
    let removable = |byte: u8| mask.as_ref().map_or(byte.is_ascii_whitespace(), |mask| mask.as_bytes().contains(&byte));
    let mut start = 0;
    let mut end = bytes.len();
    if direction != 2 { while start < end && removable(bytes[start]) { start += 1; } }
    if direction != 1 { while end > start && removable(bytes[end - 1]) { end -= 1; } }
    Ok(Typval::String(OxStr(bytes[start..end].to_vec())))
}

fn join(args: &[Typval]) -> Result<Typval> {
    let Typval::List(values) = &args[0] else { return Err(EvalError::new("E714", 0, "List required")); };
    let separator = args.get(1).map(string_arg).transpose()?.unwrap_or_else(|| OxStr::from(" "));
    let mut result = Vec::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 { result.extend_from_slice(separator.as_bytes()); }
        result.extend_from_slice(string_arg(value)?.as_bytes());
    }
    Ok(Typval::String(OxStr(result)))
}

fn repeat(args: &[Typval]) -> Result<Typval> {
    let count = number_arg(&args[1])?;
    let count = usize::try_from(count.max(0)).map_err(|_| EvalError::new("E1240", 0, "Resulting text too long"))?;
    match &args[0] {
        Typval::String(value) => Ok(Typval::String(OxStr(value.as_bytes().repeat(count)))),
        Typval::List(value) => {
            let mut repeated = Vec::with_capacity(value.len().saturating_mul(count));
            for _ in 0..count {
                repeated.extend(value.iter().cloned());
            }
            Ok(Typval::List(repeated))
        }
        Typval::Blob(value) => Ok(Typval::Blob(value.repeat(count))),
        _ => Err(EvalError::new("E1294", 0, "String, List or Blob required")),
    }
}

fn reverse(mut args: Vec<Typval>) -> Result<Typval> {
    match args.pop() {
        Some(Typval::String(value)) => {
            let reversed: String = String::from_utf8_lossy(value.as_bytes()).chars().rev().collect();
            Ok(Typval::String(OxStr(reversed.into_bytes())))
        }
        Some(Typval::List(mut value)) => { value.reverse(); Ok(Typval::List(value)) }
        Some(Typval::Blob(mut value)) => { value.reverse(); Ok(Typval::Blob(value)) }
        _ => Err(EvalError::new("E899", 0, "List, Blob or String required")),
    }
}

fn string_index(args: &[Typval], reverse: bool) -> Result<Typval> {
    let haystack = string_arg(&args[0])?;
    let needle = string_arg(&args[1])?;
    let requested = args.get(2).map(number_arg).transpose()?;
    let bytes = haystack.as_bytes();
    let position = if reverse {
        let maximum_start = requested.unwrap_or_else(|| saturating_i64(bytes.len())).max(0) as usize;
        let prefix_end = maximum_start.saturating_add(needle.as_bytes().len()).min(bytes.len());
        find_subslice_reverse(&bytes[..prefix_end], needle.as_bytes()).filter(|position| *position <= maximum_start)
    } else {
        let start = requested.unwrap_or(0).max(0) as usize;
        if start > bytes.len() { None } else { find_subslice(&bytes[start..], needle.as_bytes()).map(|position| position + start) }
    };
    Ok(Typval::Number(position.map_or(-1, saturating_i64)))
}

fn strpart(args: &[Typval]) -> Result<Typval> {
    let value = string_arg(&args[0])?;
    let start = number_arg(&args[1])?;
    let length = args.get(2).map(number_arg).transpose()?.unwrap_or(i64::MAX);
    let start = usize::try_from(start.max(0)).unwrap_or(usize::MAX).min(value.as_bytes().len());
    let length = usize::try_from(length.max(0)).unwrap_or(usize::MAX);
    let end = start.saturating_add(length).min(value.as_bytes().len());
    Ok(Typval::String(OxStr(value.as_bytes()[start..end].to_vec())))
}

fn escape(args: &[Typval]) -> Result<Typval> {
    let value = string_arg(&args[0])?;
    let chars = string_arg(&args[1])?;
    let mut result = Vec::with_capacity(value.as_bytes().len());
    for byte in value.as_bytes() {
        if chars.as_bytes().contains(byte) { result.push(b'\\'); }
        result.push(*byte);
    }
    Ok(Typval::String(OxStr(result)))
}

fn count(args: &[Typval]) -> Result<Typval> {
    let needle = &args[1];
    let ignore_case = args.get(2).is_some_and(Typval::is_truthy);
    let start = args.get(3).map(number_arg).transpose()?.unwrap_or(0).max(0) as usize;
    let total = match &args[0] {
        Typval::List(values) => {
            let mut total = 0;
            for value in values.iter().skip(start) { if values_equal(value, needle, ignore_case, 0)? { total += 1; } }
            total
        }
        Typval::Dict(values) => {
            let mut total = 0;
            for (_, value) in values.iter().skip(start) { if values_equal(value, needle, ignore_case, 0)? { total += 1; } }
            total
        }
        Typval::String(value) => {
            let needle = string_arg(needle)?;
            non_overlapping_count(value.as_bytes(), needle.as_bytes())
        }
        _ => return Err(EvalError::new("E706", 0, "List, Dictionary or String required")),
    };
    Ok(Typval::Number(saturating_i64(total)))
}

fn extend(mut args: Vec<Typval>) -> Result<Typval> {
    let mode = if args.len() > 2 { Some(string_arg(&args[2])?) } else { None };
    let right = args.get(1).cloned().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
    let left = args.get_mut(0).ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
    match (left, right) {
        (Typval::List(left), Typval::List(right)) => { left.extend(right); Ok(args.remove(0)) }
        (Typval::Dict(left), Typval::Dict(right)) => {
            let mode = mode.as_ref().map_or(b"force".as_slice(), OxStr::as_bytes);
            for (key, value) in right {
                if let Some((_, existing)) = left.iter_mut().find(|(candidate, _)| candidate == &key) {
                    match mode {
                        b"keep" => {}
                        b"error" => return Err(EvalError::new("E737", 0, format!("Key already exists: {}", key.to_string_lossy()))),
                        b"force" => *existing = value,
                        _ => return Err(EvalError::new("E475", 0, "Invalid argument")),
                    }
                } else { left.push((key, value)); }
            }
            Ok(args.remove(0))
        }
        _ => Err(EvalError::new("E712", 0, "Argument of extend() must be a List or Dictionary")),
    }
}

fn get(args: &[Typval]) -> Result<Typval> {
    let fallback = args.get(2).cloned().unwrap_or(Typval::Number(0));
    match &args[0] {
        Typval::List(values) => Ok(normalize_index(values.len(), number_arg(&args[1])?).and_then(|index| values.get(index)).cloned().unwrap_or(fallback)),
        Typval::Blob(values) => Ok(normalize_index(values.len(), number_arg(&args[1])?).and_then(|index| values.get(index)).map_or(fallback, |value| Typval::Number(i64::from(*value)))),
        Typval::Dict(values) => {
            let key = string_arg(&args[1])?;
            Ok(values.iter().find(|(candidate, _)| candidate == &key).map(|(_, value)| value.clone()).unwrap_or(fallback))
        }
        _ => Err(EvalError::new("E896", 0, "List, Dictionary or Blob required")),
    }
}

fn has_key(args: &[Typval]) -> Result<Typval> {
    let Typval::Dict(values) = &args[0] else { return Err(EvalError::new("E1206", 0, "Dictionary required")); };
    let key = string_arg(&args[1])?;
    Ok(Typval::Number(i64::from(values.iter().any(|(candidate, _)| candidate == &key))))
}

fn index(args: &[Typval]) -> Result<Typval> {
    let Typval::List(values) = &args[0] else { return Err(EvalError::new("E714", 0, "List required")); };
    let ignore_case = args.get(3).is_some_and(Typval::is_truthy);
    let start = args.get(2).map(number_arg).transpose()?.unwrap_or(0);
    let start = normalize_index(values.len(), start).unwrap_or(values.len());
    for (index, value) in values.iter().enumerate().skip(start) {
        if values_equal(value, &args[1], ignore_case, 0)? { return Ok(Typval::Number(saturating_i64(index))); }
    }
    Ok(Typval::Number(-1))
}

fn insert(mut args: Vec<Typval>) -> Result<Typval> {
    let index = if args.len() > 2 { number_arg(&args[2])? } else { 0 };
    let value = args.get(1).cloned().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
    match args.get_mut(0) {
        Some(Typval::List(values)) => {
            let index = if index < 0 { values.len().saturating_sub(index.unsigned_abs() as usize).saturating_add(1) } else { usize::try_from(index).unwrap_or(usize::MAX) };
            if index > values.len() { return Err(EvalError::new("E684", 0, "List index out of range")); }
            values.insert(index, value);
            Ok(args.remove(0))
        }
        Some(Typval::Blob(values)) => {
            let byte = u8::try_from(number_arg(&value)?).map_err(|_| EvalError::new("E1230", 0, "Blob value must be in range 0 to 255"))?;
            let index = usize::try_from(index.max(0)).unwrap_or(usize::MAX);
            if index > values.len() { return Err(EvalError::new("E979", 0, "Blob index out of range")); }
            values.insert(index, byte);
            Ok(args.remove(0))
        }
        _ => Err(EvalError::new("E899", 0, "List or Blob required")),
    }
}

enum Projection { Items, Keys, Values }

fn dict_projection(value: &Typval, projection: Projection) -> Result<Typval> {
    let Typval::Dict(values) = value else { return Err(EvalError::new("E1206", 0, "Dictionary required")); };
    Ok(Typval::List(values.iter().map(|(key, value)| match projection {
        Projection::Items => Typval::List(vec![Typval::String(key.clone()), value.clone()]),
        Projection::Keys => Typval::String(key.clone()),
        Projection::Values => value.clone(),
    }).collect()))
}

fn extremum(value: &Typval, maximum: bool) -> Result<Typval> {
    let values: Vec<&Typval> = match value {
        Typval::List(values) => values.iter().collect(),
        Typval::Dict(values) => values.iter().map(|(_, value)| value).collect(),
        _ => return Err(EvalError::new("E712", 0, "List or Dictionary required")),
    };
    if values.is_empty() { return Ok(Typval::Number(0)); }
    let mut selected = values[0];
    for value in &values[1..] {
        let ordering = compare_values(selected, value, 0)?;
        if (maximum && ordering == Ordering::Less) || (!maximum && ordering == Ordering::Greater) { selected = value; }
    }
    Ok(selected.clone())
}

fn range(args: &[Typval]) -> Result<Typval> {
    let (start, end, stride) = match args.len() {
        1 => (0, number_arg(&args[0])? - 1, 1),
        2 => (number_arg(&args[0])?, number_arg(&args[1])?, 1),
        _ => (number_arg(&args[0])?, number_arg(&args[1])?, number_arg(&args[2])?),
    };
    if stride == 0 { return Err(EvalError::new("E726", 0, "Stride is zero")); }
    let mut values = Vec::new();
    let mut current = start;
    while (stride > 0 && current <= end) || (stride < 0 && current >= end) {
        values.push(Typval::Number(current));
        let Some(next) = current.checked_add(stride) else { break };
        current = next;
        if values.len() > 1_000_000 { return Err(EvalError::new("E1240", 0, "Resulting List too long")); }
    }
    Ok(Typval::List(values))
}

fn remove(mut args: Vec<Typval>) -> Result<Typval> {
    let first = number_arg(&args[1])?;
    let last = args.get(2).map(number_arg).transpose()?;
    let dict_key = string_arg(&args[1]).ok();
    match args.get_mut(0) {
        Some(Typval::List(values)) => {
            let index = normalize_index(values.len(), first).ok_or_else(|| EvalError::new("E684", 0, "List index out of range"))?;
            if let Some(last) = last {
                let end = normalize_index(values.len(), last).ok_or_else(|| EvalError::new("E684", 0, "List index out of range"))?;
                if end < index { return Err(EvalError::new("E16", 0, "Invalid range")); }
                Ok(Typval::List(values.drain(index..=end).collect()))
            } else { Ok(values.remove(index)) }
        }
        Some(Typval::Blob(values)) => {
            let index = normalize_index(values.len(), first).ok_or_else(|| EvalError::new("E979", 0, "Blob index out of range"))?;
            if let Some(last) = last {
                let end = normalize_index(values.len(), last).ok_or_else(|| EvalError::new("E979", 0, "Blob index out of range"))?;
                Ok(Typval::Blob(values.drain(index..=end).collect()))
            } else { Ok(Typval::Number(i64::from(values.remove(index)))) }
        }
        Some(Typval::Dict(values)) => {
            let key = dict_key.ok_or_else(|| EvalError::new("E731", 0, "Dictionary key must be a String"))?;
            let index = values.iter().position(|(candidate, _)| candidate == &key).ok_or_else(|| EvalError::new("E716", 0, "Key not present in Dictionary"))?;
            Ok(values.remove(index).1)
        }
        _ => Err(EvalError::new("E896", 0, "List, Dictionary or Blob required")),
    }
}

fn uniq(mut args: Vec<Typval>) -> Result<Typval> {
    let Some(Typval::List(values)) = args.get_mut(0) else { return Err(EvalError::new("E714", 0, "List required")); };
    let mut index = 1;
    while index < values.len() {
        if values_equal(&values[index - 1], &values[index], false, 0)? { values.remove(index); } else { index += 1; }
    }
    Ok(args.remove(0))
}

fn flatten(args: &[Typval]) -> Result<Typval> {
    let Typval::List(values) = &args[0] else { return Err(EvalError::new("E686", 0, "Argument of flatten() must be a List")); };
    let maximum = args.get(1).map(number_arg).transpose()?.unwrap_or(i64::MAX);
    let mut output = Vec::new();
    flatten_into(values, maximum, 0, &mut output)?;
    Ok(Typval::List(output))
}

fn flatten_into(values: &[Typval], maximum: i64, depth: usize, output: &mut Vec<Typval>) -> Result<()> {
    if depth >= MAX_CONTAINER_DEPTH { return Err(EvalError::new("E724", 0, "too much recursion")); }
    for value in values {
        if let Typval::List(nested) = value {
            if maximum > 0 { flatten_into(nested, maximum - 1, depth + 1, output)?; } else { output.push(value.clone()); }
        } else { output.push(value.clone()); }
    }
    Ok(())
}

fn deep_copy(value: &Typval, depth: usize) -> Result<Typval> {
    if depth >= MAX_CONTAINER_DEPTH { return Err(EvalError::new("E724", 0, "too much recursion in deepcopy()")); }
    match value {
        Typval::List(values) => values.iter().map(|value| deep_copy(value, depth + 1)).collect::<Result<Vec<_>>>().map(Typval::List),
        Typval::Dict(values) => values.iter().map(|(key, value)| deep_copy(value, depth + 1).map(|value| (key.clone(), value))).collect::<Result<Vec<_>>>().map(Typval::Dict),
        _ => Ok(value.clone()),
    }
}

fn blob2list(value: &Typval) -> Result<Typval> {
    let Typval::Blob(values) = value else { return Err(EvalError::new("E972", 0, "Blob required")); };
    Ok(Typval::List(values.iter().map(|value| Typval::Number(i64::from(*value))).collect()))
}

fn list2blob(value: &Typval) -> Result<Typval> {
    let Typval::List(values) = value else { return Err(EvalError::new("E714", 0, "List required")); };
    values.iter().map(|value| u8::try_from(number_arg(value)?).map_err(|_| EvalError::new("E1230", 0, "Blob value must be in range 0 to 255"))).collect::<Result<Vec<_>>>().map(Typval::Blob)
}

fn list2str(args: &[Typval]) -> Result<Typval> {
    let Typval::List(values) = &args[0] else { return Err(EvalError::new("E714", 0, "List required")); };
    let utf8 = args.get(1).is_some_and(Typval::is_truthy);
    let mut output = Vec::new();
    for value in values {
        let number = number_arg(value)?;
        if utf8 {
            let character = u32::try_from(number).ok().and_then(char::from_u32).ok_or_else(|| EvalError::new("E1280", 0, "Illegal character code"))?;
            let mut encoded = [0; 4];
            output.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        } else { output.push(number as u8); }
    }
    Ok(Typval::String(OxStr(output)))
}

fn str2list(args: &[Typval]) -> Result<Typval> {
    let value = string_arg(&args[0])?;
    let utf8 = args.get(1).is_some_and(Typval::is_truthy);
    let values = if utf8 {
        String::from_utf8_lossy(value.as_bytes()).chars().map(|character| Typval::Number(i64::from(u32::from(character)))).collect()
    } else { value.as_bytes().iter().map(|byte| Typval::Number(i64::from(*byte))).collect() };
    Ok(Typval::List(values))
}

fn char2nr(args: &[Typval]) -> Result<Typval> {
    let value = string_arg(&args[0])?;
    if value.as_bytes().is_empty() { return Ok(Typval::Number(0)); }
    let utf8 = !args.get(1).is_some_and(|value| !value.is_truthy());
    let number = if utf8 { String::from_utf8_lossy(value.as_bytes()).chars().next().map_or(0, |character| i64::from(u32::from(character))) } else { i64::from(value.as_bytes()[0]) };
    Ok(Typval::Number(number))
}

fn nr2char(args: &[Typval]) -> Result<Typval> {
    let number = number_arg(&args[0])?;
    let utf8 = !args.get(1).is_some_and(|value| !value.is_truthy());
    if !utf8 { return Ok(Typval::String(OxStr(vec![number as u8]))); }
    let scalar = u32::try_from(number).ok().and_then(char::from_u32).ok_or_else(|| EvalError::new("E1280", 0, "Illegal character code"))?;
    let mut buffer = [0; 4];
    Ok(Typval::String(OxStr(scalar.encode_utf8(&mut buffer).as_bytes().to_vec())))
}

fn str2nr(args: &[Typval]) -> Result<Typval> {
    let value = string_arg(&args[0])?;
    let base = args.get(1).map(number_arg).transpose()?.unwrap_or(10);
    // Only the bases 2, 8, 10 and 16 are accepted; any other explicit base is
    // an error (`f_str2nr`, strings.c:2593-2598). Note that base 0 (which used
    // to trigger auto-detection here) is rejected, exactly as upstream.
    if !matches!(base, 2 | 8 | 10 | 16) {
        return Err(EvalError::new("E474", 0, "Invalid argument"));
    }
    let quoted = args.get(2).is_some_and(Typval::is_truthy);
    let number = parse_vim_number(value.as_bytes(), base, quoted)?;
    Ok(Typval::Number(number))
}

fn str2float(value: &Typval) -> Result<Typval> {
    let value = string_arg(value)?;
    let text = String::from_utf8_lossy(value.as_bytes());
    let prefix: String = text.trim_start().chars().take_while(|character| character.is_ascii_digit() || matches!(character, '+' | '-' | '.' | 'e' | 'E')).collect();
    Ok(Typval::Float(prefix.parse().unwrap_or(0.0)))
}

/// "str2nr()" digit conversion following upstream `vim_str2nr`
/// (charset.c:1219-1300) with the STR2NR_FORCE flag set by `f_str2nr`
/// (strings.c:2589-2633).
///
/// Whitespace is skipped before the optional sign and again after it, so
/// `str2nr(" - 42 ")` is `-42`. A base marker ("0x"/"0X" for 16, "0b"/"0B"
/// for 2, "0o"/"0O" for 8) is consumed as a prefix only when a digit valid in
/// that base follows it. Base 10 has no prefixes and is parsed as plain
/// decimal, so `str2nr("0xff")` is `0`. Text after the number is ignored.
fn parse_vim_number(bytes: &[u8], base: i64, quoted: bool) -> Result<i64> {
    let mut digits: Vec<u8> = bytes.iter().copied().skip_while(u8::is_ascii_whitespace).collect();
    let negative = digits.first() == Some(&b'-');
    if matches!(digits.first(), Some(b'-' | b'+')) {
        digits.drain(..1);
        while digits.first().is_some_and(u8::is_ascii_whitespace) { digits.remove(0); }
    }
    if quoted { digits.retain(|byte| *byte != b'\''); }
    let digits: &[u8] = &digits;
    // STR2NR_FORCE: skip the base marker only when a valid digit follows it.
    let digits = match base {
        16 if digits.len() > 2 && (digits.starts_with(b"0x") || digits.starts_with(b"0X")) && digits[2].is_ascii_hexdigit() => &digits[2..],
        2 if digits.len() > 2 && (digits.starts_with(b"0b") || digits.starts_with(b"0B")) && matches!(digits[2], b'0' | b'1') => &digits[2..],
        8 if digits.len() > 2 && (digits.starts_with(b"0o") || digits.starts_with(b"0O")) && matches!(digits[2], b'0'..=b'7') => &digits[2..],
        _ => digits,
    };
    let magnitude = parse_integer_prefix(digits, base as u32).unwrap_or(0);
    Ok(if negative { magnitude.saturating_neg() } else { magnitude })
}

fn parse_integer_prefix(bytes: &[u8], base: u32) -> Option<i64> {
    let mut value = 0i64;
    let mut seen = false;
    for byte in bytes {
        let Some(digit) = (*byte as char).to_digit(base) else { break };
        seen = true;
        value = value.saturating_mul(i64::from(base)).saturating_add(i64::from(digit));
    }
    seen.then_some(value)
}

fn json_encode(value: &Typval) -> Result<Typval> {
    let mut output = String::new();
    encode_json(value, 0, &mut output)?;
    Ok(Typval::String(OxStr(output.into_bytes())))
}

fn encode_json(value: &Typval, depth: usize, output: &mut String) -> Result<()> {
    if depth >= MAX_CONTAINER_DEPTH { return Err(EvalError::new("E724", 0, "too much recursion in json_encode()")); }
    match value {
        Typval::Special(Special::Null) => output.push_str("null"),
        Typval::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Typval::Number(value) => { let _ = write!(output, "{value}"); }
        Typval::Float(value) => {
            let number = serde_json::Number::from_f64(*value).ok_or_else(|| EvalError::new("E474", 0, "NaN or Infinity cannot be JSON encoded"))?;
            output.push_str(&number.to_string());
        }
        Typval::String(value) => output.push_str(&serde_json::to_string(&String::from_utf8_lossy(value.as_bytes())).map_err(|error| EvalError::new("E474", 0, error.to_string()))?),
        Typval::Blob(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() { if index > 0 { output.push(','); } let _ = write!(output, "{value}"); }
            output.push(']');
        }
        Typval::List(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() { if index > 0 { output.push(','); } encode_json(value, depth + 1, output)?; }
            output.push(']');
        }
        Typval::Dict(values) => {
            output.push('{');
            for (index, (key, value)) in values.iter().enumerate() {
                if index > 0 { output.push(','); }
                output.push_str(&serde_json::to_string(&String::from_utf8_lossy(key.as_bytes())).map_err(|error| EvalError::new("E474", 0, error.to_string()))?);
                output.push(':');
                encode_json(value, depth + 1, output)?;
            }
            output.push('}');
        }
        _ => return Err(EvalError::new("E474", 0, "value cannot be JSON encoded")),
    }
    Ok(())
}

fn json_decode(value: &Typval) -> Result<Typval> {
    let value = string_arg(value)?;
    let decoded: JsonValue = serde_json::from_slice(value.as_bytes()).map_err(|error| EvalError::new("E474", error.column(), format!("Invalid JSON: {error}")))?;
    json_to_typval(decoded, 0)
}

fn json_to_typval(value: JsonValue, depth: usize) -> Result<Typval> {
    if depth >= MAX_CONTAINER_DEPTH { return Err(EvalError::new("E724", 0, "too much recursion in json_decode()")); }
    match value {
        JsonValue::Null => Ok(Typval::Special(Special::Null)),
        JsonValue::Bool(value) => Ok(Typval::Bool(value)),
        JsonValue::Number(value) => value.as_i64().map(Typval::Number).or_else(|| value.as_f64().map(Typval::Float)).ok_or_else(|| EvalError::new("E474", 0, "JSON number is out of range")),
        JsonValue::String(value) => Ok(Typval::String(OxStr(value.into_bytes()))),
        JsonValue::Array(values) => values.into_iter().map(|value| json_to_typval(value, depth + 1)).collect::<Result<Vec<_>>>().map(Typval::List),
        JsonValue::Object(values) => values.into_iter().map(|(key, value)| json_to_typval(value, depth + 1).map(|value| (OxStr(key.into_bytes()), value))).collect::<Result<Vec<_>>>().map(Typval::Dict),
    }
}

fn vim_string(value: &Typval, depth: usize) -> Result<OxStr> {
    if depth >= MAX_CONTAINER_DEPTH { return Err(EvalError::new("E724", 0, "too much recursion in string()")); }
    let mut output = Vec::new();
    match value {
        Typval::String(value) => { output.push(b'\''); for byte in value.as_bytes() { output.push(*byte); if *byte == b'\'' { output.push(b'\''); } } output.push(b'\''); }
        Typval::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        Typval::Float(value) => output.extend_from_slice(value.to_string().as_bytes()),
        Typval::Bool(value) => output.extend_from_slice(if *value { b"v:true" } else { b"v:false" }),
        Typval::Special(Special::Null) => output.extend_from_slice(b"v:null"),
        Typval::Blob(value) => { output.extend_from_slice(b"0z"); for byte in value { let _ = write!(StringWriter(&mut output), "{byte:02X}"); } }
        Typval::List(values) => {
            output.push(b'['); for (index, value) in values.iter().enumerate() { if index > 0 { output.extend_from_slice(b", "); } output.extend_from_slice(vim_string(value, depth + 1)?.as_bytes()); } output.push(b']');
        }
        Typval::Dict(values) => {
            output.push(b'{'); for (index, (key, value)) in values.iter().enumerate() { if index > 0 { output.extend_from_slice(b", "); } output.extend_from_slice(vim_string(&Typval::String(key.clone()), depth + 1)?.as_bytes()); output.extend_from_slice(b": "); output.extend_from_slice(vim_string(value, depth + 1)?.as_bytes()); } output.push(b'}');
        }
        Typval::Funcref(Funcref { name, .. }) => { output.extend_from_slice(b"function('"); output.extend_from_slice(name.as_bytes()); output.extend_from_slice(b"')"); }
        Typval::Partial(Funcref { name, .. }) => { output.extend_from_slice(b"function('"); output.extend_from_slice(name.as_bytes()); output.extend_from_slice(b"')"); }
        Typval::Channel(value) | Typval::Job(value) => output.extend_from_slice(value.to_string().as_bytes()),
    }
    Ok(OxStr(output))
}

struct StringWriter<'a>(&'a mut Vec<u8>);
impl std::fmt::Write for StringWriter<'_> { fn write_str(&mut self, value: &str) -> std::fmt::Result { self.0.extend_from_slice(value.as_bytes()); Ok(()) } }

fn values_equal(left: &Typval, right: &Typval, ignore_case: bool, depth: usize) -> Result<bool> {
    if depth >= MAX_CONTAINER_DEPTH { return Err(EvalError::new("E724", 0, "too much recursion comparing values")); }
    match (left, right) {
        (Typval::Number(left), Typval::Number(right)) => Ok(left == right),
        (Typval::Float(left), Typval::Float(right)) => Ok(left == right),
        (Typval::String(left), Typval::String(right)) => Ok(if ignore_case { left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase() } else { left == right }),
        (Typval::Bool(left), Typval::Bool(right)) => Ok(left == right),
        (Typval::Special(left), Typval::Special(right)) => Ok(left == right),
        (Typval::Blob(left), Typval::Blob(right)) => Ok(left == right),
        (Typval::List(left), Typval::List(right)) => {
            if left.len() != right.len() { return Ok(false); }
            for (left, right) in left.iter().zip(right) { if !values_equal(left, right, ignore_case, depth + 1)? { return Ok(false); } }
            Ok(true)
        }
        (Typval::Dict(left), Typval::Dict(right)) => {
            if left.len() != right.len() { return Ok(false); }
            for (key, left) in left {
                let Some((_, right)) = right.iter().find(|(candidate, _)| candidate == key) else { return Ok(false) };
                if !values_equal(left, right, ignore_case, depth + 1)? { return Ok(false); }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn compare_values(left: &Typval, right: &Typval, depth: usize) -> Result<Ordering> {
    if depth >= MAX_CONTAINER_DEPTH { return Err(EvalError::new("E724", 0, "too much recursion comparing values")); }
    match (left, right) {
        (Typval::Number(left), Typval::Number(right)) => Ok(left.cmp(right)),
        (Typval::Float(left), Typval::Float(right)) => Ok(left.total_cmp(right)),
        (Typval::Number(left), Typval::Float(right)) => Ok((*left as f64).total_cmp(right)),
        (Typval::Float(left), Typval::Number(right)) => Ok(left.total_cmp(&(*right as f64))),
        _ => compare_strings(left, right, false),
    }
}

fn compare_strings(left: &Typval, right: &Typval, ignore_case: bool) -> Result<Ordering> {
    let left = string_arg(left)?; let right = string_arg(right)?;
    if ignore_case { Ok(left.to_string_lossy().to_lowercase().cmp(&right.to_string_lossy().to_lowercase())) } else { Ok(left.as_bytes().cmp(right.as_bytes())) }
}

fn normalize_index(length: usize, index: i64) -> Option<usize> {
    if index >= 0 { usize::try_from(index).ok().filter(|index| *index < length) } else { usize::try_from(index.unsigned_abs()).ok().and_then(|distance| length.checked_sub(distance)) }
}
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> { if needle.is_empty() { Some(0) } else { haystack.windows(needle.len()).position(|window| window == needle) } }
fn find_subslice_reverse(haystack: &[u8], needle: &[u8]) -> Option<usize> { if needle.is_empty() { Some(haystack.len()) } else { haystack.windows(needle.len()).rposition(|window| window == needle) } }
fn non_overlapping_count(haystack: &[u8], needle: &[u8]) -> usize { if needle.is_empty() { return 0; } let mut count = 0; let mut offset = 0; while let Some(position) = find_subslice(&haystack[offset..], needle) { count += 1; offset += position + needle.len(); } count }
fn saturating_i64(value: usize) -> i64 { i64::try_from(value).unwrap_or(i64::MAX) }
fn set_pair(values: &mut Vec<(OxStr, Typval)>, key: &[u8], value: Typval) { if let Some((_, existing)) = values.iter_mut().find(|(candidate, _)| candidate.as_bytes() == key) { *existing = value; } else { values.push((OxStr(key.to_vec()), value)); } }
