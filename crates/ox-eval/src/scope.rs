//! Variable scope model for the Vimscript expression evaluation. Each scope
//! namespace is an ordered `Vec<(OxStr, Typval)>`; Vimscript dictionaries and
//! scope tables are insertion-ordered and compare keys as raw bytes (no UTF-8
//! decoding is assumed).
//!
//! Unqualified variable lookups follow Vim's internal-variable resolution
//! order: `l:`, then `a:`, then `g:`. The `v:` and `a:` namespaces are
//! read-only for normal assignment; writing to them produces E46.
//!
//! Missing variables produce E121; `islocked` consults `:lockvar` marks.

use std::cell::Cell;

use ox_types::{BufHandle, DictEntry, DictEntryFlags, EntryValueLock, OxStr, Typval};

use crate::error::{EvalError, Result};

/// Scope namespace prefixes recognized in Vimscript variable names.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ScopeKind {
    /// `g:` — global variables.
    Global,
    /// `b:` — buffer-local variables.
    Buffer,
    /// `w:` — window-local variables.
    Window,
    /// `t:` — tab-local variables.
    Tab,
    /// `s:` — script-local variables.
    Script,
    /// `l:` — function-local variables.
    Local,
    /// `a:` — function arguments (read-only once bound).
    Argument,
    /// `v:` — Vim internal variables (read-only).
    Vim,
}

impl ScopeKind {
    /// The single-byte prefix used in source text (`g`, `b`, `w`, ...).
    #[must_use]
    pub const fn as_byte(&self) -> u8 {
        match self {
            Self::Global => b'g',
            Self::Buffer => b'b',
            Self::Window => b'w',
            Self::Tab => b't',
            Self::Script => b's',
            Self::Local => b'l',
            Self::Argument => b'a',
            Self::Vim => b'v',
        }
    }

    /// The textual prefix as it appears in diagnostics (`g:`, `b:`, ...).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "g:",
            Self::Buffer => "b:",
            Self::Window => "w:",
            Self::Tab => "t:",
            Self::Script => "s:",
            Self::Local => "l:",
            Self::Argument => "a:",
            Self::Vim => "v:",
        }
    }

    /// Parse a one-character namespace prefix.
    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            b'g' => Self::Global,
            b'b' => Self::Buffer,
            b'w' => Self::Window,
            b't' => Self::Tab,
            b's' => Self::Script,
            b'l' => Self::Local,
            b'a' => Self::Argument,
            b'v' => Self::Vim,
            _ => return None,
        })
    }
}

/// An ordered map of byte-keyed variables to [`Typval`] values.
///
/// Vimscript dictionaries and scope tables are insertion-ordered. We use
/// `Vec<(OxStr, Typval)>` directly because `ox_types::Dict` is
/// `Object`-valued rather than `Typval`-valued.
pub type ScopeMap = Vec<(OxStr, Typval)>;

/// Deep snapshot of a scope map for the write-back mirror. Containers are
/// copied by value (cycle-aware, like `:h copy()`'s `deepcopy()`), so an
/// in-place container mutation through an aliased read (`call add(g:l, x)`)
/// differs from the mirror at sync time instead of hiding inside shared
/// backing. A value too deep to copy (`E698`) falls back to a shared clone:
/// only pathological nesting keeps the old blindness, never an error.
#[must_use]
pub fn snapshot_map(map: &ScopeMap) -> ScopeMap {
    map.iter()
        .map(|(key, value)| {
            (
                key.clone(),
                super::builtins::deep_copy(value).unwrap_or_else(|_| value.clone()),
            )
        })
        .collect()
}

/// Option namespace for `&`, `&g:`, and `&l:` forms.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum OptionScope {
    /// `&g:` — the global option value.
    Global,
    /// `&l:` — the local option value.
    Local,
    /// `&` — the effective option value (local if set, otherwise global).
    Effective,
}

/// A full set of Vimscript scopes plus option and register maps.
///
/// `Scope` is cheap to clone: closures and partial applications capture the
/// entire table as a snapshot, which is the intended Vimscript semantics.
/// The environment is deliberately not stored here: `$VAR` reads run against
/// the live process environment through [`Scope::get_env`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scope {
    /// `g:` global variables.
    pub global: ScopeMap,
    /// `b:` buffer-local variables.
    pub buffer: ScopeMap,
    /// `w:` window-local variables.
    pub window: ScopeMap,
    /// `t:` tab-local variables.
    pub tab: ScopeMap,
    /// `s:` script-local variables.
    pub script: ScopeMap,
    /// `l:` function-local variables.
    pub local: ScopeMap,
    /// `a:` function arguments.
    pub argument: ScopeMap,
    /// `v:` Vim internal variables.
    pub vim: ScopeMap,
    /// `&g:` global option values.
    pub options_global: ScopeMap,
    /// `&l:` local option values.
    pub options_local: ScopeMap,
    /// `@r` register contents.
    pub registers: ScopeMap,
    /// Baseline `g:` snapshot backing merge-on-write: `sync_scope_into_editor`
    /// writes back only keys changed since this mirror, so a reentrant
    /// executor's concurrent additions survive the outer sync. Bookkeeping
    /// like the stamps: writable through `&Scope`.
    pub global_mirror: std::cell::RefCell<ScopeMap>,
    /// Variables `:lockvar` marked, upstream's `DI_FLAGS_LOCK`.
    pub locked: Vec<LockMark>,
    /// Editor variable-map stamps recorded by the differential sync; a map
    /// whose stamp still matches the editor is not re-read, so shared
    /// values — and the mutability metadata on their dictionary entries —
    /// survive across commands.
    pub synced: SyncVersions,
}

/// The variable-map versions a [`Scope`] last mirrored from the editor.
///
/// `Cell` keeps the stamps writable through `&Scope`, matching how the sync
/// helper receives the scope; the stamps are bookkeeping and take no part in
/// Vimscript semantics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SyncVersions {
    // Editor variable-map version stamps.
    global: Cell<u64>,
    window: Cell<u64>,
    tab: Cell<u64>,
    vim: Cell<u64>,
    // Buffer identity + version tracking: a `current_buffer()` switch
    // invalidates the cached `b:` map even when versions coincide.
    buffer_handle: Cell<Option<BufHandle>>,
    buffer_version: Cell<u64>,
    // Per-kind dirty flags: a scope-side write the editor has not received.
    dirty_global: Cell<bool>,
    dirty_buffer: Cell<bool>,
    dirty_window: Cell<bool>,
    dirty_tab: Cell<bool>,
    dirty_vim: Cell<bool>,
}

impl SyncVersions {
    /// EXHAUSTIVE MUTATION SURFACE (gated by the #15d design doc):
    /// 1. `Scope::set_scoped` → calls [`SyncVersions::mark_dirty`].
    /// 2. Direct field writes in `sync_editor_into_scope`
    ///    (`excmd_exec.rs`) → call [`SyncVersions::clear_dirty`] (fresh mirror)
    ///    or [`SyncVersions::mark_dirty`] (post-read repair).
    /// 3. `replace_scope_pair` / `remove_scope_pair` on a synced map →
    ///    callers mark the owning kind dirty.
    /// 4. Compound operators (`+=`, `.=`) and `:for` targets route through
    ///    `set_scoped` (covered by 1).
    /// 5. Future `:unlet`-style removals → must call [`SyncVersions::mark_dirty`].
    ///
    /// AUDIT: grep for `scope\.(global|buffer|window|tab|vim)\s*=`,
    /// `replace_scope_pair(&mut scope\.`, and `remove_scope_pair(&mut scope\.`
    /// to verify no unmarked mutations exist.
    ///
    /// The recorded stamp for one editor-synced map.
    #[must_use]
    pub fn get(&self, kind: ScopeKind) -> u64 {
        match kind {
            ScopeKind::Global => self.global.get(),
            ScopeKind::Window => self.window.get(),
            ScopeKind::Tab => self.tab.get(),
            ScopeKind::Vim => self.vim.get(),
            _ => 0,
        }
    }

    /// Record the stamp for one editor-synced map.
    pub fn set(&self, kind: ScopeKind, version: u64) {
        match kind {
            ScopeKind::Global => self.global.set(version),
            ScopeKind::Window => self.window.set(version),
            ScopeKind::Tab => self.tab.set(version),
            ScopeKind::Vim => self.vim.set(version),
            _ => {}
        }
    }

    /// The buffer whose variables the cached `b:` map mirrors, if any.
    #[must_use]
    pub fn buffer_identity(&self) -> Option<BufHandle> {
        self.buffer_handle.get()
    }

    /// Record which buffer the cached `b:` map mirrors.
    pub fn set_buffer_identity(&self, handle: Option<BufHandle>) {
        self.buffer_handle.set(handle);
    }

    /// The `BufferState::variables_version` the cached `b:` map mirrors.
    #[must_use]
    pub fn buffer_version(&self) -> u64 {
        self.buffer_version.get()
    }

    /// Record the buffer variable-map version the cached `b:` map mirrors.
    pub fn set_buffer_version(&self, version: u64) {
        self.buffer_version.set(version);
    }

    /// Mark one synced map as holding a scope-side write the editor has not
    /// received. Kinds the editor does not sync (`s:`, `l:`, `a:`) are no-ops.
    pub fn mark_dirty(&self, kind: ScopeKind) {
        match kind {
            ScopeKind::Global => self.dirty_global.set(true),
            ScopeKind::Buffer => self.dirty_buffer.set(true),
            ScopeKind::Window => self.dirty_window.set(true),
            ScopeKind::Tab => self.dirty_tab.set(true),
            ScopeKind::Vim => self.dirty_vim.set(true),
            ScopeKind::Script | ScopeKind::Local | ScopeKind::Argument => {}
        }
    }

    /// Whether one synced map holds an unwritten scope-side change.
    #[must_use]
    pub fn is_dirty(&self, kind: ScopeKind) -> bool {
        match kind {
            ScopeKind::Global => self.dirty_global.get(),
            ScopeKind::Buffer => self.dirty_buffer.get(),
            ScopeKind::Window => self.dirty_window.get(),
            ScopeKind::Tab => self.dirty_tab.get(),
            ScopeKind::Vim => self.dirty_vim.get(),
            ScopeKind::Script | ScopeKind::Local | ScopeKind::Argument => false,
        }
    }

    /// Clear the dirty flag for one synced map after its write landed.
    pub fn clear_dirty(&self, kind: ScopeKind) {
        match kind {
            ScopeKind::Global => self.dirty_global.set(false),
            ScopeKind::Buffer => self.dirty_buffer.set(false),
            ScopeKind::Window => self.dirty_window.set(false),
            ScopeKind::Tab => self.dirty_tab.set(false),
            ScopeKind::Vim => self.dirty_vim.set(false),
            ScopeKind::Script | ScopeKind::Local | ScopeKind::Argument => {}
        }
    }
}

/// A `:lockvar` mark on one variable.
///
/// `do_lock_var` (`eval/vars.c:1802`) sets two things: `DI_FLAGS_LOCK` on the
/// dict item, and — only when `depth` is non-zero — `v_lock` on the
/// variable's own value through `tv_item_lock`. A List or Dict carries
/// `v_lock` in its own [`ox_types::LockState`]; a scalar has nowhere to put
/// it, so the flag is recorded here beside the name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockMark {
    /// The scope map holding the marked variable.
    pub scope: ScopeKind,
    /// The variable name inside that scope, without its prefix.
    pub name: OxStr,
    /// Whether `tv_item_lock` also locked the variable's own value.
    pub value: bool,
}

impl Scope {
    /// Create an empty scope set.
    #[must_use]
    pub fn new() -> Self {
        let mut scope = Self::default();
        scope.vim.extend([
            (OxStr::from("_null_string"), Typval::String(OxStr::from(""))),
            (OxStr::from("_null_list"), Typval::list(Vec::new())),
            (OxStr::from("_null_dict"), Typval::null_dict()),
            (OxStr::from("_null_blob"), Typval::Blob(Vec::new())),
            (
                OxStr::from("msgpack_types"),
                Typval::dict(
                    [
                        "nil", "boolean", "integer", "float", "string", "array", "map", "ext",
                    ]
                    .into_iter()
                    .map(|name| (OxStr::from(name), Typval::list(Vec::new())))
                    .collect(),
                ),
            ),
        ]);
        scope
    }

    /// Clone the full scope table into an independent snapshot.
    ///
    /// Closures capture their defining scope by value; this method is the
    /// explicit spelling of that clone.
    #[must_use]
    pub fn snapshot(&self) -> Self {
        self.clone()
    }

    /// Resolve an unqualified variable name: `l:`, then `a:`, then `g:`.
    ///
    /// # Errors
    ///
    /// Returns `E121: Undefined variable: {name}` when the name is not found
    /// in any of the local, argument, or global scopes.
    pub fn get(&self, name: &[u8], offset: usize) -> Result<&Typval> {
        if let Some((_, value)) = find_pair(&self.local, name) {
            return Ok(value);
        }
        if let Some((_, value)) = find_pair(&self.argument, name) {
            return Ok(value);
        }
        if let Some((_, value)) = find_pair(&self.global, name) {
            self.mark_aliased(ScopeKind::Global, value);
            return Ok(value);
        }
        Err(undefined(name, offset))
    }

    /// Resolve a scoped variable name (`g:foo`, `v:val`, `l:count`, ...).
    ///
    /// # Errors
    ///
    /// Returns `E121: Undefined variable: {prefix}{name}` when the name is
    /// not found in the requested scope.
    pub fn get_scoped(&self, kind: ScopeKind, name: &[u8], offset: usize) -> Result<&Typval> {
        let map = self.map(kind);
        if let Some((_, value)) = find_pair(map, name) {
            self.mark_aliased(kind, value);
            return Ok(value);
        }
        Err(undefined_scoped(kind, name, offset))
    }

    /// Record that a container was handed to the caller of a synced map.
    ///
    /// A `List` or `Dict` is an `Rc<RefCell<..>>`: the map entry and every
    /// clone of it name the *same* container, so handing one out lets the
    /// caller mutate it in place — `add(g:l, 1)`, `:let g:d.k = 1`,
    /// `:unlet g:d.k` — without ever assigning to the map. No dirty-mark at
    /// an assignment site can see that write, so the read that hands out the
    /// alias is what marks the map dirty.
    ///
    /// The mark is deliberately conservative: a read that turns out not to
    /// mutate costs one extra write-back, while a missed mark loses a script
    /// write. Every alias chain starts at one of these reads, so marking here
    /// covers containers reached indirectly too.
    fn mark_aliased(&self, kind: ScopeKind, value: &Typval) {
        if matches!(value, Typval::List(_) | Typval::Dict(_)) {
            self.synced.mark_dirty(kind);
        }
    }

    /// Return a namespace dictionary snapshot for a bare scope expression.
    ///
    /// Entries owned by a scope (upstream embeds them in `buf_T` and friends)
    /// materialize with their mutability metadata attached, so a snapshot
    /// like `let d = b:` still refuses to mutate `d.changedtick`.
    ///
    /// The snapshot shares every container value with the map, so a bare `g:`
    /// read hands out aliases just like [`Scope::get_scoped`] and marks the
    /// map dirty.
    #[must_use]
    pub fn scope_dict(&self, kind: ScopeKind) -> Typval {
        let snapshot = Typval::dict_with_entries(
            self.map(kind)
                .iter()
                .map(|(key, value)| scope_var_entry(kind, key, value))
                .collect(),
        );
        self.mark_aliased(kind, &snapshot);
        snapshot
    }

    /// Assign a value to an unqualified name, storing it in `l:`.
    ///
    /// If a `l:` entry with the same byte key exists, it is updated in place;
    /// otherwise a new entry is appended, preserving insertion order.
    ///
    /// # Errors
    /// `E741` or `E1122` when `:lockvar` locked the variable being replaced.
    pub fn set(&mut self, name: &[u8], value: Typval) -> Result<()> {
        self.check_assignable(name, 0)?;
        assign(&mut self.local, name, value);
        Ok(())
    }

    /// `set_var_const` (`eval/vars.c:2869-2877`): an existing variable refuses
    /// a new value when its value is locked, and then when the variable
    /// itself is locked, in upstream's order. A name that does not exist yet
    /// has no dict item to carry either flag, so it always assigns.
    ///
    /// This is the check every assignment to `{name}` owes, including the
    /// compound (`+=`), list-element, and `:for` target forms.
    ///
    /// # Errors
    /// `E741` when the value is locked, `E1122` when the variable is, and
    /// `E742` when the value is already borrowed.
    pub fn check_assignable(&self, name: &[u8], offset: usize) -> Result<()> {
        if self.resolve(name).is_none() {
            return Ok(());
        }
        self.check_value_lock(name, offset)?;
        self.check_variable_lock(name)
    }

    /// `:lockvar[!] [depth] {name}` — `ex_lockvar` (`eval/vars.c:1554`) with
    /// `do_lock_var` (`eval/vars.c:1802`): mark the variable itself locked
    /// (`DI_FLAGS_LOCK`), then, when `depth` is non-zero, lock its value
    /// `depth` levels down. `depth` is 2 by default and -1 for `:lockvar!`.
    ///
    /// An unknown name is silently ignored, because `do_lock_var` fails
    /// without a message when `find_var` finds nothing.
    ///
    /// # Errors
    /// `E742` when a container in the traversal is already borrowed.
    pub fn lockvar(&mut self, name: &[u8], depth: i32) -> Result<()> {
        self.set_variable_lock(name, depth, true)
    }

    /// `:unlockvar[!] [depth] {name}`, the same path with `lock` false.
    ///
    /// # Errors
    /// `E742` when a container in the traversal is already borrowed.
    pub fn unlockvar(&mut self, name: &[u8], depth: i32) -> Result<()> {
        self.set_variable_lock(name, depth, false)
    }

    fn set_variable_lock(&mut self, name: &[u8], depth: i32, lock: bool) -> Result<()> {
        let Some((kind, bare)) = self.resolve(name) else {
            return Ok(());
        };
        let Some((_, value)) = find_pair(self.map(kind), bare) else {
            return Ok(());
        };
        let value = value.clone();
        let position = self
            .locked
            .iter()
            .position(|mark| mark.scope == kind && mark.name.as_bytes() == bare);
        match (lock, position) {
            (true, None) => self.locked.push(LockMark {
                scope: kind,
                name: OxStr::from(bare),
                value: depth != 0,
            }),
            (true, Some(position)) => self.locked[position].value |= depth != 0,
            (false, Some(position)) => {
                self.locked.remove(position);
            }
            (false, None) => {}
        }
        crate::builtins::lock_value(&value, depth, lock)
    }

    /// `var_check_lock` (`eval/vars.c:2990`): reject an assignment to a
    /// variable that `:lockvar` marked. An unknown name has no mark.
    ///
    /// # Errors
    /// `E1122` when the variable itself is locked.
    pub fn check_variable_lock(&self, name: &[u8]) -> Result<()> {
        let Some((kind, bare)) = self.resolve(name) else {
            return Ok(());
        };
        if self.mark(kind, bare).is_some() {
            return Err(EvalError::new(
                "E1122",
                0,
                format!("Variable is locked: {}", lossy(name)),
            ));
        }
        Ok(())
    }

    /// `value_check_lock` (`eval/typval.c:4000`): reject a change to a value
    /// `:lockvar` locked. Names the variable, as `e_value_is_locked_str` does.
    ///
    /// A List or Dict carries that flag in its own `LockState`; a scalar has
    /// nowhere to put it, so the [`LockMark`] carries it instead.
    ///
    /// # Errors
    /// `E121` when the variable does not exist, `E741` when its value is
    /// locked, and `E742` when the value is already borrowed.
    pub fn check_value_lock(&self, name: &[u8], offset: usize) -> Result<()> {
        let Some((kind, bare)) = self.resolve(name) else {
            return Err(undefined(name, offset));
        };
        let value = self.get_scoped(kind, bare, offset)?;
        let locked = self.mark(kind, bare).is_some_and(|mark| mark.value)
            || matches!(crate::builtins::is_locked_value(value)?, Typval::Number(state) if state != 0);
        if locked {
            return Err(EvalError::new(
                "E741",
                0,
                format!("Value is locked: {}", lossy(name)),
            ));
        }
        Ok(())
    }

    /// `find_var` (`eval/vars.c:2634`): the scope map that holds `name` —
    /// the one its `x:` prefix names, or the first of `l:`, `a:`, `g:` that
    /// has it — together with the name inside that map.
    fn resolve<'a>(&self, name: &'a [u8]) -> Option<(ScopeKind, &'a [u8])> {
        if name.len() >= 2 && name[1] == b':' {
            let kind = ScopeKind::from_byte(name[0])?;
            let bare = &name[2..];
            return find_pair(self.map(kind), bare).map(|_| (kind, bare));
        }
        [ScopeKind::Local, ScopeKind::Argument, ScopeKind::Global]
            .into_iter()
            .find(|kind| find_pair(self.map(*kind), name).is_some())
            .map(|kind| (kind, name))
    }

    fn mark(&self, kind: ScopeKind, name: &[u8]) -> Option<&LockMark> {
        self.locked
            .iter()
            .find(|mark| mark.scope == kind && mark.name.as_bytes() == name)
    }

    /// `find_var` for `islocked()` (`funcs.c:3110`): the resolved variable's
    /// value together with whether its `:lockvar` mark (`DI_FLAGS_LOCK`) is
    /// set. Read-only and fixed entry flags stay invisible here, as upstream
    /// `tv_islocked` ignores them.
    #[must_use]
    pub fn resolve_for_lock(&self, name: &[u8]) -> Option<(&Typval, bool)> {
        let (kind, bare) = self.resolve(name)?;
        let value = find_pair(self.map(kind), bare)?.1;
        Some((value, self.mark(kind, bare).is_some()))
    }

    /// Return the lock state of an unqualified variable (0 through 3).
    ///
    /// # Errors
    /// `E121` when the variable does not exist.
    pub fn islocked(&self, name: &[u8], offset: usize) -> Result<i64> {
        match crate::builtins::is_locked_value(self.get(name, offset)?)? {
            Typval::Number(status) => Ok(status),
            _ => Ok(0),
        }
    }

    /// Whether a scoped or unqualified variable name is currently bound.
    #[must_use]
    pub fn contains_variable(&self, name: &[u8]) -> bool {
        if name.len() >= 2 && name[1] == b':' {
            return ScopeKind::from_byte(name[0])
                .is_some_and(|kind| find_pair(self.map(kind), &name[2..]).is_some());
        }
        self.get(name, 0).is_ok()
    }

    /// Whether an environment name is set in the live process environment.
    #[must_use]
    pub fn contains_env(&self, name: &[u8]) -> bool {
        std::env::var_os(env_os_string(name)).is_some()
    }

    /// Whether an option name is present in the requested scope.
    #[must_use]
    pub fn contains_option(&self, scope: OptionScope, name: &[u8]) -> bool {
        match scope {
            OptionScope::Global => find_pair(&self.options_global, name).is_some(),
            OptionScope::Local => find_pair(&self.options_local, name).is_some(),
            OptionScope::Effective => {
                find_pair(&self.options_local, name).is_some()
                    || find_pair(&self.options_global, name).is_some()
            }
        }
    }

    /// Assign a value to a scoped name.
    ///
    /// `v:` and `a:` are read-only for normal assignment and produce `E46`;
    /// upstream checks that before either lock (`eval/vars.c:2869-2877`).
    ///
    /// # Errors
    /// `E46` for a read-only namespace, `E741` or `E1122` when `:lockvar`
    /// locked the variable being replaced.
    pub fn set_scoped(
        &mut self,
        kind: ScopeKind,
        name: &[u8],
        offset: usize,
        value: Typval,
    ) -> Result<()> {
        if matches!(kind, ScopeKind::Vim | ScopeKind::Argument) {
            return Err(EvalError::new(
                "E46",
                offset,
                format!("Cannot change read-only variable \"{}\"", lossy(name)),
            ));
        }
        if find_pair(self.map(kind), name).is_some() {
            // The lock checks and their messages name the variable as it was
            // written, which for a scoped target is `g:x`, not `x`.
            let mut written = Vec::with_capacity(kind.as_str().len() + name.len());
            written.extend_from_slice(kind.as_str().as_bytes());
            written.extend_from_slice(name);
            self.check_assignable(&written, offset)?;
        }
        assign(self.map_mut(kind), name, value);
        self.synced.mark_dirty(kind);
        Ok(())
    }

    /// Read an environment variable (`$VAR`) from the live process
    /// environment, so a value set this session (`setenv()`, `let $VAR`,
    /// `:language`) is visible to a `Scope` created earlier.
    ///
    /// Missing environment variables return an empty string, mirroring Vim's
    /// behavior that `$UNSET` evaluates to `""`.
    #[must_use]
    pub fn get_env(&self, name: &[u8]) -> Typval {
        std::env::var_os(env_os_string(name)).map_or_else(
            || Typval::String(OxStr(Vec::new())),
            |value| Typval::String(OxStr(Vec::from(value.as_encoded_bytes()))),
        )
    }

    /// Read a register (`@r`).
    ///
    /// Missing registers return an empty string.
    #[must_use]
    pub fn get_register(&self, name: &[u8]) -> Typval {
        find_pair(&self.registers, name).map_or_else(
            || Typval::String(OxStr(Vec::new())),
            |(_, value)| value.clone(),
        )
    }

    /// Set or create a register (`@r = ...`).
    pub fn set_register(&mut self, name: &[u8], value: Typval) {
        assign(&mut self.registers, name, value);
    }

    /// Read an option value (`&`, `&g:`, or `&l:`).
    ///
    /// `OptionScope::Effective` returns the local value if it exists and the
    /// global value otherwise. Missing options default to `0`, because this
    /// module has no editor knowledge of real option defaults.
    #[must_use]
    pub fn get_option(&self, scope: OptionScope, name: &[u8]) -> Typval {
        let value = match scope {
            OptionScope::Global => find_pair(&self.options_global, name),
            OptionScope::Local => find_pair(&self.options_local, name),
            OptionScope::Effective => find_pair(&self.options_local, name)
                .or_else(|| find_pair(&self.options_global, name)),
        };
        value.map_or(Typval::Number(0), |(_, value)| value.clone())
    }

    /// Set an option value (`&`, `&g:`, or `&l:`).
    ///
    /// `OptionScope::Effective` stores into the local option map, matching the
    /// common meaning of an unqualified `&opt` assignment.
    pub fn set_option(&mut self, scope: OptionScope, name: &[u8], value: Typval) {
        match scope {
            OptionScope::Global => assign(&mut self.options_global, name, value),
            OptionScope::Local | OptionScope::Effective => {
                assign(&mut self.options_local, name, value);
            }
        }
    }

    fn map(&self, kind: ScopeKind) -> &ScopeMap {
        match kind {
            ScopeKind::Global => &self.global,
            ScopeKind::Buffer => &self.buffer,
            ScopeKind::Window => &self.window,
            ScopeKind::Tab => &self.tab,
            ScopeKind::Script => &self.script,
            ScopeKind::Local => &self.local,
            ScopeKind::Argument => &self.argument,
            ScopeKind::Vim => &self.vim,
        }
    }

    fn map_mut(&mut self, kind: ScopeKind) -> &mut ScopeMap {
        match kind {
            ScopeKind::Global => &mut self.global,
            ScopeKind::Buffer => &mut self.buffer,
            ScopeKind::Window => &mut self.window,
            ScopeKind::Tab => &mut self.tab,
            ScopeKind::Script => &mut self.script,
            ScopeKind::Local => &mut self.local,
            ScopeKind::Argument => &mut self.argument,
            ScopeKind::Vim => &mut self.vim,
        }
    }

    /// Replace one entry in a scope map, marking the map dirty.
    ///
    /// This is the only sanctioned way to rewrite a synced map outside
    /// [`Scope::set_scoped`]: the dirty-mark lives here so a call site cannot
    /// forget it, and a missed mark would lose the write-back to the editor.
    /// Returns the previous value, which [`Scope::restore_pair`] puts back.
    pub fn replace_pair(&mut self, kind: ScopeKind, name: &str, value: Typval) -> Option<Typval> {
        let map = self.map_mut(kind);
        let previous = map
            .iter()
            .find(|(key, _)| key.as_bytes() == name.as_bytes())
            .map(|(_, value)| value.clone());
        map.retain(|(key, _)| key.as_bytes() != name.as_bytes());
        map.push((OxStr::from(name), value));
        self.synced.mark_dirty(kind);
        previous
    }

    /// Put back what [`Scope::replace_pair`] returned, marking the map dirty.
    ///
    /// A nested host sync inside the region that held the temporary value
    /// already wrote it to the editor, so restoring is a change the editor
    /// must receive too.
    pub fn restore_pair(&mut self, kind: ScopeKind, name: &str, previous: Option<Typval>) {
        let map = self.map_mut(kind);
        map.retain(|(key, _)| key.as_bytes() != name.as_bytes());
        if let Some(value) = previous {
            map.push((OxStr::from(name), value));
        }
        self.synced.mark_dirty(kind);
    }

    /// Remove one entry from a scope map, marking the map dirty.
    ///
    /// `:unlet` and every other removal route through here so the write-back
    /// cannot be skipped. Returns whether an entry was removed.
    pub fn remove_pair(&mut self, kind: ScopeKind, name: &[u8]) -> bool {
        let map = self.map_mut(kind);
        let before = map.len();
        map.retain(|(key, _)| key.as_bytes() != name);
        if before == map.len() {
            false
        } else {
            self.synced.mark_dirty(kind);
            true
        }
    }
}

fn find_pair<'a>(map: &'a ScopeMap, key: &[u8]) -> Option<(&'a OxStr, &'a Typval)> {
    for (k, v) in map {
        if k.as_bytes() == key {
            return Some((k, v));
        }
    }
    None
}

fn find_index(map: &ScopeMap, key: &[u8]) -> Option<usize> {
    for (i, (k, _)) in map.iter().enumerate() {
        if k.as_bytes() == key {
            return Some(i);
        }
    }
    None
}

fn assign(map: &mut ScopeMap, key: &[u8], value: Typval) {
    if let Some(i) = find_index(map, key) {
        map[i].1 = value;
    } else {
        map.push((OxStr::from(key), value));
    }
}

/// The one table that names scope-owned read-only items.
///
/// Upstream equivalent: `buf_init_changedtick` (`buffer.c:1941`) marks
/// `b:changedtick` with `DI_FLAGS_RO|DI_FLAGS_FIX` and `v_lock = VAR_FIXED`;
/// the item never carries `DI_FLAGS_LOCK`, so `islocked()` stays 0.
#[must_use]
pub fn scope_entry_flags(kind: ScopeKind, key: &[u8]) -> Option<(DictEntryFlags, EntryValueLock)> {
    match (kind, key) {
        (ScopeKind::Buffer, b"changedtick") | (ScopeKind::Vim, b"msgpack_types") => Some((
            DictEntryFlags::READ_ONLY | DictEntryFlags::FIXED,
            EntryValueLock::Fixed,
        )),
        _ => None,
    }
}

/// Build one dictionary entry for a scope-table value, marking entries the
/// scope itself owns (`scope_entry_flags`).
#[must_use]
pub fn scope_var_entry(kind: ScopeKind, key: &OxStr, value: &Typval) -> DictEntry {
    match scope_entry_flags(kind, key.as_bytes()) {
        Some((flags, value_lock)) => {
            DictEntry::marked(key.clone(), value.clone(), flags, value_lock)
        }
        None => DictEntry::new(key.clone(), value.clone()),
    }
}

/// Builds an operating-system string from raw Vimscript bytes.
///
/// Unix carries the bytes exactly; other platforms fall back to lossy UTF-8,
/// which is all their `OsStr` can represent.
pub(crate) fn env_os_string(bytes: &[u8]) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStringExt::from_vec(bytes.to_vec())
    }
    #[cfg(not(unix))]
    {
        std::ffi::OsString::from(lossy(bytes))
    }
}

fn undefined(name: &[u8], offset: usize) -> EvalError {
    EvalError::new(
        "E121",
        offset,
        format!("Undefined variable: {}", lossy(name)),
    )
}

fn undefined_scoped(kind: ScopeKind, name: &[u8], offset: usize) -> EvalError {
    EvalError::new(
        "E121",
        offset,
        format!("Undefined variable: {}{}", kind.as_str(), lossy(name)),
    )
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
