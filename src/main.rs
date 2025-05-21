/*
Secure Message Broker Server
This file implements a Rust-based server for a secure message broker system.
It uses AES-GCM encryption, Axum for HTTP routing, and Tokio-Tungstenite for WebSocket communication.
Dependencies: aes_gcm, axum, dashmap, futures_util, serde, tokio, tokio-tungstenite, tracing
 */

use aes_gcm::{
    aead::{rand_core::RngCore, Aead, KeyInit, OsRng},
    Aes256Gcm, Key, Nonce,
};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use dashmap::DashMap;
use futures_util::{future, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

// Input data structure for single messages received via HTTP.
#[derive(Serialize, Deserialize, Clone)]
struct InputData {
    content: String,
}

// Input data structure for batch messages received via HTTP.
#[derive(Serialize, Deserialize)]
struct BatchInputData {
    messages: Vec<InputData>,
}

// Structure for encrypted data, storing the ciphertext and nonce.
#[derive(Clone)]
struct EncryptedData {
    ciphertext: Vec<u8>,
    nonce: [u8; 12],
}

// Main server state, holding the data store, sender channel, and encryption key.
// Uses DashMap for thread-safe, fast access to encrypted data.
struct ServerState {
    data_store: DashMap<usize, EncryptedData>,
    sender_tx: mpsc::UnboundedSender<EncryptedData>,
    key: Key<Aes256Gcm>,
}

#[tokio::main]
async fn main() {
    // Initialize logging with the tracing framework for debugging and monitoring.
    tracing_subscriber::fmt::init();

    // Create a channel for communication between the HTTP server and WebSocket sender.
    let (sender_tx, mut sender_rx) = mpsc::unbounded_channel::<EncryptedData>();

    // Generate a shared encryption key for AES-GCM.
    let key = Aes256Gcm::generate_key(&mut OsRng);

    // Initialize the server state with a DashMap, sender channel, and encryption key.
    let server_state = Arc::new(ServerState {
        data_store: DashMap::new(),
        sender_tx,
        key,
    });

    // Set up the HTTP server (Input Gateway) using Axum for handling incoming messages.
    let app = Router::new()
        .route("/", get(|| async { "Message Broker Running" }))
        .route("/input", post(handle_input))
        .route("/input/batch", post(handle_batch_input))
        .with_state(Arc::clone(&server_state));

    let listener = TcpListener::bind("127.0.0.1:3000").await.unwrap();
    info!("Input Gateway running on http://127.0.0.1:3000");

    // Run the WebSocket sender in a separate async task to handle outgoing messages.
    tokio::spawn(async move {
        let addr = "127.0.0.1:8080";
        let listener = TcpListener::bind(addr).await.unwrap();
        info!("Sender WebSocket running on ws://{}", addr);

        // Accept incoming WebSocket connections in a loop.
        while let Ok((stream, addr)) = listener.accept().await {
            info!("New WebSocket connection from {}", addr);
            let ws_stream = match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => ws,
                Err(e) => {
                    warn!("Failed to accept WebSocket connection: {}", e);
                    continue;
                }
            };
            let (mut ws_tx, _) = ws_stream.split();
            let cipher = Aes256Gcm::new(&server_state.key);
            let mut buffer = Vec::new();

            // Receive encrypted data from the channel and send to the WebSocket client.
            while let Some(data) = sender_rx.recv().await {
                // Decrypt the data using the shared key and nonce.
                let plaintext = match cipher
                    .decrypt(Nonce::from_slice(&data.nonce), data.ciphertext.as_ref())
                {
                    Ok(pt) => pt,
                    Err(e) => {
                        warn!(
                            "Decryption failed: {}. Ciphertext: {:?}, Nonce: {:?}",
                            e, data.ciphertext, data.nonce
                        );
                        continue;
                    }
                };
                // Convert the decrypted bytes to a UTF-8 string.
                let message = match String::from_utf8(plaintext) {
                    Ok(msg) => msg,
                    Err(e) => {
                        warn!("Invalid UTF-8 data: {}", e);
                        continue;
                    }
                };
                buffer.push(message);

                // Send a batch of messages after collecting 10, optimizing network usage.
                if buffer.len() >= 10 {
                    let batch_message = serde_json::to_string(&buffer).unwrap();
                    if ws_tx
                        .send(Message::Text(batch_message.into()))
                        .await
                        .is_err()
                    {
                        warn!("Failed to send batch to WebSocket client");
                        break;
                    }
                    info!(
                        "Sent batch of {} messages to WebSocket client",
                        buffer.len()
                    );
                    buffer.clear();
                }
            }

            // Send any remaining messages in the buffer before closing the connection.
            if !buffer.is_empty() {
                let batch_message = serde_json::to_string(&buffer).unwrap();
                if ws_tx
                    .send(Message::Text(batch_message.into()))
                    .await
                    .is_err()
                {
                    warn!("Failed to send final batch to WebSocket client");
                }
                info!(
                    "Sent final batch of {} messages to WebSocket client",
                    buffer.len()
                );
            }
        }
    });

    // Run the Axum HTTP server to handle incoming requests.
    axum::serve(listener, app).await.unwrap();
}

// Handler for processing single message input via HTTP POST.
// Encrypts the message and stores it, then sends it to the WebSocket sender.
async fn handle_input(
    State(state): State<Arc<ServerState>>,
    Json(input): Json<InputData>,
) -> impl IntoResponse {
    let cipher = Aes256Gcm::new(&state.key);
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes); // Generate a random nonce for encryption.
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, input.content.as_bytes())
        .map_err(|e| {
            error!("Encryption failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let encrypted_data = EncryptedData {
        ciphertext,
        nonce: nonce_bytes,
    };

    // Store the encrypted data in the DashMap using the current store length as the key.
    let store_len = state.data_store.len();
    state.data_store.insert(store_len, encrypted_data.clone());

    // Send the encrypted data to the WebSocket sender via the channel.
    if state.sender_tx.send(encrypted_data).is_err() {
        error!("Failed to send data to Sender");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    info!("Data received and encrypted: {}", input.content);
    Ok(StatusCode::OK)
}

// Handler for processing batch message input via HTTP POST.
// Encrypts messages in parallel, stores them, and sends them to the WebSocket sender.
async fn handle_batch_input(
    State(state): State<Arc<ServerState>>,
    Json(batch): Json<BatchInputData>,
) -> impl IntoResponse {
    let _cipher = Aes256Gcm::new(&state.key);

    // Encrypt messages in parallel using async tasks for performance.
    let encrypt_tasks: Vec<_> = batch
        .messages
        .iter()
        .map(|input| {
            let cipher = Aes256Gcm::new(&state.key);
            let input = input.clone(); // Clone input for async task.
            task::spawn(async move {
                let mut nonce_bytes = [0u8; 12];
                OsRng.fill_bytes(&mut nonce_bytes);
                let nonce = Nonce::from_slice(&nonce_bytes);
                let ciphertext = cipher.encrypt(nonce, input.content.as_bytes()).unwrap();
                EncryptedData {
                    ciphertext,
                    nonce: nonce_bytes,
                }
            })
        })
        .collect();

    let encrypted_batch: Vec<_> = future::join_all(encrypt_tasks)
        .await
        .into_iter()
        .map(|res| res.unwrap())
        .collect();

    // Store the batch of encrypted data in the DashMap with sequential keys.
    let store_len = state.data_store.len();
    for (i, data) in encrypted_batch.iter().enumerate() {
        state.data_store.insert(store_len + i, data.clone());
    }

    // Send each encrypted message in the batch to the WebSocket sender.
    for data in encrypted_batch.iter() {
        if state.sender_tx.send(data.clone()).is_err() {
            error!("Failed to send data to Sender");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    info!(
        "Batch of {} messages received and encrypted",
        batch.messages.len()
    );
    Ok(StatusCode::OK)
}
