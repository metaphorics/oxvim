//! Generates the builtin inventory from Neovim's declarative `eval.lua` table.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Clone)]
struct Entry {
    name: String,
    min_args: usize,
    max_args: Option<usize>,
    signature: String,
    method: bool,
}

fn main() {
    if let Err(error) = generate() {
        panic!("failed to generate Vim builtin inventory: {error}");
    }
}

fn generate() -> Result<(), String> {
    let source_path = env::var_os("OXVIM_REF_ROOT")
        .map(PathBuf::from)
        .map(|root| root.join("src/nvim/eval.lua"))
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"))
                .join("../../codegen/upstream/eval.lua")
        });
    println!("cargo:rerun-if-env-changed=OXVIM_REF_ROOT");
    println!("cargo:rerun-if-changed={}", source_path.display());
    let source = fs::read_to_string(&source_path)
        .map_err(|error| format!("cannot read {}: {error}", source_path.display()))?;
    let entries = parse_inventory(&source)?;
    if entries.len() < 400 {
        let names = entries.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>().join(", " );
        return Err(format!(
            "only {} unique builtins recovered from eval.lua: {names}",
            entries.len()
        ));
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or_else(|| "OUT_DIR is not set".to_owned())?);
    fs::write(out.join("builtins_gen.rs"), render(&entries))
        .map_err(|error| format!("cannot write generated inventory: {error}"))
}

fn parse_inventory(source: &str) -> Result<Vec<Entry>, String> {
    let marker = "M.funcs = {";
    let table_start = source.find(marker).ok_or_else(|| "M.funcs table not found".to_owned())? + marker.len() - 1;
    let table_end = matching_brace(source.as_bytes(), table_start)
        .ok_or_else(|| "unterminated M.funcs table".to_owned())?;
    let table = &source[table_start + 1..table_end];
    let mut by_name: BTreeMap<String, Entry> = BTreeMap::new();
    let mut cursor = 0;
    while let Some(open_rel) = find_entry_open(table, cursor) {
        let open = open_rel;
        let Some(close) = matching_brace(table.as_bytes(), open) else {
            return Err(format!("unterminated builtin entry near byte {open}"));
        };
        let block = &table[open + 1..close];
        if let Some(name) = string_field(block, "name") {
            let signature = string_field(block, "signature").unwrap_or_default();
            let (min_args, max_args) = args_field(block);
            let method = integer_field(block, "base").is_some_and(|base| base > 0);
            let candidate = Entry { name: name.clone(), min_args, max_args, signature, method };
            by_name
                .entry(name)
                .and_modify(|entry| {
                    entry.min_args = entry.min_args.min(candidate.min_args);
                    entry.max_args = match (entry.max_args, candidate.max_args) {
                        (Some(left), Some(right)) => Some(left.max(right)),
                        _ => None,
                    };
                    if entry.signature.is_empty() { entry.signature.clone_from(&candidate.signature); }
                    entry.method |= candidate.method;
                })
                .or_insert(candidate);
        }
        cursor = close + 1;
    }
    Ok(by_name.into_values().map(|mut entry| {
        if entry.signature.is_empty() { entry.signature = format!("{}(...)", entry.name); }
        entry
    }).collect())
}

fn find_entry_open(source: &str, from: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = from;
    while index < bytes.len() {
        index = skip_space_and_comments(bytes, index);
        if index >= bytes.len() {
            return None;
        }
        if bytes[index] == b'}' {
            return None;
        }
        if bytes[index] == b'[' {
            index += 1;
            index = match bytes.get(index) {
                Some(b'\'') | Some(b'"') => skip_quoted(bytes, index)? + 1,
                _ => return None,
            };
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            if bytes.get(index) != Some(&b']') {
                return None;
            }
            index += 1;
        } else {
            while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_') {
                index += 1;
            }
        }
        index = skip_space_and_comments(bytes, index);
        if bytes.get(index) != Some(&b'=') {
            index += 1;
            continue;
        }
        index = skip_space_and_comments(bytes, index + 1);
        if bytes.get(index) == Some(&b'{') {
            return Some(index);
        }
        index = skip_lua_value(bytes, index)?;
    }
    None
}

fn args_field(block: &str) -> (usize, Option<usize>) {
    let Some(value) = field_value(block, "args") else { return (0, Some(0)) };
    let value = value.trim();
    if value.starts_with('{') {
        let numbers: Vec<usize> = value[1..value.find('}').unwrap_or(value.len())]
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect();
        return match numbers.as_slice() {
            [only] => (*only, None),
            [min, max, ..] => (*min, Some(*max)),
            _ => (0, Some(0)),
        };
    }
    value.parse::<usize>().map_or((0, Some(0)), |count| (count, Some(count)))
}

fn integer_field(block: &str, field: &str) -> Option<usize> {
    field_value(block, field)?.trim().parse().ok()
}

fn string_field(block: &str, field: &str) -> Option<String> {
    let value = field_value(block, field)?.trim_start();
    let quote = *value.as_bytes().first()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut result = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            byte if byte == quote => return String::from_utf8(result).ok(),
            b'\\' if index + 1 < bytes.len() => {
                index += 1;
                result.push(bytes[index]);
            }
            byte => result.push(byte),
        }
        index += 1;
    }
    None
}

fn field_value<'a>(block: &'a str, field: &str) -> Option<&'a str> {
    for line in block.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix(field) else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(value) = rest.strip_prefix('=') {
            return Some(value.trim_start().trim_end_matches(','));
        }
    }
    None
}

fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' => index = skip_quoted(bytes, index)?,
            b'[' if long_bracket_level(bytes, index).is_some() => index = skip_long_bracket(bytes, index)?,
            b'-' if bytes.get(index + 1) == Some(&b'-') => index = skip_comment(bytes, index),
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn skip_space_and_comments(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) || bytes.get(index) == Some(&b',') {
            index += 1;
        }
        if bytes.get(index) == Some(&b'-') && bytes.get(index + 1) == Some(&b'-') {
            index = skip_comment(bytes, index);
        } else {
            return index;
        }
    }
}

fn skip_lua_value(bytes: &[u8], mut index: usize) -> Option<usize> {
    match *bytes.get(index)? {
        b'\'' | b'"' => Some(skip_quoted(bytes, index)? + 1),
        b'[' if long_bracket_level(bytes, index).is_some() => Some(skip_long_bracket(bytes, index)? + 1),
        b'{' => Some(matching_brace(bytes, index)? + 1),
        _ => {
            while index < bytes.len() && bytes[index] != b',' && bytes[index] != b'\n' {
                index += 1;
            }
            Some(index)
        }
    }
}

fn skip_quoted(bytes: &[u8], open: usize) -> Option<usize> {
    let quote = *bytes.get(open)?;
    let mut index = open + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == quote {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn long_bracket_level(bytes: &[u8], open: usize) -> Option<usize> {
    if bytes.get(open) != Some(&b'[') {
        return None;
    }
    let mut index = open + 1;
    while bytes.get(index) == Some(&b'=') {
        index += 1;
    }
    (bytes.get(index) == Some(&b'[')).then_some(index - open - 1)
}

fn skip_long_bracket(bytes: &[u8], open: usize) -> Option<usize> {
    let level = long_bracket_level(bytes, open)?;
    let mut index = open + level + 2;
    while index < bytes.len() {
        if bytes[index] == b']' {
            let mut cursor = index + 1;
            let mut equals = 0;
            while bytes.get(cursor) == Some(&b'=') {
                equals += 1;
                cursor += 1;
            }
            if equals == level && bytes.get(cursor) == Some(&b']') {
                return Some(cursor);
            }
        }
        index += 1;
    }
    None
}

fn skip_comment(bytes: &[u8], open: usize) -> usize {
    if bytes.get(open + 2) == Some(&b'[') && long_bracket_level(bytes, open + 2).is_some() {
        return skip_long_bracket(bytes, open + 2).unwrap_or(bytes.len());
    }
    let mut index = open + 2;
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

fn render(entries: &[Entry]) -> String {
    let mut output = String::from("// @generated by build.rs from src/nvim/eval.lua.\n");
    output.push_str("/// Complete, name-sorted builtin inventory generated from Neovim.\n");
    output.push_str("pub static BUILTINS: &[BuiltinSpec] = &[\n");
    for entry in entries {
        let max = entry.max_args.map_or_else(|| "None".to_owned(), |value| format!("Some({value})"));
        output.push_str(&format!(
            "    BuiltinSpec {{ name: {:?}, min_args: {}, max_args: {}, signature: {:?}, method: {} }},\n",
            entry.name, entry.min_args, max, entry.signature, entry.method
        ));
    }
    output.push_str("];\n");
    output
}
