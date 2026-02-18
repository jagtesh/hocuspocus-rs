# hocuspocus-rs

A Rust implementation of the [Hocuspocus](https://hocuspocus.dev/) protocol (Yjs over WebSockets).

This crate provides a thread-safe handler for Yjs documents that follows the Hocuspocus V2 protocol, allowing Rust-based servers to synchronize with Hocuspocus and `y-websocket` clients.

## Features

- **Hocuspocus V2 Protocol**: Full support for document-name prefixed messages.
- **Yjs Sync**: Seamless synchronization using the `yrs` library.
- **Optional Persistence**: Built-in SQLite persistence with debounced saving (via `sqlite` feature).
- **Awareness**: Forwarding of awareness/presence messages.
- **Axum Integration**: Built-in WebSocket handlers for the Axum web framework (via `server` feature).

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
# For most users, 'server' is required to get the built-in sync server logic.
hocuspocus-rs = { version = "0.1.2", features = ["server"] }
```

### Feature Flags

- **`server` (Recommended)**: Enables `axum` integration and provides built-in WebSocket handlers for synchronization.
- **`sqlite`**: Enables `rusqlite` persistence layer to store document updates.

## Performance Profile and Intended Use

This crate is designed for high performance in low-concurrency environments.

- It is single-threaded by design for document persistence, because SQLite is single-threaded in this architecture.
- It is ideal for a handful of users collaborating across multiple devices (low concurrent load).
- It is especially well-suited for embedding collaborative sync into applications.
- It is currently used both as a standalone personal server deployment and embedded inside a Tauri desktop app.

If you need to support many users concurrently, prefer the `hocuspocus-rs-ws` crate: https://github.com/albireo3754/hocuspocus-rs-ws

## Client Compatibility

This server implementation is designed to connect with the **Hocuspocus JavaScript client**. 

In your frontend project, you can use the `@hocuspocus/provider` to connect:

```javascript
import { HocuspocusProvider } from '@hocuspocus/provider'
import * as Y from 'yjs'

const ydoc = new Y.Doc()

const provider = new HocuspocusProvider({
  // The 'url' should point to your Rust server's sync endpoint.
  // The default Axum router maps this to '/sync/:room_name'.
  url: 'ws://127.0.0.1:1234/sync',
  name: 'my-document-name',
  document: ydoc,
})
```

## Usage

### Using with Axum

The `server` feature provides a `create_router` function that returns an `axum::Router`. You can mount this router at any path you choose.

```rust
use hocuspocus_rs::{AppState, Database, create_router};
use std::sync::Arc;
use axum::Router;

#[tokio::main]
async fn main() {
    // 1. (Optional) Initialize database if using 'sqlite' feature
    let db = Database::init("sync.db").expect("Failed to init DB");
    
    // 2. Create shared state
    // If 'sqlite' is enabled, pass the db. Otherwise use AppState::new().
    let state = Arc::new(AppState::new(db));
    
    // 3. Create the sync router
    // This provides routes for /sync and /sync/:room_name by default.
    let hocuspocus_router = create_router(state);
    
    // 4. Nest it into your main application router at any endpoint
    let app = Router::new()
        .nest("/", hocuspocus_router); // Result: ws://localhost:1234/sync/...
    
    // 5. Run the server on your desired port
    let port = "127.0.0.1:1234";
    let listener = tokio::net::TcpListener::bind(port).await.unwrap();
    println!("Hocuspocus server listening on {}", port);
    axum::serve(listener, app).await.unwrap();
}
```

### Configuration Details

- **Endpoint**: The built-in router handles `GET /sync` (for multiplexed connections) and `GET /sync/:room_name` (for room-specific connections). You can change the base path by using `.nest("/my-custom-path", hocuspocus_router)` in your Axum setup.
- **Port**: Control the port by changing the address passed to `TcpListener::bind`.
- **Database**: If the `sqlite` feature is enabled, `AppState::new(db)` accepts a `Database` instance. If disabled, `AppState::new()` takes no arguments.

### Manual Integration (No Axum)

If you're using a different web framework, you can use the `DocHandler` directly:

```rust
use hocuspocus_rs::DocHandler;

async fn my_websocket_handler(data: &[u8], handler: &DocHandler) {
    // This will parse Hocuspocus V2 messages and return responses
    let responses = handler.handle_message(data).await;
    for resp in responses {
        // Send 'resp' back to client over your WebSocket connection
    }
}
```

## Protocol Details

Hocuspocus V2 protocol prefixes every message with the document name as a VarString. This implementation handles that automatically, allowing multiple documents to be multiplexed over the same connection if needed.

## License

BSD-3-Clause - Copyright (c) 2026, Jagtesh Chadha.
