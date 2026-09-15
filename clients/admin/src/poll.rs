use crate::{AppState, NodeData};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

// ─── Persistent admin session ───────────────────────────────────

struct AdminSession {
    reader: BufReader<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>,
}

impl AdminSession {
    /// Establish TCP + TLS + read banner → ready for commands
    fn connect(addr: &str, config: &Arc<rustls::ClientConfig>) -> Result<Self, String> {
        // TCP
        let stream =
            TcpStream::connect(addr).map_err(|e| format!("TCP connect failed: {}", e))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;

        // TLS handshake
        let domain = rustls::pki_types::ServerName::try_from("localhost".to_string())
            .map_err(|e| e.to_string())?;
        let conn = rustls::ClientConnection::new(config.clone(), domain)
            .map_err(|e| format!("TLS error: {}", e))?;
        let mut tls = rustls::StreamOwned::new(conn, stream);

        // Read banner byte-by-byte (safe: no BufReader that could lose data)
        let mut banner = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match tls.read(&mut byte) {
                Ok(0) => return Err("server closed during banner".into()),
                Err(e) => return Err(format!("banner read error: {}", e)),
                Ok(_) => {
                    banner.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
            }
        }

        let banner_str = String::from_utf8_lossy(&banner);
        info!("Connected. Banner: {}", banner_str.trim());

        // Wrap in BufReader for efficient line-based I/O
        Ok(Self {
            reader: BufReader::new(tls),
        })
    }

    /// Send a command and read one line of response
    fn query(&mut self, command: &str) -> Result<String, String> {
        let cmd_bytes = format!("{}\n", command);
        self.reader
            .get_mut()
            .write_all(cmd_bytes.as_bytes())
            .map_err(|e| format!("write error: {}", e))?;
        self.reader
            .get_mut()
            .flush()
            .map_err(|e| format!("flush error: {}", e))?;

        let mut response = String::new();
        self.reader
            .read_line(&mut response)
            .map_err(|e| format!("read error: {}", e))?;

        if response.is_empty() {
            return Err("server closed connection".into());
        }

        Ok(response.trim().to_string())
    }
}

// ─── Persistent polling loop (runs in background thread) ────────

pub fn poll_loop(
    addr: String,
    interval_secs: u64,
    tls_config: Arc<rustls::ClientConfig>,
    state: Arc<AppState>,
) {
    loop {
        match run_session(&addr, &tls_config, &state, interval_secs) {
            Ok(()) => info!("Session ended normally"),
            Err(e) => warn!("Session ended: {}", e),
        }

        // Mark unreachable
        if let Ok(mut node) = state.node.write() {
            node.reachable = false;
            node.server.health = "unreachable".into();
            node.last_update = chrono::Local::now()
                .format("%H:%M:%S")
                .to_string();
        }

        // Reconnect after short delay
        info!("Reconnecting in 2 seconds...");
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Run one persistent session: connect → poll repeatedly until error
fn run_session(
    addr: &str,
    tls_config: &Arc<rustls::ClientConfig>,
    state: &Arc<AppState>,
    interval_secs: u64,
) -> Result<(), String> {
    let mut session = AdminSession::connect(addr, tls_config)?;
    info!("Persistent admin session established");

    loop {
        match session.query("json") {
            Ok(response) => match serde_json::from_str::<NodeData>(&response) {
                Ok(mut data) => {
                    data.reachable = true;
                    data.last_update = chrono::Local::now()
                        .format("%H:%M:%S")
                        .to_string();
                    if let Ok(mut node) = state.node.write() {
                        *node = data;
                    }
                }
                Err(e) => {
                    let preview = if response.len() > 120 {
                        &response[..120]
                    } else {
                        &response
                    };
                    warn!("JSON parse error: {} — got: {}", e, preview);
                }
            },
            Err(e) => {
                error!("Query failed: {}", e);
                return Err(e);
            }
        }

        std::thread::sleep(Duration::from_secs(interval_secs));
    }
}
