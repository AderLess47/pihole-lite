//! Filter-Engine: ein Trait, zwei Backends.
//!
//! * Tier 1 (Nano) → [`NanoHashBackend`]: `Vec<u64>` aus xxHash64-Werten, sortiert,
//!   Binärsuche. Acht Byte pro Domain, kein Zeiger-Overhead.
//! * Tier 2/3 → [`RadixTrieBackend`]: Trie über die umgekehrten Labels, damit
//!   Subdomains ohne Zusatzaufwand matchen.
//!
//! Beide Backends interpretieren einen Treffer als "diese Domain **und alle
//! Subdomains** sind geblockt" – die Semantik ist damit tier-unabhängig.

use std::borrow::Cow;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::xxh3_64;

use crate::profiler::Tier;

/// Gemeinsames Interface aller Filter-Backends.
///
/// Ablauf: `add` (beliebig oft, aus dem Gravity-Stream) → `seal` → nur noch lesend.
pub trait FilterBackend: Send + Sync {
    /// Name des Backends für Logs und Dashboard.
    fn kind(&self) -> &'static str;
    /// Fügt eine Domain (mit allen Subdomains) hinzu.
    fn add(&mut self, domain: &str);
    /// Schließt die Aufbauphase ab (sortieren/deduplizieren). Idempotent.
    fn seal(&mut self);
    /// Prüft, ob die Domain oder einer ihrer Eltern-Domains geblockt ist.
    fn is_blocked(&self, domain: &str) -> bool;
    /// Anzahl eindeutiger Domains.
    fn len(&self) -> usize;
    /// Grobe Heap-Schätzung in Bytes.
    fn heap_bytes(&self) -> usize;
}

/// Normalisiert eine Domain: trimmt, entfernt führende/abschließende Punkte und
/// gibt sie in Kleinschreibung zurück. Borrowed, wenn nichts zu ändern war.
/// Aktuelle Unix-Zeit in Sekunden (0, falls die Uhr vor 1970 steht).
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

pub fn normalize(domain: &str) -> Cow<'_, str> {
    let trimmed = domain.trim();
    let trimmed = trimmed
        .strip_prefix('.')
        .unwrap_or(trimmed)
        .trim_end_matches('.');
    if trimmed.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(trimmed.to_ascii_lowercase())
    } else {
        Cow::Borrowed(trimmed)
    }
}

/// Liest eine Domain aus einer Blocklisten-Zeile (Hosts-Format oder eine Domain pro Zeile).
///
/// Gibt `None` für Kommentare, Leerzeilen und Hosts-Boilerplate (`localhost`, ...) zurück.
/// Der Rückgabewert borgt aus `line`, wenn keine Kleinschreibung nötig war, damit der
/// Streaming-Leser seinen Puffer wiederverwenden kann.
pub fn blocklist_domain_from_line(line: &str) -> Option<Cow<'_, str>> {
    const HOSTS_BOILERPLATE: [&str; 10] = [
        "localhost",
        "localhost.localdomain",
        "local",
        "broadcasthost",
        "ip6-localhost",
        "ip6-loopback",
        "ip6-allnodes",
        "ip6-allrouters",
        "ip6-localnet",
        "ip6-mcastprefix",
    ];

    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
        return None;
    }

    let mut fields = line.split_whitespace();
    let first = fields.next()?;
    let candidate = if first.parse::<IpAddr>().is_ok() {
        // Hosts-Format: "0.0.0.0 ads.example.com [alias ...]"
        fields.next()?
    } else {
        // Adblock-Syntax ("||domain^") gehört in den Gravity-Parser, nicht hierher.
        if first.starts_with("||") || first.contains('/') {
            return None;
        }
        first
    };

    let candidate = normalize(candidate);
    {
        let domain = candidate.as_ref();
        // Einzelne Labels würden eine ganze TLD blocken, und eine IP ist keine Domain –
        // beides ist immer ein Parserfehler.
        if domain.is_empty()
            || !domain.contains('.')
            || domain.len() > 253
            || domain.parse::<IpAddr>().is_ok()
        {
            return None;
        }
        if domain
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_'))
        {
            return None;
        }
        if HOSTS_BOILERPLATE
            .iter()
            .any(|entry| domain.eq_ignore_ascii_case(entry))
        {
            return None;
        }
    }
    Some(candidate)
}

/// Tier-1-Backend: sortierte xxHash64-Werte, Binärsuche, 8 Byte pro Domain.
#[derive(Debug, Default)]
pub struct NanoHashBackend {
    hashes: Vec<u64>,
    sorted: bool,
}

impl FilterBackend for NanoHashBackend {
    fn kind(&self) -> &'static str {
        "nanohash (sorted xxh3-64)"
    }

    fn add(&mut self, domain: &str) {
        let domain = normalize(domain);
        if domain.is_empty() {
            return;
        }
        self.hashes.push(xxh3_64(domain.as_bytes()));
        self.sorted = false;
    }

    fn seal(&mut self) {
        if self.sorted {
            return;
        }
        self.hashes.sort_unstable();
        self.hashes.dedup();
        // Nach dem Laden keine Reserve mehr halten: auf einem 256-MB-Pi zählt jedes KiB.
        self.hashes.shrink_to_fit();
        self.sorted = true;
    }

    fn is_blocked(&self, domain: &str) -> bool {
        if domain.is_empty() || self.hashes.is_empty() {
            return false;
        }
        debug_assert!(self.sorted, "NanoHashBackend vor seal() abgefragt");

        // Jede Suffix-Grenze hathen: "a.b.c" -> "a.b.c", "b.c", "c".
        let bytes = domain.as_bytes();
        let mut start = 0usize;
        loop {
            if self.hashes.binary_search(&xxh3_64(&bytes[start..])).is_ok() {
                return true;
            }
            match bytes[start..].iter().position(|&b| b == b'.') {
                Some(offset) => start += offset + 1,
                None => return false,
            }
        }
    }

    fn len(&self) -> usize {
        self.hashes.len()
    }

    fn heap_bytes(&self) -> usize {
        self.hashes.capacity() * std::mem::size_of::<u64>()
    }
}

/// Knoten des Label-Tries. Die Kante ist ein komplettes DNS-Label, deshalb ist die
/// Suche direkt an Label-Grenzen ausgerichtet (kein Präfix-Split nötig).
#[derive(Debug, Default)]
struct TrieNode {
    label: String,
    blocked: bool,
    children: Vec<TrieNode>,
}

/// Tier-2/3-Backend: Radix-Trie über umgekehrte Labels.
///
/// `ads.example.com` wird als `com → example → ads` eingefügt. Ein Treffer auf einem
/// inneren Knoten blockt damit automatisch alle Subdomains.
#[derive(Debug, Default)]
pub struct RadixTrieBackend {
    root: TrieNode,
    entries: usize,
}

impl FilterBackend for RadixTrieBackend {
    fn kind(&self) -> &'static str {
        "radix-trie (reversed labels)"
    }

    fn add(&mut self, domain: &str) {
        let domain = normalize(domain);
        if domain.is_empty() {
            return;
        }

        let mut node = &mut self.root;
        for label in domain.rsplit('.') {
            if label.is_empty() {
                continue;
            }
            let index = match node.children.iter().position(|child| child.label == label) {
                Some(index) => index,
                None => {
                    node.children.push(TrieNode {
                        label: label.to_owned(),
                        blocked: false,
                        children: Vec::new(),
                    });
                    node.children.len() - 1
                }
            };
            node = &mut node.children[index];
        }

        if !node.blocked {
            node.blocked = true;
            self.entries += 1;
        }
    }

    fn seal(&mut self) {
        self.root.children.shrink_to_fit();
    }

    fn is_blocked(&self, domain: &str) -> bool {
        let mut node = &self.root;
        for label in domain.rsplit('.') {
            if label.is_empty() {
                continue;
            }
            if node.blocked {
                return true;
            }
            match node.children.iter().find(|child| child.label == label) {
                Some(child) => node = child,
                None => return false,
            }
        }
        node.blocked
    }

    fn len(&self) -> usize {
        self.entries
    }

    fn heap_bytes(&self) -> usize {
        fn walk(node: &TrieNode) -> usize {
            let mut total = node.label.capacity();
            total += node.children.capacity() * std::mem::size_of::<TrieNode>();
            for child in &node.children {
                total += walk(child);
            }
            total
        }
        walk(&self.root)
    }
}

/// Erzeugt ein leeres Backend für das angegebene Tier.
///
/// Der Gravity-Updater baut damit einen neuen Bestand auf, ohne den laufenden anzufassen.
pub fn backend_for_tier(tier: Tier) -> Box<dyn FilterBackend> {
    match tier {
        Tier::Nano => Box::new(NanoHashBackend::default()),
        Tier::Balanced | Tier::Pro => Box::new(RadixTrieBackend::default()),
    }
}

/// Laufzeit-Wrapper um das gewählte Backend.
///
/// Der `RwLock` erlaubt dem Gravity-Updater später, das komplette Backend zu tauschen,
/// ohne den DNS-Pfad anzuhalten; im Normalbetrieb ist es ein unkontended Read-Lock.
pub struct FilterEngine {
    tier: Tier,
    backend: RwLock<Box<dyn FilterBackend>>,
    /// Unix-Zeitpunkt, bis zu dem der Filter aus ist (0 = aktiv).
    disabled_until: AtomicU64,
}

impl FilterEngine {
    /// Erzeugt die Engine mit dem Backend des angegebenen Tiers.
    pub fn new(tier: Tier) -> Self {
        Self {
            tier,
            backend: RwLock::new(backend_for_tier(tier)),
            disabled_until: AtomicU64::new(0),
        }
    }

    /// Tier, nach dem neue Backends gebaut werden.
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Tauscht den kompletten Bestand atomar aus.
    ///
    /// Der Write-Lock blockt Leser nur für die Dauer der Zuweisung; der alte Bestand wird
    /// danach freigegeben.
    pub fn replace_backend(&self, backend: Box<dyn FilterBackend>) {
        *self.write() = backend;
    }

    fn read(&self) -> RwLockReadGuard<'_, Box<dyn FilterBackend>> {
        // Vergiftete Locks dürfen den DNS-Dienst nicht mitreißen.
        self.backend.read().unwrap_or_else(|err| err.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Box<dyn FilterBackend>> {
        self.backend.write().unwrap_or_else(|err| err.into_inner())
    }

    /// Fügt eine Domain hinzu.
    pub fn add(&self, domain: &str) {
        self.write().add(domain);
    }

    /// Schließt die Aufbauphase ab.
    pub fn seal(&self) {
        self.write().seal();
    }

    /// Schaltet den Filter für `seconds` Sekunden ab (0 = sofort wieder an).
    pub fn disable_for(&self, seconds: u64) {
        let until = unix_seconds().saturating_add(seconds);
        self.disabled_until.store(until, Ordering::Release);
    }

    /// Schaltet den Filter wieder ein.
    pub fn enable(&self) {
        self.disabled_until.store(0, Ordering::Release);
    }

    /// Ist der Filter gerade abgeschaltet?
    pub fn is_disabled(&self) -> bool {
        self.disabled_until.load(Ordering::Acquire) > unix_seconds()
    }

    /// Restlaufzeit der Abschaltung, falls aktiv.
    pub fn disabled_remaining(&self) -> Option<Duration> {
        let until = self.disabled_until.load(Ordering::Acquire);
        let now = unix_seconds();
        (until > now).then(|| Duration::from_secs(until - now))
    }

    /// Prüft eine (bereits normalisierte, kleingeschriebene) Query-Domain.
    ///
    /// Solange der Filter abgeschaltet ist, gilt alles als erlaubt.
    pub fn is_blocked(&self, domain: &str) -> bool {
        if self.is_disabled() {
            return false;
        }
        self.read().is_blocked(domain)
    }

    /// Anzahl eindeutiger geblockter Domains.
    pub fn domain_count(&self) -> usize {
        self.read().len()
    }

    /// Backend-Name für Logs und Dashboard.
    pub fn backend_name(&self) -> &'static str {
        self.read().kind()
    }

    /// Grobe Heap-Schätzung des Backends in Bytes.
    pub fn heap_bytes(&self) -> usize {
        self.read().heap_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_matching(backend: &mut dyn FilterBackend) {
        backend.add("Ads.Example.COM.");
        backend.add("tracker.test");
        backend.add("tracker.test");
        backend.seal();

        assert!(backend.is_blocked("ads.example.com"));
        assert!(backend.is_blocked("a.b.ads.example.com"));
        assert!(backend.is_blocked("tracker.test"));
        assert!(!backend.is_blocked("example.com"));
        assert!(!backend.is_blocked("notads.example.com"));
        assert!(!backend.is_blocked("nottracker.test"));
        assert!(!backend.is_blocked(""));
        assert_eq!(2, backend.len());
    }

    #[test]
    fn nanohash_matches_domains_and_subdomains() {
        assert_matching(&mut NanoHashBackend::default());
    }

    #[test]
    fn radix_trie_matches_domains_and_subdomains() {
        assert_matching(&mut RadixTrieBackend::default());
    }

    #[test]
    fn blocklist_lines_are_parsed_or_rejected() {
        assert_eq!(
            Some("ads.example.com"),
            blocklist_domain_from_line("0.0.0.0 ads.example.com").as_deref()
        );
        assert_eq!(
            Some("ads.example.com"),
            blocklist_domain_from_line("  ads.example.com  ").as_deref()
        );
        assert_eq!(
            Some("ads.example.com"),
            blocklist_domain_from_line("ADS.EXAMPLE.COM.").as_deref()
        );
        assert_eq!(None, blocklist_domain_from_line("# comment"));
        assert_eq!(None, blocklist_domain_from_line(""));
        assert_eq!(None, blocklist_domain_from_line("127.0.0.1 localhost"));
        assert_eq!(None, blocklist_domain_from_line("0.0.0.0 0.0.0.0"));
        assert_eq!(None, blocklist_domain_from_line("||ads.example.com^"));
        assert_eq!(None, blocklist_domain_from_line("com"));
    }
}
