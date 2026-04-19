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
        div { class: "video-wrap",
            video {
                src: "{media_stream_url(&id)}",
                controls: true,
                autoplay: true,
                preload: "metadata",
            }
        }
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
