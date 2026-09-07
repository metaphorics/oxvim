//! Match highlighting builtins: `matchadd`, `matchaddpos`, `matchdelete`,
//! `clearmatches`, `getmatches`, `setmatches`, `matcharg`.
//!
//! Upstream `match.c` stores per-window match items. ox-editor stores them
//! in [`WindowApiState`] (layout.rs) so no `Editor` struct field is needed.

use ox_eval::{EvalError, builtin_spec};
use ox_types::{OxStr, Typval};

use crate::Editor;

use super::input_string_arg;

/// One stored match item (upstream `matchitem_T`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MatchItem {
    pub id: i64,
    pub priority: i64,
    pub group: String,
    pub pattern: Option<String>,
    pub positions: Vec<MatchPos>,
    pub conceal_char: Option<String>,
}

/// One position in a `matchaddpos` entry: `[lnum, col, len]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MatchPos {
    pub lnum: i64,
    pub col: i64,
    pub len: i64,
}

/// Routes one match builtin.
pub(crate) fn call(editor: &mut Editor, name: &str, args: &[Typval]) -> ox_eval::Result<Typval> {
    let spec = builtin_spec(name)
        .ok_or_else(|| EvalError::new("E117", 0, format!("Unknown function: {name}")))?;
    if args.len() < spec.min_args {
        return Err(EvalError::new(
            "E119",
            0,
            format!("Not enough arguments for function: {name}"),
        ));
    }
    if spec.max_args.is_some_and(|max| args.len() > max) {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    match name {
        "matchadd" => call_matchadd(editor, args),
        "matchaddpos" => call_matchaddpos(editor, args),
        "matchdelete" => call_matchdelete(editor, args),
        "clearmatches" => call_clearmatches(editor, args),
        "getmatches" => call_getmatches(editor, args),
        "setmatches" => call_setmatches(editor, args),
        "matcharg" => call_matcharg(editor, args),
        _ => unreachable!("match builtin route and dispatcher disagree"),
    }
}

/// Resolves the optional window argument (last positional) to a window handle.
fn resolve_window(
    editor: &Editor,
    args: &[Typval],
    dict_index: usize,
) -> Option<ox_types::WinHandle> {
    // The window is specified either as the last positional arg or inside a
    // dict argument at `dict_index` with key "window".
    if let Some(Typval::Dict(dict)) = args.get(dict_index)
        && let Ok(dict_ref) = dict.try_borrow()
    {
        for entry in &dict_ref.entries {
            if entry.key.as_bytes() == b"window"
                && let Typval::Number(id) = entry.value
            {
                return editor.find_window_by_id(id);
            }
        }
    }
    // Check last positional arg for a window id
    let last = args.len().saturating_sub(1);
    if last > dict_index
        && let Some(Typval::Number(id)) = args.get(last)
    {
        return editor.find_window_by_id(*id);
    }
    editor.current_window()
}

fn call_matchadd(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let group = input_string_arg(&args[0])?;
    let pattern = input_string_arg(&args[1])?;
    let priority = args.get(2).and_then(typval_number).unwrap_or(10);
    let id = args.get(3).and_then(typval_number).unwrap_or(-1);
    let group_str = group.to_string_lossy();
    let pat_str = pattern.to_string_lossy();

    if group_str.is_empty() || pat_str.is_empty() {
        return Ok(Typval::Number(-1));
    }
    if (1..=3).contains(&id) {
        return Err(EvalError::new(
            "E798",
            0,
            format!("ID is reserved for \":match\": {id}"),
        ));
    }
    let window = resolve_window(editor, args, 4)
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let conceal = args.get(4).and_then(|d| {
        if let Typval::Dict(dict) = d
            && let Ok(dict_ref) = dict.try_borrow()
        {
            for entry in &dict_ref.entries {
                if entry.key.as_bytes() == b"conceal" {
                    return input_string_arg(&entry.value)
                        .ok()
                        .map(|s| s.to_string_lossy().to_string());
                }
            }
        }
        None
    });

    let assigned_id = editor.add_match(
        window,
        MatchItem {
            id,
            priority,
            group: group_str.to_string(),
            pattern: Some(pat_str.to_string()),
            positions: Vec::new(),
            conceal_char: conceal,
        },
    );
    Ok(Typval::Number(assigned_id))
}

fn call_matchaddpos(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let group = input_string_arg(&args[0])?;
    let group_str = group.to_string_lossy();
    if group_str.is_empty() {
        return Ok(Typval::Number(-1));
    }

    let Typval::List(pos_list) = &args[1] else {
        return Err(EvalError::new("E686", 0, "List required"));
    };
    let pos_items = pos_list
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
    if pos_items.items.is_empty() {
        return Ok(Typval::Number(-1));
    }

    let priority = args.get(2).and_then(typval_number).unwrap_or(10);
    let id = args.get(3).and_then(typval_number).unwrap_or(-1);

    if id == 1 || id == 2 {
        return Err(EvalError::new(
            "E798",
            0,
            format!("ID is reserved for \"match\": {id}"),
        ));
    }
    if id == 0 || id < -1 {
        return Err(EvalError::new(
            "E799",
            0,
            format!("Invalid ID: {id} (must be greater than or equal to 1)"),
        ));
    }

    let window = resolve_window(editor, args, 4)
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;

    let mut positions = Vec::new();
    for item in &pos_items.items {
        match item {
            Typval::Number(lnum) => {
                positions.push(MatchPos {
                    lnum: *lnum,
                    col: 0,
                    len: 0,
                });
            }
            Typval::List(sub) => {
                let sub_ref = sub
                    .try_borrow()
                    .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
                if sub_ref.items.is_empty() {
                    return Err(EvalError::new("E5030", 0, "Empty list at position 0"));
                }
                let lnum = typval_number(&sub_ref.items[0]).unwrap_or(0);
                if lnum <= 0 {
                    continue;
                }
                let col = sub_ref.items.get(1).and_then(typval_number).unwrap_or(0);
                let len = sub_ref.items.get(2).and_then(typval_number).unwrap_or(1);
                positions.push(MatchPos { lnum, col, len });
            }
            Typval::Dict(_) => {
                return Err(EvalError::new("E5031", 0, "Empty list at position 0"));
            }
            _ => {
                return Err(EvalError::new("E5031", 0, "Invalid argument"));
            }
        }
    }

    let conceal = args.get(4).and_then(|d| {
        if let Typval::Dict(dict) = d
            && let Ok(dict_ref) = dict.try_borrow()
        {
            for entry in &dict_ref.entries {
                if entry.key.as_bytes() == b"conceal" {
                    return input_string_arg(&entry.value)
                        .ok()
                        .map(|s| s.to_string_lossy().to_string());
                }
            }
        }
        None
    });

    let assigned_id = editor.add_match(
        window,
        MatchItem {
            id,
            priority,
            group: group_str.to_string(),
            pattern: None,
            positions,
            conceal_char: conceal,
        },
    );
    Ok(Typval::Number(assigned_id))
}

fn call_matchdelete(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let id = typval_number(&args[0]).unwrap_or(-1);
    let window = args
        .get(1)
        .and_then(typval_number)
        .and_then(|id| editor.find_window_by_id(id))
        .or_else(|| editor.current_window())
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let result = editor.delete_match(window, id);
    Ok(Typval::Number(if result { 0 } else { -1 }))
}

fn call_clearmatches(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let window = args
        .first()
        .and_then(typval_number)
        .and_then(|id| editor.find_window_by_id(id))
        .or_else(|| editor.current_window())
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    editor.clear_matches(window);
    Ok(Typval::Number(0))
}

fn call_getmatches(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let window = args
        .first()
        .and_then(typval_number)
        .and_then(|id| editor.find_window_by_id(id))
        .or_else(|| editor.current_window())
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let items = editor.get_matches(window);
    let list: Vec<Typval> = items.iter().map(match_item_to_dict).collect();
    Ok(Typval::list(list))
}

fn call_setmatches(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let Typval::List(list_ref) = &args[0] else {
        return Ok(Typval::Number(-1));
    };
    let list = list_ref
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
    let window = args
        .get(1)
        .and_then(typval_number)
        .and_then(|id| editor.find_window_by_id(id))
        .or_else(|| editor.current_window())
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;

    let mut failed = false;
    let mut matches = Vec::new();
    for item in &list.items {
        let Typval::Dict(dict) = item else {
            failed = true;
            continue;
        };
        let dict_ref = dict
            .try_borrow()
            .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
        let group = dict_get_string(&dict_ref, "group").unwrap_or_default();
        let priority = dict_get_number(&dict_ref, "priority").unwrap_or(10);
        let id = dict_get_number(&dict_ref, "id").unwrap_or(-1);
        let conceal = dict_get_string(&dict_ref, "conceal");

        let pattern = dict_get_string(&dict_ref, "pattern");
        let mut positions = Vec::new();
        if pattern.is_none() {
            for i in 1..=8 {
                let key = format!("pos{i}");
                if let Some(Typval::List(pos_list)) = dict_get(&dict_ref, &key) {
                    if let Ok(pos_ref) = pos_list.try_borrow() {
                        let lnum = pos_ref.items.first().and_then(typval_number).unwrap_or(0);
                        let col = pos_ref.items.get(1).and_then(typval_number).unwrap_or(0);
                        let len = pos_ref.items.get(2).and_then(typval_number).unwrap_or(1);
                        positions.push(MatchPos { lnum, col, len });
                    }
                } else {
                    break;
                }
            }
        }
        matches.push(MatchItem {
            id,
            priority,
            group,
            pattern,
            positions,
            conceal_char: conceal,
        });
    }
    if failed {
        return Ok(Typval::Number(-1));
    }
    editor.clear_matches(window);
    for item in matches {
        editor.add_match(window, item);
    }
    Ok(Typval::Number(0))
}

fn call_matcharg(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let id = typval_number(&args[0]).unwrap_or(0);
    if !(1..=3).contains(&id) {
        return Ok(Typval::list(Vec::new()));
    }
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let items = editor.get_matches(window);
    if let Some(item) = items.iter().find(|item| item.id == id) {
        let group = item.group.clone();
        let pattern = item.pattern.clone().unwrap_or_default();
        Ok(Typval::list(vec![
            Typval::String(OxStr::from(group.as_str())),
            Typval::String(OxStr::from(pattern.as_str())),
        ]))
    } else {
        Ok(Typval::list(vec![
            Typval::String(OxStr::from("")),
            Typval::String(OxStr::from("")),
        ]))
    }
}

fn match_item_to_dict(item: &MatchItem) -> Typval {
    let mut entries = vec![
        (
            OxStr::from("group"),
            Typval::String(OxStr::from(item.group.as_str())),
        ),
        (OxStr::from("priority"), Typval::Number(item.priority)),
        (OxStr::from("id"), Typval::Number(item.id)),
    ];
    if let Some(pattern) = &item.pattern {
        entries.push((
            OxStr::from("pattern"),
            Typval::String(OxStr::from(pattern.as_str())),
        ));
    } else {
        for (i, pos) in item.positions.iter().enumerate() {
            let key = format!("pos{}", i + 1);
            let list = Typval::list(vec![
                Typval::Number(pos.lnum),
                Typval::Number(pos.col),
                Typval::Number(pos.len),
            ]);
            entries.push((OxStr::from(key.as_str()), list));
        }
    }
    if let Some(conceal) = &item.conceal_char {
        entries.push((
            OxStr::from("conceal"),
            Typval::String(OxStr::from(conceal.as_str())),
        ));
    }
    Typval::dict(entries)
}

fn typval_number(value: &Typval) -> Option<i64> {
    match value {
        Typval::Number(n) => Some(*n),
        Typval::String(s) => s.to_string_lossy().parse().ok(),
        Typval::Bool(true) => Some(1),
        Typval::Bool(false) => Some(0),
        _ => None,
    }
}

fn dict_get<'a>(dict: &'a ox_types::DictData, key: &str) -> Option<&'a Typval> {
    dict.get(key.as_bytes())
}

fn dict_get_string(dict: &ox_types::DictData, key: &str) -> Option<String> {
    dict_get(dict, key).and_then(|v| {
        input_string_arg(v)
            .ok()
            .map(|s| s.to_string_lossy().to_string())
    })
}

fn dict_get_number(dict: &ox_types::DictData, key: &str) -> Option<i64> {
    dict_get(dict, key).and_then(typval_number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Editor, Geometry};

    #[expect(clippy::unwrap_used, reason = "test constructs a valid match state")]
    #[test]
    fn setmatches_keeps_existing_matches_when_input_is_malformed() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        editor.add_match(
            window,
            MatchItem {
                id: 9,
                priority: 10,
                group: "Search".to_owned(),
                pattern: Some("old".to_owned()),
                positions: Vec::new(),
                conceal_char: None,
            },
        );
        let result = call(
            &mut editor,
            "setmatches",
            &[Typval::list(vec![Typval::Number(1)])],
        )
        .unwrap();
        assert_eq!(result, Typval::Number(-1));
        assert_eq!(editor.get_matches(window).len(), 1);
        assert_eq!(editor.get_matches(window)[0].id, 9);
    }
}
