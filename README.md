# CipherMQ: A New Generation Secure Message Broker

<p align="center">
<img src="./docs/CipherMQ.jpg" width="350" height="350">
</p>
![GitHub License](https://img.shields.io/badge/license-MIT-blue.svg)
![Rust](https://img.shields.io/badge/Rust-1.56%2B-orange.svg)
![Python](https://img.shields.io/badge/Python-3.8%2B-blue.svg)

**CipherMQ** is a secure message broker system designed to transmit encrypted messages between senders and receivers using a push-based architecture. It leverages hybrid encryption (RSA + AES-GCM) to ensure message confidentiality and authenticity. The system guarantees **zero message loss** and **exactly-once delivery** through robust acknowledgment mechanisms. Messages are temporarily held in memory (without persistent storage except for logs and receiver output) and routed through exchanges and queues to connected consumers.

The project consists of three main components:
- **Server** (`main.rs`): A Rust-based message broker for receiving, routing, and delivering messages.
- **Sender** (`Sender.py`): A Python script that encrypts and sends messages to the server in batches, with retry logic for guaranteed delivery of all queued messages.
- **Receiver** (`Receiver.py`): A Python script that receives, decrypts, deduplicates, and stores messages with acknowledgment retries.

Initial architecture of CipherMQ is as follows:

<p align="center">
<img src="./docs/diagrams/Diagram.png">
</p>

## Table of Contents
1. [Features](#features)
2. [Prerequisites](#prerequisites)
3. [Installation](#installation)
4. [Usage](#usage)
5. [Architecture](#architecture)
6. [Diagrams](#diagrams)
7. [Future Improvements](#future-improvements)
8. [Contributing](#contributing)
9. [License](#license)

## Features
- **Hybrid Encryption**: Combines RSA for session key encryption and AES-GCM for message encryption and authentication.
- **Zero Message Loss**: Sender retries until server acknowledgment (`ACK <message_id>`), and server retries delivery until receiver acknowledgment (`ack <message_id>`).
- **Exactly-Once Delivery**: Receiver deduplicates messages using `message_id` to prevent reprocessing.
- **Reliable Batch Processing**: Sender collects and sends all queued messages in batches, ensuring no messages are missed.
- **Clear Acknowledgment Logging**: Both sender and receiver log ACKs for visibility (e.g., `✅ [SENDER] Server ACK received` and `✅ [RECEIVER] Server confirmed ACK`).
- **Push-Based Messaging**: Messages are actively delivered to connected consumers.
- **Flexible Routing**: Supports exchanges and queues with routing keys for message delivery.
- **Asynchronous Processing**: Uses Tokio for high-performance, concurrent connection handling.
- **Thread-Safe Data Structures**: Leverages `DashMap` for safe multi-threaded operations.

## Prerequisites
To run CipherMQ, you need:
- [Rust](https://www.rust-lang.org/): Version 1.56 or higher (for the server).
- [Python](https://www.python.org/): Version 3.8 or higher (for Sender and Receiver).
- [Key Generation](https://slproweb.com/products/Win32OpenSSL.html): Use OpenSSL or the provided `RSA.py` script to generate keys.

## Installation
### 1. Clone the Repository
```bash
git clone https://github.com/fozouni/CipherMQ.git
cd CipherMQ
```
### 2. Set up the Rust Server
```bash
cd src
cargo build --release
```
### 3. Generate RSA Keys
Run the provided `RSA.py` script to generate public and private keys:
```bash
cd src/client
pip install pycryptodome
python RSA.py
```
This creates:
- `receiver_private.pem`: The receiver's private key.
- `receiver_public.pem`: The receiver's public key.

**Alternatively**, use OpenSSL:
```bash
openssl genrsa -out receiver_private.pem 2048
openssl rsa -in receiver_private.pem -pubout -out receiver_public.pem
```

> **Note**: Store the private key securely to prevent unauthorized access.

## Usage
### 1. Run the Server
```bash
cd src
cargo run --release
```
The server runs on `127.0.0.1:5672` and initializes a default queue (`default_queue`) and exchange (`default_exchange`) with the routing key `default_key`.

### 2. Run the Receiver
Start the receiver to listen for messages:
```bash
cd src/client
python Receiver.py
```
The receiver connects to the server, subscribes to `default_queue`, decrypts messages, and stores them in `received_messages.jsonl`.

### 3. Run the Sender
Send a batch of sample messages:
```bash
cd src/client
python Sender.py
```
The sender encrypts messages (e.g., "This is a test hybrid message."), collects all queued messages, and sends them in batches to the server via `default_exchange` and `default_key`. It ensures all messages are sent, even if they exceed the initial batch size.


## Architecture
CipherMQ architecture is based on a message broker model with the following components:
- **Server** (`main.rs`): Manages message routing and delivery using exchanges and queues.
- **Sender** (`Sender.py`): Encrypts messages using hybrid encryption and sends them in batches, ensuring all queued messages are delivered.
- **Receiver** (`Receiver.py`): Receives, decrypts, and stores messages in a JSONL file.
- **Hybrid Encryption**: Combines RSA for session key encryption and AES-GCM for message encryption and authentication.

For a detailed breakdown of the architecture, including components, interactions, and server details, refer to the [CipherMQ Project Architecture](docs/Project_Architecture.md) document in the `docs` directory.

## Diagrams
The following diagrams illustrate the architecture and operational flow of CipherMQ. They are located in the `docs/diagrams` directory:
- **[Sequence Diagram](docs/diagrams/Sequence_diagram.png)**: Shows the end-to-end flow of a message through the system.
- **[Activity Diagram](docs/diagrams/Activity_Diagram.png)**: Illustrates the operational flow and processes of CipherMQ.

To view the diagrams, open the PNG files in the `docs/diagrams` directory.

## Future Improvements
- Add TLS support for secure client-server communication.
- Implement client authentication (e.g., JWT or OAuth).
- Enable persistent message storage.
- Support standard protocols like AMQP or MQTT.
- Scale with distributed servers.

## Contributing

We welcome contributions! Before submitting a pull request, please:

1. Read our [Contributor License Agreement (CLA)](CLA.md)
2. Check the checkbox in the PR template to confirm your agreement
3. Follow our coding standards and include tests

For major changes, please open an issue first to discuss your proposed changes.
## License
This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.