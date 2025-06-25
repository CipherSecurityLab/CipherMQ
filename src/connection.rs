use async_trait::async_trait;
use std::fs::File;
use std::io::{self, BufReader};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::{ServerConfig, pki_types::{CertificateDer, PrivateKeyDer}};
use tokio_rustls::TlsAcceptor;

#[async_trait]
pub trait Connection: AsyncRead + AsyncWrite + Send + Sync + Unpin {
    async fn read_buf(&mut self, buf: &mut Vec<u8>) -> io::Result<usize>;
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
}

// ----------------- TCP ----------------------------------
// #[async_trait]
// impl Connection for TcpStream {
//     async fn read_buf(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
//         AsyncReadExt::read_buf(self, buf).await
//     }

//     async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
//         AsyncWriteExt::write_all(self, buf).await
//     }
// }

pub struct TlsConnection {
    stream: tokio_rustls::server::TlsStream<TcpStream>,
}

impl TlsConnection {
    pub async fn new(stream: TcpStream, acceptor: TlsAcceptor) -> io::Result<Self> {
        let stream = acceptor.accept(stream).await?;
        Ok(TlsConnection { stream })
    }
}

impl AsyncRead for TlsConnection {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsConnection {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

#[async_trait]
impl Connection for TlsConnection {
    async fn read_buf(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        AsyncReadExt::read_buf(&mut self.stream, buf).await
    }

    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        AsyncWriteExt::write_all(&mut self.stream, buf).await
    }
}

pub fn load_tls_config(
    cert_path: &str,
    key_path: &str,
) -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let cert_file = File::open(cert_path)?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<CertificateDer> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()?;

    let key_file = File::open(key_path)?;
    let mut key_reader = BufReader::new(key_file);
    let keys: Vec<PrivateKeyDer> = rustls_pemfile::pkcs8_private_keys(&mut key_reader)
        .map(|result| result.map(PrivateKeyDer::Pkcs8))
        .collect::<Result<Vec<_>, _>>()?;
    let key = keys
        .into_iter()
        .next()
        .ok_or_else(|| "No private key found".to_string())?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(config)
}

pub async fn create_listener(addr: &str) -> io::Result<TcpListener> {
    TcpListener::bind(addr).await
}
