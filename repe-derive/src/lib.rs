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
// Response emitters — how a generated arm answers
// ---------------------------------------------------------------------------
//
// These were methods on a `Sink` enum, because `RepeStruct` used to carry two
// dispatch methods that had to agree: `repe_handle` returned an owned
// `serde_json::Value` and `repe_handle_into` encoded straight into the response
// body, and generating both from one `Sink`-parameterised builder is what kept
// them in lockstep. There is one dispatch method now — the `Value` form went
// with `serde_json::Value` — so the parameter, and the whole class of bug it
// guarded against, is gone.

/// Respond with nothing: the acknowledgement for a write, or a method returning
/// `()`.
fn emit_null() -> TokenStream2 {
    quote! {
        {
            out.write_null();
            Ok(())
        }
    }
}

/// Respond with `expr`, serialized as JSON.
///
/// Deliberately an expression of type `StructResult<()>` rather than
/// `{ write(..)?; Ok(()) }`. That is what makes it usable where `?` is not,
/// which is how the shared-borrow read path reuses these emitters verbatim
/// under its `Option` return instead of keeping a second copy of them.
///
/// No error path and no endpoint literal: a `structio::json::Write` impl
/// returns `()`, so writing a response cannot fail.
fn emit_value(expr: TokenStream2) -> TokenStream2 {
    emit_value_spanned(Span::call_site(), expr)
}

/// [`emit_value`] with the span a bound failure should point at.
///
/// structio's bounds are what a user trips here — a field whose type has no
/// `object!` declaration — and the token worth underlining is the field, not the
/// derive that happened to visit it. Same reasoning as the `quote_spanned!` on
/// a method invocation below, with more force: a field's type is written down
/// right there.
fn emit_value_spanned(span: Span, expr: TokenStream2) -> TokenStream2 {
    quote_spanned! {span=>
        {
            out.write(&#expr);
            Ok(())
        }
    }
}

/// Respond with `expr` as a BEVE typed numeric array.
///
/// Inside an enclosing object the body falls back to a JSON array, which
/// `ResponseBody` decides for itself — see its `nested` flag.
fn emit_typed_slice(expr: TokenStream2) -> TokenStream2 {
    emit_typed_slice_spanned(Span::call_site(), expr)
}

/// [`emit_typed_slice`] with the span a bound failure should point at. See
/// [`emit_value_spanned`].
fn emit_typed_slice_spanned(span: Span, expr: TokenStream2) -> TokenStream2 {
    quote_spanned! {span=>
        {
            out.write_typed_slice(&#expr[..]);
            Ok(())
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

    if let Some(order) = &struct_attrs.listing_order {
        validate_listing_order(order, &endpoints, from_impl_block)?;
    }
    let entries = listing_entries(
        &field_specs,
        &struct_attrs.methods,
        from_impl_block,
        struct_attrs.listing_order.as_ref(),
    );

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
        // The other half of `validate_listing_order`. The derive rejects every
        // mistake it can name; the impl block's endpoints are not among them,
        // and the two generated tables are the only place they are visible.
        let order_check = struct_attrs.listing_order.as_ref().map(|order| {
            let keys = &order.keys;
            // A const-eval failure is reported at the span of the *call
            // expression*, which is the span of its leading token — and that
            // token is the crate path, emitted at call site. Without respanning
            // it, an order that omits an impl-block endpoint would point at
            // `#[derive(RepeStruct)]` rather than at the attribute that named
            // the wrong set.
            let assert = respan(quote! { #repe::structs::assert_listing_order }, order.span);
            quote_spanned! { order.span=>
                const _: () = #assert(
                    &[#(#keys),*],
                    &[#(#declared),*],
                    <#struct_ident as #repe::structs::RepeMethods>::REPE_METHOD_SIGNATURES,
                    <#struct_ident as #repe::structs::RepeMethods>::REPE_ACCESSOR_ENDPOINTS,
                    <#struct_ident as #repe::structs::RepeMethods>::REPE_TABLE_RECOVERED,
                );
            }
        });
        quote! {
            impl #repe::structs::MethodsDeclared for #struct_ident {}

            // The cross-macro half of the collision check: neither macro sees
            // the other's endpoint names, but the generated consts do.
            const _: () = #repe::structs::assert_no_endpoint_collision(
                &[#(#declared),*],
                <#struct_ident as #repe::structs::RepeMethods>::REPE_METHOD_SIGNATURES,
                <#struct_ident as #repe::structs::RepeMethods>::REPE_ACCESSOR_ENDPOINTS,
            );

            #order_check
        }
    });

    // The same terms the listing's guard is built from, published so a parent
    // that nests this struct can settle its listing before writing anything.
    // Empty means the constant `false`, which is a plain struct of leaf fields —
    // the common case, which then carries no guard at all.
    let decline_terms = listing_decline_terms(&field_specs, from_impl_block, &repe);
    let listing_declines = if decline_terms.is_empty() {
        quote! { false }
    } else {
        quote! { #(#decline_terms)||* }
    };

    // Whether the router should bother with the shared attempt for a frame that
    // carries a body. Empty means the constant `false`: every write here needs
    // the exclusive borrow, so the read lock, the walk and the decline are all
    // work whose outcome is known before it starts.
    let body_terms = shared_body_terms(&field_specs, &struct_attrs, from_impl_block, &repe);
    let shared_serves_bodies = if body_terms.is_empty() {
        quote! { false }
    } else {
        quote! { #(#body_terms)||* }
    };

    let mut bodies = Vec::new();
    {
        let listing = build_listing(&entries, &repe, Borrow::Exclusive);
        let field_arms = build_field_arms(&field_specs, &repe, Borrow::Exclusive);
        let method_arms = build_method_arms(&struct_attrs.methods, &repe, Borrow::Exclusive);
        let fallthrough = if from_impl_block {
            quote! {
                _ => <Self as #repe::structs::RepeMethods>::repe_call_into(
                    self, segments, body, out,
                ),
            }
        } else {
            quote! {
                _ => Err(#repe::StructError::InvalidPath {
                    path: #repe::structs::path_from_segments(segments),
                }),
            }
        };
        // The whole-object write. `#[repe(readonly)]` on the struct emits *only*
        // the refusal — never the assignment followed by dead code — which is
        // the point beyond the refusal itself: the read into `*self` is then
        // never generated, so the struct is not required to be readable at all.
        // A type holding live handles has no body that produces one.
        let root_write = if struct_attrs.no_replace {
            quote! {
                if body.is_some() {
                    return Err(#repe::StructError::BodyUnexpected {
                        path: String::from(""),
                    });
                }
            }
        } else {
            let root_write_ack = emit_null();
            quote! {
                if let Some(__repe_body) = body {
                    __repe_body.read_into("", self)?;
                    return #root_write_ack;
                }
            }
        };

        bodies.push(quote! {
            fn repe_handle_into(
                &mut self,
                segments: &[&str],
                body: Option<#repe::structs::RequestBody<'_>>,
                out: &mut #repe::structs::ResponseBody<'_>,
            ) -> #repe::structs::StructResult<()> {
                if segments.is_empty() {
                    #root_write
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

    // Settled before the shared listing writes or calls anything, and for the
    // whole subtree — see `listing_decline_terms`. Past it no entry can decline,
    // so the `entry_try_with` rewinds inside are the safety net for a
    // hand-written table that got the guard's inputs wrong, not a path a derived
    // listing takes. Every term folds to a constant, and a struct with none gets
    // no guard emitted.
    let shared_listing = {
        let listing = build_listing(&entries, &repe, Borrow::Shared);
        let guard = (!decline_terms.is_empty()).then(|| {
            quote! {
                if <Self as #repe::RepeStruct>::repe_listing_declines(self) {
                    return None;
                }
            }
        });
        quote! {
            {
                #guard
                #listing
            }
        }
    };
    let shared_field_arms = build_field_arms(&field_specs, &repe, Borrow::Shared);
    let shared_method_arms = build_method_arms(&struct_attrs.methods, &repe, Borrow::Shared);
    let shared_fallthrough = if from_impl_block {
        quote! {
            _ => <Self as #repe::structs::RepeMethods>::repe_call_shared_into(
                self, segments, body, out,
            ),
        }
    } else {
        quote! {
            _ => Some(Err(#repe::StructError::InvalidPath {
                path: #repe::structs::path_from_segments(segments),
            })),
        }
    };
    // A whole-object write needs `&mut self`, so it declines — unless the struct
    // is read-only, where the refusal itself is servable and there is no reason
    // to take the exclusive lock to give it.
    let shared_root_write = if struct_attrs.no_replace {
        quote! {
            if body.is_some() {
                return Some(Err(#repe::StructError::BodyUnexpected {
                    path: String::from(""),
                }));
            }
        }
    } else {
        quote! {
            if body.is_some() {
                return None;
            }
        }
    };
    bodies.push(quote! {
        fn repe_listing_declines(&self) -> bool {
            #listing_declines
        }
    });

    bodies.push(quote! {
        const REPE_SHARED_SERVES_BODIES: bool = #shared_serves_bodies;
    });

    bodies.push(quote! {
        fn repe_shared_into(
            &self,
            segments: &[&str],
            body: Option<#repe::structs::RequestBody<'_>>,
            out: &mut #repe::structs::ResponseBody<'_>,
        ) -> Option<#repe::structs::StructResult<()>> {
            if segments.is_empty() {
                #shared_root_write
                return #shared_listing;
            }

            let (head, tail) = segments.split_first().unwrap();
            match *head {
                #(#shared_field_arms)*
                #(#shared_method_arms)*
                #shared_fallthrough
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

/// Re-span every token of `tokens`, which here is always a path: a shallow walk
/// covers it because a path carries no delimited groups.
fn respan(tokens: TokenStream2, span: Span) -> TokenStream2 {
    tokens
        .into_iter()
        .map(|mut tree| {
            tree.set_span(span);
            tree
        })
        .collect()
}

/// The crate root the generated paths hang off: `::repe`, or `::repe_core` for a
/// crate that has only the core.
///
/// The `RepeStruct` surface lives in `repe-core` and is re-exported by `repe` at
/// the same paths, so either name is a valid root and one macro serves both. A
/// crate that has both wants the one it actually calls, which is why `repe` is
/// tried first.
///
/// Always an absolute path, never `crate`, even when expanding inside one of the
/// two crates. `proc_macro_crate` reports `Itself` for every target that is not
/// an integration test (it detects those by `CARGO_TARGET_TMPDIR`), which lumps
/// a crate's own examples and doc-tests in with its lib — and in those `crate`
/// is the example, not the library. An `extern crate self as ..;` in each
/// `lib.rs` makes the absolute path resolve from inside the lib too, so one
/// spelling is correct for all four cases.
fn repe_crate_path() -> TokenStream2 {
    for (package, itself) in [("repe", "repe"), ("repe-core", "repe_core")] {
        let name = match crate_name(package) {
            Ok(FoundCrate::Itself) => String::from(itself),
            Ok(FoundCrate::Name(name)) => name,
            // Not a dependency of the crate being expanded. Try the other.
            Err(_) => continue,
        };
        let ident = Ident::new(&name, Span::call_site());
        return quote!(::#ident);
    }
    // Neither resolved — no manifest to read, most likely. `repe` is the name
    // the overwhelming majority of callers depend on.
    quote!(::repe)
}

/// Reject two declarations claiming the same endpoint, pointing at the second.
fn reject_duplicate_endpoints(endpoints: &[(&str, Span)]) -> syn::Result<()> {
    for (index, (name, _)) in endpoints.iter().enumerate() {
        if endpoints[..index].iter().any(|(prior, _)| prior == name) {
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
            if meta.path.is_ident("nested_serde") {
                return Err(meta.error(
                    "`#[repe(nested_serde)]` is gone: it descended into a field by walking a \
                     `serde_json::Value` of it, and there is no tree to walk. Use \
                     `#[repe(nested)]` where the child can implement `RepeStruct`, and a \
                     structio adapter (`json::ReadAs` / `WriteAs`, named at the field as \
                     `field as Adapter`) for a type whose crate cannot depend on this one.",
                ));
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
    /// Struct-level `#[repe(readonly)]`: a write of the *whole* object is
    /// refused rather than deserialized into `Self`.
    ///
    /// The point is not only the refusal — it is that the whole-object read is
    /// then never emitted, so the struct is not required to be readable at all.
    /// A type holding an open socket or a file handle has no body that produces
    /// one, and declaring a reader that always errors trades a compile error for
    /// a runtime one and is boilerplate on every such type. It emits only the
    /// rejection, and never the assignment
    /// followed by dead code, because a crate under `#![deny(warnings)]` must
    /// not be broken by `unreachable_code` on generated code.
    ///
    /// Named for the operation it refuses rather than `readonly`, because the
    /// two are different statements: `readonly` on a field says *this subtree is
    /// not writable*, recursively, while this says *this type cannot be rebuilt
    /// from a body* and leaves every field writable. One word for both would
    /// read as correct and mean the wrong thing one level up, and it would
    /// spend the spelling that the recursive meaning should have if a struct
    /// ever wants it. `#[repe(readonly)]` on a struct is rejected outright,
    /// pointing here.
    no_replace: bool,
    /// `#[repe(listing_order("a", "b", ..))]`: the key order of the
    /// whole-object listing, given in full.
    ///
    /// Without it the listing appends in a fixed order — fields in declaration
    /// order, then struct-listed methods, then the impl block's signatures, then
    /// its field-shaped accessors — so a `#[repe(get/set)]` endpoint is always
    /// last however its logical place reads. That is wire-visible, and it is the
    /// one key order a `glz::object` with a `custom<setter, getter>` in the
    /// middle cannot be reproduced in.
    ///
    /// Reorders emission only: it costs nothing at run time, and nothing at all
    /// for a struct that does not use it.
    ///
    /// Governs both listings, which are the two that reach a client:
    /// `repe_handle_into` and `repe_shared_into`. There is no third form to fall
    /// out of step — the `serde_json::Map`-backed one, which sorted its keys and
    /// so could carry no order at all, went with `serde_json::Value`.
    listing_order: Option<ListingOrder>,
}

/// The parsed `#[repe(listing_order(..))]` list, with the span of the attribute
/// that carried it for the errors raised against it.
struct ListingOrder {
    keys: Vec<LitStr>,
    span: Span,
}

fn parse_struct_attrs(attrs: &[Attribute]) -> syn::Result<StructAttrs> {
    let mut methods = Vec::new();
    let mut methods_from_impl_block = false;
    let mut no_replace = false;
    let mut listing_order: Option<ListingOrder> = None;
    for attr in attrs {
        if !attr.path().is_ident("repe") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("no_replace") {
                no_replace = true;
                return Ok(());
            }
            if meta.path.is_ident("readonly") {
                // Deliberately an error rather than an alias. On a field
                // `readonly` is recursive — it refuses every write *through*
                // the field — and letting the same word mean "refuses only the
                // whole-object write" one level up is a trap that reads as
                // correct. The recursive meaning has no spelling on a struct
                // today; leaving the word unclaimed is what keeps it available
                // for one.
                return Err(meta.error(
                    "`#[repe(readonly)]` is a field attribute, and on a field it is recursive: \
                     it refuses every write through the field. A struct wanting that marks its \
                     fields. To refuse a write of the *whole* object while its fields stay \
                     writable — which also drops the requirement that `Self` be readable at \
                     all, for a type no body describes — use `#[repe(no_replace)]`.",
                ));
            }
            if meta.path.is_ident("listing_order") {
                let span = meta.path.span();
                if listing_order.is_some() {
                    return Err(meta.error(
                        "`#[repe(listing_order(..))]` is given once and names every key; two \
                         lists cannot both be the order",
                    ));
                }
                let content;
                syn::parenthesized!(content in meta.input);
                let keys: Punctuated<LitStr, Token![,]> =
                    content.parse_terminated(<LitStr as Parse>::parse, Token![,])?;
                listing_order = Some(ListingOrder {
                    keys: keys.into_iter().collect(),
                    span,
                });
                return Ok(());
            }
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
        no_replace,
        listing_order,
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

/// One key of the whole-object listing.
///
/// Resolved either from declaration order — fields, then struct-listed methods,
/// then the impl block's two tables — or from `#[repe(listing_order(..))]`,
/// which names the whole sequence. Both listings walk this one list, so they
/// cannot disagree about what is emitted or in what order.
enum ListingEntry<'a> {
    /// A field, listed by its value.
    Field(&'a FieldSpec),
    /// A struct-level `#[repe(methods(..))]` entry, listed by its signature
    /// string.
    Signature(&'a MethodSpec),
    /// One endpoint of the `#[repe::methods]` impl block, placed by name. Which
    /// of the block's two tables it came from is resolved in a `const` block,
    /// since this macro cannot see the block itself.
    ImplNamed(LitStr),
    /// Every `REPE_METHOD_SIGNATURES` entry, in table order. Emitted only when
    /// there is no `listing_order` placing them individually.
    ImplSignatures,
    /// Every `REPE_ACCESSOR_ENDPOINTS` entry, in table order. Same condition.
    ImplAccessors,
}

/// Resolve the listing's keys, in the order they will be emitted.
fn listing_entries<'a>(
    fields: &'a [FieldSpec],
    methods: &'a [MethodSpec],
    from_impl_block: bool,
    order: Option<&ListingOrder>,
) -> Vec<ListingEntry<'a>> {
    let Some(order) = order else {
        let mut entries: Vec<ListingEntry<'a>> = fields
            .iter()
            .filter(|field| !field.attrs.skip)
            .map(ListingEntry::Field)
            .collect();
        entries.extend(methods.iter().map(ListingEntry::Signature));
        if from_impl_block {
            entries.push(ListingEntry::ImplSignatures);
            entries.push(ListingEntry::ImplAccessors);
        }
        return entries;
    };

    // Every name here has been checked: against the fields and listed methods at
    // macro time, and against the impl block's tables by the `const` assertion
    // the handshake emits. So a name this macro does not recognize is an impl
    // block endpoint, not a typo.
    order
        .keys
        .iter()
        .map(|key| {
            let name = key.value();
            if let Some(field) = fields
                .iter()
                .find(|field| !field.attrs.skip && field.endpoint == name)
            {
                ListingEntry::Field(field)
            } else if let Some(method) = methods.iter().find(|method| method.endpoint == name) {
                ListingEntry::Signature(method)
            } else {
                ListingEntry::ImplNamed(key.clone())
            }
        })
        .collect()
}

/// Check a `#[repe(listing_order(..))]` list against everything this macro can
/// see: the fields, and the struct-level `#[repe(methods(..))]` entries.
///
/// A name the macro does not recognize is only rejected when there is no
/// `#[repe::methods]` block to have declared it — with one, the endpoints of
/// that block are the third source, and they are checked by the `const`
/// assertion the handshake emits. Everything reachable here is rejected here,
/// because only this side can name the offending key in the message.
fn validate_listing_order(
    order: &ListingOrder,
    declared: &[(&str, Span)],
    from_impl_block: bool,
) -> syn::Result<()> {
    let mut seen: Vec<String> = Vec::new();
    for key in &order.keys {
        let name = key.value();
        if seen.contains(&name) {
            return Err(syn::Error::new_spanned(
                key,
                format!(
                    "`{name}` is named twice in `#[repe(listing_order(..))]`; the listing emits \
                     each key once, so the second position could never be used"
                ),
            ));
        }
        if !from_impl_block && !declared.iter().any(|(endpoint, _)| *endpoint == name) {
            return Err(syn::Error::new_spanned(
                key,
                format!(
                    "`{name}` is not an endpoint on this struct, so the listing has nothing to \
                     emit for it"
                ),
            ));
        }
        seen.push(name);
    }

    for (endpoint, span) in declared {
        if !seen.iter().any(|name| name == endpoint) {
            return Err(syn::Error::new(
                *span,
                format!(
                    "`{endpoint}` is missing from `#[repe(listing_order(..))]`, which names the \
                     whole-object listing's keys in full. An omitted endpoint would disappear \
                     from every whole-object read while still answering on its own path."
                ),
            ));
        }
    }
    Ok(())
}

/// The `const` block that decides, at compile time, whether an ordered
/// impl-block key is a published method (listed by its signature) or a
/// field-shaped accessor (listed by its value).
fn impl_named_signature(name: &LitStr, repe: &TokenStream2) -> TokenStream2 {
    quote! {
        const {
            #repe::structs::listed_signature(
                <Self as #repe::structs::RepeMethods>::REPE_METHOD_SIGNATURES,
                #name,
            )
        }
    }
}

/// The `"/endpoint"` literal every arm names in its error paths.
fn endpoint_path(endpoint: &str) -> LitStr {
    LitStr::new(&format!("/{endpoint}"), Span::call_site())
}

/// Write one `"key": value` entry.
///
/// One form for both borrows: writing cannot fail, so there is no failure to
/// report differently.
fn listing_entry(key: TokenStream2, value: TokenStream2) -> TokenStream2 {
    quote! { __repe_obj.entry(#key, &#value); }
}

/// The response to a read of the whole struct: every field, plus every method
/// published as its signature string.
///
/// Both listings — the exclusive one and the shared one — are this one walk of
/// one resolved entry list, so they cannot disagree about *what* is emitted or
/// in what order.
///
/// Key order used to be the one thing they could disagree about, because a third
/// form assembled a `serde_json::Map` and sorted its keys. That form is gone
/// with `serde_json::Value`, so declaration order and
/// `#[repe(listing_order(..))]` now reach every frame a client sees.
fn build_listing(
    entries: &[ListingEntry<'_>],
    repe: &TokenStream2,
    borrow: Borrow,
) -> TokenStream2 {
    let mut emitted = Vec::new();
    // The object's member count, which BEVE needs in the header before any
    // member. Not a literal: the two `Impl*` entries list a `const` slice whose
    // length the deriving crate's own impl block settles. It is still a const
    // expression, so this folds to a constant at the call site.
    let mut fixed = 0usize;
    let mut variable: Vec<TokenStream2> = Vec::new();

    for entry in entries {
        match entry {
            ListingEntry::ImplSignatures => variable.push(quote! {
                <Self as #repe::structs::RepeMethods>::REPE_METHOD_SIGNATURES.len()
            }),
            ListingEntry::ImplAccessors => variable.push(quote! {
                <Self as #repe::structs::RepeMethods>::REPE_ACCESSOR_ENDPOINTS.len()
            }),
            _ => fixed += 1,
        }
        emitted.push(match entry {
            ListingEntry::Field(field) => {
                let key = LitStr::new(&field.endpoint, Span::call_site());
                let ident = &field.ident;
                if field.attrs.nested {
                    // The child writes into a *nested* body, so the frame stays
                    // JSON no matter what the child would emit on its own.
                    let call = borrow.child_call(
                        repe,
                        &field.ty,
                        ident,
                        quote! { &[], None },
                        quote! { __repe_nested },
                    );
                    borrow.entry_with(
                        quote! { #key },
                        borrow.prepend(repe, call, quote! { #key }),
                    )
                } else {
                    // A `#[repe(typed)]` field is a plain JSON array here,
                    // because the enclosing object has already committed the
                    // frame to JSON.
                    listing_entry(quote! { #key }, quote! { self.#ident })
                }
            }
            ListingEntry::Signature(method) => {
                let key = LitStr::new(&method.endpoint, Span::call_site());
                let signature = LitStr::new(&method.signature_display, Span::call_site());
                listing_entry(quote! { #key }, quote! { #signature })
            }
            ListingEntry::ImplNamed(name) => {
                let resolved = impl_named_signature(name, repe);
                let accessor =
                    borrow.entry_with(quote! { #name }, borrow.accessor_call(repe, quote! { #name }));
                let signature = listing_entry(quote! { #name }, quote! { __repe_signature });
                quote! {
                    match #resolved {
                        ::core::option::Option::Some(__repe_signature) => { #signature }
                        ::core::option::Option::None => { #accessor }
                    }
                }
            }
            ListingEntry::ImplSignatures => {
                let entry = listing_entry(quote! { name }, quote! { signature });
                quote! {
                    for &(name, signature) in <Self as #repe::structs::RepeMethods>::REPE_METHOD_SIGNATURES {
                        #entry
                    }
                }
            }
            // A field-shaped endpoint is listed the way a field is: by its
            // value. The value has to come back through the method table rather
            // than from a getter call emitted here, because this derive cannot
            // see the impl block and so does not know a single getter's name —
            // the endpoint list is all it has. That indirection costs a match
            // scan per accessor, so a whole-object read of a struct with many
            // accessors is quadratic in the endpoint count; the same read of an
            // all-field struct is not.
            //
            // A getter that returns `Err` fails the whole listing. Writing a
            // field cannot fail any more, so a getter is the only entry that can
            // — which is why one meant to be listed should report a sentinel
            // rather than an error.
            ListingEntry::ImplAccessors => {
                let accessor =
                    borrow.entry_with(quote! { name }, borrow.accessor_call(repe, quote! { name }));
                quote! {
                    for &name in <Self as #repe::structs::RepeMethods>::REPE_ACCESSOR_ENDPOINTS {
                        #accessor
                    }
                }
            }
        });
    }

    let count = quote! { #fixed #( + #variable)* };
    borrow.wrap(&emitted, count)
}

// ---------------------------------------------------------------------------
// Field arms
// ---------------------------------------------------------------------------

/// One dispatch arm per field, under either borrow.
///
/// A leaf serializes itself; a nested field forwards to its child with this
/// field's endpoint prefixed onto any error path it produces; a write lands in
/// the live field, is refused outright when the field is `#[repe(readonly)]`, or
/// declines under a shared borrow because it needs `&mut self`.
///
/// One builder for both borrows. The three places they differ — how an outcome
/// is shaped, which method a child is entered through, and what a write that
/// needs exclusive access does — are exactly what [`Borrow`] carries, so there
/// is nothing left for a second builder to hold. Field arms and *listing* arms
/// are still built separately, because there the shapes genuinely diverge: a
/// listing entry cannot decline past the guard at its top, and it rewinds.
fn build_field_arms(
    fields: &[FieldSpec],
    repe: &TokenStream2,
    borrow: Borrow,
) -> Vec<TokenStream2> {
    let refuse = refuse_write(repe, borrow);
    let subpath = refuse_subpath(repe, borrow);
    let ack = borrow.ok(emit_null());
    let mut arms = Vec::new();
    for field in fields {
        if field.attrs.skip {
            continue;
        }
        let key = LitStr::new(&field.endpoint, Span::call_site());
        let ident = &field.ident;
        let path = endpoint_path(&field.endpoint);

        if field.attrs.nested {
            // `readonly` on a nested field refuses every write *through* it,
            // subpaths included: the attribute says the field cannot be written,
            // and a write below it mutates the field just as surely as a write
            // at it. Past this guard `body` is `None`, so the descent below is
            // the same code either way.
            let guard = field.attrs.readonly.then(|| {
                quote! {
                    if body.is_some() {
                        return #refuse;
                    }
                }
            });

            // The whole-child write descends, exactly as the whole-child read
            // already did. Before this, a write with an empty tail replaced the
            // child, and so was the one path on which a child's own
            // `RepeStruct` impl was never consulted. That is precisely the path
            // where a child backed by a resource has something to say: applying
            // a partial object to live state is not the same operation as
            // replacing a struct.
            //
            // A derived child's empty-segments arm reads the body into `*self`,
            // which *merges* — a key the body omits keeps the value it had —
            // where the replacement it succeeds blanked them. That difference is
            // wire-visible on a whole-child write, and it is the behaviour a
            // partial update wants.
            //
            // `tail` *is* the empty slice when it is empty, so there is one call
            // here rather than a whole-child branch beside a descent.
            let forward = borrow.prepend(
                repe,
                borrow.child_call(
                    repe,
                    &field.ty,
                    ident,
                    quote! { tail, body },
                    quote! { out },
                ),
                quote! { #key },
            );
            arms.push(quote! { #key => { #guard #forward } });
        } else {
            let span = field.ty.span();
            let read = borrow.ok(if field.attrs.typed {
                emit_typed_slice_spanned(span, quote! { self.#ident })
            } else {
                emit_value_spanned(span, quote! { self.#ident })
            });
            let write = match (field.attrs.readonly, borrow) {
                (true, _) => quote! { Some(_) => #refuse, },
                (false, Borrow::Exclusive) => quote_spanned! {span=>
                    Some(__repe_body) => {
                        // Glaze's `read_params`: the bytes land in the live
                        // field, so a `Vec` or `String` already there keeps its
                        // allocation and a key the body omits keeps its value.
                        __repe_body.read_into(#path, &mut self.#ident)?;
                        #ack
                    }
                },
                // A write needs `&mut self`, so the shared borrow declines and
                // the caller retries under the exclusive one.
                (false, Borrow::Shared) => quote! { Some(_) => None, },
            };
            arms.push(quote! {
                #key => {
                    if !tail.is_empty() {
                        return #subpath;
                    }
                    match body {
                        None => #read,
                        #write
                    }
                }
            });
        }
    }
    arms
}

// ---------------------------------------------------------------------------
// Method arms
// ---------------------------------------------------------------------------

/// Which borrow of `self` a generated body has, and so what it returns.
///
/// One axis, and every generator in this file turns on it. Under `&mut self` a
/// body returns `StructResult<..>`: a failure is `Err`, an answer is bare, and
/// a `#[repe(nested)]` child is entered through `repe_handle_into`. Under
/// `&self` it returns `Option<StructResult<()>>`, because it may decline a path
/// that needs exclusive access, so every outcome is wrapped and a child is
/// entered through `repe_shared_into`.
///
/// This used to be two enums — one for the return shape, one for the listing
/// form — chosen together at every call site, which is what said they were the
/// same distinction. Carrying it as one variant rather than one generator per
/// borrow is what makes "the two agree" structural: there is one walk of one
/// entry list, so a `#[repe(listing_order(..))]` cannot reach one and not the
/// other.
#[derive(Clone, Copy)]
enum Borrow {
    /// `&mut self`, returning `StructResult<..>`.
    Exclusive,
    /// `&self`, returning `Option<StructResult<()>>`, `None` being a decline.
    Shared,
}

impl Borrow {
    /// `err` as an early return.
    fn err(self, err: TokenStream2) -> TokenStream2 {
        let refusal = self.refuse(err);
        quote! { return #refusal; }
    }

    /// `err` as an expression in this shape — an answer, not a decline.
    fn refuse(self, err: TokenStream2) -> TokenStream2 {
        match self {
            Borrow::Exclusive => quote! { Err(#err) },
            Borrow::Shared => quote! { Some(Err(#err)) },
        }
    }

    /// A successful `expr` in this shape.
    fn ok(self, expr: TokenStream2) -> TokenStream2 {
        match self {
            Borrow::Exclusive => expr,
            Borrow::Shared => quote! { Some(#expr) },
        }
    }

    /// The `RepeStruct` method a `#[repe(nested)]` child is entered through,
    /// under this borrow.
    ///
    /// `args` is the request being forwarded and `out` the body it writes into:
    /// `&[], None` and the enclosing object's nested body when the child is
    /// being *listed*, `tail, body` and `out` when a request is being dispatched
    /// *into* it.
    fn child_call(
        self,
        repe: &TokenStream2,
        ty: &Type,
        ident: &Ident,
        args: TokenStream2,
        out: TokenStream2,
    ) -> TokenStream2 {
        match self {
            Borrow::Exclusive => quote! {
                <#ty as #repe::RepeStruct>::repe_handle_into(
                    &mut self.#ident, #args, #out,
                )
            },
            Borrow::Shared => quote! {
                <#ty as #repe::RepeStruct>::repe_shared_into(
                    &self.#ident, #args, #out,
                )
            },
        }
    }

    /// Prefix a child's endpoint onto any error path it produced. The shared
    /// form has an `Option` in the way, since a child may decline.
    fn prepend(self, repe: &TokenStream2, call: TokenStream2, key: TokenStream2) -> TokenStream2 {
        match self {
            Borrow::Exclusive => quote! {
                #call.map_err(|err| #repe::structs::prepend_path(err, #key))
            },
            Borrow::Shared => quote! {
                #call.map(|__repe_result| {
                    __repe_result.map_err(|err| #repe::structs::prepend_path(err, #key))
                })
            },
        }
    }

    /// The `RepeMethods` call that reads a field-shaped endpoint's value back.
    fn accessor_call(self, repe: &TokenStream2, name: TokenStream2) -> TokenStream2 {
        match self {
            Borrow::Exclusive => quote! {
                <Self as #repe::structs::RepeMethods>::repe_call_into(
                    self, &[#name], None, __repe_nested,
                )
            },
            Borrow::Shared => quote! {
                <Self as #repe::structs::RepeMethods>::repe_call_shared_into(
                    self, &[#name], None, __repe_nested,
                )
            },
        }
    }

    /// Write one `"key": <value>` entry whose value the callee produces.
    ///
    /// The shared form uses `entry_try_with`, which rewinds the whole object on
    /// a decline and propagates the `None` out of `repe_shared_into` — the
    /// safety net for a hand-written table whose `REPE_LISTING_NEEDS_EXCLUSIVE`
    /// disagrees with its `repe_call_shared_into`, not a path a derived listing
    /// takes.
    fn entry_with(self, key: TokenStream2, produce: TokenStream2) -> TokenStream2 {
        match self {
            Borrow::Exclusive => quote! {
                __repe_obj.entry_with(#key, |__repe_nested| { #produce })?;
            },
            Borrow::Shared => quote! {
                if let Err(__repe_err) = __repe_obj.entry_try_with(#key, |__repe_nested| {
                    #produce
                })? {
                    return Some(Err(__repe_err));
                }
            },
        }
    }

    /// Open the body, run `entries`, and close it.
    fn wrap(self, entries: &[TokenStream2], count: TokenStream2) -> TokenStream2 {
        match self {
            Borrow::Exclusive => quote! {
                {
                    let mut __repe_obj = out.object(#count);
                    #(#entries)*
                    __repe_obj.finish();
                    Ok(())
                }
            },
            Borrow::Shared => quote! {
                {
                    let mut __repe_obj = out.object(#count);
                    #(#entries)*
                    __repe_obj.finish();
                    Some(Ok(()))
                }
            },
        }
    }
}

/// The refusal a `#[repe(readonly)]` endpoint gives a write.
///
/// Emitted *instead of* the write, never before it, so a crate building under
/// `#![deny(warnings)]` is not broken by an `unreachable_code` lint on generated
/// code. Servable under a shared borrow as well as an exclusive one: refusing a
/// write touches nothing, so there is no reason to take the write guard to say
/// so.
fn refuse_write(repe: &TokenStream2, borrow: Borrow) -> TokenStream2 {
    borrow.refuse(quote! {
        #repe::StructError::BodyUnexpected {
            path: #repe::structs::path_from_segments(segments),
        }
    })
}

/// The refusal for a path segment below a leaf endpoint.
fn refuse_subpath(repe: &TokenStream2, borrow: Borrow) -> TokenStream2 {
    borrow.refuse(quote! {
        #repe::StructError::InvalidSubpath {
            path: #repe::structs::path_from_segments(segments),
        }
    })
}

/// Bind a published method's arguments out of the request body.
///
/// One argument *is* the body — the shape the wire has always had. Two or more
/// arrive as a positional array or a name-keyed object; see `MethodArgs`. A
/// method taking none ignores whatever the body holds, which is what makes it
/// callable from a bodiless frame.
fn decode_method_args(
    method: &MethodSpec,
    bindings: &[Ident],
    repe: &TokenStream2,
    borrow: Borrow,
) -> TokenStream2 {
    if method.args.is_empty() {
        return quote! { let _ = &body; };
    }
    let path = endpoint_path(&method.endpoint);
    let missing = borrow.err(quote! {
        #repe::StructError::BodyExpected {
            path: #repe::structs::path_from_segments(segments),
        }
    });
    // One shape for both borrows. `RequestBody` is `Copy` and borrows the frame,
    // so the shared path no longer has to take the body out of an `&mut Option`
    // and owe it back to the exclusive retry when it declines.
    let take_body = quote! {
        let __repe_body = match body {
            Some(__repe_body) => __repe_body,
            None => { #missing }
        };
    };
    let bad_args = borrow.err(quote! { __repe_err });

    if method.args.len() == 1 {
        let binding = &bindings[0];
        let ty = &method.args[0].1;
        return quote! {
            #take_body
            let #binding: #ty = match __repe_body.read(#path) {
                Ok(__repe_value) => __repe_value,
                Err(__repe_err) => { #bad_args }
            };
        };
    }

    let names: Vec<LitStr> = method
        .args
        .iter()
        .map(|(ident, _)| LitStr::new(&ident.to_string(), Span::call_site()))
        .collect();
    let decls = method.args.iter().zip(bindings).map(|((_, ty), binding)| {
        let bad_args = &bad_args;
        quote! {
            let #binding: #ty = match __repe_args.next_arg() {
                Ok(__repe_value) => __repe_value,
                Err(__repe_err) => { #bad_args }
            };
        }
    });
    quote! {
        #take_body
        let mut __repe_args = match #repe::structs::MethodArgs::new(
            #path, &[#(#names),*], __repe_body,
        ) {
            Ok(__repe_args) => __repe_args,
            Err(__repe_err) => { #bad_args }
        };
        #(#decls)*
    }
}

fn build_method_arms(
    methods: &[MethodSpec],
    repe: &TokenStream2,
    borrow: Borrow,
) -> Vec<TokenStream2> {
    methods
        .iter()
        .map(|method| build_method_arm(method, repe, borrow))
        .collect()
}

fn build_method_arm(method: &MethodSpec, repe: &TokenStream2, borrow: Borrow) -> TokenStream2 {
    let key = LitStr::new(&method.endpoint, Span::call_site());
    // A `&mut self` method cannot run under a shared borrow however the frame is
    // shaped. Declining costs nothing: the exclusive path answers it exactly as
    // it always has.
    if matches!(borrow, Borrow::Shared) && !matches!(method.receiver, ReceiverKind::Ref) {
        return quote! { #key => None, };
    }
    let path = endpoint_path(&method.endpoint);
    let subpath = refuse_subpath(repe, borrow);
    let method_ident = &method.method_ident;

    let bindings: Vec<Ident> = (0..method.args.len())
        .map(|i| format_ident!("__repe_arg{}", i))
        .collect();

    let decode_args = decode_method_args(method, &bindings, repe, borrow);

    let emit_ok = if method.ret.ok_is_unit() {
        emit_null()
    } else {
        emit_value(quote! { __repe_ok })
    };
    // Spanned at the method name, because this is the one call in the generated
    // code whose receiver comes from a *declaration* rather than from a
    // signature: a `#[repe(methods(..))]` entry that says `&self` for a
    // `&mut self` method fails here, and the error has to point at that entry
    // rather than at `#[derive(RepeStruct)]`.
    let invocation = quote_spanned! { method_ident.span()=>
        Self::#method_ident(self #(, #bindings)*)
    };
    let call = borrow.ok(call_and_emit(invocation, &method.ret, emit_ok, repe, &path));

    quote! {
        #key => {
            if !tail.is_empty() {
                return #subpath;
            }
            #decode_args
            #call
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
fn build_accessor_arm(
    accessor: &AccessorSpec,
    repe: &TokenStream2,
    borrow: Borrow,
) -> TokenStream2 {
    let key = LitStr::new(&accessor.endpoint, Span::call_site());
    let path = endpoint_path(&accessor.endpoint);
    let subpath = refuse_subpath(repe, borrow);

    // No `ok_is_unit` branch: a getter that returns nothing is rejected at
    // `parse_impl_method`, since the listing would have no value to show for the
    // endpoint. A `&mut self` getter cannot run under a shared borrow.
    let read = if matches!(borrow, Borrow::Shared)
        && !matches!(accessor.get.receiver, ReceiverKind::Ref)
    {
        quote! { None }
    } else {
        let getter = &accessor.get.method_ident;
        let emit = if accessor.typed {
            emit_typed_slice(quote! { __repe_ok })
        } else {
            emit_value(quote! { __repe_ok })
        };
        borrow.ok(call_and_emit(
            quote! { Self::#getter(self) },
            &accessor.get.ret,
            emit,
            repe,
            &path,
        ))
    };

    // A getter with no setter *is* a read-only endpoint, so the refusal is the
    // same one `#[repe(readonly)]` produces on a field. Emitting only the
    // rejection — never a write followed by dead code — keeps generated code
    // clean under `#![deny(warnings)]`, as the field arms do. A setter mutates
    // by construction, so the shared borrow declines it; the refusal above
    // mutates nothing and is served either way.
    let refuse = refuse_write(repe, borrow);
    let write = match (&accessor.set, borrow) {
        (None, _) => quote! { Some(_) => #refuse, },
        (Some(_), Borrow::Shared) => quote! { Some(_) => None, },
        (Some(set), Borrow::Exclusive) => {
            let setter = &set.method_ident;
            let ty = &set.args[0].1;
            let call = call_and_emit(
                quote! { Self::#setter(self, __repe_arg) },
                &set.ret,
                emit_null(),
                repe,
                &path,
            );
            quote! {
                Some(__repe_body) => {
                    let __repe_arg: #ty = __repe_body.read(#path)?;
                    #call
                }
            }
        }
    };

    quote! {
        #key => {
            if !tail.is_empty() {
                return #subpath;
            }
            match body {
                None => #read,
                #write
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared-borrow path
// ---------------------------------------------------------------------------
//
// `repe_shared_into` and `repe_call_shared_into` serve a request through
// `&self`, so a read does not queue behind a long-running call on the same
// object. A decline is not a value any sink can encode, so this is not a third
// `Sink`; it is a second `Raise`, and the arms themselves are the same
// generators the exclusive path uses — `build_method_arm`, `build_accessor_arm`
// — called with `Borrow::Shared`.
//
// That is why the two cannot disagree about how a value is serialized or how a
// body is read: it is one generator, not two kept in step. They differ only in
// whether this borrow serves a path at all, which is decided from the receiver
// at the top of each arm. `tests/shared_reads.rs` pins the rest by driving one
// struct through both and comparing frames.
//
// The field and listing arms are still built separately, because there the
// shapes genuinely diverge: a nested child is asked to decline in turn, and a
// listing rewinds.
//
// **What decides.** The receiver, not the frame. REPE separates read from write
// at the frame level, and taking that as the borrow rule meant a `&self` method
// carrying arguments — a long computation, an HTTP fetch, a system call — ran
// under the write guard and stalled every read of the object for its duration. That is
// the receiver being known at expansion and thrown away. So: a `&self` method is
// served here whether or not it carries a body, a `&mut self` one never is, a
// field write never is, and a nested child is asked the same question in turn.
//
// **The body.** It arrives as `&mut Option<Value>` and is taken only past the
// last point an arm could still decline, because a decline owes the exclusive
// retry the request it was handed. `decode_method_args` places the `take` for
// that reason; a nested child is passed the borrow directly and holds the same
// obligation.
//
// **Listings settle first.** A listing is the one read that composes many
// others, so a decline discovered partway through would leave the entries before
// it already executed, and the exclusive retry executes them again — a `&self`
// getter over a read counter would report the second call. Rewinding the
// response buffer undoes the bytes; it cannot undo a call. Two things can force
// that decline, and the guard asks about both: an accessor on this struct whose
// getter takes `&mut self`, and a `#[repe(nested)]` child that declines at any
// depth. `listing_decline_terms` builds the question;
// `RepeStruct::repe_listing_declines` is how a parent asks it of a child. Once
// the guard passes, nothing left in the listing can decline.
//
// Reading one of those endpoints on its own is unaffected: that arm decides from
// the receiver, before it calls anything.

/// The terms of "a shared whole-object listing of this struct declines",
/// OR'd together by both places that need the answer: the guard at the top of
/// the listing, and the `RepeStruct::repe_listing_declines` this struct
/// publishes for any parent that nests it.
///
/// Two things can force a listing exclusive, and they are not the same thing:
///
/// * an accessor on **this** struct whose getter takes `&mut self`, which
///   `#[repe::methods]` reports as `REPE_LISTING_NEEDS_EXCLUSIVE`; and
/// * a `#[repe(nested)]` child that declines, at any depth — the parent's
///   listing composes the child's, so the child's refusal is the parent's.
///
/// The second is why this cannot be read off `RepeMethods` alone. A child is
/// listed *before* this struct's own accessors are read, so a child discovered
/// to decline partway through would leave those already invoked — and the
/// exclusive retry invokes them again. Asking every child up front is what makes
/// the guard's promise ("nothing after this point can decline") true rather than
/// true by accident of declaration order.
///
/// A leaf field contributes no term: it is written straight into the response
/// and has no impl of its own to ask.
///
/// An empty result means the constant `false`: a struct of plain fields, or one
/// whose nesting and accessors all read shared. The caller emits no guard at all
/// for that, so the common case costs nothing.
/// The terms of "some path on this struct answers a body-carrying frame under a
/// shared borrow", OR'd into the `RepeStruct::REPE_SHARED_SERVES_BODIES` this
/// struct publishes.
///
/// The const is a hint the router reads before it takes the read lock, so the
/// only cost of a wrong answer is concurrency: everything the shared borrow
/// answers with a body, the exclusive path answers identically. That makes `true`
/// the safe direction, and this over-approximates deliberately rather than
/// tracking every arm shape exactly.
///
/// Four things put a body-carrying answer on this struct:
///
/// * `#[repe(readonly)]` on the struct, whose whole-object refusal is servable
///   shared — there is no reason to take the write guard to say no;
/// * `#[repe(readonly)]` on a field, for the same reason one level down;
/// * a struct-listed `&self` method that takes arguments, which is a call
///   rather than a mutation and is exactly what the shared path exists for; and
/// * a `#[repe(nested)]` child with any of the above, at any depth.
///
/// The impl block's own methods and accessors are a fifth, but the derive
/// cannot see them; `#[repe::methods]` computes
/// `RepeMethods::REPE_SHARED_SERVES_BODIES` and this ORs it in.
///
/// A leaf field contributes no term unless it is read-only: a write to one is a
/// write, and needs `&mut self`.
///
/// An empty result means the constant `false` — a struct of plain fields whose
/// every write needs the exclusive borrow, which is the shape the skip is for.
fn shared_body_terms(
    fields: &[FieldSpec],
    struct_attrs: &StructAttrs,
    from_impl_block: bool,
    repe: &TokenStream2,
) -> Vec<TokenStream2> {
    // A refusal that needs no state, or a call that needs no mutation: both are
    // answers this struct can give without the write guard.
    if struct_attrs.no_replace
        || struct_attrs
            .methods
            .iter()
            .any(|m| matches!(m.receiver, ReceiverKind::Ref) && !m.args.is_empty())
    {
        return vec![quote! { true }];
    }

    let mut terms = Vec::new();
    if from_impl_block {
        terms.push(quote! {
            <Self as #repe::structs::RepeMethods>::REPE_SHARED_SERVES_BODIES
        });
    }
    for field in fields {
        if field.attrs.skip {
            continue;
        }
        if field.attrs.readonly {
            return vec![quote! { true }];
        }
        if !field.attrs.nested {
            continue;
        }
        let ty = &field.ty;
        terms.push(quote! {
            <#ty as #repe::RepeStruct>::REPE_SHARED_SERVES_BODIES
        });
    }
    terms
}

fn listing_decline_terms(
    fields: &[FieldSpec],
    from_impl_block: bool,
    repe: &TokenStream2,
) -> Vec<TokenStream2> {
    let mut terms = Vec::new();
    if from_impl_block {
        terms.push(quote! {
            <Self as #repe::structs::RepeMethods>::REPE_LISTING_NEEDS_EXCLUSIVE
        });
    }
    for field in fields {
        if field.attrs.skip || !field.attrs.nested {
            continue;
        }
        let ident = &field.ident;
        let ty = &field.ty;
        terms.push(quote! {
            <#ty as #repe::RepeStruct>::repe_listing_declines(&self.#ident)
        });
    }
    terms
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
        Ok(published) => methods_impl(&item_impl, &published, &repe, false),
        Err(err) => {
            // Emit the diagnostic *and* a table with nothing in it, marked as
            // the recovery shape. Failing with the error alone would drop the
            // `RepeMethods` impl and add a second, misleading "no
            // `#[repe::methods]` impl block" error on top of the real one; the
            // marker is what lets the compile-time checks against these tables
            // stand down without also standing down for a block that compiles
            // and happens to publish nothing.
            let error = err.to_compile_error();
            let empty = methods_impl(&item_impl, &PublishedItems::default(), &repe, true);
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
/// or backed by a resource rather than by storage.
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
    recovered: bool,
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
    // The shared whole-struct listing reads every accessor back through
    // `repe_call_shared_into`, which serves a getter only when it takes `&self`.
    // One `&mut self` getter is enough to make a decline possible partway
    // through the listing, and a decline discovered there would leave the
    // getters before it invoked twice — so the listing declines at the top
    // instead. Every getter being `&self` means no entry can decline, which is
    // what lets the listing be served shared.
    let listing_needs_exclusive = published
        .accessors
        .iter()
        .any(|spec| !matches!(spec.get.receiver, ReceiverKind::Ref));

    // Which endpoints in this table answer a frame that carries a body without
    // the exclusive borrow: a `&self` method taking arguments (a call, not a
    // mutation), a `&self` setter, and a read-only accessor, whose refusal
    // needs no state to give. Everything else declines, and the router can skip
    // the attempt when nothing here is any of the three.
    let shared_serves_bodies = published
        .methods
        .iter()
        .any(|spec| matches!(spec.receiver, ReceiverKind::Ref) && !spec.args.is_empty())
        || published.accessors.iter().any(|spec| match &spec.set {
            Some(set) => matches!(set.receiver, ReceiverKind::Ref),
            None => true,
        });

    let mut bodies = Vec::new();
    {
        let arms = build_method_arms(&published.methods, repe, Borrow::Exclusive)
            .into_iter()
            .chain(
                published
                    .accessors
                    .iter()
                    .map(|accessor| build_accessor_arm(accessor, repe, Borrow::Exclusive)),
            )
            .collect::<Vec<_>>();
        bodies.push(quote! {
            fn repe_call_into(
                &mut self,
                segments: &[&str],
                body: Option<#repe::structs::RequestBody<'_>>,
                out: &mut #repe::structs::ResponseBody<'_>,
            ) -> #repe::structs::StructResult<()> {
                let Some((head, tail)) = segments.split_first() else {
                    return Err(#repe::StructError::InvalidPath { path: String::from("") });
                };
                let _ = (&out, tail, &body);
                match *head {
                    #(#arms)*
                    _ => Err(#repe::StructError::InvalidPath {
                        path: #repe::structs::path_from_segments(segments),
                    }),
                }
            }
        });
    }

    let shared_arms = build_method_arms(&published.methods, repe, Borrow::Shared)
        .into_iter()
        .chain(
            published
                .accessors
                .iter()
                .map(|accessor| build_accessor_arm(accessor, repe, Borrow::Shared)),
        )
        .collect::<Vec<_>>();
    let shared_invalid_root = quote! {
        Some(Err(#repe::StructError::InvalidPath { path: String::from("") }))
    };
    let shared_invalid_path = quote! {
        Some(Err(#repe::StructError::InvalidPath {
            path: #repe::structs::path_from_segments(segments),
        }))
    };
    bodies.push(quote! {
        fn repe_call_shared_into(
            &self,
            segments: &[&str],
            body: Option<#repe::structs::RequestBody<'_>>,
            out: &mut #repe::structs::ResponseBody<'_>,
        ) -> Option<#repe::structs::StructResult<()>> {
            let Some((head, tail)) = segments.split_first() else {
                return #shared_invalid_root;
            };
            // Every arm may be a decline, in which case none of these is read.
            let _ = (&out, tail, &body);
            match *head {
                #(#shared_arms)*
                _ => #shared_invalid_path,
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

            const REPE_LISTING_NEEDS_EXCLUSIVE: bool = #listing_needs_exclusive;

            const REPE_SHARED_SERVES_BODIES: bool = #shared_serves_bodies;

            const REPE_TABLE_RECOVERED: bool = #recovered;

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
/// (Every `repe` type below is linked by URL rather than by path: this is a
/// proc-macro crate, and it does not — cannot — depend on `repe`.)
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
/// [`Router`]: https://docs.rs/repe/latest/repe/server/struct.Router.html
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

#[cfg(test)]
mod tests {
    //! The rejections this macro makes at expansion time.
    //!
    //! Every one of these is a `syn::Error` the user reads as a compile error,
    //! and its text is the whole remedy. Asserting on the message here rather
    //! than on a captured rustc rendering keeps the check precise about the
    //! sentence we wrote and indifferent to how the compiler frames it, which
    //! changes between releases.

    use super::*;
    use syn::parse_quote;

    fn field_error(field: syn::Field) -> String {
        match parse_field(&field) {
            Err(err) => err.to_string(),
            Ok(spec) => panic!("`{}` was supposed to be rejected", spec.endpoint),
        }
    }

    fn order(keys: &[&str]) -> ListingOrder {
        ListingOrder {
            keys: keys
                .iter()
                .map(|k| LitStr::new(k, Span::call_site()))
                .collect(),
            span: Span::call_site(),
        }
    }

    fn declared(names: &[&str]) -> Vec<(&'static str, Span)> {
        // Leaked so the borrow outlives the call; a test binary's lifetime.
        names
            .iter()
            .map(|n| {
                (
                    &*Box::leak(n.to_string().into_boxed_str()),
                    Span::call_site(),
                )
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Field attribute conflicts
    // -----------------------------------------------------------------------

    #[test]
    fn typed_and_nested_are_rejected_together() {
        let msg = field_error(parse_quote! {
            #[repe(typed, nested)]
            child: Child
        });
        assert!(
            msg.contains("BEVE typed array"),
            "the message should say why the two cannot combine, got: {msg}"
        );
    }

    #[test]
    fn nested_serde_is_rejected_with_a_migration_note() {
        // It is rejected during attribute parsing, so it is rejected in every
        // combination and there is nothing left for the `typed`/`nested`
        // conflict checks to see.
        for field in [
            parse_quote! { #[repe(nested_serde)] child: Child },
            parse_quote! { #[repe(typed, nested_serde)] child: Child },
            parse_quote! { #[repe(nested, nested_serde)] child: Child },
        ] {
            let msg = field_error(field);
            assert!(
                msg.contains("nested_serde") && msg.contains("structio"),
                "the message should name what replaced it, got: {msg}"
            );
        }
    }

    #[test]
    fn readonly_on_a_struct_is_rejected_and_points_at_no_replace() {
        // The word is deliberately unclaimed at struct level: on a field it is
        // recursive, and the recursive meaning is the one a struct should get
        // if it ever gets one.
        let attrs: Vec<syn::Attribute> = vec![parse_quote! { #[repe(readonly)] }];
        let msg = match parse_struct_attrs(&attrs) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("`readonly` on a struct is supposed to be rejected"),
        };
        assert!(
            msg.contains("no_replace") && msg.contains("recursive"),
            "the message should name the replacement and say why the two differ, got: {msg}"
        );
    }

    #[test]
    fn no_replace_on_a_struct_is_accepted() {
        let attrs: Vec<syn::Attribute> = vec![parse_quote! { #[repe(no_replace)] }];
        let Ok(parsed) = parse_struct_attrs(&attrs) else {
            panic!("`no_replace` is supposed to be accepted");
        };
        assert!(parsed.no_replace);
    }

    #[test]
    fn readonly_on_a_field_is_still_accepted() {
        // Unchanged, and the reason the struct-level spelling had to move.
        let field: syn::Field = parse_quote! { #[repe(readonly)] a: u64 };
        let Ok(spec) = parse_field(&field) else {
            panic!("`readonly` on a field is supposed to be accepted");
        };
        assert!(spec.attrs.readonly);
    }

    #[test]
    fn an_unknown_field_attribute_is_rejected() {
        let msg = field_error(parse_quote! {
            #[repe(nonsense)]
            a: u64
        });
        assert!(
            msg.contains("unsupported `repe` field attribute"),
            "got: {msg}"
        );
    }

    #[test]
    fn the_ordinary_combinations_are_accepted() {
        for field in [
            parse_quote! { #[repe(typed)] values: Vec<f64> },
            parse_quote! { #[repe(nested)] child: Child },
            parse_quote! { #[repe(nested, readonly)] child: Child },
            parse_quote! { #[repe(rename = "other")] a: u64 },
            parse_quote! { #[repe(skip)] a: u64 },
        ] {
            let field: syn::Field = field;
            assert!(parse_field(&field).is_ok());
        }
    }

    // -----------------------------------------------------------------------
    // `#[repe(listing_order(..))]`, the half checked at macro time
    // -----------------------------------------------------------------------

    #[test]
    fn an_order_naming_every_endpoint_is_accepted() {
        let d = declared(&["a", "b"]);
        assert!(validate_listing_order(&order(&["b", "a"]), &d, false).is_ok());
    }

    #[test]
    fn a_key_named_twice_is_rejected() {
        let d = declared(&["a"]);
        let msg = validate_listing_order(&order(&["a", "a"]), &d, false)
            .expect_err("a repeated key is rejected")
            .to_string();
        assert!(msg.contains("named twice"), "got: {msg}");
    }

    #[test]
    fn a_key_that_is_not_an_endpoint_is_rejected() {
        let d = declared(&["a"]);
        let msg = validate_listing_order(&order(&["a", "typo"]), &d, false)
            .expect_err("an unknown key is rejected")
            .to_string();
        assert!(
            msg.contains("`typo` is not an endpoint on this struct"),
            "the message should name the offending key, got: {msg}"
        );
    }

    #[test]
    fn an_endpoint_missing_from_the_order_is_rejected() {
        let d = declared(&["a", "b"]);
        let msg = validate_listing_order(&order(&["a"]), &d, false)
            .expect_err("an omitted endpoint is rejected")
            .to_string();
        assert!(
            msg.contains("`b` is missing from"),
            "the message should name the omitted endpoint, got: {msg}"
        );
    }

    #[test]
    fn an_impl_block_defers_the_unknown_key_check_to_the_const_assertion() {
        // The derive cannot see the impl block's endpoints, so an unrecognized
        // key here may still be one of them. `assert_listing_order` catches a
        // genuine typo at compile time instead; `repe-core`'s
        // `const_assertions.rs` pins that message.
        let d = declared(&["a"]);
        assert!(validate_listing_order(&order(&["a", "calc"]), &d, true).is_ok());

        // The other direction is still checked, because a field the derive can
        // see is one it knows the order must name.
        let msg = validate_listing_order(&order(&["calc"]), &d, true)
            .expect_err("a field omitted from the order is still rejected")
            .to_string();
        assert!(msg.contains("`a` is missing from"), "got: {msg}");
    }
}
