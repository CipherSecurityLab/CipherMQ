// Module declarations for the new structure
mod core;
mod networking;
mod broker;

use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::info; 

// Use statements for the new structure
use crate::core::state::ServerState; // For instantiating the core data store
use crate::broker::engine::BrokerEngine; // The new main engine
use crate::networking::client_handler::handle_client; // The updated client handler

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // 1. Instantiate ServerState (core data layer)
    let core_state = Arc::new(ServerState::new());

    // 2. Instantiate BrokerEngine, passing the Arc'd ServerState
    let engine = Arc::new(BrokerEngine::new(Arc::clone(&core_state)));

    let listener_result = TcpListener::bind("127.0.0.1:5672").await;

    let listener = match listener_result {
        Ok(l) => {
            info!("Message Broker running on 127.0.0.1:5672");
            l
        }
        Err(e) => {
            info!("Failed to bind TCP listener: {:?}. Ensure the address is not in use.", e);
            return;
        }
    };

    // Initial setup calls are now on BrokerEngine
    // These calls expect String arguments based on BrokerEngine's method signatures
    engine.declare_queue("default_queue".to_string());
    engine.declare_exchange("default_exchange".to_string());
    engine.bind_queue(
        "default_exchange".to_string(),
        "default_queue".to_string(),
        "default_key".to_string(),
    );

    // The retry_unacknowledged task still runs on ServerState directly
    let state_clone_for_retry = Arc::clone(&core_state);
    tokio::spawn(async move {
        state_clone_for_retry.retry_unacknowledged().await;
    });

    info!("Broker ready to accept connections."); // Added a log
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                info!("New connection from {}", addr);
                // Pass the Arc'd BrokerEngine to handle_client
                let engine_clone_for_client = Arc::clone(&engine);
                tokio::spawn(async move {
                    handle_client(stream, engine_clone_for_client).await;
                });
            }
            Err(e) => {
                info!("Failed to accept new connection: {:?}", e);
            }
        }
    }
}
