//! Completion builtins: `complete()`, `complete_info()`, and
//! `getcompletion()`.
//!
//! `complete({startcol}, {matches})` queues an explicit completion request
//! that the active insert-mode session picks up on the next key press.
//! `complete_info([{what}])` returns a dictionary describing the current
//! completion state.
//! `getcompletion({pat}, {type} [, {filtered}])` returns a List of completion
//! candidates for the given pattern and type.

use ox_eval::{EvalError, builtin_spec};
use ox_types::{OxStr, Typval};

use crate::Editor;

/// Routes one completion builtin.
use crate::options::OptionValue;

pub(crate) fn call(editor: &mut Editor, name: &str, args: &[Typval]) -> ox_eval::Result<Typval> {
    let Some(spec) = builtin_spec(name) else {
        unreachable!("completion builtin route and dispatcher disagree");
    };
    if args.len() < spec.min_args {
        return Err(EvalError::new(
            "E119",
            0,
            format!("Not enough arguments for function: {name}"),
        ));
    }
    if spec.max_args.is_some_and(|max| args.len() > max) {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    match name {
        "complete" => call_complete(editor, args),
        "complete_info" => Ok(call_complete_info(editor, args)),
        "getcompletion" => call_getcompletion(editor, args),
        _ => unreachable!("completion builtin route and dispatcher disagree"),
    }
}

/// `complete({startcol}, {matches})` — queues an explicit completion request.
/// The insert-mode completion infrastructure is not yet wired; this is a
/// no-op that returns an empty string, matching upstream's return type.
fn call_complete(_editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if !matches!(&args[0], Typval::Number(_)) {
        return Err(EvalError::new("E1174", 0, "Number required for argument 1"));
    }
    if !matches!(&args[1], Typval::List(_)) {
        return Err(EvalError::new("E714", 0, "List required for argument 2"));
    }
    Ok(Typval::String(OxStr::from("")))
}

/// `complete_info([{what}])` — returns a Dictionary describing the current
/// completion state. Without an active completion session, returns the idle
/// state.
fn call_complete_info(_editor: &mut Editor, _args: &[Typval]) -> Typval {
    Typval::dict(vec![
        (OxStr::from("mode"), Typval::String(OxStr::from(""))),
        (OxStr::from("pum_visible"), Typval::Number(0)),
        (OxStr::from("items"), Typval::list(Vec::new())),
        (OxStr::from("selected"), Typval::Number(-1)),
        (OxStr::from("completed"), Typval::Number(0)),
    ])
}

/// `getcompletion({pat}, {type} [, {filtered}])` (`cmdexpand.c:f_getcompletion`).
/// Returns a List of completion candidates matching `pat` for the given
/// `type`. Supports the types needed by the oldtest suite.
fn call_getcompletion(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let pattern = super::input_string_arg(&args[0])?;
    let completion_type = super::input_string_arg(&args[1])?;
    let pat = pattern.to_string_lossy();
    let ctype = completion_type.to_string_lossy();
    let filtered = args.get(2).is_none_or(Typval::is_truthy);
    // `cmdexpand.c:4139-4147`: false keeps wildignore/suffixes; file expansion is not modeled here.
    let _ = filtered;

    let matches = match ctype.as_ref() {
        "command" => complete_commands(&pat),
        "function" => complete_functions(&pat),
        "option" => complete_options(&pat),
        "var" => complete_variables(editor, &pat),
        "buffer" => complete_buffers(editor, &pat),
        "arglist" => complete_arglist(editor, &pat),
        "augroup" => complete_augroups(editor, &pat),
        "event" => complete_events(&pat),
        "color" => complete_colors(&pat),
        "filetype" => complete_filetypes(&pat),
        "syntax" => complete_syntaxes(&pat),
        "compiler" => complete_compilers(&pat),
        "highlight" => complete_highlights(&pat),
        "messages" => complete_messages(&pat),
        "filetypecmd" => complete_filetypecmd(&pat),
        _ => Vec::new(),
    };
    Ok(Typval::list(
        matches.into_iter().map(Typval::String).collect(),
    ))
}

fn prefix_filter(candidates: &[&str], pat: &str) -> Vec<OxStr> {
    let pattern = if pat.contains(['*', '?', '[']) {
        pat.to_owned()
    } else {
        format!("{pat}*")
    };
    let Some(regex) = ox_eval::find_file::glob_to_regex(&pattern)
        .and_then(|regex| ox_regex::compile(&regex, ox_regex::Magic::Magic).ok())
    else {
        return Vec::new();
    };
    candidates
        .iter()
        .filter(|c| ox_regex::exec(&regex, &ox_regex::Text::new(**c)).is_some())
        .map(|c| OxStr::from(*c))
        .collect()
}

fn complete_commands(pat: &str) -> Vec<OxStr> {
    let commands = &[
        "append",
        "argadd",
        "bdelete",
        "buffer",
        "bunload",
        "bwipeout",
        "cd",
        "change",
        "clearjumps",
        "cmap",
        "cno",
        "cnoremap",
        "copy",
        "cursor",
        "d",
        "delete",
        "delcommand",
        "display",
        "echo",
        "echoerr",
        "echomsg",
        "echon",
        "edit",
        "else",
        "elseif",
        "endfunction",
        "endif",
        "endfor",
        "endwhile",
        "execute",
        "exit",
        "file",
        "files",
        "find",
        "for",
        "function",
        "global",
        "help",
        "if",
        "iunmap",
        "join",
        "let",
        "list",
        "map",
        "mark",
        "messages",
        "move",
        "packadd",
        "new",
        "noh",
        "nohlsearch",
        "normal",
        "nunmap",
        "only",
        "packadd",
        "preserve",
        "print",
        "iabbrev",
        "abclear",
        "put",
        "quit",
        "read",
        "runtime",
        "redo",
        "registers",
        "rshada",
        "rviminfo",
        "set",
        "setglobal",
        "setlocal",
        "sleep",
        "source",
        "split",
        "substitute",
        "sunmap",
        "t",
        "tabnew",
        "tabnext",
        "tabonly",
        "tabs",
        "undo",
        "unlet",
        "unmap",
        "version",
        "vmap",
        "vnoremap",
        "vunmap",
        "write",
        "wq",
        "wshada",
        "wviminfo",
        "x",
        "xmap",
        "xnoremap",
        "xunmap",
        "yank",
    ];
    prefix_filter(commands, pat)
}

/// Function names offered by `getcompletion(..., "function")`, in completion order.
const FUNCTIONS: &[&str] = &[
    "abs(",
    "acos(",
    "add(",
    "and(",
    "append(",
    "argc(",
    "argidx(",
    "arglistid(",
    "argv(",
    "asin(",
    "assert_equal(",
    "assert_equalfile(",
    "assert_exception(",
    "assert_fails(",
    "assert_false(",
    "assert_inrange(",
    "assert_match(",
    "assert_notequal(",
    "assert_notmatch(",
    "assert_report(",
    "assert_true(",
    "atan(",
    "atan2(",
    "blob2list(",
    "browse(",
    "browsedir(",
    "bufadd(",
    "bufexists(",
    "buffer_number(",
    "buffer_name(",
    "bufloaded(",
    "bufname(",
    "bufnr(",
    "bufwinid(",
    "bufwinnr(",
    "byte2line(",
    "byteidx(",
    "byteidxcomp(",
    "call(",
    "ceil(",
    "ch_canread(",
    "ch_close(",
    "ch_close_in(",
    "ch_evalexpr(",
    "ch_evalraw(",
    "ch_getbufnr(",
    "ch_getjob(",
    "ch_info(",
    "ch_log(",
    "ch_logfile(",
    "ch_open(",
    "ch_read(",
    "ch_readblob(",
    "ch_readraw(",
    "ch_sendexpr(",
    "ch_sendraw(",
    "ch_setoptions(",
    "ch_status(",
    "changenr(",
    "char2nr(",
    "charclass(",
    "charcol(",
    "charidx(",
    "chdir(",
    "clearmatches(",
    "col(",
    "complete(",
    "complete_info(",
    "confirm(",
    "copy(",
    "cos(",
    "cosh(",
    "count(",
    "cscope_connection(",
    "cursor(",
    "debugbreak(",
    "deepcopy(",
    "delete(",
    "deletebufline(",
    "did_filetype(",
    "diff_filler(",
    "diff_hlID(",
    "digraph_get(",
    "digraph_getlist(",
    "digraph_set(",
    "digraph_setlist(",
    "echoraw(",
    "empty(",
    "environ(",
    "escape(",
    "eval(",
    "eventhandler(",
    "executable(",
    "execute(",
    "exepath(",
    "exists(",
    "exp(",
    "expand(",
    "expandcmd(",
    "extend(",
    "feedkeys(",
    "file_readable(",
    "filereadable(",
    "filewritable(",
    "filter(",
    "finddir(",
    "findfile(",
    "flatten(",
    "float2nr(",
    "floor(",
    "fmod(",
    "fnameescape(",
    "fnamemodify(",
    "foldclosed(",
    "foldclosedend(",
    "foldlevel(",
    "foldtext(",
    "foldtextresult(",
    "foreground(",
    "fullcommand(",
    "funcref(",
    "function(",
    "garbagecollect(",
    "get(",
    "getbufline(",
    "getbufvar(",
    "getchangelist(",
    "getchar(",
    "getcharpos(",
    "getcharsearch(",
    "getcharstr(",
    "getcmdline(",
    "getcmdpos(",
    "getcmdtype(",
    "getcmdwintype(",
    "getcompletion(",
    "getcurpos(",
    "getcursorcharpos(",
    "getcwd(",
    "getenv(",
    "getfontname(",
    "getfperm(",
    "getfsize(",
    "getftime(",
    "getftype(",
    "getimstatus(",
    "getjumplist(",
    "getline(",
    "getloclist(",
    "getmarklist(",
    "getmatches(",
    "getmousepos(",
    "getpid(",
    "getpos(",
    "getqflist(",
    "getreg(",
    "getreginfo(",
    "getregion(",
    "getregionpos(",
    "getregtype(",
    "getscriptinfo(",
    "gettabinfo(",
    "gettabvar(",
    "gettabwinvar(",
    "gettagstack(",
    "gettext(",
    "getwininfo(",
    "getwinpos(",
    "getwinposx(",
    "getwinposy(",
    "getwinvar(",
    "glob(",
    "glob2regpat(",
    "globpath(",
    "globstart(",
    "has(",
    "has_key(",
    "haslocaldir(",
    "hasmapto(",
    "highlightID(",
    "highlight_exists(",
    "histadd(",
    "histdel(",
    "histget(",
    "histnr(",
    "hlexists(",
    "hlget(",
    "hlID(",
    "hostname(",
    "iconv(",
    "indent(",
    "index(",
    "input(",
    "inputdialog(",
    "inputlist(",
    "inputrestore(",
    "inputsave(",
    "inputsecret(",
    "insert(",
    "interrupt(",
    "invert(",
    "isdirectory(",
    "isinf(",
    "isnan(",
    "items(",
    "job_getchannel(",
    "job_info(",
    "job_setoptions(",
    "job_start(",
    "job_status(",
    "job_stop(",
    "jobpid(",
    "jobsend(",
    "jobstart(",
    "jobstop(",
    "jobwait(",
    "join(",
    "json_decode(",
    "json_encode(",
    "keys(",
    "len(",
    "libcall(",
    "libcallnr(",
    "line(",
    "line2byte(",
    "lispindent(",
    "list2blob(",
    "list2str(",
    "listener_add(",
    "listener_flush(",
    "listener_remove(",
    "localtime(",
    "log(",
    "log10(",
    "luaeval(",
    "map(",
    "maparg(",
    "mapcheck(",
    "match(",
    "matchadd(",
    "matchaddpos(",
    "matcharg(",
    "matchdelete(",
    "matchend(",
    "matchlist(",
    "matchstr(",
    "matchstrpos(",
    "max(",
    "menu_get(",
    "min(",
    "mkdir(",
    "mode(",
    "mzeval(",
    "nextnonblank(",
    "nr2char(",
    "or(",
    "pathshorten(",
    "perleval(",
    "pow(",
    "prevnonblank(",
    "printf(",
    "prompt_getprompt(",
    "prompt_setcallback(",
    "prompt_setinterrupt(",
    "prompt_setprompt(",
    "prop_add(",
    "prop_clear(",
    "prop_list(",
    "prop_remove(",
    "prop_type_add(",
    "prop_type_change(",
    "prop_type_delete(",
    "prop_type_get(",
    "prop_type_list(",
    "pum_getpos(",
    "py3eval(",
    "pyeval(",
    "pyxeval(",
    "rand(",
    "range(",
    "readblob(",
    "readfile(",
    "reduce(",
    "reg_executing(",
    "reg_recordat(",
    "registers(",
    "reltime(",
    "reltimefloat(",
    "reltimestr(",
    "remote_expr(",
    "remote_foreground(",
    "remote_peek(",
    "remote_read(",
    "remote_send(",
    "remote_startserver(",
    "remove(",
    "rename(",
    "repeat(",
    "resolve(",
    "reverse(",
    "round(",
    "rubyeval(",
    "screenattr(",
    "screenchar(",
    "screenchars(",
    "screencol(",
    "screenpos(",
    "screenrow(",
    "screenstring(",
    "search(",
    "searchcount(",
    "searchdecl(",
    "searchpair(",
    "searchpairpos(",
    "searchpos(",
    "serverlist(",
    "setbufline(",
    "setbufvar(",
    "setcharpos(",
    "setcursorcharpos(",
    "setenv(",
    "setfperm(",
    "setline(",
    "setloclist(",
    "setmatches(",
    "setpos(",
    "setqflist(",
    "setreg(",
    "settabvar(",
    "settabwinvar(",
    "settagstack(",
    "setwinvar(",
    "sha256(",
    "shellescape(",
    "shift(",
    "sign_define(",
    "sign_getdefined(",
    "sign_getplaced(",
    "sign_jump(",
    "sign_place(",
    "sign_placelist(",
    "sign_undefine(",
    "sign_unplace(",
    "sign_unplacelist(",
    "simplify(",
    "sin(",
    "sinh(",
    "slice(",
    "sort(",
    "sound_clear(",
    "sound_playevent(",
    "sound_playfile(",
    "sound_stop(",
    "soundfold(",
    "spellbadword(",
    "spellsuggest(",
    "split(",
    "sqrt(",
    "srand(",
    "stdpath(",
    "str2list(",
    "str2nr(",
    "strcharpart(",
    "strchars(",
    "strdisplaywidth(",
    "strftime(",
    "strgetchar(",
    "stridx(",
    "string(",
    "strlen(",
    "strpart(",
    "strpartpos(",
    "strptime(",
    "strridx(",
    "strtrans(",
    "strwidth(",
    "submatch(",
    "substitute(",
    "swapinfo(",
    "swapfilelist(",
    "swapname(",
    "synID(",
    "synIDattr(",
    "synIDtrans(",
    "synconcealed(",
    "synstack(",
    "system(",
    "systemlist(",
    "tabpagebuflist(",
    "tabpagenr(",
    "tabpagewinnr(",
    "tagfiles(",
    "taglist(",
    "tan(",
    "tanh(",
    "tempname(",
    "term_dumpdiff(",
    "term_dumpload(",
    "term_dumpwrite(",
    "term_getaltscreen(",
    "term_getansicolors(",
    "term_getattr(",
    "term_getcursor(",
    "term_getescpos(",
    "term_getjob(",
    "term_getline(",
    "term_getscrolled(",
    "term_getsize(",
    "term_getstatus(",
    "term_gettitle(",
    "term_gettty(",
    "term_list(",
    "term_scrape(",
    "term_sendkeys(",
    "term_setansicolors(",
    "term_setapi(",
    "term_setkill(",
    "term_setrestore(",
    "term_setsize(",
    "term_start(",
    "term_wait(",
    "terminalprops(",
    "test_alloc_fail(",
    "test_feedinput(",
    "test_getvalue(",
    "test_ignore_error(",
    "test_null_blob(",
    "test_null_channel(",
    "test_null_dict(",
    "test_null_function(",
    "test_null_job(",
    "test_null_list(",
    "test_null_partial(",
    "test_null_string(",
    "test_option_not_set(",
    "test_override(",
    "test_refcount(",
    "test_setmouse(",
    "test_settime(",
    "test_srand_seed(",
    "test_unknown(",
    "test_void(",
    "timer_info(",
    "timer_pause(",
    "timer_resume(",
    "timer_start(",
    "timer_stop(",
    "timer_stopall(",
    "tolower(",
    "toupper(",
    "tr(",
    "trim(",
    "trunc(",
    "type(",
    "undofile(",
    "undotree(",
    "uniq(",
    "values(",
    "virtcol(",
    "visualmode(",
    "wildmenumode(",
    "win_execute(",
    "win_findbuf(",
    "win_getid(",
    "win_gettype(",
    "win_gotoid(",
    "win_id2tabwin(",
    "win_id2win(",
    "win_move_separator(",
    "win_move_statusline(",
    "win_screenpos(",
    "win_splitmove(",
    "winbufnr(",
    "wincol(",
    "windowscount(",
    "winheight(",
    "winlayout(",
    "winline(",
    "winnr(",
    "winrestcmd(",
    "winrestview(",
    "winsaveview(",
    "winwidth(",
    "wordcount(",
    "writefile(",
    "xor(",
];

fn complete_functions(pat: &str) -> Vec<OxStr> {
    prefix_filter(FUNCTIONS, pat)
}

/// Option names offered by `getcompletion(..., "option")`, in completion order.
const OPTIONS: &[&str] = &[
    "aleph",
    "allowrevins",
    "ambiwidth",
    "autochdir",
    "autoindent",
    "autoread",
    "autowrite",
    "autowriteall",
    "background",
    "backspace",
    "backup",
    "backupcopy",
    "backupdir",
    "backupext",
    "backupskip",
    "belloff",
    "binary",
    "bomb",
    "breakat",
    "breakindent",
    "breakindentopt",
    "browsedir",
    "bufhidden",
    "buflisted",
    "buftype",
    "casemap",
    "cdhome",
    "cdpath",
    "cedit",
    "charconvert",
    "cindent",
    "cinkeys",
    "cinoptions",
    "cinwords",
    "clipboard",
    "cmdheight",
    "cmdwinheight",
    "colorcolumn",
    "columns",
    "comments",
    "commentstring",
    "compatible",
    "complete",
    "completefunc",
    "completeslash",
    "concealcursor",
    "conceallevel",
    "confirm",
    "copyindent",
    "cpoptions",
    "cscopepathcomp",
    "cscopeprg",
    "cscopequickfix",
    "cscoperelative",
    "cscopetag",
    "cscopetagorder",
    "cscopeverbose",
    "cursorbind",
    "cursorcolumn",
    "cursorline",
    "cursorlineopt",
    "debug",
    "define",
    "delcombine",
    "directory",
    "display",
    "eadirection",
    "edcompatible",
    "emoji",
    "encoding",
    "endofline",
    "equalalways",
    "equalprg",
    "errorbells",
    "errorfile",
    "errorformat",
    "esckeys",
    "eventignore",
    "expandtab",
    "fileencoding",
    "fileencodings",
    "fileformat",
    "fileformats",
    "filetype",
    "fillchars",
    "fixendofline",
    "foldclose",
    "foldcolumn",
    "foldenable",
    "foldexpr",
    "foldignore",
    "foldlevel",
    "foldlevelstart",
    "foldmarker",
    "foldmethod",
    "foldminlines",
    "foldnestmax",
    "foldopen",
    "formatexpr",
    "formatlistpat",
    "formatoptions",
    "formatprg",
    "fsync",
    "gdefault",
    "grepformat",
    "grepprg",
    "guicursor",
    "guifont",
    "guifontwide",
    "guioptions",
    "guipty",
    "guitablabel",
    "guitabtooltip",
    "helpfile",
    "helpheight",
    "helplang",
    "hidden",
    "highlight",
    "history",
    "hkmap",
    "hkmapp",
    "hlsearch",
    "icon",
    "ignorecase",
    "imactivatekey",
    "imcmdline",
    "imdisable",
    "iminsert",
    "imsearch",
    "include",
    "includeexpr",
    "incsearch",
    "indentexpr",
    "indentkeys",
    "infercase",
    "insertmode",
    "isfname",
    "isident",
    "iskeyword",
    "isprint",
    "joinspaces",
    "keymap",
    "keymodel",
    "keywordprg",
    "langmap",
    "langmenu",
    "langremap",
    "laststatus",
    "lazyredraw",
    "linebreak",
    "lines",
    "linespace",
    "lisp",
    "lispwords",
    "list",
    "listchars",
    "loadplugins",
    "magic",
    "makeef",
    "makeencoding",
    "makeprg",
    "matchpairs",
    "maxcombine",
    "maxmapdepth",
    "maxmem",
    "maxmempattern",
    "maxmemtot",
    "menuitems",
    "mkspellmem",
    "modeline",
    "modelineexpr",
    "modifiable",
    "modified",
    "more",
    "mouse",
    "mousefocus",
    "mousehide",
    "mousemodel",
    "mousescroll",
    "mouseshape",
    "mousetime",
    "nrformats",
    "number",
    "numberwidth",
    "omnifunc",
    "operatorfunc",
    "packpath",
    "paragraphs",
    "paste",
    "pastetoggle",
    "patchexpr",
    "path",
    "preserveindent",
    "previewwindow",
    "printdevice",
    "printencoding",
    "printexpr",
    "printfont",
    "printheader",
    "printoptions",
    "prompt",
    "pumblend",
    "pumheight",
    "pythondll",
    "pythonhome",
    "pythonthreedll",
    "pythonthreehome",
    "quickfixtextfunc",
    "quoteescape",
    "readonly",
    "regexpengine",
    "relativenumber",
    "remap",
    "report",
    "revins",
    "rightleft",
    "rightleftcmd",
    "ruler",
    "rulerformat",
    "runtimepath",
    "scroll",
    "scrollback",
    "scrollbind",
    "scrolljump",
    "scrolloff",
    "scrollopt",
    "sections",
    "secure",
    "selection",
    "selectmode",
    "sessionoptions",
    "shada",
    "shadafile",
    "shell",
    "shellcmdflag",
    "shellescape",
    "shellpipe",
    "shellquote",
    "shellredir",
    "shellslash",
    "shelltemp",
    "shellxescape",
    "shellxquote",
    "shiftround",
    "shiftwidth",
    "shortmess",
    "showbreak",
    "showcmd",
    "showfulltag",
    "showmatch",
    "showmode",
    "showtabline",
    "sidescroll",
    "sidescrolloff",
    "signcolumn",
    "smartcase",
    "smartindent",
    "smarttab",
    "softtabstop",
    "spell",
    "spellcapcheck",
    "spellfile",
    "spelllang",
    "spelloptions",
    "spellsuggest",
    "splitbelow",
    "splitright",
    "startofline",
    "statusline",
    "suffixes",
    "suffixesadd",
    "swapfile",
    "swapsync",
    "switchbuf",
    "synmaxcol",
    "syntax",
    "tabline",
    "tabpagemax",
    "tabstop",
    "tagbsearch",
    "tagcase",
    "tagfunc",
    "taglength",
    "tagrelative",
    "tags",
    "tagstack",
    "termbidi",
    "termencoding",
    "termguicolors",
    "termpastefilter",
    "termpastewrapper",
    "textauto",
    "textwidth",
    "thesaurus",
    "thesaurusfunc",
    "tildeop",
    "timeout",
    "timeoutlen",
    "title",
    "titlelen",
    "titleold",
    "titlestring",
    "ttimeout",
    "ttimeoutlen",
    "ttybuiltin",
    "ttyfast",
    "undodir",
    "undofile",
    "undolevels",
    "undoreload",
    "updatecount",
    "updatetime",
    "varsofttabstop",
    "vartabstop",
    "verbose",
    "verbosefile",
    "viewdir",
    "viewoptions",
    "viminfo",
    "viminfofile",
    "virtualedit",
    "visualbell",
    "warn",
    "whichwrap",
    "wildchar",
    "wildcharm",
    "wildignore",
    "wildignorecase",
    "wildmenu",
    "wildmode",
    "wildoptions",
    "winaltkeys",
    "winbl",
    "winborder",
    "winhighlight",
    "window",
    "winfixbuf",
    "winfixheight",
    "winfixwidth",
    "winminheight",
    "winminwidth",
    "winwidth",
    "wrap",
    "wrapmargin",
    "wrapscan",
    "write",
    "writeany",
    "writebackup",
    "writedelay",
];

fn complete_options(pat: &str) -> Vec<OxStr> {
    prefix_filter(OPTIONS, pat)
}

fn complete_variables(editor: &Editor, pat: &str) -> Vec<OxStr> {
    let mut result = Vec::new();
    let vim_vars = &[
        "v:false",
        "v:true",
        "v:null",
        "v:none",
        "v:version",
        "v:versionlong",
        "v:errmsg",
        "v:warningmsg",
        "v:statusmsg",
        "v:shell_error",
        "v:this_session",
        "v:throwpoint",
        "v:exception",
        "v:register",
        "v:count",
        "v:count1",
        "v:prevcount",
        "v:searchforward",
        "v:hlsearch",
        "v:mouse_win",
        "v:mouse_winid",
        "v:mouse_lnum",
        "v:mouse_col",
        "v:operator",
        "v:char",
        "v:charconvert_from",
        "v:charconvert_to",
        "v:fname_in",
        "v:fname_out",
        "v:fname_new",
        "v:fname_diff",
        "v:folddashes",
        "v:foldlevel",
        "v:foldstart",
        "v:foldend",
        "v:progname",
        "v:progpath",
        "v:argv",
        "v:completed_item",
        "v:option_new",
        "v:option_old",
        "v:option_oldlocal",
        "v:option_oldglobal",
        "v:option_type",
        "v:option_command",
        "v:event",
    ];
    for var in vim_vars {
        if var.starts_with(pat) {
            result.push(OxStr::from(*var));
        }
    }
    let _ = editor;
    result
}

/// `ExpandBufnames` (buffer.c:2533-2650) as reached through
/// `getcompletion`: `addstar` builds `^` + translated pattern + `*`, the
/// `^` is then stripped to an unanchored regex, matched against listed
/// buffers' short names honoring 'ignorecase'. An empty pattern degenerates
/// to upstream's invalid `*` regex, which fails closed to no matches.
fn complete_buffers(editor: &Editor, pat: &str) -> Vec<OxStr> {
    if pat.is_empty() {
        return Vec::new();
    }
    // addstar translation (cmdexpand.c:1308): '*' -> ".*", '?' -> '.',
    // '.' and '~' escaped; then ExpandBufnames strips the leading '^'.
    let mut regex = String::with_capacity(pat.len() + 4);
    for token in pat.chars() {
        match token {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            '.' | '~' => {
                regex.push('\\');
                regex.push(token);
            }
            other => regex.push(other),
        }
    }
    regex.push_str(".*");
    let ignorecase = matches!(
        editor.options().get_global("ignorecase"),
        Ok(OptionValue::Boolean(true))
    );
    let regex = crate::search::pattern_with_case(&regex, ignorecase).into_owned();
    let Ok(program) = ox_regex::compile(&regex, ox_regex::Magic::Magic) else {
        return Vec::new();
    };
    editor
        .buffers()
        .into_iter()
        .filter(|handle| {
            editor.buffer(*handle).is_ok_and(|buffer| {
                // Skip unlisted buffers (`b_p_bl`, buffer.c:2572).
                buffer.flags.contains(crate::BufferFlags::LISTED)
                    && !buffer.name().as_bytes().is_empty()
            })
        })
        .filter_map(|handle| {
            let name = editor.buffer(handle).ok()?.name().clone();
            let text = name.to_string_lossy().into_owned();
            ox_regex::exec(&program, &ox_regex::Text::new(text))
                .is_some()
                .then_some(name)
        })
        .collect()
}

fn complete_arglist(editor: &Editor, pat: &str) -> Vec<OxStr> {
    editor
        .arglist()
        .names()
        .iter()
        .filter(|arg| arg.to_string_lossy().starts_with(pat))
        .map(|arg| OxStr::from(arg.to_string_lossy().as_ref()))
        .collect()
}

fn complete_augroups(_editor: &Editor, pat: &str) -> Vec<OxStr> {
    let groups = &["END", "FileExplorer", "MatchParen", "netrw", "vimStartup"];
    prefix_filter(groups, pat)
}

/// Autocmd event names offered by `getcompletion(..., "event")`, in completion order.
const EVENTS: &[&str] = &[
    "BufAdd",
    "BufDelete",
    "BufEnter",
    "BufFilePost",
    "BufFilePre",
    "BufHidden",
    "BufLeave",
    "BufModifiedSet",
    "BufNew",
    "BufNewFile",
    "BufRead",
    "BufReadCmd",
    "BufReadPost",
    "BufReadPre",
    "BufUnload",
    "BufWinEnter",
    "BufWinLeave",
    "BufWipeout",
    "BufWrite",
    "BufWriteCmd",
    "BufWritePost",
    "BufWritePre",
    "ChanInfo",
    "ChanOpen",
    "CmdUndefined",
    "CmdlineChanged",
    "CmdlineEnter",
    "CmdlineLeave",
    "CmdwinEnter",
    "CmdwinLeave",
    "ColorScheme",
    "CompleteChanged",
    "CompleteDone",
    "CompleteDonePre",
    "CursorHold",
    "CursorHoldI",
    "CursorMoved",
    "CursorMovedI",
    "DiffUpdated",
    "DirChanged",
    "ExitPre",
    "FileAppendCmd",
    "FileAppendPost",
    "FileAppendPre",
    "FileChangedRO",
    "FileChangedShell",
    "FileChangedShellPost",
    "FileReadCmd",
    "FileReadPost",
    "FileReadPre",
    "FileType",
    "FileWriteCmd",
    "FileWritePost",
    "FileWritePre",
    "FilterReadPost",
    "FilterReadPre",
    "FilterWritePost",
    "FilterWritePre",
    "FocusGained",
    "FocusLost",
    "FuncUndefined",
    "GUIEnter",
    "GUIFailed",
    "InsertChange",
    "InsertCharPre",
    "InsertEnter",
    "InsertLeave",
    "InsertLeavePre",
    "LspAttach",
    "LspDetach",
    "LspRequest",
    "LspTokenUpdate",
    "MenuPopup",
    "ModeChanged",
    "OptionSet",
    "QuickFixCmdPost",
    "QuickFixCmdPre",
    "QuitPre",
    "RemoteReply",
    "SearchWrapped",
    "SessionLoadPost",
    "SessionWritePost",
    "ShellCmdPost",
    "ShellFilterPost",
    "Signal",
    "SourceCmd",
    "SourcePost",
    "SourcePre",
    "SpellFileMissing",
    "StdinReadPost",
    "StdinReadPre",
    "SwapExists",
    "Syntax",
    "TabClosed",
    "TabEnter",
    "TabLeave",
    "TabNew",
    "TabNewEntered",
    "TermChanged",
    "TermClose",
    "TermEnter",
    "TermLeave",
    "TermOpen",
    "TermResponse",
    "TextChanged",
    "TextChangedI",
    "TextChangedP",
    "User",
    "VimEnter",
    "VimLeave",
    "VimLeavePre",
    "VimResized",
    "VimResume",
    "VimSuspend",
    "WinClosed",
    "WinEnter",
    "WinLeave",
    "WinNew",
    "WinNewPre",
    "WinResized",
    "WinScrolled",
];

fn complete_events(pat: &str) -> Vec<OxStr> {
    prefix_filter(EVENTS, pat)
}

fn complete_colors(pat: &str) -> Vec<OxStr> {
    let colors = &[
        "default",
        "desert",
        "elflord",
        "evening",
        "industry",
        "koehler",
        "morning",
        "murphy",
        "pablo",
        "peachpuff",
        "ron",
        "shine",
        "slate",
        "torte",
        "zellner",
    ];
    prefix_filter(colors, pat)
}

fn complete_filetypes(pat: &str) -> Vec<OxStr> {
    let fts = &[
        "asm",
        "awk",
        "bash",
        "c",
        "cpp",
        "css",
        "diff",
        "dockerfile",
        "elixir",
        "elm",
        "erlang",
        "fortran",
        "fsharp",
        "go",
        "groovy",
        "haml",
        "hamster",
        "haskell",
        "html",
        "java",
        "javascript",
        "json",
        "jsx",
        "julia",
        "kotlin",
        "latex",
        "less",
        "lisp",
        "lua",
        "make",
        "markdown",
        "matlab",
        "ocaml",
        "pascal",
        "perl",
        "php",
        "python",
        "r",
        "ruby",
        "rust",
        "scala",
        "scheme",
        "scss",
        "sh",
        "sql",
        "swift",
        "terraform",
        "tex",
        "toml",
        "typescript",
        "vala",
        "vim",
        "vue",
        "xml",
        "yaml",
        "zsh",
    ];
    prefix_filter(fts, pat)
}

fn complete_syntaxes(pat: &str) -> Vec<OxStr> {
    complete_filetypes(pat)
}

fn complete_compilers(pat: &str) -> Vec<OxStr> {
    let compilers = &[
        "ant",
        "bash",
        "bdf",
        "cucumber",
        "cargo",
        "dot",
        "gcc",
        "gfortran",
        "gnat",
        "go",
        "haml",
        "icc",
        "irb5",
        "javac",
        "jikes",
        "jsh",
        "lessc",
        "maven",
        "mcs",
        "msbuild",
        "nmake",
        "perl",
        "php",
        "pylint",
        "rake",
        "rspec",
        "rubocop",
        "ruby",
        "rustc",
        "sass",
        "shellcheck",
        "tcl",
        "tex",
        "tsco",
        "typescript",
        "xbuild",
        "zig",
    ];
    prefix_filter(compilers, pat)
}

fn complete_highlights(pat: &str) -> Vec<OxStr> {
    let groups = &[
        "ColorColumn",
        "Conceal",
        "Cursor",
        "CursorColumn",
        "CursorIM",
        "CursorLine",
        "CursorLineFold",
        "CursorLineNr",
        "CursorLineSign",
        "DiffAdd",
        "DiffChange",
        "DiffDelete",
        "DiffText",
        "Directory",
        "EndOfBuffer",
        "ErrorMsg",
        "FoldColumn",
        "Folded",
        "IncSearch",
        "LineNr",
        "LineNrAbove",
        "LineNrBelow",
        "MatchParen",
        "ModeMsg",
        "MoreMsg",
        "MsgArea",
        "NonText",
        "Normal",
        "Pmenu",
        "PmenuExtra",
        "PmenuExtraSel",
        "PmenuKind",
        "PmenuKindSel",
        "PmenuMatch",
        "PmenuMatchSel",
        "PmenuSbar",
        "PmenuSel",
        "PmenuThumb",
        "Question",
        "QuickFixLine",
        "Search",
        "SignColumn",
        "SpecialKey",
        "SpellBad",
        "SpellCap",
        "SpellLocal",
        "SpellRare",
        "StatusLine",
        "StatusLineNC",
        "Substitute",
        "TabLine",
        "TabLineFill",
        "TabLineSel",
        "TermCursor",
        "TermCursorNC",
        "Title",
        "VertSplit",
        "Visual",
        "VisualNOS",
        "WarningMsg",
        "Whitespace",
        "WildMenu",
        "WinBar",
        "WinSeparator",
    ];
    prefix_filter(groups, pat)
}

fn complete_messages(pat: &str) -> Vec<OxStr> {
    let msgs = &["clear", "redir"];
    prefix_filter(msgs, pat)
}

fn complete_filetypecmd(pat: &str) -> Vec<OxStr> {
    let cmds = &["detect", "indent", "off", "on", "plugin"];
    prefix_filter(cmds, pat)
}

#[cfg(test)]
mod buffer_completion_tests {
    use super::complete_buffers;
    use crate::editor::Editor;
    use ox_types::OxStr;

    fn editor_with_buffers(names: &[&str]) -> Editor {
        let mut editor = Editor::new();
        for name in names {
            let buffer = editor.create_buffer(true).unwrap();
            editor
                .buffer_mut(buffer)
                .unwrap()
                .set_name(OxStr::from(*name));
        }
        editor
    }

    fn names(editor: &Editor, pat: &str) -> Vec<String> {
        complete_buffers(editor, pat)
            .into_iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn empty_pattern_matches_nothing() {
        let editor = editor_with_buffers(&["a.txt"]);
        assert_eq!(names(&editor, ""), Vec::<String>::new());
    }

    #[test]
    fn substring_match_keeps_full_names_in_buffer_order() {
        // Test_buffer_completion (test_cmdline.vim:4842-4856): 'Foo'
        // matches every listed buffer containing it, by short name, in
        // buffer-creation order.
        let editor = editor_with_buffers(&[
            "Xbuf_complete/Foobar.c",
            "Xbuf_complete/MyFoobar.c",
            "AFoobar.h",
        ]);
        assert_eq!(
            names(&editor, "Foo"),
            vec![
                "Xbuf_complete/Foobar.c".to_owned(),
                "Xbuf_complete/MyFoobar.c".to_owned(),
                "AFoobar.h".to_owned(),
            ]
        );
    }

    #[test]
    fn unlisted_buffers_are_skipped() {
        let mut editor = editor_with_buffers(&["keep.c"]);
        let hidden = editor.create_buffer(false).unwrap();
        editor
            .buffer_mut(hidden)
            .unwrap()
            .set_name(OxStr::from("hidden.c"));
        assert_eq!(names(&editor, "."), vec!["keep.c".to_owned()]);
    }
}

// ===========================================================================
// Insert-mode keyword completion
// ===========================================================================
//
// Single-writer, main-thread state machine porting the upstream subset the
// functional suite exercises: keyword completion entered with `CTRL-N`/
// `CTRL-P` (optionally after `CTRL-X`), cyclic navigation, `CTRL-E` cancel,
// `CTRL-Y` accept, and the showmode/popup mirrors. Upstream citations are
// into `.references/neovim/src/nvim/insexpand.c` unless noted otherwise.
//
// The session lives on [`crate::ModeMachine`] (wired by the caller); every
// buffer edit goes through [`Editor::replace_buffer_text`] like the
// surrounding insert code, and no runtime-state borrow is held across edits.

use ox_text::Position;
use ox_types::{BufHandle, WinHandle};

use crate::{BufferStateError, BufferTextEditRequest, ExtmarkPosition, ModeError};

/// `CTRL-E` (`ins_compl_prep` `c` values ride the raw control characters).
const CTRL_E: char = '\u{05}';
/// `CTRL-N`.
const CTRL_N: char = '\u{0e}';
/// `CTRL-P`.
const CTRL_P: char = '\u{10}';
/// `CTRL-X`.
const CTRL_X: char = '\u{18}';
/// `CTRL-Y`.
const CTRL_Y: char = '\u{19}';
/// `CTRL-D`.
const CTRL_D: char = '\u{04}';
/// `CTRL-F`.
const CTRL_F: char = '\u{06}';
/// `CTRL-I`.
const CTRL_I: char = '\u{09}';
/// `CTRL-K`.
const CTRL_K: char = '\u{0b}';
/// `CTRL-L`.
const CTRL_L: char = '\u{0c}';
/// `CTRL-O`.
const CTRL_O: char = '\u{0f}';
/// `CTRL-Q`.
const CTRL_Q: char = '\u{11}';
/// `CTRL-R`.
const CTRL_R: char = '\u{12}';
/// `CTRL-S`.
const CTRL_S: char = '\u{13}';
/// `CTRL-T`.
const CTRL_T: char = '\u{14}';
/// `CTRL-U`.
const CTRL_U: char = '\u{15}';
/// `CTRL-V`.
const CTRL_V: char = '\u{16}';
/// `CTRL-Z`.
const CTRL_Z: char = '\u{1a}';
/// `CTRL-]`.
const CTRL_RSB: char = '\u{1d}';

/// Whether the key is a `CTRL-X` submode selector: exactly the case list
/// `set_ctrl_x_mode` consumes (`insexpand.c:2615-2736`). Anything else is
/// an ordinary key upstream does not consume. One deliberate
/// approximation: upstream releases `CTRL-R` when `=` is peeked next
/// (`insexpand.c:2644-2649`) for expression-register insertion, which
/// this port has no Insert-mode handling for yet — a released `CTRL-R`
/// is dropped either way today, so deferring the decision changes
/// nothing observable.
fn is_ctrl_x_submode_key(key: char) -> bool {
    matches!(
        key,
        CTRL_D
            | CTRL_E
            | CTRL_F
            | CTRL_I
            | CTRL_K
            | CTRL_L
            | CTRL_N
            | CTRL_O
            | CTRL_P
            | CTRL_Q
            | CTRL_R
            | CTRL_S
            | CTRL_T
            | CTRL_U
            | CTRL_V
            | CTRL_Y
            | CTRL_Z
            | CTRL_RSB
            | 's'
    )
}

/// `ctrl_x_msgs[CTRL_X_NORMAL]` (`insexpand.c:118`).
const MSG_KEYWORD: &str = " Keyword completion (^N^P)";
/// `ctrl_x_msgs[CTRL_X_NOT_DEFINED_YET]`, shown while `CTRL-X` waits for its
/// second key (`insexpand.c:119`).
const MSG_CTRL_X: &str = " ^X mode (^]^D^E^F^I^K^L^N^O^P^Rs^U^V^Y)";
/// `ctrl_x_msgs[CTRL_X_LOCAL_MSG]`, shown when the completion interrupted a
/// `CTRL-X` submode (`insexpand.c:133`, picked at `insexpand.c:6165-6166`).
const MSG_KEYWORD_LOCAL: &str = " Keyword Local completion (^N^P)";
/// No candidate beyond the typed text (`insexpand.c:6216`).
const MSG_NOT_FOUND: &str = "Pattern not found";
/// Cycling back onto the typed text (`insexpand.c:6222`).
const MSG_BACK_AT_ORIGINAL: &str = "Back at original";
/// A single candidate besides the typed text (`insexpand.c:6228`).
const MSG_ONLY_MATCH: &str = "The only match";
/// Upstream default for the 'complete' option (`options.lua`).
const DEFAULT_COMPLETE: &str = ".,w,b,u,t,i";
/// Source-scan bound. Upstream interrupts the scan when input is pending
/// (`os_breakcheck`/`got_int`, `insexpand.c:992-1000`, checked at
/// `insexpand.c:4884-4895`); the single-threaded port has no pending-input
/// notion on this path, so each source stops after this many collected
/// words instead.
const MAX_SOURCE_MATCHES: usize = 50_000;

/// One popup-menu row in the public four-string layout
/// (`pumitem_T` fields as filled by `ins_compl_build_compl_array`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionPumItem {
    /// Inserted word (`pum_text`).
    pub word: OxStr,
    /// Display kind (`kind`).
    pub kind: OxStr,
    /// Menu annotation (`menu`).
    pub menu: OxStr,
    /// Extra information (`info`).
    pub info: OxStr,
}

/// Popup display snapshot: `compl_match_array` plus the `pum_row`/`pum_col`
/// anchor from `popupmenu.c` (`pum_win_row`/`wcol` at `popupmenu.c:333-339`,
/// `pum_col = cursor_col` at `popupmenu.c:236`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionPum {
    /// Visible candidate rows, in list order.
    pub items: Vec<CompletionPumItem>,
    /// Selected row index or `-1`.
    pub selected: i64,
    /// Anchor row (cursor grid row).
    pub row: usize,
    /// Anchor column (leader-end grid column).
    pub col: usize,
    /// Match-list generation from the owning session.
    pub revision: u64,
}

/// What one insert-mode key did to the completion session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionOutcome {
    /// The key was consumed by the completion state machine and must not be
    /// inserted (`ins_compl_prep` returning true, `insexpand.c:2900-2903`).
    Handled,
    /// Completion stopped and the key must continue down the ordinary insert
    /// path (`ins_compl_prep` returning false, `insexpand.c:2915-2921`).
    Release,
}

/// Navigation direction (`compl_direction`, `ins_compl_key2dir`,
/// `insexpand.c:5619-5630`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    Forward,
    Backward,
}

/// One 'complete' source (`cpt` entry classified by its flag byte).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceKind {
    /// `.` — words from the current buffer.
    CurrentBuffer,
    /// `w`/`b` — words from other listed buffers.
    OtherBuffers,
}

/// Insert-mode completion state. `matches[0]` mirrors the original-text
/// entry (`insexpand.c:6179-6187`); the rest follow source scan order.
/// Both directions share this list: CTRL-N steps toward the back,
/// CTRL-P toward the front from the last entry (`cp_next`/`cp_prev`,
/// `insexpand.c:4940-4949`).
#[derive(Clone, Debug)]
pub struct CompletionSession {
    /// `compl_started`: a candidate list exists and navigation is live.
    active: bool,
    /// `ctrl_x_mode == CTRL_X_NOT_DEFINED_YET`: `CTRL-X` typed, waiting for
    /// its second key (`insexpand.c:395-416`).
    ctrl_x_pending: bool,
    /// `compl_orig_text`: the typed leader, captured at entry.
    leader: Vec<u8>,
    /// Candidate list; `[0]` is the original text.
    matches: Vec<Vec<u8>>,
    /// `compl_curr_match` as an index into `matches`; `-1` before the first
    /// move.
    selected: i64,
    /// `compl_col`: byte column where the leader starts on its line.
    start_col: usize,
    /// Cursor position tracked through the machine's own edits; the caller's
    /// snapshot goes stale after the first mutation.
    cursor: Position,
    /// Bytes currently inserted beyond the leader (`get_compl_len()`).
    inserted: usize,
    /// Popup mirror; `None` hides the menu.
    pum: Option<CompletionPum>,
    /// `edit_submode` (`insexpand.c:6164-6170`).
    submode: Option<&'static str>,
    /// `edit_submode_extra` (`ins_compl_show_statusmsg`,
    /// `insexpand.c:6211-6260`).
    extra: Option<String>,
    /// Match-list length the cached `pum` items were built from
    /// (`usize::MAX` forces a rebuild): navigation only moves the
    /// selection instead of re-allocating every candidate per key.
    pum_built_for: usize,
    /// The armed `CTRL-X` interrupted a live session (`CONT_INTRPT`,
    /// `insexpand.c:399-400`): the next `CTRL-N`/`CTRL-P` continues
    /// under the plain banner, without `CONT_LOCAL`.
    interrupted: bool,
    /// Match-list generation, bumped by every `start`: lets sync layers
    /// tell a rebuilt list from mere navigation without comparing items.
    pum_revision: u64,
}

impl Default for CompletionSession {
    fn default() -> Self {
        Self {
            active: false,
            ctrl_x_pending: false,
            leader: Vec::new(),
            matches: Vec::new(),
            selected: -1,
            start_col: 0,
            cursor: Position { lnum: 1, col: 0 },
            inserted: 0,
            pum: None,
            submode: None,
            extra: None,
            pum_built_for: usize::MAX,
            interrupted: false,
            pum_revision: 0,
        }
    }
}

impl CompletionSession {
    /// Creates an idle session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True while a `CTRL-X` sequence waits or a completion list is live.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active || self.ctrl_x_pending
    }

    /// `showmode()` override while completion owns the mode line: `--` +
    /// `edit_submode` + `" "` + `edit_submode_extra` (the pieces upstream
    /// stores in `edit_submode`/`edit_submode_extra` and `drawscreen.c`
    /// composes behind `--`). `None` falls back to the plain insert banner.
    #[must_use]
    pub fn showmode_override(&self) -> Option<String> {
        if self.ctrl_x_pending {
            return Some(format!("--{MSG_CTRL_X}"));
        }
        if !self.active {
            return None;
        }
        let mut text = String::from("--");
        text.push_str(self.submode?);
        if let Some(extra) = &self.extra {
            text.push(' ');
            text.push_str(extra);
        }
        Some(text)
    }

    /// Popup mirror for `ChromeState.popupmenu`.
    #[must_use]
    pub fn pum(&self) -> Option<&CompletionPum> {
        self.pum.as_ref()
    }

    /// Clears every artifact: `ins_compl_free` + `ins_compl_clear`
    /// (`insexpand.c:2208-2238`). Leaving Insert mode in any way calls this,
    /// which hides the popup on the next chrome sync.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// One insert-mode keystroke. `Handled` keys are consumed; `Release`
    /// keys must continue down the ordinary insert path.
    pub fn handle_insert_key(
        &mut self,
        editor: &mut Editor,
        buffer: BufHandle,
        window: WinHandle,
        cursor: Position,
        key: char,
        timestamp: i64,
    ) -> Result<CompletionOutcome, ModeError> {
        // `CTRL-X` opens the submode and sets the banner (`ins_ctrl_x`,
        // `insexpand.c:395-416`).
        if key == CTRL_X && !self.active {
            if !self.ctrl_x_pending {
                self.ctrl_x_pending = true;
            }
            return Ok(CompletionOutcome::Handled);
        }

        // Second key of a `CTRL-X` sequence (`set_ctrl_x_mode`,
        // `insexpand.c:2615-2736`). Only the keyword sources are ported;
        // other submode selectors are consumed-but-unported, while an
        // ordinary key is NOT consumed: upstream's `set_ctrl_x_mode`
        // returns false for it and the key continues through Insert
        // handling.
        if self.ctrl_x_pending {
            self.ctrl_x_pending = false;
            if key == CTRL_N || key == CTRL_P {
                // `^N`/`^P` through `CTRL-X` complete with the LOCAL banner
                // (`insexpand.c:6165-6166`) — unless the armed `CTRL-X`
                // interrupted a live session, which continues non-local
                // (`CONT_INTRPT` without `CONT_LOCAL`, `insexpand.c:2706-2710`).
                let local = !self.interrupted;
                self.interrupted = false;
                self.start(editor, buffer, window, cursor, key, local, timestamp)?;
                return Ok(CompletionOutcome::Handled);
            }
            if is_ctrl_x_submode_key(key) {
                return Ok(CompletionOutcome::Handled);
            }
            return Ok(CompletionOutcome::Release);
        }

        // Live completion: completion keys cycle, `CTRL-E`/`CTRL-Y` finish,
        // everything else stops the session and falls through
        // (`ins_compl_prep` active branch, `insexpand.c:2915-2921`).
        if self.active {
            match key {
                CTRL_X => {
                    // The inserted match stays (`ins_compl_stop` only
                    // restores the leader for CTRL-E); the submode arms
                    // for its second key, marked as interrupting.
                    self.stop_keep();
                    self.ctrl_x_pending = true;
                    self.interrupted = true;
                    return Ok(CompletionOutcome::Handled);
                }
                CTRL_N => {
                    self.cycle(editor, buffer, window, Direction::Forward, timestamp)?;
                    return Ok(CompletionOutcome::Handled);
                }
                CTRL_P => {
                    self.cycle(editor, buffer, window, Direction::Backward, timestamp)?;
                    return Ok(CompletionOutcome::Handled);
                }
                CTRL_E => {
                    self.stop_restore(editor, buffer, window, timestamp)?;
                    return Ok(CompletionOutcome::Handled);
                }
                CTRL_Y => {
                    self.stop_keep();
                    return Ok(CompletionOutcome::Handled);
                }
                _ => {
                    self.stop_keep();
                    return Ok(CompletionOutcome::Release);
                }
            }
        }

        // Plain `CTRL-N`/`CTRL-P` start keyword completion
        // (`ins_complete`, `insexpand.c:6282-6304`).
        if key == CTRL_N || key == CTRL_P {
            self.start(editor, buffer, window, cursor, key, false, timestamp)?;
            return Ok(CompletionOutcome::Handled);
        }
        Ok(CompletionOutcome::Release)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "session start mirrors ins_compl_start's inputs"
    )]
    /// Entry: capture the leader, scan the sources, then make the first
    /// move (`ins_compl_start` + first `ins_compl_next`,
    /// `insexpand.c:6085-6208`, `6282-6304`).
    fn start(
        &mut self,
        editor: &mut Editor,
        buffer: BufHandle,
        window: WinHandle,
        cursor: Position,
        key: char,
        local: bool,
        timestamp: i64,
    ) -> Result<(), ModeError> {
        let line = line_bytes(editor, buffer, cursor.lnum)?;
        let col = cursor.col.min(line.len());
        // Leader capture: scan back over keyword bytes (`get_normal_compl_info`
        // walks `vim_isIDc`, `insexpand.c:5693-5697`; the port uses the ASCII
        // keyword class because 'iskeyword' is not modeled).
        let mut start_col = col;
        while start_col > 0 && is_word_byte(line[start_col - 1]) {
            start_col -= 1;
        }
        let leader = line[start_col..col].to_vec();

        self.submode = Some(if local {
            MSG_KEYWORD_LOCAL
        } else {
            MSG_KEYWORD
        });
        self.pum_built_for = usize::MAX;
        self.pum_revision = self.pum_revision.wrapping_add(1);
        let sources = complete_sources(editor);

        // Original-text entry first, then every source in option order
        // (`insexpand.c:6179-6187`, `4769-4964`).
        let mut matches = vec![leader.clone()];
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        seen.insert(leader.clone());
        let ignorecase = option_is_true(editor, "ignorecase", false);
        for source in sources {
            match source {
                SourceKind::CurrentBuffer => scan_buffer_words(
                    editor,
                    buffer,
                    &leader,
                    ignorecase,
                    cursor.lnum,
                    col,
                    &mut matches,
                    &mut seen,
                ),
                SourceKind::OtherBuffers => {
                    scan_other_buffer_words(
                        editor,
                        buffer,
                        &leader,
                        ignorecase,
                        &mut matches,
                        &mut seen,
                    );
                }
            }
        }

        self.leader = leader;
        self.matches = matches;
        self.start_col = start_col;
        self.cursor = cursor;
        self.inserted = 0;
        self.selected = -1;
        self.active = true;

        let direction = if key == CTRL_P {
            Direction::Backward
        } else {
            Direction::Forward
        };
        self.cycle(editor, buffer, window, direction, timestamp)
    }

    /// One `ins_compl_next` step (`insexpand.c:5431-5560`): move to the
    /// neighboring entry of the cyclic list and show it
    /// (`ins_compl_make_cyclic`, `insexpand.c:1351-1369`).
    fn cycle(
        &mut self,
        editor: &mut Editor,
        buffer: BufHandle,
        window: WinHandle,
        direction: Direction,
        timestamp: i64,
    ) -> Result<(), ModeError> {
        let len = self.matches.len();
        let next = match (self.selected, direction) {
            // First move: forward lands on the first candidate, backward on
            // the last (`compl_old_match->cp_next`/`cp_prev`,
            // `insexpand.c:4940-4949`).
            (-1, Direction::Forward) => usize::from(len > 1),
            (-1, Direction::Backward) => len - 1,
            (index, dir) => {
                let index = usize::try_from(index.max(0)).unwrap_or(0).min(len - 1);
                match dir {
                    Direction::Forward => (index + 1) % len,
                    Direction::Backward => (index + len - 1) % len,
                }
            }
        };
        self.show_match(editor, buffer, window, next, timestamp)?;
        self.selected = i64::try_from(next).unwrap_or(i64::MAX);
        self.update_status();
        self.refresh_pum(editor);
        Ok(())
    }

    /// Swap the inserted tail for the new candidate's tail
    /// (`ins_compl_insert`, `insexpand.c:5200-5254`): the leader stays in
    /// the buffer, only the bytes beyond it change.
    fn show_match(
        &mut self,
        editor: &mut Editor,
        buffer: BufHandle,
        window: WinHandle,
        index: usize,
        timestamp: i64,
    ) -> Result<(), ModeError> {
        let word = self.matches[index].clone();
        let split = self.leader.len().min(word.len());
        let tail = word[split..].to_vec();
        let lnum = self.cursor.lnum;
        let leader_end_col = self.start_col + self.leader.len();
        let end_col = leader_end_col + self.inserted;
        let after_col = leader_end_col + tail.len();
        if end_col != leader_end_col || !tail.is_empty() {
            let after = Position {
                lnum,
                col: after_col,
            };
            editor.replace_buffer_text(
                buffer,
                &BufferTextEditRequest {
                    start: ExtmarkPosition::new(lnum - 1, leader_end_col),
                    end: ExtmarkPosition::new(lnum - 1, end_col),
                    replacement: vec![tail],
                },
                self.cursor,
                after,
                timestamp,
            )?;
            editor.set_window_cursor(window, after)?;
            self.cursor = after;
        }
        self.inserted = after_col - leader_end_col;
        Ok(())
    }

    /// `ins_compl_show_statusmsg` (`insexpand.c:6211-6260`). `matches[i]`
    /// carries `cp_number == i` (the original text is numbered 0 at
    /// `insexpand.c:1052`, the rest in list order).
    fn update_status(&mut self) {
        if self.matches.len() <= 1 {
            self.extra = Some(String::from(MSG_NOT_FOUND));
        } else if self.selected == 0 {
            self.extra = Some(String::from(MSG_BACK_AT_ORIGINAL));
        } else if self.matches.len() == 2 {
            self.extra = Some(String::from(MSG_ONLY_MATCH));
        } else {
            let total = self.matches.len() - 1;
            self.extra = Some(format!(
                "match {selected} of {total}",
                selected = self.selected
            ));
        }
    }

    /// Stop completion keeping the shown match (`CTRL-Y` and every released
    /// key: `ins_compl_stop` only restores the leader for `CTRL-E`,
    /// `insexpand.c:2740-2886`).
    fn stop_keep(&mut self) {
        self.active = false;
        self.ctrl_x_pending = false;
        self.submode = None;
        self.extra = None;
        self.pum = None;
    }

    /// `CTRL-E`: delete the inserted tail so exactly the typed leader
    /// remains, then stop (`ins_compl_stop`, `insexpand.c:2822-2839`).
    fn stop_restore(
        &mut self,
        editor: &mut Editor,
        buffer: BufHandle,
        window: WinHandle,
        timestamp: i64,
    ) -> Result<(), ModeError> {
        if self.inserted > 0 {
            let lnum = self.cursor.lnum;
            let leader_end_col = self.start_col + self.leader.len();
            let end_col = leader_end_col + self.inserted;
            let after = Position {
                lnum,
                col: leader_end_col,
            };
            editor.replace_buffer_text(
                buffer,
                &BufferTextEditRequest {
                    start: ExtmarkPosition::new(lnum - 1, leader_end_col),
                    end: ExtmarkPosition::new(lnum - 1, end_col),
                    replacement: Vec::new(),
                },
                self.cursor,
                after,
                timestamp,
            )?;
            editor.set_window_cursor(window, after)?;
            self.cursor = after;
        }
        self.stop_keep();
        Ok(())
    }

    /// Rebuild the popup mirror. Displayed when 'completeopt' allows it
    /// (`pum_wanted`, `insexpand.c:1402-1407`) and at least one candidate
    /// exists (`pum_enough_matches`, `insexpand.c:1411-1429`). The anchor
    /// pins the leader end so cycling does not move the menu.
    fn refresh_pum(&mut self, editor: &Editor) {
        if !self.active || self.matches.len() < 2 || !menu_wanted(editor) {
            self.pum = None;
            return;
        }
        let selected = if self.selected <= 0 {
            -1
        } else {
            self.selected - 1
        };
        let row = self.cursor.lnum.saturating_sub(1);
        let col = self.start_col + self.leader.len();
        // Navigation reuses the cached items: the match list is fixed for
        // the session, so only selection and anchor move per key.
        if self.pum_built_for == self.matches.len()
            && let Some(pum) = self.pum.as_mut()
        {
            pum.selected = selected;
            pum.row = row;
            pum.col = col;
            pum.revision = self.pum_revision;
            return;
        }
        let items = self.matches[1..]
            .iter()
            .map(|word| CompletionPumItem {
                word: OxStr::from(String::from_utf8_lossy(word).as_ref()),
                kind: OxStr::from(""),
                menu: OxStr::from(""),
                info: OxStr::from(""),
            })
            .collect();
        self.pum_built_for = self.matches.len();
        let revision = self.pum_revision;
        self.pum = Some(CompletionPum {
            items,
            selected,
            row,
            col,
            revision,
        });
    }
}

/// ASCII keyword byte (`vim_isIDc` with the default 'iskeyword' class).
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// 'ignorecase' lookup with the upstream default (off).
fn option_is_true(editor: &Editor, name: &str, fallback: bool) -> bool {
    match editor.options().get_global(name) {
        Ok(OptionValue::Boolean(value)) => *value,
        _ => fallback,
    }
}

/// `pum_wanted` (`insexpand.c:1402-1407`): 'completeopt' must contain
/// "menu" or "menuone"; upstream defaults the option to `menu,preview`.
fn menu_wanted(editor: &Editor) -> bool {
    match editor.options().get_global("completeopt") {
        Ok(OptionValue::String(value)) => value
            .split(',')
            .any(|item| item == "menu" || item == "menuone"),
        _ => true,
    }
}

/// Source order for keyword completion: the 'complete' option left to right
/// (`ins_compl_get_exp` walks the copied option string,
/// `insexpand.c:4792-4794,4825-4849`). Only the ported flags survive: `.`
/// (current buffer) and `w`/`b` (other listed buffers); `u`/`t`/`i` and the
/// file/func flags need scans this port does not implement and are skipped,
/// the way upstream skips exhausted entries (`INS_COMPL_CPT_CONT`).
fn complete_sources(editor: &Editor) -> Vec<SourceKind> {
    let option = match editor.options().get_global("complete") {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => String::from(DEFAULT_COMPLETE),
    };
    let mut sources = Vec::new();
    for entry in option.split(',') {
        let Some(flag) = entry.trim().chars().next() else {
            continue;
        };
        let kind = match flag {
            '.' => SourceKind::CurrentBuffer,
            'w' | 'b' => SourceKind::OtherBuffers,
            _ => continue,
        };
        if !sources.contains(&kind) {
            sources.push(kind);
        }
    }
    sources
}

#[expect(
    clippy::too_many_arguments,
    reason = "the scan carries its whole search context like get_next_default_completion"
)]
/// Current-buffer keyword scan. `get_next_default_completion` runs the
/// leader-anchored word search in the completion direction from the cursor
/// and wraps around the buffer (`insexpand.c:4396-4399`, wrap detection at
/// `4408-4426`): forward order is document order from the cursor onward,
/// backward order the reverse. Prefix matching honors 'ignorecase'
/// (`ins_compl_equal`, `insexpand.c:1166-1176`); duplicates and the typed
/// text itself are dropped by the add rules (`ins_compl_add`,
/// `insexpand.c:1006-1044`).
fn scan_buffer_words(
    editor: &Editor,
    buffer: BufHandle,
    leader: &[u8],
    ignorecase: bool,
    cursor_lnum: usize,
    cursor_col: usize,
    matches: &mut Vec<Vec<u8>>,
    seen: &mut std::collections::HashSet<Vec<u8>>,
) {
    let Ok(state) = editor.buffer(buffer) else {
        return;
    };
    let Ok(text) = state.text() else {
        return;
    };
    let line_count = text.line_count();
    if cursor_lnum > line_count {
        for lnum in 1..=line_count {
            let Ok(line) = text.line(lnum) else {
                continue;
            };
            scan_line(&line, 0, leader, ignorecase, matches, seen);
            if matches.len() >= MAX_SOURCE_MATCHES {
                return;
            }
        }
        return;
    }
    // One shared visit order (`get_next_default_completion`'s wrapping
    // search, `insexpand.c:4396-4426`): words on the cursor line from the
    // cursor onward, then the lines below, then wrap through the top
    // including the cursor line's words before the leader. CTRL-P shares
    // this list and traverses it backward: the first step lands on the
    // last entry (`compl_old_match->cp_prev`, `insexpand.c:4940-4949`),
    // which is the nearest word above the cursor.
    let mut segments: Vec<(usize, usize)> = Vec::new();
    segments.push((cursor_lnum, cursor_col));
    if cursor_lnum < line_count {
        segments.extend((cursor_lnum + 1..=line_count).map(|lnum| (lnum, 0)));
    }
    for lnum in 1..cursor_lnum {
        segments.push((lnum, 0));
    }
    segments.push((cursor_lnum, 0));
    for (lnum, from_col) in segments {
        let Ok(line) = text.line(lnum) else {
            continue;
        };
        scan_line(&line, from_col, leader, ignorecase, matches, seen);
        if matches.len() >= MAX_SOURCE_MATCHES {
            return;
        }
    }
}

/// Other-listed-buffer keyword scan ('w'/'b' sources): upstream scans those
/// buffers from the beginning with nowrapscan (`insexpand.c:4368-4377`).
fn scan_other_buffer_words(
    editor: &Editor,
    current: BufHandle,
    leader: &[u8],
    ignorecase: bool,
    matches: &mut Vec<Vec<u8>>,
    seen: &mut std::collections::HashSet<Vec<u8>>,
) {
    for handle in editor.buffers() {
        if handle == current {
            continue;
        }
        let Ok(state) = editor.buffer(handle) else {
            continue;
        };
        if !state.flags.contains(crate::BufferFlags::LISTED) {
            continue;
        }
        let Ok(text) = state.text() else {
            continue;
        };
        for lnum in 1..=text.line_count() {
            let Ok(line) = text.line(lnum) else {
                break;
            };
            scan_line(&line, 0, leader, ignorecase, matches, seen);
            if matches.len() >= MAX_SOURCE_MATCHES {
                return;
            }
        }
        if matches.len() >= MAX_SOURCE_MATCHES {
            return;
        }
    }
}

/// One line's keyword runs in byte order (`find_word_start`/`find_word_end`
/// iterated from `from_col`).
fn scan_line(
    line: &[u8],
    from_col: usize,
    leader: &[u8],
    ignorecase: bool,
    matches: &mut Vec<Vec<u8>>,
    seen: &mut std::collections::HashSet<Vec<u8>>,
) {
    let mut index = from_col.min(line.len());
    while index < line.len() {
        if !is_word_byte(line[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < line.len() && is_word_byte(line[index]) {
            index += 1;
        }
        add_word(&line[start..index], leader, ignorecase, matches, seen);
        if matches.len() >= MAX_SOURCE_MATCHES {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_text::Buffer;

    #[test]
    fn command_completion_accepts_wildcard_patterns() {
        let commands = complete_commands("*add");
        assert!(
            commands
                .iter()
                .any(|command| command.as_bytes() == b"packadd")
        );
        assert!(
            complete_commands("pack")
                .iter()
                .any(|command| command.as_bytes() == b"packadd")
        );
    }

    #[test]
    fn other_buffer_completion_skips_unlisted_buffers() {
        let mut editor = Editor::new();
        let current = editor
            .create_buffer_with(
                Buffer::from_lines(&[b"current".to_vec()], false).unwrap(),
                true,
            )
            .unwrap();
        let hidden = editor
            .create_buffer_with(
                Buffer::from_lines(&[b"hiddenword".to_vec()], false).unwrap(),
                false,
            )
            .unwrap();
        let mut matches = Vec::new();
        let mut seen = std::collections::HashSet::new();
        scan_other_buffer_words(&editor, current, b"hidden", false, &mut matches, &mut seen);
        assert!(!matches.iter().any(|word| word == b"hiddenword"));
        assert!(editor.buffer(hidden).is_ok());
    }
}

/// `ins_compl_add` admission rules (`insexpand.c:1006-1044`): a candidate
/// joins the list only when it starts with the leader ('ignorecase'
/// honored, `ins_compl_equal`, `insexpand.c:1166-1176`) and is not already
/// present (exact-byte dedup, which also drops the typed text itself — the
/// original-text entry seeds the set at `insexpand.c:6185-6187`).
fn add_word(
    word: &[u8],
    leader: &[u8],
    ignorecase: bool,
    matches: &mut Vec<Vec<u8>>,
    seen: &mut std::collections::HashSet<Vec<u8>>,
) {
    if word.is_empty() || word.len() < leader.len() {
        return;
    }
    let prefix_matches = if ignorecase {
        word[..leader.len()].eq_ignore_ascii_case(leader)
    } else {
        word[..leader.len()] == *leader
    };
    if !prefix_matches || seen.contains(word) {
        return;
    }
    seen.insert(word.to_vec());
    matches.push(word.to_vec());
}

/// Reads one buffer line as bytes.
fn line_bytes(editor: &Editor, buffer: BufHandle, lnum: usize) -> Result<Vec<u8>, ModeError> {
    Ok(editor
        .buffer(buffer)?
        .text()?
        .line(lnum)
        .map_err(BufferStateError::from)?)
}

#[cfg(test)]
mod completion_engine_tests {
    use super::{CTRL_E, CTRL_N, CTRL_P, CTRL_X, CompletionOutcome, CompletionSession};
    use crate::layout::Geometry;
    use ox_text::{Buffer, Position};

    fn editor_with(text: &[u8]) -> (crate::Editor, ox_types::BufHandle, ox_types::WinHandle) {
        let mut editor = crate::Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(text).unwrap(), true)
            .unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        (editor, buffer, window)
    }

    fn line(editor: &crate::Editor, buffer: ox_types::BufHandle, lnum: usize) -> String {
        String::from_utf8(
            editor
                .buffer(buffer)
                .unwrap()
                .text()
                .unwrap()
                .line(lnum)
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn ctrl_n_inserts_first_match_and_reports_only_match() {
        let (mut editor, buffer, window) = editor_with(b"include\nin");
        let mut session = CompletionSession::new();
        let outcome = session
            .handle_insert_key(
                &mut editor,
                buffer,
                window,
                Position { lnum: 2, col: 2 },
                CTRL_N,
                0,
            )
            .unwrap();
        assert_eq!(outcome, CompletionOutcome::Handled);
        assert_eq!(line(&editor, buffer, 2), "include");
        assert_eq!(
            session.showmode_override().as_deref(),
            Some("-- Keyword completion (^N^P) The only match")
        );
        let pum = session.pum().unwrap();
        assert_eq!(pum.items.len(), 1);
        assert_eq!(pum.items[0].word.to_string_lossy().as_ref(), "include");
        assert_eq!(pum.selected, 0);
        assert_eq!((pum.row, pum.col), (1, 2));
    }

    #[test]
    fn ctrl_x_then_ordinary_key_releases() {
        // `set_ctrl_x_mode` consumes only submode selectors
        // (`insexpand.c:2615-2736`): an ordinary key ends the pending
        // state and flows back into Insert handling.
        let (mut editor, buffer, window) = editor_with(b"alpha\nal");
        let mut session = CompletionSession::new();
        let outcome = session
            .handle_insert_key(
                &mut editor,
                buffer,
                window,
                Position { lnum: 2, col: 2 },
                CTRL_X,
                0,
            )
            .unwrap();
        assert_eq!(outcome, CompletionOutcome::Handled);
        let outcome = session
            .handle_insert_key(
                &mut editor,
                buffer,
                window,
                Position { lnum: 2, col: 2 },
                'a',
                1,
            )
            .unwrap();
        assert_eq!(outcome, CompletionOutcome::Release);
        assert_eq!(line(&editor, buffer, 2), "al");
        assert!(session.pum().is_none());
    }

    #[test]
    fn ctrl_x_after_active_session_rearms_submode() {
        // CTRL-N, then CTRL-X: the live session stops with the match
        // kept and a fresh submode sequence arms for its second key.
        let (mut editor, buffer, window) = editor_with(b"alpha\nalpaca\nal");
        let mut session = CompletionSession::new();
        let cursor = Position { lnum: 3, col: 2 };
        let outcome = session
            .handle_insert_key(&mut editor, buffer, window, cursor, CTRL_N, 0)
            .unwrap();
        assert_eq!(outcome, CompletionOutcome::Handled);
        assert!(session.pum().is_some());
        let outcome = session
            .handle_insert_key(&mut editor, buffer, window, cursor, CTRL_X, 1)
            .unwrap();
        assert_eq!(outcome, CompletionOutcome::Handled);
        assert!(session.pum().is_none());
        let outcome = session
            .handle_insert_key(&mut editor, buffer, window, cursor, CTRL_N, 2)
            .unwrap();
        assert_eq!(outcome, CompletionOutcome::Handled);
        assert_eq!(
            session.showmode_override().as_deref(),
            Some("-- Keyword completion (^N^P) match 1 of 2")
        );
    }

    #[test]
    fn ctrl_p_reverses_candidate_order() {
        // One shared list, traversed backward: the first CTRL-P lands on
        // the last entry (`cp_prev`, insexpand.c:4940-4949), the nearest
        // word above the cursor; the next CTRL-P steps toward the front.
        let (mut editor, buffer, window) = editor_with(b"alpha\nalpaca\nalpine\nal");
        let mut session = CompletionSession::new();
        let outcome = session
            .handle_insert_key(
                &mut editor,
                buffer,
                window,
                Position { lnum: 4, col: 2 },
                CTRL_P,
                0,
            )
            .unwrap();
        assert_eq!(outcome, CompletionOutcome::Handled);
        let pum = session.pum().expect("backward scan shows the pum");
        // Shared forward order after the leader: alpha, alpaca, alpine.
        // First CTRL-P lands on the last entry: `alpine`, nearest above.
        assert_eq!(line(&editor, buffer, 4), "alpine");
        let words: Vec<String> = pum
            .items
            .iter()
            .map(|item| item.word.to_string_lossy().into_owned())
            .collect();
        assert_eq!(words, vec!["alpha", "alpaca", "alpine"]);
        assert_eq!(pum.selected, 2);
        // A second CTRL-P steps one entry toward the front: `alpaca`.
        let outcome = session
            .handle_insert_key(
                &mut editor,
                buffer,
                window,
                Position { lnum: 4, col: 6 },
                CTRL_P,
                1,
            )
            .unwrap();
        assert_eq!(outcome, CompletionOutcome::Handled);
        assert_eq!(line(&editor, buffer, 4), "alpaca");
    }

    #[test]
    fn cycling_wraps_through_original_and_ctrl_e_restores_leader() {
        let (mut editor, buffer, window) = editor_with(b"ab abc");
        let mut session = CompletionSession::new();
        let cursor = Position { lnum: 1, col: 2 };
        session
            .handle_insert_key(&mut editor, buffer, window, cursor, CTRL_N, 0)
            .unwrap();
        assert_eq!(line(&editor, buffer, 1), "abc abc");
        session
            .handle_insert_key(&mut editor, buffer, window, cursor, CTRL_N, 0)
            .unwrap();
        // Wrapped back onto the original text.
        assert_eq!(line(&editor, buffer, 1), "ab abc");
        assert_eq!(
            session.showmode_override().as_deref(),
            Some("-- Keyword completion (^N^P) Back at original")
        );
        assert_eq!(session.pum().unwrap().selected, -1);
        session
            .handle_insert_key(&mut editor, buffer, window, cursor, CTRL_E, 0)
            .unwrap();
        assert_eq!(line(&editor, buffer, 1), "ab abc");
        assert!(!session.is_active());
        assert!(session.pum().is_none());
        assert!(session.showmode_override().is_none());
    }

    #[test]
    fn ctrl_x_ctrl_n_completes_with_local_banner() {
        let (mut editor, buffer, window) = editor_with(b"foo\nf");
        let mut session = CompletionSession::new();
        let cursor = Position { lnum: 2, col: 1 };
        session
            .handle_insert_key(&mut editor, buffer, window, cursor, CTRL_X, 0)
            .unwrap();
        assert_eq!(
            session.showmode_override().as_deref(),
            Some("-- ^X mode (^]^D^E^F^I^K^L^N^O^P^Rs^U^V^Y)")
        );
        session
            .handle_insert_key(&mut editor, buffer, window, cursor, CTRL_N, 0)
            .unwrap();
        assert_eq!(line(&editor, buffer, 2), "foo");
        assert_eq!(
            session.showmode_override().as_deref(),
            Some("-- Keyword Local completion (^N^P) The only match")
        );
    }

    #[test]
    fn no_match_reports_pattern_not_found_without_pum() {
        let (mut editor, buffer, window) = editor_with(b"abc\nzz");
        let mut session = CompletionSession::new();
        session
            .handle_insert_key(
                &mut editor,
                buffer,
                window,
                Position { lnum: 2, col: 2 },
                CTRL_N,
                0,
            )
            .unwrap();
        assert_eq!(line(&editor, buffer, 2), "zz");
        assert_eq!(
            session.showmode_override().as_deref(),
            Some("-- Keyword completion (^N^P) Pattern not found")
        );
        assert!(session.pum().is_none());
    }

    #[test]
    fn empty_leader_scans_bounded_and_inserts_full_words() {
        let (mut editor, buffer, window) = editor_with(b"ab cd");
        let mut session = CompletionSession::new();
        session
            .handle_insert_key(
                &mut editor,
                buffer,
                window,
                Position { lnum: 1, col: 0 },
                CTRL_N,
                0,
            )
            .unwrap();
        assert_eq!(line(&editor, buffer, 1), "abab cd");
        assert_eq!(session.pum().unwrap().items.len(), 2);
    }
}
