//! Plain HTTP on the HTTPS port. Safari uses `http://` for an address typed without
//! a scheme, and shows "cannot parse response" when the TLS reply comes back. This
//! acceptor looks at a connection's first byte and redirects plain HTTP requests to
//! the same address over HTTPS.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::Duration;

use axum_server::accept::Accept;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Every TLS connection starts with a handshake record, which starts with this byte.
const TLS_HANDSHAKE: u8 = 0x16;

#[derive(Clone)]
pub struct RedirectPlainHttp;

impl<S: Send + 'static> Accept<TcpStream, S> for RedirectPlainHttp {
    type Stream = TcpStream;
    type Service = S;
    type Future = Pin<Box<dyn Future<Output = io::Result<(TcpStream, S)>> + Send>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        Box::pin(async move {
            let mut first = [0u8; 1];
            stream.peek(&mut first).await?;
            if first[0] == TLS_HANDSHAKE {
                return Ok((stream, service));
            }
            match tokio::time::timeout(Duration::from_secs(5), redirect(stream)).await {
                Ok(result) => result?,
                Err(_) => tracing::debug!("plain HTTP client sent no request"),
            }
            Err(io::Error::other("plain HTTP connection, redirected to HTTPS"))
        })
    }
}

/// Answer the request on `stream` with a redirect to the same path over HTTPS.
async fn redirect(mut stream: TcpStream) -> io::Result<()> {
    let mut head = vec![0; 4096];
    let length = stream.read(&mut head).await?;
    let request = String::from_utf8_lossy(&head[..length]);
    // The address the phone reached us on, so no need to trust the Host header.
    let location = format!("https://{}{}", stream.local_addr()?, request_path(&request));
    tracing::info!("plain HTTP request, redirecting to {location}");
    let response = format!(
        "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// The path of an HTTP request line (`GET /?k=… HTTP/1.1`), or `/`.
fn request_path(request: &str) -> &str {
    match request.split_ascii_whitespace().nth(1) {
        Some(path) if path.starts_with('/') => path,
        _ => "/",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_paths() {
        assert_eq!(request_path("GET /?k=abc HTTP/1.1\r\nHost: x\r\n\r\n"), "/?k=abc");
        assert_eq!(request_path("GET http://evil/ HTTP/1.1\r\n"), "/");
        assert_eq!(request_path(""), "/");
    }
}
