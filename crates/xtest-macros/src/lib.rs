//! Procedural macros for the `xtest` crate. Use them through `xtest`, not
//! directly: the generated code refers to `::xtest`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Attribute, ItemFn};

/// Marks a function as a test. See the `xtest` crate for details.
#[proc_macro_attribute]
pub fn test(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = TokenStream2::from(args);
    let item = match syn::parse::<ItemFn>(item.clone()) {
        Ok(f) => f,
        Err(e) => {
            // Keep the item so that later errors in it are still reported.
            let mut out = e.to_compile_error();
            out.extend(TokenStream2::from(item));
            return out.into();
        }
    };
    match expand(args, item) {
        Ok(ts) | Err(ts) => ts.into(),
    }
}

/// Expands `#[xtest::test] fn name() -> R { body }` into
///
/// ```text
/// #[test]
/// fn name() -> R {
///     fn __xtest_body() -> R { body }
///     ::xtest::__private::run(__xtest_body)
/// }
/// ```
///
/// The outer function keeps the name, so libtest lists the test under the same
/// path as a plain `#[test]`. The body moves into a nested function, which sees
/// the same module scope (private items, `use super::*`). `run` is where
/// cargo-xtest will hook in, e.g. to hand fixtures to the body.
///
/// On an unsupported signature, returns `Err` with the errors and the function
/// unchanged, so that errors in its body are still reported.
fn expand(args: TokenStream2, item: ItemFn) -> Result<TokenStream2, TokenStream2> {
    let mut errors: Option<syn::Error> = None;
    let mut fail = |span: proc_macro2::Span, msg: &str| {
        let e = syn::Error::new(span, msg);
        match &mut errors {
            Some(all) => all.combine(e),
            None => errors = Some(e),
        }
    };

    if !args.is_empty() {
        fail(args.span(), "#[xtest::test] takes no arguments");
    }
    let sig = &item.sig;
    if let Some(t) = &sig.asyncness {
        fail(t.span(), "async tests are not supported yet (see https://github.com/samvv/cargo-xtest/issues/6)");
    }
    if let Some(t) = &sig.constness {
        fail(t.span(), "a test can't be a `const fn`");
    }
    if let Some(t) = &sig.unsafety {
        fail(t.span(), "a test can't be an `unsafe fn`");
    }
    if let Some(abi) = &sig.abi {
        fail(abi.span(), "a test can't have an `extern` ABI");
    }
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        fail(sig.generics.span(), "a test can't be generic");
    }
    if !sig.inputs.is_empty() {
        fail(sig.inputs.span(), "a test can't take parameters");
    }
    if let Some(v) = &sig.variadic {
        fail(v.span(), "a test can't be variadic");
    }
    for attr in &item.attrs {
        if is_test_attribute(attr) {
            fail(attr.span(), "#[xtest::test] already makes this a test; remove this attribute");
        }
    }

    if let Some(e) = errors {
        let mut out = e.to_compile_error();
        out.extend(quote!(#item));
        return Err(out);
    }

    let ItemFn { attrs, vis, sig, block } = item;
    let output = &sig.output;
    let body_sig = quote_spanned!(sig.span()=> fn __xtest_body() #output);
    Ok(quote! {
        #[test]
        #(#attrs)*
        #vis #sig {
            #body_sig #block
            ::xtest::__private::run(__xtest_body)
        }
    })
}

/// Whether `attr` makes a function a test on its own: `#[test]`,
/// `#[tokio::test]`, `#[xtest::test]` and the like.
fn is_test_attribute(attr: &Attribute) -> bool {
    attr.path().segments.last().is_some_and(|s| s.ident == "test")
}

#[cfg(test)]
mod tests {
    use super::{expand, ItemFn, TokenStream2};
    use quote::quote;

    fn run(args: TokenStream2, item: TokenStream2) -> Result<String, String> {
        let item: ItemFn = syn::parse2(item).unwrap();
        expand(args, item).map(|t| t.to_string()).map_err(|t| t.to_string())
    }

    fn errors(item: TokenStream2) -> String {
        run(quote!(), item).expect_err("expected an error")
    }

    #[test]
    fn wraps_the_body_in_a_libtest_test() {
        let out = run(
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
        let out = run(quote!(), quote!(pub(crate) fn t() {})).unwrap();
        assert!(out.starts_with("# [test] pub (crate) fn t ()"), "{out}");
    }

    #[test]
    fn rejects_unsupported_signatures() {
        assert!(errors(quote!(async fn t() {})).contains("async tests are not supported yet"));
        assert!(errors(quote!(const fn t() {})).contains("`const fn`"));
        assert!(errors(quote!(unsafe fn t() {})).contains("`unsafe fn`"));
        assert!(errors(quote!(extern "C" fn t() {})).contains("`extern` ABI"));
        assert!(errors(quote!(fn t<T>() {})).contains("can't be generic"));
        assert!(errors(quote!(fn t() where u8: Copy {})).contains("can't be generic"));
        assert!(errors(quote!(fn t(x: u8) {})).contains("can't take parameters"));
    }

    #[test]
    fn rejects_arguments() {
        let e = run(quote!(foo), quote!(fn t() {})).unwrap_err();
        assert!(e.contains("takes no arguments"), "{e}");
    }

    #[test]
    fn rejects_a_second_test_attribute() {
        for attr in [quote!(#[test]), quote!(#[tokio::test]), quote!(#[xtest::test])] {
            let e = errors(quote!(#attr fn t() {}));
            assert!(e.contains("already makes this a test"), "{attr}: {e}");
        }
    }

    #[test]
    fn reports_every_error_and_keeps_the_function() {
        let e = errors(quote!(async fn t(x: u8) {}));
        assert!(e.contains("async") && e.contains("parameters"), "{e}");
        assert!(e.contains("async fn t (x : u8) { }"), "{e}");
    }
}
