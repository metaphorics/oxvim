//! Conversion between the API [`Object`] model and Lua values.

use std::collections::HashSet;
use std::ffi::c_void;

use mlua::{Lua, Table, Value};
use ox_types::{Dict, Object, OxStr};
use thiserror::Error;

/// Maximum container nesting accepted by the Rust converter.
///
/// Upstream uses an explicit heap work stack (`converter.c:1064-1203`) rather
/// than C recursion. This cap gives the recursive Rust implementation the same
/// stack-safety property at its public boundary.
pub const CONVERSION_RECURSION_LIMIT: usize = 100;

const LUA_REFS_REGISTRY_KEY: &str = "ox-lua.refs";
const NEXT_REF_KEY: &str = "__next";

/// Failure converting between Lua and the API object model.
#[derive(Debug, Error)]
pub enum ConversionError {
    /// mlua rejected an operation.
    #[error(transparent)]
    Lua(#[from] mlua::Error),
    /// A value exceeded the conversion nesting cap.
    #[error("Lua conversion exceeded recursion limit {limit}")]
    RecursionLimit {
        /// Configured nesting cap.
        limit: usize,
    },
    /// A table mixed keys or contained holes and could not represent an Object.
    #[error("cannot convert Lua table: expected contiguous integer keys or only string keys")]
    InvalidTable,
    /// A Lua value has no API Object representation.
    #[error("cannot convert Lua {0}")]
    UnsupportedType(&'static str),
    /// An Object referred to an unknown Lua registry entry.
    #[error("unknown LuaRef {0}")]
    MissingLuaRef(i32),
}

/// Convert one Lua value to an API [`Object`].
pub fn lua_to_object(lua: &Lua, value: &Value) -> Result<Object, ConversionError> {
    lua_to_object_inner(lua, value, 0, &mut HashSet::new())
}

fn lua_to_object_inner(
    lua: &Lua,
    value: &Value,
    depth: usize,
    active: &mut HashSet<*const c_void>,
) -> Result<Object, ConversionError> {
    if depth > CONVERSION_RECURSION_LIMIT {
        return Err(ConversionError::RecursionLimit { limit: CONVERSION_RECURSION_LIMIT });
    }

    match value {
        Value::Nil => Ok(Object::Nil),
        Value::Boolean(value) => Ok(Object::Boolean(*value)),
        Value::Integer(value) => Ok(Object::Integer(*value)),
        Value::Number(value) => {
            if *value >= i64::MAX as f64 || *value < i64::MIN as f64 {
                return Ok(Object::Float(*value));
            }
            let integer = *value as i64;
            if (integer as f64) == *value {
                Ok(Object::Integer(integer))
            } else {
                Ok(Object::Float(*value))
            }
        }
        Value::String(value) => Ok(Object::String(OxStr(value.as_bytes().to_vec()))),
        Value::Table(table) => table_to_object(lua, table, depth, active),
        Value::Function(_) | Value::UserData(_) => {
            if is_vim_nil(lua, value)? {
                Ok(Object::Nil)
            } else {
                Ok(Object::LuaRef(store_lua_ref(lua, value.clone())?))
            }
        }
        other => Err(ConversionError::UnsupportedType(other.type_name())),
    }
}

fn table_to_object(
    lua: &Lua,
    table: &Table,
    depth: usize,
    active: &mut HashSet<*const c_void>,
) -> Result<Object, ConversionError> {
    let pointer = table.to_pointer();
    if !active.insert(pointer) {
        return Err(ConversionError::RecursionLimit { limit: CONVERSION_RECURSION_LIMIT });
    }

    let result = classify_and_convert_table(lua, table, depth, active);
    active.remove(&pointer);
    result
}

fn classify_and_convert_table(
    lua: &Lua,
    table: &Table,
    depth: usize,
    active: &mut HashSet<*const c_void>,
) -> Result<Object, ConversionError> {
    let mut integer_entries = Vec::new();
    let mut string_entries = Vec::new();

    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        match key {
            Value::Integer(index) if index > 0 => integer_entries.push((index as usize, value)),
            Value::Number(index)
                if index > 0.0 && index <= usize::MAX as f64 && index.trunc() == index =>
            {
                integer_entries.push((index as usize, value));
            }
            Value::String(key) => string_entries.push((OxStr(key.as_bytes().to_vec()), value)),
            _ => return Err(ConversionError::InvalidTable),
        }
    }

    if integer_entries.is_empty() && string_entries.is_empty() {
        return if has_empty_dict_metatable(lua, table)? {
            Ok(Object::Dict(Dict(Vec::new())))
        } else {
            Ok(Object::Array(Vec::new()))
        };
    }
    if !integer_entries.is_empty() && !string_entries.is_empty() {
        return Err(ConversionError::InvalidTable);
    }
    if !string_entries.is_empty() {
        let mut entries = Vec::with_capacity(string_entries.len());
        for (key, value) in string_entries {
            entries.push((key, lua_to_object_inner(lua, &value, depth + 1, active)?));
        }
        return Ok(Object::Dict(Dict(entries)));
    }

    integer_entries.sort_unstable_by_key(|(index, _)| *index);
    let length = integer_entries.last().map_or(0, |(index, _)| *index);
    let mut values = vec![Object::Nil; length];
    for (index, value) in integer_entries {
        values[index - 1] = lua_to_object_inner(lua, &value, depth + 1, active)?;
    }
    Ok(Object::Array(values))
}

/// Convert one API [`Object`] to a Lua value.
pub fn object_to_lua(lua: &Lua, object: &Object) -> Result<Value, ConversionError> {
    object_to_lua_inner(lua, object, 0)
}

fn object_to_lua_inner(lua: &Lua, object: &Object, depth: usize) -> Result<Value, ConversionError> {
    if depth > CONVERSION_RECURSION_LIMIT {
        return Err(ConversionError::RecursionLimit { limit: CONVERSION_RECURSION_LIMIT });
    }

    Ok(match object {
        Object::Nil => vim_nil(lua)?,
        Object::Boolean(value) => Value::Boolean(*value),
        // Neovim uses lua_pushnumber, so LuaJIT observes an IEEE-754 number.
        Object::Integer(value) => Value::Number(*value as f64),
        Object::Float(value) => Value::Number(*value),
        Object::String(value) => Value::String(lua.create_string(value.as_bytes())?),
        Object::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            for (offset, value) in values.iter().enumerate() {
                table.raw_set(offset + 1, object_to_lua_inner(lua, value, depth + 1)?)?;
            }
            Value::Table(table)
        }
        Object::Dict(values) => {
            let table = lua.create_table_with_capacity(0, values.0.len())?;
            if values.0.is_empty() {
                table.set_metatable(Some(empty_dict_metatable(lua)?))?;
            }
            for (key, value) in &values.0 {
                table.raw_set(
                    lua.create_string(key.as_bytes())?,
                    object_to_lua_inner(lua, value, depth + 1)?,
                )?;
            }
            Value::Table(table)
        }
        Object::LuaRef(reference) => load_lua_ref(lua, *reference)?,
        Object::Buffer(handle) => Value::Number(i64::from(*handle) as f64),
        Object::Window(handle) => Value::Number(i64::from(*handle) as f64),
        Object::Tabpage(handle) => Value::Number(i64::from(*handle) as f64),
    })
}

fn vim_table(lua: &Lua) -> Result<Table, ConversionError> {
    Ok(lua.globals().get("vim")?)
}

fn vim_nil(lua: &Lua) -> Result<Value, ConversionError> {
    Ok(vim_table(lua)?.get("NIL")?)
}

fn empty_dict_metatable(lua: &Lua) -> Result<Table, ConversionError> {
    Ok(vim_table(lua)?.get("_empty_dict_mt")?)
}

pub(crate) fn is_vim_nil(lua: &Lua, value: &Value) -> Result<bool, ConversionError> {
    let nil = vim_nil(lua)?;
    Ok(value.equals(&nil)?)
}

pub(crate) fn has_empty_dict_metatable(
    lua: &Lua,
    table: &Table,
) -> Result<bool, ConversionError> {
    let Some(metatable) = table.metatable() else {
        return Ok(false);
    };
    Ok(metatable.to_pointer() == empty_dict_metatable(lua)?.to_pointer())
}

fn lua_refs(lua: &Lua) -> Result<Table, ConversionError> {
    match lua.named_registry_value(LUA_REFS_REGISTRY_KEY) {
        Ok(table) => Ok(table),
        Err(_) => {
            let table = lua.create_table()?;
            table.raw_set(NEXT_REF_KEY, 1_i32)?;
            lua.set_named_registry_value(LUA_REFS_REGISTRY_KEY, table.clone())?;
            Ok(table)
        }
    }
}

fn store_lua_ref(lua: &Lua, value: Value) -> Result<i32, ConversionError> {
    let refs = lua_refs(lua)?;
    let next: i32 = refs.raw_get(NEXT_REF_KEY)?;
    refs.raw_set(next, value)?;
    refs.raw_set(NEXT_REF_KEY, next.checked_add(1).ok_or(ConversionError::MissingLuaRef(next))?)?;
    Ok(next)
}

fn load_lua_ref(lua: &Lua, reference: i32) -> Result<Value, ConversionError> {
    let value: Value = lua_refs(lua)?.raw_get(reference)?;
    if matches!(value, Value::Nil) {
        Err(ConversionError::MissingLuaRef(reference))
    } else {
        Ok(value)
    }
}
