use std::collections::{HashMap, HashSet};
use std::io::{Cursor, ErrorKind};

use mlua::{AnyUserData, Function, Lua, LuaString, MetaMethod, Table, UserData, UserDataMethods, Value};
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

#[derive(Debug, Default)]
struct Packer {
    ext: Option<Table>,
    is_bin: Option<IsBin>,
}

#[derive(Clone, Debug)]
enum IsBin {
    Boolean(bool),
    Function(Function),
}

impl UserData for Packer {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Call, |lua, this, value: Value| {
            let value = lua_to_mpack(lua, &value, 0, &mut HashSet::new(), Some(this))?;
            let mut output = Vec::new();
            rmpv::encode::write_value(&mut output, &value).map_err(mlua::Error::external)?;
            lua.create_string(output)
        });
    }
}

#[derive(Debug)]
struct Unpacker {
    pending: Vec<u8>,
    ext: Table,
}

impl Unpacker {
    fn decode_next(
        &mut self,
        input: &[u8],
        start: usize,
    ) -> mlua::Result<(Option<MpackValue>, usize)> {
        if start == 0 || start > input.len() {
            return Err(mlua::Error::runtime(
                "start position must be between 1 and the input string length",
            ));
        }
        let chunk = &input[start - 1..];
        let previous_len = self.pending.len();
        let mut combined = Vec::new();
        let bytes = if previous_len == 0 {
            chunk
        } else {
            combined.reserve(previous_len + chunk.len());
            combined.extend_from_slice(&self.pending);
            combined.extend_from_slice(chunk);
            combined.as_slice()
        };
        let mut cursor = Cursor::new(bytes);
        match rmpv::decode::read_value(&mut cursor) {
            Ok(value) => {
                let consumed = usize::try_from(cursor.position())
                    .map_err(mlua::Error::external)?
                    .saturating_sub(previous_len);
                self.pending.clear();
                Ok((Some(value), start + consumed))
            }
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                if previous_len == 0 {
                    self.pending.extend_from_slice(chunk);
                } else {
                    self.pending = combined;
                }
                Ok((None, input.len() + 1))
            }
            Err(error) => {
                self.pending.clear();
                Err(mlua::Error::runtime(format!("invalid msgpack: {error}")))
            }
        }
    }
}

impl UserData for Unpacker {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method_mut(
            MetaMethod::Call,
            |lua, this, (input, start): (LuaString, Option<usize>)| {
                let input = input.as_bytes();
                let (value, next) = this.decode_next(&input, start.unwrap_or(1))?;
                Ok((
                    match value {
                        Some(value) => mpack_to_lua_with_ext(lua, value, 0, Some(&this.ext))?,
                        None => Value::Nil,
                    },
                    next,
                ))
            },
        );
    }
}

#[derive(Debug)]
struct Session {
    unpacker: AnyUserData,
    pending: HashMap<u32, Value>,
    next_id: u32,
}

impl UserData for Session {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("request", |lua, this, data: Option<Value>| {
            let first = this.next_id;
            while this.pending.contains_key(&this.next_id) {
                this.next_id = next_request_id(this.next_id);
                if this.next_id == first {
                    return Err(mlua::Error::runtime("no free msgpack-rpc request ids"));
                }
            }
            let id = this.next_id;
            this.next_id = next_request_id(id);
            this.pending.insert(id, data.unwrap_or(Value::Nil));
            lua.create_string(rpc_prefix(4, 0, Some(id))?)
        });
        methods.add_method("reply", |lua, _, id: u32| {
            lua.create_string(rpc_prefix(4, 1, Some(id))?)
        });
        methods.add_method("notify", |lua, _, ()| {
            lua.create_string(rpc_prefix(3, 2, None)?)
        });
        methods.add_method_mut(
            "receive",
            |lua, this, (input, start): (LuaString, Option<usize>)| {
                let input = input.as_bytes();
                let mut unpacker = this.unpacker.borrow_mut::<Unpacker>()?;
                let (value, next) = unpacker.decode_next(&input, start.unwrap_or(1))?;
                let Some(MpackValue::Array(mut fields)) = value else {
                    if value.is_none() {
                        return Ok((Value::Nil, Value::Nil, Value::Nil, Value::Nil, next));
                    }
                    return Err(mlua::Error::runtime("invalid msgpack-rpc string"));
                };
                if fields.is_empty() {
                    return Err(mlua::Error::runtime("invalid msgpack-rpc string"));
                }
                let kind = rpc_u32(&fields[0])?;
                let expected = if kind == 2 { 3 } else { 4 };
                if fields.len() != expected {
                    return Err(mlua::Error::runtime("invalid msgpack-rpc string"));
                }
                let ext = Some(&unpacker.ext);
                match kind {
                    0 => {
                        let id = rpc_u32(&fields[1])?;
                        let method = mpack_to_lua_with_ext(lua, fields.remove(2), 0, ext)?;
                        let args = mpack_to_lua_with_ext(lua, fields.remove(2), 0, ext)?;
                        Ok((Value::String(lua.create_string("request")?), Value::Integer(i64::from(id)), method, args, next))
                    }
                    1 => {
                        let id = rpc_u32(&fields[1])?;
                        let callback = this.pending.remove(&id).ok_or_else(|| {
                            mlua::Error::runtime("invalid msgpack-rpc response id")
                        })?;
                        let error = mpack_to_lua_with_ext(lua, fields.remove(2), 0, ext)?;
                        let result = mpack_to_lua_with_ext(lua, fields.remove(2), 0, ext)?;
                        Ok((Value::String(lua.create_string("response")?), callback, error, result, next))
                    }
                    2 => {
                        let method = mpack_to_lua_with_ext(lua, fields.remove(1), 0, ext)?;
                        let args = mpack_to_lua_with_ext(lua, fields.remove(1), 0, ext)?;
                        Ok((Value::String(lua.create_string("notification")?), Value::Nil, method, args, next))
                    }
                    _ => Err(mlua::Error::runtime("invalid msgpack-rpc string")),
                }
            },
        );
    }
}

pub(super) fn install(lua: &Lua, vim: &Table) -> mlua::Result<()> {
    let module = lua.create_table()?;
    module.set(
        "Packer",
        lua.create_function(|lua, options: Option<Table>| {
            let ext = option_table(lua, options.as_ref(), "ext")?;
            let is_bin = match options.as_ref().map(|options| options.get::<Value>("is_bin")).transpose()? {
                None | Some(Value::Nil) => None,
                Some(Value::Boolean(value)) => Some(IsBin::Boolean(value)),
                Some(Value::Function(value)) => Some(IsBin::Function(value)),
                Some(_) => {
                    return Err(mlua::Error::runtime(
                        "\"is_bin\" option must be a boolean or function",
                    ));
                }
            };
            lua.create_userdata(Packer { ext, is_bin })
        })?,
    )?;
    module.set(
        "Unpacker",
        lua.create_function(|lua, options: Option<Table>| {
            let ext = option_table(lua, options.as_ref(), "ext")?
                .unwrap_or(lua.create_table()?);
            lua.create_userdata(Unpacker { pending: Vec::new(), ext })
        })?,
    )?;
    module.set(
        "encode",
        lua.create_function(|lua, value: Value| {
            let mut active = HashSet::new();
            let value = lua_to_mpack(lua, &value, 0, &mut active, None)?;
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
    module.set(
        "Session",
        lua.create_function(|lua, options: Option<Table>| {
            let unpacker: AnyUserData = options
                .ok_or_else(|| mlua::Error::runtime("expecting a table argument"))?
                .get("unpack")?;
            unpacker.borrow::<Unpacker>().map_err(|_| {
                mlua::Error::runtime("\"unpack\" option must be a mpack.Unpacker instance")
            })?;
            lua.create_userdata(Session { unpacker, pending: HashMap::new(), next_id: 0 })
        })?,
    )?;
    vim.set("mpack", module.clone())?;
    let package: Table = lua.globals().get("package")?;
    let loaded: Table = package.get("loaded")?;
    loaded.set("mpack", module)
}

fn rpc_prefix(len: u32, kind: u32, id: Option<u32>) -> mlua::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(16);
    output.push(0x90 | u8::try_from(len).map_err(mlua::Error::external)?);
    rmpv::encode::write_value(&mut output, &MpackValue::from(kind))
        .map_err(mlua::Error::external)?;
    if let Some(id) = id {
        rmpv::encode::write_value(&mut output, &MpackValue::from(id))
            .map_err(mlua::Error::external)?;
    }
    Ok(output)
}

fn next_request_id(id: u32) -> u32 {
    if id == u32::MAX - 1 { 0 } else { id + 1 }
}

fn rpc_u32(value: &MpackValue) -> mlua::Result<u32> {
    value.as_u64().and_then(|value| u32::try_from(value).ok()).ok_or_else(|| {
        mlua::Error::runtime("invalid msgpack-rpc string")
    })
}

fn lua_to_mpack(
    lua: &Lua,
    value: &Value,
    depth: usize,
    active: &mut HashSet<usize>,
    packer: Option<&Packer>,
) -> mlua::Result<MpackValue> {
    if depth > RECURSION_LIMIT {
        return Err(mlua::Error::runtime("msgpack object is too deeply nested"));
    }
    if let (Some(packer), Value::Table(table)) = (packer, value) {
        if let (Some(ext), Some(metatable)) = (&packer.ext, table.metatable()) {
            if let Value::Function(handler) = ext.raw_get::<Value>(metatable)? {
                let (kind, payload): (i64, LuaString) = handler.call(value.clone())?;
                let kind = i8::try_from(kind)
                    .ok()
                    .filter(|kind| *kind >= 0)
                    .ok_or_else(|| mlua::Error::runtime("extension type must be between 0 and 127"))?;
                return Ok(MpackValue::Ext(kind, payload.as_bytes().to_vec()));
            }
        }
    }
    Ok(match value {
        Value::Nil => MpackValue::Nil,
        Value::Boolean(value) => MpackValue::Boolean(*value),
        Value::Integer(value) => MpackValue::Integer((*value).into()),
        Value::Number(value) if value.is_finite() => MpackValue::F64(*value),
        Value::Number(_) => return Err(mlua::Error::runtime("cannot encode non-finite number")),
        Value::String(value) => {
            let bytes = value.as_bytes();
            let is_bin = match packer.and_then(|packer| packer.is_bin.as_ref()) {
                Some(IsBin::Boolean(value)) => *value,
                Some(IsBin::Function(handler)) => handler.call::<bool>(value.clone())?,
                None => false,
            };
            if is_bin {
                MpackValue::Binary(bytes.to_vec())
            } else {
                match std::str::from_utf8(&bytes) {
                    Ok(text) => MpackValue::String(text.to_owned().into()),
                    Err(_) => MpackValue::Binary(bytes.to_vec()),
                }
            }
        }
        Value::Table(table) => table_to_mpack(lua, table, depth, active, packer)?,
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
    packer: Option<&Packer>,
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
            values[*index as usize - 1] = lua_to_mpack(lua, value, depth + 1, active, packer)?;
        }
        MpackValue::Array(values)
    } else {
        let mut values = Vec::with_capacity(entries.len());
        for (key, value) in &entries {
            values.push((
                lua_to_mpack(lua, key, depth + 1, active, packer)?,
                lua_to_mpack(lua, value, depth + 1, active, packer)?,
            ));
        }
        MpackValue::Map(values)
    };
    active.remove(&pointer);
    Ok(result)
}

fn mpack_to_lua(lua: &Lua, value: MpackValue, depth: usize) -> mlua::Result<Value> {
    mpack_to_lua_with_ext(lua, value, depth, None)
}

fn mpack_to_lua_with_ext(
    lua: &Lua,
    value: MpackValue,
    depth: usize,
    ext: Option<&Table>,
) -> mlua::Result<Value> {
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
                table.raw_set(index + 1, mpack_to_lua_with_ext(lua, value, depth + 1, ext)?)?;
            }
            Ok(Value::Table(table))
        }
        MpackValue::Map(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            if values.is_empty() {
                set_empty_dict_metatable(lua, &table)?;
            }
            for (key, value) in values {
                let key = mpack_to_lua_with_ext(lua, key, depth + 1, ext)?;
                if matches!(key, Value::Nil) || is_vim_nil(lua, &key).map_err(mlua::Error::external)? {
                    return Err(mlua::Error::runtime("msgpack map contains nil key"));
                }
                table.raw_set(key, mpack_to_lua_with_ext(lua, value, depth + 1, ext)?)?;
            }
            Ok(Value::Table(table))
        }
        MpackValue::Ext(kind, data) => {
            if let Some(handler) = ext
                .and_then(|handlers| handlers.raw_get::<Value>(i64::from(kind)).ok())
                .and_then(|value| match value {
                    Value::Function(handler) => Some(handler),
                    _ => None,
                })
            {
                handler.call::<Value>((i64::from(kind), lua.create_string(data)?))
            } else if ext.is_some() {
                Ok(Value::String(lua.create_string(data)?))
            } else {
                Ok(Value::UserData(lua.create_userdata(MpackExt { kind, data })?))
            }
        }
    }
}

fn option_table(lua: &Lua, options: Option<&Table>, name: &str) -> mlua::Result<Option<Table>> {
    let Some(options) = options else {
        return Ok(None);
    };
    match options.get::<Value>(name)? {
        Value::Nil => Ok(None),
        Value::Table(source) => {
            let copy = lua.create_table()?;
            for pair in source.pairs::<Value, Value>() {
                let (key, value) = pair?;
                copy.raw_set(key, value)?;
            }
            Ok(Some(copy))
        }
        _ => Err(mlua::Error::runtime(format!("\"{name}\" option must be a table"))),
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
