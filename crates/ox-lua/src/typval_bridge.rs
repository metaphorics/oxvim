//! Bridge between Vimscript [`Typval`] values and Lua values.

use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;

use mlua::{FromLua, Lua, Table, Value};
use ox_types::{DictEntry, Funcref, OxStr, Special, Typval};

use crate::converter::{
    CONVERSION_RECURSION_LIMIT, ConversionError, has_empty_dict_metatable, i64_to_f64, is_vim_nil,
    lua_to_object, object_to_lua, object_to_lua_not_special, u64_to_f64,
};

/// Convert a Vimscript value to the representation used by the Lua executor.
///
/// # Errors
///
/// Returns an error when the nesting limit is exceeded, a container is
/// mutably borrowed elsewhere, a Lua registry reference is out of range, or
/// Lua rejects a required operation.
pub fn typval_to_lua(lua: &Lua, value: &Typval) -> Result<Value, ConversionError> {
    typval_to_lua_inner(lua, value, 0, &mut HashMap::new())
}

fn typval_to_lua_inner(
    lua: &Lua,
    value: &Typval,
    depth: usize,
    containers: &mut HashMap<(*const c_void, u8), Table>,
) -> Result<Value, ConversionError> {
    if depth > CONVERSION_RECURSION_LIMIT {
        return Err(ConversionError::RecursionLimit {
            limit: CONVERSION_RECURSION_LIMIT,
        });
    }

    Ok(match value {
        Typval::Number(value) => Value::Number(i64_to_f64(*value)),
        Typval::Float(value) => Value::Number(*value),
        Typval::String(value) => Value::String(lua.create_string(value.as_bytes())?),
        Typval::Blob(value) => Value::String(lua.create_string(value)?),
        Typval::Bool(value) => Value::Boolean(*value),
        Typval::Special(Special::Null) => object_to_lua_not_special(lua, &ox_types::Object::Nil)?,
        Typval::Channel(value) | Typval::Job(value) => Value::Number(u64_to_f64(*value)),
        Typval::List(list) => {
            let key = (Rc::as_ptr(list).cast::<c_void>(), ox_types::VAR_LIST);
            if let Some(table) = containers.get(&key) {
                return Ok(Value::Table(table.clone()));
            }
            let items = list
                .try_borrow()
                .map_err(|_| ConversionError::UnsupportedType("borrowed Vimscript List"))?
                .items
                .clone();
            let table = lua.create_table_with_capacity(items.len(), 0)?;
            containers.insert(key, table.clone());
            for (offset, item) in items.iter().enumerate() {
                table.raw_set(
                    offset + 1,
                    typval_to_lua_inner(lua, item, depth + 1, containers)?,
                )?;
            }
            Value::Table(table)
        }
        Typval::Dict(dict) => {
            let key = (Rc::as_ptr(dict).cast::<c_void>(), ox_types::VAR_DICT);
            if let Some(table) = containers.get(&key) {
                return Ok(Value::Table(table.clone()));
            }
            let entries = dict
                .try_borrow()
                .map_err(|_| ConversionError::UnsupportedType("borrowed Vimscript Dictionary"))?
                .entries
                .clone();
            let table = if entries.is_empty() {
                let Value::Table(table) =
                    object_to_lua(lua, &ox_types::Object::Dict(ox_types::Dict(Vec::new())))?
                else {
                    return Err(ConversionError::UnsupportedType(
                        "empty Vimscript Dictionary",
                    ));
                };
                table
            } else {
                lua.create_table_with_capacity(0, entries.len())?
            };
            containers.insert(key, table.clone());
            for entry in &entries {
                table.raw_set(
                    lua.create_string(entry.key.as_bytes())?,
                    typval_to_lua_inner(lua, &entry.value, depth + 1, containers)?,
                )?;
            }
            Value::Table(table)
        }
        Typval::Funcref(funcref) | Typval::Partial(funcref) => {
            if let Some(reference) = funcref.registry {
                let reference = i32::try_from(reference)
                    .map_err(|_| ConversionError::MissingLuaRef(i32::MAX))?;
                object_to_lua(lua, &ox_types::Object::LuaRef(reference))?
            } else {
                Value::Nil
            }
        }
    })
}
/// Convert a Lua executor value to a Vimscript [`Typval`].
///
/// # Errors
///
/// Returns an error when a table mixes integer and string keys or cannot
/// represent a List or Dictionary, the nesting limit is exceeded, a container
/// is mutably borrowed elsewhere, or Lua rejects a required operation.
pub fn lua_to_typval(lua: &Lua, value: &Value) -> Result<Typval, ConversionError> {
    lua_to_typval_inner(lua, value, 0, &mut HashMap::new())
}

/// Collect every Lua reference id stored while converting `value` (the
/// function references of `Funcref` typvals, including those nested in lists,
/// dictionaries, and bound arguments).
pub fn collect_typval_refs(value: &Typval, out: &mut Vec<i32>) {
    match value {
        Typval::List(items) => items
            .borrow()
            .items
            .iter()
            .for_each(|item| collect_typval_refs(item, out)),
        Typval::Dict(pairs) => pairs
            .borrow()
            .entries
            .iter()
            .for_each(|entry| collect_typval_refs(&entry.value, out)),
        Typval::Funcref(funcref) | Typval::Partial(funcref) => {
            funcref
                .args
                .iter()
                .for_each(|arg| collect_typval_refs(arg, out));
            if let Some(Ok(reference)) = funcref.registry.map(i32::try_from) {
                out.push(reference);
            }
        }
        _ => {}
    }
}

/// Release every Lua reference id collected from a converted [`Typval`].
pub fn free_typval_refs(lua: &Lua, refs: &[i32]) {
    for reference in refs {
        let _ = crate::converter::free_lua_ref(lua, *reference);
    }
}

fn lua_to_typval_inner(
    lua: &Lua,
    value: &Value,
    depth: usize,
    containers: &mut HashMap<*const c_void, Typval>,
) -> Result<Typval, ConversionError> {
    if depth > CONVERSION_RECURSION_LIMIT {
        return Err(ConversionError::RecursionLimit {
            limit: CONVERSION_RECURSION_LIMIT,
        });
    }

    match value {
        Value::Nil => Ok(Typval::Special(Special::Null)),
        Value::Boolean(value) => Ok(Typval::Bool(*value)),
        Value::Integer(value) => Ok(Typval::Number(*value)),
        Value::Number(value) => {
            let Ok(integer) = i64::from_lua(Value::Number(*value), lua) else {
                return Ok(Typval::Float(*value));
            };
            let integer_as_number = i64_to_f64(integer);
            if integer_as_number.to_bits() == value.to_bits()
                || (integer == 0 && value.to_bits() == (-0.0_f64).to_bits())
            {
                Ok(Typval::Number(integer))
            } else {
                Ok(Typval::Float(*value))
            }
        }
        Value::String(value) => Ok(Typval::String(OxStr(value.as_bytes().to_vec()))),
        Value::Table(table) => table_to_typval(lua, table, depth, containers),
        Value::Function(_) => {
            let ox_types::Object::LuaRef(reference) = lua_to_object(lua, value)? else {
                return Err(ConversionError::UnsupportedType("function"));
            };
            let Ok(registry) = usize::try_from(reference) else {
                return Err(ConversionError::MissingLuaRef(i32::MAX));
            };
            Ok(Typval::Funcref(Funcref {
                name: OxStr::from(""),
                args: Vec::new(),
                dict: None,
                registry: Some(registry),
            }))
        }
        Value::UserData(_) if is_vim_nil(lua, value)? => Ok(Typval::Special(Special::Null)),
        other => Err(ConversionError::UnsupportedType(other.type_name())),
    }
}

fn table_to_typval(
    lua: &Lua,
    table: &Table,
    depth: usize,
    containers: &mut HashMap<*const c_void, Typval>,
) -> Result<Typval, ConversionError> {
    let pointer = table.to_pointer();
    if let Some(value) = containers.get(&pointer) {
        return Ok(value.clone());
    }

    let mut numeric = Vec::new();
    let mut strings = Vec::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        match key {
            Value::Integer(index) if index > 0 => {
                let Ok(index) = usize::try_from(index) else {
                    return Err(ConversionError::InvalidTable);
                };
                numeric.push((index, value));
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
                numeric.push((integer, value));
            }
            Value::String(name) => strings.push((OxStr(name.as_bytes().to_vec()), value)),
            _ => return Err(ConversionError::InvalidTable),
        }
    }
    if !numeric.is_empty() && !strings.is_empty() {
        return Err(ConversionError::InvalidTable);
    }

    if strings.is_empty() && (!numeric.is_empty() || !has_empty_dict_metatable(lua, table)?) {
        numeric.sort_unstable_by_key(|(index, _)| *index);
        let length = numeric.last().map_or(0, |(index, _)| *index);
        let result = Typval::list(Vec::new());
        containers.insert(pointer, result.clone());
        let mut items = vec![Typval::Special(Special::Null); length];
        for (index, value) in numeric {
            items[index - 1] = lua_to_typval_inner(lua, &value, depth + 1, containers)?;
        }
        let Typval::List(list) = &result else {
            return Err(ConversionError::UnsupportedType("Vimscript List"));
        };
        list.try_borrow_mut()
            .map_err(|_| ConversionError::UnsupportedType("borrowed Vimscript List"))?
            .items = items;
        return Ok(result);
    }

    let result = Typval::dict(Vec::new());
    containers.insert(pointer, result.clone());
    let mut entries = Vec::with_capacity(strings.len());
    for (name, value) in strings {
        entries.push(DictEntry::new(
            name,
            lua_to_typval_inner(lua, &value, depth + 1, containers)?,
        ));
    }
    let Typval::Dict(dict) = &result else {
        return Err(ConversionError::UnsupportedType("Vimscript Dictionary"));
    };
    dict.try_borrow_mut()
        .map_err(|_| ConversionError::UnsupportedType("borrowed Vimscript Dictionary"))?
        .entries = entries;
    Ok(result)
}
