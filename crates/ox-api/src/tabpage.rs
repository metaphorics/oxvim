//! Tabpage-scoped Neovim API functions.

use ox_editor::Editor;

use crate::{api, ApiError, Object, OxStr, Registry, RegistryError, TabHandle, WinHandle};

fn exception(error: impl std::fmt::Display) -> ApiError {
    ApiError::exception(error.to_string())
}

fn resolve_tabpage(editor: &Editor, tabpage: TabHandle) -> Result<TabHandle, ApiError> {
    if !tabpage.is_current() {
        editor.tabpage(tabpage).map_err(exception)?;
        return Ok(tabpage);
    }
    editor
        .current_tabpage()
        .ok_or_else(|| ApiError::exception("No current tabpage"))
}

#[api(since = 1, method)]
pub fn nvim_tabpage_list_wins(
    editor: &mut Editor,
    tabpage: TabHandle,
) -> Result<Vec<WinHandle>, ApiError> {
    let tabpage = resolve_tabpage(editor, tabpage)?;
    editor.tabpage(tabpage).map(|tab| tab.windows()).map_err(exception)
}

#[api(since = 1, method)]
pub fn nvim_tabpage_get_win(
    editor: &mut Editor,
    tabpage: TabHandle,
) -> Result<WinHandle, ApiError> {
    let tabpage = resolve_tabpage(editor, tabpage)?;
    editor
        .tabpage(tabpage)
        .map(|tab| tab.current_window())
        .map_err(exception)
}

#[api(since = 12, method)]
pub fn nvim_tabpage_set_win(
    editor: &mut Editor,
    tabpage: TabHandle,
    win: WinHandle,
) -> Result<(), ApiError> {
    let tabpage = resolve_tabpage(editor, tabpage)?;
    let owner = editor.window_tabpage(win).map_err(exception)?;
    if owner != tabpage {
        return Err(ApiError::exception(format!(
            "Window does not belong to tabpage {}",
            i64::from(tabpage)
        )));
    }
    let original_tabpage = editor.current_tabpage();
    editor.set_current_window(win).map_err(exception)?;
    if let Some(original) = original_tabpage.filter(|current| *current != tabpage) {
        editor.set_current_tabpage(original).map_err(exception)?;
    }
    Ok(())
}

#[api(since = 1, method)]
pub fn nvim_tabpage_is_valid(
    editor: &mut Editor,
    tabpage: TabHandle,
) -> Result<bool, ApiError> {
    if tabpage.is_current() {
        return Ok(editor.current_tabpage().is_some());
    }
    Ok(editor.tabpage(tabpage).is_ok())
}

#[api(since = 1, method)]
pub fn nvim_tabpage_get_number(
    editor: &mut Editor,
    tabpage: TabHandle,
) -> Result<i64, ApiError> {
    let tabpage = resolve_tabpage(editor, tabpage)?;
    let index = editor
        .tabpages()
        .iter()
        .position(|candidate| *candidate == tabpage)
        .ok_or_else(|| ApiError::exception(format!("Invalid tabpage id: {}", i64::from(tabpage))))?;
    i64::try_from(index)
        .map(|number| number + 1)
        .map_err(|_| ApiError::exception("Tabpage number exceeds Integer range"))
}

#[api(since = 1, method)]
pub fn nvim_tabpage_get_var(
    editor: &mut Editor,
    tabpage: TabHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let tabpage = resolve_tabpage(editor, tabpage)?;
    editor
        .tabpage_variables(tabpage)
        .map_err(exception)?
        .get(&name)
        .cloned()
        .ok_or_else(|| ApiError::validation(format!("Key not found: {}", name.to_string_lossy())))
}

#[api(since = 1, method)]
pub fn nvim_tabpage_set_var(
    editor: &mut Editor,
    tabpage: TabHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let tabpage = resolve_tabpage(editor, tabpage)?;
    editor
        .tabpage_variables_mut(tabpage)
        .map_err(exception)?
        .insert(name, value);
    Ok(())
}

#[api(since = 1, method)]
pub fn nvim_tabpage_del_var(
    editor: &mut Editor,
    tabpage: TabHandle,
    name: OxStr,
) -> Result<(), ApiError> {
    let tabpage = resolve_tabpage(editor, tabpage)?;
    let variables = editor.tabpage_variables_mut(tabpage).map_err(exception)?;
    let Some(index) = variables.iter().position(|(key, _)| key == &name) else {
        return Err(ApiError::validation(format!(
            "Key not found: {}",
            name.to_string_lossy()
        )));
    };
    variables.0.remove(index);
    Ok(())
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_tabpage_list_wins__API_META(), nvim_tabpage_list_wins__API_DISPATCH)?;
    registry.register(nvim_tabpage_get_win__API_META(), nvim_tabpage_get_win__API_DISPATCH)?;
    registry.register(nvim_tabpage_set_win__API_META(), nvim_tabpage_set_win__API_DISPATCH)?;
    registry.register(nvim_tabpage_is_valid__API_META(), nvim_tabpage_is_valid__API_DISPATCH)?;
    registry.register(nvim_tabpage_get_number__API_META(), nvim_tabpage_get_number__API_DISPATCH)?;
    registry.register(nvim_tabpage_get_var__API_META(), nvim_tabpage_get_var__API_DISPATCH)?;
    registry.register(nvim_tabpage_set_var__API_META(), nvim_tabpage_set_var__API_DISPATCH)?;
    registry.register(nvim_tabpage_del_var__API_META(), nvim_tabpage_del_var__API_DISPATCH)?;
    Ok(())
}
