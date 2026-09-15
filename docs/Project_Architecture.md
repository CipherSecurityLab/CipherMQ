# CipherMQ Project Architecture: A Secure Message Queue with mTLS, ACL, Rate Limiting, and Guaranteed Delivery

## 1. Introduction

**CipherMQ** is a high-performance, secure message queue system built entirely in **Rust** the broker, the Sender reference client, the Receiver reference client, and the Admin Console are all Rust binaries. It is designed for encrypted message transmission with **zero message loss** and **exactly-once delivery**. It leverages **Mutual Transport Layer Security (mTLS)** for secure client-server communication, **hybrid encryption** (X25519 + XSalsa20-Poly1305 sealed box, then ChaCha20-Poly1305) for message confidentiality between sender and receiver, and **AES-256-GCM** for encrypting receivers' public keys at rest in a PostgreSQL database.

The system now includes **role-based Access Control (ACL)** that maps mTLS certificate Common Names to `sender`, `receiver`, or `admin` roles with strict per-command permission enforcement, **token-bucket rate limiting** with per-IP and per-CN connection caps, **Prometheus-compatible metrics** with lock-free atomic counters, a **real-time admin dashboard**, and **heartbeat monitoring** for production-grade deployments.

The project consists of:
- **Server** (`server/src/`): A Rust/Tokio message broker for routing and delivering messages over mTLS, with ACL enforcement, rate limiting, metrics collection, heartbeat monitoring, and an mTLS-secured admin console. PostgreSQL-backed persistence for metadata and public keys.
- **Sender** (`clients/sender_1/`): A Rust/Tokio client that fetches receiver public keys, hybrid-encrypts messages, and sends them through an async, backpressure-bounded pipeline with retry logic.
- **Receiver** (`clients/receiver_1/`): A Rust/Tokio client that registers its public key, receives, decrypts, deduplicates, and persists messages, acknowledging each as it is processed, with periodic heartbeat reporting.
- **Admin Console** (`clients/admin/`): An Axum-based web dashboard that polls the server's admin endpoint over mTLS and renders real-time server health, queue details, binding maps, connection statistics, and Prometheus metrics.
- **Logging**: Structured JSON logs with rotation and level-based filtering on the server; JSON file logs plus a human-readable console layer on both clients.

This document provides a comprehensive overview of the architecture, covering the server, the clients, mTLS, encryption, key distribution, acknowledgment mechanisms, access control, rate limiting, metrics, admin console, and storage, all based directly on the current Rust implementation.

## 2. Architecture Overview

CipherMQ operates as a message queue with **queues** and **exchanges**, exposed over mTLS connections via a simple line-oriented, text-based protocol. Key features include:

- **Mutual TLS (mTLS)**: Secures client-server communication with two-way authentication. The server uses `tokio-rustls` with a `WebPkiClientVerifier` (`server/src/auth.rs`); all Rust clients use `tokio-rustls` on the client side. Server-side parameters (cert/key/CA paths, listen address) are configured via `config.toml`; client-side parameters via each client's `config.json`.
- **Role-Based Access Control (ACL)**: Each connecting client's CN is resolved to a `Role` (`Sender`, `Receiver`, `Admin`, or `Unknown`) via the `AclManager`. Unknown roles are rejected at connection time. Each command is checked against a permission matrix before execution, and receivers are additionally restricted to resources matching their own CN.
- **Token-Bucket Rate Limiting**: Per-CN global and publish-specific token buckets with configurable RPS and burst. Per-IP and per-CN connection count limits prevent resource exhaustion.
- **Hybrid Encryption**: For every message, the sender generates a random 256-bit session key and 96-bit nonce. The session key is wrapped for the receiver using an X25519 ECDH shared secret combined with XSalsa20-Poly1305 (the NaCl/libsodium "sealed box" construction); the message content itself is encrypted with ChaCha20-Poly1305 using that session key and nonce.
- **Public Key Distribution**: Receivers register their public key using the `register_public_key` command; senders retrieve a receiver's public key using `get_public_key`. Keys are stored AES-256-GCM-encrypted in PostgreSQL.
- **Zero Message Loss**: The sender retries publishing until it receives `ACK <message_id>` from the broker; the broker keeps a message in its queue (and retains its metadata in PostgreSQL) until the receiver sends `ack <message_id>`.
- **Exactly-Once Delivery**: The receiver deduplicates messages in-memory using `message_id` before processing or storing them.
- **Asynchronous Processing**: Built with Tokio throughout server and all clients for concurrent, high-performance connection handling.
- **Thread-Safe Data Structures**: The server uses `DashMap` for its in-memory queue/exchange/binding/consumer/status maps, allowing lock-free concurrent access across client-handling tasks.
- **Flexible Routing**: Supports exchanges, queues, and routing-key-based bindings, declared explicitly by clients before publishing or consuming.
- **Heartbeat Monitoring**: Receivers send periodic `heartbeat` commands; the server tracks liveness per client and automatically cleans up timed-out connections.
- **Prometheus-Compatible Metrics**: Atomic counters (`AtomicU64`) for messages published, acknowledged, delivered, errors, connections, and rate-limit rejections; gauges for connected clients, queue count, messages in queues, consumers, and uptime all exportable in Prometheus text exposition format.
- **Admin Console**: A dedicated mTLS-secured TCP endpoint (`server/src/admin_server.rs`) exposes node status as JSON and Prometheus metrics. A companion Axum web dashboard (`clients/admin/`) polls this endpoint and renders live server health, queue details, binding maps, connection statistics, and counter data.
- **Structured Logging**: The server writes JSON logs split by level (`INFO`, `DEBUG`, `ERROR`) into separate files, with configurable rotation (`hourly`, `daily`, or `never`). Both clients write JSON logs to a single configured file plus a readable console layer, filtered by a single configured level.
- **Persistent Storage**: The server stores message metadata and AES-256-GCM-encrypted public keys in PostgreSQL via a dedicated async storage actor task.

## 3. Architectural Components

### 3.1. Server (`server/src/`)

The server is the core of CipherMQ, managing message routing, delivery, secure connections via mTLS, access control, rate limiting, metrics, heartbeat monitoring, and persistent storage in PostgreSQL.

#### 3.1.1. Data Structures (`state.rs`)

- **`ServerState`**: The broker's in-memory state, shared behind `Arc<ServerState>` (lock-free via DashMap):
  - `queues: DashMap<String, VecDeque<(String, EncryptedInputData)>>`: Messages currently held in each declared queue, awaiting consumption. Uses `VecDeque` for O(1) push/pop.
  - `message_queues: DashMap<String, DashSet<String>>`: Maps each `message_id` to the set of queues it resides in, enabling efficient removal on ACK.
  - `bindings: DashMap<String, Vec<(String, String)>>`: For each exchange, the list of `(queue_name, routing_key)` pairs bound to it.
  - `exchanges: DashMap<String, Vec<String>>`: The set of declared exchanges.
  - `consumers: DashMap<String, Vec<mpsc::UnboundedSender<(String, EncryptedInputData)>>>`: Per-queue channels used to push newly published messages directly to subscribed consumer connections.
  - `message_status: DashMap<String, MessageStatus>`: Per-message `sent_time` / `delivered_time` / `acknowledged_time` (as `Option<std::time::Instant>`), used to track in-flight messages; the entry is removed once the message is acknowledged.
  - `connected_clients: Arc<AtomicUsize>`: Count of currently connected clients, incremented/decremented atomically (lock-free).
  - `storage: Arc<Storage>`: Handle to the PostgreSQL-backed storage actor.
  - `max_queue_size: usize`: Maximum number of messages per queue; oldest messages are evicted when exceeded.
- **`EncryptedInputData`**: The wire format for one encrypted message body, exchanged in the `publish` / `publish_batch` / `consume` / `fetch` commands:
  - `message_id: String`: Unique identifier for deduplication, constructed by the sender as `"{sender_id}-{correlation_id}-{receiver_client_id}"`.
  - `receiver_client_id: String`: The target receiver's client ID.
  - `enc_session_key: String`: Base64-encoded sealed-box ciphertext (32-byte ephemeral X25519 public key followed by the XSalsa20-Poly1305-encrypted session key).
  - `nonce: String`: Base64-encoded 96-bit ChaCha20-Poly1305 nonce for the message payload.
  - `ciphertext: String`: Base64-encoded ChaCha20-Poly1305 ciphertext of the message content (the Poly1305 authentication tag is appended to this ciphertext by the AEAD crate).
- **`MessageStatus`**: Tracks per-message lifecycle with `sent_time`, `delivered_time`, and `acknowledged_time` as `Option<Instant>`. The `can_deliver()` method returns `true` if the message has not yet been acknowledged.

**Background Tasks**:
- **Message Cleanup** (`cleanup_old_messages`): Runs every 60 seconds; removes messages from queues and status that were delivered more than 3000 seconds ago and never acknowledged.
- **Dead Consumer Cleanup** (`cleanup_dead_consumers`): Runs every 60 seconds; removes closed `mpsc::UnboundedSender` channels from the consumer map and cleans up empty consumer lists.

#### 3.1.2. Access Control (`acl.rs`)

- **`Role` enum**: `Sender`, `Receiver`, `Admin`, `Unknown`.
- **`AuthResult` enum**: `Allowed` or `Denied(&'static str)`.
- **`AclManager`**: Holds three `Arc<HashSet<String>>` sets for sender, receiver, and admin CNs. Provides:
  - `resolve_role(cn: &str) -> Role`: Maps a CN to its role. Admins are checked first (for priority), then senders, then receivers; any unrecognized CN returns `Unknown`.
  - `check_command(role, command) -> AuthResult`: Enforces the per-role permission matrix (see §8 in README). Admins have full access; unknown roles are denied all commands.
  - `check_resource(role, cn, resource_type, resource_name) -> AuthResult`: For receivers, verifies that the resource (queue, routing_key) matches their own CN prefix (e.g., `receiver_1` can only declare `receiver_1_queue`). Admins can access any resource.

**Permission Matrix**:

| Command | Sender | Receiver | Admin |
|---|---|---|---|
| `declare_queue` | ❌ | ✅ (own resources only) | ✅ |
| `declare_exchange` | ❌ | ✅ | ✅ |
| `bind` | ❌ | ✅ (own resources only) | ✅ |
| `publish` / `publish_batch` | ✅ | ❌ | ✅ |
| `consume` | ❌ | ✅ | ✅ |
| `ack` | ✅ | ✅ | ✅ |
| `register_public_key` | ❌ | ✅ | ✅ |
| `get_public_key` | ✅ | ❌ | ✅ |
| `resend` | ✅ | ✅ | ✅ |
| `heartbeat` | ❌ | ✅ | ✅ |

#### 3.1.3. Connection Handling (`connection.rs`, `auth.rs`)

- **mTLS**: The server uses `tokio-rustls` with ECDSA P-384 certificates for both the server identity and client authentication.
- **Authentication** (`auth.rs`): Builds a `rustls::ServerConfig` from `cert_path`/`key_path`/`ca_cert_path` and a `WebPkiClientVerifier` built from the CA's root store, requiring every connecting client to present a certificate signed by that CA.
- **Client identity** (`connection.rs`): Extracts the client's `client_id` directly from the Common Name (CN) field of its TLS peer certificate once the handshake completes there is no separate login or identity command.
- **Protocol**: A line-oriented text protocol over the TLS stream. Each connection is handled by `handle_client` (`server.rs`) in a loop that reads one command per line and, concurrently, forwards any messages pushed to that client's consumer channel.

#### 3.1.4. Rate Limiting (`rate_limiter.rs`)

- **`RateLimiter`**: Uses per-CN token buckets for global and publish-specific rate limiting. Buckets refill at the configured RPS rate up to the burst capacity.
- **Connection Limits**: Tracks active connections per IP and per CN via `HashMap<String, usize>` behind `RwLock`. Connections are incremented on accept and decremented on disconnect.
- **`RateLimitResult`**: `Allowed`, `RateLimited`, or `ConnectionLimit`.
- All rate limiter state is behind `Arc<RwLock<>>` for concurrent access.

#### 3.1.5. Metrics (`metrics.rs`)

- **`Metrics`**: Holds `AtomicU64` counters for:
  - `messages_published`, `messages_acked`, `messages_delivered`
  - `errors_total`, `connections_total`
  - `connections_rejected_rate_limit`, `connections_rejected_conn_limit`
  - `requests_rate_limited`
- **`ServerSnapshot`**: A `RwLock`-protected snapshot updated periodically with `connected_clients`, `queue_count`, `total_messages`, and `consumer_count`.
- **`render_prometheus()`**: Returns Prometheus text exposition format with `# HELP`, `# TYPE`, and metric values.
- **`render_status()`**: Returns a human-readable status summary.
- **`start_time`**: `Instant` recorded at server startup for uptime calculation.

#### 3.1.6. Admin Server (`admin_server.rs`)

- **mTLS Secured**: Uses its own `TlsAcceptor` built from the same CA and server certificates. Only clients with an admin CN (verified via `AclManager::resolve_role`) can connect.
- **Commands**:
  - `json` Full JSON status including server info, counters, snapshot, live data, connection stats, queues, bindings, and exchanges.
  - `metrics` Prometheus-format metrics output.
  - `status` Human-readable status summary.
  - `queues` Queue count, total messages, total consumers.
  - `connections` Per-IP and per-CN connection breakdown.
  - `quit` / `exit` Disconnect.

#### 3.1.7. Heartbeat Monitor (`main.rs`)

- **`HeartbeatMonitor`**: A `DashMap<String, Instant>` tracking the last heartbeat time per client. Configurable timeout (default 60 seconds).
- **`monitor_task()`**: Runs every 10 seconds, scanning for clients whose last heartbeat exceeds the timeout. Timed-out clients are removed and logged as warnings.
- **`record_heartbeat(client_id)`**: Called on every received command (all data received counts as a heartbeat) and on initial connection.
- **`get_stats()`**: Returns `(total_tracked, alive_count)` for the admin console.

#### 3.1.8. Storage (`storage.rs`)

The server talks to PostgreSQL through a single background task (a "storage actor") that owns the database client and processes commands sent to it over an `mpsc` channel, so all database access is serialized through one place even though many client connections run concurrently.

- **Connection Pool**: Uses `sqlx::PgPool` with up to 200 connections, retry logic (5 attempts with 5-second backoff), and automatic reconnection if the pool closes.
- **PostgreSQL schema**, created automatically on startup if it does not already exist:
  - **`message_metadata`**:
    - `message_id` (TEXT, PRIMARY KEY)
    - `client_id` (TEXT): The sender's client ID.
    - `exchange_name` (TEXT)
    - `routing_key` (TEXT)
    - `sent_time` (TEXT, RFC 3339)
    - `delivered_time` (TEXT, RFC 3339, nullable)
    - `acknowledged_time` (TEXT, RFC 3339, nullable)
  - **`public_keys`**:
    - `client_id` (TEXT, PRIMARY KEY)
    - `public_key_ciphertext` (TEXT): Base64-encoded AES-256-GCM ciphertext of the receiver's raw X25519 public key.
    - `nonce` (TEXT): Base64-encoded 96-bit AES-GCM nonce, randomly generated per registration.
    - `tag` (TEXT): Base64-encoded 128-bit AES-GCM authentication tag.
- **Batched Updates**: Delivered-time and acknowledged-time updates are buffered in `HashMap<String, String>` and flushed to PostgreSQL in batch every 500ms or when the buffer reaches 50 items, using a single `UPDATE ... FROM (SELECT unnest(...))` query.
- **Encryption at rest**: Public keys are encrypted with **AES-256-GCM** using the 32-byte key configured at `[encryption].aes_key` in `config.toml`. The ciphertext and tag produced by the `aes-gcm` crate are split apart before storage and concatenated again on read.

#### 3.1.9. Configuration (`config.rs`)

- Loads and parses `config.toml` (sections: `[server]`, `[tls]`, `[logging]`, `[database]`, `[encryption]`, `[performance]`, `[admin]`, `[rate_limit]`, `[acl]`).
- Validates that `tls.cert_path`, `tls.key_path`, and `tls.ca_cert_path` are present when `connection_type = "tls"`.
- Validates that `database.host`, `database.user`, and `database.dbname` are non-empty.
- Validates that `encryption.algorithm` is exactly `"x25519_chacha20_poly1305"` and that `encryption.aes_key` Base64-decodes to exactly 32 bytes.
- Applies defaults for any empty/zero logging fields (`level` → `"info"`, `rotation` → `"daily"`, log file paths → `logs/info.log` / `logs/debug.log` / `logs/error.log`, `max_size_mb` → `10`).
- `PerformanceConfig` defaults: `max_queue_size` → 10,000, `heartbeat_timeout` → 60s, `cleanup_interval` → 3000s, `consumer_cleanup_interval` → 60s, `idle_timeout_sec` → 60s.
- `AdminConfig` defaults: `enabled` → false, `address` → `127.0.0.1:9091`, `allowed_cns` → empty.
- `RateLimitConfig` defaults: all values as listed in §3.1.4; `enabled` → false.
- `AclConfig` defaults: all CN lists empty (no ACL enforcement if unconfigured).

### 3.2. Sender (`clients/sender_1/`)

- **Role**: Fetches receiver public keys, hybrid-encrypts messages, and sends them through an asynchronous, backpressure-bounded pipeline with automatic retries.
- **mTLS**: Authenticates with `tls.client_cert_path` / `tls.client_key_path` from `config.json`, verifies the server using `tls.certificate_path` (the CA certificate). When `tls.check_hostname` is `false`, certificate-chain validation still runs, but the TLS `ServerName` hostname check is skipped via a custom `ServerCertVerifier`.
- **Identity**: The sender's own `client_id` is not configured explicitly it is extracted from the CN field of its own client certificate at startup (`extract_client_id`) and used to populate the `{sender_id}` template placeholder and the generated `message_id`.
- **Encryption**: For each message and each target receiver, generates a fresh random 256-bit session key and 96-bit nonce; wraps the session key for that receiver with X25519 + XSalsa20-Poly1305 sealed-box encryption (`sealed_box_encrypt`), and encrypts the message body with ChaCha20-Poly1305 (`encrypt_message`). A separate ciphertext/session-key pair is generated per receiver, so compromising one receiver's private key does not expose messages addressed to other receivers.
- **Message Generation**: Uses configurable `content_template` with placeholders `{sender_id}`, `{correlation_id}`, `{timestamp}`, `{seq}`, and custom `extra_fields`.
- **Commands used**:
  - `declare_queue <queue>`, `declare_exchange <exchange>`, `bind <queue> <exchange> <routing_key>`: Issued once per configured binding before publishing.
  - `get_public_key <receiver_client_id>`: Retrieves a receiver's public key (and caches it to `keys/<receiver_client_id>_public.key`).
  - `publish <exchange> <routing_key> <json_message>`: Sends one encrypted message; the server replies with `ACK <message_id>` or an `Error:` line.
- **Pipeline**: Opens a dedicated mTLS connection for the send/ACK pipeline. A `tokio::sync::Semaphore` sized to `sender.max_inflight` bounds how many messages may be outstanding at once; a separate task continuously reads ACK lines from the same connection and releases semaphore permits as ACKs arrive. After all messages have been sent, the sender drains remaining ACKs up to a timeout, then retries any still-unacknowledged messages up to `sender.max_retries` times with exponential backoff, before reporting final throughput and failure counts.
- **Logging**: JSON logs to `logging.info_file_path`, plus a readable console layer, both filtered by `logging.level`.

### 3.3. Receiver (`clients/receiver_1/`)

- **Role**: Registers its public key, receives, decrypts, deduplicates, and stores messages in `data/<queue_name>_received_messages.jsonl`, acknowledging each as it is processed, with periodic heartbeats.
- **mTLS**: Authenticates with `tls.client_cert_path` / `tls.client_key_path`, verifies the server using `tls.certificate_path`. Same `check_hostname` behavior as the Sender.
- **Decryption**: Loads its X25519 private key from `keys/receiver_private.key` (Base64, 32 bytes). For each incoming message it reverses the sender's scheme: unwraps the session key with the sealed-box construction (X25519 ECDH + XSalsa20-Poly1305) using its own private key and the ephemeral public key embedded in `enc_session_key`, then decrypts the payload with ChaCha20-Poly1305 using that session key and the message's `nonce`.
- **Commands used**:
  - `register_public_key <base64_public_key>`: Registers this receiver's X25519 public key with the broker; on success the server replies `Public key registered`.
  - `declare_queue <queue>`, `declare_exchange <exchange>`, `bind <queue> <exchange> <routing_key>`: Issued once at startup using `queue_name` / `exchange_name` / `routing_key` from `config.json`.
  - `consume <queue>`: Subscribes as a push consumer; the broker then streams `Message: <id> <json>` lines as they are published.
  - `ack <message_id>`: Sent after a message has been successfully decrypted and queued for local storage.
  - `heartbeat`: Sent every 30 seconds to maintain liveness with the server's heartbeat monitor.
- **Deduplication**: Maintains an in-memory `HashSet<String>` of processed `message_id`s for the lifetime of the connection/process, skipping any message whose ID has already been seen.
- **Persistence**: Decrypted messages are batched (up to 100 at a time, or on a 5-second idle timeout) and appended as JSON Lines to `data/<queue_name>_received_messages.jsonl`, each record containing `message_id`, the decrypted `message` content, and a Unix `timestamp`.
- **Concurrency**: Uses separate Tokio tasks for message receiving, ACK sending, heartbeat reporting, and message persistence, coordinated via `mpsc` channels and `AtomicBool` running flag.
- **Resilience**: Automatically reconnects (with a 1-second pause) if the TLS connection drops, and responds to `Ctrl+C` (`SIGINT`) by draining in-flight work before exiting.
- **Logging**: JSON logs to `logging.info_file_path`, plus a readable console layer, filtered by `logging.level`.

### 3.4. Admin Console (`clients/admin/`)

- **Architecture**: A two-tier system:
  1. **Background Polling Thread** (`poll.rs`): Establishes a persistent mTLS connection to the server's admin endpoint, sends `json` commands at the configured `poll_interval_secs`, and updates a shared `RwLock<NodeData>` with the parsed JSON response. Automatically reconnects on failure.
  2. **Axum Web Server** (`main.rs`): Serves the HTML dashboard at `/` and a JSON API endpoint at `/api/status` that returns the latest `NodeData`. Uses CORS middleware for cross-origin access.

- **`NodeData` structure**: Mirrors the server's JSON status output exactly:
  - `server`: name, uptime, health, max_queue_size
  - `counters`: messages_published, messages_acked, messages_delivered, errors_total, connections_total, connections_rejected_rate_limit, connections_rejected_conn_limit, requests_rate_limited
  - `snapshot`: connected_clients, queue_count, total_messages, consumer_count
  - `live`: connected_clients, queues, total_messages, total_consumers, exchanges
  - `connection_stats`: active_by_ip, active_by_cn, by_ip map, by_cn map
  - `queues`: array of {name, message_count, consumer_count}
  - `bindings`: array of {exchange, queue, routing_key}
  - `exchanges`: array of exchange names
  - `reachable`: boolean (false if last poll failed)
  - `last_update`: timestamp string

- **Dashboard** (`dashboard.html`): A self-contained HTML file with embedded CSS and JavaScript that fetches `/api/status` periodically and renders:
  - Server health badge (healthy/unreachable)
  - Uptime and server name
  - Connected clients, queues, consumers, exchanges
  - Messages published/acked/delivered
  - Rate-limit rejections and connection limits
  - Per-queue table with message counts and consumer counts
  - Full binding map
  - Connection statistics by IP and CN

- **Configuration** (`config.json`):
  - `server_name`: Display name for the dashboard.
  - `poll_interval_secs`: How often to poll the server (default: 3).
  - `web_address`: Dashboard listen address (default: `127.0.0.1:8080`).
  - `tls.*`: CA cert, client cert, client key for mTLS.
  - `server.admin_address`: Server's admin console endpoint.

### 3.5. Hybrid Encryption

- **Sender** (per message, per receiver):
  1. Looks up the receiver's X25519 public key (fetched live via `get_public_key`, or read from a local cache under `keys/`).
  2. Generates a random 256-bit session key and 96-bit nonce.
  3. Wraps the session key for the receiver using a NaCl-style sealed box: an ephemeral X25519 key pair is generated, an X25519 shared secret is computed with the receiver's public key, a nonce is derived by hashing the ephemeral and receiver public keys with BLAKE2b, and the session key is encrypted with XSalsa20-Poly1305 under that shared secret and derived nonce. The ephemeral public key is prepended to the resulting ciphertext to form `enc_session_key`.
  4. Encrypts the plaintext message content with ChaCha20-Poly1305 under the session key and the random nonce, producing `ciphertext` (with the Poly1305 tag appended) and `nonce`.
- **Receiver** (per incoming message):
  1. Splits `enc_session_key` into the embedded ephemeral public key and the sealed-box ciphertext.
  2. Recomputes the X25519 shared secret using its own private key and the embedded ephemeral public key, rederives the same BLAKE2b-based nonce, and decrypts the sealed box with XSalsa20-Poly1305 to recover the session key.
  3. Decrypts `ciphertext` with ChaCha20-Poly1305 using the recovered session key and `nonce`, verifying the Poly1305 tag and recovering the plaintext message content.

### 3.6. Public Key Distribution

- **Receiver**:
  - Generates an X25519 key pair offline using the `create_ca_key/Rust_Key_Maker_X25519` utility (`receiver_private.key`, `receiver_public.key`).
  - On startup, sends `register_public_key <base64_public_key>` to the server over its mTLS connection (its `client_id` is implicit from its certificate CN).
  - The server encrypts the raw public key bytes with AES-256-GCM and stores the result in the `public_keys` table in PostgreSQL.
- **Sender**:
  - Sends `get_public_key <receiver_client_id>` to retrieve a receiver's public key.
  - Caches the returned key locally as `keys/<receiver_client_id>_public.key` for reuse if the server is later unreachable.
- **Security**:
  - Public keys are encrypted at rest in PostgreSQL with AES-256-GCM.
  - mTLS restricts both key registration and key retrieval to clients holding a certificate signed by the configured CA.
  - ACL restricts `register_public_key` to receiver-role clients and `get_public_key` to sender-role clients.

## 4. Component Interactions

1. **Key and Certificate Generation**:
   - `create_ca_key/Rust_CA_Maker_ECDSA_P-384_Multi_Client` (a Rust binary using `rcgen`) generates `ca.crt`/`ca.key`, `server.crt`/`server.key`, and one `client.crt`/`client.key` pair per CN argument supplied on the command line (e.g., `sender_1`, `receiver_1`, `admin`).
   - `create_ca_key/Rust_Key_Maker_X25519` (a Rust binary using `x25519-dalek`) generates `receiver_public.key` and `receiver_private.key`.

2. **Server Startup**:
   - Loads `config.toml` and initializes logging (JSON, per-level files + console).
   - Connects to PostgreSQL with retry logic; creates `message_metadata` and `public_keys` tables if they don't exist.
   - Initializes `ServerState` (DashMap-based, with background cleanup tasks), `Metrics`, `RateLimiter`, `AclManager`, and `HeartbeatMonitor`.
   - Spawns the admin console task (if `[admin].enabled = true`).
   - Spawns the heartbeat monitor task.
   - Loads unacknowledged message metadata from PostgreSQL for resend requests.
   - Begins accepting mTLS connections.

3. **Client Connection**:
   - mTLS handshake → CN extracted → ACL role resolved.
   - If role is `Unknown`, connection is rejected immediately.
   - Connection limits checked (per-IP, per-CN).
   - Client enters command loop.

4. **Receiver to Server**:
   - Receiver authenticates via mTLS, then sends `register_public_key <base64_public_key>`.
   - Declares its queue and exchange, binds them with its routing key, and subscribes with `consume`.
   - Sends `heartbeat` every 30 seconds.

5. **Sender to Server**:
   - Sender authenticates via mTLS, declares/binds its configured exchange and queues (these commands are **denied by ACL** for senders the sender uses a separate connection or relies on pre-declared resources).
   - Sends `get_public_key <receiver_client_id>` for each configured receiver.
   - Hybrid-encrypts and publishes messages with `publish <exchange> <routing_key> <json_message>`.
   - Server replies `ACK <message_id>` for each successfully queued message.

6. **Server to Receiver**:
   - The server routes each published message to every queue bound to its exchange/routing-key pair and pushes it immediately to any subscribed consumer as `Message: <id> <json>`.
   - The receiver decrypts, deduplicates by `message_id`, appends the result to its JSONL output file, and sends `ack <message_id>`.
   - The server replies `ACK_CONFIRMED <message_id>`, removes the message from the relevant queue(s), and removes its entry from in-memory `message_status`.

7. **Admin Console to Server**:
   - Admin client connects to the admin endpoint over mTLS.
   - Server verifies the client CN is in `admin_cns` via `AclManager`.
   - Admin client sends `json` / `metrics` / `status` / `queues` / `connections` commands.
   - Web dashboard polls `/api/status` and renders the data in real-time.

8. **Message Removal**:
   - The server removes a message from all queues once it has been acknowledged.
   - The `message_queues` map entry is also removed, and the `message_status` entry is cleared.

## 5. Acknowledgment Mechanism

- **Sender–Server ACK**:
  - The server replies `ACK <message_id>` immediately after a `publish` is queued to at least one bound queue, or an `Error: ...` line if publishing failed (e.g. duplicate message ID, or no queue bound to that exchange/routing key).
  - The sender tracks every published message in a `pending` map until its ACK arrives; an `mpsc` channel carries ACK results from a dedicated reader task back to the main send loop.
  - After the send loop completes, the sender drains remaining ACKs (bounded by `ack_timeout_secs + 5`), then explicitly retries any still-pending messages up to `sender.max_retries` times with exponential backoff.
- **Receiver–Server ACK**:
  - The receiver sends `ack <message_id>` once a message has been decrypted and handed off to its local storage task.
  - The server replies `ACK_CONFIRMED <message_id>`, removes the message from its queues, and clears its `message_status` entry.
  - If the receiver's connection drops before an `ack` is sent, the unacknowledged message remains queued on the broker (and its metadata persists in PostgreSQL) until a future `consume`/`fetch` redelivers it after reconnection.
- **Independence**: Sender↔server and receiver↔server acknowledgments are fully decoupled each side only needs to track its own outstanding requests.
- **Retry responsibility**: Both clients own their respective retry logic; the broker's role is limited to queuing, delivery, and tracking acknowledgment state.

## 6. Access Control (ACL) Detailed Design

### 6.1. Role Resolution

The `AclManager` is constructed at server startup from the `[acl]` section of `config.toml`. It holds three `HashSet<String>` collections for sender, receiver, and admin CNs. When a client connects, its CN (extracted from the mTLS certificate) is looked up against these sets:

```
CN in admin_cns   → Role::Admin
CN in sender_cns  → Role::Sender
CN in receiver_cns → Role::Receiver
CN not found      → Role::Unknown → connection rejected
```

Admin is checked first to ensure admin CNs take precedence if accidentally listed in multiple sets.

### 6.2. Command-Level Enforcement

In `server.rs`, every command parsed from the client's input is checked via `acl_manager.check_command(&role, command)` before execution. If denied, the server responds with `Access denied: <reason> (role=<role>)` and the command is not executed.

### 6.3. Resource-Level Enforcement

For `declare_queue` and `bind`, an additional `check_resource` call verifies that receivers only manage resources matching their own CN. This prevents `receiver_1` from declaring `receiver_2_queue` or binding with `receiver_2_key`.

### 6.4. Admin Console ACL

The admin server (`admin_server.rs`) independently extracts the connecting client's CN from the TLS handshake and verifies it resolves to `Role::Admin` before serving any data. Non-admin clients are silently disconnected.

## 7. Rate Limiting Detailed Design

### 7.1. Token Bucket Algorithm

Each client CN gets two independent token buckets:
- **Global bucket**: Configured with `global_rps` (refill rate) and `global_burst` (max tokens). Every command consumes one token.
- **Publish bucket**: Configured with `publish_rps` and `publish_burst`. Only checked for `publish` and `publish_batch` commands.

Tokens refill at the RPS rate based on elapsed time since the last request, capped at the burst value.

### 7.2. Connection Limits

- Per-IP: Maximum `max_connections_per_ip` concurrent connections from a single IP address.
- Per-CN: Maximum `max_connections_per_cn` concurrent connections per client CN.
- Connections are tracked in `HashMap<String, usize>` behind `RwLock`, incremented on accept and decremented on disconnect.

### 7.3. Integration Points

- **Connection acceptance** (`main.rs`): `rate_limiter.check_connection(cn, ip)` is called after TLS handshake. If rejected, the connection is closed and `connections_rejected_*` counters are incremented.
- **Per-command** (`server.rs`): `rate_limiter.check(cn, ip, is_publish)` is called before each command. If rejected, the server responds with `Rate limited` and `requests_rate_limited` is incremented.

## 8. Metrics Detailed Design

### 8.1. Atomic Counters

All counters use `AtomicU64` with `Ordering::Relaxed` for maximum throughput. This means individual reads may see slightly stale values, but counters are monotonically increasing and never lose updates.

### 8.2. Snapshot Updates

The `ServerSnapshot` is updated by periodic tasks that scan DashMap sizes. It provides a point-in-time view of connected clients, queue count, total messages, and consumer count.

### 8.3. Prometheus Format

The `render_prometheus()` method outputs standard Prometheus text exposition format:
```
# HELP ciphermq_messages_published_total Total messages published
# TYPE ciphermq_messages_published_total counter
ciphermq_messages_published_total 1234
```

This can be scraped by Prometheus or any compatible monitoring system.

## 9. Logging and Verification

- **Sender**:
  - Logs key retrieval, per-message encryption, publish attempts, ACK receipt, retries, and final pipeline throughput/failure counts.
  - Example: `INFO sender: Encrypted message <message_id> for <receiver_client_id> with routing_key <routing_key>`.
- **Receiver**:
  - Logs public-key registration, queue/exchange setup, message receipt, decryption, deduplication, ACK sending, heartbeat sending, and batch persistence.
  - Example: `INFO receiver: Processed and decrypted message <message_id>`.
- **Server**:
  - Logs connections, ACL role resolution, command handling, queue/exchange/binding declarations, publish/ack outcomes, rate-limit rejections, heartbeat timeouts, and errors in structured JSON.
  - Example: `{"timestamp":"2026-06-08T07:03:00Z","level":"ERROR","fields":{"message":"TLS handshake failed"}}`.
- **Admin Console**:
  - Logs connection establishment, TLS handshake results, ACL verification, polling cycles, and dashboard readiness.
- **Verification**:
  - Check the sender's console/log output for messages that exhausted `max_retries` (`"permanently failed after {n} retries"`).
  - Check the receiver's `data/<queue_name>_received_messages.jsonl` for processed messages.
  - Check server logs for acknowledgment confirmations, ACL denials, or errors.
  - Query PostgreSQL's `message_metadata` table for message timing/status and `public_keys` for registered key material.
  - Check the admin dashboard for real-time server health, queue details, and rate-limit statistics.
  - Query the admin server's `metrics` endpoint for Prometheus-compatible metrics.
