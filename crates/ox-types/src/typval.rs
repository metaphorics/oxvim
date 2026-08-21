//! The Vimscript value model (`typval_T`), for `ox-eval`.
//!
//! Mirrors upstream `typval_defs.h` `VarType` kinds. Vimscript strings are
//! byte strings ([`OxStr`]); lists and dictionaries are shared reference types.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::rc::Rc;

use crate::byte_str::OxStr;

/// Unspecified value tag.
pub const VAR_UNKNOWN: u8 = 0;
/// Number value tag.
pub const VAR_NUMBER: u8 = 1;
/// String value tag.
pub const VAR_STRING: u8 = 2;
/// Funcref value tag.
pub const VAR_FUNC: u8 = 3;
/// List value tag.
pub const VAR_LIST: u8 = 4;
/// Dictionary value tag.
pub const VAR_DICT: u8 = 5;
/// Float value tag.
pub const VAR_FLOAT: u8 = 6;
/// Boolean value tag.
pub const VAR_BOOL: u8 = 7;
/// Special value tag.
pub const VAR_SPECIAL: u8 = 8;
/// Partial value tag.
pub const VAR_PARTIAL: u8 = 9;
/// Blob value tag.
pub const VAR_BLOB: u8 = 10;

/// Vimscript special values.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Special {
    /// `v:null` (and `v:none`).
    Null,
}

/// The depth at which a container was locked.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Hash)]
pub enum LockScope {
    /// No container lock.
    #[default]
    None,
    /// Lock this container only.
    Shallow,
    /// Lock every reachable container.
    Deep,
}

/// Mutable lock metadata stored with every reference container.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Hash)]
pub struct LockState {
    /// Applied lock depth.
    pub scope: LockScope,
    /// Whether this container rejects mutation.
    pub locked: bool,
}

/// Mutable payload of a shared List reference.
#[derive(Clone)]
pub struct ListData {
    /// Ordered List items.
    pub items: Vec<Typval>,
    /// Container lock metadata.
    pub lock: LockState,
}

/// Mutable payload of a shared Dictionary reference.
#[derive(Clone)]
pub struct DictData {
    /// Ordered Dictionary entries.
    pub entries: Vec<(OxStr, Typval)>,
    /// Container lock metadata.
    pub lock: LockState,
}

/// Shared List identity.
pub type ListRef = Rc<RefCell<ListData>>;
/// Shared Dictionary identity.
pub type DictRef = Rc<RefCell<DictData>>;

/// A function reference — plain named function or registered closure.
/// Named function or registered closure reference.
#[derive(Clone, PartialEq)]
pub struct Funcref {
    /// Function name.
    pub name: OxStr,
    /// Bound arguments.
    pub args: Vec<Typval>,
    /// Optional bound self dictionary.
    pub dict: Option<Vec<(OxStr, Typval)>>,
    /// Closure registry identity, for lambdas.
    pub registry: Option<usize>,
}

/// A Vimscript value (`typval_T`).
/// A Vimscript runtime value.
#[derive(Clone)]
pub enum Typval {
    /// Signed integer.
    Number(i64),
    /// Floating-point number.
    Float(f64),
    /// Byte string.
    String(OxStr),
    /// Byte blob.
    Blob(Vec<u8>),
    /// Shared List.
    List(ListRef),
    /// Shared ordered Dictionary.
    Dict(DictRef),
    /// Named function reference.
    Funcref(Funcref),
    /// Closure or partially-bound function.
    Partial(Funcref),
    /// Boolean.
    Bool(bool),
    /// Special value.
    Special(Special),
    /// Channel identifier, represented as a Number upstream.
    Channel(u64),
    /// Job identifier, represented as a Number upstream.
    Job(u64),
}

impl Typval {
    /// Construct a new unlocked List identity.
    #[must_use]
    pub fn list(items: Vec<Self>) -> Self {
        Self::List(Rc::new(RefCell::new(ListData { items, lock: LockState::default() })))
    }

    /// Construct a new unlocked ordered Dictionary identity.
    #[must_use]
    pub fn dict(entries: Vec<(OxStr, Self)>) -> Self {
        Self::Dict(Rc::new(RefCell::new(DictData { entries, lock: LockState::default() })))
    }

    /// Return the upstream numeric `VAR_*` tag.
    #[must_use]
    pub const fn vartype(&self) -> u8 {
        match self {
            Self::Number(_) | Self::Channel(_) | Self::Job(_) => VAR_NUMBER,
            Self::Float(_) => VAR_FLOAT,
            Self::String(_) => VAR_STRING,
            Self::Blob(_) => VAR_BLOB,
            Self::List(_) => VAR_LIST,
            Self::Dict(_) => VAR_DICT,
            Self::Funcref(_) => VAR_FUNC,
            Self::Partial(_) => VAR_PARTIAL,
            Self::Bool(_) => VAR_BOOL,
            Self::Special(_) => VAR_SPECIAL,
        }
    }

    /// Vimscript `tv2bool()` truthiness. A conflicting mutable borrow is never
    /// observable during well-formed evaluation; treating it as empty keeps
    /// this infallible query from panicking during host re-entry.
    #[must_use]
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Number(n) => *n != 0,
            Self::Float(f) => *f != 0.0,
            Self::String(s) => !s.as_bytes().is_empty(),
            Self::Blob(b) => !b.is_empty(),
            Self::List(list) => list.try_borrow().is_ok_and(|data| !data.items.is_empty()),
            Self::Dict(dict) => dict.try_borrow().is_ok_and(|data| !data.entries.is_empty()),
            Self::Funcref(funcref) => !funcref.name.as_bytes().is_empty(),
            Self::Partial(_) => true,
            Self::Bool(value) => *value,
            Self::Special(Special::Null) => false,
            Self::Channel(id) | Self::Job(id) => *id != 0,
        }
    }
}

impl PartialEq for Typval {
    fn eq(&self, other: &Self) -> bool {
        fn equal(left: &Typval, right: &Typval, seen: &mut HashSet<(usize, usize, u8)>) -> bool {
            match (left, right) {
                (Typval::Number(a), Typval::Number(b)) => a == b,
                (Typval::Float(a), Typval::Float(b)) => a == b,
                (Typval::String(a), Typval::String(b)) => a == b,
                (Typval::Blob(a), Typval::Blob(b)) => a == b,
                (Typval::Bool(a), Typval::Bool(b)) => a == b,
                (Typval::Special(a), Typval::Special(b)) => a == b,
                (Typval::Channel(a), Typval::Channel(b)) | (Typval::Job(a), Typval::Job(b)) => a == b,
                (Typval::Funcref(a), Typval::Funcref(b)) | (Typval::Partial(a), Typval::Partial(b)) => a == b,
                (Typval::List(a), Typval::List(b)) => {
                    let pair = (Rc::as_ptr(a) as usize, Rc::as_ptr(b) as usize, VAR_LIST);
                    if !seen.insert(pair) {
                        return true;
                    }
                    let (Ok(a), Ok(b)) = (a.try_borrow(), b.try_borrow()) else {
                        return false;
                    };
                    let left = a.items.clone();
                    let right = b.items.clone();
                    drop(a);
                    drop(b);
                    left.len() == right.len()
                        && left.iter().zip(&right).all(|(a, b)| equal(a, b, seen))
                }
                (Typval::Dict(a), Typval::Dict(b)) => {
                    let pair = (Rc::as_ptr(a) as usize, Rc::as_ptr(b) as usize, VAR_DICT);
                    if !seen.insert(pair) {
                        return true;
                    }
                    let (Ok(a), Ok(b)) = (a.try_borrow(), b.try_borrow()) else {
                        return false;
                    };
                    let left = a.entries.clone();
                    let right = b.entries.clone();
                    drop(a);
                    drop(b);
                    left.len() == right.len()
                        && left.iter().all(|(key, value)| {
                            right.iter().find(|(candidate, _)| candidate == key)
                                .is_some_and(|(_, other)| equal(value, other, seen))
                        })
                }
                _ => false,
            }
        }

        equal(self, other, &mut HashSet::new())
    }
}

impl fmt::Debug for Funcref {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Funcref")
            .field("name", &self.name)
            .field("args", &self.args)
            .field("dict", &self.dict)
            .field("registry", &self.registry)
            .finish()
    }
}

impl fmt::Debug for Typval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn write_value(value: &Typval, f: &mut fmt::Formatter<'_>, active: &mut HashSet<(usize, u8)>) -> fmt::Result {
            match value {
                Typval::List(list) => {
                    let key = (Rc::as_ptr(list) as usize, VAR_LIST);
                    if !active.insert(key) {
                        return f.write_str("List([...])");
                    }
                    f.write_str("List([")?;
                    let items = list.try_borrow().map_err(|_| fmt::Error)?.items.clone();
                    for (index, item) in items.iter().enumerate() {
                        if index != 0 { f.write_str(", ")?; }
                        write_value(item, f, active)?;
                    }
                    active.remove(&key);
                    f.write_str("])")
                }
                Typval::Dict(dict) => {
                    let key = (Rc::as_ptr(dict) as usize, VAR_DICT);
                    if !active.insert(key) {
                        return f.write_str("Dict({...})");
                    }
                    f.write_str("Dict({")?;
                    let entries = dict.try_borrow().map_err(|_| fmt::Error)?.entries.clone();
                    for (index, (name, item)) in entries.iter().enumerate() {
                        if index != 0 { f.write_str(", ")?; }
                        write!(f, "{name:?}: ")?;
                        write_value(item, f, active)?;
                    }
                    active.remove(&key);
                    f.write_str("})")
                }
                Typval::Number(v) => f.debug_tuple("Number").field(v).finish(),
                Typval::Float(v) => f.debug_tuple("Float").field(v).finish(),
                Typval::String(v) => f.debug_tuple("String").field(v).finish(),
                Typval::Blob(v) => f.debug_tuple("Blob").field(v).finish(),
                Typval::Funcref(v) => f.debug_tuple("Funcref").field(v).finish(),
                Typval::Partial(v) => f.debug_tuple("Partial").field(v).finish(),
                Typval::Bool(v) => f.debug_tuple("Bool").field(v).finish(),
                Typval::Special(v) => f.debug_tuple("Special").field(v).finish(),
                Typval::Channel(v) => f.debug_tuple("Channel").field(v).finish(),
                Typval::Job(v) => f.debug_tuple("Job").field(v).finish(),
            }
        }

        write_value(self, formatter, &mut HashSet::new())
    }
}

#[cfg(test)]
mod tests {
    use super::{Funcref, LockScope, Special, Typval};
    use crate::byte_str::OxStr;

    #[test]
    fn vartype_and_truthiness_follow_upstream() {
        assert_eq!(Typval::list(vec![]).vartype(), super::VAR_LIST);
        assert_eq!(Typval::dict(vec![]).vartype(), super::VAR_DICT);
        assert!(!Typval::list(vec![]).is_truthy());
        assert!(Typval::list(vec![Typval::Number(0)]).is_truthy());
        assert!(!Typval::String(OxStr::from("")).is_truthy());
        assert!(Typval::String(OxStr::from("0")).is_truthy());
        assert_eq!(super::VAR_UNKNOWN, 0);
        assert_eq!(super::VAR_BLOB, 10);
    }

    #[test]
    fn clone_shares_container_and_equality_handles_cycles() {
        let list = Typval::list(vec![]);
        let Typval::List(reference) = &list else { return };
        reference.borrow_mut().items.push(list.clone());
        let alias = list.clone();
        assert_eq!(list, alias);
        assert_eq!(format!("{list:?}"), "List([List([...])])");
    }

    #[test]
    fn lock_state_defaults_unlocked() {
        let Typval::List(list) = Typval::list(vec![]) else { return };
        let data = list.borrow();
        assert!(!data.lock.locked);
        assert_eq!(data.lock.scope, LockScope::None);
    }

    #[test]
    fn funcref_types_remain_distinct() {
        let function = Funcref { name: OxStr::from("g:fn"), args: vec![], dict: None, registry: None };
        assert_eq!(Typval::Funcref(function.clone()).vartype(), super::VAR_FUNC);
        assert_eq!(Typval::Partial(function).vartype(), super::VAR_PARTIAL);
        assert_eq!(Typval::Special(Special::Null).vartype(), super::VAR_SPECIAL);
    }
}
