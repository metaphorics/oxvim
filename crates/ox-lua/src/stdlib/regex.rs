use mlua::{Function, Lua, LuaString, Table, UserData, UserDataMethods, Value};

use ox_regex::{Magic, Prog, Text};

#[derive(Clone, Debug)]
struct LuaRegex(Prog);

impl UserData for LuaRegex {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("match_str", |_, this, input: LuaString| {
            let bytes = input.as_bytes();
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| mlua::Error::runtime("regex input is not valid UTF-8"))?;
            match_span(&this.0, text)
        });
        methods.add_method(
            "match_line",
            |lua, this, (buffer, line, start, end): (i64, i64, Option<i64>, Option<i64>)| {
                if line < 0 {
                    return Err(mlua::Error::runtime("line index must be non-negative"));
                }
                let vim: Table = lua.globals().get("vim")?;
                let api: Table = vim.get("api")?;
                let get_lines: mlua::Function = api.get("nvim_buf_get_lines")?;
                let lines: Table = get_lines.call((buffer, line, line + 1, false))?;
                let line_value: LuaString = lines
                    .raw_get(1)
                    .map_err(|_| mlua::Error::runtime("line index is out of range"))?;
                let bytes = line_value.as_bytes();
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| mlua::Error::runtime("buffer line is not valid UTF-8"))?;
                let text_len = i64::try_from(text.len()).map_err(mlua::Error::external)?;
                let start = checked_offset(start.unwrap_or(0), text.len(), "start")?;
                let end = checked_offset(end.unwrap_or(text_len), text.len(), "end")?;
                if start > end {
                    return Err(mlua::Error::runtime("start must not exceed end"));
                }
                if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                    return Err(mlua::Error::runtime(
                        "regex range must use UTF-8 byte boundaries",
                    ));
                }
                match_span(&this.0, &text[start..end])
            },
        );
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, _, ()| Ok("<regex>"));
    }
}

pub(super) fn install(lua: &Lua, vim: &Table) -> mlua::Result<()> {
    // Failures return `(false, message)` and pass through the string-error
    // shim: a bad pattern must reach `pcall` as a string (upstream raises one
    // via `lua_error`), never an mlua WrappedFailure userdata.
    let native = lua.create_function(|lua, pattern: LuaString| {
        let bytes = pattern.as_bytes();
        let compiled = std::str::from_utf8(&bytes)
            .map_err(|_| "regex pattern is not valid UTF-8".to_owned())
            .and_then(|pattern| {
                ox_regex::compile(pattern, Magic::Magic)
                    .map_err(|error| format!("couldn't parse regex: {error}"))
            });
        match compiled {
            Ok(program) => Ok((true, Value::UserData(lua.create_userdata(LuaRegex(program))?))),
            Err(message) => Ok((false, Value::String(lua.create_string(message)?))),
        }
    })?;
    let wrapped: Function = crate::vim::error_shim(lua)?.call(native)?;
    vim.set("regex", wrapped)
}

fn checked_offset(value: i64, length: usize, name: &str) -> mlua::Result<usize> {
    let value = usize::try_from(value)
        .map_err(|_| mlua::Error::runtime(format!("{name} must be non-negative")))?;
    if value > length {
        Err(mlua::Error::runtime(format!("{name} is past end of line")))
    } else {
        Ok(value)
    }
}

fn match_span(program: &Prog, input: &str) -> mlua::Result<(Option<i64>, Option<i64>)> {
    let text = Text::new(input);
    let matched = ox_regex::try_exec(program, &text).map_err(mlua::Error::external)?;
    matched.map_or(Ok((None, None)), |matched| {
        Ok((
            Some(i64::try_from(matched.start.byte).map_err(mlua::Error::external)?),
            Some(i64::try_from(matched.end.byte).map_err(mlua::Error::external)?),
        ))
    })
}
