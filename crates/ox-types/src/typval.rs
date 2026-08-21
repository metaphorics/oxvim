//! The Vimscript value model (`typval_T`), for `ox-eval`.
//!
//! Mirrors upstream `typval_defs.h` `VarType` kinds. Vimscript strings are
//! byte strings ([`OxStr`]), lists/dicts are value collections here (ox-eval
//! owns the reference/identity machinery).

use crate::byte_str::OxStr;

/// `VAR_UNKNOWN` — unspecified value.
pub const VAR_UNKNOWN: u8 = 0;
/// `VAR_NUMBER` — a signed 64-bit integer.
pub const VAR_NUMBER: u8 = 1;
/// `VAR_STRING` — a (byte) string.
pub const VAR_STRING: u8 = 2;
/// `VAR_FUNC` — a named function reference.
pub const VAR_FUNC: u8 = 3;
/// `VAR_LIST` — a list.
pub const VAR_LIST: u8 = 4;
/// `VAR_DICT` — a dictionary.
pub const VAR_DICT: u8 = 5;
/// `VAR_FLOAT` — a floating-point number.
pub const VAR_FLOAT: u8 = 6;
/// `VAR_BOOL` — `true`/`false`.
pub const VAR_BOOL: u8 = 7;
/// `VAR_SPECIAL` — `v:null` (and `v:none`).
pub const VAR_SPECIAL: u8 = 8;
/// `VAR_PARTIAL` — a closure (funcref with bound args/dict).
pub const VAR_PARTIAL: u8 = 9;
/// `VAR_BLOB` — a byte blob.
pub const VAR_BLOB: u8 = 10;

/// Special Vimscript values, mirroring `SpecialVarValue`
/// (`typval_defs.h:96-98`).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Special {
    /// `v:null` — upstream `kSpecialVarNull`. `v:none` shares the same special
    /// value in Vim, so `v:none`/`v:null` both map here.
    Null,
}

/// A function reference — the payload of [`Typval::Funcref`] (a plain named
/// function, `VAR_FUNC`) and [`Typval::Partial`] (a closure, `VAR_PARTIAL`).
///
/// This carries what upstream `partial_T` holds: a name plus optional bound
/// arguments and `self` dict (`pt_argv`/`pt_dict`, `typval_defs.h:369-372`).
#[derive(Debug, Clone, PartialEq)]
pub struct Funcref {
    /// The function name.
    pub name: OxStr,
    /// Partially-bound arguments.
    pub args: Vec<Typval>,
    /// The `self` dict, if bound.
    pub dict: Option<Vec<(OxStr, Typval)>>,
}

/// A Vimscript value (`typval_T`).
///
/// `Channel`/`Job` are convenience wrappers for channel/job ids — upstream
/// stores those as plain numbers, so their `vartype()` is [`VAR_NUMBER`].
#[derive(Debug, Clone, PartialEq)]
pub enum Typval {
    /// `VAR_NUMBER`.
    Number(i64),
    /// `VAR_FLOAT`.
    Float(f64),
    /// `VAR_STRING`.
    String(OxStr),
    /// `VAR_BLOB`.
    Blob(Vec<u8>),
    /// `VAR_LIST`.
    List(Vec<Typval>),
    /// `VAR_DICT` (ordered, same rationale as the API [`super::object::Dict`]).
    Dict(Vec<(OxStr, Typval)>),
    /// `VAR_FUNC` — a named function reference.
    Funcref(Funcref),
    /// `VAR_PARTIAL` — a closure with bound args/self-dict.
    Partial(Funcref),
    /// `VAR_BOOL`.
    Bool(bool),
    /// `VAR_SPECIAL`.
    Special(Special),
    /// A channel id (`v:channel`), stored as a number upstream.
    Channel(u64),
    /// A job id, stored as a number upstream.
    Job(u64),
}

impl Typval {
    /// The numeric `VAR_*` constant for this value, matching upstream
    /// `VarType` (`typval_defs.h:108-120`).
    #[must_use]
    pub const fn vartype(&self) -> u8 {
        match self {
            Typval::Number(_) | Typval::Channel(_) | Typval::Job(_) => VAR_NUMBER,
            Typval::Float(_) => VAR_FLOAT,
            Typval::String(_) => VAR_STRING,
            Typval::Blob(_) => VAR_BLOB,
            Typval::List(_) => VAR_LIST,
            Typval::Dict(_) => VAR_DICT,
            Typval::Funcref(_) => VAR_FUNC,
            Typval::Partial(_) => VAR_PARTIAL,
            Typval::Bool(_) => VAR_BOOL,
            Typval::Special(_) => VAR_SPECIAL,
        }
    }

    /// Vimscript truthiness, per upstream `tv2bool()` (`eval/typval.c:4778-4817`):
    /// a value is falsy iff zero, an empty string/blob/list/dict, `false`, or
    /// `v:null`.
    #[must_use]
    pub fn is_truthy(&self) -> bool {
        match self {
            Typval::Number(n) => *n != 0,
            Typval::Float(f) => *f != 0.0,
            // tv2bool VAR_STRING: `*v_string != NUL` — "0" is non-empty, so
            // truthy.
            Typval::String(s) => !s.as_bytes().is_empty(),
            Typval::Blob(b) => !b.is_empty(),
            Typval::List(l) => !l.is_empty(),
            Typval::Dict(d) => !d.is_empty(),
            // tv2bool VAR_FUNC: the name string is non-empty.
            Typval::Funcref(f) => !f.name.as_bytes().is_empty(),
            // tv2bool VAR_PARTIAL: `v_partial != NULL` — a present partial is
            // always truthy.
            Typval::Partial(_) => true,
            Typval::Bool(b) => *b,
            // tv2bool VAR_SPECIAL: `v_special != kSpecialVarNull`.
            Typval::Special(Special::Null) => false,
            // Channels/jobs are numbers upstream; truthiness follows the id.
            Typval::Channel(id) | Typval::Job(id) => *id != 0,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{Funcref, Special, Typval};
    use crate::byte_str::OxStr;

    #[test]
    fn vartype_constants_match_upstream() {
        assert_eq!(Typval::Number(0).vartype(), super::VAR_NUMBER);
        assert_eq!(Typval::Float(1.5).vartype(), super::VAR_FLOAT);
        assert_eq!(Typval::String(OxStr::from("")).vartype(), super::VAR_STRING);
        assert_eq!(Typval::Blob(vec![1]).vartype(), super::VAR_BLOB);

        let f = Funcref { name: OxStr::from("g:fn"), args: vec![], dict: None };
        assert_eq!(Typval::Funcref(f.clone()).vartype(), super::VAR_FUNC);
        assert_eq!(Typval::Partial(f).vartype(), super::VAR_PARTIAL);
        assert_eq!(Typval::List(vec![]).vartype(), super::VAR_LIST);
        assert_eq!(Typval::Dict(vec![]).vartype(), super::VAR_DICT);
        assert_eq!(Typval::Bool(true).vartype(), super::VAR_BOOL);
        assert_eq!(Typval::Special(Special::Null).vartype(), super::VAR_SPECIAL);
        // Channels/jobs are numbers upstream.
        assert_eq!(Typval::Channel(3).vartype(), super::VAR_NUMBER);
        assert_eq!(Typval::Job(3).vartype(), super::VAR_NUMBER);
        assert_eq!(super::VAR_UNKNOWN, 0);
        assert_eq!(super::VAR_BLOB, 10);
    }

    // Truthiness verified against tv2bool() semantics (eval/typval.c:4778).
    #[test]
    fn truthiness_falsy() {
        assert!(!Typval::Number(0).is_truthy());
        assert!(!Typval::Float(0.0).is_truthy());
        // "0" is a non-empty string → truthy per tv2bool VAR_STRING.
        assert!(Typval::String(OxStr::from("0")).is_truthy());
        assert!(!Typval::String(OxStr::from("")).is_truthy());
        assert!(!Typval::Blob(vec![]).is_truthy());
        assert!(!Typval::List(vec![]).is_truthy());
        assert!(!Typval::Dict(vec![]).is_truthy());
        assert!(!Typval::Bool(false).is_truthy());
        assert!(!Typval::Special(Special::Null).is_truthy());
        assert!(!Typval::Channel(0).is_truthy());
    }

    #[test]
    fn truthiness_truthy() {
        assert!(Typval::Number(1).is_truthy());
        assert!(Typval::Number(-3).is_truthy());
        assert!(Typval::Float(0.1).is_truthy());
        assert!(Typval::String(OxStr::from("x")).is_truthy());
        assert!(Typval::Blob(vec![0]).is_truthy());
        // [0] is a non-empty list → truthy.
        assert!(Typval::List(vec![Typval::Number(0)]).is_truthy());
        assert!(Typval::Dict(vec![(OxStr::from("k"), Typval::Number(0))]).is_truthy());
        assert!(Typval::Bool(true).is_truthy());
        let f = Funcref { name: OxStr::from("fn"), args: vec![], dict: None };
        assert!(Typval::Funcref(f.clone()).is_truthy());
        assert!(Typval::Partial(f).is_truthy());
        // A funcref with an empty name is VAR_FUNC with an empty string:
        // falsy like an empty string.
        let empty = Funcref { name: OxStr::from(""), args: vec![], dict: None };
        assert!(!Typval::Funcref(empty).is_truthy());
    }
}