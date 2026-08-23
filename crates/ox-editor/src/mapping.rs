//! Mode-aware mappings and insert-mode abbreviations.
//!
//! Local-first and longest-prefix behavior follows `src/nvim/input.c:2319-2438`.
//! Abbreviation validation and word-boundary scanning follow
//! `src/nvim/mapping.c:624-629,1455-1529`.

use std::ops::{BitOr, BitOrAssign};

use ox_excmd::{ExCommand, ParseError, Parser};
use ox_types::BufHandle;
use thiserror::Error;

use crate::script::SourceContext;
use crate::typeahead::{Keys, Typeahead};

/// One mapping mode accepted by the `:map` family.
///
/// The discriminants are upstream's `MODE_*` bits (`state_defs.h:21-28`), not a
/// private numbering: `maparg()`'s `mode_bits` key reports them verbatim
/// (`mapblock_fill_dict`, `mapping.c:2143`), so any other assignment would
/// need a translation table beside them that can drift.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u16)]
pub enum MapMode {
    /// Normal mode (`n`), `MODE_NORMAL`.
    Normal = 0x01,
    /// Visual mode (`x`), `MODE_VISUAL`.
    Visual = 0x02,
    /// Operator-pending mode (`o`), `MODE_OP_PENDING`.
    OperatorPending = 0x04,
    /// Command-line mode (`c`), `MODE_CMDLINE`.
    CommandLine = 0x08,
    /// Insert mode (`i`), `MODE_INSERT`.
    Insert = 0x10,
    /// Language-argument mode (`l`), `MODE_LANGMAP`.
    LangArg = 0x20,
    /// Select mode (`s`), `MODE_SELECT`.
    Select = 0x40,
    /// Terminal mode (`t`), `MODE_TERMINAL`.
    Terminal = 0x80,
}

/// Compact set of mapping modes.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct MapModes(u16);

impl MapModes {
    /// Empty mode set.
    pub const NONE: Self = Self(0);
    /// Every supported mode.
    pub const ALL: Self = Self(0xff);
    /// Traditional `:map` modes.
    pub const MAP: Self = Self(
        MapMode::Normal as u16
            | MapMode::Visual as u16
            | MapMode::Select as u16
            | MapMode::OperatorPending as u16,
    );
    /// Traditional `:map!` modes.
    pub const MAP_BANG: Self = Self(MapMode::Insert as u16 | MapMode::CommandLine as u16);

    /// Creates a singleton mode set.
    #[must_use]
    pub const fn one(mode: MapMode) -> Self {
        Self(mode as u16)
    }

    /// Whether this set contains a mode.
    #[must_use]
    pub const fn contains(self, mode: MapMode) -> bool {
        self.0 & mode as u16 != 0
    }

    /// Whether two mode sets overlap.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Removes every mode in `other`.
    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether no modes are present.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw `MODE_*` bit set, which `maparg()` reports as `mode_bits`
    /// (`mapping.c:2143`).
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// `map_mode_to_chars` (`mapping.c:170-208`): the mode characters
    /// `:map`'s listing and `maparg()`'s `mode` key show. Never longer than
    /// six characters, hence upstream's seven-byte buffer.
    #[must_use]
    pub fn to_chars(self) -> String {
        let mut out = String::with_capacity(6);
        if self.contains(MapMode::Insert) && self.contains(MapMode::CommandLine) {
            out.push('!');
        } else if self.contains(MapMode::Insert) {
            out.push('i');
        } else if self.contains(MapMode::LangArg) {
            out.push('l');
        } else if self.contains(MapMode::CommandLine) {
            out.push('c');
        } else if Self(self.0 & Self::MAP.0) == Self::MAP {
            out.push(' ');
        } else {
            if self.contains(MapMode::Normal) {
                out.push('n');
            }
            if self.contains(MapMode::OperatorPending) {
                out.push('o');
            }
            if self.contains(MapMode::Terminal) {
                out.push('t');
            }
            if self.contains(MapMode::Visual) && self.contains(MapMode::Select) {
                out.push('v');
            } else {
                if self.contains(MapMode::Visual) {
                    out.push('x');
                }
                if self.contains(MapMode::Select) {
                    out.push('s');
                }
            }
        }
        out
    }

    /// `get_map_mode` (`mapping.c:988-1023`) over a mode string rather than a
    /// command name: only the first character decides, an unrecognized or
    /// empty string means `:map`, and `n` followed by `o` is `:noremap`
    /// rather than `:nmap`.
    #[must_use]
    pub fn from_mode_string(mode: &str) -> Self {
        let bytes = mode.as_bytes();
        match bytes.first().copied() {
            Some(b'i') => Self::one(MapMode::Insert),
            Some(b'l') => Self::one(MapMode::LangArg),
            Some(b'c') => Self::one(MapMode::CommandLine),
            Some(b'n') if bytes.get(1) != Some(&b'o') => Self::one(MapMode::Normal),
            Some(b'v') => Self::one(MapMode::Visual) | Self::one(MapMode::Select),
            Some(b'x') => Self::one(MapMode::Visual),
            Some(b's') => Self::one(MapMode::Select),
            Some(b'o') => Self::one(MapMode::OperatorPending),
            Some(b't') => Self::one(MapMode::Terminal),
            _ => Self::MAP,
        }
    }

    /// `MAP_HASH` (`mapping.c:75-78`): the `maphash[]` bucket a mapping with
    /// this mode set and first lhs byte lives in. `:map`'s listing walks the
    /// buckets in ascending order, so this is the primary sort key of every
    /// listing.
    #[must_use]
    pub const fn hash_bucket(self, first: u8) -> u16 {
        const HASHED: u16 = MapMode::Normal as u16
            | MapMode::Visual as u16
            | MapMode::Select as u16
            | MapMode::OperatorPending as u16
            | MapMode::Terminal as u16;
        if self.0 & HASHED != 0 { first as u16 } else { first as u16 ^ 0x80 }
    }
}

impl From<MapMode> for MapModes {
    fn from(value: MapMode) -> Self {
        Self::one(value)
    }
}

impl BitOr for MapModes {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOr<MapMode> for MapMode {
    type Output = MapModes;

    fn bitor(self, rhs: MapMode) -> Self::Output {
        MapModes::one(self) | MapModes::one(rhs)
    }
}

impl BitOrAssign for MapModes {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Global or buffer-local mapping scope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MapScope {
    /// Process-editor global mapping.
    #[default]
    Global,
    /// Mapping visible only while one buffer is current.
    Buffer(BufHandle),
}

/// Deferred right-hand-side behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MappingAction {
    /// Encoded replacement keys.
    Keys(Keys),
    /// Parsed `<Cmd>...<CR>` or `:...<CR>` Ex commands.
    ///
    /// `keys` is the same right-hand side as `commands` before it was parsed —
    /// upstream's `m_str`, which `maparg()`'s string form and `:map`'s listing
    /// both render. Parsing loses it (a `Vec<ExCommand>` does not print back
    /// to its source text), so it is carried here rather than reconstructed.
    ExCommands {
        /// The right-hand side after `replace_termcodes`, upstream's `m_str`.
        keys: Keys,
        /// The same text parsed into the Ex commands the mapping runs.
        commands: Vec<ExCommand>,
    },
    /// `<expr>` right-hand side, re-evaluated into replacement keys on every
    /// use (`str_to_mapargs`'s `expr` flag, `mapping.c:439-443`).
    Expr(String),
    /// Host callback identity.
    Callback(u64),
    /// `<Nop>` consumes the lhs without producing input.
    Nop,
}
impl MappingAction {
    /// Parses command-shaped mapping right-hand sides and decodes all others
    /// as key notation ([`Keys::parse_notation`]).
    ///
    /// The `<Cmd>`/`:` forms are recognized *before* the notation pass, so the
    /// `<CR>` that terminates them stays a terminator rather than becoming a
    /// carriage return inside the command text.
    pub fn parse_rhs(rhs: &str, leader: &str, local_leader: &str) -> Result<Self, MappingError> {
        if rhs.eq_ignore_ascii_case("<nop>") {
            return Ok(Self::Nop);
        }
        let command = rhs
            .strip_prefix("<Cmd>")
            .and_then(|body| body.strip_suffix("<CR>"))
            .or_else(|| rhs.strip_prefix(':').and_then(|body| body.strip_suffix("<CR>")));
        if let Some(command) = command {
            return Parser::new()
                .parse(command)
                .map(|commands| Self::ExCommands {
                    keys: Keys::parse_notation(rhs, leader, local_leader),
                    commands,
                })
                .map_err(MappingError::ExCommand);
        }
        Ok(Self::Keys(Keys::parse_notation(rhs, leader, local_leader)))
    }

    /// The right-hand side after `replace_termcodes`, upstream's
    /// `mapblock_T.m_str`, which `maparg()`'s string form and `:map`'s listing
    /// render through `str2special`.
    ///
    /// `None` is upstream's `m_luaref != LUA_NOREF`: a callback has no key
    /// string at all.
    #[must_use]
    pub fn replaced_keys(&self) -> Option<&[u8]> {
        match self {
            Self::Keys(keys) | Self::ExCommands { keys, .. } => Some(keys.as_bytes()),
            // `<expr>` stores the expression itself as `m_str` (`do_map` runs
            // `replace_termcodes` over every right-hand side before
            // `map_add`), so it renders the same way.
            Self::Expr(text) => Some(text.as_bytes()),
            // `<Nop>` is an empty `m_str`, which is what makes `showmap` and
            // `get_maparg` print the literal `<Nop>` for it.
            Self::Nop => Some(&[]),
            Self::Callback(_) => None,
        }
    }
}

/// Everything `:map` recorded about one mapping besides its lhs and its
/// decoded action: the flags the input loop reads, and the text and script
/// context `maparg()` and `:map`'s listing report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MappingOptions {
    /// Active modes.
    pub modes: MapModes,
    /// Global or buffer-local scope.
    pub scope: MapScope,
    /// Whether produced keys may themselves be mapped.
    pub remap: bool,
    /// Prefer a complete match immediately despite longer candidates.
    pub nowait: bool,
    /// Suppress command echo while the mapping runs.
    pub silent: bool,
    /// Optional user-facing description.
    pub description: Option<String>,
    /// Whether `<script>` was given (`REMAP_SCRIPT`, `mapping.c:2108,2129`).
    ///
    /// It restricts remapping to `<SID>` mappings, and this port has no
    /// script-local mappings, so execution folds it into [`Self::remap`] being
    /// false. The flag is still recorded because `maparg()`'s `script` key is
    /// the only thing that can tell `<script>` from `:noremap` apart.
    pub script: bool,
    /// The right-hand side exactly as written, before `<>` notation was
    /// decoded (`mapblock_T.m_orig_str`), which is what `maparg()`'s `rhs`
    /// key reports in its compatible form (`mapping.c:2114-2117`).
    pub orig_rhs: String,
    /// Script context of the `:map` that defined this mapping
    /// (`mapblock_T.m_script_ctx`), reported as `maparg()`'s `sid` and `lnum`.
    /// All zeroes when no script was sourcing, as upstream's `current_sctx` is
    /// then at the command line.
    pub script_context: SourceContext,
}

impl Default for MappingOptions {
    fn default() -> Self {
        Self {
            modes: MapModes::MAP,
            scope: MapScope::Global,
            remap: true,
            nowait: false,
            silent: false,
            description: None,
            script: false,
            orig_rhs: String::new(),
            script_context: SourceContext::default(),
        }
    }
}

/// One registered mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mapping {
    /// Encoded left-hand side.
    pub lhs: Keys,
    /// Deferred right-hand-side action.
    pub action: MappingAction,
    /// Registration flags.
    pub options: MappingOptions,
    sequence: u64,
}

/// Prefix lookup result for the input loop's timeout decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lookup<'a> {
    /// Longest complete mapping; number of encoded bytes to consume.
    Exact(&'a Mapping, usize),
    /// More bytes may select a longer mapping. A complete fallback may be present.
    Prefix(Option<&'a Mapping>),
    /// No mapping shares the queued prefix.
    None,
}

/// One insert-mode abbreviation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Abbreviation {
    /// Literal trigger text.
    pub lhs: String,
    /// Deferred replacement behavior.
    pub action: MappingAction,
    /// Global or buffer-local scope.
    pub scope: MapScope,
    /// Whether produced keys may be remapped.
    pub remap: bool,
    sequence: u64,
}

/// Invalid mapping or abbreviation request.
#[derive(Debug, Error)]
pub enum MappingError {
    /// Mapping lhs was empty.
    #[error("mapping lhs must not be empty")]
    EmptyLhs,
    /// No modes were selected.
    #[error("mapping mode set must not be empty")]
    EmptyModes,
    /// Abbreviation contains whitespace or crosses keyword classes.
    #[error("invalid abbreviation lhs {0:?}")]
    InvalidAbbreviation(String),
    /// Command-shaped rhs did not parse.
    #[error(transparent)]
    ExCommand(#[from] ParseError),
}

/// Editor-owned mapping and abbreviation tables.
#[derive(Clone, Debug)]
pub struct Mappings {
    mappings: Vec<Mapping>,
    abbreviations: Vec<Abbreviation>,
    next_sequence: u64,
    timeout_len_ms: u32,
}

impl Default for Mappings {
    fn default() -> Self {
        Self::new()
    }
}

impl Mappings {
    /// Creates empty tables with upstream's default `timeoutlen` data value.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mappings: Vec::new(),
            abbreviations: Vec::new(),
            next_sequence: 1,
            timeout_len_ms: 1_000,
        }
    }

    /// Current mapping ambiguity timeout. Timer wiring belongs to the input loop.
    #[must_use]
    pub const fn timeout_len_ms(&self) -> u32 {
        self.timeout_len_ms
    }

    /// Updates timeout data without starting a timer.
    pub const fn set_timeout_len_ms(&mut self, value: u32) {
        self.timeout_len_ms = value;
    }

    /// Whether a mapping with exactly this lhs already covers any of `modes`
    /// in `scope`.
    ///
    /// `<unique>`'s rejection (`do_map`'s `retval = 5`, `mapping.c:802`, which
    /// becomes `E227`). Upstream also rejects a *prefix* overlap and a
    /// buffer-local definition shadowed by a global one; this answers the
    /// exact-lhs case, which is the one `:map <unique>` is written for.
    #[must_use]
    pub fn conflicts(&self, lhs: &Keys, modes: MapModes, scope: MapScope) -> bool {
        self.mappings.iter().any(|mapping| {
            &mapping.lhs == lhs
                && mapping.options.scope == scope
                && mapping.options.modes.intersects(modes)
        })
    }

    /// Defines or replaces overlapping mode bits for one lhs and scope.
    pub fn map(
        &mut self,
        lhs: Keys,
        action: MappingAction,
        options: MappingOptions,
    ) -> Result<(), MappingError> {
        if lhs.is_empty() {
            return Err(MappingError::EmptyLhs);
        }
        if options.modes.is_empty() {
            return Err(MappingError::EmptyModes);
        }
        for mapping in &mut self.mappings {
            if mapping.lhs == lhs && mapping.options.scope == options.scope {
                mapping.options.modes = mapping.options.modes.without(options.modes);
            }
        }
        self.mappings
            .retain(|mapping| !mapping.options.modes.is_empty());
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.mappings.push(Mapping {
            lhs,
            action,
            options,
            sequence,
        });
        Ok(())
    }

    /// Defines a noremap without a boolean selector at call sites.
    pub fn noremap(
        &mut self,
        lhs: Keys,
        action: MappingAction,
        mut options: MappingOptions,
    ) -> Result<(), MappingError> {
        options.remap = false;
        self.map(lhs, action, options)
    }

    /// Removes matching mode bits from one lhs and scope.
    pub fn unmap(&mut self, lhs: &Keys, modes: MapModes, scope: MapScope) -> usize {
        let mut changed = 0;
        for mapping in &mut self.mappings {
            if &mapping.lhs == lhs
                && mapping.options.scope == scope
                && mapping.options.modes.intersects(modes)
            {
                mapping.options.modes = mapping.options.modes.without(modes);
                changed += 1;
            }
        }
        self.mappings
            .retain(|mapping| !mapping.options.modes.is_empty());
        changed
    }

    /// Implements `mapclear` for selected modes and one scope.
    pub fn mapclear(&mut self, modes: MapModes, scope: MapScope) -> usize {
        let mut changed = 0;
        for mapping in &mut self.mappings {
            if mapping.options.scope == scope && mapping.options.modes.intersects(modes) {
                mapping.options.modes = mapping.options.modes.without(modes);
                changed += 1;
            }
        }
        self.mappings
            .retain(|mapping| !mapping.options.modes.is_empty());
        changed
    }

    /// `check_map` with `exact` set (`mapping.c:2010-2061`): the mapping whose
    /// lhs is exactly `lhs` and whose mode set overlaps `modes`, buffer-local
    /// table searched before the global one. The `bool` is upstream's
    /// `local_ptr`, which `maparg()` reports as `buffer`.
    ///
    /// Upstream returns the *first* hit in hash order rather than the longest
    /// or newest; with an exact length test at most one entry per scope can
    /// match, because [`Self::map`] never keeps two entries with the same lhs,
    /// scope and overlapping modes.
    #[must_use]
    pub fn find_exact(
        &self,
        lhs: &[u8],
        modes: MapModes,
        buffer: Option<BufHandle>,
    ) -> Option<(&Mapping, bool)> {
        buffer
            .and_then(|buffer| {
                self.exact_in_scope(lhs, modes, MapScope::Buffer(buffer))
                    .map(|mapping| (mapping, true))
            })
            .or_else(|| {
                self.exact_in_scope(lhs, modes, MapScope::Global)
                    .map(|mapping| (mapping, false))
            })
    }

    /// `do_map`'s listing passes (`mapping.c:698-726,746-793`): every mapping
    /// whose mode set overlaps `modes` and whose lhs and `lhs` are a prefix of
    /// one another — upstream compares only `min(keylen, len)` bytes, so an
    /// empty `lhs` matches everything.
    ///
    /// The order is the order upstream prints in: the buffer-local table
    /// first, then within each table the `maphash[]` buckets ascending
    /// ([`MapModes::hash_bucket`]) and, inside one bucket, newest first
    /// because `map_add` pushes onto the bucket head (`mapping.c:545-547`).
    #[must_use]
    pub fn matching(
        &self,
        lhs: &[u8],
        modes: MapModes,
        buffer: Option<BufHandle>,
    ) -> Vec<(&Mapping, bool)> {
        let mut found: Vec<(&Mapping, bool)> = self
            .mappings
            .iter()
            .filter(|mapping| mapping.options.modes.intersects(modes))
            .filter(|mapping| {
                let keys = mapping.lhs.as_bytes();
                let shared = keys.len().min(lhs.len());
                keys[..shared] == lhs[..shared]
            })
            .filter_map(|mapping| match mapping.options.scope {
                MapScope::Global => Some((mapping, false)),
                MapScope::Buffer(handle) => (Some(handle) == buffer).then_some((mapping, true)),
            })
            .collect();
        found.sort_by_key(|(mapping, local)| {
            let bucket = mapping
                .lhs
                .as_bytes()
                .first()
                .map_or(0, |first| mapping.options.modes.hash_bucket(*first));
            (!*local, bucket, u64::MAX - mapping.sequence)
        });
        found
    }

    fn exact_in_scope(&self, lhs: &[u8], modes: MapModes, scope: MapScope) -> Option<&Mapping> {
        self.mappings.iter().find(|mapping| {
            mapping.options.scope == scope
                && mapping.options.modes.intersects(modes)
                && mapping.lhs.as_bytes() == lhs
        })
    }

    /// Looks up the typeahead bytes using local-first, longest-prefix rules.
    #[must_use]
    pub fn lookup(
        &self,
        typeahead: &[u8],
        mode: MapMode,
        buffer: Option<BufHandle>,
    ) -> Lookup<'_> {
        if let Some(buffer) = buffer {
            let local = self.lookup_scope(typeahead, mode, MapScope::Buffer(buffer));
            if local != Lookup::None {
                return local;
            }
        }
        self.lookup_scope(typeahead, mode, MapScope::Global)
    }

    /// Convenience lookup against the editor's typeahead stack.
    #[must_use]
    pub fn lookup_typeahead(
        &self,
        typeahead: &Typeahead,
        mode: MapMode,
        buffer: Option<BufHandle>,
    ) -> Lookup<'_> {
        self.lookup(typeahead.as_bytes(), mode, buffer)
    }

    /// Registers an insert-mode abbreviation.
    pub fn abbreviate(
        &mut self,
        lhs: &str,
        action: MappingAction,
        scope: MapScope,
        remap: bool,
    ) -> Result<(), MappingError> {
        if !valid_abbreviation(lhs) {
            return Err(MappingError::InvalidAbbreviation(lhs.to_owned()));
        }
        self.abbreviations
            .retain(|entry| !(entry.lhs == lhs && entry.scope == scope));
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.abbreviations.push(Abbreviation {
            lhs: lhs.to_owned(),
            action,
            scope,
            remap,
            sequence,
        });
        Ok(())
    }

    /// Removes one abbreviation in one scope.
    pub fn unabbreviate(&mut self, lhs: &str, scope: MapScope) -> bool {
        let before = self.abbreviations.len();
        self.abbreviations
            .retain(|entry| !(entry.lhs == lhs && entry.scope == scope));
        before != self.abbreviations.len()
    }

    /// Clears abbreviations in exactly one scope.
    pub fn abbrevclear(&mut self, scope: MapScope) -> usize {
        let before = self.abbreviations.len();
        self.abbreviations.retain(|entry| entry.scope != scope);
        before - self.abbreviations.len()
    }

    /// Resolves an abbreviation before inserting a typed delimiter.
    #[must_use]
    pub fn lookup_abbreviation(
        &self,
        before_cursor: &str,
        typed: char,
        buffer: Option<BufHandle>,
    ) -> Option<&Abbreviation> {
        if is_keyword(typed) {
            return None;
        }
        if let Some(buffer) = buffer {
            if let Some(found) = self.lookup_abbreviation_scope(
                before_cursor,
                MapScope::Buffer(buffer),
            ) {
                return Some(found);
            }
        }
        self.lookup_abbreviation_scope(before_cursor, MapScope::Global)
    }

    /// Removes local mappings and abbreviations when a buffer is wiped.
    pub fn remove_buffer(&mut self, buffer: BufHandle) {
        let scope = MapScope::Buffer(buffer);
        self.mappings.retain(|mapping| mapping.options.scope != scope);
        self.abbreviations.retain(|entry| entry.scope != scope);
    }

    /// Number of mapping entries.
    #[must_use]
    pub fn mapping_len(&self) -> usize {
        self.mappings.len()
    }

    /// Number of abbreviation entries.
    #[must_use]
    pub fn abbreviation_len(&self) -> usize {
        self.abbreviations.len()
    }

    fn lookup_scope(&self, input: &[u8], mode: MapMode, scope: MapScope) -> Lookup<'_> {
        let mut full: Option<&Mapping> = None;
        let mut longer = false;
        for mapping in self
            .mappings
            .iter()
            .filter(|mapping| mapping.options.scope == scope && mapping.options.modes.contains(mode))
        {
            let lhs = mapping.lhs.as_bytes();
            if lhs.starts_with(input) && lhs.len() > input.len() {
                longer = true;
            }
            if input.starts_with(lhs)
                && full.is_none_or(|found| {
                    lhs.len() > found.lhs.len()
                        || (lhs.len() == found.lhs.len() && mapping.sequence > found.sequence)
                })
            {
                full = Some(mapping);
            }
        }
        if longer && full.is_none_or(|mapping| !mapping.options.nowait) {
            return Lookup::Prefix(full);
        }
        full.map_or(Lookup::None, |mapping| Lookup::Exact(mapping, mapping.lhs.len()))
    }

    fn lookup_abbreviation_scope(
        &self,
        before_cursor: &str,
        scope: MapScope,
    ) -> Option<&Abbreviation> {
        self.abbreviations
            .iter()
            .filter(|entry| entry.scope == scope && before_cursor.ends_with(&entry.lhs))
            .filter(|entry| abbreviation_boundary(before_cursor, &entry.lhs))
            .max_by_key(|entry| (entry.lhs.len(), entry.sequence))
    }
}

fn valid_abbreviation(lhs: &str) -> bool {
    if lhs.is_empty() || lhs.chars().any(char::is_whitespace) {
        return false;
    }
    let mut characters = lhs.chars().peekable();
    let Some(first) = characters.next() else {
        return false;
    };
    let prefix_class = is_keyword(first);
    let mut last = first;
    let mut count = 1usize;
    let mut prefix_uniform = true;
    while let Some(ch) = characters.next() {
        if characters.peek().is_some() {
            prefix_uniform &= is_keyword(ch) == prefix_class;
        }
        last = ch;
        count += 1;
    }
    !is_keyword(last) || count <= 2 || prefix_uniform
}

fn abbreviation_boundary(before_cursor: &str, lhs: &str) -> bool {
    let prefix_len = before_cursor.len().saturating_sub(lhs.len());
    let prefix = &before_cursor[..prefix_len];
    let Some(first) = lhs.chars().next() else {
        return false;
    };
    let Some(last) = lhs.chars().next_back() else {
        return false;
    };
    let Some(previous) = prefix.chars().next_back() else {
        return true;
    };
    if !is_keyword(last) {
        previous.is_whitespace()
    } else {
        previous.is_whitespace() || is_keyword(previous) != is_keyword(first)
    }
}

fn is_keyword(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}
