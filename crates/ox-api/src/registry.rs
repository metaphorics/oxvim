//! Explicit aggregation of macro-generated API entries.

use std::collections::HashMap;

use ox_types::{ApiError, Object};
use thiserror::Error;

use crate::metadata::FunctionMetadata;
use crate::api_function_names::API_FUNCTIONS;

/// The callable shape emitted for every exported API function.
pub type DispatchFn = fn(&mut ox_editor::Editor, &[Object]) -> Result<Object, ApiError>;

#[derive(Clone, Copy)]
struct Entry {
    metadata: FunctionMetadata,
    dispatch: DispatchFn,
}

/// Error returned while assembling an API registry.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RegistryError {
    /// More than one entry used the same public API name.
    #[error("API function `{0}` is already registered")]
    DuplicateName(&'static str),
    /// The checked-in canonical API metadata could not be decoded.
    #[error("canonical API metadata is invalid")]
    InvalidCanonicalMetadata,
}

/// An explicit, insertion-ordered registry of API functions.
///
/// The RPC layer's API-info builder (`ox_rpc::ApiMetadata`) should iterate this
/// registry in order, translate each [`FunctionMetadata`] to the upstream
/// function dictionary, and pass it to `ApiMetadata::add_function`. That fills
/// the `functions` array returned by `nvim_get_api_info`. Keeping registration
/// explicit avoids platform-specific linker sections and makes the metadata
/// order deterministic.
#[derive(Default)]
pub struct Registry {
    entries: Vec<Entry>,
    by_name: HashMap<&'static str, usize>,
}

impl Registry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one macro-generated metadata/dispatch pair.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::DuplicateName`] without modifying the registry
    /// when `metadata.name` is already registered.
    pub fn register(
        &mut self,
        metadata: FunctionMetadata,
        dispatch: DispatchFn,
    ) -> Result<(), RegistryError> {
        if self.by_name.contains_key(metadata.name) {
            return Err(RegistryError::DuplicateName(metadata.name));
        }

        let index = self.entries.len();
        self.by_name.insert(metadata.name, index);
        self.entries.push(Entry { metadata, dispatch });
        Ok(())
    }

    /// Look up an API function by its public name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<(&FunctionMetadata, DispatchFn)> {
        let index = *self.by_name.get(name)?;
        let entry = self.entries.get(index)?;
        Some((&entry.metadata, entry.dispatch))
    }

    /// Iterate over metadata/dispatch pairs in registration order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&FunctionMetadata, DispatchFn)> + '_ {
        self.entries
            .iter()
            .map(|entry| (&entry.metadata, entry.dispatch))
    }

    /// Return the number of registered API functions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no API functions have been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Builds the registry containing every core buffer, window, tabpage, and global API.
///
/// # Errors
///
/// Returns an error if two implementation modules accidentally register the same public name.
pub fn core() -> Result<Registry, RegistryError> {
    let mut implemented = Registry::new();
    crate::autocmd::register(&mut implemented)?;
    crate::buffer::register(&mut implemented)?;
    crate::channel::register(&mut implemented)?;
    crate::context::register(&mut implemented)?;
    crate::deprecated::register(&mut implemented)?;
    crate::extmark::register(&mut implemented)?;
    crate::keymap::register(&mut implemented)?;
    crate::window::register(&mut implemented)?;
    crate::tabpage::register(&mut implemented)?;
    crate::ui::register(&mut implemented)?;
    crate::global::register(&mut implemented)?;

    let mut registry = Registry::new();
    for &metadata in API_FUNCTIONS {
        let dispatch = implemented
            .get(metadata.name)
            .map_or(unavailable_dispatch as DispatchFn, |(_, dispatch)| dispatch);
        registry.register(metadata, dispatch)?;
    }
    Ok(registry)
}

fn unavailable_dispatch(
    _editor: &mut ox_editor::Editor,
    _args: &[Object],
) -> Result<Object, ApiError> {
    Err(ApiError::exception("API function is not implemented"))
}

#[cfg(test)]
mod tests {
    use super::{Registry, RegistryError};
    use crate::metadata::{FunctionMetadata, TypeRef};
    use ox_editor::Editor;
    use ox_types::{ApiError, Object};

    fn dispatch(_editor: &mut Editor, args: &[Object]) -> Result<Object, ApiError> {
        Ok(args.first().cloned().unwrap_or(Object::Nil))
    }

    fn metadata(name: &'static str) -> FunctionMetadata {
        FunctionMetadata {
            name,
            since: 1,
            deprecated_since: None,
            method: false,
            fast: false,
            textlock: false,
            textlock_allow: false,
            returns: TypeRef::Nil,
            params: &[],
        }
    }

    #[test]
    fn duplicate_names_are_rejected_without_mutation() {
        let mut registry = Registry::new();
        assert_eq!(registry.register(metadata("one"), dispatch), Ok(()));
        assert_eq!(
            registry.register(metadata("one"), dispatch),
            Err(RegistryError::DuplicateName("one"))
        );
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn lookup_and_iteration_preserve_registration_order() {
        let mut registry = Registry::new();
        assert_eq!(registry.register(metadata("second"), dispatch), Ok(()));
        assert_eq!(registry.register(metadata("first"), dispatch), Ok(()));

        let names: Vec<_> = registry.iter().map(|(metadata, _)| metadata.name).collect();
        assert_eq!(names, ["second", "first"]);
        let mut editor = Editor::new();
        let result = registry
            .get("first")
            .map(|(_, dispatch)| dispatch(&mut editor, &[Object::Integer(1)]));
        assert_eq!(result, Some(Ok(Object::Integer(1))));
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn core_registry_covers_canonical_api_inventory() {
        let registry = super::core().unwrap();
        let canonical = ox_rpc::canonical_metadata().unwrap();
        let Object::Dict(root) = canonical else { panic!("metadata must be a dictionary") };
        let Object::Array(functions) = root.get(&ox_types::OxStr::from("functions")).unwrap() else { panic!("functions must be an array") };
        assert_eq!(registry.len(), functions.len());
        for ((registered, _), canonical) in registry.iter().zip(functions) {
            let Object::Dict(fields) = canonical else { panic!("function must be a dictionary") };
            assert_eq!(fields.get(&ox_types::OxStr::from("name")), Some(&Object::String(ox_types::OxStr::from(registered.name))));
            assert_eq!(fields.get(&ox_types::OxStr::from("since")), Some(&Object::Integer(i64::from(registered.since))));
            assert_eq!(fields.get(&ox_types::OxStr::from("deprecated_since")), registered.deprecated_since.map(|value| Object::Integer(i64::from(value))).as_ref());
            assert_eq!(fields.get(&ox_types::OxStr::from("method")), Some(&Object::Boolean(registered.method)));
            assert_eq!(fields.get(&ox_types::OxStr::from("return_type")), Some(&Object::String(ox_types::OxStr::from(registered.returns.to_string().as_str()))));
            let Object::Array(parameters) = fields.get(&ox_types::OxStr::from("parameters")).unwrap() else { panic!("parameters must be an array") };
            assert_eq!(parameters.len(), registered.params.len(), "{}", registered.name);
            for (actual, (name, ty, optional)) in parameters.iter().zip(registered.params) {
                assert_eq!(actual, &Object::Array(vec![Object::String(ox_types::OxStr::from(ty.to_string().as_str())), Object::String(ox_types::OxStr::from(*name)), Object::Boolean(*optional)]), "{}", registered.name);
            }
        }
        assert_eq!(registry.iter().filter(|(metadata, _)| metadata.fast).count(), 14);
        assert_eq!(registry.iter().filter(|(metadata, _)| metadata.textlock).count(), 16);
        assert_eq!(registry.iter().filter(|(metadata, _)| metadata.textlock_allow).count(), 0);
        assert!(registry.get("nvim_get_api_info").unwrap().0.fast);
        assert!(registry.get("nvim_buf_set_lines").unwrap().0.textlock);
    }
}
