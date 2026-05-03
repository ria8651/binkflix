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

/// Per-session user override on top of the server's compat verdict.
#[derive(Clone, Copy, PartialEq)]
struct PlaybackOverride {
    /// `None` = follow the auto verdict (Direct > Remux > Transcode).
    mode: Option<BrowserCompat>,
    /// Transcode bitrate in kbps. `None` = "Auto" (server picks from source).
    bitrate_kbps: Option<u32>,
}

/// Bitrate menu presets. `None` = "Auto" — the server derives a bitrate
/// from the source. Each numeric option carries the matching auto-derived
/// height label for display.
const BITRATE_PRESETS: &[(Option<u32>, &str)] = &[
    (None, "Auto"),
    (Some(8000), "8 Mbps · ~1080p"),
    (Some(4000), "4 Mbps · ~720p"),
    (Some(2000), "2 Mbps · ~480p"),
    (Some(1000), "1 Mbps · ~360p"),
];

const ICON_PLAY: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor" aria-hidden="true"><path d="M8 5v14l11-7z"/></svg>"#;
const ICON_BACK: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M19 12H5M12 19l-7-7 7-7"/></svg>"#;
const ICON_CAPTIONS: &str = r#"<svg viewBox="0 0 24 24" width="22" height="22" fill="currentColor" aria-hidden="true"><path d="M19 4H5c-1.11 0-2 .9-2 2v12c0 1.1.89 2 2 2h14c1.1 0 2-.9 2-2V6c0-1.1-.9-2-2-2zm-8 7H9.5v-.5h-2v3h2V13H11v1c0 .55-.45 1-1 1H7c-.55 0-1-.45-1-1v-4c0-.55.45-1 1-1h3c.55 0 1 .45 1 1v1zm7 0h-1.5v-.5h-2v3h2V13H18v1c0 .55-.45 1-1 1h-3c-.55 0-1-.45-1-1v-4c0-.55.45-1 1-1h3c.55 0 1 .45 1 1v1z"/></svg>"#;
const ICON_FULLSCREEN: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 9V4h5M21 9V4h-5M3 15v5h5M21 15v5h-5"/></svg>"#;
const ICON_INFO: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 16v-5M12 8h.01"/></svg>"#;
const ICON_CHECK: &str = r#"<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6L9 17l-5-5"/></svg>"#;
const ICON_AUDIO: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 10v4M7 7v10M11 4v16M15 8v8M19 11v2"/></svg>"#;
const ICON_SETTINGS: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>"#;
const ICON_PREV: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor" aria-hidden="true"><path d="M6 6h2v12H6zM20 6v12L9 12z"/></svg>"#;
const ICON_NEXT: &str = r#"<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor" aria-hidden="true"><path d="M16 6h2v12h-2zM4 6v12l11-6z"/></svg>"#;

#[component]
pub fn VideoPlayer(id: String, back_route: crate::app::Route) -> Element {
    // The component is keyed on `id` from `MediaPlay`, so a different
    // episode triggers a full unmount + remount with fresh state. Inside
    // the component we therefore treat `id` as a constant for the
    // lifetime of this mount and just clone it where async closures need
    // it; no signal-tracking or "did the id change?" gating is required.
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
        let track_id = effective_id.read().clone()?;
        let tracks_read = tracks.read();
        let Some(Ok(list)) = &*tracks_read else { return None };
        let track = list.iter().find(|t| t.id == track_id)?;
        Some(SubCommand {
            format: if track.format == "ass" { SubFormat::Ass } else { SubFormat::Vtt },
            url: media_subtitle_url(&apply_id, &track.id),
            label: track.label.clone(),
            language: track.language.clone(),
        })
    });

    // Audio-track picker. `None` = haven't touched (default = first
    // stream). `Some(N)` = explicit N. The dropdown only renders when
    // the source has ≥2 audio tracks.
    let mut audio_pick = use_signal(|| None::<u32>);
    let effective_audio = use_memo(move || -> u32 { audio_pick.read().unwrap_or(0) });

    let mut loading = use_signal(|| false);
    let mut last_applied = use_signal(|| None::<Option<SubCommand>>);
    // Use the Shell's shared OpenMenu context (rather than a local
    // signal) so the global pointerdown handler — which closes any
    // popover on outside-click — also closes the subtitle picker.
    // The wrapping div carries `data-popover="subtitles"` to mark
    // its descendants as "inside".
    let mut open_menu = use_context::<crate::app::OpenMenu>().0;
    let mut debug_open = use_signal(|| false);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut debug_stats = use_signal(|| None::<serde_json::Value>);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut hls_state = use_signal(|| None::<HlsState>);

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
                    Some(sid) => get_show(sid).await.ok(),
                    None => None,
                },
                _ => None,
            }
        }
    });

    // Prev/next episode IDs derived from the show's full episode list.
    // Empty for movies / one-off media or when the show fetch hasn't
    // resolved yet. Server returns seasons/episodes already ordered by
    // (season_number, episode_number), so a flat iteration is enough.
    let neighbors = use_memo(move || -> (Option<String>, Option<String>) {
        let media_snapshot = media_resource.read_unchecked().clone();
        let show_snapshot = show_resource.read_unchecked().clone().flatten();
        let (Some(Ok(m)), Some(detail)) = (media_snapshot, show_snapshot) else {
            return (None, None);
        };
        if m.kind != "episode" {
            return (None, None);
        }
        let flat: Vec<&EpisodeSummary> = detail
            .seasons
            .iter()
            .flat_map(|s| s.episodes.iter())
            .collect();
        let Some(i) = flat.iter().position(|e| e.id == m.id) else {
            return (None, None);
        };
        let prev = if i > 0 { Some(flat[i - 1].id.clone()) } else { None };
        let next = flat.get(i + 1).map(|e| e.id.clone());
        (prev, next)
    });

    // User-override on top of the server's compat verdict. `mode = None`
    // means "follow whatever the probe says"; explicit Some(...) pins one
    // of Direct/Remux/Transcode. `bitrate_kbps = None` means "Auto" —
    // the server derives a bitrate from the source.
    //
    // Hydrated from server-side prefs on mount (see prefs_resource +
    // hydration effect below) so reopening an episode of the same show
    // remembers the previous Mode/Bitrate choice.
    let mut playback_override = use_signal(|| PlaybackOverride { mode: None, bitrate_kbps: None });

    // ---- Sticky playback prefs ----------------------------------------
    // Scope key: episodes share preferences across the whole show
    // (`show:<id>`); movies key by media id. Falls back to media id when
    // an episode is missing show_id (an oddity, but safe).
    let scope_key = use_memo(move || -> Option<String> {
        let m = media_resource.read_unchecked().clone();
        match m {
            Some(Ok(m)) => {
                if m.kind == "episode" {
                    Some(
                        m.show_id
                            .clone()
                            .map(|sid| format!("show:{sid}"))
                            .unwrap_or_else(|| format!("media:{}", m.id)),
                    )
                } else {
                    Some(format!("media:{}", m.id))
                }
            }
            _ => None,
        }
    });

    let prefs_resource = use_resource(move || {
        let scope = scope_key.read().clone();
        async move {
            match scope {
                Some(s) => get_preferences(&s).await.ok().flatten(),
                // No scope yet (media_resource still in flight) — keep
                // the resource pending instead of resolving to "no
                // prefs". A premature `None` resolution would let the
                // hydration effect race ahead and stick `hydrated=true`
                // before the real fetch ever runs.
                None => std::future::pending::<Option<MediaPreferences>>().await,
            }
        }
    });

    let mut hydrated = use_signal(|| false);
    let mut last_saved = use_signal(MediaPreferences::default);

    // Apply prefs once tech, tracks, and prefs have all resolved. We wait
    // on tech and tracks so audio-index / subtitle-id matching has the
    // real track lists to work against; otherwise we'd race the player
    // mount and hand it stale defaults.
    use_effect(move || {
        if *hydrated.peek() {
            return;
        }
        // Wait for scope_key first: until media_resource resolves, scope
        // is None and prefs_resource short-circuits to a "ready, but
        // empty" Some(None). Without this gate we'd hydrate that empty
        // value (sticking hydrated=true), then ignore the real fetch
        // that fires once scope finally lands.
        if scope_key.read().is_none() {
            return;
        }
        let prefs_ready = prefs_resource.read_unchecked().is_some();
        let tracks_ready = tracks.read_unchecked().is_some();
        let tech_ready = tech.read_unchecked().is_some();
        if !prefs_ready || !tracks_ready || !tech_ready {
            return;
        }
        let prefs = prefs_resource
            .read_unchecked()
            .clone()
            .flatten()
            .unwrap_or_default();

        // Subtitle: empty stored id = explicit Off; otherwise prefer
        // exact id match, fall back to language match.
        let sub_tracks: Vec<SubtitleTrack> = match &*tracks.read_unchecked() {
            Some(Ok(list)) => list.clone(),
            _ => Vec::new(),
        };
        match prefs.subtitle_id.as_deref() {
            None => {} // no pref — leave default behavior
            Some("") => user_pick.set(Some(None)),
            Some(id) => {
                if sub_tracks.iter().any(|t| t.id == id) {
                    user_pick.set(Some(Some(id.to_string())));
                } else if let Some(lang) = prefs.subtitle_lang.as_deref() {
                    if let Some(t) = sub_tracks.iter().find(|t| t.language == lang) {
                        user_pick.set(Some(Some(t.id.clone())));
                    }
                }
            }
        }

        // Audio: if the stored index resolves (and language, if stored,
        // still matches) trust it; otherwise hop to the first track with
        // the same language.
        let audio_tracks: Vec<AudioTrackInfo> = match &*tech.read_unchecked() {
            Some(Ok(info)) => info.audio.clone(),
            _ => Vec::new(),
        };
        if let Some(idx) = prefs.audio_idx {
            let lang = prefs.audio_lang.as_deref();
            let chosen = audio_tracks.get(idx as usize).and_then(|t| {
                let ok = match (lang, t.language.as_deref()) {
                    (None, _) => true,
                    (Some(a), Some(b)) => a == b,
                    (Some(_), None) => false,
                };
                if ok { Some(idx) } else { None }
            });
            let chosen = chosen.or_else(|| {
                lang.and_then(|l| {
                    audio_tracks
                        .iter()
                        .position(|t| t.language.as_deref() == Some(l))
                        .map(|i| i as u32)
                })
            });
            if let Some(idx) = chosen {
                audio_pick.set(Some(idx));
            }
        }

        // Quality: apply mode + bitrate verbatim — these are about the
        // user's network/device, not the file's tracks.
        let mode = match prefs.transcode_mode.as_deref() {
            Some("direct") => Some(BrowserCompat::Direct),
            Some("remux") => Some(BrowserCompat::Remux),
            Some("transcode") => Some(BrowserCompat::Transcode),
            _ => None,
        };
        playback_override.set(PlaybackOverride { mode, bitrate_kbps: prefs.bitrate_kbps });

        last_saved.set(prefs);
        hydrated.set(true);
    });

    // Save on change. Subscribes to user_pick / audio_pick / override;
    // builds a fresh prefs payload and POSTs only if it differs from the
    // last-saved snapshot. The `hydrated` gate keeps the very first run
    // (which fires as we apply hydrated values) from immediately echoing
    // them back.
    use_effect(move || {
        if !*hydrated.read() {
            return;
        }
        let Some(scope) = scope_key.read().clone() else { return };
        let sub_tracks_snap: Vec<SubtitleTrack> = match &*tracks.read_unchecked() {
            Some(Ok(list)) => list.clone(),
            _ => Vec::new(),
        };
        let audio_tracks_snap: Vec<AudioTrackInfo> = match &*tech.read_unchecked() {
            Some(Ok(info)) => info.audio.clone(),
            _ => Vec::new(),
        };

        let (subtitle_id, subtitle_lang) = match user_pick.read().clone() {
            None => (None, None),
            Some(None) => (Some(String::new()), None),
            Some(Some(id)) => {
                let lang = sub_tracks_snap
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.language.clone());
                (Some(id), lang)
            }
        };
        let (audio_idx, audio_lang, audio_codec) = match *audio_pick.read() {
            None => (None, None, None),
            Some(i) => {
                let info = audio_tracks_snap.get(i as usize);
                (
                    Some(i),
                    info.and_then(|t| t.language.clone()),
                    info.map(|t| t.codec.clone()),
                )
            }
        };
        let ovr = *playback_override.read();
        let transcode_mode = ovr.mode.map(|m| match m {
            BrowserCompat::Direct => "direct".to_string(),
            BrowserCompat::Remux => "remux".to_string(),
            BrowserCompat::Transcode => "transcode".to_string(),
        });
        let cur = MediaPreferences {
            subtitle_id,
            subtitle_lang,
            audio_idx,
            audio_lang,
            audio_codec,
            transcode_mode,
            bitrate_kbps: ovr.bitrate_kbps,
        };
        if cur == *last_saved.peek() {
            return;
        }
        last_saved.set(cur.clone());
        spawn(async move {
            #[cfg(feature = "web")]
            {
                let _ = set_preferences(&scope, &cur).await;
            }
            #[cfg(not(feature = "web"))]
            {
                let _ = (scope, cur);
            }
        });
    });

    // Auto-pick: pick the simplest mode the file supports. Direct >
    // Remux > Transcode. Probe failure falls back to Direct so the
    // browser's own error surface takes over rather than us silently
    // 501ing.
    let auto_mode = use_memo(move || -> BrowserCompat {
        match &*tech.read_unchecked() {
            Some(Ok(info)) => info.browser_compat,
            _ => BrowserCompat::Direct,
        }
    });

    // What we'll actually deliver, accounting for user override.
    let effective_mode = use_memo(move || -> BrowserCompat {
        playback_override.read().mode.unwrap_or_else(|| *auto_mode.read())
    });

    // Compute the src to hand the video element. Empty string means "don't
    // load yet" — we're still waiting on the tech probe.
    let id_for_src = id.clone();
    let stream_src = use_memo(move || -> String {
        // Wait for the probe before setting a src — otherwise we'd hit
        // `/stream` optimistically and risk a redirect/error race. Also
        // wait for sticky-prefs hydration so we mount the <video> with
        // the right audio track / mode / bitrate from the get-go (avoids
        // a re-attach when the saved pref lands a beat after probe).
        let probed = matches!(&*tech.read_unchecked(), Some(_));
        let hyd = *hydrated.read();
        if !probed || !hyd {
            return String::new();
        }
        let aidx = *effective_audio.read();
        let mode = *effective_mode.read();
        let bitrate = playback_override.read().bitrate_kbps;
        match mode {
            // Direct serve only carries the file's first audio track.
            // Picking a non-default track transparently switches to HLS
            // so the server can `-map 0:a:N?` the right one.
            BrowserCompat::Direct => {
                if aidx == 0 {
                    media_stream_url(&id_for_src)
                } else {
                    media_hls_url(&id_for_src, aidx, "remux", None)
                }
            }
            BrowserCompat::Remux => media_hls_url(&id_for_src, aidx, "remux", None),
            BrowserCompat::Transcode => media_hls_url(&id_for_src, aidx, "transcode", bitrate),
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

    // Same idea but for the HLS pipeline state (producer head, cached
    // segments). Polled whenever the file is going through the HLS
    // pipeline (Remux or Transcode) — we use `producer.head` to drive
    // the "Remuxing…/Transcoding… seg N/total" label inside the loading
    // overlay so the user can tell whether ffmpeg is making progress
    // while the player buffers. A `poll_alive` gate lets the component
    // drop the loop on unmount.
    let mut poll_alive = use_signal(|| true);
    use_drop(move || poll_alive.set(false));
    // Polling task — depends on whether we're in the HLS pipeline, plus a
    // generation counter so re-runs (e.g. probe-pending → probe-resolved
    // toggles `effective_mode`) cancel the old spawned loop instead of
    // running it in parallel with the new one. The loop reads audio /
    // mode / bitrate / id via `peek()` each tick, so settings changes
    // don't even need to re-fire the effect — and the active loop always
    // queries with the current values.
    let id_for_hls_state = id.clone();
    let mut hls_poll_gen = use_signal(|| 0u64);
    use_effect(move || {
        let in_pipeline = matches!(
            *effective_mode.read(),
            BrowserCompat::Remux | BrowserCompat::Transcode
        );
        if !in_pipeline {
            return;
        }
        let my_gen = *hls_poll_gen.peek() + 1;
        hls_poll_gen.set(my_gen);
        #[cfg_attr(not(feature = "web"), allow(unused_variables))]
        let id = id_for_hls_state.clone();
        spawn(async move {
            #[cfg(feature = "web")]
            loop {
                if !*poll_alive.peek() { break; }
                if *hls_poll_gen.peek() != my_gen { break; }
                let aidx = *effective_audio.peek();
                let mode_str = match *effective_mode.peek() {
                    BrowserCompat::Transcode => "transcode",
                    _ => "remux",
                };
                let bitrate = playback_override.peek().bitrate_kbps;
                match get_hls_state(&id, aidx, mode_str, bitrate).await {
                    Ok(s) => hls_state.set(Some(s)),
                    Err(_) => { /* leave previous; next tick retries */ }
                }
                gloo_timers::future::TimeoutFuture::new(1000).await;
            }
            #[cfg(not(feature = "web"))]
            { let _ = id; }
        });
    });

    // End-of-video flag, polled from the JS side so the "Up next" overlay
    // can render without an autoplay path. Reuses `poll_alive` so the
    // loop dies on unmount.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut is_ended = use_signal(|| false);
    let mut ended_poll_gen = use_signal(|| 0u64);
    use_effect(move || {
        if stream_src.read().is_empty() {
            return;
        }
        let my_gen = *ended_poll_gen.peek() + 1;
        ended_poll_gen.set(my_gen);
        spawn(async move {
            #[cfg(feature = "web")]
            loop {
                if !*poll_alive.peek() { break; }
                if *ended_poll_gen.peek() != my_gen { break; }
                let mut eval = document::eval(&format!(
                    "dioxus.send(window.binkflixPlayer?.getPlaybackState('{video_dom_id}')?.ended ?? false);"
                ));
                let ended = match eval.recv::<serde_json::Value>().await {
                    Ok(v) => v.as_bool().unwrap_or(false),
                    Err(_) => false,
                };
                if *is_ended.peek() != ended {
                    is_ended.set(ended);
                }
                gloo_timers::future::TimeoutFuture::new(750).await;
            }
        });
    });

    // Wire up custom controls whenever the <video> src transitions from
    // empty to populated — the element is conditionally rendered (we wait
    // on the tech probe + user choice for transcode-needed files), so
    // initControls must fire *after* the video mounts, not on component
    // mount when the element doesn't exist yet. `initControls` is
    // idempotent, so re-running on subsequent src changes is safe.
    let id_for_resume = id.clone();
    #[cfg_attr(not(feature = "web"), allow(unused_variables))]
    let room_ctx = crate::syncplay_client::use_room_context();
    use_effect(move || {
        let src = stream_src.read().clone();
        if src.is_empty() { return; }
        // Capture the live currentTime *before* swapping the source so
        // a mid-playback src change (audio-track switch, transcode-prompt
        // mode flip) resumes where the user was. Returns 0 on the very
        // first attach since the element hasn't loaded anything yet —
        // we fall through to the saved-progress lookup in that case.
        // (Episode changes don't go through this code path: the parent
        // re-keys `VideoPlayer` on `id`, so a new episode = new mount =
        // fresh `<video>` with currentTime 0.)
        let capture_js = format!(
            "dioxus.send(window.binkflixPlayer?.getPlaybackState('{video_dom_id}')?.currentTime ?? 0);"
        );
        let id_for_resume = id_for_resume.clone();
        spawn(async move {
            // Resolve the user's intended start position. Priority:
            //   1. live currentTime carried over from a previous src
            //      on the same `<video>` element (mode/bitrate/audio
            //      switch — strictly freshest).
            //   2. saved-progress from the API on cold loads, unless
            //      we're joining a syncplay room currently watching
            //      this media (the room's catch-up is authoritative).
            //   3. zero — fresh start.
            //
            // The resolved time is appended to the m3u8 URL as `?t=`.
            // The server emits `#EXT-X-START:TIME-OFFSET=<t>` in the
            // playlist, which both hls.js and Safari respect — so the
            // player's first segment fetch lands at the right idx and
            // the transcode producer never spawns at seg 1 in pursuit
            // of an init.mp4 the user doesn't care about.
            //
            // `initialTime` is also passed to attach() and surfaces as
            // hls.js's `startPosition` config — belt-and-braces for
            // older clients that don't honour EXT-X-START.
            #[cfg_attr(not(feature = "web"), allow(unused_variables))]
            let live_pos: f64 = {
                #[cfg(feature = "web")]
                {
                    let mut eval = document::eval(&capture_js);
                    eval.recv::<f64>().await.unwrap_or(0.0)
                }
                #[cfg(not(feature = "web"))]
                { let _ = capture_js; 0.0 }
            };
            #[allow(unused_mut)]
            let mut initial_time: f64 = if live_pos > 5.0 { live_pos } else { 0.0 };
            #[cfg(feature = "web")]
            if initial_time == 0.0
                && !room_ctx
                    .current
                    .peek()
                    .as_ref()
                    .map(|s| s.media_id == id_for_resume && room_ctx.room_id.peek().is_some())
                    .unwrap_or(false)
            {
                if let Ok(Some(p)) = get_progress(&id_for_resume).await {
                    if !p.completed && p.position_secs > 5.0 {
                        initial_time = p.position_secs;
                    }
                }
            }

            // Append `?t=` to the stream URL when we have a resolved
            // start position. Done here (rather than in `stream_src`)
            // because the time depends on async state that isn't
            // available at memo-evaluation time.
            let attach_url = if initial_time > 5.0 && src.contains("/hls/") {
                let sep = if src.contains('?') { '&' } else { '?' };
                format!("{src}{sep}t={initial_time:.3}")
            } else {
                src.clone()
            };

            // Attach the source (native or hls.js, decided by the JS
            // side based on canPlayType), then wire up the custom
            // controls. `attach` is idempotent and re-attaches cleanly
            // when `src` changes.
            let js = format!(
                "(async () => {{ await window.binkflixPlayer?.attach('{video_dom_id}', '{attach_url}', {{ initialTime: {initial_time} }}); window.binkflixPlayer?.initControls('{video_dom_id}'); }})();"
            );
            let _ = document::eval(&js).await;

            // Force the video element's currentTime to the resolved
            // position. EXT-X-START + hls.js startPosition handle the
            // *segment fetch* alignment, but they don't move the video
            // element — without this, the element's currentTime starts
            // at 0 after the src swap and the player visibly jumps to
            // the head of the file (and plays seg-1 audio for a frame)
            // before hls.js's seek-driven fetch lands. seekTo waits for
            // metadata to load before applying.
            #[cfg(feature = "web")]
            if initial_time > 5.0 {
                let js = format!(
                    "window.binkflixPlayer?.seekTo('{video_dom_id}', {initial_time});"
                );
                let _ = document::eval(&js).await;
            }
            #[cfg(not(feature = "web"))]
            { let _ = id_for_resume; }
        });
    });

    // Heartbeat: every 10s while the player is mounted, POST current
    // position back so the home page's Continue Watching row stays fresh
    // and the next session can resume. Suppressed while paused (no
    // forward progress) and while duration is unknown (still loading).
    let id_for_heartbeat = id.clone();
    let mut heartbeat_gen = use_signal(|| 0u64);
    use_effect(move || {
        if stream_src.read().is_empty() {
            return;
        }
        let my_gen = *heartbeat_gen.peek() + 1;
        heartbeat_gen.set(my_gen);
        let id = id_for_heartbeat.clone();
        spawn(async move {
            #[cfg(feature = "web")]
            {
                let mut last_pos: f64 = -1.0;
                loop {
                    gloo_timers::future::TimeoutFuture::new(10_000).await;
                    if *heartbeat_gen.peek() != my_gen { break; }
                    let mut eval = document::eval(&format!(
                        "dioxus.send(window.binkflixPlayer?.getPlaybackState('{video_dom_id}') || null);"
                    ));
                    let Ok(v) = eval.recv::<serde_json::Value>().await else { break };
                    let Some(state) = v.as_object() else { continue };
                    let pos = state.get("currentTime").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let dur = state.get("duration").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let paused = state.get("paused").and_then(|x| x.as_bool()).unwrap_or(true);
                    if dur <= 0.0 { continue; }
                    if paused && (pos - last_pos).abs() < 0.25 { continue; }
                    let _ = report_progress(&id, pos, dur).await;
                    last_pos = pos;
                }
            }
            #[cfg(not(feature = "web"))]
            { let _ = id; }
        });
    });

    // Pause + detach the stream when the component unmounts. Without this
    // the browser keeps the range request alive (and audio playing)
    // through a soft route change, which is jarring when navigating back
    // to the library from the player. Also flush one last progress
    // report so the resume position stays accurate to the second.
    let id_for_drop = id.clone();
    use_drop(move || {
        // `detach` tears down any hls.js instance attached to this video
        // element in addition to clearing src/pausing — without it the
        // hls.js xhr loop keeps running after a soft nav.
        let media_id = id_for_drop.clone();
        let js = format!(
            "window.binkflixPlayer?.flushProgress('{video_dom_id}', '{media_id}'); window.binkflixPlayer?.detach('{video_dom_id}');"
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
                        room_ctx.push_event(format!("⚠ subtitle: {msg}"));
                    }
                }
                Err(e) => {
                    if show_spinner {
                        room_ctx.push_event(format!("⚠ subtitle: eval failed: {e}"));
                    }
                }
            }
        });
    });

    let current = effective_id.read().clone().unwrap_or_default();
    let is_loading = *loading.read();

    let wrap_class = if stream_src.read().is_empty() {
        // No <video> mounted yet — JS isn't there to toggle .loading,
        // so apply it from Rust so the spinner overlay still shows.
        "video-wrap loading"
    } else {
        "video-wrap"
    };

    rsx! {
        div { class: wrap_class,
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
                    let show_title = show_resource
                        .read_unchecked()
                        .clone()
                        .flatten()
                        .map(|d| d.show.title);
                    match media_snapshot {
                        Some(Ok(m)) => {
                            let (primary, secondary) = if m.kind == "episode" {
                                let ep_label = match (m.season_number, m.episode_number) {
                                    (Some(s), Some(e)) => Some(format!("S{s:02}E{e:02} · {}", m.title)),
                                    _ => Some(m.title.clone()),
                                };
                                let primary = show_title.unwrap_or_else(|| m.title.clone());
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
                // Pull FPS from the tech probe so player.js's frame-step
                // (`,` / `.`) keys know how big a frame is. Subscribe via
                // `read()` so the attribute updates when the probe lands
                // after the element is already mounted.
                let fps_attr = tech.read().as_ref()
                    .and_then(|r| r.as_ref().ok())
                    .and_then(|info| info.video.as_ref())
                    .and_then(|v| v.fps)
                    .map(|f| format!("{f}"))
                    .unwrap_or_default();
                if !src.is_empty() {
                    rsx! {
                        video {
                            id: "{video_dom_id}",
                            autoplay: true,
                            preload: "metadata",
                            "data-fps": "{fps_attr}",
                            "data-media-id": "{id}",
                        }
                    }
                } else {
                    rsx! {}
                }
            }
            // Loading spinner overlay — shown while `.loading` is on the
            // wrap (initial load / buffering / stalled). The class is
            // toggled from player.js while the video is mounted, and
            // forced on from Rust above when stream_src is empty (so
            // the user gets feedback while waiting on the tech probe).
            //
            // The label under the spinner narrates *why* we're waiting:
            //   * empty src         → "Preparing playback…"
            //   * remux + producer  → "Remuxing… seg N / total"
            //   * transcode + producer → "Transcoding… seg N / total"
            // This makes a stalled pipeline visible from the UI alone
            // instead of needing the debug panel.
            {
                let src_empty = stream_src.read().is_empty();
                let mode = *effective_mode.read();
                let label: Option<String> = if src_empty {
                    Some("Preparing playback…".to_string())
                } else if matches!(mode, BrowserCompat::Remux | BrowserCompat::Transcode) {
                    let verb = match mode {
                        BrowserCompat::Transcode => "Transcoding",
                        _ => "Remuxing",
                    };
                    hls_state.read().as_ref().and_then(|s| {
                        let p = s.producer.as_ref()?;
                        Some(format!(
                            "{verb}… seg {} / {}",
                            p.head.max(p.start_idx), s.total_segments
                        ))
                    })
                } else {
                    None
                };
                rsx! {
                    div { class: "player-loading", aria_hidden: "true",
                        span { class: "spinner" }
                        if let Some(text) = label {
                            div { class: "player-loading-label", "{text}" }
                        }
                    }
                }
            }
            // Codec / playback error overlay — shown while `.errored` is
            // on the wrap. player.js fills the inner `.player-error-msg`.
            // Two buttons in the body: Back for genuinely unrecoverable
            // cases, Dismiss for blips the user wants to ignore (HTTP
            // 5xx, transient buffer error). Both styled identically and
            // sit side-by-side so neither is a cryptic corner glyph.
            div { class: "player-error", role: "alert",
                div { class: "player-error-icon", "⚠" }
                div { class: "player-error-body",
                    div { class: "player-error-title", "Can't play this video" }
                    div { class: "player-error-msg" }
                    div { class: "player-error-actions",
                        Link { to: back_route.clone(), class: "player-error-btn", "← Back" }
                        button {
                            class: "player-error-btn",
                            r#type: "button",
                            aria_label: "Dismiss",
                            onclick: move |_| {
                                let js = format!(
                                    "window.binkflixPlayer?.dismissError('{video_dom_id}');"
                                );
                                spawn(async move { let _ = document::eval(&js).await; });
                            },
                            "Dismiss"
                        }
                    }
                }
            }
            // End-of-video "Up next" overlay. Renders only when the video
            // has actually ended *and* a next episode exists. No autoplay:
            // user must click to advance. Hidden for movies and the last
            // episode of a series.
            {
                let next_for_end = neighbors.read().1.clone();
                let show_overlay = *is_ended.read() && next_for_end.is_some();
                if show_overlay {
                    rsx! {
                        div { class: "player-end-overlay", role: "dialog", "aria-label": "Up next",
                            div { class: "player-end-card",
                                if let Some(nid) = next_for_end {
                                    Link {
                                        to: crate::app::Route::MediaPlay { id: nid },
                                        class: "btn",
                                        span { dangerous_inner_html: ICON_NEXT }
                                        span { "Play next episode" }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    rsx! {}
                }
            }
            div { class: "player-chrome",
                div { class: "player-scrub-preview", aria_hidden: "true",
                    div { class: "player-scrub-preview-img" }
                    div { class: "player-scrub-preview-time", "0:00" }
                }
                input {
                    class: "player-scrub",
                    r#type: "range",
                    min: "0",
                    max: "1000",
                    step: "1",
                    value: "0",
                }
                div { class: "player-row",
                    {
                        let (prev_id, next_id) = neighbors.read().clone();
                        rsx! {
                            if let Some(pid) = prev_id {
                                Link {
                                    to: crate::app::Route::MediaPlay { id: pid },
                                    class: "player-btn prev-ep-btn",
                                    title: "Previous episode",
                                    span { dangerous_inner_html: ICON_PREV }
                                }
                            }
                            button {
                                class: "player-btn play-btn",
                                r#type: "button",
                                dangerous_inner_html: ICON_PLAY,
                            }
                            if let Some(nid) = next_id {
                                Link {
                                    to: crate::app::Route::MediaPlay { id: nid },
                                    class: "player-btn next-ep-btn",
                                    title: "Next episode",
                                    span { dangerous_inner_html: ICON_NEXT }
                                }
                            }
                        }
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
                            let is_open = *open_menu.read() == Some("subtitles");
                            rsx! {
                                div { class: "player-menu-wrap", "data-popover": "subtitles",
                                    button {
                                        class: "player-btn subs-btn",
                                        r#type: "button",
                                        disabled: is_loading,
                                        onclick: move |_| {
                                            open_menu.set(if is_open { None } else { Some("subtitles") });
                                        },
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
                                                    open_menu.set(None);
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
                                                                open_menu.set(None);
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

                    // Audio-track menu. Only renders when the source
                    // probe resolves *and* there are at least two audio
                    // streams; single-track files don't need a picker.
                    {
                        let tech_snapshot = tech.read_unchecked().clone();
                        match tech_snapshot {
                            Some(Ok(info)) if info.audio.len() >= 2 => {
                                let tracks = info.audio.clone();
                                let is_open = *open_menu.read() == Some("audio");
                                let cur_audio = *effective_audio.read();
                                rsx! {
                                    div { class: "player-menu-wrap", "data-popover": "audio",
                                        button {
                                            class: "player-btn audio-btn",
                                            r#type: "button",
                                            onclick: move |_| {
                                                open_menu.set(if is_open { None } else { Some("audio") });
                                            },
                                            title: "Audio track",
                                            dangerous_inner_html: ICON_AUDIO,
                                        }
                                        if is_open {
                                            div { class: "player-menu",
                                                for (i, t) in tracks.iter().enumerate() {
                                                    {
                                                        let idx = i as u32;
                                                        let label = audio_option_label(t, idx);
                                                        let is_active = cur_audio == idx;
                                                        rsx! {
                                                            button {
                                                                key: "{idx}",
                                                                class: if is_active { "active" } else { "" },
                                                                r#type: "button",
                                                                onclick: move |_| {
                                                                    audio_pick.set(Some(idx));
                                                                    open_menu.set(None);
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
                    }

                    // Video settings menu. Lets the user override the
                    // server's auto-picked mode (Direct/Remux/Transcode)
                    // and choose a transcode bitrate. The auto-picked
                    // mode is checked by default; clicking it again
                    // clears the override.
                    {
                        let is_open = *open_menu.read() == Some("settings");
                        let current_auto = *auto_mode.read();
                        let cur = *playback_override.read();
                        let cur_mode = cur.mode.unwrap_or(current_auto);
                        let cur_bitrate = cur.bitrate_kbps;
                        rsx! {
                            div { class: "player-menu-wrap", "data-popover": "settings",
                                button {
                                    class: "player-btn settings-btn",
                                    r#type: "button",
                                    onclick: move |_| {
                                        open_menu.set(if is_open { None } else { Some("settings") });
                                    },
                                    title: "Video settings",
                                    dangerous_inner_html: ICON_SETTINGS,
                                }
                                if is_open {
                                    div { class: "player-menu",
                                        div { class: "player-menu-section", "Mode" }
                                        for (m, label) in [
                                            (BrowserCompat::Direct, "Direct"),
                                            (BrowserCompat::Remux, "Remux"),
                                            (BrowserCompat::Transcode, "Transcode"),
                                        ] {
                                            {
                                                let is_active = cur_mode == m;
                                                let display_label = if m == current_auto {
                                                    format!("{label} (auto)")
                                                } else {
                                                    label.to_string()
                                                };
                                                rsx! {
                                                    button {
                                                        key: "{label}",
                                                        class: if is_active { "active" } else { "" },
                                                        r#type: "button",
                                                        onclick: move |_| {
                                                            // Clicking the auto-recommended option clears
                                                            // the override — keeps the override "sticky"
                                                            // semantics minimal.
                                                            let next = if m == current_auto { None } else { Some(m) };
                                                            playback_override.with_mut(|o| o.mode = next);
                                                        },
                                                        span { "{display_label}" }
                                                        if is_active {
                                                            span { class: "check", dangerous_inner_html: ICON_CHECK }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        if matches!(cur_mode, BrowserCompat::Transcode) {
                                            div { class: "player-menu-section", "Quality" }
                                            for (preset, label) in BITRATE_PRESETS.iter().copied() {
                                                {
                                                    let is_active = cur_bitrate == preset;
                                                    rsx! {
                                                        button {
                                                            key: "{label}",
                                                            class: if is_active { "active" } else { "" },
                                                            r#type: "button",
                                                            onclick: move |_| {
                                                                playback_override.with_mut(|o| o.bitrate_kbps = preset);
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
            }
            // Floating debug panel. Lives outside .player-chrome so the
            // chrome's auto-hide opacity doesn't affect it — "always on"
            // once opened, until the user hits the close button.
            if *debug_open.read() {
                {
                    let tech_snapshot = tech.read_unchecked().clone();
                    let stats_snapshot = debug_stats.read().clone();
                    let hls_snapshot = hls_state.read().clone();
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
                                    hls: hls_snapshot,
                                    effective_mode: *effective_mode.read(),
                                    bitrate_override: playback_override.read().bitrate_kbps,
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
    hls: Option<HlsState>,
    effective_mode: BrowserCompat,
    bitrate_override: Option<u32>,
) -> Element {
    // Prefer what the server actually told us over our inference: if we
    // saw `Accept-Ranges: bytes` in the response, it's a direct serve;
    // `none` is remux. The content type tells us the output container.
    // Fall back to the effective-mode hint while the HEAD probe is in
    // flight.
    let observed: Option<ObservedStream> = stats.as_ref().and_then(ObservedStream::from_stats);
    let observed_mode = observed.as_ref().map(|o| o.mode);
    let observed_container = observed.as_ref().and_then(|o| o.container());
    let observed_encoder = observed.as_ref().and_then(|o| o.encoder.clone());
    let buffered_ranges: Vec<(f64, f64)> = stats
        .as_ref()
        .and_then(|s| s.get("buffered_ranges"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let pair = r.as_array()?;
                    let s = pair.first()?.as_f64()?;
                    let e = pair.get(1)?.as_f64()?;
                    Some((s, e))
                })
                .collect()
        })
        .unwrap_or_default();
    let current_time = stats
        .as_ref()
        .and_then(|s| s.get("current_time"))
        .and_then(|v| v.as_f64());
    rsx! {
        if matches!(effective_mode, BrowserCompat::Remux | BrowserCompat::Transcode) {
            div { class: "debug-section",
                div { class: "debug-section-title", "HLS pipeline" }
                match &hls {
                    Some(state) => rsx! {
                        HlsTimeline {
                            state: state.clone(),
                            buffered_ranges: buffered_ranges.clone(),
                            current_time,
                        }
                        HlsPipelineRows { state: state.clone() }
                    },
                    None => rsx! { div { class: "debug-row muted", "Gathering…" } },
                }
            }
        }
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
                        observed_encoder: observed_encoder.clone(),
                        bitrate_override,
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
    /// Server-reported ffmpeg H.264 encoder for transcode mode
    /// (`libx264`, `h264_vaapi`, `h264_qsv`, `h264_videotoolbox`).
    /// `None` for non-transcode responses or older servers.
    encoder: Option<String>,
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
        let encoder = info.get("encoder").and_then(|v| v.as_str()).map(String::from);
        let mode = match mode_hdr.as_deref() {
            Some("direct") => BrowserCompat::Direct,
            Some("remux") => BrowserCompat::Remux,
            Some("transcode") => BrowserCompat::Transcode,
            _ => return None,
        };
        Some(Self { mode, content_type, encoder })
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

/// Mirror the server's auto-bitrate logic so the debug panel can show
/// the same value the server would pick when the user has Auto selected.
/// Keep in sync with `resolve_bitrate` in `src/server/hls/mod.rs`.
fn auto_bitrate_kbps(source_kbps: Option<u64>) -> u32 {
    let auto = source_kbps
        .and_then(|b| u32::try_from(b).ok())
        .unwrap_or(4000);
    auto.clamp(200, 6000)
}

/// Mirror of `height_for_bitrate` in `src/server/hls/plan.rs`.
fn height_for_bitrate(bitrate_kbps: u32) -> u32 {
    match bitrate_kbps {
        b if b >= 6000 => 1080,
        b if b >= 3000 => 720,
        b if b >= 1500 => 480,
        _ => 360,
    }
}

#[component]
fn DeliveryRows(
    info: MediaTechInfo,
    effective_mode: BrowserCompat,
    observed_container: Option<String>,
    /// ffmpeg encoder name from the server's `X-Stream-Encoder` header
    /// (`libx264`, `h264_vaapi`, etc.). Drives the transcode-mode label
    /// so the panel reflects whether GPU offload is in effect.
    observed_encoder: Option<String>,
    bitrate_override: Option<u32>,
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
                if let Some(reason) = info.compat_reason.clone() {
                    DebugRow { label: "Why", value: reason }
                }
            }
        }
        BrowserCompat::Transcode => {
            let source_video = info
                .video
                .as_ref()
                .map(|v| v.codec.clone())
                .unwrap_or_else(|| "none".into());
            let container_label = observed_container
                .clone()
                .unwrap_or_else(|| "fragmented MP4".into());
            // Mirror the server's bitrate/height pick so the panel
            // shows the same numbers the producer is using. When the
            // user picks "Auto", `bitrate_override` is None and we
            // derive from the source's probed bitrate (clamped).
            let target_bitrate = bitrate_override.unwrap_or_else(|| auto_bitrate_kbps(info.bitrate_kbps));
            let target_height = height_for_bitrate(target_bitrate);
            // Source resolution is the upper bound — never upscale.
            let effective_height = info
                .video
                .as_ref()
                .and_then(|v| v.height)
                .map(|h| h.min(target_height))
                .unwrap_or(target_height);
            let bitrate_label = if bitrate_override.is_some() {
                format!("{target_bitrate} kbps")
            } else {
                format!("{target_bitrate} kbps (auto)")
            };
            let encoder_label = observed_encoder
                .clone()
                .unwrap_or_else(|| "libx264".to_string());
            rsx! {
                DebugRow { label: "Mode", value: format!("transcode ({encoder_label})") }
                DebugRow { label: "Container", value: container_label }
                DebugRow {
                    label: "Video",
                    value: format!("{source_video} → h264 · ≤{effective_height}p"),
                }
                DebugRow { label: "Bitrate", value: bitrate_label }
                DebugRow { label: "Audio", value: "AAC · stereo · 192 kbps".to_string() }
                if let Some(reason) = info.compat_reason.clone() {
                    DebugRow { label: "Why", value: reason }
                }
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

/// Stacked timeline showing what the HLS pipeline is doing right now:
/// * server-cached segments (green run)
/// * client-buffered range[s] from `<video>.buffered` (blue overlay)
/// * producer's "active window" — start_idx through head + lookahead (yellow band)
/// * producer head — vertical orange line (where ffmpeg has reached)
/// * current playback position — vertical red line
///
/// Inline-styled (not utility classes) because the absolute positions are
/// computed from runtime data; cleaner to do this in Rust than to plumb a
/// CSS variable per-segment. Stylesheet covers the static chrome.
#[component]
fn HlsTimeline(
    state: HlsState,
    buffered_ranges: Vec<(f64, f64)>,
    current_time: Option<f64>,
) -> Element {
    let dur = state.duration.max(0.001);

    // Map a segment index to (left%, width%) on the bar. Falls back to a
    // proportional slice if the segment_durations array doesn't cover this
    // index — defensive against version drift.
    let seg_rect = move |idx: u32| -> (f64, f64) {
        let n = state.segment_durations.len();
        if n == 0 || idx == 0 || (idx as usize) > n {
            return (0.0, 0.0);
        }
        let prefix: f64 = state.segment_durations[..(idx as usize - 1)].iter().sum();
        let d = state.segment_durations[idx as usize - 1];
        (prefix / dur * 100.0, d / dur * 100.0)
    };

    // Collapse contiguous cached indices into runs so the DOM only has a
    // handful of rectangles (a 1200-segment file with sequential cache =
    // one rectangle, not 1200). Keeps render cheap regardless of plan size.
    let mut cached_runs: Vec<(u32, u32)> = Vec::new();
    for &i in &state.cached_segments {
        match cached_runs.last_mut() {
            Some((_, last_end)) if *last_end + 1 == i => *last_end = i,
            _ => cached_runs.push((i, i)),
        }
    }

    let producer = state.producer.clone();
    let head_pct = producer.as_ref().and_then(|p| {
        if p.head == 0 {
            None
        } else {
            let (l, w) = seg_rect(p.head);
            Some(l + w)
        }
    });
    let window_rect = producer.as_ref().and_then(|p| {
        // Pull-driven window: from start_idx up to `target_head` —
        // the range ffmpeg is currently *allowed* to produce. Beyond
        // target_head ffmpeg is SIGSTOP'd until a new request pulls
        // it further.
        if p.start_idx == 0 || p.target_head == 0 {
            return None;
        }
        let (l1, _) = seg_rect(p.start_idx.max(1));
        let (l2, w2) = seg_rect(p.target_head.min(state.total_segments));
        Some((l1, (l2 + w2 - l1).max(0.0)))
    });

    let cur_pct = current_time.map(|t| (t / dur * 100.0).clamp(0.0, 100.0));

    rsx! {
        div { class: "hls-timeline",
            // Producer's read-ahead window (start..head+lookahead). Drawn
            // first so cached/buffered/head paint over it.
            if let Some((l, w)) = window_rect {
                div {
                    class: "hls-tl-window",
                    style: "left: {l}%; width: {w}%;",
                }
            }
            // Server-side cache (segments on disk).
            for (a, b) in cached_runs.iter().copied() {
                {
                    let (l1, _) = seg_rect(a);
                    let (l2, w2) = seg_rect(b);
                    let w = (l2 + w2 - l1).max(0.0);
                    rsx! {
                        div {
                            key: "{a}-{b}",
                            class: "hls-tl-cached",
                            style: "left: {l1}%; width: {w}%;",
                        }
                    }
                }
            }
            // Client-side buffered ranges (what the <video> can play right now).
            for (i, (s, e)) in buffered_ranges.iter().copied().enumerate() {
                {
                    let l = (s / dur * 100.0).clamp(0.0, 100.0);
                    let w = ((e - s) / dur * 100.0).max(0.0);
                    rsx! {
                        div {
                            key: "{i}",
                            class: "hls-tl-buffered",
                            style: "left: {l}%; width: {w}%;",
                        }
                    }
                }
            }
            // Producer head — vertical line at the boundary of the last
            // segment ffmpeg finished writing.
            if let Some(p) = head_pct {
                div {
                    class: if producer.as_ref().map(|p| p.paused).unwrap_or(false) {
                        "hls-tl-head paused"
                    } else {
                        "hls-tl-head"
                    },
                    style: "left: {p}%;",
                }
            }
            // Current playback position.
            if let Some(p) = cur_pct {
                div { class: "hls-tl-cursor", style: "left: {p}%;" }
            }
        }
        // Legend so colors aren't a guessing game.
        div { class: "hls-tl-legend",
            span { class: "hls-tl-swatch cached" } "cached"
            span { class: "hls-tl-swatch buffered" } "buffered"
            span { class: "hls-tl-swatch window" } "window"
            span { class: "hls-tl-swatch head" } "head"
            span { class: "hls-tl-swatch cursor" } "now"
        }
    }
}

#[component]
fn HlsPipelineRows(state: HlsState) -> Element {
    let total = state.total_segments;
    let cached_count = state.cached_segments.len() as u32;
    let cached_pct = if total > 0 {
        (cached_count as f64 / total as f64 * 100.0).round() as u32
    } else {
        0
    };
    let producer_label = match &state.producer {
        None => "idle".to_string(),
        Some(p) => {
            let status = if p.paused { "paused" } else { "running" };
            format!("{status} · seg {} / {total}", p.head)
        }
    };
    let window_label = match &state.producer {
        None => "—".to_string(),
        Some(p) => format!(
            "start {} → head {} → target {} (lookahead +{})",
            p.start_idx, p.head, p.target_head, p.lookahead_buffer
        ),
    };
    let idle_label = state
        .producer
        .as_ref()
        .map(|p| format!("{:.1}s since last request", p.idle_for_secs))
        .unwrap_or_else(|| "—".into());
    rsx! {
        DebugRow { label: "Producer", value: producer_label }
        DebugRow { label: "Window", value: window_label }
        DebugRow { label: "Idle", value: idle_label }
        DebugRow {
            label: "Cache",
            value: format!("{cached_count} / {total} segs ({cached_pct}%)"),
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

/// "English · 5.1 · ac3" / "Japanese · stereo · aac". Falls back to
/// "Track N" when the source has no language or title metadata.
fn audio_option_label(t: &AudioTrackInfo, idx: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(lang) = t.language.as_deref().filter(|s| !s.is_empty()) {
        parts.push(lang.to_string());
    } else if let Some(title) = t.title.as_deref().filter(|s| !s.is_empty()) {
        parts.push(title.to_string());
    }
    if let Some(layout) = t.channel_layout.as_deref().filter(|s| !s.is_empty()) {
        parts.push(layout.to_string());
    } else if let Some(c) = t.channels {
        parts.push(format!("{c}ch"));
    }
    if !t.codec.is_empty() {
        parts.push(t.codec.clone());
    }
    if parts.is_empty() {
        format!("Track {}", idx + 1)
    } else {
        parts.join(" · ")
    }
}
