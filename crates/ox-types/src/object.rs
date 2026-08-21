//! The `Object` wire type used by the msgpack-RPC layer.
//!
//! Mirrors upstream `Object` in `api/private/defs.h`, whose `ObjectType`
//! discriminates `kObjectTypeNil` through `kObjectTypeTabpage`.

use crate::byte_str::OxStr;
use crate::handle::{BufHandle, TabHandle, WinHandle};

/// msgpack EXT code for a buffer handle.
///
/// `mpack_handle()` in `msgpack_rpc/packer.c` packs a handle as EXT with
/// `type = ObjectType - EXT_OBJECT_TYPE_SHIFT`, so Buffer maps to `0`.
pub const EXT_TYPE_BUFFER: i8 = 0;
/// msgpack EXT code for a window handle (`kObjectTypeWindow`).
pub const EXT_TYPE_WINDOW: i8 = 1;
/// msgpack EXT code for a tabpage handle (`kObjectTypeTabpage`).
pub const EXT_TYPE_TABPAGE: i8 = 2;

/// A value crossing the API boundary (RPC, `vim.api`).
///
/// Variants mirror the upstream `kObjectType*` set, minus `kObjectTypeUnset`
/// which is internal-only and never crosses the boundary (it becomes `Nil`).
#[derive(Clone, Debug, PartialEq)]
pub enum Object {
    /// The msgpack NIL (`kObjectTypeNil`).
    Nil,
    /// A boolean (`kObjectTypeBoolean`).
    Boolean(bool),
    /// A signed 64-bit integer (`kObjectTypeInteger`).
    Integer(i64),
    /// An IEEE-754 double (`kObjectTypeFloat`).
    Float(f64),
    /// A (possibly non-UTF-8) byte string (`kObjectTypeString`).
    String(OxStr),
    /// An ordered array (`kObjectTypeArray`).
    Array(Vec<Object>),
    /// An insertion-ordered key/value map (`kObjectTypeDict`).
    Dict(Dict),
    /// A reference into the Lua registry (`kObjectTypeLuaRef`).
    LuaRef(i32),
    /// A buffer handle (`kObjectTypeBuffer`, msgpack EXT 0).
    Buffer(BufHandle),
    /// A window handle (`kObjectTypeWindow`, msgpack EXT 1).
    Window(WinHandle),
    /// A tabpage handle (`kObjectTypeTabpage`, msgpack EXT 2).
    Tabpage(TabHandle),
}

/// An insertion-ordered dictionary of [`Object`] values.
///
/// Upstream dicts are hash tables, but both the API-metadata schema and RPC
/// consumers observe a stable order, so a `Vec`-backed map is chosen (per the
/// project plan).
#[derive(Clone, Debug, PartialEq)]
pub struct Dict(pub Vec<(OxStr, Object)>);

impl Dict {
    /// Look up a value by key.
    #[must_use]
    pub fn get(&self, key: &OxStr) -> Option<&Object> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Insert `value` under `key`.
    ///
    /// If the key already exists its value is replaced **in place**, preserving
    /// the original position in the insertion order; otherwise the pair is
    /// appended.
    pub fn insert(&mut self, key: OxStr, value: Object) {
        if let Some((_, existing)) = self.0.iter_mut().find(|(k, _)| *k == key) {
            *existing = value;
        } else {
            self.0.push((key, value));
        }
    }

    /// Iterate over `(key, value)` pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &(OxStr, Object)> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::Dict;
    use super::Object;
    use crate::byte_str::OxStr;
    use crate::handle::BufHandle;

    #[test]
    fn dict_insert_replaces_in_place() {
        let mut d = Dict(vec![]);
        let a = OxStr::from("a");
        let b = OxStr::from("b");
        d.insert(a.clone(), Object::Integer(1));
        d.insert(b.clone(), Object::Integer(2));
        d.insert(a.clone(), Object::Integer(99));

        // "a" stays at its original first position with the new value.
        assert_eq!(d.0.len(), 2);
        assert_eq!(d.get(&a), Some(&Object::Integer(99)));
        assert_eq!(d.get(&b), Some(&Object::Integer(2)));
        assert_eq!(d.0[0].0, a);
        assert_eq!(d.0[0].1, Object::Integer(99));
        assert_eq!(d.0[1].0, b);
    }

    #[test]
    fn dict_get_missing_returns_none() {
        let d = Dict(vec![]);
        assert_eq!(d.get(&OxStr::from("nope")), None);
        assert!(d.0.is_empty());
    }

    #[test]
    fn dict_iter_preserves_order() {
        let d = Dict(vec![
            (OxStr::from("x"), Object::Nil),
            (OxStr::from("y"), Object::Integer(1)),
        ]);
        let keys: Vec<&OxStr> = d.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, [&OxStr::from("x"), &OxStr::from("y")]);
    }

    #[test]
    fn object_ext_constants() {
        assert_eq!(super::EXT_TYPE_BUFFER, 0);
        assert_eq!(super::EXT_TYPE_WINDOW, 1);
        assert_eq!(super::EXT_TYPE_TABPAGE, 2);
        // The handle payloads carry the extensible kinds.
        let obj = Object::Buffer(BufHandle::try_from(5).unwrap());
        assert_eq!(obj, Object::Buffer(BufHandle::try_from(5).unwrap()));
    }
}
