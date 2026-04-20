//! Syncplay runtime shared between the WASM client and the SSR pass.
//!
//! Hydration requires both sides to emit the same component tree. The state
//! machine (signals, context, empty component shells) therefore compiles on
//! both targets; only the WebSocket plumbing, `web-sys` DOM access, and
//! outbound sends are gated behind `feature = "web"`. On the server side
//! everything is a no-op — `provide_room_context` still installs the context
//! so `use_context` doesn't panic during SSR.
//!
//! The long-lived WS task is fed by an `UnboundedSender` stored in the
//! `RoomContext`. Leaving a room clears the sender → the paired receiver
//! drains and the socket closes automatically.

use crate::types::{ClientMsg, RoomListItem, RoomState};
use dioxus::prelude::*;

#[cfg(feature = "web")]
use crate::types::Broadcast;

#[cfg(feature = "web")]
use futures::channel::mpsc::{unbounded, UnboundedSender};
#[cfg(feature = "web")]
use futures::{SinkExt, StreamExt};
#[cfg(feature = "web")]
use gloo_net::websocket::{futures::WebSocket, Message};
#[cfg(feature = "web")]
use std::cell::{Cell, RefCell};
#[cfg(feature = "web")]
use std::rc::Rc;
#[cfg(feature = "web")]
use wasm_bindgen::closure::Closure;
#[cfg(feature = "web")]
use wasm_bindgen::JsCast;
#[cfg(feature = "web")]
use web_sys::HtmlVideoElement;

/// On non-web targets we don't actually send anything, but we still need a
/// type with the same shape so `RoomContext` is identical across targets.
/// `()` satisfies that — `Signal<Option<()>>` is just a presence bit.
#[cfg(not(feature = "web"))]
type UnboundedSender<T> = std::marker::PhantomData<T>;

/// A discrete remote playback event applied to our local `<video>`. Tagged
/// with a monotonic counter so repeated values (e.g. two pauses at 0ms) still
/// trigger `PartialEq`-based effects.
#[derive(Clone, PartialEq)]
pub struct RemoteEvent {
    pub seq: u64,
    pub kind: RemoteKind,
}

#[derive(Clone, PartialEq)]
pub enum RemoteKind {
    Play { position_ms: i64 },
    Pause { position_ms: i64 },
    Seek { position_ms: i64 },
}

/// Provided once at the `App` root. All fields are `Copy` signals; the struct
/// itself is `Copy`, so subcomponents grab it via `use_context::<RoomContext>()`
/// and mutate signals without further plumbing.
#[derive(Clone, Copy)]
pub struct RoomContext {
    pub room_id: Signal<Option<String>>,
    pub client_id: Signal<Option<String>>,
    pub viewers: Signal<usize>,
    pub current: Signal<Option<RoomState>>,
    /// Target media the router should push to. Cleared by the navigator after
    /// it performs the navigation. Set by the WS task on Welcome/SetMedia.
    pub pending_nav: Signal<Option<String>>,
    /// Media id whose SetMedia we've already emitted or applied. Breaks
    /// navigation loops — a client navigated here by a remote SetMedia won't
    /// re-emit it, and a client applying its own SetMedia won't re-apply it.
    pub last_applied_media: Signal<Option<String>>,
    pub last_remote: Signal<Option<RemoteEvent>>,
    /// Outbound channel to the WS write loop. `None` when not in a room.
    /// Clearing this drops the sender, which drains the receiver and stops
    /// the task. On non-web targets this is a PhantomData so nothing crosses.
    pub tx: Signal<Option<UnboundedSender<ClientMsg>>>,
}

impl RoomContext {
    pub fn is_in_room(&self) -> bool {
        self.room_id.peek().is_some()
    }

    #[cfg(feature = "web")]
    pub fn send(&self, msg: ClientMsg) {
        if let Some(tx) = self.tx.peek().as_ref() {
            let _ = tx.unbounded_send(msg);
        }
    }

    #[cfg(not(feature = "web"))]
    pub fn send(&self, _msg: ClientMsg) {}

    /// Local Leave: clears all signals, which drops the sender and stops the task.
    pub fn leave(&self) {
        let mut this = *self;
        this.tx.set(None);
        this.room_id.set(None);
        this.client_id.set(None);
        this.viewers.set(0);
        this.current.set(None);
        this.pending_nav.set(None);
        this.last_applied_media.set(None);
        this.last_remote.set(None);
    }

    pub fn set_last_applied(&self, v: Option<String>) {
        let mut this = *self;
        this.last_applied_media.set(v);
    }

    pub fn set_pending_nav(&self, v: Option<String>) {
        let mut this = *self;
        this.pending_nav.set(v);
    }
}

/// Call this once at the top of `Shell` so the context is available everywhere.
/// Must be called inside a component — signals attach to that scope.
pub fn provide_room_context() -> RoomContext {
    let room_id = use_signal::<Option<String>>(|| None);
    let client_id = use_signal::<Option<String>>(|| None);
    let viewers = use_signal::<usize>(|| 0);
    let current = use_signal::<Option<RoomState>>(|| None);
    let pending_nav = use_signal::<Option<String>>(|| None);
    let last_applied_media = use_signal::<Option<String>>(|| None);
    let last_remote = use_signal::<Option<RemoteEvent>>(|| None);
    let tx = use_signal::<Option<UnboundedSender<ClientMsg>>>(|| None);
    let ctx = RoomContext {
        room_id,
        client_id,
        viewers,
        current,
        pending_nav,
        last_applied_media,
        last_remote,
        tx,
    };
    use_context_provider(|| ctx);
    ctx
}

pub fn use_room_context() -> RoomContext {
    use_context::<RoomContext>()
}

// ---------- web-only: WebSocket task + DOM bridging ----------

#[cfg(feature = "web")]
fn ws_url(room_id: &str) -> Option<String> {
    let loc = web_sys::window()?.location();
    let protocol = loc.protocol().ok()?;
    let host = loc.host().ok()?;
    let ws_proto = if protocol == "https:" { "wss" } else { "ws" };
    Some(format!("{ws_proto}://{host}/api/syncplay/{room_id}"))
}

#[cfg(feature = "web")]
pub fn join_room(ctx: RoomContext, room_id: String) {
    ctx.leave();

    let Some(url) = ws_url(&room_id) else {
        tracing::warn!("no window.location; cannot build ws url");
        return;
    };

    let (tx, mut rx) = unbounded::<ClientMsg>();
    let mut tx_sig = ctx.tx;
    tx_sig.set(Some(tx));
    let mut room_id_sig = ctx.room_id;
    room_id_sig.set(Some(room_id));

    wasm_bindgen_futures::spawn_local(async move {
        let ws = match WebSocket::open(&url) {
            Ok(ws) => ws,
            Err(e) => {
                tracing::warn!(?e, "ws open failed");
                ctx.leave();
                return;
            }
        };
        let (mut write, mut read) = ws.split();

        // Outbound pump — owns the receiver. Exits when the last sender drops.
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(msg) = rx.next().await {
                let Ok(text) = serde_json::to_string(&msg) else { continue };
                if write.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            let _ = write.close().await;
        });

        let mut seq: u64 = 0;
        while let Some(Ok(Message::Text(text))) = read.next().await {
            let Ok(b) = serde_json::from_str::<Broadcast>(&text) else { continue };
            handle_broadcast(ctx, b, &mut seq);
        }
        ctx.leave();
    });
}

#[cfg(feature = "web")]
pub fn create_and_join(ctx: RoomContext) {
    wasm_bindgen_futures::spawn_local(async move {
        match crate::client_api::create_room().await {
            Ok(resp) => join_room(ctx, resp.id),
            Err(e) => tracing::warn!(%e, "create_room failed"),
        }
    });
}

#[cfg(not(feature = "web"))]
pub fn join_room(_ctx: RoomContext, _room_id: String) {}

#[cfg(not(feature = "web"))]
pub fn create_and_join(_ctx: RoomContext) {}

#[cfg(feature = "web")]
fn handle_broadcast(ctx: RoomContext, b: Broadcast, seq: &mut u64) {
    let our_id = ctx.client_id.peek().clone();
    let is_us = |from: &str| our_id.as_deref() == Some(from);
    let mut client_id_sig = ctx.client_id;
    let mut viewers_sig = ctx.viewers;
    let mut current_sig = ctx.current;
    let mut last_remote_sig = ctx.last_remote;

    match b {
        Broadcast::Welcome { client_id, current, .. } => {
            client_id_sig.set(Some(client_id));
            if let Some(state) = current.as_ref() {
                let media = state.media_id.clone();
                if ctx.last_applied_media.peek().as_deref() != Some(&media) {
                    ctx.set_pending_nav(Some(media));
                }
            }
            current_sig.set(current);
        }
        Broadcast::Peer { viewers, .. } => {
            viewers_sig.set(viewers);
        }
        Broadcast::SetMedia { media_id, from, .. } => {
            if is_us(&from) {
                return;
            }
            if ctx.last_applied_media.peek().as_deref() == Some(&media_id) {
                return;
            }
            ctx.set_pending_nav(Some(media_id.clone()));
            current_sig.set(Some(RoomState {
                media_id,
                position_ms: 0,
                playing: true,
                updated_at: 0,
            }));
        }
        Broadcast::Play { position_ms, from, .. } => {
            if is_us(&from) {
                return;
            }
            *seq = seq.wrapping_add(1);
            last_remote_sig.set(Some(RemoteEvent {
                seq: *seq,
                kind: RemoteKind::Play { position_ms },
            }));
        }
        Broadcast::Pause { position_ms, from, .. } => {
            if is_us(&from) {
                return;
            }
            *seq = seq.wrapping_add(1);
            last_remote_sig.set(Some(RemoteEvent {
                seq: *seq,
                kind: RemoteKind::Pause { position_ms },
            }));
        }
        Broadcast::Seek { position_ms, from, .. } => {
            if is_us(&from) {
                return;
            }
            *seq = seq.wrapping_add(1);
            last_remote_sig.set(Some(RemoteEvent {
                seq: *seq,
                kind: RemoteKind::Seek { position_ms },
            }));
        }
        Broadcast::Pong { .. } | Broadcast::Drift { .. } => {}
    }
}

// ---------- components (both targets) ----------

/// Watches `pending_nav` and drives the router. Must be mounted inside the
/// Router subtree so `use_navigator()` works.
#[component]
pub fn RoomNavigator() -> Element {
    let ctx = use_room_context();
    let nav = use_navigator();

    use_effect(move || {
        let target = ctx.pending_nav.read().clone();
        if let Some(media_id) = target {
            ctx.set_pending_nav(None);
            ctx.set_last_applied(Some(media_id.clone()));
            nav.push(crate::app::Route::MediaPlay { id: media_id });
        }
    });

    rsx! {}
}

/// Rooms dropdown in the topbar. Shows active rooms and Join/Leave controls.
#[component]
pub fn RoomsDropdown() -> Element {
    let ctx = use_room_context();
    let mut open_menu = use_context::<crate::app::OpenMenu>().0;
    let is_open = *open_menu.read() == Some("rooms");
    let mut rooms = use_resource(move || async move {
        // Reading `open_menu` in the future body subscribes — opening refetches.
        let _ = open_menu.read();
        crate::client_api::get_rooms().await
    });

    let in_room = ctx.room_id.read().is_some();
    let viewers = *ctx.viewers.read();
    let current_state = ctx.current.read().clone();

    let title = if in_room {
        let id = ctx.room_id.read().clone().unwrap_or_default();
        let short: String = id.chars().take(6).collect();
        let media = current_state
            .as_ref()
            .map(|s| format!(" · watching {}", s.media_id))
            .unwrap_or_else(|| " · idle".to_string());
        format!("Room {short}…{media} ({viewers})")
    } else {
        "Rooms".to_string()
    };
    let _ = viewers;

    let current_media_title = current_state.as_ref().map(|s| s.media_id.clone());
    let short_id: Option<String> = ctx
        .room_id
        .read()
        .clone()
        .map(|id| id.chars().take(6).collect());

    rsx! {
        div { class: "rooms-dd",
            button {
                class: if in_room { "rooms-btn btn-icon in-room" } else { "rooms-btn btn-icon" },
                aria_label: "Rooms",
                title: "{title}",
                onclick: move |_| {
                    if is_open {
                        open_menu.set(None);
                    } else {
                        open_menu.set(Some("rooms"));
                        rooms.restart();
                    }
                },
                span { class: "icon", dangerous_inner_html: crate::app::ICON_GROUP }
                if in_room {
                    span { class: "rooms-badge", "{viewers}" }
                }
            }
            if is_open {
                div { class: "rooms-panel",
                    div { class: "rooms-head",
                        span { class: "rooms-title", "Watch party" }
                        if in_room {
                            span { class: "rooms-pill", "Live · {viewers}" }
                        }
                    }

                    if in_room {
                        div { class: "rooms-current",
                            div { class: "rooms-current-main",
                                span { class: "rooms-current-label",
                                    if let Some(t) = current_media_title.as_deref() {
                                        "{t}"
                                    } else {
                                        "Idle — no media playing"
                                    }
                                }
                                if let Some(sid) = short_id.as_deref() {
                                    span { class: "rooms-current-id", "Room {sid}…" }
                                }
                            }
                            button {
                                class: "rooms-leave",
                                r#type: "button",
                                onclick: move |_| {
                                    ctx.leave();
                                    open_menu.set(None);
                                },
                                "Leave"
                            }
                        }
                    } else {
                        button {
                            class: "rooms-create",
                            r#type: "button",
                            onclick: move |_| {
                                create_and_join(ctx);
                                open_menu.set(None);
                            },
                            span { class: "rooms-plus", "＋" }
                            span { "Create new room" }
                        }

                        div { class: "rooms-section-label", "Active rooms" }
                        div { class: "rooms-list",
                            match &*rooms.read_unchecked() {
                                None => rsx! { div { class: "rooms-empty", "Loading…" } },
                                Some(Err(e)) => rsx! { div { class: "rooms-empty error", "Error: {e}" } },
                                Some(Ok(list)) if list.is_empty() => rsx! {
                                    div { class: "rooms-empty", "No active rooms" }
                                },
                                Some(Ok(list)) => rsx! {
                                    for r in list.iter().cloned() {
                                        RoomRow { key: "{r.id}", room: r }
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
fn RoomRow(room: RoomListItem) -> Element {
    let ctx = use_room_context();
    let (label, is_idle) = match room.current_media_title.clone() {
        Some(t) => (t, false),
        None => ("Idle".to_string(), true),
    };
    let id = room.id.clone();
    let short: String = room.id.chars().take(6).collect();
    rsx! {
        button {
            class: "rooms-row",
            r#type: "button",
            onclick: move |_| {
                join_room(ctx, id.clone());
            },
            span { class: "rooms-row-main",
                span {
                    class: if is_idle { "rooms-row-title muted" } else { "rooms-row-title" },
                    "{label}"
                }
                span { class: "rooms-row-meta",
                    "Room {short}… · {room.viewers} viewer"
                    if room.viewers != 1 { "s" }
                }
            }
            span { class: "rooms-row-join", "Join" }
        }
    }
}

/// Wires a `<video>` element to the room WS. Mounts inside `MediaPlay`.
/// On non-web targets this renders nothing (SSR sees an empty node, which
/// matches the hydrated DOM shape since the real effects run post-hydration).
#[component]
pub fn SyncplayBridge(video_dom_id: String, media_id: String) -> Element {
    #[cfg(feature = "web")]
    {
        let ctx = use_room_context();
        let suppress: Rc<Cell<u32>> = use_hook(|| Rc::new(Cell::new(0)));
        // Handle slot lives for component lifetime; Drop removes listeners on unmount.
        let handle_slot: Rc<RefCell<Option<ListenerHandle>>> =
            use_hook(|| Rc::new(RefCell::new(None)));

        // Announce what this tab is playing. Subscribes to `room_id` so if the
        // user joins a room while already on MediaPlay the SetMedia still fires.
        {
            let media_id = media_id.clone();
            use_effect(move || {
                // Subscribe to room_id, not peek — re-run when joining/leaving.
                let in_room = ctx.room_id.read().is_some();
                if !in_room {
                    return;
                }
                if ctx.last_applied_media.peek().as_deref() == Some(&media_id) {
                    return;
                }
                ctx.set_last_applied(Some(media_id.clone()));
                ctx.send(ClientMsg::SetMedia { media_id: media_id.clone() });
            });
        }

        // Attach DOM event listeners. The <video> element is now conditionally
        // rendered — VideoPlayer holds off mounting it until the tech probe
        // resolves (and, for HLS-needed files, until `attach` has wired up
        // hls.js). That means a single post-commit `use_effect` often fires
        // before the element exists. Poll for it from a spawned task so we
        // attach as soon as it lands, and stop the moment we succeed or the
        // component unmounts.
        {
            let video_dom_id = video_dom_id.clone();
            let suppress_l = suppress.clone();
            let slot = handle_slot.clone();
            use_hook(|| {
                wasm_bindgen_futures::spawn_local(async move {
                    // Cap polling at ~60s — well past any realistic probe +
                    // first-segment wait. If we haven't seen the element by
                    // then, the bridge will stay silent rather than spin.
                    for _ in 0..600 {
                        if slot.borrow().is_some() {
                            return;
                        }
                        if let Some(video) = lookup_video(&video_dom_id) {
                            let mut handle = ListenerHandle::new(video.clone());
                            let video_for_cb = video.clone();
                            let make = |mapper: Box<dyn Fn(i64) -> ClientMsg>|
                                -> Closure<dyn FnMut()>
                            {
                                let suppress = suppress_l.clone();
                                let video = video_for_cb.clone();
                                Closure::wrap(Box::new(move || {
                                    let n = suppress.get();
                                    if n > 0 {
                                        suppress.set(n - 1);
                                        return;
                                    }
                                    let pos_ms = (video.current_time() * 1000.0) as i64;
                                    ctx.send(mapper(pos_ms));
                                }) as Box<dyn FnMut()>)
                            };
                            let on_play = make(Box::new(|p| ClientMsg::Play { position_ms: p }));
                            let on_pause = make(Box::new(|p| ClientMsg::Pause { position_ms: p }));
                            let on_seek = make(Box::new(|p| ClientMsg::Seek { position_ms: p }));
                            let _ = video.add_event_listener_with_callback(
                                "play", on_play.as_ref().unchecked_ref());
                            let _ = video.add_event_listener_with_callback(
                                "pause", on_pause.as_ref().unchecked_ref());
                            let _ = video.add_event_listener_with_callback(
                                "seeked", on_seek.as_ref().unchecked_ref());
                            handle.push("play", on_play);
                            handle.push("pause", on_pause);
                            handle.push("seeked", on_seek);
                            *slot.borrow_mut() = Some(handle);

                            // Initial catch-up: when a user is pulled into
                            // a room mid-playback, the Welcome carries the
                            // current position + playing state but nothing
                            // pushes it to the fresh video element. Apply
                            // it here once, at the time the listeners are
                            // wired, so the new viewer lands at the right
                            // moment instead of starting from 0. Wait for
                            // the element to be ready to seek — calling
                            // set_current_time before loadedmetadata queues
                            // the seek silently on some browsers.
                            if let Some(state) = ctx.current.peek().clone() {
                                let kind = if state.playing {
                                    RemoteKind::Play { position_ms: state.position_ms }
                                } else {
                                    RemoteKind::Pause { position_ms: state.position_ms }
                                };
                                let video_for_sync = video.clone();
                                let suppress_sync = suppress_l.clone();
                                wasm_bindgen_futures::spawn_local(async move {
                                    // `readyState >= 1` (HAVE_METADATA) is
                                    // the minimum for a seek to land on a
                                    // real keyframe; poll briefly, then
                                    // apply even if we time out so a stall
                                    // in metadata load doesn't leave the
                                    // late joiner at t=0 forever.
                                    for _ in 0..100 {
                                        if video_for_sync.ready_state() >= 1 {
                                            break;
                                        }
                                        gloo_timers::future::TimeoutFuture::new(100).await;
                                    }
                                    apply_remote(&video_for_sync, &kind, &suppress_sync);
                                });
                            }
                            return;
                        }
                        gloo_timers::future::TimeoutFuture::new(100).await;
                    }
                });
            });
        }

        // React to remote events: apply to the video element.
        {
            let video_dom_id = video_dom_id.clone();
            let suppress_r = suppress.clone();
            use_effect(move || {
                let Some(evt) = ctx.last_remote.read().clone() else { return };
                let Some(video) = lookup_video(&video_dom_id) else { return };
                apply_remote(&video, &evt.kind, &suppress_r);
            });
        }
    }

    #[cfg(not(feature = "web"))]
    {
        let _ = (video_dom_id, media_id);
    }

    rsx! {}
}

#[cfg(feature = "web")]
fn lookup_video(dom_id: &str) -> Option<HtmlVideoElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id(dom_id)?
        .dyn_into::<HtmlVideoElement>()
        .ok()
}

#[cfg(feature = "web")]
fn apply_remote(video: &HtmlVideoElement, kind: &RemoteKind, suppress: &Rc<Cell<u32>>) {
    match kind {
        RemoteKind::Seek { position_ms } => {
            let target = *position_ms as f64 / 1000.0;
            if (video.current_time() - target).abs() > 0.25 {
                suppress.set(suppress.get() + 1);
                video.set_current_time(target);
            }
        }
        RemoteKind::Play { position_ms } => {
            let target = *position_ms as f64 / 1000.0;
            if (video.current_time() - target).abs() > 0.5 {
                suppress.set(suppress.get() + 1);
                video.set_current_time(target);
            }
            if video.paused() {
                suppress.set(suppress.get() + 1);
                let _ = video.play();
            }
        }
        RemoteKind::Pause { position_ms } => {
            let target = *position_ms as f64 / 1000.0;
            if (video.current_time() - target).abs() > 0.5 {
                suppress.set(suppress.get() + 1);
                video.set_current_time(target);
            }
            if !video.paused() {
                suppress.set(suppress.get() + 1);
                let _ = video.pause();
            }
        }
    }
}

#[cfg(feature = "web")]
struct ListenerHandle {
    target: Option<HtmlVideoElement>,
    listeners: Vec<(&'static str, Closure<dyn FnMut()>)>,
}

#[cfg(feature = "web")]
impl ListenerHandle {
    fn empty() -> Self {
        Self { target: None, listeners: Vec::new() }
    }
    fn new(target: HtmlVideoElement) -> Self {
        Self { target: Some(target), listeners: Vec::new() }
    }
    fn push(&mut self, name: &'static str, cb: Closure<dyn FnMut()>) {
        self.listeners.push((name, cb));
    }
}

#[cfg(feature = "web")]
impl Drop for ListenerHandle {
    fn drop(&mut self) {
        if let Some(el) = &self.target {
            for (name, cb) in &self.listeners {
                let _ = el.remove_event_listener_with_callback(name, cb.as_ref().unchecked_ref());
            }
        }
    }
}
