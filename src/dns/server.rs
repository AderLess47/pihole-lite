//! UDP-Listener auf Port 53: Socket, Semaphor und Senden.
//!
//! Jede Query läuft in einem eigenen Task, aber nur solange ein Permit des Semaphors
//! frei ist. Das ist die Rückstau-Bremse: ohne sie könnten 500 parallele Clients auf
//! einem Pi 1 den Heap sprengen.
//!
//! Wie eine Query beantwortet wird, entscheidet [`DnsServer::resolve`] in
//! [`super::resolve`]; hier bleibt nur der Loop: Bytes empfangen, Ergebnis senden,
//! Verdict buchen, weitergeleitete Antworten in den Cache legen.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use simple_dns::Name;
use tokio::net::UdpSocket;
use tokio::sync::Semaphore;

use super::cache::{cache_ttl, ResponseCache};
use super::resolve::Reply;
use super::upstream::Upstream;
use crate::filter::engine::FilterEngine;
use crate::stats::ringbuffer::{Stats, Verdict};

/// Maximale Query-Größe über UDP.
const MAX_QUERY_BYTES: usize = 4096;
/// Länge des DNS-Headers.
pub(super) const HEADER_BYTES: usize = 12;

/// Wie geblockte Namen beantwortet werden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SinkholeMode {
    /// `A` → 0.0.0.0, `AAAA` → `::`, andere Typen leer mit NOERROR.
    #[default]
    ZeroIp,
    /// NXDOMAIN für alle Typen.
    NxDomain,
}

impl SinkholeMode {
    /// Anzeigename für das Startbanner.
    pub fn label(self) -> &'static str {
        match self {
            Self::ZeroIp => "zero-ip",
            Self::NxDomain => "nxdomain",
        }
    }
}

/// Laufzeit-Optionen des Listeners.
pub struct ServerOptions {
    /// Bind-Adresse für UDP.
    pub bind: SocketAddr,
    /// Antwortmodus für geblockte Namen.
    pub mode: SinkholeMode,
    /// TTL der Sinkhole-Antworten.
    pub sinkhole_ttl: u32,
    /// Gleichzeitig laufende Queries (Rückstau-Bremse).
    pub max_in_flight: usize,
    /// Einträge im Antwort-Cache; 0 schaltet ihn ab.
    pub cache_entries: usize,
    /// Pro-Query-Logging.
    pub verbose: bool,
}

/// Der UDP-DNS-Server.
pub struct DnsServer {
    pub(super) options: ServerOptions,
    pub(super) engine: Arc<FilterEngine>,
    pub(super) upstream: Arc<Upstream>,
    pub(super) cache: ResponseCache,
    pub(super) stats: Arc<Stats>,
}

impl DnsServer {
    /// Baut den Server samt Antwort-Cache.
    pub fn new(
        options: ServerOptions,
        engine: Arc<FilterEngine>,
        upstream: Arc<Upstream>,
        stats: Arc<Stats>,
    ) -> Self {
        Self {
            cache: ResponseCache::new(options.cache_entries),
            options,
            engine,
            upstream,
            stats,
        }
    }

    /// Bindet den Socket und verarbeitet Queries bis zum ersten Socket-Fehler.
    pub async fn run(self: Arc<Self>) -> io::Result<()> {
        let socket = Arc::new(UdpSocket::bind(self.options.bind).await?);
        let local = socket.local_addr()?;
        println!("[dns] udp listener auf {local}");
        println!("[dns] antwort-cache: {} einträge", self.cache.capacity());
        if local.ip().is_unspecified() {
            println!(
                "[dns] hinweis: 0.0.0.0 gebunden – clients erreichen den dienst über die host-IP"
            );
        }

        let in_flight = Arc::new(Semaphore::new(self.options.max_in_flight.max(1)));
        let mut buf = vec![0u8; MAX_QUERY_BYTES];

        loop {
            let (len, peer) = match socket.recv_from(&mut buf).await {
                Ok(received) => received,
                Err(err) => {
                    // EINTR/ICMP-Fehler dürfen den Listener nicht beenden.
                    eprintln!("[dns] recv_from fehlgeschlagen: {err}");
                    continue;
                }
            };

            let permit = match Arc::clone(&in_flight).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };

            let request = buf[..len].to_vec();
            let server = Arc::clone(&self);
            let socket = Arc::clone(&socket);
            tokio::spawn(async move {
                server.handle(&socket, request, peer).await;
                drop(permit);
            });
        }
        Ok(())
    }

    /// Schickt eine Query durch [`Self::resolve`] und verbucht das Ergebnis.
    ///
    /// Reihenfolge wie bisher: geblockte, gecachte und fehlgeschlagene Queries werden vor
    /// dem Senden gebucht; eine weitergeleitete Antwort erst danach und erst dann in den
    /// Cache, damit ein voller Cache die Antwort an den Client nicht verzögert.
    async fn handle(&self, socket: &UdpSocket, request: Vec<u8>, peer: SocketAddr) {
        let mut reply = self.resolve(&request, peer).await;

        if reply.verdict == Some(Verdict::Forwarded) {
            self.send(socket, &reply, peer).await;
            reply.record(&self.stats);
            if let Some(ttl) = reply.bytes.as_deref().and_then(cache_ttl) {
                // Name und Bytes getrennt ausleihen bzw. verschieben: der Name zeigt in
                // `reply.name`, die Antwortbytes gehen ohne Kopie in den Cache.
                let name = std::str::from_utf8(&reply.name[..reply.name_len]).ok();
                if let Some(name) = name {
                    if let Some(bytes) = reply.bytes.take() {
                        self.cache.insert(name, reply.qtype, bytes, ttl);
                    }
                }
            }
            return;
        }

        reply.record(&self.stats);
        self.send(socket, &reply, peer).await;
    }

    /// Schickt die Antwortbytes; ein Fehlschlag wird geloggt, nicht weitergegeben.
    async fn send(&self, socket: &UdpSocket, reply: &Reply, peer: SocketAddr) {
        let Some(bytes) = reply.bytes.as_ref() else {
            return;
        };
        if let Err(err) = socket.send_to(bytes, peer).await {
            eprintln!(
                "[dns] {} an {peer} fehlgeschlagen: {err}",
                reply.send_label()
            );
        }
    }

    /// Pro-Query-Logging; ohne `verbose` passiert nichts.
    pub(super) fn log(&self, args: std::fmt::Arguments<'_>) {
        if self.options.verbose {
            eprintln!("[dns] {args}");
        }
    }
}

/// Schreibt den Query-Namen kleingeschrieben, ohne Wurzelpunkt, in `out`.
///
/// Gibt die Länge zurück oder `None`, wenn der Name nicht darstellbar ist (zu lang,
/// Nicht-ASCII). Kein Heap, kein `String`.
pub(super) fn normalize_qname(qname: &Name<'_>, out: &mut [u8; 256]) -> Option<usize> {
    let mut len = 0usize;
    for label in qname.as_bytes() {
        if label.is_empty() {
            continue;
        }
        if len > 0 {
            out[len] = b'.';
            len += 1;
        }
        if len + label.len() > out.len() {
            return None;
        }
        for &byte in label {
            if !byte.is_ascii() {
                return None;
            }
            out[len] = byte.to_ascii_lowercase();
            len += 1;
        }
    }
    if len == 0 {
        return None;
    }
    Some(len)
}
