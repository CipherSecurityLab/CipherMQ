// Importing required libraries for concurrent data structures, serialization, and asynchronous I/O
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tokio::time::{timeout, Duration};
use tracing::{error, info, warn, instrument};

// Structure to hold encrypted message data with unique identifiers and cryptographic fields
#[derive(Clone, Serialize, Deserialize)]
pub struct EncryptedInputData {
    pub message_id: String, // Unique identifier for the message
    pub enc_session_key: String, // Encrypted session key for secure communication
    pub nonce: String, // Nonce for cryptographic operations
    pub tag: String, // Authentication tag for message integrity
    pub ciphertext: String, // Encrypted message content
}

// Structure to track the status of a message through its lifecycle
#[derive(Clone)]
pub struct MessageStatus {
    sent_time: Option<Instant>, // Time when the message was sent
    delivered_time: Option<Instant>, // Time when the message was delivered
    acknowledged_time: Option<Instant>, // Time when the message was acknowledged
}

// Implementation of methods for updating message status
impl MessageStatus {
    // Create a new MessageStatus instance when a message is sent
    pub fn sent(time: Instant) -> Self {
        MessageStatus {
            sent_time: Some(time),
            delivered_time: None,
            acknowledged_time: None,
        }
    }

    // Update the delivered time for a message
    pub fn delivered(&mut self, time: Instant) {
        self.delivered_time = Some(time);
    }

    // Update the acknowledged time for a message
    pub fn acknowledged(&mut self, time: Instant) {
        self.acknowledged_time = Some(time);
    }
}

// Structure to manage the server's state, including queues, bindings, and message tracking
pub struct ServerState {
    queues: DashMap<String, Vec<(String, EncryptedInputData)>>, // Map of queue names to message vectors
    bindings: DashMap<String, Vec<String>>,                     // Map of exchange names to bound queues
    exchanges: DashMap<String, Vec<String>>,                    // Map of exchange names to routing keys
    consumers: DashMap<String, Vec<mpsc::UnboundedSender<(String, EncryptedInputData)>>>, // Map of queues to consumer channels
    message_status: DashMap<String, MessageStatus>,             // Map of message IDs to their status
    request_times: DashMap<String, Vec<f64>>,                   // Map of command names to their execution times
    connected_clients: Arc<RwLock<usize>>,                      // Thread-safe counter for connected clients
}

// Implementation of methods for managing server state
impl ServerState {
    // Initialize a new ServerState instance with empty collections
    pub fn new() -> Self {
        ServerState {
            queues: DashMap::new(),
            bindings: DashMap::new(),
            exchanges: DashMap::new(),
            consumers: DashMap::new(),
            message_status: DashMap::new(),
            request_times: DashMap::new(),
            connected_clients: Arc::new(RwLock::new(0)),
        }
    }

    // Declare a new queue if it doesn't exist
    pub fn declare_queue(&self, queue_name: &str) {
        self.queues.entry(queue_name.to_string()).or_insert_with(Vec::new);
        info!("Queue '{}' declared", queue_name);
    }

    // Declare a new exchange if it doesn't exist
    pub fn declare_exchange(&self, exchange_name: &str) {
        self.exchanges
            .entry(exchange_name.to_string())
            .or_insert_with(Vec::new);
        info!("Exchange '{}' declared", exchange_name);
    }

    // Bind a queue to an exchange with a routing key
    pub fn bind_queue(&self, queue_name: &str, exchange_name: &str, routing_key: &str) {
        if self.queues.contains_key(queue_name) && self.exchanges.contains_key(exchange_name) {
            self.bindings
                .entry(exchange_name.to_string())
                .or_insert_with(Vec::new)
                .push(queue_name.to_string());
            info!(
                "Queue '{}' bound to exchange '{}' with routing key '{}'",
                queue_name, exchange_name, routing_key
            );
        } else {
            warn!(
                "Binding failed: Queue '{}' or exchange '{}' does not exist",
                queue_name, exchange_name
            );
        }
    }

    // Publish a message to an exchange, routing it to bound queues
    pub fn publish(
        &self,
        exchange_name: &str,
        routing_key: &str,
        message: EncryptedInputData,
        received_time: Instant,
    ) -> Result<(), String> {
        let message_id = message.message_id.clone();
        // Ignore duplicate messages
        if self.message_status.contains_key(&message_id) {
            warn!("Duplicate message {} ignored", message_id);
            return Ok(());
        }
        if let Some(queue_names) = self.bindings.get(exchange_name) {
            for queue_name in queue_names.iter() {
                if let Some(mut queue) = self.queues.get_mut(queue_name) {
                    queue.push((message_id.clone(), message.clone()));
                    if let Some(mut consumer_senders) = self.consumers.get_mut(queue_name) {
                        consumer_senders.retain(|sender| {
                            if sender.send((message_id.clone(), message.clone())).is_err() {
                                warn!("Failed to send message to consumer for queue '{}'", queue_name);
                                false
                            } else {
                                true
                            }
                        });
                    }
                }
            }
            self.message_status
                .insert(message_id.clone(), MessageStatus::sent(received_time));
            info!(
                "Published message {} to queue '{}' via exchange '{}'",
                message_id, routing_key, exchange_name
            );
            Ok(())
        } else {
            Err(format!("Exchange '{}' not found", exchange_name))
        }
    }

    // Register a consumer for a queue to receive messages
    pub fn register_consumer(
        &self,
        queue_name: &str,
        sender: mpsc::UnboundedSender<(String, EncryptedInputData)>,
    ) {
        self.declare_queue(queue_name);
        self.consumers
            .entry(queue_name.to_string())
            .or_insert_with(Vec::new)
            .push(sender);
        info!("Consumer registered for queue '{}'", queue_name);
    }

    // Consume a message from a queue, marking it as delivered
    pub fn consume(&self, queue_name: &str) -> Option<(String, EncryptedInputData)> {
        if let Some(mut queue) = self.queues.get_mut(queue_name) {
            if !queue.is_empty() {
                let (message_id, message) = queue.remove(0);
                if let Some(mut status) = self.message_status.get_mut(&message_id) {
                    status.delivered(Instant::now());
                }
                return Some((message_id, message));
            }
        }
        None
    }

    // Acknowledge a message, removing it from queues and status
    pub fn acknowledge(&self, message_id: &str) {
        if let Some(mut status) = self.message_status.get_mut(message_id) {
            status.acknowledged(Instant::now());
        }
        // Remove message from all queues
        for mut queue in self.queues.iter_mut() {
            queue.retain(|(id, _)| id != message_id);
        }
        // Remove message status
        self.message_status.remove(message_id);
        info!("Message {} acknowledged by client, removed from queues and status", message_id);
    }

    // Reset server statistics, clearing message status and request times
    pub fn reset_stats(&self) {
        self.message_status.clear();
        self.request_times.clear();
        info!("Server stats reset");
    }

    // Increment the count of connected clients
    pub async fn increment_client_count(&self) {
        let mut count = self.connected_clients.write().await;
        *count += 1;
    }

    // Decrement the count of connected clients
    pub async fn decrement_client_count(&self) {
        let mut count = self.connected_clients.write().await;
        *count -= 1;
    }

    // Record the execution time of a command
    pub fn record_request_time(&self, command: &str, duration: f64) {
        self.request_times
            .entry(command.to_string())
            .or_insert_with(Vec::new)
            .push(duration);
    }

    // Retrieve server statistics in JSON format
    pub async fn get_stats(&self) -> serde_json::Value {
        let queue_stats: Vec<(String, usize)> = self
            .queues
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().len()))
            .collect();
        let consumer_stats: Vec<(String, usize)> = self
            .consumers
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().len()))
            .collect();
        let status_counts = self.message_status.iter().fold(
            (0, 0, 0),
            |(sent, delivered, acknowledged), entry| {
                let status = entry.value();
                let sent = if status.sent_time.is_some() { sent + 1 } else { sent };
                let delivered = if status.delivered_time.is_some() { delivered + 1 } else { delivered };
                let acknowledged = if status.acknowledged_time.is_some() { acknowledged + 1 } else { acknowledged };
                (sent, delivered, acknowledged)
            },
        );
// --------------------- Start Calculate average times ----------------------------------------------

        // // Calculate average times for publish-to-ack and delivery-to-ack
        // let mut publish_to_ack_times: Vec<f64> = Vec::new();
        // let mut delivery_to_ack_times: Vec<f64> = Vec::new();
        // for entry in self.message_status.iter() {
        //     let status = entry.value();
        //     if let (Some(sent_time), Some(ack_time)) = (status.sent_time, status.acknowledged_time) {
        //         publish_to_ack_times.push(ack_time.duration_since(sent_time).as_secs_f64());
        //     }
        //     if let (Some(delivered_time), Some(ack_time)) = (status.delivered_time, status.acknowledged_time) {
        //         delivery_to_ack_times.push(ack_time.duration_since(delivered_time).as_secs_f64());
        //     }
        // }
        // let avg_publish_to_ack = if !publish_to_ack_times.is_empty() {
        //     publish_to_ack_times.iter().sum::<f64>() / publish_to_ack_times.len() as f64
        // } else {
        //     0.0
        // };
        // let avg_delivery_to_ack = if !delivery_to_ack_times.is_empty() {
        //     delivery_to_ack_times.iter().sum::<f64>() / delivery_to_ack_times.len() as f64
        // } else {
        //     0.0
        // };

        // // Collect request time statistics, including average and total times
        // let request_time_stats: Vec<(String, f64, usize, f64)> = self
        //     .request_times
        //     .iter()
        //     .map(|entry| {
        //         let times = entry.value();
        //         let count = times.len();
        //         let sum_time = times.iter().sum::<f64>();
        //         let avg_time = if count > 0 { sum_time / count as f64 } else { 0.0 };
        //         (entry.key().clone(), avg_time, count, sum_time)
        //     })
        //     .collect();

// --------------------- End Calculate average times ----------------------------------------------

        // Return statistics as a JSON object
        json!({
            "queues": queue_stats,
            "consumers": consumer_stats,
            "message_status_counts": {
                "sent": status_counts.0,
                "delivered": status_counts.1,
                "acknowledged": status_counts.2
            },
            "total_exchanges": self.exchanges.len(),
            "total_bindings": self.bindings.iter().map(|entry| entry.value().len()).sum::<usize>(),
            "connected_clients": *self.connected_clients.read().await,
            // "request_times_avg_seconds": request_time_stats, //Calculate average times
            // "avg_publish_to_ack_seconds": avg_publish_to_ack, //Calculate average times
            // "avg_delivery_to_ack_seconds": avg_delivery_to_ack //Calculate average times
        })
    }
}

// Handle incoming client connections and process commands
#[instrument(skip(stream, state))]
async fn handle_client(mut stream: TcpStream, state: Arc<RwLock<ServerState>>) {
    state.write().await.increment_client_count().await;
    let mut buffer = Vec::with_capacity(4096);
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, EncryptedInputData)>();

    loop {
        tokio::select! {
            // Read incoming data from the client
            result = timeout(Duration::from_secs(1), stream.read_buf(&mut buffer)) => {
                match result {
                    Ok(Ok(n)) if n == 0 => {
                        info!("Client disconnected");
                        state.write().await.decrement_client_count().await;
                        return;
                    }
                    Ok(Ok(_)) => {
                        let start_time = Instant::now();
                        let request = String::from_utf8_lossy(&buffer).trim().to_string();
                        info!("Received request: {}", request);

                        let parts: Vec<&str> = request.splitn(2, ' ').collect();
                        let command = parts.get(0).unwrap_or(&"");
                        let args_str = parts.get(1).unwrap_or(&"");

                        // Process client commands
                        match *command {
                            "declare_queue" => {
                                if !args_str.is_empty() {
                                    state.read().await.declare_queue(args_str);
                                    stream.write_all(b"Queue declared\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing queue name\n").await.unwrap();
                                }
                            }
                            "declare_exchange" => {
                                if !args_str.is_empty() {
                                    state.read().await.declare_exchange(args_str);
                                    stream.write_all(b"Exchange declared\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing exchange name\n").await.unwrap();
                                }
                            }
                            "bind" => {
                                let args: Vec<&str> = args_str.splitn(3, ' ').collect();
                                if args.len() >= 3 {
                                    state.read().await.bind_queue(args[0], args[1], args[2]);
                                    stream.write_all(b"Queue bound\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing parameters\n").await.unwrap();
                                }
                            }
                            "publish" => {
                                let args: Vec<&str> = args_str.splitn(3, ' ').collect();
                                if args.len() >= 3 {
                                    let message_str = args[2];
                                    match serde_json::from_str::<EncryptedInputData>(message_str) {
                                        Ok(message) => {
                                            match state.read().await.publish(args[0], args[1], message.clone(), start_time) {
                                                Ok(()) => {
                                                    stream
                                                        .write_all(format!("ACK {}\n", message.message_id).as_bytes())
                                                        .await
                                                        .unwrap();
                                                }
                                                Err(e) => {
                                                    stream
                                                        .write_all(format!("Error: {}\n", e).as_bytes())
                                                        .await
                                                        .unwrap();
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            stream
                                                .write_all(format!("Invalid message format: {}\n", e).as_bytes())
                                                .await
                                                .unwrap();
                                        }
                                    }
                                } else {
                                    stream.write_all(b"Missing parameters\n").await.unwrap();
                                }
                            }
                            "publish_batch" => {
                                let args: Vec<&str> = args_str.splitn(3, ' ').collect();
                                if args.len() >= 3 {
                                    let exchange = args[0];
                                    let routing_key = args[1];
                                    let message_str = args[2];
                                    info!("Parsing batch JSON: {}", message_str);
                                    match serde_json::from_str::<Vec<EncryptedInputData>>(message_str) {
                                        Ok(messages) => {
                                            let mut acks = Vec::new();
                                            for message in messages {
                                                match state.read().await.publish(exchange, routing_key, message.clone(), start_time) {
                                                    Ok(()) => {
                                                        acks.push(format!("ACK {}", message.message_id));
                                                    }
                                                    Err(e) => {
                                                        acks.push(format!("Error: {} for message {}", e, message.message_id));
                                                    }
                                                }
                                            }
                                            let response = acks.join("\n") + "\n";
                                            stream.write_all(response.as_bytes()).await.unwrap();
                                        }
                                        Err(e) => {
                                            stream
                                                .write_all(format!("Invalid batch format: {}\n", e).as_bytes())
                                                .await
                                                .unwrap();
                                        }
                                    }
                                } else {
                                    stream.write_all(b"Missing parameters\n").await.unwrap();
                                }
                            }
                            "consume" => {
                                if !args_str.is_empty() {
                                    state.read().await.register_consumer(args_str, tx.clone());
                                    stream.write_all(b"Subscribed to queue\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing queue name\n").await.unwrap();
                                }
                            }
                            "fetch" => {
                                if !args_str.is_empty() {
                                    if let Some((message_id, message)) = state.read().await.consume(args_str) {
                                        let message_str = serde_json::to_string(&message).unwrap();
                                        stream
                                            .write_all(format!("Message: {} {}\n", message_id, message_str).as_bytes())
                                            .await
                                            .unwrap();
                                    } else {
                                        stream.write_all(b"No messages\n").await.unwrap();
                                    }
                                } else {
                                    stream.write_all(b"Missing queue name\n").await.unwrap();
                                }
                            }
                            "ack" => {
                                if !args_str.is_empty() {
                                    info!("Processing ACK for message {}", args_str);
                                    state.read().await.acknowledge(args_str);
                                    stream
                                        .write_all(format!("ACK_CONFIRMED {}\n", args_str).as_bytes())
                                        .await
                                        .unwrap();
                                } else {
                                    stream.write_all(b"Missing message ID\n").await.unwrap();
                                }
                            }
                            "stats" => {
                                let stats = state.read().await.get_stats().await;
                                let stats_str = serde_json::to_string(&stats).unwrap();
                                stream
                                    .write_all(format!("Stats: {}\n", stats_str).as_bytes())
                                    .await
                                    .unwrap();
                            }
                            "reset_stats" => {
                                state.read().await.reset_stats();
                                stream.write_all(b"Stats reset\n").await.unwrap();
                            }
                            _ => {
                                stream.write_all(b"Unknown command\n").await.unwrap();
                            }
                        }
                        let duration = start_time.elapsed().as_secs_f64();
                        state.read().await.record_request_time(command, duration);
                        buffer.clear();
                    }
                    Ok(Err(e)) => {
                        error!("Error reading from stream: {}", e);
                        state.write().await.decrement_client_count().await;
                        return;
                    }
                    Err(_) => {
                        continue;
                    }
                }
            }
            // Handle messages sent to consumers
            Some((message_id, message)) = rx.recv() => {
                let start_time = Instant::now();
                let message_str = serde_json::to_string(&message).unwrap();
                if stream
                    .write_all(format!("Message: {} {}\n", message_id, message_str).as_bytes())
                    .await
                    .is_err()
                {
                    warn!("Failed to send message to client");
                    state.write().await.decrement_client_count().await;
                    return;
                }
                if let Some(mut status) = state.read().await.message_status.get_mut(&message_id) {
                    status.delivered(start_time);
                }
                let duration = start_time.elapsed().as_secs_f64();
                state.read().await.record_request_time("message_delivery", duration);
            }
        }
    }
}

// Main function to start the server
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init(); // Initialize logging

    let listener = TcpListener::bind("127.0.0.1:5672").await.unwrap(); // Bind to local TCP port
    let state = Arc::new(RwLock::new(ServerState::new())); // Create shared server state
    info!("Server listening on 127.0.0.1:5672");

    // Accept incoming connections and spawn handlers
    loop {
        let (stream, addr) = listener.accept().await.unwrap();
        info!("New connection from {}", addr);
        let state = state.clone();
        tokio::spawn(async move {
            handle_client(stream, state).await;
        });
    }
}