#![allow(unused_imports)]

mod app;
mod client_api;
mod types;

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
        #[cfg(target_arch = "wasm32")]
        tracing_wasm::set_as_global_default();
        dioxus::launch(App);
    }
}
