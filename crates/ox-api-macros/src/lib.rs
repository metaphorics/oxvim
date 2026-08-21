#![forbid(unsafe_code)]
//! The `#[api]` proc-macro for dispatch and metadata code generation.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{
    Error, Expr, FnArg, GenericArgument, ItemFn, Lit, Meta, Pat, PathArguments, ReturnType, Token,
    Type,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ApiArgs {
    since: Option<u16>,
    deprecated_since: Option<u16>,
    fast: bool,
    textlock: bool,
    method: bool,
    noexport: bool,
}

impl ApiArgs {
    fn parse(tokens: TokenStream2) -> Result<Self, Error> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(tokens)?;
        let mut args = Self::default();

        for meta in metas {
            match meta {
                Meta::NameValue(value) if value.path.is_ident("since") => {
                    set_number(&mut args.since, &value.value, "since")?;
                }
                Meta::NameValue(value) if value.path.is_ident("deprecated_since") => {
                    set_number(
                        &mut args.deprecated_since,
                        &value.value,
                        "deprecated_since",
                    )?;
                }
                Meta::Path(path) if path.is_ident("fast") => {
                    set_flag(&mut args.fast, &path, "fast")?;
                }
                Meta::Path(path) if path.is_ident("textlock") => {
                    set_flag(&mut args.textlock, &path, "textlock")?;
                }
                Meta::Path(path) if path.is_ident("method") => {
                    set_flag(&mut args.method, &path, "method")?;
                }
                Meta::Path(path) if path.is_ident("noexport") => {
                    set_flag(&mut args.noexport, &path, "noexport")?;
                }
                unsupported => {
                    return Err(Error::new_spanned(
                        unsupported,
                        "unknown #[api] attribute; expected since, deprecated_since, fast, textlock, method, or noexport",
                    ));
                }
            }
        }

        if args.since.is_none() && !args.noexport {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "exported #[api] functions require `since = N`",
            ));
        }
        if args.fast && args.textlock {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "#[api] attributes `fast` and `textlock` cannot be combined",
            ));
        }
        if let (Some(since), Some(deprecated)) = (args.since, args.deprecated_since)
            && deprecated < since
        {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "`deprecated_since` cannot precede `since`",
            ));
        }

        Ok(args)
    }
}

fn set_number(slot: &mut Option<u16>, expression: &Expr, name: &str) -> Result<(), Error> {
    if slot.is_some() {
        return Err(Error::new_spanned(
            expression,
            format!("duplicate #[api] attribute `{name}`"),
        ));
    }
    let Expr::Lit(expression) = expression else {
        return Err(Error::new_spanned(
            expression,
            format!("`{name}` must be an integer literal"),
        ));
    };
    let Lit::Int(value) = &expression.lit else {
        return Err(Error::new_spanned(
            expression,
            format!("`{name}` must be an integer literal"),
        ));
    };
    *slot = Some(value.base10_parse::<u16>()?);
    Ok(())
}

fn set_flag(slot: &mut bool, path: &syn::Path, name: &str) -> Result<(), Error> {
    if *slot {
        return Err(Error::new_spanned(
            path,
            format!("duplicate #[api] attribute `{name}`"),
        ));
    }
    *slot = true;
    Ok(())
}

struct Signature {
    parameter_names: Vec<syn::Ident>,
    parameter_types: Vec<Type>,
    return_type: Type,
}

fn validate_signature(function: &ItemFn, args: ApiArgs) -> Result<Signature, Error> {
    if function.sig.asyncness.is_some() {
        return Err(Error::new_spanned(
            &function.sig.asyncness,
            "#[api] functions cannot be async",
        ));
    }
    if matches!(function.sig.safety, syn::Safety::Unsafe(_)) {
        return Err(Error::new_spanned(
            &function.sig.safety,
            "#[api] functions cannot be unsafe",
        ));
    }
    if !function.sig.generics.params.is_empty() || function.sig.generics.where_clause.is_some() {
        return Err(Error::new_spanned(
            &function.sig.generics,
            "#[api] functions cannot be generic",
        ));
    }

    let mut parameter_names = Vec::with_capacity(function.sig.inputs.len());
    let mut parameter_types = Vec::with_capacity(function.sig.inputs.len());
    for input in &function.sig.inputs {
        let FnArg::Typed(parameter) = input else {
            return Err(Error::new_spanned(input, "#[api] functions cannot have a receiver"));
        };
        let Pat::Ident(name) = parameter.pat.as_ref() else {
            return Err(Error::new_spanned(
                &parameter.pat,
                "#[api] parameters must use identifier patterns",
            ));
        };
        if name.by_ref.is_some() || name.subpat.is_some() {
            return Err(Error::new_spanned(
                name,
                "#[api] parameters must be plain identifiers",
            ));
        }
        parameter_names.push(name.ident.clone());
        parameter_types.push((*parameter.ty).clone());
    }

    let return_type = result_ok_type(&function.sig.output)?;
    if args.method {
        validate_method_receiver(function, &parameter_types)?;
    }

    Ok(Signature {
        parameter_names,
        parameter_types,
        return_type,
    })
}

fn result_ok_type(output: &ReturnType) -> Result<Type, Error> {
    let ReturnType::Type(_, output_type) = output else {
        return Err(Error::new_spanned(
            output,
            "#[api] functions must return Result<T, ApiError>",
        ));
    };
    let Type::Path(path) = output_type.as_ref() else {
        return Err(Error::new_spanned(
            output_type,
            "#[api] functions must return Result<T, ApiError>",
        ));
    };
    let Some(result) = path.path.segments.last() else {
        return Err(Error::new_spanned(
            path,
            "#[api] functions must return Result<T, ApiError>",
        ));
    };
    if result.ident != "Result" {
        return Err(Error::new_spanned(
            result,
            "#[api] functions must return Result<T, ApiError>",
        ));
    }
    let PathArguments::AngleBracketed(arguments) = &result.arguments else {
        return Err(Error::new_spanned(
            result,
            "#[api] functions must return Result<T, ApiError>",
        ));
    };
    let types: Vec<&Type> = arguments
        .args
        .iter()
        .filter_map(|argument| match argument {
            GenericArgument::Type(argument) => Some(argument),
            _ => None,
        })
        .collect();
    if types.len() != 2 || !type_last_ident(types[1]).is_some_and(|ident| ident == "ApiError") {
        return Err(Error::new_spanned(
            arguments,
            "#[api] functions must return Result<T, ApiError>",
        ));
    }
    Ok(types[0].clone())
}

fn validate_method_receiver(function: &ItemFn, parameter_types: &[Type]) -> Result<(), Error> {
    let name = function.sig.ident.to_string();
    let expected = if name.starts_with("nvim_buf_") {
        "BufHandle"
    } else if name.starts_with("nvim_win_") {
        "WinHandle"
    } else if name.starts_with("nvim_tabpage_") {
        "TabHandle"
    } else {
        return Err(Error::new_spanned(
            &function.sig.ident,
            "#[api(method)] requires an nvim_buf_*, nvim_win_*, or nvim_tabpage_* name",
        ));
    };

    let Some(receiver) = parameter_types.first() else {
        return Err(Error::new_spanned(
            &function.sig.ident,
            format!("#[api(method)] function `{name}` requires `{expected}` as its first parameter"),
        ));
    };
    if !type_last_ident(receiver).is_some_and(|ident| ident == expected) {
        return Err(Error::new_spanned(
            receiver,
            format!("#[api(method)] function `{name}` requires `{expected}` as its first parameter"),
        ));
    }
    Ok(())
}

fn type_last_ident(ty: &Type) -> Option<&syn::Ident> {
    let Type::Path(path) = ty else {
        return None;
    };
    path.path.segments.last().map(|segment| &segment.ident)
}

fn expand(args: ApiArgs, function: ItemFn) -> Result<TokenStream2, Error> {
    let signature = validate_signature(&function, args)?;
    if args.noexport {
        return Ok(quote!(#function));
    }

    let name = &function.sig.ident;
    let visibility = &function.vis;
    let function_name = name.to_string();
    let metadata_const = format_ident!("{}__API_META", name);
    let dispatch_const = format_ident!("{}__API_DISPATCH", name);
    let metadata_function = format_ident!("__{}_api_meta", name);
    let dispatch_function = format_ident!("__{}_api_dispatch", name);
    let parameter_names = &signature.parameter_names;
    let parameter_name_strings: Vec<String> = parameter_names.iter().map(ToString::to_string).collect();
    let parameter_types = &signature.parameter_types;
    let return_type = &signature.return_type;
    let argument_count = parameter_names.len();
    let argument_positions = 1..=argument_count;
    let argument_indexes = 0..argument_count;
    let since = args.since.ok_or_else(|| {
        Error::new(
            proc_macro2::Span::call_site(),
            "exported #[api] functions require `since = N`",
        )
    })?;
    let deprecated_since = match args.deprecated_since {
        Some(value) => quote!(::core::option::Option::Some(#value)),
        None => quote!(::core::option::Option::None),
    };
    let method = args.method;
    let fast = args.fast;
    let textlock = args.textlock;

    Ok(quote! {
        #function

        fn #metadata_function() -> ::ox_api::FunctionMetadata {
            ::ox_api::FunctionMetadata {
                name: #function_name,
                since: #since,
                deprecated_since: #deprecated_since,
                method: #method,
                fast: #fast,
                textlock: #textlock,
                returns: <#return_type as ::ox_api::ApiType>::TYPE,
                params: &[
                    #((#parameter_name_strings, <#parameter_types as ::ox_api::ApiType>::TYPE)),*
                ],
            }
        }

        #[doc = concat!("Metadata constructor for [`", stringify!(#name), "`].")]
        #[allow(non_upper_case_globals)]
        #visibility const #metadata_const: fn() -> ::ox_api::FunctionMetadata = #metadata_function;

        fn #dispatch_function(
            arguments: &[::ox_api::Object],
        ) -> ::core::result::Result<::ox_api::Object, ::ox_api::ApiError> {
            if arguments.len() != #argument_count {
                return ::core::result::Result::Err(::ox_api::ApiError::exception(
                    ::std::format!(
                        "Wrong number of arguments: expecting {} but got {}",
                        #argument_count,
                        arguments.len(),
                    ),
                ));
            }
            #(
                let #parameter_names = <#parameter_types as ::ox_api::FromObject>::from_object(
                    &arguments[#argument_indexes],
                    #argument_positions,
                    #function_name,
                )?;
            )*
            let result = #name(#(#parameter_names),*)?;
            ::core::result::Result::Ok(
                <#return_type as ::ox_api::IntoObject>::into_object(result)
            )
        }

        #[doc = concat!("Object-array dispatch shim for [`", stringify!(#name), "`].")]
        #[allow(non_upper_case_globals)]
        #visibility const #dispatch_const: ::ox_api::DispatchFn = #dispatch_function;
    })
}

/// Generate API metadata and positional Object dispatch for a Rust function.
#[proc_macro_attribute]
pub fn api(attributes: TokenStream, item: TokenStream) -> TokenStream {
    let result = ApiArgs::parse(TokenStream2::from(attributes)).and_then(|args| {
        let function = syn::parse2::<ItemFn>(TokenStream2::from(item))?;
        expand(args, function)
    });
    match result {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiArgs, expand};
    use quote::quote;

    fn error(attributes: proc_macro2::TokenStream, function: proc_macro2::TokenStream) -> String {
        ApiArgs::parse(attributes)
            .and_then(|args| syn::parse2(function).and_then(|function| expand(args, function)))
            .expect_err("test input should be rejected")
            .to_string()
    }

    #[test]
    fn parses_the_exact_attribute_set() {
        let args = ApiArgs::parse(quote!(
            since = 1, deprecated_since = 4, fast, method
        ))
        .expect("valid attributes");
        assert_eq!(args.since, Some(1));
        assert_eq!(args.deprecated_since, Some(4));
        assert!(args.fast && args.method);

        let error = ApiArgs::parse(quote!(since = 1, remote_only))
            .expect_err("unknown attribute")
            .to_string();
        assert!(error.starts_with("unknown #[api] attribute"));
    }

    #[test]
    fn rejects_duplicate_malformed_and_conflicting_attributes() {
        assert!(ApiArgs::parse(quote!(since = 1, since = 2)).is_err());
        assert!(ApiArgs::parse(quote!(since = "1")).is_err());
        assert!(ApiArgs::parse(quote!(since = 1, fast, textlock)).is_err());
        assert!(ApiArgs::parse(quote!(since = 4, deprecated_since = 3)).is_err());
        assert!(ApiArgs::parse(quote!(fast)).is_err());
        assert!(ApiArgs::parse(quote!(noexport)).is_ok());
    }

    #[test]
    fn noexport_preserves_only_the_original_function() {
        let args = ApiArgs::parse(quote!(noexport)).expect("valid noexport attribute");
        let function = syn::parse2(quote!(
            fn internal(value: i64) -> Result<i64, ApiError> { Ok(value) }
        ))
        .expect("valid function");
        let expansion = expand(args, function).expect("valid noexport function").to_string();

        assert!(expansion.contains("fn internal"));
        assert!(!expansion.contains("API_META"));
        assert!(!expansion.contains("API_DISPATCH"));
    }

    #[test]
    fn validates_method_name_and_receiver_prefix() {
        let valid = error(
            quote!(since = 1, method),
            quote!(fn nvim_buf_get_name(buf: WinHandle) -> Result<Object, ApiError> { loop {} }),
        );
        assert_eq!(
            valid,
            "#[api(method)] function `nvim_buf_get_name` requires `BufHandle` as its first parameter"
        );

        let invalid_name = error(
            quote!(since = 1, method),
            quote!(fn nvim_get_name(buf: BufHandle) -> Result<Object, ApiError> { loop {} }),
        );
        assert_eq!(
            invalid_name,
            "#[api(method)] requires an nvim_buf_*, nvim_win_*, or nvim_tabpage_* name"
        );
    }

    #[test]
    fn rejects_non_dispatchable_signatures() {
        assert_eq!(
            error(
                quote!(since = 1),
                quote!(fn nvim_bad(value: i64) -> Object { loop {} }),
            ),
            "#[api] functions must return Result<T, ApiError>"
        );
        assert_eq!(
            error(
                quote!(since = 1),
                quote!(fn nvim_bad((value, _): (i64, i64)) -> Result<Object, ApiError> { loop {} }),
            ),
            "#[api] parameters must use identifier patterns"
        );
    }
}
