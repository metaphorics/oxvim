//! Metadata describing exported API functions.

use core::fmt;

/// The metadata type names exposed by `nvim_get_api_info`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypeRef {
    /// MessagePack nil.
    Nil,
    /// Boolean value.
    Boolean,
    /// Signed integer value.
    Integer,
    /// Floating-point value.
    Float,
    /// Byte string value.
    String,
    /// Untyped array value.
    Array,
    /// Untyped dictionary value.
    Dict,
    /// Any object value.
    Object,
    /// Reference into the Lua registry.
    LuaRef,
    /// Buffer handle.
    Buffer,
    /// Window handle.
    Window,
    /// Tabpage handle.
    Tabpage,
    /// Function with no return value.
    Void,
    /// Array whose elements have a declared API type.
    ArrayOf(&'static TypeRef),
    /// Dictionary whose values have a declared API type.
    DictOf(&'static TypeRef),
    /// An exact upstream type expression not covered by the typed variants.
    Named(&'static str),
}

impl fmt::Display for TypeRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nil => formatter.write_str("Nil"),
            Self::Boolean => formatter.write_str("Boolean"),
            Self::Integer => formatter.write_str("Integer"),
            Self::Float => formatter.write_str("Float"),
            Self::String => formatter.write_str("String"),
            Self::Array => formatter.write_str("Array"),
            Self::Dict => formatter.write_str("Dict"),
            Self::Object => formatter.write_str("Object"),
            Self::LuaRef => formatter.write_str("LuaRef"),
            Self::Buffer => formatter.write_str("Buffer"),
            Self::Window => formatter.write_str("Window"),
            Self::Tabpage => formatter.write_str("Tabpage"),
            Self::Void => formatter.write_str("void"),
            Self::ArrayOf(element) => write!(formatter, "ArrayOf({element})"),
            Self::DictOf(value) => write!(formatter, "DictOf({value})"),
            Self::Named(name) => formatter.write_str(name),
        }
    }
}

/// Maps a Rust boundary type to its public API metadata type.
pub trait ApiType {
    /// The public type recorded in API metadata.
    const TYPE: TypeRef;
}

/// Metadata for one exported API function.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FunctionMetadata {
    /// Public function name.
    pub name: &'static str,
    /// API compatibility level that introduced the function.
    pub since: u16,
    /// API compatibility level that deprecated the function.
    pub deprecated_since: Option<u16>,
    /// Whether the first argument is the receiver handle.
    pub method: bool,
    /// Whether the function may execute during a fast callback.
    pub fast: bool,
    /// Whether the function is forbidden while text is locked.
    pub textlock: bool,
    /// Whether the function may run while text is locked despite the default restriction.
    pub textlock_allow: bool,
    /// Public return type.
    pub returns: TypeRef,
    /// Public positional parameter names, types, and optionality.
    pub params: &'static [(&'static str, TypeRef, bool)],
}
