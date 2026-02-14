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
hocuspocus-rs = "0.1.0"
```

### Feature Flags

- `server`: Enables `axum` integration and built-in WebSocket handlers.
- `sqlite`: Enables `rusqlite` persistence layer.

## Client Compatibility

This server implementation is designed to connect with the **Hocuspocus JavaScript client**. 

In your frontend project, you can use the `@hocuspocus/provider` to connect:

```javascript
import { HocuspocusProvider } from '@hocuspocus/provider'
import * as Y from 'yjs'

const ydoc = new Y.Doc()

const provider = new HocuspocusProvider({
  url: 'ws://127.0.0.1:1234/sync',
  name: 'my-document-name',
  document: ydoc,
})
```

## Usage

### Using with Axum (and SQLite)

Enable the `server` and `sqlite` features in your `Cargo.toml`.

```rust
use hocuspocus_rs::{AppState, Database, create_router};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // 1. Initialize database (requires 'sqlite' feature)
    let db = Database::init("sync.db").expect("Failed to init DB");
    
    // 2. Create shared state (AppState structure depends on feature flags)
    let state = Arc::new(AppState::new(db));
    
    // 3. Create router (requires 'server' feature)
    let app = create_router(state);
    
    // 4. Run the server
    let listener = tokio::net::TcpListener::bind("127.0.0.1:1234").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

### Using with Axum (In-Memory / No SQLite)

Enable only the `server` feature.

```rust
use hocuspocus_rs::{AppState, create_router};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let state = Arc::new(AppState::new());
    let app = create_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:1234").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

### Manual Integration

You can also use the `DocHandler` directly if you're using a different web framework:

```rust
use hocuspocus_rs::DocHandler;

async fn my_websocket_handler(data: &[u8], handler: &DocHandler) {
    let responses = handler.handle_message(data).await;
    for resp in responses {
        // Send resp back to client over WebSocket
    }
}
```

## Protocol Details

Hocuspocus V2 protocol prefixes every message with the document name as a VarString. This implementation handles that automatically, allowing multiple documents to be multiplexed over the same connection if needed.

## License

BSD-3-Clause - Copyright (c) 2026, Jagtesh Chadha.
