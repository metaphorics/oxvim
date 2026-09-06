//! `:wshada`/`:rshada` — editor-state persistence (`shada.c`).
//!
//! Upstream: `.references/neovim/src/nvim/shada.c`. Entry types mirror the
//! `ShadaEntryType` enum (`shada.c:144-161`); each entry is stored as the
//! `[type, timestamp, length]` unsigned-integer prefix followed by exactly one
//! `MessagePack` object (`shada_pack_entry`, `shada.c:1557-1570`). Read
//! failures carry the upstream texts: `RCERR`/`E576` for critical stream
//! errors (`shada.c:125, 3046-3167`), `RERR`/`E575` `READERR` for malformed
//! entries (`shada.c:120, 3090-3093`), and `SERR`/`E886` for system errors
//! (`shada.c:127`).
//!
//! What is persisted follows the port's state: the last search pattern and
//! the Ex command-line history live on [`ModeMachine`]; registers, marks,
//! jump list, buffer list, and `v:oldfiles` live on [`Editor`]. Global
//! variables (`!` in `'shada'`) and the substitute replacement string are not
//! persisted by this port. Timestamps do not exist on port state, so the
//! upstream merge-by-timestamp comparisons degrade to presence checks: a
//! plain `:rshada` only fills absent state, `:rshada!` overwrites, and
//! `:wshada!` skips the merge with the old file (`shada_write_file`,
//! `shada.c:2703-2735`).

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ox_text::shada::{Entry as StreamEntry, EntryType, ShaDa as Stream};
use ox_text::{Buffer, Position};
use ox_types::{Object, OxStr};
use rmpv::Value;

use crate::editor::Editor;
use crate::marks::MarkLocation;
use crate::mode::ModeMachine;
use crate::options::OptionValue;
use crate::register::{RegisterContent, RegisterKind};
use crate::search::SearchDirection;

/// A `ShaDa` command failure carrying the upstream message text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShadaError {
    /// Upstream error code (`RCERR`, `RERR`, or `SERR`).
    pub(crate) code: &'static str,
    /// Message text without the code prefix.
    pub(crate) message: String,
}

impl ShadaError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// `READERR` (`shada.c:3090-3093`): one malformed entry.
fn read_error(entry: &str, position: u64, detail: &str) -> ShadaError {
    ShadaError::new(
        "E575",
        format!("Error while reading ShaDa file: {entry} entry at position {position} {detail}"),
    )
}

/// `RCERR` (`shada.c:125`): a critical stream error.
fn critical_error(message: impl std::fmt::Display) -> ShadaError {
    ShadaError::new("E576", format!("Error while reading ShaDa file: {message}"))
}

/// `SERR` (`shada.c:127`).
fn system_error(verb: &str, file: &Path, detail: &str) -> ShadaError {
    ShadaError::new(
        "E886",
        format!(
            "System error while {verb} ShaDa file {}: {detail}",
            file.display()
        ),
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Parsed `'shada'` option text.
///
/// `find_shada_parameter` and `get_shada_parameter` (`shada.c:3733-3760`).
pub(crate) struct ShadaParams<'s> {
    text: &'s str,
}

impl<'s> ShadaParams<'s> {
    pub(crate) fn new(text: &'s str) -> Self {
        Self { text }
    }

    /// `find_shada_parameter` (`shada.c:3745-3760`): scan flag-by-flag,
    /// stopping at `'n'` (always the last flag) or at the end.
    fn find_parameter(&self, flag: char) -> Option<&'s str> {
        let bytes = self.text.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] as char == flag {
                return Some(&self.text[index + 1..]);
            }
            if bytes[index] == b'n' {
                break;
            }
            match self.text[index..].find(',') {
                Some(offset) => index += offset + 1,
                None => break,
            }
        }
        None
    }

    /// `get_shada_parameter` (`shada.c:3733-3740`): the decimal value that
    /// follows the flag, or `None` when the flag is absent or bare.
    fn parameter(&self, flag: char) -> Option<i64> {
        let rest = self.find_parameter(flag)?;
        let end = rest
            .bytes()
            .position(|byte| !byte.is_ascii_digit())
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        rest[..end].parse().ok()
    }

    /// `'s'`: per-item size cap in KiB, default 10 (`shada.c:2245-2248`).
    fn max_kbyte(&self) -> i64 {
        self.parameter('s').unwrap_or(10)
    }

    /// `'''`: marked files kept (`shada.c:2263`).
    fn num_marked_files(&self) -> i64 {
        self.parameter('\'').unwrap_or(-1)
    }

    /// `'<'` (else `'"'`): register line cap; negative means no cap
    /// (`shada.c:2256-2260`).
    fn max_reg_lines(&self) -> i64 {
        match self.parameter('<') {
            Some(value) if value >= 0 => value,
            _ => self.parameter('"').unwrap_or(-1),
        }
    }

    fn dump_registers(&self) -> bool {
        self.max_reg_lines() != 0
    }

    /// `'f'`: dump global marks unless `f0` (`shada.c:2264`).
    fn dump_global_marks(&self) -> bool {
        self.parameter('f').unwrap_or(-1) != 0
    }

    /// `'%'`: buffer list (`shada.c:2332`).
    fn dump_buffer_list(&self) -> bool {
        self.find_parameter('%').is_some()
    }

    /// History item count for one bank; a missing flag falls back to the
    /// `'history'` option (`shada.c:2268-2280`).
    fn history_count(&self, flag: char, history_option: i64) -> i64 {
        self.parameter(flag).unwrap_or(history_option).max(0)
    }
}

/// The `'shada'` text a command runs under, applying `ex_shada`'s override
/// (`ex_docmd.c:7861-7873`): an empty option behaves as `'100`.
pub(crate) fn shada_text(editor: &Editor) -> String {
    let text = match editor.options().get_global("shada") {
        Ok(OptionValue::String(value)) => String::from_utf8_lossy(value.as_bytes()).into_owned(),
        _ => String::new(),
    };
    if text.is_empty() {
        "'100".to_owned()
    } else {
        text
    }
}

/// Resolves the `ShaDa` file (`shada_filename`, `shada.c:1289-1316`). The
/// explicit argument wins, then `'shadafile'` (`NONE` disables `ShaDa` for the
/// session), then the `'n'` flag of `'shada'`, then the default user-state
/// path. `None` means "`ShaDa` is disabled".
pub(crate) fn resolve_file(
    editor: &Editor,
    argument: &str,
    params: &ShadaParams,
) -> Option<PathBuf> {
    let argument = argument.trim();
    if !argument.is_empty() {
        return Some(PathBuf::from(argument));
    }
    if let Ok(OptionValue::String(name)) = editor.options().get_global("shadafile") {
        let name = String::from_utf8_lossy(name.as_bytes()).into_owned();
        if !name.is_empty() {
            return if name == "NONE" {
                None
            } else {
                Some(PathBuf::from(name))
            };
        }
    }
    if let Some(rest) = params.find_parameter('n') {
        let end = rest.find(',').unwrap_or(rest.len());
        let name = rest[..end].trim();
        if !name.is_empty() {
            return Some(PathBuf::from(name));
        }
    }
    default_file()
}

/// `shada_get_default_file` (`shada.c:1269-1277`): `nvim/shada/main.shada`
/// below the user state directory.
fn default_file() -> Option<PathBuf> {
    let state = match std::env::var_os("XDG_STATE_HOME") {
        Some(state) if Path::new(&state).is_absolute() => PathBuf::from(state),
        _ => PathBuf::from(std::env::var_os("HOME")?)
            .join(".local")
            .join("state"),
    };
    Some(state.join("nvim").join("shada").join("main.shada"))
}

/// The `'history'` option value, falling back to upstream's 10000 default.
fn history_option(editor: &Editor) -> i64 {
    match editor.options().get_global("history") {
        Ok(OptionValue::Number(value)) => *value,
        _ => 10_000,
    }
}

fn path_bytes(file: &Path) -> Vec<u8> {
    file.to_string_lossy().into_owned().into_bytes()
}

fn store(file: &Path, bytes: &[u8]) -> Result<(), ShadaError> {
    // os_fileio.c opens the shada file after os_file_mkdir to create the
    // state tree; a first write into a fresh XDG state home must build it.
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(file, bytes).map_err(|error| system_error("opening", file, &error.to_string()))
}

/// Serializes editor state to `file` (`shada_write` + `shada_write_file`,
/// `shada.c:2240-2692, 2703-2881`). `nomerge` is `:wshada!`.
///
/// # Errors
///
/// Returns [`ShadaError`] with the upstream `E886` text when the file cannot
/// be written.
pub(crate) fn write_shada(
    editor: &Editor,
    machine: Option<&ModeMachine>,
    file: &Path,
    nomerge: bool,
    shada_text: &str,
) -> Result<(), ShadaError> {
    let params = ShadaParams::new(shada_text);
    let max_kbyte = params.max_kbyte();
    if max_kbyte == 0 {
        // shada.c:2249-2251: `s0` writes no entries; the file is replaced empty.
        return store(file, &[]);
    }
    let timestamp = now();
    let cap = usize::try_from(max_kbyte).unwrap_or(0);
    let mut entries = Vec::new();
    entries.push(header_entry(timestamp, max_kbyte)); // shada.c:2304-2329
    if params.dump_buffer_list() {
        // shada.c:2331-2340
        entries.push(buffer_list_entry(editor, timestamp));
    }
    if params.dump_global_marks() {
        // shada.c:2444-2489, 2599-2600
        entries.extend(global_mark_entries(editor, timestamp));
    }
    if params.dump_registers() {
        // shada.c:2491-2494, 2601
        entries.extend(register_entries(editor, &params, timestamp));
    }
    if params.num_marked_files() > 0 {
        // shada.c:2408-2411, 2602-2608
        entries.extend(jump_entries(editor, timestamp));
    }
    let history = history_option(editor);
    if params.history_count('/', history) > 0 {
        // shada.c:2413-2424, 2619: `/0` in 'shada' disables search patterns.
        if let Some(entry) = search_pattern_entry(machine, timestamp) {
            entries.push(entry);
        }
    }
    if params.num_marked_files() > 0 {
        // shada.c:2496-2558, 2624-2655
        entries.extend(local_mark_entries(editor, timestamp));
        entries.extend(change_entries(editor, timestamp));
    }
    if params.history_count(':', history) > 0 {
        // shada.c:2658-2673
        entries.extend(history_entries(machine, history, &params, timestamp));
    }
    let fresh = Stream { entries };
    let merged = if nomerge {
        fresh
    } else {
        // shada.c:2560-2566: merge with the old file when it is readable.
        match std::fs::read(file) {
            Ok(bytes) => match Stream::read(&bytes[..], cap) {
                Ok(existing) => existing.merge(&fresh),
                Err(_) => fresh,
            },
            Err(_) => fresh,
        }
    };
    let mut bytes = Vec::new();
    merged
        .write(&mut bytes, cap)
        .map_err(|error| system_error("encoding", file, &error.to_string()))?;
    store(file, &bytes)
}

fn header_entry(timestamp: u64, max_kbyte: i64) -> StreamEntry {
    // shada.c:2304-2329
    StreamEntry::new(
        EntryType::Header,
        timestamp,
        Value::Map(vec![
            (Value::from("generator"), Value::from("nvim")),
            (Value::from("version"), Value::Binary(b"0.8.1".to_vec())),
            (Value::from("max_kbyte"), Value::from(max_kbyte)),
            (
                Value::from("pid"),
                Value::from(u64::from(std::process::id())),
            ),
            (Value::from("encoding"), Value::Binary(b"utf-8".to_vec())),
        ]),
    )
}

fn buffer_list_entry(editor: &Editor, timestamp: u64) -> StreamEntry {
    // shada.c:1513-1536
    let buffers = editor
        .buffers()
        .into_iter()
        .filter_map(|handle| {
            let state = editor.buffer(handle).ok()?;
            if !state.flags.contains(crate::buffer::BufferFlags::LISTED) {
                return None;
            }
            let name = state.name().as_bytes();
            if name.is_empty() {
                return None;
            }
            Some(Value::Map(vec![(
                Value::from("f"),
                Value::Binary(name.to_vec()),
            )]))
        })
        .collect();
    StreamEntry::new(EntryType::BufferList, timestamp, Value::Array(buffers))
}

fn filemark_value(name: Option<char>, file: &Path, position: Position) -> Value {
    // shada.c:1453-1481: `f` always; `l` when the line is not 1; `c` when the
    // column is not 0; `n` for named marks only (never for jumps/changes).
    let mut map = vec![(Value::from("f"), Value::Binary(path_bytes(file)))];
    if position.lnum != 1 {
        map.push((
            Value::from("l"),
            Value::from(u64::try_from(position.lnum).unwrap_or(u64::MAX)),
        ));
    }
    if position.col != 0 {
        map.push((
            Value::from("c"),
            Value::from(u64::try_from(position.col).unwrap_or(u64::MAX)),
        ));
    }
    if let Some(name) = name.filter(|name| *name != '"') {
        map.push((Value::from("n"), Value::from(u64::from(name_byte(name)))));
    }
    Value::Map(map)
}

fn name_byte(name: char) -> u8 {
    u8::try_from(name).unwrap_or(0)
}

fn global_mark_entries(editor: &Editor, timestamp: u64) -> Vec<StreamEntry> {
    editor
        .global_marks()
        .iter()
        .filter_map(|(name, location)| {
            let file = location.file()?;
            Some(StreamEntry::new(
                EntryType::GlobalMark,
                timestamp,
                filemark_value(Some(name), file, location.position),
            ))
        })
        .collect()
}

fn register_entries(editor: &Editor, params: &ShadaParams, timestamp: u64) -> Vec<StreamEntry> {
    // shada_initialize_registers: `-`, `0-9`, and `a-z`.
    let cap = params.max_reg_lines();
    let unnamed = editor.registers().unnamed_target_name();
    let names = std::iter::once('-').chain('0'..='9').chain('a'..='z');
    let mut entries = Vec::new();
    for name in names {
        let Ok(Some(content)) = editor.registers().get(name) else {
            continue;
        };
        if cap >= 0 && i64::try_from(content.lines().len()).unwrap_or(i64::MAX) > cap {
            continue;
        }
        entries.push(StreamEntry::new(
            EntryType::Register,
            timestamp,
            register_value(name, content, unnamed == name),
        ));
    }
    entries
}

fn register_value(name: char, content: &RegisterContent, is_unnamed: bool) -> Value {
    // shada.c:1483-1511: `rc` and `n` always; `rt`/`rw`/`ru` only when they
    // differ from the defaults (characterwise, width 0, not unnamed).
    let contents = content
        .lines()
        .iter()
        .map(|line| Value::Binary(line.clone()))
        .collect();
    let mut map = vec![
        (Value::from("rc"), Value::Array(contents)),
        (Value::from("n"), Value::from(u64::from(name_byte(name)))),
    ];
    let type_id: u64 = match content.kind() {
        RegisterKind::CharacterWise => 0,
        RegisterKind::LineWise => 1,
        RegisterKind::BlockWise { .. } => 2,
    };
    if type_id != 0 {
        map.push((Value::from("rt"), Value::from(type_id)));
    }
    if let RegisterKind::BlockWise { width } = content.kind()
        && width != 0
    {
        map.push((
            Value::from("rw"),
            Value::from(u64::try_from(width).unwrap_or(u64::MAX)),
        ));
    }
    if is_unnamed {
        map.push((Value::from("ru"), Value::Boolean(true)));
    }
    Value::Map(map)
}

fn jump_entries(editor: &Editor, timestamp: u64) -> Vec<StreamEntry> {
    // shada_init_jumps: file-backed jump entries only.
    editor
        .jumplist()
        .entries()
        .iter()
        .filter_map(|location| {
            let file = location.file()?;
            Some(StreamEntry::new(
                EntryType::Jump,
                timestamp,
                filemark_value(None, file, location.position),
            ))
        })
        .collect()
}

fn search_pattern_entry(machine: Option<&ModeMachine>, timestamp: u64) -> Option<StreamEntry> {
    // shada.c:1421-1451: `sp` always; flags only when they differ from the
    // defaults (magic on, forward, not a substitute pattern).
    let state = machine?.search_state();
    let pattern = state.last_pattern()?.as_bytes().to_vec();
    let mut map = vec![(Value::from("sp"), Value::Binary(pattern))];
    if state.last_direction() == Some(SearchDirection::Backward) {
        map.push((Value::from("sb"), Value::Boolean(true)));
    }
    Some(StreamEntry::new(
        EntryType::SearchPattern,
        timestamp,
        Value::Map(map),
    ))
}

fn local_mark_entries(editor: &Editor, timestamp: u64) -> Vec<StreamEntry> {
    // shada.c:2496-2537: named file marks for every buffer with a file name.
    let mut entries = Vec::new();
    for handle in editor.buffers() {
        let Ok(state) = editor.buffer(handle) else {
            continue;
        };
        let name = state.name().as_bytes();
        if name.is_empty() {
            continue;
        }
        let path = PathBuf::from(String::from_utf8_lossy(name).into_owned());
        for (mark, position) in state.marks.iter() {
            if !mark.is_ascii_lowercase() {
                continue;
            }
            entries.push(StreamEntry::new(
                EntryType::LocalMark,
                timestamp,
                filemark_value(Some(mark), &path, position),
            ));
        }
    }
    entries
}

fn change_entries(editor: &Editor, timestamp: u64) -> Vec<StreamEntry> {
    // shada.c:2538-2556
    let mut entries = Vec::new();
    for handle in editor.buffers() {
        let Ok(state) = editor.buffer(handle) else {
            continue;
        };
        let name = state.name().as_bytes();
        if name.is_empty() {
            continue;
        }
        let path = PathBuf::from(String::from_utf8_lossy(name).into_owned());
        let Some(changes) = editor.changelists().entries(handle) else {
            continue;
        };
        for position in changes {
            entries.push(StreamEntry::new(
                EntryType::Change,
                timestamp,
                filemark_value(None, &path, *position),
            ));
        }
    }
    entries
}

fn history_entries(
    machine: Option<&ModeMachine>,
    history: i64,
    params: &ShadaParams,
    timestamp: u64,
) -> Vec<StreamEntry> {
    let Some(machine) = machine else {
        return Vec::new();
    };
    let cap = params.history_count(':', history);
    if cap <= 0 {
        return Vec::new();
    }
    let items = machine.cmdline_history_entries();
    let skip = items
        .len()
        .saturating_sub(usize::try_from(cap).unwrap_or(0));
    items[skip..]
        .iter()
        .map(|item| {
            // shada.c:1377-1389: `[histtype, string]` for the command bank.
            StreamEntry::new(
                EntryType::History,
                timestamp,
                Value::Array(vec![
                    Value::from(0u64),
                    Value::Binary(item.as_bytes().to_vec()),
                ]),
            )
        })
        .collect()
}

/// One entry parsed from the stream, with its start position.
struct RawEntry {
    position: u64,
    type_id: u64,
    data: Value,
}

/// Everything `parse_stream` recovered before a critical error.
struct ParsedStream {
    entries: Vec<Result<RawEntry, ShadaError>>,
    fatal: Option<ShadaError>,
}

/// Streams `[type, timestamp, length]`-prefixed entries with upstream error
/// positions (`msgpack_read_uint64`, `shada.c:3029-3088`, and
/// `shada_read_next_item`, `shada.c:3105-3260`).
///
/// Entry-level payload problems become recorded `E575` errors the caller
/// skips (`kSDReadStatusMalformed`, `shada.c:965-966`); prefix, truncation,
/// and internal-use errors are fatal (`kSDReadStatusNotShaDa`). Unknown types
/// above 11 are skipped without decoding (`shada.c:3221-3238`), except that a
/// first entry of type `'\n'` or above must still parse as `MessagePack` or
/// the file is rejected as not-ShaDa (`shada.c:3174-3218`).
fn parse_stream(bytes: &[u8]) -> ParsedStream {
    let mut stream = ParsedStream {
        entries: Vec::new(),
        fatal: None,
    };
    let mut cursor = Cursor::new(bytes);
    let total = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let mut first = true;
    while cursor.position() < total {
        let start = cursor.position();
        let type_id = match read_envelope_uint(&mut cursor, true) {
            Ok(Some(value)) => value,
            Ok(None) => break,
            Err(error) => {
                stream.fatal = Some(error);
                break;
            }
        };
        let _timestamp = match read_envelope_uint(&mut cursor, false) {
            Ok(Some(value)) => value,
            Ok(None) => break,
            Err(error) => {
                stream.fatal = Some(error);
                break;
            }
        };
        let length = match read_envelope_uint(&mut cursor, false) {
            Ok(Some(value)) => value,
            Ok(None) => break,
            Err(error) => {
                stream.fatal = Some(error);
                break;
            }
        };
        let Ok(length) = usize::try_from(length) else {
            // shada.c:3146-3151
            stream.fatal = Some(critical_error(format!(
                "there is an item at position {start} that is stated to be too long"
            )));
            break;
        };
        let start_index = usize::try_from(cursor.position()).unwrap_or(usize::MAX);
        let end = start_index.checked_add(length);
        let Some(payload) = end.and_then(|end| bytes.get(start_index..end)) else {
            stream.fatal = Some(critical_error(format!(
                "there is an item at position {start} that is stated to be too long"
            )));
            break;
        };
        if let Some(end) = end {
            cursor.set_position(u64::try_from(end).unwrap_or(total));
        }
        if type_id == 0 {
            // shada.c:3158-3168
            stream.fatal = Some(critical_error(format!(
                "there is an item at position {start} that must not be there: Missing items are for internal uses only"
            )));
            break;
        }
        if first && (type_id == 10 || type_id > 11) {
            // 10 is '\n'
            // shada.c:3174-3218: a text file's first byte must at least parse.
            first = false;
            let mut probe = Cursor::new(payload);
            if rmpv::decode::read_value(&mut probe).is_err() {
                stream.fatal = Some(critical_error(format!(
                    "there is an item at position {start} that is not a ShaDa item"
                )));
                break;
            }
            continue;
        }
        first = false;
        if type_id > 11 {
            // Unknown forward-compat entries are skipped without decoding.
            continue;
        }
        let mut payload_cursor = Cursor::new(payload);
        let data = match rmpv::decode::read_value(&mut payload_cursor) {
            Ok(data) => data,
            Err(error) => {
                stream.entries.push(Err(read_error(
                    "item",
                    start,
                    &format!("is not valid MessagePack data: {error}"),
                )));
                continue;
            }
        };
        if usize::try_from(payload_cursor.position()).unwrap_or(usize::MAX) != payload.len() {
            // shada.c:3518-3523
            stream
                .entries
                .push(Err(read_error("item", start, "additional bytes")));
            continue;
        }
        stream.entries.push(Ok(RawEntry {
            position: start,
            type_id,
            data,
        }));
    }
    stream
}

/// `msgpack_read_uint64` (`shada.c:3029-3088`): positive fixnums and
/// `0xCC..=0xCF` only, with the upstream error texts.
fn read_envelope_uint(
    cursor: &mut Cursor<&[u8]>,
    allow_eof: bool,
) -> Result<Option<u64>, ShadaError> {
    let position = cursor.position();
    let index = usize::try_from(position).unwrap_or(usize::MAX);
    let Some(byte) = cursor.get_ref().get(index).copied() else {
        if allow_eof {
            return Ok(None);
        }
        // shada.c:3046-3050
        return Err(critical_error(format!(
            "expected positive integer at position {position}, but got nothing"
        )));
    };
    cursor.set_position(position.saturating_add(1));
    let value = match byte {
        0x00..=0x7F => u64::from(byte),
        0xCC => read_be_uint(cursor, 1)?,
        0xCD => read_be_uint(cursor, 2)?,
        0xCE => read_be_uint(cursor, 4)?,
        0xCF => read_be_uint(cursor, 8)?,
        // shada.c:3073-3075
        _ => {
            return Err(critical_error(format!(
                "expected positive integer at position {position}"
            )));
        }
    };
    Ok(Some(value))
}

fn read_be_uint(cursor: &mut Cursor<&[u8]>, width: usize) -> Result<u64, ShadaError> {
    let start = cursor.position();
    let index = usize::try_from(start).unwrap_or(usize::MAX);
    let Some(bytes) = cursor.get_ref().get(index..index.saturating_add(width)) else {
        // Truncated scalar (shada.c:3081-3083 read failure).
        return Err(critical_error(format!(
            "expected positive integer at position {start}"
        )));
    };
    let mut value = 0u64;
    for byte in bytes {
        value = (value << 8) | u64::from(*byte);
    }
    cursor.set_position(start.saturating_add(u64::try_from(width).unwrap_or(0)));
    Ok(value)
}

fn value_bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Binary(data) => Some(data.clone()),
        Value::String(text) => Some(text.as_str()?.as_bytes().to_vec()),
        _ => None,
    }
}

fn value_name(value: &Value) -> Option<char> {
    value
        .as_u64()
        .and_then(|byte| u32::try_from(byte).ok())
        .and_then(char::from_u32)
}

fn map_entry<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter().find_map(|(candidate, value)| {
        let matches = candidate.as_str().is_some_and(|text| text == key)
            || candidate
                .as_slice()
                .is_some_and(|bytes| bytes == key.as_bytes());
        matches.then_some(value)
    })
}

/// The `v:oldfiles` list as owned strings.
fn oldfiles(editor: &Editor) -> Vec<OxStr> {
    match editor.vvars().get(&OxStr::from("oldfiles")) {
        Some(Object::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Object::String(text) => Some(text.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Applies a `ShaDa` file to editor state (`shada_read` + `shada_read_file`,
/// `shada.c:576-612, 905-1263`). `forceit` is `:rshada!`.
///
/// # Errors
///
/// Returns [`ShadaError`] with the upstream `E886` text when the file cannot
/// be read, the first `E575` text for a malformed entry, or the fatal `E576`
/// text for a corrupt stream. Entries before a failure stay applied, matching
/// upstream's stop-at-error read loop.
pub(crate) fn read_shada(
    editor: &mut Editor,
    machine: Option<&mut ModeMachine>,
    file: &Path,
    forceit: bool,
    shada_text: &str,
) -> Result<(), ShadaError> {
    let bytes =
        std::fs::read(file).map_err(|error| system_error("opening", file, &error.to_string()))?;
    let parsed = parse_stream(&bytes);
    let params = ShadaParams::new(shada_text);
    let history = history_option(editor);
    let existing_oldfiles = oldfiles(editor);
    let rebuild_oldfiles = forceit || existing_oldfiles.is_empty(); // shada.c:910-911
    let mut state = ApplyState {
        editor,
        machine,
        forceit,
        want_marks: params.num_marked_files() > 0, // shada.c:929-931
        rebuild_oldfiles,
        history,
        params,
        oldfiles: if rebuild_oldfiles {
            Vec::new()
        } else {
            existing_oldfiles
        },
        malformed: None,
    };
    // Buffer-list entries land before everything else so file-addressed
    // entries (local marks, change marks) find their target buffers; the
    // writer interleaves them by section, so this is a stable reordering
    // of independent entry types, not a stream rewrite.
    let mut deferred = Vec::new();
    for entry in parsed.entries {
        match entry {
            Err(error) => state.record(error),
            Ok(entry) => {
                if entry.type_id == 9 {
                    state.apply(entry);
                } else {
                    deferred.push(entry);
                }
            }
        }
    }
    for entry in deferred {
        state.apply(entry);
    }
    if let Some(fatal) = parsed.fatal {
        // Entries before the corruption were applied; upstream stops there.
        return Err(fatal);
    }
    if state.rebuild_oldfiles {
        // shada.c:949-952, 1161-1174
        let malformed = state.malformed.take();
        let files: Vec<Object> = state.oldfiles.into_iter().map(Object::String).collect();
        state
            .editor
            .vvars_mut()
            .insert(OxStr::from("oldfiles"), Object::Array(files));
        state.malformed = malformed;
    }
    match state.malformed {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Per-file application state for one `:rshada`.
struct ApplyState<'a, 's> {
    editor: &'a mut Editor,
    machine: Option<&'a mut ModeMachine>,
    forceit: bool,
    want_marks: bool,
    rebuild_oldfiles: bool,
    history: i64,
    params: ShadaParams<'s>,
    oldfiles: Vec<OxStr>,
    malformed: Option<ShadaError>,
}

impl ApplyState<'_, '_> {
    fn record(&mut self, error: ShadaError) {
        if self.malformed.is_none() {
            self.malformed = Some(error);
        }
    }

    fn apply(&mut self, entry: RawEntry) {
        let RawEntry {
            position,
            type_id,
            data,
        } = entry;
        match type_id {
            2 => self.apply_search_pattern(position, &data),
            4 => self.apply_history(position, &data),
            5 => self.apply_register(position, &data),
            7 => self.apply_global_mark(position, &data),
            8 => self.apply_jump(position, &data),
            9 => self.apply_buffer_list(position, &data),
            10 => self.apply_local_mark(position, &data),
            11 => self.apply_change_mark(position, &data),
            // Header entries are never applied (shada.c:973-975); variables
            // and the substitute replacement have no port state to receive
            // them (shada.c:1017-1037, 1076-1081). Unknown types were skipped
            // by the parser.
            _ => {}
        }
    }

    fn apply_search_pattern(&mut self, position: u64, data: &Value) {
        // shada.c:976-1014; payload shada.c:3247-3262.
        let Some(map) = data.as_map() else {
            self.record(read_error(
                "search pattern",
                position,
                "is not a dictionary",
            ));
            return;
        };
        let Some(pattern) = map_entry(map, "sp").and_then(value_bytes) else {
            // shada.c:3256-3258
            self.record(read_error("search pattern", position, "has no pattern"));
            return;
        };
        let backward = map_entry(map, "sb")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Ok(pattern) = String::from_utf8(pattern) else {
            self.record(read_error("search pattern", position, "is not valid UTF-8"));
            return;
        };
        let Some(machine) = self.machine.as_deref_mut() else {
            return;
        };
        if !self.forceit && machine.search_state().last_pattern().is_some() {
            // Timestamp proxy: keep newer in-session state (shada.c:977-988).
            return;
        }
        machine.restore_search_pattern(pattern, backward);
    }

    fn apply_history(&mut self, position: u64, data: &Value) {
        // shada.c:1038-1045; payload shada.c:3336-3373.
        let Some(items) = data.as_array() else {
            self.record(read_error(
                "history",
                position,
                "is not an array with enough elements",
            ));
            return;
        };
        if items.len() < 2 {
            // shada.c:3341-3343
            self.record(read_error(
                "history",
                position,
                "is not an array with enough elements",
            ));
            return;
        }
        let Some(hist_type) = items[0].as_u64() else {
            // shada.c:3346-3348
            self.record(read_error(
                "history",
                position,
                "has wrong history type type",
            ));
            return;
        };
        if hist_type != 0 {
            // Only the Ex command bank exists in this port (shada.c:1039-1042).
            return;
        }
        let Some(item) = value_bytes(&items[1]) else {
            // shada.c:3351-3353
            self.record(read_error(
                "history",
                position,
                "has wrong history string type",
            ));
            return;
        };
        if item.contains(&0) {
            // shada.c:3355-3357
            self.record(read_error(
                "history",
                position,
                "contains string with zero byte inside",
            ));
            return;
        }
        let Ok(item) = String::from_utf8(item) else {
            self.record(read_error(
                "history",
                position,
                "has wrong history string type",
            ));
            return;
        };
        let cap = self.params.history_count(':', self.history);
        if cap <= 0 {
            return;
        }
        let Some(machine) = self.machine.as_deref_mut() else {
            return;
        };
        let mut merged = machine.cmdline_history_entries().to_vec();
        if !merged.iter().any(|existing| existing == &item) {
            merged.push(item);
        }
        let skip = merged
            .len()
            .saturating_sub(usize::try_from(cap).unwrap_or(0));
        machine.set_cmdline_history(merged[skip..].to_vec());
    }

    fn apply_register(&mut self, position: u64, data: &Value) {
        // shada.c:1046-1074; payload shada.c:3308-3330.
        let Some(map) = data.as_map() else {
            self.record(read_error("register", position, "is not a dictionary"));
            return;
        };
        let missing_contents = "has rc key with missing or empty array";
        let Some(contents) = map_entry(map, "rc").and_then(Value::as_array) else {
            // shada.c:3315-3319
            self.record(read_error("register", position, missing_contents));
            return;
        };
        if contents.is_empty() {
            self.record(read_error("register", position, missing_contents));
            return;
        }
        let mut rows = Vec::with_capacity(contents.len());
        for row in contents {
            if let Some(bytes) = value_bytes(row) {
                // Upstream rows never carry separators; a hostile file may
                // embed them and register rows reject newlines, so split.
                rows.extend(bytes.split(|byte| *byte == b'\n').map(<[u8]>::to_vec));
            } else {
                self.record(read_error(
                    "register",
                    position,
                    "has a contents row that is not a binary string",
                ));
                return;
            }
        }
        let Some(name) = map_entry(map, "n").and_then(value_name) else {
            return; // unnamed/malformed names are dropped, as upstream defaults
        };
        let type_id = map_entry(map, "rt").and_then(Value::as_u64).unwrap_or(0);
        let kind = match type_id {
            0 => RegisterKind::CharacterWise,
            1 => RegisterKind::LineWise,
            2 => {
                let width =
                    usize::try_from(map_entry(map, "rw").and_then(Value::as_u64).unwrap_or(0))
                        .unwrap_or(0);
                if width == 0 {
                    // Upstream drops invalid registers (shada.c:1047-1052);
                    // a zero-width block would break downstream geometry.
                    return;
                }
                RegisterKind::BlockWise { width }
            }
            // shada.c:1047-1052
            _ => return,
        };
        if !self.forceit && matches!(self.editor.registers().get(name), Ok(Some(_))) {
            // Timestamp proxy (shada.c:1056-1062).
            return;
        }
        let content = RegisterContent::from_binary_lines(kind, rows);
        let _ = self.editor.registers_mut().set(name, content);
    }

    fn apply_global_mark(&mut self, position: u64, data: &Value) {
        // shada.c:1082-1102; payload shada.c:3267-3306.
        let Some(map) = data.as_map() else {
            self.record(read_error("mark", position, "is not a dictionary"));
            return;
        };
        let Some(file) = map_entry(map, "f").and_then(value_bytes) else {
            // shada.c:3294-3297
            self.record(read_error("mark", position, "is missing file name"));
            return;
        };
        let lnum = map_entry(map, "l").and_then(Value::as_u64).unwrap_or(1);
        let col = map_entry(map, "c").and_then(Value::as_u64).unwrap_or(0);
        if lnum == 0 {
            // shada.c:3298-3300
            self.record(read_error("mark", position, "has invalid line number"));
            return;
        }
        let Some(name) = map_entry(map, "n").and_then(value_name) else {
            return;
        };
        if !name.is_ascii_uppercase() && !name.is_ascii_digit() {
            return;
        }
        if !self.forceit
            && self
                .editor
                .global_marks()
                .get(name)
                .ok()
                .flatten()
                .is_some()
        {
            // Timestamp proxy (shada.c:1099).
            return;
        }
        let location = MarkLocation::in_file(
            String::from_utf8_lossy(&file).into_owned(),
            Position {
                lnum: usize::try_from(lnum).unwrap_or(1),
                col: usize::try_from(col).unwrap_or(0),
            },
        );
        let _ = self.editor.global_marks_mut().set(name, location);
    }

    fn apply_jump(&mut self, position: u64, data: &Value) {
        // shada.c:1103-1134.
        let Some(map) = data.as_map() else {
            self.record(read_error("mark", position, "is not a dictionary"));
            return;
        };
        let Some(file) = map_entry(map, "f").and_then(value_bytes) else {
            self.record(read_error("mark", position, "is missing file name"));
            return;
        };
        let lnum = map_entry(map, "l").and_then(Value::as_u64).unwrap_or(1);
        if lnum == 0 {
            self.record(read_error("mark", position, "has invalid line number"));
            return;
        }
        let col = map_entry(map, "c").and_then(Value::as_u64).unwrap_or(0);
        let location = MarkLocation::in_file(
            String::from_utf8_lossy(&file).into_owned(),
            Position {
                lnum: usize::try_from(lnum).unwrap_or(1),
                col: usize::try_from(col).unwrap_or(0),
            },
        );
        self.editor.jumplist_mut().push(location);
    }

    fn apply_buffer_list(&mut self, position: u64, data: &Value) {
        // shada.c:924-927 (gate) + 1139-1157 (apply).
        if !self.params.dump_buffer_list() || !self.editor.arglist().is_empty() {
            return;
        }
        let Some(items) = data.as_array() else {
            // shada.c:3446-3448
            self.record(read_error("buffer list", position, "is not an array"));
            return;
        };
        for item in items {
            let Some(map) = item.as_map() else {
                continue;
            };
            let Some(name) = map_entry(map, "f").and_then(value_bytes) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(&name).into_owned();
            let existing = self.editor.buffers().into_iter().find(|handle| {
                self.editor
                    .buffer(*handle)
                    .is_ok_and(|state| state.name().as_bytes() == name.as_slice())
            });
            if existing.is_some() {
                continue;
            }
            if let Ok(handle) = self.editor.create_buffer_with(Buffer::new(), true) {
                let _ = self
                    .editor
                    .rename_buffer(handle, OxStr::from(text.as_str()));
            }
        }
    }

    fn push_oldfile(&mut self, file: &[u8]) {
        let text = OxStr::from(String::from_utf8_lossy(file).into_owned().as_str());
        if !self.oldfiles.contains(&text) {
            // shada.c:1161-1174: insertion-ordered dedup.
            self.oldfiles.push(text);
        }
    }

    fn apply_local_mark(&mut self, position: u64, data: &Value) {
        // shada.c:1159-1224.
        let Some(map) = data.as_map() else {
            self.record(read_error("mark", position, "is not a dictionary"));
            return;
        };
        let Some(file) = map_entry(map, "f").and_then(value_bytes) else {
            self.record(read_error("mark", position, "is missing file name"));
            return;
        };
        let lnum = map_entry(map, "l").and_then(Value::as_u64).unwrap_or(1);
        if lnum == 0 {
            self.record(read_error("mark", position, "has invalid line number"));
            return;
        }
        let col = map_entry(map, "c").and_then(Value::as_u64).unwrap_or(0);
        if self.rebuild_oldfiles {
            self.push_oldfile(&file);
        }
        if !self.want_marks {
            return;
        }
        let Some(name) = map_entry(map, "n").and_then(value_name) else {
            return;
        };
        if !name.is_ascii_lowercase() {
            return;
        }
        let text = String::from_utf8_lossy(&file).into_owned();
        let target = self.editor.buffers().into_iter().find(|handle| {
            self.editor
                .buffer(*handle)
                .is_ok_and(|state| state.name().as_bytes() == text.as_bytes())
        });
        let Some(handle) = target else {
            return; // shada.c:1180-1184: marks need an existing buffer
        };
        if !self.forceit {
            let present = self
                .editor
                .buffer(handle)
                .ok()
                .and_then(|state| state.marks.get(name).ok().flatten())
                .is_some();
            if present {
                return;
            }
        }
        let value = Position {
            lnum: usize::try_from(lnum).unwrap_or(1),
            col: usize::try_from(col).unwrap_or(0),
        };
        let _ = self.editor.set_local_mark(handle, name, value);
    }

    fn apply_change_mark(&mut self, position: u64, data: &Value) {
        // Changes feed v:oldfiles like local marks (shada.c:1159-1175); the
        // port keeps no restorable per-buffer change timestamps.
        let Some(map) = data.as_map() else {
            self.record(read_error("mark", position, "is not a dictionary"));
            return;
        };
        let Some(file) = map_entry(map, "f").and_then(value_bytes) else {
            self.record(read_error("mark", position, "is missing file name"));
            return;
        };
        if map_entry(map, "l").and_then(Value::as_u64) == Some(0) {
            self.record(read_error("mark", position, "has invalid line number"));
            return;
        }
        if self.rebuild_oldfiles {
            self.push_oldfile(&file);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::excmd_exec::{ExExecutor, ExecError, TestEditorAccess};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Upstream `'shada'` default plus the buffer-list flag.
    // Upstream comma grammar: each part is one flag. `!'100` would hide the
    // mark cap from the scanner (find_shada_parameter skips to the comma);
    // the upstream default uses `!,'100`.
    const TEST_SHADA: &str = "!,'100,<50,s10,h,%";

    /// A temp file removed on drop (`std::env::temp_dir`, per the wave brief).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let name = format!(
                "oxvim-shada-{tag}-{}-{:?}.shada",
                std::process::id(),
                std::thread::current().id()
            );
            Self(std::env::temp_dir().join(name))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn editor_with_machine() -> (Editor, Rc<RefCell<ModeMachine>>) {
        (Editor::new(), Rc::new(RefCell::new(ModeMachine::default())))
    }

    fn envelope(type_id: u64, timestamp: u64, payload: &[u8], bytes: &mut Vec<u8>) {
        push_uint(bytes, type_id);
        push_uint(bytes, timestamp);
        push_uint(bytes, u64::try_from(payload.len()).unwrap_or(0));
        bytes.extend_from_slice(payload);
    }

    fn push_uint(bytes: &mut Vec<u8>, value: u64) {
        rmpv::encode::write_value(bytes, &Value::from(value)).expect("uint encodes");
    }

    fn payload_bytes(value: &Value) -> Vec<u8> {
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, value).expect("payload encodes");
        bytes
    }

    fn register_payload(name: char, rows: &[&str]) -> Vec<u8> {
        let contents = rows
            .iter()
            .map(|row| Value::Binary(row.as_bytes().to_vec()))
            .collect();
        payload_bytes(&Value::Map(vec![
            (Value::from("rc"), Value::Array(contents)),
            (
                Value::from("n"),
                Value::from(u64::from(u8::try_from(name).expect("ascii name"))),
            ),
        ]))
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one round-trip scenario asserting every restored section"
    )]
    fn round_trip_registers_marks_search_history_and_oldfiles() {
        let (mut editor, machine) = editor_with_machine();
        editor
            .registers_mut()
            .yank_to(
                'a',
                RegisterContent::linewise(vec![b"one".to_vec(), b"two".to_vec()])
                    .expect("register content"),
            )
            .expect("register a");
        editor
            .registers_mut()
            .yank_to(
                'b',
                RegisterContent::characterwise(b"xyz").expect("register content"),
            )
            .expect("register b");
        let marked = std::env::temp_dir().join("oxvim-shada-roundtrip-file");
        editor
            .global_marks_mut()
            .set(
                'A',
                MarkLocation::in_file(marked.clone(), Position { lnum: 3, col: 2 }),
            )
            .expect("global mark");
        let buffer = editor
            .create_buffer_with(Buffer::new(), true)
            .expect("buffer");
        editor
            .rename_buffer(buffer, OxStr::from("Xmarkbuf"))
            .expect("buffer name");
        editor
            .set_local_mark(buffer, 'a', Position { lnum: 2, col: 1 })
            .expect("local mark");
        machine
            .borrow_mut()
            .restore_search_pattern("needle".to_owned(), true);
        machine
            .borrow_mut()
            .set_cmdline_history(vec!["echo one".to_owned(), "echo two".to_owned()]);

        let scratch = Scratch::new("roundtrip");
        write_shada(
            &editor,
            Some(&*machine.borrow()),
            scratch.path(),
            false,
            TEST_SHADA,
        )
        .expect("write succeeds");
        let bytes = std::fs::read(scratch.path()).expect("file exists");

        let (mut fresh, fresh_machine) = editor_with_machine();
        read_shada(
            &mut fresh,
            Some(&mut *fresh_machine.borrow_mut()),
            scratch.path(),
            false,
            TEST_SHADA,
        )
        .expect("read succeeds");

        // Registers round-trip with their row shape.
        let restored = fresh
            .registers()
            .get('a')
            .expect("valid name")
            .expect("register a restored");
        assert_eq!(restored.getreg_bytes(), b"one\ntwo\n");
        let restored = fresh
            .registers()
            .get('b')
            .expect("valid name")
            .expect("register b restored");
        assert_eq!(restored.getreg_bytes(), b"xyz");

        // Global and buffer-local marks round-trip.
        let global = fresh
            .global_marks()
            .get('A')
            .expect("valid name")
            .expect("mark A restored");
        assert_eq!(global.file(), Some(marked.as_path()));
        assert_eq!(global.position, Position { lnum: 3, col: 2 });
        let handle = fresh
            .buffers()
            .into_iter()
            .find(|handle| {
                fresh
                    .buffer(*handle)
                    .is_ok_and(|state| state.name().as_bytes() == b"Xmarkbuf")
            })
            .expect("buffer list restored the named buffer");
        let local = fresh
            .buffer(handle)
            .expect("live buffer")
            .marks
            .get('a')
            .expect("valid mark")
            .expect("local mark restored");
        assert_eq!(local, Position { lnum: 2, col: 1 });

        // Search pattern and history restore through the mode machine.
        assert_eq!(
            fresh_machine.borrow().search_state().last_pattern(),
            Some("needle")
        );
        assert_eq!(
            fresh_machine.borrow().search_state().last_direction(),
            Some(SearchDirection::Backward)
        );
        assert_eq!(fresh_machine.borrow().cmdline_history(0), Some("echo one"));
        assert_eq!(fresh_machine.borrow().cmdline_history(1), Some("echo two"));

        // v:oldfiles gains the file names from local-mark entries
        // (shada.c:1159-1175): the named buffer, not the global-mark file.
        let oldfiles = fresh
            .vvars()
            .get(&OxStr::from("oldfiles"))
            .cloned()
            .expect("oldfiles list");
        let Object::Array(items) = oldfiles else {
            panic!("oldfiles is a list");
        };
        assert!(
            items
                .iter()
                .any(|item| matches!(item, Object::String(text) if text.as_bytes() == b"Xmarkbuf")),
            "oldfiles records the marked file, got {items:?}"
        );
        assert_eq!(
            bytes.first().copied(),
            Some(1),
            "the stream starts with a header entry"
        );
    }

    #[test]
    fn malformed_prefix_reports_e576_at_the_failing_position() {
        let (mut editor, _) = editor_with_machine();
        let scratch = Scratch::new("malformed");
        std::fs::write(scratch.path(), b"\xa3abc").expect("scratch file");
        let error = read_shada(&mut editor, None, scratch.path(), false, TEST_SHADA)
            .expect_err("malformed prefix fails");
        assert_eq!(error.code, "E576");
        assert_eq!(
            error.message,
            "Error while reading ShaDa file: expected positive integer at position 0"
        );
    }

    #[test]
    fn missing_entry_type_is_rejected_as_internal_use() {
        let (mut editor, _) = editor_with_machine();
        let scratch = Scratch::new("internal");
        let mut bytes = Vec::new();
        // One payload byte is declared but never decoded: type 0 is rejected
        // before the payload is read (shada.c:3158-3168).
        envelope(0, 1, &[0x01], &mut bytes);
        std::fs::write(scratch.path(), &bytes).expect("scratch file");
        let error = read_shada(&mut editor, None, scratch.path(), false, TEST_SHADA)
            .expect_err("type 0 is fatal");
        assert_eq!(error.code, "E576");
        assert!(
            error.message.contains("position 0"),
            "message cites the entry position: {}",
            error.message
        );
        assert!(
            error
                .message
                .contains("Missing items are for internal uses only"),
            "message matches the upstream text: {}",
            error.message
        );
    }

    #[test]
    fn unknown_entry_types_are_skipped_without_failing_the_read() {
        let (mut editor, _) = editor_with_machine();
        let scratch = Scratch::new("unknown");
        let mut bytes = Vec::new();
        envelope(5, 1, &register_payload('a', &["alpha"]), &mut bytes);
        // Type 100: the forward-compat form; its payload is never decoded.
        envelope(100, 2, &[0xC1, 0x02, 0x03], &mut bytes);
        envelope(5, 3, &register_payload('b', &["beta"]), &mut bytes);
        std::fs::write(scratch.path(), &bytes).expect("scratch file");
        read_shada(&mut editor, None, scratch.path(), false, TEST_SHADA)
            .expect("unknown entries skip");
        let restored = editor
            .registers()
            .get('a')
            .expect("valid name")
            .expect("register a present");
        assert_eq!(restored.getreg_bytes(), b"alpha");
        let restored = editor
            .registers()
            .get('b')
            .expect("valid name")
            .expect("register b present");
        assert_eq!(restored.getreg_bytes(), b"beta");
    }

    #[test]
    fn wshada_and_rshada_commands_round_trip_a_register() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let scratch = Scratch::new("dispatch");
        editor
            .editor_mut()
            .registers_mut()
            .yank_to(
                'q',
                RegisterContent::characterwise(b"dispatched").expect("register content"),
            )
            .expect("register q");
        exec.execute_line(&editor, &format!("wshada {}", scratch.path().display()))
            .expect("wshada runs");
        assert!(scratch.path().exists(), "wshada wrote the file");
        editor
            .editor_mut()
            .registers_mut()
            .set(
                'q',
                RegisterContent::characterwise(b"cleared").expect("register content"),
            )
            .expect("clear register q");
        exec.execute_line(&editor, &format!("rshada! {}", scratch.path().display()))
            .expect("rshada! runs");
        let inner = editor.editor();
        let restored = inner
            .registers()
            .get('q')
            .expect("valid name")
            .expect("register q restored");
        assert_eq!(restored.getreg_bytes(), b"dispatched");
    }

    #[test]
    fn rshada_reports_missing_files_and_corrupt_streams_as_upstream_errors() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let scratch = Scratch::new("errors");
        let missing = exec.execute_line(&editor, &format!("rshada {}", scratch.path().display()));
        let ExecError::Vim(exception) = missing.expect_err("missing file fails") else {
            panic!("expected a Vim exception");
        };
        assert!(
            exception
                .message()
                .contains("System error while opening ShaDa file"),
            "upstream E886 text: {}",
            exception.message()
        );
        assert!(
            exception.message().contains("No such file or directory"),
            "io detail is preserved: {}",
            exception.message()
        );

        std::fs::write(scratch.path(), b"\xa3abc").expect("scratch file");
        let corrupt = exec.execute_line(&editor, &format!("rshada {}", scratch.path().display()));
        let ExecError::Vim(exception) = corrupt.expect_err("corrupt stream fails") else {
            panic!("expected a Vim exception");
        };
        assert!(
            exception.message().contains(
                "Error while reading ShaDa file: expected positive integer at position 0"
            ),
            "upstream E576 text: {}",
            exception.message()
        );
    }
}
