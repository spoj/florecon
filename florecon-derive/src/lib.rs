//! `#[derive(Record)]` — make one struct the single source of truth for a
//! plugin's wire schema, its typed projection, and its identity.
//!
//! A field's Rust type fixes its wire type (`i64` → int, `f64` → float,
//! `String` → text). Attributes mark roles:
//!
//! ```ignore
//! #[derive(florecon::sdk::Record)]
//! struct Ledger {
//!     #[record(id)] row_id: i64,            // the stable per-row identity
//!     company: String,
//!     #[record(amount)] bs_usd: f64,        // the host's headline display amount
//!     gl_date: String,
//! }
//! ```
//!
//! The derive generates `Record::fields()` (for `describe()`), `from_view()`
//! (the typed projection of a `RowView`), and `ext_id()` — so an author never
//! writes a stringly column name or touches `RowView` by hand.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Type, parse_macro_input};

enum Kind {
    Int,
    Float,
    Text,
}

fn kind_of(ty: &Type) -> Option<Kind> {
    let Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    match seg.ident.to_string().as_str() {
        "i64" => Some(Kind::Int),
        "f64" => Some(Kind::Float),
        "String" => Some(Kind::Text),
        _ => None,
    }
}

/// True if the field carries `#[record(<flag>)]`.
fn has_flag(field: &syn::Field, flag: &str) -> bool {
    let mut found = false;
    for attr in &field.attrs {
        if !attr.path().is_ident("record") {
            continue;
        }
        // Parse the comma-separated idents inside `record(...)`.
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident(flag) {
                found = true;
            }
            Ok(())
        });
    }
    found
}

#[proc_macro_derive(Record, attributes(record))]
pub fn derive_record(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => &n.named,
            _ => {
                return syn::Error::new_spanned(name, "Record requires named fields")
                    .to_compile_error()
                    .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(name, "Record can only derive on a struct")
                .to_compile_error()
                .into();
        }
    };

    let mut field_decls = Vec::new();
    let mut projections = Vec::new();
    let mut id_field: Option<syn::Ident> = None;

    for f in fields {
        let ident = f.ident.clone().unwrap();
        let col = ident.to_string();
        let Some(kind) = kind_of(&f.ty) else {
            return syn::Error::new_spanned(&f.ty, "Record field must be i64, f64, or String")
                .to_compile_error()
                .into();
        };
        let is_id = has_flag(f, "id");
        let is_amount = has_flag(f, "amount");

        let ctor = match kind {
            Kind::Int => quote! { ::florecon::sdk::Field::int(#col) },
            Kind::Float => quote! { ::florecon::sdk::Field::float(#col) },
            Kind::Text => quote! { ::florecon::sdk::Field::text(#col) },
        };
        let ctor = if is_amount {
            quote! { #ctor.amount() }
        } else {
            ctor
        };
        field_decls.push(ctor);

        let read = match kind {
            Kind::Int => quote! { #ident: r.i64(#col) },
            Kind::Float => quote! { #ident: r.f64(#col) },
            Kind::Text => quote! { #ident: r.str(#col).to_string() },
        };
        projections.push(read);

        if is_id {
            if id_field.is_some() {
                return syn::Error::new_spanned(
                    &ident,
                    "Record has more than one #[record(id)] field",
                )
                .to_compile_error()
                .into();
            }
            if !matches!(kind, Kind::Int) {
                return syn::Error::new_spanned(&f.ty, "#[record(id)] field must be i64")
                    .to_compile_error()
                    .into();
            }
            id_field = Some(ident.clone());
        }
    }

    let Some(id_field) = id_field else {
        return syn::Error::new_spanned(name, "Record needs exactly one #[record(id)] field")
            .to_compile_error()
            .into();
    };

    quote! {
        impl ::florecon::sdk::Record for #name {
            fn fields() -> ::std::vec::Vec<::florecon::sdk::Field> {
                ::std::vec![ #( #field_decls ),* ]
            }
            fn from_view(r: &::florecon::sdk::RowView<'_>) -> Self {
                Self { #( #projections ),* }
            }
            fn ext_id(&self) -> u64 {
                self.#id_field as u64
            }
        }
    }
    .into()
}
