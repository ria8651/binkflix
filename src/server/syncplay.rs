use super::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tracing::{debug, info};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/syncplay/{room}", get(ws_handler))
}

#[derive(Default)]
pub struct Hub {
    rooms: DashMap<String, broadcast::Sender<Broadcast>>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn subscribe(&self, room: &str) -> (broadcast::Sender<Broadcast>, broadcast::Receiver<Broadcast>) {
        let sender = self
            .rooms
            .entry(room.to_string())
            .or_insert_with(|| broadcast::channel(128).0)
            .clone();
        let rx = sender.subscribe();
        (sender, rx)
    }

    fn maybe_drop(&self, room: &str) {
        if let Some(entry) = self.rooms.get(room) {
            if entry.receiver_count() == 0 {
                drop(entry);
                self.rooms.remove(room);
            }
        }
    }
}

/// Messages sent by clients.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    /// Client-authoritative intent: play at media-time `position_ms`.
    Play { position_ms: i64 },
    /// Pause at `position_ms`.
    Pause { position_ms: i64 },
    /// Seek to `position_ms` (stays in current play/pause state).
    Seek { position_ms: i64 },
    /// Periodic heartbeat for latency estimation. Server echoes back.
    Ping { client_ts: i64 },
    /// Report current playback position for drift detection.
    Heartbeat { position_ms: i64, playing: bool },
}

/// Messages fanned out to all clients in a room.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Broadcast {
    Welcome { client_id: String, server_ts: i64 },
    Peer { client_id: String, joined: bool },
    Play { position_ms: i64, server_ts: i64, from: String },
    Pause { position_ms: i64, server_ts: i64, from: String },
    Seek { position_ms: i64, server_ts: i64, from: String },
    Pong { client_ts: i64, server_ts: i64 },
    Drift { client_id: String, position_ms: i64, playing: bool, server_ts: i64 },
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

async fn handle_socket(socket: WebSocket, hub: Arc<Hub>, room: String) {
    let client_id = Uuid::new_v4().to_string();
    let (tx, mut rx) = hub.subscribe(&room);
    info!(%room, %client_id, "syncplay client joined");

    let (mut ws_sink, mut ws_stream) = socket.split();

    // Initial welcome direct to the new client.
    let welcome = Broadcast::Welcome { client_id: client_id.clone(), server_ts: now_ms() };
    if let Ok(text) = serde_json::to_string(&welcome) {
        let _ = ws_sink.send(Message::Text(text.into())).await;
    }
    let _ = tx.send(Broadcast::Peer { client_id: client_id.clone(), joined: true });

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
                ClientMsg::Play { position_ms } => Broadcast::Play {
                    position_ms,
                    server_ts: now_ms(),
                    from: my_id.clone(),
                },
                ClientMsg::Pause { position_ms } => Broadcast::Pause {
                    position_ms,
                    server_ts: now_ms(),
                    from: my_id.clone(),
                },
                ClientMsg::Seek { position_ms } => Broadcast::Seek {
                    position_ms,
                    server_ts: now_ms(),
                    from: my_id.clone(),
                },
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

    let _ = tx.send(Broadcast::Peer { client_id: client_id.clone(), joined: false });
    info!(%room, %client_id, "syncplay client left");
    hub.maybe_drop(&room);
}
