//! Autocommand registration and firing plans.
//!
//! Event names mirror `src/nvim/auevents.lua`. Pattern splitting and matching
//! follow `src/nvim/autocmd.c:887-957`, `src/nvim/autocmd.c:1865-1890`, and
//! `src/nvim/fileio.c:3694-3869`. Execution belongs to the host.

use std::collections::{BTreeMap, BTreeSet};

use ox_types::BufHandle;
use thiserror::Error;

/// How an event's pattern is interpreted by the editor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternKind {
    /// File-name glob, matched against a full path when it contains `/`, otherwise the tail.
    File,
    /// Buffer event; normal globs match the buffer name and `<abuf>` selects one handle.
    Buffer,
    /// Event-defined match text rather than a file or buffer pattern.
    None,
}

macro_rules! define_events {
    ($($event:ident => ($name:literal, $kind:expr),)+) => {
        /// Autocommand events from Neovim's generated event table.
        #[allow(missing_docs)]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub enum Event { $($event,)+ }

        impl Event {
            /// Every canonical event in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$event,)+];

            /// Canonical event spelling.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$event => $name,)+ }
            }

            /// Pattern interpretation used by this event.
            #[must_use]
            pub const fn pattern_kind(self) -> PatternKind {
                match self { $(Self::$event => $kind,)+ }
            }

            /// Resolves canonical names and the four aliases from `auevents.lua`.
            #[must_use]
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$event),)+
                    "BufCreate" => Some(Self::BufAdd),
                    "BufRead" => Some(Self::BufReadPost),
                    "BufWrite" => Some(Self::BufWritePre),
                    "FileEncoding" => Some(Self::EncodingChanged),
                    _ => None,
                }
            }
        }
    };
}

define_events! {
            BufAdd => ("BufAdd", PatternKind::File),
            BufDelete => ("BufDelete", PatternKind::File),
            BufEnter => ("BufEnter", PatternKind::Buffer),
            BufFilePost => ("BufFilePost", PatternKind::File),
            BufFilePre => ("BufFilePre", PatternKind::File),
            BufHidden => ("BufHidden", PatternKind::File),
            BufLeave => ("BufLeave", PatternKind::Buffer),
            BufNew => ("BufNew", PatternKind::File),
            BufNewFile => ("BufNewFile", PatternKind::File),
            BufReadCmd => ("BufReadCmd", PatternKind::File),
            BufReadPost => ("BufReadPost", PatternKind::File),
            BufReadPre => ("BufReadPre", PatternKind::File),
            BufUnload => ("BufUnload", PatternKind::File),
            BufWinEnter => ("BufWinEnter", PatternKind::Buffer),
            BufWinLeave => ("BufWinLeave", PatternKind::Buffer),
            BufWipeout => ("BufWipeout", PatternKind::File),
            BufWriteCmd => ("BufWriteCmd", PatternKind::File),
            BufWritePost => ("BufWritePost", PatternKind::File),
            BufWritePre => ("BufWritePre", PatternKind::File),
            ChanClose => ("ChanClose", PatternKind::None),
            ChanInfo => ("ChanInfo", PatternKind::None),
            ChanOpen => ("ChanOpen", PatternKind::None),
            CmdAtom => ("CmdAtom", PatternKind::None),
            CmdUndefined => ("CmdUndefined", PatternKind::None),
            CmdlineChanged => ("CmdlineChanged", PatternKind::None),
            CmdlineEnter => ("CmdlineEnter", PatternKind::None),
            CmdlineLeave => ("CmdlineLeave", PatternKind::None),
            CmdlineLeavePre => ("CmdlineLeavePre", PatternKind::None),
            CmdwinEnter => ("CmdwinEnter", PatternKind::None),
            CmdwinLeave => ("CmdwinLeave", PatternKind::None),
            ColorScheme => ("ColorScheme", PatternKind::None),
            ColorSchemePre => ("ColorSchemePre", PatternKind::None),
            CompleteChanged => ("CompleteChanged", PatternKind::None),
            CompleteDone => ("CompleteDone", PatternKind::None),
            CompleteDonePre => ("CompleteDonePre", PatternKind::None),
            CursorHold => ("CursorHold", PatternKind::Buffer),
            CursorHoldI => ("CursorHoldI", PatternKind::Buffer),
            CursorMoved => ("CursorMoved", PatternKind::Buffer),
            CursorMovedC => ("CursorMovedC", PatternKind::Buffer),
            CursorMovedI => ("CursorMovedI", PatternKind::Buffer),
            DiagnosticChanged => ("DiagnosticChanged", PatternKind::Buffer),
            DiffUpdated => ("DiffUpdated", PatternKind::None),
            DirChanged => ("DirChanged", PatternKind::None),
            DirChangedPre => ("DirChangedPre", PatternKind::None),
            EncodingChanged => ("EncodingChanged", PatternKind::None),
            ExitPre => ("ExitPre", PatternKind::None),
            FileAppendCmd => ("FileAppendCmd", PatternKind::File),
            FileAppendPost => ("FileAppendPost", PatternKind::File),
            FileAppendPre => ("FileAppendPre", PatternKind::File),
            FileChangedRO => ("FileChangedRO", PatternKind::File),
            FileChangedShell => ("FileChangedShell", PatternKind::File),
            FileChangedShellPost => ("FileChangedShellPost", PatternKind::File),
            FileReadCmd => ("FileReadCmd", PatternKind::File),
            FileReadPost => ("FileReadPost", PatternKind::File),
            FileReadPre => ("FileReadPre", PatternKind::File),
            FileType => ("FileType", PatternKind::File),
            FileWriteCmd => ("FileWriteCmd", PatternKind::File),
            FileWritePost => ("FileWritePost", PatternKind::File),
            FileWritePre => ("FileWritePre", PatternKind::File),
            FilterReadPost => ("FilterReadPost", PatternKind::File),
            FilterReadPre => ("FilterReadPre", PatternKind::File),
            FilterWritePost => ("FilterWritePost", PatternKind::File),
            FilterWritePre => ("FilterWritePre", PatternKind::File),
            FocusGained => ("FocusGained", PatternKind::None),
            FocusLost => ("FocusLost", PatternKind::None),
            FuncUndefined => ("FuncUndefined", PatternKind::None),
            GUIEnter => ("GUIEnter", PatternKind::None),
            GUIFailed => ("GUIFailed", PatternKind::None),
            InsertChange => ("InsertChange", PatternKind::Buffer),
            InsertCharPre => ("InsertCharPre", PatternKind::Buffer),
            InsertEnter => ("InsertEnter", PatternKind::Buffer),
            InsertLeave => ("InsertLeave", PatternKind::Buffer),
            InsertLeavePre => ("InsertLeavePre", PatternKind::Buffer),
            LspAttach => ("LspAttach", PatternKind::Buffer),
            LspDetach => ("LspDetach", PatternKind::Buffer),
            LspNotify => ("LspNotify", PatternKind::None),
            LspProgress => ("LspProgress", PatternKind::None),
            LspRequest => ("LspRequest", PatternKind::None),
            LspTokenUpdate => ("LspTokenUpdate", PatternKind::Buffer),
            MarkSet => ("MarkSet", PatternKind::None),
            MenuPopup => ("MenuPopup", PatternKind::None),
            ModeChanged => ("ModeChanged", PatternKind::None),
            OptionSet => ("OptionSet", PatternKind::None),
            QuickFixCmdPost => ("QuickFixCmdPost", PatternKind::None),
            QuickFixCmdPre => ("QuickFixCmdPre", PatternKind::None),
            QuitPre => ("QuitPre", PatternKind::None),
            PackChangedPre => ("PackChangedPre", PatternKind::None),
            PackChanged => ("PackChanged", PatternKind::None),
            Progress => ("Progress", PatternKind::None),
            RecordingEnter => ("RecordingEnter", PatternKind::Buffer),
            RecordingLeave => ("RecordingLeave", PatternKind::Buffer),
            RemoteReply => ("RemoteReply", PatternKind::None),
            SafeState => ("SafeState", PatternKind::None),
            SearchWrapped => ("SearchWrapped", PatternKind::Buffer),
            SessionLoadPost => ("SessionLoadPost", PatternKind::None),
            SessionLoadPre => ("SessionLoadPre", PatternKind::None),
            SessionWritePre => ("SessionWritePre", PatternKind::None),
            SessionWritePost => ("SessionWritePost", PatternKind::None),
            ShellCmdPost => ("ShellCmdPost", PatternKind::None),
            ShellFilterPost => ("ShellFilterPost", PatternKind::Buffer),
            Signal => ("Signal", PatternKind::None),
            SourceCmd => ("SourceCmd", PatternKind::None),
            SourcePost => ("SourcePost", PatternKind::None),
            SourcePre => ("SourcePre", PatternKind::None),
            SpellFileMissing => ("SpellFileMissing", PatternKind::None),
            StdinReadPost => ("StdinReadPost", PatternKind::None),
            StdinReadPre => ("StdinReadPre", PatternKind::None),
            SwapExists => ("SwapExists", PatternKind::None),
            Syntax => ("Syntax", PatternKind::None),
            TabClosed => ("TabClosed", PatternKind::None),
            TabClosedPre => ("TabClosedPre", PatternKind::None),
            TabEnter => ("TabEnter", PatternKind::None),
            TabLeave => ("TabLeave", PatternKind::None),
            TabMoved => ("TabMoved", PatternKind::None),
            TabNew => ("TabNew", PatternKind::None),
            TabNewEntered => ("TabNewEntered", PatternKind::None),
            TermChanged => ("TermChanged", PatternKind::None),
            TermClose => ("TermClose", PatternKind::None),
            TermEnter => ("TermEnter", PatternKind::None),
            TermLeave => ("TermLeave", PatternKind::None),
            TermOpen => ("TermOpen", PatternKind::None),
            TermRequest => ("TermRequest", PatternKind::None),
            TermResponse => ("TermResponse", PatternKind::None),
            TextChanged => ("TextChanged", PatternKind::Buffer),
            TextChangedI => ("TextChangedI", PatternKind::Buffer),
            TextChangedP => ("TextChangedP", PatternKind::Buffer),
            TextChangedT => ("TextChangedT", PatternKind::Buffer),
            TextPutPost => ("TextPutPost", PatternKind::Buffer),
            TextPutPre => ("TextPutPre", PatternKind::Buffer),
            TextYankPost => ("TextYankPost", PatternKind::Buffer),
            UIEnter => ("UIEnter", PatternKind::None),
            UILeave => ("UILeave", PatternKind::None),
            User => ("User", PatternKind::None),
            VimEnter => ("VimEnter", PatternKind::None),
            VimLeave => ("VimLeave", PatternKind::None),
            VimLeavePre => ("VimLeavePre", PatternKind::None),
            VimResized => ("VimResized", PatternKind::None),
            VimResume => ("VimResume", PatternKind::None),
            VimSuspend => ("VimSuspend", PatternKind::None),
            WinClosed => ("WinClosed", PatternKind::Buffer),
            WinEnter => ("WinEnter", PatternKind::Buffer),
            WinLeave => ("WinLeave", PatternKind::Buffer),
            WinNewPre => ("WinNewPre", PatternKind::None),
            WinNew => ("WinNew", PatternKind::None),
            WinResized => ("WinResized", PatternKind::Buffer),
            WinScrolled => ("WinScrolled", PatternKind::Buffer),
}

/// Number of canonical autocmd events.
pub const EVENT_COUNT: usize = Event::ALL.len();

/// Stable augroup identity. Zero is the default, ungrouped namespace.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AugroupId(pub u64);

/// Host-owned action referenced by an autocommand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AutocmdKind {
    /// Ex source to execute later.
    ExString(String),
    /// Lua registry callback identity to invoke later.
    LuaCallback(u64),
}

/// One action produced by the firing planner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutocmdAction {
    /// Stable definition identity.
    pub id: u64,
    /// Event which produced the action.
    pub event: Event,
    /// Host-owned executable payload.
    pub kind: AutocmdKind,
    /// Whether the definition is one-shot and removed when the host
    /// acknowledges execution via `Autocmds::consume_once`.
    pub once: bool,
    /// Whether actions raised while this action runs may execute immediately.
    pub nested: bool,
    /// Definition augroup.
    pub group: AugroupId,
    /// Augroup name, absent for the default group.
    pub group_name: Option<String>,
    /// Original source pattern.
    pub pattern: String,
    /// Selected buffer for a buffer-local pattern.
    pub buffer: Option<BufHandle>,
    /// Optional user-facing description.
    pub description: Option<String>,
}

/// Execution seam implemented by Vimscript/Lua hosting layers.
pub trait AutocmdSink {
    /// Host execution failure.
    type Error;
    /// Executes one already-planned action.
    fn run(&mut self, action: &AutocmdAction) -> Result<(), Self::Error>;
}

/// Registration options shared by Ex and API-created autocmds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AutocmdOptions {
    /// Destination augroup, or the default group.
    pub group: AugroupId,
    /// Buffer substituted for a `<abuf>` pattern.
    pub buffer: Option<BufHandle>,
    /// Remove after the first firing plan.
    pub once: bool,
    /// Permit nested autocmd execution.
    pub nested: bool,
    /// Optional user-facing description.
    pub description: Option<String>,
}

/// Event occurrence supplied to the firing planner.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AutocmdContext<'a> {
    /// Buffer associated with the event.
    pub buffer: Option<BufHandle>,
    /// Event match name, normally a buffer or file name.
    pub file_name: Option<&'a str>,
    /// True when this event may fire nested, false when it is raised inside a
    /// non-`++nested` outer autocmd and must be suppressed entirely.
    ///
    /// The host passes the *outer* autocmd's `++nested` flag, and `true` for a
    /// top-level event. Gating is decided once per event, never per candidate.
    pub nested: bool,
}

/// Ordered actions for one event occurrence.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FiringPlan {
    /// Actions which may run immediately, in global definition order.
    pub ready: Vec<AutocmdAction>,
}

/// Selector corresponding to the useful `:autocmd!` forms.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeleteAutocmds<'a> {
    /// Optional exact augroup.
    pub group: Option<AugroupId>,
    /// Optional exact event.
    pub event: Option<Event>,
    /// Optional comma-list of exact source patterns.
    pub pattern: Option<&'a str>,
}

/// Invalid registration or augroup operation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AutocmdError {
    /// An empty augroup name was requested.
    #[error("augroup name must not be empty")]
    EmptyGroupName,
    /// An empty pattern was requested.
    #[error("autocmd pattern must not be empty")]
    EmptyPattern,
    /// `<abuf>` was used without a registration buffer.
    #[error("<abuf> requires a buffer handle")]
    MissingBuffer,
    /// The selected augroup does not exist.
    #[error("unknown augroup {0:?}")]
    UnknownGroup(AugroupId),
    /// Pattern alternation braces were malformed.
    #[error("unbalanced braces in autocmd pattern")]
    UnbalancedBraces,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StoredPattern {
    Glob(Vec<String>),
    Buffer(BufHandle),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    id: u64,
    sequence: u64,
    event: Event,
    pattern: StoredPattern,
    source_pattern: String,
    kind: AutocmdKind,
    options: AutocmdOptions,
}

/// Editor-owned augroups and autocmd definitions.
#[derive(Clone, Debug)]
pub struct Autocmds {
    groups: BTreeMap<AugroupId, (String, u64)>,
    group_names: BTreeMap<String, AugroupId>,
    entries: Vec<Entry>,
    ignored: BTreeSet<Event>,
    next_group: u64,
    next_id: u64,
    next_sequence: u64,
}

impl Default for Autocmds {
    fn default() -> Self { Self::new() }
}

impl Autocmds {
    /// Creates an empty autocmd and augroup store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            groups: BTreeMap::new(),
            group_names: BTreeMap::new(),
            entries: Vec::new(),
            ignored: BTreeSet::new(),
            next_group: 1,
            next_id: 1,
            next_sequence: 1,
        }
    }

    /// Creates a group or returns its existing identity. `clear` implements `augroup!`.
    pub fn create_group(&mut self, name: &str, clear: bool) -> Result<AugroupId, AutocmdError> {
        if name.is_empty() { return Err(AutocmdError::EmptyGroupName); }
        if let Some(id) = self.group_names.get(name).copied() {
            if clear { self.clear_group(id)?; }
            return Ok(id);
        }
        let id = AugroupId(self.next_group);
        self.next_group = self.next_group.saturating_add(1);
        let order = id.0;
        self.groups.insert(id, (name.to_owned(), order));
        self.group_names.insert(name.to_owned(), id);
        Ok(id)
    }

    /// Looks up an augroup by name.
    pub fn group(&self, name: &str) -> Option<AugroupId> { self.group_names.get(name).copied() }

    /// Deletes an augroup and all of its definitions.
    pub fn delete_group(&mut self, id: AugroupId) -> Result<(), AutocmdError> {
        let Some((name, _)) = self.groups.remove(&id) else { return Err(AutocmdError::UnknownGroup(id)); };
        self.group_names.remove(&name);
        self.entries.retain(|entry| entry.options.group != id);
        Ok(())
    }

    /// Clears every definition in an augroup while preserving the group.
    pub fn clear_group(&mut self, id: AugroupId) -> Result<usize, AutocmdError> {
        if id != AugroupId::default() && !self.groups.contains_key(&id) {
            return Err(AutocmdError::UnknownGroup(id));
        }
        let before = self.entries.len();
        self.entries.retain(|entry| entry.options.group != id);
        Ok(before - self.entries.len())
    }

    /// Registers one definition for every top-level comma-separated pattern.
    pub fn register(
        &mut self,
        event: Event,
        patterns: &str,
        kind: AutocmdKind,
        options: AutocmdOptions,
    ) -> Result<Vec<u64>, AutocmdError> {
        if patterns.is_empty() { return Err(AutocmdError::EmptyPattern); }
        if options.group != AugroupId::default() && !self.groups.contains_key(&options.group) {
            return Err(AutocmdError::UnknownGroup(options.group));
        }
        let parts = split_pattern_list(patterns)?;
        let mut ids = Vec::with_capacity(parts.len());
        for part in parts {
            if part.is_empty() { return Err(AutocmdError::EmptyPattern); }
            let stored = if part == "<abuf>" {
                StoredPattern::Buffer(options.buffer.ok_or(AutocmdError::MissingBuffer)?)
            } else {
                StoredPattern::Glob(expand_braces(&part).ok_or(AutocmdError::UnbalancedBraces)?)
            };
            let id = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
            let sequence = self.next_sequence;
            self.next_sequence = self.next_sequence.saturating_add(1);
            self.entries.push(Entry {
                id,
                sequence,
                event,
                pattern: stored,
                source_pattern: part,
                kind: kind.clone(),
                options: options.clone(),
            });
            ids.push(id);
        }
        Ok(ids)
    }

    /// Removes definitions selected by group, event, and exact source pattern.
    pub fn delete(&mut self, selector: DeleteAutocmds<'_>) -> Result<usize, AutocmdError> {
        let patterns = selector.pattern.map(split_pattern_list).transpose()?;
        let before = self.entries.len();
        self.entries.retain(|entry| {
            let group_matches = selector.group.is_none_or(|group| entry.options.group == group);
            let event_matches = selector.event.is_none_or(|event| entry.event == event);
            let pattern_matches = patterns.as_ref().is_none_or(|items| items.contains(&entry.source_pattern));
            (group_matches && event_matches && pattern_matches) == false
        });
        Ok(before - self.entries.len())
    }

    /// Adds an event to the editor's `eventignore` set.
    pub fn ignore(&mut self, event: Event) { self.ignored.insert(event); }
    /// Removes an event from the editor's `eventignore` set.
    pub fn unignore(&mut self, event: Event) { self.ignored.remove(&event); }
    /// Whether the event is currently ignored.
    #[must_use]
    pub fn is_ignored(&self, event: Event) -> bool { self.ignored.contains(&event) }
    /// Number of registered pattern definitions.
    #[must_use]
    pub fn len(&self) -> usize { self.entries.len() }
    /// Whether no definitions are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// Removes all buffer-local definitions for a wiped buffer.
    pub fn remove_buffer(&mut self, buffer: BufHandle) {
        self.entries.retain(|entry| !matches!(entry.pattern, StoredPattern::Buffer(value) if value == buffer));
    }

    /// Builds the firing plan for one event occurrence.
    ///
    /// Matching actions are returned in global definition order; augroups
    /// filter definitions but never reorder them (autocmd.c:80-83). When the
    /// event is raised inside a non-`++nested` outer autocmd the whole event
    /// is suppressed rather than split by candidate flags (autocmd.c:1465-1468,
    /// 2000-2002). One-shot definitions are *not* consumed here; the host
    /// acknowledges execution with `consume_once`, so abandoned plans leave
    /// `++once` definitions intact.
    pub fn plan(&mut self, event: Event, context: AutocmdContext<'_>) -> FiringPlan {
        if self.ignored.contains(&event) || !context.nested {
            return FiringPlan::default();
        }
        let mut matched: Vec<&Entry> = self.entries.iter().filter(|entry| {
            entry.event == event && pattern_matches(&entry.pattern, context.buffer, context.file_name)
        }).collect();
        matched.sort_by_key(|entry| entry.sequence);
        let ready: Vec<AutocmdAction> = matched.into_iter().map(|entry| self.action(entry)).collect();
        FiringPlan { ready }
    }

    /// Removes the one-shot definition identified by `id` once the host begins
    /// executing it. Returns `true` when a `++once` definition was removed.
    pub fn consume_once(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| !(entry.id == id && entry.options.once));
        self.entries.len() != before
    }

    fn action(&self, entry: &Entry) -> AutocmdAction {
        AutocmdAction {
            id: entry.id,
            event: entry.event,
            kind: entry.kind.clone(),
            once: entry.options.once,
            nested: entry.options.nested,
            group: entry.options.group,
            group_name: self.groups.get(&entry.options.group).map(|(name, _)| name.clone()),
            pattern: entry.source_pattern.clone(),
            buffer: match entry.pattern { StoredPattern::Buffer(buffer) => Some(buffer), StoredPattern::Glob(_) => None },
            description: entry.options.description.clone(),
        }
    }
}

fn pattern_matches(pattern: &StoredPattern, buffer: Option<BufHandle>, file_name: Option<&str>) -> bool {
    match pattern {
        StoredPattern::Buffer(expected) => buffer == Some(*expected),
        StoredPattern::Glob(patterns) => {
            let name = file_name.unwrap_or_default();
            patterns.iter().any(|pattern| {
                let candidate = if pattern.contains('/') {
                    name
                } else {
                    name.rsplit('/').next().unwrap_or(name)
                };
                glob_match(pattern, candidate)
            })
        }
    }
}

fn split_pattern_list(patterns: &str) -> Result<Vec<String>, AutocmdError> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut escaped = false;
    for ch in patterns.chars() {
        if escaped { current.push(ch); escaped = false; continue; }
        if ch == '\\' { escaped = true; current.push(ch); continue; }
        match ch {
            '{' => { depth += 1; current.push(ch); }
            '}' if depth == 0 => return Err(AutocmdError::UnbalancedBraces),
            '}' => { depth -= 1; current.push(ch); }
            ',' if depth == 0 => { result.push(current); current = String::new(); }
            _ => current.push(ch),
        }
    }
    if escaped { current.push('\\'); }
    if depth != 0 { return Err(AutocmdError::UnbalancedBraces); }
    result.push(current);
    Ok(result)
}

fn expand_braces(pattern: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = pattern.chars().collect();
    let start = chars.iter().position(|ch| *ch == '{');
    let Some(start) = start else { return Some(vec![pattern.to_owned()]); };
    let mut depth = 0usize;
    let mut end = None;
    for (index, ch) in chars.iter().enumerate().skip(start) {
        if *ch == '{' { depth += 1; }
        if *ch == '}' { depth = depth.checked_sub(1)?; if depth == 0 { end = Some(index); break; } }
    }
    let end = end?;
    let prefix: String = chars[..start].iter().collect();
    let suffix: String = chars[end + 1..].iter().collect();
    let middle: String = chars[start + 1..end].iter().collect();
    let alternatives = split_pattern_list(&middle).ok()?;
    let mut result = Vec::new();
    for alternative in alternatives {
        for expanded in expand_braces(&format!("{prefix}{alternative}{suffix}"))? { result.push(expanded); }
    }
    Some(result)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut row = vec![false; text.len() + 1];
    row[0] = true;
    let mut index = 0usize;
    while index < pattern.len() {
        let mut next = vec![false; text.len() + 1];
        match pattern[index] {
            '*' => {
                next[0] = row[0];
                for column in 1..=text.len() { next[column] = row[column] || next[column - 1]; }
            }
            '?' => {
                for column in 1..=text.len() { next[column] = row[column - 1]; }
            }
            '\\' if index + 1 < pattern.len() => {
                index += 1;
                for column in 1..=text.len() { next[column] = row[column - 1] && pattern[index] == text[column - 1]; }
            }
            literal => {
                for column in 1..=text.len() { next[column] = row[column - 1] && literal == text[column - 1]; }
            }
        }
        row = next;
        index += 1;
    }
    row[text.len()]
}
