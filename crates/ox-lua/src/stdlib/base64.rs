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
        output.push(
            ALPHABET[usize::from((chunk[0] & 0x03) << 4 | chunk.get(1).copied().unwrap_or(0) >> 4)],
        );
        if let Some(second) = chunk.get(1) {
            output.push(
                ALPHABET
                    [usize::from((second & 0x0f) << 2 | chunk.get(2).copied().unwrap_or(0) >> 6)],
            );
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
    let (chunks, _) = input.as_chunks::<4>();
    for (index, &[first, second, third, fourth]) in chunks.iter().enumerate() {
        let last = index + 1 == chunks.len();
        let a = sextet(first)?;
        let b = sextet(second)?;
        let c = if third == b'=' {
            if !last || fourth != b'=' {
                return Err(mlua::Error::runtime("invalid base64 data"));
            }
            0
        } else {
            sextet(third)?
        };
        let d = if fourth == b'=' {
            if !last {
                return Err(mlua::Error::runtime("invalid base64 data"));
            }
            0
        } else {
            sextet(fourth)?
        };

        if third == b'=' && b & 0x0f != 0 || fourth == b'=' && third != b'=' && c & 0x03 != 0 {
            return Err(mlua::Error::runtime("invalid base64 data"));
        }
        output.push(a << 2 | b >> 4);
        if third != b'=' {
            output.push(b << 4 | c >> 2);
        }
        if fourth != b'=' {
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
