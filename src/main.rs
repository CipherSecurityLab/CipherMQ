use crate::config::Config;
use crate::connection::{create_listener, load_tls_config, TlsConnection};
use crate::server::handle_client;
use crate::state::ServerState;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info};

mod state;
mod server;
mod connection;
mod config;

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
            let tls_config = load_tls_config(&cert_path, &key_path)?;
            let acceptor = TlsAcceptor::from(Arc::new(tls_config));

            loop {
                let (stream, addr) = listener.accept().await?;
                info!("New TLS connection from {}", addr);
                let state = state.clone();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match TlsConnection::new(stream, acceptor).await {
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
        "tcp" => {
            loop {
                let (stream, addr) = listener.accept().await?;
                info!("New TCP connection from {}", addr);
                let state = state.clone();
                tokio::spawn(async move {
                    handle_client(stream, state).await;
                });
            }
        }
        _ => Err("Invalid connection_type in config".into()),
    }
}