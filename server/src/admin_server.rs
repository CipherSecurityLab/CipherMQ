use crate::{metrics::Metrics, rate_limiter::RateLimiter, state::ServerState};
use crate::acl::AclManager;
use std::fs::File;
use std::io::BufReader as StdBufReader;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    RootCertStore, ServerConfig,
};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};
use x509_parser::prelude::*;
use x509_parser::oid_registry::OID_X509_COMMON_NAME;

pub async fn serve(
    address: String,
    cert_path: String,
    key_path: String,
    ca_path: String,
    _allowed_cns: Vec<String>,
    metrics: Arc<Metrics>,
    limiter: Arc<RateLimiter>,
    state: Arc<ServerState>,
    acl_manager: AclManager,
) -> Result<(), String> {
    let listener = TcpListener::bind(&address)
        .await
        .map_err(|e| e.to_string())?;
    let acceptor = build_acceptor(&cert_path, &key_path, &ca_path)
        .map_err(|e| e.to_string())?;

    info!("Admin console listening on {}", address);

    loop {
        let (stream, addr) = listener.accept().await.map_err(|e| e.to_string())?;
        let acceptor = acceptor.clone();
        let metrics = metrics.clone();
        let limiter = limiter.clone();
        let state = state.clone();
        let acl_manager = acl_manager.clone();

        tokio::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    error!("Admin TLS handshake from {} failed: {}", addr, e);
                    return;
                }
            };

            // ── ACL: verify client CN is an admin ──────────────────────────
            let client_cn = extract_admin_cn(&stream);
            let role = acl_manager.resolve_role(&client_cn);
            if role != crate::acl::Role::Admin {
                warn!(cn = %client_cn, addr = %addr, "Non-admin CN rejected by admin server ACL");
                return;
            }
            info!(cn = %client_cn, addr = %addr, "Admin client authenticated via ACL");

            let (rd, mut wr) = tokio::io::split(stream);
            let mut reader = BufReader::new(rd);

            // ── banner: single line ──────────────────────────────────────
            if wr.write_all(b"CipherMQ Admin\n").await.is_err() {
                return;
            }
            if wr.flush().await.is_err() {
                return;
            }

            // ── command loop ─────────────────────────────────────────────
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let cmd = line.trim();
                        if cmd == "quit" || cmd == "exit" {
                            break;
                        }

                        let response = match cmd {
                            "json" => json_status(&metrics, &limiter, &state).await,
                            "metrics" => metrics.render_prometheus().await,
                            "status" => metrics.render_status().await,
                            "connections" => {
                                let (ip, cn) = limiter.connection_stats().await;
                                format!("By IP: {:?}\nBy CN: {:?}\n", ip, cn)
                            }
                            "queues" => format!(
                                "Queues: {}\nMessages: {}\nConsumers: {}\n",
                                state.queues.len(),
                                state
                                    .queues
                                    .iter()
                                    .map(|q| q.value().len())
                                    .sum::<usize>(),
                                state
                                    .consumers
                                    .iter()
                                    .map(|c| c.value().len())
                                    .sum::<usize>()
                            ),
                            _ => "Unknown command\n".to_string(),
                        };

                        if wr
                            .write_all(format!("{}\n", response).as_bytes())
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if wr.flush().await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Admin read error: {}", e);
                        break;
                    }
                }
            }
        });
    }
}

// ── JSON status for the admin client ──────────────────────────────────────

async fn json_status(
    m: &Arc<Metrics>,
    l: &Arc<RateLimiter>,
    s: &Arc<ServerState>,
) -> String {
    let snapshot = m.snapshot.read().await.clone();
    let (ip, cn) = l.connection_stats().await;

    // Collect all queues
    let queues: Vec<_> = s.queues.iter().map(|q| {
        let consumers = s.consumers.get(q.key()).map(|c| c.len()).unwrap_or(0);
        serde_json::json!({
            "name": q.key(),
            "message_count": q.value().len(),
            "consumer_count": consumers
        })
    }).collect();

    // Collect all bindings
    let bindings: Vec<_> = s.bindings.iter().flat_map(|entry| {
        let exchange = entry.key().clone();
        let pairs: Vec<(String, String)> = entry.value().iter()
            .map(|(q, r)| (q.clone(), r.clone()))
            .collect();
        pairs.into_iter().map(move |(queue, routing_key)| {
            serde_json::json!({
                "exchange": exchange,
                "queue": queue,
                "routing_key": routing_key
            })
        })
    }).collect();

    // Collect exchanges
    let exchanges: Vec<String> = s.exchanges.iter().map(|e| e.key().clone()).collect();

    // Total messages across all queues
    let total_messages: usize = s.queues.iter().map(|q| q.value().len()).sum();
    let total_consumers: usize = s.consumers.iter().map(|c| c.value().len()).sum();

    serde_json::json!({
        "server": {
            "name": "CipherMQ Standalone",
            "uptime_secs": m.start_time.elapsed().as_secs(),
            "health": "healthy",
            "max_queue_size": s.max_queue_size
        },
        "counters": {
            "messages_published": m.messages_published.load(std::sync::atomic::Ordering::Relaxed),
            "messages_acked": m.messages_acked.load(std::sync::atomic::Ordering::Relaxed),
            "messages_delivered": m.messages_delivered.load(std::sync::atomic::Ordering::Relaxed),
            "errors_total": m.errors_total.load(std::sync::atomic::Ordering::Relaxed),
            "connections_total": m.connections_total.load(std::sync::atomic::Ordering::Relaxed),
            "connections_rejected_rate_limit": m.connections_rejected_rate_limit.load(std::sync::atomic::Ordering::Relaxed),
            "connections_rejected_conn_limit": m.connections_rejected_conn_limit.load(std::sync::atomic::Ordering::Relaxed),
            "requests_rate_limited": m.requests_rate_limited.load(std::sync::atomic::Ordering::Relaxed)
        },
        "snapshot": {
            "connected_clients": snapshot.connected_clients,
            "queue_count": snapshot.queue_count,
            "total_messages": snapshot.total_messages,
            "consumer_count": snapshot.consumer_count
        },
        "live": {
            "connected_clients": s.connected_clients.load(std::sync::atomic::Ordering::Relaxed),
            "queues": queues.len(),
            "total_messages": total_messages,
            "total_consumers": total_consumers,
            "exchanges": exchanges.len()
        },
        "connection_stats": {
            "active_by_ip": ip.values().sum::<usize>(),
            "active_by_cn": cn.values().sum::<usize>(),
            "by_ip": ip,
            "by_cn": cn
        },
        "queues": queues,
        "bindings": bindings,
        "exchanges": exchanges
    })
    .to_string()
}

// ── mTLS acceptor builder ─────────────────────────────────────────────────

fn build_acceptor(
    cert: &str,
    key: &str,
    ca: &str,
) -> Result<TlsAcceptor, Box<dyn std::error::Error + Send + Sync>> {
    let mut r = StdBufReader::new(File::open(cert)?);
    let certs: Vec<CertificateDer> =
        rustls_pemfile::certs(&mut r).collect::<Result<_, _>>()?;

    let mut kr = StdBufReader::new(File::open(key)?);
    let key = rustls_pemfile::pkcs8_private_keys(&mut kr)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .next()
        .map(PrivateKeyDer::Pkcs8)
        .ok_or("No private key")?;

    let mut cr = StdBufReader::new(File::open(ca)?);
    let mut roots = RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut cr) {
        roots.add(c?)?;
    }

    let verifier = rustls::server::WebPkiClientVerifier::builder(roots.into()).build()?;

    Ok(TlsAcceptor::from(Arc::new(
        ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)?,
    )))
}

// ── Extract CN from client TLS certificate ──────────────────────────────────

fn extract_admin_cn(stream: &tokio_rustls::server::TlsStream<tokio::net::TcpStream>) -> String {
    if let Some(cert_chain) = stream.get_ref().1.peer_certificates() {
        if let Some(client_cert) = cert_chain.first() {
            match parse_x509_certificate(client_cert.as_ref()) {
                Ok((_, cert)) => {
                    let subject = cert.subject();
                    for rdn in subject.iter() {
                        for attr in rdn.iter() {
                            if attr.attr_type() == &OID_X509_COMMON_NAME {
                                if let Ok(cn) = attr.as_str() {
                                    return cn.to_string();
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to parse admin client certificate: {}", e);
                }
            }
        }
    }
    "unknown".to_string()
}
