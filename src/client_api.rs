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

pub async fn get_media_tech(id: &str) -> Result<MediaTechInfo, String> {
    fetch_json(&format!("/api/media/{id}/tech")).await
}

// Only consumed from the `#[cfg(feature = "web")]` polling loop in
// the debug panel; non-web builds compile but never call it.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub async fn get_hls_state(
    id: &str,
    audio_idx: u32,
    mode: &str,
    bitrate_kbps: Option<u32>,
) -> Result<HlsState, String> {
    let mut url = format!("/api/media/{id}/hls/state?a={audio_idx}");
    if !mode.is_empty() {
        url.push_str(&format!("&mode={mode}"));
    }
    if let Some(b) = bitrate_kbps {
        url.push_str(&format!("&bitrate={b}"));
    }
    fetch_json(&url).await
}

#[cfg(feature = "web")]
async fn post_empty<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let resp = gloo_net::http::Request::post(url)
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
async fn post_empty<T: serde::de::DeserializeOwned>(_url: &str) -> Result<T, String> {
    Err("client fetcher invoked on non-wasm target".to_string())
}

pub async fn get_rooms() -> Result<Vec<RoomListItem>, String> {
    fetch_json("/api/rooms").await
}

#[allow(dead_code)]
pub async fn create_room() -> Result<CreateRoomResp, String> {
    post_empty("/api/rooms").await
}

pub async fn get_scan_status() -> Result<ScanProgress, String> {
    fetch_json("/api/scan/status").await
}

pub async fn start_scan() -> Result<ScanProgress, String> {
    post_empty("/api/scan").await
}

pub async fn get_continue_watching() -> Result<Vec<ContinueItem>, String> {
    fetch_json("/api/continue-watching").await
}

#[derive(Default, Clone, Debug)]
pub struct SearchQuery {
    pub q: String,
    pub kind: String,        // "" (any) | "movie" | "show"
    pub year_min: Option<i64>,
    pub year_max: Option<i64>,
    pub genres: Vec<String>,
    pub watched: String,     // "any" | "watched" | "unwatched" | "in_progress"
    pub sort: String,        // "title" | "year_desc" | "year_asc" | "recently_added"
    pub limit: Option<i64>,
}

fn encode_query_component(s: &str) -> String {
    // RFC 3986 unreserved + a handful that are safe in querystrings; everything
    // else gets percent-encoded. Avoid pulling a URL-encoding crate just for
    // this — the search box is the only caller.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

pub async fn search_library(q: &SearchQuery) -> Result<SearchResponse, String> {
    let mut parts: Vec<String> = Vec::new();
    if !q.q.is_empty() {
        parts.push(format!("q={}", encode_query_component(&q.q)));
    }
    if !q.kind.is_empty() {
        parts.push(format!("kind={}", encode_query_component(&q.kind)));
    }
    if let Some(y) = q.year_min {
        parts.push(format!("year_min={y}"));
    }
    if let Some(y) = q.year_max {
        parts.push(format!("year_max={y}"));
    }
    for g in &q.genres {
        parts.push(format!("genres={}", encode_query_component(g)));
    }
    if !q.watched.is_empty() && q.watched != "any" {
        parts.push(format!("watched={}", encode_query_component(&q.watched)));
    }
    if !q.sort.is_empty() {
        parts.push(format!("sort={}", encode_query_component(&q.sort)));
    }
    if let Some(l) = q.limit {
        parts.push(format!("limit={l}"));
    }
    let url = if parts.is_empty() {
        "/api/search".to_string()
    } else {
        format!("/api/search?{}", parts.join("&"))
    };
    fetch_json(&url).await
}

pub async fn get_genres() -> Result<Vec<String>, String> {
    fetch_json("/api/genres").await
}

#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub async fn get_progress(id: &str) -> Result<Option<WatchProgress>, String> {
    fetch_json(&format!("/api/media/{id}/progress")).await
}

#[cfg(feature = "web")]
pub async fn report_progress(id: &str, position_secs: f64, duration_secs: f64) -> Result<(), String> {
    let url = format!("/api/media/{id}/progress");
    let body = ProgressReport { position_secs, duration_secs };
    let resp = gloo_net::http::Request::post(&url)
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| format!("network error hitting {url}: {e}"))?;
    if !(200..300).contains(&resp.status()) {
        return Err(format!("{url} returned HTTP {}", resp.status()));
    }
    Ok(())
}

#[cfg(not(feature = "web"))]
#[allow(dead_code)]
pub async fn report_progress(_id: &str, _position_secs: f64, _duration_secs: f64) -> Result<(), String> {
    Err("client fetcher invoked on non-wasm target".to_string())
}

#[cfg(feature = "web")]
pub async fn mark_watched(id: &str) -> Result<(), String> {
    let url = format!("/api/media/{id}/watched");
    let resp = gloo_net::http::Request::post(&url)
        .send()
        .await
        .map_err(|e| format!("network error hitting {url}: {e}"))?;
    if !(200..300).contains(&resp.status()) {
        return Err(format!("{url} returned HTTP {}", resp.status()));
    }
    Ok(())
}

#[cfg(not(feature = "web"))]
#[allow(dead_code)]
pub async fn mark_watched(_id: &str) -> Result<(), String> {
    Err("client fetcher invoked on non-wasm target".to_string())
}

#[cfg(feature = "web")]
pub async fn dismiss_continue_watching(id: &str) -> Result<(), String> {
    let url = format!("/api/continue-watching/dismiss/{id}");
    let resp = gloo_net::http::Request::post(&url)
        .send()
        .await
        .map_err(|e| format!("network error hitting {url}: {e}"))?;
    if !(200..300).contains(&resp.status()) {
        return Err(format!("{url} returned HTTP {}", resp.status()));
    }
    Ok(())
}

#[cfg(not(feature = "web"))]
#[allow(dead_code)]
pub async fn dismiss_continue_watching(_id: &str) -> Result<(), String> {
    Err("client fetcher invoked on non-wasm target".to_string())
}

// ---- Sticky playback preferences (audio/subtitle/quality) ----
//
// Scope is opaque to the server: the player builds `show:<id>` for
// episodes and `media:<id>` for movies. Encoded with encodeURIComponent
// equivalent so the `:` survives transit.

#[cfg(feature = "web")]
fn encode_scope(scope: &str) -> String {
    js_sys::encode_uri_component(scope).as_string().unwrap_or_else(|| scope.to_string())
}

#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub async fn get_preferences(scope: &str) -> Result<Option<MediaPreferences>, String> {
    #[cfg(feature = "web")]
    {
        let s = encode_scope(scope);
        return fetch_json(&format!("/api/preferences/{s}")).await;
    }
    #[cfg(not(feature = "web"))]
    {
        let _ = scope;
        Err("client fetcher invoked on non-wasm target".to_string())
    }
}

#[cfg(feature = "web")]
pub async fn set_preferences(scope: &str, prefs: &MediaPreferences) -> Result<(), String> {
    let url = format!("/api/preferences/{}", encode_scope(scope));
    let resp = gloo_net::http::Request::post(&url)
        .header("content-type", "application/json")
        .body(serde_json::to_string(prefs).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| format!("network error hitting {url}: {e}"))?;
    if !(200..300).contains(&resp.status()) {
        return Err(format!("{url} returned HTTP {}", resp.status()));
    }
    Ok(())
}

#[cfg(not(feature = "web"))]
#[allow(dead_code)]
pub async fn set_preferences(_scope: &str, _prefs: &MediaPreferences) -> Result<(), String> {
    Err("client fetcher invoked on non-wasm target".to_string())
}
