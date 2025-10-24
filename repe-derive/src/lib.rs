use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{ToTokens, quote};
use syn::{
    Attribute, DeriveInput, Field, Ident, LitStr, Token, Type,
    parse::{Parse, ParseBuffer, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
    spanned::Spanned,
};

#[proc_macro_derive(RepeStruct, attributes(repe))]
pub fn derive_repe_struct(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_repe_struct(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_repe_struct(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let repe_path = repe_crate_path();

    let struct_ident = &input.ident;
    let syn::Data::Struct(data_struct) = &input.data else {
        return Err(syn::Error::new_spanned(
            input,
            "RepeStruct can only be derived for structs",
        ));
    };

    let fields = match &data_struct.fields {
        syn::Fields::Named(named) => &named.named,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "RepeStruct requires named fields",
            ));
        }
    };

    let struct_attrs = parse_struct_attrs(&input.attrs)?;
    let field_specs = fields
        .iter()
        .map(parse_field)
        .collect::<syn::Result<Vec<_>>>()?;

    let root_map_entries = build_root_map_entries(&field_specs, &struct_attrs.methods, &repe_path);
    let field_match_arms = build_field_match_arms(&field_specs, &repe_path);
    let method_match_arms = build_method_match_arms(&struct_attrs.methods, &repe_path);
    let root_path_deserialize = quote! {
        *self = ::serde_json::from_value(value).map_err(|source| #repe_path::StructError::Deserialize {
            path: String::from(""),
            source,
        })?;
        return Ok(None);
    };

    let expanded = quote! {
        impl #repe_path::RepeStruct for #struct_ident {
            fn repe_handle(
                &mut self,
                segments: &[&str],
                body: Option<::serde_json::Value>,
            ) -> #repe_path::structs::StructResult<Option<::serde_json::Value>> {
                if segments.is_empty() {
                    if let Some(value) = body {
                        #root_path_deserialize
                    }
                    let mut map = ::serde_json::Map::new();
                    #(#root_map_entries)*
                    return Ok(Some(::serde_json::Value::Object(map)));
                }

                let (head, tail) = segments.split_first().unwrap();
                match *head {
                    #(#field_match_arms)*
                    #(#method_match_arms)*
                    _ => Err(#repe_path::StructError::InvalidPath {
                        path: #repe_path::structs::path_from_segments(segments),
                    }),
                }
            }
        }
    };

    Ok(expanded)
}

fn repe_crate_path() -> TokenStream2 {
    match crate_name("repe") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!(::#ident)
        }
        Err(_) => quote!(::repe),
    }
}

#[derive(Default)]
struct FieldAttrs {
    rename: Option<String>,
    skip: bool,
    nested: bool,
    readonly: bool,
}

struct FieldSpec {
    ident: Ident,
    ty: Type,
    attrs: FieldAttrs,
    endpoint: String,
}

fn parse_field(field: &Field) -> syn::Result<FieldSpec> {
    let ident = field
        .ident
        .clone()
        .ok_or_else(|| syn::Error::new(field.span(), "expected named field"))?;
    let mut attrs = FieldAttrs::default();

    for attr in &field.attrs {
        if !attr.path().is_ident("repe") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                attrs.skip = true;
                return Ok(());
            }
            if meta.path.is_ident("nested") {
                attrs.nested = true;
                return Ok(());
            }
            if meta.path.is_ident("readonly") {
                attrs.readonly = true;
                return Ok(());
            }
            if meta.path.is_ident("rename") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                attrs.rename = Some(lit.value());
                return Ok(());
            }
            Err(meta.error("unsupported `repe` field attribute"))
        })?;
    }

    let endpoint = attrs.rename.clone().unwrap_or_else(|| ident.to_string());

    Ok(FieldSpec {
        ident,
        ty: field.ty.clone(),
        attrs,
        endpoint,
    })
}

#[derive(Debug)]
enum ReceiverKind {
    Ref,
    Mut,
}

struct MethodSpec {
    endpoint: String,
    method_ident: Ident,
    arg: Option<(Ident, Type)>,
    ret: Type,
    signature_display: String,
}

fn parse_struct_attrs(attrs: &[Attribute]) -> syn::Result<StructAttrs> {
    let mut methods = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("repe") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("methods") {
                let content;
                syn::parenthesized!(content in meta.input);
                let list: Punctuated<MethodEntry, Token![,]> =
                    content.parse_terminated(MethodEntry::parse, Token![,])?;
                for entry in list {
                    methods.push(entry.spec);
                }
                Ok(())
            } else {
                Err(meta.error("unsupported `repe` attribute"))
            }
        })?;
    }
    Ok(StructAttrs { methods })
}

struct StructAttrs {
    methods: Vec<MethodSpec>,
}

struct MethodEntry {
    spec: MethodSpec,
}

impl Parse for MethodEntry {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let lookahead: Ident = input.parse()?;
        let (endpoint, method_ident) = if input.peek(Token![=]) {
            input.parse::<Token![=]>()?;
            let method: Ident = input.parse()?;
            (lookahead.to_string(), method)
        } else {
            let method = lookahead.clone();
            (lookahead.to_string(), method)
        };

        let content;
        syn::parenthesized!(content in input);
        let receiver = parse_receiver(&content)?;

        let arg = if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
            let arg_ident: Ident = content.parse()?;
            content.parse::<Token![:]>()?;
            let arg_ty: Type = content.parse()?;
            Some((arg_ident, arg_ty))
        } else {
            None
        };
        if !content.is_empty() {
            return Err(content.error("unexpected tokens in method parameter list"));
        }

        let ret = if input.peek(Token![->]) {
            input.parse::<Token![->]>()?;
            input.parse::<Type>()?
        } else {
            syn::parse_quote! { () }
        };

        let signature_display = build_signature_string(&receiver, arg.as_ref(), &ret);

        Ok(MethodEntry {
            spec: MethodSpec {
                endpoint,
                method_ident,
                arg,
                ret,
                signature_display,
            },
        })
    }
}

fn parse_receiver(input: &ParseBuffer<'_>) -> syn::Result<ReceiverKind> {
    input.parse::<Token![&]>()?;
    if input.peek(Token![mut]) {
        input.parse::<Token![mut]>()?;
        input.parse::<Token![self]>()?;
        Ok(ReceiverKind::Mut)
    } else {
        input.parse::<Token![self]>()?;
        Ok(ReceiverKind::Ref)
    }
}

fn build_signature_string(
    receiver: &ReceiverKind,
    arg: Option<&(Ident, Type)>,
    ret: &Type,
) -> String {
    let recv = match receiver {
        ReceiverKind::Ref => "&self",
        ReceiverKind::Mut => "&mut self",
    };
    let args = match arg {
        Some((name, ty)) => format!("{}, {}: {}", recv, name, normalize_type_string(ty)),
        None => recv.to_string(),
    };
    let ret_str = normalize_type_string(ret);
    let ret_display = if ret_str == "()" {
        "()".to_string()
    } else {
        ret_str
    };
    format!("fn({}) -> {}", args, ret_display)
}

fn normalize_type_string(ty: &Type) -> String {
    ty.to_token_stream()
        .to_string()
        .replace(" ,", ",")
        .replace(" :: ", "::")
        .replace(" < ", "<")
        .replace(" >", ">")
        .replace(" > >", ">>")
}

fn build_root_map_entries(
    fields: &[FieldSpec],
    methods: &[MethodSpec],
    repe_path: &TokenStream2,
) -> Vec<TokenStream2> {
    let mut entries = Vec::new();
    for field in fields {
        if field.attrs.skip {
            continue;
        }
        let endpoint_lit = LitStr::new(&field.endpoint, Span::call_site());
        let full_path = LitStr::new(&format!("/{}", field.endpoint), Span::call_site());
        let ident = &field.ident;
        if field.attrs.nested {
            let ty = &field.ty;
            entries.push(quote! {
                {
                    let nested = <#ty as #repe_path::RepeStruct>::repe_handle(&mut self.#ident, &[], None)
                        .map_err(|err| #repe_path::structs::prepend_path(err, #endpoint_lit))?;
                    map.insert(
                        String::from(#endpoint_lit),
                        nested.unwrap_or(::serde_json::Value::Null),
                    );
                }
            });
        } else {
            entries.push(quote! {
                {
                    let value = ::serde_json::to_value(&self.#ident)
                        .map_err(|source| #repe_path::StructError::Serialize {
                            path: String::from(#full_path),
                            source,
                        })?;
                    map.insert(String::from(#endpoint_lit), value);
                }
            });
        }
    }

    for method in methods {
        let endpoint_lit = LitStr::new(&method.endpoint, Span::call_site());
        let sig_lit = LitStr::new(&method.signature_display, Span::call_site());
        entries.push(quote! {
            map.insert(
                String::from(#endpoint_lit),
                ::serde_json::Value::String(String::from(#sig_lit)),
            );
        });
    }

    entries
}

fn build_field_match_arms(fields: &[FieldSpec], repe_path: &TokenStream2) -> Vec<TokenStream2> {
    let mut arms = Vec::new();
    for field in fields {
        if field.attrs.skip {
            continue;
        }
        let endpoint_lit = LitStr::new(&field.endpoint, Span::call_site());
        let ident = &field.ident;
        let full_path = LitStr::new(&format!("/{}", field.endpoint), Span::call_site());
        let readonly_guard = if field.attrs.readonly {
            quote! {
                return Err(#repe_path::StructError::BodyUnexpected {
                    path: #repe_path::structs::path_from_segments(segments),
                });
            }
        } else {
            quote! {}
        };

        if field.attrs.nested {
            let ty = &field.ty;
            arms.push(quote! {
                #endpoint_lit => {
                    if tail.is_empty() {
                        return match body {
                            None => {
                                let nested = <#ty as #repe_path::RepeStruct>::repe_handle(&mut self.#ident, &[], None)
                                    .map_err(|err| #repe_path::structs::prepend_path(err, #endpoint_lit))?;
                                Ok(Some(nested.unwrap_or(::serde_json::Value::Null)))
                            }
                            Some(value) => {
                                #readonly_guard
                                self.#ident = ::serde_json::from_value(value).map_err(|source| #repe_path::StructError::Deserialize {
                                    path: String::from(#full_path),
                                    source,
                                })?;
                                Ok(None)
                            }
                        };
                    } else {
                        return <#ty as #repe_path::RepeStruct>::repe_handle(&mut self.#ident, tail, body)
                            .map_err(|err| #repe_path::structs::prepend_path(err, #endpoint_lit));
                    }
                }
            });
        } else {
            arms.push(quote! {
                #endpoint_lit => {
                    if !tail.is_empty() {
                        return Err(#repe_path::StructError::InvalidSubpath {
                            path: #repe_path::structs::path_from_segments(segments),
                        });
                    }
                    return match body {
                        None => {
                            let value = ::serde_json::to_value(&self.#ident)
                                .map_err(|source| #repe_path::StructError::Serialize {
                                    path: String::from(#full_path),
                                    source,
                                })?;
                            Ok(Some(value))
                        }
                        Some(value) => {
                            #readonly_guard
                            self.#ident = ::serde_json::from_value(value)
                                .map_err(|source| #repe_path::StructError::Deserialize {
                                    path: String::from(#full_path),
                                    source,
                                })?;
                            Ok(None)
                        }
                    };
                }
            });
        }
    }
    arms
}

fn build_method_match_arms(methods: &[MethodSpec], repe_path: &TokenStream2) -> Vec<TokenStream2> {
    let mut arms = Vec::new();
    for method in methods {
        let endpoint_lit = LitStr::new(&method.endpoint, Span::call_site());
        let full_path = LitStr::new(&format!("/{}", method.endpoint), Span::call_site());
        let method_ident = &method.method_ident;
        let ret_ty = &method.ret;
        let returns_unit = matches!(ret_ty, Type::Tuple(tuple) if tuple.elems.is_empty());

        let result_handling = if returns_unit {
            quote! { Ok(None) }
        } else {
            quote! {
                {
                    let value = ::serde_json::to_value(result)
                        .map_err(|source| #repe_path::StructError::Serialize {
                            path: String::from(#full_path),
                            source,
                        })?;
                    Ok(Some(value))
                }
            }
        };

        let arm = if let Some((_arg_ident, arg_ty)) = &method.arg {
            let invocation = quote! { Self::#method_ident(self, arg) };
            quote! {
                #endpoint_lit => {
                    if !tail.is_empty() {
                        return Err(#repe_path::StructError::InvalidSubpath {
                            path: #repe_path::structs::path_from_segments(segments),
                        });
                    }
                    let value = match body {
                        Some(value) => value,
                        None => {
                            return Err(#repe_path::StructError::BodyExpected {
                                path: #repe_path::structs::path_from_segments(segments),
                            });
                        }
                    };
                    let arg: #arg_ty = ::serde_json::from_value(value).map_err(|source| #repe_path::StructError::Deserialize {
                        path: String::from(#full_path),
                        source,
                    })?;
                    let result = #invocation;
                    #result_handling
                }
            }
        } else {
            let invocation = quote! { Self::#method_ident(self) };
            quote! {
                #endpoint_lit => {
                    if !tail.is_empty() {
                        return Err(#repe_path::StructError::InvalidSubpath {
                            path: #repe_path::structs::path_from_segments(segments),
                        });
                    }
                    let result = #invocation;
                    let _ = &body;
                    #result_handling
                }
            }
        };

        arms.push(arm);
    }
    arms
}
