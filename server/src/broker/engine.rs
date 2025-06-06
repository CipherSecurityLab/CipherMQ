use crate::core::message::{EncryptedInputData, MessageStatus};
use crate::core::state::ServerState; // This is the data store
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;
use tracing::{error, info, warn}; // Add tracing, removed debug

// The BrokerEngine is responsible for handling the main logic of message brokerage.
// It uses ServerState as its underlying data store.
pub struct BrokerEngine {
    state: Arc<ServerState>,
}

impl BrokerEngine {
    pub fn new(state: Arc<ServerState>) -> Self {
        BrokerEngine { state }
    }

    // --- Command-processing methods ---

    pub fn declare_queue(&self, queue_name: String) {
        // Logic moved from ServerState::declare_queue
        if self.state.queues.contains_key(&queue_name) {
            warn!("Queue {} already exists", queue_name);
            return;
        }
        // ServerState now needs a way to create a new queue, or Queue::new is used directly
        // Assuming Queue::new is accessible via crate::core::queue::Queue
        self.state.queues.insert(queue_name.clone(), crate::core::queue::Queue::new(queue_name.clone()));
        info!("BrokerEngine: Queue declared: {}", queue_name);
    }

    pub fn declare_exchange(&self, exchange_name: String) {
        // Logic moved from ServerState::declare_exchange
        if self.state.exchanges.contains_key(&exchange_name) {
            warn!("Exchange {} already exists", exchange_name);
            return;
        }
        // Assuming Exchange::new is accessible via crate::core::exchange::Exchange
        self.state.exchanges.insert(exchange_name.clone(), crate::core::exchange::Exchange::new(exchange_name.clone()));
        info!("BrokerEngine: Exchange declared: {}", exchange_name);
    }

    pub fn bind_queue(
        &self,
        exchange_name: String,
        queue_name: String,
        routing_key: String,
    ) {
        // Logic moved from ServerState::bind_queue
        if !self.state.queues.contains_key(&queue_name) {
            error!(
                "BrokerEngine: Binding failed: Queue {} does not exist. Declare it first.",
                queue_name
            );
            return;
        }

        let Some(mut exchange) = self.state.exchanges.get_mut(&exchange_name) else {
            error!(
                "BrokerEngine: Binding failed: Exchange {} does not exist. Declare it first.",
                exchange_name
            );
            return;
        };

        exchange
            .bindings
            .entry(routing_key.clone())
            .or_default()
            .push(queue_name.clone());
        info!(
            "BrokerEngine: Queue {} bound to exchange {} with routing key {}",
            queue_name, exchange_name, routing_key
        );
    }

    pub async fn publish(
        &self,
        exchange_name: String,
        routing_key: String,
        message_data: EncryptedInputData,
    ) -> Result<Uuid, String> {
        // Logic moved from ServerState::publish
        let Some(exchange) = self.state.exchanges.get(&exchange_name) else {
            error!("BrokerEngine: Exchange {} not found for publishing", exchange_name);
            return Err(format!("Exchange {} not found", exchange_name));
        };

        let bound_queue_names = match exchange.bindings.get(&routing_key) {
            Some(names) if !names.is_empty() => names.clone(),
            _ => {
                warn!(
                    "BrokerEngine: No queues bound to exchange {} with routing key {}. Message dropped.",
                    exchange_name, routing_key
                );
                return Err(format!("No queues bound for routing key {}", routing_key));
            }
        };

        let message_id = Uuid::new_v4();
        let timestamp = chrono::Utc::now().timestamp(); // Ensure chrono is in scope
        let message_status = MessageStatus {
            message_id,
            data: message_data,
            timestamp,
            attempts: 0,
        };

        for queue_name in bound_queue_names {
            let Some(mut queue_entry) = self.state.queues.get_mut(&queue_name) else {
                warn!(
                    "BrokerEngine: Queue {} not found for message {} from exchange {}",
                    queue_name, message_id, exchange_name
                );
                continue;
            };

            let queue = &mut *queue_entry;
            let mut sent_to_consumer = false;

            if !queue.consumers.is_empty() {
                let consumer_tx = queue.consumers.remove(0);
                match consumer_tx.send(message_status.clone()).await {
                    Ok(_) => {
                        info!(
                            "BrokerEngine: Message {} sent directly to a consumer of queue {}",
                            message_id, queue_name
                        );
                        self.state.unacknowledged_messages.insert(
                            message_id,
                            (queue_name.clone(), message_status.clone()),
                        );
                        sent_to_consumer = true;
                        if !consumer_tx.is_closed() {
                            queue.consumers.push(consumer_tx);
                        } else {
                            warn!("BrokerEngine: Consumer for queue {} disconnected after send.", queue_name);
                        }
                    }
                    Err(e) => {
                        error!(
                            "BrokerEngine: Failed to send message {} to consumer of queue {}: {:?}.",
                            message_id, queue_name, e
                        );
                    }
                }
            }

            if !sent_to_consumer {
                queue.messages.push_back(message_status.clone());
                info!(
                    "BrokerEngine: Message {} enqueued in {} for exchange {} with key {}",
                    message_id, queue_name, exchange_name, routing_key
                );
                self.state.unacknowledged_messages.insert(
                    message_id,
                    (queue_name.clone(), message_status.clone()),
                );
            }
        }
        Ok(message_id)
    }

    pub async fn register_consumer(
        &self,
        queue_name: String,
        consumer_tx: mpsc::Sender<MessageStatus>,
    ) -> Result<(), String> {
        // Logic moved from ServerState::register_consumer
        let mut queue = self
            .state.queues
            .get_mut(&queue_name)
            .ok_or_else(|| format!("BrokerEngine: Queue {} not found", queue_name))?;

        if let Some(message_status) = queue.messages.pop_front() {
            match consumer_tx.send(message_status.clone()).await {
                Ok(_) => {
                    info!(
                        "BrokerEngine: Sent existing message {} to new consumer for queue {}",
                        message_status.message_id, queue_name
                    );
                    self.state.unacknowledged_messages.insert(
                        message_status.message_id,
                        (queue_name.clone(), message_status),
                    );
                    if !consumer_tx.is_closed() {
                        queue.consumers.push(consumer_tx);
                    } else {
                        warn!("BrokerEngine: Consumer for {} closed immediately after receiving an existing message.", queue_name);
                    }
                }
                Err(e) => {
                    error!(
                        "BrokerEngine: Failed to send message {} to new consumer for queue {}: {:?}. Message re-enqueued.",
                        message_status.message_id, queue_name, e
                    );
                    queue.messages.push_front(message_status);
                    return Err(format!(
                        "Failed to send initial message to consumer: {:?}",
                        e
                    ));
                }
            }
        } else {
            queue.consumers.push(consumer_tx);
            info!("BrokerEngine: New consumer registered for queue {}", queue_name);
        }
        Ok(())
    }

    pub fn fetch_one_message(&self, queue_name: &str) -> Option<MessageStatus> {
        // Logic moved from ServerState::fetch_one_message
        let mut queue_entry = self.state.queues.get_mut(queue_name)?;
        
        let mut message_to_send_idx = None;
        for (idx, _msg_status) in queue_entry.messages.iter().enumerate() {
            message_to_send_idx = Some(idx);
            break;
        }

        if let Some(idx) = message_to_send_idx {
            if let Some(message_status) = queue_entry.messages.remove(idx) {
                info!("BrokerEngine: Message {} fetched from queue {}", message_status.message_id, queue_name);
                self.state.unacknowledged_messages.insert(
                    message_status.message_id,
                    (queue_name.to_string(), message_status.clone()),
                );
                return Some(message_status);
            }
        }
        None
    }

    pub fn acknowledge(&self, message_id_str: String) {
        // Logic moved from ServerState::acknowledge
        match Uuid::parse_str(&message_id_str) {
            Ok(message_id) => {
                if self.state.unacknowledged_messages.remove(&message_id).is_some() {
                    info!("BrokerEngine: Message {} acknowledged and removed from unacknowledged list.", message_id);
                    for mut queue_entry in self.state.queues.iter_mut() {
                        let queue = queue_entry.value_mut();
                        let initial_len = queue.messages.len();
                        queue.messages.retain(|msg_status| msg_status.message_id != message_id);
                        if queue.messages.len() < initial_len {
                             info!("BrokerEngine: Message {} removed from queue {}'s message deque.", message_id, queue.name);
                        }
                    }
                } else {
                    warn!(
                        "BrokerEngine: Attempted to acknowledge message {}, but it was not found in unacknowledged list.",
                        message_id
                    );
                }
            }
            Err(e) => {
                error!("BrokerEngine: Invalid UUID format for acknowledgment: {}. Error: {:?}", message_id_str, e);
            }
        }
    }

    // The retry_unacknowledged logic involves an infinite loop and async sleep,
    // so it's better managed by ServerState itself or spawned as a separate task by main.
    // BrokerEngine can provide a way to access the state for this if needed,
    // or ServerState's retry_unacknowledged can remain mostly as is, using its own fields.
    // For this refactoring, we'll assume `retry_unacknowledged` remains a public method on `ServerState`
    // and `BrokerEngine` doesn't need to wrap it directly, but `main.rs` would use `state.retry_unacknowledged()`.
    // If BrokerEngine *were* to manage it, it might look like:
    // pub async fn run_retry_logic(&self) {
    //    self.state.retry_unacknowledged().await;
    // }
    // But this means BrokerEngine itself would have to be Arc-wrapped if multiple things call its methods.
    // For now, we keep retry logic primarily in ServerState.
}
