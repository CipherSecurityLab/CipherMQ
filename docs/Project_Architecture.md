# CipherMQ Project Architecture: A Secure Message Queue with mTLS and Guaranteed Delivery

## 1. Introduction

**CipherMQ** is a high-performance, secure message queue system built in Rust, designed for encrypted message transmission with **zero message loss** and **exactly-once delivery**. It leverages **Mutual Transport Layer Security (mTLS)** for secure client-server communication and **AES-256-GCM** encryption for storing public keys in a PostgreSQL database. Messages are temporarily held in memory, routed via exchanges and queues, and managed with robust acknowledgment mechanisms. This version updates the storage backend from SQLite to PostgreSQL, enhances logging with structured JSON output, and maintains mTLS and hybrid encryption (x25519 + AES-GCM-256) for message confidentiality.

The project consists of:
- **Server** (`main.rs`): A Rust-based message queue for routing and delivering messages over mTLS.
- **Sender** (`sender.py`): A Python script that fetches receiver public keys, encrypts, and sends messages in batches with retry logic.
- **Receiver** (`receiver.py`): A Python script that registers its public key, receives, decrypts, deduplicates, and stores messages.
- **Logging**: Structured JSON logs with rotation and level-based filtering, replacing timestamped console logs.

This document provides a comprehensive overview of the architecture, covering server, clients, mTLS, encryption, key distribution, acknowledgment mechanisms, and storage.

## 2. Architecture Overview

CipherMQ operates as a message queue with **queues** and **exchanges**, supporting mTLS connections via a text-based protocol. Key features include:
- **Mutual TLS (mTLS)**: Secures client-server communication with two-way authentication using `tokio-rustls` (server) and Python’s `ssl` module (clients), configurable via `config.toml`.
- **Hybrid Encryption**: Combines x25519 for session key encryption and AES-GCM-256 for message encryption and authentication.
- **Public Key Distribution**: Receivers register public keys using `register_key`, and senders retrieve them using `get_public_key`, with keys stored securely in PostgreSQL.
- **Zero Message Loss**: Sender and server retries with acknowledgments ensure reliable delivery.
- **Exactly-Once Delivery**: Receiver deduplicates messages using `message_id`.
- **Asynchronous Processing**: Built with Tokio for concurrent, high-performance connection handling.
- **Thread-Safe Data Structures**: Uses `DashMap` for multi-threaded operations.
- **Flexible Routing**: Supports exchanges, queues, and routing keys.
- **Structured Logging**: JSON-based logging with rotation (`hourly`, `daily`, or `never`) and level-based filtering (`INFO`, `DEBUG`, `ERROR`).
- **Persistent Storage**: Stores message metadata and encrypted public keys in PostgreSQL.

The architecture is illustrated in the [Sequence Diagram](diagrams/Sequence_diagram.png) and [Activity Diagram](diagrams/Activity_Diagram.png) in `docs/diagrams`.

## 3. Architectural Components

### 3.1. Server (`main.rs`, `server.rs`, `connection.rs`, `state.rs`, `config.rs`, `auth.rs`, `storage.rs`)

The server is the core of CipherMQ, managing message routing, delivery, secure connections with mTLS, and persistent storage in PostgreSQL.

#### 3.1.1. Data Structures (`state.rs`)
- **ServerState**:
  - `queues: DashMap<String, Vec<(String, EncryptedInputData)>>`: Stores messages for each queue.
  - `bindings: DashMap<String, Vec<String>>`: Maps exchanges to queues.
  - `exchanges: DashMap<String, Vec<String>>`: Stores exchange definitions.
  - `consumers: DashMap<String, Vec<mpsc::UnboundedSender<(String, EncryptedInputData)>>>`: Tracks consumer channels.
  - `message_status: DashMap<String, MessageStatus>`: Tracks statuses (`Sent`, `Delivered`, `Acknowledged`).
  - `request_times: DashMap<String, Vec<f64>>`: Records request processing times.
  - `connected_clients: Arc<RwLock<usize>>`: Tracks active client count.
- **EncryptedInputData**:
  - `message_id: String`: Unique UUID for deduplication.
  - `receiver_client_id: String`: Target receiver identifier.
  - `enc_session_key: String`: x25519-encrypted session key (base64-encoded).
  - `nonce: String`: AES-GCM-256 nonce (12 bytes, base64-encoded).
  - `tag: String`: AES-GCM-256 authentication tag (16 bytes, base64-encoded).
  - `ciphertext: String`: Encrypted message content.

#### 3.1.2. Connection Handling (`connection.rs`, `auth.rs`)
- **mTLS**: Uses `tokio-rustls` for secure connections with ECDSA P-384 certificates.
- **Authentication**: Verifies client certificates against `ca.crt` and server certificate (`server.crt`, `server.key`).
- **Commands**: Supports `declare_queue`, `declare_exchange`, `bind`, `publish`, `publish_batch`, `consume`, `fetch`, `ack`, `resend`, `register_public_key`, `get_public_key`.

#### 3.1.3. Storage (`storage.rs`)
- **PostgreSQL Database**:
  - **message_metadata**:
    - `message_id` (TEXT, PRIMARY KEY)
    - `client_id` (TEXT)
    - `exchange_name` (TEXT)
    - `routing_key` (TEXT)
    - `sent_time` (TEXT)
    - `delivered_time` (TEXT)
    - `acknowledged_time` (TEXT)
  - **public_keys**:
    - `client_id` (TEXT, PRIMARY KEY)
    - `public_key_ciphertext` (TEXT)
    - `nonce` (TEXT)
    - `tag` (TEXT)
- **Encryption**: Public keys are encrypted with ChaCha20-Poly1305 using a 32-byte key from `config.toml`.

#### 3.1.4. Configuration (`config.rs`)
- Loads `config.toml` with sections for server, TLS, logging, database, and encryption.
- Validates AES key length and required fields.

#### 3.1.5. Logging
- Structured JSON logs written to `server_info.log`, `server_debug.log`, and `server_error.log`.
- Configurable rotation (`hourly`, `daily`, `never`) and max size (e.g., 100 MB).
- Example: `{"timestamp":"2025-09-08T07:03:00Z","level":"INFO","message":"Published message <message_id> to queue 'my_queue' via exchange 'my_exchange'"}`.

### 3.2. Sender (`sender.py`)
- **Role**: Fetches receiver public keys, encrypts messages with hybrid encryption, and sends them in batches.
- **mTLS**: Authenticates with `client.crt`, `client.key`, and verifies server with `ca.crt`.
- **Encryption**: Uses x25519 to encrypt session key and AES-GCM-256 for message content.
- **Commands**:
  - `get_key <receiver_client_id>`: Retrieves public key.
  - `publish <exchange_name> <routing_key> <json_message>`: Sends single message.
  - `publish_batch <exchange_name> <routing_key> <json_messages>`: Sends multiple messages.
- **Retry Logic**: Retries up to 3 times (10-second timeout) until server sends `ACK <message_id>`.
- **Logging**: Console logs with timestamps (e.g., `[2025-09-08 07:03:00] [SENDER] Server ACK received for message <message_id>`).

### 3.3. Receiver (`receiver.py`)
- **Role**: Registers public key, receives, decrypts, deduplicates, and stores messages in `data/received_messages.jsonl`.
- **mTLS**: Authenticates with `client.crt`, `client.key`, and verifies server with `ca.crt`.
- **Decryption**: Uses private key (`receiver_private.key`) for session key and AES-GCM-256 for message content.
- **Commands**:
  - `register_key <receiver_client_id> <public_key>`: Registers public key.
  - `consume <queue_name>`: Subscribes to queue.
  - `ack <message_id>`: Acknowledges message.
- **Deduplication**: Uses `message_id` to prevent reprocessing.
- **Retry Logic**: Retries `ack` up to 3 times (10-second timeout).
- **Logging**: Console logs with timestamps (e.g., `[2025-09-08 07:03:00] [RECEIVER] Decrypted message <message_id>`).

### 3.4. Hybrid Encryption
- **Sender**:
  1. Fetches receiver’s public key using `get_key <receiver_client_id>`.
  2. Generates session key, encrypts it with x25519, and encrypts message with AES-GCM-256, producing `ciphertext`, `nonce`, and `tag`.
- **Receiver**:
  1. Decrypts session key with private key (`receiver_private.key`).
  2. Decrypts message with AES-GCM-256, verifying `tag`.
- **Security**: Ensures confidentiality and authenticity.

### 3.5. Public Key Distribution
- **Receiver**:
  - Generates x25519 key pair (`receiver_private.key`, `receiver_public.key`).
  - Sends `register_key <receiver_client_id> <public_key>` to server.
  - Server encrypts public key with AES-256-GCM and stores in PostgreSQL.
- **Sender**:
  - Sends `get_public_key <receiver_client_id>` to retrieve public key.
  - Stores public key locally as `certs/<receiver_client_id>_public.key`.
- **Security**:
  - Public keys are encrypted in PostgreSQL.
  - mTLS restricts key registration and retrieval to authenticated clients.

## 4. Component Interactions
1. **Key and Certificate Generation**:
   - OpenSSL generates `ca.crt`, `server.crt`, `server.key`, `client.crt`, and `client.key` for mTLS.
   - Python script (`key_maker.py`) generates `receiver_public.key` and `receiver_private.key`.

2. **Receiver to Server**:
   - Receiver authenticates with mTLS, sends `register_key <receiver_client_id> <public_key>`.
   - Declares `my_queue`, binds to `my_exchange` with `my_key`, and subscribes with `consume`.

3. **Sender to Server**:
   - Sender authenticates with mTLS, sends `get_public_key <receiver_client_id>`.
   - Encrypts messages, sends batches to `my_exchange` with `my_key`.
   - Server sends `ACK <message_id>` for each message.
   - Sender logs: `[2025-09-08 07:03:00] [SENDER] Server ACK received for message <message_id>`.

4. **Server to Receiver**:
   - Server routes messages to `my_queue` and pushes to consumers.
   - Receiver decrypts, deduplicates, stores messages in `data/received_messages.jsonl`, and sends `ack <message_id>`.
   - Server sends `ACK_CONFIRMED <message_id>`, receiver logs: `[2025-09-08 07:03:00] [RECEIVER] Server confirmed ACK`.
   - Server removes message from `queues` and updates `message_status`.

5. **Message Removal**:
   - Server removes message from `queues` after `ack <message_id>`.
   - Logs: `{"timestamp":"2025-09-08T07:03:00Z","level":"INFO","message":"Message <message_id> acknowledged by client, removed from queues and status"}`.

## 5. Acknowledgment Mechanism
- **Sender-Server ACK**:
  - Server sends `ACK <message_id>` after queuing message.
  - Sender retries up to 3 times (10-second timeout) until ACK received.
  - Sender removes message from `pending_messages` upon ACK.
  - Logged: `[2025-09-08 07:03:00] [SENDER] Server ACK received`.
- **Receiver-Server ACK**:
  - Receiver sends `ack <message_id>` after processing.
  - Server sends `ACK_CONFIRMED <message_id>`, removes message from `queues` and `message_status`.
  - Receiver retries up to 3 times (10-second timeout).
  - Logged: `[2025-09-08 07:03:00] [RECEIVER] Server confirmed ACK`.
- **Independence**: Sender and receiver ACKs are decoupled.
- **Retry Logic**: Clients handle retries; server focuses on queuing and delivery.

## 6. Logging and Verification
- **Sender**:
  - Logs encryption, key retrieval, batching, sending, ACK receipt, and message removal with timestamps.
  - Example: `[2025-09-08 07:03:00] [SENDER] Encrypted message <message_id> for <receiver_client_id>`.
- **Receiver**:
  - Logs key registration, message receipt, decryption, ACK sending, confirmation, and storage with timestamps.
  - Example: `[2025-09-08 07:03:00] [RECEIVER] Decrypted message <message_id>: <message>`.
- **Server**:
  - Logs connections, key registration, message statuses, and errors in JSON format.
  - Example: `{"timestamp":"2025-09-08T07:03:00Z","level":"ERROR","message":"TLS handshake failed"}`.
- **Verification**:
  - Check sender’s `pending_messages` for unacknowledged messages.
  - Check receiver’s `data/received_messages.jsonl` for processed messages.
  - Check server logs for `Acknowledged` status or errors.
  - Query PostgreSQL `message_metadata` for message status and `public_keys` for key storage.