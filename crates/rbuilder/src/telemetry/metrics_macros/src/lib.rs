extern crate proc_macro;
use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Item};

/// Generates and registers the given static Prometheus metrics to a static
/// Registry named REGISTRY.
///
/// This avoids the need for the caller to manually initialize the registry
/// and call REGISTERY.register on every metric.
///
/// Metrics are eagerly initialized when the program starts with `ctor`,
/// so they will show up in the Prometheus metrics endpoint immediately.
///
/// Note: `lazy_static::lazy_static` and `ctor::ctor` must be in scope.
///
/// # Example
///
/// ```ignore
/// register_metrics! {
///     pub static CURRENT_BLOCK: IntGauge = IntGauge::new("current_block", "Current Block").unwrap();
///
///     pub static BLOCK_FILL_TIME: HistogramVec = HistogramVec::new(
///         HistogramOpts::new("block_fill_time", "Block Fill Times (ms)")
///             .buckets(exponential_buckets_range(1.0, 3000.0, 100)),
///         &["builder_name"]
///     )
///     .unwrap();
/// }
/// ```
///
/// expands to
///
/// ```ignore
/// lazy_static! {
///     pub static ref CURRENT_BLOCK: IntGauge = {
///         let metric = IntGauge::new("current_block", "Current Block").unwrap();
///         REGISTRY.register(Box::new(metric.clone())).unwrap();
///         metric
///     };
///
///     pub static ref BLOCK_FILL_TIME: HistogramVec = {
///         let metric = HistogramVec::new(
///             HistogramOpts::new("block_fill_time", "Block Fill Times (ms)")
///                 .buckets(exponential_buckets_range(1.0, 3000.0, 100)),
///             &["builder_name"]
///         ).unwrap();
///         REGISTRY.register(Box::new(metric.clone())).expect("Failed to register metric");
///         metric
///     };
/// }
///
/// #[ctor]
/// fn initialize_metrics() {
///     // Force initialization of lazy statics
///     let _ = *CURRENT_BLOCK;
///     let _ = *BLOCK_FILL_TIME;
/// }
/// ```
#[proc_macro]
pub fn register_metrics(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::File);

    // Stuff to put in lazy_static!
    let mut metrics = quote! {};

    // Stuff to put in ctor
    let mut initializers = quote! {};

    for item in input.items {
        if let Item::Static(static_item) = item {
            let vis = &static_item.vis;
            let ident = &static_item.ident;
            let ty = &static_item.ty;
            let expr = &static_item.expr;

            // Create the static metric call REGISTER.register with it.
            metrics.extend(quote! {
                #vis static ref #ident: #ty = {
                    let metric = #expr;
                    REGISTRY.register(Box::new(metric.clone())).expect("Failed to register metric");
                    metric
                };
            });

            // Make sure the metric is eagerly initialized
            initializers.extend(quote! {
                let _ = *#ident;
            });
        } else {
            panic!("register_metrics! only supports static items");
        }
    }

    let out = quote! {
        lazy_static! {
            #metrics
        }

        #[ctor]
        fn initialize_metrics() {
            #initializers
        }
    };

    TokenStream::from(out)
}
