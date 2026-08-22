//! Pure path and filesystem builtins.
//!
//! Semantics follow `src/nvim/eval/funcs.c` and `runtime/doc/vimfn.txt`, with
//! modifier ordering from `runtime/doc/cmdline.txt` `filename-modifiers`.

use std::fs;
use std::path::{Path, PathBuf};

use ox_types::{OxStr, Typval};

use crate::error::{EvalError, Result};
use crate::eval::RegexEngine;

pub(crate) fn filereadable(value: &Typval) -> Result<Typval> {
    Ok(boolean(path_arg(value)?.is_file()))
}

pub(crate) fn isdirectory(value: &Typval) -> Result<Typval> {
    Ok(boolean(path_arg(value)?.is_dir()))
}

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

pub(crate) fn glob(args: &[Typval]) -> Result<Typval> {
    let pattern = string_arg(&args[0])?.to_string_lossy().into_owned();
    glob_result(expand_glob(&pattern, truthy(args.get(3))?), truthy(args.get(2))?)
}

pub(crate) fn globpath(args: &[Typval]) -> Result<Typval> {
    let paths = string_arg(&args[0])?.to_string_lossy().into_owned();
    let pattern = string_arg(&args[1])?.to_string_lossy().into_owned();
    let all_links = truthy(args.get(4))?;
    let list = truthy(args.get(3))?;
    let mut matches = Vec::new();
    for directory in split_path_list(&paths) {
        matches.extend(expand_glob(&Path::new(&directory).join(&pattern).to_string_lossy(), all_links));
    }
    matches.sort();
    matches.dedup();
    glob_result(matches, list)
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

fn truthy(value: Option<&Typval>) -> Result<bool> {
    value.map(number_arg).transpose().map(|value| value.unwrap_or(0) != 0)
}

fn glob_result(matches: Vec<String>, list: bool) -> Result<Typval> {
    if list {
        Ok(Typval::list(matches.into_iter().map(|item| text(item)).collect()))
    } else {
        Ok(text(matches.join("\n")))
    }
}

fn expand_glob(pattern: &str, all_links: bool) -> Vec<String> {
    let expanded = expand_home(pattern);
    let path = Path::new(&expanded);
    let absolute = path.is_absolute();
    let components: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::RootDir => None,
            std::path::Component::CurDir => Some(".".to_owned()),
            std::path::Component::ParentDir => Some("..".to_owned()),
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            std::path::Component::Prefix(value) => Some(value.as_os_str().to_string_lossy().into_owned()),
        })
        .collect();
    let base = if absolute { PathBuf::from("/") } else { PathBuf::new() };
    let mut output = Vec::new();
    expand_components(&base, &components, 0, all_links, &mut output);
    output.sort();
    output.dedup();
    output
}

fn expand_components(
    base: &Path,
    components: &[String],
    index: usize,
    all_links: bool,
    output: &mut Vec<String>,
) {
    if index == components.len() {
        let exists = if all_links { fs::symlink_metadata(base).is_ok() } else { base.exists() };
        if exists {
            let rendered = if base.as_os_str().is_empty() { ".".to_owned() } else { base.to_string_lossy().into_owned() };
            output.push(rendered);
        }
        return;
    }
    let component = &components[index];
    if component == "**" {
        expand_components(base, components, index + 1, all_links, output);
        if let Ok(entries) = fs::read_dir(if base.as_os_str().is_empty() { Path::new(".") } else { base }) {
            let mut directories: Vec<PathBuf> = entries
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir() && !path.is_symlink())
                .collect();
            directories.sort();
            for directory in directories {
                expand_components(&directory, components, index, all_links, output);
            }
        }
        return;
    }
    if !has_wildcard(component) {
        expand_components(&base.join(component), components, index + 1, all_links, output);
        return;
    }
    let directory = if base.as_os_str().is_empty() { Path::new(".") } else { base };
    let Ok(entries) = fs::read_dir(directory) else { return };
    let mut entries: Vec<_> = entries.filter_map(std::result::Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let filename = entry.file_name().to_string_lossy().into_owned();
        if wildcard_match(component.as_bytes(), filename.as_bytes()) {
            expand_components(&entry.path(), components, index + 1, all_links, output);
        }
    }
}

fn has_wildcard(component: &str) -> bool {
    component.bytes().any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

fn wildcard_match(pattern: &[u8], value: &[u8]) -> bool {
    fn matches(pattern: &[u8], value: &[u8], pattern_index: usize, value_index: usize) -> bool {
        if pattern_index == pattern.len() { return value_index == value.len(); }
        match pattern[pattern_index] {
            b'*' => {
                (value_index..=value.len()).any(|next| matches(pattern, value, pattern_index + 1, next))
            }
            b'?' => value_index < value.len() && matches(pattern, value, pattern_index + 1, value_index + 1),
            b'[' => {
                let Some(close_relative) = pattern.get(pattern_index + 1..).and_then(|tail| tail.iter().position(|byte| *byte == b']')) else {
                    return value_index < value.len() && value[value_index] == b'[' && matches(pattern, value, pattern_index + 1, value_index + 1);
                };
                let close = pattern_index + 1 + close_relative;
                let class = &pattern[pattern_index + 1..close];
                let negated = class.first().is_some_and(|byte| matches!(byte, b'!' | b'^'));
                let class = if negated { &class[1..] } else { class };
                let mut accepted = false;
                let mut cursor = 0;
                while cursor < class.len() {
                    if cursor + 2 < class.len() && class[cursor + 1] == b'-' {
                        accepted |= value_index < value.len() && (class[cursor]..=class[cursor + 2]).contains(&value[value_index]);
                        cursor += 3;
                    } else {
                        accepted |= value_index < value.len() && class[cursor] == value[value_index];
                        cursor += 1;
                    }
                }
                value_index < value.len() && (accepted != negated) && matches(pattern, value, close + 1, value_index + 1)
            }
            literal => value_index < value.len() && literal == value[value_index] && matches(pattern, value, pattern_index + 1, value_index + 1),
        }
    }
    matches(pattern, value, 0, 0)
}

fn split_path_list(paths: &str) -> Vec<String> {
    let mut output = vec![String::new()];
    let mut escaped = false;
    for character in paths.chars() {
        if escaped {
            output.last_mut().expect("one path").push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == ',' {
            output.push(String::new());
        } else {
            output.last_mut().expect("one path").push(character);
        }
    }
    if escaped { output.last_mut().expect("one path").push('\\'); }
    output
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

fn path_arg(value: &Typval) -> Result<PathBuf> {
    Ok(PathBuf::from(string_arg(value)?.to_string_lossy().into_owned()))
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
    let trimmed = if name.len() > 1 {
        name.trim_end_matches('/')
    } else {
        name
    };
    match trimmed.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(index) => trimmed[..index].to_owned(),
        None => String::new(),
    }
}

fn path_tail(name: &str) -> String {
    let trimmed = name.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_owned()
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
