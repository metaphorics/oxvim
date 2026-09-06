//! Editor-global Neovim API functions.

use std::collections::HashSet;
use std::rc::Rc;

use ox_editor::{
    AutocmdContext, BufferRelease, Editor, EditorError, Event, FocusContainer, K_SPECIAL,
    KE_FILLER, KS_EXTRA, KS_SPECIAL, KS_ZERO, Keys, Message, MessageKind, OptionError,
    OptionListKind, OptionMetadata, OptionScope, OptionType, OptionValue, TypeaheadFlags,
    UserCommand, focus_transition,
};
use ox_excmd::ExCommand;
use ox_types::{Special, Typval};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::option_merge::SetOp;
use crate::runtime::{with_command_executor, with_lua_executor};
use crate::{
    ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError, TabHandle, WinHandle, api,
    session::ApiSession,
};

const MAX_CONVERSION_DEPTH: usize = 100;
const MAX_FUNC_ARGS: usize = 20;
const NVIM_MAX_PATH_LENGTH: usize = 4096;

/// Execution seam deliberately owned by the future Ex-command host.
pub trait CommandExecutor {
    /// Execute commands that have already passed `ox-excmd` parsing.
    ///
    /// # Errors
    ///
    /// Returns an API error when command execution fails.
    fn execute(&mut self, session: &ApiSession, commands: &[ExCommand]) -> Result<(), ApiError>;

    /// Executes one command line.
    ///
    /// # Errors
    ///
    /// Returns an API error when parsing or executing the command fails.
    fn execute_command(&mut self, session: &ApiSession, command: &str) -> Result<(), ApiError> {
        let commands = self.parse_cmdline(session, command)?;
        self.execute(session, &commands)
    }

    /// Executes Vimscript source text. Hosts with a script engine override
    /// this so continuations and function blocks share one script context.
    ///
    /// # Errors
    ///
    /// Returns an API error when parsing or executing the source fails.
    fn execute_script(&mut self, session: &ApiSession, source: &str) -> Result<(), ApiError> {
        let commands = self.parse_cmdline(session, source)?;
        self.execute(session, &commands)
    }

    /// Defines one user command, globally when `buffer` is None, else in that
    /// buffer's table. `force` replaces an existing definition.
    ///
    /// # Errors
    ///
    /// Returns an API error when the command definition is invalid or conflicts
    /// with an existing definition.
    fn define_user_command(
        &mut self,
        session: &ApiSession,
        buffer: Option<BufHandle>,
        command: UserCommand,
        force: bool,
    ) -> Result<(), ApiError>;

    /// Deletes one user command from the matching table.
    ///
    /// # Errors
    ///
    /// Returns an API error when the command table cannot be accessed or the
    /// named command cannot be deleted.
    fn delete_user_command(
        &mut self,
        session: &ApiSession,
        buffer: Option<BufHandle>,
        name: &str,
    ) -> Result<(), ApiError>;

    /// Lists one table: global commands when `buffer` is None, else that
    /// buffer's local commands, never merged.
    ///
    /// # Errors
    ///
    /// Returns an API error when the requested command table cannot be listed.
    fn list_user_commands(
        &mut self,
        session: &ApiSession,
        buffer: Option<BufHandle>,
    ) -> Result<Vec<UserCommand>, ApiError>;

    /// Parses a command line with the live registry, without executing it.
    ///
    /// # Errors
    ///
    /// Returns an API error when `line` is not a valid command line.
    fn parse_cmdline(
        &mut self,
        session: &ApiSession,
        line: &str,
    ) -> Result<Vec<ExCommand>, ApiError>;

    /// Drops one buffer's local commands; the `nvim_buf_delete` wipe hook.
    ///
    /// # Errors
    ///
    /// Returns an API error when the buffer's command table cannot be removed.
    fn remove_buffer(&mut self, buffer: BufHandle) -> Result<(), ApiError>;

    /// Evaluates one Vimscript expression.
    ///
    /// # Errors
    ///
    /// Returns an API error when parsing or evaluating the expression fails.
    fn evaluate(&mut self, session: &ApiSession, expression: &str) -> Result<Typval, ApiError>;

    /// Calls one Vimscript function through the live Ex evaluator, so
    /// builtins and user-defined functions share Vimscript semantics.
    ///
    /// # Errors
    ///
    /// Returns an API error when the host cannot dispatch the function or
    /// the function itself fails.
    fn call_builtin(
        &mut self,
        session: &ApiSession,
        name: &OxStr,
        args: Vec<Typval>,
    ) -> Result<Typval, ApiError>;

    /// Changes the process working directory globally.
    ///
    /// `path` is valid UTF-8. The host retains the previous directory for
    /// `:cd -`.
    ///
    /// # Errors
    ///
    /// Returns an API error when the directory transition fails.
    fn change_directory(&mut self, session: &ApiSession, path: &str) -> Result<(), ApiError>;

    /// Creates an independent host instance sharing the same underlying
    /// registries, for a reentrant call deeper than the primary/nested slot
    /// pair. Hosts whose state is shared `Rc` handles can cheaply clone
    /// themselves; the default reports no deeper host.
    fn fork(&self) -> Option<Box<dyn CommandExecutor>> {
        let _ = self;
        None
    }
}

/// Parse and execute a command through an explicitly supplied host.
///
/// # Errors
///
/// Returns an API error when `command` is not valid UTF-8, cannot be parsed, or
/// fails during execution.
pub fn execute_command(
    session: &ApiSession,
    command: &OxStr,
    executor: &mut dyn CommandExecutor,
) -> Result<(), ApiError> {
    let utf8 = std::str::from_utf8(command.as_bytes())
        .map_err(|_| ApiError::validation("Command must be valid UTF-8"))?;
    executor.execute_command(session, utf8)
}

fn finish_message_capture(
    session: &ApiSession,
    message_start: usize,
    result: Result<(), ApiError>,
) -> Result<OxStr, ApiError> {
    let captured = result.map(|()| {
        session.with_editor(|editor| {
            editor.messages()[message_start..]
                .iter()
                .filter(|message| message.kind == MessageKind::Echo)
                .filter_map(|message| match &message.content {
                    Object::String(text) => Some(text.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
    });
    session.with_editor_mut(|editor| editor.truncate_messages(message_start));
    captured.map(|captured| OxStr(captured.into_bytes()))
}

/// Decode and execute one structured Ex command through an explicit host.
///
/// `addr`, `nargs`, and `nextcmd` are accepted but ignored, matching
/// `nvim_cmd()` rather than `nvim_parse_cmd()`.
///
/// # Errors
///
/// Returns an API error when the command or options are invalid, or when
/// command execution fails.
pub fn execute_nvim_cmd(
    session: &ApiSession,
    cmd: &Dict,
    opts: &Dict,
    executor: &mut dyn CommandExecutor,
) -> Result<OxStr, ApiError> {
    let (command, output) = build_nvim_cmd(cmd, opts)?;
    let message_start = session.with_editor(|editor| editor.messages().len());
    let result = execute_command(session, &OxStr(command.into_bytes()), executor);
    if !output {
        result?;
        return Ok(OxStr::from(""));
    }
    finish_message_capture(session, message_start, result)
}

fn build_nvim_cmd(cmd: &Dict, opts: &Dict) -> Result<(String, bool), ApiError> {
    reject_keys(
        cmd,
        &[
            "cmd", "args", "bang", "count", "range", "reg", "mods", "magic", "addr", "nargs",
            "nextcmd",
        ],
    )?;
    reject_keys(opts, &["output"])?;

    let name = required_string(cmd, "cmd")?;
    let name = std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("'cmd' must be valid UTF-8"))?;
    if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(ApiError::validation(
            "'cmd' must be a non-empty command name",
        ));
    }

    let output = optional_bool(opts, "output")?.unwrap_or(false);
    let mut line = String::new();
    if let Some(mods) = optional_dict(cmd, "mods")? {
        append_modifiers(&mut line, mods)?;
    }
    if let Some(range) = field(cmd, "range") {
        append_range(&mut line, range)?;
    }
    line.push_str(name);
    if optional_bool(cmd, "bang")?.unwrap_or(false) {
        line.push('!');
    }
    if let Some(Object::Integer(count)) = field(cmd, "count") {
        if *count < 0 {
            return Err(ApiError::validation("'count' must be non-negative"));
        }
        line.push(' ');
        line.push_str(&count.to_string());
    } else if field(cmd, "count").is_some() {
        return Err(ApiError::validation("'count' must be an Integer"));
    }
    if let Some(reg) = optional_string(cmd, "reg")? {
        let reg = std::str::from_utf8(reg.as_bytes())
            .map_err(|_| ApiError::validation("'reg' must be valid UTF-8"))?;
        if reg.chars().count() != 1 || reg == "=" {
            return Err(ApiError::validation("'reg' must be one non-'=' character"));
        }
        line.push(' ');
        line.push('"');
        line.push_str(reg);
    }

    let magic_bar = optional_dict(cmd, "magic")?
        .map(|magic| {
            reject_keys(magic, &["file", "bar"])?;
            optional_bool(magic, "bar").map(|value| value.unwrap_or(true))
        })
        .transpose()?
        .unwrap_or(true);
    if let Some(args) = field(cmd, "args") {
        let Object::Array(args) = args else {
            return Err(ApiError::validation("'args' must be an Array"));
        };
        for arg in args {
            let text = match arg {
                Object::String(text) => {
                    let text = std::str::from_utf8(text.as_bytes()).map_err(|_| {
                        ApiError::validation("command arguments must be valid UTF-8")
                    })?;
                    if text.bytes().all(|byte| byte.is_ascii_whitespace()) {
                        return Err(ApiError::validation(
                            "command arguments must not be whitespace-only",
                        ));
                    }
                    text.to_owned()
                }
                Object::Integer(value) => value.to_string(),
                Object::Boolean(value) => i32::from(*value).to_string(),
                _ => {
                    return Err(ApiError::validation(
                        "command arguments must be Strings, Integers, or Booleans",
                    ));
                }
            };
            line.push(' ');
            if magic_bar {
                line.push_str(&text);
            } else {
                line.push_str(&text.replace('|', "\\|"));
            }
        }
    }
    Ok((line, output))
}

fn append_range(line: &mut String, range: &Object) -> Result<(), ApiError> {
    let Object::Array(range) = range else {
        return Err(ApiError::validation("'range' must be an Array"));
    };
    if range.len() > 2 {
        return Err(ApiError::validation(
            "'range' must contain at most two elements",
        ));
    }
    for (index, value) in range.iter().enumerate() {
        let Object::Integer(value) = value else {
            return Err(ApiError::validation("range elements must be Integers"));
        };
        if *value < 0 {
            return Err(ApiError::validation("range elements must be non-negative"));
        }
        if index != 0 {
            line.push(',');
        }
        line.push_str(&value.to_string());
    }
    Ok(())
}

fn append_modifiers(line: &mut String, mods: &Dict) -> Result<(), ApiError> {
    const FLAGS: &[(&str, &str)] = &[
        ("silent", "silent"),
        ("emsg_silent", "silent!"),
        ("unsilent", "unsilent"),
        ("sandbox", "sandbox"),
        ("noautocmd", "noautocmd"),
        ("browse", "browse"),
        ("confirm", "confirm"),
        ("hide", "hide"),
        ("keepalt", "keepalt"),
        ("keepjumps", "keepjumps"),
        ("keepmarks", "keepmarks"),
        ("keeppatterns", "keeppatterns"),
        ("lockmarks", "lockmarks"),
        ("noswapfile", "noswapfile"),
    ];
    reject_keys(
        mods,
        &[
            "silent",
            "emsg_silent",
            "unsilent",
            "sandbox",
            "noautocmd",
            "browse",
            "confirm",
            "hide",
            "keepalt",
            "keepjumps",
            "keepmarks",
            "keeppatterns",
            "lockmarks",
            "noswapfile",
            "tab",
            "verbose",
            "vertical",
            "horizontal",
            "split",
            "filter",
        ],
    )?;
    append_count_modifier(line, mods, "tab")?;
    append_count_modifier(line, mods, "verbose")?;
    if let Some(split) = optional_string(mods, "split")? {
        let split = std::str::from_utf8(split.as_bytes())
            .map_err(|_| ApiError::validation("'mods.split' must be valid UTF-8"))?;
        if !split.is_empty() {
            let canonical = match split {
                "aboveleft" | "leftabove" => "aboveleft",
                "belowright" | "rightbelow" => "belowright",
                "topleft" => "topleft",
                "botright" => "botright",
                _ => return Err(ApiError::validation("invalid 'mods.split' value")),
            };
            line.push_str(canonical);
            line.push(' ');
        }
    }
    if let Some(filter) = optional_dict(mods, "filter")? {
        let force = optional_bool(filter, "force")?.unwrap_or(false);
        let pattern = optional_string(filter, "pattern")?;
        line.push_str("filter");
        if force {
            line.push('!');
        }
        if let Some(pattern) = pattern {
            let pattern = std::str::from_utf8(pattern.as_bytes())
                .map_err(|_| ApiError::validation("'mods.filter.pattern' must be valid UTF-8"))?;
            if !pattern.is_empty() {
                line.push(' ');
                line.push_str(pattern);
            }
        }
        line.push(' ');
    }
    for (key, spelling) in [("vertical", "vertical"), ("horizontal", "horizontal")] {
        if optional_bool(mods, key)?.unwrap_or(false) {
            line.push_str(spelling);
            line.push(' ');
        }
    }
    for (key, spelling) in FLAGS {
        if optional_bool(mods, key)?.unwrap_or(false) {
            line.push_str(spelling);
            line.push(' ');
        }
    }
    Ok(())
}

fn append_count_modifier(line: &mut String, mods: &Dict, key: &str) -> Result<(), ApiError> {
    let Some(value) = field(mods, key) else {
        return Ok(());
    };
    let Object::Integer(value) = value else {
        return Err(ApiError::validation(format!(
            "'mods.{key}' must be an Integer"
        )));
    };
    if *value >= 0 {
        line.push_str(&value.to_string());
        line.push_str(key);
        line.push(' ');
    }
    Ok(())
}

fn field<'a>(dict: &'a Dict, name: &str) -> Option<&'a Object> {
    dict.iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .map(|(_, value)| value)
}

fn required_string<'a>(dict: &'a Dict, name: &str) -> Result<&'a OxStr, ApiError> {
    optional_string(dict, name)?
        .ok_or_else(|| ApiError::validation(format!("'{name}' is required")))
}

fn optional_string<'a>(dict: &'a Dict, name: &str) -> Result<Option<&'a OxStr>, ApiError> {
    match field(dict, name) {
        None => Ok(None),
        Some(Object::String(value)) => Ok(Some(value)),
        Some(_) => Err(ApiError::validation(format!("'{name}' must be a String"))),
    }
}

fn optional_dict<'a>(dict: &'a Dict, name: &str) -> Result<Option<&'a Dict>, ApiError> {
    match field(dict, name) {
        None => Ok(None),
        Some(Object::Dict(value)) => Ok(Some(value)),
        Some(Object::Array(value)) if value.is_empty() => Ok(None),
        Some(_) => Err(ApiError::validation(format!("'{name}' must be a Dict"))),
    }
}

pub(crate) fn optional_bool(dict: &Dict, name: &str) -> Result<Option<bool>, ApiError> {
    match field(dict, name) {
        None => Ok(None),
        Some(Object::Boolean(value)) => Ok(Some(*value)),
        Some(Object::Integer(value)) => Ok(Some(*value != 0)),
        Some(_) => Err(ApiError::validation(format!(
            "Invalid '{name}': not a boolean"
        ))),
    }
}

pub(crate) fn reject_keys(dict: &Dict, allowed: &[&str]) -> Result<(), ApiError> {
    for (key, _) in dict.iter() {
        if !allowed
            .iter()
            .any(|allowed| key.as_bytes() == allowed.as_bytes())
        {
            return Err(ApiError::validation(format!(
                "Invalid key: {}",
                key.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn exception(error: impl std::fmt::Display) -> ApiError {
    ApiError::exception(error.to_string())
}

fn current_handle_error(error: EditorError) -> ApiError {
    // Upstream resolves handles through `find_window_by_handle` /
    // `find_tab_by_handle` (api/private/helpers.c:278-298), whose
    // `VALIDATE_INT` failure is a validation-type error
    // (api/private/validate.c:12-19) - the wire type clients branch on.
    match error {
        EditorError::UnknownWindow(window) => {
            ApiError::validation(format!("Invalid window id: {}", i64::from(window)))
        }
        EditorError::UnknownTabpage(tabpage) => {
            ApiError::validation(format!("Invalid tabpage id: {}", i64::from(tabpage)))
        }
        error => exception(error),
    }
}

fn option_value_error(error: OptionError) -> ApiError {
    match error {
        OptionError::UnknownOption(name) => {
            ApiError::validation(format!("Unknown option '{name}'"))
        }
        error => exception(error),
    }
}

fn current_buffer(session: &ApiSession) -> Result<BufHandle, ApiError> {
    session
        .with_editor(Editor::current_buffer)
        .ok_or_else(|| ApiError::exception("No current buffer"))
}

fn current_window(session: &ApiSession) -> Result<WinHandle, ApiError> {
    session
        .with_editor(Editor::current_window)
        .ok_or_else(|| ApiError::exception("No current window"))
}

fn current_tabpage(session: &ApiSession) -> Result<TabHandle, ApiError> {
    session
        .with_editor(Editor::current_tabpage)
        .ok_or_else(|| ApiError::exception("No current tabpage"))
}

#[api(since = 1)]
pub fn nvim_get_current_buf(session: &ApiSession) -> Result<BufHandle, ApiError> {
    current_buffer(session)
}

#[api(since = 1, textlock)]
pub fn nvim_set_current_buf(session: &ApiSession, buf: BufHandle) -> Result<(), ApiError> {
    // Upstream routes through `do_buffer` (`api/vim.c` `nvim_set_current_buf`),
    // so the switch fires the buffer lifecycle: `BufLeave` on the old buffer
    // (`set_curbuf`, buffer.c:1735), then `BufEnter` and `BufWinEnter` on the
    // entered one (`enter_buffer`, buffer.c:1850-1851). The current buffer is
    // "nothing to do" before any check or event (`buffer.c:1657-1659`).
    // Unlike upstream's `switch_to_buf_curwin` probe, the port has no
    // window-search model, so the buffer sequence always runs in the current
    // window — the port-faithful reading of the same switch.
    //
    // `find_buffer_by_handle` resolves the 0 sentinel to the current buffer
    // (api/private/helpers.c:265-267), so set_current_buf(0) is the no-op
    // the resolved comparison below returns.
    let buf = if buf.is_current() {
        let Some(current) = session.with_editor(Editor::current_buffer) else {
            return Ok(());
        };
        current
    } else {
        buf
    };
    let old = session.with_editor(Editor::current_buffer);
    if old == Some(buf) {
        return Ok(());
    }
    // The handle resolves before anything else: `find_buffer_by_handle`
    // (`api/vim.c:967`) fails the whole call through `VALIDATE_INT` - a
    // validation-type `Invalid buffer id` (api/private/validate.c:12-19) -
    // when the target never existed, with no events fired. E86 is
    // `do_buffer`'s later error for buffer numbers, not the API's
    // invalid-handle report.
    if session.with_editor(|editor| editor.buffer(buf).is_err()) {
        return Err(ApiError::validation(format!(
            "Invalid buffer id: {}",
            i64::from(buf)
        )));
    }
    let transition = focus_transition(old, buf, FocusContainer::Buffer);
    fire_focus_events(session, &transition.leaves, old)?;
    // `set_curbuf` skips the entry when a handler invalidated the target
    // (`buffer.c:1790-1794`).
    if session.with_editor(|editor| editor.buffer(buf).is_err()) {
        return Ok(());
    }
    session
        .with_editor_mut(|editor| editor.set_current_buffer(buf, BufferRelease::KeepLoaded))
        .map_err(exception)?;
    fire_focus_events(session, &transition.enters, Some(buf))
}

#[api(since = 1)]
pub fn nvim_get_current_win(session: &ApiSession) -> Result<WinHandle, ApiError> {
    current_window(session)
}

#[api(since = 1, textlock)]
pub fn nvim_set_current_win(session: &ApiSession, win: WinHandle) -> Result<(), ApiError> {
    // Upstream routes through `goto_tabpage_win` (`api/vim.c:1024`,
    // `window.c:4953`): the window's tabpage is entered first, then the
    // window itself; both steps are silent when they change nothing.
    // `find_window_by_handle` resolves the 0 sentinel to the current window
    // (api/private/helpers.c:280-282), which the two-step then treats as
    // the no-op it is.
    let win = if win.is_current() {
        let Some(current) = session.with_editor(Editor::current_window) else {
            return Ok(());
        };
        current
    } else {
        win
    };
    let owner = session.with_editor(|editor| {
        editor.tabpages().into_iter().find(|tab| {
            editor
                .tabpage_windows(*tab)
                .is_ok_and(|windows| windows.contains(&win))
        })
    });
    let current = session.with_editor(Editor::current_tabpage);
    if owner.is_none() {
        return Err(current_handle_error(EditorError::UnknownWindow(win)));
    }
    if current != owner
        && let Some(owner) = owner
    {
        enter_tabpage(session, owner)?;
    }
    enter_window(session, win)
}

#[api(since = 1)]
pub fn nvim_get_current_tabpage(session: &ApiSession) -> Result<TabHandle, ApiError> {
    current_tabpage(session)
}

#[api(since = 1, textlock)]
pub fn nvim_set_current_tabpage(session: &ApiSession, tabpage: TabHandle) -> Result<(), ApiError> {
    // `find_tab_by_handle` resolves the 0 sentinel to the current tabpage
    // (api/private/helpers.c:292-294); the resolved comparison inside makes
    // that the documented no-op.
    let tabpage = if tabpage.is_current() {
        let Some(current) = session.with_editor(Editor::current_tabpage) else {
            return Ok(());
        };
        current
    } else {
        tabpage
    };
    enter_tabpage(session, tabpage)
}

/// Fires the leave sequence, performs the tabpage switch, and fires the
/// enter sequence (`goto_tabpage_tp`, window.c:4920: the current tabpage is
/// a no-op, `leave_tabpage` window.c:4727, `enter_tabpage` window.c:4767).
/// A failing leave handler aborts before the switch.
fn enter_tabpage(session: &ApiSession, target: TabHandle) -> Result<(), ApiError> {
    if session.with_editor(Editor::current_tabpage) == Some(target) {
        return Ok(());
    }
    let (old, new) = session.with_editor(|editor| {
        let new = editor
            .tabpage(target)
            .ok()
            .and_then(|tab| editor.window(tab.current_window()).ok())
            .map(|window| window.buffer);
        (editor.current_buffer(), new)
    });
    let Some(new) = new else {
        return session
            .with_editor_mut(|editor| editor.set_current_tabpage(target))
            .map_err(current_handle_error);
    };
    let leave_transition = focus_transition(old, new, FocusContainer::Tab);
    fire_focus_events(session, &leave_transition.leaves, old)?;
    // `goto_tabpage_tp` only enters a still-valid tabpage
    // (`window.c:4931-4936`); a handler that closed the target ends the
    // switch without enter events or an error.
    if session.with_editor(|editor| editor.tabpage(target).is_err()) {
        return Ok(());
    }
    session
        .with_editor_mut(|editor| editor.set_current_tabpage(target))
        .map_err(current_handle_error)?;
    // `enter_tabpage` binds its events to the window that is current AFTER
    // the switch (`window.c:4767` reads the tab's `tp_curwin` post-switch);
    // a leave handler may have closed the snapshot window or promoted
    // another, so both the enter-event set and its binding come from the
    // state actually entered - recomputed from `old`, because a promoted
    // window can change whether BufEnter belongs in the sequence at all -
    // not from the pre-handler snapshot.
    let entered = session.with_editor(|editor| {
        editor
            .tabpage(target)
            .ok()
            .and_then(|tab| editor.window(tab.current_window()).ok())
            .map(|window| window.buffer)
    });
    if let Some(entered) = entered {
        let enter_transition = focus_transition(old, entered, FocusContainer::Tab);
        fire_focus_events(session, &enter_transition.enters, Some(entered))
    } else {
        Ok(())
    }
}

/// Fires the leave sequence, performs the window switch, and fires the enter
/// sequence (`win_enter_ext`, window.c:5243: the current window is a no-op,
/// the leave events fire before the switch at window.c:5259 and 5265, the
/// enter events after it at window.c:5317 and 5319). A failing leave handler
/// aborts before the switch.
fn enter_window(session: &ApiSession, target: WinHandle) -> Result<(), ApiError> {
    let (current, old, new) = session.with_editor(|editor| {
        let new = editor.window(target).ok().map(|window| window.buffer);
        (editor.current_window(), editor.current_buffer(), new)
    });
    if current == Some(target) {
        return Ok(());
    }
    let Some(new) = new else {
        return session
            .with_editor_mut(|editor| editor.set_current_window(target))
            .map_err(current_handle_error);
    };
    let leave_transition = focus_transition(old, new, FocusContainer::Window);
    fire_focus_events(session, &leave_transition.leaves, old)?;
    // `goto_tabpage_win` enters only a still-valid window
    // (`window.c:4956`); a handler that closed the target ends the switch
    // without enter events or an error.
    if session.with_editor(|editor| editor.window(target).is_err()) {
        return Ok(());
    }
    session
        .with_editor_mut(|editor| editor.set_current_window(target))
        .map_err(current_handle_error)?;
    // `win_enter_ext` fires the enters with `curwin`/`curbuf` already
    // switched (window.c:5304-5319); a leave handler may have changed the
    // target window's buffer, so the enter set and binding come from the
    // buffer live after the switch.
    let entered =
        session.with_editor(|editor| editor.window(target).ok().map(|window| window.buffer));
    if let Some(entered) = entered {
        let enter_transition = focus_transition(old, entered, FocusContainer::Window);
        fire_focus_events(session, &enter_transition.enters, Some(entered))
    } else {
        Ok(())
    }
}

/// Fires `events` for one half of a focus transition, each bound to `buffer`
/// as `<abuf>` and to the buffer's name as `<afile>`/`<amatch>`, through the
/// shared planner and the api firing executor.
fn fire_focus_events(
    session: &ApiSession,
    events: &[Event],
    buffer: Option<BufHandle>,
) -> Result<(), ApiError> {
    let Some(buffer) = buffer else {
        return Ok(());
    };
    if events.is_empty() {
        return Ok(());
    }
    let name = session
        .with_editor(|editor| {
            editor
                .buffer(buffer)
                .ok()
                .map(|state| state.name().to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    for &event in events {
        let plan = session.with_editor_mut(|editor| {
            editor.autocmds_mut().plan(
                event,
                AutocmdContext {
                    buffer: Some(buffer),
                    file_name: Some(&name),
                    ..AutocmdContext::default()
                },
            )
        });
        crate::autocmd::execute_firing_plan(session, plan)?;
    }
    Ok(())
}

#[api(since = 1)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_set_current_dir(session: &ApiSession, dir: OxStr) -> Result<(), ApiError> {
    if dir.as_bytes().len() >= NVIM_MAX_PATH_LENGTH {
        return Err(ApiError::validation("Invalid directory name: '(too long)'"));
    }
    let path = std::str::from_utf8(dir.as_bytes())
        .map_err(|_| ApiError::validation("Directory must be valid UTF-8"))?;
    with_command_executor(session, |session, executor| {
        executor.change_directory(session, path)
    })
}

#[api(since = 1)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "`#[api]` handlers must return `Result` for the generated dispatcher"
)]
pub fn nvim_list_bufs(session: &ApiSession) -> Result<Vec<BufHandle>, ApiError> {
    Ok(session.with_editor(Editor::buffers))
}

#[api(since = 1)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "`#[api]` handlers must return `Result` for the generated dispatcher"
)]
pub fn nvim_list_wins(session: &ApiSession) -> Result<Vec<WinHandle>, ApiError> {
    Ok(session.with_editor(Editor::windows))
}

#[api(since = 1)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "`#[api]` handlers must return `Result` for the generated dispatcher"
)]
pub fn nvim_list_tabpages(session: &ApiSession) -> Result<Vec<TabHandle>, ApiError> {
    Ok(session.with_editor(Editor::tabpages))
}

#[api(since = 1, fast)]
pub fn nvim_get_api_info(_session: &ApiSession) -> Result<Vec<Object>, ApiError> {
    let metadata = ox_rpc::canonical_metadata()
        .map_err(|error| ApiError::exception(format!("invalid API metadata: {error}")))?;
    Ok(vec![Object::Integer(0), metadata])
}

#[api(since = 1)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_command(session: &ApiSession, command: OxStr) -> Result<(), ApiError> {
    with_command_executor(session, |session, executor| {
        execute_command(session, &command, executor)
    })
}

/// Runs one command through the installed host and captures its echo messages.
pub(crate) fn command_output(session: &ApiSession, command: &OxStr) -> Result<OxStr, ApiError> {
    let message_start = session.with_editor(|editor| editor.messages().len());
    with_command_executor(session, |session, executor| {
        let result = execute_command(session, command, executor);
        finish_message_capture(session, message_start, result)
    })
}

/// api/vim.c `nvim_exec2`: run Vimscript, returning `{ output = ... }` only
/// when the caller asked for it.
#[api(since = 11)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_exec2(session: &ApiSession, src: OxStr, opts: Dict) -> Result<Dict, ApiError> {
    reject_keys(&opts, &["output"])?;
    let output = optional_bool(&opts, "output")?.unwrap_or(false);
    let captured = exec_capturing(session, &src, output)?;
    Ok(Dict(if output {
        vec![(OxStr::from("output"), Object::String(captured))]
    } else {
        Vec::new()
    }))
}

/// api/vim.c `nvim_cmd`: the structured command form, which decodes to a
/// command line and runs through the same host.
#[api(since = 10)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_cmd(session: &ApiSession, cmd: Dict, opts: Dict) -> Result<OxStr, ApiError> {
    with_command_executor(session, |session, executor| {
        execute_nvim_cmd(session, &cmd, &opts, executor)
    })
}

fn exec_lua_error(message: &str) -> ApiError {
    let message = message
        .split_once("[string \"<nvim>\"]:")
        .and_then(|(_, rest)| rest.split_once(": ").map(|(_, detail)| detail))
        .unwrap_or(message);
    ApiError::exception(message)
}

/// api/vim.c `nvim_exec_lua`: `luaeval`-style execution of one chunk with
/// `args` bound to `...`.
#[api(since = 7)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_exec_lua(
    session: &ApiSession,
    code: OxStr,
    args: Vec<Object>,
) -> Result<Object, ApiError> {
    let code = std::str::from_utf8(code.as_bytes())
        .map_err(|_| ApiError::validation("Lua chunk must be valid UTF-8"))?
        .to_owned();
    with_lua_executor(session, |session, executor| {
        executor
            .exec(session, &code, args)
            .map_err(|message| exec_lua_error(&message))
    })
}

/// Runs one script through the command host, collecting the `:echo` messages
/// it produced when `capture` is set, the way `nvim_exec2`'s `output` option
/// and `nvim_cmd`'s already do.
fn exec_capturing(session: &ApiSession, src: &OxStr, capture: bool) -> Result<OxStr, ApiError> {
    let source = std::str::from_utf8(src.as_bytes())
        .map_err(|_| ApiError::validation("Command must be valid UTF-8"))?;
    let message_start = session.with_editor(|editor| editor.messages().len());
    with_command_executor(session, |session, executor| {
        let result = executor.execute_script(session, source);
        if !capture {
            result?;
            return Ok(OxStr::from(""));
        }
        finish_message_capture(session, message_start, result)
    })
}

#[api(since = 1)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_eval(session: &ApiSession, expr: OxStr) -> Result<Object, ApiError> {
    let expression = std::str::from_utf8(expr.as_bytes())
        .map_err(|_| ApiError::validation("Expression must be valid UTF-8"))?;
    with_command_executor(session, |session, executor| {
        executor
            .evaluate(session, expression)
            .and_then(|value| typval_to_object(&value, 0))
    })
}

#[api(since = 1)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_call_function(
    session: &ApiSession,
    fn_name: OxStr,
    args: Vec<Object>,
) -> Result<Object, ApiError> {
    if args.len() > MAX_FUNC_ARGS {
        return Err(ApiError::validation(
            "Function called with too many arguments",
        ));
    }
    let args = args
        .iter()
        .map(|argument| object_to_typval(argument, 0))
        .collect::<Result<Vec<_>, _>>()?;
    with_command_executor(session, |session, executor| {
        executor
            .call_builtin(session, &fn_name, args)
            .and_then(|value| typval_to_object(&value, 0))
    })
}

#[api(since = 1)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_get_vvar(session: &ApiSession, name: OxStr) -> Result<Object, ApiError> {
    session
        .with_editor(|editor| editor.vvars().get(&name).cloned())
        .ok_or_else(|| ApiError::validation(format!("Key not found: {}", name.to_string_lossy())))
}

#[api(since = 6)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "`#[api]` handlers must return `Result` for the generated dispatcher"
)]
pub fn nvim_set_vvar(session: &ApiSession, name: OxStr, value: Object) -> Result<(), ApiError> {
    session.with_editor_mut(|editor| {
        editor.vvars_mut().insert(name, value);
    });
    Ok(())
}

#[api(since = 1)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_get_var(session: &ApiSession, name: OxStr) -> Result<Object, ApiError> {
    session
        .with_editor(|editor| editor.gvars().get(&name).cloned())
        .ok_or_else(|| ApiError::validation(format!("Key not found: {}", name.to_string_lossy())))
}

#[api(since = 1)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "`#[api]` handlers must return `Result` for the generated dispatcher"
)]
pub fn nvim_set_var(session: &ApiSession, name: OxStr, value: Object) -> Result<(), ApiError> {
    session.with_editor_mut(|editor| {
        editor.gvars_mut().insert(name, value);
    });
    Ok(())
}

#[api(since = 1)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_del_var(session: &ApiSession, name: OxStr) -> Result<(), ApiError> {
    session.with_editor_mut(|editor| {
        let variables = editor.gvars_mut();
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

#[api(since = 1, deprecated_since = 11)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_get_option(session: &ApiSession, name: OxStr) -> Result<Object, ApiError> {
    let name = option_name(&name)?;
    session
        .with_editor(|editor| {
            editor
                .options()
                .get_global(name)
                .map(option_value_to_object)
        })
        .map_err(exception)
}

#[api(since = 1, deprecated_since = 11)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_set_option(session: &ApiSession, name: OxStr, value: Object) -> Result<(), ApiError> {
    let name = option_name(&name)?;
    let metadata = ox_editor::OptionStore::metadata(name).map_err(exception)?;
    let value = object_to_legacy_option_value(metadata, name, value)?;
    session
        .with_editor_mut(|editor| editor.options_mut().set_global(name, value))
        .map_err(exception)
}

#[api(since = 9)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_get_option_value(
    session: &ApiSession,
    name: OxStr,
    opts: Dict,
) -> Result<Object, ApiError> {
    if opts.get(&OxStr::from("dry_run")).is_some() || opts.get(&OxStr::from("operation")).is_some()
    {
        return Err(ApiError::validation(
            "Invalid key for nvim_get_option_value",
        ));
    }
    let name = option_name(&name)?;
    let target = option_target(session, name, &opts)?;
    get_option_at(session, name, target)
}

#[api(since = 9)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_set_option_value(
    session: &ApiSession,
    name: OxStr,
    value: Object,
    opts: Dict,
) -> Result<Object, ApiError> {
    let name = option_name(&name)?;
    let metadata = ox_editor::OptionStore::metadata(name).map_err(option_value_error)?;
    let operation = match dict_string(&opts, "operation")? {
        Some(text) => SetOp::parse(text.as_bytes())?,
        None => SetOp::Set,
    };
    operation.check_supported(metadata)?;
    let target = option_target(session, name, &opts)?;
    let value = object_to_option_value(metadata, name, value)?;
    let value = match value {
        // option.c stropt_get_newval expands `$VAR` and `~` before merging.
        OptionValue::String(text) => OptionValue::String(crate::option_merge::expand_value(
            metadata, operation, &text,
        )),
        other => other,
    };
    let value = crate::option_merge::merge(
        metadata,
        &current_option_value(session, name, target)?,
        value,
        operation,
    )?;
    if dict_bool(&opts, "dry_run")? == Some(true) {
        validate_option_at(session, name, &value, target)?;
        return Ok(structured_option_value(metadata, &value));
    }
    let assigned = structured_option_value(metadata, &value);
    set_option_at(session, name, value, target)?;
    if metadata.name == "modified"
        && let OptionTarget::Buffer(buffer) | OptionTarget::GlobalAndBuffer(buffer) = target
        && let Object::Boolean(modified) = &assigned
    {
        session.with_editor_mut(|editor| {
            editor
                .buffer_mut(buffer)
                .map(|state| {
                    if *modified {
                        state.mark_modified();
                    } else {
                        state.mark_saved();
                    }
                })
                .map_err(|error| ApiError::validation(error.to_string()))
        })?;
    }
    // A committed buffer-local 'filetype' (canonical `filetype`, alias `ft`)
    // fires FileType once, after the value is stored; dry-run, merge errors,
    // and global- or window-scoped requests never reach this point.
    if metadata.name == "filetype"
        && let OptionTarget::Buffer(buffer) | OptionTarget::GlobalAndBuffer(buffer) = target
        && let Object::String(raw) = &assigned
    {
        crate::autocmd::fire_filetype(session, buffer, &raw.to_string_lossy())?;
    }
    Ok(assigned)
}

#[api(since = 1, fast)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_input(session: &ApiSession, keys: OxStr) -> Result<i64, ApiError> {
    let count = i64::try_from(keys.as_bytes().len())
        .map_err(|_| ApiError::exception("Input length exceeds Integer range"))?;
    let encoded = Keys::encode(keys.as_bytes());
    session.with_editor_mut(|editor| {
        editor
            .typeahead_mut()
            .append(&encoded, TypeaheadFlags::default());
    });
    Ok(count)
}

/// Sends a mouse event from a GUI (upstream `nvim_input_mouse`,
/// api/vim.c:406-476: button/action/modifier validation with the single
/// validation message, then a non-blocking enqueue). The port's input path
/// is typeahead keys, so the event enqueues as the equivalent key sequence
/// with the grid recorded in the modifier-free position suffix; multigrid
/// positioning is a UI-layer feature the port does not wire yet, and `grid`
/// is validated but not mapped.
#[api(since = 6, fast)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "`#[api]` requires owned arguments and a `Result` return"
)]
pub fn nvim_input_mouse(
    session: &ApiSession,
    button: OxStr,
    action: OxStr,
    modifier: OxStr,
    grid: i64,
    row: i64,
    col: i64,
) -> Result<(), ApiError> {
    if !(row >= 0 && col >= 0 && grid >= 0) {
        return Err(ApiError::validation("invalid button or action"));
    }
    let button = match button.as_bytes() {
        b"left" => "Left",
        b"middle" => "Middle",
        b"right" => "Right",
        b"wheel" => "ScrollWheel",
        b"x1" => "X1",
        b"x2" => "X2",
        b"move" => "Move",
        _ => return Err(ApiError::validation("invalid button or action")),
    };
    let suffix = if button == "ScrollWheel" {
        match action.as_bytes() {
            b"up" => "Up",
            b"down" => "Down",
            b"left" => "Left",
            b"right" => "Right",
            _ => return Err(ApiError::validation("invalid button or action")),
        }
    } else if button == "Move" {
        // `move` ignores its action, matching upstream's doc note.
        "Mouse"
    } else {
        match action.as_bytes() {
            b"press" => "Mouse",
            b"drag" => "Drag",
            b"release" => "Release",
            _ => return Err(ApiError::validation("invalid button or action")),
        }
    };
    // Modifier chars accept the optional '-' separators of key notation
    // (upstream parses "C-A-", "c-a", "CA" alike for the mask).
    let mut prefix = String::new();
    for byte in modifier.as_bytes() {
        if *byte == b'-' {
            continue;
        }
        prefix.push(char::from(byte.to_ascii_uppercase()));
    }
    let sequence = format!("<{prefix}{button}{suffix}><{row},{col}>");
    let encoded = Keys::encode(sequence.as_bytes());
    session.with_editor_mut(|editor| {
        editor
            .typeahead_mut()
            .append(&encoded, TypeaheadFlags::default());
    });
    Ok(())
}

#[api(since = 1, fast)]
#[expect(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    reason = "`#[api]` requires owned arguments and a `Result` return"
)]
pub fn nvim_replace_termcodes(
    _session: &ApiSession,
    str: OxStr,
    from_part: bool,
    do_lt: bool,
    special: bool,
) -> Result<OxStr, ApiError> {
    // `from_part` exists for upstream API parity; this translator ignores it.
    let _ = from_part;
    let replaced = replace_termcode_notation(str.as_bytes(), do_lt, special);
    Ok(OxStr::from(replaced.as_slice()))
}

#[api(since = 1)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_strwidth(session: &ApiSession, text: OxStr) -> Result<i64, ApiError> {
    let bytes = text.as_bytes();

    // Upstream VALIDATE_S: "text length" "(too long)" — check the full raw
    // length before NUL truncation (api/vim.c nvim_strwidth → mb_string2cells).
    if bytes.len() > i32::MAX as usize {
        return Err(ApiError::validation("Invalid text length: '(too long)'"));
    }

    // mb_string2cells stops at the first NUL; only the visible prefix is
    // measured and only it needs to be valid UTF-8.
    let visible = match bytes.iter().position(|&b| b == 0) {
        Some(pos) => &bytes[..pos],
        None => bytes,
    };
    let visible = std::str::from_utf8(visible)
        .map_err(|_| ApiError::validation("text must be valid UTF-8"))?;

    // 'ambiwidth' selects the East Asian Width table: "double" uses the CJK
    // variant (unicode-width 0.2 width_cjk), everything else uses width.
    let ambiguous_wide = session.with_editor(|editor| {
        matches!(
            editor.options().get_global("ambiwidth"),
            Ok(OptionValue::String(value)) if value == "double"
        )
    });
    let width = if ambiguous_wide {
        UnicodeWidthStr::width_cjk(visible)
    } else {
        UnicodeWidthStr::width(visible)
    };

    // Source: unicode-width 0.2 implements terminal columns from Unicode
    // Standard Annex #11.
    i64::try_from(width).map_err(|_| ApiError::exception("Text width exceeds Integer range"))
}

#[api(since = 1, deprecated_since = 13)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "`#[api]` handlers must return `Result` for the generated dispatcher"
)]
pub fn nvim_err_writeln(session: &ApiSession, str: OxStr) -> Result<(), ApiError> {
    session.with_editor_mut(|editor| {
        editor.push_message(Message {
            kind: MessageKind::Error,
            content: Object::String(str),
            history: true,
            leading_newline: true,
        });
    });
    Ok(())
}

#[api(since = 7)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_echo(
    session: &ApiSession,
    chunks: Vec<Object>,
    history: bool,
    opts: Dict,
) -> Result<Object, ApiError> {
    validate_echo_chunks(&chunks)?;
    if let Some((key, _)) = opts.iter().find(|(key, _)| key.as_bytes() != b"err") {
        return Err(ApiError::validation(format!(
            "Echo option '{}' is unavailable",
            key.to_string_lossy()
        )));
    }
    let kind = if dict_bool(&opts, "err")? == Some(true) {
        MessageKind::Error
    } else {
        MessageKind::Echo
    };
    session.with_editor_mut(|editor| {
        editor.push_message(Message {
            kind,
            content: Object::Array(chunks),
            history,
            leading_newline: true,
        });
    });
    Ok(Object::Integer(-1))
}

#[derive(Clone, Copy)]
enum OptionTarget {
    Global,
    Buffer(BufHandle),
    Window(WinHandle),
    GlobalAndBuffer(BufHandle),
    GlobalAndWindow(WinHandle),
}

fn option_target(session: &ApiSession, name: &str, opts: &Dict) -> Result<OptionTarget, ApiError> {
    reject_unknown_option_keys(opts)?;
    if opts.get(&OxStr::from("filetype")).is_some() || opts.get(&OxStr::from("tab")).is_some() {
        return Err(ApiError::validation(
            "filetype/tab option context is unavailable",
        ));
    }
    let buffer = dict_handle(opts, "buf", BufHandle::try_from)?;
    let window = dict_handle(opts, "win", WinHandle::try_from)?;
    if buffer.is_some() && window.is_some() {
        return Err(ApiError::validation(
            "opts.buf and opts.win are mutually exclusive",
        ));
    }
    let scope = dict_string(opts, "scope")?;
    if let Some(scope) = &scope
        && scope.as_bytes() != b"global"
        && scope.as_bytes() != b"local"
    {
        return Err(ApiError::validation(
            "opts.scope must be 'global' or 'local'",
        ));
    }
    if scope
        .as_ref()
        .is_some_and(|scope| scope.as_bytes() == b"global")
    {
        if buffer.is_some() || window.is_some() {
            return Err(ApiError::validation(
                "global scope cannot be combined with opts.buf or opts.win",
            ));
        }
        return Ok(OptionTarget::Global);
    }
    if let Some(buffer) = buffer {
        return Ok(OptionTarget::Buffer(resolve_buffer(session, buffer)?));
    }
    if let Some(window) = window {
        return Ok(OptionTarget::Window(resolve_window(session, window)?));
    }
    let metadata = ox_editor::OptionStore::metadata(name).map_err(option_value_error)?;
    let has_window = metadata.scopes.contains(&OptionScope::Window);
    let has_buffer = metadata.scopes.contains(&OptionScope::Buffer);
    if scope.is_some() {
        if has_window {
            return Ok(OptionTarget::Window(current_window(session)?));
        }
        if has_buffer {
            return Ok(OptionTarget::Buffer(current_buffer(session)?));
        }
        return Err(ApiError::validation(format!(
            "Option '{name}' has no local value"
        )));
    }
    if metadata.scopes.contains(&OptionScope::Global) && has_window {
        return Ok(OptionTarget::GlobalAndWindow(current_window(session)?));
    }
    if metadata.scopes.contains(&OptionScope::Global) && has_buffer {
        return Ok(OptionTarget::GlobalAndBuffer(current_buffer(session)?));
    }
    if has_window {
        return Ok(OptionTarget::Window(current_window(session)?));
    }
    if has_buffer {
        return Ok(OptionTarget::Buffer(current_buffer(session)?));
    }
    Ok(OptionTarget::Global)
}

fn get_option_at(
    session: &ApiSession,
    name: &str,
    target: OptionTarget,
) -> Result<Object, ApiError> {
    let metadata = ox_editor::OptionStore::metadata(name).map_err(option_value_error)?;
    if metadata.name == "modified"
        && let OptionTarget::Buffer(buffer) | OptionTarget::GlobalAndBuffer(buffer) = target
    {
        let modified = session.with_editor(|editor| {
            editor
                .buffer(buffer)
                .map(|state| state.flags.contains(ox_editor::BufferFlags::MODIFIED))
                .map_err(exception)
        })?;
        return Ok(Object::Boolean(modified));
    }
    let value = session.with_editor(|editor| match target {
        OptionTarget::Global => editor
            .options()
            .get_global(name)
            .map(option_value_to_object),
        OptionTarget::Buffer(buffer) | OptionTarget::GlobalAndBuffer(buffer) => editor
            .options()
            .get_buffer(buffer, name)
            .map(option_value_to_object),
        OptionTarget::Window(window) | OptionTarget::GlobalAndWindow(window) => editor
            .options()
            .get_window(window, name)
            .map(option_value_to_object),
    });
    value.map_err(option_value_error)
}

fn validate_option_at(
    session: &ApiSession,
    name: &str,
    value: &OptionValue,
    target: OptionTarget,
) -> Result<(), ApiError> {
    let metadata = ox_editor::OptionStore::metadata(name).map_err(option_value_error)?;
    if metadata.value_type != value.value_type() {
        return Err(ApiError::validation(format!(
            "Option '{name}' has invalid type"
        )));
    }
    match target {
        OptionTarget::Global if !metadata.scopes.contains(&OptionScope::Global) => Err(
            ApiError::validation(format!("Option '{name}' has no global value")),
        ),
        OptionTarget::Buffer(_) | OptionTarget::GlobalAndBuffer(_)
            if !metadata.scopes.contains(&OptionScope::Buffer) =>
        {
            Err(ApiError::validation(format!(
                "Option '{name}' has no buffer-local value"
            )))
        }
        OptionTarget::Window(_) | OptionTarget::GlobalAndWindow(_)
            if !metadata.scopes.contains(&OptionScope::Window) =>
        {
            Err(ApiError::validation(format!(
                "Option '{name}' has no window-local value"
            )))
        }
        _ => {
            let _ = session;
            Ok(())
        }
    }
}

fn set_option_at(
    session: &ApiSession,
    name: &str,
    value: OptionValue,
    target: OptionTarget,
) -> Result<(), ApiError> {
    let metadata = ox_editor::OptionStore::metadata(name).map_err(option_value_error)?;
    let buffer_only = metadata.scopes.contains(&OptionScope::Buffer)
        && !metadata.scopes.contains(&OptionScope::Global);
    session.with_editor_mut(|editor| match target {
        OptionTarget::Global => {
            if buffer_only {
                editor
                    .options_mut()
                    .set_global_default(name, value)
                    .map_err(option_value_error)
            } else {
                editor
                    .options_mut()
                    .set_global(name, value)
                    .map_err(option_value_error)
            }
        }
        OptionTarget::Buffer(buffer) => editor
            .options_mut()
            .set_buffer(buffer, name, value)
            .map_err(option_value_error),
        OptionTarget::Window(window) => editor
            .options_mut()
            .set_window(window, name, value)
            .map_err(option_value_error),
        OptionTarget::GlobalAndBuffer(buffer) => {
            if buffer_only {
                editor
                    .options_mut()
                    .set_global_default(name, value.clone())
                    .map_err(option_value_error)?;
            } else {
                editor
                    .options_mut()
                    .set_global(name, value.clone())
                    .map_err(option_value_error)?;
            }
            editor
                .options_mut()
                .set_buffer(buffer, name, value)
                .map_err(option_value_error)
        }
        OptionTarget::GlobalAndWindow(window) => {
            editor
                .options_mut()
                .set_global(name, value.clone())
                .map_err(option_value_error)?;
            editor
                .options_mut()
                .set_window(window, name, value)
                .map_err(option_value_error)
        }
    })
}

fn resolve_buffer(session: &ApiSession, buffer: BufHandle) -> Result<BufHandle, ApiError> {
    session.with_editor(|editor| {
        if buffer.is_current() {
            return editor
                .current_buffer()
                .ok_or_else(|| ApiError::exception("No current buffer"));
        }
        editor.buffer(buffer).map_err(exception)?;
        Ok(buffer)
    })
}

fn resolve_window(session: &ApiSession, window: WinHandle) -> Result<WinHandle, ApiError> {
    session.with_editor(|editor| {
        if window.is_current() {
            return editor
                .current_window()
                .ok_or_else(|| ApiError::exception("No current window"));
        }
        editor.window(window).map_err(exception)?;
        Ok(window)
    })
}

fn option_name(name: &OxStr) -> Result<&str, ApiError> {
    std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("Option name must be valid UTF-8"))
}

/// Mirrors upstream `api_typename()` for the option-value validation messages.
fn api_typename(value: &Object) -> &'static str {
    match value {
        Object::Nil => "Nil",
        Object::Boolean(_) => "Boolean",
        Object::Integer(_) => "Integer",
        Object::Float(_) => "Float",
        Object::String(_) => "String",
        Object::Array(_) => "Array",
        Object::Dict(_) => "Dict",
        Object::LuaRef(_) => "LuaRef",
        Object::Buffer(_) => "Buffer",
        Object::Window(_) => "Window",
        Object::Tabpage(_) => "Tabpage",
    }
}

/// The lowercase `:set`-style type name used by upstream value errors.
fn option_type_name(value_type: OptionType) -> &'static str {
    match value_type {
        OptionType::Boolean => "boolean",
        OptionType::Number => "number",
        OptionType::String => "string",
    }
}

fn invalid_option_type(name: &str, value: &Object) -> ApiError {
    ApiError::validation(format!(
        "Invalid '{name}': expected a valid type, got {}",
        api_typename(value)
    ))
}

/// Options whose number type also accepts a numeric string, mirroring the
/// 'wildchar'/'wildcharm' special case in upstream `optval_from_obj()`.
fn accepts_numeric_string(name: &str) -> bool {
    name == "wildchar" || name == "wildcharm"
}

fn option_string(value: OxStr) -> Result<String, ApiError> {
    String::from_utf8(value.0)
        .map_err(|_| ApiError::validation("Option string must be valid UTF-8"))
}

/// Converts an incoming value for `nvim_set_option_value`, accepting the
/// structured Array/Dict forms for list options and producing upstream's
/// `Invalid '<name>': expected a valid type, got <T>` validation errors.
pub(crate) fn object_to_option_value(
    metadata: &'static OptionMetadata,
    name: &str,
    value: Object,
) -> Result<OptionValue, ApiError> {
    match value {
        Object::Boolean(value) if metadata.value_type == OptionType::Boolean => {
            Ok(OptionValue::Boolean(value))
        }
        Object::Integer(value) if metadata.value_type == OptionType::Number => {
            Ok(OptionValue::Number(value))
        }
        Object::String(value) => {
            let text = option_string(value)?;
            match metadata.value_type {
                OptionType::String => Ok(OptionValue::String(text)),
                OptionType::Number if accepts_numeric_string(name) => text
                    .parse::<i64>()
                    .map(OptionValue::Number)
                    .map_err(|_| numeric_string_error(name, &text)),
                _ => Err(invalid_option_type(
                    name,
                    &Object::String(OxStr::from(text.as_str())),
                )),
            }
        }
        Object::Array(items) => {
            if metadata.list.is_none() {
                return Err(invalid_option_type(name, &Object::Array(items)));
            }
            let mut parts: Vec<String> = Vec::with_capacity(items.len());
            for item in items {
                let Object::String(value) = item else {
                    return Err(invalid_option_type(name, &Object::Array(Vec::new())));
                };
                let text = option_string(value)?;
                if metadata.deny_duplicates && parts.contains(&text) {
                    continue;
                }
                parts.push(text);
            }
            Ok(OptionValue::String(parts.join(",")))
        }
        Object::Dict(entries) => {
            dict_to_option_string(metadata, name, entries).map(OptionValue::String)
        }
        other => Err(invalid_option_type(name, &other)),
    }
}

fn numeric_string_error(name: &str, text: &str) -> ApiError {
    ApiError::exception(format!(
        "Invalid value for option '{name}': expected number, got string \"{text}\""
    ))
}

/// Joins a structured Dict input into its canonical `:set` string following
/// upstream `optval_from_obj()`: truthy keys for flag lists, `key:value`
/// entries for colon maps, sorted where the result is a comma list or map.
fn dict_to_option_string(
    metadata: &'static OptionMetadata,
    name: &str,
    entries: Dict,
) -> Result<String, ApiError> {
    let truthy = |value: &Object| !matches!(value, Object::Nil | Object::Boolean(false));
    let key_of = |entry: &(OxStr, Object)| entry.0.to_string_lossy().into_owned();
    let parts: Vec<String> = match metadata.list {
        Some(OptionListKind::Flags) => entries
            .0
            .iter()
            .filter(|(_, value)| truthy(value))
            .map(key_of)
            .collect(),
        Some(OptionListKind::FlagsComma) => {
            let mut parts: Vec<String> = entries
                .0
                .iter()
                .filter(|(_, value)| truthy(value))
                .map(key_of)
                .collect();
            parts.sort();
            parts
        }
        Some(OptionListKind::CommaColon | OptionListKind::OneCommaColon) => {
            let mut parts = Vec::with_capacity(entries.0.len());
            for (key, value) in entries.0 {
                let key = key.to_string_lossy();
                match value {
                    Object::String(value) => {
                        parts.push(format!("{key}:{}", option_string(value)?));
                    }
                    Object::Integer(value) => parts.push(format!("{key}:{value}")),
                    Object::Boolean(true) | Object::Nil => parts.push(key.into_owned()),
                    Object::Boolean(false) => {}
                    other => {
                        return Err(invalid_option_type(
                            name,
                            &Object::Dict(Dict(vec![(OxStr::from(key.as_ref()), other)])),
                        ));
                    }
                }
            }
            parts.sort();
            parts
        }
        _ => return Err(invalid_option_type(name, &Object::Dict(entries))),
    };
    let separator = match metadata.list {
        Some(OptionListKind::Flags) => "",
        _ => ",",
    };
    Ok(parts.join(separator))
}

/// Converts an incoming value for the deprecated setters, mirroring upstream
/// `set_option_to()`: only scalars pass the type gate (with its
/// `Invalid 'value': expected valid option type` validation error) and type
/// mismatches surface the deep `Invalid value for option '<name>'` exception.
pub(crate) fn object_to_legacy_option_value(
    metadata: &'static OptionMetadata,
    name: &str,
    value: Object,
) -> Result<OptionValue, ApiError> {
    match &value {
        Object::Nil | Object::Boolean(_) | Object::Integer(_) | Object::String(_) => {}
        other => {
            return Err(ApiError::validation(format!(
                "Invalid 'value': expected valid option type, got {}",
                api_typename(other)
            )));
        }
    }
    let mismatch = |value: &Object| {
        let (type_name, repr) = match value {
            Object::Boolean(value) => ("boolean", value.to_string()),
            Object::Integer(value) => ("number", value.to_string()),
            Object::String(value) => ("string", format!("\"{}\"", value.to_string_lossy())),
            other => (api_typename(other), String::new()),
        };
        ApiError::exception(format!(
            "Invalid value for option '{name}': expected {}, got {} {}",
            option_type_name(metadata.value_type),
            type_name,
            repr
        ))
    };
    match value {
        Object::Nil => Err(ApiError::validation(
            "Option value must be Boolean, Integer, or String",
        )),
        Object::Boolean(value) if metadata.value_type == OptionType::Boolean => {
            Ok(OptionValue::Boolean(value))
        }
        Object::Integer(value) if metadata.value_type == OptionType::Number => {
            Ok(OptionValue::Number(value))
        }
        Object::String(value) => match metadata.value_type {
            OptionType::String => Ok(OptionValue::String(option_string(value)?)),
            OptionType::Number if accepts_numeric_string(name) => {
                let text = option_string(value.clone())?;
                text.parse::<i64>()
                    .map(OptionValue::Number)
                    .map_err(|_| numeric_string_error(name, &text))
            }
            _ => Err(mismatch(&Object::String(value))),
        },
        other => Err(mismatch(&other)),
    }
}

/// Converts a stored option value to upstream's structured return form for
/// `nvim_set_option_value`: flag and colon lists decompose into Dicts, plain
/// comma lists into Arrays, and scalars pass through unchanged.
pub(crate) fn structured_option_value(
    metadata: &'static OptionMetadata,
    value: &OptionValue,
) -> Object {
    let OptionValue::String(text) = value else {
        return option_value_to_object(value);
    };
    let Some(kind) = metadata.list else {
        return Object::String(OxStr::from(text.as_str()));
    };
    match kind {
        OptionListKind::Flags => {
            let flags = text
                .chars()
                .map(|flag| {
                    let mut buffer = [0u8; 4];
                    let key: &str = flag.encode_utf8(&mut buffer);
                    (OxStr::from(key), Object::Boolean(true))
                })
                .collect::<Vec<_>>();
            Object::Dict(Dict(flags))
        }
        OptionListKind::FlagsComma => Object::Dict(Dict(
            comma_items(text)
                .map(|item| (OxStr::from(item), Object::Boolean(true)))
                .collect::<Vec<_>>(),
        )),
        OptionListKind::Comma | OptionListKind::OneComma => Object::Array(
            comma_items(text)
                .map(|item| Object::String(OxStr::from(item)))
                .collect::<Vec<_>>(),
        ),
        OptionListKind::CommaColon | OptionListKind::OneCommaColon => Object::Dict(Dict(
            comma_items(text)
                .map(|item| match item.split_once(':') {
                    Some((key, value)) => (OxStr::from(key), Object::String(OxStr::from(value))),
                    None => (OxStr::from(item), Object::Boolean(true)),
                })
                .collect::<Vec<_>>(),
        )),
    }
}

fn option_value_to_object(value: &OptionValue) -> Object {
    match value {
        OptionValue::Boolean(value) => Object::Boolean(*value),
        OptionValue::Number(value) => Object::Integer(*value),
        OptionValue::String(value) => Object::String(OxStr::from(value.as_str())),
    }
}

fn comma_items(text: &str) -> impl Iterator<Item = &str> {
    text.split(',').filter(|item| !item.is_empty())
}

/// The option's value at `target`, which `nvim_set_option_value`'s merges fold
/// the incoming value into (upstream reads it through `get_varp_from`).
fn current_option_value(
    session: &ApiSession,
    name: &str,
    target: OptionTarget,
) -> Result<OptionValue, ApiError> {
    session
        .with_editor(|editor| match target {
            OptionTarget::Global => editor.options().get_global(name).cloned(),
            OptionTarget::Buffer(buffer) | OptionTarget::GlobalAndBuffer(buffer) => {
                editor.options().get_buffer(buffer, name).cloned()
            }
            OptionTarget::Window(window) | OptionTarget::GlobalAndWindow(window) => {
                editor.options().get_window(window, name).cloned()
            }
        })
        .map_err(option_value_error)
}

pub(crate) fn object_to_typval(value: &Object, depth: usize) -> Result<Typval, ApiError> {
    if depth >= MAX_CONVERSION_DEPTH {
        return Err(ApiError::exception("Object nesting is too deep"));
    }
    match value {
        Object::Nil => Ok(Typval::Special(Special::Null)),
        Object::Boolean(value) => Ok(Typval::Bool(*value)),
        Object::Integer(value) => Ok(Typval::Number(*value)),
        Object::Float(value) => Ok(Typval::Float(*value)),
        Object::String(value) => Ok(Typval::String(value.clone())),
        Object::Array(values) => values
            .iter()
            .map(|value| object_to_typval(value, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Typval::list),
        Object::Dict(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), object_to_typval(value, depth + 1)?)))
            .collect::<Result<Vec<_>, ApiError>>()
            .map(Typval::dict),
        Object::Buffer(value) => Ok(Typval::Number(i64::from(*value))),
        Object::Window(value) => Ok(Typval::Number(i64::from(*value))),
        Object::Tabpage(value) => Ok(Typval::Number(i64::from(*value))),
        Object::LuaRef(_) => Err(ApiError::exception(
            "LuaRef Typval conversion is unavailable",
        )),
    }
}

fn typval_to_object(value: &Typval, depth: usize) -> Result<Object, ApiError> {
    typval_to_object_inner(value, depth, &mut HashSet::new())
}

fn typval_to_object_inner(
    value: &Typval,
    depth: usize,
    active: &mut HashSet<(usize, u8)>,
) -> Result<Object, ApiError> {
    if depth >= MAX_CONVERSION_DEPTH {
        return Err(ApiError::exception("Typval nesting is too deep"));
    }
    match value {
        Typval::Number(value) => Ok(Object::Integer(*value)),
        Typval::Float(value) => Ok(Object::Float(*value)),
        Typval::String(value) => Ok(Object::String(value.clone())),
        Typval::Blob(value) => Ok(Object::String(OxStr::from(value.as_slice()))),
        Typval::Bool(value) => Ok(Object::Boolean(*value)),
        Typval::Special(Special::Null) | Typval::Funcref(_) | Typval::Partial(_) => Ok(Object::Nil),
        Typval::List(values) => {
            let key = (Rc::as_ptr(values).cast::<()>() as usize, ox_types::VAR_LIST);
            if !active.insert(key) {
                return Ok(Object::Nil);
            }
            let result = values
                .try_borrow()
                .map_err(|_| ApiError::exception("Cannot convert mutably borrowed List"))?
                .items
                .iter()
                .map(|value| typval_to_object_inner(value, depth + 1, active))
                .collect::<Result<Vec<_>, _>>()
                .map(Object::Array);
            active.remove(&key);
            result
        }
        Typval::Dict(values) => {
            let key = (Rc::as_ptr(values).cast::<()>() as usize, ox_types::VAR_DICT);
            if !active.insert(key) {
                return Ok(Object::Nil);
            }
            let result = values
                .try_borrow()
                .map_err(|_| ApiError::exception("Cannot convert mutably borrowed Dictionary"))?
                .entries
                .iter()
                .map(|entry| {
                    Ok((
                        entry.key.clone(),
                        typval_to_object_inner(&entry.value, depth + 1, active)?,
                    ))
                })
                .collect::<Result<Vec<_>, ApiError>>()
                .map(|entries| Object::Dict(Dict(entries)));
            active.remove(&key);
            result
        }
        Typval::Channel(value) | Typval::Job(value) => i64::try_from(*value)
            .map(Object::Integer)
            .map_err(|_| ApiError::exception("Channel/job id exceeds Integer range")),
    }
}

/// Appends one literal byte, quoting NUL and `K_SPECIAL` the way key sequences
/// are stored internally (src/nvim/keycodes.h:15-20,32-45,70-89).
fn push_encoded(output: &mut Vec<u8>, byte: u8) {
    match byte {
        0 => output.extend_from_slice(&[K_SPECIAL, KS_ZERO, KE_FILLER]),
        K_SPECIAL => output.extend_from_slice(&[K_SPECIAL, KS_SPECIAL, KE_FILLER]),
        value => output.push(value),
    }
}

fn replace_termcode_notation(input: &[u8], do_lt: bool, special: bool) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut offset = 0;
    while offset < input.len() {
        if !special || input[offset] != b'<' {
            push_encoded(&mut output, input[offset]);
            offset += 1;
            continue;
        }
        let Some(relative_end) = input[offset + 1..].iter().position(|byte| *byte == b'>') else {
            for byte in &input[offset..] {
                push_encoded(&mut output, *byte);
            }
            break;
        };
        let end = offset + 1 + relative_end;
        let notation = &input[offset + 1..end];
        if let Some(byte) = simple_termcode(notation, do_lt) {
            push_encoded(&mut output, byte);
        } else if let Some(code) = special_keycode(notation) {
            output.extend_from_slice(&code);
        } else {
            for byte in &input[offset..=end] {
                push_encoded(&mut output, *byte);
            }
        }
        offset = end + 1;
    }
    output
}

/// Encodes a named special key (`<Up>`, `<F1>`, `<BS>`, `<Tab>`, ...) as its
/// core `(KS_xxx, KE_xxx)` pair; the third byte is a KE_* enum value when the
/// key has no termcap name, otherwise a termcap byte (src/nvim/keycodes.h
/// `TERMCAP2KEY`). Function keys F13-F63, the keypad set, the shifted/control
/// cursor keys, and the extra xterm keys are all dedicated three-byte keys.
const SPECIAL_KEY_PAIRS: &[(&[u8], (u8, u8))] = &[
    // Base cursor/navigation keys (termcap byte pairs).
    (b"up", (b'k', b'u')),
    (b"down", (b'k', b'd')),
    (b"left", (b'k', b'l')),
    (b"right", (b'k', b'r')),
    (b"home", (b'k', b'h')),
    (b"end", (b'@', b'7')),
    (b"pageup", (b'k', b'P')),
    (b"pagedown", (b'k', b'N')),
    (b"del", (b'k', b'D')),
    (b"delete", (b'k', b'D')),
    (b"bs", (b'k', b'b')),
    (b"backspace", (b'k', b'b')),
    (b"tab", (KS_EXTRA, 54)),    // KE_TAB
    (b"ignore", (KS_EXTRA, 53)), // KE_IGNORE
    (b"nop", (KS_EXTRA, 97)),    // KE_NOP
    (b"insert", (b'k', b'I')),
    (b"ins", (b'k', b'I')),
    (b"help", (b'%', b'1')),
    (b"undo", (b'&', b'8')),
    (b"find", (b'@', b'0')),
    (b"select", (b'*', b'6')), // K_KSELECT
    // Shifted/control cursor keys and shifted Tab (modifier_keys_table).
    (b"s-tab", (b'k', b'B')),   // K_S_TAB
    (b"s-up", (KS_EXTRA, 4)),   // KE_S_UP
    (b"s-down", (KS_EXTRA, 5)), // KE_S_DOWN
    (b"s-left", (b'#', b'4')),
    (b"s-right", (b'%', b'i')),
    (b"s-home", (b'#', b'2')),
    (b"s-end", (b'*', b'7')),
    (b"s-del", (b'*', b'4')),
    (b"c-left", (KS_EXTRA, 85)),  // KE_C_LEFT
    (b"c-right", (KS_EXTRA, 86)), // KE_C_RIGHT
    (b"c-home", (KS_EXTRA, 87)),  // KE_C_HOME
    (b"c-end", (KS_EXTRA, 88)),   // KE_C_END
    // Keypad keys: k0-k9 and k-prefixed navigation/arithmetic.
    (b"k0", (b'K', b'C')),
    (b"k1", (b'K', b'D')),
    (b"k2", (b'K', b'E')),
    (b"k3", (b'K', b'F')),
    (b"k4", (b'K', b'G')),
    (b"k5", (b'K', b'H')),
    (b"k6", (b'K', b'I')),
    (b"k7", (b'K', b'J')),
    (b"k8", (b'K', b'K')),
    (b"k9", (b'K', b'L')),
    (b"kup", (b'K', b'u')),
    (b"kdown", (b'K', b'd')),
    (b"kleft", (b'K', b'l')),
    (b"kright", (b'K', b'r')),
    (b"khome", (b'K', b'1')),
    (b"kend", (b'K', b'4')),
    (b"kpageup", (b'K', b'3')),
    (b"kpagedown", (b'K', b'5')),
    (b"korigin", (b'K', b'2')),
    (b"kplus", (b'K', b'6')),
    (b"kminus", (b'K', b'7')),
    (b"kdivide", (b'K', b'8')),
    (b"kmultiply", (b'K', b'9')),
    (b"kenter", (b'K', b'A')),
    (b"kpoint", (b'K', b'B')),
    (b"kcomma", (b'K', b'M')),
    (b"kequal", (b'K', b'N')),
    (b"kinsert", (KS_EXTRA, 79)), // KE_KINS
    (b"kdel", (KS_EXTRA, 80)),    // KE_KDEL
    // Function keys F1-F12 (F13-F63 handled by the numeric run in `special_pair`).
    (b"f1", (b'k', b'1')),
    (b"f2", (b'k', b'2')),
    (b"f3", (b'k', b'3')),
    (b"f4", (b'k', b'4')),
    (b"f5", (b'k', b'5')),
    (b"f6", (b'k', b'6')),
    (b"f7", (b'k', b'7')),
    (b"f8", (b'k', b'8')),
    (b"f9", (b'k', b'9')),
    (b"f10", (b'k', b';')),
    (b"f11", (b'F', b'1')),
    (b"f12", (b'F', b'2')),
    // Shifted function keys F1-F12.
    (b"s-f1", (KS_EXTRA, 6)),
    (b"s-f2", (KS_EXTRA, 7)),
    (b"s-f3", (KS_EXTRA, 8)),
    (b"s-f4", (KS_EXTRA, 9)),
    (b"s-f5", (KS_EXTRA, 10)),
    (b"s-f6", (KS_EXTRA, 11)),
    (b"s-f7", (KS_EXTRA, 12)),
    (b"s-f8", (KS_EXTRA, 13)),
    (b"s-f9", (KS_EXTRA, 14)),
    (b"s-f10", (KS_EXTRA, 15)),
    (b"s-f11", (KS_EXTRA, 16)),
    (b"s-f12", (KS_EXTRA, 17)),
    // Extra vt100 xterm keys and shifted variants.
    (b"xup", (KS_EXTRA, 65)),    // KE_XUP
    (b"xdown", (KS_EXTRA, 66)),  // KE_XDOWN
    (b"xleft", (KS_EXTRA, 67)),  // KE_XLEFT
    (b"xright", (KS_EXTRA, 68)), // KE_XRIGHT
    (b"xhome", (KS_EXTRA, 63)),  // KE_XHOME
    (b"zhome", (KS_EXTRA, 64)),  // KE_ZHOME
    (b"xend", (KS_EXTRA, 61)),   // KE_XEND
    (b"zend", (KS_EXTRA, 62)),   // KE_ZEND
    (b"xf1", (KS_EXTRA, 57)),
    (b"xf2", (KS_EXTRA, 58)),
    (b"xf3", (KS_EXTRA, 59)),
    (b"xf4", (KS_EXTRA, 60)),
    (b"s-xf1", (KS_EXTRA, 71)),
    (b"s-xf2", (KS_EXTRA, 72)),
    (b"s-xf3", (KS_EXTRA, 73)),
    (b"s-xf4", (KS_EXTRA, 74)),
];

/// Resolves one lowercased `<Notation>` name to its `(second, third)` pair.
fn special_pair(name: &[u8]) -> Option<(u8, u8)> {
    // Function keys F13-F63 are a numeric run with a computed third byte.
    if let Some(digits) = name.strip_prefix(b"f")
        && !digits.is_empty()
        && digits.iter().all(u8::is_ascii_digit)
    {
        let n: u8 = std::str::from_utf8(digits).ok()?.parse().ok()?;
        return function_key(n);
    }
    SPECIAL_KEY_PAIRS
        .iter()
        .find(|entry| entry.0 == name)
        .map(|entry| entry.1)
}

/// Computes the `(second, third)` bytes for a function key F1-F63 following
/// keycodes.h (`K_F1`..`K_F63` termcap byte runs).
fn function_key(n: u8) -> Option<(u8, u8)> {
    match n {
        1..=10 => Some((b'k', *b"123456789;".get((n - 1) as usize)?)),
        11..=12 => Some((b'F', n - 11 + b'1')),
        13..=40 => Some((
            b'F',
            *b"3456789ABCDEFGHIJKLMNOPQRSTU".get((n - 13) as usize)?,
        )),
        41..=63 => Some((b'F', *b"VWXYZabcdefghijklmnopqr".get((n - 41) as usize)?)),
        _ => None,
    }
}

/// Translates a `<Notation>` special-key name to the internal three-byte keycode
/// form `K_SPECIAL second third`, reusing the editor's `Keys::special` encoder.
fn special_keycode(notation: &[u8]) -> Option<[u8; 3]> {
    let lower = notation
        .iter()
        .map(u8::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let pair = special_pair(&lower)?;
    let encoded = Keys::special(pair.0, pair.1).ok()?;
    let bytes = encoded.as_bytes();
    Some([bytes[0], bytes[1], bytes[2]])
}

fn simple_termcode(notation: &[u8], do_lt: bool) -> Option<u8> {
    if notation.eq_ignore_ascii_case(b"lt") {
        return do_lt.then_some(b'<');
    }
    let named = [
        (&b"cr"[..], b'\r'),
        (&b"enter"[..], b'\r'),
        (&b"esc"[..], 0x1b),
        (&b"space"[..], b' '),
        (&b"bar"[..], b'|'),
        (&b"bslash"[..], b'\\'),
        (&b"nul"[..], 0),
    ];
    if let Some((_, value)) = named
        .iter()
        .find(|(name, _)| notation.eq_ignore_ascii_case(name))
    {
        return Some(*value);
    }
    if notation.len() == 3 && notation[0].eq_ignore_ascii_case(&b'c') && notation[1] == b'-' {
        let key = notation[2].to_ascii_uppercase();
        if key == b'?' {
            return Some(0x7f);
        }
        if (b'@'..=b'_').contains(&key) {
            return Some(key & 0x1f);
        }
    }
    None
}

fn dict_string(dict: &Dict, key: &str) -> Result<Option<OxStr>, ApiError> {
    match dict.get(&OxStr::from(key)) {
        None => Ok(None),
        Some(Object::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ApiError::validation(format!("opts.{key} must be String"))),
    }
}

fn dict_bool(dict: &Dict, key: &str) -> Result<Option<bool>, ApiError> {
    match dict.get(&OxStr::from(key)) {
        None => Ok(None),
        Some(Object::Boolean(value)) => Ok(Some(*value)),
        Some(_) => Err(ApiError::validation(format!("opts.{key} must be Boolean"))),
    }
}

fn dict_handle<T, E>(
    dict: &Dict,
    key: &str,
    convert: impl FnOnce(i64) -> Result<T, E>,
) -> Result<Option<T>, ApiError>
where
    E: std::fmt::Display,
{
    match dict.get(&OxStr::from(key)) {
        None => Ok(None),
        Some(Object::Integer(value)) => convert(*value).map(Some).map_err(exception),
        Some(Object::Buffer(value)) if key == "buf" => {
            convert(i64::from(*value)).map(Some).map_err(exception)
        }
        Some(Object::Window(value)) if key == "win" => {
            convert(i64::from(*value)).map(Some).map_err(exception)
        }
        Some(_) => Err(ApiError::validation(format!("opts.{key} must be a handle"))),
    }
}

fn reject_unknown_option_keys(opts: &Dict) -> Result<(), ApiError> {
    for (key, _) in opts.iter() {
        if !matches!(
            key.as_bytes(),
            b"buf" | b"win" | b"tab" | b"filetype" | b"scope" | b"dry_run" | b"operation"
        ) {
            return Err(ApiError::validation(format!(
                "Invalid key: {}",
                key.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn validate_echo_chunks(chunks: &[Object]) -> Result<(), ApiError> {
    for chunk in chunks {
        let Object::Array(values) = chunk else {
            return Err(ApiError::validation("Each echo chunk must be an Array"));
        };
        if !(1..=2).contains(&values.len()) || !matches!(values.first(), Some(Object::String(_))) {
            return Err(ApiError::validation(
                "Each echo chunk must contain text and an optional highlight group",
            ));
        }
        if values.len() == 2
            && !matches!(values.get(1), Some(Object::String(_) | Object::Integer(_)))
        {
            return Err(ApiError::validation(
                "Echo highlight group must be String or Integer",
            ));
        }
    }
    Ok(())
}

// api/vim.c:723-727 delegates deletion to the normal buffer splice.
#[api(since = 1, textlock)]
pub fn nvim_del_current_line(session: &ApiSession) -> Result<(), ApiError> {
    let end = session.with_editor(|editor| {
        let window = editor
            .current_window()
            .ok_or_else(|| ApiError::validation("No current window"))?;
        i64::try_from(editor.window(window).map_err(exception)?.cursor.lnum).map_err(exception)
    })?;
    crate::buffer::nvim_buf_set_lines(
        session,
        BufHandle::CURRENT,
        end.saturating_sub(1),
        end,
        true,
        Vec::new(),
    )
}

fn global_mark_name(name: &OxStr) -> Result<char, ApiError> {
    let [byte] = name.as_bytes() else {
        return Err(ApiError::validation(format!(
            "Invalid mark name (must be a single char): '{}'",
            name.to_string_lossy()
        )));
    };
    if !byte.is_ascii_uppercase() && !byte.is_ascii_digit() {
        return Err(ApiError::validation(format!(
            "Invalid mark name (must be file/uppercase): '{}'",
            name.to_string_lossy()
        )));
    }
    Ok(char::from(*byte))
}

// api/vim.c:2105-2119; helpers.c:1005-1034: deletion sets a zero position,
// and succeeds even when the valid named slot was already unset.
#[api(since = 8)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "`#[api]` requires owned arguments and a `Result` return"
)]
pub fn nvim_del_mark(session: &ApiSession, name: OxStr) -> Result<bool, ApiError> {
    let name = global_mark_name(&name)?;
    session.with_editor_mut(|editor| editor.global_marks_mut().remove(name).map_err(exception))?;
    Ok(true)
}

// api/vim.c:2135-2194: never load a file-backed mark just to inspect it.
#[api(since = 8)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "`#[api]` requires owned arguments and a `Result` return"
)]
pub fn nvim_get_mark(
    session: &ApiSession,
    name: OxStr,
    opts: Dict,
) -> Result<Vec<Object>, ApiError> {
    reject_keys(&opts, &[])?;
    let name = global_mark_name(&name)?;
    session.with_editor(|editor| {
        let unset = || {
            vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(0),
                Object::String(OxStr::from("")),
            ]
        };
        let Some(mark) = editor.global_marks().get(name).map_err(exception)? else {
            return Ok(unset());
        };
        if mark.position.lnum == 0 {
            return Ok(unset());
        }
        let (buffer, filename) = match &mark.target {
            ox_editor::MarkTarget::Buffer(buffer) => {
                let state = editor.buffer(*buffer).map_err(exception)?;
                (i64::from(*buffer), state.name().clone())
            }
            ox_editor::MarkTarget::File(path) => (0, OxStr::from(path.to_string_lossy().as_ref())),
        };
        Ok(vec![
            Object::Integer(i64::try_from(mark.position.lnum).map_err(exception)?),
            Object::Integer(i64::try_from(mark.position.col).map_err(exception)?),
            Object::Integer(buffer),
            Object::String(filename),
        ])
    })
}

// api/vimscript.c:281-350: a String resolves a dict member, whereas an RPC
// Dict calls the supplied function name directly. call() owns self binding.
#[api(since = 4)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "`#[api]` requires owned arguments and a `Result` return"
)]
pub fn nvim_call_dict_function(
    session: &ApiSession,
    dict: Object,
    fn_name: OxStr,
    args: Vec<Object>,
) -> Result<Object, ApiError> {
    if args.len() > MAX_FUNC_ARGS {
        return Err(ApiError::validation(
            "Function called with too many arguments",
        ));
    }
    // Argument shape validates before the executor is acquired, like
    // upstream's early api_set_error paths (api/vim.c:262-266 checks the
    // dict before evaluating anything).
    if !matches!(dict, Object::String(_) | Object::Dict(_)) {
        return Err(ApiError::validation(
            "Invalid dict argument: expected String or Dict",
        ));
    }
    with_command_executor(session, |_, executor| {
        let (dictionary, lookup) = match &dict {
            Object::String(expression) => (
                executor.evaluate(
                    session,
                    std::str::from_utf8(expression.as_bytes()).map_err(exception)?,
                )?,
                true,
            ),
            Object::Dict(_) => (object_to_typval(&dict, 0)?, false),
            _ => unreachable!("validated above"),
        };
        let Typval::Dict(entries) = &dictionary else {
            return Err(ApiError::validation("dict not found"));
        };
        if fn_name.as_bytes().is_empty() {
            return Err(ApiError::validation("Invalid function name: (empty)"));
        }
        let function = if lookup {
            let entries = entries.try_borrow().map_err(exception)?;
            let value = entries.get(fn_name.as_bytes()).ok_or_else(|| {
                ApiError::validation(format!("Not found: {}", fn_name.to_string_lossy()))
            })?;
            match value {
                Typval::Funcref(_) => value.clone(),
                Typval::Partial(_) => {
                    return Err(ApiError::validation("partial function not supported"));
                }
                _ => {
                    return Err(ApiError::validation(format!(
                        "Not a function: {}",
                        fn_name.to_string_lossy()
                    )));
                }
            }
        } else {
            Typval::String(OxStr::from(fn_name.as_bytes()))
        };
        let arguments = args
            .iter()
            .map(|arg| object_to_typval(arg, 0))
            .collect::<Result<Vec<_>, _>>()?;
        executor
            .call_builtin(
                session,
                &OxStr::from("call"),
                vec![function, Typval::list(arguments), dictionary],
            )
            .and_then(|value| typval_to_object(&value, 0))
    })
}

// option.c:7180-7233 is the return schema; metadata comes from options.lua.
fn option_info(metadata: &OptionMetadata) -> Dict {
    let scope = if metadata.scopes.contains(&OptionScope::Buffer) {
        "buf"
    } else if metadata.scopes.contains(&OptionScope::Window) {
        "win"
    } else {
        "global"
    };
    let default = metadata
        .default
        .value
        .map_or(Object::Nil, |value| option_value_to_object(&value.into()));
    Dict(vec![
        (
            OxStr::from("name"),
            Object::String(OxStr::from(metadata.name)),
        ),
        (
            OxStr::from("shortname"),
            Object::String(OxStr::from(metadata.short_name.unwrap_or(""))),
        ),
        (
            OxStr::from("type"),
            Object::String(OxStr::from(option_type_name(metadata.value_type))),
        ),
        (OxStr::from("default"), default),
        (OxStr::from("scope"), Object::String(OxStr::from(scope))),
        (
            OxStr::from("global_local"),
            Object::Boolean(scope != "global" && metadata.scopes.contains(&OptionScope::Global)),
        ),
        (
            OxStr::from("commalist"),
            Object::Boolean(
                metadata
                    .list
                    .is_some_and(|kind| kind != OptionListKind::Flags),
            ),
        ),
        (
            OxStr::from("flaglist"),
            Object::Boolean(matches!(
                metadata.list,
                Some(OptionListKind::Flags | OptionListKind::FlagsComma)
            )),
        ),
        (
            OxStr::from("allows_duplicates"),
            Object::Boolean(!metadata.deny_duplicates),
        ),
        (OxStr::from("was_set"), Object::Boolean(false)),
        (OxStr::from("last_set_sid"), Object::Integer(0)),
        (OxStr::from("last_set_linenr"), Object::Integer(0)),
        (OxStr::from("last_set_chan"), Object::Integer(0)),
    ])
}

#[api(since = 7)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "`#[api]` requires owned arguments and a `Result` return"
)]
pub fn nvim_get_all_options_info(_session: &ApiSession) -> Result<Dict, ApiError> {
    Ok(Dict(
        ox_editor::OPTION_METADATA
            .iter()
            .map(|metadata| {
                (
                    OxStr::from(metadata.name),
                    Object::Dict(option_info(metadata)),
                )
            })
            .collect::<Vec<_>>(),
    ))
}

#[api(since = 11)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "`#[api]` requires owned arguments and a `Result` return"
)]
pub fn nvim_get_option_info2(
    session: &ApiSession,
    name: OxStr,
    opts: Dict,
) -> Result<Dict, ApiError> {
    let name = option_name(&name)?;
    let metadata = ox_editor::OptionStore::metadata(name).map_err(option_value_error)?;
    let target = option_target(session, name, &opts)?;
    get_option_at(session, name, target)?;
    Ok(option_info(metadata))
}

// api/vimscript.c:360-499. This parses syntax; no evaluator is invoked.
#[api(since = 4, fast)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "`#[api]` requires owned arguments and a `Result` return"
)]
pub fn nvim_parse_expression(
    _session: &ApiSession,
    expr: OxStr,
    flags: OxStr,
    hl: bool,
) -> Result<Dict, ApiError> {
    for flag in flags.as_bytes() {
        if !matches!(flag, b'E' | b'l' | b'm') {
            let label = if *flag == 0 {
                "\\0".to_owned()
            } else {
                char::from(*flag).to_string()
            };
            return Err(ApiError::validation(format!(
                "Invalid flag: '{label}' ({flag})"
            )));
        }
    }
    let source = expr.as_bytes();
    let mut consumed = source.len();
    let mut parsed = ox_eval::Parser::new(source)
        .with_max_nesting(MAX_CONVERSION_DEPTH)
        .parse();
    // Multi permits a following expression, but returns only the first.
    if flags.as_bytes().contains(&b'm')
        && let Err(error) = &parsed
        && error.code == "E488"
    {
        consumed = error.offset;
        parsed = ox_eval::Parser::new(
            source
                .get(..consumed)
                .ok_or_else(|| exception("Invalid parser offset"))?,
        )
        .with_max_nesting(MAX_CONVERSION_DEPTH)
        .parse();
    }
    let mut result = Dict(vec![(
        OxStr::from("len"),
        Object::Integer(i64::try_from(consumed).map_err(exception)?),
    )]);
    let ast = match parsed {
        Ok(node) => expression_api_node(&node, source, 0)?,
        Err(error) => {
            result.0.push((
                OxStr::from("error"),
                Object::Dict(Dict(vec![
                    (
                        OxStr::from("message"),
                        Object::String(OxStr::from("E15: Invalid expression: %.*s")),
                    ),
                    (
                        OxStr::from("arg"),
                        Object::String(OxStr::from(source.get(error.offset..).unwrap_or_default())),
                    ),
                ])),
            ));
            Object::Nil
        }
    };
    result.0.push((OxStr::from("ast"), ast));
    if hl {
        result
            .0
            .push((OxStr::from("highlight"), Object::Array(Vec::new())));
    }
    Ok(result)
}

// Node names and local fields: viml/parser/expressions.c:861-900,
// api/vimscript.c:560-616. The evaluator AST does not retain punctuation nodes.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per ExprKind of the parser's grammar; splitting it would scatter the AST mapping"
)]
fn expression_api_node(
    node: &ox_eval::Expr,
    source: &[u8],
    depth: usize,
) -> Result<Object, ApiError> {
    use ox_eval::parser::{BinaryOp, ExprKind, UnaryOp};
    if depth > MAX_CONVERSION_DEPTH {
        return Err(exception("Expression nesting is too deep"));
    }
    let mut fields = Vec::new();
    let mut children = Vec::new();
    let mut start = node.span.start;
    let mut len = node.span.end.saturating_sub(start);
    let convert =
        |child: &ox_eval::Expr| expression_api_node(child, source, depth.saturating_add(1));
    let kind = match &node.kind {
        ExprKind::Literal(Typval::Number(value)) => {
            fields.push((OxStr::from("ivalue"), Object::Integer(*value)));
            "Integer"
        }
        ExprKind::Literal(Typval::Float(value)) => {
            fields.push((OxStr::from("fvalue"), Object::Float(*value)));
            "Float"
        }
        ExprKind::Literal(Typval::String(value)) => {
            fields.push((OxStr::from("svalue"), Object::String(value.clone())));
            if source.get(start) == Some(&b'\'') {
                "SingleQuotedString"
            } else {
                "DoubleQuotedString"
            }
        }
        ExprKind::Variable(name) => {
            let bytes = name.as_bytes();
            let scoped = bytes.get(1) == Some(&b':');
            let scope = if scoped {
                i64::from(bytes.first().copied().unwrap_or_default())
            } else {
                0
            };
            let ident = if scoped {
                bytes.get(2..).unwrap_or_default()
            } else {
                bytes
            };
            fields.push((OxStr::from("scope"), Object::Integer(scope)));
            fields.push((OxStr::from("ident"), Object::String(OxStr::from(ident))));
            "PlainIdentifier"
        }
        ExprKind::Environment(name) => {
            fields.push((OxStr::from("ident"), Object::String(name.clone())));
            "Environment"
        }
        ExprKind::Option { scope, name } => {
            let scope = match scope {
                ox_eval::parser::OptionScope::Effective => 0,
                ox_eval::parser::OptionScope::Global => i64::from(b'g'),
                ox_eval::parser::OptionScope::Local => i64::from(b'l'),
            };
            fields.push((OxStr::from("scope"), Object::Integer(scope)));
            fields.push((OxStr::from("ident"), Object::String(name.clone())));
            "Option"
        }
        ExprKind::Register(name) => {
            fields.push((OxStr::from("name"), Object::Integer(i64::from(*name))));
            "Register"
        }
        ExprKind::Unary { op, expr } => {
            children.push(convert(expr)?);
            len = 1;
            match op {
                UnaryOp::Not => "Not",
                UnaryOp::Negate => "UnaryMinus",
                UnaryOp::Plus => "UnaryPlus",
            }
        }
        ExprKind::Binary { op, left, right } => {
            children.push(convert(left)?);
            children.push(convert(right)?);
            start = left.span.end;
            while source.get(start).is_some_and(u8::is_ascii_whitespace) {
                start = start.saturating_add(1);
            }
            len = match op {
                BinaryOp::And | BinaryOp::Or => 2,
                _ => 1,
            };
            match op {
                BinaryOp::Or => "Or",
                BinaryOp::And => "And",
                BinaryOp::Add => "BinaryPlus",
                BinaryOp::Subtract => "BinaryMinus",
                BinaryOp::Concat => "Concat",
                BinaryOp::Multiply => "Multiplication",
                BinaryOp::Divide => "Division",
                BinaryOp::Modulo => "Mod",
            }
        }
        ExprKind::List(items) => {
            for item in items {
                children.push(convert(item)?);
            }
            len = 1;
            "ListLiteral"
        }
        ExprKind::Dict(items) => {
            for (key, value) in items {
                children.push(convert(key)?);
                children.push(convert(value)?);
            }
            len = 1;
            "DictLiteral"
        }
        ExprKind::Call { callee, args } => {
            children.push(convert(callee)?);
            for arg in args {
                children.push(convert(arg)?);
            }
            start = callee.span.end;
            len = 1;
            "Call"
        }
        ExprKind::Index { target, index } => {
            children.push(convert(target)?);
            children.push(convert(index)?);
            start = target.span.end;
            len = 1;
            "Subscript"
        }
        ExprKind::CurlyName(expr) => {
            children.push(convert(expr)?);
            len = 1;
            "CurlyBracesIdentifier"
        }
        _ => {
            return Err(exception(
                "Expression syntax has no lossless public AST conversion",
            ));
        }
    };
    fields.push((OxStr::from("type"), Object::String(OxStr::from(kind))));
    fields.push((
        OxStr::from("start"),
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(i64::try_from(start).map_err(exception)?),
        ]),
    ));
    fields.push((
        OxStr::from("len"),
        Object::Integer(i64::try_from(len).map_err(exception)?),
    ));
    if !children.is_empty() {
        fields.push((OxStr::from("children"), Object::Array(children)));
    }
    Ok(Object::Dict(Dict(fields)))
}
const STL_ALL: &[u8] = b"fFtcvVlLknoObBrRhHmMyYwWqpPaN{=<$#TXCFS";

// optionstr.c:285-351: reject illegal item chars, unclosed expressions,
// and unbalanced groups before building anything.
fn check_stl_option(statusline: &[u8]) -> Result<(), ApiError> {
    let mut offset = 0;
    let mut depth = 0i64;
    while let Some(index) = statusline[offset..].iter().position(|byte| *byte == b'%') {
        offset = offset.saturating_add(index).saturating_add(1);
        let Some(item) = statusline.get(offset) else {
            break;
        };
        if matches!(item, b'%' | b'<' | b'=') {
            offset = offset.saturating_add(1);
            continue;
        }
        if *item == b')' {
            offset = offset.saturating_add(1);
            depth = depth.saturating_sub(1);
            if depth < 0 {
                break;
            }
            continue;
        }
        let mut cursor = offset;
        if statusline.get(cursor) == Some(&b'-') {
            cursor = cursor.saturating_add(1);
        }
        while statusline.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor = cursor.saturating_add(1);
        }
        if statusline.get(cursor) == Some(&b'*') {
            offset = cursor;
            continue;
        }
        if statusline.get(cursor) == Some(&b'.') {
            cursor = cursor.saturating_add(1);
            while statusline.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor = cursor.saturating_add(1);
            }
        }
        if statusline.get(cursor) == Some(&b'(') {
            depth = depth.saturating_add(1);
            offset = cursor;
            continue;
        }
        if !STL_ALL.contains(item) {
            return Err(ApiError::validation(format!(
                "E539: Illegal character <{}>",
                char::from(*item)
            )));
        }
        if *item == b'{' {
            cursor = cursor.saturating_add(1);
            let reevaluate = statusline.get(cursor) == Some(&b'%');
            if reevaluate {
                cursor = cursor.saturating_add(1);
                if statusline.get(cursor) == Some(&b'}') {
                    return Err(ApiError::validation("E539: Illegal character <}>"));
                }
            }
            while let Some(byte) = statusline.get(cursor) {
                if *byte == b'}'
                    && (!reevaluate || statusline.get(cursor.wrapping_sub(1)) == Some(&b'%'))
                {
                    break;
                }
                cursor = cursor.saturating_add(1);
            }
            if statusline.get(cursor) != Some(&b'}') {
                return Err(ApiError::validation(
                    "E540: Unclosed expression sequence %{",
                ));
            }
        }
        offset = cursor;
    }
    if depth != 0 {
        return Err(ApiError::validation("E542: Unbalanced groups"));
    }
    Ok(())
}

fn cell_width(text: &[u8]) -> usize {
    String::from_utf8_lossy(text).width()
}

// api/vim.c:2221-2387 and statusline.c:1143-1971: a faithful subset of the
// item grammar over editor state this harness owns. Degraded items and the
// missing width model are recorded in the task report.
#[api(since = 8, fast)]
#[expect(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "`#[api]` requires owned arguments and a `Result` return; the body ports the upstream item grammar in one pass"
)]
pub fn nvim_eval_statusline(
    session: &ApiSession,
    str: OxStr,
    opts: Dict,
) -> Result<Dict, ApiError> {
    reject_keys(
        &opts,
        &[
            "winid",
            "maxwidth",
            "fillchar",
            "highlights",
            "use_winbar",
            "use_tabline",
            "use_statuscol_lnum",
        ],
    )?;
    let format = str.as_bytes();
    if !(format.len() >= 2 && format[0] == b'%' && format[1] == b'!') {
        check_stl_option(format)?;
    }
    let fillchar = match dict_string(&opts, "fillchar")? {
        Some(value) => {
            let bytes = value.as_bytes();
            let Some(character) = std::str::from_utf8(bytes)
                .ok()
                .and_then(|text| text.chars().next())
            else {
                return Err(ApiError::validation(
                    "fillchar: expected single character, got invalid UTF-8",
                ));
            };
            if character.len_utf8() != bytes.len() {
                let shown = String::from_utf8_lossy(bytes).into_owned();
                return Err(ApiError::validation(format!(
                    "fillchar: expected single character, got {shown}"
                )));
            }
            bytes.to_vec()
        }
        None => b" ".to_vec(),
    };
    let use_winbar = optional_bool(&opts, "use_winbar")?.unwrap_or(false);
    let use_tabline = optional_bool(&opts, "use_tabline")?.unwrap_or(false);
    let statuscol_lnum = dict_handle(&opts, "use_statuscol_lnum", i64::try_from)?;
    let requested_highlights = optional_bool(&opts, "highlights")?.unwrap_or(false);
    let window = if use_tabline {
        current_window(session)?
    } else {
        let winid = dict_handle(&opts, "winid", WinHandle::try_from)?;
        let handle = winid.unwrap_or(WinHandle::CURRENT);
        let known = session.with_editor(|editor| editor.windows().contains(&handle));
        if handle != WinHandle::CURRENT && !known {
            let number = i64::from(handle);
            return Err(ApiError::exception(format!("unknown winid {number}")));
        }
        resolve_window(session, handle)?
    };
    let mut use_count = usize::from(use_winbar) + usize::from(use_tabline);
    if let Some(lnum) = statuscol_lnum {
        use_count = use_count.saturating_add(1);
        if lnum <= 0 {
            return Err(ApiError::validation(
                "use_statuscol_lnum: expected range > 0",
            ));
        }
        let count = session.with_editor(|editor| {
            let state = editor
                .window(window)
                .map_err(|error| ApiError::validation(error.to_string()))?;
            let buffer = editor
                .buffer(state.buffer)
                .map_err(|error| ApiError::validation(error.to_string()))?;
            if buffer.residency.is_loaded() {
                i64::try_from(buffer.text().map_err(exception)?.line_count()).map_err(exception)
            } else {
                Ok(0)
            }
        })?;
        if lnum > count {
            return Err(ApiError::validation(format!(
                "use_statuscol_lnum: expected range <= {count}"
            )));
        }
    }
    if use_count > 1 {
        return Err(ApiError::validation(
            "Can only use one of 'use_winbar', 'use_tabline' and 'use_statuscol_lnum'",
        ));
    }
    let maxwidth = match dict_handle(&opts, "maxwidth", i64::try_from)? {
        Some(value) => value,
        None => session.with_editor(|editor| {
            i64::try_from(editor.window_geometry(window).map_err(exception)?.width)
                .map_err(exception)
        })?,
    };
    let context = session.with_editor(|editor| {
        let state = editor
            .window(window)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        let cursor = state.cursor;
        let buffer = editor
            .buffer(state.buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        let loaded = buffer.residency.is_loaded();
        let line_count = if loaded {
            buffer.text().map_or(0, ox_text::Buffer::line_count)
        } else {
            0
        };
        let line: Vec<u8> = if loaded {
            buffer
                .text()
                .ok()
                .and_then(|text| text.line(cursor.lnum.saturating_sub(1)).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let modified = buffer.flags.contains(ox_editor::BufferFlags::MODIFIED);
        let modifiable = !buffer.flags.contains(ox_editor::BufferFlags::READONLY);
        let name = buffer.name().clone();
        let handle = state.buffer;
        Ok(StatuslineContext {
            cursor,
            line_count,
            line,
            modified,
            modifiable,
            name,
            handle,
            readonly: false,
            filetype: String::new(),
            arglist: (0, 0),
        })
    })?;
    let mut builder = StlBuilder {
        out: Vec::new(),
        alignment: None,
        truncation: None,
        highlights: Vec::new(),
        default_group: OxStr::from(if use_tabline {
            "TabLineFill"
        } else if use_winbar {
            "WinBar"
        } else {
            "StatusLine"
        }),
        current_groups: Vec::new(),
    };
    let mut offset = 0;
    while offset < format.len() {
        if format[offset] != b'%' {
            let literal = format[offset];
            builder.push_text(&[literal]);
            offset = offset.saturating_add(1);
            continue;
        }
        offset = offset.saturating_add(1);
        let Some(item) = format.get(offset).copied() else {
            break;
        };
        offset = offset.saturating_add(1);
        match item {
            b'%' => builder.push_text(b"%"),
            b'<' => builder.truncation = Some(builder.out.len()),
            b'=' => {
                builder.alignment.get_or_insert(builder.out.len());
            }
            b'#' | b'$' => {
                let end = format
                    .get(offset..)
                    .and_then(|rest| rest.iter().position(|byte| *byte == item))
                    .map(|position| offset.saturating_add(position));
                let Some(end) = end else { break };
                let group = OxStr::from(format.get(offset..end).unwrap_or_default());
                if item == b'#' {
                    builder.close_highlight();
                    builder.current_groups = vec![group];
                } else {
                    builder.current_groups.push(group);
                }
                offset = end.saturating_add(1);
            }
            b'{' => {
                let mut end = offset;
                let reevaluate = format.get(end) == Some(&b'%');
                if reevaluate {
                    end = end.saturating_add(1);
                }
                while let Some(byte) = format.get(end) {
                    if *byte == b'}'
                        && (!reevaluate || format.get(end.wrapping_sub(1)) == Some(&b'%'))
                    {
                        break;
                    }
                    end = end.saturating_add(1);
                }
                if format.get(end) != Some(&b'}') {
                    break;
                }
                let source = std::str::from_utf8(format.get(offset..end).unwrap_or_default())
                    .map_err(|_| ApiError::validation("Expression must be valid UTF-8"))?;
                let value = with_command_executor(session, |_, executor| {
                    executor.evaluate(session, source)
                })?;
                let rendered = match &value {
                    Typval::Number(number) => number.to_string().into_bytes(),
                    Typval::String(text) => text.as_bytes().to_vec(),
                    other => crate::global::typval_to_object(other, 0)
                        .ok()
                        .and_then(|object| {
                            if let Object::String(text) = object {
                                Some(text.as_bytes().to_vec())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default(),
                };
                builder.push_item(&rendered, false);
                offset = end.saturating_add(1);
            }
            b'!' => {
                let source = std::str::from_utf8(format.get(offset..).unwrap_or_default())
                    .map_err(|_| ApiError::validation("Expression must be valid UTF-8"))?;
                let value = with_command_executor(session, |_, executor| {
                    executor.evaluate(session, source)
                })?;
                let rendered = match &value {
                    Typval::Number(number) => number.to_string().into_bytes(),
                    Typval::String(text) => text.as_bytes().to_vec(),
                    _ => Vec::new(),
                };
                builder.push_item(&rendered, false);
                break;
            }
            b'(' | b')' | b'@' | b'T' | b'X' | b'C' | b'S' => {
                if item == b'@' {
                    let end = format
                        .get(offset..)
                        .and_then(|rest| rest.iter().position(|byte| *byte == b'@'))
                        .map(|position| offset.saturating_add(position));
                    let Some(end) = end else { break };
                    offset = end.saturating_add(1);
                }
            }
            b'0'..=b'9' => {
                let end = format
                    .get(offset..)
                    .and_then(|rest| rest.iter().position(|byte| !byte.is_ascii_digit()))
                    .map_or(format.len(), |position| offset.saturating_add(position));
                let digits = format.get(offset..end).unwrap_or_default();
                if digits.len() == 1 && matches!(format.get(end), Some(b'(') | None) {
                    builder.close_highlight();
                    let group = OxStr::from(format!("User{}", char::from(digits[0])).as_bytes());
                    builder.current_groups = vec![group];
                } else if digits.len() == 1 {
                    let value = i64::from(digits[0])
                        .checked_sub(i64::from(b'0'))
                        .ok_or_else(|| exception("Invalid user highlight"))?;
                    builder.push_item(value.to_string().as_bytes(), false);
                }
                offset = end;
                if format.get(offset).is_some_and(u8::is_ascii_digit) {
                    offset = offset.saturating_add(1);
                }
            }
            _ => {
                let mut cursor = offset;
                if format.get(cursor) == Some(&b'-') {
                    cursor = cursor.saturating_add(1);
                }
                let start_digits = cursor;
                while format.get(cursor).is_some_and(u8::is_ascii_digit) {
                    cursor = cursor.saturating_add(1);
                }
                let minwid =
                    std::str::from_utf8(format.get(start_digits..cursor).unwrap_or_default())
                        .unwrap_or("0")
                        .parse::<i64>()
                        .unwrap_or(0);
                let mut maxwid = 9999i64;
                if format.get(cursor) == Some(&b'.') {
                    cursor = cursor.saturating_add(1);
                    let digits_start = cursor;
                    while format.get(cursor).is_some_and(u8::is_ascii_digit) {
                        cursor = cursor.saturating_add(1);
                    }
                    maxwid =
                        std::str::from_utf8(format.get(digits_start..cursor).unwrap_or_default())
                            .unwrap_or("50")
                            .parse::<i64>()
                            .unwrap_or(50);
                }
                let Some(target) = format.get(cursor).copied() else {
                    break;
                };
                offset = cursor.saturating_add(1);
                let mut piece: Vec<u8> = Vec::new();
                let mut fillable = true;
                let mut numeric = false;
                match target {
                    b'f' | b'F' => {
                        fillable = false;
                        piece = context.name.as_bytes().to_vec();
                    }
                    b't' => {
                        fillable = false;
                        let name = context.name.as_bytes();
                        piece = name.iter().rposition(|byte| *byte == b'/').map_or_else(
                            || name.to_vec(),
                            |position| {
                                name.get(position.saturating_add(1)..)
                                    .unwrap_or_default()
                                    .to_vec()
                            },
                        );
                    }
                    b'l' => {
                        numeric = true;
                        piece = context.cursor.lnum.to_string().into_bytes();
                    }
                    b'L' => {
                        numeric = true;
                        piece = context.line_count.to_string().into_bytes();
                    }
                    b'c' => {
                        numeric = true;
                        let column = if context.line.is_empty() {
                            0
                        } else {
                            context.cursor.col.saturating_add(1)
                        };
                        piece = column.to_string().into_bytes();
                    }
                    b'v' => {
                        numeric = true;
                        piece = context
                            .cursor
                            .col
                            .saturating_add(1)
                            .to_string()
                            .into_bytes();
                    }
                    b'V' => {
                        let column = if context.line.is_empty() {
                            0
                        } else {
                            context.cursor.col.saturating_add(1)
                        };
                        piece = format!("-{column}").into_bytes();
                    }
                    b'n' => {
                        numeric = true;
                        piece = i64::from(context.handle).to_string().into_bytes();
                    }
                    b'p' => {
                        numeric = true;
                        let percent = if context.line_count == 0 {
                            0
                        } else {
                            context
                                .cursor
                                .lnum
                                .saturating_mul(100)
                                .checked_div(context.line_count)
                                .ok_or_else(|| exception("Division by zero"))?
                        };
                        piece = percent.to_string().into_bytes();
                    }
                    b'P' => {
                        if context.line_count <= 1 || context.cursor.lnum == 1 {
                            piece = b"Top".to_vec();
                        } else if context.cursor.lnum >= context.line_count {
                            piece = b"Bot".to_vec();
                        } else {
                            let span = context.line_count.saturating_sub(1);
                            let percent = context
                                .cursor
                                .lnum
                                .saturating_sub(1)
                                .saturating_mul(100)
                                .checked_div(span)
                                .ok_or_else(|| exception("Division by zero"))?;
                            piece = format!("{percent}%").into_bytes();
                        }
                    }
                    b'm' | b'M' => {
                        if !context.modifiable {
                            piece = if target == b'M' {
                                b",-".to_vec()
                            } else {
                                b"[-]".to_vec()
                            };
                        } else if context.modified {
                            piece = if target == b'M' {
                                b",+".to_vec()
                            } else {
                                b"[+]".to_vec()
                            };
                        }
                    }
                    b'r' | b'R' => {
                        if context.readonly {
                            piece = if target == b'R' {
                                b",RO".to_vec()
                            } else {
                                b"[RO]".to_vec()
                            };
                        }
                    }
                    b'y' | b'Y' => {
                        fillable = false;
                        if !context.filetype.is_empty() {
                            piece = if target == b'Y' {
                                format!(",{}", context.filetype).into_bytes()
                            } else {
                                format!("[{}]", context.filetype).into_bytes()
                            };
                        }
                    }
                    b'b' => {
                        numeric = true;
                        let byte = context.line.get(context.cursor.col).copied().unwrap_or(0);
                        let value = if byte == b'\n' { 0 } else { byte };
                        piece = value.to_string().into_bytes();
                    }
                    b'B' => {
                        numeric = true;
                        let byte = context.line.get(context.cursor.col).copied().unwrap_or(0);
                        let value = if byte == b'\n' { 0 } else { byte };
                        piece = format!("{value:02X}").into_bytes();
                    }
                    b'a' => {
                        fillable = false;
                        let (index, total) = context.arglist;
                        if total > 0 {
                            piece = format!("({index} of {total})").into_bytes();
                        }
                    }
                    b'N' => {
                        numeric = true;
                    }
                    other => {
                        return Err(ApiError::validation(format!(
                            "E539: Illegal character <{}>",
                            char::from(other)
                        )));
                    }
                }
                let mut text = piece;
                let maxwid_cells = usize::try_from(maxwid.max(0)).unwrap_or(usize::MAX);
                if maxwid > 0
                    && cell_width(&text)
                        > maxwid_cells.saturating_mul(fillchar_width(&fillchar).max(1))
                {
                    truncate_cells(&mut text, maxwid_cells);
                }
                let width = cell_width(&text);
                let minimum = usize::try_from(minwid.unsigned_abs()).unwrap_or(usize::MAX);
                if minwid > 0 && width < minimum {
                    let pad = minimum.saturating_sub(width);
                    let fill = fillchar.repeat(pad);
                    text.splice(0..0, fill);
                } else if minwid < 0 && width < minimum {
                    let pad = minimum.saturating_sub(width);
                    text.extend(fillchar.repeat(pad));
                }
                let _ = numeric;
                builder.push_item(&text, fillable);
            }
        }
    }
    builder.close_highlight();
    if maxwidth > 0 {
        let fill_cells = cell_width(&fillchar).max(1);
        if let Some(at) = builder.alignment {
            let width = cell_width(&builder.out);
            if width < usize::try_from(maxwidth.unsigned_abs()).unwrap_or(usize::MAX) {
                let pad = usize::try_from(maxwidth.unsigned_abs())
                    .unwrap_or(usize::MAX)
                    .saturating_sub(width)
                    .saturating_div(fill_cells);
                let fill = fillchar.repeat(pad);
                let fill_len = fill.len();
                builder.out.splice(at..at, fill);
                builder.shift_highlights(at, isize::try_from(fill_len).unwrap_or(isize::MAX));
            }
        }
        if cell_width(&builder.out) > usize::try_from(maxwidth.unsigned_abs()).unwrap_or(usize::MAX)
            && let Some(at) = builder.truncation
        {
            let tail = builder.out.split_off(at);
            let mut replacement = b"<".to_vec();
            replacement.extend(tail);
            let excess = cell_width(&replacement)
                .saturating_sub(usize::try_from(maxwidth.unsigned_abs()).unwrap_or(usize::MAX));
            if excess > 0 {
                truncate_cells(
                    &mut replacement,
                    usize::try_from(maxwidth.unsigned_abs()).unwrap_or(usize::MAX),
                );
            }
            builder.out = replacement;
            builder.shift_highlights(0, isize::try_from(at).unwrap_or(isize::MAX).wrapping_neg());
        }
    }
    let width = cell_width(&builder.out);
    let mut result = Dict(vec![
        (
            OxStr::from("str"),
            Object::String(OxStr::from(builder.out.as_slice())),
        ),
        (
            OxStr::from("width"),
            Object::Integer(i64::try_from(width).map_err(exception)?),
        ),
    ]);
    if requested_highlights {
        let mut entries = Vec::new();
        let mut first_covers_zero = false;
        for segment in &builder.highlights {
            if segment.0 == 0 {
                first_covers_zero = true;
            }
        }
        if !first_covers_zero && !builder.highlights.is_empty() {
            entries.push(highlight_entry(0, &[builder.default_group.clone()]));
        }
        for (start, groups) in &builder.highlights {
            entries.push(highlight_entry(*start, groups));
        }
        result
            .0
            .push((OxStr::from("highlights"), Object::Array(entries)));
    }
    Ok(result)
}

fn highlight_entry(start: usize, groups: &[OxStr]) -> Object {
    Object::Dict(Dict(vec![
        (
            OxStr::from("start"),
            Object::Integer(i64::try_from(start).unwrap_or(i64::MAX)),
        ),
        (
            OxStr::from("group"),
            Object::String(
                groups
                    .first()
                    .cloned()
                    .unwrap_or_else(|| OxStr::from(&b""[..])),
            ),
        ),
        (
            OxStr::from("groups"),
            Object::Array(
                groups
                    .iter()
                    .map(|group| Object::String(group.clone()))
                    .collect(),
            ),
        ),
    ]))
}

fn fillchar_width(fillchar: &[u8]) -> usize {
    String::from_utf8_lossy(fillchar)
        .chars()
        .map(char::width_usize)
        .sum()
}

fn truncate_cells(text: &mut Vec<u8>, max_cells: usize) {
    let mut kept = Vec::new();
    let mut cells: usize = 0;
    for character in String::from_utf8_lossy(text).chars() {
        let width = character.width_usize();
        if cells.saturating_add(width) > max_cells {
            break;
        }
        cells = cells.saturating_add(width);
        let mut encoded = [0; 4];
        kept.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
    }
    *text = kept;
}

trait CharWidth {
    fn width_usize(self) -> usize;
}

impl CharWidth for char {
    fn width_usize(self) -> usize {
        self.width().unwrap_or(0)
    }
}

struct StatuslineContext {
    cursor: ox_text::Position,
    line_count: usize,
    line: Vec<u8>,
    modified: bool,
    modifiable: bool,
    name: OxStr,
    handle: BufHandle,
    readonly: bool,
    filetype: String,
    arglist: (usize, usize),
}

struct StlBuilder {
    out: Vec<u8>,
    alignment: Option<usize>,
    truncation: Option<usize>,
    highlights: Vec<(usize, Vec<OxStr>)>,
    default_group: OxStr,
    current_groups: Vec<OxStr>,
}

impl StlBuilder {
    fn push_text(&mut self, text: &[u8]) {
        if !self.current_groups.is_empty() {
            let start = self.out.len();
            self.highlights.push((start, self.current_groups.clone()));
        }
        self.out.extend_from_slice(text);
    }

    fn push_item(&mut self, text: &[u8], fillable: bool) {
        let start = self.out.len();
        if !self.current_groups.is_empty() {
            self.highlights.push((start, self.current_groups.clone()));
        }
        if fillable {
            for byte in text {
                if *byte == b' ' {
                    self.out.extend_from_slice(b" ");
                } else {
                    self.out.push(*byte);
                }
            }
        } else {
            self.out.extend_from_slice(text);
        }
    }

    fn close_highlight(&mut self) {
        self.current_groups.clear();
    }

    fn shift_highlights(&mut self, at: usize, by: isize) {
        for (start, _) in &mut self.highlights {
            if *start >= at {
                let shifted = (*start).cast_signed().saturating_add(by);
                *start = shifted.max(0).cast_unsigned();
            }
        }
    }
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(
        nvim_get_current_buf__API_META(),
        nvim_get_current_buf__API_DISPATCH,
    )?;
    registry.register(
        nvim_set_current_buf__API_META(),
        nvim_set_current_buf__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_current_win__API_META(),
        nvim_get_current_win__API_DISPATCH,
    )?;
    registry.register(
        nvim_set_current_win__API_META(),
        nvim_set_current_win__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_current_tabpage__API_META(),
        nvim_get_current_tabpage__API_DISPATCH,
    )?;
    registry.register(
        nvim_set_current_tabpage__API_META(),
        nvim_set_current_tabpage__API_DISPATCH,
    )?;
    registry.register(
        nvim_set_current_dir__API_META(),
        nvim_set_current_dir__API_DISPATCH,
    )?;
    registry.register(nvim_list_bufs__API_META(), nvim_list_bufs__API_DISPATCH)?;
    registry.register(nvim_list_wins__API_META(), nvim_list_wins__API_DISPATCH)?;
    registry.register(
        nvim_list_tabpages__API_META(),
        nvim_list_tabpages__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_api_info__API_META(),
        nvim_get_api_info__API_DISPATCH,
    )?;
    registry.register(nvim_command__API_META(), nvim_command__API_DISPATCH)?;
    registry.register(nvim_exec2__API_META(), nvim_exec2__API_DISPATCH)?;
    registry.register(nvim_cmd__API_META(), nvim_cmd__API_DISPATCH)?;
    registry.register(nvim_exec_lua__API_META(), nvim_exec_lua__API_DISPATCH)?;
    registry.register(nvim_eval__API_META(), nvim_eval__API_DISPATCH)?;
    registry.register(
        nvim_call_function__API_META(),
        nvim_call_function__API_DISPATCH,
    )?;
    registry.register(nvim_get_vvar__API_META(), nvim_get_vvar__API_DISPATCH)?;
    registry.register(nvim_set_vvar__API_META(), nvim_set_vvar__API_DISPATCH)?;
    registry.register(nvim_get_var__API_META(), nvim_get_var__API_DISPATCH)?;
    registry.register(nvim_set_var__API_META(), nvim_set_var__API_DISPATCH)?;
    registry.register(nvim_del_var__API_META(), nvim_del_var__API_DISPATCH)?;
    registry.register(nvim_get_option__API_META(), nvim_get_option__API_DISPATCH)?;
    registry.register(nvim_set_option__API_META(), nvim_set_option__API_DISPATCH)?;
    registry.register(
        nvim_get_option_value__API_META(),
        nvim_get_option_value__API_DISPATCH,
    )?;
    registry.register(
        nvim_set_option_value__API_META(),
        nvim_set_option_value__API_DISPATCH,
    )?;
    registry.register(nvim_input__API_META(), nvim_input__API_DISPATCH)?;
    registry.register(
        nvim_replace_termcodes__API_META(),
        nvim_replace_termcodes__API_DISPATCH,
    )?;
    registry.register(nvim_strwidth__API_META(), nvim_strwidth__API_DISPATCH)?;
    registry.register(nvim_err_writeln__API_META(), nvim_err_writeln__API_DISPATCH)?;
    registry.register(nvim_echo__API_META(), nvim_echo__API_DISPATCH)?;
    registry.register(
        nvim_del_current_line__API_META(),
        nvim_del_current_line__API_DISPATCH,
    )?;
    registry.register(nvim_del_mark__API_META(), nvim_del_mark__API_DISPATCH)?;
    registry.register(nvim_get_mark__API_META(), nvim_get_mark__API_DISPATCH)?;
    registry.register(
        nvim_call_dict_function__API_META(),
        nvim_call_dict_function__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_all_options_info__API_META(),
        nvim_get_all_options_info__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_option_info2__API_META(),
        nvim_get_option_info2__API_DISPATCH,
    )?;
    registry.register(
        nvim_parse_expression__API_META(),
        nvim_parse_expression__API_DISPATCH,
    )?;
    registry.register(
        nvim_eval_statusline__API_META(),
        nvim_eval_statusline__API_DISPATCH,
    )?;
    registry.register(nvim_input_mouse__API_META(), nvim_input_mouse__API_DISPATCH)?;
    Ok(())
}
