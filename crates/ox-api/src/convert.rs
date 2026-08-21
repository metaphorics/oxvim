//! Conversion between Rust API boundary types and [`Object`].

use ox_types::{ApiError, BufHandle, Dict, Object, OxStr, TabHandle, WinHandle};

use crate::metadata::{ApiType, TypeRef};

/// A typed MessagePack nil value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Nil;

/// A reference into the Lua registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LuaRef(pub i32);

/// Converts an API [`Object`] argument into a Rust value.
///
/// `argument` is one-based, matching Neovim's generated dispatch diagnostics.
pub trait FromObject: ApiType + Sized {
    /// Convert `object`, or return an upstream-compatible argument error.
    fn from_object(
        object: &Object,
        argument: usize,
        function: &str,
    ) -> Result<Self, ApiError>;
}

/// Converts a Rust API return value into an [`Object`].
pub trait IntoObject: ApiType {
    /// Convert this value into its API wire representation.
    fn into_object(self) -> Object;
}

fn wrong_type<T: ApiType>(argument: usize, function: &str) -> ApiError {
    ApiError::exception(format!(
        "Wrong type for argument {argument} when calling {function}, expecting {}",
        T::TYPE
    ))
}

macro_rules! exact_conversion {
    ($rust:ty, $type_ref:expr, $variant:ident) => {
        impl ApiType for $rust {
            const TYPE: TypeRef = $type_ref;
        }

        impl FromObject for $rust {
            fn from_object(
                object: &Object,
                argument: usize,
                function: &str,
            ) -> Result<Self, ApiError> {
                if let Object::$variant(value) = object {
                    Ok(value.clone())
                } else {
                    Err(wrong_type::<Self>(argument, function))
                }
            }
        }

        impl IntoObject for $rust {
            fn into_object(self) -> Object {
                Object::$variant(self)
            }
        }
    };
}

exact_conversion!(i64, TypeRef::Integer, Integer);
exact_conversion!(OxStr, TypeRef::String, String);

impl ApiType for Dict {
    const TYPE: TypeRef = TypeRef::Dict;
}

impl FromObject for Dict {
    fn from_object(object: &Object, argument: usize, function: &str) -> Result<Self, ApiError> {
        match object {
            Object::Dict(value) => Ok(value.clone()),
            Object::Array(values) if values.is_empty() => Ok(Self(Vec::new())),
            _ => Err(wrong_type::<Self>(argument, function)),
        }
    }
}

impl IntoObject for Dict {
    fn into_object(self) -> Object {
        Object::Dict(self)
    }
}

impl ApiType for bool {
    const TYPE: TypeRef = TypeRef::Boolean;
}

impl FromObject for bool {
    fn from_object(object: &Object, argument: usize, function: &str) -> Result<Self, ApiError> {
        match object {
            Object::Boolean(value) => Ok(*value),
            Object::Integer(value) if *value >= 0 => Ok(*value != 0),
            _ => Err(wrong_type::<Self>(argument, function)),
        }
    }
}

impl IntoObject for bool {
    fn into_object(self) -> Object {
        Object::Boolean(self)
    }
}

impl ApiType for f64 {
    const TYPE: TypeRef = TypeRef::Float;
}

impl FromObject for f64 {
    fn from_object(object: &Object, argument: usize, function: &str) -> Result<Self, ApiError> {
        match object {
            Object::Float(value) => Ok(*value),
            Object::Integer(value) => Ok(*value as Self),
            _ => Err(wrong_type::<Self>(argument, function)),
        }
    }
}

impl IntoObject for f64 {
    fn into_object(self) -> Object {
        Object::Float(self)
    }
}

impl<T: ApiType> ApiType for Vec<T> {
    const TYPE: TypeRef = TypeRef::ArrayOf(&T::TYPE);
}

impl<T: FromObject> FromObject for Vec<T> {
    fn from_object(object: &Object, argument: usize, function: &str) -> Result<Self, ApiError> {
        let Object::Array(values) = object else {
            return Err(wrong_type::<Self>(argument, function));
        };
        values
            .iter()
            .map(|value| T::from_object(value, argument, function))
            .collect()
    }
}

impl<T: IntoObject> IntoObject for Vec<T> {
    fn into_object(self) -> Object {
        Object::Array(self.into_iter().map(IntoObject::into_object).collect())
    }
}

macro_rules! handle_conversion {
    ($handle:ty, $type_ref:expr, $variant:ident) => {
        impl ApiType for $handle {
            const TYPE: TypeRef = $type_ref;
        }

        impl FromObject for $handle {
            fn from_object(
                object: &Object,
                argument: usize,
                function: &str,
            ) -> Result<Self, ApiError> {
                match object {
                    Object::$variant(value) => Ok(*value),
                    Object::Integer(value) if *value >= 0 => {
                        Self::try_from(*value).map_err(|_| wrong_type::<Self>(argument, function))
                    }
                    _ => Err(wrong_type::<Self>(argument, function)),
                }
            }
        }

        impl IntoObject for $handle {
            fn into_object(self) -> Object {
                Object::$variant(self)
            }
        }
    };
}

handle_conversion!(BufHandle, TypeRef::Buffer, Buffer);
handle_conversion!(WinHandle, TypeRef::Window, Window);
handle_conversion!(TabHandle, TypeRef::Tabpage, Tabpage);

impl ApiType for LuaRef {
    const TYPE: TypeRef = TypeRef::LuaRef;
}

impl FromObject for LuaRef {
    fn from_object(object: &Object, argument: usize, function: &str) -> Result<Self, ApiError> {
        if let Object::LuaRef(value) = object {
            Ok(Self(*value))
        } else {
            Err(wrong_type::<Self>(argument, function))
        }
    }
}

impl IntoObject for LuaRef {
    fn into_object(self) -> Object {
        Object::LuaRef(self.0)
    }
}

impl ApiType for Nil {
    const TYPE: TypeRef = TypeRef::Nil;
}

impl FromObject for Nil {
    fn from_object(object: &Object, argument: usize, function: &str) -> Result<Self, ApiError> {
        if matches!(object, Object::Nil) {
            Ok(Self)
        } else {
            Err(wrong_type::<Self>(argument, function))
        }
    }
}

impl IntoObject for Nil {
    fn into_object(self) -> Object {
        Object::Nil
    }
}

impl ApiType for () {
    const TYPE: TypeRef = TypeRef::Void;
}

impl FromObject for () {
    fn from_object(object: &Object, argument: usize, function: &str) -> Result<Self, ApiError> {
        if matches!(object, Object::Nil) {
            Ok(())
        } else {
            Err(wrong_type::<Self>(argument, function))
        }
    }
}

impl IntoObject for () {
    fn into_object(self) -> Object {
        Object::Nil
    }
}

impl ApiType for Object {
    const TYPE: TypeRef = TypeRef::Nil;
}

impl FromObject for Object {
    fn from_object(object: &Object, _argument: usize, _function: &str) -> Result<Self, ApiError> {
        Ok(object.clone())
    }
}

impl IntoObject for Object {
    fn into_object(self) -> Object {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{FromObject, IntoObject, LuaRef, Nil};
    use ox_types::{ApiError, BufHandle, Dict, Object, OxStr};

    #[test]
    fn errors_use_generated_dispatch_phrase() {
        let error = bool::from_object(&Object::String(OxStr::from("x")), 2, "nvim_demo");
        assert_eq!(
            error,
            Err(ApiError::exception(
                "Wrong type for argument 2 when calling nvim_demo, expecting Boolean"
            ))
        );

        let error = Vec::<i64>::from_object(&Object::Nil, 1, "nvim_array");
        assert_eq!(
            error,
            Err(ApiError::exception(
                "Wrong type for argument 1 when calling nvim_array, expecting ArrayOf(Integer)"
            ))
        );
    }

    #[test]
    fn upstream_compatible_coercions_are_supported() {
        assert_eq!(bool::from_object(&Object::Integer(2), 1, "f"), Ok(true));
        assert_eq!(f64::from_object(&Object::Integer(-2), 1, "f"), Ok(-2.0));
        assert_eq!(
            Dict::from_object(&Object::Array(Vec::new()), 1, "f"),
            Ok(Dict(Vec::new()))
        );
        assert_eq!(
            BufHandle::from_object(&Object::Integer(0), 1, "f"),
            Ok(BufHandle::CURRENT)
        );
    }

    #[test]
    fn supported_types_round_trip() {
        let buffer = BufHandle::try_from(4).unwrap_or(BufHandle::CURRENT);
        let cases = [
            true.into_object(),
            4_i64.into_object(),
            2.5_f64.into_object(),
            OxStr::from("x").into_object(),
            vec![1_i64, 2].into_object(),
            Dict(Vec::new()).into_object(),
            LuaRef(8).into_object(),
            buffer.into_object(),
            Nil.into_object(),
            ().into_object(),
        ];
        assert_eq!(cases[0], Object::Boolean(true));
        assert_eq!(cases[4], Object::Array(vec![Object::Integer(1), Object::Integer(2)]));
        assert_eq!(cases[6], Object::LuaRef(8));
        assert_eq!(cases[8], Object::Nil);
        assert_eq!(cases[9], Object::Nil);
    }
}
