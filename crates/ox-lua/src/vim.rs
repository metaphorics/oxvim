//! C-side core of the global `vim` Lua table.

use std::cell::Cell;
use std::rc::Rc;

use mlua::{
    FromLuaMulti, Function, Lua, LuaString, MetaMethod, MultiValue, Table, UserData,
    UserDataMethods, Value, Variadic,
};
use ox_api::Registry;
use ox_types::{Object, OxStr, Typval};

use crate::converter::{free_lua_ref, lua_to_object, object_to_lua, object_to_lua_legacy};
use crate::typval_bridge::{collect_typval_refs, free_typval_refs, lua_to_typval, typval_to_lua};

/// A deferred Lua callback owned by the eventual main-loop adapter.
pub type Work = Box<dyn FnOnce() -> mlua::Result<()> + 'static>;

/// Main-loop scheduling seam used by `vim.schedule`.
pub trait Scheduler {
    /// Enqueue work for a later normal-event-loop turn.
    ///
    /// # Errors
    ///
    /// Returns an error when the main-loop adapter cannot enqueue `work`.
    fn schedule_deferred(&self, work: Work) -> Result<(), String>;
}

/// Vimscript builtin dispatch seam used by `vim.call` and `vim.fn`.
pub trait BuiltinHost {
    /// Invoke a named Vimscript function with converted arguments.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot invoke `name`, including lookup,
    /// argument-conversion, and Vimscript execution failures.
    fn call(&self, name: &OxStr, args: Vec<Typval>) -> Result<Typval, String>;

    /// Whether this function is safe in a fast callback.
    fn is_fast(&self, _name: &OxStr) -> bool {
        false
    }
}

/// Variable namespaces exposed by `vim.g`, `vim.b`, `vim.w`, `vim.t`, and `vim.v`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VariableScope {
    /// Editor-global `g:` namespace.
    Global,
    /// Buffer-local `b:` namespace.
    Buffer,
    /// Window-local `w:` namespace.
    Window,
    /// Tabpage-local `t:` namespace.
    Tabpage,
    /// Internal `v:` namespace.
    Vim,
}

impl VariableScope {
    fn parse(scope: &[u8]) -> mlua::Result<Self> {
        match scope {
            b"g" => Ok(Self::Global),
            b"b" => Ok(Self::Buffer),
            b"w" => Ok(Self::Window),
            b"t" => Ok(Self::Tabpage),
            b"v" => Ok(Self::Vim),
            _ => Err(mlua::Error::runtime("invalid scope")),
        }
    }
}

/// Editor-owned variable storage captured by the Lua magic accessors.
pub trait VariableHost {
    /// Return one variable, or `None` when the key does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot read the requested variable.
    fn get_var(
        &self,
        scope: VariableScope,
        handle: i64,
        name: &OxStr,
    ) -> Result<Option<Object>, String>;

    /// Set one variable, or delete it when `value` is `None`.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot set or delete the requested
    /// variable.
    fn set_var(
        &self,
        scope: VariableScope,
        handle: i64,
        name: OxStr,
        value: Option<Object>,
    ) -> Result<(), String>;
}

/// Shared session context captured by the generated `vim.api` Lua closures.
#[derive(Clone)]
pub struct ApiDispatchContext {
    session: Rc<ox_api::ApiSession>,
    textlock_depth: Rc<Cell<u32>>,
}

impl ApiDispatchContext {
    /// Create a dispatch context for one API session.
    #[must_use]
    pub fn new(session: Rc<ox_api::ApiSession>) -> Self {
        Self {
            session,
            textlock_depth: Rc::new(Cell::new(0)),
        }
    }

    /// The session every generated `vim.api` closure dispatches through.
    #[must_use]
    pub fn session(&self) -> &ox_api::ApiSession {
        &self.session
    }

    /// Enter textlock until the returned guard is dropped.
    #[must_use]
    pub fn enter_textlock(&self) -> TextlockGuard {
        self.textlock_depth
            .set(self.textlock_depth.get().saturating_add(1));
        TextlockGuard {
            depth: self.textlock_depth.clone(),
        }
    }

    fn text_locked(&self) -> bool {
        self.textlock_depth.get() != 0
    }
}

/// Scope guard returned by [`ApiDispatchContext::enter_textlock`].
pub struct TextlockGuard {
    depth: Rc<Cell<u32>>,
}

impl Drop for TextlockGuard {
    fn drop(&mut self) {
        self.depth.set(self.depth.get().saturating_sub(1));
    }
}

/// Shared nesting counter for libuv-style fast callbacks.
#[derive(Clone, Default)]
pub struct FastCallbackState {
    depth: Rc<Cell<u32>>,
}

impl FastCallbackState {
    /// Return whether execution is currently inside at least one fast callback.
    #[must_use]
    pub fn in_fast_callback(&self) -> bool {
        self.depth.get() != 0
    }

    /// Enter a fast callback until the returned guard is dropped.
    #[must_use]
    pub fn enter(&self) -> FastCallbackGuard {
        self.depth.set(self.depth.get().saturating_add(1));
        FastCallbackGuard {
            state: self.clone(),
        }
    }

    /// Raise the upstream E5560 error for a disallowed operation.
    ///
    /// # Errors
    ///
    /// Returns error E5560 when called from inside a fast callback.
    pub fn guard(&self, operation: &str) -> mlua::Result<()> {
        if self.in_fast_callback() {
            Err(mlua::Error::runtime(format!(
                "E5560: vimL function must not be called in a lua loop callback: {operation}"
            )))
        } else {
            Ok(())
        }
    }
}

/// Scope guard returned by [`FastCallbackState::enter`].
pub struct FastCallbackGuard {
    state: FastCallbackState,
}

impl Drop for FastCallbackGuard {
    fn drop(&mut self) {
        self.state
            .depth
            .set(self.state.depth.get().saturating_sub(1));
    }
}

#[derive(Clone, Copy)]
struct NilSentinel;

impl UserData for NilSentinel {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("vim.NIL"));
        methods.add_meta_method(MetaMethod::Index, |_, _, _: Value| -> mlua::Result<Value> {
            Err(mlua::Error::runtime("attempt to index vim.NIL"))
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, _: (Value, Value)| -> mlua::Result<()> {
                Err(mlua::Error::runtime("attempt to index vim.NIL"))
            },
        );
    }
}

/// Install the C-owned fields of the global `vim` table.
///
/// # Errors
///
/// Returns an error if Lua cannot create or register any of the tables,
/// userdata, or functions that make up the global `vim` table.
pub fn install_vim_core(
    lua: &Lua,
    builtins: Rc<dyn BuiltinHost>,
    scheduler: Rc<dyn Scheduler>,
) -> mlua::Result<FastCallbackState> {
    let vim = lua.create_table()?;
    // executor.c:nlua_common_vim_init: vim.is_thread and the vim._core table
    // the Lua prelude (vim._init_packages) attaches editor hooks to. This host
    // only creates main-thread states, so is_thread reports false.
    vim.set("is_thread", lua.create_function(|_, ()| Ok(false))?)?;
    vim.set("_core", lua.create_table()?)?;
    vim.set("NIL", lua.create_userdata(NilSentinel)?)?;

    let empty_dict_mt = lua.create_table()?;
    empty_dict_mt.set(
        "__tostring",
        lua.create_function(|_, ()| Ok("vim.empty_dict()"))?,
    )?;
    vim.set("_empty_dict_mt", empty_dict_mt)?;

    let state = FastCallbackState::default();
    let state_for_lua = state.clone();
    vim.set(
        "in_fast_event",
        lua.create_function(move |_, ()| Ok(state_for_lua.in_fast_callback()))?,
    )?;

    install_builtin_functions(lua, &vim, builtins, state.clone())?;
    install_schedule(lua, &vim, scheduler)?;
    vim.set("api", lua.create_table()?)?;
    lua.globals().set("vim", vim)?;
    Ok(state)
}

/// Lua wrapper factory for Rust natives that signal failure as
/// `(false, message)`: the wrapper re-raises the message as a *string* error,
/// so `pcall` never observes an mlua `WrappedFailure` userdata (upstream
/// raises plain strings; userdata returns make the exec_lua harness reject
/// with "cannot be serialized over RPC"). Multi-value safe: success returns
/// pass every value after the flag through untouched.
///
/// # Errors
///
/// Returns the chunk compilation/registration error.
pub fn error_shim(lua: &Lua) -> mlua::Result<Function> {
    lua.load(
        "return function(native) \
           return function(...) \
             local results = { native(...) } \
             if results[1] == false then error(results[2], 2) end \
             return unpack(results, 2) \
           end \
         end",
    )
    .eval()
}

fn install_builtin_functions(
    lua: &Lua,
    vim: &Table,
    builtins: Rc<dyn BuiltinHost>,
    fast_state: FastCallbackState,
) -> mlua::Result<()> {
    let wrap_builtin: Function = lua
        .load(
            "return function(native) \
               return function(...) \
                 local ok, value = native(...) \
                 if not ok then error(value, 2) end \
                 return value \
               end \
             end",
        )
        .eval()?;

    let call_host = builtins.clone();
    let call_state = fast_state.clone();
    let native_call = lua.create_function(
        move |lua, (name, args): (mlua::LuaString, Variadic<Value>)| {
            dispatch_builtin(
                lua,
                call_host.as_ref(),
                &call_state,
                &name.as_bytes(),
                args.as_slice(),
            )
        },
    )?;
    let call: Function = wrap_builtin.call(native_call)?;
    vim.set("call", call)?;

    let fn_table = lua.create_table()?;
    let fn_metatable = lua.create_table()?;
    fn_metatable.set(
        "__index",
        lua.create_function(move |lua, (_table, name): (Table, mlua::LuaString)| {
            let host = builtins.clone();
            let state = fast_state.clone();
            let name = OxStr(name.as_bytes().to_vec());
            let native = lua.create_function(move |lua, args: Variadic<Value>| {
                dispatch_builtin(lua, host.as_ref(), &state, name.as_bytes(), args.as_slice())
            })?;
            wrap_builtin.call::<Function>(native)
        })?,
    )?;
    fn_table.set_metatable(Some(fn_metatable))?;
    vim.set("fn", fn_table)
}

fn dispatch_builtin(
    lua: &Lua,
    host: &dyn BuiltinHost,
    fast_state: &FastCallbackState,
    name: &[u8],
    args: &[Value],
) -> mlua::Result<(bool, Value)> {
    let name = OxStr(name.to_vec());
    if fast_state.in_fast_callback()
        && !host.is_fast(&name)
        && let Err(error) = fast_state.guard(&format!(
            "Vimscript function \"{}\"",
            name.to_string_lossy()
        ))
    {
        return api_failure(lua, error.to_string());
    }

    let mut converted = Vec::with_capacity(args.len());
    let mut arg_refs = Vec::new();
    for value in args {
        match lua_to_typval(lua, value) {
            Ok(value) => {
                collect_typval_refs(&value, &mut arg_refs);
                converted.push(value);
            }
            Err(error) => {
                free_typval_refs(lua, &arg_refs);
                return api_failure(lua, error.to_string());
            }
        }
    }

    let result = host.call(&name, converted);
    // executor.c:nlua_call frees the argument LuaRefs once the call returns.
    free_typval_refs(lua, &arg_refs);
    match result {
        Ok(value) => match typval_to_lua(lua, &value) {
            Ok(value) => Ok((true, value)),
            Err(error) => api_failure(lua, error.to_string()),
        },
        Err(error) => api_failure(lua, error),
    }
}

fn install_schedule(lua: &Lua, vim: &Table, scheduler: Rc<dyn Scheduler>) -> mlua::Result<()> {
    vim.set(
        "schedule",
        lua.create_function(move |lua, callback: Function| {
            let callback = callback.clone();
            let lua = lua.clone();
            scheduler
                .schedule_deferred(Box::new(move || {
                    call_with_traceback(&lua, &callback, MultiValue::new()).map(|_| ())
                }))
                .map_err(mlua::Error::runtime)
        })?,
    )
}

/// Install the native variable functions used by the runtime's magic tables.
///
/// # Errors
///
/// Returns an error if the global `vim` table is unavailable or Lua cannot
/// create or install either native variable function.
pub fn bind_variables(lua: &Lua, host: Rc<dyn VariableHost>) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let get_host = host.clone();
    vim.set(
        "_getvar",
        lua.create_function(
            move |lua, (scope, handle, name): (mlua::LuaString, i64, mlua::LuaString)| {
                let scope = VariableScope::parse(&scope.as_bytes())?;
                let name = OxStr(name.as_bytes().to_vec());
                match get_host
                    .get_var(scope, handle, &name)
                    .map_err(mlua::Error::runtime)?
                {
                    Some(value) => object_to_lua(lua, &value).map_err(mlua::Error::external),
                    None => Ok(Value::Nil),
                }
            },
        )?,
    )?;

    vim.set(
        "_setvar",
        lua.create_function(
            move |lua, (scope, handle, name, value): (mlua::LuaString, i64, mlua::LuaString, Value)| {
                let scope = VariableScope::parse(&scope.as_bytes())?;
                let name = OxStr(name.as_bytes().to_vec());
                let value = if value.is_nil() {
                    None
                } else {
                    Some(lua_to_object(lua, &value).map_err(mlua::Error::external)?)
                };
                host.set_var(scope, handle, name, value).map_err(mlua::Error::runtime)
            },
        )?,
    )
}
fn api_failure(lua: &Lua, error: String) -> mlua::Result<(bool, Value)> {
    Ok((false, Value::String(lua.create_string(error)?)))
}

/// Populate `vim.api` from the concrete API registry.
///
/// # Errors
///
/// Returns an error if `vim.api` is unavailable, the Lua wrapper cannot be
/// compiled or evaluated, or Lua cannot create, wrap, or install an API
/// function.
#[expect(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "one registry walk installs the public API and its internal runtime companion"
)]
pub fn bind_api(
    lua: &Lua,
    registry: &Registry,
    context: ApiDispatchContext,
    fast_state: FastCallbackState,
) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let api: Table = vim.get("api")?;
    let wrap_api: Function = lua
        .load(
            "local function pack(...) return { n = select('#', ...), ... } end \
             return function(native) \
               return function(...) \
                 local values = pack(native(...)) \
                 if not values[1] then error(values[2], 2) end \
                 return unpack(values, 2, values.n) \
               end \
             end",
        )
        .eval()?;

    for (metadata, dispatch) in registry.iter() {
        let name = metadata.name;
        let fast = metadata.fast;
        let textlock = metadata.textlock;
        let params = metadata.params;
        let legacy_floats = metadata.since < 11;
        let state = fast_state.clone();
        let context = context.clone();
        let native = lua.create_function(move |lua, args: Variadic<Value>| {
            let result = (|| -> Result<Vec<Value>, String> {
                if state.in_fast_callback() && !fast {
                    state.guard(name).map_err(|error| match error {
                        mlua::Error::RuntimeError(message) => message,
                        error => error.to_string(),
                    })?;
                }
                if textlock && context.text_locked() {
                    return Err("E565: Not allowed to change text or change window".to_owned());
                }
                let mut args = args
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        if params
                            .get(index)
                            .is_some_and(|(_, kind, _)| *kind == ox_api::TypeRef::Boolean)
                        {
                            return Ok(Object::Boolean(!matches!(
                                value,
                                Value::Nil | Value::Boolean(false)
                            )));
                        }
                        lua_to_object(lua, value).map_err(|error| error.to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                // executor.c nlua_api_call: a trailing optional Dict the
                // caller left off arrives as an empty one, which is what lets
                // `vim.cmd(...)` reach `nvim_exec2(src)` with no opts.
                while args.len() < params.len() {
                    let (_, kind, optional) = params[args.len()];
                    if !optional || kind != ox_api::TypeRef::Dict {
                        break;
                    }
                    args.push(Object::Dict(ox_types::Dict(Vec::new())));
                }
                let result = dispatch(context.session(), &args)
                    .map_err(|error| error.message().to_owned())?;
                let values = match &result {
                    Object::Array(values) if matches!(name, "nvim_buf_call" | "nvim_win_call") => {
                        values
                            .iter()
                            .map(|value| {
                                let convert = if legacy_floats {
                                    object_to_lua_legacy
                                } else {
                                    object_to_lua
                                };
                                convert(lua, value).map_err(|error| error.to_string())
                            })
                            .collect::<Result<Vec<_>, _>>()?
                    }
                    value => {
                        let convert = if legacy_floats {
                            object_to_lua_legacy
                        } else {
                            object_to_lua
                        };
                        vec![convert(lua, value).map_err(|error| error.to_string())?]
                    }
                };
                match &result {
                    Object::LuaRef(reference) => {
                        let _ = free_lua_ref(lua, *reference);
                    }
                    Object::Array(values) => {
                        for value in values {
                            if let Object::LuaRef(reference) = value {
                                let _ = free_lua_ref(lua, *reference);
                            }
                        }
                    }
                    _ => {}
                }
                Ok(values)
            })();
            let values = match result {
                Ok(values) => {
                    let mut returned = Vec::with_capacity(values.len() + 1);
                    returned.push(Value::Boolean(true));
                    returned.extend(values);
                    returned
                }
                Err(error) => vec![
                    Value::Boolean(false),
                    Value::String(lua.create_string(error)?),
                ],
            };
            Ok(MultiValue::from_vec(values))
        })?;
        let binding: Function = wrap_api.call(native)?;
        api.set(name, binding)?;
    }

    // api/vim.c `nvim__get_runtime` is an internal, so it is absent from the
    // canonical API metadata the registry is built from, but the package loader
    // in runtime/lua/vim/_init_packages.lua reaches 'runtimepath' through it.
    // Bind it here, where the editor whose 'runtimepath' it must walk is in
    // scope. Upstream `runtime_get_named` defaults a missing `is_lua` to false.
    let native_runtime = lua.create_function(move |lua, args: Variadic<Value>| {
        let values: Vec<Value> = args.into();
        let result =
            <(Vec<String>, bool, Table)>::from_lua_multi(MultiValue::from_vec(values), lua)
                .map_err(|error| error.to_string())
                .and_then(|(patterns, all, opts)| {
                    let is_lua = opts
                        .get::<Option<bool>>("is_lua")
                        .map_err(|error| error.to_string())?
                        .unwrap_or(false);
                    let paths =
                        ox_api::runtime_get_named(context.session(), &patterns, all, is_lua)
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect::<Vec<_>>();
                    lua.create_sequence_from(paths)
                        .map(Value::Table)
                        .map_err(|error| error.to_string())
                });
        match result {
            Ok(value) => Ok((true, value)),
            Err(error) => api_failure(lua, error),
        }
    })?;
    let runtime_binding: Function = wrap_api.call(native_runtime)?;
    api.set("nvim__get_runtime", runtime_binding)?;
    // Ex-to-Lua calls replace `vim.api` temporarily, and user code can replace
    // `tostring`, so both lookups must happen when `print` runs.
    lua.globals().set(
        "print",
        lua.create_function(|lua, args: Variadic<Value>| {
            let to_string: Function = lua.globals().get("tostring")?;
            let mut bytes = Vec::new();
            for (index, value) in args.into_iter().enumerate() {
                if index > 0 {
                    bytes.push(b' ');
                }
                let rendered: LuaString = to_string.call(value)?;
                bytes.extend_from_slice(&rendered.as_bytes());
            }
            let vim: Table = lua.globals().get("vim")?;
            let api: Table = vim.get("api")?;
            let out_write: Function = api.get("nvim_out_write")?;
            out_write.call::<()>(lua.create_string(&bytes)?)?;
            Ok(())
        })?,
    )?;
    Ok(())
}

/// Call a Lua function through `xpcall` with a handler that keeps mlua's
/// typed callback errors intact and renders ordinary raised Lua values the
/// way Neovim does: protected `tostring`, one traceback, and
/// `[UNPRINTABLE ERROR]` when the value cannot be rendered.
///
/// mlua's built-in traceback handler stringifies the error unprotected, so a
/// raising `__tostring` would lose the original error to `LUA_ERRERR`. This
/// handler passes typed mlua errors through unchanged (their traceback is
/// captured when the Rust callback fails) and renders ordinary values safely.
///
/// # Errors
///
/// Returns the function error with its typed cause and traceback preserved.
pub fn call_with_traceback(
    lua: &Lua,
    function: &Function,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let debug: Table = lua.globals().get("debug")?;
    let traceback: Function = debug.get("traceback")?;
    let to_string: Function = lua.globals().get("tostring")?;
    let xpcall: Function = lua.globals().get("xpcall")?;
    let handler = lua.create_function(move |lua, value: Value| -> mlua::Result<Value> {
        if matches!(value, Value::Error(_)) {
            return Ok(value);
        }
        let text = nlua_error_text(&value, &to_string);
        let with_traceback: String = traceback.call((text, 0))?;
        Ok(Value::String(lua.create_string(with_traceback)?))
    })?;

    let mut xpcall_args = args;
    xpcall_args.reserve(2);
    xpcall_args.push_front(Value::Function(handler));
    xpcall_args.push_front(Value::Function(function.clone()));
    let mut results: MultiValue = xpcall.call(xpcall_args)?;
    match results.pop_front() {
        Some(Value::Boolean(true)) => Ok(results),
        Some(Value::Boolean(false)) => {
            let error = results.pop_front().unwrap_or(Value::Nil);
            Err(match error {
                Value::Error(err) => *err,
                Value::String(message) => mlua::Error::RuntimeError(
                    String::from_utf8_lossy(&message.as_bytes()).into_owned(),
                ),
                _ => mlua::Error::runtime("[UNPRINTABLE ERROR]"),
            })
        }
        _ => Err(mlua::Error::runtime("xpcall returned no status")),
    }
}

/// Render an ordinary raised Lua value following Neovim's `nlua_get_error`:
/// strings verbatim, protected Lua `tostring`, and `[UNPRINTABLE ERROR]` when
/// conversion fails or does not return a string.
fn nlua_error_text(value: &Value, to_string: &Function) -> String {
    match value {
        Value::String(s) => String::from_utf8_lossy(&s.as_bytes()).into_owned(),
        other => match to_string.call::<Value>(other.clone()) {
            Ok(Value::String(s)) => String::from_utf8_lossy(&s.as_bytes()).into_owned(),
            _ => "[UNPRINTABLE ERROR]".to_string(),
        },
    }
}

/// Render an [`mlua::Error`] for the host layer: `RuntimeError` text
/// verbatim, `Display` for every other variant (which owns labels and
/// merged tracebacks for its wrapped causes).
pub(crate) fn mlua_error_text(error: &mlua::Error) -> String {
    match error {
        mlua::Error::RuntimeError(message) => message.clone(),
        error => error.to_string(),
    }
}
