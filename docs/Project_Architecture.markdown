# CipherMQ Project Architecture: A new generation secure message broker

## 1. Introduction
The **CipherMQ** project is a secure message broker system designed to transmit encrypted messages between senders and receivers. It employs a push-based architecture, meaning messages are actively delivered to recipients and are stored only in the server's memory (without persistent storage except for logs). The project architecture consists of three main components:
- **Server** (`main.rs`): The core message broker that receives, routes, and delivers messages.
- **Sender** (`Sender.py`): Encrypts messages and sends them to the server.
- **Receiver** (`Receiver.py`): Receives, decrypts, and stores messages.

This document provides a detailed breakdown of the project's architecture, with a special focus on the server and interactions between components.

## 2. Architecture Overview
The CipherMQ architecture is based on a message broker model using **queues** and **exchanges**, employing the TCP protocol for client-server communication. Messages are secured using hybrid encryption (RSA + AES-GCM). The main architectural components include:
- **Message Broker Server**: Implemented in Rust, utilizing Tokio and DashMap libraries for asynchronous performance and multi-threaded data management.
- **Sender and Receiver Clients**: Implemented in Python, using the `pycryptodome` library for encryption.
- **Hybrid Encryption**: Combines RSA for session key encryption and AES-GCM for message encryption and authentication.
- **Communication Protocol**: A simple TCP-based text protocol for sending commands and messages.

## 3. Architectural Components
### 3.1. Server (`main.rs`)
The server is the heart of the CipherMQ project, responsible for receiving, routing, and delivering messages. This component is implemented in the `main.rs` file and leverages an asynchronous architecture to handle multiple clients.

#### 3.1.1. Data Structures
The server uses three key data structures to manage messages and communications:
- **ServerState**:
  - `queues: DashMap<String, Queue>`: A thread-safe map to store queues by their names. `DashMap` is chosen for its high performance in multi-threaded environments.
  - `exchanges: DashMap<String, Exchange>`: A map to store exchanges by their names.
  - `message_status: DashMap<String, MessageStatus>`: A map to track message statuses (`Sent`, `Delivered`, `Acknowledged`) using `message_id`.
- **Queue**:
  - `name: String`: A unique queue name.
  - `messages: VecDeque<(EncryptedInputData, MessageStatus)>`: A double-ended queue (Deque) to store encrypted messages and their statuses.
  - `consumers: Vec<mpsc::UnboundedSender>`: A list of communication channels (Tokio MPSC) to send messages to connected consumers.
- **Exchange**:
  - `name: String`: A unique exchange name.
  - `bindings: HashMap<String, String>`: A mapping of routing keys to queue names.
- **EncryptedInputData**:
  - `message_id: String`: A unique identifier for each message (generated using UUID).
  - `ciphertext: String`: The encrypted message content.
  - `nonce: String`: A one-time value for AES-GCM.
  - `tag: String`: An authentication tag to verify message integrity.
  - `enc_session_key: String`: The session key encrypted with RSA.

#### 3.1.2. Key Server Functions
The server uses the following functions to manage messages and communications:
- **declare_queue**: Creates a new queue with the specified name. If the queue already exists, it is ignored.
- **declare_exchange**: Creates a new exchange with the specified name.
- **bind_queue**: Binds a queue to an exchange using a routing key.
- **publish**: Publishes a message to an exchange, which is then routed to the associated queue. The message is sent to all connected consumers, and its status is set to `Sent`.
- **register_consumer**: Registers a consumer for a queue and sends existing messages to it.
- **consume**: Manually retrieves a message from a queue (for Pull mode) and sets its status to `Delivered`.
- **acknowledge**: Confirms message receipt by a consumer, removes it from the queue, and updates its status to `Acknowledged`.

#### 3.1.3. Connection Management
- **Communication Protocol**: The server uses a simple TCP-based text protocol. Clients send commands such as `publish <exchange> <routing_key> <message_json>` or `consume <queue>`.
- **Asynchronous Management**: The server uses Tokio's `TcpListener` to listen for connections on port 5672. Each client connection is processed in a separate Task.
- **Message Sending and Receiving**: `tokio::select!` is used to manage simultaneous receiving of client commands and sending messages to consumers.
- **Error Handling**: Network errors (e.g., disconnections) and invalid commands are handled with logging via `tracing` and error messages sent to the client.

#### 3.1.4. Architectural Features of the Server
- **Asynchronicity**: Utilizes Tokio for high-performance concurrent connection handling.
- **Memory Safety**: Leverages Rust and DashMap to prevent concurrency issues (e.g., Race Conditions).
- **Flexible Routing**: Supports exchanges and routing keys to direct messages to appropriate queues.
- **Message Status Management**: Tracks message statuses (`Sent`, `Delivered`, `Acknowledged`) to ensure proper delivery.

#### 3.1.6. Server Technical Details
- **Data Structures**:
  - **ServerState**: Manages queues, exchanges, and message statuses using thread-safe `DashMap`.
  - **Queue**: Handles message storage with `VecDeque` and consumer channels with `mpsc::UnboundedSender`.
  - **Exchange**: Manages bindings between routing keys and queues using `HashMap`.
  - **EncryptedInputData**: Encapsulates message metadata including `message_id`, `ciphertext`, `nonce`, `tag`, and `enc_session_key`.
- **Key Functions**:
  - **declare_queue**: Initializes a new queue if not already present.
  - **declare_exchange**: Sets up a new exchange.
  - **bind_queue**: Links a queue to an exchange with a routing key.
  - **publish**: Routes messages to queues and notifies consumers.
  - **register_consumer**: Adds a consumer and delivers pending messages.
  - **consume**: Pulls a message from a queue for manual retrieval.
  - **acknowledge**: Confirms delivery and cleans up acknowledged messages.
- **Connection Management**:
  - Uses a TCP-based text protocol on port 5672.
  - Employs Tokio's `TcpListener` and tasks for concurrent client handling.
  - Utilizes `tokio::select!` for simultaneous command processing and message dispatch.
  - Logs errors with `tracing` and responds with error messages to clients.

### 3.2. Sender (`Sender.py`)
The sender encrypts messages and sends them to the server.

#### 3.2.1. Key Components
- **encrypt_message_hybrid Function**:
  - Generates a random session key (16 bytes) using `get_random_bytes`.
  - Encrypts the session key with RSA using the receiver's public key (via `PKCS1_OAEP`).
  - Encrypts the message with AES-GCM using the session key, producing `ciphertext`, `nonce`, and `tag`.
  - Produces a JSON output containing `message_id`, `enc_session_key`, `nonce`, `tag`, and `ciphertext`.
- **send_message Function**:
  - Sends the encrypted message to the server via a TCP socket.
  - Receives the server's response (e.g., "Message published").
- **Error Handling**: Cryptographic or network errors are reported with error messages.

#### 3.2.2. Architectural Features
- **Hybrid Encryption**: Uses RSA for session key security and AES-GCM for fast encryption and authentication.
- **Simple Communication**: Uses a TCP socket to send commands to the server.
- **Unique Identifier Generation**: Uses UUID to create a unique `message_id`.

### 3.3. Receiver (`Receiver.py`)
The receiver retrieves messages from the server, decrypts them, and stores them in a file.

#### 3.3.1. Key Components
- **decrypt_message_hybrid Function**:
  - Decrypts the session key using the RSA private key (via `PKCS1_OAEP`).
  - Decrypts the message with AES-GCM using the session key, `nonce`, and `tag`.
  - Verifies message integrity using the `tag` to ensure it has not been tampered with.
- **receive_messages Function**:
  - Connects to the server via a TCP socket and subscribes to a queue (using the `consume` command).
  - Receives messages, decrypts them, and sends an acknowledgment (`ack`) to the server.
  - Adds decrypted messages to a thread-safe queue (`queue.Queue`).
- **process_messages Function**:
  - Runs a separate thread to process messages from the queue and store them in `received_messages.json`.
- **Signal Handling**: Uses `signal_handler` for safe shutdown upon receiving SIGINT (e.g., Ctrl+C).

#### 3.3.2. Architectural Features
- **Multi-Threaded Processing**: Uses a separate thread for storing messages to prevent blocking message reception.
- **Error Handling**: Thoroughly checks received messages (e.g., ensuring all JSON fields are present) and reports errors.
- **Message Storage**: Stores decrypted messages in a JSON file with timestamps.

### 3.4. Hybrid Encryption
Hybrid encryption is the security core of the project, combining RSA and AES-GCM:
- **RSA**:
  - Used to encrypt the session key.
  - Employs the `PKCS1_OAEP` algorithm for enhanced security.
  - Uses a 2048-bit key for a balance between security and performance.
- **AES-GCM**:
  - Used for message encryption and authentication.
  - Generates a `nonce` to prevent replay attacks.
  - Produces a `tag` to verify message integrity and authenticity.
- **Process**:
  1. The sender generates a random session key.
  2. The session key is encrypted with the receiver's public key.
  3. The message is encrypted with AES-GCM using the session key.
  4. The receiver decrypts the session key with its private key.
  5. The message is decrypted using the session key, `nonce`, and `tag`.

## 4. Component Interactions
1. **Key Generation**:
   - The `RSA.py` script or OpenSSL tool generates public and private keys.
2. **Server Execution**:
   - The server runs on `127.0.0.1:5672` and creates a default queue (`default_queue`) and exchange (`default_exchange`).
3. **Message Sending**:
   - The sender (`Sender.py`) encrypts a message and sends it to the server using the `publish` command.
   - The server routes the message to the associated queue and sends it to connected consumers.
4. **Message Receiving**:
   - The receiver (`Receiver.py`) subscribes to a queue using the `consume` command.
   - Messages are received, decrypted, and stored in a file.
   - The receiver sends an acknowledgment (`ack`) to the server.
5. **Status Management**:
   - The server updates message statuses from `Sent` to `Delivered` and then `Acknowledged`.
   - Acknowledged messages are removed from the queue.
