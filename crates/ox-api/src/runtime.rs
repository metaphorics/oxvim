//! Host-layer state shared by API families that cannot live in `ox-editor`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use ox_editor::{AutocmdAction, Editor};
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

/// Filesystem seam used by runtime-file discovery.
pub trait FileIO {
    /// Returns paths below `root` matching `pattern`.
    fn glob(&self, root: &Path, pattern: &str) -> Result<Vec<PathBuf>, String>;
}

/// Standard filesystem implementation for runtime lookup.
#[derive(Default)]
pub struct StdFileIO;

impl FileIO for StdFileIO {
    fn glob(&self, root: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
        let mut matches = Vec::new();
        visit(root, root, pattern.as_bytes(), &mut matches)?;
        matches.sort();
        Ok(matches)
    }
}

fn visit(root: &Path, directory: &Path, pattern: &[u8], output: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(directory).map_err(|error| error.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            visit(root, &path, pattern, output)?;
        } else if let Ok(relative) = path.strip_prefix(root)
            && wildcard(pattern, relative.to_string_lossy().as_bytes())
        {
            output.push(path);
        }
    }
    Ok(())
}

fn wildcard(pattern: &[u8], text: &[u8]) -> bool {
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
    pub ui_channels: UiChannels,
    pub chrome: ChromeState,
    pub highlights: HlState,
    pub current_hl_ns: i64,
    pub fast_hl_ns: i64,
    pub channels: BTreeMap<u64, ChannelInfo>,
    pub subscriptions: BTreeMap<u64, BTreeSet<OxStr>>,
    pub channel_sink: Option<Box<dyn ChannelSink>>,
    pub autocmd_executor: Option<Box<dyn AutocmdExecutor>>,
    pub file_io: Box<dyn FileIO>,
    pub runtime_paths: Vec<PathBuf>,
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
        let runtime_paths = std::env::var_os("OXVIM_REF_ROOT")
            .map(PathBuf::from)
            .map(|root| vec![root.join("runtime")])
            .unwrap_or_default();
        Self {
            namespaces: BTreeMap::new(),
            next_namespace: 1,
            ui_channels: UiChannels::new(),
            chrome: ChromeState::new(),
            highlights: HlState::new(),
            current_hl_ns: 0,
            fast_hl_ns: 0,
            channels,
            subscriptions: BTreeMap::new(),
            channel_sink: None,
            autocmd_executor: None,
            file_io: Box::new(StdFileIO),
            runtime_paths,
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

/// Installs runtime roots and filesystem lookup for one editor.
pub fn set_runtime_files(editor: &Editor, roots: Vec<PathBuf>, file_io: Box<dyn FileIO>) {
    with_state_mut(editor, |state| {
        state.runtime_paths = roots;
        state.file_io = file_io;
    });
}
