//! User-defined Vimscript functions and call-frame state.
//!
//! Function bodies remain source text and are executed by [`crate::ExExecutor`]
//! so command/control state has one owner. This module owns names, definition
//! flags, argument binding, local scopes, and `maxfuncdepth` enforcement.

use std::collections::BTreeMap;
use std::fmt;

use ox_eval::scope::ScopeMap;
use ox_eval::Scope;
use ox_types::{OxStr, Typval};

use crate::script::Sid;

/// Upstream's default `'maxfuncdepth'`.
pub const MAX_FUNC_DEPTH: usize = 100;

/// Flags accepted after a `:function` signature.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UserFuncFlags {
    /// Abort the function after an uncaught error.
    pub abort: bool,
    /// The function accepts an Ex line range as one invocation.
    pub range: bool,
    /// The function expects a dictionary receiver (`self`).
    pub dict: bool,
    /// The function may capture its defining local scope.
    pub closure: bool,
}

/// One user-defined function.
#[derive(Clone, Debug)]
pub struct UserFunc {
    /// Canonical function name. Script-local names use `<SNR>{sid}_Name`.
    pub name: String,
    /// Positional parameter names.
    pub args: Vec<String>,
    /// Source text of trailing default expressions, aligned with the last
    /// `default_args.len()` entries of `args` (userfunc.c `uf_def_args`).
    pub default_args: Vec<String>,
    /// Whether `...` was present.
    pub varargs: bool,
    /// Definition flags.
    pub flags: UserFuncFlags,
    /// Logical source lines forming the body.
    pub body: Vec<String>,
    /// SID of the defining script, or zero for command-line definitions.
    pub sid: Sid,
    /// Optional defining local-scope snapshot for `closure` functions.
    pub captured: ScopeMap,
}

/// One active call frame.
#[derive(Clone, Debug)]
pub struct CallFrame {
    /// Canonical function name.
    pub name: String,
    /// SID whose `s:` scope is visible in the function.
    pub sid: Sid,
    /// Caller-local scope restored after the call.
    caller_local: ScopeMap,
    /// Caller argument scope restored after the call.
    caller_argument: ScopeMap,
    /// One-based function-body line currently executing.
    pub current_line: usize,
}

/// User-function definition/call failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserFuncError {
    /// Traditional Vim error code.
    pub code: &'static str,
    /// Human-readable detail.
    pub message: String,
}

impl UserFuncError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for UserFuncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for UserFuncError {}

/// Parsed `:function` signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionSignature {
    /// Name as written, before script-local normalization.
    pub name: String,
    /// Ordered positional parameter names.
    pub args: Vec<String>,
    /// Source text of trailing default expressions (`name = expr`), aligned
    /// with the last `default_args.len()` entries of `args`.
    pub default_args: Vec<String>,
    /// Whether the final parameter is `...`.
    pub varargs: bool,
    /// Definition flags.
    pub flags: UserFuncFlags,
}

/// Executor-owned user-function registry and active call stack.
#[derive(Clone, Debug, Default)]
pub struct UserFunctions {
    functions: BTreeMap<String, UserFunc>,
    call_stack: Vec<CallFrame>,
}

impl UserFunctions {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses `Name(arg, ...) abort range dict closure`.
    pub fn parse_signature(source: &str) -> Result<FunctionSignature, UserFuncError> {
        let source = source.trim();
        let open = source
            .find('(')
            .ok_or_else(|| UserFuncError::new("E124", "Missing '(': function declaration"))?;
        let close = source[open + 1..]
            .find_top_level(b')')
            .map(|offset| open + 1 + offset)
            .ok_or_else(|| UserFuncError::new("E125", "Illegal argument: missing ')'"))?;
        let name = source[..open].trim();
        validate_function_name(name)?;

        let mut args: Vec<String> = Vec::new();
        let mut default_args: Vec<String> = Vec::new();
        let mut varargs = false;
        let arg_text = &source[open + 1..close];
        let pieces = split_argument_ranges(arg_text);
        let last = pieces.len() - 1;
        for (index, (start, end)) in pieces.into_iter().enumerate() {
            let raw = &arg_text[start..end];
            let argument = raw.trim();
            if argument.is_empty() {
                if index == last {
                    // `F(a,)`: upstream tolerates a trailing comma.
                    break;
                }
                if arg_text.trim().is_empty() {
                    break;
                }
                return Err(UserFuncError::new(
                    "E475",
                    format!("Invalid argument: {arg_text}"),
                ));
            }
            if varargs {
                // `...` sets mustend upstream; anything after it is invalid.
                return Err(UserFuncError::new(
                    "E475",
                    format!("Invalid argument: {}", &source[open + 1..]),
                ));
            }
            if argument == "..." {
                varargs = true;
                continue;
            }
            if index != last && raw.trim_end().len() != raw.len() {
                // get_function_args: no white space allowed before the
                // separating comma.
                let rest = &arg_text[start + raw.trim_end().len()..];
                return Err(UserFuncError::new(
                    "E1068",
                    format!("No white space allowed before ',': {rest}"),
                ));
            }
            let (parameter, default) = match argument.split_once('=') {
                Some((parameter, rest)) => (parameter.trim(), Some(rest.trim())),
                None => (argument, None),
            };
            if !is_identifier(parameter) || parameter == "firstline" || parameter == "lastline" {
                return Err(UserFuncError::new(
                    "E125",
                    format!("Illegal argument: {argument}"),
                ));
            }
            if args.iter().any(|existing| existing == parameter) {
                return Err(UserFuncError::new(
                    "E853",
                    format!("Duplicate argument name: {parameter}"),
                ));
            }
            match default {
                Some(expression) => {
                    // get_function_args syntax-checks the default with eval1
                    // (parse without evaluation) at definition time.
                    if ox_eval::Parser::new(expression.as_bytes()).parse().is_err() {
                        return Err(UserFuncError::new(
                            "E475",
                            format!("Invalid argument: {}", &source[open + 1..]),
                        ));
                    }
                    default_args.push(expression.to_owned());
                }
                None => {
                    if !default_args.is_empty() {
                        return Err(UserFuncError::new(
                            "E989",
                            "Non-default argument follows default argument",
                        ));
                    }
                }
            }
            args.push(parameter.to_owned());
        }

        let mut flags = UserFuncFlags::default();
        for flag in source[close + 1..].split_ascii_whitespace() {
            match flag {
                "abort" => flags.abort = true,
                "range" => flags.range = true,
                "dict" => flags.dict = true,
                "closure" => flags.closure = true,
                _ => {
                    return Err(UserFuncError::new(
                        "E488",
                        format!("Trailing characters: {flag}"),
                    ));
                }
            }
        }
        Ok(FunctionSignature {
            name: name.to_owned(),
            args,
            default_args,
            varargs,
            flags,
        })
    }

    /// Canonicalizes `s:Name`/`<SID>Name` against the defining SID.
    #[must_use]
    pub fn canonical_name(name: &str, sid: Sid) -> String {
        if let Some(local) = name.strip_prefix("s:") {
            return format!("<SNR>{sid}_{local}");
        }
        if let Some(local) = name.strip_prefix("<SID>") {
            return format!("<SNR>{sid}_{local}");
        }
        name.to_owned()
    }

    /// Defines a function. `replace` implements `:function!`.
    pub fn define(
        &mut self,
        signature: FunctionSignature,
        body: Vec<String>,
        sid: Sid,
        replace: bool,
        scope: &Scope,
    ) -> Result<String, UserFuncError> {
        let name = Self::canonical_name(&signature.name, sid);
        if self.functions.contains_key(&name) && !replace {
            return Err(UserFuncError::new(
                "E122",
                format!("Function {name} already exists, add ! to replace it"),
            ));
        }
        let captured = if signature.flags.closure {
            scope.local.clone()
        } else {
            ScopeMap::new()
        };
        self.functions.insert(
            name.clone(),
            UserFunc {
                name: name.clone(),
                args: signature.args,
                default_args: signature.default_args,
                varargs: signature.varargs,
                flags: signature.flags,
                body,
                sid,
                captured,
            },
        );
        Ok(name)
    }

    /// Removes a function by canonical name.
    pub fn remove(&mut self, name: &str, sid: Sid) -> bool {
        self.functions.remove(&Self::canonical_name(name, sid)).is_some()
    }

    /// Looks up a function, resolving script-local spelling in `sid`.
    #[must_use]
    pub fn get(&self, name: &str, sid: Sid) -> Option<&UserFunc> {
        self.functions.get(&Self::canonical_name(name, sid))
    }

    /// Clones a function for execution without holding a registry borrow.
    #[must_use]
    pub fn resolve(&self, name: &str, sid: Sid) -> Option<UserFunc> {
        self.get(name, sid).cloned()
    }

    /// Whether a canonical or script-local function exists.
    #[must_use]
    pub fn contains(&self, name: &str, sid: Sid) -> bool {
        self.get(name, sid).is_some()
    }

    /// All functions in canonical name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &UserFunc)> {
        self.functions
            .iter()
            .map(|(name, function)| (name.as_str(), function))
    }

    /// Begins one call, replacing `l:` and `a:` with frame-local maps.
    /// Returns an owned function descriptor for the executor to run.
    pub fn begin_call(
        &mut self,
        name: &str,
        sid: Sid,
        values: Vec<Typval>,
        first_line: usize,
        last_line: usize,
        scope: &mut Scope,
    ) -> Result<UserFunc, UserFuncError> {
        if self.call_stack.len() >= MAX_FUNC_DEPTH {
            return Err(UserFuncError::new(
                "E132",
                "Function call depth is higher than 'maxfuncdepth'",
            ));
        }
        let function = self.resolve(name, sid).ok_or_else(|| {
            UserFuncError::new("E117", format!("Unknown function: {name}"))
        })?;
        // check_user_func_argcount: required = uf_args - uf_def_args;
        // calls may omit any trailing defaulted parameters.
        let required = function.args.len() - function.default_args.len();
        if values.len() < required || (!function.varargs && values.len() > function.args.len())
        {
            let relation = if values.len() < required {
                "Not enough"
            } else {
                "Too many"
            };
            return Err(UserFuncError::new(
                if values.len() < required { "E119" } else { "E118" },
                format!("{relation} arguments for function: {}", function.name),
            ));
        }

        let caller_local = std::mem::take(&mut scope.local);
        let caller_argument = std::mem::take(&mut scope.argument);
        scope.local = function.captured.clone();
        scope.argument = ScopeMap::new();
        for (parameter, value) in function.args.iter().zip(values.iter()) {
            scope.argument.push((OxStr::from(parameter.as_str()), value.clone()));
        }
        let extras = values.into_iter().skip(function.args.len()).collect::<Vec<_>>();
        scope
            .argument
            .push((OxStr::from("0"), Typval::Number(extras.len() as i64)));
        scope
            .argument
            .push((OxStr::from("000"), Typval::list(extras.clone())));
        for (index, value) in extras.into_iter().enumerate() {
            scope.argument.push((
                OxStr((index + 1).to_string().into_bytes()),
                value,
            ));
        }
        scope.argument.push((
            OxStr::from("firstline"),
            Typval::Number(first_line as i64),
        ));
        scope.argument.push((
            OxStr::from("lastline"),
            Typval::Number(last_line as i64),
        ));

        self.call_stack.push(CallFrame {
            name: function.name.clone(),
            sid: function.sid,
            caller_local,
            caller_argument,
            current_line: 0,
        });
        Ok(function)
    }

    /// Ends one call and restores the caller's `l:`/`a:` scopes.
    pub fn end_call(&mut self, scope: &mut Scope) -> Option<CallFrame> {
        let frame = self.call_stack.pop()?;
        scope.local = frame.caller_local.clone();
        scope.argument = frame.caller_argument.clone();
        Some(frame)
    }

    /// Sets the current function-body line for throwpoint rendering.
    pub fn set_current_line(&mut self, line: usize) {
        if let Some(frame) = self.call_stack.last_mut() {
            frame.current_line = line;
        }
    }

    /// Active call frames, outermost first.
    #[must_use]
    pub fn call_stack(&self) -> &[CallFrame] {
        &self.call_stack
    }

    /// SID visible to `s:` names in the active function body.
    #[must_use]
    pub fn active_sid(&self) -> Option<Sid> {
        self.call_stack.last().map(|frame| frame.sid).filter(|sid| *sid != 0)
    }

    /// Upstream-style call-stack throwpoint prefix.
    #[must_use]
    pub fn throwpoint_prefix(&self) -> String {
        self.call_stack
            .iter()
            .map(|frame| format!("function {}[{}]", frame.name, frame.current_line))
            .collect::<Vec<_>>()
            .join("..")
    }
}

fn validate_function_name(name: &str) -> Result<(), UserFuncError> {
    if name.is_empty() {
        return Err(UserFuncError::new("E129", "Function name required"));
    }
    if let Some((dictionary, member)) = name.rsplit_once('.') {
        let dictionary = dictionary
            .get(2..)
            .filter(|_| dictionary.as_bytes().get(1) == Some(&b':'))
            .unwrap_or(dictionary);
        if is_identifier(member)
            && !dictionary.is_empty()
            && dictionary.split('.').all(is_identifier)
        {
            return Ok(());
        }
        return Err(UserFuncError::new(
            "E128",
            format!("Invalid function name: {name}"),
        ));
    }
    let (unqualified, script_local) = if let Some(local) = name.strip_prefix("s:") {
        (local, true)
    } else if let Some(local) = name.strip_prefix("<SID>") {
        (local, true)
    } else if let Some(local) = name
        .strip_prefix("<SNR>")
        .and_then(|local| local.split_once('_').map(|(_, name)| name))
    {
        (local, true)
    } else {
        (name, false)
    };
    if !script_local
        && !unqualified.contains('#')
        && !unqualified
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_uppercase)
    {
        return Err(UserFuncError::new(
            "E128",
            format!("Function name must start with a capital or contain '#': {name}"),
        ));
    }
    if unqualified.split('#').any(|part| !is_identifier(part)) {
        return Err(UserFuncError::new(
            "E128",
            format!("Invalid function name: {name}"),
        ));
    }
    Ok(())
}

fn is_identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Byte-scanner used by [`UserFunctions::parse_signature`].
///
/// `get_function_args` walks the argument list with `eval1`, which steps over
/// nested `()`/`[]`/`{}` and quoted strings; comma and closing-paren
/// detection must do the same so default expressions may contain commas,
/// strings, and calls.
trait FindTopLevel {
    fn find_top_level(&self, needle: u8) -> Option<usize>;
}

impl FindTopLevel for str {
    fn find_top_level(&self, needle: u8) -> Option<usize> {
        let bytes = self.as_bytes();
        let mut depth = 0usize;
        let mut index = 0usize;
        while index < bytes.len() {
            match bytes[index] {
                b'\'' | b'"' => skip_quoted(bytes, &mut index),
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    index += 1;
                }
                close @ (b')' | b']' | b'}') => {
                    if close == needle && depth == 0 {
                        return Some(index);
                    }
                    depth = depth.saturating_sub(1);
                    index += 1;
                }
                b',' if needle == b',' && depth == 0 => return Some(index),
                _ => index += 1,
            }
        }
        None
    }
}

/// Advances `index` past one Vim single- or double-quoted string.
fn skip_quoted(bytes: &[u8], index: &mut usize) {
    let quote = bytes[*index];
    *index += 1;
    while *index < bytes.len() {
        if quote == b'"' && bytes[*index] == b'\\' && *index + 1 < bytes.len() {
            *index += 2;
            continue;
        }
        if bytes[*index] == quote {
            if quote == b'\'' && bytes.get(*index + 1) == Some(&b'\'') {
                *index += 2;
                continue;
            }
            *index += 1;
            return;
        }
        *index += 1;
    }
}

/// Splits `text` into `(start, end)` byte ranges at top-level commas.
fn split_argument_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    while let Some(position) = text[offset..].find_top_level(b',') {
        ranges.push((offset, offset + position));
        offset += position + 1;
    }
    ranges.push((offset, text.len()));
    ranges
}
