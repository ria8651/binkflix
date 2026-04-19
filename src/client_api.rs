//! HTTP fetchers used by Dioxus components. Compiles on both targets — on non-wasm
//! builds these return stubs so SSR/native can still compile the UI. Actual
//! requests only happen in the browser.

use crate::types::*;

#[cfg(feature = "web")]
async fn fetch_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let resp = gloo_net::http::Request::get(url)
        .send()
        .await
        .map_err(|e| format!("network error hitting {url}: {e}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("failed to read response body from {url}: {e}"))?;

    if !(200..300).contains(&status) {
        return Err(format!("{url} returned HTTP {status}: {}", truncate(&text, 300)));
    }

    serde_json::from_str::<T>(&text).map_err(|e| {
        format!(
            "invalid JSON from {url}: {e}. First bytes: {}",
            truncate(&text, 200)
        )
    })
}

#[cfg(not(feature = "web"))]
async fn fetch_json<T: serde::de::DeserializeOwned>(_url: &str) -> Result<T, String> {
    Err("client fetcher invoked on non-wasm target".to_string())
}

#[cfg(feature = "web")]
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..n]) }
}

pub async fn get_library() -> Result<Library, String> {
    fetch_json("/api/library").await
}

pub async fn get_media(id: &str) -> Result<Media, String> {
    fetch_json(&format!("/api/media/{id}")).await
}

pub async fn get_show(id: &str) -> Result<ShowDetail, String> {
    fetch_json(&format!("/api/shows/{id}")).await
}

pub async fn get_subtitles(id: &str) -> Result<Vec<SubtitleTrack>, String> {
    fetch_json(&format!("/api/media/{id}/subtitles")).await
}
