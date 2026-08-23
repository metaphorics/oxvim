//! Generated builtin metadata and typval-only builtin implementations.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::rc::Rc;

use ox_types::{Funcref, OxStr, Special, Typval};
use serde_json::Value as JsonValue;
use unicode_width::UnicodeWidthChar;

use crate::error::{EvalError, Result};
use crate::eval::{compare_bytes, BuiltinHost, BufferHost, ClosureRegistry, Evaluator, RegexEngine};
use crate::parser::Parser;
use crate::path_builtins;
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
    closures: ClosureRegistry,
    ambiguous_wide: bool,
}

impl<'a> Builtins<'a> {
    /// Create a dispatcher with regex-backed builtins enabled through `regex`.
    #[must_use]
    pub fn new(regex: &'a dyn RegexEngine) -> Self {
        Self { regex: Some(regex), closures: ClosureRegistry::new(), ambiguous_wide: false }
    }

    /// Create a dispatcher whose regex-backed operations return a typed error.
    #[must_use]
    pub fn without_regex() -> Self {
        Self { regex: None, closures: ClosureRegistry::new(), ambiguous_wide: false }
    }

    /// Use double-cell widths for East-Asian ambiguous characters.
    #[must_use]
    pub fn with_ambiguous_width(mut self, wide: bool) -> Self {
        self.ambiguous_wide = wide;
        self
    }

    /// Reuse the evaluator's closure registry for callback-capable builtins.
    #[must_use]
    pub fn with_closure_registry(mut self, registry: ClosureRegistry) -> Self {
        self.closures = registry;
        self
    }

    /// Borrow the shared closure registry.
    #[must_use]
    pub const fn closure_registry(&self) -> &ClosureRegistry { &self.closures }

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
            "copy" => shallow_copy(&args[0]),
            "count" => count(&args),
            "deepcopy" => deep_copy(&args[0]),
            "empty" => Ok(Typval::Number(i64::from(is_empty(&args[0])))),
            "escape" => escape(&args),
            "executable" => path_builtins::executable(&args[0]),
            "exepath" => path_builtins::exepath(&args[0]),
            "exists" => exists(&args[0], scope),
            "extend" | "extendnew" => extend(args),
            "filter" => self.filter_or_map(args, scope, CollectionOp::Filter),
            "flatten" => flatten(&args, true),
            "flattennew" => flatten(&args, false),
            "float2nr" | "trunc" => float_to_number(&args[0]),
            "floor" => float_unary(&args[0], f64::floor),
            "fnamemodify" => path_builtins::fnamemodify(self.regex, &args[0], &args[1]),
            "get" => get(&args),
            "gettext" => gettext(&args[0]),
            "getcwd" => path_builtins::getcwd(&args),
            "getpid" => Ok(Typval::Number(i64::from(std::process::id()))),
            "has" => has_feature(&args),
            "has_key" => has_key(&args),
            "hostname" => hostname(),
            "index" => index(&args),
            "indexof" => self.indexof(&args, scope),
            "insert" => insert(args),
            "isabsolutepath" => path_builtins::is_absolute_path(&args[0]),
            "islocked" => is_locked_value(&args[0]),
            "items" => dict_projection(&args[0], Projection::Items),
            "join" => join(&args),
            "keytrans" => keytrans(&args[0]),
            "json_decode" => json_decode(&args[0]),
            "json_encode" => json_encode(&args[0]),
            "keys" => dict_projection(&args[0], Projection::Keys),
            "len" | "strlen" => length(&args[0], name == "strlen"),
            "list2blob" => list2blob(&args[0]),
            "list2str" => list2str(&args),
            "map" => self.filter_or_map(args, scope, CollectionOp::Map),
            "mapnew" => self.filter_or_map(args, scope, CollectionOp::MapNew),
            "foreach" => self.filter_or_map(args, scope, CollectionOp::ForEach),
            "reduce" => self.reduce(args, scope),
            "match" | "matchend" | "matchstr" => self.regex_match(name, &args),
            "matchlist" | "matchstrpos" => self.regex_result(name, &args),
            "max" => extremum(&args[0], true),
            "min" => extremum(&args[0], false),
            "nr2char" => nr2char(&args),
            "or" => binary_number(&args, |left, right| left | right),
            "pow" => float_binary(&args, f64::powf),
            "printf" => printf_builtin(&args),
            "range" => range(&args),
            "remove" => remove(args),
            "repeat" => repeat(&args),
            "resolve" => path_builtins::resolve(&args[0]),
            "pathshorten" => pathshorten(&args),
            "reverse" => reverse(args),
            "setenv" => setenv(&args),
            "simplify" => path_builtins::simplify(&args[0]),
            "slice" => slice(&args),
            "sort" => self.sort(args, scope),
            "split" => self.regex_split(&args),
            "sqrt" => float_unary(&args[0], f64::sqrt),
            "str2float" => str2float(&args[0]),
            "str2list" => str2list(&args),
            "str2nr" => str2nr(&args),
            "strcharlen" => strcharlen(&args[0]),
            "strchars" => strchars(&args),
            "strtrans" => strtrans(&args[0]),
            "strutf16len" => strutf16len(&args),
            "strwidth" => strwidth(&args[0], self.ambiguous_wide),
            "stridx" => string_index(&args, false),
            "string" => Ok(Typval::String(vim_string(&args[0], 0)?)),
            "strpart" => strpart(&args),
            "strridx" => string_index(&args, true),
            "substitute" => self.regex_substitute(&args),
            "tolower" => change_case(&args[0], false),
            "toupper" => change_case(&args[0], true),
            "trim" => trim(&args),
            "tr" => translate(&args),
            "utf16idx" => utf16idx(&args),
            "charidx" => charidx(&args),
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
        if args.len() == 1 {
            let parts = text
                .as_bytes()
                .split(u8::is_ascii_whitespace)
                .filter(|part| !part.is_empty())
                .map(|part| Typval::String(OxStr(part.to_vec())))
                .collect();
            return Ok(Typval::list(parts));
        }
        let pattern = string_arg(&args[1])?;
        let keep_empty = args.get(2).is_some_and(Typval::is_truthy);
        self.regex()?.split(&text, &pattern, keep_empty).map(|parts| {
            Typval::list(parts.into_iter().map(Typval::String).collect())
        })
    }

    fn regex_match(&self, name: &str, args: &[Typval]) -> Result<Typval> {
        let pattern = string_arg(&args[1])?;
        let start_number = args.get(2).map(number_arg).transpose()?.unwrap_or(0).max(0);
        let occurrence = args.get(3).map(number_arg).transpose()?.unwrap_or(1).max(1) as usize;
        match &args[0] {
            Typval::List(values) => {
                let values = list_items(values)?;
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

    fn regex_result(&self, name: &str, args: &[Typval]) -> Result<Typval> {
        let pattern = string_arg(&args[1])?;
        let start_number = args.get(2).map(number_arg).transpose()?.unwrap_or(0).max(0);
        match &args[0] {
            Typval::List(values) if name == "matchstrpos" => {
                let values = list_items(values)?;
                for (index, value) in values.iter().enumerate().skip(usize::try_from(start_number).unwrap_or(usize::MAX)) {
                    let text = string_arg(value)?;
                    if let Some(found) = self.regex()?.find_captures(&text, &pattern, 0)? {
                        return Ok(Typval::list(vec![Typval::String(OxStr(text.as_bytes()[found.start..found.end].to_vec())), Typval::Number(saturating_i64(index)), Typval::Number(saturating_i64(found.start)), Typval::Number(saturating_i64(found.end))]));
                    }
                }
                Ok(Typval::list(vec![Typval::String(OxStr(Vec::new())), Typval::Number(-1), Typval::Number(-1), Typval::Number(-1)]))
            }
            Typval::List(_) => Err(EvalError::new("E730", 0, "Using a List as a String")),
            value => {
                let text = string_arg(value)?;
                let found = self.regex()?.find_captures(&text, &pattern, usize::try_from(start_number).unwrap_or(usize::MAX))?;
                if name == "matchstrpos" {
                    return Ok(match found {
                        Some(found) => Typval::list(vec![Typval::String(OxStr(text.as_bytes()[found.start..found.end].to_vec())), Typval::Number(saturating_i64(found.start)), Typval::Number(saturating_i64(found.end))]),
                        None => Typval::list(vec![Typval::String(OxStr(Vec::new())), Typval::Number(-1), Typval::Number(-1)]),
                    });
                }
                let Some(found) = found else { return Ok(Typval::list(Vec::new())); };
                let mut result = Vec::with_capacity(10);
                result.push(Typval::String(OxStr(text.as_bytes()[found.start..found.end].to_vec())));
                for capture in found.captures.into_iter().take(9) {
                    result.push(Typval::String(capture.map_or_else(|| OxStr(Vec::new()), |(start, end)| OxStr(text.as_bytes()[start..end].to_vec()))));
                }
                while result.len() < 10 { result.push(Typval::String(OxStr(Vec::new()))); }
                Ok(Typval::list(result))
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

    fn filter_or_map(&mut self, mut args: Vec<Typval>, scope: &mut Scope, operation: CollectionOp) -> Result<Typval> {
        let callback = args.pop().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
        let container = args.pop().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
        match &container {
            Typval::List(reference) => {
                let (items, previous_lock) = {
                    let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                    if data.lock.locked && matches!(operation, CollectionOp::Map | CollectionOp::Filter) { return Err(locked_error()); }
                    let items = data.items.clone();
                    let previous = data.lock;
                    data.lock.locked = true;
                    (items, previous)
                };
                let mut output = Vec::with_capacity(items.len());
                let mut current_index = 0usize;
                let evaluated = (|| {
                    for (callback_index, value) in items.iter().cloned().enumerate() {
                        let mapped = self.eval_callback(&callback, Typval::Number(saturating_i64(callback_index)), value, scope)?;
                        match operation {
                            CollectionOp::Map => {
                                let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                                let Some(slot) = data.items.get_mut(current_index) else { return Err(borrow_error()); };
                                *slot = mapped;
                                current_index += 1;
                            }
                            CollectionOp::Filter => {
                                let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                                if mapped.is_truthy() { current_index += 1; } else if current_index < data.items.len() { data.items.remove(current_index); }
                            }
                            CollectionOp::MapNew => output.push(mapped),
                            CollectionOp::ForEach => {}
                        }
                    }
                    Ok(())
                })();
                reference.try_borrow_mut().map_err(|_| borrow_error())?.lock = previous_lock;
                evaluated?;
                Ok(match operation {
                    CollectionOp::MapNew => Typval::list(output),
                    CollectionOp::Map | CollectionOp::Filter | CollectionOp::ForEach => container,
                })
            }
            Typval::Dict(reference) => {
                let (entries, previous_lock) = {
                    let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                    if data.lock.locked && matches!(operation, CollectionOp::Map | CollectionOp::Filter) { return Err(locked_error()); }
                    let entries = data.entries.clone();
                    let previous = data.lock;
                    data.lock.locked = true;
                    (entries, previous)
                };
                let mut output = Vec::with_capacity(entries.len());
                let evaluated = (|| {
                    for (key, value) in entries.iter().cloned() {
                        let mapped = self.eval_callback(&callback, Typval::String(key.clone()), value, scope)?;
                        match operation {
                            CollectionOp::Map => {
                                let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                                let Some((_, slot)) = data.entries.iter_mut().find(|(candidate, _)| candidate == &key) else { return Err(borrow_error()); };
                                *slot = mapped;
                            }
                            CollectionOp::Filter => {
                                if !mapped.is_truthy() {
                                    let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                                    let Some(index) = data.entries.iter().position(|(candidate, _)| candidate == &key) else { return Err(borrow_error()); };
                                    data.entries.remove(index);
                                }
                            }
                            CollectionOp::MapNew => output.push((key, mapped)),
                            CollectionOp::ForEach => {}
                        }
                    }
                    Ok(())
                })();
                reference.try_borrow_mut().map_err(|_| borrow_error())?.lock = previous_lock;
                evaluated?;
                Ok(match operation {
                    CollectionOp::MapNew => Typval::dict(output),
                    CollectionOp::Map | CollectionOp::Filter | CollectionOp::ForEach => container,
                })
            }
            Typval::Blob(bytes) => {
                let mut output = Vec::with_capacity(bytes.len());
                for (index, byte) in bytes.iter().copied().enumerate() {
                    let mapped = self.eval_callback(&callback, Typval::Number(saturating_i64(index)), Typval::Number(i64::from(byte)), scope)?;
                    match operation {
                        CollectionOp::Map | CollectionOp::MapNew => output.push(u8::try_from(number_arg(&mapped)?).map_err(|_| EvalError::new("E1230", 0, "Blob value must be in range 0 to 255"))?),
                        CollectionOp::Filter if mapped.is_truthy() => output.push(byte),
                        CollectionOp::Filter | CollectionOp::ForEach => {}
                    }
                }
                Ok(match operation {
                    CollectionOp::ForEach => container,
                    CollectionOp::Map | CollectionOp::MapNew | CollectionOp::Filter => Typval::Blob(output),
                })
            }
            Typval::String(text) => {
                let characters = string_elements(text.as_bytes());
                let mut output = Vec::new();
                for (index, character) in characters.into_iter().enumerate() {
                    let original = Typval::String(character.clone());
                    let mapped = self.eval_callback(&callback, Typval::Number(saturating_i64(index)), original, scope)?;
                    match operation {
                        CollectionOp::Map | CollectionOp::MapNew => output.extend_from_slice(string_arg(&mapped)?.as_bytes()),
                        CollectionOp::Filter if mapped.is_truthy() => output.extend_from_slice(character.as_bytes()),
                        CollectionOp::Filter | CollectionOp::ForEach => {}
                    }
                }
                Ok(match operation {
                    CollectionOp::ForEach => container,
                    CollectionOp::Map | CollectionOp::MapNew | CollectionOp::Filter => Typval::String(OxStr(output)),
                })
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
            Typval::Funcref(_) | Typval::Partial(_) => {
                let regex = RegexRef(self.regex);
                Evaluator::new(self, &regex).invoke(callback.clone(), vec![key, value], &mut scope.snapshot())
            }
            _ => Err(EvalError::new("E921", 0, "Invalid callback argument")),
        }
    }

    /// `indexof()` — upstream `f_indexof` (eval/funcs.c 2961-3002): evaluate
    /// the callback with `v:key`/`v:val` for each List item or Blob byte
    /// starting at `opts.startidx` (negative counts from the end; out of range
    /// finds nothing) and return the first index whose result converts to a
    /// nonzero number (tv_get_bool_chk, funcs.c 2872), else -1. An empty or
    /// null-string callback never matches, and a callback error aborts the
    /// search like upstream's `did_emsg` check.
    fn indexof(&mut self, args: &[Typval], scope: &mut Scope) -> Result<Typval> {
        let callback = match &args[1] {
            Typval::String(expression) if expression.as_bytes().is_empty() => return Ok(Typval::Number(-1)),
            Typval::Special(Special::Null) => return Ok(Typval::Number(-1)),
            Typval::String(_) | Typval::Funcref(_) | Typval::Partial(_) => args[1].clone(),
            _ => return Err(EvalError::new("E1256", 0, "String or function required for argument 2")),
        };
        let startidx = match args.get(2) {
            None | Some(Typval::Special(Special::Null)) => 0,
            Some(Typval::Dict(reference)) => dict_entries(reference)?
                .iter()
                .find(|(key, _)| key.as_bytes() == b"startidx")
                .map_or(0, |(_, value)| number_arg(value).unwrap_or(0)),
            Some(_) => return Err(EvalError::new("E1206", 0, "Dictionary required for argument 3")),
        };
        let found = match &args[0] {
            Typval::List(reference) => {
                let items = list_items(reference)?;
                let start = normalize_index(items.len(), startidx).unwrap_or(items.len());
                let mut found = -1;
                for (index, value) in items.into_iter().enumerate().skip(start) {
                    let matched = self
                        .eval_callback(&callback, Typval::Number(saturating_i64(index)), value, scope)
                        .and_then(|result| number_arg(&result).map(|number| number != 0))?;
                    if matched {
                        found = saturating_i64(index);
                        break;
                    }
                }
                found
            }
            Typval::Blob(bytes) => {
                let start = normalize_index(bytes.len(), startidx).unwrap_or(bytes.len());
                let mut found = -1;
                for (index, byte) in bytes.iter().copied().enumerate().skip(start) {
                    let matched = self
                        .eval_callback(&callback, Typval::Number(saturating_i64(index)), Typval::Number(i64::from(byte)), scope)
                        .and_then(|result| number_arg(&result).map(|number| number != 0))?;
                    if matched {
                        found = saturating_i64(index);
                        break;
                    }
                }
                found
            }
            _ => return Err(EvalError::new("E1226", 0, "List or Blob required for argument 1")),
        };
        Ok(Typval::Number(found))
    }

    fn sort(&mut self, args: Vec<Typval>, scope: &mut Scope) -> Result<Typval> {
        let Some(Typval::List(reference)) = args.first() else {
            return Err(EvalError::new("E714", 0, "List required"));
        };
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
                _ => SortMode::Callback(args[1].clone()),
            },
            Some(Typval::Funcref(_) | Typval::Partial(_)) => SortMode::Callback(args[1].clone()),
            _ => return Err(EvalError::new("E921", 0, "Invalid callback argument")),
        };
        let (mut values, previous_lock) = {
            let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
            if data.lock.locked { return Err(locked_error()); }
            let values = data.items.clone();
            let previous = data.lock;
            data.lock.locked = true;
            (values, previous)
        };
        let mut failure = None;
        values.sort_by(|left, right| {
            if failure.is_some() {
                return Ordering::Equal;
            }
            let result = match &mode {
                SortMode::Default | SortMode::Locale => Ok(sort_string_pair(left, right, false)),
                SortMode::IgnoreCase => Ok(sort_string_pair(left, right, true)),
                SortMode::Numeric => Ok(sort_numeric(left).total_cmp(&sort_numeric(right))),
                SortMode::Integer => Ok(sort_integer(left).cmp(&sort_integer(right))),
                SortMode::Float => Ok(sort_float(left).total_cmp(&sort_float(right))),
                SortMode::Callback(callback) => self.eval_callback(callback, left.clone(), right.clone(), scope)
                    .and_then(|value| number_arg(&value)).map(|value| value.cmp(&0)),
            };
            match result {
                Ok(ordering) => ordering,
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                    Ordering::Equal
                }
            }
        });
        reference.try_borrow_mut().map_err(|_| borrow_error())?.lock = previous_lock;
        if let Some(error) = failure { return Err(error); }
        reference.try_borrow_mut().map_err(|_| borrow_error())?.items = values;
        Ok(args[0].clone())
    }

    fn reduce(&mut self, args: Vec<Typval>, scope: &mut Scope) -> Result<Typval> {
        let source = args.first().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
        let callback = args.get(1).ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
        let mut previous_lock = None;
        let mut items = match source {
            Typval::List(reference) => {
                let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                let items = data.items.clone();
                previous_lock = Some((reference.clone(), data.lock));
                data.lock.locked = true;
                items
            }
            Typval::Blob(bytes) => bytes.iter().map(|byte| Typval::Number(i64::from(*byte))).collect(),
            Typval::String(text) => string_elements(text.as_bytes()).into_iter().map(Typval::String).collect(),
            _ => return Err(EvalError::new("E1252", 0, "String, List or Blob required")),
        };
        let reduced = (|| {
            let mut accumulator = if let Some(initial) = args.get(2) {
                initial.clone()
            } else if items.is_empty() {
                return Err(EvalError::new("E998", 0, "Reduce of an empty value with no initial value"));
            } else {
                items.remove(0)
            };
            for item in items { accumulator = self.eval_callback(callback, accumulator, item, scope)?; }
            Ok(accumulator)
        })();
        if let Some((reference, lock)) = previous_lock {
            reference.try_borrow_mut().map_err(|_| borrow_error())?.lock = lock;
        }
        reduced
    }

}

#[derive(Clone, Copy)]
enum CollectionOp { Map, Filter, MapNew, ForEach }

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

    fn closure_registry(&self) -> Option<ClosureRegistry> { Some(self.closures.clone()) }
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
    fn find_captures(&self, text: &OxStr, pattern: &OxStr, start: usize) -> Result<Option<crate::eval::RegexMatch>> {
        self.0.ok_or_else(|| EvalError::new("E54", 0, "regular-expression engine is not installed"))?.find_captures(text, pattern, start)
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
        "deepcopy" | "empty" | "escape" | "executable" | "exepath" | "exists" | "extend" | "extendnew" | "filter" | "flatten" |
        "flattennew" | "foreach" | "float2nr" | "floor" | "fnamemodify" | "get" | "gettext" | "getcwd" | "getpid" | "has" | "has_key" | "hostname" | "index" | "insert" | "items" |
        "indexof" | "isabsolutepath" | "islocked" | "join" | "json_decode" | "json_encode" | "keytrans" | "keys" | "len" | "strlen" | "list2blob" | "list2str" | "map" | "mapnew" |
        "match" | "matchend" | "matchstr" | "matchlist" | "matchstrpos" | "max" | "min" | "nr2char" | "or" | "pathshorten" | "pow" | "printf" | "range" | "reduce" | "resolve" |
        "remove" | "repeat" | "reverse" | "setenv" | "simplify" | "slice" | "sort" | "split" | "sqrt" | "str2float" | "str2list" |
        "str2nr" | "strcharlen" | "strchars" | "stridx" | "string" | "strpart" | "strridx" | "strtrans" | "strutf16len" | "strwidth" |
        "substitute" | "tolower" | "toupper" | "tr" | "trim" | "trunc" | "type" | "uniq" | "utf16idx" | "charidx" | "values" | "xor"
    )
}

/// Implements the evaluator-owned portions of `exists()`: environment,
/// option, builtin-function, and variable names. Hosts with user functions,
/// Ex commands, and autocommands layer those namespaces on top.
pub fn exists(value: &Typval, scope: &Scope) -> Result<Typval> {
    let operand = string_arg(value)?;
    let bytes = operand.as_bytes();
    let found = match bytes.first() {
        Some(b'$') => {
            let name = &bytes[1..];
            scope.contains_env(name)
                || std::env::var_os(String::from_utf8_lossy(name).as_ref()).is_some()
        }
        Some(b'&' | b'+') => option_exists(scope, &bytes[1..]),
        Some(b'*') => std::str::from_utf8(&bytes[1..])
            .ok()
            .is_some_and(|name| builtin_spec(name).is_some()),
        Some(b':' | b'#') | None => false,
        _ => matches!(bytes, b"v:true" | b"v:false" | b"v:null" | b"v:none")
            || scope.contains_variable(bytes),
    };
    Ok(Typval::Number(i64::from(found)))
}

fn option_exists(scope: &Scope, name: &[u8]) -> bool {
    let (option_scope, name) = if let Some(name) = name.strip_prefix(b"g:") {
        (crate::scope::OptionScope::Global, name)
    } else if let Some(name) = name.strip_prefix(b"l:") {
        (crate::scope::OptionScope::Local, name)
    } else {
        (crate::scope::OptionScope::Effective, name)
    };
    !name.is_empty() && scope.contains_option(option_scope, name)
}

/// Whether `name` is a builtin served through a [`BufferHost`] seam rather
/// than this typval-only dispatcher.
#[must_use]
pub fn is_buffer_builtin(name: &str) -> bool {
    matches!(name, "getline" | "setline")
}

/// Serve `getline()`/`setline()` against a buffer seam, mirroring
/// `f_getline`/`f_setline` in `eval/buffer.c` (`get_buffer_lines` and
/// `set_buffer_lines`). Hosts call this before generic dispatch for names
/// admitted by [`is_buffer_builtin`].
///
/// # Panics
/// Panics when `name` is not admitted by [`is_buffer_builtin`].
pub fn call_buffer_builtin(buffer: &mut dyn BufferHost, name: &str, args: Vec<Typval>) -> Result<Typval> {
    assert!(is_buffer_builtin(name), "{name} is not a buffer builtin");
    let spec = builtin_spec(name).expect("getline and setline are in the generated eval.lua table");
    check_arity(spec, args.len())?;
    match name {
        "getline" => get_buffer_lines(buffer, &args),
        _ => set_buffer_lines(buffer, &args),
    }
}

/// `tv_get_lnum` (`eval/typval.c`): numeric conversion first — a Number, or
/// the parsed integer prefix of a String (`"5"` → 5). When that yields a
/// non-positive value for a String, the address is translated like
/// `var2fpos`: `"$"` resolves to the last line through the seam, `"."` and
/// `"'x"` through [`BufferHost::address_line`]. An unresolvable address
/// degrades to 0.
fn lnum_arg(buffer: &dyn BufferHost, value: &Typval) -> Result<i64> {
    let numeric = number_arg(value)?;
    if numeric > 0 {
        return Ok(numeric);
    }
    if let Typval::String(text) = value {
        if !text.as_bytes().is_empty() {
            let address = text.to_string_lossy();
            if text.as_bytes() == b"$" {
                return Ok(saturating_i64(buffer.line_count()?));
            }
            if let Some(line) = buffer.address_line(&address)? {
                return Ok(line);
            }
        }
    }
    Ok(numeric)
}

/// `typval_tostring(value, false)` (`eval.c`): a String stays itself; any
/// other type uses its `string()` rendering.
fn line_text_arg(value: &Typval) -> Result<OxStr> {
    match value {
        Typval::String(text) => Ok(text.clone()),
        other => vim_string(other, 0),
    }
}

/// `f_getline` → `get_buffer_lines(curbuf, lnum, end, retlist, rettv)`:
/// a String without `{end}` (empty when out of range) and a List with it,
/// clamped to the buffer with `end < start` and negative `start` yielding
/// an empty List.
fn get_buffer_lines(buffer: &mut dyn BufferHost, args: &[Typval]) -> Result<Typval> {
    let line_count = buffer.line_count()?;
    let start = lnum_arg(buffer, &args[0])?;
    let Some(end_arg) = args.get(1) else {
        let line = if start >= 1 && start <= saturating_i64(line_count) {
            buffer.get_line(start as usize)?
        } else {
            None
        };
        return Ok(Typval::String(line.unwrap_or_else(|| OxStr(Vec::new()))));
    };
    let end = lnum_arg(buffer, end_arg)?;
    if start < 0 || end < start {
        return Ok(Typval::list(Vec::new()));
    }
    let first = start.max(1) as usize;
    let last = end.min(saturating_i64(line_count)) as usize;
    let mut lines = Vec::new();
    for lnum in first..=last {
        lines.push(Typval::String(buffer.get_line(lnum)?.unwrap_or_else(|| OxStr(Vec::new()))));
    }
    Ok(Typval::list(lines))
}

/// `f_setline` → `set_buffer_lines(curbuf, lnum, append = false, ...)`:
/// replaces existing lines, appends at `line_count + 1`, writes list items
/// onto consecutive lines, stops at the first line past `line_count + 1`,
/// and reports failure as 1 — except an empty List, which always succeeds.
fn set_buffer_lines(buffer: &mut dyn BufferHost, args: &[Typval]) -> Result<Typval> {
    let mut lnum = lnum_arg(buffer, &args[0])?;
    if lnum < 1 {
        return Ok(Typval::Number(1));
    }
    match &args[1] {
        Typval::List(reference) => {
            let items = list_items(reference)?;
            if items.is_empty() {
                return Ok(Typval::Number(0));
            }
            let mut failed = false;
            for item in items {
                // Lines already appended extend the buffer, so re-read the
                // count like upstream re-reads b_ml.ml_line_count per item.
                if lnum > saturating_i64(buffer.line_count()?) + 1 {
                    failed = true;
                    break;
                }
                set_buffer_line(buffer, lnum, &line_text_arg(&item)?)?;
                lnum += 1;
            }
            Ok(Typval::Number(i64::from(failed)))
        }
        single => {
            if lnum > saturating_i64(buffer.line_count()?) + 1 {
                return Ok(Typval::Number(1));
            }
            set_buffer_line(buffer, lnum, &line_text_arg(single)?)?;
            Ok(Typval::Number(0))
        }
    }
}

/// One iteration of upstream's write loop: replace an existing line, or
/// append when `lnum` is the line just past the end.
fn set_buffer_line(buffer: &mut dyn BufferHost, lnum: i64, text: &OxStr) -> Result<()> {
    let line_count = buffer.line_count()?;
    if lnum <= saturating_i64(line_count) {
        buffer.replace_line(lnum as usize, text)
    } else {
        buffer.append_line(text)
    }
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

fn setenv(args: &[Typval]) -> Result<Typval> {
    let name = string_arg(&args[0])?.to_string_lossy().into_owned();
    if args[1] == Typval::Special(Special::Null) {
        ox_sys::unset_env(name);
    } else {
        let value = string_arg(&args[1])?.to_string_lossy().into_owned();
        ox_sys::set_env(name, value);
    }
    Ok(Typval::Number(0))
}

fn borrow_error() -> EvalError { EvalError::new("E742", 0, "Cannot change value during recursive container access") }
fn locked_error() -> EvalError { EvalError::new("E741", 0, "Value is locked") }

fn list_items(reference: &ox_types::ListRef) -> Result<Vec<Typval>> {
    reference.try_borrow().map(|data| data.items.clone()).map_err(|_| borrow_error())
}

fn dict_entries(reference: &ox_types::DictRef) -> Result<Vec<(OxStr, Typval)>> {
    reference.try_borrow().map(|data| data.entries.clone()).map_err(|_| borrow_error())
}

fn ensure_unlocked(lock: ox_types::LockState) -> Result<()> {
    if lock.locked { Err(locked_error()) } else { Ok(()) }
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
        Some(container @ Typval::List(_)) => {
            let Typval::List(reference) = &container else { return Err(EvalError::new("E714", 0, "List required")); };
            let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
            ensure_unlocked(data.lock)?;
            data.items.push(value);
            drop(data);
            Ok(container)
        }
        Some(Typval::Blob(mut values)) => {
            let byte = u8::try_from(number_arg(&value)?).map_err(|_| EvalError::new("E1230", 0, "Blob value must be in range 0 to 255"))?;
            values.push(byte);
            Ok(Typval::Blob(values))
        }
        _ => Err(EvalError::new("E899", 0, "List or Blob required")),
    }
}

fn is_empty(value: &Typval) -> bool {
    match value {
        Typval::Number(value) => *value == 0,
        Typval::Float(value) => *value == 0.0,
        Typval::String(value) => value.as_bytes().is_empty(),
        Typval::Blob(value) => value.is_empty(),
        Typval::List(value) => value.try_borrow().map_or(true, |data| data.items.is_empty()),
        Typval::Dict(value) => value.try_borrow().map_or(true, |data| data.entries.is_empty()),
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
        Typval::List(value) => list_items(value)?.len(),
        Typval::Dict(value) => dict_entries(value)?.len(),
        _ => string_arg(value)?.as_bytes().len(),
    };
    Ok(Typval::Number(saturating_i64(length)))
}

fn strcharlen(value: &Typval) -> Result<Typval> {
    let value = string_arg(value)?;
    let count = String::from_utf8_lossy(value.as_bytes()).chars().filter(|character| UnicodeWidthChar::width(*character).unwrap_or(0) != 0).count();
    Ok(Typval::Number(saturating_i64(count)))
}

fn strchars(args: &[Typval]) -> Result<Typval> {
    let value = string_arg(&args[0])?;
    let skip_composing = args.get(1).is_some_and(Typval::is_truthy);
    let count = String::from_utf8_lossy(value.as_bytes()).chars().filter(|character| !skip_composing || UnicodeWidthChar::width(*character).unwrap_or(0) != 0).count();
    Ok(Typval::Number(saturating_i64(count)))
}

fn string_elements(mut bytes: &[u8]) -> Vec<OxStr> {
    let mut elements = Vec::new();
    while !bytes.is_empty() {
        let width = match bytes[0] {
            0x00..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 1,
        }.min(bytes.len());
        let length = if width > 1 && std::str::from_utf8(&bytes[..width]).is_ok() { width } else { 1 };
        elements.push(OxStr(bytes[..length].to_vec()));
        bytes = &bytes[length..];
    }
    elements
}

fn change_case(value: &Typval, upper: bool) -> Result<Typval> {
    let value = string_arg(value)?;
    let text = String::from_utf8_lossy(value.as_bytes());
    let mut changed = String::with_capacity(text.len());
    for character in text.chars() {
        let mapped = if upper {
            let mut values = character.to_uppercase();
            let first = values.next().unwrap_or(character);
            if values.next().is_none() { first } else { character }
        } else {
            let mut values = character.to_lowercase();
            let first = values.next().unwrap_or(character);
            if values.next().is_none() { first } else if character == '\u{0130}' { 'i' } else { character }
        };
        changed.push(mapped);
    }
    Ok(Typval::String(OxStr(changed.into_bytes())))
}

fn trim(args: &[Typval]) -> Result<Typval> {
    let value = string_arg(&args[0])?;
    let mask = args.get(1).map(|value| strict_string_arg(value, 2)).transpose()?;
    let direction = args.get(2).map(number_arg).transpose()?.unwrap_or(0);
    if !(0..=2).contains(&direction) {
        return Err(EvalError::new("E475", 0, "Invalid argument"));
    }
    let text = String::from_utf8_lossy(value.as_bytes());
    let mask_text = mask.as_ref().map(|value| String::from_utf8_lossy(value.as_bytes()));
    let removable = |character: char| mask_text.as_ref().map_or(character <= '\u{20}' || character == '\u{a0}', |mask| mask.contains(character));
    let start = if direction != 2 { text.char_indices().find(|(_, character)| !removable(*character)).map_or(text.len(), |(index, _)| index) } else { 0 };
    let end = if direction != 1 { text.char_indices().rev().find(|(_, character)| !removable(*character)).map_or(start, |(index, character)| index + character.len_utf8()) } else { text.len() };
    Ok(Typval::String(OxStr(text.as_bytes()[start.min(end)..end].to_vec())))
}

fn translate(args: &[Typval]) -> Result<Typval> {
    let input = string_arg(&args[0])?;
    let from = string_arg(&args[1])?;
    let to = string_arg(&args[2])?;
    let from_chars = String::from_utf8_lossy(from.as_bytes()).chars().collect::<Vec<_>>();
    let to_chars = String::from_utf8_lossy(to.as_bytes()).chars().collect::<Vec<_>>();
    if from_chars.len() != to_chars.len() {
        return Err(EvalError::new("E475", 0, "Invalid argument: fromstr and tostr have different number of characters"));
    }
    let replacements = from_chars.into_iter().zip(to_chars).collect::<HashMap<_, _>>();
    let mut output = String::new();
    for character in String::from_utf8_lossy(input.as_bytes()).chars() {
        output.push(replacements.get(&character).copied().unwrap_or(character));
    }
    Ok(Typval::String(OxStr(output.into_bytes())))
}

fn strwidth(value: &Typval, ambiguous_wide: bool) -> Result<Typval> {
    let value = string_arg(value)?;
    let width = String::from_utf8_lossy(value.as_bytes()).chars().map(|character| match character { '\t' => 1, _ if ambiguous_wide => UnicodeWidthChar::width_cjk(character).unwrap_or(0), _ => UnicodeWidthChar::width(character).unwrap_or(0) }).sum::<usize>();
    Ok(Typval::Number(saturating_i64(width)))
}

fn strtrans(value: &Typval) -> Result<Typval> {
    let value = string_arg(value)?;
    let mut output = String::new();
    for element in string_elements(value.as_bytes()) {
        let bytes = element.as_bytes();
        if bytes.len() == 1 {
            match bytes[0] {
                0x00..=0x1f => { output.push('^'); output.push(char::from(bytes[0] + b'@')); }
                0x7f => output.push_str("^?"),
                0x80..=0xff => output.push_str(&format!("<{:02x}>", bytes[0])),
                byte => output.push(char::from(byte)),
            }
            continue;
        }
        let character = std::str::from_utf8(bytes).ok().and_then(|text| text.chars().next());
        match character {
            Some(character @ ('\u{80}'..='\u{9f}' | '\u{200b}' | '\u{feff}')) => output.push_str(&format!("<{:x}>", character as u32)),
            Some(character) => output.push(character),
            None => for byte in bytes { output.push_str(&format!("<{byte:02x}>")); },
        }
    }
    Ok(Typval::String(OxStr(output.into_bytes())))
}

fn strutf16len(args: &[Typval]) -> Result<Typval> {
    let value = strict_string_arg(&args[0], 1)?;
    let count_composing = bool_number_arg(args.get(1))?;
    let mut units = 0usize;
    for character in String::from_utf8_lossy(value.as_bytes()).chars() {
        if count_composing || UnicodeWidthChar::width(character).unwrap_or(0) != 0 { units += character.len_utf16(); }
    }
    Ok(Typval::Number(saturating_i64(units)))
}

fn charidx(args: &[Typval]) -> Result<Typval> {
    let value = strict_string_arg(&args[0], 1)?;
    let index = strict_number_arg(&args[1], 2)?;
    let count_composing = bool_number_arg(args.get(2))?;
    let utf16_index = bool_number_arg(args.get(3))?;
    if index < 0 { return Ok(Typval::Number(-1)); }
    let target = usize::try_from(index).unwrap_or(usize::MAX);
    let text = String::from_utf8_lossy(value.as_bytes());
    let limit = if utf16_index { text.encode_utf16().count() } else { value.as_bytes().len() };
    if target > limit { return Ok(Typval::Number(-1)); }
    let mut position = 0usize;
    let mut characters = 0usize;
    for character in text.chars() {
        let next = position + if utf16_index { character.len_utf16() } else { character.len_utf8() };
        if target < next {
            let index = if !count_composing && UnicodeWidthChar::width(character).unwrap_or(0) == 0 { characters.saturating_sub(1) } else { characters };
            return Ok(Typval::Number(saturating_i64(index)));
        }
        position = next;
        if count_composing || UnicodeWidthChar::width(character).unwrap_or(0) != 0 { characters += 1; }
    }
    Ok(Typval::Number(if target == position { saturating_i64(characters) } else { -1 }))
}

fn utf16idx(args: &[Typval]) -> Result<Typval> {
    let value = strict_string_arg(&args[0], 1)?;
    let index = strict_number_arg(&args[1], 2)?;
    let count_composing = bool_number_arg(args.get(2))?;
    let char_index = bool_number_arg(args.get(3))?;
    if index < 0 { return Ok(Typval::Number(-1)); }
    let target = usize::try_from(index).unwrap_or(usize::MAX);
    let text = String::from_utf8_lossy(value.as_bytes());
    let limit = if char_index { text.chars().filter(|character| count_composing || UnicodeWidthChar::width(*character).unwrap_or(0) != 0).count() } else { value.as_bytes().len() };
    if target > limit { return Ok(Typval::Number(-1)); }
    let mut source_position = 0usize;
    let mut units = 0usize;
    let mut cluster_start = 0usize;
    for character in text.chars() {
        let composing = UnicodeWidthChar::width(character).unwrap_or(0) == 0;
        let source_width = if char_index { usize::from(count_composing || !composing) } else { character.len_utf8() };
        if target < source_position + source_width {
            let result = if !count_composing && composing { cluster_start } else { units };
            return Ok(Typval::Number(saturating_i64(result)));
        }
        source_position += source_width;
        if count_composing || !composing {
            cluster_start = units;
            units += character.len_utf16();
        }
    }
    Ok(Typval::Number(if target == source_position { saturating_i64(units) } else { -1 }))
}

fn bool_number_arg(value: Option<&Typval>) -> Result<bool> {
    match value { None => Ok(false), Some(Typval::Bool(value)) => Ok(*value), Some(Typval::Number(0)) => Ok(false), Some(Typval::Number(1)) => Ok(true), Some(_) => Err(EvalError::new("E1212", 0, "Bool required")) }
}

fn strict_string_arg(value: &Typval, argument: usize) -> Result<OxStr> {
    match value { Typval::String(value) => Ok(value.clone()), _ => Err(EvalError::new("E1174", 0, format!("String required for argument {argument}"))) }
}

fn strict_number_arg(value: &Typval, argument: usize) -> Result<i64> {
    match value { Typval::Number(value) => Ok(*value), _ => Err(EvalError::new("E1210", 0, format!("Number required for argument {argument}"))) }
}

fn pathshorten(args: &[Typval]) -> Result<Typval> {
    let value = string_arg(&args[0])?;
    let keep = args.get(1).map(number_arg).transpose()?.unwrap_or(1).max(1) as usize;
    let source = String::from_utf8_lossy(value.as_bytes());
    let mut components = source.split('/').collect::<Vec<_>>();
    let last = if source.ends_with('/') { components.len().saturating_sub(1) } else { components.iter().rposition(|component| !component.is_empty()).unwrap_or(0) };
    for (index, component) in components.iter_mut().enumerate() {
        if index == last || component.is_empty() { continue; }
        let prefix = component.chars().take_while(|character| matches!(character, '.' | '~')).count();
        let end = component.char_indices().nth(prefix + keep).map_or(component.len(), |(index, _)| index);
        *component = &component[..end];
    }
    Ok(Typval::String(OxStr(components.join("/").into_bytes())))
}

fn keytrans(value: &Typval) -> Result<Typval> {
    let Typval::String(value) = value else { return Err(EvalError::new("E1174", 0, "String required for argument 1")); };
    let bytes = value.as_bytes();
    let mut output = String::new();
    let mut index = 0usize;
    let mut modifiers = 0u8;
    while index < bytes.len() {
        if bytes.get(index..index + 3).is_some_and(|value| value[0] == 0x80 && value[1] == 0xfc) { modifiers = bytes[index + 2]; index += 3; continue; }
        let (name, consumed) = if bytes.get(index..index + 3).is_some_and(|value| value[0] == 0x80 && value[1] == 0xfd) {
            (match bytes[index + 2] { b'B' => "BS".to_owned(), b'T' => "Tab".to_owned(), b'N' => "NL".to_owned(), b'R' => "CR".to_owned(), b'E' => "Esc".to_owned(), b'S' => "Space".to_owned(), b'L' => "lt".to_owned(), b'\\' => "Bslash".to_owned(), b'|' => "Bar".to_owned(), b'D' => "Del".to_owned(), b'H' => "Home".to_owned(), other => char::from(other).to_string() }, 3)
        } else {
            let width = match bytes[index] { 0x00..=0x7f => 1, 0xc2..=0xdf => 2, 0xe0..=0xef => 3, 0xf0..=0xf4 => 4, _ => 1 }.min(bytes.len() - index);
            let raw = &bytes[index..index + width];
            let name = match raw { b" " => "Space".to_owned(), b"<" => "lt".to_owned(), b"|" => "Bar".to_owned(), b"\\" => "Bslash".to_owned(), [0x08] => "BS".to_owned(), [b'\t'] => "C-I".to_owned(), [b'\r'] => "CR".to_owned(), [0x1b] => "Esc".to_owned(), [0x7f] => "Del".to_owned(), [control @ 1..=26] => format!("C-{}", char::from(control + b'@')), _ => String::from_utf8_lossy(raw).into_owned() };
            (name, width)
        };
        index += consumed;
        let requires_brackets = modifiers != 0 || name.chars().count() != 1 || matches!(name.as_str(), "Space" | "lt" | "Bar" | "Bslash");
        if requires_brackets {
            output.push('<');
            if modifiers & 2 != 0 && !name.starts_with("C-") { output.push_str("C-"); }
            if modifiers & 1 != 0 { output.push_str("S-"); }
            if modifiers & 4 != 0 { output.push_str("M-"); }
            output.push_str(&name);
            output.push('>');
        } else { output.push_str(&name); }
        modifiers = 0;
    }
    Ok(Typval::String(OxStr(output.into_bytes())))
}

fn join(args: &[Typval]) -> Result<Typval> {
    let Typval::List(reference) = &args[0] else { return Err(EvalError::new("E714", 0, "List required")); };
    let values = list_items(reference)?;
    let separator = args.get(1).map(string_arg).transpose()?.unwrap_or_else(|| OxStr::from(" "));
    let mut result = Vec::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 { result.extend_from_slice(separator.as_bytes()); }
        result.extend_from_slice(string_arg(value)?.as_bytes());
    }
    Ok(Typval::String(OxStr(result)))
}

fn repeat(args: &[Typval]) -> Result<Typval> {
    let count = usize::try_from(number_arg(&args[1])?.max(0)).map_err(|_| EvalError::new("E1240", 0, "Resulting text too long"))?;
    match &args[0] {
        Typval::String(value) => Ok(Typval::String(OxStr(value.as_bytes().repeat(count)))),
        Typval::List(value) => {
            let items = list_items(value)?;
            let mut repeated = Vec::with_capacity(items.len().saturating_mul(count));
            for _ in 0..count { repeated.extend(items.iter().cloned()); }
            Ok(Typval::list(repeated))
        }
        Typval::Blob(value) => Ok(Typval::Blob(value.repeat(count))),
        _ => Err(EvalError::new("E1294", 0, "String, List or Blob required")),
    }
}

fn reverse(mut args: Vec<Typval>) -> Result<Typval> {
    match args.pop() {
        Some(Typval::String(value)) => {
            let text = String::from_utf8_lossy(value.as_bytes());
            let mut clusters: Vec<String> = Vec::new();
            for character in text.chars() {
                if is_combining(character) {
                    if let Some(cluster) = clusters.last_mut() { cluster.push(character); } else { clusters.push(character.to_string()); }
                } else if is_regional_indicator(character) && clusters.last().is_some_and(|cluster| cluster.chars().count() == 1 && cluster.chars().next().is_some_and(is_regional_indicator)) {
                    clusters.last_mut().expect("checked above").push(character);
                } else {
                    clusters.push(character.to_string());
                }
            }
            clusters.reverse();
            Ok(Typval::String(OxStr(clusters.concat().into_bytes())))
        }
        Some(container @ Typval::List(_)) => {
            let Typval::List(reference) = &container else { return Err(EvalError::new("E714", 0, "List required")); };
            let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
            ensure_unlocked(data.lock)?;
            data.items.reverse();
            drop(data);
            Ok(container)
        }
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
    let mut start = number_arg(&args[1])?;
    let mut length = args.get(2).map(number_arg).transpose()?.unwrap_or(i64::MAX);
    if start < 0 { length = length.saturating_add(start); start = 0; }
    let start = usize::try_from(start).unwrap_or(usize::MAX).min(value.as_bytes().len());
    if args.get(3).is_some_and(Typval::is_truthy) {
        let suffix = String::from_utf8_lossy(&value.as_bytes()[start..]);
        let mut base_count = 0i64;
        let mut end = 0usize;
        for (offset, character) in suffix.char_indices() {
            if !is_combining(character) {
                if base_count >= length.max(0) { break; }
                base_count += 1;
            }
            end = offset + character.len_utf8();
        }
        return Ok(Typval::String(OxStr(suffix.as_bytes()[..end].to_vec())));
    }
    let length = usize::try_from(length.max(0)).unwrap_or(usize::MAX);
    let end = start.saturating_add(length).min(value.as_bytes().len());
    Ok(Typval::String(OxStr(value.as_bytes()[start..end].to_vec())))
}

fn is_combining(character: char) -> bool {
    matches!(character as u32, 0x0300..=0x036f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0x20d0..=0x20ff | 0xfe20..=0xfe2f)
}

fn is_regional_indicator(character: char) -> bool { matches!(character as u32, 0x1f1e6..=0x1f1ff) }

/// `f_printf` (`eval/strings.c`): C-style formatting over typvals. Handles
/// `%s`, `%d`/`%i`/`%u`, `%x`/`%X`/`%o`/`%b`/`%B`, `%c`, `%f`/`%e`/`%g`
/// families with `-`/`0`/`+` flags, width, and precision; `%%` is literal.
/// Too few arguments is E766, leftovers are E767.
fn printf_builtin(args: &[Typval]) -> Result<Typval> {
    let format = match &args[0] {
        Typval::String(text) => text.to_string_lossy().into_owned(),
        other => vim_string(other, 0)?.to_string_lossy().into_owned(),
    };
    let mut pieces = String::new();
    let mut arguments = args[1..].iter();
    let mut source = format.chars().peekable();
    while let Some(character) = source.next() {
        if character != '%' {
            pieces.push(character);
            continue;
        }
        if source.peek() == Some(&'%') {
            source.next();
            pieces.push('%');
            continue;
        }
        let mut flags = String::new();
        while matches!(source.peek(), Some('-') | Some('0') | Some('+') | Some(' ') | Some('#')) {
            flags.push(source.next().expect("peeked flag"));
        }
        let mut width = String::new();
        while source.peek().is_some_and(|digit| digit.is_ascii_digit()) {
            width.push(source.next().expect("peeked digit"));
        }
        let mut precision: Option<usize> = None;
        if source.peek() == Some(&'.') {
            source.next();
            let mut digits = String::new();
            while source.peek().is_some_and(|digit| digit.is_ascii_digit()) {
                digits.push(source.next().expect("peeked digit"));
            }
            precision = Some(digits.parse().unwrap_or(0));
        }
        while matches!(source.peek(), Some('l') | Some('h') | Some('z')) {
            source.next();
        }
        let Some(conversion) = source.next() else {
            return Err(EvalError::new("E806", 0, "Invalid format specifier"));
        };
        let Some(value) = arguments.next() else {
            return Err(EvalError::new(
                "E766",
                0,
                format!("Insufficient arguments for printf() at {}", conversion),
            ));
        };
        let left = flags.contains('-');
        let zero = flags.contains('0') && !left;
        let width: usize = width.parse().unwrap_or(0);
        let rendered = match conversion {
            'd' | 'i' | 'u' => {
                let mut number = number_arg(value)?.to_string();
                if flags.contains('+') && !number.starts_with('-') {
                    number.insert(0, '+');
                }
                if let Some(digits) = precision {
                    let magnitude = number.trim_start_matches(['-', '+']).len();
                    if magnitude < digits {
                        let sign = number.starts_with(['-', '+']);
                        let insert_at = usize::from(sign);
                        number.insert_str(insert_at, &"0".repeat(digits - magnitude));
                    }
                }
                number
            }
            'x' | 'X' | 'o' | 'b' | 'B' => {
                let number = number_arg(value)?;
                let radix = match conversion {
                    'x' | 'X' => 16,
                    'o' => 8,
                    _ => 2,
                };
                let mut digits = to_radix(number.unsigned_abs(), radix);
                if conversion.is_ascii_uppercase() {
                    digits = digits.to_uppercase();
                }
                if flags.contains('#') && number != 0 {
                    let prefix = match conversion {
                        'x' => "0x",
                        'X' => "0X",
                        'o' => "0",
                        'b' => "0b",
                        _ => "0B",
                    };
                    digits = format!("{prefix}{digits}");
                }
                if let Some(count) = precision {
                    if digits.len() < count {
                        digits = format!("{}{digits}", "0".repeat(count - digits.len()));
                    }
                }
                digits
            }
            'c' => match value {
                Typval::String(text) if !text.as_bytes().is_empty() => {
                    char::from(text.as_bytes()[0]).to_string()
                }
                other => char::from_u32(number_arg(other)? as u32).unwrap_or('\0').to_string(),
            },
            'f' | 'F' | 'e' | 'E' | 'g' | 'G' => {
                let number = float_arg(value)?;
                let digits = precision.unwrap_or(6);
                match conversion {
                    'f' | 'F' | 'g' => format!("{number:.digits$}"),
                    'e' => format!("{number:.digits$e}"),
                    'E' => format!("{number:.digits$e}").to_uppercase(),
                    _ => format!("{number:.digits$}").to_uppercase(),
                }
            }
            's' => {
                let mut text = match value {
                    Typval::String(text) => text.to_string_lossy().into_owned(),
                    other => vim_string(other, 0)?.to_string_lossy().into_owned(),
                };
                if let Some(count) = precision {
                    text = text.chars().take(count).collect();
                }
                text
            }
            _ => {
                return Err(EvalError::new(
                    "E806",
                    0,
                    format!("Invalid format specifier: %{conversion}"),
                ));
            }
        };
        let padding = width.saturating_sub(rendered.chars().count());
        if left {
            pieces.push_str(&rendered);
            pieces.push_str(&" ".repeat(padding));
        } else if zero && matches!(conversion, 'd' | 'i' | 'u' | 'x' | 'X' | 'o' | 'b' | 'B' | 'c') {
            let (sign, digits) = match rendered.strip_prefix(['-', '+']) {
                Some(digits) => (rendered[..1].to_owned(), digits),
                None => (String::new(), rendered.as_str()),
            };
            pieces.push_str(&sign);
            pieces.push_str(&"0".repeat(padding));
            pieces.push_str(digits);
        } else {
            pieces.push_str(&" ".repeat(padding));
            pieces.push_str(&rendered);
        }
    }
    if arguments.next().is_some() {
        return Err(EvalError::new("E767", 0, "Too many arguments to printf()"));
    }
    Ok(Typval::String(OxStr(pieces.into_bytes())))
}

/// Digits of `value` in `radix`, lowercase, without prefix.
fn to_radix(mut value: u64, radix: u64) -> String {
    if value == 0 {
        return "0".to_owned();
    }
    let mut digits = Vec::new();
    while value > 0 {
        let alphabet = b"0123456789abcdef";
        digits.push(alphabet[(value % radix) as usize]);
        value /= radix;
    }
    digits.reverse();
    String::from_utf8(digits).expect("digit bytes")
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
        Typval::List(values) => list_items(values)?.iter().skip(start).try_fold(0usize, |total, value| Ok::<_, EvalError>(total + usize::from(values_equal(value, needle, ignore_case, 0)?)))?,
        Typval::Dict(values) => dict_entries(values)?.iter().skip(start).try_fold(0usize, |total, (_, value)| Ok::<_, EvalError>(total + usize::from(values_equal(value, needle, ignore_case, 0)?)))?,
        Typval::String(value) => non_overlapping_count(value.as_bytes(), string_arg(needle)?.as_bytes()),
        _ => return Err(EvalError::new("E706", 0, "List, Dictionary or String required")),
    };
    Ok(Typval::Number(saturating_i64(total)))
}

fn extend(mut args: Vec<Typval>) -> Result<Typval> {
    let mode = if args.len() > 2 { Some(string_arg(&args[2])?) } else { None };
    let right = args.get(1).cloned().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
    let left = args.first().cloned().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
    match (&left, right) {
        (Typval::List(left_ref), Typval::List(right_ref)) => {
            let right = list_items(&right_ref)?;
            let mut left = left_ref.try_borrow_mut().map_err(|_| borrow_error())?;
            ensure_unlocked(left.lock)?;
            left.items.extend(right);
            drop(left);
            Ok(args.remove(0))
        }
        (Typval::Dict(left_ref), Typval::Dict(right_ref)) => {
            let right = dict_entries(&right_ref)?;
            let mut left = left_ref.try_borrow_mut().map_err(|_| borrow_error())?;
            ensure_unlocked(left.lock)?;
            let mode = mode.as_ref().map_or(b"force".as_slice(), OxStr::as_bytes);
            for (key, value) in right {
                if let Some((_, existing)) = left.entries.iter_mut().find(|(candidate, _)| candidate == &key) {
                    match mode {
                        b"keep" => {}
                        b"error" => return Err(EvalError::new("E737", 0, format!("Key already exists: {}", key.to_string_lossy()))),
                        b"force" => *existing = value,
                        _ => return Err(EvalError::new("E475", 0, "Invalid argument")),
                    }
                } else { left.entries.push((key, value)); }
            }
            drop(left);
            Ok(args.remove(0))
        }
        _ => Err(EvalError::new("E712", 0, "Argument of extend() must be a List or Dictionary")),
    }
}

fn get(args: &[Typval]) -> Result<Typval> {
    let fallback = args.get(2).cloned().unwrap_or(Typval::Number(0));
    match &args[0] {
        Typval::List(values) => {
            let values = list_items(values)?;
            Ok(normalize_index(values.len(), number_arg(&args[1])?).and_then(|index| values.get(index)).cloned().unwrap_or(fallback))
        }
        Typval::Blob(values) => Ok(normalize_index(values.len(), number_arg(&args[1])?).and_then(|index| values.get(index)).map_or(fallback, |value| Typval::Number(i64::from(*value)))),
        Typval::Dict(values) => {
            let key = string_arg(&args[1])?;
            Ok(dict_entries(values)?.into_iter().find(|(candidate, _)| candidate == &key).map_or(fallback, |(_, value)| value))
        }
        _ => Err(EvalError::new("E896", 0, "List, Dictionary or Blob required")),
    }
}

/// `"has"` — feature probe. Mirrors `f_has` in `eval/funcs.c`: the
/// `"nvim-X.Y[.Z]"` form compares against the Neovim version this build
/// targets (0.13.0, matching `ox_rpc`'s `API_LEVEL = 15`); everything else
/// answers from a small compile-time-honest table and defaults to 0, which is
/// what upstream returns for features the build does not provide.
fn has_feature(args: &[Typval]) -> Result<Typval> {
    let feature = string_arg(&args[0])?.to_string_lossy().into_owned();
    let supported = if let Some(version) = feature.strip_prefix("nvim-") {
        let mut parts = version.split('.').map(|part| part.parse::<u64>().unwrap_or(u64::MAX));
        let requested = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        requested <= (0, 13, 0) && parts.next().is_none()
    } else {
        match feature.as_str() {
            "unix" => cfg!(unix),
            "win32" | "win64" => cfg!(windows),
            "macunix" => cfg!(target_os = "macos"),
            "multi_byte" => true,
            _ => false,
        }
    };
    Ok(Typval::Number(i64::from(supported)))
}

fn has_key(args: &[Typval]) -> Result<Typval> {
    let Typval::Dict(values) = &args[0] else { return Err(EvalError::new("E1206", 0, "Dictionary required")); };
    let key = string_arg(&args[1])?;
    Ok(Typval::Number(i64::from(dict_entries(values)?.iter().any(|(candidate, _)| candidate == &key))))
}

fn gettext(value: &Typval) -> Result<Typval> {
    let Typval::String(text) = value else {
        return Err(EvalError::new("E1174", 0, "String required for argument 1"));
    };
    if text.as_bytes().is_empty() {
        return Err(EvalError::new("E1175", 0, "Non-empty string required for argument 1"));
    }
    Ok(Typval::String(text.clone()))
}

fn hostname() -> Result<Typval> {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|value| value.trim_end_matches(['\r', '\n']).to_owned())
        .or_else(|_| std::env::var("HOSTNAME"))
        .map_err(|error| EvalError::new("E500", 0, format!("Cannot determine hostname: {error}")))?;
    Ok(Typval::String(OxStr::from(name.as_str())))
}

fn slice(args: &[Typval]) -> Result<Typval> {
    let start = number_arg(&args[1])?;
    let end = args.get(2).map(number_arg).transpose()?;
    match &args[0] {
        Typval::List(reference) => {
            let items = list_items(reference)?;
            let (start, end) = slice_bounds(items.len(), start, end);
            Ok(Typval::list(items[start..end].to_vec()))
        }
        Typval::Blob(value) => {
            let (start, end) = slice_bounds(value.len(), start, end);
            Ok(Typval::Blob(value[start..end].to_vec()))
        }
        Typval::String(value) => {
            let characters = String::from_utf8_lossy(value.as_bytes()).chars().collect::<Vec<_>>();
            let (start, end) = slice_bounds(characters.len(), start, end);
            Ok(Typval::String(OxStr(characters[start..end].iter().collect::<String>().into_bytes())))
        }
        _ => Err(EvalError::new("E1170", 0, "Cannot use slice() with this type")),
    }
}

fn slice_bounds(length: usize, start: i64, end: Option<i64>) -> (usize, usize) {
    fn bound(length: usize, index: i64) -> usize {
        if index < 0 {
            length.saturating_sub(index.unsigned_abs() as usize)
        } else {
            usize::try_from(index).unwrap_or(usize::MAX).min(length)
        }
    }
    let start = bound(length, start);
    let end = end.map_or(length, |index| bound(length, index));
    (start.min(end), end)
}

fn index(args: &[Typval]) -> Result<Typval> {
    let Typval::List(values) = &args[0] else { return Err(EvalError::new("E714", 0, "List required")); };
    let values = list_items(values)?;
    let ignore_case = args.get(3).is_some_and(Typval::is_truthy);
    let start_number = args.get(2).map(number_arg).transpose()?.unwrap_or(0);
    let start = normalize_index(values.len(), start_number).unwrap_or(values.len());
    for (index, value) in values.iter().enumerate().skip(start) {
        if values_equal(value, &args[1], ignore_case, 0)? { return Ok(Typval::Number(saturating_i64(index))); }
    }
    Ok(Typval::Number(-1))
}

fn insert(mut args: Vec<Typval>) -> Result<Typval> {
    let index = if args.len() > 2 { number_arg(&args[2])? } else { 0 };
    let value = args.get(1).cloned().ok_or_else(|| EvalError::new("E119", 0, "not enough arguments"))?;
    match args.first() {
        Some(Typval::List(reference)) => {
            let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
            ensure_unlocked(data.lock)?;
            let index = if index < 0 {
                return Err(EvalError::new("E686", 0, "Argument of insert() must be non-negative"));
            } else {
                usize::try_from(index).unwrap_or(usize::MAX)
            };
            if index > data.items.len() { return Err(EvalError::new("E686", 0, "List index out of range")); }
            data.items.insert(index, value);
            drop(data);
            Ok(args.remove(0))
        }
        Some(Typval::Blob(values)) => {
            let mut values = values.clone();
            let byte = u8::try_from(number_arg(&value)?).map_err(|_| EvalError::new("E1230", 0, "Blob value must be in range 0 to 255"))?;
            let index = usize::try_from(index.max(0)).unwrap_or(usize::MAX);
            if index > values.len() { return Err(EvalError::new("E979", 0, "Blob index out of range")); }
            values.insert(index, byte);
            Ok(Typval::Blob(values))
        }
        _ => Err(EvalError::new("E899", 0, "List or Blob required")),
    }
}

enum Projection { Items, Keys, Values }

fn dict_projection(value: &Typval, projection: Projection) -> Result<Typval> {
    let Typval::Dict(values) = value else { return Err(EvalError::new("E1206", 0, "Dictionary required")); };
    Ok(Typval::list(dict_entries(values)?.iter().map(|(key, value)| match projection {
        Projection::Items => Typval::list(vec![Typval::String(key.clone()), value.clone()]),
        Projection::Keys => Typval::String(key.clone()),
        Projection::Values => value.clone(),
    }).collect()))
}

fn extremum(value: &Typval, maximum: bool) -> Result<Typval> {
    let values = match value {
        Typval::List(values) => list_items(values)?,
        Typval::Dict(values) => dict_entries(values)?.into_iter().map(|(_, value)| value).collect(),
        _ => return Err(EvalError::new("E712", 0, "List or Dictionary required")),
    };
    if values.is_empty() { return Ok(Typval::Number(0)); }
    let mut selected = values[0].clone();
    for value in &values[1..] {
        let ordering = compare_values(&selected, value, 0)?;
        if (maximum && ordering == Ordering::Less) || (!maximum && ordering == Ordering::Greater) { selected = value.clone(); }
    }
    Ok(selected)
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
    Ok(Typval::list(values))
}

fn remove(args: Vec<Typval>) -> Result<Typval> {
    let first = number_arg(&args[1])?;
    let last = args.get(2).map(number_arg).transpose()?;
    let dict_key = string_arg(&args[1]).ok();
    match args.first() {
        Some(Typval::List(reference)) => {
            let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
            ensure_unlocked(data.lock)?;
            let index = normalize_index(data.items.len(), first).ok_or_else(|| EvalError::new("E684", 0, "List index out of range"))?;
            if let Some(last) = last {
                let end = normalize_index(data.items.len(), last).ok_or_else(|| EvalError::new("E684", 0, "List index out of range"))?;
                if end < index { return Ok(Typval::list(vec![])); }
                Ok(Typval::list(data.items.drain(index..=end).collect()))
            } else { Ok(data.items.remove(index)) }
        }
        Some(Typval::Blob(values)) => {
            let mut values = values.clone();
            let index = normalize_index(values.len(), first).ok_or_else(|| EvalError::new("E979", 0, "Blob index out of range"))?;
            if let Some(last) = last {
                let end = normalize_index(values.len(), last).ok_or_else(|| EvalError::new("E979", 0, "Blob index out of range"))?;
                Ok(Typval::Blob(values.drain(index..=end).collect()))
            } else { Ok(Typval::Number(i64::from(values.remove(index)))) }
        }
        Some(Typval::Dict(reference)) => {
            let key = dict_key.ok_or_else(|| EvalError::new("E731", 0, "Dictionary key must be a String"))?;
            let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
            ensure_unlocked(data.lock)?;
            let index = data.entries.iter().position(|(candidate, _)| candidate == &key).ok_or_else(|| EvalError::new("E716", 0, "Key not present in Dictionary"))?;
            Ok(data.entries.remove(index).1)
        }
        _ => Err(EvalError::new("E896", 0, "List, Dictionary or Blob required")),
    }
}

fn uniq(mut args: Vec<Typval>) -> Result<Typval> {
    let Some(container @ Typval::List(_)) = args.get_mut(0).cloned() else { return Err(EvalError::new("E714", 0, "List required")); };
    let Typval::List(reference) = &container else { return Err(EvalError::new("E714", 0, "List required")); };
    let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
    ensure_unlocked(data.lock)?;
    let mut index = 1;
    while index < data.items.len() {
        if values_equal(&data.items[index - 1], &data.items[index], false, 0)? { data.items.remove(index); } else { index += 1; }
    }
    drop(data);
    Ok(container)
}

fn shallow_copy(value: &Typval) -> Result<Typval> {
    match value {
        Typval::List(reference) => Ok(Typval::list(list_items(reference)?)),
        Typval::Dict(reference) => Ok(Typval::dict(dict_entries(reference)?)),
        _ => Ok(value.clone()),
    }
}

fn flatten(args: &[Typval], mutate: bool) -> Result<Typval> {
    let Typval::List(reference) = &args[0] else { return Err(EvalError::new("E686", 0, "Argument of flatten() must be a List")); };
    if mutate { ensure_unlocked(reference.try_borrow().map_err(|_| borrow_error())?.lock)?; }
    let maximum = args.get(1).map(number_arg).transpose()?.unwrap_or(i64::MAX);
    let mut output = Vec::new();
    flatten_into(reference, maximum, 0, &mut HashSet::new(), &mut output)?;
    if mutate {
        reference.try_borrow_mut().map_err(|_| borrow_error())?.items = output;
        Ok(args[0].clone())
    } else {
        Ok(Typval::list(output))
    }
}

fn flatten_into(reference: &ox_types::ListRef, maximum: i64, depth: usize, active: &mut HashSet<usize>, output: &mut Vec<Typval>) -> Result<()> {
    if depth >= MAX_CONTAINER_DEPTH { return Err(EvalError::new("E724", 0, "too much recursion")); }
    let pointer = Rc::as_ptr(reference) as usize;
    if !active.insert(pointer) { return Err(EvalError::new("E724", 0, "too much recursion in flatten()")); }
    for value in list_items(reference)? {
        if let Typval::List(nested) = &value {
            if maximum > 0 { flatten_into(nested, maximum - 1, depth + 1, active, output)?; } else { output.push(value); }
        } else { output.push(value); }
    }
    active.remove(&pointer);
    Ok(())
}

fn deep_copy(value: &Typval) -> Result<Typval> {
    fn copy(
        value: &Typval,
        lists: &mut HashMap<usize, ox_types::ListRef>,
        dicts: &mut HashMap<usize, ox_types::DictRef>,
        depth: usize,
    ) -> Result<Typval> {
        if depth >= MAX_CONTAINER_DEPTH { return Err(EvalError::new("E698", 0, "variable nested too deep for making a copy")); }
        match value {
            Typval::List(source) => {
                let key = Rc::as_ptr(source) as usize;
                if let Some(existing) = lists.get(&key) { return Ok(Typval::List(existing.clone())); }
                let Typval::List(target) = Typval::list(vec![]) else { return Err(EvalError::new("E698", 0, "copy failed")); };
                lists.insert(key, target.clone());
                let source_items = list_items(source)?;
                let mut items = Vec::with_capacity(source_items.len());
                for item in &source_items { items.push(copy(item, lists, dicts, depth + 1)?); }
                target.try_borrow_mut().map_err(|_| borrow_error())?.items = items;
                Ok(Typval::List(target))
            }
            Typval::Dict(source) => {
                let key = Rc::as_ptr(source) as usize;
                if let Some(existing) = dicts.get(&key) { return Ok(Typval::Dict(existing.clone())); }
                let Typval::Dict(target) = Typval::dict(vec![]) else { return Err(EvalError::new("E698", 0, "copy failed")); };
                dicts.insert(key, target.clone());
                let source_entries = dict_entries(source)?;
                let mut entries = Vec::with_capacity(source_entries.len());
                for (name, item) in &source_entries { entries.push((name.clone(), copy(item, lists, dicts, depth + 1)?)); }
                target.try_borrow_mut().map_err(|_| borrow_error())?.entries = entries;
                Ok(Typval::Dict(target))
            }
            _ => Ok(value.clone()),
        }
    }
    copy(value, &mut HashMap::new(), &mut HashMap::new(), 0)
}

/// Lock a container shallowly or through every reachable container.
pub fn lock_value(value: &Typval, deep: bool) -> Result<()> {
    fn lock(value: &Typval, scope: ox_types::LockScope, seen: &mut HashSet<(usize, u8)>) -> Result<()> {
        match value {
            Typval::List(reference) => {
                let key = (Rc::as_ptr(reference) as usize, ox_types::VAR_LIST);
                if !seen.insert(key) { return Ok(()); }
                let items = {
                    let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                    data.lock = ox_types::LockState { scope, locked: true };
                    data.items.clone()
                };
                if scope == ox_types::LockScope::Deep { for item in &items { lock(item, scope, seen)?; } }
            }
            Typval::Dict(reference) => {
                let key = (Rc::as_ptr(reference) as usize, ox_types::VAR_DICT);
                if !seen.insert(key) { return Ok(()); }
                let entries = {
                    let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                    data.lock = ox_types::LockState { scope, locked: true };
                    data.entries.clone()
                };
                if scope == ox_types::LockScope::Deep { for (_, item) in &entries { lock(item, scope, seen)?; } }
            }
            _ => {}
        }
        Ok(())
    }
    lock(value, if deep { ox_types::LockScope::Deep } else { ox_types::LockScope::Shallow }, &mut HashSet::new())
}

/// Return the encoded lock state: 0 unlocked, 1 direct, 2 shallow, 3 deep.
pub fn is_locked_value(value: &Typval) -> Result<Typval> {
    let lock = match value {
        Typval::List(reference) => reference.try_borrow().map_err(|_| borrow_error())?.lock,
        Typval::Dict(reference) => reference.try_borrow().map_err(|_| borrow_error())?.lock,
        _ => ox_types::LockState::default(),
    };
    let status = match (lock.locked, lock.scope) {
        (false, _) => 0,
        (true, ox_types::LockScope::None) => 1,
        (true, ox_types::LockScope::Shallow) => 2,
        (true, ox_types::LockScope::Deep) => 3,
    };
    Ok(Typval::Number(status))
}

fn blob2list(value: &Typval) -> Result<Typval> {
    let Typval::Blob(values) = value else { return Err(EvalError::new("E972", 0, "Blob required")); };
    Ok(Typval::list(values.iter().map(|value| Typval::Number(i64::from(*value))).collect()))
}

fn list2blob(value: &Typval) -> Result<Typval> {
    let Typval::List(values) = value else { return Err(EvalError::new("E714", 0, "List required")); };
    list_items(values)?.iter().map(|value| u8::try_from(number_arg(value)?).map_err(|_| EvalError::new("E1230", 0, "Blob value must be in range 0 to 255"))).collect::<Result<Vec<_>>>().map(Typval::Blob)
}

fn list2str(args: &[Typval]) -> Result<Typval> {
    let Typval::List(values) = &args[0] else { return Err(EvalError::new("E714", 0, "List required")); };
    let utf8 = args.get(1).is_some_and(Typval::is_truthy);
    let mut output = Vec::new();
    for value in list_items(values)? {
        let number = number_arg(&value)?;
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
    Ok(Typval::list(values))
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
    let digits: &[u8] = &digits;
    // STR2NR_FORCE: skip the base marker only when a valid digit follows it.
    let digits = match base {
        16 if digits.len() > 2 && (digits.starts_with(b"0x") || digits.starts_with(b"0X")) && digits[2].is_ascii_hexdigit() => &digits[2..],
        2 if digits.len() > 2 && (digits.starts_with(b"0b") || digits.starts_with(b"0B")) && matches!(digits[2], b'0' | b'1') => &digits[2..],
        8 if digits.len() > 2 && (digits.starts_with(b"0o") || digits.starts_with(b"0O")) && matches!(digits[2], b'0'..=b'7') => &digits[2..],
        _ => digits,
    };
    let normalized;
    let digits = if quoted {
        normalized = {
            let mut output = Vec::with_capacity(digits.len());
            let mut index = 0;
            while index < digits.len() {
                if digits[index] == b'\'' && !output.is_empty() && digits.get(index + 1).is_some_and(|byte| (*byte as char).to_digit(base as u32).is_some()) { index += 1; continue; }
                output.push(digits[index]);
                index += 1;
            }
            output
        };
        normalized.as_slice()
    } else { digits };
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
    fn encode(value: &Typval, depth: usize, active: &mut HashSet<(usize, u8)>, output: &mut String) -> Result<()> {
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
                output.push('['); for (index, value) in values.iter().enumerate() { if index > 0 { output.push(','); } let _ = write!(output, "{value}"); } output.push(']');
            }
            Typval::List(reference) => {
                let key = (Rc::as_ptr(reference) as usize, ox_types::VAR_LIST);
                if !active.insert(key) { return Err(EvalError::new("E724", 0, "recursive List cannot be JSON encoded")); }
                output.push('['); for (index, value) in list_items(reference)?.iter().enumerate() { if index > 0 { output.push(','); } encode(value, depth + 1, active, output)?; } output.push(']');
                active.remove(&key);
            }
            Typval::Dict(reference) => {
                let key = (Rc::as_ptr(reference) as usize, ox_types::VAR_DICT);
                if !active.insert(key) { return Err(EvalError::new("E724", 0, "recursive Dictionary cannot be JSON encoded")); }
                output.push('{');
                for (index, (name, value)) in dict_entries(reference)?.iter().enumerate() {
                    if index > 0 { output.push(','); }
                    output.push_str(&serde_json::to_string(&String::from_utf8_lossy(name.as_bytes())).map_err(|error| EvalError::new("E474", 0, error.to_string()))?);
                    output.push(':'); encode(value, depth + 1, active, output)?;
                }
                output.push('}'); active.remove(&key);
            }
            _ => return Err(EvalError::new("E474", 0, "value cannot be JSON encoded")),
        }
        Ok(())
    }
    let mut output = String::new();
    encode(value, 0, &mut HashSet::new(), &mut output)?;
    Ok(Typval::String(OxStr(output.into_bytes())))
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
        JsonValue::Array(values) => values.into_iter().map(|value| json_to_typval(value, depth + 1)).collect::<Result<Vec<_>>>().map(Typval::list),
        JsonValue::Object(values) => values.into_iter().map(|(key, value)| json_to_typval(value, depth + 1).map(|value| (OxStr(key.into_bytes()), value))).collect::<Result<Vec<_>>>().map(Typval::dict),
    }
}

fn vim_string(value: &Typval, _depth: usize) -> Result<OxStr> {
    fn render(value: &Typval, active: &mut HashSet<(usize, u8)>, output: &mut Vec<u8>) -> Result<()> {
        match value {
            Typval::String(value) => { output.push(b'\''); for byte in value.as_bytes() { output.push(*byte); if *byte == b'\'' { output.push(b'\''); } } output.push(b'\''); }
            Typval::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
            Typval::Float(value) => output.extend_from_slice(value.to_string().as_bytes()),
            Typval::Bool(value) => output.extend_from_slice(if *value { b"v:true" } else { b"v:false" }),
            Typval::Special(Special::Null) => output.extend_from_slice(b"v:null"),
            Typval::Blob(value) => { output.extend_from_slice(b"0z"); for byte in value { let _ = write!(StringWriter(output), "{byte:02X}"); } }
            Typval::List(reference) => {
                let key = (Rc::as_ptr(reference) as usize, ox_types::VAR_LIST);
                if !active.insert(key) { output.extend_from_slice(b"[...]"); return Ok(()); }
                output.push(b'[');
                for (index, item) in list_items(reference)?.iter().enumerate() { if index > 0 { output.extend_from_slice(b", "); } render(item, active, output)?; }
                output.push(b']'); active.remove(&key);
            }
            Typval::Dict(reference) => {
                let key = (Rc::as_ptr(reference) as usize, ox_types::VAR_DICT);
                if !active.insert(key) { output.extend_from_slice(b"{...}"); return Ok(()); }
                output.push(b'{');
                for (index, (name, item)) in dict_entries(reference)?.iter().enumerate() {
                    if index > 0 { output.extend_from_slice(b", "); }
                    render(&Typval::String(name.clone()), active, output)?; output.extend_from_slice(b": "); render(item, active, output)?;
                }
                output.push(b'}'); active.remove(&key);
            }
            Typval::Funcref(Funcref { name, .. }) | Typval::Partial(Funcref { name, .. }) => { output.extend_from_slice(b"function('"); output.extend_from_slice(name.as_bytes()); output.extend_from_slice(b"')"); }
            Typval::Channel(value) | Typval::Job(value) => output.extend_from_slice(value.to_string().as_bytes()),
        }
        Ok(())
    }
    let mut output = Vec::new();
    render(value, &mut HashSet::new(), &mut output)?;
    Ok(OxStr(output))
}
struct StringWriter<'a>(&'a mut Vec<u8>);
impl std::fmt::Write for StringWriter<'_> { fn write_str(&mut self, value: &str) -> std::fmt::Result { self.0.extend_from_slice(value.as_bytes()); Ok(()) } }

fn values_equal(left: &Typval, right: &Typval, ignore_case: bool, depth: usize) -> Result<bool> {
    fn equal(left: &Typval, right: &Typval, ignore_case: bool, depth: usize, seen: &mut HashSet<(usize, usize, u8)>) -> Result<bool> {
        if depth >= MAX_CONTAINER_DEPTH { return Err(EvalError::new("E724", 0, "too much recursion comparing values")); }
        match (left, right) {
            (Typval::String(left), Typval::String(right)) => Ok(if ignore_case { left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase() } else { left == right }),
            (Typval::List(left), Typval::List(right)) => {
                let pair=(Rc::as_ptr(left) as usize,Rc::as_ptr(right) as usize,ox_types::VAR_LIST); if !seen.insert(pair) { return Ok(true); }
                let left=list_items(left)?; let right=list_items(right)?; if left.len()!=right.len(){return Ok(false)}
                for (left,right) in left.iter().zip(&right){if !equal(left,right,ignore_case,depth+1,seen)?{return Ok(false)}} Ok(true)
            }
            (Typval::Dict(left), Typval::Dict(right)) => {
                let pair=(Rc::as_ptr(left) as usize,Rc::as_ptr(right) as usize,ox_types::VAR_DICT); if !seen.insert(pair) { return Ok(true); }
                let left=dict_entries(left)?; let right=dict_entries(right)?; if left.len()!=right.len(){return Ok(false)}
                for (key,value) in &left { let Some((_,other))=right.iter().find(|(candidate,_)|candidate==key) else{return Ok(false)}; if !equal(value,other,ignore_case,depth+1,seen)?{return Ok(false)} } Ok(true)
            }
            _ => Ok(left == right),
        }
    }
    equal(left, right, ignore_case, depth, &mut HashSet::new())
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
