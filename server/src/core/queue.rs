use crate::core::message::MessageStatus; // Updated path
use std::collections::VecDeque;
use tokio::sync::mpsc;

// Original Queue struct from old state.rs
#[derive(Debug, Clone)]
pub struct Queue {
    pub name: String,
    pub messages: VecDeque<MessageStatus>,
    pub consumers: Vec<mpsc::Sender<MessageStatus>>, // This was mpsc::Sender<MessageStatus>
                                                  // In original state.rs, Queue::consumers was Vec<mpsc::Sender<MessageStatus>>
                                                  // This seems correct based on the previous refactoring.
}

impl Queue {
    pub fn new(name: String) -> Self {
        Queue {
            name,
            messages: VecDeque::new(),
            consumers: Vec::new(),
        }
    }
}
