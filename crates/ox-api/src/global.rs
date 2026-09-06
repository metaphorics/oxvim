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
use unicode_width::UnicodeWidthStr;

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
    match error {
        EditorError::UnknownWindow(window) => {
            ApiError::exception(format!("Invalid window id: {}", i64::from(window)))
        }
        EditorError::UnknownTabpage(tabpage) => {
            ApiError::exception(format!("Invalid tabpage id: {}", i64::from(tabpage)))
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
    let old = session.with_editor(Editor::current_buffer);
    if old == Some(buf) {
        return Ok(());
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
    let transition = focus_transition(old, new, FocusContainer::Tab);
    fire_focus_events(session, &transition.leaves, old)?;
    // `goto_tabpage_tp` only enters a still-valid tabpage
    // (`window.c:4931-4936`); a handler that closed the target ends the
    // switch without enter events or an error.
    if session.with_editor(|editor| editor.tabpage(target).is_err()) {
        return Ok(());
    }
    session
        .with_editor_mut(|editor| editor.set_current_tabpage(target))
        .map_err(current_handle_error)?;
    fire_focus_events(session, &transition.enters, Some(new))
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
    let transition = focus_transition(old, new, FocusContainer::Window);
    fire_focus_events(session, &transition.leaves, old)?;
    // `goto_tabpage_win` enters only a still-valid window
    // (`window.c:4956`); a handler that closed the target ends the switch
    // without enter events or an error.
    if session.with_editor(|editor| editor.window(target).is_err()) {
        return Ok(());
    }
    session
        .with_editor_mut(|editor| editor.set_current_window(target))
        .map_err(current_handle_error)?;
    fire_focus_events(session, &transition.enters, Some(new))
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
    Ok(())
}
