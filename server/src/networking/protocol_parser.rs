// use crate::state::EncryptedInputData; // Old path
use crate::core::message::EncryptedInputData; // New path from core module
use serde_json; // Needed for parsing in PUBLISH

#[derive(Debug)]
pub enum Command {
    DeclareQueue(String),
    DeclareExchange(String),
    Bind {
        exchange_name: String,
        queue_name: String,
        routing_key: String,
    },
    Publish {
        exchange_name: String,
        routing_key: String,
        message: EncryptedInputData,
    },
    Consume(String), // Queue name
    Fetch(String),   // Queue name
    Ack(String),     // Message ID
    Unknown,
    Invalid,
}

pub fn parse_request(request: &str) -> Command {
    let parts: Vec<&str> = request.trim().splitn(4, ' ').collect();
    match parts.get(0) {
        Some(&"DECLARE") => match parts.get(1) {
            Some(&"QUEUE") => parts
                .get(2)
                .map(|name| Command::DeclareQueue(name.to_string()))
                .unwrap_or(Command::Invalid),
            Some(&"EXCHANGE") => parts
                .get(2)
                .map(|name| Command::DeclareExchange(name.to_string()))
                .unwrap_or(Command::Invalid),
            _ => Command::Invalid,
        },
        Some(&"BIND") => {
            if parts.len() == 4 {
                let exchange_name = parts.get(1).unwrap_or(&"").to_string();
                let queue_name = parts.get(2).unwrap_or(&"").to_string();
                let routing_key = parts.get(3).unwrap_or(&"").to_string();
                if exchange_name.is_empty() || queue_name.is_empty() || routing_key.is_empty() {
                    Command::Invalid
                } else {
                    Command::Bind {
                        exchange_name,
                        queue_name,
                        routing_key,
                    }
                }
            } else {
                Command::Invalid
            }
        }
        Some(&"PUBLISH") => {
            if parts.len() >= 4 {
                let exchange_name = parts.get(1).unwrap_or(&"").to_string();
                let routing_key = parts.get(2).unwrap_or(&"").to_string();
                let message_json = parts.get(3).unwrap_or(&"");

                if exchange_name.is_empty() || routing_key.is_empty() || message_json.is_empty() {
                    return Command::Invalid;
                }

                match serde_json::from_str::<EncryptedInputData>(message_json) {
                    Ok(message) => Command::Publish {
                        exchange_name,
                        routing_key,
                        message,
                    },
                    Err(_) => Command::Invalid, 
                }
            } else {
                Command::Invalid
            }
        }
        Some(&"CONSUME") => parts
            .get(1)
            .map(|name| Command::Consume(name.to_string()))
            .unwrap_or(Command::Invalid),
        Some(&"FETCH") => parts 
            .get(1)
            .map(|name| Command::Fetch(name.to_string()))
            .unwrap_or(Command::Invalid),
        Some(&"ACK") => parts
            .get(1)
            .map(|id| Command::Ack(id.to_string()))
            .unwrap_or(Command::Invalid),
        _ => Command::Unknown,
    }
}
