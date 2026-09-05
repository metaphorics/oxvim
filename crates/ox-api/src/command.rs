//! User-command API surface over the live Ex-command host registry.
//!
//! Every handler delegates definition, deletion, listing, and parsing to the
//! installed [`crate::CommandExecutor`]; this module owns only argument
//! validation and dictionary serialization, mirroring upstream
//! `nvim/api/command.c` and `usercmd.c:commands_array`.

use crate::{ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError, api};
use ox_editor::{
    OptionValue, SearchDirection, SearchState, UserCommand, UserCommandComplete, UserCommandRange,
};
use ox_excmd::{
    AddrType, Address, AddressBase, CommandFlags, ExCommand, ModifierKind, Range, RangeKind,
    ResolvedCommand, effective_addr_type, effective_flags,
};
use ox_text::Position;

use crate::buffer::{resolve_buffer, resolve_buffer_if_valid};
use crate::global::{optional_bool, reject_keys};
use crate::runtime::with_command_executor;
use crate::session::ApiSession;

/// `:command` attributes accepted by the create/delete API surface.
const COMMAND_OPTS: &[&str] = &[
    "nargs",
    "range",
    "count",
    "addr",
    "bang",
    "bar",
    "register",
    "keepscript",
    "complete",
    "preview",
    "desc",
    "force",
];

/// Commands whose argument tail is `[lhs, rhs-with-spaces]`
/// (`ex_docmd.c:is_map_cmd`).
const MAP_COMMANDS: &[&str] = &[
    "map",
    "nmap",
    "vmap",
    "xmap",
    "smap",
    "omap",
    "imap",
    "lmap",
    "cmap",
    "tmap",
    "noremap",
    "nnoremap",
    "vnoremap",
    "xnoremap",
    "snoremap",
    "onoremap",
    "inoremap",
    "lnoremap",
    "cnoremap",
    "tnoremap",
    "unmap",
    "nunmap",
    "vunmap",
    "xunmap",
    "sunmap",
    "ounmap",
    "iunmap",
    "lunmap",
    "cunmap",
    "tunmap",
    "mapclear",
    "nmapclear",
    "vmapclear",
    "xmapclear",
    "smapclear",
    "omapclear",
    "imapclear",
    "lmapclear",
    "cmapclear",
    "tmapclear",
    "abbreviate",
    "iabbrev",
    "cabbrev",
    "abclear",
    "iabclear",
    "cabclear",
];

/// api/command.c `nvim_parse_cmd`: parse one command line without running it.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes command text and options as owned values"
)]
#[api(since = 10, fast)]
pub fn nvim_parse_cmd(session: &ApiSession, command: OxStr, opts: Dict) -> Result<Dict, ApiError> {
    reject_keys(&opts, &[])?;
    if command.as_bytes().contains(&b'\n') {
        return Err(ApiError::validation("Command cannot contain newlines"));
    }
    let line = std::str::from_utf8(command.as_bytes())
        .map_err(|_| ApiError::validation("Command must be valid UTF-8"))?;
    let commands = with_command_executor(session, |session, executor| {
        executor
            .parse_cmdline(session, line)
            .map_err(|error| ApiError::exception(format!("Parsing command-line: {error}")))
    })?;
    let Some(parsed) = commands.into_iter().next() else {
        return Err(ApiError::exception("Parsing command-line"));
    };
    serialize_parse_cmd(session, &parsed, line)
}

/// api/command.c `nvim_create_user_command`: define a global user command.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes command names, bodies, and options as owned values"
)]
#[api(since = 9)]
pub fn nvim_create_user_command(
    session: &ApiSession,
    name: OxStr,
    cmd: Object,
    opts: Dict,
) -> Result<(), ApiError> {
    create_user_command(session, None, &name, cmd, &opts)
}

/// api/command.c `nvim_buf_create_user_command`: define a buffer-local command.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes command names, bodies, and options as owned values"
)]
#[api(since = 9, method)]
pub fn nvim_buf_create_user_command(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
    cmd: Object,
    opts: Dict,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    create_user_command(session, Some(buffer), &name, cmd, &opts)
}

/// api/command.c `nvim_del_user_command`: delete a global user command.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes command names as owned strings"
)]
#[api(since = 9)]
pub fn nvim_del_user_command(session: &ApiSession, name: OxStr) -> Result<(), ApiError> {
    delete_user_command(session, None, &name)
}

/// api/command.c `nvim_buf_del_user_command`: delete a buffer-local command.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes command names as owned strings"
)]
#[api(since = 9, method)]
pub fn nvim_buf_del_user_command(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    delete_user_command(session, Some(buffer), &name)
}

/// api/command.c `nvim_get_commands`: list global user commands.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes command options as an owned dictionary"
)]
#[api(since = 4)]
pub fn nvim_get_commands(session: &ApiSession, opts: Dict) -> Result<Dict, ApiError> {
    reject_keys(&opts, &["builtin"])?;
    if optional_bool(&opts, "builtin")?.unwrap_or(false) {
        return Err(ApiError::validation("builtin=true not implemented"));
    }
    with_command_executor(session, |session, executor| {
        Ok(commands_array(executor.list_user_commands(session, None)?))
    })
}

/// api/command.c `nvim_buf_get_commands`: list buffer-local commands.
///
/// An invalid buffer yields an empty dictionary instead of an error, matching
/// upstream divergence from the create/delete handlers.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes command options as an owned dictionary"
)]
#[api(since = 4, method)]
pub fn nvim_buf_get_commands(
    session: &ApiSession,
    buffer: BufHandle,
    opts: Dict,
) -> Result<Dict, ApiError> {
    reject_keys(&opts, &["builtin"])?;
    if optional_bool(&opts, "builtin")?.unwrap_or(false) {
        return Ok(Dict(Vec::new()));
    }
    let Some(buffer) = resolve_buffer_if_valid(session, buffer) else {
        return Ok(Dict(Vec::new()));
    };
    with_command_executor(session, |session, executor| {
        Ok(commands_array(
            executor.list_user_commands(session, Some(buffer))?,
        ))
    })
}

/// Shared create path for the global and buffer-local handlers.
fn create_user_command(
    session: &ApiSession,
    buffer: Option<BufHandle>,
    name: &OxStr,
    cmd: Object,
    opts: &Dict,
) -> Result<(), ApiError> {
    let name = command_name(name)?;
    let (command, force) = build_user_command(&name, cmd, opts)?;
    with_command_executor(session, |session, executor| {
        executor
            .define_user_command(session, buffer, command, force)
            .map_err(|error| {
                // Only the hosts' known duplicate-definition failure becomes
                // the canonical validation error (live host reports it as
                // `E174: Command already exists: …`); reentrancy and other
                // host failures keep their original class and message.
                if error.message().contains("Command already exists") {
                    ApiError::validation(format!("Command already exists: {name}"))
                } else {
                    error
                }
            })
    })
}

/// Shared delete path; the only failure is a missing command.
fn delete_user_command(
    session: &ApiSession,
    buffer: Option<BufHandle>,
    name: &OxStr,
) -> Result<(), ApiError> {
    let name = std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("Command name must be valid UTF-8"))?
        .to_owned();
    with_command_executor(session, |session, executor| {
        executor
            .delete_user_command(session, buffer, &name)
            .map_err(|_| ApiError::exception(format!("Invalid command (not found): {name}")))
    })
}

/// Validates a command name (`usercmd.c:uc_validate_name` plus the uppercase
/// requirement), returning it as UTF-8 text.
fn command_name(name: &OxStr) -> Result<String, ApiError> {
    let text = std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("Command name must be valid UTF-8"))?;
    let bytes = text.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_alphabetic)
        || !bytes.iter().all(u8::is_ascii_alphanumeric)
    {
        return Err(ApiError::validation(format!(
            "Invalid command name: '{text}'"
        )));
    }
    if bytes[0].is_ascii_lowercase() {
        return Err(ApiError::validation(format!(
            "Invalid command name (must start with uppercase): '{text}'"
        )));
    }
    Ok(text.to_owned())
}

/// Builds a canonical [`UserCommand`] from create-time arguments, mirroring
/// `create_user_command` in `api/command.c` including the shared `def`
/// between `-range=N` and `-count=N` and the `ADDR_NONE` default domain.
#[expect(
    clippy::too_many_lines,
    reason = "one ordered pass preserves Neovim option precedence, shared range/count state, and exact validation errors"
)]
fn build_user_command(
    name: &str,
    cmd: Object,
    opts: &Dict,
) -> Result<(UserCommand, bool), ApiError> {
    reject_keys(opts, COMMAND_OPTS)?;
    if has_key(opts, "range") && has_key(opts, "count") {
        return Err(ApiError::validation("Cannot use both 'range' and 'count'"));
    }

    let nargs = parse_nargs(opts)?;
    let (completion, complete_arg) = match opts.get(&OxStr::from("complete")) {
        None => (None, None),
        Some(Object::LuaRef(reference)) => (
            Some(UserCommandComplete::Callback(lua_u64(
                *reference, "complete",
            )?)),
            None,
        ),
        Some(Object::String(value)) => {
            let text = text_of(value, "complete")?;
            // `custom,func` / `customlist,func` split into completion type
            // and argument (`usercmd.c:parse_complete_arg`).
            if let Some((kind, argument)) = text.split_once(',') {
                if !matches!(kind, "custom" | "customlist") || argument.is_empty() {
                    return Err(ApiError::validation(format!(
                        "Invalid complete value: {text}"
                    )));
                }
                (
                    Some(UserCommandComplete::Name(kind.to_owned())),
                    Some(argument.to_owned()),
                )
            } else {
                (Some(UserCommandComplete::Name(text)), None)
            }
        }
        Some(_) => {
            return Err(ApiError::validation(
                "Invalid 'complete': expected Function or String",
            ));
        }
    };
    if completion.is_some() && nargs == '0' {
        return Err(ApiError::validation("'complete' used without 'nargs'"));
    }

    let mut accepts_range = false;
    let mut accepts_count = false;
    let mut addr = AddrType::None;
    let mut default: Option<i64> = None;
    let mut default_range: Option<UserCommandRange> = None;

    match opts.get(&OxStr::from("range")) {
        None | Some(Object::Boolean(false)) => {}
        Some(Object::Boolean(true)) => {
            accepts_range = true;
            addr = AddrType::Lines;
            default_range = Some(UserCommandRange::Dot);
        }
        Some(Object::String(value)) => {
            if text_of(value, "range")? != "%" {
                return Err(ApiError::validation("Invalid 'range'"));
            }
            accepts_range = true;
            addr = AddrType::Lines;
            default_range = Some(UserCommandRange::Percent);
        }
        Some(Object::Integer(value)) => {
            accepts_range = true;
            addr = AddrType::Lines;
            default = Some(*value);
            default_range = Some(UserCommandRange::Count(*value));
        }
        Some(value) => {
            return Err(ApiError::validation(format!(
                "Invalid 'range': expected Boolean, String, or Integer, got {}",
                type_name(value)
            )));
        }
    }

    match opts.get(&OxStr::from("count")) {
        None | Some(Object::Boolean(false)) => {}
        Some(Object::Boolean(true)) => {
            accepts_count = true;
            accepts_range = true;
            addr = AddrType::Other;
            default = Some(0);
        }
        Some(Object::Integer(value)) => {
            accepts_count = true;
            accepts_range = true;
            addr = AddrType::Other;
            default = Some(*value);
        }
        Some(value) => {
            return Err(ApiError::validation(format!(
                "Invalid 'count': expected Boolean or Integer, got {}",
                type_name(value)
            )));
        }
    }

    if let Some(value) = opts.get(&OxStr::from("addr")) {
        let Object::String(text) = value else {
            return Err(ApiError::validation(format!(
                "Invalid 'addr': expected String, got {}",
                type_name(value)
            )));
        };
        addr = parse_addr_type(&text_of(text, "addr")?)?;
        accepts_range = true;
    }

    // `-count=N` shares upstream's `uc_def`, so the range string reports `N`
    // even when only `-count` selected the range acceptance.
    if accepts_range && default_range.is_none() {
        default_range = default.map(UserCommandRange::Count);
    }

    let accepts_bang = opt_bool(opts, "bang")?;
    let bar = opt_bool(opts, "bar")?;
    let accepts_register = opt_bool(opts, "register")?;
    let keepscript = opt_bool(opts, "keepscript")?;
    let force = opts
        .get(&OxStr::from("force"))
        .map(|value| match value {
            Object::Boolean(value) => Ok(*value),
            value => Err(ApiError::validation(format!(
                "Invalid 'force': expected Boolean, got {}",
                type_name(value)
            ))),
        })
        .transpose()?
        .unwrap_or(true);

    let preview = match opts.get(&OxStr::from("preview")) {
        None => None,
        Some(Object::LuaRef(reference)) => Some(lua_u64(*reference, "preview")?),
        Some(value) => {
            return Err(ApiError::validation(format!(
                "Invalid 'preview': expected Function, got {}",
                type_name(value)
            )));
        }
    };
    let desc = match opts.get(&OxStr::from("desc")) {
        None => String::new(),
        Some(Object::String(value)) => text_of(value, "desc")?,
        Some(value) => {
            return Err(ApiError::validation(format!(
                "Invalid 'desc': expected String, got {}",
                type_name(value)
            )));
        }
    };

    let (body, callback) = match cmd {
        Object::String(value) => (text_of(&value, "command")?, None),
        Object::LuaRef(reference) => (String::new(), Some(lua_u64(reference, "command")?)),
        _ => {
            return Err(ApiError::validation(
                "Invalid 'command': expected Function or String",
            ));
        }
    };
    let command = UserCommand {
        name: name.to_owned(),
        body,
        nargs,
        accepts_bang,
        accepts_range,
        accepts_register,
        accepts_count,
        bar,
        addr_type: addr,
        default_range,
        count_default: if accepts_count { default } else { None },
        desc,
        completion,
        complete_arg,
        callback,
        preview,
        // API-defined commands serialize with upstream's `SID_LUA`.
        script_context: ox_editor::script::SourceContext::default(),
        keepscript,
        script_id: -8,
    };
    Ok((command, force))
}

/// Parses the `nargs` option into its canonical character.
fn parse_nargs(opts: &Dict) -> Result<char, ApiError> {
    match opts.get(&OxStr::from("nargs")) {
        None | Some(Object::Integer(0)) => Ok('0'),
        Some(Object::Integer(1)) => Ok('1'),
        Some(Object::Integer(value)) => {
            Err(ApiError::validation(format!("Invalid 'nargs': {value}")))
        }
        Some(Object::String(value)) => {
            let text = text_of(value, "nargs")?;
            let bytes = text.as_bytes();
            if bytes.len() > 1 {
                return Err(ApiError::validation(format!("Invalid 'nargs': '{text}'")));
            }
            match bytes.first() {
                Some(b'*') => Ok('*'),
                Some(b'?') => Ok('?'),
                Some(b'+') => Ok('+'),
                Some(b'_') => Ok('_'),
                _ => Err(ApiError::validation(format!("Invalid 'nargs': '{text}'"))),
            }
        }
        Some(_) => Err(ApiError::validation("Invalid 'nargs'")),
    }
}

/// Maps an `-addr=` name onto its domain (`usercmd.c:parse_addr_type_arg`).
fn parse_addr_type(value: &str) -> Result<AddrType, ApiError> {
    match value {
        "arguments" => Ok(AddrType::Arguments),
        "lines" => Ok(AddrType::Lines),
        "loaded_buffers" => Ok(AddrType::LoadedBuffers),
        "tabs" => Ok(AddrType::Tabs),
        "buffers" => Ok(AddrType::Buffers),
        "windows" => Ok(AddrType::Windows),
        "quickfix" => Ok(AddrType::QuickFix),
        "other" => Ok(AddrType::Other),
        _ => Err(ApiError::validation(format!("Invalid 'addr': '{value}'"))),
    }
}

/// api/usercmd.c `commands_array`: name → command-info dictionary.
fn commands_array(commands: Vec<UserCommand>) -> Dict {
    Dict(
        commands
            .into_iter()
            .map(|command| (OxStr::from(command.name.as_str()), command_info(&command)))
            .collect(),
    )
}

/// Serializes one command for `nvim_get_commands`, preserving Lua callback,
/// completion, and preview references.
fn command_info(command: &UserCommand) -> Object {
    let mut entries: Vec<(OxStr, Object)> = Vec::new();
    entries.push((
        OxStr::from("name"),
        Object::String(OxStr::from(command.name.as_str())),
    ));
    entries.push((
        OxStr::from("definition"),
        Object::String(OxStr::from(if command.callback.is_some() {
            ""
        } else {
            command.body.as_str()
        })),
    ));
    entries.push((
        OxStr::from("desc"),
        Object::String(OxStr::from(command.desc.as_str())),
    ));
    entries.push((OxStr::from("script_id"), Object::Integer(command.script_id)));
    entries.push((OxStr::from("bang"), Object::Boolean(command.accepts_bang)));
    entries.push((OxStr::from("bar"), Object::Boolean(command.bar)));
    entries.push((
        OxStr::from("register"),
        Object::Boolean(command.accepts_register),
    ));
    entries.push((
        OxStr::from("keepscript"),
        Object::Boolean(command.keepscript),
    ));
    if let Some(preview) = command.preview {
        entries.push((OxStr::from("preview"), Object::LuaRef(lua_ref(preview))));
    }
    if let Some(callback) = command.callback {
        entries.push((OxStr::from("callback"), Object::LuaRef(lua_ref(callback))));
    }
    entries.push((
        OxStr::from("nargs"),
        Object::String(OxStr::from(command.nargs.to_string().as_str())),
    ));
    entries.push((
        OxStr::from("complete"),
        match &command.completion {
            Some(UserCommandComplete::Name(name)) => Object::String(OxStr::from(name.as_str())),
            Some(UserCommandComplete::Callback(reference)) => Object::LuaRef(lua_ref(*reference)),
            None => Object::Nil,
        },
    ));
    entries.push((
        OxStr::from("complete_arg"),
        match &command.complete_arg {
            Some(value) => Object::String(OxStr::from(value.as_str())),
            None => Object::Nil,
        },
    ));
    let count_entries = if command.accepts_count {
        vec![(
            OxStr::from("count"),
            Object::String(OxStr::from(count_text(command).as_str())),
        )]
    } else {
        vec![(OxStr::from("count"), Object::Nil)]
    };
    entries.extend(count_entries);
    let range_entries = if command.accepts_range {
        vec![(
            OxStr::from("range"),
            Object::String(OxStr::from(range_text(command).as_str())),
        )]
    } else {
        vec![(OxStr::from("range"), Object::Nil)]
    };
    entries.extend(range_entries);
    entries.push((OxStr::from("addr"), addr_full_name(command.addr_type)));
    Object::Dict(Dict(entries))
}

/// The `count` string for `commands_array` (`-count=N` reports N, bare
/// `-count` reports 0).
fn count_text(command: &UserCommand) -> String {
    match command.count_default {
        Some(default) if default >= 0 => default.to_string(),
        _ => "0".to_owned(),
    }
}

/// The `range` string for `commands_array` (`-range=%`, `-range=N`, plain
/// `-range`, or no default).
fn range_text(command: &UserCommand) -> String {
    match command.default_range {
        Some(UserCommandRange::Percent) => "%".to_owned(),
        Some(UserCommandRange::Count(default)) if default >= 0 => default.to_string(),
        _ => ".".to_owned(),
    }
}

/// Folds a trailing `-count=N` into the resolved range, mirroring
/// `set_cmd_count`: a trailing count on a line-domain command re-anchors
/// `line1` at the old `line2` and extends `line2` by `count - 1`; other
/// domains just replace `line2`. Returns the effective `count` value.
fn fold_count_into_range(
    range_values: &mut Option<Vec<i64>>,
    count: Option<u64>,
    lines_domain: bool,
) -> Option<i64> {
    match (&mut *range_values, count) {
        (Some(values), Some(count)) => {
            let count = i64::try_from(count).unwrap_or(i64::MAX);
            if lines_domain {
                let line2 = values
                    .last()
                    .copied()
                    .unwrap_or(1)
                    .saturating_add(count.saturating_sub(1));
                if values.len() == 1 {
                    values.push(line2);
                } else if let Some(last) = values.last().copied() {
                    values[0] = last;
                    values[1] = line2;
                }
            } else if let Some(last) = values.last_mut() {
                *last = count;
            }
            values.last().copied()
        }
        (Some(values), None) => values.last().copied(),
        (None, Some(count)) => {
            let count = i64::try_from(count).unwrap_or(i64::MAX);
            *range_values = Some(vec![count]);
            Some(count)
        }
        (None, None) => None,
    }
}

/// Serializes the first parsed command (`api/command.c:nvim_parse_cmd`).
fn serialize_parse_cmd(
    session: &ApiSession,
    parsed: &ExCommand,
    input: &str,
) -> Result<Dict, ApiError> {
    let flags = effective_flags(&parsed.command);
    let name = match &parsed.command {
        ResolvedCommand::Builtin(spec) => spec.name,
        ResolvedCommand::User(info) => info.name.as_str(),
        ResolvedCommand::RangeOnly => "",
    };

    let mut entries: Vec<(OxStr, Object)> = Vec::new();
    entries.push((OxStr::from("cmd"), Object::String(OxStr::from(name))));
    let lines_domain = matches!(effective_addr_type(&parsed.command), AddrType::Lines);
    let mut range_values = match parsed.range.as_ref() {
        Some(range) if flags.contains(CommandFlags::RANGE) => Some(resolve_range(session, range)?),
        _ => None,
    };
    let mut count_value: Option<i64> = None;
    if flags.contains(CommandFlags::COUNT) {
        count_value = fold_count_into_range(&mut range_values, parsed.count, lines_domain);
    }
    if let Some(values) = range_values {
        entries.push((
            OxStr::from("range"),
            Object::Array(values.into_iter().map(Object::Integer).collect()),
        ));
    }
    if let Some(count) = count_value {
        entries.push((OxStr::from("count"), Object::Integer(count)));
    }

    // REGSTR commands always carry `reg`, empty when no register was given.
    if flags.contains(CommandFlags::REGSTR) {
        entries.push((
            OxStr::from("reg"),
            Object::String(OxStr::from(
                parsed
                    .register
                    .map(|register| register.to_string())
                    .unwrap_or_default()
                    .as_str(),
            )),
        ));
    }
    entries.push((OxStr::from("bang"), Object::Boolean(parsed.bang)));
    entries.push((OxStr::from("args"), args_object(name, parsed, flags)));
    entries.push((
        OxStr::from("nargs"),
        Object::String(OxStr::from(nargs_from_flags(flags).to_string().as_str())),
    ));
    entries.push((
        OxStr::from("addr"),
        Object::String(OxStr::from(addr_short_name(effective_addr_type(
            &parsed.command,
        )))),
    ));
    entries.push((
        OxStr::from("nextcmd"),
        Object::String(OxStr::from(
            input[parsed.span.end..].trim_start_matches('|').trim(),
        )),
    ));
    entries.push((
        OxStr::from("mods"),
        Object::Dict(mods_dict(session, parsed)),
    ));
    entries.push((
        OxStr::from("magic"),
        Object::Dict(Dict(vec![
            (
                OxStr::from("file"),
                Object::Boolean(flags.contains(CommandFlags::XFILE)),
            ),
            (
                OxStr::from("bar"),
                Object::Boolean(flags.contains(CommandFlags::TRLBAR)),
            ),
        ])),
    ));
    Ok(Dict(entries))
}

/// Splits parsed arguments per `nvim_parse_cmd` rules: mapping commands keep
/// `[lhs, rhs]`, `NOSPC` commands keep one verbatim argument, and everything
/// else splits on unescaped whitespace (`uc_split_args_iter`).
fn args_object(name: &str, parsed: &ExCommand, flags: CommandFlags) -> Object {
    if parsed.args.is_empty() {
        return Object::Array(Vec::new());
    }
    let values: Vec<String> = if !name.is_empty() && MAP_COMMANDS.contains(&name) {
        map_command_args(&parsed.args)
    } else if flags.contains(CommandFlags::NOSPC) {
        vec![parsed.args.clone()]
    } else {
        split_user_command_args(&parsed.args)
    };
    Object::Array(
        values
            .into_iter()
            .map(|value| Object::String(OxStr::from(value.as_str())))
            .collect(),
    )
}

/// `api/command.c:parse_map_cmd`: lhs up to whitespace, rhs verbatim.
fn map_command_args(arg: &str) -> Vec<String> {
    let end = arg
        .bytes()
        .position(|byte| byte.is_ascii_whitespace())
        .unwrap_or(arg.len());
    let mut values = vec![arg[..end].to_owned()];
    let rhs = arg[end..].trim_start();
    if !rhs.is_empty() {
        values.push(rhs.to_owned());
    }
    values
}

/// `usercmd.c:uc_split_args_iter`: whitespace split honoring `\ ` and `\\`
/// escapes, dropping empty segments.
pub(crate) fn split_user_command_args(arg: &str) -> Vec<String> {
    let bytes = arg.as_bytes();
    let len = bytes.len();
    let mut values: Vec<String> = Vec::new();
    if len == 0 {
        return values;
    }
    let mut end = 0usize;
    loop {
        let mut pos = end;
        while pos < len && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let mut chunk: Vec<u8> = Vec::new();
        let mut done = true;
        while pos < len - 1 {
            if bytes[pos] == b'\\'
                && (bytes[pos + 1] == b'\\' || bytes[pos + 1].is_ascii_whitespace())
            {
                pos += 1;
                chunk.push(bytes[pos]);
            } else {
                chunk.push(bytes[pos]);
            }
            if bytes[pos + 1].is_ascii_whitespace() {
                end = pos + 1;
                done = false;
                break;
            }
            pos += 1;
        }
        if done && pos < len && !bytes[pos].is_ascii_whitespace() {
            chunk.push(bytes[pos]);
        }
        if !chunk.is_empty() {
            values.push(String::from_utf8_lossy(&chunk).into_owned());
        }
        if done {
            return values;
        }
    }
}

/// `nvim_parse_cmd` nargs derivation from effective flags; user commands fold
/// their stored argument shape into the same flag set upstream uses.
fn nargs_from_flags(flags: CommandFlags) -> char {
    if !flags.contains(CommandFlags::EXTRA) {
        '0'
    } else if flags.contains(CommandFlags::NOSPC) {
        if flags.contains(CommandFlags::NEEDARG) {
            '1'
        } else {
            '?'
        }
    } else if flags.contains(CommandFlags::NEEDARG) {
        '+'
    } else {
        '*'
    }
}

/// Short domain names for `nvim_parse_cmd` (`nvim_parse_cmd` address switch).
fn addr_short_name(addr: AddrType) -> &'static str {
    match addr {
        AddrType::Lines => "line",
        AddrType::Arguments => "arg",
        AddrType::Buffers => "buf",
        AddrType::LoadedBuffers => "load",
        AddrType::Windows => "win",
        AddrType::Tabs => "tab",
        AddrType::QuickFix => "qf",
        AddrType::None => "none",
        AddrType::Other | AddrType::Unsigned | AddrType::TabsRelative | AddrType::QuickFixValid => {
            "?"
        }
    }
}

/// Full domain names for `commands_array` (`addr_type_complete`); `lines` and
/// the domains absent from the upstream table serialize as Nil.
fn addr_full_name(addr: AddrType) -> Object {
    let name = match addr {
        AddrType::Arguments => "arguments",
        AddrType::LoadedBuffers => "loaded_buffers",
        AddrType::Tabs => "tabs",
        AddrType::Buffers => "buffers",
        AddrType::Windows => "windows",
        AddrType::QuickFix => "quickfix",
        AddrType::Other => "?",
        AddrType::Lines
        | AddrType::None
        | AddrType::Unsigned
        | AddrType::TabsRelative
        | AddrType::QuickFixValid => return Object::Nil,
    };
    Object::String(OxStr::from(name))
}

/// Serializes command modifiers with upstream's key order and defaults.
#[expect(
    clippy::too_many_lines,
    reason = "single ordered pass fills upstream's fixed smods key set; splitting would scatter the per-modifier output state"
)]
fn mods_dict(session: &ApiSession, parsed: &ExCommand) -> Dict {
    let mut silent = false;
    let mut emsg_silent = false;
    let mut unsilent = false;
    let mut sandbox = false;
    let mut noautocmd = false;
    let mut browse = false;
    let mut confirm = false;
    let mut hide = false;
    let mut horizontal = false;
    let mut keepalt = false;
    let mut keepjumps = false;
    let mut keepmarks = false;
    let mut keeppatterns = false;
    let mut lockmarks = false;
    let mut noswapfile = false;
    let mut vertical = false;
    let mut tab: Option<i64> = None;
    let mut verbose: Option<i64> = None;
    let mut filter_pattern = String::new();
    let mut filter_force = false;
    let mut split = "";

    for modifier in &parsed.modifiers {
        match modifier.kind {
            ModifierKind::AboveLeft | ModifierKind::LeftAbove => split = "aboveleft",
            ModifierKind::BelowRight | ModifierKind::RightBelow => split = "belowright",
            ModifierKind::BotRight => split = "botright",
            ModifierKind::TopLeft => split = "topleft",
            ModifierKind::Browse => browse = true,
            ModifierKind::Confirm => confirm = true,
            ModifierKind::Filter => {
                if let Some(pattern) = modifier.pattern.as_deref() {
                    pattern.clone_into(&mut filter_pattern);
                }
                filter_force = modifier.bang;
            }
            ModifierKind::Hide => hide = true,
            ModifierKind::Horizontal => horizontal = true,
            ModifierKind::KeepAlt => keepalt = true,
            ModifierKind::KeepJumps => keepjumps = true,
            ModifierKind::KeepMarks => keepmarks = true,
            ModifierKind::KeepPatterns => keeppatterns = true,
            ModifierKind::LockMarks => lockmarks = true,
            ModifierKind::NoAutocmd => noautocmd = true,
            ModifierKind::NoSwapfile => noswapfile = true,
            ModifierKind::Sandbox => sandbox = true,
            ModifierKind::Silent => {
                silent = true;
                if modifier.bang {
                    emsg_silent = true;
                }
            }
            // A bare `:tab` resolves against the current tabpage upstream;
            // an explicit count is the raw tab number.
            ModifierKind::Tab => {
                tab = Some(match modifier.count {
                    Some(count) => i64::try_from(count).unwrap_or(i64::MAX),
                    None => session
                        .with_editor(|editor| {
                            editor
                                .current_tabpage()
                                .and_then(|tab| editor.tabpage_index(tab))
                        })
                        .map_or(-1, |index| i64::try_from(index).unwrap_or(i64::MAX)),
                });
            }
            ModifierKind::Unsilent => unsilent = true,
            ModifierKind::Verbose => {
                verbose = Some(match modifier.count {
                    Some(count) => i64::try_from(count).unwrap_or(i64::MAX),
                    None => -1,
                });
            }
            ModifierKind::Vertical => vertical = true,
        }
    }

    Dict(vec![
        (
            OxStr::from("filter"),
            Object::Dict(Dict(vec![
                (
                    OxStr::from("pattern"),
                    Object::String(OxStr::from(filter_pattern.as_str())),
                ),
                (OxStr::from("force"), Object::Boolean(filter_force)),
            ])),
        ),
        (OxStr::from("silent"), Object::Boolean(silent)),
        (OxStr::from("emsg_silent"), Object::Boolean(emsg_silent)),
        (OxStr::from("unsilent"), Object::Boolean(unsilent)),
        (OxStr::from("sandbox"), Object::Boolean(sandbox)),
        (OxStr::from("noautocmd"), Object::Boolean(noautocmd)),
        (OxStr::from("tab"), Object::Integer(tab.unwrap_or(-1))),
        (
            OxStr::from("verbose"),
            Object::Integer(verbose.unwrap_or(-1)),
        ),
        (OxStr::from("browse"), Object::Boolean(browse)),
        (OxStr::from("confirm"), Object::Boolean(confirm)),
        (OxStr::from("hide"), Object::Boolean(hide)),
        (OxStr::from("keepalt"), Object::Boolean(keepalt)),
        (OxStr::from("keepjumps"), Object::Boolean(keepjumps)),
        (OxStr::from("keepmarks"), Object::Boolean(keepmarks)),
        (OxStr::from("keeppatterns"), Object::Boolean(keeppatterns)),
        (OxStr::from("lockmarks"), Object::Boolean(lockmarks)),
        (OxStr::from("noswapfile"), Object::Boolean(noswapfile)),
        (OxStr::from("vertical"), Object::Boolean(vertical)),
        (OxStr::from("horizontal"), Object::Boolean(horizontal)),
        (OxStr::from("split"), Object::String(OxStr::from(split))),
    ])
}

/// Resolves a parsed range into positive line numbers. Unresolvable marks,
/// searches, or buffers are upstream's `E16: Invalid range`.
fn resolve_range(session: &ApiSession, range: &Range) -> Result<Vec<i64>, ApiError> {
    let current = cursor_line(session);
    let last = last_line(session);
    match range.kind {
        RangeKind::WholeBuffer => Ok(vec![1, i64::try_from(last).unwrap_or(i64::MAX)]),
        RangeKind::Single => Ok(vec![resolve_address(
            session,
            range.start.as_ref().ok_or_else(invalid_range)?,
            current,
            last,
        )?]),
        RangeKind::Pair { .. } => {
            let start = resolve_address(
                session,
                range.start.as_ref().ok_or_else(invalid_range)?,
                current,
                last,
            )?;
            let end = resolve_address(
                session,
                range.end.as_ref().ok_or_else(invalid_range)?,
                current,
                last,
            )?;
            Ok(vec![start, end])
        }
    }
}

/// Resolves one address and its signed offsets.
fn resolve_address(
    session: &ApiSession,
    address: &Address,
    current: usize,
    last: usize,
) -> Result<i64, ApiError> {
    let mut value = match &address.base {
        AddressBase::Current => current,
        AddressBase::Last => last,
        AddressBase::Line(line) => usize::try_from(*line).map_err(|_| invalid_range())?,
        AddressBase::Mark(name) => session.with_editor(|editor| {
            editor
                .local_mark(editor.current_buffer().ok_or_else(invalid_range)?, *name)
                .ok()
                .flatten()
                .map_or(Err(invalid_range()), |position| Ok(position.lnum))
        })?,
        AddressBase::ForwardSearch(pattern) => {
            search_line(session, pattern, SearchDirection::Forward, current)?
        }
        AddressBase::BackwardSearch(pattern) => {
            search_line(session, pattern, SearchDirection::Backward, current)?
        }
    };
    for offset in &address.offsets {
        value = if *offset >= 0 {
            value.saturating_add(usize::try_from(*offset).unwrap_or(usize::MAX))
        } else {
            value.saturating_sub(usize::try_from(offset.unsigned_abs()).unwrap_or(usize::MAX))
        };
    }
    Ok(i64::try_from(value).unwrap_or(i64::MAX))
}

/// Runs a pattern search against the current buffer without touching search
/// history or the cursor; a miss is `E16` in parse context.
fn search_line(
    session: &ApiSession,
    pattern: &str,
    direction: SearchDirection,
    current: usize,
) -> Result<usize, ApiError> {
    session.with_editor(|editor| {
        let buffer = editor.current_buffer().ok_or_else(invalid_range)?;
        let state = editor.buffer(buffer).map_err(|_| invalid_range())?;
        let text = state.text().map_err(|_| invalid_range())?;
        let count = text.line_count();
        let mut lines = Vec::with_capacity(count);
        for lnum in 1..=count {
            lines.push(text.line(lnum).map_err(|_| invalid_range())?);
        }
        let cursor = editor
            .current_window()
            .and_then(|window| editor.window(window).ok())
            .map_or(
                Position {
                    lnum: current,
                    col: 0,
                },
                |window| window.cursor,
            );
        let wrapscan = matches!(
            editor.options().get_global("wrapscan"),
            Ok(OptionValue::Boolean(true))
        );
        SearchState::default()
            .search(&lines, cursor, pattern, direction, 1, wrapscan)
            .map(|result| result.target.lnum)
            .map_err(|_| invalid_range())
    })
}

fn invalid_range() -> ApiError {
    ApiError::exception("Parsing command-line: E16: Invalid range")
}

fn cursor_line(session: &ApiSession) -> usize {
    session.with_editor(|editor| {
        editor
            .current_window()
            .and_then(|window| editor.window(window).ok())
            .map_or(1, |window| window.cursor.lnum)
    })
}

fn last_line(session: &ApiSession) -> usize {
    session.with_editor(|editor| {
        editor
            .current_buffer()
            .and_then(|buffer| editor.buffer(buffer).ok())
            .and_then(|state| state.text().ok())
            .map_or(1, ox_text::Buffer::line_count)
    })
}

fn has_key(opts: &Dict, name: &str) -> bool {
    opts.iter()
        .any(|(key, _)| key.as_bytes() == name.as_bytes())
}

fn text_of(value: &OxStr, what: &str) -> Result<String, ApiError> {
    std::str::from_utf8(value.as_bytes())
        .map(str::to_owned)
        .map_err(|_| ApiError::validation(format!("'{what}' must be valid UTF-8")))
}

fn opt_bool(opts: &Dict, name: &str) -> Result<bool, ApiError> {
    match opts.get(&OxStr::from(name)) {
        None => Ok(false),
        Some(Object::Boolean(value)) => Ok(*value),
        Some(value) => Err(ApiError::validation(format!(
            "Invalid '{name}': expected Boolean, got {}",
            type_name(value)
        ))),
    }
}

fn lua_u64(reference: i32, what: &str) -> Result<u64, ApiError> {
    u64::try_from(i64::from(reference)).map_err(|_| ApiError::validation(format!("invalid {what}")))
}

/// Upstream `api_typename` spellings for `got …` diagnostics.
fn type_name(value: &Object) -> &'static str {
    match value {
        Object::Nil => "nil",
        Object::Boolean(_) => "Boolean",
        Object::Integer(_) => "Integer",
        Object::Float(_) => "Float",
        Object::String(_) => "String",
        Object::Array(_) => "Array",
        Object::Dict(_) => "Dict",
        Object::LuaRef(_) => "Function",
        Object::Buffer(_) => "Buffer",
        Object::Window(_) => "Window",
        Object::Tabpage(_) => "Tabpage",
    }
}

fn lua_ref(reference: u64) -> i32 {
    i32::try_from(reference).unwrap_or(i32::MAX)
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_parse_cmd__API_META(), nvim_parse_cmd__API_DISPATCH)?;
    registry.register(
        nvim_create_user_command__API_META(),
        nvim_create_user_command__API_DISPATCH,
    )?;
    registry.register(
        nvim_del_user_command__API_META(),
        nvim_del_user_command__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_commands__API_META(),
        nvim_get_commands__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_create_user_command__API_META(),
        nvim_buf_create_user_command__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_del_user_command__API_META(),
        nvim_buf_del_user_command__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_get_commands__API_META(),
        nvim_buf_get_commands__API_DISPATCH,
    )?;
    Ok(())
}
