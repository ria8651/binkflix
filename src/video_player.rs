//! HTML5 video with a custom overlay control bar and subtitle picker.
//!
//! The subtitle state machine (`user_pick` → `effective_id` → `sub_command`
//! → effect) is owned in Rust so the picker can react to library state. The
//! mechanical control wiring (play/pause button, scrubber, time labels,
//! volume, fullscreen, auto-hide) is handed off to
//! `window.binkflixPlayer.initControls(videoId)` in assets/static/player.js
//! — pushing real-time video state through Dioxus events would require
//! `web_sys`, and the DOM is the natural place for it anyway.
//!
//! Reactive subtitle pipeline:
//!   * `user_pick`     — user's explicit choice (`None` = untouched).
//!   * `effective_id`  — memo: user's pick if any, else the "default"/first
//!                       track from the list. PartialEq-deduped.
//!   * `sub_command`   — memo: the concrete call to make into JS
//!                       (`Option<SubCommand>`). Deduped by value.
//!   * the effect      — subscribes to `sub_command` only. Fires the eval
//!                       exactly once per actual change.

use crate::client_api::*;
use crate::types::*;
use dioxus::prelude::*;

const ICON_PLAY: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor" aria-hidden="true"><path d="M8 5v14l11-7z"/></svg>"#;
const ICON_BACK: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M19 12H5M12 19l-7-7 7-7"/></svg>"#;
const ICON_CAPTIONS: &str = r#"<svg viewBox="0 0 24 24" width="22" height="22" fill="currentColor" aria-hidden="true"><path d="M19 4H5c-1.11 0-2 .9-2 2v12c0 1.1.89 2 2 2h14c1.1 0 2-.9 2-2V6c0-1.1-.9-2-2-2zm-8 7H9.5v-.5h-2v3h2V13H11v1c0 .55-.45 1-1 1H7c-.55 0-1-.45-1-1v-4c0-.55.45-1 1-1h3c.55 0 1 .45 1 1v1zm7 0h-1.5v-.5h-2v3h2V13H18v1c0 .55-.45 1-1 1h-3c-.55 0-1-.45-1-1v-4c0-.55.45-1 1-1h3c.55 0 1 .45 1 1v1z"/></svg>"#;
const ICON_FULLSCREEN: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 9V4h5M21 9V4h-5M3 15v5h5M21 15v5h-5"/></svg>"#;
const ICON_INFO: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 16v-5M12 8h.01"/></svg>"#;
const ICON_CHECK: &str = r#"<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6L9 17l-5-5"/></svg>"#;

#[component]
pub fn VideoPlayer(id: String, back_route: crate::app::Route) -> Element {
    let id_for_subs = id.clone();
    let tracks = use_resource(move || {
        let id = id_for_subs.clone();
        async move { get_subtitles(&id).await }
    });

    // Stable DOM id so the JS helper can find the <video> element.
    let video_dom_id = "binkflix-video";

    // `None` = user hasn't touched the picker (fall back to default);
    // `Some(None)` = user explicitly chose "Off";
    // `Some(Some(id))` = user picked a track.
    let mut user_pick = use_signal(|| None::<Option<String>>);

    let effective_id = use_memo(move || -> Option<String> {
        if let Some(explicit) = user_pick.read().clone() {
            return explicit;
        }
        let tracks_read = tracks.read();
        let Some(Ok(list)) = &*tracks_read else { return None };
        list.iter()
            .find(|t| t.default)
            .or_else(|| list.first())
            .map(|t| t.id.clone())
    });

    let apply_id = id.clone();
    let sub_command = use_memo(move || -> Option<SubCommand> {
        let id = effective_id.read().clone()?;
        let tracks_read = tracks.read();
        let Some(Ok(list)) = &*tracks_read else { return None };
        let track = list.iter().find(|t| t.id == id)?;
        Some(SubCommand {
            format: if track.format == "ass" { SubFormat::Ass } else { SubFormat::Vtt },
            url: media_subtitle_url(&apply_id, &track.id),
            label: track.label.clone(),
            language: track.language.clone(),
        })
    });

    let mut loading = use_signal(|| false);
    let mut sub_error = use_signal(|| None::<String>);
    let mut last_applied = use_signal(|| None::<Option<SubCommand>>);
    let mut menu_open = use_signal(|| false);
    let mut debug_open = use_signal(|| false);
    let mut debug_stats = use_signal(|| None::<serde_json::Value>);

    // Tech probe drives two things: (1) the info panel, and (2) the
    // transcode-prompt flow below. For Direct/Remux verdicts the server
    // picks the right mode itself, so the video element can load plain
    // `/stream` without waiting. For Transcode we need the probe result
    // before deciding what to show — but that's the slow path, not the
    // hot path, so the extra beat is fine.
    let id_for_tech = id.clone();
    let tech = use_resource(move || {
        let id = id_for_tech.clone();
        async move { get_media_tech(&id).await }
    });

    // Title bar data. Movies show their title; episodes show the show
    // title with the episode number + title as a subtitle line.
    let id_for_media = id.clone();
    let media_resource = use_resource(move || {
        let id = id_for_media.clone();
        async move { get_media(&id).await }
    });
    let show_resource = use_resource(move || {
        let media = media_resource.read_unchecked().clone();
        async move {
            match media {
                Some(Ok(m)) if m.kind == "episode" => match m.show_id.as_deref() {
                    Some(sid) => get_show(sid).await.ok().map(|s| s.show.title),
                    None => None,
                },
                _ => None,
            }
        }
    });

    // `None` = use /stream (server picks).
    // `Some("remux"|"direct")` = user explicitly opted in after the
    //     transcode prompt; pin that mode on subsequent loads.
    let mut forced_mode = use_signal(|| None::<&'static str>);

    // Compute the src to hand the video element. Empty string means "don't
    // load yet" — we're waiting on the tech probe to decide whether to
    // show the transcode prompt. Rendering with an empty src is harmless
    // because we render the <video> behind the prompt overlay (it won't
    // be interacted with until the overlay is dismissed).
    let id_for_src = id.clone();
    let stream_src = use_memo(move || -> String {
        if let Some(mode) = *forced_mode.read() {
            // "Try remux" now goes through the HLS pipeline so the user
            // gets real random-access seeking instead of the fMP4-over-
            // pipe workaround. "Try direct" still hits byte-range serve
            // so source containers that are already browser-friendly
            // skip the ffmpeg round-trip entirely.
            return match mode {
                "remux" => media_hls_url(&id_for_src),
                _ => media_stream_url_with_mode(&id_for_src, mode),
            };
        }
        match &*tech.read_unchecked() {
            // Wait for the probe before setting a src — otherwise we
            // hit `/stream` optimistically, the server returns 501 for
            // transcode-needed files, the <video> fires an error, and
            // the "Can't play this video" overlay stacks underneath
            // the transcode prompt that appears once tech does resolve.
            None => String::new(),
            // Probe failed: try direct and let the browser's own error
            // surface if it can't handle it.
            Some(Err(_)) => media_stream_url(&id_for_src),
            Some(Ok(info)) => match info.browser_compat {
                // Remux goes through the new HLS pipeline (real random
                // access + keyframe-aligned fMP4 segments). Direct stays
                // on the byte-range `/stream` path for now — HLS with
                // `-c:v copy` can't repackage VP9/AV1 into fMP4, and most
                // Direct-compat files are plain MP4 that ServeFile handles
                // natively.
                BrowserCompat::Direct => media_stream_url(&id_for_src),
                BrowserCompat::Remux => media_hls_url(&id_for_src),
                // Don't set a src — the overlay below prompts the user
                // to pick a best-effort mode.
                BrowserCompat::Transcode => String::new(),
            },
        }
    });

    // True when the probe says we need transcoding and the user hasn't
    // yet picked remux/direct as a fallback.
    let show_transcode_prompt = use_memo(move || -> bool {
        if forced_mode.read().is_some() { return false; }
        matches!(
            &*tech.read_unchecked(),
            Some(Ok(info)) if info.browser_compat == BrowserCompat::Transcode
        )
    });

    // What's actually happening on the wire, accounting for user
    // overrides on top of the server's verdict. Surfaced in the debug
    // panel's Delivery section so the panel doesn't keep claiming
    // "transcode required" after the user picked remux/direct.
    let effective_mode = use_memo(move || -> BrowserCompat {
        if let Some(mode) = *forced_mode.read() {
            return match mode {
                "direct" => BrowserCompat::Direct,
                "remux" => BrowserCompat::Remux,
                _ => BrowserCompat::Transcode,
            };
        }
        match &*tech.read_unchecked() {
            Some(Ok(info)) => info.browser_compat,
            _ => BrowserCompat::Direct,
        }
    });

    // When the debug menu is open, poll runtime stats (buffered, dropped
    // frames, readyState) from the <video> element once a second.
    use_effect(move || {
        if !*debug_open.read() {
            return;
        }
        spawn(async move {
            #[cfg(feature = "web")]
            loop {
                if !*debug_open.peek() { break; }
                let mut eval = document::eval(&format!(
                    "dioxus.send(window.binkflixPlayer?.getDebugStats('{video_dom_id}') || null);"
                ));
                if let Ok(v) = eval.recv::<serde_json::Value>().await {
                    debug_stats.set(Some(v));
                }
                gloo_timers::future::TimeoutFuture::new(1000).await;
            }
        });
    });

    // Wire up custom controls whenever the <video> src transitions from
    // empty to populated — the element is conditionally rendered (we wait
    // on the tech probe + user choice for transcode-needed files), so
    // initControls must fire *after* the video mounts, not on component
    // mount when the element doesn't exist yet. `initControls` is
    // idempotent, so re-running on subsequent src changes is safe.
    use_effect(move || {
        let src = stream_src.read().clone();
        if src.is_empty() { return; }
        // Attach the source first (native or hls.js, decided by the JS
        // side based on canPlayType), then wire up the custom controls.
        // `attach` is idempotent and re-attaches cleanly when `src`
        // changes (e.g. user picks `?mode=remux` in the transcode prompt).
        let js = format!(
            "(async () => {{ await window.binkflixPlayer?.attach('{video_dom_id}', '{src}'); window.binkflixPlayer?.initControls('{video_dom_id}'); }})();"
        );
        spawn(async move {
            let _ = document::eval(&js).await;
        });
    });

    // Pause + detach the stream when the component unmounts. Without this
    // the browser keeps the range request alive (and audio playing)
    // through a soft route change, which is jarring when navigating back
    // to the library from the player.
    use_drop(move || {
        // `detach` tears down any hls.js instance attached to this video
        // element in addition to clearing src/pausing — without it the
        // hls.js xhr loop keeps running after a soft nav.
        let js = format!(
            "window.binkflixPlayer?.detach('{video_dom_id}');"
        );
        spawn(async move { let _ = document::eval(&js).await; });
    });

    use_effect(move || {
        // Don't push subtitles to a video element that hasn't mounted
        // yet (we wait on the tech probe + user choice in transcode
        // cases). The player.js setters throw "video element not found"
        // otherwise, which bubbles up as a banner at the bottom of the
        // page. When src eventually becomes non-empty this effect re-
        // runs because both signals are read here.
        if stream_src.read().is_empty() {
            return;
        }
        let cmd = sub_command.read().clone();
        if matches!(&*last_applied.peek(), Some(p) if p == &cmd) {
            return;
        }
        last_applied.set(Some(cmd.clone()));

        let js = match &cmd {
            None => format!(
                r#"
                (async () => {{
                    try {{
                        await window.binkflixPlayer?.clear('{video_dom_id}');
                        dioxus.send({{ ok: true }});
                    }} catch (e) {{
                        dioxus.send({{ ok: false, error: String(e && e.message || e) }});
                    }}
                }})();
                "#
            ),
            Some(cmd) => {
                let url = &cmd.url;
                let label = cmd.label.replace('\\', "\\\\").replace('\'', "\\'");
                let lang = cmd.language.replace('\\', "\\\\").replace('\'', "\\'");
                let call = match cmd.format {
                    SubFormat::Ass =>
                        format!("window.binkflixPlayer?.setAss('{video_dom_id}', '{url}')"),
                    SubFormat::Vtt =>
                        format!("window.binkflixPlayer?.setVtt('{video_dom_id}', '{url}', '{label}', '{lang}')"),
                };
                format!(
                    r#"
                    (async () => {{
                        const timeout = new Promise((_, rej) =>
                            setTimeout(() => rej(new Error('timed out after 15s')), 15000)
                        );
                        try {{
                            await Promise.race([{call}, timeout]);
                            dioxus.send({{ ok: true }});
                        }} catch (e) {{
                            console.error('subtitle load failed', e);
                            dioxus.send({{ ok: false, error: String(e && e.message || e) }});
                        }}
                    }})();
                    "#
                )
            }
        };
        let show_spinner = cmd.is_some();
        if show_spinner {
            loading.set(true);
            sub_error.set(None);
        }
        spawn(async move {
            let mut eval = document::eval(&js);
            let received = eval.recv::<serde_json::Value>().await;
            if show_spinner {
                loading.set(false);
            }
            match received {
                Ok(v) => {
                    let ok = v.get("ok").and_then(serde_json::Value::as_bool) == Some(true);
                    if !ok && show_spinner {
                        let msg = v
                            .get("error")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        sub_error.set(Some(msg));
                    }
                }
                Err(e) => {
                    if show_spinner {
                        sub_error.set(Some(format!("eval failed: {e}")));
                    }
                }
            }
        });
    });

    let current = effective_id.read().clone().unwrap_or_default();
    let is_loading = *loading.read();

    rsx! {
        div { class: "video-wrap",
            // Unified top bar: back link on the left, title in the middle,
            // rooms + theme controls on the right. Lives inside
            // `.video-wrap` so it auto-hides with the rest of the chrome
            // and stays visible when the wrap itself is fullscreened.
            div { class: "player-topbar",
                Link { to: back_route.clone(), class: "player-topbar-back",
                    span { dangerous_inner_html: ICON_BACK }
                    span { "Back" }
                }
                {
                    let media_snapshot = media_resource.read_unchecked().clone();
                    let show_snapshot = show_resource.read_unchecked().clone().flatten();
                    match media_snapshot {
                        Some(Ok(m)) => {
                            let (primary, secondary) = if m.kind == "episode" {
                                let ep_label = match (m.season_number, m.episode_number) {
                                    (Some(s), Some(e)) => Some(format!("S{s:02}E{e:02} · {}", m.title)),
                                    _ => Some(m.title.clone()),
                                };
                                let primary = show_snapshot.unwrap_or_else(|| m.title.clone());
                                (primary, ep_label)
                            } else {
                                (m.title.clone(), m.year.map(|y| y.to_string()))
                            };
                            rsx! {
                                div { class: "player-topbar-title",
                                    div { class: "player-topbar-primary", "{primary}" }
                                    if let Some(sec) = secondary {
                                        div { class: "player-topbar-secondary", "{sec}" }
                                    }
                                }
                            }
                        }
                        _ => rsx! { div { class: "player-topbar-title" } },
                    }
                }
                div { class: "player-topbar-right",
                    crate::syncplay_client::RoomsDropdown {}
                    crate::app::ThemeSwitcher {}
                }
            }
            // Only mount the <video> once we have a real src. We don't
            // set the `src` attribute in markup — `binkflixPlayer.attach`
            // picks between native playback (Safari's HLS support) and
            // hls.js based on browser capability, and needs to own the
            // element's source. Rendering a .m3u8 via bare `src=` fails
            // immediately on Chrome/Firefox.
            {
                let src = stream_src.read().clone();
                if !src.is_empty() {
                    rsx! {
                        video {
                            id: "{video_dom_id}",
                            autoplay: true,
                            preload: "metadata",
                        }
                    }
                } else {
                    rsx! {}
                }
            }
            // Transcode prompt: source codec isn't natively playable and
            // we don't have a real transcode path yet. Offer remux
            // (cheap, video-copy into fMP4/WebM — may or may not decode)
            // and direct (serve the raw file — browser plays if it can).
            if *show_transcode_prompt.read() {
                div { class: "player-transcode-prompt", role: "dialog",
                    div { class: "player-transcode-icon", "⚠" }
                    div { class: "player-transcode-title", "This file needs transcoding" }
                    div { class: "player-transcode-body",
                        "The source codec isn't one we can reliably play in a browser. "
                        "Transcoding isn't implemented yet — you can try remuxing (fast, may fail to decode) or direct streaming (browser decides)."
                    }
                    div { class: "player-transcode-actions",
                        button {
                            class: "player-transcode-btn primary",
                            r#type: "button",
                            onclick: move |_| forced_mode.set(Some("remux")),
                            "Try remux"
                        }
                        button {
                            class: "player-transcode-btn",
                            r#type: "button",
                            onclick: move |_| forced_mode.set(Some("direct")),
                            "Try direct"
                        }
                        Link { to: back_route.clone(), class: "player-transcode-btn", "Back" }
                    }
                }
            }
            // Loading spinner overlay — shown while `.loading` is on the
            // wrap (initial load / buffering / stalled). The class is
            // toggled from player.js.
            div { class: "player-loading", aria_hidden: "true",
                span { class: "spinner" }
            }
            // Codec / playback error overlay — shown while `.errored` is
            // on the wrap. player.js fills the inner `.player-error-msg`.
            div { class: "player-error", role: "alert",
                div { class: "player-error-icon", "⚠" }
                div { class: "player-error-body",
                    div { class: "player-error-title", "Can't play this video" }
                    div { class: "player-error-msg" }
                    Link { to: back_route.clone(), class: "player-error-back", "← Back" }
                }
            }
            div { class: "player-chrome",
                input {
                    class: "player-scrub",
                    r#type: "range",
                    min: "0",
                    max: "1000",
                    step: "1",
                    value: "0",
                }
                div { class: "player-row",
                    button {
                        class: "player-btn play-btn",
                        r#type: "button",
                        dangerous_inner_html: ICON_PLAY,
                    }
                    span { class: "player-time",
                        span { class: "time-cur", "0:00" }
                        " / "
                        span { class: "time-dur", "0:00" }
                    }
                    span { class: "player-spacer" }

                    // Subtitle menu
                    match &*tracks.read_unchecked() {
                        Some(Ok(list)) if !list.is_empty() => {
                            let list = list.clone();
                            let is_open = *menu_open.read();
                            rsx! {
                                div { class: "player-menu-wrap",
                                    button {
                                        class: "player-btn subs-btn",
                                        r#type: "button",
                                        disabled: is_loading,
                                        onclick: move |_| { let cur = *menu_open.peek(); menu_open.set(!cur); },
                                        title: "Subtitles",
                                        if is_loading {
                                            span { class: "spinner" }
                                        } else {
                                            span { dangerous_inner_html: ICON_CAPTIONS }
                                        }
                                    }
                                    if is_open {
                                        div { class: "player-menu",
                                            button {
                                                class: if current.is_empty() { "active" } else { "" },
                                                r#type: "button",
                                                onclick: move |_| {
                                                    user_pick.set(Some(None));
                                                    menu_open.set(false);
                                                },
                                                span { "Off" }
                                                if current.is_empty() {
                                                    span { class: "check", dangerous_inner_html: ICON_CHECK }
                                                }
                                            }
                                            for t in list.iter() {
                                                {
                                                    let tid = t.id.clone();
                                                    let label = subtitle_option_label(t);
                                                    let is_active = current == tid;
                                                    rsx! {
                                                        button {
                                                            key: "{tid}",
                                                            class: if is_active { "active" } else { "" },
                                                            r#type: "button",
                                                            onclick: move |_| {
                                                                user_pick.set(Some(Some(tid.clone())));
                                                                menu_open.set(false);
                                                            },
                                                            span { "{label}" }
                                                            if is_active {
                                                                span { class: "check", dangerous_inner_html: ICON_CHECK }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => rsx! {}
                    }

                    // Debug / tech-info toggle. The panel itself is
                    // rendered outside .player-chrome so it stays visible
                    // even while the chrome auto-hides during playback.
                    button {
                        class: if *debug_open.read() { "player-btn debug-btn active" } else { "player-btn debug-btn" },
                        r#type: "button",
                        title: "Playback info",
                        onclick: move |_| { let cur = *debug_open.peek(); debug_open.set(!cur); },
                        dangerous_inner_html: ICON_INFO,
                    }

                    div { class: "player-volume",
                        button { class: "player-btn volume-btn", r#type: "button" }
                        input {
                            class: "volume-slider",
                            r#type: "range",
                            min: "0",
                            max: "1",
                            step: "0.01",
                            value: "1",
                        }
                    }
                    button {
                        class: "player-btn fullscreen-btn",
                        r#type: "button",
                        title: "Fullscreen",
                        dangerous_inner_html: ICON_FULLSCREEN,
                    }
                }
                if let Some(msg) = sub_error.read().clone() {
                    div { class: "sub-error", title: "{msg}", "⚠ {msg}" }
                }
            }
            // Floating debug panel. Lives outside .player-chrome so the
            // chrome's auto-hide opacity doesn't affect it — "always on"
            // once opened, until the user hits the close button.
            if *debug_open.read() {
                {
                    let tech_snapshot = tech.read_unchecked().clone();
                    let stats_snapshot = debug_stats.read().clone();
                    rsx! {
                        div { class: "debug-panel", role: "dialog", aria_label: "Playback info",
                            div { class: "debug-panel-head",
                                span { class: "debug-panel-title", "Playback info" }
                                button {
                                    class: "debug-panel-close",
                                    r#type: "button",
                                    aria_label: "Close",
                                    title: "Close",
                                    onclick: move |_| debug_open.set(false),
                                    "×"
                                }
                            }
                            div { class: "debug-panel-body",
                                DebugMenuBody {
                                    tech: tech_snapshot,
                                    stats: stats_snapshot,
                                    effective_mode: *effective_mode.read(),
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SubFormat { Ass, Vtt }

#[derive(Clone, PartialEq)]
struct SubCommand {
    format: SubFormat,
    url: String,
    label: String,
    language: String,
}

#[component]
fn DebugMenuBody(
    tech: Option<Result<MediaTechInfo, String>>,
    stats: Option<serde_json::Value>,
    effective_mode: BrowserCompat,
) -> Element {
    // Prefer what the server actually told us over our inference: if we
    // saw `Accept-Ranges: bytes` in the response, it's a direct serve;
    // `none` is remux. The content type tells us the output container.
    // Fall back to the effective-mode hint while the HEAD probe is in
    // flight.
    let observed: Option<ObservedStream> = stats.as_ref().and_then(ObservedStream::from_stats);
    let observed_mode = observed.as_ref().map(|o| o.mode);
    let observed_container = observed.as_ref().and_then(|o| o.container());
    rsx! {
        div { class: "debug-section",
            div { class: "debug-section-title", "Playback" }
            match &stats {
                Some(v) => rsx! { DebugStatsRows { stats: v.clone() } },
                None => rsx! { div { class: "debug-row muted", "Gathering…" } },
            }
        }
        div { class: "debug-section",
            div { class: "debug-section-title", "Source" }
            match &tech {
                None => rsx! { div { class: "debug-row muted", span { class: "spinner" } " Probing…" } },
                Some(Err(e)) => rsx! { div { class: "debug-row error", "ffprobe failed: {e}" } },
                Some(Ok(info)) => rsx! { TechInfoRows { info: info.clone() } },
            }
        }
        div { class: "debug-section",
            div { class: "debug-section-title", "Delivery" }
            match &tech {
                None => rsx! { div { class: "debug-row muted", "—" } },
                Some(Err(_)) => rsx! { div { class: "debug-row muted", "—" } },
                Some(Ok(info)) => rsx! {
                    DeliveryRows {
                        info: info.clone(),
                        effective_mode: observed_mode.unwrap_or(effective_mode),
                        observed_container: observed_container.clone(),
                    }
                },
            }
        }
    }
}

/// What the HEAD probe in player.js saw on the actual stream response —
/// the authoritative signal for how the server chose to deliver the
/// file, independent of any client-side state. The mode comes from an
/// explicit `X-Stream-Mode` header rather than `Accept-Ranges`, since
/// a future transcode path would also be non-seekable and thus
/// indistinguishable from remux at the protocol level.
struct ObservedStream {
    mode: BrowserCompat,
    content_type: Option<String>,
}

impl ObservedStream {
    fn from_stats(v: &serde_json::Value) -> Option<Self> {
        let info = v.get("stream_info")?;
        if info.is_null() { return None; }
        let mode_hdr = info
            .get("mode")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());
        let content_type = info.get("content_type").and_then(|v| v.as_str()).map(String::from);
        let mode = match mode_hdr.as_deref() {
            Some("direct") => BrowserCompat::Direct,
            Some("remux") => BrowserCompat::Remux,
            Some("transcode") => BrowserCompat::Transcode,
            _ => return None,
        };
        Some(Self { mode, content_type })
    }

    /// Human-readable container derived from the observed Content-Type.
    fn container(&self) -> Option<String> {
        let ct = self.content_type.as_deref()?;
        // Strip params like `; charset=...` just in case.
        let main = ct.split(';').next()?.trim();
        Some(match main {
            "video/mp4" => "fragmented MP4".to_string(),
            "video/webm" => "WebM".to_string(),
            "video/x-matroska" => "Matroska".to_string(),
            other => other.to_string(),
        })
    }
}

#[component]
fn DeliveryRows(
    info: MediaTechInfo,
    effective_mode: BrowserCompat,
    observed_container: Option<String>,
) -> Element {
    // Describe what the browser is actually receiving on the wire, as a
    // complement to the Source section above. `effective_mode` reflects
    // user overrides ("Try remux"/"Try direct") on top of the server
    // verdict — otherwise a user who picked remux for a transcode-needed
    // file would still see "transcode required" here, which is wrong.
    match effective_mode {
        BrowserCompat::Direct => rsx! {
            DebugRow { label: "Mode", value: "direct".to_string() }
            DebugRow {
                label: "Container",
                value: observed_container
                    .clone()
                    .or_else(|| info.container.clone())
                    .unwrap_or_else(|| "—".into()),
            }
            DebugRow {
                label: "Video",
                value: info.video.as_ref().map(|v| v.codec.clone()).unwrap_or_else(|| "none".into()),
            }
            DebugRow {
                label: "Audio",
                value: info.audio.first().map(|a| a.codec.clone()).unwrap_or_else(|| "none".into()),
            }
        },
        BrowserCompat::Remux => {
            // Mirror the server's family-choice logic in remux.rs so the
            // panel describes what's actually being streamed. Observed
            // container (from Content-Type) overrides our inference when
            // available.
            let source_video = info
                .video
                .as_ref()
                .map(|v| v.codec.clone())
                .unwrap_or_else(|| "none".into());
            let source_audio = info
                .audio
                .iter()
                .find(|a| a.default)
                .or_else(|| info.audio.first())
                .map(|a| a.codec.clone());
            let webm_family = matches!(source_video.as_str(), "vp9" | "vp8" | "av1");
            let inferred_container = if webm_family { "WebM" } else { "fragmented MP4" };
            let container_label = observed_container
                .clone()
                .unwrap_or_else(|| inferred_container.to_string());
            let audio_label = match (webm_family, source_audio.as_deref()) {
                (false, Some("aac" | "mp3")) => format!("{} (copy)", source_audio.clone().unwrap()),
                (false, _) => "AAC · stereo · 192 kbps".to_string(),
                (true, Some("opus" | "vorbis")) => format!("{} (copy)", source_audio.clone().unwrap()),
                (true, _) => "Opus · stereo · 160 kbps".to_string(),
            };
            rsx! {
                DebugRow { label: "Mode", value: "remux (ffmpeg)".to_string() }
                DebugRow { label: "Container", value: container_label }
                DebugRow { label: "Video", value: format!("{source_video} (copy)") }
                DebugRow { label: "Audio", value: audio_label }
            }
        }
        BrowserCompat::Transcode => rsx! {
            DebugRow { label: "Mode", value: "transcode required".to_string() }
            DebugRow { label: "Status", value: "not implemented".to_string() }
        },
    }
}

#[component]
fn DebugStatsRows(stats: serde_json::Value) -> Element {
    let get_str = |k: &str| stats.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let get_num = |k: &str| stats.get(k).and_then(|v| v.as_f64());
    let get_bool = |k: &str| stats.get(k).and_then(|v| v.as_bool());
    let get_i64 = |k: &str| stats.get(k).and_then(|v| v.as_i64());

    let w = get_num("videoWidth").unwrap_or(0.0) as u32;
    let h = get_num("videoHeight").unwrap_or(0.0) as u32;
    let rendered = if w > 0 && h > 0 { format!("{w}×{h}") } else { "—".to_string() };
    let ready = get_str("readyState").unwrap_or_else(|| "—".into());
    let buffered_ahead = get_num("buffered_ahead_seconds")
        .map(|s| format!("{s:.1}s"))
        .unwrap_or_else(|| "—".into());
    let rate = get_num("playback_rate")
        .map(|r| format!("{r:.2}×"))
        .unwrap_or_else(|| "—".into());
    let dropped = get_i64("dropped_frames");
    let total = get_i64("total_frames");
    let frames = match (dropped, total) {
        (Some(d), Some(t)) if t > 0 => format!("{d} dropped / {t}"),
        (Some(d), _) => format!("{d} dropped"),
        _ => "—".into(),
    };
    let err = stats.get("error").and_then(|e| {
        if e.is_null() { None } else {
            let code = e.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("");
            Some(if msg.is_empty() { format!("code {code}") } else { format!("code {code}: {msg}") })
        }
    });
    let muted = get_bool("muted").unwrap_or(false);
    let volume = get_num("volume").unwrap_or(1.0);
    let volume_label = if muted { "muted".to_string() } else { format!("{:.0}%", volume * 100.0) };

    rsx! {
        DebugRow { label: "Rendered", value: rendered }
        DebugRow { label: "Ready", value: ready }
        DebugRow { label: "Buffer ahead", value: buffered_ahead }
        DebugRow { label: "Speed", value: rate }
        DebugRow { label: "Frames", value: frames }
        DebugRow { label: "Volume", value: volume_label }
        if let Some(e) = err {
            DebugRow { label: "Error", value: e }
        }
    }
}

#[component]
fn TechInfoRows(info: MediaTechInfo) -> Element {
    let container = info.container.clone().unwrap_or_else(|| "—".into());
    let duration = info.duration_seconds.map(fmt_hms).unwrap_or_else(|| "—".into());
    let total_bitrate = info
        .bitrate_kbps
        .map(fmt_kbps)
        .unwrap_or_else(|| "—".into());
    let file_size = info
        .file_size
        .map(fmt_bytes)
        .unwrap_or_else(|| "—".into());

    let video_line = match &info.video {
        None => "none".to_string(),
        Some(v) => {
            let mut parts: Vec<String> = vec![v.codec.clone()];
            if let Some(p) = &v.profile { parts.push(p.clone()); }
            if let (Some(w), Some(h)) = (v.width, v.height) {
                parts.push(format!("{w}×{h}"));
            }
            if let Some(f) = v.fps { parts.push(format!("{f:.3} fps")); }
            if let Some(b) = v.bitrate_kbps { parts.push(fmt_kbps(b)); }
            if let Some(pf) = &v.pix_fmt { parts.push(pf.clone()); }
            parts.join(" · ")
        }
    };

    rsx! {
        DebugRow { label: "Container", value: container }
        DebugRow { label: "Duration", value: duration }
        DebugRow { label: "Size", value: file_size }
        DebugRow { label: "Bitrate", value: total_bitrate }
        DebugRow { label: "Video", value: video_line }
        if info.audio.is_empty() {
            DebugRow { label: "Audio", value: "none".to_string() }
        } else {
            for (i, a) in info.audio.iter().enumerate() {
                {
                    let mut parts: Vec<String> = vec![a.codec.clone()];
                    if let Some(layout) = &a.channel_layout {
                        parts.push(layout.clone());
                    } else if let Some(c) = a.channels {
                        parts.push(format!("{c}ch"));
                    }
                    if let Some(sr) = a.sample_rate_hz {
                        parts.push(format!("{:.1} kHz", sr as f64 / 1000.0));
                    }
                    if let Some(b) = a.bitrate_kbps { parts.push(fmt_kbps(b)); }
                    if let Some(lang) = &a.language { parts.push(lang.clone()); }
                    if a.default { parts.push("default".into()); }
                    let label = if info.audio.len() > 1 {
                        format!("Audio {}", i + 1)
                    } else {
                        "Audio".into()
                    };
                    rsx! { DebugRow { key: "{i}", label: label, value: parts.join(" · ") } }
                }
            }
        }
    }
}

#[component]
fn DebugRow(label: String, value: String) -> Element {
    rsx! {
        div { class: "debug-row",
            span { class: "debug-label", "{label}" }
            span { class: "debug-value", "{value}" }
        }
    }
}

fn fmt_hms(s: f64) -> String {
    if !s.is_finite() || s < 0.0 { return "—".into(); }
    let total = s as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let sec = total % 60;
    if h > 0 { format!("{h}:{m:02}:{sec:02}") } else { format!("{m}:{sec:02}") }
}

fn fmt_kbps(kbps: u64) -> String {
    if kbps >= 1000 {
        format!("{:.1} Mbps", kbps as f64 / 1000.0)
    } else {
        format!("{kbps} kbps")
    }
}

fn fmt_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0_f64.powi(3) {
        format!("{:.2} GB", b / 1024.0_f64.powi(3))
    } else if b >= 1024.0_f64.powi(2) {
        format!("{:.1} MB", b / 1024.0_f64.powi(2))
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn subtitle_option_label(t: &SubtitleTrack) -> String {
    let mut s = t.label.clone();
    if !t.language.is_empty() && !s.to_lowercase().contains(&t.language.to_lowercase()) {
        s = format!("{s} ({})", t.language);
    }
    if t.forced { s.push_str(" · forced"); }
    if t.default { s.push_str(" · default"); }
    s
}
