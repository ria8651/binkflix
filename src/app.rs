use crate::client_api::*;
use crate::types::*;
use crate::video_player::VideoPlayer;
use dioxus::prelude::*;

const ICON_PALETTE: &str = r#"<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="13.5" cy="6.5" r=".5" fill="currentColor"/><circle cx="17.5" cy="10.5" r=".5" fill="currentColor"/><circle cx="8.5" cy="7.5" r=".5" fill="currentColor"/><circle cx="6.5" cy="12.5" r=".5" fill="currentColor"/><path d="M12 2a10 10 0 0 0 0 20c1.1 0 2-.9 2-2 0-.48-.18-.95-.55-1.28A1.5 1.5 0 0 1 14.5 16H16a6 6 0 0 0 6-6c0-4.42-4.48-8-10-8z"/></svg>"#;
const ICON_CARET: &str = r#"<svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 9l6 6 6-6"/></svg>"#;
const ICON_CHECK_SMALL: &str = r#"<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6L9 17l-5-5"/></svg>"#;
const ICON_BACK: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M19 12H5M12 19l-7-7 7-7"/></svg>"#;
const ICON_PLAY_BTN: &str = r#"<svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor" aria-hidden="true"><path d="M8 5v14l11-7z"/></svg>"#;
pub const ICON_GROUP: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M23 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/></svg>"#;

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
        document::Stylesheet { href: asset!("/assets/tokens.css") }
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
    crate::syncplay_client::provide_room_context();

    rsx! {
        header { class: "topbar",
            Link { to: Route::Home {}, class: "brand", "BINKFLIX" }
            div { class: "top-right",
                crate::syncplay_client::RoomsDropdown {}
                ThemeSwitcher {}
            }
        }
        main {
            crate::syncplay_client::RoomNavigator {}
            Outlet::<Route> {}
        }
    }
}

const THEMES: &[(&str, &str)] = &[
    ("default", "Default (dark)"),
    ("classic-light", "Classic light"),
    ("terminal", "Terminal"),
    ("material", "Material"),
];

#[component]
pub fn ThemeSwitcher() -> Element {
    let mut theme = use_signal(|| "default".to_string());
    let mut open = use_signal(|| false);

    // On mount: restore from localStorage, then apply current theme.
    use_effect(move || {
        spawn(async move {
            let mut eval = document::eval(
                r#"
                const saved = localStorage.getItem('binkflix-theme') || 'default';
                document.documentElement.dataset.theme = saved;
                dioxus.send(saved);
                "#,
            );
            if let Ok(val) = eval.recv::<serde_json::Value>().await {
                if let Some(s) = val.as_str() {
                    theme.set(s.to_string());
                }
            }
        });
    });

    // Whenever the theme signal changes, apply + persist.
    use_effect(move || {
        let t = theme.read().clone();
        let js = format!(
            r#"
            document.documentElement.dataset.theme = '{t}';
            localStorage.setItem('binkflix-theme', '{t}');
            "#
        );
        spawn(async move { let _ = document::eval(&js).await; });
    });

    let is_open = *open.read();
    let current_id = theme.read().clone();

    rsx! {
        div { class: "theme-switcher",
            button {
                class: "btn-theme btn-icon",
                r#type: "button",
                aria_label: "Theme",
                onclick: move |_| { let cur = *open.peek(); open.set(!cur); },
                span { class: "icon", dangerous_inner_html: ICON_PALETTE }
            }
            if is_open {
                div { class: "menu",
                    for (id, label) in THEMES.iter() {
                        {
                            let tid = id.to_string();
                            let active = current_id == *id;
                            rsx! {
                                button {
                                    key: "{id}",
                                    class: if active { "active" } else { "" },
                                    r#type: "button",
                                    onclick: move |_| {
                                        theme.set(tid.clone());
                                        open.set(false);
                                    },
                                    span { "{label}" }
                                    if active {
                                        span { class: "check", dangerous_inner_html: ICON_CHECK_SMALL }
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
                            Link { to: Route::MediaPlay { id: m.id.clone() }, class: "btn",
                                span { dangerous_inner_html: ICON_PLAY_BTN }
                                span { "Play" }
                            }
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

    let back_route = match &*media.read_unchecked() {
        Some(Ok(m)) if m.kind == "episode" => m
            .show_id
            .clone()
            .map(|sid| Route::ShowDetail { id: sid })
            .unwrap_or(Route::Home {}),
        _ => Route::MediaDetail { id: id.clone() },
    };

    rsx! {
        div { class: "player-fullpage",
            Link { to: back_route, class: "player-back",
                span { dangerous_inner_html: ICON_BACK }
                span { "Back" }
            }
            div { class: "player-theme",
                crate::syncplay_client::RoomsDropdown {}
                ThemeSwitcher {}
            }
            VideoPlayer { id: id.clone() }
            crate::syncplay_client::SyncplayBridge {
                video_dom_id: "binkflix-video".to_string(),
                media_id: id.clone(),
            }
        }
    }
}

