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
use quote::{ToTokens, format_ident, quote, quote_spanned};
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
    ///
    /// The encoding form is the write's own `StructResult` rather than
    /// `{ write(..)?; Ok(()) }`, which is the same thing spelled longer. That
    /// matters beyond brevity: it makes the expression usable where `?` is not,
    /// which is how the shared-borrow read path reuses these emitters verbatim
    /// under its `Option` return instead of keeping a second copy of them.
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
            Sink::Encode => quote! { out.write(#path, &#expr) },
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
            Sink::Encode => quote! { out.write_typed_slice(#path, &#expr[..]) },
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
        // Everything the *derive* publishes: fields, plus any method named in a
        // struct-level `#[repe(methods(..))]` list. Both win dispatch over the
        // impl block's table, so both have to be in the cross-macro check —
        // leaving the listed methods out is how a struct-list method and an
        // impl-block accessor of the same name once compiled clean and emitted
        // the key twice.
        let declared: Vec<LitStr> = endpoints
            .iter()
            .map(|(endpoint, _)| LitStr::new(endpoint, Span::call_site()))
            .collect();
        quote! {
            impl #repe::structs::MethodsDeclared for #struct_ident {}

            // The cross-macro half of the collision check: neither macro sees
            // the other's endpoint names, but the generated consts do.
            const _: () = #repe::structs::assert_no_endpoint_collision(
                &[#(#declared),*],
                <#struct_ident as #repe::structs::RepeMethods>::REPE_METHOD_SIGNATURES,
                <#struct_ident as #repe::structs::RepeMethods>::REPE_ACCESSOR_ENDPOINTS,
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

    let read_listing =
        build_read_listing(&field_specs, &struct_attrs.methods, from_impl_block, &repe);
    let read_field_arms = build_read_field_arms(&field_specs, &repe);
    let read_method_arms = build_read_method_arms(&struct_attrs.methods, &repe);
    let read_fallthrough = if from_impl_block {
        quote! {
            _ => <Self as #repe::structs::RepeMethods>::repe_call_read_into(self, segments, out),
        }
    } else {
        quote! {
            _ => Some(Err(#repe::StructError::InvalidPath {
                path: #repe::structs::path_from_segments(segments),
            })),
        }
    };
    bodies.push(quote! {
        fn repe_read_into(
            &self,
            segments: &[&str],
            out: &mut #repe::structs::ResponseBody<'_>,
        ) -> Option<#repe::structs::StructResult<()>> {
            if segments.is_empty() {
                return #read_listing;
            }

            let (head, tail) = segments.split_first().unwrap();
            match *head {
                #(#read_field_arms)*
                #(#read_method_arms)*
                #read_fallthrough
            }
        }
    });

    Ok(quote! {
        impl #repe::RepeStruct for #struct_ident {
            #(#bodies)*
        }

        #handshake
    })
}

fn repe_crate_path() -> TokenStream2 {
    match crate_name("repe") {
        // `::repe`, not `crate`, even when expanding inside repe itself.
        // `proc_macro_crate` reports `Itself` for every target that is not an
        // integration test (it detects those by `CARGO_TARGET_TMPDIR`), which
        // lumps repe's own examples and doc-tests in with its lib — and in those
        // `crate` is the example, not repe. `extern crate self as repe;` in
        // `lib.rs` makes this path resolve from inside the lib too, so one
        // spelling is correct for all four cases.
        Ok(FoundCrate::Itself) => quote!(::repe),
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
    /// `&self` or `&mut self`. Read by the shared-borrow path, which can only
    /// call the former.
    receiver: ReceiverKind,
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
            receiver,
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
        // A field-shaped endpoint is listed the way a field is: by its value.
        // The value has to come back through `repe_call` rather than from a
        // getter call emitted here, because this derive cannot see the impl
        // block and so does not know a single getter's name — the endpoint list
        // is all it has. That indirection costs a match scan per accessor, so a
        // whole-object read of a struct with many accessors is quadratic in the
        // endpoint count; the same read of an all-field struct is not.
        //
        // A getter that returns `Err` fails the whole listing, exactly as a
        // field whose `Serialize` impl fails does. That is the field analogy
        // held to consistently rather than an oversight, and it is why a getter
        // meant to be listed should report a sentinel rather than an error.
        entries.push(match sink {
            Sink::Value => quote! {
                for &name in <Self as #repe::structs::RepeMethods>::REPE_ACCESSOR_ENDPOINTS {
                    let value = <Self as #repe::structs::RepeMethods>::repe_call(self, &[name], None)?;
                    map.insert(
                        String::from(name),
                        value.unwrap_or(::serde_json::Value::Null),
                    );
                }
            },
            Sink::Encode => quote! {
                for &name in <Self as #repe::structs::RepeMethods>::REPE_ACCESSOR_ENDPOINTS {
                    __repe_obj.entry_with(name, |__repe_nested| {
                        <Self as #repe::structs::RepeMethods>::repe_call_into(
                            self, &[name], None, __repe_nested,
                        )
                    })?;
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

    let ok_is_unit = method.ret.ok_is_unit();
    let emit_ok = if ok_is_unit {
        sink.emit_null()
    } else {
        sink.emit_value(repe, &path, quote! { __repe_ok })
    };
    let call_and_emit = call_and_emit(
        quote! { Self::#method_ident(self #(, #bindings)*) },
        &method.ret,
        emit_ok,
        repe,
        &path,
    );

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

/// Call a published method and turn what it returned into a response.
///
/// `emit_ok` is the response for a success, written against a binding named
/// `__repe_ok` when the `Ok` type is not `()`. A declared `Result` turns `Err`
/// into a [`StructError::Execution`] naming `path`, rather than serializing the
/// `Result` itself.
///
/// Shared by the method arms and by both halves of an accessor pair, so a
/// getter that fails reports it exactly as a method that fails does.
fn call_and_emit(
    invocation: TokenStream2,
    ret: &ReturnSpec,
    emit_ok: TokenStream2,
    repe: &TokenStream2,
    path: &LitStr,
) -> TokenStream2 {
    if ret.fallible {
        let ok_pattern = if ret.ok_is_unit() {
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
    } else if ret.ok_is_unit() {
        // Braced so every branch is an *expression*, usable as a match arm body
        // as well as a statement tail.
        quote! {
            {
                #invocation;
                #emit_ok
            }
        }
    } else {
        quote! {
            {
                let __repe_ok = #invocation;
                #emit_ok
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Accessor arms
// ---------------------------------------------------------------------------

/// One dispatch arm for a field-shaped endpoint, shaped exactly like the arm
/// [`build_field_arms`] emits for a real field: no body reads, a body writes,
/// and a trailing path segment is a `InvalidSubpath` either way.
///
/// The only difference is where the value comes from — a getter call instead of
/// a field read, a setter call instead of an assignment — which is the whole
/// point of the attribute.
fn build_accessor_arm(accessor: &AccessorSpec, repe: &TokenStream2, sink: Sink) -> TokenStream2 {
    let key = LitStr::new(&accessor.endpoint, Span::call_site());
    let path = LitStr::new(&format!("/{}", accessor.endpoint), Span::call_site());

    let getter = &accessor.get.method_ident;
    let emit = if accessor.typed {
        sink.emit_typed_slice(repe, &path, quote! { __repe_ok })
    } else {
        sink.emit_value(repe, &path, quote! { __repe_ok })
    };
    let read = call_and_emit(
        quote! { Self::#getter(self) },
        &accessor.get.ret,
        emit,
        repe,
        &path,
    );

    // A getter with no setter *is* a read-only endpoint, so the refusal is the
    // same one `#[repe(readonly)]` produces on a field. Emitting only the
    // rejection — never a write followed by dead code — keeps generated code
    // clean under `#![deny(warnings)]`, as the field arms do.
    let write = match &accessor.set {
        None => quote! {
            Some(_) => Err(#repe::StructError::BodyUnexpected {
                path: #repe::structs::path_from_segments(segments),
            }),
        },
        Some(set) => {
            let setter = &set.method_ident;
            let ty = &set.args[0].1;
            let call = call_and_emit(
                quote! { Self::#setter(self, __repe_arg) },
                &set.ret,
                sink.emit_null(),
                repe,
                &path,
            );
            quote! {
                Some(value) => {
                    let __repe_arg: #ty = ::serde_json::from_value(value)
                        .map_err(|source| #repe::StructError::Deserialize {
                            path: String::from(#path),
                            source,
                        })?;
                    #call
                }
            }
        }
    };

    quote! {
        #key => {
            if !tail.is_empty() {
                return Err(#repe::StructError::InvalidSubpath {
                    path: #repe::structs::path_from_segments(segments),
                });
            }
            match body {
                None => #read,
                #write
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared-borrow read path
// ---------------------------------------------------------------------------
//
// `repe_read_into` and `repe_call_read_into` serve a bodiless request through
// `&self`, so a read does not queue behind a long-running call on the same
// object. They are built separately from the two `&mut self` bodies rather than
// as a third `Sink` pass, because the arm *shape* is genuinely different: there
// is no body, so no write branch, and an arm that cannot be served under a
// shared borrow declines instead of answering.
//
// The encoding is not duplicated. Every value below goes through the same
// `Sink::Encode` emitter the exclusive path uses, wrapped in `Some(..)`, so the
// two cannot disagree about how a value is serialized — only about whether this
// path serves it at all. `tests/shared_reads.rs` pins the rest by reading one
// struct through both and comparing frames.
//
// One rule governs what may run here: **a listing never invokes a published
// method.** A listing is the one read that composes many others, so a decline
// discovered partway through would leave the entries before it already
// executed, and the exclusive retry then executes them again — a `&self` getter
// over a read counter would report the second call. Getters are the only
// listing entries that are invoked rather than serialized, so a struct with any
// field-shaped endpoint declines its listing at the top, before writing or
// calling anything. What is left is serialization, which re-runs harmlessly.
// Reading one of those endpoints on its own is unaffected: that arm decides
// from the receiver, before it calls anything.

/// The refusal for a path segment below a leaf endpoint.
fn read_invalid_subpath(repe: &TokenStream2) -> TokenStream2 {
    quote! {
        Some(Err(#repe::StructError::InvalidSubpath {
            path: #repe::structs::path_from_segments(segments),
        }))
    }
}

/// The read arm shared by a published method and the getter half of a
/// field-shaped endpoint: both are a zero-argument call whose result is the
/// whole response, and both decline a `&mut self` receiver.
fn build_read_call_arm(
    endpoint: &str,
    receiver: ReceiverKind,
    invocation: TokenStream2,
    ret: &ReturnSpec,
    typed: bool,
    repe: &TokenStream2,
) -> TokenStream2 {
    let key = LitStr::new(endpoint, Span::call_site());
    if !matches!(receiver, ReceiverKind::Ref) {
        return quote! { #key => None, };
    }
    let path = LitStr::new(&format!("/{endpoint}"), Span::call_site());
    let emit_ok = if ret.ok_is_unit() {
        Sink::Encode.emit_null()
    } else if typed {
        Sink::Encode.emit_typed_slice(repe, &path, quote! { __repe_ok })
    } else {
        Sink::Encode.emit_value(repe, &path, quote! { __repe_ok })
    };
    let call = call_and_emit(invocation, ret, emit_ok, repe, &path);
    let subpath = read_invalid_subpath(repe);
    quote! {
        #key => {
            if !tail.is_empty() {
                return #subpath;
            }
            Some(#call)
        }
    }
}

/// One read arm per field: a leaf serializes itself, a nested field asks its
/// child, which may decline in turn.
fn build_read_field_arms(fields: &[FieldSpec], repe: &TokenStream2) -> Vec<TokenStream2> {
    let subpath = read_invalid_subpath(repe);
    let mut arms = Vec::new();
    for field in fields {
        if field.attrs.skip {
            continue;
        }
        let key = LitStr::new(&field.endpoint, Span::call_site());
        let ident = &field.ident;
        let path = LitStr::new(&format!("/{}", field.endpoint), Span::call_site());

        if field.attrs.nested {
            // `tail` covers both cases the exclusive path splits: empty reads
            // the child whole, non-empty descends. Only the write branch needed
            // them apart, and there is no write here.
            let ty = &field.ty;
            arms.push(quote! {
                #key => <#ty as #repe::RepeStruct>::repe_read_into(&self.#ident, tail, out)
                    .map(|__repe_result| {
                        __repe_result.map_err(|err| #repe::structs::prepend_path(err, #key))
                    }),
            });
        } else {
            let read = if field.attrs.typed {
                Sink::Encode.emit_typed_slice(repe, &path, quote! { self.#ident })
            } else {
                Sink::Encode.emit_value(repe, &path, quote! { self.#ident })
            };
            arms.push(quote! {
                #key => {
                    if !tail.is_empty() {
                        return #subpath;
                    }
                    Some(#read)
                }
            });
        }
    }
    arms
}

/// One read arm per published method, served when the signature allows it.
fn build_read_method_arms(methods: &[MethodSpec], repe: &TokenStream2) -> Vec<TokenStream2> {
    methods
        .iter()
        .map(|method| {
            // A method taking arguments reads them from a request body, and a
            // request carrying one is a write, so the shared path never has to
            // serve this. Declining costs nothing: the exclusive path answers
            // it with the same `BodyExpected` it always has.
            if !method.args.is_empty() {
                let key = LitStr::new(&method.endpoint, Span::call_site());
                return quote! { #key => None, };
            }
            let method_ident = &method.method_ident;
            // Spanned at the method name, because this is the one call in the
            // generated code whose receiver comes from a *declaration* rather
            // than from a signature: a `#[repe(methods(..))]` entry that says
            // `&self` for a `&mut self` method fails here, and the error has to
            // point at that entry rather than at `#[derive(RepeStruct)]`.
            let invocation = quote_spanned! { method_ident.span()=>
                Self::#method_ident(self)
            };
            build_read_call_arm(
                &method.endpoint,
                method.receiver,
                invocation,
                &method.ret,
                false,
                repe,
            )
        })
        .collect()
}

/// The read arm for a field-shaped endpoint: the getter, when it takes `&self`.
fn build_read_accessor_arm(accessor: &AccessorSpec, repe: &TokenStream2) -> TokenStream2 {
    let getter = &accessor.get.method_ident;
    build_read_call_arm(
        &accessor.endpoint,
        accessor.get.receiver,
        quote! { Self::#getter(self) },
        &accessor.get.ret,
        accessor.typed,
        repe,
    )
}

/// The whole-struct listing, read through a shared borrow.
///
/// Serialization only: see the rule at the top of this section. The one entry
/// that can still decline is a nested child, which declines before writing
/// anything of its own, and `ObjectBody::entry_try_with` rewinds the object it
/// was building — so a `None` from here leaves the response body exactly as it
/// was found, which is what `RepeStruct::repe_read_into` promises.
fn build_read_listing(
    fields: &[FieldSpec],
    methods: &[MethodSpec],
    from_impl_block: bool,
    repe: &TokenStream2,
) -> TokenStream2 {
    // A constant, so for a table with no field-shaped endpoints this folds away
    // and the listing is served shared as it stands.
    let guard = from_impl_block.then(|| {
        quote! {
            if !<Self as #repe::structs::RepeMethods>::REPE_ACCESSOR_ENDPOINTS.is_empty() {
                return None;
            }
        }
    });

    let mut entries = Vec::new();
    for field in fields {
        if field.attrs.skip {
            continue;
        }
        let key = LitStr::new(&field.endpoint, Span::call_site());
        let ident = &field.ident;
        if field.attrs.nested {
            let ty = &field.ty;
            entries.push(quote! {
                if let Err(__repe_err) = __repe_obj.entry_try_with(#key, |__repe_nested| {
                    <#ty as #repe::RepeStruct>::repe_read_into(&self.#ident, &[], __repe_nested)
                        .map(|__repe_result| {
                            __repe_result.map_err(|err| #repe::structs::prepend_path(err, #key))
                        })
                })? {
                    return Some(Err(__repe_err));
                }
            });
        } else {
            entries.push(quote! {
                if let Err(__repe_err) = __repe_obj.entry(#key, &self.#ident) {
                    return Some(Err(__repe_err));
                }
            });
        }
    }

    for method in methods {
        let key = LitStr::new(&method.endpoint, Span::call_site());
        let signature = LitStr::new(&method.signature_display, Span::call_site());
        entries.push(quote! {
            if let Err(__repe_err) = __repe_obj.entry(#key, &#signature) {
                return Some(Err(__repe_err));
            }
        });
    }

    if from_impl_block {
        entries.push(quote! {
            for &(name, signature) in <Self as #repe::structs::RepeMethods>::REPE_METHOD_SIGNATURES {
                if let Err(__repe_err) = __repe_obj.entry(name, &signature) {
                    return Some(Err(__repe_err));
                }
            }
        });
    }

    quote! {
        {
            #guard
            let mut __repe_obj = out.object();
            #(#entries)*
            __repe_obj.finish();
            Some(Ok(()))
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
        Ok(published) => methods_impl(&item_impl, &published, &repe),
        Err(err) => {
            // Emit the diagnostic *and* a table with nothing in it. Failing with
            // the error alone would drop the `RepeMethods` impl and add a second,
            // misleading "no `#[repe::methods]` impl block" error on top of the
            // real one.
            let error = err.to_compile_error();
            let empty = methods_impl(&item_impl, &PublishedItems::default(), &repe);
            quote! {
                #error
                #empty
            }
        }
    }
}

/// Which of the three published shapes one signature in a `#[repe::methods]`
/// block takes.
enum Published {
    /// An ordinary endpoint: the request body carries the arguments.
    Method,
    /// The read half of a field-shaped endpoint, carrying whether the getter
    /// asked for the BEVE typed-slice encoding.
    Get { typed: bool },
    /// The write half of a field-shaped endpoint.
    Set,
}

/// A **field-shaped endpoint** backed by a getter/setter pair.
///
/// The wire shape is a field's — a bodiless request reads it, a request with a
/// body writes it, and the whole-struct listing shows its value — while the
/// implementation is two methods. That is the one endpoint shape a struct
/// cannot express by naming a field: a value that is derived, unit-converted,
/// or backed by a register rather than by storage.
struct AccessorSpec {
    endpoint: String,
    /// The read half. Every accessor has one: without it the whole-struct
    /// listing would have no value to show for the endpoint.
    get: MethodSpec,
    /// `#[repe(typed)]` on the getter: encode the value as a BEVE typed numeric
    /// array, exactly as the field attribute of the same name does.
    typed: bool,
    /// The write half, absent for a read-only accessor — which is how a
    /// read-only field-shaped endpoint is spelled: there is no setter to call,
    /// so a write is refused the way `#[repe(readonly)]` refuses one.
    set: Option<MethodSpec>,
}

/// Everything one `#[repe::methods]` block publishes.
#[derive(Default)]
struct PublishedItems {
    methods: Vec<MethodSpec>,
    accessors: Vec<AccessorSpec>,
}

/// Reject two `#[repe(get = "x")]` methods, or two `#[repe(set = "x")]` methods,
/// claiming one endpoint.
///
/// Separate from [`reject_duplicate_endpoints`] only for the remedy: that one
/// recommends `#[repe(rename = "...")]`, which is itself a hard error on an
/// accessor half.
fn reject_duplicate_accessor_half<'a>(
    halves: &(impl Iterator<Item = &'a MethodSpec> + Clone),
    key: &str,
) -> syn::Result<()> {
    let mut seen: Vec<&str> = Vec::new();
    for spec in halves.clone() {
        if seen.contains(&spec.endpoint.as_str()) {
            return Err(syn::Error::new(
                spec.method_ident.span(),
                format!(
                    "two methods claim `#[repe({key} = \"{endpoint}\")]`; a field-shaped \
                     endpoint has one read half and one write half, so one of these could never \
                     be called. Give one of them a different endpoint.",
                    endpoint = spec.endpoint
                ),
            ));
        }
        seen.push(&spec.endpoint);
    }
    Ok(())
}

/// Validate every method in the block and collect what will be published.
fn collect_methods(
    item_impl: &ItemImpl,
    stripped: Vec<syn::Result<MethodAttrs>>,
) -> syn::Result<PublishedItems> {
    if let Some((_, path, _)) = &item_impl.trait_ {
        return Err(syn::Error::new_spanned(
            path,
            "`#[repe::methods]` applies to an inherent impl block, not a trait impl",
        ));
    }

    let mut methods: Vec<MethodSpec> = Vec::new();
    // Halves arrive in source order and are paired afterwards, so the two halves
    // of one endpoint need not sit next to each other.
    let mut getters: Vec<(MethodSpec, bool)> = Vec::new();
    let mut setters: Vec<MethodSpec> = Vec::new();
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
        let Some((published, spec)) = parse_impl_method(&func.sig, &func.attrs, attrs)? else {
            continue;
        };
        match published {
            Published::Method => methods.push(spec),
            Published::Get { typed } => getters.push((spec, typed)),
            Published::Set => setters.push(spec),
        }
    }

    // Each half is checked on its own, with its own message: the generic
    // duplicate-endpoint error recommends `#[repe(rename = "...")]`, which is a
    // hard error on an accessor half, so it would send the reader somewhere the
    // macro refuses to follow.
    reject_duplicate_accessor_half(&getters.iter().map(|(spec, _)| spec), "get")?;
    reject_duplicate_accessor_half(&setters.iter(), "set")?;

    let mut accessors: Vec<AccessorSpec> = Vec::new();
    for (get, typed) in getters {
        let set = setters
            .iter()
            .position(|spec| spec.endpoint == get.endpoint)
            .map(|index| setters.remove(index));
        accessors.push(AccessorSpec {
            endpoint: get.endpoint.clone(),
            get,
            typed,
            set,
        });
    }
    // A setter with no getter left over. Publishing it would give the endpoint a
    // value the listing cannot show and a client cannot read back, so the shape
    // is refused rather than half-served.
    if let Some(orphan) = setters.first() {
        return Err(syn::Error::new(
            orphan.method_ident.span(),
            format!(
                "`#[repe(set = \"{endpoint}\")]` has no matching \
                 `#[repe(get = \"{endpoint}\")]` in this block: a field-shaped endpoint that \
                 cannot be read has no value for the whole-struct listing to show. Add the read \
                 half, or publish this as an ordinary method.",
                endpoint = orphan.endpoint
            ),
        ));
    }

    let endpoints: Vec<(&str, Span)> = methods
        .iter()
        .map(|spec| (spec.endpoint.as_str(), spec.method_ident.span()))
        .chain(
            accessors
                .iter()
                .map(|spec| (spec.endpoint.as_str(), spec.get.method_ident.span())),
        )
        .collect();
    reject_duplicate_endpoints(&endpoints)?;
    Ok(PublishedItems { methods, accessors })
}

/// The impl block itself plus the `RepeMethods` table for `published`.
fn methods_impl(
    item_impl: &ItemImpl,
    published: &PublishedItems,
    repe: &TokenStream2,
) -> TokenStream2 {
    let self_ty = &item_impl.self_ty;
    let (impl_generics, _ty_generics, where_clause) = item_impl.generics.split_for_impl();

    let signatures = published.methods.iter().map(|spec| {
        let endpoint = LitStr::new(&spec.endpoint, Span::call_site());
        let signature = LitStr::new(&spec.signature_display, Span::call_site());
        quote! { (#endpoint, #signature) }
    });
    let accessor_endpoints = published
        .accessors
        .iter()
        .map(|spec| LitStr::new(&spec.endpoint, Span::call_site()));

    let mut bodies = Vec::new();
    for sink in [Sink::Value, Sink::Encode] {
        let arms = build_method_arms(&published.methods, repe, sink)
            .into_iter()
            .chain(
                published
                    .accessors
                    .iter()
                    .map(|accessor| build_accessor_arm(accessor, repe, sink)),
            )
            .collect::<Vec<_>>();
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

    let read_arms = build_read_method_arms(&published.methods, repe)
        .into_iter()
        .chain(
            published
                .accessors
                .iter()
                .map(|accessor| build_read_accessor_arm(accessor, repe)),
        )
        .collect::<Vec<_>>();
    let read_invalid_root = quote! {
        Some(Err(#repe::StructError::InvalidPath { path: String::from("") }))
    };
    let read_invalid_path = quote! {
        Some(Err(#repe::StructError::InvalidPath {
            path: #repe::structs::path_from_segments(segments),
        }))
    };
    bodies.push(quote! {
        fn repe_call_read_into(
            &self,
            segments: &[&str],
            out: &mut #repe::structs::ResponseBody<'_>,
        ) -> Option<#repe::structs::StructResult<()>> {
            let Some((head, tail)) = segments.split_first() else {
                return #read_invalid_root;
            };
            // Every arm may be a decline, in which case neither is read.
            let _ = (&out, tail);
            match *head {
                #(#read_arms)*
                _ => #read_invalid_path,
            }
        }
    });

    // No handshake assertion to emit: `MethodsDeclared` is a supertrait of
    // `RepeMethods`, so this impl cannot compile unless the struct declared
    // `#[repe(methods)]`.
    quote! {
        #item_impl

        impl #impl_generics #repe::structs::RepeMethods for #self_ty #where_clause {
            const REPE_METHOD_SIGNATURES: &'static [(&'static str, &'static str)] =
                &[#(#signatures),*];

            const REPE_ACCESSOR_ENDPOINTS: &'static [&'static str] =
                &[#(#accessor_endpoints),*];

            #(#bodies)*
        }
    }
}

#[derive(Default)]
struct MethodAttrs {
    rename: Option<String>,
    skip: bool,
    /// `#[repe(get = "endpoint")]`: this method is the read half of the
    /// field-shaped endpoint it names.
    get: Option<LitStr>,
    /// `#[repe(set = "endpoint")]`: this method is the write half.
    set: Option<LitStr>,
    /// `#[repe(typed)]` on a getter, the accessor twin of the field attribute.
    /// Carries its own span, which is the only one an error about it can point
    /// at when there is no `get`/`set` literal to blame.
    typed: Option<Span>,
}

impl MethodAttrs {
    /// The endpoint this method serves as an accessor half, if it is one.
    fn accessor_endpoint(&self) -> Option<&LitStr> {
        self.get.as_ref().or(self.set.as_ref())
    }
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
            if meta.path.is_ident("get") {
                parsed.get = Some(meta.value()?.parse()?);
                return Ok(());
            }
            if meta.path.is_ident("set") {
                parsed.set = Some(meta.value()?.parse()?);
                return Ok(());
            }
            if meta.path.is_ident("typed") {
                parsed.typed = Some(meta.path.span());
                return Ok(());
            }
            Err(meta.error("unsupported `repe` method attribute"))
        })?;
    }
    attrs.retain(|attr| !attr.path().is_ident("repe"));
    validate_method_attrs(&parsed)?;
    Ok(parsed)
}

/// Reject combinations of method attributes that contradict each other, before
/// any of them reaches code generation.
fn validate_method_attrs(attrs: &MethodAttrs) -> syn::Result<()> {
    if let (Some(get), Some(_)) = (&attrs.get, &attrs.set) {
        return Err(syn::Error::new_spanned(
            get,
            "a method is either the `get` half of a field-shaped endpoint or the `set` half, not \
             both: one takes no arguments and returns the value, the other takes the value and \
             returns nothing",
        ));
    }
    let Some(endpoint) = attrs.accessor_endpoint() else {
        if let Some(typed) = attrs.typed {
            return Err(syn::Error::new(
                typed,
                "`#[repe(typed)]` applies to the `get` half of a field-shaped endpoint; an \
                 ordinary method's return value is always encoded as JSON",
            ));
        }
        return Ok(());
    };
    if attrs.skip {
        return Err(syn::Error::new_spanned(
            endpoint,
            "`#[repe(skip)]` and `#[repe(get/set)]` contradict each other: one withholds the \
             method from the served surface, the other publishes it",
        ));
    }
    if attrs.rename.is_some() {
        return Err(syn::Error::new_spanned(
            endpoint,
            "`#[repe(rename = \"...\")]` has nothing to rename here: the endpoint of a \
             field-shaped accessor is the name given to `get`/`set`",
        ));
    }
    if attrs.typed.is_some() && attrs.set.is_some() {
        return Err(syn::Error::new_spanned(
            endpoint,
            "`#[repe(typed)]` describes how a value is *encoded* into a response, so it belongs \
             on the `get` half; a `set` decodes whatever the client sent",
        ));
    }
    let value = endpoint.value();
    if value.is_empty() {
        return Err(syn::Error::new_spanned(
            endpoint,
            "a field-shaped endpoint needs a name",
        ));
    }
    if value.contains('/') {
        return Err(syn::Error::new_spanned(
            endpoint,
            "a field-shaped endpoint is one path segment below the struct root, so it cannot \
             contain `/`",
        ));
    }
    Ok(())
}

/// Turn one signature from a `#[repe::methods]` block into a published method,
/// or `Ok(None)` for an associated function that is not one.
fn parse_impl_method(
    sig: &Signature,
    remaining_attrs: &[Attribute],
    attrs: MethodAttrs,
) -> syn::Result<Option<(Published, MethodSpec)>> {
    let Some(FnArg::Receiver(receiver)) = sig.inputs.first() else {
        // An associated function has no instance to dispatch on. `Self::new()`
        // and friends belong in the block; they are simply not endpoints.
        if let Some(endpoint) = attrs.accessor_endpoint() {
            return Err(syn::Error::new_spanned(
                endpoint,
                "a field-shaped endpoint is served by a method on the instance, so its halves \
                 need a `&self` or `&mut self` receiver",
            ));
        }
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

    let (published, endpoint) = match (&attrs.get, &attrs.set) {
        (Some(name), _) => {
            if !args.is_empty() {
                return Err(syn::Error::new_spanned(
                    &sig.inputs,
                    "the `get` half of a field-shaped endpoint takes no arguments: a client reads \
                     it the way it reads a field, with no request body to decode them from",
                ));
            }
            if ret.ok_is_unit() {
                return Err(syn::Error::new_spanned(
                    &sig.output,
                    "the `get` half of a field-shaped endpoint has to return the value; a read \
                     that yields nothing has nothing to publish",
                ));
            }
            (
                Published::Get {
                    typed: attrs.typed.is_some(),
                },
                name.value(),
            )
        }
        (None, Some(name)) => {
            if args.len() != 1 {
                return Err(syn::Error::new_spanned(
                    &sig.inputs,
                    "the `set` half of a field-shaped endpoint takes exactly one argument: the \
                     request body *is* the new value, as it is for a field write",
                ));
            }
            if !ret.ok_is_unit() {
                return Err(syn::Error::new_spanned(
                    &sig.output,
                    "the `set` half of a field-shaped endpoint returns nothing (or \
                     `Result<(), E>`): a field write is acknowledged with `null`, so a returned \
                     value would be silently dropped",
                ));
            }
            (Published::Set, name.value())
        }
        (None, None) => (
            Published::Method,
            attrs.rename.unwrap_or_else(|| sig.ident.to_string()),
        ),
    };

    Ok(Some((
        published,
        MethodSpec {
            endpoint,
            method_ident: sig.ident.clone(),
            receiver: kind,
            args,
            ret,
            signature_display,
        },
    )))
}

// ---------------------------------------------------------------------------
// `#[repe::plugin]` — the C-ABI plugin exports
// ---------------------------------------------------------------------------

/// Export a [`Router`] constructor as a REPE C-ABI plugin.
///
/// Applied to a zero-argument function returning a `Router`, this emits the five
/// symbols a REPE host resolves after `dlopen` — `repe_plugin_interface_version`,
/// `repe_plugin_info`, `repe_plugin_init`, `repe_plugin_shutdown`, and
/// `repe_plugin_call` — each delegating to a `repe::plugin::PluginRuntime` built
/// from your function. See `repe::plugin` for the ABI contract and the
/// deployment requirements that come with it.
///
/// ```ignore
/// // Cargo.toml: [lib] crate-type = ["cdylib"]
/// use repe::server::Router;
///
/// #[repe::plugin(root = "/calculator")]
/// fn build() -> Router {
///     Router::new().with_typed("/calculator/add", |(a, b): (i64, i64)| Ok(a + b))
/// }
/// ```
///
/// # Arguments
///
/// * `root` (required) — the RPC path prefix this plugin claims, reported to the
///   host as `repe_plugin_data::root_path`. Must be an absolute JSON Pointer
///   prefix: leading `/`, no trailing `/`.
/// * `name` — defaults to the crate's `CARGO_PKG_NAME`.
/// * `version` — defaults to the crate's `CARGO_PKG_VERSION`.
///
/// Both defaults exist so the plugin's identity is not restated beside the
/// manifest that already carries it, and so it cannot drift from it.
///
/// The annotated function is left in place and stays callable, which is what
/// lets the same router be exercised by ordinary in-process tests.
///
/// [`Router`]: repe::server::Router
#[proc_macro_attribute]
pub fn plugin(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut args = PluginArgs::default();
    let parser = syn::meta::parser(|meta| args.parse(meta));
    parse_macro_input!(attr with parser);

    let item_fn = parse_macro_input!(item as syn::ItemFn);
    match expand_plugin(args, item_fn) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[derive(Default)]
struct PluginArgs {
    name: Option<LitStr>,
    version: Option<LitStr>,
    root: Option<LitStr>,
}

impl PluginArgs {
    fn parse(&mut self, meta: ParseNestedMeta) -> syn::Result<()> {
        if meta.path.is_ident("name") {
            self.name = Some(meta.value()?.parse()?);
        } else if meta.path.is_ident("version") {
            self.version = Some(meta.value()?.parse()?);
        } else if meta.path.is_ident("root") {
            self.root = Some(meta.value()?.parse()?);
        } else {
            return Err(meta.error(
                "unknown `#[repe::plugin]` argument; expected `root`, `name`, or `version`",
            ));
        }
        Ok(())
    }
}

/// Reject a string that cannot survive the trip through a C `const char*`.
///
/// An interior nul truncates the string silently at the host, which surfaces as
/// a plugin that loads under a name nobody typed — worth catching here, where
/// the literal is in view.
fn reject_interior_nul(field: &str, literal: &LitStr) -> syn::Result<()> {
    if literal.value().contains('\0') {
        return Err(syn::Error::new_spanned(
            literal,
            format!(
                "`{field}` cannot contain a nul byte: it is handed to the host as a C string and \
                 would be truncated there"
            ),
        ));
    }
    Ok(())
}

fn expand_plugin(args: PluginArgs, item_fn: syn::ItemFn) -> syn::Result<TokenStream2> {
    let repe = repe_crate_path();

    let Some(root) = args.root else {
        return Err(syn::Error::new(
            Span::call_site(),
            "`#[repe::plugin]` needs a `root` path prefix, e.g. \
             `#[repe::plugin(root = \"/calculator\")]`",
        ));
    };
    reject_interior_nul("root", &root)?;
    let root_value = root.value();
    if !root_value.starts_with('/') {
        return Err(syn::Error::new_spanned(
            &root,
            "`root` must be an absolute JSON Pointer prefix, so it has to start with `/`",
        ));
    }
    if root_value.len() > 1 && root_value.ends_with('/') {
        return Err(syn::Error::new_spanned(
            &root,
            "`root` must not end with `/`: hosts prefix-match this against request paths, and the \
             trailing separator is not part of the prefix",
        ));
    }

    // Default identity to the manifest rather than restating it. `concat!`
    // expands `env!` eagerly, so both branches produce one `&'static str`
    // literal usable in a `static` initializer.
    let name = match &args.name {
        Some(name) => {
            reject_interior_nul("name", name)?;
            quote! { #name }
        }
        None => quote! { env!("CARGO_PKG_NAME") },
    };
    let version = match &args.version {
        Some(version) => {
            reject_interior_nul("version", version)?;
            quote! { #version }
        }
        None => quote! { env!("CARGO_PKG_VERSION") },
    };

    if !item_fn.sig.inputs.is_empty() {
        return Err(syn::Error::new_spanned(
            &item_fn.sig.inputs,
            "`#[repe::plugin]` applies to a function taking no arguments: the host calls it \
             through a C ABI that has nothing to pass",
        ));
    }
    if !item_fn.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item_fn.sig.generics,
            "`#[repe::plugin]` applies to a non-generic function: the exports are one fixed              set of symbols, so there is no parameter for the host to choose",
        ));
    }
    if item_fn.sig.asyncness.is_some() {
        return Err(syn::Error::new_spanned(
            &item_fn.sig,
            "`#[repe::plugin]` cannot be applied to an `async fn`: `repe_plugin_call` is \
             synchronous, with no runtime on the host side of the ABI to drive a future",
        ));
    }

    let build = &item_fn.sig.ident;

    Ok(quote! {
        #item_fn

        #[doc(hidden)]
        static __REPE_PLUGIN_INFO: #repe::plugin::RepePluginData = #repe::plugin::RepePluginData {
            name: concat!(#name, "\0").as_ptr() as *const ::core::ffi::c_char,
            version: concat!(#version, "\0").as_ptr() as *const ::core::ffi::c_char,
            root_path: concat!(#root, "\0").as_ptr() as *const ::core::ffi::c_char,
        };

        #[doc(hidden)]
        static __REPE_PLUGIN_RUNTIME: #repe::plugin::PluginRuntime =
            #repe::plugin::PluginRuntime::new(#build);

        /// `repe_plugin_interface_version`: the ABI version this plugin was
        /// built against, which the host checks before reading anything else.
        #[unsafe(no_mangle)]
        pub extern "C" fn repe_plugin_interface_version() -> ::core::primitive::u32 {
            #repe::plugin::REPE_PLUGIN_INTERFACE_VERSION
        }

        /// `repe_plugin_info`: this plugin's name, version, and claimed RPC path
        /// prefix. The pointee is a `static`, so it outlives every call.
        #[unsafe(no_mangle)]
        pub extern "C" fn repe_plugin_info() -> *const #repe::plugin::RepePluginData {
            &__REPE_PLUGIN_INFO
        }

        /// `repe_plugin_init`: build the router. Optional for the host to call —
        /// the first request builds it lazily if this never runs.
        #[unsafe(no_mangle)]
        pub extern "C" fn repe_plugin_init() -> #repe::plugin::RepeResult {
            __REPE_PLUGIN_RUNTIME.init()
        }

        /// `repe_plugin_shutdown`: refuse further requests.
        #[unsafe(no_mangle)]
        pub extern "C" fn repe_plugin_shutdown() {
            __REPE_PLUGIN_RUNTIME.shutdown()
        }

        /// `repe_plugin_call`: dispatch one REPE request frame.
        ///
        /// # Safety
        ///
        /// `request` must point to `request_size` readable bytes, or be null
        /// with a `request_size` of 0. The returned buffer borrows from this
        /// thread's response buffer and is invalidated by this thread's next
        /// call, so the host must copy before calling again.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn repe_plugin_call(
            request: *const ::core::ffi::c_char,
            request_size: ::core::primitive::u64,
        ) -> #repe::plugin::RepeBuffer {
            unsafe { __REPE_PLUGIN_RUNTIME.call(request, request_size) }
        }
    })
}
