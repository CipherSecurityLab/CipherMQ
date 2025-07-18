# CipherMQ Project Architecture: A Secure Message Broker with mTLS and Guaranteed Delivery



## 1. Introduction

**CipherMQ** is a high-performance, secure message broker designed to transmit encrypted messages between senders and receivers using a push-based architecture. It ensures **zero message loss** and **exactly-once delivery** through robust acknowledgment mechanisms, with messages temporarily held in memory (except for receiver output). The system leverages **hybrid encryption** (x25519 + AES-GCM-256) for message confidentiality and authenticity and **Mutual Transport Layer Security (mTLS)** for secure client-server communication with two-way authentication. Public keys are securely stored in an SQLite database with AES-GCM encryption, and receivers register their public keys with the server for secure distribution to senders.

This version introduces **mTLS support** and **timestamped console logs**, enhancing transport-layer security and monitoring. Logging is simplified to console output with timestamps (e.g., `[2025-07-14 10:05:00] [SENDER] Message sent`). The project consists of:

- **Server** (`main.rs`): A Rust-based message broker for routing and delivering messages over mTLS.
- **Sender** (`sender.py`): A Python script that fetches receiver public keys, encrypts, and sends messages in batches with retry logic.
- **Receiver** (`receiver.py`): A Python script that registers its public key, receives, decrypts, deduplicates, and stores messages.

This document provides a comprehensive overview of the architecture, covering server, clients, mTLS, encryption, key distribution, and acknowledgment mechanisms.



## 2. Architecture Overview

CipherMQ operates as a message broker with **queues** and **exchanges**, supporting mTLS connections via a text-based protocol. Key features include:

- **Mutual TLS (mTLS)**: Secures client-server communication with two-way authentication using `tokio-rustls` (server) and Python’s `ssl` module (clients), configurable via `config.toml` and `config.json`.
- **Hybrid Encryption**: Combines x25519 for session key encryption and AES-GCM-256 for message encryption and authentication.
- **Public Key Distribution**: Receivers register public keys using `register_key`, and senders retrieve them using `get_key`, with keys stored securely in SQLite.
- **Zero Message Loss**: Sender and server retries with acknowledgments ensure reliable delivery.
- **Exactly-Once Delivery**: Receiver deduplicates messages using `message_id`.
- **Asynchronous Processing**: Built with Tokio for concurrent, high-performance connection handling.
- **Thread-Safe Data Structures**: Uses `DashMap` for multi-threaded operations.
- **Flexible Routing**: Supports exchanges, queues, and routing keys.
- **Timestamped Console Logs**: Console output with timestamps for monitoring (e.g., `[2025-07-14 10:05:00] [RECEIVER] Decrypted message`).

The architecture is illustrated in the [Sequence Diagram](diagrams/Sequence_diagram.png) and [Activity Diagram](diagrams/Activity_Diagram.png) in `docs/diagrams`.



## 3. Architectural Components

### 3.1. Server (`main.rs`, `server.rs`, `connection.rs`, `state.rs`, `config.rs`, `auth.rs`, `storage.rs`)

The server is the core of CipherMQ, managing message routing, delivery, secure connections with mTLS, and public key storage.

#### 3.1.1. Data Structures (`state.rs`)

- **ServerState**:
  - `queues: DashMap<String, Vec<(String, EncryptedInputData)>>`: Stores messages for each queue.
  - `bindings: DashMap<String, Vec<String>>`: Maps exchanges to queues.
  - `exchanges: DashMap<String, Vec<String>>`: Stores exchange definitions.
  - `consumers: DashMap<String, Vec<mpsc::UnboundedSender<(String, EncryptedInputData)>>>`: Tracks consumer channels.
  - `message_status: DashMap<String, MessageStatus>`: Tracks message statuses (`Sent`, `Delivered`, `Acknowledged`).
  - `request_times: DashMap<String, Vec<f64>>`: Records request processing times.
  - `connected_clients: Arc<RwLock<usize>>`: Tracks active client count.
- **EncryptedInputData**:
  - `message_id: String`: Unique UUID for deduplication.
  - `receiver_client_id: String`: Target receiver identifier.
  - `enc_session_key: String`: x25519-encrypted session key (base64-encoded).
  - `nonce: String`: AES-GCM nonce (12 bytes, base64-encoded).
  - `tag: String`: AES-GCM authentication tag (16 bytes, base64-encoded).
  - `ciphertext: String`: Encrypted message content (base64-encoded).
- **MessageStatus**:
  - `sent_time: Option<Instant>`: Time message was queued.
  - `delivered_time: Option<Instant>`: Time message was delivered.
  - `acknowledged_time: Option<Instant>`: Time message was acknowledged.

#### 3.1.2. Key Server Functions (`server.rs`, `state.rs`, `storage.rs`)

- **declare_queue**: Creates a new queue in `queues`.
- **declare_exchange**: Creates a new exchange in `exchanges`.
- **bind_queue**: Binds a queue to an exchange with a routing key, stored in `bindings`.
- **publish**: Queues messages, sends `ACK <message_id>` to sender, and delivers to consumers via `consumers`. Checks for duplicate `message_id`s.
- **publish_batch**: Processes a batch of messages, sending individual `ACK <message_id>` responses.
- **register_key**: Stores a receiver’s public key in SQLite with AES-GCM encryption, identified by `client_id`.
- **get_key**: Retrieves a receiver’s public key from SQLite for a sender.
- **register_consumer**: Registers a consumer channel for push-based delivery.
- **consume**: Retrieves messages manually (pull mode).
- **acknowledge**: Marks message as `Acknowledged`, removes it from `queues`, and sends `ACK_CONFIRMED <message_id>` to receiver.
- **get_stats**: Returns JSON stats (queue sizes, consumer counts, message statuses).
- **reset_stats**: Clears `message_status` and `request_times`.

#### 3.1.3. Connection Management (`connection.rs`, `main.rs`, `auth.rs`)

- **Protocol**: Text-based protocol over mTLS (e.g., `publish_batch <exchange> <routing_key> <message_json>`).
- **mTLS Support**:
  - Uses `tokio-rustls` with `WebPkiClientVerifier` for two-way authentication (`auth.rs`).
  - Loads server certificates (`server.crt`, `server.key`) and CA certificate (`ca.crt`) for client verification via `config.toml`.
  - Clients load `client.crt`, `client.key`, and `ca.crt` for server verification via `config.json`.
- **Asynchronous Handling**: Tokio’s `TcpListener` and `tokio::select!` manage concurrent connections.
- **Acknowledgment**:
  - Sends `ACK <message_id>` to sender after queuing.
  - Sends `ACK_CONFIRMED <message_id>` to receiver after acknowledgment.
- **Error Handling**: Logs errors via `tracing` (e.g., `[2025-07-14 10:05:00] TLS handshake failed`) and sends error messages to clients.

### 3.2. Sender (`sender.py`)

The sender fetches receiver public keys, encrypts, and sends messages in batches with retry logic.

#### 3.2.1. Key Components

- **get_public_key**:
  - Sends `get_key <receiver_client_id>` to server to retrieve the receiver’s public key, stored locally as `certs/<receiver_client_id>_public.key`.
- **encrypt_message**:
  - Generates a 32-byte session key, encrypts it with x25519 (`SealedBox` from `pynacl`), and encrypts the message with AES-GCM-256.
  - Outputs JSON with `message_id` (UUID), `receiver_client_id`, `enc_session_key`, `nonce`, `tag`, and `ciphertext` (all base64-encoded).
- **send_message_batch**:
  - Sends a batch of messages, retries up to 3 times (10-second timeout) until `ACK <message_id>` is received for each message.
  - Stores messages in `pending_messages` until acknowledged.
  - Logs: `[2025-07-14 10:05:00] [SENDER] Server ACK received for message <message_id>`.
- **encrypt_message_batch**:
  - Collects messages into batches (up to 2 messages by default).
  - Ensures all queued messages in `message_queue` are encrypted and sent.
- **mTLS Support**:
  - Uses `ssl.SSLContext` with `PROTOCOL_TLS_CLIENT`, loads `ca.crt` for server verification, and `client.crt`, `client.key` for client authentication.
  - Disables hostname verification for self-signed certificates (`check_hostname=False`).
- **Configuration**:
  - Reads `sender_config.json` for `receiver_client_ids`, `exchange_name`, `routing_key`, `server_address`, `server_port`, `certificate_path`, `client_cert_path`, and `client_key_path`.

#### 3.2.2. Architectural Features

- **Reliability**: Retries and batching ensure no message loss.
- **Security**: mTLS secures transport; hybrid encryption protects messages.
- **Logging**: Console logs with timestamps for encryption, sending, batching, and ACK receipt.
- **Deduplication**: Uses UUID to prevent server-side duplicates.

### 3.3. Receiver (`receiver.py`)

The receiver registers its public key, retrieves, decrypts, and stores messages.

#### 3.3.1. Key Components

- **register_public_key**:
  - Sends `register_key receiver_1 <public_key>` to server to store the public key in SQLite with AES-GCM encryption.
- **decrypt_message_hybrid**:
  - Decrypts session key with x25519 private key (`SealedBox` from `pynacl`) and message with AES-GCM-256.
  - Verifies integrity using `tag`.
- **receive_messages**:
  - Subscribes to a queue, receives messages, and deduplicates using `processed_messages` set.
  - Sends `ack <message_id>` with retries (3 attempts, 10-second timeout) until `ACK_CONFIRMED <message_id>` is received.
  - Logs: `[2025-07-14 10:05:00] [RECEIVER] Server confirmed ACK for message <message_id>`.
- **process_messages**:
  - Stores decrypted messages in `data/received_messages.jsonl` with `message_id` and timestamp.
  - Batches writes (up to 10 messages) for efficiency.
- **signal_handler**: Ensures graceful shutdown on SIGINT.
- **mTLS Support**:
  - Uses `ssl.SSLContext` to load `ca.crt` for server verification and `client.crt`, `client.key` for client authentication.
  - Logs cipher (e.g., `[2025-07-14 10:05:00] [RECEIVER] Cipher: TLS_AES_256_GCM_SHA384`).
- **Configuration**:
  - Reads `receiver_config.json` for `queue_name`, `exchange_name`, `routing_key`, `server_address`, `server_port`, `certificate_path`, `client_cert_path`, and `client_key_path`.

#### 3.3.2. Architectural Features

- **Deduplication**: Prevents reprocessing using `processed_messages`.
- **Reliability**: Retries ACKs to ensure server cleanup.
- **Security**: mTLS and hybrid encryption ensure secure transport and message integrity.
- **Logging**: Console logs with timestamps for receipt, decryption, ACKs, and file writes.

### 3.4. Hybrid Encryption

- **x25519**: Encrypts a 32-byte session key with `SealedBox` (using `receiver_public.key`).
- **AES-GCM-256**: Encrypts message, generates `nonce` (12 bytes) and `tag` (16 bytes) for authentication.
- **Process**:
  1. Sender retrieves receiver’s public key using `get_key <receiver_client_id>`.
  2. Sender generates session key, encrypts it with x25519, and encrypts message with AES-GCM-256, producing `ciphertext`, `nonce`, and `tag`.
  3. Receiver decrypts session key with private key (`receiver_private.key`) and message with AES-GCM-256, verifying `tag`.

### 3.5. Public Key Distribution

- **Receiver**:

  - Generates x25519 key pair (`receiver_private.key`, `receiver_public.key`).
  - Sends `register_key receiver_1 <public_key>` to server upon connection.
  - Server stores public key in SQLite with AES-GCM encryption.

- **Sender**:

  - Sends `get_key receiver_1` to retrieve receiver’s public key.
  - Stores public key locally as `certs/receiver_1_public.key` for encryption.

- **Security**:

  - Public keys are encrypted in SQLite to prevent unauthorized access.

  - mTLS ensures only authenticated clients can register or retrieve keys.

    

## 4. Component Interactions

1. **Key and Certificate Generation**:

   - OpenSSL generates `ca.crt`, `server.crt`, `server.key`, `client.crt`, and `client.key` for mTLS.
   - Python script generates `receiver_public.key` and `receiver_private.key` for hybrid encryption.

2. **Receiver to Server**:

   - Receiver authenticates with `client.crt` and `client.key`, verifies server with `ca.crt`.
   - Receiver sends `register_key receiver_1 <public_key>` to store its public key.
   - Receiver declares `my_queue`, binds to `my_exchange` with `my_key`, and subscribes with `consume`.

3. **Sender to Server**:

   - Sender authenticates with `client.crt` and `client.key`, verifies server with `ca.crt`.
   - Sender sends `get_key receiver_1` to retrieve receiver’s public key.
   - Sender encrypts messages, sends batches to `my_exchange` with `my_key`.
   - Server queues messages, sends `ACK <message_id>` for each.
   - Sender logs: `[2025-07-14 10:05:00] [SENDER] Server ACK received for message <message_id>`.

4. **Server to Receiver**:

   - Server routes messages to `my_queue` and pushes to consumers.
   - Receiver deduplicates, decrypts, and stores messages in `data/received_messages.jsonl`.
   - Receiver sends `ack <message_id>`, logs: `[2025-07-14 10:05:00] [RECEIVER] Sending ACK for message <message_id>`.
   - Server sends `ACK_CONFIRMED <message_id>`, receiver logs: `[2025-07-14 10:05:00] [RECEIVER] Server confirmed ACK`.

5. **Message Removal**:

   - Server removes message from `queues` after `ack <message_id>`.

   - Logs: `[2025-07-14 10:05:00] Message <message_id> acknowledged by client, removed from queues and status`.

     

## 5. Acknowledgment Mechanism

- **Sender-Server ACK**:

  - Server sends `ACK <message_id>` after queuing message.
  - Sender retries up to 3 times (10-second timeout) until ACK is received.
  - Sender removes message from `pending_messages` upon ACK.
  - Logged: `[2025-07-14 10:05:00] [SENDER] Server ACK received`.

- **Receiver-Server ACK**:

  - Receiver sends `ack <message_id>` after processing message.
  - Server sends `ACK_CONFIRMED <message_id>` and removes message from `queues` and `message_status`.
  - Receiver retries up to 3 times (10-second timeout).
  - Logged: `[2025-07-14 10:05:00] [RECEIVER] Server confirmed ACK`.

- **Independence**: Sender and receiver ACKs are decoupled for modularity.

- **Retry Logic**: Server does not retry delivery; clients handle retries.

  

## 6. Logging and Verification

- **Sender**:
  - Logs encryption, public key retrieval, batching, sending, ACK receipt, and removal from `pending_messages` with timestamps.
  - Example: `[2025-07-14 10:05:00] [SENDER] Encrypted message <message_id> for receiver_1`.
- **Receiver**:
  - Logs public key registration, message receipt, decryption, ACK sending, confirmation, and storage in `data/received_messages.jsonl` with timestamps.
  - Example: `[2025-07-14 10:05:00] [RECEIVER] Decrypted message <message_id>: <message>`.
- **Server**:
  - Logs via `tracing` for connections, key registration, message statuses, and errors with timestamps.
  - Example: `[2025-07-14 10:05:00] Published message <message_id> to queue 'my_key' via exchange 'my_exchange'`.
- **Verification**:
  - Check `pending_messages` (sender) for unacknowledged messages.
  - Check `data/received_messages.jsonl` (receiver) for processed messages.
  - Check server logs for `Acknowledged` status or errors (e.g., `[2025-07-14 10:05:00] TLS handshake failed`).