//! Owner-scoped local endpoint abstraction: Unix sockets and Windows pipes.
//!
//! There is no TCP listener. The endpoint is the single-instance lock: a
//! successful bind means this daemon owns the session surface.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(windows)]
use std::collections::VecDeque;
#[cfg(windows)]
use std::hash::{DefaultHasher, Hash, Hasher};
#[cfg(windows)]
use std::time::{Duration, Instant};

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

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "{}", path.display()),
            Self::Windows(name) => f.write_str(name),
        }
    }
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

    // A relative override such as `rel.sock` has an empty parent; binding it
    // in the current directory is still valid.
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .await?;
        // A directory owned by another user can be replaced between our bind
        // and a client connect, which would redirect the socket. The default
        // runtime directory is created by us; an explicit path in a shared,
        // root-owned directory (for example /tmp) is still allowed.
        let meta = std::fs::metadata(parent)?;
        if !directory_owner_is_trusted(meta.uid(), unsafe { libc::geteuid() }) {
            return Err(EndpointError::NotOwner);
        }
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
fn directory_owner_is_trusted(owner_uid: u32, effective_uid: u32) -> bool {
    owner_uid == effective_uid || owner_uid == 0
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

/// Pending pipe instances kept ready for concurrent clients. Windows returns
/// `ERROR_PIPE_BUSY` when every instance of a pipe is already serving a client,
/// so a single instance made simultaneous attach/command connections flaky.
#[cfg(not(unix))]
const PIPE_POOL: usize = 4;
#[cfg(not(unix))]
const ERROR_PIPE_BUSY: i32 = 231;

#[cfg(not(unix))]
fn pipe_instance(
    name: &str,
    first: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, io::Error> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let mut options = ServerOptions::new();
    options.first_pipe_instance(first).max_instances(254);
    options.create(name)
}

#[cfg(not(unix))]
fn bind_windows(name: &str) -> Result<Listener, EndpointError> {
    let first = pipe_instance(name, true)?;
    let mut pending = VecDeque::with_capacity(PIPE_POOL);
    pending.push_back(first);
    for _ in 1..PIPE_POOL {
        pending.push_back(pipe_instance(name, false)?);
    }
    Ok(Listener {
        inner: ListenerInner::Windows {
            name: name.to_owned(),
            pending: Mutex::new(pending),
        },
    })
}

#[cfg(not(unix))]
async fn connect_windows(name: &str) -> Result<Stream, EndpointError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    // Retry ERROR_PIPE_BUSY: a burst of attaches can outrun the listener's
    // pool refill. The daemon owns the endpoint, so waiting is bounded and
    // cheap; any other error is returned immediately.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match ClientOptions::new().open(name) {
            Ok(client) => return Ok(Stream::new(client)),
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if Instant::now() >= deadline {
                    return Err(err.into());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
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
        pending: Mutex<VecDeque<tokio::net::windows::named_pipe::NamedPipeServer>>,
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
            ListenerInner::Windows { name, pending } => {
                let mut pending = pending.lock().await;
                let server = pending
                    .pop_front()
                    .ok_or_else(|| io::Error::other("no named-pipe instance available"))?;
                // Refill before waiting for this client so the next connect
                // already has an instance to land on.
                if let Ok(spare) = pipe_instance(name, false) {
                    pending.push_back(spare);
                }
                drop(pending);
                server.connect().await?;
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

    #[cfg(unix)]
    #[test]
    fn shared_endpoint_directory_must_not_be_owned_by_another_user() {
        assert!(directory_owner_is_trusted(1000, 1000));
        assert!(directory_owner_is_trusted(0, 1000), "/tmp is root-owned");
        assert!(!directory_owner_is_trusted(1001, 1000));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_endpoint_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("stitch-mux-perm-{}-{unique}", std::process::id()));
        let path = dir.join("endpoint.sock");
        let listener = Endpoint::Unix(path.clone()).bind().await.unwrap();
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "runtime directory must be owner-only");
        let socket_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(socket_mode, 0o600, "socket must be owner-only");
        drop(listener);
        std::fs::remove_dir_all(&dir).unwrap();
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
