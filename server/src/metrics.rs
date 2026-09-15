use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ServerSnapshot {
    pub connected_clients: usize,
    pub queue_count: usize,
    pub total_messages: usize,
    pub consumer_count: usize,
}

pub struct Metrics {
    pub messages_published: AtomicU64,
    pub messages_acked: AtomicU64,
    pub messages_delivered: AtomicU64,
    pub errors_total: AtomicU64,
    pub connections_total: AtomicU64,
    pub connections_rejected_rate_limit: AtomicU64,
    pub connections_rejected_conn_limit: AtomicU64,
    pub requests_rate_limited: AtomicU64,
    pub snapshot: Arc<RwLock<ServerSnapshot>>,
    pub start_time: Instant,
}

impl Default for Metrics { fn default() -> Self { Self::new() } }
impl Metrics {
    pub fn new() -> Self {
        Self { messages_published: AtomicU64::new(0), messages_acked: AtomicU64::new(0), messages_delivered: AtomicU64::new(0), errors_total: AtomicU64::new(0), connections_total: AtomicU64::new(0), connections_rejected_rate_limit: AtomicU64::new(0), connections_rejected_conn_limit: AtomicU64::new(0), requests_rate_limited: AtomicU64::new(0), snapshot: Arc::new(RwLock::new(ServerSnapshot::default())), start_time: Instant::now() }
    }
    #[allow(dead_code)]
    pub async fn update_snapshot(&self, snapshot: ServerSnapshot) { *self.snapshot.write().await = snapshot; }
    pub async fn render_prometheus(&self) -> String {
        let s = self.snapshot.read().await;
        let mut out = String::new();
        metric(&mut out, "ciphermq_messages_published_total", "Total messages published", self.messages_published.load(Ordering::Relaxed), "counter");
        metric(&mut out, "ciphermq_messages_acked_total", "Total messages acknowledged", self.messages_acked.load(Ordering::Relaxed), "counter");
        metric(&mut out, "ciphermq_messages_delivered_total", "Total messages delivered", self.messages_delivered.load(Ordering::Relaxed), "counter");
        metric(&mut out, "ciphermq_errors_total", "Total server errors", self.errors_total.load(Ordering::Relaxed), "counter");
        metric(&mut out, "ciphermq_connections_total", "Accepted client connections", self.connections_total.load(Ordering::Relaxed), "counter");
        metric(&mut out, "ciphermq_connections_rejected_rate_limit_total", "Connections rejected by rate limit", self.connections_rejected_rate_limit.load(Ordering::Relaxed), "counter");
        metric(&mut out, "ciphermq_connections_rejected_conn_limit_total", "Connections rejected by connection limit", self.connections_rejected_conn_limit.load(Ordering::Relaxed), "counter");
        metric(&mut out, "ciphermq_requests_rate_limited_total", "Requests rejected by rate limit", self.requests_rate_limited.load(Ordering::Relaxed), "counter");
        metric(&mut out, "ciphermq_connected_clients", "Current connected clients", s.connected_clients as u64, "gauge");
        metric(&mut out, "ciphermq_queue_count", "Current queue count", s.queue_count as u64, "gauge");
        metric(&mut out, "ciphermq_messages_in_queues", "Messages currently in queues", s.total_messages as u64, "gauge");
        metric(&mut out, "ciphermq_consumer_count", "Current consumers", s.consumer_count as u64, "gauge");
        metric(&mut out, "ciphermq_uptime_seconds", "Server uptime", self.start_time.elapsed().as_secs(), "gauge");
        out
    }
    pub async fn render_status(&self) -> String {
        let s = self.snapshot.read().await;
        format!("=== CipherMQ Node Status ===\nConnected clients: {}\nQueues:            {}\nMessages in queues: {}\nConsumers:          {}\nUptime:             {}s\nPublished:          {}\nAcknowledged:       {}\nRate limited:       {}\n", s.connected_clients, s.queue_count, s.total_messages, s.consumer_count, self.start_time.elapsed().as_secs(), self.messages_published.load(Ordering::Relaxed), self.messages_acked.load(Ordering::Relaxed), self.requests_rate_limited.load(Ordering::Relaxed))
    }
}
fn metric(out: &mut String, name: &str, help: &str, value: u64, kind: &str) { out.push_str(&format!("# HELP {} {}\n# TYPE {} {}\n{} {}\n", name, help, name, kind, name, value)); }
