//! `#[derive(Reloadable)]` — generates [`Reloadable`] diff logic
//! for config structs so the reloadable-field list can't silently
//! drift when a new field lands. Every field must be classified
//! (`#[reloadable]`, `#[restart_required]` or `#[nested]`); an
//! unmarked field is a compile error.
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

/// Classify one field. Every field of a `Reloadable` struct must
/// carry exactly one of the three attributes — an unmarked field is
/// a compile error so a new config knob can never be silently
/// ignored by the hot-reload diff.
fn classify(field: &syn::Field) -> Result<FieldCfg, syn::Error> {
    for attr in &field.attrs {
        if attr.path().is_ident("reloadable") {
            let path = match &attr.meta {
                Meta::List(_) => name_value_string(attr, "path"),
                Meta::Path(_) | Meta::NameValue(_) => None,
            };
            return Ok(FieldCfg::Reloadable { path });
        }
        if attr.path().is_ident("restart_required") {
            let group = match &attr.meta {
                Meta::List(_) => name_value_string(attr, "group"),
                Meta::Path(_) | Meta::NameValue(_) => None,
            };
            return Ok(FieldCfg::RestartRequired { group });
        }
        if attr.path().is_ident("nested") {
            return Ok(FieldCfg::Nested);
        }
    }
    Err(syn::Error::new_spanned(
        field,
        "Reloadable: every field needs a hot-reload classification — \
         add #[reloadable], #[restart_required] or #[nested]",
    ))
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

/// The four emit buckets one struct's fields sort into.
struct Buckets {
    reloadable_emits: Vec<TokenStream2>,
    restart_plain: Vec<TokenStream2>,
    /// Restart-required fields that share a `group` label, keyed by
    /// it, so several fields can collapse to one report entry.
    restart_groups: BTreeMap<String, Vec<TokenStream2>>,
    nested_emits: Vec<TokenStream2>,
}

/// Sort every field into its emit bucket by attribute.
fn collect_buckets(
    fields: &syn::punctuated::Punctuated<syn::Field, syn::Token![,]>,
) -> syn::Result<Buckets> {
    let mut b = Buckets {
        reloadable_emits: Vec::new(),
        restart_plain: Vec::new(),
        restart_groups: BTreeMap::new(),
        nested_emits: Vec::new(),
    };
    for field in fields {
        let fname = field.ident.as_ref().expect("named field");
        let fname_str = fname.to_string();
        match classify(field)? {
            FieldCfg::Reloadable { path } => {
                let path_expr = path_expr(&fname_str, path.as_deref());
                b.reloadable_emits.push(quote! {
                    if self.#fname != new.#fname {
                        report.reloaded.push(#path_expr);
                    }
                });
            }
            FieldCfg::RestartRequired { group: Some(label) } => {
                b.restart_groups
                    .entry(label)
                    .or_default()
                    .push(quote!( self.#fname != new.#fname ));
            }
            FieldCfg::RestartRequired { group: None } => {
                let path_expr = path_expr(&fname_str, None);
                b.restart_plain.push(quote! {
                    if self.#fname != new.#fname {
                        report.restart_required.push(#path_expr);
                    }
                });
            }
            FieldCfg::Nested => {
                let prefix = path_expr(&fname_str, None);
                let tmp = format_ident!("__nested_prefix_{}", fname);
                b.nested_emits.push(quote! {
                    let #tmp = #prefix;
                    <_ as crate::reloader::Reloadable>::diff_into(
                        &self.#fname, &new.#fname, report, &#tmp,
                    );
                });
            }
        }
    }
    Ok(b)
}

/// One `if a != a' || b != b' { report.push(label) }` per group.
fn group_emits(groups: BTreeMap<String, Vec<TokenStream2>>) -> Vec<TokenStream2> {
    groups
        .into_iter()
        .map(|(label, conds)| {
            // `conds` is non-empty by construction.
            let mut iter = conds.into_iter();
            let first = iter.next().expect("at least one cond per group");
            let tail = iter;
            quote! {
                if #first #( || #tail )* {
                    report.restart_required.push(::std::string::String::from(#label));
                }
            }
        })
        .collect()
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

    let Buckets {
        reloadable_emits,
        restart_plain,
        restart_groups,
        nested_emits,
    } = match collect_buckets(fields) {
        Ok(b) => b,
        Err(e) => return e.to_compile_error().into(),
    };
    let group_emits = group_emits(restart_groups);

    let has_path_prefix_use =
        !reloadable_emits.is_empty() || !restart_plain.is_empty() || !nested_emits.is_empty();
    let prefix_silencer = if has_path_prefix_use {
        quote!()
    } else {
        quote!(let _ = path_prefix;)
    };

    let expanded = quote! {
           impl #impl_gen crate::reloader::Reloadable for #name #ty_gen #where_gen {
    // A config diff asks "did the operator write a different
    // value?", so exact inequality is the intended test even
    // for float-valued knobs.
               #[allow(clippy::float_cmp)]
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
