//! Prompt builtins: they read a reply from the message/typeahead seam instead
//! of a terminal (upstream `ex_getln.c`, `getchar.c`).

use crate::Editor;
use crate::editor::PromptDialog;
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
    let (minimum, maximum) = match name {
        "confirm" => (1, 4),
        "inputlist" => (1, 1),
        "input" | "inputdialog" => (1, 3),
        _ => (0, 2),
    };
    if args.len() < minimum {
        return Err(EvalError::new(
            "E119",
            0,
            format!("Not enough arguments for function: {name}"),
        ));
    }
    if args.len() > maximum {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    match name {
        "getchar" | "getcharstr" => host
            .access
            .with_ex_editor(|editor| call_getchar_builtin(editor, name, args)),
        "confirm" => host
            .access
            .with_ex_editor(|editor| call_confirm_builtin(editor, args)),
        "input" | "inputdialog" | "inputlist" => host
            .access
            .with_ex_editor(|editor| call_input_builtin(editor, name, args)),
        _ => unreachable!("input builtin route and dispatcher disagree"),
    }
}

fn call_input_builtin(editor: &mut Editor, name: &str, args: &[Typval]) -> ox_eval::Result<Typval> {
    if name == "inputlist" {
        return call_inputlist_builtin(editor, args);
    }
    let mut prompt = OxStr::from("");
    let mut reply = OxStr::from("");
    let mut cancelreturn = Typval::String(OxStr::from(""));
    let mut completion = None;
    let mut highlight_callback = None;
    if let Some(Typval::Dict(options)) = args.first() {
        if args.len() != 1 {
            return Err(EvalError::new(
                "E5050",
                0,
                "{opts} must be the only argument",
            ));
        }
        let options = options
            .try_borrow()
            .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
        if let Some(value) = options.get(b"prompt") {
            prompt = input_string_arg(value)?;
        }
        if let Some(value) = options.get(b"default") {
            reply = input_string_arg(value)?;
        }
        if let Some(value) = options.get(b"cancelreturn") {
            cancelreturn = value.clone();
        }
        completion = options
            .get(b"completion")
            .map(input_string_arg)
            .transpose()?;
        highlight_callback = options.get(b"highlight").cloned();
    } else {
        if let Some(value) = args.first() {
            prompt = input_string_arg(value)?;
        }
        if let Some(value) = args.get(1) {
            reply = input_string_arg(value)?;
        }
        if let Some(value) = args.get(2) {
            let value = input_string_arg(value)?;
            if name == "inputdialog" {
                cancelreturn = Typval::String(value);
            } else {
                completion = Some(value);
            }
        }
    }
    let message = if let Some(last_newline) = prompt.0.iter().rposition(|byte| *byte == b'\n') {
        let suffix = prompt.0.split_off(last_newline.saturating_add(1));
        std::mem::replace(&mut prompt, OxStr(suffix))
    } else {
        OxStr::from("")
    };
    editor.prompt_dialog = Some(PromptDialog {
        separator: !message.as_bytes().is_empty(),
        message,
        prompt,
        cursor: reply.as_bytes().len(),
        reply,
        highlight: editor.echo_highlight.clone(),
        completion,
        highlight_callback,
        cancelreturn,
        buttons: None,
        number: false,
        result: None,
    });
    drain_prompt(editor)
}

fn call_inputlist_builtin(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let Some(Typval::List(lines)) = args.first() else {
        return Err(EvalError::new(
            "E686",
            0,
            "Argument of inputlist() must be a List",
        ));
    };
    let mut text = Vec::new();
    {
        let lines = lines
            .try_borrow()
            .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
        for line in &lines.items {
            text.extend_from_slice(input_string_arg(line)?.as_bytes());
            text.push(b'\n');
        }
    }
    editor.prompt_dialog = Some(PromptDialog {
        message: OxStr(text),
        prompt: OxStr::from("Type number and <Enter> (q or empty cancels): "),
        reply: OxStr::from(""),
        cursor: 0,
        highlight: OxStr::from(""),
        separator: true,
        completion: None,
        highlight_callback: None,
        cancelreturn: Typval::Number(0),
        buttons: None,
        number: true,
        result: None,
    });
    drain_prompt(editor)
}

fn call_confirm_builtin(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let message = args
        .first()
        .map(input_string_arg)
        .transpose()?
        .unwrap_or_else(|| OxStr::from(""));
    let buttons = args
        .get(1)
        .map(input_string_arg)
        .transpose()?
        .unwrap_or_else(|| OxStr::from(""));
    let default = args
        .get(2)
        .map(super::position::number_value)
        .transpose()?
        .unwrap_or(1);
    if let Some(value) = args.get(3) {
        input_string_arg(value)?;
    }
    if editor.message_routing.silent {
        return Ok(Typval::Number(default));
    }
    let buttons = if buttons.as_bytes().is_empty() {
        "&Ok".into()
    } else {
        buttons.to_string_lossy()
    };
    let (choices, hotkeys) = confirm_choices(&buttons, default);
    let mut text = vec![b'\n'];
    text.extend_from_slice(message.as_bytes());
    text.push(b'\n');
    editor.prompt_dialog = Some(PromptDialog {
        message: OxStr(text),
        prompt: OxStr::from(choices.as_str()),
        reply: OxStr::from(""),
        cursor: 0,
        highlight: OxStr::from("MoreMsg"),
        separator: true,
        completion: None,
        highlight_callback: None,
        cancelreturn: Typval::Number(0),
        buttons: Some((hotkeys, default)),
        number: false,
        result: None,
    });
    drain_prompt(editor)
}

fn drain_prompt(editor: &mut Editor) -> ox_eval::Result<Typval> {
    while let Some(key) = editor
        .typeahead_mut()
        .pop()
        .map_err(|error| EvalError::new("E475", 0, error.to_string()))?
    {
        if let Some(result) = editor.feed_prompt_key(key)? {
            return Ok(result);
        }
    }
    // The synchronous EvalHost cannot suspend yet. Keep result=None so the
    // host can distinguish exhaustion from acceptance and resume this prompt.
    Ok(editor.prompt_dialog.as_ref().map_or(
        Typval::String(OxStr::from("")),
        |dialog| match &dialog.buttons {
            Some((_, default)) => Typval::Number(*default),
            None if dialog.number => Typval::Number(0),
            None => Typval::String(dialog.reply.clone()),
        },
    ))
}

impl Editor {
    /// Returns the retained message-area prompt, including pending input.
    #[must_use]
    pub fn prompt_dialog(&self) -> Option<&PromptDialog> {
        self.prompt_dialog.as_ref()
    }

    /// Removes a prompt after the host restores the surrounding command line.
    pub fn clear_prompt_dialog(&mut self) {
        self.prompt_dialog = None;
    }

    /// Updates the `:echohl` group used by subsequent prompts.
    pub fn set_echo_highlight(&mut self, group: OxStr) {
        self.echo_highlight = group;
    }

    /// Consumes one key from the normal typeahead source for a retained prompt.
    ///
    /// `None` means more input is required, never an accepted empty reply.
    /// Completion and highlight callbacks must run outside the editor borrow.
    ///
    /// # Errors
    ///
    /// Returns the evaluation error the prompt's cancel/accept handler
    /// produced, mirroring `vgetc`-driven prompt loops upstream.
    pub fn feed_prompt_key(&mut self, key: Key) -> ox_eval::Result<Option<Typval>> {
        let key = match key {
            Key::Special(KS_EXTRA, b'R' | b'N') => Key::Byte(b'\r'),
            Key::Special(KS_EXTRA, b'T') => Key::Byte(b'\t'),
            Key::Special(KS_EXTRA, b'E') => Key::Byte(0x1b),
            Key::Special(KS_EXTRA, b'B' | b'D') => Key::Byte(0x08),
            key => key,
        };
        let Some(dialog) = self.prompt_dialog.as_mut() else {
            return Ok(None);
        };
        if dialog.result.is_some() {
            return Ok(dialog.result.clone());
        }
        if matches!(key, Key::Byte(0x03 | 0x1b)) {
            dialog.result = Some(dialog.cancelreturn.clone());
        } else if let Some((hotkeys, default)) = &dialog.buttons {
            match key {
                Key::Byte(b'\r' | b'\n' | 0) => dialog.result = Some(Typval::Number(*default)),
                Key::Byte(byte) => {
                    dialog.reply.0.push(byte);
                    match std::str::from_utf8(dialog.reply.as_bytes()) {
                        Ok(text) => {
                            if let Some(character) = text.chars().next()
                                && let Some(index) = hotkeys.iter().position(|hotkey| {
                                    hotkey.to_lowercase().eq(character.to_lowercase())
                                })
                            {
                                let choice =
                                    i64::try_from(index.saturating_add(1)).map_err(|error| {
                                        EvalError::new("E475", 0, error.to_string())
                                    })?;
                                dialog.result = Some(Typval::Number(choice));
                            }
                            dialog.reply.0.clear();
                        }
                        Err(error) if error.error_len().is_some() => dialog.reply.0.clear(),
                        Err(_) => {}
                    }
                }
                Key::Special(_, _) => {}
            }
        } else {
            match key {
                Key::Byte(b'\r' | b'\n') => {
                    dialog.result = Some(if dialog.number {
                        Typval::Number(dialog.reply.to_string_lossy().parse().unwrap_or(0))
                    } else {
                        Typval::String(dialog.reply.clone())
                    });
                }
                Key::Byte(b'q') if dialog.number => dialog.result = Some(Typval::Number(0)),
                Key::Byte(0x08 | 0x7f) => {
                    if let Some(start) = dialog.reply.0.iter().rposition(|byte| byte & 0xc0 != 0x80)
                    {
                        dialog.reply.0.truncate(start);
                    }
                }
                Key::Byte(0x15) => dialog.reply.0.clear(),
                Key::Byte(byte) => dialog.reply.0.push(byte),
                Key::Special(_, _) => {}
            }
            dialog.cursor = dialog.reply.as_bytes().len();
        }
        Ok(dialog.result.clone())
    }
}

fn confirm_choices(buttons: &str, default: i64) -> (String, Vec<char>) {
    let mut display = String::new();
    let mut hotkeys = Vec::new();
    for (index, button) in buttons.split('\n').enumerate() {
        if index != 0 {
            display.push_str(", ");
        }
        let mut hotkey = button.chars().next().unwrap_or('\0');
        let mut first = !button.contains('&');
        let mut chars = button.chars();
        while let Some(character) = chars.next() {
            if character != '&' && !first {
                display.push(character);
                continue;
            }
            first = false;
            let character = if character == '&' {
                let Some(next) = chars.next() else { break };
                if next == '&' {
                    display.push('&');
                    continue;
                }
                next
            } else {
                character
            };
            hotkey = character;
            let selected = i64::try_from(index.saturating_add(1)) == Ok(default);
            display.push(if selected { '[' } else { '(' });
            display.push(character);
            display.push(if selected { ']' } else { ')' });
        }
        hotkeys.push(hotkey);
    }
    display.push_str(": ");
    (display, hotkeys)
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
