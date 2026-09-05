//! Prompt builtins: they read a reply from the message/typeahead seam instead
//! of a terminal (upstream `ex_getln.c`, `getchar.c`).

use crate::Editor;
use crate::excmd_exec::ExEditorAccess;
use crate::script::FileIO;
use crate::typeahead::{K_SPECIAL, KS_EXTRA, Key};
use ox_eval::EvalError;
use ox_types::{OxStr, Typval};

use super::input_string_arg;
use crate::excmd_exec::EvalHost;

/// Second byte of an internal key code that carries modifier flags.
const KS_MODIFIER: u8 = 0xfc;
/// `KS_MODIFIER` flag bit marking a control-modified key.
const MOD_MASK_CTRL: u8 = 0x04;

/// Routes one prompt builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    match name {
        "getchar" | "getcharstr" => host
            .access
            .with_ex_editor(|editor| call_getchar_builtin(editor, name, args)),
        "input" | "inputdialog" | "inputlist" => host
            .access
            .with_ex_editor(|editor| call_input_builtin(editor, name, args)),
        _ => unreachable!("input builtin route and dispatcher disagree"),
    }
}

fn call_input_builtin(editor: &mut Editor, name: &str, args: &[Typval]) -> ox_eval::Result<Typval> {
    let default = args
        .get(1)
        .map(input_string_arg)
        .transpose()?
        .unwrap_or_else(|| OxStr::from(""));
    let cancel = args
        .get(2)
        .map(input_string_arg)
        .transpose()?
        .unwrap_or_else(|| OxStr::from(""));
    let mut bytes = Vec::new();
    let mut cancelled = false;
    while let Some(key) = editor
        .typeahead_mut()
        .pop()
        .map_err(|error| EvalError::new("E475", 0, error.to_string()))?
    {
        match key {
            Key::Byte(b'\r' | b'\n') => break,
            Key::Byte(0x1b) => {
                cancelled = true;
                break;
            }
            Key::Byte(0x08 | 0x7f) => {
                bytes.pop();
            }
            Key::Byte(byte) => bytes.push(byte),
            Key::Special(_, _) => {}
        }
    }
    if name == "inputlist" {
        if cancelled || bytes == b"q" {
            return Ok(Typval::Number(0));
        }
        return Ok(Typval::Number(
            String::from_utf8_lossy(&bytes).parse().unwrap_or(0),
        ));
    }
    if cancelled {
        return Ok(Typval::String(cancel));
    }
    Ok(Typval::String(if bytes.is_empty() {
        default
    } else {
        OxStr(bytes)
    }))
}

fn call_getchar_builtin(
    editor: &mut Editor,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.len() > 2 {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    let mut number = name == "getchar";
    let mut simplify = true;
    if let Some(options) = args.get(1) {
        let Typval::Dict(options) = options else {
            return Err(EvalError::new(
                "E1206",
                0,
                "Dictionary required for argument 2",
            ));
        };
        let options = options
            .try_borrow()
            .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
        for entry in &options.entries {
            match entry.key.as_bytes() {
                b"number" if name == "getcharstr" => {
                    return Err(EvalError::new(
                        "E475",
                        0,
                        "Invalid value for argument number",
                    ));
                }
                b"number" => number = entry.value.is_truthy(),
                b"simplify" => simplify = entry.value.is_truthy(),
                _ => {}
            }
        }
    }
    let Some(first) = editor
        .typeahead_mut()
        .pop()
        .map_err(|error| EvalError::new("E475", 0, error.to_string()))?
    else {
        return Ok(if number {
            Typval::Number(0)
        } else {
            Typval::String(OxStr::from(""))
        });
    };
    let keys = collect_modified_key(editor, first)?;
    let raw = keys
        .iter()
        .flat_map(|key| match key {
            // Output the un-escaped byte: `Key::Byte(0x80)` is the decoded
            // form of the internal `K_SPECIAL KS_SPECIAL KE_FILLER` escape,
            // and `getchar()` must return the raw byte, matching `\<X>`
            // notation which also stores the un-escaped form.
            Key::Byte(byte) => vec![*byte],
            Key::Special(second, third) => vec![K_SPECIAL, *second, *third],
        })
        .collect::<Vec<_>>();
    let simplified = if simplify {
        match keys.as_slice() {
            [Key::Special(KS_EXTRA, b'T')] => Some(b'\t'),
            [Key::Special(KS_EXTRA, b'N')] => Some(b'\n'),
            [Key::Special(KS_EXTRA, b'R')] => Some(b'\r'),
            [Key::Special(KS_EXTRA, b'E')] => Some(0x1b),
            [Key::Special(KS_EXTRA, b'S')] => Some(b' '),
            [Key::Special(KS_EXTRA, b'L')] => Some(b'<'),
            [Key::Special(KS_EXTRA, b'D')] => Some(0x7f),
            [Key::Special(KS_MODIFIER, modifiers), Key::Byte(byte)]
                if modifiers & MOD_MASK_CTRL != 0 =>
            {
                Some(byte & 0x1f)
            }
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
    Ok(Typval::String(OxStr(
        simplified.map_or(raw, |byte| vec![byte]),
    )))
}

/// Collects the complete key sequence started by `first`.
///
/// A `KS_MODIFIER` prefix is followed by the modified key itself; when that
/// key is the leading byte of a multibyte UTF-8 character, its continuation
/// bytes are consumed as well so the whole character is returned. Any other
/// key comes back on its own.
fn collect_modified_key(editor: &mut Editor, first: Key) -> ox_eval::Result<Vec<Key>> {
    let mut keys = vec![first];
    if !matches!(first, Key::Special(KS_MODIFIER, _)) {
        return Ok(keys);
    }
    // For a multibyte UTF-8 character (e.g. `<M-…>` where `…` = E2 80
    // A6), the typeahead stores each byte as a separate `Key`, with
    // 0x80 (K_SPECIAL) escaped as a 3-byte triple. We must consume all
    // continuation bytes to return the full character, matching
    // upstream `vgetorpeek` + `mb_cptr2len`.
    let Some(second) = editor
        .typeahead_mut()
        .pop()
        .map_err(|error| EvalError::new("E475", 0, error.to_string()))?
    else {
        return Ok(keys);
    };
    keys.push(second);
    // Only a byte starting a multibyte UTF-8 sequence pulls in more keys.
    let Key::Byte(lead) = second else {
        return Ok(keys);
    };
    if lead < 0xC0 {
        return Ok(keys);
    }
    let expected = utf8_len_from_start(lead);
    while keys.len() < 1 + expected {
        match editor
            .typeahead_mut()
            .peek()
            .map_err(|error| EvalError::new("E475", 0, error.to_string()))?
        {
            Some(Key::Byte(b)) if b & 0xC0 == 0x80 => {
                let Some(key) = editor
                    .typeahead_mut()
                    .pop()
                    .map_err(|error| EvalError::new("E475", 0, error.to_string()))?
                else {
                    break;
                };
                keys.push(key);
            }
            _ => break,
        }
    }
    Ok(keys)
}

/// UTF-8 character byte length from the leading byte.
fn utf8_len_from_start(byte: u8) -> usize {
    match byte {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}
