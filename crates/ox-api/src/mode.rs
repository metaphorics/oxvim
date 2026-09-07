//! Input-mode API.

use ox_editor::{CmdlineKind, Mode, SearchDirection, VisualKind};

use crate::session::ApiSession;
use crate::{ApiError, Dict, Object, OxStr, Registry, RegistryError, api};

/// Returns the canonical Vim mode name for the live input state.
///
/// # Errors
///
/// Returns an error when the host has not installed the mode machine or is
/// mutating it during this query.
pub fn current_mode_name(session: &ApiSession) -> Result<(&'static str, bool), ApiError> {
    let machine = session
        .with_state(|state| state.mode_machine.clone())
        .ok_or_else(|| ApiError::exception("input mode state is not installed"))?;
    let machine = machine
        .try_borrow()
        .map_err(|_| ApiError::exception("input mode state is busy"))?;
    let terminal = session.with_editor(|editor| {
        editor
            .current_buffer()
            .is_some_and(|buffer| editor.is_terminal_buffer(buffer))
    });
    let name = match machine.mode() {
        Mode::Normal(_) if terminal => "nt",
        Mode::Normal(_) => "n",
        Mode::Insert(_) if terminal => "t",
        Mode::Insert(_) => "i",
        Mode::Replace(_) => "R",
        Mode::Cmdline(_) => "c",
        Mode::OperatorPending(_) => "no",
        Mode::Visual(state) => match state.kind {
            VisualKind::Character => "v",
            VisualKind::Line => "V",
            VisualKind::Block => "\u{16}",
        },
    };
    Ok((name, machine.is_blocking()))
}

/// Returns the active command-line prefix, or an empty string outside command-line mode.
///
/// # Errors
///
/// Returns an error when the host has not installed the mode machine or is
/// mutating it during this query.
pub fn current_cmdline_type(session: &ApiSession) -> Result<&'static str, ApiError> {
    let machine = session
        .with_state(|state| state.mode_machine.clone())
        .ok_or_else(|| ApiError::exception("input mode state is not installed"))?;
    let machine = machine
        .try_borrow()
        .map_err(|_| ApiError::exception("input mode state is busy"))?;
    Ok(match machine.mode() {
        Mode::Cmdline(state) => match state.kind {
            CmdlineKind::Ex => ":",
            CmdlineKind::Search(SearchDirection::Forward) => "/",
            CmdlineKind::Search(SearchDirection::Backward) => "?",
        },
        _ => "",
    })
}
/// Returns the command-line text currently being edited.
///
/// # Errors
///
/// Returns an error when the host mode machine is unavailable or busy.
pub fn current_cmdline_text(session: &ApiSession) -> Result<String, ApiError> {
    let machine = session
        .with_state(|state| state.mode_machine.clone())
        .ok_or_else(|| ApiError::exception("input mode state is not installed"))?;
    let machine = machine
        .try_borrow()
        .map_err(|_| ApiError::exception("input mode state is busy"))?;
    Ok(machine.cmdline_text().to_owned())
}

/// Returns one Ex command-history entry using Vim's signed indexing.
///
/// # Errors
///
/// Returns an error when the host mode machine is unavailable or busy.
pub fn command_history(session: &ApiSession, index: isize) -> Result<Option<String>, ApiError> {
    let machine = session
        .with_state(|state| state.mode_machine.clone())
        .ok_or_else(|| ApiError::exception("input mode state is not installed"))?;
    let machine = machine
        .try_borrow()
        .map_err(|_| ApiError::exception("input mode state is busy"))?;
    Ok(machine.cmdline_history(index).map(str::to_owned))
}

/// Returns the register currently recording a macro.
///
/// # Errors
///
/// Returns an error when the host mode machine is unavailable or busy.
pub fn recording_register(session: &ApiSession) -> Result<Option<char>, ApiError> {
    let machine = session
        .with_state(|state| state.mode_machine.clone())
        .ok_or_else(|| ApiError::exception("input mode state is not installed"))?;
    let machine = machine
        .try_borrow()
        .map_err(|_| ApiError::exception("input mode state is busy"))?;
    Ok(machine.recording_register())
}

#[api(since = 2, fast)]
pub fn nvim_get_mode(session: &ApiSession) -> Result<Dict, ApiError> {
    let (name, blocking) = current_mode_name(session)?;
    Ok(Dict(vec![
        (OxStr::from("mode"), Object::String(OxStr::from(name))),
        (OxStr::from("blocking"), Object::Boolean(blocking)),
    ]))
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_get_mode__API_META(), nvim_get_mode__API_DISPATCH)
}
