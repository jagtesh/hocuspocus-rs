//! Yjs Sync Protocol Implementation
//!
//! This module implements the y-websocket sync protocol for compatibility with
//! Hocuspocus/y-websocket clients. The protocol flow is:
//!
//! 1. Client connects and sends SyncStep1(client_state_vector)
//! 2. Server responds with SyncStep2(server_diff) + SyncStep1(server_state_vector)
//! 3. Client sends SyncStep2(client_diff)
//! 4. Ongoing: Both sides exchange Update messages
//!
//! Note: Awareness protocol (for cursors/presence) is handled by forwarding
//! messages between clients without server-side state.

#[cfg(feature = "sqlite")]
use std::sync::Arc;
use std::sync::{Arc as StdArc, Mutex as StdMutex};
use tokio::sync::{broadcast, Mutex as TokioMutex};
use tokio::time::{Duration, Instant};
use yrs::encoding::read::Read;
use yrs::encoding::write::Write;
use yrs::updates::decoder::{Decode, DecoderV1};
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{Doc, ReadTxn, StateVector, Transact, TransactionMut, Update};

#[cfg(feature = "sqlite")]
use crate::db::Database;

/// Message types for y-websocket protocol
/// The first byte of each message indicates its type
pub const MSG_SYNC: u8 = 0;
pub const MSG_AWARENESS: u8 = 1;
pub const MSG_AUTH: u8 = 2; // Hocuspocus-specific
pub const MSG_QUERY_AWARENESS: u8 = 3;
pub const MSG_SYNC_STATUS: u8 = 8;

/// Debounce interval for persistence (milliseconds)
#[cfg(feature = "sqlite")]
const PERSIST_DEBOUNCE_MS: u64 = 500;

/// Debounce interval for application repair hooks (milliseconds)
const UPDATE_HOOK_DEBOUNCE_MS: u64 = 250;

/// A synchronous hook that can repair application-specific invariants after an
/// incoming Yjs update has been applied.
///
/// The hook receives a mutable transaction and should return the number of
/// repairs it made. Any non-zero return value is encoded as an update,
/// broadcast to peers, and included in persistence.
pub type UpdateHook = StdArc<dyn Fn(&mut TransactionMut) -> usize + Send + Sync>;

#[derive(Default)]
struct UpdateHookSchedulerState {
    running: bool,
    dirty: bool,
    coalesced_updates: u64,
}

/// A handler for a single Yjs document/room
/// Manages the document state, persistence, and broadcasting
///
/// Note: We use std::sync::Mutex for the Doc because yrs::Doc operations are
/// synchronous and fast. tokio::sync::Mutex would cause unnecessary overhead.
pub struct DocHandler {
    pub doc_name: String,
    /// Thread-safe document access using std Mutex (Doc ops are sync & fast)
    doc: StdArc<StdMutex<Doc>>,
    /// Database for persistence
    #[cfg(feature = "sqlite")]
    db: Database,
    /// Broadcast channel for sending updates to other clients
    pub broadcast_tx: broadcast::Sender<Vec<u8>>,
    /// Optional application-level invariant repair hook.
    update_hook: Option<UpdateHook>,
    /// Coalesces repair hook work so incremental content updates stay cheap.
    update_hook_scheduler: StdArc<TokioMutex<UpdateHookSchedulerState>>,
    /// Track when persistence was last requested for debouncing
    #[cfg(feature = "sqlite")]
    last_persist_request: Arc<TokioMutex<Option<Instant>>>,
    /// Flag to indicate persistence is pending
    #[cfg(feature = "sqlite")]
    persist_pending: Arc<TokioMutex<bool>>,
}

// Explicitly mark DocHandler as Send + Sync since we use std::sync::Mutex
// and all fields are thread-safe
unsafe impl Send for DocHandler {}
unsafe impl Sync for DocHandler {}

impl DocHandler {
    #[cfg(feature = "sqlite")]
    pub async fn new(doc_name: String, db: Database) -> Self {
        Self::new_with_update_hook(doc_name, db, None).await
    }

    #[cfg(feature = "sqlite")]
    pub async fn new_with_update_hook(
        doc_name: String,
        db: Database,
        update_hook: Option<UpdateHook>,
    ) -> Self {
        let doc = Doc::new();
        let (broadcast_tx, _) = broadcast::channel(256);

        // Load existing state from DB
        tracing::info!("Loading document '{}' from database...", doc_name);
        if let Ok(Some(data)) = db.get_doc(&doc_name).await {
            tracing::info!(
                "Found existing data for '{}': {} bytes",
                doc_name,
                data.len()
            );
            let mut txn = doc.transact_mut();
            match Update::decode_v1(&data) {
                Ok(update) => {
                    txn.apply_update(update);
                    tracing::debug!("Applied stored state to document '{}'", doc_name);
                }
                Err(e) => {
                    tracing::error!("Failed to decode stored state for '{}': {:?}", doc_name, e);
                }
            }
        } else {
            tracing::info!("No existing data found for '{}', starting fresh", doc_name);
        }

        Self {
            doc_name,
            doc: StdArc::new(StdMutex::new(doc)),
            db,
            broadcast_tx,
            update_hook,
            update_hook_scheduler: StdArc::new(
                TokioMutex::new(UpdateHookSchedulerState::default()),
            ),
            last_persist_request: Arc::new(TokioMutex::new(None)),
            persist_pending: Arc::new(TokioMutex::new(false)),
        }
    }

    #[cfg(not(feature = "sqlite"))]
    pub async fn new(doc_name: String) -> Self {
        Self::new_with_update_hook(doc_name, None).await
    }

    #[cfg(not(feature = "sqlite"))]
    pub async fn new_with_update_hook(doc_name: String, update_hook: Option<UpdateHook>) -> Self {
        let doc = Doc::new();
        let (broadcast_tx, _) = broadcast::channel(256);

        Self {
            doc_name,
            doc: StdArc::new(StdMutex::new(doc)),
            broadcast_tx,
            update_hook,
            update_hook_scheduler: StdArc::new(
                TokioMutex::new(UpdateHookSchedulerState::default()),
            ),
        }
    }

    /// Generate the initial sync messages to send when a client connects
    /// Returns: [SyncStep1(server_state_vector)]
    pub fn generate_initial_sync(&self) -> Vec<Vec<u8>> {
        let doc = self.doc.lock().unwrap_or_else(|e| {
            tracing::warn!("Doc mutex was poisoned for '{}', recovering", self.doc_name);
            e.into_inner()
        });
        let txn = doc.transact();
        let state_vector = txn.state_vector();

        // Encode SyncStep1: [Tag 0] [Len] [SV]
        let mut encoder = EncoderV1::new();
        encoder.write_var(0u32); // Tag SyncStep1

        // Encode SV to bytes first
        let mut sv_encoder = EncoderV1::new();
        state_vector.encode(&mut sv_encoder);
        let sv_bytes = sv_encoder.to_vec();

        // Write as buffer (Length + Bytes)
        encoder.write_buf(&sv_bytes);

        let payload = encoder.to_vec();

        let encoded = self.encode_hocuspocus_message(MSG_SYNC, &payload);

        tracing::debug!(
            "Generated initial sync message ({} bytes): {:02x?}",
            encoded.len(),
            encoded
        );

        vec![encoded]
    }

    /// Process an incoming message from a client
    /// Returns a list of response messages to send back to this client
    /// Also broadcasts updates to other clients via the broadcast channel
    pub async fn handle_message(&self, msg_data: &[u8]) -> Vec<Vec<u8>> {
        let mut responses = Vec::new();

        if msg_data.is_empty() {
            return responses;
        }

        tracing::trace!("Received message ({} bytes)", msg_data.len());

        // Hocuspocus Protocol V2: [DocName (VarString)] [MessageType (VarUint)] [Payload]
        // We first need to skip the document name since we already know context from the room connection
        let (content_data, _doc_name) = match DocHandler::read_and_skip_doc_name(msg_data) {
            Some(res) => res,
            None => {
                tracing::warn!(
                    "Failed to parse document name from message: {:02x?}",
                    msg_data
                );
                return responses;
            }
        };

        if content_data.is_empty() {
            return responses;
        }

        let msg_type = content_data[0];
        let payload = &content_data[1..];

        match msg_type {
            MSG_SYNC => {
                self.handle_sync_message(payload, &mut responses).await;
            }
            MSG_AWARENESS => {
                // Awareness messages are forwarded to other clients
                // We re-wrap them with our doc_name to ensure clients route them correctly
                self.forward_awareness_message(payload);
            }
            MSG_QUERY_AWARENESS => {
                // Query awareness - we don't maintain server-side awareness state
                // Clients will receive awareness updates from other clients directly
                tracing::debug!("Received QUERY_AWARENESS (no server state maintained)");
            }
            MSG_AUTH => {
                // Auth messages are handled at the WebSocket layer
                // For now, we accept all connections
                tracing::debug!("Received AUTH message (accepted)");
            }
            _ => {
                tracing::warn!("Unknown message type: {}", msg_type);
            }
        }

        responses
    }

    /// Helper to read the VarString document name and return the rest of the buffer
    pub fn read_and_skip_doc_name(data: &[u8]) -> Option<(&[u8], String)> {
        let mut offset = 0;
        let mut len: usize = 0;
        let mut shift = 0;

        // Decode VarUint length
        loop {
            if offset >= data.len() {
                return None;
            }
            let b = data[offset];
            offset += 1;
            len |= ((b & 0x7F) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 64 {
                return None;
            }
        }

        if offset + len > data.len() {
            return None;
        }

        // Decode string for debug/verification (optional but good for logging)
        let name_bytes = &data[offset..offset + len];
        let name = String::from_utf8_lossy(name_bytes).to_string();

        Some((&data[offset + len..], name))
    }

    /// Wraps a raw payload in the Hocuspocus V2 protocol structure:
    /// [DocName : VarString] [MsgType : VarUint] [Payload : Bytes]
    pub fn encode_hocuspocus_message(&self, msg_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut encoder = EncoderV1::new();
        encoder.write_string(&self.doc_name);
        encoder.write_var(msg_type as u32);

        let mut encoded = encoder.to_vec();
        encoded.extend_from_slice(payload);
        encoded
    }

    /// Forward awareness message to other clients
    fn forward_awareness_message(&self, payload: &[u8]) {
        // Re-wrap the awareness message with the Hocuspocus protocol V2 prefix
        let broadcast_msg = self.encode_hocuspocus_message(MSG_AWARENESS, payload);
        let _ = self.broadcast_tx.send(broadcast_msg);
        tracing::trace!("Forwarded awareness message for '{}'", self.doc_name);
    }

    /// Handle sync protocol messages (SyncStep1, SyncStep2, Update)
    async fn handle_sync_message(&self, payload: &[u8], responses: &mut Vec<Vec<u8>>) {
        let mut decoder = DecoderV1::from(payload);

        // Loop over the payload to decode multiple messages (Hocuspocus/y-protocols stream)
        // We check loop by trying to read the next tag
        while let Ok(tag) = decoder.read_var::<u32>() {
            match tag {
                0 => {
                    // SyncStep1: [Tag 0] [Len] [StateVector]
                    // First read the length-prefixed buffer
                    match decoder.read_buf() {
                        Ok(sv_data) => {
                            // Then decode SV from the buffer
                            match StateVector::decode(&mut DecoderV1::from(sv_data)) {
                                Ok(client_sv) => {
                                    tracing::debug!(
                                        "Handling SyncStep1 (SV len: {})",
                                        client_sv.len()
                                    );
                                    let doc = self.doc.lock().unwrap_or_else(|e| {
                                        tracing::warn!(
                                            "Doc mutex was poisoned for '{}', recovering",
                                            self.doc_name
                                        );
                                        e.into_inner()
                                    });
                                    let txn = doc.transact();

                                    // Reply with SyncStep2 (updates client needs)
                                    // [Tag 1] [Len] [Bytes]
                                    let update = txn.encode_state_as_update_v1(&client_sv);
                                    tracing::debug!(
                                        doc = %self.doc_name,
                                        update_bytes = update.len(),
                                        client_state_vector_len = client_sv.len(),
                                        "encoded SyncStep2 response"
                                    );
                                    let mut encoder = EncoderV1::new();
                                    encoder.write_var(1u32);
                                    encoder.write_buf(&update);
                                    responses.push(
                                        self.encode_hocuspocus_message(MSG_SYNC, &encoder.to_vec()),
                                    );

                                    // Also send our SyncStep1 (server SV) so client can sync vs us
                                    // [Tag 0] [Len] [StateVector]
                                    let server_sv = txn.state_vector();

                                    let mut sv_encoder = EncoderV1::new();
                                    server_sv.encode(&mut sv_encoder);
                                    let sv_bytes = sv_encoder.to_vec();

                                    let mut encoder_sv = EncoderV1::new();
                                    encoder_sv.write_var(0u32);
                                    encoder_sv.write_buf(&sv_bytes);

                                    responses.push(
                                        self.encode_hocuspocus_message(
                                            MSG_SYNC,
                                            &encoder_sv.to_vec(),
                                        ),
                                    );

                                    tracing::debug!(
                                        "Processed SyncStep1 for '{}', sent SyncStep2 + SyncStep1",
                                        self.doc_name
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Failed to decode StateVector in SyncStep1: {:?}",
                                        e
                                    );
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to read SyncStep1 payload: {:?}", e);
                            break;
                        }
                    }
                }
                1 => {
                    // SyncStep2: [Tag 1] [Len] [Bytes]
                    match decoder.read_buf() {
                        Ok(update_data) => {
                            tracing::debug!(
                                doc = %self.doc_name,
                                update_bytes = update_data.len(),
                                "handling SyncStep2 update"
                            );
                            if update_data.is_empty() {
                                tracing::debug!("Received empty SyncStep2 update, ignoring");
                                responses.push(self.encode_sync_status(true));
                                continue;
                            }

                            if let Err(e) = self.apply_update(update_data) {
                                tracing::error!(
                                    "Failed to apply SyncStep2 update: {:?}. Payload: {:02x?}",
                                    e,
                                    update_data
                                );
                            } else {
                                tracing::debug!(
                                    doc = %self.doc_name,
                                    update_bytes = update_data.len(),
                                    "applied SyncStep2 update"
                                );
                                self.broadcast_sync_update(update_data);
                                self.schedule_update_hook().await;
                                responses.push(self.encode_sync_status(true));
                                self.request_persist().await;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to read SyncStep2 payload: {:?}", e);
                            break;
                        }
                    }
                }
                2 => {
                    // Update: [Tag 2] [Len] [Bytes]
                    match decoder.read_buf() {
                        Ok(update_data) => {
                            tracing::debug!(
                                doc = %self.doc_name,
                                update_bytes = update_data.len(),
                                "handling incremental update"
                            );
                            if let Err(e) = self.apply_update(update_data) {
                                tracing::error!("Failed to apply incremental update: {:?}", e);
                            } else {
                                tracing::debug!(
                                    doc = %self.doc_name,
                                    update_bytes = update_data.len(),
                                    "applied incremental update"
                                );

                                self.broadcast_sync_update(update_data);
                                self.schedule_update_hook().await;
                                responses.push(self.encode_sync_status(true));
                                self.request_persist().await;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to read Update payload: {:?}", e);
                            break;
                        }
                    }
                }
                _ => {
                    tracing::warn!("Unknown sync message tag: {}", tag);
                    break;
                }
            }
        }
    }

    fn encode_sync_update(&self, update_data: &[u8]) -> Vec<u8> {
        let mut encoder = EncoderV1::new();
        encoder.write_var(2u32);
        encoder.write_buf(update_data);
        self.encode_hocuspocus_message(MSG_SYNC, &encoder.to_vec())
    }

    fn encode_sync_status(&self, applied: bool) -> Vec<u8> {
        let mut encoder = EncoderV1::new();
        encoder.write_var(if applied { 1u32 } else { 0u32 });
        self.encode_hocuspocus_message(MSG_SYNC_STATUS, &encoder.to_vec())
    }

    fn broadcast_sync_update(&self, update_data: &[u8]) {
        let msg = self.encode_sync_update(update_data);
        match self.broadcast_tx.send(msg) {
            Ok(receiver_count) => tracing::trace!(
                doc = %self.doc_name,
                update_bytes = update_data.len(),
                receiver_count,
                "broadcast sync update"
            ),
            Err(e) => tracing::trace!(
                doc = %self.doc_name,
                update_bytes = update_data.len(),
                error = ?e,
                "sync update broadcast had no receivers"
            ),
        }
    }

    fn run_update_hook_for(
        doc_name: &str,
        doc: &StdArc<StdMutex<Doc>>,
        hook: &UpdateHook,
    ) -> Option<Vec<u8>> {
        let started = std::time::Instant::now();
        let doc = doc.lock().unwrap_or_else(|e| {
            tracing::warn!("Doc mutex was poisoned for '{}', recovering", doc_name);
            e.into_inner()
        });

        let before_repair = {
            let txn = doc.transact();
            txn.state_vector()
        };

        let repaired = {
            let mut txn = doc.transact_mut();
            hook(&mut txn)
        };

        if repaired == 0 {
            tracing::debug!(
                doc = %doc_name,
                elapsed_ms = started.elapsed().as_millis(),
                "update hook checked"
            );
            return None;
        }

        let repair_update = {
            let txn = doc.transact();
            txn.encode_state_as_update_v1(&before_repair)
        };

        tracing::info!(
            doc = %doc_name,
            repaired,
            repair_update_bytes = repair_update.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "update hook repaired document"
        );

        Some(repair_update)
    }

    async fn schedule_update_hook(&self) {
        let Some(hook) = self.update_hook.clone() else {
            return;
        };

        let mut scheduler = self.update_hook_scheduler.lock().await;
        scheduler.dirty = true;

        if scheduler.running {
            scheduler.coalesced_updates += 1;
            tracing::trace!(
                doc = %self.doc_name,
                coalesced_updates = scheduler.coalesced_updates,
                "coalesced update hook request"
            );
            return;
        }

        scheduler.running = true;
        let doc_name = self.doc_name.clone();
        let doc = self.doc.clone();
        let broadcast_tx = self.broadcast_tx.clone();
        let scheduler_state = self.update_hook_scheduler.clone();
        #[cfg(feature = "sqlite")]
        let db = self.db.clone();
        #[cfg(feature = "sqlite")]
        let last_persist_request = self.last_persist_request.clone();
        #[cfg(feature = "sqlite")]
        let persist_pending = self.persist_pending.clone();

        tracing::debug!(
            doc = %doc_name,
            debounce_ms = UPDATE_HOOK_DEBOUNCE_MS,
            "scheduled update hook"
        );

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(UPDATE_HOOK_DEBOUNCE_MS)).await;

                let coalesced_updates = {
                    let mut scheduler = scheduler_state.lock().await;
                    scheduler.dirty = false;
                    let coalesced = scheduler.coalesced_updates;
                    scheduler.coalesced_updates = 0;
                    coalesced
                };

                tracing::debug!(
                    doc = %doc_name,
                    coalesced_updates,
                    "running update hook"
                );

                let repair_update = Self::run_update_hook_for(&doc_name, &doc, &hook);

                if let Some(repair_update) = repair_update {
                    let msg = Self::encode_hocuspocus_sync_update(&doc_name, &repair_update);
                    match broadcast_tx.send(msg) {
                        Ok(receiver_count) => tracing::debug!(
                            doc = %doc_name,
                            repair_update_bytes = repair_update.len(),
                            receiver_count,
                            "broadcast repair update"
                        ),
                        Err(e) => tracing::debug!(
                            doc = %doc_name,
                            repair_update_bytes = repair_update.len(),
                            error = ?e,
                            "repair update broadcast had no receivers"
                        ),
                    }

                    #[cfg(feature = "sqlite")]
                    {
                        Self::request_persist_for(
                            doc_name.clone(),
                            db.clone(),
                            doc.clone(),
                            last_persist_request.clone(),
                            persist_pending.clone(),
                        )
                        .await;
                    }
                }

                let should_continue = {
                    let mut scheduler = scheduler_state.lock().await;
                    if scheduler.dirty {
                        true
                    } else {
                        scheduler.running = false;
                        false
                    }
                };

                if !should_continue {
                    tracing::debug!(doc = %doc_name, "update hook idle");
                    break;
                }
            }
        });
    }

    fn encode_hocuspocus_sync_update(doc_name: &str, update_data: &[u8]) -> Vec<u8> {
        let mut update_encoder = EncoderV1::new();
        update_encoder.write_var(2u32);
        update_encoder.write_buf(update_data);

        let mut envelope = EncoderV1::new();
        envelope.write_string(doc_name);
        envelope.write_var(MSG_SYNC as u32);

        let mut encoded = envelope.to_vec();
        encoded.extend_from_slice(&update_encoder.to_vec());
        encoded
    }

    /// Apply a Yjs update to the document
    pub fn apply_update(
        &self,
        update_data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let started = Instant::now();
        let update = Update::decode_v1(update_data)?;
        let doc = self.doc.lock().unwrap_or_else(|e| {
            tracing::warn!("Doc mutex was poisoned for '{}', recovering", self.doc_name);
            e.into_inner()
        });
        let mut txn = doc.transact_mut();
        txn.apply_update(update);
        tracing::debug!(
            doc = %self.doc_name,
            update_bytes = update_data.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "applied yrs update"
        );
        Ok(())
    }

    /// Request persistence with debouncing
    pub async fn request_persist(&self) {
        #[cfg(feature = "sqlite")]
        {
            Self::request_persist_for(
                self.doc_name.clone(),
                self.db.clone(),
                self.doc.clone(),
                self.last_persist_request.clone(),
                self.persist_pending.clone(),
            )
            .await;
        }
    }

    #[cfg(feature = "sqlite")]
    async fn request_persist_for(
        doc_name: String,
        db: Database,
        doc: StdArc<StdMutex<Doc>>,
        last_persist_request: Arc<TokioMutex<Option<Instant>>>,
        persist_pending: Arc<TokioMutex<bool>>,
    ) {
        let now = Instant::now();

        {
            let mut last_request = last_persist_request.lock().await;
            *last_request = Some(now);
        }

        // Check if persistence is already pending
        let already_pending = {
            let pending = persist_pending.lock().await;
            *pending
        };

        if !already_pending {
            // Mark as pending
            {
                let mut pending = persist_pending.lock().await;
                *pending = true;
            }

            tokio::spawn(async move {
                let debounce = Duration::from_millis(PERSIST_DEBOUNCE_MS);

                loop {
                    let persisted_request = loop {
                        let sleep_for = {
                            let last_request = last_persist_request.lock().await;
                            match *last_request {
                                Some(last) => {
                                    let elapsed = last.elapsed();
                                    if elapsed >= debounce {
                                        break last;
                                    }
                                    debounce - elapsed
                                }
                                None => debounce,
                            }
                        };
                        tokio::time::sleep(sleep_for).await;
                    };

                    let state = {
                        let doc = doc.lock().unwrap_or_else(|e| {
                            tracing::warn!("Doc mutex was poisoned for '{}', recovering", doc_name);
                            e.into_inner()
                        });
                        let txn = doc.transact();
                        txn.encode_state_as_update_v1(&StateVector::default())
                    };

                    if let Err(e) = db.save_doc(&doc_name, state).await {
                        tracing::error!("Failed to persist document '{}': {:?}", doc_name, e);
                    } else {
                        tracing::debug!("Persisted document '{}'", doc_name);
                    }

                    {
                        let mut pending = persist_pending.lock().await;
                        *pending = false;
                    }

                    let latest_request = {
                        let last_request = last_persist_request.lock().await;
                        *last_request
                    };

                    if latest_request == Some(persisted_request) {
                        break;
                    }

                    let mut pending = persist_pending.lock().await;
                    if *pending {
                        break;
                    }
                    *pending = true;
                }
            });
        }
    }

    /// Force immediate persistence (for graceful shutdown)
    pub async fn force_persist(&self) {
        #[cfg(feature = "sqlite")]
        {
            let state = {
                let doc = self.doc.lock().unwrap_or_else(|e| {
                    tracing::warn!("Doc mutex was poisoned for '{}', recovering", self.doc_name);
                    e.into_inner()
                });
                let txn = doc.transact();
                txn.encode_state_as_update_v1(&StateVector::default())
            };

            if let Err(e) = self.db.save_doc(&self.doc_name, state).await {
                tracing::error!(
                    "Failed to persist document '{}' on shutdown: {:?}",
                    self.doc_name,
                    e
                );
            } else {
                tracing::info!("Persisted document '{}' on shutdown", self.doc_name);
            }
        }
    }

    /// Get a subscription to broadcast messages
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.broadcast_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sqlite")]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(feature = "sqlite")]
    use yrs::encoding::read::Read;
    #[cfg(feature = "sqlite")]
    use yrs::updates::decoder::DecoderV1;
    #[cfg(feature = "sqlite")]
    use yrs::updates::encoder::{Encoder, EncoderV1};
    #[cfg(feature = "sqlite")]
    use yrs::{GetString, Text, Transact, WriteTxn};

    /// Creates an in-memory test database
    #[cfg(feature = "sqlite")]
    async fn create_test_db() -> Database {
        Database::init_in_memory().expect("Failed to create test database")
    }

    #[cfg(feature = "sqlite")]
    fn encode_test_msg(doc_name: &str, msg_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut encoder = EncoderV1::new();
        encoder.write_string(doc_name);
        encoder.write_var(msg_type as u32);
        let mut v = encoder.to_vec();
        v.extend_from_slice(payload);
        v
    }

    #[cfg(feature = "sqlite")]
    fn encode_sync_step1(sv: &StateVector) -> Vec<u8> {
        let mut sv_encoder = EncoderV1::new();
        sv.encode(&mut sv_encoder);
        let sv_bytes = sv_encoder.to_vec();

        let mut encoder = EncoderV1::new();
        encoder.write_var(0u32); // Tag 0
        encoder.write_buf(&sv_bytes);
        encoder.to_vec()
    }

    #[cfg(feature = "sqlite")]
    fn encode_update(update: &[u8]) -> Vec<u8> {
        let mut encoder = EncoderV1::new();
        encoder.write_var(2u32); // Tag 2
        encoder.write_buf(update);
        encoder.to_vec()
    }

    #[cfg(feature = "sqlite")]
    fn encode_sync_step2(update: &[u8]) -> Vec<u8> {
        let mut encoder = EncoderV1::new();
        encoder.write_var(1u32); // Tag 1
        encoder.write_buf(update);
        encoder.to_vec()
    }

    #[cfg(feature = "sqlite")]
    fn extract_sync_update(message: &[u8]) -> Vec<u8> {
        let (rest, _name) = DocHandler::read_and_skip_doc_name(message).unwrap();
        let mut decoder = DecoderV1::from(rest);
        let msg_type: u32 = decoder.read_var().unwrap();
        assert_eq!(msg_type as u8, MSG_SYNC);
        let tag: u32 = decoder.read_var().unwrap();
        assert_eq!(tag, 2);
        decoder.read_buf().unwrap().to_vec()
    }

    #[cfg(feature = "sqlite")]
    fn is_applied_sync_status(message: &[u8]) -> bool {
        let (rest, _name) = DocHandler::read_and_skip_doc_name(message).unwrap();
        let mut decoder = DecoderV1::from(rest);
        let msg_type: u32 = decoder.read_var().unwrap();
        let applied: u32 = decoder.read_var().unwrap();
        msg_type as u8 == MSG_SYNC_STATUS && applied == 1
    }

    #[cfg(feature = "sqlite")]
    fn repair_text(handler: &DocHandler) -> String {
        let doc = handler.doc.lock().unwrap();
        let txn = doc.transact();
        txn.get_text("repair")
            .map(|text| text.get_string(&txn))
            .unwrap_or_default()
    }

    #[cfg(feature = "sqlite")]
    fn single_repair_hook() -> UpdateHook {
        std::sync::Arc::new(|txn| {
            let text = txn.get_or_insert_text("repair");
            if text.get_string(txn).is_empty() {
                text.push(txn, "fixed");
                1
            } else {
                0
            }
        })
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_doc_handler_creation() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;
        assert_eq!(handler.doc_name, "test-room");
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_initial_sync_generation() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;

        let messages = handler.generate_initial_sync();
        assert_eq!(messages.len(), 1);

        // Should start with doc name "test-room"
        let (rest, name) =
            DocHandler::read_and_skip_doc_name(&messages[0]).expect("Should parse doc name");
        assert_eq!(name, "test-room");

        // Next should be MSG_SYNC
        let mut decoder = DecoderV1::from(rest);
        let msg_type: u32 = decoder.read_var().expect("Should parse msg type");
        assert_eq!(msg_type as u8, MSG_SYNC);
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_sync_step1_response() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;

        // Create a client state vector (empty = requesting all updates)
        let client_sv = StateVector::default();
        let payload = encode_sync_step1(&client_sv);

        let msg = encode_test_msg("test-room", MSG_SYNC, &payload);

        let responses = handler.handle_message(&msg).await;

        // Should get SyncStep2 + SyncStep1 back
        assert_eq!(responses.len(), 2);

        // Verify response structure
        for resp in responses {
            let (rest, name) = DocHandler::read_and_skip_doc_name(&resp).unwrap();
            assert_eq!(name, "test-room");
            let mut d = DecoderV1::from(rest);
            let t: u32 = d.read_var().unwrap();
            assert_eq!(t as u8, MSG_SYNC);
        }
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_update_application_and_broadcast() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;

        // Subscribe to broadcasts
        let mut rx = handler.subscribe();

        // Create an update from a client doc
        let client_doc = Doc::new();
        let update = {
            let text = client_doc.get_or_insert_text("test");
            let mut txn = client_doc.transact_mut();
            text.push(&mut txn, "Hello, World!");
            txn.encode_update_v1()
        };

        // Send as SyncMessage::Update
        let payload = encode_update(&update);
        let msg = encode_test_msg("test-room", MSG_SYNC, &payload);

        let responses = handler.handle_message(&msg).await;
        assert!(
            responses
                .iter()
                .any(|response| is_applied_sync_status(response)),
            "server must acknowledge applied updates so clients clear unsynced state"
        );

        // Should have broadcast the update
        let broadcast = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(broadcast.is_ok());
        let broadcast_data = broadcast.unwrap().unwrap();

        // Verify broadcast format
        let (_, name) = DocHandler::read_and_skip_doc_name(&broadcast_data).unwrap();
        assert_eq!(name, "test-room");
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_persistence_after_update() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db.clone()).await;

        // Create an update
        let client_doc = Doc::new();
        let update = {
            let text = client_doc.get_or_insert_text("test");
            let mut txn = client_doc.transact_mut();
            text.push(&mut txn, "Persistent data");
            txn.encode_update_v1()
        };

        // Send update
        let payload = encode_update(&update);
        let msg = encode_test_msg("test-room", MSG_SYNC, &payload);

        let _responses = handler.handle_message(&msg).await;

        // Force persist (normally debounced)
        handler.force_persist().await;

        // Verify data was saved
        let saved = db.get_doc("test-room").await.unwrap();
        assert!(saved.is_some());
        assert!(!saved.unwrap().is_empty());
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_debounced_persistence_saves_latest_live_state() {
        let db = create_test_db().await;
        let handler = DocHandler::new("debounce-room".to_string(), db.clone()).await;
        let client_doc = Doc::new();
        let text = client_doc.get_or_insert_text("test");

        let first_update = {
            let mut txn = client_doc.transact_mut();
            text.push(&mut txn, "first");
            txn.encode_update_v1()
        };
        let first_msg = encode_test_msg("debounce-room", MSG_SYNC, &encode_update(&first_update));
        handler.handle_message(&first_msg).await;

        tokio::time::sleep(Duration::from_millis(100)).await;

        let second_update = {
            let mut txn = client_doc.transact_mut();
            text.push(&mut txn, "second");
            txn.encode_update_v1()
        };
        let second_msg = encode_test_msg("debounce-room", MSG_SYNC, &encode_update(&second_update));
        handler.handle_message(&second_msg).await;

        tokio::time::sleep(Duration::from_millis(PERSIST_DEBOUNCE_MS * 2 + 250)).await;

        let reloaded = DocHandler::new("debounce-room".to_string(), db).await;
        let doc = reloaded.doc.lock().unwrap();
        let text = doc.get_or_insert_text("test");
        let txn = doc.transact();
        assert_eq!(text.get_string(&txn), "firstsecond");
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_empty_sync_step2_acknowledges_initial_handshake() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;

        let payload = encode_sync_step2(&[]);
        let msg = encode_test_msg("test-room", MSG_SYNC, &payload);
        let responses = handler.handle_message(&msg).await;

        assert_eq!(responses.len(), 1);
        assert!(is_applied_sync_status(&responses[0]));
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_update_hook_acknowledges_before_debounced_repair() {
        let db = create_test_db().await;
        let handler = DocHandler::new_with_update_hook(
            "test-room".to_string(),
            db,
            Some(single_repair_hook()),
        )
        .await;
        let mut rx = handler.subscribe();

        let client_doc = Doc::new();
        let update = {
            let text = client_doc.get_or_insert_text("content");
            let mut txn = client_doc.transact_mut();
            text.push(&mut txn, "client data");
            txn.encode_update_v1()
        };

        let payload = encode_update(&update);
        let msg = encode_test_msg("test-room", MSG_SYNC, &payload);
        let responses = handler.handle_message(&msg).await;

        assert_eq!(responses.len(), 1);
        assert!(
            responses
                .iter()
                .any(|response| is_applied_sync_status(response)),
            "sender should receive a syncStatus ack after the update is applied"
        );
        assert_eq!(repair_text(&handler), "");

        let original = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(extract_sync_update(&original), update);

        let repair_response = tokio::time::timeout(Duration::from_millis(1000), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let repair_update = extract_sync_update(&repair_response);
        let receiver_doc = Doc::new();
        {
            let mut txn = receiver_doc.transact_mut();
            txn.apply_update(Update::decode_v1(&repair_update).unwrap());
        }
        let repair = receiver_doc.get_or_insert_text("repair");
        assert_eq!(repair.get_string(&receiver_doc.transact()), "fixed");
        assert_eq!(repair_text(&handler), "fixed");
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_update_hook_repair_is_debounced_broadcast_and_persisted() {
        let db = create_test_db().await;
        let handler = DocHandler::new_with_update_hook(
            "hook-room".to_string(),
            db.clone(),
            Some(single_repair_hook()),
        )
        .await;
        let mut rx = handler.subscribe();

        let client_doc = Doc::new();
        let update = {
            let text = client_doc.get_or_insert_text("content");
            let mut txn = client_doc.transact_mut();
            text.push(&mut txn, "client data");
            txn.encode_update_v1()
        };

        let payload = encode_update(&update);
        let msg = encode_test_msg("hook-room", MSG_SYNC, &payload);
        handler.handle_message(&msg).await;

        let original = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let repair = tokio::time::timeout(Duration::from_millis(1000), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            DocHandler::read_and_skip_doc_name(&original).unwrap().1,
            "hook-room"
        );
        assert_eq!(
            DocHandler::read_and_skip_doc_name(&repair).unwrap().1,
            "hook-room"
        );

        handler.force_persist().await;
        let reloaded = DocHandler::new("hook-room".to_string(), db).await;
        assert_eq!(repair_text(&reloaded), "fixed");
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_update_hook_coalesces_multiple_small_updates() {
        let db = create_test_db().await;
        let hook_runs = std::sync::Arc::new(AtomicUsize::new(0));
        let hook_runs_for_hook = hook_runs.clone();
        let hook: UpdateHook = std::sync::Arc::new(move |_txn| {
            hook_runs_for_hook.fetch_add(1, Ordering::SeqCst);
            0
        });
        let handler =
            DocHandler::new_with_update_hook("coalesce-room".to_string(), db, Some(hook)).await;

        let client_doc = Doc::new();
        let text = client_doc.get_or_insert_text("content");

        for value in ["a", "b", "c", "d", "e"] {
            let update = {
                let mut txn = client_doc.transact_mut();
                text.push(&mut txn, value);
                txn.encode_update_v1()
            };
            let msg = encode_test_msg("coalesce-room", MSG_SYNC, &encode_update(&update));
            handler.handle_message(&msg).await;
        }

        tokio::time::sleep(Duration::from_millis(UPDATE_HOOK_DEBOUNCE_MS * 2)).await;
        assert_eq!(
            hook_runs.load(Ordering::SeqCst),
            1,
            "rapid content updates should share one repair hook run"
        );
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_document_reload_from_db() {
        let db = create_test_db().await;

        // Create a handler and add some data
        let handler1 = DocHandler::new("reload-test".to_string(), db.clone()).await;

        let client_doc = Doc::new();
        let update = {
            let text = client_doc.get_or_insert_text("content");
            let mut txn = client_doc.transact_mut();
            text.push(&mut txn, "Test content for reload");
            txn.encode_update_v1()
        };

        let payload = encode_update(&update);
        let msg = encode_test_msg("reload-test", MSG_SYNC, &payload);

        handler1.handle_message(&msg).await;
        handler1.force_persist().await;

        // Drop the first handler
        drop(handler1);

        // Create a new handler for the same room - should load from DB
        let handler2 = DocHandler::new("reload-test".to_string(), db).await;

        // Verify the document has the content
        let doc = handler2.doc.lock().unwrap();
        let text = doc.get_or_insert_text("content");
        let txn = doc.transact();
        let content = text.get_string(&txn);

        assert_eq!(content, "Test content for reload");
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_awareness_forwarding() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;

        // Subscribe to broadcasts
        let mut rx = handler.subscribe();

        // Create a fake awareness message
        let body = vec![1, 2, 3, 4];
        let awareness_msg = encode_test_msg("test-room", MSG_AWARENESS, &body);

        let _responses = handler.handle_message(&awareness_msg).await;

        // Should have broadcast the awareness message
        let broadcast = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(broadcast.is_ok());
        let received = broadcast.unwrap().unwrap();

        // Should effectively be identical to input since we re-wrap with same doc name
        assert_eq!(received, awareness_msg);
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_empty_message_handling() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;

        // Empty message should return empty responses
        let responses = handler.handle_message(&[]).await;
        assert!(responses.is_empty());
    }

    #[tokio::test]
    #[cfg(not(feature = "sqlite"))]
    async fn test_doc_handler_no_sqlite() {
        let handler = DocHandler::new("test-room-no-db".to_string()).await;
        assert_eq!(handler.doc_name, "test-room-no-db");

        // Basic sync generation should still work
        let messages = handler.generate_initial_sync();
        assert_eq!(messages.len(), 1);

        let (_, name) = DocHandler::read_and_skip_doc_name(&messages[0]).unwrap();
        assert_eq!(name, "test-room-no-db");
    }

    // ---- Resilience tests: ensure no panics on malformed input ----

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_malformed_binary_message() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;

        // Pure garbage bytes should not panic
        let garbage = vec![0xFF, 0xFE, 0xFD, 0xFC, 0xFB];
        let responses = handler.handle_message(&garbage).await;
        // Should gracefully return (possibly empty), not panic
        let _ = responses;
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_truncated_sync_message() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;

        // Valid doc name header + MSG_SYNC + truncated payload (incomplete SyncStep1)
        let mut msg = Vec::new();
        // VarString "test-room" = len 9 + bytes
        msg.push(9);
        msg.extend_from_slice(b"test-room");
        msg.push(MSG_SYNC); // msg type
        msg.push(0); // SyncStep1 tag
        msg.push(99); // claims 99 bytes of SV data but provides none

        let responses = handler.handle_message(&msg).await;
        // Should return empty (decode failure logged), not panic
        assert!(responses.is_empty());
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_invalid_update_data() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;

        // Valid envelope with Update tag (2) but invalid yrs data inside
        let garbage_update = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let payload = encode_update(&garbage_update);
        let msg = encode_test_msg("test-room", MSG_SYNC, &payload);

        let responses = handler.handle_message(&msg).await;
        // apply_update should return Err (logged), not panic
        let _ = responses;
    }

    #[tokio::test]
    #[cfg(feature = "sqlite")]
    async fn test_empty_content_after_doc_name() {
        let db = create_test_db().await;
        let handler = DocHandler::new("test-room".to_string(), db).await;

        // Message with only doc name, no msg type or payload
        let mut msg = Vec::new();
        msg.push(9);
        msg.extend_from_slice(b"test-room");
        // Nothing after the doc name

        let responses = handler.handle_message(&msg).await;
        assert!(responses.is_empty());
    }
}
