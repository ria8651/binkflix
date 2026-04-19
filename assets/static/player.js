// Subtitle overlay for the binkflix player.
//
// Exposes a tiny imperative API on `window.binkflixPlayer` that the Dioxus
// component drives via `dioxus::document::eval`. All state lives here so we
// can tear down the JASSUB worker / native tracks cleanly between track
// changes.
//
// - ASS/SSA → rendered with JASSUB (libass compiled to WASM).
// - VTT     → native <track kind="subtitles"> on the <video> element.
//
// JASSUB is pulled from a CDN; the version is pinned to keep the worker/WASM
// URLs matching the module build.

// JASSUB is vendored under /jassub/ — served same-origin so the Web Worker
// and WASM can load (cross-origin workers are blocked by the browser).
const JASSUB_MODULE = "/jassub/jassub.es.js";
const JASSUB_WORKER = "/jassub/jassub-worker.js";
const JASSUB_WASM = "/jassub/jassub-worker.wasm";

let jassubModulePromise = null;
function loadJassub() {
    if (!jassubModulePromise) {
        jassubModulePromise = import(JASSUB_MODULE).then((m) => m.default || m.JASSUB || m);
    }
    return jassubModulePromise;
}

// Renderer + native-track references keyed by video element id.
const state = new Map();
// Per-video serialization: chains attach/detach calls so a fast burst of
// effect runs can't stack multiple renderers on the same element.
const locks = new Map();

function run(videoId, task) {
    const prev = locks.get(videoId) || Promise.resolve();
    const next = prev.catch(() => {}).then(task);
    locks.set(videoId, next);
    return next;
}

function getVideo(videoId) {
    const el = document.getElementById(videoId);
    if (!el || el.tagName !== "VIDEO") return null;
    return el;
}

function clearNativeTracks(video) {
    // Remove every <track> we added; don't touch tracks embedded in the media.
    for (const node of Array.from(video.querySelectorAll("track[data-binkflix]"))) {
        const url = node.dataset.blobUrl;
        if (url) URL.revokeObjectURL(url);
        node.remove();
    }
}

function detach(videoId) {
    // Synchronous visible cleanup first — rip the overlay canvases off the
    // DOM and nuke any native tracks so the user sees subs disappear
    // instantly. The async work (renderer.destroy() behind the per-video
    // lock) still runs, but can't keep the old subs visible while pending
    // attaches finish ahead of us in the queue.
    const video = getVideo(videoId);
    if (video) {
        const parent = video.parentElement;
        if (parent) {
            for (const node of Array.from(parent.querySelectorAll(".JASSUB"))) {
                node.remove();
            }
        }
        clearNativeTracks(video);
    }
    return run(videoId, () => detachInner(videoId));
}

async function detachInner(videoId) {
    const entry = state.get(videoId);
    if (entry?.renderer) {
        try { entry.renderer.destroy(); } catch (e) { /* ignore */ }
    }
    const video = getVideo(videoId);
    if (video) {
        clearNativeTracks(video);
        // Safety net: JASSUB.destroy() should remove its DOM, but if multiple
        // renderers got stacked (e.g. effect fired before previous attach
        // finished), sweep any orphaned containers under the same parent.
        const parent = video.parentElement;
        if (parent) {
            for (const node of Array.from(parent.querySelectorAll(".JASSUB"))) {
                node.remove();
            }
        }
    }
    state.delete(videoId);
}

function setAss(videoId, url) {
    return run(videoId, async () => {
        // The Rust side guarantees (via memo dedupe) that we only get called
        // on a real track change, so we always tear down + rebuild here.
        // Per-phase timing so slowdowns are attributable (fetch vs. JASSUB
        // module vs. WASM init vs. libass first paint). All payloads are
        // strings because Dioxus's dev-mode `patch_console.js` bridge chokes
        // on object args.
        const t0 = performance.now();
        const mark = (label) => {
            const dt = (performance.now() - t0).toFixed(0);
            console.log(`[binkflix] ${label} t+${dt}ms`);
        };
        mark(`setAss start url=${url}`);

        const video = getVideo(videoId);
        if (!video) throw new Error(`video element '${videoId}' not found`);

        const jassubP = loadJassub().then((m) => { mark("JASSUB module loaded"); return m; });
        const fetchP = fetch(url).then(async (r) => {
            mark(`subtitle response status=${r.status}`);
            if (!r.ok) throw new Error(`subtitle fetch failed: ${r.status}`);
            const text = await r.text();
            mark(`subtitle body read bytes=${text.length}`);
            return text;
        });
        const [JASSUB, subText] = await Promise.all([jassubP, fetchP]);
        mark("fetch+module both ready");

        // Deliberately no `loadedmetadata` wait. JASSUB 1.7+ uses
        // ResizeObserver and recovers when the video eventually gets real
        // dimensions — blocking here hands control to the video element's
        // (possibly slow) header parse, which on a NAS/USB disk can take
        // tens of seconds and trips our watchdog despite nothing being
        // actually wrong with the subtitle pipeline.

        await detachInner(videoId);
        mark("previous renderer detached");

        mark(`constructing JASSUB videoW=${video.videoWidth} videoH=${video.videoHeight}`);
        const renderer = new JASSUB({
            video,
            subContent: subText,
            workerUrl: JASSUB_WORKER,
            wasmUrl: JASSUB_WASM,
            prescaleFactor: 1.0,
            asyncRender: true,
        });
        mark("JASSUB constructor returned");

        // JASSUB emits 'ready' once the libass WASM worker has finished
        // initializing and done its first frame — this is what actually
        // shows subs on screen. Log it so we can see wasm init cost.
        try {
            renderer.addEventListener?.("ready", () => mark("JASSUB 'ready' event"));
        } catch (_) { /* older versions: no event */ }

        state.set(videoId, { renderer, url });
        mark("setAss done");
    });
}

function setVtt(videoId, url, label, language) {
    return run(videoId, () => setVttInner(videoId, url, label, language));
}

async function setVttInner(videoId, url, label, language) {
    const video = getVideo(videoId);
    if (!video) throw new Error(`video element '${videoId}' not found`);

    await detachInner(videoId);

    // Fetch + blob-url so we can revoke it on teardown and so cross-origin
    // edge cases (shouldn't apply here but future-proof) are avoided.
    const resp = await fetch(url);
    if (!resp.ok) throw new Error(`vtt fetch failed: ${resp.status}`);
    const blob = await resp.blob();
    const blobUrl = URL.createObjectURL(blob);

    const track = document.createElement("track");
    track.kind = "subtitles";
    track.src = blobUrl;
    track.label = label || "Subtitles";
    if (language) track.srclang = language;
    track.default = true;
    track.dataset.binkflix = "1";
    track.dataset.blobUrl = blobUrl;
    video.appendChild(track);

    // Force-enable once the browser has registered it.
    requestAnimationFrame(() => {
        for (const t of video.textTracks) {
            t.mode = t.label === track.label ? "showing" : "disabled";
        }
    });

    state.set(videoId, { nativeTrack: track, url });
}

// The Dioxus component calls us via `document::eval` right after the
// component mounts. Because this file is a module, it may not have finished
// loading by the time that eval runs — without a stub, those calls silently
// no-op and subtitles never appear. A pre-stub (installed synchronously
// when the HTML parser hits the <script type="module">) queues the call
// and replays it once the real implementation is ready.
// ── Custom overlay controls ────────────────────────────────────
//
// Rust renders the static chrome (buttons, scrubber, time labels, volume
// slider, subtitle menu). We wire up the dynamic behaviour here — binding
// DOM events to the <video>, syncing the scrubber + time + icon state,
// and managing the auto-hide timer. Subtitle menu state stays in Rust.
//
// Expected DOM inside the .video-wrap containing `#videoId`:
//   .player-chrome
//     input.player-scrub[type=range]
//     .player-row
//       button.player-btn.play-btn
//       span.time-cur
//       span.time-dur
//       input.volume-slider[type=range]
//       button.player-btn.fullscreen-btn
//
// Idempotent: safe to call multiple times on the same video.

const controllers = new Map(); // videoId -> teardown fn

const SVG_PLAY = `<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor" aria-hidden="true"><path d="M8 5v14l11-7z"/></svg>`;
const SVG_PAUSE = `<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor" aria-hidden="true"><rect x="6" y="5" width="4" height="14" rx="1"/><rect x="14" y="5" width="4" height="14" rx="1"/></svg>`;
const SVG_VOL_HIGH = `<svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M11 5L6 9H3v6h3l5 4V5z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M18.5 5.5a9 9 0 0 1 0 13"/></svg>`;
const SVG_VOL_MUTE = `<svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M11 5L6 9H3v6h3l5 4V5z"/><path d="M22 9l-6 6M16 9l6 6"/></svg>`;

function fmtTime(s) {
    if (!isFinite(s) || s < 0) s = 0;
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = Math.floor(s % 60);
    const pad = (n) => String(n).padStart(2, "0");
    return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`;
}

function initControls(videoId) {
    // Tear down any previous controller for this id so hot-reload / remount
    // doesn't stack listeners.
    controllers.get(videoId)?.();

    const video = getVideo(videoId);
    if (!video) return;
    const wrap = video.closest(".video-wrap");
    if (!wrap) return;

    const $ = (sel) => wrap.querySelector(sel);
    const playBtn = $(".play-btn");
    const scrub = $(".player-scrub");
    const timeCur = $(".time-cur");
    const timeDur = $(".time-dur");
    const volSlider = $(".volume-slider");
    const volBtn = $(".volume-btn");
    const fsBtn = $(".fullscreen-btn");

    let seeking = false;
    let activeTimer = null;

    const setPausedClass = () => wrap.classList.toggle("paused", video.paused);
    const setPlayIcon = () => {
        if (playBtn) playBtn.innerHTML = video.paused ? SVG_PLAY : SVG_PAUSE;
    };
    const setMuteIcon = () => {
        if (volBtn) volBtn.innerHTML = (video.muted || video.volume === 0) ? SVG_VOL_MUTE : SVG_VOL_HIGH;
    };

    const updateFill = () => {
        if (!scrub) return;
        const d = video.duration;
        let played = 0, buffered = 0;
        if (isFinite(d) && d > 0) {
            played = (video.currentTime / d) * 100;
            try {
                const b = video.buffered;
                // Pick the range covering currentTime, else the last one.
                for (let i = 0; i < b.length; i++) {
                    if (b.start(i) <= video.currentTime && video.currentTime <= b.end(i)) {
                        buffered = (b.end(i) / d) * 100;
                        break;
                    }
                    if (i === b.length - 1) buffered = (b.end(i) / d) * 100;
                }
            } catch (_) { /* ignore */ }
        }
        buffered = Math.max(buffered, played);
        scrub.style.setProperty("--played", played + "%");
        scrub.style.setProperty("--buffered", buffered + "%");
    };

    const syncTime = () => {
        if (!seeking && scrub && isFinite(video.duration)) {
            scrub.value = String((video.currentTime / video.duration) * 1000);
        }
        if (timeCur) timeCur.textContent = fmtTime(video.currentTime);
        updateFill();
    };
    const syncDuration = () => {
        if (timeDur) timeDur.textContent = fmtTime(video.duration);
        updateFill();
    };

    const onPlay = () => { setPausedClass(); setPlayIcon(); };
    const onPause = () => { setPausedClass(); setPlayIcon(); };
    const onTime = () => syncTime();
    const onMeta = () => { syncDuration(); syncTime(); };

    // Persist volume / mute across sessions. The <video> element starts
    // at volume=1 / muted=false unless we restore explicitly; apply any
    // saved preference before any user interaction so there's no
    // perceptible jump from default to saved level.
    const VOLUME_KEY = "binkflix:volume";
    try {
        const saved = JSON.parse(localStorage.getItem(VOLUME_KEY) || "null");
        if (saved && typeof saved.volume === "number") {
            video.volume = Math.min(1, Math.max(0, saved.volume));
        }
        if (saved && typeof saved.muted === "boolean") {
            video.muted = saved.muted;
        }
    } catch (_) { /* corrupt entry — ignore */ }

    const persistVolume = () => {
        try {
            localStorage.setItem(
                VOLUME_KEY,
                JSON.stringify({ volume: video.volume, muted: video.muted }),
            );
        } catch (_) { /* storage disabled / full — nothing we can do */ }
    };

    const onVol = () => {
        const v = video.muted ? 0 : video.volume;
        if (volSlider) {
            volSlider.value = String(v);
            volSlider.style.setProperty("--vol", (v * 100) + "%");
        }
        setMuteIcon();
        persistVolume();
    };

    const onProgress = () => updateFill();

    // ── Load / buffer / error state ────────────────────────────
    //
    // Drive two CSS classes on the wrap so Rust can show overlays without
    // round-tripping state through Dioxus for every video event:
    //   .loading — initial load or waiting on buffer; shows a spinner.
    //   .errored — <video>'s error event fired; shows the message.
    //
    // Error message is written into `.player-error-msg` so Rust can render
    // the static container once and we just fill the text here. Clearing
    // .errored when playback recovers (e.g. user switches source) is a
    // simple "hide on loadstart" — the video element fires that on every
    // fresh src attach.
    const errMsgEl = wrap.querySelector(".player-error-msg");
    const setLoading = (on) => wrap.classList.toggle("loading", !!on);
    const setError = (msg) => {
        if (msg) {
            if (errMsgEl) errMsgEl.textContent = msg;
            wrap.classList.add("errored");
            wrap.classList.remove("loading");
        } else {
            wrap.classList.remove("errored");
            if (errMsgEl) errMsgEl.textContent = "";
        }
    };
    const describeError = () => {
        const e = video.error;
        if (!e) return "Playback failed.";
        // MEDIA_ERR_SRC_NOT_SUPPORTED (4) is the big one: container or
        // codec the browser can't decode. Show the detail message if the
        // browser gave one.
        const codes = {
            1: "Playback aborted.",
            2: "Network error while loading video.",
            3: "Video decode error — the stream is corrupt or uses a codec this browser can't decode.",
            4: "Source or codec not supported by this browser.",
        };
        const base = codes[e.code] || `Playback failed (code ${e.code}).`;
        return e.message ? `${base} (${e.message})` : base;
    };

    const onLoadStart = () => { setError(null); setLoading(true); };
    const onWaiting = () => setLoading(true);
    const onCanPlay = () => setLoading(false);
    const onPlaying = () => setLoading(false);
    const onLoadedData = () => setLoading(false);
    const onStalled = () => setLoading(true);
    const onError = () => setError(describeError());

    video.addEventListener("play", onPlay);
    video.addEventListener("pause", onPause);
    video.addEventListener("timeupdate", onTime);
    video.addEventListener("loadedmetadata", onMeta);
    video.addEventListener("durationchange", onMeta);
    video.addEventListener("volumechange", onVol);
    video.addEventListener("progress", onProgress);
    video.addEventListener("loadstart", onLoadStart);
    video.addEventListener("waiting", onWaiting);
    video.addEventListener("canplay", onCanPlay);
    video.addEventListener("playing", onPlaying);
    video.addEventListener("loadeddata", onLoadedData);
    video.addEventListener("stalled", onStalled);
    video.addEventListener("error", onError);
    // If we're initialising after the video already started loading, the
    // readyState lets us pick the right starting state without waiting for
    // the next event.
    if (video.error) {
        setError(describeError());
    } else if (video.readyState < 3) {
        setLoading(true);
    }

    // Stop chrome-button clicks from ever bubbling to the wrap-level listener,
    // which only treats clicks on the video itself as a play/pause toggle.
    // Defence-in-depth: even though the wrap handler checks `target === video`,
    // some browsers / devtools redispatch events oddly, and a single stray
    // pause from clicking the mute button is a miserable UX bug.
    const stopBubble = (e) => e.stopPropagation();

    // Drop focus after any chrome interaction so the slider doesn't park
    // focus and hijack future arrow keys. Our custom shortcuts still work
    // regardless (onKey allows range inputs through), but this also kills
    // the persistent focus ring.
    const blurSelf = (e) => { e.currentTarget.blur?.(); };

    const onPlayBtn = () => { if (video.paused) video.play(); else video.pause(); };
    const onScrubInput = () => {
        seeking = true;
        if (timeCur && isFinite(video.duration)) {
            timeCur.textContent = fmtTime((scrub.value / 1000) * video.duration);
        }
    };
    const onScrubChange = () => {
        if (isFinite(video.duration)) {
            video.currentTime = (scrub.value / 1000) * video.duration;
        }
        seeking = false;
    };
    const onVolInput = () => {
        const v = Number(volSlider.value);
        video.volume = v;
        video.muted = v === 0;
        volSlider.style.setProperty("--vol", (v * 100) + "%");
    };
    const onVolBtn = () => { video.muted = !video.muted; };
    const onFsBtn = () => {
        if (document.fullscreenElement) document.exitFullscreen();
        else wrap.requestFullscreen?.();
    };

    playBtn?.addEventListener("click", onPlayBtn);
    playBtn?.addEventListener("click", stopBubble);
    playBtn?.addEventListener("pointerup", blurSelf);
    scrub?.addEventListener("input", onScrubInput);
    scrub?.addEventListener("change", onScrubChange);
    scrub?.addEventListener("click", stopBubble);
    scrub?.addEventListener("pointerup", blurSelf);
    volSlider?.addEventListener("input", onVolInput);
    volSlider?.addEventListener("click", stopBubble);
    volSlider?.addEventListener("pointerup", blurSelf);
    volBtn?.addEventListener("click", onVolBtn);
    volBtn?.addEventListener("click", stopBubble);
    volBtn?.addEventListener("pointerup", blurSelf);
    fsBtn?.addEventListener("click", onFsBtn);
    fsBtn?.addEventListener("click", stopBubble);
    fsBtn?.addEventListener("pointerup", blurSelf);

    // Click video area toggles play (but not on the chrome itself).
    const onVideoClick = (e) => {
        if (e.target === video) onPlayBtn();
    };
    wrap.addEventListener("click", onVideoClick);

    // Auto-hide: show chrome on mouse activity, hide after 2s of idle playback.
    const bumpActive = () => {
        wrap.classList.add("active");
        if (activeTimer) clearTimeout(activeTimer);
        activeTimer = setTimeout(() => {
            if (!video.paused) wrap.classList.remove("active");
        }, 2000);
    };
    const onLeave = () => {
        if (activeTimer) clearTimeout(activeTimer);
        if (!video.paused) wrap.classList.remove("active");
    };
    wrap.addEventListener("mousemove", bumpActive);
    wrap.addEventListener("mouseleave", onLeave);

    // Keyboard shortcuts (document-level so the user doesn't have to click the
    // video first). Bail if they're typing in a form field.
    const onKey = (e) => {
        // Only bail for *text* inputs / editables — don't let a focused
        // range slider (scrubber, volume) steal arrow keys from our custom
        // shortcuts. After a user drags a slider, focus parks there and
        // would otherwise capture ArrowLeft/Right/Up/Down.
        const t = e.target;
        if (t) {
            const tag = t.tagName;
            const type = (t.type || "").toLowerCase();
            const isTextInput =
                tag === "TEXTAREA" ||
                t.isContentEditable ||
                (tag === "INPUT" && type !== "range" && type !== "button" && type !== "checkbox");
            if (isTextInput) return;
        }
        const step = 5;
        const vstep = 0.05;
        switch (e.key) {
            case " ":
            case "k":
                e.preventDefault(); onPlayBtn(); bumpActive(); break;
            case "ArrowLeft":
                e.preventDefault();
                video.currentTime = Math.max(0, video.currentTime - step);
                bumpActive(); break;
            case "ArrowRight":
                e.preventDefault();
                if (isFinite(video.duration)) {
                    video.currentTime = Math.min(video.duration, video.currentTime + step);
                }
                bumpActive(); break;
            case "ArrowUp":
                e.preventDefault();
                video.muted = false;
                video.volume = Math.min(1, video.volume + vstep);
                bumpActive(); break;
            case "ArrowDown":
                e.preventDefault();
                video.volume = Math.max(0, video.volume - vstep);
                bumpActive(); break;
            case "m":
            case "M":
                e.preventDefault(); video.muted = !video.muted; bumpActive(); break;
            case "f":
            case "F":
                e.preventDefault(); onFsBtn(); bumpActive(); break;
        }
    };
    document.addEventListener("keydown", onKey);

    // Initial sync
    setPausedClass();
    setPlayIcon();
    syncTime();
    syncDuration();
    onVol();

    controllers.set(videoId, () => {
        video.removeEventListener("play", onPlay);
        video.removeEventListener("pause", onPause);
        video.removeEventListener("timeupdate", onTime);
        video.removeEventListener("loadedmetadata", onMeta);
        video.removeEventListener("durationchange", onMeta);
        video.removeEventListener("volumechange", onVol);
        video.removeEventListener("progress", onProgress);
        video.removeEventListener("loadstart", onLoadStart);
        video.removeEventListener("waiting", onWaiting);
        video.removeEventListener("canplay", onCanPlay);
        video.removeEventListener("playing", onPlaying);
        video.removeEventListener("loadeddata", onLoadedData);
        video.removeEventListener("stalled", onStalled);
        video.removeEventListener("error", onError);
        playBtn?.removeEventListener("click", onPlayBtn);
        playBtn?.removeEventListener("click", stopBubble);
        playBtn?.removeEventListener("pointerup", blurSelf);
        scrub?.removeEventListener("input", onScrubInput);
        scrub?.removeEventListener("change", onScrubChange);
        scrub?.removeEventListener("click", stopBubble);
        scrub?.removeEventListener("pointerup", blurSelf);
        volSlider?.removeEventListener("input", onVolInput);
        volSlider?.removeEventListener("click", stopBubble);
        volSlider?.removeEventListener("pointerup", blurSelf);
        volBtn?.removeEventListener("click", onVolBtn);
        volBtn?.removeEventListener("click", stopBubble);
        volBtn?.removeEventListener("pointerup", blurSelf);
        fsBtn?.removeEventListener("click", onFsBtn);
        fsBtn?.removeEventListener("click", stopBubble);
        fsBtn?.removeEventListener("pointerup", blurSelf);
        wrap.removeEventListener("click", onVideoClick);
        wrap.removeEventListener("mousemove", bumpActive);
        wrap.removeEventListener("mouseleave", onLeave);
        document.removeEventListener("keydown", onKey);
        if (activeTimer) clearTimeout(activeTimer);
        controllers.delete(videoId);
    });
}

// Cache of per-URL HEAD probe results. Populated lazily by getDebugStats
// so we know what the server actually sent back (Content-Type / Accept-
// Ranges) rather than inferring from a client-side button press. Keyed
// by currentSrc so re-loading with a different ?mode= fetches fresh.
const streamInfoCache = new Map(); // src -> { content_type, accept_ranges, content_length }
const streamInfoPending = new Set();

function ensureStreamInfo(src) {
    if (!src || streamInfoCache.has(src) || streamInfoPending.has(src)) return;
    streamInfoPending.add(src);
    fetch(src, { method: "HEAD" })
        .then((r) => {
            streamInfoCache.set(src, {
                content_type: r.headers.get("content-type"),
                accept_ranges: r.headers.get("accept-ranges"),
                content_length: r.headers.get("content-length"),
                // Explicit mode + per-stream actions set by the server
                // — read these instead of inferring from Accept-Ranges
                // so the panel can distinguish remux from transcode.
                mode: r.headers.get("x-stream-mode"),
                video_action: r.headers.get("x-stream-video"),
                audio_action: r.headers.get("x-stream-audio"),
                status: r.status,
            });
        })
        .catch(() => { /* leave absent; next poll retries */ })
        .finally(() => streamInfoPending.delete(src));
}

// Runtime playback stats for the debug menu. ffprobe gives us the
// authoritative codec/bitrate info server-side; this covers what only
// the browser knows — the rendered resolution (post-scaling),
// readyState, buffered ranges, dropped frames, playback rate, plus
// observed response headers for the current stream.
function getDebugStats(videoId) {
    const video = getVideo(videoId);
    if (!video) return null;
    let buffered = 0;
    try {
        const b = video.buffered;
        for (let i = 0; i < b.length; i++) {
            if (b.start(i) <= video.currentTime && video.currentTime <= b.end(i)) {
                buffered = b.end(i) - video.currentTime;
                break;
            }
        }
    } catch (_) { /* ignore */ }
    let droppedFrames = null, totalFrames = null;
    try {
        const q = video.getVideoPlaybackQuality?.();
        if (q) {
            droppedFrames = q.droppedVideoFrames;
            totalFrames = q.totalVideoFrames;
        }
    } catch (_) { /* ignore */ }
    const readyStates = ["HAVE_NOTHING", "HAVE_METADATA", "HAVE_CURRENT_DATA", "HAVE_FUTURE_DATA", "HAVE_ENOUGH_DATA"];
    const src = video.currentSrc || video.src;
    ensureStreamInfo(src);
    return {
        videoWidth: video.videoWidth,
        videoHeight: video.videoHeight,
        readyState: readyStates[video.readyState] || String(video.readyState),
        networkState: video.networkState,
        buffered_ahead_seconds: Number(buffered.toFixed(1)),
        current_time: Number(video.currentTime.toFixed(1)),
        duration: isFinite(video.duration) ? Number(video.duration.toFixed(1)) : null,
        playback_rate: video.playbackRate,
        dropped_frames: droppedFrames,
        total_frames: totalFrames,
        muted: video.muted,
        volume: Number(video.volume.toFixed(2)),
        current_src: src,
        error: video.error ? { code: video.error.code, message: video.error.message || null } : null,
        stream_info: streamInfoCache.get(src) || null,
    };
}

const realApi = {
    setAss,
    setVtt,
    clear: detach,
    initControls,
    getDebugStats,
};

const pending = (window.binkflixPlayer && window.binkflixPlayer.__queue) || [];
window.binkflixPlayer = new Proxy(realApi, {
    get(target, prop) {
        return target[prop];
    },
});
for (const [method, args] of pending) {
    try {
        const result = realApi[method]?.(...args);
        if (result && typeof result.catch === "function") {
            result.catch((e) => console.error(`[binkflix] ${method} (replayed) failed`, e));
        }
    } catch (e) {
        console.error(`[binkflix] ${method} (replayed) threw`, e);
    }
}
