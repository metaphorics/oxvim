//! Conversion between the API [`Object`] model and Lua values.

use std::collections::HashSet;
use std::ffi::c_void;

use mlua::{FromLua, Lua, Table, Value};
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
const FREE_REF_KEY: &str = "__free";

const U32_NUMBER_SCALE: f64 = 4_294_967_296.0;

/// Converts a `u64` to `f64` via exact decomposed arithmetic (no lossy `as` cast).
pub(crate) fn u64_to_f64(value: u64) -> f64 {
    let bytes = value.to_le_bytes();
    let low = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let high = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    // Both halves convert exactly, leaving the addition to perform the required IEEE-754 rounding.
    f64::from(high) * U32_NUMBER_SCALE + f64::from(low)
}

/// Converts an `i64` to `f64` without lossy casts.
pub(crate) fn i64_to_f64(value: i64) -> f64 {
    let magnitude = u64_to_f64(value.unsigned_abs());
    if value < 0 { -magnitude } else { magnitude }
}

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

/// Convert one Lua value to an API [`Object`], keeping top-level tables,
/// functions, and userdata as fresh [`Object::LuaRef`] registry entries
/// (upstream `lua_to_object` with `true` last argument). Nested values inside
/// tables still convert by value; the caller owns every fresh reference and
/// releases it with [`free_lua_ref`].
///
/// # Errors
///
/// Returns [`ConversionError::Lua`] when the registry rejects a required
/// operation, or [`ConversionError::UnsupportedType`] for values with no
/// object representation.
pub fn lua_to_object_ref(lua: &Lua, value: &Value) -> Result<Object, ConversionError> {
    match value {
        Value::Table(_) | Value::Function(_) | Value::UserData(_) => {
            if is_vim_nil(lua, value)? {
                Ok(Object::Nil)
            } else {
                Ok(Object::LuaRef(store_lua_ref(lua, value.clone())?))
            }
        }
        _ => lua_to_object(lua, value),
    }
}

/// Convert one Lua value to an API [`Object`].
///
/// # Errors
///
/// Returns an error when the value is unsupported, a table cannot represent an
/// API object, the nesting limit is exceeded, or Lua rejects a required
/// operation.
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
        return Err(ConversionError::RecursionLimit {
            limit: CONVERSION_RECURSION_LIMIT,
        });
    }

    match value {
        Value::Nil => Ok(Object::Nil),
        Value::Boolean(value) => Ok(Object::Boolean(*value)),
        Value::Integer(value) => Ok(Object::Integer(*value)),
        Value::Number(value) => {
            let Ok(integer) = i64::from_lua(Value::Number(*value), lua) else {
                return Ok(Object::Float(*value));
            };
            let integer_as_number = i64_to_f64(integer);
            if integer_as_number.to_bits() == value.to_bits()
                || (integer == 0 && value.to_bits() == (-0.0_f64).to_bits())
            {
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
        return Err(ConversionError::RecursionLimit {
            limit: CONVERSION_RECURSION_LIMIT,
        });
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
    let type_tag: Value = table.raw_get(true)?;
    if matches!(type_tag, Value::Integer(3))
        || matches!(type_tag, Value::Number(value) if value.to_bits() == 3.0_f64.to_bits())
    {
        let value: Value = table.raw_get(false)?;
        return match value {
            Value::Integer(value) => Ok(Object::Float(i64_to_f64(value))),
            Value::Number(value) => Ok(Object::Float(value)),
            _ => Err(ConversionError::InvalidTable),
        };
    }
    let mut integer_entries = Vec::new();
    let mut string_entries = Vec::new();

    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        match key {
            Value::Integer(index) if index > 0 => {
                let Ok(index) = usize::try_from(index) else {
                    return Err(ConversionError::InvalidTable);
                };
                integer_entries.push((index, value));
            }
            Value::Number(index) => {
                let Ok(integer) = usize::from_lua(Value::Number(index), lua) else {
                    return Err(ConversionError::InvalidTable);
                };
                let Ok(integer_u64) = u64::try_from(integer) else {
                    return Err(ConversionError::InvalidTable);
                };
                if integer == 0 || u64_to_f64(integer_u64).to_bits() != index.to_bits() {
                    return Err(ConversionError::InvalidTable);
                }
                integer_entries.push((integer, value));
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
///
/// Mirrors upstream `nlua_push_Object` with `kNluaPushSpecial`: `Object::Nil`
/// surfaces as Lua `nil` (not `vim.NIL` userdata), so Lua `== nil` checks on
/// void API returns succeed.
///
/// # Errors
///
/// Returns an error when the nesting limit is exceeded, an object references
/// an unknown Lua registry value, or Lua rejects a required operation.
pub fn object_to_lua(lua: &Lua, object: &Object) -> Result<Value, ConversionError> {
    object_to_lua_inner(lua, object, 0, FloatRepresentation::Native, true)
}

pub(crate) fn object_to_lua_legacy(lua: &Lua, object: &Object) -> Result<Value, ConversionError> {
    object_to_lua_inner(lua, object, 0, FloatRepresentation::TypedTable, true)
}

/// Convert an API [`Object`] to a Lua value without the `kNluaPushSpecial`
/// flag, so `Object::Nil` surfaces as `vim.NIL` userdata. Used by the typval
/// bridge where Vimscript `v:null` must remain `vim.NIL` (upstream
/// `nlua_push_typval` without `kNluaPushSpecial`).
pub(crate) fn object_to_lua_not_special(
    lua: &Lua,
    object: &Object,
) -> Result<Value, ConversionError> {
    object_to_lua_inner(lua, object, 0, FloatRepresentation::Native, false)
}

#[derive(Clone, Copy)]
enum FloatRepresentation {
    Native,
    TypedTable,
}

fn object_to_lua_inner(
    lua: &Lua,
    object: &Object,
    depth: usize,
    floats: FloatRepresentation,
    push_special: bool,
) -> Result<Value, ConversionError> {
    if depth > CONVERSION_RECURSION_LIMIT {
        return Err(ConversionError::RecursionLimit {
            limit: CONVERSION_RECURSION_LIMIT,
        });
    }

    Ok(match object {
        Object::Nil => {
            if push_special {
                Value::Nil
            } else {
                vim_nil(lua)?
            }
        }
        Object::Boolean(value) => Value::Boolean(*value),
        // Neovim uses lua_pushnumber, so LuaJIT observes an IEEE-754 number.
        Object::Integer(value) => Value::Number(i64_to_f64(*value)),
        Object::Float(value) if matches!(floats, FloatRepresentation::TypedTable) => {
            let table = lua.create_table_with_capacity(0, 2)?;
            table.raw_set(false, *value)?;
            table.raw_set(true, 3)?;
            Value::Table(table)
        }
        Object::Float(value) => Value::Number(*value),
        Object::String(value) => Value::String(lua.create_string(value.as_bytes())?),
        Object::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            for (offset, value) in values.iter().enumerate() {
                table.raw_set(
                    offset + 1,
                    object_to_lua_inner(lua, value, depth + 1, floats, push_special)?,
                )?;
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
                    object_to_lua_inner(lua, value, depth + 1, floats, push_special)?,
                )?;
            }
            Value::Table(table)
        }
        Object::LuaRef(reference) => load_lua_ref(lua, *reference)?,
        Object::Buffer(handle) => Value::Number(i64_to_f64(i64::from(*handle))),
        Object::Window(handle) => Value::Number(i64_to_f64(i64::from(*handle))),
        Object::Tabpage(handle) => Value::Number(i64_to_f64(i64::from(*handle))),
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

pub(crate) fn has_empty_dict_metatable(lua: &Lua, table: &Table) -> Result<bool, ConversionError> {
    let Some(metatable) = table.metatable() else {
        return Ok(false);
    };
    Ok(metatable.to_pointer() == empty_dict_metatable(lua)?.to_pointer())
}

fn lua_refs(lua: &Lua) -> Result<Table, ConversionError> {
    if let Ok(table) = lua.named_registry_value(LUA_REFS_REGISTRY_KEY) {
        return Ok(table);
    }
    let table = lua.create_table()?;
    table.raw_set(NEXT_REF_KEY, 1_i32)?;
    table.raw_set(FREE_REF_KEY, lua.create_table()?)?;
    lua.set_named_registry_value(LUA_REFS_REGISTRY_KEY, table.clone())?;
    Ok(table)
}

fn store_lua_ref(lua: &Lua, value: Value) -> Result<i32, ConversionError> {
    let refs = lua_refs(lua)?;
    let free: Table = refs.raw_get(FREE_REF_KEY)?;
    let len = free.raw_len();
    let slot = if len > 0 {
        let slot: i32 = free.raw_get(len)?;
        free.raw_set(len, Value::Nil)?;
        Some(slot)
    } else {
        None
    };
    let next = match slot {
        Some(slot) => slot,
        None => refs.raw_get(NEXT_REF_KEY)?,
    };
    refs.raw_set(next, value)?;
    if slot.is_none() {
        refs.raw_set(
            NEXT_REF_KEY,
            next.checked_add(1)
                .ok_or(ConversionError::MissingLuaRef(next))?,
        )?;
    }
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

/// Release one stored Lua reference, mirroring upstream `nlua_unref` /
/// `api_free_luaref`.
///
/// The slot is cleared and recycled by a later [`lua_to_object`] conversion,
/// so long-running sessions do not grow the reference table without bound.
/// Releasing an already-released (or never-issued) reference is a no-op, so
/// double release is safe; exactly one owner must release each live
/// reference.
///
/// # Errors
///
/// Returns [`ConversionError::Lua`] when the Lua registry is unreachable.
pub fn free_lua_ref(lua: &Lua, reference: i32) -> Result<(), ConversionError> {
    let refs = lua_refs(lua)?;
    let existing: Value = refs.raw_get(reference)?;
    if matches!(existing, Value::Nil) {
        return Ok(());
    }
    refs.raw_set(reference, Value::Nil)?;
    let free: Table = refs.raw_get(FREE_REF_KEY)?;
    let tail = free.raw_len() + 1;
    free.raw_set(tail, reference)?;
    Ok(())
}
