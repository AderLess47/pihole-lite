//! Lock-freier Ringbuffer für die letzten Queries plus atomare Zähler.
//!
//! Der Ring hat eine feste Kapazität und besteht aus Slots mit je einem Seqlock:
//! Schreiber erhöhen `seq` auf einen ungeraden Wert, schreiben Name und Metadaten und
//! erhöhen `seq` wieder auf gerade. Leser prüfen `seq` vor und nach dem Kopieren und
//! überspringen den Slot, wenn sich währenddessen etwas geändert hat. Es gibt keinen
//! Mutex und keine Allokation beim Schreiben; nur ein Snapshot baut Strings.
//!
//! Namen werden auf [`MAX_NAME_BYTES`] gekürzt – ein Log-Eintrag soll nicht mehr RAM
//! belegen als ein paar Dutzend Bytes.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

/// Maximale Namenslänge pro Log-Eintrag.
const MAX_NAME_BYTES: usize = 96;

/// Ergebnis einer Query aus Sicht des Servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Verdict {
    /// Vom Filter geblockt (Sinkhole).
    Blocked = 0,
    /// Aus dem Antwort-Cache beantwortet.
    Cached = 1,
    /// Vom Upstream geholt.
    Forwarded = 2,
    /// Upstream-Fehler (SERVFAIL).
    Error = 3,
}

impl Verdict {
    /// Anzeigename für JSON und Logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Cached => "cached",
            Self::Forwarded => "forwarded",
            Self::Error => "error",
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Blocked),
            1 => Some(Self::Cached),
            2 => Some(Self::Forwarded),
            3 => Some(Self::Error),
            _ => None,
        }
    }
}

/// Ein Eintrag im Query-Log.
#[derive(Debug, Clone)]
pub struct QueryEntry {
    /// Alter des Eintrags beim Erzeugen des Snapshots, in Sekunden.
    pub age_secs: u32,
    /// Gefragter Name (ggf. gekürzt).
    pub name: String,
    /// QTYPE als Zahl (1 = A, 28 = AAAA, ...).
    pub qtype: u16,
    /// Wie die Query beantwortet wurde.
    pub verdict: Verdict,
}

struct Slot {
    /// Seqlock: 0 = leer, ungerade = wird geschrieben, gerade = gültig.
    seq: AtomicU32,
    /// `timestamp << 32 | qtype << 16 | verdict << 8 | name_len`.
    meta: AtomicU64,
    name: [AtomicU8; MAX_NAME_BYTES],
}

/// Fester Ring der letzten Queries.
pub struct QueryLog {
    slots: Box<[Slot]>,
    head: AtomicU64,
}

impl QueryLog {
    /// Erzeugt einen Ring mit `capacity` Einträgen (mindestens 1).
    pub fn new(capacity: usize) -> Self {
        let slots = (0..capacity.max(1))
            .map(|_| Slot {
                seq: AtomicU32::new(0),
                meta: AtomicU64::new(0),
                name: std::array::from_fn(|_| AtomicU8::new(0)),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            head: AtomicU64::new(0),
        }
    }

    /// Schreibt einen Eintrag; überschreibt dabei den ältesten Slot.
    pub fn record(&self, name: &str, qtype: u16, verdict: Verdict, timestamp: u32) {
        let index = self.head.fetch_add(1, Ordering::Relaxed) as usize % self.slots.len();
        let slot = &self.slots[index];

        // Ungerade Sequenznummer markiert den Slot als "wird geschrieben".
        slot.seq.fetch_add(1, Ordering::Acquire);

        let bytes = name.as_bytes();
        let length = bytes.len().min(MAX_NAME_BYTES);
        for (index, byte) in bytes[..length].iter().enumerate() {
            slot.name[index].store(*byte, Ordering::Relaxed);
        }
        slot.meta.store(
            pack_meta(timestamp, qtype, verdict, length),
            Ordering::Relaxed,
        );

        slot.seq.fetch_add(1, Ordering::Release);
    }

    /// Liefert die neuesten `limit` Einträge, neueste zuerst.
    pub fn snapshot(&self, limit: usize, now: u32) -> Vec<QueryEntry> {
        let head = self.head.load(Ordering::Relaxed);
        let available = (head as usize).min(self.slots.len());
        let count = limit.min(available);

        let mut entries = Vec::with_capacity(count);
        for offset in 0..count {
            let index = (head as usize - 1 - offset) % self.slots.len();
            if let Some(entry) = self.read_slot(index, now) {
                entries.push(entry);
            }
        }
        entries
    }

    fn read_slot(&self, index: usize, now: u32) -> Option<QueryEntry> {
        let slot = &self.slots[index];

        // Drei Versuche: bei einem gleichzeitigen Schreibvorgang wird der Slot
        // übersprungen, statt inkonsistente Daten auszuliefern.
        for _ in 0..3 {
            let before = slot.seq.load(Ordering::Acquire);
            if before == 0 || before % 2 == 1 {
                return None;
            }

            let meta = slot.meta.load(Ordering::Relaxed);
            let (timestamp, qtype, verdict_code, length) = unpack_meta(meta);
            let length = length.min(MAX_NAME_BYTES);

            let mut bytes = [0u8; MAX_NAME_BYTES];
            for (index, byte) in bytes[..length].iter_mut().enumerate() {
                *byte = slot.name[index].load(Ordering::Relaxed);
            }

            if slot.seq.load(Ordering::Acquire) != before {
                continue;
            }

            return Some(QueryEntry {
                age_secs: now.saturating_sub(timestamp),
                name: String::from_utf8_lossy(&bytes[..length]).into_owned(),
                qtype,
                verdict: Verdict::from_code(verdict_code)?,
            });
        }
        None
    }
}

fn pack_meta(timestamp: u32, qtype: u16, verdict: Verdict, length: usize) -> u64 {
    ((timestamp as u64) << 32)
        | ((qtype as u64) << 16)
        | ((verdict as u64) << 8)
        | (length as u64 & 0xFF)
}

fn unpack_meta(meta: u64) -> (u32, u16, u8, usize) {
    (
        (meta >> 32) as u32,
        ((meta >> 16) & 0xFFFF) as u16,
        ((meta >> 8) & 0xFF) as u8,
        (meta & 0xFF) as usize,
    )
}

/// Momentaufnahme für `/api/stats`.
pub struct Snapshot {
    /// Laufzeit seit dem Start.
    pub uptime: Duration,
    /// Beantwortete Queries insgesamt.
    pub queries: u64,
    /// Davon geblockt.
    pub blocked: u64,
    /// Davon aus dem Cache.
    pub cached: u64,
    /// Davon vom Upstream.
    pub forwarded: u64,
    /// Upstream-Fehler.
    pub errors: u64,
    /// Die letzten Queries, neueste zuerst.
    pub recent: Vec<QueryEntry>,
}

/// Zähler plus Query-Log.
pub struct Stats {
    started: Instant,
    log: QueryLog,
    queries: AtomicU64,
    blocked: AtomicU64,
    cached: AtomicU64,
    forwarded: AtomicU64,
    errors: AtomicU64,
}

impl Stats {
    /// Erzeugt die Statistik mit einem Query-Log der angegebenen Größe.
    pub fn new(log_capacity: usize) -> Self {
        Self {
            started: Instant::now(),
            log: QueryLog::new(log_capacity),
            queries: AtomicU64::new(0),
            blocked: AtomicU64::new(0),
            cached: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    /// Erfasst eine beantwortete Query.
    pub fn record(&self, name: &str, qtype: u16, verdict: Verdict) {
        self.queries.fetch_add(1, Ordering::Relaxed);
        let counter = match verdict {
            Verdict::Blocked => &self.blocked,
            Verdict::Cached => &self.cached,
            Verdict::Forwarded => &self.forwarded,
            Verdict::Error => &self.errors,
        };
        counter.fetch_add(1, Ordering::Relaxed);

        let timestamp = self.started.elapsed().as_secs().min(u32::MAX as u64) as u32;
        self.log.record(name, qtype, verdict, timestamp);
    }

    /// Erfasst ein Paket, das nicht beantwortet werden konnte (kein Name, kaputt).
    pub fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Baut eine Momentaufnahme für die API.
    pub fn snapshot(&self, recent: usize) -> Snapshot {
        let now = self.started.elapsed().as_secs().min(u32::MAX as u64) as u32;
        Snapshot {
            uptime: self.started.elapsed(),
            queries: self.queries.load(Ordering::Relaxed),
            blocked: self.blocked.load(Ordering::Relaxed),
            cached: self.cached.load(Ordering::Relaxed),
            forwarded: self.forwarded.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            recent: self.log.snapshot(recent, now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn snapshot_returns_newest_first() {
        let stats = Stats::new(4);
        stats.record("a.test", 1, Verdict::Forwarded);
        stats.record("b.test", 1, Verdict::Blocked);

        let snapshot = stats.snapshot(10);
        assert_eq!(2, snapshot.queries);
        assert_eq!(1, snapshot.blocked);
        assert_eq!(1, snapshot.forwarded);
        assert_eq!("b.test", snapshot.recent[0].name);
        assert_eq!(Verdict::Blocked, snapshot.recent[0].verdict);
        assert_eq!("a.test", snapshot.recent[1].name);
    }

    #[test]
    fn ring_wraps_and_keeps_only_the_newest_entries() {
        let stats = Stats::new(2);
        stats.record("one.test", 1, Verdict::Forwarded);
        stats.record("two.test", 1, Verdict::Forwarded);
        stats.record("three.test", 1, Verdict::Blocked);

        let snapshot = stats.snapshot(10);
        assert_eq!(3, snapshot.queries);
        assert_eq!(2, snapshot.recent.len());
        assert_eq!("three.test", snapshot.recent[0].name);
        assert_eq!("two.test", snapshot.recent[1].name);
    }

    #[test]
    fn long_names_are_truncated_and_qtype_is_preserved() {
        let stats = Stats::new(2);
        let long = "a".repeat(MAX_NAME_BYTES * 2);
        stats.record(&long, 28, Verdict::Cached);

        let snapshot = stats.snapshot(1);
        let entry = &snapshot.recent[0];
        assert_eq!(MAX_NAME_BYTES, entry.name.len());
        assert_eq!(28, entry.qtype);
        assert_eq!(Verdict::Cached, entry.verdict);
    }

    #[test]
    fn concurrent_writers_never_produce_torn_entries() {
        let stats = Arc::new(Stats::new(8));
        let mut handles = Vec::new();
        for writer in 0..4 {
            let stats = Arc::clone(&stats);
            handles.push(std::thread::spawn(move || {
                for index in 0..200 {
                    let name = format!("w{writer}-{index}.test");
                    let verdict = if index % 2 == 0 {
                        Verdict::Blocked
                    } else {
                        Verdict::Forwarded
                    };
                    stats.record(&name, 1, verdict);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let snapshot = stats.snapshot(16);
        assert_eq!(800, snapshot.queries);
        assert!(snapshot.recent.len() <= 8);
        for entry in &snapshot.recent {
            assert!(entry.name.ends_with(".test"));
            assert!(entry.name.starts_with('w'));
        }
    }
}
