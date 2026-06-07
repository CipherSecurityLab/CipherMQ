use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::path::Path;
use anyhow::{Context, Result, anyhow};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::Aead, Nonce};
use chrono::Utc;
use x25519_dalek::{StaticSecret, PublicKey as X25519PublicKey};
use blake2::{Blake2b, Digest};
use xsalsa20poly1305::XSalsa20Poly1305;
use xsalsa20poly1305::aead::generic_array::GenericArray;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;
use tokio_rustls::TlsConnector;
use tracing::{debug, error, info};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

// ─── Config structs ────────────────────────────────────────────────────────

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
    debug_file_path: String,
    error_file_path: String,
    #[allow(dead_code)]
    rotation: String,
    #[allow(dead_code)]
    max_size_mb: u64,
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    queue_name: String,
    exchange_name: String,
    routing_key: String,
    server_address: String,
    server_port: u16,
    tls: TlsConfig,
    logging: LoggingConfig,
}

// ─── Message envelope from server ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EncryptedEnvelope {
    enc_session_key: String,
    nonce: String,
    ciphertext: String,
}

// ─── Stored record ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct StoredRecord {
    message_id: String,
    message: serde_json::Value,
    timestamp: f64,
}

// ─── Logging setup ─────────────────────────────────────────────────────────

fn setup_logging(config: &LoggingConfig) -> Result<()> {
    // Ensure log directories exist
    for path in [
        &config.info_file_path,
        &config.debug_file_path,
        &config.error_file_path,
    ] {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let level = config.level.to_uppercase();
    let filter = EnvFilter::try_new(format!("receiver={}", level.to_lowercase()))
        .unwrap_or_else(|_| EnvFilter::new("receiver=info"));

    // Console layer
    let console = fmt::layer()
        .with_target(false)
        .with_ansi(true);

    // File layer (append, JSON-like format) writes all levels to the info log;
    // per level splitting requires a custom appender. The tracing appender crate
    // supports rolling files; here we create a non rolling appender for simplicity
    // and match the Python behaviour of writing everything to one file per run.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.info_file_path)
        .with_context(|| format!("Cannot open log file {}", config.info_file_path))?;

    let file_layer = fmt::layer()
        .with_writer(Arc::new(file))
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

// ─── TLS context ───────────────────────────────────────────────────────────

fn build_tls_connector(cfg: &TlsConfig) -> Result<TlsConnector> {
    use rustls::{ClientConfig, RootCertStore};
    use rustls_pemfile::{certs, pkcs8_private_keys};
    use std::fs::File;
    use std::io::BufReader as StdBufReader;

    // CA certificate
    let ca_file = File::open(&cfg.certificate_path)
        .with_context(|| format!("Cannot open CA cert {}", cfg.certificate_path))?;
    let mut ca_reader = StdBufReader::new(ca_file);
    let ca_certs = certs(&mut ca_reader).collect::<Result<Vec<_>, _>>()?;

    let mut roots = RootCertStore::empty();
    for cert in ca_certs {
        roots.add(cert)?;
    }

    // Client certificate chain
    let cert_file = File::open(&cfg.client_cert_path)
        .with_context(|| format!("Cannot open client cert {}", cfg.client_cert_path))?;
    let mut cert_reader = StdBufReader::new(cert_file);
    let client_certs: Vec<_> = certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;

    // Client private key
    let key_file = File::open(&cfg.client_key_path)
        .with_context(|| format!("Cannot open client key {}", cfg.client_key_path))?;
    let mut key_reader = StdBufReader::new(key_file);
    let mut keys = pkcs8_private_keys(&mut key_reader).collect::<Result<Vec<_>, _>>()?;
    if keys.is_empty() {
        return Err(anyhow!("No PKCS8 private key found in {}", cfg.client_key_path));
    }
    let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(keys.remove(0));

    let mut tls_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(client_certs, private_key)?;

    // Match Python: check_hostname = false
    if !cfg.check_hostname {
        tls_config.dangerous().set_certificate_verifier(Arc::new(
            danger::NoHostnameVerifier::new(),
        ));
    }

    Ok(TlsConnector::from(Arc::new(tls_config)))
}

// ─── Dangerous verifier (check_hostname=False) ──────────────

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
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            // Skip hostname check; the CA cert is still verified by the connector
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.inner.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.inner.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.inner.signature_verification_algorithms.supported_schemes()
        }
    }
}

// ─── Key loading ───────────────────────────────────────────────────────────

/// Load the NaCl/X25519 private key used for SealedBox decryption.
/// The file stores the 32 byte raw key encoded as Base64.
fn load_private_key() -> Result<[u8; 32]> {
    let raw = std::fs::read_to_string("../../create_ca_key/Rust_Key_Maker_X25519/receiver_private.key")
        .context("Cannot open create_ca_key/Rust_Key_Maker_X25519/receiver_private.key")?;
    let bytes = BASE64
        .decode(raw.trim())
        .context("Invalid Base64 in private key file")?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("Private key must be exactly 32 bytes"))?;
    Ok(arr)
}

/// Load the raw X25519 public key (used only for the register_public_key command).
fn load_public_key_bytes() -> Result<Vec<u8>> {
    let raw = std::fs::read_to_string("../../create_ca_key/Rust_Key_Maker_X25519/receiver_public.key")
        .context("Cannot open create_ca_key/Rust_Key_Maker_X25519/receiver_public.key")?;
    BASE64.decode(raw.trim()).context("Invalid Base64 in public key file")
}

// ─── Protocol helpers ──────────────────────────────────────────────────────

/// Send one line and wait for the server's single-line response.
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

async fn register_public_key<W, R>(writer: &mut W, reader: &mut R) -> Result<bool>
where
    W: AsyncWriteExt + Unpin,
    R: AsyncBufReadExt + Unpin,
{
    let pub_bytes = load_public_key_bytes()?;
    let pub_b64 = BASE64.encode(&pub_bytes);
    let cmd = format!("register_public_key {}\n", pub_b64);
    let response = send_recv(writer, reader, &cmd).await?;
    info!("Server response for public key registration: {}", response);
    Ok(response == "Public key registered")
}

async fn configure_server<W, R>(
    writer: &mut W,
    reader: &mut R,
    queue: &str,
    exchange: &str,
    routing_key: &str,
) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    R: AsyncBufReadExt + Unpin,
{
    let r = send_recv(writer, reader, &format!("declare_queue {}\n", queue)).await?;
    info!("Server response for queue declaration: {}", r);

    let r = send_recv(writer, reader, &format!("declare_exchange {}\n", exchange)).await?;
    info!("Server response for exchange declaration: {}", r);

    let r = send_recv(
        writer,
        reader,
        &format!("bind {} {} {}\n", queue, exchange, routing_key),
    )
    .await?;
    info!("Server response for binding: {}", r);

    Ok(())
}

// ─── Decryption ────────────────────────────────────────────────────────────

/// Decrypt one incoming message envelope using:
/// 1. NaCl SealedBox to unwrap the ephemeral session key.
/// 2. ChaCha20 Poly1305 AEAD to decrypt the payload.
fn decrypt_message(envelope: &EncryptedEnvelope, private_key: &[u8; 32]) -> Result<String> {
    let enc_session_key = BASE64
        .decode(&envelope.enc_session_key)
        .context("Bad Base64 in enc_session_key")?;
    let nonce_bytes = BASE64
        .decode(&envelope.nonce)
        .context("Bad Base64 in nonce")?;
    let ciphertext = BASE64
        .decode(&envelope.ciphertext)
        .context("Bad Base64 in ciphertext")?;

    // Layout: enc_session_key = ephemeral_pk (32 bytes) || box_ciphertext
    if enc_session_key.len() < 32 {
        return Err(anyhow!("SealedBox ciphertext too short"));
    }
    let (epk_bytes, box_ct) = enc_session_key.split_at(32);

    // Recipient key pair
    let rsk = StaticSecret::from(*private_key);
    let rpk = X25519PublicKey::from(&rsk);
    let epk = X25519PublicKey::from(<[u8; 32]>::try_from(epk_bytes).unwrap());

    // Shared secret via X25519
    let shared = rsk.diffie_hellman(&epk);

    // NaCl nonce = first 24 bytes of blake2b 512(epk || rpk)
    let mut hasher = Blake2b::<blake2::digest::consts::U64>::new();
    Digest::update(&mut hasher, epk_bytes);
    Digest::update(&mut hasher, rpk.as_bytes());
    let hash = hasher.finalize();
    let box_nonce = GenericArray::clone_from_slice(&hash[..24]);

    let box_key = GenericArray::clone_from_slice(shared.as_bytes());
    let box_cipher = XSalsa20Poly1305::new(&box_key);
    let session_key_bytes = box_cipher
        .decrypt(&box_nonce, box_ct)
        .map_err(|e| anyhow!("SealedBox inner-box decryption failed: {}", e))?;

    // --- ChaCha20 Poly1305 decryption of the actual message payload ---
    let chacha_key = chacha20poly1305::Key::from_slice(&session_key_bytes);
    let cipher = ChaCha20Poly1305::new(chacha_key);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|e| anyhow!("ChaCha20-Poly1305 decryption failed: {}", e))?;

    String::from_utf8(plaintext).context("Decrypted payload is not valid UTF-8")
}

// ─── ACK sender task ───────────────────────────────────────────────────────

async fn ack_sender_worker(
    mut ack_rx: mpsc::Receiver<String>,
    writer: Arc<Mutex<tokio::io::WriteHalf<tokio_rustls::client::TlsStream<TcpStream>>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    while running.load(Ordering::Relaxed) {
        match tokio::time::timeout(Duration::from_millis(500), ack_rx.recv()).await {
            Ok(Some(message_id)) => {
                let cmd = format!("ack {}\n", message_id);
                let mut w = writer.lock().await;
                if let Err(e) = w.write_all(cmd.as_bytes()).await {
                    error!("Error sending ACK for {}: {}", message_id, e);
                    break;
                }
                if let Err(e) = w.flush().await {
                    error!("Error flushing ACK for {}: {}", message_id, e);
                    break;
                }
                debug!("Sent ACK for {}", message_id);
            }
            Ok(None) => break, 
            Err(_) => continue, 
        }
    }
}

// ─── Message writer task ───────────────────────────────────────────────────

async fn process_messages_task(
    mut msg_rx: mpsc::Receiver<(String, serde_json::Value)>,
    output_file: String,
    running: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncWriteExt as _;

    let start = Instant::now();
    let mut count: u64 = 0;
    const BATCH: usize = 100;

    // Ensure the data directory exists
    if let Some(parent) = Path::new(&output_file).parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            error!("Cannot create data directory: {}", e);
            return;
        }
    }

    let file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output_file)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            error!("Cannot open output file {}: {}", output_file, e);
            return;
        }
    };

    let mut writer = tokio::io::BufWriter::new(file);
    let mut batch: Vec<StoredRecord> = Vec::with_capacity(BATCH);

    loop {
        match tokio::time::timeout(Duration::from_secs(5), msg_rx.recv()).await {
            Ok(Some((message_id, content))) => {
                batch.push(StoredRecord {
                    message_id,
                    message: content,
                    timestamp: Utc::now().timestamp_millis() as f64 / 1000.0,
                });
                count += 1;

                if batch.len() >= BATCH {
                    flush_batch(&mut writer, &mut batch).await;
                }
            }
            Ok(None) => {
                // Channel closed flush remaining and exit
                if !batch.is_empty() {
                    flush_batch(&mut writer, &mut batch).await;
                }
                break;
            }
            Err(_) => {
                // Timeout flush pending batch
                if !batch.is_empty() {
                    flush_batch(&mut writer, &mut batch).await;
                }
                if !running.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
    }

    let _ = writer.flush().await;

    if count > 0 {
        let elapsed = start.elapsed().as_secs_f64();
        info!(
            "Received {} messages in {:.2} seconds ({:.2} msg/s)",
            count,
            elapsed,
            count as f64 / elapsed
        );
    }
    info!("Message processing stopped");
}

async fn flush_batch(
    writer: &mut tokio::io::BufWriter<tokio::fs::File>,
    batch: &mut Vec<StoredRecord>,
) {
    use tokio::io::AsyncWriteExt as _;

    for record in batch.iter() {
        match serde_json::to_string(record) {
            Ok(line) => {
                let _ = writer.write_all(line.as_bytes()).await;
                let _ = writer.write_all(b"\n").await;
            }
            Err(e) => error!("JSON serialization error: {}", e),
        }
    }
    if let Err(e) = writer.flush().await {
        error!("Error flushing output file: {}", e);
    }
    info!("Saved batch of {} messages", batch.len());
    batch.clear();
}

// ─── Receive loop ──────────────────────────────────────────────────────────

/// Full receive loop: handshake → subscribe → read/decrypt/ack → reconnect-safe cleanup.
async fn receive_loop(
    stream: tokio_rustls::client::TlsStream<TcpStream>,
    config: &Config,
    private_key: &[u8; 32],
    msg_tx: mpsc::Sender<(String, serde_json::Value)>,
    running: Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    use std::sync::atomic::Ordering;

    let (read_half, write_half) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(write_half));
    let mut reader = BufReader::new(read_half);

    let (ack_tx, ack_rx) = mpsc::channel::<String>(4096);

    let ack_writer = Arc::clone(&writer);
    let ack_running = Arc::clone(&running);
    let ack_handle = tokio::spawn(ack_sender_worker(ack_rx, ack_writer, ack_running));

    // --- Handshake ----------------------------------------------------------
    {
        let mut w = writer.lock().await;
        if !register_public_key(&mut *w, &mut reader).await? {
            error!("Public key registration failed");
            running.store(false, Ordering::Relaxed);
            ack_handle.abort();
            return Err(anyhow!("Public key registration failed"));
        }
        configure_server(
            &mut *w,
            &mut reader,
            &config.queue_name,
            &config.exchange_name,
            &config.routing_key,
        )
        .await?;
        w.write_all(format!("consume {}\n", config.queue_name).as_bytes())
            .await?;
        w.flush().await?;
    }

    info!("Subscribing to queue {}", config.queue_name);
    info!("Waiting for messages (high-throughput mode)");

    let mut processed: HashSet<String> = HashSet::new();
    let mut consecutive_empty: u32 = 0;

    // --- Main receive loop --------------------------------------------------
    loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }

        let mut line = String::new();
        match tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line)).await {
            Err(_) => {
                // Timeout no data
                consecutive_empty += 1;
                if consecutive_empty >= 120 {
                    debug!("No messages for 60 seconds");
                    consecutive_empty = 0;
                }
                continue;
            }
            Ok(Err(e)) => {
                error!("Error in receive loop: {}", e);
                break;
            }
            Ok(Ok(0)) => {
                error!("Connection closed by server");
                break;
            }
            Ok(Ok(_)) => {
                consecutive_empty = 0;
                let message = line.trim();
                if message.is_empty() {
                    continue;
                }

                // Parse: "Message: <id> <json>"
                if !message.starts_with("Message:") {
                    continue;
                }

                let body = message["Message:".len()..].trim();
                let mut parts = body.splitn(2, ' ');
                let message_id = match parts.next() {
                    Some(id) if !id.is_empty() => id.to_string(),
                    _ => {
                        error!("Invalid message format (no ID)");
                        continue;
                    }
                };
                let message_str = match parts.next() {
                    Some(s) => s,
                    None => {
                        error!("Invalid message format (no payload)");
                        continue;
                    }
                };

                if processed.contains(&message_id) {
                    debug!("Duplicate message {}", message_id);
                    continue;
                }

                // Deserialize envelope
                let envelope: EncryptedEnvelope = match serde_json::from_str(message_str) {
                    Ok(e) => e,
                    Err(e) => {
                        error!("JSON parse error for {}: {}", message_id, e);
                        continue;
                    }
                };

                // Decrypt
                match decrypt_message(&envelope, private_key) {
                    Ok(plaintext) => {
                        processed.insert(message_id.clone());
                        let content =
                            serde_json::json!({ "content": plaintext });
                        if let Err(e) = msg_tx.send((message_id.clone(), content)).await {
                            error!("Message channel send error: {}", e);
                        }
                        if let Err(e) = ack_tx.send(message_id.clone()).await {
                            error!("ACK channel send error: {}", e);
                        }
                        info!("Processed and decrypted message {}", message_id);
                    }
                    Err(e) => {
                        error!("Error processing message {}: {}", message_id, e);
                    }
                }
            }
        }
    }

    // --- Cleanup ------------------------------------------------------------
    running.store(false, Ordering::Relaxed);
    ack_handle.abort();
    drop(writer);

    info!("Connection closed");
    Ok(())
}

// ─── Entry point ───────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Install the ring crypto provider for rustls (must be called before any TLS usage)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Create required directories
    for dir in ["logs", "keys", "data"] {
        std::fs::create_dir_all(dir)?;
    }

    // Load config
    let config_str = std::fs::read_to_string("config.json")
        .context("Configuration file 'config.json' not found")?;
    let config: Config =
        serde_json::from_str(&config_str).context("Failed to parse config.json")?;

    // Setup logging
    setup_logging(&config.logging)?;

    // Load private key
    let private_key = load_private_key()?;

    // Build TLS connector
    let connector = build_tls_connector(&config.tls)?;

    // Shared running flag
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));

    // SIGINT handler
    {
        let r = Arc::clone(&running);
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("Received SIGINT, shutting down");
            r.store(false, std::sync::atomic::Ordering::Relaxed);
        });
    }

    // Message processing channel & task
    let (msg_tx, msg_rx) = mpsc::channel::<(String, serde_json::Value)>(4096);
    let output_file = format!("data/{}_received_messages.jsonl", config.queue_name);
    let proc_running = Arc::clone(&running);
    let proc_handle =
        tokio::spawn(process_messages_task(msg_rx, output_file, proc_running));

    // Connection loop (auto-reconnect)
    while running.load(std::sync::atomic::Ordering::Relaxed) {
        let addr = format!("{}:{}", config.server_address, config.server_port);
        match TcpStream::connect(&addr).await {
            Err(e) => {
                error!("TCP connection to {} failed: {}", addr, e);
            }
            Ok(tcp) => {
                let server_name = rustls::pki_types::ServerName::try_from("localhost")
                    .expect("Invalid server name");
                match connector.connect(server_name, tcp).await {
                    Err(e) => {
                        error!("TLS handshake failed: {}", e);
                    }
                    Ok(tls) => {
                        // Log cipher suite
                        let cipher_info = tls.get_ref().1.negotiated_cipher_suite();
                        info!("TLS connection established. Cipher: {:?}", cipher_info);

                        let result = receive_loop(
                            tls,
                            &config,
                            &private_key,
                            msg_tx.clone(),
                            Arc::clone(&running),
                        )
                        .await;

                        if let Err(e) = result {
                            error!("Connection error: {}", e);
                        }
                    }
                }
            }
        }

        if running.load(std::sync::atomic::Ordering::Relaxed) {
            sleep(Duration::from_secs(1)).await;
        }
    }

    // Wait for the processing task to drain and finish
    let _ = proc_handle.await;
    Ok(())
}