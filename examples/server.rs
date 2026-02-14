use hocuspocus_rs::{AppState, create_router};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "hocuspocus_rs=trace,tower_http=debug".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Create shared state
    #[cfg(feature = "sqlite")]
    let state = Arc::new(AppState::new(hocuspocus_rs::Database::init_in_memory().unwrap()));
    #[cfg(not(feature = "sqlite"))]
    let state = Arc::new(AppState::new());
    
    let app = create_router(state);
    
    let addr = "127.0.0.1:1234";
    let listener = TcpListener::bind(addr).await.unwrap();
    println!("Listening on: {}", addr);
    
    axum::serve(listener, app).await.unwrap();
}
