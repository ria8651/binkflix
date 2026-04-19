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

    // Wire up custom controls once on mount.
    use_effect(move || {
        let js = format!(
            "window.binkflixPlayer?.initControls('{video_dom_id}');"
        );
        spawn(async move {
            let _ = document::eval(&js).await;
        });
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

fn subtitle_option_label(t: &SubtitleTrack) -> String {
    let mut s = t.label.clone();
    if !t.language.is_empty() && !s.to_lowercase().contains(&t.language.to_lowercase()) {
        s = format!("{s} ({})", t.language);
    }
    if t.forced { s.push_str(" · forced"); }
    if t.default { s.push_str(" · default"); }
    s
}
