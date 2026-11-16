//! Upstream-Forwarder: schickt die Original-Query per UDP an den ersten Resolver,
//! der gültig antwortet.
//!
//! DoT/DoH kommen als eigener Transport in diesen Modul dazu; der Aufrufer sieht nur
//! [`Upstream::query`].

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::timeout;

/// Maximale DNS-Antwortgröße über UDP (EDNS0-Puffer).
const MAX_RESPONSE_BYTES: usize = 4096;

/// Fehler beim Upstream-Versuch.
#[derive(Debug)]
pub enum UpstreamError {
    /// In der Konfiguration steht kein Resolver.
    NoServers,
    /// Socket-Fehler (Bind, Send, Recv).
    Io(io::Error),
    /// Keine Antwort innerhalb des Timeouts.
    Timeout,
    /// Antwort kürzer als ein DNS-Header oder kein Response-Flag.
    Malformed,
    /// Antwort-ID passt nicht zur Query (verworfene Fremdantwort).
    IdMismatch,
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoServers => write!(f, "kein Upstream konfiguriert"),
            Self::Io(err) => write!(f, "socket error: {err}"),
            Self::Timeout => write!(f, "timeout"),
            Self::Malformed => write!(f, "ungültige Antwort"),
            Self::IdMismatch => write!(f, "Antwort-ID passt nicht"),
        }
    }
}

impl std::error::Error for UpstreamError {}

/// Forwarder mit Resolver-Liste und Timeout.
pub struct Upstream {
    servers: Vec<SocketAddr>,
    timeout: Duration,
    verbose: bool,
}

impl Upstream {
    /// Erzeugt den Forwarder. Die Resolver werden in Reihenfolge probiert.
    pub fn new(servers: Vec<SocketAddr>, timeout: Duration, verbose: bool) -> Self {
        Self {
            servers,
            timeout,
            verbose,
        }
    }

    /// Schickt `request` unverändert an die Resolver und gibt die erste gültige Antwort zurück.
    pub async fn query(&self, request: &[u8], id: u16) -> Result<Vec<u8>, UpstreamError> {
        if self.servers.is_empty() {
            return Err(UpstreamError::NoServers);
        }

        let mut last_error = UpstreamError::NoServers;
        for server in &self.servers {
            match self.query_one(*server, request, id).await {
                Ok(response) => return Ok(response),
                Err(err) => {
                    if self.verbose {
                        eprintln!("[upstream] {server}: {err}");
                    }
                    last_error = err;
                }
            }
        }
        Err(last_error)
    }

    async fn query_one(
        &self,
        server: SocketAddr,
        request: &[u8],
        id: u16,
    ) -> Result<Vec<u8>, UpstreamError> {
        let bind = if server.is_ipv4() {
            SocketAddr::from(([0u8, 0, 0, 0], 0))
        } else {
            SocketAddr::from(([0u16; 8], 0))
        };
        let socket = UdpSocket::bind(bind).await.map_err(UpstreamError::Io)?;
        socket.connect(server).await.map_err(UpstreamError::Io)?;
        socket.send(request).await.map_err(UpstreamError::Io)?;

        let mut buf = [0u8; MAX_RESPONSE_BYTES];
        match timeout(self.timeout, socket.recv(&mut buf)).await {
            Ok(Ok(len)) => {
                if len < 12 || buf[2] & 0x80 == 0 {
                    return Err(UpstreamError::Malformed);
                }
                if u16::from_be_bytes([buf[0], buf[1]]) != id {
                    return Err(UpstreamError::IdMismatch);
                }
                Ok(buf[..len].to_vec())
            }
            Ok(Err(err)) => Err(UpstreamError::Io(err)),
            Err(_) => Err(UpstreamError::Timeout),
        }
    }
}
