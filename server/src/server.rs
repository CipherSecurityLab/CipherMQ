use crate::state::{EncryptedInputData, ServerState};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, warn, instrument};
use crate::connection::Connection;
use tokio::io::AsyncWriteExt;
use crate::HeartbeatMonitor;
use crate::acl::{AclManager, AuthResult};
use crate::{metrics::Metrics, rate_limiter::RateLimiter};

#[instrument(skip(stream, state, heartbeat_monitor, metrics, rate_limiter, acl_manager), fields(client_id = %stream.client_id()))]
pub async fn handle_client(
    mut stream: impl Connection,
    state: Arc<ServerState>,
    heartbeat_monitor: Arc<HeartbeatMonitor>,
    metrics: Arc<Metrics>,
    rate_limiter: Arc<RateLimiter>,
    ip: String,
    acl_manager: AclManager,
) {
    let client_id = stream.client_id().to_string();

    // ── ACL: resolve role from CN ──────────────────────────────────────────
    let role = acl_manager.resolve_role(&client_id);
    info!(client_id = %client_id, role = %role.as_str(), "Client role resolved via ACL");

    if role == crate::acl::Role::Unknown {
        warn!(client_id = %client_id, "Unknown CN rejected by ACL");
        let _ = stream.write_all(b"Access denied: unrecognized client identity\n").await;
        return;
    }

    if rate_limiter.check_connection(stream.client_id(), &ip).await.is_allowed() == false { metrics.connections_rejected_conn_limit.fetch_add(1, std::sync::atomic::Ordering::Relaxed); return; }
    metrics.connections_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    state.increment_client_count(); // Atomic operation, lock-free
    heartbeat_monitor.record_heartbeat(&client_id); // Record initial heartbeat
    let mut buffer = Vec::with_capacity(8196);
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, EncryptedInputData)>();

    loop {
        tokio::select! {
            result = timeout(Duration::from_secs(600), stream.read_buf(&mut buffer)) => {
                match result {
                    Ok(Ok(n)) if n == 0 => {
                        info!(client_id = %client_id, "Client disconnected");
                        state.decrement_client_count();
                        heartbeat_monitor.remove_client(&client_id);
                        return;
                    }
                    Ok(Ok(_)) => {
                        heartbeat_monitor.record_heartbeat(&client_id);
                        let request = String::from_utf8_lossy(&buffer).trim().to_string();
                        info!(request = %request, client_id = %client_id, role = %role.as_str(), "Received request");

                        let parts: Vec<&str> = request.splitn(2, ' ').collect();
                        let command = parts.get(0).unwrap_or(&"");
                        let args_str = parts.get(1).unwrap_or(&"");

                        // ── ACL: command-level permission check ───────────────
                        match acl_manager.check_command(&role, command) {
                            AuthResult::Allowed => { /* proceed */ }
                            AuthResult::Denied(reason) => {
                                warn!(
                                    client_id = %client_id,
                                    role = %role.as_str(),
                                    command = %command,
                                    reason = %reason,
                                    "ACL: command denied"
                                );
                                metrics.requests_rate_limited.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let _ = stream.write_all(format!("Access denied: {} (role={})\n", reason, role.as_str()).as_bytes()).await;
                                buffer.clear();
                                continue;
                            }
                        }

                        if rate_limiter.check(stream.client_id(), &ip, *command == "publish" || *command == "publish_batch").await.is_allowed() == false { metrics.requests_rate_limited.fetch_add(1, std::sync::atomic::Ordering::Relaxed); let _=stream.write_all(b"Rate limited\n").await; buffer.clear(); continue; }
                        match *command {
                            "declare_queue" => {
                                if !args_str.is_empty() {
                                    // ── ACL: resource-level check for receivers ──
                                    if let AuthResult::Denied(reason) = acl_manager.check_resource(&role, &client_id, "queue", args_str) {
                                        warn!(client_id = %client_id, queue = %args_str, reason = %reason, "ACL: resource denied");
                                        let _ = stream.write_all(format!("Access denied: {}\n", reason).as_bytes()).await;
                                        buffer.clear();
                                        continue;
                                    }
                                    state.declare_queue(args_str);
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
                                    state.declare_exchange(args_str);
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
                                    // ── ACL: resource-level check for receivers ──
                                    if let AuthResult::Denied(reason) = acl_manager.check_resource(&role, &client_id, "queue", args[0]) {
                                        warn!(client_id = %client_id, queue = %args[0], reason = %reason, "ACL: resource denied for bind");
                                        let _ = stream.write_all(format!("Access denied: {}\n", reason).as_bytes()).await;
                                        buffer.clear();
                                        continue;
                                    }
                                    state.bind_queue(args[0], args[1], args[2]);
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
                                            match state.publish(args[0], args[1], message.clone(), client_id.clone(), Instant::now()).await {
                                                Ok(()) => {
                                                    metrics.messages_published.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                                                match state.publish(exchange, routing_key, message.clone(), client_id.clone(), Instant::now()).await {
                                                    Ok(()) => {
                                                    metrics.messages_published.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                                    state.register_consumer(args_str, tx.clone());
                                    info!(queue_name = %args_str, "Consumer registered successfully");
                                } else {
                                    error!(queue_name = %args_str, "Missing queue name");
                                    stream.write_all(b"Missing queue name\n").await.unwrap_or_else(|e| {
                                        error!(queue_name = %args_str, error = %e, "Failed to send response");
                                    });
                                }
                            }
                            "ack" => {
                                if !args_str.is_empty() {
                                    state.acknowledge(args_str).await;
                                    metrics.messages_acked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if let Err(e) = stream.write_all(format!("ACK confirmed {}\n", args_str).as_bytes()).await {
                                        error!(message_id = %args_str, error = %e, "Failed to send ACK response due to closed connection");
                                        state.decrement_client_count();
                                        return;
                                    }
                                    if let Err(e) = stream.flush().await {
                                        error!(message_id = %args_str, error = %e, "Failed to flush stream");
                                        state.decrement_client_count();
                                        return;
                                    }
                                    debug!("Sent ACK confirmation for message {}", args_str);
                                } else {
                                    error!(message_id = %args_str, "Missing message ID for ack");
                                    if let Err(e) = stream.write_all(b"Missing message ID\n").await {
                                        error!(message_id = %args_str, error = %e, "Failed to send response");
                                        state.decrement_client_count();
                                        return;
                                    }
                                }
                            }
                            "resend" => {
                                if !args_str.is_empty() {
                                    match state.storage.get_client_id_for_message(args_str).await {
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
                                    match state.save_public_key(&client_id, args_str).await {
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
                                    match state.get_public_key(args_str).await {
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
                            "heartbeat" => {
                                    heartbeat_monitor.record_heartbeat(&client_id);
                                    stream.write_all(b"Heartbeat received\n").await.unwrap_or_else(|e| {
                                        error!(client_id = %client_id, error = %e, "Failed to send heartbeat response");
                                    });
                                    info!("Heartbeat processed for client {}", client_id);
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
                        if e.kind() == std::io::ErrorKind::UnexpectedEof {
                            info!(client_id = %client_id, "Client disconnected (TLS closed)");
                        } else {
                            error!(client_id = %client_id, error = %e, "Error reading from stream");
                        }
                        state.decrement_client_count();
                        heartbeat_monitor.remove_client(&client_id);
                        return;
                    }
                    Err(_) => {
                        continue;
                    }
                }
            }
            Some((message_id, message)) = rx.recv() => {
                // ✅ Check that the message has not already been acknowledged.
                if !state.message_status.contains_key(&message_id) {
                    continue; // It was already removed; skip it.
                }
                
                let should_deliver = state.message_status
                    .get(&message_id)
                    .map(|status| status.can_deliver())
                    .unwrap_or(false);

                if !should_deliver {
                    info!(message_id = %message_id, "Skipping delivery of already acknowledged message");
                    continue;
                }

                let message_str = serde_json::to_string(&message).unwrap();
                let full_message = format!("Message: {} {}\n", message_id, message_str);
                
                // ✅ Improvement: retry logic for delivery.
                let mut retry_count = 0;
                const MAX_RETRIES: usize = 3;
                
                while retry_count < MAX_RETRIES {
                    match stream.write_all(full_message.as_bytes()).await {
                        Ok(_) => {
                            // ✅ Success: flush the stream.
                            if let Err(e) = stream.flush().await {
                                warn!(message_id = %message_id, error = %e, "Failed to flush after write");
                                retry_count += 1;
                                tokio::time::sleep(Duration::from_millis(10)).await;
                                continue;
                            }
                            
                            // ✅ Update the delivery status.
                            if let Some(mut status) = state.message_status.get_mut(&message_id) {
                                if status.delivered_time.is_none() { 
                                    status.delivered(Instant::now());
                                    let delivered_time = chrono::Utc::now().to_rfc3339();
                                    state.storage.update_delivered_time_async(&message_id, &delivered_time).await;
                                    info!(message_id = %message_id, "Message delivered to client");
                                }
                            }
                            break; // Success.
                        }
                        Err(e) => {
                            warn!(
                                message_id = %message_id, 
                                error = %e, 
                                retry = retry_count + 1,
                                "Failed to send message to client, retrying"
                            );
                            retry_count += 1;
                            
                            if retry_count >= MAX_RETRIES {
                                error!(message_id = %message_id, "Failed to deliver message after {} retries", MAX_RETRIES);
                                state.decrement_client_count();
                                return;
                            }
                            
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                }
                // ✅ OPTIMIZED: update message delivery status (async, non-blocking).
                if let Some(mut status) = state.message_status.get_mut(&message_id) {
                    if status.delivered_time.is_none() { 
                        status.delivered(Instant::now());
                        let delivered_time = chrono::Utc::now().to_rfc3339();
                        // ✅ Fire-and-forget: no await needed for performance
                        state.storage.update_delivered_time_async(&message_id, &delivered_time).await;
                        info!(message_id = %message_id, "Message delivered to client");
                    }
                }
            }
            else => {
                warn!("Consumer channel closed unexpectedly for client {}", client_id);
                state.decrement_client_count();
                heartbeat_monitor.remove_client(&client_id); // Remove from heartbeat monitor
                return;
            }
        }
    }
}
