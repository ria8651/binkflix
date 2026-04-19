#![allow(unused_imports)]

mod app;
mod client_api;
mod types;

mod syncplay_client;

#[cfg(feature = "server")]
mod server;

use app::App;

fn main() {
    #[cfg(feature = "server")]
    {
        server::run();
        return;
    }

    #[cfg(all(feature = "web", not(feature = "server")))]
    {
        // Default tracing_wasm is TRACE, which floods the devtools console with
        // Dioxus-internal diff/signal chatter. Pin to WARN so the console stays
        // usable.
        #[cfg(target_arch = "wasm32")]
        {
            let config = tracing_wasm::WASMLayerConfigBuilder::new()
                .set_max_level(tracing::Level::WARN)
                .build();
            tracing_wasm::set_as_global_default_with_config(config);
        }
        dioxus::launch(App);
    }
}
