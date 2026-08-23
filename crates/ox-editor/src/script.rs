//! Script sourcing state: SIDs, `s:` scopes, `<SNR>` expansion, runtime
//! roots, load-once autoload registry, and comment-aware line continuation.
//!
//! Semantics mirror `do_source`, `getline_equal`, and the continuation
//! handling inside `do_cmdline` (`src/nvim/ex_docmd.c:717-1050,
//! 1330-1500`), plus the autoload name-to-path rule in
//! `src/nvim/runtime.c:144-167`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ox_eval::scope::ScopeMap;
use ox_eval::Scope;
use ox_excmd::Parser as ExParser;

/// Stable identifier assigned to one sourcing event.
pub type Sid = u64;

/// Upstream's `sctx_T`: the script context in force when something was
/// defined. A definition records it so later queries — `maparg()`'s `sid` and
/// `lnum`, the `:function` reload rule — can report where it came from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceContext {
    /// Defining script id (`sc_sid`), zero at the command line.
    pub sid: Sid,
    /// Sourcing sequence in force (`sc_seq`). Only a *different* sequence
    /// under the same `sid` is a reload.
    pub seq: u64,
    /// Physical line within the defining script (`sc_lnum`). Zero for a whole
    /// script, whose executing line is tracked separately.
    pub lnum: usize,
}

/// Maximum length of one logical line after continuation joining.
const MAX_LOGICAL_LINE: usize = 1_048_576;

/// Filesystem object kind exposed through [`FileIO`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileKind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Any other filesystem object.
    Other,
}

/// Metadata needed by filesystem Vimscript builtins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileMetadata {
    /// Object kind.
    pub kind: FileKind,
    /// Byte length.
    pub len: u64,
    /// Modification time, when available.
    pub modified: Option<SystemTime>,
    /// Unix permission bits, or zero on platforms without them.
    pub mode: u32,
}

/// One directory entry returned by [`FileIO::read_dir`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    /// Full entry path.
    pub path: PathBuf,
    /// Entry name.
    pub name: std::ffi::OsString,
}

fn unsupported(operation: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, operation)
}

/// Filesystem seam for sourcing, editing, and filesystem builtins.
pub trait FileIO {
    /// Reads a complete file as text. Invalid UTF-8 loses bytes to the
    /// replacement character, matching the editor's byte-tolerant default.
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// Reads a complete file without decoding it.
    fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.read_to_string(path).map(String::into_bytes)
    }
    /// Writes a complete file.
    fn write_string(&self, path: &Path, contents: &str) -> io::Result<()>;
    /// Writes bytes, optionally appending to an existing file.
    fn write_bytes(&self, path: &Path, contents: &[u8], append: bool) -> io::Result<()> {
        if append {
            return Err(unsupported("append is not supported by this FileIO"));
        }
        let contents = std::str::from_utf8(contents)
            .map_err(|_| unsupported("binary writes are not supported by this FileIO"))?;
        self.write_string(path, contents)
    }
    /// Whether the path names a readable regular file.
    fn exists(&self, path: &Path) -> bool;
    /// Returns metadata, following links when requested.
    fn metadata(&self, _path: &Path, _follow_links: bool) -> io::Result<FileMetadata> {
        Err(unsupported("metadata is not supported by this FileIO"))
    }
    /// Lists a directory.
    fn read_dir(&self, _path: &Path) -> io::Result<Vec<FileEntry>> {
        Err(unsupported("directory enumeration is not supported by this FileIO"))
    }
    /// Creates one directory or a complete parent chain.
    fn create_dir(&self, _path: &Path, _recursive: bool, _mode: u32) -> io::Result<()> {
        Err(unsupported("directory creation is not supported by this FileIO"))
    }
    /// Removes a file or symbolic link.
    fn remove_file(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("file removal is not supported by this FileIO"))
    }
    /// Removes an empty directory.
    fn remove_dir(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("directory removal is not supported by this FileIO"))
    }
    /// Removes a directory tree.
    fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("recursive removal is not supported by this FileIO"))
    }
    /// Renames a filesystem object.
    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(unsupported("rename is not supported by this FileIO"))
    }
    /// Canonical form used for the source-once registry. Implementations may
    /// fall back to the input path when canonicalization fails.
    fn canonicalize(&self, path: &Path) -> PathBuf;
    /// Replaces Unix permission bits (or readonly state on non-Unix hosts).
    fn set_permissions(&self, _path: &Path, _mode: u32) -> io::Result<()> {
        Err(unsupported("permission mutation is not supported by this FileIO"))
    }
}

/// Filesystem-backed [`FileIO`].
#[derive(Clone, Copy, Debug, Default)]
pub struct RealFileIO;

impl FileIO for RealFileIO {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let bytes = fs::read(path)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn write_string(&self, path: &Path, contents: &str) -> io::Result<()> {
        fs::write(path, contents)
    }

    fn write_bytes(&self, path: &Path, contents: &[u8], append: bool) -> io::Result<()> {
        use std::io::Write as _;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true);
        if append { options.append(true); } else { options.truncate(true); }
        options.open(path)?.write_all(contents)
    }

    fn exists(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn metadata(&self, path: &Path, follow_links: bool) -> io::Result<FileMetadata> {
        let metadata = if follow_links { fs::metadata(path)? } else { fs::symlink_metadata(path)? };
        let file_type = metadata.file_type();
        let kind = if file_type.is_file() { FileKind::File }
            else if file_type.is_dir() { FileKind::Directory }
            else if file_type.is_symlink() { FileKind::Symlink }
            else { FileKind::Other };
        #[cfg(unix)]
        let mode = { use std::os::unix::fs::PermissionsExt as _; metadata.permissions().mode() };
        #[cfg(not(unix))]
        let mode = if metadata.permissions().readonly() { 0o444 } else { 0o666 };
        Ok(FileMetadata { kind, len: metadata.len(), modified: metadata.modified().ok(), mode })
    }

    fn set_permissions(&self, path: &Path, mode: u32) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
        }
        #[cfg(not(unix))]
        {
            let mut permissions = fs::metadata(path)?.permissions();
            permissions.set_readonly(mode & 0o222 == 0);
            fs::set_permissions(path, permissions)
        }
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<FileEntry>> {
        fs::read_dir(path)?.map(|entry| entry.map(|entry| FileEntry { path: entry.path(), name: entry.file_name() })).collect()
    }

    fn create_dir(&self, path: &Path, recursive: bool, mode: u32) -> io::Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(recursive);
        #[cfg(unix)]
        { use std::os::unix::fs::DirBuilderExt as _; builder.mode(mode); }
        builder.create(path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> { fs::remove_file(path) }
    fn remove_dir(&self, path: &Path) -> io::Result<()> { fs::remove_dir(path) }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> { fs::remove_dir_all(path) }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> { fs::rename(from, to) }

    fn canonicalize(&self, path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
}

/// One raw line joined with its `\` continuations, with its first physical
/// line retained for throwpoints and error text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalLine {
    /// Joined command text; leading whitespace of continuations removed.
    pub text: String,
    /// One-based physical line where this logical line started.
    pub first_line: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeredocKind {
    Script,
    Let,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HeredocSpec<'a> {
    kind: HeredocKind,
    marker: String,
    trim: bool,
    command_indent: &'a str,
}

fn heredoc_spec(line: &str) -> Result<Option<HeredocSpec<'_>>, (&'static str, String)> {
    let Ok(commands) = ExParser::new().parse(line) else {
        return Ok(None);
    };
    let [command] = commands.as_slice() else {
        return Ok(None);
    };
    let name = command.command.name();
    let args = command.args.trim_start_matches([' ', '\t']);
    let (kind, modifiers) = if name == "lua" {
        let Some(modifiers) = args.strip_prefix("<<") else {
            return Ok(None);
        };
        (HeredocKind::Script, modifiers)
    } else if matches!(name, "let" | "const") {
        let Some(assignment) = args.find('=') else {
            return Ok(None);
        };
        let Some(modifiers) = args[assignment + 1..].strip_prefix("<<") else {
            return Ok(None);
        };
        (HeredocKind::Let, modifiers)
    } else {
        return Ok(None);
    };

    let mut words = modifiers.trim_start_matches([' ', '\t']);
    let mut trim = false;
    loop {
        let modifier_end = words
            .find(|character: char| character.is_ascii_whitespace())
            .unwrap_or(words.len());
        match &words[..modifier_end] {
            "trim" => trim = true,
            "eval" => {},
            _ => break,
        }
        words = words[modifier_end..].trim_start_matches([' ', '\t']);
    }
    let marker_end = words
        .find(|character: char| character.is_ascii_whitespace())
        .unwrap_or(words.len());
    let marker = &words[..marker_end];
    let trailing = words[marker_end..].trim_start_matches([' ', '\t']);
    if !marker.starts_with('\"') && !trailing.is_empty() && !trailing.starts_with('\"') {
        return Err(("E488", "Trailing characters".to_owned()));
    }
    let marker = if marker.is_empty() || marker.starts_with('\"') {
        if kind == HeredocKind::Script {
            "."
        } else {
            return Err(("E172", "Missing marker".to_owned()));
        }
    } else {
        marker
    };
    if kind == HeredocKind::Let && marker.as_bytes()[0].is_ascii_lowercase() {
        return Err(("E221", "Marker cannot start with lower case letter".to_owned()));
    }
    let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
    Ok(Some(HeredocSpec {
        kind,
        marker: marker.to_owned(),
        trim,
        command_indent: &line[..indent_len],
    }))
}

/// Error raised while joining continuation lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptError {
    /// Stable error code prefix.
    pub code: &'static str,
    /// Human-readable detail.
    pub message: String,
    /// One-based physical line responsible, when known.
    pub line: Option<usize>,
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}: {} (line {line})", self.code, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for ScriptError {}

/// One active `:source` frame: the script currently being executed.
#[derive(Clone, Debug)]
pub struct SourceFrame {
    /// SID of the script being sourced. Re-sourcing the same file reuses
    /// its SID (`runtime.c` `find_script_by_name`), so `s:` variables and
    /// `<SNR>` names survive.
    pub sid: Sid,
    /// Sequence number of *this* sourcing event, fresh every time
    /// (`current_sctx.sc_seq = ++last_current_SID_seq`, `runtime.c:2333`).
    /// Together with `sid` it is what lets a script redefine its own
    /// functions and commands on a reload without `!`.
    pub seq: u64,
    /// Display name used in throwpoints (`/abs/path.vim` or `<cmdline>`).
    pub name: String,
    /// One-based physical line currently executing.
    pub current_line: usize,
}

/// Registry entry for one sourced script.
#[derive(Clone, Debug)]
pub struct ScriptInfo {
    /// Display name of the script.
    pub name: String,
    /// `s:` variables owned by this SID.
    pub vars: ScopeMap,
}
/// One runtime-search root used by autoload resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeRoot {
    path: PathBuf,
}

impl RuntimeRoot {
    /// Creates a runtime root.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Root directory containing `autoload/`.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl From<PathBuf> for RuntimeRoot {
    fn from(path: PathBuf) -> Self {
        Self::new(path)
    }
}

impl From<&Path> for RuntimeRoot {
    fn from(path: &Path) -> Self {
        Self::new(path.to_path_buf())
    }
}

/// Builds the startup default for `'runtimepath'` (and its `'packpath'`
/// copy), per option.c `set_init_default_*` calling runtime.c
/// `runtimepath_default`: XDG config entries, XDG data `site` entries,
/// the runtime tree, then the mirrored `after` entries in reverse.
/// `clean` mirrors `--clean`: user and system XDG entries are dropped so
/// only the runtime tree remains. `vimruntime` is this build's runtime
/// tree, upstream's `$VIMRUNTIME`.
#[must_use]
pub fn default_runtimepath(clean: bool, vimruntime: &Path) -> String {
    let (config_home, config_dirs, data_home, data_dirs) = if clean {
        (None, Vec::new(), None, Vec::new())
    } else {
        (
            xdg_home_dir("XDG_CONFIG_HOME", "~/.config"),
            xdg_dir_list("XDG_CONFIG_DIRS", "/etc/xdg"),
            xdg_home_dir("XDG_DATA_HOME", "~/.local/share"),
            xdg_dir_list("XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
        )
    };
    build_runtimepath(config_home.as_deref(), &config_dirs, data_home.as_deref(), &data_dirs, vimruntime)
}

/// Assembles the 'runtimepath' entry list from resolved XDG pieces and
/// the runtime tree, in `runtimepath_default` order. Crate-visible for
/// deterministic tests of the entry layout.
pub(crate) fn build_runtimepath(
    config_home: Option<&str>,
    config_dirs: &[String],
    data_home: Option<&str>,
    data_dirs: &[String],
    vimruntime: &Path,
) -> String {
    const APPNAME: &str = "nvim";
    fn joined(base: &str, suffix: &str) -> String {
        format!("{}/{}", base.trim_end_matches('/'), suffix)
    }
    let mut entries = Vec::new();
    if let Some(home) = config_home {
        entries.push(joined(home, APPNAME));
    }
    for dir in config_dirs {
        entries.push(joined(dir, APPNAME));
    }
    if let Some(home) = data_home {
        entries.push(joined(home, &format!("{APPNAME}/site")));
    }
    for dir in data_dirs {
        entries.push(joined(dir, &format!("{APPNAME}/site")));
    }
    entries.push(vimruntime.to_string_lossy().trim_end_matches('/').to_owned());
    for dir in data_dirs.iter().rev() {
        entries.push(joined(dir, &format!("{APPNAME}/site/after")));
    }
    if let Some(home) = data_home {
        entries.push(joined(home, &format!("{APPNAME}/site/after")));
    }
    for dir in config_dirs.iter().rev() {
        entries.push(joined(dir, &format!("{APPNAME}/after")));
    }
    if let Some(home) = config_home {
        entries.push(joined(home, &format!("{APPNAME}/after")));
    }
    entries.join(",")
}

/// Resolves one single-directory XDG variable, falling back to the
/// upstream default with `~` expanded through `$HOME` (stdpaths.c
/// `stdpaths_get_xdg_var` + `expand_env_save`). An unset-but-present
/// empty variable contributes nothing, like upstream.
fn xdg_home_dir(env: &str, fallback: &str) -> Option<String> {
    match std::env::var_os(env) {
        Some(value) => {
            let text = value.to_string_lossy().into_owned();
            (!text.is_empty()).then_some(text)
        }
        None => Some(expand_home(fallback)),
    }
}

/// Resolves one colon-separated XDG list, dropping empty entries.
fn xdg_dir_list(env: &str, fallback: &str) -> Vec<String> {
    let raw = std::env::var_os(env)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| fallback.to_owned());
    raw.split(':')
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Expands a leading `~/` through `$HOME`, leaving other paths untouched.
fn expand_home(path: &str) -> String {
    path.strip_prefix("~/").map_or_else(
        || path.to_owned(),
        |rest| {
            std::env::var_os("HOME").map_or_else(
                || path.to_owned(),
                |home| Path::new(&home).join(rest).to_string_lossy().into_owned(),
            )
        },
    )
}

/// One `stdpath()` selector, `f_stdpath`'s `what` argument
/// (`eval/funcs.c:7021-7039`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StdPath {
    /// `$XDG_CACHE_HOME/nvim`.
    Cache,
    /// `$XDG_CONFIG_HOME/nvim`.
    Config,
    /// Each `$XDG_CONFIG_DIRS` entry with `/nvim` appended.
    ConfigDirs,
    /// `$XDG_DATA_HOME/nvim`.
    Data,
    /// Each `$XDG_DATA_DIRS` entry with `/nvim` appended.
    DataDirs,
    /// `$XDG_STATE_HOME/nvim/logs`.
    Log,
    /// `$XDG_RUNTIME_DIR`, with no application component.
    Run,
    /// `$XDG_STATE_HOME/nvim`.
    State,
}

impl StdPath {
    /// Parses the `what` argument, or `None` for a name upstream rejects with
    /// `E6100` (`eval/funcs.c:7038`).
    #[must_use]
    pub fn parse(what: &str) -> Option<Self> {
        Some(match what {
            "cache" => Self::Cache,
            "config" => Self::Config,
            "config_dirs" => Self::ConfigDirs,
            "data" => Self::Data,
            "data_dirs" => Self::DataDirs,
            "log" => Self::Log,
            "run" => Self::Run,
            "state" => Self::State,
            _ => return None,
        })
    }

    /// Whether this selector answers a list rather than a single directory.
    #[must_use]
    pub const fn is_list(self) -> bool {
        matches!(self, Self::ConfigDirs | Self::DataDirs)
    }
}

/// Resolves one `stdpath()` selector, `f_stdpath` (`eval/funcs.c:7011-7040`)
/// through `get_xdg_home` and `stdpaths_get_xdg_var` (`os/stdpaths.c:151-225`).
///
/// `get_xdg_home` appends `$NVIM_APPNAME`, defaulting to `nvim`
/// (`os/stdpaths.c:70-87,222`), to every selector but `run`, which is the raw
/// `$XDG_RUNTIME_DIR`, and `log`, which is the state home plus `logs`
/// (`eval/funcs.c:7030-7032`). A list selector answers one entry per
/// directory. This is the same resolver `'runtimepath'` is built from, so a
/// plugin's `stdpath('config')` and the rtp entry it expects to find itself on
/// cannot disagree.
#[must_use]
pub fn stdpath(what: StdPath) -> Vec<String> {
    const APPNAME: &str = "nvim";
    fn under(base: Option<String>, suffix: &str) -> Vec<String> {
        base.into_iter()
            .map(|dir| format!("{}/{suffix}", dir.trim_end_matches('/')))
            .collect()
    }
    match what {
        StdPath::Cache => under(xdg_home_dir("XDG_CACHE_HOME", "~/.cache"), APPNAME),
        StdPath::Config => under(xdg_home_dir("XDG_CONFIG_HOME", "~/.config"), APPNAME),
        StdPath::Data => under(xdg_home_dir("XDG_DATA_HOME", "~/.local/share"), APPNAME),
        StdPath::State => under(xdg_home_dir("XDG_STATE_HOME", "~/.local/state"), APPNAME),
        StdPath::Log => under(
            xdg_home_dir("XDG_STATE_HOME", "~/.local/state"),
            &format!("{APPNAME}/logs"),
        ),
        // `stdpaths_get_xdg_var` has no fallback for the runtime dir and no
        // `get_xdg_home` wrapper, so this is the variable verbatim
        // (`os/stdpaths.c:59,182-190`; `eval/funcs.c:7032`). Unset falls back
        // to the temporary directory upstream decides at startup.
        StdPath::Run => vec![
            xdg_home_dir("XDG_RUNTIME_DIR", "")
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| {
                    std::env::temp_dir().to_string_lossy().trim_end_matches('/').to_owned()
                }),
        ],
        StdPath::ConfigDirs => xdg_dir_list("XDG_CONFIG_DIRS", "/etc/xdg")
            .into_iter()
            .map(|dir| format!("{}/{APPNAME}", dir.trim_end_matches('/')))
            .collect(),
        StdPath::DataDirs => xdg_dir_list("XDG_DATA_DIRS", "/usr/local/share:/usr/share")
            .into_iter()
            .map(|dir| format!("{}/{APPNAME}", dir.trim_end_matches('/')))
            .collect(),
    }
}

/// Sourcing state owned by the Ex executor.
#[derive(Debug)]
pub struct ScriptCtx<F: FileIO = RealFileIO> {
    io: F,
    next_sid: Sid,
    next_seq: u64,
    scripts: BTreeMap<Sid, ScriptInfo>,
    source_stack: Vec<SourceFrame>,
    sourced_once: BTreeSet<PathBuf>,
    runtime_roots: Vec<RuntimeRoot>,
}

impl<F: FileIO> ScriptCtx<F> {
    /// Creates an empty context reading through `io`.
    #[must_use]
    pub fn new(io: F) -> Self {
        Self {
            io,
            next_sid: 1,
            next_seq: 0,
            scripts: BTreeMap::new(),
            source_stack: Vec::new(),
            sourced_once: BTreeSet::new(),
            runtime_roots: Vec::new(),
        }
    }

    /// Borrows the file-IO seam.
    #[must_use]
    pub fn io(&self) -> &F {
        &self.io
    }

    /// Runtime roots searched for autoload files, in priority order.
    #[must_use]
    pub fn runtime_roots(&self) -> &[RuntimeRoot] {
        &self.runtime_roots
    }

    /// Adds a runtime root searched before later roots.
    pub fn add_runtime_root(&mut self, root: impl Into<RuntimeRoot>) {
        self.runtime_roots.push(root.into());
    }

    /// Replaces every runtime search root with the comma-separated entries
    /// of `'runtimepath'`, in order (runtime.c `do_in_runtimepath` walks
    /// `p_rtp` entries left to right). Empty entries are skipped.
    pub fn set_runtime_roots_from_rtp(&mut self, rtp: &str) {
        self.runtime_roots = crate::options::CommaItems::new(rtp)
            .filter(|entry| !entry.is_empty())
            .map(|entry| RuntimeRoot::new(PathBuf::from(entry)))
            .collect();
    }

    /// Allocates a fresh SID for one sourcing event.
    pub fn allocate_sid(&mut self, name: &str) -> Sid {
        let sid = self.next_sid;
        self.next_sid = self.next_sid.saturating_add(1);
        self.scripts.insert(
            sid,
            ScriptInfo {
                name: name.to_owned(),
                vars: ScopeMap::new(),
            },
        );
        sid
    }

    /// Pushes a source frame, returning the SID whose `s:` scope the frame
    /// runs in.
    ///
    /// `do_source` looks the file up with `find_script_by_name` and reuses
    /// the SID it already has (`runtime.c:2226,2335`), so a script sourced
    /// twice keeps its `s:` variables and its `<SNR>` number; only the
    /// sequence number is new. That is what makes a guard like setup.vim's
    /// `if exists('s:did_load') | finish | endif` work on the second
    /// sourcing. Named contexts that are not files — `<command line>` and
    /// friends — are not looked up, matching `do_source_str`, which never
    /// consults the registry.
    pub fn push_source(&mut self, name: String) -> Sid {
        let sid = self
            .reusable_sid(&name)
            .unwrap_or_else(|| self.allocate_sid(&name));
        self.next_seq = self.next_seq.saturating_add(1);
        self.source_stack.push(SourceFrame {
            sid,
            seq: self.next_seq,
            name,
            current_line: 0,
        });
        sid
    }

    /// The SID a previous sourcing of `name` already owns, when `name` is a
    /// file rather than an anonymous context.
    fn reusable_sid(&self, name: &str) -> Option<Sid> {
        if name.starts_with('<') {
            return None;
        }
        self.scripts
            .iter()
            .rev()
            .find(|(_, info)| info.name == name)
            .map(|(sid, _)| *sid)
    }

    /// Pops the current source frame, returning the SID of the caller when
    /// one remains.
    pub fn pop_source(&mut self) -> Option<Sid> {
        self.source_stack.pop();
        self.source_stack.last().map(|frame| frame.sid)
    }

    /// The SID whose `s:` scope is currently visible.
    #[must_use]
    pub fn current_sid(&self) -> Option<Sid> {
        self.source_stack.last().map(|frame| frame.sid)
    }

    /// The sequence number of the sourcing event currently running, or `0`
    /// outside any script (upstream's `current_sctx.sc_seq`, which starts at
    /// zero and is only ever bumped by `do_source`).
    #[must_use]
    pub fn current_seq(&self) -> u64 {
        self.source_stack.last().map_or(0, |frame| frame.seq)
    }

    /// The display name of the current script or line context.
    #[must_use]
    pub fn current_name(&self) -> Option<&str> {
        self.source_stack.last().map(|frame| frame.name.as_str())
    }

    /// Marks the physical line currently executing.
    pub fn set_current_line(&mut self, line: usize) {
        if let Some(frame) = self.source_stack.last_mut() {
            frame.current_line = line;
        }
    }

    /// The physical line currently executing in the innermost script.
    #[must_use]
    pub fn current_line(&self) -> usize {
        self.source_stack
            .last()
            .map_or(0, |frame| frame.current_line)
    }

    /// `s:` variables for one SID.
    #[must_use]
    pub fn script_vars(&self, sid: Sid) -> Option<&ScopeMap> {
        self.scripts.get(&sid).map(|info| &info.vars)
    }

    /// Mutable `s:` variables for one SID.
    pub fn script_vars_mut(&mut self, sid: Sid) -> Option<&mut ScopeMap> {
        self.scripts.get_mut(&sid).map(|info| &mut info.vars)
    }

    /// Name registered for a SID.
    #[must_use]
    pub fn script_name(&self, sid: Sid) -> Option<&str> {
        self.scripts.get(&sid).map(|info| info.name.as_str())
    }

    /// All (SID, name) pairs in allocation order.
    #[must_use]
    pub fn script_names(&self) -> Vec<(Sid, &str)> {
        self.scripts
            .iter()
            .map(|(sid, info)| (*sid, info.name.as_str()))
            .collect()
    }

    /// Expands `<SNR>` in one script text line to the current SID's prefix.
    ///
    /// Upstream expands `<SNR>` to `<SID>{sid}_` wherever it appears in a
    /// sourced line (`src/nvim/ex_docmd.c:9543-9566`).
    #[must_use]
    pub fn expand_snr(&self, line: &str, sid: Sid) -> String {
        if !line.contains("<SNR>") {
            return line.to_owned();
        }
        line.replace("<SNR>", &format!("<SNR>{sid}_"))
    }

    /// Canonical current `<SNR>` prefix (`<SNR>{sid}_`), when sourcing.
    #[must_use]
    pub fn snr_prefix(&self) -> Option<String> {
        self.current_sid().map(|sid| format!("<SNR>{sid}_"))
    }

    /// Resolves a `#`-named autoload function to its script path: components
    /// before the last become directories under `autoload/`, e.g.
    /// `a#b#c` → `autoload/a/b.vim` (`src/nvim/runtime.c:144-167`).
    #[must_use]
    pub fn resolve_autoload(&self, function: &str) -> Option<PathBuf> {
        let mut components: Vec<&str> = function.split('#').collect();
        let last = components.pop()?;
        if last.is_empty() || components.iter().any(|part| part.is_empty()) {
            return None;
        }
        let mut relative = PathBuf::from("autoload");
        for component in &components {
            relative.push(component);
        }
        relative.set_extension("vim");
        for root in &self.runtime_roots {
            let candidate = root.path().join(&relative);
            if self.io.exists(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Whether `path` was already sourced through the load-once registry.
    #[must_use]
    pub fn is_sourced_once(&self, path: &Path) -> bool {
        self.sourced_once.contains(&self.io.canonicalize(path))
    }

    /// Records `path` in the load-once registry.
    pub fn mark_sourced_once(&mut self, path: &Path) {
        self.sourced_once.insert(self.io.canonicalize(path));
    }

    /// Reads a script through the IO seam.
    pub fn read_script(&self, path: &Path) -> io::Result<String> {
        self.io.read_to_string(path)
    }

    /// Synchronizes the visible `s:` scope into `scope.script` for the SID.
    pub fn load_script_scope(&self, sid: Sid, scope: &mut Scope) {
        if let Some(vars) = self.script_vars(sid) {
            scope.script = vars.clone();
        } else {
            scope.script = ScopeMap::new();
        }
    }

    /// Writes back the visible `s:` scope after execution.
    pub fn store_script_scope(&mut self, sid: Sid, scope: &Scope) {
        if let Some(vars) = self.script_vars_mut(sid) {
            vars.clone_from(&scope.script);
        }
    }

    /// Joins `\`-continuation lines, dropping comment lines, per the sourcing
    /// getline rules in `src/nvim/ex_docmd.c:1330-1430`:
    ///
    /// * A line whose first non-blank character is `\` continues the previous
    ///   logical line; the backslash, and whitespace before it, is removed.
    /// * Comment lines (first non-blank `"`) are skipped entirely and do not
    ///   terminate a pending continuation.
    /// * A `#!` interpreter line at the very start of a script is ignored.
    /// * A control character in the text terminates the script, mirroring
    ///   upstream treating NUL as end-of-file.
    /// * A trailing CR stays in the line. `get_one_sourceline`
    ///   (`runtime.c:2891-2905`) removes it only when the source file is
    ///   `EOL_DOS`, and that whole branch sits under `#ifdef USE_CRNL`, which
    ///   is a Windows-only define — so on this platform a sourced
    ///   `let g:v = 4<CR>` keeps its CR and reaches `eval0` as E488.
    pub fn join_logical_lines(&self, text: &str) -> Result<Vec<LogicalLine>, ScriptError> {
        let physical = text.split('\n').collect::<Vec<_>>();
        let mut logical: Vec<LogicalLine> = Vec::new();
        let mut first_line_of_script = true;
        let mut index = 0;
        while index < physical.len() {
            let number = index.saturating_add(1);
            let content = physical[index];
            if first_line_of_script && content.starts_with("#!") {
                first_line_of_script = false;
                index += 1;
                continue;
            }
            first_line_of_script = false;

            let spec = heredoc_spec(content).map_err(|(code, message)| ScriptError {
                code,
                message,
                line: Some(number),
            })?;
            if let Some(spec) = spec {
                let mut joined = content.trim_end_matches([' ', '\t']).to_owned();
                joined.push('\n');
                let mut text_indent: Option<&str> = None;
                let mut found_marker = false;
                index += 1;
                while index < physical.len() {
                    let body = physical[index];
                    if body.is_empty() && index + 1 == physical.len() && text.ends_with('\n') {
                        break;
                    }
                    let marker_line = if spec.trim {
                        body.strip_prefix(spec.command_indent).unwrap_or(body)
                    } else {
                        body
                    };
                    if marker_line == spec.marker {
                        found_marker = true;
                        index += 1;
                        break;
                    }
                    if spec.trim && text_indent.is_none() && !body.is_empty() {
                        let indent_len = body
                            .find(|character: char| !character.is_ascii_whitespace())
                            .unwrap_or(body.len());
                        text_indent = Some(&body[..indent_len]);
                    }
                    let body = text_indent.map_or(body, |indent| {
                        let matching = body
                            .bytes()
                            .zip(indent.bytes())
                            .take_while(|(left, right)| left == right)
                            .count();
                        &body[matching..]
                    });
                    if joined.len().saturating_add(body.len()).saturating_add(1) > MAX_LOGICAL_LINE {
                        return Err(ScriptError {
                            code: "E1389",
                            message: "continued line too long".to_owned(),
                            line: Some(index.saturating_add(1)),
                        });
                    }
                    joined.push_str(body);
                    joined.push('\n');
                    index += 1;
                }
                if !found_marker && spec.kind == HeredocKind::Let {
                    return Err(ScriptError {
                        code: "E990",
                        message: format!("Missing end marker '{}'", spec.marker),
                        line: Some(number),
                    });
                }
                logical.push(LogicalLine { text: joined, first_line: number });
                continue;
            }

            let trimmed_start = content.trim_start_matches([' ', '\t']);
            // Comment lines are skipped and never break a continuation.
            if trimmed_start.starts_with('\"') {
                index += 1;
                continue;
            }
            if let Some(continuation) = trimmed_start.strip_prefix('\\') {
                match logical.last_mut() {
                    Some(open) => {
                        let joined_len = open.text.len().saturating_add(continuation.len());
                        if joined_len > MAX_LOGICAL_LINE {
                            return Err(ScriptError {
                                code: "E1389",
                                message: "continued line too long".to_owned(),
                                line: Some(number),
                            });
                        }
                        open.text.push_str(continuation);
                    }
                    None => {
                        // Continuation at script start is taken literally.
                        logical.push(LogicalLine {
                            text: content.to_owned(),
                            first_line: number,
                        });
                    }
                }
                index += 1;
                continue;
            }
            logical.push(LogicalLine {
                text: content.trim_end_matches([' ', '\t']).to_owned(),
                first_line: number,
            });
            index += 1;
        }
        Ok(logical)
    }

    /// Renders the current source stack into an upstream-style throwpoint
    /// string: `function F[3]..script /path[12]`; innermost last.
    #[must_use]
    pub fn throwpoint_tail(&self) -> String {
        match self.source_stack.last() {
            Some(frame) => format!("script {}[{}]", frame.name, frame.current_line),
            None => "command line".to_owned(),
        }
    }
}

impl Default for ScriptCtx<RealFileIO> {
    fn default() -> Self {
        Self::new(RealFileIO)
    }
}
