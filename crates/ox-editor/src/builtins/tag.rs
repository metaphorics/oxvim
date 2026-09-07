//! Tag builtins (`taglist`, `gettagstack`, `settagstack`).

use crate::excmd_exec::ExEditorAccess;
use ox_eval::EvalError;
use ox_eval::builtin_spec;
use ox_types::{BufHandle, OxStr, Typval, WinHandle};

use crate::Editor;
use crate::excmd_exec::{EvalHost, SetLayer, option_value, typval_number};
use crate::options::OptionValue;
use crate::script::FileIO;
use crate::tags::{self, TagStackItem};

/// Routes tag builtins that need editor state.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    match name {
        "taglist" => taglist(host, args),
        "gettagstack" => host
            .access
            .with_ex_editor(|editor| gettagstack(editor, args)),
        "settagstack" => host
            .access
            .with_ex_editor(|editor| settagstack(editor, args)),
        _ => unreachable!("tag builtin route and dispatcher disagree"),
    }
}

fn taglist<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let spec = builtin_spec("taglist")
        .ok_or_else(|| EvalError::not_implemented(OxStr::from("taglist")))?;
    if args.len() < spec.min_args {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: taglist",
        ));
    }
    if spec.max_args.is_some_and(|maximum| args.len() > maximum) {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: taglist",
        ));
    }
    let pattern = match &args[0] {
        Typval::String(value) => value.to_string_lossy().into_owned(),
        Typval::Number(value) => value.to_string(),
        _ => String::new(),
    };
    if pattern.is_empty() {
        return Ok(Typval::list(Vec::new()));
    }
    let (tags_option, taglength, ignorecase) = host.access.with_ex_editor(|editor| {
        let tags_option = option_value(editor, "tags", SetLayer::Effective)
            .or_else(|| editor.options().get_global("tags").ok())
            .and_then(|value| match value {
                OptionValue::String(text) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let taglength = match option_value(editor, "taglength", SetLayer::Effective)
            .or_else(|| editor.options().get_global("taglength").ok())
        {
            Some(OptionValue::Number(value)) if *value > 0 => {
                usize::try_from(*value).unwrap_or(usize::MAX)
            }
            _ => 0,
        };
        let ignorecase = matches!(
            option_value(editor, "ignorecase", SetLayer::Effective),
            Some(OptionValue::Boolean(true))
        );
        (tags_option, taglength, ignorecase)
    });
    let matches = match tags::lookup_pattern(
        host.runtime.scripts.io(),
        &tags_option,
        &pattern,
        taglength,
        ignorecase,
    ) {
        Ok(matches) => matches,
        Err((code, message)) if code == "E431" => {
            return Err(EvalError::new(code, 0, message));
        }
        Err(_) => return Ok(Typval::list(Vec::new())),
    };

    let preferred = args
        .get(1)
        .map(crate::excmd_exec::typval_to_text)
        .filter(|name| !name.is_empty());
    let matches = tags::prefer_filename(matches, preferred.as_deref());
    let items = matches
        .into_iter()
        .map(|matched| {
            let mut entries = vec![
                (
                    OxStr::from("name"),
                    Typval::String(OxStr::from(matched.name.as_str())),
                ),
                (
                    OxStr::from("filename"),
                    Typval::String(OxStr::from(matched.filename.to_string_lossy().as_ref())),
                ),
                (
                    OxStr::from("cmd"),
                    Typval::String(OxStr::from(matched.cmd.as_str())),
                ),
            ];
            for (key, value) in matched.fields {
                if key == "file" {
                    entries.push((OxStr::from("static"), Typval::Number(1)));
                    continue;
                }
                entries.push((
                    OxStr::from(key.as_str()),
                    Typval::String(OxStr::from(value.as_str())),
                ));
            }
            Typval::dict(entries)
        })
        .collect();
    Ok(Typval::list(items))
}

fn gettagstack(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: gettagstack",
        ));
    }
    let Some(window) = window_from_nr(editor, args.first()) else {
        return Ok(Typval::dict(Vec::new()));
    };
    let Ok(stack) = editor.window_tag_stack(window) else {
        return Ok(Typval::dict(Vec::new()));
    };
    Ok(stack_to_typval(stack))
}

fn settagstack(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() < 2 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: settagstack",
        ));
    }
    if args.len() > 3 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: settagstack",
        ));
    }
    let Some(window) = window_from_nr(editor, Some(&args[0])) else {
        return Ok(Typval::Number(-1));
    };
    match &args[1] {
        Typval::Dict(_) => {}
        Typval::List(_) => {
            return Err(EvalError::new(
                "E1206",
                0,
                "Dictionary required for argument 2",
            ));
        }
        _ => return Ok(Typval::Number(-1)),
    }
    let Typval::Dict(dict) = &args[1] else {
        return Ok(Typval::Number(-1));
    };
    if args[1].is_null_dict() {
        return Ok(Typval::Number(-1));
    }
    let action = if args.len() == 3 {
        match &args[2] {
            Typval::String(text) => text.to_string_lossy().into_owned(),
            Typval::Number(_) => {
                return Err(EvalError::new("E1174", 0, "String required for argument 3"));
            }
            _ => return Err(EvalError::new("E1174", 0, "String required for argument 3")),
        }
    } else {
        "r".to_owned()
    };
    if !matches!(action.as_str(), "r" | "a" | "t") {
        return Err(EvalError::new(
            "E962",
            0,
            format!("Invalid action: '{action}'"),
        ));
    }
    let data = dict
        .try_borrow()
        .map_err(|_| EvalError::new("E698", 0, "variable nested too deep"))?;
    let mut items = None;
    let mut curidx = None;
    for entry in &data.entries {
        match entry.key.as_bytes() {
            b"items" => match &entry.value {
                Typval::List(list) => {
                    let parsed = parse_stack_items(&list.borrow().items)?;
                    items = Some(parsed);
                }
                _ => return Err(EvalError::new("E714", 0, "List required")),
            },
            b"curidx" => curidx = typval_number(&entry.value),
            _ => {}
        }
    }
    let Ok(stack) = editor.window_tag_stack_mut(window) else {
        return Ok(Typval::Number(-1));
    };
    if let Some(idx) = curidx {
        stack.set_curidx(idx);
    }
    if let Some(items) = items {
        match action.as_str() {
            "a" => stack.append(items),
            "t" => stack.truncate_and_push(items),
            _ => stack.replace(items),
        }
    } else if action == "t" {
        stack.truncate_and_push(Vec::new());
    }
    Ok(Typval::Number(0))
}

fn parse_stack_items(values: &[Typval]) -> ox_eval::Result<Vec<TagStackItem>> {
    let mut items = Vec::new();
    for value in values {
        let Typval::Dict(dict) = value else {
            continue;
        };
        let data = dict
            .try_borrow()
            .map_err(|_| EvalError::new("E698", 0, "variable nested too deep"))?;
        let mut tagname = None;
        let mut from = None;
        let mut bufnr = None;
        let mut matchnr = 1usize;
        let mut user_data = None;
        for entry in &data.entries {
            match entry.key.as_bytes() {
                b"tagname" => {
                    tagname = Some(match &entry.value {
                        Typval::String(text) => text.to_string_lossy().into_owned(),
                        other => crate::excmd_exec::typval_to_text(other),
                    });
                }
                b"from" => from = parse_from(&entry.value),
                b"bufnr" => {
                    bufnr = typval_number(&entry.value)
                        .and_then(|value| BufHandle::try_from(value).ok());
                }
                b"matchnr" => {
                    if let Some(value) = typval_number(&entry.value)
                        && value > 0
                    {
                        matchnr = usize::try_from(value).unwrap_or(usize::MAX);
                    }
                }
                b"user_data" => user_data = Some(entry.value.clone()),
                _ => {}
            }
        }
        let Some(tagname) = tagname else { continue };
        let Some((from_bufnr, from_lnum, from_col, from_off)) = from else {
            continue;
        };
        items.push(TagStackItem {
            tagname,
            from_bufnr,
            from_lnum,
            from_col,
            from_off,
            bufnr,
            matchnr,
            user_data,
        });
    }
    Ok(items)
}

fn parse_from(value: &Typval) -> Option<(BufHandle, usize, usize, i64)> {
    let Typval::List(list) = value else {
        return None;
    };
    let items = list.try_borrow().ok()?;
    if items.items.len() < 4 {
        return None;
    }
    let bufnr = typval_number(&items.items[0])?;
    let lnum = typval_number(&items.items[1])?;
    let col = typval_number(&items.items[2])?;
    let off = typval_number(&items.items[3]).unwrap_or(0);
    let bufnr = BufHandle::try_from(bufnr).ok()?;
    Some((
        bufnr,
        usize::try_from(lnum.max(0)).unwrap_or(usize::MAX),
        usize::try_from(col.max(0)).unwrap_or(usize::MAX),
        off,
    ))
}

fn stack_to_typval(stack: &tags::TagStack) -> Typval {
    let items = stack
        .items()
        .iter()
        .map(|item| {
            let mut entries = vec![
                (
                    OxStr::from("bufnr"),
                    Typval::Number(i64::from(item.from_bufnr)),
                ),
                (
                    OxStr::from("tagname"),
                    Typval::String(OxStr::from(item.tagname.as_str())),
                ),
                (
                    OxStr::from("from"),
                    Typval::list(vec![
                        Typval::Number(i64::from(item.from_bufnr)),
                        Typval::Number(i64::try_from(item.from_lnum).unwrap_or(i64::MAX)),
                        Typval::Number(i64::try_from(item.from_col).unwrap_or(i64::MAX)),
                        Typval::Number(item.from_off),
                    ]),
                ),
                (
                    OxStr::from("matchnr"),
                    Typval::Number(i64::try_from(item.matchnr).unwrap_or(i64::MAX)),
                ),
            ];
            if let Some(user_data) = &item.user_data {
                entries.push((OxStr::from("user_data"), user_data.clone()));
            }
            Typval::dict(entries)
        })
        .collect();
    Typval::dict(vec![
        (
            OxStr::from("length"),
            Typval::Number(i64::try_from(stack.len()).unwrap_or(i64::MAX)),
        ),
        (
            OxStr::from("curidx"),
            Typval::Number(i64::try_from(stack.curidx()).unwrap_or(i64::MAX)),
        ),
        (OxStr::from("items"), Typval::list(items)),
    ])
}

fn window_from_nr(editor: &Editor, value: Option<&Typval>) -> Option<WinHandle> {
    let Some(value) = value else {
        return editor.current_window();
    };
    let number = typval_number(value)?;
    if number == 0 {
        return editor.current_window();
    }
    if number < 0 {
        return None;
    }
    if let Ok(handle) = WinHandle::try_from(number)
        && editor.window(handle).is_ok()
    {
        return Some(handle);
    }
    let index = usize::try_from(number.checked_sub(1)?).ok()?;
    let tab = editor.current_tabpage()?;
    editor.tabpage_windows(tab).ok()?.get(index).copied()
}
