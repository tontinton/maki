use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{DeriveInput, Expr, Fields, Ident, LitStr, Token, Type, parse_macro_input, parse_str};

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
    min: Option<Expr>,
    desc: Option<String>,
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

fn parse_field_attrs(field: &syn::Field) -> syn::Result<FieldAttrs> {
    let mut attrs = FieldAttrs {
        skip: false,
        default: None,
        min: None,
        desc: None,
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

fn config_value_expr(ty_name: &str, default: &Option<Expr>) -> TokenStream2 {
    match ty_name {
        "bool" => {
            let val = default.as_ref().expect("bool field requires default");
            quote! { ConfigValue::Bool(#val) }
        }
        "u32" => {
            let val = default.as_ref().expect("u32 field requires default");
            quote! { ConfigValue::U32(#val) }
        }
        "u64" => {
            let val = default.as_ref().expect("u64 field requires default");
            quote! { ConfigValue::U64(#val) }
        }
        "usize" => {
            let val = default.as_ref().expect("usize field requires default");
            quote! { ConfigValue::Usize(#val) }
        }
        "String" => quote! { ConfigValue::OptionalString },
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

    let fields = match &input.data {
        syn::Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            _ => return Err(syn::Error::new(name.span(), "only named fields supported")),
        },
        _ => return Err(syn::Error::new(name.span(), "only structs supported")),
    };

    let mut field_data = Vec::new();
    for field in fields {
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
        let default_expr = config_value_expr(ty_name, &attrs.default);
        let min_expr = match &attrs.min {
            Some(m) => quote! { Some(#m as u64) },
            None => quote! { None },
        };

        fields_entries.push(quote! {
            ConfigField {
                name: #field_name,
                ty: #ty_name,
                default: #default_expr,
                min: #min_expr,
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
