# CipherMQ Messaging System Benchmark Report

## Introduction
CipherMQ is a secure, high-performance message broker built in Rust, designed for reliable, encrypted message transmission using a push-based architecture. It guarantees **zero message loss** and **exactly-once delivery** through robust acknowledgment mechanisms and hybrid encryption (RSA-2048 + AES-256-GCM). This benchmark evaluates CipherMQ’s performance on a single system, focusing on throughput, error rate, and resource consumption (CPU and RAM) across 1-to-1 and 3-to-1 sender-receiver tests.

## Test Setup
### Hardware Specifications
| Specification | Value |
|---------------|-------|
| **Processor (CPU)** | 12th Gen Intel(R) Core(TM) i7-12700H, 2700 MHz, 14 cores, 20 logical processors |
| **Memory (RAM)** | 32.0 GB |
| **Virtual Memory** | 36.4 GB |

### Test Parameters
| Parameter | Value |
|-----------|-------|
| **Messages File** | messages1.jsonl, messages2.jsonl, messages3.jsonl (300,000 encrypted messages total) |
| **Encryption Scheme** | Hybrid (RSA-2048 for session key, AES-256-GCM for message encryption) |
| **Server Port** | 5672 |
| **Exchange** | ciphermq_exchange |
| **Routing Key** | ciphermq_key |
| **Batch Size** | 100 |
| **Message Generation** | Pregenerated JSONL files with unique message IDs, encrypted session keys, nonces, tags, and ciphertexts (~537 bytes) |

### Methodology
The CipherMQ server (`main.rs`) was run on `127.0.0.1:5672`, initializing `ciphermq_queue` and `ciphermq_exchange` with routing key `ciphermq_key`. A receiver (`Receiver.py`) subscribed to the queue, decrypting messages and storing them in `received_messages.jsonl`. For 3-to-1 tests, three senders (`Sender.py`) sent pregenerated messages from three JSONL files (100,000 messages each), totaling 300,000 messages. Tests scaled from 100 to 100,000 messages per sender. For details, see the [CipherMQ README](README.md) and [Project Architecture](docs/Project_Architecture.md).

## Benchmark Results
CipherMQ was tested in 1-to-1 and 3-to-1 sender-receiver configurations, measuring throughput, error rate, and resource usage. Key findings:
- **Throughput**: Stable at ~3378–3379 messages/second in 1-to-1 tests; increases to 4818.93 messages/second in 3-to-1 tests (30,000 messages).
- **Error Rate**: 0% across all tests, confirming exactly-once delivery.
- **Resource Usage**: CPU usage rises with message volume (20.5% to 38.5%), and RAM usage scales from 380.2 MB to 880.1 MB.

| Sender - Receiver | Test # | Messages | Batch Size | Successful Messages | Execution Time (s) | Throughput (msg/s) | Error Rate (%) | Avg CPU Usage (%) | Avg RAM Usage (MB) |
|-------------------|--------|----------|------------|---------------------|--------------------|----------------|---------------|-------------------|-------------------|
| 1 - 1 | Test 1 | 100      | 100        | 100/100             | 0.03               | 3379.50  | 0             | 20.5              | 380.2             |
| 1 - 1 | Test 2 | 1,000    | 100        | 1,000/1,000         | 0.31            | 3378.91  | 0             | 23.8              | 465.7             |
| 1 - 1 | Test 3 | 10,000   | 100        | 10,000/10,000       | 2.96            | 3378.88  | 0             | 27.4              | 580.9             |
| 1 - 1 | Test 4 | 100,000  | 100        | 100,000/100,000     | 29.60          | 3378.29  | 0             | 30.2              | 850.3             |
| 3 - 1 | Test 1 | 3 * 100  | 100        | 300/300             | 0.08            | 3539.05  | 0             | 32.7              | 650.1             |
| 3 - 1 | Test 2 | 3 * 1,000| 100        | 3,000/3,000         | 0.69            | 4343.83  | 0             | 36.4              | 780.4             |
| 3 - 1 | Test 3 | 3 * 10,000 | 100 | 30,000/30,000 | 6.23 | 4818.93 | 0 | 38.5 | 880.1 |

## Analysis
CipherMQ demonstrates robust performance:
- **Throughput**: The 1-to-1 tests show consistent throughput (~3378 messages/second), suggesting optimized single-sender processing. In 3-to-1 tests, throughput rises to 4818.93 messages/second (30,000 messages), indicating strong scalability with multiple senders.
- **Error Rate**: A 0% error rate across all tests validates CipherMQ’s acknowledgment mechanism for exactly-once delivery.
- **Resource Usage**: CPU usage increases with message volume, from 20.5% (100 messages, 1-to-1) to 38.5% (30,000 messages, 3-to-1), likely due to encryption overhead. RAM usage scales from 380.2 MB to 880.1 MB, reflecting moderate memory demands suitable for high-performance systems.

## Future Work
Future benchmarks will:
- Test larger datasets (e.g., 1,000,000 messages per sender).

- Test reading messages from various db (e.g., Redis)  instead of pregenerated JSONL files.

- Test under varied network conditions (e.g., high latency).

  

