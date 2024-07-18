use proc_macro::TokenStream;
use quote::quote;
use reqwest::blocking::Client;
use std::{env, time::Duration};
use syn::{parse_macro_input, ItemFn, LitStr};

#[proc_macro_attribute]
pub fn ignore_if_env_not_set(attr: TokenStream, item: TokenStream) -> TokenStream {
    let env_var_to_check = parse_macro_input!(attr as LitStr).value();
    let input = parse_macro_input!(item as ItemFn);

    // Check if the environment variable is set
    let env_var_set = env::var(env_var_to_check).is_ok();

    let result = if env_var_set {
        // If the environment variable is set, return the original function
        quote! { #input }
    } else {
        // If the environment variable is not set, add the #[ignore] attribute
        let attrs = &input.attrs;
        let vis = &input.vis;
        let sig = &input.sig;
        let block = &input.block;

        quote! {
            #[ignore]
            #(#attrs)*
            #vis #sig #block
        }
    };

    result.into()
}

#[proc_macro_attribute]
pub fn ignore_if_endpoint_unavailable(attr: TokenStream, item: TokenStream) -> TokenStream {
    let endpoint_to_check = parse_macro_input!(attr as LitStr).value();
    let input = parse_macro_input!(item as ItemFn);

    // Check if the HTTP endpoint is available
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let endpoint_available = client.get(endpoint_to_check).send().is_ok();

    let result = if endpoint_available {
        // If the endpoint is available, return the original function
        quote! { #input }
    } else {
        // If the endpoint is not available, add the #[ignore] attribute
        let attrs = &input.attrs;
        let vis = &input.vis;
        let sig = &input.sig;
        let block = &input.block;

        quote! {
            #(#attrs)*
            #[ignore]
            #vis #sig #block
        }
    };

    result.into()
}
