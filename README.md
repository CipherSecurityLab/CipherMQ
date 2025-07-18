# CipherMQ: A New Generation Secure Message Broker



<p align="center">
<img src="./docs/CipherMQ.jpg" width="350" height="350">
</p>


![GitHub License](https://img.shields.io/badge/license-MIT-blue.svg)  ![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange.svg)  ![Python](https://img.shields.io/badge/Python-3.8%2B-blue.svg)

**CipherMQ** is a secure, high-performance message broker designed for encrypted message transmission between senders and receivers using a push-based architecture. It leverages **hybrid encryption** (x25519 + AES-GCM-256) for message confidentiality and authenticity, combined with **Mutual TLS (mTLS)** for secure client-server communication. The system ensures **zero message loss** and **exactly-once delivery** through robust acknowledgment mechanisms, with messages temporarily held in memory and routed via exchanges and queues. Public keys are securely stored in an SQLite database with AES-GCM encryption, and receivers register their public keys with the server for secure distribution to senders.



Initial architecture of CipherMQ is as follows:



<p align="center">
<img src="./docs/diagrams/Diagram.png">
</p>





## Table of Contents
1. [Features](#features)
2. [Prerequisites](#prerequisites)
3. [Installation](#installation)
4. [Configuration](#configuration)
5. [Usage](#usage)
6. [Architecture](#architecture)
7. [Diagrams](#diagrams)
8. [Future Improvements](#future-improvements)
9. [Contributing](#contributing)
10. [License](#license)



## Features

- **Mutual TLS (mTLS)**: Ensures secure client-server communication with two-way authentication using ECDSA P-384 certificates.
- **Hybrid Encryption**: Utilizes x25519 for session key encryption and AES-GCM-256 for message encryption and authentication.
- **Public Key Registration**: Receivers register their public keys with the server using the `register_key` command, which are securely stored and retrievable by senders via the `get_key` command.
- **Zero Message Loss**: Sender retries until server acknowledgment (`ACK <message_id>`), and server retries delivery until receiver acknowledgment (`ack <message_id>`).
- **Exactly-Once Delivery**: Receiver deduplicates messages using `message_id` to prevent reprocessing.
- **Batch Processing**: Sender collects and sends messages in batches, ensuring all queued messages are delivered.
- **Asynchronous Processing**: Built with Tokio for concurrent, high-performance connection handling.
- **Push-Based Messaging**: Messages are delivered to connected consumers.
- **Thread-Safe Data Structures**: Uses `DashMap` for safe multi-threaded operations.
- **Flexible Routing**: Supports exchanges and queues with routing keys for efficient message delivery.
- **Secure Key Storage**: Public keys are encrypted with AES-GCM and stored in an SQLite database.



## Prerequisites

To run CipherMQ with TLS, you need:
- [Rust](https://www.rust-lang.org/): Version 1.56 or higher (for the server).
- [Python](https://www.python.org/): Version 3.8 or higher (for Sender and Receiver).
- Certificates Generation: Use the provided `generate_certs.py` script to generate mTLS certificates.
- Key Generation: Use the provided `key_maker.py` script to generate x25519 key pairs.



## Installation

### 1. Clone the Repository
```bash
git clone https://github.com/fozouni/CipherMQ.git
cd CipherMQ
```

### 2. Set up the Rust Server
```bash
cargo build --release
```
### 3. Generate mTLS Certificates
Run the provided `generate_certs.py` script to generate CA certificates, server certificate, and client certificate for mTLS:

```bash
cd root
pip install cryptography
generate_certs.py

```

This creates:
- `ca.crt`: Certificate Authority (CA) certificate for verifying server and client certificates.
- `server.crt`: Server certificate for mTLS.
- `server.key`: Server private key for mTLS.
- `client.crt`: Client certificate for mTLS.
- `client.key`: Client private key for mTLS.

> **Note**: Store `ca.key` securely and do not distribute it. It is only used for certificate generation.

### 4. Generate x25519 Keys

Run the provided `key_maker.py` script to generate x25519 key pairs for hybrid encryption for the receiver:
```bash
cd src/client
python key_maker.py
```

This creates:
- `receiver/certs/receiver_private.key`: Receiver's private key for decryption.
- `receiver/certs/receiver_public.key`: Public key for sender encryption.



## Configuration

### Server Configuration
Create a `config.toml` file in the `CipherMQ` root directory:
```toml
[server]
connection_type = "tls"
address = "127.0.0.1:5672"

[tls]
cert_path = "certs/server.crt"
key_path = "certs/server.key"
ca_cert_path = "certs/ca.crt"

[database]
dbname = "public_keys.db"

[encryption]
aes_key = "YOUR_BASE64_ENCODED_32_BYTE_AES_KEY"
```

> **Note**: Replace `YOUR_BASE64_ENCODED_32_BYTE_AES_KEY` with a 32-byte key encoded in base64. Generate it using:
```bash
openssl rand -base64 32
```

### Client Configuration
 `config.json` file in both `sender/` and `receiver/` directories:`

> **Note**: Ensure `exchange_name`, `queue_name`, and `routing_key` match across Sender, Receiver, and server for proper message routing.

> **Security Note**: Restrict access to `server.key`, `client.key`, and `receiver_private.key (e.g., `chmod 600`).



## Usage

### 1. Run the Server
Start the server with TLS support:
```bash
cd root
cargo run --release
```

### 2. Run the Receiver
Start the receiver to subscribe to messages:
```bash
cd src/client/receiver
python Receiver.py
```
The receiver connects to the server via TLS, registers its public key using the `register_key` command, declares and binds to `my_queue`, decrypts messages, and saves them to `data/received_messages.jsonl`.

### 3. Run the Sender

```bash
cd src/client/sender
python Sender.py
```
The sender fetches the receiver's public key using the `get_key` command, encrypts messages using hybrid encryption, sends them in batches via `my_exchange` and `my_key`, and retries until acknowledgment.


## Architecture

CipherMQ is a message broker system with the following components:
- **Server** (`main.rs`, `server.rs`, `connection.rs`, `state.rs`, `config.rs`, `auth.rs`, `storage.rs`): A Rust-based broker that handles mTLS connections, message routing, and delivery using exchanges and queues. Public keys are encrypted with AES-GCM and stored in an SQLite database. The server supports the `register_key` command to store receiver public keys and the `get_key` command to provide them to senders.
- **Sender** (`sender.py`): Fetches receiver public keys using `get_key`, encrypts messages with hybrid encryption (x25519 + AES-GCM-256), sends them in batches, and ensures delivery with retries.
- **Receiver** (`receiver.py`): Registers its public key with the server using `register_key`, receives, decrypts, deduplicates, and stores messages in JSONL format, with acknowledgment retries.
- **mTLS Integration** (`auth.rs`, `connection.rs`): Supports secure two-way authentication using `tokio-rustls` and `WebPkiClientVerifier`.
- **Hybrid Encryption**: Combines x25519 for session key encryption and AES-GCM-256 for message encryption and authentication.
- **Key Storage** (`storage.rs`): Public keys are encrypted with AES-GCM and stored in an SQLite database, accessible via `register_key` and `get_key` commands.

For a detailed architecture overview, see [CipherMQ Project Architecture](docs/Project_Architecture.md).



## Diagrams
The following diagrams, located in `docs/diagrams`, illustrate CipherMQ's architecture and mTLS flow:
- **[Sequence Diagram](docs/diagrams/Sequence_diagram.png)**: Shows the end-to-end message flow, including mTLS handshakes, public key registration, and hybrid encryption.
- **[Activity Diagram](docs/diagrams/Activity_Diagram.png)**: Details the operational flow, including mTLS connection setup, key registration, and message processing.

## Future Improvements
- Support standard protocols like AMQP or MQTT for broader compatibility.
- Enable distributed server scaling for high availability.
- Replacing Python Script with specialized Rust libraries for generating keys and certificates.
- Implement certificate rotation and CRL/OCSP for enhanced security.
- Add support for Hardware Security Modules (HSM) for key management.
- Add persistent message storage to handle server restarts.
- Enhance monitoring with structured logging (optional reintroduction).



## Contributing
Contributions are welcome! Please:
1. Review the [Contributor License Agreement (CLA)](CLA.md).
2. Check the PR template checkbox to confirm agreement.
3. Follow coding standards and include tests.

For major changes, open an issue to discuss your proposal.

## License
This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
