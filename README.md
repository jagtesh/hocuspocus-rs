# hocuspocus-rs

A Rust implementation of the [Hocuspocus](https://hocuspocus.dev/) protocol (Yjs over WebSockets).

This crate provides a thread-safe handler for Yjs documents that follows the Hocuspocus V2 protocol, allowing Rust-based servers to synchronize with Hocuspocus and `y-websocket` clients.

## Features

- **Hocuspocus V2 Protocol**: Full support for document-name prefixed messages.
- **Yjs Sync**: Seamless synchronization using the `yrs` library.
- **Persistence**: Built-in SQLite persistence with debounced saving.
- **Awareness**: Forwarding of awareness/presence messages.
- **Axum Integration**: Optional built-in WebSocket handlers for the Axum web framework.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
hocuspocus-rs = "0.1.0"
```

## Usage

### Using with Axum

```rust
use hocuspocus_rs::{AppState, Database, create_router};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Initialize database
    let db = Database::init("sync.db").expect("Failed to init DB");
    
    // Create shared state
    let state = Arc::new(AppState::new(db));
    
    // Create router and nest it or use it directly
    let app = create_router(state);
    
    // Run the server
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

### Manual Integration

You can also use the `DocHandler` directly if you're using a different web framework or protocol:

```rust
use hocuspocus_rs::{DocHandler, Database};

async fn handle_message(handler: &DocHandler, data: &[u8]) {
    let responses = handler.handle_message(data).await;
    for resp in responses {
        // Send resp back to client
    }
}
```

## Protocol Details

Hocuspocus V2 protocol prefixes every message with the document name as a VarString. This implementation handles that automatically, allowing multiple documents to be multiplexed over the same connection if needed (though the provided Axum handlers typically use one room per connection).

## License

BSD-3-Clause - Copyright (c) 2026, Jagtesh Chadha.
