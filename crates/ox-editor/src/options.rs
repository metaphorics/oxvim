//! Generated option metadata and scoped runtime option values.

use std::collections::HashMap;

use ox_types::{BufHandle, WinHandle};
use thiserror::Error;

/// A layer on which an option may be read or written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionScope {
    /// The editor-wide value.
    Global,
    /// A buffer-local value.
    Buffer,
    /// A window-local value.
    Window,
    /// A tab-local value described by upstream metadata.
    ///
    /// `OptionStore` does not expose tab overlays because the editor core does
    /// not yet have a public tab handle.
    Tab,
}

/// The runtime representation required by an option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionType {
    /// A true or false value.
    Boolean,
    /// A signed integer value.
    Number,
    /// Text, including upstream callback and expression options.
    String,
}

/// The grammar attached to a string-list option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionListKind {
    /// A comma-separated list that may contain empty items.
    Comma,
    /// A comma-separated list without empty items.
    OneComma,
    /// A comma-separated list whose items may have `name:value` form.
    CommaColon,
    /// A comma-separated list without empty items, optionally in `name:value` form.
    OneCommaColon,
    /// A sequence of unique single-character flags.
    Flags,
    /// A comma-separated list of unique single-character flags.
    FlagsComma,
}

/// A literal default captured from the upstream option description.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionDefaultValue {
    /// A boolean literal.
    Boolean(bool),
    /// An integer literal.
    Number(i64),
    /// A string literal.
    String(&'static str),
}

/// The build-time default metadata for an option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptionDefault {
    /// The usable literal default, if upstream supplied one directly.
    pub value: Option<OptionDefaultValue>,
    /// The exact upstream Lua expression.
    pub raw: &'static str,
    /// Whether `raw` was a conditional defaults table.
    pub conditional: bool,
    /// Whether upstream supplied a default at all.
    pub present: bool,
}

/// Static metadata for one canonical option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptionMetadata {
    /// The canonical long name.
    pub name: &'static str,
    /// The conventional short name, when one exists.
    pub short_name: Option<&'static str>,
    /// Additional accepted historical names.
    pub aliases: &'static [&'static str],
    /// Layers on which callers may set the option.
    pub scopes: &'static [OptionScope],
    /// The runtime value type.
    pub value_type: OptionType,
    /// The original upstream type (`func` and `expr` remain distinguishable).
    pub upstream_type: &'static str,
    /// Literal and raw default information.
    pub default: OptionDefault,
    /// Optional string-list grammar.
    pub list: Option<OptionListKind>,
    /// Whether repeated comma-list items are rejected.
    pub deny_duplicates: bool,
    /// `:set` expands environment variables and `~` in string values
    /// (option.c `kOptFlagExpand`, the `expand` key in options.lua).
    pub expand: bool,
}

include!(concat!(env!("OUT_DIR"), "/options_metadata.rs"));

/// An owned runtime option value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OptionValue {
    /// A true or false value.
    Boolean(bool),
    /// A signed integer value.
    Number(i64),
    /// Text, including callback names and expressions.
    String(String),
}

impl OptionValue {
    /// Returns the metadata type represented by this value.
    #[must_use]
    pub const fn value_type(&self) -> OptionType {
        match self {
            Self::Boolean(_) => OptionType::Boolean,
            Self::Number(_) => OptionType::Number,
            Self::String(_) => OptionType::String,
        }
    }
}

impl From<OptionDefaultValue> for OptionValue {
    fn from(value: OptionDefaultValue) -> Self {
        match value {
            OptionDefaultValue::Boolean(value) => Self::Boolean(value),
            OptionDefaultValue::Number(value) => Self::Number(value),
            OptionDefaultValue::String(value) => Self::String(value.to_owned()),
        }
    }
}
impl OptionMetadata {
    /// Whether this option has no global baseline (`noglob` in options.lua).
    ///
    /// `:setglobal` on these options is accepted, but it does not alter the
    /// value inherited by new buffers or windows.
    #[must_use]
    pub fn no_global(&self) -> bool {
        matches!(
            self.name,
            "bufhidden"
                | "buflisted"
                | "buftype"
                | "busy"
                | "diff"
                | "filetype"
                | "modifiable"
                | "previewwindow"
                | "readonly"
                | "syntax"
        )
    }
}

/// A checked option lookup or update failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OptionError {
    /// No canonical name, short name, or alias matched the request.
    #[error("unknown option `{0}`")]
    UnknownOption(String),
    /// The requested storage layer is not declared for the option.
    #[error("option `{name}` cannot be used at {requested:?} scope")]
    WrongScope {
        /// The canonical option name.
        name: &'static str,
        /// The rejected layer.
        requested: OptionScope,
    },
    /// The supplied value has a different runtime type.
    #[error("option `{name}` requires {expected:?}, not {actual:?}")]
    TypeMismatch {
        /// The canonical option name.
        name: &'static str,
        /// The declared runtime type.
        expected: OptionType,
        /// The supplied runtime type.
        actual: OptionType,
    },
    /// A string value does not satisfy its declared list grammar.
    #[error("invalid value for option `{name}`: {reason}")]
    InvalidList {
        /// The canonical option name.
        name: &'static str,
        /// A stable explanation suitable for user-facing diagnostics.
        reason: &'static str,
    },
    /// Upstream describes the default with an expression that cannot be
    /// expanded safely at build time.
    #[error("option `{name}` has no literal runtime default (upstream: {raw})")]
    DefaultUnavailable {
        /// The canonical option name.
        name: &'static str,
        /// The preserved upstream Lua expression.
        raw: &'static str,
    },
}

/// Global option baselines plus buffer-local and window-local overlays.
///
/// All names are canonicalized before storage, so aliases never create a
/// second value. Local reads fall back to the captured global baseline.
#[derive(Clone, Debug)]
pub struct OptionStore {
    global: HashMap<&'static str, OptionValue>,
    buffers: HashMap<BufHandle, HashMap<&'static str, OptionValue>>,
    windows: HashMap<WinHandle, HashMap<&'static str, OptionValue>>,
}

impl Default for OptionStore {
    fn default() -> Self {
        let mut global = HashMap::with_capacity(OPTION_COUNT);
        for metadata in OPTION_METADATA {
            if let Some(value) = metadata.default.value {
                global.insert(metadata.name, value.into());
            }
        }
        Self {
            global,
            buffers: HashMap::new(),
            windows: HashMap::new(),
        }
    }
}

impl OptionStore {
    /// Creates a store initialized with every statically known upstream default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolves a canonical name, short name, or historical alias.
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if no canonical name, short name,
    /// or alias matches `name`.
    pub fn metadata(name: &str) -> Result<&'static OptionMetadata, OptionError> {
        option_metadata(name).ok_or_else(|| OptionError::UnknownOption(name.to_owned()))
    }

    /// Reads an editor-wide option.
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if `name` is not recognized, or
    /// [`OptionError::WrongScope`] if the option is not declared at global scope.
    /// Returns [`OptionError::DefaultUnavailable`] if no baseline value exists.
    pub fn get_global(&self, name: &str) -> Result<&OptionValue, OptionError> {
        let metadata = Self::metadata(name)?;
        require_scope(metadata, OptionScope::Global)?;
        self.baseline(metadata)
    }
    /// Reads the inherited global baseline for any option, regardless of the
    /// scopes it is declared with.  New buffers and windows fall back to this
    /// value when they have no local overlay, so `:setglobal` on a local option
    /// updates the baseline they inherit.
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if `name` is not recognized, or
    /// [`OptionError::DefaultUnavailable`] if no baseline value exists.
    pub fn get_global_baseline(&self, name: &str) -> Result<&OptionValue, OptionError> {
        let metadata = Self::metadata(name)?;
        self.baseline(metadata)
    }

    /// Reads a buffer-local option, falling back to its global baseline.
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if `name` is not recognized, or
    /// [`OptionError::WrongScope`] if the option is not declared at buffer scope.
    /// Returns [`OptionError::DefaultUnavailable`] if no baseline value exists.
    pub fn get_buffer(&self, buffer: BufHandle, name: &str) -> Result<&OptionValue, OptionError> {
        let metadata = Self::metadata(name)?;
        require_scope(metadata, OptionScope::Buffer)?;
        if let Some(value) = self
            .buffers
            .get(&buffer)
            .and_then(|values| values.get(metadata.name))
        {
            return Ok(value);
        }
        self.baseline(metadata)
    }

    /// Reads a window-local option, falling back to its global baseline.
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if `name` is not recognized, or
    /// [`OptionError::WrongScope`] if the option is not declared at window scope.
    /// Returns [`OptionError::DefaultUnavailable`] if no baseline value exists.
    pub fn get_window(&self, window: WinHandle, name: &str) -> Result<&OptionValue, OptionError> {
        let metadata = Self::metadata(name)?;
        require_scope(metadata, OptionScope::Window)?;
        if let Some(value) = self
            .windows
            .get(&window)
            .and_then(|values| values.get(metadata.name))
        {
            return Ok(value);
        }
        self.baseline(metadata)
    }

    /// Sets an editor-wide option after type and list validation.
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if `name` is not recognized,
    /// [`OptionError::WrongScope`] if the option is not declared at global scope,
    /// [`OptionError::TypeMismatch`] if the value type differs, or
    /// [`OptionError::InvalidList`] if a string-list value is malformed.
    pub fn set_global(&mut self, name: &str, value: OptionValue) -> Result<(), OptionError> {
        let metadata = Self::metadata(name)?;
        require_scope(metadata, OptionScope::Global)?;
        validate_value(metadata, &value)?;
        self.global.insert(metadata.name, value);
        Ok(())
    }

    /// Updates the global baseline for a buffer-local or window-local option
    /// that has no global scope, bypassing the scope check that `set_global`
    /// enforces. Options flagged `noglob` in options.lua have no global
    /// baseline, so this call validates the value but does not store it (`:setglobal`
    /// on these options is accepted and has no effect).
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if `name` is not recognized,
    /// [`OptionError::TypeMismatch`] if the value type differs, or
    /// [`OptionError::InvalidList`] if a string-list value is malformed.
    pub fn set_global_default(
        &mut self,
        name: &str,
        value: OptionValue,
    ) -> Result<(), OptionError> {
        let metadata = Self::metadata(name)?;
        validate_value(metadata, &value)?;
        if !metadata.no_global() {
            self.global.insert(metadata.name, value);
        }
        Ok(())
    }

    /// Sets a buffer-local overlay after type and list validation.
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if `name` is not recognized,
    /// [`OptionError::WrongScope`] if the option is not declared at buffer scope,
    /// [`OptionError::TypeMismatch`] if the value type differs, or
    /// [`OptionError::InvalidList`] if a string-list value is malformed.
    pub fn set_buffer(
        &mut self,
        buffer: BufHandle,
        name: &str,
        value: OptionValue,
    ) -> Result<(), OptionError> {
        let metadata = Self::metadata(name)?;
        require_scope(metadata, OptionScope::Buffer)?;
        validate_value(metadata, &value)?;
        self.buffers
            .entry(buffer)
            .or_default()
            .insert(metadata.name, value);
        Ok(())
    }

    /// Sets a window-local overlay after type and list validation.
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if `name` is not recognized,
    /// [`OptionError::WrongScope`] if the option is not declared at window scope,
    /// [`OptionError::TypeMismatch`] if the value type differs, or
    /// [`OptionError::InvalidList`] if a string-list value is malformed.
    pub fn set_window(
        &mut self,
        window: WinHandle,
        name: &str,
        value: OptionValue,
    ) -> Result<(), OptionError> {
        let metadata = Self::metadata(name)?;
        require_scope(metadata, OptionScope::Window)?;
        validate_value(metadata, &value)?;
        self.windows
            .entry(window)
            .or_default()
            .insert(metadata.name, value);
        Ok(())
    }

    /// Removes a buffer-local overlay and restores fallback behavior.
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if `name` is not recognized, or
    /// [`OptionError::WrongScope`] if the option is not declared at buffer scope.
    pub fn clear_buffer(&mut self, buffer: BufHandle, name: &str) -> Result<bool, OptionError> {
        let metadata = Self::metadata(name)?;
        require_scope(metadata, OptionScope::Buffer)?;
        let removed = self
            .buffers
            .get_mut(&buffer)
            .is_some_and(|values| values.remove(metadata.name).is_some());
        if self.buffers.get(&buffer).is_some_and(HashMap::is_empty) {
            self.buffers.remove(&buffer);
        }
        Ok(removed)
    }

    /// Removes a window-local overlay and restores fallback behavior.
    ///
    /// # Errors
    ///
    /// Returns [`OptionError::UnknownOption`] if `name` is not recognized, or
    /// [`OptionError::WrongScope`] if the option is not declared at window scope.
    pub fn clear_window(&mut self, window: WinHandle, name: &str) -> Result<bool, OptionError> {
        let metadata = Self::metadata(name)?;
        require_scope(metadata, OptionScope::Window)?;
        let removed = self
            .windows
            .get_mut(&window)
            .is_some_and(|values| values.remove(metadata.name).is_some());
        if self.windows.get(&window).is_some_and(HashMap::is_empty) {
            self.windows.remove(&window);
        }
        Ok(removed)
    }

    /// Drops every local value owned by a wiped buffer.
    pub fn remove_buffer(&mut self, buffer: BufHandle) {
        self.buffers.remove(&buffer);
    }

    /// Copies current global baselines for every buffer-scoped option that has
    /// a global baseline into a new buffer's local overlay, so the buffer
    /// inherits the live global defaults rather than the static compile-time
    /// defaults. Upstream `buf_alloc` (`buffer.c`) seeds each new buffer from
    /// the current global option values; this method mirrors that snapshot.
    /// Options flagged `noglob` in options.lua are skipped because they have no
    /// global value to inherit.
    pub fn snapshot_buffer_defaults(&mut self, buffer: BufHandle) {
        let mut overlay = HashMap::new();
        for metadata in OPTION_METADATA {
            if metadata.scopes.contains(&OptionScope::Buffer)
                && !metadata.no_global()
                && let Some(value) = self.global.get(metadata.name)
            {
                // Global-local integer options use a sentinel "unset" value
                // for the local copy on new buffers (upstream `option.c:388-391`
                // sets `b_p_ul = NO_LOCAL_UNDOLEVEL`).  Only `undolevels`
                // carries the -123_456 sentinel; other global+buf options
                // inherit the global value.
                if metadata.name == "undolevels" {
                    overlay.insert(metadata.name, OptionValue::Number(-123_456));
                } else {
                    overlay.insert(metadata.name, value.clone());
                }
            }
        }
        if !overlay.is_empty() {
            self.buffers.insert(buffer, overlay);
        }
    }

    /// Drops every local value owned by a closed window.
    pub fn remove_window(&mut self, window: WinHandle) {
        self.windows.remove(&window);
    }

    fn baseline(&self, metadata: &'static OptionMetadata) -> Result<&OptionValue, OptionError> {
        self.global
            .get(metadata.name)
            .ok_or(OptionError::DefaultUnavailable {
                name: metadata.name,
                raw: metadata.default.raw,
            })
    }
}

fn require_scope(
    metadata: &'static OptionMetadata,
    requested: OptionScope,
) -> Result<(), OptionError> {
    if metadata.scopes.contains(&requested) {
        Ok(())
    } else {
        Err(OptionError::WrongScope {
            name: metadata.name,
            requested,
        })
    }
}

fn validate_value(
    metadata: &'static OptionMetadata,
    value: &OptionValue,
) -> Result<(), OptionError> {
    let actual = value.value_type();
    if actual != metadata.value_type {
        return Err(OptionError::TypeMismatch {
            name: metadata.name,
            expected: metadata.value_type,
            actual,
        });
    }

    let (Some(kind), OptionValue::String(value)) = (metadata.list, value) else {
        return Ok(());
    };
    validate_list(kind, value).map_err(|reason| {
        OptionError::InvalidList {
            name: metadata.name,
            reason,
        }
    })
}

fn validate_list(kind: OptionListKind, value: &str) -> Result<(), &'static str> {
    match kind {
        OptionListKind::Flags => validate_flags(value),
        OptionListKind::FlagsComma => validate_comma_flags(value),
        OptionListKind::Comma
        | OptionListKind::OneComma
        | OptionListKind::CommaColon
        | OptionListKind::OneCommaColon => {
            let reject_empty = matches!(
                kind,
                OptionListKind::OneComma | OptionListKind::OneCommaColon
            );
            let check_colon = matches!(
                kind,
                OptionListKind::CommaColon | OptionListKind::OneCommaColon
            );
            let items = CommaItems::new(value);
            for item in items {
                if reject_empty && !value.is_empty() && item.is_empty() {
                    return Err("empty comma-list item");
                }
                if check_colon && !item.is_empty() {
                    validate_colon_item(item)?;
                }
            }
            // Upstream allows duplicates on plain set (`:set path=.,,`
            // keeps both empties); `deny_duplicates` only drops items on
            // `+=`/`^=`, which `modify_comma_list` already handles.
            Ok(())
        }
    }
}

fn validate_flags(value: &str) -> Result<(), &'static str> {
    for (offset, flag) in value.char_indices() {
        if flag == ',' {
            return Err("flags list must not contain commas");
        }
        if value[..offset].chars().any(|previous| previous == flag) {
            return Err("duplicate flag");
        }
    }
    Ok(())
}

fn validate_comma_flags(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Ok(());
    }
    for item in CommaItems::new(value) {
        if item.chars().count() != 1 {
            return Err("each comma-separated flag must be one character");
        }
    }
    if has_duplicate_items(value) {
        return Err("duplicate flag");
    }
    Ok(())
}

fn validate_colon_item(item: &str) -> Result<(), &'static str> {
    let Some(colon) = find_unescaped(item, ':') else {
        return Ok(());
    };
    if colon == 0 || colon + 1 == item.len() {
        Err("colon-list items require text on both sides of `:`")
    } else {
        Ok(())
    }
}

fn has_duplicate_items(value: &str) -> bool {
    for (index, item) in CommaItems::new(value).enumerate() {
        if CommaItems::new(value)
            .skip(index + 1)
            .any(|other| other == item)
        {
            return true;
        }
    }
    false
}

pub(crate) fn find_unescaped(value: &str, needle: char) -> Option<usize> {
    let mut escaped = false;
    for (offset, character) in value.char_indices() {
        if character == needle && !escaped {
            return Some(offset);
        }
        if character == '\\' {
            escaped = !escaped;
        } else {
            escaped = false;
        }
    }
    None
}

pub(crate) struct CommaItems<'a> {
    remaining: Option<&'a str>,
}

impl<'a> CommaItems<'a> {
    pub(crate) fn new(value: &'a str) -> Self {
        Self {
            remaining: Some(value),
        }
    }
}

impl<'a> Iterator for CommaItems<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let remaining = self.remaining.take()?;
        let mut escaped = false;
        for (offset, character) in remaining.char_indices() {
            if character == ',' && !escaped {
                let next = offset + character.len_utf8();
                self.remaining = Some(&remaining[next..]);
                return Some(&remaining[..offset]);
            }
            if character == '\\' {
                escaped = !escaped;
            } else {
                escaped = false;
            }
        }
        Some(remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::{OptionDefaultValue, option_metadata};

    #[test]
    fn cpo_has_vim_literal_default() {
        let metadata = option_metadata("cpo").expect("cpo is a canonical option");
        assert_eq!(
            metadata.default.value,
            Some(OptionDefaultValue::String("aABceFs_"))
        );
    }
}

#[test]
fn new_buffer_inherits_current_global_for_buffer_local_option() {
    use super::{OptionStore, OptionValue};
    use ox_types::BufHandle;
    let mut store = OptionStore::new();
    // autoindent is buffer-only; its compile-time default is true.
    // Lower the global baseline to false, then snapshot into a new buffer.
    store
        .set_global_default("autoindent", OptionValue::Boolean(false))
        .unwrap();
    let buf = BufHandle::try_from(1).unwrap();
    store.snapshot_buffer_defaults(buf);
    assert_eq!(
        store.get_buffer(buf, "autoindent").unwrap(),
        &OptionValue::Boolean(false),
        "new buffer should inherit the current global baseline"
    );
}

#[test]
fn new_buffer_uses_static_default_when_no_global_set() {
    use super::{OptionStore, OptionValue};
    use ox_types::BufHandle;
    let store = OptionStore::new();
    // Without any set_global_default, the baseline is the compile-time
    // default (autoindent = true). snapshot copies that into the overlay.
    let mut store = store;
    let buf = BufHandle::try_from(1).unwrap();
    store.snapshot_buffer_defaults(buf);
    assert_eq!(
        store.get_buffer(buf, "autoindent").unwrap(),
        &OptionValue::Boolean(true),
        "new buffer should get the static default when no global override"
    );
}

#[test]
fn setlocal_does_not_change_global_baseline() {
    use super::{OptionStore, OptionValue};
    use ox_types::BufHandle;
    let mut store = OptionStore::new();
    store
        .set_global_default("autoindent", OptionValue::Boolean(false))
        .unwrap();
    let buf1 = BufHandle::try_from(1).unwrap();
    store.snapshot_buffer_defaults(buf1);
    // setlocal autoindent on buf1
    store
        .set_buffer(buf1, "autoindent", OptionValue::Boolean(true))
        .unwrap();
    // global baseline should still be false
    let buf2 = BufHandle::try_from(2).unwrap();
    store.snapshot_buffer_defaults(buf2);
    assert_eq!(
        store.get_buffer(buf2, "autoindent").unwrap(),
        &OptionValue::Boolean(false),
        "setlocal must not change the global baseline inherited by new buffers"
    );
    assert_eq!(
        store.get_buffer(buf1, "autoindent").unwrap(),
        &OptionValue::Boolean(true),
        "setlocal should set the buffer overlay"
    );
}

#[test]
fn set_global_default_does_not_weaken_set_global_scope_check() {
    use super::{OptionError, OptionStore, OptionValue};
    let mut store = OptionStore::new();
    // set_global must still reject buffer-only options.
    let result = store.set_global("autoindent", OptionValue::Boolean(false));
    assert!(
        matches!(result, Err(OptionError::WrongScope { .. })),
        "set_global must reject buffer-only options"
    );
    // set_global_default accepts it.
    store
        .set_global_default("autoindent", OptionValue::Boolean(false))
        .unwrap();
}

#[test]
fn set_global_default_is_no_op_for_noglobal_options() {
    use super::{OptionStore, OptionValue};
    let mut store = OptionStore::new();
    // modifiable is a `noglob` buffer-local option with a default. :setglobal
    // on it is accepted, but it must not update the global baseline.
    store
        .set_global_default("modifiable", OptionValue::Boolean(false))
        .unwrap();
    assert_eq!(
        store.get_global_baseline("modifiable").unwrap(),
        &OptionValue::Boolean(true),
        "set_global_default on a noglob option must not store the value"
    );
    let buf = ox_types::BufHandle::try_from(1).unwrap();
    store.snapshot_buffer_defaults(buf);
    assert_eq!(
        store.get_buffer(buf, "modifiable").unwrap(),
        &OptionValue::Boolean(true),
        "new buffer must not inherit a noglob global baseline"
    );
}

#[test]
fn get_global_baseline_reads_any_option() {
    use super::{OptionStore, OptionValue};
    let store = OptionStore::new();
    // autoindent is buffer-only; get_global rejects it, but the baseline
    // (the value new buffers inherit) is still readable.
    assert_eq!(
        store.get_global_baseline("autoindent").unwrap(),
        &OptionValue::Boolean(true)
    );
    // get_global still rejects the buffer-only option.
    assert!(matches!(
        store.get_global("autoindent"),
        Err(super::OptionError::WrongScope { .. })
    ));
}
