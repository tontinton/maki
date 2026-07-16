//! Proc macros keeping `maki-lua` API registration and docs in one place.
//!
//! `#[lua_fn]` turns a doc-commented Rust fn into a Lua function plus its
//! `FnDoc`: the Lua name comes from the fn ident, the argument list from the
//! signature, and `@param` / `@return` tags are validated against the real
//! parameters at compile time. `lua_table!` assembles the registration fn and
//! the `ModuleDoc` const from one list, so nothing can drift.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    Attribute, Expr, FnArg, Ident, ItemFn, Lit, LitStr, Meta, Pat, PatType, Path, Token, Type,
    Visibility, parse_macro_input,
};

fn doc_lines(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .filter_map(|a| match &a.meta {
            Meta::NameValue(nv) => match &nv.value {
                Expr::Lit(l) => match &l.lit {
                    Lit::Str(s) => {
                        let v = s.value();
                        Some(v.strip_prefix(' ').unwrap_or(&v).to_owned())
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}

struct ParsedDoc {
    desc: String,
    params: Vec<(String, String, String)>,
    returns: String,
    example: String,
}

fn parse_doc(lines: &[String], span: proc_macro2::Span) -> syn::Result<ParsedDoc> {
    let mut desc_parts: Vec<&str> = Vec::new();
    let mut params: Vec<(String, String, String)> = Vec::new();
    let mut returns = String::new();
    let mut example = String::new();
    #[derive(PartialEq)]
    enum State {
        Desc,
        Param,
        Return,
        Example,
    }
    let mut state = State::Desc;
    for line in lines {
        if let Some(rest) = line.strip_prefix("@param ") {
            let mut it = rest.splitn(3, ' ');
            let (name, ty, d) = (
                it.next().unwrap_or_default(),
                it.next().unwrap_or_default(),
                it.next().unwrap_or_default(),
            );
            if name.is_empty() || ty.is_empty() {
                return Err(syn::Error::new(
                    span,
                    format!("malformed @param line: `{line}`"),
                ));
            }
            params.push((name.to_owned(), ty.to_owned(), d.to_owned()));
            state = State::Param;
        } else if let Some(rest) = line.strip_prefix("@return") {
            returns = rest.trim_start().to_owned();
            state = State::Return;
        } else if let Some(rest) = line.strip_prefix("@example") {
            example = rest.trim_start().to_owned();
            state = State::Example;
        } else {
            match state {
                State::Desc => {
                    if !(desc_parts.is_empty() && line.trim().is_empty()) {
                        desc_parts.push(line.trim_end());
                    }
                }
                State::Param => {
                    let d = &mut params.last_mut().unwrap().2;
                    if !d.is_empty() {
                        d.push('\n');
                    }
                    d.push_str(line.trim_end());
                }
                State::Return => {
                    if !returns.is_empty() {
                        returns.push('\n');
                    }
                    returns.push_str(line.trim_end());
                }
                State::Example => {
                    if !(example.is_empty() && line.trim().is_empty()) {
                        if !example.is_empty() {
                            example.push('\n');
                        }
                        example.push_str(line.trim_end());
                    }
                }
            }
        }
    }
    let desc = desc_parts.join("\n");
    let desc = desc.trim().to_owned();
    let returns = returns.trim().to_owned();
    let example = example.trim().to_owned();
    if desc.is_empty() {
        return Err(syn::Error::new(
            span,
            "lua_fn requires a doc comment description",
        ));
    }
    for (_, _, d) in &mut params {
        *d = d.trim().to_owned();
    }
    Ok(ParsedDoc {
        desc,
        params,
        returns,
        example,
    })
}

fn last_segment_is(ty: &Type, name: &str) -> bool {
    match ty {
        Type::Path(p) => p.path.segments.last().is_some_and(|s| s.ident == name),
        Type::Reference(r) => last_segment_is(&r.elem, name),
        _ => false,
    }
}

struct LuaFnArgs {
    guard: Option<Path>,
    name: Option<String>,
}

fn parse_lua_fn_args(attr: TokenStream) -> syn::Result<LuaFnArgs> {
    let metas = syn::parse::Parser::parse(Punctuated::<Meta, Token![,]>::parse_terminated, attr)?;
    let mut out = LuaFnArgs {
        guard: None,
        name: None,
    };
    for meta in metas {
        match &meta {
            Meta::NameValue(nv) if nv.path.is_ident("guard") => match &nv.value {
                Expr::Path(p) => out.guard = Some(p.path.clone()),
                _ => {
                    return Err(syn::Error::new(
                        nv.value.span(),
                        "guard expects a Permission variant",
                    ));
                }
            },
            Meta::NameValue(nv) if nv.path.is_ident("name") => match &nv.value {
                Expr::Lit(l) => match &l.lit {
                    Lit::Str(s) => out.name = Some(s.value()),
                    _ => return Err(syn::Error::new(l.span(), "name expects a string literal")),
                },
                _ => {
                    return Err(syn::Error::new(
                        nv.value.span(),
                        "name expects a string literal",
                    ));
                }
            },
            other => return Err(syn::Error::new(other.span(), "unknown lua_fn option")),
        }
    }
    Ok(out)
}

/// See crate docs. Options: `guard = <Permission variant>`, `name = "<lua name>"`.
#[proc_macro_attribute]
pub fn lua_fn(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = match parse_lua_fn_args(attr) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error().into(),
    };
    let mut func = parse_macro_input!(item as ItemFn);
    match expand_lua_fn(args, &mut func) {
        Ok(ts) => ts.into(),
        Err(e) => {
            let err = e.to_compile_error();
            quote!(#func #err).into()
        }
    }
}

fn expand_lua_fn(args: LuaFnArgs, func: &mut ItemFn) -> syn::Result<TokenStream2> {
    let fn_ident = func.sig.ident.clone();
    let span = fn_ident.span();
    let is_async = func.sig.asyncness.is_some();
    let doc = parse_doc(&doc_lines(&func.attrs), span)?;

    let mut inputs = func.sig.inputs.iter_mut();
    let Some(FnArg::Typed(_lua_param)) = inputs.next() else {
        return Err(syn::Error::new(
            span,
            "first parameter must be the Lua handle",
        ));
    };
    let mut method: Option<MethodKind> = None;
    let mut ctx: Vec<(Ident, Type)> = Vec::new();
    let mut real: Vec<(Ident, Type, bool, bool)> = Vec::new();
    for input in inputs {
        let FnArg::Typed(pt) = input else {
            return Err(syn::Error::new(
                span,
                "lua_fn does not support self parameters",
            ));
        };
        let is_ctx = pt.attrs.iter().any(|a| a.path().is_ident("ctx"));
        pt.attrs.retain(|a| !a.path().is_ident("ctx"));
        let Pat::Ident(pi) = &*pt.pat else {
            return Err(syn::Error::new(
                pt.pat.span(),
                "lua_fn parameters must be plain identifiers",
            ));
        };
        let ident = pi.ident.clone();
        let ty = (*pt.ty).clone();
        if ident == "this" {
            if method.is_some() || is_ctx || !ctx.is_empty() || !real.is_empty() {
                return Err(syn::Error::new(
                    ident.span(),
                    "`this` must be the single parameter right after the Lua handle",
                ));
            }
            method = Some(method_kind(&ty, is_async)?);
            continue;
        }
        if is_ctx {
            if !real.is_empty() {
                return Err(syn::Error::new(
                    ident.span(),
                    "#[ctx] parameters must come before Lua parameters",
                ));
            }
            ctx.push((ident, ty));
        } else {
            let optional = last_segment_is(&ty, "Option");
            let variadic = last_segment_is(&ty, "Variadic");
            real.push((ident, ty, optional, variadic));
        }
    }

    let documented: Vec<&str> = doc.params.iter().map(|(n, _, _)| n.as_str()).collect();
    let actual: Vec<String> = real.iter().map(|(i, ..)| i.unraw().to_string()).collect();
    if documented != actual.iter().map(String::as_str).collect::<Vec<_>>() {
        return Err(syn::Error::new(
            span,
            format!(
                "@param tags {documented:?} do not match parameters {actual:?} (same names, same order)"
            ),
        ));
    }

    let rendered: Vec<String> = real
        .iter()
        .map(|(ident, _, optional, variadic)| {
            let ident = ident.unraw();
            if *variadic {
                "{...}".to_owned()
            } else if *optional {
                format!("{{{ident}?}}")
            } else {
                format!("{{{ident}}}")
            }
        })
        .collect();
    let args_str = rendered.join(", ");
    let lua_name = args.name.unwrap_or_else(|| fn_ident.unraw().to_string());
    let doc_ident = format_ident!("{fn_ident}__doc");
    let reg_ident = format_ident!("{fn_ident}__register");

    let desc = &doc.desc;
    let returns = &doc.returns;
    let example = &doc.example;
    let param_docs =
        doc.params.iter().zip(&rendered).map(
            |((_, ty, d), name)| quote!(crate::docs::ParamDoc { name: #name, ty: #ty, desc: #d }),
        );

    let ctx_idents: Vec<&Ident> = ctx.iter().map(|(i, _)| i).collect();
    let ctx_tys: Vec<&Type> = ctx.iter().map(|(_, t)| t).collect();
    // Fresh binding names so closure params can never shadow the target fn.
    let arg_idents: Vec<Ident> = (0..real.len()).map(|i| format_ident!("__arg{i}")).collect();
    let real_tys: Vec<&Type> = real.iter().map(|(_, t, ..)| t).collect();

    let call = quote!(#fn_ident(lua #(, #ctx_idents.clone())* #(, #arg_idents)*));
    let closure = if real.is_empty() {
        quote!(move |lua, ()| #call)
    } else {
        quote!(move |lua, (#(#arg_idents,)*): (#(#real_tys,)*)| #call)
    };
    let doc_const = quote! {
        #[allow(non_upper_case_globals)]
        pub(crate) const #doc_ident: crate::docs::FnDoc = crate::docs::FnDoc {
            name: #lua_name,
            args: #args_str,
            desc: #desc,
            params: &[#(#param_docs),*],
            returns: #returns,
            example: #example,
        };
    };

    if let Some(kind) = method {
        if args.guard.is_some() || !ctx.is_empty() {
            return Err(syn::Error::new(
                span,
                "guard and #[ctx] are not supported on methods",
            ));
        }
        let MethodKind { self_ty, add_fn } = kind;
        let closure = if real.is_empty() {
            quote!(move |lua, this, ()| #fn_ident(lua, this))
        } else {
            quote!(move |lua, this, (#(#arg_idents,)*): (#(#real_tys,)*)| #fn_ident(lua, this #(, #arg_idents)*))
        };
        return Ok(quote! {
            #func
            #doc_const
            pub(crate) fn #reg_ident<M: mlua::UserDataMethods<#self_ty>>(methods: &mut M) {
                methods.#add_fn(#lua_name, #closure);
            }
        });
    }

    let create = match (&args.guard, is_async) {
        (Some(g), false) => {
            quote!(perms.guard(crate::plugin_permissions::Permission::#g, lua, #closure)?)
        }
        (Some(g), true) => {
            quote!(perms.guard_async(crate::plugin_permissions::Permission::#g, lua, #closure)?)
        }
        (None, false) => quote!(lua.create_function(#closure)?),
        (None, true) => quote!(lua.create_async_function(#closure)?),
    };
    let perms_param = args
        .guard
        .is_some()
        .then(|| quote!(, perms: &crate::plugin_permissions::PluginPermissions));

    Ok(quote! {
        #func
        #doc_const
        pub(crate) fn #reg_ident(
            t: &mlua::Table,
            lua: &mlua::Lua
            #perms_param
            #(, #ctx_idents: #ctx_tys)*
        ) -> mlua::Result<()> {
            t.set(#lua_name, #create)
        }
    })
}

struct MethodKind {
    self_ty: Type,
    add_fn: Ident,
}

fn method_kind(ty: &Type, is_async: bool) -> syn::Result<MethodKind> {
    let (self_ty, add_fn) = match ty {
        Type::Reference(r) if !is_async => (
            (*r.elem).clone(),
            if r.mutability.is_some() {
                "add_method_mut"
            } else {
                "add_method"
            },
        ),
        Type::Path(p) if is_async => {
            let seg = p.path.segments.last().unwrap();
            let add_fn = match seg.ident.to_string().as_str() {
                "UserDataRef" => "add_async_method",
                "UserDataRefMut" => "add_async_method_mut",
                _ => {
                    return Err(syn::Error::new(
                        ty.span(),
                        "async `this` must be UserDataRef<T> or UserDataRefMut<T>",
                    ));
                }
            };
            let syn::PathArguments::AngleBracketed(ab) = &seg.arguments else {
                return Err(syn::Error::new(ty.span(), "missing userdata type argument"));
            };
            let Some(syn::GenericArgument::Type(inner)) = ab.args.first() else {
                return Err(syn::Error::new(ty.span(), "missing userdata type argument"));
            };
            (inner.clone(), add_fn)
        }
        _ => {
            return Err(syn::Error::new(
                ty.span(),
                "`this` must be &T / &mut T (sync) or UserDataRef<T> / UserDataRefMut<T> (async)",
            ));
        }
    };
    Ok(MethodKind {
        self_ty,
        add_fn: Ident::new(add_fn, ty.span()),
    })
}

struct Entry {
    manual: bool,
    ident: Ident,
    args: Vec<Ident>,
}

impl Parse for Entry {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut manual = false;
        if input.peek(Ident) && input.peek2(Ident) {
            let kw: Ident = input.parse()?;
            if kw != "manual" {
                return Err(syn::Error::new(
                    kw.span(),
                    "expected `manual` or a function name",
                ));
            }
            manual = true;
        }
        let ident: Ident = input.parse()?;
        let mut args = Vec::new();
        if input.peek(syn::token::Paren) {
            let inner;
            syn::parenthesized!(inner in input);
            args = Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?
                .into_iter()
                .collect();
        }
        Ok(Self {
            manual,
            ident,
            args,
        })
    }
}

struct LuaTableInput {
    desc: String,
    extend: bool,
    lua_name: LitStr,
    vis: Visibility,
    fn_ident: Ident,
    params: Vec<PatType>,
    docs_ident: Ident,
    entries: Vec<Entry>,
}

impl Parse for LuaTableInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let desc = doc_lines(&attrs).join("\n").trim().to_owned();
        let extend = if input.peek(Ident) {
            let kw: Ident = input.parse()?;
            if kw != "extend" {
                return Err(syn::Error::new(
                    kw.span(),
                    "expected `extend` or a module name string",
                ));
            }
            true
        } else {
            false
        };
        let lua_name: LitStr = input.parse()?;
        input.parse::<Token![=>]>()?;
        let vis: Visibility = input.parse()?;
        input.parse::<Token![fn]>()?;
        let fn_ident: Ident = input.parse()?;
        let inner;
        syn::parenthesized!(inner in input);
        let params = Punctuated::<FnArg, Token![,]>::parse_terminated(&inner)?
            .into_iter()
            .map(|a| match a {
                FnArg::Typed(pt) => Ok(pt),
                FnArg::Receiver(r) => Err(syn::Error::new(r.span(), "self not allowed here")),
            })
            .collect::<syn::Result<Vec<_>>>()?;
        input.parse::<Token![,]>()?;
        let docs_ident: Ident = input.parse()?;
        let inner;
        syn::bracketed!(inner in input);
        let entries = Punctuated::<Entry, Token![,]>::parse_terminated(&inner)?
            .into_iter()
            .collect();
        Ok(Self {
            desc,
            extend,
            lua_name,
            vis,
            fn_ident,
            params,
            docs_ident,
            entries,
        })
    }
}

/// `lua_table! { /// desc … "maki.x" => pub(crate) fn create_x_table(perms: &PluginPermissions), DOCS [a(perms), b, manual c] }`
///
/// Generates the `ModuleDoc` const and the registration fn from one entry
/// list. `manual` entries are documented but registered by hand. Prefix with
/// `extend` to register into an existing table instead of creating one.
#[proc_macro]
pub fn lua_table(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as LuaTableInput);
    let LuaTableInput {
        desc,
        extend,
        lua_name,
        vis,
        fn_ident,
        params,
        docs_ident,
        entries,
    } = input;

    let doc_idents = entries.iter().map(|e| format_ident!("{}__doc", e.ident));
    let calls = entries.iter().filter(|e| !e.manual).map(|e| {
        let reg = format_ident!("{}__register", e.ident);
        let forwarded = e.args.iter().map(|arg| {
            let by_ref = params.iter().any(|p| {
                matches!(&*p.pat, Pat::Ident(pi) if pi.ident == *arg)
                    && matches!(&*p.ty, Type::Reference(_))
            });
            if by_ref {
                quote!(#arg)
            } else {
                quote!(#arg.clone())
            }
        });
        quote!(#reg(&t, lua #(, #forwarded)*)?;)
    });

    let docs = quote! {
        pub(crate) const #docs_ident: crate::docs::ModuleDoc = crate::docs::ModuleDoc {
            name: #lua_name,
            kind: crate::docs::DocKind::Table,
            desc: #desc,
            fns: &[#(#doc_idents),*],
        };
    };
    let body = if extend {
        quote! {
            #vis fn #fn_ident(t: &mlua::Table, lua: &mlua::Lua #(, #params)*) -> mlua::Result<()> {
                let t = t.clone();
                #(#calls)*
                Ok(())
            }
        }
    } else {
        quote! {
            #vis fn #fn_ident(lua: &mlua::Lua #(, #params)*) -> mlua::Result<mlua::Table> {
                let t = lua.create_table()?;
                #(#calls)*
                Ok(t)
            }
        }
    };
    quote!(#docs #body).into()
}

struct LuaClassInput {
    desc: String,
    lua_name: LitStr,
    self_ty: Type,
    docs_ident: Ident,
    entries: Vec<Entry>,
    fields: Option<Ident>,
    extra: Option<Ident>,
}

impl Parse for LuaClassInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let desc = doc_lines(&attrs).join("\n").trim().to_owned();
        let lua_name: LitStr = input.parse()?;
        input.parse::<Token![=>]>()?;
        let self_ty: Type = input.parse()?;
        input.parse::<Token![,]>()?;
        let docs_ident: Ident = input.parse()?;
        let inner;
        syn::bracketed!(inner in input);
        let entries = Punctuated::<Entry, Token![,]>::parse_terminated(&inner)?
            .into_iter()
            .collect::<Vec<_>>();
        for entry in &entries {
            if !entry.args.is_empty() {
                return Err(syn::Error::new(
                    entry.ident.span(),
                    "method entries take no arguments",
                ));
            }
        }
        let (mut fields, mut extra) = (None, None);
        while !input.is_empty() {
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
                continue;
            }
            let kw: Ident = input.parse()?;
            let target: Ident = input.parse()?;
            match kw.to_string().as_str() {
                "fields" => fields = Some(target),
                "extra" => extra = Some(target),
                _ => return Err(syn::Error::new(kw.span(), "expected `fields` or `extra`")),
            }
        }
        Ok(Self {
            desc,
            lua_name,
            self_ty,
            docs_ident,
            entries,
            fields,
            extra,
        })
    }
}

/// `lua_class! { /// desc … "maki.ui.Buf" => BufHandle, DOCS [a, b, manual c] fields f, extra g }`
///
/// Generates the `DocKind::Class` `ModuleDoc` const and the `mlua::UserData`
/// impl from one method list. `manual` entries are documented but registered
/// by the `extra` fn, which runs after the listed methods; `fields` wires an
/// `add_fields` body.
#[proc_macro]
pub fn lua_class(input: TokenStream) -> TokenStream {
    let LuaClassInput {
        desc,
        lua_name,
        self_ty,
        docs_ident,
        entries,
        fields,
        extra,
    } = parse_macro_input!(input as LuaClassInput);

    let doc_idents = entries.iter().map(|e| format_ident!("{}__doc", e.ident));
    let calls = entries.iter().filter(|e| !e.manual).map(|e| {
        let reg = format_ident!("{}__register", e.ident);
        quote!(#reg(methods);)
    });
    let extra_call = extra.map(|f| quote!(#f(methods);));
    let fields_impl = fields.map(|f| {
        quote! {
            fn add_fields<F: mlua::UserDataFields<Self>>(fields: &mut F) {
                #f(fields);
            }
        }
    });

    quote! {
        pub(crate) const #docs_ident: crate::docs::ModuleDoc = crate::docs::ModuleDoc {
            name: #lua_name,
            kind: crate::docs::DocKind::Class,
            desc: #desc,
            fns: &[#(#doc_idents),*],
        };

        impl mlua::UserData for #self_ty {
            #fields_impl

            fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
                #(#calls)*
                #extra_call
            }
        }
    }
    .into()
}
