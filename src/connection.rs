use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use std::io;
use x509_parser::prelude::*;
use x509_parser::oid_registry::OID_X509_COMMON_NAME;

#[async_trait]
pub trait Connection: AsyncRead + AsyncWrite + Send + Sync + Unpin {
    async fn read_buf(&mut self, buf: &mut Vec<u8>) -> io::Result<usize>;
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
    fn client_id(&self) -> &str;
}

pub struct TlsConnection {
    stream: TlsStream<TcpStream>,
    client_id: String,
}

impl TlsConnection {
    pub async fn new(stream: TcpStream, acceptor: TlsAcceptor) -> io::Result<Self> {
        let stream = acceptor.accept(stream).await?;
        let client_id = Self::extract_client_id(&stream)?;
        Ok(TlsConnection { stream, client_id })
    }

    #[allow(dead_code)]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    fn extract_client_id(stream: &TlsStream<TcpStream>) -> io::Result<String> {
        if let Some(cert_chain) = stream.get_ref().1.peer_certificates() {
            if let Some(client_cert) = cert_chain.first() {
                match parse_x509_certificate(client_cert.as_ref()) {
                    Ok((_, cert)) => {
                        let subject = cert.subject();
                        for rdn in subject.iter() {
                            for attr in rdn.iter() {
                                if attr.attr_type() == &OID_X509_COMMON_NAME {
                                    if let Ok(cn) = attr.as_str() {
                                        return Ok(cn.to_string());
                                    }
                                }
                            }
                        }
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "No Common Name found in client certificate",
                        ))
                    }
                    Err(e) => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Failed to parse certificate: {}", e),
                    )),
                }
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "No client certificate provided",
                ))
            }
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "No client certificates found",
            ))
        }
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

    fn client_id(&self) -> &str {
        &self.client_id
    }
}

pub async fn create_listener(addr: &str) -> io::Result<TcpListener> {
    TcpListener::bind(addr).await
}