//! `MessagePack` API metadata output.

use ox_api::{Object, RegistryError};

/// Build the metadata dictionary printed by `--api-info`.
pub fn metadata() -> Result<Object, RegistryError> {
    ox_rpc::canonical_metadata().map_err(|_| RegistryError::InvalidCanonicalMetadata)
}

/// Encode API metadata as exactly one `MessagePack` value.
pub fn encoded() -> Result<Vec<u8>, RegistryError> {
    let object = metadata()?;
    let mut encoded = Vec::new();
    ox_rpc::encode(&mut encoded, &object).map_err(|_| RegistryError::MetadataEncode)?;
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::panic,
        clippy::unwrap_used,
        reason = "assertion helper requires dictionary fields to exist"
    )]
    fn field<'a>(value: &'a Object, name: &str) -> &'a Object {
        let Object::Dict(dict) = value else {
            panic!("expected dictionary")
        };
        dict.0
            .iter()
            .find(|(key, _)| key.as_bytes() == name.as_bytes())
            .map(|(_, value)| value)
            .unwrap()
    }

    #[test]
    #[expect(
        clippy::panic,
        clippy::unwrap_used,
        reason = "metadata shape assertions require successful assembly and an array"
    )]
    fn assembled_metadata_has_api_level_and_functions() {
        let value = metadata().unwrap();
        assert_eq!(
            field(field(&value, "version"), "api_level"),
            &Object::Integer(15)
        );
        let Object::Array(functions) = field(&value, "functions") else {
            panic!("expected functions")
        };
        assert!(!functions.is_empty());
    }
}
