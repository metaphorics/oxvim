//! Editor-stateful builtin dispatch.
//!
//! `ox-eval` serves every builtin that needs nothing but typvals. The families
//! below need editor state — windows, buffers, jobs, the message list, the
//! script stack — so the Ex host routes them here first: [`route`] maps a
//! builtin name to its [`Family`], and [`call`] hands the name to that
//! family's dispatcher.

pub(crate) mod assert;
pub(crate) mod buffer;
pub(crate) mod completion;
pub(crate) mod environment;
pub(crate) mod eval;
pub(crate) mod filesystem;
pub(crate) mod fold;
pub(crate) mod input;
pub(crate) mod mapping;
pub(crate) mod matches;
pub(crate) mod position;
pub(crate) mod process;
pub(crate) mod quickfix;
pub(crate) mod register;
pub(crate) mod search;
pub(crate) mod tag;
pub(crate) mod window;

use ox_eval::{EvalError, Scope, is_buffer_builtin};
use ox_types::{OxStr, Special, Typval};

use crate::excmd_exec::{EvalHost, ExEditorAccess};
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
    /// Fold queries, served by [`fold`].
    Fold,
    /// Prompts that read a reply.
    Input,
    /// Mapping and abbreviation queries, served by [`mapping`].
    Mapping,
    /// Cursor position reads and writes.
    Position,
    /// Jobs, channels, and the shell.
    Process,
    /// Window geometry, window identity, screen cells.
    Window,
    /// Buffer search from the cursor.
    Search,
    /// Quickfix list queries, served by [`crate::quickfix`].
    Quickfix,
    /// Register reads and writes, served by [`register`].
    Register,
    /// Insert-completion and `getcompletion`, served by [`completion`].
    Completion,
    /// Match highlighting, served by [`matches`].
    Match,
    /// Tags-file queries, served by [`tag`].
    Tag,
}

/// Maps a builtin name to the family that serves it, or `None` when the name
/// needs no editor state and the typval-only dispatcher owns it.
pub(crate) fn route(name: &str) -> Option<Family> {
    let family = match name {
        "assert_beeps" | "assert_nobeep" | "assert_equal" | "assert_equalfile"
        | "assert_exception" | "assert_fails" | "assert_false" | "assert_inrange"
        | "assert_match" | "assert_notequal" | "assert_notmatch" | "assert_report"
        | "assert_true" => Family::Assert,
        "append" | "appendbufline" | "bufadd" | "bufexists" | "bufload" | "bufname" | "bufnr"
        | "changenr" | "deletebufline" | "getbufinfo" | "getbufline" | "getbufvar"
        | "getchangelist" | "last_buffer_nr" | "prompt_getprompt" | "prompt_setprompt"
        | "setbufline" | "setbufvar" | "undotree" => Family::Buffer,
        "api_info" | "chdir" | "defer" | "eventhandler" | "highlight_exists" | "hlID"
        | "hlexists" | "shellescape" | "stdpath" | "strdisplaywidth" | "strftime" | "mode"
        | "swapname" => Family::Environment,
        "eval" | "execute" | "exists" | "expand" | "feedkeys" | "fullcommand" | "funcref"
        | "function" | "luaeval" | "submatch" => Family::Eval,
        "swapfilelist" => Family::FileSystem,
        "foldclosed" | "foldclosedend" | "foldlevel" => Family::Fold,
        "getchar" | "getcharstr" | "input" | "inputdialog" | "inputlist" => Family::Input,
        "maparg" | "mapcheck" | "hasmapto" => Family::Mapping,
        "charcol" | "col" | "cursor" | "getcharpos" | "getcurpos" | "getcursorcharpos"
        | "getpos" | "getregion" | "getregionpos" | "line" | "line2byte" | "setcharpos"
        | "setcursorcharpos" | "setpos" | "virtcol" => Family::Position,
        "chansend" | "jobpid" | "jobsend" | "jobstart" | "jobstop" | "jobwait" | "system"
        | "systemlist" => Family::Process,
        "search" | "searchpair" | "searchpairpos" | "searchcount" => Family::Search,
        "getqflist" | "setqflist" | "getloclist" | "setloclist" => Family::Quickfix,
        "screenattr" | "screenchar" | "screenchars" | "screenstring" | "screencol"
        | "screenrow" | "tabpagenr" | "tabpagewinnr" | "win_getid" | "win_gotoid" | "winbufnr"
        | "winheight" | "winnr" | "winwidth" | "winsaveview" | "winrestview" | "winline"
        | "wincol" | "getwinvar" | "setwinvar" | "winlayout" | "getwininfo" => Family::Window,

        "getreg" | "getregtype" | "setreg" | "getreginfo" => Family::Register,
        "complete" | "complete_info" | "getcompletion" => Family::Completion,
        "matchadd" | "matchaddpos" | "matchdelete" | "clearmatches" | "getmatches"
        | "setmatches" | "matcharg" => Family::Match,
        "taglist" | "gettagstack" | "settagstack" => Family::Tag,
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
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    family: Family,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    match family {
        Family::ArgList => host
            .access
            .with_ex_editor(|editor| crate::arglist::call(editor, name, args)),
        Family::Assert => assert::call(host, name, args, scope),
        Family::Buffer => buffer::call(host, name, args, scope),
        Family::Environment => environment::call(host, name, args),
        Family::Eval => eval::call(host, name, args, scope),
        Family::FileSystem => filesystem::call(host, name, args),
        Family::Fold => fold::call(host, name, args),
        Family::Input => input::call(host, name, args),
        Family::Mapping => mapping::call(host, name, args, scope),
        Family::Position => position::call(host, name, args),
        Family::Process => process::call(host, name, args, scope),
        Family::Search => search::call(host, name, args, scope),
        Family::Quickfix => host
            .access
            .with_ex_editor(|editor| crate::quickfix::call(editor, name, args)),
        Family::Register => register::call(host, name, args, scope),
        Family::Completion => host
            .access
            .with_ex_editor(|editor| completion::call(editor, name, args)),
        Family::Match => host
            .access
            .with_ex_editor(|editor| matches::call(editor, name, args)),
        Family::Tag => tag::call(host, name, args),
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
        // `tv_get_string_buf_chk` (`typval.c:4684-4685`) renders a Float with
        // `%g`; E806 belongs only to `check_can_index` (`eval.c:3225-3229`).
        Typval::Float(number) => Ok(ox_eval::float_as_string(*number)),
        _ => Err(EvalError::new("E729", 0, "Using invalid value as a String")),
    }
}
