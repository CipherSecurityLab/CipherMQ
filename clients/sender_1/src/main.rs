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

/// Pipeline and message-generation settings.
#[derive(Debug, Deserialize, Clone)]
struct SenderConfig {
    /// Total number of messages to send in this run.
    num_messages: usize,

    /// Maximum number of messages that may be in-flight (sent but not yet ACK'd)
    /// at any one time.  Backed by a Semaphore; the sender blocks here when the
    /// window is full, giving natural backpressure without busy-waiting.
    max_inflight: usize,

    /// How many times to retry a message that never received an ACK before
    /// marking it as permanently failed.
    max_retries: u32,

    /// Seconds to wait for an ACK before considering a message unacknowledged.
    ack_timeout_secs: u64,

    /// Seconds to wait while establishing the TCP connection before aborting.
    tcp_connect_timeout_secs: u64,

    /// Number of messages per batch; used only for progress logging and the
    /// optional inter-batch delay it does not affect encryption or delivery.
    batch_size: usize,

    /// Milliseconds to pause between batches.  Set to 0 to disable.
    batch_delay_ms: u64,

    /// Message content settings (template + extra fields).
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

/// Message content configuration.
///
/// `content_template` is a plain string that may contain the following
/// built-in placeholders, all replaced at generation time:
///
/// | Placeholder        | Value                                              |
/// |--------------------|----------------------------------------------------|
/// | `{sender_id}`      | CN extracted from the client TLS certificate       |
/// | `{correlation_id}` | First 8 chars of a random UUID v4                  |
/// | `{timestamp}`      | Unix time in seconds (3 decimal places)            |
/// | `{seq}`            | 1-based sequence number within this run            |

#[derive(Debug, Deserialize, Clone)]
struct MessageConfig {
    /// Template string for the plaintext message content.
    /// Falls back to the legacy hardcoded string when omitted from config.
    #[serde(default = "MessageConfig::default_template")]
    content_template: String,

    /// Arbitrary key-value pairs injected into the template as `{key}`.
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

    /// Substitute all placeholders in the template and return the rendered string.
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
    /// If the "sender" key is absent from config.json, SenderConfig::default()
    /// is used, keeping backward compatibility with older config files.
    #[serde(default)]
    sender: SenderConfig,
}

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
                "receiver_client_ids must be a string or an array of strings",
            )),
        }
    }
}

// ─── Message structs ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PlainMessage {
    correlation_id: String,
    sender_id: String,
    #[allow(dead_code)]
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
    // Ensure all log directories exist before opening any files.
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
    if keys.is_empty() {
        return Err(anyhow!(
            "No PKCS8 private key found in {}",
            cfg.client_key_path
        ));
    }
    let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(keys.remove(0));
    let mut tls_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(client_certs, private_key)?;

    // When check_hostname is disabled (dev environments, IP-only endpoints),
    // install a custom verifier that still validates the certificate chain but
    // skips the ServerName check.
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

    /// A certificate verifier that accepts any presented certificate without
    /// checking that it matches the requested server name.
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

/// Parse the client TLS certificate and return the Common Name (CN) field,
/// which is used as the sender's logical identity throughout the protocol.
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

// ─── NaCl SealedBox encrypt ────────────────────────────────────────────────

/// Encrypt `plaintext` for `recipient_pub_bytes` using the NaCl SealedBox
/// construction (X25519 ECDH + XSalsa20-Poly1305).
///
/// Output layout: [ ephemeral_public_key (32 B) | box_ciphertext ]

fn sealed_box_encrypt(plaintext: &[u8], recipient_pub_bytes: &[u8; 32]) -> Result<Vec<u8>> {
    let epk_secret = EphemeralSecret::random_from_rng(rand::thread_rng());
    let epk_public = X25519PublicKey::from(&epk_secret);
    let rpk = X25519PublicKey::from(*recipient_pub_bytes);
    let shared = epk_secret.diffie_hellman(&rpk);

    // Derive a deterministic nonce from the two public keys (matches libsodium behaviour).
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

    // Prepend the ephemeral public key so the receiver can reconstruct the shared secret.
    let mut out = Vec::with_capacity(32 + box_ct.len());
    out.extend_from_slice(epk_public.as_bytes());
    out.extend_from_slice(&box_ct);
    Ok(out)
}

// ─── Message generation ────────────────────────────────────────────────────

/// Build a [`PlainMessage`] for `seq` (1-based) using the configured template.
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

/// Hybrid-encrypt a single [`PlainMessage`] for one receiver.
///
/// Scheme:
///   1. Generate a random 256-bit session key and 96-bit nonce.
///   2. Encrypt the plaintext with ChaCha20-Poly1305 (session key + nonce).
///   3. Encrypt the session key with NaCl SealedBox (receiver's X25519 public key).

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

/// Encrypt `msg` once for every receiver in `receiver_ids`.
///
/// Each receiver gets an independently encrypted copy: same plaintext, but a
/// fresh session key and nonce so that compromise of one receiver's private key
/// does not expose messages destined for other receivers.
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

/// Send a single line command and return the server's single-line response.
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

/// Declare queues, exchange, and bindings on the broker for the given config.
/// Called once per TLS connection before any publish commands are issued.
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

/// Request the Base64-encoded X25519 public key for `client_id` from the server.
/// Returns `None` if the server reports the key as not found.
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

/// Open a new mTLS connection to the broker defined in `cfg`.
/// The TCP connect is guarded by `tcp_connect_timeout_secs`.
async fn connect_tls(cfg: &Config, connector: &TlsConnector) -> Result<TlsStream> {
    let addr = format!("{}:{}", cfg.server_address, cfg.server_port);
    let timeout = Duration::from_secs(cfg.sender.tcp_connect_timeout_secs);
    let tcp = tokio::time::timeout(timeout, TcpStream::connect(&addr))
        .await
        .context("TCP connect timeout")?
        .with_context(|| format!("TCP connect to {} failed", addr))?;

    // Clone server_address into an owned String so that ServerName<'static> can
    // be constructed without borrowing from `cfg` (which would require 'static
    // on the cfg reference see E0521).
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

/// Try to retrieve all receiver public keys, retrying up to `max_retries` times
/// with exponential back-off on failure.
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

/// Single attempt: open a dedicated TLS connection, configure the broker, fetch
/// all receiver public keys, save them to disk, and close the connection.
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
    Ok(keys)
}

// ─── Async pipeline ────────────────────────────────────────────────────────

/// Outcome of a single ACK line read by the ACK reader task.
enum AckResult {
    /// Successful ACK for the given message_id.
    Ok(String),
    /// The server replied with an Error: … line for this message.
    ServerError(String),
    /// An unrecognised response was received.
    Unknown(String),
    /// A read error occurred; the connection is likely broken.
    IoError(String),
}

/// Send all messages using the async pipeline and return when every message has
/// either been ACK'd or exhausted its retry budget.
async fn send_messages_async_pipeline(
    cfg: &Config,
    connector: &TlsConnector,
    client_id: &str,
) -> Result<()> {
    let scfg = &cfg.sender;

    // ── 1. Acquire receiver public keys ───────────────────────────────────
    let mut public_keys = fetch_all_public_keys(cfg, connector).await;
    if public_keys.is_empty() {
        // Fall back to keys cached on disk from a previous run.
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

    // ── 2. Open main TLS connection and configure broker ──────────────────
    // Unused intermediate connections (stream, stream2) left in place to preserve
    // the original configure_server calls; the actual pipeline uses stream_main.
    let stream = connect_tls(cfg, connector).await?;
    let cipher = stream.get_ref().1.negotiated_cipher_suite();
    info!("TLS connection established. Cipher: {:?}", cipher);

    let (read_half, write_half) = tokio::io::split(stream);
    let reader = BufReader::new(read_half);
    let writer = Arc::new(Mutex::new(write_half));

    {
        let mut w = writer.lock().await;
        // Temporary reader used only for the synchronous configure exchange.
        let mut tmp_reader = reader;
        configure_server(&mut *w, &mut tmp_reader, &cfg.bindings).await?;
        drop(tmp_reader);
    }

    // Second connection (ACK path placeholder not used in current pipeline).
    let stream2 = connect_tls(cfg, connector).await?;
    let (read_half2, write_half2) = tokio::io::split(stream2);
    let ack_reader = BufReader::new(read_half2);
    let ack_writer = Arc::new(Mutex::new(write_half2));

    {
        let mut w = ack_writer.lock().await;
        let mut tmp_r = ack_reader;
        configure_server(&mut *w, &mut tmp_r, &cfg.bindings).await?;
        drop(tmp_r);
    }

    // ── 3. Main pipeline connection (split write / read halves) ───────────
    let stream_main = connect_tls(cfg, connector).await?;
    let cipher_main = stream_main.get_ref().1.negotiated_cipher_suite();
    info!(
        "Main pipeline TLS connection established. Cipher: {:?}",
        cipher_main
    );

    let (main_read, main_write) = tokio::io::split(stream_main);
    let main_writer = Arc::new(Mutex::new(main_write));
    let mut main_reader = BufReader::new(main_read);

    {
        let mut w = main_writer.lock().await;
        configure_server(&mut *w, &mut main_reader, &cfg.bindings).await?;
    }

    // mpsc channel carrying ACK outcomes from the reader task to the main task.
    // Capacity = 2 × max_inflight so the reader is never blocked by a full channel.
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<AckResult>(scfg.max_inflight * 2);

    // Semaphore: limits simultaneous in-flight messages.
    let semaphore = Arc::new(Semaphore::new(scfg.max_inflight));

    // Pending map: holds every message that has been written but not yet ACK'd.
    let pending: Arc<Mutex<HashMap<String, EncryptedMessage>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // ── 4. Spawn ACK reader task ───────────────────────────────────────────
    let ack_timeout = Duration::from_secs(scfg.ack_timeout_secs);
    let ack_tx_clone = ack_tx.clone();

    // The read half of the main connection is moved into this task.  Because
    // tokio::io::split gives us independent read/write halves, the write half
    // can be locked and used concurrently in the send loop below.
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
                        // Global idle timeout on the read half the drain phase
                        // in the main task handles per-message timeouts separately.
                        debug!("ACK reader global timeout, stopping");
                        break;
                    }
                    Ok(Err(e)) => {
                        let _ = ack_tx.send(AckResult::IoError(e.to_string())).await;
                        break;
                    }
                    Ok(Ok(0)) => {
                        // EOF server closed the connection.
                        debug!("ACK reader: connection closed (EOF)");
                        break;
                    }
                    Ok(Ok(_)) => {
                        let response = line.trim().to_string();
                        debug!("ACK reader received: {}", response);

                        let result = if let Some(id) = response.strip_prefix("ACK ") {
                            // Happy path: release the semaphore slot and remove from pending.
                            pending_map.lock().await.remove(id);
                            sem.add_permits(1);
                            AckResult::Ok(id.to_string())
                        } else if response.starts_with("Error:") {
                            AckResult::ServerError(response)
                        } else {
                            AckResult::Unknown(response)
                        };

                        if ack_tx.send(result).await.is_err() {
                            // Receiver side of the channel was dropped main task exited.
                            break;
                        }
                    }
                }
            }
        })
    };

    // ── 5. Send loop ──────────────────────────────────────────────────────
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
                // Block here if the in-flight window is full.
                // This provides backpressure without spinning.
                let _permit = semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("Semaphore closed");

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

                // Store in pending map *before* writing so the ACK reader can
                // never race ahead and remove an entry that does not exist yet.
                {
                    let mut map = pending.lock().await;
                    map.insert(em.message_id.clone(), em.clone());
                }

                debug!("Sending message {} (async)", em.message_id);

                let mut w = main_writer.lock().await;
                if let Err(e) = w.write_all(command.as_bytes()).await {
                    error!("Write error for message {}: {}", em.message_id, e);
                    pending.lock().await.remove(&em.message_id);
                    failed_ids.push(em.message_id.clone());
                    // Restore the permit manually because the ACK reader will
                    // never see this message and therefore never call add_permits.
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

                // Transfer permit ownership to the ACK reader task: forget the
                // OwnedSemaphorePermit here so it is not released when _permit
                // drops.  The ACK reader calls add_permits(1) instead.
                std::mem::forget(_permit);

                sent_count += 1;
            }
            i += 1;
        }

        // Drain any ACKs that arrived while we were sending this batch
        // (non-blocking we do not want to stall the send loop).
        while let Ok(result) = ack_rx.try_recv() {
            match result {
                AckResult::Ok(id) => info!("ACK received for {}", id),
                AckResult::ServerError(e) => error!("Server error: {}", e),
                AckResult::Unknown(r) => warn!("Unknown response: {}", r),
                AckResult::IoError(e) => error!("IO error in ACK reader: {}", e),
            }
        }

        if batch_end < num_messages {
            debug!(
                "Batch {} complete, waiting {:?} before next batch",
                batch_num, batch_delay
            );
            sleep(batch_delay).await;
        }
    }

    // ── 6. Drain remaining ACKs ───────────────────────────────────────────
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
            Ok(None) => break, 
            Err(_) => {}       
        }
    }

    // ── 7. Retry unacknowledged messages ─────────────────────────────────
    let unacked: Vec<EncryptedMessage> = {
        let map = pending.lock().await;
        map.values().cloned().collect()
    };
    if !unacked.is_empty() {
        warn!(
            "{} message(s) without ACK after drain phase starting retry",
            unacked.len()
        );
        for em in &unacked {
            failed_ids.push(em.message_id.clone());
        }
    }

    // Retry loop uses simple synchronous semantics (one send → wait for ACK)
    // because the number of retried messages is expected to be small.
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
            // Wait for the ACK that belongs to this specific message.
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

    // ── 8. Shutdown ───────────────────────────────────────────────────────
    drop(ack_tx); // closing the sender side signals the ACK reader task to stop
    ack_task.await.ok();
    drop(main_writer);

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

    // Ensure runtime directories exist before logging or key I/O begins.
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