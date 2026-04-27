use crate::client_api::*;
use crate::types::*;
use crate::video_player::VideoPlayer;
use dioxus::prelude::*;

const ICON_PALETTE: &str = r#"<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="13.5" cy="6.5" r=".5" fill="currentColor"/><circle cx="17.5" cy="10.5" r=".5" fill="currentColor"/><circle cx="8.5" cy="7.5" r=".5" fill="currentColor"/><circle cx="6.5" cy="12.5" r=".5" fill="currentColor"/><path d="M12 2a10 10 0 0 0 0 20c1.1 0 2-.9 2-2 0-.48-.18-.95-.55-1.28A1.5 1.5 0 0 1 14.5 16H16a6 6 0 0 0 6-6c0-4.42-4.48-8-10-8z"/></svg>"#;
const ICON_CHECK_SMALL: &str = r#"<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6L9 17l-5-5"/></svg>"#;
const ICON_PLAY_BTN: &str = r#"<svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor" aria-hidden="true"><path d="M8 5v14l11-7z"/></svg>"#;
const ICON_CHECK_BADGE: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6L9 17l-5-5"/></svg>"#;
pub const ICON_GROUP: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M23 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/></svg>"#;
const ICON_SEARCH: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="11" cy="11" r="7"/><path d="M21 21l-4.3-4.3"/></svg>"#;
const ICON_REFRESH: &str = r#"<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M23 4v6h-6"/><path d="M1 20v-6h6"/><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10"/><path d="M20.49 15A9 9 0 0 1 5.64 18.36L1 14"/></svg>"#;

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

const LOGO_SVG: &str = include_str!("../assets/binkflix.svg");

/// Which topbar dropdown is open. Shared so opening one closes the others.
#[derive(Clone, Copy, Default)]
pub struct OpenMenu(pub Signal<Option<&'static str>>);

/// Library search query, read by Home to filter shows/movies.
#[derive(Clone, Copy, Default)]
pub struct SearchQuery(pub Signal<String>);

#[component]
fn Shell() -> Element {
    crate::syncplay_client::provide_room_context();
    use_context_provider(|| OpenMenu(Signal::new(None)));
    use_context_provider(|| SearchQuery(Signal::new(String::new())));

    let mut open_menu = use_context::<OpenMenu>().0;

    // Close any open popover when the user clicks outside one. The old
    // `.menu-backdrop` div only worked in the Shell's stacking context;
    // on the video-player route the fullpage layer sits above it, so
    // clicks there never reached the backdrop. A document-level
    // pointerdown listener doesn't care about z-index at all — if the
    // click target isn't inside a registered dropdown wrapper, we close.
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
                    document.addEventListener('pointerdown', (e) => {
                        if (!e.target.closest('[data-popover]')) {
                            window.dispatchEvent(new CustomEvent('binkflix-close-popover'));
                        }
                    }, true);
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
            Link {
                to: Route::Home {},
                class: "brand",
                aria_label: "Binkflix home",
                dangerous_inner_html: LOGO_SVG,
            }
            div { class: "top-right",
                SearchDropdown {}
                RescanButton {}
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
fn SearchDropdown() -> Element {
    let mut open_menu = use_context::<OpenMenu>().0;
    let mut query = use_context::<SearchQuery>().0;
    let is_open = *open_menu.read() == Some("search");
    let current = query.read().clone();

    // Clear the query whenever the popover is not open. Covers every close
    // path (toolbar button toggle, outside-click from OpenMenu, navigation)
    // without threading a clear call through each of them.
    use_effect(move || {
        if *open_menu.read() != Some("search") && !query.read().is_empty() {
            query.set(String::new());
        }
    });

    rsx! {
        div { class: "search-dd", "data-popover": "search",
            button {
                class: "btn-theme btn-icon",
                r#type: "button",
                aria_label: "Search",
                onclick: move |_| {
                    open_menu.set(if is_open { None } else { Some("search") });
                },
                span { class: "icon", dangerous_inner_html: ICON_SEARCH }
            }
            if is_open {
                div { class: "search-panel",
                    input {
                        id: "search-input",
                        r#type: "search",
                        placeholder: "Search shows and movies…",
                        value: "{current}",
                        // Dioxus' `autofocus` attribute only works on initial page
                        // load; this panel mounts dynamically, so explicitly
                        // focus via JS once the input is in the DOM.
                        onmounted: move |_| {
                            spawn(async move {
                                let _ = document::eval(
                                    "document.getElementById('search-input')?.focus();",
                                ).await;
                            });
                        },
                        oninput: move |e| query.set(e.value()),
                    }
                    if !current.is_empty() {
                        button {
                            class: "search-clear",
                            r#type: "button",
                            aria_label: "Clear search",
                            onclick: move |_| {
                                query.set(String::new());
                                spawn(async move {
                                    let _ = document::eval(
                                        "document.getElementById('search-input')?.focus();",
                                    ).await;
                                });
                            },
                            "×"
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
                        if let Some(cur) = s.current.as_deref() {
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

#[component]
fn Home() -> Element {
    let lib = use_resource(get_library);
    let mut cont = use_resource(get_continue_watching);
    let query = use_context::<SearchQuery>().0;
    let q = query.read().to_lowercase();
    let q = q.trim().to_string();

    // Continue Watching is hidden while a search is active — the row is
    // already short and this avoids confusion when the user is filtering
    // the library proper.
    let cont_items: Vec<ContinueItem> = if q.is_empty() {
        match &*cont.read_unchecked() {
            Some(Ok(items)) => items.clone(),
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };

    rsx! {
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
                let shows: Vec<_> = lib.shows.iter().cloned()
                    .filter(|s| q.is_empty() || s.title.to_lowercase().contains(&q))
                    .collect();
                let movies: Vec<_> = lib.movies.iter().cloned()
                    .filter(|m| q.is_empty() || m.title.to_lowercase().contains(&q))
                    .collect();
                rsx! {
                    if shows.is_empty() && movies.is_empty() {
                        p { class: "empty", "No matches for “{q}”." }
                    }
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
                    if !shows.is_empty() {
                        section {
                            h2 { class: "section", "Shows" }
                            div { class: "grid",
                                for s in shows {
                                    ShowCard { key: "{s.id}", show: s }
                                }
                            }
                        }
                    }
                    if !movies.is_empty() {
                        section {
                            h2 { class: "section", "Movies" }
                            div { class: "grid",
                                for m in movies {
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
fn ContinueCard(item: ContinueItem, on_change: EventHandler<()>) -> Element {
    // Episodes render as landscape cards with the 16:9 sidecar still that
    // ships next to the .mkv (or our generated thumbnail fallback).
    // Movies keep the 2:3 poster.
    let is_episode = item.show_id.is_some();
    let route = Route::MediaPlay { id: item.media_id.clone() };
    let card_class = if is_episode { "card card-wide" } else { "card" };
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
    rsx! {
        article { class: "{card_class}",
            Link { to: route, class: "card-link",
                div { class: "poster-wrap",
                    img {
                        class: "poster",
                        src: "{media_image_url(&item.media_id)}",
                        loading: "lazy",
                        decoding: "async",
                        alt: "{item.title}",
                    }
                    if pct > 0.0 {
                        div { class: "progress",
                            div { class: "progress-bar", style: "width: {pct}%;" }
                        }
                    }
                }
                h3 { class: "title", "{item.title}" }
            }
            button {
                class: "mark-watched",
                title: "Mark as watched",
                "aria-label": "Mark as watched",
                onclick: on_mark,
                dangerous_inner_html: ICON_CHECK_BADGE,
            }
        }
    }
}

#[component]
fn MovieCard(movie: MovieSummary) -> Element {
    let year = movie.year.map(|y| y.to_string()).unwrap_or_default();

    rsx! {
        article { class: "card",
            Link { to: Route::MediaDetail { id: movie.id.clone() }, class: "card-link",
                img {
                    class: "poster",
                    src: "{media_image_url(&movie.id)}",
                    loading: "lazy",
                    decoding: "async",
                    alt: "{movie.title}",
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
                img {
                    class: "poster",
                    src: "{show_poster_url(&show.id)}",
                    loading: "lazy",
                    decoding: "async",
                    alt: "{show.title}",
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

#[component]
fn MediaDetail(id: String) -> Element {
    let id_clone = id.clone();
    let media = use_resource(move || {
        let id = id_clone.clone();
        async move { get_media(&id).await }
    });

    rsx! {
        match &*media.read_unchecked() {
            None => rsx! { p { class: "empty", "Loading…" } },
            Some(Err(e)) => rsx! { p { class: "empty", "Failed to load: {e}" } },
            Some(Ok(m)) => rsx! {
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
                        p { class: "meta-row",
                            if m.kind == "episode" {
                                if let (Some(s), Some(e)) = (m.season_number, m.episode_number) {
                                    "S{s:02}E{e:02}"
                                }
                            } else if let Some(y) = m.year {
                                "{y}"
                            }
                            if let Some(r) = m.runtime_minutes {
                                " · {r} min"
                            }
                        }
                        if let Some(plot) = m.plot.as_deref() {
                            p { class: "plot", "{plot}" }
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
                    article { class: "detail",
                        div {
                            class: "poster",
                            style: "background-image: url('{show_poster_url(&d.show.id)}')",
                        }
                        header { class: "detail-info",
                            h1 { "{d.show.title}" }
                            if let Some(y) = d.show.year {
                                p { class: "meta-row", "{y}" }
                            }
                            if let Some(plot) = d.show.plot.as_deref() {
                                p { class: "plot", "{plot}" }
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
                        SeasonBlock { key: "{season.number}", season }
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
fn SeasonBlock(season: Season) -> Element {
    rsx! {
        section { class: "season",
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
    rsx! {
        article { class: "episode",
            Link { to: Route::MediaPlay { id: episode.id.clone() }, class: "episode-link",
                img {
                    class: "ep-thumb",
                    src: "{media_image_url(&episode.id)}",
                    loading: "lazy",
                    decoding: "async",
                    alt: "",
                }
                div { class: "ep-body",
                    h3 { class: "ep-title",
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
            VideoPlayer { id: id.clone(), back_route: back_route.clone() }
            crate::syncplay_client::SyncplayBridge {
                video_dom_id: "binkflix-video".to_string(),
                media_id: id.clone(),
            }
        }
    }
}

