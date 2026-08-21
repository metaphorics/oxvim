//! Modal input state machine, shaped like `state.c`'s check/execute loop.

use ox_text::Position;
use ox_types::{BufHandle, WinHandle};
use thiserror::Error;

use crate::insert;
use crate::motion::{resolve, resolve_find, FindDirection, FindMotion};
use crate::ops::{self, EditRange, Operator};
use crate::search::{SearchDirection, SearchState};
use crate::textobject;
use crate::{BufferStateError, Editor, EditorError, Key, KeyDecodeError, MarkLocation, MotionKind, OptionValue, SearchError, VisualKind, VisualState};

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
    /// Direction selected by `/` or `?`.
    pub direction: SearchDirection,
    /// Pattern and offset text entered so far.
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
    /// Search parsing or execution failed.
    #[error(transparent)] Search(#[from] SearchError),
    /// Operator range application failed.
    #[error(transparent)] Operator(#[from] ops::OperatorError),
    /// Buffer text could not be read.
    #[error(transparent)] Buffer(#[from] BufferStateError),
    /// A named special key has no behavior in the current mode.
    #[error("non-character input is not supported in this modal state")]
    UnsupportedKey,
}

/// Stateful modal command processor.
#[derive(Clone, Debug, Default)]
pub struct ModeMachine {
    /// Currently active input mode.
    pub mode: Mode,
    search: SearchState,
    last_find: Option<FindMotion>,
    last_visual: Option<VisualState>,
    timestamp: i64,
}

struct Context { buffer: BufHandle, window: WinHandle, cursor: Position, lines: Vec<Vec<u8>>, topline: usize, bottomline: usize }

impl ModeMachine {
    /// Returns the active mode.
    #[must_use] pub const fn mode(&self) -> &Mode { &self.mode }
    /// Returns the last-search state.
    #[must_use] pub const fn search_state(&self) -> &SearchState { &self.search }

    /// `state.c:34-106`: checking consumes input but performs no buffer mutation.
    pub fn check(&mut self, editor: &mut Editor) -> Result<Step, ModeError> {
        let Some(key) = editor.typeahead_mut().pop()? else { return Ok(Step::Idle); };
        match key { Key::Byte(byte) => Ok(Step::Key(char::from(byte))), Key::Special(_, _) => Ok(Step::ProcessEvents) }
    }

    /// Executes the action classified by [`Self::check`].
    pub fn execute(&mut self, editor: &mut Editor, step: Step) -> Result<(), ModeError> {
        match step { Step::Idle | Step::ProcessEvents => Ok(()), Step::Key(key) => self.execute_key(editor, key) }
    }

    /// Runs one check/execute iteration, returning whether work was ready.
    pub fn run_once(&mut self, editor: &mut Editor) -> Result<bool, ModeError> {
        let step = self.check(editor)?; let ready = step != Step::Idle; self.execute(editor, step)?; Ok(ready)
    }

    /// Convenience entry point used by behavioral tests and embedding frontends.
    pub fn feed_keys(&mut self, editor: &mut Editor, keys: &str) -> Result<(), ModeError> {
        for key in keys.chars() { self.execute_key(editor, key)?; }
        Ok(())
    }

    fn execute_key(&mut self, editor: &mut Editor, key: char) -> Result<(), ModeError> {
        self.timestamp = self.timestamp.saturating_add(1);
        let mode = std::mem::take(&mut self.mode);
        self.mode = match mode {
            Mode::Normal(state) => self.normal(editor, state, key)?,
            Mode::Insert(state) => self.insert(editor, state, key)?,
            Mode::Visual(state) => self.visual(editor, state, key)?,
            Mode::Cmdline(state) => self.cmdline(editor, state, key)?,
            Mode::OperatorPending(state) => self.operator_pending(editor, state, key)?,
        };
        Ok(())
    }

    fn normal(&mut self, editor: &mut Editor, mut state: NormalState, key: char) -> Result<Mode, ModeError> {
        if state.prefix == "register" { state.register = Some(key); state.prefix.clear(); return Ok(Mode::Normal(state)); }
        if matches!(state.prefix.as_str(), "f" | "F" | "t" | "T") {
            let find = FindMotion { direction: if matches!(state.prefix.as_str(), "f" | "t") { FindDirection::Forward } else { FindDirection::Backward }, till: matches!(state.prefix.as_str(), "t" | "T"), target: key as u8 };
            self.move_find(editor, find, state.count.max(1), false)?; self.last_find = Some(find); return Ok(Mode::default());
        }
        if state.prefix == "g" {
            state.prefix.clear();
            if key == 'v' {
                if let Some(visual) = self.last_visual.clone() { editor.set_window_cursor(context(editor)?.window, visual.cursor)?; return Ok(Mode::Visual(visual)); }
                return Ok(Mode::default());
            }
            if key == 'u' || key == 'U' { return Ok(Mode::OperatorPending(OperatorPendingState { operator: if key == 'u' { Operator::Lowercase } else { Operator::Uppercase }, count: state.count.max(1), count_was_set: state.count != 0, motion_count: 0, register: state.register, prefix: String::new() })); }
            let command = format!("g{key}"); self.move_command(editor, &command, state.count.max(1), false)?; return Ok(Mode::default());
        }
        if key.is_ascii_digit() && (key != '0' || state.count != 0) { state.count = append_digit(state.count, key); return Ok(Mode::Normal(state)); }
        let count = state.count.max(1);
        match key {
            '"' => { state.prefix = "register".into(); Ok(Mode::Normal(state)) }
            'd' | 'c' | 'y' | '>' | '<' | '=' => Ok(Mode::OperatorPending(OperatorPendingState { operator: operator_for(key), count, count_was_set: state.count != 0, motion_count: 0, register: state.register, prefix: String::new() })),
            'g' => { state.prefix = "g".into(); Ok(Mode::Normal(state)) }
            'f' | 'F' | 't' | 'T' => { state.prefix = key.to_string(); Ok(Mode::Normal(state)) }
            ';' | ',' => { if let Some(mut find) = self.last_find { if key == ',' { find.direction = reverse_find(find.direction); } self.move_find(editor, find, count, false)?; } Ok(Mode::default()) }
            'h' | 'j' | 'k' | 'l' | 'w' | 'W' | 'e' | 'E' | 'b' | 'B' | '0' | '^' | '$' | '%' | '{' | '}' | '(' | ')' | 'G' | 'H' | 'M' | 'L' => { let command = if key == 'G' && state.count != 0 { "G_count".to_owned() } else { key.to_string() }; self.move_command(editor, &command, count, false)?; Ok(Mode::default()) }
            'i' => Ok(Mode::Insert(InsertState)),
            'a' => { self.advance_insert_cursor(editor, false)?; Ok(Mode::Insert(InsertState)) }
            'A' => { self.advance_insert_cursor(editor, true)?; Ok(Mode::Insert(InsertState)) }
            'I' => { self.move_command(editor, "^", 1, false)?; Ok(Mode::Insert(InsertState)) }
            'o' | 'O' => { self.open_line(editor, key == 'o')?; Ok(Mode::Insert(InsertState)) }
            'v' | 'V' | '\u{16}' => { let cursor = context(editor)?.cursor; Ok(Mode::Visual(VisualState::new(cursor, if key == 'v' { VisualKind::Character } else if key == 'V' { VisualKind::Line } else { VisualKind::Block }))) }
            '/' | '?' => Ok(Mode::Cmdline(CmdlineState { direction: if key == '/' { SearchDirection::Forward } else { SearchDirection::Backward }, text: String::new(), count })),
            'n' | 'N' => { self.repeat_search(editor, key == 'N', count)?; Ok(Mode::default()) }
            'x' => { let ctx = context(editor)?; let end = Position { lnum: ctx.cursor.lnum, col: ctx.cursor.col.saturating_add(count - 1) }; ops::apply(editor, ctx.buffer, ctx.window, Operator::Delete, EditRange { start: ctx.cursor, end, kind: MotionKind::CharacterWise, inclusive: true }, state.register, self.timestamp)?; Ok(Mode::default()) }
            '~' => { let ctx = context(editor)?; let end = Position { lnum: ctx.cursor.lnum, col: ctx.cursor.col.saturating_add(count - 1) }; ops::apply(editor, ctx.buffer, ctx.window, Operator::ToggleCase, EditRange { start: ctx.cursor, end, kind: MotionKind::CharacterWise, inclusive: true }, None, self.timestamp)?; self.move_command(editor, "l", count, false)?; Ok(Mode::default()) }
            'u' => { let ctx = context(editor)?; editor.buffer_undo(ctx.buffer)?; Ok(Mode::default()) }
            _ => Ok(Mode::default()),
        }
    }

    fn operator_pending(&mut self, editor: &mut Editor, mut state: OperatorPendingState, key: char) -> Result<Mode, ModeError> {
        if matches!(state.prefix.as_str(), "f" | "F" | "t" | "T") {
            let find = FindMotion { direction: if matches!(state.prefix.as_str(), "f" | "t") { FindDirection::Forward } else { FindDirection::Backward }, till: matches!(state.prefix.as_str(), "t" | "T"), target: key as u8 };
            let ctx = context(editor)?;
            let Some(motion) = resolve_find(&ctx.lines, ctx.cursor, find, state.count.saturating_mul(state.motion_count.max(1))) else { return Ok(Mode::default()); };
            self.apply_operator(editor, &state, EditRange::from_motion(ctx.cursor, motion))?; self.last_find = Some(find); return Ok(if state.operator == Operator::Change { Mode::Insert(InsertState) } else { Mode::default() });
        }
        if state.prefix == "g" { state.prefix.clear(); let command = format!("g{key}"); return self.finish_operator_motion(editor, state, &command); }
        if state.prefix == "i" || state.prefix == "a" {
            let inner = state.prefix == "i"; let ctx = context(editor)?;
            if let Some(range) = textobject::resolve(&ctx.lines, ctx.cursor, inner, key, state.count.saturating_mul(state.motion_count.max(1))) { let change = state.operator == Operator::Change; self.apply_operator(editor, &state, range)?; return Ok(if change { Mode::Insert(InsertState) } else { Mode::default() }); }
            return Ok(Mode::default());
        }
        if key.is_ascii_digit() && (key != '0' || state.motion_count != 0) { state.motion_count = append_digit(state.motion_count, key); return Ok(Mode::OperatorPending(state)); }
        if key == 'i' || key == 'a' || matches!(key, 'f' | 'F' | 't' | 'T') { state.prefix = key.to_string(); return Ok(Mode::OperatorPending(state)); }
        if key == 'g' { state.prefix = "g".into(); return Ok(Mode::OperatorPending(state)); }
        if key == operator_key(state.operator) { let ctx = context(editor)?; let end = Position { lnum: ctx.cursor.lnum.saturating_add(state.count.saturating_mul(state.motion_count.max(1)) - 1).min(ctx.lines.len()), col: 0 }; let range = EditRange { start: ctx.cursor, end, kind: MotionKind::LineWise, inclusive: true }; let change = state.operator == Operator::Change; self.apply_operator(editor, &state, range)?; return Ok(if change { Mode::Insert(InsertState) } else { Mode::default() }); }
        self.finish_operator_motion(editor, state, &key.to_string())
    }

    fn finish_operator_motion(&mut self, editor: &mut Editor, state: OperatorPendingState, command: &str) -> Result<Mode, ModeError> {
        let ctx = context(editor)?; let count = state.count.saturating_mul(state.motion_count.max(1));
        let current = ctx.lines.get(ctx.cursor.lnum.saturating_sub(1)).and_then(|line| line.get(ctx.cursor.col)).copied();
        let resolved_command = match (state.operator, command, current) {
            (Operator::Change, "w", Some(byte)) if !byte.is_ascii_whitespace() => "e",
            (Operator::Change, "W", Some(byte)) if !byte.is_ascii_whitespace() => "E",
            (_, "G", _) if state.count_was_set || state.motion_count != 0 => "G_count",
            _ => command,
        };
        if let Some(motion) = resolve(&ctx.lines, ctx.cursor, resolved_command, count, option_bool(editor, "startofline", true), (ctx.topline, ctx.bottomline)) { let change = state.operator == Operator::Change; if motion.is_jump { push_jump(editor, ctx.buffer, ctx.cursor); } self.apply_operator(editor, &state, EditRange::from_motion(ctx.cursor, motion))?; return Ok(if change { Mode::Insert(InsertState) } else { Mode::default() }); }
        Ok(Mode::default())
    }

    fn apply_operator(&mut self, editor: &mut Editor, state: &OperatorPendingState, range: EditRange) -> Result<(), ModeError> { let ctx = context(editor)?; ops::apply(editor, ctx.buffer, ctx.window, state.operator, range, state.register, self.timestamp)?; Ok(()) }

    fn visual(&mut self, editor: &mut Editor, mut state: VisualState, key: char) -> Result<Mode, ModeError> {
        if state.prefix == "g" {
            state.prefix.clear();
            if matches!(key, 'u' | 'U' | '~') {
                let operator = match key { 'u' => Operator::Lowercase, 'U' => Operator::Uppercase, _ => Operator::ToggleCase };
                return self.finish_visual_operator(editor, state, operator);
            }
            let ctx = context(editor)?; let command = format!("g{key}");
            if let Some(motion) = resolve(&ctx.lines, ctx.cursor, &command, state.count.max(1), option_bool(editor, "startofline", true), (ctx.topline, ctx.bottomline)) { editor.set_window_cursor(ctx.window, motion.target)?; state.extend(motion.target); }
            state.count = 0; return Ok(Mode::Visual(state));
        }
        if matches!(state.prefix.as_str(), "f" | "F" | "t" | "T") {
            if key.len_utf8() != 1 { state.prefix.clear(); return Ok(Mode::Visual(state)); }
            let find = FindMotion { direction: if matches!(state.prefix.as_str(), "f" | "t") { FindDirection::Forward } else { FindDirection::Backward }, till: matches!(state.prefix.as_str(), "t" | "T"), target: key as u8 };
            let ctx = context(editor)?; if let Some(motion) = resolve_find(&ctx.lines, ctx.cursor, find, state.count.max(1)) { editor.set_window_cursor(ctx.window, motion.target)?; state.extend(motion.target); self.last_find = Some(find); }
            state.prefix.clear(); state.count = 0; return Ok(Mode::Visual(state));
        }
        if state.prefix == "i" || state.prefix == "a" {
            let inner = state.prefix == "i"; let ctx = context(editor)?;
            if let Some(range) = textobject::resolve(&ctx.lines, ctx.cursor, inner, key, state.count.max(1)) { state.anchor = range.start; state.cursor = range.end; state.kind = match range.kind { MotionKind::LineWise => VisualKind::Line, MotionKind::BlockWise => VisualKind::Block, MotionKind::CharacterWise => VisualKind::Character }; editor.set_window_cursor(ctx.window, state.cursor)?; }
            state.prefix.clear(); state.count = 0; return Ok(Mode::Visual(state));
        }
        if key.is_ascii_digit() && (key != '0' || state.count != 0) { state.count = append_digit(state.count, key); return Ok(Mode::Visual(state)); }
        match key {
            '\u{1b}' => { self.last_visual = Some(state); Ok(Mode::default()) }
            'o' | 'O' => { if key == 'O' && state.kind == VisualKind::Block { state.swap_columns(); } else { state.swap_ends(); } let window = context(editor)?.window; editor.set_window_cursor(window, state.cursor)?; Ok(Mode::Visual(state)) }
            'g' | 'f' | 'F' | 't' | 'T' | 'i' | 'a' => { state.prefix = key.to_string(); Ok(Mode::Visual(state)) }
            'd' | 'c' | 'y' | '>' | '<' | '=' | 'u' | 'U' | '~' => self.finish_visual_operator(editor, state, match key { 'u' => Operator::Lowercase, 'U' => Operator::Uppercase, '~' => Operator::ToggleCase, _ => operator_for(key) }),
            _ => { let ctx = context(editor)?; if let Some(motion) = resolve(&ctx.lines, ctx.cursor, &key.to_string(), state.count.max(1), option_bool(editor, "startofline", true), (ctx.topline, ctx.bottomline)) { editor.set_window_cursor(ctx.window, motion.target)?; state.extend(motion.target); } state.count = 0; Ok(Mode::Visual(state)) }
        }
    }

    fn finish_visual_operator(&mut self, editor: &mut Editor, state: VisualState, operator: Operator) -> Result<Mode, ModeError> {
        let ctx = context(editor)?; self.last_visual = Some(state.clone()); let result = ops::apply(editor, ctx.buffer, ctx.window, operator, state.range(), None, self.timestamp)?; Ok(if result.enter_insert { Mode::Insert(InsertState) } else { Mode::default() })
    }

    fn insert(&mut self, editor: &mut Editor, _state: InsertState, key: char) -> Result<Mode, ModeError> {
        let ctx = context(editor)?;
        match key {
            '\u{1b}' => { insert::normal_cursor(editor, ctx.window, ctx.cursor)?; Ok(Mode::default()) }
            '\n' | '\r' => { insert::newline(editor, ctx.buffer, ctx.window, ctx.cursor, self.timestamp)?; Ok(Mode::Insert(InsertState)) }
            '\u{8}' | '\u{7f}' => { insert::backspace(editor, ctx.buffer, ctx.window, ctx.cursor, option_contains(editor, "backspace", "eol", true), self.timestamp)?; Ok(Mode::Insert(InsertState)) }
            ch if !ch.is_control() => { insert::insert_char(editor, ctx.buffer, ctx.window, ctx.cursor, ch, self.timestamp)?; Ok(Mode::Insert(InsertState)) }
            _ => Ok(Mode::Insert(InsertState)),
        }
    }

    fn cmdline(&mut self, editor: &mut Editor, mut state: CmdlineState, key: char) -> Result<Mode, ModeError> {
        match key {
            '\u{1b}' => Ok(Mode::default()),
            '\u{8}' | '\u{7f}' => { state.text.pop(); Ok(Mode::Cmdline(state)) }
            '\n' | '\r' => { let ctx = context(editor)?; let result = self.search.search(&ctx.lines, ctx.cursor, &state.text, state.direction, state.count.max(1), option_bool(editor, "wrapscan", true))?; push_jump(editor, ctx.buffer, ctx.cursor); editor.set_window_cursor(ctx.window, result.target)?; Ok(Mode::default()) }
            ch if !ch.is_control() => { state.text.push(ch); Ok(Mode::Cmdline(state)) }
            _ => Ok(Mode::Cmdline(state)),
        }
    }

    fn move_command(&mut self, editor: &mut Editor, command: &str, count: usize, visual: bool) -> Result<(), ModeError> { let ctx = context(editor)?; if let Some(motion) = resolve(&ctx.lines, ctx.cursor, command, count, option_bool(editor, "startofline", true), (ctx.topline, ctx.bottomline)) { if motion.is_jump && !visual { push_jump(editor, ctx.buffer, ctx.cursor); } editor.set_window_cursor(ctx.window, motion.target)?; } Ok(()) }
    fn move_find(&mut self, editor: &mut Editor, find: FindMotion, count: usize, _visual: bool) -> Result<(), ModeError> { let ctx = context(editor)?; if let Some(motion) = resolve_find(&ctx.lines, ctx.cursor, find, count) { editor.set_window_cursor(ctx.window, motion.target)?; } Ok(()) }
    fn repeat_search(&mut self, editor: &mut Editor, opposite: bool, count: usize) -> Result<(), ModeError> { let ctx = context(editor)?; let result = self.search.repeat(&ctx.lines, ctx.cursor, opposite, count, option_bool(editor, "wrapscan", true))?; push_jump(editor, ctx.buffer, ctx.cursor); editor.set_window_cursor(ctx.window, result.target)?; Ok(()) }
    fn advance_insert_cursor(&self, editor: &mut Editor, line_end: bool) -> Result<(), ModeError> { let ctx = context(editor)?; let line = &ctx.lines[ctx.cursor.lnum - 1]; let col = if line_end { line.len() } else { next_boundary(line, ctx.cursor.col) }; editor.set_window_cursor(ctx.window, Position { lnum: ctx.cursor.lnum, col })?; Ok(()) }
    fn open_line(&self, editor: &mut Editor, below: bool) -> Result<(), ModeError> { let ctx = context(editor)?; let after_line = if below { ctx.cursor.lnum } else { ctx.cursor.lnum.saturating_sub(1) }; let pos = Position { lnum: after_line + 1, col: 0 }; editor.append_buffer_lines(ctx.buffer, after_line, &[Vec::new()], ctx.cursor, self.timestamp)?; editor.set_window_cursor(ctx.window, pos)?; Ok(()) }
}

fn context(editor: &Editor) -> Result<Context, ModeError> {
    let tab = editor.current_tabpage().ok_or(EditorError::UnknownTabpage(ox_types::TabHandle::CURRENT))?;
    let tabpage = editor.tabpage(tab)?; let window = tabpage.current_window(); let height = tabpage.layout().window_geometry(window).map_err(EditorError::from)?.height;
    let state = editor.window(window)?; let buffer = state.buffer; let cursor = state.cursor; let topline = state.topline; let text = editor.buffer(buffer)?.text()?;
    let lines = (1..=text.line_count()).map(|lnum| text.line(lnum)).collect::<Result<Vec<_>, _>>().map_err(BufferStateError::from)?;
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
fn option_bool(editor: &Editor, name: &str, fallback: bool) -> bool { match editor.options().get_global(name) { Ok(OptionValue::Boolean(value)) => *value, _ => fallback } }
fn option_contains(editor: &Editor, name: &str, item: &str, fallback: bool) -> bool { match editor.options().get_global(name) { Ok(OptionValue::String(value)) => value.split(',').any(|candidate| candidate == item), _ => fallback } }
fn push_jump(editor: &mut Editor, buffer: BufHandle, position: Position) { editor.jumplist_mut().push(MarkLocation::in_buffer(buffer, position)); }
