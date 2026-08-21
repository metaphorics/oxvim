use mlua::{Lua, LuaString, Table};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(super) fn install(lua: &Lua, vim: &Table) -> mlua::Result<()> {
    let module = lua.create_table()?;
    module.set(
        "encode",
        lua.create_function(|lua, input: LuaString| lua.create_string(encode(&input.as_bytes())))?,
    )?;
    module.set(
        "decode",
        lua.create_function(|lua, input: LuaString| {
            let decoded = decode(&input.as_bytes())?;
            lua.create_string(decoded)
        })?,
    )?;
    vim.set("base64", module)
}

fn encode(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        output.push(ALPHABET[usize::from(chunk[0] >> 2)]);
        output.push(ALPHABET[usize::from((chunk[0] & 0x03) << 4 | chunk.get(1).copied().unwrap_or(0) >> 4)]);
        if let Some(second) = chunk.get(1) {
            output.push(ALPHABET[usize::from((second & 0x0f) << 2 | chunk.get(2).copied().unwrap_or(0) >> 6)]);
        } else {
            output.push(b'=');
        }
        if let Some(third) = chunk.get(2) {
            output.push(ALPHABET[usize::from(third & 0x3f)]);
        } else {
            output.push(b'=');
        }
    }
    output
}

fn decode(input: &[u8]) -> mlua::Result<Vec<u8>> {
    if !input.len().is_multiple_of(4) {
        return Err(mlua::Error::runtime("invalid base64 data"));
    }

    let padding = input.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 {
        return Err(mlua::Error::runtime("invalid base64 data"));
    }
    let mut output = Vec::with_capacity(input.len() / 4 * 3 - padding);
    for (index, chunk) in input.chunks_exact(4).enumerate() {
        let last = index + 1 == input.len() / 4;
        let a = sextet(chunk[0])?;
        let b = sextet(chunk[1])?;
        let c = if chunk[2] == b'=' {
            if !last || chunk[3] != b'=' {
                return Err(mlua::Error::runtime("invalid base64 data"));
            }
            0
        } else {
            sextet(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            if !last {
                return Err(mlua::Error::runtime("invalid base64 data"));
            }
            0
        } else {
            sextet(chunk[3])?
        };

        if chunk[2] == b'=' && b & 0x0f != 0 || chunk[3] == b'=' && chunk[2] != b'=' && c & 0x03 != 0 {
            return Err(mlua::Error::runtime("invalid base64 data"));
        }
        output.push(a << 2 | b >> 4);
        if chunk[2] != b'=' {
            output.push(b << 4 | c >> 2);
        }
        if chunk[3] != b'=' {
            output.push(c << 6 | d);
        }
    }
    Ok(output)
}

fn sextet(byte: u8) -> mlua::Result<u8> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(mlua::Error::runtime("invalid base64 data")),
    }
}
