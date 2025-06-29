use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use std::io::{self};


#[async_trait]
pub trait Connection: AsyncRead + AsyncWrite + Send + Sync + Unpin {
    async fn read_buf(&mut self, buf: &mut Vec<u8>) -> io::Result<usize>;
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
}

pub struct TlsConnection {
    stream: TlsStream<TcpStream>,
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

pub async fn create_listener(addr: &str) -> io::Result<TcpListener> {
    TcpListener::bind(addr).await
}