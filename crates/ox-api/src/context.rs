//! Editor context save/load API.

use ox_editor::{Editor, MarkTarget, RegisterContent, RegisterKind};

use crate::runtime::with_state_mut;
use crate::{api, ApiError, Dict, Object, OxStr, Registry, RegistryError};

const TYPES: [&str; 5] = ["regs", "jumps", "bufs", "gvars", "funcs"];

fn selected(opts: &Dict) -> Result<Vec<String>, ApiError> {
    let Some(value) = opts.get(&OxStr::from("types")) else { return Ok(TYPES.iter().map(ToString::to_string).collect()); };
    let Object::Array(values) = value else { return Err(ApiError::validation("types must be an Array")); };
    values.iter().map(|value| {
        let Object::String(value) = value else { return Err(ApiError::validation("context type must be a String")); };
        let name = String::from_utf8(value.0.clone()).map_err(|_| ApiError::validation("context type must be UTF-8"))?;
        if !TYPES.contains(&name.as_str()) { return Err(ApiError::validation(format!("Invalid context type: {name}"))); }
        Ok(name)
    }).collect()
}

fn register_type(kind: RegisterKind) -> OxStr {
    match kind {
        RegisterKind::CharacterWise => OxStr::from("v"),
        RegisterKind::LineWise => OxStr::from("V"),
        RegisterKind::BlockWise { .. } => OxStr::from("\u{16}"),
    }
}

fn save_registers(editor: &Editor) -> Vec<Object> {
    ('0'..='9').chain('a'..='z').chain(['"', '-']).filter_map(|name| {
        editor.registers().get(name).ok().flatten().map(|content| Object::Dict(Dict(vec![
            (OxStr::from("name"), Object::String(OxStr::from(name.to_string().as_str()))),
            (OxStr::from("type"), Object::String(register_type(content.kind()))),
            (OxStr::from("lines"), Object::Array(content.lines().iter().map(|line| Object::String(OxStr::from(line.as_slice()))).collect())),
        ])))
    }).collect()
}

fn save_jumps(editor: &Editor) -> Vec<Object> {
    editor.jumplist().entries().iter().map(|jump| {
        let mut values = vec![
            (OxStr::from("lnum"), Object::Integer(i64::try_from(jump.position.lnum).unwrap_or(i64::MAX))),
            (OxStr::from("col"), Object::Integer(i64::try_from(jump.position.col).unwrap_or(i64::MAX))),
        ];
        match &jump.target {
            MarkTarget::Buffer(buffer) => values.push((OxStr::from("bufnr"), Object::Integer(i64::from(*buffer)))),
            MarkTarget::File(path) => values.push((OxStr::from("filename"), Object::String(OxStr::from(path.to_string_lossy().as_bytes())))),
        }
        Object::Dict(Dict(values))
    }).collect()
}

#[api(since = 6)]
pub fn nvim_get_context(editor: &mut Editor, opts: Dict) -> Result<Dict, ApiError> {
    let selected = selected(&opts)?;
    let mut values = Vec::new();
    for kind in selected {
        let value = match kind.as_str() {
            "regs" => Object::Array(save_registers(editor)),
            "jumps" => Object::Array(save_jumps(editor)),
            "bufs" => Object::Array(editor.buffers().into_iter().map(Object::Buffer).collect()),
            "gvars" => Object::Dict(editor.vvars().clone()),
            "funcs" => Object::Array(Vec::new()),
            _ => continue,
        };
        values.push((OxStr::from(kind.as_str()), value));
    }
    let context = Dict(values);
    with_state_mut(editor, |state| state.saved_context = Some(context.clone()));
    Ok(context)
}

fn load_registers(editor: &mut Editor, values: &[Object]) -> Result<(), ApiError> {
    for value in values {
        let Object::Dict(entry) = value else { return Err(ApiError::validation("context register must be a Dictionary")); };
        let Some(Object::String(name)) = entry.get(&OxStr::from("name")) else { return Err(ApiError::validation("context register requires name")); };
        let name = std::str::from_utf8(name.as_bytes()).ok().and_then(|name| name.chars().next()).ok_or_else(|| ApiError::validation("invalid context register name"))?;
        let kind = match entry.get(&OxStr::from("type")) {
            Some(Object::String(value)) if value.as_bytes() == b"V" => RegisterKind::LineWise,
            Some(Object::String(value)) if value.as_bytes() == [0x16] => RegisterKind::BlockWise { width: 1 },
            _ => RegisterKind::CharacterWise,
        };
        let Some(Object::Array(lines)) = entry.get(&OxStr::from("lines")) else { return Err(ApiError::validation("context register requires lines")); };
        let lines = lines.iter().map(|line| match line { Object::String(line) => Ok(line.0.clone()), _ => Err(ApiError::validation("register line must be a String")) }).collect::<Result<Vec<_>, _>>()?;
        let kind = match kind { RegisterKind::BlockWise { .. } => RegisterKind::BlockWise { width: lines.iter().map(Vec::len).max().unwrap_or(1).max(1) }, other => other };
        let content = RegisterContent::new(kind, lines).map_err(|error| ApiError::validation(error.to_string()))?;
        editor.registers_mut().set(name, content).map_err(|error| ApiError::validation(error.to_string()))?;
    }
    Ok(())
}

#[api(since = 6)]
pub fn nvim_load_context(editor: &mut Editor, dict: Dict) -> Result<Object, ApiError> {
    for (key, value) in &dict.0 {
        match (key.as_bytes(), value) {
            (b"regs", Object::Array(values)) => load_registers(editor, values)?,
            (b"gvars", Object::Dict(values)) => *editor.vvars_mut() = values.clone(),
            (b"jumps" | b"bufs" | b"funcs", Object::Array(_)) => {},
            (b"regs" | b"gvars" | b"jumps" | b"bufs" | b"funcs", _) => return Err(ApiError::validation(format!("invalid context value for {}", key.to_string_lossy()))),
            _ => return Err(ApiError::validation(format!("Invalid context type: {}", key.to_string_lossy()))),
        }
    }
    with_state_mut(editor, |state| state.saved_context = Some(dict));
    Ok(Object::Nil)
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_get_context__API_META(), nvim_get_context__API_DISPATCH)?;
    registry.register(nvim_load_context__API_META(), nvim_load_context__API_DISPATCH)?;
    Ok(())
}
