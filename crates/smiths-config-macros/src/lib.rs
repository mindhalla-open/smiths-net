//! `#[derive(Reloadable)]` — generates [`Reloadable`] diff logic
//! for config structs so the reloadable-field list can't silently
//! drift when a new field lands.
//!
//! See the crate README for attribute semantics and the decision
//! tree between `#[reloadable]` / `#[restart_required]` / `#[nested]`.
//!
//! The generated `impl` references `crate::reloader::Reloadable`
//! plus `crate::reloader::ApplyReport`, so the macro is intended
//! to be used only inside `smiths-core` itself (where those paths
//! resolve). If a future caller needs to derive `Reloadable` in a
//! downstream crate, switch the generated paths to
//! `::smiths_core::reloader::...` — nothing else changes.

use std::collections::BTreeMap;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Attribute, Data, DeriveInput, Expr, ExprLit, Fields, Lit, Meta, parse_macro_input};

/// Per-field classification parsed from attributes.
enum FieldCfg {
    /// `#[reloadable]` / `#[reloadable(path = "...")]`.
    Reloadable { path: Option<String> },
    /// `#[restart_required]` / `#[restart_required(group = "...")]`.
    RestartRequired { group: Option<String> },
    /// `#[nested]` — recurse into a field whose type also impls
    /// `Reloadable`.
    Nested,
    /// No attribute → skipped. The macro treats unmarked fields as
    /// "outside the `apply_report` universe"; developer discipline
    /// chooses whether that's intentional.
    Skip,
}

/// Extract the `path = "..."` / `group = "..."` string value from
/// an attribute's `(key = "value", ...)` argument list. Returns
/// `None` when the key is absent.
fn name_value_string(attr: &Attribute, key: &str) -> Option<String> {
    let mut out: Option<String> = None;
    // `parse_nested_meta` covers `#[x(key = "val", ...)]` shape.
    let _ = attr.parse_nested_meta(|m| {
        if m.path.is_ident(key) {
            let v: Expr = m.value()?.parse()?;
            if let Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) = v
            {
                out = Some(s.value());
            }
        }
        Ok(())
    });
    out
}

fn classify(field: &syn::Field) -> FieldCfg {
    for attr in &field.attrs {
        if attr.path().is_ident("reloadable") {
            let path = match &attr.meta {
                Meta::List(_) => name_value_string(attr, "path"),
                Meta::Path(_) | Meta::NameValue(_) => None,
            };
            return FieldCfg::Reloadable { path };
        }
        if attr.path().is_ident("restart_required") {
            let group = match &attr.meta {
                Meta::List(_) => name_value_string(attr, "group"),
                Meta::Path(_) | Meta::NameValue(_) => None,
            };
            return FieldCfg::RestartRequired { group };
        }
        if attr.path().is_ident("nested") {
            return FieldCfg::Nested;
        }
    }
    FieldCfg::Skip
}

/// Build an expression that yields the dotted path for a field.
/// `path_prefix` (empty or `"foo.bar"`) is joined with the field's
/// own name; if the caller provided an explicit `path = "..."`, it
/// wins and no prefix is consulted.
fn path_expr(field_name: &str, explicit: Option<&str>) -> TokenStream2 {
    if let Some(p) = explicit {
        return quote!( ::std::string::String::from(#p) );
    }
    quote! {
        if path_prefix.is_empty() {
            ::std::string::String::from(#field_name)
        } else {
            ::std::format!("{}.{}", path_prefix, #field_name)
        }
    }
}

#[proc_macro_derive(Reloadable, attributes(reloadable, restart_required, nested))]
pub fn derive_reloadable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_gen, ty_gen, where_gen) = input.generics.split_for_impl();

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => &n.named,
            _ => {
                return syn::Error::new_spanned(
                    name,
                    "Reloadable requires a struct with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(name, "Reloadable is only supported on structs")
                .to_compile_error()
                .into();
        }
    };

    // Emit buckets. Restart-required fields with a `group` label get
    // coalesced: multiple fields sharing the same label collapse to a
    // single `if (f1 != f1') || (f2 != f2') || ... { report.push(label) }`.
    let mut reloadable_emits: Vec<TokenStream2> = Vec::new();
    let mut restart_plain: Vec<TokenStream2> = Vec::new();
    let mut restart_groups: BTreeMap<String, Vec<TokenStream2>> = BTreeMap::new();
    let mut nested_emits: Vec<TokenStream2> = Vec::new();

    for field in fields {
        let fname = field.ident.as_ref().expect("named field");
        let fname_str = fname.to_string();
        match classify(field) {
            FieldCfg::Reloadable { path } => {
                let path_expr = path_expr(&fname_str, path.as_deref());
                reloadable_emits.push(quote! {
                    if self.#fname != new.#fname {
                        report.reloaded.push(#path_expr);
                    }
                });
            }
            FieldCfg::RestartRequired { group: Some(label) } => {
                restart_groups
                    .entry(label)
                    .or_default()
                    .push(quote!( self.#fname != new.#fname ));
            }
            FieldCfg::RestartRequired { group: None } => {
                let path_expr = path_expr(&fname_str, None);
                restart_plain.push(quote! {
                    if self.#fname != new.#fname {
                        report.restart_required.push(#path_expr);
                    }
                });
            }
            FieldCfg::Nested => {
                let prefix = path_expr(&fname_str, None);
                let tmp = format_ident!("__nested_prefix_{}", fname);
                nested_emits.push(quote! {
                    let #tmp = #prefix;
                    <_ as crate::reloader::Reloadable>::diff_into(
                        &self.#fname, &new.#fname, report, &#tmp,
                    );
                });
            }
            FieldCfg::Skip => {}
        }
    }

    let group_emits = restart_groups.into_iter().map(|(label, conds)| {
        // conds is non-empty by construction.
        let mut iter = conds.into_iter();
        let first = iter.next().expect("at least one cond per group");
        let tail = iter;
        quote! {
            if #first #( || #tail )* {
                report.restart_required.push(::std::string::String::from(#label));
            }
        }
    });

    let has_path_prefix_use =
        !reloadable_emits.is_empty() || !restart_plain.is_empty() || !nested_emits.is_empty();
    let prefix_silencer = if has_path_prefix_use {
        quote!()
    } else {
        quote!(let _ = path_prefix;)
    };

    let expanded = quote! {
        impl #impl_gen crate::reloader::Reloadable for #name #ty_gen #where_gen {
            fn diff_into(
                &self,
                new: &Self,
                report: &mut crate::reloader::ApplyReport,
                path_prefix: &str,
            ) {
                #prefix_silencer
                #(#reloadable_emits)*
                #(#restart_plain)*
                #(#group_emits)*
                #(#nested_emits)*
            }
        }
    };

    expanded.into()
}
