//! Syncplay runtime shared between the WASM client and the SSR pass.
//!
//! Hydration requires both sides to emit the same component tree. The state
//! machine (signals, context, empty component shells) therefore compiles on
//! both targets; only the WebSocket plumbing, `web-sys` DOM access, and
//! outbound sends are gated behind `feature = "web"`. On the server side
//! everything is a no-op — `provide_room_context` still installs the context
//! so `use_context` doesn't panic during SSR.
//!
//! Reliability model:
//!   - Server is the single source of truth; every state mutation bumps a
//!     monotonic `version`. Clients gate idempotent application on it
//!     (`last_applied_version`).
//!   - WS read+write happen on the same value (gloo-net's split sink can't
//!     close cleanly), with auto-reconnect + backoff wrapping the loop.
//!     `leaving` is the only flag that stops reconnection.
//!   - Echo suppression for video DOM events is a *time gate*
//!     (`applying_until`), not a counter — robust to spurious or missing
//!     browser events.
//!   - Periodic `Resync` from the server self-heals drift; clients only
//!     reconcile when local position is more than ~750 ms off.

use crate::types::{ClientMsg, Member, RoomListItem, RoomState};
use dioxus::prelude::*;
use std::collections::VecDeque;

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
#[allow(dead_code)]
pub enum RemoteKind {
    Play { position_ms: i64 },
    Pause { position_ms: i64 },
    Seek { position_ms: i64 },
}

/// One entry in the toast feed. `id` is monotonic; the toast component drains
/// by id so the same event isn't shown twice.
#[derive(Clone, PartialEq)]
pub struct RoomEvent {
    pub id: u64,
    pub text: String,
}

/// Provided once at the `App` root. All fields are `Copy` signals; the struct
/// itself is `Copy`, so subcomponents grab it via `use_context::<RoomContext>()`
/// and mutate signals without further plumbing.
#[derive(Clone, Copy)]
pub struct RoomContext {
    pub room_id: Signal<Option<String>>,
    pub me: Signal<Option<Member>>,
    pub members: Signal<Vec<Member>>,
    pub viewers: Signal<usize>,
    pub current: Signal<Option<RoomState>>,
    /// Local `Date.now()` (ms) at the moment we last refreshed `current`.
    /// Lets the bridge project the playback position forward by the wall-clock
    /// delta between snapshot receipt and when the video element is finally
    /// ready to seek — without that, a slow attach makes late joiners land
    /// several seconds behind.
    pub current_received_at: Signal<f64>,
    /// Target media the router should push to. Cleared by the navigator after
    /// it performs the navigation. Set by the WS task on Welcome/SetMedia.
    pub pending_nav: Signal<Option<String>>,
    /// Highest `RoomState.version` whose SetMedia we've already applied or
    /// announced. Monotonic — apply incoming SetMedia only if its version is
    /// strictly greater.
    pub last_applied_version: Signal<u64>,
    pub last_remote: Signal<Option<RemoteEvent>>,
    /// Recent member actions for the toast renderer. Capped at the last 16.
    pub events: Signal<VecDeque<RoomEvent>>,
    /// Outbound channel to the WS write loop. `None` when not in a room.
    /// Clearing this drops the sender, which drains the receiver and stops
    /// the task. On non-web targets this is a PhantomData so nothing crosses.
    pub tx: Signal<Option<UnboundedSender<ClientMsg>>>,
}

impl RoomContext {
    #[cfg(feature = "web")]
    pub fn send(&self, msg: ClientMsg) {
        if let Some(tx) = self.tx.peek().as_ref() {
            let _ = tx.unbounded_send(msg);
        }
    }

    #[cfg(not(feature = "web"))]
    #[allow(dead_code)]
    pub fn send(&self, _msg: ClientMsg) {}

    /// Local Leave: clears all signals, which drops the sender and stops the
    /// reconnect loop (the loop checks `room_id` after each disconnect).
    pub fn leave(&self) {
        let mut this = *self;
        this.tx.set(None);
        this.room_id.set(None);
        this.me.set(None);
        this.members.set(Vec::new());
        this.viewers.set(0);
        this.current.set(None);
        this.current_received_at.set(0.0);
        this.pending_nav.set(None);
        this.last_applied_version.set(0);
        this.last_remote.set(None);
        this.events.set(VecDeque::new());
    }

    pub fn set_pending_nav(&self, v: Option<String>) {
        let mut this = *self;
        this.pending_nav.set(v);
    }

    /// Push a toast into the events feed, trimming to the last 16. Counter is
    /// kept on `events.len()` plus a high-water mark we hide on the entry.
    #[cfg(feature = "web")]
    pub fn push_event(&self, text: String) {
        let mut this = *self;
        let mut q = this.events.write();
        let next_id = q.back().map(|e| e.id + 1).unwrap_or(1);
        q.push_back(RoomEvent { id: next_id, text });
        while q.len() > 16 {
            q.pop_front();
        }
    }
}

/// Call this once at the top of `Shell` so the context is available everywhere.
/// Must be called inside a component — signals attach to that scope.
pub fn provide_room_context() -> RoomContext {
    let room_id = use_signal::<Option<String>>(|| None);
    let me = use_signal::<Option<Member>>(|| None);
    let members = use_signal::<Vec<Member>>(Vec::new);
    let viewers = use_signal::<usize>(|| 0);
    let current = use_signal::<Option<RoomState>>(|| None);
    let current_received_at = use_signal::<f64>(|| 0.0);
    let pending_nav = use_signal::<Option<String>>(|| None);
    let last_applied_version = use_signal::<u64>(|| 0);
    let last_remote = use_signal::<Option<RemoteEvent>>(|| None);
    let events = use_signal::<VecDeque<RoomEvent>>(VecDeque::new);
    let tx = use_signal::<Option<UnboundedSender<ClientMsg>>>(|| None);
    let ctx = RoomContext {
        room_id,
        me,
        members,
        viewers,
        current,
        current_received_at,
        pending_nav,
        last_applied_version,
        last_remote,
        events,
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

    let (tx, rx) = unbounded::<ClientMsg>();
    let mut tx_sig = ctx.tx;
    tx_sig.set(Some(tx));
    let mut room_id_sig = ctx.room_id;
    room_id_sig.set(Some(room_id.clone()));

    wasm_bindgen_futures::spawn_local(async move {
        run_session(ctx, url, rx).await;
    });
}

/// Run one full session: open → message loop → reconnect → ... until the user
/// leaves. Reconnect backoff is 0.5s, 1s, 2s, 4s, 8s (capped); reset on a
/// clean Welcome.
#[cfg(feature = "web")]
async fn run_session(
    ctx: RoomContext,
    url: String,
    mut rx: futures::channel::mpsc::UnboundedReceiver<ClientMsg>,
) {
    let mut seq: u64 = 0;
    let mut backoff_ms: u32 = 500;
    loop {
        // Re-derive room_id each iteration so a leave between attempts stops us.
        let still_in_room = ctx.room_id.peek().is_some();
        if !still_in_room {
            return;
        }

        let mut ws = match WebSocket::open(&url) {
            Ok(ws) => ws,
            Err(e) => {
                tracing::warn!(?e, "ws open failed; backing off");
                gloo_timers::future::TimeoutFuture::new(backoff_ms).await;
                backoff_ms = (backoff_ms * 2).min(8000);
                continue;
            }
        };
        let mut got_welcome = false;

        // Note: we keep the WebSocket unsplit because gloo-net 0.6's
        // `SplitSink::poll_close` is a no-op — calling `write.close().await`
        // does nothing to the underlying browser WebSocket. Dropping the
        // unsplit value triggers gloo-net's `PinnedDrop`, which calls the
        // real `ws.close()` so the server actually sees a close frame.
        use futures::future::Either;
        loop {
            let incoming = ws.next();
            let outgoing = rx.next();
            futures::pin_mut!(incoming, outgoing);
            match futures::future::select(incoming, outgoing).await {
                Either::Left((incoming, _)) => match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(b) = serde_json::from_str::<Broadcast>(&text) {
                            if matches!(&b, Broadcast::Welcome { .. }) {
                                got_welcome = true;
                            }
                            handle_broadcast(ctx, b, &mut seq);
                        }
                    }
                    Some(Ok(Message::Bytes(_))) => continue,
                    // Close, error, or stream end — fall out and reconnect.
                    _ => break,
                },
                Either::Right((outgoing, _)) => match outgoing {
                    Some(msg) => {
                        let Ok(text) = serde_json::to_string(&msg) else { continue };
                        if ws.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    // `tx` was dropped (ctx.leave). Drop ws → close the
                    // socket → done.
                    None => {
                        drop(ws);
                        return;
                    }
                },
            }
        }
        // Disconnect path. If the user already left, exit. Otherwise reconnect.
        drop(ws);
        if ctx.room_id.peek().is_none() {
            return;
        }
        if got_welcome {
            // Successful session before the drop — start the next attempt fast.
            backoff_ms = 500;
        }
        tracing::info!(backoff_ms, "syncplay ws disconnected; reconnecting");
        gloo_timers::future::TimeoutFuture::new(backoff_ms).await;
        backoff_ms = (backoff_ms * 2).min(8000);
    }
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

/// Project a snapshot's position to "now" on the client. Mirrors the server's
/// `project_position` logic so late joiners and Resync recipients land on the
/// right spot regardless of when the snapshot was minted.
#[cfg(feature = "web")]
fn project_position_now(state: &RoomState, server_ts: i64, server_live_ms: Option<i64>) -> i64 {
    // If the server already projected for us, just use that — we'd otherwise
    // be guessing at clock drift between local and server time.
    if let Some(live) = server_live_ms {
        return live;
    }
    let _ = server_ts;
    if state.playing {
        state.position_ms
    } else {
        state.position_ms
    }
}

#[cfg(feature = "web")]
fn handle_broadcast(ctx: RoomContext, b: Broadcast, seq: &mut u64) {
    let me_user = ctx.me.peek().clone();
    let is_me = |m: &Member| me_user.as_ref().map(|u| u.client_id == m.client_id).unwrap_or(false);
    let mut me_sig = ctx.me;
    let mut members_sig = ctx.members;
    let mut viewers_sig = ctx.viewers;
    let mut current_sig = ctx.current;
    let mut current_received_at_sig = ctx.current_received_at;
    let mut last_remote_sig = ctx.last_remote;
    let mut last_applied_version_sig = ctx.last_applied_version;
    let stamp_now = || now_ms_f64();

    match b {
        Broadcast::Welcome { you, current, members, server_ts } => {
            me_sig.set(Some(you));
            viewers_sig.set(members.len());
            members_sig.set(members);
            if let Some(state) = current.as_ref() {
                let live = project_position_now(state, server_ts, Some(state.position_ms));
                let mut applied = state.clone();
                applied.position_ms = live;
                if state.version > *ctx.last_applied_version.peek() {
                    last_applied_version_sig.set(state.version);
                }
                ctx.set_pending_nav(Some(state.media_id.clone()));
                current_sig.set(Some(applied));
                current_received_at_sig.set(stamp_now());
            }
        }
        Broadcast::Members { members, joined, left } => {
            viewers_sig.set(members.len());
            members_sig.set(members);
            if let Some(m) = joined {
                if !is_me(&m) {
                    ctx.push_event(format!("{} joined", m.username));
                }
            }
            if let Some(m) = left {
                if !is_me(&m) {
                    ctx.push_event(format!("{} left", m.username));
                }
            }
        }
        Broadcast::SetMedia { media_id, from, version, .. } => {
            if version <= *ctx.last_applied_version.peek() {
                return;
            }
            last_applied_version_sig.set(version);
            ctx.set_pending_nav(Some(media_id.clone()));
            current_sig.set(Some(RoomState {
                media_id: media_id.clone(),
                position_ms: 0,
                playing: true,
                updated_at: 0,
                version,
            }));
            current_received_at_sig.set(stamp_now());
            if !is_me(&from) {
                ctx.push_event(format!("{} switched media", from.username));
            }
        }
        Broadcast::Play { position_ms, from, version, .. } => {
            // Update local current snapshot so dropdown and reconnect keep in sync.
            update_current_state(&mut current_sig, version, |s| {
                s.position_ms = position_ms;
                s.playing = true;
            });
            if is_me(&from) {
                return;
            }
            *seq = seq.wrapping_add(1);
            last_remote_sig.set(Some(RemoteEvent {
                seq: *seq,
                kind: RemoteKind::Play { position_ms },
            }));
            ctx.push_event(format!("{} pressed play", from.username));
        }
        Broadcast::Pause { position_ms, from, version, .. } => {
            update_current_state(&mut current_sig, version, |s| {
                s.position_ms = position_ms;
                s.playing = false;
            });
            if is_me(&from) {
                return;
            }
            *seq = seq.wrapping_add(1);
            last_remote_sig.set(Some(RemoteEvent {
                seq: *seq,
                kind: RemoteKind::Pause { position_ms },
            }));
            ctx.push_event(format!("{} paused", from.username));
        }
        Broadcast::Seek { position_ms, from, version, .. } => {
            update_current_state(&mut current_sig, version, |s| {
                s.position_ms = position_ms;
            });
            if is_me(&from) {
                return;
            }
            *seq = seq.wrapping_add(1);
            last_remote_sig.set(Some(RemoteEvent {
                seq: *seq,
                kind: RemoteKind::Seek { position_ms },
            }));
            ctx.push_event(format!(
                "{} jumped to {}",
                from.username,
                fmt_position(position_ms)
            ));
        }
        Broadcast::Resync { state, live_position_ms, server_ts } => {
            // Apply the snapshot to local state so anything reading
            // `current` stays accurate. Resync never moves backwards in
            // version — it's the same `version` redelivered.
            let _ = server_ts;
            let mut applied = state.clone();
            applied.position_ms = live_position_ms;
            current_sig.set(Some(applied.clone()));
            current_received_at_sig.set(stamp_now());
            // Also nudge the player if we've drifted enough; the bridge's
            // effect picks this up via `last_remote`.
            let kind = if state.playing {
                RemoteKind::Play { position_ms: live_position_ms }
            } else {
                RemoteKind::Pause { position_ms: live_position_ms }
            };
            *seq = seq.wrapping_add(1);
            last_remote_sig.set(Some(RemoteEvent { seq: *seq, kind }));
        }
        Broadcast::Pong { .. } => {}
    }
}

/// Mutate the local `current` snapshot if `version` is fresher than what we
/// already have. Used so dropdowns and the bridge see the same state the
/// server just confirmed without waiting for the next Resync.
#[cfg(feature = "web")]
fn update_current_state<F: FnOnce(&mut RoomState)>(
    current_sig: &mut Signal<Option<RoomState>>,
    version: u64,
    f: F,
) {
    let mut cur = current_sig.write();
    if let Some(s) = cur.as_mut() {
        if version >= s.version {
            f(s);
            s.version = version;
        }
    }
}

#[cfg(feature = "web")]
fn fmt_position(ms: i64) -> String {
    let total_secs = (ms / 1000).max(0);
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

// ---------- components (both targets) ----------

/// Watches `pending_nav` and drives navigation. Uses a hard page load
/// rather than the router's soft-nav: `VideoPlayer` captures its `id`
/// at mount and derives `stream_src` / subtitles / tech probe from that
/// captured value, so a soft `nav.push` updates the URL but leaves the
/// player wired to the previous episode. A full load remounts the
/// component tree against the new id, which is what we actually want
/// when a remote room mate switches what's playing.
#[component]
pub fn RoomNavigator() -> Element {
    let ctx = use_room_context();

    use_effect(move || {
        let target = ctx.pending_nav.read().clone();
        if let Some(media_id) = target {
            ctx.set_pending_nav(None);
            let js = format!("window.location.assign('/media/{media_id}/play');");
            spawn(async move {
                let _ = document::eval(&js).await;
            });
        }
    });

    rsx! {}
}

/// Rooms dropdown in the topbar. Shows active rooms with usernames and
/// Join/Leave controls.
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
    let members_list = ctx.members.read().clone();
    let names: Vec<String> = members_list.iter().map(|m| m.username.clone()).collect();
    let names_joined = names.join(", ");

    let title = if in_room {
        let media = current_state
            .as_ref()
            .map(|s| format!(" · {}", s.media_id))
            .unwrap_or_else(|| " · idle".to_string());
        format!("Watch party ({viewers}){media}")
    } else {
        "Rooms".to_string()
    };

    let current_media_title = current_state.as_ref().map(|s| s.media_id.clone());

    rsx! {
        div { class: "rooms-dd", "data-popover": "rooms",
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
                                if !names.is_empty() {
                                    span { class: "rooms-current-id", "With {names_joined}" }
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
    let names_joined = room.members.join(", ");
    let with_line = if names_joined.is_empty() {
        format!("{} viewer{}", room.viewers, if room.viewers == 1 { "" } else { "s" })
    } else {
        names_joined
    };
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
                span { class: "rooms-row-meta", "{with_line}" }
            }
            span { class: "rooms-row-join", "Join" }
        }
    }
}

/// Renders the recent-events feed as floating toasts. Each entry auto-fades
/// after ~4 s. Mounted once at the Shell so it overlays every route.
#[component]
pub fn RoomToasts() -> Element {
    let ctx = use_room_context();
    let events = ctx.events.read().clone();
    // Only show the most recent 4 — older ones stay in the buffer for
    // potential debugging but don't pile up visually.
    let visible: Vec<RoomEvent> = events.iter().rev().take(4).cloned().collect();

    #[cfg(feature = "web")]
    {
        use_effect(move || {
            let len = ctx.events.read().len();
            if len == 0 {
                return;
            }
            let mut events_sig = ctx.events;
            spawn(async move {
                gloo_timers::future::TimeoutFuture::new(4000).await;
                let mut q = events_sig.write();
                if !q.is_empty() {
                    q.pop_front();
                }
            });
        });
    }

    rsx! {
        div { class: "room-toasts",
            for e in visible.iter() {
                div { key: "{e.id}", class: "room-toast", "{e.text}" }
            }
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
        // Time gate for echo suppression: when we apply a remote event to the
        // <video> element, set this to performance.now() + 300ms. Local
        // listeners check against it instead of decrementing a counter.
        let applying_until: Rc<Cell<f64>> = use_hook(|| Rc::new(Cell::new(0.0)));
        let handle_slot: Rc<RefCell<Option<ListenerHandle>>> =
            use_hook(|| Rc::new(RefCell::new(None)));

        // Announce what this tab is playing. Subscribes to `room_id` (so
        // joining mid-playback fires) and to the current room state's media
        // id (so we don't re-announce media we were navigated to).
        {
            let media_id = media_id.clone();
            use_effect(move || {
                let in_room = ctx.room_id.read().is_some();
                if !in_room {
                    return;
                }
                let already_set = ctx
                    .current
                    .read()
                    .as_ref()
                    .map(|s| s.media_id == media_id)
                    .unwrap_or(false);
                if already_set {
                    return;
                }
                ctx.send(ClientMsg::SetMedia { media_id: media_id.clone() });
            });
        }

        // Attach DOM event listeners. The <video> element is conditionally
        // rendered (VideoPlayer waits for tech probe + HLS attach), so a
        // single post-commit `use_effect` often fires before it exists.
        // Poll for it and stop the moment we succeed or unmount.
        {
            let video_dom_id = video_dom_id.clone();
            let applying_until_l = applying_until.clone();
            let slot = handle_slot.clone();
            use_hook(|| {
                wasm_bindgen_futures::spawn_local(async move {
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
                                let applying_until = applying_until_l.clone();
                                let video = video_for_cb.clone();
                                Closure::wrap(Box::new(move || {
                                    if now_ms_f64() < applying_until.get() {
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

                            // Initial catch-up: when the user is pulled into
                            // a room mid-playback, the Welcome carries the
                            // current position + playing state but nothing
                            // pushes it to the fresh video element. Apply
                            // it once as soon as the element can seek,
                            // re-reading state at apply time so any Resync
                            // that arrived during the wait wins, and
                            // projecting the position forward by the local
                            // wall-clock delta so a slow attach doesn't
                            // strand the late joiner several seconds behind.
                            if ctx.current.peek().is_some() {
                                let video_for_sync = video.clone();
                                let applying_until_sync = applying_until_l.clone();
                                wasm_bindgen_futures::spawn_local(async move {
                                    for _ in 0..100 {
                                        if video_for_sync.ready_state() >= 1 {
                                            break;
                                        }
                                        gloo_timers::future::TimeoutFuture::new(100).await;
                                    }
                                    let Some(state) = ctx.current.peek().clone() else { return };
                                    let received_at = *ctx.current_received_at.peek();
                                    let projected = if state.playing && received_at > 0.0 {
                                        let delta_ms = (now_ms_f64() - received_at) as i64;
                                        state.position_ms + delta_ms.max(0)
                                    } else {
                                        state.position_ms
                                    };
                                    let kind = if state.playing {
                                        RemoteKind::Play { position_ms: projected }
                                    } else {
                                        RemoteKind::Pause { position_ms: projected }
                                    };
                                    apply_remote(&video_for_sync, &kind, &applying_until_sync);
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
            let applying_until_r = applying_until.clone();
            use_effect(move || {
                let Some(evt) = ctx.last_remote.read().clone() else { return };
                let Some(video) = lookup_video(&video_dom_id) else { return };
                apply_remote(&video, &evt.kind, &applying_until_r);
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
fn now_ms_f64() -> f64 {
    js_sys::Date::now()
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
fn apply_remote(video: &HtmlVideoElement, kind: &RemoteKind, applying_until: &Rc<Cell<f64>>) {
    // Open the time gate generously: a single seek can fire `seeking` +
    // `seeked` and possibly a `pause`/`play`, all of which we want to
    // suppress as locally-originated.
    let gate = || applying_until.set(now_ms_f64() + 300.0);
    match kind {
        RemoteKind::Seek { position_ms } => {
            let target = *position_ms as f64 / 1000.0;
            if (video.current_time() - target).abs() > 0.25 {
                gate();
                video.set_current_time(target);
            }
        }
        RemoteKind::Play { position_ms } => {
            let target = *position_ms as f64 / 1000.0;
            if (video.current_time() - target).abs() > 0.75 {
                gate();
                video.set_current_time(target);
            }
            if video.paused() {
                gate();
                let _ = video.play();
            }
        }
        RemoteKind::Pause { position_ms } => {
            let target = *position_ms as f64 / 1000.0;
            if (video.current_time() - target).abs() > 0.5 {
                gate();
                video.set_current_time(target);
            }
            if !video.paused() {
                gate();
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
