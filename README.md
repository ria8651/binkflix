# binkflix

A self-hosted media server in the spirit of Jellyfin/Plex — from scratch, Rust end to end. One axum backend, one Dioxus WASM client, built and served together by the Dioxus fullstack toolchain. Single binary for prod, one `dx serve` command for dev.

Metadata comes from Kodi-style NFO sidecar files (whatever sonarr/radarr drops next to your media), so there's no scraping or TMDB integration to manage.

## What works today

- **Library scanning** — walks one or more roots, classifies each video as movie or episode by looking at its NFO root element (`<movie>` / `<tvshow>` / `<episodedetails>`), falls back to ancestor `tvshow.nfo` or `SxxEyy` filenames.
- **Incremental rescans** — mtime + file_size check per row, so restarts after the first scan are near-instant.
- **Shows grouped by show → season** — seasons collapse automatically, with season posters when present.
- **HTTP range streaming** — proper 206 responses; any browser-native `<video>` client (or VLC) works.
- **Posters / fanart / episode thumbs** with lazy-loaded `<img>` so the home page doesn't nuke your NIC.
- **SyncPlay hub scaffolded** — WebSocket room at `/api/syncplay/:room` for play/pause/seek/heartbeat fan-out. Client UI not wired yet.
- **Single-origin dev** — `dx serve` runs client + backend together on one port with HMR.

## Not yet

- No auth. The server is wide open — don't expose it to the internet yet.
- No on-the-fly transcoding. Direct-play only; the client must natively support the codec/container.
- No client-side SyncPlay UI (the server hub is there, you'd have to build your own client).
- Episodes without both an NFO and an `SxxEyy` filename (e.g. `One Pace` batch files) are skipped — see the warn logs.
- No filesystem watcher — a scan runs at startup; you restart the server (or it HMR-restarts) to pick up new files.

## Stack

| Concern            | Choice                                              |
|--------------------|-----------------------------------------------------|
| Framework          | Dioxus 0.7 fullstack (one crate, two targets)       |
| HTTP               | axum 0.8 + tower-http                               |
| Database           | SQLite via sqlx (WAL, auto-migrate)                 |
| Metadata           | Kodi NFO files parsed with quick-xml                |
| Streaming          | `tower_http::services::ServeFile` (range support)   |
| Real-time          | axum WebSocket + `tokio::sync::broadcast`           |

## Requirements

- Rust (stable) with the `wasm32-unknown-unknown` target:
  ```sh
  rustup target add wasm32-unknown-unknown
  ```
- [Dioxus CLI](https://dioxuslabs.com/learn/0.7/guide/installation):
  ```sh
  cargo binstall dioxus-cli
  ```

## Running it

1. Copy the template and point it at your media:
   ```sh
   cp .env.example .env
   # edit BINKFLIX_LIBRARY — colon-separated paths, e.g.
   # BINKFLIX_LIBRARY=/Volumes/media/shows:/Volumes/media/movies
   ```

2. Start the dev server (client + backend + HMR, single port):
   ```sh
   dx serve --port 9356
   ```

3. Open http://localhost:9356

For a standalone backend (no UI rebuild) use `cargo run --features server`.

## Layout

```
src/
  main.rs            # feature-gated entry (web vs server)
  app.rs             # Dioxus routes + components (compiles on both targets)
  types.rs           # serde DTOs shared client/server
  client_api.rs      # gloo-net fetchers (wasm-only bodies)
  server/            # #[cfg(feature = "server")] — axum, DB, scanner, NFO, syncplay
migrations/0001_init.sql
assets/style.css
Dioxus.toml
.env.example
```

## Library layout it expects

Standard Kodi/Jellyfin/sonarr/radarr conventions:

```
shows/
  Show Name/
    tvshow.nfo
    poster.jpg
    fanart.jpg
    season01-poster.jpg
    Season 1/
      Show Name - S01E01.mkv
      Show Name - S01E01.nfo             # <episodedetails>
      Show Name - S01E01-thumb.jpg
movies/
  Movie Name (2020)/
    Movie Name (2020).mkv
    Movie Name (2020).nfo                # <movie>
    poster.jpg
    fanart.jpg
```

Point `BINKFLIX_LIBRARY` at one root or many (colon-separated). The scanner doesn't care whether a root is "shows" or "movies" — it decides per-file from the NFO.

## Config reference

See [.env.example](.env.example). Short version:

- `BINKFLIX_LIBRARY` — one or more library roots, colon-separated. Required.
- `BINKFLIX_DB` — SQLite path, default `./data/binkflix.db`.
- `BINKFLIX_BIND` — override the bind address. If unset, `dx serve` picks one.
- `RUST_LOG` — tracing filter, defaults to `info,binkflix=debug`.

## API (for now)

- `GET /api/library` — `{ movies: [...], shows: [...] }`
- `GET /api/media/:id` — movie or episode metadata (episode fields are null for movies)
- `GET /api/media/:id/stream` — range-GET enabled
- `GET /api/media/:id/image` — poster (movie) or thumb (episode)
- `GET /api/media/:id/fanart` — movies only
- `GET /api/shows/:id` — show + `seasons[] { number, episodes[] }`
- `GET /api/shows/:id/poster` | `/fanart` | `/seasons/:n/poster`
- `WS  /api/syncplay/:room` — play/pause/seek/heartbeat hub

## License

Personal project, no license yet. Don't ship it.
