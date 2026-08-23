//! `findfile()` and `finddir()`: the `'path'`-list search.
//!
//! Port of `src/nvim/file_search.c` — `find_file_in_path_option`,
//! `vim_findfile_init`, `vim_findfile`, `vim_findfile_stopdir`,
//! `ff_check_visited`, `ff_wc_equal`, `ff_path_in_stoplist` — together with
//! the wildcard expansion those call: `expand_wildcards` with
//! `EW_DIR|EW_ADDSLASH|EW_SILENT|EW_NOTWILD` over `do_path_expand`
//! (`src/nvim/path.c`) and `file_pat_to_reg_pat` (`src/nvim/fileio.c`).
//!
//! Upstream passes `curbuf->b_ffname` as the `rel_fname` used for a `.`
//! entry in `'path'`. This crate owns no buffer, so `rel_fname` is always
//! absent, which is exactly upstream's behaviour for an unnamed buffer: a
//! `.` entry then resolves against the current directory.

use std::fs;
use std::path::Path;

use ox_types::OxStr;

use crate::error::{EvalError, Result};
use crate::eval::RegexEngine;

/// `FF_MAX_STAR_STAR_EXPAND` (`file_search.c:155`).
const MAX_STAR_STAR_EXPAND: u8 = 30;

/// The `level` `findfilendir` passes to `vim_findfile_init` (`eval/fs.c:563`).
const MAX_LEVEL: i32 = 100;

/// `FINDFILE_DIR` versus `FINDFILE_FILE`; `FINDFILE_BOTH` has no caller here.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum FindWhat {
    /// Accept only non-directories.
    File,
    /// Accept only directories.
    Dir,
}

/// A file's identity, `FileID` from `os_fileid`.
#[cfg(unix)]
type FileId = (u64, u64);
#[cfg(not(unix))]
type FileId = String;

/// `ff_stack_T` (`file_search.c`).
struct StackEntry {
    fix_path: String,
    wc_path: Vec<u8>,
    level: i32,
    star_star_empty: bool,
    filearray: Option<Vec<String>>,
    filearray_cur: usize,
}

/// `ff_search_ctx_T` for one entry of the path list.
struct Context {
    start_dir: String,
    /// Index of upstream's `path_end` inside `start_dir`.
    path_end: usize,
    fix_path: String,
    wc_path: Vec<u8>,
    stopdirs: Option<Vec<String>>,
    stack: Vec<StackEntry>,
}

/// A running `findfile()`/`finddir()` search, yielding matches in upstream's
/// order. The visited lists are shared across path entries, because upstream
/// reuses one search context with `free_visited` false.
pub(crate) struct FindSearch<'a> {
    regex: &'a dyn RegexEngine,
    find_what: FindWhat,
    file_to_find: String,
    suffixes: Vec<String>,
    entries: std::vec::IntoIter<String>,
    context: Option<Context>,
    /// `ffsc_visited_list`: files already reported.
    visited: Vec<FileId>,
    /// `ffsc_dir_visited_list`: directories already searched, with the
    /// wildcard remainder they were searched with.
    dir_visited: Vec<(FileId, Vec<u8>)>,
    /// The absolute-or-`.`-relative branch answers only the first call.
    direct: Option<String>,
    first: bool,
}

impl<'a> FindSearch<'a> {
    /// `find_file_in_path_option` (`file_search.c:1419`) prepared for
    /// repeated calls: decide between the direct branch, which answers a
    /// name that is absolute or starts with `.`/`..`, and the path-list
    /// walk.
    pub(crate) fn new(
        regex: &'a dyn RegexEngine,
        name: &str,
        path: &str,
        suffixes: &str,
        find_what: FindWhat,
    ) -> Self {
        let file_to_find = expand_env(name);
        let suffixes = option_parts(suffixes, b",");
        let direct = if is_absolute(&file_to_find) || is_relative_to_curdir(&file_to_find) {
            direct_match(&file_to_find, &suffixes, find_what)
        } else {
            None
        };
        let entries = if direct.is_some() || is_absolute(&file_to_find) || is_relative_to_curdir(&file_to_find) {
            Vec::new()
        } else {
            option_parts(path, b" ,")
        };
        Self {
            regex,
            find_what,
            file_to_find,
            suffixes,
            entries: entries.into_iter(),
            context: None,
            visited: Vec::new(),
            dir_visited: Vec::new(),
            direct,
            first: true,
        }
    }

    /// The next match, or `None` once every path entry is exhausted.
    ///
    /// # Errors
    /// `E343` when a `**` count is not followed by a path separator, which is
    /// where `vim_findfile_init` fails.
    pub(crate) fn next_match(&mut self) -> Result<Option<String>> {
        if self.first {
            self.first = false;
            if let Some(found) = self.direct.take() {
                return Ok(Some(found));
            }
        }
        loop {
            if self.context.is_none() {
                let Some(entry) = self.entries.next() else { return Ok(None) };
                let (path, stopdirs) = split_stopdir(&entry);
                self.context = init_context(&path, stopdirs.as_deref())?;
                continue;
            }
            let found = self.step();
            if found.is_some() {
                return Ok(found);
            }
            self.context = None;
        }
    }

    /// `vim_findfile` (`file_search.c:601`): the downward stack walk wrapped
    /// in the upward-search loop, resuming from the state left by the
    /// previous match.
    fn step(&mut self) -> Option<String> {
        loop {
            while let Some(mut entry) = self.context.as_mut().and_then(|context| context.stack.pop()) {
                if entry.filearray.is_none() && !self.mark_dir_visited(&entry) {
                    continue;
                }
                if entry.level <= 0 {
                    continue;
                }
                let rest = self.expand_entry(&mut entry);
                if let Some(found) = self.check_stage(&mut entry, &rest) {
                    return Some(found);
                }
                self.descend_star_star(&entry);
            }
            if !self.ascend() {
                return None;
            }
        }
    }

    /// The dir half of `ff_check_visited` (`file_search.c:1151`): reject a
    /// directory already searched with an equivalent wildcard remainder, and
    /// reject one whose identity cannot be read at all.
    fn mark_dir_visited(&mut self, entry: &StackEntry) -> bool {
        let Some(id) = file_id(&entry.fix_path) else { return false };
        if self
            .dir_visited
            .iter()
            .any(|(candidate, wildcards)| *candidate == id && wc_equal(wildcards, &entry.wc_path))
        {
            return false;
        }
        self.dir_visited.push((id, entry.wc_path.clone()));
        true
    }

    /// `file_search.c:686-816`: build the directory pattern for this stack
    /// entry, consume one wildcard component from its remainder, and expand
    /// it. Returns the wildcard remainder still to be handled.
    fn expand_entry(&mut self, entry: &mut StackEntry) -> Vec<u8> {
        if entry.filearray.is_some() {
            return Vec::new();
        }
        let start_dir = self.context.as_ref().map_or("", |context| context.start_dir.as_str());
        let mut pattern = String::new();
        if !is_absolute(&entry.fix_path) && !start_dir.is_empty() {
            pattern.push_str(start_dir);
            if !after_pathsep(start_dir) {
                pattern.push('/');
            }
        }
        let fix_had_sep = after_pathsep(&entry.fix_path);
        pattern.push_str(&entry.fix_path);
        if !fix_had_sep {
            pattern.push('/');
        }

        let mut cursor = 0;
        let mut second = None;
        if !entry.wc_path.is_empty() {
            if entry.wc_path.starts_with(b"**") {
                // The byte after "**" is a binary descent counter, not text.
                if entry.wc_path.get(2).copied().unwrap_or(0) > 0 {
                    entry.wc_path[2] -= 1;
                    pattern.push('*');
                }
                if entry.wc_path.get(2).copied().unwrap_or(0) == 0 {
                    entry.wc_path.drain(..3.min(entry.wc_path.len()));
                } else {
                    cursor = 3;
                }
                if !entry.star_star_empty {
                    entry.star_star_empty = true;
                    second = Some(entry.fix_path.clone());
                }
            }
            while let Some(byte) = entry.wc_path.get(cursor) {
                if *byte == b'/' {
                    cursor += 1;
                    break;
                }
                pattern.push(char::from(*byte));
                cursor += 1;
            }
        }
        let rest = entry.wc_path.get(cursor..).unwrap_or_default().to_vec();

        entry.filearray = Some(if path_with_url(&pattern) {
            vec![pattern]
        } else {
            expand_directories(self.regex, &pattern, second.as_deref())
        });
        entry.filearray_cur = 0;
        rest
    }

    /// `file_search.c:818-930`: with no wildcards left, test the searched
    /// name (and each `'suffixesadd'` variant) in every expanded directory;
    /// otherwise push those directories for the next component.
    fn check_stage(&mut self, entry: &mut StackEntry, rest: &[u8]) -> Option<String> {
        let expanded = entry.filearray.clone().unwrap_or_default();
        if !rest.is_empty() {
            for directory in expanded.iter().skip(entry.filearray_cur) {
                if !is_dir(directory) {
                    continue;
                }
                self.push(StackEntry {
                    fix_path: directory.clone(),
                    wc_path: rest.to_vec(),
                    level: entry.level - 1,
                    star_star_empty: false,
                    filearray: None,
                    filearray_cur: 0,
                });
            }
            entry.filearray_cur = 0;
            return None;
        }
        for (index, directory) in expanded.iter().enumerate().skip(entry.filearray_cur) {
            if !path_with_url(directory) && !is_dir(directory) {
                continue;
            }
            let mut base = directory.clone();
            if !after_pathsep(&base) {
                base.push('/');
            }
            base.push_str(&self.file_to_find);
            let suffixes = self.suffixes.clone();
            for suffix in std::iter::once(String::new()).chain(suffixes) {
                let candidate = format!("{base}{suffix}");
                if !self.accepts(&candidate) {
                    continue;
                }
                entry.filearray_cur = index + 1;
                let resumed = StackEntry {
                    fix_path: entry.fix_path.clone(),
                    wc_path: entry.wc_path.clone(),
                    level: entry.level,
                    star_star_empty: entry.star_star_empty,
                    filearray: entry.filearray.clone(),
                    filearray_cur: entry.filearray_cur,
                };
                self.push(resumed);
                return Some(report(&candidate));
            }
        }
        entry.filearray_cur = 0;
        None
    }

    /// The file half of `ff_check_visited`, guarded by the existence and
    /// type test of `file_search.c:853-862`.
    fn accepts(&mut self, candidate: &str) -> bool {
        if path_with_url(candidate) {
            return true;
        }
        if !Path::new(candidate).exists() || (self.find_what == FindWhat::Dir) != is_dir(candidate) {
            return false;
        }
        let Some(id) = file_id(candidate) else { return false };
        if self.visited.contains(&id) {
            return false;
        }
        self.visited.push(id);
        true
    }

    /// `file_search.c:932-951`: while the remainder still starts with `**`,
    /// push every expanded subdirectory to be searched one level deeper.
    fn descend_star_star(&mut self, entry: &StackEntry) {
        if !entry.wc_path.starts_with(b"**") {
            return;
        }
        let expanded = entry.filearray.clone().unwrap_or_default();
        for directory in expanded.iter().skip(entry.filearray_cur) {
            if path_equal(directory, &entry.fix_path) || !is_dir(directory) {
                continue;
            }
            self.push(StackEntry {
                fix_path: directory.clone(),
                wc_path: entry.wc_path.clone(),
                level: entry.level - 1,
                star_star_empty: true,
                filearray: None,
                filearray_cur: 0,
            });
        }
    }

    fn push(&mut self, entry: StackEntry) {
        if let Some(context) = self.context.as_mut() {
            context.stack.push(entry);
        }
    }

    /// `file_search.c:957-1014`: nothing was found below, so cut the last
    /// component off the starting directory and search again, unless that
    /// directory is in the stop list or nothing is left.
    fn ascend(&mut self) -> bool {
        let Some(context) = self.context.as_mut() else { return false };
        let Some(stopdirs) = context.stopdirs.as_ref() else { return false };
        if context.start_dir.is_empty() {
            return false;
        }
        let end = context.start_dir.len();
        let reached = context.path_end + usize::from(context.path_end < end);
        if in_stoplist(&context.start_dir[..reached.min(end)], stopdirs) {
            return false;
        }
        let bytes = context.start_dir.as_bytes();
        let mut cut = context.path_end;
        while cut > 0 && cut < bytes.len() && bytes[cut] == b'/' {
            cut -= 1;
        }
        while cut > 0 && bytes[cut - 1] != b'/' {
            cut -= 1;
        }
        context.start_dir.truncate(cut);
        context.path_end = cut.saturating_sub(1);
        if context.start_dir.is_empty() {
            return false;
        }
        let mut restart = context.start_dir.clone();
        if !after_pathsep(&restart) {
            restart.push('/');
        }
        restart.push_str(&context.fix_path);
        let entry = StackEntry {
            fix_path: restart,
            wc_path: context.wc_path.clone(),
            level: MAX_LEVEL,
            star_star_empty: false,
            filearray: None,
            filearray_cur: 0,
        };
        context.stack.push(entry);
        true
    }
}

/// `vim_findfile_init` (`file_search.c:248`) with `rel_fname` absent: pick
/// the starting directory, split the entry into its fixed prefix and its
/// wildcard remainder, encode each `**` count as a binary descent counter,
/// and seed the stack.
fn init_context(path: &str, stopdirs: Option<&str>) -> Result<Option<Context>> {
    let mut start_dir = String::new();
    if path.is_empty() || !is_absolute(path) {
        let Ok(current) = std::env::current_dir() else { return Ok(None) };
        start_dir = current.to_string_lossy().into_owned();
    }

    let mut fix_path;
    let mut wc_path = Vec::new();
    if let Some(offset) = path.find('*') {
        fix_path = path[..offset].to_owned();
        let tail = path[offset..].as_bytes();
        let mut cursor = 0;
        while cursor < tail.len() {
            if !tail[cursor..].starts_with(b"**") {
                wc_path.push(tail[cursor]);
                cursor += 1;
                continue;
            }
            wc_path.extend_from_slice(b"**");
            cursor += 2;
            let digits_start = cursor;
            while tail.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
            }
            // `getdigits(&errpt, false, 255)`: no digits leaves the cursor
            // untouched, and anything too large reads as the default 255.
            let digits = &tail[digits_start..cursor];
            let count = if digits.is_empty() {
                None
            } else {
                Some(std::str::from_utf8(digits).ok().and_then(|text| text.parse::<i64>().ok()).unwrap_or(255))
            };
            match count {
                Some(count) if count > 0 && count < 255 => wc_path.push(count as u8),
                Some(0) => {
                    wc_path.truncate(wc_path.len() - 2);
                }
                _ => wc_path.push(MAX_STAR_STAR_EXPAND),
            }
            if tail.get(cursor).is_some_and(|byte| *byte != b'/') {
                return Err(EvalError::new(
                    "E343",
                    0,
                    "Invalid path: '**[number]' must be at the end of the path or be followed by '/'.",
                ));
            }
        }
    } else {
        fix_path = path.to_owned();
    }

    if start_dir.is_empty() {
        start_dir = fix_path.clone();
        fix_path.clear();
    }

    let mut expanded = start_dir.clone();
    if !after_pathsep(&expanded) {
        expanded.push('/');
    }
    let fix_had_sep = after_pathsep(&fix_path);
    if is_dir(&format!("{expanded}{fix_path}")) {
        if !fix_path.is_empty() {
            expanded.push_str(&fix_path);
            if !fix_had_sep {
                expanded.push('/');
            }
        }
    } else {
        let tail = path_tail_index(&fix_path);
        let mut kept = fix_path.len();
        if tail > 0 {
            kept = tail - 1;
            // Never walk into "..", which would restart the search upwards.
            if fix_path.starts_with("..") && (kept == 2 || fix_path.as_bytes().get(2) == Some(&b'/')) {
                return Ok(None);
            }
            expanded.push_str(&fix_path[..kept]);
            if !fix_had_sep {
                expanded.push('/');
            }
        }
        if !wc_path.is_empty() {
            let mut merged = fix_path.as_bytes()[kept..].to_vec();
            merged.extend_from_slice(&wc_path);
            wc_path = merged;
        }
    }

    let stack = vec![StackEntry {
        fix_path: expanded,
        wc_path: wc_path.clone(),
        level: MAX_LEVEL,
        star_star_empty: false,
        filearray: None,
        filearray_cur: 0,
    }];
    Ok(Some(Context {
        path_end: start_dir.len(),
        start_dir,
        fix_path,
        wc_path,
        stopdirs: stopdirs.map(parse_stopdirs),
        stack,
    }))
}

/// `vim_findfile_stopdir` (`file_search.c:542`): split an entry at its first
/// unescaped `;`, unescaping `\;` in the part before it.
fn split_stopdir(entry: &str) -> (String, Option<String>) {
    let bytes = entry.as_bytes();
    let mut path = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b';' {
            let stopdirs = String::from_utf8_lossy(&bytes[cursor + 1..]).into_owned();
            return (String::from_utf8_lossy(&path).into_owned(), Some(stopdirs));
        }
        if bytes[cursor] == b'\\' && bytes.get(cursor + 1) == Some(&b';') {
            path.push(b';');
            cursor += 2;
            continue;
        }
        path.push(bytes[cursor]);
        cursor += 1;
    }
    (String::from_utf8_lossy(&path).into_owned(), None)
}

/// `file_search.c:343-377`: split the stop-directory string on `;`, making
/// each relative entry absolute. An empty entry means "ascend to the top".
fn parse_stopdirs(stopdirs: &str) -> Vec<String> {
    let trimmed = stopdirs.trim_start_matches(';');
    trimmed
        .split(';')
        .map(|entry| {
            if entry.is_empty() || is_absolute(entry) {
                entry.to_owned()
            } else {
                full_name(entry)
            }
        })
        .collect()
}

/// `ff_path_in_stoplist` (`file_search.c:1314`).
fn in_stoplist(path: &str, stopdirs: &[String]) -> bool {
    let bytes = path.as_bytes();
    let mut length = bytes.len();
    while length > 1 && bytes[length - 1] == b'/' {
        length -= 1;
    }
    if length == 0 {
        return true;
    }
    let path = &bytes[..length];
    stopdirs.iter().any(|stop| {
        let stop = stop.as_bytes();
        // `strncmp(stop, path, path_len) == 0`, where a short stop entry
        // compares its terminator against a path byte and cannot match.
        let matched = path.iter().enumerate().all(|(index, byte)| stop.get(index) == Some(byte));
        matched && (stop.len() <= length || stop[length] == b'/')
    })
}

/// `ff_wc_equal` (`file_search.c:1116`): equal character by character,
/// except that a difference is tolerated once both previous characters were
/// `*`, so `**` counters of different depths compare equal.
fn wc_equal(left: &[u8], right: &[u8]) -> bool {
    if left == right {
        return true;
    }
    let mut previous = (0u8, 0u8);
    let mut index = 0;
    while index < left.len() && index < right.len() {
        if left[index] != right[index] && !(previous.0 == b'*' && previous.1 == b'*') {
            return false;
        }
        previous = (previous.1, left[index]);
        index += 1;
    }
    left.len() == right.len()
}

/// `find_file_in_path_option`'s direct branch (`file_search.c:1465-1524`):
/// with `rel_fname` absent the name is tested as given, then with each
/// `'suffixesadd'` part appended.
fn direct_match(name: &str, suffixes: &[String], find_what: FindWhat) -> Option<String> {
    for suffix in std::iter::once(String::new()).chain(suffixes.iter().cloned()) {
        let candidate = format!("{name}{suffix}");
        if Path::new(&candidate).exists() && (find_what == FindWhat::Dir) == is_dir(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// `expand_wildcards(n, patterns, EW_DIR|EW_ADDSLASH|EW_SILENT|EW_NOTWILD)`
/// (`path.c` `gen_expand_wildcards`): expand each pattern in turn, sorting
/// only the matches that pattern contributed, and keep directories with a
/// trailing separator. A pattern without wildcards is added as-is if it
/// exists and is a directory.
fn expand_directories(regex: &dyn RegexEngine, pattern: &str, second: Option<&str>) -> Vec<String> {
    let mut found = Vec::new();
    for pattern in std::iter::once(pattern).chain(second) {
        let expanded = expand_env(pattern);
        if has_wildcard(&expanded) {
            let start = found.len();
            path_expand(regex, &mut found, &expanded);
            found[start..].sort_unstable();
        } else if let Some(entry) = add_directory(&expanded) {
            found.push(entry);
        }
    }
    found
}

/// `do_path_expand` (`path.c`) restricted to what `vim_findfile` asks of it:
/// no `**` (already consumed by the caller), matching one wildcard
/// component at a time and recursing for the rest of the pattern.
fn path_expand(regex: &dyn RegexEngine, found: &mut Vec<String>, pattern: &str) {
    let bytes = pattern.as_bytes();
    // The first component containing a wildcard, delimited by separators.
    let mut component_start = 0;
    let mut component_end = None;
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' && cursor + 1 < bytes.len() {
            cursor += 2;
            continue;
        }
        if bytes[cursor] == b'/' {
            if component_end.is_some() {
                break;
            }
            component_start = cursor + 1;
        } else if matches!(bytes[cursor], b'*' | b'?' | b'[' | b'{') {
            component_end = Some(cursor);
        }
        cursor += 1;
    }
    if component_end.is_none() {
        if let Some(entry) = add_directory(pattern) {
            found.push(entry);
        }
        return;
    }
    let directory = &pattern[..component_start];
    let component = remove_backslashes(&pattern[component_start..cursor]);
    let remainder = &pattern[cursor..];
    let Some(matcher) = glob_to_regex(&component) else { return };
    let starts_with_dot = component.starts_with('.');

    let scan_root = if directory.is_empty() { "." } else { directory };
    let Ok(entries) = fs::read_dir(scan_root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !starts_with_dot {
            continue;
        }
        // `EW_NOTWILD` also accepts a literal match when the pattern does
        // not compile as a regular expression.
        let literal = name == component;
        let matched = literal
            || regex.is_match(&OxStr::from(name.as_str()), &OxStr::from(matcher.as_str()), false).unwrap_or(false);
        if !matched {
            continue;
        }
        let candidate = format!("{directory}{name}{remainder}");
        if remainder.contains(['*', '?', '[', '{']) {
            path_expand(regex, found, &candidate);
        } else if let Some(entry) = add_directory(&remove_backslashes(&candidate)) {
            found.push(entry);
        }
    }
}

/// `addfile` (`path.c`) with `EW_DIR|EW_ADDSLASH`: keep existing
/// directories only, with a trailing separator.
fn add_directory(path: &str) -> Option<String> {
    if !Path::new(path).exists() || !is_dir(path) {
        return None;
    }
    let mut entry = path.to_owned();
    if !after_pathsep(&entry) {
        entry.push('/');
    }
    Some(entry)
}

/// `file_pat_to_reg_pat(pat, end, NULL, false)` (`fileio.c`) on a platform
/// without `BACKSLASH_IN_FILENAME`. `None` mirrors upstream's E219/E220
/// failure, which `do_path_expand` treats as "expand nothing".
fn glob_to_regex(pattern: &str) -> Option<String> {
    let bytes = pattern.as_bytes();
    if bytes.is_empty() {
        return Some("^$".to_owned());
    }
    let mut converted = String::with_capacity(bytes.len() * 2 + 2);
    // A leading run of '*' drops the '^' anchor; a trailing run drops '$'.
    let mut start = 0;
    if bytes[0] == b'*' {
        while bytes.get(start) == Some(&b'*') && start < bytes.len() - 1 {
            start += 1;
        }
    } else {
        converted.push('^');
    }
    let mut end = bytes.len();
    let mut add_dollar = true;
    if bytes[end - 1] == b'*' {
        while end - 1 > start && bytes[end - 1] == b'*' {
            end -= 1;
        }
        add_dollar = false;
    }
    let mut nested = 0i32;
    let mut cursor = start;
    while cursor < end && nested >= 0 {
        let byte = bytes[cursor];
        match byte {
            b'*' => {
                converted.push_str(".*");
                while bytes.get(cursor + 1) == Some(&b'*') {
                    cursor += 1;
                }
            }
            b'.' | b'~' => {
                converted.push('\\');
                converted.push(char::from(byte));
            }
            b'?' => converted.push('.'),
            b'\\' => {
                let Some(next) = bytes.get(cursor + 1).copied() else { break };
                cursor += 1;
                match next {
                    b'?' => converted.push('?'),
                    b',' | b'%' | b'#' | b' ' | b'\t' | b'{' | b'}' => converted.push(char::from(next)),
                    b'\\' if bytes.get(cursor + 1) == Some(&b'\\') && bytes.get(cursor + 2) == Some(&b'{') => {
                        converted.push_str("\\{");
                        cursor += 2;
                    }
                    _ => {
                        converted.push('\\');
                        converted.push(char::from(next));
                    }
                }
            }
            b'{' => {
                converted.push_str("\\(");
                nested += 1;
            }
            b'}' => {
                converted.push_str("\\)");
                nested -= 1;
            }
            b',' => {
                if nested > 0 {
                    converted.push_str("\\|");
                } else {
                    converted.push(',');
                }
            }
            _ => converted.push(char::from(byte)),
        }
        cursor += 1;
    }
    if add_dollar {
        converted.push('$');
    }
    if nested == 0 { Some(converted) } else { None }
}

/// `backslash_halve`: drop a backslash that escapes the next character.
fn remove_backslashes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' && cursor + 1 < bytes.len() {
            output.push(bytes[cursor + 1]);
            cursor += 2;
            continue;
        }
        output.push(bytes[cursor]);
        cursor += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// `path_has_wildcard(p, false)` with `PATH_ESC_WILDCARDS` = `*?[{`.
fn has_wildcard(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' && cursor + 1 < bytes.len() {
            cursor += 2;
            continue;
        }
        if matches!(bytes[cursor], b'*' | b'?' | b'[' | b'{') {
            return true;
        }
        cursor += 1;
    }
    false
}

/// `copy_option_part` over a whole option value: split on `sep_chars`,
/// honouring a backslash before a separator, then skip one separator plus
/// following spaces (`skip_to_option_part`).
fn option_parts(value: &str, separators: &[u8]) -> Vec<String> {
    let bytes = value.as_bytes();
    let mut parts = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let mut part = Vec::new();
        if bytes[cursor] == b'.' {
            part.push(b'.');
            cursor += 1;
        }
        while cursor < bytes.len() && !separators.contains(&bytes[cursor]) {
            if bytes[cursor] == b'\\' && bytes.get(cursor + 1).is_some_and(|byte| separators.contains(byte)) {
                cursor += 1;
            }
            part.push(bytes[cursor]);
            cursor += 1;
        }
        parts.push(String::from_utf8_lossy(&part).into_owned());
        if cursor < bytes.len() && bytes[cursor] != b',' {
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b',') {
            cursor += 1;
        }
        while bytes.get(cursor) == Some(&b' ') {
            cursor += 1;
        }
    }
    parts
}

/// `expand_env`: `~` at the start becomes `$HOME`, and `$NAME` or
/// `${NAME}` becomes the environment value, or is left alone when unset.
fn expand_env(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    if bytes.first() == Some(&b'~') && (bytes.len() == 1 || bytes[1] == b'/') {
        if let Some(home) = std::env::var_os("HOME") {
            output.push_str(&home.to_string_lossy());
            cursor = 1;
        }
    }
    while cursor < bytes.len() {
        if bytes[cursor] != b'$' {
            // `$` is ASCII, so every run between two of them starts and ends
            // on a UTF-8 boundary and copies over as-is. Copying byte by byte
            // through `char::from` would re-encode each byte as its Latin-1
            // scalar and corrupt a non-ASCII path component.
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor] != b'$' {
                cursor += 1;
            }
            output.push_str(&text[start..cursor]);
            continue;
        }
        let braced = bytes.get(cursor + 1) == Some(&b'{');
        let name_start = cursor + 1 + usize::from(braced);
        let mut name_end = name_start;
        while bytes.get(name_end).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') {
            name_end += 1;
        }
        let closed = !braced || bytes.get(name_end) == Some(&b'}');
        let name = &text[name_start..name_end];
        match std::env::var_os(name) {
            Some(value) if closed && !name.is_empty() => {
                output.push_str(&value.to_string_lossy());
                cursor = name_end + usize::from(braced);
            }
            _ => {
                output.push('$');
                cursor += 1;
            }
        }
    }
    output
}

/// `simplify_filename` then `path_shorten_fname` against the current
/// directory (`file_search.c:881-893`), which is what makes a match under
/// the current directory come back relative.
fn report(candidate: &str) -> String {
    if path_with_url(candidate) {
        return candidate.to_owned();
    }
    let simplified = crate::path_builtins::simplify_name(candidate);
    let Ok(current) = std::env::current_dir() else { return simplified };
    let current = current.to_string_lossy();
    let Some(tail) = simplified.strip_prefix(current.as_ref()) else { return simplified };
    if current.ends_with('/') {
        return tail.to_owned();
    }
    match tail.strip_prefix('/') {
        Some(tail) => tail.trim_start_matches('/').to_owned(),
        None => simplified,
    }
}

/// `vim_FullName(name, ..., false)` for a relative stop directory.
fn full_name(name: &str) -> String {
    if is_absolute(name) {
        return name.to_owned();
    }
    let Ok(current) = std::env::current_dir() else { return name.to_owned() };
    crate::path_builtins::simplify_name(&format!("{}/{name}", current.to_string_lossy()))
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
}

/// `rel_to_curdir` (`file_search.c:1459-1464`): `.`, `..`, `./x`, `../x`.
fn is_relative_to_curdir(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.first() != Some(&b'.') {
        return false;
    }
    match bytes.get(1) {
        None | Some(b'/') => true,
        Some(b'.') => matches!(bytes.get(2), None | Some(b'/')),
        Some(_) => false,
    }
}

fn after_pathsep(path: &str) -> bool {
    path.ends_with('/')
}

/// `path_tail`, as the index just past the last separator.
fn path_tail_index(path: &str) -> usize {
    path.rfind('/').map_or(0, |offset| offset + 1)
}

/// `path_with_url`: a scheme followed by `://`.
fn path_with_url(path: &str) -> bool {
    let scheme = path.split("://").next().unwrap_or(path);
    scheme.len() < path.len()
        && !scheme.is_empty()
        && scheme.starts_with(|character: char| character.is_ascii_alphabetic())
        && scheme.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '.' | '-'))
}

/// `path_equal(a, b, kPathCmpLiteral)`: byte equality after trimming a
/// trailing separator, which is how upstream avoids re-pushing a directory
/// onto itself.
fn path_equal(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

fn is_dir(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// `os_fileid`.
#[cfg(unix)]
fn file_id(path: &str) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_id(path: &str) -> Option<FileId> {
    fs::canonicalize(path).ok().map(|path| path.to_string_lossy().into_owned())
}
