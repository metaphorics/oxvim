//! Variable scope model for the Vimscript expression evaluator.
//!
//! Each scope namespace (`g:`, `b:`, `w:`, `t:`, `s:`, `l:`, `a:`, `v:`) is
//! stored as an ordered `Vec<(OxStr, Typval)>`. Vimscript dictionaries and
//! scope tables are insertion-ordered and compare keys as raw bytes; no
//! UTF-8 decoding is assumed.
//!
//! Unqualified variable lookups follow Vim's internal-variable resolution
//! order: `l:`, then `a:`, then `g:`. The `v:` and `a:` namespaces are
//! read-only for normal assignment; writing to `v:` produces `E46`. Missing
//! variables produce `E121`.

use crate::error::{EvalError, Result};
use ox_types::{OxStr, Typval};

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

/// A full set of Vimscript scopes plus environment, option, and register maps.
///
/// `Scope` is cheap to clone: closures and partial applications capture the
/// entire table as a snapshot, which is the intended Vimscript semantics.
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
    /// `$VAR` environment variables.
    pub env: ScopeMap,
    /// `&g:` global option values.
    pub options_global: ScopeMap,
    /// `&l:` local option values.
    pub options_local: ScopeMap,
    /// `@r` register contents.
    pub registers: ScopeMap,
    /// Variables `:lockvar` marked, upstream's `DI_FLAGS_LOCK`.
    pub locked: Vec<LockMark>,
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
            (OxStr::from("_null_dict"), Typval::dict(Vec::new())),
            (OxStr::from("_null_blob"), Typval::Blob(Vec::new())),
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
    /// On failure, produces `E121: Undefined variable: {name}`.
    pub fn get(&self, name: &[u8], offset: usize) -> Result<&Typval> {
        if let Some((_, value)) = find_pair(&self.local, name) {
            return Ok(value);
        }
        if let Some((_, value)) = find_pair(&self.argument, name) {
            return Ok(value);
        }
        if let Some((_, value)) = find_pair(&self.global, name) {
            return Ok(value);
        }
        Err(undefined(name, offset))
    }

    /// Resolve a scoped variable name (`g:foo`, `v:val`, `l:count`, ...).
    ///
    /// On failure, produces `E121: Undefined variable: {prefix}{name}`.
    pub fn get_scoped(&self, kind: ScopeKind, name: &[u8], offset: usize) -> Result<&Typval> {
        let map = self.map(kind);
        if let Some((_, value)) = find_pair(map, name) {
            return Ok(value);
        }
        Err(undefined_scoped(kind, name, offset))
    }

    /// Return a namespace dictionary snapshot for a bare scope expression.
    #[must_use]
    pub fn scope_dict(&self, kind: ScopeKind) -> Typval {
        Typval::dict(self.map(kind).clone())
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
        let Some((kind, bare)) = self.resolve(name) else { return Ok(()) };
        let Some((_, value)) = find_pair(self.map(kind), bare) else { return Ok(()) };
        let value = value.clone();
        let position = self.locked.iter().position(|mark| mark.scope == kind && mark.name.as_bytes() == bare);
        match (lock, position) {
            (true, None) => self.locked.push(LockMark { scope: kind, name: OxStr::from(bare), value: depth != 0 }),
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
        let Some((kind, bare)) = self.resolve(name) else { return Ok(()) };
        if self.mark(kind, bare).is_some() {
            return Err(EvalError::new("E1122", 0, format!("Variable is locked: {}", lossy(name))));
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
        let Some((kind, bare)) = self.resolve(name) else { return Err(undefined(name, offset)) };
        let value = self.get_scoped(kind, bare, offset)?;
        let locked = self.mark(kind, bare).is_some_and(|mark| mark.value)
            || matches!(crate::builtins::is_locked_value(value)?, Typval::Number(state) if state != 0);
        if locked {
            return Err(EvalError::new("E741", 0, format!("Value is locked: {}", lossy(name))));
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
        self.locked.iter().find(|mark| mark.scope == kind && mark.name.as_bytes() == name)
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

    /// Whether an environment name is present in the evaluator overlay.
    #[must_use]
    pub fn contains_env(&self, name: &[u8]) -> bool {
        find_pair(&self.env, name).is_some()
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
        Ok(())
    }

    /// Read an environment variable (`$VAR`).
    ///
    /// Missing environment variables return an empty string, mirroring Vim's
    /// behavior that `$UNSET` evaluates to `""`.
    #[must_use]
    pub fn get_env(&self, name: &[u8]) -> Typval {
        find_pair(&self.env, name).map_or_else(|| Typval::String(OxStr(Vec::new())), |(_, value)| value.clone())
    }

    /// Set or create an environment variable (`$VAR = ...`).
    pub fn set_env(&mut self, name: &[u8], value: Typval) {
        assign(&mut self.env, name, value);
    }

    /// Read a register (`@r`).
    ///
    /// Missing registers return an empty string.
    #[must_use]
    pub fn get_register(&self, name: &[u8]) -> Typval {
        find_pair(&self.registers, name).map_or_else(|| Typval::String(OxStr(Vec::new())), |(_, value)| value.clone())
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
            OptionScope::Effective => find_pair(&self.options_local, name).or_else(|| find_pair(&self.options_global, name)),
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
                assign(&mut self.options_local, name, value)
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
