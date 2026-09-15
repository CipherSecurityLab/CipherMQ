use anyhow::{Context, Result};
use axum::extract::State as AxumState;
use axum::http::Method;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::sync::{Arc, RwLock};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

mod poll;

// ─── Config ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
struct Config {
    server_name: String,
    poll_interval_secs: u64,
    web_address: String,
    tls: TlsCfg,
    server: ServerCfg,
}

#[derive(Debug, Deserialize, Clone)]
struct TlsCfg {
    ca_cert_path: String,
    client_cert_path: String,
    client_key_path: String,
}

#[derive(Debug, Deserialize, Clone)]
struct ServerCfg {
    admin_address: String,
}

// ─── Node data (mirrors server JSON exactly) ────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct NodeData {
    #[serde(default)]
    server: ServerInfo,
    #[serde(default)]
    counters: CounterData,
    #[serde(default)]
    snapshot: SnapshotData,
    #[serde(default)]
    live: LiveData,
    #[serde(default)]
    connection_stats: ConnStats,
    #[serde(default)]
    queues: Vec<QueueData>,
    #[serde(default)]
    bindings: Vec<BindingData>,
    #[serde(default)]
    exchanges: Vec<String>,
    #[serde(default)]
    reachable: bool,
    #[serde(default)]
    last_update: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ServerInfo {
    name: String,
    uptime_secs: u64,
    health: String,
    max_queue_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CounterData {
    messages_published: u64,
    messages_acked: u64,
    messages_delivered: u64,
    errors_total: u64,
    connections_total: u64,
    connections_rejected_rate_limit: u64,
    connections_rejected_conn_limit: u64,
    requests_rate_limited: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SnapshotData {
    connected_clients: usize,
    queue_count: usize,
    total_messages: usize,
    consumer_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LiveData {
    connected_clients: usize,
    queues: usize,
    total_messages: usize,
    total_consumers: usize,
    exchanges: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ConnStats {
    active_by_ip: usize,
    active_by_cn: usize,
    by_cn: HashMap<String, usize>,
    by_ip: HashMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct QueueData {
    name: String,
    message_count: usize,
    consumer_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct BindingData {
    exchange: String,
    queue: String,
    routing_key: String,
}

// ─── Shared state ────────────────────────────────────────────────

struct AppState {
    #[allow(dead_code)]
    config: Config,
    node: RwLock<NodeData>,
}

// ─── TLS setup ───────────────────────────────────────────────────

fn load_tls_config(tls: &TlsCfg) -> Result<Arc<ClientConfig>> {
    let ca_file = File::open(&tls.ca_cert_path)
        .context(format!("opening CA cert: {}", tls.ca_cert_path))?;
    let mut ca_reader = BufReader::new(ca_file);
    let ca_certs: Vec<CertificateDer> =
        rustls_pemfile::certs(&mut ca_reader).collect::<Result<Vec<_>, _>>()?;

    let mut root_store = RootCertStore::empty();
    for cert in ca_certs {
        root_store.add(cert)?;
    }

    let cert_file = File::open(&tls.client_cert_path).context("opening client cert")?;
    let mut cert_reader = BufReader::new(cert_file);
    let client_certs: Vec<CertificateDer> =
        rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;

    let key = load_key(&tls.client_key_path)?;

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(client_certs, key)
        .context("building TLS config")?;

    Ok(Arc::new(config))
}

fn load_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    {
        let mut r = BufReader::new(File::open(path)?);
        let mut keys: Vec<_> = rustls_pemfile::pkcs8_private_keys(&mut r)
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(key) = keys.pop() {
            return Ok(PrivateKeyDer::Pkcs8(key));
        }
    }
    {
        let mut r = BufReader::new(File::open(path)?);
        let mut keys: Vec<_> = rustls_pemfile::rsa_private_keys(&mut r)
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(key) = keys.pop() {
            return Ok(PrivateKeyDer::Pkcs1(key));
        }
    }
    {
        let mut r = BufReader::new(File::open(path)?);
        let mut keys: Vec<_> = rustls_pemfile::ec_private_keys(&mut r)
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(key) = keys.pop() {
            return Ok(PrivateKeyDer::Sec1(key));
        }
    }
    Err(anyhow::anyhow!("No private key found in {}", path))
}

// ─── HTTP handlers ───────────────────────────────────────────────

async fn serve_dashboard() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

async fn api_status(AxumState(state): AxumState<Arc<AppState>>) -> Response {
    let node = state.node.read().unwrap();
    axum::Json(node.clone()).into_response()
}

// ─── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.json".to_string());

    let config: Config = {
        let content = std::fs::read_to_string(&config_path)?;
        serde_json::from_str(&content)?
    };

    info!("Starting CipherMQ Admin Console");
    info!(
        "Server: {} at {}",
        config.server_name, config.server.admin_address
    );
    info!("Dashboard: http://{}/", config.web_address);

    let tls_config = load_tls_config(&config.tls)?;

    let state = Arc::new(AppState {
        config: config.clone(),
        node: RwLock::new(NodeData::default()),
    });

    let poll_state = state.clone();
    let poll_addr = config.server.admin_address.clone();
    let poll_interval = config.poll_interval_secs;
    std::thread::spawn(move || {
        poll::poll_loop(poll_addr, poll_interval, tls_config, poll_state);
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET])
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(serve_dashboard))
        .route("/api/status", get(api_status))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.web_address).await?;
    info!("Dashboard ready at http://{}/", config.web_address);

    axum::serve(listener, app).await?;

    Ok(())
}
