//! Owner-scoped local endpoint abstraction: Unix sockets and Windows pipes.
//!
//! There is no TCP listener. The endpoint is the single-instance lock: a
//! successful bind means this daemon owns the session surface.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(windows)]
use std::hash::{DefaultHasher, Hash, Hasher};
#[cfg(windows)]
use std::sync::Arc;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
#[cfg(windows)]
use tokio::sync::Mutex;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Endpoint {
    Unix(PathBuf),
    Windows(String),
}

#[derive(Debug, Error)]
pub enum EndpointError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("this platform does not use {0} endpoints here")]
    WrongPlatform(&'static str),
    #[error("the local endpoint is already owned by a running daemon")]
    AddrInUse,
    #[error("stale endpoint is not owned by the current user")]
    NotOwner,
}

impl Endpoint {
    /// Default endpoint for this platform, honoring XDG_RUNTIME_DIR on Unix.
    pub fn default_endpoint() -> Result<Self, EndpointError> {
        platform_default()
    }

    /// Build an endpoint from --socket or STITCH_MUX_SOCKET.
    pub fn from_override(value: &str) -> Self {
        if value.starts_with(r"\\.\pipe\") {
            return Self::Windows(value.to_owned());
        }
        platform_endpoint(value)
    }

    pub async fn bind(&self) -> Result<Listener, EndpointError> {
        match self {
            Self::Unix(path) => bind_unix(path).await,
            Self::Windows(name) => bind_windows(name),
        }
    }

    pub async fn connect(&self) -> Result<Stream, EndpointError> {
        match self {
            Self::Unix(path) => connect_unix(path).await,
            Self::Windows(name) => connect_windows(name).await,
        }
    }
}

#[cfg(unix)]
fn platform_default() -> Result<Endpoint, EndpointError> {
    let uid = unsafe { libc::getuid() };
    if let Ok(base) = std::env::var("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(base).join("stitch-mux");
        return Ok(Endpoint::Unix(dir.join(format!("{uid}.sock"))));
    }
    Ok(Endpoint::Unix(PathBuf::from(format!(
        "/tmp/stitch-mux-{uid}/default.sock"
    ))))
}

#[cfg(unix)]
fn platform_endpoint(value: &str) -> Endpoint {
    Endpoint::Unix(PathBuf::from(value))
}

#[cfg(unix)]
async fn bind_unix(path: &PathBuf) -> Result<Listener, EndpointError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        tokio::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .await?;
    }
    if path.exists() {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Err(EndpointError::AddrInUse);
        }
        let meta = std::fs::metadata(path)?;
        if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(EndpointError::NotOwner);
        }
        tokio::fs::remove_file(path).await?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Listener {
        inner: ListenerInner::Unix(listener),
    })
}

#[cfg(unix)]
async fn connect_unix(path: &PathBuf) -> Result<Stream, EndpointError> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    Ok(Stream::new(stream))
}

#[cfg(not(unix))]
fn platform_default() -> Result<Endpoint, EndpointError> {
    let mut hasher = DefaultHasher::new();
    let scope = format!(
        "{}|{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "local".into())
    );
    scope.hash(&mut hasher);
    Ok(Endpoint::Windows(format!(
        r"\\.\pipe\stitch-mux-{:016x}",
        hasher.finish()
    )))
}

#[cfg(not(unix))]
fn platform_endpoint(value: &str) -> Endpoint {
    Endpoint::Windows(format!(r"\\.\pipe\{value}"))
}

#[cfg(not(unix))]
fn bind_windows(name: &str) -> Result<Listener, EndpointError> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let mut options = ServerOptions::new();
    options.first_pipe_instance(true).max_instances(254);
    let first = options.create(name)?;
    Ok(Listener {
        inner: ListenerInner::Windows {
            name: name.to_owned(),
            next: Arc::new(Mutex::new(Some(first))),
        },
    })
}

#[cfg(not(unix))]
async fn connect_windows(name: &str) -> Result<Stream, EndpointError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let client = ClientOptions::new().open(name)?;
    Ok(Stream::new(client))
}

#[cfg(unix)]
fn bind_windows(_name: &str) -> Result<Listener, EndpointError> {
    Err(EndpointError::WrongPlatform("named pipe"))
}

#[cfg(unix)]
async fn connect_windows(_name: &str) -> Result<Stream, EndpointError> {
    Err(EndpointError::WrongPlatform("named pipe"))
}

#[cfg(not(unix))]
async fn bind_unix(_path: &PathBuf) -> Result<Listener, EndpointError> {
    Err(EndpointError::WrongPlatform("unix socket"))
}

#[cfg(not(unix))]
async fn connect_unix(_path: &PathBuf) -> Result<Stream, EndpointError> {
    Err(EndpointError::WrongPlatform("unix socket"))
}

enum ListenerInner {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(not(unix))]
    Windows {
        name: String,
        next: Arc<Mutex<Option<tokio::net::windows::named_pipe::NamedPipeServer>>>,
    },
}

pub struct Listener {
    inner: ListenerInner,
}

impl Listener {
    pub async fn accept(&self) -> Result<Stream, EndpointError> {
        match &self.inner {
            #[cfg(unix)]
            ListenerInner::Unix(listener) => {
                let (stream, _) = listener.accept().await?;
                Ok(Stream::new(stream))
            }
            #[cfg(not(unix))]
            ListenerInner::Windows { name, next } => {
                let server = next
                    .lock()
                    .await
                    .take()
                    .ok_or_else(|| io::Error::other("no named-pipe instance available"))?;
                server.connect().await?;
                use tokio::net::windows::named_pipe::ServerOptions;
                let mut options = ServerOptions::new();
                options.first_pipe_instance(false).max_instances(254);
                *next.lock().await = Some(options.create(name)?);
                Ok(Stream::new(server))
            }
        }
    }
}

trait StreamIo: AsyncRead + AsyncWrite + Send {}

impl<T> StreamIo for T where T: AsyncRead + AsyncWrite + Send {}

/// Type-erased async byte stream over either transport.
pub struct Stream {
    inner: Box<dyn StreamIo + Unpin>,
}

impl Stream {
    fn new<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            inner: Box::new(stream),
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        AsyncRead::poll_read(Pin::new(self.inner.as_mut()), cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(self.inner.as_mut()), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(self.inner.as_mut()), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(self.inner.as_mut()), cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_detects_pipe_paths() {
        assert!(matches!(
            Endpoint::from_override(r"\\.\pipe\custom"),
            Endpoint::Windows(_)
        ));
    }

    #[test]
    fn default_endpoint_is_local() {
        let endpoint = Endpoint::default_endpoint().unwrap();
        match endpoint {
            Endpoint::Unix(path) => assert!(path.to_string_lossy().contains("stitch-mux")),
            Endpoint::Windows(name) => assert!(name.starts_with(r"\\.\pipe\stitch-mux-")),
        }
    }
}
