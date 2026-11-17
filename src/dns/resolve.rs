//! Entscheidung über eine Query: filtern, Cache, Upstream – ohne Socket.
//!
//! Der Loop in [`super::server`] ruft nur [`DnsServer::resolve`], sendet die Bytes aus dem
//! [`Reply`] und bucht das Verdict. Dadurch sind alle Antwortpfade ohne Listener, ohne
//! Socket und ohne echtes Warten prüfbar; die Tests liegen in `resolve/tests.rs`.

use std::net::SocketAddr;

use simple_dns::rdata::{RData, A, AAAA};
use simple_dns::{Packet, PacketFlag, ResourceRecord, CLASS, QTYPE, RCODE, TYPE};

use super::server::{normalize_qname, DnsServer, SinkholeMode, HEADER_BYTES};
use crate::stats::ringbuffer::{Stats, Verdict};

impl DnsServer {
    /// Entscheidet anhand der Bytes, wie die Query beantwortet wird.
    ///
    /// Kein Socket, keine Statistik: das Ergebnis steht im [`Reply`] und wird vom
    /// Aufrufer gesendet und gebucht. Der Name entsteht direkt im Reply-Puffer
    /// ([`normalize_qname`]), also ohne Heap und ohne Kopie.
    pub(super) async fn resolve(&self, request: &[u8], peer: SocketAddr) -> Reply {
        let mut reply = Reply::none();

        if request.len() < HEADER_BYTES {
            self.log(format_args!(
                "{peer}: paket zu kurz ({} bytes), verworfen",
                request.len()
            ));
            return reply;
        }
        let id = u16::from_be_bytes([request[0], request[1]]);

        let packet = match Packet::parse(request) {
            Ok(packet) => packet,
            Err(err) => {
                self.log(format_args!(
                    "{peer}: parse fehlgeschlagen ({err}), FORMERR"
                ));
                reply.bytes = self.build_error(id, RCODE::FormatError);
                return reply;
            }
        };
        // Antworten anderer Server nehmen wir nicht an (kein Reflection-Loop).
        if packet.has_flags(PacketFlag::RESPONSE) {
            self.log(format_args!("{peer}: response statt query ignoriert"));
            return reply;
        }
        let Some(question) = packet.questions.first() else {
            reply.bytes = self.build_error(id, RCODE::FormatError);
            return reply;
        };

        // Nicht-ASCII (escapete Labels) geht ungefiltert zum Upstream.
        reply.name_len = normalize_qname(&question.qname, &mut reply.name).unwrap_or(0);
        reply.qtype = question.qtype.into();
        let name = std::str::from_utf8(&reply.name[..reply.name_len]).ok();

        if let Some(name) = name {
            if self.engine.is_blocked(name) {
                self.log(format_args!("{peer}: {name} -> geblockt"));
                reply.verdict = Some(Verdict::Blocked);
                reply.bytes = self.build_sinkhole(id, &packet, question);
                return reply;
            }

            if let Some(cached) = self.cache.lookup(name, reply.qtype, id) {
                self.log(format_args!("{peer}: {name} -> cache-treffer"));
                reply.verdict = Some(Verdict::Cached);
                reply.bytes = Some(cached);
                return reply;
            }
        }

        match self.upstream.query(request, packet.id()).await {
            Ok(response) => {
                // Ohne Namen (Nicht-ASCII) bleibt es bei record_error, gesendet wird
                // die Antwort trotzdem.
                reply.verdict = reply.has_name().then_some(Verdict::Forwarded);
                reply.bytes = Some(response);
            }
            Err(err) => {
                self.log(format_args!(
                    "{peer}: upstream fehlgeschlagen ({err}), SERVFAIL"
                ));
                reply.verdict = reply.has_name().then_some(Verdict::Error);
                reply.bytes = self.build_error(packet.id(), RCODE::ServerFailure);
            }
        }
        reply
    }

    /// Baut die Sinkhole-Antwort (0.0.0.0 / :: / NXDOMAIN).
    fn build_sinkhole(
        &self,
        id: u16,
        packet: &Packet<'_>,
        question: &simple_dns::Question<'_>,
    ) -> Option<Vec<u8>> {
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
            Ok(bytes) => Some(bytes),
            Err(err) => {
                eprintln!("[dns] konnte sinkhole-antwort nicht serialisieren: {err}");
                None
            }
        }
    }

    /// Minimale Fehlerantwort ohne Question-Section.
    fn build_error(&self, id: u16, code: RCODE) -> Option<Vec<u8>> {
        let mut reply = Packet::new_reply(id);
        reply.set_flags(PacketFlag::RECURSION_AVAILABLE);
        *reply.rcode_mut() = code;
        reply.build_bytes_vec().ok()
    }
}

/// Ergebnis einer Query, bevor der Socket sie sieht.
///
/// Trägt alles, was zum Senden, Buchen und Cachen nötig ist: Antwortbytes, Verdict und
/// den normalisierten Namen als Puffer (kein `String`, keine Allokation pro Query).
pub(super) struct Reply {
    /// Antwortbytes; `None`, wenn nichts gesendet wird (Paket zu kurz, Antwort statt Query).
    pub(super) bytes: Option<Vec<u8>>,
    /// Wie die Query beantwortet wurde; `None` = Fehler ohne Domain (`record_error`).
    pub(super) verdict: Option<Verdict>,
    /// QTYPE der Query (0, wenn kein Name vorliegt).
    pub(super) qtype: u16,
    /// Normalisierter Query-Name, gefüllt von [`normalize_qname`].
    pub(super) name: [u8; 256],
    pub(super) name_len: usize,
}

impl Reply {
    fn none() -> Self {
        Self {
            bytes: None,
            verdict: None,
            qtype: 0,
            name: [0; 256],
            name_len: 0,
        }
    }

    fn has_name(&self) -> bool {
        self.name_len > 0
    }

    /// Bezeichnung der Antwort für Logzeilen bei einem Sendefehler.
    pub(super) fn send_label(&self) -> &'static str {
        match self.verdict {
            Some(Verdict::Blocked) => "sinkhole-antwort",
            Some(Verdict::Cached) => "cache-antwort",
            Some(Verdict::Forwarded) => "antwort",
            Some(Verdict::Error) | None => "fehlerantwort",
        }
    }

    /// Bucht das Ergebnis in die Statistik.
    ///
    /// Ohne Namen (zu kurz, kaputt, Antwort statt Query, Nicht-ASCII) zählt das als
    /// Fehler – genau wie vorher im Socket-Loop.
    pub(super) fn record(&self, stats: &Stats) {
        match self.verdict {
            Some(verdict) => match std::str::from_utf8(&self.name[..self.name_len]) {
                Ok(name) => stats.record(name, self.qtype, verdict),
                Err(_) => stats.record_error(),
            },
            None => stats.record_error(),
        }
    }
}

#[cfg(test)]
mod tests;
