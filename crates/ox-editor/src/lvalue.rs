//! Parser-backed lvalue resolution for `:let` and `:unlet`.

use std::cell::RefCell;
use std::rc::Rc;

use ox_eval::lexer::TokenKind;
use ox_eval::parser::{Expr, ExprKind};
use ox_eval::{
    Builtins, EvalError, Evaluator, Scope, ScopeKind, list_slice_bounds, normalize_list_index,
};
use ox_types::{DictEntry, DictEntryFlags, DictRef, ListRef, OxStr, Typval};

use crate::editor::Editor;
use crate::excmd_exec::{
    EvalHost, ExEditorAccess, ExRuntime, Flow, LuaExec, VimRegex, assign_option, eval_text,
    read_option, typval_to_text,
};
use crate::register::RegisterContent;
use crate::script::FileIO;

/// `parse_expression_prefix`: parse one leading expression and report how
/// many bytes it consumed.
fn parse_expression_prefix(bytes: &[u8]) -> Result<(Expr, usize), EvalError> {
    let expression = ox_eval::Parser::new(bytes).parse()?;
    let consumed = expression.span.end;
    Ok((expression, consumed))
}

/// Strict number coercion for lvalue bounds and indices.
#[expect(
    clippy::cast_possible_truncation,
    reason = "Vim float-to-Number coercion truncates toward zero"
)]
fn typval_to_number_strict(value: &Typval) -> Result<i64, EvalError> {
    match value {
        Typval::Number(number) => Ok(*number),
        Typval::Bool(flag) => Ok(i64::from(*flag)),
        Typval::Float(float) => Ok(*float as i64),
        Typval::String(text) => String::from_utf8_lossy(text.as_bytes())
            .trim()
            .parse::<i64>()
            .map_err(|_| EvalError::new("E745", 0, "Using a String as a Number")),
        _ => Err(EvalError::new("E745", 0, "Using a String as a Number")),
    }
}

/// Strict string coercion for index keys and `v:` variable writes.
fn typval_to_string_strict(value: &Typval) -> Result<OxStr, EvalError> {
    match value {
        Typval::String(text) => Ok(text.clone()),
        Typval::Number(number) => Ok(OxStr::from(number.to_string().as_bytes())),
        Typval::Bool(flag) => Ok(OxStr::from(if *flag {
            b"v:true" as &[u8]
        } else {
            b"v:false"
        })),
        _ => Err(EvalError::new("E730", 0, "Using a String as a Number")),
    }
}

/// The upstream `vimvars` type table, narrowed to what `assign_vim_variable`
/// coerces toward.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimVariableType {
    String,
    Number,
    List,
    Dict,
}

pub(crate) fn vim_variable_type(name: &[u8]) -> Option<VimVariableType> {
    match name {
        b"errmsg" | b"warningmsg" | b"statusmsg" | b"this_session" | b"fcs_choice"
        | b"scrollstart" | b"swapchoice" | b"char" | b"progpath" | b"servername" => {
            Some(VimVariableType::String)
        }
        b"shell_error" | b"shell_exitcode" | b"searchforward" | b"hlsearch" | b"mouse_win"
        | b"mouse_winid" | b"mouse_lnum" | b"mouse_col" => Some(VimVariableType::Number),
        b"errors" | b"oldfiles" => Some(VimVariableType::List),
        b"completed_item" => Some(VimVariableType::Dict),
        _ => None,
    }
}

/// One parsed assignment target.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Lvalue {
    Variable {
        name: OxStr,
        scope_kind: Option<ScopeKind>,
        subs: Vec<BoundSub>,
    },
    Destructure {
        targets: Vec<Lvalue>,
        rest: bool,
    },
    Register(u8),
    Env(String),
    Option(String),
}

/// One bound subscript on a variable lvalue.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BoundSub {
    Key(OxStr),
    Index(Typval),
    Slice(Option<Typval>, Option<Typval>),
}

pub(crate) fn parse_and_bind_lvalue<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    target: &str,
) -> Result<Lvalue, Flow> {
    let target = expand_curly_target(runtime, access, scope, lua, target.trim())?;
    if let Some(inner) = balanced_brackets(&target) {
        return parse_destructure(runtime, access, scope, lua, inner);
    }
    if let Some(rest) = target.strip_prefix('@') {
        let mut chars = rest.chars();
        let Some(register) = chars.next() else {
            return Err(lvalue_error(
                runtime,
                "E488",
                format!("Trailing characters: {target}"),
            ));
        };
        if chars.next().is_some() {
            return Err(lvalue_error(
                runtime,
                "E488",
                format!("Trailing characters: {target}"),
            ));
        }
        return Ok(Lvalue::Register(register as u8));
    }
    if let Some(rest) = target.strip_prefix('$') {
        let name_len = env_name_len(rest);
        if name_len == 0 || name_len != rest.len() {
            return Err(lvalue_error(
                runtime,
                "E488",
                format!("Trailing characters: {target}"),
            ));
        }
        return Ok(Lvalue::Env(rest[..name_len].to_owned()));
    }
    if let Some(rest) = target.strip_prefix('&') {
        if rest.is_empty() || trailing_special_garbage(rest) {
            return Err(lvalue_error(
                runtime,
                "E488",
                format!("Trailing characters: {target}"),
            ));
        }
        return Ok(Lvalue::Option(rest.to_owned()));
    }
    let (expression, consumed) =
        parse_expression_prefix(target.as_bytes()).map_err(|error| eval_error(runtime, error))?;
    if expression.span.start != 0 || consumed != target.len() {
        let rest = &target[expression.span.end.min(target.len())..];
        return Err(lvalue_error(
            runtime,
            "E488",
            format!("Trailing characters: {rest}"),
        ));
    }
    let (name, scope_kind, mut subs) =
        bind_expression_chain(runtime, access, scope, lua, &expression)?;
    subs.reverse();
    Ok(Lvalue::Variable {
        name,
        scope_kind,
        subs,
    })
}

pub(crate) fn read_lvalue<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &Scope,
    lvalue: &Lvalue,
) -> Result<Typval, Flow> {
    match lvalue {
        Lvalue::Variable {
            name,
            scope_kind,
            subs,
        } if subs.is_empty() => read_root_variable(runtime, scope, name.as_bytes(), *scope_kind),
        Lvalue::Variable {
            name,
            scope_kind,
            subs,
        } => {
            let root = read_root_variable(runtime, scope, name.as_bytes(), *scope_kind)?;
            read_subscript_chain(runtime, root, subs, 0)
        }
        Lvalue::Destructure { .. } => Err(lvalue_error(
            runtime,
            "E15",
            "Invalid expression: destructure target",
        )),
        Lvalue::Register(register) => Ok(scope.get_register(&[*register])),
        Lvalue::Env(name) => Ok(read_environment(name)),
        Lvalue::Option(name) => Ok(access.with_ex_editor(|editor| read_option(editor, name))),
    }
}

pub(crate) fn assign_lvalue<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lvalue: &Lvalue,
    value: Typval,
    constant: bool,
) -> Result<(), Flow> {
    let _ = constant;
    match lvalue {
        Lvalue::Variable {
            name,
            scope_kind,
            subs,
        } if subs.is_empty() => {
            assign_root_variable(runtime, scope, name.as_bytes(), *scope_kind, value)
        }
        Lvalue::Variable {
            name,
            scope_kind,
            subs,
        } => {
            let lock_name = scoped_lock_name(*scope_kind, name.as_bytes());
            scope
                .check_value_lock(&lock_name, 0)
                .map_err(|error| eval_error(runtime, error))?;
            let root = read_root_variable(runtime, scope, name.as_bytes(), *scope_kind)?;
            let target = rendered_target(*scope_kind, name.as_bytes(), subs);
            assign_subscript_chain(runtime, root, subs, value, 0, &target)
        }
        Lvalue::Destructure { targets, rest } => {
            assign_destructure(runtime, access, scope, targets, *rest, value)
        }
        Lvalue::Register(register) => assign_register(runtime, access, scope, *register, value),
        Lvalue::Env(name) => {
            assign_environment(name, &value);
            Ok(())
        }
        Lvalue::Option(name) => assign_option(runtime, access, scope, name, &value),
    }
}

pub(crate) fn remove_lvalue<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lvalue: &Lvalue,
    bang: bool,
) -> Result<bool, Flow> {
    match lvalue {
        Lvalue::Variable {
            name,
            scope_kind,
            subs,
        } if subs.is_empty() => Ok(remove_root_variable(scope, name.as_bytes(), *scope_kind)),
        Lvalue::Variable {
            name,
            scope_kind,
            subs,
        } => {
            let root = read_root_variable(runtime, scope, name.as_bytes(), *scope_kind)?;
            let target = rendered_target(*scope_kind, name.as_bytes(), subs);
            remove_subscript_chain(runtime, root, subs, 0, bang, &target)
        }
        Lvalue::Destructure { targets, .. } => {
            let mut removed = false;
            for target in targets {
                removed |= remove_lvalue(runtime, access, scope, target, bang)?;
            }
            Ok(removed)
        }
        Lvalue::Register(register) => {
            Ok(access.with_ex_editor(|editor| clear_register(editor, scope, *register)))
        }
        Lvalue::Env(name) => {
            let was_set = std::env::var_os(name).is_some();
            ox_sys::unset_env(name);
            Ok(was_set)
        }
        Lvalue::Option(name) => Err(lvalue_error(
            runtime,
            "E518",
            format!("Can't unlet option {name}"),
        )),
    }
}

fn assign_destructure<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    targets: &[Lvalue],
    rest: bool,
    value: Typval,
) -> Result<(), Flow> {
    let Typval::List(values) = value else {
        return Err(lvalue_error(runtime, "E714", "List required"));
    };
    let values = values.borrow().items.clone();
    if rest {
        if targets.is_empty() {
            return Err(lvalue_error(
                runtime,
                "E687",
                "Less targets than List items",
            ));
        }
        let fixed = targets.len() - 1;
        if values.len() < fixed {
            return Err(lvalue_error(
                runtime,
                "E687",
                "Less targets than List items",
            ));
        }
        for (target, item) in targets.iter().take(fixed).zip(values.iter().take(fixed)) {
            assign_lvalue(runtime, access, scope, target, item.clone(), false)?;
        }
        if let Some(last) = targets.last() {
            assign_lvalue(
                runtime,
                access,
                scope,
                last,
                Typval::list(values[fixed..].to_vec()),
                false,
            )?;
        }
        return Ok(());
    }
    if targets.len() < values.len() {
        return Err(lvalue_error(
            runtime,
            "E687",
            "Less targets than List items",
        ));
    }
    if targets.len() > values.len() {
        return Err(lvalue_error(
            runtime,
            "E688",
            "More targets than List items",
        ));
    }
    for (target, item) in targets.iter().zip(values) {
        assign_lvalue(runtime, access, scope, target, item, false)?;
    }
    Ok(())
}

pub(crate) fn expand_curly_target<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    target: &str,
) -> Result<String, Flow> {
    if !target.contains('{') {
        return Ok(target.to_owned());
    }
    // `make_expanded_name` (eval.c:5769) evaluates every `{expr}` group and
    // splices the value into the literal name. Group boundaries come from
    // the target's own lexer tokens, so quoted `}`s (`g:{'a}b'}`), escapes,
    // and nested `{}`/`#{}` dict braces are all invisible to the scan
    // instead of a second hand-rolled quote grammar.
    let (tokens, _) = ox_eval::lexer::Lexer::new(target.as_bytes()).tokenize_tolerant();
    let mut expanded = String::with_capacity(target.len());
    let mut cursor = 0_usize;
    let mut group_start = 0_usize;
    let mut group_body_start = 0_usize;
    let mut depth = 0_usize;
    for token in &tokens {
        if matches!(token.kind, TokenKind::Eof) {
            break;
        }
        match token.kind {
            TokenKind::LBrace | TokenKind::HashLBrace if depth == 0 => {
                // `#{` opens one byte in so the `#` stays part of the name,
                // matching find_name_end's brace scan.
                group_start =
                    token.span.start + usize::from(matches!(token.kind, TokenKind::HashLBrace));
                group_body_start = token.span.end;
                depth = 1;
            }
            TokenKind::LBrace | TokenKind::HashLBrace => depth += 1,
            TokenKind::RBrace if depth > 1 => depth -= 1,
            TokenKind::RBrace if depth == 1 => {
                expanded.push_str(&target[cursor..group_start]);
                let value = eval_text(
                    runtime,
                    access,
                    scope,
                    lua,
                    &target[group_body_start..token.span.start],
                )?;
                expanded.push_str(&typval_to_text(&value));
                cursor = token.span.end;
                depth = 0;
            }
            _ => {}
        }
    }
    if depth > 0 {
        return Err(lvalue_error(runtime, "E15", "Invalid expression"));
    }
    if cursor == 0 {
        return Ok(target.to_owned());
    }
    expanded.push_str(&target[cursor..]);
    Ok(expanded)
}

fn balanced_brackets(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let mut depth = 0usize;
    let mut quote = None;
    for byte in trimmed.as_bytes() {
        if let Some(active) = quote {
            if *byte == active {
                quote = None;
            }
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = Some(*byte);
            continue;
        }
        match byte {
            b'[' => depth += 1,
            b']' => depth -= 1,
            _ => {}
        }
    }
    (depth == 0 && trimmed.starts_with('[') && trimmed.ends_with(']')).then_some(inner)
}

fn parse_destructure<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    inner: &str,
) -> Result<Lvalue, Flow> {
    let (parts, rest_target) = split_destructure_parts(runtime, inner)?;
    let mut targets = Vec::with_capacity(parts.len() + usize::from(rest_target.is_some()));
    for part in parts {
        targets.push(parse_and_bind_lvalue(runtime, access, scope, lua, part)?);
    }
    if let Some(rest_name) = rest_target {
        targets.push(parse_and_bind_lvalue(
            runtime, access, scope, lua, rest_name,
        )?);
        return Ok(Lvalue::Destructure {
            targets,
            rest: true,
        });
    }
    Ok(Lvalue::Destructure {
        targets,
        rest: false,
    })
}

fn split_destructure_parts<'a, F: FileIO>(
    runtime: &ExRuntime<F>,
    source: &'a str,
) -> Result<(Vec<&'a str>, Option<&'a str>), Flow> {
    let bytes = source.as_bytes();
    let mut quote = None;
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if let Some(active) = quote {
            if byte == active {
                quote = None;
            }
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            continue;
        }
        if matches!(byte, b'(' | b'[' | b'{') {
            depth += 1;
            continue;
        }
        if matches!(byte, b')' | b']' | b'}') {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth == 0 && byte == b';' {
            let left = source[..index].trim();
            let right = source[index + 1..].trim();
            if right.is_empty() {
                return Err(lvalue_error(
                    runtime,
                    "E688",
                    "More targets than List items",
                ));
            }
            if left.contains(';') {
                return Err(lvalue_error(runtime, "E15", "Invalid destructure target"));
            }
            return Ok((split_comma_args(left), Some(right)));
        }
    }
    Ok((split_comma_args(source), None))
}

fn bind_expression_chain<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    expression: &Expr,
) -> Result<(OxStr, Option<ScopeKind>, Vec<BoundSub>), Flow> {
    let mut subs = Vec::new();
    let mut current = expression;
    loop {
        match &current.kind {
            ExprKind::Variable(name) => {
                let (mut scope_kind, bare) = split_scoped_name(name.as_bytes());
                if bare.contains(&b'#') {
                    scope_kind = Some(ScopeKind::Global);
                }
                return Ok((OxStr::from(bare), scope_kind, subs));
            }
            ExprKind::Member { target, name } => {
                subs.push(BoundSub::Key(name.clone()));
                current = target;
            }
            ExprKind::Index { target, index } => {
                if let ExprKind::Literal(Typval::String(key)) = &index.kind {
                    subs.push(BoundSub::Key(key.clone()));
                } else {
                    let value = eval_expression(runtime, access, scope, lua, index)?;
                    subs.push(BoundSub::Index(value));
                }
                current = target;
            }
            ExprKind::Slice { target, start, end } => {
                let start = bind_optional_bound(runtime, access, scope, lua, start.as_deref())?;
                let end = bind_optional_bound(runtime, access, scope, lua, end.as_deref())?;
                subs.push(BoundSub::Slice(start, end));
                current = target;
            }
            _ => return Err(lvalue_error(runtime, "E461", "Illegal variable name")),
        }
    }
}

fn bind_optional_bound<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    expression: Option<&Expr>,
) -> Result<Option<Typval>, Flow> {
    match expression {
        Some(expr) => Ok(Some(eval_expression(runtime, access, scope, lua, expr)?)),
        None => Ok(None),
    }
}

fn eval_expression<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    expression: &Expr,
) -> Result<Typval, Flow> {
    let regex = VimRegex;
    let ambiguous_wide = access.with_ex_editor(|editor| {
        matches!(
            editor.options().get_global("ambiwidth"),
            Ok(crate::OptionValue::String(value)) if value == "double"
        )
    });
    let mut host = EvalHost {
        runtime,
        access,
        lua,
        builtins: Builtins::new(&regex).with_ambiguous_width(ambiguous_wide),
        submatches: None,
        escaped_exception: None,
    };
    Evaluator::new(&mut host, &regex)
        .eval(expression, scope)
        .map_err(|error| eval_error(host.runtime, error))
}

/// The verbatim spelling of a variable lvalue — `d.changedtick`,
/// `b:["changedtick"]` — which entry read-only errors name, exactly as
/// upstream's `get_lval` carries the written form through resolution.
fn rendered_target(scope_kind: Option<ScopeKind>, name: &[u8], subs: &[BoundSub]) -> String {
    let mut target = String::new();
    if let Some(kind) = scope_kind {
        target.push_str(kind.as_str());
    }
    target.push_str(&String::from_utf8_lossy(name));
    for sub in subs {
        match sub {
            BoundSub::Key(key)
                if key
                    .as_bytes()
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'#')) =>
            {
                target.push('.');
                target.push_str(&key.to_string_lossy());
            }
            BoundSub::Key(key) => {
                target.push_str("[\"");
                target.push_str(&key.to_string_lossy());
                target.push_str("\"]");
            }
            BoundSub::Index(index) => {
                target.push('[');
                target.push_str(&typval_to_text(index));
                target.push(']');
            }
            BoundSub::Slice(start, end) => {
                target.push('[');
                if let Some(start) = start {
                    target.push_str(&typval_to_text(start));
                }
                target.push(':');
                if let Some(end) = end {
                    target.push_str(&typval_to_text(end));
                }
                target.push(']');
            }
        }
    }
    target
}
fn read_root_variable<F: FileIO>(
    runtime: &ExRuntime<F>,
    scope: &Scope,
    name: &[u8],
    scope_kind: Option<ScopeKind>,
) -> Result<Typval, Flow> {
    if let Some(kind) = scope_kind {
        if name.is_empty() {
            // `b:` with subscripts names the scope dictionary itself;
            // materialize the same flagged snapshot a bare `b:` read gets.
            return Ok(scope.scope_dict(kind));
        }
        scope
            .get_scoped(kind, name, 0)
            .cloned()
            .map_err(|error| eval_error(runtime, error))
    } else {
        scope
            .get(name, 0)
            .cloned()
            .map_err(|error| eval_error(runtime, error))
    }
}

/// Whether an lvalue's final dict entry carries the read-only flag — the
/// `get_lval_dict_item` (eval.c:949) resolution check that refuses
/// `:lockvar`/`:unlockvar` on the item with E46 before any locking.
pub(crate) fn names_read_only_entry<F: FileIO>(
    runtime: &ExRuntime<F>,
    scope: &Scope,
    lvalue: &Lvalue,
) -> Result<bool, Flow> {
    let Lvalue::Variable {
        name,
        scope_kind,
        subs,
    } = lvalue
    else {
        return Ok(false);
    };
    let Some((last, parents)) = subs.split_last() else {
        return Ok(false);
    };
    let mut current = read_root_variable(runtime, scope, name.as_bytes(), *scope_kind)?;
    for sub in parents {
        current = read_one_subscript(runtime, current, sub)?;
    }
    let Typval::Dict(dict) = current else {
        return Ok(false);
    };
    let key = match last {
        BoundSub::Key(key) | BoundSub::Index(Typval::String(key)) => key.clone(),
        _ => return Ok(false),
    };
    Ok(dict
        .try_borrow()
        .map_err(|_| borrow_error(runtime))?
        .get_entry(key.as_bytes())
        .is_some_and(|entry| entry.flags.intersects(DictEntryFlags::READ_ONLY)))
}

/// `$VAR` reads the live process environment (`vim_getenv`), so values set
/// this session are visible and an unset name is the empty string.
fn read_environment(name: &str) -> Typval {
    Typval::String(std::env::var_os(name).map_or_else(
        || OxStr::from(""),
        |value| OxStr::from(value.to_string_lossy().as_ref()),
    ))
}

fn read_subscript_chain<F: FileIO>(
    runtime: &ExRuntime<F>,
    mut current: Typval,
    subs: &[BoundSub],
    index: usize,
) -> Result<Typval, Flow> {
    if index >= subs.len() {
        return Ok(current);
    }
    if matches!(subs[index], BoundSub::Slice(_, _)) && index + 1 != subs.len() {
        return Err(lvalue_error(runtime, "E708", "[: ] must come last"));
    }
    current = read_one_subscript(runtime, current, &subs[index])?;
    read_subscript_chain(runtime, current, subs, index + 1)
}

fn read_one_subscript<F: FileIO>(
    runtime: &ExRuntime<F>,
    container: Typval,
    sub: &BoundSub,
) -> Result<Typval, Flow> {
    match (container, sub) {
        (Typval::Dict(dict), BoundSub::Key(key) | BoundSub::Index(Typval::String(key))) => {
            dict_read_key(runtime, &dict, key.as_bytes())
        }
        (Typval::Dict(dict), BoundSub::Index(index)) => {
            let key = typval_to_string_strict(index).map_err(|error| eval_error(runtime, error))?;
            dict_read_key(runtime, &dict, key.as_bytes())
        }
        (Typval::List(list), BoundSub::Index(index)) => list_read_index(runtime, &list, index),
        (Typval::List(list), BoundSub::Slice(start, end)) => {
            list_read_slice(runtime, &list, start.as_ref(), end.as_ref())
        }
        (Typval::Dict(_), BoundSub::Slice(_, _)) => {
            Err(lvalue_error(runtime, "E719", "Cannot slice a Dictionary"))
        }
        (Typval::List(_), BoundSub::Key(_)) => {
            Err(lvalue_error(runtime, "E909", "invalid value for subscript"))
        }
        _ => Err(lvalue_error(runtime, "E715", "Dictionary required")),
    }
}

fn assign_subscript_chain<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    container: Typval,
    subs: &[BoundSub],
    value: Typval,
    index: usize,
    target: &str,
) -> Result<(), Flow> {
    if index >= subs.len() {
        return Ok(());
    }
    if matches!(subs[index], BoundSub::Slice(_, _)) && index + 1 != subs.len() {
        return Err(lvalue_error(runtime, "E708", "[: ] must come last"));
    }
    if index + 1 == subs.len() {
        return assign_leaf_subscript(runtime, container, &subs[index], value, target);
    }
    let next = read_one_subscript(runtime, container.clone(), &subs[index])?;
    assign_subscript_chain(runtime, next, subs, value, index + 1, target)
}

fn assign_leaf_subscript<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    container: Typval,
    sub: &BoundSub,
    value: Typval,
    target: &str,
) -> Result<(), Flow> {
    match (container, sub) {
        (Typval::Dict(dict), BoundSub::Key(key) | BoundSub::Index(Typval::String(key))) => {
            dict_write_key(runtime, &dict, key, value, target)
        }
        (Typval::Dict(dict), BoundSub::Index(index)) => {
            let key = OxStr::from(
                typval_to_string_strict(index)
                    .map_err(|error| eval_error(runtime, error))?
                    .as_bytes(),
            );
            dict_write_key(runtime, &dict, &key, value, target)
        }
        (Typval::List(list), BoundSub::Index(index)) => {
            list_write_index(runtime, &list, index, value)
        }
        (Typval::List(list), BoundSub::Slice(start, end)) => {
            list_write_slice(runtime, &list, start.as_ref(), end.as_ref(), value)
        }
        (Typval::Dict(_), BoundSub::Slice(_, _)) => {
            Err(lvalue_error(runtime, "E719", "Cannot slice a Dictionary"))
        }
        _ => Err(lvalue_error(runtime, "E715", "Dictionary required")),
    }
}

fn remove_subscript_chain<F: FileIO>(
    runtime: &ExRuntime<F>,
    container: Typval,
    subs: &[BoundSub],
    index: usize,
    bang: bool,
    target: &str,
) -> Result<bool, Flow> {
    if index >= subs.len() {
        return Ok(false);
    }
    if index + 1 == subs.len() {
        return remove_leaf_subscript(runtime, container, &subs[index], bang, target);
    }
    let next = read_one_subscript(runtime, container, &subs[index])?;
    remove_subscript_chain(runtime, next, subs, index + 1, bang, target)
}

fn remove_leaf_subscript<F: FileIO>(
    runtime: &ExRuntime<F>,
    container: Typval,
    sub: &BoundSub,
    bang: bool,
    target: &str,
) -> Result<bool, Flow> {
    match (container, sub) {
        (Typval::Dict(dict), BoundSub::Key(key) | BoundSub::Index(Typval::String(key))) => {
            dict_remove_key(runtime, &dict, key, bang, target)
        }
        (Typval::Dict(dict), BoundSub::Index(index)) => {
            let key = OxStr::from(
                typval_to_string_strict(index)
                    .map_err(|error| eval_error(runtime, error))?
                    .as_bytes(),
            );
            dict_remove_key(runtime, &dict, &key, bang, target)
        }
        (Typval::List(list), BoundSub::Index(index)) => list_remove_index(runtime, &list, index),
        (Typval::List(list), BoundSub::Slice(start, end)) => {
            list_remove_slice(runtime, &list, start.as_ref(), end.as_ref())
        }
        _ => Ok(false),
    }
}

fn dict_read_key<F: FileIO>(
    runtime: &ExRuntime<F>,
    dict: &DictRef,
    key: &[u8],
) -> Result<Typval, Flow> {
    dict.try_borrow()
        .map_err(|_| borrow_error(runtime))?
        .get(key)
        .cloned()
        .ok_or_else(|| {
            lvalue_error(
                runtime,
                "E716",
                format!(
                    "Key not present in Dictionary: {}",
                    String::from_utf8_lossy(key)
                ),
            )
        })
}

fn dict_write_key<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    dict: &DictRef,
    key: &OxStr,
    value: Typval,
    target: &str,
) -> Result<(), Flow> {
    let mut data = dict.try_borrow_mut().map_err(|_| borrow_error(runtime))?;
    if data.lock.locked {
        return Err(lvalue_error(runtime, "E741", "Value is locked"));
    }
    if let Some(entry) = data
        .entries
        .iter_mut()
        .find(|entry| entry.key.as_bytes() == key.as_bytes())
    {
        // `get_lval_dict_item` (eval.c:949): the reached item's read-only
        // flag refuses the write with E46, naming the target verbatim.
        if entry.flags.intersects(DictEntryFlags::READ_ONLY) {
            return Err(lvalue_error(
                runtime,
                "E46",
                format!("Cannot change read-only variable \"{target}\""),
            ));
        }
        entry.value = value;
    } else {
        data.entries.push(DictEntry::new(key.clone(), value));
    }
    Ok(())
}

fn dict_remove_key<F: FileIO>(
    runtime: &ExRuntime<F>,
    dict: &DictRef,
    key: &OxStr,
    bang: bool,
    target: &str,
) -> Result<bool, Flow> {
    let mut data = dict.try_borrow_mut().map_err(|_| borrow_error(runtime))?;
    if data.lock.locked {
        return Err(lvalue_error(runtime, "E741", "Value is locked"));
    }
    let Some(index) = data
        .entries
        .iter()
        .position(|entry| entry.key.as_bytes() == key.as_bytes())
    else {
        if bang {
            return Ok(false);
        }
        return Err(lvalue_error(
            runtime,
            "E716",
            format!("Key not present in Dictionary: {}", key.to_string_lossy()),
        ));
    };
    // Dict-item resolution checks the read-only flag only; the fixed bit is
    // not consulted on this path (`do_unlet_var`).
    if data.entries[index]
        .flags
        .intersects(DictEntryFlags::READ_ONLY)
    {
        return Err(lvalue_error(
            runtime,
            "E46",
            format!("Cannot change read-only variable \"{target}\""),
        ));
    }
    data.entries.remove(index);
    Ok(true)
}

fn list_read_index<F: FileIO>(
    runtime: &ExRuntime<F>,
    list: &ListRef,
    index: &Typval,
) -> Result<Typval, Flow> {
    let requested = typval_to_number_strict(index).map_err(|error| eval_error(runtime, error))?;
    let items = list
        .try_borrow()
        .map_err(|_| borrow_error(runtime))?
        .items
        .clone();
    let normalized = normalize_list_index(items.len(), requested).ok_or_else(|| {
        lvalue_error(
            runtime,
            "E684",
            format!("list index out of range: {requested}"),
        )
    })?;
    Ok(items[normalized].clone())
}

fn list_read_slice<F: FileIO>(
    runtime: &ExRuntime<F>,
    list: &ListRef,
    start: Option<&Typval>,
    end: Option<&Typval>,
) -> Result<Typval, Flow> {
    let start = bound_to_i64(start).map_err(|error| eval_error(runtime, error))?;
    let end = bound_to_i64(end).map_err(|error| eval_error(runtime, error))?;
    let items = list
        .try_borrow()
        .map_err(|_| borrow_error(runtime))?
        .items
        .clone();
    let (start, end) = list_slice_bounds(items.len(), start, end);
    Ok(Typval::list(items[start..end].to_vec()))
}

fn list_write_index<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    list: &ListRef,
    index: &Typval,
    value: Typval,
) -> Result<(), Flow> {
    let requested = typval_to_number_strict(index).map_err(|error| eval_error(runtime, error))?;
    let mut data = list.try_borrow_mut().map_err(|_| borrow_error(runtime))?;
    if data.lock.locked {
        return Err(lvalue_error(runtime, "E741", "Value is locked"));
    }
    let normalized = normalize_list_index(data.items.len(), requested).ok_or_else(|| {
        lvalue_error(
            runtime,
            "E684",
            format!("list index out of range: {requested}"),
        )
    })?;
    data.items[normalized] = value;
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "bounded and unbounded slice assignment are two halves of one Vim semantics operation; splitting obscures the shared index resolution"
)]
fn list_write_slice<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    list: &ListRef,
    start: Option<&Typval>,
    end: Option<&Typval>,
    value: Typval,
) -> Result<(), Flow> {
    let Typval::List(source) = value else {
        return Err(lvalue_error(runtime, "E709", "invalid value for slice"));
    };

    // Snapshot the RHS before borrowing the destination mutably.
    // If the source and destination are the same List identity, clone the
    // current items and release the borrow before touching the destination.
    let source_items = if Rc::ptr_eq(&source, list) {
        list.try_borrow()
            .map_err(|_| borrow_error(runtime))?
            .items
            .clone()
    } else {
        source
            .try_borrow()
            .map_err(|_| borrow_error(runtime))?
            .items
            .clone()
    };

    let start_i64 = bound_to_i64(start).map_err(|error| eval_error(runtime, error))?;
    let end_i64 = bound_to_i64(end).map_err(|error| eval_error(runtime, error))?;

    let mut data = list.try_borrow_mut().map_err(|_| borrow_error(runtime))?;
    if data.lock.locked {
        return Err(lvalue_error(runtime, "E741", "Value is locked"));
    }

    let len = data.items.len();
    let len_i64 = i64::try_from(len).unwrap_or(i64::MAX);

    // Resolve the start index. Negative is relative to the end; if still
    // negative, clamp to 0 (matching tv_list_find_index). Out of range is
    // an error, so a bounded assignment cannot start beyond the list tail.
    let mut first = start_i64.unwrap_or(0);
    if first < 0 {
        first = first.saturating_add(len_i64);
        if first < 0 {
            first = 0;
        }
    }
    let start_idx = match usize::try_from(first) {
        Ok(idx) if idx < len => idx,
        _ => {
            return Err(lvalue_error(
                runtime,
                "E684",
                format!("list index out of range: {first}"),
            ));
        }
    };

    // Resolve the end index. Negative end must point into the list;
    // otherwise the literal end is used so a bounded assignment can extend
    // the list when the end is past the current tail.
    let end_idx = match end_i64 {
        None => None,
        Some(mut end) => {
            if end < 0 {
                end = end.saturating_add(len_i64);
                if end < 0 {
                    return Err(lvalue_error(
                        runtime,
                        "E684",
                        format!("list index out of range: {end}"),
                    ));
                }
            }
            if end < first {
                return Err(lvalue_error(
                    runtime,
                    "E684",
                    format!("list index out of range: {end}"),
                ));
            }
            Some(end)
        }
    };

    if let Some(end_idx) = end_idx {
        // Bounded: source must exactly cover [first, end_idx] inclusive.
        let count = i128::from(end_idx) - i128::from(first) + 1;
        if count <= 0 {
            return Err(lvalue_error(
                runtime,
                "E684",
                format!("list index out of range: {end_idx}"),
            ));
        }
        let Ok(required) = usize::try_from(count) else {
            return Err(lvalue_error(
                runtime,
                "E712",
                "List value has less items than target",
            ));
        };
        if source_items.len() > required {
            return Err(lvalue_error(
                runtime,
                "E710",
                "List value has more items than target",
            ));
        }
        if source_items.len() < required {
            return Err(lvalue_error(
                runtime,
                "E712",
                "List value has less items than target",
            ));
        }
        for (offset, item) in source_items.into_iter().enumerate() {
            let dest_idx = start_idx + offset;
            if dest_idx < data.items.len() {
                data.items[dest_idx] = item;
            } else {
                data.items.push(item);
            }
        }
        return Ok(());
    }

    // Unbounded: source replaces from start through the end of the list.
    // Extra source items extend the list; too few leave trailing items, E712.
    let tail = data.items.len() - start_idx;
    if source_items.len() < tail {
        return Err(lvalue_error(
            runtime,
            "E712",
            "List value has less items than target",
        ));
    }
    for (offset, item) in source_items.into_iter().enumerate() {
        let dest_idx = start_idx + offset;
        if dest_idx < data.items.len() {
            data.items[dest_idx] = item;
        } else {
            data.items.push(item);
        }
    }
    Ok(())
}

fn list_remove_index<F: FileIO>(
    runtime: &ExRuntime<F>,
    list: &ListRef,
    index: &Typval,
) -> Result<bool, Flow> {
    let requested = typval_to_number_strict(index).map_err(|error| eval_error(runtime, error))?;
    let mut data = list.try_borrow_mut().map_err(|_| borrow_error(runtime))?;
    if data.lock.locked {
        return Err(lvalue_error(runtime, "E741", "Value is locked"));
    }
    let normalized = normalize_list_index(data.items.len(), requested).ok_or_else(|| {
        lvalue_error(
            runtime,
            "E684",
            format!("list index out of range: {requested}"),
        )
    })?;
    data.items.remove(normalized);
    Ok(true)
}

fn list_remove_slice<F: FileIO>(
    runtime: &ExRuntime<F>,
    list: &ListRef,
    start: Option<&Typval>,
    end: Option<&Typval>,
) -> Result<bool, Flow> {
    let start_i64 = bound_to_i64(start).map_err(|error| eval_error(runtime, error))?;
    let end_i64 = bound_to_i64(end).map_err(|error| eval_error(runtime, error))?;
    let mut data = list.try_borrow_mut().map_err(|_| borrow_error(runtime))?;
    if data.lock.locked {
        return Err(lvalue_error(runtime, "E741", "Value is locked"));
    }
    let (start_idx, end_exclusive) = list_slice_bounds(data.items.len(), start_i64, end_i64);
    if start_idx >= end_exclusive {
        return Ok(false);
    }
    data.items.drain(start_idx..end_exclusive);
    Ok(true)
}

fn assign_root_variable<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    scope: &mut Scope,
    name: &[u8],
    scope_kind: Option<ScopeKind>,
    value: Typval,
) -> Result<(), Flow> {
    if let Some(kind) = scope_kind {
        if kind == ScopeKind::Vim {
            return assign_vim_variable(runtime, scope, name, value);
        }
        scope
            .set_scoped(kind, name, 0, value)
            .map_err(|error| eval_error(runtime, error))
    } else {
        scope
            .set(name, value)
            .map_err(|error| eval_error(runtime, error))
    }
}

pub(crate) fn assign_vim_variable<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    scope: &mut Scope,
    name: &[u8],
    value: Typval,
) -> Result<(), Flow> {
    match vim_variable_type(name) {
        Some(vvar_type) => {
            let value = match vvar_type {
                VimVariableType::String => Typval::String(OxStr::from(
                    typval_to_string_strict(&value)
                        .map_err(|error| eval_error(runtime, error))?
                        .as_bytes(),
                )),
                VimVariableType::Number => Typval::Number(
                    typval_to_number_strict(&value).map_err(|error| eval_error(runtime, error))?,
                ),
                VimVariableType::List if matches!(value, Typval::List(_)) => value,
                VimVariableType::Dict if matches!(value, Typval::Dict(_)) => value,
                VimVariableType::List | VimVariableType::Dict => {
                    let name = String::from_utf8_lossy(name);
                    return Err(lvalue_error(
                        runtime,
                        "E963",
                        format!("Setting v:{name} to value with wrong type"),
                    ));
                }
            };
            scope.replace_pair(ScopeKind::Vim, &String::from_utf8_lossy(name), value);
            Ok(())
        }
        None => scope
            .set_scoped(ScopeKind::Vim, name, 0, value)
            .map_err(|error| eval_error(runtime, error)),
    }
}

fn assign_register<F: FileIO, E: ExEditorAccess>(
    runtime: &ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    register: u8,
    value: Typval,
) -> Result<(), Flow> {
    let text = typval_to_text(&value);
    let content = RegisterContent::from_text(text.as_bytes())
        .map_err(|error| lvalue_error(runtime, "E354", error.to_string()))?;
    access
        .with_ex_editor(|editor| editor.registers_mut().set(register as char, content))
        .map_err(|error| lvalue_error(runtime, "E354", error.to_string()))?;
    scope.set_register(&[register], value);
    Ok(())
}

/// `ex_let_env` (`eval/vars.c`:1349-1351): the assignment *is*
/// `vim_setenv_ext`, a process-environment change that children inherit.
fn assign_environment(name: &str, value: &Typval) {
    let text = typval_to_text(value);
    ox_sys::set_env(name, &text);
}

fn remove_root_variable(scope: &mut Scope, name: &[u8], scope_kind: Option<ScopeKind>) -> bool {
    match scope_kind {
        Some(ScopeKind::Global) => scope.remove_pair(ScopeKind::Global, name),
        Some(ScopeKind::Buffer) => scope.remove_pair(ScopeKind::Buffer, name),
        Some(ScopeKind::Window) => scope.remove_pair(ScopeKind::Window, name),
        Some(ScopeKind::Tab) => scope.remove_pair(ScopeKind::Tab, name),
        Some(ScopeKind::Script) => scope.remove_pair(ScopeKind::Script, name),
        Some(ScopeKind::Local) => scope.remove_pair(ScopeKind::Local, name),
        Some(ScopeKind::Argument | ScopeKind::Vim) => false,
        None => {
            scope.remove_pair(ScopeKind::Local, name) || scope.remove_pair(ScopeKind::Global, name)
        }
    }
}

fn clear_register(editor: &mut Editor, scope: &mut Scope, register: u8) -> bool {
    let Ok(content) = RegisterContent::characterwise(&[]) else {
        return false;
    };
    if editor
        .registers_mut()
        .set(register as char, content)
        .is_err()
    {
        return false;
    }
    scope.set_register(&[register], Typval::String(OxStr::from("")));
    true
}

fn scoped_lock_name(scope_kind: Option<ScopeKind>, name: &[u8]) -> Vec<u8> {
    match scope_kind {
        Some(kind) => [kind.as_str().as_bytes(), name].concat(),
        None => name.to_vec(),
    }
}

fn split_scoped_name(name: &[u8]) -> (Option<ScopeKind>, &[u8]) {
    if name.len() > 2
        && name[1] == b':'
        && let Some(kind) = ScopeKind::from_byte(name[0])
    {
        return (Some(kind), &name[2..]);
    }
    (None, name)
}

fn env_name_len(text: &str) -> usize {
    text.bytes()
        .position(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
        .unwrap_or(text.len())
}

fn trailing_special_garbage(text: &str) -> bool {
    text.as_bytes()
        .iter()
        .any(|byte| matches!(*byte, b'.' | b'[' | b']'))
}

fn split_comma_args(source: &str) -> Vec<&str> {
    split_top_level(source, b',', true)
}

fn split_top_level(source: &str, delimiter: u8, exact: bool) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut result = Vec::new();
    let mut start = 0usize;
    let mut quote = None;
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active) = quote {
            if byte == active && (index == 0 || bytes[index - 1] != b'\\') {
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if matches!(byte, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(byte, b')' | b']' | b'}') {
            depth = depth.saturating_sub(1);
        } else if depth == 0
            && (byte == delimiter || (!exact && delimiter == b' ' && byte.is_ascii_whitespace()))
        {
            if start < index {
                result.push(source[start..index].trim());
            }
            while index + 1 < bytes.len() && bytes[index + 1].is_ascii_whitespace() {
                index += 1;
            }
            start = index + 1;
        }
        index += 1;
    }
    if start < source.len() {
        result.push(source[start..].trim());
    }
    result
}

fn bound_to_i64(value: Option<&Typval>) -> Result<Option<i64>, EvalError> {
    value.map(typval_to_number_strict).transpose()
}

fn lvalue_error<F: FileIO>(
    runtime: &ExRuntime<F>,
    code: &'static str,
    message: impl Into<String>,
) -> Flow {
    Flow::Exception(runtime.exception(code, message))
}

fn eval_error<F: FileIO>(runtime: &ExRuntime<F>, error: EvalError) -> Flow {
    match error.kind {
        ox_eval::EvalErrorKind::NotImplemented(name) => {
            Flow::NotImplemented(name.to_string_lossy().into_owned())
        }
        ox_eval::EvalErrorKind::Vim => lvalue_error(runtime, error.code, error.message),
    }
}

fn borrow_error<F: FileIO>(runtime: &ExRuntime<F>) -> Flow {
    lvalue_error(runtime, "E742", "Value is locked")
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn split_comma_args_respects_quotes() {
        assert_eq!(split_comma_args(r"'a,b', c"), vec!["'a,b'", "c"]);
    }

    #[test]
    fn env_name_len_stops_at_first_invalid_byte() {
        assert_eq!(env_name_len("HOME"), 4);
        assert_eq!(env_name_len("HO=ME"), 2);
    }
}
