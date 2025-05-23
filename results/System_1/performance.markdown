# Performance Analysis

## Introduction
This document evaluates the performance of CipherMQ, a system designed for secure, real-time messaging with AES-GCM encryption. The analysis is based on a benchmark conducted using `src/client/Sim_send_100000_records.py`, modified to send 100,000 messages to the server. Key metrics include latency, throughput, and success rate, demonstrating the system’s efficiency for high-volume workloads. The results highlight its suitability for applications requiring low-latency, reliable communication, such as IoT or financial systems.

## Methodology
The benchmarking script sends 100,000 messages to the HTTP server (`http://127.0.0.1:3000/input/batch`) in batches of 20,000 messages, using `aiohttp` for asynchronous requests. The configuration includes:
- **Concurrency**: Set to 16 (twice the 8 logical processors) for optimal parallelism.
- **Batch Size**: 20,000 messages per request to maximize throughput.
- **Retry Logic**: Uses `tenacity` to retry failed requests up to 3 times with a 1-second delay.

The benchmark was run on the following hardware:
- **Processor**: 2x Intel(R) Xeon(R) Gold 5118 CPU @ 2.30GHz, 4 cores each (8 cores total), 8 logical processors.
- **RAM**: 16.0 GB
- **Virtual Memory**: 29.0 GB

Results are saved to `results/benchmark_results.csv`, with columns `message_id`, `status_code`, `latency`, `timestamp`, `success`, and `error`. A summary is printed to the console and saved in `results/Result.txt`. Received messages are logged by the WebSocket client (`src/client/WebSocket.py`) to `received_messages.json`.


## Results
The benchmark achieved exceptional performance, processing 100,000 messages in 1.78 seconds with a 100% success rate. Below is a summary of the results (from `Result.txt`):

| Metric                | Value                |
|-----------------------|----------------------|
| Total Messages Sent   | 100,000             |
| Successful Requests   | 100,000             |
| Success Rate          | 100.00%             |
| Total Time            | 1.78 seconds        |
| Average Latency       | 0.0000243 seconds   |
| Throughput            | 56,258.20 messages/sec |
| Batches Sent          | 5                   |

## Analysis
The results demonstrate CipherMQ's outstanding performance:
- **100% Success Rate**: All 100,000 messages were processed without errors, indicating robust error handling and retry logic (`tenacity`).
- **Low Latency (0.0000243 seconds)**: The average latency per message is extremely low, driven by efficient batch processing (20,000 messages per batch) and `tokio`’s async handling in the Rust server.
- **High Throughput (56,258.20 messages/sec)**: The system processes over 56,000 messages per second, making it suitable for high-volume, real-time applications.

The use of large batches (20,000 messages) significantly reduces network overhead, contributing to the high throughput. The hardware’s 8 cores and 16 GB RAM support the concurrency level of 16, though the low CPU utilization (due to I/O-bound tasks) suggests room for further scaling.

## Conclusion
CipherMQ achieves exceptional performance, processing 100,000 messages in 1.78 seconds with a 100% success rate, average latency of 0.0000243 seconds, and throughput of 56,258.20 messages/second. These results make it ideal for high-throughput, low-latency applications like real-time analytics or secure IoT communication. The large batch size and efficient concurrency model ensure scalability, with opportunities for further optimization as outlined.
