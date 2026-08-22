//! MessagePack API metadata output.

use ox_api::{Dict, FunctionMetadata, Object, OxStr, RegistryError};

/// Build the metadata dictionary printed by `--api-info`.
pub fn metadata() -> Result<Object, RegistryError> {
    let registry = ox_api::core()?;
    let mut result = ox_rpc::ApiMetadata::new();
    for (function, _) in registry.iter() {
        result.add_function(Object::Dict(function_object(function)));
    }
    Ok(result.metadata_object())
}

/// Encode API metadata as exactly one MessagePack value.
pub fn encoded() -> Result<Vec<u8>, RegistryError> {
    metadata().map(|object| ox_rpc::encode(&object))
}

fn function_object(metadata: &FunctionMetadata) -> Dict {
    let parameters = metadata
        .params
        .iter()
        .map(|(name, ty)| {
            Object::Array(vec![
                Object::String(OxStr::from(ty.to_string().as_str())),
                Object::String(OxStr::from(*name)),
            ])
        })
        .collect();
    let mut fields = vec![
        (OxStr::from("name"), Object::String(OxStr::from(metadata.name))),
        (OxStr::from("parameters"), Object::Array(parameters)),
        (
            OxStr::from("return_type"),
            Object::String(OxStr::from(metadata.returns.to_string().as_str())),
        ),
        (OxStr::from("since"), Object::Integer(i64::from(metadata.since))),
        (OxStr::from("method"), Object::Boolean(metadata.method)),
        (OxStr::from("fast"), Object::Boolean(metadata.fast)),
        (OxStr::from("textlock"), Object::Boolean(metadata.textlock)),
    ];
    if let Some(since) = metadata.deprecated_since {
        fields.push((OxStr::from("deprecated_since"), Object::Integer(i64::from(since))));
    }
    Dict(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(value: &'a Object, name: &str) -> &'a Object {
        let Object::Dict(dict) = value else { panic!("expected dictionary") };
        dict.0.iter().find(|(key, _)| key.as_bytes() == name.as_bytes()).map(|(_, value)| value).unwrap()
    }

    #[test]
    fn assembled_metadata_has_api_level_and_functions() {
        let value = metadata().unwrap();
        assert_eq!(field(field(&value, "version"), "api_level"), &Object::Integer(15));
        let Object::Array(functions) = field(&value, "functions") else { panic!("expected functions") };
        assert!(!functions.is_empty());
    }
}
