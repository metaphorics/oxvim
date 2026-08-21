use std::collections::HashSet;

use mlua::{Lua, LuaString, Table, Value};
use serde::Serialize;
use serde_json::{Map, Number, Value as JsonValue};

use crate::converter::{has_empty_dict_metatable, is_vim_nil};

const RECURSION_LIMIT: usize = 100;

#[derive(Clone, Copy, Default)]
struct LuaNilOptions {
    object: bool,
    array: bool,
}

struct EncodeOptions {
    escape_slash: bool,
    indent: Vec<u8>,
    sort_keys: bool,
}

pub(super) fn install(lua: &Lua, vim: &Table) -> mlua::Result<()> {
    let module = lua.create_table()?;
    module.set(
        "encode",
        lua.create_function(|lua, (value, options): (Value, Option<Table>)| {
            let options = parse_encode_options(options)?;
            let mut active = HashSet::new();
            let value = lua_to_json(lua, &value, 0, &mut active, options.sort_keys)?;
            let mut output = if options.indent.is_empty() {
                serde_json::to_vec(&value).map_err(mlua::Error::external)?
            } else {
                let formatter = serde_json::ser::PrettyFormatter::with_indent(&options.indent);
                let mut output = Vec::new();
                let mut serializer = serde_json::Serializer::with_formatter(&mut output, formatter);
                value.serialize(&mut serializer).map_err(mlua::Error::external)?;
                output
            };
            if options.escape_slash {
                output = escape_slashes(&output);
            }
            lua.create_string(output)
        })?,
    )?;
    module.set(
        "decode",
        lua.create_function(|lua, (input, options): (LuaString, Option<Table>)| {
            let (luanil, skip_comments) = parse_decode_options(options)?;
            let input = if skip_comments {
                strip_comments(&input.as_bytes())?
            } else {
                input.as_bytes().to_vec()
            };
            let value: JsonValue = serde_json::from_slice(&input)
                .map_err(|error| mlua::Error::runtime(format!("invalid JSON: {error}")))?;
            json_to_lua(lua, value, 0, luanil, Container::Top)
        })?,
    )?;
    vim.set("json", module)
}

fn parse_encode_options(options: Option<Table>) -> mlua::Result<EncodeOptions> {
    let Some(options) = options else {
        return Ok(EncodeOptions { escape_slash: false, indent: Vec::new(), sort_keys: false });
    };
    let indent = options
        .get::<Option<LuaString>>("indent")?
        .map_or_else(Vec::new, |value| value.as_bytes().to_vec());
    if indent.iter().any(|byte| !matches!(byte, b' ' | b'\t')) {
        return Err(mlua::Error::runtime("JSON indent must contain only spaces or tabs"));
    }
    Ok(EncodeOptions {
        escape_slash: options.get::<Option<bool>>("escape_slash")?.unwrap_or(false),
        indent,
        sort_keys: options.get::<Option<bool>>("sort_keys")?.unwrap_or(false),
    })
}

fn parse_decode_options(options: Option<Table>) -> mlua::Result<(LuaNilOptions, bool)> {
    let Some(options) = options else {
        return Ok((LuaNilOptions::default(), false));
    };
    let luanil = match options.get::<Value>("luanil")? {
        Value::Nil => LuaNilOptions::default(),
        Value::Table(value) => LuaNilOptions {
            object: value.get::<Option<bool>>("object")?.unwrap_or(false),
            array: value.get::<Option<bool>>("array")?.unwrap_or(false),
        },
        _ => return Err(mlua::Error::runtime("luanil must be a table")),
    };
    Ok((luanil, options.get::<Option<bool>>("skip_comments")?.unwrap_or(false)))
}

fn lua_to_json(
    lua: &Lua,
    value: &Value,
    depth: usize,
    active: &mut HashSet<usize>,
    sort_keys: bool,
) -> mlua::Result<JsonValue> {
    if depth > RECURSION_LIMIT {
        return Err(mlua::Error::runtime("JSON object is too deeply nested"));
    }
    Ok(match value {
        Value::Nil => JsonValue::Null,
        Value::Boolean(value) => JsonValue::Bool(*value),
        Value::Integer(value) => JsonValue::Number(Number::from(*value)),
        Value::Number(value) => JsonValue::Number(
            Number::from_f64(*value)
                .ok_or_else(|| mlua::Error::runtime("cannot encode non-finite number as JSON"))?,
        ),
        Value::String(value) => {
            let bytes = value.as_bytes();
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| mlua::Error::runtime("JSON strings must be valid UTF-8"))?;
            JsonValue::String(text.to_owned())
        }
        Value::Table(table) => table_to_json(lua, table, depth, active, sort_keys)?,
        Value::UserData(_) if is_vim_nil(lua, value).map_err(mlua::Error::external)? => {
            JsonValue::Null
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "cannot encode {} as JSON",
                other.type_name()
            )))
        }
    })
}

fn table_to_json(
    lua: &Lua,
    table: &Table,
    depth: usize,
    active: &mut HashSet<usize>,
    sort_keys: bool,
) -> mlua::Result<JsonValue> {
    let pointer = table.to_pointer() as usize;
    if !active.insert(pointer) {
        return Err(mlua::Error::runtime("cannot encode recursive table as JSON"));
    }
    let mut entries = Vec::new();
    let mut max_index = 0usize;
    let mut array_only = !has_empty_dict_metatable(lua, table).map_err(mlua::Error::external)?;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        match key {
            Value::Integer(index) if index > 0 => {
                max_index = max_index.max(index as usize);
                entries.push((Value::Integer(index), value));
            }
            key => {
                array_only = false;
                entries.push((key, value));
            }
        }
    }

    let result = if array_only {
        if max_index != entries.len() {
            return Err(mlua::Error::runtime("cannot encode sparse array as JSON"));
        }
        let mut values = vec![JsonValue::Null; max_index];
        for (key, value) in &entries {
            let Value::Integer(index) = key else {
                continue;
            };
            values[*index as usize - 1] = lua_to_json(lua, value, depth + 1, active, sort_keys)?;
        }
        JsonValue::Array(values)
    } else {
        let mut values = entries
            .into_iter()
            .map(|(key, value)| {
                let Value::String(key) = key else {
                    return Err(mlua::Error::runtime("JSON object keys must be strings"));
                };
                let bytes = key.as_bytes();
                let key = std::str::from_utf8(&bytes)
                    .map_err(|_| mlua::Error::runtime("JSON object keys must be valid UTF-8"))?
                    .to_owned();
                Ok((key, value))
            })
            .collect::<mlua::Result<Vec<_>>>()?;
        if sort_keys {
            values.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        }
        let mut object = Map::new();
        for (key, value) in values {
            object.insert(key, lua_to_json(lua, &value, depth + 1, active, sort_keys)?);
        }
        JsonValue::Object(object)
    };
    active.remove(&pointer);
    Ok(result)
}

#[derive(Clone, Copy)]
enum Container {
    Top,
    Object,
    Array,
}

fn json_to_lua(
    lua: &Lua,
    value: JsonValue,
    depth: usize,
    luanil: LuaNilOptions,
    container: Container,
) -> mlua::Result<Value> {
    if depth > RECURSION_LIMIT {
        return Err(mlua::Error::runtime("JSON object is too deeply nested"));
    }
    match value {
        JsonValue::Null => {
            if matches!(container, Container::Object) && luanil.object
                || matches!(container, Container::Array) && luanil.array
            {
                Ok(Value::Nil)
            } else {
                vim_nil(lua)
            }
        }
        JsonValue::Bool(value) => Ok(Value::Boolean(value)),
        JsonValue::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(Value::Integer(value))
            } else if let Some(value) = value.as_u64().filter(|value| *value <= i64::MAX as u64) {
                Ok(Value::Integer(value as i64))
            } else {
                value
                    .as_f64()
                    .map(Value::Number)
                    .ok_or_else(|| mlua::Error::runtime("JSON number is outside Lua range"))
            }
        }
        JsonValue::String(value) => Ok(Value::String(lua.create_string(value)?)),
        JsonValue::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            for (index, value) in values.into_iter().enumerate() {
                let value = json_to_lua(lua, value, depth + 1, luanil, Container::Array)?;
                if !matches!(value, Value::Nil) {
                    table.raw_set(index + 1, value)?;
                }
            }
            Ok(Value::Table(table))
        }
        JsonValue::Object(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            if values.is_empty() {
                set_empty_dict_metatable(lua, &table)?;
            }
            for (key, value) in values {
                let value = json_to_lua(lua, value, depth + 1, luanil, Container::Object)?;
                if !matches!(value, Value::Nil) {
                    table.raw_set(key, value)?;
                }
            }
            Ok(Value::Table(table))
        }
    }
}

fn vim_nil(lua: &Lua) -> mlua::Result<Value> {
    let vim: Table = lua.globals().get("vim")?;
    vim.get("NIL")
}

fn set_empty_dict_metatable(lua: &Lua, table: &Table) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let metatable: Table = vim.get("_empty_dict_mt")?;
    table.set_metatable(Some(metatable))
}

fn escape_slashes(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    for byte in input {
        if *byte == b'/' {
            output.push(b'\\');
        }
        output.push(*byte);
    }
    output
}

fn strip_comments(input: &[u8]) -> mlua::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    let mut quoted = false;
    let mut escaped = false;
    while index < input.len() {
        let byte = input[index];
        if quoted {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            quoted = true;
            output.push(byte);
            index += 1;
        } else if input.get(index..index + 2) == Some(b"//") {
            index += 2;
            while index < input.len() && input[index] != b'\n' {
                index += 1;
            }
        } else if input.get(index..index + 2) == Some(b"/*") {
            index += 2;
            let mut closed = false;
            while index + 1 < input.len() {
                if &input[index..index + 2] == b"*/" {
                    index += 2;
                    closed = true;
                    break;
                }
                index += 1;
            }
            if !closed {
                return Err(mlua::Error::runtime("unterminated JSON block comment"));
            }
        } else {
            output.push(byte);
            index += 1;
        }
    }
    Ok(output)
}
