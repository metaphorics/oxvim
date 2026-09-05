//! Host-layer state shared by API families that cannot live in `ox-editor`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ox_editor::{AutocmdAction, ModeMachine, OptionValue};
use ox_rpc::{CHAN_STDERR, CHAN_STDIO, ChannelId};
use ox_types::{ApiError, Dict, Object, OxStr};

use crate::session::ApiSession;

/// Byte sink used by `nvim_chan_send`.
pub trait ChannelSink {
    /// Writes bytes to a channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the channel transport cannot accept the bytes.
    fn send(&mut self, channel: u64, bytes: &[u8]) -> Result<(), String>;

    /// Drains any PTY output produced by a previous `send` on a terminal channel.
    ///
    /// # Errors
    ///
    /// Returns an error when output cannot be read from the terminal transport.
    fn take_pty_output(&mut self, _channel: u64) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

/// Result of running one planned autocmd action.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AutocmdExecution {
    /// Keep the executing definition registered.
    #[default]
    Keep,
    /// Delete only the executing definition.
    Delete,
}

/// Host executor for actions produced by the editor's autocmd planner.
pub trait AutocmdExecutor {
    /// Executes one planned definition without editor access.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot execute the action.
    fn execute(&mut self, action: &AutocmdAction) -> Result<AutocmdExecution, String>;

    /// Executes one planned definition against the live editor the host is
    /// already running against. Hosts that re-enter the API override this;
    /// the default keeps editor-independent hosts unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot execute the action.
    fn execute_with_session(
        &mut self,
        _session: &ApiSession,
        action: &AutocmdAction,
    ) -> Result<AutocmdExecution, String> {
        self.execute(action)
    }

    /// Releases one Lua registry reference after its last definition is gone.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot release the registry reference.
    fn release_callback(&mut self, reference: u64) -> Result<(), String>;

    /// Creates an independent host instance sharing the same underlying
    /// registries, for a reentrant autocmd deeper than the primary/nested
    /// slot pair. The default reports no deeper host.
    fn fork(&self) -> Option<Box<dyn AutocmdExecutor>> {
        let _ = self;
        None
    }
}

/// Host seam the decoration-provider lifecycle uses to invoke and free Lua
/// callbacks. Delegates to the editor's existing `LuaExec` host so provider
/// callbacks share the exact execution path (`invoke_callback`,
/// `free_callback`) the Ex `:lua` commands use.
pub trait LuaExecutor {
    /// Compiles and runs one Lua chunk with `args` bound to `...`.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot compile or execute the chunk.
    fn exec(
        &mut self,
        session: &ApiSession,
        code: &str,
        args: Vec<Object>,
    ) -> Result<Object, String>;

    /// Invokes one registered Lua callback with converted values.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot invoke the callback.
    fn invoke_callback(
        &mut self,
        session: &ApiSession,
        reference: usize,
        args: Vec<Object>,
    ) -> Result<Object, String>;

    /// Invokes one registered Lua function and returns every result in stack order.
    ///
    /// Nil results remain explicit, while top-level tables, functions, and userdata
    /// are returned as fresh [`Object::LuaRef`] entries owned by the caller.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot invoke the function or preserve one
    /// of its return values.
    fn call_ref(
        &mut self,
        session: &ApiSession,
        reference: usize,
        args: Vec<Object>,
    ) -> Result<Vec<Object>, String>;

    /// Releases one Lua registry callback reference.
    ///
    /// # Errors
    ///
    /// Returns an error when the host cannot release the callback reference.
    fn free_callback(&mut self, reference: usize) -> Result<(), String>;

    /// Creates an independent host instance sharing the same underlying Lua
    /// registry, for a reentrant call deeper than the primary/nested slot
    /// pair. The default reports no deeper host.
    fn fork(&self) -> Option<Box<dyn LuaExecutor>> {
        let _ = self;
        None
    }
}

/// What a wildcard expansion may match, mirroring the `EW_DIR`/`EW_FILE` pair
/// upstream passes to `gen_expand_wildcards()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchKind {
    /// Directories only (`EW_DIR`), used to expand 'runtimepath' entries.
    Dirs,
    /// Regular files only (`EW_FILE`).
    Files,
    /// Either, upstream's `DIP_DIRFILE` used by `nvim_get_runtime_file()`.
    DirsAndFiles,
}

/// Filesystem seam used by runtime-file discovery.
pub trait FileIO {
    /// Expands one path pattern into the existing paths of the requested kind,
    /// in directory order. `*` and `?` match within a single path component,
    /// as they do for upstream's `gen_expand_wildcards()`.
    fn expand(&self, pattern: &str, kind: MatchKind) -> Vec<PathBuf>;

    /// Whether `path` names an existing directory (`os_isdir`).
    fn is_dir(&self, path: &Path) -> bool;

    /// Whether `path` names an existing readable file (`os_file_is_readable`).
    fn is_readable(&self, path: &Path) -> bool;
}

/// Standard filesystem implementation for runtime lookup.
#[derive(Default)]
pub struct StdFileIO;

impl FileIO for StdFileIO {
    fn expand(&self, pattern: &str, kind: MatchKind) -> Vec<PathBuf> {
        if pattern.is_empty() {
            return Vec::new();
        }
        let mut heads = vec![if pattern.starts_with('/') {
            PathBuf::from("/")
        } else {
            PathBuf::new()
        }];
        for component in pattern.split('/').filter(|part| !part.is_empty()) {
            if !component
                .as_bytes()
                .iter()
                .any(|byte| matches!(byte, b'*' | b'?'))
            {
                for head in &mut heads {
                    head.push(component);
                }
                continue;
            }
            heads = heads
                .iter()
                .flat_map(|head| expand_component(head, component))
                .collect();
        }
        heads.retain(|path| match std::fs::metadata(path) {
            Ok(metadata) => match kind {
                MatchKind::Dirs => metadata.is_dir(),
                MatchKind::Files => metadata.is_file(),
                MatchKind::DirsAndFiles => true,
            },
            Err(_) => false,
        });
        heads
    }

    fn is_dir(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
    }

    fn is_readable(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
    }
}

/// Lists the children of `directory` whose names match one wildcard component,
/// sorted so a wildcard entry expands deterministically. A leading dot is not
/// matched by a wildcard, as in shell globbing.
fn expand_component(directory: &Path, component: &str) -> Vec<PathBuf> {
    let listed = if directory.as_os_str().is_empty() {
        Path::new(".")
    } else {
        directory
    };
    let Ok(entries) = std::fs::read_dir(listed) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.') && wildcard(component.as_bytes(), name.as_bytes()))
        .collect();
    names.sort();
    names.into_iter().map(|name| directory.join(name)).collect()
}

pub(crate) fn wildcard(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t, mut star, mut retry) = (0, 0, None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = t;
        } else if let Some(index) = star {
            retry += 1;
            t = retry;
            p = index + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// One entry of the expanded runtime search path (runtime.c `SearchPathItem`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchPathItem {
    pub path: PathBuf,
    pub after: bool,
}

/// Whether one 'runtimepath' entry names an `after` directory. Only a final
/// path component of exactly `after` counts, as in runtime.c `path_is_after`.
fn path_is_after(entry: &str) -> bool {
    let bytes = entry.as_bytes();
    bytes.ends_with(b"after")
        && (bytes.len() == 5 || (bytes.len() > 5 && bytes[bytes.len() - 6] == b'/'))
}

/// Accumulates the search path with runtime.c's deduplication: the first
/// occurrence of a directory wins and later ones are dropped entirely.
struct SearchPathBuilder<'a> {
    file_io: &'a dyn FileIO,
    items: Vec<SearchPathItem>,
    used: HashSet<String>,
    after_queue: Vec<String>,
}

impl SearchPathBuilder<'_> {
    /// runtime.c `expand_rtp_entry`: an entry already on the path is skipped
    /// whole; otherwise every directory its wildcards expand to is pushed.
    fn push_entry(&mut self, entry: &str, after: bool) {
        if self.used.contains(entry) {
            return;
        }
        for path in self.file_io.expand(entry, MatchKind::Dirs) {
            let key = path.to_string_lossy().into_owned();
            if self.used.insert(key) {
                self.items.push(SearchPathItem { path, after });
            }
        }
    }

    /// runtime.c `expand_pack_entry`: a 'packpath' entry contributes the start
    /// bundles below it, and queues each bundle's `after` directory for the
    /// pass that runs once every non-after entry has been placed.
    fn push_pack_entry(&mut self, entry: &str) {
        for suffix in ["/pack/*/start/*", "/start/*"] {
            let bundle = format!("{entry}{suffix}");
            self.push_entry(&bundle, false);
            self.after_queue.push(format!("{bundle}/after"));
        }
    }
}

/// Builds the runtime search path from 'runtimepath' and 'packpath', following
/// runtime.c `runtime_search_path_build`: walk 'runtimepath' until the first
/// `after` entry, expanding each entry's wildcards and splicing in the start
/// bundles of any entry that is also a 'packpath' entry; then the start bundles
/// of the remaining 'packpath' entries; then every queued package `after`
/// directory; and finally the rest of 'runtimepath' from the entry the first
/// pass stopped on, in its original order.
fn build_search_path(session: &ApiSession, file_io: &dyn FileIO) -> Vec<SearchPathItem> {
    let runtimepath = global_option_string(session, "runtimepath");
    let packpath = global_option_string(session, "packpath");
    let entries: Vec<&str> = comma_entries(&runtimepath);
    let packs: Vec<&str> = comma_entries(&packpath);

    let mut builder = SearchPathBuilder {
        file_io,
        items: Vec::new(),
        used: HashSet::new(),
        after_queue: Vec::new(),
    };
    let mut used_packs: HashSet<&str> = HashSet::new();
    let mut tail = entries.len();
    for (index, entry) in entries.iter().enumerate() {
        if path_is_after(entry) {
            tail = index;
            break;
        }
        builder.push_entry(entry, false);
        if packs.contains(entry) {
            used_packs.insert(entry);
            builder.push_pack_entry(entry);
        }
    }
    for pack in packs.iter().filter(|pack| !used_packs.contains(*pack)) {
        builder.push_pack_entry(pack);
    }
    for entry in std::mem::take(&mut builder.after_queue) {
        builder.push_entry(&entry, true);
    }
    for entry in &entries[tail..] {
        builder.push_entry(entry, path_is_after(entry));
    }
    builder.items
}

fn global_option_string(session: &ApiSession, name: &str) -> String {
    session.with_editor(|editor| match editor.options().get_global(name) {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => String::new(),
    })
}

fn comma_entries(value: &str) -> Vec<&str> {
    value.split(',').filter(|entry| !entry.is_empty()).collect()
}

/// Returns the cached search path, rebuilding it when 'runtimepath' or
/// 'packpath' changed. Upstream keeps the same cache behind
/// `runtime_search_path_valid`, invalidated by `did_set_runtimepackpath`.
fn with_search_path<R>(
    session: &ApiSession,
    operation: impl FnOnce(&[SearchPathItem], &dyn FileIO) -> R,
) -> R {
    session.with_state_mut(|state| {
        let key = (
            global_option_string(session, "runtimepath"),
            global_option_string(session, "packpath"),
        );
        state.search_path.take_if(|(cached, _)| cached != &key);
        let file_io = state.file_io.as_ref();
        let (_, items) = state
            .search_path
            .get_or_insert_with(|| (key, build_search_path(session, file_io)));
        operation(items, file_io)
    })
}

/// runtime.c `do_in_cached_path` with `DIP_DIRFILE`: walk the search path in
/// order and expand each whitespace-separated pattern of `name` below every
/// entry. An empty `name` yields the search path itself, which is how upstream
/// implements `nvim_list_runtime_paths()`. Without `all`, the walk stops at the
/// first match, so the earliest 'runtimepath' entry wins.
pub(crate) fn find_runtime_files(session: &ApiSession, name: &str, all: bool) -> Vec<PathBuf> {
    with_search_path(session, |items, file_io| {
        let mut found = Vec::new();
        for item in items {
            if name.is_empty() {
                found.push(item.path.clone());
                if !all {
                    return found;
                }
                continue;
            }
            for pattern in name.split([' ', '\t']).filter(|part| !part.is_empty()) {
                let joined = item.path.join(pattern);
                for path in file_io.expand(&joined.to_string_lossy(), MatchKind::DirsAndFiles) {
                    found.push(path);
                    if !all {
                        return found;
                    }
                }
            }
        }
        found
    })
}

/// runtime.c `runtime_get_named`, the search behind `nvim__get_runtime()` and
/// therefore behind every Lua `require` of a module on 'runtimepath': each
/// pattern is probed as a literal readable file below each search-path entry.
/// With `is_lua`, entries without a `lua/` subdirectory are skipped.
#[must_use]
pub fn runtime_get_named(
    session: &ApiSession,
    patterns: &[String],
    all: bool,
    is_lua: bool,
) -> Vec<PathBuf> {
    with_search_path(session, |items, file_io| {
        let mut found = Vec::new();
        for item in items {
            if is_lua && !file_io.is_dir(&item.path.join("lua")) {
                continue;
            }
            for pattern in patterns {
                let candidate = item.path.join(pattern);
                if file_io.is_readable(&candidate) {
                    found.push(candidate);
                    if !all {
                        return found;
                    }
                }
            }
        }
        found
    })
}

#[derive(Clone, Debug)]
/// Metadata returned by `nvim_get_chan_info()` and `nvim_list_chans()`.
pub struct ChannelInfo {
    /// Numeric channel identifier.
    pub id: u64,
    /// Transport stream type, such as `stdio`, `stderr`, or `socket`.
    pub stream: OxStr,
    /// Channel mode, such as `rpc`, `bytes`, or `terminal`.
    pub mode: OxStr,
    /// Pseudoterminal name when the channel exposes one.
    pub pty: Option<OxStr>,
    /// Buffer attached to a terminal channel.
    pub buffer: Option<i64>,
    /// RPC client metadata advertised by the peer.
    pub client: Dict,
}

impl ChannelInfo {
    fn rpc(id: ChannelId, stream: &str) -> Self {
        Self {
            id: id.get(),
            stream: OxStr::from(stream),
            mode: OxStr::from("rpc"),
            pty: None,
            buffer: None,
            client: Dict(Vec::new()),
        }
    }

    /// Returns the built-in stdio RPC channel.
    #[must_use]
    pub fn stdio_rpc() -> Self {
        Self::rpc(CHAN_STDIO, "stdio")
    }

    /// Returns the built-in byte-oriented stderr channel.
    #[must_use]
    pub fn stderr_bytes() -> Self {
        Self {
            id: CHAN_STDERR.get(),
            stream: OxStr::from("stderr"),
            mode: OxStr::from("bytes"),
            pty: None,
            buffer: None,
            client: Dict(Vec::new()),
        }
    }

    /// Returns an RPC channel accepted from a socket listener.
    #[must_use]
    pub fn socket_rpc(id: ChannelId) -> Self {
        Self::rpc(id, "socket")
    }

    /// Returns a terminal channel attached to `buffer`.
    #[must_use]
    pub fn terminal(id: u64, buffer: i64) -> Self {
        Self {
            id,
            stream: OxStr::from("socket"),
            mode: OxStr::from("terminal"),
            pty: None,
            buffer: Some(buffer),
            client: Dict(Vec::new()),
        }
    }
}

/// Input callback and terminal-mode state for one `nvim_open_term()` channel.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TerminalInput {
    pub callback: Option<usize>,
    pub bracketed_paste: bool,
}

/// Publishes a dynamically allocated channel after its transport is ready.
///
/// # Errors
///
/// Returns an error if `info` uses a reserved ID or its ID is already registered.
pub fn register_channel(session: &ApiSession, info: ChannelInfo) -> Result<(), ApiError> {
    if info.id <= CHAN_STDERR.get() {
        return Err(ApiError::exception("reserved channel id"));
    }
    session.with_state_mut(|state| {
        if state.channels.contains_key(&info.id) {
            return Err(ApiError::exception("channel already registered"));
        }
        state.channels.insert(info.id, info);
        Ok(())
    })
}

/// Replaces the client metadata of an open RPC channel.
pub fn update_channel_client(
    session: &ApiSession,
    id: ChannelId,
    client: Dict,
) -> Result<(), ApiError> {
    session.with_state_mut(|state| {
        let info = state
            .channels
            .get_mut(&id.get())
            .ok_or_else(|| ApiError::validation("Invalid channel"))?;
        if info.mode != OxStr::from("rpc") {
            return Err(ApiError::validation("channel is not RPC"));
        }
        info.client = client;
        Ok(())
    })
}

/// Removes a dynamically allocated channel and its subscriptions.
///
/// # Errors
///
/// Returns an error if `id` belongs to a reserved channel.
pub fn close_channel(session: &ApiSession, id: ChannelId) -> Result<bool, ApiError> {
    if id.get() <= CHAN_STDERR.get() {
        return Err(ApiError::exception("cannot close reserved channel"));
    }
    session.with_state_mut(|state| {
        let removed = state.channels.remove(&id.get()).is_some();
        state.subscriptions.remove(&id.get());
        Ok(removed)
    })
}

/// Installs the byte sink for one editor's RPC channels.
pub fn set_channel_sink(session: &ApiSession, sink: Box<dyn ChannelSink>) {
    session.with_state_mut(|state| state.channel_sink = Some(sink));
}

/// Installs the live input-mode state used by `nvim_get_mode`.
pub fn set_mode_machine(session: &ApiSession, mode_machine: Rc<RefCell<ModeMachine>>) {
    session.with_state_mut(|state| state.mode_machine = Some(mode_machine));
}

pub(crate) fn mode_machine(session: &ApiSession) -> Option<Rc<RefCell<ModeMachine>>> {
    session.with_state(|state| state.mode_machine.clone())
}

/// Installs the byte sink for one editor's job/terminal channels.
///
/// `nvim_chan_send` uses this when the target channel is an editor-owned
/// terminal channel, so `chansend()` from Vimscript reaches the child.
pub fn set_job_sink(session: &ApiSession, sink: Box<dyn ChannelSink>) {
    session.with_state_mut(|state| state.job_sink = Some(sink));
}

/// Installs the executor pair for actions produced by autocmd firing plans:
/// index 0 serves the outermost firing, index 1 a reentrant one, and deeper
/// depths fork from the prototype (seeded from `primary`) on demand.
pub fn set_autocmd_executor(
    session: &ApiSession,
    primary: Box<dyn AutocmdExecutor>,
    nested: Box<dyn AutocmdExecutor>,
) {
    session.with_state_mut(|state| {
        if state.autocmd_prototype.is_none() {
            state.autocmd_prototype = primary.fork();
        }
        state.autocmd_pool.clear();
        state.autocmd_pool.push(Rc::new(RefCell::new(primary)));
        state.autocmd_pool.push(Rc::new(RefCell::new(nested)));
        state.autocmd_depth = 0;
    });
}

/// Installs the filesystem seam used by runtime-file discovery, discarding any
/// search path cached from the previous one.
pub fn set_file_io(session: &ApiSession, file_io: Box<dyn FileIO>) {
    session.with_state_mut(|state| {
        state.file_io = file_io;
        state.search_path = None;
    });
}

/// Installs the Ex-command host pair `nvim_exec2`, `nvim_cmd` and
/// `nvim_command` run through: index 0 serves the outermost call, index 1 a
/// reentrant one, and deeper depths fork from the prototype (seeded from
/// `primary`) on demand.
pub fn set_command_executor(
    session: &ApiSession,
    primary: Box<dyn crate::CommandExecutor>,
    nested: Box<dyn crate::CommandExecutor>,
) {
    session.with_state_mut(|state| {
        if state.command_prototype.is_none() {
            state.command_prototype = primary.fork();
        }
        state.command_pool.clear();
        state.command_pool.push(Rc::new(RefCell::new(primary)));
        state.command_pool.push(Rc::new(RefCell::new(nested)));
        state.command_depth = 0;
    });
}

/// Installs the Lua host pair `nvim_exec_lua` runs through: index 0 serves
/// the outermost call, index 1 a reentrant one, and deeper depths fork from
/// the prototype (seeded from `primary`) on demand.
pub fn set_lua_executor(
    session: &ApiSession,
    primary: Box<dyn LuaExecutor>,
    nested: Box<dyn LuaExecutor>,
) {
    session.with_state_mut(|state| {
        if state.lua_prototype.is_none() {
            state.lua_prototype = primary.fork();
        }
        state.lua_pool.clear();
        state.lua_pool.push(Rc::new(RefCell::new(primary)));
        state.lua_pool.push(Rc::new(RefCell::new(nested)));
        state.lua_depth = 0;
    });
}

/// Queues or performs one decoration-provider Lua callback release. Inside a
/// [`with_lua_executor`] frame every slot up to `depth` is mutably borrowed,
/// so the reference queues and is drained by the outer frame; at depth 0 it
/// releases against `lua_pool[0]` immediately. Queued releases are drained by
/// the next frame, so a freed slot's integer being recycled can never
/// collapse two releases into one.
pub(crate) fn release_lua_callback(session: &ApiSession, reference: usize) {
    let depth = session.with_state(|state| state.lua_depth);
    if depth > 0 {
        session.with_state_mut(|state| {
            state.pending_lua_callback_releases.push(reference);
        });
        return;
    }
    let slot = session.with_state(|state| state.lua_pool.first().cloned());
    match slot {
        Some(slot) => {
            let _ = slot.borrow_mut().free_callback(reference);
        }
        None => {
            session.with_state_mut(|state| {
                state.pending_lua_callback_releases.push(reference);
            });
        }
    }
}

/// Releases one pool depth index when a `with_*_executor` frame exits, on
/// both the normal path and a panic unwind. Declared before the host borrow
/// so the host `RefMut` drops first; the two guards touch different cells
/// either way.
struct DepthGuard<'a> {
    session: &'a ApiSession,
    decrement: fn(&mut crate::session::SessionState),
}

fn dec_command_depth(state: &mut crate::session::SessionState) {
    state.command_depth -= 1;
}

fn dec_lua_depth(state: &mut crate::session::SessionState) {
    state.lua_depth -= 1;
}

fn dec_autocmd_depth(state: &mut crate::session::SessionState) {
    state.autocmd_depth -= 1;
}

impl<'a> DepthGuard<'a> {
    fn command(session: &'a ApiSession) -> DepthGuard<'a> {
        DepthGuard {
            session,
            decrement: dec_command_depth,
        }
    }

    fn lua(session: &'a ApiSession) -> DepthGuard<'a> {
        DepthGuard {
            session,
            decrement: dec_lua_depth,
        }
    }

    fn autocmd(session: &'a ApiSession) -> DepthGuard<'a> {
        DepthGuard {
            session,
            decrement: dec_autocmd_depth,
        }
    }
}

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        // Safe during unwind: no `with_state*` borrow is live while
        // `operation` runs (shortest-scope rule), so the cell is free.
        self.session.with_state_mut(self.decrement);
    }
}

/// Runs `operation` with the installed command host, taking the pool slot at
/// the current reentry depth: index 0 for the outermost call, deeper indices
/// as frames nest. A pool exhausted past the primary/nested pair forks a
/// fresh stateless host from the never-checked-out prototype, so any depth of
/// reentrancy keeps a usable Ex-command host.
pub(crate) fn with_command_executor<R>(
    session: &ApiSession,
    operation: impl FnOnce(&ApiSession, &mut dyn crate::CommandExecutor) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    let depth = session.with_state(|state| state.command_depth);
    let need_grow = session.with_state(|state| depth >= state.command_pool.len());
    if need_grow {
        let forked = session.with_state_mut(|state| {
            state
                .command_prototype
                .as_mut()
                .and_then(|host| host.fork())
        });
        if let Some(forked) = forked {
            session.with_state_mut(|state| {
                if depth >= state.command_pool.len() {
                    state.command_pool.push(Rc::new(RefCell::new(forked)));
                }
            });
        }
    }

    session.with_state_mut(|state| state.command_depth += 1);
    let depth_guard = DepthGuard::command(session);

    let slot = session.with_state(|state| state.command_pool.get(depth).cloned());
    let Some(slot) = slot else {
        return Err(ApiError::exception("no Ex-command host is installed"));
    };

    // WHY no with_editor_mut here: the host runs user code; holding the
    // editor borrow across it would panic on any reentrant API call.
    // Hosts re-borrow the editor per statement through the session.
    let mut host = slot.borrow_mut();
    let result = operation(session, host.as_mut());
    drop(host);
    drop(depth_guard);
    result
}

/// Releases a callback immediately when no autocmd is executing. Reentrant
/// removals queue until the outermost action's host drains them, so sibling
/// and `++once` removals collapse into one ownership decision.
pub(crate) fn release_autocmd_callback(
    session: &ApiSession,
    reference: u64,
) -> Result<(), ApiError> {
    let depth = session.with_state(|state| state.autocmd_depth);
    if depth > 0 {
        session.with_state_mut(|state| {
            if !state.pending_autocmd_callback_releases.contains(&reference) {
                state.pending_autocmd_callback_releases.push(reference);
            }
        });
        return Ok(());
    }
    let slot = session.with_state(|state| state.autocmd_pool.first().cloned());
    let Some(slot) = slot else {
        session.with_state_mut(|state| {
            if !state.pending_autocmd_callback_releases.contains(&reference) {
                state.pending_autocmd_callback_releases.push(reference);
            }
        });
        return Ok(());
    };
    slot.borrow_mut()
        .release_callback(reference)
        .map_err(ApiError::exception)
}

pub(crate) fn take_pending_autocmd_callback_releases(session: &ApiSession) -> Vec<u64> {
    session.with_state_mut(|state| std::mem::take(&mut state.pending_autocmd_callback_releases))
}

/// Runs one planned autocmd action on the pool host at the current reentry
/// depth, releasing the state borrow before the host re-enters APIs through
/// `session`. A pool exhausted past the primary/nested pair forks a fresh
/// host from the never-checked-out prototype so the chain keeps a usable
/// executor instead of silently skipping the action; with no host installed
/// at all the action is skipped.
pub(crate) fn with_autocmd_executor(
    session: &ApiSession,
    action: &AutocmdAction,
) -> Result<AutocmdExecution, ApiError> {
    let depth = session.with_state(|state| state.autocmd_depth);
    let need_grow = session.with_state(|state| depth >= state.autocmd_pool.len());
    if need_grow {
        let forked = session.with_state_mut(|state| {
            state
                .autocmd_prototype
                .as_mut()
                .and_then(|host| host.fork())
        });
        if let Some(forked) = forked {
            session.with_state_mut(|state| {
                if depth >= state.autocmd_pool.len() {
                    state.autocmd_pool.push(Rc::new(RefCell::new(forked)));
                }
            });
        }
    }

    session.with_state_mut(|state| state.autocmd_depth += 1);
    let depth_guard = DepthGuard::autocmd(session);

    let Some(slot) = session.with_state(|state| state.autocmd_pool.get(depth).cloned()) else {
        return Ok(AutocmdExecution::Keep);
    };

    let mut host = slot.borrow_mut();
    let execution = host
        .execute_with_session(session, action)
        .map_err(ApiError::exception);
    drop(host);

    drop(depth_guard);
    execution
}

/// The `nvim_exec_lua` counterpart of [`with_command_executor`]: the frame
/// takes the pool slot at its reentry depth, and on the way out drains any
/// Lua callback releases that queued while it ran (including its own).
pub(crate) fn with_lua_executor<R>(
    session: &ApiSession,
    operation: impl FnOnce(&ApiSession, &mut dyn LuaExecutor) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    let depth = session.with_state(|state| state.lua_depth);
    let need_grow = session.with_state(|state| depth >= state.lua_pool.len());
    if need_grow {
        let forked = session
            .with_state_mut(|state| state.lua_prototype.as_mut().and_then(|host| host.fork()));
        if let Some(forked) = forked {
            session.with_state_mut(|state| {
                if depth >= state.lua_pool.len() {
                    state.lua_pool.push(Rc::new(RefCell::new(forked)));
                }
            });
        }
    }

    session.with_state_mut(|state| state.lua_depth += 1);
    let depth_guard = DepthGuard::lua(session);

    let Some(slot) = session.with_state(|state| state.lua_pool.get(depth).cloned()) else {
        return Err(ApiError::exception("no Lua host is installed"));
    };

    // Same borrow rule as with_command_executor: user code runs without an
    // outstanding editor borrow.
    let mut host = slot.borrow_mut();
    let result = operation(session, host.as_mut());
    drop(host);

    // Drain queued callback releases (from nested release_lua_callback calls
    // and, at depth 0, releases that raced the pool install).
    let pending =
        session.with_state_mut(|state| std::mem::take(&mut state.pending_lua_callback_releases));
    let mut release_error = None;
    if !pending.is_empty() {
        let mut host = slot.borrow_mut();
        for reference in pending {
            if let Err(error) = host.free_callback(reference)
                && release_error.is_none()
            {
                release_error = Some(ApiError::exception(error));
            }
        }
    }

    drop(depth_guard);

    let outcome = result?;
    if let Some(error) = release_error {
        return Err(error);
    }
    Ok(outcome)
}
