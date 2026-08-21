//! Mode-aware mappings and insert-mode abbreviations.
//!
//! Local-first and longest-prefix behavior follows `src/nvim/input.c:2319-2438`.
//! Abbreviation validation and word-boundary scanning follow
//! `src/nvim/mapping.c:624-629,1455-1529`.

use std::ops::{BitOr, BitOrAssign};

use ox_excmd::{ExCommand, ParseError, Parser};
use ox_types::BufHandle;
use thiserror::Error;

use crate::typeahead::{Keys, Typeahead};

/// One mapping mode accepted by the `:map` family.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u16)]
pub enum MapMode {
    /// Normal mode (`n`).
    Normal = 1 << 0,
    /// Visual mode (`v`).
    Visual = 1 << 1,
    /// Select mode (`s`).
    Select = 1 << 2,
    /// Operator-pending mode (`o`).
    OperatorPending = 1 << 3,
    /// Insert mode (`i`).
    Insert = 1 << 4,
    /// Command-line mode (`c`).
    CommandLine = 1 << 5,
    /// Language-argument mode (`l`).
    LangArg = 1 << 6,
    /// Terminal mode (`t`).
    Terminal = 1 << 7,
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
    ExCommands(Vec<ExCommand>),
    /// Expression identity evaluated by the host.
    Expr(u64),
    /// Host callback identity.
    Callback(u64),
    /// `<Nop>` consumes the lhs without producing input.
    Nop,
}

impl MappingAction {
    /// Parses command-shaped mapping right-hand sides and encodes all others as keys.
    pub fn parse_rhs(rhs: &str) -> Result<Self, MappingError> {
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
                .map(Self::ExCommands)
                .map_err(MappingError::ExCommand);
        }
        Ok(Self::Keys(Keys::from(rhs)))
    }
}

/// Registration flags preserved for the input loop.
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

/// Host seam for `<expr>` mappings.
pub trait MappingExprSink {
    /// Host evaluation failure.
    type Error;
    /// Evaluates the callback identity into replacement keys.
    fn evaluate(&mut self, callback: u64) -> Result<Keys, Self::Error>;
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
