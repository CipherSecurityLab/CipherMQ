// networking module - for handling network protocols, client connections, command parsing

pub mod protocol_parser;
pub mod client_handler;

// Re-exports removed as they are reported as unused.
// Items will be accessed via their full paths like crate::networking::protocol_parser::Command
// pub use protocol_parser::{Command, parse_request};
// pub use client_handler::handle_client;
