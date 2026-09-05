//! Behavioral contract tests for the Lua host core.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{ErrorContext, Function, MultiValue, Value};
use ox_editor::{Editor, Geometry};
use ox_lua::{
    ApiDispatchContext, BuiltinHost, CONVERSION_RECURSION_LIMIT, ConversionError, ExecError,
    LuaHost, RuntimeRoot, Scheduler, Work, bind_api, call_with_traceback, lua_to_object,
    lua_to_typval, object_to_lua, typval_to_lua,
};
use ox_types::{ApiError, BufHandle, Dict, Object, OxStr, Special, TabHandle, Typval, WinHandle};

struct TestUserdata(i64);

impl mlua::UserData for TestUserdata {}

#[derive(Default)]
struct FakeScheduler {
    queue: RefCell<VecDeque<Work>>,
}

impl FakeScheduler {
    fn drain(&self) -> mlua::Result<()> {
        while let Some(work) = self.queue.borrow_mut().pop_front() {
            work()?;
        }
        Ok(())
    }
}

impl Scheduler for FakeScheduler {
    fn schedule_deferred(&self, work: Work) -> Result<(), String> {
        self.queue.borrow_mut().push_back(work);
        Ok(())
    }
}

#[derive(Default)]
struct FakeBuiltins {
    calls: RefCell<Vec<(OxStr, Vec<Typval>)>>,
}

impl BuiltinHost for FakeBuiltins {
    fn call(&self, name: &OxStr, args: Vec<Typval>) -> Result<Typval, String> {
        self.calls.borrow_mut().push((name.clone(), args));
        Ok(Typval::String(OxStr::from("called")))
    }
}

fn runtime_root() -> RuntimeRoot {
    RuntimeRoot::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime"))
}

fn host() -> (LuaHost, Rc<FakeBuiltins>, Rc<FakeScheduler>) {
    let builtins = Rc::new(FakeBuiltins::default());
    let scheduler = Rc::new(FakeScheduler::default());
    let host = LuaHost::new(runtime_root(), builtins.clone(), scheduler.clone()).unwrap();
    (host, builtins, scheduler)
}

#[test]
fn opens_upstream_luajit_library_set_and_runtime_path() {
    let (host, _, _) = host();
    let globals = host.lua().globals();
    for name in [
        "coroutine",
        "table",
        "io",
        "os",
        "string",
        "math",
        "package",
        "debug",
        "bit",
        "jit",
    ] {
        assert!(
            !matches!(globals.get::<Value>(name).unwrap(), Value::Nil),
            "missing {name}"
        );
    }
    assert!(matches!(globals.get::<Value>("ffi").unwrap(), Value::Nil));

    let package: mlua::Table = globals.get("package").unwrap();
    let path: String = package.get("path").unwrap();
    assert!(path.contains("runtime/lua/?.lua"));
    assert_eq!(
        host.runtime_root().runtime_entries("lua").as_slice(),
        &[host.runtime_root().resolve("lua")]
    );
}

#[test]
fn ffi_is_preloaded_and_requireable_without_global() {
    let (host, _, _) = host();
    let globals = host.lua().globals();
    assert!(matches!(globals.get::<Value>("ffi").unwrap(), Value::Nil));
    let ffi: Value = host.lua().load("return require('ffi')").eval().unwrap();
    assert!(
        matches!(ffi, Value::Table(_)),
        "require('ffi') should return the ffi module table"
    );
    assert!(
        matches!(globals.get::<Value>("ffi").unwrap(), Value::Nil),
        "ffi must not be a global after require"
    );
}

#[test]
fn prelude_merges_shared_functions_into_vim_table() {
    let (host, _, _) = host();
    let lua = host.lua();
    // executor.c:nlua_init_packages tail: require('vim._init_packages') ran
    // during host init, so vim._core.shared's surface is live on the global
    // vim table, and the vim._core.editor assembly followed it.
    let loaded: bool = lua
        .load("return package.loaded['vim._init_packages'] ~= nil")
        .eval()
        .unwrap();
    assert!(
        loaded,
        "vim._init_packages should have been required during init"
    );
    let shared_surface: bool = lua
        .load(
            "return vim.startswith('abc', 'a') \
             and vim.endswith('abc', 'c') \
             and vim.split('a,b,c', ',')[2] == 'b' \
             and vim.tbl_isempty({}) \
             and vim.tbl_contains({ 'x' }, 'x') \
             and vim.deepcopy({ 1, { 2 } })[2][1] == 2",
        )
        .eval()
        .unwrap();
    assert!(
        shared_surface,
        "vim._core.shared functions missing after init"
    );
    let editor_assembly: bool = lua
        .load(
            "return type(vim.wait) == 'function' \
             and type(vim.schedule_wrap) == 'function' \
             and type(vim.fn) == 'table' \
             and type(vim.cmd) == 'table' \
             and type(vim.o) == 'table' \
             and vim.is_thread() == false \
             and type(vim._core) == 'table'",
        )
        .eval()
        .unwrap();
    assert!(
        editor_assembly,
        "vim._core.editor assembly missing after init"
    );
}

#[test]
fn object_converter_covers_scalars_containers_bytes_and_handles() {
    let (host, _, _) = host();
    let lua = host.lua();
    let values = vec![
        Object::Nil,
        Object::Boolean(true),
        Object::Integer(42),
        Object::Float(1.5),
        Object::String(OxStr(vec![0, 0xff, b'x'])),
        Object::Array(vec![Object::Integer(1), Object::Integer(2)]),
        Object::Dict(Dict(vec![(OxStr::from("key"), Object::Boolean(false))])),
        Object::Dict(Dict(Vec::new())),
    ];

    for expected in values {
        let lua_value = object_to_lua(lua, &expected).unwrap();
        assert_eq!(lua_to_object(lua, &lua_value).unwrap(), expected);
    }

    for object in [
        Object::Buffer(BufHandle::try_from(3).unwrap()),
        Object::Window(WinHandle::try_from(4).unwrap()),
        Object::Tabpage(TabHandle::try_from(5).unwrap()),
    ] {
        let lua_value = object_to_lua(lua, &object).unwrap();
        let expected = match object {
            Object::Buffer(value) => i64::from(value),
            Object::Window(value) => i64::from(value),
            Object::Tabpage(value) => i64::from(value),
            _ => unreachable!(),
        };
        assert_eq!(
            lua_to_object(lua, &lua_value).unwrap(),
            Object::Integer(expected)
        );
    }
}

#[test]
fn empty_table_and_empty_dict_metatable_remain_distinct() {
    let (host, _, _) = host();
    let lua = host.lua();
    let plain = lua.create_table().unwrap();
    assert_eq!(
        lua_to_object(lua, &Value::Table(plain)).unwrap(),
        Object::Array(Vec::new())
    );

    let dictionary = object_to_lua(lua, &Object::Dict(Dict(Vec::new()))).unwrap();
    let Value::Table(dictionary) = dictionary else {
        unreachable!()
    };
    let vim: mlua::Table = lua.globals().get("vim").unwrap();
    let marker: mlua::Table = vim.get("_empty_dict_mt").unwrap();
    assert_eq!(
        dictionary.metatable().unwrap().to_pointer(),
        marker.to_pointer()
    );
    assert_eq!(
        lua_to_object(lua, &Value::Table(dictionary)).unwrap(),
        Object::Dict(Dict(Vec::new()))
    );
}

#[test]
fn sparse_numeric_tables_fill_holes_with_api_nil() {
    let (host, _, _) = host();
    let table = host.lua().create_table().unwrap();
    table.raw_set(3, "last").unwrap();
    assert_eq!(
        lua_to_object(host.lua(), &Value::Table(table)).unwrap(),
        Object::Array(vec![
            Object::Nil,
            Object::Nil,
            Object::String(OxStr::from("last")),
        ])
    );
}

#[test]
fn lua_refs_round_trip_functions_and_userdata() {
    let (host, _, _) = host();
    let lua = host.lua();
    let function: Function = lua
        .load("return function(x) return x + 1 end")
        .eval()
        .unwrap();
    let object = lua_to_object(lua, &Value::Function(function)).unwrap();
    let Object::LuaRef(reference) = object else {
        unreachable!()
    };
    let Value::Function(function) = object_to_lua(lua, &Object::LuaRef(reference)).unwrap() else {
        unreachable!()
    };
    assert_eq!(function.call::<i64>(4).unwrap(), 5);

    let userdata = lua.create_userdata(TestUserdata(17)).unwrap();
    let object = lua_to_object(lua, &Value::UserData(userdata.clone())).unwrap();
    let Object::LuaRef(reference) = object else {
        unreachable!()
    };
    let Value::UserData(round_trip) = object_to_lua(lua, &Object::LuaRef(reference)).unwrap()
    else {
        unreachable!()
    };
    assert_eq!(round_trip.borrow::<TestUserdata>().unwrap().0, 17);
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn numeric_conversion_follows_luajit_double_precision() {
    let (host, _, _) = host();
    let lua = host.lua();
    let original = Object::Integer(i64::MAX);
    let lua_value = object_to_lua(lua, &original).unwrap();
    assert!(matches!(lua_value, Value::Number(_)));
    assert_eq!(
        lua_to_object(lua, &lua_value).unwrap(),
        Object::Float(i64::MAX as f64)
    );

    let exact_large = Value::Number(9_007_199_254_740_994.0);
    assert_eq!(
        lua_to_object(lua, &exact_large).unwrap(),
        Object::Integer(9_007_199_254_740_994)
    );
}

#[test]
fn conversion_depth_is_typed_and_bounded() {
    let (host, _, _) = host();
    let source = format!(
        "local root={{}}; local current=root; for _=1,{} do local next={{}}; current[1]=next; current=next end; return root",
        CONVERSION_RECURSION_LIMIT + 2
    );
    let value: Value = host.lua().load(&source).eval().unwrap();
    assert!(matches!(
        lua_to_object(host.lua(), &value),
        Err(ConversionError::RecursionLimit {
            limit: CONVERSION_RECURSION_LIMIT
        })
    ));
}

#[test]
fn vim_nil_maps_to_api_nil_and_vimscript_null() {
    let (host, _, _) = host();
    let lua = host.lua();
    // API dispatch path (kNluaPushSpecial): Object::Nil surfaces as Lua nil,
    // not vim.NIL userdata, matching upstream nlua_push_Object.
    let nil = object_to_lua(lua, &Object::Nil).unwrap();
    assert!(
        matches!(nil, Value::Nil),
        "Object::Nil should map to Value::Nil"
    );
    assert_eq!(lua_to_object(lua, &nil).unwrap(), Object::Nil);
    assert_eq!(
        lua_to_typval(lua, &nil).unwrap(),
        Typval::Special(Special::Null)
    );
    // Vimscript bridge path (no kNluaPushSpecial): v:null remains vim.NIL userdata.
    let pushed_null = typval_to_lua(lua, &Typval::Special(Special::Null)).unwrap();
    assert_eq!(lua_to_object(lua, &pushed_null).unwrap(), Object::Nil);
    assert!(matches!(pushed_null, Value::UserData(_)));
    assert_eq!(
        lua.load("return tostring(vim.NIL)")
            .eval::<String>()
            .unwrap(),
        "vim.NIL"
    );
}

#[test]
fn void_api_return_is_lua_nil_and_vim_nil_round_trips() {
    let (host, _, _) = host();
    let lua = host.lua();
    // Void API return: Object::Nil → Lua nil (not vim.NIL userdata).
    let void = object_to_lua(lua, &Object::Nil).unwrap();
    assert!(matches!(void, Value::Nil));
    // Lua `== nil` check succeeds, mirroring buffer_spec T7 pattern.
    let check: bool = lua
        .load("return select(1, ...) == nil")
        .call::<bool>(void)
        .unwrap();
    assert!(check);
    // Explicit vim.NIL from Lua round-trips back to Object::Nil unchanged.
    let explicit_nil: Value = lua.load("return vim.NIL").eval().unwrap();
    assert!(matches!(explicit_nil, Value::UserData(_)));
    assert_eq!(lua_to_object(lua, &explicit_nil).unwrap(), Object::Nil);
}

#[test]
fn typval_bridge_maps_lists_dicts_funcrefs_and_blobs() {
    let (host, _, _) = host();
    let lua = host.lua();
    let value = Typval::dict(vec![
        (
            OxStr::from("list"),
            Typval::list(vec![Typval::Number(1), Typval::Bool(true)]),
        ),
        (OxStr::from("empty"), Typval::dict(Vec::new())),
    ]);
    let lua_value = typval_to_lua(lua, &value).unwrap();
    assert_eq!(lua_to_typval(lua, &lua_value).unwrap(), value);

    let blob = Typval::Blob(vec![0, 0xff]);
    let blob_lua = typval_to_lua(lua, &blob).unwrap();
    assert_eq!(
        lua_to_typval(lua, &blob_lua).unwrap(),
        Typval::String(OxStr(vec![0, 0xff]))
    );

    let function: Value = lua.load("return function() return 7 end").eval().unwrap();
    let funcref = lua_to_typval(lua, &function).unwrap();
    let round_trip = typval_to_lua(lua, &funcref).unwrap();
    let Value::Function(round_trip) = round_trip else {
        unreachable!()
    };
    assert_eq!(round_trip.call::<i64>(()).unwrap(), 7);
}

#[test]
fn typval_bridge_preserves_null_entries_and_recursive_identity() {
    let (host, _, _) = host();
    let lua = host.lua();
    let list = Typval::list(vec![Typval::Number(1), Typval::Special(Special::Null)]);
    let Value::Table(table) = typval_to_lua(lua, &list).unwrap() else {
        unreachable!()
    };
    assert_eq!(table.raw_len(), 2);
    assert_eq!(
        lua_to_object(lua, &table.raw_get::<Value>(2).unwrap()).unwrap(),
        Object::Nil
    );

    let recursive = Typval::list(Vec::new());
    let Typval::List(items) = &recursive else {
        unreachable!()
    };
    items.borrow_mut().items.push(recursive.clone());
    let Value::Table(recursive_lua) = typval_to_lua(lua, &recursive).unwrap() else {
        unreachable!()
    };
    let child: mlua::Table = recursive_lua.raw_get(1).unwrap();
    assert_eq!(child.to_pointer(), recursive_lua.to_pointer());

    let lua_cycle = lua.create_table().unwrap();
    lua_cycle.raw_set(1, lua_cycle.clone()).unwrap();
    let converted = lua_to_typval(lua, &Value::Table(lua_cycle)).unwrap();
    let Typval::List(root) = &converted else {
        unreachable!()
    };
    let Typval::List(child) = root.borrow().items[0].clone() else {
        unreachable!()
    };
    assert!(Rc::ptr_eq(root, &child));
}

#[test]
fn vim_call_and_fn_dispatch_through_builtin_host() {
    let (host, builtins, _) = host();
    assert_eq!(
        host.lua()
            .load("return vim.call('Record', 3, 'x')")
            .eval::<String>()
            .unwrap(),
        "called"
    );
    assert_eq!(
        host.lua()
            .load("return vim.fn.Other(4)")
            .eval::<String>()
            .unwrap(),
        "called"
    );
    let calls = builtins.calls.borrow();
    // Host init runs the runtime prelude, which probes has('win32') exactly
    // once (runtime/lua/vim/_core/system.lua).
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].0, OxStr::from("has"));
    assert_eq!(calls[0].1, vec![Typval::String(OxStr::from("win32"))]);
    assert_eq!(calls[1].0, OxStr::from("Record"));
    assert_eq!(
        calls[1].1,
        vec![Typval::Number(3), Typval::String(OxStr::from("x"))]
    );
    assert_eq!(calls[2].0, OxStr::from("Other"));
}

#[test]
fn global_print_uses_current_bindings_and_preserves_argument_positions() {
    let (host, _, _) = host();
    let session = Rc::new(ox_api::ApiSession::new(Rc::new(
        RefCell::new(Editor::new()),
    )));
    let registry = ox_api::core().unwrap();
    bind_api(
        host.lua(),
        &registry,
        ApiDispatchContext::new(Rc::clone(&session)),
        host.fast_callbacks(),
    )
    .unwrap();

    let (return_count, tostring_calls, writes, written): (i64, i64, i64, String) = host
        .lua()
        .load(
            r"local builtin_tostring = tostring
local tostring_calls = 0
local writes = 0
local written = ''
_G.tostring = function(v)
  tostring_calls = tostring_calls + 1
  if v == nil then return '<nil>' end
  return builtin_tostring(v)
end
vim.api.nvim_out_write = function(v)
  writes = writes + 1
  written = v
  return 'must not leak'
end
local return_count = select('#', print('', nil, 7, nil))
return return_count, tostring_calls, writes, written",
        )
        .eval()
        .unwrap();

    assert_eq!(
        (return_count, tostring_calls, writes, written),
        (0, 4, 1, " <nil> 7 <nil>".to_owned())
    );
}

#[test]
fn api_registry_dispatches_against_editor_and_enforces_guards() {
    let (host, _, _) = host();
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let session = Rc::new(ox_api::ApiSession::new(Rc::new(RefCell::new(editor))));
    let context = ApiDispatchContext::new(Rc::clone(&session));
    let registry = ox_api::core().unwrap();
    bind_api(
        host.lua(),
        &registry,
        context.clone(),
        host.fast_callbacks(),
    )
    .unwrap();

    assert_eq!(
        host.lua()
            .load("return vim.api.nvim_get_current_buf()")
            .eval::<i64>()
            .unwrap(),
        i64::from(buffer)
    );
    assert!(
        host.lua()
            .load(
                "local function capture(...) return select('#', ...), ... end \
         local count, value = capture(vim.api.nvim_buf_set_lines(0, 0, -1, true, {'one', 'two'})) \
         return count == 1 and value == nil"
            )
            .eval::<bool>()
            .unwrap()
    );
    assert_eq!(
        host.lua()
            .load("return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, true), ',')")
            .eval::<String>()
            .unwrap(),
        "one,two",
    );
    let (ok, error_type, error): (bool, String, String) = host
        .lua()
        .load(
            "local ok, err = pcall(vim.api.nvim_create_autocmd, nil, {}) \
             return ok, type(err), err",
        )
        .eval()
        .unwrap();
    assert!(!ok);
    assert_eq!(error_type, "string");
    assert!(error.ends_with("Invalid 'event': expected Array or String, got nil"));
    let (ok, error_type): (bool, String) = host
        .lua()
        .load(
            "local ok, err = pcall(vim.api.nvim__get_runtime, {}, false, 42) \
             return ok, type(err)",
        )
        .eval()
        .unwrap();
    assert!(!ok);
    assert_eq!(error_type, "string");
    let state = host.fast_callbacks();
    let fast_guard = state.enter();
    let (ok, error_type, error): (bool, String, String) = host
        .lua()
        .load(
            "local ok, err = pcall(vim.api.nvim_get_current_buf) \
             return ok, type(err), err",
        )
        .eval()
        .unwrap();
    assert!(!ok);
    assert_eq!(error_type, "string");
    assert!(error.contains("E5560"));
    let builtin_error = host
        .lua()
        .load("return vim.call('Record')")
        .eval::<Value>()
        .unwrap_err();
    assert!(builtin_error.to_string().contains("E5560"));
    drop(fast_guard);
    assert!(!state.in_fast_callback());

    let textlock_guard = context.enter_textlock();
    let (ok, error_type, error): (bool, String, String) = host
        .lua()
        .load(
            "local ok, err = pcall(vim.api.nvim_buf_set_lines, 0, 0, -1, true, {'blocked'}) \
             return ok, type(err), err",
        )
        .eval()
        .unwrap();
    assert!(!ok);
    assert_eq!(error_type, "string");
    assert!(error.contains("E565"));
    drop(textlock_guard);
    assert_eq!(
        session
            .with_editor(|editor| { editor.buffer(buffer).unwrap().text().unwrap().line_count() }),
        2
    );
}

#[test]
fn decoration_provider_callback_survives_the_api_call() {
    let (host, _, _) = host();
    let mut editor = Editor::new();
    editor.create_buffer(true).unwrap();
    let session = Rc::new(ox_api::ApiSession::new(Rc::new(RefCell::new(editor))));
    let registry = ox_api::core().unwrap();
    bind_api(
        host.lua(),
        &registry,
        ApiDispatchContext::new(Rc::clone(&session)),
        host.fast_callbacks(),
    )
    .unwrap();

    // Register through the real Lua bridge: `on_line` crosses into the API
    // as a fresh `Object::LuaRef` argument.
    host.lua()
        .load(
            "provider_line_calls = 0 \
             local ns = vim.api.nvim_create_namespace('provider-lifetime') \
             vim.api.nvim_set_decoration_provider(ns, { \
               on_line = function() provider_line_calls = provider_line_calls + 1 end, \
             }) \
             return ns",
        )
        .eval::<Value>()
        .unwrap();

    // The argument reference was not freed when the call returned: the id
    // stored in the provider still resolves and invokes, mirroring upstream
    // moving the LuaRef into the provider (`extmark.c:1088-1096`).
    let provider = session.with_editor(|editor| {
        let ids = editor
            .decorations()
            .phase_provider_ids(ox_editor::decoration::CallbackPhase::Line);
        assert_eq!(ids.len(), 1);
        ids[0]
    });
    let reference = session
        .with_editor(|editor| {
            editor
                .decorations()
                .phase_callback(provider, ox_editor::decoration::CallbackPhase::Line)
        })
        .expect("the provider must keep its on_line callback");
    let callback = object_to_lua(
        host.lua(),
        &Object::LuaRef(i32::try_from(reference).unwrap()),
    )
    .unwrap();
    let Value::Function(function) = callback else {
        unreachable!("stored provider callback did not resolve to a function");
    };
    let _: () = function.call(()).unwrap();

    let calls: i64 = host
        .lua()
        .load("return provider_line_calls")
        .eval()
        .unwrap();
    assert_eq!(calls, 1);
}

#[test]
fn pcall_error_contains_traceback() {
    let (host, _, _) = host();
    let function: Function = host
        .lua()
        .load("return function() local function inner() error('boom') end inner() end")
        .eval()
        .unwrap();
    let error = call_with_traceback(host.lua(), &function, MultiValue::new()).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("boom"), "{text}");
    assert_eq!(text.matches("stack traceback").count(), 1, "{text}");
    assert!(text.contains("inner"), "{text}");
    for token in ["<userdata", "CallbackError {", "ExternalError("] {
        assert!(!text.contains(token), "{text}");
    }
}

#[test]
fn pcall_forwards_arguments() {
    let (host, _, _) = host();
    let function: Function = host
        .lua()
        .load("return function(a, b) return a + b end")
        .eval()
        .unwrap();
    let args = MultiValue::from_vec(vec![Value::Integer(4), Value::Integer(5)]);
    let results = call_with_traceback(host.lua(), &function, args).unwrap();
    assert_eq!(results.front(), Some(&Value::Integer(9)));
}

#[test]
fn nested_callback_error_keeps_root_and_single_traceback() {
    let (host, _, _) = host();
    let lua = host.lua();
    let leaf: Function = lua
        .create_function(|_, ()| -> mlua::Result<()> {
            Err(mlua::Error::external("inner-root-42"))
        })
        .unwrap();
    lua.globals().set("leaf_cb", leaf).unwrap();
    let outer: Function = lua
        .create_function(|lua, ()| -> mlua::Result<()> {
            let leaf: Function = lua.globals().get("leaf_cb")?;
            leaf.call::<()>(())
        })
        .unwrap();
    lua.globals().set("outer_cb", outer).unwrap();
    let function: Function = lua
        .load("return function() return outer_cb() end")
        .eval()
        .unwrap();
    let error = call_with_traceback(lua, &function, MultiValue::new()).unwrap_err();
    let text = error.to_string();
    assert!(text.starts_with("inner-root-42"), "{text}");
    assert_eq!(text.matches("inner-root-42").count(), 1, "{text}");
    assert_eq!(text.matches("stack traceback").count(), 1, "{text}");
    assert!(text.contains("outer_cb"), "{text}");
    for token in ["<userdata", "CallbackError {", "ExternalError("] {
        assert!(!text.contains(token), "{text}");
    }
}

#[test]
fn exec_preserves_api_error_message_and_single_traceback() {
    let (mut host, _, _) = host();
    let lua = host.lua();
    let fail: Function = lua
        .create_function(|_, ()| -> mlua::Result<()> {
            Err(mlua::Error::external(ApiError::validation(
                "Invalid 'group': 9",
            )))
        })
        .unwrap();
    lua.globals().set("fail_cb", fail).unwrap();
    let error = host.exec("return fail_cb()", vec![]).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("Invalid 'group': 9"), "{text}");
    assert_eq!(text.matches("stack traceback").count(), 1, "{text}");
    for token in ["<userdata", "CallbackError {", "ExternalError("] {
        assert!(!text.contains(token), "{text}");
    }
}

#[test]
fn exec_preserves_with_context_label_and_traceback() {
    let (mut host, _, _) = host();
    let lua = host.lua();
    let fail: Function = lua
        .create_function(|_, ()| -> mlua::Result<()> {
            Err(mlua::Error::runtime("root-cause-77").context("while loading session"))
        })
        .unwrap();
    lua.globals().set("ctx_cb", fail).unwrap();
    let error = host.exec("return ctx_cb()", vec![]).unwrap_err();
    let text = error.to_string();
    let context_at = text.find("while loading session").unwrap_or(usize::MAX);
    let root_at = text.find("root-cause-77").unwrap_or(usize::MAX);
    assert!(context_at < root_at, "{text}");
    assert!(text.contains("while loading session"), "{text}");
    assert!(text.contains("root-cause-77"), "{text}");
    assert_eq!(text.matches("stack traceback").count(), 1, "{text}");
}

#[test]
fn exec_preserves_bad_argument_label() {
    let (mut host, _, _) = host();
    let lua = host.lua();
    let typed: Function = lua.create_function(|_, _: String| Ok(())).unwrap();
    lua.globals().set("typed_cb", typed).unwrap();
    let error = host.exec("return typed_cb({})", vec![]).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("bad argument #1"), "{text}");
    assert!(text.contains("converting Lua table to String"), "{text}");
    assert_eq!(text.matches("stack traceback").count(), 1, "{text}");
}

#[test]
fn exec_preserves_raised_table_tostring() {
    let (mut host, _, _) = host();
    let error = host
        .exec(
            "error(setmetatable({}, {__tostring=function() return 'named-error-9' end}))",
            vec![],
        )
        .unwrap_err();
    let text = error.to_string();
    assert!(text.starts_with("Lua: named-error-9"), "{text}");
    assert_eq!(text.matches("stack traceback").count(), 1, "{text}");
    assert!(!text.contains("<userdata"), "{text}");
    assert!(!text.contains("table: 0x"), "{text}");
}

#[test]
fn exec_reports_unprintable_error_for_failing_tostring() {
    let (mut host, _, _) = host();
    let error = host
        .exec(
            "error(setmetatable({}, {__tostring=function() error('boom-meta') end}))",
            vec![],
        )
        .unwrap_err();
    let text = error.to_string();
    assert_eq!(
        text.split_once("\nstack traceback")
            .map(|(message, _)| message),
        Some("Lua: [UNPRINTABLE ERROR]"),
        "{text}"
    );
    assert!(!text.contains("boom-meta"), "{text}");
    assert_eq!(text.matches("stack traceback").count(), 1, "{text}");
    assert!(!text.contains("<userdata"), "{text}");
}

#[test]
fn exec_reports_unprintable_error_for_non_string_tostring() {
    let (mut host, _, _) = host();
    let error = host
        .exec(
            "error(setmetatable({}, {__tostring=function() return {} end}))",
            vec![],
        )
        .unwrap_err();
    let text = error.to_string();
    assert_eq!(
        text.split_once("\nstack traceback")
            .map(|(message, _)| message),
        Some("Lua: [UNPRINTABLE ERROR]"),
        "{text}"
    );
    assert_eq!(text.matches("stack traceback").count(), 1, "{text}");
}

#[test]
fn schedule_defers_until_scheduler_drains() {
    let (host, _, scheduler) = host();
    host.lua()
        .load("scheduled = false; vim.schedule(function() scheduled = true end)")
        .exec()
        .unwrap();
    assert!(!host.lua().globals().get::<bool>("scheduled").unwrap());
    scheduler.drain().unwrap();
    assert!(host.lua().globals().get::<bool>("scheduled").unwrap());
}

#[test]
fn exec_args_reachable_via_varargs() {
    let (mut host, _, _) = host();
    assert_eq!(
        host.exec(
            "local a, b = ...; return a + b",
            vec![Object::Integer(3), Object::Integer(4)]
        )
        .unwrap(),
        Object::Integer(7)
    );
}

#[test]
fn exec_converts_each_object_kind() {
    let (mut host, _, _) = host();
    let cases: Vec<(Object, Object)> = vec![
        (Object::Nil, Object::Nil),
        (Object::Boolean(true), Object::Boolean(true)),
        (Object::Integer(42), Object::Integer(42)),
        (Object::Float(1.5), Object::Float(1.5)),
        (
            Object::String(OxStr(vec![0, 0xff, b'x'])),
            Object::String(OxStr(vec![0, 0xff, b'x'])),
        ),
        (
            Object::Array(vec![Object::Integer(1), Object::Nil]),
            // Nil inside arrays is lost when pushed to Lua (lua_rawseti with
            // nil removes the key), matching upstream kNluaPushSpecial behavior.
            Object::Array(vec![Object::Integer(1)]),
        ),
        (
            Object::Dict(Dict(vec![(OxStr::from("key"), Object::Boolean(false))])),
            Object::Dict(Dict(vec![(OxStr::from("key"), Object::Boolean(false))])),
        ),
        (
            Object::Dict(Dict(Vec::new())),
            Object::Dict(Dict(Vec::new())),
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(host.exec("return ...", vec![input]).unwrap(), expected);
    }

    for object in [
        Object::Buffer(BufHandle::try_from(3).unwrap()),
        Object::Window(WinHandle::try_from(4).unwrap()),
        Object::Tabpage(TabHandle::try_from(5).unwrap()),
    ] {
        let expected = match object {
            Object::Buffer(value) => Object::Integer(i64::from(value)),
            Object::Window(value) => Object::Integer(i64::from(value)),
            Object::Tabpage(value) => Object::Integer(i64::from(value)),
            _ => unreachable!(),
        };
        assert_eq!(host.exec("return ...", vec![object]).unwrap(), expected);
    }

    let reference = {
        let lua = host.lua();
        let function: Function = lua
            .load("return function(x) return x + 1 end")
            .eval()
            .unwrap();
        let Object::LuaRef(reference) = lua_to_object(lua, &Value::Function(function)).unwrap()
        else {
            unreachable!()
        };
        reference
    };
    let result = host
        .exec("return ...", vec![Object::LuaRef(reference)])
        .unwrap();
    let call_result = {
        let lua = host.lua();
        let Object::LuaRef(reference) = result else {
            unreachable!()
        };
        let Value::Function(round_trip) = object_to_lua(lua, &Object::LuaRef(reference)).unwrap()
        else {
            unreachable!()
        };
        round_trip.call::<i64>(4).unwrap()
    };
    assert_eq!(call_result, 5);
}

#[test]
fn exec_error_contains_lua_message_and_traceback() {
    let (mut host, _, _) = host();
    let error = host
        .exec("local function inner() error('boom') end inner()", vec![])
        .unwrap_err();
    let text = error.to_string();
    assert!(text.contains("boom"), "{text}");
    assert!(text.contains("stack traceback"), "{text}");
    assert!(text.contains("inner"), "{text}");
    assert!(matches!(error, ExecError::Runtime(_)));
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn exec_file_runs_lua_file() {
    let (mut host, _, _) = host();
    let path = std::env::temp_dir().join("ox-lua-exec-file-test.lua");
    std::fs::write(&path, "exec_file_global = 42\nreturn 1").unwrap();
    let _guard = TempFile(path.clone());
    host.exec_file(&path).unwrap();
    let value: i64 = host.lua().globals().get("exec_file_global").unwrap();
    assert_eq!(value, 42);
}

#[test]
fn exec_file_error_contains_message() {
    let (mut host, _, _) = host();
    let path = std::env::temp_dir().join("ox-lua-exec-file-error-test.lua");
    std::fs::write(&path, "error('file boom')").unwrap();
    let _guard = TempFile(path.clone());
    let error = host.exec_file(&path).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("file boom"), "{text}");
    assert!(matches!(error, ExecError::Load(_)));
}
