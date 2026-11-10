//! Procedural macros for the `xtest` crate. Use them through `xtest`, not
//! directly: the generated code refers to `::xtest`.
//!
//! Every macro also embeds a metadata record in the binary (see
//! `xtest_proto::meta`), so cargo-xtest knows the fixtures and which tests
//! use them without running anything.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Attribute, FnArg, ItemFn, LitStr, Pat, Path, Signature, Type};

/// Marks a function as a test. See the `xtest` crate for details.
#[proc_macro_attribute]
pub fn test(args: TokenStream, item: TokenStream) -> TokenStream {
    entry(args, item, expand_test)
}

/// Marks a function as a fixture. See the `xtest` crate for details.
#[proc_macro_attribute]
pub fn fixture(args: TokenStream, item: TokenStream) -> TokenStream {
    entry(args, item, expand_fixture)
}

/// Marks the teardown of a fixture. See the `xtest` crate for details.
#[proc_macro_attribute]
pub fn teardown(args: TokenStream, item: TokenStream) -> TokenStream {
    entry(args, item, expand_teardown)
}

type Expand = fn(TokenStream2, ItemFn) -> Result<TokenStream2, TokenStream2>;

fn entry(args: TokenStream, item: TokenStream, expand: Expand) -> TokenStream {
    let item = match syn::parse::<ItemFn>(item.clone()) {
        Ok(f) => f,
        Err(e) => {
            // Keep the item so that later errors in it are still reported.
            let mut out = e.to_compile_error();
            out.extend(TokenStream2::from(item));
            return out.into();
        }
    };
    match expand(args.into(), item) {
        Ok(ts) | Err(ts) => ts.into(),
    }
}

/// Collects errors, so that all of them are reported at once.
#[derive(Default)]
struct Errors(Option<syn::Error>);

impl Errors {
    fn add(&mut self, span: Span, msg: impl std::fmt::Display) {
        self.add_error(syn::Error::new(span, msg));
    }

    fn add_error(&mut self, e: syn::Error) {
        match &mut self.0 {
            Some(all) => all.combine(e),
            None => self.0 = Some(e),
        }
    }

    /// On errors: the errors, followed by `item` unchanged so that errors in
    /// its body are still reported.
    fn finish(self, item: &ItemFn) -> Result<(), TokenStream2> {
        match self.0 {
            None => Ok(()),
            Some(e) => {
                let mut out = e.to_compile_error();
                out.extend(quote!(#item));
                Err(out)
            }
        }
    }
}

/// Checks what every kind of function shares: no generics, `const`,
/// `unsafe`, ABI, variadics or receivers.
fn check_signature(sig: &Signature, what: &str, errors: &mut Errors) {
    if let Some(t) = &sig.constness {
        errors.add(t.span(), format!("{what} can't be a `const fn`"));
    }
    if let Some(t) = &sig.unsafety {
        errors.add(t.span(), format!("{what} can't be an `unsafe fn`"));
    }
    if let Some(abi) = &sig.abi {
        errors.add(abi.span(), format!("{what} can't have an `extern` ABI"));
    }
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        errors.add(sig.generics.span(), format!("{what} can't be generic"));
    }
    if let Some(v) = &sig.variadic {
        errors.add(v.span(), format!("{what} can't be variadic"));
    }
    for arg in &sig.inputs {
        if let FnArg::Receiver(r) = arg {
            errors.add(r.span(), format!("{what} can't take `self`"));
        }
    }
}

/// Whether `attr` makes a function a test on its own: `#[test]`,
/// `#[tokio::test]`, `#[xtest::test]` and the like.
fn is_test_attribute(attr: &Attribute) -> bool {
    attr.path().segments.last().is_some_and(|s| s.ident == "test")
}

fn check_no_test_attribute(attrs: &[Attribute], errors: &mut Errors, msg: &str) {
    for attr in attrs {
        if is_test_attribute(attr) {
            errors.add(attr.span(), msg);
        }
    }
}

/// A parameter that takes a fixture's value.
struct FixtureParam {
    /// The parameter, without `#[fixture(...)]`.
    arg: FnArg,
    /// What it refers to: `#[fixture(path)]`, or its own name.
    fixture: Path,
    by_ref: bool,
}

impl FixtureParam {
    /// How the generated code passes the value: `&values.get(&fixture)`.
    fn arg(&self, values: &syn::Ident) -> TokenStream2 {
        let f = &self.fixture;
        if self.by_ref {
            quote!(&#values.get(&#f()))
        } else {
            quote!(#values.get(&#f()))
        }
    }
}

fn fixture_param(arg: &FnArg, errors: &mut Errors) -> Option<FixtureParam> {
    let FnArg::Typed(pt) = arg else { return None };
    let mut pt = pt.clone();
    let mut fixture = None;
    let mut keep = Vec::new();
    for attr in pt.attrs.drain(..) {
        if attr.path().is_ident("fixture") {
            match attr.parse_args::<Path>() {
                Ok(p) => fixture = Some(p),
                Err(e) => errors.add_error(e),
            }
        } else {
            keep.push(attr);
        }
    }
    pt.attrs = keep;
    let fixture = match (&*pt.pat, fixture) {
        (_, Some(p)) => p,
        (Pat::Ident(p), None) => p.ident.clone().into(),
        (pat, None) => {
            errors.add(pat.span(), "a fixture parameter needs a name, or #[fixture(path)] to say which fixture it is");
            return None;
        }
    };
    let by_ref = match &*pt.ty {
        Type::Reference(r) if r.mutability.is_some() => {
            errors.add(r.span(), "a fixture's value can't be borrowed mutably; take it by value or as `&T`");
            false
        }
        Type::Reference(_) => true,
        _ => false,
    };
    Some(FixtureParam { arg: FnArg::Typed(pt), fixture, by_ref })
}

/// Whether `arg` is `&mut <...>::name`, e.g. `&mut Setup`.
fn is_context(arg: &FnArg, name: &str) -> bool {
    let FnArg::Typed(pt) = arg else { return false };
    let Type::Reference(r) = &*pt.ty else { return false };
    let Type::Path(p) = &*r.elem else { return false };
    r.mutability.is_some() && p.path.segments.last().is_some_and(|s| s.ident == name)
}

/// The statements that embed a metadata record. `pairs` are `&str`
/// expressions: key, value, key, value, ...
fn record(pairs: Vec<TokenStream2>) -> TokenStream2 {
    quote! {
        const __XTEST_PAIRS: &[&str] = &[#(#pairs),*];
        const __XTEST_LEN: usize = ::xtest::__private::record_len(__XTEST_PAIRS);
        #[used]
        static __XTEST_RECORD: [u8; __XTEST_LEN] = ::xtest::__private::record::<__XTEST_LEN>(__XTEST_PAIRS);
        ::xtest::__private::keep(&__XTEST_RECORD);
    }
}

/// Expands `#[xtest::test] fn name(fixtures...) -> R { body }` into
///
/// ```text
/// #[test]
/// fn name() -> R {
///     fn __xtest_body(fixtures...) -> R { body }
///     ::xtest::__private::run(__xtest_body)   // or run_with(...), with fixtures
/// }
/// ```
///
/// The outer function keeps the name, so libtest lists the test under the same
/// path as a plain `#[test]`. The body moves into a nested function, which sees
/// the same module scope (private items, `use super::*`).
///
/// A test with fixtures is also `#[ignore]`d, so plain `cargo test` skips it;
/// its metadata says whether the user ignored it too.
fn expand_test(args: TokenStream2, item: ItemFn) -> Result<TokenStream2, TokenStream2> {
    let mut errors = Errors::default();
    if !args.is_empty() {
        errors.add(args.span(), "#[xtest::test] takes no arguments");
    }
    let sig = &item.sig;
    if let Some(t) = &sig.asyncness {
        errors.add(t.span(), "async tests are not supported yet (see https://github.com/samvv/cargo-xtest/issues/6)");
    }
    check_signature(sig, "a test", &mut errors);
    check_no_test_attribute(&item.attrs, &mut errors, "#[xtest::test] already makes this a test; remove this attribute");
    let params: Vec<FixtureParam> = sig.inputs.iter().filter_map(|a| fixture_param(a, &mut errors)).collect();
    errors.finish(&item)?;

    let ItemFn { attrs, vis, sig, block } = item;
    let output = &sig.output;
    let mut outer = sig.clone();
    outer.inputs.clear();
    if params.is_empty() {
        let body_sig = quote_spanned!(sig.span()=> fn __xtest_body() #output);
        return Ok(quote! {
            #[test]
            #(#attrs)*
            #vis #outer {
                #body_sig #block
                ::xtest::__private::run(__xtest_body)
            }
        });
    }

    let user_ignored = attrs.iter().any(|a| a.path().is_ident("ignore"));
    let ignore = (!user_ignored).then(|| quote!(#[ignore = "uses fixtures; run it with `cargo xtest`"]));
    let name = sig.ident.to_string();
    let mut pairs = vec![
        quote!("kind"),
        quote!("test"),
        quote!("module"),
        quote!(::core::module_path!()),
        quote!("name"),
        quote!(#name),
        quote!("ignored"),
        if user_ignored { quote!("1") } else { quote!("0") },
    ];
    for p in &params {
        let f = &p.fixture;
        pairs.extend([quote!("uses"), quote!(#f().id())]);
    }
    let record = record(pairs);
    let inputs = params.iter().map(|p| &p.arg);
    let values = format_ident!("__xtest_values");
    let call_args = params.iter().map(|p| p.arg(&values));
    let names = params.iter().map(|p| {
        let f = &p.fixture;
        quote!(#f().name())
    });
    Ok(quote! {
        #[test]
        #ignore
        #(#attrs)*
        #vis #outer {
            fn __xtest_body(#(#inputs),*) #output #block
            #record
            ::xtest::__private::run_with(&[#(#names),*], |#values| __xtest_body(#(#call_args),*))
        }
    })
}

/// Arguments of `#[xtest::fixture(...)]`.
struct FixtureArgs {
    scope: String,
    memory: Option<String>,
    timeout: Option<String>,
    environment: bool,
}

fn parse_fixture_args(args: TokenStream2, errors: &mut Errors) -> FixtureArgs {
    let mut out = FixtureArgs { scope: "test".into(), memory: None, timeout: None, environment: false };
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("scope") {
            let s: LitStr = meta.value()?.parse()?;
            if !["test", "binary", "run"].contains(&s.value().as_str()) {
                return Err(syn::Error::new(s.span(), "scope must be \"test\", \"binary\" or \"run\""));
            }
            out.scope = s.value();
        } else if meta.path.is_ident("memory") {
            let s: LitStr = meta.value()?.parse()?;
            if !valid_size(&s.value()) {
                return Err(syn::Error::new(s.span(), "expected a size like \"512M\" or \"2G\""));
            }
            out.memory = Some(s.value());
        } else if meta.path.is_ident("timeout") {
            let s: LitStr = meta.value()?.parse()?;
            if !valid_duration(&s.value()) {
                return Err(syn::Error::new(s.span(), "expected a duration like \"30s\" or \"5m\""));
            }
            out.timeout = Some(s.value());
        } else if meta.path.is_ident("environment") {
            out.environment = true;
        } else {
            return Err(meta.error("unknown argument; expected `scope`, `memory`, `timeout` or `environment`"));
        }
        Ok(())
    });
    if let Err(e) = syn::parse::Parser::parse2(parser, args) {
        errors.add_error(e);
    }
    out
}

/// Splits `512M` into the number and the unit, if the number is valid.
fn split_number(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    num.parse::<f64>().ok().map(|_| (num, unit.trim()))
}

/// The sizes cargo-xtest's config accepts: `512M`, `4G`, `1.5GiB`, bytes.
fn valid_size(s: &str) -> bool {
    split_number(s).is_some_and(|(_, unit)| {
        let unit = unit.to_ascii_uppercase();
        ["", "K", "M", "G", "T"].contains(&unit.trim_end_matches("IB").trim_end_matches('B'))
    })
}

/// The durations cargo-xtest's config accepts: `500ms`, `30s`, `2m`, `1h`, seconds.
fn valid_duration(s: &str) -> bool {
    split_number(s).is_some_and(|(_, unit)| ["ms", "", "s", "m", "h"].contains(&unit))
}

/// Expands
///
/// ```text
/// #[xtest::fixture(scope = "run")]
/// async fn pg(cx: &mut Setup, dep: &Dep) -> xtest::Result<Pg> { body }
/// ```
///
/// into
///
/// ```text
/// const fn pg() -> ::xtest::Fixture<Pg> { ... }    // what tests refer to
/// async fn __xtest_fixture_body_pg(cx: &mut Setup, dep: &Dep) -> xtest::Result<Pg> { body }
/// #[test] #[ignore] fn __xtest_fixture_pg() { ... } // the setup cargo-xtest runs
/// ```
fn expand_fixture(args: TokenStream2, item: ItemFn) -> Result<TokenStream2, TokenStream2> {
    let mut errors = Errors::default();
    let args = parse_fixture_args(args, &mut errors);
    let sig = &item.sig;
    check_signature(sig, "a fixture", &mut errors);
    check_no_test_attribute(&item.attrs, &mut errors, "a fixture can't be a test; remove this attribute");
    let ret = match &sig.output {
        syn::ReturnType::Type(_, ty) => Some(ty.clone()),
        syn::ReturnType::Default => {
            errors.add(sig.span(), "a fixture must return its value: `-> xtest::Result<T>`");
            None
        }
    };
    let mut call_args = Vec::new();
    let mut deps = Vec::new();
    let values = format_ident!("__xtest_deps");
    for (i, arg) in sig.inputs.iter().enumerate() {
        if is_context(arg, "Setup") {
            if i != 0 {
                errors.add(arg.span(), "`&mut Setup` must be the first parameter");
            }
            call_args.push(quote!(&mut __xtest_cx));
        } else if let Some(p) = fixture_param(arg, &mut errors) {
            call_args.push(p.arg(&values));
            deps.push(p);
        }
    }
    errors.finish(&item)?;
    let ret = ret.unwrap();

    let ItemFn { attrs, vis, sig, block } = item;
    let name = &sig.ident;
    let name_str = name.to_string();
    let body_name = format_ident!("__xtest_fixture_body_{}", name);
    let hidden = format_ident!("__xtest_fixture_{}", name);
    let hidden_str = hidden.to_string();
    let (docs, attrs): (Vec<_>, Vec<_>) = attrs.into_iter().partition(|a| a.path().is_ident("doc"));
    let mut body_sig = sig.clone();
    body_sig.ident = body_name.clone();
    for (arg, p) in body_sig.inputs.iter_mut().filter(|a| !is_context(a, "Setup")).zip(&deps) {
        *arg = p.arg.clone();
    }
    let awaited = sig.asyncness.map(|_| quote!(.await));

    let mut pairs = vec![
        quote!("kind"),
        quote!("fixture"),
        quote!("id"),
        quote!(#name().id()),
        quote!("name"),
        quote!(#name_str),
        quote!("module"),
        quote!(::core::module_path!()),
        quote!("test"),
        quote!(#hidden_str),
    ];
    let scope = &args.scope;
    pairs.extend([quote!("scope"), quote!(#scope)]);
    let env = if args.environment { "1" } else { "0" };
    pairs.extend([quote!("environment"), quote!(#env)]);
    if let Some(m) = &args.memory {
        pairs.extend([quote!("memory"), quote!(#m)]);
    }
    if let Some(t) = &args.timeout {
        pairs.extend([quote!("timeout"), quote!(#t)]);
    }
    for d in &deps {
        let f = &d.fixture;
        pairs.extend([quote!("dep"), quote!(#f().id())]);
    }
    let record = record(pairs);

    Ok(quote! {
        // A function, not a const: a test parameter with the fixture's name
        // can shadow a function, but would be a pattern matching a const.
        #(#docs)*
        #[allow(dead_code)]
        #vis const fn #name() -> ::xtest::Fixture<::xtest::__private::Value<#ret>> {
            ::xtest::Fixture::__new(::core::concat!(::core::file!(), "#", #name_str), #name_str)
        }

        #[allow(dead_code)]
        #[doc(hidden)]
        #(#attrs)*
        #body_sig #block

        #[test]
        #[ignore = "fixture setup; cargo xtest runs it"]
        #[doc(hidden)]
        #[allow(unused_mut)]
        fn #hidden() {
            #record
            ::xtest::__private::fixture_main(&#name(), |mut __xtest_cx, #values| {
                ::std::boxed::Box::pin(async move {
                    let __xtest_out = #body_name(#(#call_args),*) #awaited;
                    (__xtest_cx, __xtest_out)
                })
            });
        }
    })
}

/// Expands `#[xtest::teardown(pg)] fn drop(cx: &mut Teardown, pg: Pg) { ... }`
/// into the function itself plus a hidden test `__xtest_teardown_drop` that
/// cargo-xtest runs to tear `pg` down.
fn expand_teardown(args: TokenStream2, item: ItemFn) -> Result<TokenStream2, TokenStream2> {
    let mut errors = Errors::default();
    let fixture: Option<Path> = match syn::parse2::<Path>(args.clone()) {
        Ok(p) => Some(p),
        Err(_) => {
            errors.add(args.span(), "expected the fixture this tears down: #[xtest::teardown(fixture)]");
            None
        }
    };
    let sig = &item.sig;
    check_signature(sig, "a teardown", &mut errors);
    check_no_test_attribute(&item.attrs, &mut errors, "a teardown can't be a test; remove this attribute");
    let mut call_args = Vec::new();
    let mut value_params = 0;
    for (i, arg) in sig.inputs.iter().enumerate() {
        if is_context(arg, "Teardown") {
            if i != 0 {
                errors.add(arg.span(), "`&mut Teardown` must be the first parameter");
            }
            call_args.push(quote!(&mut __xtest_cx));
        } else {
            value_params += 1;
            let by_ref = matches!(arg, FnArg::Typed(pt) if matches!(&*pt.ty, Type::Reference(_)));
            call_args.push(if by_ref { quote!(&__xtest_value) } else { quote!(__xtest_value) });
        }
    }
    if value_params != 1 {
        errors.add(sig.inputs.span(), "a teardown takes the fixture's value (and optionally `&mut Teardown` first)");
    }
    errors.finish(&item)?;
    let fixture = fixture.unwrap();

    let name = &item.sig.ident;
    let hidden = format_ident!("__xtest_teardown_{}", name);
    let hidden_str = hidden.to_string();
    let awaited = item.sig.asyncness.map(|_| quote!(.await));
    let record = record(vec![
        quote!("kind"),
        quote!("teardown"),
        quote!("fixture"),
        quote!(#fixture().id()),
        quote!("module"),
        quote!(::core::module_path!()),
        quote!("test"),
        quote!(#hidden_str),
    ]);
    Ok(quote! {
        #[allow(dead_code)]
        #item

        #[test]
        #[ignore = "fixture teardown; cargo xtest runs it"]
        #[doc(hidden)]
        #[allow(unused_mut)]
        fn #hidden() {
            #record
            ::xtest::__private::teardown_main(&#fixture(), |mut __xtest_cx, __xtest_value| {
                ::std::boxed::Box::pin(async move { #name(#(#call_args),*) #awaited })
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{expand_fixture, expand_teardown, expand_test, valid_duration, valid_size, ItemFn, TokenStream2};
    use quote::quote;

    fn run(f: super::Expand, args: TokenStream2, item: TokenStream2) -> Result<String, String> {
        let item: ItemFn = syn::parse2(item).unwrap();
        f(args, item).map(|t| t.to_string()).map_err(|t| t.to_string())
    }

    fn test_errors(item: TokenStream2) -> String {
        run(expand_test, quote!(), item).expect_err("expected an error")
    }

    #[test]
    fn wraps_the_body_in_a_libtest_test() {
        let out = run(
            expand_test,
            quote!(),
            quote! {
                /// Docs.
                #[should_panic]
                fn it_works() -> Result<(), String> { Ok(()) }
            },
        )
        .unwrap();
        let expected = quote! {
            #[test]
            #[doc = r" Docs."]
            #[should_panic]
            fn it_works() -> Result<(), String> {
                fn __xtest_body() -> Result<(), String> { Ok(()) }
                ::xtest::__private::run(__xtest_body)
            }
        };
        assert_eq!(out, expected.to_string());
    }

    #[test]
    fn keeps_visibility() {
        let out = run(expand_test, quote!(), quote!(pub(crate) fn t() {})).unwrap();
        assert!(out.starts_with("# [test] pub (crate) fn t ()"), "{out}");
    }

    #[test]
    fn a_test_with_fixtures_is_ignored_and_records_them() {
        let out = run(expand_test, quote!(), quote!(fn t(redis: &Redis, #[fixture(common::pg)] db: Db) {})).unwrap();
        assert!(out.contains(r#"# [ignore = "uses fixtures; run it with `cargo xtest`"]"#), "{out}");
        assert!(out.contains("fn __xtest_body (redis : & Redis , db : Db)"), "{out}");
        assert!(out.contains(r#""uses" , redis () . id () , "uses" , common :: pg () . id ()"#), "{out}");
        assert!(out.contains("__xtest_body (& __xtest_values . get (& redis ()) , __xtest_values . get (& common :: pg ()))"), "{out}");

        // The user's own #[ignore] is kept, and recorded.
        let out = run(expand_test, quote!(), quote!(#[ignore] fn t(redis: &Redis) {})).unwrap();
        assert_eq!(out.matches("# [ignore").count(), 1, "{out}");
        assert!(out.contains(r#""ignored" , "1""#), "{out}");
    }

    #[test]
    fn rejects_unsupported_tests() {
        assert!(test_errors(quote!(async fn t() {})).contains("async tests are not supported yet"));
        assert!(test_errors(quote!(const fn t() {})).contains("`const fn`"));
        assert!(test_errors(quote!(unsafe fn t() {})).contains("`unsafe fn`"));
        assert!(test_errors(quote!(extern "C" fn t() {})).contains("`extern` ABI"));
        assert!(test_errors(quote!(fn t<T>() {})).contains("can't be generic"));
        assert!(test_errors(quote!(fn t() where u8: Copy {})).contains("can't be generic"));
        assert!(test_errors(quote!(fn t((a, b): (u8, u8)) {})).contains("needs a name"));
        assert!(test_errors(quote!(fn t(x: &mut X) {})).contains("borrowed mutably"));
        let e = run(expand_test, quote!(foo), quote!(fn t() {})).unwrap_err();
        assert!(e.contains("takes no arguments"), "{e}");
    }

    #[test]
    fn rejects_a_second_test_attribute() {
        for attr in [quote!(#[test]), quote!(#[tokio::test]), quote!(#[xtest::test])] {
            let e = test_errors(quote!(#attr fn t() {}));
            assert!(e.contains("already makes this a test"), "{attr}: {e}");
        }
    }

    #[test]
    fn reports_every_error_and_keeps_the_function() {
        let e = test_errors(quote!(async fn t<T>() {}));
        assert!(e.contains("async") && e.contains("generic"), "{e}");
        assert!(e.contains("async fn t < T > () { }"), "{e}");
    }

    #[test]
    fn expands_a_fixture() {
        let out = run(
            expand_fixture,
            quote!(scope = "run", memory = "512M", environment),
            quote! {
                /// Docs.
                pub async fn pg(cx: &mut Setup, net: &Net) -> xtest::Result<Pg> { todo!() }
            },
        )
        .unwrap();
        assert!(out.contains("pub const fn pg () -> :: xtest :: Fixture < :: xtest :: __private :: Value < xtest :: Result < Pg > >>"), "{out}");
        assert!(out.contains("async fn __xtest_fixture_body_pg (cx : & mut Setup , net : & Net)"), "{out}");
        assert!(out.contains("fn __xtest_fixture_pg ()"), "{out}");
        assert!(out.contains(r#""scope" , "run" , "environment" , "1" , "memory" , "512M" , "dep" , net () . id ()"#), "{out}");
        assert!(out.contains("__xtest_fixture_body_pg (& mut __xtest_cx , & __xtest_deps . get (& net ())) . await"), "{out}");
        // Docs go on the function tests refer to, which is what users see.
        assert!(out.starts_with(r#"# [doc = r" Docs."] # [allow (dead_code)] pub const fn pg ()"#), "{out}");
    }

    #[test]
    fn rejects_bad_fixtures() {
        let e = |args, item| run(expand_fixture, args, item).unwrap_err();
        assert!(e(quote!(scope = "forever"), quote!(async fn f() -> R<T> {})).contains("scope must be"));
        assert!(e(quote!(memory = "lots"), quote!(async fn f() -> R<T> {})).contains("a size like"));
        assert!(e(quote!(timeout = "1y"), quote!(async fn f() -> R<T> {})).contains("a duration like"));
        assert!(e(quote!(colour = "red"), quote!(async fn f() -> R<T> {})).contains("unknown argument"));
        assert!(e(quote!(), quote!(async fn f() {})).contains("must return its value"));
        assert!(e(quote!(), quote!(async fn f(d: &D, cx: &mut Setup) -> R<T> {})).contains("must be the first parameter"));
    }

    #[test]
    fn expands_a_teardown() {
        let out = run(expand_teardown, quote!(db), quote!(async fn drop_db(cx: &mut Teardown, db: Db) -> R {})).unwrap();
        assert!(out.contains("fn __xtest_teardown_drop_db ()"), "{out}");
        assert!(out.contains(r#""fixture" , db () . id ()"#), "{out}");
        assert!(out.contains("drop_db (& mut __xtest_cx , __xtest_value) . await"), "{out}");

        let e = run(expand_teardown, quote!(), quote!(fn d(x: X) {})).unwrap_err();
        assert!(e.contains("expected the fixture"), "{e}");
        let e = run(expand_teardown, quote!(db), quote!(fn d(cx: &mut Teardown) {})).unwrap_err();
        assert!(e.contains("takes the fixture's value"), "{e}");
    }

    #[test]
    fn validates_sizes_and_durations_like_the_config() {
        assert!(valid_size("512M") && valid_size("1.5GiB") && valid_size("100KB") && valid_size("1024"));
        assert!(!valid_size("12X") && !valid_size("M"));
        assert!(valid_duration("500ms") && valid_duration("30s") && valid_duration("2m") && valid_duration("10"));
        assert!(!valid_duration("1y"));
    }
}
