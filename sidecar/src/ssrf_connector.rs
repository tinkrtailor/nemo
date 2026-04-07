//! Custom hyper connector for the model proxy upstream client.
//!
//! The default `hyper_util::client::legacy::connect::HttpConnector`
//! performs its own DNS resolution from the `Uri` and cannot satisfy
//! FR-18 — it would create both the fail-open and the DNS-rebinding
//! windows that the spec is supposed to fix. **We MUST use a custom
//! connector.**
//!
//! This connector:
//!
//! 1. Runs `ssrf::resolve_safe(host, 443)` to get a vetted
//!    [`SocketAddr`](std::net::SocketAddr).
//! 2. Dials that address directly via `TcpStream::connect(addr)`.
//! 3. Wraps the TCP stream in TLS via `tokio_rustls::TlsConnector`,
//!    passing the **original hostname** as the `ServerName` so
//!    certificate verification and SNI still use it.
//! 4. Returns the TLS stream wrapped in `TokioIo` so hyper can read and
//!    write it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tower_service::Service;

use crate::ssrf::{self, SsrfError};

/// Errors produced by [`SsrfConnector::call`].
#[derive(Debug, Error)]
pub enum SsrfConnectorError {
    /// The request URI did not have a host component.
    #[error("URI is missing host")]
    MissingHost,
    /// FR-18 SSRF check rejected the host.
    #[error("ssrf check failed: {0}")]
    Ssrf(#[from] SsrfError),
    /// TCP dial failed.
    #[error("TCP connect failed: {0}")]
    Connect(std::io::Error),
    /// TLS handshake failed.
    #[error("TLS handshake failed: {0}")]
    Tls(std::io::Error),
    /// The URI host was not a valid DNS name.
    #[error("invalid DNS name {host:?}: {source}")]
    InvalidDnsName {
        host: String,
        #[source]
        source: rustls::pki_types::InvalidDnsNameError,
    },
}

/// Custom hyper connector implementing FR-18. Clone is cheap — the
/// contained `Arc<ClientConfig>` is the only state.
#[derive(Clone)]
pub struct SsrfConnector {
    tls_config: Arc<ClientConfig>,
}

impl SsrfConnector {
    /// Construct a connector using the provided rustls client config.
    pub fn new(tls_config: Arc<ClientConfig>) -> Self {
        Self { tls_config }
    }
}

impl Service<Uri> for SsrfConnector {
    type Response = SsrfConnection;
    type Error = SsrfConnectorError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let tls_config = Arc::clone(&self.tls_config);
        Box::pin(async move {
            let host = uri
                .host()
                .ok_or(SsrfConnectorError::MissingHost)?
                .to_string();
            let port = uri.port_u16().unwrap_or(443);

            // FR-18 step 1-4: resolve once, fail closed, pass SocketAddr
            // to dialer.
            let socket_addr = ssrf::resolve_safe(&host, port).await?;

            // Dial the vetted IP directly — never redial by hostname.
            let tcp = TcpStream::connect(socket_addr)
                .await
                .map_err(SsrfConnectorError::Connect)?;
            // Disable Nagle for lower latency SSE streaming.
            let _ = tcp.set_nodelay(true);

            // Wrap in TLS using the ORIGINAL hostname for SNI.
            let connector = TlsConnector::from(tls_config);
            let server_name = ServerName::try_from(host.clone()).map_err(|source| {
                SsrfConnectorError::InvalidDnsName {
                    host: host.clone(),
                    source,
                }
            })?;
            let tls = connector
                .connect(server_name, tcp)
                .await
                .map_err(SsrfConnectorError::Tls)?;

            Ok(SsrfConnection {
                inner: TokioIo::new(tls),
            })
        })
    }
}

pin_project_lite::pin_project! {
    /// Wrapper around a TLS-wrapped TCP stream that implements
    /// [`hyper_util::client::legacy::connect::Connection`] so
    /// hyper-util's legacy Client can use it. The inner TokioIo
    /// provides the [`hyper::rt::Read`] / [`hyper::rt::Write`]
    /// implementations — we delegate through a pin projection.
    pub struct SsrfConnection {
        #[pin]
        inner: TokioIo<TlsStream<TcpStream>>,
    }
}

impl Connection for SsrfConnection {
    fn connected(&self) -> Connected {
        // We do not advertise HTTP/2 via ALPN even though rustls could
        // — HTTP/1.1 is sufficient and matches Go parity, and h2 adds
        // head-of-line blocking subtleties to SSE we do not need.
        Connected::new()
    }
}

impl hyper::rt::Read for SsrfConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl hyper::rt::Write for SsrfConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.project().inner.poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        self.project().inner.poll_write_vectored(cx, bufs)
    }
}

// Suppress "unused imports" for tokio::io; we only needed those to
// verify the TlsStream<TcpStream> type bounds.
#[allow(dead_code)]
fn _assert_tls_io<T: AsyncRead + AsyncWrite>(_t: T) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tls_config() -> Arc<ClientConfig> {
        crate::tls::build_client_config_with_env(None).expect("tls config must build")
    }

    // Helper: extract the error variant from a connector call, mapping
    // Ok to an assertion failure without formatting the SsrfConnection
    // (which does not implement Debug).
    async fn call_expecting_error(uri: &str) -> SsrfConnectorError {
        let mut connector = SsrfConnector::new(test_tls_config());
        let uri: Uri = uri.parse().expect("uri parses");
        match connector.call(uri).await {
            Ok(_) => panic!("expected an error, got Ok connection"),
            Err(e) => e,
        }
    }

    #[tokio::test]
    async fn test_connector_fails_closed_on_dns_error() {
        let err = call_expecting_error("https://nonexistent.invalid/").await;
        assert!(
            matches!(err, SsrfConnectorError::Ssrf(_)),
            "expected Ssrf error"
        );
    }

    #[tokio::test]
    async fn test_connector_fails_closed_on_private_ip() {
        // `localhost` resolves to 127.0.0.1 — must be blocked.
        let err = call_expecting_error("https://localhost/").await;
        assert!(
            matches!(err, SsrfConnectorError::Ssrf(SsrfError::PrivateIp(_))),
            "expected Ssrf(PrivateIp)"
        );
    }

    #[tokio::test]
    async fn test_connector_missing_host() {
        // A URI without a host — e.g. path-only.
        let err = call_expecting_error("/foo").await;
        assert!(matches!(err, SsrfConnectorError::MissingHost));
    }
}
