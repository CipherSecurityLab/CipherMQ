use async_trait::async_trait;
use std::fs::File;
use std::io::{self, BufReader};
use tokio_rustls::rustls::{ServerConfig, pki_types::{CertificateDer, PrivateKeyDer}};
use tokio_rustls::TlsAcceptor;
use crate::connection::TlsConnection;
use tokio::net::TcpStream;
use rustls::RootCertStore;
use tracing::error;

// Trait for handling authentication
#[async_trait]
pub trait AuthHandler: Send + Sync + 'static {
    #[allow(dead_code)] // Ignore dead_code warning
    fn configure(&self, config: ServerConfig) -> Result<ServerConfig, Box<dyn std::error::Error>>;
    async fn authenticate(&self, stream: TcpStream) -> io::Result<TlsConnection>;
}

// Implementation of authentication for mTLS
#[derive(Clone)]
pub struct MTlsAuth {
    acceptor: TlsAcceptor,
}

impl MTlsAuth {
    pub fn new(
        cert_path: &str,
        key_path: &str,
        ca_cert_path: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let config = configure_mtls(cert_path, key_path, ca_cert_path)?;
        let acceptor = TlsAcceptor::from(std::sync::Arc::new(config));
        Ok(MTlsAuth { acceptor })
    }
}

#[async_trait]
impl AuthHandler for MTlsAuth {
    fn configure(&self, config: ServerConfig) -> Result<ServerConfig, Box<dyn std::error::Error>> {
        Ok(config)
    }

    async fn authenticate(&self, stream: TcpStream) -> io::Result<TlsConnection> {
        TlsConnection::new(stream, self.acceptor.clone()).await
    }
}

// Helper function to configure mTLS
fn configure_mtls(
    cert_path: &str,
    key_path: &str,
    ca_cert_path: &str,
) -> Result<ServerConfig, Box<dyn std::error::Error>> {
    // Load server certificates
    let cert_file = File::open(cert_path).map_err(|e| {
        error!("Failed to open cert file {}: {}", cert_path, e);
        e
    })?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<CertificateDer> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            error!("Failed to read certificates from {}: {}", cert_path, e);
            e
        })?;

    // Load server private key
    let key_file = File::open(key_path).map_err(|e| {
        error!("Failed to open key file {}: {}", key_path, e);
        e
    })?;
    let mut key_reader = BufReader::new(key_file);
    let keys: Vec<PrivateKeyDer> = rustls_pemfile::pkcs8_private_keys(&mut key_reader)
        .map(|result| result.map(PrivateKeyDer::Pkcs8))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            error!("Failed to read private key from {}: {}", key_path, e);
            e
        })?;
    let key = keys
        .into_iter()
        .next()
        .ok_or_else(|| {
            let err = "No private key found".to_string();
            error!("{}", err);
            err
        })?;

    // Load CA certificate for client verification
    let ca_file = File::open(ca_cert_path).map_err(|e| {
        error!("Failed to open CA cert file {}: {}", ca_cert_path, e);
        e
    })?;
    let mut ca_reader = BufReader::new(ca_file);
    let ca_certs: Vec<CertificateDer> = rustls_pemfile::certs(&mut ca_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            error!("Failed to read CA certificates from {}: {}", ca_cert_path, e);
            e
        })?;

    // Configure RootCertStore for client certificate verification
    let mut root_store = RootCertStore::empty();
    for cert in ca_certs {
        root_store.add(cert).map_err(|e| {
            error!("Failed to add CA certificate to root store: {}", e);
            e
        })?;
    }

    // Configure client certificate verification
    let verifier = rustls::server::WebPkiClientVerifier::builder(root_store.into())
        .build()
        .map_err(|e| {
            error!("Failed to build client certificate verifier: {}", e);
            e
        })?;

    // Configure ServerConfig with client authentication
    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| {
            error!("Failed to build ServerConfig: {}", e);
            e
        })?;
    Ok(config)
}