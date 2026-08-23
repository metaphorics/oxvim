//! Pure path and filesystem builtins.
//!
//! Semantics follow `src/nvim/eval/funcs.c` and `runtime/doc/vimfn.txt`, with
//! modifier ordering from `runtime/doc/cmdline.txt` `filename-modifiers`.

use std::fs;
use std::path::{Path, PathBuf};

use ox_types::{OxStr, Typval};

use crate::error::{EvalError, Result};
use crate::eval::RegexEngine;

pub(crate) fn getcwd(args: &[Typval]) -> Result<Typval> {
    for argument in args {
        number_arg(argument)?;
    }
    let directory = std::env::current_dir()
        .map_err(|error| EvalError::new("E472", 0, error.to_string()))?;
    Ok(text(directory.to_string_lossy()))
}

pub(crate) fn executable(value: &Typval) -> Result<Typval> {
    let program = string_arg(value)?.to_string_lossy().into_owned();
    if program.is_empty() {
        return Ok(boolean(false));
    }
    let path = Path::new(&program);
    if path.components().count() > 1 {
        return Ok(boolean(is_executable(path)));
    }
    let found = std::env::var_os("PATH").is_some_and(|search| {
        std::env::split_paths(&search).any(|directory| is_executable(&directory.join(&program)))
    });
    Ok(boolean(found))
}

pub(crate) fn exepath(value: &Typval) -> Result<Typval> {
    let program = string_arg(value)?.to_string_lossy().into_owned();
    if program.is_empty() { return Ok(text("")); }
    let path = Path::new(&program);
    if path.components().count() > 1 {
        return Ok(if is_executable(path) { text(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy()) } else { text("") });
    }
    let found = std::env::var_os("PATH").and_then(|search| std::env::split_paths(&search).map(|directory| directory.join(&program)).find(|candidate| is_executable(candidate)));
    Ok(found.map_or_else(|| text(""), |candidate| text(fs::canonicalize(&candidate).unwrap_or(candidate).to_string_lossy())))
}

pub(crate) fn simplify(value: &Typval) -> Result<Typval> {
    let input = string_arg(value)?;
    Ok(text(simplify_name(&input.to_string_lossy())))
}

pub(crate) fn resolve(value: &Typval) -> Result<Typval> {
    let input = string_arg(value)?.to_string_lossy().into_owned();
    let trailing_separator = input.ends_with('/');
    let leading_dot = input.starts_with("./");
    let path = Path::new(&input);
    let mut output = if let Ok(canonical) = fs::canonicalize(path) {
        if path.is_absolute() {
            canonical.to_string_lossy().into_owned()
        } else if let Ok(current) = std::env::current_dir() {
            canonical
                .strip_prefix(current)
                .unwrap_or(&canonical)
                .to_string_lossy()
                .into_owned()
        } else {
            canonical.to_string_lossy().into_owned()
        }
    } else {
        simplify_name(&input)
    };
    if leading_dot && !path.is_absolute() && !output.starts_with("./") {
        output.insert_str(0, "./");
    }
    if trailing_separator && !output.ends_with('/') {
        output.push('/');
    }
    Ok(text(output))
}

pub(crate) fn fnamemodify(
    regex: Option<&dyn RegexEngine>,
    filename: &Typval,
    modifiers: &Typval,
) -> Result<Typval> {
    let mut name = string_arg(filename)?.to_string_lossy().into_owned();
    let modifiers = string_arg(modifiers)?;
    let bytes = modifiers.as_bytes();
    let mut cursor = 0;

    while bytes.get(cursor) == Some(&b':') {
        cursor += 1;
        let Some(modifier) = bytes.get(cursor).copied() else {
            break;
        };
        cursor += 1;
        match modifier {
            b'8' => {}
            b'p' => name = absolute_name(&name),
            b'~' => name = relative_to_home(&name),
            b'.' => name = relative_to_current(&name),
            b'h' => name = path_head(&name),
            b't' => name = path_tail(&name),
            b'r' => name = path_root(&name),
            b'e' => {
                let mut count = 1;
                while bytes.get(cursor..cursor + 2) == Some(b":e") {
                    count += 1;
                    cursor += 2;
                }
                name = path_extension(&name, count);
            }
            b's' | b'g' => {
                let global = modifier == b'g';
                if global {
                    if bytes.get(cursor) != Some(&b's') {
                        break;
                    }
                    cursor += 1;
                }
                let Some(delimiter) = bytes.get(cursor).copied() else {
                    break;
                };
                cursor += 1;
                let Some((pattern, next)) = modifier_part(bytes, cursor, delimiter) else {
                    break;
                };
                cursor = next;
                let Some((replacement, next)) = modifier_part(bytes, cursor, delimiter) else {
                    break;
                };
                cursor = next;
                let engine = regex.ok_or_else(|| {
                    EvalError::new("E54", 0, "regular-expression engine is not installed")
                })?;
                let flags = if global { OxStr::from("g") } else { OxStr::from("") };
                name = engine
                    .substitute(
                        &OxStr(name.into_bytes()),
                        &OxStr(pattern.into_bytes()),
                        &OxStr(replacement.into_bytes()),
                        &flags,
                    )?
                    .to_string_lossy()
                    .into_owned();
            }
            b'S' => name = shell_escape(&name),
            _ => break,
        }
    }

    Ok(text(name))
}

fn boolean(value: bool) -> Typval {
    Typval::Number(i64::from(value))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn text(value: impl AsRef<str>) -> Typval {
    Typval::String(OxStr::from(value.as_ref()))
}

fn string_arg(value: &Typval) -> Result<OxStr> {
    match value {
        Typval::String(value) => Ok(value.clone()),
        Typval::Number(value) => Ok(OxStr(value.to_string().into_bytes())),
        Typval::Bool(value) => Ok(OxStr::from(if *value { "v:true" } else { "v:false" })),
        Typval::Special(ox_types::Special::Null) => Ok(OxStr::from("")),
        Typval::Float(_) => Err(EvalError::new("E806", 0, "Using a Float as a String")),
        Typval::List(_) => Err(EvalError::new("E730", 0, "Using a List as a String")),
        Typval::Dict(_) => Err(EvalError::new("E731", 0, "Using a Dictionary as a String")),
        _ => Err(EvalError::new("E729", 0, "Using invalid value as a String")),
    }
}

fn number_arg(value: &Typval) -> Result<i64> {
    match value {
        Typval::Number(value) => Ok(*value),
        Typval::Bool(value) => Ok(i64::from(*value)),
        Typval::Special(ox_types::Special::Null) => Ok(0),
        Typval::String(value) => Ok(parse_integer_prefix(value.as_bytes()).unwrap_or(0)),
        Typval::Float(_) => Err(EvalError::new("E805", 0, "Using a Float as a Number")),
        Typval::List(_) => Err(EvalError::new("E745", 0, "Using a List as a Number")),
        Typval::Dict(_) => Err(EvalError::new("E728", 0, "Using a Dictionary as a Number")),
        _ => Err(EvalError::new("E745", 0, "Using invalid value as a Number")),
    }
}

fn parse_integer_prefix(bytes: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(bytes).ok()?.trim_start();
    let end = text
        .char_indices()
        .take_while(|(index, character)| character.is_ascii_digit() || (*index == 0 && matches!(character, '+' | '-')))
        .map(|(index, character)| index + character.len_utf8())
        .last()?;
    text[..end].parse().ok()
}

fn absolute_name(name: &str) -> String {
    let expanded = expand_home(name);
    let path = Path::new(&expanded);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut output = fs::canonicalize(&absolute)
        .unwrap_or_else(|_| PathBuf::from(simplify_name(&absolute.to_string_lossy())))
        .to_string_lossy()
        .into_owned();
    if absolute.is_dir() && !output.ends_with('/') {
        output.push('/');
    }
    output
}

fn expand_home(name: &str) -> String {
    if (name == "~" || name.starts_with("~/")) && std::env::var_os("HOME").is_some() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("checked above"));
        return format!("{}{}", home.to_string_lossy(), &name[1..]);
    }
    name.to_owned()
}

fn relative_to_home(name: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return name.to_owned();
    };
    relative_with_prefix(name, &PathBuf::from(home), "~")
}

fn relative_to_current(name: &str) -> String {
    let Ok(current) = std::env::current_dir() else {
        return name.to_owned();
    };
    relative_with_prefix(name, &current, "")
}

fn relative_with_prefix(name: &str, base: &Path, prefix: &str) -> String {
    match Path::new(name).strip_prefix(base) {
        Ok(relative) if relative.as_os_str().is_empty() => prefix.to_owned(),
        Ok(relative) if prefix.is_empty() => relative.to_string_lossy().into_owned(),
        Ok(relative) => format!("{prefix}/{}", relative.to_string_lossy()),
        Err(_) => name.to_owned(),
    }
}

fn path_head(name: &str) -> String {
    if name.is_empty() {
        return ".".to_owned();
    }
    if name.len() > 1 && name.ends_with('/') {
        return name.trim_end_matches('/').to_owned();
    }
    match name.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(index) => name[..index].to_owned(),
        None => String::new(),
    }
}

fn path_tail(name: &str) -> String {
    if name.ends_with('/') {
        String::new()
    } else {
        name.rsplit('/').next().unwrap_or(name).to_owned()
    }
}

fn tail_dot(name: &str) -> Option<usize> {
    let tail = name.rsplit('/').next().unwrap_or(name);
    let dot = tail.rfind('.')?;
    (dot > 0).then_some(name.len() - tail.len() + dot)
}

fn path_root(name: &str) -> String {
    tail_dot(name).map_or_else(|| name.to_owned(), |dot| name[..dot].to_owned())
}

fn path_extension(name: &str, count: usize) -> String {
    let tail = name.rsplit('/').next().unwrap_or(name);
    let dots: Vec<usize> = tail
        .match_indices('.')
        .map(|(index, _)| index)
        .filter(|index| *index > 0)
        .collect();
    if dots.is_empty() {
        return String::new();
    }
    tail[dots[dots.len().saturating_sub(count)] + 1..].to_owned()
}

fn modifier_part(bytes: &[u8], start: usize, delimiter: u8) -> Option<(String, usize)> {
    let mut output = Vec::new();
    let mut cursor = start;
    while cursor < bytes.len() {
        if bytes[cursor] == delimiter {
            return Some((String::from_utf8_lossy(&output).into_owned(), cursor + 1));
        }
        if bytes[cursor] == b'\\' && bytes.get(cursor + 1) == Some(&delimiter) {
            cursor += 1;
        }
        output.push(bytes[cursor]);
        cursor += 1;
    }
    None
}

fn shell_escape(name: &str) -> String {
    format!("'{}'", name.replace('\'', "'\\''"))
}

fn simplify_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let absolute = name.starts_with('/');
    let leading_dot = name.starts_with("./");
    let trailing_separator = name.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for component in name.split('/') {
        match component {
            "" | "." => {}
            ".." if parts.last().is_some_and(|part| *part != "..") => {
                parts.pop();
            }
            ".." if !absolute => parts.push(component),
            ".." => {}
            _ => parts.push(component),
        }
    }
    let mut output = parts.join("/");
    if absolute {
        output.insert(0, '/');
    } else if leading_dot && !output.starts_with("../") {
        output.insert_str(0, "./");
    }
    if output.is_empty() {
        output = if absolute {
            "/"
        } else if leading_dot {
            "./"
        } else {
            "."
        }
        .to_owned();
    }
    if trailing_separator && !output.ends_with('/') {
        output.push('/');
    }
    output
}
