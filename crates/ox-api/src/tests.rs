#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::cell::RefCell;
use std::rc::Rc;

use ox_editor::{
    AutocmdAction, AutocmdKind, AutocmdOptions, Editor, Event, Geometry, K_SPECIAL, KS_EXTRA, Mode,
    ModeMachine, NullExprEval, OptionValue,
};
use ox_eval::{BuiltinHost, Builtins, Evaluator, NoRegex, Parser, Scope, scope::ScopeMap};
use ox_text::Buffer;
use ox_types::Typval;

use crate::{ApiError, Dict, Object, OxStr, TypeRef};

fn session() -> crate::ApiSession {
    session_with(Editor::new())
}

fn session_with(editor: Editor) -> crate::ApiSession {
    crate::ApiSession::new(Rc::new(RefCell::new(editor)))
}

fn dict(entries: &[(&str, Object)]) -> Dict {
    Dict(
        entries
            .iter()
            .map(|(key, value)| (OxStr::from(*key), value.clone()))
            .collect(),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RecordedOperation {
    Command,
    Script,
    Evaluate,
    Function,
    ChangeDirectory(String),
}

#[derive(Default)]
struct RecordingExecutor {
    commands: Vec<crate::ExCommand>,
    message: Option<&'static str>,
    users: std::collections::BTreeMap<(Option<crate::BufHandle>, String), ox_editor::UserCommand>,
    wiped: Rc<std::cell::RefCell<Vec<crate::BufHandle>>>,
    operations: Rc<RefCell<Vec<RecordedOperation>>>,
    /// One-shot failure returned by the next `define_user_command` call,
    /// simulating a host/reentrancy error.
    define_error: Option<ApiError>,
}

impl crate::CommandExecutor for RecordingExecutor {
    fn execute(
        &mut self,
        session: &crate::ApiSession,
        commands: &[crate::ExCommand],
    ) -> Result<(), ApiError> {
        self.commands.extend_from_slice(commands);
        if let Some(message) = self.message {
            session.with_editor_mut(|editor| {
                editor.push_message(ox_editor::Message {
                    kind: ox_editor::MessageKind::Echo,
                    content: Object::String(OxStr::from(message)),
                    history: true,
                    leading_newline: true,
                });
            });
        }
        Ok(())
    }

    fn execute_command(
        &mut self,
        session: &crate::ApiSession,
        command: &str,
    ) -> Result<(), ApiError> {
        self.operations
            .borrow_mut()
            .push(RecordedOperation::Command);
        let commands = self.parse_cmdline(session, command)?;
        self.execute(session, &commands)
    }

    fn execute_script(
        &mut self,
        session: &crate::ApiSession,
        source: &str,
    ) -> Result<(), ApiError> {
        self.operations.borrow_mut().push(RecordedOperation::Script);
        let commands = self.parse_cmdline(session, source)?;
        self.execute(session, &commands)
    }

    fn define_user_command(
        &mut self,
        _session: &crate::ApiSession,
        buffer: Option<crate::BufHandle>,
        command: ox_editor::UserCommand,
        force: bool,
    ) -> Result<(), ApiError> {
        if let Some(error) = self.define_error.take() {
            return Err(error);
        }
        let key = (buffer, command.name.clone());
        if !force && self.users.contains_key(&key) {
            return Err(ApiError::exception(format!(
                "Command already exists: {}",
                command.name
            )));
        }
        self.users.insert(key, command);
        Ok(())
    }

    fn delete_user_command(
        &mut self,
        _session: &crate::ApiSession,
        buffer: Option<crate::BufHandle>,
        name: &str,
    ) -> Result<(), ApiError> {
        self.users
            .remove(&(buffer, name.to_owned()))
            .map(|_| ())
            .ok_or_else(|| ApiError::exception(format!("Invalid command (not found): {name}")))
    }

    fn list_user_commands(
        &mut self,
        _session: &crate::ApiSession,
        buffer: Option<crate::BufHandle>,
    ) -> Result<Vec<ox_editor::UserCommand>, ApiError> {
        Ok(self
            .users
            .iter()
            .filter(|((entry_buffer, _), _)| *entry_buffer == buffer)
            .map(|(_, command)| command.clone())
            .collect())
    }

    fn parse_cmdline(
        &mut self,
        _session: &crate::ApiSession,
        line: &str,
    ) -> Result<Vec<crate::ExCommand>, ApiError> {
        ox_excmd::Parser::new().parse(line).map_err(|error| {
            ApiError::exception(format!("{}: {}", error.code.as_str(), error.message))
        })
    }

    fn remove_buffer(&mut self, buffer: crate::BufHandle) -> Result<(), ApiError> {
        self.wiped.borrow_mut().push(buffer);
        self.users
            .retain(|(entry_buffer, _), _| *entry_buffer != Some(buffer));
        Ok(())
    }

    fn evaluate(
        &mut self,
        session: &crate::ApiSession,
        expression: &str,
    ) -> Result<Typval, ApiError> {
        self.operations
            .borrow_mut()
            .push(RecordedOperation::Evaluate);
        session.with_editor_mut(|editor| {
            evaluate_builtin(
                editor,
                &OxStr::from("eval"),
                vec![Typval::String(OxStr::from(expression))],
            )
        })
    }

    fn call_builtin(
        &mut self,
        session: &crate::ApiSession,
        name: &OxStr,
        args: Vec<Typval>,
    ) -> Result<Typval, ApiError> {
        self.operations
            .borrow_mut()
            .push(RecordedOperation::Function);
        session.with_editor_mut(|editor| evaluate_builtin(editor, name, args))
    }

    fn change_directory(
        &mut self,
        _session: &crate::ApiSession,
        path: &str,
    ) -> Result<(), ApiError> {
        self.operations
            .borrow_mut()
            .push(RecordedOperation::ChangeDirectory(path.to_owned()));
        Ok(())
    }
}

/// Evaluates one builtin against a live editor the way the real host does:
/// scopes read from the editor, `eval()` re-entering the expression engine.
fn evaluate_builtin(
    editor: &mut Editor,
    name: &OxStr,
    args: Vec<Typval>,
) -> Result<Typval, ApiError> {
    let scope_values = |values: &Dict| -> Result<ScopeMap, ApiError> {
        values
            .iter()
            .map(|(key, value)| Ok((key.clone(), crate::global::object_to_typval(value, 0)?)))
            .collect()
    };
    let mut builtins = Builtins::without_regex();
    let mut scope = Scope::new();
    scope.global = scope_values(editor.gvars())?;
    scope.vim = scope_values(editor.vvars())?;
    if let Some(buffer) = editor.current_buffer() {
        scope.buffer = scope_values(editor.buffer(buffer).unwrap().variables())?;
    }
    if let Some(window) = editor.current_window() {
        scope.window = scope_values(editor.window_variables(window).unwrap())?;
    }
    if let Some(tabpage) = editor.current_tabpage() {
        scope.tab = scope_values(editor.tabpage_variables(tabpage).unwrap())?;
    }
    if name.as_bytes() == b"eval" {
        let source = match &args[..] {
            [source] => ox_eval::builtins::string_arg(source)
                .map_err(|error| ApiError::exception(error.to_string()))?,
            [] => {
                return Err(ApiError::exception(
                    "E119: Not enough arguments for function: eval",
                ));
            }
            _ => {
                return Err(ApiError::exception(
                    "E118: Too many arguments for function: eval",
                ));
            }
        };
        let expression = Parser::new(source.as_bytes())
            .parse()
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let regex = NoRegex;
        Evaluator::new(&mut builtins, &regex)
            .eval(&expression, &mut scope)
            .map_err(|error| ApiError::exception(error.to_string()))
    } else {
        builtins
            .call(name, args, &mut scope)
            .map_err(|error| ApiError::exception(error.to_string()))
    }
}

#[test]
fn nvim_cmd_decodes_structure_and_captures_output() {
    let session = session();
    let mut executor = RecordingExecutor {
        commands: Vec::new(),
        message: Some("captured"),
        ..Default::default()
    };
    let command = dict(&[
        ("cmd", Object::String(OxStr::from("delete"))),
        ("count", Object::Integer(3)),
        (
            "mods",
            Object::Dict(dict(&[
                ("silent", Object::Boolean(true)),
                ("keepjumps", Object::Boolean(true)),
                ("vertical", Object::Boolean(true)),
                ("verbose", Object::Integer(2)),
            ])),
        ),
    ]);
    let result = crate::execute_nvim_cmd(
        &session,
        &command,
        &dict(&[("output", Object::Boolean(true))]),
        &mut executor,
    )
    .unwrap();

    assert_eq!(result, OxStr::from("captured"));
    assert!(session.with_editor(|editor| editor.messages().is_empty()));
    let parsed = executor.commands.first().expect("one parsed command");
    assert!(!parsed.bang);
    assert_eq!(parsed.count, None);
    assert_eq!(parsed.args, "");
    assert!(parsed.range.is_none());
    assert_eq!(parsed.modifiers.len(), 4);

    let edit = dict(&[
        ("cmd", Object::String(OxStr::from("edit"))),
        ("bang", Object::Boolean(true)),
        (
            "args",
            Object::Array(vec![Object::String(OxStr::from("file.txt"))]),
        ),
    ]);
    crate::execute_nvim_cmd(&session, &edit, &Dict(Vec::new()), &mut executor).unwrap();
    let parsed = executor.commands.last().expect("parsed edit command");
    assert!(parsed.bang);
    assert_eq!(parsed.args, "file.txt");

    let ranged = dict(&[
        ("cmd", Object::String(OxStr::from("delete"))),
        (
            "range",
            Object::Array(vec![Object::Integer(2), Object::Integer(4)]),
        ),
    ]);
    crate::execute_nvim_cmd(&session, &ranged, &Dict(Vec::new()), &mut executor).unwrap();
    assert!(
        executor
            .commands
            .last()
            .expect("parsed ranged command")
            .range
            .is_some()
    );
}

#[test]
fn nvim_cmd_rejects_invalid_structured_fields() {
    let session = session();
    let mut executor = RecordingExecutor::default();
    for command in [
        dict(&[]),
        dict(&[
            ("cmd", Object::String(OxStr::from("echo"))),
            ("range", Object::Array(vec![Object::Integer(-1)])),
        ]),
        dict(&[
            ("cmd", Object::String(OxStr::from("echo"))),
            (
                "mods",
                Object::Dict(dict(&[("split", Object::String(OxStr::from("sideways")))])),
            ),
        ]),
        dict(&[
            ("cmd", Object::String(OxStr::from("echo"))),
            ("mystery", Object::Boolean(true)),
        ]),
    ] {
        assert!(
            crate::execute_nvim_cmd(&session, &command, &Dict(Vec::new()), &mut executor).is_err()
        );
    }
}

fn editor_with_lines(
    lines: &[&str],
) -> (Editor, crate::BufHandle, crate::TabHandle, crate::WinHandle) {
    let mut editor = Editor::new();
    let lines = lines
        .iter()
        .map(|line| line.as_bytes().to_vec())
        .collect::<Vec<_>>();
    let buffer = editor
        .create_buffer_with(Buffer::from_lines(&lines, false).unwrap(), true)
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    (editor, buffer, tab, window)
}
fn editor_with_two_windows() -> (Editor, crate::BufHandle, crate::TabHandle, crate::WinHandle) {
    let (mut editor, buffer, tab, window) = editor_with_lines(&["target"]);
    let other = editor
        .create_buffer_with(Buffer::from_lines(&[b"other".to_vec()], false).unwrap(), true)
        .unwrap();
    editor.split_vertical(tab, window, other, true).unwrap();
    (editor, buffer, tab, window)
}
fn set_global_hidden(session: &crate::ApiSession, enabled: bool) {
    crate::global::nvim_set_option_value(
        session,
        OxStr::from("hidden"),
        Object::Boolean(enabled),
        dict(&[("scope", Object::String(OxStr::from("global")))]),
    )
    .unwrap();
}

fn set_buffer_hidden_policy(session: &crate::ApiSession, buffer: crate::BufHandle, value: &str) {
    crate::global::nvim_set_option_value(
        session,
        OxStr::from("bufhidden"),
        Object::String(OxStr::from(value)),
        dict(&[("buf", Object::Buffer(buffer))]),
    )
    .unwrap();
}



#[test]
fn set_current_window_reports_invalid_id_and_switches_valid_window() {
    let (mut editor, buffer, first_tab, first_window) = editor_with_lines(&["one"]);
    let second_tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let second_window = editor.tabpage(second_tab).unwrap().current_window();
    editor.set_current_tabpage(first_tab).unwrap();
    assert_eq!(editor.current_window(), Some(first_window));
    let session = session_with(editor);

    let invalid = crate::WinHandle::try_from(9_999).unwrap();
    // Upstream `find_window_by_handle` fails through `VALIDATE_INT` - a
    // validation-type error, not an exception (api/private/helpers.c:280,
    // api/private/validate.c:12-19).
    assert_eq!(
        crate::global::nvim_set_current_win(&session, invalid),
        Err(ApiError::validation("Invalid window id: 9999"))
    );
    // The 0 sentinel resolves to the current window (helpers.c:280-282):
    // a no-op, not an error.
    crate::global::nvim_set_current_win(&session, crate::WinHandle::CURRENT).unwrap();
    assert_eq!(
        session.with_editor(Editor::current_window),
        Some(first_window)
    );
    crate::global::nvim_set_current_win(&session, second_window).unwrap();
    assert_eq!(
        session.with_editor(Editor::current_window),
        Some(second_window)
    );
}

/// Records one planned action per fired event so order assertions can name
/// the event sequence directly.
fn focus_autocmd(session: &crate::ApiSession, event: &str) -> i64 {
    crate::autocmd::nvim_create_autocmd(
        session,
        Object::String(OxStr::from(event)),
        dict(&[
            ("pattern", Object::String(OxStr::from("*"))),
            ("command", Object::String(OxStr::from("echo fired"))),
        ]),
    )
    .unwrap()
}

/// `nvim_set_current_win` follows upstream `goto_tabpage_win`
/// (`api/vim.c:1024`, `window.c:4953`): a different window in the current
/// tabpage runs `win_enter_ext` — `BufLeave` only when the target shows
/// another buffer, then `WinLeave`, `WinEnter`, and `BufEnter`
/// (window.c:5259, 5265, 5317, 5319). The current window is a no-op
/// (window.c:5248-5250).
#[test]
fn set_current_win_fires_winleave_winenter_in_order() {
    let (mut editor, first_buffer, tab, first_window) = editor_with_lines(&["one"]);
    let second_buffer = editor
        .create_buffer_with(Buffer::from_lines(&[b"two".to_vec()], false).unwrap(), true)
        .unwrap();
    editor
        .split_vertical(tab, first_window, second_buffer, true)
        .unwrap();
    let session = session_with(editor);
    focus_autocmd(&session, "BufLeave");
    focus_autocmd(&session, "WinLeave");
    focus_autocmd(&session, "WinEnter");
    focus_autocmd(&session, "BufEnter");
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    crate::global::nvim_set_current_win(&session, first_window).unwrap();
    assert_eq!(
        session.with_editor(Editor::current_window),
        Some(first_window)
    );
    assert_eq!(
        actions
            .borrow()
            .iter()
            .map(|action| action.event)
            .collect::<Vec<_>>(),
        [
            Event::BufLeave,
            Event::WinLeave,
            Event::WinEnter,
            Event::BufEnter
        ]
    );
    assert_eq!(
        session.with_editor(Editor::current_buffer),
        Some(first_buffer)
    );

    crate::global::nvim_set_current_win(&session, first_window).unwrap();
    assert_eq!(actions.borrow().len(), 4, "current window must not fire");
}

/// `nvim_set_current_buf` reaches the same `do_buffer` switch as `:buffer`
/// (`api/vim.c`), so it fires `BufLeave` before the switch and `BufEnter`,
/// `BufWinEnter` after it (buffer.c:1735, 1850-1851); naming the current
/// buffer fires nothing (buffer.c:1657-1659).
#[test]
fn set_current_buf_fires_the_buffer_lifecycle_in_order() {
    let (mut editor, _first_buffer, _, _) = editor_with_lines(&["one"]);
    let second_buffer = editor
        .create_buffer_with(Buffer::from_lines(&[b"two".to_vec()], false).unwrap(), true)
        .unwrap();
    let session = session_with(editor);
    focus_autocmd(&session, "BufLeave");
    focus_autocmd(&session, "BufEnter");
    focus_autocmd(&session, "BufWinEnter");
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    crate::global::nvim_set_current_buf(&session, second_buffer).unwrap();
    assert_eq!(
        session.with_editor(Editor::current_buffer),
        Some(second_buffer)
    );
    assert_eq!(
        actions
            .borrow()
            .iter()
            .map(|action| action.event)
            .collect::<Vec<_>>(),
        [Event::BufLeave, Event::BufEnter, Event::BufWinEnter,]
    );

    crate::global::nvim_set_current_buf(&session, second_buffer).unwrap();
    assert_eq!(actions.borrow().len(), 3, "same buffer must not fire");
}

/// `nvim_set_current_buf` must preserve invalid Unix filename bytes while
/// probing and reading an unloaded buffer, rather than taking `BufNewFile`.
#[cfg(unix)]
#[test]
fn set_current_buf_reads_unloaded_non_utf8_file_name() {
    use std::os::unix::ffi::OsStrExt;

    let root = std::env::temp_dir().join(format!(
        "oxvim-api-byte-path-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let file_name = std::ffi::OsStr::from_bytes(b"target-\xff.txt");
    let path = root.join(file_name);
    std::fs::write(&path, b"one\ntwo\n").unwrap();

    let (editor, _source, _, _) = editor_with_lines(&["source"]);
    let session = session_with(editor);
    let target = session.with_editor_mut(|editor| {
        let target = editor.create_buffer(true).unwrap();
        let state = editor.buffer_mut(target).unwrap();
        state.set_name(OxStr(path.as_os_str().as_bytes().to_vec()));
        state.unload().unwrap();
        target
    });
    for event in ["BufReadPre", "BufReadPost", "BufNewFile"] {
        focus_autocmd(&session, event);
    }
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    crate::global::nvim_set_current_buf(&session, target).unwrap();

    assert_eq!(
        actions
            .borrow()
            .iter()
            .map(|action| action.event)
            .collect::<Vec<_>>(),
        [Event::BufReadPre, Event::BufReadPost]
    );
    assert_eq!(
        session.with_editor(|editor| {
            editor
                .buffer(target)
                .unwrap()
                .text()
                .unwrap()
                .line(1)
                .unwrap()
                .to_vec()
        }),
        b"one"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn set_current_buf_read_hook_sets_target_buffer_local_option() {
    let (editor, source, _, _) = editor_with_lines(&["source"]);
    let session = session_with(editor);
    let path = std::env::temp_dir().join(format!(
        "oxvim-api-current-buffer-hook-{}",
        std::process::id()
    ));
    std::fs::write(&path, b"target\n").unwrap();
    crate::global::nvim_set_option_value(
        &session,
        OxStr::from("shiftwidth"),
        Object::Integer(8),
        dict(&[("scope", Object::String(OxStr::from("local")))]),
    )
    .unwrap();
    let target = session.with_editor_mut(|editor| {
        let target = editor.create_buffer(true).unwrap();
        let state = editor.buffer_mut(target).unwrap();
        state.set_name(OxStr::from(path.to_string_lossy().as_ref()));
        state.unload().unwrap();
        target
    });
    let setlocal = Rc::new(|session: &crate::ApiSession| {
        crate::global::nvim_set_option_value(
            session,
            OxStr::from("shiftwidth"),
            Object::Integer(3),
            dict(&[("scope", Object::String(OxStr::from("local")))]),
        )
        .map(|_| ())
    });
    crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufReadPre")),
        dict(&[
            ("pattern", Object::String(OxStr::from("*"))),
            ("command", Object::String(OxStr::from("setlocal shiftwidth=3"))),
        ]),
    )
    .unwrap();
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions,
            reenter: Some(setlocal),
        }),
        Box::new(ActionRecorder::default()),
    );

    crate::global::nvim_set_current_buf(&session, target).unwrap();

    assert_eq!(
        session.with_editor(|editor| {
            editor
                .options()
                .get_buffer(target, "shiftwidth")
                .unwrap()
                .clone()
        }),
        OptionValue::Number(3),
    );
    assert_eq!(
        session.with_editor(|editor| {
            editor
                .options()
                .get_buffer(source, "shiftwidth")
                .unwrap()
                .clone()
        }),
        OptionValue::Number(8),
    );
    std::fs::remove_file(path).unwrap();
}

/// A nonexistent target fails through `find_buffer_by_handle`'s
/// `VALIDATE_INT` before any event (api/vim.c:967,
/// api/private/helpers.c:263-275): a validation-type `Invalid buffer id`,
/// not E86 - E86 is `do_buffer`'s error for buffer numbers later in the
/// switch. The current buffer stays untouched.
#[test]
fn set_current_buf_reports_missing_target_without_firing() {
    let (editor, first_buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    focus_autocmd(&session, "BufLeave");
    focus_autocmd(&session, "BufEnter");
    focus_autocmd(&session, "BufWinEnter");
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    let missing = crate::BufHandle::try_from(9999).unwrap();
    let result = crate::global::nvim_set_current_buf(&session, missing);

    assert_eq!(result, Err(ApiError::validation("Invalid buffer id: 9999")));
    assert!(actions.borrow().is_empty(), "failed lookup must not fire");
    assert_eq!(
        session.with_editor(Editor::current_buffer),
        Some(first_buffer)
    );
}

/// `set_curbuf` skips the entry when a `BufLeave` handler wiped the target
/// (`buffer.c:1790-1794`): the leave half already ran, the switch stays
/// silent instead of erroring, and the caller keeps the old buffer.
#[test]
fn set_current_buf_stays_silent_when_a_handler_wipes_the_target() {
    let (mut editor, first_buffer, _, _) = editor_with_lines(&["one"]);
    let second_buffer = editor
        .create_buffer_with(Buffer::from_lines(&[b"two".to_vec()], false).unwrap(), true)
        .unwrap();
    let session = session_with(editor);
    focus_autocmd(&session, "BufLeave");
    focus_autocmd(&session, "BufEnter");
    focus_autocmd(&session, "BufWinEnter");
    let wipe = Rc::new(move |session: &crate::ApiSession| {
        crate::buffer::nvim_buf_delete(
            session,
            second_buffer,
            dict(&[("force", Object::Boolean(true))]),
        )
    });
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: Some(wipe),
        }),
        Box::new(ActionRecorder::default()),
    );

    crate::global::nvim_set_current_buf(&session, second_buffer).unwrap();

    assert_eq!(
        session.with_editor(Editor::current_buffer),
        Some(first_buffer)
    );
    assert_eq!(
        actions
            .borrow()
            .iter()
            .map(|action| action.event)
            .collect::<Vec<_>>(),
        [Event::BufLeave]
    );
}

/// `nvim_set_current_tabpage` runs `goto_tabpage_tp` (`api/vim.c:1320`,
/// `window.c:4920`): `WinLeave`, `TabLeave` on the old tab, `WinEnter`,
/// `TabEnter` on the new one, with `BufLeave`/`BufEnter` only on a buffer
/// change (window.c:4733-4742, 4793, 4826, 4828). The current tabpage is a
/// no-op (window.c:4927).
#[test]
fn set_current_tabpage_fires_the_tab_sequence_in_order() {
    let (mut editor, buffer, first_tab, _) = editor_with_lines(&["one"]);
    let second_tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    editor.set_current_tabpage(first_tab).unwrap();
    let session = session_with(editor);
    focus_autocmd(&session, "WinLeave");
    focus_autocmd(&session, "TabLeave");
    focus_autocmd(&session, "WinEnter");
    focus_autocmd(&session, "TabEnter");
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    crate::global::nvim_set_current_tabpage(&session, second_tab).unwrap();
    assert_eq!(
        session.with_editor(Editor::current_tabpage),
        Some(second_tab)
    );
    assert_eq!(
        actions
            .borrow()
            .iter()
            .map(|action| action.event)
            .collect::<Vec<_>>(),
        [
            Event::WinLeave,
            Event::TabLeave,
            Event::WinEnter,
            Event::TabEnter
        ]
    );

    crate::global::nvim_set_current_tabpage(&session, second_tab).unwrap();
    assert_eq!(actions.borrow().len(), 4, "current tabpage must not fire");
}

#[test]
fn set_current_tabpage_reports_invalid_id_and_switches_valid_tabpage() {
    let (mut editor, buffer, first_tab, _) = editor_with_lines(&["one"]);
    let second_tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    editor.set_current_tabpage(first_tab).unwrap();
    assert_eq!(editor.current_tabpage(), Some(first_tab));
    let session = session_with(editor);

    let invalid = crate::TabHandle::try_from(999).unwrap();
    assert_eq!(
        crate::global::nvim_set_current_tabpage(&session, invalid),
        Err(ApiError::validation("Invalid tabpage id: 999"))
    );
    crate::global::nvim_set_current_tabpage(&session, second_tab).unwrap();
    assert_eq!(
        session.with_editor(Editor::current_tabpage),
        Some(second_tab)
    );
}

#[test]
fn nvim_eval_reads_live_editor_scopes() {
    let (mut editor, buffer, tabpage, window) = editor_with_lines(&["one"]);
    editor
        .gvars_mut()
        .insert(OxStr::from("global"), Object::Integer(1));
    editor
        .buffer_mut(buffer)
        .unwrap()
        .variables_mut()
        .insert(OxStr::from("buffer"), Object::Integer(2));
    editor
        .window_variables_mut(window)
        .unwrap()
        .insert(OxStr::from("window"), Object::Integer(3));
    editor
        .tabpage_variables_mut(tabpage)
        .unwrap()
        .insert(OxStr::from("tabpage"), Object::Integer(4));
    let session = session_with(editor);
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor::default()),
        Box::new(RecordingExecutor::default()),
    );
    assert_eq!(
        crate::global::nvim_eval(
            &session,
            OxStr::from("g:global + b:buffer + w:window + t:tabpage"),
        ),
        Ok(Object::Integer(10)),
    );
}

#[test]
fn nvim_eval_uses_expression_executor() {
    let session = session();
    let operations = Rc::new(RefCell::new(Vec::new()));
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            operations: operations.clone(),
            ..Default::default()
        }),
        Box::new(RecordingExecutor {
            operations: Rc::new(RefCell::new(Vec::new())),
            ..Default::default()
        }),
    );

    assert_eq!(
        crate::global::nvim_eval(&session, OxStr::from("1 + 1")),
        Ok(Object::Integer(2))
    );
    assert_eq!(&*operations.borrow(), &[RecordedOperation::Evaluate]);
}

#[test]
fn deprecated_vim_eval_dispatches_through_the_expression_executor() {
    let session = session();
    let operations = Rc::new(RefCell::new(Vec::new()));
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            operations: operations.clone(),
            ..Default::default()
        }),
        Box::new(RecordingExecutor {
            operations: Rc::new(RefCell::new(Vec::new())),
            ..Default::default()
        }),
    );
    let registry = crate::core().unwrap();
    let dispatch = registry.get("vim_eval").unwrap().1;

    assert_eq!(
        dispatch(&session, &[Object::String(OxStr::from("1 + 1"))]),
        Ok(Object::Integer(2))
    );
    assert_eq!(&*operations.borrow(), &[RecordedOperation::Evaluate]);
}

#[test]
fn deprecated_call_atomic_formats_empty_method_name() {
    let session = session();
    let registry = crate::core().unwrap();
    let dispatch = registry.get("nvim_call_atomic").unwrap().1;

    assert_eq!(
        dispatch(
            &session,
            &[Object::Array(vec![Object::Array(vec![
                Object::String(OxStr::from("")),
                Object::Array(vec![]),
            ])])],
        ),
        Ok(Object::Array(vec![
            Object::Array(vec![]),
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::String(OxStr::from("Invalid method: <empty>")),
            ]),
        ])),
    );
}

#[test]
fn deprecated_call_atomic_validates_each_call_field() {
    let session = session();
    for (call, message) in [
        (Object::Nil, "Invalid 'calls' item: expected Array, got nil"),
        (
            Object::Integer(1),
            "Invalid 'calls' item: expected Array, got Integer",
        ),
        (
            Object::Array(vec![Object::String(OxStr::from("nvim_get_var"))]),
            "Invalid 'calls' item: expected 2-item Array",
        ),
        (
            Object::Array(vec![Object::Integer(1), Object::Array(Vec::new())]),
            "Invalid 'name': expected String, got Integer",
        ),
        (
            Object::Array(vec![Object::LuaRef(7), Object::Array(Vec::new())]),
            "Invalid 'name': expected String, got Function",
        ),
        (
            Object::Array(vec![
                Object::String(OxStr::from("nvim_get_var")),
                Object::Integer(1),
            ]),
            "Invalid call args: expected Array, got Integer",
        ),
    ] {
        assert_eq!(
            crate::deprecated::nvim_call_atomic(&session, vec![call]),
            Err(ApiError::validation(message))
        );
    }
}

#[test]
fn deprecated_call_atomic_keeps_completed_calls_before_invalid_item() {
    let session = session();
    let result = crate::deprecated::nvim_call_atomic(
        &session,
        vec![
            Object::Array(vec![
                Object::String(OxStr::from("nvim_set_var")),
                Object::Array(vec![
                    Object::String(OxStr::from("atomic_applied")),
                    Object::Integer(1),
                ]),
            ]),
            Object::Array(Vec::new()),
        ],
    );

    assert_eq!(
        result,
        Err(ApiError::validation(
            "Invalid 'calls' item: expected 2-item Array"
        ))
    );
    assert_eq!(
        crate::global::nvim_get_var(&session, OxStr::from("atomic_applied")),
        Ok(Object::Integer(1))
    );
}

#[test]
fn deprecated_call_atomic_rejects_non_utf8_method_name() {
    let session = session();
    assert_eq!(
        crate::deprecated::nvim_call_atomic(
            &session,
            vec![Object::Array(vec![
                Object::String(OxStr(vec![0xff])),
                Object::Array(Vec::new()),
            ])],
        ),
        Err(ApiError::validation("call name must be UTF-8"))
    );
}

#[test]
fn buffer_lines_honor_negative_clamping_and_strict_errors() {
    let (editor, buffer, _, _) = editor_with_lines(&["one", "two", "three"]);
    let session = session_with(editor);
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&session, buffer, -2, -1, true),
        Ok(vec![OxStr::from("three")])
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&session, buffer, -99, 99, false),
        Ok(vec![
            OxStr::from("one"),
            OxStr::from("two"),
            OxStr::from("three")
        ])
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&session, buffer, -99, 1, true),
        Err(ApiError::validation("Index out of bounds"))
    );
    crate::buffer::nvim_buf_set_lines(&session, buffer, 1, 2, true, vec![OxStr::from("replaced")])
        .unwrap();
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&session, buffer, 0, -1, true).unwrap(),
        [
            OxStr::from("one"),
            OxStr::from("replaced"),
            OxStr::from("three")
        ]
    );
    assert_eq!(
        crate::global::nvim_get_option_value(
            &session,
            OxStr::from("modified"),
            dict(&[("buf", Object::Buffer(buffer))]),
        ),
        Ok(Object::Boolean(true)),
    );
}

#[test]
fn buffer_mutations_advance_changedtick() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let before = crate::buffer::nvim_buf_get_changedtick(&session, buffer).unwrap();
    crate::buffer::nvim_buf_set_text(&session, buffer, 0, 1, 0, 2, vec![OxStr::from("X")]).unwrap();
    assert!(crate::buffer::nvim_buf_get_changedtick(&session, buffer).unwrap() > before);
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&session, buffer, 0, -1, true).unwrap(),
        [OxStr::from("oXe")]
    );
}

#[test]
fn forced_buffer_delete_rehomes_attached_windows() {
    let (editor, buffer, _, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    crate::buffer::nvim_buf_delete(&session, buffer, dict(&[("force", Object::Boolean(true))]))
        .unwrap();
    assert!(session.with_editor(|editor| editor.buffer(buffer).is_err()));
    assert_ne!(
        session.with_editor(|editor| editor.window(window).unwrap().buffer),
        buffer
    );
}

#[test]
fn cursor_columns_clamp_but_rows_validate() {
    let (editor, _, _, window) = editor_with_lines(&["abc", "z"]);
    let session = session_with(editor);
    crate::window::nvim_win_set_cursor(&session, window, vec![1, 99]).unwrap();
    assert_eq!(
        crate::window::nvim_win_get_cursor(&session, window),
        Ok(vec![1, 3])
    );
    assert_eq!(
        crate::window::nvim_win_set_cursor(&session, window, vec![3, 0]),
        Err(ApiError::validation("Cursor row outside buffer"))
    );
}

#[test]
fn win_close_hidden_keeps_modified_last_buffer() {
    let (editor, buffer, _, window) = editor_with_two_windows();
    let session = session_with(editor);
    set_global_hidden(&session, true);
    session.with_editor_mut(|editor| {
        editor
            .buffer_mut(buffer)
            .unwrap()
            .flags
            .set(ox_editor::BufferFlags::MODIFIED, true);
    });

    crate::window::nvim_win_close(&session, window, false).unwrap();

    session.with_editor(|editor| {
        let state = editor.buffer(buffer).unwrap();
        assert_eq!(state.attachments, 0);
        assert!(state.residency.is_hidden());
        assert!(state.flags.contains(ox_editor::BufferFlags::MODIFIED));
    });
}

#[test]
fn win_close_without_hidden_rejects_modified_last_buffer_with_e37() {
    let (editor, buffer, _, window) = editor_with_two_windows();
    let session = session_with(editor);
    set_global_hidden(&session, false);
    session.with_editor_mut(|editor| {
        editor
            .buffer_mut(buffer)
            .unwrap()
            .flags
            .set(ox_editor::BufferFlags::MODIFIED, true);
    });

    assert_eq!(
        crate::window::nvim_win_close(&session, window, false),
        Err(ApiError::exception(
            "E37: No write since last change (add ! to override)"
        ))
    );
    session.with_editor(|editor| {
        let state = editor.buffer(buffer).unwrap();
        assert_eq!(state.attachments, 1);
        assert!(!state.residency.is_hidden());
    });
}

#[test]
fn win_close_bufhidden_overrides_global_policy_and_controls_release() {
    let (editor, buffer, _, window) = editor_with_two_windows();
    let session = session_with(editor);
    set_global_hidden(&session, false);
    set_buffer_hidden_policy(&session, buffer, "hide");
    session.with_editor_mut(|editor| {
        editor
            .buffer_mut(buffer)
            .unwrap()
            .flags
            .set(ox_editor::BufferFlags::MODIFIED, true);
    });
    crate::window::nvim_win_close(&session, window, false).unwrap();
    session.with_editor(|editor| {
        let state = editor.buffer(buffer).unwrap();
        assert_eq!(state.attachments, 0);
        assert!(state.residency.is_hidden());
        assert!(state.flags.contains(ox_editor::BufferFlags::MODIFIED));
    });

    let (editor, buffer, _, window) = editor_with_two_windows();
    let session = session_with(editor);
    set_global_hidden(&session, true);
    set_buffer_hidden_policy(&session, buffer, "unload");
    session.with_editor_mut(|editor| {
        editor
            .buffer_mut(buffer)
            .unwrap()
            .flags
            .set(ox_editor::BufferFlags::MODIFIED, true);
    });
    assert_eq!(
        crate::window::nvim_win_close(&session, window, false),
        Err(ApiError::exception(
            "E37: No write since last change (add ! to override)"
        ))
    );

    for policy in ["unload", "delete", "wipe"] {
        let (editor, buffer, _, window) = editor_with_two_windows();
        let session = session_with(editor);
        set_global_hidden(&session, true);
        set_buffer_hidden_policy(&session, buffer, policy);
        crate::window::nvim_win_close(&session, window, false).unwrap();
        session.with_editor(|editor| match policy {
            "unload" => {
                let state = editor.buffer(buffer).unwrap();
                assert!(!state.residency.is_loaded());
                assert!(state.flags.contains(ox_editor::BufferFlags::LISTED));
            }
            "delete" => {
                let state = editor.buffer(buffer).unwrap();
                assert!(!state.residency.is_loaded());
                assert!(!state.flags.contains(ox_editor::BufferFlags::LISTED));
            }
            "wipe" => assert!(editor.buffer(buffer).is_err()),
            _ => unreachable!(),
        });
    }
}
#[test]
fn win_close_force_honors_explicit_bufhidden_for_modified_buffer() {
    for policy in ["unload", "delete", "wipe"] {
        let (editor, buffer, _, window) = editor_with_two_windows();
        let session = session_with(editor);
        set_global_hidden(&session, true);
        set_buffer_hidden_policy(&session, buffer, policy);
        session.with_editor_mut(|editor| {
            editor
                .buffer_mut(buffer)
                .unwrap()
                .flags
                .set(ox_editor::BufferFlags::MODIFIED, true);
        });

        crate::window::nvim_win_close(&session, window, true).unwrap();

        session.with_editor(|editor| match policy {
            "unload" => {
                let state = editor.buffer(buffer).unwrap();
                assert!(!state.residency.is_loaded());
                assert!(!state.flags.contains(ox_editor::BufferFlags::MODIFIED));
            }
            "delete" => {
                let state = editor.buffer(buffer).unwrap();
                assert!(!state.residency.is_loaded());
                assert!(!state.flags.contains(ox_editor::BufferFlags::LISTED));
            }
            "wipe" => assert!(editor.buffer(buffer).is_err()),
            _ => unreachable!(),
        });
    }

    let (editor, buffer, _, window) = editor_with_two_windows();
    let session = session_with(editor);
    set_global_hidden(&session, false);
    session.with_editor_mut(|editor| {
        editor
            .buffer_mut(buffer)
            .unwrap()
            .flags
            .set(ox_editor::BufferFlags::MODIFIED, true);
    });
    crate::window::nvim_win_close(&session, window, true).unwrap();
    session.with_editor(|editor| {
        let state = editor.buffer(buffer).unwrap();
        assert!(state.residency.is_hidden());
        assert!(state.residency.is_loaded());
        assert!(state.flags.contains(ox_editor::BufferFlags::MODIFIED));
    });
}
#[test]
fn win_close_bufhidden_does_not_release_buffer_with_other_attachment() {
    let (mut editor, buffer, tab, window) = editor_with_two_windows();
    let second = editor.split_vertical(tab, window, buffer, true).unwrap();
    let session = session_with(editor);
    set_global_hidden(&session, true);
    set_buffer_hidden_policy(&session, buffer, "wipe");

    crate::window::nvim_win_close(&session, second, false).unwrap();

    session.with_editor(|editor| {
        let state = editor.buffer(buffer).unwrap();
        assert_eq!(state.attachments, 1);
        assert!(state.residency.is_loaded());
    });
}



#[test]
fn floating_windows_validate_round_trip_and_close() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let invalid = dict(&[
        ("relative", Object::String(OxStr::from("editor"))),
        ("row", Object::Float(1.0)),
        ("col", Object::Float(2.0)),
        ("width", Object::Integer(0)),
        ("height", Object::Integer(2)),
    ]);
    assert!(matches!(
        crate::window::nvim_open_win(&session, buffer, false, invalid),
        Err(ApiError::Validation(_))
    ));

    let config = dict(&[
        ("relative", Object::String(OxStr::from("editor"))),
        ("row", Object::Float(1.0)),
        ("col", Object::Float(2.0)),
        ("width", Object::Integer(10)),
        ("height", Object::Integer(2)),
        ("border", Object::String(OxStr::from("single"))),
    ]);
    let float = crate::window::nvim_open_win(&session, buffer, false, config).unwrap();
    let returned = crate::window::nvim_win_get_config(&session, float).unwrap();
    assert_eq!(
        returned.get(&OxStr::from("width")),
        Some(&Object::Integer(10))
    );
    assert_eq!(
        crate::window::nvim_win_get_position(&session, float),
        Ok(vec![2, 3])
    );
    crate::window::nvim_win_close(&session, float, false).unwrap();
    assert_eq!(crate::window::nvim_win_is_valid(&session, float), Ok(false));
}

#[test]
fn tabpage_lists_and_selects_real_windows() {
    let (mut editor, buffer, tab, window) = editor_with_lines(&["one"]);
    let second = editor.split_vertical(tab, window, buffer, true).unwrap();
    let session = session_with(editor);
    assert_eq!(
        crate::tabpage::nvim_tabpage_list_wins(&session, tab).unwrap(),
        vec![window, second]
    );
    crate::tabpage::nvim_tabpage_set_win(&session, tab, window).unwrap();
    assert_eq!(
        crate::tabpage::nvim_tabpage_get_win(&session, tab),
        Ok(window)
    );
}

#[test]
fn option_value_scope_distinguishes_global_and_local() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    crate::global::nvim_set_option_value(
        &session,
        OxStr::from("autocomplete"),
        Object::Boolean(false),
        dict(&[("scope", Object::String(OxStr::from("global")))]),
    )
    .unwrap();
    crate::global::nvim_set_option_value(
        &session,
        OxStr::from("autocomplete"),
        Object::Boolean(true),
        dict(&[("scope", Object::String(OxStr::from("local")))]),
    )
    .unwrap();
    assert_eq!(
        crate::global::nvim_get_option_value(
            &session,
            OxStr::from("autocomplete"),
            dict(&[("scope", Object::String(OxStr::from("global")))])
        ),
        Ok(Object::Boolean(false))
    );
    assert_eq!(
        crate::global::nvim_get_option_value(
            &session,
            OxStr::from("autocomplete"),
            dict(&[("scope", Object::String(OxStr::from("local")))])
        ),
        Ok(Object::Boolean(true))
    );
}

#[test]
fn core_registry_metadata_matches_cross_family_sample() {
    let registry = crate::core().unwrap();
    assert_eq!(registry.len(), 262);
    let expected = [
        ("nvim_buf_get_lines", 1, TypeRef::ArrayOf(&TypeRef::String)),
        ("nvim_buf_get_text", 9, TypeRef::ArrayOf(&TypeRef::String)),
        ("nvim_buf_get_changedtick", 2, TypeRef::Integer),
        (
            "nvim_win_get_cursor",
            1,
            TypeRef::Named("ArrayOf(Integer, 2)"),
        ),
        ("nvim_open_win", 6, TypeRef::Window),
        ("nvim_win_set_hl_ns", 10, TypeRef::Void),
        (
            "nvim_tabpage_list_wins",
            1,
            TypeRef::ArrayOf(&TypeRef::Window),
        ),
        ("nvim_tabpage_set_win", 12, TypeRef::Void),
        ("nvim_get_option_value", 9, TypeRef::Object),
        ("nvim_echo", 7, TypeRef::Object),
    ];
    for (name, since, returns) in expected {
        let metadata = registry.get(name).unwrap().0;
        assert_eq!(
            (metadata.since, metadata.returns),
            (since, returns),
            "{name}"
        );
        if name.starts_with("nvim_win_") {
            assert!(metadata.method, "{name} must advertise a window receiver");
        }
    }
}


#[test]
fn registry_dispatch_converts_objects_and_preserves_api_errors() {
    let (editor, buffer, _, _) = editor_with_lines(&["one", "two"]);
    let session = session_with(editor);
    let registry = crate::core().unwrap();
    let dispatch = registry.get("nvim_buf_line_count").unwrap().1;
    assert_eq!(
        dispatch(&session, &[Object::Buffer(buffer)]),
        Ok(Object::Integer(2))
    );
    assert_eq!(
        dispatch(&session, &[Object::String(OxStr::from("bad"))]),
        Err(ApiError::exception(
            "Wrong type for argument 1 when calling nvim_buf_line_count, expecting Buffer"
        ))
    );
}

#[test]
fn nvim_set_current_dir_dispatch_validates_before_executor() {
    let session = session();
    let operations = Rc::new(RefCell::new(Vec::new()));
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            operations: operations.clone(),
            ..Default::default()
        }),
        Box::new(RecordingExecutor {
            operations: Rc::new(RefCell::new(Vec::new())),
            ..Default::default()
        }),
    );
    let registry = crate::core().unwrap();
    let dispatch = registry.get("nvim_set_current_dir").unwrap().1;

    assert_eq!(
        dispatch(&session, &[]),
        Err(ApiError::exception(
            "Wrong number of arguments: expecting 1 but got 0"
        ))
    );
    assert_eq!(
        dispatch(&session, &[Object::Integer(1)]),
        Err(ApiError::exception(
            "Wrong type for argument 1 when calling nvim_set_current_dir, expecting String"
        ))
    );
    assert_eq!(
        dispatch(&session, &[Object::String(OxStr(vec![0xff; 4096]))]),
        Err(ApiError::validation("Invalid directory name: '(too long)'"))
    );
    assert_eq!(
        dispatch(&session, &[Object::String(OxStr(vec![0xff]))]),
        Err(ApiError::validation("Directory must be valid UTF-8"))
    );
    assert!(operations.borrow().is_empty());
}

#[test]
fn nvim_set_current_dir_dispatches_exact_path_without_parsing() {
    let session = session();
    let operations = Rc::new(RefCell::new(Vec::new()));
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            operations: operations.clone(),
            ..Default::default()
        }),
        Box::new(RecordingExecutor {
            operations: Rc::new(RefCell::new(Vec::new())),
            ..Default::default()
        }),
    );
    let registry = crate::core().unwrap();
    let (metadata, dispatch) = registry.get("nvim_set_current_dir").unwrap();

    assert_eq!(*metadata, crate::global::nvim_set_current_dir__API_META());
    assert_eq!(
        dispatch(
            &session,
            &[Object::String(OxStr::from("literal | directory"))]
        ),
        Ok(Object::Nil)
    );
    assert_eq!(
        &*operations.borrow(),
        &[RecordedOperation::ChangeDirectory(
            "literal | directory".to_owned()
        )]
    );
}

#[test]
fn current_line_setter_dispatches_through_the_buffer_mutation_contract() {
    let (editor, buffer, _, window) = editor_with_lines(&["one", "two"]);
    let session = session_with(editor);
    let before = crate::buffer::nvim_buf_get_changedtick(&session, buffer).unwrap();
    let registry = crate::core().unwrap();
    let (metadata, dispatch) = registry.get("nvim_set_current_line").unwrap();

    assert_eq!(metadata.returns, TypeRef::Void);
    assert!(metadata.textlock);
    assert_eq!(
        dispatch(&session, &[Object::String(OxStr::from("replacement"))],),
        Ok(Object::Nil)
    );
    assert_eq!(
        crate::buffer::nvim_get_current_line(&session),
        Ok(OxStr::from("replacement"))
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&session, buffer, 0, -1, true),
        Ok(vec![OxStr::from("replacement"), OxStr::from("two")])
    );
    assert_eq!(
        session.with_editor(|editor| editor.window(window).unwrap().cursor.lnum),
        1
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_changedtick(&session, buffer),
        Ok(before + 1)
    );
}

#[test]
fn nvim_put_dispatch_validates_type_before_any_editor_side_effect() {
    let (editor, buffer, _, window) = editor_with_lines(&["base"]);
    let session = session_with(editor);
    let mode_machine = Rc::new(RefCell::new(ModeMachine::default()));
    session.with_editor_mut(|editor| {
        mode_machine
            .borrow_mut()
            .feed_keys(editor, "v", &mut NullExprEval)
            .unwrap();
    });
    crate::set_mode_machine(&session, mode_machine.clone());
    let before_lines = crate::buffer::nvim_buf_get_lines(&session, buffer, 0, -1, true).unwrap();
    let before_cursor = session.with_editor(|editor| editor.window(window).unwrap().cursor);
    let before_tick = crate::buffer::nvim_buf_get_changedtick(&session, buffer).unwrap();
    let registry = crate::core().unwrap();
    let dispatch = registry.get("nvim_put").unwrap().1;

    assert_eq!(
        dispatch(
            &session,
            &[
                Object::Array(vec![Object::Integer(42)]),
                Object::String(OxStr::from("l")),
                Object::Boolean(false),
                Object::Boolean(false),
            ],
        ),
        Err(ApiError::validation(
            "Invalid 'line': expected String, got Integer"
        ))
    );
    for put_type in ["x", "bx", "b3x"] {
        assert_eq!(
            dispatch(
                &session,
                &[
                    Object::Array(vec![Object::String(OxStr::from("foo"))]),
                    Object::String(OxStr::from(put_type)),
                    Object::Boolean(false),
                    Object::Boolean(false),
                ],
            ),
            Err(ApiError::validation(format!(
                "Invalid 'type': '{put_type}'"
            )))
        );
    }
    assert_eq!(
        dispatch(
            &session,
            &[
                Object::Array(Vec::new()),
                Object::String(OxStr::from("x")),
                Object::Boolean(false),
                Object::Boolean(false),
            ],
        ),
        Err(ApiError::validation("Invalid 'type': 'x'"))
    );
    for put_type in [
        "",
        "v",
        "c",
        "V",
        "l",
        "b",
        "\u{16}",
        "b0",
        "b3",
        "\u{16}3",
        "b999999999999999999999999999999",
    ] {
        assert_eq!(
            dispatch(
                &session,
                &[
                    Object::Array(Vec::new()),
                    Object::String(OxStr::from(put_type)),
                    Object::Boolean(false),
                    Object::Boolean(false),
                ],
            ),
            Ok(Object::Nil),
            "{put_type:?}"
        );
    }
    assert_eq!(
        crate::buffer::nvim_buf_get_lines(&session, buffer, 0, -1, true),
        Ok(before_lines)
    );
    assert_eq!(
        session.with_editor(|editor| editor.window(window).unwrap().cursor),
        before_cursor
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_changedtick(&session, buffer),
        Ok(before_tick)
    );
    assert!(matches!(mode_machine.borrow().mode(), Mode::Visual(_)));
}

#[test]
fn buffer_delete_rehomes_windows_without_force() {
    // Rehoming windows off a deleted buffer must not depend on `force`;
    // `force` only overrides unsaved-change protection (buffer.c:1039-1059).
    let (editor, buffer, _, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    crate::buffer::nvim_buf_delete(&session, buffer, dict(&[])).unwrap();
    assert!(session.with_editor(|editor| editor.buffer(buffer).is_err()));
    assert_ne!(
        session.with_editor(|editor| editor.window(window).unwrap().buffer),
        buffer
    );
}

#[test]
fn buffer_delete_rehomes_windows_with_explicit_false_force() {
    let (editor, buffer, _, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    crate::buffer::nvim_buf_delete(&session, buffer, dict(&[("force", Object::Boolean(false))]))
        .unwrap();
    assert!(session.with_editor(|editor| editor.buffer(buffer).is_err()));
    assert_ne!(
        session.with_editor(|editor| editor.window(window).unwrap().buffer),
        buffer
    );
}

#[test]
fn cursor_column_rejects_values_above_maxcol_before_clamping() {
    // Upstream rejects col > MAXCOL before the silent clamp
    // (api/window.c:122-130, pos_defs.h MAXCOL = 0x7fffffff).
    let (editor, _, _, window) = editor_with_lines(&["abc", "z"]);
    let session = session_with(editor);
    assert_eq!(
        crate::window::nvim_win_set_cursor(&session, window, vec![1, i64::MAX]),
        Err(ApiError::validation("Invalid cursor column: out of range"))
    );
    assert_eq!(
        crate::window::nvim_win_set_cursor(&session, window, vec![1, -1]),
        Err(ApiError::validation("Invalid cursor column: out of range"))
    );
    // MAXCOL itself is accepted, then clamped to the line length.
    crate::window::nvim_win_set_cursor(&session, window, vec![1, 0x7fff_ffff]).unwrap();
    assert_eq!(
        crate::window::nvim_win_get_cursor(&session, window),
        Ok(vec![1, 3])
    );
}

#[derive(Clone)]
struct CapturingAutocmds(Rc<RefCell<Vec<Option<u64>>>>);

impl crate::AutocmdExecutor for CapturingAutocmds {
    fn execute(&mut self, action: &AutocmdAction) -> Result<crate::AutocmdExecution, String> {
        self.0.borrow_mut().push(action.api_id);
        Ok(crate::AutocmdExecution::Keep)
    }

    fn release_callback(&mut self, _reference: u64) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn autocmd_round_trip_shape_clear_and_definition_order() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    // One registration spans the event×pattern cross product under one id.
    let first = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::Array(vec![
            Object::String(OxStr::from("BufEnter")),
            Object::String(OxStr::from("BufLeave")),
        ]),
        dict(&[
            (
                "pattern",
                Object::Array(vec![
                    Object::String(OxStr::from("*.rs")),
                    Object::String(OxStr::from("*.ts")),
                ]),
            ),
            ("command", Object::String(OxStr::from("first"))),
        ]),
    )
    .unwrap();
    let second = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("pattern", Object::String(OxStr::from("*.rs"))),
            ("command", Object::String(OxStr::from("second"))),
        ]),
    )
    .unwrap();
    let returned =
        crate::autocmd::nvim_get_autocmds(&session, dict(&[("id", Object::Integer(first))]))
            .unwrap();
    // The one shared API id covers all event×pattern siblings; private
    // entry ids never surface.
    assert_eq!(returned.len(), 4);
    assert!(
        returned
            .iter()
            .all(|definition| definition.get(&OxStr::from("id")) == Some(&Object::Integer(first)))
    );
    let keys = returned[0]
        .iter()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys,
        ["buflocal", "command", "event", "id", "once", "pattern"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    let captured = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(CapturingAutocmds(captured.clone())),
        Box::new(CapturingAutocmds(Rc::new(RefCell::new(Vec::new())))),
    );
    crate::autocmd::nvim_exec_autocmds(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[("pattern", Object::String(OxStr::from("file.rs")))]),
    )
    .unwrap();
    // Matching siblings fire in definition order, each carrying the id.
    assert_eq!(
        &*captured.borrow(),
        &[Some(first.cast_unsigned()), Some(second.cast_unsigned())]
    );
    // Deleting the id removes every sibling at once.
    crate::autocmd::nvim_del_autocmd(&session, first).unwrap();
    assert!(
        crate::autocmd::nvim_get_autocmds(&session, dict(&[("id", Object::Integer(first))]))
            .unwrap()
            .is_empty()
    );
    crate::autocmd::nvim_clear_autocmds(
        &session,
        dict(&[
            ("event", Object::String(OxStr::from("BufEnter"))),
            ("pattern", Object::String(OxStr::from("*.rs"))),
        ]),
    )
    .unwrap();
    assert!(
        crate::autocmd::nvim_get_autocmds(&session, dict(&[]))
            .unwrap()
            .is_empty()
    );
    // A fully removed id reports not-found.
    assert_eq!(
        crate::autocmd::nvim_del_autocmd(&session, first),
        Err(ApiError::validation(format!("Invalid 'id': {first}"))),
    );
}

#[test]
fn vimscript_callback_round_trips_as_callback_not_command() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let id = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[(
            "callback",
            Object::String(OxStr::from("MyVimscriptFunction")),
        )]),
    )
    .unwrap();

    let definitions =
        crate::autocmd::nvim_get_autocmds(&session, dict(&[("id", Object::Integer(id))])).unwrap();
    let [definition] = definitions.as_slice() else {
        panic!("expected one autocmd definition");
    };
    assert_eq!(
        definition.get(&OxStr::from("command")),
        Some(&Object::String(OxStr::from(""))),
    );
    assert_eq!(
        definition.get(&OxStr::from("callback")),
        Some(&Object::String(OxStr::from("MyVimscriptFunction"))),
    );
}

#[test]
fn firing_plan_skips_an_entry_deleted_by_an_earlier_action() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let first = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("pattern", Object::String(OxStr::from("*"))),
            ("command", Object::String(OxStr::from("first"))),
        ]),
    )
    .unwrap();
    let second = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("pattern", Object::String(OxStr::from("*"))),
            ("command", Object::String(OxStr::from("second"))),
        ]),
    )
    .unwrap();
    let deleted = Rc::new(RefCell::new(false));
    let delete_second = {
        let deleted = deleted.clone();
        Rc::new(move |session: &crate::ApiSession| {
            if !*deleted.borrow() {
                *deleted.borrow_mut() = true;
                crate::autocmd::nvim_del_autocmd(session, second)?;
            }
            Ok(())
        })
    };
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: Some(delete_second),
        }),
        Box::new(ActionRecorder {
            actions: Rc::new(RefCell::new(Vec::new())),
            reenter: None,
        }),
    );

    crate::autocmd::nvim_exec_autocmds(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[]),
    )
    .unwrap();

    assert_eq!(
        actions
            .borrow()
            .iter()
            .map(|action| action.api_id)
            .collect::<Vec<_>>(),
        [Some(first.cast_unsigned())]
    );
}

type SessionReentry = Rc<dyn Fn(&crate::ApiSession) -> Result<(), ApiError>>;

/// Records every planned action; `reenter` stands in for a handler that
/// re-enters the API with the live editor while the host is checked out.
#[derive(Clone, Default)]
struct ActionRecorder {
    actions: Rc<RefCell<Vec<AutocmdAction>>>,
    reenter: Option<SessionReentry>,
}

impl crate::AutocmdExecutor for ActionRecorder {
    fn execute(&mut self, action: &AutocmdAction) -> Result<crate::AutocmdExecution, String> {
        self.actions.borrow_mut().push(action.clone());
        Ok(crate::AutocmdExecution::Keep)
    }

    fn execute_with_session(
        &mut self,
        session: &crate::ApiSession,
        action: &AutocmdAction,
    ) -> Result<crate::AutocmdExecution, String> {
        self.actions.borrow_mut().push(action.clone());
        if let Some(reenter) = self.reenter.clone() {
            reenter(session).map_err(|error| error.to_string())?;
        }
        Ok(crate::AutocmdExecution::Keep)
    }

    fn release_callback(&mut self, _reference: u64) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct CallbackLifecycleRecorder {
    actions: Rc<RefCell<Vec<AutocmdAction>>>,
    released: Rc<RefCell<Vec<u64>>>,
    outcome: crate::AutocmdExecution,
    reenter: Option<SessionReentry>,
}

impl crate::AutocmdExecutor for CallbackLifecycleRecorder {
    fn execute(&mut self, action: &AutocmdAction) -> Result<crate::AutocmdExecution, String> {
        self.actions.borrow_mut().push(action.clone());
        Ok(self.outcome)
    }

    fn execute_with_session(
        &mut self,
        session: &crate::ApiSession,
        action: &AutocmdAction,
    ) -> Result<crate::AutocmdExecution, String> {
        let outcome = self.execute(action)?;
        if let Some(reenter) = self.reenter.clone() {
            reenter(session).map_err(|error| error.to_string())?;
        }
        Ok(outcome)
    }

    fn release_callback(&mut self, reference: u64) -> Result<(), String> {
        self.released.borrow_mut().push(reference);
        Ok(())
    }
}

#[test]
fn callback_delete_outcome_removes_entry_and_releases_its_data_reference() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let data = Object::Dict(dict(&[("answer", Object::Integer(42))]));
    let id = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("User")),
        dict(&[
            ("pattern", Object::String(OxStr::from("Build"))),
            ("callback", Object::LuaRef(41)),
        ]),
    )
    .unwrap();
    let actions = Rc::new(RefCell::new(Vec::new()));
    let released = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(CallbackLifecycleRecorder {
            actions: actions.clone(),
            released: released.clone(),
            outcome: crate::AutocmdExecution::Delete,
            reenter: None,
        }),
        Box::new(CallbackLifecycleRecorder {
            actions: Rc::new(RefCell::new(Vec::new())),
            released: Rc::new(RefCell::new(Vec::new())),
            outcome: crate::AutocmdExecution::Keep,
            reenter: None,
        }),
    );

    crate::autocmd::nvim_exec_autocmds(
        &session,
        Object::String(OxStr::from("User")),
        dict(&[
            ("pattern", Object::String(OxStr::from("Build"))),
            ("data", data.clone()),
        ]),
    )
    .unwrap();

    assert!(
        crate::autocmd::nvim_get_autocmds(&session, dict(&[("id", Object::Integer(id))]))
            .unwrap()
            .is_empty()
    );
    assert_eq!(&*released.borrow(), &[41]);
    let action = actions.borrow()[0].clone();
    assert_eq!(action.data, Some(data.clone()));
    let callback_args = action.callback_args().unwrap();
    let [Object::Dict(args)] = callback_args.as_slice() else {
        panic!("expected one callback dictionary");
    };
    assert_eq!(args.get(&OxStr::from("id")), Some(&Object::Integer(id)));
    assert_eq!(
        args.get(&OxStr::from("event")),
        Some(&Object::String(OxStr::from("User")))
    );
    assert_eq!(
        args.get(&OxStr::from("match")),
        Some(&Object::String(OxStr::from("Build")))
    );
    assert_eq!(args.get(&OxStr::from("data")), Some(&data));
}

#[test]
fn callback_reference_lives_until_every_sibling_is_removed() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let id = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::Array(vec![
            Object::String(OxStr::from("BufEnter")),
            Object::String(OxStr::from("BufLeave")),
        ]),
        dict(&[
            ("pattern", Object::String(OxStr::from("*"))),
            ("callback", Object::LuaRef(42)),
            ("once", Object::Boolean(true)),
        ]),
    )
    .unwrap();
    let actions = Rc::new(RefCell::new(Vec::new()));
    let released = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(CallbackLifecycleRecorder {
            actions,
            released: released.clone(),
            outcome: crate::AutocmdExecution::Keep,
            reenter: None,
        }),
        Box::new(CallbackLifecycleRecorder {
            actions: Rc::new(RefCell::new(Vec::new())),
            released: Rc::new(RefCell::new(Vec::new())),
            outcome: crate::AutocmdExecution::Keep,
            reenter: None,
        }),
    );

    crate::autocmd::nvim_exec_autocmds(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[]),
    )
    .unwrap();
    assert_eq!(definition_count(&session, id), 1);
    assert!(released.borrow().is_empty());
    crate::autocmd::nvim_exec_autocmds(
        &session,
        Object::String(OxStr::from("BufLeave")),
        dict(&[]),
    )
    .unwrap();
    assert_eq!(definition_count(&session, id), 0);
    assert_eq!(&*released.borrow(), &[42]);
}

#[test]
fn reentrant_once_callback_sibling_deletion_releases_once() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let id = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::Array(vec![
            Object::String(OxStr::from("BufEnter")),
            Object::String(OxStr::from("BufLeave")),
        ]),
        dict(&[
            ("pattern", Object::String(OxStr::from("Clean"))),
            ("callback", Object::LuaRef(43)),
            ("once", Object::Boolean(true)),
        ]),
    )
    .unwrap();
    let released = Rc::new(RefCell::new(Vec::new()));
    let delete_self =
        Rc::new(move |session: &crate::ApiSession| crate::autocmd::nvim_del_autocmd(session, id));
    crate::set_autocmd_executor(
        &session,
        Box::new(CallbackLifecycleRecorder {
            actions: Rc::new(RefCell::new(Vec::new())),
            released: released.clone(),
            outcome: crate::AutocmdExecution::Keep,
            reenter: Some(delete_self),
        }),
        Box::new(CallbackLifecycleRecorder {
            actions: Rc::new(RefCell::new(Vec::new())),
            released: released.clone(),
            outcome: crate::AutocmdExecution::Keep,
            reenter: None,
        }),
    );

    crate::autocmd::nvim_exec_autocmds(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[("pattern", Object::String(OxStr::from("Clean")))]),
    )
    .unwrap();

    assert_eq!(&*released.borrow(), &[43]);
}

/// `nvim_exec_autocmds` on an unloaded buffer enters it before running the
/// callback — upstream `ctx_switch` (legacy `aucmd_prepbuf`) makes the
/// target current even when its text is not resident — so a callback
/// observing `nvim_get_current_buf()` sees the target, and the caller's
/// buffer is current again afterwards.
#[test]
fn exec_autocmds_enters_an_unloaded_target_buffer() {
    let (mut editor, caller, _tab, _window) = editor_with_lines(&["one"]);
    let target = editor.create_buffer(true).unwrap();
    editor.unload_buffer(target).unwrap();
    let session = session_with(editor);
    crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("buffer", Object::Integer(i64::from(target))),
            ("callback", Object::LuaRef(44)),
        ]),
    )
    .unwrap();
    let observed = Rc::new(RefCell::new(Vec::new()));
    let record = {
        let observed = observed.clone();
        Rc::new(move |session: &crate::ApiSession| {
            observed
                .borrow_mut()
                .push(crate::global::nvim_get_current_buf(session)?);
            Ok(())
        })
    };
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: Rc::new(RefCell::new(Vec::new())),
            reenter: Some(record),
        }),
        Box::new(ActionRecorder::default()),
    );

    crate::autocmd::nvim_exec_autocmds(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[("buf", Object::Integer(i64::from(target)))]),
    )
    .unwrap();

    assert_eq!(&*observed.borrow(), &[target]);
    assert_eq!(
        crate::global::nvim_get_current_buf(&session).unwrap(),
        caller,
        "the caller's buffer is restored after the callback"
    );
    assert!(
        session.with_editor(|editor| editor.buffer(target).unwrap().residency.is_loaded()),
        "the entered target stays loaded once the switch is undone"
    );
}

fn filetype_autocmd(session: &crate::ApiSession, pattern: &str, once: bool) -> i64 {
    crate::autocmd::nvim_create_autocmd(
        session,
        Object::String(OxStr::from("FileType")),
        dict(&[
            ("pattern", Object::String(OxStr::from(pattern))),
            ("command", Object::String(OxStr::from("echo fired"))),
            ("once", Object::Boolean(once)),
        ]),
    )
    .unwrap()
}

fn set_filetype(
    session: &crate::ApiSession,
    name: &str,
    value: &str,
    opts: &[(&str, Object)],
) -> Result<Object, ApiError> {
    crate::global::nvim_set_option_value(
        session,
        OxStr::from(name),
        Object::String(OxStr::from(value)),
        dict(opts),
    )
}

fn definition_count(session: &crate::ApiSession, id: i64) -> usize {
    crate::autocmd::nvim_get_autocmds(session, dict(&[("id", Object::Integer(id))]))
        .unwrap()
        .len()
}

#[test]
fn set_option_value_fires_one_populated_filetype_after_commit() {
    let (mut editor, current, _, window) = editor_with_lines(&["one"]);
    let target = editor
        .create_buffer_with(Buffer::from_lines(&[b"two".to_vec()], false).unwrap(), true)
        .unwrap();
    let session = session_with(editor);
    let expected_name = ox_editor::expand_buffer_name(&OxStr::from("src/lib.rs"));
    crate::buffer::nvim_buf_set_name(&session, target, OxStr::from("src/lib.rs")).unwrap();
    let id = filetype_autocmd(&session, "rust", false);
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    let assigned = set_filetype(
        &session,
        "filetype",
        "rust",
        &[("buf", Object::Integer(i64::from(target)))],
    )
    .unwrap();

    assert_eq!(assigned, Object::String(OxStr::from("rust")));
    assert_eq!(
        session.with_editor(|editor| { editor.options().get_buffer(target, "filetype").cloned() }),
        Ok(OptionValue::String("rust".to_owned()))
    );
    // The selected buffer stayed selected by handle; no window switched.
    assert_eq!(session.with_editor(Editor::current_buffer), Some(current));
    assert_eq!(
        session.with_editor(|editor| editor.window(window).unwrap().buffer),
        current
    );
    let recorded = actions.borrow();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].api_id, Some(u64::try_from(id).unwrap()));
    assert_eq!(recorded[0].file_name, expected_name.to_string_lossy());
    assert_eq!(recorded[0].buffer, Some(target));
}

#[test]
fn ft_alias_with_local_scope_fires_for_the_current_buffer() {
    let (editor, current, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    filetype_autocmd(&session, "python", false);
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    set_filetype(
        &session,
        "ft",
        "python",
        &[("scope", Object::String(OxStr::from("local")))],
    )
    .unwrap();

    let recorded = actions.borrow();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].match_name, "python");
    assert_eq!(recorded[0].buffer, Some(current));
}

#[test]
fn same_value_top_level_filetype_assignment_fires() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    filetype_autocmd(&session, "rust", false);
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    for _ in 0..2 {
        set_filetype(&session, "filetype", "rust", &[]).unwrap();
    }

    assert_eq!(actions.borrow().len(), 2);
}

#[test]
fn recursive_same_value_filetype_firing_is_suppressed_before_planning() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let outer = filetype_autocmd(&session, "rust", true);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let reenter = Rc::new(move |session: &crate::ApiSession| {
        // Registered while the outer dispatch runs: only a changed nested
        // occurrence may consume it, because the same-value recursion is
        // suppressed before planning.
        let inner = filetype_autocmd(session, "rust", true);
        crate::autocmd::fire_filetype(session, buffer, "rust")?;
        assert_eq!(definition_count(session, inner), 1);
        Ok(())
    });
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: Some(reenter),
        }),
        Box::new(ActionRecorder::default()),
    );

    set_filetype(&session, "filetype", "rust", &[]).unwrap();

    assert_eq!(actions.borrow().len(), 1);
    // The outer `++once` definition was consumed at execution start.
    assert_eq!(definition_count(&session, outer), 0);
}

#[test]
fn changed_recursive_filetype_occurrence_fires_and_terminates() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let outer = filetype_autocmd(&session, "rust", true);
    let inner = filetype_autocmd(&session, "python", true);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let reenter = Rc::new(move |session: &crate::ApiSession| {
        // The changed value is not in flight, so it plans and consumes.
        crate::autocmd::fire_filetype(session, buffer, "python")?;
        assert_eq!(definition_count(session, inner), 0);
        Ok(())
    });
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: Some(reenter),
        }),
        Box::new(ActionRecorder::default()),
    );

    set_filetype(&session, "filetype", "rust", &[]).unwrap();

    assert_eq!(actions.borrow().len(), 1);
    assert_eq!(definition_count(&session, outer), 0);
    assert_eq!(
        session.with_editor(|editor| { editor.options().get_buffer(buffer, "filetype").cloned() }),
        Ok(OptionValue::String("rust".to_owned()))
    );
}

#[test]
fn failing_handler_keeps_the_assignment_and_the_consumed_once() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let id = filetype_autocmd(&session, "rust", true);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let reenter = Rc::new(|_: &crate::ApiSession| Err(ApiError::exception("boom")));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: Some(reenter),
        }),
        Box::new(ActionRecorder::default()),
    );

    let result = set_filetype(&session, "filetype", "rust", &[]);

    assert_eq!(result, Err(ApiError::exception("boom")));
    assert_eq!(actions.borrow().len(), 1);
    assert_eq!(
        session.with_editor(|editor| { editor.options().get_buffer(buffer, "filetype").cloned() }),
        Ok(OptionValue::String("rust".to_owned()))
    );
    assert_eq!(definition_count(&session, id), 0);
}

#[test]
fn dry_run_filetype_write_neither_fires_nor_consumes() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let id = filetype_autocmd(&session, "rust", true);
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    let assigned = set_filetype(
        &session,
        "filetype",
        "rust",
        &[("dry_run", Object::Boolean(true))],
    )
    .unwrap();

    assert_eq!(assigned, Object::String(OxStr::from("rust")));
    assert!(actions.borrow().is_empty());
    assert_eq!(definition_count(&session, id), 1);

    set_filetype(&session, "filetype", "rust", &[]).unwrap();
    assert_eq!(actions.borrow().len(), 1);
    assert_eq!(definition_count(&session, id), 0);
}

#[test]
fn nonmatching_filetype_write_does_not_consume_once() {
    let (mut editor, current, _, _) = editor_with_lines(&["one"]);
    let target = editor
        .create_buffer_with(Buffer::from_lines(&[b"two".to_vec()], false).unwrap(), true)
        .unwrap();
    // `<abuf>` registers a genuinely buffer-local definition, bound to the
    // first buffer only.
    editor
        .autocmds_mut()
        .register_legacy(
            &[Event::FileType],
            "<abuf>",
            &AutocmdKind::ExString("echo fired".to_owned()),
            &AutocmdOptions {
                buffer: Some(current),
                once: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();
    let session = session_with(editor);
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    // The same value on another buffer matches nothing and must not consume.
    set_filetype(
        &session,
        "filetype",
        "python",
        &[("buf", Object::Integer(i64::from(target)))],
    )
    .unwrap();
    assert!(actions.borrow().is_empty());
    assert!(session.with_editor(|editor| {
        editor
            .autocmds()
            .definitions()
            .iter()
            .any(|item| item.event == Event::FileType)
    }));

    set_filetype(
        &session,
        "filetype",
        "python",
        &[("buf", Object::Integer(i64::from(current)))],
    )
    .unwrap();
    assert_eq!(actions.borrow().len(), 1);
    // The matching write consumed the `++once` definition at execution.
    assert!(session.with_editor(|editor| {
        editor
            .autocmds()
            .definitions()
            .iter()
            .all(|item| item.event != Event::FileType)
    }));
}

#[test]
fn invalid_filetype_targets_never_fire() {
    let (editor, _, _, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    filetype_autocmd(&session, "rust", false);
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    assert!(
        set_filetype(
            &session,
            "filetype",
            "rust",
            &[("win", Object::Integer(i64::from(window)))]
        )
        .is_err()
    );
    assert!(
        set_filetype(
            &session,
            "filetype",
            "rust",
            &[("scope", Object::String(OxStr::from("global")))]
        )
        .is_err()
    );
    assert!(actions.borrow().is_empty());
}

#[test]
fn exec_autocmds_consumes_once_before_the_host_runs() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let id = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("pattern", Object::String(OxStr::from("*"))),
            ("command", Object::String(OxStr::from("echo hi"))),
            ("once", Object::Boolean(true)),
        ]),
    )
    .unwrap();
    // No autocmd host installed: the `++once` definition is still consumed
    // when the executor loop begins the action.
    crate::autocmd::nvim_exec_autocmds(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[]),
    )
    .unwrap();
    assert_eq!(definition_count(&session, id), 0);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered setup/query/tombstone scenario verifies optional-field omission across API and legacy autocmds"
)]
fn get_autocmds_serializes_optional_fields_only_when_present() {
    let (mut editor, buffer, _, _) = editor_with_lines(&["one"]);
    // A legacy `:autocmd` entry carries no API id.
    editor
        .autocmds_mut()
        .register_legacy(
            &[Event::BufEnter],
            "*",
            &AutocmdKind::ExString("legacy".to_owned()),
            &AutocmdOptions::default(),
        )
        .unwrap();
    let session = session_with(editor);
    let group_id = crate::autocmd::nvim_create_augroup(
        &session,
        OxStr::from("mine"),
        dict(&[("clear", Object::Boolean(false))]),
    )
    .unwrap();
    let with_id = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("group", Object::Integer(group_id)),
            ("desc", Object::String(OxStr::from("described"))),
            ("callback", Object::LuaRef(41)),
            ("buffer", Object::Integer(i64::from(buffer))),
        ]),
    )
    .unwrap();
    let grouped =
        crate::autocmd::nvim_get_autocmds(&session, dict(&[("group", Object::Integer(group_id))]))
            .unwrap();
    assert_eq!(grouped.len(), 1);
    let described = &grouped[0];
    let keys = described
        .iter()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys,
        [
            "buf",
            "buflocal",
            "buffer",
            "callback",
            "command",
            "desc",
            "event",
            "group",
            "group_name",
            "id",
            "once",
            "pattern"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    assert_eq!(
        described.get(&OxStr::from("id")),
        Some(&Object::Integer(with_id))
    );
    assert_eq!(
        described.get(&OxStr::from("callback")),
        Some(&Object::LuaRef(41))
    );
    assert_eq!(
        described.get(&OxStr::from("command")),
        Some(&Object::String(OxStr::from("")))
    );
    assert_eq!(
        described.get(&OxStr::from("buflocal")),
        Some(&Object::Boolean(true))
    );
    assert_eq!(
        described.get(&OxStr::from("buf")),
        Some(&Object::Integer(i64::from(buffer)))
    );
    assert_eq!(
        described.get(&OxStr::from("buffer")),
        Some(&Object::Integer(i64::from(buffer)))
    );
    assert_eq!(
        described.get(&OxStr::from("group")),
        Some(&Object::Integer(group_id))
    );
    assert_eq!(
        described.get(&OxStr::from("group_name")),
        Some(&Object::String(OxStr::from("mine")))
    );

    let all = crate::autocmd::nvim_get_autocmds(&session, dict(&[])).unwrap();
    let legacy = all
        .iter()
        .find(|entry| {
            entry.get(&OxStr::from("command")) == Some(&Object::String(OxStr::from("legacy")))
        })
        .unwrap();
    let keys = legacy
        .iter()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect::<std::collections::BTreeSet<_>>();
    // The legacy entry omits `id` and group metadata, and every optional
    // field it never specified stays absent.
    assert_eq!(
        keys,
        ["buflocal", "command", "event", "once", "pattern"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    assert_eq!(
        legacy.get(&OxStr::from("buflocal")),
        Some(&Object::Boolean(false))
    );

    // A legacy-deleted group keeps serializing group metadata under the
    // tombstone name on a global query.
    session.with_editor_mut(|editor| {
        editor
            .autocmds_mut()
            .delete_group_legacy(ox_editor::AugroupId(u64::try_from(group_id).unwrap()))
            .unwrap();
    });
    let tombstoned = crate::autocmd::nvim_get_autocmds(&session, dict(&[])).unwrap();
    let tombstoned = tombstoned
        .iter()
        .find(|entry| entry.get(&OxStr::from("id")) == Some(&Object::Integer(with_id)))
        .unwrap();
    assert_eq!(
        tombstoned.get(&OxStr::from("group")),
        Some(&Object::Integer(group_id))
    );
    assert_eq!(
        tombstoned.get(&OxStr::from("group_name")),
        Some(&Object::String(OxStr::from("--Deleted--")))
    );
}

#[test]
fn callback_args_carry_group_only_for_named_groups() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let group_id = crate::autocmd::nvim_create_augroup(
        &session,
        OxStr::from("ArgsGroup"),
        dict(&[("clear", Object::Boolean(false))]),
    )
    .unwrap();
    let grouped = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("User")),
        dict(&[
            ("group", Object::Integer(group_id)),
            ("pattern", Object::String(OxStr::from("One"))),
            ("callback", Object::LuaRef(51)),
        ]),
    )
    .unwrap();
    let default = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("User")),
        dict(&[
            ("pattern", Object::String(OxStr::from("Two"))),
            ("callback", Object::LuaRef(52)),
        ]),
    )
    .unwrap();
    let actions = Rc::new(RefCell::new(Vec::new()));
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: None,
        }),
        Box::new(ActionRecorder::default()),
    );

    crate::autocmd::nvim_exec_autocmds(
        &session,
        Object::String(OxStr::from("User")),
        dict(&[("pattern", Object::String(OxStr::from("One")))]),
    )
    .unwrap();
    crate::autocmd::nvim_exec_autocmds(
        &session,
        Object::String(OxStr::from("User")),
        dict(&[("pattern", Object::String(OxStr::from("Two")))]),
    )
    .unwrap();

    let actions = actions.borrow();
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0].api_id, Some(grouped.cast_unsigned()));
    assert_eq!(actions[1].api_id, Some(default.cast_unsigned()));
    let grouped_callback_args = actions[0].callback_args().unwrap();
    let [Object::Dict(grouped_args)] = grouped_callback_args.as_slice() else {
        panic!("expected one callback dictionary");
    };
    // Named groups keep the numeric group id in the callback args.
    assert_eq!(
        grouped_args.get(&OxStr::from("group")),
        Some(&Object::Integer(group_id))
    );
    let default_callback_args = actions[1].callback_args().unwrap();
    let [Object::Dict(default_args)] = default_callback_args.as_slice() else {
        panic!("expected one callback dictionary");
    };
    // The default group omits `group` entirely (upstream key absence).
    assert_eq!(default_args.get(&OxStr::from("group")), None);
}

#[test]
fn query_event_type_errors_match_upstream_text() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(&session, dict(&[("event", Object::Boolean(true))])),
        Err(ApiError::validation(
            "Invalid 'event': expected String or Array"
        )),
    );
    assert_eq!(
        crate::autocmd::nvim_clear_autocmds(&session, dict(&[("event", Object::Boolean(true))])),
        Err(ApiError::validation(
            "Invalid 'event': expected Array or String, got Boolean"
        )),
    );
    // Array item errors keep the create-style wording.
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(
            &session,
            dict(&[("event", Object::Array(vec![Object::Integer(7)]))]),
        ),
        Err(ApiError::validation(
            "Invalid 'event' item: expected String, got Integer"
        )),
    );

    session.with_editor_mut(|editor| {
        editor
            .autocmds_mut()
            .register_legacy(
                &[Event::BufEnter],
                "*",
                &AutocmdKind::ExString("legacy".to_owned()),
                &AutocmdOptions::default(),
            )
            .unwrap();
    });
    // An explicit empty event list still selects nothing instead of failing,
    // and string queries keep working.
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(&session, dict(&[("event", Object::Array(vec![]))])),
        Ok(Vec::new()),
    );
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(
            &session,
            dict(&[("event", Object::String(OxStr::from("BufEnter")))])
        )
        .unwrap()
        .len(),
        1,
    );
}

#[test]
fn deleted_augroup_ids_report_invalid_group_not_e367() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let group_id = crate::autocmd::nvim_create_augroup(
        &session,
        OxStr::from("Gone"),
        dict(&[("clear", Object::Boolean(false))]),
    )
    .unwrap();
    crate::autocmd::nvim_del_augroup_by_id(&session, group_id).unwrap();
    let invalid = |raw: i64| ApiError::validation(format!("Invalid 'group': {raw}"));

    // Deletion itself keeps the E367 form for unknown groups.
    assert_eq!(
        crate::autocmd::nvim_del_augroup_by_id(&session, 9_997_999),
        Err(ApiError::exception("Vim:E367: No such group: \"[NULL]\"")),
    );
    // A deleted id used as a query/create/exec/clear group reports the
    // invalid-group validation form, never E367.
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(&session, dict(&[("group", Object::Integer(group_id))])),
        Err(invalid(group_id)),
    );
    assert_eq!(
        crate::autocmd::nvim_create_autocmd(
            &session,
            Object::String(OxStr::from("FileType")),
            dict(&[
                ("group", Object::Integer(group_id)),
                ("pattern", Object::String(OxStr::from("*"))),
                ("command", Object::String(OxStr::from("echo hello"))),
            ]),
        ),
        Err(invalid(group_id)),
    );
    assert_eq!(
        crate::autocmd::nvim_exec_autocmds(
            &session,
            Object::String(OxStr::from("FileType")),
            dict(&[("group", Object::Integer(group_id))]),
        ),
        Err(invalid(group_id)),
    );
    assert_eq!(
        crate::autocmd::nvim_clear_autocmds(
            &session,
            dict(&[("group", Object::Integer(group_id))])
        ),
        Err(invalid(group_id)),
    );
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(&session, dict(&[("group", Object::Integer(9_997_999))])),
        Err(invalid(9_997_999)),
    );

    // A legacy `:augroup!` tombstones the group; its id is still rejected.
    let legacy_group = crate::autocmd::nvim_create_augroup(
        &session,
        OxStr::from("Legacy"),
        dict(&[("clear", Object::Boolean(false))]),
    )
    .unwrap();
    session.with_editor_mut(|editor| {
        editor
            .autocmds_mut()
            .delete_group_legacy(ox_editor::AugroupId(u64::try_from(legacy_group).unwrap()))
            .unwrap();
    });
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(
            &session,
            dict(&[("group", Object::Integer(legacy_group))])
        ),
        Err(invalid(legacy_group)),
    );
}

#[test]
fn clear_autocmds_by_buf_keeps_unrelated_entries() {
    let (mut editor, current, _, _) = editor_with_lines(&["one"]);
    let other = editor
        .create_buffer_with(Buffer::from_lines(&[b"two".to_vec()], false).unwrap(), true)
        .unwrap();
    let session = session_with(editor);
    let buffer_local = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("command", Object::String(OxStr::from("buflocal"))),
            ("buffer", Object::Integer(i64::from(current))),
        ]),
    )
    .unwrap();
    let other_buffer_local = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("command", Object::String(OxStr::from("other"))),
            ("buffer", Object::Integer(i64::from(other))),
        ]),
    )
    .unwrap();
    let global = crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("pattern", Object::String(OxStr::from("*.rs"))),
            ("command", Object::String(OxStr::from("global"))),
        ]),
    )
    .unwrap();

    crate::autocmd::nvim_clear_autocmds(&session, dict(&[("buf", Object::Integer(0))])).unwrap();

    assert_eq!(definition_count(&session, buffer_local), 0);
    let mut ids: Vec<i64> = crate::autocmd::nvim_get_autocmds(&session, dict(&[]))
        .unwrap()
        .iter()
        .filter_map(|entry| match entry.get(&OxStr::from("id")) {
            Some(Object::Integer(id)) => Some(*id),
            _ => None,
        })
        .collect();
    ids.sort_unstable();
    let mut expected = vec![other_buffer_local, global];
    expected.sort_unstable();
    assert_eq!(ids, expected);
}

#[test]
fn extmark_details_order_limit_delete_and_clear() {
    let (editor, buffer, _, _) = editor_with_lines(&["one", "two"]);
    let session = session_with(editor);
    let namespace = crate::extmark::nvim_create_namespace(&session, OxStr::from("tests")).unwrap();
    let first = crate::extmark::nvim_buf_set_extmark(
        &session,
        buffer,
        namespace,
        0,
        1,
        dict(&[
            ("right_gravity", Object::Boolean(false)),
            ("end_row", Object::Integer(1)),
            ("end_col", Object::Integer(2)),
            ("hl_group", Object::String(OxStr::from("Visual"))),
        ]),
    )
    .unwrap();
    let second =
        crate::extmark::nvim_buf_set_extmark(&session, buffer, namespace, 1, 0, dict(&[])).unwrap();
    let marks = crate::extmark::nvim_buf_get_extmarks(
        &session,
        buffer,
        namespace,
        Object::Array(vec![Object::Integer(0), Object::Integer(0)]),
        Object::Integer(-1),
        dict(&[
            ("details", Object::Boolean(true)),
            ("limit", Object::Integer(1)),
        ]),
    )
    .unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0][0], Object::Integer(first));
    let Object::Dict(details) = &marks[0][3] else {
        panic!("missing details")
    };
    assert_eq!(
        details.get(&OxStr::from("right_gravity")),
        Some(&Object::Boolean(false))
    );
    assert!(crate::extmark::nvim_buf_del_extmark(&session, buffer, namespace, second).unwrap());
    crate::extmark::nvim_buf_clear_namespace(&session, buffer, namespace, 0, -1).unwrap();
    assert!(
        crate::extmark::nvim_buf_get_extmark_by_id(&session, buffer, namespace, first, dict(&[]))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn extmark_stacked_hl_group_roundtrips() {
    let (editor, buffer, _, _) = editor_with_lines(&["text"]);
    let session = session_with(editor);
    let namespace = crate::extmark::nvim_create_namespace(&session, OxStr::from("test")).unwrap();
    // Array form.
    let id = crate::extmark::nvim_buf_set_extmark(
        &session,
        buffer,
        namespace,
        0,
        0,
        dict(&[(
            "hl_group",
            Object::Array(vec![
                Object::String(OxStr::from("A")),
                Object::String(OxStr::from("B")),
            ]),
        )]),
    )
    .unwrap();
    let marks = crate::extmark::nvim_buf_get_extmarks(
        &session,
        buffer,
        namespace,
        Object::Integer(0),
        Object::Integer(-1),
        dict(&[("details", Object::Boolean(true))]),
    )
    .unwrap();
    let Object::Dict(details) = &marks[0][3] else {
        panic!("missing details")
    };
    assert_eq!(
        details.get(&OxStr::from("hl_group")),
        Some(&Object::Array(vec![
            Object::String(OxStr::from("A")),
            Object::String(OxStr::from("B"))
        ]))
    );
    // String form stays string.
    crate::extmark::nvim_buf_del_extmark(&session, buffer, namespace, id).unwrap();
    crate::extmark::nvim_buf_set_extmark(
        &session,
        buffer,
        namespace,
        0,
        0,
        dict(&[("hl_group", Object::String(OxStr::from("Single")))]),
    )
    .unwrap();
    let marks = crate::extmark::nvim_buf_get_extmarks(
        &session,
        buffer,
        namespace,
        Object::Integer(0),
        Object::Integer(-1),
        dict(&[("details", Object::Boolean(true))]),
    )
    .unwrap();
    let Object::Dict(details) = &marks[0][3] else {
        panic!("missing details")
    };
    assert_eq!(
        details.get(&OxStr::from("hl_group")),
        Some(&Object::String(OxStr::from("Single")))
    );
}

#[test]
fn extmark_decoration_provider_accepts_internal_underscore_keys() {
    let session = session();
    let namespace = crate::extmark::nvim_create_namespace(&session, OxStr::from("tests")).unwrap();
    // Upstream stores the internal `_on_*` hooks alongside the public `on_*`
    // callbacks (extmark.c:1075-1085); this port stores them too, with
    // invocation landing when the corresponding redraw events exist.
    crate::extmark::nvim_set_decoration_provider(
        &session,
        namespace,
        dict(&[
            ("_on_hl_def", Object::LuaRef(11)),
            ("_on_spell_nav", Object::LuaRef(12)),
            ("_on_conceal_line", Object::LuaRef(13)),
        ]),
    )
    .unwrap();
    // The non-underscore spelling is not an accepted key upstream.
    assert_eq!(
        crate::extmark::nvim_set_decoration_provider(
            &session,
            namespace,
            dict(&[("on_hl_def", Object::LuaRef(14))]),
        ),
        Err(ApiError::validation("unexpected key: on_hl_def"))
    );
}

/// A Lua host that records every released callback reference.
#[derive(Default)]
struct ReleasingLua {
    released: Rc<RefCell<Vec<usize>>>,
}

impl crate::LuaExecutor for ReleasingLua {
    fn exec(
        &mut self,
        _session: &crate::ApiSession,
        _code: &str,
        _args: Vec<Object>,
    ) -> Result<Object, String> {
        Ok(Object::Nil)
    }

    fn invoke_callback(
        &mut self,
        _session: &crate::ApiSession,
        _reference: usize,
        args: Vec<Object>,
    ) -> Result<Object, String> {
        Ok(Object::Array(args))
    }

    fn call_ref(
        &mut self,
        _session: &crate::ApiSession,
        _reference: usize,
        args: Vec<Object>,
    ) -> Result<Vec<Object>, String> {
        Ok(args)
    }

    fn free_callback(&mut self, reference: usize) -> Result<(), String> {
        self.released.borrow_mut().push(reference);
        Ok(())
    }
}

#[test]
fn decoration_provider_failure_releases_incoming_references() {
    let session = session();
    let released = Rc::new(RefCell::new(Vec::new()));
    crate::set_lua_executor(
        &session,
        Box::new(ReleasingLua {
            released: Rc::clone(&released),
        }),
        Box::new(ReleasingLua {
            released: Rc::clone(&released),
        }),
    );
    let namespace = crate::extmark::nvim_create_namespace(&session, OxStr::from("tests")).unwrap();

    // A valid callback ahead of an invalid value: the parsed ref is released
    // even though validation stopped first (extmark.c frees the request's
    // refs on every failure before ownership moves into the provider).
    assert_eq!(
        crate::extmark::nvim_set_decoration_provider(
            &session,
            namespace,
            dict(&[
                ("on_line", Object::LuaRef(41)),
                ("on_buf", Object::Integer(7)),
            ]),
        ),
        Err(ApiError::validation(
            "Invalid value for 'on_buf': expected Lua function reference"
        ))
    );
    assert_eq!(&*released.borrow(), &[41]);

    // An invalid key ahead of a valid callback: the bridge acquired the
    // callback's registry slot before validation ran, so it is released too
    // and repeated failed calls never accumulate registry references.
    assert_eq!(
        crate::extmark::nvim_set_decoration_provider(
            &session,
            namespace,
            dict(&[
                ("unexpected", Object::Integer(0)),
                ("on_line", Object::LuaRef(42)),
            ]),
        ),
        Err(ApiError::validation("unexpected key: unexpected"))
    );
    assert_eq!(&*released.borrow(), &[41, 42]);

    // A failed call leaves a live definition untouched: ref 43 stays owned
    // by the provider while only the failed call's ref 44 is released.
    crate::extmark::nvim_set_decoration_provider(
        &session,
        namespace,
        dict(&[("on_line", Object::LuaRef(43))]),
    )
    .unwrap();
    assert_eq!(&*released.borrow(), &[41, 42]);
    assert_eq!(
        crate::extmark::nvim_set_decoration_provider(
            &session,
            namespace,
            dict(&[
                ("on_line", Object::LuaRef(44)),
                ("on_win", Object::Integer(1)),
            ]),
        ),
        Err(ApiError::validation(
            "Invalid value for 'on_win': expected Lua function reference"
        ))
    );
    assert_eq!(&*released.borrow(), &[41, 42, 44]);
    let providers = session.with_editor(|editor| {
        let ids = editor
            .decorations()
            .phase_provider_ids(ox_editor::decoration::CallbackPhase::Line);
        assert_eq!(ids.len(), 1);
        ids[0]
    });
    assert_eq!(
        session.with_editor(|editor| editor
            .decorations()
            .phase_callback(providers, ox_editor::decoration::CallbackPhase::Line)),
        Some(43)
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one extmark lifecycle scenario compares explicit, default, corrupted, and filtered sign details"
)]
fn extmark_sign_details_match_neovim() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let namespace = crate::extmark::nvim_create_namespace(&session, OxStr::from("signs")).unwrap();
    let id = crate::extmark::nvim_buf_set_extmark(
        &session,
        buffer,
        namespace,
        0,
        0,
        dict(&[
            ("sign_text", Object::String(OxStr::from(">>"))),
            ("sign_hl_group", Object::String(OxStr::from("Statement"))),
            ("number_hl_group", Object::String(OxStr::from("Statement"))),
            ("line_hl_group", Object::String(OxStr::from("Statement"))),
            (
                "cursorline_hl_group",
                Object::String(OxStr::from("Statement")),
            ),
            ("priority", Object::Integer(0)),
        ]),
    )
    .unwrap();
    let result = crate::extmark::nvim_buf_get_extmark_by_id(
        &session,
        buffer,
        namespace,
        id,
        dict(&[("details", Object::Boolean(true))]),
    )
    .unwrap();
    let Object::Dict(details) = &result[2] else {
        panic!("missing details")
    };
    assert_eq!(
        details.get(&OxStr::from("sign_text")),
        Some(&Object::String(OxStr::from(">>")))
    );
    assert_eq!(
        details.get(&OxStr::from("sign_hl_group")),
        Some(&Object::String(OxStr::from("Statement")))
    );
    assert_eq!(
        details.get(&OxStr::from("number_hl_group")),
        Some(&Object::String(OxStr::from("Statement")))
    );
    assert_eq!(
        details.get(&OxStr::from("line_hl_group")),
        Some(&Object::String(OxStr::from("Statement")))
    );
    assert_eq!(
        details.get(&OxStr::from("cursorline_hl_group")),
        Some(&Object::String(OxStr::from("Statement")))
    );
    assert_eq!(
        details.get(&OxStr::from("priority")),
        Some(&Object::Integer(0))
    );
    assert!(details.get(&OxStr::from("sign_name")).is_none());
    assert!(details.get(&OxStr::from("invalidate")).is_none());
    assert!(details.get(&OxStr::from("undo_restore")).is_none());

    let default_id = crate::extmark::nvim_buf_set_extmark(
        &session,
        buffer,
        namespace,
        0,
        0,
        dict(&[(
            "cursorline_hl_group",
            Object::String(OxStr::from("Statement")),
        )]),
    )
    .unwrap();
    let result = crate::extmark::nvim_buf_get_extmark_by_id(
        &session,
        buffer,
        namespace,
        default_id,
        dict(&[("details", Object::Boolean(true))]),
    )
    .unwrap();
    let Object::Dict(details) = &result[2] else {
        panic!("missing details")
    };
    assert_eq!(
        details.get(&OxStr::from("cursorline_hl_group")),
        Some(&Object::String(OxStr::from("Statement")))
    );
    assert_eq!(
        details.get(&OxStr::from("priority")),
        Some(&Object::Integer(0x1000))
    );

    let ns = ox_editor::NamespaceId::new(u32::try_from(namespace).unwrap()).unwrap();
    let eid = ox_editor::ExtmarkId::new(u32::try_from(id).unwrap()).unwrap();
    let mut placement = session.with_editor(|editor| {
        editor
            .buffer(buffer)
            .unwrap()
            .extmarks
            .get(ns, eid)
            .unwrap()
            .unwrap()
            .placement
            .clone()
    });
    placement.attributes.sign_name = Some("sign1".into());
    placement
        .attributes
        .flags
        .set(ox_editor::ExtmarkFlags::INVALIDATE, true);
    placement
        .attributes
        .flags
        .set(ox_editor::ExtmarkFlags::UNDO_RESTORE, false);
    session.with_editor_mut(|editor| {
        editor
            .buffer_mut(buffer)
            .unwrap()
            .extmarks
            .set(ns, Some(eid), placement)
            .unwrap()
    });
    let result = crate::extmark::nvim_buf_get_extmark_by_id(
        &session,
        buffer,
        namespace,
        id,
        dict(&[("details", Object::Boolean(true))]),
    )
    .unwrap();
    let Object::Dict(details) = &result[2] else {
        panic!("missing details")
    };
    assert_eq!(
        details.get(&OxStr::from("sign_name")),
        Some(&Object::String(OxStr::from("sign1")))
    );
    assert_eq!(
        details.get(&OxStr::from("invalidate")),
        Some(&Object::Boolean(true))
    );
    assert_eq!(
        details.get(&OxStr::from("undo_restore")),
        Some(&Object::Boolean(false))
    );

    let typed = crate::extmark::nvim_buf_get_extmarks(
        &session,
        buffer,
        namespace,
        Object::Integer(0),
        Object::Integer(-1),
        dict(&[("type", Object::String(OxStr::from("sign")))]),
    )
    .unwrap();
    assert!(!typed.is_empty());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the validation matrix shares one editor, namespace, and buffer to pin related extmark diagnostics"
)]
fn extmark_argument_bound_namespace_and_filter_diagnostics_match_api_contract() {
    let (editor, buffer, _, _) = editor_with_lines(&["12345"]);
    let session = session_with(editor);
    let namespace =
        crate::extmark::nvim_create_namespace(&session, OxStr::from("validation")).unwrap();
    let invalid_ns = namespace + 1;

    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(&session, buffer, invalid_ns, 0, 0, dict(&[]))
            .unwrap_err()
            .message(),
        format!("Invalid 'ns_id': {invalid_ns}")
    );
    assert_eq!(
        crate::extmark::nvim_buf_del_extmark(&session, buffer, invalid_ns, 1)
            .unwrap_err()
            .message(),
        format!("Invalid 'ns_id': {invalid_ns}")
    );
    assert_eq!(
        crate::extmark::nvim_buf_get_extmarks(
            &session,
            buffer,
            invalid_ns,
            Object::Integer(0),
            Object::Integer(-1),
            dict(&[]),
        )
        .unwrap_err()
        .message(),
        format!("Invalid 'ns_id': {invalid_ns}")
    );
    assert_eq!(
        crate::extmark::nvim_buf_get_extmark_by_id(&session, buffer, invalid_ns, 1, dict(&[]))
            .unwrap_err()
            .message(),
        format!("Invalid 'ns_id': {invalid_ns}")
    );

    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[
                ("id", Object::Array(Vec::new())),
                ("end_col", Object::Integer(1)),
                ("end_row", Object::Integer(1))
            ]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'id': expected Integer, got Array"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[("id", Object::Integer(0))]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'id': expected positive Integer"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[
                ("end_col", Object::Array(Vec::new())),
                ("end_row", Object::Integer(1))
            ]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'end_col': expected Integer, got Array"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[
                ("end_col", Object::Integer(1)),
                ("end_row", Object::Array(Vec::new()))
            ]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'end_row': expected Integer, got Array"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[("virt_text_pos", Object::Integer(0))]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'virt_text_pos': expected String, got Integer"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[("virt_text_pos", Object::String(OxStr::from("foo")))]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'virt_text_pos': 'foo'"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[
                ("virt_text_pos", Object::String(OxStr::from("foo"))),
                ("virt_text_win_col", Object::Integer(5)),
            ]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'virt_text_pos': 'foo'"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[("hl_mode", Object::Integer(0))]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'hl_mode': expected String, got Integer"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[("hl_mode", Object::String(OxStr::from("foo")))]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'hl_mode': 'foo'"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[("virt_lines_overflow", Object::String(OxStr::from("foo")))]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'virt_lines_overflow': 'foo'"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[
                ("end_row", Object::Integer(0)),
                ("end_line", Object::Integer(0))
            ]),
        )
        .unwrap_err()
        .message(),
        "cannot use both 'end_row' and 'end_line'"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[("end_right_gravity", Object::Boolean(true))]),
        )
        .unwrap_err()
        .message(),
        "cannot set end_right_gravity without end_row or end_col"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[("priority", Object::Integer(-1))]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'priority': out of range"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(&session, buffer, namespace, 0, 6, dict(&[]))
            .unwrap_err()
            .message(),
        "Invalid 'col': out of range"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(&session, buffer, namespace, 3, 6, dict(&[]))
            .unwrap_err()
            .message(),
        "Invalid 'line': out of range"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[
                ("end_col", Object::Integer(1)),
                ("end_row", Object::Integer(1))
            ]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'end_col': out of range"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[
                ("end_col", Object::Integer(-1)),
                ("end_row", Object::Integer(0))
            ]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'end_col': out of range"
    );

    assert_eq!(
        crate::extmark::nvim_buf_get_extmarks(
            &session,
            buffer,
            namespace,
            Object::Array(Vec::new()),
            Object::Array(vec![Object::Integer(-1), Object::Integer(-1)]),
            dict(&[]),
        )
        .unwrap_err()
        .message(),
        "Invalid mark position: expected 2 Integer items"
    );
    assert_eq!(
        crate::extmark::nvim_buf_get_extmarks(
            &session,
            buffer,
            namespace,
            Object::Boolean(true),
            Object::Array(vec![Object::Integer(-1), Object::Integer(-1)]),
            dict(&[]),
        )
        .unwrap_err()
        .message(),
        "Invalid mark position: expected mark id Integer or 2-item Array"
    );
    assert_eq!(
        crate::extmark::nvim_buf_get_extmarks(
            &session,
            buffer,
            namespace,
            Object::Integer(-2),
            Object::Integer(-1),
            dict(&[]),
        )
        .unwrap_err()
        .message(),
        "Invalid mark id: -2"
    );
    assert_eq!(
        crate::extmark::nvim_buf_get_extmarks(
            &session,
            buffer,
            namespace,
            Object::Integer(99),
            Object::Integer(-1),
            dict(&[]),
        )
        .unwrap_err()
        .message(),
        "Invalid mark id (not found): 99"
    );
    assert_eq!(
        crate::extmark::nvim_buf_get_extmarks(
            &session,
            buffer,
            namespace,
            Object::Integer(0),
            Object::Integer(-1),
            dict(&[("type", Object::String(OxStr::from("bogus")))]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'type': expected sign, virt_text, virt_lines or highlight, got bogus"
    );
    assert_eq!(
        crate::extmark::nvim_buf_get_extmarks(
            &session,
            buffer,
            namespace,
            Object::Integer(0),
            Object::Integer(-1),
            dict(&[("limit", Object::Boolean(true))]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'limit': expected Integer, got Boolean"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            namespace,
            0,
            0,
            dict(&[("virt_lines", Object::Integer(1))]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'virt_lines': expected Array, got Integer"
    );
}

#[test]
fn extmark_boolean_option_diagnostic_matches_api_contract() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let namespace = crate::extmark::nvim_create_namespace(&session, OxStr::from("tests")).unwrap();
    let error = crate::extmark::nvim_buf_set_extmark(
        &session,
        buffer,
        namespace,
        0,
        0,
        dict(&[("right_gravity", Object::String(OxStr::from("invalid")))]),
    )
    .unwrap_err();

    assert_eq!(error.message(), "Invalid 'right_gravity': expected boolean");
}

#[test]
fn context_round_trip_restores_gvars() {
    let (mut editor, _, _, _) = editor_with_lines(&["one"]);
    editor
        .gvars_mut()
        .0
        .push((OxStr::from("answer"), Object::Integer(42)));
    let session = session_with(editor);
    let context = crate::context::nvim_get_context(
        &session,
        dict(&[(
            "types",
            Object::Array(vec![
                Object::String(OxStr::from("gvars")),
                Object::String(OxStr::from("bufs")),
            ]),
        )]),
    )
    .unwrap();
    session.with_editor_mut(|editor| editor.gvars_mut().0.clear());
    crate::context::nvim_load_context(&session, context).unwrap();
    assert_eq!(
        session.with_editor(|editor| editor.gvars().get(&OxStr::from("answer")).cloned()),
        Some(Object::Integer(42))
    );
}
#[test]
fn client_info_round_trip_reports_registered_channel() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let _stdio_caller = session.enter_rpc_call(ox_rpc::ChannelId::new(1));
    crate::channel::nvim_set_client_info(
        &session,
        OxStr::from("tests"),
        dict(&[("major", Object::Integer(1))]),
        OxStr::from("remote"),
        dict(&[]),
        dict(&[]),
    )
    .unwrap();
    let info = crate::channel::nvim_get_chan_info(&session, 1).unwrap();
    assert_eq!(
        info.iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<std::collections::BTreeSet<_>>(),
        ["client", "id", "mode", "stream"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    assert!(
        crate::channel::nvim_set_client_info(
            &session,
            OxStr::from("bad"),
            dict(&[]),
            OxStr::from("invalid"),
            dict(&[]),
            dict(&[])
        )
        .is_err()
    );
}

#[test]
fn ui_attach_and_highlight_round_trip_matches_named_colors() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    crate::ui::nvim_ui_attach(
        &session,
        80,
        24,
        dict(&[
            ("ext_linegrid", Object::Boolean(true)),
            ("ext_hlstate", Object::Boolean(true)),
        ]),
    )
    .unwrap();
    assert_eq!(crate::ui::nvim_list_uis(&session).unwrap().len(), 1);
    crate::ui::nvim_set_hl(
        &session,
        0,
        OxStr::from("Task11b"),
        dict(&[
            ("fg", Object::Integer(0x0011_2233)),
            ("bold", Object::Boolean(true)),
        ]),
    )
    .unwrap();
    let highlight = crate::ui::nvim_get_hl(
        &session,
        0,
        dict(&[("name", Object::String(OxStr::from("Task11b")))]),
    )
    .unwrap();
    assert_eq!(
        highlight.get(&OxStr::from("fg")),
        Some(&Object::Integer(0x0011_2233))
    );
    crate::ui::nvim_set_hl(
        &session,
        0,
        OxStr::from("Named"),
        dict(&[
            ("fg", Object::String(OxStr::from("LightGrey"))),
            ("bg", Object::String(OxStr::from("DarkGrey"))),
            ("ctermfg", Object::String(OxStr::from("White"))),
        ]),
    )
    .unwrap();
    let named = crate::ui::nvim_get_hl(
        &session,
        0,
        dict(&[("name", Object::String(OxStr::from("Named")))]),
    )
    .unwrap();
    assert_eq!(
        named.get(&OxStr::from("fg")),
        Some(&Object::Integer(0x00d3_d3d3))
    );
    assert_eq!(
        named.get(&OxStr::from("bg")),
        Some(&Object::Integer(0x00a9_a9a9))
    );
    crate::ui::nvim_set_hl_ns(&session, 9).unwrap();
    assert_eq!(crate::ui::nvim_get_hl_ns(&session, dict(&[])), Ok(9));
}

#[test]
fn new_family_metadata_and_dispatch_are_registered() {
    let registry = crate::core().unwrap();
    for (name, since, deprecated) in [
        ("nvim_create_autocmd", 9, None),
        ("nvim_buf_set_extmark", 7, None),
        ("nvim_get_context", 6, None),
        ("nvim_ui_attach", 1, None),
        ("nvim_buf_get_number", 1, Some(2)),
    ] {
        let metadata = registry.get(name).unwrap().0;
        assert_eq!(
            (metadata.since, metadata.deprecated_since),
            (since, deprecated)
        );
    }
    let session = session();
    let dispatch = registry.get("nvim_create_namespace").unwrap().1;
    assert_eq!(
        dispatch(&session, &[Object::String(OxStr::from("dispatch"))]),
        Ok(Object::Integer(1))
    );
    let dispatch = registry.get("nvim_list_uis").unwrap().1;
    assert_eq!(dispatch(&session, &[]), Ok(Object::Array(Vec::new())));
}

#[test]
fn open_win_split_creates_tiled_window() {
    let (editor, buffer, _, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let split = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[("split", Object::String(OxStr::from("right")))]),
    )
    .unwrap();
    assert_ne!(split, window);
    // Tiled windows carry no float config: `relative` is empty.
    let config = crate::window::nvim_win_get_config(&session, split).unwrap();
    assert_eq!(
        config.get(&OxStr::from("relative")),
        Some(&Object::String(OxStr::from("")))
    );
}

#[test]
fn open_win_split_honors_four_way_direction() {
    // "left"/"above" place the new window before the target; "right"/"below"
    // place it after, matching upstream split directions. Preorder order shows
    // the side each direction lands on.
    let (editor, buffer, tab, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let left = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[("split", Object::String(OxStr::from("left")))]),
    )
    .unwrap();
    assert_eq!(
        session.with_editor(|editor| editor.tabpage(tab).unwrap().windows()),
        vec![left, window]
    );

    let (editor, buffer, tab, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let right = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[("split", Object::String(OxStr::from("right")))]),
    )
    .unwrap();
    assert_eq!(
        session.with_editor(|editor| editor.tabpage(tab).unwrap().windows()),
        vec![window, right]
    );

    let (editor, buffer, tab, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let above = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[("split", Object::String(OxStr::from("above")))]),
    )
    .unwrap();
    assert_eq!(
        session.with_editor(|editor| editor.tabpage(tab).unwrap().windows()),
        vec![above, window]
    );

    let (editor, buffer, tab, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let below = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[("split", Object::String(OxStr::from("below")))]),
    )
    .unwrap();
    assert_eq!(
        session.with_editor(|editor| editor.tabpage(tab).unwrap().windows()),
        vec![window, below]
    );
}

#[test]
fn open_win_split_honors_config_win_target_on_any_tabpage() {
    let (mut editor, buffer, tab, _) = editor_with_lines(&["one"]);
    let other_tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let far = editor.tabpage(other_tab).unwrap().current_window();
    let session = session_with(editor);
    // The `win` config selects the split target even though it lives on a
    // different (non-current) tabpage.
    let split = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[
            ("split", Object::String(OxStr::from("right"))),
            ("win", Object::Window(far)),
        ]),
    )
    .unwrap();
    // The new window joins its target on `other_tab`, not the current tab.
    assert_eq!(
        session.with_editor(|editor| editor.tabpage(other_tab).unwrap().windows()),
        vec![far, split]
    );
    assert!(
        !session
            .with_editor(|editor| editor.tabpage(tab).unwrap().windows())
            .contains(&split)
    );
    // Passing a floating window as the split target is rejected.
    let float = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("editor"))),
            ("row", Object::Float(0.0)),
            ("col", Object::Float(0.0)),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(2)),
        ]),
    )
    .unwrap();
    assert_eq!(
        crate::window::nvim_open_win(
            &session,
            buffer,
            false,
            dict(&[
                ("split", Object::String(OxStr::from("right"))),
                ("win", Object::Window(float)),
            ]),
        ),
        Err(ApiError::exception("Cannot split a floating window"))
    );
}

#[test]
fn open_win_split_enter_false_preserves_current_window() {
    // `nvim_open_win(buf, false, {split='right'})` must NOT change the
    // current window, matching the float-window behavior (buffer_spec T1).
    let (editor, _buffer, _, origin) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let other_buf = session.with_editor_mut(|editor| editor.create_buffer(true).unwrap());
    let split = crate::window::nvim_open_win(
        &session,
        other_buf,
        false,
        dict(&[("split", Object::String(OxStr::from("right")))]),
    )
    .unwrap();
    assert_ne!(split, origin, "split should create a new window");
    assert_eq!(
        session.with_editor(Editor::current_window),
        Some(origin),
        "enter=false must preserve the current window"
    );
    assert_eq!(
        session.with_editor(|editor| editor.window(split).unwrap().buffer),
        other_buf,
        "split window shows the requested buffer"
    );
}

#[test]
fn open_win_split_enter_true_makes_new_window_current() {
    let (editor, _buffer, _, origin) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let other_buf = session.with_editor_mut(|editor| editor.create_buffer(true).unwrap());
    let split = crate::window::nvim_open_win(
        &session,
        other_buf,
        true,
        dict(&[("split", Object::String(OxStr::from("right")))]),
    )
    .unwrap();
    assert_eq!(
        session.with_editor(Editor::current_window),
        Some(split),
        "enter=true must make the new window current"
    );
    assert_eq!(
        session.with_editor(Editor::previous_window),
        Some(origin),
        "enter=true sets previous to the origin window"
    );
}

#[test]
fn open_win_float_enter_false_preserves_current_window() {
    // Float enter=false already works — this test guards against regression.
    let (editor, _buffer, _, origin) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let other_buf = session.with_editor_mut(|editor| editor.create_buffer(true).unwrap());
    let float = crate::window::nvim_open_win(
        &session,
        other_buf,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("editor"))),
            ("row", Object::Float(0.0)),
            ("col", Object::Float(0.0)),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(5)),
        ]),
    )
    .unwrap();
    assert_eq!(
        session.with_editor(Editor::current_window),
        Some(origin),
        "float enter=false preserves the current window"
    );
    assert_eq!(
        session.with_editor(|editor| editor.window(float).unwrap().buffer),
        other_buf,
        "float window shows the requested buffer"
    );
}

#[test]
fn open_win_external_reports_typed_not_implemented() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    assert_eq!(
        crate::window::nvim_open_win(
            &session,
            buffer,
            false,
            dict(&[("external", Object::Boolean(true))]),
        ),
        Err(ApiError::exception(
            "Not implemented: external floating windows require a UI layer"
        ))
    );
}

#[test]
fn open_win_accepts_style_focusable_hide_noautocmd() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let float = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("editor"))),
            ("row", Object::Float(1.0)),
            ("col", Object::Float(2.0)),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(2)),
            ("style", Object::String(OxStr::from("minimal"))),
            ("focusable", Object::Boolean(false)),
            ("hide", Object::Boolean(false)),
            ("noautocmd", Object::Boolean(true)),
            ("zindex", Object::Integer(10)),
        ]),
    )
    .unwrap();
    assert_eq!(
        crate::window::nvim_win_get_position(&session, float),
        Ok(vec![1, 2])
    );
    assert_eq!(crate::window::nvim_win_is_valid(&session, float), Ok(true));
    // Unknown style values are rejected.
    let invalid = dict(&[
        ("relative", Object::String(OxStr::from("editor"))),
        ("row", Object::Float(0.0)),
        ("col", Object::Float(0.0)),
        ("width", Object::Integer(1)),
        ("height", Object::Integer(1)),
        ("style", Object::String(OxStr::from("fancy"))),
    ]);
    assert!(matches!(
        crate::window::nvim_open_win(&session, buffer, false, invalid),
        Err(ApiError::Validation(_))
    ));
}

#[test]
fn open_win_bufpos_supplies_default_row_and_col() {
    // bufpos ([line, column]) is valid only with relative="win" and supplies
    // row/col defaults: row=1 (NW anchor), col=0 when neither is given
    // (api/window.c:1307-1320). With a [0, 0] bufpos the float is anchored
    // to the first buffer cell, so the resolved geometry is the default offset.
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let float = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("win"))),
            (
                "bufpos",
                Object::Array(vec![Object::Integer(0), Object::Integer(0)]),
            ),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(2)),
        ]),
    )
    .unwrap();
    assert_eq!(
        crate::window::nvim_win_get_position(&session, float),
        Ok(vec![1, 0])
    );
}

#[test]
fn open_win_bufpos_changes_resolved_geometry() {
    // Buffer-relative float placement must depend on the supplied bufpos:
    // [1, 0] and [20, 40] resolve to different screen cells in the window.
    let owned: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
    let lines: Vec<&str> = owned.iter().map(std::string::String::as_str).collect();
    let (editor, buffer, _, _) = editor_with_lines(&lines);
    let session = session_with(editor);
    let first = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("win"))),
            (
                "bufpos",
                Object::Array(vec![Object::Integer(1), Object::Integer(0)]),
            ),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(2)),
        ]),
    )
    .unwrap();
    let first_pos = crate::window::nvim_win_get_position(&session, first).unwrap();
    let second = crate::window::nvim_open_win(
        &session,
        buffer,
        false,
        dict(&[
            ("relative", Object::String(OxStr::from("win"))),
            (
                "bufpos",
                Object::Array(vec![Object::Integer(20), Object::Integer(40)]),
            ),
            ("width", Object::Integer(10)),
            ("height", Object::Integer(2)),
        ]),
    )
    .unwrap();
    let second_pos = crate::window::nvim_win_get_position(&session, second).unwrap();
    assert_ne!(first_pos, second_pos);
    assert_eq!(first_pos, vec![2, 0]);
    assert_eq!(second_pos, vec![21, 40]);
}

#[test]
fn open_win_rejects_invalid_bufpos_combinations() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    // A one-element "array" is rejected with a typed Validation error (no panic).
    assert_eq!(
        crate::window::nvim_open_win(
            &session,
            buffer,
            false,
            dict(&[
                ("relative", Object::String(OxStr::from("win"))),
                ("bufpos", Object::Array(vec![Object::Integer(2)])),
                ("width", Object::Integer(10)),
                ("height", Object::Integer(2)),
            ]),
        ),
        Err(ApiError::validation(
            "Invalid 'config.bufpos': expected [line, column] array of length 2"
        ))
    );
    // bufpos anchors float geometry to a window's text, so it is only valid
    // together with relative="win".
    assert_eq!(
        crate::window::nvim_open_win(
            &session,
            buffer,
            false,
            dict(&[
                ("relative", Object::String(OxStr::from("editor"))),
                (
                    "bufpos",
                    Object::Array(vec![Object::Integer(2), Object::Integer(3)])
                ),
                ("width", Object::Integer(10)),
                ("height", Object::Integer(2)),
            ]),
        ),
        Err(ApiError::validation(
            "Invalid 'config.bufpos': only valid when relative is 'win'"
        ))
    );
}

#[test]
fn open_win_accepts_highlight_tuple_borders_and_titles() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let config = dict(&[
        ("relative", Object::String(OxStr::from("editor"))),
        ("row", Object::Float(1.0)),
        ("col", Object::Float(2.0)),
        ("width", Object::Integer(10)),
        ("height", Object::Integer(2)),
        (
            "border",
            Object::Array(vec![
                Object::Array(vec![
                    Object::String(OxStr::from("+")),
                    Object::String(OxStr::from("MyCorner")),
                ]),
                Object::String(OxStr::from("x")),
            ]),
        ),
        (
            "title",
            Object::Array(vec![Object::Array(vec![
                Object::String(OxStr::from("Doc")),
                Object::String(OxStr::from("FloatTitle")),
            ])]),
        ),
        (
            "footer",
            Object::Array(vec![Object::String(OxStr::from("read-only"))]),
        ),
    ]);
    let float = crate::window::nvim_open_win(&session, buffer, false, config).unwrap();
    assert_eq!(crate::window::nvim_win_is_valid(&session, float), Ok(true));
}

#[test]
fn replace_termcodes_translates_named_special_keys() {
    const NAMED_KEY_CODES: &[(&str, [u8; 3])] = &[
        // Named special keys become the internal three-byte keycode form.
        ("<Up>", [K_SPECIAL, b'k', b'u']),
        ("<Down>", [K_SPECIAL, b'k', b'd']),
        ("<Left>", [K_SPECIAL, b'k', b'l']),
        ("<Right>", [K_SPECIAL, b'k', b'r']),
        ("<Home>", [K_SPECIAL, b'k', b'h']),
        ("<End>", [K_SPECIAL, b'@', b'7']),
        ("<Del>", [K_SPECIAL, b'k', b'D']),
        ("<PageUp>", [K_SPECIAL, b'k', b'P']),
        ("<PageDown>", [K_SPECIAL, b'k', b'N']),
        ("<F1>", [K_SPECIAL, b'k', b'1']),
        ("<F10>", [K_SPECIAL, b'k', b';']),
        ("<F11>", [K_SPECIAL, b'F', b'1']),
        ("<F12>", [K_SPECIAL, b'F', b'2']),
        // <BS> and <Tab> are special keys (K_BS, K_TAB), not literal control bytes.
        ("<BS>", [K_SPECIAL, b'k', b'b']),
        ("<Tab>", [K_SPECIAL, KS_EXTRA, 54]),
        ("<Ignore>", [K_SPECIAL, KS_EXTRA, 53]),
        ("<Nop>", [K_SPECIAL, KS_EXTRA, 97]),
        // Editing/document keys (K_INS, K_HELP, K_UNDO).
        ("<Insert>", [K_SPECIAL, b'k', b'I']),
        ("<Ins>", [K_SPECIAL, b'k', b'I']),
        ("<Help>", [K_SPECIAL, b'%', b'1']),
        ("<Undo>", [K_SPECIAL, b'&', b'8']),
        // Shifted Tab (K_S_TAB).
        ("<S-Tab>", [K_SPECIAL, b'k', b'B']),
        // Keypad keys (k0-k9 and k-prefixed navigation/arithmetic).
        ("<k0>", [K_SPECIAL, b'K', b'C']),
        ("<kUp>", [K_SPECIAL, b'K', b'u']),
        ("<kEnd>", [K_SPECIAL, b'K', b'4']),
        ("<kPlus>", [K_SPECIAL, b'K', b'6']),
        // Shifted and control cursor keys.
        ("<S-Up>", [K_SPECIAL, KS_EXTRA, 4]),
        ("<S-Down>", [K_SPECIAL, KS_EXTRA, 5]),
        ("<S-Left>", [K_SPECIAL, b'#', b'4']),
        ("<S-Right>", [K_SPECIAL, b'%', b'i']),
        ("<S-Home>", [K_SPECIAL, b'#', b'2']),
        ("<C-Left>", [K_SPECIAL, KS_EXTRA, 85]),
        ("<C-Right>", [K_SPECIAL, KS_EXTRA, 86]),
        ("<C-Home>", [K_SPECIAL, KS_EXTRA, 87]),
        ("<C-End>", [K_SPECIAL, KS_EXTRA, 88]),
        // Function keys beyond F12 (computed K_F13..K_F63 bytes).
        ("<F13>", [K_SPECIAL, b'F', b'3']),
        ("<F20>", [K_SPECIAL, b'F', b'A']),
        ("<F40>", [K_SPECIAL, b'F', b'U']),
        ("<F41>", [K_SPECIAL, b'F', b'V']),
        ("<F46>", [K_SPECIAL, b'F', b'a']),
        ("<F63>", [K_SPECIAL, b'F', b'r']),
        // Shifted function keys and extra xterm keys.
        ("<S-F1>", [K_SPECIAL, KS_EXTRA, 6]),
        ("<S-F12>", [K_SPECIAL, KS_EXTRA, 17]),
        ("<xUp>", [K_SPECIAL, KS_EXTRA, 65]),
    ];
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let termcodes = |input: &str, do_lt: bool, special: bool| {
        crate::global::nvim_replace_termcodes(&session, OxStr::from(input), true, do_lt, special)
            .unwrap()
    };
    for (input, bytes) in NAMED_KEY_CODES {
        assert_eq!(termcodes(input, true, true).as_bytes(), bytes.as_slice());
    }
    // Literal control keys remain single bytes.
    assert_eq!(termcodes("<CR>", true, true).as_bytes(), b"\r");
    assert_eq!(termcodes("<Esc>", true, true).as_bytes(), &[0x1b]);
    assert_eq!(termcodes("<Space>", true, true).as_bytes(), b" ");
    // <lt> only translates when do_lt is set; special=false leaves keycodes.
    assert_eq!(termcodes("<lt>", true, true).as_bytes(), b"<");
    assert_eq!(termcodes("<lt>", false, true).as_bytes(), b"<lt>");
    assert_eq!(termcodes("<CR>", true, false).as_bytes(), b"<CR>");
}

#[test]
fn clear_autocmds_omitted_group_only_targets_default_group() {
    // api.txt `nvim_clear_autocmds()`: an omitted group matches autocommands
    // that are in NO group (the default augroup), not every group.
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let group_id = crate::autocmd::nvim_create_augroup(
        &session,
        OxStr::from("mine"),
        dict(&[("clear", Object::Boolean(false))]),
    )
    .unwrap();
    crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[("command", Object::String(OxStr::from("default")))]),
    )
    .unwrap();
    crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufEnter")),
        dict(&[
            ("group", Object::Integer(group_id)),
            ("command", Object::String(OxStr::from("grouped"))),
        ]),
    )
    .unwrap();
    crate::autocmd::nvim_clear_autocmds(
        &session,
        dict(&[("event", Object::String(OxStr::from("BufEnter")))]),
    )
    .unwrap();
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(
            &session,
            dict(&[("event", Object::String(OxStr::from("BufEnter")))])
        )
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        crate::autocmd::nvim_get_autocmds(&session, dict(&[("group", Object::Integer(group_id))]))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn get_extmarks_all_namespaces_and_mark_id_bounds() {
    // api.txt `nvim_buf_get_extmarks()`: ns_id -1 queries every namespace, and
    // start/end may be valid extmark ids whose positions define the bounds.
    let (editor, buffer, _, _) = editor_with_lines(&["one", "two", "three"]);
    let session = session_with(editor);
    let ns_a = crate::extmark::nvim_create_namespace(&session, OxStr::from("a")).unwrap();
    let ns_b = crate::extmark::nvim_create_namespace(&session, OxStr::from("b")).unwrap();
    let m1 = crate::extmark::nvim_buf_set_extmark(&session, buffer, ns_a, 0, 0, dict(&[])).unwrap();
    let m2 = crate::extmark::nvim_buf_set_extmark(&session, buffer, ns_b, 2, 0, dict(&[])).unwrap();

    let all = crate::extmark::nvim_buf_get_extmarks(
        &session,
        buffer,
        -1,
        Object::Array(vec![Object::Integer(0), Object::Integer(0)]),
        Object::Integer(-1),
        dict(&[]),
    )
    .unwrap();
    assert_eq!(all.len(), 2);
    for id in [m1, m2] {
        assert!(all.iter().any(|mark| mark[0] == Object::Integer(id)));
    }

    // Positive integer bounds are extmark ids resolved within the namespace.
    let in_ns = crate::extmark::nvim_buf_get_extmarks(
        &session,
        buffer,
        ns_a,
        Object::Integer(m1),
        Object::Integer(m1),
        dict(&[]),
    )
    .unwrap();
    assert_eq!(in_ns.len(), 1);
    assert_eq!(in_ns[0][0], Object::Integer(m1));

    // All-namespace queries cannot resolve an id bound to one namespace.
    assert!(
        crate::extmark::nvim_buf_get_extmarks(
            &session,
            buffer,
            -1,
            Object::Integer(m2),
            Object::Integer(-1),
            dict(&[]),
        )
        .is_err()
    );
}

#[test]
fn set_extmark_strict_rejects_out_of_buffer_and_line() {
    // api.txt `nvim_buf_set_extmark()` `strict` (default true): the mark is not
    // placed if the line is past end-of-buffer or the column past end-of-line.
    let (editor, buffer, _, _) = editor_with_lines(&["one", "two"]);
    let session = session_with(editor);
    let ns = crate::extmark::nvim_create_namespace(&session, OxStr::from("s")).unwrap();
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(&session, buffer, ns, 5, 0, dict(&[]))
            .unwrap_err()
            .message(),
        "Invalid 'line': out of range"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(&session, buffer, ns, 0, 50, dict(&[]))
            .unwrap_err()
            .message(),
        "Invalid 'col': out of range"
    );
    assert_eq!(
        crate::extmark::nvim_buf_set_extmark(
            &session,
            buffer,
            ns,
            0,
            0,
            dict(&[
                ("end_row", Object::Integer(9)),
                ("end_col", Object::Integer(0))
            ]),
        )
        .unwrap_err()
        .message(),
        "Invalid 'end_row': out of range"
    );
    // strict=false allows out-of-range placement.
    let id = crate::extmark::nvim_buf_set_extmark(
        &session,
        buffer,
        ns,
        5,
        50,
        dict(&[("strict", Object::Boolean(false))]),
    )
    .unwrap();
    let marks = crate::extmark::nvim_buf_get_extmarks(
        &session,
        buffer,
        ns,
        Object::Integer(0),
        Object::Integer(-1),
        dict(&[]),
    )
    .unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0][0], Object::Integer(id));
}

#[test]
fn highlight_groups_are_namespace_scoped_and_activatible() {
    // api.txt `nvim_set_hl()`/`nvim_get_hl()`: namespaces scope highlight
    // groups (ns 0 is global) and `nvim_set_hl_ns()` activates a namespace's
    // distinct definitions.
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    crate::ui::nvim_set_hl(
        &session,
        0,
        OxStr::from("Scope"),
        dict(&[("fg", Object::Integer(0x0011_1111))]),
    )
    .unwrap();
    crate::ui::nvim_set_hl(
        &session,
        7,
        OxStr::from("Scope"),
        dict(&[("fg", Object::Integer(0x0077_7777))]),
    )
    .unwrap();
    let global = crate::ui::nvim_get_hl(
        &session,
        0,
        dict(&[("name", Object::String(OxStr::from("Scope")))]),
    )
    .unwrap();
    assert_eq!(
        global.get(&OxStr::from("fg")),
        Some(&Object::Integer(0x0011_1111))
    );
    let scoped = crate::ui::nvim_get_hl(
        &session,
        7,
        dict(&[("name", Object::String(OxStr::from("Scope")))]),
    )
    .unwrap();
    assert_eq!(
        scoped.get(&OxStr::from("fg")),
        Some(&Object::Integer(0x0077_7777))
    );
    // A namespace that has not defined the group reports not found.
    assert!(
        crate::ui::nvim_get_hl(
            &session,
            9,
            dict(&[("name", Object::String(OxStr::from("Scope")))])
        )
        .is_err()
    );
    // set_hl_ns switches the active namespace, and edits to it stay distinct.
    crate::ui::nvim_set_hl_ns(&session, 7).unwrap();
    assert_eq!(crate::ui::nvim_get_hl_ns(&session, dict(&[])), Ok(7));
    crate::ui::nvim_set_hl(
        &session,
        7,
        OxStr::from("Scope"),
        dict(&[("fg", Object::Integer(0x0006_0606))]),
    )
    .unwrap();
    let updated = crate::ui::nvim_get_hl(
        &session,
        7,
        dict(&[("name", Object::String(OxStr::from("Scope")))]),
    )
    .unwrap();
    assert_eq!(
        updated.get(&OxStr::from("fg")),
        Some(&Object::Integer(0x0006_0606))
    );
    assert_eq!(
        crate::ui::nvim_get_hl(
            &session,
            0,
            dict(&[("name", Object::String(OxStr::from("Scope")))])
        )
        .unwrap()
        .get(&OxStr::from("fg")),
        Some(&Object::Integer(0x0011_1111))
    );
}

#[test]
fn set_client_info_accepts_msgpack_rpc_type() {
    // api.txt `nvim_set_client_info()`: "msgpack-rpc" is a valid client type.
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let _stdio_caller = session.enter_rpc_call(ox_rpc::ChannelId::new(1));
    crate::channel::nvim_set_client_info(
        &session,
        OxStr::from("rpc"),
        dict(&[("major", Object::Integer(1))]),
        OxStr::from("msgpack-rpc"),
        dict(&[]),
        dict(&[]),
    )
    .unwrap();
    let info = crate::channel::nvim_get_chan_info(&session, 1).unwrap();
    let Object::Dict(client) = info.get(&OxStr::from("client")).unwrap() else {
        panic!("missing client dict")
    };
    assert_eq!(
        client.get(&OxStr::from("type")),
        Some(&Object::String(OxStr::from("msgpack-rpc")))
    );
    assert_eq!(
        client.get(&OxStr::from("name")),
        Some(&Object::String(OxStr::from("rpc")))
    );
}

fn set_option_value(
    session: &crate::ApiSession,
    name: &str,
    value: Object,
    opts: &[(&str, Object)],
) -> Result<Object, ApiError> {
    crate::global::nvim_set_option_value(
        session,
        OxStr::from(name),
        value,
        dict(
            &opts
                .iter()
                .map(|(key, value)| (*key, value.clone()))
                .collect::<Vec<_>>(),
        ),
    )
}

fn get_option_value(session: &crate::ApiSession, name: &str) -> Result<Object, ApiError> {
    crate::global::nvim_get_option_value(session, OxStr::from(name), Dict(Vec::new()))
}

#[test]
fn set_option_value_returns_assigned_scalars_like_upstream() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    // Upstream nvim_set_option_value returns the assigned value (v0.13-dev
    // structured option returns); verified against the reference binary.
    assert_eq!(
        set_option_value(&session, "number", Object::Boolean(true), &[]),
        Ok(Object::Boolean(true))
    );
    assert_eq!(
        set_option_value(&session, "tabstop", Object::Integer(3), &[]),
        Ok(Object::Integer(3))
    );
    assert_eq!(
        set_option_value(&session, "undolevels", Object::Integer(100), &[]),
        Ok(Object::Integer(100))
    );
    assert_eq!(
        set_option_value(
            &session,
            "background",
            Object::String(OxStr::from("dark")),
            &[]
        ),
        Ok(Object::String(OxStr::from("dark")))
    );
    assert_eq!(
        set_option_value(
            &session,
            "wildcharm",
            Object::String(OxStr::from("23")),
            &[]
        ),
        Ok(Object::Integer(23))
    );
}

#[test]
fn set_option_value_returns_structured_list_forms_like_upstream() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    // Flag list ('shortmess', Flags): each character becomes a key.
    assert_eq!(
        set_option_value(
            &session,
            "shortmess",
            Object::String(OxStr::from("ltToOCF")),
            &[]
        ),
        Ok(Object::Dict(dict(&[
            ("l", Object::Boolean(true)),
            ("t", Object::Boolean(true)),
            ("T", Object::Boolean(true)),
            ("o", Object::Boolean(true)),
            ("O", Object::Boolean(true)),
            ("C", Object::Boolean(true)),
            ("F", Object::Boolean(true)),
        ])))
    );
    // Comma flag list ('whichwrap', FlagsComma): each item becomes a key.
    assert_eq!(
        set_option_value(
            &session,
            "whichwrap",
            Object::String(OxStr::from("b,s")),
            &[]
        ),
        Ok(Object::Dict(dict(&[
            ("b", Object::Boolean(true)),
            ("s", Object::Boolean(true)),
        ])))
    );
    // Comma list ('wildignore', OneComma): items become an Array.
    assert_eq!(
        set_option_value(
            &session,
            "wildignore",
            Object::String(OxStr::from("*.o,*.obj")),
            &[]
        ),
        Ok(Object::Array(vec![
            Object::String(OxStr::from("*.o")),
            Object::String(OxStr::from("*.obj")),
        ]))
    );
    // 'matchpairs' is a plain OneComma list upstream: `(:)` items stay strings.
    assert_eq!(
        set_option_value(
            &session,
            "matchpairs",
            Object::String(OxStr::from("(:),{:}")),
            &[]
        ),
        Ok(Object::Array(vec![
            Object::String(OxStr::from("(:)")),
            Object::String(OxStr::from("{:}")),
        ]))
    );
    // Colon map ('listchars', OneCommaColon): items become key/value pairs.
    assert_eq!(
        set_option_value(
            &session,
            "listchars",
            Object::String(OxStr::from("eol:~,tab:>-")),
            &[]
        ),
        Ok(Object::Dict(dict(&[
            ("eol", Object::String(OxStr::from("~"))),
            ("tab", Object::String(OxStr::from(">-"))),
        ])))
    );
    assert_eq!(
        set_option_value(
            &session,
            "fillchars",
            Object::String(OxStr::from("vert:|,fold:-")),
            &[]
        ),
        Ok(Object::Dict(dict(&[
            ("vert", Object::String(OxStr::from("|"))),
            ("fold", Object::String(OxStr::from("-"))),
        ])))
    );
}

#[test]
fn set_option_value_accepts_structured_inputs_like_upstream() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    // Array input joins into the canonical comma string and returns the
    // structured form; duplicate items drop on NoDup lists ('wildignore').
    assert_eq!(
        set_option_value(
            &session,
            "wildignore",
            Object::Array(vec![
                Object::String(OxStr::from("*.a")),
                Object::String(OxStr::from("*.b")),
            ]),
            &[]
        ),
        Ok(Object::Array(vec![
            Object::String(OxStr::from("*.a")),
            Object::String(OxStr::from("*.b")),
        ]))
    );
    assert_eq!(
        set_option_value(
            &session,
            "wildignore",
            Object::Array(vec![
                Object::String(OxStr::from("*.a")),
                Object::String(OxStr::from("*.a")),
            ]),
            &[]
        ),
        Ok(Object::Array(vec![Object::String(OxStr::from("*.a"))]))
    );
    // Dict input for a flag list keeps truthy keys.
    assert_eq!(
        set_option_value(
            &session,
            "shortmess",
            Object::Dict(dict(&[
                ("a", Object::Boolean(true)),
                ("o", Object::Boolean(true))
            ])),
            &[]
        ),
        Ok(Object::Dict(dict(&[
            ("a", Object::Boolean(true)),
            ("o", Object::Boolean(true)),
        ])))
    );
    // Dict input for a colon map joins key:value pairs, bare flags stay bare,
    // and the joined result is sorted like upstream optval_from_obj().
    assert_eq!(
        set_option_value(
            &session,
            "fillchars",
            Object::Dict(dict(&[
                ("vert", Object::String(OxStr::from("|"))),
                ("fold", Object::Boolean(true))
            ])),
            &[]
        ),
        Ok(Object::Dict(dict(&[
            ("fold", Object::Boolean(true)),
            ("vert", Object::String(OxStr::from("|"))),
        ])))
    );
}

#[test]
fn set_option_value_dry_run_returns_value_without_storing() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    assert_eq!(
        set_option_value(
            &session,
            "shortmess",
            Object::String(OxStr::from("filnxtToOF")),
            &[("dry_run", Object::Boolean(true))]
        ),
        Ok(Object::Dict(dict(&[
            ("f", Object::Boolean(true)),
            ("i", Object::Boolean(true)),
            ("l", Object::Boolean(true)),
            ("n", Object::Boolean(true)),
            ("x", Object::Boolean(true)),
            ("t", Object::Boolean(true)),
            ("T", Object::Boolean(true)),
            ("o", Object::Boolean(true)),
            ("O", Object::Boolean(true)),
            ("F", Object::Boolean(true)),
        ])))
    );
    // dry_run must not modify the stored value.
    assert_eq!(
        get_option_value(&session, "shortmess"),
        Ok(Object::String(OxStr::from("ltToOCF")))
    );
}

#[test]
fn legacy_option_setters_return_nil_like_upstream() {
    let (editor, buffer, _, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    // The deprecated setters are void upstream; their RPC responses are nil.
    assert_eq!(
        crate::global::nvim_set_option(&session, OxStr::from("ignorecase"), Object::Boolean(true)),
        Ok(())
    );
    assert_eq!(
        crate::buffer::nvim_buf_set_option(
            &session,
            buffer,
            OxStr::from("expandtab"),
            Object::Boolean(false)
        ),
        Ok(())
    );
    assert_eq!(
        crate::window::nvim_win_set_option(
            &session,
            window,
            OxStr::from("cursorline"),
            Object::Boolean(true)
        ),
        Ok(())
    );
    assert_eq!(
        get_option_value(&session, "ignorecase"),
        Ok(Object::Boolean(true))
    );
}

#[test]
fn option_value_unknown_errors_match_upstream() {
    let (editor, _buffer, _tab, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let invalid_get = Err(ApiError::validation("Unknown option 'invalid-option'"));
    assert_eq!(get_option_value(&session, "invalid-option"), invalid_get);
    assert_eq!(
        crate::global::nvim_get_option_value(
            &session,
            OxStr::from("invalid-option"),
            dict(&[("win", Object::Window(window))]),
        ),
        invalid_get
    );

    let invalid_set = Err(ApiError::validation("Unknown option 'foobar'"));
    assert_eq!(
        set_option_value(&session, "foobar", Object::String(OxStr::from("baz")), &[],),
        invalid_set
    );
    assert_eq!(
        set_option_value(
            &session,
            "foobar",
            Object::String(OxStr::from("baz")),
            &[("win", Object::Window(window))],
        ),
        invalid_set
    );
}

#[test]
fn option_value_error_preserves_non_unknown_exceptions() {
    let (editor, _buffer, _tab, window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    assert_eq!(
        crate::global::nvim_get_option_value(
            &session,
            OxStr::from("equalalways"),
            dict(&[("win", Object::Window(window))]),
        ),
        Err(ApiError::exception(
            "option `equalalways` cannot be used at Window scope"
        ))
    );
}

#[test]
fn deprecated_option_info_unknown_error_matches_upstream() {
    let session = session();
    assert_eq!(
        crate::deprecated::nvim_get_option_info(&session, OxStr::from("bogus")),
        Err(ApiError::validation("Invalid option (not found): 'bogus'"))
    );
}

#[test]
fn option_setter_error_shapes_match_upstream() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    // nvim_set_option_value reports validation errors in upstream's
    // "Invalid '<name>': expected a valid type" shape.
    assert_eq!(
        set_option_value(&session, "ignorecase", Object::Integer(3), &[]),
        Err(ApiError::validation(
            "Invalid 'ignorecase': expected a valid type, got Integer"
        ))
    );
    assert_eq!(
        set_option_value(&session, "ignorecase", Object::Array(Vec::new()), &[]),
        Err(ApiError::validation(
            "Invalid 'ignorecase': expected a valid type, got Array"
        ))
    );
    // The deprecated setters first reject non-scalar values, then report the
    // deep "Invalid value for option" exception with the offending literal.
    assert_eq!(
        crate::global::nvim_set_option(
            &session,
            OxStr::from("ignorecase"),
            Object::Array(Vec::new())
        ),
        Err(ApiError::validation(
            "Invalid 'value': expected valid option type, got Array"
        ))
    );
    assert_eq!(
        crate::global::nvim_set_option(&session, OxStr::from("ignorecase"), Object::Integer(3)),
        Err(ApiError::exception(
            "Invalid value for option 'ignorecase': expected boolean, got number 3"
        ))
    );
    assert_eq!(
        crate::buffer::nvim_buf_set_option(
            &session,
            buffer,
            OxStr::from("expandtab"),
            Object::String(OxStr::from("x"))
        ),
        Err(ApiError::exception(
            "Invalid value for option 'expandtab': expected boolean, got string \"x\""
        ))
    );
    // api/options.c validate_option_value_args rejects an unknown operation by
    // name and a merge into a boolean option as a conflict.
    assert_eq!(
        set_option_value(
            &session,
            "wildignore",
            Object::String(OxStr::from("*.x")),
            &[("operation", Object::String(OxStr::from("bogus")))]
        ),
        Err(ApiError::validation(
            "Invalid 'operation': expected 'set', 'append', 'prepend', or 'remove'"
        ))
    );
    assert_eq!(
        set_option_value(
            &session,
            "ignorecase",
            Object::Boolean(true),
            &[("operation", Object::String(OxStr::from("append")))]
        ),
        Err(ApiError::validation(
            "Conflict: 'append' not allowed with boolean options"
        ))
    );
}

// ---------------------------------------------------------------------------
// Runtime-file search over 'runtimepath'
// ---------------------------------------------------------------------------

/// An in-memory directory tree, so the ordering rules can be exercised without
/// a real filesystem. Every entry is an absolute path; a trailing `/` marks a
/// directory, and every parent directory of a listed path exists.
struct MemoryFileIO {
    dirs: std::collections::BTreeSet<String>,
    files: std::collections::BTreeSet<String>,
}

impl MemoryFileIO {
    fn new(entries: &[&str]) -> Self {
        let mut io = Self {
            dirs: std::collections::BTreeSet::new(),
            files: std::collections::BTreeSet::new(),
        };
        for entry in entries {
            let path = entry.trim_end_matches('/');
            if entry.ends_with('/') {
                io.dirs.insert(path.to_owned());
            } else {
                io.files.insert(path.to_owned());
            }
            let mut parent = std::path::Path::new(path).parent();
            while let Some(directory) = parent.filter(|directory| directory.as_os_str().len() > 1) {
                io.dirs.insert(directory.to_string_lossy().into_owned());
                parent = directory.parent();
            }
        }
        io
    }

    /// Component-wise wildcard match, the way a shell glob and upstream's
    /// `gen_expand_wildcards()` both treat `*`: it never spans a separator.
    fn matches(pattern: &str, path: &str) -> bool {
        let (pattern, path): (Vec<&str>, Vec<&str>) =
            (pattern.split('/').collect(), path.split('/').collect());
        pattern.len() == path.len()
            && pattern
                .iter()
                .zip(&path)
                .all(|(part, name)| crate::runtime::wildcard(part.as_bytes(), name.as_bytes()))
    }
}

impl crate::FileIO for MemoryFileIO {
    fn expand(&self, pattern: &str, kind: crate::MatchKind) -> Vec<std::path::PathBuf> {
        let candidates: Box<dyn Iterator<Item = &String>> = match kind {
            crate::MatchKind::Dirs => Box::new(self.dirs.iter()),
            crate::MatchKind::Files => Box::new(self.files.iter()),
            crate::MatchKind::DirsAndFiles => Box::new(self.dirs.iter().chain(self.files.iter())),
        };
        let mut found: Vec<String> = candidates
            .filter(|path| Self::matches(pattern, path))
            .cloned()
            .collect();
        found.sort();
        found.into_iter().map(std::path::PathBuf::from).collect()
    }

    fn is_dir(&self, path: &std::path::Path) -> bool {
        self.dirs.contains(path.to_string_lossy().as_ref())
    }

    fn is_readable(&self, path: &std::path::Path) -> bool {
        self.files.contains(path.to_string_lossy().as_ref())
    }
}

/// Builds an editor whose 'runtimepath'/'packpath' and filesystem are the
/// supplied ones, with nothing inherited from the host.
fn runtime_editor(runtimepath: &str, packpath: &str, entries: &[&str]) -> crate::ApiSession {
    let mut editor = Editor::new();
    for (name, value) in [("runtimepath", runtimepath), ("packpath", packpath)] {
        editor
            .options_mut()
            .set_global(name, ox_editor::OptionValue::String(value.to_owned()))
            .expect("option is settable");
    }
    let session = session_with(editor);
    crate::set_file_io(&session, Box::new(MemoryFileIO::new(entries)));
    session
}

fn list_paths(session: &crate::ApiSession) -> Vec<String> {
    crate::channel::nvim_list_runtime_paths(session)
        .expect("listing succeeds")
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn runtime_file(session: &crate::ApiSession, name: &str, all: bool) -> Vec<String> {
    crate::channel::nvim_get_runtime_file(session, OxStr::from(name), all)
        .expect("lookup succeeds")
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn get_named(
    session: &crate::ApiSession,
    patterns: &[&str],
    all: bool,
    is_lua: bool,
) -> Vec<String> {
    let patterns: Vec<String> = patterns
        .iter()
        .map(|pattern| (*pattern).to_owned())
        .collect();
    crate::runtime_get_named(session, &patterns, all, is_lua)
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

const TREE: &[&str] = &[
    "/a/lua/shared.lua",
    "/a/lua/onlya.lua",
    "/a/plugin/x.vim",
    "/b/after/lua/shared.lua",
    "/b/after/plugin/x.vim",
    "/c/lua/shared.lua",
    "/c/plugin/x.vim",
    "/nolua/plugin/shared.lua",
    "/w/p1/lua/shared.lua",
    "/w/p2/lua/shared.lua",
    "/pk/pack/vendor/start/bundle/lua/shared.lua",
    "/pk/pack/vendor/start/bundle/after/lua/shared.lua",
];

// runtime.c do_in_cached_path — `all` decides whether the walk collects every
// match or stops at the first. Three entries all hold the file, so a search
// that ignored `all` would answer with three paths either way, and one that
// ignored 'runtimepath' order would not stop on /a.
#[test]
fn runtime_file_lookup_honors_the_all_flag() {
    let session = runtime_editor("/a,/b/after,/c", "", TREE);
    assert_eq!(
        runtime_file(&session, "lua/shared.lua", true),
        [
            "/a/lua/shared.lua",
            "/b/after/lua/shared.lua",
            "/c/lua/shared.lua"
        ]
    );
    assert_eq!(
        runtime_file(&session, "lua/shared.lua", false),
        ["/a/lua/shared.lua"]
    );
    assert_eq!(
        get_named(&session, &["lua/shared.lua"], false, true),
        ["/a/lua/shared.lua"]
    );
}

// runtime.c do_in_cached_path — the walk follows 'runtimepath' left to right,
// so reordering the same three entries reorders every answer. A search that
// sorted its results, or read the option once and cached it, would return the
// first block's answer here too.
#[test]
fn runtime_file_lookup_follows_runtimepath_order() {
    let session = runtime_editor("/c,/a", "", TREE);
    assert_eq!(
        runtime_file(&session, "lua/shared.lua", true),
        ["/c/lua/shared.lua", "/a/lua/shared.lua"]
    );
    assert_eq!(
        runtime_file(&session, "lua/shared.lua", false),
        ["/c/lua/shared.lua"]
    );

    session.with_editor_mut(|editor| {
        editor
            .options_mut()
            .set_global(
                "runtimepath",
                ox_editor::OptionValue::String("/a,/c".to_owned()),
            )
            .expect("option is settable");
    });
    assert_eq!(
        runtime_file(&session, "lua/shared.lua", true),
        ["/a/lua/shared.lua", "/c/lua/shared.lua"]
    );
    assert_eq!(
        runtime_file(&session, "lua/shared.lua", false),
        ["/a/lua/shared.lua"]
    );
}

// runtime.c runtime_search_path_build — the first pass stops at the first
// `after` entry and the rest of 'runtimepath' is appended from there, so an
// `after` entry in the middle keeps its place. Partitioning the entries into
// non-after then after would move /b/after behind /c and answer with the same
// list as the tail-after case below, which is what makes the pair a test.
#[test]
fn after_entries_keep_their_runtimepath_position() {
    let middle = runtime_editor("/a,/b/after,/c", "", TREE);
    assert_eq!(list_paths(&middle), ["/a", "/b/after", "/c"]);

    let tail = runtime_editor("/a,/c,/b/after", "", TREE);
    assert_eq!(list_paths(&tail), ["/a", "/c", "/b/after"]);

    assert_ne!(list_paths(&middle), list_paths(&tail));
}

// runtime.c runtime_search_path_build — an entry that is also a 'packpath'
// entry splices its start bundles in directly behind itself, while the
// bundles' `after` directories wait for the pass that runs once every
// non-after entry is placed. Appending the after dir next to its bundle, or
// putting the bundles at the end, both reorder this list.
#[test]
fn package_bundles_follow_their_packpath_entry_and_after_dirs_come_last() {
    let session = runtime_editor("/a,/pk,/c", "/pk", TREE);
    assert_eq!(
        list_paths(&session),
        [
            "/a",
            "/pk",
            "/pk/pack/vendor/start/bundle",
            "/c",
            "/pk/pack/vendor/start/bundle/after"
        ]
    );
}

// runtime.c expand_rtp_entry/push_path — a wildcard entry expands to the
// directories it matches, in sorted order, and a directory already on the path
// is never placed twice. Upstream drops a repeat at two points, and each needs
// its own case: naming an entry that is already on the path skips the whole
// entry, while a *different* pattern that expands onto a directory already
// there is caught only when the expansion is pushed. Without the expansion
// /w/* would contribute nothing at all.
#[test]
fn wildcard_entries_expand_and_repeats_collapse() {
    let named = runtime_editor("/w/*,/c,/w/p1", "", TREE);
    assert_eq!(list_paths(&named), ["/w/p1", "/w/p2", "/c"]);
    assert_eq!(
        runtime_file(&named, "lua/shared.lua", true),
        [
            "/w/p1/lua/shared.lua",
            "/w/p2/lua/shared.lua",
            "/c/lua/shared.lua"
        ]
    );

    // `/w/p1*` is not the text of any placed entry, so only the per-expansion
    // check can tell that it produces a directory already on the path.
    let overlapping = runtime_editor("/w/*,/c,/w/p1*", "", TREE);
    assert_eq!(list_paths(&overlapping), ["/w/p1", "/w/p2", "/c"]);

    // The same holds the other way round: the narrower pattern comes first and
    // the wider one may add only what it did not already place.
    let widening = runtime_editor("/w/p1*,/w/*", "", TREE);
    assert_eq!(list_paths(&widening), ["/w/p1", "/w/p2"]);
}

// runtime.c runtime_get_named — with `is_lua` an entry that has no `lua/`
// subdirectory is skipped entirely, which is how `require` avoids probing
// every runtime directory. nvim_get_runtime_file has no such filter, so the
// same tree answers differently through the two entry points.
#[test]
fn lua_lookup_skips_entries_without_a_lua_directory() {
    let session = runtime_editor("/nolua,/a", "", TREE);
    assert_eq!(list_paths(&session), ["/nolua", "/a"]);
    assert_eq!(
        get_named(&session, &["plugin/shared.lua"], true, true),
        Vec::<String>::new()
    );
    assert_eq!(
        get_named(&session, &["plugin/shared.lua"], true, false),
        ["/nolua/plugin/shared.lua"]
    );
    assert_eq!(
        runtime_file(&session, "plugin/shared.lua", true),
        ["/nolua/plugin/shared.lua"]
    );
}

// api/vim.c nvim_get_runtime_file — the name may hold several whitespace
// separated patterns and may glob, and DIP_DIRFILE lets it match directories
// as well as files. All three are tried under one entry before moving on.
#[test]
fn runtime_file_lookup_expands_multiple_patterns_and_directories() {
    let session = runtime_editor("/a,/c", "", TREE);
    assert_eq!(
        runtime_file(&session, "lua/onlya.lua plugin/x.vim", true),
        ["/a/lua/onlya.lua", "/a/plugin/x.vim", "/c/plugin/x.vim"]
    );
    assert_eq!(
        runtime_file(&session, "lua/*.lua", true),
        ["/a/lua/onlya.lua", "/a/lua/shared.lua", "/c/lua/shared.lua"]
    );
    assert_eq!(runtime_file(&session, "lua", true), ["/a/lua", "/c/lua"]);
}

// runtime.c runtime_get_named — patterns are literal readable-file probes, so
// an entry contributes at most one path per pattern and a directory never
// answers. `all` stops the walk on the first hit, as it does for the search.
#[test]
fn lua_lookup_probes_literal_paths_in_order() {
    let session = runtime_editor("/a,/c", "", TREE);
    assert_eq!(
        get_named(&session, &["lua/onlya.lua", "lua/shared.lua"], true, true),
        ["/a/lua/onlya.lua", "/a/lua/shared.lua", "/c/lua/shared.lua"]
    );
    assert_eq!(
        get_named(&session, &["lua/onlya.lua", "lua/shared.lua"], false, true),
        ["/a/lua/onlya.lua"]
    );
    assert_eq!(
        get_named(&session, &["lua/*.lua"], true, true),
        Vec::<String>::new()
    );
    assert_eq!(
        get_named(&session, &["lua"], true, true),
        Vec::<String>::new()
    );
}

// ---------------------------------------------------------------------------
// vim.opt append / prepend / remove
// ---------------------------------------------------------------------------

fn merge_option(
    session: &crate::ApiSession,
    name: &str,
    value: &str,
    operation: &str,
) -> Result<Object, ApiError> {
    set_option_value(
        session,
        name,
        Object::String(OxStr::from(value)),
        &[("operation", Object::String(OxStr::from(operation)))],
    )
}

fn option_text(session: &crate::ApiSession, name: &str) -> String {
    match get_option_value(session, name) {
        Ok(Object::String(value)) => value.to_string_lossy().into_owned(),
        other => panic!("expected a string option, got {other:?}"),
    }
}

// option.c get_option_newval — the comma-list merges `vim.opt.rtp:append()`,
// `:prepend()` and `:remove()` compile to, checked against the reference
// binary: appending an entry already present is a no-op, and removing one that
// is absent leaves the value alone.
#[test]
fn comma_list_options_append_prepend_and_remove() {
    let session = runtime_editor("/a,/b", "", &[]);
    assert_eq!(
        merge_option(&session, "runtimepath", "/c", "append"),
        Ok(Object::Array(vec![
            Object::String(OxStr::from("/a")),
            Object::String(OxStr::from("/b")),
            Object::String(OxStr::from("/c")),
        ]))
    );
    assert_eq!(option_text(&session, "runtimepath"), "/a,/b,/c");
    merge_option(&session, "runtimepath", "/z", "prepend").expect("prepend succeeds");
    assert_eq!(option_text(&session, "runtimepath"), "/z,/a,/b,/c");
    merge_option(&session, "runtimepath", "/b", "remove").expect("remove succeeds");
    assert_eq!(option_text(&session, "runtimepath"), "/z,/a,/c");
    merge_option(&session, "runtimepath", "/a", "append").expect("duplicate append succeeds");
    assert_eq!(option_text(&session, "runtimepath"), "/z,/a,/c");
    merge_option(&session, "runtimepath", "/nope", "remove").expect("absent remove succeeds");
    assert_eq!(option_text(&session, "runtimepath"), "/z,/a,/c");
    // Removing the first and the last item each take exactly one comma with them.
    merge_option(&session, "runtimepath", "/z", "remove").expect("remove succeeds");
    assert_eq!(option_text(&session, "runtimepath"), "/a,/c");
    merge_option(&session, "runtimepath", "/c", "remove").expect("remove succeeds");
    assert_eq!(option_text(&session, "runtimepath"), "/a");
}

// option.c stropt_concat_with_comma — an empty original value takes no
// separator, and a flag-list option is not comma separated at all.
#[test]
fn merge_adds_a_separator_only_where_the_option_has_one() {
    let session = runtime_editor("", "", &[]);
    merge_option(&session, "runtimepath", "/only", "append").expect("append succeeds");
    assert_eq!(option_text(&session, "runtimepath"), "/only");

    session.with_editor_mut(|editor| {
        editor
            .options_mut()
            .set_global(
                "shortmess",
                ox_editor::OptionValue::String("filnx".to_owned()),
            )
            .expect("option is settable");
    });
    merge_option(&session, "shortmess", "tI", "append").expect("append succeeds");
    assert_eq!(option_text(&session, "shortmess"), "filnxtI");
    merge_option(&session, "shortmess", "l", "remove").expect("remove succeeds");
    assert_eq!(option_text(&session, "shortmess"), "finxtI");
}

// option.c get_option_newval — a number option adds, multiplies and subtracts
// rather than concatenating, so `prepend` on 'scrolloff' is a product.
#[test]
fn number_options_merge_arithmetically() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let apply = |operation: &str, start: i64, value: i64| {
        // 'scrolloff' is global-local, so reset it through the same target the
        // merge reads its old value from.
        set_option_value(&session, "scrolloff", Object::Integer(start), &[])
            .expect("reset succeeds");
        set_option_value(
            &session,
            "scrolloff",
            Object::Integer(value),
            &[("operation", Object::String(OxStr::from(operation)))],
        )
    };
    assert_eq!(apply("append", 5, 3), Ok(Object::Integer(8)));
    assert_eq!(apply("prepend", 5, 3), Ok(Object::Integer(15)));
    assert_eq!(apply("remove", 5, 3), Ok(Object::Integer(2)));
}

// option.c stropt_handle_keymatch — for a `key:value` comma list, an appended
// item replaces the entry with the same key instead of adding a second one,
// and a removal matches on the key. A plain comma-list merge would leave
// `fold:-` in place beside `fold:.`.
#[test]
fn key_value_options_merge_on_the_key() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    session.with_editor_mut(|editor| {
        editor
            .options_mut()
            .set_global(
                "fillchars",
                ox_editor::OptionValue::String("vert:|,fold:-".to_owned()),
            )
            .expect("option is settable");
    });
    merge_option(&session, "fillchars", "fold:.", "append").expect("append succeeds");
    assert_eq!(option_text(&session, "fillchars"), "vert:|,fold:.");
    merge_option(&session, "fillchars", "vert:|", "remove").expect("remove succeeds");
    assert_eq!(option_text(&session, "fillchars"), "fold:.");
}

// option.c option_expand — an option flagged `expand` substitutes `$VAR` and a
// leading `~` before the merge, so `vim.opt.rtp:prepend('~/x')` stores an
// absolute path. An unset variable is left standing.
#[test]
fn expand_flagged_options_substitute_home_and_environment() {
    // Read from the ambient environment rather than mutating it: `set_var` is
    // unsafe, and this crate forbids unsafe code.
    let home = std::env::var("HOME").expect("HOME is set for the test process");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let session = runtime_editor("/a", "", &[]);

    merge_option(&session, "runtimepath", "~/tp", "prepend").expect("prepend succeeds");
    assert_eq!(
        option_text(&session, "runtimepath"),
        format!("{home}/tp,/a")
    );
    merge_option(&session, "runtimepath", "$CARGO_MANIFEST_DIR/x", "append")
        .expect("append succeeds");
    assert_eq!(
        option_text(&session, "runtimepath"),
        format!("{home}/tp,/a,{manifest}/x")
    );
    merge_option(&session, "runtimepath", "${CARGO_MANIFEST_DIR}/y", "append")
        .expect("append succeeds");
    assert_eq!(
        option_text(&session, "runtimepath"),
        format!("{home}/tp,/a,{manifest}/x,{manifest}/y")
    );
    // An unset variable and a `~` that does not open a path component both
    // stay literal, and an option without the expand flag is never touched.
    merge_option(&session, "runtimepath", "$OXVIM_TEST_RTP_UNSET/z", "append")
        .expect("append succeeds");
    assert!(option_text(&session, "runtimepath").ends_with(",$OXVIM_TEST_RTP_UNSET/z"));
    merge_option(&session, "runtimepath", "~tilde", "append").expect("append succeeds");
    assert!(option_text(&session, "runtimepath").ends_with(",~tilde"));
    merge_option(&session, "wildignore", "~/w", "append").expect("append succeeds");
    assert_eq!(option_text(&session, "wildignore"), "~/w");
}

// api/options.c nvim_set_option_value — `dry_run` still merges and returns the
// result, but leaves the option where it was.
#[test]
fn dry_run_merges_without_storing() {
    let session = runtime_editor("/a", "", &[]);
    assert_eq!(
        set_option_value(
            &session,
            "runtimepath",
            Object::String(OxStr::from("/b")),
            &[
                ("operation", Object::String(OxStr::from("append"))),
                ("dry_run", Object::Boolean(true)),
            ]
        ),
        Ok(Object::Array(vec![
            Object::String(OxStr::from("/a")),
            Object::String(OxStr::from("/b"))
        ]))
    );
    assert_eq!(option_text(&session, "runtimepath"), "/a");
}

// ---------------------------------------------------------------------------
// Mappings and the Ex-command / Lua hosts
// ---------------------------------------------------------------------------

fn set_keymap(
    session: &crate::ApiSession,
    mode: &str,
    lhs: &str,
    rhs: &str,
    opts: &[(&str, Object)],
) -> Result<(), ApiError> {
    crate::keymap::nvim_set_keymap(
        session,
        OxStr::from(mode),
        OxStr::from(lhs),
        OxStr::from(rhs),
        dict(opts),
    )
}

fn keymaps(session: &crate::ApiSession, mode: &str) -> Vec<Dict> {
    crate::keymap::nvim_get_keymap(session, OxStr::from(mode))
        .expect("listing succeeds")
        .into_iter()
        .map(|entry| match entry {
            Object::Dict(entry) => entry,
            other => panic!("expected a dictionary, got {other:?}"),
        })
        .collect()
}

fn field(entry: &Dict, key: &str) -> Option<Object> {
    entry.get(&OxStr::from(key)).cloned()
}

// mapping.c mapblock_fill_dict — a mapping set through the API comes back with
// upstream's key set and values, read off the reference binary. `noremap`
// tracks the option rather than the default, `desc` appears only when given,
// and `<Leader>` in the lhs is replaced by the default backslash.
#[test]
fn set_keymap_round_trips_through_get_keymap() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    set_keymap(
        &session,
        "n",
        "<Leader>x",
        ":echo \"hi\"<CR>",
        &[
            ("noremap", Object::Boolean(true)),
            ("silent", Object::Boolean(true)),
            ("desc", Object::String(OxStr::from("probe cmd"))),
        ],
    )
    .expect("set succeeds");
    set_keymap(&session, "n", "gp", "gP", &[]).expect("set succeeds");

    let maps = keymaps(&session, "n");
    assert_eq!(maps.len(), 2);
    let leader = maps
        .iter()
        .find(|entry| field(entry, "lhs") == Some(Object::String(OxStr::from("\\x"))))
        .expect("leader mapping");
    assert_eq!(
        field(leader, "rhs"),
        Some(Object::String(OxStr::from(":echo \"hi\"<CR>")))
    );
    assert_eq!(field(leader, "noremap"), Some(Object::Integer(1)));
    assert_eq!(field(leader, "silent"), Some(Object::Integer(1)));
    assert_eq!(
        field(leader, "desc"),
        Some(Object::String(OxStr::from("probe cmd")))
    );
    assert_eq!(
        field(leader, "mode"),
        Some(Object::String(OxStr::from("n")))
    );
    assert_eq!(field(leader, "mode_bits"), Some(Object::Integer(1)));
    assert_eq!(field(leader, "buffer"), Some(Object::Integer(0)));
    assert_eq!(field(leader, "buf"), Some(Object::Integer(0)));
    assert_eq!(field(leader, "abbr"), Some(Object::Integer(0)));
    assert_eq!(field(leader, "scriptversion"), Some(Object::Integer(1)));

    let plain = maps
        .iter()
        .find(|entry| field(entry, "lhs") == Some(Object::String(OxStr::from("gp"))))
        .expect("plain mapping");
    assert_eq!(field(plain, "noremap"), Some(Object::Integer(0)));
    assert_eq!(field(plain, "silent"), Some(Object::Integer(0)));
    assert_eq!(field(plain, "desc"), None);

    crate::keymap::nvim_del_keymap(&session, OxStr::from("n"), OxStr::from("gp"))
        .expect("del succeeds");
    assert_eq!(keymaps(&session, "n").len(), 1);
}

// mapping.c modify_keymap/keymap_array — the mode string selects the mode set,
// so a mapping set in one mode is invisible in another and the `:map` modes
// (the empty string) see it while a single unrelated mode does not.
#[test]
fn keymap_modes_select_which_mappings_are_visible() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    set_keymap(&session, "n", "za", "zA", &[]).expect("set succeeds");
    set_keymap(&session, "i", "zb", "zB", &[]).expect("set succeeds");
    set_keymap(&session, "!", "zc", "zC", &[]).expect("set succeeds");

    assert_eq!(keymaps(&session, "n").len(), 1);
    // 'i' sees its own mapping and the `:map!` one, which covers insert.
    assert_eq!(keymaps(&session, "i").len(), 2);
    assert_eq!(keymaps(&session, "c").len(), 1);
    assert_eq!(keymaps(&session, "o").len(), 0);
    // The empty mode is `:map`: normal, visual, select and operator-pending.
    assert_eq!(keymaps(&session, "").len(), 1);
    let bang = keymaps(&session, "c")
        .first()
        .cloned()
        .expect("cmdline mapping");
    assert_eq!(field(&bang, "mode"), Some(Object::String(OxStr::from("!"))));
    assert_eq!(field(&bang, "mode_bits"), Some(Object::Integer(24)));
}

// mapping.c modify_keymap — the rejections, each with upstream's own message.
#[test]
fn keymap_rejections_match_upstream() {
    let (editor, _, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    assert_eq!(
        set_keymap(&session, "zz", "a", "b", &[]),
        Err(ApiError::validation("Invalid mode shortname: \"zz\""))
    );
    assert_eq!(
        set_keymap(&session, "nv", "a", "b", &[]),
        Err(ApiError::validation("Invalid mode shortname: \"nv\""))
    );
    assert_eq!(
        set_keymap(&session, "n", "", "b", &[]),
        Err(ApiError::validation("Invalid (empty) LHS"))
    );
    assert_eq!(
        set_keymap(&session, "n", "a", "b", &[("bogus", Object::Boolean(true))]),
        Err(ApiError::validation("invalid key: bogus"))
    );
    assert_eq!(
        set_keymap(
            &session,
            "n",
            "a",
            "b",
            &[("replace_keycodes", Object::Boolean(true))]
        ),
        Err(ApiError::validation(
            "\"replace_keycodes\" requires \"expr\""
        ))
    );
    assert_eq!(
        crate::keymap::nvim_del_keymap(&session, OxStr::from("n"), OxStr::from("nosuch")),
        Err(ApiError::exception("E31: No such mapping"))
    );
    set_keymap(&session, "n", "zr", "zR", &[]).expect("set succeeds");
    assert_eq!(
        set_keymap(
            &session,
            "n",
            "zr",
            "zR",
            &[("unique", Object::Boolean(true))]
        ),
        Err(ApiError::exception("E227: Mapping already exists for zr"))
    );
}

// api/buffer.c nvim_buf_set_keymap/nvim_buf_get_keymap — a buffer-local
// mapping is reported by the buffer listing with its handle, and never by the
// global one, which is the distinction between the two scopes.
#[test]
fn buffer_keymaps_stay_out_of_the_global_listing() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    set_keymap(&session, "n", "gg", "gG", &[]).expect("global set succeeds");
    crate::keymap::nvim_buf_set_keymap(
        &session,
        buffer,
        OxStr::from("n"),
        OxStr::from("gb"),
        OxStr::from("gB"),
        dict(&[("desc", Object::String(OxStr::from("buffer local")))]),
    )
    .expect("buffer set succeeds");

    let global = keymaps(&session, "n");
    assert_eq!(global.len(), 1);
    assert_eq!(
        field(&global[0], "lhs"),
        Some(Object::String(OxStr::from("gg")))
    );

    let local = crate::keymap::nvim_buf_get_keymap(&session, buffer, OxStr::from("n"))
        .expect("buffer listing succeeds");
    assert_eq!(local.len(), 1);
    let Object::Dict(entry) = &local[0] else {
        panic!("expected a dictionary")
    };
    assert_eq!(field(entry, "lhs"), Some(Object::String(OxStr::from("gb"))));
    assert_eq!(field(entry, "buffer"), Some(Object::Integer(1)));
    assert_eq!(
        field(entry, "buf"),
        Some(Object::Integer(i64::from(buffer)))
    );
    assert_eq!(
        field(entry, "desc"),
        Some(Object::String(OxStr::from("buffer local")))
    );

    crate::keymap::nvim_buf_del_keymap(&session, buffer, OxStr::from("n"), OxStr::from("gb"))
        .expect("buffer del succeeds");
    assert!(
        crate::keymap::nvim_buf_get_keymap(&session, buffer, OxStr::from("n"))
            .expect("listing")
            .is_empty()
    );
    assert_eq!(keymaps(&session, "n").len(), 1);
}

#[test]
fn command_and_script_use_distinct_executor_methods() {
    let session = session();
    let operations = Rc::new(RefCell::new(Vec::new()));
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            operations: operations.clone(),
            ..Default::default()
        }),
        Box::new(RecordingExecutor::default()),
    );

    crate::global::nvim_command(&session, OxStr::from("write")).unwrap();
    crate::global::nvim_exec2(&session, OxStr::from("echo 'x'"), Dict(Vec::new())).unwrap();

    assert_eq!(
        &*operations.borrow(),
        &[RecordedOperation::Command, RecordedOperation::Script]
    );
}

#[test]
fn function_call_rejects_more_than_twenty_arguments_before_dispatch() {
    let session = session();
    let operations = Rc::new(RefCell::new(Vec::new()));
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            operations: operations.clone(),
            ..Default::default()
        }),
        Box::new(RecordingExecutor::default()),
    );
    let mut accepted = vec![Object::String(OxStr("%s".repeat(19).into_bytes()))];
    accepted.resize(20, Object::String(OxStr::from("x")));

    assert!(crate::global::nvim_call_function(&session, OxStr::from("printf"), accepted).is_ok());
    assert_eq!(
        crate::global::nvim_call_function(&session, OxStr::from("printf"), vec![Object::Nil; 21],),
        Err(ApiError::validation(
            "Function called with too many arguments"
        ))
    );
    assert_eq!(&*operations.borrow(), &[RecordedOperation::Function]);
}

#[test]
fn function_call_eval_coerces_scalar_and_checks_arity() {
    let session = session();
    let operations = Rc::new(RefCell::new(Vec::new()));
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            operations: operations.clone(),
            ..Default::default()
        }),
        Box::new(RecordingExecutor::default()),
    );

    assert_eq!(
        crate::global::nvim_call_function(&session, OxStr::from("eval"), vec![Object::Integer(17)],),
        Ok(Object::Integer(17))
    );
    assert_eq!(
        crate::global::nvim_call_function(&session, OxStr::from("eval"), Vec::new()),
        Err(ApiError::exception(
            "E119: Not enough arguments for function: eval"
        ))
    );
    assert_eq!(
        crate::global::nvim_call_function(
            &session,
            OxStr::from("eval"),
            vec![Object::Integer(1), Object::Integer(2)],
        ),
        Err(ApiError::exception(
            "E118: Too many arguments for function: eval"
        ))
    );
    assert_eq!(
        &*operations.borrow(),
        &[
            RecordedOperation::Function,
            RecordedOperation::Function,
            RecordedOperation::Function,
        ]
    );
}

// api/vim.c nvim_exec2 / nvim_cmd / nvim_command run through the installed
// Ex-command host, and `output` decides whether the messages the script
// produced come back. Without a host installed they say so rather than
// claiming the function does not exist.
#[test]
fn exec_functions_run_through_the_installed_command_host() {
    let session = session();
    assert_eq!(
        crate::global::nvim_command(&session, OxStr::from("write")),
        Err(ApiError::exception("no Ex-command host is installed"))
    );

    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            commands: Vec::new(),
            message: Some("captured"),
            ..Default::default()
        }),
        Box::new(RecordingExecutor::default()),
    );
    assert_eq!(
        crate::global::nvim_command(&session, OxStr::from("write")),
        Ok(())
    );
    assert_eq!(
        crate::global::nvim_exec2(&session, OxStr::from("echo 'x'"), Dict(Vec::new())),
        Ok(Dict(Vec::new()))
    );
    assert_eq!(
        crate::global::nvim_exec2(
            &session,
            OxStr::from("echo 'x'"),
            dict(&[("output", Object::Boolean(true))])
        ),
        Ok(dict(&[("output", Object::String(OxStr::from("captured")))]))
    );
    assert_eq!(
        crate::global::nvim_cmd(
            &session,
            dict(&[("cmd", Object::String(OxStr::from("write")))]),
            dict(&[("output", Object::Boolean(true))])
        ),
        Ok(OxStr::from("captured"))
    );
    // The host is put back after every call, so a second one still finds it.
    assert_eq!(
        crate::global::nvim_command(&session, OxStr::from("write")),
        Ok(())
    );
}

#[test]
fn command_output_captures_and_removes_command_messages() {
    let session = session();
    let operations = Rc::new(RefCell::new(Vec::new()));
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            message: Some("captured"),
            operations: operations.clone(),
            ..Default::default()
        }),
        Box::new(RecordingExecutor::default()),
    );

    assert_eq!(
        crate::deprecated::nvim_command_output(&session, OxStr::from("echo 'captured'")),
        Ok(OxStr::from("captured"))
    );
    assert!(session.with_editor(|editor| editor.messages().is_empty()));

    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            message: Some(""),
            operations: operations.clone(),
            ..Default::default()
        }),
        Box::new(RecordingExecutor::default()),
    );
    assert_eq!(
        crate::deprecated::nvim_command_output(&session, OxStr::from("echo ''")),
        Ok(OxStr::from(""))
    );
    assert!(session.with_editor(|editor| editor.messages().is_empty()));
    assert_eq!(
        &*operations.borrow(),
        &[RecordedOperation::Command, RecordedOperation::Command]
    );
}

struct EchoingLua;

impl crate::LuaExecutor for EchoingLua {
    fn exec(
        &mut self,
        session: &crate::ApiSession,
        code: &str,
        args: Vec<Object>,
    ) -> Result<Object, String> {
        // Prove the host receives the editor as well as the chunk.
        session.with_editor_mut(|editor| {
            editor.push_message(ox_editor::Message {
                kind: ox_editor::MessageKind::Echo,
                content: Object::String(OxStr::from(code)),
                history: false,
                leading_newline: true,
            });
        });
        Ok(Object::Array(args))
    }

    fn invoke_callback(
        &mut self,
        _session: &crate::ApiSession,
        _reference: usize,
        args: Vec<Object>,
    ) -> Result<Object, String> {
        Ok(Object::Array(args))
    }

    fn call_ref(
        &mut self,
        _session: &crate::ApiSession,
        _reference: usize,
        args: Vec<Object>,
    ) -> Result<Vec<Object>, String> {
        Ok(args)
    }

    fn free_callback(&mut self, _reference: usize) -> Result<(), String> {
        Ok(())
    }
}

/// A Lua host whose `exec` panics with a sentinel; used to prove a panicking
/// operation does not leak the pool depth index.
struct PanickingLua;

impl crate::LuaExecutor for PanickingLua {
    fn exec(
        &mut self,
        _session: &crate::ApiSession,
        _code: &str,
        _args: Vec<Object>,
    ) -> Result<Object, String> {
        panic!("lua host panicked");
    }
    fn invoke_callback(
        &mut self,
        _session: &crate::ApiSession,
        _reference: usize,
        _args: Vec<Object>,
    ) -> Result<Object, String> {
        Ok(Object::Nil)
    }

    fn call_ref(
        &mut self,
        _session: &crate::ApiSession,
        _reference: usize,
        _args: Vec<Object>,
    ) -> Result<Vec<Object>, String> {
        Ok(Vec::new())
    }

    fn free_callback(&mut self, _reference: usize) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn pool_depth_restored_after_operation_panic() {
    let session = session();
    crate::set_lua_executor(&session, Box::new(PanickingLua), Box::new(PanickingLua));

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::runtime::with_lua_executor(&session, |session, executor| {
            executor
                .exec(session, "return 1", Vec::new())
                .map_err(ApiError::exception)
        })
    }))
    .expect_err("the panicking host must propagate its panic");
    assert_eq!(
        panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str)),
        Some("lua host panicked")
    );

    // The DepthGuard unwound the depth increment: the pool is idle again and
    // a subsequent frame still runs on pool[0].
    assert_eq!(session.with_state(|state| state.lua_depth), 0);
    crate::set_lua_executor(&session, Box::new(EchoingLua), Box::new(EchoingLua));
    assert_eq!(
        crate::global::nvim_exec_lua(
            &session,
            OxStr::from("return ..."),
            vec![Object::Integer(1)]
        ),
        Ok(Object::Array(vec![Object::Integer(1)]))
    );
}
/// A Lua host that re-enters `with_lua_executor` until its shared counter hits
/// zero, then returns the visit count. `fork` hands the same counter to the
/// child, so depth 2 (past the installed primary/nested pair) proves the S13
/// prototype-growth path: no fork override would surface as "no Lua host is
/// installed" instead of the count.
struct ReenteringLua {
    remaining: std::rc::Rc<std::cell::Cell<u8>>,
    visits: std::rc::Rc<std::cell::Cell<u8>>,
}

impl crate::LuaExecutor for ReenteringLua {
    fn exec(
        &mut self,
        session: &crate::ApiSession,
        _code: &str,
        _args: Vec<Object>,
    ) -> Result<Object, String> {
        self.visits.set(self.visits.get() + 1);
        if self.remaining.get() == 0 {
            return Ok(Object::Integer(i64::from(self.visits.get())));
        }
        self.remaining.set(self.remaining.get() - 1);
        crate::runtime::with_lua_executor(session, |session, executor| {
            executor
                .exec(session, "", Vec::new())
                .map_err(ApiError::exception)
        })
        .map_err(|error| error.to_string())
    }

    fn invoke_callback(
        &mut self,
        _session: &crate::ApiSession,
        _reference: usize,
        args: Vec<Object>,
    ) -> Result<Object, String> {
        Ok(Object::Array(args))
    }

    fn call_ref(
        &mut self,
        _session: &crate::ApiSession,
        _reference: usize,
        _args: Vec<Object>,
    ) -> Result<Vec<Object>, String> {
        Ok(Vec::new())
    }

    fn free_callback(&mut self, _reference: usize) -> Result<(), String> {
        Ok(())
    }

    fn fork(&self) -> Option<Box<dyn crate::LuaExecutor>> {
        Some(Box::new(Self {
            remaining: self.remaining.clone(),
            visits: self.visits.clone(),
        }))
    }
}

#[test]
fn pool_grows_from_prototype_at_depth_three() {
    let session = session();
    let remaining = std::rc::Rc::new(std::cell::Cell::new(2));
    let visits = std::rc::Rc::new(std::cell::Cell::new(0));
    let host = || {
        Box::new(ReenteringLua {
            remaining: remaining.clone(),
            visits: visits.clone(),
        })
    };
    // Two installed hosts serve depths 0-1; depth 2 must fork the prototype
    // seeded from the primary at setter time.
    crate::set_lua_executor(&session, host(), host());
    let outcome = crate::runtime::with_lua_executor(&session, |session, executor| {
        executor
            .exec(session, "outer", Vec::new())
            .map_err(ApiError::exception)
    });
    assert_eq!(outcome, Ok(Object::Integer(3)));
    assert_eq!(session.with_state(|state| state.lua_depth), 0);
    assert_eq!(session.with_state(|state| state.lua_pool.len()), 3);
    assert_eq!(visits.get(), 3);
}

// api/vim.c nvim_exec_lua hands the chunk and its arguments to the Lua host
// and returns what the host produced.
#[test]
fn exec_lua_runs_through_the_installed_lua_host() {
    let session = session();
    assert_eq!(
        crate::global::nvim_exec_lua(&session, OxStr::from("return 1"), Vec::new()),
        Err(ApiError::exception("no Lua host is installed"))
    );
    crate::set_lua_executor(&session, Box::new(EchoingLua), Box::new(EchoingLua));
    assert_eq!(
        crate::global::nvim_exec_lua(
            &session,
            OxStr::from("return ..."),
            vec![Object::Integer(7)]
        ),
        Ok(Object::Array(vec![Object::Integer(7)]))
    );
    assert_eq!(
        session.with_editor(|editor| {
            editor
                .messages()
                .last()
                .map(|message| message.content.clone())
        }),
        Some(Object::String(OxStr::from("return ...")))
    );
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReplaceBufferTextSnapshot {
    text: Vec<u8>,
    changedtick: u64,
    changedtick_diag: u64,
    changedtick_fold: u64,
    modified: bool,
    undo_seq: u64,
    undo_block_len: usize,
    local_mark: Option<ox_text::Position>,
    global_mark: Option<ox_text::Position>,
    jumplist: Option<ox_text::Position>,
    changelist: Option<ox_text::Position>,
    extmark_col: i64,
    window_col: usize,
}
fn snapshot_replace_buffer_text_state(
    session: &crate::ApiSession,
    buffer: crate::BufHandle,
    namespace: i64,
    extmark: i64,
    window: crate::WinHandle,
) -> ReplaceBufferTextSnapshot {
    let mut snapshot = session.with_editor(|editor| {
        let state = editor.buffer(buffer).unwrap();
        ReplaceBufferTextSnapshot {
            text: state.text().unwrap().to_bytes(),
            changedtick: state.changedtick(),
            changedtick_diag: state.changedtick_diag,
            changedtick_fold: state.changedtick_fold,
            modified: state.flags.contains(ox_editor::BufferFlags::MODIFIED),
            undo_seq: state.undo.current_seq(),
            undo_block_len: state.undo.current_block_len(),
            local_mark: editor.local_mark(buffer, 'a').unwrap(),
            global_mark: editor
                .global_marks()
                .get('A')
                .unwrap()
                .map(|mark| mark.position),
            jumplist: editor
                .jumplist()
                .entries()
                .first()
                .map(|entry| entry.position),
            changelist: editor
                .changelists()
                .entries(buffer)
                .unwrap()
                .first()
                .copied(),
            extmark_col: 0,
            window_col: editor.window(window).unwrap().cursor.col,
        }
    });
    let details =
        crate::extmark::nvim_buf_get_extmark_by_id(session, buffer, namespace, extmark, dict(&[]))
            .unwrap();
    snapshot.extmark_col = match details[1].clone() {
        Object::Integer(value) => value,
        other => panic!("expected integer extmark column, got {other:?}"),
    };
    snapshot
}

fn seeded_replace_buffer_text_editor(
    lines: &[&str],
) -> (
    crate::ApiSession,
    crate::BufHandle,
    crate::TabHandle,
    crate::WinHandle,
    i64,
    i64,
) {
    let (editor, buffer, tab, window) = editor_with_lines(lines);
    let session = session_with(editor);
    let classic = ox_text::Position {
        lnum: 1,
        col: lines[0].len().clamp(1, 3),
    };
    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_lines(ox_editor::LineReplaceRequest {
                buffer,
                start: 1,
                end: 1,
                lines: &[lines[0].as_bytes().to_vec()],
                cursor_before: classic,
                cursor_after: classic,
                timestamp: 0,
            })
            .unwrap();
        editor.sync_buffer_undo(buffer);
        editor.set_local_mark(buffer, 'a', classic).unwrap();
        editor
            .global_marks_mut()
            .set('A', ox_editor::MarkLocation::in_buffer(buffer, classic))
            .unwrap();
        editor
            .jumplist_mut()
            .push(ox_editor::MarkLocation::in_buffer(buffer, classic));
    });
    let namespace =
        crate::extmark::nvim_create_namespace(&session, OxStr::from("replace-buffer-text"))
            .unwrap();
    let extmark = crate::extmark::nvim_buf_set_extmark(
        &session,
        buffer,
        namespace,
        0,
        i64::try_from(classic.col).unwrap(),
        dict(&[]),
    )
    .unwrap();
    let second = session.with_editor_mut(|editor| {
        let second = editor.split_vertical(tab, window, buffer, true).unwrap();
        editor.set_window_cursor(second, classic).unwrap();
        second
    });
    (session, buffer, tab, second, namespace, extmark)
}

fn extmark_pos(
    session: &crate::ApiSession,
    buffer: crate::BufHandle,
    namespace: i64,
    extmark: i64,
) -> (i64, i64) {
    let details =
        crate::extmark::nvim_buf_get_extmark_by_id(session, buffer, namespace, extmark, dict(&[]))
            .unwrap();
    match (details[0].clone(), details[1].clone()) {
        (Object::Integer(row), Object::Integer(col)) => (row, col),
        other => panic!("expected integer row/col, got {other:?}"),
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "setup, splice, undo, and redo must remain one stateful geometry scenario"
)]
fn set_text_keeps_classic_columns_while_byte_geometry_tracks_the_splice() {
    let (mut editor, buffer, tab, window) = editor_with_lines(&["0123456789"]);
    let classic = ox_text::Position { lnum: 1, col: 6 };

    editor
        .replace_buffer_lines(ox_editor::LineReplaceRequest {
            buffer,
            start: 1,
            end: 1,
            lines: &[b"0123456789".to_vec()],
            cursor_before: classic,
            cursor_after: classic,
            timestamp: 0,
        })
        .unwrap();
    editor.sync_buffer_undo(buffer);
    editor.set_local_mark(buffer, 'a', classic).unwrap();
    editor
        .global_marks_mut()
        .set('A', ox_editor::MarkLocation::in_buffer(buffer, classic))
        .unwrap();
    editor
        .jumplist_mut()
        .push(ox_editor::MarkLocation::in_buffer(buffer, classic));
    let session = session_with(editor);

    let namespace =
        crate::extmark::nvim_create_namespace(&session, OxStr::from("classic-byte-columns"))
            .unwrap();
    let extmark =
        crate::extmark::nvim_buf_set_extmark(&session, buffer, namespace, 0, 6, dict(&[])).unwrap();
    let second = session.with_editor_mut(|editor| {
        let second = editor.split_vertical(tab, window, buffer, true).unwrap();
        editor.set_window_cursor(second, classic).unwrap();
        second
    });

    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(0, 1),
                    end: ox_editor::ExtmarkPosition::new(0, 3),
                    replacement: vec![b"WXYZ".to_vec()],
                },
                ox_text::Position { lnum: 1, col: 1 },
                ox_text::Position { lnum: 1, col: 1 },
                0,
            )
            .unwrap();
        editor.sync_buffer_undo(buffer);
    });

    assert_eq!(
        session.with_editor(|editor| editor.local_mark(buffer, 'a').unwrap()),
        Some(classic)
    );
    assert_eq!(
        session.with_editor(|editor| editor.global_marks().get('A').unwrap().unwrap().position),
        classic
    );
    assert_eq!(
        session.with_editor(|editor| editor.jumplist().entries()[0].position),
        classic
    );
    assert_eq!(
        session.with_editor(|editor| editor.changelists().entries(buffer).unwrap()[0]),
        classic
    );
    assert_eq!(
        crate::extmark::nvim_buf_get_extmark_by_id(
            &session,
            buffer,
            namespace,
            extmark,
            dict(&[]),
        )
        .unwrap(),
        vec![Object::Integer(0), Object::Integer(8)]
    );
    assert_eq!(
        session.with_editor(|editor| editor.window(second).unwrap().cursor.col),
        8
    );

    session.with_editor_mut(|editor| editor.buffer_undo(buffer).unwrap());
    assert_eq!(
        session.with_editor(|editor| editor.local_mark(buffer, 'a').unwrap()),
        Some(classic)
    );
    assert_eq!(
        session.with_editor(|editor| editor.global_marks().get('A').unwrap().unwrap().position),
        classic
    );
    assert_eq!(
        session.with_editor(|editor| editor.jumplist().entries()[0].position),
        classic
    );
    assert_eq!(
        session.with_editor(|editor| editor.changelists().entries(buffer).unwrap()[0]),
        classic
    );
    assert_eq!(
        crate::extmark::nvim_buf_get_extmark_by_id(
            &session,
            buffer,
            namespace,
            extmark,
            dict(&[]),
        )
        .unwrap(),
        vec![Object::Integer(0), Object::Integer(6)]
    );
    assert_eq!(
        session.with_editor(|editor| editor.window(second).unwrap().cursor.col),
        8
    );

    session.with_editor_mut(|editor| editor.buffer_redo(buffer).unwrap());
    assert_eq!(
        session.with_editor(|editor| editor.local_mark(buffer, 'a').unwrap()),
        Some(classic)
    );
    assert_eq!(
        session.with_editor(|editor| editor.global_marks().get('A').unwrap().unwrap().position),
        classic
    );
    assert_eq!(
        session.with_editor(|editor| editor.jumplist().entries()[0].position),
        classic
    );
    assert_eq!(
        session.with_editor(|editor| editor.changelists().entries(buffer).unwrap()[0]),
        classic
    );
    assert_eq!(
        crate::extmark::nvim_buf_get_extmark_by_id(
            &session,
            buffer,
            namespace,
            extmark,
            dict(&[]),
        )
        .unwrap(),
        vec![Object::Integer(0), Object::Integer(8)]
    );
    assert_eq!(
        session.with_editor(|editor| editor.window(second).unwrap().cursor.col),
        8
    );
}

#[test]
fn replace_buffer_text_out_of_range_leaves_state_unchanged() {
    let (session, buffer, _tab, window, namespace, extmark) =
        seeded_replace_buffer_text_editor(&["abc"]);
    let before = snapshot_replace_buffer_text_state(&session, buffer, namespace, extmark, window);
    for (start, end) in [
        (
            ox_editor::ExtmarkPosition::new(0, usize::MAX),
            ox_editor::ExtmarkPosition::new(0, usize::MAX),
        ),
        (
            ox_editor::ExtmarkPosition::new(9, 0),
            ox_editor::ExtmarkPosition::new(9, 0),
        ),
    ] {
        let err = session.with_editor_mut(|editor| {
            editor
                .replace_buffer_text(
                    buffer,
                    &ox_editor::BufferTextEditRequest {
                        start,
                        end,
                        replacement: vec![b"X".to_vec()],
                    },
                    ox_text::Position { lnum: 1, col: 0 },
                    ox_text::Position { lnum: 1, col: 0 },
                    0,
                )
                .unwrap_err()
        });
        assert!(matches!(
            err,
            ox_editor::EditorError::Buffer(ox_editor::BufferStateError::TextEdit(
                ox_editor::BufferTextEditError::OutOfRange
            ))
        ));
        assert_eq!(
            snapshot_replace_buffer_text_state(&session, buffer, namespace, extmark, window),
            before
        );
    }
}

#[test]
fn replace_buffer_text_reversed_same_row_leaves_state_unchanged() {
    let (session, buffer, _tab, window, namespace, extmark) =
        seeded_replace_buffer_text_editor(&["abc"]);
    let before = snapshot_replace_buffer_text_state(&session, buffer, namespace, extmark, window);
    let err = session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(0, 2),
                    end: ox_editor::ExtmarkPosition::new(0, 1),
                    replacement: vec![b"abc".to_vec()],
                },
                ox_text::Position { lnum: 1, col: 1 },
                ox_text::Position { lnum: 1, col: 1 },
                0,
            )
            .unwrap_err()
    });
    assert!(matches!(
        err,
        ox_editor::EditorError::Buffer(ox_editor::BufferStateError::TextEdit(
            ox_editor::BufferTextEditError::ReversedRange
        ))
    ));
    assert_eq!(
        snapshot_replace_buffer_text_state(&session, buffer, namespace, extmark, window),
        before
    );
}

#[test]
fn replace_buffer_text_reversed_cross_row_leaves_state_unchanged() {
    let (session, buffer, _tab, window, namespace, extmark) =
        seeded_replace_buffer_text_editor(&["alpha", "bravo", "charlie"]);
    let before = snapshot_replace_buffer_text_state(&session, buffer, namespace, extmark, window);
    let err = session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(2, 0),
                    end: ox_editor::ExtmarkPosition::new(0, 1),
                    replacement: vec![b"x".to_vec()],
                },
                ox_text::Position { lnum: 1, col: 0 },
                ox_text::Position { lnum: 1, col: 0 },
                0,
            )
            .unwrap_err()
    });
    assert!(matches!(
        err,
        ox_editor::EditorError::Buffer(ox_editor::BufferStateError::TextEdit(
            ox_editor::BufferTextEditError::ReversedRange
        ))
    ));
    assert_eq!(
        snapshot_replace_buffer_text_state(&session, buffer, namespace, extmark, window),
        before
    );
}

#[test]
fn replace_buffer_text_non_char_boundary_leaves_state_unchanged() {
    let (session, buffer, _tab, window, namespace, extmark) =
        seeded_replace_buffer_text_editor(&["한글"]);
    let before = snapshot_replace_buffer_text_state(&session, buffer, namespace, extmark, window);
    let err = session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(0, 1),
                    end: ox_editor::ExtmarkPosition::new(0, 3),
                    replacement: vec![b"X".to_vec()],
                },
                ox_text::Position { lnum: 1, col: 0 },
                ox_text::Position { lnum: 1, col: 0 },
                0,
            )
            .unwrap_err()
    });
    assert!(matches!(
        err,
        ox_editor::EditorError::Buffer(ox_editor::BufferStateError::TextEdit(
            ox_editor::BufferTextEditError::NotCharBoundary(1)
        ))
    ));
    assert_eq!(
        snapshot_replace_buffer_text_state(&session, buffer, namespace, extmark, window),
        before
    );
}

#[test]
fn replace_buffer_text_more_rows_than_removed_preserves_prefix_suffix() {
    let (session, buffer, _tab, _window, namespace, extmark) =
        seeded_replace_buffer_text_editor(&["alpha", "omega"]);
    let before_col = extmark_pos(&session, buffer, namespace, extmark).1;
    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(0, 1),
                    end: ox_editor::ExtmarkPosition::new(0, 2),
                    replacement: vec![b"L".to_vec(), b"M".to_vec(), b"N".to_vec()],
                },
                ox_text::Position { lnum: 1, col: 1 },
                ox_text::Position { lnum: 1, col: 1 },
                0,
            )
            .unwrap()
    });
    assert_eq!(
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes()),
        b"aL\nM\nNpha\nomega"
    );
    let after_edit = extmark_pos(&session, buffer, namespace, extmark);
    session.with_editor_mut(|editor| editor.buffer_undo(buffer).unwrap());
    assert_eq!(
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes()),
        b"alpha\nomega"
    );
    assert_eq!(
        extmark_pos(&session, buffer, namespace, extmark),
        (0, before_col)
    );
    session.with_editor_mut(|editor| editor.buffer_redo(buffer).unwrap());
    assert_eq!(
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes()),
        b"aL\nM\nNpha\nomega"
    );
    assert_eq!(
        extmark_pos(&session, buffer, namespace, extmark),
        after_edit
    );
}

#[test]
fn replace_buffer_text_fewer_rows_than_removed_preserves_prefix_suffix() {
    let (editor, buffer, _tab, _window) = editor_with_lines(&["alpha", "omega"]);
    let session = session_with(editor);
    let namespace = crate::extmark::nvim_create_namespace(&session, OxStr::from("shrink")).unwrap();
    let first =
        crate::extmark::nvim_buf_set_extmark(&session, buffer, namespace, 0, 4, dict(&[])).unwrap();
    let second =
        crate::extmark::nvim_buf_set_extmark(&session, buffer, namespace, 1, 3, dict(&[])).unwrap();
    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(0, 1),
                    end: ox_editor::ExtmarkPosition::new(1, 2),
                    replacement: vec![b"x".to_vec()],
                },
                ox_text::Position { lnum: 1, col: 1 },
                ox_text::Position { lnum: 1, col: 1 },
                0,
            )
            .unwrap()
    });
    assert_eq!(
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes()),
        b"axega"
    );
    session.with_editor_mut(|editor| editor.buffer_undo(buffer).unwrap());
    assert_eq!(
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes()),
        b"alpha\nomega"
    );
    assert_eq!(extmark_pos(&session, buffer, namespace, first), (0, 4));
    assert_eq!(extmark_pos(&session, buffer, namespace, second), (1, 3));
    session.with_editor_mut(|editor| editor.buffer_redo(buffer).unwrap());
    assert_eq!(
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes()),
        b"axega"
    );
}

#[test]
fn replace_buffer_text_multiline_request_updates_byte_geometry() {
    let (session, buffer, _tab, window, namespace, extmark) =
        seeded_replace_buffer_text_editor(&["ab", "cd", "ef"]);
    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(0, 1),
                    end: ox_editor::ExtmarkPosition::new(1, 2),
                    replacement: vec![b"X".to_vec(), b"Yc".to_vec()],
                },
                ox_text::Position { lnum: 1, col: 1 },
                ox_text::Position { lnum: 2, col: 1 },
                0,
            )
            .unwrap()
    });
    assert_eq!(
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes()),
        b"aX\nYc\nef"
    );
    assert_eq!(extmark_pos(&session, buffer, namespace, extmark).0, 1);
    assert!(session.with_editor(|editor| editor.window(window).unwrap().cursor.lnum) >= 1);
}

#[test]
fn replace_buffer_text_grouped_undo_replays_members_in_reverse_byte_exact() {
    let (editor, buffer, tab, window) = editor_with_lines(&["alpha", "bravo", "charlie"]);
    let session = session_with(editor);
    let namespace =
        crate::extmark::nvim_create_namespace(&session, OxStr::from("grouped")).unwrap();
    let marks = [
        crate::extmark::nvim_buf_set_extmark(&session, buffer, namespace, 0, 1, dict(&[])).unwrap(),
        crate::extmark::nvim_buf_set_extmark(&session, buffer, namespace, 1, 2, dict(&[])).unwrap(),
        crate::extmark::nvim_buf_set_extmark(&session, buffer, namespace, 2, 1, dict(&[])).unwrap(),
    ];
    let _second =
        session.with_editor_mut(|editor| editor.split_vertical(tab, window, buffer, true).unwrap());

    let pre_text =
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes());
    let pre_marks: Vec<_> = marks
        .iter()
        .map(|id| extmark_pos(&session, buffer, namespace, *id))
        .collect();

    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(0, 1),
                    end: ox_editor::ExtmarkPosition::new(0, 3),
                    replacement: vec![b"LP".to_vec()],
                },
                ox_text::Position { lnum: 1, col: 1 },
                ox_text::Position { lnum: 1, col: 1 },
                0,
            )
            .unwrap();
        editor.buffer_undojoin(buffer).unwrap();
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(1, 2),
                    end: ox_editor::ExtmarkPosition::new(2, 1),
                    replacement: vec![b"Q".to_vec()],
                },
                ox_text::Position { lnum: 2, col: 2 },
                ox_text::Position { lnum: 2, col: 2 },
                0,
            )
            .unwrap();
        editor.buffer_undojoin(buffer).unwrap();
    });
    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(0, 4),
                    end: ox_editor::ExtmarkPosition::new(0, 4),
                    replacement: vec![b"xx".to_vec()],
                },
                ox_text::Position { lnum: 1, col: 4 },
                ox_text::Position { lnum: 1, col: 4 },
                0,
            )
            .unwrap()
    });

    let final_text =
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes());
    let final_marks: Vec<_> = marks
        .iter()
        .map(|id| extmark_pos(&session, buffer, namespace, *id))
        .collect();

    session.with_editor_mut(|editor| editor.buffer_undo(buffer).unwrap());
    assert_eq!(
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes()),
        pre_text
    );
    let undone_marks: Vec<_> = marks
        .iter()
        .map(|id| extmark_pos(&session, buffer, namespace, *id))
        .collect();
    assert_eq!(undone_marks, pre_marks);

    session.with_editor_mut(|editor| editor.buffer_redo(buffer).unwrap());
    assert_eq!(
        session.with_editor(|editor| editor.buffer(buffer).unwrap().text().unwrap().to_bytes()),
        final_text
    );
    let redone_marks: Vec<_> = marks
        .iter()
        .map(|id| extmark_pos(&session, buffer, namespace, *id))
        .collect();
    assert_eq!(redone_marks, final_marks);
}

#[test]
fn replace_buffer_text_undo_to_seq_walks_headers_byte_exact() {
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Snap {
        text: Vec<u8>,
        mark: (i64, i64),
        seq: u64,
    }
    let (editor, buffer, _tab, _window) = editor_with_lines(&["one", "two", "three", "four"]);
    let session = session_with(editor);
    let namespace =
        crate::extmark::nvim_create_namespace(&session, OxStr::from("undo-to-seq")).unwrap();
    let mark =
        crate::extmark::nvim_buf_set_extmark(&session, buffer, namespace, 0, 1, dict(&[])).unwrap();

    let capture = |session: &crate::ApiSession| {
        let (text, seq) = session.with_editor(|editor| {
            (
                editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
                editor.buffer(buffer).unwrap().undo.current_seq(),
            )
        });
        Snap {
            text,
            mark: extmark_pos(session, buffer, namespace, mark),
            seq,
        }
    };

    let mut snaps = vec![capture(&session)];

    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_lines(ox_editor::LineReplaceRequest {
                buffer,
                start: 2,
                end: 2,
                lines: &[b"TWO".to_vec()],
                cursor_before: ox_text::Position { lnum: 2, col: 0 },
                cursor_after: ox_text::Position { lnum: 2, col: 0 },
                timestamp: 0,
            })
            .unwrap();
        editor.sync_buffer_undo(buffer);
    });
    snaps.push(capture(&session));

    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(0, 1),
                    end: ox_editor::ExtmarkPosition::new(0, 2),
                    replacement: vec![b"XY".to_vec()],
                },
                ox_text::Position { lnum: 1, col: 1 },
                ox_text::Position { lnum: 1, col: 1 },
                0,
            )
            .unwrap();
        editor.sync_buffer_undo(buffer);
    });
    snaps.push(capture(&session));

    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_lines(ox_editor::LineReplaceRequest {
                buffer,
                start: 4,
                end: 4,
                lines: &[b"FOUR".to_vec()],
                cursor_before: ox_text::Position { lnum: 4, col: 0 },
                cursor_after: ox_text::Position { lnum: 4, col: 0 },
                timestamp: 0,
            })
            .unwrap();
        editor.sync_buffer_undo(buffer);
    });
    snaps.push(capture(&session));

    session.with_editor_mut(|editor| {
        editor
            .replace_buffer_text(
                buffer,
                &ox_editor::BufferTextEditRequest {
                    start: ox_editor::ExtmarkPosition::new(2, 0),
                    end: ox_editor::ExtmarkPosition::new(2, 1),
                    replacement: vec![b"Z".to_vec(), b"W".to_vec()],
                },
                ox_text::Position { lnum: 3, col: 0 },
                ox_text::Position { lnum: 3, col: 0 },
                0,
            )
            .unwrap();
        editor.sync_buffer_undo(buffer);
    });
    snaps.push(capture(&session));

    let middle = snaps[2].seq;
    let newest = snaps[4].seq;
    let oldest = snaps[0].seq;

    session.with_editor_mut(|editor| editor.buffer_undo_to_seq(buffer, middle).unwrap());
    assert_eq!(capture(&session), snaps[2]);

    session.with_editor_mut(|editor| editor.buffer_undo_to_seq(buffer, newest).unwrap());
    assert_eq!(capture(&session), snaps[4]);

    session.with_editor_mut(|editor| editor.buffer_undo_to_seq(buffer, oldest).unwrap());
    assert_eq!(capture(&session), snaps[0]);
}

fn dict_field<'a>(dict: &'a Dict, key: &str) -> &'a Object {
    dict.0
        .iter()
        .find(|(entry_key, _)| entry_key.as_bytes() == key.as_bytes())
        .map_or_else(|| panic!("missing dict key {key}"), |(_, value)| value)
}

struct FailingWipe;

impl crate::CommandExecutor for FailingWipe {
    fn execute(
        &mut self,
        _session: &crate::ApiSession,
        _commands: &[crate::ExCommand],
    ) -> Result<(), ApiError> {
        Ok(())
    }

    fn remove_buffer(&mut self, _buffer: crate::BufHandle) -> Result<(), ApiError> {
        Err(ApiError::exception("boom"))
    }

    fn define_user_command(
        &mut self,
        _session: &crate::ApiSession,
        _buffer: Option<crate::BufHandle>,
        _command: ox_editor::UserCommand,
        _force: bool,
    ) -> Result<(), ApiError> {
        Ok(())
    }

    fn delete_user_command(
        &mut self,
        _session: &crate::ApiSession,
        _buffer: Option<crate::BufHandle>,
        _name: &str,
    ) -> Result<(), ApiError> {
        Ok(())
    }

    fn list_user_commands(
        &mut self,
        _session: &crate::ApiSession,
        _buffer: Option<crate::BufHandle>,
    ) -> Result<Vec<ox_editor::UserCommand>, ApiError> {
        Ok(Vec::new())
    }

    fn parse_cmdline(
        &mut self,
        _session: &crate::ApiSession,
        _line: &str,
    ) -> Result<Vec<crate::ExCommand>, ApiError> {
        Ok(Vec::new())
    }

    fn evaluate(
        &mut self,
        session: &crate::ApiSession,
        expression: &str,
    ) -> Result<Typval, ApiError> {
        session.with_editor_mut(|editor| {
            evaluate_builtin(
                editor,
                &OxStr::from("eval"),
                vec![Typval::String(OxStr::from(expression))],
            )
        })
    }

    fn call_builtin(
        &mut self,
        session: &crate::ApiSession,
        name: &OxStr,
        args: Vec<Typval>,
    ) -> Result<Typval, ApiError> {
        session.with_editor_mut(|editor| evaluate_builtin(editor, name, args))
    }

    fn change_directory(
        &mut self,
        _session: &crate::ApiSession,
        _path: &str,
    ) -> Result<(), ApiError> {
        Ok(())
    }
}

#[test]
fn user_command_create_list_delete_round_trip() {
    let session = session();
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor::default()),
        Box::new(RecordingExecutor::default()),
    );

    crate::command::nvim_create_user_command(
        &session,
        OxStr::from("Greet"),
        Object::String(OxStr::from("echo 'hi'")),
        dict(&[
            ("nargs", Object::String(OxStr::from("*"))),
            ("bang", Object::Boolean(true)),
            ("bar", Object::Boolean(true)),
            ("register", Object::Boolean(true)),
            ("desc", Object::String(OxStr::from("say hi"))),
            ("complete", Object::String(OxStr::from("file"))),
        ]),
    )
    .unwrap();

    let listed = crate::command::nvim_get_commands(&session, dict(&[])).unwrap();
    let (name, info) = listed.0.first().expect("one listed command");
    assert_eq!(name.to_string_lossy(), "Greet");
    let Object::Dict(info) = info else {
        panic!("command info must be a Dict");
    };
    assert_eq!(
        dict_field(info, "definition"),
        &Object::String(OxStr::from("echo 'hi'"))
    );
    assert_eq!(
        dict_field(info, "desc"),
        &Object::String(OxStr::from("say hi"))
    );
    assert_eq!(dict_field(info, "script_id"), &Object::Integer(-8));
    assert_eq!(dict_field(info, "bang"), &Object::Boolean(true));
    assert_eq!(dict_field(info, "bar"), &Object::Boolean(true));
    assert_eq!(dict_field(info, "register"), &Object::Boolean(true));
    assert_eq!(dict_field(info, "keepscript"), &Object::Boolean(false));
    assert_eq!(dict_field(info, "nargs"), &Object::String(OxStr::from("*")));
    assert_eq!(
        dict_field(info, "complete"),
        &Object::String(OxStr::from("file"))
    );
    assert_eq!(dict_field(info, "complete_arg"), &Object::Nil);
    assert_eq!(dict_field(info, "count"), &Object::Nil);
    assert_eq!(dict_field(info, "range"), &Object::Nil);
    assert_eq!(dict_field(info, "addr"), &Object::Nil);

    assert_eq!(
        crate::command::nvim_create_user_command(
            &session,
            OxStr::from("Greet"),
            Object::String(OxStr::from("echo 'no'")),
            dict(&[("force", Object::Boolean(false))]),
        ),
        Err(ApiError::validation("Command already exists: Greet"))
    );
    crate::command::nvim_create_user_command(
        &session,
        OxStr::from("Greet"),
        Object::String(OxStr::from("echo 'again'")),
        dict(&[]),
    )
    .unwrap();

    crate::command::nvim_del_user_command(&session, OxStr::from("Greet")).unwrap();
    assert!(
        crate::command::nvim_get_commands(&session, dict(&[]))
            .unwrap()
            .0
            .is_empty()
    );
    assert_eq!(
        crate::command::nvim_del_user_command(&session, OxStr::from("Greet")),
        Err(ApiError::exception("Invalid command (not found): Greet"))
    );
}
// A host failure that is not a duplicate definition — e.g. reentrant-executor
// exhaustion on the live host — keeps its own class and message; only the
// known "Command already exists" error is canonicalized.
#[test]
fn user_command_create_preserves_non_duplicate_host_errors() {
    let session = session();
    let executor = RecordingExecutor {
        define_error: Some(ApiError::exception(
            "no free Ex executor for a nested command",
        )),
        ..RecordingExecutor::default()
    };
    crate::set_command_executor(
        &session,
        Box::new(executor),
        Box::new(RecordingExecutor::default()),
    );

    assert_eq!(
        crate::command::nvim_create_user_command(
            &session,
            OxStr::from("Greet"),
            Object::String(OxStr::from("echo 'hi'")),
            dict(&[]),
        ),
        Err(ApiError::exception(
            "no free Ex executor for a nested command"
        )),
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one validation matrix preserves exact user-command error precedence"
)]
fn user_command_create_rejects_invalid_input() {
    type RejectCase = (
        &'static str,
        Object,
        Vec<(&'static str, Object)>,
        &'static str,
    );

    let session = session();
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor::default()),
        Box::new(RecordingExecutor::default()),
    );

    let body = || Object::String(OxStr::from("echo 1"));
    let rejects: Vec<RejectCase> = vec![
        ("t@", body(), Vec::new(), "Invalid command name: 't@'"),
        (
            "test",
            body(),
            Vec::new(),
            "Invalid command name (must start with uppercase): 'test'",
        ),
        (
            "Test",
            body(),
            vec![
                ("range", Object::Boolean(true)),
                ("count", Object::Boolean(true)),
            ],
            "Cannot use both 'range' and 'count'",
        ),
        (
            "Test",
            body(),
            vec![("mystery", Object::Boolean(true))],
            "Invalid key: mystery",
        ),
        (
            "Test",
            body(),
            vec![("complete", Object::String(OxStr::from("file")))],
            "'complete' used without 'nargs'",
        ),
        (
            "Test",
            body(),
            vec![("nargs", Object::Integer(2))],
            "Invalid 'nargs': 2",
        ),
        (
            "Test",
            body(),
            vec![("nargs", Object::String(OxStr::from("xx")))],
            "Invalid 'nargs': 'xx'",
        ),
        (
            "Test",
            body(),
            vec![("desc", Object::Integer(5))],
            "Invalid 'desc': expected String, got Integer",
        ),
        (
            "Test",
            body(),
            vec![("preview", Object::Integer(5))],
            "Invalid 'preview': expected Function, got Integer",
        ),
        (
            "Test",
            body(),
            vec![("complete", Object::Integer(5))],
            "Invalid 'complete': expected Function or String",
        ),
        (
            "Test",
            Object::Integer(5),
            Vec::new(),
            "Invalid 'command': expected Function or String",
        ),
        (
            "Test",
            body(),
            vec![("force", Object::Integer(5))],
            "Invalid 'force': expected Boolean, got Integer",
        ),
    ];
    for (name, command, opts, message) in rejects {
        let created = crate::command::nvim_create_user_command(
            &session,
            OxStr::from(name),
            command,
            dict(&opts),
        );
        assert_eq!(created, Err(ApiError::validation(message)));
    }
    let missing = crate::BufHandle::try_from(9999).unwrap();
    assert_eq!(
        crate::command::nvim_buf_create_user_command(
            &session,
            missing,
            OxStr::from("Bufcmd"),
            body(),
            dict(&[]),
        ),
        Err(ApiError::validation("Invalid buffer id: 9999"))
    );
    assert_eq!(
        crate::command::nvim_buf_del_user_command(&session, missing, OxStr::from("Bufcmd")),
        Err(ApiError::validation("Invalid buffer id: 9999"))
    );
    assert_eq!(
        crate::command::nvim_buf_get_commands(&session, missing, dict(&[])),
        Ok(Dict(Vec::new()))
    );
    assert_eq!(
        crate::command::nvim_get_commands(&session, dict(&[("builtin", Object::Boolean(true))])),
        Err(ApiError::validation("builtin=true not implemented"))
    );
    assert_eq!(
        crate::command::nvim_buf_get_commands(
            &session,
            missing,
            dict(&[("builtin", Object::Boolean(true))])
        ),
        Ok(Dict(Vec::new()))
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one parser scenario compares all serialized command fields"
)]
fn nvim_parse_cmd_serializes_first_command() {
    let (editor, _, _, _) = editor_with_lines(&["one", "two", "three"]);
    let session = session_with(editor);
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor::default()),
        Box::new(RecordingExecutor::default()),
    );
    let parse = |session: &crate::ApiSession, line: &str| {
        crate::command::nvim_parse_cmd(session, OxStr::from(line), dict(&[]))
    };

    let parsed = parse(&session, "echo foo").unwrap();
    assert_eq!(
        dict_field(&parsed, "cmd"),
        &Object::String(OxStr::from("echo"))
    );
    assert_eq!(
        dict_field(&parsed, "args"),
        &Object::Array(vec![Object::String(OxStr::from("foo"))])
    );
    assert_eq!(dict_field(&parsed, "bang"), &Object::Boolean(false));
    assert_eq!(
        dict_field(&parsed, "nargs"),
        &Object::String(OxStr::from("*"))
    );
    assert_eq!(
        dict_field(&parsed, "addr"),
        &Object::String(OxStr::from("none"))
    );
    assert_eq!(
        dict_field(&parsed, "nextcmd"),
        &Object::String(OxStr::from(""))
    );
    assert_eq!(parsed.get(&OxStr::from("range")), None);
    assert_eq!(parsed.get(&OxStr::from("count")), None);
    assert_eq!(parsed.get(&OxStr::from("reg")), None);
    let Object::Dict(mods) = dict_field(&parsed, "mods") else {
        panic!("mods must be a Dict");
    };
    assert_eq!(dict_field(mods, "tab"), &Object::Integer(-1));
    assert_eq!(dict_field(mods, "verbose"), &Object::Integer(-1));
    assert_eq!(dict_field(mods, "silent"), &Object::Boolean(false));
    assert_eq!(dict_field(mods, "split"), &Object::String(OxStr::from("")));
    let Object::Dict(filter) = dict_field(mods, "filter") else {
        panic!("filter must be a Dict");
    };
    assert_eq!(
        dict_field(filter, "pattern"),
        &Object::String(OxStr::from(""))
    );
    assert_eq!(dict_field(filter, "force"), &Object::Boolean(false));

    let ranged = parse(&session, "4,6s/a/b/").unwrap();
    assert_eq!(
        dict_field(&ranged, "cmd"),
        &Object::String(OxStr::from("substitute"))
    );
    assert_eq!(
        dict_field(&ranged, "range"),
        &Object::Array(vec![Object::Integer(4), Object::Integer(6)])
    );
    assert_eq!(
        dict_field(&ranged, "addr"),
        &Object::String(OxStr::from("line"))
    );
    assert_eq!(
        dict_field(&ranged, "args"),
        &Object::Array(vec![Object::String(OxStr::from("/a/b/"))])
    );

    let counted = parse(&session, "buffer 1").unwrap();
    assert_eq!(
        dict_field(&counted, "range"),
        &Object::Array(vec![Object::Integer(1)])
    );
    assert_eq!(dict_field(&counted, "count"), &Object::Integer(1));

    let regged = parse(&session, "1,3delete * 5").unwrap();
    assert_eq!(
        dict_field(&regged, "range"),
        &Object::Array(vec![Object::Integer(3), Object::Integer(7)])
    );
    assert_eq!(dict_field(&regged, "count"), &Object::Integer(7));
    assert_eq!(
        dict_field(&regged, "reg"),
        &Object::String(OxStr::from("*"))
    );
    assert_eq!(dict_field(&regged, "args"), &Object::Array(Vec::new()));

    let bar_separated = parse(&session, "echo one | echo two").unwrap();
    assert_eq!(
        dict_field(&bar_separated, "nextcmd"),
        &Object::String(OxStr::from("echo two"))
    );

    assert_eq!(
        parse(&session, "silent  1Fubar arg | echo hi"),
        Err(ApiError::exception(
            "Parsing command-line: E492: Not an editor command: 1Fubar arg | echo hi"
        ))
    );
    assert_eq!(
        parse(&session, ""),
        Err(ApiError::exception("Parsing command-line"))
    );
    assert_eq!(
        parse(&session, "\" foo"),
        Err(ApiError::exception("Parsing command-line"))
    );
    assert_eq!(
        parse(&session, "echo 1\necho 2"),
        Err(ApiError::validation("Command cannot contain newlines"))
    );
}

#[test]
fn wipe_cleans_buffer_local_commands_but_unload_keeps_them() {
    let (editor, buffer, _tab, _window) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let wiped: Rc<std::cell::RefCell<Vec<crate::BufHandle>>> = Rc::default();
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor {
            commands: Vec::new(),
            message: None,
            users: std::collections::BTreeMap::default(),
            wiped: wiped.clone(),
            operations: Rc::default(),
            define_error: None,
        }),
        Box::new(RecordingExecutor::default()),
    );

    crate::command::nvim_buf_create_user_command(
        &session,
        buffer,
        OxStr::from("Bcmd"),
        Object::String(OxStr::from("echo 1")),
        dict(&[]),
    )
    .unwrap();
    assert!(
        crate::command::nvim_get_commands(&session, dict(&[]))
            .unwrap()
            .0
            .is_empty()
    );

    crate::buffer::nvim_buf_delete(&session, buffer, dict(&[("unload", Object::Boolean(true))]))
        .unwrap();
    assert!(wiped.borrow().is_empty());
    assert_eq!(
        crate::command::nvim_buf_get_commands(&session, buffer, dict(&[]))
            .unwrap()
            .0
            .len(),
        1
    );

    crate::buffer::nvim_buf_delete(&session, buffer, dict(&[])).unwrap();
    assert_eq!(wiped.borrow().as_slice(), &[buffer]);

    let (bare, bare_buffer, _tab, _window) = editor_with_lines(&["two"]);
    let bare_session = session_with(bare);
    crate::buffer::nvim_buf_delete(&bare_session, bare_buffer, dict(&[])).unwrap();

    let (failing, failing_buffer, _tab, _window) = editor_with_lines(&["three"]);
    let failing_session = session_with(failing);
    crate::set_command_executor(
        &failing_session,
        Box::new(FailingWipe),
        Box::new(FailingWipe),
    );
    assert_eq!(
        crate::buffer::nvim_buf_delete(&failing_session, failing_buffer, dict(&[])),
        Err(ApiError::exception("boom"))
    );
}

#[test]
fn nvim_command_parses_through_the_installed_host() {
    let session = session();
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor::default()),
        Box::new(RecordingExecutor::default()),
    );
    assert_eq!(
        crate::global::nvim_command(&session, OxStr::from("Fubar")),
        Err(ApiError::exception("E492: Not an editor command: Fubar"))
    );
}

#[test]
fn user_command_lua_refs_survive_serialization() {
    let session = session();
    crate::set_command_executor(
        &session,
        Box::new(RecordingExecutor::default()),
        Box::new(RecordingExecutor::default()),
    );

    crate::command::nvim_create_user_command(
        &session,
        OxStr::from("Luacmd"),
        Object::LuaRef(41),
        dict(&[
            ("nargs", Object::String(OxStr::from("?"))),
            ("preview", Object::LuaRef(42)),
            ("complete", Object::LuaRef(43)),
        ]),
    )
    .unwrap();

    let listed = crate::command::nvim_get_commands(&session, dict(&[])).unwrap();
    let (_, info) = listed.0.first().expect("listed command");
    let Object::Dict(info) = info else {
        panic!("command info must be a Dict");
    };
    assert_eq!(
        dict_field(info, "definition"),
        &Object::String(OxStr::from(""))
    );
    assert_eq!(dict_field(info, "callback"), &Object::LuaRef(41));
    assert_eq!(dict_field(info, "preview"), &Object::LuaRef(42));
    assert_eq!(dict_field(info, "complete"), &Object::LuaRef(43));
    assert_eq!(dict_field(info, "nargs"), &Object::String(OxStr::from("?")));
}

// ── nvim_strwidth ──────────────────────────────────────────────────────────

/// Table-driven display-width checks mirroring the upstream functional spec
/// (`test/functional/api/vim_spec.lua` `nvim_strwidth`).
#[test]
fn nvim_strwidth_table() {
    let (editor, _, _, _) = editor_with_lines(&[""]);
    let session = session_with(editor);

    // (label, input bytes, expected width with default ambiwidth)
    let cases: &[(&str, &[u8], i64)] = &[
        ("empty", b"", 0),
        ("ascii", b"abc", 3),
        // "neovim" (6) + 19 Japanese chars × 2 cells = 44.
        (
            "japanese",
            "neovimのデザインかなりまともなのになってる。".as_bytes(),
            44,
        ),
        // Combining mark: 'e' + U+0301 (combining acute) → 1 cell.
        ("combining", "e\u{0301}".as_bytes(), 1),
        // ❤️ = U+2764 + U+FE0F (variation selector) → 2 cells.
        ("selector-heart", "❤\u{fe0f}".as_bytes(), 2),
        // ❤ = U+2764 alone → 1 cell.
        ("plain-heart", "❤".as_bytes(), 1),
        // 🏳️‍⚧️ = flag + VS16 + ZWJ + ⚧ + VS16 → 2 cells.
        ("zwj-emoji", "🏳\u{fe0f}\u{200d}⚧\u{fe0f}".as_bytes(), 2),
        // 🧑‍🌾 = person + ZWJ + ear of rice → 2 cells.
        ("zwj-emoji-2", "🧑\u{200d}🌾".as_bytes(), 2),
    ];

    for (label, input, expected) in cases {
        assert_eq!(
            crate::global::nvim_strwidth(&session, OxStr::from(*input)),
            Ok(*expected),
            "{label}: width mismatch"
        );
    }
}

/// Leading NUL yields width 0, matching upstream `mb_string2cells` which stops
/// at the first NUL byte.
#[test]
fn nvim_strwidth_leading_nul() {
    let (editor, _, _, _) = editor_with_lines(&[""]);
    let session = session_with(editor);
    assert_eq!(
        crate::global::nvim_strwidth(&session, OxStr::from(&b"\0abc"[..])),
        Ok(0),
    );
}

/// Interior NUL: only the prefix before the first NUL is measured.
#[test]
fn nvim_strwidth_interior_nul() {
    let (editor, _, _, _) = editor_with_lines(&[""]);
    let session = session_with(editor);
    assert_eq!(
        crate::global::nvim_strwidth(&session, OxStr::from(&b"ab\0cd"[..])),
        Ok(2),
    );
}

/// Malformed UTF-8 *before* the NUL errors; the visible prefix must be valid.
#[test]
fn nvim_strwidth_malformed_before_nul_errors() {
    let (editor, _, _, _) = editor_with_lines(&[""]);
    let session = session_with(editor);
    // 0xFF is not a valid UTF-8 lead byte.
    assert!(matches!(
        crate::global::nvim_strwidth(&session, OxStr::from(&b"\xff\0valid"[..])),
        Err(ApiError::Validation(_))
    ));
}

/// Malformed UTF-8 *after* the NUL is ignored — the suffix is never decoded.
#[test]
fn nvim_strwidth_malformed_after_nul_ignored() {
    let (editor, _, _, _) = editor_with_lines(&[""]);
    let session = session_with(editor);
    assert_eq!(
        crate::global::nvim_strwidth(&session, OxStr::from(&b"abc\0\xff\xfe"[..])),
        Ok(3),
    );
}

/// `ambiwidth=double` treats East Asian Ambiguous characters as wide (2 cells).
/// U+00B7 · is ambiguous: 1 cell with default `single`, 2 with `double`.
#[test]
fn nvim_strwidth_ambiwidth_double() {
    let (editor, _, _, _) = editor_with_lines(&[""]);
    let session = session_with(editor);

    // Default ambiwidth is "single": · is 1 cell.
    assert_eq!(
        crate::global::nvim_strwidth(&session, OxStr::from("·")),
        Ok(1),
    );

    // Switch to "double": · becomes 2 cells.
    set_option_value(
        &session,
        "ambiwidth",
        Object::String(OxStr::from("double")),
        &[],
    )
    .unwrap();

    assert_eq!(
        crate::global::nvim_strwidth(&session, OxStr::from("·")),
        Ok(2),
    );
}

// ===========================================================================
// ApiSession ownership battery (unit A1)
// ===========================================================================

mod session_ownership {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ox_editor::Editor;
    use ox_rpc::ChannelId;
    use ox_types::{BufHandle, OxStr};

    use crate::session::ApiSession;

    fn session() -> ApiSession {
        ApiSession::new(Rc::new(RefCell::new(Editor::new())))
    }

    #[test]
    fn two_sessions_on_one_editor_cannot_observe_each_other() {
        // WHY one shared carrier: the retired thread-local model keyed state
        // by editor identity, so only sessions wrapping the SAME editor can
        // distinguish per-session state from per-editor state. This test
        // fails against that model and passes only under session ownership.
        let carrier = Rc::new(RefCell::new(Editor::new()));
        let first = ApiSession::new(Rc::clone(&carrier));
        let second = ApiSession::new(Rc::clone(&carrier));

        first.with_state_mut(|state| {
            state.namespaces.insert(OxStr::from("shared"), 7);
        });

        let second_namespaces =
            second.with_state(|state| state.namespaces.contains_key(&OxStr::from("shared")));
        assert!(
            !second_namespaces,
            "a second session on the same thread must not see the first session's state"
        );
        let first_namespaces =
            first.with_state(|state| state.namespaces.contains_key(&OxStr::from("shared")));
        assert!(
            first_namespaces,
            "the first session still sees its own state"
        );
    }

    #[test]
    fn session_drop_releases_state_with_no_global_residue() {
        let first = session();
        first.with_state_mut(|state| state.paste_cancelled = true);
        drop(first);

        // A fresh session on the same thread starts from defaults: nothing
        // survived the drop.
        let second = session();
        assert!(!second.with_state(|state| state.paste_cancelled));
    }

    #[test]
    fn caller_guard_after_teardown_is_inert() {
        let api = session();
        let guard = api.enter_rpc_call(ChannelId::new(9));
        drop(api);

        // Dropping the guard after the session is gone must neither panic
        // nor resurrect any state.
        drop(guard);
    }

    #[test]
    fn nested_caller_guards_remove_own_frames_in_any_order() {
        let api = session();
        let outer = api.enter_rpc_call(ChannelId::new(1));
        let inner = api.enter_internal_call();
        assert_eq!(api.requesting_channel(), None, "inner masks the outer RPC");

        drop(inner);
        assert_eq!(
            api.requesting_channel(),
            Some(ChannelId::new(1)),
            "outer frame survives the inner guard"
        );
        drop(outer);
        assert_eq!(api.requesting_channel(), None);
    }

    #[test]
    fn filetype_suppression_is_session_local() {
        let carrier = Rc::new(RefCell::new(Editor::new()));
        let first = ApiSession::new(Rc::clone(&carrier));
        let second = ApiSession::new(Rc::clone(&carrier));

        let buffer = BufHandle::try_from(1).expect("handle 1 is valid");
        first.with_state_mut(|state| {
            state.filetype_dispatches.push((buffer, "rust".to_owned()));
        });

        let second_in_flight = second.with_state(|state| state.filetype_dispatches.len());
        assert_eq!(
            second_in_flight, 0,
            "in-flight filetype suppression must not leak across sessions"
        );
        let first_in_flight = first.with_state(|state| state.filetype_dispatches.len());
        assert_eq!(first_in_flight, 1);
    }
}
// ===========================================================================
// nvim_buf_call context-switch tests
// (context.c ctx_switch/ctx_restore, buffer_spec.lua:2597-2816)
// ===========================================================================

type Callback = Box<dyn Fn(&crate::ApiSession) -> Result<Vec<Object>, String>>;

struct CallbackLua {
    callback: Callback,
}

impl crate::LuaExecutor for CallbackLua {
    fn exec(
        &mut self,
        _session: &crate::ApiSession,
        _code: &str,
        args: Vec<Object>,
    ) -> Result<Object, String> {
        Ok(Object::Array(args))
    }
    fn invoke_callback(
        &mut self,
        _session: &crate::ApiSession,
        _reference: usize,
        args: Vec<Object>,
    ) -> Result<Object, String> {
        Ok(Object::Array(args))
    }
    fn call_ref(
        &mut self,
        session: &crate::ApiSession,
        _reference: usize,
        _args: Vec<Object>,
    ) -> Result<Vec<Object>, String> {
        (self.callback)(session)
    }
    fn free_callback(&mut self, _reference: usize) -> Result<(), String> {
        Ok(())
    }
}

fn install_callback(session: &crate::ApiSession, callback: Callback) {
    crate::set_lua_executor(
        session,
        Box::new(CallbackLua { callback }),
        Box::new(CallbackLua {
            callback: Box::new(|_| Ok(Vec::new())),
        }),
    );
}

fn buf_call(session: &crate::ApiSession, buffer: crate::BufHandle, callback: Callback) {
    install_callback(session, callback);
    let _ = crate::buffer::nvim_buf_call(session, buffer, crate::convert::LuaRef(1));
}

fn enter_visual(session: &crate::ApiSession) {
    let machine = Rc::new(RefCell::new(ModeMachine::default()));
    machine.borrow_mut().mode = Mode::Visual(ox_editor::VisualState::new(
        ox_text::Position { lnum: 1, col: 0 },
        ox_editor::VisualKind::Character,
    ));
    crate::set_mode_machine(session, machine);
}

fn is_visual(session: &crate::ApiSession) -> bool {
    let machine = crate::runtime::mode_machine(session);
    machine.is_some_and(|m| matches!(m.borrow().mode, Mode::Visual(_)))
}

#[test]
fn buf_call_same_buffer_preserves_visual_mode() {
    let (editor, buffer, _tab, _win) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    enter_visual(&session);
    buf_call(&session, buffer, Box::new(|_| Ok(Vec::new())));
    assert!(
        is_visual(&session),
        "visual mode must survive a same-buffer call"
    );
}

#[test]
fn buf_call_same_buffer_callback_ends_visual_stays_ended() {
    let (editor, buffer, _tab, _win) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    enter_visual(&session);
    buf_call(
        &session,
        buffer,
        Box::new(|session| {
            let machine = crate::runtime::mode_machine(session);
            if let Some(m) = machine {
                m.borrow_mut().mode = Mode::Normal(ox_editor::NormalState::default());
            }
            Ok(Vec::new())
        }),
    );
    assert!(
        !is_visual(&session),
        "visual mode must stay ended after callback ends it"
    );
}

#[test]
fn buf_call_cross_buffer_restores_caller_buffer() {
    let (editor, buf1, _tab, _win) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let buf2 = session.with_editor_mut(|editor| editor.create_buffer(true).unwrap());
    let observed = Rc::new(RefCell::new(None));
    let obs = observed.clone();
    buf_call(
        &session,
        buf2,
        Box::new(move |session| {
            *obs.borrow_mut() = session.with_editor(Editor::current_buffer);
            Ok(Vec::new())
        }),
    );
    assert_eq!(
        *observed.borrow(),
        Some(buf2),
        "callback runs in target buffer"
    );
    assert_eq!(
        session.with_editor(Editor::current_buffer),
        Some(buf1),
        "caller buffer is restored after the call"
    );
}

#[test]
fn buf_call_enters_window_already_showing_target() {
    let (mut editor, buf1, tab, win1) = editor_with_lines(&["one"]);
    let buf2 = editor.create_buffer(true).unwrap();
    let win2 = editor.split_vertical(tab, win1, buf2, true).unwrap();
    editor.set_current_window(win1).unwrap();
    let session = session_with(editor);
    let observed_win = Rc::new(RefCell::new(None));
    let observed_buf = Rc::new(RefCell::new(None));
    let ow = observed_win.clone();
    let ob = observed_buf.clone();
    buf_call(
        &session,
        buf2,
        Box::new(move |session| {
            *ow.borrow_mut() = session.with_editor(Editor::current_window);
            *ob.borrow_mut() = session.with_editor(Editor::current_buffer);
            Ok(Vec::new())
        }),
    );
    assert_eq!(
        *observed_win.borrow(),
        Some(win2),
        "entered the window showing buf2"
    );
    assert_eq!(
        *observed_buf.borrow(),
        Some(buf2),
        "callback runs with buf2 current"
    );
    assert_eq!(
        session.with_editor(Editor::current_window),
        Some(win1),
        "restored to caller window"
    );
    assert_eq!(
        session.with_editor(Editor::current_buffer),
        Some(buf1),
        "caller buffer restored"
    );
    assert_eq!(
        session.with_editor(|editor| editor.window(win2).unwrap().buffer),
        buf2,
        "win2 buffer unchanged"
    );
}

#[test]
fn buf_call_window_keeps_buffer_when_callback_closes_original_window() {
    let (mut editor, _buf1, tab, origin) = editor_with_lines(&["one"]);
    let buf2 = editor.create_buffer(true).unwrap();
    let other_win = editor.split_vertical(tab, origin, buf2, true).unwrap();
    editor.set_current_window(origin).unwrap();
    let session = session_with(editor);
    let new_buf = session.with_editor_mut(|editor| editor.create_buffer(true).unwrap());
    buf_call(
        &session,
        buf2,
        Box::new(move |session| {
            session.with_editor_mut(|editor| {
                let _ = editor.close_window(tab, origin, true);
            });
            session.with_editor_mut(|editor| {
                let _ = editor.set_window_buffer(
                    other_win,
                    new_buf,
                    ox_editor::BufferRelease::KeepLoaded,
                );
            });
            Ok(Vec::new())
        }),
    );
    assert_eq!(
        session.with_editor(Editor::current_window),
        Some(other_win),
        "stay in the nvim_buf_call window when origin is closed"
    );
    assert_eq!(
        session.with_editor(|editor| editor.window(other_win).unwrap().buffer),
        buf2,
        "other_win buffer is restored to buf2"
    );
}

#[test]
fn buf_call_nested_hidden_buffers_unwind_correctly() {
    let (editor, buf0, _tab, _win) = editor_with_lines(&["base"]);
    let session = session_with(editor);
    // Three sequential nested-style calls, each creating a scratch buffer.
    // After all three, the original buffer must be restored.
    for _ in 0..5 {
        let scratch = session.with_editor_mut(|editor| editor.create_buffer(false).unwrap());
        let observed = Rc::new(RefCell::new(None));
        let obs = observed.clone();
        buf_call(
            &session,
            scratch,
            Box::new(move |session| {
                *obs.borrow_mut() = session.with_editor(Editor::current_buffer);
                Ok(Vec::new())
            }),
        );
        assert_eq!(
            *observed.borrow(),
            Some(scratch),
            "callback ran in scratch buffer"
        );
    }
    assert_eq!(
        session.with_editor(Editor::current_buffer),
        Some(buf0),
        "original buffer restored after sequential hidden-buffer calls"
    );
}

#[test]
fn buf_call_restores_visual_state_even_on_callback_error() {
    let (editor, buf1, _tab, _win) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let buf2 = session.with_editor_mut(|editor| editor.create_buffer(true).unwrap());
    enter_visual(&session);
    buf_call(
        &session,
        buf2,
        Box::new(|_| Err("callback error".to_string())),
    );
    assert!(
        is_visual(&session),
        "visual mode must be restored even when the callback errors"
    );
    assert_eq!(
        session.with_editor(Editor::current_buffer),
        Some(buf1),
        "caller buffer restored after callback error"
    );
}

#[test]
fn buf_get_offset_empty_buffer_returns_zero_and_one_for_eof() {
    let (editor, buffer, _tab, _win) = editor_with_lines(&[""]);
    let session = session_with(editor);
    // Empty buffer: line 0 → offset 0, EOF pseudo-line → 1 (virtual newline
    // because fixeol defaults true and has_eol is false).
    assert_eq!(
        crate::buffer::nvim_buf_get_offset(&session, buffer, 0).unwrap(),
        0
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_offset(&session, buffer, 1).unwrap(),
        1
    );
}

#[test]
fn buf_get_offset_one_line_no_eol_counts_virtual_newline_at_eof() {
    let (editor, buffer, _tab, _win) = editor_with_lines(&["text"]);
    let session = session_with(editor);
    // "text" without trailing newline: offset 0 for line 0, 5 (4 + 1
    // virtual newline) for the EOF pseudo-line.
    assert_eq!(
        crate::buffer::nvim_buf_get_offset(&session, buffer, 0).unwrap(),
        0
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_offset(&session, buffer, 1).unwrap(),
        5
    );
}

#[test]
fn buf_get_offset_multi_line_no_eol_counts_virtual_newline_at_eof() {
    let (editor, buffer, _tab, _win) = editor_with_lines(&["aaa", "bbb"]);
    let session = session_with(editor);
    // "aaa\nbbb" without trailing newline: offsets 0, 4, 8 (7 + 1 virtual).
    assert_eq!(
        crate::buffer::nvim_buf_get_offset(&session, buffer, 0).unwrap(),
        0
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_offset(&session, buffer, 1).unwrap(),
        4
    );
    assert_eq!(
        crate::buffer::nvim_buf_get_offset(&session, buffer, 2).unwrap(),
        8
    );
}

#[test]
fn buf_get_offset_no_virtual_newline_when_eol_and_fixeol_both_false() {
    let (editor, buffer, _tab, _win) = editor_with_lines(&["aaa", "bbb", "ccc"]);
    let session = session_with(editor);
    // Disable both eol and fixeol: no virtual newline at EOF.
    crate::global::nvim_set_option_value(
        &session,
        OxStr::from("eol"),
        Object::Boolean(false),
        dict(&[("buf", Object::Buffer(buffer))]),
    )
    .unwrap();
    crate::global::nvim_set_option_value(
        &session,
        OxStr::from("fixeol"),
        Object::Boolean(false),
        dict(&[("buf", Object::Buffer(buffer))]),
    )
    .unwrap();
    // "aaa\nbbb\nccc" = 11 bytes, no virtual newline.
    assert_eq!(
        crate::buffer::nvim_buf_get_offset(&session, buffer, 3).unwrap(),
        11
    );
}

#[test]
fn open_tabpage_loads_unloaded_buffer_before_read_hooks() {
    let (editor, source, original_tab, _) = editor_with_lines(&["source"]);
    let session = session_with(editor);
    let path = std::env::temp_dir().join(format!(
        "oxvim-api-open-tabpage-hook-{}",
        std::process::id()
    ));
    std::fs::write(&path, b"target\n").unwrap();
    crate::global::nvim_set_option_value(
        &session,
        OxStr::from("shiftwidth"),
        Object::Integer(8),
        dict(&[("scope", Object::String(OxStr::from("local")))]),
    )
    .unwrap();
    let target = session.with_editor_mut(|editor| {
        let target = editor.create_buffer(true).unwrap();
        let state = editor.buffer_mut(target).unwrap();
        state.set_name(OxStr::from(path.to_string_lossy().as_ref()));
        state.unload().unwrap();
        target
    });
    crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("BufReadPre")),
        dict(&[
            ("pattern", Object::String(OxStr::from("*"))),
            ("command", Object::String(OxStr::from("setlocal shiftwidth=3"))),
        ]),
    )
    .unwrap();
    let setlocal = Rc::new(|session: &crate::ApiSession| {
        crate::global::nvim_set_option_value(
            session,
            OxStr::from("shiftwidth"),
            Object::Integer(3),
            dict(&[("scope", Object::String(OxStr::from("local")))]),
        )
        .map(|_| ())
    });
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: Rc::new(RefCell::new(Vec::new())),
            reenter: Some(setlocal),
        }),
        Box::new(ActionRecorder::default()),
    );

    let tab = crate::tabpage::nvim_open_tabpage(&session, target, false, dict(&[])).unwrap();

    assert_eq!(
        session.with_editor(Editor::current_tabpage),
        Some(original_tab)
    );
    assert_eq!(
        session.with_editor(Editor::current_buffer),
        Some(source)
    );
    assert_eq!(
        session.with_editor(|editor| {
            editor
                .buffer(target)
                .unwrap()
                .text()
                .unwrap()
                .line(1)
        }),
        Ok(b"target".to_vec())
    );
    assert_eq!(
        session.with_editor(|editor| {
            editor
                .options()
                .get_buffer(target, "shiftwidth")
                .unwrap()
                .clone()
        }),
        OptionValue::Number(3)
    );
    assert_eq!(
        session.with_editor(|editor| {
            editor
                .options()
                .get_buffer(source, "shiftwidth")
                .unwrap()
                .clone()
        }),
        OptionValue::Number(8)
    );
    let window = session
        .with_editor(|editor| editor.tabpage(tab).unwrap().current_window());
    assert_eq!(
        session.with_editor(|editor| editor.window(window).unwrap().buffer),
        target
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn open_tabpage_appends_and_enters_by_default() {
    let (mut editor, _buffer, first_tab, _) = editor_with_lines(&["one"]);
    let other = editor.create_buffer(true).unwrap();
    editor.set_current_tabpage(first_tab).unwrap();
    let session = session_with(editor);

    let tab = crate::tabpage::nvim_open_tabpage(&session, other, true, dict(&[])).unwrap();
    let (current, tabs, windows, window, window_buffer) = session.with_editor(|editor| {
        (
            editor.current_tabpage(),
            editor.tabpages(),
            editor.tabpage(tab).unwrap().windows(),
            editor.tabpage(tab).unwrap().current_window(),
            editor
                .window(editor.tabpage(tab).unwrap().current_window())
                .unwrap()
                .buffer,
        )
    });
    assert_eq!(current, Some(tab));
    assert_eq!(tabs, [first_tab, tab]);
    assert_eq!(windows.len(), 1);
    assert_eq!(window_buffer, other);
    // Entering also makes the new tabpage's window current.
    assert_eq!(
        session.with_editor(|editor| editor.window_tabpage(window).unwrap()),
        tab
    );
    assert_eq!(
        session.with_editor(ox_editor::Editor::current_window),
        Some(window)
    );
}

#[test]
fn open_tabpage_leaves_current_tabpage_and_window_when_not_entering() {
    let (mut editor, buffer, first_tab, first_window) = editor_with_lines(&["one"]);
    editor.set_current_tabpage(first_tab).unwrap();
    let session = session_with(editor);

    let tab = crate::tabpage::nvim_open_tabpage(&session, buffer, false, dict(&[])).unwrap();
    let (current, tabs, window_buffer) = session.with_editor(|editor| {
        (
            editor.current_tabpage(),
            editor.tabpages(),
            editor
                .window(editor.tabpage(tab).unwrap().current_window())
                .unwrap()
                .buffer,
        )
    });
    assert_eq!(current, Some(first_tab));
    assert_eq!(tabs, [first_tab, tab]);
    // The new tabpage still exists with its own window showing the buffer.
    assert_eq!(window_buffer, buffer);
    assert_eq!(
        session.with_editor(ox_editor::Editor::current_window),
        Some(first_window)
    );
}

#[test]
fn open_tabpage_honors_after_positions() {
    let (mut editor, buffer, tab1, _) = editor_with_lines(&["one"]);
    let tab2 = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let session = session_with(editor);

    // after = 0 puts the new tabpage first.
    let first = crate::tabpage::nvim_open_tabpage(
        &session,
        buffer,
        false,
        dict(&[("after", Object::Integer(0))]),
    )
    .unwrap();
    assert_eq!(session.with_editor(Editor::tabpages), [first, tab1, tab2]);

    // after = 1 inserts before tabpage number 2 (`tab1`).
    let before_tab2 = crate::tabpage::nvim_open_tabpage(
        &session,
        buffer,
        false,
        dict(&[("after", Object::Integer(1))]),
    )
    .unwrap();
    assert_eq!(
        session.with_editor(Editor::tabpages),
        [first, before_tab2, tab1, tab2]
    );

    // after = N + 1 appends past the end.
    let last = crate::tabpage::nvim_open_tabpage(
        &session,
        buffer,
        false,
        dict(&[("after", Object::Integer(9))]),
    )
    .unwrap();
    assert_eq!(
        session.with_editor(Editor::tabpages),
        [first, before_tab2, tab1, tab2, last]
    );

    // Negative `after` falls back to the "after current" default.
    let after_current = crate::tabpage::nvim_open_tabpage(
        &session,
        buffer,
        false,
        dict(&[("after", Object::Integer(-1))]),
    )
    .unwrap();
    assert_eq!(
        session.with_editor(Editor::tabpages),
        [first, before_tab2, tab1, tab2, after_current, last]
    );
}

#[test]
fn open_tabpage_rejects_invalid_buffer_and_config() {
    let (editor, buffer, _, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);

    let invalid_buffer = crate::BufHandle::try_from(999).unwrap();
    assert_eq!(
        crate::tabpage::nvim_open_tabpage(&session, invalid_buffer, true, dict(&[])),
        Err(ApiError::validation("Invalid buffer id: 999"))
    );
    assert_eq!(
        crate::tabpage::nvim_open_tabpage(
            &session,
            buffer,
            true,
            dict(&[("mystery", Object::Boolean(true))])
        ),
        Err(ApiError::validation(
            "Invalid 'config.mystery': unexpected key"
        ))
    );
    assert_eq!(
        crate::tabpage::nvim_open_tabpage(
            &session,
            buffer,
            true,
            dict(&[("after", Object::String(OxStr::from("1")))])
        ),
        Err(ApiError::validation(
            "Invalid 'config.after': expected Integer"
        ))
    );
    // Every rejected call leaves the editor untouched.
    assert_eq!(session.with_editor(|editor| editor.tabpages().len()), 1);
}

#[test]
fn open_tabpage_fires_creation_events_in_new_tab_context() {
    let (mut editor, buffer, original, _) = editor_with_lines(&["one"]);
    for event in [Event::WinNew, Event::TabNew] {
        editor
            .autocmds_mut()
            .register_legacy(
                &[event],
                "*",
                &AutocmdKind::ExString("echo fired".to_owned()),
                &AutocmdOptions::default(),
            )
            .unwrap();
    }
    let session = session_with(editor);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let callback_tabs = Rc::new(RefCell::new(Vec::new()));
    let callback_tabs_for_reentry = callback_tabs.clone();
    crate::set_autocmd_executor(
        &session,
        Box::new(ActionRecorder {
            actions: actions.clone(),
            reenter: Some(Rc::new(move |session| {
                callback_tabs_for_reentry
                    .borrow_mut()
                    .push(session.with_editor(Editor::current_tabpage));
                Ok(())
            })),
        }),
        Box::new(ActionRecorder::default()),
    );

    let created = crate::tabpage::nvim_open_tabpage(&session, buffer, false, dict(&[])).unwrap();

    assert_eq!(
        actions
            .borrow()
            .iter()
            .map(|action| action.event)
            .collect::<Vec<_>>(),
        [Event::WinNew, Event::TabNew]
    );
    assert_eq!(
        callback_tabs.borrow().as_slice(),
        [Some(created), Some(created)]
    );
    assert_eq!(
        session.with_editor(Editor::current_tabpage),
        Some(original),
        "enter=false restores the original tab only after creation callbacks"
    );
}

#[test]
fn open_tabpage_registry_dispatch_returns_new_tabpage() {
    let (editor, buffer, first_tab, _) = editor_with_lines(&["one"]);
    let session = session_with(editor);
    let registry = crate::core().unwrap();
    let (metadata, dispatch) = registry.get("nvim_open_tabpage").unwrap();
    assert_eq!(metadata.since, 14);
    assert!(metadata.textlock);

    let tab = dispatch(
        &session,
        &[
            Object::Buffer(buffer),
            Object::Boolean(true),
            Object::Dict(dict(&[])),
        ],
    )
    .unwrap();
    assert_eq!(tab, Object::Tabpage(crate::TabHandle::try_from(2).unwrap()));
    assert_eq!(
        session.with_editor(ox_editor::Editor::current_tabpage),
        Some(crate::TabHandle::try_from(2).unwrap())
    );
    assert_eq!(
        session.with_editor(Editor::tabpages),
        [first_tab, crate::TabHandle::try_from(2).unwrap()]
    );
}

// ---- task-W4 boundary tests ---------------------------------------------

#[test]
fn w4_mark_names_validate() {
    let session = session();
    let two = OxStr::from("AB");
    assert_eq!(
        crate::global::nvim_del_mark(&session, two.clone()),
        Err(ApiError::validation(
            "Invalid mark name (must be a single char): 'AB'"
        ))
    );
    let lower = OxStr::from("a");
    assert_eq!(
        crate::global::nvim_del_mark(&session, lower.clone()),
        Err(ApiError::validation(
            "Invalid mark name (must be file/uppercase): 'a'"
        ))
    );
    // A valid global mark deletes cleanly and reports success even unset.
    assert!(crate::global::nvim_del_mark(&session, OxStr::from("A")).is_ok());
}

#[test]
fn w4_get_mark_unset_reports_zero_position() {
    let session = session();
    let mark = crate::global::nvim_get_mark(&session, OxStr::from("B"), dict(&[])).unwrap();
    assert_eq!(mark[0], Object::Integer(0));
    assert_eq!(mark[1], Object::Integer(0));
    assert_eq!(mark[2], Object::Integer(0));
    assert_eq!(mark[3], Object::String(OxStr::from("")));
}

#[test]
fn w4_call_dict_function_rejects_invalid_dict() {
    let session = session();
    let result = crate::global::nvim_call_dict_function(
        &session,
        Object::Integer(7),
        OxStr::from("anything"),
        Vec::new(),
    );
    assert_eq!(
        result,
        Err(ApiError::validation(
            "Invalid dict argument: expected String or Dict"
        ))
    );
}

#[test]
fn w4_options_info_carry_upstream_fields() {
    let session = session();
    let all = crate::global::nvim_get_all_options_info(&session).unwrap();
    let Object::Dict(fields) = all
        .iter()
        .find(|(name, _)| name.as_bytes() == b"winminheight".as_slice())
        .map(|(_, value)| value.clone())
        .expect("winminheight is a known option")
    else {
        panic!("option info is a dict");
    };
    for key in ["name", "shortname", "type", "default", "scope", "was_set"] {
        assert!(
            fields
                .iter()
                .any(|(name, _)| name.as_bytes() == key.as_bytes()),
            "missing field {key}"
        );
    }
    let single =
        crate::global::nvim_get_option_info2(&session, OxStr::from("winminheight"), dict(&[]))
            .unwrap();
    assert_eq!(single, Dict(fields.iter().cloned().collect::<Vec<_>>()));
}

#[test]
fn w4_input_mouse_validates_button_and_action() {
    let session = session();
    let bad_button = crate::global::nvim_input_mouse(
        &session,
        OxStr::from("thumb"),
        OxStr::from("press"),
        OxStr::from(""),
        0,
        1,
        1,
    );
    assert_eq!(
        bad_button,
        Err(ApiError::validation("invalid button or action"))
    );
    let bad_action = crate::global::nvim_input_mouse(
        &session,
        OxStr::from("wheel"),
        OxStr::from("press"),
        OxStr::from(""),
        0,
        1,
        1,
    );
    assert_eq!(
        bad_action,
        Err(ApiError::validation("invalid button or action"))
    );
    assert!(
        crate::global::nvim_input_mouse(
            &session,
            OxStr::from("left"),
            OxStr::from("press"),
            OxStr::from(""),
            0,
            1,
            1
        )
        .is_ok()
    );
}

#[test]
fn w4_eval_statusline_renders_literal_and_filename() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let session = session_with(editor);
    let result =
        crate::global::nvim_eval_statusline(&session, OxStr::from("static"), dict(&[])).unwrap();
    let value = result
        .iter()
        .find(|(key, _)| key.as_bytes() == b"str".as_slice());
    let Some((_, Object::String(rendered))) = value else {
        panic!("statusline result carries str");
    };
    assert_eq!(rendered.as_bytes(), b"static".as_slice());
    let unknown = crate::global::nvim_eval_statusline(
        &session,
        OxStr::from("x"),
        dict(&[("winid", Object::Integer(9_999))]),
    );
    assert!(unknown.is_err(), "unknown winid must fail");
}

#[test]
fn nvim_echo_accepts_documented_progress_options_and_rejects_unknown_keys() {
    let session = session();
    let chunks = vec![Object::Array(vec![Object::String(OxStr::from(
        "checking %s",
    ))])];

    // health.lua-shaped progress call must not error and must return a real
    // positive message-id instead of -1.
    let progress = dict(&[
        ("kind", Object::String(OxStr::from("progress"))),
        ("source", Object::String(OxStr::from("vim.health"))),
        ("title", Object::String(OxStr::from("checkhealth"))),
        ("status", Object::String(OxStr::from("running"))),
        ("percent", Object::Integer(42)),
    ]);
    let result = crate::global::nvim_echo(&session, chunks.clone(), false, progress).unwrap();
    assert!(matches!(result, Object::Integer(id) if id > 0), "got {result:?}");

    // `spellfile.lua` uses `kind = 'empty'`.
    let empty = dict(&[("kind", Object::String(OxStr::from("empty")))]);
    let result = crate::global::nvim_echo(&session, chunks.clone(), false, empty).unwrap();
    assert!(matches!(result, Object::Integer(id) if id > 0), "got {result:?}");

    // A caller-provided string `id` is returned as-is.
    let with_id = dict(&[
        ("kind", Object::String(OxStr::from("progress"))),
        ("source", Object::String(OxStr::from("tests"))),
        ("status", Object::String(OxStr::from("running"))),
        ("id", Object::String(OxStr::from("my.progress"))),
    ]);
    assert_eq!(
        crate::global::nvim_echo(&session, chunks.clone(), false, with_id).unwrap(),
        Object::String(OxStr::from("my.progress"))
    );

    // Truly unknown keys still fail.
    let unknown = dict(&[("bogus", Object::Boolean(true))]);
    assert!(matches!(
        crate::global::nvim_echo(&session, chunks, false, unknown),
        Err(ApiError::Validation(_))
    ));
}

#[test]
fn nvim_echo_validates_integer_message_ids() {
    let session = session();
    let chunks = vec![Object::Array(vec![Object::String(OxStr::from("hello"))])];

    // The first automatic id is established before an explicit integer can
    // refer to it.
    assert_eq!(
        crate::global::nvim_echo(&session, chunks.clone(), false, dict(&[])),
        Ok(Object::Integer(1))
    );
    assert_eq!(
        crate::global::nvim_echo(
            &session,
            chunks.clone(),
            false,
            dict(&[("id", Object::Integer(1))]),
        ),
        Ok(Object::Integer(1))
    );
    assert_eq!(
        crate::global::nvim_echo(
            &session,
            chunks.clone(),
            false,
            dict(&[("id", Object::Integer(2))]),
        ),
        Err(ApiError::validation("Invalid 'id': 2"))
    );
    assert_eq!(
        crate::global::nvim_echo(
            &session,
            chunks.clone(),
            false,
            dict(&[
                ("id", Object::Integer(0)),
                ("verbose", Object::Boolean(true)),
            ]),
        ),
        Err(ApiError::validation("Invalid 'id': 0"))
    );
    assert_eq!(
        crate::global::nvim_echo(
            &session,
            chunks.clone(),
            false,
            dict(&[("id", Object::String(OxStr::from("custom")))]),
        ),
        Ok(Object::String(OxStr::from("custom")))
    );
    // `Union(Integer, String)` is represented as an Object by both upstream
    // keyset decoders, so other object types remain caller-defined as well.
    assert_eq!(
        crate::global::nvim_echo(
            &session,
            chunks,
            false,
            dict(&[("id", Object::Boolean(true))]),
        ),
        Ok(Object::Boolean(true))
    );
}

#[test]
fn nvim_echo_rejects_wrong_typed_option_values() {
    let session = session();
    let chunks = vec![Object::Array(vec![Object::String(OxStr::from("hello"))])];

    // Boolean members fail the strict boolean pop (`api_spec.lua:301`).
    for key in ["err", "verbose", "_truncate"] {
        assert_eq!(
            crate::global::nvim_echo(&session, chunks.clone(), false, dict(&[
                (key, Object::String(OxStr::from("x"))),
            ])),
            Err(ApiError::validation(format!("Invalid '{key}': not a boolean"))),
            "{key}",
        );
    }
    // Numbers coerce against zero, `nil` is `false`.
    assert!(crate::global::nvim_echo(
        &session,
        chunks.clone(),
        false,
        dict(&[("err", Object::Integer(2))])
    )
    .is_ok());
    assert!(crate::global::nvim_echo(
        &session,
        chunks.clone(),
        false,
        dict(&[("verbose", Object::Float(0.0))])
    )
    .is_ok());

    // The remaining typed members fail the RPC-side `VALIDATE_T` shape.
    for (key, expected) in [
        ("kind", "String"),
        ("title", "String"),
        ("status", "String"),
        ("source", "String"),
        ("percent", "Integer"),
        ("data", "Dict"),
    ] {
        assert_eq!(
            crate::global::nvim_echo(&session, chunks.clone(), false, dict(&[
                (key, Object::Array(vec![Object::Integer(1)])),
            ])),
            Err(ApiError::validation(format!(
                "Invalid '{key}': expected {expected}, got Array"
            ))),
            "{key}",
        );
    }
    assert_eq!(
        crate::global::nvim_echo(
            &session,
            chunks.clone(),
            false,
            dict(&[("data", Object::Nil)]),
        ),
        Err(ApiError::validation(
            "Invalid 'data': expected Dict, got nil"
        )),
    );

}

#[test]
fn nvim_echo_applies_upstream_progress_validations() {
    let session = session();
    let chunks = vec![Object::Array(vec![Object::String(OxStr::from("hello"))])];
    let progress = |extra: &[(&str, Object)]| {
        let mut entries = vec![
            ("kind", Object::String(OxStr::from("progress"))),
            ("source", Object::String(OxStr::from("tests"))),
            ("status", Object::String(OxStr::from("running"))),
        ];
        for (key, value) in extra {
            if let Some(slot) = entries.iter_mut().find(|(name, _)| name == key) {
                slot.1 = value.clone();
            } else {
                entries.push((key, value.clone()));
            }
        }
        dict(&entries)
    };

    // The five progress-only fields are rejected on a plain message with
    // the defaulted kind label (`messages_spec.lua:3665`).
    for (key, value) in [
        ("status", Object::String(OxStr::from("running"))),
        ("title", Object::String(OxStr::from("TestSuit"))),
        ("data", Object::Dict(Dict(vec![(OxStr::from("tag"), Object::Integer(1))]))),
        ("percent", Object::Integer(0)),
        ("source", Object::String(OxStr::from("tests"))),
    ] {
        assert_eq!(
            crate::global::nvim_echo(&session, chunks.clone(), false, dict(&[
                (key, value),
            ])),
            Err(ApiError::validation(
                "Conflict: title/source/status/percent/data not allowed with kind='echo'"
            )),
            "{key}",
        );
    }
    assert!(crate::global::nvim_echo(
        &session,
        chunks.clone(),
        false,
        dict(&[("data", Object::Array(vec![]))]),
    )
    .is_ok());

    assert_eq!(
        crate::global::nvim_echo(&session, chunks.clone(), false, dict(&[
            ("kind", Object::String(OxStr::from("empty"))),
            ("title", Object::String(OxStr::from("TestSuit"))),
        ])),
        Err(ApiError::validation(
            "Conflict: title/source/status/percent/data not allowed with kind='empty'"
        )),
    );
    assert_eq!(
        crate::global::nvim_echo(&session, chunks.clone(), true, dict(&[
            ("err", Object::Boolean(true)),
            ("status", Object::String(OxStr::from("running"))),
        ])),
        Err(ApiError::validation(
            "Conflict: title/source/status/percent/data not allowed with kind='echoerr'"
        )),
    );
    assert_eq!(
        crate::global::nvim_echo(&session, chunks.clone(), true, dict(&[
            ("title", Object::String(OxStr::from("TestSuit"))),
        ])),
        Err(ApiError::validation(
            "Conflict: title/source/status/percent/data not allowed with kind='echomsg'"
        )),
    );
    // `status` only takes the documented values, and only on progress
    // messages (`messages_spec.lua:3691`).
    assert_eq!(
        crate::global::nvim_echo(&session, chunks.clone(), false, progress(&[(
            "status",
            Object::String(OxStr::from("live")),
        )])),
        Err(ApiError::validation(
            "Invalid 'status': expected success|failed|running|cancel, got live"
        )),
    );

    // `percent` is range-checked (`messages_spec.lua:3702`, `:3712`).
    for percent in [-1, 101] {
        assert_eq!(
            crate::global::nvim_echo(&session, chunks.clone(), false, progress(&[(
                "percent",
                Object::Integer(percent),
            )])),
            Err(ApiError::validation("Invalid 'percent': out of range")),
            "{percent}",
        );
    }
    assert!(crate::global::nvim_echo(&session, chunks.clone(), false, progress(&[(
        "percent",
        Object::Integer(100),
    )]))
    .is_ok());
    assert_eq!(
        crate::global::nvim_echo(&session, chunks.clone(), false, dict(&[
            ("kind", Object::String(OxStr::from("progress"))),
            ("source", Object::String(OxStr::from("tests"))),
        ])),
        Err(ApiError::validation(
            "Invalid 'status': expected success|failed|running|cancel"
        )),
    );

    assert_eq!(
        crate::global::nvim_echo(&session, chunks.clone(), false, dict(&[
            ("kind", Object::String(OxStr::from("progress"))),
            ("status", Object::String(OxStr::from("running"))),
            ("source", Object::String(OxStr::from(""))),
        ])),
        Err(ApiError::validation("Required: 'opts.source'")),
    );

    // `source` is required and the reserved name "nvim" is rejected
    // (`messages_spec.lua:3736`, `vim_spec.lua:4190`).
    assert_eq!(
        crate::global::nvim_echo(&session, chunks.clone(), false, dict(&[
            ("kind", Object::String(OxStr::from("progress"))),
            ("status", Object::String(OxStr::from("running"))),
        ])),
        Err(ApiError::validation("Required: 'opts.source'")),
    );
    assert_eq!(
        crate::global::nvim_echo(&session, chunks.clone(), false, dict(&[
            ("kind", Object::String(OxStr::from("progress"))),
            ("status", Object::String(OxStr::from("success"))),
            ("source", Object::String(OxStr::from("nvim"))),
        ])),
        Err(ApiError::validation("Invalid 'source': 'nvim'")),
    );
}

#[test]
fn nvim_echo_data_accepts_dict_and_empty_array() {
    let session = session();
    let chunks = vec![Object::Array(vec![Object::String(OxStr::from("hello"))])];
    let data = dict(&[
        ("kind", Object::String(OxStr::from("progress"))),
        ("source", Object::String(OxStr::from("tests"))),
        ("status", Object::String(OxStr::from("running"))),
        ("data", Object::Dict(Dict(vec![(
            OxStr::from("tag"),
            Object::Integer(1),
        )]))),
    ]);
    assert!(crate::global::nvim_echo(&session, chunks.clone(), false, data).is_ok());
    let empty_array = dict(&[
        ("kind", Object::String(OxStr::from("progress"))),
        ("source", Object::String(OxStr::from("tests"))),
        ("status", Object::String(OxStr::from("running"))),
        ("data", Object::Array(vec![])),
    ]);
    assert!(crate::global::nvim_echo(&session, chunks, false, empty_array).is_ok());
}

#[test]
fn nvim_echo_auto_ids_are_scoped_to_each_api_session() {
    let first = session();
    let second = session();
    let chunks = vec![Object::Array(vec![Object::String(OxStr::from("hello"))])];

    assert_eq!(
        crate::global::nvim_echo(&first, chunks.clone(), false, dict(&[])),
        Ok(Object::Integer(1))
    );
    assert_eq!(
        crate::global::nvim_echo(&first, chunks.clone(), false, dict(&[])),
        Ok(Object::Integer(2))
    );
    assert_eq!(
        crate::global::nvim_echo(&second, chunks.clone(), false, dict(&[])),
        Ok(Object::Integer(1))
    );

    drop(first);
    let fresh = session();
    assert_eq!(
        crate::global::nvim_echo(&fresh, chunks, false, dict(&[])),
        Ok(Object::Integer(1))
    );
}

#[test]
fn nvim_echo_plain_echo_still_pushes_message() {
    let session = session();
    let before = session.with_editor(|editor| editor.messages().len());
    let chunks = vec![Object::Array(vec![Object::String(OxStr::from("hello"))])];
    let result = crate::global::nvim_echo(&session, chunks, true, dict(&[])).unwrap();
    assert!(matches!(result, Object::Integer(id) if id > 0), "got {result:?}");
    let after = session.with_editor(|editor| editor.messages().len());
    assert_eq!(after, before + 1);
}

#[test]
fn nvim_echo_progress_fires_progress_autocmd() {
    let session = session();
    let recorder = ActionRecorder::default();
    crate::set_autocmd_executor(
        &session,
        Box::new(recorder.clone()),
        Box::new(recorder.clone()),
    );
    crate::autocmd::nvim_create_autocmd(
        &session,
        Object::String(OxStr::from("Progress")),
        dict(&[(
            "command",
            Object::String(OxStr::from("let g:progress_fired = 1")),
        )]),
    )
    .unwrap();

    let chunks = vec![Object::Array(vec![Object::String(OxStr::from("msg"))])];

    let opts = dict(&[
        ("kind", Object::String(OxStr::from("progress"))),
        ("source", Object::String(OxStr::from("test"))),
        ("title", Object::String(OxStr::from("test"))),
        ("status", Object::String(OxStr::from("running"))),
        ("percent", Object::Integer(25)),
    ]);
    assert!(matches!(
        crate::global::nvim_echo(&session, chunks, false, opts).unwrap(),
        Object::Integer(id) if id > 0
    ));

    let actions = recorder.actions.borrow();
    assert_eq!(actions.len(), 1, "Progress should fire exactly once");
    assert_eq!(actions[0].event.as_str(), "Progress");
    let Object::Dict(data) = actions[0].data.as_ref().expect("Progress data") else {
        panic!("Progress data must be a dict");
    };
    assert_eq!(
        data.get(&OxStr::from("kind")),
        Some(&Object::String(OxStr::from("progress")))
    );
    assert_eq!(
        data.get(&OxStr::from("title")),
        Some(&Object::String(OxStr::from("test")))
    );
    assert_eq!(
        data.get(&OxStr::from("status")),
        Some(&Object::String(OxStr::from("running")))
    );
    assert_eq!(
        data.get(&OxStr::from("percent")),
        Some(&Object::Integer(25))
    );
    assert!(data.get(&OxStr::from("id")).is_some());
    assert!(data.get(&OxStr::from("text")).is_some());
}

#[test]
fn extmark_hl_group_round_trips_string_and_array_source_order() {
    let (editor, buffer, _, _) = editor_with_lines(&["one", "two"]);
    let session = session_with(editor);
    let ns = crate::extmark::nvim_create_namespace(&session, OxStr::from("tests")).unwrap();

    let string_id = crate::extmark::nvim_buf_set_extmark(
        &session,
        buffer,
        ns,
        0,
        0,
        dict(&[("hl_group", Object::String(OxStr::from("Visual")))]),
    )
    .unwrap();

    let array_id = crate::extmark::nvim_buf_set_extmark(
        &session,
        buffer,
        ns,
        1,
        0,
        dict(&[(
            "hl_group",
            Object::Array(vec![
                Object::String(OxStr::from("Visual")),
                Object::String(OxStr::from("Search")),
            ]),
        )]),
    )
    .unwrap();

    let marks = crate::extmark::nvim_buf_get_extmarks(
        &session,
        buffer,
        ns,
        Object::Array(vec![Object::Integer(0), Object::Integer(0)]),
        Object::Integer(-1),
        dict(&[("details", Object::Boolean(true))]),
    )
    .unwrap();

    let string_mark = marks
        .iter()
        .find(|mark| mark[0] == Object::Integer(string_id))
        .expect("string hl_group mark");
    let Object::Dict(string_details) = &string_mark[3] else {
        panic!("details must be a dict");
    };
    assert_eq!(
        string_details.get(&OxStr::from("hl_group")),
        Some(&Object::String(OxStr::from("Visual")))
    );

    let array_mark = marks
        .iter()
        .find(|mark| mark[0] == Object::Integer(array_id))
        .expect("array hl_group mark");
    let Object::Dict(array_details) = &array_mark[3] else {
        panic!("details must be a dict");
    };
    assert_eq!(
        array_details.get(&OxStr::from("hl_group")),
        Some(&Object::Array(vec![
            Object::String(OxStr::from("Visual")),
            Object::String(OxStr::from("Search")),
        ]))
    );
}
