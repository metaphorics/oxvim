//! Modal input state machine, shaped like `state.c`'s check/execute loop.

use ox_text::Position;
use ox_types::{BufHandle, WinHandle};
use thiserror::Error;

use crate::indent::{self, CinTrigger, ExprEval, IndentExprError};
use crate::insert;
use crate::motion::{resolve, resolve_find, FindDirection, FindMotion};
use crate::ops::{self, EditRange, Operator};
use crate::put::PutDirection;
use crate::search::{SearchDirection, SearchState};
use crate::textobject;
use crate::{BufferStateError, BufferTextEditRequest, Editor, EditorError, ExtmarkPosition, Key, KeyDecodeError, MarkLocation, MotionKind, OptionValue, SearchError, VisualKind, VisualState};
use crate::{Lookup, MapMode, MappingAction, MappingOptions, Remap, TypeaheadError, TypeaheadFlags, KS_EXTRA};

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
}

/// Active editor input mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Normal command mode.
    Normal(NormalState),
    /// Plain insertion mode.
    Insert(InsertState),
    /// Visual selection mode.
    Visual(VisualState),
    /// Search command-line mode.
    Cmdline(CmdlineState),
    /// An operator is waiting for its range.
    OperatorPending(OperatorPendingState),
}
impl Default for Mode { fn default() -> Self { Self::Normal(NormalState::default()) } }

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
    #[error(transparent)] Editor(#[from] EditorError),
    /// Encoded input could not be decoded.
    #[error(transparent)] KeyDecode(#[from] KeyDecodeError),
    /// Mapped input could not be inserted into typeahead.
    #[error(transparent)] Typeahead(#[from] TypeaheadError),
    /// Search parsing or execution failed.
    #[error(transparent)] Search(#[from] SearchError),
    /// Operator range application failed.
    #[error(transparent)] Operator(#[from] ops::OperatorError),
    /// Buffer text could not be read.
    #[error(transparent)] Buffer(#[from] BufferStateError),
    /// Insert-mode operation failed.
    #[error(transparent)] Insert(#[from] insert::InsertError),
    /// Indent expression evaluation failed.
    #[error(transparent)] Indent(#[from] IndentExprError),
    /// A named special key has no behavior in the current mode.
    #[error("non-character input is not supported in this modal state")]
    UnsupportedKey,
    /// `'maxmapdepth'` mapping expansions happened without consuming a key.
    #[error("recursive mapping")]
    RecursiveMapping,
}

/// Stateful modal command processor.
#[derive(Clone, Debug, Default)]
pub struct ModeMachine {
    /// Currently active input mode.
    pub mode: Mode,
    search: SearchState,
    last_find: Option<FindMotion>,
    last_visual: Option<VisualState>,
    completed_ex_command: Option<String>,
    pending_mapping_action: Option<(MappingAction, MappingOptions)>,
    /// `mapdepth` (`getchar.c`): mapping expansions since the last key was
    /// consumed. `nmap ,x ,x` re-expands forever without it.
    map_depth: u32,
    timestamp: i64,
}

struct Context { buffer: BufHandle, window: WinHandle, cursor: Position, lines: Vec<Vec<u8>>, topline: usize, bottomline: usize }

impl ModeMachine {
    /// Returns the active mode.
    #[must_use] pub const fn mode(&self) -> &Mode { &self.mode }
    /// Returns the last-search state.
    #[must_use] pub const fn search_state(&self) -> &SearchState { &self.search }
    /// Takes an Ex command completed with Enter.
    pub fn take_ex_command(&mut self) -> Option<String> { self.completed_ex_command.take() }
    /// Takes a non-key mapping action, with the flags it was registered with,
    /// for execution by the embedding host.
    pub fn take_mapping_action(&mut self) -> Option<(MappingAction, MappingOptions)> { self.pending_mapping_action.take() }

    /// `state.c:34-106`: checking consumes mappings and input but performs no buffer mutation.
    pub fn check(&mut self, editor: &mut Editor) -> Result<Step, ModeError> {
        loop {
            let Some(flags) = editor.typeahead().front_flags() else { return Ok(Step::Idle); };
            if flags.remap == Remap::Yes {
                let mode = map_mode(&self.mode);
                let buffer = editor.current_buffer();
                let lookup = editor.mappings().lookup_typeahead(editor.typeahead(), mode, buffer);
                let resolved = match lookup {
                    Lookup::Exact(mapping, width) => Some((mapping.action.clone(), mapping.options.clone(), width)),
                    Lookup::Prefix(_) => return Ok(Step::Idle),
                    Lookup::None => None,
                };
                if let Some((action, options, width)) = resolved {
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
            self.map_depth = 0;
            let Some(key) = editor.typeahead_mut().pop()? else { return Ok(Step::Idle); };
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
    pub fn timeout_pending_mapping(&mut self, editor: &mut Editor) -> Result<bool, ModeError> {
        let Some(flags) = editor.typeahead().front_flags() else { return Ok(false) };
        if flags.remap != Remap::Yes {
            return Ok(false);
        }
        let mode = map_mode(&self.mode);
        let buffer = editor.current_buffer();
        let resolved = match editor.mappings().lookup_typeahead(editor.typeahead(), mode, buffer) {
            Lookup::Prefix(Some(mapping)) => {
                Some((mapping.action.clone(), mapping.options.clone(), mapping.lhs.len()))
            }
            Lookup::Prefix(None) => None,
            Lookup::Exact(_, _) | Lookup::None => return Ok(false),
        };
        match resolved {
            Some((action, options, width)) => {
                self.apply_mapping(editor, action, options, width)?;
                Ok(true)
            }
            None => {
                editor.typeahead_mut().deny_front_remap();
                Ok(true)
            }
        }
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
                editor.typeahead_mut().push(&keys, 0, TypeaheadFlags {
                    remap: if options.remap { Remap::Yes } else { Remap::No },
                    modes: options.modes,
                    buffer,
                    mapped: true,
                    silent: options.silent,
                })?;
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
        if flags.mapped || matches!(self.mode, Mode::Insert(_) | Mode::Cmdline(_)) {
            return;
        }
        editor.sync_current_undo();
    }

    /// Executes the action classified by [`Self::check`].
    pub fn execute(&mut self, editor: &mut Editor, step: Step, eval: &mut dyn ExprEval) -> Result<(), ModeError> {
        match step { Step::Idle | Step::ProcessEvents => Ok(()), Step::Key(key) => self.execute_key(editor, key, eval) }
    }

    /// Runs one check/execute iteration, returning whether work was ready.
    pub fn run_once(&mut self, editor: &mut Editor, eval: &mut dyn ExprEval) -> Result<bool, ModeError> {
        let step = self.check(editor)?; let ready = step != Step::Idle; self.execute(editor, step, eval)?; Ok(ready)
    }

    /// Convenience entry point used by behavioral tests and embedding frontends.
    pub fn feed_keys(&mut self, editor: &mut Editor, keys: &str, eval: &mut dyn ExprEval) -> Result<(), ModeError> {
        for key in keys.chars() { self.execute_key(editor, key, eval)?; }
        Ok(())
    }

    fn execute_key(&mut self, editor: &mut Editor, key: char, eval: &mut dyn ExprEval) -> Result<(), ModeError> {
        self.timestamp = self.timestamp.saturating_add(1);
        // Handlers borrow `self` next to their variant state, so the mode is
        // taken into a local for the duration of dispatch; every exit path
        // refills the slot, so a handler error restores the exact pre-key
        // variant state instead of stranding the machine on a default Normal.
        let mut mode = std::mem::take(&mut self.mode);
        let transition = match &mut mode {
            Mode::Normal(state) => self.normal(editor, state, key, eval),
            Mode::Insert(state) => self.insert(editor, state, key, eval),
            Mode::Visual(state) => self.visual(editor, state, key, eval),
            Mode::Cmdline(state) => self.cmdline(editor, state, key),
            Mode::OperatorPending(state) => self.operator_pending(editor, state, key, eval),
        };
        match transition {
            Ok(Some(next)) => self.mode = next,
            Ok(None) => self.mode = mode,
            Err(error) => {
                self.mode = mode;
                return Err(error);
            }
        }
        Ok(())
    }

    fn normal(&mut self, editor: &mut Editor, state: &mut NormalState, key: char, eval: &mut dyn ExprEval) -> Result<Option<Mode>, ModeError> {
        if state.prefix == "register" { state.register = Some(key); state.prefix.clear(); return Ok(None); }
        if matches!(state.prefix.as_str(), "f" | "F" | "t" | "T") {
            let find = FindMotion { direction: if matches!(state.prefix.as_str(), "f" | "t") { FindDirection::Forward } else { FindDirection::Backward }, till: matches!(state.prefix.as_str(), "t" | "T"), target: key as u8 };
            // `nv_csearch`: `if (searchc(cap, t_cmd) == false) clearopbeep()`.
            // `searchc` records the target before searching, so a failed
            // `fz` is still what `;` repeats.
            if !self.move_find(editor, find, state.count.max(1), false)? { beep_flush(editor); }
            self.last_find = Some(find); return Ok(Some(Mode::default()));
        }
        if state.prefix == "g" {
            state.prefix.clear();
            if key == 'v' {
                if let Some(visual) = self.last_visual.clone() { let window = context(editor)?.window; editor.set_window_cursor(window, visual.cursor)?; return Ok(Some(Mode::Visual(visual))); }
                return Ok(Some(Mode::default()));
            }
            if key == 'u' || key == 'U' { return Ok(Some(Mode::OperatorPending(OperatorPendingState { operator: if key == 'u' { Operator::Lowercase } else { Operator::Uppercase }, count: state.count.max(1), count_was_set: state.count != 0, motion_count: 0, register: state.register, prefix: String::new() }))); }
            let command = format!("g{key}"); self.move_command(editor, &command, state.count.max(1), false)?; return Ok(Some(Mode::default()));
        }
        if key.is_ascii_digit() && (key != '0' || state.count != 0) { state.count = append_digit(state.count, key); return Ok(None); }
        let count = state.count.max(1);
        match key {
            '"' => { state.prefix = "register".into(); Ok(None) }
            'd' | 'c' | 'y' | '>' | '<' | '=' => Ok(Some(Mode::OperatorPending(OperatorPendingState { operator: operator_for(key), count, count_was_set: state.count != 0, motion_count: 0, register: state.register, prefix: String::new() }))),
            'g' => { state.prefix = "g".into(); Ok(None) }
            'f' | 'F' | 't' | 'T' => { state.prefix = key.to_string(); Ok(None) }
            // `searchc` returns false when there is no previous `f`/`t` to
            // repeat (`*lastc == NUL`), and `nv_csearch` turns that into
            // `clearopbeep` — which flushes the rest of the mapped typeahead,
            // so the remainder of a `:normal` argument never runs.
            ';' | ',' => {
                let moved = match self.last_find {
                    Some(mut find) => {
                        if key == ',' { find.direction = reverse_find(find.direction); }
                        self.move_find(editor, find, count, false)?
                    }
                    None => false,
                };
                if !moved { beep_flush(editor); }
                Ok(Some(Mode::default()))
            }
            'h' | 'j' | 'k' | 'l' | 'w' | 'W' | 'e' | 'E' | 'b' | 'B' | '0' | '^' | '$' | '%' | '{' | '}' | '(' | ')' | 'G' | 'H' | 'M' | 'L' => { let command = if key == 'G' && state.count != 0 { "G_count".to_owned() } else { key.to_string() }; self.move_command(editor, &command, count, false)?; Ok(Some(Mode::default())) }
            'i' => Ok(Some(Mode::Insert(InsertState))),
            'a' => { self.advance_insert_cursor(editor, false)?; Ok(Some(Mode::Insert(InsertState))) }
            'A' => { self.advance_insert_cursor(editor, true)?; Ok(Some(Mode::Insert(InsertState))) }
            'I' => { self.move_command(editor, "^", 1, false)?; Ok(Some(Mode::Insert(InsertState))) }
            'o' | 'O' => { self.open_line(editor, key == 'o', eval)?; Ok(Some(Mode::Insert(InsertState))) }
            'v' | 'V' | '\u{16}' => { let cursor = context(editor)?.cursor; Ok(Some(Mode::Visual(VisualState::new(cursor, if key == 'v' { VisualKind::Character } else if key == 'V' { VisualKind::Line } else { VisualKind::Block })))) }
            '/' | '?' => Ok(Some(Mode::Cmdline(CmdlineState { kind: CmdlineKind::Search(if key == '/' { SearchDirection::Forward } else { SearchDirection::Backward }), text: String::new(), count }))),
            ':' => Ok(Some(Mode::Cmdline(CmdlineState { kind: CmdlineKind::Ex, text: String::new(), count }))),
            'n' | 'N' => { self.repeat_search(editor, key == 'N', count)?; Ok(Some(Mode::default())) }
            'p' | 'P' => {
                let ctx = context(editor)?;
                let name = state.register.unwrap_or('"');
                let direction = if key == 'p' { PutDirection::After } else { PutDirection::Before };
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
            'x' => { let ctx = context(editor)?; let end = Position { lnum: ctx.cursor.lnum, col: ctx.cursor.col.saturating_add(count - 1) }; ops::apply(editor, ctx.buffer, ctx.window, Operator::Delete, EditRange { start: ctx.cursor, end, kind: MotionKind::CharacterWise, inclusive: true }, state.register, self.timestamp, eval)?; Ok(Some(Mode::default())) }
            '~' => { let ctx = context(editor)?; let end = Position { lnum: ctx.cursor.lnum, col: ctx.cursor.col.saturating_add(count - 1) }; ops::apply(editor, ctx.buffer, ctx.window, Operator::ToggleCase, EditRange { start: ctx.cursor, end, kind: MotionKind::CharacterWise, inclusive: true }, None, self.timestamp, eval)?; self.move_command(editor, "l", count, false)?; Ok(Some(Mode::default())) }
            'u' => { let ctx = context(editor)?; editor.buffer_undo(ctx.buffer)?; Ok(Some(Mode::default())) }
            _ => Ok(Some(Mode::default())),
        }
    }

    fn operator_pending(&mut self, editor: &mut Editor, state: &mut OperatorPendingState, key: char, eval: &mut dyn ExprEval) -> Result<Option<Mode>, ModeError> {
        if matches!(state.prefix.as_str(), "f" | "F" | "t" | "T") {
            let find = FindMotion { direction: if matches!(state.prefix.as_str(), "f" | "t") { FindDirection::Forward } else { FindDirection::Backward }, till: matches!(state.prefix.as_str(), "t" | "T"), target: key as u8 };
            let ctx = context(editor)?;
            let Some(motion) = resolve_find(&ctx.lines, ctx.cursor, find, state.count.saturating_mul(state.motion_count.max(1))) else { return Ok(Some(Mode::default())); };
            self.apply_operator(editor, &state, EditRange::from_motion(ctx.cursor, motion), eval)?; self.last_find = Some(find); return Ok(Some(if state.operator == Operator::Change { Mode::Insert(InsertState) } else { Mode::default() }));
        }
        if state.prefix == "g" { state.prefix.clear(); let command = format!("g{key}"); return self.finish_operator_motion(editor, state, &command, eval); }
        if state.prefix == "i" || state.prefix == "a" {
            let inner = state.prefix == "i"; let ctx = context(editor)?;
            if let Some(range) = textobject::resolve(&ctx.lines, ctx.cursor, inner, key, state.count.saturating_mul(state.motion_count.max(1))) { let change = state.operator == Operator::Change; self.apply_operator(editor, &state, range, eval)?; return Ok(Some(if change { Mode::Insert(InsertState) } else { Mode::default() })); }
            return Ok(Some(Mode::default()));
        }
        if key.is_ascii_digit() && (key != '0' || state.motion_count != 0) { state.motion_count = append_digit(state.motion_count, key); return Ok(None); }
        if key == 'i' || key == 'a' || matches!(key, 'f' | 'F' | 't' | 'T') { state.prefix = key.to_string(); return Ok(None); }
        if key == 'g' { state.prefix = "g".into(); return Ok(None); }
        if key == operator_key(state.operator) { let ctx = context(editor)?; let end = Position { lnum: ctx.cursor.lnum.saturating_add(state.count.saturating_mul(state.motion_count.max(1)) - 1).min(ctx.lines.len()), col: 0 }; let range = EditRange { start: ctx.cursor, end, kind: MotionKind::LineWise, inclusive: true }; let change = state.operator == Operator::Change; self.apply_operator(editor, &state, range, eval)?; return Ok(Some(if change { Mode::Insert(InsertState) } else { Mode::default() })); }
        self.finish_operator_motion(editor, state, &key.to_string(), eval)
    }

    fn finish_operator_motion(&mut self, editor: &mut Editor, state: &mut OperatorPendingState, command: &str, eval: &mut dyn ExprEval) -> Result<Option<Mode>, ModeError> {
        let ctx = context(editor)?; let count = state.count.saturating_mul(state.motion_count.max(1));
        let current = ctx.lines.get(ctx.cursor.lnum.saturating_sub(1)).and_then(|line| line.get(ctx.cursor.col)).copied();
        let resolved_command = match (state.operator, command, current) {
            (Operator::Change, "w", Some(byte)) if !byte.is_ascii_whitespace() => "e",
            (Operator::Change, "W", Some(byte)) if !byte.is_ascii_whitespace() => "E",
            (_, "G", _) if state.count_was_set || state.motion_count != 0 => "G_count",
            _ => command,
        };
        if let Some(motion) = resolve(&ctx.lines, ctx.cursor, resolved_command, count, option_bool(editor, "startofline", true), (ctx.topline, ctx.bottomline)) { let change = state.operator == Operator::Change; if motion.is_jump { push_jump(editor, ctx.buffer, ctx.cursor); } self.apply_operator(editor, &state, EditRange::from_motion(ctx.cursor, motion), eval)?; return Ok(Some(if change { Mode::Insert(InsertState) } else { Mode::default() })); }
        Ok(Some(Mode::default()))
    }

    fn apply_operator(&mut self, editor: &mut Editor, state: &OperatorPendingState, range: EditRange, eval: &mut dyn ExprEval) -> Result<(), ModeError> { let ctx = context(editor)?; ops::apply(editor, ctx.buffer, ctx.window, state.operator, range, state.register, self.timestamp, eval)?; Ok(()) }

    fn visual(&mut self, editor: &mut Editor, state: &mut VisualState, key: char, eval: &mut dyn ExprEval) -> Result<Option<Mode>, ModeError> {
        if state.prefix == "g" {
            state.prefix.clear();
            if matches!(key, 'u' | 'U' | '~') {
                let operator = match key { 'u' => Operator::Lowercase, 'U' => Operator::Uppercase, _ => Operator::ToggleCase };
                return self.finish_visual_operator(editor, state, operator, eval);
            }
            let ctx = context(editor)?; let command = format!("g{key}");
            if let Some(motion) = resolve(&ctx.lines, ctx.cursor, &command, state.count.max(1), option_bool(editor, "startofline", true), (ctx.topline, ctx.bottomline)) { editor.set_window_cursor(ctx.window, motion.target)?; extend_visual(state, motion.target, ctx.cursor); }
            state.count = 0; return Ok(None);
        }
        if matches!(state.prefix.as_str(), "f" | "F" | "t" | "T") {
            if key.len_utf8() != 1 { state.prefix.clear(); return Ok(None); }
            let find = FindMotion { direction: if matches!(state.prefix.as_str(), "f" | "t") { FindDirection::Forward } else { FindDirection::Backward }, till: matches!(state.prefix.as_str(), "t" | "T"), target: key as u8 };
            let ctx = context(editor)?; if let Some(motion) = resolve_find(&ctx.lines, ctx.cursor, find, state.count.max(1)) { editor.set_window_cursor(ctx.window, motion.target)?; extend_visual(state, motion.target, ctx.cursor); self.last_find = Some(find); }
            state.prefix.clear(); state.count = 0; return Ok(None);
        }
        if state.prefix == "i" || state.prefix == "a" {
            let inner = state.prefix == "i"; let ctx = context(editor)?;
            if let Some(range) = textobject::resolve(&ctx.lines, ctx.cursor, inner, key, state.count.max(1)) { state.anchor = range.start; state.cursor = range.end; state.kind = match range.kind { MotionKind::LineWise => VisualKind::Line, MotionKind::BlockWise => VisualKind::Block, MotionKind::CharacterWise => VisualKind::Character }; editor.set_window_cursor(ctx.window, state.cursor)?; }
            state.prefix.clear(); state.count = 0; return Ok(None);
        }
        if key.is_ascii_digit() && (key != '0' || state.count != 0) { state.count = append_digit(state.count, key); return Ok(None); }
        match key {
            '\u{1b}' => { self.last_visual = Some(state.clone()); Ok(Some(Mode::default())) }
            'o' | 'O' => { if key == 'O' && state.kind == VisualKind::Block { state.swap_columns(); } else { state.swap_ends(); } let window = context(editor)?.window; editor.set_window_cursor(window, state.cursor)?; Ok(None) }
            'g' | 'f' | 'F' | 't' | 'T' | 'i' | 'a' => { state.prefix = key.to_string(); Ok(None) }
            'J' => {
                let range = state.range();
                let start_lnum = range.start.lnum.min(range.end.lnum);
                let end_lnum = range.start.lnum.max(range.end.lnum);
                self.join_lines(editor, start_lnum, end_lnum)?;
                Ok(Some(Mode::default()))
            }
            'd' | 'x' | 'X' | 'c' | 'y' | '>' | '<' | '=' | 'u' | 'U' | '~' => self.finish_visual_operator(editor, state, match key { 'x' | 'X' => Operator::Delete, 'u' => Operator::Lowercase, 'U' => Operator::Uppercase, '~' => Operator::ToggleCase, _ => operator_for(key) }, eval),
            _ => { let ctx = context(editor)?; if let Some(motion) = resolve(&ctx.lines, ctx.cursor, &key.to_string(), state.count.max(1), option_bool(editor, "startofline", true), (ctx.topline, ctx.bottomline)) { editor.set_window_cursor(ctx.window, motion.target)?; extend_visual(state, motion.target, ctx.cursor); } state.count = 0; Ok(None) }
        }
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
        let mut previous_was_comment = remove_comments
            && scan_comment_line(first, &comments).ends_open;
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
            for _ in 0..separator_len {
                joined.push(b' ');
            }
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

    fn finish_visual_operator(&mut self, editor: &mut Editor, state: &VisualState, operator: Operator, eval: &mut dyn ExprEval) -> Result<Option<Mode>, ModeError> {
        let ctx = context(editor)?; self.last_visual = Some(state.clone()); let result = ops::apply(editor, ctx.buffer, ctx.window, operator, state.range(), None, self.timestamp, eval)?; Ok(Some(if result.enter_insert { Mode::Insert(InsertState) } else { Mode::default() }))
    }

    fn insert(&mut self, editor: &mut Editor, _state: &mut InsertState, key: char, eval: &mut dyn ExprEval) -> Result<Option<Mode>, ModeError> {
        let ctx = context(editor)?;
        match key {
            '\u{1b}' => { insert::normal_cursor(editor, ctx.window, ctx.cursor)?; Ok(Some(Mode::default())) }
            '\n' | '\r' => { insert::newline(editor, ctx.buffer, ctx.window, ctx.cursor, self.timestamp, eval)?; Ok(None) }
            '\u{8}' | '\u{7f}' => { insert::backspace(editor, ctx.buffer, ctx.window, ctx.cursor, option_contains(editor, "backspace", "eol", true), self.timestamp)?; Ok(None) }
            ch if !ch.is_control() => { insert::insert_char(editor, ctx.buffer, ctx.window, ctx.cursor, ch, self.timestamp)?; Ok(None) }
            _ => Ok(None),
        }
    }

    fn cmdline(&mut self, editor: &mut Editor, state: &mut CmdlineState, key: char) -> Result<Option<Mode>, ModeError> {
        match key {
            '\u{1b}' => Ok(Some(Mode::default())),
            '\u{8}' | '\u{7f}' => { state.text.pop(); Ok(None) }
            '\n' | '\r' => {
                match state.kind {
                    CmdlineKind::Search(direction) => {
                        let ctx = context(editor)?;
                        let result = self.search.search(&ctx.lines, ctx.cursor, &state.text, direction, state.count.max(1), option_bool(editor, "wrapscan", true))?;
                        push_jump(editor, ctx.buffer, ctx.cursor);
                        editor.set_window_cursor(ctx.window, result.target)?;
                    }
                    CmdlineKind::Ex => self.completed_ex_command = Some(std::mem::take(&mut state.text)),
                }
                Ok(Some(Mode::default()))
            }
            ch if !ch.is_control() => { state.text.push(ch); Ok(None) }
            _ => Ok(None),
        }
    }

    fn move_command(&mut self, editor: &mut Editor, command: &str, count: usize, visual: bool) -> Result<(), ModeError> { let ctx = context(editor)?; if let Some(motion) = resolve(&ctx.lines, ctx.cursor, command, count, option_bool(editor, "startofline", true), (ctx.topline, ctx.bottomline)) { if motion.is_jump && !visual { push_jump(editor, ctx.buffer, ctx.cursor); } editor.set_window_cursor(ctx.window, motion.target)?; } Ok(()) }
    /// `searchc` (`search.c`): reports whether the target was found, so the
    /// caller can `clearopbeep` when it was not.
    fn move_find(&mut self, editor: &mut Editor, find: FindMotion, count: usize, _visual: bool) -> Result<bool, ModeError> { let ctx = context(editor)?; let Some(motion) = resolve_find(&ctx.lines, ctx.cursor, find, count) else { return Ok(false) }; editor.set_window_cursor(ctx.window, motion.target)?; Ok(true) }
    fn repeat_search(&mut self, editor: &mut Editor, opposite: bool, count: usize) -> Result<(), ModeError> { let ctx = context(editor)?; let result = self.search.repeat(&ctx.lines, ctx.cursor, opposite, count, option_bool(editor, "wrapscan", true))?; push_jump(editor, ctx.buffer, ctx.cursor); editor.set_window_cursor(ctx.window, result.target)?; Ok(()) }
    fn advance_insert_cursor(&self, editor: &mut Editor, line_end: bool) -> Result<(), ModeError> { let ctx = context(editor)?; let line = &ctx.lines[ctx.cursor.lnum - 1]; let col = if line_end { line.len() } else { next_boundary(line, ctx.cursor.col) }; editor.set_window_cursor(ctx.window, Position { lnum: ctx.cursor.lnum, col })?; Ok(()) }
    fn open_line(&self, editor: &mut Editor, below: bool, eval: &mut dyn ExprEval) -> Result<(), ModeError> {
        let ctx = context(editor)?;
        let opts = indent::IndentOptions::capture(editor, ctx.buffer);
        let source = &ctx.lines[ctx.cursor.lnum - 1];
        let smart = indent::smart_source_trigger(source, !below, &opts);
        let mut indent_bytes = indent::smart_newline_indent(source, smart, &opts);
        let after_line = if below { ctx.cursor.lnum } else { ctx.cursor.lnum.saturating_sub(1) };
        let new_lnum = after_line + 1;
        let mut lines = ctx.lines.clone();
        lines.insert(new_lnum - 1, indent_bytes.clone());
        let trigger = if below { CinTrigger::OpenForward } else { CinTrigger::OpenBackward };
        {
            let context = indent::IndentEvalContext::new(editor, ctx.buffer, &lines);
            if let Some(whitespace) = indent::fix_line_indent(&context, new_lnum, trigger, &opts, eval)? {
                indent_bytes = whitespace;
            }
        }
        let pos = Position { lnum: new_lnum, col: indent_bytes.len() };
        editor.append_buffer_lines(ctx.buffer, after_line, &[indent_bytes], ctx.cursor, self.timestamp)?;
        editor.set_window_cursor(ctx.window, pos)?;
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
        && !((right_char < 0x100 && !unicode_eats_join_space(left_char))
            || (left_char < 0x100 && !unicode_eats_join_space(right_char)))
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
            while line
                .get(end)
                .is_some_and(|byte| byte.is_ascii_whitespace())
            {
                end += 1;
            }
            scan.leading_removal = end;
        }
        scan.ends_open = !part.flags.contains('e');
    }
    scan
}

/// `beep_flush` (`input.c:523-529`): an error in Normal mode discards the
/// mapped run at the front of the typeahead, which is how the rest of a
/// `:normal` argument — or of a mapping's right-hand side — is abandoned.
/// The beep itself is a UI effect this port has no channel for.
fn beep_flush(editor: &mut Editor) {
    editor.typeahead_mut().flush_mapped();
}

fn map_mode(mode: &Mode) -> MapMode {
    match mode {
        Mode::Normal(_) => MapMode::Normal,
        Mode::Insert(_) => MapMode::Insert,
        Mode::Visual(_) => MapMode::Visual,
        Mode::Cmdline(_) => MapMode::CommandLine,
        Mode::OperatorPending(_) => MapMode::OperatorPending,
    }
}

/// Extends a visual selection with a resolved motion target, keeping the
/// wanted block column on vertical motions (virtual edges on short lines).
fn extend_visual(state: &mut VisualState, target: Position, from: Position) {
    if state.kind == VisualKind::Block { state.extend_block(target, from); } else { state.extend(target); }
}

/// Snapshots the editor state one mode operation reads.
///
/// The window cursor is validated first, the way `check_cursor_lnum`
/// (`cursor.c`) validates it before upstream runs a normal-mode command: a
/// window keeps its cursor when its buffer is replaced or shortened, so
/// `w_cursor.lnum` can point past the last line, and every caller below
/// indexes `lines` with it.
fn context(editor: &mut Editor) -> Result<Context, ModeError> {
    let tab = editor.current_tabpage().ok_or(EditorError::UnknownTabpage(ox_types::TabHandle::CURRENT))?;
    let tabpage = editor.tabpage(tab)?; let window = tabpage.current_window(); let height = tabpage.layout().window_geometry(window).map_err(EditorError::from)?.height;
    let state = editor.window(window)?; let buffer = state.buffer; let mut cursor = state.cursor; let topline = state.topline; let text = editor.buffer(buffer)?.text()?;
    let lines = (1..=text.line_count()).map(|lnum| text.line(lnum)).collect::<Result<Vec<_>, _>>().map_err(BufferStateError::from)?;
    let valid = cursor.lnum.clamp(1, lines.len().max(1));
    if valid != cursor.lnum { cursor.lnum = valid; editor.set_window_cursor(window, cursor)?; }
    let bottomline = topline.saturating_add(height.saturating_sub(1)).min(lines.len().max(1));
    Ok(Context { buffer, window, cursor, lines, topline, bottomline })
}
fn append_digit(value: usize, key: char) -> usize { value.saturating_mul(10).saturating_add((key as u8).saturating_sub(b'0') as usize) }
fn operator_for(key: char) -> Operator { match key { 'd' => Operator::Delete, 'c' => Operator::Change, 'y' => Operator::Yank, '>' => Operator::Indent, '<' => Operator::Unindent, '=' => Operator::Format, 'u' => Operator::Lowercase, 'U' => Operator::Uppercase, '~' => Operator::ToggleCase, _ => Operator::Yank } }
fn operator_key(operator: Operator) -> char { match operator { Operator::Delete => 'd', Operator::Change => 'c', Operator::Yank => 'y', Operator::Indent => '>', Operator::Unindent => '<', Operator::Format => '=', Operator::Lowercase => 'u', Operator::Uppercase => 'U', Operator::ToggleCase => '~' } }
fn reverse_find(direction: FindDirection) -> FindDirection { match direction { FindDirection::Forward => FindDirection::Backward, FindDirection::Backward => FindDirection::Forward } }
fn next_boundary(line: &[u8], col: usize) -> usize {
    let mut next = col.saturating_add(1).min(line.len());
    while next < line.len() && std::str::from_utf8(line).map_or(false, |text| !text.is_char_boundary(next)) { next += 1; }
    next
}
/// `'maxmapdepth'` (`p_mmd`), upstream's default 1000.
fn max_map_depth(editor: &Editor) -> u64 {
    match editor.options().get_global("maxmapdepth") {
        Ok(OptionValue::Number(value)) if *value > 0 => u64::try_from(*value).unwrap_or(1000),
        _ => 1000,
    }
}
fn option_bool(editor: &Editor, name: &str, fallback: bool) -> bool { match editor.options().get_global(name) { Ok(OptionValue::Boolean(value)) => *value, _ => fallback } }
fn option_contains(editor: &Editor, name: &str, item: &str, fallback: bool) -> bool { match editor.options().get_global(name) { Ok(OptionValue::String(value)) => value.split(',').any(|candidate| candidate == item), _ => fallback } }
fn push_jump(editor: &mut Editor, buffer: BufHandle, position: Position) { editor.jumplist_mut().push(MarkLocation::in_buffer(buffer, position)); }
