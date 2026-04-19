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
const ICON_CAPTIONS: &str = r#"<svg viewBox="0 0 24 24" width="22" height="22" fill="currentColor" aria-hidden="true"><path d="M19 4H5c-1.11 0-2 .9-2 2v12c0 1.1.89 2 2 2h14c1.1 0 2-.9 2-2V6c0-1.1-.9-2-2-2zm-8 7H9.5v-.5h-2v3h2V13H11v1c0 .55-.45 1-1 1H7c-.55 0-1-.45-1-1v-4c0-.55.45-1 1-1h3c.55 0 1 .45 1 1v1zm7 0h-1.5v-.5h-2v3h2V13H18v1c0 .55-.45 1-1 1h-3c-.55 0-1-.45-1-1v-4c0-.55.45-1 1-1h3c.55 0 1 .45 1 1v1z"/></svg>"#;
const ICON_FULLSCREEN: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 9V4h5M21 9V4h-5M3 15v5h5M21 15v5h-5"/></svg>"#;
const ICON_INFO: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 16v-5M12 8h.01"/></svg>"#;
const ICON_CHECK: &str = r#"<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6L9 17l-5-5"/></svg>"#;

#[component]
pub fn VideoPlayer(id: String) -> Element {
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

    let id_for_tech = id.clone();
    let tech = use_resource(move || {
        let id = id_for_tech.clone();
        async move { get_media_tech(&id).await }
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

    // Wire up custom controls once on mount.
    use_effect(move || {
        let js = format!(
            "window.binkflixPlayer?.initControls('{video_dom_id}');"
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
        let js = format!(
            r#"
            const v = document.getElementById('{video_dom_id}');
            if (v) {{
                try {{ v.pause(); }} catch (_) {{}}
                v.removeAttribute('src');
                try {{ v.load(); }} catch (_) {{}}
            }}
            "#
        );
        spawn(async move { let _ = document::eval(&js).await; });
    });

    use_effect(move || {
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
            video {
                id: "{video_dom_id}",
                src: "{media_stream_url(&id)}",
                autoplay: true,
                preload: "metadata",
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
) -> Element {
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
