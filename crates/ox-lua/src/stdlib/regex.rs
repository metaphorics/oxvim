use mlua::{AnyUserData, Lua, LuaString, Table, UserData, UserDataMethods};
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
                let start = checked_offset(start.unwrap_or(0), text.len(), "start")?;
                let end = checked_offset(end.unwrap_or(text.len() as i64), text.len(), "end")?;
                if start > end {
                    return Err(mlua::Error::runtime("start must not exceed end"));
                }
                if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                    return Err(mlua::Error::runtime("regex range must use UTF-8 byte boundaries"));
                }
                match_span(&this.0, &text[start..end])
            },
        );
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, _, ()| Ok("<regex>"));
    }
}

pub(super) fn install(lua: &Lua, vim: &Table) -> mlua::Result<()> {
    vim.set(
        "regex",
        lua.create_function(|lua, pattern: LuaString| -> mlua::Result<AnyUserData> {
            let bytes = pattern.as_bytes();
            let pattern = std::str::from_utf8(&bytes)
                .map_err(|_| mlua::Error::runtime("regex pattern is not valid UTF-8"))?;
            let program = ox_regex::compile(pattern, Magic::Magic)
                .map_err(|error| mlua::Error::runtime(format!("couldn't parse regex: {error}")))?;
            lua.create_userdata(LuaRegex(program))
        })?,
    )
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
        Ok((Some(matched.start.byte as i64), Some(matched.end.byte as i64)))
    })
}
