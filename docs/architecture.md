# Architecture Overview

## Introduction
The Secure Message Broker is a distributed messaging system designed for secure, real-time communication. It consists of a Rust-based server handling HTTP and WebSocket communication, a Python-based WebSocket client for receiving messages, and a benchmarking script for performance evaluation. The system uses AES-GCM encryption to ensure message confidentiality and integrity, making it suitable for applications requiring robust security.

## System Components
The architecture comprises three main components, as illustrated in the [Component Diagram](./diagrams/Component_diagram.puml):

1. **HTTP Server (Rust)**:
   - Built with the `axum` framework, running on port 3000.
   - Exposes two endpoints: `/input` for single messages and `/input/batch` for batch messages.
   - Encrypts incoming messages using AES-GCM and stores them in a `DashMap` for thread-safe access.
   - Forwards encrypted messages to the WebSocket sender via an `mpsc` channel.

2. **WebSocket Sender (Rust)**:
   - Built with `tokio-tungstenite`, running on port 8080.
   - Receives encrypted messages from the HTTP server, decrypts them, and batches them (10 messages per batch) for efficient transmission to clients.
   - Handles WebSocket connections, ensuring reliable message delivery.

3. **WebSocket Client (Python)**:
   - Built with the `websocket` library, connects to the WebSocket sender.
   - Receives batched messages, parses them, and saves them to a JSON file (`received_messages.json`).
   - Uses a `queue.Queue` for thread-safe message processing.

## Data Flow
The [Sequence Diagram](diagrams/Sequence_diagram.puml) illustrates the data flow:
1. A client sends a message (or batch) to the HTTP server via POST requests.
2. The server encrypts the message(s) and stores them in the `DashMap`.
3. The encrypted messages are sent to the WebSocket sender via the `mpsc` channel.
4. The sender decrypts the messages, batches them, and transmits them to connected WebSocket clients.
5. The Python client receives the messages, processes them, and saves them to a file.

## Design Choices
- **Rust for Server**: Chosen for its performance, memory safety, and concurrency support via `tokio`.
- **Python for Client**: Provides accessibility and ease of use for rapid prototyping.
- **AES-GCM Encryption**: Ensures confidentiality and integrity with a widely trusted algorithm.
- **Batching**: Improves throughput by reducing network overhead (10 messages per batch).
- **DashMap**: Enables fast, thread-safe data storage for concurrent access.

## Scalability and Extensibility
The system is designed to handle concurrent requests efficiently, with `aiohttp` for asynchronous HTTP in the benchmarking script and `tokio` for async WebSocket handling. Future enhancements could include:
- Load balancing for the HTTP server.
- Persistent storage (e.g., database) instead of `DashMap`.
- Authentication for WebSocket clients.

For detailed interactions, refer to the [Sequence Diagram](diagrams/Sequence_diagram.puml).
