//! Prompt builtins: they read a reply from the message/typeahead seam instead
//! of a terminal (upstream `ex_getln.c`, `getchar.c`).

use ox_eval::EvalError;
use ox_types::{OxStr, Typval};
use crate::script::FileIO;
use crate::typeahead::{Key, KS_EXTRA, K_SPECIAL};
use crate::Editor;

use crate::excmd_exec::{EvalHost};
use super::{input_string_arg};

/// Routes one prompt builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    match name {
        "getchar" | "getcharstr" => call_getchar_builtin(host.editor, name, args),
        "input" | "inputdialog" | "inputlist" => call_input_builtin(host.editor, name, args),
        _ => unreachable!("input builtin route and dispatcher disagree"),
    }
}

fn call_input_builtin(editor: &mut Editor, name: &str, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    let default = args.get(1).map(input_string_arg).transpose()?.unwrap_or_else(|| OxStr::from(""));
    let cancel = args.get(2).map(input_string_arg).transpose()?.unwrap_or_else(|| OxStr::from(""));
    let mut bytes = Vec::new();
    let mut cancelled = false;
    while let Some(key) = editor.typeahead_mut().pop().map_err(|error| EvalError::new("E475", 0, error.to_string()))? {
        match key {
            Key::Byte(b'\r' | b'\n') => break,
            Key::Byte(0x1b) => { cancelled = true; break; }
            Key::Byte(0x08 | 0x7f) => { bytes.pop(); }
            Key::Byte(byte) => bytes.push(byte),
            Key::Special(_, _) => {}
        }
    }
    if name == "inputlist" {
        if cancelled || bytes == b"q" { return Ok(Typval::Number(0)); }
        return Ok(Typval::Number(String::from_utf8_lossy(&bytes).parse().unwrap_or(0)));
    }
    if cancelled { return Ok(Typval::String(cancel)); }
    Ok(Typval::String(if bytes.is_empty() { default } else { OxStr(bytes) }))
}

fn call_getchar_builtin(editor: &mut Editor, name: &str, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    const KS_MODIFIER: u8 = 0xfc;
    if args.len() > 2 {
        return Err(EvalError::new("E118", 0, format!("Too many arguments for function: {name}")));
    }
    let mut number = name == "getchar";
    let mut simplify = true;
    if let Some(options) = args.get(1) {
        let Typval::Dict(options) = options else {
            return Err(EvalError::new("E1206", 0, "Dictionary required for argument 2"));
        };
        let options = options.try_borrow().map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
        for (key, value) in &options.entries {
            match key.as_bytes() {
                b"number" if name == "getcharstr" => {
                    return Err(EvalError::new("E475", 0, "Invalid value for argument number"));
                }
                b"number" => number = value.is_truthy(),
                b"simplify" => simplify = value.is_truthy(),
                _ => {}
            }
        }
    }
    let Some(first) = editor.typeahead_mut().pop().map_err(|error| EvalError::new("E475", 0, error.to_string()))? else {
        return Ok(if number { Typval::Number(0) } else { Typval::String(OxStr::from("")) });
    };
    let mut keys = vec![first];
    if matches!(first, Key::Special(KS_MODIFIER, _)) {
        if let Some(key) = editor.typeahead_mut().pop().map_err(|error| EvalError::new("E475", 0, error.to_string()))? {
            keys.push(key);
        }
    }
    let raw = keys.iter().flat_map(|key| match key {
        Key::Byte(byte) => vec![*byte],
        Key::Special(second, third) => vec![K_SPECIAL, *second, *third],
    }).collect::<Vec<_>>();
    let simplified = if simplify {
        match keys.as_slice() {
            [Key::Special(KS_EXTRA, b'T')] => Some(b'\t'),
            [Key::Special(KS_EXTRA, b'N')] => Some(b'\n'),
            [Key::Special(KS_EXTRA, b'R')] => Some(b'\r'),
            [Key::Special(KS_EXTRA, b'E')] => Some(0x1b),
            [Key::Special(KS_EXTRA, b'S')] => Some(b' '),
            [Key::Special(KS_EXTRA, b'L')] => Some(b'<'),
            [Key::Special(KS_EXTRA, b'D')] => Some(0x7f),
            [Key::Special(KS_MODIFIER, modifiers), Key::Byte(byte)] if modifiers & 2 != 0 => Some(byte & 0x1f),
            [Key::Byte(byte)] => Some(*byte),
            _ => None,
        }
    } else {
        None
    };
    if number {
        return Ok(simplified.map_or_else(
            || Typval::String(OxStr(raw)),
            |byte| Typval::Number(i64::from(byte)),
        ));
    }
    Ok(Typval::String(OxStr(simplified.map_or(raw, |byte| vec![byte]))))
}
