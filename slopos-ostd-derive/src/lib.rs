//! Derive macros for `slopos-ostd`.
//!
//! Currently exports a single `#[derive(Pod)]` for the
//! `slopos_ostd::Pod` trait. The derive enforces three rules:
//!  1. The type must carry `#[repr(C)]` or `#[repr(transparent)]`.
//!  2. `#[repr(packed)]` is rejected (alignment invariants conflict
//!     with `read_pod` / `write_pod` alignment checks).
//!  3. Enums are rejected; only structs (named, tuple, unit) are
//!     accepted. Each field type acquires a `T: ::slopos_ostd::Pod`
//!     `where`-bound so the type-checker enforces field POD-ness.

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
    match expand_pod(&input) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_pod(input: &DeriveInput) -> syn::Result<TokenStream2> {
    check_repr(input)?;

    let fields = match &input.data {
        Data::Struct(s) => collect_field_types(&s.fields),
        Data::Enum(e) => {
            return Err(syn::Error::new(
                e.enum_token.span(),
                "Pod cannot be derived for enums",
            ));
        }
        Data::Union(u) => {
            return Err(syn::Error::new(
                u.union_token.span(),
                "Pod cannot be derived for unions",
            ));
        }
    };

    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let mut predicates: Vec<TokenStream2> = Vec::new();
    if let Some(wc) = where_clause {
        for p in &wc.predicates {
            predicates.push(quote! { #p });
        }
    }
    for ty in &fields {
        predicates.push(quote! { #ty: ::slopos_ostd::Pod });
    }

    let where_tokens = if predicates.is_empty() {
        quote! {}
    } else {
        quote! { where #(#predicates),* }
    };

    Ok(quote! {
        // SAFETY: derive enforces #[repr(C)] / #[repr(transparent)],
        // forbids #[repr(packed)], and adds field-level Pod bounds.
        unsafe impl #impl_generics ::slopos_ostd::Pod for #name #ty_generics #where_tokens {}
    })
}

fn collect_field_types(fields: &Fields) -> Vec<syn::Type> {
    match fields {
        Fields::Named(n) => n.named.iter().map(|f| f.ty.clone()).collect(),
        Fields::Unnamed(u) => u.unnamed.iter().map(|f| f.ty.clone()).collect(),
        Fields::Unit => Vec::new(),
    }
}

fn check_repr(input: &DeriveInput) -> syn::Result<()> {
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
            "Pod cannot be derived for #[repr(packed)] types: misaligned reads conflict with read_pod alignment checks",
        ));
    }
    if !saw_c_or_transparent {
        return Err(syn::Error::new(
            input.ident.span(),
            "Pod requires #[repr(C)] or #[repr(transparent)] on the type",
        ));
    }
    Ok(())
}
