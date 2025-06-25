use crate::state::{EncryptedInputData, ServerState};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{timeout, Duration};
use tracing::{error, info, warn, instrument};
use crate::connection::Connection;

#[instrument(skip(stream, state))]
pub async fn handle_client(mut stream: impl Connection, state: Arc<RwLock<ServerState>>) {
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
                        let start_time = Instant::now();
                        let request = String::from_utf8_lossy(&buffer).trim().to_string();
                        info!("Received request: {}", request);

                        let parts: Vec<&str> = request.splitn(2, ' ').collect();
                        let command = parts.get(0).unwrap_or(&"");
                        let args_str = parts.get(1).unwrap_or(&"");

                        match *command {
                            "declare_queue" => {
                                if !args_str.is_empty() {
                                    state.read().await.declare_queue(args_str);
                                    stream.write_all(b"Queue declared\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing queue name\n").await.unwrap();
                                }
                            }
                            "declare_exchange" => {
                                if !args_str.is_empty() {
                                    state.read().await.declare_exchange(args_str);
                                    stream.write_all(b"Exchange declared\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing queue name\n").await.unwrap();
                                }
                            }
                            "bind" => {
                                let args: Vec<&str> = args_str.splitn(3, ' ').collect();
                                if args.len() >= 3 {
                                    state.read().await.bind_queue(args[0], args[1], args[2]);
                                    stream.write_all(b"Queue bound\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing parameters\n").await.unwrap();
                                }
                            }
                            "publish" => {
                                let args: Vec<&str> = args_str.splitn(3, ' ').collect();
                                if args.len() >= 3 {
                                    let message_str = args[2];
                                    match serde_json::from_str::<EncryptedInputData>(message_str) {
                                        Ok(message) => {
                                            match state.read().await.publish(args[0], args[1], message.clone(), start_time) {
                                                Ok(()) => {
                                                    stream
                                                        .write_all(format!("ACK {}\n", message.message_id).as_bytes())
                                                        .await
                                                        .unwrap();
                                                }
                                                Err(e) => {
                                                    stream
                                                        .write_all(format!("Error: {}\n", e).as_bytes())
                                                        .await
                                                        .unwrap();
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            stream
                                                .write_all(format!("Invalid message format: {}\n", e).as_bytes())
                                                .await
                                                .unwrap();
                                        }
                                    }
                                } else {
                                    stream.write_all(b"Missing parameters\n").await.unwrap();
                                }
                            }
                            "publish_batch" => {
                                let args: Vec<&str> = args_str.splitn(3, ' ').collect();
                                if args.len() >= 3 {
                                    let exchange = args[0];
                                    let routing_key = args[1];
                                    let message_str = args[2];
                                    info!("Parsing batch JSON: {}", message_str);
                                    match serde_json::from_str::<Vec<EncryptedInputData>>(message_str) {
                                        Ok(messages) => {
                                            let mut acks = Vec::new();
                                            for message in messages {
                                                match state.read().await.publish(exchange, routing_key, message.clone(), start_time) {
                                                    Ok(()) => {
                                                        acks.push(format!("ACK {}", message.message_id));
                                                    }
                                                    Err(e) => {
                                                        acks.push(format!("Error: {} for message {}", e, message.message_id));
                                                    }
                                                }
                                            }
                                            let response = acks.join("\n") + "\n";
                                            stream.write_all(response.as_bytes()).await.unwrap();
                                        }
                                        Err(e) => {
                                            stream
                                                .write_all(format!("Invalid batch format: {}\n", e).as_bytes())
                                                .await
                                                .unwrap();
                                        }
                                    }
                                } else {
                                    stream.write_all(b"Missing parameters\n").await.unwrap();
                                }
                            }
                            "consume" => {
                                if !args_str.is_empty() {
                                    state.read().await.register_consumer(args_str, tx.clone());
                                    stream.write_all(b"Subscribed to queue\n").await.unwrap();
                                } else {
                                    stream.write_all(b"Missing queue name\n").await.unwrap();
                                }
                            }
                            "fetch" => {
                                if !args_str.is_empty() {
                                    if let Some((message_id, message)) = state.read().await.consume(args_str) {
                                        let message_str = serde_json::to_string(&message).unwrap();
                                        stream
                                            .write_all(format!("Message: {} {}\n", message_id, message_str).as_bytes())
                                            .await
                                            .unwrap();
                                    } else {
                                        stream.write_all(b"No messages\n").await.unwrap();
                                    }
                                } else {
                                    stream.write_all(b"Missing queue name\n").await.unwrap();
                                }
                            }
                            "ack" => {
                                if !args_str.is_empty() {
                                    info!("Processing ACK for message {}", args_str);
                                    state.read().await.acknowledge(args_str);
                                    stream
                                        .write_all(format!("ACK_CONFIRMED {}\n", args_str).as_bytes())
                                        .await
                                        .unwrap();
                                } else {
                                    stream.write_all(b"Missing message ID\n").await.unwrap();
                                }
                            }
                            "stats" => {
                                let stats = state.read().await.get_stats().await;
                                let stats_str = serde_json::to_string(&stats).unwrap();
                                stream
                                    .write_all(format!("Stats: {}\n", stats_str).as_bytes())
                                    .await
                                    .unwrap();
                            }
                            "reset_stats" => {
                                state.read().await.reset_stats();
                                stream.write_all(b"Stats reset\n").await.unwrap();
                            }
                            _ => {
                                stream.write_all(b"Unknown command\n").await.unwrap();
                            }
                        }
                        let duration = start_time.elapsed().as_secs_f64();
                        state.read().await.record_request_time(command, duration);
                        buffer.clear();
                    }
                    Ok(Err(e)) => {
                        error!("Error reading from stream: {}", e);
                        state.write().await.decrement_client_count().await;
                        return;
                    }
                    Err(_) => {
                        continue;
                    }
                }
            }
            Some((message_id, message)) = rx.recv() => {
                let start_time = Instant::now();
                let message_str = serde_json::to_string(&message).unwrap();
                if stream
                    .write_all(format!("Message: {} {}\n", message_id, message_str).as_bytes())
                    .await
                    .is_err()
                {
                    warn!("Failed to send message to client");
                    state.write().await.decrement_client_count().await;
                    return;
                }
                if let Some(mut status) = state.read().await.message_status.get_mut(&message_id) {
                    status.delivered(start_time);
                }
                let duration = start_time.elapsed().as_secs_f64();
                state.read().await.record_request_time("message_delivery", duration);
            }
        }
    }
}
