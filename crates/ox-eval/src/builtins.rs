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

/// Upstream's `VAR_TYPE_*` values (`eval/typval_defs.h:123-133`): the numbers
/// `type()` returns and the `v:t_*` variables hold. These are a *public*
/// numbering, deliberately distinct from the internal `VAR_*` discriminants in
/// `ox_types` — `f_type` (`eval/funcs.c:7570-7597`) translates one to the
/// other, and the two disagree for every type but Blob.
const VAR_TYPE_NUMBER: i64 = 0;
const VAR_TYPE_STRING: i64 = 1;
const VAR_TYPE_FUNC: i64 = 2;
const VAR_TYPE_LIST: i64 = 3;
const VAR_TYPE_DICT: i64 = 4;
const VAR_TYPE_FLOAT: i64 = 5;
const VAR_TYPE_BOOL: i64 = 6;
const VAR_TYPE_SPECIAL: i64 = 7;
const VAR_TYPE_BLOB: i64 = 10;

/// The read-only `v:t_*` type constants, in the order `set_vim_var_nr` defines
/// them (`eval/vars.c:324-331`). `VAR_TYPE_SPECIAL` has no `v:t_` name
/// upstream — `exists('v:t_special')` is 0 on the oracle — so it has none here.
pub const VIM_TYPE_VARS: [(&[u8], i64); 8] = [
    (b"v:t_number", VAR_TYPE_NUMBER),
    (b"v:t_string", VAR_TYPE_STRING),
    (b"v:t_func", VAR_TYPE_FUNC),
    (b"v:t_list", VAR_TYPE_LIST),
    (b"v:t_dict", VAR_TYPE_DICT),
    (b"v:t_float", VAR_TYPE_FLOAT),
    (b"v:t_bool", VAR_TYPE_BOOL),
    (b"v:t_blob", VAR_TYPE_BLOB),
];

/// `f_type` (`eval/funcs.c:7570-7597`): the public type number of a value.
///
/// A Partial answers `VAR_TYPE_FUNC` alongside a Funcref, as upstream's
/// `case VAR_PARTIAL:` falls through to `case VAR_FUNC:`. Channel and Job are
/// Numbers upstream and answer `VAR_TYPE_NUMBER`.
#[must_use]
pub const fn type_constant(value: &Typval) -> i64 {
    match value {
        Typval::Number(_) | Typval::Channel(_) | Typval::Job(_) => VAR_TYPE_NUMBER,
        Typval::String(_) => VAR_TYPE_STRING,
        Typval::Funcref(_) | Typval::Partial(_) => VAR_TYPE_FUNC,
        Typval::List(_) => VAR_TYPE_LIST,
        Typval::Dict(_) => VAR_TYPE_DICT,
        Typval::Float(_) => VAR_TYPE_FLOAT,
        Typval::Bool(_) => VAR_TYPE_BOOL,
        Typval::Special(_) => VAR_TYPE_SPECIAL,
        Typval::Blob(_) => VAR_TYPE_BLOB,
    }
}

/// The value of a `v:t_*` constant, by fully qualified name.
#[must_use]
pub fn vim_type_var(name: &[u8]) -> Option<i64> {
    VIM_TYPE_VARS.iter().find_map(|(key, value)| (*key == name).then_some(*value))
}

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
        if !is_builtin_implemented(name) {
            return Err(EvalError::not_implemented(OxStr::from(name)));
        }
        check_arity(spec, args.len())?;
        match name {
            "abs" => absolute(&args[0]),
            "add" => add(args),
            "and" => binary_number(&args, |left, right| left & right),
            // `float_op_wrapper` (`funcs.c:344`) hands the value to the
            // same-named libm function; `func_float` in `eval.lua` names it.
            "acos" => float_unary(&args[0], f64::acos),
            "asin" => float_unary(&args[0], f64::asin),
            "atan" => float_unary(&args[0], f64::atan),
            "atan2" => float_binary(&args, f64::atan2),
            "cos" => float_unary(&args[0], f64::cos),
            "cosh" => float_unary(&args[0], f64::cosh),
            "exp" => float_unary(&args[0], f64::exp),
            "fmod" => float_binary(&args, |left, right| left % right),
            "isinf" => Ok(Typval::Number(float_infinity_sign(&args[0]))),
            "isnan" => Ok(Typval::Number(i64::from(
                matches!(&args[0], Typval::Float(number) if number.is_nan()),
            ))),
            "log" => float_unary(&args[0], f64::ln),
            "log10" => float_unary(&args[0], f64::log10),
            "round" => float_unary(&args[0], f64::round),
            "sin" => float_unary(&args[0], f64::sin),
            "sinh" => float_unary(&args[0], f64::sinh),
            "tan" => float_unary(&args[0], f64::tan),
            "tanh" => float_unary(&args[0], f64::tanh),
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
            "findfile" => path_builtins::findfilendir(self.regex, &args, scope, crate::find_file::FindWhat::File),
            "finddir" => path_builtins::findfilendir(self.regex, &args, scope, crate::find_file::FindWhat::Dir),
            "float2nr" => float_to_number(&args[0]),
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
            "matchstrlist" => self.matchstrlist(&args),
            "matchfuzzy" | "matchfuzzypos" => self.matchfuzzy(name, &args, scope),
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
            "setenv" => setenv(&args, scope),
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
            "tempname" => path_builtins::tempname(),
            "tr" => translate(&args),
            // `trunc` is `float_op_wrapper` over libm's `trunc` (`eval.lua`),
            // so it answers with a Float: `trunc(4.8)` is `4.0`, not `4`. It
            // shared `float2nr`'s arm here and answered a Number.
            "trunc" => float_unary(&args[0], f64::trunc),
            "utf16idx" => utf16idx(&args),
            "charidx" => charidx(&args),
            "type" => Ok(Typval::Number(type_constant(&args[0]))),
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

    fn matchstrlist(&self, args: &[Typval]) -> Result<Typval> {
        let items = match &args[0] {
            Typval::List(reference) => list_items(reference)?,
            Typval::Special(Special::Null) => Vec::new(),
            _ => return Err(EvalError::new("E1211", 0, "List required for argument 1")),
        };
        let pattern = match &args[1] {
            Typval::String(pattern) => pattern.clone(),
            Typval::Special(Special::Null) => OxStr(Vec::new()),
            _ => return Err(EvalError::new("E1174", 0, "String required for argument 2")),
        };
        let include_submatches = match args.get(2) {
            None | Some(Typval::Special(Special::Null)) => false,
            Some(Typval::Dict(reference)) => {
                let value = dict_entries(reference)?
                    .into_iter()
                    .find(|(key, _)| key.as_bytes() == b"submatches")
                    .map(|(_, value)| value);
                match value {
                    None => false,
                    Some(Typval::Bool(value)) => value,
                    Some(Typval::Number(value)) if matches!(value, 0 | 1) => value != 0,
                    Some(_) => return Err(EvalError::new("E475", 0, "Invalid value for argument submatches")),
                }
            }
            Some(_) => return Err(EvalError::new("E1206", 0, "Dictionary required for argument 3")),
        };

        let mut matches = Vec::new();
        for (index, value) in items.iter().enumerate() {
            let text = match value {
                Typval::String(text) => text.clone(),
                Typval::Special(Special::Null) => OxStr(Vec::new()),
                _ => string_arg(value)?,
            };
            let Some(found) = self.regex()?.find_captures(&text, &pattern, 0)? else { continue };
            let mut entry = vec![
                (OxStr::from("idx"), Typval::Number(saturating_i64(index))),
                (OxStr::from("byteidx"), Typval::Number(saturating_i64(found.start))),
                (OxStr::from("text"), Typval::String(OxStr(text.as_bytes()[found.start..found.end].to_vec()))),
            ];
            if include_submatches {
                let mut captures = found.captures.into_iter().take(9).map(|range| {
                    Typval::String(range.map_or_else(
                        || OxStr(Vec::new()),
                        |(start, end)| OxStr(text.as_bytes()[start..end].to_vec()),
                    ))
                }).collect::<Vec<_>>();
                captures.resize(9, Typval::String(OxStr(Vec::new())));
                entry.push((OxStr::from("submatches"), Typval::list(captures)));
            }
            matches.push(Typval::dict(entry));
        }
        Ok(Typval::list(matches))
    }

    /// `matchfuzzy()`/`matchfuzzypos()` — `do_fuzzymatch` (`fuzzy.c:349-417`)
    /// driving `fuzzy_match_in_list` (`fuzzy.c:200-345`). Scoring lives in
    /// [`crate::fuzzy`]; this method owns argument validation, the `key` /
    /// `text_cb` / `limit` / `matchseq` options, the tie-break sort, and the
    /// two return shapes.
    fn matchfuzzy(&mut self, name: &str, args: &[Typval], scope: &mut Scope) -> Result<Typval> {
        let retmatchpos = name == "matchfuzzypos";
        let Typval::List(reference) = &args[0] else {
            return Err(EvalError::new("E686", 0, format!("Argument of {name}() must be a List")));
        };
        let pattern = match &args[1] {
            Typval::String(value) => value.clone(),
            other => {
                let rendered = string_arg(other)?;
                return Err(EvalError::new("E475", 0, format!("Invalid argument: {}", rendered.to_string_lossy())));
            }
        };

        let options = fuzzy_options(args.get(2))?;

        let pattern_chars = crate::fuzzy::composed_chars(pattern.as_bytes());
        let mut found: Vec<FuzzyItem> = Vec::new();
        for (index, item) in list_items(reference)?.into_iter().enumerate() {
            if options.limit > 0 && saturating_i64(found.len()) >= options.limit {
                break;
            }
            let Some(text) = self.fuzzy_item_text(&item, options.key.as_ref(), options.text_cb.as_ref(), scope)? else {
                continue;
            };
            let haystack = crate::fuzzy::composed_chars(text.as_bytes());
            let Some(matched) = crate::fuzzy::fuzzy_match(&haystack, &pattern_chars, options.matchseq) else {
                continue;
            };
            found.push(FuzzyItem { index, item, score: matched.score, positions: matched.positions, text });
        }

        // `fuzzy_match_item_compare` (`fuzzy.c:162-189`): score descending,
        // then an exact prefix match at the first matched position wins, then
        // the original order. Upstream indexes `itemstr` with the character
        // position `matches[0]` as if it were a byte offset; that quirk is
        // observable, so it is reproduced.
        found.sort_by(|left, right| {
            right.score.cmp(&left.score).then_with(|| {
                let exact = |item: &FuzzyItem| {
                    let offset = item.positions.first().copied().unwrap_or(0);
                    item.text.as_bytes().get(offset..).is_some_and(|tail| tail.starts_with(pattern.as_bytes()))
                };
                exact(right).cmp(&exact(left)).then_with(|| left.index.cmp(&right.index))
            })
        });

        let items = found.iter().map(|entry| entry.item.clone()).collect();
        if !retmatchpos {
            return Ok(Typval::list(items));
        }
        let positions = found
            .iter()
            .map(|entry| {
                // `fuzzy.c:264-276`: one position per pattern character,
                // skipping blanks unless "matchseq" was given.
                let mut slot = 0usize;
                let mut values = Vec::new();
                for character in &pattern_chars {
                    if slot >= crate::fuzzy::MATCH_MAX_LEN {
                        break;
                    }
                    if options.matchseq || !matches!(character, ' ' | '\t') {
                        values.push(Typval::Number(saturating_i64(entry.positions.get(slot).copied().unwrap_or(0))));
                        slot += 1;
                    }
                }
                Typval::list(values)
            })
            .collect();
        let scores = found.iter().map(|entry| Typval::Number(i64::from(entry.score))).collect();
        Ok(Typval::list(vec![Typval::list(items), Typval::list(positions), Typval::list(scores)]))
    }

    /// The string a `matchfuzzy()` list item contributes: the item itself for
    /// a String, the `key` entry or the `text_cb` result for a Dict, and
    /// nothing for anything else, so the item is skipped (`fuzzy.c:224-254`).
    fn fuzzy_item_text(
        &mut self,
        item: &Typval,
        key: Option<&OxStr>,
        text_cb: Option<&Typval>,
        scope: &Scope,
    ) -> Result<Option<OxStr>> {
        match item {
            Typval::String(text) => Ok(Some(text.clone())),
            Typval::Dict(entries) => {
                if let Some(key) = key {
                    return dict_entries(entries)?
                        .iter()
                        .find(|(candidate, _)| candidate == key)
                        .map(|(_, value)| string_arg(value))
                        .transpose();
                }
                let Some(callback) = text_cb else { return Ok(None) };
                let regex = RegexRef(self.regex);
                let result = Evaluator::new(self, &regex)
                    .invoke(callback.clone(), vec![item.clone()], &mut scope.snapshot())?;
                Ok(match result {
                    Typval::String(text) => Some(text),
                    _ => None,
                })
            }
            _ => Ok(None),
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

/// `fuzzyItem_T` (`fuzzy.c:56-65`), reduced to the fields the two return
/// shapes and the tie-break comparator need.
struct FuzzyItem {
    index: usize,
    item: Typval,
    score: i32,
    positions: Vec<usize>,
    text: OxStr,
}

/// The optional third argument of `matchfuzzy()`/`matchfuzzypos()`.
#[derive(Default)]
struct FuzzyOptions {
    key: Option<OxStr>,
    text_cb: Option<Typval>,
    limit: i64,
    matchseq: bool,
}

/// Parse the `{dict}` argument of `matchfuzzy()` (`fuzzy.c:363-399`).
/// `text_cb` is consulted only when `key` is absent, and `matchseq` is keyed
/// on presence rather than value.
fn fuzzy_options(value: Option<&Typval>) -> Result<FuzzyOptions> {
    let Some(value) = value else { return Ok(FuzzyOptions::default()) };
    let Typval::Dict(value) = value else {
        return Err(EvalError::new("E1206", 0, "Dictionary required for argument 3"));
    };
    let entries = dict_entries(value)?;
    let entry = |wanted: &[u8]| {
        entries.iter().find(|(candidate, _)| candidate.as_bytes() == wanted).map(|(_, value)| value)
    };
    let mut options = FuzzyOptions::default();
    if let Some(value) = entry(b"key") {
        match value {
            Typval::String(text) if !text.as_bytes().is_empty() => options.key = Some(text.clone()),
            _ => {
                let rendered = string_arg(value)?;
                return Err(EvalError::new("E475", 0, format!("Invalid value for argument key: {}", rendered.to_string_lossy())));
            }
        }
    } else if let Some(value) = entry(b"text_cb") {
        // `tv_dict_get_callback` (`typval.c:2506-2529`) rejects a
        // non-function, non-String value with E6000; then
        // `callback_from_typval` rejects a String starting with a digit with
        // E921. An empty String leaves no callback at all.
        options.text_cb = match value {
            Typval::Funcref(_) | Typval::Partial(_) => Some(value.clone()),
            Typval::String(function) => match function.as_bytes().first() {
                None => None,
                Some(b'0'..=b'9') => return Err(EvalError::new("E921", 0, "Invalid callback argument")),
                Some(_) => Some(Typval::Funcref(Funcref {
                    name: function.clone(),
                    args: Vec::new(),
                    dict: None,
                    registry: None,
                })),
            },
            _ => return Err(EvalError::new("E6000", 0, "Argument is not a function or function name")),
        };
    }
    if let Some(value) = entry(b"limit") {
        let Typval::Number(value) = value else {
            return Err(EvalError::new("E475", 0, "Invalid value for argument limit"));
        };
        options.limit = *value;
    }
    options.matchseq = entry(b"matchseq").is_some();
    Ok(options)
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
        Typval::String(text) => crate::eval::string_to_number(text.as_bytes()),
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

/// Whether this port implements `name`, as opposed to merely carrying its
/// entry in the generated `eval.lua` metadata table.
///
/// [`builtin_spec`] answers the *inventory* question — arity, method
/// eligibility, the name upstream declares — for every builtin Neovim has.
/// [`Builtins::dispatch`] serves only the subset below and reports `E117` for
/// the rest, so this predicate, not `builtin_spec`, is what `exists('*name')`
/// has to key on. `check.vim`'s `CheckFunction` is `exists('*' .. name)`, so a
/// name that answers 1 and then reports `not implemented` turns an honest skip
/// into a wall of failures.
#[must_use]
pub fn is_builtin_implemented(name: &str) -> bool {
    matches!(name,
        "abs" | "acos" | "add" | "and" | "asin" | "atan" | "atan2" | "blob2list" | "ceil" |
        "char2nr" | "copy" | "cos" | "cosh" | "count" | "exp" | "fmod" | "isinf" | "isnan" |
        "log" | "log10" | "round" | "sin" | "sinh" | "tan" | "tanh" |
        "deepcopy" | "empty" | "escape" | "executable" | "exepath" | "exists" | "extend" | "extendnew" | "filter" | "flatten" |
        "flattennew" | "foreach" | "float2nr" | "floor" | "fnamemodify" | "finddir" | "findfile" | "get" | "gettext" | "getcwd" | "getpid" | "has" | "has_key" | "hostname" | "index" | "insert" | "items" |
        "indexof" | "isabsolutepath" | "islocked" | "join" | "json_decode" | "json_encode" | "keytrans" | "keys" | "len" | "strlen" | "list2blob" | "list2str" | "map" | "mapnew" |
        "match" | "matchend" | "matchstr" | "matchlist" | "matchstrpos" | "matchstrlist" | "matchfuzzy" | "matchfuzzypos" |
        "max" | "min" | "nr2char" | "or" | "pathshorten" | "pow" | "printf" | "range" | "reduce" | "resolve" |
        "remove" | "repeat" | "reverse" | "setenv" | "simplify" | "slice" | "sort" | "split" | "sqrt" | "str2float" | "str2list" |
        "str2nr" | "strcharlen" | "strchars" | "stridx" | "string" | "strpart" | "strridx" | "strtrans" | "strutf16len" | "strwidth" |
        "substitute" | "tempname" | "tolower" | "toupper" | "tr" | "trim" | "trunc" | "type" | "uniq" | "utf16idx" | "charidx" | "values" | "xor"
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
        // `f_exists` calls `function_exists`, which asks whether the function
        // can be *called* — so the answer is the implemented subset, not the
        // generated inventory (see [`is_builtin_implemented`]).
        Some(b'*') => std::str::from_utf8(&bytes[1..])
            .ok()
            .is_some_and(is_builtin_implemented),
        Some(b':' | b'#') | None => false,
        _ => matches!(bytes, b"v:true" | b"v:false" | b"v:null" | b"v:none")
            || vim_type_var(bytes).is_some()
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

/// `tv_get_number_chk` (`typval.c:4292-4325`). The String arm is
/// `vim_str2nr(…, STR2NR_ALL, …)`, shared with the evaluator's own coercion
/// rather than restated: this used to be a decimal-only prefix scan, so
/// `abs('-12')` was 0 and `abs('0x10')` was 0 against the oracle's 12 and 16.
/// The error codes are `num_errors` (`typval.c:4171-4181`), which gives a
/// Funcref E703 and a Blob E974.
fn number_arg(value: &Typval) -> Result<i64> {
    match value {
        Typval::Number(number) => Ok(*number),
        Typval::Bool(value) => Ok(i64::from(*value)),
        Typval::Special(Special::Null) => Ok(0),
        Typval::String(value) => Ok(crate::eval::string_to_number(value.as_bytes())),
        Typval::Float(_) => Err(EvalError::new("E805", 0, "Using a Float as a Number")),
        Typval::Funcref(_) | Typval::Partial(_) => Err(EvalError::new("E703", 0, "Using a Funcref as a Number")),
        Typval::List(_) => Err(EvalError::new("E745", 0, "Using a List as a Number")),
        Typval::Dict(_) => Err(EvalError::new("E728", 0, "Using a Dictionary as a Number")),
        Typval::Blob(_) => Err(EvalError::new("E974", 0, "Using a Blob as a Number")),
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
        Typval::Float(number) => Ok(float_as_string(*number)),
        Typval::List(_) => Err(EvalError::new("E730", 0, "Using a List as a String")),
        Typval::Dict(_) => Err(EvalError::new("E731", 0, "Using a Dictionary as a String")),
        Typval::Blob(_) => Err(EvalError::new("E976", 0, "Using a Blob as a String")),
        Typval::Funcref(_) | Typval::Partial(_) => Err(EvalError::new("E729", 0, "Using a Funcref as a String")),
        _ => Err(EvalError::new("E729", 0, "Using invalid value as a String")),
    }
}

/// `f_setenv` (`eval/funcs.c`) is `os_setenv`/`os_unsetenv`, so the assignment
/// changes the process environment. oxvim additionally keeps a snapshot of the
/// environment in `Scope::env`, taken once at startup, and `$VAR` reads come
/// from that snapshot; upstream has no snapshot and reads the live environment
/// through `os_getenv` every time. Writing only the process environment
/// therefore left `setenv('X', 'v')` invisible to `echo $X` in the same
/// session, so both are updated here, exactly as `:let $VAR` does.
fn setenv(args: &[Typval], scope: &mut Scope) -> Result<Typval> {
    let name = string_arg(&args[0])?.to_string_lossy().into_owned();
    if args[1] == Typval::Special(Special::Null) {
        ox_sys::unset_env(&name);
        scope.unset_env(name.as_bytes());
    } else {
        let value = string_arg(&args[1])?.to_string_lossy().into_owned();
        ox_sys::set_env(&name, &value);
        scope.set_env(name.as_bytes(), Typval::String(OxStr::from(value.as_str())));
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

/// `f_isinf` (`funcs.c:3141`): the sign of an infinite Float, and 0 for
/// everything else — including a Number, which never carries an infinity.
fn float_infinity_sign(value: &Typval) -> i64 {
    match value {
        Typval::Float(number) if number.is_infinite() => {
            if *number > 0.0 { 1 } else { -1 }
        }
        _ => 0,
    }
}

/// `f_float2nr` (`funcs.c:1484-1500`). The bounds are
/// `f <= (float_T)(-VARNUMBER_MAX) + DBL_EPSILON` and
/// `f >= (float_T)VARNUMBER_MAX - DBL_EPSILON`, and the two `DBL_EPSILON`
/// terms do nothing: `(double)VARNUMBER_MAX` is exactly 2^63, whose
/// neighbouring doubles are 1024 apart, so 2.2e-16 is absorbed and both
/// comparisons are against ±2^63. Inside that range the C cast truncates
/// toward zero.
///
/// The saturation value is `±VARNUMBER_MAX`, so the low end is
/// -9223372036854775807 and not `VARNUMBER_MIN` — the off-by-one this fixes.
/// NaN fails both comparisons, since every comparison with a NaN is false,
/// and reaches the cast, which on x86-64 gives `INT64_MIN`. Measured on the
/// oracle: `float2nr(-1.0/0.0)` is -9223372036854775807 and
/// `float2nr(0.0/0.0)` is -9223372036854775808.
fn float_to_number(value: &Typval) -> Result<Typval> {
    let value = float_arg(value)?;
    // `i64::MAX as f64` rounds up to 2^63, which is the bound upstream
    // compares against after its own `(float_T)` conversion does the same.
    let limit = i64::MAX as f64;
    let number = if value <= -limit {
        -i64::MAX
    } else if value >= limit {
        i64::MAX
    } else if value.is_nan() {
        i64::MIN
    } else {
        value.trunc() as i64
    };
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

/// `f_len` (`funcs.c:3793-3819`) and `f_strlen` (`funcs.c`) are two different
/// questions and were one function here, which the Float-as-String fix made
/// visible: with a Float now rendering, `len(1.0)` would have answered 3 where
/// upstream answers E701.
///
/// `len()` counts a container's elements and the string length of a String or
/// a Number, and refuses everything else with E701 — a Bool, a Special, a
/// Float and a Funcref all reach that arm. `strlen()` is only
/// `strlen(tv_get_string(...))`, so it coerces: `strlen(v:true)` is 6 and
/// `strlen(1.0)` is 3, while a List, Dict, Blob or Funcref raises its own
/// String error out of `string_arg`.
fn length(value: &Typval, string_length: bool) -> Result<Typval> {
    if string_length {
        return Ok(Typval::Number(saturating_i64(string_arg(value)?.as_bytes().len())));
    }
    let length = match value {
        Typval::String(value) => value.as_bytes().len(),
        Typval::Number(value) => value.to_string().len(),
        Typval::Blob(value) => value.len(),
        Typval::List(value) => list_items(value)?.len(),
        Typval::Dict(value) => dict_entries(value)?.len(),
        _ => return Err(EvalError::new("E701", 0, "Invalid type for len()")),
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
        let mut zero = flags.contains('0') && !left;
        // `strings.c:1516,1585`: `space_for_positive` starts set and only `+`
        // clears it, so `+` wins over ` ` whichever order they appear in.
        let force_sign = flags.contains('+') || flags.contains(' ');
        let space_for_positive = !flags.contains('+');
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
                // `tv_float` (`strings.c:716`) has its own error, distinct from
                // the E808 `tv_get_float` raises for `sqrt("a")`.
                let number = match value {
                    Typval::Float(number) => *number,
                    Typval::Number(number) => *number as f64,
                    _ => return Err(EvalError::new("E807", 0, "Expected Float argument for printf()")),
                };
                let (rendered, numeric) =
                    format_float(conversion, number, precision, force_sign, space_for_positive);
                // `infinity_str` and `nan` both clear `zero_padding`
                // (`strings.c:2109`, `2114`), so `%06f` of infinity pads with
                // blanks.
                zero &= numeric;
                rendered
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
        } else if zero
            && matches!(
                conversion,
                'd' | 'i' | 'u' | 'x' | 'X' | 'o' | 'b' | 'B' | 'c' | 'f' | 'F' | 'e' | 'E' | 'g' | 'G'
            )
        {
            // `strings.c:2188-2192`: padding zeroes go after the sign, and
            // `space_for_positive` puts a blank there instead of a `+`.
            let (sign, digits) = match rendered.strip_prefix(['-', '+', ' ']) {
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

/// `vim_snprintf`'s float conversions (`strings.c:2075-2196`), returning the
/// rendering and whether zero padding still applies to it.
///
/// C's `%g` prints `1.0` as `1`, which upstream refuses ("can't use %g
/// directly"): it rewrites `%g` to `%f` or `%e` by magnitude and then strips
/// the superfluous zeroes itself, keeping the one just after the dot. That one
/// kept zero is why `string(1.0)` is `'1.0'` and not `'1'`, and scripts compare
/// against it.
fn format_float(
    conversion: char,
    number: f64,
    precision: Option<usize>,
    force_sign: bool,
    space_for_positive: bool,
) -> (String, bool) {
    let magnitude = number.abs();
    let mut spec = conversion;
    let mut remove_trailing_zeroes = false;
    if matches!(spec, 'g' | 'G') {
        let upper = spec == 'G';
        spec = if (0.001..1.0e7).contains(&magnitude) || magnitude == 0.0 {
            if upper { 'F' } else { 'f' }
        } else if upper {
            'E'
        } else {
            'e'
        };
        remove_trailing_zeroes = true;
    }
    let upper = spec.is_ascii_uppercase();

    // `infinity_str` (`strings.c:800`) ignores the sign flags for a negative
    // value, and `%f` gives up on anything past 1e307 rather than print 300
    // digits.
    if number.is_infinite() || (matches!(spec, 'f' | 'F') && magnitude > 1.0e307) {
        let sign = if number < 0.0 {
            "-"
        } else if !force_sign {
            ""
        } else if space_for_positive {
            " "
        } else {
            "+"
        };
        return (format!("{sign}{}", if upper { "INF" } else { "inf" }), false);
    }
    if number.is_nan() {
        // Not a number has no sign, not even a forced one.
        return ((if upper { "NAN" } else { "nan" }).to_owned(), false);
    }

    // `TMP_LEN - 10`, less the integer digits when `%f` has any to print.
    let mut digits = precision.unwrap_or(6);
    if precision.is_some() {
        let mut limit = 340usize;
        if matches!(spec, 'f' | 'F') && magnitude > 1.0 {
            limit -= magnitude.log10() as usize;
        }
        digits = digits.min(limit);
    }
    let mut rendered = if matches!(spec, 'e' | 'E') {
        // Rust writes `1.23e2`; C writes `1.230000e+02`.
        let plain = format!("{number:.digits$e}");
        let (mantissa, exponent) = plain.split_once('e').expect("LowerExp emits an exponent");
        let exponent: i32 = exponent.parse().expect("LowerExp emits a decimal exponent");
        let marker = if spec == 'E' { 'E' } else { 'e' };
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}{marker}{sign}{:02}", exponent.abs())
    } else {
        format!("{number:.digits$}")
    };
    if force_sign && !rendered.starts_with('-') {
        rendered.insert(0, if space_for_positive { ' ' } else { '+' });
    }
    if remove_trailing_zeroes {
        rendered = strip_superfluous_zeroes(&rendered, matches!(spec, 'e' | 'E'), precision.is_some());
    }
    (rendered, true)
}

/// The `remove_trailing_zeroes` half of the float conversion
/// (`strings.c:2144-2176`), which only `%g`/`%G` ask for.
///
/// An exponent loses its `+` and its leading zeroes unconditionally; the
/// mantissa loses its trailing zeroes only when no precision was given, and
/// never the one directly after the dot.
fn strip_superfluous_zeroes(rendered: &str, exponential: bool, precision_specified: bool) -> String {
    let mut text: Vec<char> = rendered.chars().collect();
    let mut mantissa_end = text.len();
    if exponential {
        let Some(marker) = text.iter().position(|character| matches!(character, 'e' | 'E')) else {
            return rendered.to_owned();
        };
        let mut cursor = marker + 1;
        if text.get(cursor) == Some(&'+') {
            text.remove(cursor);
        } else if text.get(cursor) == Some(&'-') {
            cursor += 1;
        }
        while text.get(cursor) == Some(&'0') && cursor + 1 < text.len() {
            text.remove(cursor);
        }
        mantissa_end = marker;
    }
    if !precision_specified {
        // The kept zero is the one directly after the dot. Upstream also
        // bounds the loop at `tp > tmp + 2`, which never fires: `%f` and
        // `%e` always emit that dot ahead of the zeroes, so the dot is what
        // stops the loop, and `> 2` here is only index safety.
        while mantissa_end > 2 && text[mantissa_end - 1] == '0' && text[mantissa_end - 2] != '.' {
            text.remove(mantissa_end - 1);
            mantissa_end -= 1;
        }
    }
    text.into_iter().collect()
}

/// `tv_get_string_buf_chk`'s `VAR_FLOAT` arm (`eval/typval.c:4684-4685`):
/// `vim_snprintf(buf, NUMBUFLEN, "%g", …)`, which never fails. A Float
/// coerces to a String wherever upstream wants a String — `1.0 . ''` is
/// `'1.0'`, `strlen(1.0)` is 3 — and `tv_check_str` (`typval.c:4245`) accepts
/// `VAR_FLOAT` for the same reason.
///
/// E806 is deliberately absent here. Upstream raises it in exactly one place,
/// `check_can_index` (`eval.c:3225-3229`), so it is the answer for `1.0[0]`
/// and `1.0[1:2]` and for nothing else.
pub fn float_as_string(number: f64) -> OxStr {
    OxStr(format_float('g', number, None, false, true).0.into_bytes())
}

/// `TYPVAL_ENCODE_CONV_FLOAT` (`eval/encode.c:351-372`): `%g` for a finite
/// value, and a re-readable `str2float()` call for the two that `%g` cannot
/// round-trip.
fn vim_float_string(number: f64) -> String {
    if number.is_nan() {
        return "str2float('nan')".to_owned();
    }
    if number.is_infinite() {
        let sign = if number < 0.0 { "-" } else { "" };
        return format!("{sign}str2float('inf')");
    }
    format_float('g', number, None, false, true).0
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

/// Feature names this build genuinely provides, answered by `has()`.
///
/// Upstream's `has_list` (`eval/funcs.c:2532-2667`) is a compile-time list of
/// everything *Neovim* provides, so copying it wholesale would make `has()`
/// lie: a test that stops skipping runs code paths this rewrite does not have,
/// which turns an honest skip into a wall of noise. Every entry below was
/// admitted only after the capability it names was exercised against the
/// oracle and matched; the subsystem that answers for it is cited. Names
/// upstream lists but this build does not implement are deliberately absent,
/// so they keep returning 0 — see `.outline/sdd/reports/task-63.md` for the
/// per-name evidence and for what each omission is still missing.
///
/// Sorted for `binary_search`; `f_has` compares with `STRICMP`, so lookups
/// lowercase the query first. The trailing comment on each line names the
/// module that answers for the feature, or the probe that proved it.
pub(crate) const FEATURES: &[&str] = &[
    "eval",                // ox-eval/eval.rs: `eval("1+2")` == 3
    "file_in_path",        // ox-eval/find_file.rs: `findfile()` honours 'path'
    "float",               // ox-eval Typval::Float: arithmetic, str2float, float2nr, sqrt, floor
    "fork",                // ox-uv/process.rs forks for `system()`
    "lambda",              // ox-eval/parser.rs: `{a, b -> a * b}(6, 7)` == 42
    "modify_fname",        // ox-eval/path_builtins.rs: `fnamemodify()` modifiers
    "multi_byte",          // ox-eval: strchars/strlen agree on multi-byte input
    "multi_byte_encoding", // ox-eval: char2nr/nr2char round-trip; upstream is unconditional
    "num64",               // ox-eval Typval::Number is i64
    "nvim",                // this build targets Neovim 0.13 (ox_rpc API_LEVEL 15)
    "path_extra",          // ox-eval/find_file.rs: `**` downward and `dir;` upward search
    "startuptime",         // oxvim/cli.rs implements `--startuptime`
    "textobjects",         // ox-editor: `daw` deletes a word with its white space
    "user-commands",       // the spelling upstream keeps for 5.4 compatibility
    "user_commands",       // ox-editor/excmd_exec.rs: `:command! -nargs=1` plus `<f-args>`
    "vertsplit",           // ox-editor/layout.rs: `:vsplit` yields two windows
    "vimscript-1",         // legacy Vimscript is the dialect ox-eval implements
    "visual",              // ox-editor: `v2ld` deletes the Visual selection
    "windows",             // ox-editor/layout.rs: `:split` yields two windows
];

/// `"has"` — feature probe. Mirrors `f_has` in `eval/funcs.c`: the
/// `"nvim-X.Y[.Z]"` form compares against the Neovim version this build
/// targets (0.13.0, matching `ox_rpc`'s `API_LEVEL = 15`); everything else
/// answers from [`FEATURES`], which lists only capabilities this build was
/// observed to provide, and defaults to 0, which is what upstream returns for
/// features the build does not provide.
fn has_feature(args: &[Typval]) -> Result<Typval> {
    let feature = string_arg(&args[0])?.to_string_lossy().to_ascii_lowercase();
    let supported = if let Some(version) = feature.strip_prefix("nvim-") {
        let mut parts = version.split('.').map(|part| part.parse::<u64>().unwrap_or(u64::MAX));
        let requested = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        requested <= (0, 13, 0) && parts.next().is_none()
    } else {
        match feature.as_str() {
            "unix" => cfg!(unix),
            "win32" | "win64" => cfg!(windows),
            "macunix" => cfg!(target_os = "macos"),
            // `#ifndef CASE_INSENSITIVE_FILENAME` and `#ifdef __linux__`.
            "fname_case" => cfg!(not(any(target_os = "macos", windows))),
            "linux" => cfg!(target_os = "linux"),
            name => FEATURES.binary_search(&name).is_ok(),
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

/// `tv_item_lock` (`eval/typval.c`): lock or unlock a container `depth`
/// levels down, where `depth` 0 changes nothing and a negative `depth`
/// reaches every nested container. `:lockvar` uses 2, `:lockvar!` uses -1.
///
/// The recorded [`ox_types::LockScope`] is what `islocked()` reports:
/// `Shallow` for a single level, `Deep` for anything that recurses.
///
/// # Errors
/// `E742` when a container in the traversal is already borrowed.
pub fn lock_value(value: &Typval, depth: i32, lock: bool) -> Result<()> {
    fn apply(value: &Typval, depth: i32, lock: bool, seen: &mut HashSet<(usize, u8)>) -> Result<()> {
        if depth == 0 {
            return Ok(());
        }
        let recurse = depth < 0 || depth > 1;
        let state = ox_types::LockState {
            scope: if recurse { ox_types::LockScope::Deep } else { ox_types::LockScope::Shallow },
            locked: lock,
        };
        match value {
            Typval::List(reference) => {
                let key = (Rc::as_ptr(reference) as usize, ox_types::VAR_LIST);
                if !seen.insert(key) { return Ok(()); }
                let items = {
                    let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                    data.lock = state;
                    data.items.clone()
                };
                if recurse { for item in &items { apply(item, depth - 1, lock, seen)?; } }
            }
            Typval::Dict(reference) => {
                let key = (Rc::as_ptr(reference) as usize, ox_types::VAR_DICT);
                if !seen.insert(key) { return Ok(()); }
                let entries = {
                    let mut data = reference.try_borrow_mut().map_err(|_| borrow_error())?;
                    data.lock = state;
                    data.entries.clone()
                };
                if recurse { for (_, item) in &entries { apply(item, depth - 1, lock, seen)?; } }
            }
            _ => {}
        }
        Ok(())
    }
    apply(value, depth, lock, &mut HashSet::new())
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

/// `f_str2float` (`funcs.c:7042-7056`): leading white space is skipped, then
/// an optional sign — after which white space is skipped *again*, so
/// `str2float('- 1.5')` is `-1.5` — and the rest goes to `string2float`. Only
/// a leading `-` sets the sign; a `+` is consumed and ignored.
fn str2float(value: &Typval) -> Result<Typval> {
    let text = string_arg(value)?;
    let mut bytes = skip_white(text.as_bytes());
    let negative = bytes.first() == Some(&b'-');
    if matches!(bytes.first(), Some(b'-' | b'+')) {
        bytes = skip_white(&bytes[1..]);
    }
    let number = string2float(bytes);
    // Upstream multiplies by -1, which flips the sign of a zero and leaves a
    // NaN a NaN: `str2float('-')` is `-0.0` and `str2float('-nan')` is `nan`.
    Ok(Typval::Float(if negative { number * -1.0 } else { number }))
}

/// `skipwhite` (`charset.c`), where `ascii_iswhite` is a space or a tab and
/// nothing else — a newline or a form feed stops it.
fn skip_white(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().position(|byte| !matches!(byte, b' ' | b'\t')).unwrap_or(bytes.len());
    &bytes[end..]
}

/// `string2float` (`eval.c:4611-4630`): the three spellings MS-Windows'
/// `strtod` gets wrong are matched case-insensitively ahead of it, so
/// `str2float('INF')` is infinity and `str2float('infinity')` is too — the
/// check is a three-byte prefix, not a whole word. `-inf` is unreachable from
/// `f_str2float`, which strips the sign first, but `string2float` is also the
/// number literal scanner (`eval.c:3490`) and is kept whole.
fn string2float(bytes: &[u8]) -> f64 {
    if bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"inf") {
        return f64::INFINITY;
    }
    if bytes.len() >= 4 && bytes[..4].eq_ignore_ascii_case(b"-inf") {
        return f64::NEG_INFINITY;
    }
    if bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"nan") {
        return f64::NAN;
    }
    strtod(bytes)
}

/// The `strtod` `string2float` falls back to, under the C locale upstream
/// pins with `setlocale(LC_NUMERIC, "C")`. It takes the longest valid prefix
/// and answers 0.0 when there is none, which is why `str2float('abc')` is
/// 0.0 and `str2float('1.5abc')` is 1.5. A `0x` significand with a binary
/// exponent is part of the grammar, so `str2float('0x10')` is 16.0.
fn strtod(bytes: &[u8]) -> f64 {
    let (negative, rest) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let hexadecimal = rest.len() > 2 && rest[0] == b'0' && matches!(rest[1], b'x' | b'X');
    let magnitude = if hexadecimal { hex_float_prefix(&rest[2..]) } else { None }
        .or_else(|| decimal_float_prefix(rest))
        .unwrap_or(0.0);
    if negative { -magnitude } else { magnitude }
}

/// `[0-9]*(\.[0-9]*)?([eE][+-]?[0-9]+)?` with at least one significand digit,
/// handed to Rust's parser, which accepts the same shapes (`1.`, `.5`, `1e3`)
/// and saturates the exponent the way `strtod` does.
fn decimal_float_prefix(bytes: &[u8]) -> Option<f64> {
    let mut end = 0;
    let mut digits = 0;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) { end += 1; digits += 1; }
    if bytes.get(end) == Some(&b'.') {
        end += 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) { end += 1; digits += 1; }
    }
    if digits == 0 { return None; }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut cursor = end + 1;
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) { cursor += 1; }
        let exponent = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) { cursor += 1; }
        // An `e` with no digits after it is not part of the number.
        if cursor > exponent { end = cursor; }
    }
    std::str::from_utf8(&bytes[..end]).ok()?.parse().ok()
}

/// `0x` already consumed: `[0-9a-f]*(\.[0-9a-f]*)?([pP][+-]?[0-9]+)?` with at
/// least one significand digit. The value is assembled by scaling rather than
/// parsed, since Rust has no hexadecimal float literal.
fn hex_float_prefix(bytes: &[u8]) -> Option<f64> {
    let mut cursor = 0;
    let mut value = 0.0_f64;
    let mut digits = 0;
    while let Some(digit) = bytes.get(cursor).and_then(|byte| (*byte as char).to_digit(16)) {
        value = value * 16.0 + f64::from(digit);
        cursor += 1;
        digits += 1;
    }
    let mut exponent = 0i32;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        while let Some(digit) = bytes.get(cursor).and_then(|byte| (*byte as char).to_digit(16)) {
            value = value * 16.0 + f64::from(digit);
            cursor += 1;
            digits += 1;
            exponent -= 4;
        }
    }
    if digits == 0 { return None; }
    if matches!(bytes.get(cursor), Some(b'p' | b'P')) {
        let mut scan = cursor + 1;
        let negative = bytes.get(scan) == Some(&b'-');
        if matches!(bytes.get(scan), Some(b'+' | b'-')) { scan += 1; }
        let start = scan;
        let mut binary = 0i32;
        while let Some(digit) = bytes.get(scan).and_then(|byte| (*byte as char).to_digit(10)) {
            binary = binary.saturating_mul(10).saturating_add(digit as i32);
            scan += 1;
        }
        if scan > start { exponent += if negative { -binary } else { binary }; }
    }
    Some(value * 2.0_f64.powi(exponent))
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
            Typval::Float(value) => output.extend_from_slice(vim_float_string(*value).as_bytes()),
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
