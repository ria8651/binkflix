use super::analytics;
use super::auth::Session;
use super::AppState;
use crate::types::{Broadcast, ClientMsg, Member, RoomState};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Extension, Router};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tracing::{debug, info};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/syncplay/{room}", get(ws_handler))
}

/// Metadata stored alongside each room. Purely informational — state transitions
/// are driven by the broadcast channel, this is just what we report to HTTP callers.
#[derive(Debug, Clone)]
pub struct RoomMeta {
    pub id: String,
    pub created_at: i64,
}

struct RoomEntry {
    meta: RoomMeta,
    state: Mutex<Option<RoomState>>,
    members: Mutex<Vec<Member>>,
    tx: broadcast::Sender<Broadcast>,
}

#[derive(Default)]
pub struct Hub {
    rooms: DashMap<String, Arc<RoomEntry>>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Create a new empty room. Sweeps entries that have been empty for >60s first
    /// so creators who never connect don't leak ghost rooms.
    pub fn create_room(&self) -> RoomMeta {
        self.sweep_stale();
        let id = Uuid::new_v4().to_string();
        let meta = RoomMeta { id: id.clone(), created_at: now_ms() };
        let (tx, _) = broadcast::channel(128);
        let entry = Arc::new(RoomEntry {
            meta: meta.clone(),
            state: Mutex::new(None),
            members: Mutex::new(Vec::new()),
            tx,
        });
        self.rooms.insert(id.clone(), entry.clone());
        spawn_resync_task(id, entry);
        meta
    }

    pub fn list_rooms(&self) -> Vec<(RoomMeta, Option<RoomState>, usize, Vec<String>)> {
        self.rooms
            .iter()
            .map(|e| {
                let state = e.state.lock().ok().and_then(|g| g.clone());
                let viewers = e.tx.receiver_count();
                let members = e
                    .members
                    .lock()
                    .ok()
                    .map(|g| g.iter().map(|m| m.username.clone()).collect())
                    .unwrap_or_default();
                (e.meta.clone(), state, viewers, members)
            })
            .collect()
    }

    fn get(&self, room: &str) -> Option<Arc<RoomEntry>> {
        self.rooms.get(room).map(|r| r.clone())
    }

    fn maybe_drop(&self, room: &str) {
        if let Some(entry) = self.rooms.get(room) {
            if entry.tx.receiver_count() == 0 {
                // Only drop if also stale — lets a creator get a small grace window
                // between create_room and the first socket opening.
                let stale = now_ms() - entry.meta.created_at > 60_000;
                let has_state = entry.state.lock().ok().map(|g| g.is_some()).unwrap_or(false);
                drop(entry);
                if stale || has_state {
                    self.rooms.remove(room);
                }
            }
        }
    }

    fn sweep_stale(&self) {
        let now = now_ms();
        self.rooms.retain(|_, e| {
            let empty = e.tx.receiver_count() == 0;
            let old = now - e.meta.created_at > 60_000;
            !(empty && old)
        });
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Project the live playback position from a snapshot. While playing, advance
/// by wall-clock time since `updated_at`; while paused, the snapshot is exact.
fn project_position(state: &RoomState, now: i64) -> i64 {
    if state.playing {
        let elapsed = (now - state.updated_at).max(0);
        state.position_ms + elapsed
    } else {
        state.position_ms
    }
}

/// One per room. Wakes every 5s; if there are at least two viewers and a
/// state, sends a Resync. Self-terminates when the room is empty for two
/// consecutive ticks (the room's broadcast Sender is held by the entry, so
/// `receiver_count() == 0` reliably signals "everyone left").
fn spawn_resync_task(_room_id: String, entry: Arc<RoomEntry>) {
    tokio::spawn(async move {
        let mut idle_ticks: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if Arc::strong_count(&entry) <= 1 {
                // Hub dropped its reference; nothing else holds the entry.
                return;
            }
            let viewers = entry.tx.receiver_count();
            if viewers == 0 {
                idle_ticks += 1;
                if idle_ticks >= 2 {
                    return;
                }
                continue;
            }
            idle_ticks = 0;
            if viewers < 2 {
                continue;
            }
            let state = entry.state.lock().ok().and_then(|g| g.clone());
            if let Some(state) = state {
                let now = now_ms();
                let live_position_ms = project_position(&state, now);
                let _ = entry.tx.send(Broadcast::Resync {
                    state,
                    live_position_ms,
                    server_ts: now,
                });
            }
        }
    });
}

async fn ws_handler(
    State(state): State<AppState>,
    Path(room): Path<String>,
    Extension(session): Extension<Session>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let hub = state.hub.clone();
    let pool = state.pool.clone();
    ws.on_upgrade(move |socket| handle_socket(socket, hub, pool, room, session))
}

/// Pull the current media_id (if any) out of a room entry — best effort,
/// returns None if the lock is poisoned or no media is set yet.
fn current_media_id(entry: &RoomEntry) -> Option<String> {
    entry
        .state
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.media_id.clone()))
}

async fn handle_socket(
    mut socket: WebSocket,
    hub: Arc<Hub>,
    pool: SqlitePool,
    room: String,
    session: Session,
) {
    let Some(entry) = hub.get(&room) else {
        // Unknown room: send a close frame and bail. Prevents silent ghost-room creation.
        let _ = socket.send(Message::Close(None)).await;
        return;
    };

    let client_id = Uuid::new_v4().to_string();
    let me = Member {
        client_id: client_id.clone(),
        user_sub: session.user_sub.clone(),
        username: session.login.clone(),
    };
    let tx = entry.tx.clone();
    let mut rx = tx.subscribe();
    info!(%room, %client_id, username = %me.username, "syncplay client joined");

    // Add to roster *after* we've subscribed, so the join broadcast reaches us
    // too (clients use it to discover their own membership consistently).
    let members_snapshot = {
        let mut g = entry.members.lock().expect("members lock");
        g.push(me.clone());
        g.clone()
    };

    let (mut ws_sink, mut ws_stream) = socket.split();

    // Welcome direct to the new client — includes the live-projected room
    // snapshot and full roster so late joiners can catch up without extra
    // round trips.
    let current = entry
        .state
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .map(|mut s| {
            s.position_ms = project_position(&s, now_ms());
            s.updated_at = now_ms();
            s
        });
    let welcome = Broadcast::Welcome {
        you: me.clone(),
        server_ts: now_ms(),
        current,
        members: members_snapshot.clone(),
    };
    if let Ok(text) = serde_json::to_string(&welcome) {
        let _ = ws_sink.send(Message::Text(text.into())).await;
    }
    let _ = tx.send(Broadcast::Members {
        members: members_snapshot,
        joined: Some(me.clone()),
        left: None,
    });

    analytics::record_event(
        &pool,
        "room.join",
        Some(&session.user_sub),
        current_media_id(&entry).as_deref(),
        Some(&room),
        &json!({ "client_id": client_id }),
    )
    .await;

    // Task: forward room broadcasts to this client.
    let outbound_id = client_id.clone();
    let mut outbound = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            // Don't echo Pong to anyone — the original asker already got a
            // direct reply. (Server doesn't currently send Pong over the
            // broadcast channel, but be defensive.)
            if matches!(&msg, Broadcast::Pong { .. }) {
                continue;
            }
            // Skip echoes of our own playback events: the local client already
            // applied them via the DOM listener. SetMedia is *not* filtered —
            // the sender wants the confirmed `version` so its
            // `last_applied_version` stays in sync with everyone else's.
            let echo_of_self = match &msg {
                Broadcast::Play { from, .. }
                | Broadcast::Pause { from, .. }
                | Broadcast::Seek { from, .. } => from.client_id == outbound_id,
                _ => false,
            };
            if echo_of_self {
                continue;
            }
            let Ok(text) = serde_json::to_string(&msg) else { continue };
            if ws_sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Task: read messages from this client, translate, publish.
    let entry_in = entry.clone();
    let tx_in = tx.clone();
    let me_in = me.clone();
    let pool_in = pool.clone();
    let room_in = room.clone();
    let user_sub_in = session.user_sub.clone();
    let mut inbound = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_stream.next().await {
            let text = match msg {
                Message::Text(t) => t,
                Message::Close(_) => break,
                _ => continue,
            };
            let Ok(parsed) = serde_json::from_str::<ClientMsg>(&text) else {
                debug!(%text, "ignoring malformed syncplay message");
                continue;
            };

            // (kind, media_id, data) for the analytics event, or None to skip
            // (Ping/Heartbeat aren't user-meaningful actions).
            let mut event: Option<(&'static str, Option<String>, serde_json::Value)> = None;

            let out = match parsed {
                ClientMsg::Play { position_ms } => {
                    let version = update_state(&entry_in, |s| {
                        s.position_ms = position_ms;
                        s.playing = true;
                        s.updated_at = now_ms();
                    });
                    let Some(version) = version else { continue };
                    event = Some((
                        "room.play",
                        current_media_id(&entry_in),
                        json!({ "position_ms": position_ms, "version": version }),
                    ));
                    Broadcast::Play {
                        position_ms,
                        server_ts: now_ms(),
                        from: me_in.clone(),
                        version,
                    }
                }
                ClientMsg::Pause { position_ms } => {
                    let version = update_state(&entry_in, |s| {
                        s.position_ms = position_ms;
                        s.playing = false;
                        s.updated_at = now_ms();
                    });
                    let Some(version) = version else { continue };
                    event = Some((
                        "room.pause",
                        current_media_id(&entry_in),
                        json!({ "position_ms": position_ms, "version": version }),
                    ));
                    Broadcast::Pause {
                        position_ms,
                        server_ts: now_ms(),
                        from: me_in.clone(),
                        version,
                    }
                }
                ClientMsg::Seek { position_ms } => {
                    let version = update_state(&entry_in, |s| {
                        s.position_ms = position_ms;
                        s.updated_at = now_ms();
                    });
                    let Some(version) = version else { continue };
                    event = Some((
                        "room.seek",
                        current_media_id(&entry_in),
                        json!({ "position_ms": position_ms, "version": version }),
                    ));
                    Broadcast::Seek {
                        position_ms,
                        server_ts: now_ms(),
                        from: me_in.clone(),
                        version,
                    }
                }
                ClientMsg::SetMedia { media_id } => {
                    let version = {
                        let mut g = entry_in.state.lock().expect("state lock");
                        let next = g.as_ref().map(|s| s.version + 1).unwrap_or(1);
                        *g = Some(RoomState {
                            media_id: media_id.clone(),
                            position_ms: 0,
                            playing: true,
                            updated_at: now_ms(),
                            version: next,
                        });
                        next
                    };
                    event = Some((
                        "room.set_media",
                        Some(media_id.clone()),
                        json!({ "version": version }),
                    ));
                    Broadcast::SetMedia {
                        media_id,
                        server_ts: now_ms(),
                        from: me_in.clone(),
                        version,
                    }
                }
                ClientMsg::Ping { client_ts } => Broadcast::Pong {
                    client_ts,
                    server_ts: now_ms(),
                },
                ClientMsg::Heartbeat { .. } => {
                    // Heartbeats no longer need to fan out — periodic Resync
                    // covers drift correction for the whole room.
                    continue;
                }
            };
            if let Some((kind, media_id, data)) = event {
                analytics::record_event(
                    &pool_in,
                    kind,
                    Some(&user_sub_in),
                    media_id.as_deref(),
                    Some(&room_in),
                    &data,
                )
                .await;
            }
            let _ = tx_in.send(out);
        }
    });

    tokio::select! {
        _ = &mut outbound => { inbound.abort(); }
        _ = &mut inbound => { outbound.abort(); }
    }

    // Awaiting the aborted task to force its rx drop sounds cleaner but
    // can deadlock: if the outbound task was parked inside `ws_sink.send`
    // against a half-closed socket, `abort()` doesn't always force an
    // immediate drop, and the await never returns. That leaves
    // `handle_socket` stuck and rx subscribed, so `receiver_count` stays
    // at N and /api/rooms reports ghost viewers forever.
    //
    // Instead: remove ourselves from the roster, broadcast the new
    // member list, and return. The aborted task's future is dropped at
    // the next scheduler poll; receiver_count catches up a tick later.
    let members_after = {
        let mut g = entry.members.lock().expect("members lock");
        g.retain(|m| m.client_id != client_id);
        g.clone()
    };
    let _ = tx.send(Broadcast::Members {
        members: members_after,
        joined: None,
        left: Some(me.clone()),
    });
    analytics::record_event(
        &pool,
        "room.leave",
        Some(&session.user_sub),
        current_media_id(&entry).as_deref(),
        Some(&room),
        &json!({ "client_id": client_id }),
    )
    .await;
    info!(%room, %client_id, username = %me.username, "syncplay client left");
    drop(entry);
    hub.maybe_drop(&room);
}

/// Mutate the stored `RoomState` and bump `version`. Returns the new version,
/// or `None` if no state existed (Play/Pause/Seek without an active media are
/// ignored — the client should SetMedia first).
fn update_state<F: FnOnce(&mut RoomState)>(entry: &RoomEntry, f: F) -> Option<u64> {
    let mut g = entry.state.lock().ok()?;
    let s = g.as_mut()?;
    f(s);
    s.version += 1;
    Some(s.version)
}
