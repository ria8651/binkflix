use crate::client_api::*;
use crate::types::*;
use dioxus::prelude::*;

#[derive(Routable, Clone, PartialEq)]
#[rustfmt::skip]
pub enum Route {
    #[layout(Shell)]
        #[route("/")]
        Home {},
        #[route("/media/:id")]
        MediaDetail { id: String },
        #[route("/media/:id/play")]
        MediaPlay { id: String },
        #[route("/show/:id")]
        ShowDetail { id: String },
}

#[component]
pub fn App() -> Element {
    rsx! {
        document::Stylesheet { href: asset!("/assets/style.css") }
        // Synchronous stub queues calls made before the async module below
        // finishes evaluating; the module replays the queue on load.
        // Served via axum's ServeDir (see server/mod.rs) rather than the
        // Dioxus asset pipeline, which strips Content-Type — browsers reject
        // `<script type="module">` without `application/javascript`.
        document::Script { src: "/static/player-stub.js" }
        document::Script { src: "/static/player.js", r#type: "module" }
        Router::<Route> {}
    }
}

#[component]
fn Shell() -> Element {
    rsx! {
        header { class: "topbar",
            Link { to: Route::Home {}, class: "brand", "BINKFLIX" }
            span { class: "muted", "self-hosted" }
        }
        main {
            Outlet::<Route> {}
        }
    }
}

#[component]
fn Home() -> Element {
    let lib = use_resource(get_library);

    rsx! {
        match &*lib.read_unchecked() {
            None => rsx! { div { class: "empty", "Loading…" } },
            Some(Err(e)) => rsx! { div { class: "empty", "Failed to load: {e}" } },
            Some(Ok(lib)) if lib.movies.is_empty() && lib.shows.is_empty() => rsx! {
                div { class: "empty",
                    p { "Your library is empty." }
                    p { class: "muted", "Add movies and/or shows with .nfo metadata, then restart the server." }
                }
            },
            Some(Ok(lib)) => rsx! {
                if !lib.shows.is_empty() {
                    section {
                        h2 { class: "section", "Shows" }
                        div { class: "grid",
                            for s in lib.shows.iter().cloned() {
                                ShowCard { key: "{s.id}", show: s }
                            }
                        }
                    }
                }
                if !lib.movies.is_empty() {
                    section {
                        h2 { class: "section", "Movies" }
                        div { class: "grid",
                            for m in lib.movies.iter().cloned() {
                                MovieCard { key: "{m.id}", movie: m }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn MovieCard(movie: MovieSummary) -> Element {
    let nav = use_navigator();
    let id = movie.id.clone();
    let year = movie.year.map(|y| y.to_string()).unwrap_or_default();

    rsx! {
        div {
            class: "card",
            onclick: move |_| { nav.push(Route::MediaDetail { id: id.clone() }); },
            img {
                class: "poster",
                src: "{media_image_url(&movie.id)}",
                loading: "lazy",
                decoding: "async",
                alt: "{movie.title}",
            }
            div { class: "meta",
                div { class: "title", "{movie.title}" }
                div { class: "year", "{year}" }
            }
        }
    }
}

#[component]
fn ShowCard(show: ShowSummary) -> Element {
    let nav = use_navigator();
    let id = show.id.clone();
    let year = show.year.map(|y| y.to_string()).unwrap_or_default();
    let count = show.episode_count;

    rsx! {
        div {
            class: "card",
            onclick: move |_| { nav.push(Route::ShowDetail { id: id.clone() }); },
            img {
                class: "poster",
                src: "{show_poster_url(&show.id)}",
                loading: "lazy",
                decoding: "async",
                alt: "{show.title}",
            }
            div { class: "meta",
                div { class: "title", "{show.title}" }
                div { class: "year",
                    "{year}"
                    if !year.is_empty() { " · " }
                    "{count} eps"
                }
            }
        }
    }
}

#[component]
fn MediaDetail(id: String) -> Element {
    let id_clone = id.clone();
    let media = use_resource(move || {
        let id = id_clone.clone();
        async move { get_media(&id).await }
    });

    rsx! {
        match &*media.read_unchecked() {
            None => rsx! { div { class: "empty", "Loading…" } },
            Some(Err(e)) => rsx! { div { class: "empty", "Failed to load: {e}" } },
            Some(Ok(m)) => rsx! {
                div { class: "detail",
                    div {
                        class: "poster",
                        style: if m.kind == "episode" {
                            if let Some(sid) = m.show_id.as_deref() {
                                format!("background-image: url('{}')", show_poster_url(sid))
                            } else {
                                String::new()
                            }
                        } else {
                            format!("background-image: url('{}')", media_image_url(&m.id))
                        },
                    }
                    div {
                        h1 { "{m.title}" }
                        div { class: "meta-row",
                            if m.kind == "episode" {
                                if let (Some(s), Some(e)) = (m.season_number, m.episode_number) {
                                    span { "S{s:02}E{e:02}" }
                                }
                            } else if let Some(y) = m.year {
                                span { "{y}" }
                            }
                            if let Some(r) = m.runtime_minutes {
                                span { " · {r} min" }
                            }
                        }
                        if let Some(plot) = m.plot.as_deref() {
                            p { class: "plot", "{plot}" }
                        }
                        div { style: "margin-top: 1.5rem; display: flex; gap: 0.75rem;",
                            Link { to: Route::MediaPlay { id: m.id.clone() }, class: "btn", "▶ Play" }
                            if m.kind == "episode" {
                                if let Some(sid) = m.show_id.as_deref() {
                                    Link { to: Route::ShowDetail { id: sid.to_string() }, class: "btn ghost", "Show" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ShowDetail(id: String) -> Element {
    let id_clone = id.clone();
    let detail = use_resource(move || {
        let id = id_clone.clone();
        async move { get_show(&id).await }
    });

    rsx! {
        match &*detail.read_unchecked() {
            None => rsx! { div { class: "empty", "Loading…" } },
            Some(Err(e)) => rsx! { div { class: "empty", "Failed to load: {e}" } },
            Some(Ok(d)) => rsx! {
                div { class: "detail",
                    div {
                        class: "poster",
                        style: "background-image: url('{show_poster_url(&d.show.id)}')",
                    }
                    div {
                        h1 { "{d.show.title}" }
                        if let Some(y) = d.show.year {
                            div { class: "meta-row", "{y}" }
                        }
                        if let Some(plot) = d.show.plot.as_deref() {
                            p { class: "plot", "{plot}" }
                        }
                    }
                }
                for season in d.seasons.iter().cloned() {
                    SeasonBlock { key: "{season.number}", show_id: d.show.id.clone(), season }
                }
            }
        }
    }
}

#[component]
fn SeasonBlock(show_id: String, season: Season) -> Element {
    let title = if season.number == 0 {
        "Specials".to_string()
    } else {
        format!("Season {}", season.number)
    };
    rsx! {
        section { class: "season",
            div { class: "season-head",
                div {
                    class: "season-poster",
                    style: "background-image: url('{season_poster_url(&show_id, season.number)}'), url('{show_poster_url(&show_id)}')",
                }
                h2 { class: "section", "{title}" }
            }
            div { class: "episode-list",
                for ep in season.episodes.iter().cloned() {
                    EpisodeRow { key: "{ep.id}", episode: ep }
                }
            }
        }
    }
}

#[component]
fn EpisodeRow(episode: EpisodeSummary) -> Element {
    let nav = use_navigator();
    let id = episode.id.clone();

    rsx! {
        div {
            class: "episode",
            onclick: move |_| { nav.push(Route::MediaPlay { id: id.clone() }); },
            img {
                class: "ep-thumb",
                src: "{media_image_url(&episode.id)}",
                loading: "lazy",
                decoding: "async",
                alt: "",
            }
            div { class: "ep-body",
                div { class: "ep-title",
                    span { class: "ep-num", "S{episode.season_number:02}E{episode.episode_number:02}" }
                    " · "
                    "{episode.title}"
                }
                if let Some(plot) = episode.plot.as_deref() {
                    p { class: "ep-plot", "{plot}" }
                }
            }
        }
    }
}

#[component]
fn MediaPlay(id: String) -> Element {
    let id_clone = id.clone();
    let media = use_resource(move || {
        let id = id_clone.clone();
        async move { get_media(&id).await }
    });

    rsx! {
        VideoPlayer { id: id.clone() }
        div { style: "margin-top: 1rem;",
            match &*media.read_unchecked() {
                Some(Ok(m)) if m.kind == "episode" => {
                    let back_to = m.show_id.clone().map(|sid| Route::ShowDetail { id: sid })
                        .unwrap_or(Route::Home {});
                    rsx! { Link { to: back_to, class: "btn ghost", "← Back" } }
                }
                _ => rsx! {
                    Link { to: Route::MediaDetail { id: id.clone() }, class: "btn ghost", "← Back" }
                }
            }
        }
    }
}

/// HTML5 video + subtitle track picker. Hands subtitle loading off to
/// `window.binkflixPlayer` (see assets/player.js): ASS goes through JASSUB,
/// VTT through a native `<track>`.
///
/// The state machine is small but deliberately reactive:
///   * `user_pick`     — user's explicit choice (`None` = untouched).
///   * `effective_id`  — memo: user's pick if any, else the "default"/first
///                       track from the list. PartialEq-deduped.
///   * `sub_command`   — memo: the concrete call to make into JS
///                       (`Option<SubCommand>`). Deduped by value.
///   * the effect      — subscribes to `sub_command` only. Fires the eval
///                       exactly once per actual change, no flag bookkeeping.
#[component]
fn VideoPlayer(id: String) -> Element {
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

    // The track the player should currently be showing. Recomputed when
    // either the user touches the picker or the track list resolves.
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

    // What we actually want to tell the JS side to do. By deriving this as
    // a memo with PartialEq, the downstream effect only fires when the value
    // genuinely changes — independent of how many times Dioxus re-renders.
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

    // `loading` shows a spinner while a subtitle attach is in flight;
    // `sub_error` surfaces the exception message from JS/ffmpeg so the user
    // isn't left staring at a silent failure.
    let mut loading = use_signal(|| false);
    let mut sub_error = use_signal(|| None::<String>);

    // Belt-and-suspenders dedupe. Dioxus memos don't reliably suppress
    // downstream notification on PartialEq-equal values across all render
    // paths (hydration, HMR, Resource re-emission), so we explicitly
    // remember the last-applied command and bail out if unchanged.
    // `.peek()` reads without subscribing — writing here doesn't re-trigger us.
    let mut last_applied = use_signal(|| None::<Option<SubCommand>>);

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

    rsx! {
        div { class: "video-wrap",
            video {
                id: "{video_dom_id}",
                src: "{media_stream_url(&id)}",
                controls: true,
                autoplay: true,
                preload: "metadata",
            }
        }
        div { class: "player-controls",
            match &*tracks.read_unchecked() {
                None => rsx! { span { class: "muted", "Loading subtitles…" } },
                Some(Err(e)) => rsx! { span { class: "muted", "Subtitles unavailable: {e}" } },
                Some(Ok(list)) if list.is_empty() => rsx! {
                    span { class: "muted", "No subtitle tracks found" }
                },
                Some(Ok(list)) => {
                    let list = list.clone();
                    let current = effective_id.read().clone().unwrap_or_default();
                    let is_loading = *loading.read();
                    rsx! {
                        label { class: "muted", "Subtitles: " }
                        select {
                            disabled: is_loading,
                            onchange: move |evt| {
                                let v = evt.value();
                                // Any onchange is an explicit choice — including "Off".
                                user_pick.set(Some(if v.is_empty() { None } else { Some(v) }));
                            },
                            option {
                                value: "",
                                selected: current.is_empty(),
                                "Off"
                            }
                            for t in list.iter() {
                                option {
                                    key: "{t.id}",
                                    value: "{t.id}",
                                    selected: current == t.id,
                                    {subtitle_option_label(t)}
                                }
                            }
                        }
                        if is_loading {
                            span { class: "spinner", aria_label: "loading subtitles" }
                            span { class: "muted", "Loading subtitles…" }
                        }
                        if let Some(msg) = sub_error.read().clone() {
                            span { class: "sub-error", title: "{msg}", "⚠ {msg}" }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SubFormat { Ass, Vtt }

/// Concrete instruction for the JS player layer. `PartialEq` drives memo
/// dedupe: the apply-effect only re-fires when a field actually changes.
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
