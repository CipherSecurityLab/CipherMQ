# CipherMQ Project Architecture: A Secure Message Broker with mTLS and Guaranteed Delivery

## 1. Introduction
**CipherMQ** is a high-performance, secure message broker designed to transmit encrypted messages between senders and receivers using a push-based architecture. It ensures **zero message loss** and **exactly-once delivery** through robust acknowledgment mechanisms, with messages temporarily held in memory (except for logs and receiver output). The system leverages **hybrid encryption** (RSA + AES-GCM) for message confidentiality and authenticity and **Mutual Transport Layer Security (mTLS)** for secure client-server communication with two-way authentication.

This version introduces **mTLS support**, enhancing transport-layer security with client and server certificate verification, as implemented in `auth.rs`, `connection.rs`, and `main.rs`, with configuration via `config.toml` and `config.json`. The project consists of:
- **Server** (`main.rs`): A Rust-based message broker for routing and delivering messages over mTLS.
- **Sender** (`Sender.py`): A Python script that encrypts and sends messages in batches with retry logic.
- **Receiver** (`Receiver.py`): A Python script that receives, decrypts, deduplicates, and stores messages.

This document provides a comprehensive overview of the architecture, covering server, clients, mTLS, encryption, and acknowledgment mechanisms.

## 2. Architecture Overview
CipherMQ operates as a message broker with **queues** and **exchanges**, supporting mTLS connections via a text-based protocol. Key features include:
- **Mutual TLS (mTLS)**: Secures client-server communication with two-way authentication using `tokio-rustls` (server) and Python’s `ssl` module (clients), configurable via `config.toml` and `config.json`.
- **Hybrid Encryption**: Combines RSA for session key encryption and AES-GCM for message encryption and authentication.
- **Zero Message Loss**: Sender and server retries with acknowledgments ensure reliable delivery.
- **Exactly-Once Delivery**: Receiver deduplicates messages using `message_id`.
- **Asynchronous Processing**: Built with Tokio for concurrent, high-performance connection handling.
- **Thread-Safe Data Structures**: Uses `DashMap` for multi-threaded operations.
- **Flexible Routing**: Supports exchanges, queues, and routing keys.
- **Clear Logging**: Detailed logs for ACKs (e.g., `✅ [SENDER] Server ACK received`, `✅ [RECEIVER] Server confirmed ACK`).

The architecture is illustrated in the [Sequence Diagram](diagrams/Sequence_diagram.png) and [Activity Diagram](diagrams/Activity_Diagram.png) in `docs/diagrams`.

## 3. Architectural Components

### 3.1. Server (`main.rs`, `server.rs`, `connection.rs`, `state.rs`, `config.rs`, `auth.rs`)
The server is the core of CipherMQ, managing message routing, delivery, and secure connections with mTLS.

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
  - `enc_session_key: String`: RSA-encrypted session key (base64-encoded).
  - `nonce: String`: AES-GCM nonce (base64-encoded).
  - `tag: String`: AES-GCM authentication tag (base64-encoded).
  - `ciphertext: String`: Encrypted message content (base64-encoded).
- **MessageStatus**:
  - `sent_time: Option<Instant>`: Time message was queued.
  - `delivered_time: Option<Instant>`: Time message was delivered.
  - `acknowledged_time: Option<Instant>`: Time message was acknowledged.

#### 3.1.2. Key Server Functions (`server.rs`, `state.rs`)
- **declare_queue**: Creates a new queue in `queues`.
- **declare_exchange**: Creates a new exchange in `exchanges`.
- **bind_queue**: Binds a queue to an exchange with a routing key, stored in `bindings`.
- **publish**: Queues messages, sends `ACK <message_id>` to sender, and delivers to consumers via `consumers`. Checks for duplicate `message_id`s.
- **publish_batch**: Processes a batch of messages, sending individual `ACK <message_id>` responses.
- **register_consumer**: Registers a consumer channel for push-based delivery.
- **consume**: Retrieves messages manually (pull mode).
- **acknowledge**: Marks message as `Acknowledged`, removes it from `queues`, and sends `ACK_CONFIRMED <message_id>` to receiver.
- **get_stats**: Returns JSON stats (queue sizes, consumer counts, message statuses).
- **reset_stats**: Clears `message_status` and `request_times`.

#### 3.1.3. Connection Management (`connection.rs`, `main.rs`, `auth.rs`)
- **Protocol**: Text-based protocol over mTLS (e.g., `publish <exchange> <routing_key> <message_json>`).
- **mTLS Support**:
  - Uses `tokio-rustls` with `WebPkiClientVerifier` for two-way authentication (`auth.rs`).
  - Loads server certificates (`server.crt`, `server.key`) and CA certificate (`ca.crt`) for client verification via `config.toml`.
  - Clients load `client.crt`, `client.key`, and `ca.crt` for server verification via `config.json`.
- **Asynchronous Handling**: Tokio’s `TcpListener` and `tokio::select!` manage concurrent connections.
- **Acknowledgment**:
  - Sends `ACK <message_id>` to sender after queuing.
  - Sends `ACK_CONFIRMED <message_id>` to receiver after acknowledgment.
- **Error Handling**: Logs errors via `tracing` (e.g., `TLS handshake failed`) and sends error messages to clients.

### 3.2. Sender (`Sender.py`)
The sender encrypts and sends messages in batches with retry logic.

#### 3.2.1. Key Components
- **encrypt_message_hybrid**:
  - Generates a 16-byte session key, encrypts it with RSA (`PKCS1_OAEP`, 2048-bit key from `receiver_public.pem`), and encrypts the message with AES-GCM.
  - Outputs JSON with `message_id` (UUID), `enc_session_key`, `nonce`, `tag`, and `ciphertext` (all base64-encoded).
- **send_message_batch**:
  - Sends a batch of messages, retries up to 3 times (5-second timeout) until `ACK <message_id>` is received for each message.
  - Stores messages in `pending_messages` until acknowledged.
  - Logs: `✅ [SENDER] Server ACK received for message <message_id>`.
- **collect_and_send_messages**:
  - Collects messages into batches (up to 10 messages or 1-second timeout).
  - Ensures all queued messages in `message_queue` are sent.
- **mTLS Support**:
  - Uses `ssl.SSLContext` with `PROTOCOL_TLS_CLIENT`, loads `ca.crt` for server verification, and `client.crt`, `client.key` for client authentication.
  - Disables hostname verification for self-signed certificates (`check_hostname=False`).
- **Configuration**:
  - Reads `config.json` for `exchange_name`, `routing_key`, `server_address`, `server_port`, `certificate_path`, `client_cert_path`, and `client_key_path`.

#### 3.2.2. Architectural Features
- **Reliability**: Retries and batching ensure no message loss.
- **Security**: mTLS secures transport; hybrid encryption protects messages.
- **Logging**: Logs encryption, sending, batching, and ACK receipt.
- **Deduplication**: Uses UUID to prevent server-side duplicates.

### 3.3. Receiver (`Receiver.py`)
The receiver retrieves, decrypts, and stores messages.

#### 3.3.1. Key Components
- **decrypt_message_hybrid**:
  - Decrypts session key with RSA private key (`PKCS1_OAEP`, `receiver_private.pem`) and message with AES-GCM.
  - Verifies integrity using `tag`.
- **receive_messages**:
  - Subscribes to a queue, receives messages, and deduplicates using `processed_messages` set.
  - Sends `ack <message_id>` with retries (3 attempts, 5-second timeout) until `ACK_CONFIRMED <message_id>` is received.
  - Logs: `✅ [RECEIVER] Server confirmed ACK for message <message_id>`.
- **process_messages**:
  - Stores decrypted messages in `received_messages.jsonl` with `message_id` and timestamp.
  - Batches writes (up to 10 messages) for efficiency.
- **signal_handler**: Ensures graceful shutdown on SIGINT.
- **mTLS Support**:
  - Uses `ssl.SSLContext` to load `ca.crt` for server verification and `client.crt`, `client.key` for client authentication.
  - Logs cipher (e.g., `Cipher: TLS_AES_256_GCM_SHA384`).
- **Configuration**:
  - Reads `config.json` for `queue_name`, `exchange_name`, `routing_key`, `server_address`, `server_port`, `certificate_path`, `client_cert_path`, and `client_key_path`.

#### 3.3.2. Architectural Features
- **Deduplication**: Prevents reprocessing using `processed_messages`.
- **Reliability**: Retries ACKs to ensure server cleanup.
- **Security**: mTLS and hybrid encryption ensure secure transport and message integrity.
- **Logging**: Logs receipt, decryption, ACKs, and file writes.

### 3.4. Hybrid Encryption
- **RSA**: Encrypts a 16-byte session key with `PKCS1_OAEP` (2048-bit key).
- **AES-GCM**: Encrypts message, generates `nonce` (12 bytes) and `tag` (16 bytes) for authentication.
- **Process**:
  1. Sender generates session key, encrypts it with receiver’s public key (`receiver_public.pem`).
  2. Message is encrypted with AES-GCM, producing `ciphertext`, `nonce`, and `tag`.
  3. Receiver decrypts session key with private key (`receiver_private.pem`) and message with AES-GCM, verifying `tag`.

## 4. Component Interactions
1. **Key and Certificate Generation**:
   - OpenSSL generates `ca.crt`, `server.crt`, `server.key`, `client.crt`, and `client.key` for mTLS.
   - OpenSSL generates `receiver_public.pem` and `receiver_private.pem` for hybrid encryption.
2. **Sender to Server**:
   - Sender authenticates with `client.crt` and `client.key`, verifies server with `ca.crt`.
   - Sender encrypts messages, sends batches to `ciphermq_exchange` with `ciphermq_key`.
   - Server queues messages, sends `ACK <message_id>` for each.
   - Sender logs: `✅ [SENDER] Server ACK received for message <message_id>`.
3. **Server to Receiver**:
   - Receiver authenticates with `client.crt` and `client.key`, verifies server with `ca.crt`.
   - Server routes messages to `ciphermq_queue` and pushes to consumers.
   - Receiver deduplicates, decrypts, and stores messages in `received_messages.jsonl`.
   - Receiver sends `ack <message_id>`, logs: `📤 [RECEIVER] Sending ACK for message <message_id>`.
   - Server sends `ACK_CONFIRMED <message_id>`, receiver logs: `✅ [RECEIVER] Server confirmed ACK`.
4. **Message Removal**:
   - Server removes message from `queues` after `ack <message_id>`.
   - Logs: `Message <message_id> acknowledged by client, removed from queues and status`.

## 5. Acknowledgment Mechanism
- **Sender-Server ACK**:
  - Server sends `ACK <message_id>` after queuing message.
  - Sender retries up to 3 times (5-second timeout) until ACK is received.
  - Sender removes message from `pending_messages` upon ACK.
  - Logged: `✅ [SENDER] Server ACK received`.
- **Receiver-Server ACK**:
  - Receiver sends `ack <message_id>` after processing message.
  - Server sends `ACK_CONFIRMED <message_id>` and removes message from `queues` and `message_status`.
  - Receiver retries up to 3 times (5-second timeout).
  - Logged: `✅ [RECEIVER] Server confirmed ACK`.
- **Independence**: Sender and receiver ACKs are decoupled for modularity.
- **Retry Logic**: Server does not retry delivery; clients handle retries.

## 6. Logging and Verification
- **Sender**:
  - Logs encryption, batching, sending, ACK receipt, and removal from `pending_messages`.
  - Example: `📤 [SENDER] Message <message_id> added to pending messages`.
- **Receiver**:
  - Logs message receipt, decryption, ACK sending, confirmation, and storage in `received_messages.jsonl`.
  - Example: `✅ [RECEIVER] Decrypted message <message_id>: <message>`.
- **Server**:
  - Logs via `tracing` for connections, message statuses, and errors.
  - Example: `Published message <message_id> to queue 'ciphermq_key' via exchange 'ciphermq_exchange'`.
- **Verification**:
  - Check `pending_messages` (sender) for unacknowledged messages.
  - Check `received_messages.jsonl` (receiver) for processed messages.
  - Check server logs for `Acknowledged` status or errors (e.g., `TLS handshake failed`).