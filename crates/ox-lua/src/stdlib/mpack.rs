use std::collections::HashSet;
use std::io::Cursor;

use mlua::{Lua, LuaString, MetaMethod, Table, UserData, UserDataMethods, Value};
use rmpv::Value as MpackValue;

use crate::converter::{has_empty_dict_metatable, is_vim_nil};

const RECURSION_LIMIT: usize = 100;

#[derive(Clone, Debug)]
struct MpackExt {
    kind: i8,
    data: Vec<u8>,
}

impl UserData for MpackExt {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("vim.mpack.ext({}, {} bytes)", this.kind, this.data.len()))
        });
    }
}

pub(super) fn install(lua: &Lua, vim: &Table) -> mlua::Result<()> {
    let module = lua.create_table()?;
    module.set(
        "encode",
        lua.create_function(|lua, value: Value| {
            let mut active = HashSet::new();
            let value = lua_to_mpack(lua, &value, 0, &mut active)?;
            let mut output = Vec::new();
            rmpv::encode::write_value(&mut output, &value).map_err(mlua::Error::external)?;
            lua.create_string(output)
        })?,
    )?;
    module.set(
        "decode",
        lua.create_function(|lua, input: LuaString| {
            let bytes = input.as_bytes();
            let mut cursor = Cursor::new(bytes.as_ref());
            let value = rmpv::decode::read_value(&mut cursor)
                .map_err(|error| mlua::Error::runtime(format!("invalid msgpack: {error}")))?;
            if cursor.position() != bytes.len() as u64 {
                return Err(mlua::Error::runtime("invalid msgpack: trailing data"));
            }
            mpack_to_lua(lua, value, 0)
        })?,
    )?;
    vim.set("mpack", module)
}

fn lua_to_mpack(
    lua: &Lua,
    value: &Value,
    depth: usize,
    active: &mut HashSet<usize>,
) -> mlua::Result<MpackValue> {
    if depth > RECURSION_LIMIT {
        return Err(mlua::Error::runtime("msgpack object is too deeply nested"));
    }
    Ok(match value {
        Value::Nil => MpackValue::Nil,
        Value::Boolean(value) => MpackValue::Boolean(*value),
        Value::Integer(value) => MpackValue::Integer((*value).into()),
        Value::Number(value) if value.is_finite() => MpackValue::F64(*value),
        Value::Number(_) => return Err(mlua::Error::runtime("cannot encode non-finite number")),
        Value::String(value) => {
            let bytes = value.as_bytes();
            match std::str::from_utf8(&bytes) {
                Ok(text) => MpackValue::String(text.to_owned().into()),
                Err(_) => MpackValue::Binary(bytes.to_vec()),
            }
        }
        Value::Table(table) => table_to_mpack(lua, table, depth, active)?,
        Value::UserData(_) if is_vim_nil(lua, value).map_err(mlua::Error::external)? => {
            MpackValue::Nil
        }
        Value::UserData(userdata) => {
            let extension = userdata.borrow::<MpackExt>().map_err(|_| {
                mlua::Error::runtime("cannot encode userdata other than vim.NIL or msgpack extension")
            })?;
            MpackValue::Ext(extension.kind, extension.data.clone())
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "cannot encode {} as msgpack",
                other.type_name()
            )))
        }
    })
}

fn table_to_mpack(
    lua: &Lua,
    table: &Table,
    depth: usize,
    active: &mut HashSet<usize>,
) -> mlua::Result<MpackValue> {
    let pointer = table.to_pointer() as usize;
    if !active.insert(pointer) {
        return Err(mlua::Error::runtime("cannot encode recursive table"));
    }
    let mut entries = Vec::new();
    let mut max_index = 0usize;
    let mut array_only = !has_empty_dict_metatable(lua, table).map_err(mlua::Error::external)?;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        if let Value::Integer(index) = key {
            if let Ok(index) = usize::try_from(index) {
                if index > 0 {
                    max_index = max_index.max(index);
                    entries.push((Value::Integer(index as i64), value));
                    continue;
                }
            }
        }
        array_only = false;
        entries.push((key, value));
    }

    let result = if array_only && max_index == entries.len() {
        let mut values = vec![MpackValue::Nil; max_index];
        for (key, value) in &entries {
            let Value::Integer(index) = key else {
                continue;
            };
            values[*index as usize - 1] = lua_to_mpack(lua, value, depth + 1, active)?;
        }
        MpackValue::Array(values)
    } else {
        let mut values = Vec::with_capacity(entries.len());
        for (key, value) in &entries {
            values.push((
                lua_to_mpack(lua, key, depth + 1, active)?,
                lua_to_mpack(lua, value, depth + 1, active)?,
            ));
        }
        MpackValue::Map(values)
    };
    active.remove(&pointer);
    Ok(result)
}

fn mpack_to_lua(lua: &Lua, value: MpackValue, depth: usize) -> mlua::Result<Value> {
    if depth > RECURSION_LIMIT {
        return Err(mlua::Error::runtime("msgpack object is too deeply nested"));
    }
    match value {
        MpackValue::Nil => vim_nil(lua),
        MpackValue::Boolean(value) => Ok(Value::Boolean(value)),
        MpackValue::Integer(value) => {
            if let Some(value) = value.as_i64() {
                Ok(Value::Integer(value))
            } else if let Some(value) = value.as_u64() {
                if value <= i64::MAX as u64 {
                    Ok(Value::Integer(value as i64))
                } else {
                    Ok(Value::Number(value as f64))
                }
            } else {
                Err(mlua::Error::runtime("invalid msgpack integer"))
            }
        }
        MpackValue::F32(value) => Ok(Value::Number(f64::from(value))),
        MpackValue::F64(value) => Ok(Value::Number(value)),
        MpackValue::String(value) => Ok(Value::String(lua.create_string(value.as_bytes())?)),
        MpackValue::Binary(value) => Ok(Value::String(lua.create_string(value)?)),
        MpackValue::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            for (index, value) in values.into_iter().enumerate() {
                table.raw_set(index + 1, mpack_to_lua(lua, value, depth + 1)?)?;
            }
            Ok(Value::Table(table))
        }
        MpackValue::Map(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            if values.is_empty() {
                set_empty_dict_metatable(lua, &table)?;
            }
            for (key, value) in values {
                let key = mpack_to_lua(lua, key, depth + 1)?;
                if matches!(key, Value::Nil) || is_vim_nil(lua, &key).map_err(mlua::Error::external)? {
                    return Err(mlua::Error::runtime("msgpack map contains nil key"));
                }
                table.raw_set(key, mpack_to_lua(lua, value, depth + 1)?)?;
            }
            Ok(Value::Table(table))
        }
        MpackValue::Ext(kind, data) => {
            Ok(Value::UserData(lua.create_userdata(MpackExt { kind, data })?))
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
