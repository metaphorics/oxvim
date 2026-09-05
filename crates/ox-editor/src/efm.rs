//! 'errorformat' compiler and per-line parser.
//!
//! Port of upstream `parse_efm.c` (folded into `quickfix.c` in this checkout):
//! `parse_efm_option` (quickfix.c:643-688) compiles the comma-separated
//! 'errorformat' into a list of `efm_T` regex alternatives, and
//! `qf_parse_line` (quickfix.c:951-1040) matches one input line against them,
//! maintaining the directory stack (`%D`/`%X`), the file stack (`%P`/`%Q`),
//! multi-line continuation state (`%A`/`%C`/`%Z`/`%E`/`%W`/`%I`/`%N`), the
//! `%>` resume marker, and `%r` tail re-scanning.
//!
//! The compiled pattern is a Vim-magic regex anchored `^...$` and always
//! matched case-insensitively (upstream `regmatch.rm_ic = true`,
//! quickfix.c:1638), rendered here as a leading `\c` after `^` so the `^`
//! keeps its anchor position.

use ox_regex::{Capture, Magic, Match, Prog, Text};

/// Upstream `fmt_pat[]` (quickfix.c:400-421): conversion char → regex body.
/// Index order is load-bearing; `addr[]` and `qf_parse_fmt[]` are indexed by
/// it. `%f` (0) and `%r` (9) are special-cased.
const FMT_PAT: [(u8, &str); FMT_PATTERNS] = [
    (b'f', ".\\+"),     // only used when at end
    (b'b', "\\d\\+"),   // 1
    (b'n', "\\d\\+"),   // 2
    (b'l', "\\d\\+"),   // 3
    (b'e', "\\d\\+"),   // 4
    (b'c', "\\d\\+"),   // 5
    (b'k', "\\d\\+"),   // 6
    (b't', "."),        // 7
    (b'm', ".\\+"),     // 8 = FMT_PATTERN_M
    (b'r', ".*"),       // 9 = FMT_PATTERN_R
    (b'p', "[-\t .]*"), // 10
    (b'v', "\\d\\+"),   // 11
    (b's', ".\\+"),     // 12
    (b'o', ".\\+"),     // 13
];
const FMT_PATTERNS: usize = 14;
const FMT_PATTERN_M: usize = 8;
const FMT_PATTERN_R: usize = 9;

/// An 'errorformat' compile or parse failure carrying a Vim-style error code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EfmError {
    /// `E…` error number.
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
}

impl EfmError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for EfmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// One compiled 'errorformat' alternative (upstream `efm_T`, quickfix.c:174).
#[derive(Debug)]
struct EfmPart {
    /// Compiled `^…$` regex for this alternative.
    prog: Prog,
    /// Capture-group number per `FMT_PAT` index, 0 = absent (`addr[]`).
    addr: [u8; FMT_PATTERNS],
    /// `DXAEWINCZGOPQ` prefix letter, or 0.
    prefix: u8,
    /// `+` or `-` flag, or 0.
    flags: u8,
    /// `%>` appeared in this alternative: resume matching from here.
    conthere: bool,
}

/// A compiled 'errorformat' option value (the `efm_T` list).
#[derive(Debug)]
pub struct ErrorFormat {
    parts: Vec<EfmPart>,
}

/// Per-list parse state carried across lines (the `qf_list_T` fields
/// `qf_dir_stack`, `qf_file_stack`, `qf_directory`, `qf_currfile`,
/// `qf_multiline`, `qf_multiignore`, `qf_multiscan`, quickfix.c:141-148, plus
/// the `%>` resume position `fmt_start`, quickfix.c:596 — upstream keeps it in
/// a file-static, here it is per list which is strictly saner).
#[derive(Clone, Debug, Default)]
pub struct EfmState {
    /// `%D`/`%X` directory stack, top of stack last (`qf_dir_stack`).
    dir_stack: Vec<String>,
    /// `%P`/`%Q` file stack, top of stack last (`qf_file_stack`).
    file_stack: Vec<String>,
    /// Directory relative filenames resolve against (`qf_directory`).
    directory: Option<String>,
    /// File pushed by `%P` used when an entry has no `%f` (`qf_currfile`).
    currfile: Option<String>,
    /// Inside a multi-line `%A`/`%E`/`%W`/`%I`/`%N` block (`qf_multiline`).
    multiline: bool,
    /// Inside a `%-`-excluded multi-line block (`qf_multiignore`).
    multiignore: bool,
    /// Re-scanning a `%r` tail (`qf_multiscan`).
    multiscan: bool,
    /// `%>` resume position: index into `ErrorFormat::parts` (`fmt_start`).
    fmt_start: Option<usize>,
}

/// Host services the parser needs from the editor (buffer table).
pub trait EfmContext {
    /// Whether buffer number `nr` exists (upstream `buflist_findnr`).
    fn buffer_exists(&mut self, nr: i64) -> bool;
    /// Finds or creates a buffer for `name`, returning its number
    /// (upstream `buflist_new` via `qf_get_fnum`, quickfix.c:2350).
    /// Returns 0 on failure.
    fn buffer_for_name(&mut self, name: &str) -> i64;
}

/// A parsed error line after filename→buffer resolution (`qfline_T`).
#[derive(Clone, Debug, Default)]
pub struct ParsedEntry {
    /// Resolved buffer number, or 0 when the entry has no file.
    pub bufnr: i64,
    /// `%o` module name.
    pub module: String,
    /// `%l` line number, or 0.
    pub lnum: i64,
    /// `%e` end line number, or 0.
    pub end_lnum: i64,
    /// `%c`/`%p`/`%v` column, or 0.
    pub col: i64,
    /// `%k` end column, or 0.
    pub end_col: i64,
    /// Column is a visual column (`%p`/`%v`).
    pub vcol: bool,
    /// `%n` error number, -1 when unset (upstream `enr` default).
    pub nr: i64,
    /// `%s` search pattern (`^\V…\$` wrapped).
    pub pattern: String,
    /// `%m` message text.
    pub text: String,
    /// `%t` type character, or 0.
    pub item_type: u8,
    /// Whether the entry has a usable position.
    pub valid: bool,
}

/// Field values a `%C`/`%Z` continuation line contributes when the fold
/// target lives outside the current batch (upstream folds into
/// `qfl->qf_last`, quickfix.c:1707).
#[derive(Clone, Debug, Default)]
pub struct Continuation {
    /// `%m` text to append after a newline, empty when absent.
    pub text: String,
    /// `%n` error number, applied when the target's `nr` is -1.
    pub nr: i64,
    /// `%t` type char, applied when printable and the target has none.
    pub item_type: u8,
    /// `%l` line number, applied when the target's is 0.
    pub lnum: i64,
    /// `%e` end line, applied when the target's is 0.
    pub end_lnum: i64,
    /// `%c`/`%p`/`%v` column, applied when the target's is 0.
    pub col: i64,
    /// `%k` end column, applied when the target's is 0.
    pub end_col: i64,
    /// Column is a visual column.
    pub vcol: bool,
    /// Resolved buffer number for a target with `bufnr == 0`.
    pub bufnr: i64,
}

/// What one parsed input line produced (upstream `QF_` status,
/// quickfix.c:206-214).
pub enum LineOutcome {
    /// The line produced an entry (`QF_OK`).
    Entry(ParsedEntry),
    /// The line was folded into `entries.last()` (`%C`/`%Z`).
    Folded,
    /// The line was a `%C`/`%Z` continuation but `entries` is empty; the
    /// caller may fold the data into the target list's own last item.
    FoldOutside(Box<Continuation>),
    /// The line was consumed without producing an entry
    /// (`QF_IGNORE_LINE`: `%-`-flagged formats, `%P`/`%Q` without tail,
    /// interior of an ignored multi-line block).
    Ignored,
}

/// Scratch field values for one line (upstream `qffields_T`,
/// quickfix.c:234-250).
#[derive(Default)]
struct Fields {
    namebuf: String,
    bnr: i64,
    module: String,
    errmsg: String,
    lnum: i64,
    end_lnum: i64,
    col: i64,
    end_col: i64,
    use_viscol: bool,
    pattern: String,
    enr: i64,
    item_type: u8,
    valid: bool,
}

impl ErrorFormat {
    /// Compiles one comma-separated 'errorformat' list
    /// (`parse_efm_option`, quickfix.c:643-688).
    ///
    /// # Errors
    ///
    /// E372 (duplicate `%x`), E373 (`%x` unexpected for the prefix),
    /// E374 (missing `]` in `%*[…]`), E375 (unsupported `%*x`),
    /// E376 (invalid `%x` prefix), E377 (invalid `%x`), E378 (no pattern),
    /// or the regex compiler's own message.
    pub fn compile(spec: &str) -> Result<Self, EfmError> {
        let bytes = spec.as_bytes();
        let mut parts = Vec::new();
        let mut pos = 0;
        while pos < bytes.len() {
            // `efm_option_part_len` (quickfix.c:627): a part ends at the
            // first `,` not preceded by `\`.
            let start = pos;
            while pos < bytes.len() && bytes[pos] != b',' {
                if bytes[pos] == b'\\' && pos + 1 < bytes.len() {
                    pos += 1;
                }
                pos += 1;
            }
            parts.push(compile_part(&spec[start..pos])?);
            // `skip_to_option_part` (option.c:6978): skip the comma and
            // following spaces.
            if pos < bytes.len() {
                pos += 1; // the ','
            }
            while pos < bytes.len() && bytes[pos] == b' ' {
                pos += 1;
            }
        }
        if parts.is_empty() {
            return Err(EfmError::new("E378", "'errorformat' contains no pattern"));
        }
        Ok(Self { parts })
    }
}

/// `efm_to_regpat` (quickfix.c:534-594): converts one 'errorformat' part to a
/// `^…$` Vim-magic regex and records the `%x` capture-group assignments.
fn compile_part(part: &str) -> Result<EfmPart, EfmError> {
    let mut addr = [0u8; FMT_PATTERNS];
    let mut prefix = 0u8;
    let mut flags = 0u8;
    let mut conthere = false;
    // `^` must stay at pattern position 0 to remain an anchor; `\c` after it
    // applies upstream's unconditional `rm_ic = true` (quickfix.c:1638).
    // The pattern is built byte-wise like upstream; the source is valid
    // UTF-8 so the emitted subset converts back losslessly.
    let mut regpat: Vec<u8> = b"^\\c".to_vec();
    let bytes = part.as_bytes();
    let mut round = 0u8;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            i += 1;
            if i >= bytes.len() {
                return Err(EfmError::new("E377", "Invalid % in format string"));
            }
            let c = bytes[i];
            if let Some(fi) = FMT_PAT.iter().position(|&(cc, _)| cc == c) {
                push_field_group(bytes, i, fi, prefix, &mut addr, &mut round, &mut regpat)?;
            } else if c == b'*' {
                // `scanf_fmt_to_regpat` (quickfix.c:483-514).
                push_scanf_class(part.as_bytes(), &mut i, &mut regpat)?;
            } else if matches!(c, b'%' | b'\\' | b'.' | b'^' | b'$' | b'~' | b'[') {
                // Regexp magic characters pass through (quickfix.c:562-563).
                regpat.push(c);
            } else if c == b'#' {
                regpat.push(b'*');
            } else if c == b'>' {
                conthere = true;
            } else if i == 1 {
                // `efm_analyze_prefix` (quickfix.c:517-531): a prefix is only
                // allowed at the start of an option part.
                if matches!(c, b'+' | b'-') {
                    flags = c;
                    i += 1;
                    if i >= bytes.len() {
                        return Err(EfmError::new("E376", "Invalid % in format string prefix"));
                    }
                }
                let p = bytes[i];
                if matches!(
                    p,
                    b'D' | b'X'
                        | b'A'
                        | b'E'
                        | b'W'
                        | b'I'
                        | b'N'
                        | b'C'
                        | b'Z'
                        | b'G'
                        | b'O'
                        | b'P'
                        | b'Q'
                ) {
                    prefix = p;
                } else {
                    return Err(EfmError::new(
                        "E376",
                        format!("Invalid %{} in format string prefix", p as char),
                    ));
                }
            } else {
                return Err(EfmError::new(
                    "E377",
                    format!("Invalid %{} in format string", c as char),
                ));
            }
            i += 1;
        } else {
            // Copy a normal character (quickfix.c:579-588): `\` quotes the
            // next char verbatim; regex atoms are escaped.
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 1;
            } else if matches!(bytes[i], b'.' | b'*' | b'^' | b'$' | b'~' | b'[') {
                regpat.push(b'\\');
            }
            regpat.push(bytes[i]);
            i += 1;
        }
    }
    regpat.push(b'$');
    let regpat = String::from_utf8_lossy(&regpat);
    let prog = ox_regex::compile(&regpat, Magic::Magic)
        .map_err(|e| EfmError::new("E377", e.to_string()))?;
    Ok(EfmPart {
        prog,
        addr,
        prefix,
        flags,
        conthere,
    })
}

/// Emits one `%x` field as a capture group and records its capture round
/// (`efmpat_to_regpat`, quickfix.c:427-479).
fn push_field_group(
    bytes: &[u8],
    i: usize,
    fi: usize,
    prefix: u8,
    addr: &mut [u8; FMT_PATTERNS],
    round: &mut u8,
    regpat: &mut Vec<u8>,
) -> Result<(), EfmError> {
    let c = bytes[i];
    if addr[fi] != 0 {
        return Err(EfmError::new(
            "E372",
            format!("Too many %{} in format string", c as char),
        ));
    }
    if (fi != 0 && fi < FMT_PATTERN_R && matches!(prefix, b'D' | b'X' | b'O' | b'P' | b'Q'))
        || (fi == FMT_PATTERN_R && !matches!(prefix, b'O' | b'P' | b'Q'))
    {
        return Err(EfmError::new(
            "E373",
            format!("Unexpected %{} in format string", c as char),
        ));
    }
    *round += 1;
    addr[fi] = *round;
    regpat.extend_from_slice(b"\\(");
    if c == b'f' && i + 1 < bytes.len() {
        if bytes[i + 1] != b'\\' && bytes[i + 1] != b'%' {
            // A file name may contain the following literal (e.g. ':' in
            // "%f:%l:%m"); non-greedy any-char run (quickfix.c:454-462).
            regpat.extend_from_slice(b".\\{-1,}");
        } else {
            // Followed by '\\' or '%': file-name chars only
            // (quickfix.c:463-467).
            regpat.extend_from_slice(b"\\f\\+");
        }
    } else {
        regpat.extend_from_slice(FMT_PAT[fi].1.as_bytes());
    }
    regpat.extend_from_slice(b"\\)");
    Ok(())
}

/// Emits the regex for a `%*` scanf-style class (`%*[...]`, `%*\D`, ...) and
/// advances `i` past it (`scanf_fmt_to_regpat`, quickfix.c:483-514).
fn push_scanf_class(bytes: &[u8], i: &mut usize, regpat: &mut Vec<u8>) -> Result<(), EfmError> {
    *i += 1;
    if *i >= bytes.len() {
        return Err(EfmError::new("E375", "Unsupported %* in format string"));
    }
    if bytes[*i] == b'[' {
        regpat.push(b'[');
        *i += 1;
        if *i < bytes.len() && bytes[*i] == b'^' {
            regpat.push(b'^');
            *i += 1;
        }
        if *i < bytes.len() {
            // First char may be a literal ']'.
            regpat.push(bytes[*i]);
            *i += 1;
            let mut closed = false;
            while *i < bytes.len() {
                let ch = bytes[*i];
                regpat.push(ch);
                *i += 1;
                if ch == b']' {
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err(EfmError::new("E374", "Missing ] in format string"));
            }
        }
        regpat.extend_from_slice(b"\\+");
    } else if bytes[*i] == b'\\' {
        // %*\D, %*\s etc.: emit the escaped class then `\+`.
        regpat.push(b'\\');
        *i += 1;
        if *i < bytes.len() {
            regpat.push(bytes[*i]);
        }
        regpat.extend_from_slice(b"\\+");
    } else {
        return Err(EfmError::new(
            "E375",
            format!("Unsupported %*{} in format string", bytes[*i] as char),
        ));
    }
    Ok(())
}

/// Parses one input line against the compiled formats
/// (`qf_parse_line`, quickfix.c:951-1040).
///
/// `entries` accumulates produced entries and is the fold target for
/// `%C`/`%Z` continuation lines (upstream `qfl->qf_last`). `state` carries the
/// directory/file stacks and multi-line flags across calls.
///
/// # Errors
///
/// E379 when a `%D` line has no directory name; other failures mirror
/// upstream `QF_FAIL` aborts.
pub fn parse_line(
    mut line: &str,
    fmt: &ErrorFormat,
    state: &mut EfmState,
    entries: &mut [ParsedEntry],
    ctx: &mut dyn EfmContext,
) -> Result<LineOutcome, EfmError> {
    // `restofline:` — a `%r` tail re-scans the remainder of the line.
    loop {
        let mut fields = Fields {
            valid: true,
            enr: -1,
            ..Fields::default()
        };
        let mut tail: Option<usize> = None;
        let mut mode = ScanMode {
            multiline: state.multiline,
            multiscan: state.multiscan,
            tail: &mut tail,
        };

        // Without a `%>` resume marker start at the first pattern, else at
        // the last used one (quickfix.c:960-967).
        let start = state.fmt_start.take().unwrap_or(0);
        let mut matched = None;
        for (i, part) in fmt.parts.iter().enumerate().skip(start) {
            if get_fields(line, part, &mut fields, &mut mode, ctx) {
                matched = Some(i);
                break;
            }
        }
        state.multiscan = false;

        let Some(i) = matched else {
            // No format matched: an invalid entry holding the line text, and
            // the multi-line state resets (quickfix.c:994-1000).
            nomatch(&mut fields, line);
            state.multiline = false;
            state.multiignore = false;
            return Ok(LineOutcome::Entry(make_entry(fields, state, ctx)));
        };

        let part = &fmt.parts[i];
        let idx = part.prefix;
        if idx == b'D' || idx == b'X' {
            // Directory specifiers still produce a nomatch entry
            // (quickfix.c:986-997).
            dir_pfx(idx, &fields, state)?;
            nomatch(&mut fields, line);
            return Ok(LineOutcome::Entry(make_entry(fields, state, ctx)));
        }

        // Honor the `%>` item (quickfix.c:1002-1005).
        if part.conthere {
            state.fmt_start = Some(i);
        }

        match idx {
            b'A' | b'E' | b'W' | b'I' | b'N' => {
                // Start of a multi-line message (quickfix.c:1007-1009).
                state.multiline = true;
                state.multiignore = false;
            }
            b'C' | b'Z' => {
                return Ok(fold_continuation(idx, fields, state, entries, ctx));
            }
            b'O' | b'P' | b'Q' => {
                // `qf_parse_file_pfx` (quickfix.c:1673-1690).
                fields.valid = false;
                if fields.namebuf.is_empty() || path_exists(&fields.namebuf) {
                    if !fields.namebuf.is_empty() && idx == b'P' {
                        state.currfile =
                            Some(push_dir(&mut state.file_stack, &fields.namebuf, true));
                    } else if idx == b'Q' {
                        state.currfile = pop_dir(&mut state.file_stack);
                    }
                    fields.namebuf.clear();
                    if let Some(t) = tail.filter(|&t| !line[t..].is_empty()) {
                        let s = skipwhite(&line[t..]);
                        if s.len() >= line.len() {
                            // Flag set only on an actual re-scan: an early
                            // return must not leak multiscan into the next
                            // line (state persists across lines).
                            return Ok(LineOutcome::Ignored);
                        }
                        state.multiscan = true;
                        line = s;
                        continue;
                    }
                }
            }
            _ => {}
        }
        if part.flags == b'-' {
            // Generally exclude this line (quickfix.c:1030-1036).
            if state.multiline {
                state.multiignore = true;
            }
            return Ok(LineOutcome::Ignored);
        }
        return Ok(LineOutcome::Entry(make_entry(fields, state, ctx)));
    }
}

/// `qf_parse_get_fields` (quickfix.c:1613-1649): resets the per-line fields
/// and runs one alternative's regex.
fn get_fields(
    line: &str,
    part: &EfmPart,
    fields: &mut Fields,
    mode: &mut ScanMode<'_>,
    ctx: &mut dyn EfmContext,
) -> bool {
    if mode.multiscan && !matches!(part.prefix, b'O' | b'P' | b'Q') {
        return false;
    }
    fields.namebuf.clear();
    fields.bnr = 0;
    fields.module.clear();
    fields.pattern.clear();
    if !mode.multiscan {
        fields.errmsg.clear();
    }
    fields.lnum = 0;
    fields.end_lnum = 0;
    fields.col = 0;
    fields.end_col = 0;
    fields.use_viscol = false;
    fields.enr = -1;
    fields.item_type = 0;
    *mode.tail = None;

    let text = Text::new(line);
    // An engine step-limit error is treated as a non-match; upstream
    // `vim_regexec` has no error channel.
    let Some(m) = ox_regex::try_exec(&part.prog, &text).ok().flatten() else {
        return false;
    };
    parse_match(line, part, &m, fields, mode, ctx)
}

/// The cross-call scan flags upstream threads through `qf_parse_line`.
struct ScanMode<'a> {
    multiline: bool,
    multiscan: bool,
    tail: &'a mut Option<usize>,
}

/// `qf_parse_match` (quickfix.c:1567-1607): extracts every `%x` field from a
/// matched line.
fn parse_match(
    line: &str,
    part: &EfmPart,
    m: &Match,
    fields: &mut Fields,
    mode: &mut ScanMode<'_>,
    ctx: &mut dyn EfmContext,
) -> bool {
    let idx = part.prefix;
    if (idx == b'C' || idx == b'Z') && !mode.multiline {
        return false;
    }
    fields.item_type = if matches!(idx, b'E' | b'W' | b'I' | b'N') {
        idx
    } else {
        0
    };

    for i in 0..FMT_PATTERNS {
        let midx = part.addr[i] as usize;
        let cap = |m: &Match| -> Option<Capture> {
            if midx == 0 {
                None
            } else {
                m.captures.get(midx - 1).cloned().flatten()
            }
        };
        let ok = if i == 0 {
            midx == 0 || fmt_f(line, cap(m), fields, idx)
        } else if i == FMT_PATTERN_M {
            if part.flags == b'+' && !mode.multiscan {
                // `%+`: copy the whole line (copy_nonerror_line,
                // quickfix.c:1436-1448).
                line.clone_into(&mut fields.errmsg);
                true
            } else {
                midx == 0 || fmt_m(line, cap(m), fields)
            }
        } else if i == FMT_PATTERN_R {
            if midx == 0 {
                true
            } else if let Some(c) = cap(m) {
                *mode.tail = Some(c.start.byte);
                true
            } else {
                false
            }
        } else {
            midx == 0 || fmt_field(i, line, cap(m), fields, ctx)
        };
        if !ok {
            return false;
        }
    }
    true
}

/// `qf_parse_fmt_f` (quickfix.c:1331-1351): `%f` filename, with `~`/`$VAR`
/// expansion and the `%O`/`%P`/`%Q` existence check.
fn fmt_f(line: &str, cap: Option<Capture>, fields: &mut Fields, prefix: u8) -> bool {
    let Some(c) = cap else { return false };
    fields.namebuf = expand_env(slice(line, &c));
    if matches!(prefix, b'O' | b'P' | b'Q') && !path_exists(&fields.namebuf) {
        return false;
    }
    true
}

/// `qf_parse_fmt_m` (quickfix.c:1452-1465): `%m` message text.
fn fmt_m(line: &str, cap: Option<Capture>, fields: &mut Fields) -> bool {
    let Some(c) = cap else { return false };
    slice(line, &c).clone_into(&mut fields.errmsg);
    true
}

/// The remaining `qf_parse_fmt_*` extractors (quickfix.c:1355-1539),
/// dispatched by `FMT_PAT` index.
fn fmt_field(
    i: usize,
    line: &str,
    cap: Option<Capture>,
    fields: &mut Fields,
    ctx: &mut dyn EfmContext,
) -> bool {
    let Some(c) = cap else { return false };
    let text = slice(line, &c);
    match i {
        // `%b`: buffer number must name an existing buffer
        // (qf_parse_fmt_b, quickfix.c:1355-1366).
        1 => {
            let nr = atoi(text);
            if !ctx.buffer_exists(nr) {
                return false;
            }
            fields.bnr = nr;
        }
        2 => fields.enr = atoi(text),      // %n
        3 => fields.lnum = atoi(text),     // %l
        4 => fields.end_lnum = atoi(text), // %e
        5 => fields.col = atoi(text),      // %c
        6 => fields.end_col = atoi(text),  // %k
        7 => {
            // %t: first byte of the match (qf_parse_fmt_t).
            fields.item_type = text.as_bytes().first().copied().unwrap_or(0);
        }
        10 => {
            // %p: pointer line — column is the match width in screen cells
            // plus one (qf_parse_fmt_p, quickfix.c:1480-1497).
            fields.col = 0;
            for &b in text.as_bytes() {
                fields.col += 1;
                if b == b'\t' {
                    fields.col += 7;
                    fields.col -= fields.col % 8;
                }
            }
            fields.col += 1;
            fields.use_viscol = true;
        }
        11 => {
            // %v: virtual column (qf_parse_fmt_v).
            fields.col = atoi(text);
            fields.use_viscol = true;
        }
        12 => {
            // %s: search text wrapped as a very-nomagic anchored pattern
            // (qf_parse_fmt_s, quickfix.c:1513-1526).
            fields.pattern = format!("^\\V{text}\\$");
        }
        // %o: module name (qf_parse_fmt_o).
        13 => fields.module.push_str(text),
        _ => {}
    }
    true
}

/// `qf_parse_dir_pfx` (quickfix.c:1654-1670): `%D` pushes, `%X` pops the
/// directory stack.
fn dir_pfx(idx: u8, fields: &Fields, state: &mut EfmState) -> Result<(), EfmError> {
    if idx == b'D' {
        if fields.namebuf.is_empty() {
            return Err(EfmError::new("E379", "Missing or empty directory name"));
        }
        state.directory = Some(push_dir(&mut state.dir_stack, &fields.namebuf, false));
    } else {
        state.directory = pop_dir(&mut state.dir_stack);
    }
    Ok(())
}

/// `qf_parse_line_nomatch` (quickfix.c:1694-1701): an unmatched line becomes
/// an invalid entry holding the whole line.
fn nomatch(fields: &mut Fields, line: &str) {
    fields.namebuf.clear();
    fields.lnum = 0;
    fields.valid = false;
    line.clone_into(&mut fields.errmsg);
}

/// `qf_parse_multiline_pfx` (quickfix.c:1704-1754): folds a `%C`/`%Z` line
/// into the previous entry.
fn fold_continuation(
    idx: u8,
    fields: Fields,
    state: &mut EfmState,
    entries: &mut [ParsedEntry],
    ctx: &mut dyn EfmContext,
) -> LineOutcome {
    if !state.multiignore {
        // The filename rule mirrors qf_init_process_nextline
        // (quickfix.c:357-360).
        let fname = continuation_fname(&fields, state);
        if let Some(prev) = entries.last_mut() {
            if prev.bufnr == 0 {
                prev.bufnr = get_fnum(state, &fname, ctx);
            }
            apply_fold(prev, &fields);
        } else {
            if idx == b'Z' {
                // A %Z ends the multiline block even when the fold target
                // lives outside this call (quickfix.c:1728-1731).
                state.multiline = false;
                state.multiignore = false;
            }
            let bufnr = get_fnum(state, &fname, ctx);
            return LineOutcome::FoldOutside(Box::new(Continuation {
                text: fields.errmsg,
                nr: fields.enr,
                // Only printable type chars are applied
                // (quickfix.c:1722-1725).
                item_type: if is_printable(fields.item_type) {
                    fields.item_type
                } else {
                    0
                },
                lnum: fields.lnum,
                end_lnum: fields.end_lnum,
                col: fields.col,
                end_col: fields.end_col,
                vcol: fields.use_viscol,
                bufnr,
            }));
        }
    }
    if idx == b'Z' {
        state.multiline = false;
        state.multiignore = false;
    }
    LineOutcome::Ignored
}

/// The `namebuf`/`qf_currfile` filename selection shared by entry creation
/// and continuation folding (quickfix.c:357-360, 1741-1745).
fn continuation_fname(fields: &Fields, state: &EfmState) -> String {
    if !fields.namebuf.is_empty() || state.directory.is_some() {
        fields.namebuf.clone()
    } else if fields.valid {
        state.currfile.clone().unwrap_or_default()
    } else {
        String::new()
    }
}

/// Applies continuation fields to a previous entry (quickfix.c:1712-1746).
fn apply_fold(prev: &mut ParsedEntry, fields: &Fields) {
    if !fields.errmsg.is_empty() {
        prev.text.push('\n');
        prev.text.push_str(&fields.errmsg);
    }
    if prev.nr == -1 {
        prev.nr = fields.enr;
    }
    if is_printable(fields.item_type) && prev.item_type == 0 {
        prev.item_type = fields.item_type;
    }
    if prev.lnum == 0 {
        prev.lnum = fields.lnum;
    }
    if prev.end_lnum == 0 {
        prev.end_lnum = fields.end_lnum;
    }
    if prev.col == 0 {
        prev.col = fields.col;
        prev.vcol = fields.use_viscol;
    }
    if prev.end_col == 0 {
        prev.end_col = fields.end_col;
    }
}

/// `qf_init_process_nextline` + `qf_add_entry` field mapping
/// (quickfix.c:355-373, 1925-2011).
fn make_entry(fields: Fields, state: &mut EfmState, ctx: &mut dyn EfmContext) -> ParsedEntry {
    let fname = continuation_fname(&fields, state);
    let bufnr = if fields.bnr != 0 {
        fields.bnr
    } else {
        get_fnum(state, &fname, ctx)
    };
    // Only printable type chars are kept (quickfix.c:1982-1985).
    let item_type = if fields.item_type != 1 && !is_printable(fields.item_type) {
        0
    } else {
        fields.item_type
    };
    ParsedEntry {
        bufnr,
        module: fields.module,
        lnum: fields.lnum,
        end_lnum: fields.end_lnum,
        col: fields.col,
        end_col: fields.end_col,
        vcol: fields.use_viscol,
        nr: fields.enr,
        pattern: fields.pattern,
        text: fields.errmsg,
        item_type,
        valid: fields.valid,
    }
}

/// `qf_get_fnum` (quickfix.c:2312-2360): resolves `fname` against the current
/// directory stack and finds-or-creates the buffer.
fn get_fnum(state: &mut EfmState, fname: &str, ctx: &mut dyn EfmContext) -> i64 {
    if fname.is_empty() {
        return 0;
    }
    let name = resolve_name(state, fname);
    ctx.buffer_for_name(&name)
}

/// The directory-join and `qf_guess_filepath` fallback half of
/// `qf_get_fnum` (quickfix.c:2322-2341), without the buffer lookup so a
/// caller can defer buffer creation.
fn resolve_name(state: &mut EfmState, fname: &str) -> String {
    if fname.is_empty() {
        return String::new();
    }
    let Some(dir) = state.directory.clone() else {
        return fname.to_owned();
    };
    if is_abs_name(fname) {
        return fname.to_owned();
    }
    let joined = join_fnames(&dir, fname);
    if path_exists(&joined) {
        return joined;
    }
    // The file may live in a directory further down the stack when a "make"
    // run omitted a "leaving directory" line (quickfix.c:2328-2336).
    match guess_filepath(state, fname) {
        Some(guess) => join_fnames(&guess, fname),
        None => fname.to_owned(),
    }
}

/// `qf_push_dir` (quickfix.c:2364-2424): pushes `dirbuf`, resolving a
/// relative name against the deepest stack entry where it names a real
/// directory. `stack` is bottom-first; the top is the last element.
fn push_dir(stack: &mut Vec<String>, dirbuf: &str, is_file_stack: bool) -> String {
    if is_abs_name(dirbuf) || stack.is_empty() || is_file_stack {
        stack.push(dirbuf.to_owned());
        return dirbuf.to_owned();
    }
    // Relative directory: it must be a subdirectory of one already on the
    // stack; search from the top down (quickfix.c:2380-2397).
    let mut found = None;
    for i in (0..stack.len()).rev() {
        if is_dir(&join_fnames(&stack[i], dirbuf)) {
            found = Some(i);
            break;
        }
    }
    if let Some(i) = found {
        let dirname = join_fnames(&stack[i], dirbuf);
        // Clean up all dirs we already left (quickfix.c:2399-2405).
        stack.truncate(i + 1);
        stack.push(dirname);
    } else {
        // Nothing found: it must be on top level (quickfix.c:2407-2411).
        stack.clear();
        stack.push(dirbuf.to_owned());
    }
    stack.last().cloned().unwrap_or_default()
}

/// `qf_pop_dir` (quickfix.c:2428-2443): pops the top entry and returns the
/// new top, or `None` when the stack is empty.
fn pop_dir(stack: &mut Vec<String>) -> Option<String> {
    stack.pop();
    stack.last().cloned()
}

/// `qf_guess_filepath` (quickfix.c:2475-2507): finds the stack directory
/// containing `filename`, cleaning up intermediate entries.
fn guess_filepath(state: &mut EfmState, filename: &str) -> Option<String> {
    if state.dir_stack.is_empty() {
        return None;
    }
    let top = state.dir_stack.len() - 1;
    // Search below the current top (quickfix.c:2482).
    let mut found = None;
    for i in (0..top).rev() {
        if path_exists(&join_fnames(&state.dir_stack[i], filename)) {
            found = Some(i);
            break;
        }
    }
    // Clean up all dirs we already left (quickfix.c:2498-2504).
    let keep_below = found.map_or(0, |i| i + 1);
    state.dir_stack.drain(keep_below..top);
    found.map(|i| state.dir_stack[i].clone())
}

/// `concat_fnames(dir, file, true)`: joins with one path separator.
fn join_fnames(dir: &str, file: &str) -> String {
    if dir.ends_with('/') || dir.ends_with('\\') {
        format!("{dir}{file}")
    } else {
        format!("{dir}/{file}")
    }
}

/// `vim_isAbsName`: `/…`, `~…`, or a Windows drive/UNC path.
fn is_abs_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.first() == Some(&b'/')
        || b.first() == Some(&b'~')
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'/' | b'\\'))
        || b.starts_with(b"\\\\")
}

/// `os_path_exists`.
fn path_exists(name: &str) -> bool {
    std::path::Path::new(name).exists()
}

/// `os_isdir`.
fn is_dir(name: &str) -> bool {
    std::path::Path::new(name).is_dir()
}

/// `skipwhite`: skips spaces and tabs.
fn skipwhite(s: &str) -> &str {
    s.trim_start_matches([' ', '\t'])
}

/// `atol` on a `\d\+` match: leading digits, saturating.
fn atoi(s: &str) -> i64 {
    let end = s
        .as_bytes()
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    s[..end].parse().unwrap_or(i64::MAX)
}

/// `vim_isprintc` for a single byte: printable ASCII or any non-ASCII byte
/// (upstream checks the full multibyte char; the byte approximation only
/// differs for stray continuation bytes).
fn is_printable(c: u8) -> bool {
    (0x20..0x7f).contains(&c) || c >= 0x80
}

/// Byte-slices the line at a capture's offsets.
fn slice<'a>(line: &'a str, cap: &Capture) -> &'a str {
    line.get(cap.start.byte..cap.end.byte).unwrap_or("")
}

/// `expand_env` on a `%f` match: `~` at the start and `$VAR`/`${VAR}`
/// environment expansion (envvar.c). Unknown variables are left literal.
fn expand_env(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'~' if i == 0 => {
                match std::env::var("HOME") {
                    Ok(home) => out.extend_from_slice(home.as_bytes()),
                    Err(_) => out.push(b'~'),
                }
                i += 1;
            }
            b'$' => {
                if bytes.get(i + 1) == Some(&b'{') {
                    if let Some(rel) = bytes[i + 2..].iter().position(|&b| b == b'}') {
                        let name = &src[i + 2..i + 2 + rel];
                        if let Ok(v) = std::env::var(name) {
                            out.extend_from_slice(v.as_bytes());
                        } else {
                            out.extend_from_slice(&bytes[i..i + 3 + rel]);
                        }
                        i += 3 + rel;
                    } else {
                        out.push(b'$');
                        i += 1;
                    }
                } else {
                    let start = i + 1;
                    let mut end = start;
                    while end < bytes.len()
                        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                    {
                        end += 1;
                    }
                    if end > start {
                        match std::env::var(&src[start..end]) {
                            Ok(v) => out.extend_from_slice(v.as_bytes()),
                            Err(_) => out.extend_from_slice(&bytes[i..end]),
                        }
                        i = end;
                    } else {
                        out.push(b'$');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory `EfmContext`: buffer numbers are assigned in order of first
    /// use; `existing` holds pre-registered buffer numbers for `%b`.
    struct MockCtx {
        names: Vec<String>,
        existing: Vec<i64>,
    }

    impl MockCtx {
        fn new() -> Self {
            Self {
                names: Vec::new(),
                existing: Vec::new(),
            }
        }
    }

    impl EfmContext for MockCtx {
        fn buffer_exists(&mut self, nr: i64) -> bool {
            self.existing.contains(&nr)
        }
        fn buffer_for_name(&mut self, name: &str) -> i64 {
            if let Some(i) = self.names.iter().position(|n| n == name) {
                return i64::try_from(i).unwrap() + 1;
            }
            self.names.push(name.to_owned());
            i64::try_from(self.names.len()).unwrap()
        }
    }

    fn parse_all(spec: &str, lines: &[&str]) -> (Vec<ParsedEntry>, EfmState, MockCtx) {
        let fmt = ErrorFormat::compile(spec).unwrap();
        let mut state = EfmState::default();
        let mut entries = Vec::new();
        let mut ctx = MockCtx::new();
        for line in lines {
            match parse_line(line, &fmt, &mut state, &mut entries, &mut ctx).unwrap() {
                LineOutcome::Entry(e) => entries.push(e),
                LineOutcome::Folded | LineOutcome::Ignored => {}
                LineOutcome::FoldOutside(_) => panic!("fold without target"),
            }
        }
        (entries, state, ctx)
    }

    #[test]
    fn default_efm_parses_file_line_message() {
        // The DFLT_EFM shape: `%f:%l:%m` is one of its alternatives.
        let (entries, _, ctx) = parse_all(
            "%*[^\"]\"%f\"%*\\D%l: %m,%f:%l:%c:%m,%f:%l:%m",
            &["Xfile1:10:Line 10"],
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].lnum, 10);
        assert_eq!(entries[0].text, "Line 10");
        assert!(entries[0].valid);
        assert_eq!(ctx.names, ["Xfile1"]);
    }

    #[test]
    fn ignored_multiscan_return_does_not_leak_into_next_line() {
        // `%O%rx` ignores lines ending in `x` via the tail guard; the flag
        // must not survive into the next line, or the second pattern would
        // be skipped as non-`%O`-family under a leaked multiscan.
        let (entries, _state, _ctx) = parse_all("%O%rx,%f:%l:%m", &["ax", "f:1:msg"]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].lnum, 1);
        assert_eq!(entries[0].text, "msg");
    }

    #[test]
    fn column_format() {
        let (entries, ..) = parse_all("%f:%l:%c:%m", &["x.c:3:7:bad"]);
        assert_eq!(entries[0].lnum, 3);
        assert_eq!(entries[0].col, 7);
        assert_eq!(entries[0].text, "bad");
    }

    #[test]
    fn minus_g_ignores_line() {
        let (entries, ..) = parse_all("%-Gignore%.%#,%f:%l:%m", &["ignore me", "x.c:1:m"]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "m");
    }

    #[test]
    fn multiline_folds_into_one_entry() {
        // %A starts a block, %C continues it (needs %m to capture text),
        // %Z ends it. %Z precedes %C here because alternatives are tried in
        // order and %C's %m would otherwise swallow the terminator.
        let (entries, ..) = parse_all(
            "%A%f:%l:%m,%ZEND,%C%m",
            &["x.c:5:first", "second line", "third", "END"],
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].lnum, 5);
        assert_eq!(entries[0].text, "first\nsecond line\nthird");
    }

    #[test]
    fn directory_stack_resolves_relative_file() {
        let dir = std::env::temp_dir().join(format!("oxvim_efm_{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("x.c"), "int x;\n").unwrap();
        let spec = "%DEntering directory '%f',%f:%l:%m,%XLeaving directory";
        let (entries, state, ctx) = parse_all(
            spec,
            &[
                &format!("Entering directory '{}'", sub.display()),
                "x.c:9:oops",
                "Leaving directory",
            ],
        );
        // %D and %X lines produce invalid entries; the file entry resolves
        // against the pushed directory.
        assert_eq!(entries.len(), 3);
        assert!(!entries[0].valid);
        assert!(entries[1].valid);
        assert_eq!(ctx.names[0], sub.join("x.c").to_string_lossy());
        assert!(state.directory.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pointer_line_sets_visual_column() {
        let (entries, ..) = parse_all("%p^", &["   ^"]);
        assert_eq!(entries[0].col, 4);
        assert!(entries[0].vcol);
    }

    #[test]
    fn invalid_format_char_is_e377() {
        let err = ErrorFormat::compile("%f:%z:%m").unwrap_err();
        assert_eq!(err.code, "E377");
    }

    #[test]
    fn duplicate_format_char_is_e372() {
        let err = ErrorFormat::compile("%f:%f:%m").unwrap_err();
        assert_eq!(err.code, "E372");
    }

    #[test]
    fn empty_spec_is_e378() {
        let err = ErrorFormat::compile("").unwrap_err();
        assert_eq!(err.code, "E378");
    }

    #[test]
    fn nonmatching_line_is_invalid_entry() {
        let (entries, ..) = parse_all("%f:%l:%m", &["garbage"]);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].valid);
        assert_eq!(entries[0].text, "garbage");
        assert_eq!(entries[0].bufnr, 0);
    }

    #[test]
    fn file_stack_supplies_filename() {
        // %P pushes a file name used by later entries that have no %f;
        // %Q pops it (qf_parse_file_pfx, quickfix.c:1673-1690). %P's %f
        // requires the file to exist (quickfix.c:1345-1347).
        let dir = std::env::temp_dir().join(format!("oxvim_efm_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("real.c");
        std::fs::write(&file, "int x;\n").unwrap();
        let spec = "%P[%f],%Q[pop],%f:%l:%m,%l:%m";
        let (entries, state, ctx) = parse_all(
            spec,
            &[&format!("[{}]", file.display()), "5:msg", "[pop]", "7:gone"],
        );
        assert_eq!(entries.len(), 4);
        assert!(!entries[0].valid); // %P line
        assert_eq!(entries[1].lnum, 5);
        assert_eq!(ctx.names, [file.to_string_lossy().into_owned()]);
        // After %Q the current file is gone: the last entry has no buffer.
        assert_eq!(entries[3].bufnr, 0);
        assert!(state.currfile.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
