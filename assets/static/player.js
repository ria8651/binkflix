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
const realApi = {
    setAss,
    setVtt,
    clear: detach,
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
