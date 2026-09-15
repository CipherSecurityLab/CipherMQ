use dashmap::{DashMap, DashSet};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time;
use tracing::{error, info, warn, debug};
use crate::Storage;
use crate::storage::{MessageMetadata, PublicKeyData};

#[derive(Clone, Serialize, Deserialize)]
pub struct EncryptedInputData {
    pub message_id: String,
    pub receiver_client_id: String,
    pub enc_session_key: String,
    pub nonce: String,
    pub ciphertext: String,
}

#[derive(Clone)]
pub struct MessageStatus {
    pub sent_time: Option<Instant>,
    pub delivered_time: Option<Instant>,
    pub acknowledged_time: Option<Instant>,
}

impl MessageStatus {
    pub fn sent(time: Instant) -> Self {
        MessageStatus {
            sent_time: Some(time),
            delivered_time: None,
            acknowledged_time: None,
        }
    }

    pub fn delivered(&mut self, time: Instant) {
        self.delivered_time = Some(time);
    }

    pub fn acknowledged(&mut self, time: Instant) {
        self.acknowledged_time = Some(time);
    }
    
    pub fn can_deliver(&self) -> bool {
        self.acknowledged_time.is_none()
    }
}

/// ✅ IMPROVED: Lock-free ServerState using only DashMap (interior mutability)
#[derive(Clone)]
pub struct ServerState {
    pub queues: DashMap<String, VecDeque<(String, EncryptedInputData)>>,
    pub message_queues: DashMap<String, DashSet<String>>,
    pub bindings: DashMap<String, Vec<(String, String)>>,
    pub exchanges: DashMap<String, Vec<String>>,
    pub consumers: DashMap<String, Vec<mpsc::UnboundedSender<(String, EncryptedInputData)>>>,
    pub message_status: DashMap<String, MessageStatus>,
    // ✅ Changed from Arc<RwLock<usize>> to AtomicUsize (lock-free!)
    pub connected_clients: Arc<AtomicUsize>,
    pub storage: Arc<Storage>,
    pub max_queue_size: usize,
}

impl ServerState {
    pub fn new(storage: Arc<Storage>, max_queue_size: usize) -> Self {
        let state = ServerState {
            queues: DashMap::new(),
            message_queues: DashMap::new(),
            bindings: DashMap::new(),
            exchanges: DashMap::new(),
            consumers: DashMap::new(),
            message_status: DashMap::new(),
            // ✅ No more RwLock!
            connected_clients: Arc::new(AtomicUsize::new(0)),
            storage,
            max_queue_size,
        };
        
        // Task 1: Cleanup old messages
        let state_clone = state.clone();
        let max_queue_size = max_queue_size.max(10000);
        tokio::spawn(async move {
            state_clone.cleanup_old_messages(Duration::from_secs(3000)).await;
        });
        
        // Task 2: Cleanup dead consumers
        let state_clone = state.clone();
        tokio::spawn(async move {
            state_clone.cleanup_dead_consumers().await;
        });
        
        state
    }

    async fn cleanup_dead_consumers(&self) {
        loop {
            time::sleep(Duration::from_secs(60)).await;
            
            let mut total_removed = 0;
            let mut total_consumers = 0;
            let mut empty_queues = Vec::new();
            
            for mut entry in self.consumers.iter_mut() {
                let queue_name = entry.key().clone();
                let consumers = entry.value_mut();
                
                let initial_count = consumers.len();
                total_consumers += initial_count;
                
                // Remove inactive consumers.
                consumers.retain(|sender| {
                    let is_open = !sender.is_closed();
                    if !is_open {
                        debug!("Removing closed consumer from queue '{}'", queue_name);
                    }
                    is_open
                });
                
                let removed = initial_count - consumers.len();
                total_removed += removed;

                // ✅ Mark only; do not remove yet.
                if consumers.is_empty() {
                    empty_queues.push(queue_name.clone());
                }
                
                if removed > 0 {
                    info!(
                        "Cleaned up {} dead consumer(s) from queue '{}', remaining: {}",
                        removed, queue_name, consumers.len()
                    );
                }
            }
            
            // ✅ Remove empty queues after iteration.
            for queue_name in empty_queues {
                self.consumers.remove(&queue_name);
                info!("Removed empty consumer list for queue '{}'", queue_name);
            }
            
            if total_removed > 0 {
                info!(
                    "Consumer cleanup completed: removed {}/{} dead consumers",
                    total_removed, total_consumers
                );
            } else {
                debug!("Consumer cleanup");
            }
        }
    }

    pub async fn cleanup_old_messages(&self, max_age: Duration) {
        loop {
            time::sleep(Duration::from_secs(60)).await;
            let now = Instant::now();
            let mut expired_messages = Vec::new();
            
            for entry in self.message_status.iter() {
                if let Some(delivered_time) = entry.value().delivered_time {
                    if now.duration_since(delivered_time) > max_age && entry.value().acknowledged_time.is_none() {
                        expired_messages.push(entry.key().clone());
                    }
                }
            }
            
            for message_id in expired_messages {
                if let Some(queue_set) = self.message_queues.get(&message_id) {
                    for queue_name in queue_set.iter() {
                        if let Some(mut queue) = self.queues.get_mut(&*queue_name) {
                            queue.retain(|(id, _)| id != &message_id);
                        }
                    }
                }
                self.message_queues.remove(&message_id);
                self.message_status.remove(&message_id);
                info!("Removed expired message {} from queues and status", message_id);
            }
        }
    }

    pub fn declare_queue(&self, queue_name: &str) {
        self.queues.entry(queue_name.to_string()).or_insert_with(VecDeque::new);
        info!("Queue '{}' declared", queue_name);
    }

    pub fn declare_exchange(&self, exchange_name: &str) {
        self.exchanges
            .entry(exchange_name.to_string())
            .or_insert_with(Vec::new);
        info!("Exchange '{}' declared", exchange_name);
    }

    pub fn bind_queue(&self, queue_name: &str, exchange_name: &str, routing_key: &str) {
        if self.queues.contains_key(queue_name) && self.exchanges.contains_key(exchange_name) {
            let binding = (queue_name.to_string(), routing_key.to_string());
            
            self.bindings
                .entry(exchange_name.to_string())
                .and_modify(|bindings| {
                    // ✅ Check if binding already exists
                    if !bindings.contains(&binding) {
                        bindings.push(binding.clone());
                        info!(
                            "Queue '{}' bound to exchange '{}' with routing key '{}'",
                            queue_name, exchange_name, routing_key
                        );
                    } else {
                        debug!(
                            "Binding already exists: Queue '{}' to exchange '{}' with routing key '{}'",
                            queue_name, exchange_name, routing_key
                        );
                    }
                })
                .or_insert_with(|| {
                    info!(
                        "Queue '{}' bound to exchange '{}' with routing key '{}'",
                        queue_name, exchange_name, routing_key
                    );
                    vec![binding]
                });
        } else {
            warn!(
                "Binding failed: Queue '{}' or exchange '{}' does not exist",
                queue_name, exchange_name
            );
        }
    }

    /// ✅ No lock held during I/O operations
    /// ✅ No duplicates, optimal performance
    pub async fn publish(
        &self,
        exchange_name: &str,
        routing_key: &str,
        message: EncryptedInputData,
        client_id: String,
        received_time: Instant,
    ) -> Result<(), String> {
        let message_id = message.message_id.clone();
        
        // ✅ Quick duplicate check
        if self.message_status.contains_key(&message_id) {
            warn!("Duplicate message {} ignored", message_id);
            return Err(format!("Duplicate message {} ignored", message_id));
        }

        // ✅ Prepare metadata
        let metadata = MessageMetadata {
            message_id: message_id.clone(),
            client_id,
            exchange_name: exchange_name.to_string(),
            routing_key: routing_key.to_string(),
            sent_time: Some(chrono::Utc::now().to_rfc3339()),
            delivered_time: None,
            acknowledged_time: None,
        };
        
        // ✅ Database I/O (async, no state locks)
        if let Err(e) = self.storage.save_metadata(&metadata).await {
            error!("Failed to save metadata for message {}: {}", message_id, e);
            return Err(format!("Failed to save metadata: {}", e));
        }

        // ✅ Find target queues
        let target_queues: Vec<String> = if let Some(bindings) = self.bindings.get(exchange_name) {
            bindings
                .iter()
                .filter(|(_, bound_routing_key)| *bound_routing_key == routing_key)
                .map(|(queue_name, _)| queue_name.clone())
                .collect()
        } else {
            Vec::new()
        };

        if target_queues.is_empty() {
            return Err(format!(
                "No queues bound to exchange '{}' with routing key '{}'",
                exchange_name, routing_key
            ));
        }

        let queue_set = DashSet::new();
        
        // ✅ Add to queues ONCE per queue
        for queue_name in &target_queues {
            if let Some(mut queue) = self.queues.get_mut(queue_name) {
                // Check queue size limit
                if queue.len() >= self.max_queue_size {
                    warn!("Queue '{}' exceeded max size ({}), removing oldest message", 
                        queue_name, self.max_queue_size);
                    if let Some((old_message_id, _)) = queue.pop_front() {
                        if let Some(q_set) = self.message_queues.get(&old_message_id) {
                            q_set.remove(queue_name);
                        }
                        self.message_status.remove(&old_message_id);
                        debug!("Removed oldest message {} from queue {}", old_message_id, queue_name);
                    }
                }
                
                // ✅ CRITICAL: Add message EXACTLY ONCE
                queue.push_back((message_id.clone(), message.clone()));
                queue_set.insert(queue_name.clone());
                debug!("Added message {} to queue {}, current size: {}", 
                    message_id, queue_name, queue.len());
            }
            // Lock released here
        }

        // ✅ Update tracking structures
        self.message_queues.insert(message_id.clone(), queue_set);
        self.message_status.insert(message_id.clone(), MessageStatus::sent(received_time));

        // ✅ Notify consumers (separate phase, after all queues updated)
        for queue_name in &target_queues {
            if let Some(mut consumer_senders) = self.consumers.get_mut(queue_name) {
                let initial_count = consumer_senders.len();
                
                // ✅ Send notification ONCE per consumer
                consumer_senders.retain(|sender| {
                    if sender.is_closed() {
                        debug!("Removing closed consumer during publish to queue '{}'", queue_name);
                        false
                    } else {
                        // ✅ Send returns Result - check it
                        match sender.send((message_id.clone(), message.clone())) {
                            Ok(_) => true,  // Keep consumer
                            Err(_) => {
                                warn!("Failed to send message to consumer for queue '{}', removing", queue_name);
                                false  // Remove consumer
                            }
                        }
                    }
                });
                
                let removed = initial_count - consumer_senders.len();
                if removed > 0 {
                    debug!("Removed {} dead consumer(s) from queue '{}'", removed, queue_name);
                }
            }
            // Lock released here
        }

        info!(
            "Published message {} to exchange '{}' with routing key '{}'",
            message_id, exchange_name, routing_key
        );
        Ok(())
    }

    // ✅ FIXED: deliver queued messages to the new consumer.
    pub fn register_consumer(
        &self,
        queue_name: &str,
        sender: mpsc::UnboundedSender<(String, EncryptedInputData)>,
    ) {
        self.declare_queue(queue_name);
        
        // ✅ STEP 1: register the consumer.
        self.consumers
            .entry(queue_name.to_string())
            .or_insert_with(Vec::new)
            .push(sender.clone());
        
        info!("Consumer registered for queue '{}'", queue_name);
        
        // ✅ STEP 2: deliver queued messages with a short delay.
        if let Some(queue) = self.queues.get(queue_name) {
            let pending_count = queue.len();
            if pending_count > 0 {
                info!(
                    "Found {} pending messages in queue '{}', will deliver to new consumer",
                    pending_count, queue_name
                );
                
                // ✅ Clone messages first to avoid holding lock
                let pending_messages: Vec<(String, EncryptedInputData)> = queue
                    .iter()
                    .filter(|(message_id, _)| {
                        self.message_status
                            .get(message_id)
                            .map(|status| status.can_deliver())
                            .unwrap_or(false)
                    })
                    .map(|(id, msg)| (id.clone(), msg.clone()))
                    .collect();
                
                drop(queue); // Release lock
                
                // ✅ Send messages with small delay to avoid race condition
                let sender_clone = sender.clone();
                tokio::spawn(async move {
                    // Wait 100ms to let consume response be sent first
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    
                    for (message_id, message) in pending_messages {
                        match sender_clone.send((message_id.clone(), message)) {
                            Ok(_) => {
                                debug!("Sent pending message {} to new consumer", message_id);
                                // Small delay between messages
                                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                            }
                            Err(e) => {
                                warn!("Failed to send pending message {} to new consumer: {}", 
                                    message_id, e);
                                break;
                            }
                        }
                    }
                });
                
                info!(
                    "Scheduled delivery of {} pending messages to new consumer",
                    pending_count
                );
            }
        }
        
        if let Some(consumers) = self.consumers.get(queue_name) {
            debug!("Queue '{}' now has {} consumer(s)", queue_name, consumers.len());
        }
    }

    /// ✅ OPTIMIZED: No lock held during I/O
    pub async fn consume(&self, queue_name: &str) -> Option<(String, EncryptedInputData)> {
        // ✅ Pop from queue (lock released immediately)
        let message = if let Some(mut queue) = self.queues.get_mut(queue_name) {
            queue.pop_front()
        } else {
            None
        };
        
        if let Some((message_id, msg)) = message {
            // ✅ Update status (separate lock)
            if let Some(mut status) = self.message_status.get_mut(&message_id) {
                status.delivered(Instant::now());
            }
            
            // ✅ OPTIMIZED: Fire-and-forget async update (no blocking)
            let delivered_time = chrono::Utc::now().to_rfc3339();
            self.storage.update_delivered_time_async(&message_id, &delivered_time).await;
            
            return Some((message_id, msg));
        }
        
        None
    }

    /// ✅ OPTIMIZED: No lock held during I/O
    pub async fn acknowledge(&self, message_id: &str) {
        debug!("Received ACK request for message {}", message_id);
        
        // ✅ Quick check (no lock held)
        if !self.message_status.contains_key(message_id) {
            warn!("Ignoring ACK for non-existent or already acknowledged message {}", message_id);
            return;
        }

        debug!("Processing ACK for message {}", message_id);
        
        // ✅ Update status (separate lock)
        if let Some(mut status) = self.message_status.get_mut(message_id) {
            status.acknowledged(Instant::now());
        }
        
        // ✅ OPTIMIZED: Fire-and-forget async update (no blocking)
        let ack_time = chrono::Utc::now().to_rfc3339();
        self.storage.update_acknowledged_time_async(&message_id, &ack_time).await;
        info!("ACK confirmed for message {}", message_id);

        // ✅ Cleanup (each operation has its own lock)
        if let Some(queue_set) = self.message_queues.get(message_id) {
            for queue_name in queue_set.iter() {
                if let Some(mut queue) = self.queues.get_mut(&*queue_name) {
                    queue.retain(|(id, _)| id != message_id);
                    debug!("Removed message {} from queue {}", message_id, queue_name.to_string());
                }
            }
        }
        
        self.message_queues.remove(message_id);
        self.message_status.remove(message_id);
        debug!("Message {} acknowledged and removed from queues and status", message_id);
    }

    /// ✅ IMPROVED: Lock-free atomic operations
    pub fn increment_client_count(&self) {
        let previous = self.connected_clients.fetch_add(1, Ordering::SeqCst);
        debug!("Client connected, total: {}", previous + 1);
    }

    pub fn decrement_client_count(&self) {
        let previous = self.connected_clients.fetch_sub(1, Ordering::SeqCst);
        debug!("Client disconnected, total: {}", previous.saturating_sub(1));
    }
    
    pub fn get_client_count(&self) -> usize {
        self.connected_clients.load(Ordering::SeqCst)
    }

    /// ✅ FIXED: No lock held during I/O
    pub async fn save_public_key(&self, client_id: &str, public_key: &str) -> Result<(), String> {
        let key_data = PublicKeyData {
            client_id: client_id.to_string(),
            public_key: public_key.to_string(),
        };
        
        // ✅ Direct I/O call, no state locks involved
        self.storage.save_public_key(&key_data).await
            .map_err(|e| format!("Failed to save public key: {}", e))?;
        info!("Public key saved for client {}", client_id);
        Ok(())
    }

    /// ✅ FIXED: No lock held during I/O
    pub async fn get_public_key(&self, client_id: &str) -> Result<Option<String>, String> {
        // ✅ Direct I/O call, no state locks involved
        self.storage.get_public_key(client_id).await
            .map_err(|e| format!("Failed to get public key: {}", e))
    }
    
    pub fn cleanup_consumers_for_queue(&self, queue_name: &str) -> usize {
        if let Some(mut consumers) = self.consumers.get_mut(queue_name) {
            let initial_count = consumers.len();
            consumers.retain(|sender| !sender.is_closed());
            let removed = initial_count - consumers.len();
            if removed > 0 {
                info!("Manually cleaned up {} consumer(s) from queue '{}'", removed, queue_name);
            }
            removed
        } else {
            0
        }
    }
    
    pub fn get_consumer_stats(&self) -> Vec<(String, usize, usize)> {
        self.consumers.iter().map(|entry| {
            let queue_name = entry.key().clone();
            let total = entry.value().len();
            let alive = entry.value().iter().filter(|s| !s.is_closed()).count();
            (queue_name, total, alive)
        }).collect()
    }
}