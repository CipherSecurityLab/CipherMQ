use crate::state::{EncryptedInputData, ServerState};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{timeout, Duration};
use tracing::{error, info, warn, instrument};
use crate::connection::Connection;

#[instrument(skip(stream, state), fields(client_id = %stream.client_id()))]
pub async fn handle_client(mut stream: impl Connection, state: Arc<RwLock<ServerState>>) {
    let client_id = stream.client_id().to_string();
    state.write().await.increment_client_count().await;
    let mut buffer = Vec::with_capacity(4096);
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, EncryptedInputData)>();

    loop {
        tokio::select! {
            result = timeout(Duration::from_secs(1), stream.read_buf(&mut buffer)) => {
                match result {
                    Ok(Ok(n)) if n == 0 => {
                        info!("Client disconnected");
                        state.write().await.decrement_client_count().await;
                        return;
                    }
                    Ok(Ok(_)) => {
                        let request = String::from_utf8_lossy(&buffer).trim().to_string();
                        info!(request = %request, "Received request");

                        let parts: Vec<&str> = request.splitn(2, ' ').collect();
                        let command = parts.get(0).unwrap_or(&"");
                        let args_str = parts.get(1).unwrap_or(&"");

                        match *command {
                            "declare_queue" => {
                                if !args_str.is_empty() {
                                    state.read().await.declare_queue(args_str);
                                    stream.write_all(b"Queue declared\n").await.unwrap_or_else(|e| {
                                        error!(queue_name = %args_str, error = %e, "Failed to send response");
                                    });
                                    info!(queue_name = %args_str, "Queue declared successfully");
                                } else {
                                    error!(queue_name = %args_str, "Missing queue name");
                                    stream.write_all(b"Missing queue name\n").await.unwrap_or_else(|e| {
                                        error!(queue_name = %args_str, error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "declare_exchange" => {
                                if !args_str.is_empty() {
                                    state.read().await.declare_exchange(args_str);
                                    stream.write_all(b"Exchange declared\n").await.unwrap_or_else(|e| {
                                        error!(exchange_name = %args_str, error = %e, "Failed to send response");
                                    });
                                    info!(exchange_name = %args_str, "Exchange declared successfully");
                                } else {
                                    error!(exchange_name = %args_str, "Missing exchange name");
                                    stream.write_all(b"Missing exchange name\n").await.unwrap_or_else(|e| {
                                        error!(exchange_name = %args_str, error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "bind" => {
                                let args: Vec<&str> = args_str.splitn(3, ' ').collect();
                                if args.len() >= 3 {
                                    state.read().await.bind_queue(args[0], args[1], args[2]);
                                    stream.write_all(b"Queue bound\n").await.unwrap_or_else(|e| {
                                        error!(queue_name = %args[0], exchange_name = %args[1], error = %e, "Failed to send response");
                                    });
                                    info!(queue_name = %args[0], exchange_name = %args[1], routing_key = %args[2], "Queue bound successfully");
                                } else {
                                    error!(args = %args_str, "Missing parameters for bind");
                                    stream.write_all(b"Missing parameters\n").await.unwrap_or_else(|e| {
                                        error!(args = %args_str, error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "publish" => {
                                let args: Vec<&str> = args_str.splitn(3, ' ').collect();
                                if args.len() >= 3 {
                                    let message_str = args[2];
                                    match serde_json::from_str::<EncryptedInputData>(message_str) {
                                        Ok(message) => {
                                            match state.read().await.publish(args[0], args[1], message.clone(), client_id.clone(), Instant::now()).await {
                                                Ok(()) => {
                                                    stream
                                                        .write_all(format!("ACK {}\n", message.message_id).as_bytes())
                                                        .await
                                                        .unwrap_or_else(|e| {
                                                            error!(message_id = %message.message_id, error = %e, "Failed to send ACK");
                                                        });
                                                    info!(message_id = %message.message_id, exchange_name = %args[0], routing_key = %args[1], "Message published successfully");
                                                }
                                                Err(e) => {
                                                    stream
                                                        .write_all(format!("Error: {}\n", e).as_bytes())
                                                        .await
                                                        .unwrap_or_else(|e2| {
                                                            error!(message_id = %message.message_id, error = %e2, "Failed to send error response");
                                                        });
                                                    error!(message_id = %message.message_id, error = %e, "Failed to publish message");
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            stream
                                                .write_all(format!("Invalid message format: {}\n", e).as_bytes())
                                                .await
                                                .unwrap_or_else(|e2| {
                                                    error!(error = %e2, "Failed to send error response");
                                                });
                                            error!(error = %e, "Invalid message format");
                                        }
                                    }
                                } else {
                                    error!(args = %args_str, "Missing parameters for publish");
                                    stream.write_all(b"Missing parameters\n").await.unwrap_or_else(|e| {
                                        error!(args = %args_str, error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "publish_batch" => {
                                let args: Vec<&str> = args_str.splitn(3, ' ').collect();
                                if args.len() >= 3 {
                                    let exchange = args[0];
                                    let routing_key = args[1];
                                    let message_str = args[2];
                                    info!(exchange_name = %exchange, routing_key = %routing_key, "Parsing batch JSON");
                                    match serde_json::from_str::<Vec<EncryptedInputData>>(message_str) {
                                        Ok(messages) => {
                                            let mut acks = Vec::new();
                                            for message in messages {
                                                match state.read().await.publish(exchange, routing_key, message.clone(), client_id.clone(), Instant::now()).await {
                                                    Ok(()) => {
                                                        acks.push(format!("ACK {}", message.message_id));
                                                        info!(message_id = %message.message_id, exchange_name = %exchange, routing_key = %routing_key, "Batch message published successfully");
                                                    }
                                                    Err(e) => {
                                                        acks.push(format!("Error: {} for message {}", e, message.message_id));
                                                        error!(message_id = %message.message_id, error = %e, "Failed to publish batch message");
                                                    }
                                                }
                                            }
                                            let response = acks.join("\n") + "\n";
                                            stream.write_all(response.as_bytes()).await.unwrap_or_else(|e| {
                                                error!(exchange_name = %exchange, error = %e, "Failed to send batch response");
                                            });
                                        }
                                        Err(e) => {
                                            stream
                                                .write_all(format!("Invalid batch format: {}\n", e).as_bytes())
                                                .await
                                                .unwrap_or_else(|e2| {
                                                    error!(error = %e2, "Failed to send error response");
                                                });
                                            error!(error = %e, "Invalid batch format");
                                        }
                                    }
                                } else {
                                    error!(args = %args_str, "Missing parameters for publish_batch");
                                    stream.write_all(b"Missing parameters\n").await.unwrap_or_else(|e| {
                                        error!(args = %args_str, error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "consume" => {
                                if !args_str.is_empty() {
                                    state.read().await.register_consumer(args_str, tx.clone());
                                    stream.write_all(b"Subscribed to queue\n").await.unwrap_or_else(|e| {
                                        error!(queue_name = %args_str, error = %e, "Failed to send response");
                                    });
                                    info!(queue_name = %args_str, "Consumer subscribed to queue");
                                } else {
                                    error!(queue_name = %args_str, "Missing queue name for consume");
                                    stream.write_all(b"Missing queue name\n").await.unwrap_or_else(|e| {
                                        error!(queue_name = %args_str, error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "fetch" => {
                                if !args_str.is_empty() {
                                    if let Some((message_id, message)) = state.read().await.consume(args_str).await {
                                        let message_str = serde_json::to_string(&message).unwrap();
                                        stream
                                            .write_all(format!("Message: {} {}\n", message_id, message_str).as_bytes())
                                            .await
                                            .unwrap_or_else(|e| {
                                                error!(message_id = %message_id, queue_name = %args_str, error = %e, "Failed to send message");
                                            });
                                        info!(message_id = %message_id, queue_name = %args_str, "Message fetched from queue");
                                    } else {
                                        stream.write_all(b"No messages\n").await.unwrap_or_else(|e| {
                                            error!(queue_name = %args_str, error = %e, "Failed to send response");
                                        });
                                        info!(queue_name = %args_str, "No messages available in queue");
                                    }
                                } else {
                                    error!(queue_name = %args_str, "Missing queue name for fetch");
                                    stream.write_all(b"Missing queue name\n").await.unwrap_or_else(|e| {
                                        error!(queue_name = %args_str, error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "ack" => {
                                if !args_str.is_empty() {
                                    state.read().await.acknowledge(args_str).await;
                                    stream
                                        .write_all(format!("ACK_CONFIRMED {}\n", args_str).as_bytes())
                                        .await
                                        .unwrap_or_else(|e| {
                                            error!(message_id = %args_str, error = %e, "Failed to send ACK confirmation");
                                        });
                                    info!(message_id = %args_str, "ACK confirmed for message");
                                } else {
                                    error!(message_id = %args_str, "Missing message ID for ack");
                                    stream.write_all(b"Missing message ID\n").await.unwrap_or_else(|e| {
                                        error!(message_id = %args_str, error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "resend" => {
                                if !args_str.is_empty() {
                                    match state.read().await.storage.get_client_id_for_message(args_str).await {
                                        Ok(client_id_opt) => {
                                            if let Some(target_client_id) = client_id_opt {
                                                if target_client_id == client_id {
                                                    info!(message_id = %args_str, "Requested resend for message");
                                                    stream.write_all(b"Resend requested\n").await.unwrap_or_else(|e| {
                                                        error!(message_id = %args_str, error = %e, "Failed to send resend response");
                                                    });
                                                } else {
                                                    error!(message_id = %args_str, "Resend request from wrong client");
                                                    stream.write_all(b"Unauthorized resend request\n").await.unwrap_or_else(|e| {
                                                        error!(message_id = %args_str, error = %e, "Failed to send response");
                                                    });
                                                }
                                            } else {
                                                error!(message_id = %args_str, "Message ID not found");
                                                stream.write_all(b"Message ID not found\n").await.unwrap_or_else(|e| {
                                                    error!(message_id = %args_str, error = %e, "Failed to send response");
                                                });
                                            }
                                        }
                                        Err(e) => {
                                            error!(message_id = %args_str, error = %e, "Failed to retrieve client_id for resend");
                                            stream.write_all(b"Error retrieving message metadata\n").await.unwrap_or_else(|e| {
                                                error!(message_id = %args_str, error = %e, "Failed to send response");
                                            });
                                        }
                                    }
                                } else {
                                    error!(message_id = %args_str, "Missing message ID for resend");
                                    stream.write_all(b"Missing message ID\n").await.unwrap_or_else(|e| {
                                        error!(message_id = %args_str, error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "register_public_key" => {
                                if !args_str.is_empty() {
                                    match state.read().await.save_public_key(&client_id, args_str).await {
                                        Ok(()) => {
                                            stream.write_all(b"Public key registered\n").await.unwrap_or_else(|e| {
                                                error!(error = %e, "Failed to send response");
                                            });
                                            info!("Public key registered for client {}", client_id);
                                        }
                                        Err(e) => {
                                            stream.write_all(format!("Error: {}\n", e).as_bytes()).await.unwrap_or_else(|e2| {
                                                error!(error = %e2, "Failed to send error response");
                                            });
                                            error!("Failed to register public key: {}", e);
                                        }
                                    }
                                } else {
                                    error!("Missing public key");
                                    stream.write_all(b"Missing public key\n").await.unwrap_or_else(|e| {
                                        error!(error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "get_public_key" => {
                                if !args_str.is_empty() {
                                    match state.read().await.get_public_key(args_str).await {
                                        Ok(Some(public_key)) => {
                                            stream.write_all(format!("Public key: {}\n", public_key).as_bytes()).await.unwrap_or_else(|e| {
                                                error!(client_id = %args_str, error = %e, "Failed to send response");
                                            });
                                            info!("Public key retrieved for client {}", args_str);
                                        }
                                        Ok(None) => {
                                            stream.write_all(b"Public key not found\n").await.unwrap_or_else(|e| {
                                                error!(client_id = %args_str, error = %e, "Failed to send response");
                                            });
                                            info!("Public key not found for client {}", args_str);
                                        }
                                        Err(e) => {
                                            stream.write_all(format!("Error: {}\n", e).as_bytes()).await.unwrap_or_else(|e2| {
                                                error!(client_id = %args_str, error = %e2, "Failed to send error response");
                                            });
                                            error!("Failed to get public key: {}", e);
                                        }
                                    }
                                } else {
                                    error!("Missing client ID for get_public_key");
                                    stream.write_all(b"Missing client ID\n").await.unwrap_or_else(|e| {
                                        error!(error = %e, "Failed to send response");
                                    });
                                }
                            }
                            _ => {
                                warn!(command = %command, "Unknown command received");
                                stream.write_all(b"Unknown command\n").await.unwrap_or_else(|e| {
                                    error!(command = %command, error = %e, "Failed to send response");
                                });
                            }
                        }
                        buffer.clear();
                    }
                    Ok(Err(e)) => {
                        error!(error = %e, "Error reading from stream");
                        state.write().await.decrement_client_count().await;
                        return;
                    }
                    Err(_) => {
                        continue;
                    }
                }
            }
            Some((message_id, message)) = rx.recv() => {
                let message_str = serde_json::to_string(&message).unwrap();
                if stream
                    .write_all(format!("Message: {} {}\n", message_id, message_str).as_bytes())
                    .await
                    .is_err()
                {
                    warn!(message_id = %message_id, "Failed to send message to client");
                    state.write().await.decrement_client_count().await;
                    return;
                }
                if let Some(mut status) = state.read().await.message_status.get_mut(&message_id) {
                    status.delivered(Instant::now());
                    let delivered_time = chrono::Utc::now().to_rfc3339();
                    if let Err(e) = state.read().await.storage.update_delivered_time(&message_id, &delivered_time).await {
                        error!("Failed to update delivered time for message {}: {}", message_id, e);
                    }
                }
                info!(message_id = %message_id, "Message delivered to client");
            }
        }
    }
}