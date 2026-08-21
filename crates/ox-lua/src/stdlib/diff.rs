use mlua::{Function, Lua, LuaString, Table, Value};
use similar::{Algorithm, DiffOp, DiffTag, TextDiff};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResultType {
    Unified,
    Indices,
}

#[derive(Clone)]
struct Options {
    algorithm: Algorithm,
    context: usize,
    result_type: ResultType,
    on_hunk: Option<Function>,
}

pub(super) fn install(lua: &Lua, vim: &Table) -> mlua::Result<()> {
    let function = lua.create_function(
        |lua, (old, new, options): (LuaString, LuaString, Option<Table>)| {
            let old = checked_text(&old, "first")?;
            let new = checked_text(&new, "second")?;
            let options = parse_options(options)?;
            let mut config = TextDiff::configure();
            config.algorithm(options.algorithm);
            let diff = config.diff_lines(&old, &new);
            let hunks = change_hunks(&diff);

            if let Some(callback) = options.on_hunk {
                for (old_start, old_count, new_start, new_count) in hunks {
                    let result: Option<i64> = callback.call((
                        old_start as i64,
                        old_count as i64,
                        new_start as i64,
                        new_count as i64,
                    ))?;
                    if result.is_some_and(|value| value < 0) {
                        break;
                    }
                }
                return Ok(Value::Nil);
            }

            match options.result_type {
                ResultType::Unified => {
                    let mut output = diff.unified_diff();
                    output.context_radius(options.context);
                    Ok(Value::String(lua.create_string(output.to_string())?))
                }
                ResultType::Indices => {
                    let result = lua.create_table_with_capacity(hunks.len(), 0)?;
                    for (index, (old_start, old_count, new_start, new_count)) in
                        hunks.into_iter().enumerate()
                    {
                        let hunk = lua.create_sequence_from([
                            old_start as i64,
                            old_count as i64,
                            new_start as i64,
                            new_count as i64,
                        ])?;
                        result.raw_set(index + 1, hunk)?;
                    }
                    Ok(Value::Table(result))
                }
            }
        },
    )?;
    vim.set("diff", function.clone())?;
    let text = match vim.get::<Value>("text")? {
        Value::Table(table) => table,
        Value::Nil => {
            let table = lua.create_table()?;
            vim.set("text", table.clone())?;
            table
        }
        _ => return Err(mlua::Error::runtime("vim.text must be a table")),
    };
    text.set("diff", function)
}

fn checked_text(value: &LuaString, which: &str) -> mlua::Result<String> {
    String::from_utf8(value.as_bytes().to_vec())
        .map_err(|_| mlua::Error::runtime(format!("{which} diff input is not valid UTF-8")))
}

fn parse_options(options: Option<Table>) -> mlua::Result<Options> {
    let Some(options) = options else {
        return Ok(Options {
            algorithm: Algorithm::Myers,
            context: 0,
            result_type: ResultType::Unified,
            on_hunk: None,
        });
    };
    let algorithm = match options.get::<Option<String>>("algorithm")?.as_deref() {
        None | Some("myers" | "minimal") => Algorithm::Myers,
        Some("patience") => Algorithm::Patience,
        Some("histogram") => Algorithm::Histogram,
        Some(value) => {
            return Err(mlua::Error::runtime(format!(
                "invalid diff algorithm: {value}"
            )))
        }
    };
    let context = options.get::<Option<i64>>("ctxlen")?.unwrap_or(0);
    let context = usize::try_from(context)
        .map_err(|_| mlua::Error::runtime("ctxlen must be non-negative"))?;
    let result_type = match options.get::<Option<String>>("result_type")?.as_deref() {
        None | Some("unified") => ResultType::Unified,
        Some("indices") => ResultType::Indices,
        Some(value) => {
            return Err(mlua::Error::runtime(format!(
                "invalid diff result_type: {value}"
            )))
        }
    };
    Ok(Options {
        algorithm,
        context,
        result_type,
        on_hunk: options.get("on_hunk")?,
    })
}

fn change_hunks(diff: &TextDiff<'_, '_, str>) -> Vec<(usize, usize, usize, usize)> {
    diff.ops()
        .iter()
        .filter(|operation| operation.tag() != DiffTag::Equal)
        .map(hunk_from_op)
        .collect()
}

fn hunk_from_op(operation: &DiffOp) -> (usize, usize, usize, usize) {
    let old = operation.old_range();
    let new = operation.new_range();
    let old_count = old.len();
    let new_count = new.len();
    (
        if old_count == 0 { old.start } else { old.start + 1 },
        old_count,
        if new_count == 0 { new.start } else { new.start + 1 },
        new_count,
    )
}
