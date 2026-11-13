//! The share service's local control channel: a Unix socket on Linux, a
//! named pipe on Windows. Only this user can use either.

use std::io;

use tokio::io::{AsyncRead, AsyncWrite};

pub type Reader = Box<dyn AsyncRead + Unpin + Send>;
pub type Writer = Box<dyn AsyncWrite + Unpin + Send>;

fn split<T: AsyncRead + AsyncWrite + Send + 'static>(s: T) -> (Reader, Writer) {
    let (r, w) = tokio::io::split(s);
    (Box::new(r), Box::new(w))
}

/// Connects to the running share service.
pub async fn connect() -> io::Result<(Reader, Writer)> {
    #[cfg(unix)]
    {
        Ok(split(tokio::net::UnixStream::connect(crate::paths::host_control_socket()).await?))
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let name = crate::paths::host_control_socket();
        // All instances busy: the service is answering someone else; wait a bit.
        for _ in 0..20 {
            match ClientOptions::new().open(&name) {
                Ok(c) => return Ok(split(c)),
                Err(e) if e.raw_os_error() == Some(231 /* ERROR_PIPE_BUSY */) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "the share service is busy"))
    }
}

/// The service's side: accepts one connection at a time, each handled by the caller.
pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl Listener {
    pub fn bind() -> io::Result<Self> {
        let path = crate::paths::host_control_socket();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = path.parent().expect("socket path has a directory");
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            let _ = std::fs::remove_file(&path);
            let inner = tokio::net::UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self { inner })
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            // The first instance claims the name: a second service fails here.
            let next = ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&path)?;
            Ok(Self { next })
        }
    }

    pub async fn accept(&mut self) -> io::Result<(Reader, Writer)> {
        #[cfg(unix)]
        {
            let (s, _) = self.inner.accept().await?;
            Ok(split(s))
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            self.next.connect().await?;
            let fresh = ServerOptions::new().reject_remote_clients(true).create(crate::paths::host_control_socket())?;
            Ok(split(std::mem::replace(&mut self.next, fresh)))
        }
    }
}

/// Removes the socket file when the service stops (nothing to do for a pipe).
pub fn cleanup() {
    #[cfg(unix)]
    let _ = std::fs::remove_file(crate::paths::host_control_socket());
}
