# CipherMQ: A new generation message broker

<p align="center">
<img src="https://github.com/fozouni/CipherMQ/blob/main/docs/CipherMQ.jpg" width="350" height="350">
</p>

The CipherMQ is a high-performance, real-time messaging system designed to facilitate secure and efficient communication between distributed applications. Built with a Rust-based server and Python-based clients, it leverages AES-GCM encryption to ensure the confidentiality and integrity of messages, making it ideal for applications requiring robust security, such as IoT, financial systems, or real-time collaboration tools. The system uses WebSocket communication for low-latency, bidirectional data transfer and supports both single and batch message processing to accommodate diverse workloads.

This project combines the memory safety and performance of Rust with the accessibility and flexibility of Python, enabling developers to integrate secure messaging into their applications with ease. Its modular architecture, comprehensive benchmarking tools, and detailed documentation make it suitable for both production environments and research purposes. Whether you’re building a scalable microservices architecture or experimenting with secure communication protocols, the Secure Message Broker provides a lightweight yet powerful solution.

## Features

- **AES-GCM Encryption**: Ensures end-to-end message confidentiality and integrity using the industry-standard AES-256-GCM algorithm, protecting data against unauthorized access and tampering.
- **Real-Time WebSocket Communication**: Enables low-latency, bidirectional messaging through WebSocket, ideal for real-time applications like chat systems or live data feeds.
- **Efficient Batch Processing**: Supports sending multiple messages in a single request, reducing overhead and improving throughput for high-volume workloads.
- **Robust Concurrency Handling**: Utilizes Rust’s `DashMap` for thread-safe data storage and Python’s `aiohttp` for asynchronous HTTP requests, ensuring scalability under concurrent loads.
- **Comprehensive Benchmarking**: Includes a Python script (`Sim_send_100000_records.py`) to measure performance metrics like latency, throughput, and success rate, helping developers optimize system performance.
- **Cross-Language Integration**: Combines Rust’s performance and safety for the server with Python’s simplicity for clients, making the system accessible to a wide range of developers.
- **Extensive Logging and Monitoring**: Provides detailed logging through Rust’s `tracing` and Python’s `logging` modules, facilitating debugging and performance analysis.

Initial architecture of CipherMQ is as follows:

<p align="center">
<img src="https://github.com/fozouni/CipherMQ/blob/main/docs/diagrams/Component_diagram.png">
</p>


## Prerequisites

- [Rust](https://www.rust-lang.org/) (latest stable version)
- [Python 3.8+](https://www.python.org/)
- [PlantUML](https://plantuml.com/) (optional, for rendering diagrams)

## Installation

1. **Clone the repository**:
   ```bash
   git clone https://github.com/fozouni/CipherMQ.git
   cd CipherMQ
   ```

2. **Set up the Rust server**:
   
   ```bash
   cd src
   cargo build --release
   ```

## Usage

1. **Run the Rust server**:
   ```bash
   cd src
   cargo run --release
   ```

2. **Run the WebSocket client**:
   ```bash
   cd src/client
   python WebSocket.py
   ```

3. **Run the benchmarking script**:
   ```bash
   cd src/client
   python Sim_send_100000_records.py
   ```

## Documentation
The `docs` directory contains detailed project documentation:
- **[Architecture Overview](./docs/architecture.md)**: Describes the system’s components, data flow, and design choices.
- **Diagrams**: The `docs/diagrams/` folder contains PlantUML files (`Component_diagram.puml`, `Sequence_diagram.puml`). To render them:
  - Use an online tool like [PlantUML Server](http://www.plantuml.com/plantuml).
  - Or install PlantUML locally and run:
    ```bash
    plantuml docs/diagrams/*.puml
    ```

## Benchmark Results
The `results` directory contains performance data from the benchmarking script (`src/client/Sim_send_100000_records.py`):
- **`performance.markdown`**: Documents performance metrics (e.g., latency, throughput, success rate) across different machines.
- **`benchmark_results.csv`**: Raw data from benchmarking runs, suitable for further analysis.
- **`received_messages.json`**: Messages received by the WebSocket client, saved for verification.

## Contributing
Guidelines on how to contribute to this project will be published soon.

## License
This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
