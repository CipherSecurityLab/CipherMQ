use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, Sender};
use tracing::{error, info, debug, warn};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use thiserror::Error;
use aes_gcm::{Aes256Gcm, Key, Nonce};
use aes_gcm::aead::Aead;
use aes_gcm::KeyInit;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rand::Rng;
use crate::config::{Config, DatabaseConfig};
use tokio::time::{self, Duration, interval};
use std::collections::HashMap;

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("PostgreSQL error: {0}")]
    Postgres(#[from] sqlx::Error),
    #[error("Channel send error: {0}")]
    ChannelSend(String),
    #[error("Channel receive error")]
    ChannelReceive,
    #[error("Encryption error: {0}")]
    EncryptionError(String),
}

#[derive(Clone, Serialize, Deserialize, sqlx::FromRow)]
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
    // ✅ NEW: Fire-and-forget updates (no reply channel)
    UpdateDeliveredTimeAsync(String, String),
    UpdateAcknowledgedTimeAsync(String, String),
    // ✅ Keep synchronous versions for critical operations
    UpdateDeliveredTime(String, String, Sender<Result<(), StorageError>>),
    UpdateAcknowledgedTime(String, String, Sender<Result<(), StorageError>>),
    LoadUnacknowledgedMetadata(Sender<Result<Vec<MessageMetadata>, StorageError>>),
    GetClientIdForMessage(String, Sender<Result<Option<String>, StorageError>>),
    SavePublicKey(PublicKeyData, Sender<Result<(), StorageError>>),
    GetPublicKey(String, Sender<Result<Option<String>, StorageError>>),
    // ✅ NEW: Flush buffered updates
    FlushUpdates,
}

impl Storage {
    async fn connect_with_retry(config: &DatabaseConfig) -> Result<Arc<PgPool>, StorageError> {
        let conn_str = format!(
            "postgres://{}:{}@{}:{}/{}",
            config.user, config.password, config.host, config.port, config.dbname
        );
        let max_attempts = 5;
        let mut attempts = 0;

        loop {
            attempts += 1;
            match sqlx::postgres::PgPoolOptions::new()
                .max_connections(200) 
                .min_connections(10)
                .max_lifetime(Duration::from_secs(3600)) 
                .idle_timeout(Duration::from_secs(6000)) 
                .acquire_timeout(Duration::from_secs(3000)) 
                .connect(&conn_str)
                .await
            {
                Ok(pool) => {
                    info!("Successfully connected to PostgreSQL database on attempt {}", attempts);
                    return Ok(Arc::new(pool));
                }
                Err(e) => {
                    error!("Failed to connect to database on attempt {}/{}: {}", attempts, max_attempts, e);
                    if attempts >= max_attempts {
                        return Err(StorageError::Postgres(e));
                    }
                    time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    pub async fn new(config: &DatabaseConfig) -> Result<Self, StorageError> {
        info!("Attempting to connect to PostgreSQL database: {}", config.dbname);

        let pool = Self::connect_with_retry(config).await?;

        info!("Creating message_metadata table if not exists");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS message_metadata (
                message_id TEXT PRIMARY KEY,
                client_id TEXT,
                exchange_name TEXT,
                routing_key TEXT,
                sent_time TEXT,
                delivered_time TEXT,
                acknowledged_time TEXT
            )"
        )
        .execute(&*pool)
        .await
        .map_err(|e| {
            error!("Failed to create message_metadata table: {}", e);
            StorageError::Postgres(e)
        })?;

        info!("Creating public_keys table if not exists");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS public_keys (
                client_id TEXT PRIMARY KEY,
                public_key_ciphertext TEXT NOT NULL,
                nonce TEXT NOT NULL,
                tag TEXT NOT NULL
            )"
        )
        .execute(&*pool)
        .await
        .map_err(|e| {
            error!("Failed to create public_keys table: {}", e);
            StorageError::Postgres(e)
        })?;
        info!("Table creation completed");

        let (sender, mut receiver) = mpsc::channel::<StorageCommand>(1000);
        info!("Database worker task started with optimized batching");

        let config_clone = config.clone();
        tokio::spawn(async move {
            let mut pool = pool;
            let config = Config::load("config.toml").map_err(|e| {
                error!("Failed to load config in database task: {}", e);
                e
            }).expect("Failed to load config");
            let aes_key = match BASE64.decode(&config.encryption.aes_key) {
                Ok(key) => key,
                Err(e) => {
                    error!("Failed to decode AES key: {}", e);
                    return;
                }
            };
            let key = Key::<Aes256Gcm>::from_slice(&aes_key);
            let cipher = Aes256Gcm::new(key);

            // ✅ Buffers for batching updates
            let mut delivered_updates: HashMap<String, String> = HashMap::new();
            let mut acknowledged_updates: HashMap<String, String> = HashMap::new();

            // ✅ Flush interval: every 500ms or when buffer reaches 50 items
            let mut flush_interval = interval(Duration::from_millis(500));
            const BATCH_SIZE: usize = 50;

            loop {
                tokio::select! {
                    Some(command) = receiver.recv() => {
                        if pool.is_closed() {
                            error!("Database pool closed, attempting to reconnect");
                            match Self::connect_with_retry(&config_clone).await {
                                Ok(new_pool) => {
                                    pool = new_pool;
                                    info!("Reconnected to database successfully");
                                }
                                Err(e) => {
                                    error!("Failed to reconnect to database: {}", e);
                                    continue;
                                }
                            }
                        }

                        match command {
                            StorageCommand::SaveMetadata(metadata, reply) => {
                                let result = async {
                                    match sqlx::query(
                                        "INSERT INTO message_metadata (
                                            message_id, client_id, exchange_name, routing_key,
                                            sent_time, delivered_time, acknowledged_time
                                        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
                                        ON CONFLICT (message_id) DO UPDATE SET
                                            client_id = EXCLUDED.client_id,
                                            exchange_name = EXCLUDED.exchange_name,
                                            routing_key = EXCLUDED.routing_key,
                                            sent_time = EXCLUDED.sent_time
                                        RETURNING message_id"
                                    )
                                    .bind(&metadata.message_id)
                                    .bind(&metadata.client_id)
                                    .bind(&metadata.exchange_name)
                                    .bind(&metadata.routing_key)
                                    .bind(&metadata.sent_time)
                                    .bind(&metadata.delivered_time)
                                    .bind(&metadata.acknowledged_time)
                                    .execute(&*pool)
                                    .await
                                    {
                                        Ok(_) => Ok(()),
                                        Err(e) => {
                                            error!("Failed to save metadata for message {}: {}", metadata.message_id, e);
                                            Err(StorageError::Postgres(e))
                                        }
                                    }
                                }.await;
                                let _ = reply.send(result).await;
                            }
                            // ✅ NEW: Async updates (buffered)
                            StorageCommand::UpdateDeliveredTimeAsync(message_id, time) => {
                                delivered_updates.insert(message_id, time);
                                
                                // Auto-flush if buffer is full
                                if delivered_updates.len() >= BATCH_SIZE {
                                    Self::flush_delivered_updates(&pool, &mut delivered_updates).await;
                                }
                            }
                            StorageCommand::UpdateAcknowledgedTimeAsync(message_id, time) => {
                                acknowledged_updates.insert(message_id, time);
                                
                                // Auto-flush if buffer is full
                                if acknowledged_updates.len() >= BATCH_SIZE {
                                    Self::flush_acknowledged_updates(&pool, &mut acknowledged_updates).await;
                                }
                            }
                            // ✅ Keep synchronous versions for critical paths
                            StorageCommand::UpdateDeliveredTime(message_id, time, reply) => {
                                let result = Self::update_delivered_time_sync(&pool, &message_id, &time).await;
                                let _ = reply.send(result).await;
                            }
                            StorageCommand::UpdateAcknowledgedTime(message_id, time, reply) => {
                                let result = Self::update_acknowledged_time_sync(&pool, &message_id, &time).await;
                                let _ = reply.send(result).await;
                            }
                            StorageCommand::FlushUpdates => {
                                Self::flush_delivered_updates(&pool, &mut delivered_updates).await;
                                Self::flush_acknowledged_updates(&pool, &mut acknowledged_updates).await;
                            }
                            StorageCommand::LoadUnacknowledgedMetadata(reply) => {
                                let result = async {
                                    match sqlx::query_as::<_, MessageMetadata>(
                                        "SELECT * FROM message_metadata WHERE acknowledged_time IS NULL"
                                    )
                                    .fetch_all(&*pool)
                                    .await
                                    {
                                        Ok(rows) => Ok(rows),
                                        Err(e) => {
                                            error!("Failed to load unacknowledged metadata: {}", e);
                                            Err(StorageError::Postgres(e))
                                        }
                                    }
                                }.await;
                                let _ = reply.send(result).await;
                            }
                            StorageCommand::GetClientIdForMessage(message_id, reply) => {
                                let result = async {
                                    match sqlx::query(
                                        "SELECT client_id FROM message_metadata WHERE message_id = $1"
                                    )
                                    .bind(&message_id)
                                    .fetch_optional(&*pool)
                                    .await
                                    {
                                        Ok(row) => Ok(row.map(|r| r.get::<String, _>("client_id"))),
                                        Err(e) => {
                                            error!("Failed to get client_id for message {}: {}", message_id, e);
                                            Err(StorageError::Postgres(e))
                                        }
                                    }
                                }.await;
                                let _ = reply.send(result).await;
                            }
                            StorageCommand::SavePublicKey(key_data, reply) => {
                                let result = async {
                                    let nonce_bytes: [u8; 12] = rand::thread_rng().gen();
                                    let nonce = Nonce::from_slice(&nonce_bytes);
                                    let public_key_bytes = BASE64.decode(&key_data.public_key)
                                        .map_err(|e| StorageError::EncryptionError(format!("Invalid public key: {}", e)))?;
                                    let encrypted_result = cipher.encrypt(nonce, public_key_bytes.as_slice())
                                        .map_err(|e| StorageError::EncryptionError(format!("Encryption failed: {}", e)))?;
                                    let (ciphertext, tag) = encrypted_result.split_at(encrypted_result.len() - 16);
                                    match sqlx::query(
                                        "INSERT INTO public_keys (client_id, public_key_ciphertext, nonce, tag)
                                        VALUES ($1, $2, $3, $4)
                                        ON CONFLICT (client_id) DO UPDATE SET
                                            public_key_ciphertext = EXCLUDED.public_key_ciphertext,
                                            nonce = EXCLUDED.nonce,
                                            tag = EXCLUDED.tag"
                                    )
                                    .bind(&key_data.client_id)
                                    .bind(BASE64.encode(&ciphertext))
                                    .bind(BASE64.encode(&nonce_bytes))
                                    .bind(BASE64.encode(&tag))
                                    .execute(&*pool)
                                    .await
                                    {
                                        Ok(_) => Ok(()),
                                        Err(e) => {
                                            error!("Failed to save public key for client {}: {}", key_data.client_id, e);
                                            Err(StorageError::Postgres(e))
                                        }
                                    }
                                }.await;
                                let _ = reply.send(result).await;
                            }
                            StorageCommand::GetPublicKey(client_id, reply) => {
                                let result = async {
                                    match sqlx::query(
                                        "SELECT public_key_ciphertext, nonce, tag FROM public_keys WHERE client_id = $1"
                                    )
                                    .bind(&client_id)
                                    .fetch_optional(&*pool)
                                    .await
                                    {
                                        Ok(row) => {
                                            let public_key = row.map(|r| {
                                                let ciphertext_b64: String = r.get("public_key_ciphertext");
                                                let nonce_b64: String = r.get("nonce");
                                                let tag_b64: String = r.get("tag");
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
                                        }
                                        Err(e) => {
                                            error!("Failed to get public key for client {}: {}", client_id, e);
                                            Err(StorageError::Postgres(e))
                                        }
                                    }
                                }.await;
                                let _ = reply.send(result).await;
                            }
                        }
                    }
                    // ✅ Periodic flush every 100ms
                    _ = flush_interval.tick() => {
                        if !delivered_updates.is_empty() || !acknowledged_updates.is_empty() {
                            Self::flush_delivered_updates(&pool, &mut delivered_updates).await;
                            Self::flush_acknowledged_updates(&pool, &mut acknowledged_updates).await;
                        }
                    }
                }
            }
        });

        let storage = Storage { sender };
        debug!("Storage initialized");
        Ok(storage)
    }

    // ✅ Helper: Flush delivered updates in batch
    async fn flush_delivered_updates(pool: &PgPool, updates: &mut HashMap<String, String>) {
        if updates.is_empty() {
            return;
        }

        let count = updates.len();
        let message_ids: Vec<String> = updates.keys().cloned().collect();
        let times: Vec<String> = updates.values().cloned().collect();

        match sqlx::query(
            "UPDATE message_metadata 
             SET delivered_time = data.time
             FROM (SELECT unnest($1::text[]) as mid, unnest($2::text[]) as time) as data
             WHERE message_id = data.mid AND delivered_time IS NULL"
        )
        .bind(&message_ids)
        .bind(&times)
        .execute(pool)
        .await
        {
            Ok(result) => {
                debug!("Flushed {} delivered_time updates, {} rows affected", count, result.rows_affected());
            }
            Err(e) => {
                error!("Failed to flush delivered_time updates: {}", e);
            }
        }

        updates.clear();
    }

    // ✅ Helper: Flush acknowledged updates in batch
    async fn flush_acknowledged_updates(pool: &PgPool, updates: &mut HashMap<String, String>) {
        if updates.is_empty() {
            return;
        }

        let count = updates.len();
        let message_ids: Vec<String> = updates.keys().cloned().collect();
        let times: Vec<String> = updates.values().cloned().collect();

        match sqlx::query(
            "UPDATE message_metadata 
             SET acknowledged_time = data.time
             FROM (SELECT unnest($1::text[]) as mid, unnest($2::text[]) as time) as data
             WHERE message_id = data.mid AND acknowledged_time IS NULL"
        )
        .bind(&message_ids)
        .bind(&times)
        .execute(pool)
        .await
        {
            Ok(result) => {
                debug!("Flushed {} acknowledged_time updates, {} rows affected", count, result.rows_affected());
            }
            Err(e) => {
                error!("Failed to flush acknowledged_time updates: {}", e);
            }
        }

        updates.clear();
    }

    // ✅ Synchronous update (for critical paths)
    async fn update_delivered_time_sync(pool: &PgPool, message_id: &str, time: &str) -> Result<(), StorageError> {
        match sqlx::query(
            "UPDATE message_metadata SET delivered_time = $1 WHERE message_id = $2 AND delivered_time IS NULL"
        )
        .bind(time)
        .bind(message_id)
        .execute(pool)
        .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("Failed to update delivered time for message {}: {}", message_id, e);
                Err(StorageError::Postgres(e))
            }
        }
    }

    async fn update_acknowledged_time_sync(pool: &PgPool, message_id: &str, time: &str) -> Result<(), StorageError> {
        match sqlx::query(
            "UPDATE message_metadata SET acknowledged_time = $1 WHERE message_id = $2 AND acknowledged_time IS NULL"
        )
        .bind(time)
        .bind(message_id)
        .execute(pool)
        .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("Failed to update acknowledged time for message {}: {}", message_id, e);
                Err(StorageError::Postgres(e))
            }
        }
    }

    // ✅ Public API: Async updates (fire-and-forget)
    pub async fn update_delivered_time_async(&self, message_id: &str, time: &str) {
        let _ = self.sender
            .send(StorageCommand::UpdateDeliveredTimeAsync(message_id.to_string(), time.to_string()))
            .await;
    }

    pub async fn update_acknowledged_time_async(&self, message_id: &str, time: &str) {
        let _ = self.sender
            .send(StorageCommand::UpdateAcknowledgedTimeAsync(message_id.to_string(), time.to_string()))
            .await;
    }

    // Existing public methods remain the same
    pub async fn save_metadata(&self, metadata: &MessageMetadata) -> Result<(), StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        self.sender
            .send(StorageCommand::SaveMetadata(metadata.clone(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send SaveMetadata command".to_string()))?;
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    // Keep synchronous version for critical paths
    pub async fn update_delivered_time(&self, message_id: &str, time: &str) -> Result<(), StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        self.sender
            .send(StorageCommand::UpdateDeliveredTime(message_id.to_string(), time.to_string(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send UpdateDeliveredTime command".to_string()))?;
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn update_acknowledged_time(&self, message_id: &str, time: &str) -> Result<(), StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        self.sender
            .send(StorageCommand::UpdateAcknowledgedTime(message_id.to_string(), time.to_string(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send UpdateAcknowledgedTime command".to_string()))?;
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn load_unacknowledged_metadata(&self) -> Result<Vec<MessageMetadata>, StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        self.sender
            .send(StorageCommand::LoadUnacknowledgedMetadata(reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send LoadUnacknowledgedMetadata command".to_string()))?;
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn get_client_id_for_message(&self, message_id: &str) -> Result<Option<String>, StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        self.sender
            .send(StorageCommand::GetClientIdForMessage(message_id.to_string(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send GetClientIdForMessage command".to_string()))?;
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn save_public_key(&self, key_data: &PublicKeyData) -> Result<(), StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        self.sender
            .send(StorageCommand::SavePublicKey(key_data.clone(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send SavePublicKey command".to_string()))?;
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn get_public_key(&self, client_id: &str) -> Result<Option<String>, StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        self.sender
            .send(StorageCommand::GetPublicKey(client_id.to_string(), reply_tx))
            .await
            .map_err(|_| StorageError::ChannelSend("Failed to send GetPublicKey command".to_string()))?;
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }
}