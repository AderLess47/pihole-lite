//! UDP-Listener auf Port 53: parsen, filtern, beantworten oder weiterleiten.
//!
//! Jede Query läuft in einem eigenen Task, aber nur solange ein Permit des Semaphors
//! frei ist. Das ist die Rückstau-Bremse: ohne sie könnten 500 parallele Clients auf
//! einem Pi 1 den Heap sprengen.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use simple_dns::rdata::{RData, A, AAAA};
use simple_dns::{Name, Packet, PacketFlag, ResourceRecord, CLASS, QTYPE, RCODE, TYPE};
use tokio::net::UdpSocket;
use tokio::sync::Semaphore;

use super::cache::{cache_ttl, ResponseCache};
use super::upstream::Upstream;
use crate::filter::engine::FilterEngine;
use crate::stats::ringbuffer::{Stats, Verdict};

/// Maximale Query-Größe über UDP.
const MAX_QUERY_BYTES: usize = 4096;
/// Länge des DNS-Headers.
const HEADER_BYTES: usize = 12;

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
    options: ServerOptions,
    engine: Arc<FilterEngine>,
    upstream: Arc<Upstream>,
    cache: ResponseCache,
    stats: Arc<Stats>,
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

    async fn handle(&self, socket: &UdpSocket, request: Vec<u8>, peer: SocketAddr) {
        if request.len() < HEADER_BYTES {
            self.stats.record_error();
            self.log(format_args!(
                "{peer}: paket zu kurz ({} bytes), verworfen",
                request.len()
            ));
            return;
        }
        let id = u16::from_be_bytes([request[0], request[1]]);

        let packet = match Packet::parse(&request) {
            Ok(packet) => packet,
            Err(err) => {
                self.stats.record_error();
                self.log(format_args!(
                    "{peer}: parse fehlgeschlagen ({err}), FORMERR"
                ));
                self.send_error(socket, peer, id, RCODE::FormatError).await;
                return;
            }
        };
        // Antworten anderer Server nehmen wir nicht an (kein Reflection-Loop).
        if packet.has_flags(PacketFlag::RESPONSE) {
            self.stats.record_error();
            self.log(format_args!("{peer}: response statt query ignoriert"));
            return;
        }
        let Some(question) = packet.questions.first() else {
            self.stats.record_error();
            self.send_error(socket, peer, id, RCODE::FormatError).await;
            return;
        };

        // Name ohne Heap normalisieren: [u8; 256] reicht für jedes DNS-Label-Set.
        let mut name_buf = [0u8; 256];
        // Nicht-ASCII (escapete Labels) geht ungefiltert zum Upstream.
        let name = normalize_qname(&question.qname, &mut name_buf)
            .and_then(|len| std::str::from_utf8(&name_buf[..len]).ok());
        let qtype: u16 = question.qtype.into();

        if let Some(name) = name {
            if self.engine.is_blocked(name) {
                self.stats.record(name, qtype, Verdict::Blocked);
                self.log(format_args!("{peer}: {name} -> geblockt"));
                self.send_sinkhole(socket, peer, id, &packet, question)
                    .await;
                return;
            }

            if let Some(cached) = self.cache.lookup(name, qtype, id) {
                self.stats.record(name, qtype, Verdict::Cached);
                self.log(format_args!("{peer}: {name} -> cache-treffer"));
                if let Err(err) = socket.send_to(&cached, peer).await {
                    eprintln!("[dns] cache-antwort an {peer} fehlgeschlagen: {err}");
                }
                return;
            }
        }

        match self.upstream.query(&request, packet.id()).await {
            Ok(response) => {
                if let Err(err) = socket.send_to(&response, peer).await {
                    eprintln!("[dns] antwort an {peer} fehlgeschlagen: {err}");
                }
                // Erst nach dem Senden cachen: ein voller Cache darf die Antwort an den
                // Client weder verzögern noch verhindern.
                if let Some(name) = name {
                    self.stats.record(name, qtype, Verdict::Forwarded);
                    if let Some(ttl) = cache_ttl(&response) {
                        self.cache.insert(name, qtype, response, ttl);
                    }
                } else {
                    self.stats.record_error();
                }
            }
            Err(err) => {
                if let Some(name) = name {
                    self.stats.record(name, qtype, Verdict::Error);
                } else {
                    self.stats.record_error();
                }
                self.log(format_args!(
                    "{peer}: upstream fehlgeschlagen ({err}), SERVFAIL"
                ));
                self.send_error(socket, peer, packet.id(), RCODE::ServerFailure)
                    .await;
            }
        }
    }

    /// Baut die Sinkhole-Antwort (0.0.0.0 / :: / NXDOMAIN) und schickt sie.
    async fn send_sinkhole(
        &self,
        socket: &UdpSocket,
        peer: SocketAddr,
        id: u16,
        packet: &Packet<'_>,
        question: &simple_dns::Question<'_>,
    ) {
        let mut reply = Packet::new_reply(id);
        reply.set_flags(PacketFlag::RECURSION_AVAILABLE);
        if packet.has_flags(PacketFlag::RECURSION_DESIRED) {
            reply.set_flags(PacketFlag::RECURSION_DESIRED);
        }
        reply.questions.push(question.clone());

        match self.options.mode {
            SinkholeMode::NxDomain => *reply.rcode_mut() = RCODE::NameError,
            SinkholeMode::ZeroIp => match question.qtype {
                QTYPE::TYPE(TYPE::A) => reply.answers.push(ResourceRecord::new(
                    question.qname.clone(),
                    CLASS::IN,
                    self.options.sinkhole_ttl,
                    RData::A(A { address: 0 }),
                )),
                QTYPE::TYPE(TYPE::AAAA) => reply.answers.push(ResourceRecord::new(
                    question.qname.clone(),
                    CLASS::IN,
                    self.options.sinkhole_ttl,
                    RData::AAAA(AAAA { address: 0 }),
                )),
                // Andere Typen: leeres NOERROR, damit Clients nicht auf TCP ausweichen.
                _ => {}
            },
        }

        match reply.build_bytes_vec() {
            Ok(bytes) => {
                if let Err(err) = socket.send_to(&bytes, peer).await {
                    eprintln!("[dns] sinkhole-antwort an {peer} fehlgeschlagen: {err}");
                }
            }
            Err(err) => eprintln!("[dns] konnte sinkhole-antwort nicht serialisieren: {err}"),
        }
    }

    /// Minimale Fehlerantwort ohne Question-Section.
    async fn send_error(&self, socket: &UdpSocket, peer: SocketAddr, id: u16, code: RCODE) {
        let mut reply = Packet::new_reply(id);
        reply.set_flags(PacketFlag::RECURSION_AVAILABLE);
        *reply.rcode_mut() = code;
        if let Ok(bytes) = reply.build_bytes_vec() {
            if let Err(err) = socket.send_to(&bytes, peer).await {
                eprintln!("[dns] fehlerantwort an {peer} fehlgeschlagen: {err}");
            }
        }
    }

    fn log(&self, args: std::fmt::Arguments<'_>) {
        if self.options.verbose {
            eprintln!("[dns] {args}");
        }
    }
}

/// Schreibt den Query-Namen kleingeschrieben, ohne Wurzelpunkt, in `out`.
///
/// Gibt die Länge zurück oder `None`, wenn der Name nicht darstellbar ist (zu lang,
/// Nicht-ASCII). Kein Heap, kein `String`.
fn normalize_qname(qname: &Name<'_>, out: &mut [u8; 256]) -> Option<usize> {
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
