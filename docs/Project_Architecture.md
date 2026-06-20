# CipherMQ Project Architecture: A Secure Message Queue with mTLS and Guaranteed Delivery

## 1. Introduction

**CipherMQ** is a high-performance, secure message queue system built entirely in **Rust** the broker, the Sender reference client, and the Receiver reference client are all Rust binaries. It is designed for encrypted message transmission with **zero message loss** and **exactly-once delivery**. It leverages **Mutual Transport Layer Security (mTLS)** for secure client-server communication, **hybrid encryption** (X25519 + XSalsa20-Poly1305 sealed box, then ChaCha20-Poly1305) for message confidentiality between sender and receiver, and **AES-256-GCM** for encrypting receivers' public keys at rest in a PostgreSQL database.

The project consists of:
- **Server** (`src/main.rs`, `src/server.rs`, `src/connection.rs`, `src/state.rs`, `src/auth.rs`, `src/storage.rs`, `src/config.rs`): A Rust/Tokio message broker for routing and delivering messages over mTLS, with PostgreSQL-backed persistence for metadata and public keys.
- **Sender** (`clients/Sender_1/src/main.rs`): A Rust/Tokio client that fetches receiver public keys, hybrid-encrypts messages, and sends them through an async, backpressure-bounded pipeline with retry logic.
- **Receiver** (`clients/Receiver_1/src/main.rs`): A Rust/Tokio client that registers its public key, receives, decrypts, deduplicates, and persists messages, acknowledging each as it is processed.
- **Logging**: Structured JSON logs with rotation and level-based filtering on the server; JSON file logs plus a human-readable console layer on both clients.

This document provides a comprehensive overview of the architecture, covering the server, the clients, mTLS, encryption, key distribution, acknowledgment mechanisms, and storage, all based directly on the current Rust implementation.

## 2. Architecture Overview

CipherMQ operates as a message queue with **queues** and **exchanges**, exposed over mTLS connections via a simple line-oriented, text-based protocol. Key features include:

- **Mutual TLS (mTLS)**: Secures client-server communication with two-way authentication. The server uses `tokio-rustls` with a `WebPkiClientVerifier` (`src/auth.rs`); both Rust clients use `tokio-rustls` on the client side. Server-side parameters (cert/key/CA paths, listen address) are configured via `config.toml`; client-side parameters via each client's `config.json`.
- **Hybrid Encryption**: For every message, the sender generates a random 256-bit session key and 96-bit nonce. The session key is wrapped for the receiver using an X25519 ECDH shared secret combined with XSalsa20-Poly1305 (the NaCl/libsodium "sealed box" construction); the message content itself is encrypted with ChaCha20-Poly1305 using that session key and nonce.
- **Public Key Distribution**: Receivers register their public key using the `register_public_key` command; senders retrieve a receiver's public key using `get_public_key`. Keys are stored AES-256-GCM-encrypted in PostgreSQL.
- **Zero Message Loss**: The sender retries publishing until it receives `ACK <message_id>` from the broker; the broker keeps a message in its queue (and retains its metadata in PostgreSQL) until the receiver sends `ack <message_id>`.
- **Exactly-Once Delivery**: The receiver deduplicates messages in-memory using `message_id` before processing or storing them.
- **Asynchronous Processing**: Built with Tokio throughout server and both clients for concurrent, high-performance connection handling.
- **Thread-Safe Data Structures**: The server uses `DashMap` for its in-memory queue/exchange/binding/consumer/status maps, allowing lock-free concurrent access across client-handling tasks.
- **Flexible Routing**: Supports exchanges, queues, and routing-key-based bindings, declared explicitly by clients before publishing or consuming.
- **Structured Logging**: The server writes JSON logs split by level (`INFO`, `DEBUG`, `ERROR`) into separate files, with configurable rotation (`hourly`, `daily`, or `never`). Both clients write JSON logs to a single configured file plus a readable console layer, filtered by a single configured level.
- **Persistent Storage**: The server stores message metadata and AES-256-GCM-encrypted public keys in PostgreSQL via a dedicated async storage actor task.

The architecture is illustrated in the [Sequence Diagram](diagrams/Sequence_diagram.png) and [Activity Diagram](diagrams/Activity_Diagram.png) in `docs/diagrams`.

## 3. Architectural Components

### 3.1. Server (`src/main.rs`, `src/server.rs`, `src/connection.rs`, `src/state.rs`, `src/config.rs`, `src/auth.rs`, `src/storage.rs`)

The server is the core of CipherMQ, managing message routing, delivery, secure connections via mTLS, and persistent storage in PostgreSQL.

#### 3.1.1. Data Structures (`src/state.rs`)

- **`ServerState`**: The broker's in-memory state, shared behind `Arc<RwLock<ServerState>>`:
  - `queues: DashMap<String, Vec<(String, EncryptedInputData)>>`: Messages currently held in each declared queue, awaiting consumption.
  - `bindings: DashMap<String, Vec<(String, String)>>`: For each exchange, the list of `(queue_name, routing_key)` pairs bound to it.
  - `exchanges: DashMap<String, Vec<String>>`: The set of declared exchanges.
  - `consumers: DashMap<String, Vec<mpsc::UnboundedSender<(String, EncryptedInputData)>>>`: Per-queue channels used to push newly published messages directly to subscribed consumer connections.
  - `message_status: DashMap<String, MessageStatus>`: Per-message `sent_time` / `delivered_time` / `acknowledged_time` (as `Option<std::time::Instant>`), used to track in-flight messages; the entry is removed once the message is acknowledged.
  - `connected_clients: Arc<RwLock<usize>>`: Count of currently connected clients, incremented/decremented as TLS connections open and close.
  - `storage: Arc<Storage>`: Handle to the PostgreSQL-backed storage actor.
- **`EncryptedInputData`**: The wire format for one encrypted message body, exchanged in the `publish` / `publish_batch` / `consume` / `fetch` commands:
  - `message_id: String`: Unique identifier for deduplication, constructed by the sender as `"{sender_id}-{correlation_id}-{receiver_client_id}"`.
  - `receiver_client_id: String`: The target receiver's client ID.
  - `enc_session_key: String`: Base64-encoded sealed-box ciphertext (32-byte ephemeral X25519 public key followed by the XSalsa20-Poly1305-encrypted session key).
  - `nonce: String`: Base64-encoded 96-bit ChaCha20-Poly1305 nonce for the message payload.
  - `ciphertext: String`: Base64-encoded ChaCha20-Poly1305 ciphertext of the message content (the Poly1305 authentication tag is appended to this ciphertext by the AEAD crate; CipherMQ does not transmit a separate `tag` field for message payloads: That field exists only in the server's public-key storage schema, described in §3.1.3).

#### 3.1.2. Connection Handling (`src/connection.rs`, `src/auth.rs`)

- **mTLS**: The server uses `tokio-rustls` with ECDSA P-384 certificates for both the server identity and client authentication.
- **Authentication**: `auth.rs` builds a `rustls::ServerConfig` from `cert_path`/`key_path`/`ca_cert_path` and a `WebPkiClientVerifier` built from the CA's root store, requiring every connecting client to present a certificate signed by that CA.
- **Client identity**: `connection.rs` extracts the client's `client_id` directly from the Common Name (CN) field of its TLS peer certificate once the handshake completes there is no separate login or identity command.
- **Protocol**: A line-oriented text protocol over the TLS stream. Each connection is handled by `handle_client` (`src/server.rs`) in a loop that reads one command per line and, concurrently, forwards any messages pushed to that client's consumer channel. Supported commands:
  - `declare_queue <name>`
  - `declare_exchange <name>`
  - `bind <queue> <exchange> <routing_key>`
  - `publish <exchange> <routing_key> <json_message>`
  - `publish_batch <exchange> <routing_key> <json_array_of_messages>`
  - `consume <queue>`: Registers the connection as a push consumer for that queue.
  - `fetch <queue>`: Pull-style alternative: returns one message from the queue if available, or `No messages`.
  - `ack <message_id>`: Acknowledges and removes a message from all queues and from `message_status`.
  - `resend <message_id>`: Client-initiated resend request; the server only logs and confirms the request, after checking via storage that the requesting client owns that message.
  - `register_public_key <base64_public_key>`
  - `get_public_key <client_id>`

#### 3.1.3. Storage (`src/storage.rs`)

The server talks to PostgreSQL through a single background task (a "storage actor") that owns the database client and processes commands sent to it over an `mpsc` channel, so all database access is serialized through one place even though many client connections run concurrently.

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
- **Encryption at rest**: Public keys are encrypted with **AES-256-GCM** using the 32-byte key configured at `[encryption].aes_key` in `config.toml`. The ciphertext and tag produced by the `aes-gcm` crate are split apart before storage and concatenated again on read.

#### 3.1.4. Configuration (`src/config.rs`)

- Loads and parses `config.toml` (sections: `[server]`, `[tls]`, `[logging]`, `[database]`, `[encryption]`).
- Validates that `tls.cert_path`, `tls.key_path`, and `tls.ca_cert_path` are present when `connection_type = "tls"`.
- Validates that `database.host`, `database.user`, and `database.dbname` are non-empty.
- Validates that `encryption.algorithm` is exactly `"x25519_chacha20_poly1305"` and that `encryption.aes_key` Base64-decodes to exactly 32 bytes.
- Applies defaults for any empty/zero logging fields (`level` → `"info"`, `rotation` → `"daily"`, log file paths → `logs/info.log` / `logs/debug.log` / `logs/error.log`, `max_size_mb` → `10`).

#### 3.1.5. Logging

- Structured JSON logs written to separate files per level: `server_info.log`, `server_debug.log`, `server_error.log` (paths configurable), using `tracing` + `tracing-appender` rolling file writers.
- Configurable rotation (`hourly`, `daily`, `never`) and an additional pretty-printed console layer for local development.
- Example JSON log line: `{"timestamp":"2026-06-08T07:03:00Z","level":"INFO","fields":{"message":"Published message <message_id> to exchange 'my_exchange' with routing key 'my_key'"}}`.

### 3.2. Sender (`clients/Sender_1/src/main.rs`)

- **Role**: Fetches receiver public keys, hybrid-encrypts messages, and sends them through an asynchronous, backpressure-bounded pipeline with automatic retries.
- **mTLS**: Authenticates with `tls.client_cert_path` / `tls.client_key_path` from `config.json`, verifies the server using `tls.certificate_path` (the CA certificate). When `tls.check_hostname` is `false`, certificate-chain validation still runs, but the TLS `ServerName` hostname check is skipped via a custom `ServerCertVerifier`.
- **Identity**: The sender's own `client_id` is not configured explicitly it is extracted from the CN field of its own client certificate at startup (`extract_client_id`) and used to populate the `{sender_id}` template placeholder and the generated `message_id`.
- **Encryption**: For each message and each target receiver, generates a fresh random 256-bit session key and 96-bit nonce; wraps the session key for that receiver with X25519 + XSalsa20-Poly1305 sealed-box encryption (`sealed_box_encrypt`), and encrypts the message body with ChaCha20-Poly1305 (`encrypt_message`). A separate ciphertext/session-key pair is generated per receiver, so compromising one receiver's private key does not expose messages addressed to other receivers.
- **Commands used**:
  - `declare_queue <queue>`, `declare_exchange <exchange>`, `bind <queue> <exchange> <routing_key>`: Issued once per configured binding before publishing.
  - `get_public_key <receiver_client_id>`: Retrieves a receiver's public key (and caches it to `keys/<receiver_client_id>_public.key`).
  - `publish <exchange> <routing_key> <json_message>`: Sends one encrypted message; the server replies with `ACK <message_id>` or an `Error:` line.
- **Pipeline**: Opens a dedicated mTLS connection for the send/ACK pipeline. A `tokio::sync::Semaphore` sized to `sender.max_inflight` bounds how many messages may be outstanding at once; a separate task continuously reads ACK lines from the same connection and releases semaphore permits as ACKs arrive. After all messages have been sent, the sender drains remaining ACKs up to a timeout, then retries any still-unacknowledged messages up to `sender.max_retries` times with exponential backoff, before reporting final throughput and failure counts.
- **Logging**: JSON logs to `logging.info_file_path`, plus a readable console layer, both filtered by `logging.level`.

### 3.3. Receiver (`clients/Receiver_1/src/main.rs`)

- **Role**: Registers its public key, receives, decrypts, deduplicates, and stores messages in `data/<queue_name>_received_messages.jsonl`, acknowledging each as it is processed.
- **mTLS**: Authenticates with `tls.client_cert_path` / `tls.client_key_path`, verifies the server using `tls.certificate_path`. Same `check_hostname` behavior as the Sender.
- **Decryption**: Loads its X25519 private key from `receiver_private.key` (Base64, 32 bytes) at a path relative to the client working directory (`create_ca_key/Rust_Key_Maker_X25519/receiver_private.key`). For each incoming message it reverses the sender's scheme: unwraps the session key with the sealed-box construction (X25519 ECDH + XSalsa20-Poly1305) using its own private key and the ephemeral public key embedded in `enc_session_key`, then decrypts the payload with ChaCha20-Poly1305 using that session key and the message's `nonce`.
- **Commands used**:
  - `register_public_key <base64_public_key>`: Registers this receiver's X25519 public key with the broker; on success the server replies `Public key registered`.
  - `declare_queue <queue>`, `declare_exchange <exchange>`, `bind <queue> <exchange> <routing_key>`: Issued once at startup using `queue_name` / `exchange_name` / `routing_key` from `config.json`.
  - `consume <queue>`: Subscribes as a push consumer; the broker then streams `Message: <id> <json>` lines as they are published.
  - `ack <message_id>`: Sent after a message has been successfully decrypted and queued for local storage.
- **Deduplication**: Maintains an in-memory `HashSet<String>` of processed `message_id`s for the lifetime of the connection/process, skipping any message whose ID has already been seen.
- **Persistence**: Decrypted messages are batched (up to 100 at a time, or on a 5-second idle timeout) and appended as JSON Lines to `data/<queue_name>_received_messages.jsonl`, each record containing `message_id`, the decrypted `message` content, and a Unix `timestamp`.
- **Resilience**: Automatically reconnects (with a 1-second pause) if the TLS connection drops, and responds to `Ctrl+C` (`SIGINT`) by draining in-flight work before exiting.
- **Logging**: JSON logs to `logging.info_file_path`, plus a readable console layer, filtered by `logging.level`.

### 3.4. Hybrid Encryption

- **Sender** (per message, per receiver):
  1. Looks up the receiver's X25519 public key (fetched live via `get_public_key`, or read from a local cache under `keys/`).
  2. Generates a random 256-bit session key and 96-bit nonce.
  3. Wraps the session key for the receiver using a NaCl-style sealed box: an ephemeral X25519 key pair is generated, an X25519 shared secret is computed with the receiver's public key, a nonce is derived by hashing the ephemeral and receiver public keys with BLAKE2b, and the session key is encrypted with XSalsa20-Poly1305 under that shared secret and derived nonce. The ephemeral public key is prepended to the resulting ciphertext to form `enc_session_key`.
  4. Encrypts the plaintext message content with ChaCha20-Poly1305 under the session key and the random nonce, producing `ciphertext` (with the Poly1305 tag appended) and `nonce`.
- **Receiver** (per incoming message):
  1. Splits `enc_session_key` into the embedded ephemeral public key and the sealed-box ciphertext.
  2. Recomputes the X25519 shared secret using its own private key and the embedded ephemeral public key, rederives the same BLAKE2b-based nonce, and decrypts the sealed box with XSalsa20-Poly1305 to recover the session key.
  3. Decrypts `ciphertext` with ChaCha20-Poly1305 using the recovered session key and `nonce`, verifying the Poly1305 tag and recovering the plaintext message content.

### 3.5. Public Key Distribution

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

## 4. Component Interactions

1. **Key and Certificate Generation**:
   - `create_ca_key/Rust_CA_Maker_ECDSA_P-384_Multi_Client` (a Rust binary using `rcgen`) generates `ca.crt`/`ca.key`, `server.crt`/`server.key`, and one `client.crt`/`client.key` pair per CN argument supplied on the command line.
   - `create_ca_key/Rust_Key_Maker_X25519` (a Rust binary using `x25519-dalek`) generates `receiver_public.key` and `receiver_private.key`.

2. **Receiver to Server**:
   - Receiver authenticates via mTLS, then sends `register_public_key <base64_public_key>`.
   - Declares its queue and exchange, binds them with its routing key, and subscribes with `consume`.

3. **Sender to Server**:
   - Sender authenticates via mTLS, declares/binds its configured exchange and queues, then sends `get_public_key <receiver_client_id>` for each configured receiver.
   - Hybrid-encrypts and publishes messages with `publish <exchange> <routing_key> <json_message>`.
   - Server replies `ACK <message_id>` for each successfully queued message.

4. **Server to Receiver**:
   - The server routes each published message to every queue bound to its exchange/routing-key pair and pushes it immediately to any subscribed consumer as `Message: <id> <json>`.
   - The receiver decrypts, deduplicates by `message_id`, appends the result to its JSONL output file, and sends `ack <message_id>`.
   - The server replies `ACK_CONFIRMED <message_id>`, removes the message from the relevant queue(s), and removes its entry from in-memory `message_status`.

5. **Message Removal**:
   - The server removes a message from all queues once it has been acknowledged.
   - Logged as: `{"timestamp":"...","level":"INFO","fields":{"message":"Message <message_id> acknowledged by client, removed from queues and status"}}`.

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

## 6. Logging and Verification

- **Sender**:
  - Logs key retrieval, per-message encryption, publish attempts, ACK receipt, retries, and final pipeline throughput/failure counts.
  - Example: `INFO sender: Encrypted message <message_id> for <receiver_client_id> with routing_key <routing_key>`.
- **Receiver**:
  - Logs public-key registration, queue/exchange setup, message receipt, decryption, deduplication, ACK sending, and batch persistence.
  - Example: `INFO receiver: Processed and decrypted message <message_id>`.
- **Server**:
  - Logs connections, command handling, queue/exchange/binding declarations, publish/ack outcomes, and errors in structured JSON.
  - Example: `{"timestamp":"2026-06-08T07:03:00Z","level":"ERROR","fields":{"message":"TLS handshake failed"}}`.
- **Verification**:
  - Check the sender's console/log output for messages that exhausted `max_retries` (`"permanently failed after {n} retries"`).
  - Check the receiver's `data/<queue_name>_received_messages.jsonl` for processed messages.
  - Check server logs for acknowledgment confirmations or errors.
  - Query PostgreSQL's `message_metadata` table for message timing/status and `public_keys` for registered key material.
