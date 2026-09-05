//! Quickfix list storage, Vimscript builtins, and editor operations.

use crate::editor::{BufferRelease, Editor};
use crate::excmd_exec::buffer_lines;
use crate::layout::Geometry;
use ox_eval::{EvalError, Result};
use ox_text::{Buffer, Position};
use ox_types::{BufHandle, DictEntry, OxStr, Special, Typval, WinHandle};

use crate::options::OptionValue;
use crate::search::{SearchDirection, SearchState};
/// One entry in a quickfix or location list (`qfline_T`).
#[derive(Clone, Debug)]
pub struct QuickfixItem {
    /// Buffer number the entry points at, or 0 for a filename-only entry.
    pub bufnr: i64,
    /// Module name for non-buffer sources (e.g. a Python module path).
    pub module: OxStr,
    /// 1-based line number, or 0 when only `pattern` is set.
    pub lnum: i64,
    /// 1-based byte column, or 0.
    pub col: i64,
    /// End line for a multi-line entry, or 0.
    pub end_lnum: i64,
    /// End column for a multi-line entry, or 0.
    pub end_col: i64,
    /// Non-zero when `col` is a visual column rather than a byte column.
    pub vcol: i64,
    /// Error number from the compiler, or 0.
    pub nr: i64,
    /// Search pattern to locate the entry when `lnum` is 0.
    pub pattern: OxStr,
    /// Display text for the entry.
    pub text: OxStr,
    /// Single-letter type flag (`E`, `W`, `I`, etc.).
    pub item_type: OxStr,
    /// Whether the entry has a usable position.
    pub valid: bool,
    /// Arbitrary user data attached to the entry.
    pub user_data: Typval,
}

/// One list in the quickfix history (`qf_list_T`).
#[derive(Clone, Debug)]
pub struct QuickfixList {
    items: Vec<QuickfixItem>,
    title: OxStr,
    id: u64,
    idx: usize,
    changedtick: u64,
    context: Typval,
    quickfixtextfunc: Typval,
}

impl QuickfixList {
    fn new(id: u64, title: OxStr) -> Self {
        Self {
            items: Vec::new(),
            title,
            id,
            idx: 0,
            changedtick: 0,
            context: Typval::String(OxStr::from("")),
            quickfixtextfunc: Typval::String(OxStr::from("")),
        }
    }

    /// Returns the list entries.
    #[must_use]
    pub fn items(&self) -> &[QuickfixItem] {
        &self.items
    }
    /// Returns the list title.
    #[must_use]
    pub fn title(&self) -> &OxStr {
        &self.title
    }
    /// Returns the stable list identifier.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// One-based current entry, or zero for an empty list.
    #[must_use]
    pub const fn idx(&self) -> usize {
        self.idx
    }
    /// Monotonic tick bumped on every content or property change.
    #[must_use]
    pub const fn changedtick(&self) -> u64 {
        self.changedtick
    }
    /// Replaces all entries and resets the cursor to the first valid one.
    pub fn set_items(&mut self, items: Vec<QuickfixItem>) {
        self.items = items;
        self.idx = self
            .items
            .iter()
            .position(|item| item.valid)
            .map_or(0, |index| index + 1);
        self.changedtick = self.changedtick.saturating_add(1);
    }
    /// Appends entries, selecting the first valid one if the list was empty.
    pub fn append_items(&mut self, items: Vec<QuickfixItem>) {
        let was_empty = self.items.is_empty();
        self.items.extend(items);
        if was_empty {
            self.idx = self
                .items
                .iter()
                .position(|item| item.valid)
                .map_or(0, |index| index + 1);
        }
        self.changedtick = self.changedtick.saturating_add(1);
    }

    /// Bumps the tick for a what-dict property change (`qf_set_properties`
    /// marks the list changed for title/context edits too).
    pub fn touch(&mut self) {
        self.changedtick = self.changedtick.saturating_add(1);
    }

    fn set_idx(&mut self, idx: i64) {
        if idx <= 0 {
            self.idx = self
                .items
                .iter()
                .position(|item| item.valid)
                .map_or(0, |index| index + 1);
        } else {
            self.idx = usize::try_from(idx)
                .unwrap_or(usize::MAX)
                .min(self.items.len());
        }
    }
}

/// Global quickfix history (`qf_info_T`).
#[derive(Clone, Debug)]
pub struct QuickfixStack {
    lists: Vec<QuickfixList>,
    current: usize,
    next_id: u64,
    buffer: Option<BufHandle>,
    window: Option<WinHandle>,
}

impl Default for QuickfixStack {
    fn default() -> Self {
        Self::new()
    }
}

impl QuickfixStack {
    /// Creates an empty quickfix stack with no lists.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            lists: Vec::new(),
            current: 0,
            next_id: 1,
            buffer: None,
            window: None,
        }
    }
    /// Returns the current (active) list, if any.
    #[must_use]
    pub fn current(&self) -> Option<&QuickfixList> {
        self.lists.get(self.current)
    }
    /// Returns mutable access to the current list, if any.
    pub fn current_mut(&mut self) -> Option<&mut QuickfixList> {
        self.lists.get_mut(self.current)
    }

    /// Returns mutable access to the list at `index`.
    pub fn list_mut(&mut self, index: usize) -> &mut QuickfixList {
        &mut self.lists[index]
    }

    /// Pushes a fresh list onto the history and makes it current.
    pub fn push(&mut self, title: OxStr) -> usize {
        self.lists.truncate(self.current + 1);
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.lists.push(QuickfixList::new(id, title));
        self.current = self.lists.len() - 1;
        self.current
    }
    /// Number of lists in the history.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lists.len()
    }

    /// Returns whether the history contains no lists.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lists.is_empty()
    }

    /// One-based position of the current list, or 0 when empty.
    #[must_use]
    pub const fn current_number(&self) -> usize {
        if self.lists.is_empty() {
            0
        } else {
            self.current + 1
        }
    }
    /// Buffer backing the quickfix window, if one has been created.
    #[must_use]
    pub const fn buffer(&self) -> Option<BufHandle> {
        self.buffer
    }
    /// Window displaying the quickfix list, if one is open.
    #[must_use]
    pub const fn window(&self) -> Option<WinHandle> {
        self.window
    }
    /// Removes all lists and resets the cursor.
    pub fn clear(&mut self) {
        self.lists.clear();
        self.current = 0;
    }

    /// `:colder`/`:cnewer`: walk the list history. E380 before the first
    /// list, E381 after the last (`ex_colder` / `ex_cnewer`).
    ///
    /// # Errors
    ///
    /// Returns E42 when the history is empty, E380 before the first list, or
    /// E381 after the last list.
    pub fn shift_history(&mut self, delta: i32) -> std::result::Result<(), QuickfixError> {
        if self.lists.is_empty() {
            return Err(QuickfixError::no_errors());
        }
        let next = i32::try_from(self.current)
            .unwrap_or(i32::MAX)
            .saturating_add(delta);
        if next < 0 {
            return Err(QuickfixError {
                code: "E380",
                message: "At bottom of quickfix stack".to_owned(),
            });
        }
        let last = i32::try_from(self.lists.len() - 1).unwrap_or(i32::MAX);
        if next > last {
            return Err(QuickfixError {
                code: "E381",
                message: "At top of quickfix stack".to_owned(),
            });
        }
        self.current = usize::try_from(next).unwrap_or(0);
        Ok(())
    }

    fn select(&self, what: &[DictEntry]) -> Option<usize> {
        let mut selected = self.current;
        if let Some(value) = dict_value(what, "nr") {
            selected = match value {
                Typval::Number(0) => selected,
                Typval::Number(number) if *number > 0 => usize::try_from(*number - 1).ok()?,
                Typval::String(value) if value.as_bytes() == b"$" => {
                    self.lists.len().checked_sub(1)?
                }
                _ => return None,
            };
        }
        if let Some(value) = dict_value(what, "id") {
            match value {
                Typval::Number(0) => {}
                Typval::Number(id) if *id > 0 => {
                    let id = u64::try_from(*id).ok()?;
                    selected = self.lists.iter().position(|list| list.id == id)?;
                }
                _ => return None,
            }
        }
        (selected < self.lists.len()).then_some(selected)
    }

    fn target_for_set(&mut self, action: char, what: &[DictEntry], title: OxStr) -> Option<usize> {
        if action == ' ' || self.lists.is_empty() {
            return Some(self.push(title));
        }
        let selected = self.select(what)?;
        self.current = selected;
        Some(selected)
    }

    fn move_entry(
        &mut self,
        movement: QuickfixMove,
    ) -> std::result::Result<&QuickfixItem, QuickfixError> {
        let quickfix_list = self.current_mut().ok_or(QuickfixError::no_errors())?;
        if quickfix_list.items.is_empty() {
            return Err(QuickfixError::no_errors());
        }
        let current = quickfix_list.idx.saturating_sub(1);
        // Upstream `ex_cc` clamps an out-of-range entry number to the last
        // entry; `ex_cnext`/`ex_cprev` step at most `count` times, stopping at
        // the end entry, and fail E553 only when the first step cannot move.
        let last_index = quickfix_list.items.len() - 1;
        let target = match movement {
            QuickfixMove::Absolute(index) => index.min(last_index),
            QuickfixMove::First => 0,
            QuickfixMove::Last => last_index,
            QuickfixMove::Next(count) => current.saturating_add(count).min(last_index),
            QuickfixMove::Previous(count) => {
                if current == 0 {
                    return Err(QuickfixError::before_first());
                }
                current.saturating_sub(count)
            }
        };
        let mut selected = target;
        // `:cfirst`/`:clast` select the first/last *valid* entry, like the
        // stepped moves; only `:cc` addresses a literal index.
        if !matches!(movement, QuickfixMove::Absolute(_)) {
            let forward = !matches!(movement, QuickfixMove::Previous(_) | QuickfixMove::Last);
            loop {
                if quickfix_list
                    .items
                    .get(selected)
                    .is_some_and(|item| item.valid)
                {
                    break;
                }
                if forward {
                    if selected >= last_index {
                        return Err(QuickfixError::beyond_last());
                    }
                    selected += 1;
                } else if selected == 0 {
                    return Err(QuickfixError::before_first());
                } else {
                    selected -= 1;
                }
            }
        }
        quickfix_list.idx = selected + 1;
        Ok(&quickfix_list.items[selected])
    }
}

/// Cursor movement for `:cnext`/`:cprev`/`:cc`/`:cfirst`/`:clast`.
#[derive(Clone, Copy, Debug)]
pub enum QuickfixMove {
    /// Jump to a one-based entry index.
    Absolute(usize),
    /// Jump to the first valid entry.
    First,
    /// Jump to the last valid entry.
    Last,
    /// Move forward by `N` valid entries.
    Next(usize),
    /// Move backward by `N` valid entries.
    Previous(usize),
}

/// Quickfix operation failure with a Vim-style error code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuickfixError {
    /// Traditional Vim error code (`E42`, `E553`, etc.).
    pub code: &'static str,
    /// Message text without the `E123: ` prefix.
    pub message: String,
}

/// Whether `window` is pinned to its buffer by 'winfixbuf'.
fn window_fixed(editor: &Editor, window: WinHandle) -> bool {
    editor
        .options()
        .get_window(window, "winfixbuf")
        .is_ok_and(|value| matches!(value, OptionValue::Boolean(true)))
}

impl QuickfixError {
    fn no_errors() -> Self {
        Self {
            code: "E42",
            message: "No Errors".to_owned(),
        }
    }
    fn before_first() -> Self {
        Self {
            code: "E553",
            message: "No more items".to_owned(),
        }
    }
    fn beyond_last() -> Self {
        Self {
            code: "E553",
            message: "No more items".to_owned(),
        }
    }
    fn editor(message: &(impl ToString + ?Sized)) -> Self {
        Self {
            code: "E925",
            message: message.to_string(),
        }
    }
    fn winfixbuf() -> Self {
        Self {
            code: "E1513",
            message: "Cannot switch buffer. 'winfixbuf' is enabled".to_owned(),
        }
    }
}

/// Dispatches the editor-stateful quickfix builtins.
pub(crate) fn call(editor: &mut Editor, name: &str, args: &[Typval]) -> Result<Typval> {
    check_arity(name, args.len())?;
    match name {
        "setqflist" => setqflist(editor, args),
        "getqflist" => getqflist(editor, args),
        "setloclist" => setloclist(editor, args),
        "getloclist" => getloclist(editor, args),
        _ => unreachable!("quickfix builtin router and dispatcher disagree"),
    }
}

fn check_arity(name: &str, count: usize) -> Result<()> {
    let (min_args, max_args) = match name {
        "setqflist" => (1, Some(3)),
        "getqflist" => (0, Some(1)),
        "setloclist" => (2, Some(4)),
        "getloclist" => (1, Some(2)),
        _ => (0, None),
    };
    if count < min_args {
        return Err(EvalError::new(
            "E119",
            0,
            format!("Not enough arguments for function: {name}"),
        ));
    }
    if max_args.is_some_and(|maximum| count > maximum) {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    Ok(())
}

/// `setloclist({nr}, {list}, ...)`: window-local lists share the quickfix
/// stack until per-window loclists land; winid 0 is the current window.
/// Validates the window number, then applies `setqflist` semantics.
fn setloclist(editor: &mut Editor, args: &[Typval]) -> Result<Typval> {
    match &args[0] {
        Typval::Number(_) | Typval::Bool(_) => {}
        _ => return Err(EvalError::new("E745", 0, "Using a List as a Number")),
    }
    setqflist(editor, &args[1..])
}

fn getloclist(editor: &mut Editor, args: &[Typval]) -> Result<Typval> {
    match &args[0] {
        Typval::Number(_) | Typval::Bool(_) => {}
        _ => return Err(EvalError::new("E745", 0, "Using a List as a Number")),
    }
    getqflist(editor, &args[1..])
}

/// Applies `setqflist` to the editor's quickfix stack.
///
/// # Errors
///
/// Returns E714 when the first argument is not a list, E742 when a list or
/// dictionary is locked, E927 for an invalid action, E1174 when the action
/// is not a string, E475 for contradictory arguments, and E715 when the
/// `what` argument is not a dictionary.
fn setqflist(editor: &mut Editor, args: &[Typval]) -> Result<Typval> {
    let Typval::List(items_ref) = &args[0] else {
        return Err(EvalError::new("E714", 0, "List required"));
    };
    let raw_items = items_ref
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?
        .items
        .clone();
    let action = match args.get(1) {
        None => ' ',
        Some(Typval::String(value)) if value.as_bytes().len() == 1 => {
            let action = char::from(value.as_bytes()[0]);
            if matches!(action, 'a' | 'r' | 'u' | ' ' | 'f') {
                action
            } else {
                return Err(EvalError::new(
                    "E927",
                    0,
                    format!("Invalid action: '{}'", value.to_string_lossy()),
                ));
            }
        }
        Some(Typval::String(value)) => {
            return Err(EvalError::new(
                "E927",
                0,
                format!("Invalid action: '{}'", value.to_string_lossy()),
            ));
        }
        Some(_) => return Err(EvalError::new("E1174", 0, "String required for argument 2")),
    };
    if action == 'f' {
        editor.quickfix_mut().clear();
        return Ok(Typval::Number(0));
    }

    let (what, legacy_title) = match args.get(2) {
        None => (Vec::new(), None),
        Some(Typval::Dict(reference)) => {
            let entries = reference
                .try_borrow()
                .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?
                .entries
                .clone();
            if !raw_items.is_empty() {
                return Err(EvalError::new(
                    "E475",
                    0,
                    "Invalid argument: cannot have both a list and a \"what\" argument",
                ));
            }
            (entries, None)
        }
        Some(Typval::String(title)) => (Vec::new(), Some(title.clone())),
        Some(_) => return Err(EvalError::new("E715", 0, "Dictionary required")),
    };

    // A locked `items` list is E742 and a non-List value is E1211;
    // upstream `qf_set_properties` rejects both and leaves the existing
    // list untouched (quickfix.c) — never substitute an empty item set,
    // which would silently wipe the list.
    let source_items = match dict_value(&what, "items") {
        None => raw_items,
        Some(Typval::List(reference)) => match reference.try_borrow() {
            Ok(list) => list.items.clone(),
            Err(_) => {
                return Err(EvalError::new("E742", 0, "Cannot change value"));
            }
        },
        Some(_) => {
            return Err(EvalError::new("E1211", 0, "List required for argument 3"));
        }
    };
    let parsed_items = parse_items(editor, &source_items)?;
    let title = legacy_title
        .or_else(|| dict_string(&what, "title"))
        .unwrap_or_else(|| OxStr::from(":setqflist()"));
    let target = editor
        .quickfix_mut()
        .target_for_set(action, &what, title)
        .ok_or_else(|| EvalError::new("E475", 0, "Invalid argument"))?;
    apply_what_fields(
        editor.quickfix_mut().list_mut(target),
        &what,
        action,
        !source_items.is_empty(),
        parsed_items,
    );
    Ok(Typval::Number(0))
}

/// Applies the `what` dictionary fields to the target list, preserving the
/// setqflist event order: touch, title, context, quickfixtextfunc, items,
/// then cursor index.
fn apply_what_fields(
    list: &mut QuickfixList,
    what: &[DictEntry],
    action: char,
    has_source_items: bool,
    parsed_items: Vec<QuickfixItem>,
) {
    if dict_string(what, "title").is_some()
        || dict_value(what, "context").is_some()
        || dict_value(what, "quickfixtextfunc").is_some()
    {
        list.touch();
    }
    if let Some(title) = dict_string(what, "title") {
        list.title = title;
    }
    if let Some(context) = dict_value(what, "context") {
        list.context = context.clone();
    }
    if let Some(function) = dict_value(what, "quickfixtextfunc") {
        list.quickfixtextfunc = function.clone();
    }
    if dict_value(what, "items").is_some() || has_source_items || what.is_empty() {
        if action == 'a' {
            list.append_items(parsed_items);
        } else {
            list.set_items(parsed_items);
        }
    }
    if let Some(idx) = dict_number(what, "idx") {
        list.set_idx(idx);
    }
}

/// Answers the editor-stateful `getqflist()` builtin.
///
/// # Errors
///
/// Returns E742 when the argument dictionary is locked and E715 when the
/// first argument is not a dictionary.
fn getqflist(editor: &Editor, args: &[Typval]) -> Result<Typval> {
    if args.is_empty() {
        return Ok(Typval::list(
            editor.quickfix().current().map_or_else(Vec::new, |list| {
                list.items.iter().map(item_typval).collect()
            }),
        ));
    }
    let Typval::Dict(reference) = &args[0] else {
        return Err(EvalError::new("E715", 0, "Dictionary required"));
    };
    let what = reference
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?
        .entries
        .clone();
    let all = dict_number(&what, "all").is_some_and(|value| value != 0);
    let selected = editor
        .quickfix()
        .select(&what)
        .and_then(|index| editor.quickfix().lists.get(index));
    let wants = |key: &str| all || dict_value(&what, key).is_some();
    let mut answer = Vec::new();
    if wants("title") {
        answer.push(pair(
            "title",
            selected.map_or_else(
                || Typval::String(OxStr::from("")),
                |list| Typval::String(list.title.clone()),
            ),
        ));
    }
    if wants("items") {
        answer.push(pair(
            "items",
            Typval::list(selected.map_or_else(Vec::new, |list| {
                list.items.iter().map(item_typval).collect()
            })),
        ));
    }
    if wants("context") {
        answer.push(pair(
            "context",
            selected.map_or_else(
                || Typval::String(OxStr::from("")),
                |list| list.context.clone(),
            ),
        ));
    }
    if wants("quickfixtextfunc") {
        answer.push(pair(
            "quickfixtextfunc",
            selected.map_or_else(
                || Typval::String(OxStr::from("")),
                |list| list.quickfixtextfunc.clone(),
            ),
        ));
    }
    push_list_numbers(editor, &what, all, &mut answer);
    push_stack_handles(editor, &what, all, &mut answer);
    if let (Some(list), true) = (selected, wants("id")) {
        answer.push(pair(
            "id",
            Typval::Number(i64::try_from(list.id).unwrap_or(i64::MAX)),
        ));
    }
    if let (Some(list), true) = (selected, wants("idx")) {
        answer.push(pair(
            "idx",
            Typval::Number(i64::try_from(list.idx).unwrap_or(i64::MAX)),
        ));
    }
    if let (Some(list), true) = (selected, wants("size")) {
        answer.push(pair(
            "size",
            Typval::Number(i64::try_from(list.items.len()).unwrap_or(i64::MAX)),
        ));
    }
    if let (Some(list), true) = (selected, wants("changedtick")) {
        answer.push(pair(
            "changedtick",
            Typval::Number(i64::try_from(list.changedtick).unwrap_or(i64::MAX)),
        ));
    }
    Ok(Typval::dict(answer))
}
/// Appends the list-numbering metadata key (`nr`) for `getqflist`.
///
/// `nr` reports the one-based position of the selected list, or 0 when no
/// list matches the `what` selector.
fn push_list_numbers(
    editor: &Editor,
    what: &[DictEntry],
    all: bool,
    answer: &mut Vec<(OxStr, Typval)>,
) {
    if all || dict_value(what, "nr").is_some() {
        let number_usize = editor
            .quickfix()
            .select(what)
            .map_or(0_usize, |index| index + 1);
        let number = i64::try_from(number_usize).unwrap_or(i64::MAX);
        answer.push(pair("nr", Typval::Number(number)));
    }
}

/// Appends the stack-handle metadata keys (`winid`, `qfbufnr`) that report
/// the quickfix window and buffer when they still exist.
fn push_stack_handles(
    editor: &Editor,
    what: &[DictEntry],
    all: bool,
    answer: &mut Vec<(OxStr, Typval)>,
) {
    if all || dict_value(what, "winid").is_some() {
        let window = editor
            .quickfix()
            .window
            .filter(|window| editor.window(*window).is_ok());
        answer.push(pair("winid", Typval::Number(window.map_or(0, i64::from))));
    }
    if all || dict_value(what, "qfbufnr").is_some() {
        let buffer = editor
            .quickfix()
            .buffer
            .filter(|buffer| editor.buffer(*buffer).is_ok());
        answer.push(pair("qfbufnr", Typval::Number(buffer.map_or(0, i64::from))));
    }
}

/// Parses quickfix list entries from Vimscript values.
///
/// # Errors
///
/// Returns E742 when an entry list or dictionary is locked, E948 when buffer
/// creation fails, and E86 when the new buffer cannot be renamed.
pub(crate) fn parse_items(editor: &mut Editor, values: &[Typval]) -> Result<Vec<QuickfixItem>> {
    let mut items = Vec::new();
    for value in values {
        let Typval::Dict(reference) = value else {
            continue;
        };
        let entries = reference
            .try_borrow()
            .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?
            .entries
            .clone();
        let mut bufnr = dict_number(&entries, "bufnr").unwrap_or(0);
        if bufnr == 0
            && let Some(filename) = dict_string(&entries, "filename")
            && !filename.as_bytes().is_empty()
        {
            let existing = editor.buffers().into_iter().find(|buffer| {
                editor
                    .buffer(*buffer)
                    .is_ok_and(|state| state.name() == &filename)
            });
            let buffer = if let Some(buffer) = existing {
                buffer
            } else {
                let buffer = editor
                    .create_buffer(true)
                    .map_err(|error| EvalError::new("E948", 0, error.to_string()))?;
                editor
                    .buffer_mut(buffer)
                    .map_err(|error| EvalError::new("E86", 0, error.to_string()))?
                    .set_name(filename);
                buffer
            };
            bufnr = i64::from(buffer);
        }
        if bufnr != 0
            && BufHandle::try_from(bufnr)
                .ok()
                .is_none_or(|buffer| editor.buffer(buffer).is_err())
        {
            bufnr = 0;
        }
        let lnum = dict_number(&entries, "lnum").unwrap_or(0);
        let pattern = dict_string(&entries, "pattern").unwrap_or_else(|| OxStr::from(""));
        let detected_valid = bufnr != 0 && (lnum != 0 || !pattern.as_bytes().is_empty());
        items.push(QuickfixItem {
            bufnr,
            module: dict_string(&entries, "module").unwrap_or_else(|| OxStr::from("")),
            lnum,
            col: dict_number(&entries, "col").unwrap_or(0),
            end_lnum: dict_number(&entries, "end_lnum").unwrap_or(0),
            end_col: dict_number(&entries, "end_col").unwrap_or(0),
            vcol: dict_number(&entries, "vcol").unwrap_or(0),
            nr: dict_number(&entries, "nr").unwrap_or(0),
            pattern,
            text: dict_string(&entries, "text").unwrap_or_else(|| OxStr::from("")),
            item_type: dict_string(&entries, "type").unwrap_or_else(|| OxStr::from("")),
            valid: dict_number(&entries, "valid").map_or(detected_valid, |value| value != 0),
            user_data: dict_value(&entries, "user_data")
                .cloned()
                .unwrap_or(Typval::Special(Special::Null)),
        });
    }
    Ok(items)
}

fn item_typval(item: &QuickfixItem) -> Typval {
    let mut entries = vec![
        pair("bufnr", Typval::Number(item.bufnr)),
        pair("module", Typval::String(item.module.clone())),
        pair("lnum", Typval::Number(item.lnum)),
        pair("end_lnum", Typval::Number(item.end_lnum)),
        pair("col", Typval::Number(item.col)),
        pair("end_col", Typval::Number(item.end_col)),
        pair("vcol", Typval::Number(item.vcol)),
        pair("nr", Typval::Number(item.nr)),
        pair("pattern", Typval::String(item.pattern.clone())),
        pair("text", Typval::String(item.text.clone())),
        pair("type", Typval::String(item.item_type.clone())),
        pair("valid", Typval::Number(i64::from(item.valid))),
    ];
    if !matches!(item.user_data, Typval::Special(Special::Null)) {
        entries.push(pair("user_data", item.user_data.clone()));
    }
    Typval::dict(entries)
}

fn pair(name: &str, value: Typval) -> (OxStr, Typval) {
    (OxStr::from(name), value)
}

fn dict_value<'a>(entries: &'a [DictEntry], name: &str) -> Option<&'a Typval> {
    entries
        .iter()
        .find(|entry| entry.key.as_bytes() == name.as_bytes())
        .map(|entry| &entry.value)
}

fn dict_number(entries: &[DictEntry], name: &str) -> Option<i64> {
    match dict_value(entries, name) {
        Some(Typval::Number(value)) => Some(*value),
        Some(Typval::Bool(value)) => Some(i64::from(*value)),
        _ => None,
    }
}

fn dict_string(entries: &[DictEntry], name: &str) -> Option<OxStr> {
    match dict_value(entries, name) {
        Some(Typval::String(value)) => Some(value.clone()),
        _ => None,
    }
}

/// Opens or refreshes the global quickfix window.
///
/// # Errors
///
/// Returns E925 when the backing buffer cannot be read or rewritten, a new
/// buffer cannot be created or configured, or window layout operations fail.
pub fn open(editor: &mut Editor) -> std::result::Result<WinHandle, QuickfixError> {
    let lines = editor.quickfix().current().map_or_else(
        || vec![Vec::new()],
        |list| {
            if list.items.is_empty() {
                vec![Vec::new()]
            } else {
                list.items.iter().map(format_item).collect()
            }
        },
    );
    let existing_buffer = editor
        .quickfix()
        .buffer
        .filter(|buffer| editor.buffer(*buffer).is_ok());
    let buffer = if let Some(buffer) = existing_buffer {
        let count = editor
            .buffer(buffer)
            .map_err(|error| QuickfixError::editor(&error))?
            .text()
            .map_err(|error| QuickfixError::editor(&error))?
            .line_count();
        editor
            .replace_buffer_lines(crate::LineReplaceRequest {
                buffer,
                start: 1,
                end: count,
                lines: &lines,
                cursor_before: Position { lnum: 1, col: 0 },
                cursor_after: Position { lnum: 1, col: 0 },
                timestamp: 0,
            })
            .map_err(|error| QuickfixError::editor(&error))?;
        buffer
    } else {
        let text =
            Buffer::from_lines(&lines, true).map_err(|error| QuickfixError::editor(&error))?;
        let buffer = editor
            .create_buffer_with(text, false)
            .map_err(|error| QuickfixError::editor(&error))?;
        editor
            .buffer_mut(buffer)
            .map_err(|error| QuickfixError::editor(&error))?
            .set_name(OxStr::from("quickfix"));
        editor
            .options_mut()
            .set_buffer(
                buffer,
                "buftype",
                OptionValue::String("quickfix".to_owned()),
            )
            .map_err(|error| QuickfixError::editor(&error))?;
        editor.quickfix_mut().buffer = Some(buffer);
        buffer
    };

    if let Some(window) = editor
        .quickfix()
        .window
        .filter(|window| editor.window(*window).is_ok())
    {
        editor
            .set_current_window(window)
            .map_err(|error| QuickfixError::editor(&error))?;
        // The recorded window can outlive its buffer (a wiped buffer, a
        // manual `:buffer` inside it): point it back at the quickfix buffer
        // unless it is pinned to something else.
        let shows = editor.window(window).map(|state| state.buffer);
        if shows.is_ok_and(|shown| shown != buffer) && !window_fixed(editor, window) {
            editor
                .set_current_buffer(buffer, BufferRelease::KeepLoaded)
                .map_err(|error| QuickfixError::editor(&error))?;
        }
        return Ok(window);
    }
    let window =
        if let (Some(tab), Some(current)) = (editor.current_tabpage(), editor.current_window()) {
            editor
                .split_horizontal(tab, current, buffer, true)
                .map_err(|error| QuickfixError::editor(&error))?
        } else {
            let tab = editor
                .create_tabpage(
                    buffer,
                    Geometry::new(0, 0, 80, 24).map_err(|error| QuickfixError::editor(&error))?,
                )
                .map_err(|error| QuickfixError::editor(&error))?;
            editor
                .tabpage(tab)
                .map_err(|error| QuickfixError::editor(&error))?
                .current_window()
        };
    editor.quickfix_mut().window = Some(window);
    editor
        .set_current_window(window)
        .map_err(|error| QuickfixError::editor(&error))?;
    Ok(window)
}

/// Closes the quickfix window if one is open.
///
/// # Errors
///
/// Returns E925 when locating the window's tabpage or closing the window
/// fails.
pub fn close(editor: &mut Editor) -> std::result::Result<(), QuickfixError> {
    let Some(window) = editor
        .quickfix()
        .window
        .filter(|window| editor.window(*window).is_ok())
    else {
        return Ok(());
    };
    let tab = editor
        .window_tabpage(window)
        .map_err(|error| QuickfixError::editor(&error))?;
    editor
        .close_window(tab, window, true)
        .map_err(|error| QuickfixError::editor(&error))?;
    editor.quickfix_mut().window = None;
    Ok(())
}

/// Locates a pattern-only entry in the target buffer: upstream `qf_jump`
/// searches `pattern` when `lnum` is 0. Returns `None` when the buffer is
/// unreadable, the pattern rejects, or nothing matches; the caller then
/// jumps to the top like before.
fn pattern_position(editor: &Editor, buffer: BufHandle, pattern: &OxStr) -> Option<Position> {
    let lines = buffer_lines(editor, buffer).ok()?;
    let result = SearchState::default()
        .search(
            &lines,
            Position { lnum: 1, col: 0 },
            &pattern.to_string_lossy(),
            SearchDirection::Forward,
            1,
            true,
        )
        .ok()?;
    Some(result.target)
}

/// Restores a pre-jump quickfix cursor after a failed target switch:
/// upstream `qf_jump` puts the index back when the jump fails
/// (quickfix.c:3320-3331), so a failed `:cnext` never consumes an entry.
fn restore_idx(editor: &mut Editor, idx: Option<usize>) {
    if let (Some(idx), Some(list)) = (idx, editor.quickfix_mut().current_mut()) {
        list.idx = idx;
    }
}

/// Moves the quickfix cursor and switches to the target buffer/window.
///
/// # Errors
///
/// Returns E42 when there is no list or entry, E553 when the cursor cannot
/// move, and E925 when buffer or window state changes fail.
pub fn jump(
    editor: &mut Editor,
    movement: QuickfixMove,
    forceit: bool,
) -> std::result::Result<(), QuickfixError> {
    let previous_idx = editor.quickfix().current().map(QuickfixList::idx);
    let item = editor.quickfix_mut().move_entry(movement)?.clone();
    let buffer = BufHandle::try_from(item.bufnr).map_err(|_| {
        restore_idx(editor, previous_idx);
        QuickfixError::no_errors()
    })?;
    if editor.buffer(buffer).is_err() {
        restore_idx(editor, previous_idx);
        return Err(QuickfixError::no_errors());
    }
    // `qf_jump_edit_buffer` (quickfix.c:2969-3006): a 'winfixbuf' window
    // never switches buffers for an entry. Prefer the previous window when
    // it is free, else split a fresh window; only give up with E1513 when
    // neither is possible.
    if !forceit
        && editor.current_window_fixed_to_buffer()
        && editor.current_buffer() != Some(buffer)
    {
        let quickfix_buffer = editor.quickfix().buffer;
        let previous = editor.previous_window().filter(|window| {
            !window_fixed(editor, *window)
                && editor
                    .window(*window)
                    .is_ok_and(|state| Some(state.buffer) != quickfix_buffer)
        });
        if let Some(previous) = previous
            && editor.set_current_window(previous).is_err()
        {
            restore_idx(editor, previous_idx);
            return Err(QuickfixError::winfixbuf());
        }
        if editor.current_window_fixed_to_buffer() {
            let tab = editor.current_tabpage().ok_or_else(|| {
                restore_idx(editor, previous_idx);
                QuickfixError::no_errors()
            })?;
            let current_window = editor.current_window().ok_or_else(|| {
                restore_idx(editor, previous_idx);
                QuickfixError::no_errors()
            })?;
            let current_buffer = editor.current_buffer().ok_or_else(|| {
                restore_idx(editor, previous_idx);
                QuickfixError::no_errors()
            })?;
            editor
                .split_horizontal(tab, current_window, current_buffer, true)
                .map_err(|_| {
                    restore_idx(editor, previous_idx);
                    QuickfixError::winfixbuf()
                })?;
            if editor.current_window_fixed_to_buffer() {
                restore_idx(editor, previous_idx);
                return Err(QuickfixError::winfixbuf());
            }
        }
    }
    let qf_window = editor
        .quickfix()
        .window
        .filter(|window| editor.current_window() == Some(*window));
    if qf_window.is_some() {
        // Upstream skips pinned windows when choosing a jump target
        // (quickfix.c:2868-2885): a jump from the quickfix window must not
        // route the entry into a 'winfixbuf' window.
        let quickfix_buffer = editor.quickfix().buffer;
        let target = editor.windows().into_iter().find(|window| {
            Some(*window) != qf_window
                && !window_fixed(editor, *window)
                && editor
                    .window(*window)
                    .is_ok_and(|state| Some(state.buffer) != quickfix_buffer)
        });
        if let Some(target) = target {
            editor
                .set_current_window(target)
                .map_err(|error| QuickfixError::editor(&error))?;
        }
    }
    editor
        .set_current_buffer(buffer, BufferRelease::KeepLoaded)
        .map_err(|error| QuickfixError::editor(&error))?;
    let line_count = editor
        .buffer(buffer)
        .map_err(|error| QuickfixError::editor(&error))?
        .text()
        .map_err(|error| QuickfixError::editor(&error))?
        .line_count();
    let patterned = (item.lnum == 0 && !item.pattern.as_bytes().is_empty())
        .then(|| pattern_position(editor, buffer, &item.pattern))
        .flatten();
    let (lnum, col) = match patterned {
        Some(found) => (found.lnum, found.col),
        None => (
            usize::try_from(item.lnum.max(1))
                .unwrap_or(1)
                .min(line_count.max(1)),
            usize::try_from(item.col.saturating_sub(1).max(0)).unwrap_or(0),
        ),
    };
    let window = editor
        .current_window()
        .ok_or_else(QuickfixError::no_errors)?;
    editor
        .window_mut(window)
        .map_err(|error| QuickfixError::editor(&error))?
        .cursor = Position { lnum, col };
    Ok(())
}

fn format_item(item: &QuickfixItem) -> Vec<u8> {
    let name = BufHandle::try_from(item.bufnr)
        .ok()
        .map_or_else(String::new, |buffer| i64::from(buffer).to_string());
    let kind = item.item_type.to_string_lossy();
    let number = if item.nr != 0 {
        format!("{kind}{:3}", item.nr)
    } else {
        kind.into_owned()
    };
    format!(
        "{name}|{} col {}| {} {}",
        item.lnum,
        item.col,
        number,
        item.text.to_string_lossy()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExExecutor, Geometry};

    fn setup() -> (Editor, ExExecutor) {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"one\ntwo\nthree").unwrap(), true)
            .unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        (editor, ExExecutor::new())
    }

    fn item(buffer: BufHandle, lnum: i64, text: &str) -> Typval {
        Typval::dict(vec![
            pair("bufnr", Typval::Number(i64::from(buffer))),
            pair("lnum", Typval::Number(lnum)),
            pair("text", Typval::String(OxStr::from(text))),
        ])
    }

    #[test]
    fn winfixbuf_jump_redirects_to_previous_window_then_splits() {
        // qf_jump_edit_buffer (quickfix.c:2969-3006): a pinned window never
        // switches buffers for an entry; it prefers the free previous
        // window, else splits.
        let (mut editor, _) = setup();
        let target = editor.current_buffer().unwrap();
        let pinned_buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"pinned").unwrap(), true)
            .unwrap();
        let tab = editor.current_tabpage().unwrap();
        let window = editor.current_window().unwrap();
        let previous_window = editor
            .split_horizontal(tab, window, pinned_buffer, false)
            .unwrap();
        editor.set_current_window(previous_window).unwrap();
        editor
            .options_mut()
            .set_window(previous_window, "winfixbuf", OptionValue::Boolean(true))
            .unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![item(target, 1, "entry")])],
        )
        .unwrap();
        jump(&mut editor, QuickfixMove::Absolute(0), false).unwrap();
        // The previous (free) window now shows the entry buffer and carries
        // focus; the pinned window kept its buffer.
        assert_eq!(editor.current_window(), Some(window));
        assert_eq!(editor.window(window).unwrap().buffer, target);
        assert_eq!(
            editor.window(previous_window).unwrap().buffer,
            pinned_buffer
        );
    }

    #[test]
    fn winfixbuf_jump_splits_when_previous_window_is_pinned() {
        let (mut editor, _) = setup();
        // The current (first) window is pinned and shows `target`; the
        // second window is pinned too, so no previous window is free and
        // the entry (aimed at a different buffer) must open a split.
        let target = editor.current_buffer().unwrap();
        let entry_buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"entry").unwrap(), true)
            .unwrap();
        let tab = editor.current_tabpage().unwrap();
        let first = editor.current_window().unwrap();
        let second = editor.split_horizontal(tab, first, target, false).unwrap();
        editor
            .options_mut()
            .set_window(first, "winfixbuf", OptionValue::Boolean(true))
            .unwrap();
        editor
            .options_mut()
            .set_window(second, "winfixbuf", OptionValue::Boolean(true))
            .unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![item(entry_buffer, 1, "entry")])],
        )
        .unwrap();
        let before = editor.windows().len();
        jump(&mut editor, QuickfixMove::Absolute(0), false).unwrap();
        assert_eq!(editor.windows().len(), before + 1, "expected a split");
        assert_eq!(
            editor
                .window(editor.current_window().unwrap())
                .unwrap()
                .buffer,
            entry_buffer
        );
    }

    #[test]
    fn winfixbuf_jump_forceit_switches_in_place() {
        let (mut editor, _) = setup();
        let target = editor.current_buffer().unwrap();
        let pinned_buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"pinned").unwrap(), true)
            .unwrap();
        let window = editor.current_window().unwrap();
        editor
            .set_current_buffer(pinned_buffer, BufferRelease::KeepLoaded)
            .unwrap();
        editor
            .options_mut()
            .set_window(window, "winfixbuf", OptionValue::Boolean(true))
            .unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![item(target, 1, "entry")])],
        )
        .unwrap();
        jump(&mut editor, QuickfixMove::Absolute(0), true).unwrap();
        assert_eq!(editor.current_window(), Some(window));
        assert_eq!(editor.window(window).unwrap().buffer, target);
    }

    #[test]
    fn stack_replace_append_and_free_match_setqflist() {
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        call(
            &mut editor,
            "setqflist",
            &[
                Typval::list(vec![item(buffer, 1, "first")]),
                Typval::String(OxStr::from("r")),
            ],
        )
        .unwrap();
        call(
            &mut editor,
            "setqflist",
            &[
                Typval::list(vec![item(buffer, 2, "second")]),
                Typval::String(OxStr::from("a")),
            ],
        )
        .unwrap();
        assert_eq!(editor.quickfix().current().unwrap().items().len(), 2);
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(Vec::new()), Typval::String(OxStr::from("f"))],
        )
        .unwrap();
        assert_eq!(editor.quickfix().len(), 0);
    }

    #[test]
    fn getqflist_reports_items_number_and_stable_id() {
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![item(buffer, 2, "second")])],
        )
        .unwrap();
        let answer = call(
            &mut editor,
            "getqflist",
            &[Typval::dict(vec![
                pair("items", Typval::Number(0)),
                pair("nr", Typval::Number(0)),
                pair("id", Typval::Number(0)),
            ])],
        )
        .unwrap();
        let Typval::Dict(answer) = answer else {
            panic!("dictionary expected")
        };
        let answer = answer.borrow();
        assert_eq!(dict_number(&answer.entries, "nr"), Some(1));
        assert_eq!(dict_number(&answer.entries, "id"), Some(1));
        let Some(Typval::List(items)) = dict_value(&answer.entries, "items") else {
            panic!("items expected")
        };
        assert_eq!(items.borrow().items.len(), 1);
    }

    #[test]
    fn open_creates_named_quickfix_buffer_and_jump_selects_entry() {
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![item(buffer, 2, "second")])],
        )
        .unwrap();
        let window = open(&mut editor).unwrap();
        let qf_buffer = editor.window(window).unwrap().buffer;
        assert_eq!(
            editor.buffer(qf_buffer).unwrap().name().as_bytes(),
            b"quickfix"
        );
        assert!(
            matches!(editor.options().get_buffer(qf_buffer, "buftype"), Ok(OptionValue::String(value)) if value == "quickfix")
        );
        jump(&mut editor, QuickfixMove::Absolute(0), false).unwrap();
        assert_eq!(editor.current_buffer(), Some(buffer));
        assert_eq!(
            editor
                .window(editor.current_window().unwrap())
                .unwrap()
                .cursor
                .lnum,
            2
        );
    }
    #[test]
    fn empty_stack_navigation_reports_no_errors() {
        // Xtest_browse (test_quickfix.vim 536-558): E42 from :cnext/:cprev/:cc
        // on an empty stack, and after Xexpr ''.
        let (mut editor, _) = setup();
        for movement in [
            QuickfixMove::Next(1),
            QuickfixMove::Previous(1),
            QuickfixMove::Absolute(0),
            QuickfixMove::First,
            QuickfixMove::Last,
        ] {
            assert_eq!(
                jump(&mut editor, movement, false).unwrap_err(),
                QuickfixError {
                    code: "E42",
                    message: "No Errors".to_owned()
                }
            );
        }
        call(&mut editor, "setqflist", &[Typval::list(Vec::new())]).unwrap();
        assert_eq!(
            jump(&mut editor, QuickfixMove::Absolute(0), false)
                .unwrap_err()
                .code,
            "E42"
        );
    }

    #[test]
    fn first_previous_and_beyond_clamp_like_browse_test() {
        // Xtest_browse (test_quickfix.vim 570-617): 6-entry list; :cprev from
        // the first entry fails E553; :cnext past the end fails E553; count
        // overruns clamp (10Xcc -> idx 6, 10Xnext -> last, 10Xprev -> first).
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        let entries: Vec<Typval> = [(1, 5i64), (1, 6), (1, 10), (1, 11)]
            .into_iter()
            .map(|(_, line)| item(buffer, line, "line"))
            .collect();
        call(&mut editor, "setqflist", &[Typval::list(entries)]).unwrap();
        assert_eq!(
            jump(&mut editor, QuickfixMove::Previous(1), false)
                .unwrap_err()
                .code,
            "E553"
        );
        call(
            &mut editor,
            "getqflist",
            &[Typval::dict(vec![pair("idx", Typval::Number(1))])],
        )
        .unwrap();
        assert_eq!(editor.quickfix().current().unwrap().idx(), 1);
        jump(&mut editor, QuickfixMove::Absolute(9), false).unwrap();
        assert_eq!(editor.quickfix().current().unwrap().idx(), 4);
        jump(&mut editor, QuickfixMove::First, false).unwrap();
        assert_eq!(editor.quickfix().current().unwrap().idx(), 1);
        jump(&mut editor, QuickfixMove::Next(10), false).unwrap();
        assert_eq!(editor.quickfix().current().unwrap().idx(), 4);
        jump(&mut editor, QuickfixMove::Previous(10), false).unwrap();
        assert_eq!(editor.quickfix().current().unwrap().idx(), 1);
    }

    #[test]
    fn invalid_action_reports_e927() {
        let (mut editor, _) = setup();
        let error = call(
            &mut editor,
            "setqflist",
            &[Typval::list(Vec::new()), Typval::String(OxStr::from("q"))],
        )
        .unwrap_err();
        assert_eq!(error.code, "E927");
    }

    #[test]
    fn replace_resets_changedtick_and_append_bumps_it() {
        // Xqftick_tests (test_quickfix.vim 4201-4234).
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        let first = Typval::list(vec![item(buffer, 10, "L7")]);
        call(
            &mut editor,
            "setqflist",
            &[first.clone(), Typval::String(OxStr::from(" "))],
        )
        .unwrap();
        assert_eq!(editor.quickfix().current().unwrap().changedtick(), 1);
        call(
            &mut editor,
            "setqflist",
            &[
                Typval::list(vec![item(buffer, 11, "L11")]),
                Typval::String(OxStr::from("a")),
            ],
        )
        .unwrap();
        assert_eq!(editor.quickfix().current().unwrap().changedtick(), 2);
        call(
            &mut editor,
            "setqflist",
            &[first, Typval::String(OxStr::from(" "))],
        )
        .unwrap();
        assert_eq!(editor.quickfix().current().unwrap().changedtick(), 1);
    }

    #[test]
    fn select_by_id_and_nr_match_qf_getprop_qfidx() {
        // f_getqflist what-dict id/nr selection (quickfix.c 6859-6903).
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![item(buffer, 1, "one")])],
        )
        .unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![item(buffer, 2, "two")])],
        )
        .unwrap();
        assert_eq!(editor.quickfix().len(), 2);
        let answer = call(
            &mut editor,
            "getqflist",
            &[Typval::dict(vec![
                pair("id", Typval::Number(1)),
                pair("size", Typval::Number(0)),
            ])],
        )
        .unwrap();
        let Typval::Dict(answer) = answer else {
            panic!("dictionary expected")
        };
        assert_eq!(dict_number(&answer.borrow().entries, "size"), Some(1));
        let answer = call(
            &mut editor,
            "getqflist",
            &[Typval::dict(vec![
                pair("nr", Typval::Number(2)),
                pair("size", Typval::Number(0)),
            ])],
        )
        .unwrap();
        let Typval::Dict(answer) = answer else {
            panic!("dictionary expected")
        };
        assert_eq!(dict_number(&answer.borrow().entries, "size"), Some(1));
    }
    #[test]
    fn first_and_last_skip_invalid_entries() {
        // Review finding: `First`/`Last` took the literal ends even when
        // invalid, contradicting their "first/last valid entry" contract.
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        let invalid_first = Typval::dict(vec![
            pair("bufnr", Typval::Number(i64::from(buffer))),
            pair("lnum", Typval::Number(1)),
            pair("valid", Typval::Number(0)),
        ]);
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![
                invalid_first,
                item(buffer, 2, "two"),
                item(buffer, 3, "three"),
            ])],
        )
        .unwrap();
        jump(&mut editor, QuickfixMove::Last, false).unwrap();
        assert_eq!(editor.quickfix().current().unwrap().idx(), 3);
        jump(&mut editor, QuickfixMove::First, false).unwrap();
        assert_eq!(editor.quickfix().current().unwrap().idx(), 2);
        let invalid_last = Typval::dict(vec![
            pair("bufnr", Typval::Number(i64::from(buffer))),
            pair("lnum", Typval::Number(3)),
            pair("valid", Typval::Number(0)),
        ]);
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![
                item(buffer, 1, "one"),
                item(buffer, 2, "two"),
                invalid_last,
            ])],
        )
        .unwrap();
        jump(&mut editor, QuickfixMove::First, false).unwrap();
        assert_eq!(editor.quickfix().current().unwrap().idx(), 1);
        jump(&mut editor, QuickfixMove::Last, false).unwrap();
        assert_eq!(editor.quickfix().current().unwrap().idx(), 2);
    }

    #[test]
    fn winfixbuf_split_failure_restores_the_entry_index() {
        // The E1513 path restores the index like the E42 path: exhaust
        // splits with pinned windows so the fallback cannot open one.
        let (mut editor, _) = setup();
        let target = editor.current_buffer().unwrap();
        let entry_buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"entry").unwrap(), true)
            .unwrap();
        let tab = editor.current_tabpage().unwrap();
        for _ in 0..100 {
            let current = editor.current_window().unwrap();
            if editor.split_horizontal(tab, current, target, true).is_err() {
                break;
            }
            let created = editor.current_window().unwrap();
            editor
                .options_mut()
                .set_window(created, "winfixbuf", OptionValue::Boolean(true))
                .unwrap();
        }
        let current = editor.current_window().unwrap();
        editor
            .options_mut()
            .set_window(current, "winfixbuf", OptionValue::Boolean(true))
            .unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![
                item(target, 1, "here"),
                item(entry_buffer, 1, "entry"),
            ])],
        )
        .unwrap();
        jump(&mut editor, QuickfixMove::Absolute(0), false).unwrap();
        assert_eq!(editor.quickfix().current().unwrap().idx(), 1);
        let error = jump(&mut editor, QuickfixMove::Next(1), false).unwrap_err();
        assert_eq!(error.code, "E1513");
        assert_eq!(editor.quickfix().current().unwrap().idx(), 1);
    }

    #[test]
    fn qf_window_redirect_skips_pinned_windows() {
        // Review finding: jumping from the quickfix window could route the
        // entry into a 'winfixbuf' window with no error. Upstream skips
        // pinned windows when choosing a target (quickfix.c:2868-2885).
        let (mut editor, _) = setup();
        let target = editor.current_buffer().unwrap();
        let pinned_buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"pinned").unwrap(), true)
            .unwrap();
        let entry_buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"e1\ne2\ne3").unwrap(), true)
            .unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![item(entry_buffer, 2, "entry")])],
        )
        .unwrap();
        let qf_window = open(&mut editor).unwrap();
        let tab = editor.current_tabpage().unwrap();
        let first = editor.current_window().unwrap();
        assert_eq!(first, qf_window);
        let spare = editor
            .split_horizontal(tab, qf_window, pinned_buffer, true)
            .unwrap();
        editor.set_current_window(qf_window).unwrap();
        // Pin the earliest window: it is the first redirect candidate, so
        // a pin-blind search would steal it.
        let earliest = editor.windows().into_iter().min().unwrap();
        editor
            .options_mut()
            .set_window(earliest, "winfixbuf", OptionValue::Boolean(true))
            .unwrap();
        jump(&mut editor, QuickfixMove::Absolute(0), false).unwrap();
        assert_eq!(editor.window(earliest).unwrap().buffer, target);
        assert_eq!(editor.window(spare).unwrap().buffer, entry_buffer);
        assert_eq!(editor.current_window(), Some(spare));
    }

    #[test]
    fn open_restores_the_quickfix_buffer_in_its_window() {
        // Review finding: a wiped buffer (or a manual `:buffer` inside the
        // quickfix window) left `open` focusing a window showing other
        // content instead of the quickfix buffer.
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![item(buffer, 2, "second")])],
        )
        .unwrap();
        let window = open(&mut editor).unwrap();
        let qf_buffer = editor.window(window).unwrap().buffer;
        editor
            .set_current_buffer(buffer, BufferRelease::KeepLoaded)
            .unwrap();
        assert_eq!(editor.window(window).unwrap().buffer, buffer);
        assert_eq!(open(&mut editor).unwrap(), window);
        assert_eq!(editor.window(window).unwrap().buffer, qf_buffer);
    }

    #[test]
    fn getqflist_all_flag_reports_metadata_keys() {
        // Review finding: `getqflist({'all': 1})` omitted `nr`, `winid`,
        // and `qfbufnr`, and `{'all': 0}` wrongly enabled everything.
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![item(buffer, 2, "second")])],
        )
        .unwrap();
        open(&mut editor).unwrap();
        for (all, present) in [
            (Typval::Number(1), true),
            (Typval::Bool(true), true),
            (Typval::Number(0), false),
            (Typval::Bool(false), false),
        ] {
            let answer = call(
                &mut editor,
                "getqflist",
                &[Typval::dict(vec![pair("all", all)])],
            )
            .unwrap();
            let Typval::Dict(answer) = answer else {
                panic!("dictionary expected")
            };
            let answer = answer.borrow();
            assert_eq!(
                dict_value(&answer.entries, "nr").is_some(),
                present,
                "nr presence"
            );
            assert_eq!(
                dict_value(&answer.entries, "winid").is_some(),
                present,
                "winid presence"
            );
            assert_eq!(
                dict_value(&answer.entries, "qfbufnr").is_some(),
                present,
                "qfbufnr presence"
            );
            assert_eq!(
                dict_value(&answer.entries, "title").is_some(),
                present,
                "title presence"
            );
            if present {
                assert_eq!(dict_number(&answer.entries, "nr"), Some(1));
                assert!(dict_number(&answer.entries, "winid").is_some_and(|id| id != 0));
                assert!(dict_number(&answer.entries, "qfbufnr").is_some_and(|id| id != 0));
            }
        }
    }

    #[test]
    fn setqflist_replace_with_empty_list_clears() {
        // Review finding, probed against upstream: `setqflist([], 'r')`
        // clears the list (length 0); the port left the old entries.
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![
                item(buffer, 1, "one"),
                item(buffer, 2, "two"),
            ])],
        )
        .unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(Vec::new()), Typval::String(OxStr::from("r"))],
        )
        .unwrap();
        let answer = call(&mut editor, "getqflist", &[]).unwrap();
        let Typval::List(items) = answer else {
            panic!("list expected")
        };
        assert!(items.borrow().items.is_empty());
    }

    #[test]
    fn pattern_only_entry_searches_the_target_buffer() {
        // Review finding: valid pattern-only entries jumped to line 1
        // instead of their matching line (upstream `qf_jump` searches).
        let (mut editor, _) = setup();
        let buffer = editor.current_buffer().unwrap();
        call(
            &mut editor,
            "setqflist",
            &[Typval::list(vec![Typval::dict(vec![
                pair("bufnr", Typval::Number(i64::from(buffer))),
                pair("pattern", Typval::String(OxStr::from("two"))),
                pair("text", Typval::String(OxStr::from("two"))),
            ])])],
        )
        .unwrap();
        jump(&mut editor, QuickfixMove::Absolute(0), false).unwrap();
        let cursor = editor
            .window(editor.current_window().unwrap())
            .unwrap()
            .cursor;
        assert_eq!(cursor.lnum, 2);
    }
}
