//! Modal input state machine, shaped like `state.c`'s check/execute loop.

use ox_text::Position;
use ox_types::{BufHandle, WinHandle};
use thiserror::Error;

use crate::indent::{self, CinTrigger, ExprEval, IndentExprError};
use crate::insert;
use crate::motion::{FindDirection, FindMotion, resolve, resolve_find};
use crate::ops::{self, EditRange, Operator};
use crate::put::PutDirection;
use crate::register::{RegisterContent, RegisterKind};
use crate::search::{SearchDirection, SearchState};
use crate::textobject;
use crate::typeahead::{K_SPECIAL, KE_FILLER, KS_SPECIAL, KS_ZERO, Keys};
use crate::{
    BufferRelease, BufferStateError, BufferTextEditRequest, Editor, EditorError, ExtmarkPosition,
    Key, KeyDecodeError, MarkLocation, MotionKind, OperatorRequest, OptionValue, SearchError,
    VisualKind, VisualState,
};
use crate::{
    KS_EXTRA, Lookup, MapFlags, MapMode, MappingAction, MappingOptions, Remap, TypeaheadError,
    TypeaheadFlags,
};

/// Kind of command line currently being edited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CmdlineKind {
    /// Forward or backward search.
    Search(SearchDirection),
    /// Ex command entered with `:`.
    Ex,
}

/// Parser state retained between normal-mode keys.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NormalState {
    /// Count accumulated before a command.
    pub count: usize,
    /// Incomplete multi-key command prefix.
    pub prefix: String,
    /// Explicit register selected with `"`.
    pub register: Option<char>,
}
/// Insert mode has no extra retained state for the supported basics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InsertState;
/// Replace mode has no extra retained state beyond the insert session.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReplaceState;
/// Outcome of the shared `CTRL-\` second-key arm (`insert.c:640`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CtrlBslash {
    /// Exit the mode (`CTRL-N` follow key).
    Exit,
    /// Sequence consumed with no further action (`CTRL-\` itself).
    Consumed,
    /// Write the literal backslash byte, then process the follow key.
    Literal,
    /// No sequence active — ordinary input.
    None,
}
/// Search command-line state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CmdlineState {
    /// Search or Ex command-line behavior.
    pub kind: CmdlineKind,
    /// Pattern or command text entered so far.
    pub text: String,
    /// Match occurrence requested before entering command-line mode.
    pub count: usize,
}
/// State retained between an operator and its motion or text object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatorPendingState {
    /// Operator awaiting a range.
    pub operator: Operator,
    /// Count before the operator.
    pub count: usize,
    /// Whether the pre-operator count was explicitly typed.
    pub count_was_set: bool,
    /// Count after the operator.
    pub motion_count: usize,
    /// Explicit destination register.
    pub register: Option<char>,
    /// Incomplete motion or text-object prefix.
    pub prefix: String,
    /// Forced range shape selected with operator-pending `v` or `V`.
    pub force_motion: Option<MotionKind>,
    /// Search direction and expression retained while entering `/` or `?`.
    pub search: Option<(SearchDirection, String)>,
}

/// Active editor input mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Normal command mode.
    Normal(NormalState),
    /// Plain insertion mode.
    Insert(InsertState),
    /// Replace mode overwrites existing text (`gR`-shaped semantics).
    Replace(ReplaceState),
    /// Visual selection mode.
    Visual(VisualState),
    /// Search command-line mode.
    Cmdline(CmdlineState),
    /// An operator is waiting for its range.
    OperatorPending(OperatorPendingState),
}
impl Default for Mode {
    fn default() -> Self {
        Self::Normal(NormalState::default())
    }
}

/// One state-loop action produced by input checking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    /// No input is ready.
    Idle,
    /// Deferred editor events should run before more input.
    ProcessEvents,
    /// Execute one decoded character key.
    Key(char),
}

/// Failures produced by modal input execution.
#[derive(Debug, Error)]
pub enum ModeError {
    /// Core editor operation failed.
    #[error(transparent)]
    Editor(#[from] EditorError),
    /// Encoded input could not be decoded.
    #[error(transparent)]
    KeyDecode(#[from] KeyDecodeError),
    /// Mapped input could not be inserted into typeahead.
    #[error(transparent)]
    Typeahead(#[from] TypeaheadError),
    /// Search parsing or execution failed.
    #[error(transparent)]
    Search(#[from] SearchError),
    /// Operator range application failed.
    #[error(transparent)]
    Operator(#[from] ops::OperatorError),
    /// Buffer text could not be read.
    #[error(transparent)]
    Buffer(#[from] BufferStateError),
    /// Insert-mode operation failed.
    #[error(transparent)]
    Insert(#[from] insert::InsertError),
    /// Indent expression evaluation failed.
    #[error(transparent)]
    Indent(#[from] IndentExprError),
    /// A named special key has no behavior in the current mode.
    #[error("non-character input is not supported in this modal state")]
    UnsupportedKey,
    /// `'maxmapdepth'` mapping expansions happened without consuming a key.
    #[error("recursive mapping")]
    RecursiveMapping,
    /// Ident/define search failed (`E349`/`E387`/`E388`/`E389`).
    #[error("{1}")]
    Vim(&'static str, String),
}

/// Stateful modal command processor.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the modal state machine tracks independent input protocol predicates"
)]
pub struct ModeMachine {
    /// Currently active input mode.
    pub mode: Mode,
    search: SearchState,
    last_find: Option<FindMotion>,
    last_visual: Option<VisualState>,
    completed_ex_command: Option<String>,
    cmdline_history: Vec<String>,
    pending_mapping_action: Option<(MappingAction, MappingOptions)>,
    /// `mapdepth` (`getchar.c`): mapping expansions since the last key was
    /// consumed. `nmap ,x ,x` re-expands forever without it.
    map_depth: u32,
    timestamp: i64,
    /// Set while [`Self::check`] parks on a typeahead front that is only the
    /// prefix of a longer mapping (`vgetorpeek` waiting for the mapping to
    /// resolve): `nvim_get_mode` reports it as `blocking`.
    map_pending: bool,
    /// Whether the current drain is closed to further input (`:normal`,
    /// `feedkeys()` with `x`): an incomplete mapping must resolve like a
    /// timeout because no key can arrive. The interactive host loop clears
    /// this so a pending mapping parks, the way upstream's main loop waits.
    no_more_input: bool,
    /// `CTRL-\` seen, waiting for the second key of `CTRL-\ CTRL-N`
    /// (`nv_normal`, `normal.c`), which exits Insert, Cmdline, or Visual to
    /// Normal mode.
    pending_ctrl_bslash: bool,
    /// `CTRL-V` in command-line mode: insert the next character literally.
    pending_cmdline_literal: bool,
    /// Insert mode temporarily yielded to one Normal-mode command with `CTRL-O`.
    one_normal_command: bool,
    /// Register currently recording macros, if any (`reg_recording`).
    recording: Option<char>,
    /// Encoded keys captured for the active recording (`recordbuff`).
    recordbuff: Vec<u8>,
    /// Encoded keys replayed by `.` (`redobuff.cur.body`), paste-shaped.
    redo_buf: Vec<u8>,
    /// Captured paste-stream content between the paste start/end keys.
    paste_capture: Option<Vec<u8>>,
    /// Complete captured paste streams waiting for `nvim_paste` replay.
    pending_paste_repeats: Vec<Vec<u8>>,
}

impl Default for ModeMachine {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            search: SearchState::default(),
            last_find: None,
            last_visual: None,
            completed_ex_command: None,
            cmdline_history: Vec::new(),
            pending_mapping_action: None,
            map_depth: 0,
            timestamp: 0,
            map_pending: false,
            no_more_input: true,
            pending_ctrl_bslash: false,
            pending_cmdline_literal: false,
            one_normal_command: false,
            recording: None,
            recordbuff: Vec::new(),
            redo_buf: Vec::new(),
            paste_capture: None,
            pending_paste_repeats: Vec::new(),
        }
    }
}

impl ModeMachine {
    /// Internal key opening a stored paste stream (`K_PASTE_START`).
    const PASTE_START_KEY: u8 = b'p';
    /// Internal key closing a stored paste stream (`K_PASTE_END`).
    const PASTE_END_KEY: u8 = b'q';
}

/// Editor context captured for one key execution.
struct Context {
    buffer: BufHandle,
    window: WinHandle,
    cursor: Position,
    lines: Vec<Vec<u8>>,
    topline: usize,
    bottomline: usize,
}

/// The per-keystroke snapshot typing paths need: window, buffer, and a
/// validated cursor, with no buffer copy. Motions and textobjects fetch
/// lines through the full [`Context`]; insert and replace typing never
/// read more than one line, so they avoid the whole-buffer copy
/// `context()` pays on every key.
struct CursorContext {
    buffer: BufHandle,
    window: WinHandle,
    cursor: Position,
}

impl ModeMachine {
    /// Returns the active mode.
    #[must_use]
    pub const fn mode(&self) -> &Mode {
        &self.mode
    }
    /// Returns mutable access to the active mode state.
    pub const fn mode_mut(&mut self) -> &mut Mode {
        &mut self.mode
    }

    /// Whether the current mode is waiting for a key that completes a command.
    ///
    /// Two upstream states block: an incomplete multi-key command prefix
    /// (`g`, `CTRL-W`), and a typeahead front that is only the prefix of a
    /// longer mapping (`vgetorpeek` waiting for the mapping to resolve,
    /// tracked by [`Self::check`]).
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        if self.map_pending {
            return true;
        }
        match &self.mode {
            Mode::Normal(state) => !state.prefix.is_empty(),
            Mode::Visual(state) => !state.prefix.is_empty(),
            Mode::Insert(_) | Mode::Replace(_) | Mode::Cmdline(_) | Mode::OperatorPending(_) => {
                false
            }
        }
    }
    /// Returns the last-search state.
    #[must_use]
    pub const fn search_state(&self) -> &SearchState {
        &self.search
    }
    /// Takes an Ex command completed with Enter.
    pub fn take_ex_command(&mut self) -> Option<String> {
        self.completed_ex_command.take()
    }

    /// Returns the command-line text currently being edited.
    #[must_use]
    pub fn cmdline_text(&self) -> &str {
        match &self.mode {
            Mode::Cmdline(state) => &state.text,
            _ => "",
        }
    }

    /// Returns an entry from Ex command-line history using Vim's signed indexing.
    #[must_use]
    pub fn cmdline_history(&self, index: isize) -> Option<&str> {
        let len = isize::try_from(self.cmdline_history.len()).ok()?;
        let index = if index < 0 {
            len.checked_add(index)?
        } else {
            index
        };
        usize::try_from(index)
            .ok()
            .and_then(|index| self.cmdline_history.get(index))
            .map(String::as_str)
    }

    /// Enters Insert mode at the current cursor.
    pub fn enter_insert(&mut self) {
        self.mode = Mode::Insert(InsertState);
        self.pending_ctrl_bslash = false;
        self.pending_cmdline_literal = false;
    }

    /// Moves to the end of the current line and enters Insert mode.
    ///
    /// # Errors
    ///
    /// Returns an editor error when the current cursor context is unavailable.
    pub fn enter_append(&mut self, editor: &mut Editor) -> Result<(), ModeError> {
        Self::advance_insert_cursor(editor, true)?;
        self.enter_insert();
        Ok(())
    }

    /// Enters Replace mode at the cursor (`:startreplace`, `edit.c`).
    /// Clears two-key parser residue (`CTRL-\` half-sequence, cmdline
    /// literal) so a pending sequence from the prior mode cannot leak in.
    pub fn enter_replace(&mut self) {
        self.mode = Mode::Replace(ReplaceState);
        self.pending_ctrl_bslash = false;
        self.pending_cmdline_literal = false;
    }

    /// Leaves Insert, Replace, or terminal-input mode without applying a cursor motion.
    pub fn stop_insert(&mut self) {
        if matches!(self.mode, Mode::Insert(_) | Mode::Replace(_)) {
            self.mode = Mode::Normal(NormalState::default());
        }
        self.pending_ctrl_bslash = false;
        self.pending_cmdline_literal = false;
    }

    /// Ends active Visual mode for an API focus change, retaining the saved
    /// `gv` selection (`end_visual_mode` / `reset_VIsual_and_resel`,
    /// `normal.c`).
    ///
    /// When the active mode is [`Mode::Visual`], clears its transient parser
    /// fields (prefix and count) and moves the resulting [`VisualState`] —
    /// anchor, cursor, kind, and wanted column — into `last_visual` so `gv`
    /// can restore the selection, switches to default Normal, and clears
    /// pending `CTRL-\` state. When Visual is not active, leaves the current
    /// mode and `last_visual` unchanged. This is the single
    /// invariant-preserving way for API focus changes to end Visual mode;
    /// callers must not mutate `mode` directly.
    pub fn reset_visual_mode(&mut self) {
        if matches!(self.mode, Mode::Visual(_)) {
            if let Mode::Visual(state) = &mut self.mode {
                // Clear transient parser fields before saving the `gv`
                // snapshot so a restored selection starts with clean parser
                // state; preserve anchor, cursor, kind, and wanted column.
                state.count = 0;
                state.prefix.clear();
                self.last_visual = Some(state.clone());
            }
            self.mode = Mode::default();
            self.pending_ctrl_bslash = false;
        }
    }

    /// Returns the register currently recording macros, if any.
    #[must_use]
    pub const fn recording_register(&self) -> Option<char> {
        self.recording
    }

    /// Takes every completed captured paste stream (`paste_repeat`'s
    /// typeahead read) for replay by the embedding host.
    pub fn take_paste_repeats(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_paste_repeats)
    }

    /// Whether a completed stored paste must run before more typeahead.
    #[must_use]
    pub fn has_pending_paste_repeat(&self) -> bool {
        !self.pending_paste_repeats.is_empty()
    }

    /// Opens a paste stream in the redo and recording buffers
    /// (`paste_store`, `input.c:3490-3501`).
    pub fn paste_store_start(&mut self, record: bool) {
        self.paste_store_marker(Self::PASTE_START_KEY, record);
    }

    /// Closes a paste stream in the redo and recording buffers.
    pub fn paste_store_end(&mut self, record: bool) {
        self.paste_store_marker(Self::PASTE_END_KEY, record);
    }

    fn paste_store_marker(&mut self, marker: u8, record: bool) {
        let bytes = [K_SPECIAL, KS_EXTRA, marker];
        if marker == Self::PASTE_START_KEY {
            // `redo_new` (`input.c:3493-3495`): a paste starting outside an
            // insert session begins a new redo entry, replacing the last one.
            self.redo_buf.clear();
        }
        self.redo_buf.extend_from_slice(&bytes);
        if record && self.recording.is_some() {
            self.recordbuff.extend_from_slice(&bytes);
        }
    }

    /// Appends pasted content to the redo and recording buffers, escaping
    /// the bytes the typeahead cannot carry literally (`paste_store`,
    /// `input.c:3504-3538`).
    pub fn paste_store_content(&mut self, data: &[u8], crlf: bool, record: bool) {
        let mut content = Vec::with_capacity(data.len());
        if crlf {
            let mut index = 0;
            while index < data.len() {
                if data[index] == b'\r' {
                    content.push(b'\n');
                    index += usize::from(data.get(index + 1) == Some(&b'\n'));
                } else {
                    content.push(data[index]);
                }
                index += 1;
            }
        } else {
            content.extend_from_slice(data);
        }
        let encoded = Keys::encode(&content);
        self.redo_buf.extend_from_slice(encoded.as_bytes());
        if record && self.recording.is_some() {
            self.recordbuff.extend_from_slice(encoded.as_bytes());
        }
    }
    /// Takes a non-key mapping action, with the flags it was registered with,
    /// for execution by the embedding host.
    pub fn take_mapping_action(&mut self) -> Option<(MappingAction, MappingOptions)> {
        self.pending_mapping_action.take()
    }

    /// `state.c:34-106`: checking consumes mappings and input but performs no buffer mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when input decoding or mapping expansion fails, or when
    /// recursive mapping reaches `'maxmapdepth'`.
    pub fn check(&mut self, editor: &mut Editor) -> Result<Step, ModeError> {
        loop {
            let Some(flags) = editor.typeahead().front_flags() else {
                self.map_pending = false;
                return Ok(Step::Idle);
            };
            if flags.remap == Remap::Yes {
                let mode = map_mode(&self.mode);
                let buffer = editor.current_buffer();
                let lookup = editor
                    .mappings()
                    .lookup_typeahead(editor.typeahead(), mode, buffer);
                let resolved = match lookup {
                    Lookup::Exact(mapping, width) => {
                        Some((mapping.action.clone(), mapping.options.clone(), width))
                    }
                    Lookup::Prefix(_) => {
                        self.map_pending = true;
                        return Ok(Step::Idle);
                    }
                    Lookup::None => None,
                };
                if let Some((action, options, width)) = resolved {
                    if self.recording.is_some() && !flags.mapped {
                        let lhs = editor.typeahead().keylen(width).to_vec();
                        self.recordbuff.extend_from_slice(&lhs);
                    }
                    if self.apply_mapping(editor, action, options, width)? {
                        return Ok(Step::ProcessEvents);
                    }
                    continue;
                }
            }
            self.may_sync_undo(editor, flags);
            // `mapdepth = 0` once a character is actually returned
            // (`vgetorpeek`, `getchar.c`): the limit counts expansions that
            // produced no input, not expansions overall.
            let Some(key) = editor.typeahead_mut().pop()? else {
                self.map_pending = false;
                return Ok(Step::Idle);
            };
            self.map_pending = false;
            // `gotchars` (`input.c`): only typed keys enter a recording, and
            // a mapping's left-hand side is typed even though its expansion
            // is not (recorded above at the mapping site).
            if self.recording.is_some() && !flags.mapped {
                self.recordbuff.extend_from_slice(&encoded_key(key));
            }
            // `paste_repeat` (`input.c:3550-3572`): the bytes between the
            // paste start and end keys are captured, not executed.
            if let Key::Special(KS_EXTRA, third) = key {
                if third == Self::PASTE_START_KEY {
                    self.paste_capture = Some(Vec::new());
                    continue;
                }
                if third == Self::PASTE_END_KEY {
                    if let Some(captured) = self.paste_capture.take() {
                        self.pending_paste_repeats.push(captured);
                    }
                    return Ok(Step::ProcessEvents);
                }
            }
            if let Some(captured) = self.paste_capture.as_mut() {
                match key {
                    Key::Byte(byte) => captured.push(byte),
                    Key::Special(second, third) => {
                        captured.extend_from_slice(&[K_SPECIAL, second, third]);
                    }
                }
                continue;
            }
            return match key {
                Key::Byte(byte) => Ok(Step::Key(char::from(byte))),
                Key::Special(KS_EXTRA, b'R' | b'N') => Ok(Step::Key('\r')),
                Key::Special(KS_EXTRA, b'T') => Ok(Step::Key('\t')),
                Key::Special(KS_EXTRA, b'E') => Ok(Step::Key('\u{1b}')),
                Key::Special(KS_EXTRA, b'B') => Ok(Step::Key('\u{8}')),
                Key::Special(_, _) => Ok(Step::ProcessEvents),
            };
        }
    }

    /// `vgetorpeek`'s mapping timeout (`getchar.c`), for the case where the
    /// wait is already over: inside `:normal`, or under `feedkeys()`'s `x`
    /// flag, no further key can arrive, so an incomplete mapping "behaves like
    /// it timed out". The longest *complete* match already queued wins; with
    /// no complete match the queued keys are used literally, which upstream
    /// arranges by clearing one byte of `typebuf.tb_noremap` and returning it.
    ///
    /// [`Self::check`] deliberately keeps waiting instead, because the
    /// interactive path really can receive another key. Returns whether the
    /// queue changed, so a caller draining it knows to keep going.
    ///
    /// # Errors
    ///
    /// Returns an error when expanding the completed mapping fails or reaches
    /// `'maxmapdepth'`.
    pub fn timeout_pending_mapping(&mut self, editor: &mut Editor) -> Result<bool, ModeError> {
        if !self.no_more_input {
            // Interactive drain: the host can still receive keys, so the
            // pending mapping parks (`nvim_get_mode` reports `blocking`)
            // instead of resolving.
            return Ok(false);
        }
        let Some(flags) = editor.typeahead().front_flags() else {
            return Ok(false);
        };
        if flags.remap != Remap::Yes {
            return Ok(false);
        }
        let mode = map_mode(&self.mode);
        let buffer = editor.current_buffer();
        let resolved = match editor
            .mappings()
            .lookup_typeahead(editor.typeahead(), mode, buffer)
        {
            Lookup::Prefix(Some(mapping)) => Some((
                mapping.action.clone(),
                mapping.options.clone(),
                mapping.lhs.len(),
            )),
            Lookup::Prefix(None) => None,
            Lookup::Exact(_, _) | Lookup::None => return Ok(false),
        };
        if let Some((action, options, width)) = resolved {
            self.apply_mapping(editor, action, options, width)?;
            Ok(true)
        } else {
            editor.typeahead_mut().deny_front_remap();
            Ok(true)
        }
    }

    /// Reports whether the current drain is closed to further input.
    #[must_use]
    pub const fn no_more_input(&self) -> bool {
        self.no_more_input
    }

    /// Marks whether the current drain is closed to further input (`:normal`,
    /// `feedkeys()` with `x`). Closed drains resolve an incomplete mapping
    /// like a timeout; the interactive host loop clears this so a pending
    /// mapping survives, the way upstream's main loop waits for the next key.
    pub fn set_no_more_input(&mut self, no_more_input: bool) {
        self.no_more_input = no_more_input;
    }

    /// Consumes a matched mapping's left-hand side and installs its
    /// right-hand side: replacement keys go back onto the typeahead as
    /// not-typed input (`ins_typebuf`), and anything only a host can run is
    /// parked for [`Self::take_mapping_action`].
    ///
    /// Reports whether an action was parked, which is the caller's cue to
    /// leave the input loop so the host can run it.
    fn apply_mapping(
        &mut self,
        editor: &mut Editor,
        action: MappingAction,
        options: MappingOptions,
        width: usize,
    ) -> Result<bool, ModeError> {
        // `if (++mapdepth >= p_mmd) { emsg(e_recursive_mapping) }`
        // (`vgetorpeek`, `getchar.c`): without this `nmap ,x ,x` re-expands
        // its own right-hand side forever and never returns a key.
        self.map_depth = self.map_depth.saturating_add(1);
        if u64::from(self.map_depth) >= max_map_depth(editor) {
            editor.typeahead_mut().flush();
            return Err(ModeError::RecursiveMapping);
        }
        editor.typeahead_mut().consume(width);
        match action {
            MappingAction::Keys(keys) => {
                let buffer = editor.current_buffer();
                editor.typeahead_mut().push(
                    &keys,
                    0,
                    TypeaheadFlags {
                        remap: if options.flags.contains(MapFlags::REMAP) {
                            Remap::Yes
                        } else {
                            Remap::No
                        },
                        modes: options.modes,
                        buffer,
                        mapped: true,
                        silent: options.flags.contains(MapFlags::SILENT),
                    },
                )?;
                Ok(false)
            }
            MappingAction::Nop => Ok(false),
            action => {
                self.pending_mapping_action = Some((action, options));
                Ok(true)
            }
        }
    }

    /// `may_sync_undo` (`input.c:1294-1306`): consuming a *typed* key closes
    /// the open undo block, so everything one command does lands in one block
    /// and the next thing the user types starts another.
    ///
    /// Keys a mapping produced are exempt because upstream only reports the
    /// bytes past `typebuf.tb_maplen` through `gotchars`
    /// (`input.c:2495-2497`), which is what calls this. Insert and command-line
    /// mode are exempt so one insert session, and one typed Ex command line,
    /// stay single blocks.
    ///
    /// Named gap: upstream also syncs inside Insert mode once a cursor key has
    /// moved the caret (`Ins.moved != kInsNone`). This port's insert mode has
    /// no cursor-key handling to set that state, so there is nothing here to
    /// read; when it gains one, this is the predicate to extend.
    fn may_sync_undo(&mut self, editor: &mut Editor, flags: TypeaheadFlags) {
        if flags.mapped
            || matches!(
                self.mode,
                Mode::Insert(_) | Mode::Replace(_) | Mode::Cmdline(_)
            )
        {
            return;
        }
        editor.sync_current_undo();
    }

    /// Executes the action classified by [`Self::check`].
    ///
    /// # Errors
    ///
    /// Returns an error when the key's editor operation, search, operator,
    /// insertion, indentation, or input decoding fails.
    pub fn execute(
        &mut self,
        editor: &mut Editor,
        step: Step,
        eval: &mut dyn ExprEval,
    ) -> Result<(), ModeError> {
        match step {
            Step::Idle | Step::ProcessEvents => Ok(()),
            Step::Key(key) => self.execute_key(editor, key, eval),
        }
    }

    /// Runs one check/execute iteration, returning whether work was ready.
    ///
    /// # Errors
    ///
    /// Returns an error propagated by [`Self::check`] or [`Self::execute`].
    pub fn run_once(
        &mut self,
        editor: &mut Editor,
        eval: &mut dyn ExprEval,
    ) -> Result<bool, ModeError> {
        let step = self.check(editor)?;
        let ready = step != Step::Idle;
        self.execute(editor, step, eval)?;
        Ok(ready)
    }

    /// Convenience entry point used by behavioral tests and embedding frontends.
    ///
    /// # Errors
    ///
    /// Returns an error when executing any supplied key fails.
    pub fn feed_keys(
        &mut self,
        editor: &mut Editor,
        keys: &str,
        eval: &mut dyn ExprEval,
    ) -> Result<(), ModeError> {
        for key in keys.chars() {
            self.execute_key(editor, key, eval)?;
        }
        Ok(())
    }

    fn execute_key(
        &mut self,
        editor: &mut Editor,
        key: char,
        eval: &mut dyn ExprEval,
    ) -> Result<(), ModeError> {
        self.timestamp = self.timestamp.saturating_add(1);
        // Handlers borrow `self` next to their variant state, so the mode is
        // taken into a local for the duration of dispatch; every exit path
        // refills the slot, so a handler error restores the exact pre-key
        // variant state instead of stranding the machine on a default Normal.
        let mut mode = std::mem::take(&mut self.mode);
        let was_insert = matches!(mode, Mode::Insert(_));
        let transition = match &mut mode {
            Mode::Normal(state) => self.normal(editor, state, key, eval),
            Mode::Insert(state) => self.insert(editor, state, key, eval),
            Mode::Replace(state) => self.replace_insert(editor, state, key, eval),
            Mode::Visual(state) => self.visual(editor, state, key, eval),
            Mode::Cmdline(state) => self.cmdline(editor, state, key),
            Mode::OperatorPending(state) => self.operator_pending(editor, state, key, eval),
        };
        match transition {
            Ok(Some(mut next)) => {
                // `CTRL-\` is half of a two-key command: leaving the mode that
                // captured it cancels the pending second key.
                if std::mem::discriminant(&next) != std::mem::discriminant(&mode) {
                    self.pending_ctrl_bslash = false;
                }
                if self.one_normal_command && !was_insert && matches!(next, Mode::Normal(_)) {
                    self.one_normal_command = false;
                    next = Mode::Insert(InsertState);
                }
                self.mode = next;
            }
            Ok(None) => self.mode = mode,
            Err(error) => {
                self.mode = mode;
                return Err(error);
            }
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "recovered normal-mode state machine preserves Neovim transition ordering"
    )]
    fn normal(
        &mut self,
        editor: &mut Editor,
        state: &mut NormalState,
        key: char,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        // `normal_begin` (`normal.c`): at the start of each command — when
        // no count, prefix, or register has been accumulated — reset
        // `v:register` to the unnamed default. The value set by a prior
        // register-prefixed command is thus cleared before the next
        // unprefixed command begins, while mappings and ignored keys that
        // fire during the prefixed command (non-fresh state) preserve it.
        if state.count == 0 && state.prefix.is_empty() && state.register.is_none() {
            editor.set_v_register('"');
        }
        if state.prefix == "register" {
            state.prefix.clear();
            // `op_reg_index` / `set_reg_may_clear` (`normal.c`): validate
            // the prefix key before storing it. An invalid or ignored key
            // (e.g. Escape) cancels the prefix without recording a register,
            // so the state is fresh and the next command resets `v:register`
            // to the default and behaves as an unprefixed command — no stale
            // invalid register survives to corrupt the following operation.
            if !is_valid_register_name(key) {
                return Ok(None);
            }
            // Retain the original case in `state.register` so uppercase
            // prefixes keep their append semantics (`A` appends to `a`).
            state.register = Some(key);
            // Publish the canonical lowercase named slot to `v:register`,
            // matching `get_register_name` (`register.h:31-74`): an
            // uppercase write canonicalizes to its lowercase slot, so
            // omitted Register-family queries see `a` for both `"a` and
            // `"A` prefixes.
            let canonical = if key.is_ascii_uppercase() {
                ((key as u8) - b'A' + b'a') as char
            } else {
                key
            };
            editor.set_v_register(canonical);
            return Ok(None);
        }
        if state.prefix == "record" {
            state.prefix.clear();
            if key == '\u{1b}' {
                return Ok(None);
            }
            self.recording = Some(key);
            self.recordbuff.clear();
            return Ok(None);
        }
        if state.prefix == "exec" {
            state.prefix.clear();
            if key == '\u{1b}' {
                return Ok(None);
            }
            Self::exec_register(editor, key, state.count.max(1))?;
            return Ok(Some(Mode::default()));
        }
        if state.prefix == "r" {
            if key == '\u{16}' || key == '\u{11}' {
                state.prefix = "r\u{16}".into();
                return Ok(None);
            }
            state.prefix.clear();
            if key == '\u{1b}' {
                return Ok(Some(Mode::default()));
            }
            self.replace_chars(editor, state.count.max(1), classify_replace_key(key, false))?;
            return Ok(Some(Mode::default()));
        }
        if state.prefix == "r\u{16}" {
            state.prefix.clear();
            if key == '\u{1b}' {
                return Ok(Some(Mode::default()));
            }
            self.replace_chars(editor, state.count.max(1), classify_replace_key(key, true))?;
            return Ok(Some(Mode::default()));
        }
        if matches!(state.prefix.as_str(), "f" | "F" | "t" | "T") {
            let find = FindMotion {
                direction: if matches!(state.prefix.as_str(), "f" | "t") {
                    FindDirection::Forward
                } else {
                    FindDirection::Backward
                },
                till: matches!(state.prefix.as_str(), "t" | "T"),
                target: key as u8,
            };
            // `nv_csearch`: `if (searchc(cap, t_cmd) == false) clearopbeep()`.
            // `searchc` records the target before searching, so a failed
            // `fz` is still what `;` repeats.
            if !Self::move_find(editor, find, state.count.max(1), false)? {
                beep_flush(editor);
            }
            self.last_find = Some(find);
            return Ok(Some(Mode::default()));
        }
        if state.prefix == "g" {
            state.prefix.clear();
            if key == 'v' {
                if let Some(visual) = self.last_visual.clone() {
                    let window = context(editor)?.window;
                    editor.set_window_cursor(window, visual.cursor)?;
                    return Ok(Some(Mode::Visual(visual)));
                }
                return Ok(Some(Mode::default()));
            }
            if key == 'H' {
                let cursor = context(editor)?.cursor;
                return Ok(Some(Mode::Visual(VisualState::new(
                    cursor,
                    VisualKind::Line,
                ))));
            }
            if key == 'u' || key == 'U' {
                return Ok(Some(Mode::OperatorPending(OperatorPendingState {
                    operator: if key == 'u' {
                        Operator::Lowercase
                    } else {
                        Operator::Uppercase
                    },
                    count: state.count.max(1),
                    count_was_set: state.count != 0,
                    motion_count: 0,
                    register: state.register,
                    prefix: String::new(),
                    force_motion: None,
                    search: None,
                })));
            }
            if key == 'f' {
                Self::goto_file_under_cursor(editor)?;
                return Ok(Some(Mode::default()));
            }
            let command = format!("g{key}");
            Self::move_command(editor, &command, state.count.max(1), false)?;
            return Ok(Some(Mode::default()));
        }
        if state.prefix == "[" || state.prefix == "]" {
            let prefix = state.prefix.clone();
            state.prefix.clear();
            if matches!(key, 'i' | 'I' | '\t' | 'd' | 'D' | '\u{4}') {
                Self::ident_search(editor, &prefix, key, state.count.max(1))?;
                return Ok(Some(Mode::default()));
            }
            let command = format!("{prefix}{key}");
            Self::move_command(editor, &command, state.count.max(1), false)?;
            return Ok(Some(Mode::default()));
        }
        if state.prefix == "\u{17}g" {
            state.prefix.clear();
            if key == '}' {
                Self::preview_ident_tag(editor, state.count.max(1))?;
            }
            return Ok(Some(Mode::default()));
        }
        if state.prefix == "\u{17}" {
            if key == 'g' || key == '\u{7}' {
                state.prefix = "\u{17}g".into();
                return Ok(None);
            }
            state.prefix.clear();
            if matches!(key, 'i' | 'I' | '\t' | 'd' | 'D' | '\u{4}') {
                Self::ident_search(editor, "\u{17}", key, state.count.max(1))?;
                return Ok(Some(Mode::default()));
            }
            if key == '}' {
                Self::preview_ident_tag(editor, state.count.max(1))?;
                return Ok(Some(Mode::default()));
            }
            Self::wincmd(editor, key);
            return Ok(Some(Mode::default()));
        }

        if key.is_ascii_digit() && (key != '0' || state.count != 0) {
            state.count = append_digit(state.count, key);
            return Ok(None);
        }
        let count = state.count.max(1);
        match key {
            '"' => {
                state.prefix = "register".into();
                Ok(None)
            }
            'd' | 'c' | 'y' | '>' | '<' | '=' => {
                Ok(Some(Mode::OperatorPending(OperatorPendingState {
                    operator: operator_for(key),
                    count,
                    count_was_set: state.count != 0,
                    motion_count: 0,
                    register: state.register,
                    prefix: String::new(),
                    force_motion: None,
                    search: None,
                })))
            }
            'g' | '[' | ']' | '\u{17}' | 'f' | 'F' | 't' | 'T' => {
                state.prefix = key.to_string();
                Ok(None)
            }
            'r' => {
                state.prefix = "r".into();
                Ok(None)
            }
            // `searchc` returns false when there is no previous `f`/`t` to
            // repeat (`*lastc == NUL`), and `nv_csearch` turns that into
            // `clearopbeep` — which flushes the rest of the mapped typeahead,
            // so the remainder of a `:normal` argument never runs.
            ';' | ',' => {
                let moved = match self.last_find {
                    Some(mut find) => {
                        if key == ',' {
                            find.direction = reverse_find(find.direction);
                        }
                        Self::move_find(editor, find, count, false)?
                    }
                    None => false,
                };
                if !moved {
                    beep_flush(editor);
                }
                Ok(Some(Mode::default()))
            }
            'h' | 'j' | 'k' | 'l' | 'w' | 'W' | 'e' | 'E' | 'b' | 'B' | '0' | '^' | '$' | '%'
            | '{' | '}' | '(' | ')' | 'G' | 'H' | 'M' | 'L' | '|' | ' ' => {
                let command = if key == 'G' && state.count != 0 {
                    "G_count".to_owned()
                } else if key == ' ' {
                    "l".to_owned()
                } else {
                    key.to_string()
                };
                Self::move_command(editor, &command, count, false)?;
                Ok(Some(Mode::default()))
            }
            'i' => Ok(Some(Mode::Insert(InsertState))),
            'R' => Ok(Some(Mode::Replace(ReplaceState))),
            'a' => {
                Self::advance_insert_cursor(editor, false)?;
                Ok(Some(Mode::Insert(InsertState)))
            }
            'A' => {
                Self::advance_insert_cursor(editor, true)?;
                Ok(Some(Mode::Insert(InsertState)))
            }
            'I' => {
                Self::move_command(editor, "^", 1, false)?;
                Ok(Some(Mode::Insert(InsertState)))
            }
            'o' | 'O' => {
                self.open_line(editor, key == 'o', eval)?;
                Ok(Some(Mode::Insert(InsertState)))
            }
            'v' | 'V' | '\u{16}' | '\u{11}' => {
                let cursor = context(editor)?.cursor;
                Ok(Some(Mode::Visual(VisualState::new(
                    cursor,
                    if key == 'v' {
                        VisualKind::Character
                    } else if key == 'V' {
                        VisualKind::Line
                    } else {
                        VisualKind::Block
                    },
                ))))
            }
            '/' | '?' => Ok(Some(Mode::Cmdline(CmdlineState {
                kind: CmdlineKind::Search(if key == '/' {
                    SearchDirection::Forward
                } else {
                    SearchDirection::Backward
                }),
                text: String::new(),
                count,
            }))),
            ':' => Ok(Some(Mode::Cmdline(CmdlineState {
                kind: CmdlineKind::Ex,
                text: String::new(),
                count,
            }))),
            'n' | 'N' => {
                self.repeat_search(editor, key == 'N', count)?;
                Ok(Some(Mode::default()))
            }
            'p' | 'P' => {
                let ctx = context(editor)?;
                let name = state.register.unwrap_or('"');
                let direction = if key == 'p' {
                    PutDirection::After
                } else {
                    PutDirection::Before
                };
                let _ = editor.put_register(ctx.window, name, direction, count, self.timestamp)?;
                Ok(Some(Mode::default()))
            }
            'J' => {
                let ctx = context(editor)?;
                let end_lnum = ctx
                    .cursor
                    .lnum
                    .saturating_add(count.max(2) - 1)
                    .min(ctx.lines.len());
                self.join_lines(editor, ctx.cursor.lnum, end_lnum)?;
                Ok(Some(Mode::default()))
            }
            'x' => {
                let ctx = context(editor)?;
                let end = Position {
                    lnum: ctx.cursor.lnum,
                    col: ctx.cursor.col.saturating_add(count - 1),
                };
                ops::apply(
                    editor,
                    OperatorRequest {
                        buffer: ctx.buffer,
                        window: ctx.window,
                        operator: Operator::Delete,
                        range: EditRange {
                            start: ctx.cursor,
                            end,
                            kind: MotionKind::CharacterWise,
                            inclusive: true,
                        },
                        register: state.register,
                        timestamp: self.timestamp,
                        eval,
                    },
                )?;
                Ok(Some(Mode::default()))
            }
            '~' => {
                let ctx = context(editor)?;
                let end = Position {
                    lnum: ctx.cursor.lnum,
                    col: ctx.cursor.col.saturating_add(count - 1),
                };
                ops::apply(
                    editor,
                    OperatorRequest {
                        buffer: ctx.buffer,
                        window: ctx.window,
                        operator: Operator::ToggleCase,
                        range: EditRange {
                            start: ctx.cursor,
                            end,
                            kind: MotionKind::CharacterWise,
                            inclusive: true,
                        },
                        register: None,
                        timestamp: self.timestamp,
                        eval,
                    },
                )?;
                Self::move_command(editor, "l", count, false)?;
                Ok(Some(Mode::default()))
            }
            '\u{1}' | '\u{18}' => {
                let delta = i64::try_from(count.min(999_999_999)).unwrap_or(999_999_999);
                self.adjust_number(editor, if key == '\u{1}' { delta } else { -delta })?;
                Ok(Some(Mode::default()))
            }
            '.' => {
                if self.redo_buf.is_empty() {
                    beep_flush(editor);
                } else {
                    let keys = Keys::from_encoded(self.redo_buf.clone())?;
                    let flags = TypeaheadFlags {
                        remap: Remap::No,
                        mapped: true,
                        ..TypeaheadFlags::default()
                    };
                    for _ in 0..count {
                        editor.typeahead_mut().append(&keys, flags);
                    }
                }
                Ok(Some(Mode::default()))
            }
            'q' => {
                if let Some(name) = self.recording.take() {
                    if self.recordbuff.last() == Some(&b'q') {
                        self.recordbuff.pop();
                    }
                    let rows: Vec<Vec<u8>> = self
                        .recordbuff
                        .split(|byte| *byte == b'\n')
                        .map(<[u8]>::to_vec)
                        .collect();
                    self.recordbuff.clear();
                    if editor
                        .registers_mut()
                        .set(
                            name,
                            RegisterContent::from_binary_lines(RegisterKind::CharacterWise, rows),
                        )
                        .is_err()
                    {
                        beep_flush(editor);
                    }
                } else {
                    state.prefix = "record".into();
                }
                Ok(None)
            }
            '@' => {
                state.prefix = "exec".into();
                Ok(None)
            }
            'u' => {
                let ctx = context(editor)?;
                editor.buffer_undo(ctx.buffer)?;
                Ok(Some(Mode::default()))
            }
            '\u{1d}' => {
                Self::jump_ident_tag(editor, count)?;
                Ok(Some(Mode::default()))
            }
            '\u{14}' => {
                let window = context(editor)?.window;
                let old_idx = editor
                    .window_tag_stack(window)
                    .map_or(1, crate::tags::TagStack::curidx);
                let item = match editor
                    .window_tag_stack_mut(window)
                    .map(|stack| stack.pop(count))
                {
                    Ok(Ok(item)) => item,
                    Ok(Err(crate::tags::TagStackBoundary::Empty)) => {
                        return Err(ModeError::Vim("E73", "Tag stack empty".to_owned()));
                    }
                    Ok(Err(crate::tags::TagStackBoundary::Bottom)) => {
                        return Err(ModeError::Vim("E555", "At bottom of tag stack".to_owned()));
                    }
                    Ok(Err(crate::tags::TagStackBoundary::Top)) => {
                        return Err(ModeError::Vim("E556", "At top of tag stack".to_owned()));
                    }
                    Err(error) => return Err(error.into()),
                };
                let current = editor.current_buffer();
                if current != Some(item.from_bufnr)
                    && current.is_some_and(|handle| {
                        editor
                            .buffer(handle)
                            .is_ok_and(|buffer| buffer.flags.contains(crate::BufferFlags::MODIFIED))
                    })
                {
                    if let Ok(stack) = editor.window_tag_stack_mut(window) {
                        stack.set_curidx(i64::try_from(old_idx).unwrap_or(i64::MAX));
                    }
                    return Err(ModeError::Vim(
                        "E37",
                        "No write since last change (add ! to override)".to_owned(),
                    ));
                }
                if editor
                    .set_current_buffer(item.from_bufnr, BufferRelease::KeepLoaded)
                    .is_err()
                {
                    if let Ok(stack) = editor.window_tag_stack_mut(window) {
                        stack.set_curidx(i64::try_from(old_idx).unwrap_or(i64::MAX));
                    }
                    return Err(ModeError::Vim("E555", "At bottom of tag stack".to_owned()));
                }
                editor.set_window_cursor(
                    window,
                    Position {
                        lnum: item.from_lnum.max(1),
                        col: item.from_col.saturating_sub(1),
                    },
                )?;
                Ok(Some(Mode::default()))
            }
            _ => Ok(Some(Mode::default())),
        }
    }

    fn operator_pending(
        &mut self,
        editor: &mut Editor,
        state: &mut OperatorPendingState,
        key: char,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        if let Some((direction, expression)) = state.search.as_mut() {
            let direction = *direction;
            let expression = std::mem::take(expression);
            return self.operator_pending_search(editor, state, key, eval, direction, expression);
        }
        if matches!(state.prefix.as_str(), "f" | "F" | "t" | "T") {
            return self.operator_pending_find(editor, state, key, eval);
        }
        self.operator_pending_rest(editor, state, key, eval)
    }

    /// Entering a count (`1d/foo`) re-borrows `state` while the operator's
    /// search expression is being edited, so the expression is owned here and
    /// written back for the non-terminal keys.
    fn operator_pending_search(
        &mut self,
        editor: &mut Editor,
        state: &mut OperatorPendingState,
        key: char,
        eval: &mut dyn ExprEval,
        direction: SearchDirection,
        mut expression: String,
    ) -> Result<Option<Mode>, ModeError> {
        match key {
            '\u{1b}' => Ok(Some(Mode::default())),
            '\u{8}' | '\u{7f}' => {
                expression.pop();
                state.search = Some((direction, expression));
                Ok(None)
            }
            '\n' | '\r' => {
                let delimiter = match direction {
                    SearchDirection::Forward => '/',
                    SearchDirection::Backward => '?',
                };
                if expression.ends_with(delimiter) {
                    expression.pop();
                }
                let ctx = context(editor)?;
                let count = state.count.saturating_mul(state.motion_count.max(1));
                let Ok(result) = self.search.search(
                    &ctx.lines,
                    ctx.cursor,
                    &expression,
                    direction,
                    count,
                    option_bool(editor, "wrapscan", true),
                ) else {
                    beep_flush(editor);
                    return Ok(Some(Mode::default()));
                };
                push_jump(editor, ctx.buffer, ctx.cursor);
                let range = EditRange::from_motion(
                    ctx.cursor,
                    crate::motion::Motion {
                        target: result.target,
                        kind: if result.has_line_offset {
                            MotionKind::LineWise
                        } else {
                            MotionKind::CharacterWise
                        },
                        inclusive: result.use_end,
                        is_jump: true,
                        keep_curswant: false,
                    },
                );
                let change = state.operator == Operator::Change;
                self.apply_operator(editor, state, range, eval)?;
                Ok(Some(if change {
                    Mode::Insert(InsertState)
                } else {
                    Mode::default()
                }))
            }
            ch if !ch.is_control() => {
                expression.push(ch);
                state.search = Some((direction, expression));
                Ok(None)
            }
            _ => {
                state.search = Some((direction, expression));
                Ok(None)
            }
        }
    }
    /// Completes an operator range with an `f`/`F`/`t`/`T` find motion.
    fn operator_pending_find(
        &mut self,
        editor: &mut Editor,
        state: &mut OperatorPendingState,
        key: char,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        let find = FindMotion {
            direction: if matches!(state.prefix.as_str(), "f" | "t") {
                FindDirection::Forward
            } else {
                FindDirection::Backward
            },
            till: matches!(state.prefix.as_str(), "t" | "T"),
            target: key as u8,
        };
        let ctx = context(editor)?;
        let Some(motion) = resolve_find(
            &ctx.lines,
            ctx.cursor,
            find,
            state.count.saturating_mul(state.motion_count.max(1)),
        ) else {
            return Ok(Some(Mode::default()));
        };
        self.apply_operator(
            editor,
            state,
            EditRange::from_motion(ctx.cursor, motion),
            eval,
        )?;
        self.last_find = Some(find);
        Ok(Some(if state.operator == Operator::Change {
            Mode::Insert(InsertState)
        } else {
            Mode::default()
        }))
    }

    fn operator_pending_rest(
        &mut self,
        editor: &mut Editor,
        state: &mut OperatorPendingState,
        key: char,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        if state.prefix == "g" {
            state.prefix.clear();
            let command = format!("g{key}");
            return self.finish_operator_motion(editor, state, &command, eval);
        }
        if state.prefix == "i" || state.prefix == "a" {
            let inner = state.prefix == "i";
            let ctx = context(editor)?;
            if let Some(range) = textobject::resolve(
                &ctx.lines,
                ctx.cursor,
                inner,
                key,
                state.count.saturating_mul(state.motion_count.max(1)),
            ) {
                let change = state.operator == Operator::Change;
                self.apply_operator(editor, state, range, eval)?;
                return Ok(Some(if change {
                    Mode::Insert(InsertState)
                } else {
                    Mode::default()
                }));
            }
            return Ok(Some(Mode::default()));
        }
        if key == '\u{1b}' {
            return Ok(Some(Mode::default()));
        }
        if key == 'v' || key == 'V' {
            state.force_motion = Some(if key == 'v' {
                MotionKind::CharacterWise
            } else {
                MotionKind::LineWise
            });
            return Ok(None);
        }
        if key == '/' || key == '?' {
            state.search = Some((
                if key == '/' {
                    SearchDirection::Forward
                } else {
                    SearchDirection::Backward
                },
                String::new(),
            ));
            return Ok(None);
        }
        if key.is_ascii_digit() && (key != '0' || state.motion_count != 0) {
            state.motion_count = append_digit(state.motion_count, key);
            return Ok(None);
        }
        if key == 'i' || key == 'a' || matches!(key, 'f' | 'F' | 't' | 'T') {
            state.prefix = key.to_string();
            return Ok(None);
        }
        if key == 'g' {
            state.prefix = "g".into();
            return Ok(None);
        }
        if key == operator_key(state.operator) {
            let ctx = context(editor)?;
            let end = Position {
                lnum: ctx
                    .cursor
                    .lnum
                    .saturating_add(state.count.saturating_mul(state.motion_count.max(1)) - 1)
                    .min(ctx.lines.len()),
                col: 0,
            };
            let range = EditRange {
                start: ctx.cursor,
                end,
                kind: MotionKind::LineWise,
                inclusive: true,
            };
            let change = state.operator == Operator::Change;
            self.apply_operator(editor, state, range, eval)?;
            return Ok(Some(if change {
                Mode::Insert(InsertState)
            } else {
                Mode::default()
            }));
        }
        self.finish_operator_motion(editor, state, &key.to_string(), eval)
    }

    fn finish_operator_motion(
        &mut self,
        editor: &mut Editor,
        state: &mut OperatorPendingState,
        command: &str,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        let ctx = context(editor)?;
        let count = state.count.saturating_mul(state.motion_count.max(1));
        let current = ctx
            .lines
            .get(ctx.cursor.lnum.saturating_sub(1))
            .and_then(|line| line.get(ctx.cursor.col))
            .copied();
        let resolved_command = match (state.operator, command, current) {
            (Operator::Change, "w", Some(byte)) if !byte.is_ascii_whitespace() => "e",
            (Operator::Change, "W", Some(byte)) if !byte.is_ascii_whitespace() => "E",
            (_, "G", _) if state.count_was_set || state.motion_count != 0 => "G_count",
            _ => command,
        };
        if let Some(motion) = resolve(
            &ctx.lines,
            ctx.cursor,
            resolved_command,
            count,
            option_bool(editor, "startofline", true),
            (ctx.topline, ctx.bottomline),
        ) {
            let change = state.operator == Operator::Change;
            if motion.is_jump {
                push_jump(editor, ctx.buffer, ctx.cursor);
            }
            self.apply_operator(
                editor,
                state,
                EditRange::from_motion(ctx.cursor, motion),
                eval,
            )?;
            return Ok(Some(if change {
                Mode::Insert(InsertState)
            } else {
                Mode::default()
            }));
        }
        Ok(Some(Mode::default()))
    }

    fn apply_operator(
        &mut self,
        editor: &mut Editor,
        state: &OperatorPendingState,
        mut range: EditRange,
        eval: &mut dyn ExprEval,
    ) -> Result<(), ModeError> {
        if let Some(force) = state.force_motion {
            if force == MotionKind::CharacterWise {
                range.inclusive = if range.kind == MotionKind::CharacterWise {
                    !range.inclusive
                } else {
                    false
                };
            }
            range.kind = force;
        }
        let ctx = context(editor)?;
        ops::apply(
            editor,
            OperatorRequest {
                buffer: ctx.buffer,
                window: ctx.window,
                operator: state.operator,
                range,
                register: state.register,
                timestamp: self.timestamp,
                eval,
            },
        )?;
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "visual-mode dispatch keeps transition ordering visible in one state handler"
    )]
    fn visual(
        &mut self,
        editor: &mut Editor,
        state: &mut VisualState,
        key: char,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        if key == '\u{1c}' {
            self.pending_ctrl_bslash = true;
            return Ok(None);
        }
        if self.pending_ctrl_bslash {
            self.pending_ctrl_bslash = false;
            if key == '\u{0e}' {
                // `v_CTRL-\_CTRL-N`: stop Visual mode without executing.
                self.last_visual = Some(state.clone());
                return Ok(Some(Mode::default()));
            }
            return Ok(None);
        }
        if state.prefix == "r" || state.prefix == "r\u{16}" {
            let quoted = state.prefix == "r\u{16}";
            if quoted {
                state.prefix.clear();
            } else if key == '\u{16}' || key == '\u{11}' {
                state.prefix = "r\u{16}".into();
                return Ok(None);
            } else {
                state.prefix.clear();
            }
            if key == '\u{1b}' {
                return Ok(Some(Mode::default()));
            }
            self.visual_replace(editor, state, classify_replace_key(key, quoted))?;
            return Ok(Some(Mode::default()));
        }
        if state.prefix == "g" {
            state.prefix.clear();
            if matches!(key, 'u' | 'U' | '~') {
                let operator = match key {
                    'u' => Operator::Lowercase,
                    'U' => Operator::Uppercase,
                    _ => Operator::ToggleCase,
                };
                return self.finish_visual_operator(editor, state, operator, eval);
            }
            let command = format!("g{key}");
            return Self::extend_visual_motion(editor, state, &command);
        }
        if matches!(state.prefix.as_str(), "f" | "F" | "t" | "T") {
            return self.visual_find(editor, state, key);
        }
        if state.prefix == "i" || state.prefix == "a" {
            return Self::visual_textobject(editor, state, key);
        }
        if key.is_ascii_digit() && (key != '0' || state.count != 0) {
            state.count = append_digit(state.count, key);
            return Ok(None);
        }
        match key {
            '\u{1b}' => {
                self.last_visual = Some(state.clone());
                Ok(Some(Mode::default()))
            }
            'o' | 'O' => {
                if key == 'O' && state.kind == VisualKind::Block {
                    state.swap_columns();
                } else {
                    state.swap_ends();
                }
                let window = context(editor)?.window;
                editor.set_window_cursor(window, state.cursor)?;
                Ok(None)
            }
            'g' | 'f' | 'F' | 't' | 'T' | 'i' | 'a' => {
                state.prefix = key.to_string();
                Ok(None)
            }
            'r' => {
                state.prefix = "r".into();
                Ok(None)
            }
            '/' | '?' => Ok(Some(Mode::Cmdline(CmdlineState {
                kind: CmdlineKind::Search(if key == '/' {
                    SearchDirection::Forward
                } else {
                    SearchDirection::Backward
                }),
                text: String::new(),
                count: state.count,
            }))),
            'J' => {
                let range = state.range();
                let start_lnum = range.start.lnum.min(range.end.lnum);
                let end_lnum = range.start.lnum.max(range.end.lnum);
                self.join_lines(editor, start_lnum, end_lnum)?;
                Ok(Some(Mode::default()))
            }
            'd' | 'x' | 'X' | 'c' | 'y' | '>' | '<' | '=' | 'u' | 'U' | '~' => self
                .finish_visual_operator(
                    editor,
                    state,
                    match key {
                        'x' | 'X' => Operator::Delete,
                        'u' => Operator::Lowercase,
                        'U' => Operator::Uppercase,
                        '~' => Operator::ToggleCase,
                        _ => operator_for(key),
                    },
                    eval,
                ),
            _ => {
                let command = key.to_string();
                Self::extend_visual_motion(editor, state, &command)
            }
        }
    }

    /// Extends the visual selection with a countable motion, then drops the
    /// accumulated count, as every completed visual motion does.
    fn extend_visual_motion(
        editor: &mut Editor,
        state: &mut VisualState,
        command: &str,
    ) -> Result<Option<Mode>, ModeError> {
        let ctx = context(editor)?;
        if let Some(motion) = resolve(
            &ctx.lines,
            ctx.cursor,
            command,
            state.count.max(1),
            option_bool(editor, "startofline", true),
            (ctx.topline, ctx.bottomline),
        ) {
            editor.set_window_cursor(ctx.window, motion.target)?;
            extend_visual(state, motion.target, ctx.cursor);
        }
        state.count = 0;
        Ok(None)
    }
    fn visual_replace(
        &mut self,
        editor: &mut Editor,
        state: &VisualState,
        input: ReplaceInput,
    ) -> Result<(), ModeError> {
        let ctx = context(editor)?;
        if ctx.lines.is_empty() {
            return Ok(());
        }
        let range = state.range();
        let last_lnum = ctx.lines.len();
        let start_lnum = range.start.lnum.min(range.end.lnum).clamp(1, last_lnum);
        let end_lnum = range.start.lnum.max(range.end.lnum).clamp(1, last_lnum);
        let cursor_after = Position {
            lnum: range.start.lnum.clamp(1, last_lnum),
            col: range.start.col,
        };
        let structural_block = range.kind == MotionKind::BlockWise
            && matches!(input, ReplaceInput::TypedCr | ReplaceInput::TypedNl);
        let input = if structural_block {
            input
        } else {
            literal_for_nonblock(input)
        };
        let replacement_scalar = scalar_bytes(input);
        let mut requests = Vec::with_capacity(end_lnum - start_lnum + 1);
        for lnum in start_lnum..=end_lnum {
            let line = &ctx.lines[lnum - 1];
            let (start_col, end_col) = match range.kind {
                MotionKind::BlockWise => (
                    range.start.col.min(line.len()),
                    crate::motion::next_char_boundary(line, range.end.col).min(line.len()),
                ),
                MotionKind::CharacterWise => (
                    if lnum == start_lnum {
                        range.start.col
                    } else {
                        0
                    },
                    if lnum == end_lnum {
                        crate::motion::next_char_boundary(line, range.end.col)
                    } else {
                        line.len()
                    },
                ),
                MotionKind::LineWise => (0, line.len()),
            };
            if start_col >= end_col {
                continue;
            }

            if structural_block {
                requests.push(BufferTextEditRequest {
                    start: ExtmarkPosition::new(lnum - 1, start_col),
                    end: ExtmarkPosition::new(lnum - 1, end_col),
                    replacement: vec![Vec::new(), Vec::new()],
                });
                continue;
            }

            let scalar_count = scalar_count_in_range(line, start_col, end_col);
            let mut replacement =
                Vec::with_capacity(replacement_scalar.len().saturating_mul(scalar_count));
            for _ in 0..scalar_count {
                replacement.extend_from_slice(&replacement_scalar);
            }
            requests.push(BufferTextEditRequest {
                start: ExtmarkPosition::new(lnum - 1, start_col),
                end: ExtmarkPosition::new(lnum - 1, end_col),
                replacement: vec![replacement],
            });
        }

        editor.replace_buffer_texts(
            ctx.buffer,
            ctx.window,
            &requests,
            ctx.cursor,
            cursor_after,
            self.timestamp,
        )?;
        editor.set_window_cursor(ctx.window, cursor_after)?;
        Ok(())
    }
    /// Completes a visual `f`/`F`/`t`/`T` find prefix, extending the
    /// selection to the found target when there is one.
    fn visual_find(
        &mut self,
        editor: &mut Editor,
        state: &mut VisualState,
        key: char,
    ) -> Result<Option<Mode>, ModeError> {
        if key.len_utf8() != 1 {
            state.prefix.clear();
            return Ok(None);
        }
        let find = FindMotion {
            direction: if matches!(state.prefix.as_str(), "f" | "t") {
                FindDirection::Forward
            } else {
                FindDirection::Backward
            },
            till: matches!(state.prefix.as_str(), "t" | "T"),
            target: key as u8,
        };
        let ctx = context(editor)?;
        if let Some(motion) = resolve_find(&ctx.lines, ctx.cursor, find, state.count.max(1)) {
            editor.set_window_cursor(ctx.window, motion.target)?;
            extend_visual(state, motion.target, ctx.cursor);
            self.last_find = Some(find);
        }
        state.prefix.clear();
        state.count = 0;
        Ok(None)
    }

    /// Completes a visual `i`/`a` text-object prefix by selecting the
    /// resolved object's range.
    fn visual_textobject(
        editor: &mut Editor,
        state: &mut VisualState,
        key: char,
    ) -> Result<Option<Mode>, ModeError> {
        let inner = state.prefix == "i";
        let ctx = context(editor)?;
        if let Some(range) =
            textobject::resolve(&ctx.lines, ctx.cursor, inner, key, state.count.max(1))
        {
            state.anchor = range.start;
            state.cursor = range.end;
            state.kind = match range.kind {
                MotionKind::LineWise => VisualKind::Line,
                MotionKind::BlockWise => VisualKind::Block,
                MotionKind::CharacterWise => VisualKind::Character,
            };
            editor.set_window_cursor(ctx.window, state.cursor)?;
        }
        state.prefix.clear();
        state.count = 0;
        Ok(None)
    }

    fn join_lines(
        &mut self,
        editor: &mut Editor,
        start_lnum: usize,
        end_lnum: usize,
    ) -> Result<(), ModeError> {
        if end_lnum <= start_lnum {
            return Ok(());
        }

        let ctx = context(editor)?;
        let joinspaces = matches!(
            editor.options().get_global("joinspaces"),
            Ok(OptionValue::Boolean(true))
        );
        let keep_first_join_col = matches!(
            editor.options().get_global("cpoptions"),
            Ok(OptionValue::String(value)) if value.contains('q')
        );
        let formatoptions = match editor.options().get_buffer(ctx.buffer, "formatoptions") {
            Ok(OptionValue::String(value)) => value.clone(),
            _ => "tcqj".to_owned(),
        };
        let comments = match editor.options().get_buffer(ctx.buffer, "comments") {
            Ok(OptionValue::String(value)) => value.clone(),
            _ => "s1:/*,mb:*,ex:*/,://,b:#,:%,:XCOMM,n:>,fb:-,fb:•".to_owned(),
        };
        let policy = JoinPolicy {
            joinspaces,
            multibyte: formatoptions.contains('M'),
            multibyte_pairs: formatoptions.contains('B'),
        };
        let remove_comments = formatoptions.contains('j');

        let first = &ctx.lines[start_lnum - 1];
        let mut joined = first.clone();
        let mut previous_was_comment =
            remove_comments && scan_comment_line(first, &comments).ends_open;
        let mut last_join_col = first.len();
        let mut last_leading = 0;
        let mut last_suffix_len = 0;
        for lnum in start_lnum + 1..=end_lnum {
            let line = &ctx.lines[lnum - 1];
            let comment = if remove_comments {
                scan_comment_line(line, &comments)
            } else {
                CommentScan::default()
            };
            let comment_leading = if previous_was_comment {
                comment.leading_removal
            } else {
                0
            };
            previous_was_comment = remove_comments && comment.ends_open;
            let leading = comment_leading
                + line[comment_leading..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_whitespace())
                    .count();
            let segment = &line[leading..];
            last_join_col = joined.len();
            let separator_len = join_separator_len(&joined, segment, policy);
            joined.extend(std::iter::repeat_n(b' ', separator_len));
            joined.extend_from_slice(segment);
            if lnum == end_lnum {
                last_leading = leading;
                last_suffix_len = segment.len();
            }
        }

        let cursor_after = Position {
            lnum: start_lnum,
            col: if keep_first_join_col {
                first.len()
            } else {
                last_join_col
            },
        };
        let replacement_end = joined.len() - last_suffix_len;
        editor.replace_buffer_text(
            ctx.buffer,
            &BufferTextEditRequest {
                start: ExtmarkPosition::new(start_lnum - 1, first.len()),
                end: ExtmarkPosition::new(end_lnum - 1, last_leading),
                replacement: vec![joined[first.len()..replacement_end].to_vec()],
            },
            ctx.cursor,
            cursor_after,
            self.timestamp,
        )?;
        editor.set_window_cursor(ctx.window, cursor_after)?;
        Ok(())
    }

    fn finish_visual_operator(
        &mut self,
        editor: &mut Editor,
        state: &VisualState,
        operator: Operator,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        let ctx = context(editor)?;
        self.last_visual = Some(state.clone());
        let result = ops::apply(
            editor,
            OperatorRequest {
                buffer: ctx.buffer,
                window: ctx.window,
                operator,
                range: state.range(),
                register: None,
                timestamp: self.timestamp,
                eval,
            },
        )?;
        Ok(Some(if result.enter_insert {
            Mode::Insert(InsertState)
        } else {
            Mode::default()
        }))
    }
    /// Shared `CTRL-\` second-key arm for Insert and Replace (`insert.c:640`).
    /// Pure state machine: no buffer writes. `Exit` ends the sequence with a
    /// mode change; `Consumed` ends it with nothing further; `Literal` means
    /// the caller must write the literal backslash byte first (upstream
    /// `vungetc` + `s->c = Ctrl_BSL`, `insert.c:651-653`), then process the
    /// follow key as ordinary input with FRESH editor state.
    fn ctrl_bslash_arm(&mut self, key: char) -> CtrlBslash {
        if key == '\u{1c}' {
            self.pending_ctrl_bslash = true;
            return CtrlBslash::Consumed;
        }
        if self.pending_ctrl_bslash {
            self.pending_ctrl_bslash = false;
            if key == '\u{0e}' {
                return CtrlBslash::Exit;
            }
            return CtrlBslash::Literal;
        }
        CtrlBslash::None
    }

    fn insert(
        &mut self,
        editor: &mut Editor,
        _state: &mut InsertState,
        key: char,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        if key == '\u{0f}' {
            self.one_normal_command = true;
            return Ok(Some(Mode::default()));
        }
        let ctx = cursor_context(editor)?;
        match self.ctrl_bslash_arm(key) {
            CtrlBslash::Exit => {
                insert::normal_cursor(editor, ctx.window, ctx.cursor)?;
                return Ok(Some(Mode::default()));
            }
            CtrlBslash::Consumed => return Ok(None),
            CtrlBslash::Literal => {
                // Literal backslash first, then the follow key below — with a
                // FRESH snapshot, since the write moved the cursor.
                let timestamp = self.timestamp;
                insert::insert_char(
                    editor, ctx.buffer, ctx.window, ctx.cursor, '\u{1c}', timestamp,
                )?;
                let ctx = cursor_context(editor)?;
                return self.insert_plain(editor, &ctx, key, eval);
            }
            CtrlBslash::None => {}
        }
        let ctx = cursor_context(editor)?;
        self.insert_plain(editor, &ctx, key, eval)
    }

    /// Ordinary Insert input once the `CTRL-\` arm has run.
    fn insert_plain(
        &mut self,
        editor: &mut Editor,
        ctx: &CursorContext,
        key: char,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        match key {
            '\u{1b}' => {
                insert::normal_cursor(editor, ctx.window, ctx.cursor)?;
                Ok(Some(Mode::default()))
            }
            '\n' | '\r' => {
                insert::newline(
                    editor,
                    ctx.buffer,
                    ctx.window,
                    ctx.cursor,
                    self.timestamp,
                    eval,
                )?;
                Ok(None)
            }
            '\u{8}' | '\u{7f}' => {
                insert::backspace(
                    editor,
                    ctx.buffer,
                    ctx.window,
                    ctx.cursor,
                    option_contains(editor, "backspace", "eol", true),
                    self.timestamp,
                )?;
                Ok(None)
            }
            '\u{9}' => {
                insert::insert_tab(editor, ctx.buffer, ctx.window, ctx.cursor, self.timestamp)?;
                Ok(None)
            }
            '\u{4}' => {
                insert::adjust_indent(
                    editor,
                    ctx.buffer,
                    ctx.window,
                    ctx.cursor,
                    false,
                    self.timestamp,
                )?;
                Ok(None)
            }
            '\u{14}' => {
                insert::adjust_indent(
                    editor,
                    ctx.buffer,
                    ctx.window,
                    ctx.cursor,
                    true,
                    self.timestamp,
                )?;
                Ok(None)
            }
            '\u{6}' => {
                insert::force_reindent(
                    editor,
                    ctx.buffer,
                    ctx.window,
                    ctx.cursor,
                    self.timestamp,
                    eval,
                )?;
                Ok(None)
            }
            ch if !ch.is_control() => {
                insert::insert_char(
                    editor,
                    ctx.buffer,
                    ctx.window,
                    ctx.cursor,
                    ch,
                    self.timestamp,
                )?;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Replace mode: like Insert, but a typed scalar overwrites the existing
    /// one under the cursor (`edit.c` replace semantics).
    fn replace_insert(
        &mut self,
        editor: &mut Editor,
        _state: &mut ReplaceState,
        key: char,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        match self.ctrl_bslash_arm(key) {
            CtrlBslash::Exit => {
                let ctx = context(editor)?;
                insert::normal_cursor(editor, ctx.window, ctx.cursor)?;
                return Ok(Some(Mode::default()));
            }
            CtrlBslash::Consumed => return Ok(None),
            CtrlBslash::Literal => {
                // Literal backslash through Replace's own overwrite path, then
                // the follow key below — both with FRESH snapshots.
                self.replace_scalar(editor, '\u{1c}')?;
                let ctx = cursor_context(editor)?;
                return self.replace_plain(editor, &ctx, key, eval);
            }
            CtrlBslash::None => {}
        }
        let ctx = cursor_context(editor)?;
        self.replace_plain(editor, &ctx, key, eval)
    }

    /// One overwriting scalar write at the cursor (Replace's typed path).
    fn replace_scalar(&mut self, editor: &mut Editor, ch: char) -> Result<(), ModeError> {
        let ctx = cursor_context(editor)?;
        self.replace_scalar_with(editor, &ctx, ch)
    }

    /// Scalar write against an already-fetched snapshot.
    fn replace_scalar_with(
        &mut self,
        editor: &mut Editor,
        ctx: &CursorContext,
        ch: char,
    ) -> Result<(), ModeError> {
        let owned_line = editor
            .buffer(ctx.buffer)?
            .text()?
            .line(ctx.cursor.lnum)
            .map_err(BufferStateError::from)?;
        let line = owned_line.as_slice();
        let col = ctx.cursor.col.min(line.len());
        let replaced = if col < line.len() {
            scalar_width(line[col])
        } else {
            0
        };
        let mut encoded = [0u8; 4];
        let bytes = ch.encode_utf8(&mut encoded).as_bytes().to_vec();
        let after = Position {
            lnum: ctx.cursor.lnum,
            col: col + bytes.len(),
        };
        editor.replace_buffer_text(
            ctx.buffer,
            &BufferTextEditRequest {
                start: ExtmarkPosition::new(ctx.cursor.lnum - 1, col),
                end: ExtmarkPosition::new(ctx.cursor.lnum - 1, col + replaced),
                replacement: vec![bytes],
            },
            ctx.cursor,
            after,
            self.timestamp,
        )?;
        editor.set_window_cursor(ctx.window, after)?;
        Ok(())
    }

    fn replace_plain(
        &mut self,
        editor: &mut Editor,
        ctx: &CursorContext,
        key: char,
        eval: &mut dyn ExprEval,
    ) -> Result<Option<Mode>, ModeError> {
        match key {
            '\u{1b}' => {
                insert::normal_cursor(editor, ctx.window, ctx.cursor)?;
                Ok(Some(Mode::default()))
            }
            '\n' | '\r' => {
                insert::newline(
                    editor,
                    ctx.buffer,
                    ctx.window,
                    ctx.cursor,
                    self.timestamp,
                    eval,
                )?;
                Ok(None)
            }
            ch if !ch.is_control() => {
                self.replace_scalar_with(editor, ctx, ch)?;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Replays a macro register (`do_execreg`): its bytes re-enter the
    /// typeahead as mapped input, `count` times.
    fn exec_register(editor: &mut Editor, name: char, count: usize) -> Result<(), ModeError> {
        let bytes = editor
            .registers()
            .get(name)
            .ok()
            .flatten()
            .map(RegisterContent::to_bytes);
        let Some(bytes) = bytes.filter(|bytes| !bytes.is_empty()) else {
            beep_flush(editor);
            return Ok(());
        };
        let keys = Keys::from_encoded(bytes)?;
        let flags = TypeaheadFlags {
            mapped: true,
            ..TypeaheadFlags::default()
        };
        for _ in 0..count.max(1) {
            editor.typeahead_mut().append(&keys, flags);
        }
        Ok(())
    }

    fn cmdline(
        &mut self,
        editor: &mut Editor,
        state: &mut CmdlineState,
        key: char,
    ) -> Result<Option<Mode>, ModeError> {
        if self.pending_cmdline_literal {
            self.pending_cmdline_literal = false;
            state.text.push(key);
            return Ok(None);
        }
        if key == '\u{16}' {
            self.pending_cmdline_literal = true;
            return Ok(None);
        }
        if key == '\u{1c}' {
            self.pending_ctrl_bslash = true;
            return Ok(None);
        }
        if self.pending_ctrl_bslash {
            self.pending_ctrl_bslash = false;
            if key == '\u{0e}' {
                // `c_CTRL-\_CTRL-N`: leave the command line for Normal mode.
                return Ok(Some(Mode::default()));
            }
            return Ok(None);
        }
        match key {
            '\u{1b}' => Ok(Some(Mode::default())),
            '\u{8}' | '\u{7f}' => {
                state.text.pop();
                Ok(None)
            }
            '\n' | '\r' => {
                match state.kind {
                    CmdlineKind::Search(direction) => {
                        let ctx = context(editor)?;
                        let result = self.search.search(
                            &ctx.lines,
                            ctx.cursor,
                            &state.text,
                            direction,
                            state.count.max(1),
                            option_bool(editor, "wrapscan", true),
                        )?;
                        push_jump(editor, ctx.buffer, ctx.cursor);
                        editor.set_window_cursor(ctx.window, result.target)?;
                    }
                    CmdlineKind::Ex => {
                        let command = std::mem::take(&mut state.text);
                        self.cmdline_history.push(command.clone());
                        self.completed_ex_command = Some(command);
                    }
                }
                Ok(Some(Mode::default()))
            }
            ch if !ch.is_control() => {
                state.text.push(ch);
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn move_command(
        editor: &mut Editor,
        command: &str,
        count: usize,
        visual: bool,
    ) -> Result<(), ModeError> {
        let ctx = context(editor)?;
        let Some(mut motion) = resolve(
            &ctx.lines,
            ctx.cursor,
            command,
            count,
            option_bool(editor, "startofline", true),
            (ctx.topline, ctx.bottomline),
        ) else {
            beep_flush(editor);
            return Ok(());
        };
        if motion.is_jump && !visual {
            push_jump(editor, ctx.buffer, ctx.cursor);
        }
        let keep_curswant = if motion.keep_curswant {
            editor.window(ctx.window).ok().map(|state| {
                if state.set_curswant {
                    i64::try_from(ctx.cursor.col).unwrap_or(i64::MAX)
                } else {
                    state.curswant
                }
            })
        } else {
            None
        };
        if let Some(curswant) = keep_curswant {
            let lnum = motion.target.lnum;
            let line = ctx.lines.get(lnum.saturating_sub(1));
            let col = line.map_or(0, |line| {
                let want = usize::try_from(curswant).unwrap_or(0);
                want.min(crate::motion::prev_char_boundary(line, line.len()))
            });
            motion.target.col = col;
        }
        editor.set_window_cursor(ctx.window, motion.target)?;
        if let Some(curswant) = keep_curswant
            && let Ok(state) = editor.window_mut(ctx.window)
        {
            state.curswant = curswant;
            state.set_curswant = false;
        }

        if matches!(command, "gd" | "gD") {
            let _ = editor.fold_open(
                ctx.buffer,
                crate::fold::Position::new(motion.target.lnum.saturating_sub(1), 0),
            );
        }
        Ok(())
    }

    fn ident_search(
        editor: &mut Editor,
        prefix: &str,
        key: char,
        count: usize,
    ) -> Result<(), ModeError> {
        let ctx = context(editor)?;
        let Some(ident) = crate::motion::ident_under(&ctx.lines, ctx.cursor) else {
            return Err(ModeError::Vim(
                "E349",
                "No identifier under cursor".to_owned(),
            ));
        };
        let ident = ident.to_vec();
        let kind = if matches!(key, 'd' | 'D' | '\u{4}') {
            crate::include_search::IdentSearchKind::Define
        } else {
            crate::include_search::IdentSearchKind::Any
        };
        let action = if prefix == "\u{17}" {
            crate::include_search::IdentSearchAction::Split
        } else if key.is_uppercase() {
            crate::include_search::IdentSearchAction::List
        } else if matches!(key, '\t' | '\u{4}') {
            crate::include_search::IdentSearchAction::Goto
        } else {
            crate::include_search::IdentSearchAction::Show
        };
        let start_lnum = if prefix == "]" {
            ctx.cursor.lnum.saturating_add(1)
        } else {
            1
        };
        let relative_to = editor.buffer(ctx.buffer).ok().and_then(|state| {
            let name = state.name().to_string_lossy();
            let path = std::path::Path::new(name.as_ref());
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(std::path::Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok())
        });

        let hits = crate::include_search::collect_hits_with_includes(
            &ctx.lines,
            &ident,
            true,
            kind,
            start_lnum,
            ctx.lines.len(),
            relative_to.as_deref(),
        );

        crate::include_search::apply(editor, &hits, action, count, ctx.cursor.lnum, kind)
            .map_err(|error| ModeError::Vim(error.code, error.message))
    }

    fn jump_ident_tag(editor: &mut Editor, count: usize) -> Result<(), ModeError> {
        let ctx = context(editor)?;
        let Some(ident) = crate::motion::ident_under(&ctx.lines, ctx.cursor) else {
            return Err(ModeError::Vim(
                "E349",
                "No identifier under cursor".to_owned(),
            ));
        };
        let needle = String::from_utf8_lossy(ident).into_owned();
        let tags_option = match editor.options().get_global("tags") {
            Ok(OptionValue::String(value)) => value.clone(),
            _ => "./tags;,tags".to_owned(),
        };
        let matches = crate::tags::lookup(&crate::script::RealFileIO, &tags_option, &needle)
            .map_err(|(code, message)| ModeError::Vim(code, message))?;
        let index = count.saturating_sub(1);

        let Some(chosen) = matches.get(index) else {
            return Err(ModeError::Vim("E426", format!("Tag not found: {needle}")));
        };
        let bytes = std::fs::read(&chosen.filename).unwrap_or_default();
        let text = ox_text::Buffer::from_bytes(&bytes).unwrap_or_else(|_| ox_text::Buffer::new());
        let handle = editor
            .create_buffer_with(text, true)
            .map_err(|error| ModeError::Vim("E948", error.to_string()))?;
        if let Ok(buffer) = editor.buffer_mut(handle) {
            buffer.set_name(ox_types::OxStr::from(
                chosen.filename.to_string_lossy().as_ref(),
            ));
            buffer.mark_saved();
        }
        editor
            .set_current_buffer(handle, crate::BufferRelease::KeepLoaded)
            .map_err(|error| ModeError::Vim("E948", error.to_string()))?;
        let lines = (1..=editor.buffer(handle)?.text()?.line_count())
            .filter_map(|lnum| editor.buffer(handle).ok()?.text().ok()?.line(lnum).ok())
            .collect::<Vec<_>>();
        let (target, _) = crate::tags::cmd_target(&lines, &chosen.cmd)
            .unwrap_or((ox_text::Position { lnum: 1, col: 0 }, false));
        editor.set_window_cursor(ctx.window, target)?;
        Ok(())
    }
    fn goto_file_under_cursor(editor: &mut Editor) -> Result<(), ModeError> {
        // `nv_gotofile` refuses to leave a 'winfixbuf' window and offers no
        // bang escape (`check_can_set_curbuf_disabled`, normal.c:3871).
        if editor.current_window_fixed_to_buffer() {
            return Err(ModeError::Vim(
                "E1513",
                "Cannot switch buffer. 'winfixbuf' is enabled".to_owned(),
            ));
        }
        let ctx = context(editor)?;
        let line = editor
            .buffer(ctx.buffer)?
            .text()?
            .line(ctx.cursor.lnum)
            .map_err(|error| ModeError::Vim("E474", error.to_string()))?;
        let is_file_byte = |byte: u8| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'~')
        };
        let mut start = ctx.cursor.col.min(line.len());
        while start > 0 && is_file_byte(line[start - 1]) {
            start -= 1;
        }
        let mut end = ctx.cursor.col.min(line.len());
        while end < line.len() && is_file_byte(line[end]) {
            end += 1;
        }
        if start == end {
            return Err(ModeError::Vim(
                "E447",
                "Can't find file under cursor".to_owned(),
            ));
        }
        let path =
            std::path::PathBuf::from(String::from_utf8_lossy(&line[start..end]).into_owned());
        let bytes = std::fs::read(&path).map_err(|error| {
            ModeError::Vim(
                "E447",
                format!("Can't find file {}: {error}", path.display()),
            )
        })?;
        let text = ox_text::Buffer::from_bytes(&bytes)
            .map_err(|error| ModeError::Vim("E474", error.to_string()))?;
        let handle = editor
            .create_buffer_with(text, true)
            .map_err(|error| ModeError::Vim("E948", error.to_string()))?;
        editor
            .buffer_mut(handle)?
            .set_name(ox_types::OxStr::from(path.to_string_lossy().as_ref()));
        editor.set_current_buffer(handle, crate::BufferRelease::KeepLoaded)?;
        Ok(())
    }

    fn preview_ident_tag(editor: &mut Editor, count: usize) -> Result<(), ModeError> {
        let ctx = context(editor)?;
        let Some(ident) = crate::motion::ident_under(&ctx.lines, ctx.cursor) else {
            return Err(ModeError::Vim(
                "E349",
                "No identifier under cursor".to_owned(),
            ));
        };
        let needle = String::from_utf8_lossy(ident).into_owned();
        let tags_option = match editor.options().get_global("tags") {
            Ok(OptionValue::String(value)) => value.clone(),
            _ => "./tags;,tags".to_owned(),
        };
        let matches = crate::tags::lookup(&crate::script::RealFileIO, &tags_option, &needle)
            .map_err(|(code, message)| ModeError::Vim(code, message))?;
        let Some(chosen) = matches.first() else {
            return Err(ModeError::Vim("E426", format!("Tag not found: {needle}")));
        };

        let origin_window = ctx.window;
        let bytes = std::fs::read(&chosen.filename).unwrap_or_default();
        let text = ox_text::Buffer::from_bytes(&bytes).unwrap_or_else(|_| ox_text::Buffer::new());
        let handle = editor
            .create_buffer_with(text, true)
            .map_err(|error| ModeError::Vim("E948", error.to_string()))?;
        if let Ok(buffer) = editor.buffer_mut(handle) {
            buffer.set_name(ox_types::OxStr::from(
                chosen.filename.to_string_lossy().as_ref(),
            ));
            buffer.mark_saved();
        }
        let tab = editor
            .current_tabpage()
            .ok_or(EditorError::UnknownTabpage(ox_types::TabHandle::CURRENT))?;
        let created = editor
            .split_above(tab, origin_window, handle, true)
            .map_err(|error| ModeError::Vim("E36", error.to_string()))?;
        editor
            .options_mut()
            .set_window(created, "previewwindow", OptionValue::Boolean(true))
            .map_err(|error| ModeError::Vim("E474", error.to_string()))?;
        let lines = (1..=editor.buffer(handle)?.text()?.line_count())
            .filter_map(|lnum| editor.buffer(handle).ok()?.text().ok()?.line(lnum).ok())
            .collect::<Vec<_>>();
        let (target, _) = crate::tags::cmd_target(&lines, &chosen.cmd)
            .unwrap_or((ox_text::Position { lnum: 1, col: 0 }, false));
        editor.set_window_cursor(created, target)?;

        if count > 1 {
            let _ = editor.set_window_height(created, count);
        }
        editor
            .set_current_window(origin_window)
            .map_err(|error| ModeError::Vim("E36", error.to_string()))?;
        Ok(())
    }

    /// `searchc` (`search.c`): reports whether the target was found, so the
    /// caller can `clearopbeep` when it was not.
    fn move_find(
        editor: &mut Editor,
        find: FindMotion,
        count: usize,
        _visual: bool,
    ) -> Result<bool, ModeError> {
        let ctx = context(editor)?;
        let Some(motion) = resolve_find(&ctx.lines, ctx.cursor, find, count) else {
            return Ok(false);
        };
        editor.set_window_cursor(ctx.window, motion.target)?;
        Ok(true)
    }
    fn repeat_search(
        &mut self,
        editor: &mut Editor,
        opposite: bool,
        count: usize,
    ) -> Result<(), ModeError> {
        let ctx = context(editor)?;
        let result = self.search.repeat(
            &ctx.lines,
            ctx.cursor,
            opposite,
            count,
            option_bool(editor, "wrapscan", true),
        )?;
        push_jump(editor, ctx.buffer, ctx.cursor);
        editor.set_window_cursor(ctx.window, result.target)?;
        Ok(())
    }
    /// `<c-w>` window commands from Normal mode (`normal.c:nv_window`).
    /// Handles split, close, and directional navigation.
    fn wincmd(editor: &mut Editor, key: char) {
        let Some(tab) = editor.current_tabpage() else {
            return;
        };
        let Some(current) = editor.current_window() else {
            return;
        };
        match key {
            'v' | 's' => {
                let Some(buffer) = editor.current_buffer() else {
                    return;
                };
                let result = if key == 'v' {
                    editor.split_left(tab, current, buffer, true)
                } else {
                    editor.split_above(tab, current, buffer, true)
                };
                if let Ok(created) = result {
                    let _ = editor.set_current_window(created);
                }
            }
            'c' | 'o' => {
                let windows = editor.tabpage_windows(tab).unwrap_or_default();
                if windows.len() <= 1 {
                    // E444: cannot close last window — silently ignore in mode
                    return;
                }
                let _ = editor.close_window(tab, current, true);
            }
            'h' | 'j' | 'k' | 'l' => {
                let windows = editor.tabpage_windows(tab).unwrap_or_default();
                if let Some(next) =
                    crate::excmd_exec::directional_window(editor, current, &windows, key)
                {
                    let _ = editor.set_current_window(next);
                }
            }
            'w' | 'W' => {
                let windows = editor.tabpage_windows(tab).unwrap_or_default();
                if windows.len() <= 1 {
                    return;
                }
                let idx = windows.iter().position(|w| *w == current);
                let next = if key == 'w' {
                    idx.and_then(|i| windows.get((i + 1) % windows.len()).copied())
                } else {
                    idx.and_then(|i| {
                        windows
                            .get((i + windows.len() - 1) % windows.len())
                            .copied()
                    })
                };
                if let Some(next) = next {
                    let _ = editor.set_current_window(next);
                }
            }
            'p' => {
                let windows = editor.tabpage_windows(tab).unwrap_or_default();
                if let Some(prev) = editor.previous_window().filter(|w| windows.contains(w)) {
                    let _ = editor.set_current_window(prev);
                }
            }
            _ => {}
        }
    }
    fn advance_insert_cursor(editor: &mut Editor, line_end: bool) -> Result<(), ModeError> {
        let ctx = cursor_context(editor)?;
        let owned_line = editor
            .buffer(ctx.buffer)?
            .text()?
            .line(ctx.cursor.lnum)
            .map_err(BufferStateError::from)?;
        let line = owned_line.as_slice();
        let col = if line_end {
            line.len()
        } else {
            next_boundary(line, ctx.cursor.col)
        };
        editor.set_window_cursor(
            ctx.window,
            Position {
                lnum: ctx.cursor.lnum,
                col,
            },
        )?;
        Ok(())
    }
    fn replace_chars(
        &mut self,
        editor: &mut Editor,
        count: usize,
        input: ReplaceInput,
    ) -> Result<(), ModeError> {
        let ctx = context(editor)?;
        let line = &ctx.lines[ctx.cursor.lnum - 1];
        let Some(end_col) = inclusive_scalar_end(line, ctx.cursor.col, count) else {
            return Ok(());
        };
        let (replacement, after) = match input {
            ReplaceInput::TypedCr | ReplaceInput::TypedNl => {
                let prefix = line[..ctx.cursor.col.min(line.len())].to_vec();
                let opts = indent::IndentOptions::capture(editor, ctx.buffer);
                let indent_bytes = indent::smart_newline_indent(&prefix, false, &opts);
                let after = Position {
                    lnum: ctx.cursor.lnum + 1,
                    col: indent_bytes.len().saturating_sub(1),
                };
                (vec![Vec::new(), indent_bytes], after)
            }
            _ => {
                let bytes = scalar_bytes(input);
                let mut replacement = Vec::with_capacity(bytes.len().saturating_mul(count));
                for _ in 0..count {
                    replacement.extend_from_slice(&bytes);
                }
                (vec![replacement], ctx.cursor)
            }
        };
        let request = BufferTextEditRequest {
            start: ExtmarkPosition::new(ctx.cursor.lnum - 1, ctx.cursor.col),
            end: ExtmarkPosition::new(ctx.cursor.lnum - 1, end_col + 1),
            replacement,
        };
        editor.replace_buffer_text(ctx.buffer, &request, ctx.cursor, after, self.timestamp)?;
        editor.set_window_cursor(ctx.window, after)?;
        Ok(())
    }

    fn adjust_number(&mut self, editor: &mut Editor, delta: i64) -> Result<(), ModeError> {
        let ctx = context(editor)?;
        let line = ctx.lines[ctx.cursor.lnum - 1].clone();
        let Some((start, end, rendered)) = adjust_number_span(&line, ctx.cursor.col, delta) else {
            return Ok(());
        };
        if rendered.as_slice() == &line[start..end] {
            return Ok(());
        }
        let cursor = Position {
            lnum: ctx.cursor.lnum,
            col: start.saturating_add(rendered.len().saturating_sub(1)),
        };
        let request = BufferTextEditRequest {
            start: ExtmarkPosition::new(ctx.cursor.lnum - 1, start),
            end: ExtmarkPosition::new(ctx.cursor.lnum - 1, end),
            replacement: vec![rendered],
        };
        editor.replace_buffer_text(ctx.buffer, &request, ctx.cursor, cursor, self.timestamp)?;
        editor.set_window_cursor(ctx.window, cursor)?;
        Ok(())
    }

    fn open_line(
        &self,
        editor: &mut Editor,
        below: bool,
        eval: &mut dyn ExprEval,
    ) -> Result<(), ModeError> {
        let (buffer, window, mut cursor) = {
            let tab = editor
                .current_tabpage()
                .ok_or(EditorError::UnknownTabpage(ox_types::TabHandle::CURRENT))?;
            let tabpage = editor.tabpage(tab)?;
            let window = tabpage.current_window();
            let state = editor.window(window)?;
            (state.buffer, window, state.cursor)
        };
        let count = {
            let text = editor.buffer(buffer)?.text()?;
            let count = text.line_count();
            let valid = cursor.lnum.clamp(1, count.max(1));
            if valid != cursor.lnum {
                cursor.lnum = valid;
                editor.set_window_cursor(window, cursor)?;
            }
            count
        };
        let source = {
            let text = editor.buffer(buffer)?.text()?;
            text.line(cursor.lnum).map_err(BufferStateError::from)?
        };
        let opts = indent::IndentOptions::capture(editor, buffer);
        let smart = indent::smart_source_trigger(&source, !below, &opts);
        let mut indent_bytes = indent::smart_newline_indent(&source, smart, &opts);
        let after_line = if below {
            cursor.lnum
        } else {
            cursor.lnum.saturating_sub(1)
        };
        let new_lnum = after_line + 1;
        // The staged overlay only feeds indentexpr/lisp/cindent; with every
        // method off, skip the whole-buffer materialization and clone on each
        // `o`/`O`.
        if !(opts.indentexpr.is_empty()
            && !opts.flags.contains(indent::IndentFlags::LISP)
            && !opts.flags.contains(indent::IndentFlags::CINDENT))
        {
            let text = editor.buffer(buffer)?.text()?;
            let mut lines = (1..=count)
                .map(|lnum| text.line(lnum))
                .collect::<Result<Vec<_>, _>>()
                .map_err(BufferStateError::from)?;
            lines.insert(new_lnum - 1, indent_bytes.clone());
            let trigger = if below {
                CinTrigger::OpenForward
            } else {
                CinTrigger::OpenBackward
            };
            let context = indent::IndentEvalContext::new(editor, buffer, &lines);
            if let Some(whitespace) =
                indent::fix_line_indent(&context, new_lnum, trigger, &opts, eval)?
            {
                indent_bytes = whitespace;
            }
        }
        let pos = Position {
            lnum: new_lnum,
            col: indent_bytes.len(),
        };
        editor.append_buffer_lines(buffer, after_line, &[indent_bytes], cursor, self.timestamp)?;
        editor.set_window_cursor(window, pos)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct JoinPolicy {
    joinspaces: bool,
    multibyte: bool,
    multibyte_pairs: bool,
}

fn join_separator_len(left: &[u8], right: &[u8], policy: JoinPolicy) -> usize {
    let (Some(&last), Some(&first)) = (left.last(), right.first()) else {
        return 0;
    };
    if first == b')' || last == b'\t' {
        return 0;
    }

    let left_char = last_codepoint(left);
    let right_char = first_codepoint(right);
    if policy.multibyte && (left_char >= 0x100 || right_char >= 0x100) {
        return 0;
    }
    if policy.multibyte_pairs
        && (right_char >= 0x100 || unicode_eats_join_space(left_char))
        && (left_char >= 0x100 || unicode_eats_join_space(right_char))
    {
        return 0;
    }

    if last.is_ascii_whitespace() {
        return usize::from(
            policy.joinspaces
                && last == b' '
                && left
                    .get(left.len().saturating_sub(2))
                    .is_some_and(|byte| matches!(byte, b'.' | b'?' | b'!')),
        );
    }
    1 + usize::from(policy.joinspaces && matches!(last, b'.' | b'?' | b'!'))
}

fn first_codepoint(bytes: &[u8]) -> u32 {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.chars().next())
        .map_or_else(|| u32::from(bytes[0]), u32::from)
}

fn last_codepoint(bytes: &[u8]) -> u32 {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.chars().next_back())
        .map_or_else(|| u32::from(bytes[bytes.len() - 1]), u32::from)
}

fn unicode_eats_join_space(codepoint: u32) -> bool {
    matches!(
        codepoint,
        0x2000..=0x206f
            | 0x2e00..=0x2e7f
            | 0x3000..=0x303f
            | 0xff01..=0xff0f
            | 0xff1a..=0xff20
            | 0xff3b..=0xff40
            | 0xff5b..=0xff65
    )
}

#[derive(Clone, Copy)]
struct CommentPart<'a> {
    flags: &'a str,
    leader: &'a [u8],
}

fn comment_parts(value: &str) -> impl Iterator<Item = CommentPart<'_>> {
    value.split(',').filter_map(|part| {
        let (flags, leader) = part.split_once(':')?;
        Some(CommentPart {
            flags,
            leader: leader.as_bytes(),
        })
    })
}

fn leader_match_len(line: &[u8], offset: usize, part: CommentPart<'_>) -> Option<usize> {
    if part.leader.is_empty() {
        return None;
    }
    let mut leader = part.leader;
    if leader[0].is_ascii_whitespace() {
        if offset == 0 || !line[offset - 1].is_ascii_whitespace() {
            return None;
        }
        leader = &leader[leader
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count()..];
    }
    if !line.get(offset..)?.starts_with(leader) {
        return None;
    }
    let end = offset + leader.len();
    if part.flags.contains('b')
        && line
            .get(end)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        return None;
    }
    if part.flags.contains('m')
        && line[..offset]
            .iter()
            .any(|byte| !byte.is_ascii_whitespace())
    {
        return None;
    }
    Some(leader.len())
}

fn leading_comment_part<'a>(
    line: &[u8],
    offset: usize,
    comments: &'a str,
) -> Option<(CommentPart<'a>, usize)> {
    let mut middle = None;
    for part in comment_parts(comments) {
        let Some(len) = leader_match_len(line, offset, part) else {
            continue;
        };
        if part.flags.contains('m') {
            middle.get_or_insert((part, len));
            continue;
        }
        if let Some((_, middle_len)) = middle {
            if part.flags.contains('e') && len > middle_len {
                return Some((part, len));
            }
            return middle;
        }
        return Some((part, len));
    }
    middle
}
#[derive(Clone, Copy, Default)]
struct CommentScan {
    leading_removal: usize,
    ends_open: bool,
}

fn scan_comment_line(line: &[u8], comments: &str) -> CommentScan {
    let leading = line
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    let mut scan = CommentScan::default();
    for offset in 0..line.len() {
        let Some((part, len)) = leading_comment_part(line, offset, comments) else {
            continue;
        };
        if offset == leading && !part.flags.contains('e') {
            let mut end = offset + len;
            while line.get(end).is_some_and(u8::is_ascii_whitespace) {
                end += 1;
            }
            scan.leading_removal = end;
        }
        scan.ends_open = !part.flags.contains('e');
    }
    scan
}

/// Re-encodes one decoded key into the internal byte form (`ins_typebuf`).
fn encoded_key(key: Key) -> Vec<u8> {
    match key {
        Key::Byte(0) => vec![K_SPECIAL, KS_ZERO, KE_FILLER],
        Key::Byte(K_SPECIAL) => vec![K_SPECIAL, KS_SPECIAL, KE_FILLER],
        Key::Byte(byte) => vec![byte],
        Key::Special(second, third) => vec![K_SPECIAL, second, third],
    }
}

/// UTF-8 continuation-aware width of the scalar starting at `byte`.
fn scalar_width(byte: u8) -> usize {
    match byte {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// `beep_flush` (`input.c:523-529`): an error in Normal mode discards the
/// mapped run at the front of the typeahead. Records `called_vim_beep`.
fn beep_flush(editor: &mut Editor) {
    editor.beep();
    editor.typeahead_mut().flush_mapped();
}

fn map_mode(mode: &Mode) -> MapMode {
    match mode {
        Mode::Normal(_) => MapMode::Normal,
        Mode::Insert(_) | Mode::Replace(_) => MapMode::Insert,
        Mode::Visual(_) => MapMode::Visual,
        Mode::Cmdline(_) => MapMode::CommandLine,
        Mode::OperatorPending(_) => MapMode::OperatorPending,
    }
}

/// Extends a visual selection with a resolved motion target, keeping the
/// wanted block column on vertical motions (virtual edges on short lines).
fn extend_visual(state: &mut VisualState, target: Position, from: Position) {
    if state.kind == VisualKind::Block {
        state.extend_block(target, from);
    } else {
        state.extend(target);
    }
}

/// Snapshots the editor state one mode operation reads.
///
/// The window cursor is validated first, the way `check_cursor_lnum`
/// (`cursor.c`) validates it before upstream runs a normal-mode command: a
/// window keeps its cursor when its buffer is replaced or shortened, so
/// `w_cursor.lnum` can point past the last line, and every caller below
/// indexes `lines` with it.
fn cursor_context(editor: &mut Editor) -> Result<CursorContext, ModeError> {
    let tab = editor
        .current_tabpage()
        .ok_or(EditorError::UnknownTabpage(ox_types::TabHandle::CURRENT))?;
    let tabpage = editor.tabpage(tab)?;
    let window = tabpage.current_window();
    let state = editor.window(window)?;
    let buffer = state.buffer;
    let mut cursor = state.cursor;
    let text = editor.buffer(buffer)?.text()?;
    let line_count = text.line_count();
    let valid = cursor.lnum.clamp(1, line_count.max(1));
    if valid != cursor.lnum {
        cursor.lnum = valid;
        editor.set_window_cursor(window, cursor)?;
    }
    Ok(CursorContext {
        buffer,
        window,
        cursor,
    })
}

fn context(editor: &mut Editor) -> Result<Context, ModeError> {
    let tab = editor
        .current_tabpage()
        .ok_or(EditorError::UnknownTabpage(ox_types::TabHandle::CURRENT))?;
    let tabpage = editor.tabpage(tab)?;
    let window = tabpage.current_window();
    let height = tabpage
        .layout()
        .window_geometry(window)
        .map_err(EditorError::from)?
        .height;
    let state = editor.window(window)?;
    let buffer = state.buffer;
    let mut cursor = state.cursor;
    let topline = state.topline;
    let text = editor.buffer(buffer)?.text()?;
    let lines = (1..=text.line_count())
        .map(|lnum| text.line(lnum))
        .collect::<Result<Vec<_>, _>>()
        .map_err(BufferStateError::from)?;
    let valid = cursor.lnum.clamp(1, lines.len().max(1));
    if valid != cursor.lnum {
        cursor.lnum = valid;
        editor.set_window_cursor(window, cursor)?;
    }
    let bottomline = topline
        .saturating_add(height.saturating_sub(1))
        .min(lines.len().max(1));
    Ok(Context {
        buffer,
        window,
        cursor,
        lines,
        topline,
        bottomline,
    })
}
fn append_digit(value: usize, key: char) -> usize {
    value
        .saturating_mul(10)
        .saturating_add((key as u8).saturating_sub(b'0') as usize)
}
fn operator_for(key: char) -> Operator {
    match key {
        'd' => Operator::Delete,
        'c' => Operator::Change,
        '>' => Operator::Indent,
        '<' => Operator::Unindent,
        '=' => Operator::Format,
        'u' => Operator::Lowercase,
        'U' => Operator::Uppercase,
        '~' => Operator::ToggleCase,
        _ => Operator::Yank,
    }
}
fn operator_key(operator: Operator) -> char {
    match operator {
        Operator::Delete => 'd',
        Operator::Change => 'c',
        Operator::Yank => 'y',
        Operator::Indent => '>',
        Operator::Unindent => '<',
        Operator::Format => '=',
        Operator::Lowercase => 'u',
        Operator::Uppercase => 'U',
        Operator::ToggleCase => '~',
    }
}
fn reverse_find(direction: FindDirection) -> FindDirection {
    match direction {
        FindDirection::Forward => FindDirection::Backward,
        FindDirection::Backward => FindDirection::Forward,
    }
}
fn next_boundary(line: &[u8], col: usize) -> usize {
    let mut next = col.saturating_add(1).min(line.len());
    while next < line.len()
        && std::str::from_utf8(line).is_ok_and(|text| !text.is_char_boundary(next))
    {
        next += 1;
    }
    next
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplaceInput {
    Scalar(char),
    TypedCr,
    TypedNl,
    QuotedCr,
    QuotedNl,
}

fn classify_replace_key(key: char, quoted: bool) -> ReplaceInput {
    match (key, quoted) {
        ('\r', false) => ReplaceInput::TypedCr,
        ('\n', false) => ReplaceInput::TypedNl,
        ('\r', true) => ReplaceInput::QuotedCr,
        ('\n', true) => ReplaceInput::QuotedNl,
        _ => ReplaceInput::Scalar(key),
    }
}

fn scalar_bytes(input: ReplaceInput) -> Vec<u8> {
    match input {
        ReplaceInput::Scalar(ch) => {
            let mut encoded = [0u8; 4];
            ch.encode_utf8(&mut encoded).as_bytes().to_vec()
        }
        ReplaceInput::QuotedCr | ReplaceInput::TypedCr => vec![b'\r'],
        ReplaceInput::QuotedNl | ReplaceInput::TypedNl => vec![0x00],
    }
}

fn literal_for_nonblock(input: ReplaceInput) -> ReplaceInput {
    match input {
        ReplaceInput::TypedCr => ReplaceInput::QuotedCr,
        ReplaceInput::TypedNl => ReplaceInput::QuotedNl,
        other => other,
    }
}

fn inclusive_scalar_end(line: &[u8], start: usize, count: usize) -> Option<usize> {
    let mut col = start.min(line.len());
    let mut last = None;
    for _ in 0..count {
        if col >= line.len() {
            return None;
        }
        let next = crate::motion::next_char_boundary(line, col);
        if next <= col {
            return None;
        }
        last = Some(next - 1);
        col = next;
    }
    last
}

fn scalar_count_in_range(line: &[u8], start: usize, end: usize) -> usize {
    let mut col = start.min(line.len());
    let end = end.min(line.len());
    let mut count = 0;
    while col < end {
        let next = crate::motion::next_char_boundary(line, col);
        if next <= col {
            break;
        }
        count += 1;
        col = next;
    }
    count
}
fn adjust_number_span(line: &[u8], col: usize, delta: i64) -> Option<(usize, usize, Vec<u8>)> {
    let (start, end, token) = find_number_token(line, col)?;
    let rendered = render_adjusted_number(token, &line[start..end], delta);
    Some((start, end, rendered))
}

#[derive(Clone, Copy)]
enum NumberToken {
    Decimal {
        magnitude: u64,
        negative: bool,
        overflow: bool,
    },
    Hex {
        value: u64,
        prefix: u8,
        upper: bool,
        overflow: bool,
    },
    Bin {
        value: u64,
        prefix: u8,
        overflow: bool,
    },
}

fn find_number_token(line: &[u8], col: usize) -> Option<(usize, usize, NumberToken)> {
    // Mirror `do_addsub` (`ops.c`) normal-mode scanning under default
    // 'nrformats' (hex + bin, no octal/alpha): the nearest number at or after
    // the cursor wins. A `0x`/`0b` prefix is honored only when the cursor sits
    // inside that run, or the forward-found digit run begins at the prefix's
    // `0`; otherwise the token is decimal, including a leading `-`.
    if line.is_empty() {
        return None;
    }
    let col = col.min(line.len().saturating_sub(1));

    // Cursor-context prefix walk: step left over hex digits (a superset of the
    // binary digits). On a hex/bin overlap, rescan over decimal digits so a
    // `0b…` run is not mistaken for the tail of a hex number.
    let hex_prefix_at = |p: usize| {
        p >= 1
            && matches!(line[p], b'x' | b'X')
            && line[p - 1] == b'0'
            && p + 1 < line.len()
            && line[p + 1].is_ascii_hexdigit()
    };
    let mut pos = col;
    while pos > 0 && line[pos].is_ascii_hexdigit() {
        pos -= 1;
    }
    if !hex_prefix_at(pos) {
        pos = col;
        while pos > 0 && line[pos].is_ascii_digit() {
            pos -= 1;
        }
    }
    if hex_prefix_at(pos) {
        let mut end = pos + 1;
        while end < line.len() && line[end].is_ascii_hexdigit() {
            end += 1;
        }
        return parse_prefixed(line, pos - 1, end);
    }
    if pos >= 1
        && matches!(line[pos], b'b' | b'B')
        && line[pos - 1] == b'0'
        && pos + 1 < line.len()
        && matches!(line[pos + 1], b'0' | b'1')
    {
        let mut end = pos + 1;
        while end < line.len() && matches!(line[end], b'0' | b'1') {
            end += 1;
        }
        return parse_prefixed(line, pos - 1, end);
    }

    // Forward scan from the cursor to the first decimal digit, then walk back to
    // the run start.
    let mut idx = col;
    while idx < line.len() && !line[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx >= line.len() {
        return None;
    }
    let mut start = idx;
    while start > 0 && line[start - 1].is_ascii_digit() {
        start -= 1;
    }

    // A forward-found run that begins at a prefix's `0` is that prefixed number.
    if line[start] == b'0' && start + 2 < line.len() {
        if matches!(line[start + 1], b'x' | b'X') && line[start + 2].is_ascii_hexdigit() {
            let mut end = start + 2;
            while end < line.len() && line[end].is_ascii_hexdigit() {
                end += 1;
            }
            return parse_prefixed(line, start, end);
        }
        if matches!(line[start + 1], b'b' | b'B') && matches!(line[start + 2], b'0' | b'1') {
            let mut end = start + 2;
            while end < line.len() && matches!(line[end], b'0' | b'1') {
                end += 1;
            }
            return parse_prefixed(line, start, end);
        }
    }

    // Otherwise decimal, absorbing an immediately preceding minus sign.
    let digit_start = start;
    let mut negative = false;
    if start > 0 && line[start - 1] == b'-' {
        start -= 1;
        negative = true;
    }
    let mut end = digit_start;
    while end < line.len() && line[end].is_ascii_digit() {
        end += 1;
    }
    let (magnitude, overflow) = parse_u64_digits(&line[digit_start..end], 10)?;
    Some((
        start,
        end,
        NumberToken::Decimal {
            magnitude,
            negative,
            overflow,
        },
    ))
}
fn parse_prefixed(line: &[u8], start: usize, end: usize) -> Option<(usize, usize, NumberToken)> {
    let marker = line[start + 1];
    let digits = &line[start + 2..end];
    if marker.eq_ignore_ascii_case(&b'x') {
        let (value, overflow) = parse_u64_digits(digits, 16)?;
        let upper = hex_case_upper(line, start, end);
        Some((
            start,
            end,
            NumberToken::Hex {
                value,
                prefix: marker,
                upper,
                overflow,
            },
        ))
    } else {
        let (value, overflow) = parse_u64_digits(digits, 2)?;
        Some((
            start,
            end,
            NumberToken::Bin {
                value,
                prefix: marker,
                overflow,
            },
        ))
    }
}

fn parse_u64_digits(digits: &[u8], base: u32) -> Option<(u64, bool)> {
    if digits.is_empty() {
        return None;
    }
    let mut value = 0u64;
    let mut overflow = false;
    let radix = u64::from(base);
    for &b in digits {
        let digit = match b {
            b'0'..=b'9' => u64::from(b - b'0'),
            b'a'..=b'f' if base == 16 => u64::from(b - b'a') + 10,
            b'A'..=b'F' if base == 16 => u64::from(b - b'A') + 10,
            _ => return None,
        };
        if digit >= radix {
            return None;
        }
        if overflow {
            continue;
        }
        if let Some(next) = value
            .checked_mul(radix)
            .and_then(|next| next.checked_add(digit))
        {
            value = next;
        } else {
            value = u64::MAX;
            overflow = true;
        }
    }
    Some((value, overflow))
}

fn hex_case_upper(line: &[u8], start: usize, end: usize) -> bool {
    // Upstream `hexupper` is last ASCII-alphabetic byte in the old token,
    // including the `x`/`X` marker (`ops.c` `do_addsub`).
    line[start..end]
        .iter()
        .rev()
        .find(|b| b.is_ascii_alphabetic())
        .is_some_and(u8::is_ascii_uppercase)
}

fn render_adjusted_number(token: NumberToken, old: &[u8], delta: i64) -> Vec<u8> {
    match token {
        NumberToken::Decimal {
            magnitude,
            negative,
            overflow,
        } => {
            let (next, next_negative) = if overflow {
                (magnitude, negative && magnitude != 0)
            } else {
                add_signed_u64(magnitude, negative, delta)
            };
            pad_number(old, None, next_negative, next.to_string().as_bytes())
        }
        NumberToken::Hex {
            value,
            prefix,
            upper,
            overflow,
        } => {
            let next = if overflow {
                value
            } else {
                add_u64(value, delta)
            };
            let digits = if upper {
                format!("{next:X}")
            } else {
                format!("{next:x}")
            };
            pad_number(old, Some(prefix), false, digits.as_bytes())
        }
        NumberToken::Bin {
            value,
            prefix,
            overflow,
        } => {
            let next = if overflow {
                value
            } else {
                add_u64(value, delta)
            };
            let digits = if next == 0 {
                "0".to_string()
            } else {
                format!("{next:b}")
            };
            pad_number(old, Some(prefix), false, digits.as_bytes())
        }
    }
}

fn pad_number(old: &[u8], prefix: Option<u8>, negative: bool, digits: &[u8]) -> Vec<u8> {
    let firstdigit = if old.first() == Some(&b'-') {
        old.get(1).copied()
    } else {
        old.first().copied()
    };
    let pad_width = if firstdigit == Some(b'0') {
        old.len()
            .saturating_sub(usize::from(old.first() == Some(&b'-')))
    } else {
        0
    };
    let mut out = Vec::new();
    if negative {
        out.push(b'-');
    }
    let mut body_len = digits.len();
    if let Some(marker) = prefix {
        out.push(b'0');
        out.push(marker);
        body_len += 2;
    }
    if pad_width > body_len {
        out.extend(std::iter::repeat_n(b'0', pad_width - body_len));
    }
    out.extend_from_slice(digits);
    out
}

fn add_signed_u64(magnitude: u64, negative: bool, delta: i64) -> (u64, bool) {
    let subtract = (delta < 0) ^ negative;
    let amount = delta.unsigned_abs();
    let old = magnitude;
    let next = if subtract {
        old.wrapping_sub(amount)
    } else {
        old.wrapping_add(amount)
    };
    let mut next_negative = negative;
    if subtract {
        if next > old {
            return (1u64.wrapping_add(!next), !negative);
        }
    } else if next < old {
        return (!next, !negative);
    }
    if next == 0 {
        next_negative = false;
    }
    (next, next_negative)
}

fn add_u64(value: u64, delta: i64) -> u64 {
    if delta >= 0 {
        value.wrapping_add(delta.cast_unsigned())
    } else {
        value.wrapping_sub(delta.unsigned_abs())
    }
}
/// `'maxmapdepth'` (`p_mmd`), upstream's default 1000.
fn max_map_depth(editor: &Editor) -> u64 {
    match editor.options().get_global("maxmapdepth") {
        Ok(OptionValue::Number(value)) if *value > 0 => u64::try_from(*value).unwrap_or(1000),
        _ => 1000,
    }
}
fn option_bool(editor: &Editor, name: &str, fallback: bool) -> bool {
    match editor.options().get_global(name) {
        Ok(OptionValue::Boolean(value)) => *value,
        _ => fallback,
    }
}
fn option_contains(editor: &Editor, name: &str, item: &str, fallback: bool) -> bool {
    match editor.options().get_global(name) {
        Ok(OptionValue::String(value)) => value.split(',').any(|candidate| candidate == item),
        _ => fallback,
    }
}
fn push_jump(editor: &mut Editor, buffer: BufHandle, position: Position) {
    editor
        .jumplist_mut()
        .push(MarkLocation::in_buffer(buffer, position));
}

/// Whether `key` is a recognized Vim register name (`RegisterName::try_from`
/// in `register.rs`). Used to gate `v:register` updates so invalid or
/// ignored prefix keys (e.g. Escape) do not corrupt the variable.
fn is_valid_register_name(key: char) -> bool {
    matches!(
        key,
        'a'..='z' | 'A'..='Z' | '0'..='9' | '"' | '-' | '_' | '=' | '*' | '+' | '/' | ':' | '.' | '#' | '%'
    )
}
