# binkflix

A self-hosted media server in the spirit of Jellyfin/Plex — from scratch, Rust end to end. One axum backend, one Dioxus WASM client, built and served together by the Dioxus fullstack toolchain. Single binary for prod, one `dx serve` command for dev.

Metadata comes from Kodi-style NFO sidecar files (whatever sonarr/radarr drops next to your media), so there's no scraping or TMDB integration to manage.

## What works today

- **Library scanning** — walks one or more roots, classifies each video as movie or episode by looking at its NFO root element (`<movie>` / `<tvshow>` / `<episodedetails>`), falls back to ancestor `tvshow.nfo` or `SxxEyy` filenames.
- **Incremental rescans** — mtime + file_size check per row, so restarts after the first scan are near-instant.
- **On-demand rescan with live progress** — topbar refresh button triggers a scan without restarting; dropdown shows current phase (indexing / asset extraction), `done/total`, the file being worked on, and a summary of the previous scan.
- **Shows grouped by show → season** — seasons collapse automatically, with season posters when present.
- **HTTP range streaming** — proper 206 responses for direct-served files; any browser-native `<video>` client (or VLC) works.
- **HLS VOD for remux** — files whose video codec a browser can play but whose container or audio codec isn't natively supported get segmented with ffmpeg `-c:v copy -c:a aac` into keyframe-aligned fragmented MP4 chunks on disk (`./data/hls/{id}/`). The playlist is event-type so hls.js can start playing as soon as the first segment lands; subsequent plays of the same file skip ffmpeg entirely and serve the cached segments statically. Because the playlist declares real segment boundaries, the browser can seek anywhere — no more moov-patching hacks or VideoToolbox `BadDataErr` when scrubbing past the buffer. Safari uses native HLS; other browsers lazy-load hls.js from a CDN. ffprobe runs at scan time and the verdict is cached on the `media` row — no per-request probe. For codecs HLS can't cheaply carry (HEVC on Chrome/Firefox, VP9/AV1 in fMP4, etc.) the player shows a prompt offering a best-effort remux (HLS with `-c:v copy`, plays in Safari) or direct attempt (byte-range serve) rather than silently failing.
- **Custom video player** — fullscreen page, overlay controls that auto-hide after 2s of idle playback. Title bar at the top; scrubber shows played + buffered ranges; volume slider shows fill and persists to localStorage. Keyboard shortcuts: space/`k` play-pause, `←`/`→` seek ±5s, `↑`/`↓` volume, `m` mute, `f` fullscreen. Subtitle track picker integrated into the chrome (ASS via JASSUB, VTT via native `<track>`). Debug panel shows playback runtime stats, source codec info, and the actual delivery mode read from server response headers (`X-Stream-Mode`, `X-Stream-Video`, `X-Stream-Audio`).
- **Theming** — four themes (default-dark, classic-light, terminal, material) ported from the `boom` token system; switcher in the header and in the player overlay. Persists to localStorage.
- **Posters / fanart / episode thumbs** with lazy-loaded `<img>` so the home page doesn't nuke your NIC.
- **SyncPlay (watch parties)** — topbar Rooms dropdown lets anyone create/join a room from any page. Once joined, hitting play on any media broadcasts to the room, everyone else auto-navigates to the same media, and play/pause/seek stay in sync. Rooms are in-memory and evaporate when empty. No auth — clients are anonymous UUIDs.
- **Single-origin dev** — `dx serve` runs client + backend together on one port with HMR.

## Not yet

- No auth. The server is wide open — don't expose it to the internet yet.
- No real transcode path. Files whose video codec isn't browser-copyable (HEVC on non-Safari, MPEG-2, VC-1, …) fall back to a user-confirmed best-effort remux or direct attempt; a proper `-c:v libx264` path isn't wired up.
- No WebM/DASH pipeline. VP9 / VP8 / AV1 sources can't ride the HLS path (fMP4 can't carry them) — they fall through to direct byte-range serving of the original container, which works in browsers that can play MKV/WebM natively.
- First play of a given file waits for ffmpeg to write segment 0 before playback starts (a few seconds). Subsequent plays are instant.
- Episodes without both an NFO and an `SxxEyy` filename (e.g. `One Pace` batch files) are skipped — see the warn logs.
- No filesystem watcher — a scan runs at startup; you restart the server (or it HMR-restarts) to pick up new files.

## Stack

| Concern            | Choice                                              |
|--------------------|-----------------------------------------------------|
| Framework          | Dioxus 0.7 fullstack (one crate, two targets)       |
| HTTP               | axum 0.8 + tower-http                               |
| Database           | SQLite via sqlx (WAL, auto-migrate)                 |
| Metadata           | Kodi NFO files parsed with quick-xml                |
| Streaming          | `tower_http::services::ServeFile` for direct byte-range; ffmpeg HLS VOD (fMP4 segments on disk) for remux, with hls.js on the client |
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
- `ffmpeg` + `ffprobe` on `$PATH` — used for subtitle extraction, tech probing, and on-the-fly remux. Any recent build works.

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

## Docker

Build the image and run it, mounting your media read-only and a host dir for the SQLite DB:

```sh
docker build -t binkflix .

docker run -d --name binkflix \
  -p 9356:9356 \
  -v "$PWD/data:/data" \
  -v /path/to/shows:/media/shows:ro \
  -v /path/to/movies:/media/movies:ro \
  -e BINKFLIX_LIBRARY=/media/shows:/media/movies \
  binkflix
```

`BINKFLIX_DB` defaults to `/data/binkflix.db` and `BINKFLIX_BIND` to `0.0.0.0:9356` inside the image — override with `-e` if you need to.

## Layout

```
src/
  main.rs            # feature-gated entry (web vs server)
  app.rs             # Dioxus routes + layout + theme switcher
  video_player.rs    # custom overlay player, subtitle picker, transcode prompt
  types.rs           # serde DTOs + syncplay protocol shared client/server
  client_api.rs      # gloo-net fetchers (wasm-only bodies)
  syncplay_client.rs # RoomContext, WS task, topbar dropdown, video bridge
  server/            # #[cfg(feature = "server")] — axum, DB, scanner, NFO, syncplay
    remux.rs         # /stream dispatcher: direct byte-range vs fMP4 pipe fallback
    hls.rs           # /hls/... VOD pipeline: single-flight ffmpeg → cached segments
    media_info.rs    # ffprobe wrapper + BrowserCompat verdict + DB cache
migrations/0001_init.sql
assets/
  tokens.css         # design tokens + per-theme overrides
  style.css          # component styles, all via tokens
  static/player.js   # JASSUB + custom control wiring
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
- `BINKFLIX_HWACCEL` — H.264 hardware encoder for the HLS transcode path. `auto` (default), `none`, `vaapi`, `qsv`, or `videotoolbox`. `auto` probes `ffmpeg -encoders` plus the relevant device files at startup; falls back to libx264 if nothing usable is found, and again at runtime if a hwenc spawn fails. Pass `/dev/dri` into the container (`devices: ["/dev/dri:/dev/dri"]` in compose, `--device /dev/dri:/dev/dri` with raw docker) to enable VAAPI/QSV; without it the server runs as before with libx264.

## API (for now)

- `GET /api/library` — `{ movies: [...], shows: [...] }`
- `GET /api/media/:id` — movie or episode metadata (episode fields are null for movies)
- `GET /api/media/:id/stream` — byte-range direct serve for already-playable files (+ fallback fMP4 pipe for `?mode=remux|direct|transcode` overrides). Responses include `X-Stream-Mode` and (for remux) `X-Stream-Video` / `X-Stream-Audio` headers so the client knows what actually happened.
- `GET /api/media/:id/hls/index.m3u8` — event-type HLS VOD playlist. First call spawns ffmpeg to segment the source into `init.mp4` + `seg-NNNNN.m4s` under `./data/hls/{id}/`; subsequent calls serve the cached playlist once ffmpeg has written `.done`.
- `GET /api/media/:id/hls/{file}` — init / segment / playlist serves. Filenames are validated against the fixed set ffmpeg produces.
- `GET /api/media/:id/tech` — container, codecs, duration, and `browser_compat` verdict (direct / remux / transcode). Cached on the `media` row at scan time.
- `GET /api/media/:id/image` — poster (movie) or thumb (episode)
- `GET /api/media/:id/fanart` — movies only
- `GET /api/shows/:id` — show + `seasons[] { number, episodes[] }`
- `GET /api/shows/:id/poster` | `/fanart` | `/seasons/:n/poster`
- `GET /api/rooms` — active syncplay rooms with viewer counts
- `POST /api/rooms` — create an empty room, returns `{ id }`
- `WS  /api/syncplay/:room` — play/pause/seek/set_media/heartbeat hub
- `POST /api/scan` — trigger a rescan in the background (no-op if already running)
- `GET /api/scan/status` — live progress `{ running, phase, done, total, current, last_summary, last_elapsed_ms, … }`

## License

Personal project, no license yet. Don't ship it.
