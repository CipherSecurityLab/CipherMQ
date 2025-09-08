use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, Sender};
use tracing::{error, info, debug};
use tokio_postgres::{NoTls, Error as PostgresError};
use std::sync::Arc;
use thiserror::Error;
use aes_gcm::{Aes256Gcm, Key, Nonce};
use aes_gcm::aead::Aead;
use aes_gcm::KeyInit;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rand::Rng;
use crate::config::Config;

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("PostgreSQL error: {0}")]
    Postgres(#[from] PostgresError),
    #[error("Channel send error: {0}")]
    ChannelSend(String),
    #[error("Channel receive error")]
    ChannelReceive,
    #[error("Encryption error: {0}")]
    EncryptionError(String),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MessageMetadata {
    pub message_id: String,
    pub client_id: String,
    pub exchange_name: String,
    pub routing_key: String,
    pub sent_time: Option<String>,
    pub delivered_time: Option<String>,
    pub acknowledged_time: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PublicKeyData {
    pub client_id: String,
    pub public_key: String,
}

pub struct Storage {
    sender: Sender<StorageCommand>,
}

enum StorageCommand {
    SaveMetadata(MessageMetadata, Sender<Result<(), StorageError>>),
    UpdateDeliveredTime(String, String, Sender<Result<(), StorageError>>),
    UpdateAcknowledgedTime(String, String, Sender<Result<(), StorageError>>),
    LoadUnacknowledgedMetadata(Sender<Result<Vec<MessageMetadata>, StorageError>>),
    GetClientIdForMessage(String, Sender<Result<Option<String>, StorageError>>),
    SavePublicKey(PublicKeyData, Sender<Result<(), StorageError>>),
    GetPublicKey(String, Sender<Result<Option<String>, StorageError>>),
}

impl Storage {
    pub async fn new(config: &crate::config::DatabaseConfig) -> Result<Self, StorageError> {
        info!("Attempting to connect to PostgreSQL database: {}", config.dbname);
        
        let conn_str = format!(
            "host={} port={} user={} password={} dbname={}",
            config.host, config.port, config.user, config.password, config.dbname
        );

        let (client, connection) = tokio_postgres::connect(&conn_str, NoTls).await
            .map_err(StorageError::Postgres)?;
        
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                error!("PostgreSQL connection error: {}", e);
            }
        });

        info!("Creating message_metadata table if not exists");
        client.execute(
            "CREATE TABLE IF NOT EXISTS message_metadata (
                message_id TEXT PRIMARY KEY,
                client_id TEXT,
                exchange_name TEXT,
                routing_key TEXT,
                sent_time TEXT,
                delivered_time TEXT,
                acknowledged_time TEXT
            )",
            &[],
        ).await.map_err(StorageError::Postgres)?;

        info!("Creating public_keys table if not exists");
        client.execute(
            "CREATE TABLE IF NOT EXISTS public_keys (
                client_id TEXT PRIMARY KEY,
                public_key_ciphertext TEXT NOT NULL,
                nonce TEXT NOT NULL,
                tag TEXT NOT NULL
            )",
            &[],
        ).await.map_err(StorageError::Postgres)?;
        info!("Table creation completed");

        let client = Arc::new(client);
        let (sender, mut receiver) = mpsc::channel::<StorageCommand>(100);
        info!("Database worker task started with channel capacity 100");

        let client_clone = client.clone();
        tokio::spawn(async move {
            let config = Config::load("config.toml").expect("Failed to load config");
            let aes_key = BASE64.decode(&config.encryption.aes_key)
                .expect("Invalid AES key");
            let key = Key::<Aes256Gcm>::from_slice(&aes_key);
            let cipher = Aes256Gcm::new(key);

            loop {
                debug!("Waiting for database command");
                match receiver.recv().await {
                    Some(command) => {
                        match command {
                            StorageCommand::SaveMetadata(metadata, reply) => {
                                debug!("Received SaveMetadata command for message {}", metadata.message_id);
                                let result = (|| async {
                                    debug!("Executing SaveMetadata for message {}", metadata.message_id);
                                    client_clone.execute(
                                        "INSERT INTO message_metadata (
                                            message_id, client_id, exchange_name, routing_key,
                                            sent_time, delivered_time, acknowledged_time
                                        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
                                        ON CONFLICT (message_id) DO UPDATE SET
                                            client_id = EXCLUDED.client_id,
                                            exchange_name = EXCLUDED.exchange_name,
                                            routing_key = EXCLUDED.routing_key,
                                            sent_time = EXCLUDED.sent_time,
                                            delivered_time = EXCLUDED.delivered_time,
                                            acknowledged_time = EXCLUDED.acknowledged_time",
                                        &[
                                            &metadata.message_id,
                                            &metadata.client_id,
                                            &metadata.exchange_name,
                                            &metadata.routing_key,
                                            &metadata.sent_time,
                                            &metadata.delivered_time,
                                            &metadata.acknowledged_time,
                                        ],
                                    ).await?;
                                    Ok(())
                                })().await.map_err(StorageError::Postgres);
                                if let Err(e) = &result {
                                    error!("Failed to save metadata for message {}: {}", metadata.message_id, e);
                                }
                                debug!("Sending SaveMetadata response for message {}", metadata.message_id);
                                if let Err(e) = reply.send(result).await {
                                    error!("Failed to send SaveMetadata response for message {}: {:?}", metadata.message_id, e);
                                } else {
                                    debug!("Successfully sent SaveMetadata response for message {}", metadata.message_id);
                                }
                            }
                            StorageCommand::UpdateDeliveredTime(message_id, time, reply) => {
                                debug!("Received UpdateDeliveredTime command for message {}", message_id);
                                let result = (|| async {
                                    debug!("Executing UpdateDeliveredTime for message {}", message_id);
                                    client_clone.execute(
                                        "UPDATE message_metadata SET delivered_time = $1 WHERE message_id = $2",
                                        &[&time, &message_id],
                                    ).await?;
                                    Ok(())
                                })().await.map_err(StorageError::Postgres);
                                if let Err(e) = &result {
                                    error!("Failed to update delivered time for message {}: {}", message_id, e);
                                }
                                debug!("Sending UpdateDeliveredTime response for message {}", message_id);
                                if let Err(e) = reply.send(result).await {
                                    error!("Failed to send UpdateDeliveredTime response for message {}: {:?}", message_id, e);
                                } else {
                                    debug!("Successfully sent UpdateDeliveredTime response for message {}", message_id);
                                }
                            }
                            StorageCommand::UpdateAcknowledgedTime(message_id, time, reply) => {
                                debug!("Received UpdateAcknowledgedTime command for message {}", message_id);
                                let result = (|| async {
                                    debug!("Executing UpdateAcknowledgedTime for message {}", message_id);
                                    client_clone.execute(
                                        "UPDATE message_metadata SET acknowledged_time = $1 WHERE message_id = $2",
                                        &[&time, &message_id],
                                    ).await?;
                                    Ok(())
                                })().await.map_err(StorageError::Postgres);
                                if let Err(e) = &result {
                                    error!("Failed to update acknowledged time for message {}: {}", message_id, e);
                                }
                                debug!("Sending UpdateAcknowledgedTime response for message {}", message_id);
                                if let Err(e) = reply.send(result).await {
                                    error!("Failed to send UpdateAcknowledgedTime response for message {}: {:?}", message_id, e);
                                } else {
                                    debug!("Successfully sent UpdateAcknowledgedTime response for message {}", message_id);
                                }
                            }
                            StorageCommand::LoadUnacknowledgedMetadata(reply) => {
                                debug!("Received LoadUnacknowledgedMetadata command");
                                let result = (|| async {
                                    debug!("Querying unacknowledged metadata");
                                    let rows = client_clone.query(
                                        "SELECT message_id, client_id, exchange_name, routing_key,
                                                sent_time, delivered_time, acknowledged_time
                                         FROM message_metadata WHERE acknowledged_time IS NULL",
                                        &[],
                                    ).await?;
                                    let metadata = rows.into_iter().map(|row| MessageMetadata {
                                        message_id: row.get(0),
                                        client_id: row.get(1),
                                        exchange_name: row.get(2),
                                        routing_key: row.get(3),
                                        sent_time: row.get(4),
                                        delivered_time: row.get(5),
                                        acknowledged_time: row.get(6),
                                    }).collect::<Vec<_>>();
                                    debug!("Loaded {} unacknowledged metadata records", metadata.len());
                                    Ok(metadata)
                                })().await.map_err(StorageError::Postgres);
                                if let Err(e) = &result {
                                    error!("Failed to load unacknowledged metadata: {}", e);
                                }
                                debug!("Sending LoadUnacknowledgedMetadata response");
                                if let Err(e) = reply.send(result).await {
                                    error!("Failed to send LoadUnacknowledgedMetadata response: {:?}", e);
                                } else {
                                    debug!("Successfully sent LoadUnacknowledgedMetadata response");
                                }
                            }
                            StorageCommand::GetClientIdForMessage(message_id, reply) => {
                                debug!("Received GetClientIdForMessage command for message {}", message_id);
                                let result = (|| async {
                                    debug!("Querying client_id for message {}", message_id);
                                    let row = client_clone.query_opt(
                                        "SELECT client_id FROM message_metadata WHERE message_id = $1",
                                        &[&message_id],
                                    ).await?;
                                    let client_id = row.map(|r| r.get(0));
                                    debug!("Client ID query result for message {}: {:?}", message_id, client_id);
                                    Ok(client_id)
                                })().await.map_err(StorageError::Postgres);
                                if let Err(e) = &result {
                                    error!("Failed to query client_id for message {}: {}", message_id, e);
                                }
                                debug!("Sending GetClientIdForMessage response for message {}", message_id);
                                if let Err(e) = reply.send(result).await {
                                    error!("Failed to send GetClientIdForMessage response for message {}: {:?}", message_id, e);
                                } else {
                                    debug!("Successfully sent GetClientIdForMessage response for message {}", message_id);
                                }
                            }
                            StorageCommand::SavePublicKey(key_data, reply) => {
                                debug!("Received SavePublicKey command for client {}", key_data.client_id);
                                let result = (|| async {
                                    let nonce_bytes: [u8; 12] = rand::thread_rng().gen();
                                    let nonce = Nonce::from_slice(&nonce_bytes);
                                    let public_key_bytes = BASE64.decode(&key_data.public_key)
                                        .map_err(|e| StorageError::EncryptionError(format!("Invalid public key: {}", e)))?;
                                    let ciphertext = cipher.encrypt(nonce, &public_key_bytes[..])
                                        .map_err(|e| StorageError::EncryptionError(format!("Encryption failed: {}", e)))?;
                                    let tag = ciphertext[ciphertext.len() - 16..].to_vec();
                                    let ciphertext = ciphertext[..ciphertext.len() - 16].to_vec();
                                    let nonce_b64 = BASE64.encode(nonce_bytes);
                                    let tag_b64 = BASE64.encode(&tag);
                                    let ciphertext_b64 = BASE64.encode(&ciphertext);
                                    
                                    client_clone.execute(
                                        "INSERT INTO public_keys (client_id, public_key_ciphertext, nonce, tag)
                                         VALUES ($1, $2, $3, $4)
                                         ON CONFLICT (client_id) DO UPDATE SET
                                             public_key_ciphertext = EXCLUDED.public_key_ciphertext,
                                             nonce = EXCLUDED.nonce,
                                             tag = EXCLUDED.tag",
                                        &[&key_data.client_id, &ciphertext_b64, &nonce_b64, &tag_b64],
                                    ).await.map_err(StorageError::Postgres)?;
                                    Ok(())
                                })().await;
                                if let Err(e) = &result {
                                    error!("Failed to save public key for client {}: {}", key_data.client_id, e);
                                }
                                reply.send(result).await.unwrap_or_else(|e| {
                                    error!("Failed to send SavePublicKey response: {:?}", e);
                                });
                            }
                            StorageCommand::GetPublicKey(client_id, reply) => {
                                debug!("Received GetPublicKey command for client {}", client_id);
                                let result = (|| async {
                                    let row = client_clone.query_opt(
                                        "SELECT public_key_ciphertext, nonce, tag FROM public_keys WHERE client_id = $1",
                                        &[&client_id],
                                    ).await.map_err(StorageError::Postgres)?;
                                    let public_key = row.map(|r| {
                                        let ciphertext_b64: String = r.get(0);
                                        let nonce_b64: String = r.get(1);
                                        let tag_b64: String = r.get(2);
                                        let ciphertext = BASE64.decode(&ciphertext_b64)
                                            .map_err(|e| StorageError::EncryptionError(format!("Invalid ciphertext: {}", e)))?;
                                        let nonce_bytes = BASE64.decode(&nonce_b64)
                                            .map_err(|e| StorageError::EncryptionError(format!("Invalid nonce: {}", e)))?;
                                        let tag = BASE64.decode(&tag_b64)
                                            .map_err(|e| StorageError::EncryptionError(format!("Invalid tag: {}", e)))?;
                                        let nonce = Nonce::from_slice(&nonce_bytes);
                                        let encrypted_bytes = [ciphertext, tag].concat();
                                        let decrypted_bytes = cipher.decrypt(nonce, encrypted_bytes.as_slice())
                                            .map_err(|e| StorageError::EncryptionError(format!("Decryption failed: {}", e)))?;
                                        Ok::<String, StorageError>(BASE64.encode(decrypted_bytes))
                                    }).transpose()?;
                                    Ok(public_key)
                                })().await;
                                if let Err(e) = &result {
                                    error!("Failed to get public key for client {}: {}", client_id, e);
                                }
                                reply.send(result).await.unwrap_or_else(|e| {
                                    error!("Failed to send GetPublicKey response: {:?}", e);
                                });
                            }
                        }
                        debug!("Finished processing command, checking for next command");
                    }
                    None => {
                        info!("Database worker task terminated: channel closed");
                        break;
                    }
                }
            }
        });

        let storage = Storage { sender };
        debug!("Storage initialized");
        Ok(storage)
    }

    pub async fn save_metadata(&self, metadata: &MessageMetadata) -> Result<(), StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        debug!("Sending SaveMetadata command for message {}", metadata.message_id);
        self.sender
            .send(StorageCommand::SaveMetadata(metadata.clone(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send SaveMetadata command".to_string()))?;
        debug!("Waiting for SaveMetadata response for message {}", metadata.message_id);
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn update_delivered_time(&self, message_id: &str, time: &str) -> Result<(), StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        debug!("Sending UpdateDeliveredTime command for message {}", message_id);
        self.sender
            .send(StorageCommand::UpdateDeliveredTime(message_id.to_string(), time.to_string(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send UpdateDeliveredTime command".to_string()))?;
        debug!("Waiting for UpdateDeliveredTime response for message {}", message_id);
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn update_acknowledged_time(&self, message_id: &str, time: &str) -> Result<(), StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        debug!("Sending UpdateAcknowledgedTime command for message {}", message_id);
        self.sender
            .send(StorageCommand::UpdateAcknowledgedTime(message_id.to_string(), time.to_string(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send UpdateAcknowledgedTime command".to_string()))?;
        debug!("Waiting for UpdateAcknowledgedTime response for message {}", message_id);
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn load_unacknowledged_metadata(&self) -> Result<Vec<MessageMetadata>, StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        debug!("Sending LoadUnacknowledgedMetadata command");
        self.sender
            .send(StorageCommand::LoadUnacknowledgedMetadata(reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send LoadUnacknowledgedMetadata command".to_string()))?;
        debug!("Waiting for LoadUnacknowledgedMetadata response");
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn get_client_id_for_message(&self, message_id: &str) -> Result<Option<String>, StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        debug!("Sending GetClientIdForMessage command for message {}", message_id);
        self.sender
            .send(StorageCommand::GetClientIdForMessage(message_id.to_string(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send GetClientIdForMessage command".to_string()))?;
        debug!("Waiting for GetClientIdForMessage response for message {}", message_id);
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn save_public_key(&self, key_data: &PublicKeyData) -> Result<(), StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        debug!("Sending SavePublicKey command for client {}", key_data.client_id);
        self.sender
            .send(StorageCommand::SavePublicKey(key_data.clone(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send SavePublicKey command".to_string()))?;
        debug!("Waiting for SavePublicKey response for client {}", key_data.client_id);
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn get_public_key(&self, client_id: &str) -> Result<Option<String>, StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        debug!("Sending GetPublicKey command for client {}", client_id);
        self.sender
            .send(StorageCommand::GetPublicKey(client_id.to_string(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send GetPublicKey command".to_string()))?;
        debug!("Waiting for GetPublicKey response for client {}", client_id);
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }
}