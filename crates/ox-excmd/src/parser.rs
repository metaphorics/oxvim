//! Byte-oriented parsing of Ex command lines.

use crate::command::{
    AddrType, CommandFlags, NoUserCommands, ResolveError, ResolvedCommand, UserCommandProvider,
    resolve_command,
};
use thiserror::Error;

const MAX_COMMANDS: usize = 1_024;
const MAX_MODIFIERS: usize = 64;
const MAX_OFFSETS: usize = 64;

/// An upstream-compatible error identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    /// E492: Not an editor command.
    E492,
    /// E481: No range allowed.
    E481,
    /// E488: Trailing characters.
    E488,
    /// E471: Argument required.
    E471,
}

impl ErrorCode {
    /// Returns the traditional Vim error code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::E492 => "E492",
            Self::E481 => "E481",
            Self::E488 => "E488",
            Self::E471 => "E471",
        }
    }
}

/// A command-line parse error with an input byte offset.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{code}: {message} at byte {offset}", code = .code.as_str())]
pub struct ParseError {
    /// Upstream error identifier.
    pub code: ErrorCode,
    /// Zero-based byte offset in the original command line.
    pub offset: usize,
    /// Human-readable detail.
    pub message: &'static str,
}

/// Base of one Ex address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AddressBase {
    /// Current line (`.`), including the implicit address in `,addr`.
    Current,
    /// Last line (`$`).
    Last,
    /// Absolute line number.
    Line(u64),
    /// Mark address (`'x`).
    Mark(char),
    /// Forward search (`/pattern/`).
    ForwardSearch(String),
    /// Backward search (`?pattern?`).
    BackwardSearch(String),
}

/// One address and its ordered signed offsets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Address {
    /// Address base.
    pub base: AddressBase,
    /// Signed line offsets; an omitted magnitude is one.
    pub offsets: Vec<i64>,
}

/// Separator between two range addresses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeSeparator {
    /// Comma: evaluate both addresses from the original current line.
    Comma,
    /// Semicolon: make the first address current before evaluating the second.
    Semicolon,
}

/// Shape and evaluation semantics of a parsed range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeKind {
    /// One explicit address.
    Single,
    /// Whole-buffer shorthand (`%`).
    WholeBuffer,
    /// Two addresses and their separator behavior.
    Pair {
        /// Source separator.
        separator: RangeSeparator,
        /// True only for `;`, which advances current before the second address.
        cursor_advance: bool,
    },
}

/// Parsed Ex range.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Range {
    /// First address, absent only for no range.
    pub start: Option<Address>,
    /// Second address for a pair or whole-buffer range.
    pub end: Option<Address>,
    /// Range shape.
    pub kind: RangeKind,
}

/// A recognized command modifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModifierKind {
    /// `aboveleft`.
    AboveLeft,
    /// `belowright`.
    BelowRight,
    /// `botright`.
    BotRight,
    /// `browse`.
    Browse,
    /// `confirm`.
    Confirm,
    /// `filter`.
    Filter,
    /// `hide`.
    Hide,
    /// `horizontal`.
    Horizontal,
    /// `keepalt`.
    KeepAlt,
    /// `keepjumps`.
    KeepJumps,
    /// `keepmarks`.
    KeepMarks,
    /// `keeppatterns`.
    KeepPatterns,
    /// `leftabove`.
    LeftAbove,
    /// `lockmarks`.
    LockMarks,
    /// `noautocmd`.
    NoAutocmd,
    /// `noswapfile`.
    NoSwapfile,
    /// `rightbelow`.
    RightBelow,
    /// `sandbox`.
    Sandbox,
    /// `silent`.
    Silent,
    /// `tab`.
    Tab,
    /// `topleft`.
    TopLeft,
    /// `unsilent`.
    Unsilent,
    /// `verbose`.
    Verbose,
    /// `vertical`.
    Vertical,
}

/// One modifier in source order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandModifier {
    /// Modifier kind.
    pub kind: ModifierKind,
    /// Optional prefix count (`3verbose`, `2tab`).
    pub count: Option<u64>,
    /// Whether the modifier carried `!` (`silent!`, `filter!`).
    pub bang: bool,
    /// Delimited pattern for the `filter` modifier; `None` otherwise.
    pub pattern: Option<String>,
}

/// One parsed command. Execution is intentionally out of scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExCommand {
    /// Resolved built-in or user command.
    pub command: ResolvedCommand,
    /// Ordered command modifiers.
    pub modifiers: Vec<CommandModifier>,
    /// Optional address range.
    pub range: Option<Range>,
    /// Command bang.
    pub bang: bool,
    /// `eap->usefilter` (`ex_docmd.c:2256-2275`): `:read !cmd`, `:read!cmd`,
    /// and `:write !cmd` hand their whole tail to the shell. The `!` that
    /// selected the filter is consumed, so `args` is the shell command and
    /// is never split at `|`.
    pub usefilter: bool,
    /// Post-command count.
    pub count: Option<u64>,
    /// Post-command register.
    pub register: Option<char>,
    /// Uninterpreted argument tail after count/register extraction.
    pub args: String,
    /// Byte range occupied by this command in the original input.
    pub span: std::ops::Range<usize>,
}

/// Stateless Ex command-line parser.
pub struct Parser<'a, P: UserCommandProvider + ?Sized = NoUserCommands> {
    users: &'a P,
}

impl Default for Parser<'static, NoUserCommands> {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser<'static, NoUserCommands> {
    /// Creates a parser without user-defined commands.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            users: &NoUserCommands,
        }
    }
}

impl<'a, P: UserCommandProvider + ?Sized> Parser<'a, P> {
    /// Creates a parser using the host's user-command registry.
    #[must_use]
    pub const fn with_user_commands(users: &'a P) -> Self {
        Self { users }
    }

    /// Parses all bar-separated commands from one command line.
    pub fn parse(&self, input: &str) -> Result<Vec<ExCommand>, ParseError> {
        let mut commands = Vec::new();
        let mut cursor = 0;
        while cursor < input.len() {
            cursor = skip_space_and_colons(input, cursor);
            if cursor >= input.len() || input.as_bytes()[cursor] == b'"' {
                break;
            }
            if commands.len() == MAX_COMMANDS {
                return Err(error(ErrorCode::E488, cursor, "too many commands"));
            }
            let (command, next) = self.parse_one(input, cursor)?;
            commands.push(command);
            cursor = next;
            if cursor < input.len() && input.as_bytes()[cursor] == b'|' {
                cursor += 1;
            }
        }
        Ok(commands)
    }

    fn parse_one(&self, input: &str, start: usize) -> Result<(ExCommand, usize), ParseError> {
        let mut cursor = start;
        let modifiers = parse_modifiers(input, &mut cursor)?;
        cursor = skip_ascii_space(input, cursor);
        let range_offset = cursor;
        let range = parse_range(input, &mut cursor)?;
        cursor = skip_ascii_space(input, cursor);

        let command_offset = cursor;
        let (typed, after_name) = parse_command_name(input, cursor);
        if typed.is_empty() {
            return Err(error(ErrorCode::E492, command_offset, "not an editor command"));
        }
        let command = resolve_command(typed, self.users).map_err(|resolve_error| {
            let message = match resolve_error {
                ResolveError::NotFound => "not an editor command",
                ResolveError::AmbiguousUserCommand => "ambiguous user command",
            };
            error(ErrorCode::E492, command_offset, message)
        })?;
        let flags = effective_flags(&command);
        if range.is_some() && !flags.contains(CommandFlags::RANGE) {
            return Err(error(ErrorCode::E481, range_offset, "No range allowed"));
        }

        cursor = after_name;
        let bang_offset = cursor;
        let mut bang = input.as_bytes().get(cursor) == Some(&b'!');
        if bang {
            if !flags.contains(CommandFlags::BANG) {
                return Err(error(ErrorCode::E488, bang_offset, "trailing characters"));
            }
            cursor += 1;
        }
        cursor = skip_ascii_space(input, cursor);
        // ":r!cmd" spends its bang on the filter, and a "!" standing where
        // ":read"/":write" expect a file name selects the filter too
        // (ex_docmd.c:2256-2275). Either way the "!" is consumed here so the
        // remaining line is one shell command.
        let mut usefilter = false;
        if command.name() == "read" && bang {
            usefilter = true;
            bang = false;
        } else if matches!(command.name(), "read" | "write")
            && input.as_bytes().get(cursor) == Some(&b'!')
        {
            usefilter = true;
            cursor += 1;
        }
        let end = command_end(input, cursor, flags, usefilter, command.name());
        let mut args_start = cursor;
        let mut args_end = end;
        trim_ascii_space(input, &mut args_start, &mut args_end);

        let mut args = input[args_start..args_end].to_owned();
        let register = if flags.contains(CommandFlags::REGSTR) {
            take_register(&mut args)
        } else {
            None
        };
        let count = if flags.contains(CommandFlags::COUNT) {
            take_count(&mut args)
        } else {
            None
        };

        if flags.contains(CommandFlags::NEEDARG) && args.trim().is_empty() {
            return Err(error(ErrorCode::E471, args_start, "argument required"));
        }
        if !flags.contains(CommandFlags::EXTRA)
            && !matches!(command.name(), "append" | "change" | "insert")
            && !args.trim().is_empty()
        {
            return Err(error(ErrorCode::E488, args_start, "trailing characters"));
        }

        Ok((
            ExCommand {
                command,
                modifiers,
                range,
                bang,
                usefilter,
                count,
                register,
                args: args.trim_end().to_owned(),
                span: start..end,
            },
            end,
        ))
    }
}

/// The argument flags that govern one resolved command: a built-in's table
/// entry, or the fixed set upstream gives user commands.
#[must_use]
pub fn effective_flags(command: &ResolvedCommand) -> CommandFlags {
    match command {
        ResolvedCommand::Builtin(spec) => spec.flags,
        ResolvedCommand::User(_) => CommandFlags(
            CommandFlags::RANGE.bits()
                | CommandFlags::BANG.bits()
                | CommandFlags::EXTRA.bits()
                | CommandFlags::TRLBAR.bits(),
        ),
    }
}

/// The address domain that governs one resolved command.
///
/// User commands answer [`AddrType::Lines`], upstream's `-range` default
/// (`usercmd.c:815-818`), matching the `RANGE` that [`effective_flags`]
/// grants them.
#[must_use]
pub fn effective_addr_type(command: &ResolvedCommand) -> AddrType {
    match command {
        ResolvedCommand::Builtin(spec) => spec.addr_type,
        ResolvedCommand::User(_) => AddrType::Lines,
    }
}

fn parse_command_name(input: &str, start: usize) -> (&str, usize) {
    let bytes = input.as_bytes();
    let Some(&first) = bytes.get(start) else {
        return ("", start);
    };
    if is_one_letter_command(bytes, start) {
        return (&input[start..start + 1], start + 1);
    }
    if first.is_ascii_alphabetic() {
        let mut end = start + 1;
        while bytes.get(end).is_some_and(u8::is_ascii_alphabetic) {
            end += 1;
        }
        if first.is_ascii_uppercase() || input[start..end].starts_with("py") {
            while bytes.get(end).is_some_and(u8::is_ascii_alphanumeric) {
                end += 1;
            }
        }
        return (&input[start..end], end);
    }
    if b"@!=><&~#*".contains(&first) {
        return (&input[start..start + 1], start + 1);
    }
    ("", start)
}

fn is_one_letter_command(bytes: &[u8], start: usize) -> bool {
    let at = |offset: usize| bytes.get(start + offset).copied().unwrap_or_default();
    if at(0) == b'k' && (at(1) != b'e' || at(2) != b'e') {
        return true;
    }
    if at(0) != b's' {
        return false;
    }
    let second = at(1);
    (second == b'c'
        && (at(2) == 0
            || (at(2) != b's'
                && at(2) != b'r'
                && (at(3) == 0 || (at(3) != b'i' && at(4) != b'p')))))
        || second == b'g'
        || (second == b'i' && at(2) != b'm' && at(2) != b'l' && at(2) != b'g')
        || second == b'I'
        || (second == b'r' && at(2) != b'e')
}

fn parse_modifiers(input: &str, cursor: &mut usize) -> Result<Vec<CommandModifier>, ParseError> {
    let mut modifiers = Vec::new();
    loop {
        let saved = *cursor;
        let mut probe = skip_ascii_space(input, saved);
        let count_start = probe;
        while input.as_bytes().get(probe).is_some_and(u8::is_ascii_digit) {
            probe += 1;
        }
        let count = if probe > count_start {
            let after_digits = skip_ascii_space(input, probe);
            let parsed = input[count_start..probe].parse::<u64>().ok();
            probe = after_digits;
            parsed
        } else {
            None
        };

        let name_start = probe;
        while input.as_bytes().get(probe).is_some_and(u8::is_ascii_alphabetic) {
            probe += 1;
        }
        let typed = &input[name_start..probe];
        let Some((kind, allows_count)) = modifier(typed) else {
            *cursor = saved;
            break;
        };
        if count.is_some() && !allows_count {
            *cursor = saved;
            break;
        }
        let mut bang = false;
        if input.as_bytes().get(probe) == Some(&b'!')
            && matches!(kind, ModifierKind::Silent | ModifierKind::Filter)
        {
            bang = true;
            probe += 1;
        }
        let mut pattern = None;
        let is_filter = kind == ModifierKind::Filter;
        if is_filter {
            // ":filter {pat} cmd": the pattern is mandatory and belongs to
            // the modifier, so it is consumed and retained here before the
            // nested command is routed (the 'f' case in parse_command_
            // modifiers: ex_docmd.c:2561-2591). Without a pattern, or when
            // no command follows, "filter" is not a modifier at all.
            let pattern_start = skip_ascii_space(input, probe);
            let at_command_end = matches!(
                input.as_bytes().get(pattern_start).copied(),
                None | Some(b'|') | Some(b'"')
            );
            if at_command_end {
                *cursor = saved;
                break;
            }
            let Ok((parsed_pattern, after_pattern)) =
                parse_vimgrep_pattern(input, pattern_start)
            else {
                *cursor = saved;
                break;
            };
            pattern = Some(parsed_pattern);
            probe = after_pattern;
            // Without a following nested command, "filter" is not a modifier.
            let after_pattern_space = skip_ascii_space(input, probe);
            if matches!(
                input.as_bytes().get(after_pattern_space).copied(),
                None | Some(b'|') | Some(b'"')
            ) {
                *cursor = saved;
                break;
            }
        } else if kind == ModifierKind::Hide {
            // ":hide" and ":hide | cmd" stay the builtin command; "hide" is
            // a modifier only when another command follows (the 'h' case in
            // parse_command_modifiers: ex_docmd.c:2594-2603).
            let after_word = skip_ascii_space(input, probe);
            if matches!(input.as_bytes().get(after_word).copied(), None | Some(b'|') | Some(b'"'))
            {
                *cursor = saved;
                break;
            }
        }
        // A modifier must not be a prefix of a longer identifier. "filter"
        // is exempt because probe has advanced past its pattern, where a
        // following identifier is the nested command, not a word extension
        // (":filter /pat/delete" has no separating space).
        if !is_filter && input.as_bytes().get(probe).is_some_and(u8::is_ascii_alphabetic) {
            *cursor = saved;
            break;
        }
        modifiers.push(CommandModifier { kind, count, bang, pattern });
        if modifiers.len() == MAX_MODIFIERS {
            return Err(error(ErrorCode::E488, probe, "too many modifiers"));
        }
        *cursor = skip_ascii_space(input, probe);
    }
    Ok(modifiers)
}

/// Parses one vimgrep-style pattern: a bare identifier word ("pattern fname")
/// or a delimited pattern with optional `g`/`j`/`f` flags ("/pattern/ fname"),
/// returning the pattern text and the cursor just past the pattern.
/// Mirrors `skip_vimgrep_pat`: `ex_cmds.c:4972-5010`.
fn parse_vimgrep_pattern(input: &str, start: usize) -> Result<(String, usize), ParseError> {
    let bytes = input.as_bytes();
    let Some(&first) = bytes.get(start) else {
        return Err(error(ErrorCode::E488, start, "search pattern required"));
    };
    if first.is_ascii_alphanumeric() || first == b'_' {
        // ":filter foo cmd" / ":vimgrep foo fname": bare pattern up to space.
        let pattern_start = start;
        let mut cursor = start;
        while bytes.get(cursor).is_some_and(|byte| !byte.is_ascii_whitespace()) {
            cursor += 1;
        }
        return Ok((input[pattern_start..cursor].to_owned(), cursor));
    }
    // Delimited pattern ":filter /foo/ cmd", optionally followed by flags.
    let (pattern, after) = parse_pattern(input, start, first)?;
    let mut cursor = after;
    while matches!(bytes.get(cursor), Some(b'g') | Some(b'j') | Some(b'f')) {
        cursor += 1;
    }
    Ok((pattern, cursor))
}

/// Whether `command_end` must skip a leading grep pattern before scanning
/// for `|` separators and `"` comments.
fn is_grep_command(name: &str) -> bool {
    matches!(name, "vimgrep" | "lvimgrep" | "vimgrepadd" | "lvimgrepadd")
}

/// Returns the cursor just past a vimgrep-family leading pattern, or
/// `args_start` when no pattern can be skipped (`skip_grep_pat`: ex_docmd.c
/// 3840-3854; used by `separate_nextcmd` at ex_docmd.c:4114).
fn skip_grep_pattern(input: &str, args_start: usize) -> usize {
    match parse_vimgrep_pattern(input, args_start) {
        Ok((_, after)) => after,
        Err(_) => args_start,
    }
}

fn modifier(typed: &str) -> Option<(ModifierKind, bool)> {
    const MODIFIERS: &[(&str, usize, ModifierKind, bool)] = &[
        ("aboveleft", 3, ModifierKind::AboveLeft, false),
        ("belowright", 3, ModifierKind::BelowRight, false),
        ("botright", 2, ModifierKind::BotRight, false),
        ("browse", 3, ModifierKind::Browse, false),
        ("confirm", 4, ModifierKind::Confirm, false),
        ("filter", 4, ModifierKind::Filter, false),
        ("hide", 3, ModifierKind::Hide, false),
        ("horizontal", 3, ModifierKind::Horizontal, false),
        ("keepalt", 5, ModifierKind::KeepAlt, false),
        ("keepjumps", 5, ModifierKind::KeepJumps, false),
        ("keepmarks", 3, ModifierKind::KeepMarks, false),
        ("keeppatterns", 5, ModifierKind::KeepPatterns, false),
        ("leftabove", 5, ModifierKind::LeftAbove, false),
        ("lockmarks", 3, ModifierKind::LockMarks, false),
        ("noautocmd", 3, ModifierKind::NoAutocmd, false),
        ("noswapfile", 3, ModifierKind::NoSwapfile, false),
        ("rightbelow", 6, ModifierKind::RightBelow, false),
        ("sandbox", 3, ModifierKind::Sandbox, false),
        ("silent", 3, ModifierKind::Silent, true),
        ("tab", 3, ModifierKind::Tab, true),
        ("topleft", 2, ModifierKind::TopLeft, false),
        ("unsilent", 3, ModifierKind::Unsilent, false),
        ("verbose", 4, ModifierKind::Verbose, true),
        ("vertical", 4, ModifierKind::Vertical, false),
    ];
    MODIFIERS.iter().find_map(|(name, min_len, kind, count)| {
        (typed.len() >= *min_len && name.starts_with(typed)).then_some((*kind, *count))
    })
}

fn parse_range(input: &str, cursor: &mut usize) -> Result<Option<Range>, ParseError> {
    let start = *cursor;
    if input.as_bytes().get(start) == Some(&b'%') {
        *cursor += 1;
        return Ok(Some(Range {
            start: Some(Address { base: AddressBase::Line(1), offsets: Vec::new() }),
            end: Some(Address { base: AddressBase::Last, offsets: Vec::new() }),
            kind: RangeKind::WholeBuffer,
        }));
    }

    let mut first = parse_address(input, cursor)?;
    let mut last_separator = None;
    let mut cursor_advance = false;
    let mut end = None;
    loop {
        let separator_offset = skip_ascii_space(input, *cursor);
        let separator = match input.as_bytes().get(separator_offset) {
            Some(b',') => RangeSeparator::Comma,
            Some(b';') => RangeSeparator::Semicolon,
            _ => break,
        };
        cursor_advance |= separator == RangeSeparator::Semicolon;
        last_separator = Some(separator);
        *cursor = skip_ascii_space(input, separator_offset + 1);
        let next = parse_address(input, cursor)?.unwrap_or(Address {
            base: AddressBase::Current,
            offsets: Vec::new(),
        });
        if end.is_some() {
            first = end.take();
        } else if first.is_none() {
            first = Some(Address { base: AddressBase::Current, offsets: Vec::new() });
        }
        end = Some(next);
    }
    if first.is_none() && last_separator.is_none() {
        *cursor = start;
        return Ok(None);
    }
    if let Some(separator) = last_separator {
        return Ok(Some(Range {
            start: first,
            end,
            kind: RangeKind::Pair { separator, cursor_advance },
        }));
    }
    Ok(Some(Range { start: first, end: None, kind: RangeKind::Single }))
}

fn parse_address(input: &str, cursor: &mut usize) -> Result<Option<Address>, ParseError> {
    let bytes = input.as_bytes();
    let start = *cursor;
    let base = match bytes.get(*cursor).copied() {
        Some(b'.') => {
            *cursor += 1;
            Some(AddressBase::Current)
        }
        Some(b'$') => {
            *cursor += 1;
            Some(AddressBase::Last)
        }
        Some(b'\'') => {
            let mark_offset = *cursor + 1;
            let Some(mark) = input[mark_offset..].chars().next() else {
                return Err(error(ErrorCode::E488, *cursor, "mark name required"));
            };
            *cursor = mark_offset + mark.len_utf8();
            Some(AddressBase::Mark(mark))
        }
        Some(b'/') | Some(b'?') => {
            let delimiter = bytes[*cursor];
            let (pattern, end) = parse_pattern(input, *cursor, delimiter)?;
            *cursor = end;
            if delimiter == b'/' {
                Some(AddressBase::ForwardSearch(pattern))
            } else {
                Some(AddressBase::BackwardSearch(pattern))
            }
        }
        Some(digit) if digit.is_ascii_digit() => {
            let number_start = *cursor;
            while bytes.get(*cursor).is_some_and(u8::is_ascii_digit) {
                *cursor += 1;
            }
            let number = input[number_start..*cursor]
                .parse::<u64>()
                .map_err(|_| error(ErrorCode::E488, number_start, "line number is too large"))?;
            Some(AddressBase::Line(number))
        }
        Some(b'+') | Some(b'-') => Some(AddressBase::Current),
        _ => None,
    };
    let Some(base) = base else {
        return Ok(None);
    };
    let mut offsets = Vec::new();
    while matches!(bytes.get(*cursor), Some(b'+') | Some(b'-')) {
        if offsets.len() == MAX_OFFSETS {
            return Err(error(ErrorCode::E488, *cursor, "too many address offsets"));
        }
        let sign = if bytes[*cursor] == b'+' { 1_i64 } else { -1_i64 };
        *cursor += 1;
        let magnitude_start = *cursor;
        while bytes.get(*cursor).is_some_and(u8::is_ascii_digit) {
            *cursor += 1;
        }
        let magnitude = if magnitude_start == *cursor {
            1
        } else {
            input[magnitude_start..*cursor]
                .parse::<i64>()
                .map_err(|_| error(ErrorCode::E488, magnitude_start, "offset is too large"))?
        };
        offsets.push(sign * magnitude);
    }
    if *cursor == start {
        return Ok(None);
    }
    Ok(Some(Address { base, offsets }))
}

fn parse_pattern(input: &str, start: usize, delimiter: u8) -> Result<(String, usize), ParseError> {
    let bytes = input.as_bytes();
    let mut cursor = start + 1;
    let pattern_start = cursor;
    let mut escaped = false;
    while let Some(&byte) = bytes.get(cursor) {
        if !escaped && byte == delimiter {
            return Ok((input[pattern_start..cursor].to_owned(), cursor + 1));
        }
        escaped = !escaped && byte == b'\\';
        if byte != b'\\' {
            escaped = false;
        }
        cursor += 1;
    }
    Err(error(ErrorCode::E488, start, "unterminated search pattern"))
}

fn command_end(
    input: &str,
    args_start: usize,
    flags: CommandFlags,
    usefilter: bool,
    name: &str,
) -> usize {
    if matches!(name, "append" | "change" | "insert") {
        return input.len();
    }
    // ":read !cmd" and ":write !cmd" own the rest of the line: upstream skips
    // separate_nextcmd for them (ex_docmd.c:2291-2313), so a "|" inside the
    // shell command is not a command separator.
    if usefilter {
        return input.len();
    }
    if matches!(name, "execute" | "echo" | "echon" | "echomsg" | "echoerr") {
        return expression_command_end(input, args_start);
    }
    let is_substitute = name == "substitute";
    if !flags.contains(CommandFlags::TRLBAR) && !is_substitute {
        return input.len();
    }
    let bytes = input.as_bytes();
    let mut escaped = false;
    let mut cursor = args_start;
    // vimgrep family patterns are regexes that may contain `|`; skip the
    // leading pattern (plus g/j/f flags) before scanning for bar separators
    // and quote comments, so ":vimgrep /foo|bar/ f | copen" splits after the
    // file argument (separate_nextcmd: ex_docmd.c:4112-4165).
    if is_grep_command(name) {
        cursor = skip_grep_pattern(input, args_start);
    }
    while let Some(&byte) = bytes.get(cursor) {
        if escaped {
            escaped = false;
            cursor += 1;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            cursor += 1;
            continue;
        }
        if byte == b'|' {
            return cursor;
        }
        if byte == b'"'
            && !flags.contains(CommandFlags::NOTRLCOM)
            && !is_comment_quote_exception(bytes, args_start, cursor, flags, name)
        {
            return cursor;
        }
        cursor += 1;
    }
    input.len()
}

fn expression_command_end(input: &str, start: usize) -> usize {
    let bytes = input.as_bytes();
    let mut cursor = start;
    let mut quote = None;
    let mut escaped = false;
    let mut nesting = 0_usize;
    while let Some(&byte) = bytes.get(cursor) {
        if let Some(delimiter) = quote {
            if delimiter == b'\'' && byte == b'\'' && bytes.get(cursor + 1) == Some(&b'\'') {
                cursor += 2;
                continue;
            }
            if escaped {
                escaped = false;
            } else if delimiter == b'"' && byte == b'\\' {
                escaped = true;
            } else if byte == delimiter {
                quote = None;
            }
            cursor += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'(' | b'[' | b'{' => nesting = nesting.saturating_add(1),
            b')' | b']' | b'}' => nesting = nesting.saturating_sub(1),
            b'|' if nesting == 0
                && bytes.get(cursor + 1) != Some(&b'|')
                && (cursor == start || bytes.get(cursor - 1) != Some(&b'|')) =>
            {
                return cursor;
            }
            _ => {}
        }
        cursor += 1;
    }
    input.len()
}

fn is_comment_quote_exception(
    bytes: &[u8],
    args_start: usize,
    cursor: usize,
    flags: CommandFlags,
    command_name: &str,
) -> bool {
    is_initial_quoted_register(bytes, args_start, cursor, flags)
        || (command_name == "@" && cursor == args_start)
        || (command_name == "redir"
            && cursor == args_start + 1
            && bytes.get(args_start) == Some(&b'@'))
}

fn is_initial_quoted_register(
    bytes: &[u8],
    args_start: usize,
    cursor: usize,
    flags: CommandFlags,
) -> bool {
    flags.contains(CommandFlags::REGSTR)
        && cursor == args_start
        && bytes
            .get(cursor + 1)
            .copied()
            .is_some_and(|byte| is_register(char::from(byte)))
}

fn take_register(args: &mut String) -> Option<char> {
    let trimmed = args.trim_start();
    let skipped = args.len() - trimmed.len();
    let mut chars = trimmed.char_indices();
    let (first_offset, first) = chars.next()?;
    let (register, consumed) = if first == '"' {
        let (offset, register) = chars.next()?;
        (register, offset + register.len_utf8())
    } else {
        let next = chars.next();
        if next.is_some_and(|(_, character)| !character.is_ascii_whitespace()) {
            return None;
        }
        (first, first_offset + first.len_utf8())
    };
    if !is_register(register) {
        return None;
    }
    args.drain(..skipped + consumed);
    *args = args.trim_start().to_owned();
    Some(register)
}

fn is_register(character: char) -> bool {
    character.is_ascii_alphanumeric() || "\"-:.%#=*+_/@".contains(character)
}

fn take_count(args: &mut String) -> Option<u64> {
    let trimmed = args.trim_start();
    let skipped = args.len() - trimmed.len();
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || trimmed.as_bytes().get(digits).is_some_and(|byte| !byte.is_ascii_whitespace()) {
        return None;
    }
    let count = trimmed[..digits].parse::<u64>().ok()?;
    args.drain(..skipped + digits);
    *args = args.trim_start().to_owned();
    Some(count)
}

fn skip_space_and_colons(input: &str, mut cursor: usize) -> usize {
    loop {
        cursor = skip_ascii_space(input, cursor);
        if input.as_bytes().get(cursor) == Some(&b':') {
            cursor += 1;
        } else {
            return cursor;
        }
    }
}

fn skip_ascii_space(input: &str, mut cursor: usize) -> usize {
    while input.as_bytes().get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    cursor
}

fn trim_ascii_space(input: &str, start: &mut usize, end: &mut usize) {
    *start = skip_ascii_space(input, *start);
    while *end > *start && input.as_bytes()[*end - 1].is_ascii_whitespace() {
        *end -= 1;
    }
}

fn error(code: ErrorCode, offset: usize, message: &'static str) -> ParseError {
    ParseError { code, offset, message }
}
