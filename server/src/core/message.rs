use serde::{Deserialize, Serialize};
use uuid::Uuid; // For MessageStatus

// Original EncryptedInputData from old state.rs
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EncryptedInputData {
    pub data: String, // Base64 encoded encrypted data
}

// Original MessageStatus from old state.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageStatus {
    pub message_id: Uuid,
    pub data: EncryptedInputData, 
    pub timestamp: i64,          
    pub attempts: u32,
}
