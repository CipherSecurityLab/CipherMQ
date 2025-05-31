# CipherMQ Project Architecture: A Secure Message Broker with Guaranteed Delivery

## 1. Introduction
**CipherMQ** is a secure message broker system designed to transmit encrypted messages between senders and receivers with **zero message loss** and **exactly-once delivery**. It uses a push-based architecture, storing messages in memory (without persistent storage except for logs and receiver output). The system employs hybrid encryption (RSA + AES-GCM) and robust acknowledgment mechanisms to ensure reliability. The project consists of:
- **Server** (`main.rs`): The core message broker for routing and delivering messages.
- **Sender** (`Sender.py`): Encrypts and sends messages with retry logic.
- **Receiver** (`Receiver.py`): Receives, decrypts, deduplicates, and stores messages.

This document details the architecture, focusing on the server, client interactions, and acknowledgment mechanisms.

## 2. Architecture Overview
CipherMQ uses a message broker model with **queues** and **exchanges**, communicating via a TCP-based text protocol. Key features include:
- **Hybrid Encryption**: Combines RSA for session key encryption and AES-GCM for message encryption and authentication.
- **Message Broker Server**: Implemented in Rust, utilizing Tokio and DashMap libraries for asynchronous performance and multi-threaded data management.
- **Sender and Receiver Clients**: Implemented in Python, using the `pycryptodome` library for encryption.
- **Guaranteed Delivery**: Sender and receiver retries with ACKs ensure no message loss.
- **Exactly-Once Delivery**: Receiver deduplicates messages using `message_id`.
- **Clear Logging**: ACKs are logged clearly (e.g., `✅ [SENDER] Server ACK received`, `✅ [RECEIVER] Server confirmed ACK`).
- **Communication Protocol**: A simple TCP-based text protocol for sending commands and messages.

## 3. Architectural Components

### 3.1. Server (`main.rs`)
The server is the core of CipherMQ, handling message routing and delivery.

#### 3.1.1. Data Structures
- **ServerState**:
  - `queues: DashMap<String, Queue>`: Thread-safe map for queues.
  - `exchanges: DashMap<String, Exchange>`: Map for exchanges.
  - `message_status: DashMap<String, MessageStatus>`: Tracks message statuses (`Sent`, `Delivered`, `Acknowledged`).
- **Queue**:
  - `name: String`: Unique queue name.
  - `messages: VecDeque<(EncryptedInputData, MessageStatus)>`: Stores messages and statuses.
  - `consumers: Vec<mpsc::UnboundedSender>`: Channels for sending messages to consumers.
- **Exchange**:
  - `name: String`: Unique exchange name.
  - `bindings: HashMap<String, String>`: Maps routing keys to queue names.
- **EncryptedInputData**:
  - `message_id: String`: Unique identifier (UUID).
  - `ciphertext: String`: Encrypted message content.
  - `nonce: String`: AES-GCM nonce.
  - `tag: String`: Authentication tag.
  - `enc_session_key: String`: RSA-encrypted session key.

#### 3.1.2. Key Server Functions
- **declare_queue**: Creates a new queue.
- **declare_exchange**: Creates a new exchange.
- **bind_queue**: Binds a queue to an exchange with a routing key.
- **publish**: Stores messages in queues, sends `ACK <message_id>` to sender, and delivers to consumers. Checks for duplicate `message_id`s.
- **register_consumer**: Registers a consumer and sends pending messages.
- **consume**: Retrieves messages manually (Pull mode).
- **acknowledge**: Updates message status to `Acknowledged`, removes it from queues, and sends `ACK_CONFIRMED <message_id>` to receiver.
- **retry_unacknowledged**: Retries delivery of `Delivered` messages every 30 seconds.

#### 3.1.3. Connection Management
- **Protocol**: TCP-based text protocol (e.g., `publish <exchange> <routing_key> <message_json>`).
- **Asynchronous Handling**: Uses Tokio’s `TcpListener` and `tokio::select!` for concurrent connections.
- **Acknowledgment**:
  - Sends `ACK <message_id>` to sender after queuing.
  - Sends `ACK_CONFIRMED <message_id>` to receiver after acknowledgment.
- **Error Handling**: Logs errors via `tracing` and sends error messages to clients.

#### 3.1.4. Architectural Features
- **Reliability**: Retries unacknowledged messages and checks for duplicates.
- **Logging**: Logs message statuses (e.g., `Message <message_id> acknowledged`).
- **Concurrency**: Uses `DashMap` for thread-safe operations.

### 3.2. Sender (`Sender.py`)
The sender encrypts and sends messages with retry logic.

#### 3.2.1. Key Components
- **encrypt_message_hybrid**:
  - Generates a 16-byte session key, encrypts it with RSA (`PKCS1_OAEP`), and encrypts the message with AES-GCM.
  - Outputs JSON with `message_id`, `enc_session_key`, `nonce`, `tag`, and `ciphertext`.
- **send_message**:
  - Sends messages via TCP, retries up to 3 times (5-second timeout) until `ACK <message_id>` is received.
  - Stores messages in `pending_messages` until acknowledged.
  - Logs clearly (e.g., `✅ [SENDER] Server ACK received for message <message_id>`).
- **Error Handling**: Reports cryptographic or network errors.

#### 3.2.2. Architectural Features
- **Reliability**: Retries ensure no message loss.
- **Logging**: Detailed logs for sending and ACK receipt.
- **Deduplication**: Uses UUID to prevent duplicate processing on server.

### 3.3. Receiver (`Receiver.py`)
The receiver retrieves, decrypts, and stores messages.

#### 3.3.1. Key Components
- **decrypt_message_hybrid**:
  - Decrypts session key with RSA private key and message with AES-GCM.
  - Verifies integrity using `tag`.
- **receive_messages**:
  - Subscribes to a queue, receives messages, and deduplicates using `processed_messages` set.
  - Sends `ack <message_id>` with retries until `ACK_CONFIRMED <message_id>` is received.
  - Logs clearly (e.g., `✅ [RECEIVER] Server confirmed ACK for message <message_id>`).
- **process_messages**:
  - Stores decrypted messages in `received_messages.json` with timestamps.
- **signal_handler**: Ensures graceful shutdown on SIGINT.

#### 3.3.2. Architectural Features
- **Deduplication**: Prevents reprocessing using `processed_messages`.
- **Reliability**: Retries ACKs to ensure server cleanup.
- **Logging**: Detailed logs for message receipt, decryption, and ACKs.

### 3.4. Hybrid Encryption
- **RSA**: Encrypts session key with `PKCS1_OAEP` (2048-bit key).
- **AES-GCM**: Encrypts message, generates `nonce` and `tag` for integrity.
- **Process**:
  1. Sender generates and encrypts session key.
  2. Message is encrypted with AES-GCM.
  3. Receiver decrypts session key and message, verifying integrity.

## 4. Component Interactions
1. **Key Generation**:
   - `RSA.py` or OpenSSL generates public/private keys.
2. **Sender to Server**:
   - Sender encrypts and sends message with `message_id`.
   - Server queues message, sends `ACK <message_id>`.
   - Sender logs: `✅ [SENDER] Server ACK received for message <message_id>`.
3. **Server to Receiver**:
   - Server routes message to queue and sends to consumers.
   - Receiver deduplicates, decrypts, and stores message in `received_messages.json`.
   - Receiver sends `ack <message_id>`, logs: `📤 [RECEIVER] Sending ACK for message <message_id>`.
   - Server sends `ACK_CONFIRMED <message_id>`, receiver logs: `✅ [RECEIVER] Server confirmed ACK`.
4. **Message Removal**:
   - Server removes message from queue after `ack <message_id>`.
   - Logs: `Message <message_id> acknowledged`.

## 5. Acknowledgment Mechanism
- **Sender-Server ACK**:
  - Server sends `ACK <message_id>` after queuing.
  - Sender retries up to 3 times (5-second timeout) until ACK is received.
  - Logged in sender: `✅ [SENDER] Server ACK received`.
- **Receiver-Server ACK**:
  - Receiver sends `ack <message_id>` after processing.
  - Server sends `ACK_CONFIRMED <message_id>` and removes message.
  - Receiver retries up to 3 times (5-second timeout).
  - Logged in receiver: `✅ [RECEIVER] Server confirmed ACK`.
- **Independence**: Sender and receiver ACKs are separate, ensuring modularity.
- **Retry Logic**: Server retries unacknowledged `Delivered` messages every 30 seconds.

## 6. Logging and Verification
- **Sender**: Logs sending, ACK receipt, and removal from `pending_messages`.
- **Receiver**: Logs message receipt, decryption, ACK sending, and confirmation.
- **Server**: Logs message statuses via `tracing`.
- **Verification**:
  - Check `pending_messages` (sender) to ensure ACK receipt.
  - Check `received_messages.json` (receiver) for processed messages.
  - Check server logs for `Acknowledged` status.