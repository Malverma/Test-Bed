//! One connection type over either transport (spec3 §2): quinn on the LAN,
//! with RemoteLinux's wire format, and iroh across the internet. iroh runs on
//! noq, a fork of quinn with the same API, so each method is a thin match.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;

type IrohConn = iroh::endpoint::Connection;

#[derive(Clone, Debug)]
pub enum Conn {
    Lan(quinn::Connection),
    Internet(IrohConn),
}

pub enum SendStream {
    Lan(quinn::SendStream),
    Internet(iroh::endpoint::SendStream),
}

pub enum RecvStream {
    Lan(quinn::RecvStream),
    Internet(iroh::endpoint::RecvStream),
}

/// How a connection reaches the peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    Lan,
    Direct,
    Relay,
}

/// The peer closed the connection on purpose, with this code and reason.
#[derive(Clone, Debug)]
pub struct AppClose {
    pub code: u32,
    pub reason: String,
}

impl From<quinn::Connection> for Conn {
    fn from(c: quinn::Connection) -> Self {
        Conn::Lan(c)
    }
}

impl From<IrohConn> for Conn {
    fn from(c: IrohConn) -> Self {
        Conn::Internet(c)
    }
}

impl Conn {
    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream)> {
        Ok(match self {
            Conn::Lan(c) => {
                let (s, r) = c.open_bi().await?;
                (SendStream::Lan(s), RecvStream::Lan(r))
            }
            Conn::Internet(c) => {
                let (s, r) = c.open_bi().await?;
                (SendStream::Internet(s), RecvStream::Internet(r))
            }
        })
    }

    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream)> {
        Ok(match self {
            Conn::Lan(c) => {
                let (s, r) = c.accept_bi().await?;
                (SendStream::Lan(s), RecvStream::Lan(r))
            }
            Conn::Internet(c) => {
                let (s, r) = c.accept_bi().await?;
                (SendStream::Internet(s), RecvStream::Internet(r))
            }
        })
    }

    pub async fn open_uni(&self) -> Result<SendStream> {
        Ok(match self {
            Conn::Lan(c) => SendStream::Lan(c.open_uni().await?),
            Conn::Internet(c) => SendStream::Internet(c.open_uni().await?),
        })
    }

    pub async fn accept_uni(&self) -> Result<RecvStream> {
        Ok(match self {
            Conn::Lan(c) => RecvStream::Lan(c.accept_uni().await?),
            Conn::Internet(c) => RecvStream::Internet(c.accept_uni().await?),
        })
    }

    pub async fn read_datagram(&self) -> Result<Bytes> {
        Ok(match self {
            Conn::Lan(c) => c.read_datagram().await?,
            Conn::Internet(c) => c.read_datagram().await?,
        })
    }

    pub fn send_datagram(&self, d: Bytes) -> Result<()> {
        match self {
            Conn::Lan(c) => c.send_datagram(d)?,
            Conn::Internet(c) => c.send_datagram(d)?,
        }
        Ok(())
    }

    pub fn max_datagram_size(&self) -> Option<usize> {
        match self {
            Conn::Lan(c) => c.max_datagram_size(),
            Conn::Internet(c) => c.max_datagram_size(),
        }
    }

    /// Round-trip time on the path in use.
    pub fn rtt(&self) -> Duration {
        match self {
            Conn::Lan(c) => c.rtt(),
            Conn::Internet(c) => c.paths().iter().find(|p| p.is_selected()).map(|p| p.rtt()).unwrap_or_default(),
        }
    }

    pub fn path(&self) -> Path {
        match self {
            Conn::Lan(_) => Path::Lan,
            Conn::Internet(c) => match c.paths().iter().find(|p| p.is_selected()) {
                Some(p) if p.is_ip() => Path::Direct,
                _ => Path::Relay,
            },
        }
    }

    pub fn close(&self, code: u32, reason: &[u8]) {
        match self {
            Conn::Lan(c) => c.close(code.into(), reason),
            Conn::Internet(c) => c.close(code.into(), reason),
        }
    }

    /// Waits until the connection is closed, by either side.
    pub async fn closed(&self) {
        match self {
            Conn::Lan(c) => {
                c.closed().await;
            }
            Conn::Internet(c) => {
                c.closed().await;
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        match self {
            Conn::Lan(c) => c.close_reason().is_some(),
            Conn::Internet(c) => c.close_reason().is_some(),
        }
    }

    /// Set when the peer closed the connection with an application code.
    pub fn app_close(&self) -> Option<AppClose> {
        let (code, reason) = match self {
            Conn::Lan(c) => match c.close_reason()? {
                quinn::ConnectionError::ApplicationClosed(a) => (a.error_code.into_inner(), a.reason),
                _ => return None,
            },
            Conn::Internet(c) => match c.close_reason()? {
                iroh::endpoint::ConnectionError::ApplicationClosed(a) => (a.error_code.into_inner(), a.reason),
                _ => return None,
            },
        };
        Some(AppClose { code: code as u32, reason: String::from_utf8_lossy(&reason).into_owned() })
    }

    /// The peer's IP address, on the LAN only.
    pub fn remote_address(&self) -> Option<SocketAddr> {
        match self {
            Conn::Lan(c) => Some(c.remote_address()),
            Conn::Internet(_) => None,
        }
    }

    /// Who's at the other end, for logs and status.
    pub fn describe(&self) -> String {
        match self {
            Conn::Lan(c) => c.remote_address().to_string(),
            Conn::Internet(c) => format!("internet ({})", short_id(&c.remote_id())),
        }
    }

    /// Fingerprint of the peer's certificate (LAN). Internet peers are known
    /// by their endpoint id instead, see [`Conn::peer_iroh_id`].
    pub fn peer_fingerprint(&self) -> Result<String> {
        match self {
            Conn::Lan(c) => crate::transport::peer_fingerprint(c),
            Conn::Internet(_) => Err(anyhow!("internet connections have no certificate fingerprint")),
        }
    }

    /// The key the peer is trusted by: its certificate fingerprint on the
    /// LAN, its endpoint id over the internet.
    pub fn peer_key(&self) -> Result<String> {
        match self {
            Conn::Lan(_) => self.peer_fingerprint(),
            Conn::Internet(c) => Ok(c.remote_id().to_string()),
        }
    }

    pub fn peer_iroh_id(&self) -> Option<String> {
        match self {
            Conn::Lan(_) => None,
            Conn::Internet(c) => Some(c.remote_id().to_string()),
        }
    }

    pub fn alpn(&self) -> Option<Vec<u8>> {
        match self {
            Conn::Lan(c) => crate::transport::alpn(c),
            Conn::Internet(c) => Some(c.alpn().to_vec()),
        }
    }
}

impl SendStream {
    pub async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        match self {
            SendStream::Lan(s) => s.write_all(buf).await?,
            SendStream::Internet(s) => s.write_all(buf).await?,
        }
        Ok(())
    }

    /// No more data will be written.
    pub fn finish(&mut self) {
        let _ = match self {
            SendStream::Lan(s) => s.finish().is_ok(),
            SendStream::Internet(s) => s.finish().is_ok(),
        };
    }

    /// Waits until the peer has received everything (or stopped the stream).
    pub async fn stopped(&mut self) {
        match self {
            SendStream::Lan(s) => {
                let _ = s.stopped().await;
            }
            SendStream::Internet(s) => {
                let _ = s.stopped().await;
            }
        }
    }
}

impl RecvStream {
    /// Fills `buf`. `Ok(false)` if the stream ended cleanly before any byte.
    pub async fn read_exact_or_end(&mut self, buf: &mut [u8]) -> Result<bool> {
        match self {
            RecvStream::Lan(r) => match r.read_exact(buf).await {
                Ok(()) => Ok(true),
                Err(quinn::ReadExactError::FinishedEarly(0)) => Ok(false),
                Err(e) => Err(e.into()),
            },
            RecvStream::Internet(r) => match r.read_exact(buf).await {
                Ok(()) => Ok(true),
                Err(iroh::endpoint::ReadExactError::FinishedEarly(0)) => Ok(false),
                Err(e) => Err(e.into()),
            },
        }
    }

    /// Reads what has arrived, up to `buf.len()`; `None` at the end of the stream.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<Option<usize>> {
        Ok(match self {
            RecvStream::Lan(r) => r.read(buf).await?,
            RecvStream::Internet(r) => r.read(buf).await?,
        })
    }

    pub async fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        if !self.read_exact_or_end(buf).await? && !buf.is_empty() {
            return Err(anyhow!("stream ended early"));
        }
        Ok(())
    }
}

/// True if the error is the connection or one of its streams closing, which
/// ends a session normally.
pub fn is_closed(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<quinn::ConnectionError>().is_some()
            || c.downcast_ref::<quinn::ReadExactError>().is_some()
            || c.downcast_ref::<quinn::WriteError>().is_some()
            || c.downcast_ref::<iroh::endpoint::ConnectionError>().is_some()
            || c.downcast_ref::<iroh::endpoint::ReadExactError>().is_some()
            || c.downcast_ref::<iroh::endpoint::WriteError>().is_some()
    })
}

/// First 8 characters of an endpoint id, like `short_fingerprint`.
pub fn short_id(id: &impl std::fmt::Display) -> String {
    id.to_string().chars().take(8).collect()
}
