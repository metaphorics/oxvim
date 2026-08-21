use ox_api::{
    ApiError, BufHandle, Dict, LuaRef, Nil, Object, OxStr, Registry, TabHandle, TypeRef,
    WinHandle, api,
};

#[api(since = 3, deprecated_since = 7, method)]
fn nvim_buf_boundary(
    buf: BufHandle,
    boolean: bool,
    integer: i64,
    float: f64,
    string: OxStr,
    array: Vec<i64>,
    dict: Dict,
    lua_ref: LuaRef,
    window: WinHandle,
    tabpage: TabHandle,
    nil: Nil,
    object: Object,
) -> Result<Object, ApiError> {
    Ok(Object::Array(vec![
        Object::Buffer(buf),
        Object::Boolean(boolean),
        Object::Integer(integer),
        Object::Float(float),
        Object::String(string),
        Object::Array(array.into_iter().map(Object::Integer).collect()),
        Object::Dict(dict),
        Object::LuaRef(lua_ref.0),
        Object::Window(window),
        Object::Tabpage(tabpage),
        Object::Nil,
        object,
    ]))
}

#[api(since = 1, fast)]
fn nvim_identity(value: Object) -> Result<Object, ApiError> {
    Ok(value)
}

#[api(since = 2, textlock)]
fn nvim_void() -> Result<(), ApiError> {
    Ok(())
}

#[api(noexport)]
fn internal_helper(value: i64) -> Result<i64, ApiError> {
    Ok(value)
}

fn buffer(value: i64) -> BufHandle {
    BufHandle::try_from(value).unwrap_or(BufHandle::CURRENT)
}

fn window(value: i64) -> WinHandle {
    WinHandle::try_from(value).unwrap_or(WinHandle::CURRENT)
}

fn tabpage(value: i64) -> TabHandle {
    TabHandle::try_from(value).unwrap_or(TabHandle::CURRENT)
}

#[test]
fn generated_metadata_matches_the_rust_signature() {
    let metadata = nvim_buf_boundary__API_META();
    assert_eq!(metadata.name, "nvim_buf_boundary");
    assert_eq!(metadata.since, 3);
    assert_eq!(metadata.deprecated_since, Some(7));
    assert!(metadata.method);
    assert!(!metadata.fast);
    assert!(!metadata.textlock);
    assert_eq!(metadata.returns, TypeRef::Nil);
    assert_eq!(
        metadata.params,
        &[
            ("buf", TypeRef::Buffer),
            ("boolean", TypeRef::Boolean),
            ("integer", TypeRef::Integer),
            ("float", TypeRef::Float),
            ("string", TypeRef::String),
            ("array", TypeRef::ArrayOf(&TypeRef::Integer)),
            ("dict", TypeRef::Dict),
            ("lua_ref", TypeRef::LuaRef),
            ("window", TypeRef::Window),
            ("tabpage", TypeRef::Tabpage),
            ("nil", TypeRef::Nil),
            ("object", TypeRef::Nil),
        ]
    );

    let fast = nvim_identity__API_META();
    assert!(fast.fast);
    let textlocked = nvim_void__API_META();
    assert!(textlocked.textlock);
    assert_eq!(textlocked.returns, TypeRef::Void);
}

#[test]
fn dispatch_unpacks_every_supported_boundary_type() {
    let arguments = vec![
        Object::Buffer(buffer(1)),
        Object::Boolean(true),
        Object::Integer(2),
        Object::Float(3.5),
        Object::String(OxStr::from("four")),
        Object::Array(vec![Object::Integer(5)]),
        Object::Dict(Dict(vec![(OxStr::from("six"), Object::Integer(6))])),
        Object::LuaRef(7),
        Object::Window(window(8)),
        Object::Tabpage(tabpage(9)),
        Object::Nil,
        Object::String(OxStr::from("passthrough")),
    ];

    let result = nvim_buf_boundary__API_DISPATCH(&arguments);
    assert_eq!(result, Ok(Object::Array(arguments)));
    assert_eq!(nvim_void__API_DISPATCH(&[]), Ok(Object::Nil));
    assert_eq!(internal_helper(4), Ok(4));
}

#[test]
fn object_passthrough_round_trips_every_wire_kind() {
    let values = vec![
        Object::Nil,
        Object::Boolean(false),
        Object::Integer(-2),
        Object::Float(1.5),
        Object::String(OxStr::from(&[0xff][..])),
        Object::Array(vec![Object::Integer(1)]),
        Object::Dict(Dict(vec![(OxStr::from("k"), Object::Nil)])),
        Object::LuaRef(-1),
        Object::Buffer(buffer(1)),
        Object::Window(window(2)),
        Object::Tabpage(tabpage(3)),
    ];

    for value in values {
        assert_eq!(nvim_identity__API_DISPATCH(&[value.clone()]), Ok(value));
    }
}

#[test]
fn dispatch_errors_match_upstream_generated_text() {
    assert_eq!(
        nvim_identity__API_DISPATCH(&[]),
        Err(ApiError::exception(
            "Wrong number of arguments: expecting 1 but got 0"
        ))
    );
    assert_eq!(
        nvim_buf_boundary__API_DISPATCH(&[
            Object::Buffer(buffer(1)),
            Object::String(OxStr::from("not-a-boolean")),
        ]),
        Err(ApiError::exception(
            "Wrong number of arguments: expecting 12 but got 2"
        ))
    );

    let mut arguments = vec![Object::Nil; 12];
    arguments[0] = Object::Buffer(buffer(1));
    arguments[1] = Object::String(OxStr::from("not-a-boolean"));
    assert_eq!(
        nvim_buf_boundary__API_DISPATCH(&arguments),
        Err(ApiError::exception(
            "Wrong type for argument 2 when calling nvim_buf_boundary, expecting Boolean"
        ))
    );
}

#[test]
fn generated_entries_register_explicitly() {
    let mut registry = Registry::new();
    assert_eq!(
        registry.register(nvim_identity__API_META(), nvim_identity__API_DISPATCH),
        Ok(())
    );
    assert_eq!(
        registry.register(nvim_void__API_META(), nvim_void__API_DISPATCH),
        Ok(())
    );
    let names: Vec<_> = registry.iter().map(|entry| entry.0.name).collect();
    assert_eq!(names, ["nvim_identity", "nvim_void"]);
}

#[test]
fn recursive_type_refs_preserve_upstream_spelling() {
    static INTEGER: TypeRef = TypeRef::Integer;
    static ARRAY: TypeRef = TypeRef::ArrayOf(&INTEGER);

    assert_eq!(TypeRef::Array.to_string(), "Array");
    assert_eq!(TypeRef::ArrayOf(&INTEGER).to_string(), "ArrayOf(Integer)");
    assert_eq!(TypeRef::DictOf(&ARRAY).to_string(), "DictOf(ArrayOf(Integer))");
}
