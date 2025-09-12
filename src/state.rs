use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};
use crate::Storage;
use crate::storage::{MessageMetadata, PublicKeyData};

#[derive(Clone, Serialize, Deserialize)]
pub struct EncryptedInputData {
    pub message_id: String,
    pub receiver_client_id: String,
    pub enc_session_key: String,
    pub nonce: String,
    pub tag: String,
    pub ciphertext: String,
}

#[derive(Clone)]
pub struct MessageStatus {
    sent_time: Option<Instant>,
    delivered_time: Option<Instant>,
    acknowledged_time: Option<Instant>,
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
}

pub struct ServerState {
    pub queues: DashMap<String, Vec<(String, EncryptedInputData)>>,
    pub bindings: DashMap<String, Vec<(String, String)>>, // تغییر به ذخیره (queue_name, routing_key)
    pub exchanges: DashMap<String, Vec<String>>,
    pub consumers: DashMap<String, Vec<mpsc::UnboundedSender<(String, EncryptedInputData)>>>,
    pub message_status: DashMap<String, MessageStatus>,
    pub connected_clients: Arc<RwLock<usize>>,
    pub storage: Arc<Storage>,
}

impl ServerState {
    pub fn new(storage: Arc<Storage>) -> Self {
        ServerState {
            queues: DashMap::new(),
            bindings: DashMap::new(),
            exchanges: DashMap::new(),
            consumers: DashMap::new(),
            message_status: DashMap::new(),
            connected_clients: Arc::new(RwLock::new(0)),
            storage,
        }
    }

    pub fn declare_queue(&self, queue_name: &str) {
        self.queues.entry(queue_name.to_string()).or_insert_with(Vec::new);
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
            self.bindings
                .entry(exchange_name.to_string())
                .or_insert_with(Vec::new)
                .push((queue_name.to_string(), routing_key.to_string()));
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

    pub async fn publish(
        &self,
        exchange_name: &str,
        routing_key: &str,
        message: EncryptedInputData,
        client_id: String,
        received_time: Instant,
    ) -> Result<(), String> {
        let message_id = message.message_id.clone();
        if self.message_status.contains_key(&message_id) {
            warn!("Duplicate message {} ignored", message_id);
            return Err(format!("Duplicate message {} ignored", message_id));
        }

        // ثبت متادیتا در دیتابیس
        let metadata = MessageMetadata {
            message_id: message_id.clone(),
            client_id,
            exchange_name: exchange_name.to_string(),
            routing_key: routing_key.to_string(),
            sent_time: Some(chrono::Utc::now().to_rfc3339()),
            delivered_time: None,
            acknowledged_time: None,
        };
        if let Err(e) = self.storage.save_metadata(&metadata).await {
            error!("Failed to save metadata for message {}: {}", message_id, e);
            return Err(format!("Failed to save metadata: {}", e));
        }

        // انتشار پیام فقط به صف‌هایی که routing_key مطابقت دارد
        let mut target_queues = Vec::new();
        if let Some(bindings) = self.bindings.get(exchange_name) {
            for (queue_name, bound_routing_key) in bindings.iter() {
                if bound_routing_key == routing_key {
                    target_queues.push(queue_name.clone());
                }
            }
        }

        if target_queues.is_empty() {
            return Err(format!("No queues bound to exchange '{}' with routing key '{}'", exchange_name, routing_key));
        }

        for queue_name in target_queues {
            if let Some(mut queue) = self.queues.get_mut(&queue_name) {
                queue.push((message_id.clone(), message.clone()));
                if let Some(mut consumer_senders) = self.consumers.get_mut(&queue_name) {
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

        // ثبت زمان ارسال در MessageStatus
        self.message_status
            .insert(message_id.clone(), MessageStatus::sent(received_time));

        info!(
            "Published message {} to exchange '{}' with routing key '{}'",
            message_id, exchange_name, routing_key
        );
        Ok(())
    }

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

    pub async fn consume(&self, queue_name: &str) -> Option<(String, EncryptedInputData)> {
        if let Some(mut queue) = self.queues.get_mut(queue_name) {
            if !queue.is_empty() {
                let (message_id, message) = queue.remove(0);
                if let Some(mut status) = self.message_status.get_mut(&message_id) {
                    status.delivered(Instant::now());
                    let delivered_time = chrono::Utc::now().to_rfc3339();
                    if let Err(e) = self.storage.update_delivered_time(&message_id, &delivered_time).await {
                        error!("Failed to update delivered time for message {}: {}", message_id, e);
                    }
                }
                return Some((message_id, message));
            }
        }
        None
    }

    pub async fn acknowledge(&self, message_id: &str) {
        if !self.message_status.contains_key(message_id) {
            warn!("Ignoring duplicate ACK for message {}", message_id);
            return;
        }
        if let Some(mut status) = self.message_status.get_mut(message_id) {
            status.acknowledged(Instant::now());
            let ack_time = chrono::Utc::now().to_rfc3339();
            if let Err(e) = self.storage.update_acknowledged_time(&message_id, &ack_time).await {
                error!("Failed to update acknowledged time for message {}: {}", message_id, e);
            }
        }
        for mut queue in self.queues.iter_mut() {
            queue.retain(|(id, _)| id != message_id);
        }
        self.message_status.remove(message_id);
        info!("Message {} acknowledged by client, removed from queues and status", message_id);
    }

    pub async fn increment_client_count(&self) {
        let mut count = self.connected_clients.write().await;
        *count += 1;
    }

    pub async fn decrement_client_count(&self) {
        let mut count = self.connected_clients.write().await;
        *count -= 1;
    }

    pub async fn save_public_key(&self, client_id: &str, public_key: &str) -> Result<(), String> {
        let key_data = PublicKeyData {
            client_id: client_id.to_string(),
            public_key: public_key.to_string(),
        };
        self.storage.save_public_key(&key_data).await
            .map_err(|e| format!("Failed to save public key: {}", e))?;
        info!("Public key saved for client {}", client_id);
        Ok(())
    }

    pub async fn get_public_key(&self, client_id: &str) -> Result<Option<String>, String> {
        self.storage.get_public_key(client_id).await
            .map_err(|e| format!("Failed to get public key: {}", e))
    }
}