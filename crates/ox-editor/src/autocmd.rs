//! Autocommand registration and firing plans.
//!
//! Event names mirror `src/nvim/auevents.lua`. Pattern splitting and matching
//! follow `src/nvim/autocmd.c:887-957`, `src/nvim/autocmd.c:1865-1890`, and
//! `src/nvim/fileio.c:3694-3869`. Execution belongs to the host.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use ox_types::{BufHandle, Dict, Object, OxStr};
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

/// Monotonic identity for one stored autocmd entry, independent of the
/// optional shared API id. Used by `consume_once` and callback-truthy
/// deletion to remove exactly the executing entry.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AutocmdEntryId(pub u64);

/// Host-owned action referenced by an autocommand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AutocmdKind {
    /// Ex source to execute later.
    ExString(String),
    /// Named zero-argument Vimscript function to invoke later.
    VimscriptFunction(String),
    /// Lua registry callback identity to invoke later.
    LuaCallback(u64),
}

/// One action produced by the firing planner.
#[derive(Clone, Debug, PartialEq)]
pub struct AutocmdAction {
    /// Per-entry identity of the executing definition.
    pub entry_id: AutocmdEntryId,
    /// Shared API id, absent for legacy `:autocmd` entries.
    pub api_id: Option<u64>,
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
    /// Canonical source pattern (`<buffer=N>` for buffer-local).
    pub pattern: String,
    /// Selected buffer for a buffer-local pattern.
    pub buffer: Option<BufHandle>,
    /// Match bytes supplied by this event occurrence.
    pub match_name: OxStr,
    /// File bytes supplied by this event occurrence.
    pub file_name: OxStr,
    /// Optional user-facing description.
    pub description: Option<String>,
    /// User data supplied by `nvim_exec_autocmds`.
    pub data: Option<Object>,
}

impl AutocmdAction {
    /// Builds the single dictionary argument passed to a Lua autocmd callback.
    ///
    /// # Errors
    ///
    /// Returns [`AutocmdError::IdentifierOverflow`] if the autocmd or augroup
    /// identifier cannot be represented by the callback's signed integer type.
    pub fn callback_args(&self) -> Result<Vec<Object>, AutocmdError> {
        let id = i64::try_from(self.api_id.unwrap_or_default())
            .map_err(|_| AutocmdError::IdentifierOverflow("autocmd id"))?;
        let buffer = self.buffer.map_or(Object::Integer(0), Object::Buffer);
        let mut entries = vec![
            (OxStr::from("id"), Object::Integer(id)),
            (
                OxStr::from("event"),
                Object::String(OxStr::from(self.event.as_str())),
            ),
            (
                OxStr::from("match"),
                Object::String(self.match_name.clone()),
            ),
            (OxStr::from("buf"), buffer),
            (
                OxStr::from("file"),
                Object::String(self.file_name.clone()),
            ),
        ];
        if self.group != AugroupId::default() {
            let group = i64::try_from(self.group.0)
                .map_err(|_| AutocmdError::IdentifierOverflow("augroup id"))?;
            entries.push((OxStr::from("group"), Object::Integer(group)));
        }
        if let Some(data) = &self.data {
            entries.push((OxStr::from("data"), data.clone()));
        }
        Ok(vec![Object::Dict(Dict(entries))])
    }
}

/// One registered autocmd definition exposed to API query layers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutocmdDefinition {
    /// Per-entry identity.
    pub entry_id: AutocmdEntryId,
    /// Shared API id, absent for legacy `:autocmd` entries.
    pub api_id: Option<u64>,
    /// Event matched by the definition.
    pub event: Event,
    /// Destination augroup.
    pub group: AugroupId,
    /// Augroup name, absent for the default group.
    pub group_name: Option<String>,
    /// Canonical source pattern (`<buffer=N>` for buffer-local).
    pub pattern: String,
    /// Buffer selected by a buffer-local pattern.
    pub buffer: Option<BufHandle>,
    /// Host-owned executable payload.
    pub kind: AutocmdKind,
    /// Whether the definition is removed after execution.
    pub once: bool,
    /// Whether nested autocmds may fire.
    pub nested: bool,
    /// Optional user-facing description.
    pub description: Option<String>,
}

/// Execution seam implemented by Vimscript/Lua hosting layers.
pub trait AutocmdSink {
    /// Host execution failure.
    type Error;
    /// Executes one already-planned action.
    ///
    /// # Errors
    ///
    /// Returns an error if the host cannot execute the action.
    fn run(&mut self, action: &AutocmdAction) -> Result<(), Self::Error>;
}

/// Registration options shared by Ex and API-created autocmds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AutocmdOptions {
    /// Destination augroup, or the default group.
    pub group: AugroupId,
    /// Buffer substituted for `<abuf>`, `<buffer>`, and `<buffer=0>`.
    pub buffer: Option<BufHandle>,
    /// Remove after the first firing plan.
    pub once: bool,
    /// Permit nested autocmd execution.
    pub nested: bool,
    /// Optional user-facing description.
    pub description: Option<String>,
}

/// Event occurrence supplied to the firing planner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutocmdContext<'a> {
    /// Buffer associated with the event.
    pub buffer: Option<BufHandle>,
    /// Event file name as raw Vim bytes, normally a buffer or file name.
    pub file_name: Option<&'a [u8]>,
    /// Explicit event match bytes when they differ from the associated file.
    pub match_name: Option<&'a [u8]>,
    /// True when this event may fire nested, false when it is raised inside a
    /// non-`++nested` outer autocmd and must be suppressed entirely.
    ///
    /// The host passes the *outer* autocmd's `++nested` flag, and `true` for a
    /// top-level event. Gating is decided once per event, never per candidate.
    pub nested: bool,
    /// User data supplied by an explicit API event occurrence.
    pub data: Option<&'a Object>,
}

impl Default for AutocmdContext<'_> {
    fn default() -> Self {
        Self {
            buffer: None,
            file_name: None,
            match_name: None,
            // A top-level event is not raised inside a non-nested outer
            // autocmd, so nesting is permitted and no event-level gate applies.
            nested: true,
            data: None,
        }
    }
}

/// Ordered actions for one event occurrence.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FiringPlan {
    /// Actions which may run immediately, in global definition order.
    pub ready: Vec<AutocmdAction>,
}

/// Selector for query and clear operations. Each class is OR-combined
/// within itself and AND-combined across classes. Pattern comparisons are
/// exact against the canonical stored pattern.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AutocmdFilter<'a> {
    /// Optional exact augroup. `None` matches all groups including
    /// tombstoned; `Some(id)` matches only live (or default) groups.
    pub group: Option<AugroupId>,
    /// Optional event set (OR within). `None` matches all events.
    pub events: Option<&'a [Event]>,
    /// Optional exact canonical pattern set (OR within). `None` matches all.
    pub patterns: Option<&'a [String]>,
    /// Optional buffer set (OR within). `None` matches all. Only
    /// buffer-local entries can match a buffer filter.
    pub buffers: Option<&'a [BufHandle]>,
    /// Optional shared API id. `None` matches all.
    pub api_id: Option<u64>,
}

/// Invalid registration or augroup operation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AutocmdError {
    /// An empty augroup name was requested.
    #[error("augroup name must not be empty")]
    EmptyGroupName,
    /// An empty event list was requested.
    #[error("autocmd event list must not be empty")]
    EmptyEvent,
    /// An empty pattern was requested.
    #[error("autocmd pattern must not be empty")]
    EmptyPattern,
    /// `<abuf>` or `<buffer>` was used without a registration buffer.
    #[error("<abuf> requires a buffer handle")]
    MissingBuffer,
    /// A `<buffer=N>` pattern was malformed.
    #[error("invalid buffer pattern {0:?}")]
    InvalidBufferPattern(String),
    /// The selected augroup does not exist.
    #[error("unknown augroup {0:?}")]
    UnknownGroup(AugroupId),
    /// Pattern alternation braces were malformed.
    #[error("unbalanced braces in autocmd pattern")]
    UnbalancedBraces,
    /// An internal identifier cannot cross the signed API boundary.
    #[error("{0} exceeds Integer range")]
    IdentifierOverflow(&'static str),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StoredPattern {
    Glob(Vec<String>),
    Buffer(BufHandle),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GroupState {
    Live(String),
    Tombstoned,
}

/// Name returned for entries whose augroup was legacy-deleted.
const DELETED_GROUP_NAME: &str = "--Deleted--";

#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    id: AutocmdEntryId,
    api_id: Option<u64>,
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
    groups: BTreeMap<AugroupId, GroupState>,
    group_names: BTreeMap<String, AugroupId>,
    entries: Vec<Entry>,
    ignored: BTreeSet<Event>,
    next_entry_id: u64,
    next_api_id: u64,
    next_group: u64,
    next_sequence: u64,
}

impl Default for Autocmds {
    fn default() -> Self {
        Self::new()
    }
}

impl Autocmds {
    /// Creates an autocmd store with Neovim's core augroups and no definitions.
    /// The `nvim.popupmenu` core group occupies id 1; user groups start at 2.
    /// Group ids are monotonic and never reused after deletion.
    #[must_use]
    pub fn new() -> Self {
        let popupmenu = AugroupId(1);
        Self {
            groups: BTreeMap::from([(popupmenu, GroupState::Live("nvim.popupmenu".to_owned()))]),
            group_names: BTreeMap::from([("nvim.popupmenu".to_owned(), popupmenu)]),
            entries: Vec::new(),
            ignored: BTreeSet::new(),
            next_entry_id: 1,
            next_api_id: 1,
            next_group: 2,
            next_sequence: 1,
        }
    }

    /// Creates a group or returns its existing identity. `clear` implements
    /// `augroup!` for API callers.
    ///
    /// # Errors
    ///
    /// Returns [`AutocmdError::EmptyGroupName`] when `name` is empty, or
    /// [`AutocmdError::UnknownGroup`] if clearing the existing group fails.
    pub fn create_group(&mut self, name: &str, clear: bool) -> Result<AugroupId, AutocmdError> {
        if name.is_empty() {
            return Err(AutocmdError::EmptyGroupName);
        }
        if let Some(id) = self.group_names.get(name).copied() {
            if clear {
                self.clear_group(id)?;
            }
            return Ok(id);
        }
        let id = self.allocate_group_id();
        self.groups.insert(id, GroupState::Live(name.to_owned()));
        self.group_names.insert(name.to_owned(), id);
        Ok(id)
    }

    /// Looks up a live augroup by name.
    #[must_use]
    pub fn group(&self, name: &str) -> Option<AugroupId> {
        self.group_names.get(name).copied()
    }

    /// Name of a live augroup by id.
    #[must_use]
    pub fn group_name(&self, id: AugroupId) -> Option<&str> {
        match self.groups.get(&id) {
            Some(GroupState::Live(name)) => Some(name),
            _ => None,
        }
    }

    /// Whether a group id exists (live or tombstoned). The default group
    /// always exists.
    #[must_use]
    pub fn has_group(&self, id: AugroupId) -> bool {
        id == AugroupId::default() || self.groups.contains_key(&id)
    }

    /// Whether a group id is live (or the default group).
    #[must_use]
    pub fn is_live_group(&self, id: AugroupId) -> bool {
        id == AugroupId::default() || matches!(self.groups.get(&id), Some(GroupState::Live(_)))
    }

    /// Whether a group or registered group/event/pattern query exists.
    /// The input omits the leading `#`, as in upstream `au_exists()`.
    #[must_use]
    pub fn exists(&self, query: &str) -> bool {
        let mut fields = query.splitn(3, '#');
        let first = fields.next().unwrap_or_default();
        let second = fields.next();
        let third = fields.next();
        let (group, event_name, pattern) = if let Some(group) = self.group(first) {
            let Some(event_name) = second else {
                return true;
            };
            (Some(group), event_name, third)
        } else {
            (None, first, second)
        };
        let Some(event) = Event::from_name(event_name) else {
            return false;
        };
        self.entries.iter().any(|entry| {
            entry.event == event
                && group.is_none_or(|group| entry.options.group == group)
                && pattern.is_none_or(|pattern| entry.source_pattern.eq_ignore_ascii_case(pattern))
        })
    }

    /// API deletion: removes the live group, its name, and all entries.
    /// Returns the removed executable payloads for callback-ref cleanup.
    ///
    /// # Errors
    ///
    /// Returns [`AutocmdError::UnknownGroup`] if `id` does not identify a
    /// group.
    pub fn delete_group(&mut self, id: AugroupId) -> Result<Vec<AutocmdKind>, AutocmdError> {
        let Some(state) = self.groups.remove(&id) else {
            return Err(AutocmdError::UnknownGroup(id));
        };
        if let GroupState::Live(name) = state {
            self.group_names.remove(&name);
        }
        let mut removed = Vec::new();
        self.entries.retain(|entry| {
            if entry.options.group == id {
                removed.push(entry.kind.clone());
                false
            } else {
                true
            }
        });
        Ok(removed)
    }

    /// Legacy deletion (`:augroup!`): removes the group name but preserves
    /// entries under a tombstone. Old entries remain globally queryable;
    /// recreating the same name allocates a new live group id.
    ///
    /// # Errors
    ///
    /// Returns [`AutocmdError::UnknownGroup`] if `id` does not identify a
    /// live group.
    pub fn delete_group_legacy(&mut self, id: AugroupId) -> Result<(), AutocmdError> {
        match self.groups.get(&id) {
            Some(GroupState::Live(name)) => {
                let name = name.clone();
                self.group_names.remove(&name);
                self.groups.insert(id, GroupState::Tombstoned);
                Ok(())
            }
            _ => Err(AutocmdError::UnknownGroup(id)),
        }
    }

    /// Clears every definition in an augroup while preserving the group.
    /// Returns the removed executable payloads for callback-ref cleanup.
    ///
    /// # Errors
    ///
    /// Returns [`AutocmdError::UnknownGroup`] if `id` does not identify a
    /// live group.
    pub fn clear_group(&mut self, id: AugroupId) -> Result<Vec<AutocmdKind>, AutocmdError> {
        if id != AugroupId::default() && !self.is_live_group(id) {
            return Err(AutocmdError::UnknownGroup(id));
        }
        let mut removed = Vec::new();
        self.entries.retain(|entry| {
            if entry.options.group == id {
                removed.push(entry.kind.clone());
                false
            } else {
                true
            }
        });
        Ok(removed)
    }

    /// Registers one API batch: one shared API id across every event ×
    /// pattern entry. Returns the API id.
    ///
    /// # Errors
    ///
    /// Returns an error when `events` or `patterns` is empty, the destination
    /// group is not live, a buffer-local pattern lacks a valid buffer, or a
    /// pattern is malformed.
    pub fn register_api(
        &mut self,
        events: &[Event],
        patterns: &str,
        kind: &AutocmdKind,
        options: &AutocmdOptions,
    ) -> Result<u64, AutocmdError> {
        if events.is_empty() {
            return Err(AutocmdError::EmptyEvent);
        }
        let parsed = self.parse_registration(patterns, options)?;
        let api_id = self.next_api_id;
        self.next_api_id = self.next_api_id.saturating_add(1);
        self.commit_registration(events, kind, options, parsed, Some(api_id));
        Ok(api_id)
    }

    /// Registers legacy `:autocmd` entries. No API id is assigned.
    ///
    /// # Errors
    ///
    /// Returns an error when `events` or `patterns` is empty, the destination
    /// group is not live, a buffer-local pattern lacks a valid buffer, or a
    /// pattern is malformed.
    pub fn register_legacy(
        &mut self,
        events: &[Event],
        patterns: &str,
        kind: &AutocmdKind,
        options: &AutocmdOptions,
    ) -> Result<(), AutocmdError> {
        if events.is_empty() {
            return Err(AutocmdError::EmptyEvent);
        }
        let parsed = self.parse_registration(patterns, options)?;
        self.commit_registration(events, kind, options, parsed, None);
        Ok(())
    }

    /// Validates the group and parses every pattern item without mutating
    /// state. A malformed later pattern leaves entries and counters unchanged.
    fn parse_registration(
        &self,
        patterns: &str,
        options: &AutocmdOptions,
    ) -> Result<Vec<(StoredPattern, String)>, AutocmdError> {
        if options.group != AugroupId::default() && !self.is_live_group(options.group) {
            return Err(AutocmdError::UnknownGroup(options.group));
        }
        let parts = split_pattern_list(patterns)?;
        if parts.iter().all(String::is_empty) {
            return Err(AutocmdError::EmptyPattern);
        }
        parts
            .into_iter()
            .filter(|part| !part.is_empty())
            .map(|part| parse_pattern(&part, options.buffer))
            .collect()
    }

    fn commit_registration(
        &mut self,
        events: &[Event],
        kind: &AutocmdKind,
        options: &AutocmdOptions,
        parsed: Vec<(StoredPattern, String)>,
        api_id: Option<u64>,
    ) {
        for (stored, source) in parsed {
            for &event in events {
                let entry_id = AutocmdEntryId(self.next_entry_id);
                self.next_entry_id = self.next_entry_id.saturating_add(1);
                let sequence = self.next_sequence;
                self.next_sequence = self.next_sequence.saturating_add(1);
                self.entries.push(Entry {
                    id: entry_id,
                    api_id,
                    sequence,
                    event,
                    pattern: stored.clone(),
                    source_pattern: source.clone(),
                    kind: kind.clone(),
                    options: options.clone(),
                });
            }
        }
    }

    /// Returns definitions matching the filter, in registration order.
    /// Group filtering accepts only live (or default) groups; a `None` group
    /// includes tombstoned entries.
    #[must_use]
    pub fn query(&self, filter: &AutocmdFilter<'_>) -> Vec<AutocmdDefinition> {
        self.entries
            .iter()
            .filter(|entry| self.entry_matches_filter(entry, filter))
            .map(|entry| self.definition(entry))
            .collect()
    }

    /// Removes definitions matching the filter and returns the removed
    /// executable payloads for callback-ref cleanup.
    pub fn clear(&mut self, filter: &AutocmdFilter<'_>) -> Vec<AutocmdKind> {
        let doomed: BTreeSet<AutocmdEntryId> = self
            .entries
            .iter()
            .filter(|entry| self.entry_matches_filter(entry, filter))
            .map(|entry| entry.id)
            .collect();
        let mut removed = Vec::new();
        self.entries.retain(|entry| {
            if doomed.contains(&entry.id) {
                removed.push(entry.kind.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Removes every entry sharing one API id. Returns the removed payloads.
    pub fn delete_api_id(&mut self, api_id: u64) -> Vec<AutocmdKind> {
        let mut removed = Vec::new();
        self.entries.retain(|entry| {
            if entry.api_id == Some(api_id) {
                removed.push(entry.kind.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Removes one entry by entry id (callback-truthy deletion).
    /// Returns the removed payload if the entry existed.
    pub fn delete_entry(&mut self, entry_id: AutocmdEntryId) -> Option<AutocmdKind> {
        let mut removed = None;
        self.entries.retain(|entry| {
            if entry.id == entry_id {
                removed = Some(entry.kind.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Returns all definitions in registration order.
    #[must_use]
    pub fn definitions(&self) -> Vec<AutocmdDefinition> {
        self.entries
            .iter()
            .map(|entry| self.definition(entry))
            .collect()
    }

    /// Adds an event to the editor's `eventignore` set.
    pub fn ignore(&mut self, event: Event) {
        self.ignored.insert(event);
    }
    /// Removes an event from the editor's `eventignore` set.
    pub fn unignore(&mut self, event: Event) {
        self.ignored.remove(&event);
    }
    /// Whether the event is currently ignored.
    #[must_use]
    pub fn is_ignored(&self, event: Event) -> bool {
        self.ignored.contains(&event)
    }
    /// Number of registered pattern definitions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    /// Whether no definitions are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Removes all buffer-local definitions for a wiped buffer and returns
    /// the removed executable payloads for callback-ref cleanup.
    pub fn remove_buffer(&mut self, buffer: BufHandle) -> Vec<AutocmdKind> {
        let mut removed = Vec::new();
        self.entries.retain(|entry| {
            if matches!(entry.pattern, StoredPattern::Buffer(value) if value == buffer) {
                removed.push(entry.kind.clone());
                false
            } else {
                true
            }
        });
        removed
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
        self.plan_filtered(event, None, context)
    }

    /// Builds the firing plan for one event occurrence restricted to one
    /// augroup, the shape `:doautocmd {group} {event}` needs
    /// (`autocmd.c` `do_doautocmd` → `apply_autocmds_group` with
    /// `group != AUGROUP_ALL`). Definitions outside `group` never match;
    /// matching order is unchanged.
    pub fn plan_in_group(
        &mut self,
        event: Event,
        group: AugroupId,
        context: AutocmdContext<'_>,
    ) -> FiringPlan {
        self.plan_filtered(event, Some(group), context)
    }

    fn plan_filtered(
        &mut self,
        event: Event,
        group: Option<AugroupId>,
        context: AutocmdContext<'_>,
    ) -> FiringPlan {
        if self.ignored.contains(&event) || !context.nested {
            return FiringPlan::default();
        }
        let mut matched: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.event == event
                    && group.is_none_or(|group| entry.options.group == group)
                    && pattern_matches(
                        &entry.pattern,
                        context.buffer,
                        context.match_name.or(context.file_name),
                    )
            })
            .collect();
        matched.sort_by_key(|entry| entry.sequence);
        let ready: Vec<AutocmdAction> = matched
            .into_iter()
            .map(|entry| self.action(entry, context))
            .collect();
        FiringPlan { ready }
    }

    /// Removes the one-shot definition identified by `entry_id` once the host
    /// begins executing it. Returns the removed payload when a `++once`
    /// definition was consumed.
    pub fn consume_once(&mut self, entry_id: AutocmdEntryId) -> Option<AutocmdKind> {
        let mut removed = None;
        self.entries.retain(|entry| {
            if entry.id == entry_id && entry.options.once {
                removed = Some(entry.kind.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Whether the entry identified by `entry_id` is still registered.
    /// Plan executors call this before executing a queued action to detect
    /// deletion or clearing by an earlier callback in the same firing plan.
    #[must_use]
    pub fn is_entry_live(&self, entry_id: AutocmdEntryId) -> bool {
        self.entries.iter().any(|entry| entry.id == entry_id)
    }
    /// Whether any registered definition still owns this Lua callback reference.
    #[must_use]
    pub fn uses_lua_callback(&self, reference: u64) -> bool {
        self.entries.iter().any(
            |entry| matches!(entry.kind, AutocmdKind::LuaCallback(stored) if stored == reference),
        )
    }
    fn allocate_group_id(&mut self) -> AugroupId {
        let mut candidate = self.next_group;
        loop {
            let id = AugroupId(candidate);
            if !self.groups.contains_key(&id) {
                self.next_group = candidate.saturating_add(1);
                return id;
            }
            candidate = candidate.saturating_add(1);
        }
    }

    fn group_name_for(&self, id: AugroupId) -> Option<String> {
        match self.groups.get(&id) {
            Some(GroupState::Live(name)) => Some(name.clone()),
            Some(GroupState::Tombstoned) => Some(DELETED_GROUP_NAME.to_owned()),
            None => None,
        }
    }

    fn entry_matches_filter(&self, entry: &Entry, filter: &AutocmdFilter<'_>) -> bool {
        let group_matches = filter.group.is_none_or(|id| {
            id == entry.options.group && (id == AugroupId::default() || self.is_live_group(id))
        });
        if !group_matches {
            return false;
        }
        let events_match = filter
            .events
            .is_none_or(|events| events.contains(&entry.event));
        if !events_match {
            return false;
        }
        let patterns_match = filter
            .patterns
            .is_none_or(|patterns| patterns.contains(&entry.source_pattern));
        if !patterns_match {
            return false;
        }
        let buffers_match = filter.buffers.is_none_or(|buffers| {
            matches!(entry.pattern, StoredPattern::Buffer(handle) if buffers.contains(&handle))
        });
        if !buffers_match {
            return false;
        }
        filter.api_id.is_none_or(|id| entry.api_id == Some(id))
    }

    fn definition(&self, entry: &Entry) -> AutocmdDefinition {
        AutocmdDefinition {
            entry_id: entry.id,
            api_id: entry.api_id,
            event: entry.event,
            group: entry.options.group,
            group_name: self.group_name_for(entry.options.group),
            pattern: entry.source_pattern.clone(),
            buffer: match entry.pattern {
                StoredPattern::Buffer(buffer) => Some(buffer),
                StoredPattern::Glob(_) => None,
            },
            kind: entry.kind.clone(),
            once: entry.options.once,
            nested: entry.options.nested,
            description: entry.options.description.clone(),
        }
    }

    fn action(&self, entry: &Entry, context: AutocmdContext<'_>) -> AutocmdAction {
        AutocmdAction {
            entry_id: entry.id,
            api_id: entry.api_id,
            event: entry.event,
            kind: entry.kind.clone(),
            once: entry.options.once,
            nested: entry.options.nested,
            group: entry.options.group,
            group_name: self.group_name_for(entry.options.group),
            pattern: entry.source_pattern.clone(),
            buffer: context.buffer.or(match entry.pattern {
                StoredPattern::Buffer(buffer) => Some(buffer),
                StoredPattern::Glob(_) => None,
            }),
            match_name: match context.match_name {
                Some(name) => OxStr::from(name),
                None => context.file_name.map_or_else(|| OxStr(Vec::new()), |name| {
                    let name = OxStr::from(name);
                    if entry.event.pattern_kind() == PatternKind::None || name.as_bytes().is_empty() {
                        return name;
                    }
                    let path = crate::excmd_exec::path_from_ox_str(&name);
                    if path.is_absolute() {
                        return name;
                    }
                    let full = std::env::current_dir().unwrap_or_default().join(path);
                    #[cfg(unix)]
                    {
                        use std::os::unix::ffi::OsStrExt;

                        OxStr::from(full.as_os_str().as_bytes())
                    }
                    #[cfg(not(unix))]
                    {
                        OxStr::from(full.to_string_lossy().as_ref())
                    }
                }),
            },
            file_name: OxStr::from(context.file_name.unwrap_or_default()),
            description: entry.options.description.clone(),
            data: context.data.cloned(),
        }
    }
}

fn pattern_matches(
    pattern: &StoredPattern,
    buffer: Option<BufHandle>,
    file_name: Option<&[u8]>,
) -> bool {
    match pattern {
        StoredPattern::Buffer(expected) => buffer == Some(*expected),
        StoredPattern::Glob(patterns) => {
            let name = match file_name {
                Some(name) => String::from_utf8_lossy(name),
                None => Cow::Borrowed(""),
            };
            let name = name.as_ref();
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

/// Parses one comma-split pattern item into a stored representation and
/// canonical source string. `<abuf>`, `<buffer>`, `<buffer=0>`, and
/// `<buffer=N>` resolve to a buffer handle with canonical `<buffer=N>`;
/// everything else is a literal glob.
fn parse_pattern(
    part: &str,
    fallback: Option<BufHandle>,
) -> Result<(StoredPattern, String), AutocmdError> {
    if let Some(handle) = resolve_buffer_pattern(part, fallback)? {
        let n: i64 = handle.into();
        return Ok((StoredPattern::Buffer(handle), format!("<buffer={n}>")));
    }
    let expanded = expand_braces(part).ok_or(AutocmdError::UnbalancedBraces)?;
    Ok((StoredPattern::Glob(expanded), part.to_owned()))
}

fn resolve_buffer_pattern(
    part: &str,
    fallback: Option<BufHandle>,
) -> Result<Option<BufHandle>, AutocmdError> {
    if part == "<abuf>" || part == "<buffer>" || part == "<buffer=0>" {
        return fallback.ok_or(AutocmdError::MissingBuffer).map(Some);
    }
    if let Some(rest) = part.strip_prefix("<buffer=")
        && let Some(num_str) = rest.strip_suffix('>')
    {
        let n: i64 = num_str
            .parse()
            .map_err(|_| AutocmdError::InvalidBufferPattern(part.to_owned()))?;
        if n == 0 {
            return fallback.ok_or(AutocmdError::MissingBuffer).map(Some);
        }
        let handle = BufHandle::try_from(n)
            .map_err(|_| AutocmdError::InvalidBufferPattern(part.to_owned()))?;
        return Ok(Some(handle));
    }
    Ok(None)
}

fn split_pattern_list(patterns: &str) -> Result<Vec<String>, AutocmdError> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut escaped = false;
    for ch in patterns.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            current.push(ch);
            continue;
        }
        match ch {
            '{' => {
                depth += 1;
                current.push(ch);
            }
            '}' if depth == 0 => return Err(AutocmdError::UnbalancedBraces),
            '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                result.push(current);
                current = String::new();
            }
            _ => current.push(ch),
        }
    }
    if escaped {
        current.push('\\');
    }
    if depth != 0 {
        return Err(AutocmdError::UnbalancedBraces);
    }
    result.push(current);
    Ok(result)
}

pub(crate) fn expand_braces(pattern: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = pattern.chars().collect();
    let start = chars.iter().position(|ch| *ch == '{');
    let Some(start) = start else {
        return Some(vec![pattern.to_owned()]);
    };
    let mut depth = 0usize;
    let mut end = None;
    for (index, ch) in chars.iter().enumerate().skip(start) {
        if *ch == '{' {
            depth += 1;
        }
        if *ch == '}' {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                end = Some(index);
                break;
            }
        }
    }
    let end = end?;
    let prefix: String = chars[..start].iter().collect();
    let suffix: String = chars[end + 1..].iter().collect();
    let middle: String = chars[start + 1..end].iter().collect();
    let alternatives = split_pattern_list(&middle).ok()?;
    let mut result = Vec::new();
    for alternative in alternatives {
        for expanded in expand_braces(&format!("{prefix}{alternative}{suffix}"))? {
            result.push(expanded);
        }
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
                for column in 1..=text.len() {
                    next[column] = row[column] || next[column - 1];
                }
            }
            '?' => {
                next[1..].copy_from_slice(&row[..text.len()]);
            }
            '\\' if index + 1 < pattern.len() => {
                index += 1;
                for column in 1..=text.len() {
                    next[column] = row[column - 1] && pattern[index] == text[column - 1];
                }
            }
            literal => {
                for column in 1..=text.len() {
                    next[column] = row[column - 1] && literal == text[column - 1];
                }
            }
        }
        row = next;
        index += 1;
    }
    row[text.len()]
}
