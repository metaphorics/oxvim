//! Tree-walking evaluation for parsed legacy Vimscript expressions.

use ox_types::{Funcref, OxStr, Special, Typval};

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::{EvalError, Result};
use crate::lexer::CaseSensitivity;
use crate::parser::{BinaryOp, CompareOp, Expr, ExprKind, OptionScope as AstOptionScope, UnaryOp};
use crate::scope::{OptionScope, Scope, ScopeKind};

/// Default maximum recursive evaluator depth.
pub const DEFAULT_MAX_EVAL_DEPTH: usize = 1_000;

/// Function-call integration point for Task 8b's builtin implementation.
pub trait BuiltinHost {
    /// Invoke a named Vimscript function.
    fn call(&mut self, name: &OxStr, args: Vec<Typval>, scope: &mut Scope) -> Result<Typval>;

    /// Invoke a named function through `receiver->name(...)`.
    fn call_method(&mut self, name: &OxStr, args: Vec<Typval>, scope: &mut Scope) -> Result<Typval> {
        self.call(name, args, scope)
    }
}

/// Regular-expression integration point for operators and pure regex builtins.
pub trait RegexEngine {
    /// Match `text` against a Vim regular expression.
    fn is_match(&self, text: &OxStr, pattern: &OxStr, ignore_case: bool) -> Result<bool>;

    /// Split `text` at Vim regular-expression matches.
    fn split(&self, _text: &OxStr, _pattern: &OxStr, _keep_empty: bool) -> Result<Vec<OxStr>> {
        Err(EvalError::new("E54", 0, "regular-expression split is not supported by this engine"))
    }

    /// Return the byte range of the first match at or after `start`.
    fn find(&self, _text: &OxStr, _pattern: &OxStr, _start: usize) -> Result<Option<(usize, usize)>> {
        Err(EvalError::new("E54", 0, "regular-expression search is not supported by this engine"))
    }

    /// Replace matches according to Vim's substitute flags.
    fn substitute(&self, _text: &OxStr, _pattern: &OxStr, _replacement: &OxStr, _flags: &OxStr) -> Result<OxStr> {
        Err(EvalError::new("E54", 0, "regular-expression substitution is not supported by this engine"))
    }
}

/// Host used when builtins have not been installed.
#[derive(Debug, Default)]
pub struct NoBuiltins;

impl BuiltinHost for NoBuiltins {
    fn call(&mut self, name: &OxStr, _args: Vec<Typval>, _scope: &mut Scope) -> Result<Typval> {
        Err(EvalError::new(
            "E117",
            0,
            format!("Unknown function: {}", name.to_string_lossy()),
        ))
    }
}

/// Regex seam used until `ox-regex` is connected.
#[derive(Debug, Default)]
pub struct NoRegex;

impl RegexEngine for NoRegex {
    fn is_match(&self, _text: &OxStr, _pattern: &OxStr, _ignore_case: bool) -> Result<bool> {
        Err(EvalError::new("E54", 0, "regular-expression engine is not installed"))
    }
}

#[derive(Clone)]
struct Closure {
    params: Vec<OxStr>,
    body: Expr,
    captured: Scope,
}

/// A shared, externally-oblivious registry of lambda closures.
///
/// Closure bodies and captured scopes live here under a stable index so that a
/// stored `Partial` (`<lambda>N`) resolves to the original closure even when
/// called through a different [`Evaluator`] that shares this registry. Each
/// registry carries a unique nonce so that a `Partial` created by one registry
/// cannot accidentally resolve to a closure with the same local index in an
/// unrelated registry. The nonce is carried on the `Funcref` itself, not in
/// the lambda name.
#[derive(Clone)]
pub struct ClosureRegistry {
    id: usize,
    closures: Rc<RefCell<Vec<Closure>>>,
}

impl Default for ClosureRegistry {
    fn default() -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            closures: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

impl ClosureRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of closures registered so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.closures.borrow().len()
    }

    /// True when no closures have been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.closures.borrow().is_empty()
    }

    fn register(&self, closure: Closure) -> (usize, usize) {
        let mut closures = self.closures.borrow_mut();
        let id = closures.len();
        closures.push(closure);
        (self.id, id)
    }

    fn resolve(&self, registry_id: usize, closure_id: usize) -> Option<Closure> {
        if registry_id != self.id {
            return None;
        }
        self.closures.borrow().get(closure_id).cloned()
    }
}

struct Evaluated {
    value: Typval,
    identity: Option<usize>,
}

impl Evaluated {
    fn plain(value: Typval) -> Self {
        Self { value, identity: None }
    }
}

/// A reusable evaluator with host seams and a shared closure registry.
pub struct Evaluator<'a, H: BuiltinHost, R: RegexEngine> {
    host: &'a mut H,
    regex: &'a R,
    ignore_case: bool,
    max_depth: usize,
    closures: ClosureRegistry,
}

impl<'a, H: BuiltinHost, R: RegexEngine> Evaluator<'a, H, R> {
    /// Construct an evaluator. Default string comparisons are case-sensitive.
    pub fn new(host: &'a mut H, regex: &'a R) -> Self {
        Self {
            host,
            regex,
            ignore_case: false,
            max_depth: DEFAULT_MAX_EVAL_DEPTH,
            closures: ClosureRegistry::new(),
        }
    }

    /// Borrow the shared closure registry.
    ///
    /// Cloning the registry and passing it to another [`Evaluator`] (or reusing
    /// it for a second run) lets stored `Partial`s resolve to the closures they
    /// were created from, independent of which evaluator invokes them.
    #[must_use]
    pub const fn closure_registry(&self) -> &ClosureRegistry {
        &self.closures
    }

    /// Reuse an existing closure registry for this evaluator.
    #[must_use]
    pub fn with_closure_registry(mut self, registry: ClosureRegistry) -> Self {
        self.closures = registry;
        self
    }

    /// Select the value of Vim's `'ignorecase'` option for unsuffixed comparisons.
    #[must_use]
    pub const fn with_ignore_case(mut self, ignore_case: bool) -> Self {
        self.ignore_case = ignore_case;
        self
    }

    /// Override the recursive evaluation budget.
    #[must_use]
    pub const fn with_max_depth(mut self, maximum: usize) -> Self {
        self.max_depth = maximum;
        self
    }

    /// Evaluate one parsed expression in `scope`.
    pub fn eval(&mut self, expression: &Expr, scope: &mut Scope) -> Result<Typval> {
        self.eval_at(expression, scope, 0).map(|evaluated| evaluated.value)
    }

    /// Apply numeric coercion used by legacy `:if`, distinct from `tv2bool()`.
    pub fn condition_number(value: &Typval) -> Result<bool> {
        Ok(to_number(value, 0)? != 0)
    }

    fn eval_at(&mut self, expression: &Expr, scope: &mut Scope, depth: usize) -> Result<Evaluated> {
        if depth >= self.max_depth {
            return Err(EvalError::new(
                "E1169",
                expression.span.start,
                "expression evaluation nesting is too deep",
            ));
        }
        let next = depth + 1;
        match &expression.kind {
            ExprKind::Literal(value) => {
                let identity = identity_type(value).then_some(expression as *const Expr as usize);
                Ok(Evaluated { value: value.clone(), identity })
            }
            ExprKind::Variable(name) => self.eval_variable(name, expression.span.start, scope),
            ExprKind::Environment(name) => Ok(Evaluated::plain(scope.get_env(name.as_bytes()).clone())),
            ExprKind::Option { scope: option_scope, name } => {
                let option_scope = match option_scope {
                    AstOptionScope::Effective => OptionScope::Effective,
                    AstOptionScope::Global => OptionScope::Global,
                    AstOptionScope::Local => OptionScope::Local,
                };
                Ok(Evaluated::plain(scope.get_option(option_scope, name.as_bytes()).clone()))
            }
            ExprKind::Register(name) => Ok(Evaluated::plain(scope.get_register(&[*name]).clone())),
            ExprKind::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval_at(item, scope, next)?.value);
                }
                Ok(Evaluated {
                    value: Typval::List(values),
                    identity: Some(expression as *const Expr as usize),
                })
            }
            ExprKind::Dict(entries) => {
                let mut values = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    let key_offset = key.span.start;
                    let key_value = self.eval_at(key, scope, next)?.value;
                    let key = to_string(&key_value, key_offset)?;
                    if values.iter().any(|(existing, _): &(OxStr, Typval)| existing == &key) {
                        return Err(EvalError::new("E721", key_offset, "duplicate dictionary key"));
                    }
                    values.push((key, self.eval_at(value, scope, next)?.value));
                }
                Ok(Evaluated {
                    value: Typval::Dict(values),
                    identity: Some(expression as *const Expr as usize),
                })
            }
            ExprKind::Unary { op, expr } => {
                let value = self.eval_at(expr, scope, next)?.value;
                self.eval_unary(*op, value, expression.span.start)
            }
            ExprKind::Binary { op: BinaryOp::And, left, right } => {
                let lhs = self.eval_at(left, scope, next)?.value;
                if to_number(&lhs, left.span.start)? == 0 {
                    return Ok(Evaluated::plain(Typval::Number(0)));
                }
                let rhs = self.eval_at(right, scope, next)?.value;
                Ok(Evaluated::plain(Typval::Number(i64::from(to_number(&rhs, right.span.start)? != 0))))
            }
            ExprKind::Binary { op: BinaryOp::Or, left, right } => {
                let lhs = self.eval_at(left, scope, next)?.value;
                if to_number(&lhs, left.span.start)? != 0 {
                    return Ok(Evaluated::plain(Typval::Number(1)));
                }
                let rhs = self.eval_at(right, scope, next)?.value;
                Ok(Evaluated::plain(Typval::Number(i64::from(to_number(&rhs, right.span.start)? != 0))))
            }
            ExprKind::Binary { op, left, right } => {
                let lhs = self.eval_at(left, scope, next)?.value;
                let rhs = self.eval_at(right, scope, next)?.value;
                self.eval_binary(*op, lhs, rhs, expression.span.start)
            }
            ExprKind::Compare { op, case, left, right } => {
                let lhs = self.eval_at(left, scope, next)?;
                let rhs = self.eval_at(right, scope, next)?;
                let ignore_case = match case {
                    CaseSensitivity::Default => self.ignore_case,
                    CaseSensitivity::MatchCase => false,
                    CaseSensitivity::IgnoreCase => true,
                };
                let result = self.compare(*op, &lhs, &rhs, ignore_case, expression.span.start, next)?;
                Ok(Evaluated::plain(Typval::Number(i64::from(result))))
            }
            ExprKind::Ternary { condition, then_expr, else_expr } => {
                let condition = self.eval_at(condition, scope, next)?.value;
                if to_number(&condition, expression.span.start)? != 0 {
                    self.eval_at(then_expr, scope, next)
                } else {
                    self.eval_at(else_expr, scope, next)
                }
            }
            ExprKind::Coalesce { left, right } => {
                let lhs = self.eval_at(left, scope, next)?;
                if lhs.value.is_truthy() { Ok(lhs) } else { self.eval_at(right, scope, next) }
            }
            ExprKind::Call { callee, args } => self.eval_call(callee, args, scope, next),
            ExprKind::Member { target, name } => {
                let target = self.eval_at(target, scope, next)?;
                dict_lookup(&target.value, name.as_bytes(), expression.span.start, target.identity)
            }
            ExprKind::Index { target, index } => {
                let target = self.eval_at(target, scope, next)?;
                let index_value = self.eval_at(index, scope, next)?.value;
                self.index(target, index_value, expression.span.start)
            }
            ExprKind::Slice { target, start, end } => {
                let target = self.eval_at(target, scope, next)?.value;
                let start = match start {
                    Some(bound) => Some(to_number(&self.eval_at(bound, scope, next)?.value, bound.span.start)?),
                    None => None,
                };
                let end = match end {
                    Some(bound) => Some(to_number(&self.eval_at(bound, scope, next)?.value, bound.span.start)?),
                    None => None,
                };
                self.slice(target, start, end, expression.span.start)
            }
            ExprKind::MethodCall { receiver, method, args } => {
                let receiver = self.eval_at(receiver, scope, next)?;
                let mut values = Vec::with_capacity(args.len() + 1);
                values.push(receiver.value);
                for arg in args {
                    values.push(self.eval_at(arg, scope, next)?.value);
                }
                self.call_method(method, values, scope, next)
            }
            ExprKind::Lambda { params, body, .. } => {
                let (registry_id, id) = self.closures.register(Closure {
                    params: params.clone(),
                    body: body.as_ref().clone(),
                    captured: scope.snapshot(),
                });
                let name = OxStr(format!("<lambda>{id}").into_bytes());
                let identity = Some(registry_id.wrapping_mul(0x9e37_79b9).wrapping_add(id));
                Ok(Evaluated {
                    value: Typval::Partial(Funcref { name, args: Vec::new(), dict: None, registry: Some(registry_id) }),
                    identity,
                })
            }
        }
    }

    fn eval_variable(&self, name: &OxStr, offset: usize, scope: &Scope) -> Result<Evaluated> {
        match name.as_bytes() {
            b"v:true" => return Ok(Evaluated::plain(Typval::Bool(true))),
            b"v:false" => return Ok(Evaluated::plain(Typval::Bool(false))),
            b"v:null" | b"v:none" => return Ok(Evaluated::plain(Typval::Special(Special::Null))),
            _ => {}
        }
        let bytes = name.as_bytes();
        if bytes.len() == 2 && bytes[1] == b':' {
            let kind = ScopeKind::from_byte(bytes[0]).ok_or_else(|| {
                EvalError::new("E121", offset, format!("Undefined variable: {}", name.to_string_lossy()))
            })?;
            let map = match kind {
                ScopeKind::Global => &scope.global,
                ScopeKind::Buffer => &scope.buffer,
                ScopeKind::Window => &scope.window,
                ScopeKind::Tab => &scope.tab,
                ScopeKind::Script => &scope.script,
                ScopeKind::Local => &scope.local,
                ScopeKind::Argument => &scope.argument,
                ScopeKind::Vim => &scope.vim,
            };
            return Ok(Evaluated {
                value: scope.scope_dict(kind),
                identity: Some(map as *const _ as usize),
            });
        }
        let value = if bytes.len() >= 2 && bytes[1] == b':' {
            scope.get_scoped(ScopeKind::from_byte(bytes[0]).ok_or_else(|| {
                EvalError::new("E121", offset, format!("Undefined variable: {}", name.to_string_lossy()))
            })?, &bytes[2..], offset)?
        } else {
            scope.get(bytes, offset)?
        };
        let identity = identity_type(value).then_some(value as *const Typval as usize);
        Ok(Evaluated { value: value.clone(), identity })
    }

    fn eval_unary(&self, op: UnaryOp, value: Typval, offset: usize) -> Result<Evaluated> {
        match op {
            UnaryOp::Not => match value {
                Typval::Float(value) => Ok(Evaluated::plain(Typval::Number(i64::from(value == 0.0)))),
                other => Ok(Evaluated::plain(Typval::Number(i64::from(to_number(&other, offset)? == 0)))),
            },
            UnaryOp::Plus => match value {
                Typval::Float(value) => Ok(Evaluated::plain(Typval::Float(value))),
                other => Ok(Evaluated::plain(Typval::Number(to_number(&other, offset)?))),
            },
            UnaryOp::Negate => match value {
                Typval::Float(value) => Ok(Evaluated::plain(Typval::Float(-value))),
                other => Ok(Evaluated::plain(Typval::Number(to_number(&other, offset)?.wrapping_neg()))),
            },
        }
    }

    fn eval_binary(&self, op: BinaryOp, lhs: Typval, rhs: Typval, offset: usize) -> Result<Evaluated> {
        if op == BinaryOp::Concat {
            let mut bytes = to_string(&lhs, offset)?.0;
            bytes.extend_from_slice(to_string(&rhs, offset)?.as_bytes());
            return Ok(Evaluated::plain(Typval::String(OxStr(bytes))));
        }
        if op == BinaryOp::Add {
            match (lhs, rhs) {
                (Typval::List(mut left), Typval::List(right)) => {
                    left.extend(right);
                    return Ok(Evaluated::plain(Typval::List(left)));
                }
                (Typval::Blob(mut left), Typval::Blob(right)) => {
                    left.extend(right);
                    return Ok(Evaluated::plain(Typval::Blob(left)));
                }
                (left, right) => return numeric_binary(op, left, right, offset),
            }
        }
        numeric_binary(op, lhs, rhs, offset)
    }

    fn compare(
        &self,
        op: CompareOp,
        lhs: &Evaluated,
        rhs: &Evaluated,
        ignore_case: bool,
        offset: usize,
        depth: usize,
    ) -> Result<bool> {
        if matches!(op, CompareOp::Match | CompareOp::NoMatch) {
            let text = to_string(&lhs.value, offset)?;
            let pattern = to_string(&rhs.value, offset)?;
            let matched = self.regex.is_match(&text, &pattern, ignore_case)?;
            return Ok(if op == CompareOp::NoMatch { !matched } else { matched });
        }
        if matches!(op, CompareOp::Is | CompareOp::IsNot) {
            let same = if identity_type(&lhs.value) && identity_type(&rhs.value) {
                lhs.identity.is_some() && lhs.identity == rhs.identity
            } else {
                equal_values(&lhs.value, &rhs.value, ignore_case, depth, self.max_depth)?
            };
            return Ok(if op == CompareOp::IsNot { !same } else { same });
        }
        if matches!((&lhs.value, &rhs.value), (Typval::List(_), Typval::List(_)))
            && !matches!(op, CompareOp::Equal | CompareOp::NotEqual)
        {
            return Err(EvalError::new("E692", offset, "Invalid operation for List"));
        }
        if matches!((&lhs.value, &rhs.value), (Typval::Dict(_), Typval::Dict(_)))
            && !matches!(op, CompareOp::Equal | CompareOp::NotEqual)
        {
            return Err(EvalError::new("E736", offset, "Invalid operation for Dictionary"));
        }
        let ordering = compare_values(&lhs.value, &rhs.value, ignore_case, offset, depth, self.max_depth)?;
        Ok(match op {
            CompareOp::Equal => ordering == 0,
            CompareOp::NotEqual => ordering != 0,
            CompareOp::Greater => ordering > 0,
            CompareOp::GreaterEqual => ordering >= 0,
            CompareOp::Less => ordering < 0,
            CompareOp::LessEqual => ordering <= 0,
            CompareOp::Match | CompareOp::NoMatch | CompareOp::Is | CompareOp::IsNot => false,
        })
    }

    fn index(&self, target: Evaluated, index: Typval, offset: usize) -> Result<Evaluated> {
        match target.value {
            Typval::List(values) => {
                let requested = to_number(&index, offset)?;
                let normalized = normalize_list_index(values.len(), requested)
                    .ok_or_else(|| EvalError::new("E684", offset, format!("list index out of range: {requested}")))?;
                let value = values[normalized].clone();
                let identity = identity_type(&value).then(|| derive_identity(target.identity, &normalized.to_le_bytes())).flatten();
                Ok(Evaluated { value, identity })
            }
            Typval::Blob(values) => {
                let requested = to_number(&index, offset)?;
                let normalized = normalize_list_index(values.len(), requested)
                    .ok_or_else(|| EvalError::new("E979", offset, format!("blob index out of range: {requested}")))?;
                Ok(Evaluated::plain(Typval::Number(i64::from(values[normalized]))))
            }
            Typval::Dict(values) => {
                let key = to_string(&index, offset)?;
                dict_lookup(&Typval::Dict(values), key.as_bytes(), offset, target.identity)
            }
            value @ (Typval::Number(_) | Typval::String(_)) => {
                let bytes = to_string(&value, offset)?;
                let index = to_number(&index, offset)?;
                if index < 0 || usize::try_from(index).map_or(true, |index| index >= bytes.0.len()) {
                    return Ok(Evaluated::plain(Typval::String(OxStr(Vec::new()))));
                }
                let index = usize::try_from(index).map_err(|_| EvalError::new("E111", offset, "invalid string index"))?;
                Ok(Evaluated::plain(Typval::String(OxStr(vec![bytes.0[index]]))))
            }
            _ => Err(EvalError::new("E909", offset, "invalid value for subscript")),
        }
    }

    fn slice(&self, target: Typval, start: Option<i64>, end: Option<i64>, offset: usize) -> Result<Evaluated> {
        match target {
            Typval::List(values) => {
                let (start, end) = list_slice_bounds(values.len(), start, end);
                Ok(Evaluated::plain(Typval::List(values[start..end].to_vec())))
            }
            Typval::Blob(values) => {
                let (start, end) = list_slice_bounds(values.len(), start, end);
                Ok(Evaluated::plain(Typval::Blob(values[start..end].to_vec())))
            }
            value @ (Typval::Number(_) | Typval::String(_)) => {
                let value = to_string(&value, offset)?;
                let (start, end) = string_slice_bounds(value.0.len(), start, end);
                Ok(Evaluated::plain(Typval::String(OxStr(value.0[start..end].to_vec()))))
            }
            Typval::Dict(_) => Err(EvalError::new("E719", offset, "Cannot slice a Dictionary")),
            _ => Err(EvalError::new("E709", offset, "invalid value for slice")),
        }
    }

    fn eval_call(&mut self, callee: &Expr, args: &[Expr], scope: &mut Scope, depth: usize) -> Result<Evaluated> {
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval_at(arg, scope, depth)?.value);
        }
        if let ExprKind::Variable(name) = &callee.kind {
            match self.eval_variable(name, callee.span.start, scope) {
                Ok(value) => return self.call_value(value.value, values, scope, depth),
                Err(error) if error.code == "E121" => {}
                Err(error) => return Err(error),
            }
            return self.call_named(name, values, scope, callee.span.start, depth);
        }
        let callee = self.eval_at(callee, scope, depth)?;
        self.call_value(callee.value, values, scope, depth)
    }

    fn call_method(&mut self, method: &Expr, args: Vec<Typval>, scope: &mut Scope, depth: usize) -> Result<Evaluated> {
        if let ExprKind::Variable(name) = &method.kind {
            self.host.call_method(name, args, scope).map(Evaluated::plain).map_err(|mut error| {
                if error.code == "E117" { error.offset = method.span.start; }
                error
            })
        } else {
            let method = self.eval_at(method, scope, depth)?;
            self.call_value(method.value, args, scope, depth)
        }
    }

    fn call_named(
        &mut self,
        name: &OxStr,
        args: Vec<Typval>,
        scope: &mut Scope,
        offset: usize,
        _depth: usize,
    ) -> Result<Evaluated> {
        self.host.call(name, args, scope).map(Evaluated::plain).map_err(|mut error| {
            if error.code == "E117" { error.offset = offset; }
            error
        })
    }

    fn call_value(&mut self, callee: Typval, mut args: Vec<Typval>, scope: &mut Scope, depth: usize) -> Result<Evaluated> {
        match callee {
            Typval::Funcref(funcref) | Typval::Partial(funcref) => {
                if !funcref.args.is_empty() {
                    let mut bound = funcref.args;
                    bound.append(&mut args);
                    args = bound;
                }
                match funcref.registry {
                    Some(registry_id) => {
                        let id = closure_index(funcref.name.as_bytes()).ok_or_else(|| {
                            EvalError::new("E117", 0, format!("Unknown function: {}", funcref.name.to_string_lossy()))
                        })?;
                        self.call_closure(&funcref.name, registry_id, id, &args, depth, 0)
                    }
                    None => self.call_named(&funcref.name, args, scope, 0, depth),
                }
            }
            _ => Err(EvalError::new("E1085", 0, "not a callable value")),
        }
    }

    fn call_closure(
        &mut self,
        name: &OxStr,
        registry_id: usize,
        id: usize,
        args: &[Typval],
        depth: usize,
        offset: usize,
    ) -> Result<Evaluated> {
        let closure = self.closures.resolve(registry_id, id).ok_or_else(|| {
            EvalError::new("E117", offset, format!("Unknown function: {}", name.to_string_lossy()))
        })?;
        if args.len() < closure.params.len() {
            return Err(EvalError::new("E119", offset, "not enough or too many arguments for function"));
        }
        let mut scope = closure.captured;
        for (param, value) in closure.params.iter().zip(args) {
            // Named parameters resolve both unqualified and as `a:name`.
            scope.local.retain(|(name, _)| name != param);
            bind_argument(&mut scope, param, value.clone());
        }
        // Any extra arguments behave like a variadic `...`: `a:0` is their
        // count, `a:000` is the List, and `a:1`, `a:2`, ... index them. Vim
        // always treats lambda parameter lists as variadic (uf_varargs=true).
        let extras = &args[closure.params.len()..];
        let extra_count = i64::try_from(extras.len()).unwrap_or(i64::MAX);
        bind_argument(&mut scope, &OxStr::from("0"), Typval::Number(extra_count));
        bind_argument(&mut scope, &OxStr::from("000"), Typval::List(extras.to_vec()));
        for (index, value) in extras.iter().enumerate() {
            let name = OxStr(format!("{}", index + 1).into_bytes());
            bind_argument(&mut scope, &name, value.clone());
        }
        self.eval_at(&closure.body, &mut scope, depth)
    }
}

fn bind_argument(scope: &mut Scope, name: &OxStr, value: Typval) {
    scope.argument.retain(|(existing, _)| existing != name);
    scope.argument.push((name.clone(), value));
}

fn identity_type(value: &Typval) -> bool {
    matches!(value, Typval::List(_) | Typval::Dict(_) | Typval::Blob(_) | Typval::Funcref(_) | Typval::Partial(_))
}

fn closure_index(name: &[u8]) -> Option<usize> {
    let digits = name.strip_prefix(b"<lambda>")?;
    let text = std::str::from_utf8(digits).ok()?;
    text.parse().ok()
}

fn dict_lookup(value: &Typval, key: &[u8], offset: usize, parent_identity: Option<usize>) -> Result<Evaluated> {
    let Typval::Dict(entries) = value else {
        return Err(EvalError::new("E715", offset, "Dictionary required"));
    };
    entries
        .iter()
        .find(|(candidate, _)| candidate.as_bytes() == key)
        .map(|(_, value)| Evaluated {
            value: value.clone(),
            identity: identity_type(value).then(|| derive_identity(parent_identity, key)).flatten(),
        })
        .ok_or_else(|| EvalError::new("E716", offset, format!("Key not present in Dictionary: {}", String::from_utf8_lossy(key))))
}

fn numeric_binary(op: BinaryOp, lhs: Typval, rhs: Typval, offset: usize) -> Result<Evaluated> {
    if matches!(lhs, Typval::Float(_)) || matches!(rhs, Typval::Float(_)) {
        if op == BinaryOp::Modulo {
            return Err(EvalError::new("E804", offset, "Cannot use '%' with Float"));
        }
        let left = to_float_arithmetic(&lhs, offset)?;
        let right = to_float_arithmetic(&rhs, offset)?;
        let value = match op {
            BinaryOp::Add => left + right,
            BinaryOp::Subtract => left - right,
            BinaryOp::Multiply => left * right,
            BinaryOp::Divide => left / right,
            _ => return Err(EvalError::new("E15", offset, "invalid floating-point operator")),
        };
        return Ok(Evaluated::plain(Typval::Float(value)));
    }
    let left = to_number(&lhs, offset)?;
    let right = to_number(&rhs, offset)?;
    let value = match op {
        BinaryOp::Add => left.wrapping_add(right),
        BinaryOp::Subtract => left.wrapping_sub(right),
        BinaryOp::Multiply => left.wrapping_mul(right),
        BinaryOp::Divide => vim_divide(left, right),
        BinaryOp::Modulo => if right == 0 { 0 } else { left.wrapping_rem(right) },
        _ => return Err(EvalError::new("E15", offset, "invalid numeric operator")),
    };
    Ok(Evaluated::plain(Typval::Number(value)))
}

fn vim_divide(left: i64, right: i64) -> i64 {
    if right == 0 {
        if left == 0 { i64::MIN } else if left > 0 { i64::MAX } else { i64::MIN }
    } else if left == i64::MIN && right == -1 {
        i64::MIN
    } else {
        left / right
    }
}

fn to_float_arithmetic(value: &Typval, offset: usize) -> Result<f64> {
    match value {
        Typval::Float(value) => Ok(*value),
        other => Ok(to_number(other, offset)? as f64),
    }
}

fn to_number(value: &Typval, offset: usize) -> Result<i64> {
    match value {
        Typval::Number(value) => Ok(*value),
        Typval::Channel(value) | Typval::Job(value) => Ok(i64::try_from(*value).unwrap_or(i64::MAX)),
        Typval::String(value) => Ok(parse_number_prefix(value.as_bytes())),
        Typval::Bool(value) => Ok(i64::from(*value)),
        Typval::Special(Special::Null) => Ok(0),
        Typval::Funcref(_) | Typval::Partial(_) => Err(EvalError::new("E703", offset, "Using a Funcref as a Number")),
        Typval::List(_) => Err(EvalError::new("E745", offset, "Using a List as a Number")),
        Typval::Dict(_) => Err(EvalError::new("E728", offset, "Using a Dictionary as a Number")),
        Typval::Float(_) => Err(EvalError::new("E805", offset, "Using a Float as a Number")),
        Typval::Blob(_) => Err(EvalError::new("E974", offset, "Using a Blob as a Number")),
    }
}

fn parse_number_prefix(bytes: &[u8]) -> i64 {
    let mut index = 0;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) { index += 1; }
    let negative = bytes.get(index) == Some(&b'-');
    if matches!(bytes.get(index), Some(b'-' | b'+')) { index += 1; }
    let rest = &bytes[index..];
    let (base, prefix, legacy_octal) = if rest.starts_with(b"0x") || rest.starts_with(b"0X") {
        (16_u32, 2, false)
    } else if rest.starts_with(b"0b") || rest.starts_with(b"0B") {
        (2, 2, false)
    } else if rest.starts_with(b"0o") || rest.starts_with(b"0O") {
        (8, 2, false)
    } else {
        (10, 0, rest.len() > 1 && rest.first() == Some(&b'0'))
    };
    let base = if legacy_octal { 8 } else { base };
    let mut value = 0_i64;
    let mut any = false;
    for &byte in &rest[prefix..] {
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'f' => u32::from(byte - b'a') + 10,
            b'A'..=b'F' => u32::from(byte - b'A') + 10,
            _ => break,
        };
        if digit >= base { break; }
        any = true;
        value = value.wrapping_mul(i64::from(base)).wrapping_add(i64::from(digit));
    }
    if !any { return 0; }
    if negative { value.wrapping_neg() } else { value }
}

fn to_string(value: &Typval, offset: usize) -> Result<OxStr> {
    match value {
        Typval::String(value) => Ok(value.clone()),
        Typval::Number(value) => Ok(OxStr(value.to_string().into_bytes())),
        Typval::Float(_) => Err(EvalError::new("E806", offset, "Using a Float as a String")),
        Typval::Channel(value) | Typval::Job(value) => Ok(OxStr(value.to_string().into_bytes())),
        Typval::Bool(true) => Ok(OxStr::from("v:true")),
        Typval::Bool(false) => Ok(OxStr::from("v:false")),
        Typval::Special(Special::Null) => Ok(OxStr::from("v:null")),
        Typval::Funcref(_) | Typval::Partial(_) => Err(EvalError::new("E729", offset, "Using a Funcref as a String")),
        Typval::List(_) => Err(EvalError::new("E730", offset, "Using a List as a String")),
        Typval::Dict(_) => Err(EvalError::new("E731", offset, "Using a Dictionary as a String")),
        Typval::Blob(_) => Err(EvalError::new("E976", offset, "Using a Blob as a String")),
    }
}

fn compare_values(
    lhs: &Typval,
    rhs: &Typval,
    ignore_case: bool,
    offset: usize,
    depth: usize,
    maximum: usize,
) -> Result<i8> {
    if matches!(lhs, Typval::List(_)) || matches!(rhs, Typval::List(_)) {
        if !matches!((lhs, rhs), (Typval::List(_), Typval::List(_))) {
            return Err(EvalError::new("E691", offset, "Can only compare List with List"));
        }
        return Ok(if equal_values(lhs, rhs, ignore_case, depth, maximum)? { 0 } else { 1 });
    }
    if matches!(lhs, Typval::Dict(_)) || matches!(rhs, Typval::Dict(_)) {
        if !matches!((lhs, rhs), (Typval::Dict(_), Typval::Dict(_))) {
            return Err(EvalError::new("E735", offset, "Can only compare Dictionary with Dictionary"));
        }
        return Ok(if equal_values(lhs, rhs, ignore_case, depth, maximum)? { 0 } else { 1 });
    }
    if matches!(lhs, Typval::Float(_)) || matches!(rhs, Typval::Float(_)) {
        let left = match lhs { Typval::Float(v) => *v, _ => to_number(lhs, offset)? as f64 };
        let right = match rhs { Typval::Float(v) => *v, _ => to_number(rhs, offset)? as f64 };
        return Ok(if left < right { -1 } else if left > right { 1 } else { 0 });
    }
    if matches!(lhs, Typval::Number(_) | Typval::Bool(_) | Typval::Special(_) | Typval::Channel(_) | Typval::Job(_))
        || matches!(rhs, Typval::Number(_) | Typval::Bool(_) | Typval::Special(_) | Typval::Channel(_) | Typval::Job(_))
    {
        return Ok(to_number(lhs, offset)?.cmp(&to_number(rhs, offset)?) as i8);
    }
    let left = to_string(lhs, offset)?;
    let right = to_string(rhs, offset)?;
    Ok(compare_bytes(left.as_bytes(), right.as_bytes(), ignore_case))
}

fn equal_values(lhs: &Typval, rhs: &Typval, ignore_case: bool, depth: usize, maximum: usize) -> Result<bool> {
    if depth >= maximum {
        return Err(EvalError::new("E1169", 0, "value comparison nesting is too deep"));
    }
    match (lhs, rhs) {
        (Typval::String(left), Typval::String(right)) => Ok(compare_bytes(left.as_bytes(), right.as_bytes(), ignore_case) == 0),
        (Typval::List(left), Typval::List(right)) => {
            if left.len() != right.len() { return Ok(false); }
            for (left, right) in left.iter().zip(right) {
                if !equal_values(left, right, ignore_case, depth + 1, maximum)? { return Ok(false); }
            }
            Ok(true)
        }
        (Typval::Dict(left), Typval::Dict(right)) => {
            if left.len() != right.len() { return Ok(false); }
            for (key, value) in left {
                let Some((_, other)) = right.iter().find(|(candidate, _)| candidate == key) else { return Ok(false); };
                if !equal_values(value, other, ignore_case, depth + 1, maximum)? { return Ok(false); }
            }
            Ok(true)
        }
        _ => Ok(lhs == rhs),
    }
}

/// Length in bytes of the UTF-8 sequence starting with `lead`, using only the
/// lead byte's value. Invalid or overlong leads, and continuation bytes, map
/// to 1 so they are decoded as a single raw byte below.
fn utf8_char_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if (0xC2..=0xDF).contains(&lead) {
        2
    } else if (0xE0..=0xEF).contains(&lead) {
        3
    } else if (0xF0..=0xF4).contains(&lead) {
        4
    } else {
        1
    }
}

/// Lowercase fold of a byte string into a flat byte sequence.
///
/// Each valid UTF-8 character is expanded via `char::to_lowercase` and the
/// resulting bytes are concatenated (e.g. `İ` becomes `i` + `U+0307`). Bytes
/// that do not begin a valid UTF-8 sequence are kept byte-for-byte and compared
/// case-sensitively, so invalid input never causes a panic and the ordering
/// stays deterministic.
fn fold_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut folded = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    let mut buf = [0u8; 4];
    while !rest.is_empty() {
        let len = utf8_char_len(rest[0]).min(rest.len());
        if let Ok(text) = std::str::from_utf8(&rest[..len]) {
            for ch in text.chars() {
                for lowered in ch.to_lowercase() {
                    let encoded = lowered.encode_utf8(&mut buf);
                    folded.extend_from_slice(encoded.as_bytes());
                }
            }
            rest = &rest[len..];
        } else {
            // Invalid, overlong, or truncated sequence: keep the offending
            // byte raw so it is compared case-sensitively and byte-wise.
            folded.push(rest[0]);
            rest = &rest[1..];
        }
    }
    folded
}

pub(crate) fn compare_bytes(lhs: &[u8], rhs: &[u8], ignore_case: bool) -> i8 {
    if !ignore_case {
        for (&left, &right) in lhs.iter().zip(rhs) {
            if left < right { return -1; }
            if left > right { return 1; }
        }
        return lhs.len().cmp(&rhs.len()) as i8;
    }
    // Unicode case folding from mb_strcmp_ic / mb_stricmp (mbyte.c utf_strnicmp):
    // compare the flattened lower-cased expansions of the whole string, and fall
    // back to a byte-wise case-sensitive comparison for invalid sequences.
    let left = fold_bytes(lhs);
    let right = fold_bytes(rhs);
    match left.cmp(&right) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal => 0,
    }
}

fn normalize_list_index(length: usize, requested: i64) -> Option<usize> {
    let length_i64 = i64::try_from(length).ok()?;
    let normalized = if requested < 0 { length_i64.checked_add(requested)? } else { requested };
    if normalized < 0 || normalized >= length_i64 { None } else { usize::try_from(normalized).ok() }
}

fn list_slice_bounds(length: usize, start: Option<i64>, end: Option<i64>) -> (usize, usize) {
    let len = i64::try_from(length).unwrap_or(i64::MAX);
    let mut first = start.unwrap_or(0);
    if first < 0 { first = first.saturating_add(len); }
    if first < 0 || first >= len { first = len; }
    let mut last = end.unwrap_or(len.saturating_sub(1));
    if last < 0 { last = last.saturating_add(len); }
    if last >= len { last = len.saturating_sub(1); }
    if last < first || last < 0 { return (length, length); }
    let start = usize::try_from(first).unwrap_or(length).min(length);
    let exclusive = usize::try_from(last.saturating_add(1)).unwrap_or(length).min(length);
    (start, exclusive.max(start))
}

fn string_slice_bounds(length: usize, start: Option<i64>, end: Option<i64>) -> (usize, usize) {
    let len = i64::try_from(length).unwrap_or(i64::MAX);
    let mut first = start.unwrap_or(0);
    if first < 0 { first = first.saturating_add(len).max(0); }
    let mut last = end.unwrap_or(len.saturating_sub(1));
    if last < 0 { last = last.saturating_add(len); }
    if last >= len { last = len.saturating_sub(1); }
    if first >= len || last < 0 || first > last { return (length, length); }
    let start = usize::try_from(first).unwrap_or(length).min(length);
    let exclusive = usize::try_from(last.saturating_add(1)).unwrap_or(length).min(length);
    (start, exclusive.max(start))
}

fn derive_identity(parent: Option<usize>, component: &[u8]) -> Option<usize> {
    let mut hash = (parent? as u64) ^ 0xcbf2_9ce4_8422_2325_u64;
    for &byte in component {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3_u64);
    }
    Some(hash as usize)
}
