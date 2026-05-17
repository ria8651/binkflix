use crate::client_api::*;
use crate::types::*;
use crate::video_player::VideoPlayer;
use dioxus::prelude::*;

const ICON_PALETTE: &str = r#"<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="13.5" cy="6.5" r=".5" fill="currentColor"/><circle cx="17.5" cy="10.5" r=".5" fill="currentColor"/><circle cx="8.5" cy="7.5" r=".5" fill="currentColor"/><circle cx="6.5" cy="12.5" r=".5" fill="currentColor"/><path d="M12 2a10 10 0 0 0 0 20c1.1 0 2-.9 2-2 0-.48-.18-.95-.55-1.28A1.5 1.5 0 0 1 14.5 16H16a6 6 0 0 0 6-6c0-4.42-4.48-8-10-8z"/></svg>"#;
const ICON_CHECK_SMALL: &str = r#"<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6L9 17l-5-5"/></svg>"#;
const ICON_PLAY_BTN: &str = r#"<svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor" aria-hidden="true"><path d="M8 5v14l11-7z"/></svg>"#;
const ICON_CHECK_BADGE: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6L9 17l-5-5"/></svg>"#;
const ICON_X_BADGE: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M18 6L6 18"/><path d="M6 6l12 12"/></svg>"#;
pub const ICON_GROUP: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M23 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/></svg>"#;
const ICON_SEARCH: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="11" cy="11" r="7"/><path d="M21 21l-4.3-4.3"/></svg>"#;
const ICON_REFRESH: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M23 4v6h-6"/><path d="M1 20v-6h6"/><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10"/><path d="M20.49 15A9 9 0 0 1 5.64 18.36L1 14"/></svg>"#;

#[derive(Routable, Clone, PartialEq)]
#[rustfmt::skip]
pub enum Route {
    #[layout(Shell)]
        #[route("/")]
        Home {},
        #[route("/search")]
        Search {},
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
        document::Link {
            rel: "icon",
            r#type: "image/svg+xml",
            href: asset!("/assets/logo-icon.svg"),
        }
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

const LOGO_SVG: &str = include_str!("../assets/binkflix.svg");

/// Which topbar dropdown is open. Shared so opening one closes the others.
#[derive(Clone, Copy, Default)]
pub struct OpenMenu(pub Signal<Option<&'static str>>);

#[component]
fn Shell() -> Element {
    crate::syncplay_client::provide_room_context();
    use_context_provider(|| OpenMenu(Signal::new(None)));

    let mut open_menu = use_context::<OpenMenu>().0;

    // Close any open popover when the user clicks outside one. The old
    // `.menu-backdrop` div only worked in the Shell's stacking context;
    // on the video-player route the fullpage layer sits above it, so
    // clicks there never reached the backdrop. A document-level click
    // listener doesn't care about z-index at all — if the click target
    // isn't inside a registered dropdown wrapper, we close.
    //
    // We listen on `click` in bubble phase rather than `pointerdown` in
    // capture phase so that link/button click handlers fire *first*. With
    // `pointerdown`, the close-and-clear-query side effect would re-render
    // the Home grid and unmount any filtered show card before the click
    // event ever reached its target — clicking a search result would
    // close the search but never navigate.
    //
    // The listener is installed by JS (easier to reach document events
    // from there) and dispatches a custom window event back to Rust;
    // the Rust side then flips the signal. The JS half is installed
    // once and stays; it's a no-op while no menu is open because the
    // Rust side ignores the event unless `menu_open` is true.
    use_effect(move || {
        let currently_open = *open_menu.read();
        if currently_open.is_none() {
            return;
        }
        spawn(async move {
            let mut eval = document::eval(
                r#"
                if (!window.__binkflixOutsideClickInstalled) {
                    document.addEventListener('click', (e) => {
                        if (!e.target.closest('[data-popover]')) {
                            window.dispatchEvent(new CustomEvent('binkflix-close-popover'));
                        }
                    });
                    window.__binkflixOutsideClickInstalled = true;
                }
                // Resolve this eval on the next outside-click so the Rust
                // side can close the menu. Each `use_effect` run gets its
                // own one-shot listener, removed after fire.
                await new Promise((res) => {
                    const once = () => {
                        window.removeEventListener('binkflix-close-popover', once);
                        res();
                    };
                    window.addEventListener('binkflix-close-popover', once);
                });
                dioxus.send(true);
                "#,
            );
            if eval.recv::<serde_json::Value>().await.is_ok() {
                open_menu.set(None);
            }
        });
    });

    rsx! {
        header { class: "topbar",
            // Five-layer progressive blur stack. Each div applies a
            // `backdrop-filter: blur(N)` and is masked into a band, so
            // the blur ramps from light at the bottom of the bar to
            // heavy at the top. Each layer compounds the blur of the
            // layer beneath it. Decorative — themes that don't want
            // the effect leave the blur tokens as `none`.
            div { class: "topbar-blur topbar-blur-1", aria_hidden: "true" }
            div { class: "topbar-blur topbar-blur-2", aria_hidden: "true" }
            div { class: "topbar-blur topbar-blur-3", aria_hidden: "true" }
            div { class: "topbar-blur topbar-blur-4", aria_hidden: "true" }
            div { class: "topbar-blur topbar-blur-5", aria_hidden: "true" }
            Link {
                to: Route::Home {},
                class: "brand",
                aria_label: "Binkflix home",
                dangerous_inner_html: LOGO_SVG,
            }
            div { class: "top-right",
                Link {
                    to: Route::Search {},
                    class: "btn-theme btn-icon",
                    aria_label: "Search",
                    span { class: "icon", dangerous_inner_html: ICON_SEARCH }
                }
                RescanButton {}
                crate::syncplay_client::RoomsDropdown {}
                ThemeSwitcher {}
            }
        }
        main {
            crate::syncplay_client::RoomNavigator {}
            Outlet::<Route> {}
        }
        crate::syncplay_client::RoomToasts {}
    }
}

const THEMES: &[(&str, &str)] = &[
    ("default", "Default (dark)"),
    ("classic-light", "Classic light"),
    ("terminal", "Terminal"),
    ("material", "Material"),
    ("elegantfin", "ElegantFin"),
];

#[component]
pub fn ThemeSwitcher() -> Element {
    let mut theme = use_signal(|| "default".to_string());
    let mut open_menu = use_context::<OpenMenu>().0;
    let is_open = *open_menu.read() == Some("theme");

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

    let current_id = theme.read().clone();

    rsx! {
        div { class: "theme-switcher", "data-popover": "theme",
            button {
                class: "btn-theme btn-icon",
                r#type: "button",
                aria_label: "Theme",
                onclick: move |_| {
                    open_menu.set(if is_open { None } else { Some("theme") });
                },
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
                                        open_menu.set(None);
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
fn RescanButton() -> Element {
    let mut status = use_signal(ScanProgress::default);
    let mut open_menu = use_context::<OpenMenu>().0;
    let is_open = *open_menu.read() == Some("rescan");

    // Fetch once on mount, then only poll while a scan is running OR while
    // the menu is open. Previously this polled forever every 3s on every
    // page (including the player), which was wasteful bandwidth and
    // cluttered the network tab. External scans started in another tab
    // won't light up the topbar until the user opens the menu — acceptable
    // tradeoff for a single-user media app.
    use_future(move || async move {
        #[cfg(feature = "web")]
        {
            if let Ok(s) = get_scan_status().await {
                status.set(s);
            }
            loop {
                let should_poll = status.peek().running || *open_menu.read() == Some("rescan");
                if !should_poll {
                    gloo_timers::future::TimeoutFuture::new(1500).await;
                    continue;
                }
                let ms = if status.peek().running { 500 } else { 2000 };
                gloo_timers::future::TimeoutFuture::new(ms).await;
                if let Ok(s) = get_scan_status().await {
                    status.set(s);
                }
            }
        }
    });

    let s = status.read().clone();
    let running = s.running;
    let pct: Option<u32> = if s.total > 0 {
        Some(((s.done * 100) / s.total).min(100) as u32)
    } else {
        None
    };
    let label = if running {
        if s.total > 0 {
            format!("Scanning {}/{}", s.done, s.total)
        } else {
            format!("Scanning — {}", s.phase)
        }
    } else {
        "Rescan".to_string()
    };

    rsx! {
        div { class: "rescan", "data-popover": "rescan",
            button {
                class: if running { "btn-theme btn-icon scanning" } else { "btn-theme btn-icon" },
                r#type: "button",
                aria_label: "Rescan library",
                title: "{label}",
                onclick: move |_| {
                    let was_open = is_open;
                    open_menu.set(if was_open { None } else { Some("rescan") });
                    if !running && !was_open {
                        // Optimistic local state so the user sees feedback
                        // even when the scan finishes before the next poll.
                        let mut w = status.write();
                        w.running = true;
                        w.phase = "starting".into();
                        w.done = 0;
                        w.total = 0;
                        w.current = None;
                        w.message = None;
                        drop(w);
                        spawn(async move {
                            let _ = start_scan().await;
                            if let Ok(s) = get_scan_status().await {
                                status.set(s);
                            }
                        });
                    }
                },
                span { class: "icon", dangerous_inner_html: ICON_REFRESH }
            }
            if is_open {
                div { class: "menu rescan-menu",
                    div { class: "rescan-row",
                        strong {
                            if running {
                                if s.phase == "indexing" { "Indexing library…" }
                                else if s.phase == "subtitles" { "Extracting subtitles…" }
                                else if s.phase == "thumbnails" { "Extracting thumbnails…" }
                                else if s.phase == "trickplay" { "Building trickplay…" }
                                else if s.phase == "saving" { "Saving metadata…" }
                                // Pre-0.9 builds and the brief moment between
                                // phase 1 → phase 2 still report "assets".
                                else if s.phase == "assets" { "Extracting assets…" }
                                else { "Scanning…" }
                            } else {
                                "Library scan"
                            }
                        }
                    }
                    if running {
                        div { class: "rescan-row muted",
                            if s.total > 0 {
                                "{s.done} / {s.total}"
                            } else if s.done > 0 {
                                "Discovering — {s.done} files found"
                            } else {
                                "Starting…"
                            }
                        }
                        div { class: "rescan-progress",
                            div {
                                class: "rescan-progress-bar",
                                class: if s.total == 0 { "indeterminate" } else { "" },
                                style: match pct {
                                    Some(p) => format!("width: {p}%"),
                                    None => String::new(),
                                },
                            }
                        }
                        // Phase 2 (asset extraction) runs up to N files in
                        // parallel, so list each one with its current stage.
                        // Phase 1 (indexing) is sequential — fall back to the
                        // single `current` filename row.
                        if !s.active.is_empty() {
                            div { class: "rescan-active",
                                for j in s.active.iter() {
                                    div { class: "rescan-active-row",
                                        span { class: "rescan-active-title", "{j.title}" }
                                        span { class: "rescan-active-stage", "{j.stage}" }
                                    }
                                }
                            }
                        } else if let Some(cur) = s.current.as_deref() {
                            div { class: "rescan-row muted rescan-current", "{cur}" }
                        }
                    } else {
                        if let Some(summary) = s.last_summary.as_deref() {
                            div { class: "rescan-row muted", "Last scan: {summary}" }
                            if let Some(ms) = s.last_elapsed_ms {
                                div { class: "rescan-row muted rescan-time", "Took {format_elapsed(ms)}" }
                            }
                        } else {
                            div { class: "rescan-row muted", "No scan run yet this session." }
                        }
                        if let Some(msg) = s.message.as_deref() {
                            div { class: "rescan-row rescan-error", "{msg}" }
                        }
                        button {
                            class: "rescan-start",
                            r#type: "button",
                            onclick: move |_| {
                                let mut w = status.write();
                                w.running = true;
                                w.phase = "starting".into();
                                w.done = 0;
                                w.total = 0;
                                w.current = None;
                                w.message = None;
                                drop(w);
                                spawn(async move {
                                    let _ = start_scan().await;
                                    if let Ok(s) = get_scan_status().await {
                                        status.set(s);
                                    }
                                });
                            },
                            "Start rescan"
                        }
                    }
                }
            }
        }
    }
}

fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// `"2019-01-04"` → `"Jan 4, 2019"`. Returns the input unchanged if it
/// doesn't parse as YYYY-MM-DD (defensive — NFOs occasionally carry an
/// already-formatted string or just a year).
fn format_long_date(raw: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = raw.splitn(3, '-');
    let (y, m, d) = match (parts.next(), parts.next(), parts.next()) {
        (Some(y), Some(m), Some(d)) => (y, m, d),
        _ => return raw.to_string(),
    };
    let m_idx: usize = match m.parse() {
        Ok(n) if (1..=12).contains(&n) => n,
        _ => return raw.to_string(),
    };
    let day: u32 = match d.parse() {
        Ok(n) => n,
        _ => return raw.to_string(),
    };
    if y.parse::<i32>().is_err() {
        return raw.to_string();
    }
    format!("{} {}, {}", MONTHS[m_idx - 1], day, y)
}

/// Pull the four-digit year out of a YYYY-MM-DD prefix. None if the
/// first four bytes don't parse.
fn year_from_date(raw: &str) -> Option<i32> {
    raw.get(..4).and_then(|s| s.parse().ok())
}

fn format_votes(n: i64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        let k = n as f64 / 1000.0;
        if k >= 10.0 { format!("{k:.0}k") } else { format!("{k:.1}k") }
    } else {
        let m = n as f64 / 1_000_000.0;
        format!("{m:.1}M")
    }
}

fn format_bytes(n: i64) -> String {
    if n < 0 {
        return "0 B".into();
    }
    let n = n as f64;
    if n < 1024.0 {
        return format!("{n:.0} B");
    }
    let k = n / 1024.0;
    if k < 1024.0 {
        return format!("{k:.1} KB");
    }
    let mb = k / 1024.0;
    if mb < 1024.0 {
        return format!("{mb:.1} MB");
    }
    let gb = mb / 1024.0;
    format!("{gb:.2} GB")
}

fn imdb_url(id: &str) -> String {
    format!("https://www.imdb.com/title/{id}/")
}
fn tmdb_movie_url(id: &str) -> String {
    format!("https://www.themoviedb.org/movie/{id}")
}
fn tmdb_tv_url(id: &str) -> String {
    format!("https://www.themoviedb.org/tv/{id}")
}
fn tvdb_url(id: &str) -> String {
    format!("https://thetvdb.com/?id={id}&tab=series")
}

#[component]
fn Home() -> Element {
    let lib = use_resource(get_library);
    let mut cont = use_resource(get_continue_watching);

    let cont_items: Vec<ContinueItem> = match &*cont.read_unchecked() {
        Some(Ok(items)) => items.clone(),
        _ => Vec::new(),
    };

    rsx! {
        document::Title { "Binkflix" }
        match &*lib.read_unchecked() {
            None => rsx! { p { class: "empty", "Loading…" } },
            Some(Err(e)) => rsx! { p { class: "empty", "Failed to load: {e}" } },
            Some(Ok(lib)) if lib.movies.is_empty() && lib.shows.is_empty() => rsx! {
                div { class: "empty",
                    p { "Your library is empty." }
                    p { class: "muted", "Add movies and/or shows with .nfo metadata, then restart the server." }
                }
            },
            Some(Ok(lib)) => {
                rsx! {
                    if !cont_items.is_empty() {
                        section {
                            h2 { class: "section", "Continue Watching" }
                            div { class: "grid grid-wide",
                                for c in cont_items.iter().cloned() {
                                    ContinueCard {
                                        key: "{c.media_id}",
                                        item: c,
                                        on_change: move |_| cont.restart(),
                                    }
                                }
                            }
                        }
                    }
                    if !lib.recently_added.is_empty() {
                        section {
                            h2 { class: "section", "Recently Added" }
                            div { class: "grid grid-wide",
                                for item in lib.recently_added.iter().cloned() {
                                    RecentCard { key: "{item.media_id}", item }
                                }
                            }
                        }
                    }
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
}

#[component]
fn Search() -> Element {
    let mut query = use_signal(String::new);
    let mut kind = use_signal(|| String::new()); // "" any, "movie", "show"
    let mut year_min = use_signal::<Option<i64>>(|| None);
    let mut year_max = use_signal::<Option<i64>>(|| None);
    let mut selected_genres = use_signal::<Vec<String>>(Vec::new);
    let mut watched = use_signal(|| "any".to_string());
    let mut sort = use_signal(|| "title".to_string());
    let mut show_filters = use_signal(|| false);

    // Debounce tick — bump after each input change; the resource only
    // re-fetches once it has been stable for the timeout.
    let mut tick = use_signal(|| 0u64);

    let genres_resource = use_resource(get_genres);

    let results = use_resource(move || {
        let q = query.read().clone();
        let k = kind.read().clone();
        let ymin = *year_min.read();
        let ymax = *year_max.read();
        let g = selected_genres.read().clone();
        let w = watched.read().clone();
        let s = sort.read().clone();
        let _ = *tick.read();
        async move {
            // 250ms debounce: any further input bumps `tick`, restarting
            // this future before the timeout fires.
            #[cfg(feature = "web")]
            gloo_timers::future::TimeoutFuture::new(250).await;
            let sq = SearchQuery {
                q,
                kind: k,
                year_min: ymin,
                year_max: ymax,
                genres: g,
                watched: w,
                sort: s,
                limit: Some(120),
            };
            search_library(&sq).await
        }
    });

    let active_filter_count = {
        let mut n = 0;
        if !kind.read().is_empty() { n += 1; }
        if year_min.read().is_some() { n += 1; }
        if year_max.read().is_some() { n += 1; }
        if !selected_genres.read().is_empty() { n += 1; }
        if watched.read().as_str() != "any" { n += 1; }
        if sort.read().as_str() != "title" { n += 1; }
        n
    };

    let q_for_empty = query.read().clone();
    let no_input = q_for_empty.trim().is_empty() && active_filter_count == 0;

    rsx! {
        document::Title { "Search — Binkflix" }
        section { class: "search-page",
            header { class: "search-header",
                div { class: "search-input-wrap",
                    span { class: "search-input-icon", dangerous_inner_html: ICON_SEARCH }
                    input {
                        class: "search-input",
                        id: "search-page-input",
                        r#type: "search",
                        placeholder: "Search shows and movies…",
                        autofocus: "true",
                        value: "{query.read()}",
                        oninput: move |e| {
                            query.set(e.value());
                            tick.with_mut(|t| *t = t.wrapping_add(1));
                        },
                    }
                    if !query.read().is_empty() {
                        button {
                            class: "search-clear",
                            r#type: "button",
                            aria_label: "Clear search",
                            onclick: move |_| {
                                query.set(String::new());
                                tick.with_mut(|t| *t = t.wrapping_add(1));
                            },
                            "×"
                        }
                    }
                }
                button {
                    class: if *show_filters.read() { "filter-toggle is-open" } else { "filter-toggle" },
                    r#type: "button",
                    aria_expanded: if *show_filters.read() { "true" } else { "false" },
                    onclick: move |_| show_filters.toggle(),
                    "Filters"
                    if active_filter_count > 0 {
                        span { class: "filter-count", "{active_filter_count}" }
                    }
                }
            }

            if *show_filters.read() {
                div { class: "search-filters",
                    div { class: "filter-group",
                        label { class: "filter-label", "Type" }
                        div { class: "segmented",
                            {
                                let opts = [("", "All"), ("movie", "Movies"), ("show", "Shows")];
                                rsx! {
                                    for (val, label) in opts {
                                        {
                                            let v = val.to_string();
                                            let active = *kind.read() == v;
                                            rsx! {
                                                button {
                                                    key: "{val}",
                                                    class: if active { "seg active" } else { "seg" },
                                                    r#type: "button",
                                                    onclick: move |_| { kind.set(v.clone()); tick.with_mut(|t| *t = t.wrapping_add(1)); },
                                                    "{label}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div { class: "filter-group",
                        label { class: "filter-label", "Year" }
                        div { class: "year-range",
                            input {
                                class: "year-input",
                                r#type: "number",
                                placeholder: "From",
                                value: (*year_min.read()).map(|y| y.to_string()).unwrap_or_default(),
                                oninput: move |e| {
                                    let v = e.value();
                                    year_min.set(v.trim().parse::<i64>().ok());
                                    tick.with_mut(|t| *t = t.wrapping_add(1));
                                },
                            }
                            span { class: "year-sep", "–" }
                            input {
                                class: "year-input",
                                r#type: "number",
                                placeholder: "To",
                                value: (*year_max.read()).map(|y| y.to_string()).unwrap_or_default(),
                                oninput: move |e| {
                                    let v = e.value();
                                    year_max.set(v.trim().parse::<i64>().ok());
                                    tick.with_mut(|t| *t = t.wrapping_add(1));
                                },
                            }
                        }
                    }

                    div { class: "filter-group",
                        label { class: "filter-label", "Watched" }
                        div { class: "segmented",
                            {
                                let opts = [
                                    ("any", "Any"),
                                    ("unwatched", "Unwatched"),
                                    ("in_progress", "In progress"),
                                    ("watched", "Watched"),
                                ];
                                rsx! {
                                    for (val, label) in opts {
                                        {
                                            let v = val.to_string();
                                            let active = *watched.read() == v;
                                            rsx! {
                                                button {
                                                    key: "{val}",
                                                    class: if active { "seg active" } else { "seg" },
                                                    r#type: "button",
                                                    onclick: move |_| { watched.set(v.clone()); tick.with_mut(|t| *t = t.wrapping_add(1)); },
                                                    "{label}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div { class: "filter-group",
                        label { class: "filter-label", "Sort" }
                        select {
                            class: "sort-select",
                            value: "{sort.read()}",
                            onchange: move |e| { sort.set(e.value()); tick.with_mut(|t| *t = t.wrapping_add(1)); },
                            option { value: "title", "Title (A–Z)" }
                            option { value: "year_desc", "Year (newest)" }
                            option { value: "year_asc", "Year (oldest)" }
                            option { value: "recently_added", "Recently added" }
                        }
                    }

                    {
                        let genres = match &*genres_resource.read_unchecked() {
                            Some(Ok(g)) => g.clone(),
                            _ => Vec::new(),
                        };
                        (!genres.is_empty()).then(|| rsx! {
                            div { class: "filter-group filter-group-wide",
                                label { class: "filter-label", "Genre" }
                                div { class: "chip-row",
                                    for g in genres {
                                        {
                                            let value = g.clone();
                                            let label = g.clone();
                                            let active = selected_genres.read().iter().any(|x| x == &value);
                                            rsx! {
                                                button {
                                                    key: "{value}",
                                                    class: if active { "filter-chip is-active" } else { "filter-chip" },
                                                    r#type: "button",
                                                    onclick: move |_| {
                                                        selected_genres.with_mut(|gs| {
                                                            if let Some(idx) = gs.iter().position(|x| x == &value) {
                                                                gs.remove(idx);
                                                            } else {
                                                                gs.push(value.clone());
                                                            }
                                                        });
                                                        tick.with_mut(|t| *t = t.wrapping_add(1));
                                                    },
                                                    "{label}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        })
                    }

                    div { class: "filter-actions",
                        button {
                            class: "btn ghost",
                            r#type: "button",
                            onclick: move |_| {
                                kind.set(String::new());
                                year_min.set(None);
                                year_max.set(None);
                                selected_genres.set(Vec::new());
                                watched.set("any".into());
                                sort.set("title".into());
                                tick.with_mut(|t| *t = t.wrapping_add(1));
                            },
                            "Clear filters"
                        }
                    }
                }
            }

            div { class: "search-results",
                match &*results.read_unchecked() {
                    None => rsx! { p { class: "empty muted", "Searching…" } },
                    Some(Err(e)) => rsx! { p { class: "empty", "Search failed: {e}" } },
                    Some(Ok(r)) if r.movies.is_empty() && r.shows.is_empty() => {
                        if no_input {
                            rsx! { p { class: "empty muted", "Start typing or open Filters to browse the library." } }
                        } else {
                            rsx! { p { class: "empty", "No matches." } }
                        }
                    }
                    Some(Ok(r)) => rsx! {
                        if !r.shows.is_empty() {
                            section {
                                h2 { class: "section", "Shows ({r.total_shows})" }
                                div { class: "grid",
                                    for s in r.shows.iter().cloned() {
                                        ShowCard { key: "{s.id}", show: s }
                                    }
                                }
                            }
                        }
                        if !r.movies.is_empty() {
                            section {
                                h2 { class: "section", "Movies ({r.total_movies})" }
                                div { class: "grid",
                                    for m in r.movies.iter().cloned() {
                                        MovieCard { key: "{m.id}", movie: m }
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

/// How a card tile's image should be rendered. The aspect ratio is
/// applied by CSS based on the surrounding `.card` / `.card-wide` class,
/// not by the shape itself — this enum just chooses the markup (plain
/// img vs. a letterbox div that puts the foreground image over a blurred
/// copy so empty bars never show as solid black).
#[derive(Clone, Copy, PartialEq)]
enum PosterShape {
    Plain,
    Letterbox,
}

/// Image element used inside `.poster-wrap` across every card type
/// (`MovieCard`, `ShowCard`, `ContinueCard`, `RecentCard`). The caller
/// supplies the URL appropriate for what's being shown (portrait poster,
/// show poster, episode fanart, movie fanart with poster fallback) and a
/// shape; the surrounding CSS handles aspect ratio and rounding.
#[component]
fn Poster(src: String, alt: String, shape: PosterShape) -> Element {
    match shape {
        PosterShape::Plain => rsx! {
            img {
                class: "poster",
                src: "{src}",
                loading: "lazy",
                decoding: "async",
                alt: "{alt}",
            }
        },
        PosterShape::Letterbox => rsx! {
            // The same URL feeds both layers; when fanart exists the
            // foreground covers its (identical) blurred backdrop, and when
            // only the portrait poster is available the foreground sits
            // letterboxed inside a blurred copy of itself.
            div { class: "poster poster-letterbox",
                img {
                    class: "poster-bg",
                    src: "{src}",
                    loading: "lazy",
                    decoding: "async",
                    aria_hidden: "true",
                }
                img {
                    class: "poster-fg",
                    src: "{src}",
                    loading: "lazy",
                    decoding: "async",
                    alt: "{alt}",
                }
            }
        }
    }
}

#[component]
fn ContinueCard(
    item: ContinueItem,
    on_change: EventHandler<()>,
    /// When true (used on the show-detail page), episode tiles show
    /// the per-episode thumbnail instead of the show fanart so the
    /// "play next" affordance is visually specific to that episode.
    #[props(default = false)]
    use_episode_thumb: bool,
) -> Element {
    // Every tile is a 16:9 landscape card so the row reads uniformly. Episode
    // stills come from the show fanart (with server-side fallback to the
    // episode still); movie posters get letterboxed onto a blurred backdrop.
    let is_episode = item.show_id.is_some();
    let shape = if is_episode { PosterShape::Plain } else { PosterShape::Letterbox };
    let route = Route::MediaPlay { id: item.media_id.clone() };
    let ep_se = if is_episode {
        let s = item.season_number.unwrap_or(0);
        let e = item.episode_number.unwrap_or(0);
        Some(format!("S{s}E{e}"))
    } else {
        None
    };
    let show_link = item
        .show_id
        .clone()
        .zip(item.show_title.clone())
        .filter(|(_, t)| !t.is_empty());
    let year = if !is_episode {
        item.year.map(|y| y.to_string())
    } else {
        None
    };
    let pct = if item.duration_secs > 0.0 {
        (item.position_secs / item.duration_secs * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let media_id_for_mark = item.media_id.clone();
    let on_mark = move |evt: Event<MouseData>| {
        // The button sits outside the Link so a plain click already won't
        // navigate; stop_propagation is just defense in depth.
        evt.stop_propagation();
        let id = media_id_for_mark.clone();
        spawn(async move {
            let _ = mark_watched(&id).await;
            on_change.call(());
        });
    };
    let media_id_for_dismiss = item.media_id.clone();
    let on_dismiss = move |evt: Event<MouseData>| {
        evt.stop_propagation();
        let id = media_id_for_dismiss.clone();
        spawn(async move {
            let _ = dismiss_continue_watching(&id).await;
            on_change.call(());
        });
    };
    let poster_src = if is_episode && use_episode_thumb {
        media_image_url(&item.media_id)
    } else {
        media_fanart_url(&item.media_id)
    };
    rsx! {
        article { class: "card card-wide",
            Link { to: route, class: "card-link",
                div { class: "poster-wrap",
                    Poster {
                        src: poster_src,
                        alt: item.title.clone(),
                        shape,
                    }
                    div { class: "play-overlay",
                        span { class: "icon-btn overlay lg", dangerous_inner_html: ICON_PLAY_BTN }
                    }
                    if pct > 0.0 {
                        div { class: "progress",
                            div { class: "progress-bar", style: "width: {pct}%;" }
                        }
                    }
                }
                h3 { class: "title", "{item.title}" }
                SubtitleLine { show_link, ep_se, year }
            }
            button {
                class: "icon-btn overlay mark-watched",
                title: "Mark as watched",
                "aria-label": "Mark as watched",
                onclick: on_mark,
                dangerous_inner_html: ICON_CHECK_BADGE,
            }
            button {
                class: "icon-btn overlay dismiss-cw",
                title: "Remove from Continue Watching",
                "aria-label": "Remove from Continue Watching",
                onclick: on_dismiss,
                dangerous_inner_html: ICON_X_BADGE,
            }
        }
    }
}

#[component]
fn RecentCard(item: RecentItem) -> Element {
    // Episodes: 16:9 still + "Show · S1E2" subtitle.
    // Movies: 16:9 fanart (or letterboxed poster) + year subtitle.
    let is_episode = item.show_id.is_some();
    let shape = if is_episode { PosterShape::Plain } else { PosterShape::Letterbox };
    let route = Route::MediaPlay { id: item.media_id.clone() };
    let ep_se = if is_episode {
        let s = item.season_number.unwrap_or(0);
        let e = item.episode_number.unwrap_or(0);
        Some(format!("S{s}E{e}"))
    } else {
        None
    };
    let show_link = item
        .show_id
        .clone()
        .zip(item.show_title.clone())
        .filter(|(_, t)| !t.is_empty());
    let year = if !is_episode {
        item.year.map(|y| y.to_string())
    } else {
        None
    };
    rsx! {
        article { class: "card card-wide",
            Link { to: route, class: "card-link",
                div { class: "poster-wrap",
                    Poster {
                        src: media_fanart_url(&item.media_id),
                        alt: item.title.clone(),
                        shape,
                    }
                    div { class: "play-overlay",
                        span { class: "icon-btn overlay lg", dangerous_inner_html: ICON_PLAY_BTN }
                    }
                }
                h3 { class: "title", "{item.title}" }
                SubtitleLine { show_link, ep_se, year }
            }
        }
    }
}

/// Card subtitle for `ContinueCard` / `RecentCard`.
///
/// The show name (when present) is a clickable span that navigates to
/// `ShowDetail` while suppressing the outer card link. Using a span instead
/// of a nested `Link` avoids invalid nested `<a>` tags.
#[component]
fn SubtitleLine(
    show_link: Option<(String, String)>,
    ep_se: Option<String>,
    year: Option<String>,
) -> Element {
    let nav = use_navigator();
    match (show_link, ep_se, year) {
        (Some((sid, title)), Some(se), _) => rsx! {
            p { class: "year",
                span {
                    class: "show-link",
                    onclick: move |evt: Event<MouseData>| {
                        evt.stop_propagation();
                        evt.prevent_default();
                        nav.push(Route::ShowDetail { id: sid.clone() });
                    },
                    "{title}"
                }
                " · {se}"
            }
        },
        (None, Some(se), _) => rsx! { p { class: "year", "{se}" } },
        (_, _, Some(y)) if !y.is_empty() => rsx! { p { class: "year", "{y}" } },
        _ => rsx! {},
    }
}

#[component]
fn MovieCard(movie: MovieSummary) -> Element {
    let year = movie.year.map(|y| y.to_string()).unwrap_or_default();

    rsx! {
        article { class: "card",
            Link { to: Route::MediaDetail { id: movie.id.clone() }, class: "card-link",
                div { class: "poster-wrap",
                    Poster {
                        src: media_image_url(&movie.id),
                        alt: movie.title.clone(),
                        shape: PosterShape::Plain,
                    }
                    div { class: "play-overlay",
                        span { class: "icon-btn overlay lg", dangerous_inner_html: ICON_PLAY_BTN }
                    }
                }
                h3 { class: "title", "{movie.title}" }
                p { class: "year", "{year}" }
            }
        }
    }
}

#[component]
fn ShowCard(show: ShowSummary) -> Element {
    let year = show.year.map(|y| y.to_string()).unwrap_or_default();
    let count = show.episode_count;

    rsx! {
        article { class: "card",
            Link { to: Route::ShowDetail { id: show.id.clone() }, class: "card-link",
                div { class: "poster-wrap",
                    Poster {
                        src: show_poster_url(&show.id),
                        alt: show.title.clone(),
                        shape: PosterShape::Plain,
                    }
                    div { class: "play-overlay",
                        span { class: "icon-btn overlay lg", dangerous_inner_html: ICON_PLAY_BTN }
                    }
                }
                h3 { class: "title", "{show.title}" }
                p { class: "year",
                    "{year}"
                    if !year.is_empty() { " · " }
                    "{count} eps"
                }
            }
        }
    }
}

/// Fanart hero behind a detail page: the fixed fullscreen image, the
/// purely-horizontal side-wash for text contrast, and the vh-anchored
/// mask that fades the whole hero into `--bg-page` as the user scrolls.
/// Order matters: the mask renders last so it covers the fade (same
/// z-index) when scrolled past the hero.
#[component]
fn DetailBackdrop(fanart_url: String) -> Element {
    rsx! {
        div {
            class: "show-backdrop",
            style: "background-image: url('{fanart_url}')",
        }
        div { class: "detail-fade" }
        div { class: "show-backdrop-mask" }
    }
}

#[component]
fn MediaDetail(id: String) -> Element {
    // `use_reactive` makes `id` a tracked dependency so navigating between
    // two movies (Dioxus reuses the route component when the path matches)
    // restarts the fetch with the new id rather than serving the one
    // captured at first render.
    let media = use_resource(use_reactive!(|id| async move {
        get_media(&id).await
    }));

    rsx! {
        match &*media.read_unchecked() {
            None => rsx! { p { class: "empty", "Loading…" } },
            Some(Err(e)) => rsx! { p { class: "empty", "Failed to load: {e}" } },
            Some(Ok(m)) => rsx! {
                document::Title { "{m.title} — Binkflix" }
                if m.has_fanart {
                    DetailBackdrop { fanart_url: media_fanart_url(&m.id) }
                }
                article { class: "detail",
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
                    header { class: "detail-info",
                        h1 { "{m.title}" }
                        {
                            // Original title only if it differs from the displayed
                            // title (NFOs sometimes echo the same string into both).
                            let alt = m.original_title.as_deref().filter(|s| *s != m.title);
                            alt.map(|s| rsx! { p { class: "original-title", "{s}" } })
                        }
                        if let Some(tagline) = m.tagline.as_deref() {
                            p { class: "tagline", "{tagline}" }
                        }
                        {
                            // Flow numeric/date facts as a single text line; chips
                            // for MPAA / status sit in their own row below so
                            // categorical labels don't get lost in the run-on.
                            let mut bits: Vec<String> = Vec::new();
                            if m.kind == "episode" {
                                if let (Some(s), Some(e)) = (m.season_number, m.episode_number) {
                                    bits.push(format!("S{s:02}E{e:02}"));
                                }
                                if let Some(d) = m.release_date.as_deref() {
                                    bits.push(format_long_date(d));
                                }
                            } else if let Some(d) = m.release_date.as_deref() {
                                bits.push(format_long_date(d));
                            } else if let Some(y) = m.year {
                                bits.push(format!("{y}"));
                            }
                            if let Some(r) = m.runtime_minutes {
                                bits.push(format!("{r} min"));
                            }
                            let meta_text = bits.join(" · ");
                            let rating = m.rating;
                            let votes = m.rating_votes.map(format_votes);
                            let has_meta_text = !meta_text.is_empty();
                            (has_meta_text || rating.is_some()).then(|| rsx! {
                                p { class: "meta-row",
                                    if has_meta_text { "{meta_text}" }
                                    if let Some(rv) = rating {
                                        if has_meta_text { " · " }
                                        span { class: "rating-star", "★" }
                                        " {rv:.1}"
                                        if let Some(v) = votes.as_deref() {
                                            span { class: "rating-votes", " ({v})" }
                                        }
                                    }
                                }
                            })
                        }
                        {
                            let has_mpaa = m.mpaa.is_some();
                            has_mpaa.then(|| rsx! {
                                div { class: "meta-chips",
                                    if let Some(mpaa) = m.mpaa.as_deref() {
                                        span { class: "meta-chip mpaa", "{mpaa}" }
                                    }
                                }
                            })
                        }
                        if let Some(studio) = m.studio.as_deref() {
                            p { class: "studio-line", "{studio}" }
                        }
                        if let Some(plot) = m.plot.as_deref() {
                            p { class: "plot", "{plot}" }
                        }
                        {
                            // Movie-only credits line. Episodes don't carry
                            // director/writers in our NFOs, and the column is
                            // NULL there.
                            let dir = m.director.as_deref();
                            let wri = m.writers.as_deref();
                            (dir.is_some() || wri.is_some()).then(|| rsx! {
                                p { class: "credits-line",
                                    if let Some(d) = dir {
                                        "Directed by {d}"
                                    }
                                    if dir.is_some() && wri.is_some() { " · " }
                                    if let Some(w) = wri {
                                        "Written by {w}"
                                    }
                                }
                            })
                        }
                        if !m.genres.is_empty() {
                            div { class: "genre-chips",
                                for g in m.genres.iter() {
                                    span { key: "{g}", class: "meta-chip genre", "{g}" }
                                }
                            }
                        }
                        {
                            // External link footer. IMDb/TMDb only on movies and
                            // episodes; episodes inherit the show via show_id but
                            // not the show's IDs, so links only render when the
                            // media row itself has them.
                            let imdb = m.imdb_id.as_deref();
                            let tmdb = m.tmdb_id.as_deref();
                            (imdb.is_some() || tmdb.is_some()).then(|| rsx! {
                                p { class: "ext-links",
                                    if let Some(id) = imdb {
                                        a { href: imdb_url(id), target: "_blank", rel: "noopener", "IMDb" }
                                    }
                                    if imdb.is_some() && tmdb.is_some() { " · " }
                                    if let Some(id) = tmdb {
                                        {
                                            let url = if m.kind == "episode" {
                                                tmdb_tv_url(id)
                                            } else {
                                                tmdb_movie_url(id)
                                            };
                                            rsx! { a { href: "{url}", target: "_blank", rel: "noopener", "TMDb" } }
                                        }
                                    }
                                }
                            })
                        }
                        nav { class: "detail-actions",
                            Link { to: Route::MediaPlay { id: m.id.clone() }, class: "btn",
                                span { dangerous_inner_html: ICON_PLAY_BTN }
                                "Play"
                            }
                            if m.kind == "episode" {
                                if let Some(sid) = m.show_id.as_deref() {
                                    Link { to: Route::ShowDetail { id: sid.to_string() }, class: "btn ghost", "Show" }
                                }
                            }
                        }
                        p { class: "file-footnote", "{format_bytes(m.file_size)}" }
                    }
                }
            }
        }
    }
}

#[component]
fn ShowDetail(id: String) -> Element {
    // `use_reactive` makes `id` a tracked dependency, so navigating from
    // one show to another (Dioxus reuses the route component when the
    // path matches) actually restarts the fetch with the new id rather
    // than reusing the stale one captured at first render.
    let mut detail = use_resource(use_reactive!(|id| async move {
        get_show(&id).await
    }));

    // Show-scoped Continue Watching. The server returns the full global
    // list; we filter client-side by `show_id` since the list is small
    // and filtering here avoids a server route just for this view.
    let mut cont = use_resource(get_continue_watching);

    // Library is fetched so we can pick a sibling show's banner for the
    // footer flourish. Cheap because the home page warms this cache.
    let lib = use_resource(get_library);

    // Currently selected season number. `None` until the resource resolves
    // and a default is chosen; once set, `selected` drives which season's
    // episode list renders. Tabs flip the value.
    let mut selected = use_signal::<Option<i64>>(|| None);

    rsx! {
        match &*detail.read_unchecked() {
            None => rsx! { p { class: "empty", "Loading…" } },
            Some(Err(e)) => rsx! { p { class: "empty", "Failed to load: {e}" } },
            Some(Ok(d)) => {
                // Default to the lowest non-Specials season; fall back to
                // Specials if that's all we have. We do this lazily here so
                // the selection follows the resource without an extra effect.
                let current = match *selected.read() {
                    Some(n) if d.seasons.iter().any(|s| s.number == n) => n,
                    _ => default_season(&d.seasons),
                };
                let active = d.seasons.iter().find(|s| s.number == current).cloned();
                rsx! {
                    document::Title { "{d.show.title} — Binkflix" }
                    if d.show.has_fanart {
                        DetailBackdrop { fanart_url: show_fanart_url(&d.show.id) }
                    }
                    article { class: "detail",
                        div {
                            class: "poster",
                            style: "background-image: url('{show_poster_url(&d.show.id)}')",
                        }
                        // header rendered after the poster so default themes
                        // (which auto-place into the grid in source order)
                        // keep poster-left / info-right. Elegantfin places
                        // each child explicitly via `--show-detail-*-col`.
                        header { class: "detail-info",
                            if d.show.has_clearlogo {
                                img {
                                    class: "show-clearlogo",
                                    src: "{show_clearlogo_url(&d.show.id)}",
                                    alt: "{d.show.title}",
                                    loading: "eager",
                                    decoding: "async",
                                }
                            } else {
                                h1 { "{d.show.title}" }
                            }
                            {
                                let alt = d.show.original_title.as_deref().filter(|s| *s != d.show.title);
                                alt.map(|s| rsx! { p { class: "original-title", "{s}" } })
                            }
                            {
                                // Year (or year-range) + rating in a single flowing
                                // line, matching MediaDetail. End-year is preferred
                                // from `end_date`; fall back to bare `year` when the
                                // show is ongoing or pre-Tier-B data.
                                let start_year = d
                                    .show
                                    .premiered_date
                                    .as_deref()
                                    .and_then(year_from_date)
                                    .map(|y| y as i64)
                                    .or(d.show.year);
                                let end_year = d
                                    .show
                                    .end_date
                                    .as_deref()
                                    .and_then(year_from_date);
                                let year_text = match (start_year, end_year) {
                                    (Some(s), Some(e)) if (e as i64) > s => format!("{s} – {e}"),
                                    (Some(s), _) => format!("{s}"),
                                    _ => String::new(),
                                };
                                let has_year = !year_text.is_empty();
                                let rating = d.show.rating;
                                let votes = d.show.rating_votes.map(format_votes);
                                (has_year || rating.is_some()).then(|| rsx! {
                                    p { class: "meta-row",
                                        if has_year { "{year_text}" }
                                        if let Some(rv) = rating {
                                            if has_year { " · " }
                                            span { class: "rating-star", "★" }
                                            " {rv:.1}"
                                            if let Some(v) = votes.as_deref() {
                                                span { class: "rating-votes", " ({v})" }
                                            }
                                        }
                                    }
                                })
                            }
                            {
                                let has_mpaa = d.show.mpaa.is_some();
                                let has_status = d.show.status.is_some();
                                (has_mpaa || has_status).then(|| rsx! {
                                    div { class: "meta-chips",
                                        if let Some(mpaa) = d.show.mpaa.as_deref() {
                                            span { class: "meta-chip mpaa", "{mpaa}" }
                                        }
                                        if let Some(status) = d.show.status.as_deref() {
                                            span { class: "meta-chip status", "{status}" }
                                        }
                                    }
                                })
                            }
                            if let Some(studio) = d.show.studio.as_deref() {
                                p { class: "studio-line", "{studio}" }
                            }
                            if let Some(plot) = d.show.plot.as_deref() {
                                p { class: "plot", "{plot}" }
                            }
                            if !d.show.genres.is_empty() {
                                div { class: "genre-chips",
                                    for g in d.show.genres.iter() {
                                        span { key: "{g}", class: "meta-chip genre", "{g}" }
                                    }
                                }
                            }
                            {
                                let imdb = d.show.imdb_id.as_deref();
                                let tmdb = d.show.tmdb_id.as_deref();
                                let tvdb = d.show.tvdb_id.as_deref();
                                (imdb.is_some() || tmdb.is_some() || tvdb.is_some()).then(|| rsx! {
                                    p { class: "ext-links",
                                        if let Some(id) = imdb {
                                            a { href: imdb_url(id), target: "_blank", rel: "noopener", "IMDb" }
                                        }
                                        if imdb.is_some() && (tmdb.is_some() || tvdb.is_some()) { " · " }
                                        if let Some(id) = tmdb {
                                            a { href: tmdb_tv_url(id), target: "_blank", rel: "noopener", "TMDb" }
                                        }
                                        if tmdb.is_some() && tvdb.is_some() { " · " }
                                        if let Some(id) = tvdb {
                                            a { href: tvdb_url(id), target: "_blank", rel: "noopener", "TVDB" }
                                        }
                                    }
                                })
                            }
                            {
                                // The single most-recent in-progress
                                // episode for this show. Lives inside
                                // the info column so the "play next"
                                // affordance sits with the title/plot
                                // rather than as a separate row.
                                let show_id = d.show.id.clone();
                                let next = match &*cont.read_unchecked() {
                                    Some(Ok(items)) => items
                                        .iter()
                                        .find(|c| c.show_id.as_deref() == Some(show_id.as_str()))
                                        .cloned(),
                                    _ => None,
                                };
                                next.map(|c| rsx! {
                                    div { class: "detail-continue",
                                        h2 { class: "section", "Continue Watching" }
                                        ContinueCard {
                                            key: "{c.media_id}",
                                            item: c,
                                            use_episode_thumb: true,
                                            on_change: move |_| {
                                                cont.restart();
                                                detail.restart();
                                            },
                                        }
                                    }
                                })
                            }
                        }
                    }
                    nav { class: "season-picker",
                        for season in d.seasons.iter() {
                            {
                                let n = season.number;
                                let label = season_label(n);
                                let is_active = n == current;
                                let cls = if is_active { "season-pick active" } else { "season-pick" };
                                let poster = format!(
                                    "background-image: url('{}'), url('{}')",
                                    season_poster_url(&d.show.id, n),
                                    show_poster_url(&d.show.id),
                                );
                                rsx! {
                                    button {
                                        key: "{n}",
                                        class: "{cls}",
                                        "aria-label": "{label}",
                                        title: "{label}",
                                        onclick: move |_| selected.set(Some(n)),
                                        div { class: "season-pick-poster", style: "{poster}" }
                                        span { class: "season-pick-label", "{label}" }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(season) = active {
                        SeasonBlock {
                            key: "{season.number}",
                            season,
                            on_change: move |_| detail.restart(),
                        }
                    }
                    {
                        // Footer flourish: a responsive grid of sibling-show
                        // banners at the bottom of the page. Picks are
                        // deterministic per show (byte-sum seed, walking the
                        // candidate list) so the same companion set pairs
                        // with the same show across visits. We render up to
                        // BANNER_COUNT and let `auto-fill` lay out one tidy
                        // row at the current viewport width.
                        const BANNER_COUNT: usize = 5;
                        let picks: Vec<String> = match &*lib.read_unchecked() {
                            Some(Ok(library)) => {
                                let here = &d.show.id;
                                let candidates: Vec<&ShowSummary> = library
                                    .shows
                                    .iter()
                                    .filter(|s| s.has_banner && s.id != *here)
                                    .collect();
                                if candidates.is_empty() {
                                    Vec::new()
                                } else {
                                    let seed: usize = here.bytes().map(|b| b as usize).sum();
                                    let take = BANNER_COUNT.min(candidates.len());
                                    (0..take)
                                        .map(|i| candidates[(seed + i) % candidates.len()].id.clone())
                                        .collect()
                                }
                            }
                            _ => Vec::new(),
                        };
                        (!picks.is_empty()).then(|| rsx! {
                            div { class: "detail-footer-banners",
                                for sid in picks {
                                    Link {
                                        key: "{sid}",
                                        to: Route::ShowDetail { id: sid.clone() },
                                        class: "detail-footer-banner",
                                        img {
                                            src: "{show_banner_url(&sid)}",
                                            loading: "lazy",
                                            decoding: "async",
                                            alt: "",
                                        }
                                    }
                                }
                            }
                        })
                    }
                }
            }
        }
    }
}

fn season_label(n: i64) -> String {
    if n == 0 {
        "Specials".to_string()
    } else {
        format!("Season {n}")
    }
}

/// Pick the season we open the page on: lowest non-Specials season; falls
/// back to Specials if the show is *only* specials, and to 1 for an empty
/// list (defensive — the UI guards against rendering in that case).
fn default_season(seasons: &[Season]) -> i64 {
    seasons
        .iter()
        .map(|s| s.number)
        .filter(|n| *n != 0)
        .min()
        .or_else(|| seasons.first().map(|s| s.number))
        .unwrap_or(1)
}

#[component]
fn SeasonBlock(season: Season, on_change: EventHandler<()>) -> Element {
    rsx! {
        section { class: "season",
            div { class: "episode-list",
                for ep in season.episodes.iter().cloned() {
                    EpisodeRow {
                        key: "{ep.id}",
                        episode: ep,
                        on_change: move |_| on_change.call(()),
                    }
                }
            }
        }
    }
}

#[component]
fn EpisodeRow(episode: EpisodeSummary, on_change: EventHandler<()>) -> Element {
    let completed = episode.completed != 0;
    let pct = if completed {
        100.0
    } else if episode.duration_secs > 0.0 {
        (episode.position_secs / episode.duration_secs * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let media_id_for_mark = episode.id.clone();
    let on_mark = move |evt: Event<MouseData>| {
        // The button sits inside the row's <Link>, so both stop_propagation
        // and prevent_default are needed to keep the click from navigating
        // to the player.
        evt.stop_propagation();
        evt.prevent_default();
        let id = media_id_for_mark.clone();
        spawn(async move {
            let _ = mark_watched(&id).await;
            on_change.call(());
        });
    };
    rsx! {
        article { class: "episode",
            Link { to: Route::MediaPlay { id: episode.id.clone() }, class: "episode-link",
                div { class: "ep-thumb-wrap",
                    img {
                        class: "ep-thumb",
                        src: "{media_image_url(&episode.id)}",
                        loading: "lazy",
                        decoding: "async",
                        alt: "",
                    }
                    div { class: "play-overlay",
                        span { class: "icon-btn overlay lg", dangerous_inner_html: ICON_PLAY_BTN }
                    }
                    if pct > 0.0 {
                        div { class: "progress",
                            div { class: "progress-bar", style: "width: {pct}%;" }
                        }
                    }
                    if !completed {
                        button {
                            class: "icon-btn overlay mark-watched",
                            title: "Mark as watched",
                            "aria-label": "Mark as watched",
                            onclick: on_mark,
                            dangerous_inner_html: ICON_CHECK_BADGE,
                        }
                    }
                }
                div { class: "ep-body",
                    h3 { class: "ep-title",
                        span { class: "ep-num", "S{episode.season_number:02}E{episode.episode_number:02}" }
                        " · "
                        "{episode.title}"
                    }
                    {
                        let mut bits: Vec<String> = Vec::new();
                        if let Some(r) = episode.runtime_minutes {
                            bits.push(format!("{r} min"));
                        }
                        if let Some(d) = episode.release_date.as_deref() {
                            bits.push(format_long_date(d));
                        }
                        (!bits.is_empty()).then(|| {
                            let text = bits.join(" · ");
                            rsx! { p { class: "ep-meta", "{text}" } }
                        })
                    }
                    if let Some(plot) = episode.plot.as_deref() {
                        p { class: "ep-plot", "{plot}" }
                    }
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

    let page_title = match &*media.read_unchecked() {
        Some(Ok(m)) => format!("{} — Binkflix", m.title),
        _ => "Binkflix".to_string(),
    };

    // The `for` + keyed wrapper `div` exists to force a full unmount +
    // remount of `VideoPlayer` and `SyncplayBridge` whenever `id` changes
    // (prev/next-episode soft-nav, or remote SetMedia from a watch party).
    // Dioxus 0.7's `key:` is only honoured inside a list context — on a
    // lone child it's silently ignored — so we have to render the wrapper
    // through a one-element iterator to put it in a list. With remount
    // semantics, every id-derived piece of state inside the player
    // (subtitle URLs, tech probe, stream src, `<video>` element, listener
    // handles, last_applied caches) is automatically fresh per episode,
    // and we don't have to add per-effect "did the id change?" gating.
    // `display: contents` on the wrapper keeps it from affecting layout.
    rsx! {
        document::Title { "{page_title}" }
        div { class: "player-fullpage",
            for episode_id in [id.clone()] {
                {
                    let back = back_route.clone();
                    rsx! {
                        div {
                            key: "{episode_id}",
                            class: "player-keyed",
                            VideoPlayer {
                                id: episode_id.clone(),
                                back_route: back,
                            }
                            crate::syncplay_client::SyncplayBridge {
                                video_dom_id: "binkflix-video".to_string(),
                                media_id: episode_id.clone(),
                            }
                        }
                    }
                }
            }
        }
    }
}

