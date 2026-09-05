//! Register builtins backed by the editor's existing register bank.

use crate::excmd_exec::ExEditorAccess;
use ox_eval::{EvalError, Scope, ScopeKind, builtin_spec};
use ox_types::{OxStr, Typval};

use crate::excmd_exec::{EvalHost, eval_text};
use crate::register::{RegisterContent, RegisterKind};
use crate::script::FileIO;

use super::input_string_arg;

pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    check_arity(name, args.len())?;
    match name {
        "getreg" => getreg(host, args, scope),
        "getregtype" => getregtype(host, args, scope),
        "setreg" => setreg(host, args, scope),
        "getreginfo" => getreginfo(host, args, scope),
        // The route table admitted a name this dispatcher does not serve;
        // answer the same typed error an unknown name gets instead of
        // aborting the process.
        _ => Err(EvalError::new(
            "E117",
            0,
            format!("Unknown function: {name}"),
        )),
    }
}

fn check_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = builtin_spec(name)
        .ok_or_else(|| EvalError::new("E117", 0, format!("Unknown function: {name}")))?;
    if count < spec.min_args {
        return Err(EvalError::new(
            "E119",
            0,
            format!("Not enough arguments for function: {name}"),
        ));
    }
    if spec.max_args.is_some_and(|maximum| count > maximum) {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    Ok(())
}

/// The single byte `getreg_get_regname` (`eval/funcs.c:2353-2368`) reads:
/// the first byte of the name, with an empty name selecting the unnamed
/// register.
fn first_register_byte(name: &[u8]) -> char {
    name.first().map_or('"', |&byte| byte as char)
}

/// The register a query builtin reads: an explicit argument contributes its
/// first byte, and an omitted argument reads `v:register` from the current
/// scope, falling back to the unnamed register while it is unset.
fn query_register_name(args: &[Typval], scope: &Scope) -> ox_eval::Result<char> {
    match args.first() {
        Some(value) => Ok(first_register_byte(input_string_arg(value)?.as_bytes())),
        None => Ok(scope
            .get_scoped(ScopeKind::Vim, b"register", 0)
            .ok()
            .and_then(|value| input_string_arg(value).ok())
            .map_or('"', |name| first_register_byte(name.as_bytes()))),
    }
}

/// The register `setreg` writes: an empty name and `@` normalize to the
/// unnamed register, and a longer name contributes its first byte
/// (`f_setreg`, `eval/funcs.c:6461-6468`).
fn target_register_name(args: &[Typval]) -> ox_eval::Result<char> {
    let name = match args.first() {
        Some(value) => first_register_byte(input_string_arg(value)?.as_bytes()),
        None => '"',
    };
    Ok(if name == '@' { '"' } else { name })
}

fn getreg<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let name = query_register_name(args, scope)?;
    let expression_source = args.get(1).is_some_and(Typval::is_truthy);
    let as_list = args.get(2).is_some_and(Typval::is_truthy);

    if name == '=' {
        let Some(source) = host
            .access
            .with_ex_editor(|editor| editor.registers().expression_source().map(<[u8]>::to_vec))
        else {
            return Ok(empty_getreg(as_list));
        };
        if expression_source {
            return getreg_value(vec![source], RegisterKind::CharacterWise, as_list);
        }
        let source = String::from_utf8_lossy(&source);
        let value = eval_text(host.runtime, host.access, scope, host.lua, &source)
            .map_err(|flow| EvalError::new("E15", 0, format!("{flow:?}")))?;
        let text = input_string_arg(&value)?;
        return getreg_value(
            vec![text.as_bytes().to_vec()],
            RegisterKind::CharacterWise,
            as_list,
        );
    }

    let content = host
        .access
        .with_ex_editor(|editor| {
            editor
                .registers()
                .get(name)
                .map(Option::<&RegisterContent>::cloned)
        })
        .map_err(|error| EvalError::new("E354", 0, error.to_string()))?;
    let Some(content) = content else {
        return Ok(empty_getreg(as_list));
    };
    if as_list {
        return Ok(Typval::list(
            content
                .getreg_lines()
                .into_iter()
                .map(|line| Typval::String(OxStr(line)))
                .collect(),
        ));
    }
    Ok(Typval::String(OxStr(content.getreg_bytes())))
}

fn empty_getreg(as_list: bool) -> Typval {
    if as_list {
        Typval::list(Vec::new())
    } else {
        Typval::String(OxStr::from(""))
    }
}

fn getreg_value(lines: Vec<Vec<u8>>, kind: RegisterKind, as_list: bool) -> ox_eval::Result<Typval> {
    if as_list {
        return Ok(Typval::list(
            lines
                .into_iter()
                .map(|line| Typval::String(OxStr(line)))
                .collect(),
        ));
    }
    let content = RegisterContent::new(kind, lines)
        .map_err(|error| EvalError::new("E354", 0, error.to_string()))?;
    Ok(Typval::String(OxStr(content.getreg_bytes())))
}

fn getregtype<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    let name = query_register_name(args, scope)?;
    if name == '='
        && host
            .access
            .with_ex_editor(|editor| editor.registers().expression_source().is_some())
    {
        return Ok(string_regtype(RegisterKind::CharacterWise));
    }
    let content = host
        .access
        .with_ex_editor(|editor| {
            editor
                .registers()
                .get(name)
                .map(|opt| opt.map(RegisterContent::kind))
        })
        .map_err(|error| EvalError::new("E354", 0, error.to_string()))?;
    let Some(content) = content else {
        return Ok(Typval::String(OxStr::from("")));
    };
    Ok(string_regtype(content))
}

/// The `v`/`V`/Ctrl-V-plus-width register type rendering shared by
/// `getregtype` and `getreginfo` (`format_reg_type`).
fn string_regtype(kind: RegisterKind) -> Typval {
    let rendered = match kind {
        RegisterKind::CharacterWise => "v".to_owned(),
        RegisterKind::LineWise => "V".to_owned(),
        RegisterKind::BlockWise { width } => format!("\u{16}{width}"),
    };
    Typval::String(OxStr::from(rendered.as_str()))
}

/// `f_getreginfo` (`eval/funcs.c:4980-5032`): a Dictionary of `regcontents`,
/// `regtype`, and — for the unnamed register — `points_to`, else
/// `isunnamed`. Invalid or unset registers answer an empty Dictionary; only
/// argument type errors propagate. `=` reports the stored expression source
/// itself as one characterwise line, never its evaluated value.
fn getreginfo<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    let name = query_register_name(args, scope)?;
    let name = if name == '@' { '"' } else { name };
    let (resolved, target) = host.access.with_ex_editor(|editor| {
        let registers = editor.registers();
        // The black hole is a valid register that is always empty, not an unset
        // one (`get_spec_reg`, `register.c:912-914`).
        let resolved = match name {
            '=' => registers
                .expression_source()
                .map(|source| (vec![source.to_vec()], RegisterKind::CharacterWise)),
            '_' => Some((vec![Vec::new()], RegisterKind::CharacterWise)),
            _ => registers
                .get(name)
                .ok()
                .flatten()
                .map(|content| (content.getreg_lines(), content.kind())),
        };
        (resolved, registers.unnamed_target_name())
    });
    let Some((lines, kind)) = resolved else {
        return Ok(Typval::dict(Vec::new()));
    };

    let mut entries = vec![
        (
            OxStr::from("regcontents"),
            Typval::list(
                lines
                    .into_iter()
                    .map(|line| Typval::String(OxStr(line)))
                    .collect(),
            ),
        ),
        (OxStr::from("regtype"), string_regtype(kind)),
    ];
    // `target` is the unnamed register's current pointer, read above.
    if name == '"' {
        let mut buf = [0; 4];
        let pointer: &str = target.encode_utf8(&mut buf);
        entries.push((
            OxStr::from("points_to"),
            Typval::String(OxStr::from(pointer)),
        ));
    } else {
        // Compare the requested byte with the current unnamed target exactly:
        // an `A` query reads `a`'s slot but is not the pointer itself.
        entries.push((OxStr::from("isunnamed"), Typval::Bool(name == target)));
    }
    Ok(Typval::dict(entries))
}

fn setreg<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let name = target_register_name(args)?;
    let inferred = register_content(&args[1])?;
    let flags = args
        .get(2)
        .map(input_string_arg)
        .transpose()?
        .unwrap_or_else(|| OxStr::from(""));
    let flags = flags.to_string_lossy();
    if matches!(name, '/' | '=') && inferred.lines().len() > 1 {
        return Err(EvalError::new(
            "E883",
            0,
            "Search pattern and expression register may not contain two or more lines",
        ));
    }
    let (kind, append, unnamed) = parse_register_type(&flags, inferred.lines(), inferred.kind());
    let content = if kind == inferred.kind() {
        inferred
    } else {
        RegisterContent::new(kind, inferred.getreg_lines())
            .map_err(|error| EvalError::new("E354", 0, error.to_string()))?
    };
    host.access
        .with_ex_editor(|editor| {
            editor
                .registers_mut()
                .set_from_setreg(name, content, append)
        })
        .map_err(|error| EvalError::new("E354", 0, error.to_string()))?;
    // `setreg(..., 'u'/'"')` points the unnamed register at the written slot
    // rather than cloning the content into an independent `"` copy
    // (`op_reg_set_previous`, `register.c:298-307`).
    if unnamed {
        host.access
            .with_ex_editor(|editor| editor.registers_mut().set_unnamed_target(name));
    }
    if name.is_ascii_alphabetic() || matches!(name, '0'..='9' | '"' | '-') {
        let stored_name = name.to_ascii_lowercase();
        if let Some(stored_bytes) = host.access.with_ex_editor(|editor| {
            editor
                .registers()
                .get(stored_name)
                .map_err(|error| EvalError::new("E354", 0, error.to_string()))
                .map(|content| content.map(RegisterContent::getreg_bytes))
        })? {
            scope.set_register(&[stored_name as u8], Typval::String(OxStr(stored_bytes)));
        }
    }
    Ok(Typval::Number(0))
}

fn register_content(value: &Typval) -> ox_eval::Result<RegisterContent> {
    if let Typval::List(values) = value {
        let values = values
            .try_borrow()
            .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
        let lines = values
            .items
            .iter()
            .map(input_string_arg)
            .collect::<ox_eval::Result<Vec<_>>>()?
            .into_iter()
            .map(|line| line.as_bytes().to_vec())
            .collect();
        return RegisterContent::linewise(lines)
            .map_err(|error| EvalError::new("E354", 0, error.to_string()));
    }
    let text = input_string_arg(value)?;
    RegisterContent::from_text(text.as_bytes())
        .map_err(|error| EvalError::new("E354", 0, error.to_string()))
}

fn parse_register_type(
    flags: &str,
    lines: &[Vec<u8>],
    inferred: RegisterKind,
) -> (RegisterKind, bool, bool) {
    let append = flags.bytes().any(|byte| byte.eq_ignore_ascii_case(&b'a'));
    let unnamed = flags.bytes().any(|byte| byte.eq_ignore_ascii_case(&b'u'));
    let kind = if flags.bytes().any(|byte| matches!(byte, b'l' | b'L' | b'V')) {
        RegisterKind::LineWise
    } else if let Some(index) = flags
        .bytes()
        .position(|byte| matches!(byte, b'b' | b'B' | 0x16))
    {
        let digits = flags[index + 1..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .collect::<Vec<_>>();
        let width = if digits.is_empty() {
            lines.iter().map(Vec::len).max().unwrap_or(0).max(1)
        } else {
            std::str::from_utf8(&digits)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1)
        };
        RegisterKind::BlockWise { width }
    } else if flags.bytes().any(|byte| matches!(byte, b'c' | b'C' | b'v')) {
        RegisterKind::CharacterWise
    } else {
        inferred
    };
    (kind, append, unnamed)
}
