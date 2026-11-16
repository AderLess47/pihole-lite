//! Antwort-Cache: LRU mit TTL-Ablauf, ohne zusätzliche Dependency.
//!
//! Aufbau: ein Slab aus Slots plus HashMap auf `(xxh3(name), qtype)`. Der Name liegt
//! zusätzlich im Slot und wird beim Lookup verglichen – eine Hash-Kollision wird als
//! Miss behandelt, nie als falscher Treffer. Alle Listenoperationen sind O(1), es gibt
//! keine Allokation pro Lookup.
//!
//! TTL-Behandlung im Wire-Format: beim Einfügen werden die TTLs in der Antwort auf die
//! Cache-Lebensdauer gekappt, beim Treffer um das vergangene Alter dekrementiert. Das
//! läuft als ein Durchlauf über die Records (mit Namens-Kompression), damit weder
//! Parsen noch Neubauen pro Treffer nötig ist. Das ID-Feld wird auf die aktuelle Query
//! gesetzt. OPT-Records werden ausgelassen: ihr "TTL"-Feld trägt EDNS-Flags.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use simple_dns::{Packet, PacketFlag, QTYPE, RCODE};
use xxhash_rust::xxh3::xxh3_64;

/// Kleinste TTL, die überhaupt gecacht wird; 0 heißt "nur für diese Transaktion".
const MIN_CACHE_TTL_SECS: u32 = 1;
/// Größte TTL, die wir akzeptieren – auch wenn der Upstream mehr liefert.
const MAX_CACHE_TTL_SECS: u32 = 3600;
/// TTL für negative Antworten (NXDOMAIN/NODATA).
const NEGATIVE_TTL_SECS: u32 = 60;
/// Typ-Code von OPT-Records (RFC 6891).
const OPT_TYPE_CODE: u16 = 41;
/// Länge des DNS-Headers.
const HEADER_BYTES: usize = 12;
/// Sentinel für "kein Index" in der LRU-Liste.
const NIL: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    name_hash: u64,
    qtype: u16,
}

impl Key {
    fn new(name: &str, qtype: u16) -> Self {
        Self {
            name_hash: xxh3_64(name.as_bytes()),
            qtype,
        }
    }
}

struct Slot {
    key: Key,
    name: Box<str>,
    response: Vec<u8>,
    inserted: Instant,
    expires_at: Instant,
    prev: u32,
    next: u32,
}

/// LRU-Liste plus HashMap; liegt hinter einem `Mutex` in [`ResponseCache`].
struct Inner {
    slots: Vec<Slot>,
    free: Vec<u32>,
    map: HashMap<Key, u32>,
    head: u32,
    tail: u32,
}

impl Inner {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            map: HashMap::new(),
            head: NIL,
            tail: NIL,
        }
    }

    fn unlink(&mut self, index: u32) {
        let (prev, next) = {
            let slot = &self.slots[index as usize];
            (slot.prev, slot.next)
        };
        if prev == NIL {
            self.head = next;
        } else {
            self.slots[prev as usize].next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.slots[next as usize].prev = prev;
        }
        let slot = &mut self.slots[index as usize];
        slot.prev = NIL;
        slot.next = NIL;
    }

    fn push_front(&mut self, index: u32) {
        let old_head = self.head;
        {
            let slot = &mut self.slots[index as usize];
            slot.prev = NIL;
            slot.next = old_head;
        }
        match old_head {
            NIL => self.tail = index,
            head => self.slots[head as usize].prev = index,
        }
        self.head = index;
    }

    fn touch(&mut self, index: u32) {
        if self.head == index {
            return;
        }
        self.unlink(index);
        self.push_front(index);
    }

    /// Entfernt einen Slot aus Liste und Map und gibt seinen Speicher frei.
    fn remove(&mut self, index: u32) {
        let key = self.slots[index as usize].key;
        self.unlink(index);
        self.map.remove(&key);
        let slot = &mut self.slots[index as usize];
        slot.name = Box::default();
        slot.response = Vec::new();
        self.free.push(index);
    }

    fn evict_lru(&mut self) {
        if self.tail != NIL {
            let tail = self.tail;
            self.remove(tail);
        }
    }

    fn insert_slot(&mut self, slot: Slot) {
        let key = slot.key;
        let index = match self.free.pop() {
            Some(index) => {
                self.slots[index as usize] = slot;
                index
            }
            None => {
                self.slots.push(slot);
                (self.slots.len() - 1) as u32
            }
        };
        self.map.insert(key, index);
        self.push_front(index);
    }
}

/// LRU-Cache für rohe DNS-Antworten.
pub struct ResponseCache {
    capacity: usize,
    inner: Mutex<Inner>,
}

impl ResponseCache {
    /// Erzeugt einen Cache mit `capacity` Einträgen (0 schaltet ihn ab).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(Inner::new()),
        }
    }

    /// Konfigurierte Kapazität (für Logs).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Ein vergifteter Lock darf den DNS-Dienst nicht mitreißen.
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// Sucht eine Antwort und liefert sie mit aktueller ID und gealterten TTLs.
    pub fn lookup(&self, name: &str, qtype: u16, id: u16) -> Option<Vec<u8>> {
        self.lookup_at(name, qtype, id, Instant::now())
    }

    fn lookup_at(&self, name: &str, qtype: u16, id: u16, now: Instant) -> Option<Vec<u8>> {
        if self.capacity == 0 {
            return None;
        }

        let key = Key::new(name, qtype);
        let mut inner = self.lock();
        let index = *inner.map.get(&key)?;

        let (matches, expired) = {
            let slot = &inner.slots[index as usize];
            (slot.name.as_ref() == name, slot.expires_at <= now)
        };
        if !matches {
            // Hash-Kollision: nicht als Treffer verkaufen.
            return None;
        }
        if expired {
            inner.remove(index);
            return None;
        }

        let inserted = inner.slots[index as usize].inserted;
        let elapsed_secs = now.saturating_duration_since(inserted).as_secs();
        let elapsed_secs = elapsed_secs.min(u32::MAX as u64) as u32;

        inner.touch(index);
        let mut response = inner.slots[index as usize].response.clone();
        drop(inner);

        if response.len() < HEADER_BYTES || !age_ttls(&mut response, elapsed_secs) {
            return None;
        }
        response[0..2].copy_from_slice(&id.to_be_bytes());
        Some(response)
    }

    /// Legt eine Antwort ab; `ttl` begrenzt die Lebensdauer im Cache.
    pub fn insert(&self, name: &str, qtype: u16, response: Vec<u8>, ttl: Duration) {
        self.insert_at(name, qtype, response, ttl, Instant::now());
    }

    fn insert_at(&self, name: &str, qtype: u16, response: Vec<u8>, ttl: Duration, now: Instant) {
        if self.capacity == 0 || ttl.is_zero() {
            return;
        }
        let ttl_secs = ttl
            .as_secs()
            .min(MAX_CACHE_TTL_SECS as u64)
            .min(u32::MAX as u64) as u32;
        if ttl_secs < MIN_CACHE_TTL_SECS {
            return;
        }
        let ttl = Duration::from_secs(ttl_secs as u64);

        let mut response = response;
        if response.len() < HEADER_BYTES || !clamp_ttls(&mut response, ttl_secs) {
            return;
        }
        response.shrink_to_fit();

        let key = Key::new(name, qtype);
        let expires_at = now + ttl;
        let mut inner = self.lock();

        if let Some(&index) = inner.map.get(&key) {
            let matches = inner.slots[index as usize].name.as_ref() == name;
            if matches {
                {
                    let slot = &mut inner.slots[index as usize];
                    slot.response = response;
                    slot.inserted = now;
                    slot.expires_at = expires_at;
                }
                inner.touch(index);
                return;
            }
            // Kollision: alten Eintrag entfernen, sonst zeigt die Map auf einen
            // Slot, den das spätere Aufräumen nicht mehr findet.
            inner.remove(index);
        }

        if inner.map.len() >= self.capacity {
            inner.evict_lru();
        }
        inner.insert_slot(Slot {
            key,
            name: name.into(),
            response,
            inserted: now,
            expires_at,
            prev: NIL,
            next: NIL,
        });
    }
}

/// Ermittelt die TTL, unter der eine Antwort gecacht werden darf.
///
/// `None` heißt: nicht cachen (Fehler-RCODE, Truncation, 0-TTL, Zone-Transfer).
pub fn cache_ttl(response: &[u8]) -> Option<Duration> {
    let packet = Packet::parse(response).ok()?;
    if !packet.has_flags(PacketFlag::RESPONSE) || packet.has_flags(PacketFlag::TRUNCATION) {
        return None;
    }
    match packet.rcode() {
        RCODE::NoError | RCODE::NameError => {}
        _ => return None,
    }
    let question = packet.questions.first()?;
    if packet.questions.len() != 1 {
        return None;
    }
    match question.qtype {
        QTYPE::ANY | QTYPE::AXFR | QTYPE::IXFR => return None,
        QTYPE::TYPE(_) => {}
        _ => return None,
    }

    let mut min_ttl = u32::MAX;
    for record in &packet.answers {
        min_ttl = min_ttl.min(record.ttl);
    }

    let seconds = if min_ttl == u32::MAX {
        // Negative Antwort ohne SOA-Auswertung: konservativ kurz cachen.
        NEGATIVE_TTL_SECS
    } else {
        min_ttl.min(MAX_CACHE_TTL_SECS)
    };
    if seconds < MIN_CACHE_TTL_SECS {
        return None;
    }
    Some(Duration::from_secs(seconds as u64))
}

/// Kappt alle Record-TTLs auf `cap_secs`; `false` bei kaputtem Paket.
fn clamp_ttls(buf: &mut [u8], cap_secs: u32) -> bool {
    patch_record_ttls(buf, |ttl| {
        let value = read_u32(ttl).min(cap_secs);
        ttl.copy_from_slice(&value.to_be_bytes());
    })
}

/// Zieht alle Record-TTLs um `elapsed_secs` herunter.
fn age_ttls(buf: &mut [u8], elapsed_secs: u32) -> bool {
    patch_record_ttls(buf, |ttl| {
        let value = read_u32(ttl).saturating_sub(elapsed_secs);
        ttl.copy_from_slice(&value.to_be_bytes());
    })
}

/// Läuft über alle Records und ruft `patch` mit den vier TTL-Bytes auf.
///
/// OPT-Records werden übersprungen: ihr TTL-Feld enthält Extended-RCODE und Flags.
fn patch_record_ttls(buf: &mut [u8], mut patch: impl FnMut(&mut [u8])) -> bool {
    if buf.len() < HEADER_BYTES {
        return false;
    }
    let questions = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let answers = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    let authorities = u16::from_be_bytes([buf[8], buf[9]]) as usize;
    let additional = u16::from_be_bytes([buf[10], buf[11]]) as usize;

    let mut pos = HEADER_BYTES;

    // Question-Section: Name plus QTYPE/QCLASS.
    for _ in 0..questions {
        pos = match skip_name(buf, pos) {
            Some(next) => next,
            None => return false,
        };
        pos += 4;
        if pos > buf.len() {
            return false;
        }
    }

    for _ in 0..(answers + authorities + additional) {
        pos = match skip_name(buf, pos) {
            Some(next) => next,
            None => return false,
        };
        if pos + 10 > buf.len() {
            return false;
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let rdlength = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        if rtype != OPT_TYPE_CODE {
            patch(&mut buf[pos + 4..pos + 8]);
        }
        pos += 10 + rdlength;
        if pos > buf.len() {
            return false;
        }
    }
    true
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Überspringt einen (ggf. komprimierten) Namen und liefert die Position danach.
fn skip_name(buf: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let length = *buf.get(pos)? as usize;
        match length & 0xC0 {
            // Kompressionszeiger: zwei Bytes, danach ist der Name zu Ende.
            0xC0 => {
                if pos + 2 > buf.len() {
                    return None;
                }
                return Some(pos + 2);
            }
            0 => {
                if length == 0 {
                    return Some(pos + 1);
                }
                pos += 1 + length;
                if pos > buf.len() {
                    return None;
                }
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_dns::rdata::{RData, A};
    use simple_dns::{Name, Question, ResourceRecord, CLASS, QCLASS, TYPE};

    /// Antwort mit einem A-Record (TTL `ttl`) auf `name`.
    fn a_response(id: u16, name: &str, ttl: u32, ip: [u8; 4]) -> Vec<u8> {
        let mut packet = Packet::new_reply(id);
        packet.questions.push(Question::new(
            Name::new(name).unwrap(),
            QTYPE::TYPE(TYPE::A),
            QCLASS::CLASS(CLASS::IN),
            false,
        ));
        packet.answers.push(ResourceRecord::new(
            Name::new(name).unwrap(),
            CLASS::IN,
            ttl,
            RData::A(A {
                address: u32::from_be_bytes(ip),
            }),
        ));
        packet.build_bytes_vec().unwrap()
    }

    /// Handgebautes Paket mit einem OPT-Record; liefert (Bytes, OPT-TTL-Offset).
    fn wire_with_opt(answer_ttl: u32, opt_ttl: u32) -> (Vec<u8>, usize) {
        let mut wire = Vec::new();
        wire.extend_from_slice(&0x1234u16.to_be_bytes()); // ID
        wire.extend_from_slice(&0x8180u16.to_be_bytes()); // QR, RD, RA, NOERROR
        wire.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        wire.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT
        wire.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
        wire.extend_from_slice(&1u16.to_be_bytes()); // ARCOUNT
                                                     // Frage: x.test A IN
        wire.extend_from_slice(&[1, b'x', 4, b't', b'e', b's', b't', 0]);
        wire.extend_from_slice(&1u16.to_be_bytes());
        wire.extend_from_slice(&1u16.to_be_bytes());
        // Antwort: Zeiger auf die Frage, A, IN, TTL, 4 Byte Adresse
        wire.extend_from_slice(&[0xC0, 0x0C]);
        wire.extend_from_slice(&1u16.to_be_bytes());
        wire.extend_from_slice(&1u16.to_be_bytes());
        wire.extend_from_slice(&answer_ttl.to_be_bytes());
        wire.extend_from_slice(&4u16.to_be_bytes());
        wire.extend_from_slice(&[1, 2, 3, 4]);
        // OPT: Wurzelname, Typ 41, Klasse 1232, TTL-Feld mit DO-Bit, keine Daten
        wire.push(0);
        wire.extend_from_slice(&OPT_TYPE_CODE.to_be_bytes());
        wire.extend_from_slice(&1232u16.to_be_bytes());
        let opt_offset = wire.len();
        wire.extend_from_slice(&opt_ttl.to_be_bytes());
        wire.extend_from_slice(&0u16.to_be_bytes());
        (wire, opt_offset)
    }

    #[test]
    fn hit_rewrites_id_and_ages_ttls() {
        let cache = ResponseCache::new(8);
        let now = Instant::now();
        let response = a_response(1, "example.com", 300, [1, 2, 3, 4]);
        cache.insert_at(
            "example.com",
            1,
            response,
            Duration::from_secs(300),
            now - Duration::from_secs(100),
        );

        let hit = cache
            .lookup_at("example.com", 1, 4242, now)
            .expect("Treffer");
        let packet = Packet::parse(&hit).unwrap();
        assert_eq!(4242, packet.id());
        assert_eq!(200, packet.answers[0].ttl);
    }

    #[test]
    fn expired_entries_are_dropped() {
        let cache = ResponseCache::new(8);
        let now = Instant::now();
        cache.insert_at(
            "old.test",
            1,
            a_response(1, "old.test", 5, [1, 1, 1, 1]),
            Duration::from_secs(5),
            now - Duration::from_secs(10),
        );
        assert!(cache.lookup_at("old.test", 1, 7, now).is_none());

        // Der Slot ist wieder frei und kann neu befüllt werden.
        cache.insert_at(
            "old.test",
            1,
            a_response(2, "old.test", 60, [1, 1, 1, 1]),
            Duration::from_secs(60),
            now,
        );
        assert!(cache.lookup_at("old.test", 1, 8, now).is_some());
    }

    #[test]
    fn evicts_least_recently_used_entry() {
        let cache = ResponseCache::new(2);
        let now = Instant::now();
        for (name, ip) in [("a.test", [1, 1, 1, 1]), ("b.test", [2, 2, 2, 2])] {
            cache.insert_at(
                name,
                1,
                a_response(1, name, 60, ip),
                Duration::from_secs(60),
                now,
            );
        }
        // a wird zum MRU-Eintrag, b ist danach der LRU.
        assert!(cache.lookup_at("a.test", 1, 2, now).is_some());

        cache.insert_at(
            "c.test",
            1,
            a_response(3, "c.test", 60, [3, 3, 3, 3]),
            Duration::from_secs(60),
            now,
        );

        assert!(cache.lookup_at("b.test", 1, 4, now).is_none());
        assert!(cache.lookup_at("a.test", 1, 5, now).is_some());
        assert!(cache.lookup_at("c.test", 1, 6, now).is_some());
    }

    #[test]
    fn qtype_and_name_are_part_of_the_key() {
        let cache = ResponseCache::new(8);
        let now = Instant::now();
        cache.insert_at(
            "a.test",
            1,
            a_response(1, "a.test", 60, [1, 2, 3, 4]),
            Duration::from_secs(60),
            now,
        );
        assert!(cache.lookup_at("a.test", 28, 1, now).is_none());
        assert!(cache.lookup_at("b.test", 1, 1, now).is_none());
        assert!(cache.lookup_at("a.test", 1, 1, now).is_some());
    }

    #[test]
    fn opt_record_ttl_is_not_touched() {
        let cache = ResponseCache::new(8);
        let now = Instant::now();
        let (wire, opt_offset) = wire_with_opt(300, 0x0000_8000);
        cache.insert_at(
            "x.test",
            1,
            wire,
            Duration::from_secs(300),
            now - Duration::from_secs(50),
        );

        let hit = cache.lookup_at("x.test", 1, 9, now).expect("Treffer");
        assert_eq!(
            &0x0000_8000u32.to_be_bytes(),
            &hit[opt_offset..opt_offset + 4],
            "EDNS-Flags dürfen nicht als TTL gealtert werden"
        );
        // Antwort-TTL: Header 12 + Frage 12 + Name-Zeiger 2 + Typ/Klasse 4 = Offset 30.
        assert_eq!(250, read_u32(&hit[30..34]));
    }

    #[test]
    fn cache_ttl_uses_min_answer_ttl_and_clamps() {
        assert_eq!(
            Some(Duration::from_secs(300)),
            cache_ttl(&a_response(1, "x.test", 300, [1, 2, 3, 4]))
        );
        assert_eq!(
            Some(Duration::from_secs(MAX_CACHE_TTL_SECS as u64)),
            cache_ttl(&a_response(1, "x.test", 86_400, [1, 2, 3, 4]))
        );
        assert_eq!(None, cache_ttl(&a_response(1, "x.test", 0, [1, 2, 3, 4])));
        assert_eq!(None, cache_ttl(&[0u8; 4]));
    }

    #[test]
    fn negative_answers_get_a_short_ttl() {
        let mut packet = Packet::new_reply(1);
        packet.questions.push(Question::new(
            Name::new("nx.test").unwrap(),
            QTYPE::TYPE(TYPE::A),
            QCLASS::CLASS(CLASS::IN),
            false,
        ));
        *packet.rcode_mut() = RCODE::NameError;
        let bytes = packet.build_bytes_vec().unwrap();

        assert_eq!(
            Some(Duration::from_secs(NEGATIVE_TTL_SECS as u64)),
            cache_ttl(&bytes)
        );
    }
}
