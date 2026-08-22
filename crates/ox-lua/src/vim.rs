//! C-side core of the global `vim` Lua table.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mlua::{
    Function, Lua, MetaMethod, MultiValue, Table, UserData, UserDataMethods, Value, Variadic,
};
use ox_api::Registry;
use ox_editor::Editor;
use ox_types::{OxStr, Typval};

use crate::converter::{lua_to_object, object_to_lua};
use crate::typval_bridge::{lua_to_typval, typval_to_lua};

/// A deferred Lua callback owned by the eventual main-loop adapter.
pub type Work = Box<dyn FnOnce() -> mlua::Result<()> + 'static>;

/// Main-loop scheduling seam used by `vim.schedule`.
pub trait Scheduler {
    /// Enqueue work for a later normal-event-loop turn.
    fn schedule_deferred(&self, work: Work) -> Result<(), String>;
}

/// Vimscript builtin dispatch seam used by `vim.call` and `vim.fn`.
pub trait BuiltinHost {
    /// Invoke a named Vimscript function with converted arguments.
    fn call(&self, name: &OxStr, args: Vec<Typval>) -> Result<Typval, String>;

    /// Whether this function is safe in a fast callback.
    fn is_fast(&self, _name: &OxStr) -> bool {
        false
    }
}

/// Shared editor context captured by the generated `vim.api` Lua closures.
#[derive(Clone)]
pub struct ApiDispatchContext {
    editor: Rc<RefCell<Editor>>,
    textlock_depth: Rc<Cell<u32>>,
}

impl ApiDispatchContext {
    /// Create a dispatch context for one editor instance.
    #[must_use]
    pub fn new(editor: Rc<RefCell<Editor>>) -> Self {
        Self { editor, textlock_depth: Rc::new(Cell::new(0)) }
    }

    /// Enter textlock until the returned guard is dropped.
    #[must_use]
    pub fn enter_textlock(&self) -> TextlockGuard {
        self.textlock_depth.set(self.textlock_depth.get().saturating_add(1));
        TextlockGuard { depth: self.textlock_depth.clone() }
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
        FastCallbackGuard { state: self.clone() }
    }

    /// Raise the upstream E5560 error for a disallowed operation.
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
        self.state.depth.set(self.state.depth.get().saturating_sub(1));
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

fn install_builtin_functions(
    lua: &Lua,
    vim: &Table,
    builtins: Rc<dyn BuiltinHost>,
    fast_state: FastCallbackState,
) -> mlua::Result<()> {
    let call_host = builtins.clone();
    let call_state = fast_state.clone();
    vim.set(
        "call",
        lua.create_function(move |lua, (name, args): (mlua::LuaString, Variadic<Value>)| {
            dispatch_builtin(
                lua,
                call_host.as_ref(),
                &call_state,
                &name.as_bytes(),
                args.as_slice(),
            )
        })?,
    )?;

    let fn_table = lua.create_table()?;
    let fn_metatable = lua.create_table()?;
    fn_metatable.set(
        "__index",
        lua.create_function(move |lua, (_table, name): (Table, mlua::LuaString)| {
            let host = builtins.clone();
            let state = fast_state.clone();
            let name = OxStr(name.as_bytes().to_vec());
            lua.create_function(move |lua, args: Variadic<Value>| {
                dispatch_builtin(
                    lua,
                    host.as_ref(),
                    &state,
                    name.as_bytes(),
                    args.as_slice(),
                )
            })
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
) -> mlua::Result<Value> {
    let name = OxStr(name.to_vec());
    if fast_state.in_fast_callback() && !host.is_fast(&name) {
        fast_state.guard(&format!("Vimscript function \"{}\"", name.to_string_lossy()))?;
    }
    let converted = args
        .iter()
        .map(|value| lua_to_typval(lua, value).map_err(mlua::Error::external))
        .collect::<Result<Vec<_>, _>>()?;
    let result = host.call(&name, converted).map_err(mlua::Error::runtime)?;
    typval_to_lua(lua, &result).map_err(mlua::Error::external)
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

/// Populate `vim.api` from the concrete API registry.
pub fn bind_api(
    lua: &Lua,
    registry: &Registry,
    context: ApiDispatchContext,
    fast_state: FastCallbackState,
) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let api: Table = vim.get("api")?;

    for (metadata, dispatch) in registry.iter() {
        let name = metadata.name;
        let fast = metadata.fast;
        let textlock = metadata.textlock;
        let state = fast_state.clone();
        let context = context.clone();
        api.set(
            name,
            lua.create_function(move |lua, args: Variadic<Value>| {
                if state.in_fast_callback() && !fast {
                    state.guard(name)?;
                }
                if textlock && context.text_locked() {
                    return Err(mlua::Error::runtime(
                        "E565: Not allowed to change text or change window",
                    ));
                }
                let args = args
                    .iter()
                    .map(|value| lua_to_object(lua, value).map_err(mlua::Error::external))
                    .collect::<Result<Vec<_>, _>>()?;
                let result = dispatch(&mut context.editor.borrow_mut(), &args)
                    .map_err(mlua::Error::external)?;
                object_to_lua(lua, &result).map_err(mlua::Error::external)
            })?,
        )?;
    }
    Ok(())
}

/// Call a Lua function through `xpcall(..., debug.traceback)`.
pub fn call_with_traceback(
    lua: &Lua,
    function: &Function,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let debug: Table = lua.globals().get("debug")?;
    let traceback: Function = debug.get("traceback")?;
    let xpcall: Function = lua.globals().get("xpcall")?;
    let function = function.clone();
    let wrapper = lua.create_function(move |_, ()| function.call::<MultiValue>(args.clone()))?;

    let xpcall_args = MultiValue::from_vec(vec![
        Value::Function(wrapper),
        Value::Function(traceback),
    ]);
    let mut results: MultiValue = xpcall.call(xpcall_args)?;
    match results.pop_front() {
        Some(Value::Boolean(true)) => Ok(results),
        Some(Value::Boolean(false)) => {
            let error = results.pop_front().unwrap_or(Value::Nil);
            Err(mlua::Error::runtime(lua_error_text(error)))
        }
        _ => Err(mlua::Error::runtime("xpcall returned no status")),
    }
}

fn lua_error_text(value: Value) -> String {
    match value {
        Value::String(value) => String::from_utf8_lossy(&value.as_bytes()).into_owned(),
        other => format!("{other:?}"),
    }
}
