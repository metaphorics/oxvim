//! Behavioral contract tests for the Lua host core.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Function, MultiValue, Value};
use ox_lua::{
    bind_api, call_with_traceback, lua_to_object, lua_to_typval, object_to_lua, typval_to_lua,
    ApiFunction, ApiRegistry, BuiltinHost, ConversionError, LuaHost, RuntimeRoot, Scheduler, Work,
    CONVERSION_RECURSION_LIMIT,
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
        "coroutine", "table", "io", "os", "string", "math", "package", "debug", "bit", "jit",
    ] {
        assert!(!matches!(globals.get::<Value>(name).unwrap(), Value::Nil), "missing {name}");
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
    assert!(matches!(ffi, Value::Table(_)), "require('ffi') should return the ffi module table");
    assert!(matches!(globals.get::<Value>("ffi").unwrap(), Value::Nil), "ffi must not be a global after require");
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
        Object::Array(vec![Object::Integer(1), Object::Nil]),
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
        assert_eq!(lua_to_object(lua, &lua_value).unwrap(), Object::Integer(expected));
    }
}

#[test]
fn empty_table_and_empty_dict_metatable_remain_distinct() {
    let (host, _, _) = host();
    let lua = host.lua();
    let plain = lua.create_table().unwrap();
    assert_eq!(lua_to_object(lua, &Value::Table(plain)).unwrap(), Object::Array(Vec::new()));

    let dictionary = object_to_lua(lua, &Object::Dict(Dict(Vec::new()))).unwrap();
    let Value::Table(dictionary) = dictionary else { unreachable!() };
    let vim: mlua::Table = lua.globals().get("vim").unwrap();
    let marker: mlua::Table = vim.get("_empty_dict_mt").unwrap();
    assert_eq!(dictionary.metatable().unwrap().to_pointer(), marker.to_pointer());
    assert_eq!(lua_to_object(lua, &Value::Table(dictionary)).unwrap(), Object::Dict(Dict(Vec::new())));
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
    let function: Function = lua.load("return function(x) return x + 1 end").eval().unwrap();
    let object = lua_to_object(lua, &Value::Function(function)).unwrap();
    let Object::LuaRef(reference) = object else { unreachable!() };
    let Value::Function(function) = object_to_lua(lua, &Object::LuaRef(reference)).unwrap() else {
        unreachable!()
    };
    assert_eq!(function.call::<i64>(4).unwrap(), 5);

    let userdata = lua.create_userdata(TestUserdata(17)).unwrap();
    let object = lua_to_object(lua, &Value::UserData(userdata.clone())).unwrap();
    let Object::LuaRef(reference) = object else { unreachable!() };
    let Value::UserData(round_trip) = object_to_lua(lua, &Object::LuaRef(reference)).unwrap() else {
        unreachable!()
    };
    assert_eq!(round_trip.borrow::<TestUserdata>().unwrap().0, 17);
}

#[test]
fn numeric_conversion_follows_luajit_double_precision() {
    let (host, _, _) = host();
    let lua = host.lua();
    let original = Object::Integer(i64::MAX);
    let lua_value = object_to_lua(lua, &original).unwrap();
    assert!(matches!(lua_value, Value::Number(_)));
    assert_eq!(lua_to_object(lua, &lua_value).unwrap(), Object::Float(i64::MAX as f64));

    let exact_large = Value::Number(9_007_199_254_740_994.0);
    assert_eq!(lua_to_object(lua, &exact_large).unwrap(), Object::Integer(9_007_199_254_740_994));
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
        Err(ConversionError::RecursionLimit { limit: CONVERSION_RECURSION_LIMIT })
    ));
}

#[test]
fn vim_nil_maps_to_api_nil_and_vimscript_null() {
    let (host, _, _) = host();
    let lua = host.lua();
    let nil = object_to_lua(lua, &Object::Nil).unwrap();
    assert_eq!(lua_to_object(lua, &nil).unwrap(), Object::Nil);
    assert_eq!(lua_to_typval(lua, &nil).unwrap(), Typval::Special(Special::Null));
    let pushed_null = typval_to_lua(lua, &Typval::Special(Special::Null)).unwrap();
    assert_eq!(lua_to_object(lua, &pushed_null).unwrap(), Object::Nil);
    assert!(matches!(pushed_null, Value::UserData(_)));
    assert_eq!(lua.load("return tostring(vim.NIL)").eval::<String>().unwrap(), "vim.NIL");
}

#[test]
fn typval_bridge_maps_lists_dicts_funcrefs_and_blobs() {
    let (host, _, _) = host();
    let lua = host.lua();
    let value = Typval::dict(vec![
        (OxStr::from("list"), Typval::list(vec![Typval::Number(1), Typval::Bool(true)])),
        (OxStr::from("empty"), Typval::dict(Vec::new())),
    ]);
    let lua_value = typval_to_lua(lua, &value).unwrap();
    assert_eq!(lua_to_typval(lua, &lua_value).unwrap(), value);

    let blob = Typval::Blob(vec![0, 0xff]);
    let blob_lua = typval_to_lua(lua, &blob).unwrap();
    assert_eq!(lua_to_typval(lua, &blob_lua).unwrap(), Typval::String(OxStr(vec![0, 0xff])));

    let function: Value = lua.load("return function() return 7 end").eval().unwrap();
    let funcref = lua_to_typval(lua, &function).unwrap();
    let round_trip = typval_to_lua(lua, &funcref).unwrap();
    let Value::Function(round_trip) = round_trip else { unreachable!() };
    assert_eq!(round_trip.call::<i64>(()).unwrap(), 7);
}

#[test]
fn typval_bridge_preserves_null_entries_and_recursive_identity() {
    let (host, _, _) = host();
    let lua = host.lua();
    let list = Typval::list(vec![Typval::Number(1), Typval::Special(Special::Null)]);
    let Value::Table(table) = typval_to_lua(lua, &list).unwrap() else { unreachable!() };
    assert_eq!(table.raw_len(), 2);
    assert_eq!(lua_to_object(lua, &table.raw_get::<Value>(2).unwrap()).unwrap(), Object::Nil);

    let recursive = Typval::list(Vec::new());
    let Typval::List(items) = &recursive else { unreachable!() };
    items.borrow_mut().items.push(recursive.clone());
    let Value::Table(recursive_lua) = typval_to_lua(lua, &recursive).unwrap() else { unreachable!() };
    let child: mlua::Table = recursive_lua.raw_get(1).unwrap();
    assert_eq!(child.to_pointer(), recursive_lua.to_pointer());

    let lua_cycle = lua.create_table().unwrap();
    lua_cycle.raw_set(1, lua_cycle.clone()).unwrap();
    let converted = lua_to_typval(lua, &Value::Table(lua_cycle)).unwrap();
    let Typval::List(root) = &converted else { unreachable!() };
    let Typval::List(child) = root.borrow().items[0].clone() else { unreachable!() };
    assert!(Rc::ptr_eq(root, &child));
}

#[test]
fn vim_call_and_fn_dispatch_through_builtin_host() {
    let (host, builtins, _) = host();
    assert_eq!(
        host.lua().load("return vim.call('Record', 3, 'x')").eval::<String>().unwrap(),
        "called"
    );
    assert_eq!(
        host.lua().load("return vim.fn.Other(4)").eval::<String>().unwrap(),
        "called"
    );
    let calls = builtins.calls.borrow();
    assert_eq!(calls[0].0, OxStr::from("Record"));
    assert_eq!(calls[0].1, vec![Typval::Number(3), Typval::String(OxStr::from("x"))]);
    assert_eq!(calls[1].0, OxStr::from("Other"));
}

struct OneApi;

fn nil_api(_: &[Object]) -> Result<Object, ApiError> {
    Ok(Object::Nil)
}

impl ApiRegistry for OneApi {
    fn functions(&self) -> Vec<ApiFunction> {
        vec![ApiFunction { name: "nvim_guarded", fast: false, textlock: true, dispatch: nil_api }]
    }
}

#[test]
fn api_nil_uses_sentinel_and_fast_callback_raises_e5560() {
    let (host, _, _) = host();
    bind_api(host.lua(), &OneApi, host.fast_callbacks()).unwrap();
    assert!(host.lua().load("return vim.api.nvim_guarded() == vim.NIL").eval::<bool>().unwrap());

    let state = host.fast_callbacks();
    let guard = state.enter();
    let error = host.lua().load("return vim.api.nvim_guarded()").eval::<Value>().unwrap_err();
    assert!(error.to_string().contains("E5560"));
    let builtin_error = host.lua().load("return vim.call('Record')").eval::<Value>().unwrap_err();
    assert!(builtin_error.to_string().contains("E5560"));
    drop(guard);
    assert!(!state.in_fast_callback());
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
    assert!(text.contains("boom"));
    assert!(text.contains("stack traceback"));
    assert!(text.contains("inner"));
}

#[test]
fn pcall_forwards_arguments() {
    let (host, _, _) = host();
    let function: Function = host.lua().load("return function(a, b) return a + b end").eval().unwrap();
    let args = MultiValue::from_vec(vec![Value::Integer(4), Value::Integer(5)]);
    let results = call_with_traceback(host.lua(), &function, args).unwrap();
    assert_eq!(results.front(), Some(&Value::Integer(9)));
}

#[test]
fn schedule_defers_until_scheduler_drains() {
    let (host, _, scheduler) = host();
    host.lua().load("scheduled = false; vim.schedule(function() scheduled = true end)").exec().unwrap();
    assert!(!host.lua().globals().get::<bool>("scheduled").unwrap());
    scheduler.drain().unwrap();
    assert!(host.lua().globals().get::<bool>("scheduled").unwrap());
}
