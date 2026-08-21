//! Editor-global Neovim API functions.

use ox_editor::{
    BufferRelease, Editor, KE_FILLER, K_SPECIAL, KS_EXTRA, KS_SPECIAL, KS_ZERO, Keys, Message,
    MessageKind, OptionScope, OptionValue, TypeaheadFlags,
};
use ox_eval::{BuiltinHost, Builtins, EvalError, EvalErrorKind, Evaluator, NoRegex, Parser as EvalParser, Scope};
use ox_excmd::{ExCommand, Parser as CommandParser};
use ox_types::{Special, Typval};
use unicode_width::UnicodeWidthStr;

use crate::{
    api, ApiError, BufHandle, Dict, FunctionMetadata, Object, OxStr, Registry, RegistryError,
    TabHandle, WinHandle,
};

const MAX_CONVERSION_DEPTH: usize = 100;

/// Execution seam deliberately owned by the future Ex-command host.
pub trait CommandExecutor {
    /// Execute commands that have already passed `ox-excmd` parsing.
    fn execute(&mut self, editor: &mut Editor, commands: &[ExCommand]) -> Result<(), ApiError>;
}

/// Parse and execute a command through an explicitly supplied host.
pub fn execute_command(
    editor: &mut Editor,
    command: &OxStr,
    executor: &mut dyn CommandExecutor,
) -> Result<(), ApiError> {
    let commands = parse_command(command)?;
    executor.execute(editor, &commands)
}

fn exception(error: impl std::fmt::Display) -> ApiError {
    ApiError::exception(error.to_string())
}

fn current_buffer(editor: &Editor) -> Result<BufHandle, ApiError> {
    editor
        .current_buffer()
        .ok_or_else(|| ApiError::exception("No current buffer"))
}

fn current_window(editor: &Editor) -> Result<WinHandle, ApiError> {
    editor
        .current_window()
        .ok_or_else(|| ApiError::exception("No current window"))
}

fn current_tabpage(editor: &Editor) -> Result<TabHandle, ApiError> {
    editor
        .current_tabpage()
        .ok_or_else(|| ApiError::exception("No current tabpage"))
}

#[api(since = 1)]
pub fn nvim_get_current_buf(editor: &mut Editor) -> Result<BufHandle, ApiError> {
    current_buffer(editor)
}

#[api(since = 1, textlock)]
pub fn nvim_set_current_buf(editor: &mut Editor, buf: BufHandle) -> Result<(), ApiError> {
    editor
        .set_current_buffer(buf, BufferRelease::KeepLoaded)
        .map_err(exception)
}

#[api(since = 1)]
pub fn nvim_get_current_win(editor: &mut Editor) -> Result<WinHandle, ApiError> {
    current_window(editor)
}

#[api(since = 1, textlock)]
pub fn nvim_set_current_win(editor: &mut Editor, win: WinHandle) -> Result<(), ApiError> {
    editor.set_current_window(win).map_err(exception)
}

#[api(since = 1)]
pub fn nvim_get_current_tabpage(editor: &mut Editor) -> Result<TabHandle, ApiError> {
    current_tabpage(editor)
}

#[api(since = 1, textlock)]
pub fn nvim_set_current_tabpage(
    editor: &mut Editor,
    tabpage: TabHandle,
) -> Result<(), ApiError> {
    editor.set_current_tabpage(tabpage).map_err(exception)
}

#[api(since = 1)]
pub fn nvim_list_bufs(editor: &mut Editor) -> Result<Vec<BufHandle>, ApiError> {
    Ok(editor.buffers())
}

#[api(since = 1)]
pub fn nvim_list_wins(editor: &mut Editor) -> Result<Vec<WinHandle>, ApiError> {
    Ok(editor.windows())
}

#[api(since = 1)]
pub fn nvim_list_tabpages(editor: &mut Editor) -> Result<Vec<TabHandle>, ApiError> {
    Ok(editor.tabpages())
}

#[api(since = 1, fast)]
pub fn nvim_get_api_info(_editor: &mut Editor) -> Result<Vec<Object>, ApiError> {
    let registry = crate::registry::core().map_err(exception)?;
    let mut api_metadata = ox_rpc::ApiMetadata::new();
    for (metadata, _) in registry.iter() {
        api_metadata.add_function(Object::Dict(function_metadata(metadata)));
    }
    // Dispatch has no RPC channel context; zero is the documented anonymous channel id.
    let Object::Array(info) = api_metadata.api_info_object() else {
        return Err(ApiError::exception("invalid API metadata response"));
    };
    Ok(info)
}

#[api(since = 1)]
pub fn nvim_command(_editor: &mut Editor, command: OxStr) -> Result<(), ApiError> {
    let _commands = parse_command(&command)?;
    Err(ApiError::exception("Not implemented: nvim_command executor"))
}

#[api(since = 1)]
pub fn nvim_eval(editor: &mut Editor, expr: OxStr) -> Result<Object, ApiError> {
    let expression = EvalParser::new(expr.as_bytes()).parse().map_err(map_eval_error)?;
    let mut builtins = Builtins::without_regex();
    let regex = NoRegex;
    let mut evaluator = Evaluator::new(&mut builtins, &regex);
    let mut scope = Scope::new();
    scope.vim = editor
        .vvars()
        .iter()
        .map(|(name, value)| Ok((name.clone(), object_to_typval(value, 0)?)))
        .collect::<Result<Vec<_>, ApiError>>()?;
    evaluator
        .eval(&expression, &mut scope)
        .map_err(map_eval_error)
        .and_then(|value| typval_to_object(&value, 0))
}

#[api(since = 1)]
pub fn nvim_call_function(
    _editor: &mut Editor,
    fn_name: OxStr,
    args: Vec<Object>,
) -> Result<Object, ApiError> {
    let args = args
        .iter()
        .map(|argument| object_to_typval(argument, 0))
        .collect::<Result<Vec<_>, _>>()?;
    let mut builtins = Builtins::without_regex();
    let mut scope = Scope::new();
    builtins
        .call(&fn_name, args, &mut scope)
        .map_err(map_eval_error)
        .and_then(|value| typval_to_object(&value, 0))
}

#[api(since = 1)]
pub fn nvim_get_vvar(editor: &mut Editor, name: OxStr) -> Result<Object, ApiError> {
    editor
        .vvars()
        .get(&name)
        .cloned()
        .ok_or_else(|| ApiError::validation(format!("Key not found: {}", name.to_string_lossy())))
}

#[api(since = 6)]
pub fn nvim_set_vvar(
    editor: &mut Editor,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    editor.vvars_mut().insert(name, value);
    Ok(())
}

#[api(since = 1, deprecated_since = 11)]
pub fn nvim_get_option(editor: &mut Editor, name: OxStr) -> Result<Object, ApiError> {
    let name = option_name(&name)?;
    editor
        .options()
        .get_global(name)
        .map(option_value_to_object)
        .map_err(exception)
}

#[api(since = 1, deprecated_since = 11)]
pub fn nvim_set_option(
    editor: &mut Editor,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let name = option_name(&name)?;
    let value = object_to_option_value(value)?;
    editor.options_mut().set_global(name, value).map_err(exception)
}

#[api(since = 9)]
pub fn nvim_get_option_value(
    editor: &mut Editor,
    name: OxStr,
    opts: Dict,
) -> Result<Object, ApiError> {
    if opts.get(&OxStr::from("dry_run")).is_some()
        || opts.get(&OxStr::from("operation")).is_some()
    {
        return Err(ApiError::validation("Invalid key for nvim_get_option_value"));
    }
    let name = option_name(&name)?;
    let target = option_target(editor, name, &opts)?;
    get_option_at(editor, name, target)
}

#[api(since = 9)]
pub fn nvim_set_option_value(
    editor: &mut Editor,
    name: OxStr,
    value: Object,
    opts: Dict,
) -> Result<Object, ApiError> {
    let name = option_name(&name)?;
    let value = object_to_option_value(value)?;
    if dict_bool(&opts, "dry_run")? == Some(true) {
        validate_option_at(editor, name, &value, option_target(editor, name, &opts)?)?;
        return Ok(Object::Nil);
    }
    if let Some(operation) = dict_string(&opts, "operation")? {
        if operation.as_bytes() != b"set" {
            return Err(ApiError::validation(format!(
                "Unsupported nvim_set_option_value operation: {}",
                operation.to_string_lossy()
            )));
        }
    }
    let target = option_target(editor, name, &opts)?;
    set_option_at(editor, name, value, target)?;
    Ok(Object::Nil)
}

#[api(since = 1, fast)]
pub fn nvim_input(editor: &mut Editor, keys: OxStr) -> Result<i64, ApiError> {
    let count = i64::try_from(keys.as_bytes().len())
        .map_err(|_| ApiError::exception("Input length exceeds Integer range"))?;
    let encoded = Keys::encode(keys.as_bytes());
    editor
        .typeahead_mut()
        .append(&encoded, TypeaheadFlags::default());
    Ok(count)
}

#[api(since = 1, fast)]
pub fn nvim_replace_termcodes(
    _editor: &mut Editor,
    str: OxStr,
    _from_part: bool,
    do_lt: bool,
    special: bool,
) -> Result<OxStr, ApiError> {
    let replaced = replace_termcode_notation(str.as_bytes(), do_lt, special);
    Ok(OxStr::from(replaced.as_slice()))
}

#[api(since = 1)]
pub fn nvim_strwidth(_editor: &mut Editor, text: OxStr) -> Result<i64, ApiError> {
    let text = std::str::from_utf8(text.as_bytes())
        .map_err(|_| ApiError::validation("text must be valid UTF-8"))?;
    // Source: unicode-width 0.2 implements terminal columns from Unicode Standard Annex #11.
    i64::try_from(UnicodeWidthStr::width(text))
        .map_err(|_| ApiError::exception("Text width exceeds Integer range"))
}

#[api(since = 1, deprecated_since = 13)]
pub fn nvim_err_writeln(editor: &mut Editor, str: OxStr) -> Result<(), ApiError> {
    editor.push_message(Message {
        kind: MessageKind::Error,
        content: Object::String(str),
        history: true,
    });
    Ok(())
}

#[api(since = 7)]
pub fn nvim_echo(
    editor: &mut Editor,
    chunks: Vec<Object>,
    history: bool,
    opts: Dict,
) -> Result<Object, ApiError> {
    validate_echo_chunks(&chunks)?;
    if let Some((key, _)) = opts.iter().find(|(key, _)| key.as_bytes() != b"err") {
        return Err(ApiError::validation(format!(
            "Echo option '{}' is unavailable",
            key.to_string_lossy()
        )));
    }
    let kind = if dict_bool(&opts, "err")? == Some(true) {
        MessageKind::Error
    } else {
        MessageKind::Echo
    };
    editor.push_message(Message {
        kind,
        content: Object::Array(chunks),
        history,
    });
    Ok(Object::Integer(-1))
}

#[derive(Clone, Copy)]
enum OptionTarget {
    Global,
    Buffer(BufHandle),
    Window(WinHandle),
    GlobalAndBuffer(BufHandle),
    GlobalAndWindow(WinHandle),
}

fn option_target(editor: &Editor, name: &str, opts: &Dict) -> Result<OptionTarget, ApiError> {
    reject_unknown_option_keys(opts)?;
    if opts.get(&OxStr::from("filetype")).is_some() || opts.get(&OxStr::from("tab")).is_some() {
        return Err(ApiError::validation("filetype/tab option context is unavailable"));
    }
    let buffer = dict_handle(opts, "buf", BufHandle::try_from)?;
    let window = dict_handle(opts, "win", WinHandle::try_from)?;
    if buffer.is_some() && window.is_some() {
        return Err(ApiError::validation("opts.buf and opts.win are mutually exclusive"));
    }
    let scope = dict_string(opts, "scope")?;
    if let Some(scope) = &scope {
        if scope.as_bytes() != b"global" && scope.as_bytes() != b"local" {
            return Err(ApiError::validation("opts.scope must be 'global' or 'local'"));
        }
    }
    if scope.as_ref().is_some_and(|scope| scope.as_bytes() == b"global") {
        if buffer.is_some() || window.is_some() {
            return Err(ApiError::validation("global scope cannot be combined with opts.buf or opts.win"));
        }
        return Ok(OptionTarget::Global);
    }
    if let Some(buffer) = buffer {
        return Ok(OptionTarget::Buffer(resolve_buffer(editor, buffer)?));
    }
    if let Some(window) = window {
        return Ok(OptionTarget::Window(resolve_window(editor, window)?));
    }
    let metadata = ox_editor::OptionStore::metadata(name).map_err(exception)?;
    let has_window = metadata.scopes.contains(&OptionScope::Window);
    let has_buffer = metadata.scopes.contains(&OptionScope::Buffer);
    if scope.is_some() {
        if has_window {
            return Ok(OptionTarget::Window(current_window(editor)?));
        }
        if has_buffer {
            return Ok(OptionTarget::Buffer(current_buffer(editor)?));
        }
        return Err(ApiError::validation(format!("Option '{name}' has no local value")));
    }
    if metadata.scopes.contains(&OptionScope::Global) && has_window {
        return Ok(OptionTarget::GlobalAndWindow(current_window(editor)?));
    }
    if metadata.scopes.contains(&OptionScope::Global) && has_buffer {
        return Ok(OptionTarget::GlobalAndBuffer(current_buffer(editor)?));
    }
    if has_window {
        return Ok(OptionTarget::Window(current_window(editor)?));
    }
    if has_buffer {
        return Ok(OptionTarget::Buffer(current_buffer(editor)?));
    }
    Ok(OptionTarget::Global)
}

fn get_option_at(editor: &Editor, name: &str, target: OptionTarget) -> Result<Object, ApiError> {
    let value = match target {
        OptionTarget::Global => editor.options().get_global(name),
        OptionTarget::Buffer(buffer) | OptionTarget::GlobalAndBuffer(buffer) => {
            editor.options().get_buffer(buffer, name)
        }
        OptionTarget::Window(window) | OptionTarget::GlobalAndWindow(window) => {
            editor.options().get_window(window, name)
        }
    };
    value.map(option_value_to_object).map_err(exception)
}

fn validate_option_at(
    editor: &Editor,
    name: &str,
    value: &OptionValue,
    target: OptionTarget,
) -> Result<(), ApiError> {
    let metadata = ox_editor::OptionStore::metadata(name).map_err(exception)?;
    if metadata.value_type != value.value_type() {
        return Err(ApiError::validation(format!("Option '{name}' has invalid type")));
    }
    match target {
        OptionTarget::Global if !metadata.scopes.contains(&OptionScope::Global) => {
            Err(ApiError::validation(format!("Option '{name}' has no global value")))
        }
        OptionTarget::Buffer(_) | OptionTarget::GlobalAndBuffer(_)
            if !metadata.scopes.contains(&OptionScope::Buffer) =>
        {
            Err(ApiError::validation(format!("Option '{name}' has no buffer-local value")))
        }
        OptionTarget::Window(_) | OptionTarget::GlobalAndWindow(_)
            if !metadata.scopes.contains(&OptionScope::Window) =>
        {
            Err(ApiError::validation(format!("Option '{name}' has no window-local value")))
        }
        _ => {
            let _ = editor;
            Ok(())
        }
    }
}

fn set_option_at(
    editor: &mut Editor,
    name: &str,
    value: OptionValue,
    target: OptionTarget,
) -> Result<(), ApiError> {
    match target {
        OptionTarget::Global => editor.options_mut().set_global(name, value).map_err(exception),
        OptionTarget::Buffer(buffer) => editor
            .options_mut()
            .set_buffer(buffer, name, value)
            .map_err(exception),
        OptionTarget::Window(window) => editor
            .options_mut()
            .set_window(window, name, value)
            .map_err(exception),
        OptionTarget::GlobalAndBuffer(buffer) => {
            editor.options_mut().set_global(name, value.clone()).map_err(exception)?;
            editor.options_mut().set_buffer(buffer, name, value).map_err(exception)
        }
        OptionTarget::GlobalAndWindow(window) => {
            editor.options_mut().set_global(name, value.clone()).map_err(exception)?;
            editor.options_mut().set_window(window, name, value).map_err(exception)
        }
    }
}

fn resolve_buffer(editor: &Editor, buffer: BufHandle) -> Result<BufHandle, ApiError> {
    if buffer.is_current() {
        return current_buffer(editor);
    }
    editor.buffer(buffer).map_err(exception)?;
    Ok(buffer)
}

fn resolve_window(editor: &Editor, window: WinHandle) -> Result<WinHandle, ApiError> {
    if window.is_current() {
        return current_window(editor);
    }
    editor.window(window).map_err(exception)?;
    Ok(window)
}

fn option_name(name: &OxStr) -> Result<&str, ApiError> {
    std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("Option name must be valid UTF-8"))
}

fn object_to_option_value(value: Object) -> Result<OptionValue, ApiError> {
    match value {
        Object::Boolean(value) => Ok(OptionValue::Boolean(value)),
        Object::Integer(value) => Ok(OptionValue::Number(value)),
        Object::String(value) => String::from_utf8(value.0)
            .map(OptionValue::String)
            .map_err(|_| ApiError::validation("Option string must be valid UTF-8")),
        _ => Err(ApiError::validation("Option value must be Boolean, Integer, or String")),
    }
}

fn option_value_to_object(value: &OptionValue) -> Object {
    match value {
        OptionValue::Boolean(value) => Object::Boolean(*value),
        OptionValue::Number(value) => Object::Integer(*value),
        OptionValue::String(value) => Object::String(OxStr::from(value.as_str())),
    }
}

fn parse_command(command: &OxStr) -> Result<Vec<ExCommand>, ApiError> {
    let command = std::str::from_utf8(command.as_bytes())
        .map_err(|_| ApiError::validation("Command must be valid UTF-8"))?;
    CommandParser::new().parse(command).map_err(exception)
}

fn object_to_typval(value: &Object, depth: usize) -> Result<Typval, ApiError> {
    if depth >= MAX_CONVERSION_DEPTH {
        return Err(ApiError::exception("Object nesting is too deep"));
    }
    match value {
        Object::Nil => Ok(Typval::Special(Special::Null)),
        Object::Boolean(value) => Ok(Typval::Bool(*value)),
        Object::Integer(value) => Ok(Typval::Number(*value)),
        Object::Float(value) => Ok(Typval::Float(*value)),
        Object::String(value) => Ok(Typval::String(value.clone())),
        Object::Array(values) => values
            .iter()
            .map(|value| object_to_typval(value, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Typval::list),
        Object::Dict(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), object_to_typval(value, depth + 1)?)))
            .collect::<Result<Vec<_>, ApiError>>()
            .map(Typval::dict),
        Object::Buffer(value) => Ok(Typval::Number(i64::from(*value))),
        Object::Window(value) => Ok(Typval::Number(i64::from(*value))),
        Object::Tabpage(value) => Ok(Typval::Number(i64::from(*value))),
        Object::LuaRef(_) => Err(ApiError::exception("LuaRef Typval conversion is unavailable")),
    }
}

fn typval_to_object(value: &Typval, depth: usize) -> Result<Object, ApiError> {
    if depth >= MAX_CONVERSION_DEPTH {
        return Err(ApiError::exception("Typval nesting is too deep"));
    }
    match value {
        Typval::Number(value) => Ok(Object::Integer(*value)),
        Typval::Float(value) => Ok(Object::Float(*value)),
        Typval::String(value) => Ok(Object::String(value.clone())),
        Typval::Blob(value) => Ok(Object::String(OxStr::from(value.as_slice()))),
        Typval::Bool(value) => Ok(Object::Boolean(*value)),
        Typval::Special(Special::Null) => Ok(Object::Nil),
        Typval::List(values) => values
            .try_borrow()
            .map_err(|_| ApiError::exception("Cannot convert mutably borrowed List"))?
            .items
            .iter()
            .map(|value| typval_to_object(value, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Object::Array),
        Typval::Dict(values) => values
            .try_borrow()
            .map_err(|_| ApiError::exception("Cannot convert mutably borrowed Dictionary"))?
            .entries
            .iter()
            .map(|(key, value)| Ok((key.clone(), typval_to_object(value, depth + 1)?)))
            .collect::<Result<Vec<_>, ApiError>>()
            .map(|entries| Object::Dict(Dict(entries))),
        Typval::Funcref(_) | Typval::Partial(_) => Ok(Object::Nil),
        Typval::Channel(value) | Typval::Job(value) => i64::try_from(*value)
            .map(Object::Integer)
            .map_err(|_| ApiError::exception("Channel/job id exceeds Integer range")),
    }
}

fn map_eval_error(error: EvalError) -> ApiError {
    match error.kind {
        EvalErrorKind::NotImplemented(name) => {
            ApiError::exception(format!("Not implemented: {}", name.to_string_lossy()))
        }
        EvalErrorKind::Vim => ApiError::exception(error.to_string()),
    }
}

fn function_metadata(metadata: &FunctionMetadata) -> Dict {
    let parameters = metadata
        .params
        .iter()
        .map(|(name, ty)| {
            Object::Array(vec![
                Object::String(OxStr::from(ty.to_string().as_str())),
                Object::String(OxStr::from(*name)),
            ])
        })
        .collect();
    let mut fields = vec![
        (OxStr::from("name"), Object::String(OxStr::from(metadata.name))),
        (OxStr::from("parameters"), Object::Array(parameters)),
        (
            OxStr::from("return_type"),
            Object::String(OxStr::from(metadata.returns.to_string().as_str())),
        ),
        (OxStr::from("since"), Object::Integer(i64::from(metadata.since))),
        (OxStr::from("method"), Object::Boolean(metadata.method)),
        (OxStr::from("fast"), Object::Boolean(metadata.fast)),
        (OxStr::from("textlock"), Object::Boolean(metadata.textlock)),
    ];
    if let Some(since) = metadata.deprecated_since {
        fields.push((OxStr::from("deprecated_since"), Object::Integer(i64::from(since))));
    }
    Dict(fields)
}

/// Appends one literal byte, quoting NUL and `K_SPECIAL` the way key sequences
/// are stored internally (src/nvim/keycodes.h:15-20,32-45,70-89).
fn push_encoded(output: &mut Vec<u8>, byte: u8) {
    match byte {
        0 => output.extend_from_slice(&[K_SPECIAL, KS_ZERO, KE_FILLER]),
        K_SPECIAL => output.extend_from_slice(&[K_SPECIAL, KS_SPECIAL, KE_FILLER]),
        value => output.push(value),
    }
}

fn replace_termcode_notation(input: &[u8], do_lt: bool, special: bool) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut offset = 0;
    while offset < input.len() {
        if !special || input[offset] != b'<' {
            push_encoded(&mut output, input[offset]);
            offset += 1;
            continue;
        }
        let Some(relative_end) = input[offset + 1..].iter().position(|byte| *byte == b'>') else {
            for byte in &input[offset..] {
                push_encoded(&mut output, *byte);
            }
            break;
        };
        let end = offset + 1 + relative_end;
        let notation = &input[offset + 1..end];
        if let Some(byte) = simple_termcode(notation, do_lt) {
            push_encoded(&mut output, byte);
        } else if let Some(code) = special_keycode(notation) {
            output.extend_from_slice(&code);
        } else {
            for byte in &input[offset..=end] {
                push_encoded(&mut output, *byte);
            }
        }
        offset = end + 1;
    }
    output
}

/// Encodes a named special key (`<Up>`, `<F1>`, `<BS>`, `<Tab>`, ...) as its
/// core `(KS_xxx, KE_xxx)` pair; the third byte is a KE_* enum value when the
/// key has no termcap name, otherwise a termcap byte (src/nvim/keycodes.h
/// `TERMCAP2KEY`). Function keys F13-F63, the keypad set, the shifted/control
/// cursor keys, and the extra xterm keys are all dedicated three-byte keys.
fn special_pair(name: &[u8]) -> Option<(u8, u8)> {
    // Function keys F13-F63 are a numeric run with a computed third byte.
    if let Some(digits) = name.strip_prefix(b"f") {
        if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) {
            let n: u8 = std::str::from_utf8(digits).ok()?.parse().ok()?;
            return function_key(n);
        }
    }
    Some(match name {
        // Base cursor/navigation keys (termcap byte pairs).
        b"up" => (b'k', b'u'),
        b"down" => (b'k', b'd'),
        b"left" => (b'k', b'l'),
        b"right" => (b'k', b'r'),
        b"home" => (b'k', b'h'),
        b"end" => (b'@', b'7'),
        b"pageup" => (b'k', b'P'),
        b"pagedown" => (b'k', b'N'),
        b"del" | b"delete" => (b'k', b'D'),
        b"bs" | b"backspace" => (b'k', b'b'),
        b"tab" => (KS_EXTRA, 54), // KE_TAB
        b"insert" | b"ins" => (b'k', b'I'),
        b"help" => (b'%', b'1'),
        b"undo" => (b'&', b'8'),
        b"find" => (b'@', b'0'),
        b"select" => (b'*', b'6'), // K_KSELECT

        // Shifted/control cursor keys and shifted Tab (modifier_keys_table).
        b"s-tab" => (b'k', b'B'), // K_S_TAB
        b"s-up" => (KS_EXTRA, 4), // KE_S_UP
        b"s-down" => (KS_EXTRA, 5), // KE_S_DOWN
        b"s-left" => (b'#', b'4'),
        b"s-right" => (b'%', b'i'),
        b"s-home" => (b'#', b'2'),
        b"s-end" => (b'*', b'7'),
        b"s-del" => (b'*', b'4'),
        b"c-left" => (KS_EXTRA, 85), // KE_C_LEFT
        b"c-right" => (KS_EXTRA, 86), // KE_C_RIGHT
        b"c-home" => (KS_EXTRA, 87), // KE_C_HOME
        b"c-end" => (KS_EXTRA, 88), // KE_C_END

        // Keypad keys: k0-k9 and k-prefixed navigation/arithmetic.
        b"k0" => (b'K', b'C'),
        b"k1" => (b'K', b'D'),
        b"k2" => (b'K', b'E'),
        b"k3" => (b'K', b'F'),
        b"k4" => (b'K', b'G'),
        b"k5" => (b'K', b'H'),
        b"k6" => (b'K', b'I'),
        b"k7" => (b'K', b'J'),
        b"k8" => (b'K', b'K'),
        b"k9" => (b'K', b'L'),
        b"kup" => (b'K', b'u'),
        b"kdown" => (b'K', b'd'),
        b"kleft" => (b'K', b'l'),
        b"kright" => (b'K', b'r'),
        b"khome" => (b'K', b'1'),
        b"kend" => (b'K', b'4'),
        b"kpageup" => (b'K', b'3'),
        b"kpagedown" => (b'K', b'5'),
        b"korigin" => (b'K', b'2'),
        b"kplus" => (b'K', b'6'),
        b"kminus" => (b'K', b'7'),
        b"kdivide" => (b'K', b'8'),
        b"kmultiply" => (b'K', b'9'),
        b"kenter" => (b'K', b'A'),
        b"kpoint" => (b'K', b'B'),
        b"kcomma" => (b'K', b'M'),
        b"kequal" => (b'K', b'N'),
        b"kinsert" => (KS_EXTRA, 79), // KE_KINS
        b"kdel" => (KS_EXTRA, 80), // KE_KDEL

        // Function keys F1-F12 (F13-F63 handled by the numeric run above).
        b"f1" => (b'k', b'1'),
        b"f2" => (b'k', b'2'),
        b"f3" => (b'k', b'3'),
        b"f4" => (b'k', b'4'),
        b"f5" => (b'k', b'5'),
        b"f6" => (b'k', b'6'),
        b"f7" => (b'k', b'7'),
        b"f8" => (b'k', b'8'),
        b"f9" => (b'k', b'9'),
        b"f10" => (b'k', b';'),
        b"f11" => (b'F', b'1'),
        b"f12" => (b'F', b'2'),

        // Shifted function keys F1-F12.
        b"s-f1" => (KS_EXTRA, 6),
        b"s-f2" => (KS_EXTRA, 7),
        b"s-f3" => (KS_EXTRA, 8),
        b"s-f4" => (KS_EXTRA, 9),
        b"s-f5" => (KS_EXTRA, 10),
        b"s-f6" => (KS_EXTRA, 11),
        b"s-f7" => (KS_EXTRA, 12),
        b"s-f8" => (KS_EXTRA, 13),
        b"s-f9" => (KS_EXTRA, 14),
        b"s-f10" => (KS_EXTRA, 15),
        b"s-f11" => (KS_EXTRA, 16),
        b"s-f12" => (KS_EXTRA, 17),

        // Extra vt100 xterm keys and shifted variants.
        b"xup" => (KS_EXTRA, 65), // KE_XUP
        b"xdown" => (KS_EXTRA, 66), // KE_XDOWN
        b"xleft" => (KS_EXTRA, 67), // KE_XLEFT
        b"xright" => (KS_EXTRA, 68), // KE_XRIGHT
        b"xhome" => (KS_EXTRA, 63), // KE_XHOME
        b"zhome" => (KS_EXTRA, 64), // KE_ZHOME
        b"xend" => (KS_EXTRA, 61), // KE_XEND
        b"zend" => (KS_EXTRA, 62), // KE_ZEND
        b"xf1" => (KS_EXTRA, 57),
        b"xf2" => (KS_EXTRA, 58),
        b"xf3" => (KS_EXTRA, 59),
        b"xf4" => (KS_EXTRA, 60),
        b"s-xf1" => (KS_EXTRA, 71),
        b"s-xf2" => (KS_EXTRA, 72),
        b"s-xf3" => (KS_EXTRA, 73),
        b"s-xf4" => (KS_EXTRA, 74),
        _ => return None,
    })
}

/// Computes the `(second, third)` bytes for a function key F1-F63 following
/// keycodes.h (`K_F1`..`K_F63` termcap byte runs).
fn function_key(n: u8) -> Option<(u8, u8)> {
    match n {
        1..=10 => Some((b'k', *b"123456789;".get((n - 1) as usize)?)),
        11..=12 => Some((b'F', n - 11 + b'1')),
        13..=40 => Some((b'F', *b"3456789ABCDEFGHIJKLMNOPQRSTU".get((n - 13) as usize)?)),
        41..=63 => Some((b'F', *b"VWXYZabcdefghijklmnopqr".get((n - 41) as usize)?)),
        _ => None,
    }
}

/// Translates a `<Notation>` special-key name to the internal three-byte keycode
/// form `K_SPECIAL second third`, reusing the editor's `Keys::special` encoder.
fn special_keycode(notation: &[u8]) -> Option<[u8; 3]> {
    let lower = notation
        .iter()
        .map(|byte| byte.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let pair = special_pair(&lower)?;
    let encoded = Keys::special(pair.0, pair.1).ok()?;
    let bytes = encoded.as_bytes();
    Some([bytes[0], bytes[1], bytes[2]])
}

fn simple_termcode(notation: &[u8], do_lt: bool) -> Option<u8> {
    if notation.eq_ignore_ascii_case(b"lt") {
        return do_lt.then_some(b'<');
    }
    let named = [
        (&b"cr"[..], b'\r'),
        (&b"enter"[..], b'\r'),
        (&b"esc"[..], 0x1b),
        (&b"space"[..], b' '),
        (&b"bar"[..], b'|'),
        (&b"bslash"[..], b'\\'),
        (&b"nul"[..], 0),
    ];
    if let Some((_, value)) = named.iter().find(|(name, _)| notation.eq_ignore_ascii_case(name)) {
        return Some(*value);
    }
    if notation.len() == 3 && notation[0].eq_ignore_ascii_case(&b'c') && notation[1] == b'-' {
        let key = notation[2].to_ascii_uppercase();
        if key == b'?' {
            return Some(0x7f);
        }
        if (b'@'..=b'_').contains(&key) {
            return Some(key & 0x1f);
        }
    }
    None
}

fn dict_string(dict: &Dict, key: &str) -> Result<Option<OxStr>, ApiError> {
    match dict.get(&OxStr::from(key)) {
        None => Ok(None),
        Some(Object::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ApiError::validation(format!("opts.{key} must be String"))),
    }
}

fn dict_bool(dict: &Dict, key: &str) -> Result<Option<bool>, ApiError> {
    match dict.get(&OxStr::from(key)) {
        None => Ok(None),
        Some(Object::Boolean(value)) => Ok(Some(*value)),
        Some(_) => Err(ApiError::validation(format!("opts.{key} must be Boolean"))),
    }
}

fn dict_handle<T, E>(
    dict: &Dict,
    key: &str,
    convert: impl FnOnce(i64) -> Result<T, E>,
) -> Result<Option<T>, ApiError>
where
    E: std::fmt::Display,
{
    match dict.get(&OxStr::from(key)) {
        None => Ok(None),
        Some(Object::Integer(value)) => convert(*value).map(Some).map_err(exception),
        Some(Object::Buffer(value)) if key == "buf" => convert(i64::from(*value)).map(Some).map_err(exception),
        Some(Object::Window(value)) if key == "win" => convert(i64::from(*value)).map(Some).map_err(exception),
        Some(_) => Err(ApiError::validation(format!("opts.{key} must be a handle"))),
    }
}

fn reject_unknown_option_keys(opts: &Dict) -> Result<(), ApiError> {
    for (key, _) in opts.iter() {
        if !matches!(
            key.as_bytes(),
            b"buf" | b"win" | b"tab" | b"filetype" | b"scope" | b"dry_run" | b"operation"
        ) {
            return Err(ApiError::validation(format!(
                "Invalid key: {}",
                key.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn validate_echo_chunks(chunks: &[Object]) -> Result<(), ApiError> {
    for chunk in chunks {
        let Object::Array(values) = chunk else {
            return Err(ApiError::validation("Each echo chunk must be an Array"));
        };
        if !(1..=2).contains(&values.len()) || !matches!(values.first(), Some(Object::String(_))) {
            return Err(ApiError::validation(
                "Each echo chunk must contain text and an optional highlight group",
            ));
        }
        if values.len() == 2
            && !matches!(values.get(1), Some(Object::String(_) | Object::Integer(_)))
        {
            return Err(ApiError::validation("Echo highlight group must be String or Integer"));
        }
    }
    Ok(())
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_get_current_buf__API_META(), nvim_get_current_buf__API_DISPATCH)?;
    registry.register(nvim_set_current_buf__API_META(), nvim_set_current_buf__API_DISPATCH)?;
    registry.register(nvim_get_current_win__API_META(), nvim_get_current_win__API_DISPATCH)?;
    registry.register(nvim_set_current_win__API_META(), nvim_set_current_win__API_DISPATCH)?;
    registry.register(nvim_get_current_tabpage__API_META(), nvim_get_current_tabpage__API_DISPATCH)?;
    registry.register(nvim_set_current_tabpage__API_META(), nvim_set_current_tabpage__API_DISPATCH)?;
    registry.register(nvim_list_bufs__API_META(), nvim_list_bufs__API_DISPATCH)?;
    registry.register(nvim_list_wins__API_META(), nvim_list_wins__API_DISPATCH)?;
    registry.register(nvim_list_tabpages__API_META(), nvim_list_tabpages__API_DISPATCH)?;
    registry.register(nvim_get_api_info__API_META(), nvim_get_api_info__API_DISPATCH)?;
    registry.register(nvim_command__API_META(), nvim_command__API_DISPATCH)?;
    registry.register(nvim_eval__API_META(), nvim_eval__API_DISPATCH)?;
    registry.register(nvim_call_function__API_META(), nvim_call_function__API_DISPATCH)?;
    registry.register(nvim_get_vvar__API_META(), nvim_get_vvar__API_DISPATCH)?;
    registry.register(nvim_set_vvar__API_META(), nvim_set_vvar__API_DISPATCH)?;
    registry.register(nvim_get_option__API_META(), nvim_get_option__API_DISPATCH)?;
    registry.register(nvim_set_option__API_META(), nvim_set_option__API_DISPATCH)?;
    registry.register(nvim_get_option_value__API_META(), nvim_get_option_value__API_DISPATCH)?;
    registry.register(nvim_set_option_value__API_META(), nvim_set_option_value__API_DISPATCH)?;
    registry.register(nvim_input__API_META(), nvim_input__API_DISPATCH)?;
    registry.register(nvim_replace_termcodes__API_META(), nvim_replace_termcodes__API_DISPATCH)?;
    registry.register(nvim_strwidth__API_META(), nvim_strwidth__API_DISPATCH)?;
    registry.register(nvim_err_writeln__API_META(), nvim_err_writeln__API_DISPATCH)?;
    registry.register(nvim_echo__API_META(), nvim_echo__API_DISPATCH)?;
    Ok(())
}
