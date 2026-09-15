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
use tokio::sync::Notify;
use dashmap::DashMap;
use std::time::{Duration, Instant};
use tokio::time;


mod state;
mod server;
mod connection;
mod config;
mod auth;
mod storage;
mod metrics;
mod rate_limiter;
mod admin_server;
mod acl;

// Define the HeartbeatMonitor structure.
pub struct HeartbeatMonitor {
    // client_id -> last_heartbeat_time
    heartbeats: DashMap<String, Instant>,
    timeout_duration: Duration,
}

impl HeartbeatMonitor {
    pub fn new(timeout_seconds: u64) -> Self {
        Self {
            heartbeats: DashMap::new(),
            timeout_duration: Duration::from_secs(timeout_seconds),
        }
    }

    pub fn record_heartbeat(&self, client_id: &str) {
        self.heartbeats.insert(client_id.to_string(), Instant::now());
        tracing::debug!("Heartbeat recorded for client {}", client_id);
    }

    pub fn is_alive(&self, client_id: &str) -> bool {
        if let Some(entry) = self.heartbeats.get(client_id) {
            let elapsed = entry.value().elapsed();
            elapsed < self.timeout_duration
        } else {
            false
        }
    }

    pub fn remove_client(&self, client_id: &str) {
        self.heartbeats.remove(client_id);
        tracing::debug!("Removed client {} from heartbeat monitor", client_id);
    }

    pub async fn monitor_task(self: Arc<Self>) {
        let mut interval = time::interval(Duration::from_secs(10));
        
        loop {
            interval.tick().await;
            
            let now = Instant::now();
            let timed_out: Vec<String> = self.heartbeats
                .iter()
                .filter(|entry| {
                    let elapsed = now.duration_since(*entry.value());
                    elapsed > self.timeout_duration
                })
                .map(|entry| entry.key().clone())
                .collect();

            for client_id in &timed_out {
                tracing::warn!(
                    "Client {} timed out (no heartbeat for {:?})",
                    client_id, self.timeout_duration
                );
                self.heartbeats.remove(client_id);
            }

            if !timed_out.is_empty() {
                tracing::info!("Detected {} timed out clients", timed_out.len());
            }
        }
    }

    pub fn get_stats(&self) -> (usize, usize) {
        let total = self.heartbeats.len();
        let now = Instant::now();
        let alive = self.heartbeats
            .iter()
            .filter(|entry| {
                now.duration_since(*entry.value()) < self.timeout_duration
            })
            .count();
        
        (total, alive)
    }
}

// Define the shutdown-related structures.
pub struct ShutdownSignal {
    notify: Arc<Notify>,
}

impl ShutdownSignal {
    pub fn new() -> Self {
        Self {
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn subscribe(&self) -> ShutdownReceiver {
        ShutdownReceiver {
            notify: self.notify.clone(),
        }
    }

    pub fn trigger(&self) {
        info!("Triggering graceful shutdown");
        self.notify.notify_waiters();
    }

    pub async fn wait_for_signal() -> Self {
        let signal = Self::new();
        let signal_clone = signal.clone();

        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                
                let mut sigterm = signal(SignalKind::terminate())
                    .expect("Failed to install SIGTERM handler");
                let mut sigint = signal(SignalKind::interrupt())
                    .expect("Failed to install SIGINT handler");

                tokio::select! {
                    _ = sigterm.recv() => {
                        info!("Received SIGTERM");
                    }
                    _ = sigint.recv() => {
                        info!("Received SIGINT");
                    }
                }
            }

            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c()
                    .await
                    .expect("Failed to install Ctrl+C handler");
                info!("Received Ctrl+C");
            }

            signal_clone.trigger();
        });

        signal
    }
}

impl Clone for ShutdownSignal {
    fn clone(&self) -> Self {
        Self {
            notify: self.notify.clone(),
        }
    }
}

pub struct ShutdownReceiver {
    notify: Arc<Notify>,
}

impl ShutdownReceiver {
    pub async fn wait(&self) {
        self.notify.notified().await;
    }
}

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
    let state = Arc::new(ServerState::new(storage.clone(), config.performance.max_queue_size));
    let metrics = Arc::new(metrics::Metrics::new());
    let rate_limiter = Arc::new(rate_limiter::RateLimiter::new(rate_limiter::RateLimitConfig {
        global_rps: config.rate_limit.global_rps, global_burst: config.rate_limit.global_burst,
        publish_rps: config.rate_limit.publish_rps, publish_burst: config.rate_limit.publish_burst,
        max_connections_per_ip: config.rate_limit.max_connections_per_ip,
        max_connections_per_cn: config.rate_limit.max_connections_per_cn,
    }));
    // Initialize ACL manager
    let acl_manager = acl::AclManager::new(
        config.acl.sender_cns.clone(),
        config.acl.receiver_cns.clone(),
        config.acl.admin_cns.clone(),
    );

    if config.admin.enabled {
        let admin_metrics = metrics.clone(); let admin_limiter = rate_limiter.clone(); let admin_state = state.clone();
        let admin_address = config.admin.address.clone();
        let admin_cert = config.tls.cert_path.clone().unwrap();
        let admin_key = config.tls.key_path.clone().unwrap();
        let admin_ca = config.tls.ca_cert_path.clone().unwrap();
        let allowed_cns = config.admin.allowed_cns.clone();
        let admin_acl = acl_manager.clone();
        tokio::spawn(async move { if let Err(e) = crate::admin_server::serve(admin_address, admin_cert, admin_key, admin_ca, allowed_cns, admin_metrics, admin_limiter, admin_state, admin_acl).await { error!("Admin server stopped: {}", e); } });
    }

    // Initialize heartbeat monitor
    let heartbeat_monitor = Arc::new(HeartbeatMonitor::new(config.performance.heartbeat_timeout));
    let heartbeat_monitor_clone = heartbeat_monitor.clone();
    tokio::spawn(async move {
        heartbeat_monitor_clone.monitor_task().await;
    });
    info!("Heartbeat monitor initialized with 60-second timeout");

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

    // Initialize shutdown signal
    let shutdown_signal = ShutdownSignal::wait_for_signal().await;
    let shutdown_receiver = shutdown_signal.subscribe();

    match config.server.connection_type.as_str() {
        "tls" => {
            let cert_path = config.tls.cert_path.ok_or("Missing cert_path for TLS")?;
            let key_path = config.tls.key_path.ok_or("Missing key_path for TLS")?;
            let ca_cert_path = config.tls.ca_cert_path.ok_or("Missing ca_cert_path for TLS")?;
            let auth_handler = MTlsAuth::new(&cert_path, &key_path, &ca_cert_path)?;

            // Main server loop with shutdown handling
            loop {
                tokio::select! {
                    // Handle incoming connections
                    result = listener.accept() => {
                        match result {
                            Ok((stream, addr)) => {
                                info!(client_addr = %addr, "New TLS connection established");
                                let state = state.clone();
                                let auth_handler = auth_handler.clone();
                                let heartbeat_monitor = heartbeat_monitor.clone();
                                let client_metrics = metrics.clone();
                                let client_rate_limiter = rate_limiter.clone();
                                let client_acl = acl_manager.clone();
                                tokio::spawn(async move {
                                    match auth_handler.authenticate(stream).await {
                                        Ok(tls_stream) => {
                                            let client_id = tls_stream.client_id().to_string();
                                            handle_client(tls_stream, state.clone(), heartbeat_monitor.clone(), client_metrics, client_rate_limiter, addr.ip().to_string(), client_acl).await;
                                            info!(client_addr = %addr, "Client connection closed");
                                            // Remove client from heartbeat monitor on disconnect
                                            heartbeat_monitor.remove_client(&client_id);
                                        }
                                        Err(e) => {
                                            error!(client_addr = %addr, error = %e, "TLS handshake failed");
                                            state.decrement_client_count();
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                error!(error = %e, "Failed to accept connection");
                            }
                        }
                    }
                    // Handle shutdown signal
                    _ = shutdown_receiver.wait() => {
                        info!("Shutting down server gracefully");
                        break;
                    }
                }
            }
            // Perform cleanup before exiting
            info!("Performing final cleanup");
            state.decrement_client_count(); // Ensure all clients are marked as disconnected
            info!("Server shutdown complete");
            Ok(())
        }
        _ => Err("Invalid connection_type in config".into()),
    }
}