//! Session-owned runtime state for the API surface.
//!
//! [`ApiSession`] is the public owner of the editor and its runtime state:
//! construction wraps the sole `Rc<RefCell<Editor>>` carrier, and every
//! API operation borrows `editor` or `state` only for the shortest
//! statement-scoped operation, ending each borrow before any reentrant
//! host code (Lua, Vimscript, autocmds, callbacks) runs.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::rc::{Rc, Weak};

use ox_editor::{Editor, ModeMachine};
use ox_rpc::{CHAN_STDERR, CHAN_STDIO, ChannelId};
use ox_types::{BufHandle, Dict, OxStr};
use ox_ui::{ChromeState, HlState, UiChannels};

use crate::CommandExecutor;
use crate::runtime::{
    AutocmdExecutor, ChannelInfo, ChannelSink, FileIO, LuaExecutor, SearchPathItem, TerminalInput,
};

/// One entry on the per-session caller stack, keyed by a unique token so an
/// [`ApiCallerGuard`] removes exactly its own frame on drop regardless of
/// the order in which live guards are dropped.
pub(crate) struct CallerFrame {
    pub(crate) token: u64,
    pub(crate) channel: Option<ChannelId>,
}

/// The caller stack a guard restores into; shared so a guard can outlive the
/// state borrow that created it without keeping the session alive.
pub(crate) type CallerStack = Vec<CallerFrame>;

/// Per-connection mutable state for the API surface, owned by
/// [`ApiSession`]; its fields stay private outside the crate.
pub struct SessionState {
    pub(crate) namespaces: BTreeMap<OxStr, u32>,
    pub(crate) next_namespace: u32,
    /// Automatic `nvim_echo` message ids belong to this editor session.
    pub(crate) next_message_id: i64,
    pub(crate) ui_channels: UiChannels,
    pub(crate) ui_extra: BTreeMap<u64, crate::ui::UiExtra>,
    pub(crate) chrome: ChromeState,
    /// Active highlight definitions used for rendering (the namespace selected
    /// by `nvim_set_hl_ns()`/`nvim_set_hl_ns_fast()`, global ns 0 by default).
    pub(crate) highlights: HlState,
    /// Per-namespace highlight tables keyed by `ns_id`; ns 0 is global.
    pub(crate) hl_namespaces: BTreeMap<i64, HlState>,
    pub(crate) current_hl_ns: i64,
    pub(crate) fast_hl_ns: i64,
    pub(crate) channels: BTreeMap<u64, ChannelInfo>,
    pub(crate) subscriptions: BTreeMap<u64, BTreeSet<OxStr>>,
    pub(crate) terminal_inputs: BTreeMap<u64, TerminalInput>,
    pub(crate) paste_cancelled: bool,
    pub(crate) channel_sink: Option<Box<dyn ChannelSink>>,
    /// In-flight `FileType` dispatches as `(buffer, raw filetype)` pairs. A
    /// a changed value may fire again.
    pub(crate) filetype_dispatches: Vec<(BufHandle, String)>,
    /// Lua registry references awaiting a free while every Lua host slot is
    /// checked out (a nested decoration-provider replacement inside a
    /// callback). Lifetime-ordered: the same integer may appear again after
    /// its slot is recycled, so entries — not numeric identity — define
    /// exactly-once ownership.
    pub(crate) pending_lua_callback_releases: Vec<usize>,
    /// Pending autocmd-callback registry frees, symmetric to the Lua list.
    pub(crate) pending_autocmd_callback_releases: Vec<u64>,
    /// Caller stack, shared so guards upgrade across reentry; see
    /// [`ApiCallerGuard`].
    pub(crate) callers: Rc<RefCell<CallerStack>>,
    pub(crate) next_caller_token: u64,
    pub(crate) job_sink: Option<Box<dyn ChannelSink>>,
    /// Ex-command hosts, one per reentry depth: index 0 is the primary host,
    /// index 1 the nested fallback, deeper indices are forked on demand.
    pub(crate) command_pool: Vec<Rc<RefCell<Box<dyn CommandExecutor>>>>,
    /// How many `with_command_executor` frames are currently live; also the
    /// index of the slot the next frame takes.
    pub(crate) command_depth: usize,
    /// Lua hosts, one per reentry depth; see [`Self::command_pool`].
    pub(crate) lua_pool: Vec<Rc<RefCell<Box<dyn LuaExecutor>>>>,
    /// Live `with_lua_executor` frames; see [`Self::command_depth`].
    pub(crate) lua_depth: usize,
    /// Autocmd hosts, one per reentry depth; see [`Self::command_pool`].
    pub(crate) autocmd_pool: Vec<Rc<RefCell<Box<dyn AutocmdExecutor>>>>,
    /// Live `with_autocmd_executor` frames; see [`Self::command_depth`].
    pub(crate) autocmd_depth: usize,
    /// Never checked out: the prototype a deeper-than-nested reentrant call
    /// forks a fresh host from (`CommandExecutor::fork`). It is the only host
    /// no frame ever borrows, so it is the only safe fork source.
    pub(crate) command_prototype: Option<Box<dyn CommandExecutor>>,
    /// Never checked out; see [`Self::command_prototype`].
    pub(crate) lua_prototype: Option<Box<dyn LuaExecutor>>,
    /// Never checked out; see [`Self::command_prototype`].
    pub(crate) autocmd_prototype: Option<Box<dyn AutocmdExecutor>>,
    pub(crate) mode_machine: Option<Rc<RefCell<ModeMachine>>>,
    pub(crate) file_io: Box<dyn FileIO>,
    /// The expanded runtime search path, keyed by the ('runtimepath',
    /// 'packpath') pair it was built from.
    pub(crate) search_path: Option<((String, String), Vec<SearchPathItem>)>,
    pub(crate) saved_context: Option<Dict>,
}

impl Default for SessionState {
    fn default() -> Self {
        // Seeding mirrors the old RuntimeState::default() exactly: stdio and
        // stderr channels exist from construction, namespaces allocate from
        // 1, and highlight namespace 0 (global) is preinstalled.
        let mut channels = BTreeMap::new();
        channels.insert(CHAN_STDIO.get(), ChannelInfo::stdio_rpc());
        channels.insert(CHAN_STDERR.get(), ChannelInfo::stderr_bytes());
        Self {
            namespaces: BTreeMap::new(),
            next_namespace: 1,
            next_message_id: 1,
            ui_channels: UiChannels::new(),
            ui_extra: BTreeMap::new(),
            chrome: ChromeState::new(),
            highlights: HlState::new(),
            hl_namespaces: BTreeMap::from([(0, HlState::new())]),
            current_hl_ns: 0,
            fast_hl_ns: 0,
            channels,
            subscriptions: BTreeMap::new(),
            terminal_inputs: BTreeMap::new(),
            paste_cancelled: false,
            channel_sink: None,
            filetype_dispatches: Vec::new(),
            pending_lua_callback_releases: Vec::new(),
            pending_autocmd_callback_releases: Vec::new(),
            callers: Rc::new(RefCell::new(Vec::new())),
            next_caller_token: 0,
            job_sink: None,
            command_pool: Vec::new(),
            command_depth: 0,
            command_prototype: None,
            lua_pool: Vec::new(),
            lua_depth: 0,
            lua_prototype: None,
            autocmd_pool: Vec::new(),
            autocmd_depth: 0,
            autocmd_prototype: None,
            mode_machine: None,
            file_io: Box::new(crate::runtime::StdFileIO),
            search_path: None,
            saved_context: None,
        }
    }
}

/// The public owner of one editor connection: the sole `Rc<RefCell<Editor>>`
/// carrier plus its private [`SessionState`].
pub struct ApiSession {
    editor: Rc<RefCell<Editor>>,
    state: RefCell<SessionState>,
}

impl ApiSession {
    /// Wraps the sole editor carrier; the session now owns it.
    pub fn new(editor: Rc<RefCell<Editor>>) -> Self {
        Self {
            editor,
            state: RefCell::new(SessionState::default()),
        }
    }

    /// Shortest-scope editor borrow; the closure must not run reentrant host
    /// code.
    pub fn with_editor<R>(&self, operation: impl FnOnce(&Editor) -> R) -> R {
        operation(&self.editor.borrow())
    }

    /// Shortest-scope mutable editor borrow; the closure must not run
    /// reentrant host code.
    pub fn with_editor_mut<R>(&self, operation: impl FnOnce(&mut Editor) -> R) -> R {
        operation(&mut self.editor.borrow_mut())
    }

    /// Shortest-scope shared state borrow; the closure must not run
    /// reentrant host code.
    pub fn with_state<R>(&self, operation: impl FnOnce(&SessionState) -> R) -> R {
        operation(&self.state.borrow())
    }

    /// Whether `id` was allocated by `nvim_create_namespace`
    /// (upstream `ns_initialized`, the check `vim.ui_attach` runs).
    #[must_use]
    pub fn namespace_is_initialized(&self, id: u32) -> bool {
        self.with_state(|state| id > 0 && id < state.next_namespace)
    }

    /// Shortest-scope mutable state borrow; the closure must not run
    /// reentrant host code or outlive its statement.
    pub fn with_state_mut<R>(&self, operation: impl FnOnce(&mut SessionState) -> R) -> R {
        operation(&mut self.state.borrow_mut())
    }

    /// The render surface — UI channels, highlights, and chrome — as one
    /// exclusive borrow for the server's redraw pipeline. The payload stays
    /// private; this is the only cross-crate window onto it.
    pub fn with_render_state<R>(
        &self,
        operation: impl FnOnce(&mut UiChannels, &mut HlState, &mut ChromeState) -> R,
    ) -> R {
        self.with_state_mut(|state| {
            operation(
                &mut state.ui_channels,
                &mut state.highlights,
                &mut state.chrome,
            )
        })
    }

    /// Shared handle to the caller stack, so a guard can be created after
    /// the state borrow that minted its token has ended.
    pub(crate) fn caller_stack(&self) -> Rc<RefCell<CallerStack>> {
        self.with_state(|state| Rc::clone(&state.callers))
    }

    /// Marks the current scope as an RPC request from `channel`.
    #[must_use]
    pub fn enter_rpc_call(&self, channel: ChannelId) -> ApiCallerGuard {
        self.enter_call(Some(channel))
    }

    /// Masks any outer RPC request while dispatching an internal API call.
    #[must_use]
    pub fn enter_internal_call(&self) -> ApiCallerGuard {
        self.enter_call(None)
    }

    fn enter_call(&self, caller: Option<ChannelId>) -> ApiCallerGuard {
        let stack = self.caller_stack();
        let token = self.with_state_mut(|state| {
            let token = state.next_caller_token;
            state.next_caller_token = state.next_caller_token.wrapping_add(1);
            token
        });
        stack.borrow_mut().push(CallerFrame {
            token,
            channel: caller,
        });
        ApiCallerGuard {
            stack: Rc::downgrade(&stack),
            token,
        }
    }

    /// The channel of the innermost live caller frame, if any.
    pub(crate) fn requesting_channel(&self) -> Option<ChannelId> {
        self.caller_stack()
            .borrow()
            .last()
            .and_then(|frame| frame.channel)
    }
}

/// Request-scoped identity for a registry API dispatch. Each guard owns a
/// unique token and a weak restoration handle: dropping it removes exactly
/// its own frame when the session still lives, and is inert after teardown
/// without recreating state or keeping the session alive.
pub struct ApiCallerGuard {
    stack: Weak<RefCell<CallerStack>>,
    token: u64,
}

impl Drop for ApiCallerGuard {
    fn drop(&mut self) {
        let Some(stack) = self.stack.upgrade() else {
            return; // Session already dropped: the guard is inert.
        };
        let pos = stack
            .borrow_mut()
            .iter()
            .rposition(|frame| frame.token == self.token);
        if let Some(pos) = pos {
            stack.borrow_mut().remove(pos);
        }
    }
}

impl ox_editor::ExEditorAccess for ApiSession {
    fn with_ex_editor<R>(&self, operation: impl FnOnce(&mut Editor) -> R) -> R {
        self.with_editor_mut(operation)
    }
}
