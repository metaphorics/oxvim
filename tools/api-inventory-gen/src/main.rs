//! Deterministic generator for `crates/ox-api/src/api_function_names.rs`.
//!
//! Decodes the canonical msgpack blob via `ox_rpc::canonical_metadata()`,
//! overlays the sparse `fast` and `textlock` flag tables, and emits one
//! `FunctionMetadata` block per function in blob order. The output is
//! byte-stable: the same blob plus same overlay always produces identical
//! bytes.
//!
//! Run explicitly (never via build.rs):
//!
//! ```text
//! cargo run -p ox-api-inventory-gen > crates/ox-api/src/api_function_names.rs
//! ```
//!
//! Use `--check` to compare the generated output against the checked-in file
//! and exit non-zero on drift.

use std::fmt;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::process::ExitCode;

use ox_types::{Dict, Object, OxStr};

// ---------------------------------------------------------------------------
// Overlay tables: the only fields absent from the blob.
// ---------------------------------------------------------------------------

/// Functions permitted during a fast callback. Verified count: 14.
const FAST: &[&str] = &[
    "nvim_create_autocmd",
    "nvim_parse_cmd",
    "nvim_set_hl_ns_fast",
    "nvim_input",
    "nvim_input_mouse",
    "nvim_replace_termcodes",
    "nvim_get_runtime_file",
    "nvim_get_mode",
    "nvim_get_api_info",
    "nvim_eval_statusline",
    "nvim_parse_expression",
    "vim_input",
    "vim_replace_termcodes",
    "vim_get_api_info",
];

/// Functions forbidden while text is locked. Verified count: 16.
const TEXTLOCK: &[&str] = &[
    "nvim_buf_set_lines",
    "nvim_buf_set_text",
    "nvim_buf_delete",
    "nvim_open_tabpage",
    "nvim_set_current_line",
    "nvim_del_current_line",
    "nvim_set_current_buf",
    "nvim_set_current_win",
    "nvim_open_term",
    "nvim_set_current_tabpage",
    "nvim_paste",
    "nvim_put",
    "nvim_open_win",
    "nvim_win_set_buf",
    "nvim_win_hide",
    "nvim_win_close",
];

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum GenError {
    Decode(String),
    BadShape(&'static str),
    UnmappedType(String),
    OverlayMissing(String),
    Fmt(fmt::Error),
    Io(io::Error),
}

impl fmt::Display for GenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(msg) => write!(f, "failed to decode canonical metadata: {msg}"),
            Self::BadShape(msg) => write!(f, "malformed canonical metadata: {msg}"),
            Self::UnmappedType(ty) => write!(f, "unmapped type string: {ty}"),
            Self::OverlayMissing(name) => {
                write!(f, "overlay name absent from blob: {name}")
            }
            Self::Fmt(err) => write!(f, "format error: {err}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for GenError {}

impl From<io::Error> for GenError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

// ---------------------------------------------------------------------------
// Object field extraction helpers
// ---------------------------------------------------------------------------

fn dict_get<'a>(dict: &'a Dict, key: &str) -> Result<&'a Object, GenError> {
    dict.get(&OxStr::from(key))
        .ok_or(GenError::BadShape("missing field"))
}

fn object_str(obj: &Object) -> Result<&str, GenError> {
    let Object::String(s) = obj else {
        return Err(GenError::BadShape("expected string field"));
    };
    std::str::from_utf8(s.as_bytes()).map_err(|_| GenError::BadShape("non-utf8 string field"))
}

fn object_int(obj: &Object) -> Result<i64, GenError> {
    let Object::Integer(n) = obj else {
        return Err(GenError::BadShape("expected integer field"));
    };
    Ok(*n)
}

fn object_bool(obj: &Object) -> Result<bool, GenError> {
    let Object::Boolean(b) = obj else {
        return Err(GenError::BadShape("expected boolean field"));
    };
    Ok(*b)
}

// ---------------------------------------------------------------------------
// Type-string → TypeRef source mapper
// ---------------------------------------------------------------------------

/// Whether `s` contains a comma at paren-depth zero.
fn has_top_level_comma(s: &str) -> bool {
    let mut depth = 0i32;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

/// Map a blob type string to the Rust source for the matching `TypeRef`.
///
/// Fails on any string not present in the canonical inventory.
fn map_type(s: &str) -> Result<String, GenError> {
    if let Some(inner) = s.strip_prefix("ArrayOf(").and_then(|r| r.strip_suffix(')')) {
        // A top-level comma signals a fixed-size array, e.g.
        // "ArrayOf(Integer, 2)". These have no typed TypeRef variant and are
        // preserved verbatim via TypeRef::Named.
        if has_top_level_comma(inner) {
            return Ok(format!("TypeRef::Named(\"{s}\")"));
        }
        let element = map_type(inner)?;
        return Ok(format!("TypeRef::ArrayOf(&{element})"));
    }
    match s {
        "void" => Ok("TypeRef::Void".into()),
        "Object" => Ok("TypeRef::Object".into()),
        "Integer" => Ok("TypeRef::Integer".into()),
        "Boolean" => Ok("TypeRef::Boolean".into()),
        "String" => Ok("TypeRef::String".into()),
        "Dict" => Ok("TypeRef::Dict".into()),
        "Array" => Ok("TypeRef::Array".into()),
        "Float" => Ok("TypeRef::Float".into()),
        "Nil" => Ok("TypeRef::Nil".into()),
        "Window" => Ok("TypeRef::Window".into()),
        "Tabpage" => Ok("TypeRef::Tabpage".into()),
        "Buffer" => Ok("TypeRef::Buffer".into()),
        "LuaRef" => Ok("TypeRef::LuaRef".into()),
        _ => Err(GenError::UnmappedType(s.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

struct Function {
    name: String,
    since: u16,
    deprecated_since: Option<u16>,
    method: bool,
    fast: bool,
    textlock: bool,
    returns: String,
    params: Vec<(String, String, bool)>,
}

fn extract_functions(metadata: &Object) -> Result<Vec<Function>, GenError> {
    let Object::Dict(root) = metadata else {
        return Err(GenError::BadShape("metadata root must be a dict"));
    };
    let Object::Array(functions) = dict_get(root, "functions")? else {
        return Err(GenError::BadShape("functions must be an array"));
    };

    let mut fast_set: std::collections::HashSet<&str> = FAST.iter().copied().collect();
    let mut textlock_set: std::collections::HashSet<&str> = TEXTLOCK.iter().copied().collect();

    let mut result = Vec::with_capacity(functions.len());
    for entry in functions {
        let Object::Dict(fields) = entry else {
            return Err(GenError::BadShape("function entry must be a dict"));
        };

        let name = object_str(dict_get(fields, "name")?)?.to_string();
        let since = u16::try_from(object_int(dict_get(fields, "since")?)?)
            .map_err(|_| GenError::BadShape("since out of u16 range"))?;
        let method = object_bool(dict_get(fields, "method")?)?;
        let return_type_str = object_str(dict_get(fields, "return_type")?)?;
        let returns = map_type(return_type_str)?;

        let deprecated_since = match dict_get(fields, "deprecated_since") {
            Ok(obj) => Some(
                u16::try_from(object_int(obj)?)
                    .map_err(|_| GenError::BadShape("deprecated_since out of u16 range"))?,
            ),
            Err(_) => None,
        };

        let Object::Array(param_list) = dict_get(fields, "parameters")? else {
            return Err(GenError::BadShape("parameters must be an array"));
        };
        let mut params = Vec::with_capacity(param_list.len());
        for param in param_list {
            let Object::Array(triple) = param else {
                return Err(GenError::BadShape("parameter must be a 3-array"));
            };
            if triple.len() != 3 {
                return Err(GenError::BadShape("parameter must have exactly 3 elements"));
            }
            let type_str = object_str(&triple[0])?;
            let param_name = object_str(&triple[1])?;
            let optional = object_bool(&triple[2])?;
            params.push((param_name.to_string(), map_type(type_str)?, optional));
        }

        let fast = fast_set.remove(name.as_str());
        let textlock = textlock_set.remove(name.as_str());

        result.push(Function {
            name,
            since,
            deprecated_since,
            method,
            fast,
            textlock,
            returns,
            params,
        });
    }

    if !fast_set.is_empty() {
        return Err(GenError::OverlayMissing(
            fast_set.into_iter().next().unwrap_or_default().to_string(),
        ));
    }
    if !textlock_set.is_empty() {
        return Err(GenError::OverlayMissing(
            textlock_set
                .into_iter()
                .next()
                .unwrap_or_default()
                .to_string(),
        ));
    }

    Ok(result)
}

fn format_params(params: &[(String, String, bool)]) -> Result<String, GenError> {
    if params.is_empty() {
        return Ok("&[],".to_string());
    }
    if params.len() == 1 {
        let (name, ty, optional) = &params[0];
        return Ok(format!("&[(\"{name}\", {ty}, {optional})],"));
    }
    let mut out = String::from("&[\n");
    for (name, ty, optional) in params {
        writeln!(out, "            (\"{name}\", {ty}, {optional}),").map_err(GenError::Fmt)?;
    }
    out.push_str("        ],");
    Ok(out)
}

fn generate(functions: &[Function]) -> Result<String, GenError> {
    let mut out = String::new();
    out.push_str(
        "// Generated by `cargo run -p ox-api-inventory-gen` from the canonical msgpack\n\
         // blob at `crates/ox-rpc/src/api_metadata.msgpack` (Neovim 0.13, API level 15).\n\
         // Do not edit by hand; regenerate and review the diff.\n",
    );
    out.push_str("use crate::{FunctionMetadata, TypeRef};\n\n");
    out.push_str("pub(crate) const API_FUNCTIONS: &[FunctionMetadata] = &[\n");

    for f in functions {
        let deprecated = match f.deprecated_since {
            Some(n) => format!("Some({n})"),
            None => "None".to_string(),
        };
        let params = format_params(&f.params)?;
        write!(
            out,
            "    FunctionMetadata {{\n\
             \x20       name: \"{name}\",\n\
             \x20       since: {since},\n\
             \x20       deprecated_since: {deprecated},\n\
             \x20       method: {method},\n\
             \x20       fast: {fast},\n\
             \x20       textlock: {textlock},\n\
             \x20       returns: {returns},\n\
             \x20       params: {params}\n\
             \x20   }},\n",
            name = f.name,
            since = f.since,
            deprecated = deprecated,
            method = f.method,
            fast = f.fast,
            textlock = f.textlock,
            returns = f.returns,
            params = params,
        )
        .map_err(GenError::Fmt)?;
    }

    out.push_str("];\n");
    Ok(out)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

const TARGET: &str = "../../crates/ox-api/src/api_function_names.rs";

fn run(check: bool) -> Result<(), GenError> {
    let metadata = ox_rpc::canonical_metadata().map_err(|e| GenError::Decode(e.to_string()))?;
    let functions = extract_functions(&metadata)?;
    let output = generate(&functions)?;

    if check {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest_dir).join(TARGET);
        let existing = std::fs::read_to_string(&path)?;
        if existing != output {
            eprintln!(
                "ox-api-inventory-gen: checked-in file does not match generated output.\n\
                 Run `cargo run -p ox-api-inventory-gen > crates/ox-api/src/api_function_names.rs`."
            );
            return Err(GenError::BadShape("drift detected"));
        }
        eprintln!(
            "ox-api-inventory-gen: checked-in file is up to date ({} functions).",
            functions.len()
        );
        return Ok(());
    }

    let stdout = io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(output.as_bytes())?;
    Ok(())
}

fn main() -> ExitCode {
    let check = std::env::args().any(|a| a == "--check");
    match run(check) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ox-api-inventory-gen: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run;

    /// Drift gate: the checked-in inventory must equal what the canonical
    /// blob regenerates. Canonical metadata change without regeneration
    /// fails here, and so does a hand edit of the generated file.
    #[test]
    fn checked_in_inventory_matches_canonical_regeneration() {
        let result = run(true);
        assert!(
            result.is_ok(),
            "checked-in api_function_names.rs is stale; regenerate with \
             cargo run -p ox-api-inventory-gen: {result:?}"
        );
    }
}
