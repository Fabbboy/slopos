//! Derive macros for `slopos-ostd`.
//!
//! Exports `#[derive(Pod)]` and `#[derive(Zeroable)]` for the
//! `slopos_ostd::Pod` and `slopos_ostd::Zeroable` traits. Each derive
//! enforces three rules:
//!  1. The type must carry `#[repr(C)]` or `#[repr(transparent)]`.
//!  2. `#[repr(packed)]` is rejected (alignment invariants conflict
//!     with `read_pod` / `write_pod` alignment checks; for `Zeroable`,
//!     hand-write the `unsafe impl` instead).
//!  3. Enums are rejected; only structs (named, tuple, unit) are
//!     accepted. Each field type acquires a `T: ::slopos_ostd::Pod`
//!     (or `Zeroable`) `where`-bound so the type-checker enforces
//!     field-level conformance.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Data, DeriveInput, Fields, Meta, Token, parse_macro_input, punctuated::Punctuated,
    spanned::Spanned,
};

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
        Data::Enum(e) => {
            return Err(syn::Error::new(
                e.enum_token.span(),
                format!("{} cannot be derived for enums", kind.name()),
            ));
        }
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

fn check_repr(input: &DeriveInput, kind: MarkerKind) -> syn::Result<()> {
    let mut saw_c_or_transparent = false;
    let mut saw_packed = false;
    let mut packed_span = input.ident.span();

    for attr in &input.attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }
        let nested = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
        for meta in &nested {
            if meta.path().is_ident("C") || meta.path().is_ident("transparent") {
                saw_c_or_transparent = true;
            }
            if meta.path().is_ident("packed") {
                saw_packed = true;
                packed_span = meta.span();
            }
        }
    }

    if saw_packed {
        return Err(syn::Error::new(
            packed_span,
            format!(
                "{} cannot be derived for #[repr(packed)] types; hand-write the unsafe impl",
                kind.name()
            ),
        ));
    }
    if !saw_c_or_transparent {
        return Err(syn::Error::new(
            input.ident.span(),
            format!(
                "{} requires #[repr(C)] or #[repr(transparent)] on the type",
                kind.name()
            ),
        ));
    }
    Ok(())
}
