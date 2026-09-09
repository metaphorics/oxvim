//! Tabpage-scoped Neovim API functions.

use ox_editor::{AutocmdContext, BufferRelease, Event, Geometry, TabpageState};

use crate::{
    ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError, TabHandle, WinHandle, api,
    session::ApiSession,
};

fn exception(error: impl std::fmt::Display) -> ApiError {
    ApiError::exception(error.to_string())
}

fn fire_tabpage_event(
    session: &ApiSession,
    event: Event,
    buffer: BufHandle,
) -> Result<(), ApiError> {
    let plan = session.with_editor_mut(|editor| {
        editor.autocmds_mut().plan(
            event,
            AutocmdContext {
                buffer: Some(buffer),
                ..AutocmdContext::default()
            },
        )
    });
    crate::autocmd::execute_firing_plan(session, plan)
}

fn resolve_tabpage(session: &ApiSession, tabpage: TabHandle) -> Result<TabHandle, ApiError> {
    session.with_editor(|editor| {
        if !tabpage.is_current() {
            editor.tabpage(tabpage).map_err(exception)?;
            return Ok(tabpage);
        }
        editor
            .current_tabpage()
            .ok_or_else(|| ApiError::exception("No current tabpage"))
    })
}

#[api(since = 1, method)]
pub fn nvim_tabpage_list_wins(
    session: &ApiSession,
    tabpage: TabHandle,
) -> Result<Vec<WinHandle>, ApiError> {
    let tabpage = resolve_tabpage(session, tabpage)?;
    session.with_editor(|editor| {
        editor
            .tabpage(tabpage)
            .map(ox_editor::TabpageState::windows)
            .map_err(exception)
    })
}

#[api(since = 1, method)]
pub fn nvim_tabpage_get_win(
    session: &ApiSession,
    tabpage: TabHandle,
) -> Result<WinHandle, ApiError> {
    let tabpage = resolve_tabpage(session, tabpage)?;
    session.with_editor(|editor| {
        editor
            .tabpage(tabpage)
            .map(ox_editor::TabpageState::current_window)
            .map_err(exception)
    })
}

#[api(since = 12, method)]
pub fn nvim_tabpage_set_win(
    session: &ApiSession,
    tabpage: TabHandle,
    win: WinHandle,
) -> Result<(), ApiError> {
    let tabpage = resolve_tabpage(session, tabpage)?;
    let (owner, original_tabpage) = session.with_editor(|editor| {
        Ok::<_, ApiError>((
            editor.window_tabpage(win).map_err(exception)?,
            editor.current_tabpage(),
        ))
    })?;
    if owner != tabpage {
        return Err(ApiError::exception(format!(
            "Window does not belong to tabpage {}",
            i64::from(tabpage)
        )));
    }
    session.with_editor_mut(|editor| {
        editor.set_current_window(win).map_err(exception)?;
        if let Some(original) = original_tabpage.filter(|current| *current != tabpage) {
            editor.set_current_tabpage(original).map_err(exception)?;
        }
        Ok(())
    })
}

#[api(since = 1, method)]
pub fn nvim_tabpage_is_valid(session: &ApiSession, tabpage: TabHandle) -> Result<bool, ApiError> {
    session.with_editor(|editor| {
        if tabpage.is_current() {
            return Ok(editor.current_tabpage().is_some());
        }
        Ok(editor.tabpage(tabpage).is_ok())
    })
}

#[api(since = 1, method)]
pub fn nvim_tabpage_get_number(session: &ApiSession, tabpage: TabHandle) -> Result<i64, ApiError> {
    let tabpage = resolve_tabpage(session, tabpage)?;
    session.with_editor(|editor| {
        let index = editor
            .tabpages()
            .iter()
            .position(|candidate| *candidate == tabpage)
            .ok_or_else(|| {
                ApiError::exception(format!("Invalid tabpage id: {}", i64::from(tabpage)))
            })?;
        i64::try_from(index)
            .map(|number| number + 1)
            .map_err(|_| ApiError::exception("Tabpage number exceeds Integer range"))
    })
}
#[api(since = 14, textlock)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "RPC dispatch owns decoded config dictionaries"
)]
pub fn nvim_open_tabpage(
    session: &ApiSession,
    buf: BufHandle,
    enter: bool,
    config: Dict,
) -> Result<TabHandle, ApiError> {
    // Like `:tabnew` in `ox-excmd`, whose port has no screen model to ask.
    const TABPAGE_GEOMETRY: Geometry = Geometry {
        row: 0,
        col: 0,
        width: 80,
        height: 24,
    };
    let buffer = crate::buffer::resolve_buffer(session, buf)?;
    // Upstream `Dict(tabpage_config)` (`api/keysets_defs.h`): the only key is
    // `after`.
    for (name, _) in config.iter() {
        if name.as_bytes() != b"after" {
            return Err(invalid_config(
                name.to_string_lossy().as_ref(),
                "unexpected key",
            ));
        }
    }
    let after = match key(&config, "after") {
        Some(Object::Integer(value)) => {
            // Upstream computes `win_new_tabpage(after + 1)`, so the default
            // `-1` becomes the editor's "after current" sentinel `0`; `0`
            // becomes the first position, `N` inserts before tabpage `N + 1`,
            // and any value leaving the sum below zero also lands "after
            // current", because upstream only positions explicitly for
            // arguments above zero (`window.c:4527-4539`).
            usize::try_from(value.saturating_add(1).max(0))
                .map_err(|_| invalid_config("after", "value exceeds platform range"))?
        }
        Some(_) => return Err(invalid_config("after", "expected Integer")),
        None => 0,
    };
    let (original, original_buffer, caller_window) =
        session.with_editor(|editor| {
            (
                editor.current_tabpage(),
                editor.current_buffer(),
                editor.current_window(),
            )
        });
    let original_buffer =
        original_buffer.ok_or_else(|| ApiError::exception("No current buffer"))?;
    if enter {
        fire_tabpage_event(session, Event::WinLeave, original_buffer)?;
        fire_tabpage_event(session, Event::TabLeave, original_buffer)?;
    }
    // `win_new_tabpage` creates the first window on the old current buffer.
    // The API installs the requested buffer only after creation events.
    let tabpage = session
        .with_editor_mut(|editor| {
            editor.create_tabpage_at(original_buffer, TABPAGE_GEOMETRY, after)
        })
        .map_err(exception)?;
    let events = if enter {
        [
            Some(Event::WinNew),
            Some(Event::WinEnter),
            Some(Event::TabNew),
            Some(Event::TabEnter),
        ]
    } else {
        [Some(Event::WinNew), Some(Event::TabNew), None, None]
    };
    let event_result = events
        .into_iter()
        .flatten()
        .try_for_each(|event| fire_tabpage_event(session, event, original_buffer));
    let restore_result = original
        .filter(|_| !enter)
        .map(|original| {
            session
                .with_editor_mut(|editor| editor.set_current_tabpage(original))
                .map_err(exception)
        })
        .transpose();
    event_result?;
    restore_result?;
    let window = session
        .with_editor(|editor| editor.tabpage(tabpage).map(TabpageState::current_window))
        .map_err(|_| ApiError::exception("Tabpage was closed immediately"))?;
    let switched = crate::window::load_buffer_for_switch(
        session,
        buffer,
        move |session| {
            session.with_editor_mut(|editor| {
                if editor.current_window() != Some(window) {
                    editor.set_current_window(window).map_err(exception)?;
                }
                editor
                    .set_window_buffer(window, buffer, BufferRelease::KeepLoaded)
                    .map_err(exception)
            })
        },
        move |session, succeeded| {
            session.with_editor_mut(|editor| {
                if !succeeded
                    && editor
                        .window(window)
                        .is_ok_and(|state| state.buffer == buffer)
                    && editor.buffer(original_buffer).is_ok()
                {
                    let _ = editor.set_window_buffer(
                        window,
                        original_buffer,
                        BufferRelease::KeepLoaded,
                    );
                }
                if !enter
                    && let Some(caller_window) = caller_window
                    && editor.window(caller_window).is_ok()
                {
                    let _ = editor.set_current_window(caller_window);
                }
            });
        },
    )?;
    if !switched {
        session
            .with_editor_mut(|editor| {
                editor.set_window_buffer(window, buffer, BufferRelease::KeepLoaded)
            })
            .map_err(exception)?;
    }
    if session.with_editor(|editor| editor.tabpage(tabpage).is_err()) {
        return Err(ApiError::exception("Tabpage was closed immediately"));
    }
    Ok(tabpage)
}

fn key<'a>(dict: &'a Dict, name: &str) -> Option<&'a Object> {
    dict.iter()
        .find(|(candidate, _)| candidate.as_bytes() == name.as_bytes())
        .map(|(_, value)| value)
}

fn invalid_config(field: &str, message: impl std::fmt::Display) -> ApiError {
    ApiError::validation(format!("Invalid 'config.{field}': {message}"))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes variable names as owned Strings"
)]
#[api(since = 1, method)]
pub fn nvim_tabpage_get_var(
    session: &ApiSession,
    tabpage: TabHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let tabpage = resolve_tabpage(session, tabpage)?;
    session.with_editor(|editor| {
        editor
            .tabpage_variables(tabpage)
            .map_err(exception)?
            .get(&name)
            .cloned()
            .ok_or_else(|| {
                ApiError::validation(format!("Key not found: {}", name.to_string_lossy()))
            })
    })
}

#[api(since = 1, method)]
pub fn nvim_tabpage_set_var(
    session: &ApiSession,
    tabpage: TabHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let tabpage = resolve_tabpage(session, tabpage)?;
    session.with_editor_mut(|editor| {
        editor
            .tabpage_variables_mut(tabpage)
            .map_err(exception)?
            .insert(name, value);
        Ok(())
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes variable names as owned Strings"
)]
#[api(since = 1, method)]
pub fn nvim_tabpage_del_var(
    session: &ApiSession,
    tabpage: TabHandle,
    name: OxStr,
) -> Result<(), ApiError> {
    let tabpage = resolve_tabpage(session, tabpage)?;
    session.with_editor_mut(|editor| {
        let variables = editor.tabpage_variables_mut(tabpage).map_err(exception)?;
        let Some(index) = variables.iter().position(|(key, _)| key == &name) else {
            return Err(ApiError::validation(format!(
                "Key not found: {}",
                name.to_string_lossy()
            )));
        };
        variables.0.remove(index);
        Ok(())
    })
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(
        nvim_tabpage_list_wins__API_META(),
        nvim_tabpage_list_wins__API_DISPATCH,
    )?;
    registry.register(
        nvim_tabpage_get_win__API_META(),
        nvim_tabpage_get_win__API_DISPATCH,
    )?;
    registry.register(
        nvim_tabpage_set_win__API_META(),
        nvim_tabpage_set_win__API_DISPATCH,
    )?;
    registry.register(
        nvim_tabpage_is_valid__API_META(),
        nvim_tabpage_is_valid__API_DISPATCH,
    )?;
    registry.register(
        nvim_tabpage_get_number__API_META(),
        nvim_tabpage_get_number__API_DISPATCH,
    )?;
    registry.register(
        nvim_tabpage_get_var__API_META(),
        nvim_tabpage_get_var__API_DISPATCH,
    )?;
    registry.register(
        nvim_tabpage_set_var__API_META(),
        nvim_tabpage_set_var__API_DISPATCH,
    )?;
    registry.register(
        nvim_tabpage_del_var__API_META(),
        nvim_tabpage_del_var__API_DISPATCH,
    )?;
    registry.register(
        nvim_open_tabpage__API_META(),
        nvim_open_tabpage__API_DISPATCH,
    )?;
    Ok(())
}
