use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, Sender};
use std::sync::Arc;
use rusqlite::{Connection, params, OptionalExtension};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use aes_gcm::aead::Aead;
use aes_gcm::KeyInit;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rand::Rng;
use crate::config::Config;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{info, error};

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("SQLite error: {0}")]
    SQLite(#[from] rusqlite::Error),
    #[error("Channel send error: {0}")]
    ChannelSend(String),
    #[error("Channel receive error")]
    ChannelReceive,
    #[error("Encryption error: {0}")]
    EncryptionError(String),
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
    SavePublicKey(PublicKeyData, Sender<Result<(), StorageError>>),
    GetPublicKey(String, Sender<Result<Option<String>, StorageError>>),
}

impl Storage {
    pub async fn new(config: &crate::config::DatabaseConfig) -> Result<Self, StorageError> {
        info!("Attempting to connect to SQLite database: {}", config.dbname);
        
        let conn = Connection::open(&config.dbname)
            .map_err(StorageError::SQLite)?;
        
        info!("Creating public_keys table if not exists");
        conn.execute(
            "CREATE TABLE IF NOT EXISTS public_keys (
                client_id TEXT PRIMARY KEY,
                public_key_ciphertext TEXT NOT NULL,
                nonce TEXT NOT NULL,
                tag TEXT NOT NULL
            )",
            []
        ).map_err(StorageError::SQLite)?;
        info!("Table creation completed");

        let conn = Arc::new(Mutex::new(conn));
        let (sender, mut receiver) = mpsc::channel::<StorageCommand>(100);
        info!("Database worker task started with channel capacity 100");

        let conn_clone = conn.clone();
        tokio::spawn(async move {
            let config = Config::load("config.toml").expect("Failed to load config");
            let aes_key = BASE64.decode(&config.encryption.aes_key)
                .expect("Invalid AES key");
            let key = Key::<Aes256Gcm>::from_slice(&aes_key);
            let cipher = Aes256Gcm::new(key);

            loop {
                match receiver.recv().await {
                    Some(command) => {
                        match command {
                            StorageCommand::SavePublicKey(key_data, reply) => {
                                info!("Received SavePublicKey command for client {}", key_data.client_id);
                                let result = async {
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
                                    
                                    let conn = conn_clone.lock().await;
                                    conn.execute(
                                        "INSERT OR REPLACE INTO public_keys (client_id, public_key_ciphertext, nonce, tag)
                                         VALUES (?, ?, ?, ?)",
                                        params![key_data.client_id, ciphertext_b64, nonce_b64, tag_b64]
                                    ).map_err(StorageError::SQLite)?;
                                    Ok(())
                                }.await;

                                if let Err(e) = &result {
                                    error!("Failed to save public key for client {}: {}", key_data.client_id, e);
                                }
                                reply.send(result).await.unwrap_or_else(|e| {
                                    error!("Failed to send SavePublicKey response: {:?}", e);
                                });
                            }
                            StorageCommand::GetPublicKey(client_id, reply) => {
                                info!("Received GetPublicKey command for client {}", client_id);
                                let result = async {
                                    let conn = conn_clone.lock().await;
                                    let public_key = conn.query_row(
                                        "SELECT public_key_ciphertext, nonce, tag FROM public_keys WHERE client_id = ?",
                                        params![client_id],
                                        |row| {
                                            let ciphertext_b64: String = row.get(0)?;
                                            let nonce_b64: String = row.get(1)?;
                                            let tag_b64: String = row.get(2)?;
                                            Ok((ciphertext_b64, nonce_b64, tag_b64))
                                        }
                                    ).optional().map_err(StorageError::SQLite)?;
                                    
                                    let public_key = match public_key {
                                        Some((ciphertext_b64, nonce_b64, tag_b64)) => {
                                            let ciphertext = BASE64.decode(&ciphertext_b64)
                                                .map_err(|e| StorageError::EncryptionError(format!("Invalid ciphertext: {}", e)))?;
                                            let nonce_bytes = BASE64.decode(&nonce_b64)
                                                .map_err(|e| StorageError::EncryptionError(format!("Invalid nonce: {}", e)))?;
                                            let tag = BASE64.decode(&tag_b64)
                                                .map_err(|e| StorageError::EncryptionError(format!("Invalid tag: {}", e)))?;
                                            let mut ciphertext_with_tag = ciphertext;
                                            ciphertext_with_tag.extend_from_slice(&tag);
                                            let nonce = Nonce::from_slice(&nonce_bytes);
                                            let plaintext = cipher.decrypt(nonce, &ciphertext_with_tag[..])
                                                .map_err(|e| StorageError::EncryptionError(format!("Decryption failed: {}", e)))?;
                                            Some(BASE64.encode(plaintext))
                                        }
                                        None => None,
                                    };
                                    Ok(public_key)
                                }.await;

                                if let Err(e) = &result {
                                    error!("Failed to get public key for client {}: {}", client_id, e);
                                }
                                reply.send(result).await.unwrap_or_else(|e| {
                                    error!("Failed to send GetPublicKey response: {:?}", e);
                                });
                            }
                        }
                    }
                    None => {
                        info!("Database worker task terminated: channel closed");
                        break;
                    }
                }
            }
        });

        Ok(Storage { sender })
    }

    pub async fn save_public_key(&self, key_data: &PublicKeyData) -> Result<(), StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        self.sender
            .send(StorageCommand::SavePublicKey(key_data.clone(), reply_tx))
            .await
            .map_err(|e| StorageError::ChannelSend(format!("Failed to send SavePublicKey command: {}", e)))?;
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }

    pub async fn get_public_key(&self, client_id: &str) -> Result<Option<String>, StorageError> {
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        self.sender
            .send(StorageCommand::GetPublicKey(client_id.to_string(), reply_tx))
            .await
            .map_err(|e| StorageError::ChannelSend(format!("Failed to send GetPublicKey command: {}", e)))?;
        reply_rx.recv().await.ok_or(StorageError::ChannelReceive)?
    }
}