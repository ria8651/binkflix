Keep the build warning-free across default, `--features web --target wasm32-unknown-unknown`, and `--features server`.

The wasm client is served by and version-locked to the server (asset hashes are cache-busted), so the two never run skewed versions. Shared wire types in `src/types.rs` can therefore be strict enums (e.g. `Phase`, `Stage`) without `#[serde(other)]` fallbacks — don't add unknown-variant handling for version skew that can't happen.
