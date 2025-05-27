use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

// Data structure for encrypted message with unique identifier
#[derive(Serialize, Deserialize, Clone, Debug)]
struct EncryptedInputData {
    message_id: String, // Unique identifier for the message
    ciphertext: String,
    nonce: String,
    tag: String,
    enc_session_key: String,
}

// Message status
#[derive(Clone, Debug)]
enum MessageStatus {
    Sent,        // Message added to the queue
    Delivered,  // Message delivered to a consumer
    Acknowledged, // Consumer has acknowledged the message
}

// Structure for queue
#[derive(Clone)]
struct Queue {
    name: String,
    messages: VecDeque<(EncryptedInputData, MessageStatus)>,
    consumers: Vec<mpsc::UnboundedSender<(String, EncryptedInputData)>>, // (message_id, message)
}

// Structure for exchange
#[derive(Clone)]
struct Exchange {
    name: String,
    bindings: HashMap<String, String>, // Routing key -> Queue name
}

// Server state
struct ServerState {
    queues: DashMap<String, Queue>,
    exchanges: DashMap<String, Exchange>,
    message_status: DashMap<String, MessageStatus>, // message_id -> status
}

// Initializes server state with thread-safe DashMaps for concurrent queue, exchange, and message status management
impl ServerState {
    fn new() -> Self {
        ServerState {
            queues: DashMap::new(),
            exchanges: DashMap::new(),
            message_status: DashMap::new(),
        }
    }

    // Declare queue
    fn declare_queue(&self, queue_name: &str) {
        if !self.queues.contains_key(queue_name) {
            let queue = Queue {
                name: queue_name.to_string(),
                messages: VecDeque::new(),
                consumers: Vec::new(),
            };
            self.queues.insert(queue_name.to_string(), queue);
            info!("Queue '{}' declared with name: {}", queue_name, queue_name);
        }
    }

    // Declare exchange
    fn declare_exchange(&self, exchange_name: &str) {
        if !self.exchanges.contains_key(exchange_name) {
            let exchange = Exchange {
                name: exchange_name.to_string(),
                bindings: HashMap::new(),
            };
            self.exchanges.insert(exchange_name.to_string(), exchange);
            info!("Exchange '{}' declared with name: {}", exchange_name, exchange_name);
        }
    }

    // Bind queue to exchange
    fn bind_queue(&self, exchange_name: &str, queue_name: &str, routing_key: &str) {
        if let Some(mut exchange) = self.exchanges.get_mut(exchange_name) {
            exchange
                .bindings
                .insert(routing_key.to_string(), queue_name.to_string());
            info!(
                "Queue '{}' bound to exchange '{}' (name: {}) with routing key '{}'",
                queue_name, exchange_name, exchange.name, routing_key
            );
        } else {
            warn!("Exchange '{}' not found", exchange_name);
        }
    }

    // Publishes a message to the specified exchange, routes it to the appropriate queue based on the routing key, and notifies connected consumers
    fn publish(&self, exchange_name: &str, routing_key: &str, mut message: EncryptedInputData) {
        if message.message_id.is_empty() {
            message.message_id = Uuid::new_v4().to_string();
        }
        self.message_status
            .insert(message.message_id.clone(), MessageStatus::Sent);

        if let Some(exchange) = self.exchanges.get(exchange_name) {
            if let Some(queue_name) = exchange.bindings.get(routing_key) {
                if let Some(mut queue) = self.queues.get_mut(queue_name) {
                    queue
                        .messages
                        .push_back((message.clone(), MessageStatus::Sent));
                    // Send to connected consumers
                    for consumer in &queue.consumers {
                        if consumer
                            .send((message.message_id.clone(), message.clone()))
                            .is_err()
                        {
                            warn!("Failed to send message to consumer for queue '{}'", queue.name);
                        }
                    }
                    info!(
                        "Published message {} to queue '{}' (name: {}) via exchange '{}' (name: {})",
                        message.message_id, queue_name, queue.name, exchange_name, exchange.name
                    );
                } else {
                    warn!("Queue '{}' not found", queue_name);
                }
            } else {
                warn!("No binding found for routing key '{}'", routing_key);
            }
        } else {
            warn!("Exchange '{}' not found", exchange_name);
        }
    }

    // Register consumer
    fn register_consumer(
        &self,
        queue_name: &str,
        tx: mpsc::UnboundedSender<(String, EncryptedInputData)>,
    ) {
        if let Some(mut queue) = self.queues.get_mut(queue_name) {
            queue.consumers.push(tx);
            // Send existing messages to the new consumer
            for (message, status) in &queue.messages {
                if matches!(status, MessageStatus::Sent) {
                    if let Some(consumer) = queue.consumers.last() {
                        if consumer.send((message.message_id.clone(), message.clone())).is_err() {
                            warn!("Failed to send message to new consumer for queue '{}'", queue.name);
                        }
                    }
                }
            }
            info!("Consumer registered for queue '{}' (name: {})", queue_name, queue.name);
        } else {
            warn!("Queue '{}' not found", queue_name);
        }
    }

    // Acknowledge message by consumer
    fn acknowledge(&self, message_id: &str) {
        if let Some(mut status) = self.message_status.get_mut(message_id) {
            *status = MessageStatus::Acknowledged;
            info!("Message {} acknowledged", message_id);

            // Remove message from queues
            for mut queue_entry in self.queues.iter_mut() {
                let queue = queue_entry.value_mut();
                queue.messages.retain(|(msg, _)| msg.message_id != message_id);
            }
        } else {
            warn!("Message ID '{}' not found", message_id);
        }
    }

    // Manual message consum
    fn consume(&self, queue_name: &str) -> Option<(String, EncryptedInputData)> {
        if let Some(mut queue) = self.queues.get_mut(queue_name) {
            if let Some((message, status)) = queue.messages.pop_front() {
                if matches!(status, MessageStatus::Sent) {
                    self.message_status
                        .insert(message.message_id.clone(), MessageStatus::Delivered);
                    return Some((message.message_id.clone(), message));
                }
            }
        }
        None
    }
}

// Asynchronously handles client commands and message delivery using tokio's select! macro, with mpsc channels for consumer notifications
async fn handle_client(mut stream: TcpStream, state: Arc<ServerState>) {
    let mut buffer = [0; 1024];
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, EncryptedInputData)>();

    loop {
        tokio::select! {
            // Receive message from client
            result = stream.read(&mut buffer) => {
                match result {
                    Ok(n) if n == 0 => {
                        info!("Client disconnected");
                        return;
                    }
                    Ok(n) => {
                        let request = String::from_utf8_lossy(&buffer[..n]).trim().to_string();
                        let parts: Vec<&str> = request.splitn(2, ' ').collect();

                        if parts.is_empty() {
                            stream.write_all(b"Invalid command\n").await.unwrap();
                            continue;
                        }

                        let command = parts[0];
                        let args = parts.get(1).unwrap_or(&"").split_whitespace().collect::<Vec<&str>>();

                        match command {
                            "declare_queue" => {
                                if !args.is_empty() {
                                    state.declare_queue(args[0]);
                                    stream.write_all(b"Queue declared\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing queue name\n").await.unwrap();
                                }
                            }
                            "declare_exchange" => {
                                if !args.is_empty() {
                                    state.declare_exchange(args[0]);
                                    stream.write_all(b"Exchange declared\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing exchange name\n").await.unwrap();
                                }
                            }
                            "bind" => {
                                if args.len() >= 3 {
                                    state.bind_queue(args[0], args[1], args[2]);
                                    stream.write_all(b"Queue bound\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing parameters\n").await.unwrap();
                                }
                            }
                            "publish" => {
                                if args.len() >= 3 {
                                    let message_str = args[2..].join(" ");
                                    match serde_json::from_str::<EncryptedInputData>(&message_str) {
                                        Ok(message) => {
                                            state.publish(args[0], args[1], message);
                                            stream.write_all(b"Message published\n").await.unwrap();
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
                            "consume" => {
                                if !args.is_empty() {
                                    state.register_consumer(args[0], tx.clone());
                                    stream.write_all(b"Subscribed to queue\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing queue name\n").await.unwrap();
                                }
                            }
                            "fetch" => {
                                if !args.is_empty() {
                                    if let Some((message_id, message)) = state.consume(args[0]) {
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
                                if !args.is_empty() {
                                    state.acknowledge(args[0]);
                                    stream.write_all(b"Message acknowledged\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing message ID\n").await.unwrap();
                                }
                            }
                            _ => {
                                stream.write_all(b"Unknown command\n").await.unwrap();
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error reading from stream: {}", e);
                        return;
                    }
                }
            }
            // Send message to client
            Some((message_id, message)) = rx.recv() => {
                let message_str = serde_json::to_string(&message).unwrap();
                if stream.write_all(format!("Message: {} {}\n", message_id, message_str).as_bytes()).await.is_err() {
                    warn!("Failed to send message to client");
                    return;
                }
                // Update status to Delivered
                state.message_status.insert(message_id.clone(), MessageStatus::Delivered);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = Arc::new(ServerState::new());
    let listener = TcpListener::bind("127.0.0.1:5672").await.unwrap();
    info!("Message Broker running on 127.0.0.1:5672");

    // Sets up a default queue and exchange for basic message routing, bound with a default routing key
    state.declare_queue("default_queue");
    state.declare_exchange("default_exchange");
    state.bind_queue("default_exchange", "default_queue", "default_key");

    while let Ok((stream, addr)) = listener.accept().await {
        info!("New connection from {}", addr);
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            handle_client(stream, state).await;
        });
    }
}