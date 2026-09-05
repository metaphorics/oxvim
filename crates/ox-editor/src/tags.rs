//! Tags-file lookup for `:tag` and `taglist()`.
//!
//! A tags file is tab-separated `name\tfile\tcmd` records (`tag.c`
//! `parse_tag_line`). Lines starting with `!_` are headers and are ignored.

use std::path::{Path, PathBuf};

use ox_regex::{Magic, Text as RegexText, compile as compile_regex};
use ox_text::Position;
use ox_types::{BufHandle, Typval};

use crate::script::FileIO;

/// Maximum tag-stack depth (`TAGSTACKSIZE` in `tag.c`).
pub const TAGSTACK_SIZE: usize = 20;

/// One tags-file match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TagMatch {
    /// Tag name.
    pub name: String,
    /// File named by the tags record.
    pub filename: PathBuf,
    /// Ex command or line number used to land in that file.
    pub cmd: String,
    /// Extra `key:value` fields (`kind`, `file`, `signature`, …).
    pub fields: Vec<(String, String)>,
}

/// One tag-stack entry (`taggy_T`).
#[derive(Clone, Debug, PartialEq)]
pub struct TagStackItem {
    /// Tag name used for the jump.
    pub tagname: String,
    /// Buffer that held the cursor before the jump.
    pub from_bufnr: BufHandle,
    /// 1-based line of the origin.
    pub from_lnum: usize,
    /// 1-based column of the origin (`getpos()` column).
    pub from_col: usize,
    /// `coladd` of the origin.
    pub from_off: i64,
    /// Destination buffer after the jump, when known.
    pub bufnr: Option<BufHandle>,
    /// 1-based match index among tags of this name.
    pub matchnr: usize,
    /// Caller-owned data carried by `gettagstack()` and `settagstack()`.
    pub user_data: Option<Typval>,
}

/// Per-window tag stack (`w_tagstack` / `w_tagstackidx`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TagStack {
    items: Vec<TagStackItem>,
    /// 1-based current index; `len + 1` means past the newest entry.
    curidx: usize,
}
/// Boundary reached while moving through a tag stack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TagStackBoundary {
    /// The stack has no entries.
    Empty,
    /// The requested move crossed the oldest entry.
    Bottom,
    /// The requested move started past the newest entry.
    Top,
}

impl TagStack {
    /// Empty stack with `curidx` at 1.
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            curidx: 1,
        }
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the stack has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 1-based current index, clamped to `1..=len+1`.
    #[must_use]
    pub fn curidx(&self) -> usize {
        self.curidx.clamp(1, self.items.len() + 1)
    }

    /// Stack entries in order.
    #[must_use]
    pub fn items(&self) -> &[TagStackItem] {
        &self.items
    }

    /// Sets `curidx`, clamping to `1..=len+1`.
    pub fn set_curidx(&mut self, idx: i64) {
        let max = self.items.len() + 1;
        self.curidx = if idx < 1 {
            1
        } else {
            usize::try_from(idx).unwrap_or(max).min(max)
        };
    }

    /// Replaces the stack.
    pub fn replace(&mut self, items: Vec<TagStackItem>) {
        self.items = items;
        if self.items.len() > TAGSTACK_SIZE {
            let drop = self.items.len() - TAGSTACK_SIZE;
            self.items.drain(..drop);
        }
        self.curidx = self.items.len() + 1;
    }

    /// Appends items, dropping from the front past [`TAGSTACK_SIZE`].
    pub fn append(&mut self, items: Vec<TagStackItem>) {
        self.items.extend(items);
        if self.items.len() > TAGSTACK_SIZE {
            let drop = self.items.len() - TAGSTACK_SIZE;
            self.items.drain(..drop);
        }
        self.curidx = self.items.len() + 1;
    }

    /// Truncates at `curidx` then appends.
    pub fn truncate_and_push(&mut self, items: Vec<TagStackItem>) {
        let keep = self.curidx().saturating_sub(1).min(self.items.len());
        self.items.truncate(keep);
        self.append(items);
    }

    /// Pushes one jump, truncating anything past the current index first.
    pub fn push_jump(&mut self, item: TagStackItem) {
        let keep = self.curidx().saturating_sub(1).min(self.items.len());
        self.items.truncate(keep);
        self.append(vec![item]);
    }

    /// Current item when `curidx` points at an entry.
    #[must_use]
    pub fn current(&self) -> Option<&TagStackItem> {
        let idx = self.curidx();
        if idx == 0 || idx > self.items.len() {
            self.items.last()
        } else {
            self.items.get(idx - 1)
        }
    }

    /// Mutable current item.
    pub fn current_mut(&mut self) -> Option<&mut TagStackItem> {
        let idx = self.curidx();
        if idx == 0 || idx > self.items.len() {
            self.items.last_mut()
        } else {
            self.items.get_mut(idx - 1)
        }
    }

    /// Moves `count` entries toward the oldest entry and returns the target.
    ///
    /// # Errors
    ///
    /// Returns [`TagStackBoundary::Empty`] when the stack has no entries,
    /// [`TagStackBoundary::Bottom`] when the move would cross the oldest
    /// entry, and [`TagStackBoundary::Top`] when it starts past the newest
    /// entry.
    pub fn pop(&mut self, count: usize) -> Result<TagStackItem, TagStackBoundary> {
        if self.items.is_empty() {
            return Err(TagStackBoundary::Empty);
        }
        let Some(idx) = self.curidx().checked_sub(count) else {
            self.curidx = 1;
            return Err(TagStackBoundary::Bottom);
        };
        if idx == 0 {
            self.curidx = 1;
            return Err(TagStackBoundary::Bottom);
        }
        if idx > self.items.len() {
            return Err(TagStackBoundary::Top);
        }
        self.curidx = idx;
        Ok(self.items[idx - 1].clone())
    }

    /// Removes origin marks that refer to a wiped buffer.
    pub fn forget_buffer(&mut self, buffer: BufHandle) {
        let stack_idx = self.curidx().saturating_sub(1);
        let mut index = 0usize;
        let mut removed_before = 0usize;
        self.items.retain(|item| {
            let remove = item.from_bufnr == buffer;
            if remove && index < stack_idx {
                removed_before = removed_before.saturating_add(1);
            }
            index = index.saturating_add(1);
            !remove
        });
        self.curidx = self
            .curidx
            .saturating_sub(removed_before)
            .clamp(1, self.items.len().saturating_add(1));
    }
}

/// Parses a tags file body into matches for `needle`.
#[must_use]
pub fn parse_matches(text: &str, needle: &str) -> Vec<TagMatch> {
    parse_matches_with(text, needle, 0)
}

/// Parses a tags file body, honouring `'taglength'` for non-regex needles.
#[must_use]
pub fn parse_matches_with(text: &str, needle: &str, taglength: usize) -> Vec<TagMatch> {
    parse_matches_icase(text, needle, taglength, false).unwrap_or_default()
}

fn parse_matches_icase(
    text: &str,
    needle: &str,
    taglength: usize,
    ignorecase: bool,
) -> Result<Vec<TagMatch>, ()> {
    let records = parse_records(text, true)?;
    let Some(needle) = TagNeedle::new(needle, ignorecase) else {
        return Ok(Vec::new());
    };
    Ok(records
        .into_iter()
        .filter(|record| needle.matches(&record.name, taglength, ignorecase))
        .collect())
}

fn parse_records(text: &str, reject_empty_cmd: bool) -> Result<Vec<TagMatch>, ()> {
    let mut records = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with("!_") {
            continue;
        }
        let record = parse_record(line)?;
        if reject_empty_cmd && record.cmd.is_empty() {
            return Err(());
        }
        records.push(record);
    }
    Ok(records)
}

fn parse_record(line: &str) -> Result<TagMatch, ()> {
    let Some((name, rest)) = line.split_once('\t') else {
        return Err(());
    };
    let Some((filename, rest)) = rest.split_once('\t') else {
        return Err(());
    };
    let terminator = rest.match_indices(';').find_map(|(offset, _)| {
        let after = &rest[offset + 1..];
        after
            .trim_start()
            .starts_with('"')
            .then_some((offset, after))
    });
    let (cmd, extra) = terminator.map_or((rest, ""), |(offset, after)| {
        (
            &rest[..offset],
            after.trim_start().strip_prefix('"').unwrap_or_default(),
        )
    });
    let fields = extra
        .split('\t')
        .filter(|field| !field.is_empty())
        .map(|field| match field.split_once(':') {
            Some((key, value)) => (key.to_owned(), value.to_owned()),
            None => ("kind".to_owned(), field.to_owned()),
        })
        .collect();
    Ok(TagMatch {
        name: name.to_owned(),
        filename: PathBuf::from(crate::excmd_exec::expand_env_esc(filename)),
        cmd: cmd.to_owned(),
        fields,
    })
}

/// A tag needle compiled once per search: a `/pattern/` needle holds its
/// regex program (upstream `search_regcomp` before the record loop, tag.c
/// `find_tags`), a literal needle holds its case-folded form.
enum TagNeedle {
    Pattern(ox_regex::Prog),
    Literal(String),
}

impl TagNeedle {
    /// `None` mirrors `name_matches`' invalid-regex behavior: no record can
    /// match a needle that failed to compile.
    fn new(needle: &str, ignorecase: bool) -> Option<Self> {
        if let Some(stripped) = needle.strip_prefix('/') {
            let mut pattern = stripped.to_owned();
            if pattern.ends_with('/') {
                pattern.pop();
            }
            // Upstream's tag pattern search honors 'ignorecase': the
            // regmatch takes `rm_ic` from `p_ic` (tag.c:2248-2249) and the
            // regexp match runs through that regmatch (tag.c:1755-1772).
            // Inject the case modifier like the regular search path does;
            // an explicit `\c`/`\C` in the pattern always wins.
            let pattern = crate::search::pattern_with_case(&pattern, ignorecase);
            return compile_regex(&pattern, Magic::Magic)
                .ok()
                .map(Self::Pattern);
        }
        Some(Self::Literal(if ignorecase {
            needle.to_ascii_lowercase()
        } else {
            needle.to_owned()
        }))
    }

    fn matches(&self, name: &str, taglength: usize, ignorecase: bool) -> bool {
        match self {
            Self::Pattern(program) => {
                ox_regex::exec(program, &RegexText::new(name.to_owned())).is_some()
            }
            Self::Literal(folded) => {
                let left = if ignorecase {
                    name.to_ascii_lowercase()
                } else {
                    name.to_owned()
                };
                if taglength > 0 {
                    let n = taglength.min(left.len()).min(folded.len());
                    return left.as_bytes()[..n] == folded.as_bytes()[..n];
                }
                left == *folded
            }
        }
    }
}

fn sorted_header(text: &str) -> Option<u8> {
    text.lines().find_map(|line| {
        line.strip_prefix("!_TAG_FILE_SORTED\t")
            .and_then(|rest| rest.bytes().next())
    })
}

/// Byte-wise sortedness of tag names (the text before the first TAB)
/// without building records.
fn names_sorted(text: &str) -> bool {
    let mut previous: Option<&str> = None;
    for line in text.lines() {
        if line.is_empty() || line.starts_with("!_") {
            continue;
        }
        let name = match line.split_once('\t') {
            Some((name, _)) => name,
            None => line,
        };
        if let Some(previous) = previous
            && previous.as_bytes() > name.as_bytes()
        {
            return false;
        }
        previous = Some(name);
    }
    true
}

/// E431 layout gate for the binary path: with empty commands accepted,
/// `parse_record` rejects only lines with fewer than two TABs, so validate
/// that without building records (upstream reports E431 from the read loop,
/// tag.c:2008-2010).
fn layout_ok(text: &str) -> bool {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with("!_"))
        .all(|line| line.matches('\t').count() >= 2)
}

/// End offset of the line starting at `start`, excluding the newline.
fn line_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |offset| start + offset)
}

/// Containing line of byte `midpoint`, skipping `!_` header lines forward
/// like `findtags_get_next_line`; `None` past `high` or end of file.
fn probe_line(bytes: &[u8], midpoint: usize, high: usize) -> Option<(usize, usize)> {
    let mut start = bytes[..midpoint]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |offset| offset + 1);
    let mut end = line_end(bytes, start);
    while bytes[start..end].starts_with(b"!_") {
        start = end.saturating_add(1);
        if start >= high || start >= bytes.len() {
            return None;
        }
        end = line_end(bytes, start);
    }
    if start >= high || start >= bytes.len() {
        return None;
    }
    Some((start, end))
}

/// Line before byte offset `start` (a line start), or `None` at file start.
fn preceding_line(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    if start == 0 {
        return None;
    }
    let end = start - 1;
    let line_start = bytes[..end]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |offset| offset + 1);
    Some((line_start, end))
}

/// Line after line-end offset `end`, or `None` past end of file.
fn following_line(bytes: &[u8], end: usize) -> Option<(usize, usize)> {
    let start = end.checked_add(1)?;
    (start < bytes.len()).then_some((start, line_end(bytes, start)))
}

/// Tag name of a record line: the text before the first TAB.
fn record_name(line: &[u8]) -> &[u8] {
    match line.iter().position(|byte| *byte == b'\t') {
        Some(tab) => &line[..tab],
        None => line,
    }
}

/// Binary-searches a sorted tags file body over raw byte offsets, mirroring
/// upstream's `TS_BINARY` → `TS_SKIP_BACK` → `TS_STEP_FORWARD` walk (tag.c
/// 1641-1647, 1672-1688): compare only the probed line's name head
/// (`strncmp` within `cmplen`, tag.c:1628; a shorter name searches forward,
/// a longer one backward), then parse only the contiguous run of equal
/// names, keeping the records the needle accepts.
fn binary_matches(
    text: &str,
    needle: &str,
    needle_matcher: &TagNeedle,
    taglength: usize,
    ignorecase: bool,
) -> Vec<TagMatch> {
    let bytes = text.as_bytes();
    let equal = |line: &[u8]| {
        tag_name_cmp(record_name(line), needle.as_bytes(), taglength, ignorecase)
            == std::cmp::Ordering::Equal
    };
    let mut low = 0;
    let mut high = bytes.len();
    let mut hit = None;
    while low < high {
        let midpoint = low + (high - low) / 2;
        let Some((start, end)) = probe_line(bytes, midpoint, high) else {
            if midpoint == low {
                break;
            }
            high = midpoint;
            continue;
        };
        let probe = tag_name_cmp(
            record_name(&bytes[start..end]),
            needle.as_bytes(),
            taglength,
            ignorecase,
        );
        match probe {
            std::cmp::Ordering::Less => low = end.saturating_add(1),
            std::cmp::Ordering::Greater => {
                high = if start == low { midpoint } else { start };
            }
            std::cmp::Ordering::Equal => {
                hit = Some(start);
                break;
            }
        }
    }
    let Some(hit) = hit else {
        return Vec::new();
    };
    // Skip back to the first line of the equal-name run.
    let mut first = hit;
    while let Some((start, end)) = preceding_line(bytes, first) {
        if bytes[start..end].starts_with(b"!_") || !equal(&bytes[start..end]) {
            break;
        }
        first = start;
    }
    // Parse only the run, keeping the records the needle accepts.
    let mut matches = Vec::new();
    let mut cursor = first;
    loop {
        let end = line_end(bytes, cursor);
        let parsed = str::from_utf8(&bytes[cursor..end])
            .ok()
            .and_then(|line| parse_record(line.strip_suffix('\r').unwrap_or(line)).ok())
            .filter(|record| needle_matcher.matches(&record.name, taglength, ignorecase));
        if let Some(record) = parsed {
            matches.push(record);
        }
        let Some((start, end)) = following_line(bytes, end) else {
            break;
        };
        if bytes[start..end].starts_with(b"!_") || !equal(&bytes[start..end]) {
            break;
        }
        cursor = start;
    }
    matches
}

fn tag_name_cmp(
    name: &[u8],
    needle: &[u8],
    taglength: usize,
    ignorecase: bool,
) -> std::cmp::Ordering {
    let compared = if taglength == 0 {
        name.len().min(needle.len())
    } else {
        taglength.min(name.len()).min(needle.len())
    };
    for index in 0..compared {
        let left = if ignorecase {
            name[index].to_ascii_lowercase()
        } else {
            name[index]
        };
        let right = if ignorecase {
            needle[index].to_ascii_lowercase()
        } else {
            needle[index]
        };
        match left.cmp(&right) {
            std::cmp::Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    if taglength > 0 {
        std::cmp::Ordering::Equal
    } else {
        name.len().cmp(&needle.len())
    }
}

/// Reads `'tags'` (comma-separated) and returns matches for `needle`.
///
/// An empty tags list is E433. A list with files but no matching name is
/// E426. Missing tags files are skipped. Malformed lines are E431.
///
/// # Errors
///
/// Returns a Vim tag error code and message when no tags file is set,
/// a tags file is malformed or unsorted, or no tag matches.
pub fn lookup<F: FileIO>(
    io: &F,
    tags_option: &str,
    needle: &str,
) -> Result<Vec<TagMatch>, (&'static str, String)> {
    lookup_with(io, tags_option, needle, 0, false)
}

/// [`lookup`] with `'taglength'` and `'ignorecase'`.
///
/// # Errors
///
/// Returns a Vim tag error code and message when no tags file is set,
/// a tags file is malformed or unsorted, or no tag matches.
pub fn lookup_with<F: FileIO>(
    io: &F,
    tags_option: &str,
    needle: &str,
    taglength: usize,
    ignorecase: bool,
) -> Result<Vec<TagMatch>, (&'static str, String)> {
    lookup_search(io, tags_option, needle, taglength, ignorecase, true)
}
/// Searches tag names with Vim regular-expression semantics.
///
/// # Errors
///
/// Returns a Vim tag error code and message when no tags file is set,
/// a tags file is malformed or unsorted, or the pattern does not match.
pub fn lookup_pattern<F: FileIO>(
    io: &F,
    tags_option: &str,
    pattern: &str,
    taglength: usize,
    ignorecase: bool,
) -> Result<Vec<TagMatch>, (&'static str, String)> {
    lookup_search(
        io,
        tags_option,
        &format!("/{pattern}/"),
        taglength,
        ignorecase,
        false,
    )
}

/// [`lookup_with`] honouring `'tagbsearch'`.
///
/// # Errors
///
/// Returns a Vim tag error code and message when no tags file is set,
/// a tags file is malformed or unsorted, or no tag matches.
pub fn lookup_search<F: FileIO>(
    io: &F,
    tags_option: &str,
    needle: &str,
    taglength: usize,
    ignorecase: bool,
    tagbsearch: bool,
) -> Result<Vec<TagMatch>, (&'static str, String)> {
    // One compile serves both validation and the record loops (upstream
    let Some(needle_matcher) = TagNeedle::new(needle, ignorecase) else {
        return Err(("E426", format!("Tag not found: {needle}")));
    };
    let files: Vec<&str> = tags_option
        .split(',')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect();
    if files.is_empty() {
        return Err(("E433", "No tags file".to_owned()));
    }
    let mut matches = Vec::new();
    let mut saw_file = false;
    let mut sort_errors = Vec::new();
    for file in files {
        let path = Path::new(file);
        let Ok(text) = io.read_to_string(path) else {
            continue;
        };
        saw_file = true;
        let tag_needle = &needle_matcher;
        let header = sorted_header(&text);
        let use_binary =
            tagbsearch && !ignorecase && !needle.starts_with('/') && header != Some(b'0');
        if use_binary {
            // Upstream reads and parses tag lines during the binary search
            // and fully parses only the run of matching lines (tag.c
            // 1672-1688), so validate the byte layout for E431 and let the
            // binary walk below parse the matching range alone.
            if !layout_ok(&text) {
                return Err(("E431", format!("Format error in tags file \"{file}\"")));
            }
            if header.is_none() && !names_sorted(&text) {
                let Ok(records) = parse_records(&text, false) else {
                    return Err(("E431", format!("Format error in tags file \"{file}\"")));
                };
                let duplicate_matches: Vec<_> = records
                    .iter()
                    .filter(|record| tag_needle.matches(&record.name, taglength, ignorecase))
                    .cloned()
                    .collect();
                if duplicate_matches.len() > 1 {
                    matches.extend(duplicate_matches);
                    continue;
                }
                sort_errors.push(file.to_owned());
                continue;
            }
            matches.extend(binary_matches(
                &text, needle, tag_needle, taglength, ignorecase,
            ));
        } else {
            let Ok(records) = parse_records(&text, needle.starts_with('/')) else {
                return Err(("E431", format!("Format error in tags file \"{file}\"")));
            };
            matches.extend(
                records
                    .into_iter()
                    .filter(|record| tag_needle.matches(&record.name, taglength, ignorecase)),
            );
        }
    }
    if !sort_errors.is_empty() {
        return Err((
            "E432",
            sort_errors
                .iter()
                .map(|file| format!("Tags file not sorted: {file}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    }
    if matches.is_empty() {
        if saw_file {
            return Err(("E426", format!("Tag not found: {needle}")));
        }
        return Err(("E433", "No tags file".to_owned()));
    }
    Ok(matches)
}

/// Stable-partition matches so those whose filename equals `preferred` come first.
#[must_use]
pub fn prefer_filename(mut matches: Vec<TagMatch>, preferred: Option<&str>) -> Vec<TagMatch> {
    let Some(preferred) = preferred.filter(|name| !name.is_empty()) else {
        return matches;
    };
    let preferred_name = std::path::Path::new(preferred)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(preferred);

    matches.sort_by_key(|item| {
        let name = item.filename.file_name().and_then(|name| name.to_str());
        u8::from(name != Some(preferred_name) && item.filename.to_str() != Some(preferred))
    });
    matches
}

/// Interprets a tags `cmd` field as a 1-based line or a `/pattern/` search.
///
/// Returns `(position, guessed)` where `guessed` is true when the exact
/// pattern missed and a looser search found a line (`E435` in `tag.c`).
#[must_use]
pub fn cmd_target(lines: &[Vec<u8>], cmd: &str) -> Option<(Position, bool)> {
    cmd_target_from(lines, cmd, 0)
}

/// [`cmd_target`] searching strictly after `start_line` (1-based, exclusive
/// start): the hinted line itself is skipped, so a repeat on a later line
/// wins.
///
/// Upstream `jumpto_tag` instead starts `do_search` before the `line:`
/// field's line (tag.c:2812-2818), so that line is examined; callers
/// mirroring that pass `start_line - 1`.
#[must_use]
pub fn cmd_target_from(
    lines: &[Vec<u8>],
    cmd: &str,
    start_line: usize,
) -> Option<(Position, bool)> {
    let cmd = cmd.trim().trim_end_matches(';').trim();
    if let Ok(lnum) = cmd.parse::<usize>() {
        return Some((
            Position {
                lnum: lnum.max(1),
                col: 0,
            },
            false,
        ));
    }
    let bytes = cmd.as_bytes();
    if bytes.first() != Some(&b'/') {
        return None;
    }
    let inner = if bytes.last() == Some(&b'/') && bytes.len() > 1 {
        &cmd[1..cmd.len() - 1]
    } else {
        &cmd[1..]
    };
    let anchored_start = inner.starts_with('^');
    let anchored_end = inner.ends_with('$');
    let needle = inner.strip_prefix('^').unwrap_or(inner);
    let needle = needle.strip_suffix('$').unwrap_or(needle);
    let start = start_line;
    if let Some(lnum) = find_line(
        &lines[start.min(lines.len())..],
        needle.as_bytes(),
        anchored_start,
        anchored_end,
    ) {
        return Some((
            Position {
                lnum: lnum + start,
                col: 0,
            },
            false,
        ));
    }

    None
}

/// Guess a tag location from the tag name when the cmd pattern missed (`E435`).
///
/// Returns `(position, guessed_pattern)` so `'cpoptions'` `t` can store `@/`.
#[must_use]
pub fn guess_target(lines: &[Vec<u8>], tagname: &str) -> Option<(Position, String)> {
    if tagname.is_empty() {
        return None;
    }
    let first = format!("^{tagname}\\s*(");
    if let Some(lnum) = find_guess(lines, tagname.as_bytes(), true) {
        return Some((Position { lnum, col: 0 }, first));
    }
    let second = format!("^[#a-zA-Z_].*\\<{tagname}\\s*(");
    if let Some(lnum) = find_guess(lines, tagname.as_bytes(), false) {
        return Some((Position { lnum, col: 0 }, second));
    }
    None
}

fn find_guess(lines: &[Vec<u8>], name: &[u8], line_start: bool) -> Option<usize> {
    for (index, line) in lines.iter().enumerate() {
        let Some(col) = line
            .windows(name.len())
            .position(|window| window.eq_ignore_ascii_case(name))
        else {
            continue;
        };
        if line_start && col != 0 {
            continue;
        }
        if col > 0 {
            let before = line[col - 1];
            if before.is_ascii_alphanumeric() || before == b'_' {
                continue;
            }
        }
        let after = col + name.len();
        let rest = line.get(after..).unwrap_or(&[]);
        let skipped = rest
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        if rest.get(skipped) == Some(&b'(') {
            return Some(index + 1);
        }
    }
    None
}

fn find_line(
    lines: &[Vec<u8>],
    needle: &[u8],
    anchored_start: bool,
    anchored_end: bool,
) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    lines
        .iter()
        .position(|line| {
            if anchored_start && anchored_end {
                line.as_slice() == needle
            } else if anchored_start {
                line.starts_with(needle)
            } else if anchored_end {
                line.ends_with(needle)
            } else {
                line.windows(needle.len()).any(|window| window == needle)
            }
        })
        .map(|index| index + 1)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fmt::Write as _;
    use std::io;

    use super::*;

    struct FakeIo {
        files: HashMap<String, String>,
    }

    impl FakeIo {
        fn new(files: &[(&str, String)]) -> Self {
            Self {
                files: files
                    .iter()
                    .map(|(name, body)| ((*name).to_owned(), body.clone()))
                    .collect(),
            }
        }
    }

    impl FileIO for FakeIo {
        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.files
                .get(&path.to_string_lossy().into_owned())
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing"))
        }

        fn write_string(&self, _path: &Path, _contents: &str) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "read-only"))
        }

        fn exists(&self, path: &Path) -> bool {
            self.files
                .contains_key(&path.to_string_lossy().into_owned())
        }

        fn canonicalize(&self, path: &Path) -> PathBuf {
            path.to_path_buf()
        }
    }

    fn lines(text: &[&str]) -> Vec<Vec<u8>> {
        text.iter().map(|line| line.as_bytes().to_vec()).collect()
    }

    /// `start_line` is an exclusive start: the hinted line is skipped and a
    /// repeat on the next line is the hit.
    #[test]
    fn cmd_target_from_start_line_is_exclusive() {
        let text = lines(&["alpha", "needle", "needle again"]);
        let (position, guessed) = cmd_target_from(&text, "/needle/", 2).expect("hit");
        assert!(!guessed);
        assert_eq!(position.lnum, 3);
        assert_eq!(position.col, 0);
        // Zero still searches from the first line.
        let (position, _) = cmd_target_from(&text, "/alpha/", 0).expect("hit");
        assert_eq!(position.lnum, 1);
    }

    /// `/pat/` needles honor 'ignorecase' (tag.c:2248-2249) unless the
    /// pattern carries an explicit `\c`/`\C`.
    #[test]
    fn pattern_needle_honors_ignorecase_and_explicit_case() {
        let io = FakeIo::new(&[("tags", "Foo\tfile.rs\t1\nbar\tfile.rs\t2\n".to_owned())]);
        let found = lookup_search(&io, "tags", "/foo/", 0, true, false).expect("found");
        let names: Vec<_> = found.iter().map(|match_| match_.name.as_str()).collect();
        assert_eq!(names, ["Foo"]);
        // An explicit \C overrides the option; without it the search stays
        // case-sensitive.
        let strict = lookup_search(&io, "tags", "/\\Cfoo/", 0, true, false);
        assert!(matches!(strict, Err(("E426", _))));
        let sensitive = lookup_search(&io, "tags", "/foo/", 0, false, false);
        assert!(matches!(sensitive, Err(("E426", _))));
    }

    /// The raw-byte binary search over a sorted file finds the same matches
    /// as the linear path, including duplicate runs and `'taglength'` runs.
    #[test]
    fn binary_search_matches_linear_path_on_sorted_file() {
        let mut body = String::from("!_TAG_FILE_SORTED\t1\t/sorted/\n");
        for index in 0..2000 {
            let _ = writeln!(body, "tag{index:04}\tfile_{index}.rs\t{index}");
            if index == 500 {
                body.push_str("tag0500\tdup.rs\t7\n");
            }
        }
        let io = FakeIo::new(&[("tags", body)]);
        let linear = lookup_search(&io, "tags", "tag0500", 0, false, false).expect("found");
        let binary = lookup_search(&io, "tags", "tag0500", 0, false, true).expect("found");
        assert_eq!(binary, linear);
        assert_eq!(binary.len(), 2);
        // A 'taglength' probe expands to the whole equal-prefix run.
        let linear = lookup_search(&io, "tags", "tag0999", 4, false, false).expect("found");
        let binary = lookup_search(&io, "tags", "tag0999", 4, false, true).expect("found");
        assert_eq!(binary, linear);
        assert_eq!(binary.len(), 1001);
        let linear = lookup_search(&io, "tags", "tag1999", 0, false, false).expect("found");
        let binary = lookup_search(&io, "tags", "tag1999", 0, false, true).expect("found");
        assert_eq!(binary, linear);
    }

    /// A sorted file without the sorted header takes the same name-scan gate
    /// and reaches the same matches through the binary walk.
    #[test]
    fn binary_search_without_sorted_header_matches_linear_path() {
        let mut body = String::new();
        for index in 0..500 {
            let _ = writeln!(body, "name{index:03}\tsrc_{index}.rs\t{index}");
        }
        let io = FakeIo::new(&[("tags", body)]);
        let linear = lookup_search(&io, "tags", "name250", 0, false, false).expect("found");
        let binary = lookup_search(&io, "tags", "name250", 0, false, true).expect("found");
        assert_eq!(binary, linear);
        assert_eq!(binary.len(), 1);
    }
}
