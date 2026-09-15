use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use blake2::{Blake2b, Digest};
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::sleep;
use tokio_rustls::TlsConnector;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use uuid::Uuid;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey};
use xsalsa20poly1305::aead::generic_array::GenericArray;
use xsalsa20poly1305::{XSalsa20Poly1305};

// ─── Config structs ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
struct Binding {
    queue_name: String,
    exchange_name: String,
    routing_key: String,
}

#[derive(Debug, Deserialize, Clone)]
struct TlsConfig {
    certificate_path: String,
    client_cert_path: String,
    client_key_path: String,
    check_hostname: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct LoggingConfig {
    level: String,
    info_file_path: String,
    #[allow(dead_code)]
    debug_file_path: String,
    #[allow(dead_code)]
    error_file_path: String,
    #[allow(dead_code)]
    rotation: String,
    #[allow(dead_code)]
    max_size_mb: u64,
}

/// Message generation and sending pipeline configuration.
/// All values that were previously hardcoded are defined here.
#[derive(Debug, Deserialize, Clone)]
struct SenderConfig {
    /// Total number of messages to send.
    num_messages: usize,

    /// Maximum number of messages in flight at the same time (in-flight window).
    max_inflight: usize,

    /// Maximum number of retry attempts for each message.
    max_retries: u32,

    /// ACK wait timeout in seconds.
    ack_timeout_secs: u64,

    /// TCP connection timeout in seconds.
    tcp_connect_timeout_secs: u64,

    /// Size of each batch for progress logging.
    batch_size: usize,

    /// Delay between batches in milliseconds.
    batch_delay_ms: u64,

    /// Message template configuration.
    message: MessageConfig,
}

impl Default for SenderConfig {
    fn default() -> Self {
        SenderConfig {
            num_messages: 100,
            max_inflight: 20,
            max_retries: 3,
            ack_timeout_secs: 30,
            tcp_connect_timeout_secs: 120,
            batch_size: 10,
            batch_delay_ms: 10,
            message: MessageConfig::default(),
        }
    }
}

/// Message content generation configuration.
///
/// The message template (`content_template`) may contain the following placeholders:
///   `{sender_id}`      — sender identity from the TLS certificate
///   `{correlation_id}` — a short UUID (8 characters)
///   `{timestamp}`      — Unix timestamp in seconds (decimal)
///   `{seq}`            — message sequence number in this run
///   Each custom key from `extra_fields` is also substituted as `{key}`.
#[derive(Debug, Deserialize, Clone)]
struct MessageConfig {
    /// Message text template. The default matches the previous hardcoded value.
    #[serde(default = "MessageConfig::default_template")]
    content_template: String,

    /// Additional custom fields to substitute in the template.
    #[serde(default)]
    extra_fields: HashMap<String, String>,
}

impl Default for MessageConfig {
    fn default() -> Self {
        MessageConfig {
            content_template: MessageConfig::default_template(),
            extra_fields: HashMap::new(),
        }
    }
}

impl MessageConfig {
    fn default_template() -> String {
        "{sender_id}-CipherMQ Sample message with ID: {correlation_id}".to_string()
    }

    /// Replaces placeholders in the template with actual values.
    fn render(
        &self,
        sender_id: &str,
        correlation_id: &str,
        timestamp: f64,
        seq: usize,
    ) -> String {
        let mut out = self.content_template.clone();
        out = out.replace("{sender_id}", sender_id);
        out = out.replace("{correlation_id}", correlation_id);
        out = out.replace("{timestamp}", &format!("{:.3}", timestamp));
        out = out.replace("{seq}", &seq.to_string());
        for (k, v) in &self.extra_fields {
            out = out.replace(&format!("{{{}}}", k), v);
        }
        out
    }
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    exchange_name: String,
    bindings: Vec<Binding>,
    server_address: String,
    server_port: u16,
    tls: TlsConfig,
    logging: LoggingConfig,
    #[serde(default)]
    receiver_client_ids: ReceiverIds,
    /// Default values are used when this is absent from config.json.
    #[serde(default)]
    sender: SenderConfig,
}

/// Accepts both `"receiver_1"` (string) and `["r1","r2"]` (array) formats.
#[derive(Debug, Clone)]
struct ReceiverIds(Vec<String>);

impl Default for ReceiverIds {
    fn default() -> Self {
        ReceiverIds(Vec::new())
    }
}

impl<'de> Deserialize<'de> for ReceiverIds {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(s) => Ok(ReceiverIds(vec![s])),
            serde_json::Value::Array(arr) => {
                let ids: Vec<String> = arr
                    .into_iter()
                    .map(|x| {
                        x.as_str()
                            .map(|s| s.to_string())
                            .ok_or_else(|| Error::custom("receiver_client_ids: expected string"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ReceiverIds(ids))
            }
            _ => Err(Error::custom(
                "receiver_client_ids must be string or array of strings",
            )),
        }
    }
}

// ─── Message structs ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PlainMessage {
    correlation_id: String,
    sender_id: String,
    sent_timestamp: f64,
    content: String,
}

#[derive(Debug, Clone, Serialize)]
struct EncryptedMessage {
    message_id: String,
    receiver_client_id: String,
    enc_session_key: String,
    nonce: String,
    ciphertext: String,
    sent_time: String,
    #[serde(skip)]
    routing_key: String,
}

// ─── Logging ───────────────────────────────────────────────────────────────

fn setup_logging(cfg: &LoggingConfig) -> Result<()> {
    for path in [&cfg.info_file_path, &cfg.debug_file_path, &cfg.error_file_path] {
        if let Some(p) = Path::new(path).parent() {
            std::fs::create_dir_all(p)?;
        }
    }
    let filter = EnvFilter::try_new(format!("sender={}", cfg.level.to_lowercase()))
        .unwrap_or_else(|_| EnvFilter::new("sender=info"));
    let console = fmt::layer().with_target(false).with_ansi(true);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.info_file_path)
        .with_context(|| format!("Cannot open log file {}", cfg.info_file_path))?;
    let file_layer = fmt::layer()
        .with_writer(Arc::new(log_file))
        .with_target(false)
        .with_ansi(false)
        .json();
    tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file_layer)
        .init();
    Ok(())
}

// ─── TLS ───────────────────────────────────────────────────────────────────

fn build_tls_connector(cfg: &TlsConfig) -> Result<TlsConnector> {
    use rustls::{ClientConfig, RootCertStore};
    use rustls_pemfile::{certs, pkcs8_private_keys};
    use std::fs::File;
    use std::io::BufReader as StdBufReader;

    let mut ca_reader = StdBufReader::new(
        File::open(&cfg.certificate_path)
            .with_context(|| format!("Cannot open CA cert {}", cfg.certificate_path))?,
    );
    let mut roots = RootCertStore::empty();
    for cert in certs(&mut ca_reader).collect::<Result<Vec<_>, _>>()? {
        roots.add(cert)?;
    }
    let mut cert_reader = StdBufReader::new(
        File::open(&cfg.client_cert_path)
            .with_context(|| format!("Cannot open client cert {}", cfg.client_cert_path))?,
    );
    let client_certs: Vec<_> = certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;
    let mut key_reader = StdBufReader::new(
        File::open(&cfg.client_key_path)
            .with_context(|| format!("Cannot open client key {}", cfg.client_key_path))?,
    );
    let mut keys = pkcs8_private_keys(&mut key_reader).collect::<Result<Vec<_>, _>>()?;
    let private_key = if let Some(key) = keys.pop() {
        rustls::pki_types::PrivateKeyDer::Pkcs8(key)
    } else {
        let mut reader = StdBufReader::new(File::open(&cfg.client_key_path)?);
        let mut rsa = rustls_pemfile::rsa_private_keys(&mut reader).collect::<Result<Vec<_>, _>>()?;
        if let Some(key) = rsa.pop() {
            rustls::pki_types::PrivateKeyDer::Pkcs1(key)
        } else {
            let mut reader = StdBufReader::new(File::open(&cfg.client_key_path)?);
            let mut ec = rustls_pemfile::ec_private_keys(&mut reader).collect::<Result<Vec<_>, _>>()?;
            rustls::pki_types::PrivateKeyDer::Sec1(ec.pop().ok_or_else(|| anyhow!("No private key found in {}", cfg.client_key_path))?)
        }
    };
    let mut tls_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(client_certs, private_key)?;
    if !cfg.check_hostname {
        tls_config
            .dangerous()
            .set_certificate_verifier(Arc::new(danger::NoHostnameVerifier::new()));
    }
    Ok(TlsConnector::from(Arc::new(tls_config)))
}

mod danger {
    use rustls::{
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        pki_types::{CertificateDer, ServerName, UnixTime},
        DigitallySignedStruct, SignatureScheme,
    };
    use std::sync::Arc;

    #[derive(Debug)]
    pub struct NoHostnameVerifier {
        inner: Arc<rustls::crypto::CryptoProvider>,
    }
    impl NoHostnameVerifier {
        pub fn new() -> Self {
            Self {
                inner: rustls::crypto::ring::default_provider().into(),
            }
        }
    }
    impl ServerCertVerifier for NoHostnameVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer,
            _intermediates: &[CertificateDer],
            _server_name: &ServerName,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            msg: &[u8],
            cert: &CertificateDer,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                msg,
                cert,
                dss,
                &self.inner.signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            msg: &[u8],
            cert: &CertificateDer,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                msg,
                cert,
                dss,
                &self.inner.signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.inner
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

// ─── Extract CN from client certificate ───────────────────────────────────

fn extract_client_id(tls_cfg: &TlsConfig) -> Result<String> {
    let pem = std::fs::read(&tls_cfg.client_cert_path)
        .with_context(|| format!("Cannot read cert {}", tls_cfg.client_cert_path))?;
    let (_, cert) = x509_parser::pem::parse_x509_pem(&pem)
        .map_err(|e| anyhow!("PEM parse error: {}", e))?;
    let x509 = cert
        .parse_x509()
        .map_err(|e| anyhow!("X.509 parse error: {}", e))?;
    for rdn in x509.subject().iter() {
        for attr in rdn.iter() {
            if attr.attr_type() == &x509_parser::oid_registry::OID_X509_COMMON_NAME {
                let val = attr
                    .attr_value()
                    .as_str()
                    .map_err(|e| anyhow!("CN is not UTF-8: {}", e))?;
                return Ok(val.to_string());
            }
        }
    }
    Err(anyhow!("No Common Name found in client certificate"))
}

// ─── NaCl SealedBox ENCRYPT ────────────────────────────────────────────────

fn sealed_box_encrypt(plaintext: &[u8], recipient_pub_bytes: &[u8; 32]) -> Result<Vec<u8>> {
    let epk_secret = EphemeralSecret::random_from_rng(rand::thread_rng());
    let epk_public = X25519PublicKey::from(&epk_secret);
    let rpk = X25519PublicKey::from(*recipient_pub_bytes);
    let shared = epk_secret.diffie_hellman(&rpk);
    let mut hasher = Blake2b::<blake2::digest::consts::U64>::new();
    Digest::update(&mut hasher, epk_public.as_bytes());
    Digest::update(&mut hasher, rpk.as_bytes());
    let hash = hasher.finalize();
    let box_nonce = GenericArray::clone_from_slice(&hash[..24]);
    let box_key = GenericArray::clone_from_slice(shared.as_bytes());
    let cipher = XSalsa20Poly1305::new(&box_key);
    let box_ct = cipher
        .encrypt(&box_nonce, plaintext)
        .map_err(|e| anyhow!("SealedBox encrypt failed: {}", e))?;
    let mut out = Vec::with_capacity(32 + box_ct.len());
    out.extend_from_slice(epk_public.as_bytes());
    out.extend_from_slice(&box_ct);
    Ok(out)
}

// ─── Message generation ────────────────────────────────────────────────────

/// Generates a plain message using `MessageConfig`.
/// The `seq` parameter is the sequence number in this run (starting at 1).
fn generate_message(client_id: &str, msg_cfg: &MessageConfig, seq: usize) -> PlainMessage {
    let correlation_id = Uuid::new_v4().to_string()[..8].to_string();
    let sent_timestamp = Utc::now().timestamp_millis() as f64 / 1000.0;
    let content = msg_cfg.render(client_id, &correlation_id, sent_timestamp, seq);
    PlainMessage {
        correlation_id,
        sender_id: client_id.to_string(),
        sent_timestamp,
        content,
    }
}

// ─── Encryption ────────────────────────────────────────────────────────────

fn encrypt_message(
    msg: &PlainMessage,
    public_key_b64: &str,
    receiver_client_id: &str,
    routing_key: &str,
) -> Result<EncryptedMessage> {
    let pub_bytes_vec = BASE64
        .decode(public_key_b64.trim())
        .context("Bad Base64 in receiver public key")?;
    let pub_bytes: [u8; 32] = pub_bytes_vec
        .try_into()
        .map_err(|_| anyhow!("Receiver public key must be 32 bytes"))?;
    let mut session_key = [0u8; 32];
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut session_key);
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let enc_session_key = sealed_box_encrypt(&session_key, &pub_bytes)?;
    let chacha_key = chacha20poly1305::Key::from_slice(&session_key);
    let cipher = ChaCha20Poly1305::new(chacha_key);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext_with_tag = cipher
        .encrypt(nonce, msg.content.as_bytes())
        .map_err(|e| anyhow!("ChaCha20 encrypt failed: {}", e))?;
    let message_id = format!(
        "{}-{}-{}",
        msg.sender_id, msg.correlation_id, receiver_client_id
    );
    let sent_time: DateTime<Utc> = Utc::now();
    debug!(
        "Hybrid encryption completed for {}: content_size={}, session_key_size={}",
        receiver_client_id,
        ciphertext_with_tag.len(),
        session_key.len()
    );
    Ok(EncryptedMessage {
        message_id,
        receiver_client_id: receiver_client_id.to_string(),
        enc_session_key: BASE64.encode(&enc_session_key),
        nonce: BASE64.encode(nonce_bytes),
        ciphertext: BASE64.encode(&ciphertext_with_tag),
        sent_time: sent_time.to_rfc3339(),
        routing_key: routing_key.to_string(),
    })
}

fn encrypt_for_all_receivers(
    msg: &PlainMessage,
    receiver_ids: &[String],
    routing_map: &HashMap<String, String>,
) -> Vec<EncryptedMessage> {
    let mut results = Vec::new();
    for receiver_id in receiver_ids {
        let key_path = format!("keys/{}_public.key", receiver_id);
        let pub_key_b64 = match std::fs::read_to_string(&key_path) {
            Ok(s) => s.trim().to_string(),
            Err(_) => {
                error!(
                    "Public key for {} not found at {}",
                    receiver_id, key_path
                );
                continue;
            }
        };
        let queue_name = format!("{}_queue", receiver_id);
        let routing_key = routing_map
            .get(&queue_name)
            .cloned()
            .unwrap_or_else(|| format!("{}_key", receiver_id));
        match encrypt_message(msg, &pub_key_b64, receiver_id, &routing_key) {
            Ok(em) => {
                info!(
                    "Encrypted message {} for {} with routing_key {}",
                    em.message_id, receiver_id, routing_key
                );
                results.push(em);
            }
            Err(e) => error!("Encryption failed for {}: {}", receiver_id, e),
        }
    }
    results
}

// ─── Protocol helpers ──────────────────────────────────────────────────────

async fn send_recv<W, R>(writer: &mut W, reader: &mut R, cmd: &str) -> Result<String>
where
    W: AsyncWriteExt + Unpin,
    R: AsyncBufReadExt + Unpin,
{
    writer.write_all(cmd.as_bytes()).await?;
    writer.flush().await?;
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    Ok(line.trim().to_string())
}

async fn configure_server<W, R>(
    writer: &mut W,
    reader: &mut R,
    bindings: &[Binding],
) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    R: AsyncBufReadExt + Unpin,
{
    for b in bindings {
        let cmd = format!("declare_queue {}\n", b.queue_name);
        debug!("Sending command: {}", cmd.trim());
        let r = send_recv(writer, reader, &cmd).await?;
        info!("Server response for queue declaration: {}", r);

        let cmd = format!("declare_exchange {}\n", b.exchange_name);
        debug!("Sending command: {}", cmd.trim());
        let r = send_recv(writer, reader, &cmd).await?;
        info!("Server response for exchange declaration: {}", r);

        let cmd = format!(
            "bind {} {} {}\n",
            b.queue_name, b.exchange_name, b.routing_key
        );
        debug!("Sending command: {}", cmd.trim());
        let r = send_recv(writer, reader, &cmd).await?;
        info!("Server response for binding: {}", r);
    }
    Ok(())
}

async fn get_public_key<W, R>(
    writer: &mut W,
    reader: &mut R,
    client_id: &str,
) -> Result<Option<String>>
where
    W: AsyncWriteExt + Unpin,
    R: AsyncBufReadExt + Unpin,
{
    let cmd = format!("get_public_key {}\n", client_id);
    debug!("Sending command: {}", cmd.trim());
    let response = send_recv(writer, reader, &cmd).await?;
    if let Some(key) = response.strip_prefix("Public key: ") {
        Ok(Some(key.to_string()))
    } else if response == "Public key not found" {
        warn!("Public key not found for {}", client_id);
        Ok(None)
    } else {
        error!("Error getting public key: {}", response);
        Ok(None)
    }
}

// ─── TLS connection helper ─────────────────────────────────────────────────

type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;

async fn connect_tls(cfg: &Config, connector: &TlsConnector) -> Result<TlsStream> {
    let addr = format!("{}:{}", cfg.server_address, cfg.server_port);
    let timeout = Duration::from_secs(cfg.sender.tcp_connect_timeout_secs);
    let tcp = tokio::time::timeout(timeout, TcpStream::connect(&addr))
        .await
        .context("TCP connect timeout")?
        .with_context(|| format!("TCP connect to {} failed", addr))?;

    // Read server_name from the config; the verifier ignores it when check_hostname=false.
    // Clone the String so ServerName owns the data and its lifetime is not tied to cfg.
    let server_name: rustls::pki_types::ServerName<'static> =
        rustls::pki_types::ServerName::try_from(cfg.server_address.clone())
            .unwrap_or_else(|_| {
                rustls::pki_types::ServerName::try_from("localhost".to_string())
                    .expect("Invalid server name")
            });
    let tls = connector
        .connect(server_name, tcp)
        .await
        .context("TLS handshake failed")?;
    Ok(tls)
}

// ─── Fetch public keys from server ─────────────────────────────────────────

async fn fetch_all_public_keys(
    cfg: &Config,
    connector: &TlsConnector,
) -> HashMap<String, String> {
    let max_retries = cfg.sender.max_retries;
    for attempt in 0..max_retries {
        match fetch_all_public_keys_once(cfg, connector).await {
            Ok(map) if !map.is_empty() => return map,
            Ok(_) => warn!(
                "No public keys returned on attempt {}/{}",
                attempt + 1,
                max_retries
            ),
            Err(e) => {
                let wait = 2u64.pow(attempt);
                warn!(
                    "Failed to fetch public keys on attempt {}/{}: {}. Retrying in {}s",
                    attempt + 1,
                    max_retries,
                    e,
                    wait
                );
                sleep(Duration::from_secs(wait)).await;
            }
        }
    }
    error!("No valid public keys fetched after all retries");
    HashMap::new()
}

async fn fetch_all_public_keys_once(
    cfg: &Config,
    connector: &TlsConnector,
) -> Result<HashMap<String, String>> {
    let stream = connect_tls(cfg, connector).await?;
    let cipher = stream.get_ref().1.negotiated_cipher_suite();
    info!(
        "TLS connection established for fetching public keys. Cipher: {:?}",
        cipher
    );
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    configure_server(&mut write_half, &mut reader, &cfg.bindings).await?;
    let mut keys = HashMap::new();
    for receiver_id in &cfg.receiver_client_ids.0 {
        match get_public_key(&mut write_half, &mut reader, receiver_id).await? {
            Some(key) => {
                let path = format!("keys/{}_public.key", receiver_id);
                if let Err(e) = std::fs::write(&path, &key) {
                    error!(
                        "Failed to save public key for {} to {}: {}",
                        receiver_id, path, e
                    );
                } else {
                    info!("Saved public key for {} to {}", receiver_id, path);
                }
                keys.insert(receiver_id.clone(), key);
            }
            None => warn!("Skipping {} due to missing public key", receiver_id),
        }
    }
    write_half.shutdown().await.ok();
    Ok(keys)
}

// ─── Async pipeline: send messages and receive ACKs asynchronously ────────────
//
// Architecture:
//   • One sender task writes messages at full speed.
//   • One ACK reader task reads responses and returns results through a channel.
//   • A Semaphore with capacity `max_inflight` controls concurrent in-flight messages.
//   • A pending map stores messages that have not received an ACK (for retry).

/// Result of each ACK sent from the reader task to the main sending task.
enum AckResult {
    /// Successful ACK for the specified message_id.
    Ok(String),
    /// Error response from the server for the specified message_id.
    ServerError(String),
    /// Unknown or unexpected message.
    Unknown(String),
    /// Error reading from the connection (the connection was likely closed).
    IoError(String),
}

/// Asynchronous sending with an in-flight window.
///
/// Overall flow:
///   1. Acquire one Semaphore permit for each message.
///   2. Write the message without waiting for an ACK.
///   3. Store the message in the pending map.
///   4. The ACK reader task reads ACKs concurrently.
///   5. When an ACK arrives, release the permit and remove the message from pending.
///   6. After all sends complete, retry messages without ACKs.
async fn send_messages_async_pipeline(
    cfg: &Config,
    connector: &TlsConnector,
    client_id: &str,
) -> Result<()> {
    let scfg = &cfg.sender;

    // ─── Fetch public keys ───────────────────────────────────────────────────
    let mut public_keys = fetch_all_public_keys(cfg, connector).await;
    if public_keys.is_empty() {
        warn!("No public keys from server. Trying local files");
        for id in &cfg.receiver_client_ids.0 {
            let path = format!("keys/{}_public.key", id);
            if let Ok(key) = std::fs::read_to_string(&path) {
                public_keys.insert(id.clone(), key.trim().to_string());
            }
        }
    }
    if public_keys.is_empty() {
        error!("No valid public keys available. Exiting");
        return Ok(());
    }
    let receiver_ids: Vec<String> = public_keys.keys().cloned().collect();
    info!("Using receiver_client_ids: {:?}", receiver_ids);

    let routing_map: HashMap<String, String> = cfg
        .bindings
        .iter()
        .map(|b| (b.queue_name.clone(), b.routing_key.clone()))
        .collect();

    // ─── Open TLS connection ─────────────────────────────────────────────────
    let stream = connect_tls(cfg, connector).await?;
    let cipher = stream.get_ref().1.negotiated_cipher_suite();
    info!("TLS connection established. Cipher: {:?}", cipher);

    let (read_half, write_half) = tokio::io::split(stream);
    let reader = BufReader::new(read_half);

    // Put the writer in a Mutex so the sender task can write safely.
    let writer = Arc::new(Mutex::new(write_half));

    // Configure the server with the direct writer.
    {
        let mut w = writer.lock().await;
        let mut tmp_reader = reader; // Temporarily used for configuration.
        configure_server(&mut *w, &mut tmp_reader, &cfg.bindings).await?;
        // Return the reader for ACK reading.
        // (This block releases it from scope.)
        drop(tmp_reader);
    }

    // Separate reader for the ACK task.
    let stream2 = connect_tls(cfg, connector).await?;
    let (read_half2, write_half2) = tokio::io::split(stream2);
    let ack_reader = BufReader::new(read_half2);
    let ack_writer = Arc::new(Mutex::new(write_half2));

    // Configure the second server connection (ACK reader connection).
    {
        let mut w = ack_writer.lock().await;
        let mut tmp_r = ack_reader;
        configure_server(&mut *w, &mut tmp_r, &cfg.bindings).await?;
        drop(tmp_r);
    }

    // ─── Channel for ACK results ─────────────────────────────────────────────
    // Channel capacity equals max_inflight so the sender task does not block.
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<AckResult>(scfg.max_inflight * 2);

    // ─── Semaphore: control the number of concurrent in-flight messages ──────
    let semaphore = Arc::new(Semaphore::new(scfg.max_inflight));

    // ─── Pending map: message_id -> EncryptedMessage for retry ───────────────
    let pending: Arc<Mutex<HashMap<String, EncryptedMessage>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // ─── Start the ACK reader task ───────────────────────────────────────────
    // This task independently reads ACKs from the second connection.
    let ack_timeout = Duration::from_secs(scfg.ack_timeout_secs);
    let ack_tx_clone = ack_tx.clone();

    // ACK reader on the second connection (we cannot split one connection
    // between two tasks, but we can give the first connection's read_half to the ACK task).
    //
    // ─── Correct design: split the first connection between writer and ACK reader tasks ───
    //
    // First connection: write_half for sending, read_half for reading ACKs.
    // Both can operate concurrently because the connection has been split.

    // Reconnect and split the new connection correctly.
    let stream_main = connect_tls(cfg, connector).await?;
    let cipher_main = stream_main.get_ref().1.negotiated_cipher_suite();
    info!(
        "Main pipeline TLS connection established. Cipher: {:?}",
        cipher_main
    );

    let (main_read, main_write) = tokio::io::split(stream_main);
    let main_writer = Arc::new(Mutex::new(main_write));
    let mut main_reader = BufReader::new(main_read);

    // Configure the main connection.
    {
        let mut w = main_writer.lock().await;
        configure_server(&mut *w, &mut main_reader, &cfg.bindings).await?;
    }

    // Give main_reader to the ACK task.
    let ack_task = {
        let ack_tx = ack_tx_clone;
        let sem = semaphore.clone();
        let pending_map = pending.clone();
        tokio::spawn(async move {
            let mut reader = main_reader;
            loop {
                let mut line = String::new();
                let read_result =
                    tokio::time::timeout(ack_timeout, reader.read_line(&mut line)).await;
                match read_result {
                    Err(_elapsed) => {
                        // Timeout — release the channel so the main task knows.
                        // (This is not a global timeout; each message has its own timeout below.)
                        // Stop here so the channel can be closed.
                        debug!("ACK reader global timeout, stopping");
                        break;
                    }
                    Ok(Err(e)) => {
                        let _ = ack_tx.send(AckResult::IoError(e.to_string())).await;
                        break;
                    }
                    Ok(Ok(0)) => {
                        // EOF — the connection was closed.
                        debug!("ACK reader: connection closed (EOF)");
                        break;
                    }
                    Ok(Ok(_)) => {
                        let response = line.trim().to_string();
                        debug!("ACK reader received: {}", response);

                        let result = if let Some(id) = response.strip_prefix("ACK ") {
                            // Successful ACK: release the permit and remove it from pending.
                            pending_map.lock().await.remove(id);
                            sem.add_permits(1);
                            AckResult::Ok(id.to_string())
                        } else if response.starts_with("Error:") {
                            AckResult::ServerError(response)
                        } else {
                            AckResult::Unknown(response)
                        };

                        if ack_tx.send(result).await.is_err() {
                            break; // The receiver on the other side of the channel was closed.
                        }
                    }
                }
            }
        })
    };

    // ─── Main sending loop ───────────────────────────────────────────────────
    let start = Instant::now();
    let num_messages = scfg.num_messages;
    let batch_size = scfg.batch_size;
    let batch_delay = Duration::from_millis(scfg.batch_delay_ms);
    let mut sent_count = 0usize;
    let mut failed_ids: Vec<String> = Vec::new();

    let mut batch_num = 0usize;
    let mut i = 0usize;

    while i < num_messages {
        let batch_end = std::cmp::min(i + batch_size, num_messages);
        batch_num += 1;
        info!(
            "Sending batch {}: messages {}-{}",
            batch_num,
            i + 1,
            batch_end
        );

        while i < batch_end {
            let msg = generate_message(client_id, &scfg.message, i + 1);
            let encrypted = encrypt_for_all_receivers(&msg, &receiver_ids, &routing_map);

            for em in encrypted {
                // Acquire one semaphore permit; wait here if the limit is reached.
                // (This is our backpressure mechanism.)
                let _permit = semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("Semaphore closed");

                // Prepare the publish command.
                let payload = serde_json::json!({
                    "message_id":         em.message_id,
                    "ciphertext":         em.ciphertext,
                    "receiver_client_id": em.receiver_client_id,
                    "enc_session_key":    em.enc_session_key,
                    "nonce":              em.nonce,
                    "sent_time":          em.sent_time,
                });
                let payload_str = serde_json::to_string(&payload).unwrap_or_default();
                let command = format!(
                    "publish {} {} {}\n",
                    cfg.exchange_name, em.routing_key, payload_str
                );

                // Store the message in the pending map before sending.
                {
                    let mut map = pending.lock().await;
                    map.insert(em.message_id.clone(), em.clone());
                }

                debug!("Sending message {} (async)", em.message_id);

                // Send without waiting for an ACK.
                let mut w = main_writer.lock().await;
                if let Err(e) = w.write_all(command.as_bytes()).await {
                    error!("Write error for message {}: {}", em.message_id, e);
                    pending.lock().await.remove(&em.message_id);
                    failed_ids.push(em.message_id.clone());
                    // Release the permit because the ACK reader will not release it.
                    semaphore.add_permits(1);
                    continue;
                }
                if let Err(e) = w.flush().await {
                    error!("Flush error for message {}: {}", em.message_id, e);
                    pending.lock().await.remove(&em.message_id);
                    failed_ids.push(em.message_id.clone());
                    semaphore.add_permits(1);
                    continue;
                }

                // Transfer ownership of the permit; the ACK reader will release it.
                // (_permit would otherwise be dropped here and release another permit.)
                // Prevent double release by forgetting the permit;
                // the ACK reader is responsible for releasing it.
                std::mem::forget(_permit);

                sent_count += 1;
            }
            i += 1;
        }

        // Process ACK results received so far (non-blocking).
        while let Ok(result) = ack_rx.try_recv() {
            match result {
                AckResult::Ok(id) => info!("ACK received for {}", id),
                AckResult::ServerError(e) => {
                    error!("Server error: {}", e);
                }
                AckResult::Unknown(r) => warn!("Unknown response: {}", r),
                AckResult::IoError(e) => {
                    error!("IO error in ACK reader: {}", e);
                }
            }
        }

        if batch_end < num_messages {
            debug!(
                "Batch {} completed, waiting {:?} before next batch",
                batch_num, batch_delay
            );
            sleep(batch_delay).await;
        }
    }

    // ─── Wait for all remaining ACKs ─────────────────────────────────────────
    info!(
        "All {} messages sent. Waiting for remaining ACKs...",
        sent_count
    );
    let drain_timeout = Duration::from_secs(scfg.ack_timeout_secs + 5);
    let drain_start = Instant::now();

    loop {
        if pending.lock().await.is_empty() {
            break;
        }
        if drain_start.elapsed() > drain_timeout {
            warn!("Drain timeout reached; some ACKs may be missing");
            break;
        }
        match tokio::time::timeout(Duration::from_millis(100), ack_rx.recv()).await {
            Ok(Some(result)) => match result {
                AckResult::Ok(id) => info!("ACK received for {}", id),
                AckResult::ServerError(e) => error!("Server error: {}", e),
                AckResult::Unknown(r) => warn!("Unknown response: {}", r),
                AckResult::IoError(e) => {
                    error!("IO error in ACK reader: {}", e);
                    break;
                }
            },
            Ok(None) => break, // Channel closed.
            Err(_) => {}       // Short timeout; continue.
        }
    }

    // ─── Collect messages without ACKs and retry ──────────────────────────────
    let unacked: Vec<EncryptedMessage> = {
        let map = pending.lock().await;
        map.values().cloned().collect()
    };
    if !unacked.is_empty() {
        warn!("{} messages without ACK — starting retry", unacked.len());
        for em in &unacked {
            failed_ids.push(em.message_id.clone());
        }
    }

    // Retry with simple synchronous logic (there are few previously failed messages).
    let mut final_failed = 0usize;
    for em in &unacked {
        let mut success = false;
        for attempt in 0..scfg.max_retries {
            let payload = serde_json::json!({
                "message_id":         em.message_id,
                "ciphertext":         em.ciphertext,
                "receiver_client_id": em.receiver_client_id,
                "enc_session_key":    em.enc_session_key,
                "nonce":              em.nonce,
                "sent_time":          em.sent_time,
            });
            let payload_str = serde_json::to_string(&payload).unwrap_or_default();
            let command = format!(
                "publish {} {} {}\n",
                cfg.exchange_name, em.routing_key, payload_str
            );
            info!(
                "Retry {}/{} for message {}",
                attempt + 1,
                scfg.max_retries,
                em.message_id
            );
            {
                let mut w = main_writer.lock().await;
                if w.write_all(command.as_bytes()).await.is_err()
                    || w.flush().await.is_err()
                {
                    let wait = 2u64.pow(attempt);
                    sleep(Duration::from_secs(wait)).await;
                    continue;
                }
            }
            // Wait for an ACK with a timeout.
            match tokio::time::timeout(
                Duration::from_secs(scfg.ack_timeout_secs),
                ack_rx.recv(),
            )
            .await
            {
                Ok(Some(AckResult::Ok(id))) if id == em.message_id => {
                    info!("Retry ACK received for {}", em.message_id);
                    success = true;
                    break;
                }
                _ => {
                    let wait = 2u64.pow(attempt);
                    sleep(Duration::from_secs(wait)).await;
                }
            }
        }
        if !success {
            error!(
                "Message {} permanently failed after {} retries",
                em.message_id, scfg.max_retries
            );
            final_failed += 1;
        }
    }

    // ─── Shutdown ────────────────────────────────────────────────────────────
    drop(ack_tx); // Close the channel so the ACK task can exit.
    ack_task.await.ok();
    main_writer.lock().await.shutdown().await.ok();

    let elapsed = start.elapsed().as_secs_f64();
    info!(
        "Pipeline complete: sent={}, elapsed={:.2}s, throughput={:.1} msg/s, \
         permanent_failures={}, unacked_before_retry={}",
        sent_count,
        elapsed,
        sent_count as f64 / elapsed,
        final_failed,
        unacked.len(),
    );

    Ok(())
}

// ─── Entry point ───────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    for dir in ["logs", "keys"] {
        std::fs::create_dir_all(dir)?;
    }

    let config_str = std::fs::read_to_string("config.json")
        .context("Configuration file 'config.json' not found")?;
    let config: Config =
        serde_json::from_str(&config_str).context("Failed to parse config.json")?;

    setup_logging(&config.logging)?;

    let client_id = extract_client_id(&config.tls)?;
    info!("Extracted client_id from certificate: {}", client_id);
    info!(
        "Sender config: num_messages={}, max_inflight={}, max_retries={}, \
         ack_timeout={}s, template={:?}",
        config.sender.num_messages,
        config.sender.max_inflight,
        config.sender.max_retries,
        config.sender.ack_timeout_secs,
        config.sender.message.content_template,
    );

    let connector = build_tls_connector(&config.tls)?;

    info!("Starting async pipeline sender");
    send_messages_async_pipeline(&config, &connector, &client_id).await?;

    Ok(())
}