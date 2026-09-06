//! RPC-boundary Typval → Object conversion.
//!
//! Mirrors upstream `api/private/converter.c` `vim_to_object`: the shape the
//! msgpack layer can actually carry. The load-bearing differences from the
//! Vimscript-side conversions live in the function case
//! (`TYPVAL_ENCODE_CONV_FUNC_START`, converter.c:77): a Lua-registered
//! function becomes [`Object::LuaRef`], every other function or partial
//! becomes [`Object::Nil`]; blobs become byte strings (converter.c:74), and
//! channel/job identifiers become plain integers exactly like upstream's
//! `VAR_NUMBER` handles.

use ox_types::{Dict, Funcref, Object, OxStr, Special, Typval};

/// Converts one Vimscript [`Typval`] to its RPC-representable [`Object`].
///
/// This is the upstream-`vim_to_object` shape: buffers/windows/tabpages stay
/// integers, Lua callbacks survive as [`Object::LuaRef`], and anything that
/// has no wire form collapses to [`Object::Nil`] instead of erroring.
#[must_use]
pub fn typval_to_object(value: &Typval) -> Object {
    match value {
        Typval::Number(value) => Object::Integer(*value),
        Typval::Float(value) => Object::Float(*value),
        Typval::String(value) => Object::String(value.clone()),
        Typval::Blob(bytes) => Object::String(OxStr::from(bytes.as_slice())),
        Typval::Bool(value) => Object::Boolean(*value),
        Typval::Special(Special::Null) => Object::Nil,
        Typval::Channel(id) | Typval::Job(id) => {
            Object::Integer(i64::try_from(*id).unwrap_or(i64::MAX))
        }
        Typval::Funcref(function) | Typval::Partial(function) => funcref_to_object(function),
        Typval::List(values) => values
            .try_borrow()
            .map_or(Object::Nil, |data| Object::Array(
                data.items.iter().map(typval_to_object).collect(),
            )),
        Typval::Dict(values) => values
            .try_borrow()
            .map_or(Object::Nil, |data| Object::Dict(Dict(
                data.entries
                    .iter()
                    .map(|entry| (entry.key.clone(), typval_to_object(&entry.value)))
                    .collect(),
            ))),
    }
}

/// One function value at the RPC boundary: the Lua registry reference when
/// the function is Lua-backed, `Nil` otherwise (`TYPVAL_ENCODE_CONV_FUNC_START`).
fn funcref_to_object(function: &Funcref) -> Object {
    match function.registry {
        Some(reference) => Object::LuaRef(i32::try_from(reference).unwrap_or(i32::MAX)),
        None => Object::Nil,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_types::BufHandle;

    #[test]
    fn lua_registered_funcref_becomes_lua_ref_and_others_nil() {
        let lua_backed = Funcref {
            name: OxStr::from("<lua 7>"),
            args: Vec::new(),
            dict: None,
            registry: Some(7),
        };
        assert_eq!(
            typval_to_object(&Typval::Funcref(lua_backed.clone())),
            Object::LuaRef(7)
        );
        assert_eq!(
            typval_to_object(&Typval::Partial(lua_backed)),
            Object::LuaRef(7)
        );

        let named = Funcref {
            name: OxStr::from("setqflist"),
            args: Vec::new(),
            dict: None,
            registry: None,
        };
        // converter.c:77: a non-Lua function has no wire form, so NIL.
        assert_eq!(typval_to_object(&Typval::Funcref(named)), Object::Nil);
    }

    #[test]
    fn handles_channels_and_scalars_map_upstream() {
        assert_eq!(
            typval_to_object(&Typval::Channel(42)),
            Object::Integer(42),
            "channel ids are plain integers at the RPC boundary"
        );
        assert_eq!(
            typval_to_object(&Typval::Job(9)),
            Object::Integer(9),
            "job ids are plain integers at the RPC boundary"
        );
        assert_eq!(
            typval_to_object(&Typval::Blob(vec![1, 2, 3])),
            Object::String(OxStr::from(&[1u8, 2, 3][..])),
            "blobs convert to byte strings (converter.c:74)"
        );
        assert_eq!(
            typval_to_object(&Typval::Special(Special::Null)),
            Object::Nil
        );
        assert_eq!(
            typval_to_object(&Typval::Bool(true)),
            Object::Boolean(true)
        );
    }

    #[test]
    fn containers_convert_recursively_and_locked_borrow_falls_to_nil() {
        let list = Typval::list(vec![
            Typval::Number(1),
            Typval::Funcref(Funcref {
                name: OxStr::from("<lua 3>"),
                args: Vec::new(),
                dict: None,
                registry: Some(3),
            }),
        ]);
        assert_eq!(
            typval_to_object(&list),
            Object::Array(vec![Object::Integer(1), Object::LuaRef(3)])
        );

        let dict = Typval::dict(vec![(
            OxStr::from("buf"),
            Typval::Number(i64::from(BufHandle::try_from(3).unwrap())),
        )]);
        let converted = typval_to_object(&dict);
        let Object::Dict(entries) = converted else {
            panic!("dict must convert to Object::Dict");
        };
        assert_eq!(
            entries.get(&OxStr::from("buf")),
            Some(&Object::Integer(3)),
            "buffer handles stay integers at the RPC boundary"
        );

        // A mutably borrowed container cannot be walked; the boundary degrades
        // to NIL instead of failing the call, matching the other nil-ing cases.
        let locked = Typval::list(vec![Typval::Number(5)]);
        let Typval::List(values) = &locked else {
            unreachable!()
        };
        let _guard = values.borrow_mut();
        assert_eq!(typval_to_object(&locked), Object::Nil);
    }
}
