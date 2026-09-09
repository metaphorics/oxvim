//! The single-writer root for all editor state.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ox_text::{Buffer, Position, UndoTree};
use ox_types::{BufHandle, Dict, Object, OxStr, TabHandle, WinHandle};
use thiserror::Error;

use crate::builtins::position::cursor_vcol;
use crate::extmark::{ExtmarkPosition, NamespaceId, SignGroup, TextExtent, TextSplice};
use crate::arglist::ArgList;
use crate::autocmd::Autocmds;
use crate::buffer::{
    BufferState, BufferStateError, BufferSubscriptionRelease, BufferTextEditRequest,
};
use crate::decoration::Decorations;
use crate::layout::{
    CursorScreenPosition, Geometry, Layout, LayoutError, RelativeTo, TabpageState, WinConfig,
    WindowState,
};
use crate::fold::{FoldError, Position as FoldPosition};
use crate::mapping::Mappings;
use crate::marks::{Changelists, GlobalMarks, Jumplist, MarkError};
use crate::options::{OptionStore, OptionValue};
use crate::put::{PutDirection, PutEdit, PutPlan, plan_put, put_origin};
use crate::register::{RegisterError, RegisterKind, Registers};
use crate::script::{FileIO, RealFileIO};
use crate::typeahead::Typeahead;

pub(crate) const LOWEST_WINDOW_ID: i64 = 1_000;

/// Cloneable allocator for the process-wide dynamic channel key space.
#[derive(Clone, Debug)]
pub struct ChannelIds(Rc<Cell<u64>>);

impl Default for ChannelIds {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelIds {
    /// Start after the reserved stdio and stderr channel ids.
    #[must_use]
    pub fn new() -> Self {
        Self(Rc::new(Cell::new(3)))
    }

    /// Allocate one monotonically increasing dynamic channel id.
    ///
    /// # Panics
    ///
    /// Panics when the `u64` id space is exhausted. Ids are never reused or
    /// saturated: a wrapped id would collide with a live channel, and
    /// exhaustion of `u64::MAX - 3` allocations is impossible in practice.
    #[must_use]
    pub fn allocate(&self) -> u64 {
        let id = self.0.get();
        assert!(id < u64::MAX, "dynamic channel id space exhausted");
        self.0.set(id + 1);
        id
    }
}

/// Classification retained with a message submitted to the editor sink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageKind {
    /// Error output such as `nvim_err_writeln`.
    Error,
    /// General echo output.
    Echo,
}

/// Where the message sink sends one message's text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageDestination {
    /// Written to standard error (`message.c` `msg_puts_printf`, line 3049).
    Stderr,
    /// Written to standard output, upstream's `info_message` stream
    /// (`message.c` line 3047).
    Stdout,
    /// Handed to an attached UI (`message.c` `msg_puts_display`, line 2448).
    Ui,
    /// Dropped: batch mode with `'verbose'` zero (`message.c` line 3038).
    Suppressed,
}

/// Process-level state that decides where message output goes.
///
/// `message.c` `msg_use_printf` (line 3013) prints to stdout/stderr whenever
/// nothing else can display the text: no `--embed` peer, no attached UI and
/// no `ext_messages` UI. `main.c` starts a UI only when a terminal is
/// available and none of `--headless`, `--embed`, `-es`/`-Es` was requested
/// (line 332), so those modes reach the printf branch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MessageRouting {
    /// `--embed`: an RPC peer owns the message stream (`embedded_mode`).
    pub embedded: bool,
    /// `-es`, `-Es`, `-e -` batch mode (`silent_mode`); it both suppresses
    /// output while `'verbose'` is zero and keeps `main.c` from starting a UI
    /// (line 332, together with `--headless` and `--embed`).
    pub silent: bool,
    /// A UI has attached over RPC (`ui_active()`).
    pub ui_attached: bool,
}

/// Attributes retained for one `:highlight` group.
///
/// Values remain source spellings (`guifg=#rrggbb`, `bold`, `NONE`) so the
/// UI layer can apply terminal- or GUI-specific interpretation later.
pub type HighlightDefinition = BTreeMap<String, String>;
/// Attributes registered by the legacy `:sign define` command.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SignDefinition {
    pub(crate) text: Option<String>,
    pub(crate) text_highlight: Option<String>,
    pub(crate) number_highlight: Option<String>,
    pub(crate) line_highlight: Option<String>,
    pub(crate) cursorline_highlight: Option<String>,
}

/// A message retained until a UI or server consumes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// Message category.
    pub kind: MessageKind,
    /// API-compatible message payload.
    pub content: Object,
    /// Whether the message should enter message history.
    pub history: bool,
    /// Whether `execute()` prepends a newline before this payload.
    ///
    /// `:echo` and `:set` display start a new message line. Include/define
    /// search (`show_pat_in_path`) writes the first match on the current
    /// command line, so capture must not invent a leading newline.
    pub leading_newline: bool,
}

/// `nvim_echo` identity retained parallel to the message sink: the
/// `ui-messages` kind the call emits and the message id it returned
/// (`runtime/doc/api.txt:707-710`). Ids share one address space — an
/// autogenerated integer or a caller-supplied string.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageIdentity {
    /// Kind from `opts.kind`, else `echoerr`/`echomsg`/`echo` by `err` and
    /// `history` (`nvim_echo`, `api/vim.c:841-846`).
    pub kind: OxStr,
    /// Id returned by the producing call; `Object::Nil` when none.
    pub id: Object,
}

impl MessageIdentity {
    /// Identity for a message produced outside `nvim_echo`: the severity
    /// decides the kind and no id is attached.
    #[must_use]
    pub fn of(kind: MessageKind) -> Self {
        Self {
            kind: OxStr::from(if kind == MessageKind::Error { "emsg" } else { "echo" }),
            id: Object::Nil,
        }
    }
}

/// One validated `nvim__redraw` request (`keyset.redraw` in
/// `runtime/doc/api.txt`; `nvim__redraw`, `api/vim.c:2469`).
///
/// The binding owns upstream's validation and resolve rules, so every
/// field arrives executable: `win`/`buf` `0` already point at the current
/// window/buffer, and `flush` is the resolved value — the explicit one,
/// or the implicit true a present `valid` or `range` forces
/// (`vim.c:2544-2546`).
#[expect(
    clippy::struct_excessive_bools,
    reason = "the keyset.redraw action set is boolean flags by upstream definition"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RedrawRequest {
    /// Targeted window (`win`), or `None` when the request is unscoped.
    pub window: Option<WinHandle>,
    /// Targeted buffer (`buf`), or `None`. Upstream rejects a request
    /// naming both targets (`vim.c:2484-2487`).
    pub buffer: Option<BufHandle>,
    /// `valid` presence and value: `Some(false)` invalidates — upstream
    /// `UPD_NOT_VALID`, a forceful redraw — and `Some(true)` marks valid,
    /// changed lines only.
    pub valid: Option<bool>,
    /// `range` `[first, last]`: 0-based, end-exclusive, `-1` for the last
    /// line.
    pub range: Option<(i64, i64)>,
    /// Resolved `flush`: paint pending updates in this pass.
    pub flush: bool,
    /// `cursor`: update the cursor position on screen.
    pub cursor: bool,
    /// `tabline`: redraw the tabline.
    pub tabline: bool,
    /// `statusline`: redraw the status line.
    pub statusline: bool,
    /// `statuscolumn`: redraw the status column.
    pub statuscolumn: bool,
    /// `winbar`: redraw the winbar.
    pub winbar: bool,
}

/// Message-area content and command-line state of an input dialog.
///
/// The prefix includes its final newline; the suffix is the command-line
/// prompt (`ex_getln.c:4695-4717`). Renderers must not flatten their attributes.
#[derive(Clone, Debug)]
pub struct PromptDialog {
    /// Echoed message prefix, preceding the command-line prompt.
    pub message: OxStr,
    /// Last prompt line, preceding the editable reply.
    pub prompt: OxStr,
    /// Editable bytes, initially the default string.
    pub reply: OxStr,
    /// Byte offset of the insertion cursor within the reply.
    pub cursor: usize,
    /// Named highlight group for the message and prompt, not the reply.
    pub highlight: OxStr,
    /// Whether message scrolling needs a separator above this content.
    pub separator: bool,
    /// Requested command-line completion specification.
    pub completion: Option<OxStr>,
    /// Optional command-line highlight callback.
    pub highlight_callback: Option<ox_types::Typval>,
    /// Cancellation result, copied without coercion.
    pub cancelreturn: ox_types::Typval,
    /// Button accelerators and default result; absent for editable input.
    pub buttons: Option<(Vec<char>, i64)>,
    /// Numeric input rather than a string result.
    pub number: bool,
    /// Completed reply; absence means the host must keep waiting.
    pub result: Option<ox_types::Typval>,
}

/// Failures while mutating [`Editor`] state.
#[derive(Debug, Error)]
pub enum EditorError {
    /// A buffer handle is not live.
    #[error("Buffer {} does not exist", i64::from(*.0))]
    UnknownBuffer(BufHandle),
    /// Another live buffer already owns the requested name.
    #[error("buffer name already in use: {0}")]
    NameInUse(String),
    /// A window handle is not live.
    #[error("unknown window {0:?}")]
    UnknownWindow(WinHandle),
    /// A tabpage handle is not live.
    #[error("unknown tabpage {0:?}")]
    UnknownTabpage(TabHandle),
    /// An operation requiring a current tabpage was requested in an empty editor.
    #[error("no current tabpage")]
    NoCurrentTabpage,
    /// The only remaining tabpage cannot be closed.
    #[error("cannot close last tab page")]
    LastTabpage,
    /// A displayed buffer cannot be wiped.
    #[error("cannot wipe buffer {buffer:?} attached to {windows} window(s)")]
    BufferInUse {
        /// Buffer requested for wiping.
        buffer: BufHandle,
        /// Number of windows displaying it.
        windows: usize,
    },
    /// A 32-bit editor handle space was exhausted.
    #[error("{0} handle space exhausted")]
    HandleExhausted(&'static str),
    /// A buffer operation failed.
    #[error(transparent)]
    Buffer(#[from] BufferStateError),
    /// A frame-tree operation failed.
    #[error(transparent)]
    Layout(#[from] LayoutError),
    /// A register operation failed.
    #[error(transparent)]
    Register(#[from] RegisterError),
    /// A named-mark operation failed.
    #[error(transparent)]
    Mark(#[from] MarkError),
    /// A fold operation failed.
    #[error(transparent)]
    Fold(#[from] FoldError),
    /// An extmark store operation failed.
    #[error(transparent)]
    Extmark(#[from] crate::ExtmarkError),
}

/// What to do with an old buffer after its last window switches away.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferRelease {
    /// Retain resident text as a hidden buffer.
    KeepLoaded,
    /// Release resident text and undo history.
    Unload,
}

/// Editor input mode visible to the buffer API for cursor-adjustment policy.
///
/// The buffer API needs to know whether the current window is in INSERT mode
/// to decide whether `nvim_buf_set_text` moves the current window's cursor
/// when text is added at the cursor position (`mark_col_adjust` in
/// `mark.c` skips the current cursor when `restart_edit` is set). The host
/// sets this before dispatching API calls that mutate buffer text.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BufferEditMode {
    /// Normal command mode (the default).
    #[default]
    Normal,
    /// Insert or replace mode.
    Insert,
}

/// Scope of a directory change: session-global (`:cd`) or window-local
/// (`:lcd`), matching upstream `changedir_func`'s `local` flag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryScope {
    /// `:cd` — changes the process cwd and clears the current window's local
    /// directory so every window without its own `:lcd` follows it.
    Global,
    /// `:lcd` — changes the process cwd and records it on the current window
    /// only; other windows are unaffected.
    Window,
}

/// Failures from [`Editor::change_directory`]. The Ex layer maps these to
/// the existing Vim error codes (E16, E344).
#[derive(Debug)]
pub enum DirectoryError {
    /// A window-local directory change was requested with no current window
    /// (E16 "No current window").
    NoCurrentWindow,
    /// The OS refused the directory transition (E344 "Can't find directory").
    ChangeFailed {
        /// Directory that could not be entered.
        target: PathBuf,
        /// Underlying OS error.
        error: std::io::Error,
    },
}

/// Metadata for one editor-owned terminal channel.
///
/// A terminal channel is backed by a pseudoterminal and displayed through a
/// buffer; the pty slave name is reported by `nvim_get_chan_info` and used by
/// plugins such as termdebug to communicate with the child.
#[derive(Clone, Debug)]
pub struct TerminalChannelInfo {
    /// Pseudoterminal slave name, reported by `nvim_get_chan_info`.
    pub pty: Option<String>,
    /// Buffer displaying the terminal channel output.
    pub buffer: BufHandle,
    /// Parsed screen behind the buffer. This is the authoritative terminal
    /// state: buffer lines are a projection of it, never a second copy
    /// (upstream keeps the same split between libvterm's screen and
    /// `refresh_screen`, `terminal.c:2614-2652`).
    pub screen: crate::terminal_screen::TerminalScreen,
    /// Bytes owed to the child in answer to its queries, drained by the job
    /// layer (upstream `term_output_callback`, `terminal.c:461-464`).
    pub replies: Vec<u8>,
}

/// All mutable editor state under a single `&mut self` discipline.
///
/// No state is process-global. Event-loop and RPC layers can serialize their
/// requests into calls on this root without introducing locks into the model.
pub struct Editor {
    /// Live buffers in monotonically allocated handle order.
    buffers: BTreeMap<BufHandle, BufferState>,
    /// Subscriptions removed by wiping a buffer. The handle stays attached
    /// so lifecycle callbacks can run after the buffer itself is gone.
    pending_subscription_releases: Vec<BufferSubscriptionRelease>,
    /// Tabpage owning each live window handle.
    windows: BTreeMap<WinHandle, TabHandle>,
    /// Live tabpages and their tiled/floating layouts, keyed for lookup.
    tabpages: BTreeMap<TabHandle, TabpageState>,
    /// Tabpage order of record.
    ///
    /// Upstream keeps tabpages in an explicitly ordered linked list
    /// (`tp_next`), walks it in `tabpage_index`, and reorders it with
    /// `:tabmove`, so position is independent of when a tabpage was created.
    /// `tabpages` is keyed by handle and cannot express that, so this is the
    /// order every caller sees and `tabpages` is storage only.
    tab_order: Vec<TabHandle>,
    /// Global and scoped option values.
    options: OptionStore,
    /// Named, numbered, special, and provider-backed registers.
    registers: Registers,
    /// Global `A-Z` and numbered fileks.
    global_marks: GlobalMarks,
    /// Editor jump history.
    jumplist: Jumplist,
    /// Buffer-separated change histories.
    changelists: Changelists,
    /// Global argument list and its current entry (`arglist.c global_alist`).
    arglist: ArgList,
    /// Quickfix history and the current list (`quickfix.c` `ql_info`).
    quickfix: crate::quickfix::QuickfixStack,
    /// Per-window location list stacks (`quickfix.c` `w_llist`), freed with
    /// their window (`win_free` → `qf_free_all`, window.c:5667).
    loclists: std::collections::BTreeMap<WinHandle, crate::quickfix::QuickfixStack>,
    /// Diff-mode state: saved window options and per-tabpage diff blocks
    /// (`diff.c` `w_p_diff_saved` and `tp_first_diff`).
    pub diff: crate::diffmode::DiffState,
    /// Registered autocmds and augroups.
    autocmds: Autocmds,
    /// Registered decoration providers and active redraw-scoped output.
    decorations: Decorations,
    /// Mode-aware mappings and insert abbreviations.
    mappings: Mappings,
    /// Encoded pending input stack.
    typeahead: Typeahead,
    /// Editor-wide `g:` variables.
    gvars: Dict,
    /// Bumped by every `g:` variable writer; the differential Ex-variable
    /// sync compares this against the stamp a scope recorded to skip
    /// re-reading an unchanged map.
    gvars_version: u64,
    /// Editor-wide `v:` variables.
    vvars: Dict,
    /// Bumped by every `v:` variable writer; see `gvars_version`.
    vvars_version: u64,
    /// Named highlight groups defined by `:highlight`.
    highlights: BTreeMap<String, HighlightDefinition>,
    /// Named definitions registered by the legacy `:sign define` command.
    sign_definitions: BTreeMap<String, SignDefinition>,
    /// Namespace per named legacy sign group, editor-wide so one group name
    /// resolves to the same namespace in every buffer.
    sign_groups: BTreeMap<String, crate::extmark::SignGroup>,
    /// Messages waiting for a UI or server consumer.
    ///
    /// `message.c` `msg_puts_len` (line 2406) writes to the capture and
    /// redirection sinks before it decides where the text is displayed, so a
    /// message stays retained for `execute()`, `:redir` and `:silent` even
    /// when its destination is [`MessageDestination::Suppressed`].
    messages: Vec<Message>,
    /// Prompt state retained while the host collects interactive input.
    pub(crate) prompt_dialog: Option<PromptDialog>,
    /// Current `:echohl` group, snapshotted when a prompt starts.
    pub(crate) echo_highlight: OxStr,
    /// Sink decision recorded for each entry of `messages`, index for index.
    ///
    /// All three vectors are only ever pushed by [`Editor::push_message`] and
    /// [`Editor::push_info_message`] and truncated by
    /// [`Editor::truncate_messages`], so they stay the same length.
    message_destinations: Vec<MessageDestination>,
    /// `nvim_echo` identity (UI kind and message id) per entry of `messages`,
    /// index for index. Non-echo producers record their severity kind with no
    /// id; only `nvim_echo` attaches replace-by-id identity.
    message_identities: Vec<MessageIdentity>,
    /// Identity armed by the server before an `nvim_echo` handler pushes its
    /// message. The push consumes this marker before Progress callbacks run.
    pending_echo_identity: Option<MessageIdentity>,
    /// In-place echo replacements waiting for the server's render pass.
    echo_replacements: Vec<(Message, MessageIdentity)>,
    /// Raw `nvim_ui_send` payloads staged for the server's redraw pass.
    ///
    /// `nvim_ui_send` reaches every UI that negotiated `stdout_tty`
    /// (`api/ui.c:981-988`) from any dispatch path — RPC, nested
    /// `nvim_call_atomic`, or the Lua `vim.api` bindings — so the queue rides
    /// the session carrier those paths share, the way messages do.
    ui_sends: Vec<OxStr>,
    /// Validated `nvim__redraw` requests staged for the server's redraw
    /// pass.
    ///
    /// Like `ui_sends`, the queue rides the editor carrier so every entry
    /// path shares it and the server's redraw pass stays the single owner
    /// of delivery. The Lua binding validates and resolves each field
    /// upstream's keyset declares, so the drain sees only executable
    /// requests.
    redraws: Vec<RedrawRequest>,
    /// Process modes deciding where message output goes.
    pub message_routing: MessageRouting,
    current_tab: Option<TabHandle>,
    /// Last current window, used by `:wincmd p`.
    previous_window: Option<WinHandle>,
    /// Session-global previous directory for `cd -` (`prevdir` in
    /// `ex_docmd.c`). `None` until the first `:cd` records one.
    previous_directory: Option<PathBuf>,
    /// Session-global fallback directory (`globaldir`). Lazily initialized
    /// to the process cwd on first tabpage creation; `None` in a fresh
    /// editor with no tabpage so [`Editor::new`] stays cwd-inert.
    global_directory: Option<PathBuf>,

    next_buffer: i64,
    next_window: i64,
    next_tabpage: i64,
    /// Current edit mode for API-level cursor adjustment policy.
    edit_mode: BufferEditMode,
    /// Buffer whose current insert/replace session has recorded text.
    active_text_edit: Option<BufHandle>,
    channel_ids: ChannelIds,
    /// Channel id → editor-owned buffer used as a terminal surface.
    ///
    /// `jobstart(..., {'pty': v:true})` and `:terminal` allocate a terminal
    /// channel and bind it to a live buffer so `nvim_get_chan_info` can report
    /// the `buffer` field and so UI can show the child.
    terminal_buffers: BTreeMap<u64, TerminalChannelInfo>,
    /// `called_vim_beep` (`testing.c`): last `vim_beep` since the last take.
    beeped: bool,
    /// Cached `searchcount()` scan state (`search.c`'s search-stat statics,
    /// Editor-owned so concurrent Editors cannot observe each other's
    /// buffer, pattern, or position).
    search_count: Option<crate::search::SearchCountState>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

/// One domain request to replace an inclusive line range: named fields keep
/// buffer, range bounds, replacement, and both undo cursors from being
/// transposed at a call site.
#[derive(Clone, Copy)]
pub struct LineReplaceRequest<'lines> {
    /// Buffer whose lines are replaced.
    pub buffer: BufHandle,
    /// Inclusive one-based first replaced line.
    pub start: usize,
    /// Inclusive one-based last replaced line.
    pub end: usize,
    /// Replacement lines; an empty slice deletes the range.
    pub lines: &'lines [Vec<u8>],
    /// Cursor before the edit, recorded with the undo entry.
    pub cursor_before: Position,
    /// Cursor the edit leaves behind.
    pub cursor_after: Position,
    /// Undo timestamp shared with the surrounding edit batch.
    pub timestamp: i64,
}

impl Editor {
    /// Creates an empty editor. Buffer and tabpage handles start at one;
    /// window handles start at Neovim's reserved API window-ID floor.
    #[must_use]
    pub fn new() -> Self {
        let mut editor = Self {
            buffers: BTreeMap::new(),
            pending_subscription_releases: Vec::new(),
            windows: BTreeMap::new(),
            tabpages: BTreeMap::new(),
            tab_order: Vec::new(),
            options: OptionStore::new(),
            registers: Registers::new(),
            global_marks: GlobalMarks::new(),
            jumplist: Jumplist::new(),
            quickfix: crate::quickfix::QuickfixStack::new(),
            loclists: std::collections::BTreeMap::new(),
            diff: crate::diffmode::DiffState::default(),
            changelists: Changelists::new(),
            arglist: ArgList::new(),
            autocmds: Autocmds::new(),
            decorations: Decorations::new(),
            mappings: Mappings::new(),
            typeahead: Typeahead::new(),
            gvars: Dict(Vec::new()),
            vvars: Dict(vec![
                ("oldfiles".into(), Object::Array(Vec::new())),
                ("numbersize".into(), Object::Integer(64)),
                ("numbermax".into(), Object::Integer(i64::MAX)),
                ("numbermin".into(), Object::Integer(i64::MIN)),
                // Largest valid cursor column (`MAXCOL`, `pos_defs.h:17-19`),
                // also exposed to Lua as `vim.v.maxcol`.
                ("maxcol".into(), Object::Integer(0x7FFF_FFFF)),
                ("version".into(), Object::Integer(801)),
                ("versionlong".into(), Object::Integer(8_012_424)),
                ("errors".into(), Object::Array(Vec::new())),
                ("errmsg".into(), Object::String(OxStr::from(""))),
                ("exception".into(), Object::String(OxStr::from(""))),
                ("throwpoint".into(), Object::String(OxStr::from(""))),
                (
                    "progpath".into(),
                    Object::String(OxStr(std::env::current_exe().map_or_else(
                        |_| Vec::new(),
                        |path| path.to_string_lossy().into_owned().into_bytes(),
                    ))),
                ),
                ("_null_string".into(), Object::String(OxStr::from(""))),
                ("_null_list".into(), Object::Array(Vec::new())),
                ("_null_dict".into(), Object::Dict(Dict(Vec::new()))),
                ("register".into(), Object::String(OxStr::from("\""))),
            ]),
            gvars_version: 1,
            vvars_version: 1,
            highlights: BTreeMap::new(),
            sign_definitions: BTreeMap::new(),
            sign_groups: BTreeMap::new(),
            messages: Vec::new(),
            prompt_dialog: None,
            echo_highlight: OxStr::from(""),
            message_destinations: Vec::new(),
            message_identities: Vec::new(),
            pending_echo_identity: None,
            echo_replacements: Vec::new(),
            ui_sends: Vec::new(),
            message_routing: MessageRouting::default(),
            current_tab: None,
            redraws: Vec::new(),
            previous_window: None,
            previous_directory: None,
            global_directory: None,

            next_buffer: 1,
            next_window: LOWEST_WINDOW_ID,
            next_tabpage: 1,
            edit_mode: BufferEditMode::Normal,
            active_text_edit: None,
            channel_ids: ChannelIds::new(),
            terminal_buffers: BTreeMap::new(),
            beeped: false,
            search_count: None,
        };
        // init_highlight (highlight_group.c:755-800) runs from main(): every
        // editor instance carries the startup highlight groups so hlID()/
        // hlexists() see the same table a live session would.
        crate::highlight_init::init_highlight(&mut editor);
        editor
    }

    /// Allocates a unique channel ID for a new terminal channel.
    #[must_use]
    pub fn allocate_channel_id(&self) -> u64 {
        self.channel_ids.allocate()
    }
    /// Records a `vim_beep` (`misc1.c`) for `assert_beeps()`.
    pub fn beep(&mut self) {
        self.beeped = true;
    }

    /// Returns and clears whether a beep has been recorded since the last take.
    pub fn take_beeped(&mut self) -> bool {
        let beeped = self.beeped;
        self.beeped = false;
        beeped
    }

    /// Return the allocator shared by every dynamic channel owner.
    #[must_use]
    pub fn channel_ids(&self) -> ChannelIds {
        self.channel_ids.clone()
    }

    /// Allocate a single-row buffer and bind it to a terminal channel.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::HandleExhausted`] when the buffer handle space
    /// is exhausted.
    pub fn allocate_terminal_buffer(&mut self, channel: u64) -> Result<BufHandle, EditorError> {
        self.allocate_terminal_buffer_rows(
            channel,
            None,
            crate::terminal_screen::ScreenSize::new(1, 80),
        )
    }

    /// Attach a terminal to `attach`, or allocate a hidden buffer for a bare
    /// PTY. `jobstart({term=true})` attaches to curbuf; `:terminal` first runs
    /// `enew` (`terminal.c:585-611`, `eval/funcs.c:3529-3591`).
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::HandleExhausted`] when the buffer handle space
    /// is exhausted, or [`EditorError::Buffer`] when the text cannot load.
    pub fn allocate_terminal_buffer_rows(
        &mut self,
        channel: u64,
        attach: Option<BufHandle>,
        size: crate::terminal_screen::ScreenSize,
    ) -> Result<BufHandle, EditorError> {
        let size = crate::terminal_screen::ScreenSize::new(size.rows, size.cols);
        let lines = vec![Vec::new(); size.rows];
        let buffer = if let Some(buffer) = attach {
            let state = self.buffer_mut(buffer)?;
            let count = state.text()?.line_count();
            let cursor = Position { lnum: 1, col: 0 };
            state.replace_lines(1, count, &lines, cursor, cursor, 0)?;
            state.flags.set(crate::BufferFlags::MODIFIED, false);
            buffer
        } else {
            let text = Buffer::from_lines(&lines, false).map_err(BufferStateError::Text)?;
            self.create_buffer_with(text, false)?
        };
        self.terminal_buffers.insert(
            channel,
            TerminalChannelInfo {
                pty: None,
                buffer,
                screen: crate::terminal_screen::TerminalScreen::new(size),
                replies: Vec::new(),
            },
        );
        Ok(buffer)
    }

    /// Drop a completed terminal channel's emulator state so its buffer
    /// accepts a new terminal (`buf_close_terminal` after
    /// `terminal_running` answers false).
    pub fn close_terminal_channel(&mut self, channel: u64) {
        self.terminal_buffers.remove(&channel);
    }

    /// Record or update the pty slave path for an existing terminal channel.
    pub fn set_terminal_channel_pty(&mut self, channel: u64, pty: Option<String>) {
        if let Some(info) = self.terminal_buffers.get_mut(&channel) {
            info.pty = pty;
        }
    }

    /// Feed raw PTY output to the channel's persistent emulator and project
    /// its damaged rows into the buffer (`terminal.c:1382-1420,2614-2652`).
    /// Parsing and projection run without invoking callbacks or user code.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::Buffer`] when the channel's buffer text cannot
    /// be read or mutated, or [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved. Unknown channels are ignored, not errors.
    pub fn append_terminal_buffer(
        &mut self,
        channel: u64,
        bytes: &[u8],
    ) -> Result<(), EditorError> {
        let Some(buffer) = self.terminal_buffers.get(&channel).map(|info| info.buffer) else {
            return Ok(());
        };
        let limit = match self.options.get_buffer(buffer, "scrollback") {
            Ok(OptionValue::Number(value)) if *value >= 0 => usize::try_from(*value).ok(),
            _ => None,
        };
        let Some(info) = self.terminal_buffers.get_mut(&channel) else {
            return Ok(());
        };
        if let Some(limit) = limit {
            info.screen.set_scrollback_limit(limit);
        }
        info.screen.write(bytes);
        info.replies.extend(info.screen.take_replies());
        let cursor = info.screen.cursor();
        let cursor_line = info.screen.line_of_row(cursor.row);
        let cursor_col = info.screen.cursor_byte_column();
        let topline = info.screen.scrollback_len().saturating_add(1);
        let total = info.screen.line_count();
        if let Some(damage) = info.screen.take_damage() {
            let rebuild =
                damage.resync || damage.scrollback_deleted != 0 || damage.scrollback_pushed != 0;
            let first = if rebuild {
                1
            } else {
                info.screen.line_of_row(damage.rows.start)
            };
            let rows = if rebuild {
                (0..info.screen.scrollback_len())
                    .map(|row| info.screen.render_scrollback(row))
                    .chain((0..info.screen.rows()).map(|row| info.screen.render_row(row)))
                    .collect::<Vec<_>>()
            } else {
                damage
                    .rows
                    .map(|row| info.screen.render_row(row))
                    .collect::<Vec<_>>()
            };
            if !rows.is_empty() {
                let state = self
                    .buffers
                    .get_mut(&buffer)
                    .ok_or(EditorError::UnknownBuffer(buffer))?;
                project_terminal_rows(state, first, &rows)?;
            }
        }
        // Focused terminal input follows the emulator, while terminal-normal
        // mode retains the user's scrollback position (terminal.c:2658-2667).
        if self.edit_mode != BufferEditMode::Normal {
            for (&window, &tab) in &self.windows {
                if let Some(page) = self.tabpages.get_mut(&tab)
                    && let Ok(state) = page.window_mut(window)
                    && state.buffer == buffer
                {
                    state.cursor = Position {
                        lnum: cursor_line.min(total),
                        col: cursor_col,
                    };
                    state.topline = topline;
                }
            }
        }
        Ok(())
    }

    /// Parsed screen of a displayed terminal buffer.
    #[must_use]
    pub fn terminal_screen(
        &self,
        buffer: BufHandle,
    ) -> Option<&crate::terminal_screen::TerminalScreen> {
        self.terminal_buffers
            .values()
            .find(|info| info.buffer == buffer)
            .map(|info| &info.screen)
    }

    /// Take terminal-emulator replies after the editor borrow has ended.
    pub fn take_terminal_replies(&mut self, channel: u64) -> Vec<u8> {
        self.terminal_buffers
            .get_mut(&channel)
            .map_or_else(Vec::new, |info| std::mem::take(&mut info.replies))
    }

    /// Look up the editor-owned terminal channel metadata.
    #[must_use]
    pub fn terminal_channel(&self, channel: u64) -> Option<&TerminalChannelInfo> {
        self.terminal_buffers.get(&channel)
    }

    /// Returns all editor-owned terminal channel identifiers in ascending order.
    pub fn terminal_channel_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.terminal_buffers.keys().copied()
    }

    /// Whether `buffer` is owned by a terminal channel.
    #[must_use]
    pub fn is_terminal_buffer(&self, buffer: BufHandle) -> bool {
        self.terminal_buffers
            .values()
            .any(|info| info.buffer == buffer)
    }

    /// Returns the current tabpage, if one has been created.
    #[must_use]
    pub const fn current_tabpage(&self) -> Option<TabHandle> {
        self.current_tab
    }

    /// Returns the current window, if a tabpage has been created.
    #[must_use]
    pub fn current_window(&self) -> Option<WinHandle> {
        self.current_tab
            .and_then(|tab| self.tabpages.get(&tab))
            .map(TabpageState::current_window)
    }

    /// Returns the buffer displayed by the current window.
    #[must_use]
    pub fn current_buffer(&self) -> Option<BufHandle> {
        self.current_window()
            .and_then(|window| self.window(window).ok())
            .map(|window| window.buffer)
    }

    /// Whether the current window is pinned to its buffer by 'winfixbuf'
    /// (upstream `curwin->w_p_wfb`, window.c:200).
    #[must_use]
    pub fn current_window_fixed_to_buffer(&self) -> bool {
        self.current_window().is_some_and(|window| {
            self.options
                .get_window(window, "winfixbuf")
                .is_ok_and(|value| matches!(value, OptionValue::Boolean(true)))
        })
    }

    /// Returns live buffer handles in allocation order.
    #[must_use]
    pub fn buffers(&self) -> Vec<BufHandle> {
        self.buffers.keys().copied().collect()
    }
    /// Takes subscriptions removed by wiping a buffer, leaving the queue empty.
    ///
    /// Each record retains the wiped handle for `on_detach` delivery.
    pub fn take_pending_subscription_releases(&mut self) -> Vec<BufferSubscriptionRelease> {
        std::mem::take(&mut self.pending_subscription_releases)
    }

    /// Counts attachment releases waiting for the Lua host, including
    /// subscriptions from buffers already wiped from the editor.
    #[must_use]
    pub fn pending_subscription_releases_len(&self) -> usize {
        self.pending_subscription_releases.len()
    }

    /// Returns the highest buffer number ever allocated.
    #[must_use]
    pub fn last_buffer_nr(&self) -> i64 {
        self.next_buffer.saturating_sub(1)
    }

    /// Returns live window handles in allocation order.
    #[must_use]
    pub fn windows(&self) -> Vec<WinHandle> {
        self.windows.keys().copied().collect()
    }

    /// Returns live tabpage handles in tab order.
    #[must_use]
    pub fn tabpages(&self) -> Vec<TabHandle> {
        self.tab_order.clone()
    }

    /// Returns a live tabpage's one-based position, upstream's
    /// `tabpage_index` (`window.c`).
    #[must_use]
    pub fn tabpage_index(&self, tab: TabHandle) -> Option<usize> {
        self.tab_order
            .iter()
            .position(|entry| *entry == tab)
            .map(|index| index + 1)
    }

    /// Returns the tabpage that owns a live window.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, or [`EditorError::UnknownWindow`] when the window
    /// is not live.
    pub fn window_tabpage(&self, window: WinHandle) -> Result<TabHandle, EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        self.windows
            .get(&resolved)
            .copied()
            .ok_or(EditorError::UnknownWindow(resolved))
    }

    /// Returns an immutable live buffer state.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-buffer request
    /// has no live tabpage, or [`EditorError::UnknownBuffer`] when the buffer
    /// is not live.
    pub fn buffer(&self, buffer: BufHandle) -> Result<&BufferState, EditorError> {
        let resolved = if buffer.is_current() {
            self.current_buffer().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            buffer
        };
        self.buffers
            .get(&resolved)
            .ok_or(EditorError::UnknownBuffer(resolved))
    }

    /// Returns the `b:` variable-map version used by the differential sync.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-buffer request
    /// has no live tabpage, or [`EditorError::UnknownBuffer`] when the buffer
    /// is not live.
    pub fn buffer_variables_version(&self, buffer: BufHandle) -> Result<u64, EditorError> {
        Ok(self.buffer(buffer)?.variables_version())
    }

    /// Returns mutable state for a live buffer.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-buffer request
    /// has no live tabpage, or [`EditorError::UnknownBuffer`] when the buffer
    /// is not live.
    pub fn buffer_mut(&mut self, buffer: BufHandle) -> Result<&mut BufferState, EditorError> {
        let resolved = if buffer.is_current() {
            self.current_buffer().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            buffer
        };
        self.buffers
            .get_mut(&resolved)
            .ok_or(EditorError::UnknownBuffer(resolved))
    }

    /// Renames a live buffer and preserves its old name as the alternate buffer.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NameInUse`] when another live buffer owns `name`,
    /// or the documented buffer-allocation errors when the old name must remain
    /// addressable as an unlisted alternate buffer.
    pub fn rename_buffer(
        &mut self,
        buffer: BufHandle,
        name: OxStr,
    ) -> Result<Option<BufHandle>, EditorError> {
        let buffer = self.resolve_buffer_handle(buffer)?;
        let old_name = self.buffer(buffer)?.name().clone();
        if old_name == name {
            return Ok(None);
        }
        if !name.as_bytes().is_empty()
            && self.buffers.iter().any(|(&other, state)| {
                other != buffer && state.name().as_bytes() == name.as_bytes()
            })
        {
            return Err(EditorError::NameInUse(name.to_string_lossy().into_owned()));
        }

        // Vim keeps the old name addressable through the alternate buffer.
        let alternate = if old_name.as_bytes().is_empty() {
            None
        } else {
            let alternate = self.create_buffer(false)?;
            self.buffer_mut(alternate)?.set_name(old_name);
            Some(alternate)
        };
        let state = self.buffer_mut(buffer)?;
        state.set_name(name);
        state.flags.set(crate::BufferFlags::NOTEDITED, true);

        if let Some(window) = self.current_window()
            && self.window(window)?.buffer == buffer
        {
            self.window_mut(window)?.alternate_buffer = alternate;
        }
        Ok(alternate)
    }

    /// `do_autochdir` (buffer.c:1890): when 'autochdir' is set and the current
    /// buffer has a file name, change the window-local directory to the
    /// buffer's parent directory.  Matches upstream `vim_chdirfile` which
    /// strips the tail from `b_ffname` and `os_chdir`s to the result.
    pub fn do_autochdir(&mut self) {
        let autochdir = self
            .options
            .get_global("autochdir")
            .is_ok_and(|v| matches!(v, OptionValue::Boolean(true)));
        if !autochdir {
            return;
        }
        let Some(buffer) = self.current_buffer() else {
            return;
        };
        let name = match self.buffer(buffer) {
            Ok(state) => state.name().clone(),
            Err(_) => return,
        };
        if name.as_bytes().is_empty() {
            return;
        }
        // `vim_chdirfile`: strip the filename tail, keeping the directory.
        let name_lossy = name.to_string_lossy();
        let path = std::path::Path::new(name_lossy.as_ref());
        let Some(parent) = path.parent() else {
            return;
        };
        // Nothing to do if already there.
        if let Ok(cwd) = std::env::current_dir()
            && parent == cwd
        {
            return;
        }
        // Change to the buffer's directory (window-local scope, matching
        // upstream `kCdScopeWindow`).
        let _ = self.change_directory(parent, DirectoryScope::Window);
    }

    /// Returns an immutable live tabpage state.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-tabpage
    /// request has no live tabpage, or [`EditorError::UnknownTabpage`] when
    /// the tabpage is not live.
    pub fn tabpage(&self, tab: TabHandle) -> Result<&TabpageState, EditorError> {
        let resolved = if tab.is_current() {
            self.current_tab.ok_or(EditorError::NoCurrentTabpage)?
        } else {
            tab
        };
        self.tabpages
            .get(&resolved)
            .ok_or(EditorError::UnknownTabpage(resolved))
    }

    /// Returns immutable viewport state for a live window.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage layout rejects the
    /// window.
    pub fn window(&self, window: WinHandle) -> Result<&WindowState, EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self
            .windows
            .get(&resolved)
            .copied()
            .ok_or(EditorError::UnknownWindow(resolved))?;
        Ok(self.tabpage(tab)?.window(resolved)?)
    }

    /// Returns mutable viewport state for a live window.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage layout rejects the
    /// window.
    pub fn window_mut(&mut self, window: WinHandle) -> Result<&mut WindowState, EditorError> {
        let resolved = self.resolve_window_handle(window)?;
        let tab = self
            .windows
            .get(&resolved)
            .copied()
            .ok_or(EditorError::UnknownWindow(resolved))?;
        Ok(self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .window_mut(resolved)?)
    }

    /// Makes `window` the current window, recording the previous one for `CTRL-W p`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage does not contain
    /// the window.
    pub fn set_current_window(&mut self, window: WinHandle) -> Result<(), EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self
            .windows
            .get(&resolved)
            .copied()
            .ok_or(EditorError::UnknownWindow(resolved))?;
        if let Some(current) = self.current_window()
            && current != resolved
        {
            self.previous_window = Some(current);
        }
        self.tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .set_current(resolved)?;
        self.current_tab = Some(tab);
        self.apply_effective_directory();
        Ok(())
    }

    /// Window that `:wincmd p` would enter.
    #[must_use]
    pub const fn previous_window(&self) -> Option<WinHandle> {
        self.previous_window
    }

    /// Sets the previous-window handle, used by `nvim_buf_call` to restore
    /// the previous-window state after a context switch unwinds.
    pub fn set_previous_window(&mut self, window: Option<WinHandle>) {
        self.previous_window = window;
    }

    /// Returns the previous directory for `cd -` / `lcd -` resolution.
    ///
    /// [`DirectoryScope::Global`] reads the session-global previous directory;
    /// [`DirectoryScope::Window`] reads the current window's previous
    /// directory, or `None` when the current window has none or no current
    /// window exists.
    #[must_use]
    pub fn previous_directory(&self, scope: DirectoryScope) -> Option<PathBuf> {
        match scope {
            DirectoryScope::Global => self.previous_directory.clone(),
            DirectoryScope::Window => self
                .current_window()
                .and_then(|window| self.window(window).ok())
                .and_then(|state| state.previous_directory.clone()),
        }
    }

    /// Returns the window-local directory set by `:lcd`, or `None` when the
    /// window has none.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage does not contain
    /// the window.
    pub fn window_local_directory(
        &self,
        window: WinHandle,
    ) -> Result<Option<PathBuf>, EditorError> {
        Ok(self.window(window)?.local_directory.clone())
    }

    /// Returns the session-global fallback directory, or `None` before the
    /// first tabpage is created.
    #[must_use]
    pub fn global_directory(&self) -> Option<&Path> {
        self.global_directory.as_deref()
    }

    /// Changes the process working directory and records the directory left
    /// behind as the previous directory for the given scope.
    ///
    /// `target` is an already-resolved path (the Ex layer handles `-` and
    /// `cdpath`). The window is resolved *before* the OS mutation so a failure
    /// leaves no model state changed. On success the old `current_dir()` is
    /// stored as the previous directory and the new `current_dir()` (read back
    /// as an absolute path) is stored as the local or global directory.
    ///
    /// A [`DirectoryScope::Global`] change clears the current window's
    /// `local_directory` so every window without its own `:lcd` follows the
    /// global directory, matching upstream `:cd`.
    ///
    /// # Errors
    ///
    /// Returns [`DirectoryError::NoCurrentWindow`] when a window-scoped change
    /// has no current window, or [`DirectoryError::ChangeFailed`] when the OS
    /// refuses the transition.
    pub fn change_directory(
        &mut self,
        target: &Path,
        scope: DirectoryScope,
    ) -> Result<PathBuf, DirectoryError> {
        let window = match scope {
            DirectoryScope::Window => Some(
                self.current_window()
                    .ok_or(DirectoryError::NoCurrentWindow)?,
            ),
            DirectoryScope::Global => self.current_window(),
        };
        // Unreadable outgoing cwd records no previous path (empty PathBuf),
        // matching upstream `post_chdir` which leaves `prevdir` unset when
        // `os_dirname` fails.
        let mut previous: Option<PathBuf> = None;
        if let Ok(path) = std::env::current_dir() {
            previous = Some(path);
        }
        std::env::set_current_dir(target).map_err(|error| DirectoryError::ChangeFailed {
            target: target.to_path_buf(),
            error,
        })?;
        // Unreadable readback leaves the effective path unrecorded (None):
        // neither the local nor global state is set, and later reapplies
        // fall back to global_directory.
        let mut new_directory: Option<PathBuf> = None;
        if let Ok(path) = std::env::current_dir() {
            new_directory = Some(path);
        }
        match scope {
            DirectoryScope::Window => {
                if let Some(window) = window
                    && let Ok(state) = self.window_mut(window)
                {
                    state.previous_directory.clone_from(&previous);
                    state.local_directory = new_directory;
                }
            }
            DirectoryScope::Global => {
                self.previous_directory.clone_from(&previous);
                self.global_directory = new_directory;
                if let Some(window) = window
                    && let Ok(state) = self.window_mut(window)
                {
                    state.local_directory = None;
                }
            }
        }
        Ok(match previous {
            Some(path) => path,
            None => PathBuf::new(),
        })
    }

    /// Reapplies the effective working directory for the current window.
    ///
    /// If the current window has a `local_directory`, the process cwd is set
    /// to it; otherwise, if a `global_directory` is recorded, the process cwd
    /// is set to that. When neither is available the process cwd is left
    /// unchanged. This is called after buffer replacement, window switches,
    /// tab switches, and window closes to maintain the best-effort
    /// invariant that the process cwd reflects the current window's effective directory.
    ///
    /// Matches upstream `update_cwd` (`window.c:5365`): the window/tab/buffer
    /// transition has already committed, so a failure to `os_chdir` does not
    /// undo the transition. The state change remains valid and the process
    /// simply stays at its prior cwd — no error is queued or surfaced.
    fn apply_effective_directory(&self) {
        let target = self
            .current_window()
            .and_then(|window| self.window(window).ok())
            .and_then(|state| state.local_directory.as_ref())
            .or(self.global_directory.as_ref());
        if let Some(target) = target {
            // WHY: upstream `update_cwd` ignores `os_chdir` failure — the
            // transition is already committed and the old process cwd stays
            // in effect. No E344 is queued or surfaced.
            std::mem::drop(std::env::set_current_dir(target));
        }
    }

    /// Displays a live buffer in the current window.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when there is no current
    /// window or a current-buffer request has no live tabpage, otherwise the
    /// errors documented on [`Self::set_window_buffer`].
    pub fn set_current_buffer(
        &mut self,
        buffer: BufHandle,
        release: BufferRelease,
    ) -> Result<(), EditorError> {
        let buffer = self.resolve_buffer_handle(buffer)?;
        let window = self.current_window().ok_or(EditorError::NoCurrentTabpage)?;
        self.set_window_buffer(window, buffer, release)
    }

    /// Returns the current edit mode for API cursor-adjustment policy.
    #[must_use]
    pub const fn edit_mode(&self) -> BufferEditMode {
        self.edit_mode
    }

    /// Sets the edit mode the host reports before dispatching buffer-text
    /// mutations. The host sets [`BufferEditMode::Insert`] when the current
    /// window is in insert/replace mode so `nvim_buf_set_text` preserves the
    /// current cursor when text is added at the cursor position.
    pub fn set_edit_mode(&mut self, mode: BufferEditMode) {
        if self.edit_mode != mode {
            self.active_text_edit = None;
        }
        self.edit_mode = mode;
    }

    /// Whether this buffer has recorded text in the current insert/replace session.
    #[must_use]
    pub fn has_active_text_edit(&self, buffer: BufHandle) -> bool {
        self.edit_mode == BufferEditMode::Insert
            && self.current_buffer() == Some(buffer)
            && self.active_text_edit == Some(buffer)
    }

    /// Replaces one validated byte range and adjusts every position-bearing
    /// subsystem with column-aware cursor adjustment, matching
    /// `mark_col_adjust` (`mark.c`).
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, or [`EditorError::Buffer`] when the edit fails validation.
    pub fn replace_buffer_text(
        &mut self,
        buffer: BufHandle,
        request: &BufferTextEditRequest,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, EditorError> {
        let opens_active_edit =
            self.edit_mode == BufferEditMode::Insert && self.current_buffer() == Some(buffer);
        let prepared = self.buffer(buffer)?.prepare_buffer_text_edit(request)?;
        let splice = prepared.splice;
        let seq = self.buffer_mut(buffer)?.commit_buffer_text_edit(
            prepared,
            cursor_before,
            cursor_after,
            timestamp,
        )?;
        self.splice_text_positions(buffer, splice);
        self.changelists.push(buffer, cursor_after);
        if opens_active_edit {
            self.active_text_edit = Some(buffer);
        }
        Ok(seq)
    }

    /// Adjusts a buffer position for a byte-level text edit, mirroring
    /// `adjust_text_cursor` (`api/buffer.c:1304`). Used by the API layer to
    /// keep the mode machine's visual anchor in sync when `nvim_buf_set_text`
    /// modifies the buffer, the same way the editor cursor is adjusted.
    #[must_use]
    pub fn adjust_position_for_text_edit(
        &self,
        buffer: BufHandle,
        position: Position,
        start: ExtmarkPosition,
        end: ExtmarkPosition,
        replacement: &[Vec<u8>],
        block: bool,
    ) -> Position {
        let Some(state) = self.buffers.get(&buffer) else {
            return position;
        };
        let Ok(text) = state.text() else {
            return position;
        };
        let splice = TextSplice::from_byte_edit(start, end, replacement);
        let new_end_row_len = text
            .line(splice.new_end().row.saturating_add(1))
            .map_or(0, |line| line.len());
        let cursor = ExtmarkPosition::new(position.lnum.saturating_sub(1), position.col);
        let (adjusted, _) = adjust_text_cursor(cursor, 0, splice, new_end_row_len, block);
        Position {
            lnum: adjusted.row.saturating_add(1),
            col: adjusted.column,
        }
    }

    /// Adjusts a buffer position for a line-level edit (`set_lines`), mirroring
    /// `splice_position`. Used by the API layer to keep the mode machine's
    /// visual anchor in sync when `nvim_buf_set_lines` modifies the buffer.
    #[must_use]
    pub fn adjust_position_for_line_edit(
        &self,
        buffer: BufHandle,
        position: Position,
        start: usize,
        old_count: usize,
        new_count: usize,
    ) -> Position {
        let line_count = self
            .buffer(buffer)
            .ok()
            .and_then(|state| state.text().ok())
            .map_or(0, Buffer::line_count);
        let mut adjusted = position;
        splice_position(&mut adjusted, start, old_count, new_count, line_count);
        adjusted
    }
    /// Replaces the live prompt line without adding the replacement to normal
    /// undo history, while preserving text-edit position geometry
    /// (`f_prompt_setprompt`, `eval/buffer.c`).
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::UnknownBuffer`] when the buffer is not live, or
    /// [`EditorError::Buffer`] when the prompt line cannot be replaced.
    pub fn replace_prompt_line(
        &mut self,
        buffer: BufHandle,
        lnum: usize,
        line: Vec<u8>,
        old_len: usize,
        new_len: usize,
    ) -> Result<(), EditorError> {
        let splice = TextSplice {
            start: ExtmarkPosition::new(lnum.saturating_sub(1), 0),
            old_extent: TextExtent::new(0, old_len),
            new_extent: TextExtent::new(0, new_len),
        };
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        state.replace_prompt_line(lnum, line, splice)?;
        self.splice_text_positions(buffer, splice);
        Ok(())
    }

    /// Replaces several byte ranges as one planning-atomic batch
    /// (`op_reindent`, indent.c:947): every request is prepared against the
    /// pre-edit buffer and the cursor window is resolved before the first
    /// commit, so a validation failure leaves text, cursor, undo, and ticks
    /// untouched. Requests must be row-disjoint, same-row, and strictly
    /// ascending. Line-count-preserving batches commit as one text/derived
    /// tick; structural batches commit bottom-up with per-splice ticks.
    /// Commits join the open undo block as one transaction and the changelist
    /// gains one entry.
    pub(crate) fn replace_buffer_texts(
        &mut self,
        buffer: BufHandle,
        window: WinHandle,
        requests: &[BufferTextEditRequest],
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, EditorError> {
        debug_assert!(requests.iter().all(|r| r.start.row == r.end.row));
        debug_assert!(
            requests
                .windows(2)
                .all(|pair| pair[0].start.row < pair[1].start.row)
        );
        let opens_active_edit =
            self.edit_mode == BufferEditMode::Insert && self.current_buffer() == Some(buffer);
        let any = !requests.is_empty();
        // Validate: every fallible step runs before the first commit.
        let buffer = self.resolve_buffer_handle(buffer)?;
        let window = self.resolve_window_handle(window)?;
        self.window(window)?;
        let mut prepared = Vec::with_capacity(requests.len());
        for request in requests {
            prepared.push(self.buffer(buffer)?.prepare_buffer_text_edit(request)?);
        }
        // Commit: infallible by construction (buffer.rs prepare/commit split).
        let line_preserving = prepared
            .iter()
            .all(super::buffer::PreparedBufferTextEdit::preserves_line_count);
        let splices: Vec<TextSplice> = if line_preserving {
            prepared.iter().map(|edit| edit.splice).collect()
        } else {
            prepared.iter().rev().map(|edit| edit.splice).collect()
        };
        let mut seq = 0;
        {
            let state = self
                .buffers
                .get_mut(&buffer)
                .ok_or(EditorError::UnknownBuffer(buffer))?;
            if line_preserving {
                seq = state.commit_prepared_line_preserving_batch(
                    prepared,
                    cursor_before,
                    cursor_after,
                    timestamp,
                )?;
            } else {
                for edit in prepared.into_iter().rev() {
                    seq = state.commit_buffer_text_edit(
                        edit,
                        cursor_before,
                        cursor_after,
                        timestamp,
                    )?;
                }
            }
        }
        // Splices are applied in commit order. Line-preserving disjoint
        // splices commute; row-count-changing ones must be applied bottom-up
        // so each pre-edit-coordinate transform only row-shifts positions
        // below its already-processed span.
        for splice in splices {
            self.splice_text_positions(buffer, splice);
        }
        if any {
            self.changelists.push(buffer, cursor_after);
            if opens_active_edit {
                self.active_text_edit = Some(buffer);
            }
        }
        // Cursor last; the window was resolved above and evaluation is
        // read-only, so nothing between validation and here can remove it.
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        tabpage.window_mut(window)?.cursor = cursor_after;
        Ok(seq)
    }

    /// Changes a live window's cursor position.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage layout rejects the
    /// window.
    pub fn set_window_cursor(
        &mut self,
        window: WinHandle,
        position: Position,
    ) -> Result<(), EditorError> {
        let window = self.resolve_window_handle(window)?;
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        let height = viewport_height(tabpage, window);
        let state = tabpage.window_mut(window)?;
        state.cursor = position;
        state.topline = cursor_visible_topline(state.topline, position.lnum, height);
        // `check_cursor_moved` / most motions set `w_set_curswant`.
        state.set_curswant = true;
        Ok(())
    }

    /// Changes the first displayed line of a live window.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage layout rejects the
    /// window.
    pub fn set_window_topline(
        &mut self,
        window: WinHandle,
        topline: usize,
    ) -> Result<(), EditorError> {
        let window = self.resolve_window_handle(window)?;
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        tabpage.window_mut(window)?.topline = topline.max(1);
        Ok(())
    }

    /// Switches the buffer displayed by a window and updates both attachment counts.
    ///
    /// Entering an unloaded buffer reloads it (`buf_ensure_loaded` on every
    /// `win_enter` path) through [`Self::unloaded_buffer_text`]. The
    /// `BufRead*` event family cannot fire here — events run through a host
    /// executor — so the read-event pair belongs to callback-capable switch
    /// paths (`switch_current_buffer` and the API window loader), not this
    /// low-level state transition.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current window or
    /// buffer request has no live tabpage, [`EditorError::UnknownWindow`] or
    /// [`EditorError::UnknownBuffer`] when a handle is not live,
    /// [`EditorError::Buffer`] when attaching the new buffer fails or its
    /// file cannot be read ([`BufferStateError::Unloaded`]), or
    /// [`EditorError::Layout`] when the tabpage rejects the window switch. On
    /// failure the previous window/buffer pairing is unchanged.
    pub fn set_window_buffer(
        &mut self,
        window: WinHandle,
        buffer: BufHandle,
        release: BufferRelease,
    ) -> Result<(), EditorError> {
        let window = self.resolve_window_handle(window)?;
        let buffer = self.resolve_buffer_handle(buffer)?;
        self.require_buffer(buffer)?;
        let old_buffer = self.window(window)?.buffer;
        if old_buffer == buffer {
            return Ok(());
        }
        // `win_enter_ext` syncs undo before leaving the current buffer so the
        // block cannot be joined by a later edit made after coming back
        // (`window.c:5275-5279`, `buffer.c:1743-1750`).
        self.sync_buffer_undo(old_buffer);
        // The read finishes before any mutable borrow of the buffer so no
        // resident-state borrow is held across file IO.
        let text = self.unloaded_buffer_text(buffer)?;
        if let Some(state) = self.buffers.get_mut(&buffer) {
            if let Some(text) = text {
                state.load(text);
                state.mark_saved();
                state.flags.set(crate::BufferFlags::NOTEDITED, false);
            }
            state.attach()?;
        }
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        if let Err(error) = tabpage.window_mut(window).map(|state| {
            state.alternate_buffer = Some(old_buffer);
            // Upstream's `w_llist_ref` is meaningful only while the window
            // displays a quickfix buffer (`IS_LL_WINDOW`, quickfix.c:274-276).
            // Replacing that buffer ends the display, so the reference must
            // not follow the window into its next buffer.
            state.loclist_ref = None;
            state.buffer = buffer;
        }) {
            if let Some(state) = self.buffers.get_mut(&buffer) {
                state.detach(true);
            }
            return Err(error.into());
        }
        if let Some(state) = self.buffers.get_mut(&old_buffer) {
            state.detach(release == BufferRelease::KeepLoaded);
        }
        // Reapply the current window's effective cwd after the buffer swap
        // has committed, matching upstream `update_cwd(kCdCauseBuffer)`.
        if self.current_window() == Some(window) {
            self.apply_effective_directory();
        }
        Ok(())
    }

    /// Resolves the text an unloaded buffer enters with, or `None` when the
    /// buffer is already resident.
    ///
    /// A named ordinary buffer re-reads its file through the production
    /// [`FileIO`] seam ([`RealFileIO`]), the way `open_buffer` does; an
    /// unnamed buffer or a `buftype` upstream never reads (`bt_nofileread`,
    /// `buffer.c:4071-4077`) materializes empty text. A genuinely missing
    /// file is the shared new-file semantic (`buffer_from_file`): the buffer
    /// opens empty and unmodified.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::Buffer`]`(`[`BufferStateError::Unloaded`]`)`
    /// when the file exists but cannot be read — attaching fabricated
    /// saved-empty text instead is what let a later `:write` destroy the
    /// on-disk file.
    fn unloaded_buffer_text(&self, buffer: BufHandle) -> Result<Option<Buffer>, EditorError> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Ok(None);
        };
        if state.residency.is_loaded() {
            return Ok(None);
        }
        let nofileread = matches!(
            self.options.get_buffer(buffer, "buftype"),
            Ok(OptionValue::String(buftype)) if is_nofileread(buftype)
        );
        let name = state.name();
        if name.as_bytes().is_empty() || nofileread {
            return Ok(Some(Buffer::new()));
        }
        let path = PathBuf::from(name.to_string_lossy().as_ref());
        match RealFileIO.read_to_string(&path) {
            Ok(content) => Buffer::from_bytes(content.as_bytes())
                .map(Some)
                .map_err(|error| EditorError::Buffer(error.into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some(Buffer::new())),
            Err(_) => Err(EditorError::Buffer(BufferStateError::Unloaded)),
        }
    }

    /// Runs `f` in the display context of `buffer` for autocmd execution
    /// (upstream `ctx_switch`/`ctx_restore`, `context.c:527-713`).
    ///
    /// The first existing window already showing `buffer` and not ignoring
    /// `event` through its window-local `eventignorewin` becomes current. If
    /// every window showing the target ignores the event, the callback is
    /// skipped. A hidden buffer is temporarily displayed in the caller window
    /// instead of a synthetic handle. Global `eventignore` is checked before
    /// any window changes.
    ///
    /// The outer error is the context switch itself failing; otherwise `f`
    /// runs at most once and its result is reported only after the previous
    /// current window and every window buffer this call changed are put back
    /// when still valid, so handler buffer switches, window changes, errors,
    /// nesting, and target wipes all unwind without masking `f`'s result.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when the target buffer or the
    /// caller window cannot be resolved, or the errors of the window/buffer
    /// switches made to enter the context. `f`'s own result is never
    /// converted; it is returned inside the outer `Ok`.
    pub fn in_buffer_context<T, E>(
        &mut self,
        event: crate::autocmd::Event,
        buffer: BufHandle,
        f: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<Option<Result<T, E>>, EditorError> {
        if self.autocmds().is_ignored(event) {
            return Ok(None);
        }
        let target = self.resolve_buffer_handle(buffer)?;
        let Some(caller) = self.current_window() else {
            return Err(EditorError::NoCurrentTabpage);
        };
        let caller_buffer = self.window(caller).ok().map(|state| state.buffer);
        let visible: Vec<_> = self
            .windows()
            .into_iter()
            .filter(|&window| {
                self.window(window)
                    .is_ok_and(|state| state.buffer == target)
            })
            .collect();
        if !visible.is_empty()
            && visible
                .iter()
                .all(|&window| self.window_ignores_event(window, event))
        {
            return Ok(None);
        }
        let selected = visible
            .iter()
            .copied()
            .find(|&window| !self.window_ignores_event(window, event));
        if caller_buffer == Some(target) && selected == Some(caller) {
            return Ok(Some(f(self)));
        }
        // Prefer a window already showing the target; a hidden target
        // temporarily takes over the caller window.
        let changed = match selected {
            Some(window) if window == caller => None,
            Some(window) => {
                self.set_current_window(window)?;
                Some((window, target))
            }
            None => {
                let original = caller_buffer.unwrap_or(target);
                self.set_current_buffer(target, BufferRelease::KeepLoaded)?;
                Some((caller, original))
            }
        };
        let result = f(self);
        if let Some((window, original)) = changed
            && self
                .window(window)
                .is_ok_and(|state| state.buffer != original)
            && self.buffer(original).is_ok()
        {
            let _ = self.set_window_buffer(window, original, BufferRelease::KeepLoaded);
        }
        if self.current_window() != Some(caller) && self.window(caller).is_ok() {
            let _ = self.set_current_window(caller);
        }
        Ok(Some(result))
    }

    /// Runs `f` in the display context of `buffer` for Lua `nvim_buf_call`.
    ///
    /// Unlike [`Self::in_buffer_context`] this never consults `'eventignore'`
    /// and always runs the callback. A window already showing the buffer is
    /// entered, preferring the caller window; a hidden buffer temporarily
    /// takes over the caller window. Every window, buffer, and previous-window
    /// change this call made is undone on the way out without masking `f`'s
    /// own result, so nested calls keep their state on the Rust stack.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when the caller window cannot
    /// be resolved. Context-switch failures during entry or restore are
    /// swallowed rather than masking `f`'s result.
    pub fn with_buffer_context<T, E>(
        &mut self,
        buffer: BufHandle,
        f: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<Result<T, E>, EditorError> {
        let target = self.resolve_buffer_handle(buffer)?;
        let Some(caller) = self.current_window() else {
            return Err(EditorError::NoCurrentTabpage);
        };
        let caller_buffer = self.window(caller).ok().map(|state| state.buffer);
        let previous_before = self.previous_window;
        // The window this call took over and the buffer it showed before, so
        // the restore knows exactly what to put back.
        let mut entered = None;
        if caller_buffer != Some(target) {
            let visible = self.windows().into_iter().find(|window| {
                self.window(*window)
                    .is_ok_and(|state| state.buffer == target)
            });
            match visible {
                Some(window) if window != caller => {
                    if self.set_current_window(window).is_ok() {
                        entered = Some((window, target));
                    }
                }
                Some(_) => {}
                None => {
                    let original = caller_buffer.unwrap_or(target);
                    if self
                        .set_current_buffer(target, BufferRelease::KeepLoaded)
                        .is_ok()
                    {
                        entered = Some((caller, original));
                    }
                }
            }
        }
        let result = f(self);
        // Restore, swallowing every failure so `f`'s result is never masked.
        if let Some((window, expected)) = entered
            && self
                .window(window)
                .is_ok_and(|state| state.buffer != expected)
            && self.buffer(expected).is_ok()
        {
            let _ = self.set_window_buffer(window, expected, BufferRelease::KeepLoaded);
        }
        let prior_previous = self.previous_window;
        if self.current_window() != Some(caller) && self.window(caller).is_ok() {
            let _ = self.set_current_window(caller);
        }
        if prior_previous == Some(caller) {
            self.previous_window = previous_before;
        }
        Ok(result)
    }

    /// Runs `f` with `window` current for Lua `nvim_win_call`, the
    /// window-only counterpart of [`Self::with_buffer_context`]: no
    /// `'eventignore'` gating, no buffer switch, and every window/previous-
    /// window change this call made is undone on the way out without masking
    /// `f`'s own result.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when the caller window cannot
    /// be resolved, or [`EditorError::UnknownWindow`] when `window` is not
    /// live. Context-switch failures during entry or restore are swallowed
    /// rather than masking `f`'s result.
    pub fn with_window_context<T, E>(
        &mut self,
        window: WinHandle,
        f: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<Result<T, E>, EditorError> {
        let target = self.resolve_window_handle(window)?;
        let Some(caller) = self.current_window() else {
            return Err(EditorError::NoCurrentTabpage);
        };
        let previous_before = self.previous_window;
        if target != caller {
            let _ = self.set_current_window(target);
        }
        let result = f(self);
        // Restore, swallowing every failure so `f`'s result is never masked.
        let prior_previous = self.previous_window;
        if self.current_window() != Some(caller) && self.window(caller).is_ok() {
            let _ = self.set_current_window(caller);
        }
        if prior_previous == Some(caller) {
            self.previous_window = previous_before;
        }
        Ok(result)
    }

    fn window_ignores_event(&self, window: WinHandle, event: crate::autocmd::Event) -> bool {
        let Ok(OptionValue::String(ignored)) = self.options.get_window(window, "eventignorewin")
        else {
            return false;
        };
        ignored
            .split(',')
            .any(|name| name == "all" || crate::autocmd::Event::from_name(name) == Some(event))
    }

    /// Returns option state.
    #[must_use]
    pub const fn options(&self) -> &OptionStore {
        &self.options
    }

    /// Returns the decoration provider registry.
    #[must_use]
    pub const fn decorations(&self) -> &Decorations {
        &self.decorations
    }

    /// Returns mutable decoration provider and redraw state.
    pub const fn decorations_mut(&mut self) -> &mut Decorations {
        &mut self.decorations
    }

    /// Returns mutable option state.
    pub const fn options_mut(&mut self) -> &mut OptionStore {
        &mut self.options
    }

    /// Returns register state.
    #[must_use]
    pub const fn registers(&self) -> &Registers {
        &self.registers
    }

    /// Returns mutable register state.
    pub const fn registers_mut(&mut self) -> &mut Registers {
        &mut self.registers
    }

    /// Returns the cached `searchcount()` scan state, when the last scan
    /// produced one.
    #[must_use]
    pub(crate) const fn search_count(&self) -> Option<&crate::search::SearchCountState> {
        self.search_count.as_ref()
    }

    /// Returns mutable `searchcount()` cache state; a no-match scan stores
    /// `None`, mirroring upstream's "no last position" behavior.
    pub(crate) const fn search_count_mut(
        &mut self,
    ) -> &mut Option<crate::search::SearchCountState> {
        &mut self.search_count
    }

    /// Returns the global argument list.
    #[must_use]
    pub const fn arglist(&self) -> &ArgList {
        &self.arglist
    }

    /// Returns mutable global argument list state.
    pub const fn arglist_mut(&mut self) -> &mut ArgList {
        &mut self.arglist
    }

    /// Returns global quickfix state (`quickfix.c` `ql_info`).
    #[must_use]
    pub const fn quickfix(&self) -> &crate::quickfix::QuickfixStack {
        &self.quickfix
    }

    /// Returns the location list stack owned by `window`, if it has one
    /// (`quickfix.c` `GET_LOC_LIST` reading `wp->w_llist`).
    #[must_use]
    pub fn loclist(&self, window: WinHandle) -> Option<&crate::quickfix::QuickfixStack> {
        self.loclists.get(&window)
    }

    /// Returns the location list stack owned by `window`, allocating one
    /// when absent (`ll_get_or_alloc_list`, quickfix.c:2127-2145).
    pub fn loclist_or_alloc_mut(
        &mut self,
        window: WinHandle,
    ) -> &mut crate::quickfix::QuickfixStack {
        self.loclists.entry(window).or_default()
    }

    /// Returns mutable global quickfix state.
    pub const fn quickfix_mut(&mut self) -> &mut crate::quickfix::QuickfixStack {
        &mut self.quickfix
    }

    /// Sets a buffer-local named or specialk.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::UnknownBuffer`] when the buffer is not live, or
    /// [`EditorError::Mark`] when the mark name is invalid.
    pub fn set_local_mark(
        &mut self,
        buffer: BufHandle,
        name: char,
        position: Position,
    ) -> Result<Option<Position>, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        Ok(state.marks.set(name, position)?)
    }

    /// Reads a buffer-local named or specialk.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, or [`EditorError::Mark`] when the mark name is invalid.
    pub fn local_mark(
        &self,
        buffer: BufHandle,
        name: char,
    ) -> Result<Option<Position>, EditorError> {
        Ok(self.buffer(buffer)?.marks.get(name)?)
    }

    /// Removes one buffer-local named or special mark.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::UnknownBuffer`] when the buffer is not live, or
    /// [`EditorError::Mark`] when the mark name is invalid.
    pub fn remove_local_mark(
        &mut self,
        buffer: BufHandle,
        name: char,
    ) -> Result<Option<Position>, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        Ok(state.marks.remove(name)?)
    }

    /// Clears deletable local marks and the change list for `buffer`.
    ///
    /// The `:` mark is read-only and survives `:delmarks!`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::UnknownBuffer`] when the buffer is not live.
    pub fn clear_local_marks(&mut self, buffer: BufHandle) -> Result<(), EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        for name in ('a'..='z').chain(['\'', '`', '.', '^', '[', ']', '"']) {
            state.marks.remove(name)?;
        }
        self.changelists.clear(buffer);
        Ok(())
    }
    /// Returns globalks.
    #[must_use]
    pub const fn global_marks(&self) -> &GlobalMarks {
        &self.global_marks
    }

    /// Returns mutable globalks.
    pub const fn global_marks_mut(&mut self) -> &mut GlobalMarks {
        &mut self.global_marks
    }

    /// Returns named highlight definitions.
    #[must_use]
    pub const fn highlights(&self) -> &BTreeMap<String, HighlightDefinition> {
        &self.highlights
    }

    /// Returns mutable named highlight definitions.
    pub const fn highlights_mut(&mut self) -> &mut BTreeMap<String, HighlightDefinition> {
        &mut self.highlights
    }

    pub(crate) fn sign_definitions(&self) -> &BTreeMap<String, SignDefinition> {
        &self.sign_definitions
    }

    pub(crate) fn sign_definitions_mut(&mut self) -> &mut BTreeMap<String, SignDefinition> {
        &mut self.sign_definitions
    }

    /// Resolves the sign group for `name`, allocating its namespace on first
    /// use the way upstream's `buf_set_sign` creates one per group.
    pub(crate) fn sign_group(&mut self, name: &str) -> SignGroup {
        if let Some(group) = self.sign_groups.get(name) {
            return *group;
        }
        let offset = u32::try_from(self.sign_groups.len()).unwrap_or(u32::MAX);
        let raw = SignGroup::NAMED_BASE.saturating_add(offset);
        let namespace = NamespaceId::new(raw).unwrap_or_else(|_| NamespaceId::legacy_sign());
        let group = SignGroup::from_namespace(namespace);
        self.sign_groups.insert(name.to_owned(), group);
        group
    }

    /// Returns the sign group for `name` when `:sign place` already created it.
    pub(crate) fn sign_group_if_placed(&self, name: &str) -> Option<SignGroup> {
        self.sign_groups.get(name).copied()
    }

    /// Every named sign group allocated so far, in name order.
    pub(crate) fn sign_groups(&self) -> impl Iterator<Item = SignGroup> + '_ {
        self.sign_groups.values().copied()
    }

    /// Returns buffer-separated change history.
    #[must_use]
    pub const fn changelists(&self) -> &Changelists {
        &self.changelists
    }

    /// Returns editor jump history.
    #[must_use]
    pub const fn jumplist(&self) -> &Jumplist {
        &self.jumplist
    }

    /// Returns mutable editor jump history.
    pub const fn jumplist_mut(&mut self) -> &mut Jumplist {
        &mut self.jumplist
    }

    /// Clears the jump history (`:clearjumps`, mark.c `ex_clearjumps`):
    /// the entries and the navigation index reset together.
    pub fn clear_jumplist(&mut self) {
        self.jumplist = Jumplist::new();
    }

    /// Returns registered autocmd and augroup state.
    #[must_use]
    pub const fn autocmds(&self) -> &Autocmds {
        &self.autocmds
    }

    /// Returns mutable autocmd and augroup state.
    pub const fn autocmds_mut(&mut self) -> &mut Autocmds {
        &mut self.autocmds
    }

    /// Returns mapping and abbreviation state.
    #[must_use]
    pub const fn mappings(&self) -> &Mappings {
        &self.mappings
    }

    /// Returns mutable mapping and abbreviation state.
    pub const fn mappings_mut(&mut self) -> &mut Mappings {
        &mut self.mappings
    }

    /// Returns queued encoded input.
    #[must_use]
    pub const fn typeahead(&self) -> &Typeahead {
        &self.typeahead
    }

    /// Returns mutable queued encoded input.
    pub const fn typeahead_mut(&mut self) -> &mut Typeahead {
        &mut self.typeahead
    }

    /// Returns editor-wide `g:` variables.
    #[must_use]
    pub const fn gvars(&self) -> &Dict {
        &self.gvars
    }

    /// Returns mutable editor-wide `g:` variables.
    pub fn gvars_mut(&mut self) -> &mut Dict {
        self.gvars_version = self.gvars_version.wrapping_add(1);
        &mut self.gvars
    }

    /// Returns the `g:` variable-map version used by the differential sync.
    #[must_use]
    pub const fn gvars_version(&self) -> u64 {
        self.gvars_version
    }

    /// Returns editor-wide `v:` variables.
    #[must_use]
    pub const fn vvars(&self) -> &Dict {
        &self.vvars
    }

    /// Returns mutable editor-wide `v:` variables.
    pub fn vvars_mut(&mut self) -> &mut Dict {
        self.vvars_version = self.vvars_version.wrapping_add(1);
        &mut self.vvars
    }

    /// Returns the `v:` variable-map version used by the differential sync.
    #[must_use]
    pub const fn vvars_version(&self) -> u64 {
        self.vvars_version
    }

    /// Sets the read-only `v:register` to `name` and bumps `vvars_version`
    /// so the differential editor→scope sync propagates the new value to
    /// Vimscript and Lua. This is the sole internal writer — scripts cannot
    /// reach it through `Scope::set_scoped` because `v:` variables are not
    /// in any writable scope kind.
    pub(crate) fn set_v_register(&mut self, name: char) {
        // Register names are always single ASCII characters.
        self.vvars
            .insert("register".into(), Object::String(OxStr(vec![name as u8])));
        self.vvars_version = self.vvars_version.wrapping_add(1);
    }

    /// Returns messages retained by the editor sink.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Returns the sink decision recorded for each retained message.
    ///
    /// Index for index with [`Editor::messages`].
    #[must_use]
    pub fn message_destinations(&self) -> &[MessageDestination] {
        &self.message_destinations
    }

    /// Where a message produced now is sent.
    ///
    /// `message.c` `msg_use_printf` (line 3013) sends output to stdout or
    /// stderr unless an `--embed` peer or an attached UI can display it;
    /// `msg_puts_printf` then drops the text while `silent_mode` is set and
    /// `'verbose'` is zero (line 3038), and otherwise writes it to stderr
    /// (line 3049).
    #[must_use]
    pub fn message_destination(&self) -> MessageDestination {
        if self.message_routing.embedded || self.message_routing.ui_attached {
            return MessageDestination::Ui;
        }
        if self.message_routing.silent && self.verbose_level() == 0 {
            return MessageDestination::Suppressed;
        }
        MessageDestination::Stderr
    }

    /// `'verbose'` (`p_verbose`), zero when the option holds no number.
    #[must_use]
    fn verbose_level(&self) -> i64 {
        match self.options.get_global("verbose") {
            Ok(OptionValue::Number(level)) => *level,
            _ => 0,
        }
    }

    /// Stores a message without claiming that a UI has rendered it, together
    /// with the sink decision that applies to it.
    pub fn push_message(&mut self, message: Message) {
        if message.kind == MessageKind::Error
            && let Object::String(text) = &message.content
        {
            self.vvars_mut()
                .insert(OxStr::from("errmsg"), Object::String(text.clone()));
        }
        let kind = message.kind;
        let destination = self.message_destination();
        let identity = self
            .pending_echo_identity
            .take()
            .unwrap_or_else(|| MessageIdentity::of(kind));
        if identity.id != Object::Nil
            && let Some(slot) = self
                .message_identities
                .iter()
                .rposition(|existing| existing.id == identity.id)
        {
            self.messages[slot] = message;
            self.message_destinations[slot] = destination;
            self.message_identities[slot] = identity.clone();
            if destination == MessageDestination::Ui {
                self.echo_replacements
                    .push((self.messages[slot].clone(), identity));
            }
            return;
        }
        self.messages.push(message);
        self.message_destinations.push(destination);
        self.message_identities.push(identity);
    }

    /// Stores output produced by an informative listing command.
    ///
    /// `print_line` (`ex_cmds.c` line 1701, `:print`/`:number`/`:list`) and
    /// `showoneopt` (`option.c` line 4851, `:set` display) clear
    /// `silent_mode` and set `info_message` around their own output, so that
    /// output survives `-es` and goes to stdout rather than stderr. Only the
    /// printf branch differs: with a UI attached it is an ordinary message.
    pub fn push_info_message(&mut self, message: Message) {
        let destination = match self.message_destination() {
            MessageDestination::Ui => MessageDestination::Ui,
            _ => MessageDestination::Stdout,
        };
        let kind = message.kind;
        self.messages.push(message);
        self.message_destinations.push(destination);
        self.message_identities.push(MessageIdentity::of(kind));
    }

    /// Discards messages appended at or after `len`.
    ///
    /// Command-output capture uses this after copying newly emitted messages,
    /// matching Neovim's behavior where captured output is not also displayed.
    pub fn truncate_messages(&mut self, len: usize) {
        self.messages.truncate(len);
        self.message_destinations.truncate(len);
        self.message_identities.truncate(len);
    }

    /// Returns the `nvim_echo` identity recorded for each retained message.
    ///
    /// Index for index with [`Editor::messages`].
    #[must_use]
    pub fn message_identities(&self) -> &[MessageIdentity] {
        &self.message_identities
    }

    /// Stages raw `nvim_ui_send` content for the server's redraw pass
    /// (`nvim_ui_send`, `api/ui.c:1102-1106`).
    pub fn queue_ui_send(&mut self, content: OxStr) {
        self.ui_sends.push(content);
    }

    /// Whether any `nvim_ui_send` payload awaits the next redraw pass.
    #[must_use]
    pub fn ui_sends_pending(&self) -> bool {
        !self.ui_sends.is_empty()
    }

    /// Takes every staged `nvim_ui_send` payload.
    pub fn take_ui_sends(&mut self) -> Vec<OxStr> {
        std::mem::take(&mut self.ui_sends)
    }

    /// Stages one validated `nvim__redraw` request for the server's redraw
    /// pass (`nvim__redraw`, `api/vim.c:2469`).
    pub fn queue_redraw(&mut self, request: RedrawRequest) {
        self.redraws.push(request);
    }

    /// Whether any `nvim__redraw` request awaits the next redraw pass.
    #[must_use]
    pub fn redraws_pending(&self) -> bool {
        !self.redraws.is_empty()
    }

    /// Takes every staged `nvim__redraw` request.
    pub fn take_redraws(&mut self) -> Vec<RedrawRequest> {
        std::mem::take(&mut self.redraws)
    }

    /// Arms the identity that the next `nvim_echo` message push consumes.
    ///
    /// The API handler emits its message before firing `Progress`, so this
    /// state must be attached by [`Editor::push_message`] before any callback
    /// can reenter the editor.
    pub fn arm_echo_identity(&mut self, kind: OxStr, id: Object) {
        self.pending_echo_identity = Some(MessageIdentity { kind, id });
    }

    /// Drops an armed identity when validation or verbose gating prevents a
    /// handler from pushing a message.
    pub fn cancel_echo_identity(&mut self) {
        self.pending_echo_identity = None;
    }

    /// Takes in-place UI replacements recorded while an echo dispatch was
    /// reentrant. Appended messages remain on the normal watermark path.
    pub fn take_echo_replacements(&mut self) -> Vec<(Message, MessageIdentity)> {
        std::mem::take(&mut self.echo_replacements)
    }

    /// Stamps the generated id returned by `nvim_echo` onto its appended
    /// message. Explicit ids are attached by [`Editor::push_message`] before
    /// `Progress` can reenter the API, so this path never searches for a
    /// matching earlier entry.
    pub fn stamp_echo_identity(&mut self, appended_at: usize, kind: OxStr, id: Object) {
        if let Some(identity) = self.message_identities.get_mut(appended_at) {
            *identity = MessageIdentity { kind, id };
        }
    }

    /// Returns tabpage-local variables.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-tabpage
    /// request has no live tabpage, or [`EditorError::UnknownTabpage`] when
    /// the tabpage is not live.
    pub fn tabpage_variables(&self, tab: TabHandle) -> Result<&Dict, EditorError> {
        Ok(self.tabpage(tab)?.variables())
    }

    /// Returns mutable tabpage-local variables.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-tabpage
    /// request has no live tabpage, or [`EditorError::UnknownTabpage`] when
    /// the tabpage is not live.
    pub fn tabpage_variables_mut(&mut self, tab: TabHandle) -> Result<&mut Dict, EditorError> {
        let resolved = if tab.is_current() {
            self.current_tab.ok_or(EditorError::NoCurrentTabpage)?
        } else {
            tab
        };
        Ok(self
            .tabpages
            .get_mut(&resolved)
            .ok_or(EditorError::UnknownTabpage(resolved))?
            .variables_mut())
    }

    /// Returns the tabpage variable-map version used by the differential
    /// Ex-variable sync.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-tabpage
    /// request has no live tabpage, or [`EditorError::UnknownTabpage`] when
    /// the tabpage is not live.
    pub fn tabpage_variables_version(&self, tab: TabHandle) -> Result<u64, EditorError> {
        Ok(self.tabpage(tab)?.variables_version())
    }

    /// Returns one tabpage's tiled and floating windows in display order.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-tabpage
    /// request has no live tabpage, or [`EditorError::UnknownTabpage`] when
    /// the tabpage is not live.
    pub fn tabpage_windows(&self, tab: TabHandle) -> Result<Vec<WinHandle>, EditorError> {
        Ok(self.tabpage(tab)?.windows())
    }

    /// Resizes and equalizes a tabpage's tiled layout.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-tabpage
    /// request has no live tabpage, [`EditorError::UnknownTabpage`] when the
    /// tabpage is not live, or [`EditorError::Layout`] when the new geometry
    /// does not fit the tiled layout.
    pub fn resize_tabpage(
        &mut self,
        tab: TabHandle,
        geometry: Geometry,
    ) -> Result<(), EditorError> {
        let resolved = if tab.is_current() {
            self.current_tab.ok_or(EditorError::NoCurrentTabpage)?
        } else {
            tab
        };
        self.tabpages
            .get_mut(&resolved)
            .ok_or(EditorError::UnknownTabpage(resolved))?
            .resize(geometry)?;
        Ok(())
    }

    /// Equalizes a tabpage's tiled layout within its current rectangle.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-tabpage
    /// request has no live tabpage, [`EditorError::UnknownTabpage`] when the
    /// tabpage is not live, or [`EditorError::Layout`] when the tiled layout
    /// cannot be equalized within its current rectangle.
    pub fn equalize_tabpage(&mut self, tab: TabHandle) -> Result<(), EditorError> {
        let resolved = if tab.is_current() {
            self.current_tab.ok_or(EditorError::NoCurrentTabpage)?
        } else {
            tab
        };
        self.tabpages
            .get_mut(&resolved)
            .ok_or(EditorError::UnknownTabpage(resolved))?
            .equalize()?;
        Ok(())
    }

    /// Returns window-local variables.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage does not know the
    /// window.
    pub fn window_variables(&self, window: WinHandle) -> Result<&Dict, EditorError> {
        let tab = self.window_tabpage(window)?;
        Ok(self.tabpage(tab)?.window_api_state(window)?.variables())
    }

    /// Returns mutable window-local variables.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage does not know the
    /// window.
    pub fn window_variables_mut(&mut self, window: WinHandle) -> Result<&mut Dict, EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        Ok(self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .window_api_state_mut(resolved)?
            .variables_mut())
    }

    /// Returns the window variable-map version used by the differential
    /// Ex-variable sync.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Editor::window_variables`].
    pub fn window_variables_version(&self, window: WinHandle) -> Result<u64, EditorError> {
        let tab = self.window_tabpage(window)?;
        Ok(self
            .tabpage(tab)?
            .window_api_state(window)?
            .variables_version())
    }

    /// Returns a window's tag stack.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage does not know the
    /// window.
    pub fn window_tag_stack(
        &self,
        window: WinHandle,
    ) -> Result<&crate::tags::TagStack, EditorError> {
        let tab = self.window_tabpage(window)?;
        Ok(self.tabpage(tab)?.window_api_state(window)?.tag_stack())
    }

    /// Returns a mutable window tag stack.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage does not know the
    /// window.
    pub fn window_tag_stack_mut(
        &mut self,
        window: WinHandle,
    ) -> Result<&mut crate::tags::TagStack, EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        Ok(self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .window_api_state_mut(resolved)?
            .tag_stack_mut())
    }

    /// Returns the highlight namespace selected for a window.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage does not know the
    /// window.
    pub fn window_highlight_namespace(&self, window: WinHandle) -> Result<i64, EditorError> {
        let tab = self.window_tabpage(window)?;
        Ok(self
            .tabpage(tab)?
            .window_api_state(window)?
            .highlight_namespace())
    }

    /// Selects a window highlight namespace without attempting to render it.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] when the window is
    /// not live, [`EditorError::UnknownTabpage`] when its owning tabpage is
    /// missing, or [`EditorError::Layout`] when the tabpage does not know the
    /// window.
    pub fn set_window_highlight_namespace(
        &mut self,
        window: WinHandle,
        namespace: i64,
    ) -> Result<(), EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        self.tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .window_api_state_mut(resolved)?
            .set_highlight_namespace(namespace);
        Ok(())
    }

    /// Finds a window by its numeric id (handle) across all tabpages.
    #[must_use]
    pub fn find_window_by_id(&self, id: i64) -> Option<WinHandle> {
        if id <= 0 {
            return self.current_window();
        }
        let target = WinHandle::try_from(id).unwrap_or(WinHandle::CURRENT);
        self.windows.keys().find(|&&w| w == target).copied()
    }

    /// Adds a match item to a window, returning the assigned ID.
    pub(crate) fn add_match(
        &mut self,
        window: WinHandle,
        item: crate::builtins::matches::MatchItem,
    ) -> i64 {
        let resolved = if window.is_current() {
            self.current_window().unwrap_or(window)
        } else {
            window
        };
        let Ok(tab) = self.window_tabpage(resolved) else {
            return -1;
        };
        let Some(api) = self
            .tabpages
            .get_mut(&tab)
            .and_then(|tp| tp.window_api_state_mut(resolved).ok())
        else {
            return -1;
        };
        let id = if item.id < 0 {
            let id = api.next_match_id();
            api.set_next_match_id(id + 1);
            id
        } else {
            if api.next_match_id() < item.id + 100 {
                api.set_next_match_id(item.id + 100);
            }
            item.id
        };
        let mut item = item;
        item.id = id;
        api.matches_mut().push(item);
        id
    }

    /// Deletes a match item by ID from a window. Returns `true` if found.
    pub fn delete_match(&mut self, window: WinHandle, id: i64) -> bool {
        let resolved = if window.is_current() {
            self.current_window().unwrap_or(window)
        } else {
            window
        };
        let Ok(tab) = self.window_tabpage(resolved) else {
            return false;
        };
        let Some(api) = self
            .tabpages
            .get_mut(&tab)
            .and_then(|tp| tp.window_api_state_mut(resolved).ok())
        else {
            return false;
        };
        let before = api.matches().len();
        api.matches_mut().retain(|m| m.id != id);
        api.matches().len() < before
    }

    /// Clears all match items from a window.
    pub fn clear_matches(&mut self, window: WinHandle) {
        let resolved = if window.is_current() {
            self.current_window().unwrap_or(window)
        } else {
            window
        };
        if let Ok(tab) = self.window_tabpage(resolved)
            && let Some(api) = self
                .tabpages
                .get_mut(&tab)
                .and_then(|tp| tp.window_api_state_mut(resolved).ok())
        {
            api.matches_mut().clear();
        }
    }

    /// Returns the match items for a window.
    #[must_use]
    pub(crate) fn get_matches(
        &self,
        window: WinHandle,
    ) -> Vec<crate::builtins::matches::MatchItem> {
        let resolved = if window.is_current() {
            self.current_window().unwrap_or(window)
        } else {
            window
        };
        let Ok(tab) = self.window_tabpage(resolved) else {
            return Vec::new();
        };
        let Some(api) = self
            .tabpage(tab)
            .ok()
            .and_then(|tp| tp.window_api_state(resolved).ok())
        else {
            return Vec::new();
        };
        api.matches().to_vec()
    }

    /// Returns assigned screen geometry for a tiled window.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] or
    /// [`EditorError::UnknownTabpage`] when a handle is not live, or
    /// [`EditorError::Layout`] when the tabpage has no geometry for the
    /// window.
    pub fn window_geometry(&self, window: WinHandle) -> Result<Geometry, EditorError> {
        let tab = self.window_tabpage(window)?;
        Ok(self.tabpage(tab)?.window_geometry(window)?)
    }

    /// Changes a tiled or floating window's width.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] or
    /// [`EditorError::UnknownTabpage`] when a handle is not live, or
    /// [`EditorError::Layout`] when `width` violates the window's extent
    /// rules.
    pub fn set_window_width(&mut self, window: WinHandle, width: usize) -> Result<(), EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        self.tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .set_window_width(resolved, width)?;
        Ok(())
    }

    /// Changes a tiled or floating window's height.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] or
    /// [`EditorError::UnknownTabpage`] when a handle is not live, or
    /// [`EditorError::Layout`] when `height` violates the window's extent
    /// rules.
    pub fn set_window_height(
        &mut self,
        window: WinHandle,
        height: usize,
    ) -> Result<(), EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        self.tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .set_window_height(resolved, height)?;
        Ok(())
    }
    /// Returns the renderable text-row count for a window.
    ///
    /// # Errors
    ///
    /// Returns an editor error when the window or its tabpage is not live.
    pub fn window_text_height(&self, window: WinHandle) -> Result<usize, EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        let tabpage = self
            .tabpages
            .get(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        Ok(viewport_height(tabpage, resolved))
    }

    /// Returns floating configuration, or `None` for a tiled window.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] or
    /// [`EditorError::UnknownTabpage`] when a handle is not live, or
    /// [`EditorError::Layout`] when the tabpage does not know the window.
    pub fn window_config(&self, window: WinHandle) -> Result<Option<&WinConfig>, EditorError> {
        let tab = self.window_tabpage(window)?;
        Ok(self.tabpage(tab)?.window_config(window)?)
    }

    /// Updates an existing floating window configuration.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-window request
    /// has no live tabpage, [`EditorError::UnknownWindow`] or
    /// [`EditorError::UnknownTabpage`] when a handle is not live, or
    /// [`EditorError::Layout`] when the new configuration is invalid.
    pub fn set_window_config(
        &mut self,
        window: WinHandle,
        config: WinConfig,
    ) -> Result<(), EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        let cursor = self.cursor_position_for_float(tab, resolved, config.relative)?;
        self.tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .set_window_config(resolved, config, cursor)?;
        Ok(())
    }

    /// Allocates a listed, loaded empty buffer.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::HandleExhausted`] when the buffer handle space
    /// is exhausted.
    pub fn create_buffer(&mut self, listed: bool) -> Result<BufHandle, EditorError> {
        self.create_buffer_with(Buffer::new(), listed)
    }

    /// Allocates a listed or unlisted buffer around existing text.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::HandleExhausted`] when the buffer handle space
    /// is exhausted.
    pub fn create_buffer_with(
        &mut self,
        text: Buffer,
        listed: bool,
    ) -> Result<BufHandle, EditorError> {
        let handle = allocate_buffer_handle(&mut self.next_buffer)?;
        self.buffers.insert(handle, BufferState::new(text, listed));
        self.options.snapshot_buffer_defaults(handle);
        Ok(handle)
    }

    /// Permanently removes an unattached buffer; its handle is never reused.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-buffer request
    /// has no live tabpage, [`EditorError::UnknownBuffer`] when the buffer is
    /// not live, or [`EditorError::BufferInUse`] when windows still display
    /// it.
    pub fn wipe_buffer(&mut self, buffer: BufHandle) -> Result<(), EditorError> {
        let buffer = self.resolve_buffer_handle(buffer)?;
        let state = self
            .buffers
            .get(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        if state.attachments != 0 {
            return Err(EditorError::BufferInUse {
                buffer,
                windows: state.attachments,
            });
        }
        self.changelists.remove_buffer(buffer);
        self.options.remove_buffer(buffer);
        self.autocmds.remove_buffer(buffer);
        self.mappings.remove_buffer(buffer);
        let state = self
            .buffers
            .remove(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        self.pending_subscription_releases
            .extend(state.into_released_subscriptions_for(buffer));
        Ok(())
    }

    /// Releases resident text and undo state for an unattached buffer.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-buffer request
    /// has no live tabpage, [`EditorError::UnknownBuffer`] when the buffer is
    /// not live, or [`EditorError::Buffer`] when the buffer still has
    /// attached windows.
    pub fn unload_buffer(&mut self, buffer: BufHandle) -> Result<(), EditorError> {
        let buffer = self.resolve_buffer_handle(buffer)?;
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        state.unload()?;
        Ok(())
    }

    /// Creates a tabpage with one tiled window displaying `buffer`, appended
    /// after every existing tabpage.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, [`EditorError::HandleExhausted`] when the window or tabpage
    /// handle space is exhausted, [`EditorError::Buffer`] when attaching the
    /// buffer fails, or [`EditorError::Layout`] when the geometry cannot be
    /// realized.
    pub fn create_tabpage(
        &mut self,
        buffer: BufHandle,
        geometry: Geometry,
    ) -> Result<TabHandle, EditorError> {
        let index = self.tab_order.len();
        self.insert_tabpage(buffer, geometry, index)
    }

    /// Creates a tabpage at upstream's `win_new_tabpage(after)` position
    /// (`window.c:4484-4539`).
    ///
    /// `after` is one-based: `1` makes the new tabpage the first, a larger
    /// value inserts it *before* tabpage `after`, and a value past the end
    /// appends. `0` inserts directly after the current tabpage, which is what
    /// an addressless `:tabnew` passes.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, [`EditorError::HandleExhausted`] when the window or tabpage
    /// handle space is exhausted, [`EditorError::Buffer`] when attaching the
    /// buffer fails, or [`EditorError::Layout`] when the geometry cannot be
    /// realized.
    pub fn create_tabpage_at(
        &mut self,
        buffer: BufHandle,
        geometry: Geometry,
        after: usize,
    ) -> Result<TabHandle, EditorError> {
        let index = if after == 0 {
            self.current_tab
                .and_then(|tab| self.tabpage_index(tab))
                .unwrap_or(self.tab_order.len())
        } else {
            (after - 1).min(self.tab_order.len())
        };
        self.insert_tabpage(buffer, geometry, index)
    }

    /// Closes a tabpage and every window it owns.
    ///
    /// This is the sole owner of tabpage removal. The window path cannot take
    /// that job: `Layout::close` refuses `LastWindow`, so `close_window` is
    /// structurally unable to empty a tabpage and never removes one. Upstream
    /// puts removal in `win_close` instead (`window.c`), which it can because
    /// its layout permits closing a tabpage's last window.
    ///
    /// Refuses the last remaining tabpage, upstream's `E784`.
    ///
    /// On success the surviving current window's effective working directory
    /// is reapplied best-effort.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownTabpage`] when the tabpage handle cannot be
    /// resolved, [`EditorError::LastTabpage`] when it is the only tabpage, or
    /// the errors of [`Self::close_window`] while closing its remaining
    /// windows.
    pub fn close_tabpage(&mut self, tab: TabHandle) -> Result<(), EditorError> {
        let tab = self.resolve_tabpage_handle(tab)?;
        self.require_tabpage(tab)?;
        if self.tab_order.len() <= 1 {
            return Err(EditorError::LastTabpage);
        }
        // Close every window but the last through the normal path so buffers
        // detach and window options are dropped; the last one goes with the
        // tabpage below, since the layout will not close it.
        let windows = self.tabpage(tab)?.windows();
        for window in windows.iter().skip(1) {
            self.close_window(tab, *window, true)?;
        }
        let removed = self
            .tabpages
            .remove(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        for window in removed.windows() {
            self.windows.remove(&window);
            self.loclists.remove(&window);
            self.options.remove_window(window);
            if let Ok(state) = removed.window(window)
                && let Some(buffer_state) = self.buffers.get_mut(&state.buffer)
            {
                buffer_state.detach(true);
            }
        }
        self.tab_order.retain(|entry| *entry != tab);
        if self.current_tab == Some(tab) {
            self.current_tab = self.tab_order.first().copied();
        }
        self.apply_effective_directory();
        Ok(())
    }

    fn insert_tabpage(
        &mut self,
        buffer: BufHandle,
        geometry: Geometry,
        index: usize,
    ) -> Result<TabHandle, EditorError> {
        let buffer = self.resolve_buffer_handle(buffer)?;
        self.require_buffer(buffer)?;
        // Copy the source current window's `w_localdir`/`w_prevdir` into the
        // new tabpage's first window, as upstream `win_alloc` does; an
        // editor with no current window contributes `None`/`None`.
        let (local_directory, previous_directory) = match self
            .current_window()
            .and_then(|window| self.window(window).ok())
        {
            Some(state) => (
                state.local_directory.clone(),
                state.previous_directory.clone(),
            ),
            None => (None, None),
        };
        let window = allocate_window_handle(&mut self.next_window)?;
        let tab = allocate_tab_handle(&mut self.next_tabpage)?;
        let mut state = WindowState::new(buffer, Position { lnum: 1, col: 0 });
        state.local_directory = local_directory;
        state.previous_directory = previous_directory;
        let layout = Layout::new(window, state, geometry)?;
        if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
            buffer_state.attach()?;
        }
        self.windows.insert(window, tab);
        self.tabpages.insert(tab, TabpageState::new(layout));
        self.tab_order.insert(index.min(self.tab_order.len()), tab);
        if self.global_directory.is_none() {
            // Initialize globaldir from the process cwd only on explicit
            // success; an unreadable cwd leaves it `None` so later reapplies
            // fall back to the unchanged process cwd.
            if let Ok(cwd) = std::env::current_dir() {
                self.global_directory = Some(cwd);
            }
        }
        self.current_tab = Some(tab);
        self.apply_effective_directory();
        Ok(tab)
    }

    /// Makes a live tabpage current.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a current-tabpage
    /// request has no live tabpage, or [`EditorError::UnknownTabpage`] when
    /// the tabpage is not live.
    pub fn set_current_tabpage(&mut self, tab: TabHandle) -> Result<(), EditorError> {
        let tab = self.resolve_tabpage_handle(tab)?;
        self.require_tabpage(tab)?;
        self.current_tab = Some(tab);
        self.apply_effective_directory();
        Ok(())
    }

    /// Splits a tiled window vertically and displays `buffer` on the right.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a handle is requested as
    /// current with no live tabpage, [`EditorError::UnknownTabpage`],
    /// [`EditorError::UnknownWindow`], or [`EditorError::UnknownBuffer`] when
    /// a handle is not live, [`EditorError::Buffer`] when attaching the buffer
    /// fails, or [`EditorError::Layout`] when the split cannot fit. On failure
    /// no window is created.
    pub fn split_vertical(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
        enter: bool,
    ) -> Result<WinHandle, EditorError> {
        self.split_window(tab, target, buffer, SplitDirection::Right, enter)
    }

    /// Splits a tiled window horizontally and displays `buffer` below.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a handle is requested as
    /// current with no live tabpage, [`EditorError::UnknownTabpage`],
    /// [`EditorError::UnknownWindow`], or [`EditorError::UnknownBuffer`] when
    /// a handle is not live, [`EditorError::Buffer`] when attaching the buffer
    /// fails, or [`EditorError::Layout`] when the split cannot fit. On failure
    /// no window is created.
    pub fn split_horizontal(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
        enter: bool,
    ) -> Result<WinHandle, EditorError> {
        self.split_window(tab, target, buffer, SplitDirection::Below, enter)
    }

    /// Splits a tiled window vertically and displays `buffer` to the left.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a handle is requested as
    /// current with no live tabpage, [`EditorError::UnknownTabpage`],
    /// [`EditorError::UnknownWindow`], or [`EditorError::UnknownBuffer`] when
    /// a handle is not live, [`EditorError::Buffer`] when attaching the buffer
    /// fails, or [`EditorError::Layout`] when the split cannot fit. On failure
    /// no window is created.
    pub fn split_left(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
        enter: bool,
    ) -> Result<WinHandle, EditorError> {
        self.split_window(tab, target, buffer, SplitDirection::Left, enter)
    }

    /// Splits a tiled window horizontally and displays `buffer` above.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a handle is requested as
    /// current with no live tabpage, [`EditorError::UnknownTabpage`],
    /// [`EditorError::UnknownWindow`], or [`EditorError::UnknownBuffer`] when
    /// a handle is not live, [`EditorError::Buffer`] when attaching the buffer
    /// fails, or [`EditorError::Layout`] when the split cannot fit. On failure
    /// no window is created.
    pub fn split_above(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
        enter: bool,
    ) -> Result<WinHandle, EditorError> {
        self.split_window(tab, target, buffer, SplitDirection::Above, enter)
    }

    /// Computes the cursor's rendered position within an anchor window.
    ///
    /// `win_config_float` freezes cursor-relative coordinates from `w_wrow`
    /// and `w_wcol`, not from the cursor's byte position. The layout layer
    /// cannot derive those values because it does not own buffer text or
    /// options, so this snapshot is prepared before the tabpage borrow.
    fn cursor_screen_position(
        &self,
        window: WinHandle,
    ) -> Result<CursorScreenPosition, EditorError> {
        let (buffer, cursor, topline, coladd) = {
            let state = self.window(window)?;
            (state.buffer, state.cursor, state.topline, state.coladd)
        };
        let geometry = self.window_geometry(window)?;
        let tabstop = match self.options().get_buffer(buffer, "tabstop") {
            Ok(OptionValue::Number(value)) if *value > 0 => usize::try_from(*value).unwrap_or(8),
            _ => crate::builtins::position::tabstop(self),
        };
        let wrap = match self.options().get_window(window, "wrap") {
            Ok(OptionValue::Boolean(value)) => *value,
            _ => true,
        };
        let text = self.buffer(buffer)?.text()?;
        let cursor_line = text.line(cursor.lnum).map_err(BufferStateError::from)?;
        let virtual_column = cursor_vcol(&cursor_line, cursor.col, tabstop).saturating_add(
            usize::try_from(coladd.max(0)).unwrap_or(usize::MAX),
        );
        if !wrap || geometry.width == 0 {
            return Ok(CursorScreenPosition {
                row: cursor.lnum.saturating_sub(topline),
                col: virtual_column,
            });
        }

        let first_line = topline.min(cursor.lnum).max(1);
        let mut row = 0usize;
        for lnum in first_line..cursor.lnum {
            let line = text.line(lnum).map_err(BufferStateError::from)?;
            let line_cells = cursor_vcol(&line, line.len(), tabstop);
            row = row.saturating_add(wrapped_line_rows(line_cells, geometry.width));
        }
        row = row.saturating_add(virtual_column / geometry.width);
        Ok(CursorScreenPosition {
            row,
            col: virtual_column % geometry.width,
        })
    }

    /// Captures the rendered cursor position needed when freezing a float.
    fn cursor_position_for_float(
        &self,
        tab: TabHandle,
        window: WinHandle,
        relative: RelativeTo,
    ) -> Result<CursorScreenPosition, EditorError> {
        if !matches!(relative, RelativeTo::Cursor) {
            return Ok(CursorScreenPosition::default());
        }
        let tabpage = self
            .tabpages
            .get(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        tabpage
            .cursor_anchor_window(window)
            .map_or(Ok(CursorScreenPosition::default()), |anchor| {
                self.cursor_screen_position(anchor)
            })
    }

    /// Opens a floating window in `tab`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a handle is requested as
    /// current with no live tabpage, [`EditorError::UnknownTabpage`] or
    /// [`EditorError::UnknownBuffer`] when a handle is not live,
    /// [`EditorError::Buffer`] when attaching the buffer fails, or
    /// [`EditorError::Layout`] when the float configuration is invalid. On
    /// failure the buffer stays detached.
    pub fn open_float(
        &mut self,
        tab: TabHandle,
        buffer: BufHandle,
        config: WinConfig,
    ) -> Result<WinHandle, EditorError> {
        let tab = self.resolve_tabpage_handle(tab)?;
        let buffer = self.resolve_buffer_handle(buffer)?;
        self.require_buffer(buffer)?;
        self.require_tabpage(tab)?;
        let window = allocate_window_handle(&mut self.next_window)?;
        let cursor = self.cursor_position_for_float(tab, window, config.relative)?;
        let state = WindowState::new(buffer, Position { lnum: 1, col: 0 });
        if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
            buffer_state.attach()?;
        }
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        if let Err(error) = tabpage.add_float(window, state, config, cursor) {
            if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
                buffer_state.detach(true);
            }
            return Err(error.into());
        }
        self.windows.insert(window, tab);
        Ok(window)
    }

    /// Closes a tiled or floating window, applying the effective hidden policy.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] when a handle is requested as
    /// current with no live tabpage, [`EditorError::UnknownTabpage`] when the
    /// tabpage is not live or does not own `window`,
    /// [`EditorError::UnknownWindow`] when the window is not live, or
    /// [`EditorError::Layout`] when the layout refuses to close its last
    /// tiled window.
    pub fn close_window(
        &mut self,
        tab: TabHandle,
        window: WinHandle,
        keep_buffer_loaded: bool,
    ) -> Result<WindowState, EditorError> {
        let tab = self.resolve_tabpage_handle(tab)?;
        let window = self.resolve_window_handle(window)?;
        self.require_tabpage(tab)?;
        if self.windows.get(&window) != Some(&tab) {
            return Err(EditorError::UnknownWindow(window));
        }
        let canonical = self
            .tabpages
            .get(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .window(window)?
            .clone();
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        let removed = if tabpage
            .floating_windows()
            .any(|candidate| candidate.window == window)
        {
            tabpage.remove_float(window)?.state
        } else {
            tabpage.close_tiled(window)?
        };
        self.windows.remove(&window);
        // `win_free` frees the window's location list stack with it
        // (`qf_free_all`, quickfix.c:1848-1858, window.c:5667).
        self.loclists.remove(&window);
        self.options.remove_window(window);
        if let Some(buffer_state) = self.buffers.get_mut(&canonical.buffer) {
            buffer_state.detach(keep_buffer_loaded);
        }
        self.apply_effective_directory();
        Ok(removed)
    }

    /// Replaces buffer lines and adjusts every position-bearing subsystem.
    ///
    /// # Errors
    ///
    pub fn replace_buffer_lines(
        &mut self,
        request: LineReplaceRequest<'_>,
    ) -> Result<u64, EditorError> {
        let LineReplaceRequest {
            buffer,
            start,
            end,
            lines,
            cursor_before,
            cursor_after,
            timestamp,
        } = request;
        let old_count = end.saturating_sub(start).saturating_add(1);
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let seq = state.replace_lines(start, end, lines, cursor_before, cursor_after, timestamp)?;
        self.splice_positions(buffer, start, old_count, lines.len());
        self.changelists.push(buffer, cursor_after);
        Ok(seq)
    }

    /// Appends buffer lines and adjusts every position-bearing subsystem.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::UnknownBuffer`] when the buffer is not live, or
    /// [`EditorError::Buffer`] when the insertion is rejected by the buffer
    /// state.
    pub fn append_buffer_lines(
        &mut self,
        buffer: BufHandle,
        after: usize,
        lines: &[Vec<u8>],
        cursor: Position,
        timestamp: i64,
    ) -> Result<u64, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let seq = state.append_lines(after, lines, cursor, timestamp)?;
        let start = after.saturating_add(1);
        self.splice_positions(buffer, start, 0, lines.len());
        self.changelists.push(
            buffer,
            Position {
                lnum: cursor.lnum.saturating_add(lines.len()),
                col: cursor.col,
            },
        );
        Ok(seq)
    }

    /// Undoes a buffer's most recent change, replaying its inverse through
    /// every position-bearing subsystem (marks, jump/change history, window
    /// cursors), matching the direct-mutation pipeline. Returns the undone
    /// header's sequence, or `None` at the oldest change.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::UnknownBuffer`] when the buffer is not live, or
    /// [`EditorError::Buffer`] when replaying the undone block fails.
    pub fn buffer_undo(&mut self, buffer: BufHandle) -> Result<Option<u64>, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let Some(replayed) = state.undo()? else {
            return Ok(None);
        };
        let seq = replayed.first().map(|edit| edit.seq);
        self.finish_replay(buffer, &replayed);
        Ok(seq)
    }

    /// Redoes a buffer's next change, replaying its stored edit through every
    /// position-bearing subsystem. Returns the redone header's sequence, or
    /// `None` at the newest change.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::UnknownBuffer`] when the buffer is not live, or
    /// [`EditorError::Buffer`] when replaying the redone block fails.
    pub fn buffer_redo(&mut self, buffer: BufHandle) -> Result<Option<u64>, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let Some(replayed) = state.redo()? else {
            return Ok(None);
        };
        let seq = replayed.first().map(|edit| edit.seq);
        self.finish_replay(buffer, &replayed);
        Ok(seq)
    }

    /// Navigates a buffer's undo tree to sequence `seq`, replaying every step
    /// through the position-bearing subsystems.
    ///
    /// The target may be behind or ahead of the current state, and on another
    /// branch, which is what `:undo {N}` needs (`undo_time`, `undo.c:1975`).
    /// An unknown sequence is reported, not silently clamped.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::UnknownBuffer`] when the buffer is not live, or
    /// [`EditorError::Buffer`] when `seq` is unknown or a replay step fails.
    pub fn buffer_undo_to_seq(
        &mut self,
        buffer: BufHandle,
        seq: u64,
    ) -> Result<usize, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let replayed = state.undo_to_seq(seq)?;
        let count = replayed.len();
        for block in replayed {
            self.finish_replay(buffer, &block);
        }
        Ok(count)
    }

    /// Returns a buffer's current undo sequence, upstream's `b_u_seq_cur`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::UnknownBuffer`] when the buffer is not live.
    pub fn buffer_undo_seq(&self, buffer: BufHandle) -> Result<u64, EditorError> {
        Ok(self
            .buffers
            .get(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?
            .undo
            .current_seq())
    }

    /// Closes a buffer's open undo block, so the next edit starts a new one.
    ///
    /// This is upstream's `u_sync` (`undo.c:2704-2717`) and the only way to
    /// move an undo-block boundary from outside `BufferState`. An unknown or
    /// unloaded buffer has no block to close, so it is a no-op rather than an
    /// error: upstream's `u_sync` likewise has nothing to do when the buffer
    /// carries no entries.
    pub fn sync_buffer_undo(&mut self, buffer: BufHandle) {
        if let Some(state) = self.buffers.get_mut(&buffer) {
            state.sync_undo();
        }
    }

    /// Closes the current buffer's open undo block.
    pub fn sync_current_undo(&mut self) {
        if let Some(buffer) = self.current_buffer() {
            self.sync_buffer_undo(buffer);
        }
    }

    /// Reopens a buffer's newest undo block so the next edit joins it
    /// (`:undojoin`, `undo.c:2800-2816`).
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, or [`EditorError::Buffer`] when the buffer state rejects the
    /// join.
    pub fn buffer_undojoin(&mut self, buffer: BufHandle) -> Result<(), EditorError> {
        self.buffer_mut(buffer)?.undojoin()?;
        Ok(())
    }

    /// Returns a buffer's undo tree for reads that need the whole shape,
    /// which is what `undotree()` reports.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::UnknownBuffer`] when the buffer is not live.
    pub fn buffer_undo_tree(&self, buffer: BufHandle) -> Result<&UndoTree, EditorError> {
        Ok(&self
            .buffers
            .get(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?
            .undo)
    }

    /// Opens one containing fold, corresponding to `zo`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, or [`EditorError::Fold`] when the fold operation fails.
    pub fn fold_open(
        &mut self,
        buffer: BufHandle,
        position: FoldPosition,
    ) -> Result<bool, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.open(position)?)
    }

    /// Closes one visible containing fold, corresponding to `zc`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, or [`EditorError::Fold`] when the fold operation fails.
    pub fn fold_close(
        &mut self,
        buffer: BufHandle,
        position: FoldPosition,
    ) -> Result<bool, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.close(position)?)
    }

    /// Toggles one containing fold, corresponding to `za`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, or [`EditorError::Fold`] when the fold operation fails.
    pub fn fold_toggle(
        &mut self,
        buffer: BufHandle,
        position: FoldPosition,
    ) -> Result<bool, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.toggle(position)?)
    }

    /// Opens a containing fold and descendants, corresponding to `zO`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, or [`EditorError::Fold`] when the fold operation fails.
    pub fn fold_open_recursive(
        &mut self,
        buffer: BufHandle,
        position: FoldPosition,
    ) -> Result<usize, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.open_recursive(position)?)
    }

    /// Closes the outer containing fold, corresponding to `zC`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, or [`EditorError::Fold`] when the fold operation fails.
    pub fn fold_close_recursive(
        &mut self,
        buffer: BufHandle,
        position: FoldPosition,
    ) -> Result<usize, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.close_recursive(position)?)
    }

    /// Opens every fold in a buffer, corresponding to `zR`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved.
    pub fn fold_open_all(&mut self, buffer: BufHandle) -> Result<usize, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.open_all())
    }

    /// Closes every fold in a buffer, corresponding to `zM`.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved.
    pub fn fold_close_all(&mut self, buffer: BufHandle) -> Result<usize, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.close_all())
    }

    /// Adjusts the editor-wide position-bearing subsystems for one replayed
    /// undo block: every edit splices, and the block leaves one changelist
    /// entry, matching the one entry a recorded block leaves.
    fn finish_replay(&mut self, buffer: BufHandle, replayed: &[crate::buffer::ReplayedEdit]) {
        for edit in replayed {
            self.splice_positions(buffer, edit.start, edit.old_count, edit.new_count);
        }
        if let Some(last) = replayed.last() {
            self.changelists.push(buffer, last.cursor);
        }
    }

    /// Puts a stored register through the buffer mutation pipeline.
    ///
    /// Returns false when the selected register has no retained content.
    pub(crate) fn put_register(
        &mut self,
        window: WinHandle,
        name: char,
        direction: PutDirection,
        count: usize,
        timestamp: i64,
    ) -> Result<bool, EditorError> {
        let window = self.resolve_window_handle(window)?;
        let state = self.window(window)?;
        let buffer = state.buffer;
        let mut cursor = state.cursor;
        let Some(content) = self.registers.get(name)?.cloned() else {
            return Ok(false);
        };
        let kind = content.kind();
        let text = self.buffer(buffer)?.text()?;
        let line_count = text.line_count();
        cursor.lnum = cursor.lnum.clamp(1, line_count.max(1));
        // Fetch only the rows the put indexes: the cursor row for every
        // non-linewise put, plus the rows below it that a blockwise put
        // splices into (one per register row).
        let target_lines = match kind {
            RegisterKind::LineWise => Vec::new(),
            RegisterKind::CharacterWise => buffer_lines_between(text, cursor.lnum, cursor.lnum)?,
            RegisterKind::BlockWise { .. } => {
                let last = cursor
                    .lnum
                    .saturating_add(content.lines().len().saturating_sub(1));
                buffer_lines_between(text, cursor.lnum, line_count.min(last))?
            }
        };
        let origin_line = target_lines.first().map_or(&[][..], Vec::as_slice);
        let origin = put_origin(origin_line, line_count, cursor, kind, direction);
        let plan = plan_put(
            &target_lines,
            line_count,
            origin,
            &content,
            count.max(1),
            cursor,
        )?;
        self.commit_put_plan(buffer, Some(window), plan, timestamp)?;
        Ok(true)
    }

    /// Inserts explicit content through the same undo-aware pipeline as a register put.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::NoCurrentTabpage`] or
    /// [`EditorError::UnknownBuffer`] when the buffer handle cannot be
    /// resolved, [`EditorError::Register`] when the content cannot be planned,
    /// [`EditorError::Buffer`] when buffer text cannot be read or committed,
    /// or [`EditorError::Layout`] when window state cannot be updated.
    pub fn put_content(
        &mut self,
        buffer: BufHandle,
        position: Position,
        content: &crate::register::RegisterContent,
        timestamp: i64,
    ) -> Result<(), EditorError> {
        let text = self.buffer(buffer)?.text()?;
        let line_count = text.line_count();
        // Only a blockwise put reads target rows: one per register row
        // starting at `position.lnum`; rows past end-of-buffer stay unread
        // and land in the end-of-buffer tail, as before.
        let target_lines = if matches!(content.kind(), RegisterKind::BlockWise { .. })
            && position.lnum >= 1
            && position.lnum <= line_count
        {
            let last = position
                .lnum
                .saturating_add(content.lines().len().saturating_sub(1));
            buffer_lines_between(text, position.lnum, line_count.min(last))?
        } else {
            Vec::new()
        };
        let plan = plan_put(&target_lines, line_count, position, content, 1, position)?;
        self.commit_put_plan(buffer, None, plan, timestamp)?;
        Ok(())
    }

    fn commit_put_plan(
        &mut self,
        buffer: BufHandle,
        window: Option<WinHandle>,
        plan: PutPlan,
        timestamp: i64,
    ) -> Result<bool, EditorError> {
        let PutPlan {
            edits,
            cursor_before,
            cursor_after,
        } = plan;
        if edits.is_empty() {
            return Ok(false);
        }

        let buffer = self.resolve_buffer_handle(buffer)?;
        let opens_active_edit =
            self.edit_mode == BufferEditMode::Insert && self.current_buffer() == Some(buffer);
        self.buffer(buffer)?.text()?;
        let window = if let Some(window) = window {
            let window = self.resolve_window_handle(window)?;
            self.window(window)?;
            Some(window)
        } else {
            None
        };
        let mut prepared = Vec::with_capacity(edits.len());
        let mut trailing_insert = None;
        for edit in edits {
            match edit {
                PutEdit::Splice(request) => {
                    prepared.push(self.buffer(buffer)?.prepare_buffer_text_edit(&request)?);
                }
                PutEdit::InsertLines { after_lnum, lines } => {
                    debug_assert!(trailing_insert.is_none());
                    trailing_insert = Some((after_lnum, lines));
                }
            }
        }

        let line_preserving = prepared
            .iter()
            .all(super::buffer::PreparedBufferTextEdit::preserves_line_count);
        let splices: Vec<TextSplice> = prepared.iter().map(|edit| edit.splice).collect();
        let inserted_lines = trailing_insert
            .as_ref()
            .map(|(after_lnum, lines)| (*after_lnum, lines.len()));
        {
            let state = self
                .buffers
                .get_mut(&buffer)
                .ok_or(EditorError::UnknownBuffer(buffer))?;
            if line_preserving {
                if !prepared.is_empty() {
                    state.commit_prepared_line_preserving_batch(
                        prepared,
                        cursor_before,
                        cursor_after,
                        timestamp,
                    )?;
                }
            } else {
                for edit in prepared {
                    state.commit_buffer_text_edit(edit, cursor_before, cursor_after, timestamp)?;
                }
            }
            if let Some((after_lnum, lines)) = trailing_insert {
                state.insert_lines(after_lnum, &lines, cursor_before, cursor_after, timestamp)?;
            }
        }

        for splice in splices {
            self.splice_text_positions(buffer, splice);
        }
        if let Some((after_lnum, line_count)) = inserted_lines {
            self.splice_positions(buffer, after_lnum + 1, 0, line_count);
        }
        self.changelists.push(buffer, cursor_after);
        if opens_active_edit {
            self.active_text_edit = Some(buffer);
        }
        if let Some(window) = window {
            let tab = self
                .windows
                .get(&window)
                .copied()
                .ok_or(EditorError::UnknownWindow(window))?;
            let tabpage = self
                .tabpages
                .get_mut(&tab)
                .ok_or(EditorError::UnknownTabpage(tab))?;
            tabpage.window_mut(window)?.cursor = cursor_after;
        }
        Ok(true)
    }

    fn split_window(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
        direction: SplitDirection,
        enter: bool,
    ) -> Result<WinHandle, EditorError> {
        let tab = self.resolve_tabpage_handle(tab)?;
        let target = self.resolve_window_handle(target)?;
        let buffer = self.resolve_buffer_handle(buffer)?;
        self.require_buffer(buffer)?;
        self.require_tabpage(tab)?;
        // Splitting a floating window splits the last non-floating window
        // instead (window.c:1151-1154: "can't split float, use last
        // nonfloating window instead"). `Layout::windows` lists only tiled
        // windows — floats live in `TabpageState::floats`, outside the frame
        // tree — so its last entry is always non-floating; this must not
        // become `TabpageState::windows`, which appends floats in z-order
        // and can re-select a float.
        let target = {
            let tabpage = self
                .tabpages
                .get(&tab)
                .ok_or(EditorError::UnknownTabpage(tab))?;
            let floating = tabpage.window_config(target)?.is_some();
            if floating {
                tabpage
                    .layout()
                    .windows()
                    .into_iter()
                    .last()
                    .unwrap_or(target)
            } else {
                target
            }
        };
        let previous = self.current_window();
        let (old_topline, old_cursor, old_height, old_buffer, local_directory, previous_directory) = {
            let tabpage = self
                .tabpages
                .get(&tab)
                .ok_or(EditorError::UnknownTabpage(tab))?;
            let state = tabpage.window(target)?;
            (
                state.topline,
                state.cursor,
                tabpage.tiled_window_text_height(target)?,
                state.buffer,
                state.local_directory.clone(),
                state.previous_directory.clone(),
            )
        };
        let window = allocate_window_handle(&mut self.next_window)?;
        // When the split shows a different buffer (`:new`, `:vnew`, or a file
        // argument), the cursor starts at line 1 — matching upstream
        // `win_enter_ext` which resets `w_cursor` to `{1, 0}` for a buffer
        // that has never been displayed. When the same buffer is split
        // (`:split`), the cursor and topline are inherited so both panes
        // show the same view.
        let cursor = if buffer == old_buffer {
            old_cursor
        } else {
            Position { lnum: 1, col: 0 }
        };
        let mut state = WindowState::new(buffer, cursor);
        state.local_directory = local_directory;
        state.previous_directory = previous_directory;
        if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
            buffer_state.attach()?;
        }
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        let inserted = match direction {
            SplitDirection::Right => tabpage.split_vertical(target, window, state, enter),
            SplitDirection::Below => tabpage.split_horizontal(target, window, state, enter),
            SplitDirection::Left => tabpage.split_left(target, window, state, enter),
            SplitDirection::Above => tabpage.split_above(target, window, state, enter),
        };
        if let Err(error) = inserted {
            if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
                buffer_state.detach(true);
            }
            return Err(error.into());
        }
        let new_height = tabpage.tiled_window_text_height(target)?;
        let old_row = old_cursor
            .lnum
            .saturating_sub(old_topline)
            .min(old_height.saturating_sub(1));
        let new_row = old_row.saturating_mul(new_height) / old_height;
        let fraction_topline = old_cursor.lnum.saturating_sub(new_row).max(1);
        let target_state = tabpage.window_mut(target)?;
        target_state.topline =
            cursor_visible_topline(fraction_topline, target_state.cursor.lnum, new_height);
        if enter {
            self.previous_window = previous.filter(|current| *current != window);
        }
        self.windows.insert(window, tab);
        self.apply_effective_directory();
        Ok(window)
    }

    fn splice_positions(
        &mut self,
        buffer: BufHandle,
        start: usize,
        old_count: usize,
        new_count: usize,
    ) {
        self.global_marks
            .splice_buffer(buffer, start, old_count, new_count);
        self.jumplist
            .splice_buffer(buffer, start, old_count, new_count);
        self.changelists
            .splice_buffer(buffer, start, old_count, new_count);
        let buffer_text = self
            .buffers
            .get(&buffer)
            .and_then(|state| state.text().ok());
        let line_count = buffer_text.map_or(1, Buffer::line_count).max(1);
        let old_end = start.saturating_add(old_count);
        let windows = &self.windows;
        let tabpages = &mut self.tabpages;
        for (&window, &tab) in windows {
            let Some(tabpage) = tabpages.get_mut(&tab) else {
                continue;
            };
            let height = viewport_height(tabpage, window);
            let Ok(state) = tabpage.window_mut(window) else {
                continue;
            };
            if state.buffer != buffer {
                continue;
            }
            let replaced = (start..old_end).contains(&state.cursor.lnum);
            splice_position(&mut state.cursor, start, old_count, new_count, line_count);
            if replaced
                && let Some(text) = buffer_text
                && let Ok(line) = text.line(state.cursor.lnum)
            {
                state.cursor.col = state.cursor.col.min(line.len());
            }
            state.topline = splice_topline(ToplineSplice {
                topline: state.topline,
                cursor: state.cursor.lnum,
                height,
                line_count,
                start,
                old_count,
                new_count,
                kind: ToplineSpliceKind::Lines,
            });
        }
    }

    /// Column-aware position splice for byte-level text edits.
    ///
    /// Like [`Self::splice_positions`] forks, jumplist, changelists, and
    /// toplines, but also adjusts cursor columns for windows showing the
    /// edited buffer, matching `mark_col_adjust` (`mark.c`).
    fn splice_text_positions(&mut self, buffer: BufHandle, splice: TextSplice) {
        let start = splice.start.row + 1;
        let old_count = splice.old_extent.rows + 1;
        let new_count = splice.new_extent.rows + 1;
        self.global_marks
            .splice_buffer(buffer, start, old_count, new_count);
        self.jumplist
            .splice_buffer(buffer, start, old_count, new_count);
        self.changelists
            .splice_buffer(buffer, start, old_count, new_count);

        let current_window = self.current_window();
        let insert_current =
            self.edit_mode == BufferEditMode::Insert && self.current_buffer() == Some(buffer);

        let buffer_text = self
            .buffers
            .get(&buffer)
            .and_then(|state| state.text().ok());
        let line_count = buffer_text.map_or(1, Buffer::line_count).max(1);
        // Byte length of the replacement's final row, which upstream reads
        // with `ml_get_buf_len` when collapsing rows the edit removed.
        let new_end_row_len = buffer_text
            .and_then(|text| text.line(splice.new_end().row.saturating_add(1)).ok())
            .map_or(0, |line| line.len());
        let windows = &self.windows;
        let tabpages = &mut self.tabpages;
        for (&window, &tab) in windows {
            let Some(tabpage) = tabpages.get_mut(&tab) else {
                continue;
            };
            let height = viewport_height(tabpage, window);
            let Ok(state) = tabpage.window_mut(window) else {
                continue;
            };
            if state.buffer != buffer {
                continue;
            }

            let is_current = current_window == Some(window);
            let coladd = usize::try_from(state.coladd).unwrap_or(0);
            let cursor =
                ExtmarkPosition::new(state.cursor.lnum.saturating_sub(1), state.cursor.col);
            let (adjusted, adjusted_coladd) = adjust_text_cursor(
                cursor,
                coladd,
                splice,
                new_end_row_len,
                insert_current && is_current,
            );
            state.cursor.lnum = adjusted.row.saturating_add(1);
            state.cursor.col = adjusted.column;
            state.coladd = i64::try_from(adjusted_coladd).unwrap_or(0);

            // `check_cursor_col` (`cursor.c:327`): the transform leaves one
            // virtual column, which the new line clamps. Normal mode stops a
            // byte short of the end; INSERT mode and a cursor that was
            // already past end-of-line may sit on it, and the cells beyond
            // stay in `coladd` so the screen column survives.
            let virtual_edit = coladd > 0;
            if let Some(line) = buffer_text.and_then(|text| text.line(state.cursor.lnum).ok()) {
                let virtual_column = adjusted.column.saturating_add(adjusted_coladd);
                let len = line.len();
                if len == 0 {
                    state.cursor.col = 0;
                } else if adjusted.column >= len {
                    state.cursor.col = if (insert_current && is_current) || virtual_edit {
                        len
                    } else {
                        len - 1
                    };
                }
                if virtual_edit {
                    state.coladd =
                        i64::try_from(virtual_column.saturating_sub(state.cursor.col)).unwrap_or(0);
                }
            }

            state.topline = splice_topline(ToplineSplice {
                topline: state.topline,
                cursor: state.cursor.lnum,
                height,
                line_count,
                start,
                old_count,
                new_count,
                kind: ToplineSpliceKind::Text,
            });
        }
    }

    fn require_buffer(&self, buffer: BufHandle) -> Result<(), EditorError> {
        if self.buffers.contains_key(&buffer) {
            Ok(())
        } else {
            Err(EditorError::UnknownBuffer(buffer))
        }
    }

    fn resolve_buffer_handle(&self, buffer: BufHandle) -> Result<BufHandle, EditorError> {
        if buffer.is_current() {
            self.current_buffer().ok_or(EditorError::NoCurrentTabpage)
        } else {
            Ok(buffer)
        }
    }

    fn resolve_window_handle(&self, window: WinHandle) -> Result<WinHandle, EditorError> {
        if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)
        } else {
            Ok(window)
        }
    }

    fn resolve_tabpage_handle(&self, tab: TabHandle) -> Result<TabHandle, EditorError> {
        if tab.is_current() {
            self.current_tab.ok_or(EditorError::NoCurrentTabpage)
        } else {
            Ok(tab)
        }
    }

    fn require_tabpage(&self, tab: TabHandle) -> Result<(), EditorError> {
        if self.tabpages.contains_key(&tab) {
            Ok(())
        } else {
            Err(EditorError::UnknownTabpage(tab))
        }
    }
}

fn wrapped_line_rows(cells: usize, width: usize) -> usize {
    cells.div_ceil(width).max(1)
}

/// Renderable text rows for a window's viewport.
///
/// Tiled windows reserve a statusline row and the message row from their
/// layout frame (`tiled_window_text_height`), matching upstream
/// `w_winrow_height` minus `--statusline`/`-2` reservations; floating frames
/// already exclude their borders. Falls back to the frame height when the
/// window has no geometry.
fn viewport_height(tabpage: &TabpageState, window: WinHandle) -> usize {
    tabpage
        .tiled_window_text_height(window)
        .or_else(|_| {
            tabpage
                .window_geometry(window)
                .map(|geometry| geometry.height)
        })
        .unwrap_or(1)
        .max(1)
}
/// Projects the emulator's damaged rows into a terminal buffer.
///
/// Each row replaces its buffer line in place, growing the buffer when the
/// emulator has more rows than the buffer holds; runs written with a
/// non-default pen become extmarks in a dedicated namespace carrying the
/// pen, which the compositor resolves to a synthesized highlight group
/// (`hl_get_term_attr`, `terminal.c:1432-1442`).
///
/// # Errors
///
/// Returns buffer text mutation and extmark store failures.
fn project_terminal_rows(
    state: &mut crate::BufferState,
    first_line: usize,
    rows: &[crate::terminal_screen::RenderedRow],
) -> Result<(), EditorError> {
    let namespace = state.extmarks.create_namespace("terminal-screen")?;
    let last_line = first_line + rows.len().saturating_sub(1);
    let focus = ox_text::Position { lnum: 1, col: 0 };
    let line_count = state.text()?.line_count();
    let covered = (last_line.min(line_count) + 1).saturating_sub(first_line);
    if covered > 0 {
        let texts: Vec<Vec<u8>> = rows[..covered].iter().map(|row| row.text.clone()).collect();
        state.replace_lines(
            first_line,
            first_line + covered - 1,
            &texts,
            focus,
            focus,
            0,
        )?;
    }
    if covered < rows.len() {
        let texts: Vec<Vec<u8>> = rows[covered..].iter().map(|row| row.text.clone()).collect();
        state.append_lines(line_count, &texts, focus, 0)?;
    }
    // The previous generation's marks on the rewritten span are removed so a
    // repaint cannot stack them; the column bound only has to order past the
    // span ends.
    state.extmarks.clear(
        namespace,
        crate::ExtmarkPosition::new(first_line - 1, 0),
        crate::ExtmarkPosition::new(last_line, usize::MAX),
    )?;
    for (offset, row) in rows.iter().enumerate() {
        let line = first_line - 1 + offset;
        for span in &row.spans {
            if span.attrs.is_default() {
                continue;
            }
            let mut placement =
                crate::ExtmarkPlacement::new(crate::ExtmarkPosition::new(line, span.start));
            placement.end = Some(crate::ExtmarkEnd::new(crate::ExtmarkPosition::new(
                line, span.end,
            )));
            placement.attributes.terminal_pen = Some(span.attrs);
            state.extmarks.set(namespace, None, placement)?;
        }
    }
    Ok(())
}

fn cursor_visible_topline(topline: usize, cursor: usize, height: usize) -> usize {
    let topline = topline.max(1);
    if cursor < topline {
        cursor.max(1)
    } else if cursor >= topline.saturating_add(height) {
        cursor.saturating_sub(height.saturating_sub(1)).max(1)
    } else {
        topline
    }
}

fn splice_position(
    position: &mut Position,
    start: usize,
    old_count: usize,
    new_count: usize,
    line_count: usize,
) {
    let old_end = start.saturating_add(old_count);
    if position.lnum < start {
        return;
    }
    if position.lnum >= old_end {
        position.lnum = if new_count >= old_count {
            position.lnum.saturating_add(new_count - old_count)
        } else {
            position.lnum.saturating_sub(old_count - new_count).max(1)
        };
        return;
    }
    if old_count > new_count {
        position.lnum = position.lnum.min(line_count).max(1);
    }
}

/// Whether a topline splice came from exclusive-end line edits or inclusive-end
/// text edits. The two APIs feed `mark_adjust_buf` different `line2` values.
#[derive(Clone, Copy)]
enum ToplineSpliceKind {
    /// `nvim_buf_set_lines`: exclusive end, `line2 = start + old_count - 1`.
    Lines,
    /// `nvim_buf_set_text`: inclusive end, `line2 = start + old_count - 2`.
    Text,
}

/// Viewport and splice geometry for [`splice_topline`].
#[derive(Clone, Copy)]
struct ToplineSplice {
    topline: usize,
    cursor: usize,
    height: usize,
    line_count: usize,
    start: usize,
    old_count: usize,
    new_count: usize,
    kind: ToplineSpliceKind,
}

/// Upstream topline policy for a row splice, mirroring Neovim's two-phase
/// adjustment: `mark_adjust_buf` with `kMarkAdjustApi` (`mark.c`) followed by
/// `fix_cursor` + `update_topline` (`api/buffer.c`).
///
/// Phase 1 — `mark_adjust_buf`: the topline is treated as a mark in the range
/// `[line1, line2]`. For line splices (`set_lines`), `line2` is
/// `start + old_count - 1` and a topline in-range is left alone when the splice
/// replaces (at least one new line) and clamped to `start - 1` on pure deletion.
/// For text splices (`set_text`), `line2` is `start + old_count - 2` (the
/// inclusive end row minus one, because `mark_adjust_buf` receives
/// `end_row - 1`) and the replacement threshold is `new_count > 1` — a single
/// replacement line is still a deletion for topline purposes. A topline
/// strictly after `line2` (plus a one-row correction when `line2 < line1`,
/// i.e. pure insertion) shifts by the unsigned row delta between `new_count`
/// and `old_count`. A topline exactly at an insertion point is left alone so
/// the new line is displayed.
///
/// Phase 2 — `update_topline`: the already-adjusted cursor is kept visible by
/// scrolling the topline up or down within the window height, but only when
/// the cursor was at or below the splice start. When the cursor was above the
/// splice (adjusted cursor < `start`), phase 2 is skipped, matching
/// `fix_cursor`'s `else` branch that merely invalidates the botline.
fn splice_topline(splice: ToplineSplice) -> usize {
    let ToplineSplice {
        topline,
        cursor,
        height,
        line_count,
        start,
        old_count,
        new_count,
        kind,
    } = splice;
    let old_end = start.saturating_add(old_count);
    // `mark_adjust_buf` receives `line2 = end - 1` for `set_lines` (exclusive
    // end) and `line2 = end_row - 1` for `set_text` (inclusive end). In Oxvim
    // terms: line splice → `start + old_count - 1`; text splice →
    // `start + old_count - 2`.
    let line2_offset = match kind {
        ToplineSpliceKind::Lines => 1,
        ToplineSpliceKind::Text => 2,
    };
    let line2 = old_end.saturating_sub(line2_offset);
    // Replacement threshold: `amount_after > line1 - line2 - 1`, which
    // simplifies to `new_count > 0` for line splices and `new_count > 1` for
    // text splices.
    let replacement_threshold = line2_offset - 1;

    // Phase 1: mark_adjust_buf topline logic.
    let mut mapped = if topline >= start && topline <= line2 {
        if old_count > 0 {
            // topline is inside the deleted/replaced range.
            if new_count > replacement_threshold {
                // Replacement: leave topline for update_topline (phase 2).
                topline
            } else {
                // Pure deletion (or net deletion in text splice): fall back
                // to the row above the splice.
                start.saturating_sub(1).max(1)
            }
        } else {
            // old_count == 0 → line2 < start, so this branch is unreachable.
            topline
        }
    } else {
        // topline is after the range; shift by the unsigned row delta when
        // strictly past line2, with a one-row correction for pure insertion
        // (`line2 < start`). A zero delta is a no-op add.
        let insertion_correction = usize::from(line2 < start);
        if topline > line2.saturating_add(insertion_correction) {
            if new_count >= old_count {
                topline.saturating_add(new_count - old_count)
            } else {
                topline.saturating_sub(old_count - new_count).max(1)
            }
        } else {
            topline
        }
    };

    // Phase 2: update_topline — keep the cursor visible, but only when the
    // cursor was at or below the splice start (fix_cursor's `cursor >= lo`
    // branch). When the cursor was above the splice, the adjusted cursor is
    // still < start, and update_topline is not called.
    mapped = mapped.min(line_count).max(1);
    if cursor < start {
        mapped
    } else if cursor < mapped {
        cursor.max(1)
    } else if cursor >= mapped.saturating_add(height) {
        cursor.saturating_sub(height.saturating_sub(1)).max(1)
    } else {
        mapped
    }
}

/// Adjusts a 0-based `(row, col)` cursor plus `coladd` for a byte-level text
/// replacement between `splice.start` and `splice.old_end()`, mirroring
/// `fix_pos_col` (`api/buffer.c:1304`). Returns the mapped position and its
/// residual virtual cells; the caller clamps the column against the new line
/// (`check_cursor_col`, `cursor.c:327`).
///
/// Cursors before the range stay put. Cursors past it shift rows and, on the
/// old end row only, columns by the replacement delta, so a cursor beyond
/// end-of-line keeps its screen position. Inside the range a cursor moves up
/// to at most the replacement end; only a NORMAL-mode cursor that was not
/// already past end-of-line lands exactly on the replacement end. When
/// `insert_current` (INSERT mode in the current window) a cursor at the range
/// start is treated as between characters, matching `mark_col_adjust`
/// skipping `restart_edit` cursors.
fn adjust_text_cursor(
    cursor: ExtmarkPosition,
    coladd: usize,
    splice: TextSplice,
    new_end_row_len: usize,
    insert_current: bool,
) -> (ExtmarkPosition, usize) {
    let mut row = cursor.row;
    let col = cursor.column;
    let start_row = splice.start.row;
    let start_col = splice.start.column;
    let old_end = splice.old_end();
    let end_row = old_end.row;
    let end_col = old_end.column;
    let new_rows = splice.new_extent.rows.saturating_add(1);
    let new_cols = splice.new_extent.columns;

    // Before the edit: no change.
    if row < start_row {
        return (cursor, coladd);
    }

    let old_rows = end_row.saturating_sub(start_row).saturating_add(1);

    // After the edit: shift rows, keep the column.
    if row > end_row {
        let new_row = if new_rows >= old_rows {
            row.saturating_add(new_rows - old_rows)
        } else {
            row.saturating_sub(old_rows - new_rows)
        };
        return (ExtmarkPosition::new(new_row, col), coladd);
    }

    let change_start = if new_rows == 1 { start_col } else { 0 };
    let change_end = change_start + new_cols;

    // After the replaced range on the end row: keep the distance to the range
    // end, so columns past end-of-line survive as byte columns.
    if row == end_row && col + usize::from(!insert_current) > end_col {
        let column = if change_end >= end_col {
            col + (change_end - end_col)
        } else {
            col - (end_col - change_end).min(col)
        };
        let new_row = if new_rows >= old_rows {
            row.saturating_add(new_rows - old_rows)
        } else {
            row.saturating_sub(old_rows - new_rows)
        };
        return (ExtmarkPosition::new(new_row, column), coladd);
    }

    // Inside the replaced range: collapse toward the replacement end.
    let old_coladd = coladd;
    let mut column = col + coladd;
    let new_end_row = start_row + new_rows.saturating_sub(1);
    if row > new_end_row {
        row = new_end_row;
        column = column.max(new_end_row_len);
    }
    if row == new_end_row && column > change_end && old_coladd == 0 {
        column = change_end;
        if column >= change_start + usize::from(!insert_current) {
            column -= usize::from(!insert_current);
        }
    }
    (ExtmarkPosition::new(row, column), 0)
}

fn buffer_lines_between(
    buffer: &Buffer,
    first: usize,
    last: usize,
) -> Result<Vec<Vec<u8>>, BufferStateError> {
    (first..=last)
        .map(|lnum| buffer.line(lnum))
        .collect::<Result<Vec<_>, _>>()
        .map_err(BufferStateError::from)
}

/// Whether this `buftype` value means a buffer is never read from its name
/// (`bt_nofileread`, `buffer.c:4071-4077`): `nofile`, `terminal`,
/// `quickfix`, and `prompt` buffers materialize empty text.
pub(crate) fn is_nofileread(buftype: &str) -> bool {
    matches!(
        buftype.as_bytes(),
        [b'n', _, b'f', ..] | [b't' | b'q' | b'p', ..]
    )
}

#[derive(Clone, Copy)]
enum SplitDirection {
    Left,
    Right,
    Above,
    Below,
}

fn allocate_buffer_handle(next: &mut i64) -> Result<BufHandle, EditorError> {
    let value = *next;
    let handle = BufHandle::try_from(value).map_err(|_| EditorError::HandleExhausted("buffer"))?;
    *next = next
        .checked_add(1)
        .ok_or(EditorError::HandleExhausted("buffer"))?;
    Ok(handle)
}

fn allocate_window_handle(next: &mut i64) -> Result<WinHandle, EditorError> {
    let value = *next;
    let handle = WinHandle::try_from(value).map_err(|_| EditorError::HandleExhausted("window"))?;
    *next = next
        .checked_add(1)
        .ok_or(EditorError::HandleExhausted("window"))?;
    Ok(handle)
}

fn allocate_tab_handle(next: &mut i64) -> Result<TabHandle, EditorError> {
    let value = *next;
    let handle = TabHandle::try_from(value).map_err(|_| EditorError::HandleExhausted("tabpage"))?;
    *next = next
        .checked_add(1)
        .ok_or(EditorError::HandleExhausted("tabpage"))?;
    Ok(handle)
}

/// Expands a buffer name to a full path, matching upstream `fname_expand`
/// (buffer.c:3621-3652).  Relative names are joined with the process cwd.
/// If the resolved path is a directory, the trailing separator is preserved
/// and the symlink spelling is kept (the path is NOT canonicalized).
#[must_use]
pub fn expand_buffer_name(name: &OxStr) -> OxStr {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return name.clone();
    }
    // Already absolute — use as-is.
    let path_str = std::str::from_utf8(bytes).unwrap_or("");
    let path = std::path::Path::new(path_str);
    if path.is_absolute() {
        return name.clone();
    }
    // Join with cwd (upstream `fix_fname` calls `FullName` / `os_full_name`).
    let Ok(cwd) = std::env::current_dir() else {
        return name.clone();
    };
    let full = cwd.join(path);
    let full_str = full.to_string_lossy().into_owned();
    // If the full path is a directory, preserve symlink spelling and ensure
    // a trailing path separator (upstream `fname_expand` re-expands sfname
    // without the trailing slash via `fix_fname`, then re-appends it).
    // We must NOT canonicalize — `fs::canonicalize` resolves symlinks and
    // would break the "preserves symbolic link path" test.
    if std::fs::metadata(&full).is_ok_and(|metadata| metadata.is_dir()) {
        // The path may already end with '/' from the user input (e.g. "link/").
        // Strip it, re-join to get the clean absolute path, then re-append.
        let stripped = full_str.trim_end_matches('/');
        OxStr::from(format!("{stripped}/").as_str())
    } else {
        OxStr::from(full_str.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Anchor, RelativeTo};

    fn echo(text: &str) -> Message {
        Message {
            kind: MessageKind::Echo,
            content: Object::String(OxStr::from(text)),
            history: true,
            leading_newline: true,
        }
    }

    #[test]
    fn armed_echo_identity_replaces_matching_id_before_reentry() {
        let mut editor = Editor::new();
        editor.arm_echo_identity(OxStr::from("echo"), Object::Integer(9));
        editor.push_message(echo("earlier"));
        editor.arm_echo_identity(OxStr::from("progress"), Object::Integer(9));
        editor.push_message(echo("fresh"));
        assert_eq!(editor.messages().len(), 1, "the duplicate push is dropped");
        assert_eq!(
            editor.messages()[0].content,
            Object::String(OxStr::from("fresh"))
        );
        assert_eq!(
            editor.message_identities()[0],
            MessageIdentity {
                kind: OxStr::from("progress"),
                id: Object::Integer(9),
            }
        );
    }

    #[test]
    fn nested_echo_identity_stays_separate_from_outer_id() {
        let mut editor = Editor::new();
        editor.arm_echo_identity(OxStr::from("progress"), Object::Integer(9));
        editor.push_message(echo("outer"));
        editor.arm_echo_identity(OxStr::from("progress"), Object::Integer(10));
        editor.push_message(echo("nested"));
        assert_eq!(editor.messages().len(), 2);
        assert_eq!(editor.message_identities()[0].id, Object::Integer(9));
        assert_eq!(editor.message_identities()[1].id, Object::Integer(10));
    }

    #[test]
    fn stamp_echo_identity_only_updates_the_appended_slot() {
        let mut editor = Editor::new();
        editor.push_message(echo("first"));
        editor.push_message(echo("second"));
        editor.stamp_echo_identity(1, OxStr::from("echo"), Object::Integer(7));
        assert_eq!(editor.messages().len(), 2);
        assert_eq!(editor.message_identities().len(), 2);
        assert_eq!(
            editor.message_identities()[1].id,
            Object::Integer(7),
            "the pushed message carries the stamped id"
        );
        editor.truncate_messages(1);
        assert_eq!(
            editor.message_identities().len(),
            1,
            "the identity vector stays index-aligned under truncation"
        );
    }

    fn terminal_lines(editor: &Editor, channel: u64) -> Vec<Vec<u8>> {
        let info = editor.terminal_channel(channel).unwrap();
        let state = editor.buffer(info.buffer).unwrap();
        let text = state.text().unwrap();
        buffer_lines_between(text, 1, text.line_count()).unwrap()
    }

    /// Chunk boundaries never survive the emulator: the projected rows are
    /// the rendered screen, so split writes merge into one `hello` row.
    /// The trailing empty row is the cursor's own line after the final
    /// line feed, exactly where upstream's terminal buffer leaves it.
    #[test]
    fn terminal_buffer_merges_partial_line_chunks() {
        let mut editor = Editor::new();
        let channel = editor.allocate_channel_id();
        editor
            .allocate_terminal_buffer_rows(
                channel,
                None,
                crate::terminal_screen::ScreenSize::new(3, 80),
            )
            .unwrap();

        editor.append_terminal_buffer(channel, b"hel").unwrap();
        editor.append_terminal_buffer(channel, b"lo\n").unwrap();
        editor.append_terminal_buffer(channel, b"wor").unwrap();
        editor.append_terminal_buffer(channel, b"ld\n").unwrap();

        let lines = terminal_lines(&editor, channel);
        // A bare LF keeps the column (LNM off), so `world` renders at the
        // column the cursor kept; children emit \r\n to reset it.
        assert_eq!(
            lines,
            vec![b"hello".to_vec(), b"     world".to_vec(), Vec::new()]
        );
    }

    #[test]
    fn terminal_buffer_appends_trailing_newline_without_blank_line() {
        let mut editor = Editor::new();
        let channel = editor.allocate_channel_id();
        editor
            .allocate_terminal_buffer_rows(
                channel,
                None,
                crate::terminal_screen::ScreenSize::new(3, 80),
            )
            .unwrap();

        editor.append_terminal_buffer(channel, b"first\n").unwrap();
        editor.append_terminal_buffer(channel, b"second\n").unwrap();

        let lines = terminal_lines(&editor, channel);
        assert_eq!(
            lines,
            vec![b"first".to_vec(), b"     second".to_vec(), Vec::new()]
        );
    }

    /// `\r` resets the column before `\n` advances the row, so CRLF pairs
    /// project as clean rows exactly like the child terminal renders them.
    #[test]
    fn terminal_buffer_strips_carriage_returns_from_complete_lines() {
        let mut editor = Editor::new();
        let channel = editor.allocate_channel_id();
        editor
            .allocate_terminal_buffer_rows(
                channel,
                None,
                crate::terminal_screen::ScreenSize::new(3, 80),
            )
            .unwrap();

        editor
            .append_terminal_buffer(channel, b"one\r\ntwo\r\n")
            .unwrap();

        let lines = terminal_lines(&editor, channel);
        assert_eq!(lines, vec![b"one".to_vec(), b"two".to_vec(), Vec::new()]);
    }

    #[test]
    fn terminal_buffer_keeps_partial_line_visible_without_newline() {
        let mut editor = Editor::new();
        let channel = editor.allocate_channel_id();
        editor.allocate_terminal_buffer(channel).unwrap();

        editor.append_terminal_buffer(channel, b"partial").unwrap();

        let lines = terminal_lines(&editor, channel);
        assert_eq!(lines, vec![b"partial".to_vec()]);
    }

    fn cursor_float_position(
        text: &str,
        cursor_col: usize,
        width: usize,
        tabstop: Option<i64>,
    ) -> (usize, usize) {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true)
            .unwrap();
        if let Some(tabstop) = tabstop {
            editor
                .options_mut()
                .set_buffer(buffer, "tabstop", OptionValue::Number(tabstop))
                .unwrap();
        }
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, width, 8).unwrap())
            .unwrap();
        let anchor = editor.tabpage(tab).unwrap().current_window();
        editor
            .set_window_cursor(
                anchor,
                Position {
                    lnum: 1,
                    col: cursor_col,
                },
            )
            .unwrap();
        let config =
            WinConfig::new(RelativeTo::Cursor, Anchor::NorthWest, 0.0, 0.0, 1, 1).unwrap();
        let float = editor.open_float(tab, buffer, config).unwrap();
        let geometry = editor.window_geometry(float).unwrap();
        (geometry.row, geometry.col)
    }

    #[test]
    fn cursor_float_uses_rendered_ascii_column() {
        assert_eq!(cursor_float_position("abc", 2, 20, None), (0, 2));
    }

    #[test]
    fn cursor_float_uses_effective_tabstop_cells() {
        assert_eq!(cursor_float_position("a\tb", 2, 20, Some(4)), (0, 4));
    }

    #[test]
    fn cursor_float_uses_wide_character_cells() {
        assert_eq!(cursor_float_position("界x", 3, 20, None), (0, 2));
    }

    #[test]
    fn cursor_float_uses_combining_character_cells() {
        assert_eq!(cursor_float_position("e\u{301}x", 3, 20, None), (0, 1));
    }

    #[test]
    fn cursor_float_uses_wrapped_screen_row_and_column() {
        assert_eq!(cursor_float_position("abcdef", 5, 4, None), (1, 1));
    }

    #[test]
    fn set_cursor_float_uses_rendered_cursor_position() {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"a\tb").unwrap(), true)
            .unwrap();
        editor
            .options_mut()
            .set_buffer(buffer, "tabstop", OptionValue::Number(4))
            .unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 20, 8).unwrap())
            .unwrap();
        let anchor = editor.tabpage(tab).unwrap().current_window();
        editor
            .set_window_cursor(anchor, Position { lnum: 1, col: 2 })
            .unwrap();

        let initial =
            WinConfig::new(RelativeTo::Editor, Anchor::NorthWest, 0.0, 0.0, 1, 1).unwrap();
        let float = editor.open_float(tab, buffer, initial).unwrap();
        let cursor_config =
            WinConfig::new(RelativeTo::Cursor, Anchor::NorthWest, 0.0, 0.0, 1, 1).unwrap();
        editor.set_window_config(float, cursor_config).unwrap();

        let geometry = editor.window_geometry(float).unwrap();
        assert_eq!((geometry.row, geometry.col), (0, 4));
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxvim-editor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn unloaded_named_buffer(editor: &mut Editor, path: &Path) -> BufHandle {
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .buffer_mut(buffer)
            .unwrap()
            .set_name(OxStr::from(path.to_string_lossy().as_ref()));
        editor.unload_buffer(buffer).unwrap();
        buffer
    }

    /// The fallback for a floating split target must stay non-floating even
    /// when another float is last in z-order (`window.c:1151-1154`): a float
    /// selection is rejected by the tiled split and the command fails.
    #[test]
    fn splitting_a_float_with_another_float_open_splits_the_last_tiled_window() {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 20, 10).unwrap())
            .unwrap();
        let config = WinConfig::new(RelativeTo::Editor, Anchor::NorthWest, 0.0, 0.0, 3, 2).unwrap();
        let first_float = editor.open_float(tab, buffer, config.clone()).unwrap();
        let second_float = editor.open_float(tab, buffer, config).unwrap();

        let split = editor
            .split_vertical(tab, second_float, buffer, true)
            .unwrap();

        let tabpage = editor.tabpage(tab).unwrap();
        assert!(tabpage.window_config(split).unwrap().is_none());
        assert!(tabpage.window_config(first_float).unwrap().is_some());
        assert!(tabpage.window_config(second_float).unwrap().is_some());
        assert_eq!(tabpage.layout().window_count(), 2);
    }

    /// Re-entering an unloaded named buffer must read its file back, not
    /// fabricate empty saved text: the fabrication let a later `:write`
    /// replace the file with empty content.
    #[test]
    fn displaying_an_unloaded_named_buffer_reloads_its_file_content() {
        let dir = scratch_dir("reload");
        let path = dir.join("Xreload.txt");
        std::fs::write(&path, b"original\ncontent\n").unwrap();
        let mut editor = Editor::new();
        let buffer = unloaded_named_buffer(&mut editor, &path);

        let scratch = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(scratch, Geometry::new(0, 0, 20, 10).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();

        editor
            .set_window_buffer(window, buffer, BufferRelease::KeepLoaded)
            .unwrap();

        let state = editor.buffer(buffer).unwrap();
        let text = state.text().unwrap();
        assert_eq!(text.line_count(), 2);
        assert_eq!(text.line(1).unwrap(), b"original");
        assert_eq!(text.line(2).unwrap(), b"content");
        assert!(!state.flags.contains(crate::BufferFlags::MODIFIED));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A read failure that is not a missing file must surface and leave the
    /// buffer unloaded: silently attaching empty saved text is what allowed
    /// `:write` to destroy the file.
    #[test]
    fn displaying_an_unloaded_named_buffer_with_unreadable_file_stays_unloaded() {
        let dir = scratch_dir("unreadable");
        // A directory cannot be read as a file on any user, so the read
        // fails with an error that is not NotFound.
        let path = dir.join("Xunreadable");
        std::fs::create_dir(&path).unwrap();
        let mut editor = Editor::new();
        let buffer = unloaded_named_buffer(&mut editor, &path);

        let scratch = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(scratch, Geometry::new(0, 0, 20, 10).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();

        let error = editor
            .set_window_buffer(window, buffer, BufferRelease::KeepLoaded)
            .unwrap_err();
        assert!(matches!(
            error,
            EditorError::Buffer(BufferStateError::Unloaded)
        ));
        assert!(editor.buffer(buffer).unwrap().text().is_err());
        assert_eq!(editor.window(window).unwrap().buffer, scratch);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A missing file is the shared new-file semantic, not a failed load:
    /// the buffer opens empty and unmodified, like `buffer_from_file`.
    #[test]
    fn displaying_an_unloaded_named_buffer_with_missing_file_opens_it_empty() {
        let dir = scratch_dir("missing");
        let path = dir.join("Xmissing.txt");
        let mut editor = Editor::new();
        let buffer = unloaded_named_buffer(&mut editor, &path);

        let scratch = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(scratch, Geometry::new(0, 0, 20, 10).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();

        editor
            .set_window_buffer(window, buffer, BufferRelease::KeepLoaded)
            .unwrap();

        let state = editor.buffer(buffer).unwrap();
        let text = state.text().unwrap();
        assert_eq!(text.line_count(), 1);
        assert_eq!(text.line(1).unwrap(), b"");
        assert!(!state.flags.contains(crate::BufferFlags::MODIFIED));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
