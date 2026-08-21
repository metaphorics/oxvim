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
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let reference_root = env::var_os("OXVIM_REF_ROOT").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "OXVIM_REF_ROOT must point to the Neovim reference checkout",
        )
    })?;
    let source = PathBuf::from(reference_root).join("src/nvim/ex_cmds.lua");
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
            "    CommandSpec {{ name: {:?}, abbr: {:?}, min_prefix_len: {}, flags: CommandFlags(0x{:x}) }},",
            command.name, abbr, min_prefix_len, command.flags
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

fn parse_commands(lua: &str) -> Result<Vec<LuaCommand>, io::Error> {
    let mut commands = Vec::new();
    let mut pending_name: Option<String> = None;
    let mut pending_flags: Option<String> = None;

    for raw_line in lua.lines() {
        let line = raw_line.split("--").next().unwrap_or("").trim();
        if let Some(expression) = pending_flags.as_mut() {
            expression.push_str(line);
            if line.ends_with("),") || line == ")" {
                let name = pending_name
                    .take()
                    .ok_or_else(|| invalid("flags entry is missing command"))?;
                let complete = pending_flags
                    .take()
                    .ok_or_else(|| invalid("multiline flags disappeared"))?;
                commands.push(LuaCommand {
                    name,
                    flags: parse_flags(complete.trim_end_matches(',').trim_end())?,
                });
            }
            continue;
        }
        if let Some(value) = field_value(line, "command") {
            if pending_name.is_some() {
                return Err(invalid("command entry is missing flags"));
            }
            pending_name = Some(parse_quoted(value, "command")?.to_owned());
            continue;
        }
        if let Some(value) = field_value(line, "flags") {
            if value.starts_with("bit.bor(") && !value.ends_with(')') {
                pending_flags = Some(value.to_owned());
                continue;
            }
            let Some(name) = pending_name.take() else {
                continue;
            };
            commands.push(LuaCommand {
                name,
                flags: parse_flags(value)?,
            });
        }
    }

    if pending_name.is_some() || pending_flags.is_some() {
        return Err(invalid("last command entry is missing flags"));
    }
    Ok(commands)
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
