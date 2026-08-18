//! Derive macros for `slopos-ostd`.
//!
//! Exports `#[derive(Pod)]`, `#[derive(Zeroable)]` and
//! `#[derive(SlotFields)]`. `SlotFields` emits only safe tokens, so a consumer
//! crate writes fields into uninitialised memory without an `unsafe` block
//! appearing anywhere in its expansion.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::{
    Data, DeriveInput, Fields, ItemFn, Meta, Token, parse_macro_input, punctuated::Punctuated,
    spanned::Spanned,
};

/// Implement `slopos_ostd::process::quota::Charged` for a type carrying an
/// object charge.
///
/// The field must be named `object_charge` and typed `Charge<ObjectRow>`. The
/// derive is the only implementor of the sealed supertrait, so `impl
/// FileBacking for X {}` fails to compile unless `X` is accounted for.
///
/// Refuses to expand alongside `Clone` or `Copy`: a cloned charge refunds twice.
#[proc_macro_derive(Charged)]
pub fn derive_charged(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_charged(&input) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_charged(input: &DeriveInput) -> syn::Result<TokenStream2> {
    for attr in &input.attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let nested = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
        for meta in &nested {
            if meta.path().is_ident("Clone") || meta.path().is_ident("Copy") {
                return Err(syn::Error::new(
                    meta.span(),
                    "a Charged type may not be Clone or Copy: a cloned charge refunds \
                     twice. Clone the handle the charge accounts for instead, or wrap \
                     the object in a KArc",
                ));
            }
        }
    }

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => n,
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "Charged requires a struct with named fields: the charge is \
                     addressed by name",
                ));
            }
        },
        Data::Enum(e) => {
            return Err(syn::Error::new(
                e.enum_token.span(),
                "Charged cannot be derived for enums: a variant without the charge \
                 would be an unaccounted state",
            ));
        }
        Data::Union(u) => {
            return Err(syn::Error::new(
                u.union_token.span(),
                "Charged cannot be derived for unions",
            ));
        }
    };

    let field = named
        .named
        .iter()
        .find(|f| f.ident.as_ref().is_some_and(|i| i == "object_charge"))
        .ok_or_else(|| {
            syn::Error::new(
                input.ident.span(),
                "Charged requires a field `object_charge`, typed `Charge<ObjectRow>`, \
                 `AliasOf` for a backing whose object is charged elsewhere, or \
                 `SharedCharge` for a type whose values play both roles; without \
                 one the type claims to be accounted for and is not",
            )
        })?;

    let ty = field.ty.to_token_stream().to_string();
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    // `AliasOf` names an object charged elsewhere; the field is a written
    // claim, so the expansion consumes it once to keep `dead_code` off every
    // definition site.
    let body = if ty.contains("SharedCharge") {
        quote! { self.object_charge.get() }
    } else if ty.contains("AliasOf") {
        quote! {
            let _ = &self.object_charge.owner;
            ::core::option::Option::None
        }
    } else {
        quote! { ::core::option::Option::Some(&self.object_charge) }
    };

    Ok(quote! {
        impl #impl_generics ::slopos_ostd::process::quota::charged_sealed::ChargedSealed
            for #name #ty_generics #where_clause {}

        impl #impl_generics ::slopos_ostd::process::quota::Charged
            for #name #ty_generics #where_clause
        {
            #[inline]
            fn object_charge(
                &self,
            ) -> ::core::option::Option<
                &::slopos_ostd::process::quota::Charge<::slopos_abi::quota::ObjectRow>,
            > {
                #body
            }
        }
    })
}

#[proc_macro_derive(Pod)]
pub fn derive_pod(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_marker(&input, MarkerKind::Pod) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[proc_macro_derive(Zeroable)]
pub fn derive_zeroable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_marker(&input, MarkerKind::Zeroable) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Build the compile-time field table `SlotPtr`'s writers address a slot
/// through.
///
/// For `struct DataState { iss: u32, sendmap: SendMap }` the expansion is a
/// zero-sized `DataStateSlotFields` holding one
/// `Field<DataState, T, { offset_of!(DataState, f) }>` per field, plus the
/// `HasFields` impl carrying it as an associated `const`, so `write_field!`
/// name-checks the field path and resolves its byte offset at compile time
/// with no `unsafe` token in the invoking crate.
///
/// Named-field, non-generic, non-`#[repr(packed)]` structs only.
#[proc_macro_derive(SlotFields)]
pub fn derive_slot_fields(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_slot_fields(&input) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_slot_fields(input: &DeriveInput) -> syn::Result<TokenStream2> {
    reject_packed(input)?;

    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "SlotFields cannot be derived for generic types: offset_of! in \
             const-generic position requires generic_const_exprs",
        ));
    }

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => n,
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "SlotFields requires a struct with named fields",
                ));
            }
        },
        Data::Enum(e) => {
            return Err(syn::Error::new(
                e.enum_token.span(),
                "SlotFields cannot be derived for enums",
            ));
        }
        Data::Union(u) => {
            return Err(syn::Error::new(
                u.union_token.span(),
                "SlotFields cannot be derived for unions",
            ));
        }
    };

    let name = &input.ident;
    let vis = &input.vis;
    let table = syn::Ident::new(&format!("{name}SlotFields"), name.span());

    let mut members = Vec::new();
    let mut initialisers = Vec::new();
    let mut offsets = Vec::new();

    for field in &named.named {
        let ident = field.ident.as_ref().expect("named fields checked above");
        let ty = substitute_self(field.ty.to_token_stream(), name);
        members.push(quote! {
            #vis #ident: ::slopos_ostd::mm::init::Field<
                #name, #ty, { ::core::mem::offset_of!(#name, #ident) }
            >
        });
        initialisers.push(quote! {
            #ident: ::slopos_ostd::mm::init::Field::__new(|__t: &#name| &__t.#ident)
        });
        offsets.push(quote! { ::core::mem::offset_of!(#name, #ident) });
    }

    let field_count = named.named.len();

    Ok(quote! {
        #[doc = concat!("Compile-time field table for [`", stringify!(#name), "`].")]
        #[doc(hidden)]
        #vis struct #table {
            #(#members),*
        }

        // Hand-written: `#[derive(Clone, Copy)]` emits an `unsafe impl
        // ::core::clone::TrivialClone`, which this derive exists to avoid.
        impl ::core::clone::Clone for #table {
            #[inline]
            fn clone(&self) -> Self {
                *self
            }
        }
        impl ::core::marker::Copy for #table {}

        impl ::slopos_ostd::mm::init::HasFields for #name {
            type Fields = #table;
            const FIELD_COUNT: usize = #field_count;
            const FIELD_OFFSETS: &'static [usize] = &[#(#offsets),*];
            const FIELDS: #table = #table {
                #(#initialisers),*
            };
        }

        const _: () = assert!(
            ::core::mem::size_of::<#table>() == 0,
            "SlotFields table must be zero-sized so it costs no stack frame",
        );
    })
}

/// Rewrite every `Self` ident in a field type to the concrete type name.
///
/// The generated field table is a *different* type, so a field spelled
/// `Bitmap<{ words_for(Self::COUNT) }>` would otherwise resolve `Self` to the
/// table. In field position `Self` can only mean the struct itself, so the
/// substitution is unconditional.
fn substitute_self(tokens: TokenStream2, name: &syn::Ident) -> TokenStream2 {
    use proc_macro2::{Group, TokenTree};
    tokens
        .into_iter()
        .map(|tt| match tt {
            TokenTree::Ident(id) if id == "Self" => {
                TokenTree::Ident(syn::Ident::new(&name.to_string(), id.span()))
            }
            TokenTree::Group(g) => {
                TokenTree::Group(Group::new(g.delimiter(), substitute_self(g.stream(), name)))
            }
            other => other,
        })
        .collect()
}

/// Turn a free `fn(pkt: &mut PacketView<'_>) -> XdpAction` into a registrable
/// XDP filter.
///
/// `#[xdp_filter] fn drop_ssh(pkt: &mut PacketView<'_>) -> XdpAction { … }`
/// expands to the original function plus a zero-sized type implementing
/// `slopos_net::xdp::XdpFilter` and a `'static` instance named after the
/// function in upper-case. Register it explicitly:
///
/// ```ignore
/// XDP.register(&DROP_SSH);
/// ```
///
/// No `unsafe` and no link-section magic: filter chain membership and order
/// stay an explicit sequence of `register` calls. Paths resolve through
/// `slopos_net` (the `net` crate aliases itself via
/// `extern crate self as slopos_net;`).
#[proc_macro_attribute]
pub fn xdp_filter(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    match expand_xdp_filter(func) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_xdp_filter(func: ItemFn) -> syn::Result<TokenStream2> {
    if func.sig.inputs.len() != 1 {
        return Err(syn::Error::new(
            func.sig.span(),
            "#[xdp_filter] function must take exactly one argument: `&mut PacketView<'_>`",
        ));
    }
    if func.sig.asyncness.is_some() {
        return Err(syn::Error::new(
            func.sig.span(),
            "#[xdp_filter] function may not be `async`",
        ));
    }

    let fn_name = func.sig.ident.clone();
    let span = fn_name.span();
    let struct_name = syn::Ident::new(&format!("__XdpFilter_{fn_name}"), span);
    let static_name = syn::Ident::new(&fn_name.to_string().to_uppercase(), span);

    Ok(quote! {
        #func

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #struct_name;

        impl ::slopos_net::xdp::XdpFilter for #struct_name {
            fn execute(
                &self,
                pkt: &mut ::slopos_net::xdp::PacketView<'_>,
            ) -> ::slopos_net::xdp::XdpAction {
                #fn_name(pkt)
            }
        }

        /// `'static` filter instance generated by `#[xdp_filter]`. Pass
        /// `&#static_name` to `XDP.register` / `XDP.install`.
        #[allow(non_upper_case_globals)]
        pub static #static_name: #struct_name = #struct_name;
    })
}

#[derive(Clone, Copy)]
enum MarkerKind {
    Pod,
    Zeroable,
}

impl MarkerKind {
    fn name(self) -> &'static str {
        match self {
            MarkerKind::Pod => "Pod",
            MarkerKind::Zeroable => "Zeroable",
        }
    }

    fn trait_path(self) -> TokenStream2 {
        match self {
            MarkerKind::Pod => quote! { ::slopos_ostd::Pod },
            MarkerKind::Zeroable => quote! { ::slopos_ostd::Zeroable },
        }
    }
}

fn expand_marker(input: &DeriveInput, kind: MarkerKind) -> syn::Result<TokenStream2> {
    check_repr(input, kind)?;

    let fields = match &input.data {
        Data::Struct(s) => collect_field_types(&s.fields),
        Data::Enum(e) => match kind {
            // A fieldless enum with a primitive repr whose first variant sits
            // at discriminant 0 has a valid all-zero representation, and both
            // halves of that are checked below. `Pod` needs the stronger
            // property that *every* bit pattern is valid, which no enum has.
            MarkerKind::Zeroable => {
                check_zeroable_enum(input, e)?;
                Vec::new()
            }
            MarkerKind::Pod => {
                return Err(syn::Error::new(
                    e.enum_token.span(),
                    "Pod cannot be derived for enums: an enum has invalid bit patterns",
                ));
            }
        },
        Data::Union(u) => {
            return Err(syn::Error::new(
                u.union_token.span(),
                format!("{} cannot be derived for unions", kind.name()),
            ));
        }
    };

    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let trait_path = kind.trait_path();

    let mut predicates: Vec<TokenStream2> = Vec::new();
    if let Some(wc) = where_clause {
        for p in &wc.predicates {
            predicates.push(quote! { #p });
        }
    }
    for ty in &fields {
        predicates.push(quote! { #ty: #trait_path });
    }

    let where_tokens = if predicates.is_empty() {
        quote! {}
    } else {
        quote! { where #(#predicates),* }
    };

    Ok(quote! {
        // SAFETY: derive enforces #[repr(C)] / #[repr(transparent)],
        // forbids #[repr(packed)], and adds field-level marker bounds.
        unsafe impl #impl_generics #trait_path for #name #ty_generics #where_tokens {}
    })
}

fn collect_field_types(fields: &Fields) -> Vec<syn::Type> {
    match fields {
        Fields::Named(n) => n.named.iter().map(|f| f.ty.clone()).collect(),
        Fields::Unnamed(u) => u.unnamed.iter().map(|f| f.ty.clone()).collect(),
        Fields::Unit => Vec::new(),
    }
}

/// Scan `#[repr(...)]` for the two properties the derives care about.
fn scan_repr(input: &DeriveInput) -> syn::Result<ReprFacts> {
    const PRIMITIVE: &[&str] = &[
        "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
    ];
    let mut facts = ReprFacts {
        c_or_transparent: false,
        primitive: false,
        packed_span: None,
    };
    for attr in &input.attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }
        let nested = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
        for meta in &nested {
            if meta.path().is_ident("C") || meta.path().is_ident("transparent") {
                facts.c_or_transparent = true;
            }
            if PRIMITIVE.iter().any(|p| meta.path().is_ident(p)) {
                facts.primitive = true;
            }
            if meta.path().is_ident("packed") {
                facts.packed_span = Some(meta.span());
            }
        }
    }
    Ok(facts)
}

struct ReprFacts {
    c_or_transparent: bool,
    primitive: bool,
    packed_span: Option<proc_macro2::Span>,
}

fn reject_packed(input: &DeriveInput) -> syn::Result<()> {
    match scan_repr(input)?.packed_span {
        Some(span) => Err(syn::Error::new(
            span,
            "SlotFields cannot be derived for #[repr(packed)] types: the field \
             projection closure is E0793 on a packed field",
        )),
        None => Ok(()),
    }
}

fn check_repr(input: &DeriveInput, kind: MarkerKind) -> syn::Result<()> {
    let facts = scan_repr(input)?;

    if let Some(packed_span) = facts.packed_span {
        return Err(syn::Error::new(
            packed_span,
            format!(
                "{} cannot be derived for #[repr(packed)] types; hand-write the unsafe impl",
                kind.name()
            ),
        ));
    }
    // `Pod` reinterprets bytes, so the layout has to be the declared one.
    // `Zeroable` only claims the all-zero value is valid, which is the
    // conjunction of the fields' own claims at whatever offsets the compiler
    // picks — a property `#[repr(Rust)]` preserves.
    if matches!(kind, MarkerKind::Pod) && !facts.c_or_transparent {
        return Err(syn::Error::new(
            input.ident.span(),
            "Pod requires #[repr(C)] or #[repr(transparent)] on the type",
        ));
    }
    Ok(())
}

/// A fieldless enum is `Zeroable` exactly when a zero discriminant names a
/// real variant. Both halves of that are syntactically checkable.
fn check_zeroable_enum(input: &DeriveInput, data: &syn::DataEnum) -> syn::Result<()> {
    if !scan_repr(input)?.primitive {
        return Err(syn::Error::new(
            input.ident.span(),
            "Zeroable requires a primitive representation (e.g. #[repr(u8)]) on an enum, \
             so the discriminant encoding is defined",
        ));
    }

    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new(
                variant.span(),
                "Zeroable can only be derived for fieldless enums: a variant payload \
                 needs its own zero-validity argument",
            ));
        }
    }

    let Some(first) = data.variants.first() else {
        return Err(syn::Error::new(
            input.ident.span(),
            "Zeroable cannot be derived for an empty enum: it has no valid value at all",
        ));
    };

    // The first variant is discriminant 0 unless it says otherwise; a later
    // variant cannot occupy 0 without the first one having a negative
    // discriminant, which a `#[repr(uN)]` enum cannot have.
    match &first.discriminant {
        None => Ok(()),
        Some((_, expr)) if is_zero_literal(expr) => Ok(()),
        Some((_, expr)) => Err(syn::Error::new(
            expr.span(),
            "Zeroable requires the first variant to sit at discriminant 0",
        )),
    }
}

fn is_zero_literal(expr: &syn::Expr) -> bool {
    matches!(
        expr,
        syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(i), .. })
            if i.base10_digits() == "0"
    )
}
