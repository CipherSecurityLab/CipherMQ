use crate::networking::protocol_parser::{parse_request, Command};
use crate::core::message::{EncryptedInputData, MessageStatus};
// use crate::core::state::ServerState; // Old: direct use of ServerState
use crate::broker::engine::BrokerEngine; // New: use BrokerEngine

use serde_json;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info};

async fn handle_declare_queue_cmd(
    writer: &mut (impl AsyncWriteExt + Unpin),
    engine: &Arc<BrokerEngine>, // Changed from state to engine
    queue_name: String,
) -> tokio::io::Result<()> {
    engine.declare_queue(queue_name); // Call on engine
    writer.write_all(b"Queue declared\n").await?;
    Ok(())
}

async fn handle_declare_exchange_cmd(
    writer: &mut (impl AsyncWriteExt + Unpin),
    engine: &Arc<BrokerEngine>, // Changed from state to engine
    exchange_name: String,
) -> tokio::io::Result<()> {
    engine.declare_exchange(exchange_name); // Call on engine
    writer.write_all(b"Exchange declared\n").await?;
    Ok(())
}

async fn handle_bind_cmd(
    writer: &mut (impl AsyncWriteExt + Unpin),
    engine: &Arc<BrokerEngine>, // Changed from state to engine
    exchange_name: String,
    queue_name: String,
    routing_key: String,
) -> tokio::io::Result<()> {
    engine.bind_queue(exchange_name.clone(), queue_name.clone(), routing_key.clone()); // Call on engine
    let response = format!(
        "Queue {} bound to exchange {} with key {}\n",
        queue_name, exchange_name, routing_key
    );
    writer.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn handle_publish_cmd(
    writer: &mut (impl AsyncWriteExt + Unpin),
    engine: &Arc<BrokerEngine>, // Changed from state to engine
    exchange_name: String,
    routing_key: String,
    message: EncryptedInputData,
) -> tokio::io::Result<()> {
    match engine // Call on engine
        .publish(exchange_name.clone(), routing_key.clone(), message)
        .await
    {
        Ok(message_id) => {
            let response = format!("ACK {}\n", message_id);
            writer.write_all(response.as_bytes()).await?;
        }
        Err(e) => {
            let response = format!("Error: {}\n", e);
            writer.write_all(response.as_bytes()).await?;
        }
    }
    Ok(())
}

async fn handle_ack_cmd(
    writer: &mut (impl AsyncWriteExt + Unpin),
    engine: &Arc<BrokerEngine>, // Changed from state to engine
    message_id_str: String,
) -> tokio::io::Result<()> {
    engine.acknowledge(message_id_str.clone()); // Call on engine
    let response = format!("ACK_CONFIRMED {}\n", message_id_str);
    writer.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn handle_consume_cmd(
    writer: &mut (impl AsyncWriteExt + Unpin),
    engine: &Arc<BrokerEngine>, // Changed from state to engine
    queue_name: String,
    consumer_tx: mpsc::Sender<MessageStatus>, 
) -> tokio::io::Result<()> {
    match engine.register_consumer(queue_name.clone(), consumer_tx).await { // Call on engine
        Ok(_) => {
            info!("Consumer registered for queue {}. Client waiting for messages...", queue_name);
            writer.write_all(b"Subscribed to queue\n").await?;
        }
        Err(e) => {
            error!("Failed to register consumer for queue {}: {}", queue_name, e);
            let error_msg = format!("Error registering consumer: {}\n", e);
            writer.write_all(error_msg.as_bytes()).await?;
        }
    }
    Ok(())
}

async fn handle_fetch_cmd(
    writer: &mut (impl AsyncWriteExt + Unpin),
    engine: &Arc<BrokerEngine>, // Changed from state to engine
    queue_name: String,
) -> tokio::io::Result<()> {
    match engine.fetch_one_message(&queue_name) { // Call on engine
        Some(message_status) => { 
            match serde_json::to_string(&message_status) {
                Ok(json_response) => {
                    let response = format!("{}\n", json_response);
                    writer.write_all(response.as_bytes()).await?;
                }
                Err(e) => {
                    error!("Failed to serialize fetched message for queue {}: {}", queue_name, e);
                    writer.write_all(b"Error: Failed to serialize message on server\n").await?;
                }
            }
        }
        None => {
            writer.write_all(b"No messages\n").await?;
        }
    }
    Ok(())
}

pub async fn handle_client(stream: TcpStream, engine: Arc<BrokerEngine>) { // Changed state to engine
    let peer_addr = match stream.peer_addr() {
        Ok(addr) => addr.to_string(),
        Err(_) => "unknown".to_string(),
    };
    info!("Client connected: {}", peer_addr);

    let (reader_half, mut writer_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader_half);
    let mut line = String::new();

    let (client_consumer_tx, mut client_consumer_rx) = mpsc::channel::<MessageStatus>(10);

    loop {
        tokio::select! {
            read_result = timeout(Duration::from_secs(300), reader.read_line(&mut line)) => {
                match read_result {
                    Ok(Ok(0)) | Ok(Err(_)) => {
                        info!("Client {} disconnected or read error.", peer_addr);
                        break;
                    }
                    Ok(Ok(_n)) => {
                        debug!("Client {}: Received command: {}", peer_addr, line.trim());
                        let command = parse_request(line.trim());
                        let mut should_break = false;

                        let cmd_result = match command {
                            Command::DeclareQueue(queue_name) => {
                                handle_declare_queue_cmd(&mut writer_half, &engine, queue_name).await // Pass engine
                            }
                            Command::DeclareExchange(exchange_name) => {
                                handle_declare_exchange_cmd(&mut writer_half, &engine, exchange_name).await // Pass engine
                            }
                            Command::Bind { exchange_name, queue_name, routing_key } => {
                                handle_bind_cmd(&mut writer_half, &engine, exchange_name, queue_name, routing_key).await // Pass engine
                            }
                            Command::Publish { exchange_name, routing_key, message } => {
                                handle_publish_cmd(&mut writer_half, &engine, exchange_name, routing_key, message).await // Pass engine
                            }
                            Command::Consume(queue_name) => {
                                handle_consume_cmd(&mut writer_half, &engine, queue_name, client_consumer_tx.clone()).await // Pass engine
                            }
                            Command::Fetch(queue_name) => {
                                handle_fetch_cmd(&mut writer_half, &engine, queue_name).await // Pass engine
                            }
                            Command::Ack(message_id_str) => {
                                handle_ack_cmd(&mut writer_half, &engine, message_id_str).await // Pass engine
                            }
                            Command::Unknown => {
                                writer_half.write_all(b"Unknown command\n").await
                            }
                            Command::Invalid => {
                                writer_half.write_all(b"Invalid command format\n").await
                            }
                        };

                        if let Err(e) = cmd_result {
                            error!("Client {}: Failed to process command or send response: {}", peer_addr, e);
                            should_break = true; 
                        }

                        if !should_break {
                            if let Err(e) = writer_half.flush().await {
                                error!("Client {}: Failed to flush writer: {}", peer_addr, e);
                                should_break = true;
                            }
                        }
                        line.clear(); 
                        if should_break { break; }
                    }
                    Err(e) => { 
                        info!("Client {} connection timed out due to inactivity: {}", peer_addr, e);
                        break;
                    }
                }
            },
            Some(message_status) = client_consumer_rx.recv() => {
                match serde_json::to_string(&message_status) {
                    Ok(json_response) => {
                        debug!("Client {}: Sending message {} to consumer", peer_addr, message_status.message_id);
                        if let Err(e) = writer_half.write_all(format!("{}\n", json_response).as_bytes()).await {
                            error!("Client {}: Failed to send message to consumer: {}. Message ID: {}", peer_addr, e, message_status.message_id);
                            break; 
                        }
                        if let Err(e) = writer_half.flush().await {
                             error!("Client {}: Failed to flush writer after sending consumed message: {}", peer_addr, e);
                             break;
                        }
                    }
                    Err(e) => {
                        error!("Client {}: Failed to serialize message for consumer: {}. Message ID: {}", peer_addr, e, message_status.message_id);
                    }
                }
            }
        }
    }
    info!("Client {} disconnected.", peer_addr);
}
