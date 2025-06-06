use crate::core::exchange::Exchange;
use crate::core::message::MessageStatus; // EncryptedInputData is not directly used here anymore
use crate::core::queue::Queue;
use dashmap::DashMap;
// VecDeque is used by Queue, not directly by ServerState methods anymore
// use std::collections::VecDeque; 
// use tokio::sync::mpsc; // Not strictly needed here as mpsc::Sender type comes from Queue struct
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use chrono; // Used by retry_unacknowledged

// ServerState is now primarily a data container.
// Business logic has moved to BrokerEngine.
#[derive(Debug)]
pub struct ServerState {
    // These fields are public so BrokerEngine can access them.
    // Consider if getter methods are preferred for more controlled access in a future refactor.
    pub queues: DashMap<String, Queue>,
    pub exchanges: DashMap<String, Exchange>,
    pub unacknowledged_messages: DashMap<Uuid, (String, MessageStatus)>, 
}

impl ServerState {
    pub fn new() -> Self {
        ServerState {
            queues: DashMap::new(),
            exchanges: DashMap::new(),
            unacknowledged_messages: DashMap::new(),
        }
    }

    // Note: declare_queue, declare_exchange, bind_queue, publish, register_consumer,
    // fetch_one_message, acknowledge logic has been moved to BrokerEngine.
    // ServerState is now more of a data store. BrokerEngine interacts with its fields directly
    // or via more granular helper methods if those were/are to be implemented on ServerState.

    // The retry logic remains part of ServerState as it directly manipulates its core data
    // structures (queues, unacknowledged_messages) and handles message redelivery.
    async fn re_enqueue_message(&self, queue_name: String, mut message_status: MessageStatus) {
        let Some(mut queue_entry) = self.queues.get_mut(&queue_name) else {
            warn!("ServerState: Queue {} not found for re-enqueueing message {}. Removing from unacknowledged.", queue_name, message_status.message_id);
            self.unacknowledged_messages.remove(&message_status.message_id);
            return;
        };

        message_status.attempts += 1;
        let message_id = message_status.message_id;

        if message_status.attempts > 5 { // Max retries
            error!("ServerState: Message {} in queue {} exceeded max retries ({} attempts). Moving to dead-letter (not implemented). For now, dropping.", message_id, queue_name, message_status.attempts);
            self.unacknowledged_messages.remove(&message_id);
            // TODO: Implement dead-letter queue logic here
            return;
        }
        
        let mut sent_to_consumer = false;
        if !queue_entry.consumers.is_empty() {
            let consumer_tx = queue_entry.consumers.remove(0);
            match consumer_tx.send(message_status.clone()).await {
                Ok(_) => {
                    info!("ServerState: Message {} (retry attempt {}) sent directly to a consumer of queue {}", message_id, message_status.attempts, queue_name);
                    // Update the unacknowledged_messages with the new attempt count and potentially new timestamp
                    self.unacknowledged_messages.insert(message_id, (queue_name.clone(), message_status.clone()));
                    sent_to_consumer = true;
                    if !consumer_tx.is_closed() {
                        queue_entry.consumers.push(consumer_tx);
                    } else {
                        warn!("ServerState: Consumer for queue {} disconnected during retry send.", queue_name);
                    }
                }
                Err(e) => {
                    error!("ServerState: Failed to re-send message {} to consumer of queue {}: {:?}. Will re-enqueue to general queue.", message_id, queue_name, e);
                }
            }
        }

        if !sent_to_consumer {
            // If no consumer or send failed, put it back in the queue's VecDeque
            queue_entry.messages.push_back(message_status.clone());
            info!("ServerState: Message {} (retry attempt {}) re-enqueued in {} due to no available/successful consumer.", message_id, message_status.attempts, queue_name);
            // Ensure it's still tracked in unacknowledged_messages
            self.unacknowledged_messages.insert(message_id, (queue_name.clone(), message_status));
        }
    }

    pub async fn retry_unacknowledged(&self) {
        info!("ServerState: Starting retry mechanism for unacknowledged messages...");
        let retry_interval = tokio::time::Duration::from_secs(60); 

        loop {
            tokio::time::sleep(retry_interval).await;
            debug!("ServerState: Checking for unacknowledged messages to retry...");

            let current_time = chrono::Utc::now().timestamp();
            let mut messages_to_process = Vec::new();

            for entry in self.unacknowledged_messages.iter() {
                let (queue_name, msg_status) = entry.value();
                if current_time - msg_status.timestamp > 60 { 
                    warn!(
                        "ServerState: Message {} in queue '{}' is unacknowledged for too long ({} attempts). Attempting to re-process.",
                        msg_status.message_id, queue_name, msg_status.attempts
                    );
                    messages_to_process.push((entry.key().clone(), queue_name.clone(), msg_status.clone()));
                }
            }

            for (message_id, queue_name, message_status) in messages_to_process {
                if let Some((_id, (qn, ms))) = self.unacknowledged_messages.remove(&message_id) {
                    if qn == queue_name && ms.message_id == message_status.message_id { 
                        self.re_enqueue_message(queue_name, message_status).await;
                    } else {
                        let qn_for_log = qn.clone(); 
                        self.unacknowledged_messages.insert(message_id, (qn,ms));
                        info!("ServerState: Message {} in queue {} was modified or handled before retry could occur with specific state. Re-inserted removed value.", message_id, qn_for_log);
                    }
                } else {
                    info!("ServerState: Message {} was already acknowledged or processed before retry could occur.", message_id);
                }
            }
        }
    }
}
