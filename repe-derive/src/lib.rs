//! Procedural macros for the `repe` crate.
//!
//! Two entry points, both of which exist so the served RPC surface is derived
//! from the type rather than restated next to it:
//!
//! * [`macro@RepeStruct`] — a derive that reflects a struct's **fields** into
//!   JSON Pointer endpoints.
//! * [`macro@methods`] — an attribute on an inherent `impl` block that reflects
//!   its **methods**. A derive macro cannot see `impl` blocks, so this is a
//!   separate macro tied to the derive by a compile-time handshake.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{ToTokens, format_ident, quote};
use syn::{
    Attribute, DeriveInput, Field, FnArg, GenericArgument, Ident, ImplItem, ItemImpl, LitStr,
    PathArguments, ReturnType, Signature, Token, Type,
    meta::ParseNestedMeta,
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

/// Publish every method of an inherent `impl` block as an RPC endpoint.
///
/// See `repe::structs::RepeMethods` for the shape; the struct must carry the
/// `#[repe(methods)]` marker beside its `#[derive(RepeStruct)]` so the derived
/// router dispatches here.
#[proc_macro_attribute]
pub fn methods(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        let attr: TokenStream2 = attr.into();
        return syn::Error::new_spanned(attr, "`#[repe::methods]` takes no arguments")
            .to_compile_error()
            .into();
    }
    let item_impl = parse_macro_input!(item as ItemImpl);
    expand_methods(item_impl).into()
}

// ---------------------------------------------------------------------------
// Sink — the one place the two generated dispatch bodies differ
// ---------------------------------------------------------------------------

/// Which response representation a generated arm produces.
///
/// `RepeStruct` carries two dispatch methods that must agree: `repe_handle`
/// returns an owned `serde_json::Value`, `repe_handle_into` encodes straight
/// into the response body. Rather than write two code generators, every builder
/// below takes a `Sink` and emits the response through these four methods — so
/// the pair stays in lockstep by construction, and a reader can check the whole
/// difference between the two paths in one place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sink {
    /// `RepeStruct::repe_handle` / `RepeMethods::repe_call`.
    Value,
    /// `RepeStruct::repe_handle_into` / `RepeMethods::repe_call_into`.
    Encode,
}

impl Sink {
    /// Respond with nothing: the acknowledgement for a write, or a method
    /// returning `()`.
    fn emit_null(self) -> TokenStream2 {
        match self {
            Sink::Value => quote! { Ok(None) },
            Sink::Encode => quote! {
                {
                    out.write_null();
                    Ok(())
                }
            },
        }
    }

    /// Respond with `expr`, serialized. `path` is a string literal naming the
    /// endpoint, read only if serialization fails.
    fn emit_value(self, repe: &TokenStream2, path: &LitStr, expr: TokenStream2) -> TokenStream2 {
        match self {
            Sink::Value => quote! {
                {
                    let value = ::serde_json::to_value(&#expr)
                        .map_err(|source| #repe::StructError::Serialize {
                            path: String::from(#path),
                            source,
                        })?;
                    Ok(Some(value))
                }
            },
            Sink::Encode => quote! {
                {
                    out.write(#path, &#expr)?;
                    Ok(())
                }
            },
        }
    }

    /// Respond with `slice` as a BEVE typed numeric array.
    ///
    /// Only the encoding path can carry one; a `Value` has no representation for
    /// a typed body, so it falls back to the JSON array. That divergence is the
    /// documented cost of `#[repe(typed)]`.
    fn emit_typed_slice(
        self,
        repe: &TokenStream2,
        path: &LitStr,
        expr: TokenStream2,
    ) -> TokenStream2 {
        match self {
            Sink::Value => self.emit_value(repe, path, expr),
            Sink::Encode => quote! {
                {
                    out.write_typed_slice(#path, &#expr[..])?;
                    Ok(())
                }
            },
        }
    }

    /// The `RepeStruct` dispatch method this sink calls on a nested field.
    fn struct_method(self) -> Ident {
        match self {
            Sink::Value => format_ident!("repe_handle"),
            Sink::Encode => format_ident!("repe_handle_into"),
        }
    }

    /// The `RepeMethods` dispatch method this sink calls for a path that
    /// matched no field.
    fn methods_method(self) -> Ident {
        match self {
            Sink::Value => format_ident!("repe_call"),
            Sink::Encode => format_ident!("repe_call_into"),
        }
    }

    /// Append the response body to an argument list, which only the encoding
    /// path threads through.
    fn with_out(self, args: TokenStream2) -> TokenStream2 {
        match self {
            Sink::Value => args,
            Sink::Encode => quote! { #args, out },
        }
    }
}

// ---------------------------------------------------------------------------
// #[derive(RepeStruct)]
// ---------------------------------------------------------------------------

fn expand_repe_struct(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let repe = repe_crate_path();

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

    // An endpoint that two declarations claim is not a warning-level problem:
    // one of them silently becomes unreachable, and the listing emits a
    // duplicate JSON key. Reject every collision the macro can see.
    let mut endpoints: Vec<(&str, Span)> = Vec::new();
    for field in &field_specs {
        if !field.attrs.skip {
            endpoints.push((&field.endpoint, field.ident.span()));
        }
    }
    for method in &struct_attrs.methods {
        endpoints.push((&method.endpoint, method.method_ident.span()));
    }
    reject_duplicate_endpoints(&endpoints)?;

    let from_impl_block = struct_attrs.methods_from_impl_block;

    // The compile-time handshake, both directions. `MethodsDeclared` is a
    // supertrait of `RepeMethods`, so a `#[repe::methods]` block without this
    // marker cannot compile; and the fallthrough below names `RepeMethods`, so
    // this marker without a block cannot either.
    let handshake = from_impl_block.then(|| {
        let field_names = field_specs
            .iter()
            .filter(|field| !field.attrs.skip)
            .map(|field| LitStr::new(&field.endpoint, Span::call_site()));
        quote! {
            impl #repe::structs::MethodsDeclared for #struct_ident {}

            // The cross-macro half of the collision check: neither macro sees
            // the other's endpoint names, but the generated const does.
            const _: () = #repe::structs::assert_no_endpoint_collision(
                &[#(#field_names),*],
                <#struct_ident as #repe::structs::RepeMethods>::REPE_METHOD_SIGNATURES,
            );
        }
    });

    let mut bodies = Vec::new();
    for sink in [Sink::Value, Sink::Encode] {
        let listing = build_listing(
            &field_specs,
            &struct_attrs.methods,
            from_impl_block,
            &repe,
            sink,
        );
        let field_arms = build_field_arms(&field_specs, &repe, sink);
        let method_arms = build_method_arms(&struct_attrs.methods, &repe, sink);
        let fallthrough = if from_impl_block {
            let call = sink.methods_method();
            let args = sink.with_out(quote! { self, segments, body });
            quote! {
                _ => <Self as #repe::structs::RepeMethods>::#call(#args),
            }
        } else {
            quote! {
                _ => Err(#repe::StructError::InvalidPath {
                    path: #repe::structs::path_from_segments(segments),
                }),
            }
        };
        let root_write_ack = sink.emit_null();
        let (signature, ret) = match sink {
            Sink::Value => (
                quote! {
                    fn repe_handle(
                        &mut self,
                        segments: &[&str],
                        body: Option<::serde_json::Value>,
                    )
                },
                quote! { #repe::structs::StructResult<Option<::serde_json::Value>> },
            ),
            Sink::Encode => (
                quote! {
                    fn repe_handle_into(
                        &mut self,
                        segments: &[&str],
                        body: Option<::serde_json::Value>,
                        out: &mut #repe::structs::ResponseBody<'_>,
                    )
                },
                quote! { #repe::structs::StructResult<()> },
            ),
        };

        bodies.push(quote! {
            #signature -> #ret {
                if segments.is_empty() {
                    if let Some(value) = body {
                        *self = ::serde_json::from_value(value).map_err(|source| #repe::StructError::Deserialize {
                            path: String::from(""),
                            source,
                        })?;
                        return #root_write_ack;
                    }
                    return #listing;
                }

                let (head, tail) = segments.split_first().unwrap();
                match *head {
                    #(#field_arms)*
                    #(#method_arms)*
                    #fallthrough
                }
            }
        });
    }

    Ok(quote! {
        impl #repe::RepeStruct for #struct_ident {
            #(#bodies)*
        }

        #handshake
    })
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

/// Reject two declarations claiming the same endpoint, pointing at the second.
fn reject_duplicate_endpoints(endpoints: &[(&str, Span)]) -> syn::Result<()> {
    for (index, (name, _)) in endpoints.iter().enumerate() {
        if let Some((_, span)) = endpoints[..index].iter().find(|(prior, _)| prior == name) {
            let _ = span;
            return Err(syn::Error::new(
                endpoints[index].1,
                format!(
                    "endpoint `{name}` is declared twice on this struct; one of them would be \
                     unreachable and the whole-struct listing would emit the key twice. Rename \
                     one with `#[repe(rename = \"...\")]`."
                ),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FieldAttrs {
    rename: Option<String>,
    skip: bool,
    nested: bool,
    readonly: bool,
    typed: bool,
}

struct FieldSpec {
    ident: Ident,
    ty: Type,
    attrs: FieldAttrs,
    endpoint: String,
}

/// Parse the `skip` / `rename = "..."` pair shared by field and method
/// attributes. `Ok(false)` means the meta was not one of these, so the caller
/// should keep looking.
fn parse_shared_meta(
    meta: &ParseNestedMeta<'_>,
    rename: &mut Option<String>,
    skip: &mut bool,
) -> syn::Result<bool> {
    if meta.path.is_ident("skip") {
        *skip = true;
        return Ok(true);
    }
    if meta.path.is_ident("rename") {
        let lit: LitStr = meta.value()?.parse()?;
        *rename = Some(lit.value());
        return Ok(true);
    }
    Ok(false)
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
            if parse_shared_meta(&meta, &mut attrs.rename, &mut attrs.skip)? {
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
            if meta.path.is_ident("typed") {
                attrs.typed = true;
                return Ok(());
            }
            Err(meta.error("unsupported `repe` field attribute"))
        })?;
    }

    if attrs.typed && attrs.nested {
        return Err(syn::Error::new(
            field.span(),
            "`#[repe(typed)]` encodes a numeric slice as a BEVE typed array, which a nested struct is not",
        ));
    }

    let endpoint = attrs.rename.clone().unwrap_or_else(|| ident.to_string());

    Ok(FieldSpec {
        ident,
        ty: field.ty.clone(),
        attrs,
        endpoint,
    })
}

#[derive(Debug, Clone, Copy)]
enum ReceiverKind {
    Ref,
    Mut,
}

/// What a published method returns, and how that maps onto the wire.
struct ReturnSpec {
    /// The `Ok` payload type — `()` when there is nothing to send.
    ok_ty: Type,
    /// Whether the declared return type is a `Result`, in which case `Err`
    /// becomes an error frame rather than part of the payload.
    fallible: bool,
    /// The declared return type, verbatim, for the published signature string.
    display: String,
}

impl ReturnSpec {
    fn ok_is_unit(&self) -> bool {
        matches!(&self.ok_ty, Type::Tuple(tuple) if tuple.elems.is_empty())
    }
}

struct MethodSpec {
    endpoint: String,
    method_ident: Ident,
    args: Vec<(Ident, Type)>,
    ret: ReturnSpec,
    signature_display: String,
}

struct StructAttrs {
    methods: Vec<MethodSpec>,
    /// `#[repe(methods)]` with no list: the method table comes from a
    /// `#[repe::methods]` impl block.
    methods_from_impl_block: bool,
}

fn parse_struct_attrs(attrs: &[Attribute]) -> syn::Result<StructAttrs> {
    let mut methods = Vec::new();
    let mut methods_from_impl_block = false;
    for attr in attrs {
        if !attr.path().is_ident("repe") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("methods") {
                if !meta.input.peek(syn::token::Paren) {
                    // Bare `#[repe(methods)]`: defer to the `#[repe::methods]`
                    // impl block, which is the only place a full signature list
                    // can be read off the source of truth.
                    methods_from_impl_block = true;
                    return Ok(());
                }
                let content;
                syn::parenthesized!(content in meta.input);
                let list: Punctuated<MethodSpec, Token![,]> =
                    content.parse_terminated(MethodSpec::parse, Token![,])?;
                methods.extend(list);
                Ok(())
            } else {
                Err(meta.error("unsupported `repe` attribute"))
            }
        })?;
    }
    Ok(StructAttrs {
        methods,
        methods_from_impl_block,
    })
}

impl Parse for MethodSpec {
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

        let mut args = Vec::new();
        while content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
            if content.is_empty() {
                break;
            }
            let arg_ident: Ident = content.parse()?;
            content.parse::<Token![:]>()?;
            let arg_ty: Type = content.parse()?;
            args.push((arg_ident, arg_ty));
        }
        if !content.is_empty() {
            return Err(content.error("unexpected tokens in method parameter list"));
        }

        let ret_ty: Type = if input.peek(Token![->]) {
            input.parse::<Token![->]>()?;
            input.parse::<Type>()?
        } else {
            syn::parse_quote! { () }
        };
        let ret = classify_return(&ret_ty);
        let signature_display = build_signature_string(receiver, &args, &ret);

        Ok(MethodSpec {
            endpoint,
            method_ident,
            args,
            ret,
            signature_display,
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

/// Recognize a `Result` return so `Err` becomes an error frame.
///
/// The check is **name-based**: the macro sees a type, not a resolved one, so it
/// matches anything whose last path segment is `Result` with one or two type
/// arguments. That covers `Result<T, E>`, `std::result::Result<T, E>`, and the
/// widespread one-parameter aliases (`anyhow::Result<T>`, `std::io::Result<T>`,
/// a crate's own `pub type Result<T> = ...`).
///
/// It misses a `Result` aliased under another name (`type DeviceResult<T> = ...`),
/// which is then serialized as data — `Err` reaching the client as a *success*
/// frame carrying `{"Err": ...}`. It also misreads a type of your own that is
/// named `Result` but is not one. Both directions are documented; resolving them
/// would need type information a macro does not have.
fn classify_return(ty: &Type) -> ReturnSpec {
    let display = normalize_type_string(ty);
    if let Type::Path(type_path) = ty
        && type_path.qself.is_none()
        && let Some(segment) = type_path.path.segments.last()
        && segment.ident == "Result"
        && let PathArguments::AngleBracketed(generics) = &segment.arguments
    {
        let type_args: Vec<&Type> = generics
            .args
            .iter()
            .filter_map(|arg| match arg {
                GenericArgument::Type(ty) => Some(ty),
                _ => None,
            })
            .collect();
        if matches!(type_args.len(), 1 | 2) {
            return ReturnSpec {
                ok_ty: type_args[0].clone(),
                fallible: true,
                display,
            };
        }
    }
    ReturnSpec {
        ok_ty: ty.clone(),
        fallible: false,
        display,
    }
}

fn build_signature_string(
    receiver: ReceiverKind,
    args: &[(Ident, Type)],
    ret: &ReturnSpec,
) -> String {
    let recv = match receiver {
        ReceiverKind::Ref => "&self",
        ReceiverKind::Mut => "&mut self",
    };
    let mut params = String::from(recv);
    for (name, ty) in args {
        params.push_str(", ");
        params.push_str(&name.to_string());
        params.push_str(": ");
        params.push_str(&normalize_type_string(ty));
    }
    format!("fn({}) -> {}", params, ret.display)
}

fn normalize_type_string(ty: &Type) -> String {
    ty.to_token_stream()
        .to_string()
        .replace(" ,", ",")
        .replace(" :: ", "::")
        .replace(" < ", "<")
        .replace(" >", ">")
        .replace("& '", "&'")
}

// ---------------------------------------------------------------------------
// Whole-struct listing
// ---------------------------------------------------------------------------

/// The response to a read of the whole struct: every field, plus every method
/// published as its signature string.
///
/// Both sinks walk the same entries in declaration order — the `Value` form
/// through a `serde_json::Map`, the encoding form straight into the body.
fn build_listing(
    fields: &[FieldSpec],
    methods: &[MethodSpec],
    from_impl_block: bool,
    repe: &TokenStream2,
    sink: Sink,
) -> TokenStream2 {
    let mut entries = Vec::new();

    for field in fields {
        if field.attrs.skip {
            continue;
        }
        let key = LitStr::new(&field.endpoint, Span::call_site());
        let ident = &field.ident;
        if field.attrs.nested {
            let ty = &field.ty;
            let method = sink.struct_method();
            entries.push(match sink {
                Sink::Value => quote! {
                    {
                        let nested = <#ty as #repe::RepeStruct>::#method(&mut self.#ident, &[], None)
                            .map_err(|err| #repe::structs::prepend_path(err, #key))?;
                        map.insert(String::from(#key), nested.unwrap_or(::serde_json::Value::Null));
                    }
                },
                // The child writes into a *nested* body, so the frame stays
                // JSON no matter what the child would emit on its own.
                Sink::Encode => quote! {
                    __repe_obj.entry_with(#key, |__repe_nested| {
                        <#ty as #repe::RepeStruct>::#method(&mut self.#ident, &[], None, __repe_nested)
                            .map_err(|err| #repe::structs::prepend_path(err, #key))
                    })?;
                },
            });
        } else {
            let path = LitStr::new(&format!("/{}", field.endpoint), Span::call_site());
            entries.push(match sink {
                Sink::Value => quote! {
                    {
                        let value = ::serde_json::to_value(&self.#ident)
                            .map_err(|source| #repe::StructError::Serialize {
                                path: String::from(#path),
                                source,
                            })?;
                        map.insert(String::from(#key), value);
                    }
                },
                // A `#[repe(typed)]` field is a plain JSON array here: the frame
                // is already committed to JSON by the enclosing object, so the
                // typed body is reachable only by reading the field on its own.
                Sink::Encode => quote! { __repe_obj.entry(#key, &self.#ident)?; },
            });
        }
    }

    for method in methods {
        let key = LitStr::new(&method.endpoint, Span::call_site());
        let signature = LitStr::new(&method.signature_display, Span::call_site());
        entries.push(match sink {
            Sink::Value => quote! {
                map.insert(
                    String::from(#key),
                    ::serde_json::Value::String(String::from(#signature)),
                );
            },
            Sink::Encode => quote! { __repe_obj.entry(#key, &#signature)?; },
        });
    }

    if from_impl_block {
        entries.push(match sink {
            Sink::Value => quote! {
                for &(name, signature) in <Self as #repe::structs::RepeMethods>::REPE_METHOD_SIGNATURES {
                    map.insert(String::from(name), ::serde_json::Value::String(String::from(signature)));
                }
            },
            Sink::Encode => quote! {
                for &(name, signature) in <Self as #repe::structs::RepeMethods>::REPE_METHOD_SIGNATURES {
                    __repe_obj.entry(name, &signature)?;
                }
            },
        });
    }

    match sink {
        Sink::Value => quote! {
            {
                let mut map = ::serde_json::Map::new();
                #(#entries)*
                Ok(Some(::serde_json::Value::Object(map)))
            }
        },
        Sink::Encode => quote! {
            {
                let mut __repe_obj = out.object();
                #(#entries)*
                __repe_obj.finish();
                Ok(())
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Field arms
// ---------------------------------------------------------------------------

/// A call into a `#[repe(nested)]` field's own `RepeStruct` impl, with this
/// field's endpoint prefixed onto any error path it produces.
fn nested_dispatch(
    ty: &Type,
    ident: &Ident,
    key: LitStr,
    args: TokenStream2,
    repe: &TokenStream2,
    sink: Sink,
) -> TokenStream2 {
    let method = sink.struct_method();
    let args = sink.with_out(quote! { &mut self.#ident, #args });
    quote! {
        <#ty as #repe::RepeStruct>::#method(#args)
            .map_err(|err| #repe::structs::prepend_path(err, #key))
    }
}

fn build_field_arms(fields: &[FieldSpec], repe: &TokenStream2, sink: Sink) -> Vec<TokenStream2> {
    let mut arms = Vec::new();
    for field in fields {
        if field.attrs.skip {
            continue;
        }
        let key = LitStr::new(&field.endpoint, Span::call_site());
        let ident = &field.ident;
        let path = LitStr::new(&format!("/{}", field.endpoint), Span::call_site());

        // A readonly field emits *only* the rejection, never the write followed
        // by dead code, so a crate building under `#![deny(warnings)]` is not
        // broken by an `unreachable_code` lint on generated code.
        let write = if field.attrs.readonly {
            quote! {
                Some(_) => Err(#repe::StructError::BodyUnexpected {
                    path: #repe::structs::path_from_segments(segments),
                }),
            }
        } else {
            let ack = sink.emit_null();
            quote! {
                Some(value) => {
                    self.#ident = ::serde_json::from_value(value)
                        .map_err(|source| #repe::StructError::Deserialize {
                            path: String::from(#path),
                            source,
                        })?;
                    #ack
                }
            }
        };

        if field.attrs.nested {
            let descend = nested_dispatch(
                &field.ty,
                ident,
                key.clone(),
                quote! { tail, body },
                repe,
                sink,
            );
            let whole = nested_dispatch(
                &field.ty,
                ident,
                key.clone(),
                quote! { &[], None },
                repe,
                sink,
            );
            // Reading the child whole differs by one step: the `Value` form has
            // to lift the child's `Option` into this struct's, the encoding form
            // has already written it.
            let read = match sink {
                Sink::Value => quote! {
                    {
                        let nested = #whole?;
                        Ok(Some(nested.unwrap_or(::serde_json::Value::Null)))
                    }
                },
                Sink::Encode => whole,
            };
            arms.push(quote! {
                #key => {
                    if tail.is_empty() {
                        return match body {
                            None => #read,
                            #write
                        };
                    } else {
                        return #descend;
                    }
                }
            });
        } else {
            let read = if field.attrs.typed {
                sink.emit_typed_slice(repe, &path, quote! { self.#ident })
            } else {
                sink.emit_value(repe, &path, quote! { self.#ident })
            };
            arms.push(quote! {
                #key => {
                    if !tail.is_empty() {
                        return Err(#repe::StructError::InvalidSubpath {
                            path: #repe::structs::path_from_segments(segments),
                        });
                    }
                    return match body {
                        None => #read,
                        #write
                    };
                }
            });
        }
    }
    arms
}

// ---------------------------------------------------------------------------
// Method arms
// ---------------------------------------------------------------------------

fn build_method_arms(methods: &[MethodSpec], repe: &TokenStream2, sink: Sink) -> Vec<TokenStream2> {
    methods
        .iter()
        .map(|method| build_method_arm(method, repe, sink))
        .collect()
}

fn build_method_arm(method: &MethodSpec, repe: &TokenStream2, sink: Sink) -> TokenStream2 {
    let key = LitStr::new(&method.endpoint, Span::call_site());
    let path = LitStr::new(&format!("/{}", method.endpoint), Span::call_site());
    let method_ident = &method.method_ident;

    let bindings: Vec<Ident> = (0..method.args.len())
        .map(|i| format_ident!("__repe_arg{}", i))
        .collect();

    let take_body = quote! {
        let value = match body {
            Some(value) => value,
            None => {
                return Err(#repe::StructError::BodyExpected {
                    path: #repe::structs::path_from_segments(segments),
                });
            }
        };
    };

    // One argument *is* the body — the shape the wire has always had. Two or
    // more arrive as a positional array or a name-keyed object; see `MethodArgs`.
    let decode_args = match method.args.len() {
        0 => quote! { let _ = &body; },
        1 => {
            let binding = &bindings[0];
            let ty = &method.args[0].1;
            quote! {
                #take_body
                let #binding: #ty = ::serde_json::from_value(value).map_err(|source| #repe::StructError::Deserialize {
                    path: String::from(#path),
                    source,
                })?;
            }
        }
        _ => {
            let names: Vec<LitStr> = method
                .args
                .iter()
                .map(|(ident, _)| LitStr::new(&ident.to_string(), Span::call_site()))
                .collect();
            let decls = bindings
                .iter()
                .zip(method.args.iter())
                .map(|(binding, (_, ty))| quote! { let #binding: #ty = __repe_args.next_arg()?; });
            quote! {
                #take_body
                let mut __repe_args = #repe::structs::MethodArgs::new(#path, &[#(#names),*], value)?;
                #(#decls)*
            }
        }
    };

    let invocation = quote! { Self::#method_ident(self #(, #bindings)*) };
    let ok_is_unit = method.ret.ok_is_unit();
    let emit_ok = if ok_is_unit {
        sink.emit_null()
    } else {
        sink.emit_value(repe, &path, quote! { __repe_ok })
    };

    let call_and_emit = if method.ret.fallible {
        let ok_pattern = if ok_is_unit {
            quote! { Ok(_) }
        } else {
            quote! { Ok(__repe_ok) }
        };
        quote! {
            match #invocation {
                #ok_pattern => #emit_ok,
                Err(__repe_err) => Err(#repe::StructError::Execution {
                    path: String::from(#path),
                    message: ::std::string::ToString::to_string(&__repe_err),
                }),
            }
        }
    } else if ok_is_unit {
        quote! {
            #invocation;
            #emit_ok
        }
    } else {
        quote! {
            let __repe_ok = #invocation;
            #emit_ok
        }
    };

    quote! {
        #key => {
            if !tail.is_empty() {
                return Err(#repe::StructError::InvalidSubpath {
                    path: #repe::structs::path_from_segments(segments),
                });
            }
            #decode_args
            #call_and_emit
        }
    }
}

// ---------------------------------------------------------------------------
// #[repe::methods]
// ---------------------------------------------------------------------------

fn expand_methods(mut item_impl: ItemImpl) -> TokenStream2 {
    let repe = repe_crate_path();
    // Strip our attributes before anything can fail, so the impl block is
    // re-emittable either way — `repe` is not a registered attribute on a plain
    // inherent impl, and leaving one behind would bury the real error.
    let mut stripped = Vec::new();
    for item in &mut item_impl.items {
        if let ImplItem::Fn(func) = item {
            stripped.push(take_method_attrs(&mut func.attrs));
        }
    }

    match collect_methods(&item_impl, stripped) {
        Ok(specs) => methods_impl(&item_impl, &specs, &repe),
        Err(err) => {
            // Emit the diagnostic *and* a table with nothing in it. Failing with
            // the error alone would drop the `RepeMethods` impl and add a second,
            // misleading "no `#[repe::methods]` impl block" error on top of the
            // real one.
            let error = err.to_compile_error();
            let empty = methods_impl(&item_impl, &[], &repe);
            quote! {
                #error
                #empty
            }
        }
    }
}

/// Validate every method in the block and collect what will be published.
fn collect_methods(
    item_impl: &ItemImpl,
    stripped: Vec<syn::Result<MethodAttrs>>,
) -> syn::Result<Vec<MethodSpec>> {
    if let Some((_, path, _)) = &item_impl.trait_ {
        return Err(syn::Error::new_spanned(
            path,
            "`#[repe::methods]` applies to an inherent impl block, not a trait impl",
        ));
    }

    let mut specs: Vec<MethodSpec> = Vec::new();
    let mut stripped = stripped.into_iter();
    for item in &item_impl.items {
        let ImplItem::Fn(func) = item else {
            continue;
        };
        let attrs = stripped
            .next()
            .expect("one parsed attribute set per impl fn")?;
        if attrs.skip {
            continue;
        }
        if let Some(spec) = parse_impl_method(&func.sig, &func.attrs, attrs)? {
            specs.push(spec);
        }
    }

    let endpoints: Vec<(&str, Span)> = specs
        .iter()
        .map(|spec| (spec.endpoint.as_str(), spec.method_ident.span()))
        .collect();
    reject_duplicate_endpoints(&endpoints)?;
    Ok(specs)
}

/// The impl block itself plus the `RepeMethods` table for `specs`.
fn methods_impl(item_impl: &ItemImpl, specs: &[MethodSpec], repe: &TokenStream2) -> TokenStream2 {
    let self_ty = &item_impl.self_ty;
    let (impl_generics, _ty_generics, where_clause) = item_impl.generics.split_for_impl();

    let signatures = specs.iter().map(|spec| {
        let endpoint = LitStr::new(&spec.endpoint, Span::call_site());
        let signature = LitStr::new(&spec.signature_display, Span::call_site());
        quote! { (#endpoint, #signature) }
    });

    let mut bodies = Vec::new();
    for sink in [Sink::Value, Sink::Encode] {
        let arms = build_method_arms(specs, repe, sink);
        let (signature, ret, unused) = match sink {
            Sink::Value => (
                quote! {
                    fn repe_call(
                        &mut self,
                        segments: &[&str],
                        body: Option<::serde_json::Value>,
                    )
                },
                quote! { #repe::structs::StructResult<Option<::serde_json::Value>> },
                quote! { let _ = &body; },
            ),
            Sink::Encode => (
                quote! {
                    fn repe_call_into(
                        &mut self,
                        segments: &[&str],
                        body: Option<::serde_json::Value>,
                        out: &mut #repe::structs::ResponseBody<'_>,
                    )
                },
                quote! { #repe::structs::StructResult<()> },
                quote! { let _ = &body; let _ = &out; },
            ),
        };
        bodies.push(quote! {
            #signature -> #ret {
                let Some((head, tail)) = segments.split_first() else {
                    return Err(#repe::StructError::InvalidPath { path: String::from("") });
                };
                #unused
                match *head {
                    #(#arms)*
                    _ => Err(#repe::StructError::InvalidPath {
                        path: #repe::structs::path_from_segments(segments),
                    }),
                }
            }
        });
    }

    // No handshake assertion to emit: `MethodsDeclared` is a supertrait of
    // `RepeMethods`, so this impl cannot compile unless the struct declared
    // `#[repe(methods)]`.
    quote! {
        #item_impl

        impl #impl_generics #repe::structs::RepeMethods for #self_ty #where_clause {
            const REPE_METHOD_SIGNATURES: &'static [(&'static str, &'static str)] =
                &[#(#signatures),*];

            #(#bodies)*
        }
    }
}

#[derive(Default)]
struct MethodAttrs {
    rename: Option<String>,
    skip: bool,
}

/// Read the `#[repe(..)]` attributes off a method in a `#[repe::methods]` block
/// and strip them, since the inherent impl is re-emitted verbatim and `repe` is
/// not a registered attribute there.
fn take_method_attrs(attrs: &mut Vec<Attribute>) -> syn::Result<MethodAttrs> {
    let mut parsed = MethodAttrs::default();
    for attr in attrs.iter() {
        if !attr.path().is_ident("repe") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if parse_shared_meta(&meta, &mut parsed.rename, &mut parsed.skip)? {
                return Ok(());
            }
            Err(meta.error("unsupported `repe` method attribute"))
        })?;
    }
    attrs.retain(|attr| !attr.path().is_ident("repe"));
    Ok(parsed)
}

/// Turn one signature from a `#[repe::methods]` block into a published method,
/// or `Ok(None)` for an associated function that is not one.
fn parse_impl_method(
    sig: &Signature,
    remaining_attrs: &[Attribute],
    attrs: MethodAttrs,
) -> syn::Result<Option<MethodSpec>> {
    let Some(FnArg::Receiver(receiver)) = sig.inputs.first() else {
        // An associated function has no instance to dispatch on. `Self::new()`
        // and friends belong in the block; they are simply not endpoints.
        return Ok(None);
    };

    // Conditional compilation is applied *after* attribute macros run, so a
    // published `#[cfg]` method would be dispatched to whether or not it exists
    // — and the resulting error points at the user's own definition saying it
    // is missing. Refuse instead of generating that.
    if let Some(cfg) = remaining_attrs
        .iter()
        .find(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
    {
        return Err(syn::Error::new_spanned(
            cfg,
            "`#[repe::methods]` cannot publish a conditionally-compiled method: `#[cfg]` is \
             applied after this macro runs, so the endpoint would be generated even when the \
             method is not. Move it to a plain `impl` block, or add `#[repe(skip)]` and expose \
             an unconditional wrapper.",
        ));
    }

    if receiver.reference.is_none() {
        return Err(syn::Error::new_spanned(
            receiver,
            "`#[repe::methods]` needs a `&self` or `&mut self` receiver; a method that consumes `self` cannot be called on a served object (add `#[repe(skip)]` to exclude it)",
        ));
    }
    let kind = if receiver.mutability.is_some() {
        ReceiverKind::Mut
    } else {
        ReceiverKind::Ref
    };

    if sig.asyncness.is_some() {
        return Err(syn::Error::new_spanned(
            sig.asyncness,
            "`#[repe::methods]` cannot publish an async method; the struct router dispatches synchronously (add `#[repe(skip)]` to exclude it)",
        ));
    }
    if sig.unsafety.is_some() {
        return Err(syn::Error::new_spanned(
            sig.unsafety,
            "`#[repe::methods]` cannot publish an unsafe method (add `#[repe(skip)]` to exclude it)",
        ));
    }
    if let Some(variadic) = &sig.variadic {
        return Err(syn::Error::new_spanned(
            variadic,
            "`#[repe::methods]` cannot publish a variadic method (add `#[repe(skip)]` to exclude it)",
        ));
    }
    if sig
        .generics
        .params
        .iter()
        .any(|param| !matches!(param, syn::GenericParam::Lifetime(_)))
    {
        return Err(syn::Error::new_spanned(
            &sig.generics,
            "`#[repe::methods]` cannot publish a generic method; the dispatch table needs one concrete signature (add `#[repe(skip)]` to exclude it)",
        ));
    }

    // Names come from the signature where there is one to take. A pattern that
    // is not an identifier (`_`, a destructuring pattern) gets a positional
    // stand-in that cannot collide with a real parameter name, since the two
    // share one namespace in an object-form body.
    let mut args: Vec<(Ident, Type)> = Vec::new();
    for (index, input) in sig.inputs.iter().skip(1).enumerate() {
        let FnArg::Typed(pat_type) = input else {
            continue;
        };
        if matches!(&*pat_type.ty, Type::Reference(_)) {
            return Err(syn::Error::new_spanned(
                &pat_type.ty,
                "`#[repe::methods]` needs owned arguments, since each one is deserialized from the request body (use `String` rather than `&str`, or add `#[repe(skip)]` to exclude the method)",
            ));
        }
        if matches!(&*pat_type.ty, Type::ImplTrait(_)) {
            return Err(syn::Error::new_spanned(
                &pat_type.ty,
                "`#[repe::methods]` needs a nameable argument type, not `impl Trait` (add `#[repe(skip)]` to exclude the method)",
            ));
        }
        let name = match &*pat_type.pat {
            syn::Pat::Ident(pat_ident) => pat_ident.ident.clone(),
            _ => format_ident!("_{}", index),
        };
        args.push((name, (*pat_type.ty).clone()));
    }

    let ret_ty: Type = match &sig.output {
        ReturnType::Default => syn::parse_quote! { () },
        ReturnType::Type(_, ty) => (**ty).clone(),
    };
    if matches!(ret_ty, Type::ImplTrait(_)) {
        return Err(syn::Error::new_spanned(
            &sig.output,
            "`#[repe::methods]` needs a nameable return type, not `impl Trait` (add `#[repe(skip)]` to exclude the method)",
        ));
    }
    let ret = classify_return(&ret_ty);
    let signature_display = build_signature_string(kind, &args, &ret);
    let endpoint = attrs.rename.unwrap_or_else(|| sig.ident.to_string());

    Ok(Some(MethodSpec {
        endpoint,
        method_ident: sig.ident.clone(),
        args,
        ret,
        signature_display,
    }))
}
