#![allow(missing_docs)]

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Debug)]
struct LuaCommand {
    name: String,
    flags: u32,
    addr_type: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = env::var_os("OXVIM_REF_ROOT")
        .map(PathBuf::from)
        .map(|root| root.join("src/nvim/ex_cmds.lua"))
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"))
                .join("../../codegen/upstream/ex_cmds.lua")
        });
    println!("cargo:rerun-if-env-changed=OXVIM_REF_ROOT");
    println!("cargo:rerun-if-changed={}", source.display());

    let lua = fs::read_to_string(&source)?;
    let commands = parse_commands(&lua)?;
    if commands.len() < 400 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parsed only {} Ex commands from {}", commands.len(), source.display()),
        )
        .into());
    }

    let mut generated = String::from(
        "// @generated from src/nvim/ex_cmds.lua; do not edit.\n\
         /// Every built-in Ex command in upstream table order.\n\
         pub static COMMANDS: &[CommandSpec] = &[\n",
    );
    for (index, command) in commands.iter().enumerate() {
        let min_prefix_len = minimum_prefix_len(&commands, index);
        let abbr = &command.name[..min_prefix_len];
        writeln!(
            generated,
            "    CommandSpec {{ name: {:?}, abbr: {:?}, min_prefix_len: {}, flags: CommandFlags(0x{:x}), addr_type: AddrType::{} }},",
            command.name,
            abbr,
            min_prefix_len,
            command.flags,
            addr_type_variant(&command.addr_type)?
        )?;
    }
    generated.push_str("];\n");

    let output = PathBuf::from(env::var_os("OUT_DIR").ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "Cargo did not provide OUT_DIR")
    })?)
    .join("command_specs.rs");
    fs::write(output, generated)?;
    Ok(())
}

/// Collects one `LuaCommand` per `ex_cmds.lua` table entry.
///
/// Fields are accumulated until the entry's closing `},` because `addr_type`
/// follows `flags` in every upstream entry, so an entry cannot be emitted at
/// the moment its flags complete.
fn parse_commands(lua: &str) -> Result<Vec<LuaCommand>, io::Error> {
    let mut commands = Vec::new();
    let mut pending_name: Option<String> = None;
    let mut pending_flags: Option<u32> = None;
    let mut pending_addr: Option<String> = None;
    let mut continued_flags: Option<String> = None;

    for raw_line in lua.lines() {
        let line = raw_line.split("--").next().unwrap_or("").trim();
        if let Some(expression) = continued_flags.as_mut() {
            expression.push_str(line);
            if line.ends_with("),") || line == ")" {
                let complete = continued_flags
                    .take()
                    .ok_or_else(|| invalid("multiline flags disappeared"))?;
                pending_flags = Some(parse_flags(complete.trim_end_matches(',').trim_end())?);
            }
            continue;
        }
        if line == "}," && pending_name.is_some() {
            let name = pending_name.take().unwrap_or_default();
            let flags = pending_flags
                .take()
                .ok_or_else(|| invalid(&format!("command {name} is missing flags")))?;
            let addr_type = pending_addr
                .take()
                .ok_or_else(|| invalid(&format!("command {name} is missing addr_type")))?;
            commands.push(LuaCommand { name, flags, addr_type });
            continue;
        }
        if let Some(value) = field_value(line, "command") {
            if pending_name.is_some() {
                return Err(invalid("command entry is missing its closing brace"));
            }
            pending_name = Some(parse_quoted(value, "command")?.to_owned());
            continue;
        }
        if pending_name.is_none() {
            continue;
        }
        if let Some(value) = field_value(line, "flags") {
            if value.starts_with("bit.bor(") && !value.ends_with(')') {
                continued_flags = Some(value.to_owned());
            } else {
                pending_flags = Some(parse_flags(value)?);
            }
            continue;
        }
        if let Some(value) = field_value(line, "addr_type") {
            pending_addr = Some(parse_quoted(value, "addr_type")?.to_owned());
        }
    }

    if pending_name.is_some() || continued_flags.is_some() {
        return Err(invalid("last command entry is incomplete"));
    }
    Ok(commands)
}

/// Maps an upstream `ADDR_*` name onto the `AddrType` variant it generates.
fn addr_type_variant(name: &str) -> Result<&'static str, io::Error> {
    let variant = match name {
        "ADDR_LINES" => "Lines",
        "ADDR_WINDOWS" => "Windows",
        "ADDR_ARGUMENTS" => "Arguments",
        "ADDR_BUFFERS" => "Buffers",
        "ADDR_LOADED_BUFFERS" => "LoadedBuffers",
        "ADDR_TABS" => "Tabs",
        "ADDR_TABS_RELATIVE" => "TabsRelative",
        "ADDR_QUICKFIX" => "QuickFix",
        "ADDR_QUICKFIX_VALID" => "QuickFixValid",
        "ADDR_UNSIGNED" => "Unsigned",
        "ADDR_OTHER" => "Other",
        "ADDR_NONE" => "None",
        unknown => return Err(invalid(&format!("unknown Ex command addr_type {unknown}"))),
    };
    Ok(variant)
}

fn field_value<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(field)?.trim_start();
    rest.strip_prefix('=')
        .map(str::trim_start)
        .map(|value| value.trim_end_matches(',').trim_end())
}

fn parse_quoted<'a>(value: &'a str, field: &str) -> Result<&'a str, io::Error> {
    let quote = value.as_bytes().first().copied();
    if !matches!(quote, Some(b'\'') | Some(b'"')) {
        return Err(invalid(&format!("{field} is not a quoted string: {value}")));
    }
    let delimiter = char::from(quote.unwrap_or(b'\''));
    let tail = &value[1..];
    let end = tail
        .find(delimiter)
        .ok_or_else(|| invalid(&format!("unterminated {field}: {value}")))?;
    Ok(&tail[..end])
}

fn parse_flags(value: &str) -> Result<u32, io::Error> {
    let expression = value
        .strip_prefix("bit.bor(")
        .and_then(|body| body.strip_suffix(')'))
        .unwrap_or(value);
    expression
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .try_fold(0_u32, |bits, token| flag_value(token).map(|flag| bits | flag))
}

fn flag_value(name: &str) -> Result<u32, io::Error> {
    let value = match name {
        "RANGE" => 0x001,
        "BANG" => 0x002,
        "EXTRA" => 0x004,
        "XFILE" => 0x008,
        "NOSPC" => 0x010,
        "DFLALL" => 0x020,
        "WHOLEFOLD" => 0x040,
        "NEEDARG" => 0x080,
        "TRLBAR" => 0x100,
        "REGSTR" => 0x200,
        "COUNT" => 0x400,
        "NOTRLCOM" => 0x800,
        "ZEROR" => 0x1000,
        "CTRLV" => 0x2000,
        "CMDARG" => 0x4000,
        "BUFNAME" => 0x8000,
        "BUFUNL" => 0x1_0000,
        "ARGOPT" => 0x2_0000,
        "SBOXOK" => 0x4_0000,
        "BUFLOCK_OK" => 0x8_0000,
        "MODIFY" => 0x10_0000,
        "FLAGS" => 0x20_0000,
        "LOCK_OK" => 0x100_0000,
        "PREVIEW" => 0x800_0000,
        "FILES" => 0x00c,
        "WORD1" => 0x014,
        "FILE1" => 0x01c,
        unknown => return Err(invalid(&format!("unknown Ex command flag {unknown}"))),
    };
    Ok(value)
}

fn minimum_prefix_len(commands: &[LuaCommand], wanted: usize) -> usize {
    let name = &commands[wanted].name;
    if name == "substitute" || name == "k" {
        return 1;
    }
    for length in 1..=name.len() {
        let prefix = &name[..length];
        if commands
            .iter()
            .position(|candidate| candidate.name.starts_with(prefix))
            == Some(wanted)
        {
            return length;
        }
    }
    name.len()
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
