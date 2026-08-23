//! Host-layer state shared by API families that cannot live in `ox-editor`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use ox_editor::{AutocmdAction, Editor, OptionValue};
use ox_types::{Dict, OxStr};
use ox_ui::{ChromeState, HlState, UiChannels};

/// Byte sink used by `nvim_chan_send`.
pub trait ChannelSink {
    /// Writes bytes to a channel.
    fn send(&mut self, channel: u64, bytes: &[u8]) -> Result<(), String>;
}

/// Host executor for actions produced by the editor's autocmd planner.
pub trait AutocmdExecutor {
    /// Executes one planned definition.
    fn execute(&mut self, action: &AutocmdAction) -> Result<(), String>;
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
        let mut heads = vec![if pattern.starts_with('/') { PathBuf::from("/") } else { PathBuf::new() }];
        for component in pattern.split('/').filter(|part| !part.is_empty()) {
            if !component.as_bytes().iter().any(|byte| matches!(byte, b'*' | b'?')) {
                for head in &mut heads {
                    head.push(component);
                }
                continue;
            }
            heads = heads.iter().flat_map(|head| expand_component(head, component)).collect();
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
    let listed = if directory.as_os_str().is_empty() { Path::new(".") } else { directory };
    let Ok(entries) = std::fs::read_dir(listed) else { return Vec::new() };
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
    bytes.ends_with(b"after") && (bytes.len() == 5 || (bytes.len() > 5 && bytes[bytes.len() - 6] == b'/'))
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
fn build_search_path(editor: &Editor, file_io: &dyn FileIO) -> Vec<SearchPathItem> {
    let runtimepath = global_option_string(editor, "runtimepath");
    let packpath = global_option_string(editor, "packpath");
    let entries: Vec<&str> = comma_entries(&runtimepath);
    let packs: Vec<&str> = comma_entries(&packpath);

    let mut builder =
        SearchPathBuilder { file_io, items: Vec::new(), used: HashSet::new(), after_queue: Vec::new() };
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

fn global_option_string(editor: &Editor, name: &str) -> String {
    match editor.options().get_global(name) {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => String::new(),
    }
}

fn comma_entries(value: &str) -> Vec<&str> {
    value.split(',').filter(|entry| !entry.is_empty()).collect()
}

/// Returns the cached search path, rebuilding it when 'runtimepath' or
/// 'packpath' changed. Upstream keeps the same cache behind
/// `runtime_search_path_valid`, invalidated by `did_set_runtimepackpath`.
fn with_search_path<R>(editor: &Editor, operation: impl FnOnce(&[SearchPathItem], &dyn FileIO) -> R) -> R {
    with_state_mut(editor, |state| {
        let key = (
            global_option_string(editor, "runtimepath"),
            global_option_string(editor, "packpath"),
        );
        if state.search_path.as_ref().is_none_or(|(cached, _)| cached != &key) {
            let built = build_search_path(editor, state.file_io.as_ref());
            state.search_path = Some((key, built));
        }
        let (_, items) = state.search_path.as_ref().expect("search path was just populated");
        operation(items, state.file_io.as_ref())
    })
}

/// runtime.c `do_in_cached_path` with `DIP_DIRFILE`: walk the search path in
/// order and expand each whitespace-separated pattern of `name` below every
/// entry. An empty `name` yields the search path itself, which is how upstream
/// implements `nvim_list_runtime_paths()`. Without `all`, the walk stops at the
/// first match, so the earliest 'runtimepath' entry wins.
pub(crate) fn find_runtime_files(editor: &Editor, name: &str, all: bool) -> Vec<PathBuf> {
    with_search_path(editor, |items, file_io| {
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
pub fn runtime_get_named(editor: &Editor, patterns: &[String], all: bool, is_lua: bool) -> Vec<PathBuf> {
    with_search_path(editor, |items, file_io| {
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
pub(crate) struct ChannelInfo {
    pub id: u64,
    pub stream: OxStr,
    pub mode: OxStr,
    pub pty: Option<OxStr>,
    pub buffer: Option<i64>,
    pub client: Dict,
}

pub(crate) struct RuntimeState {
    pub namespaces: BTreeMap<OxStr, u32>,
    pub next_namespace: u32,
    pub next_channel: u64,
    pub ui_channels: UiChannels,
    pub chrome: ChromeState,
    /// Active highlight definitions used for rendering (the namespace selected
    /// by `nvim_set_hl_ns()`/`nvim_set_hl_ns_fast()`, global ns 0 by default).
    pub highlights: HlState,
    /// Per-namespace highlight tables keyed by `ns_id`; ns 0 is global.
    pub hl_namespaces: BTreeMap<i64, HlState>,
    pub current_hl_ns: i64,
    pub fast_hl_ns: i64,
    pub channels: BTreeMap<u64, ChannelInfo>,
    pub subscriptions: BTreeMap<u64, BTreeSet<OxStr>>,
    pub channel_sink: Option<Box<dyn ChannelSink>>,
    pub autocmd_executor: Option<Box<dyn AutocmdExecutor>>,
    pub file_io: Box<dyn FileIO>,
    /// The expanded runtime search path, keyed by the ('runtimepath',
    /// 'packpath') pair it was built from.
    pub search_path: Option<((String, String), Vec<SearchPathItem>)>,
    pub saved_context: Option<Dict>,
    pub paste_phase: i64,
}

impl Default for RuntimeState {
    fn default() -> Self {
        let mut channels = BTreeMap::new();
        channels.insert(1, ChannelInfo {
            id: 1,
            stream: OxStr::from("stdio"),
            mode: OxStr::from("rpc"),
            pty: None,
            buffer: None,
            client: Dict(Vec::new()),
        });
        Self {
            namespaces: BTreeMap::new(),
            next_namespace: 1,
            next_channel: 3,
            ui_channels: UiChannels::new(),
            chrome: ChromeState::new(),
            highlights: HlState::new(),
            hl_namespaces: BTreeMap::from([(0, HlState::new())]),
            current_hl_ns: 0,
            fast_hl_ns: 0,
            channels,
            subscriptions: BTreeMap::new(),
            channel_sink: None,
            autocmd_executor: None,
            file_io: Box::new(StdFileIO),
            search_path: None,
            saved_context: None,
            paste_phase: -1,
        }
    }
}

thread_local! {
    static STATES: RefCell<HashMap<u64, RuntimeState>> = RefCell::new(HashMap::new());
}

pub(crate) fn with_state<R>(editor: &Editor, operation: impl FnOnce(&RuntimeState) -> R) -> R {
    STATES.with(|states| {
        let mut states = states.borrow_mut();
        operation(states.entry(editor.api_instance_id()).or_default())
    })
}

pub(crate) fn with_state_mut<R>(editor: &Editor, operation: impl FnOnce(&mut RuntimeState) -> R) -> R {
    STATES.with(|states| {
        let mut states = states.borrow_mut();
        operation(states.entry(editor.api_instance_id()).or_default())
    })
}

/// Installs the byte sink for one editor's RPC channels.
pub fn set_channel_sink(editor: &Editor, sink: Box<dyn ChannelSink>) {
    with_state_mut(editor, |state| state.channel_sink = Some(sink));
}

/// Installs the executor for actions produced by autocmd firing plans.
pub fn set_autocmd_executor(editor: &Editor, executor: Box<dyn AutocmdExecutor>) {
    with_state_mut(editor, |state| state.autocmd_executor = Some(executor));
}

/// Installs the filesystem seam used by runtime-file discovery, discarding any
/// search path cached from the previous one.
pub fn set_file_io(editor: &Editor, file_io: Box<dyn FileIO>) {
    with_state_mut(editor, |state| {
        state.file_io = file_io;
        state.search_path = None;
    });
}
