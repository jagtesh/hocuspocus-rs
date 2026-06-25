//! Hocuspocus-RS
//!
//! A Rust implementation of the Hocuspocus protocol (Yjs over WebSockets).
//! Provides a handler for Yjs documents that follows the Hocuspocus V2 protocol structure.

#[cfg(feature = "sqlite")]
pub mod db;
pub mod sync;

#[cfg(feature = "sqlite")]
pub use db::Database;
pub use sync::{
    DocHandler, HandlerError, NamedUpdateHook, RoomWriteGuard, TransactionPersistence, UpdateHook,
    MSG_AUTH, MSG_AWARENESS, MSG_QUERY_AWARENESS, MSG_SYNC,
};

#[cfg(feature = "server")]
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
    routing::get,
    Router,
};
#[cfg(feature = "server")]
use dashmap::DashMap;
#[cfg(feature = "server")]
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
#[cfg(feature = "server")]
use std::sync::Arc;

/// Application state shared across WebSocket connections
#[cfg(feature = "server")]
pub struct AppState {
    pub rooms: DashMap<String, Arc<DocHandler>>,
    #[cfg(feature = "sqlite")]
    pub db: Database,
    immediate_update_hook: Option<NamedUpdateHook>,
    debounced_update_hook: Option<NamedUpdateHook>,
}

#[cfg(feature = "server")]
impl AppState {
    #[cfg(feature = "sqlite")]
    pub fn new(db: Database) -> Self {
        Self::new_with_update_hook(db, None)
    }

    #[cfg(feature = "sqlite")]
    pub fn new_with_update_hook(db: Database, update_hook: Option<UpdateHook>) -> Self {
        Self::new_with_named_update_hook(db, update_hook.map(sync::named_update_hook))
    }

    #[cfg(feature = "sqlite")]
    pub fn new_with_named_update_hook(db: Database, update_hook: Option<NamedUpdateHook>) -> Self {
        Self::new_with_named_hooks(db, None, update_hook)
    }

    #[cfg(feature = "sqlite")]
    pub fn new_with_named_hooks(
        db: Database,
        immediate_update_hook: Option<NamedUpdateHook>,
        debounced_update_hook: Option<NamedUpdateHook>,
    ) -> Self {
        Self {
            rooms: DashMap::new(),
            db,
            immediate_update_hook,
            debounced_update_hook,
        }
    }

    #[cfg(not(feature = "sqlite"))]
    pub fn new() -> Self {
        Self::new_with_update_hook(None)
    }

    #[cfg(not(feature = "sqlite"))]
    pub fn new_with_update_hook(update_hook: Option<UpdateHook>) -> Self {
        Self::new_with_named_update_hook(update_hook.map(sync::named_update_hook))
    }

    #[cfg(not(feature = "sqlite"))]
    pub fn new_with_named_update_hook(update_hook: Option<NamedUpdateHook>) -> Self {
        Self::new_with_named_hooks(None, update_hook)
    }

    #[cfg(not(feature = "sqlite"))]
    pub fn new_with_named_hooks(
        immediate_update_hook: Option<NamedUpdateHook>,
        debounced_update_hook: Option<NamedUpdateHook>,
    ) -> Self {
        Self {
            rooms: DashMap::new(),
            immediate_update_hook,
            debounced_update_hook,
        }
    }

    /// Get or create a document handler for a room
    pub fn get_or_create_handler(&self, room_name: &str) -> Arc<DocHandler> {
        self.rooms
            .entry(room_name.to_string())
            .or_insert_with(|| {
                let name = room_name.to_string();
                #[cfg(feature = "sqlite")]
                    {
                        let db = self.db.clone();
                        let immediate_update_hook = self.immediate_update_hook.clone();
                        let debounced_update_hook = self.debounced_update_hook.clone();
                        tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current().block_on(async {
                                Arc::new(
                                    DocHandler::new_with_named_hooks(
                                        name,
                                        db,
                                        immediate_update_hook,
                                        debounced_update_hook,
                                    )
                                    .await,
                                )
                            })
                        })
                    }
                    #[cfg(not(feature = "sqlite"))]
                    {
                        let immediate_update_hook = self.immediate_update_hook.clone();
                        let debounced_update_hook = self.debounced_update_hook.clone();
                        tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current().block_on(async {
                                Arc::new(
                                    DocHandler::new_with_named_hooks(
                                        name,
                                        immediate_update_hook,
                                        debounced_update_hook,
                                    )
                                    .await,
                                )
                            })
                        })
                    }
            })
            .clone()
    }
}

/// Create the sync router (for embedding in other servers)
#[cfg(feature = "server")]
pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/sync/:room_name", get(ws_handler))
        .route("/sync", get(ws_handler_generic))
        .with_state(state)
}

// WebSocket handlers
#[cfg(feature = "server")]
async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::Path(room_name): axum::extract::Path<String>,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket_with_room(socket, state, room_name))
}

#[cfg(feature = "server")]
async fn ws_handler_generic(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| handle_socket_generic(socket, state))
}

#[cfg(feature = "server")]
async fn handle_socket_with_room(socket: WebSocket, state: Arc<AppState>, room_name: String) {
    let handler = state.get_or_create_handler(&room_name);
    let (sender, receiver) = socket.split();
    run_connection(sender, receiver, handler, room_name, None).await;
}

#[cfg(feature = "server")]
async fn handle_socket_generic(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    // Wait for first message to determine room name
    let first_msg = match receiver.next().await {
        Some(Ok(Message::Binary(data))) => data,
        _ => return,
    };

    let (_, room_name) = match DocHandler::read_and_skip_doc_name(&first_msg) {
        Some(res) => res,
        None => return,
    };

    let handler = state.get_or_create_handler(&room_name);

    // Process initial message
    let responses = handler.handle_message(&first_msg).await;
    for resp in &responses {
        if sender.send(Message::Binary(resp.clone())).await.is_err() {
            return;
        }
    }

    run_connection(
        sender,
        receiver,
        handler,
        room_name,
        Some(first_msg.to_vec()),
    )
    .await;
}

#[cfg(feature = "server")]
pub async fn run_connection(
    mut ws_sender: SplitSink<WebSocket, Message>,
    mut ws_receiver: SplitStream<WebSocket>,
    handler: Arc<DocHandler>,
    room_name: String,
    _initial_message: Option<Vec<u8>>,
) {
    // Subscribe before sending initial sync so broadcasts during connection
    // setup are queued and delivered after the initial messages.
    let (mut broadcast_rx, initial_msgs) = handler.subscribe_with_initial_sync();
    for msg in initial_msgs {
        if ws_sender.send(Message::Binary(msg)).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        let responses = handler.handle_message(&data).await;
                        for resp in responses {
                            if ws_sender.send(Message::Binary(resp)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_sender.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::debug!("Client disconnected from room '{}'", room_name);
                        return;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("WebSocket error in room '{}': {:?}", room_name, e);
                        return;
                    }
                    _ => {}
                }
            }
            msg = broadcast_rx.recv() => {
                if let Ok(data) = msg {
                    if ws_sender.send(Message::Binary(data)).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}
