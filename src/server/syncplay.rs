use super::AppState;
use crate::types::{Broadcast, ClientMsg, RoomState};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
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
            tx,
        });
        self.rooms.insert(id, entry);
        meta
    }

    pub fn list_rooms(&self) -> Vec<(RoomMeta, Option<RoomState>, usize)> {
        self.rooms
            .iter()
            .map(|e| {
                let state = e.state.lock().ok().and_then(|g| g.clone());
                let viewers = e.tx.receiver_count();
                (e.meta.clone(), state, viewers)
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

async fn ws_handler(
    State(state): State<AppState>,
    Path(room): Path<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state.hub.clone(), room))
}

async fn handle_socket(mut socket: WebSocket, hub: Arc<Hub>, room: String) {
    let Some(entry) = hub.get(&room) else {
        // Unknown room: send a close frame and bail. Prevents silent ghost-room creation.
        let _ = socket.send(Message::Close(None)).await;
        return;
    };

    let client_id = Uuid::new_v4().to_string();
    let tx = entry.tx.clone();
    let mut rx = tx.subscribe();
    info!(%room, %client_id, "syncplay client joined");

    let (mut ws_sink, mut ws_stream) = socket.split();

    // Welcome direct to the new client — includes the current room snapshot so
    // late joiners can catch up without extra round trips.
    let current = entry.state.lock().ok().and_then(|g| g.clone());
    let welcome = Broadcast::Welcome {
        client_id: client_id.clone(),
        server_ts: now_ms(),
        current,
    };
    if let Ok(text) = serde_json::to_string(&welcome) {
        let _ = ws_sink.send(Message::Text(text.into())).await;
    }
    let viewers = tx.receiver_count();
    let _ = tx.send(Broadcast::Peer {
        client_id: client_id.clone(),
        joined: true,
        viewers,
    });

    // Task: forward room broadcasts to this client.
    let outbound_id = client_id.clone();
    let mut outbound = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            // Don't echo Pong to anyone but the original asker.
            if let Broadcast::Pong { .. } = &msg {
                continue;
            }
            // Don't echo your own Drift reports back to you.
            if let Broadcast::Drift { client_id, .. } = &msg {
                if client_id == &outbound_id {
                    continue;
                }
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
    let my_id = client_id.clone();
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

            let out = match parsed {
                ClientMsg::Play { position_ms } => {
                    update_state(&entry_in, |s| {
                        s.position_ms = position_ms;
                        s.playing = true;
                        s.updated_at = now_ms();
                    });
                    Broadcast::Play {
                        position_ms,
                        server_ts: now_ms(),
                        from: my_id.clone(),
                    }
                }
                ClientMsg::Pause { position_ms } => {
                    update_state(&entry_in, |s| {
                        s.position_ms = position_ms;
                        s.playing = false;
                        s.updated_at = now_ms();
                    });
                    Broadcast::Pause {
                        position_ms,
                        server_ts: now_ms(),
                        from: my_id.clone(),
                    }
                }
                ClientMsg::Seek { position_ms } => {
                    update_state(&entry_in, |s| {
                        s.position_ms = position_ms;
                        s.updated_at = now_ms();
                    });
                    Broadcast::Seek {
                        position_ms,
                        server_ts: now_ms(),
                        from: my_id.clone(),
                    }
                }
                ClientMsg::SetMedia { media_id } => {
                    if let Ok(mut g) = entry_in.state.lock() {
                        *g = Some(RoomState {
                            media_id: media_id.clone(),
                            position_ms: 0,
                            playing: true,
                            updated_at: now_ms(),
                        });
                    }
                    Broadcast::SetMedia {
                        media_id,
                        server_ts: now_ms(),
                        from: my_id.clone(),
                    }
                }
                ClientMsg::Ping { client_ts } => Broadcast::Pong {
                    client_ts,
                    server_ts: now_ms(),
                },
                ClientMsg::Heartbeat { position_ms, playing } => Broadcast::Drift {
                    client_id: my_id.clone(),
                    position_ms,
                    playing,
                    server_ts: now_ms(),
                },
            };
            let _ = tx_in.send(out);
        }
    });

    tokio::select! {
        _ = &mut outbound => { inbound.abort(); }
        _ = &mut inbound => { outbound.abort(); }
    }

    let viewers = tx.receiver_count().saturating_sub(1);
    let _ = tx.send(Broadcast::Peer {
        client_id: client_id.clone(),
        joined: false,
        viewers,
    });
    info!(%room, %client_id, "syncplay client left");
    drop(entry);
    hub.maybe_drop(&room);
}

/// Mutate the stored `RoomState` — only if there already is one (i.e. some client
/// has SetMedia'd). Play/Pause/Seek without an active media don't create state.
fn update_state<F: FnOnce(&mut RoomState)>(entry: &RoomEntry, f: F) {
    if let Ok(mut g) = entry.state.lock() {
        if let Some(s) = g.as_mut() {
            f(s);
        }
    }
}
