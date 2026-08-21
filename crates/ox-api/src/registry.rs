//! Explicit aggregation of macro-generated API entries.

use std::collections::HashMap;

use ox_types::{ApiError, Object};
use thiserror::Error;

use crate::metadata::FunctionMetadata;

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
    let mut registry = Registry::new();
    crate::buffer::register(&mut registry)?;
    crate::window::register(&mut registry)?;
    crate::tabpage::register(&mut registry)?;
    crate::global::register(&mut registry)?;
    Ok(registry)
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
}
