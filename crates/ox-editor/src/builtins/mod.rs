//! Editor-stateful builtin dispatch.
//!
//! `ox-eval` serves every builtin that needs nothing but typvals. The families
//! below need editor state — windows, buffers, jobs, the message list, the
//! script stack — so the Ex host routes them here first: [`route`] maps a
//! builtin name to its [`Family`], and [`call`] hands the name to that
//! family's dispatcher.

pub(crate) mod assert;
pub(crate) mod buffer;
pub(crate) mod environment;
pub(crate) mod eval;
pub(crate) mod filesystem;
pub(crate) mod input;
pub(crate) mod position;
pub(crate) mod process;
pub(crate) mod window;

use ox_eval::{is_buffer_builtin, EvalError, Scope};
use ox_types::{OxStr, Special, Typval};

use crate::excmd_exec::EvalHost;
use crate::script::FileIO;

/// One family of editor-stateful builtins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Family {
    /// Argument-list queries, served by [`crate::arglist`].
    ArgList,
    /// `assert_*` claims.
    Assert,
    /// Buffer variables, buffer identity, and buffer lines.
    Buffer,
    /// Working directory, clock, highlight table, event-loop state.
    Environment,
    /// Expression, Ex, Lua, and typeahead evaluation.
    Eval,
    /// Paths and files, served by [`crate::fs_builtins`].
    FileSystem,
    /// Prompts that read a reply.
    Input,
    /// Cursor position reads and writes.
    Position,
    /// Jobs, channels, and the shell.
    Process,
    /// Window geometry, window identity, screen cells.
    Window,
}

/// Maps a builtin name to the family that serves it, or `None` when the name
/// needs no editor state and the typval-only dispatcher owns it.
pub(crate) fn route(name: &str) -> Option<Family> {
    let family = match name {
        "assert_equal" | "assert_equalfile" | "assert_exception" | "assert_fails"
        | "assert_false" | "assert_inrange" | "assert_match" | "assert_notequal"
        | "assert_notmatch" | "assert_report" | "assert_true" => Family::Assert,
        "append" | "bufexists" | "bufname" | "bufnr" | "getbufvar" | "last_buffer_nr"
        | "setbufvar" => Family::Buffer,
        "chdir" | "eventhandler" | "highlight_exists" | "hlexists" | "strftime" => {
            Family::Environment
        }
        "eval" | "execute" | "exists" | "expand" | "feedkeys" | "fullcommand" | "funcref"
        | "function" | "luaeval" | "submatch" => Family::Eval,
        "swapfilelist" => Family::FileSystem,
        "getchar" | "getcharstr" | "input" | "inputdialog" | "inputlist" => Family::Input,
        "charcol" | "col" | "cursor" | "getcharpos" | "getcurpos" | "getcursorcharpos"
        | "getpos" | "line" | "setcharpos" | "setcursorcharpos" | "setpos" | "virtcol" => {
            Family::Position
        }
        "chansend" | "jobpid" | "jobsend" | "jobstart" | "jobstop" | "jobwait" | "system"
        | "systemlist" => Family::Process,
        "screenattr" | "screenchar" | "screenchars" | "screenstring" | "tabpagenr" | "win_getid"
        | "winheight" | "winnr" | "winwidth" => Family::Window,
        _ => return predicate_family(name),
    };
    Some(family)
}

/// Families whose membership is a predicate owned by the serving module.
fn predicate_family(name: &str) -> Option<Family> {
    if crate::fs_builtins::is_filesystem_builtin(name) {
        return Some(Family::FileSystem);
    }
    if crate::arglist::is_arglist_builtin(name) {
        return Some(Family::ArgList);
    }
    if is_buffer_builtin(name) {
        return Some(Family::Buffer);
    }
    None
}

/// Serves `name` from the family [`route`] chose for it.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    family: Family,
    name: &str,
    args: Vec<Typval>,
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    match family {
        Family::ArgList => crate::arglist::call(host.editor, name, args),
        Family::Assert => assert::call(host, name, args, scope),
        Family::Buffer => buffer::call(host, name, args, scope),
        Family::Environment => environment::call(host, name, args),
        Family::Eval => eval::call(host, name, args, scope),
        Family::FileSystem => filesystem::call(host, name, args),
        Family::Input => input::call(host, name, args),
        Family::Position => position::call(host, name, args),
        Family::Process => process::call(host, name, args, scope),
        Family::Window => window::call(host, name, args),
    }
}

pub(crate) fn input_string_arg(value: &Typval) -> ox_eval::Result<OxStr> {
    match value {
        Typval::String(value) => Ok(value.clone()),
        Typval::Number(value) => Ok(OxStr::from(value.to_string().as_str())),
        Typval::Bool(value) => Ok(OxStr::from(if *value { "v:true" } else { "v:false" })),
        Typval::Special(Special::Null) => Ok(OxStr::from("")),
        Typval::List(_) => Err(EvalError::new("E730", 0, "Using a List as a String")),
        Typval::Dict(_) => Err(EvalError::new("E731", 0, "Using a Dictionary as a String")),
        Typval::Float(_) => Err(EvalError::new("E806", 0, "Using a Float as a String")),
        _ => Err(EvalError::new("E729", 0, "Using invalid value as a String")),
    }
}
