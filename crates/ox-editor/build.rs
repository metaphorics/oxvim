//! Generates option metadata from the authoritative Neovim options table.
// Build script: panicking fails the build with the generation error, which
// is the correct outcome; there is no caller to recover on cargo's behalf.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=OXVIM_REF_ROOT");

    if let Err(error) = generate() {
        eprintln!("ox-editor option metadata generation failed: {error}");
        std::process::exit(1);
    }
}

fn generate() -> Result<(), String> {
    let manifest_dir = required_path("CARGO_MANIFEST_DIR")?;
    let source_path = env::var_os("OXVIM_REF_ROOT")
        .map(PathBuf::from)
        .map_or_else(
            || manifest_dir.join("../../codegen/upstream/options.lua"),
            |root| root.join("src/nvim/options.lua"),
        );
    println!("cargo:rerun-if-changed={}", source_path.display());

    if !source_path.is_file() {
        return Err(format!(
            "required Neovim option source is missing: {} (set OXVIM_REF_ROOT to the reference tree root)",
            source_path.display()
        ));
    }

    let source = fs::read_to_string(&source_path)
        .map_err(|error| format!("cannot read {}: {error}", source_path.display()))?;
    let options = parse_options(&source)?;
    if options.is_empty() {
        return Err(format!(
            "no top-level option records found in {}",
            source_path.display()
        ));
    }

    let out_dir = required_path("OUT_DIR")?;
    let output = render(&options)?;
    fs::write(out_dir.join("options_metadata.rs"), output)
        .map_err(|error| format!("cannot write generated option metadata: {error}"))
}

fn required_path(name: &str) -> Result<PathBuf, String> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("Cargo did not provide {name}"))
}

#[derive(Debug)]
struct ParsedOption {
    name: String,
    short_name: Option<String>,
    aliases: Vec<String>,
    scopes: Vec<String>,
    value_type: String,
    default: ParsedDefault,
    list: Option<String>,
    deny_duplicates: bool,
    /// `expand` key present (`true` or `'nodefault'`): `:set` expands
    /// environment variables and `~` in the value (option.c `kOptFlagExpand`).
    expand: bool,
}

#[derive(Debug)]
struct ParsedDefault {
    value: Option<Literal>,
    raw: String,
    conditional: bool,
    present: bool,
}

#[derive(Debug)]
enum Literal {
    Boolean(bool),
    Number(i64),
    String(String),
}

fn parse_options(source: &str) -> Result<Vec<ParsedOption>, String> {
    let marker = "\n  options = {";
    let marker_start = source
        .find(marker)
        .ok_or_else(|| "cannot locate exact-indentation `options = {` table".to_owned())?;
    let table_open = marker_start + marker.len() - 1;
    let table_close = matching_delimiter(source, table_open, b'{', b'}')?;
    let bytes = source.as_bytes();
    let mut cursor = table_open + 1;
    let mut options = Vec::new();

    while cursor < table_close {
        cursor = skip_trivia(source, cursor, table_close)?;
        if cursor >= table_close {
            break;
        }

        if bytes[cursor] == b',' {
            cursor += 1;
            continue;
        }

        if bytes[cursor] == b'{' && has_exact_indent(source, cursor, 4) {
            let close = matching_delimiter(source, cursor, b'{', b'}')?;
            let fields = parse_fields(source, cursor + 1, close)?;
            options.push(parse_option(&fields, options.len() + 1)?);
            cursor = close + 1;
        } else {
            cursor = skip_value(source, cursor, table_close)?;
        }
    }

    Ok(options)
}

fn parse_option(fields: &BTreeMap<String, String>, ordinal: usize) -> Result<ParsedOption, String> {
    let context =
        |field: &str| format!("option record {ordinal} is missing or has invalid `{field}`");
    let name = parse_string(
        fields
            .get("full_name")
            .ok_or_else(|| context("full_name"))?,
    )
    .ok_or_else(|| context("full_name"))?;
    let short_name = fields
        .get("abbreviation")
        .and_then(|value| parse_string(value));
    let aliases = fields
        .get("alias")
        .map(|value| parse_string_list(value))
        .unwrap_or_default();
    let scopes = parse_string_list(fields.get("scope").ok_or_else(|| context("scope"))?);
    if scopes.is_empty() {
        return Err(context("scope"));
    }
    for scope in &scopes {
        if !matches!(scope.as_str(), "global" | "buf" | "win" | "tab") {
            return Err(format!("option `{name}` has unsupported scope `{scope}`"));
        }
    }

    let value_type = parse_string(fields.get("type").ok_or_else(|| context("type"))?)
        .ok_or_else(|| context("type"))?;
    if !matches!(
        value_type.as_str(),
        "boolean" | "number" | "string" | "func" | "expr"
    ) {
        return Err(format!(
            "option `{name}` has unsupported type `{value_type}`"
        ));
    }

    let default = parse_default(fields.get("defaults"))?;
    let list = fields.get("list").and_then(|value| parse_string(value));
    if let Some(kind) = &list
        && !matches!(
            kind.as_str(),
            "comma" | "onecomma" | "commacolon" | "onecommacolon" | "flags" | "flagscomma"
        )
    {
        return Err(format!(
            "option `{name}` has unsupported list kind `{kind}`"
        ));
    }

    Ok(ParsedOption {
        name,
        short_name,
        aliases,
        scopes,
        value_type,
        default,
        list,
        deny_duplicates: fields
            .get("deny_duplicates")
            .is_some_and(|value| value.trim() == "true"),
        expand: fields.contains_key("expand"),
    })
}

fn parse_default(value: Option<&String>) -> Result<ParsedDefault, String> {
    let Some(raw) = value else {
        return Ok(ParsedDefault {
            value: None,
            raw: String::new(),
            conditional: false,
            present: false,
        });
    };
    let raw = raw.trim().to_owned();
    let conditional = raw.starts_with('{');
    let selected = if conditional {
        let close = matching_delimiter(&raw, 0, b'{', b'}')?;
        let fields = parse_fields(&raw, 1, close)?;
        let branch = match fields
            .get("condition")
            .and_then(|value| parse_string(value))
        {
            Some(condition) if !condition_enabled(&condition)? => "if_false",
            _ => "if_true",
        };
        fields.get(branch).cloned()
    } else {
        Some(raw.clone())
    };

    Ok(ParsedDefault {
        value: selected.as_deref().and_then(parse_literal),
        raw,
        conditional,
        present: true,
    })
}

fn condition_enabled(condition: &str) -> Result<bool, String> {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_family = env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    match condition {
        "UNIX" => Ok(target_family.split(',').any(|family| family == "unix")),
        "MSWIN" | "USE_CRNL" | "CASE_INSENSITIVE_FILENAME" | "BACKSLASH_IN_FILENAME" => {
            Ok(target_os == "windows")
        }
        other => Err(format!(
            "unsupported options.lua default condition `{other}`"
        )),
    }
}

fn parse_literal(value: &str) -> Option<Literal> {
    let value = value.trim();
    match value {
        "true" => Some(Literal::Boolean(true)),
        "false" => Some(Literal::Boolean(false)),
        // globals.h:103-104 — DFLT_COLS 80, DFLT_ROWS 24 — the headless
        // baseline the oldtest screen-size guard (`&lines < 24 ||
        // &columns < 80`) relies on.
        "macros('DFLT_COLS', 'number')" => Some(Literal::Number(80)),
        "macros('DFLT_ROWS', 'number')" => Some(Literal::Number(24)),
        "macros('DFLT_GREPFORMAT', 'string')" => {
            Some(Literal::String("%f:%l:%m,%f:%l%m,%f  %l%m".to_owned()))
        }
        // option_vars.h CPO_VIM — gen_opt_test.vim `:set cpo&` needs a literal.
        "macros('CPO_VIM', 'string')" => Some(Literal::String("aABceFs_".to_owned())),
        "macros('CTRL_F_STR', 'string')" => Some(Literal::String("\u{6}".to_owned())),
        "macros('ENC_DFLT', 'string')" => Some(Literal::String("utf-8".to_owned())),
        "macros('DFLT_ERRORFILE', 'string')" => {
            Some(Literal::String("errors.err".to_owned()))
        }
        "macros('DFLT_FO_VIM', 'string')" => Some(Literal::String("tcqj".to_owned())),
        "macros('DFLT_GFN', 'string')" => {
            // option_vars.h:38-46 selects by target OS, not host.
            let fonts = match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
                Ok("windows") => "Cascadia Code,Cascadia Mono,Consolas,Courier New,monospace",
                Ok("macos") => "SF Mono,Menlo,Monaco,Courier New,monospace",
                Ok("linux") => "Source Code Pro,DejaVu Sans Mono,Courier New,monospace",
                _ => "DejaVu Sans Mono,Courier New,monospace",
            };
            Some(Literal::String(fonts.to_owned()))
        }
        "macros('DFLT_HELPFILE', 'string')" => {
            Some(Literal::String("$VIMRUNTIME/doc/help.txt".to_owned()))
        }
        "macros('HIGHLIGHT_INIT', 'string')" => Some(Literal::String(
            // option_vars.h:15-24, concatenated C parts verbatim
            "8:SpecialKey,~:EndOfBuffer,z:TermCursor,@:NonText,d:Directory,e:ErrorMsg,i:IncSearch,l:Search,y:CurSearch,m:MoreMsg,M:ModeMsg,n:LineNr,a:LineNrAbove,b:LineNrBelow,N:CursorLineNr,G:CursorLineSign,O:CursorLineFold,r:Question,s:StatusLine,S:StatusLineNC,c:VertSplit,t:Title,v:Visual,V:VisualNOS,w:WarningMsg,W:WildMenu,f:Folded,F:FoldColumn,A:DiffAdd,C:DiffChange,D:DiffDelete,T:DiffText,E:DiffTextAdd,>:SignColumn,-:Conceal,B:SpellBad,P:SpellCap,R:SpellRare,L:SpellLocal,+:Pmenu,=:PmenuSel,k:PmenuMatch,<:PmenuMatchSel,[:PmenuKind,]:PmenuKindSel,{:PmenuExtra,}:PmenuExtraSel,x:PmenuSbar,X:PmenuThumb,*:TabLine,#:TabLineSel,_:TabLineFill,!:CursorColumn,.:CursorLine,o:ColorColumn,q:QuickFixLine,z:StatusLineTerm,Z:StatusLineTermNC,g:MsgArea,h:ComplMatchIns,0:Whitespace,I:PreInsert".to_owned(),
        )),
        "macros('LISPWORD_VALUE', 'string')" => Some(Literal::String(
            // option_vars.h:161-162
            "defun,define,defmacro,set!,lambda,if,case,let,flet,let*,letrec,do,do*,define-syntax,let-syntax,letrec-syntax,destructuring-bind,defpackage,defparameter,defstruct,deftype,defvar,do-all-symbols,do-external-symbols,do-symbols,dolist,dotimes,ecase,etypecase,eval-when,labels,macrolet,multiple-value-bind,multiple-value-call,multiple-value-prog1,multiple-value-setq,prog1,progv,typecase,unless,unwind-protect,when,with-input-from-string,with-open-file,with-open-stream,with-output-to-string,with-package-iterator,define-condition,handler-bind,handler-case,restart-bind,restart-case,with-simple-restart,store-value,use-value,muffle-warning,abort,continue,with-slots,with-slots*,with-accessors,defclass,defmethod,print-unreadable-object".to_owned(),
        )),
        "macros('B_IMODE_NONE', 'number')" => Some(Literal::Number(0)),
        "macros('B_IMODE_USE_INSERT', 'number')" => Some(Literal::Number(-1)),
        "macros('MAX_MCO', 'number')" => Some(Literal::Number(6)),
        "macros('TAB', 'number')" => Some(Literal::Number(9)),
        "macros('DFLT_EFM', 'string')" => Some(Literal::String(
            // option_vars.h:33-34 DFLT_EFM, byte-verified against
            // `nvim --cmd 'echo string(&efm)'` (VimL '' un-escaped)
            r#"%*[^"]"%f"%*\D%l: %m,"%f"%*\D%l: %m,%-Gg%\?make[%*\d]: *** [%f:%l:%m,%-Gg%\?make: *** [%f:%l:%m,%-G%f:%l: (Each undeclared identifier is reported only once,%-G%f:%l: for each function it appears in.),%-GIn file included from %f:%l:%c:,%-GIn file included from %f:%l:%c\,,%-GIn file included from %f:%l:%c,%-GIn file included from %f:%l,%-G%*[ ]from %f:%l:%c,%-G%*[ ]from %f:%l:,%-G%*[ ]from %f:%l\,,%-G%*[ ]from %f:%l,%f:%l:%c:%m,%f(%l):%m,%f:%l:%m,"%f"\, line %l%*\D%c%*[^ ] %m,%D%*\a[%*\d]: Entering directory %*[`']%f',%X%*\a[%*\d]: Leaving directory %*[`']%f',%D%*\a: Entering directory %*[`']%f',%X%*\a: Leaving directory %*[`']%f',%DMaking %*\a in %f,%f|%l| %m"#.to_owned(),
        )),
        other if other.starts_with("macros(") => panic!(
            "unmapped option default macro: {other}; add the literal from option_vars.h/globals.h instead of silently emitting default: None"
        ),
        _ => parse_string(value)
            .map(Literal::String)
            .or_else(|| value.parse::<i64>().ok().map(Literal::Number)),
    }
}

fn parse_string_list(value: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut cursor = 0;
    let bytes = value.as_bytes();
    while cursor < bytes.len() {
        if matches!(bytes[cursor], b'\'' | b'"')
            && let Some((string, end)) = decode_quoted(value, cursor)
        {
            strings.push(string);
            cursor = end;
            continue;
        }
        cursor += 1;
    }
    strings
}

fn parse_string(value: &str) -> Option<String> {
    let value = value.trim();
    let (decoded, end) = decode_quoted(value, 0)?;
    (end == value.len()).then_some(decoded)
}

fn decode_quoted(value: &str, start: usize) -> Option<(String, usize)> {
    let bytes = value.as_bytes();
    let quote = *bytes.get(start)?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }

    let mut decoded = String::new();
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            byte if byte == quote => return Some((decoded, cursor + 1)),
            b'\\' => {
                cursor += 1;
                let escaped = *bytes.get(cursor)?;
                match escaped {
                    b'n' => decoded.push('\n'),
                    b'r' => decoded.push('\r'),
                    b't' => decoded.push('\t'),
                    b'\\' => decoded.push('\\'),
                    b'\'' => decoded.push('\''),
                    b'"' => decoded.push('"'),
                    b'0'..=b'9' => {
                        let number_start = cursor;
                        let mut number_end = cursor + 1;
                        while number_end < bytes.len()
                            && number_end < number_start + 3
                            && bytes[number_end].is_ascii_digit()
                        {
                            number_end += 1;
                        }
                        let number = value[number_start..number_end].parse::<u8>().ok()?;
                        decoded.push(char::from(number));
                        cursor = number_end - 1;
                    }
                    other => decoded.push(char::from(other)),
                }
            }
            _ => {
                let character = value[cursor..].chars().next()?;
                decoded.push(character);
                cursor += character.len_utf8() - 1;
            }
        }
        cursor += 1;
    }
    None
}

fn parse_fields(
    source: &str,
    start: usize,
    end: usize,
) -> Result<BTreeMap<String, String>, String> {
    let bytes = source.as_bytes();
    let mut fields = BTreeMap::new();
    let mut cursor = start;

    while cursor < end {
        cursor = skip_trivia(source, cursor, end)?;
        if cursor >= end {
            break;
        }
        if bytes[cursor] == b',' {
            cursor += 1;
            continue;
        }
        if !is_identifier_start(bytes[cursor]) {
            cursor = skip_value(source, cursor, end)?;
            continue;
        }

        let key_start = cursor;
        cursor += 1;
        while cursor < end && is_identifier_continue(bytes[cursor]) {
            cursor += 1;
        }
        let key = &source[key_start..cursor];
        cursor = skip_trivia(source, cursor, end)?;
        if cursor >= end || bytes[cursor] != b'=' {
            cursor = skip_value(source, cursor, end)?;
            continue;
        }
        cursor += 1;
        cursor = skip_trivia(source, cursor, end)?;
        let value_start = cursor;
        cursor = skip_value(source, cursor, end)?;
        let value = source[value_start..cursor].trim().to_owned();
        if fields.insert(key.to_owned(), value).is_some() {
            return Err(format!("duplicate `{key}` field in option record"));
        }
        if cursor < end && bytes[cursor] == b',' {
            cursor += 1;
        }
    }

    Ok(fields)
}

fn skip_value(source: &str, start: usize, end: usize) -> Result<usize, String> {
    let bytes = source.as_bytes();
    let mut cursor = start;
    let mut braces = 0usize;
    let mut brackets = 0usize;
    let mut parentheses = 0usize;

    while cursor < end {
        if let Some(next) = skip_non_code(source, cursor, end)? {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'{' => braces += 1,
            b'}' if braces > 0 => braces -= 1,
            b'[' => brackets += 1,
            b']' if brackets > 0 => brackets -= 1,
            b'(' => parentheses += 1,
            b')' if parentheses > 0 => parentheses -= 1,
            b',' if braces == 0 && brackets == 0 && parentheses == 0 => break,
            _ => {}
        }
        cursor += 1;
    }
    Ok(cursor)
}

fn matching_delimiter(source: &str, open: usize, left: u8, right: u8) -> Result<usize, String> {
    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&left) {
        return Err(format!("expected `{}` at byte {open}", char::from(left)));
    }
    let mut depth = 0usize;
    let mut cursor = open;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code(source, cursor, bytes.len())? {
            cursor = next;
            continue;
        }
        if bytes[cursor] == left {
            depth += 1;
        } else if bytes[cursor] == right {
            depth -= 1;
            if depth == 0 {
                return Ok(cursor);
            }
        }
        cursor += 1;
    }
    Err(format!(
        "unterminated `{}` beginning at byte {open}",
        char::from(left)
    ))
}

fn skip_trivia(source: &str, mut cursor: usize, end: usize) -> Result<usize, String> {
    let bytes = source.as_bytes();
    loop {
        while cursor < end && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor + 1 < end && &bytes[cursor..cursor + 2] == b"--" {
            cursor = skip_comment(source, cursor, end)?;
        } else {
            return Ok(cursor);
        }
    }
}

fn skip_non_code(source: &str, cursor: usize, end: usize) -> Result<Option<usize>, String> {
    let bytes = source.as_bytes();
    if matches!(bytes[cursor], b'\'' | b'"') {
        return skip_quoted(source, cursor, end).map(Some);
    }
    if bytes[cursor] == b'['
        && let Some((equals, content_start)) = long_bracket_open(source, cursor)
    {
        return find_long_bracket_close(source, content_start, equals, end).map(Some);
    }
    if cursor + 1 < end && &bytes[cursor..cursor + 2] == b"--" {
        return skip_comment(source, cursor, end).map(Some);
    }
    Ok(None)
}

fn skip_quoted(source: &str, start: usize, end: usize) -> Result<usize, String> {
    let bytes = source.as_bytes();
    let quote = bytes[start];
    let mut cursor = start + 1;
    while cursor < end {
        if bytes[cursor] == b'\\' {
            cursor = (cursor + 2).min(end);
        } else if bytes[cursor] == quote {
            return Ok(cursor + 1);
        } else {
            cursor += 1;
        }
    }
    Err(format!("unterminated quoted string at byte {start}"))
}

fn skip_comment(source: &str, start: usize, end: usize) -> Result<usize, String> {
    let after_marker = start + 2;
    if after_marker < end
        && let Some((equals, content_start)) = long_bracket_open(source, after_marker)
    {
        return find_long_bracket_close(source, content_start, equals, end);
    }
    Ok(source[after_marker..end]
        .find('\n')
        .map_or(end, |offset| after_marker + offset + 1))
}

fn long_bracket_open(source: &str, start: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let mut cursor = start + 1;
    while bytes.get(cursor) == Some(&b'=') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'[')).then_some((cursor - start - 1, cursor + 1))
}

fn find_long_bracket_close(
    source: &str,
    content_start: usize,
    equals: usize,
    end: usize,
) -> Result<usize, String> {
    let closing = format!("]{}]", "=".repeat(equals));
    source[content_start..end]
        .find(&closing)
        .map(|offset| content_start + offset + closing.len())
        .ok_or_else(|| format!("unterminated Lua long string at byte {content_start}"))
}

fn has_exact_indent(source: &str, position: usize, spaces: usize) -> bool {
    let line_start = source[..position].rfind('\n').map_or(0, |index| index + 1);
    position - line_start == spaces
        && source.as_bytes()[line_start..position]
            .iter()
            .all(|byte| *byte == b' ')
}

fn is_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

// One linear codegen pass per option; splitting would obscure the output shape.
#[allow(clippy::too_many_lines)]
fn render(options: &[ParsedOption]) -> Result<String, String> {
    let mut names = HashMap::<String, String>::new();
    for option in options {
        register_name(&mut names, &option.name, &option.name)?;
        if let Some(short_name) = &option.short_name {
            register_name(&mut names, short_name, &option.name)?;
        }
        for alias in &option.aliases {
            register_name(&mut names, alias, &option.name)?;
        }
    }

    let mut output = String::from(
        "// @generated by crates/ox-editor/build.rs; do not edit.\n\
         /// Metadata for every option in the authoritative upstream table.\n\
         pub static OPTION_METADATA: &[OptionMetadata] = &[\n",
    );
    for option in options {
        output.push_str("    OptionMetadata {\n");
        let _ = writeln!(output, "        name: {:?},", option.name);
        output.push_str("        short_name: ");
        match &option.short_name {
            Some(name) => {
                let _ = writeln!(output, "Some({name:?}),");
            }
            None => output.push_str("None,\n"),
        }
        output.push_str("        aliases: &[");
        for (index, alias) in option.aliases.iter().enumerate() {
            if index > 0 {
                output.push_str(", ");
            }
            let _ = write!(output, "{alias:?}");
        }
        output.push_str("],\n        scopes: &[");
        for (index, scope) in option.scopes.iter().enumerate() {
            if index > 0 {
                output.push_str(", ");
            }
            output.push_str(match scope.as_str() {
                "global" => "OptionScope::Global",
                "buf" => "OptionScope::Buffer",
                "win" => "OptionScope::Window",
                "tab" => "OptionScope::Tab",
                _ => return Err(format!("unsupported scope `{scope}` during rendering")),
            });
        }
        output.push_str("],\n        value_type: ");
        output.push_str(match option.value_type.as_str() {
            "boolean" => "OptionType::Boolean",
            "number" => "OptionType::Number",
            "string" | "func" | "expr" => "OptionType::String",
            other => {
                return Err(format!(
                    "unsupported option type `{other}` during rendering"
                ));
            }
        });
        output.push_str(",\n        upstream_type: ");
        let _ = writeln!(output, "{:?},", option.value_type);
        output.push_str("        default: OptionDefault { value: ");
        match &option.default.value {
            Some(Literal::Boolean(value)) => {
                let _ = write!(output, "Some(OptionDefaultValue::Boolean({value}))");
            }
            Some(Literal::Number(value)) => {
                let _ = write!(output, "Some(OptionDefaultValue::Number({value}))");
            }
            Some(Literal::String(value)) => {
                let _ = write!(output, "Some(OptionDefaultValue::String({value:?}))");
            }
            None => output.push_str("None"),
        }
        let _ = writeln!(
            output,
            ", raw: {:?}, conditional: {}, present: {} }},",
            option.default.raw, option.default.conditional, option.default.present
        );
        output.push_str("        list: ");
        output.push_str(match option.list.as_deref() {
            None => "None",
            Some("comma") => "Some(OptionListKind::Comma)",
            Some("onecomma") => "Some(OptionListKind::OneComma)",
            Some("commacolon") => "Some(OptionListKind::CommaColon)",
            Some("onecommacolon") => "Some(OptionListKind::OneCommaColon)",
            Some("flags") => "Some(OptionListKind::Flags)",
            Some("flagscomma") => "Some(OptionListKind::FlagsComma)",
            Some(other) => return Err(format!("unsupported list kind `{other}` during rendering")),
        });
        let _ = writeln!(
            output,
            ",\n        deny_duplicates: {},\n        expand: {},\n    }},",
            option.deny_duplicates, option.expand
        );
    }
    output.push_str("];\n\n/// Number of generated upstream options.\npub const OPTION_COUNT: usize = OPTION_METADATA.len();\n\n");
    output.push_str(
        "/// Looks up option metadata by canonical name, abbreviation, or alias.\n\
         // Generated match arms are a flat data table; line count is inherent.\n\
         #[must_use]\n\
         #[allow(clippy::too_many_lines)]\n\
         pub fn option_metadata(name: &str) -> Option<&'static OptionMetadata> {\n    match name {\n",
    );
    for (index, option) in options.iter().enumerate() {
        let mut option_names = Vec::with_capacity(2 + option.aliases.len());
        option_names.push(option.name.as_str());
        if let Some(short_name) = &option.short_name {
            option_names.push(short_name);
        }
        option_names.extend(option.aliases.iter().map(String::as_str));
        output.push_str("        ");
        for (name_index, name) in option_names.iter().enumerate() {
            if name_index > 0 {
                output.push_str(" | ");
            }
            let _ = write!(output, "{name:?}");
        }
        let _ = writeln!(output, " => Some(&OPTION_METADATA[{index}]),");
    }
    output.push_str("        _ => None,\n    }\n}\n");
    Ok(output)
}

fn register_name(
    names: &mut HashMap<String, String>,
    spelling: &str,
    canonical: &str,
) -> Result<(), String> {
    if let Some(existing) = names.insert(spelling.to_owned(), canonical.to_owned()) {
        return Err(format!(
            "option name or alias `{spelling}` is shared by `{existing}` and `{canonical}`"
        ));
    }
    Ok(())
}
