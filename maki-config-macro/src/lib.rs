use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    DeriveInput, Expr, Field, Fields, Ident, LitStr, Token, Type, parse_macro_input, parse_str,
};

struct ConfigAttr {
    key: Ident,
    value: ConfigAttrValue,
}

enum ConfigAttrValue {
    Flag,
    Str(LitStr),
    Expr(Box<Expr>),
}

impl Parse for ConfigAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let key: Ident = input.parse()?;
        if !input.peek(Token![=]) {
            return Ok(Self {
                key,
                value: ConfigAttrValue::Flag,
            });
        }
        input.parse::<Token![=]>()?;
        if input.peek(LitStr) {
            let lit: LitStr = input.parse()?;
            Ok(Self {
                key,
                value: ConfigAttrValue::Str(lit),
            })
        } else {
            let expr: Expr = input.parse()?;
            Ok(Self {
                key,
                value: ConfigAttrValue::Expr(Box::new(expr)),
            })
        }
    }
}

struct StructAttrs {
    section: String,
    fields_only: bool,
}

struct FieldAttrs {
    skip: bool,
    default: Option<Expr>,
    default_doc: Option<LitStr>,
    min: Option<Expr>,
    desc: Option<String>,
    env: Option<String>,
    key: Option<String>,
    ty_override: Option<String>,
    val: Option<String>,
}

fn parse_struct_attrs(input: &DeriveInput) -> syn::Result<StructAttrs> {
    let mut section = None;
    let mut fields_only = false;

    for attr in &input.attrs {
        if !attr.path().is_ident("config") {
            continue;
        }
        let nested = attr.parse_args_with(Punctuated::<ConfigAttr, Token![,]>::parse_terminated)?;
        for item in nested {
            match item.key.to_string().as_str() {
                "section" => {
                    if let ConfigAttrValue::Str(lit) = item.value {
                        section = Some(lit.value());
                    }
                }
                "fields_only" => fields_only = true,
                other => {
                    return Err(syn::Error::new(
                        item.key.span(),
                        format!("unknown struct-level config attribute: {other}"),
                    ));
                }
            }
        }
    }

    let section = section.ok_or_else(|| {
        syn::Error::new(input.ident.span(), "missing #[config(section = \"...\")]")
    })?;

    Ok(StructAttrs {
        section,
        fields_only,
    })
}

fn parse_field_attrs(field: &Field) -> syn::Result<FieldAttrs> {
    let mut attrs = FieldAttrs {
        skip: false,
        default: None,
        default_doc: None,
        min: None,
        desc: None,
        env: None,
        key: None,
        ty_override: None,
        val: None,
    };

    for attr in &field.attrs {
        if !attr.path().is_ident("config") {
            continue;
        }
        let nested = attr.parse_args_with(Punctuated::<ConfigAttr, Token![,]>::parse_terminated)?;
        for item in nested {
            match item.key.to_string().as_str() {
                "skip" => attrs.skip = true,
                "default" => match item.value {
                    ConfigAttrValue::Str(lit) => {
                        attrs.default = Some(parse_str(&lit.value())?);
                    }
                    ConfigAttrValue::Expr(expr) => attrs.default = Some(*expr),
                    ConfigAttrValue::Flag => {
                        return Err(syn::Error::new(item.key.span(), "default requires a value"));
                    }
                },
                "default_doc" => {
                    if let ConfigAttrValue::Str(lit) = item.value {
                        attrs.default_doc = Some(lit);
                    }
                }
                "min" => {
                    if let ConfigAttrValue::Expr(expr) = item.value {
                        attrs.min = Some(*expr);
                    }
                }
                "desc" => {
                    if let ConfigAttrValue::Str(lit) = item.value {
                        attrs.desc = Some(lit.value());
                    }
                }
                "env" => {
                    if let ConfigAttrValue::Str(lit) = item.value {
                        attrs.env = Some(lit.value());
                    }
                }
                "key" => {
                    if let ConfigAttrValue::Str(lit) = item.value {
                        attrs.key = Some(lit.value());
                    }
                }
                "ty" => {
                    if let ConfigAttrValue::Str(lit) = item.value {
                        attrs.ty_override = Some(lit.value());
                    }
                }
                "val" => {
                    if let ConfigAttrValue::Str(lit) = item.value {
                        attrs.val = Some(lit.value());
                    }
                }
                other => {
                    return Err(syn::Error::new(
                        item.key.span(),
                        format!("unknown field-level config attribute: {other}"),
                    ));
                }
            }
        }
    }

    Ok(attrs)
}

enum MergeStrategy {
    Override,
    Nested,
    NestedOption,
    With(Box<Expr>),
}

fn parse_merge_strategy(field: &Field) -> syn::Result<MergeStrategy> {
    let mut strategy = MergeStrategy::Override;
    for attr in field
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("merge"))
    {
        let item: ConfigAttr = attr.parse_args()?;
        strategy = match (item.key.to_string().as_str(), item.value) {
            ("nested", ConfigAttrValue::Flag) => MergeStrategy::Nested,
            ("nested_option", ConfigAttrValue::Flag) => MergeStrategy::NestedOption,
            ("with", ConfigAttrValue::Expr(path)) => MergeStrategy::With(path),
            _ => {
                return Err(syn::Error::new(
                    item.key.span(),
                    "expected `nested`, `nested_option` or `with = path`",
                ));
            }
        };
    }
    Ok(strategy)
}

fn named_fields(input: &DeriveInput) -> syn::Result<&Punctuated<Field, Token![,]>> {
    let name = &input.ident;
    match &input.data {
        syn::Data::Struct(data) => match &data.fields {
            Fields::Named(named) => Ok(&named.named),
            _ => Err(syn::Error::new(name.span(), "only named fields supported")),
        },
        _ => Err(syn::Error::new(name.span(), "only structs supported")),
    }
}

/// Generates `merge(&mut self, overlay: Self)`. By default a field must be an
/// `Option` and the overlay wins when it is `Some`, so a new field can never
/// be silently left out of the merge. `#[merge(nested)]` merges the field
/// recursively, `#[merge(nested_option)]` does so inside an `Option`, and
/// `#[merge(with = path)]` calls `path(&mut self.field, overlay.field)`.
#[proc_macro_derive(Merge, attributes(merge))]
pub fn derive_merge(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match merge_impl(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn merge_impl(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let mut steps = Vec::new();
    for field in named_fields(input)? {
        let ident = field.ident.as_ref().unwrap();
        steps.push(match parse_merge_strategy(field)? {
            MergeStrategy::Override => quote! {
                if overlay.#ident.is_some() {
                    self.#ident = overlay.#ident;
                }
            },
            MergeStrategy::Nested => quote! { self.#ident.merge(overlay.#ident); },
            MergeStrategy::NestedOption => quote! {
                match (self.#ident.as_mut(), overlay.#ident) {
                    (Some(base), Some(over)) => base.merge(over),
                    (None, Some(over)) => self.#ident = Some(over),
                    _ => {}
                }
            },
            MergeStrategy::With(path) => quote! { #path(&mut self.#ident, overlay.#ident); },
        });
    }
    Ok(quote! {
        impl #impl_generics #name #ty_generics #where_clause {
            pub fn merge(&mut self, overlay: Self) {
                #(#steps)*
            }
        }
    })
}

fn config_value_expr(ty_name: &str, default: &Option<Expr>) -> TokenStream2 {
    match ty_name {
        "bool" => {
            let val = default.as_ref().expect("bool field requires default");
            quote! { ConfigValue::Bool(#val) }
        }
        "u32" | "u64" | "usize" => {
            let val = default.as_ref().expect("numeric field requires default");
            quote! { ConfigValue::U64(#val as u64) }
        }
        "String" => quote! { ConfigValue::Str("none") },
        other => panic!("unsupported config type: {other}"),
    }
}

fn type_to_name(ty: &Type) -> String {
    quote!(#ty).to_string().replace(' ', "")
}

#[proc_macro_derive(ConfigSection, attributes(config))]
pub fn derive_config_section(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match derive_impl(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn derive_impl(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let struct_attrs = parse_struct_attrs(input)?;
    let name = &input.ident;

    let mut field_data = Vec::new();
    for field in named_fields(input)? {
        let ident = field.ident.as_ref().unwrap();
        let attrs = parse_field_attrs(field)?;
        let ty = &field.ty;
        field_data.push((ident.clone(), attrs, ty.clone()));
    }

    let section = &struct_attrs.section;

    let mut fields_entries = Vec::new();
    let mut validate_checks = Vec::new();

    for (ident, attrs, ty) in &field_data {
        if attrs.skip {
            continue;
        }

        let ident_string = ident.to_string();
        let field_name = attrs.key.as_deref().unwrap_or(&ident_string);
        let ty_string = type_to_name(ty);
        let ty_name = attrs.ty_override.as_deref().unwrap_or(&ty_string);
        let desc = attrs.desc.as_deref().unwrap_or("");
        let default_expr = match &attrs.default_doc {
            Some(doc) => quote! { ConfigValue::Str(#doc) },
            None => config_value_expr(ty_name, &attrs.default),
        };
        let min_expr = match &attrs.min {
            Some(m) => quote! { Some(#m as u64) },
            None => quote! { None },
        };
        let env_expr = match &attrs.env {
            Some(e) => quote! { Some(#e) },
            None => quote! { None },
        };

        fields_entries.push(quote! {
            ConfigField {
                name: #field_name,
                ty: #ty_name,
                default: #default_expr,
                min: #min_expr,
                env: #env_expr,
                description: #desc,
            }
        });

        if let Some(min) = &attrs.min {
            let val_expr: TokenStream2 = if let Some(val_str) = &attrs.val {
                val_str.parse().map_err(|e| {
                    syn::Error::new(ident.span(), format!("invalid val expression: {e}"))
                })?
            } else {
                quote! { self.#ident }
            };
            let ident_str = ident.to_string();
            let field_name_for_check = attrs.key.as_deref().unwrap_or(&ident_str);
            validate_checks.push(quote! {
                check(#section, #field_name_for_check, #val_expr as u64, #min as u64)?;
            });
        }
    }

    let fields_const = quote! {
        pub const FIELDS: &[ConfigField] = &[
            #(#fields_entries),*
        ];
    };

    let validate_fn = quote! {
        pub fn validate(&self) -> Result<(), ConfigError> {
            #(#validate_checks)*
            Ok(())
        }
    };

    if struct_attrs.fields_only {
        Ok(quote! {
            impl #name {
                #fields_const
                #validate_fn
            }
        })
    } else {
        let mut default_fields = Vec::new();
        for (ident, attrs, _ty) in &field_data {
            let default = attrs.default.as_ref().unwrap_or_else(|| {
                panic!("field `{}` requires a default value in full mode", ident)
            });
            default_fields.push(quote! { #ident: #default });
        }

        Ok(quote! {
            impl Default for #name {
                fn default() -> Self {
                    Self {
                        #(#default_fields),*
                    }
                }
            }

            impl #name {
                #fields_const
                #validate_fn
            }
        })
    }
}
