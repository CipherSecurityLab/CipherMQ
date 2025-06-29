use crate::auth::{AuthHandler, MTlsAuth};
use crate::config::Config;
use crate::connection::{create_listener};
use crate::server::handle_client;
use crate::state::ServerState;
use std::sync::Arc;
use tracing::{error, info};

mod state;
mod server;
mod connection;
mod config;
mod auth;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let config = Config::load("config.toml")?;
    let state = Arc::new(tokio::sync::RwLock::new(ServerState::new()));
    let listener = create_listener(&config.address).await?;
    info!("Server listening on {}", config.address);

    match config.connection_type.as_str() {
        "tls" => {
            let cert_path = config.cert_path.ok_or("Missing cert_path for TLS")?;
            let key_path = config.key_path.ok_or("Missing key_path for TLS")?;
            let ca_cert_path = config.ca_cert_path.ok_or("Missing ca_cert_path for TLS")?;
            let auth_handler = MTlsAuth::new(&cert_path, &key_path, &ca_cert_path)?;

            loop {
                let (stream, addr) = listener.accept().await?;
                info!("New TLS connection from {}", addr);
                let state = state.clone();
                let auth_handler = auth_handler.clone();
                tokio::spawn(async move {
                    match auth_handler.authenticate(stream).await {
                        Ok(tls_stream) => {
                            handle_client(tls_stream, state).await;
                        }
                        Err(e) => {
                            error!("TLS handshake failed: {}", e);
                        }
                    }
                });
            }
        }
        _ => Err("Invalid connection_type in config".into()),
    }
}