//! Lua-facing compatibility tests for the Neovim standard-library bindings.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::rc::Rc;

use mlua::FromLuaMulti;
use ox_lua::{BuiltinHost, LuaHost, RuntimeRoot, Scheduler, Work};
use ox_types::{OxStr, Typval};

struct NoBuiltins;

impl BuiltinHost for NoBuiltins {
    fn call(&self, name: &OxStr, _args: Vec<Typval>) -> Result<Typval, String> {
        // The runtime prelude probes has('win32') during host init
        // (runtime/lua/vim/_core/system.lua).
        if name.as_bytes() == b"has" {
            return Ok(Typval::Number(0));
        }
        Err("builtin unavailable in stdlib test".to_owned())
    }
}

struct ImmediateScheduler;

impl Scheduler for ImmediateScheduler {
    fn schedule_deferred(&self, work: Work) -> Result<(), String> {
        work().map_err(|error| error.to_string())
    }
}

fn host() -> LuaHost {
    LuaHost::new(
        RuntimeRoot::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime")),
        Rc::new(NoBuiltins),
        Rc::new(ImmediateScheduler),
    )
    .unwrap()
}

fn eval<R: FromLuaMulti>(host: &LuaHost, source: &str) -> R {
    host.lua().load(source).eval().unwrap()
}

#[test]
fn mpack_uses_fixed_vectors_and_preserves_nil_binary_and_extensions() {
    let host = host();
    let nil: mlua::LuaString = eval(&host, "return vim.mpack.encode(vim.NIL)");
    assert_eq!(nil.as_bytes().as_ref(), [0xc0]);

    let array: mlua::LuaString = eval(&host, "return vim.mpack.encode({'a'})");
    assert_eq!(array.as_bytes().as_ref(), [0x91, 0xa1, b'a']);

    host.lua().globals().set("binary_value", host.lua().create_string([0, 255]).unwrap()).unwrap();
    let roundtrip: (bool, mlua::LuaString) = eval(
        &host,
        "local v = vim.mpack.decode(vim.mpack.encode({vim.NIL, binary_value})); \
         return v[1] == vim.NIL, v[2]",
    );
    assert!(roundtrip.0);
    assert_eq!(roundtrip.1.as_bytes().as_ref(), [0, 255]);

    host.lua().globals().set("raw_ext", host.lua().create_string([0xd4, 42, 0xff]).unwrap()).unwrap();
    let extension: mlua::LuaString = eval(&host, "return vim.mpack.encode(vim.mpack.decode(raw_ext))");
    assert_eq!(extension.as_bytes().as_ref(), [0xd4, 42, 0xff]);
}

#[test]
fn mpack_distinguishes_arrays_maps_and_rejects_malformed_or_recursive_values() {
    let host = host();
    let vectors: (mlua::LuaString, mlua::LuaString) = eval(
        &host,
        "local dict = setmetatable({}, vim._empty_dict_mt); \
         return vim.mpack.encode({}), vim.mpack.encode(dict)",
    );
    assert_eq!(vectors.0.as_bytes().as_ref(), [0x90]);
    assert_eq!(vectors.1.as_bytes().as_ref(), [0x80]);

    host.lua().globals().set("malformed", host.lua().create_string([0x81]).unwrap()).unwrap();
    host.lua().globals().set("trailing", host.lua().create_string([0xc0, 0xc0]).unwrap()).unwrap();
    let errors: (bool, bool, bool) = eval(
        &host,
        "local recursive = {}; recursive.self = recursive; \
         local a = pcall(vim.mpack.decode, malformed); \
         local b = pcall(vim.mpack.decode, trailing); \
         local c = pcall(vim.mpack.encode, recursive); \
         return a, b, c",
    );
    assert_eq!(errors, (false, false, false));
}

#[test]
fn json_encode_covers_shape_formatting_and_scalar_boundaries() {
    let host = host();
    let encoded: (String, String, String, String) = eval(
        &host,
        "local dict = setmetatable({}, vim._empty_dict_mt); \
         return vim.json.encode({}), vim.json.encode(dict), \
           vim.json.encode({b=2,a=1}, {sort_keys=true, indent='\t'}), \
           vim.json.encode({path='a/b'}, {escape_slash=true})",
    );
    assert_eq!(encoded.0, "[]");
    assert_eq!(encoded.1, "{}");
    assert_eq!(encoded.2, "{\n\t\"a\": 1,\n\t\"b\": 2\n}");
    assert_eq!(encoded.3, r#"{"path":"a\/b"}"#);

    host.lua().globals().set("exact_i64", 9_007_199_254_740_991_i64).unwrap();
    let scalars: (String, String, String, String) = eval(
        &host,
        "return vim.json.encode(vim.NIL), vim.json.encode(true), \
         vim.json.encode(exact_i64), vim.json.encode('é')",
    );
    assert_eq!(scalars, ("null".into(), "true".into(), "9007199254740991".into(), "\"é\"".into()));
}

#[test]
fn json_decode_honors_luanil_comments_and_empty_container_identity() {
    let host = host();
    let result: (bool, bool, bool, bool, i64) = eval(
        &host,
        "local value = vim.json.decode('{/*x*/\"a\":null,\"b\":[null],\"c\":{},\"d\":[]}', \
           {skip_comments=true, luanil={object=true,array=true}}); \
         return value.a == nil, value.b[1] == nil, \
           getmetatable(value.c) == vim._empty_dict_mt, \
           getmetatable(value.d) == nil, #value.d",
    );
    assert_eq!(result, (true, true, true, true, 0));

    let sentinel: bool = eval(&host, "return vim.json.decode('null') == vim.NIL");
    assert!(sentinel);
}

#[test]
fn json_rejects_invalid_options_values_and_shapes() {
    let host = host();
    let errors: (bool, bool, bool, bool, bool) = eval(
        &host,
        "local recursive = {}; recursive.self = recursive; \
         local sparse = {[1]='a',[3]='c'}; \
         local a = pcall(vim.json.decode, '{bad}'); \
         local b = pcall(vim.json.decode, '/*', {skip_comments=true}); \
         local c = pcall(vim.json.encode, 0/0); \
         local d = pcall(vim.json.encode, sparse); \
         local e = pcall(vim.json.encode, recursive); \
         return a,b,c,d,e",
    );
    assert_eq!(errors, (false, false, false, false, false));
}

#[test]
fn diff_matches_neovim_unified_and_index_oracles() {
    let host = host();
    let unified: String = eval(&host, "return vim.diff('a\\nb\\nc\\n','a\\nx\\nc\\n',{})");
    assert_eq!(unified, "@@ -2 +2 @@\n-b\n+x\n");

    let indices: (i64, i64, i64, i64) = eval(
        &host,
        "local h=vim.diff('a\\nb\\nc\\n','a\\nx\\nc\\n',{result_type='indices'})[1]; \
         return h[1],h[2],h[3],h[4]",
    );
    assert_eq!(indices, (2, 1, 2, 1));

    let no_newline: String = eval(&host, "return vim.diff('one\\ntwo\\n','one\\ntwo',{})");
    assert_eq!(no_newline, "@@ -2 +2 @@\n-two\n+two\n\\ No newline at end of file\n");

    let histogram: (i64, i64, i64, i64) = eval(
        &host,
        "local h=vim.diff('a\\nb\\nc\\nd\\n','a\\nc\\nd\\n', \
          {algorithm='histogram',result_type='indices'})[1]; return h[1],h[2],h[3],h[4]",
    );
    assert_eq!(histogram, (2, 1, 1, 0));
}

#[test]
fn diff_supports_context_callbacks_and_option_errors() {
    let host = host();
    let result: (bool, i64, i64, i64, i64) = eval(
        &host,
        "local seen; local result=vim.diff('a\\nb\\n','a\\nx\\n',{on_hunk=function(...) seen={...}; return -1 end}); \
         return result == nil, seen[1],seen[2],seen[3],seen[4]",
    );
    assert_eq!(result, (true, 2, 1, 2, 1));
    let errors: (bool, bool, bool) = eval(
        &host,
        "return pcall(vim.diff,'a','b',{algorithm='bogus'}), \
          pcall(vim.diff,'a','b',{result_type='bogus'}), \
          pcall(vim.diff,'a','b',{ctxlen=-1})",
    );
    assert_eq!(errors, (false, false, false));
}

#[test]
fn base64_matches_rfc_vectors_binary_boundaries_and_errors() {
    let host = host();
    let vectors: (String, String, String, String, mlua::LuaString) = eval(
        &host,
        "return vim.base64.encode(''), vim.base64.encode('f'), vim.base64.encode('fo'), \
         vim.base64.encode('foo'), vim.base64.decode('AP+A')",
    );
    assert_eq!(vectors.0, "");
    assert_eq!(vectors.1, "Zg==");
    assert_eq!(vectors.2, "Zm8=");
    assert_eq!(vectors.3, "Zm9v");
    assert_eq!(vectors.4.as_bytes().as_ref(), [0, 255, 128]);
    let errors: (bool, bool, bool, bool) = eval(
        &host,
        "return pcall(vim.base64.decode,'Zg='), pcall(vim.base64.decode,'Z===') , \
          pcall(vim.base64.decode,'Zh=='), pcall(vim.base64.decode,'Zm$=')",
    );
    assert_eq!(errors, (false, false, false, false));
}

#[test]
fn regex_reports_byte_spans_no_match_and_line_relative_ranges() {
    let host = host();
    let spans: (i64, i64, bool) = eval(
        &host,
        "local re=vim.regex('é'); local s,e=re:match_str('aéz'); \
         local x,y=re:match_str('abc'); return s,e,x==nil and y==nil",
    );
    assert_eq!(spans, (1, 3, true));

    let line_span: (i64, i64) = eval(
        &host,
        "vim.api.nvim_buf_get_lines=function(buf,s,e,strict) return {'xxéyy'} end; \
         local s,e=vim.regex('é'):match_line(7,0,1,5); return s,e",
    );
    assert_eq!(line_span, (1, 3));
}

#[test]
fn regex_rejects_invalid_patterns_ranges_and_utf8() {
    let host = host();
    host.lua().globals().set("invalid_utf8", host.lua().create_string([0xff]).unwrap()).unwrap();
    let errors: (bool, bool, bool, bool) = eval(
        &host,
        "vim.api.nvim_buf_get_lines=function() return {'abc'} end; \
         local re=vim.regex('a'); \
         return pcall(vim.regex,'['), pcall(re.match_str,re,invalid_utf8), \
           pcall(re.match_line,re,0,-1), pcall(re.match_line,re,0,0,3,2)",
    );
    assert_eq!(errors, (false, false, false, false));
}
