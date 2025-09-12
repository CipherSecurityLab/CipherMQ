use crate::auth::{AuthHandler, MTlsAuth};
use crate::config::Config;
use crate::connection::create_listener;
use crate::server::handle_client;
use crate::state::ServerState;
use crate::storage::Storage;
use std::sync::Arc;
use tracing::{error, info, Level};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};
use tracing_appender::rolling;
use tracing_appender::non_blocking;

mod state;
mod server;
mod connection;
mod config;
mod auth;
mod storage;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration
    let config = Config::load("config.toml")?;

    // Initialize logging
    let _max_file_size = config.logging.max_size_mb * 1_000_000; // Convert MB to bytes, unused due to tracing_appender limitations

    // Create rolling file appenders for each log level
    let info_appender = match config.logging.rotation.as_str() {
        "hourly" => rolling::hourly("./", &config.logging.info_file_path),
        "daily" => rolling::daily("./", &config.logging.info_file_path),
        _ => rolling::never("./", &config.logging.info_file_path),
    };
    let debug_appender = match config.logging.rotation.as_str() {
        "hourly" => rolling::hourly("./", &config.logging.debug_file_path),
        "daily" => rolling::daily("./", &config.logging.debug_file_path),
        _ => rolling::never("./", &config.logging.debug_file_path),
    };
    let error_appender = match config.logging.rotation.as_str() {
        "hourly" => rolling::hourly("./", &config.logging.error_file_path),
        "daily" => rolling::daily("./", &config.logging.error_file_path),
        _ => rolling::never("./", &config.logging.error_file_path),
    };

    // Create non-blocking writers for each log level
    let (info_writer, _info_guard) = non_blocking(info_appender);
    let (debug_writer, _debug_guard) = non_blocking(debug_appender);
    let (error_writer, _error_guard) = non_blocking(error_appender);

    // Create layers for each log level with JSON format
    let info_layer = fmt::layer()
        .json()
        .with_writer(info_writer)
        .with_filter(tracing_subscriber::filter::filter_fn(|metadata| metadata.level() == &Level::INFO));
    let debug_layer = fmt::layer()
        .json()
        .with_writer(debug_writer)
        .with_filter(tracing_subscriber::filter::filter_fn(|metadata| metadata.level() == &Level::DEBUG));
    let error_layer = fmt::layer()
        .json()
        .with_writer(error_writer)
        .with_filter(tracing_subscriber::filter::filter_fn(|metadata| metadata.level() == &Level::ERROR));

    // Console layer to show all logs (pretty format)
    let stdout_layer = fmt::layer()
        .pretty()
        .with_filter(EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(&config.logging.level)));

    // Combine all layers
    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(info_layer)
        .with(debug_layer)
        .with(error_layer)
        .init();

    info!("Logging initialized with level: {}", config.logging.level);

    // Initialize storage and server state
    let storage = Arc::new(Storage::new(&config.database).await?);
    let state = Arc::new(tokio::sync::RwLock::new(ServerState::new(storage.clone())));
    let listener = create_listener(&config.server.address).await?;
    info!("Server listening on {}", config.server.address);

    // Request resend for unacknowledged messages
    let unacknowledged_metadata = storage.load_unacknowledged_metadata().await?;
    for metadata in unacknowledged_metadata {
        info!(
            message_id = %metadata.message_id,
            client_id = %metadata.client_id,
            "Requesting resend for message"
        );
    }

    match config.server.connection_type.as_str() {
        "tls" => {
            let cert_path = config.tls.cert_path.ok_or("Missing cert_path for TLS")?;
            let key_path = config.tls.key_path.ok_or("Missing key_path for TLS")?;
            let ca_cert_path = config.tls.ca_cert_path.ok_or("Missing ca_cert_path for TLS")?;
            let auth_handler = MTlsAuth::new(&cert_path, &key_path, &ca_cert_path)?;

            loop {
                let (stream, addr) = listener.accept().await?;
                info!(client_addr = %addr, "New TLS connection established");
                let state = state.clone();
                let auth_handler = auth_handler.clone();
                tokio::spawn(async move {
                    match auth_handler.authenticate(stream).await {
                        Ok(tls_stream) => {
                            handle_client(tls_stream, state).await;
                        }
                        Err(e) => {
                            error!(client_addr = %addr, error = %e, "TLS handshake failed");
                        }
                    }
                });
            }
        }
        _ => Err("Invalid connection_type in config".into()),
    }
}