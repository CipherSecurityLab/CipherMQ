use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};
use crate::storage::{PublicKeyData, Storage};

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
    pub bindings: DashMap<String, Vec<String>>,
    pub exchanges: DashMap<String, Vec<String>>,
    pub consumers: DashMap<String, Vec<mpsc::UnboundedSender<(String, EncryptedInputData)>>>,
    pub message_status: DashMap<String, MessageStatus>,
    pub request_times: DashMap<String, Vec<f64>>,
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
            request_times: DashMap::new(),
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

    pub fn register_consumer(&self, queue_name: &str, sender: mpsc::UnboundedSender<(String, EncryptedInputData)>) {
        if self.queues.contains_key(queue_name) {
            let mut consumers = self.consumers.entry(queue_name.to_string()).or_insert_with(Vec::new);
            consumers.push(sender.clone());
            info!("Consumer registered for queue '{}'", queue_name);
            // Send any existing messages in the queue to the new consumer
            if let Some(queue) = self.queues.get(queue_name) {
                for (message_id, message) in queue.iter() {
                    if !self.message_status.contains_key(message_id) {
                        continue; // Skip if message is already acknowledged
                    }
                    if sender.send((message_id.clone(), message.clone())).is_err() {
                        warn!("Failed to send message {} to new consumer for queue '{}'", message_id, queue_name);
                    } else {
                        if let Some(mut status) = self.message_status.get_mut(message_id) {
                            status.delivered(Instant::now());
                        }
                    }
                }
            }
        } else {
            warn!("Cannot register consumer: Queue '{}' does not exist", queue_name);
        }
    }

    pub async fn publish(
        &self,
        exchange_name: &str,
        routing_key: &str,
        message: EncryptedInputData,
        _client_id: String,
    ) -> Result<(), String> {
        let message_id = message.message_id.clone();
        if self.message_status.contains_key(&message_id) {
            warn!("Duplicate message {} ignored", message_id);
            return Err(format!("Duplicate message {} ignored", message_id));
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
                .insert(message_id.clone(), MessageStatus::sent(Instant::now()));
            info!(
                "Published message {} to queue '{}' via exchange '{}'",
                message_id, routing_key, exchange_name
            );
            Ok(())
        } else {
            Err(format!("Exchange '{}' not found", exchange_name))
        }
    }

    pub async fn consume(&self, queue_name: &str) -> Option<(String, EncryptedInputData)> {
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

    pub async fn acknowledge(&self, message_id: &str) {
        if !self.message_status.contains_key(message_id) {
            warn!("Ignoring duplicate ACK for message {}", message_id);
            return;
        }
        if let Some(mut status) = self.message_status.get_mut(message_id) {
            status.acknowledged(Instant::now());
        }
        for mut queue in self.queues.iter_mut() {
            queue.retain(|(id, _)| id != message_id);
        }
        for mut consumers in self.consumers.iter_mut() {
            consumers.retain(|sender| !sender.is_closed());
        }
        self.message_status.remove(message_id);
        info!("Message {} acknowledged by client, removed from queues and status", message_id);
    }

    pub fn reset_stats(&self) {
        self.message_status.clear();
        self.request_times.clear();
        info!("Server stats reset");
    }

    pub fn record_request_time(&self, command: &str, duration: f64) {
        self.request_times
            .entry(command.to_string())
            .or_insert_with(Vec::new)
            .push(duration);
        info!("Recorded request time for '{}': {}s", command, duration);
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
        self.storage
            .save_public_key(&key_data)
            .await
            .map_err(|e| format!("Failed to save public key: {}", e))?;
        info!("Public key saved for client {}", client_id);
        Ok(())
    }

    pub async fn get_public_key(&self, client_id: &str) -> Result<Option<String>, String> {
        self.storage
            .get_public_key(client_id)
            .await
            .map_err(|e| format!("Failed to get public key: {}", e))
    }

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

        let mut publish_to_ack_times: Vec<f64> = Vec::new();
        let mut delivery_to_ack_times: Vec<f64> = Vec::new();
        for entry in self.message_status.iter() {
            let status = entry.value();
            if let (Some(sent_time), Some(ack_time)) = (status.sent_time, status.acknowledged_time) {
                publish_to_ack_times.push(ack_time.duration_since(sent_time).as_secs_f64());
            }
            if let (Some(delivered_time), Some(ack_time)) = (status.delivered_time, status.acknowledged_time) {
                delivery_to_ack_times.push(ack_time.duration_since(delivered_time).as_secs_f64());
            }
        }
        let avg_publish_to_ack = if !publish_to_ack_times.is_empty() {
            publish_to_ack_times.iter().sum::<f64>() / publish_to_ack_times.len() as f64
        } else {
            0.0
        };
        let avg_delivery_to_ack = if !delivery_to_ack_times.is_empty() {
            delivery_to_ack_times.iter().sum::<f64>() / delivery_to_ack_times.len() as f64
        } else {
            0.0
        };

        let request_time_stats: Vec<(String, f64, usize, f64)> = self
            .request_times
            .iter()
            .map(|entry| {
                let times = entry.value();
                let count = times.len();
                let sum_time = times.iter().sum::<f64>();
                let avg_time = if count > 0 { sum_time / count as f64 } else { 0.0 };
                (entry.key().clone(), avg_time, count, sum_time)
            })
            .collect();

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
            "request_times_avg_seconds": request_time_stats,
            "avg_publish_to_ack_seconds": avg_publish_to_ack,
            "avg_delivery_to_ack_seconds": avg_delivery_to_ack
        })
    }
}