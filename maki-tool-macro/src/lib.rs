//! Derive macros for tool schemas.
//!
//! The parameter shape lives in `ParamSchema` over in `maki_agent::tools::schema`.
//! We let serde do the last mile (defaults, renames, tagged enums) so this crate
//! only needs to build the schema, not reimplement serde's field logic.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Attribute, Data, DeriveInput, Expr, Fields, GenericArgument, Lit, Meta, PathArguments, Type,
    parse_macro_input,
};

fn param_description(attrs: &[Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| {
        if !attr.path().is_ident("param") {
            return None;
        }
        let nested: Meta = attr.parse_args().ok()?;
        if let Meta::NameValue(nv) = nested
            && nv.path.is_ident("description")
            && let Expr::Lit(expr_lit) = &nv.value
            && let Lit::Str(lit) = &expr_lit.lit
        {
            return Some(lit.value());
        }
        None
    })
}

fn inner_type<'a>(ty: &'a Type, wrapper: &str) -> Option<&'a Type> {
    if let Type::Path(tp) = ty
        && let Some(seg) = tp.path.segments.last()
        && seg.ident == wrapper
        && let PathArguments::AngleBracketed(args) = &seg.arguments
        && let Some(GenericArgument::Type(inner)) = args.args.first()
    {
        return Some(inner);
    }
    None
}

fn is_option(ty: &Type) -> bool {
    inner_type(ty, "Option").is_some()
}

fn unwrapped_type(ty: &Type) -> &Type {
    inner_type(ty, "Option").unwrap_or(ty)
}

fn has_serde_default(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("serde") {
            return false;
        }
        let Ok(nested) = attr.parse_args::<Meta>() else {
            return false;
        };
        matches!(nested, Meta::Path(p) if p.is_ident("default"))
    })
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Primitive {
    String,
    Bool,
    Integer,
    Number,
}

fn primitive_of(ty: &Type) -> Option<Primitive> {
    if let Type::Path(tp) = ty
        && let Some(seg) = tp.path.segments.last()
    {
        return match seg.ident.to_string().as_str() {
            "String" | "str" => Some(Primitive::String),
            "bool" => Some(Primitive::Bool),
            "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64"
            | "i128" | "isize" => Some(Primitive::Integer),
            "f32" | "f64" => Some(Primitive::Number),
            _ => None,
        };
    }
    None
}

fn is_value_type(ty: &Type) -> bool {
    if let Type::Path(tp) = ty
        && let Some(seg) = tp.path.segments.last()
    {
        return seg.ident == "Value";
    }
    false
}

/// `Option<T>` is only peeled at the top level of a field, not inside
/// containers, so serde can still round-trip optional items.
fn schema_ref(ty: &Type, description: &str, unwrap_option: bool) -> TokenStream2 {
    let inner = if unwrap_option {
        unwrapped_type(ty)
    } else {
        ty
    };

    if is_value_type(inner) {
        return quote! {
            &crate::tools::schema::ParamSchema::Any { description: #description }
        };
    }

    if let Some(prim) = primitive_of(inner) {
        let kind = primitive_kind_token(prim);
        return quote! {
            &crate::tools::schema::ParamSchema::Primitive {
                kind: #kind,
                description: #description,
            }
        };
    }

    if let Some(item_ty) = inner_type(inner, "Vec") {
        let item_schema = schema_ref(item_ty, "", false);
        return quote! {
            &crate::tools::schema::ParamSchema::Array {
                items: #item_schema,
                description: #description,
            }
        };
    }

    quote! { #inner::ITEM_SCHEMA }
}

fn primitive_kind_token(prim: Primitive) -> TokenStream2 {
    match prim {
        Primitive::String => quote! { crate::tools::schema::ParamKind::String },
        Primitive::Bool => quote! { crate::tools::schema::ParamKind::Bool },
        Primitive::Integer => quote! { crate::tools::schema::ParamKind::Integer },
        Primitive::Number => quote! { crate::tools::schema::ParamKind::Number },
    }
}

fn object_property_tokens(fields: &syn::FieldsNamed) -> Vec<TokenStream2> {
    fields
        .named
        .iter()
        .map(|field| {
            let field_name = field.ident.as_ref().unwrap();
            let field_str = field_name.to_string();
            let desc = param_description(&field.attrs).unwrap_or_default();
            let schema = schema_ref(&field.ty, &desc, true);
            let required = !(is_option(&field.ty) || has_serde_default(&field.attrs));
            quote! {
                (
                    #field_str,
                    #schema,
                    #required,
                )
            }
        })
        .collect()
}

#[proc_macro_derive(ArgEnum)]
pub fn derive_arg_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let Data::Enum(data) = &input.data else {
        return syn::Error::new_spanned(name, "ArgEnum can only be derived on enums")
            .to_compile_error()
            .into();
    };

    let variants: Vec<String> = data
        .variants
        .iter()
        .map(|v| to_snake_case(&v.ident.to_string()))
        .collect();

    let expanded = quote! {
        impl #name {
            pub(crate) const ITEM_SCHEMA: &'static crate::tools::schema::ParamSchema =
                &crate::tools::schema::ParamSchema::Enum {
                    variants: &[#(#variants),*],
                    description: "",
                };
        }
    };
    expanded.into()
}

#[proc_macro_derive(Args, attributes(param))]
pub fn derive_args(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let Data::Struct(data) = &input.data else {
        return syn::Error::new_spanned(name, "Args can only be derived on structs")
            .to_compile_error()
            .into();
    };
    let Fields::Named(fields) = &data.fields else {
        return syn::Error::new_spanned(name, "Args requires named fields")
            .to_compile_error()
            .into();
    };

    let props = object_property_tokens(fields);

    let expanded = quote! {
        impl #name {
            pub(crate) const ITEM_SCHEMA: &'static crate::tools::schema::ParamSchema =
                &crate::tools::schema::ParamSchema::Object {
                    properties: &[#(#props),*],
                    description: "",
                };
        }
    };
    expanded.into()
}

#[proc_macro_derive(Tool, attributes(param))]
pub fn derive_tool(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let Data::Struct(data) = &input.data else {
        return syn::Error::new_spanned(name, "Tool can only be derived on structs")
            .to_compile_error()
            .into();
    };
    let Fields::Named(fields) = &data.fields else {
        return syn::Error::new_spanned(name, "Tool requires named fields")
            .to_compile_error()
            .into();
    };

    let props = object_property_tokens(fields);

    let expanded = quote! {
        impl #name {
            pub(crate) const SCHEMA: &'static crate::tools::schema::ParamSchema =
                &crate::tools::schema::ParamSchema::Object {
                    properties: &[#(#props),*],
                    description: "",
                };

            pub(crate) fn schema() -> serde_json::Value {
                crate::tools::schema::to_json_schema(Self::SCHEMA)
            }

            pub(crate) fn parse_input(
                input: &serde_json::Value,
            ) -> Result<Self, crate::tools::schema::ToolInputError> {
                let sanitized = crate::tools::sanitize_tool_input(input);
                crate::tools::schema::validate_and_deserialize(Self::SCHEMA, sanitized)
            }
        }
    };

    expanded.into()
}
